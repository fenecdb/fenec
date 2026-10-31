//! Scoped access over HTTP: who a request is, and what it may read and
//! write.
//!
//! A server's `--http-token` is all or nothing. With `--jwt-secret` it also
//! takes HS256 JSON Web Tokens -- with `--jwt-keys`, those a JWKS file's
//! keys verify, HS256, RS256 and ES256 (below) -- and a policy file says
//! what each may do,
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
//! `role` claim -- or the claim `--jwt-role-claim` names -- names that role
//! (a text, or a list holding it); a role of more than one word, or holding
//! a `#`, is quoted: `for "Account Admin"`. Rules on one collection widen
//! each other, as permissive policies do. A collection no rule grants a
//! token anything does not exist as far as that token can tell. A `#`
//! outside quotes starts a comment.
//!
//! **A claim's name** is letters, digits and `_` as they stand
//! (`$jwt.sub`), or any other in double quotes: `$jwt."cognito:groups"`,
//! `$jwt."https://example.com/roles"`. A top-level claim alone: a path into
//! a nested object is refused ([`claim_name`] says why), and so is an
//! unquoted name running on into `-` or `:` (`$jwt.x-hasura-user-id`).
//!
//! **Grants** are `read`, `insert`, `update` and `delete`, and `write` is
//! the three writes. `update(balance, held)` grants an update of those
//! fields alone: a write over a row by a scoped token -- a `set`, REST
//! `PATCH` and `PUT`, a `/batch` -- is judged by the fields it changes,
//! which must all be one granting rule's, a rule admitting the row before
//! and after ([`Scope::overwrites`], through [`Check`]'s
//! `before_overwrite`). A `put` naming an id would be judged the same way,
//! by the fields that differ; a scoped token names none. An `insert` is a put of new documents; `update` is a
//! `set`, and `delete` a `del`, of rows the token may also read. A token
//! may insert into a collection it cannot read -- an event stream, a trail
//! a client appends to -- which is then not listed to it. `expired` grants
//! the rows past their `@ttl` that `expired()` reads, a reaper's: a
//! statement asking it is held to those rules' filters as well, and
//! neither `write` nor `*` grants it.
//!
//! **Fields a read shows.** `read(pickup, run_at)` grants the rows its
//! filter admits with those fields alone, and `id`: a row shows the fields
//! of every read rule admitting it, the rest answering `null` -- a courier
//! sees a ready order's pickup, and the customer's door once the order is
//! theirs. A field no rule grants is not there for the token (`\d`,
//! `/_schema`, `select *`). One a rule withholds must not show through what
//! a statement does with it either, so a `where` term reading it holds only
//! where the row shows it, and `order`, `group`, `facet`, an aggregate,
//! `match`, `near`, a `lookup` key and a `set`'s values may read it only
//! where every row they reach shows it (`fields`).
//!
//! **Transitions.** A rule's filter reads the row as it stands, and
//! `new.<field>` reads it as the write leaves it -- as `new.` names the
//! document in an upsert's `else set`. A rule that names `new.` is a
//! transition: it grants `update` and nothing else, and it is true or not
//! of the two rows together, where any other rule holds the row before and
//! the row after to its filter each:
//!
//! ```text
//! orders  update(status, run_at)  where customer = $jwt.sub and status = "ready" and new.status = "cancelled" and new.run_at is null
//! ```
//!
//! lets a customer cancel a ready order and nothing else -- not put a
//! cancelled one back, not move one up the queue -- where the one filter
//! `status = "ready" or status = "cancelled"` let the row go either way.
//! A write over a row is the token's when one rule granting `update` there
//! names every field it changes (or names none) and admits it: a
//! transition by being true of the pair, another rule by admitting the row
//! before and the row after. Where no rule of the collection names fields
//! or `new.`, rules widen each other as PostgreSQL's permissive policies
//! do, the row before one's and the row after another's ([`Scope::overwrites`]).
//! A transition picks the rows a `set` may write by what it asks of the
//! row before alone, its `new.` terms taken as passing ([`relaxed`]), and
//! judges each pair as it is written. The row after is held to what the
//! rule says of `new.` and nothing more: what must not move, its field
//! list leaves out.
//!
//! **`returning`** answers what a write wrote, so the rows it answers are
//! held to the read rules: a `set`'s as it wrote them, a `del`'s as they
//! were. A `del` and a `set` pass over the rows the read rules leave out by
//! a field the write does not change -- as a `get` passes them over -- and
//! a `set` that would leave a row its token may not read is refused whole
//! (403), nothing written: written and left out of the answer, a claim
//! would hold a job its worker was never told of; picked by the read rules
//! as the row stood, a courier's claim of a ready order -- theirs to read
//! once claimed -- took nothing and said nothing ([`Scope::answerable`]).
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
//! **A claim names someone or is not there.** A rule naming a claim the
//! token lacks does not apply, and neither does one naming a claim the
//! token holds as `null`, as `""`, as an object, as a list where the rule
//! reads one value, or as a list holding any of those ([`usable`]): bound,
//! `owner = $jwt.sub` given `null` read every row with no owner. A rule
//! only grants, so one that does not apply grants nothing. A claim a rule
//! orders by (`level <= $jwt.clearance`) holds the other side to its kind,
//! under a `not` too ([`kind_held`]); one inside a call or a sum, or in
//! order inside a value, is refused at startup ([`claims_held`]). The
//! tenant claim is the one whose absence widens (`--jwt-unbound-tenants`):
//! held as a value naming no tenant, it reaches none. A `sub` of `""` is no
//! subject.
//!
//! **A rule may read another collection** through `in (get ...)`, its
//! claims bound inside the inner `get` as anywhere in the filter:
//!
//! ```text
//! positions  read  where courier in (get orders select courier where customer = $jwt.sub and (status = "assigned" or status = "picked"))  for customer
//! ```
//!
//! lets a customer read the position of the courier carrying their order.
//! The inner `get` reads the rows of `orders` the token's own `get` would,
//! held to its rules there ([`Scope::held`]: none granting `read` is no
//! row, as PostgreSQL's row security lets a policy's subquery see no row
//! of a table no policy grants); the fields it reads are the policy's, not
//! held to the token's field lists, since its list is never shown, only
//! what it admits. It is answered under the lock that runs each statement,
//! before it runs ([`Scope::bound`]) -- a write's `WITH CHECK` tests a
//! document on its own, where no query runs, as a row's mask asks which
//! rules admit it (`fields`) -- and a `/batch` answers it again for each
//! statement, since the one before may have changed what it reads. A rule
//! naming the fields it shows may read another collection as any other.
//! Its list is held to [`MAX_CLAIM_VALUES`], as a
//! list claim is, and refused past it rather than cut. Rules that read each
//! other's collections, at any depth and whatever their roles, are refused
//! at startup: none of them could be answered first. A scoped subscription
//! whose rules read another collection is bound again when that collection
//! is written, its rows entering and leaving as the answer moves (`sse`).
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
//! provider publishes it: `oct` keys verify HS256, `RSA` keys RS256 and
//! `EC` keys on P-256 ES256, so a token an identity provider signs is taken
//! with its public keys alone.
//! A key's kind decides the algorithm it verifies, never the token: an
//! RS256 key's modulus is public, and taken for an HS256 secret it would
//! sign anything. A token naming a `kid` is checked against that key alone;
//! one naming none against each key of its algorithm. The file is read
//! again when it changes, looked at once a second at most, which is the
//! rotation: add the new key, sign with it, and take the old one out once
//! the tokens it signed have expired. A file that no longer reads keeps the
//! keys read last.
//!
//! **Whom a token is for.** A provider's keys sign every token it issues,
//! for every app of a tenant, team or pool: a good signature says who
//! issued a token, not that it was issued for this database. Told issuers,
//! audiences or clients ([`Demands`]: `--jwt-issuer`, `--jwt-audience`,
//! `--jwt-client`), the server takes only a token whose `iss` is one of
//! them, whose `aud` (a text or a list) holds one, whose `azp` -- or
//! `client_id`, where it names no `azp` -- is one; told none, it asks
//! nothing of them, as before. A refusal is 401 naming the check, never the
//! token's value.

use crate::crypto::{b64url_decode, b64url_encode, ct_eq, hmac_sha256, P256Key, RsaKey};
use fenec_core::prelude::*;
use fenec_core::query::{eval, truthy, CmpOp, EvalCtx, Pick, RowAccess};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

mod fields;
pub use fields::Mask;

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

    /// Whether a rule of the token reads another collection: its
    /// statements are then held to its rules as they answer under the lock
    /// that runs them ([`Scope::bound`]), and every other token's path is
    /// the one it was.
    pub fn reaches_out(&self) -> bool {
        matches!(self, Who::Scoped(s) if s.reaching)
    }
}

/// The keys tokens are checked against, and the policy.
pub struct Access {
    keys: RwLock<Arc<Vec<Jwk>>>,
    /// The JWKS file the keys are read again from as it changes.
    source: Option<Source>,
    rules: Vec<Rule>,
    /// Whether a rule reads another collection: a token's rules are then
    /// bound for a `not` as well ([`hold`]).
    reaching: bool,
    /// The collections no scoped token updates or deletes in (`append-only`
    /// lines), `*` for every one.
    append_only: Arc<[String]>,
    /// Tokens already verified, by their text: a client sends the same one
    /// for as long as it lives, and an RS256 verification takes 38 us (166
    /// in 32-bit limbs), an ES256 one 145, an HS256 token's with its parses
    /// 2.9, a token found here 0.5 (`make jwt-bench`).
    /// Emptied when the keys change, since a key taken out must take its
    /// tokens with it; `exp` and `nbf` are asked on every request.
    verified: [Mutex<HashMap<Box<str>, Claims>>; SHARDS],
    /// [`Demands::role_claim`], shared with each [`Scope`] for `/_whoami`
    /// to read the roles by: a reference count a request, where the name
    /// copied was an allocation.
    role_claim: Arc<str>,
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
    /// The issuers a token's `iss` must be one of (`--jwt-issuer`); none
    /// asks nothing of it. A provider's keys sign every token it issues:
    /// one key file read by servers of more than one provider's -- an `oct`
    /// key shared, a file of several providers' keys -- takes any of their
    /// tokens. Compared as written, a trailing `/` and all (Auth0's issuer
    /// ends in one).
    pub issuers: Vec<String>,
    /// The audiences a token's `aud` -- a text, or a list -- must hold one
    /// of (`--jwt-audience`); none asks nothing of it. One issuer's keys
    /// sign for every app it serves -- a Cloudflare Access team's every
    /// application, a Cognito pool's every app client, an Auth0 tenant's
    /// every API -- and a token for another is told apart by its `aud`.
    pub audiences: Vec<String>,
    /// The clients a token's `azp`, or its `client_id` where it names no
    /// `azp`, must be one of (`--jwt-client`); none asks nothing of it.
    /// Cognito's access tokens carry the app client's id in `client_id`,
    /// and an `aud` only when bound to a resource server; Auth0's,
    /// Keycloak's and Clerk's name the app in `azp` (Clerk's leave it out
    /// where the request carried no `Origin`).
    pub clients: Vec<String>,
    /// The claim a rule's `for <role>` reads (`--jwt-role-claim`, `role`
    /// unless told): a text, or a list holding the role. A provider names
    /// its own -- Cognito's `cognito:groups`, Auth0's a namespaced URL.
    pub role_claim: String,
}

impl Default for Demands {
    fn default() -> Demands {
        Demands {
            require_exp: true,
            max_age: None,
            tenant_claim: "tenant".into(),
            unbound_tenants: false,
            issuers: Vec::new(),
            audiences: Vec::new(),
            clients: Vec::new(),
            role_claim: "role".into(),
        }
    }
}

/// What a 401 says of a token the demands refuse: the check it failed,
/// never the value the token holds -- which a refused request's own sender
/// knows, and a log or a proxy in between need not.
const NO_ISSUER: &str = "the token names no issuer (iss) this server takes";
const NO_AUDIENCE: &str = "the token names no audience (aud) this server takes";
const NO_CLIENT: &str = "the token names no client (azp, or client_id) this server takes";

impl Demands {
    /// Whether `claims` name an issuer, an audience and a client this
    /// server takes, where it was told any. An empty text names none, as
    /// an empty token is none: a server told `""` takes no token.
    fn whom(&self, claims: &[(String, Value)]) -> std::result::Result<(), &'static str> {
        let claim = |name: &str| claims.iter().find(|(k, _)| k == name).map(|(_, v)| v);
        let one_of = |v: Option<&Value>, list: &[String]| match v {
            Some(Value::Text(t)) => !t.is_empty() && list.iter().any(|x| x == t),
            _ => false,
        };
        if !self.issuers.is_empty() && !one_of(claim("iss"), &self.issuers) {
            return Err(NO_ISSUER);
        }
        if !self.audiences.is_empty() {
            // RFC 7519: a list of texts, or a single text.
            let held = match claim("aud") {
                Some(Value::List(l)) => l.iter().any(|a| one_of(Some(a), &self.audiences)),
                a => one_of(a, &self.audiences),
            };
            if !held {
                return Err(NO_AUDIENCE);
            }
        }
        // `azp` where the token names one: a token naming both is judged
        // by the party it was issued to, not by a `client_id` beside it.
        if !self.clients.is_empty()
            && !one_of(claim("azp").or_else(|| claim("client_id")), &self.clients)
        {
            return Err(NO_CLIENT);
        }
        Ok(())
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
    /// ES256.
    Ec(P256Key),
}

const SHORT: &str = "a JWT secret shorter than 32 bytes can be guessed: HS256 needs 256 bits of it";

/// The keys of a JWKS file: `{"keys": [...]}`, or the list alone. A key of
/// another kind (`OKP`), an `EC` key on another curve than P-256, or one for
/// encryption (`"use": "enc"`) is passed over, as a verifier that does not
/// know it would; a file with no key this server can use is refused.
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
        // An `EC` key on P-384 or P-521 is ES384's or ES512's, passed over.
        let p256 = text("crv") == Some("P-256");
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
            Some("EC") if p256 && alg.is_none_or(|a| a == "ES256") => KeyKind::Ec(
                P256Key::new(&bytes("x")?, &bytes("y")?)
                    .ok_or("an EC key's `x` and `y` must be a point of P-256, 32 bytes each")?,
            ),
            _ => continue,
        };
        keys.push(Jwk { kid, kind });
    }
    if keys.is_empty() {
        return Err("no HS256, RS256 or ES256 key in the JWKS file".into());
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
    /// `update(a, b)`: the fields an update by this rule may change; `None`
    /// for every one (`update`, `write`).
    fields: Option<Vec<String>>,
    /// `read(a, b)`: the fields a row this rule admits shows; `None` for
    /// every one (`read`).
    read_fields: Option<Vec<String>>,
    /// With `$jwt.<claim>` as parameter `k`, the claim `claims[k]`.
    filter: Option<Expr>,
    /// A transition's (a filter naming `new.`): what it asks of the row
    /// before alone ([`relaxed`]), which picks the rows; `None` for every
    /// row, and for a rule that is no transition.
    before: Option<Expr>,
    transition: bool,
    /// The filter as the policy writes it, `$jwt.<claim>` and all: what
    /// `/_whoami` shows a token it applies to.
    text: Option<Arc<str>>,
    claims: Vec<String>,
    /// Beside each of `claims`, whether the filter reads it as a list
    /// wherever it reads it -- in an `in`'s list, or as the list `has`
    /// looks in -- so that a token may give a list there ([`usable`]).
    lists: Vec<bool>,
    role: Option<String>,
    /// The collections its inner `get`s read, at any depth: what a ring of
    /// rules reading each other is found by at startup ([`ring`]).
    reads: Vec<String>,
}

/// A token's rules, its claims bound in.
pub struct Scope {
    subject: Option<String>,
    /// When the token lapses, in milliseconds since the epoch: what a
    /// request held open past it -- a subscription -- is ended at.
    expires: Option<u64>,
    rules: Vec<Bound>,
    /// Whether a rule reads another collection ([`Who::reaches_out`]).
    reaching: bool,
    /// The tenants the token names in its tenant claim; `None` where it
    /// names none.
    tenants: Option<Vec<String>>,
    /// A token naming no tenant reaches every one (`--jwt-unbound-tenants`).
    unbound: bool,
    /// The policy's `append-only` collections, `*` for every one.
    append_only: Arc<[String]>,
    /// Whether a read rule of the token names its fields (`read(a, b)`):
    /// a token none of whose do takes the path it took before them.
    fielded: bool,
    /// The token's claims and the claim a rule's `for` reads, for
    /// `/_whoami` to say which roles the server took (the studio shows
    /// them): read there alone, so a request pays two reference counts.
    roles: (Claims, Arc<str>),
}

#[derive(Clone)]
struct Bound {
    collection: Option<String>,
    ops: u8,
    fields: Option<Vec<String>>,
    read_fields: Option<Vec<String>>,
    filter: Option<Expr>,
    /// [`Rule::before`], its claims bound.
    before: Option<Expr>,
    transition: bool,
    text: Option<Arc<str>>,
    /// Its filter holds an inner `get` not answered yet ([`Scope::bound`]).
    reaches: bool,
}

impl Bound {
    /// What the rule asks of a row as it stands: its filter, or a
    /// transition's half of it the row before can answer.
    fn using(&self) -> &Option<Expr> {
        match self.transition {
            true => &self.before,
            false => &self.filter,
        }
    }
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
            let line = uncommented(line).trim();
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
        for (r, n) in rules.iter().zip(&lines) {
            let Some(c) = &r.collection else { continue };
            let named = r.named & (UPDATE | DELETE) != 0;
            if named && append_only.iter().any(|a| a == "*" || a == c) {
                return Err(format!(
                    "policy line {n}: `{c}` is append-only, and the rule grants `update` or \
                     `delete` there"
                ));
            }
        }
        if let Some(why) = ring(&rules, &lines) {
            return Err(why);
        }
        Ok(Access {
            keys: RwLock::new(Arc::new(keys)),
            source,
            reaching: rules.iter().any(|r| !r.reads.is_empty()),
            rules,
            append_only: append_only.into(),
            verified: Default::default(),
            role_claim: Arc::from(Demands::default().role_claim.as_str()),
            demands: Demands::default(),
        })
    }

    /// The same keys and policy, holding tokens to `demands`. A token kept
    /// passed the demands it was checked under ([`Access::check`]), so
    /// those kept are let go.
    pub fn demanding(mut self, demands: Demands) -> Access {
        self.role_claim = Arc::from(demands.role_claim.as_str());
        self.demands = demands;
        for v in &self.verified {
            v.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
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
    /// `exp` get one [`MINTED_FOR`] seconds from now -- or `--jwt-max-age`
    /// from now where that is sooner: a server refuses a token without one
    /// ([`Demands::require_exp`]) and one further ahead than its age. On a
    /// server told an issuer, an audience or a client, claims naming none
    /// get the first it was told (`iss`, `aud`, `azp`). Either way, it
    /// mints no token it would refuse.
    pub fn mint(&self, claims: &str) -> std::result::Result<String, String> {
        let parsed = fenec_core::json::parse_object(claims).map_err(|e| e.to_string())?;
        let mut more = String::new();
        let mut stamp = |name: &str, value: &str| {
            if !parsed.iter().any(|(k, _)| k == name) {
                more.push_str(&format!(",\"{name}\":{value}"));
            }
        };
        let life = self
            .demands
            .max_age
            .map_or(MINTED_FOR, |m| m.min(MINTED_FOR));
        stamp("exp", &(unix_now() + life).to_string());
        let d = &self.demands;
        for (name, told) in [
            ("iss", &d.issuers),
            ("aud", &d.audiences),
            ("azp", &d.clients),
        ] {
            if let Some(first) = told.first() {
                let mut text = String::new();
                fenec_core::json::escape_into(&mut text, first);
                stamp(name, &text);
            }
        }
        let stamped;
        let claims = if more.is_empty() {
            claims
        } else {
            let open = claims.trim().strip_suffix('}').unwrap_or("{").trim_end();
            let more = if open.ends_with('{') {
                &more[1..]
            } else {
                &more
            };
            stamped = format!("{open}{more}}}");
            &stamped
        };
        let keys = self.keys.read().unwrap_or_else(|e| e.into_inner()).clone();
        let (kid, secret) = keys
            .iter()
            .find_map(|k| match &k.kind {
                KeyKind::Hmac(s) => Some((k.kid.as_deref(), s)),
                KeyKind::Rsa(_) | KeyKind::Ec(_) => None,
            })
            .ok_or(
                "no HS256 key to sign with: an RS256 or ES256 key's private half is its owner's",
            )?;
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

    /// The scope of a token, checked: HS256, RS256 or ES256, signed with one
    /// of the keys, not expired and already valid, at `now` seconds since
    /// the epoch.
    pub fn scope(&self, token: &str, now: u64) -> std::result::Result<Scope, &'static str> {
        let claims = self.verify(token, now)?;
        let claim = |name: &str| claims.iter().find(|(k, _)| k == name).map(|(_, v)| v);
        // A role is a text, or a list of them, the token holding each, in
        // the claim the server reads roles from (`role` unless told).
        let has_role = |role: &str| match claim(&self.demands.role_claim) {
            Some(Value::Text(r)) => r == role,
            Some(Value::List(l)) => l.iter().any(|v| v.as_text() == Some(role)),
            _ => false,
        };
        let mut rules = Vec::new();
        // Each rule bound for a place under an odd number of `not`s as
        // well, where a rule reads another collection: what a `not ... in`
        // over its collection is held to ([`hold`]).
        let mut negated = Vec::new();
        'rules: for r in &self.rules {
            if r.role.as_deref().is_some_and(|role| !has_role(role)) {
                continue;
            }
            // A rule naming a claim the token lacks does not apply: a filter
            // with a hole in it would match rows it was never meant to. Nor
            // does one naming a claim the token holds as `null`, `""` or a
            // value the rule cannot compare where it reads it ([`usable`]):
            // bound, each was such a hole. A rule grants and never takes
            // away, so one that does not apply grants nothing.
            let mut values = Vec::with_capacity(r.claims.len());
            for (c, &list) in r.claims.iter().zip(&r.lists) {
                match claim(c) {
                    Some(Value::List(l)) if l.len() > MAX_CLAIM_VALUES => {
                        return Err("a list claim the policy reads holds more than 1 000 values")
                    }
                    Some(v) if usable(v, list) => values.push(v.clone()),
                    _ => continue 'rules,
                }
            }
            rules.push(Bound {
                collection: r.collection.clone(),
                ops: r.ops,
                fields: r.fields.clone(),
                read_fields: r.read_fields.clone(),
                filter: r.filter.as_ref().map(|f| bind(f, &values)),
                before: r.before.as_ref().map(|f| bind(f, &values)),
                transition: r.transition,
                text: r.text.clone(),
                reaches: !r.reads.is_empty(),
            });
            if self.reaching {
                negated.push(r.filter.as_ref().map(|f| bind_held(f, &values, false)));
            }
        }
        let reaching = rules.iter().any(|r| r.reaches);
        if reaching {
            Scope::held(&mut rules, negated);
        }
        // Anything but a text or a list of texts names no tenant: a number
        // or an object compared as text could be made to match. Only a
        // token without the claim names none, which `--jwt-unbound-tenants`
        // takes for every tenant: one holding it as `null`, `""`, a number
        // or a list of no tenant's name is held to none, since the issuer
        // meant to bind it -- `"tenant": user.tenant` minted `null` for a
        // user without one, and that token reached every tenant.
        let tenants = match claim(&self.demands.tenant_claim) {
            None => None,
            Some(Value::Text(t)) if !t.is_empty() => Some(vec![t.clone()]),
            Some(Value::List(l)) => Some(
                l.iter()
                    .filter_map(|v| match v {
                        Value::Text(t) if !t.is_empty() => Some(t.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
            Some(_) => Some(Vec::new()),
        };
        // An empty claim name is none (`--jwt-tenant-claim ""` is refused
        // at the start): read as the claim no token holds, it left every
        // token unbound, and `--jwt-unbound-tenants` took each for every
        // tenant. A library's `Demands` holding one binds every token to
        // none.
        let tenants = match self.demands.tenant_claim.is_empty() {
            true => Some(Vec::new()),
            false => tenants,
        };
        let expires = match claim("exp") {
            Some(Value::Int(i)) => Some((*i).max(0) as u64 * 1000),
            Some(Value::Float(f)) => Some((f.max(0.0) * 1000.0) as u64),
            _ => None,
        };
        let fielded = rules
            .iter()
            .any(|r| r.ops & READ != 0 && r.read_fields.is_some());
        Ok(Scope {
            // `""` is no one: taken for a subject, its Idempotency-Keys were
            // kept under the server token's own, whose answers it was then
            // replayed.
            subject: match claim("sub") {
                Some(Value::Text(s)) if !s.is_empty() => Some(s.clone()),
                _ => None,
            },
            expires,
            rules,
            reaching,
            tenants,
            unbound: self.demands.unbound_tenants,
            append_only: Arc::clone(&self.append_only),
            fielded,
            roles: (Arc::clone(&claims), Arc::clone(&self.role_claim)),
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
    fn check(
        &self,
        token: &str,
        keys: &Arc<Vec<Jwk>>,
    ) -> std::result::Result<Claims, &'static str> {
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
        let alg = match field("alg") {
            Some(a @ ("HS256" | "RS256" | "ES256")) => a,
            _ => return Err("only HS256, RS256 and ES256 tokens are accepted"),
        };
        let kid = field("kid");
        let signed = format!("{head}.{body}");
        let given = b64url_decode(sig).ok_or("not a JSON Web Token")?;
        let mut named = false;
        let good = keys
            .iter()
            .filter(|k| kid.is_none() || k.kid.as_deref() == kid)
            .inspect(|_| named = true)
            .any(|k| match (&k.kind, alg) {
                (KeyKind::Hmac(s), "HS256") => ct_eq(&given, &hmac_sha256(s, signed.as_bytes())),
                (KeyKind::Rsa(r), "RS256") => r.verify_sha256(signed.as_bytes(), &given),
                (KeyKind::Ec(e), "ES256") => e.verify_sha256(signed.as_bytes(), &given),
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
        // Asked once, before the token is kept, as its signature is: the
        // demands are fixed with the `Access`, unlike the time `exp` is
        // asked against. After the signature, so a token no key signed is
        // told nothing of what this server takes.
        self.demands.whom(&claims)?;
        #[cfg(test)]
        held::at(token);
        let mut kept = self.verified[shard(token)]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Kept only if `keys` are still the keys. A reload writes the new
        // keys and then empties every shard, so a token checked against
        // the old ones and kept after its shard was emptied outlived the
        // key it was signed with -- until its `exp`, its shard filling or
        // the next change of keys (a review kept 3-5 of a removed key's
        // tokens in 10 trials of 10). Under the shard's lock, keys still
        // current mean a reload has yet to empty this shard, and will.
        // `keys` holds the old list alive, so no new list shares its
        // address. This request is answered under the keys it was checked
        // against; the next one is held to the new keys.
        let current = self.keys.read().unwrap_or_else(|e| e.into_inner());
        if Arc::ptr_eq(keys, &current) {
            drop(current);
            if kept.len() >= KEPT {
                kept.clear();
            }
            kept.insert(token.into(), claims.clone());
        }
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
    // The grants end at the first space outside a field list:
    // `read,update(balance, held)`.
    let mut depth = 0i32;
    let end = rest
        .find(|c: char| {
            depth += (c == '(') as i32 - (c == ')') as i32;
            c.is_whitespace() && depth == 0
        })
        .unwrap_or(rest.len());
    let (grants, mut rest) = rest.split_at(end);
    let (mut ops, mut named) = (0, 0);
    // `update(a, b)`: the fields an update may change, `None` for every one;
    // `read(a, b)` the fields a read shows.
    let mut fields: Option<Vec<String>> = None;
    let mut every_field = false;
    let mut read_fields: Option<Vec<String>> = None;
    let mut every_read = false;
    for g in split_grants(grants)? {
        match g {
            "read" => {
                ops |= READ;
                every_read = true;
            }
            "write" => {
                ops |= WRITES;
                every_field = true;
            }
            "insert" => named |= INSERT,
            "update" => {
                named |= UPDATE;
                every_field = true;
            }
            "delete" => named |= DELETE,
            "expired" => ops |= EXPIRED,
            g if g.starts_with("update(") && g.ends_with(')') => {
                named |= UPDATE;
                let list = fields.get_or_insert_with(Vec::new);
                field_list(g, "update(".len(), list)?;
                if list.iter().any(|f| f == "id") {
                    return Err("`update(id)`: no write changes a document's id".into());
                }
            }
            // `id` may be named, and is shown whether or not it is: a row
            // is told apart by it, and every write and deletion names it.
            g if g.starts_with("read(") && g.ends_with(')') => {
                ops |= READ;
                field_list(g, "read(".len(), read_fields.get_or_insert_with(Vec::new))?;
            }
            other => {
                return Err(format!(
                    "`{other}` is none of read, read(<fields>), insert, update, \
                     update(<fields>), delete, write and expired"
                ))
            }
        }
    }
    ops |= named;
    if every_field {
        fields = None;
    }
    if every_read {
        read_fields = None;
    }
    let role;
    (rest, role) = role_of(rest)?;
    let rest = rest.trim();
    let mut claims = Vec::new();
    let mut reads = Vec::new();
    let text = rest.strip_prefix("where").map(|t| Arc::from(t.trim()));
    let filter = if rest.is_empty() {
        None
    } else {
        let text = rest
            .strip_prefix("where")
            .ok_or_else(|| format!("expected `where <filter>`, got `{rest}`"))?;
        // A parameter the policy writes itself stands for whichever claim
        // the rewrite below numbers so: `d = $3` after three claims was
        // taken as the third, and after fewer bound nothing, panicking every
        // request under the rule. Refused wherever it stands, a string's
        // text among them, rather than told apart from one by a second
        // reading of the lexer's.
        if let Some(at) = text
            .as_bytes()
            .windows(2)
            .position(|w| w[0] == b'$' && w[1].is_ascii_digit())
        {
            let n: String = text[at + 1..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            return Err(format!(
                "`${n}`: a policy names its parameters by claim, `$jwt.<claim>`, and \
                 numbers none of its own"
            ));
        }
        // `$jwt.sub` becomes parameter $1, the next claim $2, ...
        let mut out = String::new();
        let mut s = text;
        while let Some(at) = s.find("$jwt.") {
            out.push_str(&s[..at]);
            let (name, len) = claim_name(&s[at + 5..])?;
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
            s = &s[at + 5 + len..];
        }
        out.push_str(s);
        let stmt = fenec_ql::parse_one(&format!("get _ where {out}"))
            .map_err(|e| format!("the filter: {e}"))?;
        let Statement::Select(sel) = stmt else {
            unreachable!("a get parses as a select")
        };
        if let Some(f) = &sel.filter {
            claims_held(f, true, &claims)?;
        }
        // An inner `get` reads another collection, as the token's own `get`
        // would, and is answered before each statement under its lock
        // ([`Scope::bound`]): a document being written is tested on its
        // own (`admits`), where no query runs.
        if let Some(f) = &sel.filter {
            inner_gets(f, &mut reads)?;
        }
        // Tested against a document on its own as well, where nothing
        // answers it; what a token may reap is the `expired` grant's.
        if sel.filter.as_ref().is_some_and(Expr::asks_expired) {
            return Err("a rule's filter takes no `expired()`: grant `expired`".into());
        }
        sel.filter
    };
    // A transition says what an update may make of a row; a read, an
    // insert or a delete has no row after, or none before, for its `new.`
    // to read. Granting one beside it would hold that grant to a filter
    // it cannot answer.
    let transition = filter.as_ref().is_some_and(names_new);
    if transition && ops != UPDATE {
        let why = "a rule naming `new.` judges an update -- the row before it and the row \
                   it leaves -- and grants `update` alone";
        return Err(why.into());
    }
    let before = match (transition, &filter) {
        (true, Some(f)) => relaxed(f.clone(), &|name| name.starts_with(NEW)),
        _ => None,
    };
    let mut alone = vec![false; claims.len()];
    if let Some(f) = &filter {
        read_alone(f, &mut alone);
    }
    Ok(Rule {
        collection: (collection != "*").then(|| collection.to_string()),
        ops,
        named,
        fields,
        read_fields,
        filter,
        before,
        transition,
        text,
        claims,
        lists: alone.iter().map(|a| !a).collect(),
        role,
        reads,
    })
}

/// Refuses what a claim's kind cannot be held through ([`kind_held`]): a
/// claim inside a call or an arithmetic -- `lower($jwt.c)`, `coalesce($jwt.c,
/// 0)`, `least(level, $jwt.c)`, `$jwt.c + 1` -- whose result has a kind the
/// rule cannot know until it runs, and which given a claim of another kind
/// admitted every row (`least(level, "top")` is the level, every number
/// ranking below every text), and an ordered comparison with a claim used
/// as a value (`(level <= $jwt.c) = false`, a `case`'s condition), whose
/// guard holds only where it is a condition, standing under `and`, `or` and
/// `not` alone. `cond`: `e` is a condition. `claims` names them.
fn claims_held(e: &Expr, cond: bool, claims: &[String]) -> std::result::Result<(), String> {
    // The claim named by the last parameter `e` reads, for the refusal's words.
    let named = |e: &Expr| {
        let k = e.max_param().wrapping_sub(1);
        claims.get(k).map_or("<claim>", String::as_str).to_string()
    };
    let operand = |x: &Expr| match x {
        Expr::Param(_) => Ok(()),
        x => claims_held(x, false, claims),
    };
    match e {
        Expr::Param(_) | Expr::Field(_) | Expr::Lit(_) => Ok(()),
        Expr::And(a, b) | Expr::Or(a, b) => {
            claims_held(a, cond, claims)?;
            claims_held(b, cond, claims)
        }
        Expr::Not(a) => claims_held(a, cond, claims),
        Expr::Cmp(op, a, b) => {
            let ordered = !matches!(op, CmpOp::Eq | CmpOp::Ne);
            let claim = matches!(**a, Expr::Param(_)) || matches!(**b, Expr::Param(_));
            if ordered && claim && !cond {
                return Err(format!(
                    "`$jwt.{}` is compared in order inside a value: an ordered comparison \
                     with a claim is held to the claim's kind as a condition -- standing \
                     under `and`, `or` and `not` -- not as a value another expression reads",
                    named(e)
                ));
            }
            operand(a)?;
            operand(b)
        }
        // SQL's `like` and `ilike` (and `not like`, a `not` over one) are
        // calls in the tree, but a claim as the pattern or the escape is
        // compared in no order: a text matches it or not, and a claim of
        // another kind is a type error, which refuses the read. Refused, a
        // policy matching a home directory (`path like $jwt.home`) no
        // longer started, naming a call it never wrote.
        Expr::Call(name, args)
            if matches!(name.as_str(), "like" | "ilike")
                && args.len() > 1
                && args[0].max_param() == 0 =>
        {
            claims_held(&args[0], false, claims)?;
            args[1..].iter().try_for_each(operand)
        }
        Expr::Call(name, args) if args.iter().any(|a| a.max_param() > 0) => {
            let a = args
                .iter()
                .find(|a| a.max_param() > 0)
                .expect("one holds a claim");
            Err(format!(
                "`$jwt.{}` inside `{name}(...)`: a claim is compared as the token holds it, \
                 and a call over it could hand the rule a value of any kind -- mint the claim \
                 as the rule compares it",
                named(a)
            ))
        }
        Expr::Arith(_, a, b) if e.max_param() > 0 => {
            let a = if a.max_param() > 0 { a } else { b };
            Err(format!(
                "`$jwt.{}` in arithmetic: a claim is compared as the token holds it, and a \
                 sum over it could hand the rule a value of any kind -- mint the claim as the \
                 rule compares it",
                named(a)
            ))
        }
        Expr::Call(_, args) => args.iter().try_for_each(|a| claims_held(a, false, claims)),
        Expr::Arith(_, a, b) | Expr::Like(a, b) | Expr::Has(a, b) => {
            operand(a)?;
            operand(b)
        }
        Expr::IsNull(a) => operand(a),
        Expr::In(x, items) => {
            operand(x)?;
            items.iter().try_for_each(operand)
        }
        Expr::InSelect(x, sel) => {
            operand(x)?;
            match &sel.filter {
                Some(f) => claims_held(f, true, claims),
                None => Ok(()),
            }
        }
    }
}

/// Marks in `alone` each claim `e` reads as one value somewhere: anywhere
/// but an `in`'s list, which [`bind`] spreads a list claim into, and the
/// list `has` looks in -- inner `get`s' filters, where a rule's claims
/// also go, among them.
fn read_alone(e: &Expr, alone: &mut [bool]) {
    match e {
        Expr::Param(k) => alone[*k] = true,
        Expr::In(x, items) => {
            read_alone(x, alone);
            for i in items.iter().filter(|i| !matches!(i, Expr::Param(_))) {
                read_alone(i, alone);
            }
        }
        Expr::Has(x, y) => {
            if !matches!(**x, Expr::Param(_)) {
                read_alone(x, alone);
            }
            read_alone(y, alone);
        }
        Expr::InSelect(x, sel) => {
            read_alone(x, alone);
            if let Some(f) = &sel.filter {
                read_alone(f, alone);
            }
        }
        e => e.each_child(&mut |c| read_alone(c, alone)),
    }
}

/// Whether a claim's value is one a rule can be held to where it reads it
/// (`list`: only ever as a list, [`Rule::lists`]): a text that is not
/// empty, a finite number or a bool, or there a list of those. Anything
/// else is taken for a claim the token lacks, and the rule naming it does
/// not apply. Bound as it came, `"sub": null` made `owner = $jwt.sub` the
/// filter `owner = null`, which every row whose owner is null or missing
/// matches -- every row written before `alter collection notes add field
/// owner text` -- and an app minting `null` for an anonymous user handed
/// each of them those rows, to read, `set` and `del`; `""` read the rows an
/// app stored with no owner as `""`, and a `null` in a list claim the rows
/// `room in [...]` found with no room. An object compares with nothing a
/// filter holds a field to, nor a list where one value is read: neither
/// names anyone, so neither is let stand for someone.
fn usable(v: &Value, list: bool) -> bool {
    match v {
        Value::Text(t) => !t.is_empty(),
        Value::Int(_) | Value::Bool(_) => true,
        Value::Float(f) => f.is_finite(),
        Value::List(items) => list && items.iter().all(|i| usable(i, false)),
        _ => false,
    }
}

/// The claim `$jwt.` names at the start of `rest`, and the bytes it takes
/// there: letters, digits and `_` as they stand (`$jwt.sub`), or any other
/// name in double quotes -- `$jwt."cognito:groups"`, `$jwt."https://example.com/roles"`
/// -- where a `.`, a `:` or a `/` is part of the name, never a path.
///
/// A top-level claim alone. A path into a nested object
/// (`$jwt.app_metadata.role`) is refused rather than read: providers keep
/// what a user may edit of themselves in such objects (Supabase's and
/// Auth0's `user_metadata`) beside what only the app may (`app_metadata`),
/// a word apart in a rule, and one reading the wrong object lets a user
/// grant themselves its rows. A claim at the top is one the provider was told to
/// sign there.
fn claim_name(rest: &str) -> std::result::Result<(String, usize), String> {
    let (name, len) = match rest.strip_prefix('"') {
        Some(quoted) => {
            let close = quoted
                .find('"')
                .ok_or("`$jwt.\"` is not closed: a quoted claim is `$jwt.\"<name>\"`")?;
            let name = &quoted[..close];
            // A token's member names are read unescaped; one holding `"`
            // or `\` is no provider's, and a policy that could write one
            // would be read two ways.
            if name.contains('\\') {
                return Err(format!(
                    "`$jwt.\"{name}\"`: a quoted claim name is taken as written, with no `\\`"
                ));
            }
            (name.to_string(), close + 2)
        }
        None => {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            let len = name.len();
            // `$jwt.x-hasura-user-id` read as the claim `x` minus the field
            // `hasura`... and parsed: the rule held a subtraction no one
            // wrote. A provider's names hold `-` and `:`; quoted, they are
            // the name.
            if rest[len..].starts_with(['-', ':']) {
                let written: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | ':'))
                    .collect();
                return Err(format!(
                    "`$jwt.{written}`: a claim named with `-` or `:` is read whole only quoted -- \
                     quote it: `$jwt.\"{written}\"` (an operator after a claim is spaced: \
                     `$jwt.{name} - 1`)"
                ));
            }
            (name, len)
        }
    };
    if name.is_empty() {
        let why = "`$jwt.` names no claim: `$jwt.sub`, or `$jwt.\"cognito:groups\"` for a name \
                   of other characters";
        return Err(why.into());
    }
    let path = rest[len..]
        .strip_prefix('.')
        .is_some_and(|p| p.starts_with(|c: char| c.is_alphanumeric() || c == '_' || c == '"'));
    if path {
        let written: String = rest
            .chars()
            .take_while(|c| !c.is_whitespace() && !matches!(c, ')' | ',' | ']'))
            .collect();
        return Err(format!(
            "`$jwt.{written}`: a policy reads a token's top-level claims alone -- a nested \
             object is where a provider keeps what a user may edit of themselves, and a path \
             into it would let one grant themselves rows; have the provider sign the claim at \
             the top of the token"
        ));
    }
    Ok((name, len))
}

/// A policy line up to its comment: the first `#` outside double or single
/// quotes. Cut at the first `#` anywhere, `$jwt."https://example.com/#user"` lost
/// the rest of its line and its rule failed to parse, as did a filter's
/// `'#general'`; a quoted role (`for "Team #1"`) is read whole too.
fn uncommented(line: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match quote {
            // FenecQL's texts escape with `\`; a quoted claim name or role
            // holds none, and is refused where it does.
            Some(_) if std::mem::take(&mut escaped) => {}
            Some(_) if c == '\\' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '#' => return &line[..i],
            None if c == '"' || c == '\'' => quote = Some(c),
            None => {}
        }
    }
    line
}

/// A rule's `for <role>` and the rule before it: the last word `for`
/// standing alone, then the role -- a word, or any name in double quotes
/// (`for "Account Admin"`), which a provider's group names need. Found by
/// words, never by letters: cut at the last `for` in the line,
/// `for platform-admins` lost its role's own letters and
/// `for information-security` was refused at startup.
fn role_of(rest: &str) -> std::result::Result<(&str, Option<String>), String> {
    let t = rest.trim_end();
    // A filter may end in a quoted text as well: the quotes are a role only
    // where the word before them is `for`.
    let quoted = t
        .strip_suffix('"')
        .and_then(|s| s.rfind('"').map(|open| (&s[..open], &s[open + 1..], true)));
    let Some((before, role, quoted)) = quoted.or_else(|| {
        t.rsplit_once(char::is_whitespace)
            .map(|(b, r)| (b, r, false))
    }) else {
        return Ok((rest, None));
    };
    let Some(head) = before.trim_end().strip_suffix("for") else {
        return Ok((rest, None));
    };
    if !(head.is_empty() || head.ends_with(char::is_whitespace)) {
        return Ok((rest, None));
    }
    if quoted && (role.is_empty() || role.contains('\\')) {
        return Err(format!(
            "`for \"{role}\"` names no role: a quoted role is a name, taken as written"
        ));
    }
    Ok((head, Some(role.to_string())))
}

/// The collections the inner `get`s of a rule's filter read, at any depth,
/// each held to what an inner `get` is held to (`check_subquery`). A claim
/// goes in an inner `get`'s `where` alone, which [`bind`] binds: in a
/// `limit` or a `near` it would make every token's list another statement.
/// `new.` there would read the inner collection's own field of that name,
/// not the row a write leaves, and `expired()` its rows past their time,
/// which are the `expired` grant's: both refused.
fn inner_gets(e: &Expr, out: &mut Vec<String>) -> std::result::Result<(), String> {
    if let Expr::InSelect(_, sel) = e {
        sel.check_subquery()
            .map_err(|e| format!("the filter: {e}"))?;
        let mut rest = (**sel).clone();
        let filter = rest.filter.take();
        if rest.max_param() > 0 {
            return Err("a rule's inner `get` takes `$jwt.<claim>` in its `where` alone".into());
        }
        if let Some(f) = &filter {
            if f.asks_expired() {
                return Err("a rule's filter takes no `expired()`: grant `expired`".into());
            }
            if names_new(f) {
                return Err(format!(
                    "a rule's inner `get` reads `{}` as it stands: `new.` is the row a write \
                     leaves, outside it",
                    sel.collection
                ));
            }
            inner_gets(f, out)?;
        }
        if !out.contains(&sel.collection) {
            out.push(sel.collection.clone());
        }
    }
    let mut r = Ok(());
    e.each_child(&mut |c| {
        if r.is_ok() {
            r = inner_gets(c, out);
        }
    });
    r
}

/// The first ring of rules reading each other's collections through their
/// inner `get`s -- `positions` reading `orders`, whose rules read
/// `positions` -- at any depth and whatever their roles: a rule's inner
/// `get` is held to the token's rules on the collection it reads, which are
/// answered first, and around a ring none of them can be. A rule on `*`
/// that reads a collection is one by itself, being a rule of that
/// collection too.
fn ring(rules: &[Rule], lines: &[usize]) -> Option<String> {
    // The collections the rules of `c` read, each with its rule's line:
    // `c`'s own rules' and `*`'s.
    let reads = |c: &str| -> Vec<(&str, usize)> {
        rules
            .iter()
            .zip(lines)
            .filter(|(r, _)| r.collection.as_deref().is_none_or(|x| x == c))
            .flat_map(|(r, n)| r.reads.iter().map(move |d| (d.as_str(), *n)))
            .collect()
    };
    // Depth first from each collection, the path kept: one met again on it
    // closes a ring.
    fn walk<'a>(
        reads: &dyn Fn(&str) -> Vec<(&'a str, usize)>,
        path: &mut Vec<(&'a str, usize)>,
        done: &mut Vec<&'a str>,
    ) -> Option<Vec<(&'a str, usize)>> {
        let at = path.last()?.0;
        for (next, n) in reads(at) {
            if let Some(k) = path.iter().position(|(c, _)| *c == next) {
                let mut ring = path[k..].to_vec();
                ring.push((next, n));
                return Some(ring);
            }
            if done.contains(&next) {
                continue;
            }
            path.push((next, n));
            if let Some(ring) = walk(reads, path, done) {
                return Some(ring);
            }
            path.pop();
            done.push(next);
        }
        None
    }
    let mut done = Vec::new();
    let starts = rules
        .iter()
        .flat_map(|r| r.collection.iter().chain(&r.reads))
        .map(String::as_str);
    for start in starts {
        let Some(ring) = walk(&reads, &mut vec![(start, 0)], &mut done) else {
            continue;
        };
        let mut used: Vec<usize> = ring[1..].iter().map(|(_, n)| *n).collect();
        used.sort_unstable();
        used.dedup();
        let used: Vec<String> = used.iter().map(usize::to_string).collect();
        let names: Vec<String> = ring.iter().map(|(c, _)| format!("`{c}`")).collect();
        return Some(format!(
            "policy line{} {}: these rules read each other's collections, {}: a rule's inner \
             `get` is held to the token's rules on what it reads, answered first, and around a \
             ring none can be",
            if used.len() > 1 { "s" } else { "" },
            used.join(", "),
            names.join(" -> ")
        ));
    }
    None
}

/// What a rule's filter names the row as a write leaves it by: `new.`, as
/// an upsert's `else set` names the document put.
const NEW: &str = "new.";

/// Whether `e` reads the row after (`new.<field>`).
fn names_new(e: &Expr) -> bool {
    reads(e, &|name| name.starts_with(NEW))
}

/// Whether `e` reads a field `which` names, outside an inner `get` (a
/// rule holds none). A walk of its own rather than `referenced_fields`,
/// which copies every name it meets: it is asked of the read rules of
/// every `set ... returning` a token sends.
fn reads(e: &Expr, which: &dyn Fn(&str) -> bool) -> bool {
    if let Expr::Field(f) = e {
        return which(f);
    }
    let mut any = false;
    e.each_child(&mut |c| any = any || reads(c, which));
    any
}

/// `e` as a row that cannot answer every term of it answers it: each term
/// reading a field `unseen` names -- a transition's `new.status`, a field a
/// `set` is about to write -- taken as whatever lets the row through where
/// the term stands, true under an even number of `not`s and false under an
/// odd one, and the `and`s and `or`s it decides worked out. What is left
/// admits every row `e` admits, whatever those terms come to, so it picks
/// rows without leaving one out; the terms are asked once they can be, of
/// each row as it is written. `None` where nothing is left to ask. Taken
/// by value: a filter reading none of them is handed back as it came.
fn relaxed(e: Expr, unseen: &dyn Fn(&str) -> bool) -> Option<Expr> {
    if !reads(&e, unseen) {
        return Some(e);
    }
    match loosened(e, true, unseen) {
        Expr::Lit(Value::Bool(true)) => None,
        e => Some(e),
    }
}

fn loosened(e: Expr, pass: bool, unseen: &dyn Fn(&str) -> bool) -> Expr {
    let known = |e: &Expr| match e {
        Expr::Lit(Value::Bool(b)) => Some(*b),
        _ => None,
    };
    let is = |b: bool| Expr::Lit(Value::Bool(b));
    match e {
        Expr::And(a, b) => {
            let (a, b) = (loosened(*a, pass, unseen), loosened(*b, pass, unseen));
            match (known(&a), known(&b)) {
                (Some(false), _) | (_, Some(false)) => is(false),
                (Some(true), _) => b,
                (_, Some(true)) => a,
                _ => Expr::And(Box::new(a), Box::new(b)),
            }
        }
        Expr::Or(a, b) => {
            let (a, b) = (loosened(*a, pass, unseen), loosened(*b, pass, unseen));
            match (known(&a), known(&b)) {
                (Some(true), _) | (_, Some(true)) => is(true),
                (Some(false), _) => b,
                (_, Some(false)) => a,
                _ => Expr::Or(Box::new(a), Box::new(b)),
            }
        }
        Expr::Not(a) => {
            let a = loosened(*a, !pass, unseen);
            match known(&a) {
                Some(b) => is(!b),
                None => Expr::Not(Box::new(a)),
            }
        }
        e if reads(&e, unseen) => is(pass),
        e => e,
    }
}

/// The fields of a grant's list, `update(a, b)` or `read(a, b)`, its
/// parenthesis at `open`, pushed onto `list`.
fn field_list(g: &str, open: usize, list: &mut Vec<String>) -> std::result::Result<(), String> {
    for f in g[open..g.len() - 1].split(',') {
        let f = f.trim();
        let ok = !f.is_empty()
            && f.chars().all(|c| c.is_alphanumeric() || c == '_')
            && !f.starts_with(|c: char| c.is_ascii_digit());
        if !ok {
            let verb = &g[..open - 1];
            return Err(format!("`{g}`: a field list names fields, `{verb}(a, b)`"));
        }
        list.push(f.to_string());
    }
    Ok(())
}

/// The grants of a rule, split at the commas outside a field list.
fn split_grants(grants: &str) -> std::result::Result<Vec<&str>, String> {
    let (mut out, mut depth, mut from) = (Vec::new(), 0i32, 0);
    for (i, c) in grants.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(grants[from..i].trim());
                from = i + 1;
            }
            _ => {}
        }
        if !(0..=1).contains(&depth) {
            return Err(format!(
                "`{grants}`: a field list is one pair of parentheses"
            ));
        }
    }
    if depth != 0 {
        return Err(format!("`{grants}`: a field list is not closed"));
    }
    out.push(grants[from..].trim());
    Ok(out)
}

/// `e` with each parameter replaced by the claim it stands for.
fn bind(e: &Expr, values: &[Value]) -> Expr {
    bind_held(e, values, true)
}

/// [`bind`] of `e` standing under an even number of `not`s (`pass`) or an
/// odd one: an ordered comparison with a claim is held to the claim's kind
/// ([`kind_held`]) as `cmp and held` under the first, and as `cmp or (not
/// held and other is not null)` under the second, which the `not` above
/// turns into `not cmp and (held or other is null)` -- so a value of another
/// kind passes neither the comparison nor its negation, and a row with no
/// value passes the negation as it passes `not cmp` alone. ANDed in under a
/// `not` alone, the guard became an `or` there, and `not (score >
/// $jwt.max)` admitted every text and list; negated whole, it held out the
/// rows with no `level` that `not (level > $jwt.c)` admitted on main, and
/// the server token's `not (level > 5)` admits.
/// An inner `get`'s filter stands where its `in` does: under `not ... in`,
/// a row of another kind is in its list and so held out.
fn bind_held(e: &Expr, values: &[Value], pass: bool) -> Expr {
    let b = |x: &Expr| Box::new(bind_held(x, values, pass));
    match e {
        Expr::Param(i) => Expr::Lit(values[*i].clone()),
        Expr::Field(_) | Expr::Lit(_) => e.clone(),
        Expr::And(x, y) => Expr::And(b(x), b(y)),
        Expr::Or(x, y) => Expr::Or(b(x), b(y)),
        Expr::Not(x) => Expr::Not(Box::new(bind_held(x, values, !pass))),
        Expr::IsNull(x) => Expr::IsNull(b(x)),
        Expr::Cmp(op, x, y) => {
            let cmp = Expr::Cmp(*op, b(x), b(y));
            match kind_held(*op, x, y, values, pass) {
                Some(held) if pass => Expr::And(Box::new(cmp), Box::new(held)),
                Some(unheld) => Expr::Or(Box::new(cmp), Box::new(unheld)),
                None => cmp,
            }
        }
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
                    i => out.push(bind_held(i, values, pass)),
                }
            }
            Expr::In(b(x), out)
        }
        // An inner `get` holds its claims in its `where` alone ([`rule`]).
        Expr::InSelect(x, sel) => {
            let mut sel = (**sel).clone();
            sel.filter = sel.filter.as_ref().map(|f| bind_held(f, values, pass));
            Expr::InSelect(b(x), Box::new(sel))
        }
        Expr::Call(f, args) => Expr::Call(
            f.clone(),
            args.iter().map(|a| bind_held(a, values, pass)).collect(),
        ),
    }
}

/// The lower edge of a text claim that reads as a time for a time, itself
/// a time `time::parse` reads -- the first day of year 0 -- so that a time
/// field orders against such a claim (`at <= $jwt.until`,
/// `"2026-07-01T00:00:00Z"`) by time.
const EARLIEST: &str = "0000-01-01";

/// What holds the other side of `x op y` to the kind of the claim on one
/// side, where `op` orders (`<`, `<=`, `>`, `>=`): `None` where it does not,
/// or neither side is a claim. A filter orders two values of other kinds by
/// kind (`cmp_value`) -- every bool below every number, every number below
/// every text -- so `level <= $jwt.clearance` given the text `"top"`
/// admitted every row of an `int` field, and `name >= $jwt.from` given `5`
/// every row of a `text` one. Here a value of another kind is neither below
/// the claim nor above it: the other side is held to the edges of the
/// claim's kind, past which the other kinds rank -- `> true` and `< ""` for
/// a number, `<= true` (and `>= false`) for a bool, `>= ""` and `<` the
/// byte `0xFF` for a text: above every text, whose UTF-8 never holds that
/// byte, and below every kind ranked above texts, bytes ranking with them
/// (a vector literal, the next kind, is refused beside a json field).
///
/// A text claim that reads as a time is a time's kind as well, as
/// `cmp_value` compares a time with a text by reading it: its lower edge
/// is `>= ""` for a text and [`EARLIEST`] for a time, `other >= "" or
/// other >= "0000-01-01"`. A time and an int order alike against every
/// value but such a text, so no edge ANDed in says both: `>= "0000-01-01"`
/// alone held a text field's `""`, `"-1"` and `"0.9"` out of `day <=
/// "2026-01-01"`. The engine folds the `or` to the side the field's
/// declared type reads (`fenec_core::engine`'s `fold_kinds`), so an
/// ordered index still says it as a key.
///
/// Under an even number of `not`s (`held`) the edge on the side the
/// comparison leaves open is enough, and this is it; under an odd one
/// this is `not (both edges) and other is not null`, for the comparison
/// to be ORed with ([`bind_held`]). A typed field's edges are true or false
/// of every row, which `@sorted`'s range and an index over several fields
/// take as no bound or none (`fenec_core::sorted::by_kind`), so the range
/// stays exact. `=`, `!=` and `in` never take one kind for another
/// (`cmp_value` is not `Equal` across kinds) and ask nothing more.
fn kind_held(op: CmpOp, x: &Expr, y: &Expr, values: &[Value], held: bool) -> Option<Expr> {
    // `other op claim`, or `claim op other` turned around: `below` where
    // the other side is asked to be below the claim.
    let lower = matches!(op, CmpOp::Lt | CmpOp::Le);
    let upper = matches!(op, CmpOp::Gt | CmpOp::Ge);
    let (other, claim, below) = match (x, y) {
        _ if !lower && !upper => return None,
        (o, Expr::Param(k)) => (o, &values[*k], lower),
        (Expr::Param(k), o) => (o, &values[*k], upper),
        _ => return None,
    };
    let blank = || Value::Text(String::new());
    let (lo, hi) = match claim {
        Value::Int(_) | Value::Float(_) => ((CmpOp::Gt, Value::Bool(true)), (CmpOp::Lt, blank())),
        Value::Text(_) => ((CmpOp::Ge, blank()), (CmpOp::Lt, Value::Bytes(vec![0xFF]))),
        Value::Bool(_) => (
            (CmpOp::Ge, Value::Bool(false)),
            (CmpOp::Le, Value::Bool(true)),
        ),
        _ => return None,
    };
    let bound = || Box::new(bind(other, values));
    let edge = |(op, v): (CmpOp, Value)| Expr::Cmp(op, bound(), Box::new(Expr::Lit(v)));
    let low = || {
        let first = edge(lo.clone());
        match claim {
            Value::Text(t) if fenec_core::time::parse(t).is_ok() => {
                let time = edge((CmpOp::Ge, Value::Text(EARLIEST.into())));
                Expr::Or(Box::new(first), Box::new(time))
            }
            _ => first,
        }
    };
    match (held, below, claim) {
        (false, ..) => {
            let both = Expr::And(Box::new(low()), Box::new(edge(hi)));
            let present = Expr::Not(Box::new(Expr::IsNull(bound())));
            Some(Expr::And(
                Box::new(Expr::Not(Box::new(both))),
                Box::new(present),
            ))
        }
        // Nothing ranks below a bool but `null`, which orders against
        // nothing.
        (true, true, Value::Bool(_)) => None,
        (true, true, _) => Some(low()),
        (true, false, _) => Some(edge(hi)),
    }
}

/// A list of texts as JSON, `null` for none.
fn texts_into(out: &mut String, list: Option<&[String]>) {
    let Some(list) = list else {
        out.push_str("null");
        return;
    };
    out.push('[');
    for (i, t) in list.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        fenec_core::json::escape_into(out, t);
    }
    out.push(']');
}

fn and(filter: Option<Expr>, more: Option<Expr>) -> Option<Expr> {
    match (filter, more) {
        (Some(a), Some(b)) => Some(Expr::And(Box::new(a), Box::new(b))),
        (a, b) => a.or(b),
    }
}

/// `None`: no access. `Some(None)`: every row. `Some(Some(f))`: the rows
/// `f` matches -- the filters of the `rules` granting `op` on `collection`,
/// any of them, a transition's by what the row before can answer of it.
fn granted(rules: &[Bound], collection: &str, op: u8) -> Option<Option<Expr>> {
    let mut any = false;
    let mut every = false;
    let mut either: Option<Expr> = None;
    for r in rules {
        let here = r.collection.as_deref().is_none_or(|c| c == collection);
        if !here || r.ops & op == 0 {
            continue;
        }
        any = true;
        match r.using() {
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

/// Each inner `get` in `e` held to the read rules of `pos` on the
/// collection it reads, and the inner `get`s those bring held in turn
/// ([`Scope::held`]) -- or of `neg`, the same rules bound for a place under
/// an odd number of `not`s ([`bind_held`]), where the `get` stands so
/// (`pass` false): a `not ... in`'s list. Its rules' ordered comparisons
/// with a claim hold a row of another kind out of the list as a condition
/// standing alone, and the list held out fewer rows: `author not in (get
/// bans select author)` under `bans read where severity >= $jwt.min` read
/// a post by an author banned with a `"high"` severity, which main held
/// out. Bound for the `not`, the list keeps such a row.
fn hold(pos: &[Bound], neg: &[Bound], e: &mut Expr, pass: bool) {
    match e {
        Expr::Not(x) => hold(pos, neg, x, !pass),
        Expr::InSelect(_, sel) => {
            let rules = if pass { pos } else { neg };
            sel.filter = match granted(rules, &sel.collection, READ) {
                None => Some(Expr::Lit(Value::Bool(false))),
                Some(read) => and(sel.filter.take(), read),
            };
            if let Some(f) = &mut sel.filter {
                hold(pos, neg, f, pass);
            }
        }
        e => e.each_child_mut(&mut |c| hold(pos, neg, c, pass)),
    }
}

/// Each inner `get` in `e` answered against `db` as the `in` list it is,
/// the innermost first: one column's distinct values, nulls left out, as
/// the engine answers a statement's own -- but held to
/// [`MAX_CLAIM_VALUES`], a list claim's bound, since a rule's list is made
/// before every statement its token sends, and refused past it rather than
/// cut, which would hide rows the rule grants.
fn answer(db: &Database, e: &mut Expr) -> Result<()> {
    e.each_subquery_mut(&mut |x| {
        let Expr::InSelect(lhs, sel) = std::mem::replace(x, Expr::Lit(Value::Null)) else {
            unreachable!("each_subquery_mut hands an `in (get ...)`")
        };
        let mut sel = *sel;
        if let Some(f) = &mut sel.filter {
            answer(db, f)?;
        }
        *x = Expr::In(lhs, listed(db, sel)?);
        Ok(())
    })
}

/// The values an inner `get` with no inner `get` left lists, at most
/// [`MAX_CLAIM_VALUES`] of them. A plain one -- one field, no page, no
/// ranking -- is read up to the rows that could still be under the bound,
/// and only one that fills them read again grouped by its field, its rows
/// then its distinct values: a customer's few orders are a bucket's read,
/// where grouping every list took a `set` under such a rule 4.5 -> 5.1 us
/// (`make policy-bench`).
fn listed(db: &Database, sel: Select) -> Result<Vec<Expr>> {
    // Two past the bound tell that the list is past it: one may be a null.
    let past = MAX_CLAIM_VALUES + 2;
    let mut stmt = Statement::Select(sel);
    let Statement::Select(sel) = &mut stmt else {
        unreachable!("made a select")
    };
    let ranked = sel.near.is_some() || sel.matcher.is_some();
    let plain = !ranked
        && sel.aggregate.is_empty()
        && sel.group.is_empty()
        && sel.computed.is_empty()
        && sel.limit.is_none()
        && sel.offset.at(&[]) == 0;
    let column = match &sel.project {
        Some(p) if plain && p.len() == 1 => Some(p[0].clone()),
        _ => None,
    };
    if !ranked && (sel.aggregate.is_empty() || !sel.group.is_empty()) {
        sel.limit = Some(Count::N(sel.limit.map_or(past, |l| l.at(&[]).min(past))));
    }
    let mut rows = rows_of(db, &stmt)?;
    if let (Some(column), true) = (column, rows.len() >= past) {
        let Statement::Select(sel) = &mut stmt else {
            unreachable!("made a select")
        };
        sel.aggregate = vec![
            Column {
                expr: Expr::Field(column.clone()),
                name: column.clone(),
            },
            Column {
                expr: Expr::Call("count".into(), Vec::new()),
                name: "n".into(),
            },
        ];
        sel.project = None;
        sel.group = vec![Expr::Field(column)];
        // An `in` list has no order.
        sel.order.clear();
        rows = rows_of(db, &stmt)?;
    }
    distinct(rows).ok_or_else(|| {
        let Statement::Select(sel) = &stmt else {
            unreachable!("made a select")
        };
        Error::Query(format!(
            "a rule's `in (get {} ...)` lists more than {MAX_CLAIM_VALUES} values: a policy's \
             list is held to that many, as a list claim is, and refused past it rather than cut \
             -- narrow the inner `get`, or keep the membership in the rows themselves",
            sel.collection
        ))
    })
}

fn rows_of(db: &Database, stmt: &Statement) -> Result<Vec<Row>> {
    Ok(match db.query(stmt, &[])? {
        Response::Rows(rs) => rs.rows,
        _ => Vec::new(),
    })
}

/// The first column's distinct values of `rows`, nulls left out; `None`
/// past [`MAX_CLAIM_VALUES`].
fn distinct(rows: Vec<Row>) -> Option<Vec<Expr>> {
    let mut items: Vec<Expr> = Vec::with_capacity(rows.len());
    let mut keys: Vec<Vec<u8>> = Vec::with_capacity(rows.len());
    let mut key = Vec::new();
    for row in rows {
        let Some(v) = row.values.into_iter().next().filter(|v| !v.is_null()) else {
            continue;
        };
        key.clear();
        fenec_core::codec::encode_value(&mut key, &v);
        // Told apart by their encodings, as the engine's `in` lists are,
        // kept in order: a handful a list, a thousand at the most.
        match keys.binary_search(&key) {
            Ok(_) => continue,
            Err(at) => keys.insert(at, key.clone()),
        }
        items.push(Expr::Lit(v));
    }
    (items.len() <= MAX_CLAIM_VALUES).then_some(items)
}

/// The collections `stmt` reads or writes, each `lookup` level's and each
/// inner `get`'s among them: whose rules [`Scope::bound`] answers.
fn statement_reads<'a>(stmt: &'a Statement, out: &mut Vec<&'a str>) {
    match stmt {
        Statement::Select(s) | Statement::Explain(s) => select_reads(s, out),
        Statement::Put { collection, .. } => out.push(collection),
        Statement::Update {
            collection, filter, ..
        }
        | Statement::Delete {
            collection, filter, ..
        } => {
            out.push(collection);
            if let Some(f) = filter {
                expr_reads(f, out);
            }
        }
        _ => {}
    }
}

fn select_reads<'a>(s: &'a Select, out: &mut Vec<&'a str>) {
    out.push(&s.collection);
    if let Some(f) = &s.filter {
        expr_reads(f, out);
    }
    for l in s.lookup.iter().flat_map(|l| l.chain()) {
        out.push(&l.collection);
        if let Some(f) = &l.filter {
            expr_reads(f, out);
        }
    }
}

fn expr_reads<'a>(e: &'a Expr, out: &mut Vec<&'a str>) {
    if let Expr::InSelect(_, sel) = e {
        select_reads(sel, out);
    }
    e.each_child(&mut |c| expr_reads(c, out));
}

/// The collections the inner `get`s of `e` read, at any depth, and
/// whether one of them reads the time ([`Scope::reached_from`]).
fn reached(e: &Expr, reads: &mut Vec<String>, clock: &mut bool) {
    if let Expr::InSelect(_, sel) = e {
        if !reads.contains(&sel.collection) {
            reads.push(sel.collection.clone());
        }
        if let Some(f) = &sel.filter {
            *clock = *clock || f.calls_now();
            reached(f, reads, clock);
        }
    }
    e.each_child(&mut |c| reached(c, reads, clock));
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

    /// When the token's `exp` passes, in milliseconds since the epoch;
    /// `None` for a token that names none (`--jwt-require-exp off`). A
    /// request is checked as it comes, so one held open -- a subscription
    /// -- is ended here, or it outlived its token: a stream opened two
    /// seconds before `exp` still delivered a change written four after,
    /// while the token's own `get` was 401.
    pub fn expires_ms(&self) -> Option<u64> {
        self.expires
    }

    /// Whether the token may reach tenant `name` on a `--dir` node: its
    /// tenant claim names it, or it names none and the node takes unbound
    /// tokens. The tenant comes from the path and each tenant is a file of
    /// its own, but the policy is the node's: without this a token minted
    /// for one tenant read every other whose rows its filter matched.
    pub fn reaches(&self, name: &str) -> std::result::Result<(), &'static str> {
        match &self.tenants {
            Some(names) if names.iter().any(|n| n == name) => Ok(()),
            Some(names) if names.is_empty() => Err("this token's tenant claim names no tenant"),
            Some(_) => Err("this token is for another tenant"),
            None if self.unbound => Ok(()),
            None => Err("this token names no tenant, and this node serves tenants"),
        }
    }

    /// What the token may do, as `/_whoami` answers it: its `sub`, the
    /// roles its role claim holds, the tenants its claim names, and each
    /// rule that applies to it -- the
    /// collection, the grants, the fields an update may change, the fields
    /// a read shows and the rows, the filter as the policy writes it. A
    /// token is shown its own rules and no other's: a rule for a role it
    /// does not hold, or naming a claim it lacks, never became one of its
    /// own.
    pub fn summary_into(&self, out: &mut String) {
        use fenec_core::json::escape_into;
        out.push_str("\"sub\":");
        match &self.subject {
            Some(s) => escape_into(out, s),
            None => out.push_str("null"),
        }
        // The roles a rule's `for` was asked of, from the claim the server
        // reads them from: the studio showed the token's own `role`, which
        // under `--jwt-role-claim` is a claim like any other.
        out.push_str(",\"roles\":[");
        let (claims, name) = &self.roles;
        let held = claims
            .iter()
            .find(|(k, _)| k == name.as_ref())
            .map(|(_, v)| v);
        let roles: &[Value] = match held {
            Some(Value::List(l)) => l,
            Some(v @ Value::Text(_)) => std::slice::from_ref(v),
            _ => &[],
        };
        let mut first = true;
        for role in roles
            .iter()
            .filter_map(Value::as_text)
            .filter(|r| !r.is_empty())
        {
            if !std::mem::take(&mut first) {
                out.push(',');
            }
            escape_into(out, role);
        }
        out.push_str("],\"tenants\":");
        texts_into(out, self.tenants.as_deref());
        out.push_str(",\"unbound\":");
        out.push_str(if self.unbound { "true" } else { "false" });
        out.push_str(",\"append_only\":");
        texts_into(out, Some(&self.append_only));
        out.push_str(",\"rules\":[");
        for (i, r) in self.rules.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"collection\":");
            escape_into(out, r.collection.as_deref().unwrap_or("*"));
            out.push_str(",\"grants\":[");
            let names = [
                (READ, "read"),
                (INSERT, "insert"),
                (UPDATE, "update"),
                (DELETE, "delete"),
                (EXPIRED, "expired"),
            ];
            let granted = names.iter().filter(|(bit, _)| r.ops & bit != 0);
            for (k, (_, name)) in granted.enumerate() {
                if k > 0 {
                    out.push(',');
                }
                escape_into(out, name);
            }
            out.push_str("],\"fields\":");
            texts_into(out, r.fields.as_deref());
            out.push_str(",\"read_fields\":");
            texts_into(out, r.read_fields.as_deref());
            out.push_str(",\"rows\":");
            match &r.text {
                Some(t) => escape_into(out, t),
                None => out.push_str("null"),
            }
            out.push('}');
        }
        out.push(']');
    }

    /// Whether `collection` is `append-only` in the policy.
    fn append_only(&self, collection: &str) -> bool {
        self.append_only.iter().any(|a| a == "*" || a == collection)
    }

    /// `None`: no access. `Some(None)`: every row. `Some(Some(f))`: the rows
    /// `f` matches -- the filters of the rules granting `op`, any of them,
    /// a transition's by what the row before can answer of it.
    fn filter(&self, collection: &str, op: u8) -> Option<Option<Expr>> {
        if op & (UPDATE | DELETE) != 0 && self.append_only(collection) {
            return None;
        }
        granted(&self.rules, collection, op)
    }

    /// What [`Self::filter`] would answer, without making the filter:
    /// `None` no access, `Some(false)` every row, `Some(true)` rows a
    /// filter picks. Asked where only that is wanted: a `get` made the
    /// filter twice, once to AND it in and once to see that there was one,
    /// and a rule holding a claim to its kind (`kind_held`) copied two
    /// comparisons more each time.
    fn filtered(&self, collection: &str, op: u8) -> Option<bool> {
        if op & (UPDATE | DELETE) != 0 && self.append_only(collection) {
            return None;
        }
        let mut granting = self
            .rules
            .iter()
            .filter(|r| r.ops & op != 0 && r.collection.as_deref().is_none_or(|c| c == collection));
        let first = granting.next()?;
        Some(first.using().is_some() && granting.all(|r| r.using().is_some()))
    }

    /// Each reaching rule's inner `get`s held to the token's read rules on
    /// the collection each reads, at any depth -- the rules there may read
    /// further, which the ring check at startup bounds -- so that each is
    /// the `get` the token itself would send, answered under the lock that
    /// runs a statement ([`Self::bound`]). A collection no rule lets the
    /// token read gives none of its rows, as PostgreSQL's row security
    /// gives a policy's subquery none of a table no policy grants: refused
    /// as missing, as the token's own `get` of it is, it would fail every
    /// read of `positions` over a collection the token is not to know of.
    /// Done once a token, from the rules as bound to its claims, and as
    /// bound for an odd number of `not`s (`negated`, beside each), which a
    /// `get` under `not ... in` is held to ([`hold`]).
    fn held(rules: &mut [Bound], negated: Vec<Option<Expr>>) {
        let read: Vec<Bound> = rules.to_vec();
        let mut unread = read.clone();
        for (r, n) in unread.iter_mut().zip(negated) {
            if n.is_some() {
                r.filter = n;
            }
        }
        for r in rules.iter_mut().filter(|r| r.reaches) {
            if let Some(f) = &mut r.filter {
                hold(&read, &unread, f, true);
            }
            // A transition picks by what its row before can answer, its
            // inner `get`s held as the filter's are.
            if r.transition {
                r.before = r
                    .filter
                    .clone()
                    .and_then(|f| relaxed(f, &|name| name.starts_with(NEW)));
            }
        }
    }

    /// The token as `stmts` run over `db`, under the lock the caller holds
    /// on it: each rule of a collection the statements read or write that
    /// reads another collection, its inner `get`s answered there as the
    /// `in` lists they are ([`answer`]) -- so that a statement's filters
    /// hold lists the engine reads as any other, and a written document is
    /// tested by them where no query can run ([`Check`]), and a row is
    /// shown the fields the rules admitting it name ([`Self::shown`]),
    /// where unanswered a rule admits no row it is asked of alone. A read
    /// `/batch` is bound once for all its statements, under its one lock.
    /// Rules of other
    /// collections are left as they are: answered for every statement, a
    /// customer's rule over `positions` would read `orders` for each of
    /// their reads of `orders` itself. A `/batch` answers them for each
    /// statement, under the lock it holds through all of them: the one
    /// before may have changed what an inner `get` reads -- a claim, then
    /// the courier followed.
    pub fn bound(&self, db: &Database, stmts: &[&Statement]) -> Result<Scope> {
        let mut touched = Vec::new();
        for stmt in stmts {
            statement_reads(stmt, &mut touched);
        }
        self.bound_over(db, &touched)
    }

    /// [`Self::bound`] for the rules of `collections`.
    pub fn bound_over(&self, db: &Database, collections: &[&str]) -> Result<Scope> {
        let mut rules = Vec::with_capacity(self.rules.len());
        for r in &self.rules {
            let here = r.reaches
                && r.collection
                    .as_deref()
                    .is_some_and(|c| collections.contains(&c));
            if !here {
                rules.push(r.clone());
                continue;
            }
            let mut filter = r.filter.clone();
            if let Some(f) = &mut filter {
                answer(db, f)?;
            }
            // What a transition asks of the row before is its filter's,
            // read off the answer rather than answered again.
            let before = match r.transition {
                true => filter
                    .clone()
                    .and_then(|f| relaxed(f, &|name| name.starts_with(NEW))),
                false => None,
            };
            // Field by field: `..r.clone()` copied the filter once more.
            rules.push(Bound {
                collection: r.collection.clone(),
                ops: r.ops,
                fields: r.fields.clone(),
                read_fields: r.read_fields.clone(),
                filter,
                before,
                transition: r.transition,
                text: r.text.clone(),
                reaches: false,
            });
        }
        Ok(Scope {
            subject: self.subject.clone(),
            expires: self.expires,
            reaching: rules.iter().any(|r| r.reaches),
            rules,
            tenants: self.tenants.clone(),
            unbound: self.unbound,
            append_only: Arc::clone(&self.append_only),
            fielded: self.fielded,
            roles: (Arc::clone(&self.roles.0), Arc::clone(&self.roles.1)),
        })
    }

    /// What the token's rules of `collection` read beside it, at any depth,
    /// and whether one reads the time there -- what a stream of it binds
    /// them again on: a write to one of those collections, or every look.
    /// `None` where no rule of it reads another collection.
    pub fn reached_from(&self, collection: &str) -> Option<(Vec<String>, bool)> {
        let mut reads = Vec::new();
        let mut clock = false;
        for r in &self.rules {
            let here = r.reaches && r.collection.as_deref() == Some(collection);
            if let (true, Some(f)) = (here, &r.filter) {
                reached(f, &mut reads, &mut clock);
            }
        }
        (!reads.is_empty()).then_some((reads, clock))
    }

    /// Whether the token may read `collection` at all: a rule granting
    /// `read` there, its filter not made -- every scoped write asks.
    pub fn readable(&self, collection: &str) -> bool {
        self.rules
            .iter()
            .any(|r| r.ops & READ != 0 && r.collection.as_deref().is_none_or(|c| c == collection))
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
    ///
    /// `mask` takes what the answer's rows must not show, where a rule
    /// names the fields it shows; `None` for an inner `get`, whose column
    /// is a list rather than rows.
    fn select(
        &self,
        mut sel: Select,
        params: &[Value],
        mask: Option<&mut Option<fields::Mask>>,
    ) -> Result<Select> {
        // A SQL join's probe, the engine's reading of it once its list
        // passes the bound, hangs a level off the rows that the masks made
        // below for the list's one level would not reach: a token's join
        // is held to the list's bound.
        if let Some(f) = &mut sel.flat {
            f.probe = false;
        }
        // A SQL join's tables, each held to the read rules whether the
        // reading reads it or not: a LEFT JOIN's `on` the reading leaves
        // out makes no inner `get` for the rules to reach, and the engine's
        // check of the join's names told a token granted nothing on
        // `orders` its columns -- `o.dropoff` answered, `o.nosuch` refused
        // naming them -- where `orders` does not exist for it.
        if let Some(n) = sel.flat.as_ref().and_then(|f| f.names.as_deref()) {
            for (_, c) in &n.tables {
                if self.filtered(c, READ).is_none() {
                    return Err(self.refused(c, "read"));
                }
            }
        }
        // What a rule withholds is held out of what the statement does
        // with it before the rules are ANDed in, which read it as they must.
        if self.fielded {
            let m = fields::hold(self, &mut sel, params, mask.is_none())?;
            if let Some(mask) = mask {
                *mask = m;
            }
        }
        sel.each_filter_mut(&mut |collection, filter| {
            self.inner(filter, params)?;
            let f = self.reaping(collection, filter.take())?;
            *filter = self.restrict(collection, f)?;
            Ok(())
        })?;
        // A `match` held to some of the rows is scored over the rows its
        // filter selects, not the collection: BM25's statistics over every
        // row told a user how many rows it could not read held a word --
        // alice's own memo scored 9.87, and 4.79 once bob had written 200
        // private ones holding it.
        let held = self.filtered(&sel.collection, READ) == Some(true);
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
    fn inner(&self, filter: &mut Option<Expr>, params: &[Value]) -> Result<()> {
        let Some(f) = filter else {
            return Ok(());
        };
        f.each_subquery_mut(&mut |e| {
            if let Expr::InSelect(_, sel) = e {
                **sel = self.select(std::mem::take(&mut **sel), params, None)?;
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

    /// A write's filter, with the read rules ANDed in when it answers its
    /// rows (`returning`): what a write hands back is a read, as
    /// PostgreSQL's `RETURNING` is held to the `SELECT` policies.
    /// `writable` asked that a read rule exists; an update rule wider than
    /// the read one would otherwise answer rows the token cannot `get`.
    ///
    /// A `del` answers its rows as they were, so the read rules are asked
    /// of them as they stand. A `set` answers them as it wrote them, and a
    /// term of the read rules over a field it writes is asked of the row
    /// after, not before ([`Self::answerable`]): asked before, a courier --
    /// who reads the orders they carry -- claiming a ready order with
    /// `returning *` picked nothing and wrote nothing, where the same
    /// claim without it took the order.
    fn returning(
        &self,
        collection: &str,
        pick: &Option<Box<Pick>>,
        set: Option<&[(String, Expr)]>,
        f: Option<Expr>,
    ) -> Option<Expr> {
        if !pick.as_ref().is_some_and(|p| p.returning.is_some()) {
            return f;
        }
        let read = self.filter(collection, READ).flatten();
        match (set, read) {
            (Some(set), Some(read)) => and(f, Self::answerable(read, set)),
            (_, read) => and(f, read),
        }
    }

    /// The read rules `read` as a `set` writing `set` can be held to before
    /// it writes: each term over a field it assigns taken as passing
    /// ([`relaxed`]) -- the row as written answers those, through
    /// [`Check`]'s `answering`, and one it does not admit refuses the
    /// statement (403) rather than being written and left out of the
    /// answer. The terms over the fields it leaves alone are what they will
    /// be after, and pass over a row the token will not be able to read
    /// whatever the `set` writes, as a `get` passes it over.
    fn answerable(read: Expr, set: &[(String, Expr)]) -> Option<Expr> {
        // A path is its field's: `meta.owner` is written by `{meta: ...}`.
        fn field(name: &str) -> &str {
            name.split('.').next().unwrap_or(name)
        }
        relaxed(read, &|name| {
            set.iter().any(|(k, _)| field(k) == field(name))
        })
    }

    /// The statement as this token may run it, or why it may not.
    pub fn rewrite(&self, stmt: Statement) -> Result<Statement> {
        self.rewrite_with(stmt, &[], &mut None)
    }

    /// [`Self::rewrite`] with the statement's parameters, and in `mask`
    /// what its answer must not show ([`Self::shown`]): `None` where it
    /// shows all it holds -- a token none of whose rules name fields, a
    /// statement answering no rows, or only fields every row shows. The
    /// mask goes out beside the statement, not with it: a `Statement` is a
    /// kilobyte, and returned in a pair it was copied twice more, a scoped
    /// `set` in process 1 990 -> 2 060 ns with no field list anywhere.
    pub fn rewrite_with(
        &self,
        stmt: Statement,
        params: &[Value],
        mask: &mut Option<fields::Mask>,
    ) -> Result<Statement> {
        Ok(match stmt {
            Statement::Select(sel) => Statement::Select(self.select(sel, params, Some(mask))?),
            Statement::Explain(sel) => {
                Statement::Explain(self.select(sel, params, Some(&mut None))?)
            }
            Statement::Put {
                collection,
                mut docs,
                docs_param,
                insert,
                if_absent,
                else_set,
                require,
                mut pick,
            } => {
                // `scoped()` writes a parameter's documents in first, so
                // that each is held to the rules below.
                if docs_param.is_some() {
                    return Err(denied(
                        "a scoped token's documents are read before it writes them".into(),
                    ));
                }
                let f = self.writable(&collection, INSERT)?;
                // An upsert sets the row holding the value as `set` would:
                // the token updates as well as inserts, and the row it sets
                // is one it may update (`Check::before_overwrite`).
                if else_set.is_some() {
                    self.writable(&collection, UPDATE)?;
                }
                if self.fielded {
                    let set = (else_set.as_deref(), true);
                    *mask = fields::write(self, &collection, &mut None, set, &mut pick, params)?;
                }
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
                    docs_param,
                    insert,
                    if_absent,
                    else_set,
                    require,
                    pick,
                }
            }
            // `require` counts the rows the token's filter let it write, and
            // `order` and `limit` pick among those alone: a worker's token
            // claims only the jobs its rules let it update.
            Statement::Update {
                collection,
                set,
                mut filter,
                require,
                mut pick,
            } => {
                let f = self.writable(&collection, UPDATE)?;
                if self.fielded {
                    let s = (Some(&set[..]), false);
                    *mask = fields::write(self, &collection, &mut filter, s, &mut pick, params)?;
                }
                self.inner(&mut filter, params)?;
                let filter = self.reaping(&collection, filter)?;
                Statement::Update {
                    filter: self.returning(&collection, &pick, Some(&set), and(filter, f)),
                    collection,
                    set,
                    require,
                    pick,
                }
            }
            Statement::Delete {
                collection,
                mut filter,
                require,
                mut pick,
            } => {
                let f = self.writable(&collection, DELETE)?;
                if self.fielded {
                    let s = (None, false);
                    *mask = fields::write(self, &collection, &mut filter, s, &mut pick, params)?;
                }
                self.inner(&mut filter, params)?;
                let filter = self.reaping(&collection, filter)?;
                Statement::Delete {
                    filter: self.returning(&collection, &pick, None, and(filter, f)),
                    collection,
                    require,
                    pick,
                }
            }
            Statement::ListCollections => Statement::ListCollections,
            Statement::Describe(name) => {
                if !self.readable(&name) {
                    return Err(Error::NotFound(format!("collection `{name}`")));
                }
                Statement::Describe(name)
            }
            // No database holds a named statement yet: the engine lists
            // none and finds none, for any token.
            Statement::ListStatements => Statement::ListStatements,
            Statement::DescribeStatement(name) => Statement::DescribeStatement(name),
            // Nothing runs a call yet, nor could scope one: refused for a
            // scoped token as for the server's, before anything is asked
            // of its rules. A definition and a drop are schema changes,
            // refused below.
            Statement::Call(_) => return Err(Error::Query(fenec_core::named::NOT_BUILT.into())),
            _ => {
                return Err(denied(
                    "a scoped token reads and writes documents; it changes no schema".into(),
                ))
            }
        })
    }

    /// Whether a rule granting the token `update` in `collection` is a
    /// transition: there an update's row after is judged with its row
    /// before, by [`Self::overwrites`], and not on its own.
    fn transitions(&self, collection: &str) -> bool {
        self.rules.iter().any(|r| {
            r.transition
                && r.ops & UPDATE != 0
                && r.collection.as_deref().is_none_or(|c| c == collection)
        })
    }

    /// Whether the token may write `doc` over `old` in the collection
    /// `schema` describes, when a rule granting it `update` there names the
    /// fields it may change or is a transition: the fields that differ --
    /// a `set`'s, a `put`'s over an id, whatever route -- must all be one
    /// such rule's, a rule that also admits the change: a transition by
    /// being true of the row before and the row after together, any other
    /// by admitting the row before (`USING`) and after (`WITH CHECK`).
    /// Rules that name no fields reach every field, as before, so a policy
    /// with no field list and no transition judges nothing here: its rules
    /// widen each other, the row before one's and the row after another's,
    /// as PostgreSQL's permissive policies do. A transition never lends a
    /// half: true of the pair or not, it is not the before of one change
    /// and the after of another.
    fn overwrites(
        &self,
        schema: &Schema,
        old: &Document,
        doc: &Document,
        registry: &Registry,
    ) -> Result<()> {
        let name = schema.name.as_str();
        let here =
            |r: &&Bound| r.ops & UPDATE != 0 && r.collection.as_deref().is_none_or(|c| c == name);
        if !self
            .rules
            .iter()
            .filter(here)
            .any(|r| r.fields.is_some() || r.transition)
        {
            return Ok(());
        }
        // Compared as their encodings: `==` takes -0.0 for 0.0.
        let (mut a, mut b) = (Vec::new(), Vec::new());
        let mut changed = Vec::new();
        for (field, v) in &doc.fields {
            a.clear();
            b.clear();
            fenec_core::codec::encode_value(&mut a, v);
            fenec_core::codec::encode_value(&mut b, old.get(field).unwrap_or(&Value::Null));
            if a != b {
                changed.push(field.as_str());
            }
        }
        if changed.is_empty() {
            return Ok(());
        }
        let ctx = EvalCtx {
            params: &[],
            registry,
            clock: None,
        };
        let admits = |f: &Option<Expr>, d: &Document| -> Result<bool> {
            match f {
                None => Ok(true),
                Some(f) => Ok(truthy(&eval(f, &mut Written(d, schema), &ctx)?)),
            }
        };
        let covers = |r: &Bound| {
            r.fields
                .as_ref()
                .is_none_or(|fs| changed.iter().all(|c| fs.iter().any(|f| f == c)))
        };
        let (mut before, mut after) = (false, false);
        for r in self.rules.iter().filter(here).filter(|r| covers(r)) {
            if r.transition {
                // `new.` reads the row after, a bare name the row before.
                let pair = |f: &Expr| eval(f, &mut Pair(old, doc, schema), &ctx);
                if r.filter
                    .as_ref()
                    .map_or(Ok(true), |f| pair(f).map(|v| truthy(&v)))?
                {
                    return Ok(());
                }
                continue;
            }
            before = before || admits(&r.filter, old)?;
            after = after || admits(&r.filter, doc)?;
            if before && after {
                return Ok(());
            }
        }
        // Name a field no rule lets it change, where there is one.
        let granted = |c: &&str| {
            self.rules
                .iter()
                .filter(here)
                .any(|r| r.fields.as_ref().is_none_or(|fs| fs.iter().any(|f| f == c)))
        };
        Err(denied(match changed.iter().find(|c| !granted(c)) {
            Some(field) => format!("this token may not change `{field}` in `{name}`"),
            None => format!(
                "this token may not change {} of that row of `{name}`",
                changed
                    .iter()
                    .map(|c| format!("`{c}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }))
    }

    /// Whether `doc`, as `op` would write it to `collection`, is one this
    /// token may write: an insert's by the rules granting `insert`, an
    /// update's by those granting `update`.
    ///
    /// The rules are asked one after another, as the `or` of their filters
    /// [`Self::filter`] makes would be, without making it, which copied
    /// each filter a write, twice an update: with that copy and
    /// [`Self::readable`]'s, a scoped `set`'s rules cost 2.2-2.9 us over
    /// the server token's in process, without them 1.6-1.9 (`make
    /// policy-bench`, in turns).
    fn admits(&self, schema: &Schema, op: u8, doc: &Document, registry: &Registry) -> Result<bool> {
        let name = schema.name.as_str();
        if op & (UPDATE | DELETE) != 0 && self.append_only(name) {
            return Ok(false);
        }
        let granting = || {
            self.rules.iter().filter(move |r| {
                r.ops & op != 0 && r.collection.as_deref().is_none_or(|c| c == name)
            })
        };
        if granting().any(|r| r.using().is_none()) {
            return Ok(true);
        }
        let ctx = EvalCtx {
            params: &[],
            registry,
            clock: None,
        };
        for r in granting() {
            if let Some(f) = r.using() {
                if truthy(&eval(f, &mut Written(doc, schema), &ctx)?) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

/// A document being written, read as the filter reads a stored one: a
/// field in a collation compares in it here too, and a `nocase` one by its
/// fold, or a token could write a row it would not be shown -- `owner =
/// $jwt.email` holding `Alice@x.com` reads a row of `alice@x.com`, and so
/// writes one.
struct Written<'a>(&'a Document, &'a Schema);

impl RowAccess for Written<'_> {
    fn id(&self) -> DocId {
        self.0.id
    }
    fn field(&mut self, name: &str) -> Result<Value> {
        Ok(read(self.0, name))
    }
    fn text_order(&self, name: &str) -> fenec_core::collate::TextOrder {
        order_of(self.1, name)
    }
}

/// A field of `doc`, or a path into a json one, as a filter reads it off a
/// stored row: through [`Document::get`] alone, a rule's `meta.owner`
/// read `null` of every document written, and refused each.
fn read(doc: &Document, name: &str) -> Value {
    if name == "id" {
        return Value::Int(doc.id as i64);
    }
    doc.at(name).cloned().unwrap_or(Value::Null)
}

fn order_of(schema: &Schema, name: &str) -> fenec_core::collate::TextOrder {
    let field = name.split('.').next().unwrap_or(name);
    schema
        .field(field)
        .map_or(fenec_core::collate::TextOrder::BYTES, |f| f.text_order())
}

/// The row before a write and the row it leaves, as a transition reads
/// them: `new.<field>` the row after, a bare name the row before, each in
/// its field's collation as [`Written`] reads one.
struct Pair<'a>(&'a Document, &'a Document, &'a Schema);

impl RowAccess for Pair<'_> {
    fn id(&self) -> DocId {
        self.0.id
    }
    fn field(&mut self, name: &str) -> Result<Value> {
        Ok(match name.strip_prefix(NEW) {
            Some(after) => read(self.1, after),
            None => read(self.0, name),
        })
    }
    fn text_order(&self, name: &str) -> fenec_core::collate::TextOrder {
        order_of(self.2, name.strip_prefix(NEW).unwrap_or(name))
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
    /// It judges a document and changes nothing of it.
    fn rewrites(&self) -> bool {
        false
    }
    fn before_write(&self, schema: &Schema, op: WriteOp, doc: &mut Document) -> Result<()> {
        let Some(scope) = CURRENT.with(|c| c.borrow().clone()) else {
            return Ok(());
        };
        let op = match op {
            WriteOp::Insert => INSERT,
            // Where a transition grants it, the row after is judged with
            // the row before, which `before_overwrite` is handed next:
            // every update is a write over a row the collection holds.
            WriteOp::Update if scope.transitions(&schema.name) => return Ok(()),
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
    fn before_overwrite(&self, schema: &Schema, old: &Document, doc: &Document) -> Result<()> {
        let Some(scope) = CURRENT.with(|c| c.borrow().clone()) else {
            return Ok(());
        };
        // The row written over is one the token may update: a `set` found
        // it through the token's filter, and an upsert by the value it
        // holds, which is any row's -- PostgreSQL's `ON CONFLICT DO UPDATE`
        // asks the row the same of its `USING`.
        if !scope.admits(schema, UPDATE, old, &self.registry)? {
            return Err(denied(format!(
                "the row of `{}` holding that value is outside what this token may update",
                schema.name
            )));
        }
        scope.overwrites(schema, old, doc, &self.registry)
    }
    fn answering(&self, schema: &Schema, op: WriteOp, doc: &Document) -> Result<()> {
        // A delete's rows were picked through the read rules as they stood
        // ([`Scope::returning`]), and are answered so: asked again here, a
        // rule reading `now()` could find a row picked a moment before
        // past it.
        if op == WriteOp::Delete {
            return Ok(());
        }
        let Some(scope) = CURRENT.with(|c| c.borrow().clone()) else {
            return Ok(());
        };
        if scope.admits(schema, READ, doc, &self.registry)? {
            return Ok(());
        }
        // The token knows the row: it may write it, and wrote it so. What
        // it may read is its own rules, which `/_whoami` shows it.
        let rules: Vec<&str> = scope
            .rules
            .iter()
            .filter(|r| r.ops & READ != 0)
            .filter(|r| r.collection.as_deref().is_none_or(|c| c == schema.name))
            .filter_map(|r| r.text.as_deref())
            .collect();
        Err(denied(format!(
            "`returning` answers the rows this token may read, and the write leaves a row of \
             `{}` outside them (read where {}): nothing was written",
            schema.name,
            rules.join(" or ")
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

/// A check held between its signature and keeping its token, for the
/// test of a reload landing there: a token is held by its text, so no
/// other test's checks meet the gate.
#[cfg(test)]
mod held {
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    #[derive(Default)]
    pub struct Gate {
        /// Whether the check has arrived, and whether it may go on.
        state: Mutex<(bool, bool)>,
        changed: Condvar,
    }

    static HELD: Mutex<Vec<(String, Arc<Gate>)>> = Mutex::new(Vec::new());

    /// Checks of `token` stop at the gate until it is released.
    pub fn hold(token: &str) -> Arc<Gate> {
        let gate = Arc::new(Gate::default());
        HELD.lock()
            .unwrap()
            .push((token.to_string(), Arc::clone(&gate)));
        gate
    }

    pub fn at(token: &str) {
        let gate = HELD
            .lock()
            .unwrap()
            .iter()
            .find(|(t, _)| t == token)
            .map(|(_, g)| Arc::clone(g));
        if let Some(gate) = gate {
            gate.state.lock().unwrap().0 = true;
            gate.changed.notify_all();
            gate.wait(|s| s.1);
        }
    }

    impl Gate {
        /// Waits for what must happen, bounded only so a broken test fails
        /// rather than hangs.
        fn wait(&self, until: impl Fn(&(bool, bool)) -> bool) {
            let state = self.state.lock().unwrap();
            let (state, timeout) = self
                .changed
                .wait_timeout_while(state, Duration::from_secs(60), |s| !until(s))
                .unwrap();
            drop(state);
            assert!(!timeout.timed_out(), "the held check never moved");
        }

        pub fn arrived(&self) {
            self.wait(|s| s.0);
        }

        pub fn release(&self) {
            self.state.lock().unwrap().1 = true;
            self.changed.notify_all();
        }
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
            "only HS256, RS256 and ES256 tokens are accepted"
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

    /// A JWKS as an identity provider publishes it -- an EC key on P-256
    /// (tests/es256.rs has its tokens), an RSA key -- and tokens
    /// node:crypto signed with the RSA key's private half.
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

    /// A reload landing between a check's signature and its keeping the
    /// token: the token was kept after the reload emptied its shard, and a
    /// key taken out went on verifying it from there.
    #[test]
    fn a_token_checked_across_a_reload_is_not_kept_past_it() {
        let dir = std::env::temp_dir().join(format!("fenec-jwks-race-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keys.json");
        let oct = |kid: &str, byte: u8| {
            let k = b64url_encode(&[byte; 32]);
            format!(r#"{{"keys":[{{"kty":"oct","kid":"{kid}","k":"{k}"}}]}}"#)
        };
        std::fs::write(&path, oct("old", 1)).unwrap();
        let a = Arc::new(Access::from_jwks(&path, "").unwrap());
        let token = a.mint(r#"{"sub":"alice"}"#).unwrap();
        let now = unix_now();

        let gate = held::hold(&token);
        let checking = {
            let (a, token) = (Arc::clone(&a), token.clone());
            std::thread::spawn(move || a.scope(&token, now).map(|_| ()))
        };
        // Its signature checked against the old key, the check waits to
        // keep the token; the old key goes meanwhile.
        gate.arrived();
        std::fs::write(&path, oct("new", 2)).unwrap();
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(a.keys(now + 1)[0].kid.as_deref(), Some("new"));
        gate.release();
        // Answered under the keys it was checked against, and kept by
        // none: the next use meets the new keys.
        assert!(checking.join().unwrap().is_ok());
        assert_eq!(
            a.scope(&token, now + 1).err(),
            Some("no key has the token's kid")
        );
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

    /// What a transition asks of the row before, written out.
    #[test]
    fn a_transition_picks_by_what_the_row_before_can_answer() {
        let before = |filter: &str| {
            let r = rule(&format!("orders update where {filter}")).unwrap();
            assert!(r.transition, "{filter}");
            r.before.map(|e| format!("{e:?}"))
        };
        let get = |filter: &str| {
            let Statement::Select(s) =
                fenec_ql::parse_one(&format!("get _ where {filter}")).unwrap()
            else {
                unreachable!()
            };
            Some(format!("{:?}", s.filter.unwrap()))
        };
        assert_eq!(
            before(r#"status = "ready" and new.status = "cancelled""#),
            get(r#"status = "ready""#)
        );
        assert_eq!(
            before(r#"(status = "a" and new.status = "b") or (status = "c" and new.status = "d")"#),
            get(r#"status = "a" or status = "c""#)
        );
        assert_eq!(
            before(r#"not (status = "a" or new.status = "b") and owner = "u""#),
            get(r#"not (status = "a") and owner = "u""#)
        );
        // Nothing of the row before asked: every row.
        assert_eq!(before(r#"new.x = 1 or owner = "u""#), None);
        assert_eq!(before(r#"new.status = "x""#), None);
        assert_eq!(before(r#"not (new.status = "x")"#), None);
        // A term reading both rows is the pair's.
        assert_eq!(before("new.n = n + 1 and run_at <= 5"), get("run_at <= 5"));
        // A rule naming no `new.` is no transition.
        assert!(
            !rule(r#"orders update where status = "new.x""#)
                .unwrap()
                .transition
        );
    }

    /// A row the pair admits is never left out by what the row before
    /// answers: every filter of up to three levels over two terms of the
    /// row before, two of the row after and one of both, against every
    /// assignment of 0, 1 and null to the four fields.
    #[test]
    fn what_the_row_before_answers_admits_every_row_the_pair_does() {
        struct Row([Value; 4]);
        impl RowAccess for Row {
            fn id(&self) -> DocId {
                1
            }
            fn field(&mut self, name: &str) -> Result<Value> {
                let at = ["a", "b", "new.n", "new.m"].iter().position(|f| *f == name);
                Ok(self.0[at.unwrap()].clone())
            }
        }
        let terms: Vec<Expr> = ["a = 1", "b = 1", "new.n = 1", "new.m = 1", "a = new.m"]
            .iter()
            .map(|t| {
                let Statement::Select(s) =
                    fenec_ql::parse_one(&format!("get _ where {t}")).unwrap()
                else {
                    unreachable!()
                };
                s.filter.unwrap()
            })
            .collect();
        let not = |e: &Expr| Expr::Not(Box::new(e.clone()));
        let mut one: Vec<Expr> = terms.to_vec();
        one.extend(terms.iter().map(not));
        let mut two = Vec::new();
        for x in &one {
            for y in &one {
                two.push(Expr::And(Box::new(x.clone()), Box::new(y.clone())));
                two.push(Expr::Or(Box::new(x.clone()), Box::new(y.clone())));
            }
        }
        let negated: Vec<Expr> = two.iter().map(not).collect();
        two.extend(negated);
        let mut every = one.clone();
        every.extend(two.iter().cloned());
        for x in &two {
            for y in &terms {
                every.push(Expr::And(Box::new(x.clone()), Box::new(y.clone())));
                every.push(Expr::Or(Box::new(y.clone()), Box::new(x.clone())));
            }
        }
        let registry = Registry::with_builtins();
        let ctx = EvalCtx {
            params: &[],
            registry: &registry,
            clock: None,
        };
        let values = [Value::Int(0), Value::Int(1), Value::Null];
        for e in &every {
            let loose = relaxed(e.clone(), &|name| name.starts_with(NEW));
            for k in 0..81 {
                let v = |i: u32| values[(k / 3usize.pow(i)) % 3].clone();
                let mut row = Row([v(0), v(1), v(2), v(3)]);
                if !truthy(&eval(e, &mut row, &ctx).unwrap()) {
                    continue;
                }
                let admitted = match &loose {
                    None => true,
                    Some(l) => truthy(&eval(l, &mut row, &ctx).unwrap()),
                };
                assert!(admitted, "{e:?} as {loose:?} over {:?}", row.0);
            }
        }
    }

    /// An HS256 token for `claims` exactly as given: `mint` stamps what
    /// claims leave out.
    fn signed(claims: &str) -> String {
        let head = b64url_encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let body = b64url_encode(claims.as_bytes());
        let sig = hmac_sha256(SECRET, format!("{head}.{body}").as_bytes());
        format!("{head}.{body}.{}", b64url_encode(&sig))
    }

    /// Told issuers, audiences and clients, a token is taken only where it
    /// names one of each; told none, whatever it names, or not.
    #[test]
    fn whom_a_token_is_for_is_asked_where_told() {
        let told = |d: Demands| access("").demanding(d);
        let ask = |a: &Access, claims: &str| a.verify(&signed(claims), 1_000).map(|_| ());
        let iss = told(Demands {
            issuers: vec!["https://a.example/".into(), "https://b.example".into()],
            ..Demands::default()
        });
        assert_eq!(
            ask(&iss, r#"{"iss":"https://a.example/","exp":2000}"#),
            Ok(())
        );
        assert_eq!(
            ask(&iss, r#"{"iss":"https://b.example","exp":2000}"#),
            Ok(())
        );
        for wrong in [
            r#"{"iss":"https://a.example","exp":2000}"#,
            r#"{"iss":"https://c.example","exp":2000}"#,
            r#"{"iss":["https://a.example/"],"exp":2000}"#,
            r#"{"iss":null,"exp":2000}"#,
            r#"{"exp":2000}"#,
        ] {
            assert_eq!(ask(&iss, wrong), Err(NO_ISSUER), "{wrong}");
        }

        let aud = told(Demands {
            audiences: vec!["notes-api".into()],
            ..Demands::default()
        });
        assert_eq!(ask(&aud, r#"{"aud":"notes-api","exp":2000}"#), Ok(()));
        assert_eq!(
            ask(&aud, r#"{"aud":["other","notes-api"],"exp":2000}"#),
            Ok(())
        );
        for wrong in [
            r#"{"aud":"other","exp":2000}"#,
            r#"{"aud":["other"],"exp":2000}"#,
            r#"{"aud":[],"exp":2000}"#,
            r#"{"aud":{"notes-api":true},"exp":2000}"#,
            r#"{"exp":2000}"#,
        ] {
            assert_eq!(ask(&aud, wrong), Err(NO_AUDIENCE), "{wrong}");
        }

        // `azp` where the token names one, `client_id` (Cognito's access
        // tokens) where it names none.
        let client = told(Demands {
            clients: vec!["app-1".into()],
            ..Demands::default()
        });
        assert_eq!(ask(&client, r#"{"azp":"app-1","exp":2000}"#), Ok(()));
        assert_eq!(ask(&client, r#"{"client_id":"app-1","exp":2000}"#), Ok(()));
        for wrong in [
            r#"{"azp":"app-2","client_id":"app-1","exp":2000}"#,
            r#"{"client_id":"app-2","exp":2000}"#,
            r#"{"aud":"app-1","exp":2000}"#,
        ] {
            assert_eq!(ask(&client, wrong), Err(NO_CLIENT), "{wrong}");
        }

        // An empty text names nothing: a server told one takes no token.
        let empty = told(Demands {
            issuers: vec![String::new()],
            ..Demands::default()
        });
        assert_eq!(ask(&empty, r#"{"iss":"","exp":2000}"#), Err(NO_ISSUER));

        // Told none, nothing is asked of them.
        let open = access("");
        assert_eq!(
            ask(&open, r#"{"iss":5,"aud":{},"azp":[],"exp":2000}"#),
            Ok(())
        );

        // A token kept before the demands were is let go by them.
        let a = access("");
        let t = signed(r#"{"iss":"https://c.example","exp":2000}"#);
        assert!(a.verify(&t, 1_000).is_ok());
        let a = a.demanding(Demands {
            issuers: vec!["https://a.example/".into()],
            ..Demands::default()
        });
        assert_eq!(a.verify(&t, 1_000).unwrap_err(), NO_ISSUER);

        // A refusal names the check, never the value the token holds.
        for why in [NO_ISSUER, NO_AUDIENCE, NO_CLIENT] {
            assert!(!why.contains("example") && !why.contains("app-"), "{why}");
        }

        // A token minted here is one the server takes: the first of each
        // stamped where the claims name none, never over one they name.
        let all = told(Demands {
            issuers: vec!["https://a.example/".into()],
            audiences: vec!["notes-api".into(), "x".into()],
            clients: vec!["app-1".into()],
            ..Demands::default()
        });
        let minted = all.mint(r#"{"sub":"alice"}"#).unwrap();
        assert!(all.verify(&minted, unix_now()).is_ok());
        let body = minted.split('.').nth(1).unwrap();
        let body = String::from_utf8(b64url_decode(body).unwrap()).unwrap();
        assert!(
            body.contains(r#""iss":"https://a.example/""#)
                && body.contains(r#""aud":"notes-api""#)
                && body.contains(r#""azp":"app-1""#),
            "{body}"
        );
        let other = all.mint(r#"{"sub":"alice","aud":"y"}"#).unwrap();
        assert_eq!(all.verify(&other, unix_now()).unwrap_err(), NO_AUDIENCE);
        assert!(all.mint("{}").is_ok() && all.mint(" { } ").is_ok());
    }

    /// `for <role>` reads the claim the server names, a text or a list
    /// holding the role; `role` unless told.
    #[test]
    fn a_role_is_read_from_the_claim_named() {
        let policy = "notes read for editors\n";
        let reads = |a: &Access, claims: &str| {
            a.scope(&a.mint(claims).unwrap(), 0)
                .unwrap()
                .readable("notes")
        };
        let plain = access(policy);
        assert!(reads(&plain, r#"{"sub":"a","role":"editors"}"#));
        assert!(reads(&plain, r#"{"sub":"a","role":["x","editors"]}"#));
        assert!(!reads(
            &plain,
            r#"{"sub":"a","cognito:groups":["editors"]}"#
        ));
        let groups = access(policy).demanding(Demands {
            role_claim: "cognito:groups".into(),
            ..Demands::default()
        });
        assert!(reads(
            &groups,
            r#"{"sub":"a","cognito:groups":["editors"]}"#
        ));
        assert!(reads(&groups, r#"{"sub":"a","cognito:groups":"editors"}"#));
        // The claim named, and no other: `role` is a claim like any there.
        assert!(!reads(&groups, r#"{"sub":"a","role":"editors"}"#));
        assert!(!reads(
            &groups,
            r#"{"sub":"a","cognito:groups":["readers"]}"#
        ));
        // Neither a number nor an object compared as text.
        assert!(!reads(
            &groups,
            r#"{"sub":"a","cognito:groups":{"editors":1}}"#
        ));
    }

    /// `$jwt."<name>"` names a claim of any characters, top-level; a path
    /// into a nested one is refused at startup.
    #[test]
    fn a_quoted_claim_name_is_read_whole() {
        let a = access(
            "notes read where team in $jwt.\"cognito:groups\"\n\
             posts read,write where owner = $jwt.\"https://example.com/user\" and org = $jwt.org\n\
             docs read where owner = $jwt.\"sub\"\n",
        );
        let s = a
            .scope(
                &a.mint(
                    r#"{"sub":"alice","org":"o1","cognito:groups":["t1","t2"],"https://example.com/user":"u9"}"#,
                )
                .unwrap(),
                0,
            )
            .unwrap();
        let Some(Some(Expr::In(_, items))) = s.filter("notes", READ) else {
            panic!("an in list");
        };
        assert_eq!(
            items,
            vec![
                Expr::Lit(Value::Text("t1".into())),
                Expr::Lit(Value::Text("t2".into()))
            ]
        );
        let Some(Some(Expr::And(owner, _))) = s.filter("posts", INSERT) else {
            panic!("an and");
        };
        let Expr::Cmp(CmpOp::Eq, _, v) = *owner else {
            panic!("owner = ...");
        };
        assert_eq!(*v, Expr::Lit(Value::Text("u9".into())));
        let Some(Some(Expr::Cmp(CmpOp::Eq, _, v))) = s.filter("docs", READ) else {
            panic!("owner = ...");
        };
        assert_eq!(*v, Expr::Lit(Value::Text("alice".into())));
        // `$jwt."sub"` and `$jwt.sub` are one claim, one parameter.
        let r = rule(r#"notes read where owner = $jwt."sub" or editor = $jwt.sub"#).unwrap();
        assert_eq!(r.claims, vec!["sub".to_string()]);
        // A dot inside the quotes is part of the name.
        let r = rule(r#"notes read where owner = $jwt."a.b""#).unwrap();
        assert_eq!(r.claims, vec!["a.b".to_string()]);

        for (bad, says) in [
            ("notes read where x = $jwt.app_metadata.role", "top-level"),
            (
                r#"notes read where x = $jwt."app_metadata".role"#,
                "top-level",
            ),
            (
                r#"notes read where x = $jwt.app_metadata."role""#,
                "top-level",
            ),
            (
                r#"notes read where x in $jwt."https://x/".roles"#,
                "top-level",
            ),
            (r#"notes read where x = $jwt."open"#, "not closed"),
            (r#"notes read where x = $jwt."""#, "names no claim"),
            (r#"notes read where x = $jwt."a\"b""#, "no `\\`"),
            ("notes read where x = $jwt.", "names no claim"),
            ("notes read where x = $jwt.'sub'", "names no claim"),
        ] {
            let e = Access::new(SECRET, bad).err().unwrap_or_default();
            assert!(e.contains(says), "{bad}: {e}");
        }
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

    /// A claim held as `null`, `""`, an object, a list where one value is
    /// read or a list holding any of those is a claim the token lacks: the
    /// rule naming it does not apply. Where a rule reads a claim as a list
    /// alone -- an `in`'s list, the list `has` looks in -- a list is a
    /// value, every value in it one.
    #[test]
    fn a_claim_no_rule_can_compare_is_no_claim() {
        let a = access(
            "notes read where owner = $jwt.sub\n\
             rooms read where id in [$jwt.rooms, 0]\n\
             posts read where $jwt.rooms has room\n\
             mixed read where room in $jwt.rooms or owner = $jwt.rooms\n\
             teams read where team in (get members select team where user = $jwt.sub)\n\
             members read where user = $jwt.sub\n",
        );
        let lists = |name: &str| {
            let r = a
                .rules
                .iter()
                .find(|r| r.collection.as_deref() == Some(name));
            r.unwrap().lists.clone()
        };
        assert_eq!(lists("notes"), [false]);
        assert_eq!(lists("rooms"), [true]);
        assert_eq!(lists("posts"), [true]);
        assert_eq!(lists("mixed"), [false]);
        assert_eq!(lists("teams"), [false]);

        let readable = |claims: &str| {
            let s = a.scope(&a.mint(claims).unwrap(), 0).unwrap();
            ["notes", "rooms", "posts", "mixed", "teams"]
                .into_iter()
                .filter(|c| s.readable(c))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            readable(r#"{"sub":"a","rooms":["r1",2]}"#),
            ["notes", "rooms", "posts", "teams"]
        );
        assert_eq!(readable(r#"{"sub":"a","rooms":"r1"}"#).len(), 5);
        assert_eq!(readable(r#"{"sub":7,"rooms":[]}"#).len(), 4);
        assert_eq!(readable(r#"{"sub":true,"rooms":1.5}"#).len(), 5);
        for nobody in [
            r#"{"sub":null,"rooms":null}"#,
            r#"{"sub":"","rooms":""}"#,
            r#"{"sub":{"id":"a"},"rooms":{"r":1}}"#,
            r#"{"sub":["a"],"rooms":["r1",null]}"#,
            r#"{"sub":[],"rooms":["r1",""]}"#,
            r#"{"sub":[null],"rooms":[["r1"]]}"#,
            r#"{"rooms":["r1",{"r":1}]}"#,
        ] {
            assert!(readable(nobody).is_empty(), "{nobody}");
        }
        // A parameter of the policy's own stood for whichever claim the
        // rewrite numbered so -- `$3` after three claims for the third --
        // and after fewer for nothing, panicking every request.
        for bad in [
            "notes read where owner = $3",
            "notes read where owner = $jwt.sub or x = $2",
            "notes read where a = $jwt.a or b = $jwt.b or c = $jwt.c or d = $3",
            "notes read where title = \"$1\"",
        ] {
            let why = Access::new(SECRET, bad).err().expect(bad);
            assert!(why.contains("by claim, `$jwt.<claim>`"), "{bad}: {why}");
        }
    }

    /// `""` is no one: a subject of `""` kept its Idempotency-Keys under
    /// the server token's. A tenant claim present but naming no tenant
    /// reaches none, even where a token without the claim reaches every one.
    #[test]
    fn an_empty_subject_is_none_and_a_tenant_claim_naming_none_reaches_none() {
        let a = access("").demanding(Demands {
            unbound_tenants: true,
            ..Demands::default()
        });
        let scope = |claims: &str| a.scope(&a.mint(claims).unwrap(), 0).unwrap();
        assert_eq!(scope(r#"{"sub":""}"#).subject(), None);
        assert_eq!(scope(r#"{"sub":5}"#).subject(), None);
        assert_eq!(scope(r#"{"sub":"a"}"#).subject(), Some("a"));
        assert_eq!(scope(r#"{"sub":"a"}"#).reaches("acme"), Ok(()));
        assert_eq!(scope(r#"{"tenant":["acme",""]}"#).reaches("acme"), Ok(()));
        assert_eq!(
            scope(r#"{"tenant":"acme"}"#).reaches("globex"),
            Err("this token is for another tenant")
        );
        for none in ["null", "\"\"", "0", "false", "{}", "[]", "[null, \"\", 3]"] {
            assert_eq!(
                scope(&format!(r#"{{"tenant":{none}}}"#)).reaches("acme"),
                Err("this token's tenant claim names no tenant"),
                "{none}"
            );
        }
    }

    /// An ordered comparison with a claim holds the other side to the
    /// claim's kind: under an even number of `not`s on the side the
    /// comparison leaves open, under an odd one -- `cmp or (not held and
    /// other is not null)` -- by both edges. A text that reads as a time
    /// takes a text's lower edge or a time's; `=` asks nothing more.
    #[test]
    fn an_ordered_claim_is_held_to_its_kind() {
        let a = access(
            "files read where level <= $jwt.c\n\
             people read where $jwt.c < name\n\
             notes read where owner = $jwt.c\n\
             negated read where not (level > $jwt.c)\n",
        );
        let filter = |c: &str, collection: &str| {
            let s = a
                .scope(&a.mint(&format!(r#"{{"c":{c}}}"#)).unwrap(), 0)
                .unwrap();
            s.filter(collection, READ).unwrap().unwrap()
        };
        let lit = |v: Value| Box::new(Expr::Lit(v));
        let edge = |op, v| Expr::Cmp(op, Box::new(Expr::Field("level".into())), lit(v));
        let held = |e: Expr| match e {
            Expr::And(_, held) => Some(*held),
            Expr::Cmp(..) => None,
            e => panic!("{e:?}"),
        };
        let blank = || Value::Text(String::new());
        assert_eq!(
            held(filter("5", "files")),
            Some(edge(CmpOp::Gt, Value::Bool(true)))
        );
        assert_eq!(
            held(filter("\"top\"", "files")),
            Some(edge(CmpOp::Ge, blank()))
        );
        assert_eq!(
            held(filter("\"2026-07-01T00:00:00Z\"", "files")),
            Some(Expr::Or(
                Box::new(edge(CmpOp::Ge, blank())),
                Box::new(edge(CmpOp::Ge, Value::Text(EARLIEST.into())))
            ))
        );
        assert_eq!(held(filter("true", "files")), None);
        let name = |op, v| Expr::Cmp(op, Box::new(Expr::Field("name".into())), lit(v));
        assert_eq!(held(filter("5", "people")), Some(name(CmpOp::Lt, blank())));
        assert_eq!(
            held(filter("\"b\"", "people")),
            Some(name(CmpOp::Lt, Value::Bytes(vec![0xFF])))
        );
        assert_eq!(
            held(filter("false", "people")),
            Some(name(CmpOp::Le, Value::Bool(true)))
        );
        assert_eq!(held(filter("5", "notes")), None);
        // Under a `not`: both edges, negated beside the comparison.
        let Expr::Not(inner) = filter("5", "negated") else {
            panic!("a not");
        };
        let both = Expr::And(
            Box::new(edge(CmpOp::Gt, Value::Bool(true))),
            Box::new(edge(CmpOp::Lt, blank())),
        );
        let cmp = Expr::Cmp(
            CmpOp::Gt,
            Box::new(Expr::Field("level".into())),
            lit(Value::Int(5)),
        );
        let present = Expr::Not(Box::new(Expr::IsNull(Box::new(Expr::Field(
            "level".into(),
        )))));
        assert_eq!(
            *inner,
            Expr::Or(
                Box::new(cmp),
                Box::new(Expr::And(
                    Box::new(Expr::Not(Box::new(both))),
                    Box::new(present)
                ))
            )
        );
    }

    /// A claim whose kind a rule cannot hold -- inside a call or a sum, or
    /// compared in order inside a value -- stops the server at startup;
    /// one compared bare, under `and`, `or`, `not` or in an inner `get`'s
    /// condition, is taken.
    #[test]
    fn a_claim_held_nowhere_is_refused_at_startup() {
        for bad in [
            "files read where level <= lower($jwt.c)",
            "files read where coalesce($jwt.c, 0) >= level",
            "files read where least(level, $jwt.c) = level",
            "files read where owner = upper($jwt.sub)",
            "files read where level < $jwt.c * 2",
            "files read where (level <= $jwt.c) = true",
            "files read where ((level <= $jwt.c) and x) = true",
            "files read where (level <= $jwt.c) is null",
            "files read where case when level <= $jwt.c then 1 else 0 end = 1",
            "files read where id in (get other select x where y = lower($jwt.c))",
            // `like`'s pattern is exempt as the claim it is, not a call
            // over one; the text matched is no claim.
            "files read where path like lower($jwt.home)",
            "files read where $jwt.home like path",
        ] {
            assert!(Access::new(SECRET, bad).is_err(), "{bad}");
        }
        for good in [
            "files read where level <= $jwt.c or not (level > $jwt.d)",
            "files read where (level = $jwt.c) = true",
            "files read where id in (get other select x where not (y > $jwt.c))",
            "files read where room in [$jwt.rooms, 'lobby'] and $jwt.rooms has room",
            "files read where level <= length(name)",
            "files read where path like $jwt.home",
            "files read where email ilike $jwt.domain",
            "files read where path not like $jwt.home escape $jwt.esc",
            "files read where id in (get other select x where name like $jwt.p)",
        ] {
            assert!(Access::new(SECRET, good).is_ok(), "{good}");
        }
    }

    /// A token for another app as an adversary would send one: a member
    /// given twice, an audience or issuer differing by case, Unicode form,
    /// a slash or white space, an `azp` that is no text beside a matching
    /// `client_id` -- each refused; and no spelling of a policy reaches a
    /// member of an object claim.
    #[test]
    fn whom_a_token_is_for_is_not_told_by_near_spellings() {
        let told = |d: Demands| access("").demanding(d);
        let ask = |a: &Access, claims: &str| a.verify(&signed(claims), 1_000).map(|_| ());
        let aud = told(Demands {
            audiences: vec!["notes-api".into()],
            issuers: vec!["https://idp.example/".into()],
            clients: vec!["app-1".into()],
            ..Demands::default()
        });
        let base = r#""iss":"https://idp.example/","azp":"app-1","exp":2000"#;
        // A member given twice is no token: neither the first nor the last
        // is believed.
        assert!(ask(
            &aud,
            &format!(r#"{{"aud":"other","aud":"notes-api",{base}}}"#)
        )
        .is_err());
        assert!(ask(
            &aud,
            &format!(r#"{{"aud":"notes-api","aud":"other",{base}}}"#)
        )
        .is_err());
        // Case and Unicode form are not folded; a trailing slash is not
        // trimmed; nor is white space.
        for wrong in ["Notes-api", "notes-api/", " notes-api", "notes\u{2010}api"] {
            assert!(
                ask(&aud, &format!(r#"{{"aud":"{wrong}",{base}}}"#)).is_err(),
                "{wrong}"
            );
        }
        assert!(ask(
            &aud,
            r#"{"aud":"notes-api","iss":"HTTPS://idp.example/","azp":"app-1","exp":2000}"#
        )
        .is_err());
        // A list holding the audience beside values of other kinds is
        // taken (lenient: RFC 7519 says a list of texts).
        assert!(ask(&aud, &format!(r#"{{"aud":[null,5,"notes-api"],{base}}}"#)).is_ok());
        // `azp` held as null, a list or "" is judged as `azp`: a matching
        // `client_id` beside it does not stand in for it.
        for azp in ["null", r#"["app-1"]"#, r#""""#, r#"{"app-1":1}"#] {
            let claims = format!(
                r#"{{"aud":"notes-api","iss":"https://idp.example/","azp":{azp},"client_id":"app-1","exp":2000}}"#
            );
            assert_eq!(ask(&aud, &claims), Err(NO_CLIENT), "{claims}");
        }

        // `$jwt."a.b"` reads the top-level claim of that name, never the
        // member `b` of an object `a`.
        let a = access(r#"notes read where owner = $jwt."a.b""#);
        let nested = a
            .scope(&a.mint(r#"{"a":{"b":"alice"}}"#).unwrap(), 0)
            .unwrap();
        assert!(!nested.readable("notes"));
        let flat = a.scope(&a.mint(r#"{"a.b":"alice"}"#).unwrap(), 0).unwrap();
        assert!(flat.readable("notes"));
        // Nor can FenecQL reach a member of an object claim another way.
        for bad in [
            "notes read where owner = $jwt.a.b",
            r#"notes read where owner = $jwt."a"."b""#,
            r#"notes read where owner = $jwt."a".b"#,
            r#"notes read where owner = $jwt.a."b""#,
            "notes read where owner = $jwt.a.0",
            "notes read where owner = $jwt.a->>'b'",
            "notes read where owner = $jwt.a['b']",
            // `json_extract($jwt.a, 'b')` parses, and is an unknown
            // function at the first row: no builtin reads an object member.
        ] {
            if let Ok(a) = Access::new(SECRET, bad) {
                let s = a.scope(&a.mint(r#"{"a":{"b":"alice"}}"#).unwrap(), 0);
                if let Ok(s) = s {
                    let f = s.filter("notes", READ);
                    panic!("{bad}: parsed, filter {f:?}");
                }
            }
        }
    }

    /// A role is the word after the last `for` standing alone, or a quoted
    /// name: a provider's group `platform-admins` or
    /// `information-security` holds the letters `for`, and was cut at
    /// them -- refused at startup, or read as another role.
    #[test]
    fn a_role_holding_for_is_read_whole() {
        for (line, want) in [
            ("notes read for platform-admins", "platform-admins"),
            (
                "notes read where owner = $jwt.sub for platform-admins",
                "platform-admins",
            ),
            (
                "notes read for information-security",
                "information-security",
            ),
            ("notes read for for", "for"),
            (r#"notes read for "Account Admin""#, "Account Admin"),
            (
                r#"notes read where x = "y" for "Team #1"  # a comment"#,
                "Team #1",
            ),
            ("notes read\twhere owner = $jwt.sub\tfor\tops", "ops"),
        ] {
            let r = rule(uncommented(line).trim());
            assert!(r.is_ok(), "{line}: {:?}", r.err());
            assert_eq!(r.unwrap().role.as_deref(), Some(want), "{line}");
        }
        // A filter ending in a quoted text has no role, nor does a word
        // ending in `for`.
        for line in [
            r#"notes read where title = "for""#,
            r#"notes read where title = "x for""#,
            "notes read where owner = $jwt.sub or x = platformfor",
        ] {
            let r = rule(line);
            assert_eq!(r.ok().and_then(|r| r.role), None, "{line}");
        }
        let a = access(r#"notes read for "Account Admin""#);
        let reads = |claims: &str| {
            a.scope(&a.mint(claims).unwrap(), 0)
                .unwrap()
                .readable("notes")
        };
        assert!(reads(r#"{"role":["Account Admin"]}"#));
        assert!(!reads(r#"{"role":"Account"}"#));
        assert!(Access::new(SECRET, r#"notes read for """#).is_err());
    }

    /// A `#` inside quotes is no comment: a quoted claim holding a URL's
    /// fragment, and a filter's text holding one, are read whole.
    #[test]
    fn a_quoted_claim_holding_a_hash_is_read_whole() {
        let a = Access::new(
            SECRET,
            "notes read where owner = $jwt.\"https://example.com/#user\"  # the user\n\
             chans read where name = '#general' # every member's\n",
        );
        let a = a.unwrap_or_else(|e| panic!("{e}"));
        let s = a
            .scope(
                &a.mint(r##"{"https://example.com/#user":"u1"}"##).unwrap(),
                0,
            )
            .unwrap();
        let Some(Some(Expr::Cmp(CmpOp::Eq, _, v))) = s.filter("notes", READ) else {
            panic!("owner = ...");
        };
        assert_eq!(*v, Expr::Lit(Value::Text("u1".into())));
        let Some(Some(Expr::Cmp(CmpOp::Eq, _, v))) = s.filter("chans", READ) else {
            panic!("name = ...");
        };
        assert_eq!(*v, Expr::Lit(Value::Text("#general".into())));
        assert_eq!(uncommented("a # b"), "a ");
        assert_eq!(uncommented(r##"a "x\"#y" # z"##), r##"a "x\"#y" "##);
    }

    /// `--jwt-tenant-claim "$CLAIM"` with CLAIM unset was taken at the
    /// start, and with `--jwt-unbound-tenants` a token naming tenant `acme`
    /// reached every tenant: no token holds a claim named "". The flag is
    /// refused empty; a library's `Demands` holding one binds no token.
    #[test]
    fn an_empty_tenant_claim_binds_no_token() {
        let a = access("").demanding(Demands {
            tenant_claim: String::new(),
            unbound_tenants: true,
            ..Demands::default()
        });
        let s = a
            .scope(&a.mint(r#"{"sub":"a","tenant":"acme"}"#).unwrap(), 0)
            .unwrap();
        assert!(s.reaches("beta").is_err(), "a token for acme reached beta");
        assert!(s.reaches("acme").is_err());
    }

    /// A claim named with `-` or `:`, unquoted, ran on as arithmetic or
    /// failed as FenecQL: refused, saying to quote it.
    #[test]
    fn an_unquoted_claim_running_into_a_dash_is_refused() {
        for (bad, quoted) in [
            (
                "notes read where owner = $jwt.x-hasura-user-id",
                r#"$jwt."x-hasura-user-id""#,
            ),
            (
                "notes read where team in $jwt.cognito:groups",
                r#"$jwt."cognito:groups""#,
            ),
            ("notes read where n = $jwt.n-1", r#"$jwt."n-1""#),
        ] {
            let e = Access::new(SECRET, bad).err().unwrap_or_default();
            assert!(e.contains(&format!("quote it: `{quoted}`")), "{bad}: {e}");
        }
        // Spaced, the dash is a minus after the claim `n` -- read so, and
        // refused as a claim in arithmetic is (`claims_held`), not as a
        // claim to quote.
        let e = Access::new(SECRET, "notes read where n = $jwt.n - 1")
            .err()
            .unwrap_or_default();
        assert!(e.contains("`$jwt.n` in arithmetic"), "{e}");
        assert!(Access::new(SECRET, "notes read where n - 1 = $jwt.n").is_ok());
        assert!(Access::new(SECRET, r#"notes read where x = $jwt."x-hasura-user-id""#).is_ok());
    }

    /// `/_whoami` names the roles the server read, from the claim it reads
    /// them from: under `--jwt-role-claim`, `role` is no role.
    #[test]
    fn whoami_names_the_roles_read_from_the_role_claim() {
        let roles = |a: &Access, claims: &str| {
            let s = a.scope(&a.mint(claims).unwrap(), 0).unwrap();
            let mut out = String::new();
            s.summary_into(&mut out);
            let at = out.find("\"roles\":").unwrap() + 8;
            out[at..at + out[at..].find(']').unwrap() + 1].to_string()
        };
        let plain = access("");
        assert_eq!(roles(&plain, r#"{"role":"ops"}"#), r#"["ops"]"#);
        assert_eq!(roles(&plain, r#"{"role":["a",5,"","b"]}"#), r#"["a","b"]"#);
        assert_eq!(roles(&plain, r#"{"sub":"x"}"#), "[]");
        let groups = access("").demanding(Demands {
            role_claim: "cognito:groups".into(),
            ..Demands::default()
        });
        assert_eq!(
            roles(&groups, r#"{"role":"ops","cognito:groups":["editors"]}"#),
            r#"["editors"]"#
        );
        assert_eq!(roles(&groups, r#"{"role":"ops"}"#), "[]");
    }

    /// Under `--jwt-max-age` below an hour, a token `mint` signs lives that
    /// long: an hour's was refused by the server that minted it.
    #[test]
    fn a_minted_token_lives_no_longer_than_the_server_takes() {
        let a = access("").demanding(Demands {
            max_age: Some(600),
            ..Demands::default()
        });
        let now = unix_now();
        let t = a.mint(r#"{"sub":"a"}"#).unwrap();
        assert!(a.verify(&t, now).is_ok());
        assert!(a.verify(&t, now + 601).is_err());
        let long = access("").demanding(Demands {
            max_age: Some(86_400),
            ..Demands::default()
        });
        let t = long.mint(r#"{"sub":"a"}"#).unwrap();
        assert!(
            long.verify(&t, now + MINTED_FOR + 1).is_err(),
            "an hour still"
        );
    }
}
