//! A read-only scanner that reads the SQLite file format directly.
//!
//! Why it is hand-written: no fenecdb crate depends on anything external (see
//! `fenec_core` lib.rs, design decision 1). What it reads is the table b-tree;
//! indexes, triggers, views and the WAL are out of scope -- the only thing
//! an import needs is the rows.
//!
//! Referans: <https://www.sqlite.org/fileformat2.html>

use crate::{Column, Source};
use fenec_core::error::{Error, Result};
use fenec_core::value::{DataType, Value};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const MAGIC: &[u8; 16] = b"SQLite format 3\0";

const LEAF_TABLE: u8 = 0x0d;
const INTERIOR_TABLE: u8 = 0x05;
const LEAF_INDEX: u8 = 0x0a;
const INTERIOR_INDEX: u8 = 0x02;

/// Rows to scan for columns that have no declared type.
pub const DEFAULT_SAMPLE: usize = 1_000;

/// `sqlite_master` is always rooted on page 1.
const MASTER_ROOT: u32 = 1;

fn corrupt(what: impl std::fmt::Display) -> Error {
    Error::Corrupt(format!("sqlite: {what}"))
}

// ------------------------------------------------------------------ varint

/// SQLite varint: 1-9 bytes, big-endian. The top bit of the first eight
/// bytes is a continuation flag and they carry seven bits; the ninth byte
/// contributes all eight.
fn varint(b: &[u8], pos: &mut usize) -> Result<i64> {
    let mut v: u64 = 0;
    for i in 0..9 {
        let byte = *b.get(*pos).ok_or_else(|| corrupt("truncated varint"))?;
        *pos += 1;
        if i == 8 {
            return Ok(((v << 8) | byte as u64) as i64);
        }
        v = (v << 7) | (byte & 0x7f) as u64;
        if byte & 0x80 == 0 {
            return Ok(v as i64);
        }
    }
    unreachable!()
}

fn be16(b: &[u8], at: usize) -> Result<usize> {
    let s = b
        .get(at..at + 2)
        .ok_or_else(|| corrupt("the page is shorter than expected"))?;
    Ok(u16::from_be_bytes([s[0], s[1]]) as usize)
}

fn be32(b: &[u8], at: usize) -> Result<u32> {
    let s = b
        .get(at..at + 4)
        .ok_or_else(|| corrupt("the page is shorter than expected"))?;
    Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

/// An n-byte signed big-endian integer.
fn be_int(b: &[u8], pos: &mut usize, n: usize) -> Result<i64> {
    let s = b
        .get(*pos..*pos + n)
        .ok_or_else(|| corrupt("truncated record body"))?;
    *pos += n;
    let mut v: i64 = if s[0] & 0x80 != 0 { -1 } else { 0 };
    for &byte in s {
        v = (v << 8) | byte as i64;
    }
    Ok(v)
}

// ---------------------------------------------------------------- encoding

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
}

fn decode_text(b: &[u8], enc: Encoding) -> String {
    match enc {
        Encoding::Utf8 => String::from_utf8_lossy(b).into_owned(),
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let units = b.as_chunks::<2>().0.iter().map(|c| {
                if enc == Encoding::Utf16Le {
                    u16::from_le_bytes(*c)
                } else {
                    u16::from_be_bytes(*c)
                }
            });
            char::decode_utf16(units)
                .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect()
        }
    }
}

// ------------------------------------------------------------------ pager

/// The page reader. Pages are numbered from 1.
#[derive(Debug)]
struct Pager {
    file: File,
    page_size: usize,
    /// The usable part of a page once the reserved bytes are taken off.
    usable: usize,
    encoding: Encoding,
}

impl Pager {
    fn open(path: &Path) -> Result<Pager> {
        let mut file =
            File::open(path).map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
        let mut head = [0u8; 100];
        file.read_exact(&mut head)
            .map_err(|_| Error::Corrupt(format!("{}: not a SQLite file", path.display())))?;
        if &head[..16] != MAGIC {
            return Err(Error::Corrupt(format!(
                "{}: not a SQLite file",
                path.display()
            )));
        }
        // The value 1 means 65536 -- the field is 16 bits and the value does not fit.
        let page_size = match u16::from_be_bytes([head[16], head[17]]) {
            1 => 65_536,
            n => n as usize,
        };
        if page_size < 512 || !page_size.is_power_of_two() {
            return Err(corrupt(format!("invalid page size {page_size}")));
        }
        let reserved = head[20] as usize;
        if reserved >= page_size {
            return Err(corrupt("the reserved bytes exceed the page"));
        }
        let encoding = match u32::from_be_bytes([head[56], head[57], head[58], head[59]]) {
            0 | 1 => Encoding::Utf8,
            2 => Encoding::Utf16Le,
            3 => Encoding::Utf16Be,
            n => return Err(corrupt(format!("unknown text encoding {n}"))),
        };
        Ok(Pager {
            file,
            page_size,
            usable: page_size - reserved,
            encoding,
        })
    }

    fn read(&mut self, n: u32) -> Result<Vec<u8>> {
        if n == 0 {
            return Err(corrupt("there is no page 0"));
        }
        let at = (n as u64 - 1) * self.page_size as u64;
        self.file
            .seek(SeekFrom::Start(at))
            .map_err(|e| Error::Io(e.to_string()))?;
        let mut buf = vec![0u8; self.page_size];
        self.file
            .read_exact(&mut buf)
            .map_err(|_| corrupt(format!("could not read page {n} (the file is short)")))?;
        Ok(buf)
    }

    /// The payload length that fits inside the page on a table leaf.
    fn local_len(&self, p: usize) -> usize {
        let u = self.usable;
        let x = u - 35;
        if p <= x {
            return p;
        }
        let m = ((u - 12) * 32 / 255) - 23;
        let k = m + (p - m) % (u - 4);
        if k <= x {
            k
        } else {
            m
        }
    }

    /// Collects the cell payload; the part that does not fit in the page
    /// comes from the overflow chain.
    fn payload(&mut self, page: &[u8], at: usize, total: usize) -> Result<Vec<u8>> {
        let local = self.local_len(total);
        let head = page
            .get(at..at + local)
            .ok_or_else(|| corrupt("the cell payload does not fit the page"))?;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(head);
        if local == total {
            return Ok(out);
        }
        let mut next = be32(page, at + local)?;
        let per_page = self.usable - 4;
        while next != 0 && out.len() < total {
            let ov = self.read(next)?;
            next = be32(&ov, 0)?;
            let take = (total - out.len()).min(per_page);
            out.extend_from_slice(
                ov.get(4..4 + take)
                    .ok_or_else(|| corrupt("the overflow page is short"))?,
            );
        }
        if out.len() != total {
            return Err(corrupt("the overflow chain ended early"));
        }
        Ok(out)
    }
}

/// A single cell on a table leaf.
#[derive(Debug)]
struct Cell {
    rowid: i64,
    payload: Vec<u8>,
}

/// Where the b-tree header sits inside the page. On page 1 the 100-byte file
/// header comes first.
fn header_at(page_no: u32) -> usize {
    if page_no == MASTER_ROOT {
        100
    } else {
        0
    }
}

// ------------------------------------------------------------ b-tree walk

/// A walker that starts at the root page and visits the leaves in rowid order.
#[derive(Debug)]
struct Walk {
    /// Pages still to visit; `pop` gives the next one.
    stack: Vec<u32>,
    /// What is left of the current leaf, in reverse order.
    pending: Vec<Cell>,
}

impl Walk {
    fn new(root: u32) -> Walk {
        Walk {
            stack: vec![root],
            pending: Vec::new(),
        }
    }

    fn next(&mut self, pager: &mut Pager) -> Result<Option<Cell>> {
        loop {
            if let Some(c) = self.pending.pop() {
                return Ok(Some(c));
            }
            let Some(page_no) = self.stack.pop() else {
                return Ok(None);
            };
            let page = pager.read(page_no)?;
            let h = header_at(page_no);
            let kind = *page.get(h).ok_or_else(|| corrupt("no page header"))?;
            let ncells = be16(&page, h + 3)?;
            match kind {
                LEAF_TABLE => {
                    let ptrs = h + 8;
                    // Push in reverse order so `pop` yields rowid order.
                    for i in (0..ncells).rev() {
                        let at = be16(&page, ptrs + i * 2)?;
                        let mut pos = at;
                        let total = varint(&page, &mut pos)? as usize;
                        let rowid = varint(&page, &mut pos)?;
                        let payload = pager.payload(&page, pos, total)?;
                        self.pending.push(Cell { rowid, payload });
                    }
                }
                INTERIOR_TABLE => {
                    let ptrs = h + 12;
                    let rightmost = be32(&page, h + 8)?;
                    self.stack.push(rightmost);
                    for i in (0..ncells).rev() {
                        let at = be16(&page, ptrs + i * 2)?;
                        self.stack.push(be32(&page, at)?);
                    }
                }
                LEAF_INDEX | INTERIOR_INDEX => {
                    return Err(Error::Query(
                        "the table is declared `WITHOUT ROWID`; fenec-import \
                         only reads rowid tables"
                            .into(),
                    ))
                }
                other => return Err(corrupt(format!("unknown page type {other}"))),
            }
        }
    }
}

// ------------------------------------------------------------------ record

/// Parses the record body into values. Missing trailing columns become NULL:
/// that is how older rows look after an `ALTER TABLE ADD COLUMN`.
fn record(payload: &[u8], enc: Encoding, ncols: usize) -> Result<Vec<Value>> {
    let mut pos = 0;
    let header_len = varint(payload, &mut pos)? as usize;
    if header_len > payload.len() {
        return Err(corrupt("the record header is longer than the body"));
    }
    let mut serials = Vec::with_capacity(ncols);
    while pos < header_len {
        serials.push(varint(payload, &mut pos)?);
    }
    let mut body = header_len;
    let mut out = Vec::with_capacity(ncols);
    for s in serials {
        out.push(value(s, payload, &mut body, enc)?);
    }
    out.resize(ncols, Value::Null);
    out.truncate(ncols);
    Ok(out)
}

fn value(serial: i64, b: &[u8], pos: &mut usize, enc: Encoding) -> Result<Value> {
    Ok(match serial {
        0 => Value::Null,
        1 => Value::Int(be_int(b, pos, 1)?),
        2 => Value::Int(be_int(b, pos, 2)?),
        3 => Value::Int(be_int(b, pos, 3)?),
        4 => Value::Int(be_int(b, pos, 4)?),
        5 => Value::Int(be_int(b, pos, 6)?),
        6 => Value::Int(be_int(b, pos, 8)?),
        7 => {
            let s = b
                .get(*pos..*pos + 8)
                .ok_or_else(|| corrupt("the float body is short"))?;
            *pos += 8;
            Value::Float(f64::from_be_bytes([
                s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
            ]))
        }
        8 => Value::Int(0),
        9 => Value::Int(1),
        10 | 11 => return Err(corrupt(format!("reserved serial type {serial}"))),
        n if n >= 12 => {
            let len = ((n - 12) / 2) as usize;
            let s = b
                .get(*pos..*pos + len)
                .ok_or_else(|| corrupt("the value body is short"))?;
            *pos += len;
            if n % 2 == 0 {
                Value::Bytes(s.to_vec())
            } else {
                Value::Text(decode_text(s, enc))
            }
        }
        n => return Err(corrupt(format!("invalid serial type {n}"))),
    })
}

// --------------------------------------------------------- schema parsing

/// A single column declaration inside a `CREATE TABLE` body.
#[derive(Debug, Clone, PartialEq)]
struct Decl {
    name: String,
    /// The declared type, verbatim. SQLite allows an empty type.
    ty: String,
    /// `INTEGER PRIMARY KEY` -- the value is in the rowid, not in the record.
    rowid_alias: bool,
}

/// Keywords that begin a column constraint; the type ends before them.
const COLUMN_CONSTRAINTS: &[&str] = &[
    "CONSTRAINT",
    "PRIMARY",
    "NOT",
    "NULL",
    "UNIQUE",
    "CHECK",
    "DEFAULT",
    "COLLATE",
    "REFERENCES",
    "GENERATED",
    "AS",
    "AUTOINCREMENT",
];

/// Keywords that begin a table-level constraint; these are not columns.
const TABLE_CONSTRAINTS: &[&str] = &["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN"];

/// Parses the column list inside `CREATE TABLE ... ( ... )`.
fn parse_create(sql: &str) -> Result<Vec<Decl>> {
    let open = sql
        .find('(')
        .ok_or_else(|| corrupt("there is no CREATE TABLE body"))?;
    let inner = balanced_body(&sql[open..])
        .ok_or_else(|| corrupt("the CREATE TABLE parenthesis does not close"))?;

    let mut decls = Vec::new();
    // A table-level `PRIMARY KEY (x)`: if x is INTEGER it is a rowid alias too.
    let mut table_pk: Option<String> = None;

    for part in split_top(inner) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let upper_first = first_word(part).to_ascii_uppercase();
        if TABLE_CONSTRAINTS.contains(&upper_first.as_str()) {
            if upper_first == "PRIMARY" {
                if let Some(body) = part.find('(').and_then(|i| balanced_body(&part[i..])) {
                    let cols = split_top(body);
                    if cols.len() == 1 {
                        table_pk = Some(unquote(cols[0].trim()));
                    }
                }
            }
            continue;
        }
        let (name, rest) = take_name(part);
        if name.is_empty() {
            continue;
        }
        let (ty, tail) = take_type(rest);
        let rowid_alias = ty.trim().eq_ignore_ascii_case("INTEGER") && has_primary_key(tail);
        decls.push(Decl {
            name,
            ty: ty.trim().to_string(),
            rowid_alias,
        });
    }

    if let Some(pk) = table_pk {
        for d in &mut decls {
            if d.name == pk && d.ty.trim().eq_ignore_ascii_case("INTEGER") {
                d.rowid_alias = true;
            }
        }
    }
    if decls.is_empty() {
        return Err(corrupt("no column was found in the table"));
    }
    Ok(decls)
}

/// Given a string starting with `(`, returns the body up to the matching `)`.
fn balanced_body(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    if b.first() != Some(&b'(') {
        return None;
    }
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    for (i, &c) in b.iter().enumerate() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                b'\'' | b'"' | b'`' => quote = Some(c),
                b'[' => quote = Some(b']'),
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&s[1..i]);
                    }
                }
                _ => {}
            },
        }
    }
    None
}

/// Splits on depth-0 commas.
fn split_top(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut quote: Option<u8> = None;
    for (i, &c) in b.iter().enumerate() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                b'\'' | b'"' | b'`' => quote = Some(c),
                b'[' => quote = Some(b']'),
                b'(' => depth += 1,
                b')' => depth = depth.saturating_sub(1),
                b',' if depth == 0 => {
                    out.push(&s[start..i]);
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    out.push(&s[start..]);
    out
}

fn first_word(s: &str) -> &str {
    s.trim()
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("")
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() >= 2 {
        let (open, close) = (b[0], b[b.len() - 1]);
        let pair = matches!(
            (open, close),
            (b'"', b'"') | (b'\'', b'\'') | (b'`', b'`') | (b'[', b']')
        );
        if pair {
            let inner = &s[1..s.len() - 1];
            // A quote is escaped by doubling it in SQL.
            return match open {
                b'"' => inner.replace("\"\"", "\""),
                b'\'' => inner.replace("''", "'"),
                b'`' => inner.replace("``", "`"),
                _ => inner.to_string(),
            };
        }
    }
    s.to_string()
}

/// Takes the name off the front of a column declaration.
fn take_name(s: &str) -> (String, &str) {
    let s = s.trim_start();
    let b = s.as_bytes();
    if b.is_empty() {
        return (String::new(), "");
    }
    let close = match b[0] {
        b'"' => Some(b'"'),
        b'\'' => Some(b'\''),
        b'`' => Some(b'`'),
        b'[' => Some(b']'),
        _ => None,
    };
    match close {
        Some(q) => {
            let mut i = 1;
            while i < b.len() {
                if b[i] == q {
                    // A doubled quote: an escape, keep going.
                    if i + 1 < b.len() && b[i + 1] == q {
                        i += 2;
                        continue;
                    }
                    return (unquote(&s[..=i]), &s[i + 1..]);
                }
                i += 1;
            }
            (unquote(s), "")
        }
        None => {
            let end = s
                .find(|c: char| c.is_whitespace() || c == '(')
                .unwrap_or(s.len());
            (s[..end].to_string(), &s[end..])
        }
    }
}

/// Takes the type after the name; the type ends at the first constraint
/// keyword. `VARCHAR(255)` and `DECIMAL(10,2)` stay inside parentheses and
/// are not split.
fn take_type(s: &str) -> (String, &str) {
    let mut depth = 0usize;
    let mut end = s.len();
    let mut i = 0usize;
    let b = s.as_bytes();
    while i < b.len() {
        let c = b[i] as char;
        if c == '(' {
            depth += 1;
            i += 1;
            continue;
        }
        if c == ')' {
            depth = depth.saturating_sub(1);
            i += 1;
            continue;
        }
        if depth == 0 && !c.is_whitespace() {
            let word_end = s[i..]
                .find(|c: char| c.is_whitespace() || c == '(')
                .map(|k| i + k)
                .unwrap_or(s.len());
            let word = s[i..word_end].to_ascii_uppercase();
            if COLUMN_CONSTRAINTS.contains(&word.as_str()) {
                end = i;
                break;
            }
            i = word_end;
            continue;
        }
        i += 1;
    }
    (s[..end].to_string(), &s[end..])
}

/// Whether the constraint tail contains `PRIMARY KEY`.
fn has_primary_key(tail: &str) -> bool {
    let up = tail.to_ascii_uppercase();
    let mut words = up.split_whitespace();
    while let Some(w) = words.next() {
        if w == "PRIMARY" && words.next() == Some("KEY") {
            return true;
        }
    }
    false
}

// ------------------------------------------------------------ type inference

/// The fenecdb type from the declared one. It follows SQLite's affinity rules,
/// but first checks the special cases that have a fenecdb counterpart.
fn map_decl(name: &str, decl: &str) -> Column {
    let d = decl.trim().to_ascii_uppercase();
    let src = if decl.trim().is_empty() {
        "(untyped)".to_string()
    } else {
        decl.trim().to_string()
    };
    let has = |k: &str| d.contains(k);

    if has("BOOL") {
        return Column::new(name, DataType::Bool, src);
    }
    if has("DATETIME") || has("TIMESTAMP") || d == "DATE" {
        return Column::new(name, DataType::Timestamp, src);
    }
    if has("INT") {
        return Column::new(name, DataType::Int, src);
    }
    if has("CHAR") || has("CLOB") || has("TEXT") {
        return Column::new(name, DataType::Text, src);
    }
    if has("BLOB") {
        return Column::new(name, DataType::Bytes, src);
    }
    if has("REAL") || has("FLOA") || has("DOUB") {
        return Column::new(name, DataType::Float, src);
    }
    if has("DEC") || has("NUMERIC") || has("MONEY") {
        return Column::unsupported(
            name,
            src,
            "fenecdb has no decimal. SQLite stores this column as an int or a
             float too, so the exact value is already absent from the source;
             choose `--cast <field>=float` or `=int`",
        );
    }
    // An untyped column: resolved from a value sample, which the caller fills in.
    Column::unsupported(name, src, "no declared type")
}

/// The type inferred from sample values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Seen {
    #[default]
    Nothing,
    Int,
    Float,
    Text,
    Blob,
}

impl Seen {
    fn of(v: &Value) -> Seen {
        match v {
            Value::Null => Seen::Nothing,
            Value::Int(_) => Seen::Int,
            Value::Float(_) => Seen::Float,
            Value::Text(_) => Seen::Text,
            _ => Seen::Blob,
        }
    }
    /// The common supertype of two observations. Bytes is the widest:
    /// `adapt` can turn a number or text into bytes, the reverse is not always true.
    fn join(self, other: Seen) -> Seen {
        use Seen::*;
        match (self, other) {
            (Nothing, x) | (x, Nothing) => x,
            (a, b) if a == b => a,
            (Blob, _) | (_, Blob) => Blob,
            (Text, _) | (_, Text) => Text,
            (Int, Float) | (Float, Int) => Float,
            _ => Text,
        }
    }
    fn ty(self) -> DataType {
        match self {
            // No value was seen: text is the most harmless choice.
            Seen::Nothing | Seen::Text => DataType::Text,
            Seen::Int => DataType::Int,
            Seen::Float => DataType::Float,
            Seen::Blob => DataType::Bytes,
        }
    }
}

// ------------------------------------------------------------------ reader

/// A source that reads a SQLite table row by row.
#[derive(Debug)]
pub struct Reader {
    pager: Pager,
    walk: Walk,
    columns: Vec<Column>,
    decls: Vec<Decl>,
    rowid_alias: Option<usize>,
}

impl Reader {
    /// Opens the file and finds the table.
    pub fn open(path: impl AsRef<Path>, table: &str) -> Result<Reader> {
        Reader::open_sampled(path, table, DEFAULT_SAMPLE)
    }

    /// [`Reader::open`], but with the number of rows to scan for columns
    /// that have no declared type.
    pub fn open_sampled(path: impl AsRef<Path>, table: &str, sample: usize) -> Result<Reader> {
        let path = path.as_ref();
        check_wal(path)?;
        let mut pager = Pager::open(path)?;
        let (root, sql) = find_table(&mut pager, table)?;
        let decls = parse_create(&sql)?;

        let mut columns: Vec<Column> = decls
            .iter()
            .map(|d| {
                if d.rowid_alias {
                    // A rowid is always an i64, whatever the declared type says.
                    Column::new(&d.name, DataType::Int, "INTEGER PRIMARY KEY")
                } else {
                    map_decl(&d.name, &d.ty)
                }
            })
            .collect();

        // Columns with no declared type are resolved from a value sample.
        let untyped: Vec<usize> = columns
            .iter()
            .enumerate()
            .filter(|(i, c)| c.ty.is_none() && decls[*i].ty.trim().is_empty())
            .map(|(i, _)| i)
            .collect();
        if !untyped.is_empty() && sample > 0 {
            let seen = sample_types(&mut pager, root, decls.len(), sample)?;
            for i in untyped {
                let ty = seen[i].ty();
                columns[i] = Column::new(&decls[i].name, ty.clone(), "(untyped)").note(format!(
                    "no declared type; {} was chosen from the first {sample} rows",
                    ty.name()
                ));
            }
        }

        let rowid_alias = decls.iter().position(|d| d.rowid_alias);
        Ok(Reader {
            pager,
            walk: Walk::new(root),
            columns,
            decls,
            rowid_alias,
        })
    }

    /// The number of columns in the table.
    pub fn width(&self) -> usize {
        self.decls.len()
    }
}

impl Source for Reader {
    fn columns(&mut self) -> Result<Vec<Column>> {
        Ok(self.columns.clone())
    }

    fn next_row(&mut self) -> Result<Option<Vec<Value>>> {
        let Some(cell) = self.walk.next(&mut self.pager)? else {
            return Ok(None);
        };
        let mut values = record(&cell.payload, self.pager.encoding, self.decls.len())?;
        // The `INTEGER PRIMARY KEY` column is stored as NULL in the record;
        // the real value is the cell's rowid.
        if let Some(i) = self.rowid_alias {
            values[i] = Value::Int(cell.rowid);
        }
        Ok(Some(values))
    }
}

/// Counts the rows in the table.
///
/// It is not cheap: the whole table b-tree is walked. SQLite keeps the row
/// count nowhere, so counting means a full scan -- the call site has to ask
/// for it explicitly.
pub fn count_rows(path: impl AsRef<Path>, table: &str) -> Result<u64> {
    let path = path.as_ref();
    check_wal(path)?;
    let mut pager = Pager::open(path)?;
    let (root, _) = find_table(&mut pager, table)?;
    let mut walk = Walk::new(root);
    let mut n = 0u64;
    // The record body is not decoded; only the cells are counted.
    while walk.next(&mut pager)?.is_some() {
        n += 1;
    }
    Ok(n)
}

/// When the WAL holds uncommitted changes, the main file shows stale data.
/// Stopping beats silently returning stale data.
fn check_wal(path: &Path) -> Result<()> {
    let wal = path.with_file_name(format!(
        "{}-wal",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    match std::fs::metadata(&wal) {
        Ok(m) if m.len() > 0 => Err(Error::Query(format!(
            "{} holds unprocessed changes; first run \
             `sqlite3 {} \"PRAGMA wal_checkpoint(TRUNCATE)\"`",
            wal.display(),
            path.display()
        ))),
        _ => Ok(()),
    }
}

/// Finds the table in `sqlite_master`: (root page, CREATE TABLE text).
fn find_table(pager: &mut Pager, table: &str) -> Result<(u32, String)> {
    let mut walk = Walk::new(MASTER_ROOT);
    let mut tables = Vec::new();
    while let Some(cell) = walk.next(pager)? {
        // sqlite_master: (type, name, tbl_name, rootpage, sql)
        let r = record(&cell.payload, pager.encoding, 5)?;
        let kind = r[0].as_text().unwrap_or_default();
        if kind != "table" {
            continue;
        }
        let name = r[1].as_text().unwrap_or_default().to_string();
        if name.eq_ignore_ascii_case(table) {
            let Value::Int(root) = r[3] else {
                return Err(corrupt("the root page is not a number"));
            };
            let sql = r[4].as_text().unwrap_or_default().to_string();
            if sql.is_empty() {
                return Err(Error::Query(format!(
                    "`{name}` is an internal table; it cannot be imported"
                )));
            }
            return Ok((root as u32, sql));
        }
        tables.push(name);
    }
    tables.sort();
    Err(Error::NotFound(if tables.is_empty() {
        format!("there is no `{table}` table; no table was found in the file")
    } else {
        format!(
            "there is no `{table}` table; the tables in the file: {}",
            tables.join(", ")
        )
    }))
}

/// Infers the observed type per column by looking at the first `limit` rows.
fn sample_types(pager: &mut Pager, root: u32, ncols: usize, limit: usize) -> Result<Vec<Seen>> {
    let mut seen = vec![Seen::Nothing; ncols];
    let mut walk = Walk::new(root);
    let mut n = 0;
    while n < limit {
        let Some(cell) = walk.next(pager)? else { break };
        let values = record(&cell.payload, pager.encoding, ncols)?;
        for (s, v) in seen.iter_mut().zip(&values) {
            *s = s.join(Seen::of(v));
        }
        n += 1;
    }
    Ok(seen)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_match_the_spec() {
        let case = |bytes: &[u8], want: i64| {
            let mut pos = 0;
            assert_eq!(varint(bytes, &mut pos).unwrap(), want, "{bytes:?}");
            assert_eq!(pos, bytes.len(), "{bytes:?}");
        };
        case(&[0x00], 0);
        case(&[0x7f], 127);
        case(&[0x81, 0x00], 128);
        case(&[0x82, 0x2f], 303);
        case(&[0xff, 0x7f], 16383);
        // The ninth byte contributes all eight bits.
        case(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff], -1);
    }

    #[test]
    fn signed_big_endian_sign_extends() {
        let mut p = 0;
        assert_eq!(be_int(&[0xff], &mut p, 1).unwrap(), -1);
        let mut p = 0;
        assert_eq!(be_int(&[0xff, 0xff, 0xff], &mut p, 3).unwrap(), -1);
        let mut p = 0;
        assert_eq!(be_int(&[0x00, 0x80], &mut p, 2).unwrap(), 128);
    }

    #[test]
    fn create_table_columns_are_parsed() {
        let d = parse_create(
            r#"CREATE TABLE docs (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 "head line" VARCHAR(255) NOT NULL DEFAULT 'x',
                 price DECIMAL(10,2),
                 embed BLOB,
                 untyped,
                 FOREIGN KEY (embed) REFERENCES t(id)
               )"#,
        )
        .unwrap();
        let names: Vec<&str> = d.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["id", "head line", "price", "embed", "untyped"]);
        assert_eq!(d[1].ty, "VARCHAR(255)");
        assert_eq!(
            d[2].ty, "DECIMAL(10,2)",
            "a parenthesised type must not be split"
        );
        assert_eq!(d[4].ty, "", "an untyped column carries an empty type");
        assert!(d[0].rowid_alias);
        assert!(!d[3].rowid_alias);
    }

    /// A table-level `PRIMARY KEY (x)` produces a rowid alias too.
    #[test]
    fn table_level_primary_key_is_rowid_alias() {
        let d = parse_create("CREATE TABLE t (a INTEGER, b TEXT, PRIMARY KEY (a))").unwrap();
        assert!(d[0].rowid_alias);
        assert!(!d[1].rowid_alias);
        // A multi-column primary key is not a rowid alias.
        let d = parse_create("CREATE TABLE t (a INTEGER, b INTEGER, PRIMARY KEY (a, b))").unwrap();
        assert!(!d[0].rowid_alias);
    }

    #[test]
    fn quoted_identifiers_survive() {
        let d = parse_create(
            r#"CREATE TABLE "t" ([with space] TEXT, `back` INT, "double""quote" INT)"#,
        )
        .unwrap();
        let names: Vec<&str> = d.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["with space", "back", "double\"quote"]);
    }

    #[test]
    fn declared_types_map_to_fenecdb_types() {
        let t = |decl: &str| map_decl("x", decl).ty;
        assert_eq!(t("INTEGER"), Some(DataType::Int));
        assert_eq!(t("BIGINT"), Some(DataType::Int));
        assert_eq!(t("VARCHAR(80)"), Some(DataType::Text));
        assert_eq!(t("CLOB"), Some(DataType::Text));
        assert_eq!(t("BLOB"), Some(DataType::Bytes));
        assert_eq!(t("REAL"), Some(DataType::Float));
        assert_eq!(t("DOUBLE PRECISION"), Some(DataType::Float));
        assert_eq!(t("BOOLEAN"), Some(DataType::Bool));
        assert_eq!(t("DATETIME"), Some(DataType::Timestamp));
        assert_eq!(t("DATE"), Some(DataType::Timestamp));
        // The ones with no counterpart require `--cast`.
        assert_eq!(t("DECIMAL(10,2)"), None);
        assert_eq!(t("NUMERIC"), None);
        assert_eq!(t(""), None);
    }

    #[test]
    fn sampled_types_take_the_wider_side() {
        use Seen::*;
        assert_eq!(Nothing.join(Int), Int);
        assert_eq!(Int.join(Float), Float);
        assert_eq!(Int.join(Text), Text);
        assert_eq!(Text.join(Blob), Blob);
        assert_eq!(Nothing.ty(), DataType::Text);
        assert_eq!(Float.ty(), DataType::Float);
    }

    /// The overflow threshold must match the formula in the format document.
    #[test]
    fn overflow_threshold_matches_the_format() {
        let p = Pager {
            file: File::open("/dev/null").unwrap(),
            page_size: 4096,
            usable: 4096,
            encoding: Encoding::Utf8,
        };
        // X = U - 35: everything up to this size stays in the page.
        assert_eq!(p.local_len(10), 10);
        assert_eq!(p.local_len(4061), 4061);
        // Above it, it overflows and the local part does not exceed X.
        assert!(p.local_len(4062) < 4062);
        assert!(p.local_len(100_000) <= 4061);
    }
}
