//! Where a request's time goes inside the server, phase by phase: built
//! with the `timing` feature alone (`make roundtrip-bench` builds it), and
//! without it every call here is an empty function the compiler drops, so a
//! released server carries none of it.
//!
//! A connection is a thread, so the clock is a thread-local: `begin` once
//! the request's first bytes are in, `lap` at the end of each phase -- the
//! time since the last mark goes to that phase -- and `end` once the
//! answer is written. `GET /_timing` hands back the mean of each phase over
//! the requests since `DELETE /_timing`, in microseconds.

/// A phase of one request, in the order a `/query` goes through them.
#[derive(Clone, Copy)]
pub enum Phase {
    /// The request line, the headers and the body out of the buffer.
    Http,
    /// Routing up to the statement: `/_health`, `/_metrics`, a tenant,
    /// `Fenec-After`, the token.
    Route,
    /// The body's JSON read, the statement's text and parameters out of it.
    Json,
    /// The parsed statement found by its text (`api::parse_query`'s cache),
    /// or parsed.
    Cache,
    /// `api::exactly`, a json field's lists read again as written.
    Exactly,
    /// The lock, the parameters bound and the statement run; a write's
    /// record handed to the sink.
    Execute,
    /// A write's fsync (`--sync always`), after the lock.
    Durable,
    /// The answer rendered as JSON.
    Render,
    /// The metrics, the statements' counts, the audit hook, CORS.
    Books,
    /// The answer's `writev`.
    Write,
}

const PHASES: [&str; 10] = [
    "http", "route", "json", "cache", "exactly", "execute", "durable", "render", "books", "write",
];

#[cfg(feature = "timing")]
mod on {
    use super::{Phase, PHASES};
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    static SUMS: [AtomicU64; PHASES.len()] = [const { AtomicU64::new(0) }; PHASES.len()];
    static COUNT: AtomicU64 = AtomicU64::new(0);

    thread_local! {
        static LAST: Cell<Option<Instant>> = const { Cell::new(None) };
    }

    pub fn begin() {
        LAST.with(|l| l.set(Some(Instant::now())));
    }

    pub fn lap(p: Phase) {
        LAST.with(|l| {
            if let Some(t) = l.get() {
                let now = Instant::now();
                SUMS[p as usize].fetch_add((now - t).as_nanos() as u64, Ordering::Relaxed);
                l.set(Some(now));
            }
        });
    }

    pub fn end() {
        LAST.with(|l| {
            if l.take().is_some() {
                COUNT.fetch_add(1, Ordering::Relaxed);
            }
        });
    }

    pub fn reset() {
        for s in &SUMS {
            s.store(0, Ordering::Relaxed);
        }
        COUNT.store(0, Ordering::Relaxed);
    }

    pub fn report() -> String {
        let n = COUNT.load(Ordering::Relaxed).max(1) as f64;
        let mut out = format!("{{\"requests\":{}", COUNT.load(Ordering::Relaxed));
        for (name, s) in PHASES.iter().zip(&SUMS) {
            out.push_str(&format!(
                ",\"{name}\":{:.3}",
                s.load(Ordering::Relaxed) as f64 / n / 1e3
            ));
        }
        out.push('}');
        out
    }
}

/// The request's first bytes are in.
#[inline(always)]
pub fn begin() {
    #[cfg(feature = "timing")]
    on::begin();
}

/// The time since the last mark was `p`'s.
#[inline(always)]
pub fn lap(p: Phase) {
    #[cfg(feature = "timing")]
    on::lap(p);
    #[cfg(not(feature = "timing"))]
    let _ = p;
}

/// The answer is written: one request more.
#[inline(always)]
pub fn end() {
    #[cfg(feature = "timing")]
    on::end();
}

/// `GET /_timing` and `DELETE /_timing`, where the feature is built.
#[cfg(feature = "timing")]
pub fn handle(req: &crate::http::Request) -> crate::http::Response {
    if req.method == crate::http::Method::Delete {
        on::reset();
    }
    crate::http::Response::json(200, on::report())
}

#[cfg(not(feature = "timing"))]
#[allow(dead_code)]
const _: [&str; 10] = PHASES;
