//! # fenec-shard
//!
//! A router in front of several `fenec-pg --dir` nodes. Each tenant lives
//! whole on one node, in a file of its own; the router knows which node
//! and forwards `/t/<tenant>/...` there byte for byte.
//!
//! ```text
//! client -- /t/acme/notes --> fenec-shard -- /t/acme/notes --> node n1
//!                                 |                              acme.fenec
//!                                 +--> node n2                   beta.fenec
//! ```
//!
//! **Why tenants and not hash partitioning.** A tenant on one node means
//! every query runs where all of its data is: `near`, `match`, `lookup` and
//! `count` keep their single-database meaning, ids and the change sequence
//! stay the tenant's own, and the router never parses a query. Splitting one
//! collection across nodes would need a merge for every read shape and
//! global BM25 statistics for `match`; that is a different feature, and a
//! tenant that outgrows a node is the case it would be for.
//!
//! **The router does not understand the requests it forwards.** It reads
//! the head and the `Content-Length` body, picks the node and copies the
//! answer back. An SSE subscription is copied until either side closes, so
//! the stream format lives in one place, the node.
//!
//! What it owns is the directory (tenant -> node) and three operations that
//! change it: place a new tenant, move one, delete one. See [`directory`]
//! and the `/_shard/` endpoints in [`Router::admin`].

pub mod directory;
pub mod upstream;

use directory::{Directory, Node, State};
use fenec_core::prelude::*;
use fenec_http::http::{self, Method, Request, Response};
use fenec_http::replication::{self, Replication};
use std::collections::HashSet;
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use upstream::Pool;

pub struct Config {
    pub addr: String,
    /// Token for `/_shard/`. The data requests carry whatever the nodes'
    /// `--http-token` expects; the router passes `Authorization` through.
    pub token: Option<String>,
    pub insecure: bool,
    /// Every connection is a thread, as in the nodes.
    pub max_connections: usize,
    pub max_body: usize,
    pub idle_timeout: Option<Duration>,
    /// Connect and read bound when talking to a node.
    pub upstream_timeout: Duration,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            addr: "127.0.0.1:8090".into(),
            token: None,
            insecure: false,
            max_connections: 1000,
            max_body: 64 << 20,
            idle_timeout: Some(Duration::from_secs(60)),
            upstream_timeout: Duration::from_secs(60),
        }
    }
}

pub struct Router {
    dir: RwLock<Directory>,
    /// The directory's own replication: the feed a standby reads, and the
    /// follower a standby runs. `None` without --replication-token.
    repl: Option<Arc<Replication>>,
    pool: Pool,
    cfg: Config,
    /// Tenants with a create, move or delete in progress: a second
    /// operation on the same tenant is refused rather than interleaved.
    busy: Mutex<HashSet<String>>,
    live: AtomicUsize,
}

/// An operation failure, shaped as an HTTP status and message.
struct Fail(u16, String);

type Outcome<T> = std::result::Result<T, Fail>;

impl Router {
    pub fn new(dir: Directory, cfg: Config) -> Arc<Router> {
        Router::over(dir, cfg, None)
    }

    /// A router whose directory is replicated: a standby follows it and
    /// forwards from the same placements, and takes over when promoted.
    pub fn replicated(dir: Directory, cfg: Config, repl: Arc<Replication>) -> Arc<Router> {
        Router::over(dir, cfg, Some(repl))
    }

    fn over(dir: Directory, cfg: Config, repl: Option<Arc<Replication>>) -> Arc<Router> {
        Arc::new(Router {
            dir: RwLock::new(dir),
            repl,
            pool: Pool::new(cfg.upstream_timeout),
            cfg,
            busy: Mutex::new(HashSet::new()),
            live: AtomicUsize::new(0),
        })
    }

    /// Opens the listener. As with the nodes, a non-loopback address needs
    /// a token: `/_shard/` can move and delete every tenant.
    pub fn bind(&self) -> std::io::Result<TcpListener> {
        let remote = match self.cfg.addr.to_socket_addrs() {
            Ok(mut it) => it.any(|a| !a.ip().is_loopback()),
            Err(_) => false,
        };
        if remote && self.cfg.token.is_none() && !self.cfg.insecure {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "{} is not a loopback address and there is no --token: \
                     /_shard/ would let anyone move and delete tenants",
                    self.cfg.addr
                ),
            ));
        }
        TcpListener::bind(&self.cfg.addr)
    }

    pub fn serve_on(self: &Arc<Router>, listener: TcpListener) -> std::io::Result<()> {
        fenec_http::log!(
            "fenec-shard {} listening on: http://{}  [{} node(s), {} tenant(s)]",
            fenec_core::VERSION,
            listener.local_addr()?,
            self.read_dir().nodes().len(),
            self.read_dir().tenants().len(),
        );
        for (t, p) in self.read_dir().tenants() {
            if p.state == State::Moving {
                fenec_http::log!(
                    "tenant `{t}` was being moved when the router stopped; it is served \
                     from `{}`, and moving it again clears the state",
                    p.node
                );
            }
        }
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            if self.cfg.max_connections > 0 && live > self.cfg.max_connections {
                self.live.fetch_sub(1, Ordering::SeqCst);
                let mut s = stream;
                let _ = Response::error(503, "too many connections").write(&mut s, false, false);
                continue;
            }
            let router = Arc::clone(self);
            let spawned = std::thread::Builder::new()
                .name("fenec-shard".into())
                .spawn(move || {
                    router.connection(stream);
                    router.live.fetch_sub(1, Ordering::SeqCst);
                });
            if spawned.is_err() {
                self.live.fetch_sub(1, Ordering::SeqCst);
            }
        }
        Ok(())
    }

    fn read_dir(&self) -> std::sync::RwLockReadGuard<'_, Directory> {
        self.dir.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_dir(&self) -> std::sync::RwLockWriteGuard<'_, Directory> {
        self.dir.write().unwrap_or_else(|e| e.into_inner())
    }

    fn connection(&self, stream: TcpStream) {
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(self.cfg.idle_timeout);
        let Ok(mut out) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(stream);
        loop {
            let req = match http::read_request(&mut reader, self.cfg.max_body) {
                Ok(Some(r)) => r,
                Ok(None) => return,
                Err(http::BadRequest(status, msg)) => {
                    let _ = Response::error(status, &msg).write(&mut out, false, false);
                    return;
                }
            };
            // A standby's directory moves as the primary's writes arrive:
            // the maps are read again before the request is answered from
            // them, and only then.
            if self.read_dir().stale() {
                if let Err(e) = self.write_dir().refresh() {
                    fenec_http::log!("could not read the directory: {e}");
                }
            }
            let keep = match req.segments().first() {
                Some(&"t") => self.forward(&req, &mut out),
                Some(&"_replication") => {
                    let Some(repl) = self.repl.clone() else {
                        let _ = Response::error(404, "this router has no --replication-token")
                            .write(&mut out, false, false);
                        return;
                    };
                    // The directory's database, not the router's lock: a
                    // stream lasts as long as the replica stays connected.
                    let db = self.read_dir().db().clone();
                    match replication::handle(&mut out, &db, &repl, &req) {
                        // The stream took the connection over and has ended.
                        None => return,
                        Some(resp) => {
                            resp.write(&mut out, req.keep_alive, req.method == Method::Head)
                                .is_ok()
                                && req.keep_alive
                        }
                    }
                }
                Some(&"_shard") => {
                    let resp = self.admin(&req);
                    resp.write(&mut out, req.keep_alive, req.method == Method::Head)
                        .is_ok()
                        && req.keep_alive
                }
                _ => {
                    Response::error(404, "the router serves /t/<tenant>/... and /_shard/")
                        .write(&mut out, req.keep_alive, false)
                        .is_ok()
                        && req.keep_alive
                }
            };
            if !keep {
                return;
            }
        }
    }

    // ------------------------------------------------------------ forwarding

    /// Forwards one request and copies the answer back. Returns whether the
    /// client connection can carry another request.
    fn forward(&self, req: &Request, out: &mut TcpStream) -> bool {
        let head_only = req.method == Method::Head;
        let reply = |out: &mut TcpStream, resp: Response| {
            resp.write(out, req.keep_alive, head_only).is_ok() && req.keep_alive
        };
        let segs = req.segments();
        let tenant = segs.get(1).copied().unwrap_or("");
        let addr = {
            let dir = self.read_dir();
            let node = dir
                .placement(tenant)
                .and_then(|p| dir.node(&p.node).map(|n| (p.node.clone(), n.addr.clone())));
            match node {
                Some(n) => n,
                None => {
                    return reply(
                        out,
                        Response::error(404, &format!("no tenant `{tenant}` in the directory")),
                    )
                }
            }
        };
        let (node, addr) = addr;

        let headers: Vec<(String, String)> = req
            .headers
            .iter()
            .filter(|(k, _)| !hop_by_hop(k) && !k.eq_ignore_ascii_case("host"))
            .cloned()
            .collect();
        let answer =
            match self
                .pool
                .send(&addr, req.method.name(), &req.target, &headers, &req.body)
            {
                Ok(a) => a,
                Err(e) => {
                    return reply(
                        out,
                        Response::error(
                            502,
                            &format!("node `{node}` ({addr}) did not answer: {e}"),
                        ),
                    )
                }
            };

        let status = answer.status;
        let mut head = format!("HTTP/1.1 {status} {}\r\n", http::reason(status));
        for (k, v) in &answer.headers {
            if !hop_by_hop(k) && !k.eq_ignore_ascii_case("content-length") {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
        }

        if let Some(length) = answer.content_length() {
            let body = match answer.read_body(&self.pool, head_only) {
                Ok(b) => b,
                Err(e) => {
                    return reply(
                        out,
                        Response::error(502, &format!("node `{node}` broke off: {e}")),
                    )
                }
            };
            // A HEAD answer states the length of the body it leaves out.
            head.push_str(&format!(
                "Content-Length: {}\r\nConnection: {}\r\n\r\n",
                if head_only { length } else { body.len() },
                if req.keep_alive {
                    "keep-alive"
                } else {
                    "close"
                }
            ));
            let sent = out
                .write_all(head.as_bytes())
                .and_then(|_| out.write_all(&body))
                .and_then(|_| out.flush());
            return sent.is_ok() && req.keep_alive;
        }

        // No length: a stream. It is copied as it arrives until either side
        // closes, and the client connection ends with it.
        head.push_str("Connection: close\r\n\r\n");
        if out.write_all(head.as_bytes()).is_err() {
            return false;
        }
        let _ = out.set_read_timeout(None);
        if let Ok(mut stream) = answer.into_stream() {
            let _ = std::io::copy(&mut stream, out);
        }
        false
    }

    // ------------------------------------------------------------------ admin

    /// ```text
    /// GET    /_shard/nodes                     [{name, addr}]
    /// PUT    /_shard/nodes/<n>   {addr, token} add or change a node
    /// DELETE /_shard/nodes/<n>                 refused while it holds tenants
    /// GET    /_shard/tenants                   [{name, node, state}]
    /// PUT    /_shard/tenants/<t> [{node}]      place and create; least disk wins
    /// DELETE /_shard/tenants/<t>               delete on the node, then forget
    /// POST   /_shard/tenants/<t>/move {to}     move to another node
    /// ```
    pub fn admin(&self, req: &Request) -> Response {
        if let Some(token) = &self.cfg.token {
            let given = req
                .header("authorization")
                .and_then(|v| v.strip_prefix("Bearer "))
                .unwrap_or("");
            if !constant_eq(given.as_bytes(), token.as_bytes()) {
                return Response::error(401, "invalid or missing token")
                    .header("WWW-Authenticate", "Bearer");
            }
        }
        let body = match body_fields(req) {
            Ok(b) => b,
            Err(e) => return Response::error(400, &e),
        };
        let segs = req.segments();
        // A standby's directory is a replica's file: it takes no write of
        // its own, so a change belongs on the primary. The reads below are
        // answered from the maps the primary's writes filled.
        let writes = !matches!(
            (req.method, &segs[1..]),
            (Method::Get, ["nodes"]) | (Method::Get, ["tenants"])
        );
        if writes && self.read_dir().following() {
            return Response::error(
                409,
                "this router follows another: send /_shard/ changes to the primary, \
                 or promote this one (POST /_replication/promote)",
            );
        }
        let result = match (req.method, &segs[1..]) {
            (Method::Get, ["nodes"]) => Ok(self.list_nodes()),
            (Method::Put, ["nodes", n]) => self.set_node(n, &body),
            (Method::Delete, ["nodes", n]) => self.remove_node(n),
            (Method::Get, ["tenants"]) => Ok(self.list_tenants()),
            (Method::Put, ["tenants", t]) => self.create(t, field(&body, "node")),
            (Method::Delete, ["tenants", t]) => self.delete(t),
            (Method::Post, ["tenants", t, "move"]) => match field(&body, "to") {
                Some(to) => self.relocate(t, to),
                None => Err(Fail(400, "the body needs {\"to\": \"<node>\"}".into())),
            },
            _ => Err(Fail(404, "no such endpoint under /_shard/".into())),
        };
        result.unwrap_or_else(|Fail(status, msg)| Response::error(status, &msg))
    }

    fn list_nodes(&self) -> Response {
        let dir = self.read_dir();
        let items: Vec<String> = dir
            .nodes()
            .iter()
            .map(|(name, n)| format!("{{\"name\":{},\"addr\":{}}}", quote(name), quote(&n.addr)))
            .collect();
        Response::json(200, format!("[{}]", items.join(",")))
    }

    fn list_tenants(&self) -> Response {
        let items: Vec<String> = self
            .read_dir()
            .tenants()
            .iter()
            .map(|(name, p)| {
                format!(
                    "{{\"name\":{},\"node\":{},\"state\":\"{}\"}}",
                    quote(name),
                    quote(&p.node),
                    if p.state == State::Moving {
                        "moving"
                    } else {
                        "active"
                    }
                )
            })
            .collect();
        Response::json(200, format!("[{}]", items.join(",")))
    }

    fn set_node(&self, name: &str, body: &[(String, Value)]) -> Outcome<Response> {
        fenec_http::tenants::check_name(name).map_err(|r| Fail(r.0, r.1))?;
        let (Some(addr), Some(token)) = (field(body, "addr"), field(body, "token")) else {
            return Err(Fail(
                400,
                "the body needs {\"addr\": .., \"token\": ..}".into(),
            ));
        };
        let node = Node {
            addr: addr.into(),
            token: token.into(),
        };
        // A node is checked before it is recorded: an address or token that
        // does not work would otherwise surface on the first placement.
        let (status, _) = self
            .pool
            .call(&node.addr, "GET", "/_admin/stats", &node.token, b"")
            .map_err(|e| Fail(502, format!("{addr} did not answer: {e}")))?;
        if status != 200 {
            return Err(Fail(
                502,
                format!("{addr} refused its admin token ({status})"),
            ));
        }
        self.write_dir().set_node(name, node).map_err(internal)?;
        Ok(Response::json(201, format!("{{\"node\":{}}}", quote(name))))
    }

    fn remove_node(&self, name: &str) -> Outcome<Response> {
        let mut dir = self.write_dir();
        if dir.node(name).is_none() {
            return Err(Fail(404, format!("no node `{name}`")));
        }
        dir.remove_node(name).map_err(|e| Fail(409, message(&e)))?;
        Ok(Response::empty(204))
    }

    /// Marks a tenant busy for the length of an operation.
    fn claim(&self, tenant: &str) -> Outcome<Claim<'_>> {
        let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        if !busy.insert(tenant.to_string()) {
            return Err(Fail(
                409,
                format!("another operation on `{tenant}` is in progress"),
            ));
        }
        Ok(Claim {
            router: self,
            tenant: tenant.to_string(),
        })
    }

    /// The node with the fewest bytes on disk among those that answer.
    /// Disk rather than memory: memory counts only the open tenants, and
    /// which ones are open is a matter of who asked last.
    fn least_loaded(&self) -> Outcome<String> {
        let nodes: Vec<(String, Node)> = self
            .read_dir()
            .nodes()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if nodes.is_empty() {
            return Err(Fail(
                409,
                "no nodes: add one with PUT /_shard/nodes/<name>".into(),
            ));
        }
        let mut best: Option<(u64, String)> = None;
        for (name, node) in nodes {
            let Ok((200, body)) =
                self.pool
                    .call(&node.addr, "GET", "/_admin/stats", &node.token, b"")
            else {
                continue;
            };
            let disk = fenec_core::json::parse_object(&String::from_utf8_lossy(&body))
                .ok()
                .and_then(|f| {
                    f.into_iter().find_map(|(k, v)| match (k.as_str(), v) {
                        ("disk", Value::Int(n)) => Some(n.max(0) as u64),
                        _ => None,
                    })
                });
            let Some(disk) = disk else { continue };
            if best.as_ref().is_none_or(|(d, _)| disk < *d) {
                best = Some((disk, name));
            }
        }
        best.map(|(_, n)| n)
            .ok_or_else(|| Fail(503, "no node answered".into()))
    }

    fn node(&self, name: &str) -> Outcome<Node> {
        self.read_dir()
            .node(name)
            .cloned()
            .ok_or_else(|| Fail(404, format!("no node `{name}`")))
    }

    fn create(&self, tenant: &str, node: Option<&str>) -> Outcome<Response> {
        fenec_http::tenants::check_name(tenant).map_err(|r| Fail(r.0, r.1))?;
        let _claim = self.claim(tenant)?;
        if self.read_dir().placement(tenant).is_some() {
            return Err(Fail(409, format!("tenant `{tenant}` already exists")));
        }
        let name = match node {
            Some(n) => n.to_string(),
            None => self.least_loaded()?,
        };
        let n = self.node(&name)?;
        let target = format!("/_admin/tenants/{tenant}");
        expect(
            self.pool.call(&n.addr, "PUT", &target, &n.token, b""),
            201,
            &name,
        )?;
        if let Err(e) = self.write_dir().place(tenant, &name, State::Active) {
            // Not recorded means not reachable: take the file back off the
            // node rather than leave an orphan there.
            let _ = self.pool.call(&n.addr, "DELETE", &target, &n.token, b"");
            return Err(internal(e));
        }
        Ok(Response::json(
            201,
            format!("{{\"tenant\":{},\"node\":{}}}", quote(tenant), quote(&name)),
        ))
    }

    fn delete(&self, tenant: &str) -> Outcome<Response> {
        let _claim = self.claim(tenant)?;
        let placement = self
            .read_dir()
            .placement(tenant)
            .cloned()
            .ok_or_else(|| Fail(404, format!("no tenant `{tenant}` in the directory")))?;
        let n = self.node(&placement.node)?;
        let (status, body) = self
            .pool
            .call(
                &n.addr,
                "DELETE",
                &format!("/_admin/tenants/{tenant}"),
                &n.token,
                b"",
            )
            .map_err(|e| {
                Fail(
                    502,
                    format!("node `{}` did not answer: {e}", placement.node),
                )
            })?;
        // 404 from the node: already gone there, which is what was asked.
        if status != 204 && status != 404 {
            return Err(Fail(status, String::from_utf8_lossy(&body).into_owned()));
        }
        self.write_dir().remove_tenant(tenant).map_err(internal)?;
        Ok(Response::empty(204))
    }

    /// Moves a tenant: freeze on the source, copy the image, install it on
    /// the target, flip the directory, delete the source copy.
    ///
    /// Reads keep being served by the source the whole time -- the directory
    /// still points there until the flip -- and writes get 503 with
    /// `Retry-After` from the frozen source. Nothing is refused by the
    /// router itself, so there is no window where the tenant is nowhere.
    fn relocate(&self, tenant: &str, to: &str) -> Outcome<Response> {
        let _claim = self.claim(tenant)?;
        let started = Instant::now();
        let from = self
            .read_dir()
            .placement(tenant)
            .map(|p| p.node.clone())
            .ok_or_else(|| Fail(404, format!("no tenant `{tenant}` in the directory")))?;
        if from == to {
            return Err(Fail(409, format!("tenant `{tenant}` is already on `{to}`")));
        }
        let src = self.node(&from)?;
        let dst = self.node(to)?;
        let base = format!("/_admin/tenants/{tenant}");

        self.write_dir()
            .place(tenant, &from, State::Moving)
            .map_err(internal)?;
        let thaw = |why: Fail| -> Fail {
            let _ = self
                .pool
                .call(&src.addr, "POST", &format!("{base}/thaw"), &src.token, b"");
            let _ = self.write_dir().place(tenant, &from, State::Active);
            why
        };

        let freeze = self.pool.call(
            &src.addr,
            "POST",
            &format!("{base}/freeze"),
            &src.token,
            b"",
        );
        expect(freeze, 200, &from).map_err(thaw)?;
        let image = expect(
            self.pool
                .call(&src.addr, "GET", &format!("{base}/file"), &src.token, b""),
            200,
            &from,
        )
        .map_err(thaw)?;

        let put = |image: &[u8]| {
            self.pool
                .call(&dst.addr, "PUT", &format!("{base}/file"), &dst.token, image)
        };
        let mut installed = put(&image);
        // 409: a copy left on the target by a move that died before the
        // flip. The directory still says `from`, so that copy is stale.
        if matches!(installed, Ok((409, _))) {
            let _ = self.pool.call(&dst.addr, "DELETE", &base, &dst.token, b"");
            installed = put(&image);
        }
        expect(installed, 201, to).map_err(thaw)?;

        if let Err(e) = self.write_dir().place(tenant, to, State::Active) {
            let _ = self.pool.call(&dst.addr, "DELETE", &base, &dst.token, b"");
            return Err(thaw(internal(e)));
        }

        // From here the tenant is on `to`. The source copy ends its streams
        // as it goes, and the clients reconnect through the router.
        let cleanup = self.pool.call(&src.addr, "DELETE", &base, &src.token, b"");
        let left = !matches!(cleanup, Ok((204, _)) | Ok((404, _)));
        if left {
            fenec_http::log!(
                "tenant `{tenant}` moved to `{to}`, but its copy on `{from}` could not be \
                 removed; it is frozen there and no longer routed to"
            );
        }
        Ok(Response::json(
            200,
            format!(
                "{{\"tenant\":{},\"from\":{},\"to\":{},\"bytes\":{},\"ms\":{},\"source_removed\":{}}}",
                quote(tenant),
                quote(&from),
                quote(to),
                image.len(),
                started.elapsed().as_millis(),
                !left
            ),
        ))
    }
}

struct Claim<'a> {
    router: &'a Router,
    tenant: String,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.router
            .busy
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.tenant);
    }
}

/// The answer a node gave, if it is the expected status; its body otherwise
/// becomes the error.
fn expect(got: std::io::Result<(u16, Vec<u8>)>, want: u16, node: &str) -> Outcome<Vec<u8>> {
    match got {
        Ok((status, body)) if status == want => Ok(body),
        Ok((status, body)) => Err(Fail(
            if status >= 500 { 502 } else { status },
            format!("node `{node}`: {}", String::from_utf8_lossy(&body)),
        )),
        Err(e) => Err(Fail(502, format!("node `{node}` did not answer: {e}"))),
    }
}

/// Headers that describe one connection, not the message: never forwarded.
fn hop_by_hop(name: &str) -> bool {
    [
        "connection",
        "keep-alive",
        "transfer-encoding",
        "te",
        "trailer",
        "upgrade",
        "proxy-authorization",
        "proxy-authenticate",
        "content-length",
    ]
    .iter()
    .any(|h| name.eq_ignore_ascii_case(h))
}

fn body_fields(req: &Request) -> std::result::Result<Vec<(String, Value)>, String> {
    let text = std::str::from_utf8(&req.body).map_err(|_| "the body is not UTF-8".to_string())?;
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    fenec_core::json::parse_object(text).map_err(|e| message(&e))
}

fn field<'a>(body: &'a [(String, Value)], name: &str) -> Option<&'a str> {
    body.iter().find_map(|(k, v)| match v {
        Value::Text(s) if k == name => Some(s.as_str()),
        _ => None,
    })
}

fn quote(s: &str) -> String {
    let mut out = String::new();
    fenec_core::json::escape_into(&mut out, s);
    out
}

fn message(e: &Error) -> String {
    match e {
        Error::NotFound(m)
        | Error::Exists(m)
        | Error::Type(m)
        | Error::Query(m)
        | Error::Corrupt(m)
        | Error::Io(m)
        | Error::Plugin(m)
        | Error::ReadOnly(m)
        | Error::Denied(m) => m.clone(),
    }
}

fn internal(e: Error) -> Fail {
    Fail(500, format!("directory: {}", message(&e)))
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}
