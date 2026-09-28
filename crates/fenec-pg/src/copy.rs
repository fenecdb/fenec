//! `COPY <collection> [(columns)] FROM STDIN`: the statement, the rows the
//! client streams after it, and each cell read as its field's type.
//!
//! PostgreSQL's clients load a table this way -- psql's `\copy`, psycopg's
//! `copy`, JDBC's `CopyManager` -- and without it a pg client could only
//! send a statement a row. Text and CSV, with their options; `binary`,
//! `COPY ... TO`, and a file or a program on the server's side are refused
//! with what to do instead. The server's side of the exchange is
//! `server::copy_in`.

use fenec_core::json;
use fenec_core::prelude::{Database, Expr, Value};
use fenec_core::value::DataType;

/// A PostgreSQL error: its SQLSTATE and message.
pub type Refusal = (&'static str, String);

#[derive(Debug, Clone, PartialEq)]
pub enum Format {
    /// Tab-delimited by default, `\N` for NULL, backslash escapes.
    Text { delimiter: u8, null: String },
    /// PostgreSQL's own: a `PGCOPY` header, then each row as its cells'
    /// lengths and bytes in their types' binary formats -- what asyncpg's
    /// `copy_records_to_table` and pgx's `CopyFrom` send.
    Binary,
    /// `,` by default, fields quoted with `quote`, which `escape` escapes
    /// inside one; NULL is an unquoted `null`, empty by default.
    Csv {
        delimiter: u8,
        null: String,
        quote: u8,
        escape: u8,
        header: bool,
    },
}

/// What a `COPY ... FROM STDIN` names.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub table: String,
    /// Empty: every column, `id` first, as the catalog lists them.
    pub columns: Vec<String>,
    pub format: Format,
}

/// Why a COPY among other statements is refused: its rows follow the
/// query that asks for them.
pub const ALONE: &str =
    "COPY FROM STDIN is taken as a query of its own: send it alone, as psql's \\copy does";

/// `Some` for a text that is a `COPY`: what it names, or why it is refused.
pub fn parse(sql: &str) -> Option<Result<Spec, Refusal>> {
    // Every simple query is asked, so the rest is tokenized only past a
    // first word that is `copy`.
    let text = sql.trim_start().as_bytes();
    if !text
        .get(..4)
        .is_some_and(|w| w.eq_ignore_ascii_case(b"copy"))
        || text
            .get(4)
            .is_some_and(|&b| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80)
    {
        return None;
    }
    let mut toks = tokens(sql);
    match toks.first() {
        Some(Tok::Word(w)) if w == "copy" => {}
        _ => return None,
    }
    while toks.last() == Some(&Tok::Punct(';')) {
        toks.pop();
    }
    if toks.contains(&Tok::Punct(';')) {
        return Some(Err(unsupported(ALONE)));
    }
    Some(statement(&toks[1..]))
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    /// An unquoted word, lowered as PostgreSQL folds it.
    Word(String),
    /// A `"quoted"` name, as written.
    Name(String),
    /// A `'string'`.
    Str(String),
    Punct(char),
}

pub(crate) fn tokens(sql: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut it = sql.char_indices().peekable();
    while let Some((_, c)) = it.next() {
        match c {
            c if c.is_whitespace() => {}
            '"' | '\'' => {
                let mut s = String::new();
                while let Some((_, d)) = it.next() {
                    if d == c {
                        // Doubled, it is itself.
                        if it.peek().map(|&(_, e)| e) == Some(c) {
                            it.next();
                            s.push(c);
                            continue;
                        }
                        break;
                    }
                    s.push(d);
                }
                out.push(match c {
                    '"' => Tok::Name(s),
                    _ => Tok::Str(s),
                });
            }
            c if c.is_alphanumeric() || c == '_' => {
                let mut s = c.to_lowercase().collect::<String>();
                while let Some(&(_, d)) = it.peek() {
                    if !(d.is_alphanumeric() || d == '_' || d == '$') {
                        break;
                    }
                    s.extend(d.to_lowercase());
                    it.next();
                }
                out.push(Tok::Word(s));
            }
            c => out.push(Tok::Punct(c)),
        }
    }
    out
}

fn name(t: Option<&Tok>) -> Option<String> {
    match t {
        Some(Tok::Word(w)) | Some(Tok::Name(w)) => Some(w.clone()),
        _ => None,
    }
}

fn unsupported(what: &str) -> Refusal {
    ("0A000", what.to_string())
}

fn syntax(what: &str) -> Refusal {
    ("42601", format!("COPY: {what}"))
}

fn statement(t: &[Tok]) -> Result<Spec, Refusal> {
    let mut i = 0;
    if t.first() == Some(&Tok::Punct('(')) {
        return Err(unsupported(
            "COPY of a query is not supported: COPY a collection FROM STDIN",
        ));
    }
    // The table, schema-qualified or not: `public` is the only schema.
    let mut table = name(t.get(i)).ok_or_else(|| syntax("a table name is expected"))?;
    i += 1;
    if t.get(i) == Some(&Tok::Punct('.')) {
        if table != "public" {
            return Err(("3F000", format!("schema \"{table}\" does not exist")));
        }
        table = name(t.get(i + 1)).ok_or_else(|| syntax("a table name is expected"))?;
        i += 2;
    }
    let mut columns = Vec::new();
    if t.get(i) == Some(&Tok::Punct('(')) {
        i += 1;
        loop {
            columns.push(name(t.get(i)).ok_or_else(|| syntax("a column name is expected"))?);
            i += 1;
            match t.get(i) {
                Some(Tok::Punct(',')) => i += 1,
                Some(Tok::Punct(')')) => {
                    i += 1;
                    break;
                }
                _ => return Err(syntax("`,` or `)` is expected in the column list")),
            }
        }
    }
    match t.get(i) {
        Some(Tok::Word(w)) if w == "from" => {}
        Some(Tok::Word(w)) if w == "to" => {
            return Err(unsupported(
                "COPY TO is not supported: read the rows with a query",
            ))
        }
        _ => return Err(syntax("FROM STDIN is expected")),
    }
    i += 1;
    match t.get(i) {
        Some(Tok::Word(w)) if w == "stdin" => {}
        Some(Tok::Str(_)) | Some(Tok::Word(_)) => {
            return Err(unsupported(
                "COPY FROM a file or a program is not supported: send the rows FROM STDIN, \
                 as psql's \\copy does",
            ))
        }
        _ => return Err(syntax("FROM STDIN is expected")),
    }
    i += 1;
    let format = options(&t[i..])?;
    Ok(Spec {
        table,
        columns,
        format,
    })
}

/// A single character, as `DELIMITER`, `QUOTE` and `ESCAPE` take.
fn one_byte(t: Option<&Tok>, what: &str) -> Result<u8, Refusal> {
    match t {
        Some(Tok::Str(s)) if s.len() == 1 && s.is_ascii() => Ok(s.as_bytes()[0]),
        Some(Tok::Str(_)) => Err(unsupported(&format!(
            "COPY {what} must be a single one-byte character"
        ))),
        _ => Err(syntax(&format!("{what} takes a quoted character"))),
    }
}

fn boolean(t: Option<&Tok>) -> Option<bool> {
    match t {
        Some(Tok::Word(w)) | Some(Tok::Str(w)) => match w.as_str() {
            "true" | "on" | "1" | "match" => Some(true),
            "false" | "off" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// The options after `FROM STDIN`: `WITH (FORMAT csv, HEADER, ...)` as
/// PostgreSQL writes them since 9.0, or the older `WITH CSV HEADER
/// DELIMITER AS ','` that psql still passes on.
fn options(t: &[Tok]) -> Result<Format, Refusal> {
    let mut t = t;
    if let Some(Tok::Word(w)) = t.first() {
        if w == "with" {
            t = &t[1..];
        }
    }
    let (mut csv, mut delimiter, mut null, mut quote, mut escape, mut header) =
        (false, None, None, None, None, false);
    let mut binary = false;
    if t.first() == Some(&Tok::Punct('(')) {
        if t.last() != Some(&Tok::Punct(')')) {
            return Err(syntax("the option list is not closed"));
        }
        for opt in t[1..t.len() - 1].split(|x| *x == Tok::Punct(',')) {
            let Some(Tok::Word(key)) = opt.first() else {
                return Err(syntax("an option name is expected"));
            };
            let value = opt.get(1);
            match key.as_str() {
                "format" => match value {
                    Some(Tok::Word(f)) | Some(Tok::Str(f)) if f == "csv" => csv = true,
                    Some(Tok::Word(f)) | Some(Tok::Str(f)) if f == "text" => csv = false,
                    Some(Tok::Word(f)) | Some(Tok::Str(f)) if f == "binary" => binary = true,
                    _ => return Err(syntax("FORMAT is text or csv")),
                },
                "delimiter" => delimiter = Some(one_byte(value, "delimiter")?),
                "null" => match value {
                    Some(Tok::Str(s)) => null = Some(s.clone()),
                    _ => return Err(syntax("NULL takes a quoted string")),
                },
                "header" => {
                    header = value
                        .map_or(Some(true), |v| boolean(Some(v)))
                        .unwrap_or(true)
                }
                "quote" => quote = Some(one_byte(value, "quote")?),
                "escape" => escape = Some(one_byte(value, "escape")?),
                // Asks for what a load here does anyway, or names the
                // encoding a UTF-8 text is in.
                "freeze" => {}
                "encoding" => match value {
                    Some(Tok::Str(e))
                        if e.eq_ignore_ascii_case("utf8") || e.eq_ignore_ascii_case("utf-8") => {}
                    _ => return Err(unsupported("COPY reads UTF8 alone")),
                },
                other => {
                    return Err(unsupported(&format!(
                        "COPY option \"{other}\" is not supported"
                    )))
                }
            }
        }
    } else {
        let mut i = 0;
        while i < t.len() {
            let Tok::Word(w) = &t[i] else {
                return Err(syntax("an option is expected"));
            };
            let as_ = |i: usize| match t.get(i) {
                Some(Tok::Word(a)) if a == "as" => i + 1,
                _ => i,
            };
            match w.as_str() {
                "csv" => csv = true,
                "header" => header = true,
                "binary" => binary = true,
                "delimiter" => {
                    i = as_(i + 1);
                    delimiter = Some(one_byte(t.get(i), "delimiter")?);
                }
                "null" => {
                    i = as_(i + 1);
                    match t.get(i) {
                        Some(Tok::Str(s)) => null = Some(s.clone()),
                        _ => return Err(syntax("NULL takes a quoted string")),
                    }
                }
                "quote" => {
                    i = as_(i + 1);
                    quote = Some(one_byte(t.get(i), "quote")?);
                }
                "escape" => {
                    i = as_(i + 1);
                    escape = Some(one_byte(t.get(i), "escape")?);
                }
                other => {
                    return Err(unsupported(&format!(
                        "COPY option \"{other}\" is not supported"
                    )))
                }
            }
            i += 1;
        }
    }
    if binary {
        // What shapes the text of a row has none in binary, as PostgreSQL
        // refuses it.
        let given = [
            ("DELIMITER", delimiter.is_some()),
            ("NULL", null.is_some()),
            ("QUOTE", quote.is_some()),
            ("ESCAPE", escape.is_some()),
            ("HEADER", header),
            ("CSV", csv),
        ];
        return match given.iter().find(|(_, set)| *set) {
            Some((what, _)) => Err(syntax(&format!("cannot specify {what} in BINARY mode"))),
            None => Ok(Format::Binary),
        };
    }
    let quote = quote.unwrap_or(b'"');
    Ok(if csv {
        Format::Csv {
            delimiter: delimiter.unwrap_or(b','),
            null: null.unwrap_or_default(),
            quote,
            escape: escape.unwrap_or(quote),
            header,
        }
    } else {
        if header {
            return Err(unsupported("COPY HEADER is available only in CSV mode"));
        }
        Format::Text {
            delimiter: delimiter.unwrap_or(b'\t'),
            null: null.unwrap_or_else(|| "\\N".into()),
        }
    })
}

// ------------------------------------------------------------------ rows

/// The rows of the stream, a line at a time as the CopyData messages
/// bring them -- a row may be cut across two -- each a cell a column,
/// `None` for NULL.
pub struct Reader {
    format: Format,
    buf: Vec<u8>,
    /// The header to pass over: CSV's line, binary's signature.
    header: bool,
    /// Past `\.`: whatever follows is not data.
    ended: bool,
    /// Lines read, for the errors to say where.
    pub line: u64,
}

/// A cell: text, as the text format and CSV hold one, or the bytes binary
/// holds, in its column type's binary format.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Text(String),
    Binary(Vec<u8>),
}

pub type Row = Vec<Option<Cell>>;

/// A row and the line it ended on, for an error to say where.
pub type Numbered = (u64, Row);

impl Reader {
    pub fn new(format: Format) -> Reader {
        let header = matches!(format, Format::Csv { header: true, .. } | Format::Binary);
        Reader {
            format,
            buf: Vec::new(),
            header,
            ended: false,
            line: 0,
        }
    }

    /// The rows `data` completes, onto `rows`.
    pub fn feed(&mut self, data: &[u8], rows: &mut Vec<Numbered>) -> Result<(), Refusal> {
        if self.ended {
            return Ok(());
        }
        self.buf.extend_from_slice(data);
        if self.format == Format::Binary {
            return self.binary(rows);
        }
        // Taken out while its records are read, and what is left of it --
        // a row the next message ends -- put back.
        let buf = std::mem::take(&mut self.buf);
        let mut start = 0;
        while let Some(len) = self.record_len(&buf[start..]) {
            let end = start + len;
            let mut record = &buf[start..end];
            start = end;
            record = record.strip_suffix(b"\n").unwrap_or(record);
            record = record.strip_suffix(b"\r").unwrap_or(record);
            if self.take(record, rows)? {
                self.ended = true;
                return Ok(());
            }
        }
        self.buf = buf;
        self.buf.drain(..start);
        Ok(())
    }

    /// The end of the stream: a last row with no newline after it.
    pub fn finish(&mut self, rows: &mut Vec<Numbered>) -> Result<(), Refusal> {
        if self.ended || self.buf.is_empty() {
            return Ok(());
        }
        // A binary stream may end without its trailer, between two rows as
        // PostgreSQL takes it; inside one, or its header, it is cut short.
        if self.format == Format::Binary {
            return Err(self.bad("unexpected EOF in COPY data"));
        }
        if let Format::Csv { quote, escape, .. } = &self.format {
            if open_quote(&self.buf, *quote, *escape) {
                return Err(self.bad("unterminated CSV quoted field"));
            }
        }
        let record = std::mem::take(&mut self.buf);
        let record = record.strip_suffix(b"\r").unwrap_or(&record);
        self.take(record, rows)?;
        Ok(())
    }

    /// The length of the record at `start`, its newline included, once the
    /// buffer holds all of it. A CSV newline inside quotes is data.
    fn record_len(&self, rest: &[u8]) -> Option<usize> {
        match &self.format {
            Format::Text { .. } | Format::Binary => {
                rest.iter().position(|&b| b == b'\n').map(|p| p + 1)
            }
            Format::Csv { quote, escape, .. } => {
                let mut inside = false;
                let mut i = 0;
                while i < rest.len() {
                    let b = rest[i];
                    if inside {
                        if b == *escape && escape != quote && rest.get(i + 1) == Some(quote) {
                            i += 2;
                            continue;
                        }
                        if b == *quote {
                            if escape == quote && rest.get(i + 1) == Some(quote) {
                                i += 2;
                                continue;
                            }
                            // A quote that may be doubled by the next byte,
                            // not here yet: wait for it.
                            if escape == quote && i + 1 == rest.len() {
                                return None;
                            }
                            inside = false;
                        }
                    } else if b == *quote {
                        inside = true;
                    } else if b == b'\n' {
                        return Some(i + 1);
                    }
                    i += 1;
                }
                None
            }
        }
    }

    /// The rows the buffer holds whole in the binary format, onto `rows`:
    /// past the header, each row its cells' count, then each cell's length
    /// -- -1 for NULL -- and bytes; a count of -1 is the trailer.
    fn binary(&mut self, rows: &mut Vec<Numbered>) -> Result<(), Refusal> {
        const SIGNATURE: &[u8] = b"PGCOPY\n\xff\r\n\0";
        let buf = std::mem::take(&mut self.buf);
        let i16_at = |at: usize| {
            buf.get(at..at + 2)
                .map(|b| i16::from_be_bytes([b[0], b[1]]))
        };
        let i32_at = |at: usize| {
            buf.get(at..at + 4)
                .map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        };
        let mut at = 0;
        if self.header {
            let (Some(flags), Some(ext)) = (i32_at(11), i32_at(15)) else {
                self.buf = buf;
                return Ok(());
            };
            if &buf[..11] != SIGNATURE {
                return Err(self.bad("COPY file signature not recognized"));
            }
            // Bit 16 says every row carries an OID first, which no
            // collection has.
            if flags & (1 << 16) != 0 {
                return Err(self.bad("COPY rows with OIDs are not supported"));
            }
            let end = 19 + ext.max(0) as usize;
            if buf.len() < end {
                self.buf = buf;
                return Ok(());
            }
            self.header = false;
            at = end;
        }
        while let Some(count) = i16_at(at) {
            if count == -1 {
                self.ended = true;
                return Ok(());
            }
            if count < 0 {
                return Err(self.bad("invalid COPY row field count"));
            }
            let mut p = at + 2;
            let mut row = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let Some(len) = i32_at(p) else { break };
                p += 4;
                if len == -1 {
                    row.push(None);
                    continue;
                }
                let Some(cell) = buf.get(p..p + len.max(0) as usize).filter(|_| len >= 0) else {
                    if len < -1 {
                        return Err(self.bad("invalid COPY field length"));
                    }
                    break;
                };
                row.push(Some(Cell::Binary(cell.to_vec())));
                p += len as usize;
            }
            // Cut short: the rest comes in the next message.
            if row.len() < count as usize {
                break;
            }
            self.line += 1;
            rows.push((self.line, row));
            at = p;
        }
        self.buf = buf;
        self.buf.drain(..at);
        Ok(())
    }

    /// One record: its row onto `rows`; `true` at the end marker.
    fn take(&mut self, record: &[u8], rows: &mut Vec<Numbered>) -> Result<bool, Refusal> {
        self.line += 1;
        if record == b"\\." {
            return Ok(true);
        }
        if self.header {
            self.header = false;
            return Ok(false);
        }
        let row = match &self.format {
            Format::Text { delimiter, null } => record
                .split(|b| b == delimiter)
                .map(|cell| {
                    if cell == null.as_bytes() {
                        return Ok(None);
                    }
                    utf8(unescape(cell)).map(|s| Some(Cell::Text(s)))
                })
                .collect::<Result<Row, _>>(),
            Format::Csv {
                delimiter,
                null,
                quote,
                escape,
                ..
            } => csv_cells(record, *delimiter, null, *quote, *escape),
            Format::Binary => unreachable!("binary rows are read by `binary`"),
        };
        rows.push((self.line, row.map_err(|e| self.bad(&e))?));
        Ok(false)
    }

    fn bad(&self, what: &str) -> Refusal {
        ("22P04", format!("{what}, line {}", self.line))
    }
}

fn utf8(b: Vec<u8>) -> Result<String, String> {
    String::from_utf8(b).map_err(|_| "invalid byte sequence for encoding \"UTF8\"".into())
}

/// Whether a CSV record ends inside a quoted field.
fn open_quote(b: &[u8], quote: u8, escape: u8) -> bool {
    let mut inside = false;
    let mut i = 0;
    while i < b.len() {
        if inside && b[i] == escape && b.get(i + 1) == Some(&quote) {
            i += 2;
            continue;
        }
        if b[i] == quote {
            inside = !inside;
        }
        i += 1;
    }
    inside
}

/// COPY's text escapes decoded: `\b \f \n \r \t \v`, `\\`, octal `\NNN`,
/// hex `\xHH`, and any other character after a backslash as itself.
fn unescape(f: &[u8]) -> Vec<u8> {
    if !f.contains(&b'\\') {
        return f.to_vec();
    }
    let mut out = Vec::with_capacity(f.len());
    let mut i = 0;
    while i < f.len() {
        if f[i] != b'\\' {
            out.push(f[i]);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&c) = f.get(i) else {
            out.push(b'\\');
            break;
        };
        i += 1;
        match c {
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            b'0'..=b'7' => {
                let mut v = (c - b'0') as u32;
                for _ in 0..2 {
                    match f.get(i) {
                        Some(&d @ b'0'..=b'7') => {
                            v = v * 8 + (d - b'0') as u32;
                            i += 1;
                        }
                        _ => break,
                    }
                }
                out.push(v as u8);
            }
            b'x' if f.get(i).is_some_and(u8::is_ascii_hexdigit) => {
                let mut v = 0u8;
                for _ in 0..2 {
                    match f.get(i).and_then(|d| (*d as char).to_digit(16)) {
                        Some(d) => {
                            v = v * 16 + d as u8;
                            i += 1;
                        }
                        None => break,
                    }
                }
                out.push(v);
            }
            other => out.push(other),
        }
    }
    out
}

fn csv_cells(
    record: &[u8],
    delimiter: u8,
    null: &str,
    quote: u8,
    escape: u8,
) -> Result<Row, String> {
    let mut row = Vec::new();
    let mut cell = Vec::new();
    let (mut inside, mut quoted) = (false, false);
    let mut i = 0;
    loop {
        if i == record.len() {
            if inside {
                return Err("unterminated CSV quoted field".into());
            }
            row.push(csv_cell(cell, quoted, null)?);
            return Ok(row);
        }
        let b = record[i];
        if inside {
            if b == escape && record.get(i + 1) == Some(&quote) {
                cell.push(quote);
                i += 2;
                continue;
            }
            if b == quote {
                inside = false;
            } else {
                cell.push(b);
            }
        } else if b == quote {
            inside = true;
            quoted = true;
        } else if b == delimiter {
            row.push(csv_cell(std::mem::take(&mut cell), quoted, null)?);
            quoted = false;
        } else {
            cell.push(b);
        }
        i += 1;
    }
}

/// An unquoted cell that is the NULL string is NULL; a quoted one never.
fn csv_cell(cell: Vec<u8>, quoted: bool, null: &str) -> Result<Option<Cell>, String> {
    if !quoted && cell == null.as_bytes() {
        return Ok(None);
    }
    utf8(cell).map(|s| Some(Cell::Text(s)))
}

// ----------------------------------------------------------------- cells

/// Where the rows go: the collection and each column's field and type.
pub struct Target {
    pub collection: String,
    pub columns: Vec<(String, DataType)>,
}

/// The collection `spec` names, and its columns: those listed, or `id`
/// and every field in the order the schema declares them.
pub fn target(db: &Database, spec: &Spec) -> Result<Target, Refusal> {
    let c = db.collection(&spec.table).map_err(|_| {
        (
            "42P01",
            format!("relation \"{}\" does not exist", spec.table),
        )
    })?;
    let fields = &c.schema.fields;
    let find = |name: &str| -> Result<(String, DataType), Refusal> {
        if name == "id" {
            return Ok(("id".into(), DataType::Int));
        }
        fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| (f.name.clone(), f.ty.clone()))
            .ok_or_else(|| {
                (
                    "42703",
                    format!(
                        "column \"{name}\" of relation \"{}\" does not exist",
                        spec.table
                    ),
                )
            })
    };
    let columns = match spec.columns.is_empty() {
        true => std::iter::once(Ok(("id".into(), DataType::Int)))
            .chain(fields.iter().map(|f| Ok((f.name.clone(), f.ty.clone()))))
            .collect::<Result<Vec<_>, Refusal>>()?,
        false => spec
            .columns
            .iter()
            .map(|n| find(n))
            .collect::<Result<Vec<_>, Refusal>>()?,
    };
    Ok(Target {
        collection: spec.table.clone(),
        columns,
    })
}

/// A document to put: its fields and their values.
pub type Doc = Vec<(String, Expr)>;

/// The rows read so far as documents, onto `docs`.
pub fn documents(t: &Target, rows: &mut Vec<Numbered>, docs: &mut Vec<Doc>) -> Result<(), Refusal> {
    for (line, row) in rows.drain(..) {
        docs.push(document(t, row, line)?);
    }
    Ok(())
}

/// A row as the fields of a document to put, each cell read as its field's
/// type. A NULL `id` is none, the id handed out as a put's would be.
pub fn document(t: &Target, row: Row, line: u64) -> Result<Doc, Refusal> {
    if row.len() != t.columns.len() {
        let why = match row.len() < t.columns.len() {
            true => format!("missing data for column \"{}\"", t.columns[row.len()].0),
            false => "extra data after last expected column".to_string(),
        };
        return Err((
            "22P04",
            format!("COPY {}, line {line}: {why}", t.collection),
        ));
    }
    let mut doc = Vec::with_capacity(row.len());
    for ((name, ty), cell) in t.columns.iter().zip(row) {
        let bad = |why: String, s: &str| {
            (
                "22P02",
                format!(
                    "COPY {}, line {line}, column {name}: {why}: \"{s}\"",
                    t.collection
                ),
            )
        };
        let value = match cell {
            None if name == "id" => continue,
            None => Value::Null,
            Some(Cell::Text(s)) => value(&s, ty).map_err(|why| bad(why, &s))?,
            Some(Cell::Binary(b)) => binary(&b, ty).map_err(|why| bad(why, &hex(&b)))?,
        };
        doc.push((name.clone(), Expr::Lit(value)));
    }
    Ok(doc)
}

/// A binary cell as `ty` holds it: sent as the type the field's column is
/// described as ([`crate::server::pg_oid`]), a text type as its text. A
/// vector not in pgvector's binary format is read as the text it was sent
/// as before it had one, which holds no zero byte.
fn binary(b: &[u8], ty: &DataType) -> Result<Value, String> {
    let oid = crate::server::pg_oid(ty);
    let text = |b: &[u8]| {
        let s =
            std::str::from_utf8(b).map_err(|_| "invalid byte sequence for encoding \"UTF8\"")?;
        value(s, ty)
    };
    if oid == crate::proto::OID_TEXT {
        return text(b);
    }
    if let Some(v) = crate::binary::vector(b, oid) {
        return v.map_err(|(_, why)| why);
    }
    if matches!(ty, DataType::Vector(..) | DataType::Sparse(_)) {
        return match b.contains(&0) {
            true => Err("not a vector in its binary format".into()),
            false => text(b),
        };
    }
    let want = match oid {
        crate::proto::OID_BOOL => 1,
        crate::proto::OID_BYTEA => b.len(),
        _ => 8,
    };
    if b.len() != want {
        return Err(format!("{} bytes where its type sends {want}", b.len()));
    }
    crate::params::decode(b, true, oid, None).map_err(|(_, why)| why)
}

/// Bytes as `\x` and hexadecimal digits, for an error to show them.
fn hex(b: &[u8]) -> String {
    let mut s = String::from("\\x");
    for x in b.iter().take(32) {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// A cell's text as `ty` holds it. A timestamp and a sparse vector go on as
/// text, which the put's own coercion reads; a vector is pgvector's
/// `[1,2,3]`, read as a pg parameter is, or the `{1,2,3}` psycopg writes a
/// Python list as.
pub fn value(s: &str, ty: &DataType) -> Result<Value, String> {
    let t = s.trim();
    Ok(match ty {
        DataType::Bool => match t.to_ascii_lowercase().as_str() {
            "t" | "true" | "y" | "yes" | "on" | "1" => Value::Bool(true),
            "f" | "false" | "n" | "no" | "off" | "0" => Value::Bool(false),
            _ => return Err("invalid input syntax for type boolean".into()),
        },
        DataType::Int => Value::Int(
            t.parse()
                .map_err(|_| "invalid input syntax for type bigint".to_string())?,
        ),
        DataType::Float => Value::Float(match t.to_ascii_lowercase().as_str() {
            "nan" => f64::NAN,
            "infinity" | "inf" | "+infinity" => f64::INFINITY,
            "-infinity" | "-inf" => f64::NEG_INFINITY,
            _ => fenec_core::num::parse_f64(t)
                .ok_or_else(|| "invalid input syntax for type double precision".to_string())?,
        }),
        DataType::Text => Value::Text(s.to_string()),
        // `\x` and hexadecimal digits, as PostgreSQL writes a bytea.
        DataType::Bytes => match t.strip_prefix("\\x") {
            Some(hex) => {
                Value::Bytes(unhex(hex).ok_or_else(|| "invalid hexadecimal data".to_string())?)
            }
            None => Value::Bytes(s.as_bytes().to_vec()),
        },
        DataType::Vector(..) if t.starts_with('{') => Value::List(
            array(t)
                .ok_or_else(|| "invalid input syntax for type vector".to_string())?
                .into_iter()
                .map(|item| item.and_then(|x| fenec_core::num::parse_f64(x.trim())))
                .map(|x| x.map(Value::Float))
                .collect::<Option<_>>()
                .ok_or_else(|| "invalid input syntax for type vector".to_string())?,
        ),
        DataType::Vector(..) => match json::parse(t) {
            Ok(v @ (Value::Vector(_) | Value::List(_))) => v,
            _ => return Err("invalid input syntax for type vector".into()),
        },
        DataType::List(inner) => Value::List(
            array(t)
                .ok_or_else(|| "malformed array literal".to_string())?
                .into_iter()
                .map(|item| match item {
                    None => Ok(Value::Null),
                    Some(item) => value(&item, inner),
                })
                .collect::<Result<_, _>>()?,
        ),
        DataType::Timestamp | DataType::Sparse(_) => Value::Text(t.to_string()),
    })
}

fn unhex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

/// PostgreSQL's `{a,b,"c d"}`, or JSON's `["a","b"]`: the items, `None`
/// for an unquoted NULL.
fn array(t: &str) -> Option<Vec<Option<String>>> {
    if t.starts_with('[') {
        return match json::parse(t).ok()? {
            Value::List(items) => Some(
                items
                    .into_iter()
                    .map(|v| match v {
                        Value::Null => Some(None),
                        Value::Text(s) => Some(Some(s)),
                        Value::Int(i) => Some(Some(i.to_string())),
                        Value::Float(f) => Some(Some(f.to_string())),
                        Value::Bool(b) => Some(Some(b.to_string())),
                        _ => None,
                    })
                    .collect::<Option<_>>()?,
            ),
            Value::Vector(v) => Some(v.iter().map(|f| Some(f.to_string())).collect()),
            _ => None,
        };
    }
    let inner = t.strip_prefix('{')?.strip_suffix('}')?;
    let mut out = Vec::new();
    if inner.trim().is_empty() {
        return Some(out);
    }
    let mut item = String::new();
    let (mut quoted, mut inside) = (false, false);
    let mut chars = inner.chars();
    let push = |item: &mut String, quoted: bool, out: &mut Vec<Option<String>>| {
        let s = std::mem::take(item);
        out.push(match (quoted, s.trim()) {
            (false, n) if n.eq_ignore_ascii_case("null") => None,
            (false, n) => Some(n.to_string()),
            (true, _) => Some(s),
        });
    };
    while let Some(c) = chars.next() {
        match c {
            '\\' if inside => item.push(chars.next()?),
            '"' => {
                inside = !inside;
                quoted = true;
            }
            ',' if !inside => {
                push(&mut item, quoted, &mut out);
                quoted = false;
            }
            c => item.push(c),
        }
    }
    if inside {
        return None;
    }
    push(&mut item, quoted, &mut out);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(sql: &str) -> Spec {
        parse(sql).expect("a COPY").expect("taken")
    }

    #[test]
    fn statements_and_their_options() {
        let s = spec("COPY docs (category, score, embed) FROM STDIN");
        assert_eq!(s.table, "docs");
        assert_eq!(s.columns, ["category", "score", "embed"]);
        assert_eq!(
            s.format,
            Format::Text {
                delimiter: b'\t',
                null: "\\N".into()
            }
        );
        let s = spec("copy public.\"Docs\" from stdin with (format csv, header true, delimiter ';', null 'NULL');");
        assert_eq!(s.table, "Docs");
        assert!(s.columns.is_empty());
        assert_eq!(
            s.format,
            Format::Csv {
                delimiter: b';',
                null: "NULL".into(),
                quote: b'"',
                escape: b'"',
                header: true
            }
        );
        // What psql still passes on from its own options.
        let s =
            spec("COPY t FROM STDIN WITH CSV HEADER DELIMITER AS '|' QUOTE AS '''' ESCAPE '\\'");
        assert_eq!(
            s.format,
            Format::Csv {
                delimiter: b'|',
                null: String::new(),
                quote: b'\'',
                escape: b'\\',
                header: true
            }
        );
        // What asyncpg and pgx send.
        assert_eq!(
            spec("COPY \"t\"(\"a\", \"b\") FROM STDIN (FORMAT binary)").format,
            Format::Binary
        );
        assert_eq!(
            spec("copy \"t\" ( \"a\", \"b\" ) from stdin binary;").format,
            Format::Binary
        );
        assert!(parse("get docs").is_none());
        assert!(parse("copying things").is_none());
        for (sql, code) in [
            ("COPY t TO STDOUT", "0A000"),
            ("COPY t FROM '/etc/passwd'", "0A000"),
            ("COPY t FROM PROGRAM 'ls'", "0A000"),
            ("COPY t FROM STDIN (FORMAT binary, DELIMITER ',')", "42601"),
            ("COPY t FROM STDIN WITH BINARY CSV", "42601"),
            ("COPY t FROM STDIN (HEADER)", "0A000"),
            ("COPY t FROM STDIN (DELIMITER ';;')", "0A000"),
            ("COPY (select 1) TO STDOUT", "0A000"),
            ("COPY other.t FROM STDIN", "3F000"),
            ("COPY t (a b) FROM STDIN", "42601"),
            ("COPY t FROM STDIN; SELECT 1", "0A000"),
        ] {
            assert_eq!(parse(sql).unwrap().unwrap_err().0, code, "{sql}");
        }
    }

    fn read(format: Format, chunks: &[&[u8]]) -> Result<Vec<Numbered>, Refusal> {
        let mut r = Reader::new(format);
        let mut rows = Vec::new();
        for c in chunks {
            r.feed(c, &mut rows)?;
        }
        r.finish(&mut rows)?;
        Ok(rows)
    }

    fn text() -> Format {
        Format::Text {
            delimiter: b'\t',
            null: "\\N".into(),
        }
    }

    fn csv(header: bool) -> Format {
        Format::Csv {
            delimiter: b',',
            null: String::new(),
            quote: b'"',
            escape: b'"',
            header,
        }
    }

    fn cells(rows: &[Numbered]) -> Vec<Vec<Option<&str>>> {
        rows.iter()
            .map(|(_, r)| {
                r.iter()
                    .map(|c| match c {
                        Some(Cell::Text(s)) => Some(s.as_str()),
                        Some(Cell::Binary(_)) => Some("<binary>"),
                        None => None,
                    })
                    .collect()
            })
            .collect()
    }

    /// A binary COPY stream: the header, then each row's cells.
    fn pgcopy(rows: &[&[Option<&[u8]>]], trailer: bool) -> Vec<u8> {
        let mut out = b"PGCOPY\n\xff\r\n\0".to_vec();
        out.extend_from_slice(&0i32.to_be_bytes());
        out.extend_from_slice(&4i32.to_be_bytes());
        out.extend_from_slice(b"ext!");
        for r in rows {
            out.extend_from_slice(&(r.len() as i16).to_be_bytes());
            for c in r.iter() {
                match c {
                    None => out.extend_from_slice(&(-1i32).to_be_bytes()),
                    Some(b) => {
                        out.extend_from_slice(&(b.len() as i32).to_be_bytes());
                        out.extend_from_slice(b);
                    }
                }
            }
        }
        if trailer {
            out.extend_from_slice(&(-1i16).to_be_bytes());
        }
        out
    }

    #[test]
    fn binary_rows_across_messages_and_their_cells() {
        let n = 7i64.to_be_bytes();
        let stream = pgcopy(&[&[Some(b"a"), Some(&n)], &[None, Some(&n)]], true);
        // Cut at every byte, a row, the header and a cell's length included.
        for cut in 1..stream.len() {
            let (a, b) = stream.split_at(cut);
            let rows = read(Format::Binary, &[a, b]).unwrap();
            assert_eq!(rows.len(), 2, "cut at {cut}");
            assert_eq!(rows[0].1[0], Some(Cell::Binary(b"a".to_vec())));
            assert_eq!(rows[1].1[0], None);
        }
        // A stream may end at a row's end without its trailer; not inside one.
        let open = pgcopy(&[&[Some(b"a")]], false);
        assert_eq!(read(Format::Binary, &[&open]).unwrap().len(), 1);
        assert_eq!(
            read(Format::Binary, &[&open[..open.len() - 1]])
                .unwrap_err()
                .0,
            "22P04"
        );
        assert_eq!(
            read(Format::Binary, &[b"PGCOPY-nope--------"])
                .unwrap_err()
                .0,
            "22P04"
        );

        let t = Target {
            collection: "t".into(),
            columns: vec![
                ("name".into(), DataType::Text),
                ("n".into(), DataType::Int),
                (
                    "e".into(),
                    DataType::Vector(2, fenec_core::value::VecPrec::F32),
                ),
            ],
        };
        let row = vec![
            Some(Cell::Binary(b"a".to_vec())),
            Some(Cell::Binary(n.to_vec())),
            Some(Cell::Binary(b"[1,0.5]".to_vec())),
        ];
        let doc = document(&t, row, 1).unwrap();
        assert_eq!(doc[1].1, Expr::Lit(Value::Int(7)));
        let short = vec![None, Some(Cell::Binary(vec![0, 7])), None];
        assert_eq!(document(&t, short, 3).unwrap_err().0, "22P02");
    }

    #[test]
    fn text_rows_across_messages() {
        let rows = read(
            text(),
            &[
                b"a\t1\t\\N\nb\\tc\t2\t[1,",
                b"2]\r\nlast\t3\tx\\\\y",
                b"\n\\.\nignored\n",
            ],
        )
        .unwrap();
        assert_eq!(
            cells(&rows),
            [
                vec![Some("a"), Some("1"), None],
                vec![Some("b\tc"), Some("2"), Some("[1,2]")],
                vec![Some("last"), Some("3"), Some("x\\y")],
            ]
        );
        // No newline after the last row.
        let rows = read(text(), &[b"a\t1\nb\t2"]).unwrap();
        assert_eq!(
            cells(&rows),
            [vec![Some("a"), Some("1")], vec![Some("b"), Some("2")]]
        );
        assert_eq!(unescape(b"\\101\\x42\\q"), b"ABq");
    }

    #[test]
    fn csv_rows_with_quotes_newlines_and_nulls() {
        let rows = read(
            csv(true),
            &[
                b"name,note\n\"a, b\",\"said \"\"hi\"\"\"\n",
                b"\"multi\nline\",\n,\"\"\n\"split",
                b" field\",x",
            ],
        )
        .unwrap();
        assert_eq!(
            cells(&rows),
            [
                vec![Some("a, b"), Some("said \"hi\"")],
                vec![Some("multi\nline"), None],
                vec![None, Some("")],
                vec![Some("split field"), Some("x")],
            ]
        );
        // A doubled quote cut between two messages.
        let rows = read(csv(false), &[b"\"a\"", b"\"b\",c\n"]).unwrap();
        assert_eq!(cells(&rows), [vec![Some("a\"b"), Some("c")]]);
        assert_eq!(read(csv(false), &[b"\"open\n"]).unwrap_err().0, "22P04");
    }

    #[test]
    fn cells_read_as_their_fields() {
        assert_eq!(value(" 42 ", &DataType::Int).unwrap(), Value::Int(42));
        assert_eq!(value("t", &DataType::Bool).unwrap(), Value::Bool(true));
        assert_eq!(value("off", &DataType::Bool).unwrap(), Value::Bool(false));
        assert_eq!(value("2.5", &DataType::Float).unwrap(), Value::Float(2.5));
        assert!(matches!(value("NaN", &DataType::Float).unwrap(), Value::Float(f) if f.is_nan()));
        assert_eq!(
            value(
                "[1,2.5]",
                &DataType::Vector(2, fenec_core::value::VecPrec::F32)
            )
            .unwrap(),
            Value::Vector(vec![1.0, 2.5])
        );
        assert_eq!(
            value("\\x0aff", &DataType::Bytes).unwrap(),
            Value::Bytes(vec![10, 255])
        );
        assert_eq!(
            value("{1,NULL,\"3\"}", &DataType::List(Box::new(DataType::Int))).unwrap(),
            Value::List(vec![Value::Int(1), Value::Null, Value::Int(3)])
        );
        assert!(value("abc", &DataType::Int).is_err());
        assert!(value("maybe", &DataType::Bool).is_err());
        assert!(value("[1,", &DataType::Vector(2, fenec_core::value::VecPrec::F32)).is_err());
        assert_eq!(
            value(
                "{1, 2.5}",
                &DataType::Vector(2, fenec_core::value::VecPrec::F32)
            )
            .unwrap(),
            Value::List(vec![Value::Float(1.0), Value::Float(2.5)])
        );
        assert!(value(
            "{1,x}",
            &DataType::Vector(2, fenec_core::value::VecPrec::F32)
        )
        .is_err());
    }
}
