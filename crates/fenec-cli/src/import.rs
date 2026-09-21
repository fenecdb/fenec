//! `fenec import` -- builds a collection from a SQLite file or a PostgreSQL
//! server.

use fenec_core::prelude::*;
use fenec_import::{load, map, pg, sqlite, IdSource, Options, Source};
use std::io::{IsTerminal, Write};

pub const USAGE: &str = r#"
usage: fenec import <source> --table <name> [options]

source
  file.sqlite                          a SQLite database file
  postgres://user[:password]@host[:port]/database

options
  --table <name>          source table (required)
  --into <name>           target collection (default: the --table value)
  --file <path>           target fenec file (default: <into>.fenec)
  --vector <field>:<N>    take a bytes or array field as vector<N>
  --index <field>@hnsw(metric, m=.., ef_construction=.., ef_search=..)
  --index <field>@hash    builds the index once the load finishes
  --cast <field>=<type>   force the type mapping (int, float, text, bytes,
                          bool, timestamp, vector<N>, [type])
  --id <column>|none      source of the document id (default: automatic)
  --where <expr>          FenecQL filter; applied on both sources before the
                          row is written. Field names are the target schema's.
  --source-where <SQL>    postgres: applies the filter on the server, so the
                          rows never stream. It is SQL, not FenecQL.
  --limit <N>             at most N rows
  --batch <N>             put batch size (default 2000)
  --sample <N>            sqlite: rows scanned for untyped columns (1000)
  --dry-run               print the plan, write nothing
  --count                 with --dry-run: also count how many rows will be
                          read. It means a full scan, hence optional.

example
  fenec import data.sqlite --table docs --into articles \
      --vector embed:384 --index "embed@hnsw(cosine)"
  fenec import data.sqlite --table docs --into articles \
      --where 'category = "book" and score >= 10'
"#;

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(2);
}

/// Runs `fenec import ...`; the return value is the process exit code.
pub fn main(args: &[String]) -> i32 {
    let mut source: Option<String> = None;
    let mut table: Option<String> = None;
    let mut into: Option<String> = None;
    let mut file: Option<String> = None;
    let mut source_where: Option<String> = None;
    let mut sample = sqlite::DEFAULT_SAMPLE;
    let mut dry_run = false;
    let mut count = false;
    let mut opts = Options::new("");

    let mut i = 0;
    let next = |i: &mut usize, flag: &str| -> String {
        *i += 1;
        match args.get(*i) {
            Some(v) => v.clone(),
            None => fail(&format!("{flag} expects a value")),
        }
    };
    while i < args.len() {
        match args[i].as_str() {
            "--table" | "-t" => table = Some(next(&mut i, "--table")),
            "--into" => into = Some(next(&mut i, "--into")),
            "--file" | "-f" => file = Some(next(&mut i, "--file")),
            "--where" => {
                let v = next(&mut i, "--where");
                match parse_where(&v) {
                    Ok(e) => opts.filter = Some(e),
                    Err(e) => fail(&e),
                }
            }
            "--source-where" => source_where = Some(next(&mut i, "--source-where")),
            "--vector" => {
                let v = next(&mut i, "--vector");
                match parse_vector(&v) {
                    Ok(p) => opts.vectors.push(p),
                    Err(e) => fail(&e),
                }
            }
            "--index" => {
                let v = next(&mut i, "--index");
                match parse_index(&v) {
                    Ok(p) => opts.indexes.push(p),
                    Err(e) => fail(&e),
                }
            }
            "--cast" => {
                let v = next(&mut i, "--cast");
                match parse_cast(&v) {
                    Ok(p) => opts.casts.push(p),
                    Err(e) => fail(&e),
                }
            }
            "--id" => {
                let v = next(&mut i, "--id");
                opts.id = if v == "none" {
                    IdSource::Generated
                } else {
                    IdSource::Column(v)
                };
            }
            "--limit" => opts.limit = Some(number(&next(&mut i, "--limit"), "--limit")),
            "--batch" => opts.batch = number(&next(&mut i, "--batch"), "--batch") as usize,
            "--sample" => sample = number(&next(&mut i, "--sample"), "--sample") as usize,
            "--dry-run" | "-n" => dry_run = true,
            "--count" => count = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            other if !other.starts_with('-') => {
                if source.is_some() {
                    fail(&format!("extra argument: {other}"));
                }
                source = Some(other.to_string());
            }
            other => fail(&format!("unknown option: {other}")),
        }
        i += 1;
    }
    if opts.batch == 0 {
        fail("--batch cannot be zero");
    }
    if count && !dry_run {
        fail("--count only makes sense with --dry-run; a real import already reports the count");
    }

    let Some(source) = source else {
        eprintln!("no source was given.{USAGE}");
        return 2;
    };
    let Some(table) = table else {
        fail("--table is required");
    };
    opts.into = into.unwrap_or_else(|| table.clone());
    let target = file.unwrap_or_else(|| format!("{}.fenec", opts.into));

    match run(
        &source,
        &table,
        &target,
        &source_where,
        sample,
        dry_run,
        count,
        &opts,
    ) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn run(
    source: &str,
    table: &str,
    target: &str,
    source_where: &Option<String>,
    sample: usize,
    dry_run: bool,
    count: bool,
    opts: &Options,
) -> Result<()> {
    let is_pg = source.starts_with("postgres://") || source.starts_with("postgresql://");
    // Counting scans from the start; it happens before the reader is opened
    // so it does not share the connection with the COPY stream on PostgreSQL.
    let mut total: Option<u64> = None;
    let mut src: Box<dyn Source> = if is_pg {
        let url = pg::Url::parse(source)?;
        let query = pg::Query {
            table: table.to_string(),
            filter: source_where.clone(),
            // LIMIT can only be pushed to the source when there is no local
            // filter: cutting on the server first and filtering locally
            // afterwards would yield fewer rows than asked for.
            limit: if opts.filter.is_none() {
                opts.limit
            } else {
                None
            },
        };
        println!(
            "source   {}@{}:{}/{} -> {table}",
            url.user, url.host, url.port, url.database
        );
        if count {
            total = Some(pg::count_rows(&url, &query)?);
        }
        Box::new(pg::Reader::open(&url, &query)?)
    } else {
        if source_where.is_some() {
            return Err(Error::Query(
                "--source-where is only valid for postgres sources; \
                 use --where for SQLite"
                    .into(),
            ));
        }
        println!("source   {source} -> {table}");
        if count {
            total = Some(sqlite::count_rows(source, table)?);
        }
        Box::new(sqlite::Reader::open_sampled(source, table, sample)?)
    };

    let columns = src.columns()?;
    let plan = map::plan(&columns, opts)?;

    if dry_run {
        print_plan(&plan, &columns, target, total.or_else(|| src.row_count()));
        return Ok(());
    }

    println!("target   {target} -> {}", opts.into);
    for w in &plan.warnings {
        println!("warning  {w}");
    }

    // An import that stops halfway leaves the collection in the file; since
    // fenecdb has no transactions it cannot be rolled back. Deleting the file
    // when we created it keeps a retry clean; a file the user already had is
    // left untouched.
    let existed = std::path::Path::new(target).exists();
    let interactive = std::io::stdout().is_terminal();
    let t0 = std::time::Instant::now();
    let summary = {
        let mut db = fenec_core::fs::open(target)?;
        let mut tick = |n: u64| {
            if interactive {
                print!("\r{n} records...");
                let _ = std::io::stdout().flush();
            }
        };
        match load::run_with_progress(&mut *src, &mut db, opts, &mut tick) {
            Ok(s) => s,
            Err(e) => {
                drop(db);
                if interactive {
                    println!();
                }
                if existed {
                    eprintln!(
                        "note: the `{}` collection in `{target}` may be half written",
                        opts.into
                    );
                } else {
                    let _ = std::fs::remove_file(target);
                }
                return Err(e);
            }
        }
    };
    if interactive {
        print!("\r");
    }
    println!(
        "{} records, {} ({:.2?})",
        summary.rows,
        target,
        t0.elapsed()
    );
    if summary.skipped > 0 {
        println!("skipped  {} rows (--where)", summary.skipped);
    }
    for (field, kind) in &plan.indexes {
        println!("index    {field} {}", index_name(kind));
    }
    Ok(())
}

fn print_plan(plan: &map::Plan, columns: &[fenec_import::Column], target: &str, rows: Option<u64>) {
    println!("target   {target} -> {}\n", plan.schema.name);
    println!("create collection {} (", plan.schema.name);
    let width = plan
        .schema
        .fields
        .iter()
        .map(|f| f.name.len())
        .max()
        .unwrap_or(0);
    for f in &plan.schema.fields {
        // Show which source column the field comes from.
        let src = plan
            .targets
            .iter()
            .zip(columns)
            .find(|(t, _)| matches!(t, map::Target::Field(n) if *n == f.name))
            .map(|(_, c)| c.source_type.as_str())
            .unwrap_or("");
        println!(
            "  {:<width$}  {:<14} <- {src}",
            f.name,
            f.ty.name(),
            width = width
        );
    }
    println!(")");
    if let Some((_, c)) = plan
        .targets
        .iter()
        .zip(columns)
        .find(|(t, _)| **t == map::Target::Id)
    {
        println!("  id  <- {} ({})", c.name, c.source_type);
    }
    for (field, kind) in &plan.indexes {
        println!(
            "create index on {} ({field}) {}",
            plan.schema.name,
            index_name(kind)
        );
    }
    println!();
    for w in &plan.warnings {
        println!("warning  {w}");
    }
    match rows {
        Some(n) => println!("{n} rows will be read; nothing was written (--dry-run)"),
        None => println!("nothing was written (--dry-run); use --count for the row count"),
    }
}

fn index_name(kind: &IndexKind) -> String {
    match kind {
        IndexKind::Hash => "@hash".into(),
        IndexKind::Vector(s) => format!(
            "@hnsw({}, m={}, ef_construction={}, ef_search={})",
            s.metric.name(),
            s.m,
            s.ef_construction,
            s.ef_search
        ),
        IndexKind::Text(s) => format!("@text(k1={}, b={})", s.k1(), s.b()),
        IndexKind::None => String::new(),
    }
}

// ------------------------------------------------------------ flag parsing

fn number(s: &str, flag: &str) -> u64 {
    match s.parse() {
        Ok(n) => n,
        Err(_) => fail(&format!("{flag} expects a number, got `{s}`")),
    }
}

/// `field:N`
fn parse_vector(s: &str) -> std::result::Result<(String, usize), String> {
    let (name, dim) = s
        .rsplit_once(':')
        .ok_or_else(|| format!("--vector expects `field:N`, got `{s}`"))?;
    let dim: usize = dim
        .trim()
        .parse()
        .map_err(|_| format!("the --vector dimension must be a number, got `{dim}`"))?;
    if name.is_empty() || dim == 0 {
        return Err(format!("invalid --vector: `{s}`"));
    }
    Ok((name.to_string(), dim))
}

/// Turns the `--where` contents into a FenecQL expression.
///
/// There is no separate expression parser; the expression is wrapped in a
/// `get` body and handed to the real parser. The wrapper's other clauses
/// have to stay empty, otherwise extra clauses could be smuggled in via `--where`.
fn parse_where(s: &str) -> std::result::Result<Expr, String> {
    let stmt = fenec_ql::parse_one(&format!("get t where {s}"))
        .map_err(|e| format!("--where could not be parsed: {e}"))?;
    let Statement::Select(sel) = stmt else {
        return Err("--where must be an expression".into());
    };
    if sel.project.is_some()
        || sel.near.is_some()
        || !sel.order.is_empty()
        || sel.limit.is_some()
        || sel.offset != 0
        || sel.count
    {
        return Err("--where only takes a condition expression".into());
    }
    sel.filter.ok_or_else(|| "--where is empty".to_string())
}

/// `field=type`
fn parse_cast(s: &str) -> std::result::Result<(String, DataType), String> {
    let (name, ty) = s
        .split_once('=')
        .ok_or_else(|| format!("--cast expects `field=type`, got `{s}`"))?;
    let ty = map::parse_type(ty).ok_or_else(|| format!("unknown type: `{ty}`"))?;
    if name.is_empty() {
        return Err(format!("invalid --cast: `{s}`"));
    }
    Ok((name.to_string(), ty))
}

/// `field@hash` or `field@hnsw[(metric, m=.., ef_construction=.., ef_search=..)]`
fn parse_index(s: &str) -> std::result::Result<(String, IndexKind), String> {
    let (name, spec) = s
        .split_once('@')
        .ok_or_else(|| format!("--index expects `field@hash` or `field@hnsw(...)`, got `{s}`"))?;
    if name.is_empty() {
        return Err(format!("invalid --index: `{s}`"));
    }
    let spec = spec.trim();
    if spec.eq_ignore_ascii_case("hash") {
        return Ok((name.to_string(), IndexKind::Hash));
    }
    let args = match spec.strip_prefix("hnsw").map(str::trim) {
        None => return Err(format!("unknown index kind: `{spec}`")),
        Some("") => "",
        Some(rest) => rest
            .strip_prefix('(')
            .and_then(|r| r.strip_suffix(')'))
            .ok_or_else(|| format!("the --index parenthesis does not close: `{s}`"))?,
    };
    let mut v = VectorIndexSpec::default();
    for (n, arg) in args.split(',').map(str::trim).enumerate() {
        if arg.is_empty() {
            continue;
        }
        match arg.split_once('=') {
            None => {
                // A single positional argument is the metric.
                if n != 0 {
                    return Err(format!("--index unexpected argument: `{arg}`"));
                }
                v.metric = Metric::parse(arg)
                    .ok_or_else(|| format!("unknown metric: `{arg}` (cosine, l2, dot)"))?;
            }
            Some((k, val)) => {
                let num: usize = val
                    .trim()
                    .parse()
                    .map_err(|_| format!("--index `{k}` expects a number, got `{val}`"))?;
                match k.trim() {
                    "m" => v.m = num,
                    "ef_construction" => v.ef_construction = num,
                    "ef_search" => v.ef_search = num,
                    other => return Err(format!("--index unknown parameter: `{other}`")),
                }
            }
        }
    }
    Ok((name.to_string(), IndexKind::Vector(v)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fenec_core::value::VecPrec;

    #[test]
    fn vector_flag_parses() {
        assert_eq!(parse_vector("embed:384").unwrap(), ("embed".into(), 384));
        assert!(parse_vector("embed").is_err());
        assert!(parse_vector("embed:0").is_err());
        assert!(parse_vector("embed:x").is_err());
    }

    #[test]
    fn cast_flag_parses() {
        assert_eq!(parse_cast("a=int").unwrap(), ("a".into(), DataType::Int));
        assert_eq!(
            parse_cast("e=vector<8, f16>").unwrap(),
            ("e".into(), DataType::Vector(8, VecPrec::F16))
        );
        assert!(parse_cast("a=decimal").is_err());
        assert!(parse_cast("a").is_err());
    }

    #[test]
    fn index_flag_parses() {
        assert_eq!(parse_index("k@hash").unwrap().1, IndexKind::Hash);

        let (name, IndexKind::Vector(v)) = parse_index("embed@hnsw").unwrap() else {
            panic!()
        };
        assert_eq!(name, "embed");
        assert_eq!(v, VectorIndexSpec::default());

        let (_, IndexKind::Vector(v)) =
            parse_index("embed@hnsw(l2, m=32, ef_construction=400, ef_search=64)").unwrap()
        else {
            panic!()
        };
        assert_eq!(v.metric, Metric::L2);
        assert_eq!(v.m, 32);
        assert_eq!(v.ef_construction, 400);
        assert_eq!(v.ef_search, 64);
    }

    #[test]
    fn where_flag_parses_fenecql() {
        assert!(parse_where(r#"category = "book" and score >= 10"#).is_ok());
        assert!(parse_where("id > 100").is_ok());
        assert!(parse_where(r#"tags has "rust""#).is_ok());
        assert!(parse_where("title is not null").is_ok());
    }

    /// No extra clause may be smuggled into the wrapper; otherwise `--where`
    /// would silently change the rest of the query too.
    #[test]
    fn where_flag_refuses_extra_clauses() {
        assert!(parse_where("score > 1 limit 5").is_err());
        assert!(parse_where("score > 1 order score desc").is_err());
        assert!(parse_where("score > 1 near embed [1,2]").is_err());
        assert!(parse_where("").is_err());
        assert!(parse_where("this is not an expression )(").is_err());
    }

    #[test]
    fn bad_index_flags_are_refused() {
        assert!(parse_index("embed").is_err(), "@ is missing");
        assert!(parse_index("embed@btree").is_err());
        assert!(parse_index("embed@hnsw(distance)").is_err());
        assert!(parse_index("embed@hnsw(cosine, k=3)").is_err());
        assert!(parse_index("embed@hnsw(cosine").is_err());
        assert!(parse_index("@hash").is_err());
    }
}
