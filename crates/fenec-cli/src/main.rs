//! `fenec` -- the fenecdb shell.
//!
//! ```text
//! fenec                      in-memory session
//! fenec data.fenec             open the file (create it when missing)
//! fenec data.fenec -c "get docs limit 5"
//! echo "get docs" | fenec data.fenec
//! fenec data.fenec < script.fenecql
//! ```
//!
//! A statement may run over several lines; [`Reader`] says when one runs.

#[cfg(feature = "backup")]
mod backup;
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
  once the next one begins; -c reads its text whole.

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
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-c" | "--command" => {
                i += 1;
                command = args.get(i).cloned();
            }
            "-h" | "--help" => {
                println!("usage: fenec [file.fenec] [-c \"<query>\"]{HELP}{IMPORT_HELP}{BACKUP_HELP}{MCP_HELP}{TYPES_HELP}{SHELL_HELP}");
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

    let mut db = match &path {
        Some(p) => match fenec_core::fs::open(p) {
            Ok(db) => db,
            Err(e) => {
                eprintln!("could not open {p}: {e}");
                std::process::exit(1);
            }
        },
        None => Database::new(),
    };

    if let Some(cmd) = command {
        let mut code = if run(&mut db, &cmd) { 0 } else { 1 };
        // A one-shot write that never reached the disk must not exit 0: the
        // caller's script would go on believing it.
        if let Err(e) = db.sync() {
            eprintln!("could not push the writes to disk: {e}");
            code = 1;
        }
        std::process::exit(code);
    }

    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    if interactive {
        println!(
            "fenecdb {} — {}",
            fenec_core::VERSION,
            path.as_deref().unwrap_or("in-memory")
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
    if path.is_some() && has_vectors {
        match db.checkpoint() {
            Ok(()) => {
                if interactive {
                    println!("checkpoint written (the HNSW graph is persisted)");
                }
            }
            Err(e) => eprintln!("could not write the checkpoint: {e}"),
        }
    }
    if let Err(e) = db.sync() {
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
/// ([`pending`]). Rejected: running at `;` alone, as `psql` and `sqlite3`
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
    /// A statement ran as this line ended, typed; and as the line before
    /// it did, which a line that begins no statement may have gone on
    /// with.
    ran: bool,
    ran_before: bool,
    /// Why a statement waits has been said, once a session.
    told: bool,
}

/// What [`Reader::buf`] holds, once its strings and brackets are closed.
enum Held {
    /// Nothing whole and nothing refused: empty, or a string or a bracket
    /// open.
    Nothing,
    /// A statement read whole, waiting for the next line.
    Whole,
    /// A statement refused as far as the text goes: whether that stands is
    /// for the next line to say (`Refusal::stands`).
    Refused(fenec_ql::Refusal),
}

impl Reader {
    fn new(typed: bool) -> Reader {
        Reader {
            typed,
            buf: String::new(),
            gate: Default::default(),
            held: Held::Nothing,
            more: None,
            ran: false,
            ran_before: false,
            told: false,
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
        let t = line.trim();
        // A `.` command where no string or bracket is open; inside one it
        // is text. Typed, it leaves what waits waiting, as `psql`'s do --
        // `del notes`, then `.tables` to look a name up, then its `where`
        // -- and `.quit` leaves it unrun. In a script or a paste what
        // waits was written before it, and runs first.
        if t.starts_with('.') && !self.gate.is_open() {
            // One call: from two, `meta` went out of line, 2.2 KB more.
            let typed = self.typed && !self.more();
            if !typed {
                self.finish(db);
            }
            let go_on = meta(db, t);
            if !go_on {
                self.abandon();
            }
            return go_on;
        }
        if t.is_empty() {
            // An empty line typed runs what waits, or says why it is
            // refused; pasted or piped it is white space.
            if !self.gate.is_open() && !matches!(self.held, Held::Nothing) && !self.more() {
                self.finish(db);
            } else if !self.buf.is_empty() {
                self.buf.push_str(line);
                self.gate.feed(line);
            }
            return true;
        }
        let from = self.buf.len();
        self.buf.push_str(line);
        self.gate.feed(line);
        if !self.gate.is_open() {
            self.read(db, from);
        }
        true
    }

    /// `buf` read again, its last line from `from`.
    fn read(&mut self, db: &mut Database, mut from: usize) {
        loop {
            let r = fenec_ql::reading(&self.buf);
            // A refusal held from the lines before stands where this line
            // leaves it as it was: it is told, and the line read alone.
            if let (Held::Refused(before), Rest::Refused(now)) = (&self.held, &r.rest) {
                if from > 0 && before.stands(now) {
                    let Held::Refused(before) = std::mem::replace(&mut self.held, Held::Nothing)
                    else {
                        unreachable!()
                    };
                    self.refused(&before, &self.buf[..from]);
                    self.keep(from);
                    if self.gate.is_open() {
                        return;
                    }
                    from = 0;
                    continue;
                }
            }
            return self.take(db, r);
        }
    }

    /// What a reading of `buf` found: the whole statements run, and what is
    /// left kept, run, or told as refused.
    fn take(&mut self, db: &mut Database, r: Reading) {
        for &(a, b) in &r.whole {
            run(db, &self.buf[a..b]);
        }
        match r.rest {
            Rest::Nothing => self.clear(),
            Rest::Last(at, s) => {
                self.keep(at);
                if self.runs_now(&s) {
                    run(db, &self.buf);
                    self.clear();
                    self.ran = true;
                    return;
                }
                self.held = Held::Whole;
                if self.typed && !self.told && !self.more() {
                    self.told = true;
                    println!(
                        "-- {} waits for the next line, which may go on with it: an empty line \
                         runs it, and so does a `;` after it",
                        if writes(&s) {
                            "a write"
                        } else {
                            "a statement over several lines"
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
                    ..f
                };
                if !self.more() && f.stands_alone(&self.buf) {
                    self.refused(&f, &self.buf);
                    self.clear();
                } else {
                    self.held = Held::Refused(f);
                }
            }
        }
    }

    /// Whether a statement read whole runs as its line ends: typed, with
    /// nothing more waiting, and neither a write nor a read over several
    /// lines.
    fn runs_now(&mut self, s: &Statement) -> bool {
        self.typed
            && !writes(s)
            && !(s.is_read_only() && self.buf.trim_end().contains('\n'))
            && !self.more()
    }

    /// The end of the input, a `.` command or an empty line typed: what
    /// `buf` holds is all there is.
    fn finish(&mut self, db: &mut Database) {
        if !self.buf.trim().is_empty() {
            let r = fenec_ql::reading(&self.buf);
            for &(a, b) in &r.whole {
                run(db, &self.buf[a..b]);
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
    }

    /// Typed, the end of the session: what waits is not run, and said so.
    fn abandon(&mut self) {
        if !self.buf.trim().is_empty() {
            eprintln!("not run: {}", one_line(&self.buf));
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

/// A write: what waits for the next line when typed.
fn writes(s: &Statement) -> bool {
    matches!(
        s,
        Statement::Put { .. } | Statement::Update { .. } | Statement::Delete { .. }
    )
}

/// Whether a line is waiting to be read already -- a paste, or a line typed
/// ahead while a statement ran: the shell then reads on before it judges a
/// statement whole, as it does a file. A paste comes in one write, or in
/// pieces close together over a slow link: 10 ms is long enough for those,
/// and no one types a line in it. A terminal hands a line at a time to a
/// read, so stdin's own buffer holds nothing past it.
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
    unsafe { poll(&mut fd, 1, 10) > 0 }
}

/// No `poll` declared here: typed lines are judged as they end.
#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn pending() -> bool {
    false
}

fn meta(db: &mut Database, cmd: &str) -> bool {
    let mut parts = cmd.splitn(2, char::is_whitespace);
    match parts.next().unwrap_or("") {
        ".quit" | ".exit" | ".q" => return false,
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
        ".checkpoint" => match db.checkpoint() {
            Ok(()) => println!("checkpoint written"),
            Err(e) => eprintln!("could not write it: {e}"),
        },
        ".save" => match parts.next() {
            Some(p) => {
                let p = p.trim();
                match std::fs::write(p, db.snapshot()) {
                    Ok(()) => println!("{p} written"),
                    Err(e) => eprintln!("could not write it: {e}"),
                }
            }
            None => eprintln!("usage: .save <path>"),
        },
        other => eprintln!("unknown command `{other}` (try .help)"),
    }
    true
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
