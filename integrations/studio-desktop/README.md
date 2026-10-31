# fenec studio, desktop (beta)

[fenec studio](../../site/content/docs/studio.html) as a Mac app: open a
`.fenec` file with nothing to start, make a new database, serve one to the
apps on this Mac or your network, back it up and restore it, or keep saved
connections to servers. It is **a window on a studio a `fenec-server`
serves**, not a second implementation of it: the app bundles `fenec-server`
and the `fenec` shell, starts a server per open or served file, and shows its
own `/_studio/` page, so what the window does is what the studio does in a
browser. Nothing of a database is written by the app itself: a server makes a
new file, `fenec backup` takes an image, `fenec restore` makes a file of one.

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
 │    ├─ main window                databases, servers, connections, backups, serve sheet  (bundled page, RPC)
 │    ├─ new-database window        name, folder, first collection, sync, from a backup    (bundled page, RPC)
 │    ├─ notice windows             every failure, in words       (bundled page, event bridge only)
 │    └─ one window per open thing  <server>/_studio/             (the server's page, event bridge only)
 ├─ fenec-server --file F --http H:P --sync S ... --exit-on-stdin-close
 │       one a file at most: a window's (127.0.0.1:0), a served database's (its port and
 │       reach), or one for a backup alone; its tokens in its environment
 └─ fenec backup | verify | restore | key     backups; the replication token in its environment
```

## Build, run, test

You need a Mac with the Xcode command line tools (`sips`, `iconutil`,
`hdiutil`), Node 22 or later with npm, and Rust (`rustup`, for the server).
Nothing is installed globally and no shell file is touched.

```bash
make studio-desktop        # the unsigned .app and .dmg
make studio-desktop-test   # type check, unit tests, every scripted check
```

`make studio-desktop` builds `fenec-server` and the `fenec` shell from this
checkout (`scripts/get-server.sh`), makes the icon, and builds. The results:

```
integrations/studio-desktop/build/stable-macos-arm64/fenec studio.app
integrations/studio-desktop/artifacts/macos-arm64-fenecstudio.dmg
```

`make studio-desktop STUDIO_SERVER=0.1.12` bundles that release's server and
shell instead: it downloads `fenec-v0.1.12-<target>.tar.gz`, which holds both,
and checks its SHA-256 against the release's own `SHA256SUMS` (the same
script, `scripts/get-server.sh 0.1.12`). Either way a server without the two
flags the app starts it with, `--studio` and `--exit-on-stdin-close`, is
refused with a message saying so: the first release with `--studio` is 0.1.11
and the first with both is 0.1.12, and every release before them (up to
0.1.10) has neither. A shell without `fenec verify` and `--key-file`, which the
backups use, is refused the same way (0.1.12 has both).

By hand, from this directory:

```bash
npm ci --ignore-scripts      # electrobun (the bootstrap) and typescript, pinned
sh scripts/get-server.sh     # bin/fenec-server and bin/fenec, built from this checkout
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

**The tool and what it fetches are held to `hutch-pin.json`**, in this
repository, and `scripts/electrobun.mjs` checks it on every run, cached or
fresh, before it builds anything:

* **Hutch**: the SHA-256 of the archive as it is downloaded, and of the two
  binaries in it. The release's own index sits in the same place as the archive
  and proves little against that place; a CI cache of `.hutch-home/` is not
  trusted for being one. The resolver's fallback to a Hutch that is already at
  `.hutch-home/bin` (which would be a cache's, unpinned) and
  `ELECTROBUN_HUTCH_BINARY` are switched off.
* **What Hutch fetches** (`sync`, which runs nothing of the app's, goes first): a
  digest of every file of the **Electrobun runtime** (the native library, the
  launcher, the main process's API sources and the preloads, all of which ship
  inside the app), of the **Bun** it bundles and runs, and of **Cottontail**
  (`scripts/tree-digest.mjs`: path, executable bit and content of each file,
  hidden ones too). Hutch checks these only against a lock it writes beside the
  project (`.hutch/dependencies.lock`, not in git, with no hash for its Bun), so
  a changed file there would pass it; here it stops the build, before it
  starts, naming the directory. (A changed byte of the preload, of the native
  library and of the Bun were each refused, and a changed `bin/hutch` in the
  cache is not run.)

A different Electrobun is a change to `hutch-pin.json`, which a pull request
shows: `node scripts/electrobun.mjs --print-runtime-pin` prints the runtime's
digests, and the script says which hashes a mismatch is.

### The checks

`make studio-desktop-test` runs, in order:

1. `tsc` over the sources, the unit tests and the config, then `test/*.test.ts`
   under `node --test` (names and a first collection's statement, the remote
   probe against stand-in servers, a redirect that must not carry a token, the
   wording of every failure, the token hand-off script run as a page would, the
   server's environment, what a file is before a server is started on it, the
   token hand-off run through the studio's own `connect.js`, the signing
   switches in `electrobun.config.ts`, the pin, `get-server.sh` against a
   release laid out in a directory, a server's arguments and the command the
   serve sheet shows, a served token kept in a stand-in Keychain, the figures
   read from a server's `/_metrics`), and two that drive **the real binaries
   in `bin/` through the app's own code**, Node's spawn in place of Bun's:
   `process.test.ts` (a server on port 0, its token in its environment and not
   its arguments, a stop that is exit 0, a held file, a port another program
   answers on) and `backups.test.ts` (a backup, verify, a restore into a new
   file and never over one, sealed with a key). Where `bin/` is empty they are
   skipped, saying why.
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
   | `orphan` | the app killed with two files open, one opened after the other, and a third served: **the descriptor tables (lsof), read while the app is alive, say the write end of each server's stdin is the app's alone** (a control starts a process that does hold another's and requires the reading to find it), then every server ends by itself (the end of its stdin), the served one too, and every file is free for another program. A process is not stopped: on a hosted macOS runner a SIGSTOPped server made `ps` hang, and the job with it |
   | `sandbox` | the session is `window.fenecHost`, frozen, in the page's own world, and in no storage; the studio has no sign-in form and no Sign out; only the event bridge in a studio window, no navigating off its origin; **a non-web address is dropped by the app** (the page asked, and nothing reached the system) and the first web link is handed to the browser, which the rate limit of one in three seconds does not explain |
   | `pages` | the launcher, the new-database window and a notice window cannot be navigated anywhere else (a web page, a file, another of the app's pages): without the rule, the launcher goes to example.com |
   | `storage` | what a window keeps (the studio's history, theme) is its own, **read from a second window on the same origin** (`store-window`, a test hook): a window in another store does not see a file window's, one in the same store does (the control); two saved servers on one address do not share it; a saved server's survives its window; **a removed connection's store, opened again under its id, is empty**, whether or not a window was open on it |
   | `environment` | the server is started with the allowlist and its token, whatever the app was started with (`OTEL_*`, `FENEC_JWT_SECRET`, and the checks' own variables stay out) |
   | `remote` | saved servers: token in the Keychain and in no `state.json` or process argument; the studio signed in; every failure in words (wrong token, no token, studio off, unreachable, a redirect that the token does not follow); an address changed without a token forgets the saved one, and nothing is sent to the new address; edit; Check now, with no window; two clicks open one window; remove deletes the Keychain item |
   | `newdb` | the folder dialog (answered by the check), a name, the file made and opened; an existing name refused, case-insensitively, the file untouched; bad names; a first collection and `--sync always`, made by a server; a field list that could close itself, refused with no file made; "Serve it" hands over to the serve sheet, the file a database already; Cancel |
   | `serve` | the main window draws one section at a time; the serve sheet from the Databases list: its command holds no token, Show shows the Keychain's; Start: any free port, the token from the Keychain and in no argument or `state.json`, 401 without it; Open is a window on that server, not a second one, and closing it leaves the server running; its output and figures; Restart with a port and the network (the same token); New token taken at the next start; a port another program answers on and a file another program serves, each said; a server killed under it, said, and **Start again**; Stop is exit 0 and the file free; a quit stops it; **a relaunch starts nothing** and lists it stopped; Remove deletes the Keychain item |
   | `backups` | a backup with nothing running (a server for it alone, stopped with exit 0, no window) and one through the window's own server; verify; a restore into a new file that opens with the rows; never over a file there or the open one; a key made with `fenec key` (0600, never written over), a sealed backup that is no database in the clear, verified and restored; a new database from a folder of backups; a database served with `--sync off`, refused with the reason |
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
removed as they go (a served database's, `serve:<id>`, by `forgetServeItems`
at the end of each check that serves one). Nothing of a real install (`dev.fenecdb.studio`) is read or
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
| `src/bun/server.ts`, `process.ts` | **the one place a server is started**: `fenec-server --file F --http H:P --sync S [...] --exit-on-stdin-close` (`serving.ts` writes the arguments) with a random 32-byte token in `FENEC_HTTP_TOKEN` (a served database's: the Keychain's) and, for a file whose backups are set up or a server started to take one, another in `FENEC_REPLICATION_TOKEN`, and **a pipe on its stdin that the app holds for the server's life**, found by the `fenec-http <version> listening on: http://<host>:<port>` line on stderr (the whole line, anchored, on the host and port that were asked for: a line that merely contains the words, or another address, is refused and the server stopped; `listening.ts`), which is read, with stdout, for as long as it runs into a ring of its last 1 000 lines; SIGTERM to stop it (sync and checkpoint on the way down), SIGKILL after 30 s; given a few variables (`env.ts`: `HOME`, `PATH`, `TMPDIR`, `LANG`, `LC_*`, `USER`, `LOGNAME`, `TZ`) and nothing else of the app's environment. `process.ts` takes the spawn it is handed, so `node --test` drives the real binary through it |
| `src/bun/session.ts` | **the one place a token reaches a page**: the preload that defines `window.fenecHost` (below) |
| `src/bun/windows.ts` | what a studio window is held to; the notice window; `holdTo`, which holds every window of the app to its address |
| `src/bun/app.ts` | open a file or a saved server, make a new database, serve, stop and restart a database, take and restore backups, keep the lists; **one server a file**, whatever starts or stops one going through the file's lane one after another; one window per `file:<real path>` or `remote:<id>`; one open of a thing at a time; what a quit waits for |
| `src/bun/serving.ts`, `metrics.ts`, `backups.ts` | a server's arguments, the command the serve sheet shows, a served database's facts and the snippets an app connects with; the figures read from a server's `/_metrics`; `fenec backup`, `verify`, `restore` and `key` run with the app's environment rules, and a restore put in place without writing over a file: pure, or over a runner handed in, unit-tested |
| `src/bun/pages.ts` | the main window (`launcher` in the code, as it began) and the new-database window and what they may ask (`src/shared/rpc.ts`) |
| `src/bun/messages.ts`, `remote.ts`, `names.ts`, `dbfile.ts`, `env.ts`, `token.ts`, `listening.ts` | every failure in words, the probe of a saved server, the check of a new name and of a first collection, what a file is, the environment of what the app starts, a token and the address (or the file) it is for, the lines of a server's stderr the app acts on: pure, unit-tested |
| `src/bun/secrets.ts`, `store.ts` | Keychain (`Bun.secrets`) and `state.json` |
| `src/bun/instance.ts`, `clipboard.ts` | one instance of the app at a time; a token copied to the clipboard cleared a minute later: pure but for a socket and a clipboard handed in, unit-tested |
| `src/launcher`, `src/newdb`, `src/notice`, `src/ui` | the app's own pages |

**One app at a time.** Two instances would each keep `state.json` and the
Keychain items as if alone: one's **Remove** would delete the token under a
server the other runs, and each would write the other's lists over. macOS opens
the running app rather than a second from Finder, the Dock and `open`, but not
from `open -n` or the binary run by hand, and Electrobun 2.0.4 has no
single-instance switch; so the first instance listens on a socket of its own in
the user's temporary folder (`$TMPDIR/<identifier>.instance`, a folder only the
user can reach), and a later one binds there first: refused, it asks the
instance there to show its main window and quits before it reads a list or the
Keychain. A socket an instance that ended left answers nothing and is taken
over (`instance.ts`, `test/instance.test.ts`). The test variant's identifier is
its own, so a check never meets an installed app.

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
nowhere else. **A token is for the address it was saved with**: the Keychain
item holds the token *and that address* (`token.ts`), and a token read back is
used only for the address it names, so a connection edited between the Keychain's
answer and the window (or an item left from before an edit) sends nothing to a
new address; and the connection is read again after the Keychain answers, and
the open stops if it changed. Changing a connection's address with no token
typed is refused as well, with the reason; **Forget token** in the edit form
takes a saved token away without a new one (a blank token field only ever means
"keep it"). A second click on a saved server while
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
made over), and starts a server on it for a moment, which takes an empty file
as a new database and, if one is asked for, runs its first collection's
`create collection` through `/query` with that server's token. A name is
checked as a file name (no `/`, `\`, `:`, no control character, no leading
dot, 255 bytes, and none of the controls that change the direction of text,
which would show `invoice<U+202E>slx.fenec` as `invoicenef.xls`), `.fenec`
is added once, and a server that then fails to start does not leave the empty
file behind. A first collection is held to a narrow grammar (`names.ts`: a
name, then fields of a known type and an optional index), so nothing typed can
close the field list and say something else; one the server still refuses (a
keyword for a name) leaves the database made and says so. The window also
takes when a write is safe on disk (`--sync`, kept for the file and used by
every server the app starts on it), a folder of backups to start from instead
(a restore, below), and **Serve it for my apps**, which hands the new file,
already a database, to the main window's serve sheet instead of a studio
window. (SQLite and PostgreSQL, which the design offers here too, are not
built yet: `fenec import` writes a file itself, and the app would have to hold
it to the single-writer rule another way.)

## Serving a database

The main window's serve sheet (Databases, **Serve…**) starts a server on a
file for the apps on this Mac or the network:

* **A port**, empty for any free one (port 0, named by the server's
  `listening on:` line; the apps then need the address again after each start),
  and **who can reach it**: 127.0.0.1, or 0.0.0.0 with a warning that plain
  http crosses the network unencrypted. A port is refused when **anything
  answers on 127.0.0.1 there**, which a bind does not show: the server binds
  with SO_REUSEADDR, under which macOS let a second server bind 127.0.0.1:P
  beside a first's 0.0.0.0:P (`process.test.ts` hit it), and the app and its
  windows reach every server on 127.0.0.1, with the token. The sheet asks as the
  port is typed ("Free now", "Taken").
* **The token** is made once (32 random bytes) and kept in the Keychain under
  `serve:<id>`, sealed for `file:<real path>` (`token.ts`), before any server
  is started with it, so the apps that hold it keep working across restarts;
  a file moved is another file and gets its own. The server gets it in its
  environment, never in argv, a URL, `state.json` or a log. **Show** asks the
  main process for it (the main window is the app's own page, held to
  `views://launcher/`); **Copy** puts it on the clipboard from the main
  process and takes it off a minute later, if the clipboard still holds it
  (something copied since is left alone; a quit takes it off at once,
  `clipboard.ts`); **New token** keeps a new one, taken at the next start.
* **Beside the data**: `--studio`, `--auto-compact off` when compaction is
  unticked (on by default, 0.5, the server's own), `--mcp`, `--metrics
  127.0.0.1:0` (on this Mac whoever reaches the data), and **when a write is
  safe on disk**. "What the app runs" is the command, its secrets as dots.
* **One server a file.** Serving a file open in a window closes the window
  (its server stops), starts the served one and opens the window again on it;
  opening a served file opens a window on its server with its token. A file
  another program serves is refused by the server's `<file>.lock` and said.

**How long a served server runs, decided:** until it is stopped or the app
quits. A quit stops every one, the one shutdown (SIGTERM, then the server's
sync and checkpoint), and its stdin's end stops it if the app is killed, so
**no server outlives the app** (a run of the shell is another matter: below). After a relaunch the Servers list shows each
served database **stopped**, with the settings and the port it ran with and
why it stopped (stopped, the app quit, exited), and **Start** starts it again
with them; nothing starts by itself. A server that started at launch would
take a port and a file's lock nobody asked for this time (the file may be open
in another program now), and one served to the network would be exposed again
without a decision. The design's **Start it when I log in** is not built: a
login item outlives the app, so its server would be one with nobody holding its
stdin, its token would have to sit where launchd reads it (a plist's
environment, which the token never goes in), and a later app would have to find
and adopt it, the registry and reaper this app was rid of.

The **Servers** list shows each served database, running or stopped: its
statements a second and median answer (between two reads of the server's own
`/_metrics`, every 2 s while it is in view), connections and file size, its
settings in words, where another machine reaches it and its metrics, how to
connect from curl, JavaScript, Python or a `.env` (the token left out), and the
server's output (stderr and stdout, its last lines, all 1 000 on **Show full
log**). Open studio, Settings (the sheet again: Restart server with what
changed), Restart, Stop, and for a stopped one Start and Remove (its settings
and its Keychain item go).

## Backups

Each database's backups go to a folder the user picks (Backups, **Set up**),
sealed with a key file or not, and are taken by the bundled shell:

* **Back up now**: `fenec backup http://127.0.0.1:<port> <folder>` adds an image
  of the database, taken while it runs, to the folder (`fenec archive`'s layout,
  so the shell's `verify`, `restore` and `prune` read it). It speaks the
  replication stream, so a server the app starts on a file **whose backups are
  set up** has a replication token in its environment (random, for that start,
  never stored) and `--replication-buffer 1` (1 MiB: an image needs none of the
  writes a replica that falls behind is sent, and the default is 64); the shell
  is given the token in its environment as well. **Only those:** a replication
  token also turns the server's change feed on, and `/_changes` answers every
  write to the data token, from the network for a database served there, which
  a file with no backups has no reason to offer. It is taken from the server
  serving the file, the one behind its window, or one started for the backup
  alone and stopped after (exit 0), whose `<file>.lock` refuses a file another
  program has; a server started before the backups were set up has no stream,
  and the backup says to restart it (Set up says so too). A server under
  `--sync off` serves no stream (it refuses a replication token): its backup is
  refused with the reason.
* **Sealed**: `--key-file` names a key **Make a new key…** writes with `fenec
  key` (0600, never over a file). A key **anywhere under the folder that was
  picked** is refused (the backups' folder, or the one above it when that is
  named after the database, as Choose… names it: the folder a user copies or
  syncs whole), compared as real paths, through links; so is one **on the disk
  the backups go to, when that is not the database's** (whoever takes the
  drive has both). A key others on the Mac may read is said, not refused, with
  its mode: one copied in may have lost `fenec key`'s 0600 (`keyPlacement`).
  Only the key's path is in `state.json`.
* **Verify**: `fenec verify`, what the folder can restore.
* **Restore…**: `fenec restore <folder> <new file> [--to-change n]`, from the
  newest image or any listed. **`fenec restore` writes over whatever is at the
  name it is given, a file a server is serving included** (tried: it did, exit
  0), so it writes into a folder of its own made for that restore beside the
  new file (`mkdtemp`, `.<name>.restoring-XXXXXX/db.fenec`), and the result is
  put in place with `link`, which fails if the name is taken (on a disk without
  hard links, exFAT or FAT32, a copy with `COPYFILE_EXCL`, fsynced before it is
  said to be done): never over a file. **A folder, not a name:** the shell
  writes beside its output too (`<out>.restoring`, `<out>.compacting`, its
  lock), with the extension cut, so two restores to one name, each given a
  name of its own, shared those and broke each other; the folder goes whole
  once the restore ends, however it ends. Two restores to one name also wait
  for each other in the app, so the second finds the first's file and is
  refused, and the Restore button is off while one runs
  (`test/backups.test.ts` runs three at once). The new file opens in a window.
  New database's **A backup** is the same restore.
* **The shell's runs end with the app's quit, not its death.** `fenec backup`,
  `verify`, `restore` and `key` do not read their stdin, so a pipe the app held
  would not stop one, and macOS has no parent-death signal. A quit stops every
  run (SIGTERM, which a one-shot command takes as its end) and waits for its
  caller to see it end, before the servers stop: a backup stopped so leaves no
  image (an image is written as `.tmp` and renamed once whole), a restore
  nothing at the new name, its folder removed; no run starts after (`stopCli`).
  An app **killed** (not quit) leaves a run under way to finish on its own: a
  backup adds its image, and a restore's file stays in its hidden folder,
  `.<name>.restoring-XXXXXX`, beside where it was going.
* Not built, each for a reason: a **continuous archive** (`fenec archive`, the
  design's "written as the server writes") runs until interrupted and has no
  `--exit-on-stdin-close`, so killing the app would leave it running; and
  `prune` from the app.

## The security model of the token hand-off

A studio is "a page with no authority of its own": it holds a token and sends
it. So a window that shows one gets:

* **A token that is never in a URL, a query, a fragment or argv.** For a file
  the app makes 32 random bytes per server start, gives them to that server in
  its environment (`ps` shows every user's processes' arguments, not their
  environment), and hands the page the same token in its preload.
  For a saved server, and a served database, the token comes from the Keychain. The preload is a user
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
* **A chosen port is looked at, not held, until the server binds it.** The app
  refuses a port something answers on at 127.0.0.1 (Serving, above), then
  starts the server, which binds it a moment later: a program that binds
  127.0.0.1 on that port in between gets the app's and its windows'
  connections, the token with them; so does one that binds it by name at any
  time beside a server on 0.0.0.0, since a bind by name takes loopback from a
  wildcard one. It takes a program already running on this Mac, and one
  running as you can read the token anyway (above). A window's server, on
  127.0.0.1 and a port the system picks, has no such moment.
* **A token copied to the clipboard** is there for a minute, as anything
  copied is: for every program of yours to read, and for Universal Clipboard to
  carry to your other devices where it is on. It is taken off then if the
  clipboard still holds it, and at a quit.
* **A server's page can start a download into `~/Downloads` without a prompt**
  (a `Content-Disposition: attachment`, a link with `download`): Electrobun 2.0.4
  lets any download through whatever the navigation rules say, and gives the app
  only `download-started`, `-progress`, `-completed` and `-failed` events after
  the fact, no hook that could refuse one. The file is the server's to name and
  to fill, as a browser's download is a site's; macOS puts the quarantine flag on
  it, and Gatekeeper checks what is opened from it. The page is the studio of a
  server you chose to open, and no studio route sends one that it was not asked
  for. If a later Electrobun has a decision hook, studio windows should deny.
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
(`checks/orphan.mjs` opens two files one after the other, reads the descriptor
tables with lsof to see that each server's stdin is written to by the app alone,
kills the app, and needs both to end and both files to be free). A quit waits for every server, one
being started and one whose window closed a moment ago included.

A run of the `fenec` shell is not a server, and is not held so: a quit stops
it, and a killed app leaves it to finish alone (Backups, above).

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
| a served database's server exits unasked | "A database server stopped", the exit and its last lines; its windows closed | Start again |
| a port another program answers on | "Port 8080 is taken", before any server is started, or what the server said | Change the port |
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
main's. **A maintainer sets it up once, in this order** (RELEASING.md has the
`gh` commands):

1. **First**, Settings, Environments, New environment: **`desktop-signing`**,
   with a required reviewer (a dispatch then waits for a person) and the
   deployment branch limited to `main`. **No secret goes in before this is
   done**: a secret in an Environment with no rules yet is readable by a job from
   any branch that names it.
2. **Then**, in that Environment (not in the repository's secrets, where any job
   could read them):

   | secret | what |
   |---|---|
   | `ELECTROBUN_DEVELOPER_ID` | the certificate's name |
   | `MACOS_CERTIFICATE_P12`, `MACOS_CERTIFICATE_PASSWORD` | the Developer ID Application certificate and its key as a `.p12`, base64, and its password (a runner has no keychain to read the certificate from) |
   | `ELECTROBUN_APPLEAPIKEY`, `ELECTROBUN_APPLEAPIISSUER` | the App Store Connect API key's id and issuer |
   | `ELECTROBUN_APPLEAPIKEY_P8` | its `AuthKey_<id>.p8`, base64 |

The `sign` job's first step reads the Environment's rules back with the job's own
token and fails, before it uses anything, unless it has a required reviewer and
deployment branches limited to `main` alone. Whether that token may read them is
GitHub's to say (the Environments API asks for administration rights, which a
workflow token is not given as far as the permissions go): where it may not, the
step warns and goes on, and the order above is what stands behind it. Without
the secrets the job ends at its second step with a notice. With them,
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

* From the design: import from SQLite or PostgreSQL, a folder of tenants
  (`--dir`) served as a node, folders watched for databases, continuous
  archives and Start it when I log in (both above, for their reasons).

* Updates: Electrobun makes bsdiff patches against a release it can download,
  and nothing is published; a new version is a new DMG.
* Intel Macs, Windows and Linux. Windows also needs `fenec_abi::lock` to take a
  lock there (`record::take` is Unix only), a Windows target in the release
  matrix, and a signing hook (Hutch does not sign on Windows).
* A stable origin for a file: the server listens on a port the system picks.
  A window's store is its own, so nothing a file's page keeps (its theme, its
  history) carries to the next open of the file.
* A `<file>.lock` stays beside every file the app opens, holding the last
  holder's `<pid> <program>` line, which a refusal names it by; that is
  `fenec-server`'s, not the app's.
* A database served to the network is reached by the app's own windows on
  127.0.0.1. A program of another user that binds 127.0.0.1 on that port after
  the server started would take those connections (the check before the start
  covers the time before it); the Mac's own rules on binding a port another
  user holds decide whether it can.
