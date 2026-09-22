//! `fenec backup`, `fenec archive`, `fenec restore` -- a running server's
//! database taken whole, its writes kept, and a file rebuilt from both as it
//! stood at a moment. They speak the replication stream, so the server needs
//! `--replication-token`.

use fenec_http::archive::{self, Archive, Target};
use fenec_http::replication::Upstream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

pub const USAGE: &str = r#"
usage: fenec backup  <primary> <file.fenec | archive-dir>   [--token <t>]
       fenec archive <primary> <archive-dir>                [--token <t>]
       fenec restore <archive-dir> <out.fenec> [--to <time> | --to-change <n>]

  <primary> is its HTTP address, http://host:port, and <t> its
  --replication-token (also read from FENEC_REPLICATION_TOKEN).

  backup    the database as it stands, taken while the server runs: into a
            file that opens as a database of its own, or into an archive as
            a base image a restore can start from
  archive   keeps every write that reaches the primary's disk, with when it
            was made, until interrupted; starts with a base image
  restore   the database as it stood at <time> (2026-09-22T10:15:00Z) or
            after change <n>, or at the archive's end: the latest image at
            or before that point and the writes after it
"#;

static STOP: AtomicBool = AtomicBool::new(false);

/// SIGINT and SIGTERM end `fenec archive` between two messages, its segment
/// synced; libc's `signal` is declared here as `fenec-pg` does, to add no
/// dependency.
fn on_signals() {
    extern "C" {
        fn signal(sig: i32, handler: usize) -> usize;
    }
    extern "C" fn stop(_sig: i32) {
        STOP.store(true, Ordering::SeqCst);
    }
    unsafe {
        for sig in [2, 15] {
            signal(sig, stop as *const () as usize);
        }
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(2);
}

/// Runs `fenec backup|archive|restore ...`; the return value is the process
/// exit code.
pub fn main(command: &str, args: &[String]) -> i32 {
    let mut positional = Vec::new();
    let mut token = std::env::var("FENEC_REPLICATION_TOKEN").ok();
    let mut target = Target::End;
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
            "--token" => token = Some(next(&mut i, "--token")),
            "--to" => {
                let v = next(&mut i, "--to");
                match fenec_core::time::parse(&v) {
                    Ok(ms) if ms >= 0 => target = Target::Time(ms as u64),
                    _ => fail(&format!(
                        "--to expects a time like 2026-09-22T10:15:00Z, got `{v}`"
                    )),
                }
            }
            "--to-change" => {
                let v = next(&mut i, "--to-change");
                match v.parse() {
                    Ok(n) => target = Target::Change(n),
                    Err(_) => fail(&format!("--to-change expects a number, got `{v}`")),
                }
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            other if other.starts_with('-') => fail(&format!("unknown option: {other}\n{USAGE}")),
            other => positional.push(other.to_string()),
        }
        i += 1;
    }
    let [a, b] = positional.as_slice() else {
        fail(&format!("`fenec {command}` takes two arguments\n{USAGE}"));
    };
    let upstream = || {
        let token = token
            .clone()
            .unwrap_or_else(|| fail("the primary asks for its replication token: --token"));
        Upstream::new(a, token).unwrap_or_else(|e| fail(&e))
    };

    let result = match command {
        "backup" => archive::backup(&upstream(), Path::new(b)).map(|seq| {
            println!("{b}: the database at change {seq}");
        }),
        "archive" => {
            on_signals();
            let upstream = upstream();
            Archive::new(b).and_then(|arch| {
                eprintln!("archiving {} into {b}; interrupt to stop", upstream.url());
                arch.follow(&upstream, &STOP, &|line| eprintln!("{line}"))
            })
        }
        "restore" => Archive::new(a)
            .and_then(|arch| arch.restore(Path::new(b), target))
            .map(|r| {
                let when = r.time.map_or(String::new(), |t| {
                    format!(
                        ", the last written {}",
                        fenec_core::time::format_iso(t as i64)
                    )
                });
                println!(
                    "{b}: change {} -- image {} and the writes after it{when}",
                    r.seq, r.image
                );
            }),
        _ => unreachable!("dispatched by main"),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("fenec {command}: {e}");
            1
        }
    }
}
