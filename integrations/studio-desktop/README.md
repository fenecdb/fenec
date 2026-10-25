# fenec studio, desktop (beta)

[fenec studio](../../site/content/docs/studio.html) as a Mac app: open a
`.fenec` file with nothing to start, make a new database, or keep saved
connections to servers. It is **a window on a studio a `fenec-server`
serves**, not a second implementation of it: the app bundles `fenec-server`,
starts one per open file, and shows its own `/_studio/` page, so what the
window does is what the studio does in a browser.

Beta, macOS on Apple silicon only. The shell is [Electrobun](https://electrobun.dev)
2.0.4 (a Bun main process and the system's WKWebView); its build tool ships no
Intel Mac build, and its Windows and Linux paths are not used here yet (no file
associations, no save dialog, a Windows lock for `fenec-server` that does not
exist). Not signed or notarized until a maintainer adds the secrets
([below](#signing-and-notarization)). An app you build here opens as it is; a
build you *downloaded* unsigned (the CI's artifact) is quarantined by the
browser and opens after `xattr -cr "fenec studio.app"` or *Open Anyway* in
System Settings.

```
 fenec studio.app
 ├─ Bun main process ── src/bun/*   windows, menu, servers, Keychain
 │    ├─ launcher window            recent files, saved servers   (bundled page, RPC)
 │    ├─ new-database window        folder, name                  (bundled page, RPC)
 │    ├─ notice windows             every failure, in words       (bundled page, event bridge only)
 │    └─ one window per open thing  <server>/_studio/             (the server's page, event bridge only)
 └─ fenec-server --file F --studio --http 127.0.0.1:0     one per open file, token in its environment
```

## Build, run, test

You need a Mac with the Xcode command line tools (`sips`, `iconutil`,
`hdiutil`), Node 22 or later with npm, and Rust (`rustup`, for the server).
Nothing is installed globally and no shell file is touched.

```bash
make studio-desktop        # the unsigned .app and .dmg
make studio-desktop-test   # type check, unit tests, every scripted check
```

`make studio-desktop` builds `fenec-server` from this checkout
(`scripts/get-server.sh`), makes the icon, and builds. The results:

```
integrations/studio-desktop/build/stable-macos-arm64/fenec studio.app
integrations/studio-desktop/artifacts/macos-arm64-fenecstudio.dmg
```

`make studio-desktop STUDIO_SERVER=0.1.12` bundles that release's server
instead: it downloads `fenec-v0.1.12-<target>.tar.gz` and checks its SHA-256
against the release's own `SHA256SUMS` (the same script, `scripts/get-server.sh
0.1.12`). Either way a server without the two flags the app starts it with,
`--studio` and `--exit-on-stdin-close`, is refused with a message saying so:
the first release with `--studio` is 0.1.11 and the first with both is 0.1.12,
and every release before them (up to 0.1.10) has neither.

By hand, from this directory:

```bash
npm ci --ignore-scripts      # electrobun (the bootstrap) and typescript, pinned
sh scripts/get-server.sh     # bin/fenec-server, built from this checkout
npm run dev                  # the app, from source, in the dev channel
npm run build                # the stable build, as `make studio-desktop` does
npm run typecheck && npm run test:unit
```

### The toolchain stays in this directory

Electrobun's builds are driven by Hutch, a build tool its `electrobun` npm
package downloads (from Electrobun's GitHub release) and which, left alone,
installs itself in `~/.hutch` and, if that download fails, runs an installer
that appends a line to your shell's rc file. `scripts/electrobun.mjs` is how
everything here runs it instead: `HUTCH_HOME` is `.hutch-home/` (ignored by
git; Hutch, its Bun and the Electrobun runtime, about 210 MB), and the resolver
is handed an installer that refuses, so no path ends in the global one.
`electrobun` and `typescript` are exact versions in `package.json`, held by
`package-lock.json`; Hutch's and Electrobun's by `hutch.config.ts`. Set
`HUTCH_HOME` yourself to keep the toolchain elsewhere.

**Hutch is held to `hutch-pin.json`**, in this repository: the SHA-256 of the
archive as it is downloaded, and of the two binaries in it, which
`scripts/electrobun.mjs` checks on every run, cached or fresh, before it runs
anything. The release's own index sits in the same place as the archive and
proves little against that place; a CI cache of `.hutch-home/` is not trusted
for being one. A different Electrobun is a change to `hutch-pin.json` (the
script says which hashes), which a pull request shows. What Hutch then fetches
(its Bun, the Electrobun runtime) it checks against its own lock, in
`.hutch/dependencies.lock`.

### The checks

`make studio-desktop-test` runs, in order:

1. `tsc` over the sources, the unit tests and the config, then `test/*.test.ts`
   under `node --test` (names, the remote probe against stand-in servers, a
   redirect that must not carry a token, the wording of every failure, the
   token hand-off script run as a page would, the server's environment, what a
   file is before a server is started on it, the token hand-off run through
   the studio's own `connect.js`, the signing switches in `electrobun.config.ts`, the pin, `get-server.sh`
   against a release laid out in a directory).
2. The **test variant**: the same app built with `FENEC_DESKTOP_TEST_BUILD=1`,
   which adds `src/bun/testhooks.ts` and gives it an identifier of its own
   (`dev.fenecdb.studio.test`), so a run touches none of a real install's
   recent files, saved servers or Keychain items. The hooks read
   `FENEC_DESKTOP_REPORT` (a file the app appends facts to) and
   `FENEC_DESKTOP_CONTROL` (a file of commands it reads as it grows); a page
   reports through the event-only bridge a sandboxed window keeps. They, and
   `FENEC_SERVER_BIN` (which names the server to start), are bundled only when
   `__TESTABLE__` is true, a build-time constant, and
   `scripts/checks/release.mjs` reads the shipped app's main bundle and fails if
   any of them is in it. **An environment variable the shipped app reads for
   itself is a hook, unless it is read behind `__TESTABLE__`.**
3. `scripts/checks/run.mjs`, which needs a window server (a logged-in Mac or a
   CI runner) and runs each in a process of its own, killing what one leaves
   behind:

   | check | what it holds |
   |---|---|
   | `release` | the shipped bundle has none of the hooks |
   | `warmup` | the first launch of a build (below) |
   | `files` | open, edit through the studio's own form, a second open only shows the window, close stops the server with exit 0, reopen has the edit (also at once, while the first server is still stopping); a file another program holds is a message and **Try again** works once it lets go; a missing file, a folder, a file that is not a database and an empty one are said before any server is started, and nothing is written beside them; a quit right after a close waits for that server, and it ends with exit 0 |
   | `server-dies` | `kill -9` the server under a window: a message, the window closed, **Open again** works |
   | `orphan` | the app killed with two files open, one opened after the other: **both servers end by themselves** (the end of their stdin), and both files are free for another program; so the second's start did not carry the first's pipe |
   | `sandbox` | the session is `window.fenecHost`, frozen, in the page's own world, and in no storage; the studio has no sign-in form and no Sign out; only the event bridge in a studio window, no navigating off its origin, links out only as web addresses, to the browser, one in three seconds |
   | `pages` | the launcher, the new-database window and a notice window cannot be navigated anywhere else (a web page, a file, another of the app's pages): without the rule, the launcher goes to example.com |
   | `storage` | what a window keeps (the studio's history, theme) is its own: two saved servers on one address do not share it, a saved server's survives its window, a file's does not outlive its window, a removed connection's is cleared |
   | `environment` | the server is started with the allowlist and its token, whatever the app was started with (`OTEL_*`, `FENEC_JWT_SECRET`, and the checks' own variables stay out) |
   | `remote` | saved servers: token in the Keychain and in no `state.json` or process argument; the studio signed in; every failure in words (wrong token, no token, studio off, unreachable, a redirect that the token does not follow); an address changed without a token is refused; edit; two clicks open one window; remove deletes the Keychain item |
   | `newdb` | the folder dialog (answered by the check), a name, the file made and opened; an existing name refused, case-insensitively, the file untouched; bad names; Cancel |
   | `finder` | `open -a <app> <file>` with the app not running: the file arrives as `open-url`, spaces and non-ASCII letters included |

   `ONLY=files,remote npm run checks` runs a subset (with the test variant
   built: `FENEC_DESKTOP_TEST_BUILD=1 npm run build`). A failure's logs are in
   `$SD_DIR` (default `fenec-desktop-checks` in the temp directory):
   `<check>.log` and the app's own `<check>.app.log`.

**What running the checks does to a Mac.** The test variant is a second app
with a bundle identifier of its own, and LaunchServices may list it among the
programs that can open a `.fenec` file (the `finder` check needs the document
type). `run.mjs` deletes the test variant's own folder in
`~/Library/Application Support/dev.fenecdb.studio.test` before it starts, and
its Keychain items (service `dev.fenecdb.studio.test`) are the checks' own,
removed as they go. Nothing of a real install (`dev.fenecdb.studio`) is read or
written. To take the test variant out of the "Open With" list, delete
`build-test/`.

`scripts/measure.mjs` times the launch to the studio, idle memory and quit of
the test variant (not run by the checks or CI: a shared runner's figures say
nothing); `scripts/mkdata.mjs` makes the file it opens.

**The first launch of a build is not like the others.** A stable Electrobun
build is a self-extractor: the first run unpacks the app next to itself (about
2 s, with an installer window), starts it as a process of its own, and the
first process stays in its own event loop afterwards, so quitting the app
leaves it running (Electrobun 2.0.4; seen unsigned, in place and from `open`).
`warmup` does that launch and ends the helper, so the other checks start from
a build that has been launched once. Whether a signed and notarized bundle,
which must not change after it is signed, survives the unpacking is the one
thing in the list below that has to be looked at with a real certificate.

## How it works

| | |
|---|---|
| `src/bun/server.ts` | **the one place a server is started**: `fenec-server --file F --studio --http 127.0.0.1:0 --exit-on-stdin-close` with a random 32-byte token in `FENEC_HTTP_TOKEN` and **a pipe on its stdin that the app holds for the server's life**, found by the `listening on:` line on stderr, which is read for as long as it runs; SIGTERM when the window closes (sync and checkpoint on the way down), SIGKILL after 30 s; given a few variables (`env.ts`: `HOME`, `PATH`, `TMPDIR`, `LANG`, `LC_*`, `USER`, `LOGNAME`, `TZ`) and nothing else of the app's environment |
| `src/bun/session.ts` | **the one place a token reaches a page**: the preload that defines `window.fenecHost` (below) |
| `src/bun/windows.ts` | what a studio window is held to; the notice window; `holdTo`, which holds every window of the app to its address |
| `src/bun/app.ts` | open a file or a saved server, make a new database, keep the saved list; one window per `file:<real path>` or `remote:<id>`; one open of a thing at a time; what a quit waits for |
| `src/bun/pages.ts` | the launcher and the new-database window and what they may ask (`src/shared/rpc.ts`) |
| `src/bun/messages.ts`, `remote.ts`, `names.ts`, `dbfile.ts`, `env.ts` | every failure in words, the probe of a saved server, the check of a new name, what a file is, the server's environment: pure, unit-tested |
| `src/bun/secrets.ts`, `store.ts` | Keychain (`Bun.secrets`) and `state.json` |
| `src/launcher`, `src/newdb`, `src/notice`, `src/ui` | the app's own pages |

**Open…** takes `.fenec` files (the panel is limited to them) and, whatever
is picked, looks at the file before a server is started on it
(`dbfile.ts`): `fenec-server` makes an empty file into a database and leaves
`<file>.lock` beside any file it opens, which a file picked by mistake should
not get. A file that does not start with the fenecdb signature, or is empty,
is a message, and nothing is written beside it. The one empty file that opens
is one **New database…** has just made.

A **saved server** is a window on that server's own `/_studio/`: same origin,
so no CORS to set up, and the studio is the one that server was built with.
Before the window opens the app asks the server two things (`remote.ts`) and
says what is wrong in words instead of opening a blank window: is there a
studio at `/_studio/` (served before any token), and does `/_whoami` take the
saved token. The token goes out last: `/_whoami` is asked without it first and
the token is sent only if fenecdb's own 401 (a JSON body naming an error) comes
back, and no request follows a redirect, since an `Authorization` header goes
wherever a redirect says; a redirect is a message, not a place to go. A
connection's URL, name and tenant are in `state.json` under the app's data
folder (`~/Library/Application Support/dev.fenecdb.studio/stable/`); its token
is in the Keychain, service `dev.fenecdb.studio`, name `connection:<id>`, and
nowhere else. **A token is for the address it was saved with**: changing a
connection's address with no token typed is refused, since keeping the old
token would send it to the new address. A second click on a saved server while
the first is being looked at (up to a few seconds) waits for it: one window.

Each window has **a store of its own** for what a page keeps (the studio's
query history, saved queries and theme). Electrobun makes a window's page in
one store shared by every window and told apart by origin alone, and a file's
server listens on a port the system picks, so a later window on a port used
before would show the earlier file's history. So the window is made empty and
its page's view is made in it (`windows.ts`) with a store of its own: a name
without `persist:` for a file (a store that exists for the window), and
`persist:remote-<id>` for a saved server (kept between runs, apart from every
other saved server's, and cleared when the connection is removed). Electrobun's
own view of the window, blank and in the shared store, is removed first.

**New database…** is a small window of the app's own (Electrobun has no save
panel): the folder from the folder dialog, a name, and the app creates the
file with `open(..., "wx")`, which fails if the name exists (so no file is ever
made over), and starts a server on it, which takes an empty file as a new
database. A name is checked as a file name (no `/`, `\`, `:`, no leading dot,
255 bytes), `.fenec` is added once, and a server that then fails to start does
not leave the empty file behind.

## The security model of the token hand-off

A studio is "a page with no authority of its own": it holds a token and sends
it. So a window that shows one gets:

* **A token that is never in a URL, a query, a fragment or argv.** For a file
  the app makes 32 random bytes per server start, gives them to that server in
  its environment (`ps` shows every user's processes' arguments, not their
  environment), and hands the page the same token in its preload.
  For a saved server the token comes from the Keychain. The preload is a user
  script that runs at document start, in the page's own world and before its
  modules, and defines **`window.fenecHost`**, frozen, with the session
  `{server, token, tenant}` in it, **only if `location.origin` is the server's
  own** (site/content/docs/studio.html#host): it runs in every page the window
  loads, a redirect's or a sign-in page's, and a page of another origin that
  found `fenecHost` would read the token. The studio takes the session only
  frozen and naming the origin that served it, **never stores it** (the token
  is in no storage of the page's), shows no sign-in form and no Sign out (the
  app holds the connection), and says the app handed it none, without sending
  anything, if it finds no session. `test/session.test.ts` runs the script as a
  page would and then asks the studio's own `studio/connect.js` what it makes of
  it: with another origin, a lookalike host, a token that tries to break out of
  the string, and an attempt to redefine or change what was handed over;
  `checks/sandbox.mjs` asks the running window (the preload does run in the
  page's world: an isolated one would leave the studio with nothing).
* **No call into the main process from a page a server serves.** A studio
  window is `sandbox: true`: the RPC bridge to Bun is not injected, only the
  event-only `__electrobunSendToHost` a window keeps, which carries a message
  one way and calls nothing (`checks/sandbox.mjs` lists every global and fails
  on any other). It does not answer for `views://` either
  (`allowedProtocols`), whose handler would let the page read the app's own
  pages. A notice window is sandboxed too, and draws what it is told with
  `textContent`, so what a server said is text there, never markup.
* **Every window is held to where it is.** The windows with the RPC bridge, the
  launcher and the new-database window, have Electrobun's full preload added to
  every document they load, so a link or a file dropped on one would load a
  page that is handed `openPath`, `openConnection` and the saved servers: each
  is held by navigation rules to its own `views://` folder, as a notice window
  is (`holdTo`; `checks/pages.mjs` fails without it). A studio window is held
  to its server's origin (the constructor's option did not take in 2.0.4;
  `setNavigationRules` does), and an address that is a glob (`*` in a host) is
  refused, being the rule's pattern. A link out of a studio window, `window.open`
  included, goes to the default browser as a web address, never as a `file:` or
  a custom scheme, and at most one in three seconds from a window (see below).
* **One process per file, never two over a file.** `fenec-server` takes
  `<file>.lock`; a second program is refused and the app says so. The app
  never starts a second server on a file it is still starting one for, nor one
  on a file whose server is still stopping.
* **No environment of the app's for the server**, but the allowlist: the server
  reads `OTEL_EXPORTER_OTLP_*` (trace export), `FENEC_JWT_SECRET` and more, and
  an app started from a shell must not hand them to a database it opens.

**What is not covered, said as it is:**

* **The Keychain keeps the token from other users and from pages, not from
  other programs of the same user.** The Keychain's client here is the bundled
  `bun`, a general-purpose runtime that any program running as that user can
  run with a script of its own (`Bun.secrets.get({service, name})`, the service
  being the app's identifier and the name `connection:<id>` from `state.json`),
  and nothing in the app makes the Keychain ask first; it is likely not to. Whether
  a signed build's items are bound to the signing identity so that the system
  asks for another program is to be looked at with the first signed build
  (below). A program that can run as the user can also read the files and the
  memory of the app: the app is as exposed as the Mac it runs on to what
  already runs as its user.
* The token of a local server lives in the app's and the server's memory and in
  the page's (`window.fenecHost`, readable by any script of the studio's own
  page, which is the studio) until the window closes; it is in no storage.
* A saved server on plain `http://` to another machine sends its token
  unencrypted: the launcher warns where the address is typed.
* **A page can ask for a link to open in your browser without a click**
  (`window.open` from a script): Electrobun 2.0.4 does not say whether a click
  made the request. The app hands over only web addresses, and at most one in
  three seconds from a window, so a server's page cannot open tabs without end;
  the page is the studio of a server you chose to open.

### No server outlives the app

macOS has no parent-death signal, so a child is not told when its app is
killed (not quit). But the system closes every descriptor a dead process held,
and each server's stdin is a pipe the app holds the write end of
(`--exit-on-stdin-close`, site/content/docs/server.html): the server sees it
end, **`kill -9` too**, and shuts down as on SIGTERM (sync, checkpoint, its lock
let go, exit 0) instead of running on holding the file with nobody to answer.
Nothing is noted on disk and nothing is stopped at the next launch: an earlier
app's registry and reaper are gone, since no release of this app ever held a
server that would need them. The write end must stay in the app alone: a child
that inherited it would keep the server's stdin open after the app was gone
(`checks/orphan.mjs` opens two files one after the other, kills the app, and
needs both to end and both files to be free). A quit waits for every server, one
being started and one whose window closed a moment ago included.

## What a failure looks like

Every one is a window of the app's own, never a native alert (a native message
box is a synchronous call that stops the main process, and with it every
server's supervision, until someone clicks):

| | message | button |
|---|---|---|
| the file is open in another program | "This database is open in another program: close it there first; two programs over one file would corrupt it", with what the server said | Try again |
| the file is not a database, or is empty | said before a server is started; nothing is written beside it | |
| the server could not start | "Could not open the database", with what it said (exit 1) | Try again |
| the server refuses how the app starts it (exit 2) | "This app's database server is out of step with it": the app and its server were built apart; rebuild | |
| the server dies under a window | "The database server stopped; nothing written before it stopped is lost", the exit and its last lines | Open again |
| a saved server is unreachable | "Could not reach the server": nothing is listening, the name does not resolve, no answer in 6 seconds, its certificate is not accepted | Edit connection |
| it has no studio | "This server has no studio: start it with `--studio`" | |
| it redirects | "The server sent us somewhere else", and where; the token is not sent on | Edit connection |
| the token is refused, or none is saved and one is needed | "The server refused the saved token" / "asks for a token" | Edit connection |
| a file that is gone, a folder | said, nothing opens | |

## Signing and notarization

`make studio-desktop` signs and notarizes when `ELECTROBUN_DEVELOPER_ID` is set
(the certificate's name, `Developer ID Application: Name (TEAMID)`, in a
keychain on the machine) and, for notarization, either an App Store Connect API
key (`ELECTROBUN_APPLEAPIKEY`, `ELECTROBUN_APPLEAPIISSUER`,
`ELECTROBUN_APPLEAPIKEYPATH`, the path of the `.p8`) or an Apple ID
(`ELECTROBUN_APPLEID`, `ELECTROBUN_APPLEIDPASS`, `ELECTROBUN_TEAMID`); an empty
variable is unset. Hutch signs the nested Mach-O files (the bundled
`fenec-server` among them), the app and the DMG, submits, staples and
validates. The test variant is never signed.

`.github/workflows/desktop.yml` signs **only by hand, from `main`, in a GitHub
Environment that holds the secrets**, so that a pull request, a push or a
dispatch of another branch never reaches one and the code that is signed is
main's. **A maintainer sets it up once:**

1. Settings, Environments, New environment: **`desktop-signing`**, with a
   required reviewer (a dispatch then waits for a person) and the deployment
   branch limited to `main`.
2. In that Environment (not in the repository's secrets, where any job could
   read them):

   | secret | what |
   |---|---|
   | `ELECTROBUN_DEVELOPER_ID` | the certificate's name |
   | `MACOS_CERTIFICATE_P12`, `MACOS_CERTIFICATE_PASSWORD` | the Developer ID Application certificate and its key as a `.p12`, base64, and its password (a runner has no keychain to read the certificate from) |
   | `ELECTROBUN_APPLEAPIKEY`, `ELECTROBUN_APPLEAPIISSUER` | the App Store Connect API key's id and issuer |
   | `ELECTROBUN_APPLEAPIKEY_P8` | its `AuthKey_<id>.p8`, base64 |

Without them the `sign` job ends at its first step with a notice. With them,
Actions, desktop, Run workflow, with a release's tag (`v0.1.12`): the job
bundles that release's server (SHA-256 checked), signs and notarizes with the
certificate in a keychain of its own that only `codesign` and `security` may
use (and no cache restored in it), holds the result to the same hook check the
unsigned build passes and to `codesign --verify --deep --strict`, the hardened
runtime and the Developer ID on the bundled server, `spctl` and `stapler
validate` (app and DMG), and uploads the DMG. A separate job, which has the
workflow's only write permission and none of the secrets, then attaches it to
the release. Nothing unsigned is ever attached. The release's name is checked
(`^v?N.N.N$`) before it reaches `make`, which expands what it is given.

**Not exercised (this repository had no Developer ID certificate when this
was written), to look at with the first signed build:** that the bundled
`fenec-server` carries a valid signature with the hardened runtime
(notarization fails otherwise; the job greps for it); that the first launch's
unpacking (above) does not break the seal of a signed and notarized bundle, and
what a Gatekeeper check says about the app afterwards; whether the Keychain
items, once the app is signed, are readable only by it or by `bun` run by any
program of the user (above); that they keep their access across an update,
which needs the signing identity and bundle id to stay fixed.

## Not yet

* Updates: Electrobun makes bsdiff patches against a release it can download,
  and nothing is published; a new version is a new DMG.
* Intel Macs, Windows and Linux. Windows also needs `fenec_abi::lock` to take a
  lock there (`record_lock` is unix only), a Windows target in the release
  matrix, and a signing hook (Hutch does not sign on Windows).
* A stable origin for a file: the server listens on a port the system picks.
  A window's store is its own, so nothing a file's page keeps (its theme, its
  history) carries to the next open of the file.
* A `<file>.lock` (0 bytes) stays beside every file the app opens; that is
  `fenec-server`'s, not the app's.
