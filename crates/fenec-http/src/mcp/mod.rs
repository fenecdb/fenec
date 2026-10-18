//! MCP, the Model Context Protocol: fenecdb as the search and memory tool
//! an agent calls -- Claude Code, Cursor, Claude Desktop, an agent
//! framework -- rather than a retrieval pipeline fixed ahead of time.
//!
//! The revision implemented is **2026-07-28**
//! (<https://modelcontextprotocol.io/specification/2026-07-28>), which is
//! stateless: every request names its revision and the client's
//! capabilities in `_meta`, there is no handshake and no session, and
//! `server/discover` says what is here. Beside it the handshake revisions
//! clients still speak -- 2025-11-25, 2025-06-18 and 2025-03-26: an
//! `initialize` first, then requests -- served the same way and without a
//! session, as a "dual-era" server: a request carrying `_meta`'s version
//! is the new era's, an `initialize` the old one's. The official
//! TypeScript client of October 2026 opens in the old era unless told
//! otherwise, and so do the agents built on it.
//!
//! Two transports, one answer ([`answer`]): Streamable HTTP as `POST /_mcp`
//! of `fenec-server --mcp` (and `/t/<tenant>/_mcp` of a tenant node, which a
//! router forwards as any tenant path), and stdio as `fenec mcp`
//! ([`serve_lines`] over a local file, [`bridge`] to a server's endpoint).
//! A JSON-RPC batch is refused: no revision since 2025-06-18 has one.
//!
//! **A tool call is a statement, run as `/query` runs one.** The token is
//! the request's, held to its rules by the same rewrite (`scoped`), so a
//! scoped token's agent sees exactly what its `/query` sees; a write goes
//! through `/query`'s own path -- read-only mode, the tenant's quotas, the
//! memory ceiling, the fsync -- and the tools that write are off unless the
//! server is started with `--mcp-write` (`--mcp-memory` for the memory
//! tools alone). A value is never spliced into a statement's text: a
//! filter is parsed on its own and refused unless it is a condition and
//! nothing more, names are checked against FenecQL's rule, and every value
//! is a parameter. An answer past `--mcp-max-rows` or `--mcp-max-bytes` is
//! refused with what to narrow, never cut: a model told nothing of the cut
//! takes the half answer for the whole.
//!
//! What was left out: resources and prompts. A collection as a resource is
//! what `schema` answers already, and the clients that matter here put
//! tools in front of the model and resources behind a person's click; a
//! prompt is a recipe the docs give. Neither is advertised, so no client
//! asks.

pub mod bridge;
mod tools;

pub use tools::memory_schema;

use crate::access::Who;
use crate::api;
use crate::http::{Method, Request, Response};
use crate::Config;
use fenec_core::json;
use fenec_core::prelude::*;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

/// The stateless revision: a request carrying it in `_meta` is served by
/// it alone.
pub const MODERN: &str = "2026-07-28";

/// The handshake revisions, newest first: an `initialize` naming one of
/// them is answered with it, one naming another with the first.
pub const LEGACY: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];

/// Every revision served, as `server/discover` and a version refused list
/// them.
const SUPPORTED: &str = r#"["2026-07-28","2025-11-25","2025-06-18","2025-03-26"]"#;

// JSON-RPC's codes, and the three the 2026-07-28 revision defines.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const HEADER_MISMATCH: i64 = -32020;
const UNSUPPORTED_VERSION: i64 = -32022;

/// What `--mcp` and its options turn on.
#[derive(Clone, Debug)]
pub struct Options {
    /// The `write` tool (`--mcp-write`): put, insert, set and del, held to
    /// the token's grants. Off, no tool writes but the memory ones.
    pub write: bool,
    /// The collection `remember`, `recall` and `forget` keep memories in
    /// (`--mcp-memory <collection>`); `None` lists none of them.
    pub memory: Option<String>,
    /// The most rows an answer holds (`--mcp-max-rows`).
    pub max_rows: usize,
    /// The most bytes an answer's JSON takes (`--mcp-max-bytes`).
    pub max_bytes: usize,
    /// Tool calls a second a caller may make, a bucket a second deep
    /// (`--mcp-calls-per-sec`); 0 counts none.
    pub calls_per_sec: u32,
}

impl Default for Options {
    /// A hundred rows and 64 KB: about 16 000 tokens of JSON, inside the
    /// 25 000 tokens Claude Code takes from a tool before it warns, and a
    /// page of search results with room to spare. Twenty calls a second is
    /// past what an agent's turn makes and short of a loop gone wrong.
    fn default() -> Options {
        Options {
            write: false,
            memory: None,
            max_rows: 100,
            max_bytes: 64 << 10,
            calls_per_sec: 20,
        }
    }
}

/// What a call runs against: the database, the server's configuration,
/// who is asking, and how a tool's statement runs.
pub struct Ctx<'a> {
    pub db: &'a Arc<RwLock<Database>>,
    pub cfg: &'a Config,
    pub who: &'a Who,
    pub opts: &'a Options,
    pub run: Run,
}

/// How a tool's statements run: a read and a write, each a function chosen
/// by who serves the session. Functions rather than a flag, so that a
/// binary that never names [`Run::served`] -- the shell's `fenec mcp` --
/// links none of `/query`'s path.
#[derive(Clone, Copy)]
pub struct Run {
    read: fn(&Ctx, Statement, &[Value]) -> std::result::Result<api::Response2, tools::Outcome>,
    write: fn(&Ctx, &str, &[Value]) -> Response,
}

impl Run {
    /// As a server runs a client's statement: pinned when long, and a
    /// write through `POST /query`'s own path.
    pub fn served() -> Run {
        Run {
            read: tools::read_served,
            write: tools::write_served,
        }
    }

    /// Over a database this process holds for one caller.
    pub fn local() -> Run {
        Run {
            read: tools::read_local,
            write: tools::write_local,
        }
    }
}

/// An answer to one message: the HTTP status it goes out with, and its
/// JSON -- none for a notification, which is answered 202 over HTTP and
/// with nothing over stdio.
pub struct Reply {
    pub status: u16,
    pub body: Option<String>,
}

impl Reply {
    fn error(status: u16, id: Option<&Value>, code: i64, message: &str) -> Reply {
        Reply::error_with(status, id, code, message, None)
    }

    fn error_with(
        status: u16,
        id: Option<&Value>,
        code: i64,
        message: &str,
        data: Option<&str>,
    ) -> Reply {
        let mut out = String::from("{\"jsonrpc\":\"2.0\"");
        // An id that could not be read is left out, as the revision has it
        // for an error answering no request.
        if let Some(id) = id {
            out.push_str(",\"id\":");
            json::value_into(&mut out, id);
        }
        out.push_str(",\"error\":{\"code\":");
        json::int_into(&mut out, code);
        out.push_str(",\"message\":");
        json::escape_into(&mut out, message);
        if let Some(d) = data {
            out.push_str(",\"data\":");
            out.push_str(d);
        }
        out.push_str("}}");
        Reply {
            status,
            body: Some(out),
        }
    }
}

/// `POST /_mcp`: the Streamable HTTP binding, one JSON-RPC message a
/// request, answered with one JSON object -- never a stream, since no tool
/// here has anything to say before its answer.
pub(crate) fn http(db: &Arc<RwLock<Database>>, cfg: &Config, req: &Request) -> Response {
    let (opts, who) = match admit(cfg, req) {
        Ok(admitted) => admitted,
        Err(refused) => return refused,
    };
    let Ok(text) = std::str::from_utf8(&req.body) else {
        let r = Reply::error(400, None, PARSE_ERROR, "the body is not UTF-8");
        return Response::json(400, r.body.unwrap_or_default());
    };
    let ctx = Ctx {
        db,
        cfg,
        who: &who,
        opts,
        run: Run::served(),
    };
    let reply = answer(&ctx, text, Some(req));
    match reply.body {
        // A notification, 202; a client's response, which nothing here
        // asked for, 400.
        None => Response::empty(reply.status),
        Some(body) => Response::json(reply.status, body),
    }
}

/// What the endpoint refuses a request for before it reads its body, in
/// the order the binding asks: MCP off, where the request comes from, its
/// method, its token. Apart from [`http`] so that the connection asks it
/// before a request waits for anything (`Fenec-After`): a request refused
/// here is refused at once, not after the wait.
pub(crate) fn admit<'a>(
    cfg: &'a Config,
    req: &Request,
) -> std::result::Result<(&'a Options, Who), Response> {
    let Some(opts) = cfg.mcp.as_deref() else {
        return Err(Response::error(
            404,
            "MCP is off: start the server with --mcp",
        ));
    };
    // Before the token, as the binding asks of every connection: a page
    // on another origin, or one a DNS name was rebound under, is refused
    // whatever it sends.
    if let Some(why) = foreign(cfg, req) {
        let body = Reply::error(403, None, INVALID_REQUEST, &why).body;
        return Err(Response::json(403, body.unwrap_or_default()));
    }
    match req.method {
        Method::Post => {}
        // No stream to open with a GET and no session to end with a
        // DELETE: 2026-07-28 has neither, and a 2025 client takes 405 for
        // "this server keeps none".
        _ => {
            return Err(
                Response::error(405, "the MCP endpoint takes one JSON-RPC message a POST")
                    .header("Allow", "POST"),
            )
        }
    }
    Ok((opts, crate::authenticate(cfg, req)?))
}

/// Why a request is refused for where it comes from, if it is: an
/// `Origin` that is neither the server's own nor the one `--http-cors`
/// lets in, or -- on a server that asks for no token -- a `Host` that names
/// something other than this machine. A page whose name was rebound to
/// 127.0.0.1 sends its own name as `Host` and as its origin, which agree:
/// the origin check alone lets it in, and only a server with a token keeps
/// it out without this. An address written as one cannot be rebound.
fn foreign(cfg: &Config, req: &Request) -> Option<String> {
    let open = cfg.token.is_none() && cfg.access.is_none();
    let host = req.header("host").map(str::trim);
    if open {
        if let Some(h) = host {
            if !local_host(h) {
                return Some(format!(
                    "Host `{h}` is not this machine's, and this server asks for no token: \
                     reach it as localhost or by its address, or give it --http-token"
                ));
            }
        }
    }
    let origin = req.header("origin")?.trim();
    // The origin `--http-cors` lets in, as every answer's CORS says it.
    let allowed = crate::cors::allows(cfg.cors.as_deref(), req);
    let same = origin
        .split_once("://")
        .is_some_and(|(_, authority)| Some(authority) == host);
    match allowed || same {
        true => None,
        false => Some(format!(
            "Origin `{origin}` may not call this server: --http-cors names the origin a page may come from"
        )),
    }
}

/// Whether `host` (a `Host` header, port and all) names this machine or an
/// address: `localhost` and its subdomains, or an IP literal.
fn local_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        // `[::1]:8080`
        Some(rest) => {
            return rest
                .split(']')
                .next()
                .is_some_and(|ip| ip.parse::<std::net::Ipv6Addr>().is_ok())
        }
        None => host.rsplit_once(':').map_or(host, |(n, port)| {
            if port.bytes().all(|b| b.is_ascii_digit()) {
                n
            } else {
                host
            }
        }),
    };
    let name = name.trim_end_matches('.');
    name.eq_ignore_ascii_case("localhost")
        || name.to_ascii_lowercase().ends_with(".localhost")
        || name.parse::<std::net::IpAddr>().is_ok()
}

/// One JSON-RPC message, read.
struct Message {
    /// `None`: a notification.
    id: Option<Value>,
    method: Option<String>,
    params: Vec<(String, Value)>,
    /// A response from the client, to a request this server never sends.
    response: bool,
}

fn member<'a>(members: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
    members.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn read(text: &str) -> std::result::Result<Message, Reply> {
    let v = json::parse_json(text.trim()).map_err(|e| {
        let why = match e {
            Error::Query(m) => m,
            e => e.to_string(),
        };
        Reply::error(400, None, PARSE_ERROR, &format!("Parse error: {why}"))
    })?;
    let members = match v {
        Value::Object(m) => m,
        Value::List(_) => {
            return Err(Reply::error(
                400,
                None,
                INVALID_REQUEST,
                "a JSON-RPC batch: MCP takes one message at a time since 2025-06-18",
            ))
        }
        _ => {
            return Err(Reply::error(
                400,
                None,
                INVALID_REQUEST,
                "a JSON-RPC message is an object",
            ))
        }
    };
    let id = match member(&members, "id") {
        None => None,
        Some(v @ (Value::Int(_) | Value::Text(_))) => Some(v.clone()),
        Some(_) => {
            return Err(Reply::error(
                400,
                None,
                INVALID_REQUEST,
                "a request's id is a string or an integer, never null",
            ))
        }
    };
    if member(&members, "jsonrpc") != Some(&Value::Text("2.0".into())) {
        return Err(Reply::error(
            400,
            id.as_ref(),
            INVALID_REQUEST,
            "not JSON-RPC 2.0: `jsonrpc` must be \"2.0\"",
        ));
    }
    let method = match member(&members, "method") {
        None => None,
        Some(Value::Text(m)) => Some(m.clone()),
        Some(_) => {
            return Err(Reply::error(
                400,
                id.as_ref(),
                INVALID_REQUEST,
                "`method` is a string",
            ))
        }
    };
    let params = match member(&members, "params") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => {
            return Err(Reply::error(
                400,
                id.as_ref(),
                INVALID_PARAMS,
                "`params` is an object",
            ))
        }
    };
    let response = member(&members, "result").is_some() || member(&members, "error").is_some();
    Ok(Message {
        id,
        method,
        params,
        response,
    })
}

/// The era a request is served in.
#[derive(Clone, Copy, PartialEq)]
enum Era {
    /// 2026-07-28: its version and the client's capabilities in `_meta`.
    Modern,
    /// A handshake revision, by the version its client negotiated -- or,
    /// over HTTP, by its `MCP-Protocol-Version` header.
    Legacy,
}

/// Answers one JSON-RPC message, from either transport: `http` is the
/// request it came in, whose headers the 2026-07-28 binding checks against
/// the body; stdio has none.
pub fn answer(ctx: &Ctx, text: &str, http: Option<&Request>) -> Reply {
    let msg = match read(text) {
        Ok(m) => m,
        Err(reply) => return reply,
    };
    let Some(method) = &msg.method else {
        // A response: this server sends no request for one to answer.
        return match msg.response {
            true => Reply {
                status: 400,
                body: None,
            },
            false => Reply::error(
                400,
                msg.id.as_ref(),
                INVALID_REQUEST,
                "a message names its `method`",
            ),
        };
    };
    let Some(id) = &msg.id else {
        // A notification -- `notifications/initialized`, a cancel -- is
        // taken and nothing is said back: no call here runs long enough to
        // be cancelled while another is read.
        return Reply {
            status: 202,
            body: None,
        };
    };
    let meta = match member(&msg.params, "_meta") {
        Some(Value::Object(m)) => m.as_slice(),
        _ => &[],
    };
    let modern = match member(meta, "io.modelcontextprotocol/protocolVersion") {
        Some(Value::Text(v)) => Some(v.as_str()),
        Some(_) => {
            return Reply::error(
                400,
                Some(id),
                INVALID_PARAMS,
                "`_meta`'s io.modelcontextprotocol/protocolVersion is a string",
            )
        }
        None => None,
    };
    if method == "initialize" {
        return initialize(id, &msg.params);
    }
    let era = match modern {
        Some(v) => {
            if v != MODERN {
                return unsupported(id, v);
            }
            if !matches!(
                member(meta, "io.modelcontextprotocol/clientCapabilities"),
                Some(Value::Object(_))
            ) {
                return Reply::error(
                    400,
                    Some(id),
                    INVALID_PARAMS,
                    "a 2026-07-28 request names the client's capabilities in `_meta`: io.modelcontextprotocol/clientCapabilities",
                );
            }
            if let Some(req) = http {
                let name = match method.as_str() {
                    "tools/call" => match member(&msg.params, "name") {
                        Some(Value::Text(n)) => Some(n.as_str()),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(why) = mismatch(req, v, method, name) {
                    return Reply::error(400, Some(id), HEADER_MISMATCH, &why);
                }
            }
            Era::Modern
        }
        None => {
            if let Some(v) = http.and_then(|r| r.header("mcp-protocol-version")) {
                let v = v.trim();
                if v == MODERN {
                    return Reply::error(
                        400,
                        Some(id),
                        INVALID_PARAMS,
                        "a 2026-07-28 request carries its version in `_meta`: io.modelcontextprotocol/protocolVersion",
                    );
                }
                if !LEGACY.contains(&v) {
                    return unsupported(id, v);
                }
            }
            Era::Legacy
        }
    };
    dispatch(ctx, id, method, &msg.params, era)
}

/// `UnsupportedProtocolVersionError`, with every version served.
fn unsupported(id: &Value, asked: &str) -> Reply {
    let mut data = format!("{{\"supported\":{SUPPORTED},\"requested\":");
    json::escape_into(&mut data, asked);
    data.push('}');
    Reply::error_with(
        400,
        Some(id),
        UNSUPPORTED_VERSION,
        "Unsupported protocol version",
        Some(&data),
    )
}

/// What the 2026-07-28 binding's headers say against the body, where they
/// disagree: an intermediary routes by the headers and the server runs the
/// body, so the two must be one request.
fn mismatch(req: &Request, version: &str, method: &str, name: Option<&str>) -> Option<String> {
    let header = |h: &str, want: &str, decode: bool| -> Option<String> {
        let Some(got) = req.header(h) else {
            return Some(format!("Header mismatch: the {h} header is missing"));
        };
        let got = got.trim();
        let got = match decode {
            true => match decoded(got) {
                Some(v) => v,
                None => return Some(format!("Header mismatch: {h} is not valid base64")),
            },
            false => got.to_string(),
        };
        (got != want).then(|| {
            format!("Header mismatch: {h} header value '{got}' does not match body value '{want}'")
        })
    };
    header("MCP-Protocol-Version", version, false)
        .or_else(|| header("Mcp-Method", method, false))
        .or_else(|| name.and_then(|n| header("Mcp-Name", n, true)))
}

/// A header value as the binding encodes one that is not plain ASCII:
/// `=?base64?...?=` around the UTF-8 bytes.
fn decoded(v: &str) -> Option<String> {
    match v
        .strip_prefix("=?base64?")
        .and_then(|r| r.strip_suffix("?="))
    {
        Some(b64) => String::from_utf8(crate::crypto::b64_decode(b64)?).ok(),
        None => Some(v.to_string()),
    }
}

/// `initialize`, the handshake revisions' first request: the version the
/// client asked for when it is one served here, the newest one otherwise
/// -- the client goes on with it or leaves.
fn initialize(id: &Value, params: &[(String, Value)]) -> Reply {
    let asked = match member(params, "protocolVersion") {
        Some(Value::Text(v)) => v.as_str(),
        _ => "",
    };
    let version = LEGACY.iter().find(|v| **v == asked).unwrap_or(&LEGACY[0]);
    let mut body = String::from("\"protocolVersion\":");
    json::escape_into(&mut body, version);
    body.push_str(",\"capabilities\":{\"tools\":{\"listChanged\":false}},\"serverInfo\":");
    server_info(&mut body);
    body.push_str(",\"instructions\":");
    json::escape_into(&mut body, &instructions(None));
    result(id, Era::Legacy, &body)
}

fn server_info(out: &mut String) {
    out.push_str("{\"name\":\"fenecdb\",\"title\":\"fenecdb\",\"version\":");
    json::escape_into(out, fenec_core::VERSION);
    out.push('}');
}

/// A result: `body` is its members, written without the braces. In the
/// 2026-07-28 era it says it is complete and who answered.
fn result(id: &Value, era: Era, body: &str) -> Reply {
    let mut out = String::from("{\"jsonrpc\":\"2.0\",\"id\":");
    json::value_into(&mut out, id);
    out.push_str(",\"result\":{");
    out.push_str(body);
    if era == Era::Modern {
        if !body.is_empty() {
            out.push(',');
        }
        out.push_str(
            "\"resultType\":\"complete\",\"_meta\":{\"io.modelcontextprotocol/serverInfo\":",
        );
        server_info(&mut out);
        out.push('}');
    }
    out.push_str("}}");
    Reply {
        status: 200,
        body: Some(out),
    }
}

/// What the model is told of this server before its first call.
fn instructions(opts: Option<&Options>) -> String {
    let mut s = String::from(
        "fenecdb: collections of rows. Call `schema` first: it gives each collection as the \
         FenecQL that makes it -- @text fields are searched by words, @hnsw vector fields by an \
         embedding, @hash and @sorted fields filter fast. `search` ranks rows by words, by an \
         embedding (yours to compute, with the model the collection was filled with) or both; \
         `query` runs a FenecQL read (get <collection> where <condition> order <field> limit <n>, \
         aggregates, count; SQL's SELECT ... FROM ... WHERE is read too, double quotes being text \
         and a field written bare); `get` reads rows by id. Values always go in `params` as $1, \
         $2, never into the text. An answer too large is refused rather than cut: narrow the filter, ask \
         for fewer fields or rows.",
    );
    if let Some(o) = opts {
        if o.write {
            s.push_str(" `write` runs one put, insert, set or del.");
        }
        if o.memory.is_some() {
            s.push_str(
                " `remember` keeps a fact for later conversations and `recall` finds the ones \
                 that match what you ask; recall before you answer from what you know of the user.",
            );
        }
    }
    s
}

fn dispatch(ctx: &Ctx, id: &Value, method: &str, params: &[(String, Value)], era: Era) -> Reply {
    match method {
        "server/discover" if era == Era::Modern => {
            let mut body = format!("\"supportedVersions\":{SUPPORTED}");
            body.push_str(
                ",\"capabilities\":{\"tools\":{\"listChanged\":false}},\"instructions\":",
            );
            json::escape_into(&mut body, &instructions(Some(ctx.opts)));
            body.push_str(",\"ttlMs\":300000,\"cacheScope\":\"private\"");
            result(id, era, &body)
        }
        // A handshake revision's liveness check; 2026-07-28 has none.
        "ping" if era == Era::Legacy => result(id, era, ""),
        "tools/list" => {
            let mut body = String::from("\"tools\":");
            tools::list_into(&mut body, ctx.opts);
            // The list is the server's options' and changes only with a
            // restart; it is the same for every token.
            if era == Era::Modern {
                body.push_str(",\"ttlMs\":300000,\"cacheScope\":\"private\"");
            }
            result(id, era, &body)
        }
        "tools/call" => call(ctx, id, params, era),
        _ => Reply::error(
            // A 2026-07-28 client tells an unknown method from a server with
            // no MCP endpoint by the JSON-RPC body of the 404.
            if era == Era::Modern { 404 } else { 200 },
            Some(id),
            METHOD_NOT_FOUND,
            &format!("Method not found: {method}"),
        ),
    }
}

/// `tools/call`: an unknown tool or a call that is not one is a protocol
/// error, which a model can do little with; everything the tool itself
/// refuses -- an argument, a statement, a token's rules, a bound -- comes
/// back as its result with `isError`, which the model reads and corrects.
fn call(ctx: &Ctx, id: &Value, params: &[(String, Value)], era: Era) -> Reply {
    let Some(Value::Text(name)) = member(params, "name") else {
        return Reply::error(
            200,
            Some(id),
            INVALID_PARAMS,
            "tools/call names its tool: params.name",
        );
    };
    let args = match member(params, "arguments") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Object(m)) => m.as_slice(),
        Some(_) => {
            return Reply::error(
                200,
                Some(id),
                INVALID_PARAMS,
                "a tool's arguments are an object",
            )
        }
    };
    let Some(tool) = tools::Tool::named(name, ctx.opts) else {
        return Reply::error(
            200,
            Some(id),
            INVALID_PARAMS,
            &format!("Unknown tool: {name}"),
        );
    };
    let outcome = match admitted(ctx) {
        // The seconds rounded up, as `Retry-After` counts them.
        Err(wait_ms) => tools::Outcome::come_back(
            429,
            format!(
                "too many tool calls: this server takes {} a second from one caller; call again in {wait_ms} ms",
                ctx.opts.calls_per_sec
            ),
            wait_ms.div_ceil(1000),
        ),
        Ok(()) => {
            let outcome = tools::run(ctx, tool, args);
            tools::bounded(outcome, ctx.opts)
        }
    };
    counted(tool, &outcome);
    let mut body = String::new();
    outcome.result_into(&mut body);
    result(id, era, &body)
}

// ------------------------------------------------------------ rate

/// A caller's tool calls: a token bucket, refilled at the rate, holding a
/// second's worth.
struct Bucket {
    tokens: f64,
    at: Instant,
}

/// Callers past this are forgotten all at once, which forgives each once
/// rather than let the map grow with every subject a token names.
const CALLERS: usize = 4096;

static BUCKETS: Mutex<Option<std::collections::HashMap<String, Bucket>>> = Mutex::new(None);

/// Whether the caller may make one more call now, or in how many
/// milliseconds it may. A caller is a scoped token's subject, or for the
/// server's own token -- which every unscoped client shares -- the address
/// it calls from: one agent in a loop is held back, not every agent on the
/// server's token with it.
fn admitted(ctx: &Ctx) -> std::result::Result<(), u64> {
    let rate = ctx.opts.calls_per_sec;
    if rate == 0 {
        return Ok(());
    }
    let key = match ctx.who {
        Who::Full => match crate::audit::client_ip() {
            Some(ip) => format!("@{ip}"),
            None => String::new(),
        },
        Who::Scoped(s) => format!("sub:{}", s.subject().unwrap_or("")),
    };
    let key = key.as_str();
    let rate = rate as f64;
    let now = Instant::now();
    let mut held = BUCKETS.lock().unwrap_or_else(|e| e.into_inner());
    let map = held.get_or_insert_with(Default::default);
    if map.len() >= CALLERS && !map.contains_key(key) {
        map.clear();
    }
    let b = map.entry(key.to_string()).or_insert(Bucket {
        tokens: rate,
        at: now,
    });
    b.tokens = (b.tokens + now.duration_since(b.at).as_secs_f64() * rate).min(rate);
    b.at = now;
    if b.tokens >= 1.0 {
        b.tokens -= 1.0;
        return Ok(());
    }
    Err(((1.0 - b.tokens) / rate * 1000.0).ceil() as u64)
}

// ------------------------------------------------------------ counts

/// Calls by tool and by whether the tool refused. One set for the process,
/// not a shard a thread as the statements' counts are: a tool call is a
/// model's turn, thousands of times rarer than a statement, and no core
/// waits on another's cache line for it.
static CALLS: [[AtomicU64; 2]; tools::COUNT] =
    [const { [const { AtomicU64::new(0) }; 2] }; tools::COUNT];

/// Set once a server or a stdio session serves MCP: a server without it
/// writes no MCP family into `/_metrics`.
static ON: AtomicBool = AtomicBool::new(false);

/// Notes that this process serves MCP, for [`metrics`].
pub fn serving() {
    ON.store(true, Ordering::Relaxed);
}

fn counted(tool: tools::Tool, outcome: &tools::Outcome) {
    let failed = outcome.is_error();
    CALLS[tool as usize][failed as usize].fetch_add(1, Ordering::Relaxed);
    // An agent's write is named in the audit log, as an admin's change is;
    // its reads are not, as no read is.
    if tool.writes() && crate::audit::on() {
        use crate::audit::Field;
        crate::audit::event(
            "mcp",
            &[
                ("tool", Field::Text(tool.name())),
                ("failed", Field::Bool(failed)),
            ],
        );
    }
}

/// The calls' counts, in `/_metrics`: `fenec_mcp_tool_calls_total`, by
/// tool and outcome. No collection is a label: a tool's name is one of
/// eight.
pub(crate) fn metrics(out: &mut crate::metrics::Text) {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    out.family(
        "fenec_mcp_tool_calls_total",
        "counter",
        "MCP tool calls answered, by tool and by whether the tool refused (isError).",
    );
    for tool in tools::Tool::ALL {
        for (i, outcome) in ["ok", "error"].iter().enumerate() {
            out.sample(
                "fenec_mcp_tool_calls_total",
                &[("tool", tool.name()), ("outcome", outcome)],
                CALLS[tool as usize][i].load(Ordering::Relaxed),
            );
        }
    }
}

// ------------------------------------------------------------ stdio

/// The longest message a stdio session reads: the HTTP endpoint's body
/// ceiling, which a line has no other.
const LINE_MOST: usize = 64 << 20;

/// The stdio binding over a database this process holds (`fenec mcp
/// --file`): a message a line in, an answer a line out, until the input
/// ends. Notifications and a client's responses are answered with nothing.
pub fn serve_lines(
    ctx: &Ctx,
    mut input: impl BufRead,
    mut output: impl Write,
) -> std::io::Result<()> {
    serving();
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        let reply = match std::str::from_utf8(&line) {
            _ if line.len() > LINE_MOST => Reply::error(
                400,
                None,
                INVALID_REQUEST,
                &format!("a message longer than {LINE_MOST} bytes"),
            ),
            Ok(text) if text.trim().is_empty() => continue,
            Ok(text) => answer(ctx, text, None),
            Err(_) => Reply::error(400, None, PARSE_ERROR, "the line is not UTF-8"),
        };
        // What a statement noted for the HTTP endpoint's counts, which a
        // stdio session keeps none of.
        crate::statements::clear();
        if let Some(body) = reply.body {
            output.write_all(body.as_bytes())?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_host_is_this_machine_or_an_address() {
        for h in [
            "localhost",
            "localhost:8080",
            "LOCALHOST.:80",
            "app.localhost:3000",
            "127.0.0.1:8080",
            "10.0.0.7:8080",
            "[::1]:8080",
            "[fe80::1]",
        ] {
            assert!(local_host(h), "{h}");
        }
        for h in [
            "evil.example:8080",
            "evil.example",
            "localhost.evil.example",
            "127.0.0.1.nip.io:80",
        ] {
            assert!(!local_host(h), "{h}");
        }
    }

    #[test]
    fn a_header_value_in_base64_is_read_as_its_text() {
        assert_eq!(decoded("search").as_deref(), Some("search"));
        let enc = format!(
            "=?base64?{}?=",
            crate::crypto::b64_encode("Hello, 世界".as_bytes())
        );
        assert_eq!(decoded(&enc).as_deref(), Some("Hello, 世界"));
        assert_eq!(decoded("=?base64?***?="), None);
    }
}
