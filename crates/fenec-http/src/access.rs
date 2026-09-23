//! Scoped access over HTTP: who a request is, and what it may read and
//! write.
//!
//! A server's `--http-token` is all or nothing. With `--jwt-secret` it also
//! takes HS256 JSON Web Tokens, and a policy file says what each may do,
//! collection by collection, down to the rows -- the way PostgreSQL's row
//! level security lets a browser talk to the database directly:
//!
//! ```text
//! # collection   access       rows                                 role
//! notes          read,write   where owner = $jwt.sub
//! posts          read         where published = true or author = $jwt.sub
//! posts          write        where author = $jwt.sub
//! *              read                                              for dashboard
//! ```
//!
//! A rule without `for` applies to every token; one with it, to tokens whose
//! `role` claim names that role. Rules on one collection widen each other,
//! as permissive policies do. A collection no rule lets a token read does
//! not exist as far as that token can tell.
//!
//! **Reads** get the rule's filter ANDed into every level of the query:
//! the collection's `where`, each `lookup` below it, a subscription's shape.
//! **Writes** get it twice, as PostgreSQL's `USING` and `WITH CHECK`: a
//! `set` or `del` touches only rows the filter matches, and every document
//! written -- put, or rewritten by a `set` -- has to match it afterwards,
//! which the [`Check`] hook tests on the write path itself. A put that
//! leaves out a field the filter pins to a claim (`owner = $jwt.sub`) gets
//! the claim's value. A scoped put names no `id`: it would overwrite a
//! document the token may not see.
//!
//! A scoped subscription is also told only of the rows it was sent: the
//! shape's usual "a changed id that does not match is a deletion" would
//! tell every user the ids everyone else writes (see `sse`).

use crate::crypto::{b64url_decode, b64url_encode, ct_eq, hmac_sha256};
use fenec_core::prelude::*;
use fenec_core::query::{eval, truthy, CmpOp, EvalCtx, RowAccess};
use std::cell::RefCell;
use std::sync::Arc;

/// Who a request is.
#[derive(Clone)]
pub enum Who {
    /// The server's own token, or anyone on a server that asks for none.
    Full,
    Scoped(Arc<Scope>),
}

impl Who {
    pub fn scope(&self) -> Option<&Scope> {
        match self {
            Who::Full => None,
            Who::Scoped(s) => Some(s),
        }
    }
}

/// The key tokens are checked against, and the policy.
pub struct Access {
    secret: Vec<u8>,
    rules: Vec<Rule>,
}

struct Rule {
    /// `None` for `*`.
    collection: Option<String>,
    read: bool,
    write: bool,
    /// With `$jwt.<claim>` as parameter `k`, the claim `claims[k]`.
    filter: Option<Expr>,
    claims: Vec<String>,
    role: Option<String>,
}

/// A token's rules, its claims bound in.
pub struct Scope {
    subject: Option<String>,
    rules: Vec<Bound>,
}

struct Bound {
    collection: Option<String>,
    read: bool,
    write: bool,
    filter: Option<Expr>,
}

fn denied(msg: String) -> Error {
    Error::Denied(msg)
}

impl Access {
    /// `policy` is the text of a policy file (module header).
    pub fn new(secret: &[u8], policy: &str) -> std::result::Result<Access, String> {
        if secret.len() < 32 {
            return Err(
                "a JWT secret shorter than 32 bytes can be guessed: HS256 needs 256 bits of it"
                    .into(),
            );
        }
        let mut rules = Vec::new();
        for (n, line) in policy.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            rules.push(rule(line).map_err(|e| format!("policy line {}: {e}", n + 1))?);
        }
        Ok(Access {
            secret: secret.to_vec(),
            rules,
        })
    }

    /// A token for `claims`, a JSON object -- what `fenec-pg --mint-token`
    /// prints.
    pub fn mint(&self, claims: &str) -> std::result::Result<String, String> {
        fenec_core::json::parse_object(claims).map_err(|e| e.to_string())?;
        let head = b64url_encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let body = b64url_encode(claims.as_bytes());
        let sig = hmac_sha256(&self.secret, format!("{head}.{body}").as_bytes());
        Ok(format!("{head}.{body}.{}", b64url_encode(&sig)))
    }

    /// The scope of a token, checked: HS256 only, signed with this secret,
    /// not expired and already valid, at `now` seconds since the epoch.
    pub fn scope(&self, token: &str, now: u64) -> std::result::Result<Scope, &'static str> {
        let claims = self.verify(token, now)?;
        let claim = |name: &str| claims.iter().find(|(k, _)| k == name).map(|(_, v)| v);
        let role = match claim("role") {
            Some(Value::Text(r)) => Some(r.as_str()),
            _ => None,
        };
        let mut rules = Vec::new();
        'rules: for r in &self.rules {
            if r.role.is_some() && r.role.as_deref() != role {
                continue;
            }
            // A rule naming a claim the token lacks does not apply: a filter
            // with a hole in it would match rows it was never meant to.
            let mut values = Vec::with_capacity(r.claims.len());
            for c in &r.claims {
                match claim(c) {
                    Some(v) => values.push(v.clone()),
                    None => continue 'rules,
                }
            }
            rules.push(Bound {
                collection: r.collection.clone(),
                read: r.read,
                write: r.write,
                filter: r.filter.as_ref().map(|f| bind(f, &values)),
            });
        }
        Ok(Scope {
            subject: match claim("sub") {
                Some(Value::Text(s)) => Some(s.clone()),
                _ => None,
            },
            rules,
        })
    }

    fn verify(
        &self,
        token: &str,
        now: u64,
    ) -> std::result::Result<Vec<(String, Value)>, &'static str> {
        let mut parts = token.split('.');
        let (Some(head), Some(body), Some(sig), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err("not a JSON Web Token");
        };
        let object = |part: &str| {
            let bytes = b64url_decode(part).ok_or("not a JSON Web Token")?;
            let text = std::str::from_utf8(&bytes).map_err(|_| "not a JSON Web Token")?;
            fenec_core::json::parse_object(text).map_err(|_| "not a JSON Web Token")
        };
        // The algorithm is the server's, never the token's: a token saying
        // `none` -- or anything else -- is refused rather than believed.
        let header = object(head)?;
        match header.iter().find(|(k, _)| k == "alg") {
            Some((_, Value::Text(a))) if a == "HS256" => {}
            _ => return Err("only HS256 tokens are accepted"),
        }
        let expected = hmac_sha256(&self.secret, format!("{head}.{body}").as_bytes());
        let given = b64url_decode(sig).ok_or("not a JSON Web Token")?;
        if !ct_eq(&given, &expected) {
            return Err("the token's signature does not match");
        }
        let claims = object(body)?;
        let seconds = |name: &str| {
            claims
                .iter()
                .find(|(k, _)| k == name)
                .and_then(|(_, v)| match v {
                    Value::Int(i) => Some(*i as f64),
                    Value::Float(f) => Some(*f),
                    _ => None,
                })
        };
        if seconds("exp").is_some_and(|exp| now as f64 >= exp) {
            return Err("the token has expired");
        }
        if seconds("nbf").is_some_and(|nbf| (now as f64) < nbf) {
            return Err("the token is not valid yet");
        }
        Ok(claims)
    }
}

/// One line of a policy: `<collection|*> <read|write|read,write> [where
/// <filter>] [for <role>]`.
fn rule(line: &str) -> std::result::Result<Rule, String> {
    let (collection, rest) = line
        .split_once(char::is_whitespace)
        .ok_or("no access given")?;
    let rest = rest.trim_start();
    let (grants, mut rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let (mut read, mut write) = (false, false);
    for g in grants.split(',') {
        match g {
            "read" => read = true,
            "write" => write = true,
            other => return Err(format!("`{other}` is neither read nor write")),
        }
    }
    let mut role = None;
    let words: Vec<&str> = rest.split_whitespace().collect();
    if words.len() >= 2 && words[words.len() - 2] == "for" {
        role = Some(words[words.len() - 1].to_string());
        let cut = rest.rfind("for").unwrap();
        rest = &rest[..cut];
    }
    let rest = rest.trim();
    let mut claims = Vec::new();
    let filter = if rest.is_empty() {
        None
    } else {
        let text = rest
            .strip_prefix("where")
            .ok_or_else(|| format!("expected `where <filter>`, got `{rest}`"))?;
        if text.contains("$1") || text.contains("$2") {
            return Err("parameters are `$jwt.<claim>` in a policy".into());
        }
        // `$jwt.sub` becomes parameter $1, the next claim $2, ...
        let mut out = String::new();
        let mut s = text;
        while let Some(at) = s.find("$jwt.") {
            out.push_str(&s[..at]);
            let name: String = s[at + 5..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if name.is_empty() {
                return Err("`$jwt.` names no claim".into());
            }
            let k = match claims.iter().position(|c| *c == name) {
                Some(k) => k,
                None => {
                    claims.push(name.clone());
                    claims.len() - 1
                }
            };
            out.push_str(&format!("${}", k + 1));
            s = &s[at + 5 + name.len()..];
        }
        out.push_str(s);
        let stmt = fenec_ql::parse_one(&format!("get _ where {out}"))
            .map_err(|e| format!("the filter: {e}"))?;
        let Statement::Select(sel) = stmt else {
            unreachable!("a get parses as a select")
        };
        sel.filter
    };
    Ok(Rule {
        collection: (collection != "*").then(|| collection.to_string()),
        read,
        write,
        filter,
        claims,
        role,
    })
}

/// `e` with each parameter replaced by the claim it stands for.
fn bind(e: &Expr, values: &[Value]) -> Expr {
    let b = |x: &Expr| Box::new(bind(x, values));
    match e {
        Expr::Param(i) => Expr::Lit(values[*i].clone()),
        Expr::Field(_) | Expr::Lit(_) => e.clone(),
        Expr::And(x, y) => Expr::And(b(x), b(y)),
        Expr::Or(x, y) => Expr::Or(b(x), b(y)),
        Expr::Not(x) => Expr::Not(b(x)),
        Expr::IsNull(x) => Expr::IsNull(b(x)),
        Expr::Cmp(op, x, y) => Expr::Cmp(*op, b(x), b(y)),
        Expr::Like(x, y) => Expr::Like(b(x), b(y)),
        Expr::Has(x, y) => Expr::Has(b(x), b(y)),
        Expr::In(x, items) => Expr::In(b(x), items.iter().map(|i| bind(i, values)).collect()),
        Expr::Call(f, args) => {
            Expr::Call(f.clone(), args.iter().map(|a| bind(a, values)).collect())
        }
    }
}

fn and(filter: Option<Expr>, more: Option<Expr>) -> Option<Expr> {
    match (filter, more) {
        (Some(a), Some(b)) => Some(Expr::And(Box::new(a), Box::new(b))),
        (a, b) => a.or(b),
    }
}

/// The fields a filter pins to one value: the `field = value` terms of its
/// top-level `and` chain.
fn pinned(e: &Expr, out: &mut Vec<(String, Value)>) {
    match e {
        Expr::And(a, b) => {
            pinned(a, out);
            pinned(b, out);
        }
        Expr::Cmp(CmpOp::Eq, a, b) => match (a.as_ref(), b.as_ref()) {
            (Expr::Field(f), Expr::Lit(v)) | (Expr::Lit(v), Expr::Field(f)) if f != "id" => {
                out.push((f.clone(), v.clone()))
            }
            _ => {}
        },
        _ => {}
    }
}

impl Scope {
    /// The `sub` claim, when the token has one.
    pub fn subject(&self) -> Option<&str> {
        self.subject.as_deref()
    }

    /// `None`: no access. `Some(None)`: every row. `Some(Some(f))`: the rows
    /// `f` matches -- the rules' filters, any of them.
    fn filter(&self, collection: &str, write: bool) -> Option<Option<Expr>> {
        let mut any = false;
        let mut every = false;
        let mut either: Option<Expr> = None;
        for r in &self.rules {
            let here = r.collection.as_deref().is_none_or(|c| c == collection);
            if !here || !(if write { r.write } else { r.read }) {
                continue;
            }
            any = true;
            match &r.filter {
                None => every = true,
                Some(f) => {
                    either = Some(match either {
                        None => f.clone(),
                        Some(o) => Expr::Or(Box::new(o), Box::new(f.clone())),
                    })
                }
            }
        }
        match (any, every) {
            (false, _) => None,
            (true, true) => Some(None),
            (true, false) => Some(either),
        }
    }

    /// Whether the token may read `collection` at all.
    pub fn readable(&self, collection: &str) -> bool {
        self.filter(collection, false).is_some()
    }

    /// The read filter of `collection`, ANDed into `filter`; a collection
    /// the token may not read does not exist for it.
    pub fn restrict(&self, collection: &str, filter: Option<Expr>) -> Result<Option<Expr>> {
        let f = self
            .filter(collection, false)
            .ok_or_else(|| Error::NotFound(format!("collection `{collection}`")))?;
        Ok(and(filter, f))
    }

    fn select(&self, mut sel: Select) -> Result<Select> {
        sel.filter = self.restrict(&sel.collection, sel.filter.take())?;
        let mut level = sel.lookup.as_mut();
        while let Some(l) = level {
            l.filter = self.restrict(&l.collection, l.filter.take())?;
            level = l.next.as_deref_mut();
        }
        Ok(sel)
    }

    fn writable(&self, collection: &str) -> Result<Option<Expr>> {
        if !self.readable(collection) {
            return Err(Error::NotFound(format!("collection `{collection}`")));
        }
        self.filter(collection, true)
            .ok_or_else(|| denied(format!("this token may not write to `{collection}`")))
    }

    /// The statement as this token may run it, or why it may not.
    pub fn rewrite(&self, stmt: Statement) -> Result<Statement> {
        Ok(match stmt {
            Statement::Select(sel) => Statement::Select(self.select(sel)?),
            Statement::Explain(sel) => Statement::Explain(self.select(sel)?),
            Statement::Put {
                collection,
                mut docs,
            } => {
                let f = self.writable(&collection)?;
                let mut pins = Vec::new();
                if let Some(f) = &f {
                    pinned(f, &mut pins);
                }
                for doc in &mut docs {
                    if doc.iter().any(|(k, _)| k == "id") {
                        return Err(denied(
                            "a scoped token puts new documents, without `id`; it changes \
                             existing ones with `set`"
                                .into(),
                        ));
                    }
                    for (field, v) in &pins {
                        if !doc.iter().any(|(k, _)| k == field) {
                            doc.push((field.clone(), Expr::Lit(v.clone())));
                        }
                    }
                }
                Statement::Put { collection, docs }
            }
            Statement::Update {
                collection,
                set,
                filter,
            } => {
                let f = self.writable(&collection)?;
                Statement::Update {
                    collection,
                    set,
                    filter: and(filter, f),
                }
            }
            Statement::Delete { collection, filter } => {
                let f = self.writable(&collection)?;
                Statement::Delete {
                    collection,
                    filter: and(filter, f),
                }
            }
            Statement::ListCollections => Statement::ListCollections,
            Statement::Describe(name) => {
                if !self.readable(&name) {
                    return Err(Error::NotFound(format!("collection `{name}`")));
                }
                Statement::Describe(name)
            }
            _ => {
                return Err(denied(
                    "a scoped token reads and writes documents; it changes no schema".into(),
                ))
            }
        })
    }

    /// Whether `doc`, as it would be written to `collection`, is one this
    /// token may write.
    fn admits(&self, collection: &str, doc: &Document, registry: &Registry) -> Result<bool> {
        match self.filter(collection, true) {
            None => Ok(false),
            Some(None) => Ok(true),
            Some(Some(f)) => {
                let ctx = EvalCtx {
                    params: &[],
                    registry,
                };
                Ok(truthy(&eval(&f, &mut Written(doc), &ctx)?))
            }
        }
    }
}

struct Written<'a>(&'a Document);

impl RowAccess for Written<'_> {
    fn id(&self) -> DocId {
        self.0.id
    }
    fn field(&mut self, name: &str) -> Result<Value> {
        if name == "id" {
            return Ok(Value::Int(self.0.id as i64));
        }
        Ok(self.0.get(name).cloned().unwrap_or(Value::Null))
    }
}

thread_local! {
    /// The scope the write running on this thread is made under. A
    /// thread-local, not a parameter: the check runs as a write hook, deep
    /// in the engine's write path, which knows nothing of tokens.
    static CURRENT: RefCell<Option<Arc<Scope>>> = const { RefCell::new(None) };
}

/// Runs `f` -- a statement executing under the write lock -- as `who`: the
/// documents it writes are checked against the token's rules.
pub fn within<T>(who: &Who, f: impl FnOnce() -> T) -> T {
    let Who::Scoped(scope) = who else {
        return f();
    };
    let before = CURRENT.with(|c| c.replace(Some(Arc::clone(scope))));
    struct Restore(Option<Arc<Scope>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let prev = self.0.take();
            CURRENT.with(|c| *c.borrow_mut() = prev);
        }
    }
    let _restore = Restore(before);
    f()
}

/// The `WITH CHECK` half: every document a scoped write puts or rewrites has
/// to match the token's rules once written.
pub struct Check {
    /// For the functions a filter may call; plugins' are not among them.
    registry: Registry,
}

impl Hook for Check {
    fn name(&self) -> &str {
        "access"
    }
    fn before_write(&self, collection: &str, op: WriteOp, doc: &mut Document) -> Result<()> {
        let Some(scope) = CURRENT.with(|c| c.borrow().clone()) else {
            return Ok(());
        };
        if op == WriteOp::Delete || scope.admits(collection, doc, &self.registry)? {
            return Ok(());
        }
        Err(denied(format!(
            "the document is outside what this token may write to `{collection}`"
        )))
    }
}

/// Installs [`Check`]: the server does it for a database it serves with a
/// policy.
pub struct CheckPlugin;

impl Plugin for CheckPlugin {
    fn name(&self) -> &str {
        "access"
    }
    fn init(&self, reg: &mut Registry) -> Result<()> {
        reg.register_hook(Arc::new(Check {
            registry: Registry::with_builtins(),
        }));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"a secret of more than thirty-two bytes, as HS256 wants";

    fn access(policy: &str) -> Access {
        Access::new(SECRET, policy).unwrap()
    }

    /// The token from RFC 7515's era of examples that jwt.io shows, with
    /// its secret: a signature computed elsewhere, checked here.
    #[test]
    fn a_token_signed_elsewhere_verifies() {
        let a = Access {
            secret: b"your-256-bit-secret".to_vec(),
            rules: Vec::new(),
        };
        let token = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.\
                     eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.\
                     SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let claims = a.verify(token, 1_600_000_000).unwrap();
        assert!(claims.contains(&("sub".into(), Value::Text("1234567890".into()))));
    }

    #[test]
    fn a_forged_or_stale_token_is_refused() {
        let a = access("");
        let good = a.mint(r#"{"sub":"alice","exp":2000}"#).unwrap();
        assert!(a.verify(&good, 1_000).is_ok());
        assert_eq!(a.verify(&good, 2_000).unwrap_err(), "the token has expired");
        // Another secret, a changed claim, no signature, `alg: none`.
        let other = Access::new(&[7u8; 32], "").unwrap();
        let forged = other.mint(r#"{"sub":"alice"}"#).unwrap();
        assert!(a.verify(&forged, 1_000).is_err());
        let (head, rest) = good.split_once('.').unwrap();
        let sig = rest.rsplit_once('.').unwrap().1;
        let bob = format!(
            "{head}.{}.{sig}",
            b64url_encode(br#"{"sub":"bob","exp":2000}"#)
        );
        assert!(a.verify(&bob, 1_000).is_err());
        let none = format!(
            "{}.{}.",
            b64url_encode(br#"{"alg":"none"}"#),
            b64url_encode(br#"{"sub":"alice"}"#)
        );
        assert_eq!(
            a.verify(&none, 1_000).unwrap_err(),
            "only HS256 tokens are accepted"
        );
        let early = a.mint(r#"{"sub":"alice","nbf":5000}"#).unwrap();
        assert!(a.verify(&early, 1_000).is_err());
        assert!(Access::new(b"short", "").is_err());
    }

    #[test]
    fn rules_bind_claims_and_roles() {
        let a = access(
            "# a comment\n\
             notes read,write where owner = $jwt.sub\n\
             posts read where published = true or author = $jwt.sub\n\
             posts write where author = $jwt.sub and team = $jwt.team\n\
             * read for dashboard\n",
        );
        let alice = a.scope(&a.mint(r#"{"sub":"alice"}"#).unwrap(), 0).unwrap();
        assert!(alice.readable("notes") && alice.readable("posts"));
        assert!(!alice.readable("secrets"));
        // The write rule names a claim alice's token lacks: it does not apply.
        assert!(alice.writable("posts").is_err());
        let Some(Some(Expr::Cmp(CmpOp::Eq, _, v))) = alice.filter("notes", true) else {
            panic!("a bound filter");
        };
        assert_eq!(*v, Expr::Lit(Value::Text("alice".into())));

        let dash = a
            .scope(&a.mint(r#"{"sub":"d","role":"dashboard"}"#).unwrap(), 0)
            .unwrap();
        assert!(dash.readable("secrets"));
        assert!(matches!(dash.filter("secrets", false), Some(None)));
        assert!(dash.writable("secrets").is_err());

        for bad in [
            "notes",
            "notes admin",
            "notes read where",
            "notes read where $1 = 2",
        ] {
            assert!(Access::new(SECRET, bad).is_err(), "{bad}");
        }
    }
}
