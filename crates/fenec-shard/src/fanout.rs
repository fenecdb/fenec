//! `POST /_shard/query`: one read run on every tenant -- or those a list
//! or a prefix names -- and the answers merged as the request says. The
//! router's one request that reaches more than one tenant.
//!
//! ```text
//! POST /_shard/query
//! {"query": "get users select email, created order created desc limit 20",
//!  "merge": {"order": ["created desc"], "limit": 20}}
//! -> {"rows": [{"tenant":"acme","email":..,"created":..}, ..],
//!     "tenants": 100, "answered": 100, "failed": [], "ms": 9}
//! ```
//!
//! **The router's token alone.** Tenants are the isolation boundary, and
//! this crosses it, so it is a `/_shard/` endpoint: a JSON Web Token -- the
//! router holds no keys, and the node binds one to the tenant it names --
//! and the nodes' data token are refused (401) like any other token than
//! the router's. Each tenant is asked through its node's
//! `POST /_admin/tenants/<t>/query` with the admin token the router holds
//! already: a token that can take a tenant's whole image learns nothing
//! more by reading it, where the data token would reach every tenant and
//! write. The node refuses a statement that writes (403) and runs the rest
//! in read-only mode besides.
//!
//! **The router still parses no FenecQL.** The statement and its
//! parameters go to every node as the caller wrote them, and the merge is
//! the request's own JSON: what to do with the rows is said, never read
//! out of the query. Three merges, each exact from the tenants' answers
//! alone:
//!
//! * none: the rows one after another, tenants in name order, each row
//!   with a `tenant` column (`"column"` renames it) first;
//! * `{"order": ["created desc", "id"], "limit": 20}`: a k-way merge of
//!   rows each tenant gives in that order -- checked, a tenant whose rows
//!   are not is a failure, never merged wrong -- its own `order ... limit
//!   20` the top 20 of all of them. Ties go in tenant order;
//! * `{"sum": ["n"], "min": [..], "max": [..], "group": ["day"]}`: the
//!   aggregates that add across tenants, folded by the group columns. A
//!   count is a sum of counts; an average is not a merge of averages, so
//!   there is none (fold its sum and its count). Every column of a row is
//!   a group column or folded, or the merge is refused: a column it would
//!   drop is an answer cut.
//!
//! **Never cut.** A tenant that failed, timed out or could not be read is
//! named, with its node, status and why; by default the whole query fails
//! with them (the first failure stops the asking), and only with
//! `"partial": true` are the others' rows answered beside the failures. A
//! tenant moved while the query ran is asked again on the node the
//! directory names now; one deleted meanwhile is a failure. The rows and
//! the bytes gathered are bounded (`--fanout-max-rows`,
//! `--fanout-max-bytes`) and a query past either is refused (413), never
//! answered with what fitted.
//!
//! **Consistency is a tenant's.** Each tenant's answer is one statement's,
//! as `/query`'s is; tenants are separate databases read at different
//! moments, so two answers are of no one moment, and a write landing in
//! one while the query runs may or may not be in its answer.
//!
//! **Mechanics.** The tenants come from the directory as the query
//! begins. Calls go out over the pool, `--fanout-concurrency` at once,
//! the tenants taken node by node in turn so the calls spread over the
//! nodes, and the whole query has one timeout (`--fanout-timeout`, or the
//! request's `timeout_ms` below it) that bounds every connect and read.

use crate::metrics::{self, Fanned};
use crate::{quote, Router};
use fenec_core::prelude::*;
use fenec_http::http::{Request, Response};
use fenec_http::trace;
use std::cmp::Ordering as Order;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What one query across tenants may gather and how long it may take, and
/// how many calls it has out at once.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Rows gathered from the tenants together, before the merge: past
    /// it the query is refused.
    pub max_rows: usize,
    /// Bytes of the tenants' answers together, refused past it before a
    /// byte more is read.
    pub max_bytes: usize,
    /// Calls to the nodes out at once.
    pub concurrency: usize,
    /// The longest a query runs, every call within it; a request may ask
    /// for less.
    pub timeout: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_rows: 100_000,
            max_bytes: 64 << 20,
            concurrency: 16,
            timeout: Duration::from_secs(30),
        }
    }
}

/// A node's error body read past this is no error message: it is left
/// unread, and the connection with it.
const ERROR_BODY: usize = 64 << 10;

/// How many nodes a tenant is asked on while the directory keeps moving
/// it: a move, and a failover after it, and no more.
const ASKS: usize = 3;

/// The request, read.
struct Ask {
    /// What each tenant's node is sent: `{"query": .., "params": ..}`,
    /// both the caller's text as it came.
    body: Vec<u8>,
    pick: Pick,
    merge: Merge,
    /// The name of the column a row's tenant goes in.
    column: String,
    partial: bool,
    timeout: Duration,
}

enum Pick {
    All,
    Named(Vec<String>),
    Prefix(String),
}

enum Merge {
    Concat,
    /// Columns and whether each is descending, and how many rows.
    Order {
        keys: Vec<(String, bool)>,
        limit: Option<usize>,
    },
    Fold(Fold),
}

struct Fold {
    group: Vec<String>,
    sum: Vec<String>,
    min: Vec<String>,
    max: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Group,
    Sum,
    Min,
    Max,
}

impl Fold {
    fn role(&self, column: &str) -> Option<Role> {
        let has = |l: &[String]| l.iter().any(|c| c == column);
        match () {
            _ if has(&self.group) => Some(Role::Group),
            _ if has(&self.sum) => Some(Role::Sum),
            _ if has(&self.min) => Some(Role::Min),
            _ if has(&self.max) => Some(Role::Max),
            _ => None,
        }
    }

    fn named(&self) -> impl Iterator<Item = &String> {
        self.group
            .iter()
            .chain(&self.sum)
            .chain(&self.min)
            .chain(&self.max)
    }
}

/// A tenant to ask, as the directory placed it when the query began.
struct Target {
    tenant: String,
    node: String,
    /// The node's address and admin token; `None` for a placement on a
    /// node the directory no longer has.
    at: Option<(String, String)>,
}

/// What asking one tenant came to.
enum Got {
    /// The node's answer.
    Rows(Vec<u8>),
    Failed(Failure),
    /// Not asked: the query had failed already, or passed a bound.
    Unasked,
}

struct Failure {
    tenant: String,
    node: String,
    status: u16,
    why: String,
}

/// What the workers share.
struct Shared {
    next: AtomicUsize,
    /// The asking stops: a failure without `partial`, or a bound passed.
    stop: AtomicBool,
    bytes: AtomicUsize,
    /// The bound passed, in words, where one was.
    over: Mutex<Option<String>>,
    /// Each node's calls: asked, failed, and when the last was answered.
    nodes: Mutex<HashMap<String, (u64, u64, Instant)>>,
}

/// Whether `req` is a query across tenants: `POST /_shard/query`.
pub(crate) fn asked(req: &Request) -> bool {
    req.method == fenec_http::http::Method::Post && req.segments() == ["_shard", "query"]
}

impl Router {
    /// `POST /_shard/query`, past the router's token.
    pub(crate) fn fanout(&self, req: &Request) -> Response {
        let arrived = Instant::now();
        let refused = |resp: Response| {
            metrics::fanned(Fanned::Refused, arrived.elapsed(), 0, 0);
            resp
        };
        let limits = &self.cfg.fanout;
        let ask = match read_ask(&req.body, limits) {
            Ok(a) => a,
            Err(why) => return refused(Response::error(400, &why)),
        };
        let targets = match self.targets(&ask.pick) {
            Ok(t) => t,
            Err(resp) => return refused(resp),
        };
        let deadline = arrived + ask.timeout;
        if trace::recording() {
            trace::attr("fenec.tenants", targets.len() as i64);
        }
        let fanning = trace::span("fanout");
        // A span a node, begun now under the fan-out's, which each call to
        // that node names as its parent: the node's own spans hang under
        // it. Ended once the node's last tenant answered.
        let mut names: Vec<&str> = targets.iter().map(|t| t.node.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        let spans: Vec<(String, trace::Remote)> = names
            .iter()
            .filter_map(|n| trace::remote().map(|r| (n.to_string(), r)))
            .collect();
        let shared = Shared {
            next: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            bytes: AtomicUsize::new(0),
            over: Mutex::new(None),
            nodes: Mutex::new(HashMap::new()),
        };
        let got = self.gather(&targets, &ask, deadline, &shared, &spans);
        let nodes = std::mem::take(&mut *shared.nodes.lock().unwrap_or_else(|e| e.into_inner()));
        for (node, span) in spans {
            let Some(&(asked, failed, last)) = nodes.get(&node) else {
                continue;
            };
            let addr = self
                .read_dir()
                .node(&node)
                .map(|n| n.addr.clone())
                .unwrap_or_default();
            let attrs = vec![
                ("fenec.node", trace::Val::from(node)),
                ("server.address", trace::Val::from(addr)),
                ("fenec.tenants", trace::Val::from(asked as i64)),
                ("fenec.failed", trace::Val::from(failed as i64)),
            ];
            let error = (failed > 0).then(|| format!("{failed} of {asked} tenants failed"));
            span.end_at("node", trace::Kind::Client, last, attrs, error);
        }

        let over = shared.over.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(bound) = over {
            fanning.error(bound.clone());
            return refused(Response::error(413, &bound));
        }
        let (mut answers, mut failures, mut unasked) = (Vec::new(), Vec::new(), 0usize);
        for (t, g) in targets.iter().zip(got) {
            match g {
                Got::Rows(body) => answers.push((t.tenant.as_str(), body)),
                Got::Failed(f) => failures.push(f),
                Got::Unasked => unasked += 1,
            }
        }
        let asked = targets.len();
        let answer = match failures.is_empty() || ask.partial {
            true => merged(&ask, limits, answers, &mut failures),
            false => Ok(None),
        };
        let rows = match answer {
            Err(Refusal(status, why)) => {
                fanning.error(why.clone());
                return refused(Response::error(status, &why));
            }
            Ok(rows) => rows,
        };
        let failed = failures.len() as u64;
        let answered = (asked - failures.len() - unasked) as u64;
        if trace::recording() {
            fanning.attr("fenec.answered", answered as i64);
            fanning.attr("fenec.failed", failed as i64);
        }
        let ms = arrived.elapsed().as_millis();
        let rows = match rows {
            Some(rows) if failures.is_empty() || ask.partial => rows,
            _ => {
                // A tenant failed and no partial answer was asked for: the
                // query fails, naming every tenant that did, and how many
                // were never asked once the first had.
                fanning.error(format!("{failed} tenants failed"));
                drop(fanning);
                metrics::fanned(Fanned::Failed, arrived.elapsed(), answered, failed);
                let first = &failures[0];
                let mut body = String::from("{\"error\":");
                let why = format!(
                    "{failed} of {asked} tenants failed, the first `{}` on `{}`: {} \
                     (send \"partial\": true for the others' rows)",
                    first.tenant, first.node, first.why
                );
                fenec_core::json::escape_into(&mut body, &why);
                body.push_str(&format!(",\"tenants\":{asked},\"failed\":"));
                failures_into(&mut body, &failures);
                body.push_str(&format!(",\"not_asked\":{unasked},\"ms\":{ms}}}"));
                return Response::json(status_of(&failures), body);
            }
        };
        drop(fanning);
        let how = match failures.is_empty() {
            true => Fanned::Done,
            false => Fanned::Partial,
        };
        metrics::fanned(how, arrived.elapsed(), answered, failed);
        let mut body = String::with_capacity(rows.len() + 128);
        body.push_str("{\"rows\":");
        body.push_str(&rows);
        body.push_str(&format!(
            ",\"tenants\":{asked},\"answered\":{answered},\"failed\":"
        ));
        failures_into(&mut body, &failures);
        body.push_str(&format!(",\"ms\":{ms}}}"));
        Response::json(200, body)
    }

    /// The tenants to ask, in name order, as the directory places them now.
    fn targets(&self, pick: &Pick) -> std::result::Result<Vec<Target>, Response> {
        let dir = self.read_dir();
        // A standby whose maps have not come from the primary yet knows of
        // no tenant, which is not that there is none.
        if !dir.arrived() {
            return Err(Response::retry_after(
                503,
                "the directory has not arrived from the primary yet",
                1,
            ));
        }
        let placed = match pick {
            Pick::All => dir.tenants(),
            Pick::Prefix(p) => dir
                .tenants()
                .into_iter()
                .filter(|(t, _)| t.starts_with(p.as_str()))
                .collect(),
            Pick::Named(names) => {
                let missing: Vec<String> = names
                    .iter()
                    .filter(|n| dir.placement(n).is_none())
                    .map(|n| format!("`{n}`"))
                    .collect();
                if !missing.is_empty() {
                    return Err(Response::error(
                        404,
                        &format!("no tenant {} in the directory", missing.join(", ")),
                    ));
                }
                let mut names = names.clone();
                names.sort_unstable();
                names.dedup();
                names
                    .into_iter()
                    .filter_map(|n| dir.placement(&n).cloned().map(|p| (n, p)))
                    .collect()
            }
        };
        Ok(placed
            .into_iter()
            .map(|(tenant, p)| Target {
                at: dir.node(&p.node).map(|n| (n.addr.clone(), n.token.clone())),
                node: p.node,
                tenant,
            })
            .collect())
    }

    /// Asks every target, `concurrency` at once: what each came to, in the
    /// targets' order.
    fn gather(
        &self,
        targets: &[Target],
        ask: &Ask,
        deadline: Instant,
        shared: &Shared,
        spans: &[(String, trace::Remote)],
    ) -> Vec<Got> {
        if targets.is_empty() {
            return Vec::new();
        }
        // Node by node in turn, so the calls out at once spread over the
        // nodes rather than wait on one of them.
        let mut by_node: Vec<(&str, Vec<usize>)> = Vec::new();
        for (i, t) in targets.iter().enumerate() {
            match by_node.iter_mut().find(|(n, _)| *n == t.node) {
                Some((_, list)) => list.push(i),
                None => by_node.push((&t.node, vec![i])),
            }
        }
        let mut order = Vec::with_capacity(targets.len());
        for round in 0.. {
            let before = order.len();
            for (_, list) in &by_node {
                if let Some(&i) = list.get(round) {
                    order.push(i);
                }
            }
            if order.len() == before {
                break;
            }
        }
        // The request's id and each node's trace context go out with every
        // call, as a forward's do: the workers' threads serve no request.
        let id = fenec_http::request_id::current();
        let context: Vec<(&str, String, Option<&str>)> = spans
            .iter()
            .map(|(n, r)| (n.as_str(), r.traceparent(), r.tracestate()))
            .collect();
        let work = || {
            let mut mine = Vec::new();
            while !shared.stop.load(Ordering::Relaxed) {
                let k = shared.next.fetch_add(1, Ordering::Relaxed);
                let Some(&i) = order.get(k) else { break };
                let got = self.ask(&targets[i], ask, deadline, shared, &id, &context);
                if matches!(got, Got::Failed(_)) && !ask.partial {
                    shared.stop.store(true, Ordering::Relaxed);
                }
                mine.push((i, got));
            }
            mine
        };
        let mut got: Vec<Option<Got>> = (0..targets.len()).map(|_| None).collect();
        let workers = self.cfg.fanout.concurrency.clamp(1, targets.len().max(1));
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..workers)
                .filter_map(|_| {
                    std::thread::Builder::new()
                        .name("fenec-fanout".into())
                        .spawn_scoped(s, work)
                        .ok()
                })
                .collect();
            // No thread to be had: asked from this one.
            if handles.is_empty() {
                for (i, g) in work() {
                    got[i] = Some(g);
                }
            }
            for h in handles {
                if let Ok(mine) = h.join() {
                    for (i, g) in mine {
                        got[i] = Some(g);
                    }
                }
            }
        });
        // A tenant with nothing to show was never taken -- the asking had
        // stopped -- or its worker panicked mid-call, which is named as
        // the failure it is rather than left out.
        let stopped = shared.stop.load(Ordering::Relaxed);
        got.into_iter()
            .zip(targets)
            .map(|(g, t)| match g {
                Some(g) => g,
                None if stopped => Got::Unasked,
                None => Got::Failed(Failure {
                    tenant: t.tenant.clone(),
                    node: t.node.clone(),
                    status: 500,
                    why: "the router's call to it failed".into(),
                }),
            })
            .collect()
    }

    /// Asks one tenant, on the node the directory names: again on the
    /// node it names now where a call failed and the tenant has moved
    /// meanwhile -- a move deletes the copy it left -- and failed where it
    /// has not, or is gone.
    fn ask(
        &self,
        t: &Target,
        ask: &Ask,
        deadline: Instant,
        shared: &Shared,
        id: &str,
        context: &[(&str, String, Option<&str>)],
    ) -> Got {
        let target = format!("/_admin/tenants/{}/query", t.tenant);
        let (mut node, mut at) = (t.node.clone(), t.at.clone());
        for _ in 0..ASKS {
            let called = match &at {
                None => Err((502, format!("no node `{node}` in the directory"))),
                Some((addr, token)) => {
                    let headers = Headers::new(token, id, context, &node);
                    self.call(addr, &node, &target, &headers, ask, deadline, shared)
                }
            };
            let (status, why) = match called {
                Ok(Some(body)) => return Got::Rows(body),
                Ok(None) => return Got::Unasked,
                Err(failed) => failed,
            };
            let now = {
                let dir = self.read_dir();
                dir.placement(&t.tenant).map(|p| {
                    let at = dir.node(&p.node).map(|n| (n.addr.clone(), n.token.clone()));
                    (p.node.clone(), at)
                })
            };
            match now {
                None => {
                    return Got::Failed(Failure {
                        tenant: t.tenant.clone(),
                        node,
                        status: 404,
                        why: format!("deleted from the directory while the query ran ({why})"),
                    })
                }
                Some((n, a)) if n != node => (node, at) = (n, a),
                Some(_) => {
                    return Got::Failed(Failure {
                        tenant: t.tenant.clone(),
                        node,
                        status,
                        why,
                    })
                }
            }
        }
        Got::Failed(Failure {
            tenant: t.tenant.clone(),
            node,
            status: 503,
            why: format!("moved {ASKS} times while the query ran"),
        })
    }

    /// One call to a node: the answer's body, `None` where the bytes bound
    /// was passed (and the asking stopped), or a failure's status and why.
    #[allow(clippy::too_many_arguments)]
    fn call(
        &self,
        addr: &str,
        node: &str,
        target: &str,
        headers: &Headers,
        ask: &Ask,
        deadline: Instant,
        shared: &Shared,
    ) -> std::result::Result<Option<Vec<u8>>, (u16, String)> {
        let noted = |failed: bool| {
            let mut nodes = shared.nodes.lock().unwrap_or_else(|e| e.into_inner());
            let e = nodes
                .entry(node.to_string())
                .or_insert((0, 0, Instant::now()));
            e.0 += 1;
            e.1 += failed as u64;
            e.2 = Instant::now();
        };
        let io = |e: std::io::Error| {
            noted(true);
            match e.kind() {
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => (
                    504,
                    format!("node `{node}` did not answer within the query's time"),
                ),
                _ => (502, format!("node `{node}` ({addr}) did not answer: {e}")),
            }
        };
        let answer = self
            .pool
            .send_by(addr, target, &headers.list(), &ask.body, deadline)
            .map_err(io)?;
        let status = answer.status;
        let length = answer.content_length().unwrap_or(0);
        if status == 200 {
            // The bound before a byte of the body is read: what is past it
            // is never gathered, and its connection is let go of.
            let gathered = shared.bytes.fetch_add(length, Ordering::Relaxed) + length;
            if gathered > self.cfg.fanout.max_bytes {
                noted(false);
                over(
                    shared,
                    format!(
                        "the query gathered more than {} bytes of answers from its tenants \
                         (--fanout-max-bytes): narrow it, give each tenant's statement a \
                         limit, or ask fewer tenants",
                        self.cfg.fanout.max_bytes
                    ),
                );
                return Ok(None);
            }
        } else if length > ERROR_BODY {
            noted(true);
            return Err((
                status,
                format!("node `{node}` answered {status}, with {length} bytes"),
            ));
        }
        let body = answer.read_body_by(&self.pool, deadline).map_err(io)?;
        noted(status != 200);
        match status {
            200 => Ok(Some(body)),
            // The router's admin token, not the caller's, was refused: the
            // directory holds a token the node no longer takes.
            401 => Err((
                502,
                format!("node `{node}` refused the router's admin token"),
            )),
            _ => Err((status, message_of(&body))),
        }
    }
}

/// The headers of a call to a node.
struct Headers {
    bearer: String,
    id: String,
    parent: Option<String>,
    state: Option<String>,
}

impl Headers {
    fn new(token: &str, id: &str, context: &[(&str, String, Option<&str>)], node: &str) -> Headers {
        let mine = context.iter().find(|(n, _, _)| *n == node);
        Headers {
            bearer: format!("Bearer {token}"),
            id: id.to_string(),
            parent: mine.map(|(_, p, _)| p.clone()),
            state: mine.and_then(|(_, _, s)| s.map(str::to_string)),
        }
    }

    fn list(&self) -> Vec<(&str, &str)> {
        let mut h = vec![
            ("Authorization", self.bearer.as_str()),
            ("Content-Type", "application/json"),
        ];
        if !self.id.is_empty() {
            h.push((fenec_http::request_id::HEADER, self.id.as_str()));
        }
        if let Some(p) = &self.parent {
            h.push((trace::TRACEPARENT, p.as_str()));
        }
        if let Some(s) = &self.state {
            h.push((trace::TRACESTATE, s.as_str()));
        }
        h
    }
}

/// Stops the asking for a bound passed, the first one named.
fn over(shared: &Shared, why: String) {
    shared.stop.store(true, Ordering::Relaxed);
    let mut over = shared.over.lock().unwrap_or_else(|e| e.into_inner());
    if over.is_none() {
        *over = Some(why);
    }
}

/// What a node said in an error's body: its `error`, or the text.
fn message_of(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    if let Ok(members) = fenec_core::json::parse_object(&text) {
        if let Some((_, Value::Text(e))) = members.into_iter().find(|(k, _)| k == "error") {
            return e;
        }
    }
    let t = text.trim();
    match t.char_indices().nth(300) {
        Some((at, _)) => format!("{}...", &t[..at]),
        None => t.to_string(),
    }
}

/// The status a failed query answers with: the tenants' own where they
/// all failed alike -- a statement every node refused is the request's
/// fault, 400 or 403 -- and 502 otherwise.
fn status_of(failures: &[Failure]) -> u16 {
    let first = failures[0].status;
    match failures.iter().all(|f| f.status == first) {
        true if first >= 400 => first,
        _ => 502,
    }
}

fn failures_into(out: &mut String, failures: &[Failure]) {
    out.push('[');
    for (i, f) in failures.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"tenant\":");
        out.push_str(&quote(&f.tenant));
        out.push_str(",\"node\":");
        out.push_str(&quote(&f.node));
        out.push_str(&format!(",\"status\":{},\"error\":", f.status));
        out.push_str(&quote(&f.why));
        out.push('}');
    }
    out.push(']');
}

// ------------------------------------------------------------ the request

/// The request's body read: the members it may hold, each once, and
/// nothing else -- a member misspelt would otherwise be a merge left out.
fn read_ask(body: &[u8], limits: &Limits) -> std::result::Result<Ask, String> {
    let text = std::str::from_utf8(body).map_err(|_| "the body is not UTF-8".to_string())?;
    let members = scan::object(text)?;
    let (mut query, mut params) = (None, None);
    let mut ask = Ask {
        body: Vec::new(),
        pick: Pick::All,
        merge: Merge::Concat,
        column: "tenant".into(),
        partial: false,
        timeout: limits.timeout,
    };
    let mut seen: Vec<&str> = Vec::new();
    let mut picked = false;
    for (key, range) in &members {
        let raw = &text[range.clone()];
        if seen.contains(&key.as_str()) {
            return Err(format!("`{key}` is given twice"));
        }
        let value = fenec_core::json::parse_json(raw).map_err(|e| crate::message(&e))?;
        match (key.as_str(), value) {
            ("query", Value::Text(_)) => query = Some(raw),
            ("query", _) => return Err("`query` is the statement's text".into()),
            // Read here only to know it reads: it goes to the nodes as it
            // came, every number as written.
            ("params", _) => params = Some(raw),
            ("tenants" | "prefix", _) if picked => {
                return Err("`tenants` names them, `prefix` picks them: one or the other".into())
            }
            ("tenants", Value::List(names)) => {
                picked = true;
                let names = names
                    .into_iter()
                    .map(|n| match n {
                        Value::Text(n) => Ok(n),
                        _ => Err("`tenants` is a list of tenant names".to_string()),
                    })
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                ask.pick = Pick::Named(names);
            }
            ("prefix", Value::Text(p)) => {
                picked = true;
                ask.pick = Pick::Prefix(p);
            }
            ("merge", v) => ask.merge = read_merge(v)?,
            ("column", Value::Text(c)) if !c.is_empty() => ask.column = c,
            ("partial", Value::Bool(b)) => ask.partial = b,
            ("timeout_ms", Value::Int(ms)) if ms > 0 => {
                ask.timeout = Duration::from_millis(ms as u64).min(limits.timeout)
            }
            ("tenants" | "prefix" | "column" | "partial" | "timeout_ms", _) => {
                return Err(format!(
                    "`{key}` is {}",
                    match key.as_str() {
                        "tenants" => "a list of tenant names",
                        "prefix" => "a text",
                        "column" => "a column's name",
                        "partial" => "true or false",
                        _ => "a number of milliseconds",
                    }
                ))
            }
            _ => {
                return Err(format!(
                    "no member `{key}` in a query across tenants: query, params, tenants, \
                     prefix, merge, column, partial, timeout_ms"
                ))
            }
        }
        seen.push(key);
    }
    let Some(query) = query else {
        return Err("`query` is required: the statement each tenant runs".into());
    };
    let mut node = String::from("{\"query\":");
    node.push_str(query);
    if let Some(p) = params {
        node.push_str(",\"params\":");
        node.push_str(p);
    }
    node.push('}');
    ask.body = node.into_bytes();
    Ok(ask)
}

fn read_merge(v: Value) -> std::result::Result<Merge, String> {
    let Value::Object(members) = v else {
        return Err("`merge` is an object: {\"order\": [..], \"limit\": n}, or \
                    {\"sum\": [..], \"min\": [..], \"max\": [..], \"group\": [..]}"
            .into());
    };
    let names = |key: &str, v: Value| -> std::result::Result<Vec<String>, String> {
        let Value::List(items) = v else {
            return Err(format!("`{key}` is a list of column names"));
        };
        items
            .into_iter()
            .map(|c| match c {
                Value::Text(c) if !c.trim().is_empty() => Ok(c),
                _ => Err(format!("`{key}` is a list of column names")),
            })
            .collect()
    };
    let (mut order, mut limit) = (None, None);
    let mut fold = Fold {
        group: Vec::new(),
        sum: Vec::new(),
        min: Vec::new(),
        max: Vec::new(),
    };
    for (key, v) in members {
        match key.as_str() {
            "order" => {
                let keys = names("order", v)?
                    .into_iter()
                    .map(|k| {
                        let k = k.trim();
                        match k.rsplit_once(char::is_whitespace) {
                            Some((c, d)) if d.eq_ignore_ascii_case("desc") => {
                                (c.trim().to_string(), true)
                            }
                            Some((c, d)) if d.eq_ignore_ascii_case("asc") => {
                                (c.trim().to_string(), false)
                            }
                            _ => (k.to_string(), false),
                        }
                    })
                    .collect::<Vec<_>>();
                if keys.is_empty() {
                    return Err("`order` names a column at least".into());
                }
                order = Some(keys);
            }
            "limit" => match v {
                Value::Int(n) if n >= 0 => limit = Some(n as usize),
                _ => return Err("`limit` is a whole number".into()),
            },
            "group" => fold.group = names("group", v)?,
            "sum" => fold.sum = names("sum", v)?,
            "min" => fold.min = names("min", v)?,
            "max" => fold.max = names("max", v)?,
            other => {
                return Err(format!(
                    "no merge `{other}`: order and limit, or sum, min, max and group"
                ))
            }
        }
    }
    let folds = fold.named().next().is_some();
    match order {
        Some(_) if folds => {
            Err("an order merges the rows, a sum, min or max folds them: one or the other".into())
        }
        Some(keys) => Ok(Merge::Order { keys, limit }),
        None if limit.is_some() => Err("`limit` goes with `order`: the first rows of what?".into()),
        None if folds => {
            let mut all: Vec<&String> = fold.named().collect();
            all.sort_unstable();
            if let Some(w) = all.windows(2).find(|w| w[0] == w[1]) {
                return Err(format!(
                    "`{}` is named twice: a row holds a column once",
                    w[0]
                ));
            }
            Ok(Merge::Fold(fold))
        }
        None => Ok(Merge::Concat),
    }
}

// --------------------------------------------------------------- merging

/// A query refused as a whole once the answers are in: its status and why.
struct Refusal(u16, String);

/// The rows, merged, as a JSON array: `None` where nothing is merged
/// because the query fails. A tenant whose answer does not fit the merge
/// -- not rows, not in the order asked, a column the merge does not name
/// -- joins `failures`, its rows in nothing.
fn merged(
    ask: &Ask,
    limits: &Limits,
    answers: Vec<(&str, Vec<u8>)>,
    failures: &mut Vec<Failure>,
) -> std::result::Result<Option<String>, Refusal> {
    // Each answer's rows, found where they lie in its text.
    let mut read: Vec<(&str, String, Vec<Range<usize>>)> = Vec::with_capacity(answers.len());
    let mut rows = 0usize;
    for (tenant, body) in answers {
        let text = match String::from_utf8(body) {
            Ok(t) => t,
            Err(_) => {
                failures.push(failed(tenant, "its answer is not UTF-8"));
                continue;
            }
        };
        match scan::elements(&text) {
            Ok(list) => {
                rows += list.len();
                if rows > limits.max_rows {
                    return Err(Refusal(
                        413,
                        format!(
                            "the query gathered more than {} rows from its tenants \
                             (--fanout-max-rows): narrow it, give each tenant's statement a \
                             limit, or ask fewer tenants",
                            limits.max_rows
                        ),
                    ));
                }
                read.push((tenant, text, list));
            }
            Err(_) => failures.push(failed(
                tenant,
                "its answer is not a list of rows: a query across tenants merges rows, and \
                 a facet's counts or a statement's message do not merge",
            )),
        }
    }
    let out = match &ask.merge {
        Merge::Concat => concat(&ask.column, &read, failures),
        Merge::Order { keys, limit } => ordered(&ask.column, keys, *limit, &read, failures),
        Merge::Fold(fold) => folded(fold, &read, failures)?,
    };
    Ok(Some(out))
}

/// A failure the merge found in a tenant's answer.
fn failed(tenant: &str, why: &str) -> Failure {
    Failure {
        tenant: tenant.to_string(),
        node: String::new(),
        status: 400,
        why: why.to_string(),
    }
}

/// Each row's members, checked to leave room for the tenant's column.
fn members_of(
    column: &str,
    text: &str,
    rows: &[Range<usize>],
) -> std::result::Result<Vec<scan::Members>, String> {
    rows.iter()
        .map(|r| {
            let m = scan::members(text, r.clone())?;
            if m.iter().any(|(k, _)| k == column) {
                return Err(format!(
                    "its rows hold a `{column}` column of their own: name the tenant's with \
                     \"column\""
                ));
            }
            Ok(m)
        })
        .collect()
}

/// A row with the tenant's column first: the node's text after it.
fn row_into(
    out: &mut String,
    column: &str,
    tenant: &str,
    text: &str,
    row: &Range<usize>,
    empty: bool,
) {
    out.push('{');
    out.push_str(&quote(column));
    out.push(':');
    out.push_str(&quote(tenant));
    match empty {
        true => out.push('}'),
        false => {
            out.push(',');
            // Past the row's `{`: its members and its `}` as the node wrote
            // them, never decoded and written again.
            out.push_str(text[row.start + 1..row.end].trim_start());
        }
    }
}

fn concat(
    column: &str,
    read: &[(&str, String, Vec<Range<usize>>)],
    failures: &mut Vec<Failure>,
) -> String {
    let mut out = String::with_capacity(read.iter().map(|(_, t, _)| t.len() + 64).sum());
    out.push('[');
    let mut first = true;
    for (tenant, text, rows) in read {
        let members = match members_of(column, text, rows) {
            Ok(m) => m,
            Err(why) => {
                failures.push(failed(tenant, &why));
                continue;
            }
        };
        for (row, m) in rows.iter().zip(&members) {
            if !std::mem::take(&mut first) {
                out.push(',');
            }
            row_into(&mut out, column, tenant, text, row, m.is_empty());
        }
    }
    out.push(']');
    out
}

/// Two values of an answer in the order the engine puts the values they
/// stand for. A timestamp travels as ISO text, which omits its
/// milliseconds where they are 0, so its bytes do not order as its time
/// does (`...:00Z` after `...:00.500Z`): two texts that are timestamps as
/// fenecdb writes one compare as the times they name. Anything else
/// compares as the engine compares its values, text by its bytes -- a
/// collated or `nocase` order is the engine's alone, and a tenant whose
/// rows come in one is found out of order and named, never merged wrong.
fn cmp(a: &Value, b: &Value) -> Order {
    if let (Value::Text(x), Value::Text(y)) = (a, b) {
        if let (Some(p), Some(q)) = (iso_ms(x), iso_ms(y)) {
            return p.cmp(&q);
        }
    }
    a.cmp_value(b)
}

/// The milliseconds a text names when it is a timestamp as
/// `fenec_core::time::format_iso` writes one: `2026-10-12T09:30:12Z`, or
/// `.041` before the `Z`.
fn iso_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let n = b.len();
    let digits = |r: Range<usize>| b.get(r).is_some_and(|d| d.iter().all(u8::is_ascii_digit));
    let time_end = match n >= 5 && b[n - 5] == b'.' {
        true if digits(n - 4..n - 1) => n - 5,
        true => return None,
        false => n - 1,
    };
    // `-MM-DDTHH:MM:SS` before the seconds' end, and a year of four digits
    // or more before that, a sign allowed.
    if b.last() != Some(&b'Z') || time_end < 19 {
        return None;
    }
    let t = time_end - 15;
    let shaped = b[t] == b'-'
        && digits(t + 1..t + 3)
        && b[t + 3] == b'-'
        && digits(t + 4..t + 6)
        && b[t + 6] == b'T'
        && digits(t + 7..t + 9)
        && b[t + 9] == b':'
        && digits(t + 10..t + 12)
        && b[t + 12] == b':'
        && digits(t + 13..t + 15);
    let year = b.get(..t)?;
    let year = year.strip_prefix(b"-").unwrap_or(year);
    if !shaped || year.len() < 4 || !year.iter().all(u8::is_ascii_digit) {
        return None;
    }
    fenec_core::time::parse(s).ok()
}

/// A tenant's rows for an order: each one's values of the keys, and
/// whether it holds no member at all.
type Keyed = Vec<(Vec<Value>, bool)>;

fn cmp_keys(a: &[Value], b: &[Value], keys: &[(String, bool)]) -> Order {
    for (i, (_, desc)) in keys.iter().enumerate() {
        let o = cmp(&a[i], &b[i]);
        if o != Order::Equal {
            return if *desc { o.reverse() } else { o };
        }
    }
    Order::Equal
}

/// A tenant's rows, each with the values of the order's keys: checked to
/// come in that order, which is what makes the first rows of every tenant
/// together the first rows of all of them.
fn keyed(
    column: &str,
    keys: &[(String, bool)],
    text: &str,
    rows: &[Range<usize>],
) -> std::result::Result<Keyed, String> {
    let members = members_of(column, text, rows)?;
    let mut out: Keyed = Vec::with_capacity(rows.len());
    for m in &members {
        let mut values = Vec::with_capacity(keys.len());
        for (k, _) in keys {
            let Some((_, r)) = m.iter().find(|(name, _)| name == k) else {
                return Err(format!("its rows have no `{k}` column to order by"));
            };
            values.push(
                fenec_core::json::parse_json(&text[r.clone()]).map_err(|e| crate::message(&e))?,
            );
        }
        if let Some((before, _)) = out.last() {
            if cmp_keys(before, &values, keys) == Order::Greater {
                return Err(format!(
                    "its rows are not in the merge's order ({}): its statement orders them \
                     otherwise, or by a collation the router does not compare by",
                    keys.iter()
                        .map(|(k, d)| format!("{k}{}", if *d { " desc" } else { "" }))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        out.push((values, m.is_empty()));
    }
    Ok(out)
}

/// The k-way merge: each tenant's next row in a heap, the least on top --
/// by the keys, then by the tenant's place in name order -- taken until
/// `limit` rows are.
fn ordered(
    column: &str,
    keys: &[(String, bool)],
    limit: Option<usize>,
    read: &[(&str, String, Vec<Range<usize>>)],
    failures: &mut Vec<Failure>,
) -> String {
    let mut lists: Vec<(usize, Keyed)> = Vec::new();
    for (i, (tenant, text, rows)) in read.iter().enumerate() {
        match keyed(column, keys, text, rows) {
            Ok(k) => lists.push((i, k)),
            Err(why) => failures.push(failed(tenant, &why)),
        }
    }
    let limit = limit.unwrap_or(usize::MAX);
    let mut pos = vec![0usize; lists.len()];
    let mut heap: Vec<usize> = (0..lists.len())
        .filter(|&l| !lists[l].1.is_empty())
        .collect();
    let less = |pos: &[usize], a: usize, b: usize| {
        let (x, y) = (&lists[a].1[pos[a]].0, &lists[b].1[pos[b]].0);
        cmp_keys(x, y, keys).then(a.cmp(&b)) == Order::Less
    };
    for i in (0..heap.len() / 2).rev() {
        sift_down(&mut heap, i, |a, b| less(&pos, a, b));
    }
    let mut out = String::from("[");
    let mut taken = 0;
    while taken < limit {
        let Some(&l) = heap.first() else { break };
        let (tenant, text, rows) = &read[lists[l].0];
        if taken > 0 {
            out.push(',');
        }
        row_into(
            &mut out,
            column,
            tenant,
            text,
            &rows[pos[l]],
            lists[l].1[pos[l]].1,
        );
        taken += 1;
        pos[l] += 1;
        if pos[l] == lists[l].1.len() {
            let last = heap.pop().unwrap_or(l);
            if heap.is_empty() {
                break;
            }
            heap[0] = last;
        }
        sift_down(&mut heap, 0, |a, b| less(&pos, a, b));
    }
    out.push(']');
    out
}

fn sift_down(heap: &mut [usize], mut i: usize, less: impl Fn(usize, usize) -> bool) {
    loop {
        let (l, r) = (2 * i + 1, 2 * i + 2);
        let mut m = i;
        if l < heap.len() && less(heap[l], heap[m]) {
            m = l;
        }
        if r < heap.len() && less(heap[r], heap[m]) {
            m = r;
        }
        if m == i {
            return;
        }
        heap.swap(i, m);
        i = m;
    }
}

/// A folded column as it goes.
enum Slot {
    /// A group column: its text as the first row of the group wrote it.
    Key(String),
    /// Whole numbers added exactly, the rest as floats.
    Sum {
        ints: i64,
        floats: f64,
        float: bool,
        any: bool,
    },
    /// The least or greatest value, and its text.
    Least(Option<(Value, String)>),
    Most(Option<(Value, String)>),
}

/// A row of a tenant's answer as the fold takes it: each column's role and
/// value and text, in the row's order.
type Folding = Vec<(usize, Value, Range<usize>)>;

fn folded(
    fold: &Fold,
    read: &[(&str, String, Vec<Range<usize>>)],
    failures: &mut Vec<Failure>,
) -> std::result::Result<String, Refusal> {
    // The output's columns, in the order the first row holds them.
    let mut columns: Vec<(String, Role)> = Vec::new();
    let mut groups: Vec<(Vec<Value>, Vec<Slot>)> = Vec::new();
    let mut by_key: HashMap<String, usize> = HashMap::new();
    for (tenant, text, rows) in read {
        // Read whole before any of it is folded: a tenant found wrong
        // halfway would otherwise leave its first rows in the sums.
        let mut taken: Vec<Folding> = Vec::with_capacity(rows.len());
        let mut why = None;
        for r in rows {
            match fold_row(fold, text, r.clone(), &mut columns) {
                Ok(row) => taken.push(row),
                Err(w) => {
                    why = Some(w);
                    break;
                }
            }
        }
        if let Some(w) = why {
            failures.push(failed(tenant, &w));
            continue;
        }
        for row in taken {
            let mut key = String::new();
            let mut values = Vec::with_capacity(fold.group.len());
            for g in &fold.group {
                let (_, v, _) = row
                    .iter()
                    .find(|(c, _, _)| columns[*c].0 == *g)
                    .expect("a row holds every column the merge names");
                fenec_core::json::value_into(&mut key, v);
                key.push('\0');
                values.push(v.clone());
            }
            let at = match by_key.get(&key) {
                Some(&at) => at,
                None => {
                    let slots = columns
                        .iter()
                        .map(|(_, role)| match role {
                            Role::Group => Slot::Key(String::new()),
                            Role::Sum => Slot::Sum {
                                ints: 0,
                                floats: 0.0,
                                float: false,
                                any: false,
                            },
                            Role::Min => Slot::Least(None),
                            Role::Max => Slot::Most(None),
                        })
                        .collect();
                    groups.push((values, slots));
                    by_key.insert(key, groups.len() - 1);
                    groups.len() - 1
                }
            };
            let slots = &mut groups[at].1;
            for (c, v, r) in row {
                let raw = &text[r];
                match &mut slots[c] {
                    Slot::Key(k) => {
                        if k.is_empty() {
                            k.push_str(raw);
                        }
                    }
                    Slot::Sum {
                        ints,
                        floats,
                        float,
                        any,
                    } => match v {
                        Value::Int(n) => {
                            *any = true;
                            *ints = ints.checked_add(n).ok_or_else(|| {
                                Refusal(
                                    400,
                                    format!(
                                        "the sum of `{}` passes 64 bits, which a whole number \
                                         holds at most",
                                        columns[c].0
                                    ),
                                )
                            })?;
                        }
                        Value::Float(f) => {
                            (*any, *float) = (true, true);
                            *floats += f;
                        }
                        _ => {}
                    },
                    Slot::Least(best) => {
                        if !matches!(v, Value::Null)
                            && best.as_ref().is_none_or(|(b, _)| cmp(&v, b) == Order::Less)
                        {
                            *best = Some((v, raw.to_string()));
                        }
                    }
                    Slot::Most(best) => {
                        if !matches!(v, Value::Null)
                            && best
                                .as_ref()
                                .is_none_or(|(b, _)| cmp(&v, b) == Order::Greater)
                        {
                            *best = Some((v, raw.to_string()));
                        }
                    }
                }
            }
        }
    }
    // With no group and no row, the one row a fold of nothing is: every
    // folded column null, as a sum over no rows is.
    if fold.group.is_empty() && groups.is_empty() {
        let mut out = String::from("[{");
        for (i, c) in fold
            .sum
            .iter()
            .chain(&fold.min)
            .chain(&fold.max)
            .enumerate()
        {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&quote(c));
            out.push_str(":null");
        }
        out.push_str("}]");
        return Ok(out);
    }
    groups.sort_by(|a, b| {
        a.0.iter()
            .zip(&b.0)
            .map(|(x, y)| cmp(x, y))
            .find(|o| *o != Order::Equal)
            .unwrap_or(Order::Equal)
    });
    let mut out = String::from("[");
    for (i, (_, slots)) in groups.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('{');
        for (j, ((name, _), slot)) in columns.iter().zip(slots).enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str(&quote(name));
            out.push(':');
            match slot {
                Slot::Key(raw) => out.push_str(raw),
                Slot::Sum { any: false, .. } => out.push_str("null"),
                Slot::Sum {
                    ints, float: false, ..
                } => fenec_core::json::int_into(&mut out, *ints),
                Slot::Sum { ints, floats, .. } => {
                    fenec_core::json::value_into(&mut out, &Value::Float(*ints as f64 + floats))
                }
                Slot::Least(None) | Slot::Most(None) => out.push_str("null"),
                Slot::Least(Some((_, raw))) | Slot::Most(Some((_, raw))) => out.push_str(raw),
            }
        }
        out.push('}');
    }
    out.push(']');
    Ok(out)
}

/// One row read for a fold: each of its columns named by the merge, held
/// to it -- every column grouped by or folded, every one named there, a
/// sum's a number -- and the output's columns learned from the first row.
fn fold_row(
    fold: &Fold,
    text: &str,
    row: Range<usize>,
    columns: &mut Vec<(String, Role)>,
) -> std::result::Result<Folding, String> {
    let members = scan::members(text, row)?;
    let mut out = Vec::with_capacity(members.len());
    for (name, r) in members {
        let Some(role) = fold.role(&name) else {
            return Err(format!(
                "its rows hold `{name}`, which the merge neither groups by nor folds: a \
                 column it would drop is an answer cut"
            ));
        };
        let v = fenec_core::json::parse_json(&text[r.clone()]).map_err(|e| crate::message(&e))?;
        if role == Role::Sum && !matches!(v, Value::Int(_) | Value::Float(_) | Value::Null) {
            return Err(format!(
                "`{name}` is summed, and its rows hold no number there"
            ));
        }
        let c = match columns.iter().position(|(c, _)| *c == name) {
            Some(c) => c,
            None => {
                columns.push((name, role));
                columns.len() - 1
            }
        };
        out.push((c, v, r));
    }
    if let Some(missing) = fold
        .named()
        .find(|n| !out.iter().any(|(c, _, _)| columns[*c].0 == **n))
    {
        return Err(format!("its rows have no `{missing}` column"));
    }
    Ok(out)
}

// ------------------------------------------------------------------ JSON

/// Where a JSON text's values lie, found without decoding them: the
/// router passes a node's rows on as the node wrote them, and decodes only
/// the values a merge compares or adds.
mod scan {
    use std::ops::Range;

    /// An object's members: each key, decoded, and where its value lies.
    pub type Members = Vec<(String, Range<usize>)>;

    fn ws(b: &[u8], i: &mut usize) {
        while matches!(b.get(*i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            *i += 1;
        }
    }

    /// Past the string whose quote is at `i`.
    fn string(b: &[u8], i: &mut usize) -> Result<(), String> {
        *i += 1;
        loop {
            match b.get(*i) {
                None => return Err("a string runs to the end of the text".into()),
                Some(b'"') => {
                    *i += 1;
                    return Ok(());
                }
                Some(b'\\') => *i += 2,
                Some(_) => *i += 1,
            }
        }
    }

    /// Past the value at `i`. Its extent alone: what is in it is read by
    /// whoever decodes it.
    fn value(b: &[u8], i: &mut usize) -> Result<(), String> {
        ws(b, i);
        match b.get(*i) {
            None => Err("a value is missing at the end of the text".into()),
            Some(b'"') => string(b, i),
            Some(b'{' | b'[') => {
                let mut depth = 0usize;
                loop {
                    match b.get(*i) {
                        None => {
                            return Err("an object or a list runs to the end of the text".into())
                        }
                        Some(b'"') => {
                            string(b, i)?;
                            continue;
                        }
                        Some(b'{' | b'[') => depth += 1,
                        Some(b'}' | b']') => {
                            depth -= 1;
                            if depth == 0 {
                                *i += 1;
                                return Ok(());
                            }
                        }
                        _ => {}
                    }
                    *i += 1;
                }
            }
            Some(b'}' | b']' | b',' | b':') => Err(format!("a value is missing at byte {i}")),
            Some(_) => {
                let start = *i;
                while !matches!(
                    b.get(*i),
                    None | Some(b',' | b']' | b'}' | b':' | b' ' | b'\t' | b'\n' | b'\r')
                ) {
                    *i += 1;
                }
                match *i > start {
                    true => Ok(()),
                    false => Err(format!("a value is missing at byte {start}")),
                }
            }
        }
    }

    /// A key's text: its bytes between the quotes, or decoded where it
    /// holds an escape.
    fn key(raw: &str) -> Result<String, String> {
        if !raw.contains('\\') {
            return Ok(raw[1..raw.len() - 1].to_string());
        }
        match fenec_core::json::parse_json(raw) {
            Ok(fenec_core::prelude::Value::Text(k)) => Ok(k),
            _ => Err(format!("a key that does not read: {raw}")),
        }
    }

    /// The members of the object at `range.start`, and where it ends.
    fn members_at(text: &str, start: usize) -> Result<(Members, usize), String> {
        let b = text.as_bytes();
        let mut i = start;
        ws(b, &mut i);
        if b.get(i) != Some(&b'{') {
            return Err("not an object".into());
        }
        i += 1;
        let mut out = Vec::new();
        ws(b, &mut i);
        if b.get(i) == Some(&b'}') {
            return Ok((out, i + 1));
        }
        loop {
            ws(b, &mut i);
            if b.get(i) != Some(&b'"') {
                return Err(format!("a key is missing at byte {i}"));
            }
            let k = i;
            string(b, &mut i)?;
            let name = key(&text[k..i])?;
            ws(b, &mut i);
            if b.get(i) != Some(&b':') {
                return Err(format!("`:` is missing at byte {i}"));
            }
            i += 1;
            ws(b, &mut i);
            let v = i;
            value(b, &mut i)?;
            out.push((name, v..i));
            ws(b, &mut i);
            match b.get(i) {
                Some(b',') => i += 1,
                Some(b'}') => return Ok((out, i + 1)),
                _ => return Err(format!("`,` or `}}` is missing at byte {i}")),
            }
        }
    }

    /// The members of the object `range` holds.
    pub fn members(text: &str, range: Range<usize>) -> Result<Members, String> {
        let (m, end) = members_at(text, range.start)?;
        match end == range.end {
            true => Ok(m),
            false => Err("not an object".into()),
        }
    }

    /// The members of the object the whole of `text` is.
    pub fn object(text: &str) -> Result<Members, String> {
        let (m, mut end) = members_at(text, 0).map_err(|e| format!("the body: {e}"))?;
        ws(text.as_bytes(), &mut end);
        match end == text.len() {
            true => Ok(m),
            false => Err("the body: text after its object".into()),
        }
    }

    /// Where each element of the list the whole of `text` is lies.
    pub fn elements(text: &str) -> Result<Vec<Range<usize>>, String> {
        let b = text.as_bytes();
        let mut i = 0;
        ws(b, &mut i);
        if b.get(i) != Some(&b'[') {
            return Err("not a list".into());
        }
        i += 1;
        let mut out = Vec::new();
        ws(b, &mut i);
        if b.get(i) == Some(&b']') {
            i += 1;
        } else {
            loop {
                ws(b, &mut i);
                let s = i;
                value(b, &mut i)?;
                out.push(s..i);
                ws(b, &mut i);
                match b.get(i) {
                    Some(b',') => i += 1,
                    Some(b']') => {
                        i += 1;
                        break;
                    }
                    _ => return Err(format!("`,` or `]` is missing at byte {i}")),
                }
            }
        }
        ws(b, &mut i);
        match i == b.len() {
            true => Ok(out),
            false => Err("text after the list".into()),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rows_are_found_where_they_lie() {
            let t = r#" [ {"a":1,"b":"x,]}"} , {"a":[1,{"c":"\"]"}]} ,{}] "#;
            let rows = elements(t).unwrap();
            assert_eq!(rows.len(), 3);
            assert_eq!(&t[rows[0].clone()], r#"{"a":1,"b":"x,]}"}"#);
            let m = members(t, rows[1].clone()).unwrap();
            assert_eq!(m[0].0, "a");
            assert_eq!(&t[m[0].1.clone()], r#"[1,{"c":"\"]"}]"#);
            assert!(members(t, rows[2].clone()).unwrap().is_empty());
            // A key with an escape in it is decoded: `e` is an `e`.
            let escaped = format!("{{\"t{}u0065nant\":2}}", '\\');
            let k = members(&escaped, 0..escaped.len()).unwrap();
            assert_eq!(k[0].0, "tenant");
            for bad in ["[1,]", "[1 2]", "[", r#"["a]"#, "[1] x", "{}"] {
                assert!(elements(bad).is_err(), "{bad}");
            }
            assert!(object(r#"{"a":1} {"b":2}"#).is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timestamp_compares_as_its_time_and_text_as_its_bytes() {
        let at = |s: &str| Value::Text(s.into());
        assert_eq!(
            cmp(&at("2026-10-12T09:30:00Z"), &at("2026-10-12T09:30:00.500Z")),
            Order::Less
        );
        assert_eq!(
            cmp(&at("2026-10-12T09:30:00.500Z"), &at("2026-10-12T09:30:01Z")),
            Order::Less
        );
        for not in [
            "2026-10-12",
            "2026-10-12T09:30:00",
            "x2026-10-12T09:30:00Z",
            "2026-10-12T09:30:00.5Z",
        ] {
            assert_eq!(iso_ms(not), None, "{not}");
        }
        assert_eq!(cmp(&at("b"), &at("a")), Order::Greater);
        assert_eq!(cmp(&Value::Int(2), &Value::Float(1.5)), Order::Greater);
        assert_eq!(cmp(&Value::Null, &Value::Int(0)), Order::Less);
    }

    fn ask(merge: &str) -> Ask {
        let body = format!(r#"{{"query":"get x","merge":{merge}}}"#);
        read_ask(body.as_bytes(), &Limits::default()).unwrap()
    }

    fn read(answers: &[(&'static str, &str)]) -> Vec<(&'static str, String, Vec<Range<usize>>)> {
        answers
            .iter()
            .map(|(t, a)| (*t, a.to_string(), scan::elements(a).unwrap()))
            .collect()
    }

    #[test]
    fn an_order_merges_each_tenants_first_rows() {
        let a = ask(r#"{"order":["at desc","id"],"limit":3}"#);
        let Merge::Order { keys, limit } = &a.merge else {
            panic!()
        };
        let r = read(&[
            ("acme", r#"[{"id":4,"at":9},{"id":1,"at":5}]"#),
            (
                "beta",
                r#"[{"id":2,"at":9},{"id":3,"at":7},{"id":9,"at":1}]"#,
            ),
            ("gamma", "[]"),
        ]);
        let mut f = Vec::new();
        let out = ordered("tenant", keys, *limit, &r, &mut f);
        assert!(f.is_empty());
        assert_eq!(
            out,
            r#"[{"tenant":"beta","id":2,"at":9},{"tenant":"acme","id":4,"at":9},{"tenant":"beta","id":3,"at":7}]"#
        );
        // A tenant out of order is named, and none of its rows merged.
        let r = read(&[
            ("acme", r#"[{"id":1,"at":1},{"id":2,"at":5}]"#),
            ("beta", r#"[{"id":3,"at":2}]"#),
        ]);
        let out = ordered("tenant", keys, None, &r, &mut f);
        assert_eq!(out, r#"[{"tenant":"beta","id":3,"at":2}]"#);
        assert_eq!(f[0].tenant, "acme");
        assert!(
            f[0].why.contains("not in the merge's order"),
            "{}",
            f[0].why
        );
    }

    #[test]
    fn a_fold_adds_counts_and_keeps_extremes_by_group() {
        let a = ask(r#"{"sum":["n","total"],"max":["last"],"group":["day"]}"#);
        let Merge::Fold(fold) = &a.merge else {
            panic!()
        };
        let r = read(&[
            (
                "acme",
                r#"[{"day":"2026-10-01T00:00:00Z","n":2,"total":1.5,"last":"2026-10-01T09:00:00Z"},
                    {"day":"2026-10-02T00:00:00Z","n":1,"total":null,"last":"2026-10-02T00:00:00.250Z"}]"#,
            ),
            (
                "beta",
                r#"[{"day":"2026-10-02T00:00:00Z","n":4,"total":2,"last":"2026-10-02T00:00:00Z"}]"#,
            ),
        ]);
        let mut f = Vec::new();
        let out = folded(fold, &r, &mut f).ok().unwrap();
        assert!(f.is_empty());
        assert_eq!(
            out,
            r#"[{"day":"2026-10-01T00:00:00Z","n":2,"total":1.5,"last":"2026-10-01T09:00:00Z"},{"day":"2026-10-02T00:00:00Z","n":5,"total":2,"last":"2026-10-02T00:00:00.250Z"}]"#
        );
        // A column the merge does not name fails its tenant, whose rows
        // stay out of every sum.
        let r = read(&[
            (
                "acme",
                r#"[{"day":1,"n":1,"total":1,"last":1},{"day":1,"n":1,"total":1,"last":1,"x":0}]"#,
            ),
            ("beta", r#"[{"day":1,"n":2,"total":0,"last":3}]"#),
        ]);
        let out = folded(fold, &r, &mut f).ok().unwrap();
        assert_eq!(out, r#"[{"day":1,"n":2,"total":0,"last":3}]"#);
        assert_eq!(f[0].tenant, "acme");
        let too_much = read(&[
            ("a", r#"[{"n":9223372036854775807}]"#),
            ("b", r#"[{"n":1}]"#),
        ]);
        let just = ask(r#"{"sum":["n"]}"#);
        let Merge::Fold(sum) = &just.merge else {
            panic!()
        };
        assert!(folded(sum, &too_much, &mut Vec::new()).is_err());
        assert_eq!(
            folded(sum, &[], &mut Vec::new()).ok().unwrap(),
            r#"[{"n":null}]"#
        );
    }

    #[test]
    fn a_request_says_each_thing_once_and_nothing_else() {
        let l = Limits::default();
        let bad = [
            r#"{}"#,
            r#"{"query":1}"#,
            r#"{"query":"get x","query":"get y"}"#,
            r#"{"query":"get x","merge":{"limit":2}}"#,
            r#"{"query":"get x","merge":{"order":["a"],"sum":["n"]}}"#,
            r#"{"query":"get x","merge":{"sum":["n"],"max":["n"]}}"#,
            r#"{"query":"get x","merge":{"avg":["n"]}}"#,
            r#"{"query":"get x","tenants":["a"],"prefix":"b"}"#,
            r#"{"query":"get x","partail":true}"#,
            r#"{"query":"get x","params":[1,}"#,
        ];
        for b in bad {
            assert!(read_ask(b.as_bytes(), &l).is_err(), "{b}");
        }
        let a = read_ask(
            br#"{"query":"get x where n = $1","params":[0.1, [1,2]],"timeout_ms":999999999}"#,
            &l,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(a.body).unwrap(),
            r#"{"query":"get x where n = $1","params":[0.1, [1,2]]}"#
        );
        assert_eq!(a.timeout, l.timeout);
    }
}
