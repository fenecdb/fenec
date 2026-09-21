//! `fenec` -- the fenecdb shell.
//!
//! ```text
//! fenec                      in-memory session
//! fenec data.fenec             open the file (create it when missing)
//! fenec data.fenec -c "get docs limit 5"
//! echo "get docs" | fenec data.fenec
//! ```

#[cfg(feature = "import")]
mod import;
mod types;

use fenec_core::prelude::*;
use fenec_ql::parse;
use std::io::{self, BufRead, IsTerminal, Write};

const HELP: &str = r#"
FenecQL summary
  create collection <name> ( <field> <type> [@hash|@hnsw(metric, m=.., ef_search=..)], ... )
  drop collection [if exists] <name>
  put <name> { field: value, ... }        -- or [ {...}, {...} ]
  get <name> [select a,b] [where <expr>] [near <field> <vector> [ef N] [exact]]
           [order <field> [asc|desc], ...] [limit N] [offset N] [count]
           [lookup <name> on <child> [= <parent>] [required] <clauses...>]
  select a, b from <name> ...             -- the classic SQL order works too
  set <name> { field: value } [where <expr>]
  del <name> [where <expr>]
  collections | describe <name> | compact [<name>]

Types     bool  int  float  text  bytes  vector<N>  [type]
Operators = != < <= > >=   ~ (text contains)   has (list contains)   in [..]
          and  or  not  is null  is not null
"#;

/// The import arm is only compiled with the `import` feature: the SQLite
/// reader and the PostgreSQL client add ~190 KB to the binary and are
/// unnecessary for embedded use.
#[cfg(feature = "import")]
const IMPORT_HELP: &str = r#"
Import
  fenec import <file.sqlite|postgres://...> --table <name> [--into <name>]
                                         load from SQLite or PostgreSQL
                                         for details: fenec import --help
"#;
#[cfg(not(feature = "import"))]
const IMPORT_HELP: &str = "";

const TYPES_HELP: &str = r#"
TypeScript types
  fenec types <file.fenec>                   generates `.d.ts` from the schema (stdout)
                                         for details: fenec types --help
"#;

const SHELL_HELP: &str = r#"
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
                println!("usage: fenec [file.fenec] [-c \"<query>\"]{HELP}{IMPORT_HELP}{TYPES_HELP}{SHELL_HELP}");
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
        let code = if run(&mut db, &cmd) { 0 } else { 1 };
        let _ = db.sync();
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

    let mut buffer = String::new();
    loop {
        if interactive {
            print!(
                "{}",
                if buffer.is_empty() {
                    "fenec> "
                } else {
                    "  ... "
                }
            );
            let _ = io::stdout().flush();
        }
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = line.trim();

        if buffer.is_empty() && trimmed.starts_with('.') {
            if !meta(&mut db, trimmed) {
                break;
            }
            continue;
        }
        if trimmed.is_empty() && buffer.trim().is_empty() {
            continue;
        }

        buffer.push_str(&line);
        // Keep reading until the parentheses/braces balance out.
        if !balanced(&buffer) {
            continue;
        }
        let stmt = std::mem::take(&mut buffer);
        if !stmt.trim().is_empty() {
            run(&mut db, &stmt);
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
    let _ = db.sync();
}

fn balanced(s: &str) -> bool {
    let (mut paren, mut brace, mut bracket) = (0i32, 0i32, 0i32);
    let mut in_str: Option<char> = None;
    let mut prev = '\0';
    for c in s.chars() {
        if let Some(q) = in_str {
            if c == q && prev != '\\' {
                in_str = None;
            }
            prev = c;
            continue;
        }
        match c {
            '"' | '\'' => in_str = Some(c),
            '(' => paren += 1,
            ')' => paren -= 1,
            '{' => brace += 1,
            '}' => brace -= 1,
            '[' => bracket += 1,
            ']' => bracket -= 1,
            _ => {}
        }
        prev = c;
    }
    in_str.is_none() && paren <= 0 && brace <= 0 && bracket <= 0
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
    let stmts = match parse(src) {
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
                        IndexKind::Hash => "  @hash".into(),
                        IndexKind::Vector(sp) => format!(
                            "  @hnsw({}, m={}, ef_search={})",
                            sp.metric.name(),
                            sp.m,
                            sp.ef_search
                        ),
                        IndexKind::Text(sp) => {
                            format!("  @text(k1={}, b={})", sp.k1(), sp.b())
                        }
                    };
                    println!("  {:<14} {:<14}{ix}", f.name, f.ty.name());
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
            let widths: Vec<usize> = (0..cols.len())
                .map(|i| {
                    table
                        .iter()
                        .map(|r| r.get(i).map(|s| s.chars().count()).unwrap_or(0))
                        .max()
                        .unwrap_or(0)
                        .min(48)
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
    }
}
