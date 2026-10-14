//! The restore drill: a node's tenants archived by `fenec archive --tenants`
//! while they are written, made, deleted and closed, the node's directory
//! deleted, and every tenant brought back by `fenec restore --tenants` --
//! checked by `fenec verify --tenants` and by its rows, each whole up to its
//! last archived write.

use fenec_core::prelude::*;
use fenec_http::archive::{Archive, Target};
use fenec_http::tenants::{Replicated, Tenant, Tenants};
use fenec_http::{Config, Server};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const REPL: &str = "repl";

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fenec-drill-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn exec(t: &Tenant, sql: &str) {
    let durable = {
        let mut g = t.db.write().unwrap();
        g.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
        g.flush().unwrap()
    };
    if let Some(d) = durable {
        d().unwrap();
    }
}

fn count(db: &Database) -> i64 {
    let r = db
        .query(&fenec_ql::parse_one("get notes count").unwrap(), &[])
        .unwrap();
    match r.rows().unwrap().rows[0].values[0] {
        Value::Int(n) => n,
        ref v => panic!("a count of {v:?}"),
    }
}

fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !cond() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn fenec(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_fenec"))
        .args(args)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

#[test]
fn a_node_restored_from_its_archive_holds_every_archived_write() {
    let d = scratch("drill");
    let node_dir = d.join("node");
    let tenants = Arc::new(
        Tenants::new(&node_dir)
            .unwrap()
            .with_replication(Replicated {
                token: REPL.into(),
                buffer: 8 << 20,
                upstream: None,
                sync_on_write: true,
            }),
    );
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        sync_on_write: true,
        ..Config::default()
    };
    let server = Server::with_tenants(Arc::clone(&tenants), cfg);
    let listener = server.bind().unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    let mut names: Vec<String> = (0..6).map(|i| format!("t{i}")).collect();
    names.push("doomed".into());
    for name in &names {
        exec(
            &tenants.create(name).unwrap(),
            "create collection notes (title text, n int @hash)",
        );
    }

    // The archiver, a process of its own, taking each tenant's images every
    // second beside the stream.
    let root = d.join("root");
    let log = std::fs::File::create(d.join("archiver.log")).unwrap();
    let mut archiver = Command::new(env!("CARGO_BIN_EXE_fenec"))
        .args(["archive", "--tenants", &url, root.to_str().unwrap()])
        .args(["--token", REPL, "--image-every", "1s"])
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .unwrap();
    // Everything after this is written while the archiver follows: each
    // tenant has its first image.
    for name in &names {
        let probe = d.join(format!("probe-{name}.fenec"));
        until(&format!("`{name}` was never archived"), || {
            Archive::new(root.join(name))
                .and_then(|a| a.restore(&probe, Target::End))
                .is_ok()
        });
    }

    // Four writers over the tenants, and a closer closing every one that is
    // idle for a moment; a tenant made half way, and one deleted.
    let stop = Arc::new(AtomicBool::new(false));
    let closer = {
        let (tenants, stop) = (Arc::clone(&tenants), Arc::clone(&stop));
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                tenants.close_idle(Duration::ZERO);
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    let writers: Vec<_> = (0..4)
        .map(|w| {
            let tenants = Arc::clone(&tenants);
            std::thread::spawn(move || {
                for i in 0..150usize {
                    let name = format!("t{}", (w * 7 + i * 3) % 6);
                    let t = tenants.get(&name).unwrap();
                    exec(&t, &format!("put notes {{title: \"w{w} {i}\", n: {i}}}"));
                }
            })
        })
        .collect();
    let late = tenants.create("late").unwrap();
    exec(&late, "create collection notes (title text, n int @hash)");
    for i in 0..20 {
        exec(&late, &format!("put notes {{title: \"late {i}\", n: {i}}}"));
    }
    drop(late);
    exec(
        &tenants.get("doomed").unwrap(),
        "put notes {title: \"gone soon\", n: 1}",
    );
    until("`doomed` was never let go of", || {
        tenants.delete("doomed").is_ok()
    });
    for w in writers {
        w.join().unwrap();
    }
    stop.store(true, Ordering::SeqCst);
    closer.join().unwrap();

    // Where the node stands: each tenant's last change and its rows.
    names.retain(|n| n != "doomed");
    names.push("late".into());
    let stood: Vec<(String, u64, i64)> = names
        .iter()
        .map(|name| {
            let t = tenants.get(name).unwrap();
            let g = t.db.read().unwrap();
            (name.clone(), g.change_seq(), count(&g))
        })
        .collect();
    for (name, seq, _) in &stood {
        let probe = d.join(format!("probe-{name}.fenec"));
        until(&format!("`{name}` was not archived to {seq}"), || {
            Archive::new(root.join(name))
                .and_then(|a| a.restore(&probe, Target::End))
                .is_ok_and(|r| r.seq >= *seq)
        });
    }
    until("the archive was not told `doomed` is gone", || {
        fenec_http::node_archive::gone_at(&root.join("doomed")).is_some()
    });
    // Interrupted, the archiver syncs and ends.
    let term = Command::new("kill")
        .args(["-TERM", &archiver.id().to_string()])
        .status()
        .unwrap();
    assert!(term.success());
    let ended = archiver.wait().unwrap();
    assert!(
        ended.success(),
        "{ended}: {}",
        std::fs::read_to_string(d.join("archiver.log")).unwrap_or_default()
    );

    // The node's data, gone.
    until("the tenants did not close", || {
        tenants.close_idle(Duration::ZERO);
        tenants.stats().open.is_empty()
    });
    std::fs::remove_dir_all(&node_dir).unwrap();

    let (ok, said) = fenec(&["verify", "--tenants", root.to_str().unwrap()]);
    assert!(ok, "{said}");
    assert!(said.contains("8 tenants, 0 failed"), "{said}");
    let out = d.join("restored");
    let (ok, said) = fenec(&[
        "restore",
        "--tenants",
        root.to_str().unwrap(),
        out.to_str().unwrap(),
    ]);
    assert!(ok, "{said}");
    assert!(said.contains("doomed: gone from the node"), "{said}");
    assert!(
        said.contains("7 tenants restored, 1 passed over, 0 failed"),
        "{said}"
    );
    assert!(!out.join("doomed.fenec").exists());

    // Each tenant whole, to its last write; and the directory a node's.
    let restored = Tenants::new(&out).unwrap();
    for (name, seq, rows) in &stood {
        let t = restored.get(name).unwrap();
        let g = t.db.read().unwrap();
        assert_eq!((g.change_seq(), count(&g)), (*seq, *rows), "{name}");
    }
}

/// Every tenant at a moment: the node's tenants then, each as it stood.
#[test]
fn every_tenant_restores_to_a_moment() {
    let d = scratch("moment");
    let tenants = Arc::new(
        Tenants::new(d.join("node"))
            .unwrap()
            .with_replication(Replicated {
                token: REPL.into(),
                buffer: 8 << 20,
                upstream: None,
                sync_on_write: true,
            }),
    );
    let server = Server::with_tenants(
        Arc::clone(&tenants),
        Config {
            addr: "127.0.0.1:0".into(),
            sync_on_write: true,
            ..Config::default()
        },
    );
    let listener = server.bind().unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    let a = tenants.create("a").unwrap();
    exec(&a, "create collection notes (title text, n int @hash)");
    exec(&a, "put notes {title: \"early\", n: 1}");
    let root = d.join("root");
    let mut archiver = Command::new(env!("CARGO_BIN_EXE_fenec"))
        .args([
            "archive",
            "--tenants",
            &url,
            root.to_str().unwrap(),
            "--token",
            REPL,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let at = a.db.read().unwrap().change_seq();
    let probe = d.join("probe.fenec");
    until("`a` was not archived", || {
        Archive::new(root.join("a"))
            .and_then(|x| x.restore(&probe, Target::End))
            .is_ok_and(|r| r.seq >= at)
    });
    std::thread::sleep(Duration::from_millis(30));
    let moment = fenec_core::time::format_iso(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64,
    );
    std::thread::sleep(Duration::from_millis(30));
    exec(&a, "put notes {title: \"late\", n: 2}");
    let b = tenants.create("b").unwrap();
    exec(&b, "create collection notes (title text, n int @hash)");
    let (a_end, b_end) = (
        a.db.read().unwrap().change_seq(),
        b.db.read().unwrap().change_seq(),
    );
    for (name, seq) in [("a", a_end), ("b", b_end)] {
        let probe = d.join(format!("probe-{name}.fenec"));
        until(&format!("`{name}` was not archived"), || {
            Archive::new(root.join(name))
                .and_then(|x| x.restore(&probe, Target::End))
                .is_ok_and(|r| r.seq >= seq)
        });
    }
    let _ = Command::new("kill")
        .args(["-TERM", &archiver.id().to_string()])
        .status();
    assert!(archiver.wait().unwrap().success());

    let out = d.join("then");
    let (ok, said) = fenec(&[
        "restore",
        "--tenants",
        root.to_str().unwrap(),
        out.to_str().unwrap(),
        "--to",
        &moment,
    ]);
    assert!(ok, "{said}");
    assert!(said.contains("b: archived only from after then"), "{said}");
    let db = fenec_core::fs::open(out.join("a.fenec")).unwrap();
    assert_eq!((db.change_seq(), count(&db)), (at, 1));
    assert!(!out.join("b.fenec").exists());
    // A change names one tenant's: refused for all of them.
    let (ok, said) = fenec(&[
        "restore",
        "--tenants",
        root.to_str().unwrap(),
        d.join("x").to_str().unwrap(),
        "--to-change",
        "3",
    ]);
    assert!(!ok && said.contains("restored to a moment"), "{said}");
}
