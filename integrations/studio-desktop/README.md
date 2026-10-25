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
([below](#signing-and-notarization)): an unsigned build opens after
`xattr -cr "fenec studio.app"` or *Open Anyway* in System Settings.

```
 fenec studio.app
 ├─ Bun main process ── src/bun/*   windows, menu, servers, Keychain
 │    ├─ launcher window            recent files, saved servers   (bundled page, RPC)
 │    ├─ new-database window        folder, name                  (bundled page, RPC)
 │    ├─ notice windows             every failure, in words       (bundled page, sandboxed)
 │    └─ one window per open thing  <server>/_studio/             (the server's page, sandboxed)
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

`make studio-desktop STUDIO_SERVER=0.1.11` bundles that release's server
instead: it downloads `fenec-v0.1.11-<target>.tar.gz` and checks its SHA-256
against the release's own `SHA256SUMS` (the same script, `scripts/get-server.sh
0.1.11`). Either way a server without `--studio` is refused with a message
saying so: releases before fenec studio (up to 0.1.10) have none.

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
package downloads (from Electrobun's GitHub release, checking the archive's
SHA-256 against the release's index) and which, left alone, installs itself in
`~/.hutch` and, if that download fails, runs an installer that appends a line
to your shell's rc file. `scripts/electrobun.mjs` is how everything here runs
it instead: `HUTCH_HOME` is `.hutch-home/` (ignored by git; Hutch, its Bun and
the Electrobun runtime, about 210 MB), and the resolver is handed an installer
that refuses, so no path ends in the global one. `electrobun` and `typescript`
are exact versions in `package.json`, held by `package-lock.json`; Hutch's
and Electrobun's by `hutch.config.ts`. Set `HUTCH_HOME` yourself to keep the
toolchain elsewhere.

### The checks

`make studio-desktop-test` runs, in order:

1. `tsc` over the sources, the unit tests and the config, then `test/*.test.ts`
   under `node --test` (names, the remote probe against a stand-in server, the
   wording of every failure, the token hand-off script run as a page would, the
   signing switches in `electrobun.config.ts`, `get-server.sh` against a
   release laid out in a directory).
2. The **test variant**: the same app built with `FENEC_DESKTOP_TEST_BUILD=1`,
   which adds `src/bun/testhooks.ts` and gives it an identifier of its own
   (`dev.fenecdb.studio.test`), so a run touches none of a real install's
   recent files, saved servers or Keychain items. The hooks read
   `FENEC_DESKTOP_REPORT` (a file the app appends facts to) and
   `FENEC_DESKTOP_CONTROL` (a file of commands it reads as it grows); a page
   reports through the event-only bridge a sandboxed window keeps. They are
   bundled only when `__TESTABLE__` is true, a build-time constant, and
   `scripts/checks/release.mjs` reads the shipped app's main bundle and fails if
   any of them is in it.
3. `scripts/checks/run.mjs`, which needs a window server (a logged-in Mac or a
   CI runner) and runs each in a process of its own, killing what one leaves
   behind:

   | check | what it holds |
   |---|---|
   | `release` | the shipped bundle has none of the hooks |
   | `warmup` | the first launch of a build (below) |
   | `files` | open, edit through the studio's own form, a second open only shows the window, close stops the server with exit 0, reopen has the edit (also at once, while the first server is still stopping); a file another program holds is a message and **Try again** works once it lets go; a missing file, a folder, a file that is not a database (left untouched) |
   | `server-dies` | `kill -9` the server under a window: a message, the window closed, **Open again** works |
   | `orphan` | the app killed: the server outlives it and keeps the lock; the next launch stops it |
   | `sandbox` | no RPC bridge in a studio window, no navigating off its origin, links out only as web addresses, to the browser |
   | `remote` | saved servers: token in the Keychain and in no `state.json` or process argument; the studio signed in; every failure in words (wrong token, no token, studio off, unreachable); edit; remove deletes the Keychain item |
   | `newdb` | the folder dialog (answered by the check), a name, the file made and opened; an existing name refused, case-insensitively, the file untouched; bad names; Cancel |
   | `finder` | `open -a <app> <file>` with the app not running: the file arrives as `open-url`, spaces and non-ASCII letters included |

   `ONLY=files,remote npm run checks` runs a subset (with the test variant
   built: `FENEC_DESKTOP_TEST_BUILD=1 npm run build`). A failure's logs are in
   `$SD_DIR` (default `fenec-desktop-checks` in the temp directory):
   `<check>.log` and the app's own `<check>.app.log`.

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
| `src/bun/server.ts` | **the one place a server is started**: `fenec-server --file F --studio --http 127.0.0.1:0` with a random 32-byte token in `FENEC_HTTP_TOKEN`, found by the `listening on:` line on stderr; SIGTERM when the window closes (sync and checkpoint on the way down), SIGKILL after 30 s; none of the user's `FENEC_*` variables reach it |
| `src/bun/session.ts` | **the one place a token reaches a page** (below) |
| `src/bun/windows.ts` | what a studio window is held to; the notice window |
| `src/bun/app.ts` | open a file or a saved server, make a new database, keep the saved list; one window per `file:<real path>` or `remote:<id>` |
| `src/bun/pages.ts` | the launcher and the new-database window and what they may ask (`src/shared/rpc.ts`) |
| `src/bun/messages.ts`, `remote.ts`, `names.ts` | every failure in words, the probe of a saved server, the check of a new name: pure, unit-tested |
| `src/bun/strays.ts` | a server an earlier run of the app left behind (below) |
| `src/bun/secrets.ts`, `store.ts` | Keychain (`Bun.secrets`) and `state.json` |
| `src/launcher`, `src/newdb`, `src/notice`, `src/ui` | the app's own pages |

A **saved server** is a window on that server's own `/_studio/`: same origin,
so no CORS to set up, and the studio is the one that server was built with.
Before the window opens the app asks the server two things (`remote.ts`) and
says what is wrong in words instead of opening a blank window: is there a
studio at `/_studio/` (served before any token), and does `/_whoami` take the
saved token. A connection's URL, name and tenant are in `state.json` under the
app's data folder (`~/Library/Application Support/dev.fenecdb.studio/stable/`);
its token is in the Keychain, service `dev.fenecdb.studio`, name
`connection:<id>`, and nowhere else. The Keychain item's access list names the
app: another program asking for it gets a password prompt.

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
  script that runs at document start, before the page's modules, and writes the
  key the studio already reads (`sessionStorage["fenec-studio"] =
  {server, token, tenant}`, `studio/connect.js`) **only if `location.origin` is
  the server's own**; the test runs the script as a page would, with another
  origin, a lookalike host and a token that tries to break out of the string.
* **No bridge to the main process.** A studio window is `sandbox: true`: the
  RPC bridge to Bun is not injected, only the event-only `__electrobunSendToHost`
  a window keeps (`checks/sandbox.mjs` lists every global and fails on any other).
  The app's own pages (launcher, new database) have the bridge and show nothing
  a server sent; a notice window is sandboxed and draws what it is told with
  `textContent`, so what a server said is text there, never markup.
* **A window that cannot leave its server.** Navigation rules allow the
  server's origin and nothing else (the constructor's option did not take in
  2.0.4; `setNavigationRules` does); a link to another site, `window.open`
  included, goes to the default browser as a web address, never as a `file:` or a
  custom scheme.
* **One process per file, never two over a file.** `fenec-server` takes
  `<file>.lock`; a second program is refused and the app says so. The app
  never starts a second server on a file it is still starting one for.

When `fenec-server` learns to exit when its stdin closes
(`--exit-on-stdin-close`) and `studio/connect.js` to read a frozen
`window.fenecHost.session` first, two places change and nothing else:
`spawnServer` in `server.ts` (the flag, and a stdin pipe the app holds open for
the server's life) and `sessionPreload` in `session.ts` (define that property
instead of writing storage). `strays.ts` and `checks/orphan.mjs` then go.

**What is not covered:** the app is as exposed as the Mac it runs on to a
program that can already read the user's files or process memory; the token of
a local server lives in the app's and the server's memory and in the page's
session storage until the window closes. A saved server on plain `http://` to
another machine sends its token unencrypted: the launcher warns where the
address is typed.

### A server the app left behind

macOS has no parent-death signal, so an app that is killed (not quit) cannot
stop its servers, and an orphaned server keeps the file's lock. Every server
the app starts is noted in `servers.json` beside `state.json`, and the next
launch stops the ones still running that are still *that* server (its
arguments say `--file <that file> --studio`, because a pid is reused, after a
reboot above all).

## What a failure looks like

Every one is a window of the app's own, never a native alert (a native message
box is a synchronous call that stops the main process, and with it every
server's supervision, until someone clicks):

| | message | button |
|---|---|---|
| the file is open in another program | "This database is open in another program: close it there first; two programs over one file would corrupt it", with what the server said | Try again |
| the server could not start | "Could not open the database", or "not a fenecdb database" for a file that is something else (it is left as it was) | Try again |
| the server dies under a window | "The database server stopped; nothing written before it stopped is lost", the exit and its last lines | Open again |
| a saved server is unreachable | "Could not reach the server": nothing is listening, the name does not resolve, no answer in 6 seconds, its certificate is not accepted | Edit connection |
| it has no studio | "This server has no studio: start it with `--studio`" | |
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

`.github/workflows/desktop.yml` does it on main and by hand where a maintainer
has added these repository secrets, and is skipped cleanly otherwise:

| secret | what |
|---|---|
| `ELECTROBUN_DEVELOPER_ID` | the certificate's name |
| `MACOS_CERTIFICATE_P12`, `MACOS_CERTIFICATE_PASSWORD` | the Developer ID Application certificate and its key as a `.p12`, base64, and its password (a runner has no keychain to read the certificate from) |
| `ELECTROBUN_APPLEAPIKEY`, `ELECTROBUN_APPLEAPIISSUER` | the App Store Connect API key's id and issuer |
| `ELECTROBUN_APPLEAPIKEY_P8` | its `AuthKey_<id>.p8`, base64 |

Run by hand with a release's tag, the workflow bundles that release's server
(SHA-256 checked), and attaches the DMG to the release **only after** `codesign
--verify --deep --strict`, `spctl`, and `stapler validate` pass on the app and
the DMG. Nothing unsigned is ever attached.

**Not exercised (this repository had no Developer ID certificate when this
was written), to look at with the first signed build:** that the bundled
`fenec-server` carries a valid signature with the hardened runtime
(notarization fails otherwise); that the first launch's unpacking (above) does
not break the seal of a signed and notarized bundle, and what a Gatekeeper
check says about the app afterwards; that the Keychain items keep their access
across an update, which needs the signing identity and bundle id to stay fixed.

## Not yet

* Updates: Electrobun makes bsdiff patches against a release it can download,
  and nothing is published; a new version is a new DMG.
* Intel Macs, Windows and Linux. Windows also needs `fenec_abi::lock` to take a
  lock there (`record_lock` is unix only), a Windows target in the release
  matrix, and a signing hook (Hutch does not sign on Windows).
* A stable origin: the server listens on a port the system picks, so each launch
  is a new origin and what the studio keeps in `localStorage` (its theme) is not
  kept.
* A `<file>.lock` (0 bytes) stays beside every file the app opens; that is
  `fenec-server`'s, not the app's.
