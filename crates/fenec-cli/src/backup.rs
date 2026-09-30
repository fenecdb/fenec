//! `fenec backup`, `fenec archive`, `fenec restore` -- a running server's
//! database taken whole, its writes kept, and a file rebuilt from both as it
//! stood at a moment. They speak the replication stream, so the server needs
//! `--replication-token`.

use crate::stop::{on_signals, STOP};
use fenec_http::archive::{self, Archive, Target};
use fenec_http::replication::Upstream;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `90s`, `30m`, `6h`, `7d`.
fn duration(v: &str) -> Option<Duration> {
    let (n, unit) = v.split_at(v.len().checked_sub(1)?);
    let n: u64 = n.parse().ok().filter(|&n| n > 0)?;
    let secs = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return None,
    };
    Some(Duration::from_secs(n.checked_mul(secs)?))
}

pub const USAGE: &str = r#"
usage: fenec backup  <primary> <file.fenec | archive-dir>   [--token <t>]
       fenec archive <primary> <archive-dir>                [--token <t>]
                     [--image-every <1h>] [--keep <7d>]
       fenec restore <archive-dir> <out.fenec> [--to <time> | --to-change <n>]
       fenec prune   <archive-dir> --keep <7d>
       fenec verify  <archive-dir>

  <primary> is its HTTP address, http://host:port, and <t> its
  --replication-token (also read from FENEC_REPLICATION_TOKEN).

  backup    the database as it stands, taken while the server runs: into a
            file that opens as a database of its own, or into an archive as
            a base image a restore can start from
  archive   keeps every write that reaches the primary's disk, with when it
            was made, until interrupted; starts with a base image, and takes
            one of its own end every --image-every (1h unless given) without
            asking the primary, so a restore replays at most that much;
            with --keep, lets go of what no restore within it needs
  restore   the database as it stood at <time> (2026-09-22T10:15:00Z) or
            after change <n>, or at the archive's end: the latest image at
            or before that point and the writes after it
  prune     what `archive --keep` lets go of, done once: the images before the
            newest one taken by the start of the window and the segments
            before the oldest image kept -- as on a copy synced elsewhere

  verify    reads the archive as a restore would: every image opens at its
            change, the segments run on with none missing, a restore to the
            end opens; says from when to when it can restore, and exits 1
            when it cannot

  A duration is a number and s, m, h or d: 90s, 30m, 6h, 7d.
"#;

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
    let mut every = Duration::from_secs(3600);
    let mut keep: Option<Duration> = None;
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
            "--image-every" => {
                let v = next(&mut i, "--image-every");
                every = duration(&v).unwrap_or_else(|| {
                    fail(&format!(
                        "--image-every expects a duration like 1h, got `{v}`"
                    ))
                });
            }
            "--keep" => {
                let v = next(&mut i, "--keep");
                keep = Some(duration(&v).unwrap_or_else(|| {
                    fail(&format!("--keep expects a duration like 7d, got `{v}`"))
                }));
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
    if command == "verify" {
        let [dir] = positional.as_slice() else {
            fail(&format!(
                "`fenec verify` takes the archive's directory\n{USAGE}"
            ));
        };
        let iso = |t: u64| fenec_core::time::format_iso(t as i64);
        return match Archive::new(dir).and_then(|a| a.verify()) {
            Ok(v) => {
                println!("{dir}: {} images, {} segments", v.images, v.segments);
                println!(
                    "  restores changes {} to {}, from {} to {}",
                    v.first,
                    v.last,
                    iso(v.from),
                    v.to.map_or("its last image".into(), iso)
                );
                for (a, b) in &v.gaps {
                    println!(
                        "  changes {a} to {b} are in no segment: no restore to a moment among them"
                    );
                }
                if v.torn > 0 {
                    println!("  the last segment ends in a record cut short ({} bytes), which a restore passes over", v.torn);
                }
                0
            }
            Err(e) => {
                eprintln!("fenec verify: {dir}: {e}");
                1
            }
        };
    }
    if command == "prune" {
        let [dir] = positional.as_slice() else {
            fail(&format!(
                "`fenec prune` takes the archive's directory\n{USAGE}"
            ));
        };
        let keep = keep.unwrap_or_else(|| fail("`fenec prune` needs --keep <duration>"));
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        return match Archive::new(dir).and_then(|a| a.prune(keep.as_millis() as u64, now)) {
            Ok((images, segments)) => {
                println!("{dir}: let go of {images} images and {segments} segments");
                0
            }
            Err(e) => {
                eprintln!("fenec prune: {e}");
                1
            }
        };
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
                // Its own images and the pruning beside the stream, on a
                // thread of their own: an image of a large database takes
                // a while, and the stream goes on meanwhile.
                std::thread::scope(|s| {
                    let beside =
                        s.spawn(|| arch.keep_up(every, keep, &STOP, &|line| eprintln!("{line}")));
                    let followed = arch.follow(&upstream, &STOP, &|line| eprintln!("{line}"));
                    STOP.store(true, std::sync::atomic::Ordering::SeqCst);
                    let kept = beside.join().unwrap_or(Ok(()));
                    followed.and(kept)
                })
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
