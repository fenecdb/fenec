//! fenec studio: the admin pages `fenec-server --studio` serves under
//! `/_studio/` -- a browser's view of the collections, their rows and their
//! schema, over the same HTTP surface every client uses.
//!
//! The pages hold no authority of their own. They are static files, served
//! to anyone who asks, with no token: the token is pasted into the page,
//! kept in its `sessionStorage`, and sent with each request as any client
//! sends it, so a scoped token sees in the studio exactly what it sees over
//! `fetch`. What `--studio` adds is a surface, not a power: a page on the
//! server's origin, which is why it is off unless asked for and why every
//! answer here is held to a strict content security policy -- scripts and
//! styles from this origin alone, no inline script, no framing, requests to
//! this origin and the one `--studio-connect` names.
//!
//! The files are the repository's `studio/` folder -- plain ES modules and
//! CSS, no build step -- and the HTTP client and query builder of `web/`,
//! which the studio imports as a page using `@fenecdb/web/client` would,
//! embedded with `include_bytes!` under the `studio` feature: fenec-server
//! and fenec-shard turn it on, and the shell, which reaches this crate for
//! `fenec backup`, carries none of it. They are read from outside the
//! crate's folder, as nothing publishes the crates to crates.io: the server
//! and the router are released as binaries and an image, built from the
//! repository.

use crate::http::{Method, Request, Response};

#[cfg(feature = "studio")]
macro_rules! file {
    ($path:literal, $from:literal, $type:literal) => {
        Asset {
            path: $path,
            body: include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../", $from)),
            content_type: $type,
        }
    };
}

/// Every file the studio is, by the path it is served at.
#[cfg(feature = "studio")]
pub static ASSETS: &[Asset] = &[
    file!(
        "index.html",
        "studio/index.html",
        "text/html; charset=utf-8"
    ),
    file!("app.css", "studio/app.css", "text/css; charset=utf-8"),
    file!("app.js", "studio/app.js", "text/javascript; charset=utf-8"),
    file!("dom.js", "studio/dom.js", "text/javascript; charset=utf-8"),
    file!(
        "connect.js",
        "studio/connect.js",
        "text/javascript; charset=utf-8"
    ),
    file!(
        "sidebar.js",
        "studio/sidebar.js",
        "text/javascript; charset=utf-8"
    ),
    file!(
        "grid.js",
        "studio/grid.js",
        "text/javascript; charset=utf-8"
    ),
    file!(
        "values.js",
        "studio/values.js",
        "text/javascript; charset=utf-8"
    ),
    file!(
        "edit.js",
        "studio/edit.js",
        "text/javascript; charset=utf-8"
    ),
    file!(
        "statements.js",
        "studio/statements.js",
        "text/javascript; charset=utf-8"
    ),
    file!("mark.svg", "site/mark.svg", "image/svg+xml"),
    file!(
        "plex-mono-400.woff2",
        "site/fonts/plex-mono-400-latin.woff2",
        "font/woff2"
    ),
    file!(
        "plex-mono-500.woff2",
        "site/fonts/plex-mono-500-latin.woff2",
        "font/woff2"
    ),
    file!(
        "client.js",
        "web/client.js",
        "text/javascript; charset=utf-8"
    ),
    file!(
        "builder.js",
        "web/builder.js",
        "text/javascript; charset=utf-8"
    ),
    file!("http.js", "web/http.js", "text/javascript; charset=utf-8"),
];

/// One embedded file, at `/_studio/<path>`.
pub struct Asset {
    pub path: &'static str,
    pub body: &'static [u8],
    pub content_type: &'static str,
}

/// The studio's files and the headers every answer of it carries.
pub struct Studio {
    assets: &'static [Asset],
    /// An `ETag` an asset, by its bytes: the names are not hashed (the
    /// modules import each other by name, with no build step to rename
    /// them), so a browser asks again each load and is answered 304.
    tags: Vec<String>,
    csp: String,
}

impl Studio {
    /// The studio over `assets`, its pages allowed to send requests to their
    /// own origin and to `connect` -- a router or another node, a
    /// `scheme://host[:port]` origin and nothing else, since it is written
    /// into a header.
    pub fn new(assets: &'static [Asset], connect: Option<&str>) -> Result<Studio, String> {
        let mut csp = String::from(
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self'; \
             font-src 'self'; connect-src 'self'",
        );
        if let Some(origin) = connect {
            check_origin(origin)?;
            csp.push(' ');
            csp.push_str(origin);
        }
        csp.push_str(
            "; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        );
        let tags = assets
            .iter()
            .map(|a| format!("\"{:016x}\"", fnv(a.body)))
            .collect();
        Ok(Studio { assets, tags, csp })
    }

    /// The content security policy the pages are served with.
    pub fn csp(&self) -> &str {
        &self.csp
    }
}

/// `scheme://host[:port]`: http or https, a host of letters, digits, dots,
/// dashes or a bracketed IPv6 address, an optional port, no path. A space or
/// a `;` would add a directive of its own to the policy.
fn check_origin(origin: &str) -> Result<(), String> {
    let bad = || {
        format!("--studio-connect takes an origin, `https://host[:port]` with no path: `{origin}`")
    };
    let rest = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .ok_or_else(bad)?;
    let ok = !rest.is_empty()
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'));
    if ok {
        Ok(())
    } else {
        Err(bad())
    }
}

/// FNV-1a over 64 bits: a tag, not a guard.
fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Whether `req` is the studio's: `/_studio` and everything under it,
/// answered before any token is asked for, the way `/_health` is.
pub fn is_studio(req: &Request) -> bool {
    req.segments().first() == Some(&"_studio")
}

/// The answer to a request under `/_studio`: 404 when the server was not
/// started with `--studio`, so the path says nothing more than an unknown
/// one would.
pub fn handle(studio: Option<&Studio>, req: &Request) -> Response {
    let Some(studio) = studio else {
        return Response::error(404, "the studio is off: start fenec-server with --studio");
    };
    let resp = answer(studio, req);
    guarded(resp, studio)
}

fn answer(studio: &Studio, req: &Request) -> Response {
    if !matches!(req.method, Method::Get | Method::Head) {
        return Response::error(405, "the studio's files are read with GET")
            .header("Allow", "GET, HEAD");
    }
    // `/_studio` itself: the pages' relative imports resolve against the
    // directory, so the address a person types is sent to it.
    if req.path == "/_studio" {
        return Response::empty(308).header("Location", "/_studio/");
    }
    let rest = &req.path["/_studio/".len().min(req.path.len())..];
    let name = if rest.is_empty() { "index.html" } else { rest };
    let Some(i) = studio.assets.iter().position(|a| a.path == name) else {
        return Response::error(404, "no such file in the studio");
    };
    let asset = &studio.assets[i];
    let tag = &studio.tags[i];
    let html = asset.content_type.starts_with("text/html");
    // The page is never kept: it is what a new version of the server
    // changes first, and a stale one would import modules it no longer
    // matches. The rest is kept and asked about again (`no-cache`), a 304
    // while its bytes are the same.
    let cache = if html { "no-store" } else { "no-cache" };
    if !html && req.header("if-none-match") == Some(tag.as_str()) {
        return Response::empty(304)
            .header("ETag", tag)
            .header("Cache-Control", cache);
    }
    let mut resp = Response::json(200, asset.body.to_vec());
    resp.content_type = asset.content_type;
    let resp = resp.header("Cache-Control", cache);
    if html {
        resp
    } else {
        resp.header("ETag", tag)
    }
}

/// The headers every studio answer carries, an error's too.
fn guarded(resp: Response, studio: &Studio) -> Response {
    resp.header("Content-Security-Policy", &studio.csp)
        .header("X-Frame-Options", "DENY")
        .header("X-Content-Type-Options", "nosniff")
        .header("Referrer-Policy", "no-referrer")
        .header("Cross-Origin-Opener-Policy", "same-origin")
        .header("Cross-Origin-Resource-Policy", "same-origin")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_origin_is_all_the_policy_takes() {
        for good in [
            "https://router.example.com",
            "http://127.0.0.1:8080",
            "http://[::1]:9000",
        ] {
            assert!(check_origin(good).is_ok(), "{good}");
        }
        for bad in [
            "router.example.com",
            "https://a.example; script-src *",
            "https://a.example/path",
            "https://",
            "javascript:alert(1)",
            "https://a.example 'unsafe-inline'",
        ] {
            assert!(check_origin(bad).is_err(), "{bad}");
        }
    }
}
