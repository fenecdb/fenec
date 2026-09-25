# Security policy

## Reporting a vulnerability

Report it privately, through GitHub:
**https://github.com/fenecdb/fenec/security/advisories/new**

That form is visible only to the maintainers. Please do not open a public issue
for a vulnerability, and please do not post a working exploit anywhere public
before a fix is out.

Useful in a report: the version or commit, the surface (wire protocol, HTTP,
file format, parser, WASM), and the smallest input that triggers it. If it
needs a crafted file or packet, attach it rather than describing it.

This is a small project. Expect an acknowledgement within a few days rather
than within hours, and no bounty.

## Supported versions

The latest release and `main`. Older tags get no backported fixes.

## Where the risk actually is

fenecdb takes **no external crates** in the shipped crates — own codec, own
JSON, own HNSW, own SCRAM, own SHA-256/HMAC/PBKDF2. That removes the upstream
CVE surface entirely and puts all of it in this repository instead. The crypto
in `crates/fenec-pg/src/{scram.rs,crypto.rs}` is exactly the code that most
deserves a second pair of eyes.

In scope, and worth reporting:

- Memory unsafety, panics or unbounded allocation reachable from input that
  crosses a trust boundary: the PostgreSQL v3 wire protocol, the HTTP/JSON
  surface, the FenecQL parser, the on-disk file format, the SQLite reader, and
  the WASM boundary.
- Anything that authenticates when it should not, or that lets a client skip
  authentication. Including flaws in the SCRAM-SHA-256 exchange itself.
- A non-constant-time comparison on a secret. The HTTP bearer token is compared
  in constant time on purpose; a regression there is a bug worth reporting.
- Allocation driven by an unauthenticated peer. The `fenec-pg` startup packet
  is capped at 10 000 bytes precisely because it is the only allocation before
  authentication; a path around that cap is a vulnerability.
- A crafted `.fenec` file that does more than fail to open — the reader is
  expected to reject a damaged or hostile file, not to be exploited by it.
- Query text reaching an identifier or a value without validation, from any
  client surface.

## What is documented behaviour, not a vulnerability

These are deliberate, and README explains each one:

- **There is no TLS.** `SSLRequest` is refused with `N`, in both `fenec-pg` and
  `fenec-http`. The SCRAM exchange protects the password; the data flows in the
  clear. On an open network it belongs behind a TLS terminator (stunnel,
  `nginx stream`).
- **`--insecure` does what it says.** Binding to a non-loopback address without
  authentication is refused unless that flag is passed deliberately.
- **A password on the command line shows up in `ps`.** `--password-file` and
  `FENECPG_PASSWORD` exist for that reason.
- **Two processes opening the same file corrupts it.** There is a single writer
  and no lock file; this is why `fenec-http` is a second listener inside
  `fenec-pg` rather than its own binary.
- **A transaction holds the database.** From its first write to its end every
  other session waits on it, so a client that stops mid-transaction stalls the
  server until `--idle-in-transaction-timeout` (10 s by default) puts it back.
  An import that stops halfway cannot be rolled back.
- **The whole database is resident**, with no page cache and no eviction. An
  authenticated client can ask for work that costs memory; `--max-memory` is
  the bound, and `make memory` is how you calibrate it.
- **`md5` authentication is not supported**, by `fenec-pg` or by
  `fenec import`. SCRAM-SHA-256 or cleartext.

If you think one of these is worse than the README makes it sound, that is
worth an issue — as a documentation or design argument, not as an advisory.
