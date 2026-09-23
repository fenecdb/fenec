//! FenecQL parser: token stream -> `fenec_core::query::Statement`.
//!
//! Language summary
//! ```text
//! create collection [if not exists] <name> ( <field> <type> [@index], ... )
//! drop   collection [if exists] <name>
//! put    <name> { k: v, ... }            -- or [ {...}, {...} ]
//! get    <name> [select a, b] [where <expr>] [near <field> <vector> [ef N] [exact]]
//!            [match <field> <text>] [rerank <field> <vector> [candidates N]]
//!            [fuse [k N] [candidates N]]     -- match and near, by reciprocal rank
//!            [order <field> [collate tr] [asc|desc], ...] [limit N] [offset N] [count]
//!            [lookup <name> on <child> [= <parent>] [required] <clauses...>]
//! get    <name> select [<key>,] count(*) | sum(f) | avg(f) | min(f) | max(f), ...
//!            [where <expr>] [group <key> [order <column> [desc]] [limit N] [offset N]]
//! select a, b from <name> ...            -- the classic SQL order works too
//! set    <name> { k: v, ... } [where <expr>]
//! del    <name> [where <expr>]
//! collections | describe <name> | compact [<name>]
//! ```

use crate::lexer::{tokenize, Tok, Token};
use fenec_core::collate::Collation;
use fenec_core::error::{Error, Result};
use fenec_core::query::*;
use fenec_core::schema::{Field, IndexKind, Metric, Schema, TextIndexSpec, VectorIndexSpec};
use fenec_core::value::{DataType, Value, VecPrec};

/// The maximum nesting level of an expression.
///
/// Both the parser and the evaluator are recursive: depth is stack depth
/// directly. Without a limit, `((((...))))` or a long `or` chain overflows
/// the stack -- and a stack overflow is not a catchable panic but an
/// `abort` of the process; in `fenec-pg` a single query would take the whole
/// server down.
///
/// The cost of the limit was measured (511 levels, worst shape = parentheses):
///
/// | build | stack required |
/// |---|---|
/// | release | ~750 KiB |
/// | debug   | ~5 MiB   |
///
/// So `thread::spawn`'s 2 MiB default leaves ~2.7x headroom in release and
/// is *not enough* in a debug build. 512 is therefore not safe on its own:
/// whoever runs it sizes its stack explicitly (`SESSION_STACK` in `fenec-pg`,
/// 8 MiB in tests). The value stayed at 512 because generated `or` chains
/// easily reach hundreds of terms; a lower ceiling would cut off legitimate
/// queries.
pub const MAX_EXPR_DEPTH: usize = 512;

pub struct Parser {
    toks: Vec<Token>,
    i: usize,
    /// Stack depth of the expression currently being built.
    depth: usize,
}

pub fn parse(src: &str) -> Result<Vec<Statement>> {
    let mut p = Parser {
        toks: tokenize(src)?,
        i: 0,
        depth: 0,
    };
    let mut out = Vec::new();
    while !p.at_eof() {
        out.push(p.statement()?);
    }
    if out.is_empty() {
        return Err(Error::Query("empty query".into()));
    }
    Ok(out)
}

/// Parses a single statement; errors when there is more than one.
pub fn parse_one(src: &str) -> Result<Statement> {
    let mut s = parse(src)?;
    if s.len() != 1 {
        return Err(Error::Query(format!(
            "expected a single statement, found {}",
            s.len()
        )));
    }
    Ok(s.remove(0))
}

/// A select list on its own -- `status, sum(total), count(*)` -- as `get
/// ... select` reads one: the fields, or the aggregates when it has any.
/// For a caller that assembles a select from parts, such as a query string.
pub fn parse_select_list(src: &str) -> Result<(Option<Vec<String>>, Vec<Agg>)> {
    let mut p = Parser {
        toks: tokenize(src)?,
        i: 0,
        depth: 0,
    };
    let list = p.select_list()?;
    if !p.at_eof() {
        return p.err("a select list ends where the text does");
    }
    Ok(list)
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.i].tok
    }
    fn at_eof(&self) -> bool {
        matches!(self.peek(), Tok::Eof)
    }
    fn next(&mut self) -> Tok {
        let t = self.toks[self.i].tok.clone();
        if !matches!(t, Tok::Eof) {
            self.i += 1;
        }
        t
    }
    fn pos(&self) -> usize {
        self.toks[self.i].pos
    }

    fn err<T>(&self, msg: impl std::fmt::Display) -> Result<T> {
        Err(Error::Query(format!("position {}: {msg}", self.pos())))
    }

    fn expect(&mut self, t: Tok) -> Result<()> {
        if self.peek() == &t {
            self.next();
            Ok(())
        } else {
            self.err(format!(
                "expected {}, found {}",
                t.describe(),
                self.peek().describe()
            ))
        }
    }

    /// Consumes a keyword (case insensitive).
    fn eat_kw(&mut self, kw: &str) -> bool {
        if let Tok::Ident(s) = self.peek() {
            if s.eq_ignore_ascii_case(kw) {
                self.next();
                return true;
            }
        }
        false
    }

    fn peek_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Ident(s) if s.eq_ignore_ascii_case(kw))
    }

    fn expect_kw(&mut self, kw: &str) -> Result<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            self.err(format!("expected `{kw}`, found {}", self.peek().describe()))
        }
    }

    fn ident(&mut self) -> Result<String> {
        match self.next() {
            Tok::Ident(s) => Ok(s),
            other => self.err(format!("expected a name, found {}", other.describe())),
        }
    }

    fn int(&mut self) -> Result<i64> {
        match self.next() {
            Tok::Int(i) => Ok(i),
            other => self.err(format!("expected an integer, found {}", other.describe())),
        }
    }

    /// A number that may be written with a decimal point. Only the BM25 knobs
    /// need it -- `k1 = 0.9` reads the way the literature writes it, and the
    /// schema stores it in hundredths.
    fn number(&mut self) -> Result<f64> {
        match self.next() {
            Tok::Int(i) => Ok(i as f64),
            Tok::Float(f) => Ok(f),
            other => self.err(format!("expected a number, found {}", other.describe())),
        }
    }

    // ---------------------------------------------------------- statements

    fn statement(&mut self) -> Result<Statement> {
        let kw = match self.peek() {
            Tok::Ident(s) => s.to_ascii_lowercase(),
            other => return self.err(format!("expected a command, found {}", other.describe())),
        };
        match kw.as_str() {
            "create" => self.create(),
            "drop" => self.drop(),
            "put" | "insert" => self.put(),
            "get" | "select" => self.get(),
            "explain" => {
                self.next();
                let Statement::Select(sel) = self.statement()? else {
                    return self.err("`explain` takes a `get` or `select`");
                };
                Ok(Statement::Explain(sel))
            }
            "set" | "update" => self.set(),
            "del" | "delete" => self.del(),
            "collections" => {
                self.next();
                Ok(Statement::ListCollections)
            }
            "describe" => {
                self.next();
                Ok(Statement::Describe(self.ident()?))
            }
            "compact" => {
                self.next();
                let which = match self.peek() {
                    Tok::Ident(_) => Some(self.ident()?),
                    _ => None,
                };
                Ok(Statement::Compact(which))
            }
            other => self.err(format!("unknown command `{other}`")),
        }
    }

    fn create(&mut self) -> Result<Statement> {
        self.expect_kw("create")?;
        if self.peek_kw("index") {
            return self.create_index();
        }
        self.expect_kw("collection")?;
        let mut if_not_exists = false;
        if self.eat_kw("if") {
            self.expect_kw("not")?;
            self.expect_kw("exists")?;
            if_not_exists = true;
        }
        let name = self.ident()?;
        self.expect(Tok::LParen)?;
        let mut fields = Vec::new();
        loop {
            if self.peek() == &Tok::RParen {
                break;
            }
            fields.push(self.field_def()?);
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        self.expect(Tok::RParen)?;
        if fields.is_empty() {
            return self.err("a collection must contain at least one field");
        }
        Ok(Statement::CreateCollection {
            schema: Schema::new(name, fields)?,
            if_not_exists,
        })
    }

    /// `create index [if not exists] on <collection> (<field>) @hnsw(...)`
    fn create_index(&mut self) -> Result<Statement> {
        self.expect_kw("index")?;
        let mut if_not_exists = false;
        if self.eat_kw("if") {
            self.expect_kw("not")?;
            self.expect_kw("exists")?;
            if_not_exists = true;
        }
        self.expect_kw("on")?;
        let collection = self.ident()?;
        self.expect(Tok::LParen)?;
        let field = self.ident()?;
        self.expect(Tok::RParen)?;
        self.expect(Tok::At)?;
        let kind = match self.ident()?.to_ascii_lowercase().as_str() {
            "hash" => IndexKind::Hash,
            "sorted" => IndexKind::Sorted,
            "hnsw" | "vector" => IndexKind::Vector(self.hnsw_args()?),
            "text" | "bm25" => IndexKind::Text(self.text_args()?),
            other => return self.err(format!("unknown index `{other}`")),
        };
        Ok(Statement::CreateIndex {
            collection,
            field,
            kind,
            if_not_exists,
        })
    }

    fn field_def(&mut self) -> Result<Field> {
        let name = self.ident()?;
        let ty = self.data_type()?;
        let mut field = Field::new(name, ty.clone());
        loop {
            if self.eat_kw("required") {
                field = field.required();
                continue;
            }
            if matches!(self.peek(), Tok::At) {
                self.next();
                let kind = self.ident()?.to_ascii_lowercase();
                match kind.as_str() {
                    "hash" => field = field.indexed(IndexKind::Hash),
                    "sorted" => field = field.indexed(IndexKind::Sorted),
                    "hnsw" | "vector" => {
                        let spec = self.hnsw_args()?;
                        field = field.indexed(IndexKind::Vector(spec));
                    }
                    "text" | "bm25" => {
                        let spec = self.text_args()?;
                        field = field.indexed(IndexKind::Text(spec));
                    }
                    other => return self.err(format!("unknown index `{other}`")),
                }
                continue;
            }
            break;
        }
        Ok(field)
    }

    fn text_args(&mut self) -> Result<TextIndexSpec> {
        let mut spec = TextIndexSpec::default();
        if !matches!(self.peek(), Tok::LParen) {
            return Ok(spec);
        }
        self.next();
        loop {
            if matches!(self.peek(), Tok::RParen) {
                break;
            }
            let key = self.ident()?;
            self.expect(Tok::Eq)?;
            let v = self.number()?;
            let lowered = key.to_ascii_lowercase();
            match lowered.as_str() {
                "k1" | "b" => {
                    if !(0.0..=100.0).contains(&v) {
                        return self.err(format!("`{key}` must be between 0 and 100, got {v}"));
                    }
                    let pct = (v * 100.0).round() as u16;
                    if lowered == "k1" {
                        spec.k1_pct = pct;
                    } else {
                        spec.b_pct = pct;
                    }
                }
                // `prefix` is the longest prefix indexed; 0 is off.
                "prefix" | "prefix_max" | "prefix_min" => {
                    if !(0.0..=64.0).contains(&v) || v.fract() != 0.0 {
                        return self.err(format!("`{key}` must be a whole number 0..64, got {v}"));
                    }
                    if lowered == "prefix_min" {
                        spec.prefix_min = v as u8;
                    } else {
                        spec.prefix_max = v as u8;
                    }
                }
                other => return self.err(format!("unknown text parameter `{other}`")),
            }
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        self.expect(Tok::RParen)?;
        Ok(spec)
    }

    fn hnsw_args(&mut self) -> Result<VectorIndexSpec> {
        let mut spec = VectorIndexSpec::default();
        if !matches!(self.peek(), Tok::LParen) {
            return Ok(spec);
        }
        self.next();
        loop {
            if matches!(self.peek(), Tok::RParen) {
                break;
            }
            let key = self.ident()?;
            if matches!(self.peek(), Tok::Eq) {
                self.next();
                let v = self.int()? as usize;
                match key.to_ascii_lowercase().as_str() {
                    "m" => spec.m = v.max(2),
                    "ef_construction" | "ef_c" => spec.ef_construction = v.max(8),
                    "ef_search" | "ef" => spec.ef_search = v.max(1),
                    other => return self.err(format!("unknown hnsw parameter `{other}`")),
                }
            } else {
                // positional: the metric name
                match Metric::parse(&key) {
                    Some(m) => spec.metric = m,
                    None => return self.err(format!("unknown metric `{key}`")),
                }
            }
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        self.expect(Tok::RParen)?;
        Ok(spec)
    }

    fn data_type(&mut self) -> Result<DataType> {
        // [type] -> list
        if matches!(self.peek(), Tok::LBracket) {
            self.next();
            let inner = self.data_type()?;
            self.expect(Tok::RBracket)?;
            return Ok(DataType::List(Box::new(inner)));
        }
        let name = self.ident()?.to_ascii_lowercase();
        Ok(match name.as_str() {
            "bool" => DataType::Bool,
            "int" => DataType::Int,
            "float" => DataType::Float,
            "text" | "string" => DataType::Text,
            "bytes" | "blob" => DataType::Bytes,
            "timestamp" | "timestamptz" => DataType::Timestamp,
            "vector" | "vec" => {
                self.expect(Tok::Lt)?;
                let dim = self.int()?;
                if dim <= 0 {
                    return self.err("the vector dimension must be positive");
                }
                // Optional storage precision: `vector<768, f16>`.
                let prec = if matches!(self.peek(), Tok::Comma) {
                    self.next();
                    match self.ident()?.to_ascii_lowercase().as_str() {
                        "f32" => VecPrec::F32,
                        "f16" => VecPrec::F16,
                        other => {
                            return self.err(format!(
                                "vector precision must be `f32` or `f16`, found `{other}`"
                            ))
                        }
                    }
                } else {
                    VecPrec::F32
                };
                self.expect(Tok::Gt)?;
                DataType::Vector(dim as usize, prec)
            }
            other => return self.err(format!("unknown type `{other}`")),
        })
    }

    fn drop(&mut self) -> Result<Statement> {
        self.expect_kw("drop")?;
        self.expect_kw("collection")?;
        let mut if_exists = false;
        if self.eat_kw("if") {
            self.expect_kw("exists")?;
            if_exists = true;
        }
        Ok(Statement::DropCollection {
            name: self.ident()?,
            if_exists,
        })
    }

    fn put(&mut self) -> Result<Statement> {
        self.next(); // put / insert
        self.eat_kw("into");
        let collection = self.ident()?;
        let mut docs = Vec::new();
        if matches!(self.peek(), Tok::LBracket) {
            self.next();
            loop {
                if matches!(self.peek(), Tok::RBracket) {
                    break;
                }
                docs.push(self.object()?);
                if !matches!(self.peek(), Tok::Comma) {
                    break;
                }
                self.next();
            }
            self.expect(Tok::RBracket)?;
        } else {
            docs.push(self.object()?);
            // consecutive objects are accepted too: put docs {..} {..}
            while matches!(self.peek(), Tok::LBrace) {
                docs.push(self.object()?);
            }
        }
        if docs.is_empty() {
            return self.err("put expects at least one document");
        }
        Ok(Statement::Put { collection, docs })
    }

    fn object(&mut self) -> Result<Vec<(String, Expr)>> {
        self.expect(Tok::LBrace)?;
        let mut pairs = Vec::new();
        loop {
            if matches!(self.peek(), Tok::RBrace) {
                break;
            }
            let key = match self.next() {
                Tok::Ident(s) => s,
                Tok::Str(s) => s,
                other => {
                    return self.err(format!("expected a field name, found {}", other.describe()))
                }
            };
            self.expect(Tok::Colon)?;
            pairs.push((key, self.expr()?));
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        self.expect(Tok::RBrace)?;
        Ok(pairs)
    }

    fn get(&mut self) -> Result<Statement> {
        self.next(); // get / select
                     // The classic SQL order: `select title, year from articles`. fenecdb's
                     // own order is valid in the same position (`get articles select
                     // title`), so the decision is made by backtracking: if the name list
                     // is followed by `from` it is a projection, otherwise the first name
                     // is the collection.
        let mut project = None;
        let mut aggregate = Vec::new();
        if !self.peek_kw("from") {
            let save = self.i;
            match self.projection_before_from() {
                Some((cols, aggs)) => (project, aggregate) = (cols, aggs),
                None => self.i = save,
            }
        }
        self.eat_kw("from");
        let collection = self.ident()?;
        let mut sel = Select {
            collection,
            project,
            aggregate,
            ..Default::default()
        };

        loop {
            if self.eat_kw("select") {
                (sel.project, sel.aggregate) = self.select_list()?;
                continue;
            }
            if self.eat_kw("group") {
                self.eat_kw("by");
                sel.group = Some(self.ident()?);
                continue;
            }
            if self.eat_kw("where") {
                sel.filter = Some(self.expr()?);
                continue;
            }
            if self.eat_kw("near") {
                let field = self.ident()?;
                let vector = self.expr()?;
                let mut ef = None;
                let mut exact = false;
                loop {
                    if self.eat_kw("ef") {
                        ef = Some(self.int()?.max(1) as usize);
                        continue;
                    }
                    if self.eat_kw("exact") {
                        exact = true;
                        continue;
                    }
                    break;
                }
                sel.near = Some(Near {
                    field,
                    vector,
                    ef,
                    exact,
                });
                continue;
            }
            if self.eat_kw("match") {
                let field = self.ident()?;
                let query = self.expr()?;
                sel.matcher = Some(Match { field, query });
                continue;
            }
            if self.eat_kw("fuse") {
                let mut f = Fuse {
                    k: None,
                    candidates: None,
                };
                loop {
                    if self.eat_kw("k") {
                        f.k = Some(self.int()?.clamp(0, u32::MAX as i64) as u32);
                        continue;
                    }
                    if self.eat_kw("candidates") {
                        f.candidates = Some(self.int()?.max(0) as usize);
                        continue;
                    }
                    break;
                }
                sel.fuse = Some(f);
                continue;
            }
            if self.eat_kw("rerank") {
                let field = self.ident()?;
                let vector = self.expr()?;
                let mut candidates = None;
                if self.eat_kw("candidates") {
                    candidates = Some(self.int()?.max(1) as usize);
                }
                sel.rerank = Some(Rerank {
                    field,
                    vector,
                    candidates,
                });
                continue;
            }
            if self.eat_kw("order") {
                self.order_list(&mut sel.order)?;
                continue;
            }
            if self.eat_kw("count") {
                sel.count = true;
                continue;
            }
            if self.eat_kw("limit") {
                sel.limit = Some(self.int()?.max(0) as usize);
                continue;
            }
            if self.eat_kw("offset") {
                sel.offset = self.int()?.max(0) as usize;
                continue;
            }
            // `lookup` is terminal: every clause after it binds to the child.
            // Scoping by position is what keeps qualified names out of the
            // language -- `where` means on either side exactly what it always
            // meant, and neither side needs a prefix to say which it is. The
            // cost is that a parent clause cannot follow the child ones, which
            // reads the way the query runs anyway.
            if self.eat_kw("lookup") {
                sel.lookup = Some(self.lookup_clause(1)?);
                break;
            }
            break;
        }
        // The semantic check also runs here so the error carries a position.
        // The engine repeats the same check, because plan structures can be
        // built independently of FenecQL.
        if let Err(e) = sel.check() {
            return self.err(e);
        }
        Ok(Statement::Select(sel))
    }

    /// Tries the "name list + `from`" pattern after `select`.
    ///
    /// An outer `Some` means "that was a projection and `from` was consumed";
    /// on `None` the caller rewinds the position. An inner `None` means
    /// `select *`: all fields.
    /// `select a, b` or `select *`. `None` means every field.
    fn projection_list(&mut self) -> Result<Option<Vec<String>>> {
        if matches!(self.peek(), Tok::Star) {
            self.next();
            return Ok(None);
        }
        let mut cols = Vec::new();
        loop {
            cols.push(self.ident()?);
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        Ok(Some(cols))
    }

    /// `order year desc, title asc` -- keys in priority order. Over groups a
    /// key may be an aggregate, `order sum(total) desc`, named by its column.
    fn order_list(&mut self, out: &mut Vec<Sort>) -> Result<()> {
        self.eat_kw("by");
        loop {
            let mut field = self.ident()?;
            if matches!(self.peek(), Tok::LParen) {
                self.next();
                if matches!(self.peek(), Tok::Star) {
                    self.next();
                    field = COUNT_COLUMN.to_string();
                } else {
                    field = format!("{}({})", field.to_ascii_lowercase(), self.ident()?);
                }
                self.expect(Tok::RParen)?;
            }
            // `collate` comes before the direction, as in SQL; written
            // after it, it reads just as well and is taken there too.
            let mut collate = self.collate()?;
            let asc = if self.eat_kw("desc") {
                false
            } else {
                self.eat_kw("asc");
                true
            };
            if collate.is_none() {
                collate = self.collate()?;
            }
            out.push(Sort {
                field,
                asc,
                collate,
            });
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        Ok(())
    }

    /// `collate <name>`, if it comes next.
    fn collate(&mut self) -> Result<Option<Collation>> {
        if !self.eat_kw("collate") {
            return Ok(None);
        }
        let name = self.ident()?;
        match Collation::named(&name) {
            Some(c) => Ok(Some(c)),
            None => self.err(format!(
                "unknown collation `{name}`: the one there is is `tr`"
            )),
        }
    }

    /// `lookup <name> on <child> [= <parent>]` and the clauses that follow,
    /// all of which belong to the child collection.
    ///
    /// Written without the second half of `on`, the parent key is `id`: that
    /// is the foreign-key-to-primary-key shape, which is most of them, and
    /// spelling it out every time would be noise.
    ///
    /// A second `lookup` inside the first chains it -- `products lookup
    /// reviews on product_id lookup authors on id = author_id`. Position
    /// keeps scoping the same way, one level deeper: what follows a `lookup`
    /// belongs to *its* collection, and `on child = parent` names a field of
    /// the level immediately above. Exactly one collection is in scope at
    /// any point in the query, and it is the last one named, which is what
    /// keeps qualified names out of the language at any depth.
    ///
    /// `depth` is counted here as well as in `Select::check` because this
    /// function recurses: the check would run after a pathological query had
    /// already taken the stack down with it.
    fn lookup_clause(&mut self, depth: usize) -> Result<Lookup> {
        if depth > MAX_LOOKUP_DEPTH {
            return self.err(Error::Query(format!(
                "`lookup` chained too deep: at most {MAX_LOOKUP_DEPTH} levels"
            )));
        }
        let collection = self.ident()?;
        self.expect_kw("on")?;
        let child_field = self.ident()?;
        let parent_field = if matches!(self.peek(), Tok::Eq) {
            self.next();
            self.ident()?
        } else {
            "id".to_string()
        };
        let mut l = Lookup {
            collection,
            child_field,
            parent_field,
            ..Default::default()
        };
        loop {
            if self.eat_kw("select") {
                l.project = self.projection_list()?;
                continue;
            }
            if self.eat_kw("where") {
                l.filter = Some(self.expr()?);
                continue;
            }
            if self.eat_kw("order") {
                self.order_list(&mut l.order)?;
                continue;
            }
            if self.eat_kw("limit") {
                l.limit = Some(self.int()?.max(0) as usize);
                continue;
            }
            if self.eat_kw("offset") {
                l.offset = self.int()?.max(0) as usize;
                continue;
            }
            // A bare flag among the keyword clauses, the way `exact` sits
            // inside `near`.
            if self.eat_kw("required") {
                l.required = true;
                continue;
            }
            // Terminal here for the same reason it is terminal up there: the
            // clauses after it bind to the next collection down, so nothing
            // belonging to this one can follow.
            if self.eat_kw("lookup") {
                l.next = Some(Box::new(self.lookup_clause(depth + 1)?));
                break;
            }
            break;
        }
        Ok(l)
    }

    fn projection_before_from(&mut self) -> Option<(Option<Vec<String>>, Vec<Agg>)> {
        let list = self.select_list().ok()?;
        self.eat_kw("from").then_some(list)
    }

    /// A select list: `*`, fields, or -- once any item is an aggregate
    /// call -- an aggregating list, whose plain fields are the group's key.
    /// `count` needs its parentheses there: bare, it is a field of that
    /// name.
    fn select_list(&mut self) -> Result<(Option<Vec<String>>, Vec<Agg>)> {
        if matches!(self.peek(), Tok::Star) {
            self.next();
            return Ok((None, Vec::new()));
        }
        let mut items = Vec::new();
        let mut aggregates = false;
        loop {
            let name = self.ident()?;
            if matches!(self.peek(), Tok::LParen) {
                self.next();
                let arg = if matches!(self.peek(), Tok::Star | Tok::RParen) {
                    if matches!(self.peek(), Tok::Star) {
                        self.next();
                    }
                    None
                } else {
                    Some(self.ident()?)
                };
                self.expect(Tok::RParen)?;
                items.push(match (name.to_ascii_lowercase().as_str(), arg) {
                    ("count", None) => Agg::Count,
                    ("sum", Some(f)) => Agg::Sum(f),
                    ("avg", Some(f)) => Agg::Avg(f),
                    ("min", Some(f)) => Agg::Min(f),
                    ("max", Some(f)) => Agg::Max(f),
                    ("count", Some(_)) => {
                        return self.err("`count` counts rows: `count(*)`, not a field")
                    }
                    (f @ ("sum" | "avg" | "min" | "max"), None) => {
                        return self.err(format!("`{f}` needs a field: `{f}(<field>)`"))
                    }
                    (other, _) => {
                        return self.err(format!(
                            "`{other}` is not an aggregate: count, sum, avg, min or max"
                        ))
                    }
                });
                aggregates = true;
            } else {
                items.push(Agg::Key(name));
            }
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        if aggregates {
            return Ok((None, items));
        }
        let fields = items.into_iter().map(|a| a.label()).collect();
        Ok((Some(fields), Vec::new()))
    }

    fn set(&mut self) -> Result<Statement> {
        self.next(); // set / update
        let collection = self.ident()?;
        self.eat_kw("set");
        let set = self.object()?;
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Statement::Update {
            collection,
            set,
            filter,
        })
    }

    fn del(&mut self) -> Result<Statement> {
        self.next(); // del / delete
        self.eat_kw("from");
        let collection = self.ident()?;
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Statement::Delete { collection, filter })
    }

    // -------------------------------------------------------- expression grammar
    // expr    := or
    // or      := and ( "or" and )*
    // and     := not ( "and" not )*
    // not     := "not" not | cmp
    // cmp     := primary ( op primary )?
    // primary := literal | ident | call | "(" expr ")" | list

    /// Errors when the depth counter exceeds `MAX_EXPR_DEPTH`.
    fn deepen(&mut self) -> Result<()> {
        if self.depth >= MAX_EXPR_DEPTH {
            return Err(Error::Query(format!(
                "expression too deep: at most {MAX_EXPR_DEPTH} levels"
            )));
        }
        self.depth += 1;
        Ok(())
    }

    fn expr(&mut self) -> Result<Expr> {
        self.deepen()?;
        let out = self.or_expr();
        self.depth -= 1;
        out
    }

    // `a or b or c` leans left: `Or(Or(a, b), c)`. Because parsing is a loop
    // the stack does not grow here, but the *tree* gets one level deeper on
    // every turn, and evaluation recurses to that depth. Chain length is
    // therefore counted as depth; the counter is given back once the
    // expression ends, since sibling branches do not overlap.
    fn or_expr(&mut self) -> Result<Expr> {
        let mut left = self.and_expr()?;
        let mut spine = 0;
        while self.eat_kw("or") {
            if let Err(e) = self.deepen() {
                self.depth -= spine;
                return Err(e);
            }
            spine += 1;
            let right = match self.and_expr() {
                Ok(r) => r,
                Err(e) => {
                    self.depth -= spine;
                    return Err(e);
                }
            };
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        self.depth -= spine;
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Expr> {
        let mut left = self.not_expr()?;
        let mut spine = 0;
        while self.eat_kw("and") {
            if let Err(e) = self.deepen() {
                self.depth -= spine;
                return Err(e);
            }
            spine += 1;
            let right = match self.not_expr() {
                Ok(r) => r,
                Err(e) => {
                    self.depth -= spine;
                    return Err(e);
                }
            };
            left = Expr::And(Box::new(left), Box::new(right));
        }
        self.depth -= spine;
        Ok(left)
    }

    fn not_expr(&mut self) -> Result<Expr> {
        if self.eat_kw("not") {
            self.deepen()?;
            let out = self.not_expr().map(|e| Expr::Not(Box::new(e)));
            self.depth -= 1;
            return out;
        }
        self.cmp_expr()
    }

    fn cmp_expr(&mut self) -> Result<Expr> {
        let left = self.primary()?;

        let op = match self.peek() {
            Tok::Eq => Some(CmpOp::Eq),
            Tok::Ne => Some(CmpOp::Ne),
            Tok::Lt => Some(CmpOp::Lt),
            Tok::Le => Some(CmpOp::Le),
            Tok::Gt => Some(CmpOp::Gt),
            Tok::Ge => Some(CmpOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.next();
            let right = self.primary()?;
            return Ok(Expr::Cmp(op, Box::new(left), Box::new(right)));
        }
        if matches!(self.peek(), Tok::Tilde) {
            self.next();
            let right = self.primary()?;
            return Ok(Expr::Like(Box::new(left), Box::new(right)));
        }
        if self.peek_kw("has") {
            self.next();
            let right = self.primary()?;
            return Ok(Expr::Has(Box::new(left), Box::new(right)));
        }
        if self.peek_kw("in") {
            self.next();
            self.expect(Tok::LBracket)?;
            let mut items = Vec::new();
            loop {
                if matches!(self.peek(), Tok::RBracket) {
                    break;
                }
                items.push(self.expr()?);
                if !matches!(self.peek(), Tok::Comma) {
                    break;
                }
                self.next();
            }
            self.expect(Tok::RBracket)?;
            return Ok(Expr::In(Box::new(left), items));
        }
        if self.peek_kw("is") {
            self.next();
            let negated = self.eat_kw("not");
            self.expect_kw("null")?;
            let e = Expr::IsNull(Box::new(left));
            return Ok(if negated { Expr::Not(Box::new(e)) } else { e });
        }
        Ok(left)
    }

    fn primary(&mut self) -> Result<Expr> {
        match self.peek().clone() {
            Tok::LParen => {
                self.next();
                let e = self.expr()?;
                self.expect(Tok::RParen)?;
                Ok(e)
            }
            Tok::LBracket => {
                self.next();
                let mut items = Vec::new();
                loop {
                    if matches!(self.peek(), Tok::RBracket) {
                        break;
                    }
                    items.push(self.expr()?);
                    if !matches!(self.peek(), Tok::Comma) {
                        break;
                    }
                    self.next();
                }
                self.expect(Tok::RBracket)?;
                // If every element is a constant number, produce a vector
                // directly, so embedding lists become Value::Vector in one go.
                if !items.is_empty()
                    && items
                        .iter()
                        .all(|e| matches!(e, Expr::Lit(Value::Int(_)) | Expr::Lit(Value::Float(_))))
                {
                    let v: Vec<f32> = items
                        .iter()
                        .map(|e| match e {
                            Expr::Lit(Value::Int(i)) => *i as f32,
                            Expr::Lit(Value::Float(f)) => *f as f32,
                            _ => unreachable!(),
                        })
                        .collect();
                    return Ok(Expr::Lit(Value::Vector(v)));
                }
                let lits: Option<Vec<Value>> = items
                    .iter()
                    .map(|e| match e {
                        Expr::Lit(v) => Some(v.clone()),
                        _ => None,
                    })
                    .collect();
                match lits {
                    Some(vals) => Ok(Expr::Lit(Value::List(vals))),
                    None => self.err("only constant values can be used inside a list"),
                }
            }
            Tok::Str(s) => {
                self.next();
                Ok(Expr::Lit(Value::Text(s)))
            }
            Tok::Int(i) => {
                self.next();
                Ok(Expr::Lit(Value::Int(i)))
            }
            Tok::Float(f) => {
                self.next();
                Ok(Expr::Lit(Value::Float(f)))
            }
            Tok::Param(i) => {
                self.next();
                Ok(Expr::Param(i))
            }
            Tok::Ident(name) => {
                let lower = name.to_ascii_lowercase();
                match lower.as_str() {
                    "true" => {
                        self.next();
                        return Ok(Expr::Lit(Value::Bool(true)));
                    }
                    "false" => {
                        self.next();
                        return Ok(Expr::Lit(Value::Bool(false)));
                    }
                    "null" => {
                        self.next();
                        return Ok(Expr::Lit(Value::Null));
                    }
                    _ => {}
                }
                self.next();
                if matches!(self.peek(), Tok::LParen) {
                    self.next();
                    let mut args = Vec::new();
                    loop {
                        if matches!(self.peek(), Tok::RParen) {
                            break;
                        }
                        args.push(self.expr()?);
                        if !matches!(self.peek(), Tok::Comma) {
                            break;
                        }
                        self.next();
                    }
                    self.expect(Tok::RParen)?;
                    return Ok(Expr::Call(lower, args));
                }
                Ok(Expr::Field(name))
            }
            other => self.err(format!("expected a value, found {}", other.describe())),
        }
    }
}
