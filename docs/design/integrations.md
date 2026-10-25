# Integrations and deploy

The long form of the invariants [CLAUDE.md](../../CLAUDE.md) lists under *Integrations and deploy*: `integrations/`, the Grafana data source, the desktop app and the `deploy/` kit. Each section below is a paragraph moved word for word from CLAUDE.md, which keeps its rule in a few sentences and links here; "above" and "below" in them refer to the order the invariants had there, which [README.md](README.md#the-original-order) lists.

## Integrations

**`integrations/` may use outside packages; the crates may not.** The
LangChain and LlamaIndex vector stores (`integrations/python`, one package,
the standard library for its client) and `useLiveQuery`
(`integrations/react`) are held to their frameworks' own tests, and each
language's docs example to a real server (`integrations/languages`: the
same flow over HTTP in Python, JavaScript, Go, C#, Java, PHP, Ruby and
Rust, each with its own HTTP client) -- `make python-test` runs LangChain's
standard suite and the tests LlamaIndex's integrations run from a
`python:3.13` container against a fenec-server started here, `make
languages-test` the eight examples (Python, Java, PHP and Ruby from their
images, the others with the toolchain on the machine), `make react-test`
runs the hook against a real replica, and CI runs all three
(`integrations`). The Go and .NET SDKs (`integrations/go`, module
`github.com/fenecdb/fenecdb/integrations/go`, package `fenecdb`;
`integrations/dotnet`, NuGet's `FenecDb`, `net8.0`) are the standard
library alone, the Python client's surface -- a statement and its rows,
typed rows, writes with their `Fenec-Seq`, `/batch`, an idempotency key,
`After` for a replica, a subscription, `/_changes`, a tenant, `/_health`,
a typed error -- and write a `float32` as the shortest text that reads back
as it through an `f64`, which is how the server reads a number, but for
`json::TIE`, which goes as its `f64`'s text: `encoding/json` wrote its
shortest `f32` text and the server stored the float above. A request is
bounded through its context or token, never the HTTP client's timeout,
which would cut a subscription short. `make go-test` and `make
dotnet-test` start the servers they need (`FENEC_SERVER`, else
`target/debug/fenec-server`), CI runs both, and `make languages-test`
runs the docs' Go and C# examples through each SDK too (`go-sdk`,
`dotnet-sdk`). Python, Go and .NET have the JS builder too
(`fenecdb/builder.py`'s `db.collection(...)`, Go's `db.From(...)` and
.NET's `db.From(...)`), and all four make the same text of the same chain,
to the byte, refusals by the same message: `integrations/builder-golden.json`
holds the chains (`{op, args}`, the JS builder's names, what JSON has no
word for an object of one `$` key) and what the JS builder made of each --
`web/golden.mjs` holds the chains and writes the file (`make
builder-golden`), `web/fenec.test.js` runs every case through the JS
builder again so the file cannot drift, and `make python-test`, `go-test`
and `dotnet-test` run every case through their own builder, the
endpoints' statements read off a recording transport. A builder change
starts in `web/builder.js` and a case in `golden.mjs`, then the three
follow, and the Swift, Kotlin and Dart builders with them. Go's object conditions are `Fields`/`Ops` and a document a
`D(...)`, names and values in turn, since a map has no order and the order
is the text's; its options are functional, since a struct cannot tell
`limit 0` from none. CI also builds the five packages as a release
publishes them and installs and uses them (`integrations/packages.sh`):
PyPI's `fenecdb`, npm's `@fenecdb/web` -- the client, the module and
`collate/`, `web/package.json`, and no test build -- `@fenecdb/react`,
`@fenecdb/cloudflare` and `@fenecdb/langchain`. They go out when a
release's draft is published (`packages.yml`), only after that same check,
with a token where the registry's secret holds one (`NPM_TOKEN`,
`PYPI_API_TOKEN`), with the workflow's OIDC identity (trusted publishing)
where it does not (RELEASING.md). NuGet's `FenecDb` is packed, installed
into a fresh console app and used against a server
(`integrations/dotnet/package.sh`) and only then pushed with
`NUGET_API_KEY`, its job skipped with a notice without the secret; the Go
module has no registry, and `release.yml` pushes the tag
`integrations/go/vX.Y.Z` it is fetched by beside `vX.Y.Z`. With
`full_text=True` a store indexes its text for BM25 as well and searches by
the words (`match`) or by the words and the vector fused (`fuse`):
LlamaIndex's `TEXT_SEARCH` and `HYBRID`, LangChain's `mode="text"` and
`"hybrid"`. `alpha` is not read, since `fuse` adds ranks; `quant` passes
`int8` or `bit` codes to `@hnsw`. The same store for LangChain.js
(`integrations/langchain`, `@fenecdb/langchain`) runs over anything with
`run(sql, params)` -- a `Fenec` keeps a RAG index in the page -- and is
held, LangChain.js publishing no standard suite, to what its own vector
store integrations are tested for, over a database in the page and over a
fenec-server's HTTP endpoint (`make langchain-test`); the Vercel AI SDK has no
store interface, so `integrations/ai-sdk/rag.js` is three functions to copy,
held to the SDK's mock models (`make ai-sdk-test`). `web/fenec.d.ts` is
held to `web/fenec.js` by `make types-check`: every export and class method
read at run time and touched from a generated file, and a caller's code with
the lines the types must refuse, under `tsc --strict` -- the module stays
JS with no build step. A store names
its collection and metadata columns in the statement's text, so both are
checked against FenecQL's name pattern; values always go in as parameters,
and `in` takes one per element (`in [$2, $3]`), since a parameter binds a
value and not a list.

## Grafana

**Grafana's data source binds a dashboard's values; it never splices them**
(`integrations/grafana`, plugin id `fenecdb-datasource`). The Infinity
plugin wrote variables into a body's text, sorted a frame's columns by name
and alerted on nothing a browser fills in, so a backend plugin (Go,
grafana-plugin-sdk-go 0.297, the Go SDK through `replace ../go`) and a
TypeScript frontend in `@grafana/create-plugin` 7.12.2's layout run the
queries server side. The backend reads a statement only where it must agree
with fenec-ql's lexer -- strings, comments, words, a `$` (`fenecql.go`) --
and makes each name a `$n` after the editor's parameter list: `$__from`,
`$__to`, `$__interval_ms`, `$__max_data_points` from the request, which an
alert rule has too, and `$sym` or `${n:int}` from the values the browser
sends beside the query (`values.ts`), a multi-value one after `in` a bracket
of them. Only `get`, `select` and `explain` are sent. The answer is read in
its order (`answer.go`: encoding/json's maps lose it, so the SDK grew
`QueryRaw`/`BatchRaw` and `IfNoneMatch`) and typed by the
`collections` schema, kept 30 s, where a column is a field and by its values
where computed; a group by time and a text column is a series a label (long
to wide), which a rule evaluates an instance a symbol. One query goes with
its last answer's `ETag` and a 304 is answered from the kept body: 3-5 ms
with the server's token against 10-82 ms run, nothing gained with a scoped
JWT, whose tag the server makes by running the query (`make grafana-check`'s
stack, `monitoring/grafana/bench.mjs`); several are one read-only `/batch`.
The client follows no redirect and sends the token alone, to the configured
URL and the plugin's paths. `monitoring/` builds Grafana with it
(`integrations/grafana/Dockerfile`), and `make grafana-check` holds every
panel's, variable's and annotation's frame to fenec-server's own answer, in
the select list's order, and the provisioned rule's firing instances to the
server's rows. A jest test importing `datasource.ts` -- `@grafana/runtime`
and `@grafana/ui` with it -- sat at 0% CPU for ten minutes, so the logic
jest runs lives in modules importing types alone. `site/build.py` writes
the editor's FenecQL colours into `src/fenecql/highlight.js` as it writes
the studio's.

## Studio desktop

**The desktop app is a window on a studio a `fenec-server` serves, one server
per file** (`integrations/studio-desktop`, Electrobun 2.0.4, macOS on Apple
silicon, beta). The app bundles `fenec-server` and starts `fenec-server
--file <path> --studio --http 127.0.0.1:0` for each open file, with a random
32-byte token in `FENEC_HTTP_TOKEN` (the environment: `ps` shows every user's
arguments), finds it by the `listening on:` line on stderr, as every harness
of this repository finds a server on port 0, and opens a window on its own
`/_studio/`; closing the window sends SIGTERM, which syncs and writes the
checkpoint. A saved server is a window on that server's own studio, same
origin, so no CORS, its token in the Keychain (`Bun.secrets`, service the app's
identifier, name `connection:<id>`: Electrobun has no keychain API, and the
`security` command-line fallback was dropped because `add-generic-password -w`
puts the secret in argv). The server is started in `server.ts` and a token reaches a page in
`session.ts`, each the one place. The server is given `--exit-on-stdin-close`
and a pipe on its stdin that the app holds for its life: a killed process sends
its children no signal, but the system closes every descriptor it held, so the
server sees the pipe end however the app ended and shuts down as on SIGTERM
(sync, checkpoint, `<file>.lock` let go, exit 0). The first design noted each
server in a file and stopped the strays at the next launch (SIGTERM, a wait,
SIGKILL, what survived kept): it went when this passed, and with it the file,
since no release of the app ever had a server that needed it. The write end must
stay in the app alone: `checks/orphan.mjs` opens two files one after the other,
kills the app, and needs both servers to end and both files to be free (a
descriptor not marked close-on-exec would have the second hold the first's pipe).
The exit codes (server.html: 0 stopped as asked, 1 could not start, 2 a flag
refused) are the notices': a held lock is "open in another program", 2 is "out of
step", the rest "could not open", and `get-server.sh` refuses a server without
both flags (the first release with `--studio` is 0.1.11, with both 0.1.12). The
session is a frozen `window.fenecHost` that the studio reads before storage,
defined by the preload only where `location.origin` is the server's (a preload
runs in every page the window loads, and a page of another origin that found
`fenecHost` would read the token) and never stored: `test/session.test.ts` runs
the script and then the studio's own `connect.js` on what it made,
`checks/sandbox.mjs` asks the running window (Electrobun's preload runs in the
page's world: an isolated one would show the studio no session).

The server's environment is an allowlist (`HOME`, `PATH`, `TMPDIR`, `LANG`,
`LC_*`, `USER`, `LOGNAME`, `TZ`, and the token), not "everything but ours": the
server reads `OTEL_EXPORTER_OTLP_*` (which turns trace export on) and
`FENEC_JWT_SECRET`, and an app started from a shell must not hand a database it
opens whatever that shell exported (`env.ts`; `checks/environment.mjs` starts
the app with such variables and reads the server's own environment back through
a wrapper script). A quit waits for every server, one being started and one whose
window closed a moment ago included (`serversBusy`, `stopAll`), and an open of a
file waits for that file's server to finish stopping: a second server started
over the first's checkpoint would be refused, and the user told the file is open
elsewhere.

Windows. A studio window is `sandbox: true` (no call into Bun, only the
event-only `__electrobunSendToHost`, which `checks/sandbox.mjs` holds to the
six globals Electrobun's sandboxed preload defines) with navigation rules for
the server's origin and links out as web addresses to the browser, at most one
in three seconds from a window (2.0.4 does not say whether a click asked for a
`new-window-open`, and a page could otherwise open tabs without end). It does
not answer for `views://`, whose handler sends `Access-Control-Allow-Origin: *`.
**Every window of the app is held**, not only a studio window: the launcher and
the new-database window have the RPC bridge, and Electrobun's full preload is
added to every document a window loads (not the main frame alone), so a link or
a file dropped on one is a navigation, and what it loads is handed `openPath`,
`openConnection` and the saved servers. Each is held to its own `views://`
folder (`holdTo`; the constructor's `navigationRules` option did not take in
2.0.4, a navigation to another origin reported `allowed: true`, the setter does)
and `checks/pages.mjs` fails, as the launcher goes to example.com, if the rule is
taken away. A `file:` address is not even asked about: WebKit refuses it from a
page of another scheme. An origin that holds a `*` or a `?` is refused: it is
the rule's glob. Failures are windows of
the app's own (`messages.ts`, a bundled sandboxed page that draws with
`textContent`), never `Utils.showMessageBox`: that is a synchronous native call
in an `async` function, so the main process's JavaScript, every server's
supervision and the quit handler stood still until someone clicked (a check that
never clicked held the app for good). `exitOnLastWindowClosed` is false: the
default quits as the last window's close handlers return, too soon for a
server's SIGTERM to finish while the app still holds its lock; the app stays in
the Dock, as a document app does, and stops its servers itself on quit
(`before-quit` held until they have). A second open of a file the app is still
starting a server for waits for that one, and so does a second click on a saved
server while it is being looked at (a few seconds, up to the probe's timeout):
both are in `starting`, one open of a thing at a time.

The data store. What a page keeps (`localStorage`: the studio's query history,
saved queries and theme) lives in a WebKit data store, and `BrowserWindow`
makes its page's view in the one default store, told apart by origin alone: a
file's server listens on a port the system picks, so a window on a port used
before would show the earlier file's history. `BrowserWindow` has no `partition`
option, so the window is made with no page and the view made in it with one
(`BrowserView`'s `partition`, `autoResize`, `windowId`): a name without
`persist:` is `nonPersistentDataStore`, one store for that window, which a file
gets; `persist:remote-<id>` is `dataStoreForIdentifier`, kept between runs and
apart from every other saved server's, cleared when the connection is removed.
**`clearStorageData("all")` does nothing in Electrobun 2.0.4**: the native side
(`sessionClearStorageData`) takes a comma-separated list of exactly `cookies`,
`localStorage`, `sessionStorage`, `indexedDB`, `cache` and `serviceWorkers`
(its strings are those six and no "all"), and a name it does not know is
skipped, so the first version left a removed connection's history in the
store, to be shown to a new connection that got the same id's store (ids are
random, but the README promised removal clears). The list is explicit
(`STORE_KINDS`), the connection's own window is closed first (a page writes
again while its store is cleared), and the Keychain item goes before the store. The blank view Electrobun made for the window is
removed (`win.webview.remove()`; kept, idle memory was about 20 MB more, a second
WebKit view per window). `checks/storage.mjs` holds it by reading a store from
outside the window that wrote it: a test hook (`store-window`) opens a second
window on the same server's origin in the store it is told, so under one shared
store (the thing the partitions are for) the file window's history would show,
and a window in the very same store is the positive control. A removed
connection's store, opened again under its id, is empty, with a window open on
it at the removal or not. Two saved servers on one address do not share, a saved
server's survives its window, the page is the window's size and has focus.

The saved server. The probe asks `/_studio/` (served before any token), then
`/_whoami` **without** the token and sends it only if fenecdb's own 401 (a JSON
body naming an error) comes back, and follows no redirect (`redirect: "manual"`,
a message that says where, as an origin): an `Authorization` header goes wherever
a redirect says, and a proxy's sign-in page or a moved address would be told the
token. A token is for the address it was saved with: a connection pointed at
another origin with no token typed is refused (`tokenMayStay`), since keeping it
would send the old server's token to the new one; and the Keychain item holds
the token **with the origin it was saved for** (`token.ts`, `{origin, token}`),
read back only for that origin, and the connection is read again after the
Keychain answers (a Keychain read can wait on the user), the open stopping if
its address changed meanwhile: a token from an edit that landed between the two
never reaches the address it was not saved for. A saved token is taken away
alone with "Forget token" (a blank token field means keep it, as it did, so
there was no way to remove one but to delete the connection). The Keychain keeps it from
other users and from pages, **not from other programs of the same user**: the
client is the bundled `bun`, a general-purpose runtime that any such program can
run with a script of its own (`Bun.secrets.get` with the service and the id that
`state.json` names), and nothing here makes the Keychain ask first. Whether a
signed build's items are bound to its identity, so that the system asks, is a
thing to look at with the first signed build. There is no fallback to the
`security` command-line tool, whose `add-generic-password -w` puts the secret in
argv.

New database. Electrobun 2.0.4 has no save dialog, so the window takes a folder
from the folder dialog and a name, and `createDatabase` makes the file with
`open(path, "wx")`, which refuses a name that exists whatever its case (APFS),
and then starts a server on it: `fenec-server` takes a zero-length file as a
new database (an 8-byte head). It does not truncate or otherwise write to a file
that is not one: the signature is read first and refused (`invalid fenecdb
signature`), and the app says so and nothing changed (a check holds it). A server
that then fails to start does not leave the empty file behind. Open looks first
(`dbfile.ts`): `fenec-server` makes an empty file into a database and leaves
`<file>.lock` beside any file it opens, so a file that is empty (and not one New
database just made) or does not start with `FENECDB` is a message and a server
is never started on it; the panel is limited to `.fenec`.

The test variant. The checks drive the app through a control file and read its
reports, which a shipped app must not read: `build.bun.define` makes
`__TESTABLE__` a constant, `testhooks.ts` is imported only behind it, and
`FENEC_DESKTOP_TEST_BUILD=1` builds the same app with it true under its own
identifier (so its `userData` and Keychain service are its own: a run never
touches an install's recent files or saved servers), into `build-test/`.
`checks/release.mjs` reads the shipped app's main bundle (out of the
self-extractor's `.tar.zst`, without running it) and fails on any hook marker,
and on the test variant's lacking one, so a rename cannot make it pass by
looking for what was never there. The checks wait for events -- a fact the app
reports, a process leaving, a page drawing -- and wait out a time only for what
must not happen (`waitNone`, 1.2 s, a link that must not be opened).

Toolchain. `electrobun init` installed Hutch (the build tool) in `~/.hutch` and
appended two lines to `~/.zshrc`, and the npm package's resolver would, were
its download to fail, run that installer. `scripts/electrobun.mjs` runs the
package's own resolver with `HUTCH_HOME` in the project (`.hutch-home/`,
ignored) and an `installHutch` that refuses; the archive is checked against
Electrobun's release index by the resolver, and `electrobun` is an exact
version in a lock file, so nothing is fetched that is not named by a hash or a
version. The npm package's `exports` map every subpath to a stub, so the
resolver is required by path. The index the resolver checks the archive against
is fetched from the same release as the archive, so `hutch-pin.json` holds, in
the repository, the SHA-256 of the archive as downloaded (checked through the
resolver's `download` hook) and of the two binaries in it (checked on every run,
cached or fresh: a CI cache of `.hutch-home/` is not trusted for being one);
`ELECTROBUN_HUTCH_BINARY`, which the resolver takes unchecked, is ignored, and
the resolver is told no file exists (`existsSync: () => false`), which is how it
decides on a Hutch "already at `$HUTCH_HOME/bin`" that it would run unchecked
when the paired download failed (a cache's, with no pin). That is not all that
ships: Hutch then fetches the Electrobun runtime (`libNativeWrapper.dylib`, the
launcher, the main process's API sources, `preload-sandboxed.js`), its Bun and
Cottontail, and puts the first two inside the app. It checks those against a
lock of its own (`.hutch/dependencies.lock`, not in git, no hash for its Bun) and
a tampered file in a restored cache would be in the lock that cache wrote. So
`hutch-pin.json` holds a digest of each directory too (`scripts/tree-digest.mjs`:
the path, executable bit and SHA-256 of every file, hidden ones too, and a
symlink as itself), and `scripts/electrobun.mjs` runs `sync` first (which runs
nothing of the app's), compares, and only then builds; tried with a byte added
to the preload, to the native library and to the Bun, each refused, and a
Hutch binary changed in the cache, which is not run. The digests are
deterministic (a fresh `.hutch-home/` gives them again, and the hosted runner
checks them each run). An Electrobun that Hutch could not fetch without its
install script was also considered: a lock file was not committed, since
Hutch's own is per project and per machine and with no Bun hash proved nothing
here. A Bun upgrade is a pin change by the same command
(`--print-runtime-pin`).

The first launch. A stable Electrobun build is a self-extractor: the first run
unpacks the app in place with an installer window and starts it as a process of
its own, and the first process stays in its event loop, so quitting the app
leaves it alive (seen unsigned, from `open` and from a harness). The checks do
one launch first (`warmup`) and end the helper. Not verified, for want of a
Developer ID certificate: that an in-place unpacking leaves a signed and
notarized bundle's seal whole, and that Hutch gives the bundled `fenec-server`
the hardened runtime (`desktop.yml`'s signing job fails if `codesign` says
otherwise).

Signing. `desktop.yml` builds and checks on every pull request and push to the
paths the app is made from (the studio, `fenec-server`, `fenec-http`,
`fenec-core`, the app), and signs only when a maintainer starts it by hand from
`main`, in the Environment `desktop-signing` that holds the six secrets (the
Developer ID's name, the `.p12` and its password, the App Store Connect key's id,
issuer and `.p8`): a pull request's or another branch's code never meets one, and
a dispatch from another branch is skipped. The Environment is made, with a required
reviewer and `main` as its only deployment branch, **before any secret**: a
secret added to one without rules is readable from any branch that names the
Environment, and the order cannot be enforced by the workflow alone, since the
Environments API asks for administration rights that the job's token does not
have; the job's first step reads the rules back where the token can, and fails
unless they are those, and warns where it cannot (RELEASING.md has the `gh api`
commands). The signing job has read access only,
restores no cache (what a cache holds was written by runs without the secrets),
checks out without persisting the token, imports the certificate into a
keychain of its own with `-T /usr/bin/codesign -T /usr/bin/security` and not
`-A`, validates the release it is given before it reaches `make` (which
expands `$(shell ...)` in a value on its command line), runs `release.mjs` on the
signed bundle, and greps the bundled server's signature for the hardened runtime
and for the Developer ID separately (one `grep -E` with an alternation passes if
either is there). A second job, with the workflow's only write permission and no
secret, attaches the DMG the first uploaded; nothing unsigned is ever attached.

Rejected. webview-bun with `bun build --compile` (an 89.8 MB binary, studio
visible in 0.87 s): `Webview.run()` blocks the JavaScript thread in the native
loop, so with a `setInterval` and a SIGTERM handler installed it ticked 0 times
in 5 s and ignored SIGTERM; nothing in JavaScript can supervise the server while
a window is open, and it has no menus, no file dialog and one process per
window. Tauri is the better long-term shell (a save panel, a signing and
installing story on all three systems, `keyring`, an updater, single-instance,
and `fenec-http` could run in the app's own process: no child, no orphan, no
stderr parsing, the process's own `fcntl` lock) and a larger rewrite in Rust
outside the zero-dependency set (allowed under `integrations/`) at 3-10 MB for
the shell instead of Bun's 63; re-decide before Windows. No server at all, the
studio's local mode over the file's bytes, reads a 100 000-row file (8.3 MB)
in 5.3 ms in Node and the 64 MB graph file in 101 ms, and is the wrong design
for a file: the module has no mmap and the file is loaded whole into wasm
linear memory (+39 MB for the 8.3 MB file, +268 MB, 4.2x, for the 64 MB one,
never given back, wasm32 capped near 2-4 GB where the server opens a 10 GB
file on an 8 GB machine), writes would cross an RPC to Bun to be appended with
an fsync policy of our own (WKWebView has no File System Access API, and the
window would need the RPC bridge, so no sandbox), the `<file>.lock` is an
`fcntl` record lock a page cannot take, and it is a second engine on one thread
without `--warm`, auto-compact, `@ttl` sweeps, `/_changes`, MCP, the admin view
or metrics. It stays right for the site's playground.

Measured on a MacBook Air (MacBookAir10,1, Apple M1, 8 GB), macOS 26.1, with
other work running (load average 3 to 4), by `scripts/measure.mjs` on the test
variant after its first launch, five runs each, warm page cache (not a reboot):

| | 50 rows | 100 000 rows (8.7 MB) |
|---|---|---|
| studio showing the collections, from launching the executable | 436-529 ms, median 443 | 443-452 ms, median 445 |
| `fenec-server` listening | 138-211 ms | 142-204 ms |
| idle RSS, the app's processes and WebKit's | 250-251 MB | 274-276 MB |
| ... of which Bun and the launcher, WebKit, the server | 139, 106-107, 5 | 139, 107-110, 27 |
| quit, with the server's checkpoint | 183-449 ms | 326-449 ms |

The DMG is 23.6 MB (the update archive 22.6 MB), the unpacked app 74.7 MB (Bun
63.6, `fenec-server` 4.4), against 23.0 MB and 71 MB for the spike: the icon,
three pages and the failure wording. RSS counts a shared page in every process
that maps it, so a total is an upper bound, and a window is a server process:
the server's share is per open file. Not taken again: the first launch from the
DMG (3.7 s, n=1, in the spike) and the 100 000 x 128 file with a graph (64 MB,
server 145 MB, total 392 MB there). The figures are the test variant's, which is
the shipped app plus the hooks (10 KB of JavaScript). They are of the window as it was
before each got a data store of its own (a window with no page, and the page's
view made in it). The two were measured side by side afterwards, the commit
before and this one, interleaved, on a Mac whose load other jobs moved between
2 and 25: launch to the studio and idle memory are the same within that noise
(for the 50-row file, idle RSS 215-246 MB before and 212-219 MB after, medians of
five); with the window's own blank view left under the page's it was about 20 MB
more, which is why it is removed.

## deploy

**`deploy/` is the kit for running regions, held to the binaries by two
checks** (`deploy/RUNBOOK.md`). systemd units for a tenant node, a region's
router and its standby, the global router and its standby, a node's
archiver (`fenec-archive@<node>`) and its rclone copy to a bucket, and
cloudflared; their environment files (`deploy/env/`), alert rules for
Prometheus and Grafana (`deploy/alerts/`) and the runbook. A token goes in
an environment file systemd reads as root (`EnvironmentFile=`, 0600), the
archive key and the tunnel's and rclone's credentials through
`LoadCredential=`, and never on a command line, which `ps` shows every user
(every token has a variable, and an empty one is none: An empty token is
no token, above). Each role runs as a user of its own (`fenec-node`,
`fenec-router`, `fenec-archive`, `fenec-sync`): a process reads
`/proc/<pid>/environ` of its own user's processes, which `ProtectProc=`
hides only from other users, so one `fenec` user let rclone read the
archiver's replication token. No `ProcSubset=pid`: it hid `/proc/meminfo`,
and with it `cache::room`, the compact before a file outgrows the page
cache. The tunnel's hostname is public, so `cloudflared/config.yml` sends a
tenant's paths alone to the router and answers 404 to the rest -- a
tenant's `/t/<t>/_replication` first, which the router would forward to the
node and the one replication token opens: let through, it was every
tenant's rows behind one token from anywhere. cloudflared matches the path
decoded, so `%5F` and `%2F` meet that rule, and `deploy/test/ingress.py`
asks cloudflared's own matcher (`tunnel ingress rule`) of 28 paths. Access
in front says who: the entry sends a service token
(`ACCESS_CLIENT_ID`/`ACCESS_CLIENT_SECRET`, both or neither) and the router
drops `CF-Access-Client-*` before a node (`forwarded_headers`, which
`forward` now shares with `forward_near`: its own copy of the filter kept
the token once only the shared one dropped it). The bucket copy is
`deploy/bin/fenec-archive-sync`, `rclone copy` (never `sync`: a lost
archive takes nothing from the bucket, whose lifecycle rule expires what
`--keep` let go of) with `--local-no-check-updated` -- rclone refused a
segment growing as it read it, so a busy tenant's newest segment never
reached the bucket, every run failing in the test -- and `--fast-list`;
it refuses a first copy from an archive it has not copied before (the
directory's filesystem and inode, noted in its state directory) where the
bucket holds a file the disk does not, since a rebuilt machine's new
archive rewrote the bucket's `history`, `began` and `presence` within the
timer's first five minutes; and writes each success into node_exporter's
textfile collector 0644 -- under the unit's `UMask` it was 0600,
node_exporter read nothing and `FenecArchiveSyncStale` could never fire,
so the test scrapes a real node_exporter (`FenecTextfileUnread`,
`FenecArchiveSyncUnreported` over the absent case). Access outside strict
service token authentication answers a service token's request with a
`CF_Authorization` cookie for the hostname, so the entry drops `Set-Cookie`
from every answer (no router or node sets one), the router drops every
`Cf-Access-*` header (the JWT Access adds too), and the entry refuses an
`http://` router with a token set. `FenecAutoFailoverIncomplete` is read
off the directory -- `fenec_router_tenants > 0` on a node
`fenec_router_node_failed_over` -- since the count of failed failovers,
reset only by a restart, held it after the fix and paged again at the next
node failed over whole; `FenecLeaseRenewalLate` passes over a node failed
over in the last ten minutes (its five-minute window held the outage
after the node came back) and holds a minute (the scrape before a failover
marks its node). The rules read
Prometheus's `up` by job for processes that export nothing because they
are down, a router's role (primary, standby) taken from its last
`fenec_router_following`; `tests/datadog.rs` passes over `job` and
`instance` matchers, holds the jobs to the rules' scrape config and the
textfile metric to the unit writing it. A standby router's
unit carries its primary's flags and the same directory file and conflicts
with the primary's, and a promotion is `POST /_replication/promote` and the
switch of units at once: `--replica-of` makes any file follow again
(`replication::settle_history`), so the standby's unit started after a
promotion would have the promoted file follow the dead router. `make
deploy-check` runs promtool over the rules and their tests, then boots
systemd in two privileged containers (`deploy/Dockerfile`'s `systemd`, whose
boot script makes `/` shared first: systemd in a container leaves it
private, and `LoadCredential=` then hands a service an empty directory) and
installs, verifies, starts and uses every unit (`deploy/test/units.py`:
real rclone into a `local` remote while a tenant takes 64 KB rows as fast
as it can, the copy verified as an archive; every role's user refused
every other role's environment; `/proc/meminfo` read in the node's mount
namespace);
fenec-shard's `tests/datadog.rs` holds every metric and label value the
rules read to a scrape of every kind of process, a leasing router and a
replicated node among them, and Grafana's rules to Prometheus's. `make
drill` (`deploy/drill/drill.py`, the standard library and the Docker CLI)
runs two regions -- a router keeping each one's directory and its standby,
two nodes in two zones under `--lease --replicas --sync always`, an
archiver a node, the global router and its standby -- every token in the
environment and no port fixed, a writer a tenant putting rows by id 1..K
through the routers; it cuts the global routers off (each region writes
on), cuts a node off the network and `docker kill`s it -- a machine lost:
killed alone, its kernel closed its sockets and delivered what they held --
and starts it again, kills a region's router and promotes its standby as
the runbook does, restores a tenant from its node's archive to a moment it
marked, and reads every acknowledged write back from each primary and its
replica, the router's failover read off its log against the killed
container's end (`failed_over_after`, both by the Docker VM's clock). Three
runs at a lease of 2 s and an upstream timeout of 4 s, what they saw and
not bounds (a reviewer's machine saw a repair of 1.07 s, writes stopping
1.31 s after a router's kill before grants were numbered and gaps of 28-30
ms): 41 577
acknowledged, 0 lost; the node failed over 1.92, 2.56 and 1.90 s after the
kill, its tenants' writers writing again 3.90, 3.97 and 3.93 s after the
kill, the tenant whose replica it held losing 17, 18 and 17 ms at most, every other tenant
16, 18 and 17; the repair 0.51, 0.51 and 0.50 s after the node's start; a region
whose router was killed stopped writing 1.67, 1.61 and 1.66 s after and wrote
again 0.43-0.51 s after its standby's promotion -- since grants are
numbered, 1.02, 0.83 and 1.00 s and 0.42-0.48, the node killed failed over
1.90, 2.06 and 2.56 s after, its writers again 3.90-3.94 s, 40 588
acknowledged and 0 lost (three runs of this kit's drill). The writers' figure
was the upstream timeout's, not the failover's: a writer's request in
flight to the dead node waited there until the router let it go, which at
fenec-shard's 60 s was the writer's own 5 s client timeout, every run's
4.86-4.92 s -- quoted at first as a promotion waiting out the replica's
reconnect. The failover answers it now (the paragraph on what a failover
ends, above): in two runs a side in turns, the writers wrote again 2.52 and
1.93 s after the kill as the router failed the node over at 2.53 and 1.93,
against main's 3.94 and 3.95 beside failovers at 2.49 and 2.26. CI's runs met a worse one before #340 -- a dead node's name its
resolver was slow to refuse held up the router's whole lease round, the
region's healthy nodes unrenewed and failed over, 26.5-31.4 s to write again
and 12-18 s without a write for a tenant on a healthy node, the repair
missing its 60 s in four of six runs -- which #340 fixed (the lease
paragraph above). The kit's routers run `--upstream-timeout` at 10 s
(`FENEC_UPSTREAM_TIMEOUT`), the drill's at twice its lease: chosen at twice
the lease so a failed-over node's requests were let go of, it now bounds
only what no failover ends -- a node not failed over, a relay to a router
gone, a standby's forwards -- and need not follow the lease; past it a
slow statement is a 502 while the node finishes it, and a request
carrying `Fenec-Wait` is allowed the timeout plus its wait. When
the drill fails it prints the last 300 lines of every router's and node's
log (`DRILL_LOG_LINES`), since the cleanup takes the containers and CI
leaves nothing else to read. CI's `deploy` job runs both checks.
