//! `fenec backup`, `fenec archive`, `fenec restore` -- a running server's
//! database taken whole, its writes kept, and a file rebuilt from both as it
//! stood at a moment. They speak the replication stream, so the server needs
//! `--replication-token`.

use crate::stop::{on_signals, STOP};
use fenec_http::archive::{self, Archive, Target};
use fenec_http::node_archive::{self, NodeArchive};
use fenec_http::replication::Upstream;
use fenec_http::seal::Key;
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
       fenec archive --tenants <node> <root-dir>            [the same]
       fenec restore <archive-dir | sealed-file> <out.fenec> [--to <time> | --to-change <n>]
       fenec restore --tenants <root-dir> <out-dir>         [--to <time>]
       fenec prune   [--tenants] <archive-dir | root-dir> --keep <7d>
       fenec verify  [--tenants] <archive-dir | root-dir>
       fenec key     <key-file>
  each but `key` takes --key-file <path> (or FENEC_KEY_FILE): the archive's
  files, or the backup file, sealed with it

  <primary> is its HTTP address, http://host:port, <node> a --dir node's,
  and <t> its --replication-token (also read from FENEC_REPLICATION_TOKEN;
  set and empty, it is unset). An empty --token, and an empty key file from
  --key-file or FENEC_KEY_FILE, are refused.

  backup    the database as it stands, taken while the server runs: into a
            file that opens as a database of its own, or into an archive as
            a base image a restore can start from
  archive   keeps every write that reaches the primary's disk, with when it
            was made, until interrupted; starts with a base image, and takes
            one of its own end every --image-every (1h unless given) without
            asking the primary, so a restore replays at most that much;
            with --keep, lets go of what no restore within it needs. A
            primary on a history that does not lead from the archive's
            end, at or before it -- restored to an earlier moment, or
            another database -- is refused: its writes would land among
            the archive's; archive it into a directory of its own
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

  --tenants every tenant of a --dir node (fenec-server --dir
            --replication-token), each an archive of its own under
            <root-dir>/<tenant>/, over one connection to the node however
            many tenants it holds: a tenant made there is archived from its
            first image; one deleted or moved to another node is no longer
            followed, its archive kept and noted gone, and noted back where
            it comes back. A tenant whose copy no longer leads from its
            archive -- restored to an earlier moment, or made again under
            its name -- has its line so far moved aside to
            <root-dir>/<tenant>.<history> and a new one begun.
            --image-every and --keep apply to each line. Each directory is
            an archive that restore, verify and prune read alone; with
            --tenants they read every one, the lines moved aside too.
            restore writes <out-dir>/<tenant>.fenec for each tenant the node
            held at that moment, from the line that began last by then -- a
            directory a node serves (fenec-server --dir) -- passes over a
            tenant gone then or archived only after, and fails on one whose
            moment --keep let go of

  key       writes a new key into <key-file>, 64 hexadecimal digits,
            readable by its owner alone. With it an archive's images,
            segments and history, and a backup, are encrypted and
            authenticated (ChaCha20-Poly1305): a byte changed, a file cut
            short or the wrong key is refused, never read. Keep the key
            apart from the archive: without it nothing can be restored

  A duration is a number and s, m, h or d: 90s, 30m, 6h, 7d.
"#;

/// Writes `text` into a new file only its owner can read.
fn write_private(path: &str, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()
}

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(2);
}

/// Runs `fenec backup|archive|restore ...`; the return value is the process
/// exit code.
pub fn main(command: &str, args: &[String]) -> i32 {
    let mut positional = Vec::new();
    // Set and empty is unset, as an environment file leaves a line it does
    // not use: the primary's ask for its token is then said here.
    let mut token = std::env::var("FENEC_REPLICATION_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    let mut target = Target::End;
    let mut every = Duration::from_secs(3600);
    let mut keep: Option<Duration> = None;
    let mut key_file = std::env::var("FENEC_KEY_FILE").ok();
    let mut tenants = false;
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
            "--tenants" => tenants = true,
            "--key-file" => key_file = Some(next(&mut i, "--key-file")),
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
    if command == "key" {
        let [path] = positional.as_slice() else {
            fail(&format!(
                "`fenec key` takes the file to write the key into\n{USAGE}"
            ));
        };
        if Path::new(path).exists() {
            fail(&format!(
                "{path} is there already: a key written over is an archive lost"
            ));
        }
        let Some(hex) = Key::generate() else {
            fail("no randomness from the system (/dev/urandom) to make a key with");
        };
        return match write_private(path, &format!("{hex}\n")) {
            Ok(()) => {
                println!("{path}: a new key; keep it apart from the archive");
                0
            }
            Err(e) => {
                eprintln!("fenec key: {path}: {e}");
                1
            }
        };
    }
    // `--token "$REPL"` with REPL unset: refused, not sent as a token.
    if token.as_deref() == Some("") {
        fail("--token is empty: give the primary's replication token, or leave the flag out");
    }
    // An empty key file is refused, from the environment too, never taken
    // for none: none writes the archive or the backup in the clear, and an
    // environment file's line left empty is a key meant.
    if key_file.as_deref() == Some("") {
        fail(
            "the key file is empty (--key-file or FENEC_KEY_FILE): name the key, or leave both \
             out for an archive or a backup in the clear",
        );
    }
    let key = key_file.as_ref().map(|p| {
        Key::from_file(Path::new(p)).unwrap_or_else(|e| fail(&format!("--key-file {p}: {e}")))
    });
    let archive = |dir: &str| Archive::with_key(dir, key.clone());
    if tenants {
        return every_tenant(
            command,
            &positional,
            key.as_ref(),
            (target, every, keep),
            token,
        );
    }
    if command == "verify" {
        let [dir] = positional.as_slice() else {
            fail(&format!(
                "`fenec verify` takes the archive's directory\n{USAGE}"
            ));
        };
        let iso = |t: u64| fenec_core::time::format_iso(t as i64);
        return match archive(dir).and_then(|a| a.verify()) {
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
        return match archive(dir).and_then(|a| a.prune(keep.as_millis() as u64, now)) {
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

    // The primary's address and token are checked first: a token missing
    // is said, exit 2, before anything is made beside the file.
    let backup_from = (command == "backup").then(upstream);

    // A database file a backup or a restore writes is renamed into its
    // place, its sync log removed first: held meanwhile, so a server or an
    // app with the file open -- whose writes would go on into the file
    // renamed away, and whose log would be gone -- refuses it and is
    // refused while it is written. An archive's directory is not one.
    let _held = match command {
        "backup" if !Path::new(b).is_dir() => Some(b),
        "restore" => Some(b),
        _ => None,
    }
    .map(|out| {
        crate::held::take(out, crate::held::Way::Copy).unwrap_or_else(|why| {
            eprintln!("fenec {command}: {why}");
            std::process::exit(1);
        })
    });
    let result = match command {
        "backup" => {
            let from = backup_from.unwrap_or_else(upstream);
            archive::backup(&from, Path::new(b), key.as_ref()).map(|seq| {
                println!("{b}: the database at change {seq}");
            })
        }
        "archive" => {
            on_signals();
            let upstream = upstream();
            archive(b).and_then(|arch| {
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
        // A sealed backup file: the database it holds.
        "restore" if Path::new(a).is_file() => {
            let key = key
                .as_ref()
                .unwrap_or_else(|| fail("a sealed backup opens with --key-file"));
            archive::unseal(Path::new(a), key, Path::new(b)).map(|seq| {
                println!("{b}: the database at change {seq}");
            })
        }
        "restore" => archive(a)
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

/// `fenec archive|verify|restore|prune --tenants`: every tenant of a node,
/// an archive each under a root.
fn every_tenant(
    command: &str,
    positional: &[String],
    key: Option<&Key>,
    (target, every, keep): (Target, Duration, Option<Duration>),
    token: Option<String>,
) -> i32 {
    match command {
        "archive" => {
            let [node, root] = positional else {
                fail(&format!(
                    "`fenec archive --tenants` takes the node and the root directory\n{USAGE}"
                ));
            };
            let token =
                token.unwrap_or_else(|| fail("the node asks for its replication token: --token"));
            let upstream = node_archive::node_upstream(node, token).unwrap_or_else(|e| fail(&e));
            on_signals();
            let result = NodeArchive::new(root, key.cloned()).and_then(|arch| {
                eprintln!(
                    "archiving every tenant of {} into {root} ({} archived so far); \
                     interrupt to stop",
                    upstream.url(),
                    arch.tenants().len()
                );
                // Each tenant's images and pruning on a thread of their own,
                // as for one database: the stream goes on meanwhile.
                std::thread::scope(|s| {
                    let beside =
                        s.spawn(|| arch.keep_up(every, keep, &STOP, &|line| eprintln!("{line}")));
                    let followed = arch.follow(&upstream, &STOP, &|line| eprintln!("{line}"));
                    STOP.store(true, std::sync::atomic::Ordering::SeqCst);
                    let kept = beside.join().unwrap_or(Ok(()));
                    followed.and(kept)
                })
            });
            match result {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("fenec archive: {e}");
                    1
                }
            }
        }
        "verify" | "prune" => {
            let [root] = positional else {
                fail(&format!(
                    "`fenec {command} --tenants` takes the root directory\n{USAGE}"
                ));
            };
            let keep = (command == "prune")
                .then(|| keep.unwrap_or_else(|| fail("`fenec prune` needs --keep <duration>")));
            // Every line, those moved aside too: each is an archive a restore
            // to a moment may take.
            let lines = node_archive::lines(Path::new(root))
                .unwrap_or_else(|e| fail(&format!("{root}: {e}")));
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            let iso = |t: u64| fenec_core::time::format_iso(t as i64);
            let mut failed = 0;
            for line in &lines {
                let (name, dir) = (&line.label, &line.dir);
                let arch = Archive::with_key(dir, key.cloned());
                let gone = node_archive::gone_at(dir).map_or(String::new(), |t| {
                    format!(", gone from the node {}", iso(t))
                });
                match keep {
                    None => match arch.and_then(|a| a.verify()) {
                        Ok(v) => println!(
                            "{name}: changes {} to {}, from {} to {}{}{gone}",
                            v.first,
                            v.last,
                            iso(v.from),
                            v.to.map_or("its last image".into(), iso),
                            match v.gaps.len() {
                                0 => String::new(),
                                n => format!(", {n} gaps"),
                            }
                        ),
                        Err(e) => {
                            eprintln!("{name}: {e}");
                            failed += 1;
                        }
                    },
                    Some(keep) => match arch.and_then(|a| a.prune(keep.as_millis() as u64, now)) {
                        Ok((images, segments)) => {
                            println!("{name}: let go of {images} images and {segments} segments")
                        }
                        Err(e) => {
                            eprintln!("{name}: {e}");
                            failed += 1;
                        }
                    },
                }
            }
            let tenants: std::collections::BTreeSet<&str> =
                lines.iter().map(|l| l.tenant.as_str()).collect();
            println!(
                "{root}: {} tenants in {} archives, {failed} failed",
                tenants.len(),
                lines.len()
            );
            (failed > 0) as i32
        }
        "restore" => {
            let [root, out] = positional else {
                fail(&format!(
                    "`fenec restore --tenants` takes the root directory and the directory \
                     to restore into\n{USAGE}"
                ));
            };
            let iso = |t: u64| fenec_core::time::format_iso(t as i64);
            let done = node_archive::restore_all(Path::new(root), Path::new(out), target, key)
                .unwrap_or_else(|e| fail(&format!("fenec restore --tenants: {e}")));
            let (mut restored, mut passed, mut failed) = (0, 0, 0);
            for (name, outcome) in &done {
                match outcome {
                    node_archive::Restoring::Restored(seq, from) if from == name => {
                        println!("{name}: change {seq}");
                        restored += 1;
                    }
                    node_archive::Restoring::Restored(seq, from) => {
                        println!("{name}: change {seq}, from {from}");
                        restored += 1;
                    }
                    node_archive::Restoring::Gone(t) => {
                        println!("{name}: gone from the node {}, passed over", iso(*t));
                        passed += 1;
                    }
                    node_archive::Restoring::NotYet => {
                        println!("{name}: archived only from after then, passed over");
                        passed += 1;
                    }
                    node_archive::Restoring::LetGo(first) => {
                        eprintln!(
                            "{name}: that moment was let go of (--keep): its archive holds \
                             nothing from before {}",
                            iso(*first)
                        );
                        failed += 1;
                    }
                    node_archive::Restoring::Failed(e) => {
                        eprintln!("{name}: {e}");
                        failed += 1;
                    }
                }
            }
            println!("{out}: {restored} tenants restored, {passed} passed over, {failed} failed");
            (failed > 0) as i32
        }
        _ => fail(&format!("`fenec {command}` takes no --tenants\n{USAGE}")),
    }
}
