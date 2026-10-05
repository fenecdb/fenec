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
JSON, own HNSW, own SHA-256/HMAC/PBKDF2, RSA verification and
ChaCha20-Poly1305, and the client half of SCRAM `fenec import` signs in to
PostgreSQL with. That removes the upstream CVE surface entirely and puts all
of it in this repository instead. The crypto in
`crates/fenec-http/src/crypto.rs` and `crates/fenec-wire/src/client.rs` is
exactly the code that most deserves a second pair of eyes.

In scope, and worth reporting:

- Memory unsafety, panics or unbounded allocation reachable from input that
  crosses a trust boundary: the HTTP/JSON surface, the FenecQL parser, the
  on-disk file format, the SQLite and PostgreSQL readers of `fenec import`,
  and the WASM boundary.
- Anything that authenticates when it should not, or that lets a client skip
  authentication: the bearer token, a JWT and the policy it is held to.
- A non-constant-time comparison on a secret. The HTTP bearer token is compared
  in constant time on purpose; a regression there is a bug worth reporting.
- Allocation driven by an unauthenticated peer. A request's head is capped and
  its body bounded by `max_body` before it is read; a path around either is a
  vulnerability.
- A crafted `.fenec` file that does more than fail to open — the reader is
  expected to reject a damaged or hostile file, not to be exploited by it.
- Query text reaching an identifier or a value without validation, from any
  client surface.

## What is documented behaviour, not a vulnerability

These are deliberate, and README explains each one:

- **There is no TLS.** `fenec-server` speaks plain HTTP: a token and the data
  flow in the clear. On an open network it belongs behind a TLS terminator
  (nginx, Caddy, stunnel).
- **`--insecure` does what it says.** Binding to a non-loopback address without
  authentication is refused unless that flag is passed deliberately.
- **A token on the command line shows up in `ps`.** `FENEC_HTTP_TOKEN`,
  `FENEC_JWT_SECRET`, `--jwt-secret-file` and `FENEC_REPLICATION_TOKEN` exist
  for that reason.
- **`--jwt-require-exp off` takes a token with no `exp` for ever.** By
  default such a token is refused (401); turning that off is the operator's
  call, as is how far ahead `--jwt-max-age` lets an `exp` lie.
- **`--jwt-unbound-tenants` takes a token naming no tenant for every tenant**
  of a `--dir` node. By default such a token is refused there (403), and a
  token naming a tenant reaches that tenant alone.
- **A score is of the rows the request may read.** A scoped token's
  `match` takes BM25's statistics over the rows its filter selects, so its
  scores say nothing of rows it cannot read; the server's own token scores
  over the collection. The time a `match` takes still grows with every row
  holding its words, the token's or not, as an index walk does: users who
  must learn nothing of each other's rows, not even through timing, belong
  in tenants, a file each, or in collections of their own.
- **A scoped update reaches every field unless its grant names them.**
  `accounts update for app` lets the app's token `set accounts {kind:
  ...}` as freely as `{balance: ...}`; `accounts update(balance, held,
  status) for app` refuses a write changing any other field (403), by
  every route. A field list holds a token to fields, not to amounts: one
  that may move a balance may move it anywhere its rules' filters allow,
  and a journal beside it, append-only, is what a reconciliation holds it
  to.
- **`append-only` binds scoped tokens, not the server's own.** A policy's
  `<collection> append-only` refuses every update and delete a JSON Web
  Token asks for there; the `--http-token` still changes and deletes rows,
  as corrections, erasure and `@ttl` sweeps need. An app server that should
  only append is given a scoped token. Against whoever holds the server's
  token and the file, an append-only log is a sealed archive
  (`fenec archive --key-file`) kept elsewhere.
- **Two processes opening the same file corrupts it.** There is a single writer
  and no lock file; this is why everything that writes a file runs as a thread
  of one `fenec-server`.
- **A batch holds the database.** From its first write to its end every other
  writer waits on it, and readers on a batch that is still being written.
  A batch of reads alone holds the read lock: writers wait for it as for
  any read, readers go on beside it.
  An import that stops halfway cannot be rolled back.
- **The whole database is resident**, with no page cache and no eviction. An
  authenticated client can ask for work that costs memory; `--max-memory` is
  the bound, and `make memory` is how you calibrate it.
- **`md5` authentication is not supported** by `fenec import` and
  `--follow` when they sign in to PostgreSQL. SCRAM-SHA-256 or cleartext.

If you think one of these is worse than the README makes it sound, that is
worth an issue — as a documentation or design argument, not as an advisory.
