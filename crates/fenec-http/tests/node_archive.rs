//! A node's tenants archived over one stream: nodes serving in this
//! process, an archiver of every tenant beside them, and what each tenant's
//! archive restores next to what the node holds -- tenants made, idle and
//! closed, deleted, moved between nodes, made again under a name, and
//! promoted on a standby.

use fenec_core::prelude::*;
use fenec_http::archive::{Archive, Target};
use fenec_http::node_archive::{self, NodeArchive, Restoring};
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
        matches!(done[1], (ref t, Restoring::Restored(_)) if t == "keep"),
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
            .all(|(_, r)| matches!(r, Restoring::Restored(_))),
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
    // Taken back -- its image, and the `gone` file let go of after it --
    // before it is written: a write landing first is in that image, and the
    // archive then has no segment after the gap to show it (macOS CI).
    until("node a's archive still says the tenant is gone", || {
        node_archive::gone_at(&root_a.join("m")).is_none()
    });
    let t = na.tenants.get("m").unwrap();
    exec(&t, "put notes {title: \"back on a\", n: 300}");
    archived(&root_a.join("m"), seq(&t));
    let end = rows(&t.db.read().unwrap(), "get notes");
    drop(t);
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
    assert!(node_archive::gone_at(&root_b.join("m")).is_some());
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
