//! What each statement cost, by its shape: `GET /_stats/statements`, and
//! `pg_stat_statements` over the pg wire, PostgreSQL's view of that name
//! answered from the same counts.
//!
//! A statement is counted where `/_metrics` counts it, from its arrival to
//! its answer. Its shape is its text with every literal and parameter
//! written `$1`, `$2`... in order, a list of literals alone -- a vector's
//! components -- as one, and every run of whitespace as a space: a
//! statement with its values in the text is counted with the same one run
//! with parameters, and a vector does not make each query a shape of its
//! own. The shape is written into a buffer the thread keeps and hashed
//! there, so a statement seen before costs no allocation; a new one costs
//! its text once, cut at 1 000 bytes.
//!
//! The counts are held in shards a thread each, as the metrics' are, each
//! behind a mutex that only a reader of the whole takes besides its own
//! thread. Sixteen shards keep at most 5 000 shapes, pg_stat_statements'
//! default, and a full one forgets the twentieth of its shapes called least,
//! as that extension does.
//!
//! A tenant node keeps each tenant's shapes apart, by the tenant's name: a
//! statement's text holds its collections' names, which are the tenant's
//! and not the node's to publish.

use crate::http::{Method, Request, Response};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Shapes a shard keeps before it forgets the least called.
const PER_SHARD: usize = 5_000 / 16 + 1;
/// Bytes of a shape's text kept: a bulk `put` can be megabytes.
const TEXT_MAX: usize = 1_000;

/// One shape's counts.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// The tenant, on a tenant node.
    pub tenant: Option<String>,
    /// The statement's shape.
    pub text: String,
    /// Its hash, which PostgreSQL calls `queryid`.
    pub id: u64,
    pub calls: u64,
    pub errors: u64,
    /// Rows returned or changed.
    pub rows: u64,
    pub micros: u64,
    pub min_micros: u64,
    pub max_micros: u64,
}

impl Entry {
    fn add(&mut self, other: &Entry) {
        self.calls += other.calls;
        self.errors += other.errors;
        self.rows += other.rows;
        self.micros += other.micros;
        self.min_micros = self.min_micros.min(other.min_micros);
        self.max_micros = self.max_micros.max(other.max_micros);
    }
}

/// One thread's shapes -- a few threads' past sixteen. Aligned as the
/// metrics' shards are, so that no two share a cache line.
#[repr(align(128))]
struct Shard(Mutex<HashMap<u64, Entry>>);

fn shards() -> &'static [Shard; 16] {
    static SHARDS: OnceLock<[Shard; 16]> = OnceLock::new();
    SHARDS.get_or_init(|| std::array::from_fn(|_| Shard(Mutex::new(HashMap::new()))))
}

static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static MINE: usize = NEXT_SHARD.fetch_add(1, Ordering::Relaxed) % 16;
    /// The shape of the statement being counted, written here rather than
    /// into a new `String` each time.
    static SHAPE: RefCell<String> = const { RefCell::new(String::new()) };
    /// Rows the statement this thread is running returned or changed, noted
    /// where it ran and read where it is counted, as the metrics' `wrote` is.
    static ROWS: Cell<u64> = const { Cell::new(0) };
    /// The FenecQL an HTTP request carried, noted as it was parsed.
    static TEXT: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Notes the FenecQL the request this thread is serving carries --
/// `POST /query`'s statement, `POST /batch`'s one after the other -- to be
/// counted as, rather than the request's JSON, where every statement is a
/// string and the shape of each would be the same `{$1: $2}`. A statement
/// run over HTTP is counted with the same one run over the pg wire.
pub fn text(sql: &str) {
    TEXT.with(|t| {
        let mut t = t.borrow_mut();
        if !t.is_empty() {
            t.push_str("; ");
        }
        t.push_str(sql);
    });
}

/// Notes that the statement this thread is running returned or changed `n`
/// rows.
pub fn rows(n: u64) {
    ROWS.with(|r| r.set(r.get() + n));
}

/// Counts a statement: `text` as it came -- unless its FenecQL was noted
/// ([`text`]) -- and `tenant` on a tenant node.
pub fn record(tenant: Option<&str>, text: &str, took: Duration, failed: bool) {
    let rows = ROWS.with(|r| r.replace(0));
    let micros = took.as_micros().min(u64::MAX as u128) as u64;
    SHAPE.with(|buf| {
        let mut buf = buf.borrow_mut();
        buf.clear();
        TEXT.with(|noted| {
            let mut noted = noted.borrow_mut();
            shape(if noted.is_empty() { text } else { &noted }, &mut buf);
            noted.clear();
        });
        let id = hash(tenant, &buf);
        let shard = &shards()[MINE.with(|m| *m)];
        let mut map = shard.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = map.get_mut(&id) {
            e.calls += 1;
            e.errors += failed as u64;
            e.rows += rows;
            e.micros += micros;
            e.min_micros = e.min_micros.min(micros);
            e.max_micros = e.max_micros.max(micros);
            return;
        }
        if map.len() >= PER_SHARD {
            forget(&mut map);
        }
        let mut text = buf.clone();
        if text.len() > TEXT_MAX {
            let mut end = TEXT_MAX;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push_str("...");
        }
        map.insert(
            id,
            Entry {
                tenant: tenant.map(str::to_string),
                text,
                id,
                calls: 1,
                errors: failed as u64,
                rows,
                micros,
                min_micros: micros,
                max_micros: micros,
            },
        );
    });
}

/// Forgets the twentieth of a full shard's shapes called least.
fn forget(map: &mut HashMap<u64, Entry>) {
    let mut calls: Vec<u64> = map.values().map(|e| e.calls).collect();
    let at = calls.len() / 20;
    let (_, least, _) = calls.select_nth_unstable(at);
    let least = *least;
    let mut dropped = 0;
    map.retain(|_, e| {
        let keep = e.calls > least || dropped > at;
        dropped += !keep as usize;
        keep
    });
}

/// Whose shapes a reader sees.
#[derive(Clone, Copy)]
pub enum View<'a> {
    /// A server over one file: its own.
    Node,
    /// A tenant's, under `/t/<tenant>/` or over its pg connection.
    Tenant(&'a str),
    /// Every tenant's, each named: a tenant node's admin alone.
    Tenants,
}

impl View<'_> {
    fn sees(&self, tenant: Option<&str>) -> bool {
        match self {
            View::Node => tenant.is_none(),
            View::Tenant(t) => tenant == Some(*t),
            View::Tenants => tenant.is_some(),
        }
    }
}

/// Every shape `view` sees, most time first.
pub fn snapshot(view: View) -> Vec<Entry> {
    let mut all: HashMap<u64, Entry> = HashMap::new();
    for shard in shards() {
        let map = shard.0.lock().unwrap_or_else(|e| e.into_inner());
        for (id, e) in map.iter() {
            if !view.sees(e.tenant.as_deref()) {
                continue;
            }
            match all.get_mut(id) {
                Some(have) => have.add(e),
                None => {
                    all.insert(*id, e.clone());
                }
            }
        }
    }
    let mut out: Vec<Entry> = all.into_values().collect();
    out.sort_by(|a, b| b.micros.cmp(&a.micros).then_with(|| a.text.cmp(&b.text)));
    out
}

/// Forgets every shape `view` sees.
pub fn reset(view: View) {
    for shard in shards() {
        let mut map = shard.0.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, e| !view.sees(e.tenant.as_deref()));
    }
}

/// `GET /_stats/statements`, the shapes `view` sees as JSON, most time
/// first, and `DELETE`, which forgets them; `allowed` is the caller's
/// verdict on the request's token.
pub(crate) fn handle(req: &Request, view: View, allowed: bool) -> Response {
    if !allowed {
        return Response::error(401, "invalid or missing token")
            .header("WWW-Authenticate", "Bearer");
    }
    match req.method {
        Method::Delete => {
            reset(view);
            Response::empty(204)
        }
        Method::Get | Method::Head => Response::json(200, render(&snapshot(view))),
        _ => Response::error(405, "GET or DELETE"),
    }
}

fn render(entries: &[Entry]) -> String {
    let ms = |us: u64| us as f64 / 1000.0;
    let mut out = String::from("{\"statements\":[");
    for (i, e) in entries.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"query\":");
        fenec_core::json::escape_into(&mut out, &e.text);
        if let Some(t) = &e.tenant {
            out.push_str(",\"tenant\":");
            fenec_core::json::escape_into(&mut out, t);
        }
        // A string: PostgreSQL's queryid is a signed 64-bit number, past
        // what a JSON reader's double holds exactly.
        let _ = write!(
            out,
            ",\"queryid\":\"{}\",\"calls\":{},\"rows\":{},\"errors\":{},\"total_ms\":{},\"mean_ms\":{},\"min_ms\":{},\"max_ms\":{}}}",
            e.id as i64,
            e.calls,
            e.rows,
            e.errors,
            ms(e.micros),
            ms(e.micros) / e.calls.max(1) as f64,
            ms(e.min_micros),
            ms(e.max_micros),
        );
    }
    out.push_str("]}");
    out
}

/// FNV-1a over the tenant and the shape: 64 bits, as PostgreSQL's `queryid`.
fn hash(tenant: Option<&str>, shape: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |b: u8| {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    };
    if let Some(t) = tenant {
        t.bytes().for_each(&mut eat);
        eat(0xff);
    }
    shape.bytes().for_each(&mut eat);
    h
}

/// Writes `text`'s shape into `out`: literals, parameters, and lists of
/// literals alone as `$n`, whitespace runs as a space.
pub fn shape(text: &str, out: &mut String) {
    let b = text.as_bytes();
    let mut n = 0u32;
    let mut i = 0;
    let mut hole = |out: &mut String| {
        n += 1;
        out.push('$');
        out.push_str(itoa(n, &mut [0u8; 10]));
    };
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if !out.is_empty() && i < b.len() {
                out.push(' ');
            }
            continue;
        }
        if c == b'\'' || c == b'"' {
            i = string_end(b, i);
            hole(out);
            continue;
        }
        if c == b'$' && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
            i += 1;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            hole(out);
            continue;
        }
        if c == b'[' {
            if let Some(end) = literals_until(b, i) {
                i = end;
                hole(out);
                continue;
            }
        }
        if number_starts(b, i, out) {
            i = number_end(b, i);
            hole(out);
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let from = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let word = &text[from..i];
            match ["true", "false", "null"]
                .iter()
                .any(|k| word.eq_ignore_ascii_case(k))
            {
                true => hole(out),
                false => out.push_str(word),
            }
            continue;
        }
        // A byte of a character past ASCII goes through as it is, one byte
        // at a time; the shape stays valid UTF-8, since it is copied whole.
        let len = utf8_len(c);
        out.push_str(&text[i..(i + len).min(b.len())]);
        i += len;
    }
}

/// `n` in decimal, written into `buf`.
fn itoa(mut n: u32, buf: &mut [u8; 10]) -> &str {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    std::str::from_utf8(&buf[at..]).unwrap_or("0")
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xf0..=0xff => 4,
        0xe0..=0xef => 3,
        0xc0..=0xdf => 2,
        _ => 1,
    }
}

/// Past the string that starts at `at`: `"..."` with backslash escapes,
/// `'...'` with a doubled quote for one.
fn string_end(b: &[u8], at: usize) -> usize {
    let q = b[at];
    let mut i = at + 1;
    while i < b.len() {
        if q == b'"' && b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == q {
            if q == b'\'' && b.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

/// Whether a number starts at `at`: a digit, or a sign or a point before
/// one where no name or value ends just before it -- `col1` is a name, and
/// `a -1` a subtraction only after a value.
fn number_starts(b: &[u8], at: usize, before: &str) -> bool {
    let digit = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
    let after_value = || {
        before
            .bytes()
            .next_back()
            .is_some_and(|p| p.is_ascii_alphanumeric() || matches!(p, b'_' | b')' | b']'))
    };
    match b[at] {
        c if c.is_ascii_digit() => {
            at == 0 || !(b[at - 1].is_ascii_alphanumeric() || b[at - 1] == b'_')
        }
        b'.' => digit(at + 1) && !after_value(),
        b'-' | b'+' => {
            (digit(at + 1) || (b.get(at + 1) == Some(&b'.') && digit(at + 2))) && !after_value()
        }
        _ => false,
    }
}

/// Past the number that starts at `at`.
fn number_end(b: &[u8], at: usize) -> usize {
    let mut i = at;
    if matches!(b[i], b'-' | b'+') {
        i += 1;
    }
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
        i += 1;
    }
    if i < b.len() && matches!(b[i], b'e' | b'E') {
        let mut j = i + 1;
        if j < b.len() && matches!(b[j], b'+' | b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            i = j;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    i
}

/// Past the `]` closing the list that opens at `at`, when all it holds is
/// literals: a vector, or `in [1, 2, 3]`. `None` for a list of anything
/// else, whose shape is written out.
fn literals_until(b: &[u8], at: usize) -> Option<usize> {
    let mut i = at + 1;
    let mut any = false;
    while i < b.len() {
        let c = b[i];
        if c == b']' {
            return any.then_some(i + 1);
        }
        if c.is_ascii_whitespace() || c == b',' {
            i += 1;
            continue;
        }
        if c == b'\'' || c == b'"' {
            i = string_end(b, i);
            any = true;
            continue;
        }
        if c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.') {
            let end = number_end(b, i);
            if end == i {
                return None;
            }
            i = end;
            any = true;
            continue;
        }
        return None;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(text: &str) -> String {
        let mut out = String::new();
        shape(text, &mut out);
        out
    }

    #[test]
    fn values_and_parameters_are_one_shape() {
        let a = of("get articles where year = 2024 and title = \"rust\" limit 10");
        let b = of("get articles where year = $1 and title = $2 limit $3");
        assert_eq!(a, "get articles where year = $1 and title = $2 limit $3");
        assert_eq!(a, b);
        assert_eq!(
            of("select * from t where name = 'O''Brien' and x > -1.5e3"),
            "select * from t where name = $1 and x > $2"
        );
        assert_eq!(
            of("get t where flag = TRUE or x = null"),
            "get t where flag = $1 or x = $2"
        );
    }

    #[test]
    fn a_vector_is_one_value_and_names_keep_their_digits() {
        assert_eq!(
            of("get d near embed [0.1, -0.2, 3e-4] limit 5"),
            "get d near embed $1 limit $2"
        );
        assert_eq!(of("get d where id in [1, 2, 3]"), "get d where id in $1");
        assert_eq!(
            of("get col1 select x2 where a-1 > b"),
            "get col1 select x2 where a-$1 > b"
        );
        assert_eq!(of("put d [{a: 1}, {a: 2}]"), "put d [{a: $1}, {a: $2}]");
        assert_eq!(of("  get   t\n\twhere  x = 1  "), "get t where x = $1");
        assert_eq!(
            of("get t where name = \"ç\\\"x\" and ü = 1"),
            "get t where name = $1 and ü = $2"
        );
    }

    #[test]
    fn counts_merge_by_shape_and_tenant() {
        let t = Some("shape-test-tenant");
        let view = View::Tenant("shape-test-tenant");
        reset(view);
        record(t, "get a where x = 1", Duration::from_micros(10), false);
        rows(3);
        record(t, "get a where x = 22", Duration::from_micros(30), false);
        record(t, "get b", Duration::from_micros(5), true);
        std::thread::spawn(move || {
            record(t, "get a where x = $1", Duration::from_micros(20), false)
        })
        .join()
        .unwrap();
        let got = snapshot(view);
        assert_eq!(got.len(), 2);
        let a = &got[0];
        assert_eq!(a.text, "get a where x = $1");
        assert_eq!(
            (a.calls, a.rows, a.micros, a.min_micros, a.max_micros),
            (3, 3, 60, 10, 30)
        );
        assert_eq!((got[1].text.as_str(), got[1].errors), ("get b", 1));
        assert!(snapshot(View::Tenant("another-tenant")).is_empty());
        assert!(snapshot(View::Node).iter().all(|e| e.tenant.is_none()));
        reset(view);
        assert!(snapshot(view).is_empty());
    }

    #[test]
    fn a_full_shard_forgets_the_least_called() {
        let mut map: HashMap<u64, Entry> = HashMap::new();
        for i in 0..100u64 {
            let e = Entry {
                tenant: None,
                text: i.to_string(),
                id: i,
                calls: i + 1,
                errors: 0,
                rows: 0,
                micros: 0,
                min_micros: 0,
                max_micros: 0,
            };
            map.insert(i, e);
        }
        forget(&mut map);
        assert!(map.len() < 100 && map.len() >= 90, "{}", map.len());
        assert!(map.contains_key(&99) && !map.contains_key(&0));
    }
}
