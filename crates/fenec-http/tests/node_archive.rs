//! A node's tenants archived over one stream: nodes serving in this
//! process, an archiver of every tenant beside them, and what each tenant's
//! archive restores next to what the node holds -- tenants made, idle and
//! closed, deleted, moved between nodes, made again under a name, and
//! promoted on a standby.

use fenec_core::prelude::*;
use fenec_http::archive::{Archive, Target};
use fenec_http::node_archive::{self, NodeArchive, Restoring};
use fenec_http::replication::Feed;
use fenec_http::tenants::{Replicated, Tenant, Tenants};
use fenec_http::{Config, Server};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const REPL: &str = "repl";

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fenec-node-archive-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Node {
    url: String,
    tenants: Arc<Tenants>,
}

/// A node of tenants that replicates them, as `fenec-server --dir
/// --replication-token --sync always` serves one; `follows`, the standby
/// of the node at that address.
fn node(dir: &Path, follows: Option<&str>) -> Node {
    node_with(dir, follows, |t| t)
}

/// [`node`], its registry built further by `build`.
fn node_with(dir: &Path, follows: Option<&str>, build: impl FnOnce(Tenants) -> Tenants) -> Node {
    fenec_http::audit::set_delay(0);
    let tenants = Arc::new(build(Tenants::new(dir).unwrap().with_replication(
        Replicated {
            token: REPL.into(),
            buffer: 8 << 20,
            upstream: follows.map(str::to_string),
            sync_on_write: true,
        },
    )));
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        admin_token: Some("adm".into()),
        sync_on_write: true,
        ..Config::default()
    };
    let server = Server::with_tenants(Arc::clone(&tenants), cfg);
    let listener = server.bind().unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Node { url, tenants }
}

/// A write as a client makes one: executed, then fsynced -- which is what
/// sends it on.
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

fn seq(t: &Tenant) -> u64 {
    t.db.read().unwrap().change_seq()
}

/// Rows by id, as a restore and the node are compared.
type Rows = Vec<(u64, Vec<Value>)>;

fn rows(db: &Database, sql: &str) -> Rows {
    let r = db.query(&fenec_ql::parse_one(sql).unwrap(), &[]).unwrap();
    r.rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| (r.id, r.values.clone()))
        .collect()
}

/// A tenant made, with a collection and `n` notes.
fn made(node: &Node, name: &str, n: usize) -> Arc<Tenant> {
    let t = node.tenants.create(name).unwrap();
    exec(&t, "create collection notes (title text, n int @hash)");
    for i in 0..n {
        exec(&t, &format!("put notes {{title: \"{name} {i}\", n: {i}}}"));
    }
    t
}

/// An archiver of every tenant of the node at `url`, on its own thread.
struct Archiver {
    stop: Arc<AtomicBool>,
    done: std::thread::JoinHandle<std::io::Result<()>>,
}

fn archiver(root: &Path, url: &str) -> Archiver {
    let stop = Arc::new(AtomicBool::new(false));
    let (flag, root, url) = (Arc::clone(&stop), root.to_path_buf(), url.to_string());
    let done = std::thread::spawn(move || {
        let upstream = node_archive::node_upstream(&url, REPL.into()).unwrap();
        NodeArchive::new(&root, None)?.follow(&upstream, &flag, &|_| {})
    });
    Archiver { stop, done }
}

impl Archiver {
    fn finish(self) {
        self.stop.store(true, Ordering::SeqCst);
        self.done.join().unwrap().unwrap();
    }
}

/// Waits until `cond` holds, 20 s at most.
fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits until a restore of `arch` to the end reaches `seq`.
fn archived(arch: &Path, seq: u64) {
    let probe = arch.parent().unwrap().parent().unwrap().join(format!(
        "probe-{}.fenec",
        arch.file_name().unwrap().to_string_lossy()
    ));
    until(&format!("{} did not reach {seq}", arch.display()), || {
        arch.is_dir()
            && Archive::new(arch)
                .and_then(|a| a.restore(&probe, Target::End))
                .is_ok_and(|r| r.seq >= seq)
    });
}

fn images(arch: &Path) -> usize {
    std::fs::read_dir(arch)
        .unwrap()
        .filter(|e| {
            let n = e.as_ref().unwrap().file_name();
            n.to_string_lossy().starts_with("image-")
        })
        .count()
}

fn restored(arch: &Path, out: &Path) -> Database {
    let _ = std::fs::remove_file(out);
    Archive::new(arch)
        .unwrap()
        .restore(out, Target::End)
        .unwrap();
    fenec_core::fs::open(out).unwrap()
}

/// Closes the node's tenants that nothing holds until `n` are closed: the
/// node's stream holds a tenant open only until it has sent what its disk
/// holds.
fn close_all(tenants: &Tenants, n: usize) {
    let mut closed = 0;
    until("the tenants did not close", || {
        closed += tenants.close_idle(Duration::ZERO);
        closed >= n
    });
}

#[test]
fn every_tenant_is_archived_over_one_stream_and_none_is_held_open() {
    let d = scratch("all");
    let n = node(&d.join("node"), None);
    let mut held: Vec<(String, Arc<Tenant>)> = ["a", "b", "c"]
        .iter()
        .map(|name| (name.to_string(), made(&n, name, 5)))
        .collect();
    let root = d.join("root");
    let a = archiver(&root, &n.url);
    // A tenant made while the archiver runs is archived from its image.
    held.push(("d".into(), made(&n, "d", 2)));
    for (name, t) in &held {
        for i in 0..5 {
            exec(
                t,
                &format!("put notes {{title: \"{name} late {i}\", n: {}}}", 100 + i),
            );
        }
    }
    for (name, t) in &held {
        archived(&root.join(name), seq(t));
    }
    // One stream for them all, and it holds none of them open.
    let streams = n.tenants.node_streams().unwrap();
    assert_eq!(streams.connected(), 1);
    let scrape = {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(n.url.trim_start_matches("http://")).unwrap();
        s.write_all(b"GET /_metrics HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer adm\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    };
    assert!(scrape.contains("\nfenec_node_streams 1\n"), "{scrape}");
    let ends: Vec<(String, u64, Rows)> = held
        .iter()
        .map(|(name, t)| {
            (
                name.clone(),
                seq(t),
                rows(&t.db.read().unwrap(), "get notes"),
            )
        })
        .collect();
    drop(held);
    close_all(&n.tenants, 4);

    // Opened again, a tenant's feed starts where the old one ended and the
    // stream goes on from it: its writes are archived with no image more.
    let t = n.tenants.get("a").unwrap();
    exec(&t, "put notes {title: \"after the close\", n: 999}");
    archived(&root.join("a"), seq(&t));
    assert_eq!(images(&root.join("a")), 1);
    let a_end = rows(&t.db.read().unwrap(), "get notes");
    drop(t);
    a.finish();

    for (name, at, want) in &ends {
        let db = restored(&root.join(name), &d.join(format!("{name}.fenec")));
        if name == "a" {
            assert_eq!(rows(&db, "get notes"), a_end);
        } else {
            assert_eq!(db.change_seq(), *at);
            assert_eq!(&rows(&db, "get notes"), want, "{name}");
            assert_eq!(images(&root.join(name)), 1, "{name}");
        }
    }
}

/// A write and a close on its heels, forty times: each opening's feed
/// starts where the last one ended, the stream goes on from it, and the
/// archive takes no image more and has no gap. (The close waiting for the
/// stream is held to its job where the stream is held up elsewhere, below:
/// here a durable write already waits for it.)
#[test]
fn a_tenant_closed_and_opened_between_writes_takes_no_image() {
    let d = scratch("close");
    let n = node(&d.join("node"), None);
    let x = made(&n, "x", 1);
    let at = seq(&x);
    drop(x);
    let root = d.join("root");
    let a = archiver(&root, &n.url);
    archived(&root.join("x"), at);
    let mut closed = 0;
    for i in 0..40 {
        let t = n.tenants.get("x").unwrap();
        exec(&t, &format!("put notes {{title: \"x {i}\", n: {i}}}"));
        drop(t);
        closed += n.tenants.close_idle(Duration::ZERO);
    }
    let t = n.tenants.get("x").unwrap();
    archived(&root.join("x"), seq(&t));
    let want = rows(&t.db.read().unwrap(), "get notes");
    drop(t);
    a.finish();
    assert!(closed > 0, "the tenant never closed");
    assert_eq!(images(&root.join("x")), 1);
    let v = Archive::new(root.join("x")).unwrap().verify().unwrap();
    assert!(v.gaps.is_empty(), "{:?}", v.gaps);
    assert_eq!(
        rows(&restored(&root.join("x"), &d.join("x.fenec")), "get notes"),
        want
    );
}

/// The stream held up by another tenant -- here inside that tenant's open,
/// which it makes to take an image -- does not lose a tenant's writes: the
/// tenant, open as the stream connected, waits for them to be sent before
/// it closes, though its write was answered without the stream (after
/// `SENT_WAIT`, a second). Closed with them unsent, its feed went with it
/// and they came again as an image.
#[test]
fn a_tenant_waits_for_a_stream_held_up_elsewhere_before_it_closes() {
    #[derive(Default)]
    struct Gate {
        armed: bool,
        entered: bool,
        released: bool,
    }
    let gate = Arc::new((
        std::sync::Mutex::new(Gate::default()),
        std::sync::Condvar::new(),
    ));
    let g = Arc::clone(&gate);
    let d = scratch("held-up");
    // The first open after the gate is armed is held there until it is
    // released: the stream's, of the tenant it takes an image of.
    let n = node_with(&d.join("node"), None, |t| {
        t.with_setup(move |_| {
            let (m, cv) = &*g;
            let mut s = m.lock().unwrap();
            if s.armed {
                s.armed = false;
                s.entered = true;
                cv.notify_all();
                while !s.released {
                    s = cv.wait(s).unwrap();
                }
            }
            Ok(())
        })
    });
    let big = made(&n, "big", 20);
    let x = made(&n, "x", 2);
    let root = d.join("root");
    let a = archiver(&root, &n.url);
    archived(&root.join("big"), seq(&big));
    archived(&root.join("x"), seq(&x));
    a.finish();
    // Behind in its archive and closed, `big` is opened by the next stream
    // for an image; `x` stands where its archive does, and is open.
    exec(&big, "put notes {title: \"unarchived\", n: 77}");
    drop((big, x));
    close_all(&n.tenants, 2);
    let x = n.tenants.get("x").unwrap();
    gate.0.lock().unwrap().armed = true;
    let a = archiver(&root, &n.url);
    {
        let (m, cv) = &*gate;
        let (_s, waited) = cv
            .wait_timeout_while(m.lock().unwrap(), Duration::from_secs(20), |s| !s.entered)
            .unwrap();
        assert!(!waited.timed_out(), "the stream never opened `big`");
    }
    exec(&x, "put notes {title: \"while the stream is held\", n: 88}");
    let x_at = seq(&x);
    let want = rows(&x.db.read().unwrap(), "get notes");
    drop(x);
    let closed = n.tenants.close_idle(Duration::ZERO);
    {
        let (m, cv) = &*gate;
        m.lock().unwrap().released = true;
        cv.notify_all();
    }
    assert_eq!(closed, 0, "`x` closed with its write unsent");
    // Sent once the stream goes on, and closed after.
    close_all(&n.tenants, 1);
    archived(&root.join("x"), x_at);
    a.finish();
    assert_eq!(images(&root.join("x")), 1);
    assert_eq!(
        rows(&restored(&root.join("x"), &d.join("x.fenec")), "get notes"),
        want
    );
}

#[test]
fn a_deleted_tenant_is_no_longer_followed_and_its_archive_is_kept() {
    let d = scratch("deleted");
    let n = node(&d.join("node"), None);
    let keep = made(&n, "keep", 3);
    let doomed = made(&n, "doomed", 4);
    let root = d.join("root");
    let a = archiver(&root, &n.url);
    archived(&root.join("doomed"), seq(&doomed));
    let last = rows(&doomed.db.read().unwrap(), "get notes");
    drop(doomed);
    n.tenants.delete("doomed").unwrap();
    until("the archive was not told the tenant is gone", || {
        node_archive::gone_at(&root.join("doomed")).is_some()
    });
    // The others go on.
    exec(&keep, "put notes {title: \"still here\", n: 50}");
    archived(&root.join("keep"), seq(&keep));
    drop(keep);
    a.finish();

    let db = restored(&root.join("doomed"), &d.join("doomed.fenec"));
    assert_eq!(rows(&db, "get notes"), last);
    // A node restored from the archive holds what the node held: not the
    // tenant deleted before.
    let out = d.join("restored");
    let done = node_archive::restore_all(&root, &out, Target::End, None).unwrap();
    assert!(
        matches!(done[0], (ref t, Restoring::Gone(_)) if t == "doomed"),
        "{done:?}"
    );
    assert!(
        matches!(done[1], (ref t, Restoring::Restored(..)) if t == "keep"),
        "{done:?}"
    );
    assert!(!out.join("doomed.fenec").exists());
    assert!(out.join("keep.fenec").exists());
    // To a moment before the delete, it is one of them.
    let before = node_archive::gone_at(&root.join("doomed")).unwrap() - 1;
    let out = d.join("restored-before");
    let done = node_archive::restore_all(&root, &out, Target::Time(before), None).unwrap();
    assert!(
        done.iter()
            .all(|(_, r)| matches!(r, Restoring::Restored(..))),
        "{done:?}"
    );
}

/// A tenant deleted and made again under its name starts a history of its
/// own, at fewer changes than its archive holds: taken into that archive,
/// its writes would land among the old tenant's of the same numbers. The
/// old archive is moved aside and a new one begins.
#[test]
fn a_tenant_made_again_under_its_name_begins_a_new_archive() {
    let d = scratch("again");
    let n = node(&d.join("node"), None);
    let old = made(&n, "e", 10);
    let root = d.join("root");
    let a = archiver(&root, &n.url);
    archived(&root.join("e"), seq(&old));
    let old_rows = rows(&old.db.read().unwrap(), "get notes");
    drop(old);
    n.tenants.delete("e").unwrap();
    let new = made(&n, "e", 2);
    let aside = || {
        std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("e."))
    };
    until("the old archive was not moved aside", || aside().is_some());
    archived(&root.join("e"), seq(&new));
    let new_rows = rows(&new.db.read().unwrap(), "get notes");
    drop(new);
    a.finish();

    let aside = aside().unwrap();
    assert_eq!(
        rows(&restored(&aside, &d.join("old.fenec")), "get notes"),
        old_rows
    );
    assert_eq!(
        rows(
            &restored(&root.join("e"), &d.join("new.fenec")),
            "get notes"
        ),
        new_rows
    );
    assert!(node_archive::gone_at(&root.join("e")).is_none());
    // An archive moved aside is no tenant's of the root.
    let names: Vec<String> = node_archive::tenant_dirs(&root)
        .unwrap()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(names, ["e"]);
}

/// Moved to another node -- frozen, its image installed there, deleted
/// here -- a tenant is gone from this node's archive and goes on in the
/// other's on the same history; moved back, this node's archive goes on
/// where it stopped, with a gap for its time away.
#[test]
fn a_tenant_moved_between_nodes_goes_on_in_each_nodes_archive() {
    let d = scratch("moved");
    let (na, nb) = (node(&d.join("a"), None), node(&d.join("b"), None));
    let (root_a, root_b) = (d.join("root-a"), d.join("root-b"));
    let (aa, ab) = (archiver(&root_a, &na.url), archiver(&root_b, &nb.url));
    let t = made(&na, "m", 5);
    archived(&root_a.join("m"), seq(&t));
    let history = t.db.read().unwrap().history().current();
    drop(t);

    let r#move = |from: &Node, to: &Node| {
        from.tenants.freeze("m").unwrap();
        let image = from.tenants.export("m").unwrap();
        to.tenants.import("m", &image).unwrap();
        from.tenants.delete("m").unwrap();
    };
    r#move(&na, &nb);
    until("node a's archive was not told the tenant left", || {
        node_archive::gone_at(&root_a.join("m")).is_some()
    });
    let t = nb.tenants.get("m").unwrap();
    for i in 0..5 {
        exec(
            &t,
            &format!("put notes {{title: \"on b {i}\", n: {}}}", 200 + i),
        );
    }
    archived(&root_b.join("m"), seq(&t));
    // The same history: no fork in a move.
    assert_eq!(t.db.read().unwrap().history().current(), history);
    drop(t);

    r#move(&nb, &na);
    // Taken back -- its image, and noted back after it -- before it is
    // written: a write landing first is in that image, and the
    // archive then has no segment after the gap to show it (macOS CI).
    until("node a's archive still says the tenant is gone", || {
        node_archive::gone_at(&root_a.join("m")).is_none()
    });
    let t = na.tenants.get("m").unwrap();
    exec(&t, "put notes {title: \"back on a\", n: 300}");
    archived(&root_a.join("m"), seq(&t));
    let end = rows(&t.db.read().unwrap(), "get notes");
    drop(t);
    until("node b's archive was not told the tenant left", || {
        node_archive::gone_at(&root_b.join("m")).is_some()
    });
    aa.finish();
    ab.finish();

    assert_eq!(
        rows(
            &restored(&root_a.join("m"), &d.join("a.fenec")),
            "get notes"
        ),
        end
    );
    let v = Archive::new(root_a.join("m")).unwrap().verify().unwrap();
    assert_eq!((v.images, v.gaps.len()), (2, 1), "{v:?}");
}

/// A standby's tenant archived as it follows, then promoted: the stream
/// sends the history the promotion began before the writes after it, so
/// an archiver started again goes on where it stood rather than take an
/// image.
#[test]
fn a_promoted_tenant_is_archived_on_its_new_history_without_an_image() {
    let d = scratch("promoted");
    let p = node(&d.join("primary"), None);
    let s = node(&d.join("standby"), Some(&p.url));
    let t = made(&p, "t", 3);
    s.tenants
        .import("t", &p.tenants.export("t").unwrap())
        .unwrap();
    s.tenants.follow("t", None).unwrap();
    let root = d.join("root");
    let a = archiver(&root, &s.url);
    for i in 0..5 {
        exec(
            &t,
            &format!("put notes {{title: \"from p {i}\", n: {}}}", 10 + i),
        );
    }
    archived(&root.join("t"), seq(&t));
    drop(t);

    let (_, id) = s.tenants.promote("t").unwrap();
    let st = s.tenants.get("t").unwrap();
    for i in 0..3 {
        exec(
            &st,
            &format!("put notes {{title: \"on s {i}\", n: {}}}", 20 + i),
        );
    }
    archived(&root.join("t"), seq(&st));
    let history = |root: &Path| {
        fenec_core::history::History::decode(
            &std::fs::read(root.join("t").join("history")).unwrap(),
        )
        .unwrap()
        .current()
    };
    until("the archive did not take the new history", || {
        history(&root) == id
    });
    a.finish();
    let before = images(&root.join("t"));

    let a = archiver(&root, &s.url);
    exec(&st, "put notes {title: \"after the restart\", n: 30}");
    archived(&root.join("t"), seq(&st));
    let end = rows(&st.db.read().unwrap(), "get notes");
    drop(st);
    a.finish();
    assert_eq!(images(&root.join("t")), before);
    assert_eq!(
        rows(&restored(&root.join("t"), &d.join("t.fenec")), "get notes"),
        end
    );
}

/// Each tenant's images are its own: one taken of an archive whose tenant
/// wrote since, none of one that did not.
#[test]
fn each_tenant_takes_its_own_images() {
    let d = scratch("images");
    let n = node(&d.join("node"), None);
    let busy = made(&n, "busy", 3);
    let quiet = made(&n, "quiet", 3);
    let quiet_at = seq(&quiet);
    drop(quiet);
    let root = d.join("root");
    let stop = Arc::new(AtomicBool::new(false));
    let (flag, r, url) = (Arc::clone(&stop), root.clone(), n.url.clone());
    let arch = Arc::new(NodeArchive::new(&r, None).unwrap());
    let following = {
        let arch = Arc::clone(&arch);
        std::thread::spawn(move || {
            let upstream = node_archive::node_upstream(&url, REPL.into()).unwrap();
            arch.follow(&upstream, &flag, &|_| {})
        })
    };
    archived(&root.join("busy"), seq(&busy));
    archived(&root.join("quiet"), quiet_at);
    // The first pass looks at each once.
    arch.keep_once(None, &stop, &|_| {});
    assert_eq!(
        (images(&root.join("busy")), images(&root.join("quiet"))),
        (1, 1)
    );
    exec(&busy, "put notes {title: \"more\", n: 9}");
    archived(&root.join("busy"), seq(&busy));
    arch.keep_once(None, &stop, &|_| {});
    assert_eq!(
        (images(&root.join("busy")), images(&root.join("quiet"))),
        (2, 1)
    );
    drop(busy);
    stop.store(true, Ordering::SeqCst);
    following.join().unwrap().unwrap();
    // Two archivers on one root would write each other's segments: the
    // second is refused while the first holds it, and taken after.
    assert!(NodeArchive::new(&root, None).is_err());
    drop(arch);
    let _held = NodeArchive::new(&root, None).unwrap();
}

/// One tenant's archive that does not read -- a segment of something else,
/// here -- is left as it is, for an operator, and every other tenant is
/// archived as before.
#[test]
fn an_archive_that_does_not_read_is_left_alone_and_the_others_go_on() {
    let d = scratch("broken");
    let n = node(&d.join("node"), None);
    let bad = made(&n, "bad", 2);
    let good = made(&n, "good", 2);
    let root = d.join("root");
    std::fs::create_dir_all(root.join("bad")).unwrap();
    let junk = root.join("bad").join("writes-00000000000000000001.log");
    std::fs::write(&junk, b"not an archive segment").unwrap();
    let a = archiver(&root, &n.url);
    exec(&good, "put notes {title: \"more\", n: 3}");
    exec(&bad, "put notes {title: \"more\", n: 3}");
    archived(&root.join("good"), seq(&good));
    drop((bad, good));
    a.finish();
    assert_eq!(std::fs::read(&junk).unwrap(), b"not an archive segment");
    assert_eq!(images(&root.join("bad")), 0);
}

/// A gate in a node's tenant setup: the first open after it is armed is
/// held there until it is released -- the node's stream's, of a tenant it
/// opens to look at.
#[derive(Default)]
struct GateState {
    armed: bool,
    entered: bool,
    released: bool,
}

type Gate = Arc<(std::sync::Mutex<GateState>, std::sync::Condvar)>;

fn gated(dir: &Path, sync_on_write: bool) -> (Node, Gate) {
    let gate: Gate = Arc::default();
    let g = Arc::clone(&gate);
    fenec_http::audit::set_delay(0);
    let tenants = Arc::new(
        Tenants::new(dir)
            .unwrap()
            .with_replication(Replicated {
                token: REPL.into(),
                buffer: 8 << 20,
                upstream: None,
                sync_on_write,
            })
            .with_setup(move |_| {
                let (m, cv) = &*g;
                let mut s = m.lock().unwrap();
                if s.armed {
                    s.armed = false;
                    s.entered = true;
                    cv.notify_all();
                    while !s.released {
                        s = cv.wait(s).unwrap();
                    }
                }
                Ok(())
            }),
    );
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        sync_on_write,
        ..Config::default()
    };
    let server = Server::with_tenants(Arc::clone(&tenants), cfg);
    let listener = server.bind().unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    (Node { url, tenants }, gate)
}

fn arm(gate: &Gate) {
    gate.0.lock().unwrap().armed = true;
}

fn entered(gate: &Gate) {
    let (m, cv) = &**gate;
    let (_s, waited) = cv
        .wait_timeout_while(m.lock().unwrap(), Duration::from_secs(20), |s| !s.entered)
        .unwrap();
    assert!(
        !waited.timed_out(),
        "the stream never opened the gated tenant"
    );
}

fn release(gate: &Gate) {
    let (m, cv) = &**gate;
    m.lock().unwrap().released = true;
    cv.notify_all();
}

/// The feed `t`'s writes are sent from.
fn feed_of(t: &Tenant) -> Arc<Feed> {
    Arc::clone(t.repl.as_ref().and_then(|r| r.feed()).expect("a feed"))
}

/// How many durable writes to `t` waited out their whole wait for a
/// stream: a write that waited for a stream held up elsewhere took all of
/// it, 1.008 s. Asked rather than timed, which a machine slowed by its
/// neighbours would fail.
fn waited_out(t: &Tenant) -> u64 {
    feed_of(t).waited_out()
}

/// The node's one stream held up on one tenant -- inside the open it makes
/// to take that tenant's image -- holds up no other tenant's durable write:
/// each tenant's own stream stalled that tenant alone, and one for all of
/// them held every tenant's next write for its `SENT_WAIT`.
#[test]
fn a_stream_held_up_on_one_tenant_holds_up_no_others_writes() {
    let d = scratch("hol");
    let (n, gate) = gated(&d.join("node"), true);
    let big = made(&n, "big", 20);
    let y = made(&n, "y", 2);
    let z = made(&n, "z", 2);
    let root = d.join("root");
    let a = archiver(&root, &n.url);
    for (name, t) in [("big", &big), ("y", &y), ("z", &z)] {
        archived(&root.join(name), seq(t));
    }
    a.finish();
    exec(&big, "put notes {title: \"unarchived\", n: 77}");
    drop((big, y, z));
    close_all(&n.tenants, 3);
    arm(&gate);
    let a = archiver(&root, &n.url);
    entered(&gate);
    // Opened while the stream is held up: it stands where they do, and is
    // put on their feeds.
    for name in ["y", "z"] {
        let t = n.tenants.get(name).unwrap();
        exec(&t, "put notes {title: \"while held up\", n: 2}");
        assert_eq!(
            waited_out(&t),
            0,
            "a write to `{name}` waited for the stream"
        );
    }
    release(&gate);
    for name in ["y", "z"] {
        let t = n.tenants.get(name).unwrap();
        archived(&root.join(name), seq(&t));
        assert_eq!(images(&root.join(name)), 1);
    }
    a.finish();
}

/// The node's stream as a client that stops reading sees it: the response's
/// head read, then each message as the node frames one -- its tag, its
/// length, and the tenant's name leading the body.
struct RawStream {
    r: std::io::BufReader<std::net::TcpStream>,
}

impl RawStream {
    /// The stream of every tenant, the client standing where `places`
    /// says (`<tenant> <since> <history hex> <image>` a line).
    fn open(url: &str, places: &str) -> RawStream {
        use std::io::{BufRead, Write};
        let addr = url.trim_start_matches("http://");
        let s = std::net::TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        write!(
            &s,
            "POST /_replication/tenants HTTP/1.1\r\nHost: {addr}\r\n\
             Authorization: Bearer {REPL}\r\nContent-Length: {}\r\n\r\n{places}",
            places.len()
        )
        .unwrap();
        let mut r = std::io::BufReader::new(s);
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        assert!(line.contains(" 200 "), "{line}");
        while line != "\r\n" {
            line.clear();
            r.read_line(&mut line).unwrap();
        }
        RawStream { r }
    }

    /// The next message's tag and tenant.
    fn next(&mut self) -> (u8, String) {
        use std::io::Read;
        let mut head = [0u8; 9];
        self.r.read_exact(&mut head).unwrap();
        let len = u64::from_le_bytes(head[1..].try_into().unwrap()) as usize;
        let mut body = vec![0u8; len];
        self.r.read_exact(&mut body).unwrap();
        let name = match head[0] {
            b'H' | b'I' | b'W' | b'L' | b'G' => {
                String::from_utf8(body[1..1 + body[0] as usize].to_vec()).unwrap()
            }
            _ => String::new(),
        };
        (head[0], name)
    }

    /// Reads until `tag` comes for `name`.
    fn until(&mut self, tag: u8, name: &str) {
        while self.next() != (tag, name.to_string()) {}
    }
}

/// Nor does an archiver reading slower than the stream fills its socket:
/// the stream blocked in a write to it, waited for as it kept up, held every
/// tenant's next durable write for a second.
#[test]
fn an_archiver_that_stops_reading_holds_up_no_write() {
    let d = scratch("unread");
    let n = node(&d.join("node"), None);
    let a = made(&n, "a", 2);
    let b = made(&n, "b", 2);
    let mut raw = RawStream::open(&n.url, "");
    let mut imaged = std::collections::BTreeSet::new();
    while imaged.len() < 2 {
        if let (b'I', name) = raw.next() {
            imaged.insert(name);
        }
    }
    // Sent a write of `b` after its image: the stream keeps up with it, and
    // a write of `b` waits for it.
    exec(&b, "put notes {title: \"kept up\", n: 3}");
    raw.until(b'W', "b");
    // Read no more, and write `a` until the socket and the stream's buffer
    // are full many times over: the stream is blocked in a write.
    let title = "x".repeat(256 << 10);
    for i in 0..64 {
        exec(&a, &format!("put notes {{title: \"{title}\", n: {i}}}"));
    }
    let before = waited_out(&b);
    exec(&b, "put notes {title: \"beside a full socket\", n: 4}");
    assert_eq!(
        waited_out(&b),
        before,
        "a write to `b` waited for the stream"
    );
    drop(raw);
}

/// Where a client stands in `t`: at its last change, on its history.
fn place_of(t: &Tenant) -> String {
    let g = t.db.read().unwrap();
    format!(
        "{} {} {:016x} 0\n",
        t.name(),
        g.change_seq(),
        g.history().current()
    )
}

/// Nor does a tenant whose write lock is held -- a long `/batch`, a compact
/// under the lock -- as the stream comes to judge it: waited for unheld, its
/// lock held the stream, and the next durable write of a tenant it had just
/// said hello to waited out its whole wait, 1.014 s.
#[test]
fn a_tenant_write_locked_as_the_stream_judges_it_holds_up_no_other() {
    let d = scratch("judge-lock");
    let n = node(&d.join("node"), None);
    let y = made(&n, "a-y", 2);
    let slow = made(&n, "slow", 2);
    let (locked, free) = (std::sync::mpsc::channel(), std::sync::mpsc::channel::<()>());
    let holder = {
        let db = Arc::clone(&slow.db);
        std::thread::spawn(move || {
            let _g = db.write().unwrap();
            locked.0.send(()).unwrap();
            let _ = free.1.recv();
        })
    };
    locked.1.recv().unwrap();
    // The client stands where `a-y` does and holds nothing of `slow`: the
    // stream says hello to `a-y` -- caught up, a write of it waits for the
    // stream -- and comes to judge `slow` next, in name order. `a-y`'s
    // writes go out in the round after, so once its hello is here the
    // stream has its write to send behind `slow`'s lock whatever the timing.
    let mut raw = RawStream::open(&n.url, &place_of(&y));
    raw.until(b'H', "a-y");
    exec(&y, "put notes {title: \"beside a locked tenant\", n: 3}");
    assert_eq!(waited_out(&y), 0, "a write to `a-y` waited for the stream");
    free.0.send(()).unwrap();
    holder.join().unwrap();
    raw.until(b'W', "a-y");
    drop((raw, y, slow));
}

/// An image waiting for its tenant's disk while the tenant closes and opens
/// again goes out from the tenant as it is then: sent as taken, it put the
/// closed tenant's feed back among the stream's, which read nothing from it
/// again -- the archive stood still, the node at 8 and it at 5, until the
/// archiver connected again.
#[test]
fn an_image_pending_across_a_close_and_an_open_goes_on_from_the_open() {
    let d = scratch("pending-reopen");
    let (n, gate) = gated(&d.join("node"), false);
    let a = made(&n, "a", 2);
    let z = made(&n, "zbig", 5);
    let root = d.join("root");
    let arch = archiver(&root, &n.url);
    archived(&root.join("a"), seq(&a));
    archived(&root.join("zbig"), seq(&z));
    arch.finish();
    // Both a write past the archive and closed.
    exec(&z, "put notes {title: \"behind\", n: 70}");
    exec(&a, "put notes {title: \"behind\", n: 71}");
    drop((a, z));
    close_all(&n.tenants, 2);
    // `a` opened again -- its feed starts there, short of the archive's
    // place -- and written past its disk: its image waits for the disk.
    let a = n.tenants.get("a").unwrap();
    exec_unsynced(&a, "put notes {title: \"not on disk\", n: 72}");
    arm(&gate);
    let arch = archiver(&root, &n.url);
    // Held in `zbig`'s open, after `a`'s image, in name order.
    entered(&gate);
    drop(a);
    close_all(&n.tenants, 1);
    let a = n.tenants.get("a").unwrap();
    exec(&a, "put notes {title: \"after the open\", n: 73}");
    release(&gate);
    archived(&root.join("a"), seq(&a));
    // And on from there, across another close and open.
    exec(&a, "put notes {title: \"later\", n: 74}");
    drop(a);
    close_all(&n.tenants, 1);
    let a = n.tenants.get("a").unwrap();
    exec(&a, "put notes {title: \"later still\", n: 75}");
    archived(&root.join("a"), seq(&a));
    drop(a);
    arch.finish();
}

/// A tenant deleted while the stream is busy elsewhere is gone from the
/// node's time of the delete, not from when the stream came to it: told
/// late, a moment between restored a tenant the node no longer held.
#[test]
fn a_tenant_deleted_while_the_stream_is_busy_is_gone_from_the_delete() {
    let d = scratch("gone-when");
    let (n, gate) = gated(&d.join("node"), true);
    let big = made(&n, "big", 5);
    let e = made(&n, "e", 3);
    let root = d.join("root");
    let a = archiver(&root, &n.url);
    archived(&root.join("big"), seq(&big));
    archived(&root.join("e"), seq(&e));
    let image = n.tenants.export("big").unwrap();
    drop((big, e));
    arm(&gate);
    n.tenants.import("imp", &image).unwrap();
    entered(&gate);
    n.tenants.delete("e").unwrap();
    let after = a_moment();
    release(&gate);
    until("the archive was not told `e` went", || {
        node_archive::gone_at(&root.join("e")).is_some()
    });
    a.finish();
    let (r, _) = restored_rows(&root, &d.join("out"), Target::Time(after), "e");
    assert!(matches!(r, Restoring::Gone(_)), "{r:?}");
}

/// A tenant deleted and made again while the archiver was away is heard of
/// as the new tenant alone: the line it moves aside is noted gone, by the
/// time the node said hello to the new one at the latest.
#[test]
fn a_line_moved_aside_while_the_archiver_was_away_is_noted_gone() {
    let d = scratch("aside-gone");
    let n = node(&d.join("node"), None);
    let e = made(&n, "e", 3);
    let root = d.join("root");
    let a = archiver(&root, &n.url);
    archived(&root.join("e"), seq(&e));
    a.finish();
    drop(e);
    n.tenants.delete("e").unwrap();
    drop(n.tenants.create("e").unwrap());
    let a = archiver(&root, &n.url);
    let aside = || {
        node_archive::lines(&root)
            .unwrap()
            .into_iter()
            .find(|l| l.tenant == "e" && !l.current)
    };
    until("the old line was not moved aside", || aside().is_some());
    archived(&root.join("e"), 0);
    a.finish();
    assert!(node_archive::gone_at(&aside().unwrap().dir).is_some());
}

/// A tenant made again after a delete, then gone by other means -- its file
/// taken away by hand -- is gone from when the stream saw it go, not from
/// the delete before it: given that time, its archive read as away from
/// before it was made.
#[test]
fn a_tenant_gone_by_hand_is_not_given_an_earlier_delete() {
    let d = scratch("by-hand");
    let n = node(&d.join("node"), None);
    let e = made(&n, "e", 3);
    let root = d.join("root");
    let a = archiver(&root, &n.url);
    archived(&root.join("e"), seq(&e));
    drop(e);
    n.tenants.delete("e").unwrap();
    let e = made(&n, "e", 2);
    until("the old line was not moved aside", || {
        node_archive::lines(&root).unwrap().len() == 2
    });
    archived(&root.join("e"), seq(&e));
    drop(e);
    close_all(&n.tenants, 1);
    let before = now_ms();
    std::fs::remove_file(d.join("node").join("e.fenec")).unwrap();
    // Seen at the stream's next reading of the directory.
    until("the archive was not told `e` went", || {
        node_archive::gone_at(&root.join("e")).is_some()
    });
    a.finish();
    let at = node_archive::gone_at(&root.join("e")).unwrap();
    assert!(
        at >= before,
        "gone at {at}, before the file went at {before}"
    );
}

/// Nor do the syncer and a close wait for it: under `--sync <ms>` a pass
/// over five dirty tenants took 5.06 s with the stream held up, and a
/// close held its tenant's slot, and a request for it, a second.
#[test]
fn the_syncer_and_a_close_wait_for_no_stream() {
    let d = scratch("syncer");
    let (n, gate) = gated(&d.join("node"), false);
    let names = ["t1", "t2", "t3", "t4", "t5"];
    let held: Vec<Arc<Tenant>> = names.iter().map(|x| made(&n, x, 2)).collect();
    let root = d.join("root");
    let a = archiver(&root, &n.url);
    for (x, t) in names.iter().zip(&held) {
        archived(&root.join(x), seq(t));
    }
    // The stream held up in a tenant installed closed, which it opens.
    let image = n.tenants.export("t1").unwrap();
    arm(&gate);
    n.tenants.import("imp", &image).unwrap();
    entered(&gate);
    for t in &held {
        let mut g = t.db.write().unwrap();
        g.execute(&fenec_ql::parse_one("put notes {title: \"buffered\", n: 9}").unwrap())
            .unwrap();
    }
    let feeds: Vec<Arc<Feed>> = held.iter().map(|t| feed_of(t)).collect();
    let mut held = held;
    drop(held.pop());
    // Both done while the stream is still held up: neither waited for it.
    // The close syncs `t5` and leaves it open, its write unsent.
    n.tenants.close_idle(Duration::ZERO);
    n.tenants.sync_dirty();
    for (x, f) in names.iter().zip(&feeds) {
        assert_eq!(f.waited_out(), 0, "`{x}`'s sync waited for the stream");
    }
    release(&gate);
    for (x, t) in names.iter().zip(&held) {
        archived(&root.join(x), seq(t));
    }
    drop(held);
    a.finish();
}

/// An image waits for its tenant's disk without holding up the stream:
/// waited for there, up to 30 s, every other tenant waited with it.
#[test]
fn an_image_waiting_for_its_disk_holds_up_no_other_tenant() {
    let d = scratch("pending");
    // No syncer: a write stays off the disk until flushed here.
    let (n, _gate) = gated(&d.join("node"), false);
    let early = made(&n, "a-early", 2);
    exec_unsynced(&early, "put notes {title: \"not on disk\", n: 3}");
    let later = made(&n, "b-later", 2);
    let root = d.join("root");
    let a = archiver(&root, &n.url);
    exec(&later, "put notes {title: \"on disk\", n: 4}");
    archived(&root.join("b-later"), seq(&later));
    // On disk, the image goes.
    sync(&early);
    archived(&root.join("a-early"), seq(&early));
    drop((early, later));
    a.finish();
}

fn exec_unsynced(t: &Tenant, sql: &str) {
    t.db.write()
        .unwrap()
        .execute(&fenec_ql::parse_one(sql).unwrap())
        .unwrap();
}

fn sync(t: &Tenant) {
    let durable = t.db.write().unwrap().flush().unwrap();
    if let Some(d) = durable {
        d().unwrap();
    }
}

/// A tenant deleted and made again under its name while the stream is busy
/// elsewhere -- its notices come and go meanwhile, and the new tenant
/// writes nothing to give another -- is still noticed: the old archive is
/// moved aside, and a restore brings back the new tenant, not the deleted
/// one.
#[test]
fn a_tenant_made_again_while_the_stream_is_busy_is_noticed() {
    let d = scratch("again-busy");
    let (n, gate) = gated(&d.join("node"), true);
    let big = made(&n, "big", 5);
    let e = made(&n, "e", 10);
    let root = d.join("root");
    let a = archiver(&root, &n.url);
    archived(&root.join("big"), seq(&big));
    archived(&root.join("e"), seq(&e));
    let image = n.tenants.export("big").unwrap();
    drop(big);
    arm(&gate);
    n.tenants.import("imp", &image).unwrap();
    entered(&gate);
    drop(e);
    n.tenants.delete("e").unwrap();
    drop(n.tenants.create("e").unwrap());
    let between = a_moment();
    release(&gate);
    until("the old archive was not moved aside", || {
        std::fs::read_dir(&root)
            .unwrap()
            .any(|x| x.unwrap().file_name().to_string_lossy().starts_with("e."))
    });
    archived(&root.join("e"), 0);
    a.finish();
    let out = d.join("out");
    node_archive::restore_all(&root, &out, Target::End, None).unwrap();
    let db = fenec_core::fs::open(out.join("e.fenec")).unwrap();
    assert!(
        db.collection_names().is_empty(),
        "the deleted tenant came back"
    );
    // Gone from the delete, though the stream heard of it only as the new
    // tenant: the old line told nothing, and answered for the moment.
    let (r, _) = restored_rows(&root, &d.join("between"), Target::Time(between), "e");
    assert!(
        !matches!(&r, Restoring::Restored(_, from) if from.starts_with("e.")),
        "{r:?}"
    );
}

/// A closed tenant written after it closed -- by a holder of its database,
/// as the `@ttl` sweeper is -- then opened, written and closed again while
/// the stream was busy, is read again when the stream comes to it rather
/// than taken where it closed: the archive reaches it.
#[test]
fn a_closed_tenant_written_past_its_close_reaches_its_archive() {
    let d = scratch("unfed");
    let (n, gate) = gated(&d.join("node"), true);
    let x = made(&n, "x", 3);
    let root = d.join("root");
    let a = archiver(&root, &n.url);
    archived(&root.join("x"), seq(&x));
    let db = Arc::clone(&x.db);
    let image = n.tenants.export("x").unwrap();
    drop(x);
    close_all(&n.tenants, 1);
    // Busy first, so the write past the close is told of while it cannot
    // look, and the tenant opens again ahead of the archive.
    arm(&gate);
    n.tenants.import("imp", &image).unwrap();
    entered(&gate);
    {
        let durable = {
            let mut g = db.write().unwrap();
            g.execute(&fenec_ql::parse_one("del notes where n = 0").unwrap())
                .unwrap();
            g.flush().unwrap()
        };
        if let Some(d) = durable {
            d().unwrap();
        }
    }
    drop(db);
    let x = n.tenants.get("x").unwrap();
    exec(&x, "put notes {title: \"acknowledged\", n: 42}");
    let want = seq(&x);
    drop(x);
    close_all(&n.tenants, 1);
    release(&gate);
    archived(&root.join("x"), want);
    a.finish();
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// A moment between two readings of the clock, each a few milliseconds
/// apart from it: what was done before it is before it.
fn a_moment() -> u64 {
    std::thread::sleep(Duration::from_millis(20));
    let t = now_ms();
    std::thread::sleep(Duration::from_millis(20));
    t
}

fn restored_rows(root: &Path, out: &Path, to: Target, name: &str) -> (Restoring, Option<Rows>) {
    let done = node_archive::restore_all(root, out, to, None).unwrap();
    let (_, r) = done
        .into_iter()
        .find(|(n, _)| n == name)
        .expect("the tenant listed");
    let rows = matches!(r, Restoring::Restored(..)).then(|| {
        rows(
            &fenec_core::fs::open(out.join(format!("{name}.fenec"))).unwrap(),
            "get notes",
        )
    });
    (r, rows)
}

/// A tenant made again under its name begins a line of its own, and the
/// old one, moved aside, answers for the time before the new one began: a
/// restore to a moment then took the new line, which began after it, and
/// passed the tenant over as never archived.
#[test]
fn a_restore_to_a_moment_takes_the_line_that_began_by_then() {
    let d = scratch("lines");
    let n = node(&d.join("node"), None);
    let old = made(&n, "e", 10);
    let root = d.join("root");
    let a = archiver(&root, &n.url);
    archived(&root.join("e"), seq(&old));
    let old_rows = rows(&old.db.read().unwrap(), "get notes");
    drop(old);
    let before = a_moment();
    n.tenants.delete("e").unwrap();
    let new = made(&n, "e", 2);
    archived(&root.join("e"), seq(&new));
    let new_rows = rows(&new.db.read().unwrap(), "get notes");
    drop(new);
    a.finish();
    let lines: Vec<String> = node_archive::lines(&root)
        .unwrap()
        .into_iter()
        .map(|l| l.label)
        .collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    let (r, got) = restored_rows(&root, &d.join("then"), Target::Time(before), "e");
    assert!(
        matches!(&r, Restoring::Restored(_, from) if from.starts_with("e.")),
        "{r:?}"
    );
    assert_eq!(got.unwrap(), old_rows);
    let (r, got) = restored_rows(&root, &d.join("now"), Target::End, "e");
    assert!(
        matches!(&r, Restoring::Restored(_, from) if from == "e"),
        "{r:?}"
    );
    assert_eq!(got.unwrap(), new_rows);
}

/// Moved to another node and back, a tenant was not this node's in
/// between: a restore of this node's root to that time leaves it out, where
/// the copy it gave was the one from before it left, beside the other
/// node's.
#[test]
fn a_tenant_away_on_another_node_is_left_out_of_its_time_away() {
    let d = scratch("away");
    let (na, nb) = (node(&d.join("a"), None), node(&d.join("b"), None));
    let (root_a, root_b) = (d.join("root-a"), d.join("root-b"));
    let (aa, ab) = (archiver(&root_a, &na.url), archiver(&root_b, &nb.url));
    let t = made(&na, "m", 5);
    archived(&root_a.join("m"), seq(&t));
    drop(t);
    let r#move = |from: &Node, to: &Node| {
        from.tenants.freeze("m").unwrap();
        let image = from.tenants.export("m").unwrap();
        to.tenants.import("m", &image).unwrap();
        from.tenants.delete("m").unwrap();
    };
    r#move(&na, &nb);
    until("node a's archive was not told the tenant left", || {
        node_archive::gone_at(&root_a.join("m")).is_some()
    });
    let t = nb.tenants.get("m").unwrap();
    exec(&t, "put notes {title: \"on b\", n: 200}");
    archived(&root_b.join("m"), seq(&t));
    drop(t);
    let away = a_moment();
    r#move(&nb, &na);
    until("node a's archive did not take the tenant back", || {
        node_archive::gone_at(&root_a.join("m")).is_none()
    });
    until("node b's archive was not told the tenant left", || {
        node_archive::gone_at(&root_b.join("m")).is_some()
    });
    aa.finish();
    ab.finish();
    let (r, _) = restored_rows(&root_a, &d.join("out-a"), Target::Time(away), "m");
    assert!(matches!(r, Restoring::Gone(_)), "{r:?}");
    let (r, _) = restored_rows(&root_b, &d.join("out-b"), Target::Time(away), "m");
    assert!(matches!(r, Restoring::Restored(..)), "{r:?}");
    // And back: this node's again.
    let (r, _) = restored_rows(&root_a, &d.join("end-a"), Target::End, "m");
    assert!(matches!(r, Restoring::Restored(..)), "{r:?}");
}

/// A moment the archive held and let go of (`--keep`) is refused, not
/// passed over as one before the tenant was archived: a node restored to
/// it would have come back without every tenant older than the window,
/// and said so with exit 0.
#[test]
fn a_moment_let_go_of_is_refused() {
    let d = scratch("let-go");
    let n = node(&d.join("node"), None);
    let t = made(&n, "x", 2);
    let root = d.join("root");
    let a = archiver(&root, &n.url);
    archived(&root.join("x"), seq(&t));
    let early = a_moment();
    exec(&t, "put notes {title: \"later\", n: 3}");
    archived(&root.join("x"), seq(&t));
    drop(t);
    a.finish();
    let arch = Archive::new(root.join("x")).unwrap();
    assert!(arch.consolidate().unwrap().is_some());
    std::thread::sleep(Duration::from_millis(5));
    // Nothing kept but the newest image.
    assert_eq!(arch.prune(0, now_ms()).unwrap().0, 1);
    let (r, _) = restored_rows(&root, &d.join("then"), Target::Time(early), "x");
    assert!(matches!(r, Restoring::LetGo(_)), "{r:?}");
    // Before the archive began, the tenant was not archived: passed over.
    let (r, _) = restored_rows(&root, &d.join("before"), Target::Time(1), "x");
    assert!(matches!(r, Restoring::NotYet), "{r:?}");
}
