//! `fenec` -- the fenecdb shell.
//!
//! ```text
//! fenec                      in-memory session
//! fenec data.fenec             open the file (create it when missing)
//! fenec data.fenec -c "get docs limit 5"
//! echo "get docs" | fenec data.fenec
//! fenec data.fenec < script.fenecql
//! fenec --read-only data.fenec   the file as it stands, and nothing written
//! fenec --version              `fenec <version>`, and nothing opened
//! ```
//!
//! A statement may run over several lines; [`Reader`] says when one runs.
//! A file is opened under its `<file>.lock` ([`held`]): refused while a
//! server, an agent's session, an app or another shell has it open.

#[cfg(feature = "backup")]
mod backup;
mod held;
#[cfg(feature = "import")]
mod import;
mod langs;
#[cfg(feature = "mcp")]
mod mcp;
#[cfg(any(feature = "backup", feature = "import"))]
mod stop;
mod types;

use fenec_core::prelude::*;
use fenec_ql::{Reading, Refusal, Rest};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// `--read-only`: the file the session opened, which it writes nothing to
/// -- its statements, `.checkpoint` and a `.save` over it alike -- set once
/// before the first runs.
static READ_ONLY: OnceLock<PathBuf> = OnceLock::new();

fn opened_read_only() -> bool {
    READ_ONLY.get().is_some()
}

const READ_ONLY_REFUSAL: &str = "opened --read-only, the shell writes nothing: write over \
     HTTP to the server that has the file open, or open it without --read-only once nothing has";

const USAGE: &str = r#"usage: fenec [file.fenec] [-c "<query>"]
       fenec --read-only <file.fenec>   the file as it stood at the open, beside
                                        a server writing it: no lock, no write
       fenec -V, --version              prints `fenec <version>` and exits

The shell holds <file.fenec>.lock while it has the file open: a file a
server, an agent's session (fenec mcp --file), an app or another shell has
open is refused, since two writers over one file corrupt it.
"#;

const HELP: &str = r#"
FenecQL summary
  create collection <name> ( <field> <type> [@hash|@sorted|@text|@hnsw(metric, m=.., ef_search=..)], ... )
  create index [if not exists] on <name> (<field>|<path>) @hash|@sorted|@text|@hnsw(..)
  drop collection [if exists] <name>
  put <name> { field: value, ... }        -- or [ {...}, {...} ]
  get <name> [select a,b] [where <expr>] [near <field> <vector> [ef N] [exact]]
           [order <field> [collate tr] [asc|desc], ...] [limit N] [offset N] [count]
           [lookup <name> on <child> [= <parent>] [required] <clauses...>]
           [lookup <name> on (<child>, ...) = (<parent>, ...) ...]   -- a key over several fields
  explain get <name> ...                  -- the path the query took, one row a step
  select a, b from <name> ...             -- the classic SQL order works too
  select o.id, c.name from orders o join customers c on c.id = o.customer_id
                                          -- a JOIN on a key is a lookup; CREATE TABLE, INDEX too
  set <name> { field: value } [where <expr>]
  del <name> [where <expr>]
  set | del ... [order <field> [desc]] [limit N] [returning * | a,b]   -- pick the rows, answer them
  collections | describe <name> | compact [<name>]

Types     bool  int  float  text  bytes  vector<N>  [type]  json
Paths     meta.source.rank reads into a json field, where a field goes
Case      email text nocase -- compared by its case fold everywhere, kept as written
Operators = != < <= > >=   ~ (text contains)   has (list contains)   in [..]
          and  or  not  is null  is not null
"#;

/// The import arm is only compiled with the `import` feature: the SQLite
/// reader, the PostgreSQL client and the `--follow` follower add 242 KB to
/// the binary and are unnecessary for embedded use.
#[cfg(feature = "import")]
const IMPORT_HELP: &str = r#"
Import
  fenec import <file.sqlite|postgres://...> --table <name> [--into <name>]
                                         load from SQLite or PostgreSQL;
                                         --follow keeps applying its changes
                                         for details: fenec import --help
"#;
#[cfg(not(feature = "import"))]
const IMPORT_HELP: &str = "";

#[cfg(feature = "backup")]
const BACKUP_HELP: &str = r#"
Backup
  fenec backup <http://primary> <file|dir>   the database, taken while it runs
  fenec archive <http://primary> <dir>       keep every write, until interrupted
  fenec restore <dir> <out> [--to <time>]    the database as it stood then
  fenec prune <dir> --keep <7d>              let go of what no restore since then needs
  fenec verify <dir>                         what the archive can restore, read as a restore would
                                         for details: fenec backup --help
"#;
#[cfg(not(feature = "backup"))]
const BACKUP_HELP: &str = "";

const TYPES_HELP: &str = r#"
Types and schemas
  fenec types <file.fenec|.fenecql>          generates `.d.ts` from the schema (stdout)
  fenec types --lang python|go|csharp <f>    a row of each collection in that language
  fenec types --schema | --fenecql <file>    the schema as code: Drizzle-style tables, or FenecQL
                                         for details: fenec types --help
"#;

#[cfg(feature = "mcp")]
const MCP_HELP: &str = r#"
MCP, for agents
  fenec mcp --file <path> [--write] [--memory <c>]   the MCP server over stdio, over a file
  fenec mcp --url <http://host:port/_mcp>             ... or in front of a server (FENEC_TOKEN)
                                         for details: fenec mcp --help
"#;
#[cfg(not(feature = "mcp"))]
const MCP_HELP: &str = "";

const SHELL_HELP: &str = r#"
Statements over several lines
  Typed, a read or a schema change on one line runs at Enter. A write, and a
  read over several lines, wait for the next line, which may go on with them
  (where, order, and ...): an empty line runs them, and so does a `;` after
  them; a `.` command leaves them waiting. Piped or pasted, a statement runs
  once the next one begins, and a `.` command waits behind it; -c reads its
  text whole. A `;` ends a statement: no clause goes on past one.

Shell commands
  .help        this text
  .tables      collections
  .stats       storage statistics
  .functions   registered functions and plugins
  .save <path> write a snapshot to a file
  .checkpoint  rewrite the file (saves the HNSW graph too -> fast open)
  .quit        exit
"#;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // The subcommand has to be caught before the positional path: the arm
    // below treats anything not starting with a dash as a file path.
    if args.first().is_some_and(|a| a == "types") {
        std::process::exit(types::main(&args[1..]));
    }

    if let Some(command @ ("backup" | "archive" | "restore" | "prune" | "verify" | "key")) =
        args.first().map(String::as_str)
    {
        #[cfg(feature = "backup")]
        std::process::exit(backup::main(command, &args[1..]));
        #[cfg(not(feature = "backup"))]
        {
            eprintln!("this binary was built without `{command}`: its `backup` feature is off");
            eprintln!("rebuild it: cargo build --release -p fenec-cli --features backup");
            std::process::exit(2);
        }
    }

    if args.first().is_some_and(|a| a == "mcp") {
        #[cfg(feature = "mcp")]
        std::process::exit(mcp::main(&args[1..]));
        #[cfg(not(feature = "mcp"))]
        {
            eprintln!("this binary was built without `mcp`: its `mcp` feature is off");
            eprintln!("rebuild it: cargo build --release -p fenec-cli --features mcp");
            std::process::exit(2);
        }
    }

    if args.first().is_some_and(|a| a == "import") {
        #[cfg(feature = "import")]
        std::process::exit(import::main(&args[1..]));
        #[cfg(not(feature = "import"))]
        {
            eprintln!("this binary was built with `--no-default-features`, without import support");
            eprintln!("rebuild it: cargo build --release -p fenec-cli --features import");
            std::process::exit(2);
        }
    }

    let mut path: Option<String> = None;
    let mut command: Option<String> = None;
    let mut read_only = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-c" | "--command" => {
                i += 1;
                command = args.get(i).cloned();
            }
            "--read-only" => read_only = true,
            "-h" | "--help" => {
                println!(
                    "{USAGE}{HELP}{IMPORT_HELP}{BACKUP_HELP}{MCP_HELP}{TYPES_HELP}{SHELL_HELP}"
                );
                return;
            }
            // Read as `--help` is, a flag wherever one stands (never `-c`'s
            // text), and answered before the file is opened: a package's
            // check, or a script asking which release it has, opens nothing.
            // `fenec version` stays a file named `version`, as any word is.
            "-V" | "--version" => {
                println!("fenec {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            other if !other.starts_with('-') => path = Some(other.to_string()),
            other => {
                eprintln!("unknown option: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    if read_only && path.is_none() {
        eprintln!("--read-only reads a file: name it (fenec --read-only data.fenec)");
        std::process::exit(2);
    }
    // Taken before the open, and let go after the database is (declared
    // before it, dropped after it). Opened beside a server, the shell's
    // write was appended where the server then appended its own, and the
    // file was left corrupt. Read-only, nothing is written and nothing cut,
    // so no lock is taken: the file as it stood at the open, a record a
    // server is in the middle of appending passed over as a torn tail.
    let _held = match (&path, read_only) {
        (Some(p), false) => match held::take(p, held::Way::Shell) {
            Ok(l) => Some(l),
            Err(why) => {
                eprintln!("{why}");
                std::process::exit(1);
            }
        },
        _ => None,
    };
    let mut db = match &path {
        Some(p) => {
            let opened = match read_only {
                true => fenec_core::fs::open_read_only(p),
                false => fenec_core::fs::open(p),
            };
            match opened {
                Ok(db) => db,
                Err(e) => {
                    eprintln!("could not open {p}: {e}");
                    std::process::exit(1);
                }
            }
        }
        None => Database::new(),
    };
    if let (true, Some(p)) = (read_only, &path) {
        let _ = READ_ONLY.set(std::fs::canonicalize(p).unwrap_or_else(|_| p.into()));
    }

    if let Some(cmd) = command {
        let mut code = if run(&mut db, &cmd) { 0 } else { 1 };
        // A one-shot write that never reached the disk must not exit 0: the
        // caller's script would go on believing it. Read-only, there is
        // nothing to push, and no sync is asked of the file.
        let pushed = if read_only { Ok(()) } else { db.sync() };
        if let Err(e) = pushed {
            eprintln!("could not push the writes to disk: {e}");
            code = 1;
        }
        std::process::exit(code);
    }

    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    if interactive {
        println!(
            "fenecdb {} — {}{}",
            fenec_core::VERSION,
            path.as_deref().unwrap_or("in-memory"),
            match read_only {
                true => " (read only: the file as it stood at the open)",
                false => "",
            }
        );
        println!("`.help` for help, `.quit` to exit\n");
    }

    let mut reader = Reader::new(interactive);
    loop {
        if interactive {
            print!(
                "{}",
                if reader.buf.is_empty() {
                    "fenec> "
                } else {
                    "  ... "
                }
            );
            let _ = io::stdout().flush();
        }
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            // A script's last statement runs at its end; typed, the end is
            // the user leaving, as `.quit` is.
            match interactive {
                true => reader.abandon(),
                false => reader.finish(&mut db),
            }
            break;
        }
        if !reader.line(&mut db, &line) {
            break;
        }
    }

    // Write a checkpoint on exit when there is a vector index: the next open
    // does not rebuild the graph (27 s -> 0.1 s at 100k vectors).
    let has_vectors = db.stats().iter().any(|s| !s.vector_indexes.is_empty());
    if path.is_some() && has_vectors && !read_only {
        match db.checkpoint() {
            Ok(()) => {
                if interactive {
                    println!("checkpoint written (the HNSW graph is persisted)");
                }
            }
            Err(e) => eprintln!("could not write the checkpoint: {e}"),
        }
    }
    let pushed = if read_only { Ok(()) } else { db.sync() };
    if let Err(e) = pushed {
        eprintln!("could not push the writes to disk: {e}");
        std::process::exit(1);
    }
}

/// How the shell reads its input into statements, typed, pasted or piped.
///
/// A FenecQL statement has no end of its own: `del notes` is whole, and so
/// is it with `where id = 3` on the line after. Run at the end of every
/// line whose brackets were closed, a statement written over lines ran a
/// line at a time -- the `del` deleted every row, then the `where` was
/// refused. So a statement runs once the parser says nothing can go on
/// with it (`fenec_ql::reading`): a `;` or another statement after it, or
/// the end of the input. Piped, the next line is always there to ask, and
/// a script reads as `-c` and `POST /query` read a text. Typed, the next
/// line is the user's to write, and Enter has to mean something:
///
/// - a read or a change of schema written on one line runs at Enter, as it
///   always did, since the shell answers as its user types;
/// - a write, and a read written over several lines, wait for the next
///   line: one that goes on with them (`where`, `order`, `and`) joins
///   them, an empty line or another statement runs them first, and a `;`
///   runs one at once. A `.` command leaves them waiting, and `.quit` or
///   the end of the input leaves them unrun, said so.
///
/// A paste is read as a pipe is: its next line is waiting already
/// ([`pending`]), and an empty line that was waiting so is white space.
/// In a script or a paste a `.` command read while a statement waits --
/// `.quit` too -- waits behind it, and runs once it has run or been
/// refused; and a statement a `;` ends at a line's end waits for the next
/// line there too, which a clause past the `;` would hold it back by.
///
/// Adding to FenecQL: a statement form is placed in [`kind`] (every one
/// is named, so a write cannot run at Enter by default), and its other
/// obligations -- `Parser::cut`, `lexer::Unclosed`, the corpus of
/// `a_statement_written_over_lines_reads_as_one` -- are fenec-ql's.
///
/// Rejected: running at `;` alone, as `psql` and `sqlite3`
/// do, which every typed read would pay for with a `;`; and running
/// every whole statement at Enter, holding back only a write with no
/// `where` -- `del notes where kind = 1` with `order at limit 1` on the next
/// line deletes as widely, and so does a `set` with an `and` to come.
struct Reader {
    /// Lines typed at a terminal, where a line's end may be all there is
    /// for now.
    typed: bool,
    /// What is read and not yet run: one statement at most, whole or not.
    buf: String,
    /// The strings and brackets `buf` leaves open, which no statement can
    /// be whole in: read again at each line, a document of 10 000 lines
    /// was the text parsed 10 000 times.
    gate: fenec_ql::lexer::Unclosed,
    held: Held,
    /// Whether another line is waiting already, asked once a line.
    more: Option<bool>,
    /// A line was waiting already when this one's statement was left
    /// waiting: an empty line read next is a paste's white space, not
    /// Enter typed. Asked of the empty line itself, a paste that came in
    /// pieces -- iTerm2 paces one about 15 ms apart, a link adds its own
    /// -- had its blank line run `del notes` before the `where` came.
    waiting: bool,
    /// `.` commands of a script or a paste read while a statement waited,
    /// run once it has run or been refused: run at once, `.tables` between
    /// `del notes` and its `where` ran the `del` alone first.
    deferred: Vec<String>,
    /// A statement ran as this line ended, typed; and as the line before
    /// it did, which a line that begins no statement may have gone on
    /// with.
    ran: bool,
    ran_before: bool,
    /// Why a statement waits has been said, once a session.
    told: bool,
    /// A `.quit` ran: no line more is read.
    quit: bool,
}

/// What [`Reader::buf`] holds, once its strings and brackets are closed.
enum Held {
    /// Nothing whole and nothing refused: empty, or a string or a bracket
    /// open.
    Nothing,
    /// A statement read whole, waiting for the next line.
    Whole,
    /// A statement refused as far as the text goes: whether that stands is
    /// for the next line to say (`Refusal::stands`), and where in `buf` the
    /// text refused ends. Taken from where the latest line began, the lines
    /// read inside a string or a bracket since were dropped with the
    /// refusal, and the last of them run alone -- `put notes {body: "a
    /// note of two lines:` then `del notes -- the end"}` after a refused
    /// statement deleted every row.
    Refused(fenec_ql::Refusal, usize),
}

impl Reader {
    fn new(typed: bool) -> Reader {
        Reader {
            typed,
            buf: String::new(),
            gate: Default::default(),
            held: Held::Nothing,
            more: None,
            waiting: false,
            deferred: Vec::new(),
            ran: false,
            ran_before: false,
            told: false,
            quit: false,
        }
    }

    /// Whether a next line is there to ask before a statement is judged:
    /// always where the input is not typed, and typed where a paste or a
    /// line typed ahead is waiting.
    fn more(&mut self) -> bool {
        !self.typed || *self.more.get_or_insert_with(pending)
    }

    /// A line read; `false` to quit.
    fn line(&mut self, db: &mut Database, line: &str) -> bool {
        self.more = None;
        self.ran_before = std::mem::take(&mut self.ran);
        let queued = std::mem::take(&mut self.waiting);
        self.step(db, line, queued);
        // Asked once a statement waits: typed with nothing after it, a
        // write had asked it already, for the hint.
        self.waiting = self.typed && !matches!(self.held, Held::Nothing) && self.more();
        !self.quit
    }

    fn step(&mut self, db: &mut Database, line: &str, queued: bool) {
        let t = line.trim();
        // A `.` command where no string or bracket is open; inside one it
        // is text. Typed, it leaves what waits waiting, as `psql`'s do --
        // `del notes`, then `.tables` to look a name up, then its `where`
        // -- and `.quit` leaves it unrun. In a script or a paste it waits
        // in turn behind what waits, `.quit` too, and runs once that has
        // run or been refused: run at once, a quit between `del notes` and
        // its `where` ran the `del` alone.
        if t.starts_with('.') && !self.gate.is_open() {
            self.deferred.push(t.to_string());
            if self.typed && !self.more() {
                if !self.settled(db) {
                    self.abandon();
                }
            } else if self.buf.trim().is_empty() {
                self.finish(db);
            }
            return;
        }
        if t.is_empty() {
            // An empty line typed runs what waits, or says why it is
            // refused; pasted or piped it is white space, and so is one
            // that was waiting already as the line before was judged.
            if !self.gate.is_open()
                && !matches!(self.held, Held::Nothing)
                && !queued
                && !self.more()
            {
                self.finish(db);
            } else if !self.buf.is_empty() {
                self.buf.push_str(line);
                self.gate.feed(line);
            }
            return;
        }
        self.buf.push_str(line);
        self.gate.feed(line);
        if !self.gate.is_open() {
            self.read(db);
        }
    }

    /// `buf` read again, a line or more added to it.
    fn read(&mut self, db: &mut Database) {
        loop {
            let r = fenec_ql::reading(&self.buf);
            // A refusal held from the lines before stands where the lines
            // since leave it as it was: it is told, and they are read
            // without it.
            if let (Held::Refused(before, len), Rest::Refused(now)) = (&self.held, &r.rest) {
                if before.stands(now) {
                    let len = *len;
                    let Held::Refused(before, _) = std::mem::replace(&mut self.held, Held::Nothing)
                    else {
                        unreachable!()
                    };
                    self.refused(&before, &self.buf[..len]);
                    self.keep(len);
                    if !self.settled(db) || self.gate.is_open() || self.buf.trim().is_empty() {
                        return;
                    }
                    continue;
                }
            }
            return self.take(db, r);
        }
    }

    /// The `.` commands that waited behind a statement, run now that it
    /// has run or been refused; `false` where one quits, after which no
    /// statement read past the quit runs.
    fn settled(&mut self, db: &mut Database) -> bool {
        for cmd in std::mem::take(&mut self.deferred) {
            if !meta(db, &cmd) {
                self.quit = true;
                return false;
            }
        }
        true
    }

    /// What a reading of `buf` found: the whole statements run, and what is
    /// left kept, run, or told as refused.
    fn take(&mut self, db: &mut Database, r: Reading) {
        // A `;` that ends the text, piped or pasted: the next line may be
        // a clause written past it, which holds it back as `-c` refuses
        // the two, so it waits for that line as any statement does there.
        // Run at once, `del notes;` then `where id = 3` deleted every row.
        // Typed with nothing after it, it runs.
        let mut whole = &r.whole[..];
        let hold = match whole.split_last() {
            Some((&(a, _), before)) if matches!(r.rest, Rest::Nothing) && self.more() => {
                whole = before;
                Some(a)
            }
            _ => None,
        };
        // The first statement is the one that waited, whose `.` commands
        // come after it and before the rest.
        for &(a, b) in whole {
            run(db, &self.buf[a..b]);
            if !self.settled(db) {
                return;
            }
        }
        match r.rest {
            Rest::Nothing => match hold {
                Some(a) => {
                    self.keep(a);
                    self.held = Held::Whole;
                }
                None => {
                    self.clear();
                    self.settled(db);
                }
            },
            Rest::Last(at, s) => {
                self.keep(at);
                // A statement the text's end left open -- `describe
                // statement`, with a name to come -- waits, whatever its
                // kind, as a write does.
                if !r.open && self.runs_now(&s) {
                    run(db, &self.buf);
                    self.clear();
                    self.settled(db);
                    self.ran = true;
                    return;
                }
                self.held = Held::Whole;
                if self.typed && !self.told && !self.more() {
                    self.told = true;
                    println!(
                        "-- {} waits for the next line, which may go on with it: an empty line \
                         runs it, and so does a `;` after it",
                        match kind(&s) {
                            Kind::Write => "a write",
                            _ => "a statement over several lines",
                        }
                    );
                }
            }
            Rest::Refused(f) => {
                let at = f.at;
                self.keep(at);
                let f = Refusal {
                    at: 0,
                    begins: f.begins.map(|b| b - at),
                    place: f.place - at,
                    ..f
                };
                if !self.more() && f.stands_alone(&self.buf) {
                    self.refused(&f, &self.buf);
                    self.clear();
                    self.settled(db);
                } else {
                    self.held = Held::Refused(f, self.buf.len());
                }
            }
        }
    }

    /// Whether a statement read whole runs as its line ends: typed, with
    /// nothing more waiting, and a change of schema or a read on one line.
    fn runs_now(&mut self, s: &Statement) -> bool {
        self.typed
            && match kind(s) {
                Kind::Write => false,
                Kind::Read => !self.buf.trim_end().contains('\n'),
                Kind::Schema => true,
            }
            && !self.more()
    }

    /// The end of the input, a `.` command or an empty line typed: what
    /// `buf` holds is all there is.
    fn finish(&mut self, db: &mut Database) {
        if !self.buf.trim().is_empty() {
            let r = fenec_ql::reading(&self.buf);
            for &(a, b) in &r.whole {
                run(db, &self.buf[a..b]);
                if !self.settled(db) {
                    return self.clear();
                }
            }
            match &r.rest {
                Rest::Nothing => {}
                Rest::Last(at, _) => {
                    run(db, &self.buf[*at..]);
                }
                Rest::Refused(f) => self.refused(f, &self.buf),
            }
        }
        self.clear();
        self.settled(db);
    }

    /// Typed, the end of the session: what waits is not run, and said so.
    fn abandon(&mut self) {
        if !self.buf.trim().is_empty() {
            eprintln!("not run: {}", one_line(&self.buf));
        }
        for cmd in std::mem::take(&mut self.deferred) {
            eprintln!("not run: {cmd}");
        }
        self.clear();
    }

    /// `buf` from `at` on, its gate read again over what is kept.
    fn keep(&mut self, at: usize) {
        self.buf.drain(..at);
        self.gate = Default::default();
        self.gate.feed(&self.buf);
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.gate = Default::default();
        self.held = Held::Nothing;
    }

    /// A refusal told, of `text`: and where what was refused begins no
    /// statement, the statement held back with it, or the one it may have
    /// meant to go on with.
    fn refused(&self, f: &Refusal, text: &str) {
        eprintln!("error: {}", f.error);
        let Some(b) = f.begins else { return };
        let word = text[b..].split_whitespace().next().unwrap_or_default();
        if b > f.at {
            eprintln!(
                "  not run either, since `{word}` may go on with it: {}",
                one_line(&text[f.at..b])
            );
        } else if self.ran_before {
            eprintln!(
                "  `{word}` begins no statement: the one before it ran as its line ended. \
                 Write a statement on one line, or end a line with a word that needs more, \
                 such as `where`"
            );
        }
    }
}

/// A statement's text on one line, its white space one space: what a
/// message names it by. Gathered and joined, it was a `join` more.
fn one_line(text: &str) -> String {
    let mut out = String::new();
    for w in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
}

/// What a statement typed whole does at Enter: a write waits for the next
/// line, a read runs where it is on one line, a change of schema runs.
enum Kind {
    Write,
    Read,
    Schema,
}

/// Every statement named, none by a wildcard: a statement form added to
/// FenecQL has to be placed here, or a new write would run at Enter.
fn kind(s: &Statement) -> Kind {
    match s {
        // A call writes, whatever its body: it waits for the line after.
        Statement::Put { .. }
        | Statement::Update { .. }
        | Statement::Delete { .. }
        | Statement::Call(_) => Kind::Write,
        Statement::Select(_)
        | Statement::Explain(_)
        | Statement::ListCollections
        | Statement::Describe(_)
        | Statement::ListStatements
        | Statement::DescribeStatement(_) => Kind::Read,
        Statement::CreateCollection { .. }
        | Statement::DropCollection { .. }
        | Statement::CreateIndex { .. }
        | Statement::CreateComposite { .. }
        | Statement::DropComposite { .. }
        | Statement::AlterCollection { .. }
        | Statement::DropStatement { .. }
        | Statement::Compact(_) => Kind::Schema,
        // `begin ... end` closes itself, and runs as its line ends; a body
        // of one statement may go on with the next line -- its `where`, its
        // guard -- as the write it is would.
        Statement::CreateStatement(d) if d.block => Kind::Schema,
        Statement::CreateStatement(_) => Kind::Write,
    }
}

/// Whether a line is waiting to be read already -- a paste, or a line typed
/// ahead while a statement ran: the shell then reads on before it judges a
/// statement whole, as it does a file. A paste comes in one write, or in
/// pieces: iTerm2 paces one about 15 ms apart, and a link adds jitter, so
/// 40 ms, which no one types a line and Enter in, and which a read typed
/// on one line waits once before it runs. A paste in pieces further apart
/// is read as typed; bracketed paste, the terminal marking a paste's ends,
/// would need the terminal out of its line mode. A terminal hands a line at
/// a time to a read, so stdin's own buffer holds nothing past it.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn pending() -> bool {
    #[repr(C)]
    struct PollFd {
        fd: i32,
        events: i16,
        revents: i16,
    }
    // `nfds_t`: an `unsigned int` on Apple's systems and in Android's
    // bionic, an `unsigned long` in glibc and musl.
    #[cfg(any(target_vendor = "apple", target_os = "android"))]
    type Nfds = std::ffi::c_uint;
    #[cfg(all(target_os = "linux", not(target_vendor = "apple")))]
    type Nfds = std::ffi::c_ulong;
    extern "C" {
        fn poll(fds: *mut PollFd, n: Nfds, timeout: i32) -> i32;
    }
    let mut fd = PollFd {
        fd: 0,
        events: 0x1, // POLLIN
        revents: 0,
    };
    // SAFETY: one entry, which outlives the call.
    unsafe { poll(&mut fd, 1, 40) > 0 }
}

/// No `poll` declared here: typed lines are judged as they end.
#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn pending() -> bool {
    false
}

/// The `.` commands that end the session.
fn quits(word: &str) -> bool {
    matches!(word, ".quit" | ".exit" | ".q")
}

fn meta(db: &mut Database, cmd: &str) -> bool {
    let mut parts = cmd.splitn(2, char::is_whitespace);
    match parts.next().unwrap_or("") {
        w if quits(w) => return false,
        ".help" | ".h" => println!("{HELP}{IMPORT_HELP}{TYPES_HELP}{SHELL_HELP}"),
        ".tables" => {
            let names = db.collection_names();
            if names.is_empty() {
                println!("(no collections)");
            } else {
                for n in names {
                    println!("{n}");
                }
            }
        }
        ".stats" => {
            for s in db.stats() {
                println!(
                    "{:<16} {:>8} docs  {:>9}  dead {:>9}  {} segments",
                    s.name,
                    s.documents,
                    human(s.bytes),
                    human(s.dead_bytes),
                    s.segments
                );
                for v in &s.vector_indexes {
                    println!(
                        "{:<16}   @hnsw {}: {} vectors × {} dims ({}) arena {}",
                        "",
                        v.field,
                        v.count,
                        v.dim,
                        if v.precision == VecPrec::F16 {
                            "f16"
                        } else {
                            "f32"
                        },
                        human(v.arena_bytes)
                    );
                }
            }
        }
        ".functions" => {
            let r = db.registry();
            println!("plugins:");
            if r.plugins().is_empty() {
                println!("  (none)");
            }
            for (n, v) in r.plugins() {
                println!("  {n} {v}");
            }
            println!("functions:");
            for n in r.function_names() {
                let doc = r
                    .function(&n)
                    .map(|f| f.doc().to_string())
                    .unwrap_or_default();
                println!("  {n:<14} {doc}");
            }
        }
        ".checkpoint" if opened_read_only() => eprintln!("error: {READ_ONLY_REFUSAL}"),
        ".checkpoint" => match db.checkpoint() {
            Ok(()) => println!("checkpoint written"),
            Err(e) => eprintln!("could not write it: {e}"),
        },
        ".save" => match parts.next() {
            Some(p) => match save(db, p.trim()) {
                Ok(()) => println!("{} written", p.trim()),
                Err(e) => eprintln!("could not write it: {e}"),
            },
            None => eprintln!("usage: .save <path>"),
        },
        other => eprintln!("unknown command `{other}` (try .help)"),
    }
    true
}

/// `.save <path>`: a snapshot of the database written into `path` as a
/// database file is replaced -- under its lock, refused while a server or
/// an app has it open (this session's own file too, and the one a
/// read-only session opened), written beside it and renamed over, its sync
/// log gone first. Written in place, a reader that maps it (`--read-only`,
/// `fenec types`) read past the end of a file cut short under it, and a log
/// a crash left would be played over the new one.
fn save(db: &Database, path: &str) -> std::result::Result<(), String> {
    if READ_ONLY
        .get()
        .is_some_and(|own| std::fs::canonicalize(path).ok().as_ref() == Some(own))
    {
        return Err(format!("{READ_ONLY_REFUSAL}; save to another path"));
    }
    let _held = held::take(path, held::Way::Save)?;
    // Through a link, the file it leads to, as an open writes it: renamed
    // over, the link would be a file of its own.
    let path = fenec_core::fs::resolved(Path::new(path));
    let side = fenec_core::fs::beside(&path, "saving");
    let written = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&side)?;
        f.write_all(&db.snapshot())?;
        f.sync_all()?;
        fenec_core::fs::forget_sync_log(&path)?;
        std::fs::rename(&side, &path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&side);
    }
    written.map_err(|e| e.to_string())?;
    fenec_core::fs::sync_dir(&path).map_err(|e| e.to_string())
}

fn human(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / 1048576.0)
    }
}

fn run(db: &mut Database, src: &str) -> bool {
    // A list of numbers for a json field read as written.
    let stmts = match fenec_ql::parse_for(db, src) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return false;
        }
    };
    // Refused here, before any of the text runs: handed to the engine, the
    // file's sink refused the write as a storage error, after which every
    // write said "writes are refused after a storage error".
    if opened_read_only() && stmts.iter().any(|s| !s.is_read_only()) {
        eprintln!("error: {READ_ONLY_REFUSAL}");
        return false;
    }
    for s in &stmts {
        let t0 = std::time::Instant::now();
        match db.execute(s) {
            Err(e) => {
                eprintln!("error: {e}");
                return false;
            }
            Ok(resp) => print_response(&resp, t0.elapsed()),
        }
    }
    true
}

fn print_response(r: &Response, took: std::time::Duration) {
    match r {
        Response::Ok(m) => println!("{m}"),
        Response::Affected(n) => println!("{n} records ({:.2?})", took),
        Response::Schemas(schemas) => {
            for s in schemas {
                println!("{}", s.name);
                for f in &s.fields {
                    let ix = match &f.index {
                        IndexKind::None => String::new(),
                        IndexKind::Hash { unique: false } => "  @hash".into(),
                        IndexKind::Hash { unique: true } => "  @unique".into(),
                        IndexKind::Sorted { ttl: None } => "  @sorted".into(),
                        IndexKind::Sorted { ttl: Some(ms) } => {
                            format!("  @ttl({})", fenec_core::schema::ttl_text(*ms))
                        }
                        IndexKind::Vector(sp) => format!(
                            "  @hnsw({}, m={}, ef_search={}{})",
                            sp.metric.name(),
                            sp.m,
                            sp.ef_search,
                            sp.quant_arg()
                        ),
                        IndexKind::Text(sp) => {
                            format!("  @text({})", sp.args())
                        }
                        IndexKind::Inverted => "  @inverted".into(),
                        IndexKind::Geo => "  @geo".into(),
                    };
                    let mut ty = match f.collate {
                        Some(c) => format!("{} collate {}", f.ty.name(), c.name()),
                        None => f.ty.name().to_string(),
                    };
                    if f.nocase {
                        ty.push_str(" nocase");
                    }
                    println!("  {:<14} {:<14}{ix}", f.name, ty);
                }
                // The indexes over several fields, as `create collection`
                // declares them.
                for c in &s.composites {
                    println!("  {}", fenec_core::declared::composite_text(c));
                }
            }
        }
        Response::Rows(rs) => {
            // The terminal has no nested row, so `lookup` renders the way a
            // join would: parent columns, then the child's, one line per
            // pair.
            let rs = rs.flatten();
            let mut cols = rs.columns.clone();
            let has_score = rs.rows.iter().any(|r| r.score.is_some());
            if has_score {
                cols.push("_score".into());
            }
            let mut table: Vec<Vec<String>> = vec![cols.clone()];
            for row in &rs.rows {
                let mut cells: Vec<String> = row.values.iter().map(cell).collect();
                if has_score {
                    cells.push(row.score.map(|s| format!("{s:.4}")).unwrap_or_default());
                }
                table.push(cells);
            }
            // Wide cells are cut so the columns beside them stay in view. A
            // single column has none, and an `explain` line cut at 48
            // characters loses the numbers it exists for.
            let cap = if cols.len() == 1 { usize::MAX } else { 48 };
            let widths: Vec<usize> = (0..cols.len())
                .map(|i| {
                    table
                        .iter()
                        .map(|r| r.get(i).map(|s| s.chars().count()).unwrap_or(0))
                        .max()
                        .unwrap_or(0)
                        .min(cap)
                })
                .collect();
            for (ri, row) in table.iter().enumerate() {
                let line: Vec<String> = row
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let mut c = c.clone();
                        if c.chars().count() > widths[i] {
                            c = c.chars().take(widths[i] - 1).collect::<String>() + "…";
                        }
                        format!("{:<w$}", c, w = widths[i])
                    })
                    .collect();
                println!("{}", line.join("  ").trim_end());
                if ri == 0 {
                    println!(
                        "{}",
                        widths
                            .iter()
                            .map(|w| "-".repeat(*w))
                            .collect::<Vec<_>>()
                            .join("  ")
                    );
                }
            }
            println!("({} rows, {:.2?})", rs.rows.len(), took);
            // What `facet` counted, a line a field: over every matched row,
            // so after the page rather than in it.
            for f in &rs.facets {
                let values: Vec<String> = f
                    .values
                    .iter()
                    .map(|(v, n)| format!("{} {n}", cell(v)))
                    .collect();
                println!("facet {}: {}", f.field, values.join(", "));
            }
        }
    }
}

fn cell(v: &Value) -> String {
    match v {
        Value::Null => "-".into(),
        Value::Text(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Timestamp(ms) => fenec_core::time::format_iso(*ms),
        Value::Float(f) => format!("{f}"),
        Value::Bool(b) => (if *b { "true" } else { "false" }).into(),
        Value::Bytes(b) => format!("<{} bytes>", b.len()),
        Value::Vector(x) => {
            let head: Vec<String> = x.iter().take(3).map(|f| format!("{f:.3}")).collect();
            format!(
                "[{}{}]",
                head.join(", "),
                if x.len() > 3 {
                    format!(", … ×{}", x.len())
                } else {
                    String::new()
                }
            )
        }
        Value::List(items) => format!(
            "[{}]",
            items.iter().map(cell).collect::<Vec<_>>().join(", ")
        ),
        Value::Sparse(dim, e) => format!("<{} of {dim} non-zero>", e.len()),
        Value::Object(_) => fenec_core::json::to_string(v),
    }
}
