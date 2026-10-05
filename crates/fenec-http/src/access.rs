//! Scoped access over HTTP: who a request is, and what it may read and
//! write.
//!
//! A server's `--http-token` is all or nothing. With `--jwt-secret` it also
//! takes HS256 JSON Web Tokens -- with `--jwt-keys`, those a JWKS file's
//! keys verify, HS256 and RS256 (below) -- and a policy file says what each
//! may do,
//! collection by collection, down to the rows -- the way PostgreSQL's row
//! level security lets a browser talk to the database directly:
//!
//! ```text
//! # collection   access         rows                                 role
//! notes          read,write     where owner = $jwt.sub
//! posts          read           where published = true or author = $jwt.sub
//! posts          write          where author = $jwt.sub
//! messages       read,insert    where room in $jwt.rooms
//! journal        read,insert    where owner = $jwt.sub
//! journal        append-only
//! *              read                                                for dashboard
//! ```
//!
//! A rule without `for` applies to every token; one with it, to tokens whose
//! `role` claim names that role (a text, or a list holding it). Rules on one
//! collection widen each other, as permissive policies do. A collection no
//! rule grants a token anything does not exist as far as that token can
//! tell.
//!
//! **Grants** are `read`, `insert`, `update` and `delete`, and `write` is
//! the three writes. An `insert` is a put of new documents; `update` is a
//! `set`, and `delete` a `del`, of rows the token may also read. A token
//! may insert into a collection it cannot read -- an event stream, a trail
//! a client appends to -- which is then not listed to it. `expired` grants
//! the rows past their `@ttl` that `expired()` reads, a reaper's: a
//! statement asking it is held to those rules' filters as well, and
//! neither `write` nor `*` grants it.
//!
//! **`append-only`** is a line of its own, `<collection> append-only`,
//! with no filter and no role: no scoped token updates or deletes there,
//! whatever its rules grant -- `write` and `*` mean `insert` there, and a
//! rule naming `update` or `delete` for it is refused at startup. It binds
//! scoped tokens, not the server's own: a journal's rows are corrected,
//! erased under the law or swept by `@ttl` by whoever holds that token,
//! and an app server that should only append is given a scoped token too.
//!
//! **A list claim** goes on the right of `in`: `room in $jwt.rooms`, the
//! claim a JSON array, matches the rooms it lists -- `in [$jwt.rooms,
//! 'lobby']` those and one more -- and a text claim there is a list of one.
//! A token whose claim lists more than [`MAX_CLAIM_VALUES`] is refused.
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
//!
//! **Keys.** A JWKS file holds the keys, `{"keys": [...]}` as an identity
//! provider publishes it: `oct` keys verify HS256 and `RSA` keys RS256, so
//! a token an identity provider signs is taken with its public keys alone.
//! A key's kind decides the algorithm it verifies, never the token: an
//! RS256 key's modulus is public, and taken for an HS256 secret it would
//! sign anything. A token naming a `kid` is checked against that key alone;
//! one naming none against each key of its algorithm. The file is read
//! again when it changes, looked at once a second at most, which is the
//! rotation: add the new key, sign with it, and take the old one out once
//! the tokens it signed have expired. A file that no longer reads keeps the
//! keys read last.

use crate::crypto::{b64url_decode, b64url_encode, ct_eq, hmac_sha256, RsaKey};
use fenec_core::prelude::*;
use fenec_core::query::{eval, truthy, CmpOp, EvalCtx, RowAccess};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

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

/// The keys tokens are checked against, and the policy.
pub struct Access {
    keys: RwLock<Arc<Vec<Jwk>>>,
    /// The JWKS file the keys are read again from as it changes.
    source: Option<Source>,
    rules: Vec<Rule>,
    /// The collections no scoped token updates or deletes in (`append-only`
    /// lines), `*` for every one.
    append_only: Arc<[String]>,
    /// Tokens already verified, by their text: a client sends the same one
    /// for as long as it lives, and an RS256 verification takes 38 us (166
    /// in 32-bit limbs), an HS256 token's with its parses 2.9, a token
    /// found here 0.5.
    /// Emptied when the keys change, since a key taken out must take its
    /// tokens with it; `exp` and `nbf` are asked on every request.
    verified: [Mutex<HashMap<Box<str>, Claims>>; SHARDS],
    demands: Demands,
}

/// What a token must carry beyond a good signature.
#[derive(Clone, Debug)]
pub struct Demands {
    /// A token with no `exp` is refused (`--jwt-require-exp`, on unless
    /// turned off): one minted without it was taken for ever, and a token
    /// that leaked could not be outlived, only every key rotated.
    pub require_exp: bool,
    /// A token whose `exp` lies further ahead than this many seconds is
    /// refused (`--jwt-max-age`): an `exp` ten years out is no `exp`.
    pub max_age: Option<u64>,
    /// The claim naming the tenant a token is for (`--jwt-tenant-claim`,
    /// `tenant` unless told): on a `--dir` node a token reaches `/t/<t>/`
    /// only when the claim names `<t>` -- a text, or a list holding it.
    pub tenant_claim: String,
    /// On a `--dir` node, take a token whose claims name no tenant for
    /// every tenant (`--jwt-unbound-tenants`). Off, such a token is refused
    /// there: a policy's `owner = $jwt.sub` matches the same user in every
    /// tenant's file, so a token for one tenant read every other.
    pub unbound_tenants: bool,
}

impl Default for Demands {
    fn default() -> Demands {
        Demands {
            require_exp: true,
            max_age: None,
            tenant_claim: "tenant".into(),
            unbound_tenants: false,
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// How long a token `mint` signs lives when its claims name no `exp`.
pub const MINTED_FOR: u64 = 3600;

type Claims = Arc<Vec<(String, Value)>>;
const SHARDS: usize = 16;
/// Tokens a shard keeps, emptied when full.
const KEPT: usize = 256;

fn shard(token: &str) -> usize {
    // The signature's last characters are as random as it is.
    let b = token.as_bytes();
    (b[b.len().saturating_sub(2)..]
        .iter()
        .fold(0usize, |a, &c| a * 31 + c as usize))
        % SHARDS
}

struct Source {
    path: PathBuf,
    /// The second the file was last looked at.
    looked: AtomicU64,
    modified: Mutex<Option<SystemTime>>,
}

struct Jwk {
    kid: Option<String>,
    kind: KeyKind,
}

enum KeyKind {
    /// HS256.
    Hmac(Vec<u8>),
    /// RS256.
    Rsa(RsaKey),
}

const SHORT: &str = "a JWT secret shorter than 32 bytes can be guessed: HS256 needs 256 bits of it";

/// The keys of a JWKS file: `{"keys": [...]}`, or the list alone. A key of
/// another kind (`EC`, `OKP`) or for encryption (`"use": "enc"`) is passed
/// over, as a verifier that does not know it would; a file with no key this
/// server can use is refused.
fn read_jwks(text: &str) -> std::result::Result<Vec<Jwk>, String> {
    let t = text.trim();
    let list = if t.starts_with('[') {
        t
    } else {
        // A JWK holds no `[` or `]` inside a string: base64url, a kid, an
        // algorithm's name, a URL -- only `x5c`'s list and `key_ops`'s.
        let at = t
            .find("\"keys\"")
            .ok_or("a JWKS file is {\"keys\": [...]}")?;
        let open = at
            + t[at..]
                .find('[')
                .ok_or("a JWKS file is {\"keys\": [...]}")?;
        let mut depth = 0;
        let close = t[open..]
            .char_indices()
            .find(|&(_, c)| {
                depth += (c == '[') as i32 - (c == ']') as i32;
                depth == 0
            })
            .ok_or("the list of keys is not closed")?
            .0;
        &t[open..=open + close]
    };
    let docs = fenec_core::json::parse_documents(list).map_err(|e| format!("a JWKS file: {e}"))?;
    let mut keys = Vec::new();
    for d in docs {
        let text = |k: &str| match d.iter().find(|(n, _)| n == k) {
            Some((_, Value::Text(t))) => Some(t.as_str()),
            _ => None,
        };
        let bytes = |k: &str| -> std::result::Result<Vec<u8>, String> {
            b64url_decode(text(k).ok_or(format!("a key without `{k}`"))?)
                .ok_or(format!("`{k}` is not base64url"))
        };
        if text("use").is_some_and(|u| u != "sig") {
            continue;
        }
        let kid = text("kid").map(str::to_string);
        let alg = text("alg");
        let kind = match text("kty") {
            Some("oct") if alg.is_none_or(|a| a == "HS256") => {
                let k = bytes("k")?;
                if k.len() < 32 {
                    return Err(SHORT.into());
                }
                KeyKind::Hmac(k)
            }
            Some("RSA") if alg.is_none_or(|a| a == "RS256") => KeyKind::Rsa(
                RsaKey::new(&bytes("n")?, &bytes("e")?)
                    .ok_or("an RSA key of 2048 bits at the least and an odd exponent from 3")?,
            ),
            _ => continue,
        };
        keys.push(Jwk { kid, kind });
    }
    if keys.is_empty() {
        return Err("no HS256 or RS256 key in the JWKS file".into());
    }
    Ok(keys)
}

/// What a rule grants, as bits: `read`, `insert`, `update`, `delete`.
const READ: u8 = 1;
const INSERT: u8 = 2;
const UPDATE: u8 = 4;
const DELETE: u8 = 8;
/// `expired`: the rows past their `@ttl`, which `expired()` reads and
/// every other read leaves out -- a reaper's, never granted by `read`,
/// `write` or `*`: a hold that lapsed is not a user's to see again.
const EXPIRED: u8 = 16;
/// `write`.
const WRITES: u8 = INSERT | UPDATE | DELETE;

/// The most values a list claim may hand a rule's `in`. A token is signed
/// by whoever issues them, but the server builds a filter of the list on
/// every request and tests it against every row read and every document
/// written: a token listing a million rooms would cost every request it
/// makes. 1 000 rooms or teams is far past a person's; past it, the
/// membership belongs in the rows (a collection per room, or a tenant).
pub const MAX_CLAIM_VALUES: usize = 1_000;

struct Rule {
    /// `None` for `*`.
    collection: Option<String>,
    ops: u8,
    /// The writes named one by one, which an `append-only` line refuses.
    named: u8,
    /// With `$jwt.<claim>` as parameter `k`, the claim `claims[k]`.
    filter: Option<Expr>,
    claims: Vec<String>,
    role: Option<String>,
}

/// A token's rules, its claims bound in.
pub struct Scope {
    subject: Option<String>,
    rules: Vec<Bound>,
    /// The tenants the token names in its tenant claim; `None` where it
    /// names none.
    tenants: Option<Vec<String>>,
    /// A token naming no tenant reaches every one (`--jwt-unbound-tenants`).
    unbound: bool,
    /// The policy's `append-only` collections, `*` for every one.
    append_only: Arc<[String]>,
}

struct Bound {
    collection: Option<String>,
    ops: u8,
    filter: Option<Expr>,
}

fn denied(msg: String) -> Error {
    Error::Denied(msg)
}

impl Access {
    /// `policy` is the text of a policy file (module header).
    pub fn new(secret: &[u8], policy: &str) -> std::result::Result<Access, String> {
        if secret.len() < 32 {
            return Err(SHORT.into());
        }
        let key = Jwk {
            kid: None,
            kind: KeyKind::Hmac(secret.to_vec()),
        };
        Access::with(vec![key], None, policy)
    }

    /// The keys of the JWKS file at `path`, read again as it changes.
    pub fn from_jwks(path: &std::path::Path, policy: &str) -> std::result::Result<Access, String> {
        let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let keys = read_jwks(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let source = Source {
            path: path.to_path_buf(),
            looked: AtomicU64::new(0),
            modified: Mutex::new(modified),
        };
        Access::with(keys, Some(source), policy)
    }

    fn with(
        keys: Vec<Jwk>,
        source: Option<Source>,
        policy: &str,
    ) -> std::result::Result<Access, String> {
        let mut rules = Vec::new();
        let mut append_only = Vec::new();
        let mut lines = Vec::new();
        for (n, line) in policy.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let at = |e: String| format!("policy line {}: {e}", n + 1);
            let mut words = line.split_whitespace();
            if let (Some(c), Some("append-only")) = (words.next(), words.next()) {
                if words.next().is_some() {
                    return Err(at("`append-only` takes no filter and no role: it holds \
                                   every scoped token"
                        .into()));
                }
                append_only.push(c.to_string());
                continue;
            }
            rules.push(rule(line).map_err(at)?);
            lines.push(n + 1);
        }
        // A rule granting what an `append-only` line takes away says two
        // things, and which was meant is not for the server to guess.
        // `write` and `*` stand for more than the one collection, and mean
        // `insert` there.
        for (r, n) in rules.iter().zip(lines) {
            let Some(c) = &r.collection else { continue };
            let named = r.named & (UPDATE | DELETE) != 0;
            if named && append_only.iter().any(|a| a == "*" || a == c) {
                return Err(format!(
                    "policy line {n}: `{c}` is append-only, and the rule grants `update` or \
                     `delete` there"
                ));
            }
        }
        Ok(Access {
            keys: RwLock::new(Arc::new(keys)),
            source,
            rules,
            append_only: append_only.into(),
            verified: Default::default(),
            demands: Demands::default(),
        })
    }

    /// The same keys and policy, holding tokens to `demands`.
    pub fn demanding(mut self, demands: Demands) -> Access {
        self.demands = demands;
        self
    }

    /// The keys, the file's read again first where it changed -- looked at
    /// once in a second `now`, at most.
    fn keys(&self, now: u64) -> Arc<Vec<Jwk>> {
        if let Some(src) = &self.source {
            if src.looked.swap(now, Ordering::Relaxed) != now {
                let modified = std::fs::metadata(&src.path).and_then(|m| m.modified()).ok();
                let mut last = src.modified.lock().unwrap_or_else(|e| e.into_inner());
                if modified != *last {
                    *last = modified;
                    match std::fs::read_to_string(&src.path)
                        .map_err(|e| e.to_string())
                        .and_then(|t| read_jwks(&t))
                    {
                        Ok(keys) => {
                            *self.keys.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(keys);
                            for v in &self.verified {
                                v.lock().unwrap_or_else(|e| e.into_inner()).clear();
                            }
                        }
                        Err(e) => {
                            crate::log!("{}: {e}; the keys read before stay", src.path.display())
                        }
                    }
                }
            }
        }
        self.keys.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// A token for `claims`, a JSON object -- what `fenec-server --mint-token`
    /// prints.
    /// Signed with the first HS256 key, its `kid` named. Claims naming no
    /// `exp` get one [`MINTED_FOR`] seconds from now: a server refuses a
    /// token without one ([`Demands::require_exp`]).
    pub fn mint(&self, claims: &str) -> std::result::Result<String, String> {
        let parsed = fenec_core::json::parse_object(claims).map_err(|e| e.to_string())?;
        let stamped;
        let claims = if parsed.iter().any(|(k, _)| k == "exp") {
            claims
        } else {
            let open = claims.trim().strip_suffix('}').unwrap_or("{").trim_end();
            let comma = if open.ends_with('{') { "" } else { "," };
            stamped = format!(r#"{open}{comma}"exp":{}}}"#, unix_now() + MINTED_FOR);
            &stamped
        };
        let keys = self.keys.read().unwrap_or_else(|e| e.into_inner()).clone();
        let (kid, secret) = keys
            .iter()
            .find_map(|k| match &k.kind {
                KeyKind::Hmac(s) => Some((k.kid.as_deref(), s)),
                KeyKind::Rsa(_) => None,
            })
            .ok_or("no HS256 key to sign with: an RS256 key's private half is its owner's")?;
        let mut head = String::from(r#"{"alg":"HS256","typ":"JWT""#);
        if let Some(kid) = kid {
            head.push_str(r#","kid":"#);
            fenec_core::json::escape_into(&mut head, kid);
        }
        head.push('}');
        let head = b64url_encode(head.as_bytes());
        let body = b64url_encode(claims.as_bytes());
        let sig = hmac_sha256(secret, format!("{head}.{body}").as_bytes());
        Ok(format!("{head}.{body}.{}", b64url_encode(&sig)))
    }

    /// The scope of a token, checked: HS256 or RS256, signed with one of the
    /// keys, not expired and already valid, at `now` seconds since the epoch.
    pub fn scope(&self, token: &str, now: u64) -> std::result::Result<Scope, &'static str> {
        let claims = self.verify(token, now)?;
        let claim = |name: &str| claims.iter().find(|(k, _)| k == name).map(|(_, v)| v);
        // A role is a text, or a list of them, the token holding each.
        let has_role = |role: &str| match claim("role") {
            Some(Value::Text(r)) => r == role,
            Some(Value::List(l)) => l.iter().any(|v| v.as_text() == Some(role)),
            _ => false,
        };
        let mut rules = Vec::new();
        'rules: for r in &self.rules {
            if r.role.as_deref().is_some_and(|role| !has_role(role)) {
                continue;
            }
            // A rule naming a claim the token lacks does not apply: a filter
            // with a hole in it would match rows it was never meant to.
            let mut values = Vec::with_capacity(r.claims.len());
            for c in &r.claims {
                match claim(c) {
                    Some(Value::List(l)) if l.len() > MAX_CLAIM_VALUES => {
                        return Err("a list claim the policy reads holds more than 1 000 values")
                    }
                    Some(v) => values.push(v.clone()),
                    None => continue 'rules,
                }
            }
            rules.push(Bound {
                collection: r.collection.clone(),
                ops: r.ops,
                filter: r.filter.as_ref().map(|f| bind(f, &values)),
            });
        }
        // Anything but a text or a list of texts names no tenant: a number
        // or an object compared as text could be made to match.
        let tenants = match claim(&self.demands.tenant_claim) {
            Some(Value::Text(t)) => Some(vec![t.clone()]),
            Some(Value::List(l)) => Some(
                l.iter()
                    .filter_map(|v| match v {
                        Value::Text(t) => Some(t.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        };
        Ok(Scope {
            subject: match claim("sub") {
                Some(Value::Text(s)) => Some(s.clone()),
                _ => None,
            },
            rules,
            tenants,
            unbound: self.demands.unbound_tenants,
            append_only: Arc::clone(&self.append_only),
        })
    }

    fn verify(&self, token: &str, now: u64) -> std::result::Result<Claims, &'static str> {
        let keys = self.keys(now);
        let cached = self.verified[shard(token)]
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(token)
            .cloned();
        let claims = match cached {
            Some(c) => c,
            None => self.check(token, &keys)?,
        };
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
        match seconds("exp") {
            Some(exp) if now as f64 >= exp => return Err("the token has expired"),
            Some(exp) if self.demands.max_age.is_some_and(|m| exp > (now + m) as f64) => {
                return Err("the token's exp is further ahead than this server takes")
            }
            None if self.demands.require_exp => {
                return Err("the token has no exp: one without would be good for ever")
            }
            _ => {}
        }
        if seconds("nbf").is_some_and(|nbf| (now as f64) < nbf) {
            return Err("the token is not valid yet");
        }
        Ok(claims)
    }

    /// The claims of a token one of `keys` signed.
    fn check(&self, token: &str, keys: &[Jwk]) -> std::result::Result<Claims, &'static str> {
        let mut parts = token.split('.');
        let (Some(head), Some(body), Some(sig), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err("not a JSON Web Token");
        };
        let object = |part: &str| {
            let bytes = b64url_decode(part).ok_or("not a JSON Web Token")?;
            let text = std::str::from_utf8(&bytes).map_err(|_| "not a JSON Web Token")?;
            // Read as written: a list of numbers read the quick way is a
            // vector of `f32`s, and a team `123456789` in `team in
            // $jwt.teams` would have been 123456792 -- another team's. A
            // member given twice is refused, as a document's is.
            match fenec_core::json::parse_json(text) {
                Ok(Value::Object(members)) => Ok(members),
                _ => Err("not a JSON Web Token"),
            }
        };
        // The algorithm is the server's, never the token's: a token saying
        // `none` -- or anything else -- is refused rather than believed.
        let header = object(head)?;
        let field = |name: &str| match header.iter().find(|(k, _)| k == name) {
            Some((_, Value::Text(a))) => Some(a.as_str()),
            _ => None,
        };
        let rsa = match field("alg") {
            Some("HS256") => false,
            Some("RS256") => true,
            _ => return Err("only HS256 and RS256 tokens are accepted"),
        };
        let kid = field("kid");
        let signed = format!("{head}.{body}");
        let given = b64url_decode(sig).ok_or("not a JSON Web Token")?;
        let mut named = false;
        let good = keys
            .iter()
            .filter(|k| kid.is_none() || k.kid.as_deref() == kid)
            .inspect(|_| named = true)
            .any(|k| match (&k.kind, rsa) {
                (KeyKind::Hmac(s), false) => ct_eq(&given, &hmac_sha256(s, signed.as_bytes())),
                (KeyKind::Rsa(r), true) => r.verify_sha256(signed.as_bytes(), &given),
                _ => false,
            });
        if !good {
            return Err(if named {
                "the token's signature does not match"
            } else {
                "no key has the token's kid"
            });
        }
        let claims = Arc::new(object(body)?);
        let mut kept = self.verified[shard(token)]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if kept.len() >= KEPT {
            kept.clear();
        }
        kept.insert(token.into(), claims.clone());
        Ok(claims)
    }
}

/// One line of a policy: `<collection|*> <grants> [where <filter>] [for
/// <role>]`, the grants `read`, `insert`, `update`, `delete` and `write`
/// joined by commas.
fn rule(line: &str) -> std::result::Result<Rule, String> {
    let (collection, rest) = line
        .split_once(char::is_whitespace)
        .ok_or("no access given")?;
    let rest = rest.trim_start();
    let (grants, mut rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let (mut ops, mut named) = (0, 0);
    for g in grants.split(',') {
        match g {
            "read" => ops |= READ,
            "write" => ops |= WRITES,
            "insert" => named |= INSERT,
            "update" => named |= UPDATE,
            "delete" => named |= DELETE,
            "expired" => ops |= EXPIRED,
            other => {
                return Err(format!(
                    "`{other}` is none of read, insert, update, delete, write and expired"
                ))
            }
        }
    }
    ops |= named;
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
            // `in $jwt.rooms` is `in [$jwt.rooms]`, whose list claim
            // [`bind`] spreads: FenecQL's `in` takes a list, and a
            // parameter binds one value.
            let is_in = out.trim_end().strip_suffix("in").is_some_and(|b| {
                b.is_empty() || b.ends_with(|c: char| c.is_whitespace() || c == '(')
            });
            if is_in {
                out.push_str(&format!("[${}]", k + 1));
            } else {
                out.push_str(&format!("${}", k + 1));
            }
            s = &s[at + 5 + name.len()..];
        }
        out.push_str(s);
        let stmt = fenec_ql::parse_one(&format!("get _ where {out}"))
            .map_err(|e| format!("the filter: {e}"))?;
        let Statement::Select(sel) = stmt else {
            unreachable!("a get parses as a select")
        };
        // A rule is tested against a document being written on its own
        // (`admits`), where no query runs to answer an inner `get`, and the
        // inner `get` would itself be scoped by the rules it is part of.
        if sel.filter.as_ref().is_some_and(Expr::has_subquery) {
            return Err("a rule's filter takes no `in (get ...)`".into());
        }
        // Tested against a document on its own as well, where nothing
        // answers it; what a token may reap is the `expired` grant's.
        if sel.filter.as_ref().is_some_and(Expr::asks_expired) {
            return Err("a rule's filter takes no `expired()`: grant `expired`".into());
        }
        sel.filter
    };
    Ok(Rule {
        collection: (collection != "*").then(|| collection.to_string()),
        ops,
        named,
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
        Expr::Arith(op, x, y) => Expr::Arith(*op, b(x), b(y)),
        // A list claim in an `in` list is its values: `room in [$1]`, the
        // claim `["r1", "r2"]`, is `room in ["r1", "r2"]`.
        Expr::In(x, items) => {
            let mut out = Vec::with_capacity(items.len());
            for i in items {
                match i {
                    Expr::Param(k) => match &values[*k] {
                        Value::List(l) => out.extend(l.iter().cloned().map(Expr::Lit)),
                        v => out.push(Expr::Lit(v.clone())),
                    },
                    i => out.push(bind(i, values)),
                }
            }
            Expr::In(b(x), out)
        }
        // A rule holds none ([`rule`]).
        Expr::InSelect(..) => e.clone(),
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

    /// Whether the token may reach tenant `name` on a `--dir` node: its
    /// tenant claim names it, or it names none and the node takes unbound
    /// tokens. The tenant comes from the path and each tenant is a file of
    /// its own, but the policy is the node's: without this a token minted
    /// for one tenant read every other whose rows its filter matched.
    pub fn reaches(&self, name: &str) -> std::result::Result<(), &'static str> {
        match &self.tenants {
            Some(names) if names.iter().any(|n| n == name) => Ok(()),
            Some(_) => Err("this token is for another tenant"),
            None if self.unbound => Ok(()),
            None => Err("this token names no tenant, and this node serves tenants"),
        }
    }

    /// Whether `collection` is `append-only` in the policy.
    fn append_only(&self, collection: &str) -> bool {
        self.append_only.iter().any(|a| a == "*" || a == collection)
    }

    /// `None`: no access. `Some(None)`: every row. `Some(Some(f))`: the rows
    /// `f` matches -- the filters of the rules granting `op`, any of them.
    fn filter(&self, collection: &str, op: u8) -> Option<Option<Expr>> {
        if op & (UPDATE | DELETE) != 0 && self.append_only(collection) {
            return None;
        }
        let mut any = false;
        let mut every = false;
        let mut either: Option<Expr> = None;
        for r in &self.rules {
            let here = r.collection.as_deref().is_none_or(|c| c == collection);
            if !here || r.ops & op == 0 {
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
        self.filter(collection, READ).is_some()
    }

    /// Whether any rule grants the token anything on `collection`: one
    /// none does does not exist for it.
    fn known(&self, collection: &str) -> bool {
        self.rules
            .iter()
            .any(|r| r.ops != 0 && r.collection.as_deref().is_none_or(|c| c == collection))
    }

    /// Why the token may not `what` in `collection`: it does not exist for
    /// a token granted nothing there.
    fn refused(&self, collection: &str, what: &str) -> Error {
        if !self.known(collection) {
            return Error::NotFound(format!("collection `{collection}`"));
        }
        denied(format!("this token may not {what} `{collection}`"))
    }

    /// The read filter of `collection`, ANDed into `filter`; a collection
    /// the token is granted nothing on does not exist for it.
    pub fn restrict(&self, collection: &str, filter: Option<Expr>) -> Result<Option<Expr>> {
        let f = self
            .filter(collection, READ)
            .ok_or_else(|| self.refused(collection, "read"))?;
        Ok(and(filter, f))
    }

    /// Every level of the query held to the token's read rules: its own
    /// filter, each `lookup` level's, and each inner `get` of an `in (get
    /// ...)` in any of them -- which reads its collection as any `get`
    /// does, so a token that may not read a row there learns nothing of
    /// it through the list it would have made.
    fn select(&self, mut sel: Select) -> Result<Select> {
        sel.each_filter_mut(&mut |collection, filter| {
            self.inner(filter)?;
            let f = self.reaping(collection, filter.take())?;
            *filter = self.restrict(collection, f)?;
            Ok(())
        })?;
        // A `match` held to some of the rows is scored over the rows its
        // filter selects, not the collection: BM25's statistics over every
        // row told a user how many rows it could not read held a word --
        // alice's own memo scored 9.87, and 4.79 once bob had written 200
        // private ones holding it.
        let held = matches!(self.filter(&sel.collection, READ), Some(Some(_)));
        if let Some(m) = sel.matcher.as_mut().filter(|_| held) {
            m.within = true;
        }
        Ok(sel)
    }

    /// `filter`, and where it asks `expired()` the filters of the rules
    /// granting `expired` ANDed in: a token reads, changes or deletes the
    /// rows past their time only by a grant naming them, beside the one
    /// its statement needs anyway.
    fn reaping(&self, collection: &str, filter: Option<Expr>) -> Result<Option<Expr>> {
        if !filter.as_ref().is_some_and(Expr::asks_expired) {
            return Ok(filter);
        }
        match self.filter(collection, EXPIRED) {
            Some(f) => Ok(and(filter, f)),
            None if !self.known(collection) => Err(self.refused(collection, "read")),
            None => Err(denied(format!(
                "this token may not read the rows of `{collection}` past their time: \
                 that is the `expired` grant's"
            ))),
        }
    }

    /// Each inner `get` in `filter` held to the read rules ([`Self::select`]).
    fn inner(&self, filter: &mut Option<Expr>) -> Result<()> {
        let Some(f) = filter else {
            return Ok(());
        };
        f.each_subquery_mut(&mut |e| {
            if let Expr::InSelect(_, sel) = e {
                **sel = self.select(std::mem::take(&mut **sel))?;
            }
            Ok(())
        })
    }

    /// The rows of `collection` the token may `op` -- an insert's
    /// documents, as written, an update's before and after, a delete's --
    /// or why it may not. An update and a delete find their rows by a
    /// filter, so they are of rows the token may read, as PostgreSQL's
    /// need `SELECT` beside them; an insert needs no read.
    fn writable(&self, collection: &str, op: u8) -> Result<Option<Expr>> {
        let verb = match op {
            INSERT => "insert into",
            UPDATE => "update rows of",
            _ => "delete rows of",
        };
        if op != INSERT {
            if !self.readable(collection) {
                return Err(self.refused(collection, verb));
            }
            if self.append_only(collection) {
                return Err(denied(format!(
                    "`{collection}` is append-only: a scoped token inserts into it, and \
                     neither updates nor deletes its rows"
                )));
            }
        }
        self.filter(collection, op)
            .ok_or_else(|| self.refused(collection, verb))
    }

    /// The statement as this token may run it, or why it may not.
    pub fn rewrite(&self, stmt: Statement) -> Result<Statement> {
        Ok(match stmt {
            Statement::Select(sel) => Statement::Select(self.select(sel)?),
            Statement::Explain(sel) => Statement::Explain(self.select(sel)?),
            Statement::Put {
                collection,
                mut docs,
                insert,
                if_absent,
                require,
            } => {
                let f = self.writable(&collection, INSERT)?;
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
                // An insert stays one: made a put here, it would write over.
                Statement::Put {
                    collection,
                    docs,
                    insert,
                    if_absent,
                    require,
                }
            }
            // `require` counts the rows the token's filter let it write.
            Statement::Update {
                collection,
                set,
                mut filter,
                require,
            } => {
                let f = self.writable(&collection, UPDATE)?;
                self.inner(&mut filter)?;
                let filter = self.reaping(&collection, filter)?;
                Statement::Update {
                    collection,
                    set,
                    filter: and(filter, f),
                    require,
                }
            }
            Statement::Delete {
                collection,
                mut filter,
                require,
            } => {
                let f = self.writable(&collection, DELETE)?;
                self.inner(&mut filter)?;
                let filter = self.reaping(&collection, filter)?;
                Statement::Delete {
                    collection,
                    filter: and(filter, f),
                    require,
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

    /// Whether `doc`, as `op` would write it to `collection`, is one this
    /// token may write: an insert's by the rules granting `insert`, an
    /// update's by those granting `update`.
    fn admits(&self, schema: &Schema, op: u8, doc: &Document, registry: &Registry) -> Result<bool> {
        match self.filter(&schema.name, op) {
            None => Ok(false),
            Some(None) => Ok(true),
            Some(Some(f)) => {
                let ctx = EvalCtx {
                    params: &[],
                    registry,
                    clock: None,
                };
                Ok(truthy(&eval(&f, &mut Written(doc, schema), &ctx)?))
            }
        }
    }
}

/// A document being written, read as the filter reads a stored one: a
/// field in a collation compares in it here too, or a token could write a
/// row it would not be shown.
struct Written<'a>(&'a Document, &'a Schema);

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
    fn collation(&self, name: &str) -> Option<Collation> {
        self.1.field(name).and_then(|f| f.collate)
    }
}

thread_local! {
    /// The scope the write running on this thread is made under. A
    /// thread-local, not a parameter: the check runs as a write hook, deep
    /// in the engine's write path, which knows nothing of tokens.
    static CURRENT: RefCell<Option<Arc<Scope>>> = const { RefCell::new(None) };
}

/// Runs `f` -- a statement executing under the write lock -- as `who`: the
/// documents it writes are checked against the token's rules, and what a
/// refusal tells is what the token may know ([`told`]).
pub fn within<T>(who: &Who, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let Who::Scoped(scope) = who else {
        return f();
    };
    with_scope(scope, f).map_err(told)
}

/// A refusal as a scoped token is told it. A `@unique` clash names the
/// document holding the value and echoes the value: told to a token whose
/// rows are its own, it said that another user's row exists, its id and
/// what it holds (`insert profiles {email: 'bob@x.io'}` answered "document
/// 1 holds \"bob@x.io\" already"). The token is told the field alone; that
/// the value is taken is what uniqueness itself says. A scoped token names
/// no `id` ([`Scope::rewrite`]), so every duplicate it meets is a clash.
fn told(e: Error) -> Error {
    match e {
        Error::Duplicate(m) => Error::Duplicate(match m.split_once(" is unique, and document ") {
            Some((field, _)) => format!("{field} is unique, and the value is taken"),
            None => "a unique value is taken".into(),
        }),
        e => e,
    }
}

fn with_scope<T>(scope: &Arc<Scope>, f: impl FnOnce() -> T) -> T {
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
    fn before_write(&self, schema: &Schema, op: WriteOp, doc: &mut Document) -> Result<()> {
        let Some(scope) = CURRENT.with(|c| c.borrow().clone()) else {
            return Ok(());
        };
        let op = match op {
            WriteOp::Insert => INSERT,
            WriteOp::Update => UPDATE,
            // A delete's rows were found through its filter, the token's
            // ANDed in; nothing is written to check.
            WriteOp::Delete => return Ok(()),
        };
        if scope.admits(schema, op, doc, &self.registry)? {
            return Ok(());
        }
        Err(denied(format!(
            "the document is outside what this token may write to `{}`",
            schema.name
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
        let key = Jwk {
            kid: None,
            kind: KeyKind::Hmac(b"your-256-bit-secret".to_vec()),
        };
        // It names no `exp`, which a server takes only when told to.
        let a = Access::with(vec![key], None, "")
            .unwrap()
            .demanding(Demands {
                require_exp: false,
                ..Demands::default()
            });
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
            "only HS256 and RS256 tokens are accepted"
        );
        let early = a.mint(r#"{"sub":"alice","nbf":5000}"#).unwrap();
        assert!(a.verify(&early, 1_000).is_err());
        assert!(Access::new(b"short", "").is_err());
    }

    /// A token with no `exp` was taken for ever: one that leaked could be
    /// outlived only by rotating every key.
    #[test]
    fn a_token_without_exp_is_refused_unless_told() {
        let a = access("");
        let signed = |claims: &[u8]| {
            let head = b64url_encode(br#"{"alg":"HS256","typ":"JWT"}"#);
            let body = b64url_encode(claims);
            let sig = hmac_sha256(SECRET, format!("{head}.{body}").as_bytes());
            format!("{head}.{body}.{}", b64url_encode(&sig))
        };
        let forever = signed(br#"{"sub":"alice"}"#);
        assert_eq!(
            a.verify(&forever, 1_000).unwrap_err(),
            "the token has no exp: one without would be good for ever"
        );
        // Minted with none, it gets an hour.
        let minted = a.mint(r#"{"sub":"alice"}"#).unwrap();
        let now = unix_now();
        assert!(a.verify(&minted, now).is_ok());
        assert!(a.verify(&minted, now + MINTED_FOR + 1).is_err());
        assert!(a.mint("{}").is_ok() && a.mint(" { } ").is_ok());

        let lax = access("").demanding(Demands {
            require_exp: false,
            ..Demands::default()
        });
        assert!(lax.verify(&forever, 1_000).is_ok());

        // An `exp` further ahead than --jwt-max-age is no `exp`.
        let capped = access("").demanding(Demands {
            max_age: Some(600),
            ..Demands::default()
        });
        let far = signed(br#"{"sub":"alice","exp":100000}"#);
        assert!(capped.verify(&far, 99_500).is_ok());
        assert_eq!(
            capped.verify(&far, 1_000).unwrap_err(),
            "the token's exp is further ahead than this server takes"
        );
    }

    /// A JWKS as an identity provider publishes it -- an EC key this server
    /// passes over, an RSA key -- and tokens node:crypto signed with the
    /// RSA key's private half.
    #[test]
    fn an_identity_providers_keys_verify_its_tokens() {
        let mut lines = include_str!("jwks_vectors.txt").lines();
        let (jwks, alice, bob, confused) = (
            lines.next().unwrap(),
            lines.next().unwrap(),
            lines.next().unwrap(),
            lines.next().unwrap(),
        );
        let dir = std::env::temp_dir().join(format!("fenec-jwks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keys.json");
        std::fs::write(&path, jwks).unwrap();
        // node:crypto signed them with no `exp`.
        let a = Access::from_jwks(&path, "notes read where owner = $jwt.sub")
            .unwrap()
            .demanding(Demands {
                require_exp: false,
                ..Demands::default()
            });
        let sub = |t: &str, now| a.scope(t, now).map(|s| s.subject.unwrap_or_default());
        assert_eq!(sub(alice, 1).unwrap(), "alice", "named by its kid");
        assert_eq!(sub(bob, 1).unwrap(), "bob", "no kid: each RS256 key");
        // The modulus is public: an HS256 token signed with it is refused.
        assert_eq!(
            sub(confused, 1).unwrap_err(),
            "the token's signature does not match"
        );
        assert!(a.mint("{}").is_err(), "an RS256 key does not sign");

        // Rotation: the file changes, an oct key comes in with a kid of its
        // own, the RSA key goes -- seen the next second.
        let secret = b64url_encode(&[5u8; 32]);
        let next = format!(r#"{{"keys":[{{"kty":"oct","kid":"h2","k":"{secret}"}}]}}"#);
        std::fs::write(&path, &next).unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(sub(alice, 1).unwrap(), "alice", "looked at once a second");
        assert_eq!(sub(alice, 2).unwrap_err(), "no key has the token's kid");
        let minted = a.mint(r#"{"sub":"carol"}"#).unwrap();
        assert!(minted.starts_with(&b64url_encode(br#"{"alg":"HS256","typ":"JWT","kid":"h2"}"#)));
        assert_eq!(sub(&minted, 2).unwrap(), "carol");
        // A file that no longer reads keeps the keys read last.
        std::fs::write(&path, "{").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later + std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(sub(&minted, 3).unwrap(), "carol");

        for bad in [
            r#"{"keys":[]}"#,
            r#"{"keys":[{"kty":"oct","k":"c2hvcnQ"}]}"#,
            r#"{"keys":[{"kty":"RSA","n":"AQAB","e":"AQAB"}]}"#,
            r#"{"keys":[{"kty":"oct","use":"enc","k":"BQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQU"}]}"#,
            r#"{"nothing": 1}"#,
        ] {
            assert!(read_jwks(bad).is_err(), "{bad}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
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
        assert!(alice.writable("posts", UPDATE).is_err());
        let Some(Some(Expr::Cmp(CmpOp::Eq, _, v))) = alice.filter("notes", INSERT) else {
            panic!("a bound filter");
        };
        assert_eq!(*v, Expr::Lit(Value::Text("alice".into())));

        let dash = a
            .scope(&a.mint(r#"{"sub":"d","role":"dashboard"}"#).unwrap(), 0)
            .unwrap();
        assert!(dash.readable("secrets"));
        assert!(matches!(dash.filter("secrets", READ), Some(None)));
        assert!(dash.writable("secrets", INSERT).is_err());

        for bad in [
            "notes",
            "notes admin",
            "notes read where",
            "notes read where $1 = 2",
            "notes append-only where owner = $jwt.sub",
            "notes append-only for admin",
            "notes read,update\nnotes append-only",
            "notes delete\n* append-only",
        ] {
            assert!(Access::new(SECRET, bad).is_err(), "{bad}");
        }
    }

    /// Each write is granted on its own, and `append-only` takes updates
    /// and deletes from every scoped token, whatever `write` and `*` say.
    #[test]
    fn grants_are_per_operation_and_append_only_holds_every_token() {
        let a = access(
            "journal read,write where owner = $jwt.sub\n\
             journal append-only\n\
             events insert where user = $jwt.sub\n\
             notes read,insert,update where owner = $jwt.sub\n\
             * write for admin\n\
             * read for admin\n",
        );
        let alice = a.scope(&a.mint(r#"{"sub":"alice"}"#).unwrap(), 0).unwrap();
        assert!(alice.writable("journal", INSERT).is_ok());
        for op in [UPDATE, DELETE] {
            let e = alice.writable("journal", op).unwrap_err();
            assert!(matches!(e, Error::Denied(_)) && e.to_string().contains("append-only"));
        }
        // Inserted into, never read: not listed, and its reads refused.
        assert!(alice.writable("events", INSERT).is_ok());
        assert!(!alice.readable("events"));
        assert!(matches!(
            alice.restrict("events", None),
            Err(Error::Denied(_))
        ));
        assert!(matches!(
            alice.writable("events", DELETE),
            Err(Error::Denied(_))
        ));
        assert!(alice.writable("notes", UPDATE).is_ok());
        assert!(matches!(
            alice.writable("notes", DELETE),
            Err(Error::Denied(_))
        ));
        // A collection granted nothing does not exist.
        assert!(matches!(
            alice.writable("secrets", INSERT),
            Err(Error::NotFound(_))
        ));

        // A role claim may be a list.
        let admin = a
            .scope(
                &a.mint(r#"{"sub":"root","role":["ops","admin"]}"#).unwrap(),
                0,
            )
            .unwrap();
        assert!(admin.writable("secrets", DELETE).is_ok());
        assert!(admin.writable("journal", DELETE).is_err());
    }

    /// `in $jwt.rooms` takes a list claim's values, and a token listing
    /// more than the bound is refused.
    #[test]
    fn a_list_claim_is_spread_into_in() {
        let a = access(
            "messages read,insert where room in $jwt.rooms\n\
             rooms read where id in [$jwt.rooms, 0]\n",
        );
        let s = a
            .scope(&a.mint(r#"{"sub":"a","rooms":["r1","r2"]}"#).unwrap(), 0)
            .unwrap();
        let Some(Some(Expr::In(_, items))) = s.filter("messages", READ) else {
            panic!("an in list");
        };
        assert_eq!(
            items,
            vec![
                Expr::Lit(Value::Text("r1".into())),
                Expr::Lit(Value::Text("r2".into()))
            ]
        );
        // Numbers stay as written, not a vector's f32s.
        let s = a
            .scope(&a.mint(r#"{"sub":"a","rooms":[123456789]}"#).unwrap(), 0)
            .unwrap();
        let Some(Some(Expr::In(_, items))) = s.filter("rooms", READ) else {
            panic!("an in list");
        };
        assert_eq!(
            items,
            vec![Expr::Lit(Value::Int(123456789)), Expr::Lit(Value::Int(0))]
        );
        // A text claim is a list of one.
        let s = a
            .scope(&a.mint(r#"{"sub":"a","rooms":"r1"}"#).unwrap(), 0)
            .unwrap();
        let Some(Some(Expr::In(_, items))) = s.filter("messages", READ) else {
            panic!("an in list");
        };
        assert_eq!(items.len(), 1);
        let many: Vec<String> = (0..=MAX_CLAIM_VALUES)
            .map(|i| format!("\"r{i}\""))
            .collect();
        let claims = format!(r#"{{"sub":"a","rooms":[{}]}}"#, many.join(","));
        assert!(a.scope(&a.mint(&claims).unwrap(), 0).is_err());
    }
}
