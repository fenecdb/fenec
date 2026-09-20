//! FenecQL parser: token stream -> `fenec_core::query::Statement`.
//!
//! Language summary
//! ```text
//! create collection [if not exists] <name> ( <field> <type> [@index], ... )
//! drop   collection [if exists] <name>
//! put    <name> { k: v, ... }            -- or [ {...}, {...} ]
//! get    <name> [select a, b] [where <expr>] [near <field> <vector> [ef N] [exact]]
//!            [order <field> [asc|desc], ...] [limit N] [offset N] [count]
//! select a, b from <name> ...            -- the classic SQL order works too
//! set    <name> { k: v, ... } [where <expr>]
//! del    <name> [where <expr>]
//! collections | describe <name> | compact [<name>]
//! ```

use crate::lexer::{tokenize, Tok, Token};
use fenec_core::error::{Error, Result};
use fenec_core::query::*;
use fenec_core::schema::{Field, IndexKind, Metric, Schema, VectorIndexSpec};
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
            "hnsw" | "vector" => IndexKind::Vector(self.hnsw_args()?),
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
                    "hnsw" | "vector" => {
                        let spec = self.hnsw_args()?;
                        field = field.indexed(IndexKind::Vector(spec));
                    }
                    other => return self.err(format!("unknown index `{other}`")),
                }
                continue;
            }
            break;
        }
        Ok(field)
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
        if !self.peek_kw("from") {
            let save = self.i;
            match self.projection_before_from()? {
                Some(cols) => project = cols,
                None => self.i = save,
            }
        }
        self.eat_kw("from");
        let collection = self.ident()?;
        let mut sel = Select {
            collection,
            project,
            ..Default::default()
        };

        loop {
            if self.eat_kw("select") {
                if matches!(self.peek(), Tok::Star) {
                    self.next();
                    sel.project = None;
                } else {
                    let mut cols = Vec::new();
                    loop {
                        cols.push(self.ident()?);
                        if !matches!(self.peek(), Tok::Comma) {
                            break;
                        }
                        self.next();
                    }
                    sel.project = Some(cols);
                }
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
            if self.eat_kw("order") {
                self.eat_kw("by");
                loop {
                    let field = self.ident()?;
                    let asc = if self.eat_kw("desc") {
                        false
                    } else {
                        self.eat_kw("asc");
                        true
                    };
                    sel.order.push((field, asc));
                    if !matches!(self.peek(), Tok::Comma) {
                        break;
                    }
                    self.next();
                }
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
    fn projection_before_from(&mut self) -> Result<Option<Option<Vec<String>>>> {
        if matches!(self.peek(), Tok::Star) {
            self.next();
            return Ok(if self.eat_kw("from") {
                Some(None)
            } else {
                None
            });
        }
        let mut cols = Vec::new();
        loop {
            match self.peek() {
                Tok::Ident(_) => cols.push(self.ident()?),
                _ => return Ok(None),
            }
            if !matches!(self.peek(), Tok::Comma) {
                break;
            }
            self.next();
        }
        Ok(if self.eat_kw("from") {
            Some(Some(cols))
        } else {
            None
        })
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
