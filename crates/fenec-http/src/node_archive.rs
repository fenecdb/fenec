//! Every tenant of a node archived by one process, over one stream.
//!
//! `fenec archive --tenants <node> <root>` keeps an archive a tenant under
//! `<root>/<tenant>/`, each the archive `fenec archive` writes for one
//! database -- read by `fenec restore`, `fenec verify` and `fenec prune` as
//! it is, sealed with `--key-file` and given images and let go of with
//! `--image-every` and `--keep` each on its own. It speaks the node's stream
//! of every tenant ([`crate::node_stream`]): one connection and one thread
//! on the node however many tenants it holds, where a tenant's own stream
//! each was a connection and a thread on both sides and held every tenant
//! open. A tenant made on the node is archived from its first image on, and
//! one that leaves it -- deleted, or moved to another node -- is no longer
//! followed: its archive stays as it is, and the node's word that it is gone
//! is kept beside it, and that it is back where it comes back (`presence`, a
//! line each, with its time), which a restore of every tenant reads: a node
//! restored to a moment holds the tenants it held then, not one deleted
//! before, nor a stale copy of one that lived on another node meanwhile.
//!
//! A tenant whose copy on the node no longer leads from its archive's place
//! -- restored there to an earlier moment, or another tenant made under the
//! name -- would land its writes among changes of the same numbers. Its
//! archive so far is moved aside, to `<root>/<tenant>.<history, hex>/`, and a
//! new one begins with the node's image: neither line of history is lost. A
//! restore to a moment takes the line that began last by then, so the old
//! line answers for the time before the new began; `verify`, `prune` and the
//! images taken here see every line. A single database's archive refuses
//! such a primary instead ([`crate::archive::Took::Diverged`]).
//!
//! ```text
//! <root>/archiving.lock          held while an archiver writes the root
//! <root>/<tenant>/               a tenant's archive, as `fenec archive` writes one
//! <root>/<tenant>/began          when its first image was taken: prune keeps it
//! <root>/<tenant>/presence       `gone <ms>` and `back <ms>`, by the node's clock
//! <root>/<tenant>.<hex>/         a line moved aside, its history's id
//! ```

use crate::archive::{Archive, Took, Writer};
use crate::replication::{self, read_head, read_message, u64_at, Upstream, MESSAGE};
use crate::seal::Key;
use crate::tenants::check_name;
use fenec_core::history::History;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// One line of a tenant's history archived under a root: `<root>/<tenant>/`,
/// the one followed, or `<root>/<tenant>.<history>/`, one moved aside.
#[derive(Debug, Clone)]
pub struct Line {
    pub tenant: String,
    pub dir: PathBuf,
    /// Its directory's name: the tenant's, or the tenant's and its history.
    pub label: String,
    pub current: bool,
}

/// Every line archived under a root, by tenant and then by name.
pub fn lines(root: &Path) -> io::Result<Vec<Line>> {
    let mut out = Vec::new();
    for e in fs::read_dir(root)? {
        let e = e?;
        if !e.file_type()?.is_dir() {
            continue;
        }
        let label = e.file_name().to_string_lossy().into_owned();
        let (tenant, current) = match label.split_once('.') {
            None => (label.clone(), true),
            Some((t, rest)) if !rest.is_empty() => (t.to_string(), false),
            Some(_) => continue,
        };
        if check_name(&tenant).is_ok() {
            out.push(Line {
                tenant,
                dir: e.path(),
                label,
                current,
            });
        }
    }
    out.sort_by(|a, b| (&a.tenant, !a.current, &a.label).cmp(&(&b.tenant, !b.current, &b.label)));
    Ok(out)
}

/// The tenants archived under a root, by name: the lines followed.
pub fn tenant_dirs(root: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    Ok(lines(root)?
        .into_iter()
        .filter(|l| l.current)
        .map(|l| (l.tenant, l.dir))
        .collect())
}

/// What the node said of the tenant archived in `dir`, oldest first: gone
/// (`true`) or back, and when, ms since the epoch.
fn presence(dir: &Path) -> Vec<(bool, u64)> {
    let Ok(text) = fs::read_to_string(dir.join("presence")) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| {
            let (what, at) = l.trim().split_once(' ')?;
            Some((what == "gone", at.parse().ok()?))
        })
        .collect()
}

/// When the node said the tenant archived in `dir` was gone from it -- ms
/// since the epoch -- if it did and has not held it again since.
pub fn gone_at(dir: &Path) -> Option<u64> {
    presence(dir).last().filter(|e| e.0).map(|e| e.1)
}

/// Whether the node held the tenant archived in `dir` at `at`: `None` where
/// it did, the time it was gone from where it did not -- by the last line
/// noted by then. The lines are in the order the node said them; their
/// times are its clock's, which a clock set back leaves out of order, so a
/// line later than `at` stops nothing: read up to the first, a tenant gone,
/// back, gone and back again across a clock set back read as gone.
pub fn away_at(dir: &Path, at: u64) -> Option<u64> {
    presence(dir)
        .into_iter()
        .rfind(|e| e.1 <= at)
        .filter(|e| e.0)
        .map(|e| e.1)
}

/// Notes the node said the tenant archived in `dir` was gone, or back, at
/// `at` -- the node's time, as its writes' are.
fn note(dir: &Path, gone: bool, at: u64) -> io::Result<()> {
    let mut text = fs::read_to_string(dir.join("presence")).unwrap_or_default();
    text.push_str(&format!("{} {at}\n", if gone { "gone" } else { "back" }));
    put(&dir.join("presence"), text.as_bytes())?;
    // The rename durable too: lost in a crash, a deleted tenant came back
    // at a restore.
    #[cfg(unix)]
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// A tenant's archive, and the lock the images taken beside the stream hold
/// while they read and write it, which moving it aside waits for.
struct Kept {
    archive: Archive,
    keeping: Mutex<()>,
    /// Moved aside: an image or a prune must not touch the directory its
    /// path now names, a new archive's.
    moved: AtomicBool,
}

/// Every tenant of a node, archived under one root.
pub struct NodeArchive {
    root: PathBuf,
    key: Option<Key>,
    tenants: Mutex<BTreeMap<String, Arc<Kept>>>,
    /// Two archivers writing one root would write each other's segments.
    _lock: fenec_abi::lock::FileLock,
}

/// What the node's stream sends, decoded.
enum NodeMessage {
    Node,
    Hello {
        name: String,
        seq: u64,
        /// The node's time as it said hello.
        at: u64,
        lineage: Vec<(u64, u64)>,
        image: Option<Vec<u8>>,
    },
    Writes {
        name: String,
        first: u64,
        times: Vec<u64>,
        records: Vec<u8>,
    },
    Lineage {
        name: String,
        lineage: Vec<(u64, u64)>,
    },
    /// The tenant, and when it went by the node's clock; a node from
    /// before the time sends none.
    Gone(String, Option<u64>),
    Alive,
    End(String),
}

impl NodeArchive {
    /// The archives under `root`, made when missing, its files sealed with
    /// `key` or plain.
    pub fn new(root: impl AsRef<Path>, key: Option<Key>) -> io::Result<NodeArchive> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        let held = fenec_abi::lock::hold(root.join("archiving"))
            .map_err(|e| io::Error::other(format!("{}: {e}", root.display())))?;
        let mut tenants = BTreeMap::new();
        for (name, dir) in tenant_dirs(&root)? {
            tenants.insert(name, Arc::new(Kept::new(&dir, key.clone())?));
        }
        Ok(NodeArchive {
            root,
            key,
            tenants: Mutex::new(tenants),
            _lock: held,
        })
    }

    /// The tenants it holds an archive of.
    pub fn tenants(&self) -> Vec<String> {
        lock(&self.tenants).keys().cloned().collect()
    }

    fn kept(&self, name: &str) -> io::Result<Arc<Kept>> {
        let mut all = lock(&self.tenants);
        if let Some(k) = all.get(name) {
            return Ok(Arc::clone(k));
        }
        let k = Arc::new(Kept::new(&self.root.join(name), self.key.clone())?);
        all.insert(name.into(), Arc::clone(&k));
        Ok(k)
    }

    /// Follows the node behind `upstream` into the archives until `stop` is
    /// set, connecting again whenever the stream ends. `report` hears each
    /// thing worth telling an operator.
    pub fn follow(
        &self,
        upstream: &Upstream,
        stop: &AtomicBool,
        report: &dyn Fn(&str),
    ) -> io::Result<()> {
        let mut pause = Duration::from_millis(100);
        while !stop.load(Ordering::SeqCst) {
            match self.session(upstream, stop, report) {
                Ok(()) => pause = Duration::from_millis(100),
                Err(e) => {
                    report(&format!("{e}; connecting again"));
                    pause = (pause * 2).min(Duration::from_secs(5));
                }
            }
            let until = Instant::now() + pause;
            while Instant::now() < until && !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(())
    }

    fn session(
        &self,
        upstream: &Upstream,
        stop: &AtomicBool,
        report: &dyn Fn(&str),
    ) -> io::Result<()> {
        // Where each archive stands, its file closed until it is written.
        let mut writers: HashMap<String, Writer> = HashMap::new();
        // Archives that do not read -- a file of another kind, one cut
        // inside: left as they are, for an operator, and the rest go on.
        let mut broken: BTreeSet<String> = BTreeSet::new();
        let mut body = String::new();
        let all: Vec<(String, Arc<Kept>)> = lock(&self.tenants)
            .iter()
            .map(|(n, k)| (n.clone(), Arc::clone(k)))
            .collect();
        for (name, kept) in all {
            match kept.archive.writer().and_then(|mut w| w.park().map(|_| w)) {
                Ok(w) => {
                    let (at, history) = w.place();
                    body.push_str(&format!(
                        "{name} {at} {history:016x} {}\n",
                        w.wants_image() as u8
                    ));
                    writers.insert(name, w);
                }
                Err(e) => {
                    report(&format!(
                        "tenant `{name}`: its archive does not read, left as it is: {e}"
                    ));
                    broken.insert(name);
                }
            }
        }
        let mut rx = open(upstream, &body)?;
        if !matches!(rx.receive()?, NodeMessage::Node) {
            return Err(io::Error::other("the node did not say hello"));
        }
        report(&format!(
            "following the node's {} archived tenants and every one it holds",
            writers.len()
        ));
        // Tenants with writes not yet synced: each is synced once its
        // oldest has waited the archive's sync interval, and its file closed.
        let mut dirty: BTreeSet<String> = BTreeSet::new();
        let (mut taken, mut told) = (0u64, Instant::now());
        loop {
            if stop.load(Ordering::SeqCst) {
                for name in &dirty {
                    if let Some(w) = writers.get_mut(name) {
                        w.park()?;
                    }
                }
                return Ok(());
            }
            match rx.receive()? {
                NodeMessage::Hello {
                    name,
                    seq,
                    at,
                    lineage,
                    image,
                } if !broken.contains(&name) => {
                    // An archive that does not read is left for an operator;
                    // a disk that refused a write is tried again, as a
                    // write's failure is, from where the archives stand.
                    let given = (seq, at, lineage);
                    match self.hello(&mut writers, &name, given, image.as_deref(), report) {
                        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                            report(&format!("tenant `{name}`: {e}; left as it is"));
                            writers.remove(&name);
                            broken.insert(name);
                        }
                        r => r?,
                    }
                }
                NodeMessage::Writes {
                    name,
                    first,
                    times,
                    records,
                } if !broken.contains(&name) => {
                    let Some(w) = writers.get_mut(&name) else {
                        return Err(io::Error::other(format!(
                            "the node sent tenant `{name}`'s writes before its hello"
                        )));
                    };
                    self.kept(&name)?.archive.take(w, first, &times, &records)?;
                    taken += w.taken();
                    dirty.insert(name);
                }
                NodeMessage::Lineage { name, lineage } if !broken.contains(&name) => {
                    if let Some(w) = writers.get_mut(&name) {
                        self.kept(&name)?.archive.lineage(w, lineage)?;
                    }
                }
                NodeMessage::Gone(name, at) => {
                    if let Some(mut w) = writers.remove(&name) {
                        w.park()?;
                    }
                    dirty.remove(&name);
                    let dir = self.root.join(&name);
                    if dir.is_dir() && gone_at(&dir).is_none() {
                        note(&dir, true, at.unwrap_or_else(now_ms))?;
                        report(&format!(
                            "tenant `{name}` is no longer on the node: its archive is kept, \
                             and no longer followed"
                        ));
                    }
                }
                NodeMessage::Alive => {
                    for name in std::mem::take(&mut dirty) {
                        if let Some(w) = writers.get_mut(&name) {
                            w.park()?;
                        }
                    }
                }
                NodeMessage::End(why) => return Err(io::Error::other(why)),
                NodeMessage::Node => return Err(io::Error::other("the node said hello twice")),
                // A broken tenant's: passed over.
                _ => {}
            }
            let due: Vec<String> = dirty
                .iter()
                .filter(|n| writers.get(*n).is_some_and(|w| w.due()))
                .cloned()
                .collect();
            for name in due {
                if let Some(w) = writers.get_mut(&name) {
                    w.park()?;
                }
                dirty.remove(&name);
            }
            if told.elapsed() >= Duration::from_secs(10) {
                if taken > 0 {
                    report(&format!(
                        "{taken} writes archived across the node's tenants"
                    ));
                    taken = 0;
                }
                told = Instant::now();
            }
        }
    }

    /// A tenant's hello: its archive goes on from where it says, or takes
    /// the image it sends -- in an archive of its own where the image is of
    /// a history that does not lead from the one it has.
    fn hello(
        &self,
        writers: &mut HashMap<String, Writer>,
        name: &str,
        (seq, said, lineage): (u64, u64, Vec<(u64, u64)>),
        image: Option<&[u8]>,
        report: &dyn Fn(&str),
    ) -> io::Result<()> {
        let kept = self.kept(name)?;
        if !writers.contains_key(name) {
            writers.insert(name.into(), kept.archive.writer()?);
        }
        let w = writers.get_mut(name).expect("inserted above");
        let took = kept.archive.hello(w, image, seq, lineage.clone())?;
        let kept = match took {
            Took::Went => kept,
            Took::Diverged => {
                let (at, history) = w.place();
                let aside = self.move_aside(name, &kept, history)?;
                // The copy it held is gone, if the node did not say so --
                // an archiver away as the tenant was deleted and made again
                // hears of the new one alone: by the time the node said
                // hello to it at the latest, so a restore to a moment after
                // leaves the old one out.
                if gone_at(&aside).is_none() {
                    note(&aside, true, said)?;
                }
                report(&format!(
                    "tenant `{name}`: the node's copy is at change {seq} on a history that \
                     does not lead from the archive's change {at} -- restored to an earlier \
                     moment, or another tenant of the name. The archive so far is kept as \
                     {}, and a new one begins",
                    aside.display()
                ));
                let fresh = self.kept(name)?;
                let mut w = fresh.archive.writer()?;
                if fresh.archive.hello(&mut w, image, seq, lineage)? != Took::Went {
                    return Err(io::Error::other("a new archive refused its first image"));
                }
                writers.insert(name.into(), w);
                fresh
            }
        };
        // Held again: followed, and back from where it was gone -- the time
        // away kept, so a restore to a moment in it leaves the tenant out.
        let dir = self.root.join(name);
        if gone_at(&dir).is_some() {
            note(&dir, false, said)?;
        }
        if let Some(image) = image {
            report(&format!(
                "tenant `{name}`: image at change {seq} ({:.1} MB)",
                image.len() as f64 / 1e6
            ));
        }
        drop(kept);
        Ok(())
    }

    /// Moves a tenant's archive aside, to `<root>/<tenant>.<history hex>`,
    /// and puts a new one in its place: where it went.
    fn move_aside(&self, name: &str, kept: &Arc<Kept>, history: u64) -> io::Result<PathBuf> {
        let _held = lock(&kept.keeping);
        kept.moved.store(true, Ordering::SeqCst);
        let dir = self.root.join(name);
        let mut aside = self.root.join(format!("{name}.{history:016x}"));
        let mut n = 1;
        while aside.exists() {
            n += 1;
            aside = self.root.join(format!("{name}.{history:016x}-{n}"));
        }
        fs::rename(&dir, &aside)?;
        let fresh = Arc::new(Kept::new(&dir, self.key.clone())?);
        lock(&self.tenants).insert(name.into(), fresh);
        Ok(aside)
    }

    /// Beside `follow`: an image of each tenant's archive's end every
    /// `every` once a write came after its last, and what a restore within
    /// `keep` does not need let go after it, until `stop` is set -- what
    /// `fenec archive` does for its one database, a tenant at a time.
    pub fn keep_up(
        &self,
        every: Duration,
        keep: Option<Duration>,
        stop: &AtomicBool,
        report: &dyn Fn(&str),
    ) -> io::Result<()> {
        let mut last = Instant::now();
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(200));
            if last.elapsed() < every {
                continue;
            }
            last = Instant::now();
            self.keep_once(keep, stop, report);
        }
        Ok(())
    }

    /// One pass of [`Self::keep_up`] over every tenant.
    pub fn keep_once(&self, keep: Option<Duration>, stop: &AtomicBool, report: &dyn Fn(&str)) {
        let all: Vec<(String, Arc<Kept>)> = lock(&self.tenants)
            .iter()
            .map(|(n, k)| (n.clone(), Arc::clone(k)))
            .collect();
        for (name, kept) in all {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let _held = lock(&kept.keeping);
            if kept.moved.load(Ordering::SeqCst) {
                continue;
            }
            if kept.archive.take_fresh() {
                match kept.archive.consolidate() {
                    Ok(Some(seq)) => report(&format!(
                        "tenant `{name}`: image at change {seq}, taken here"
                    )),
                    Ok(None) => {}
                    Err(e) => {
                        kept.archive.mark_fresh();
                        report(&format!("tenant `{name}`: no image taken: {e}"));
                    }
                }
            }
            if let Some(keep) = keep {
                match kept.archive.prune(keep.as_millis() as u64, now_ms()) {
                    Ok((0, 0)) => {}
                    Ok((i, s)) => report(&format!(
                        "tenant `{name}`: let go of {i} images and {s} segments"
                    )),
                    Err(e) => report(&format!("tenant `{name}`: nothing let go: {e}")),
                }
            }
        }
        // The lines moved aside take no writes, and are let go of as the
        // window says as well: left out, they were kept for good.
        let Some(keep) = keep else {
            return;
        };
        for line in lines(&self.root).unwrap_or_default() {
            if line.current || stop.load(Ordering::SeqCst) {
                continue;
            }
            let pruned = Archive::with_key(&line.dir, self.key.clone())
                .and_then(|a| a.prune(keep.as_millis() as u64, now_ms()));
            match pruned {
                Ok((0, 0)) => {}
                Ok((i, s)) => report(&format!(
                    "{}: let go of {i} images and {s} segments",
                    line.label
                )),
                Err(e) => report(&format!("{}: nothing let go: {e}", line.label)),
            }
        }
    }
}

impl Kept {
    fn new(dir: &Path, key: Option<Key>) -> io::Result<Kept> {
        Ok(Kept {
            archive: Archive::with_key(dir, key)?,
            keeping: Mutex::new(()),
            moved: AtomicBool::new(false),
        })
    }
}

/// Writes `bytes` under `path` whole: a side file, fsynced, renamed over.
fn put(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(&tmp, path)
}

/// The node's stream, asked for from where the archives stand.
fn open(upstream: &Upstream, places: &str) -> io::Result<Receiver> {
    let s = replication::connect(&upstream.addr)?;
    s.set_read_timeout(Some(replication::SILENCE))?;
    s.set_write_timeout(Some(replication::SILENCE))?;
    let request = format!(
        "POST /_replication/tenants HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\n\
         Content-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
        upstream.addr,
        upstream.token,
        places.len()
    );
    (&s).write_all(request.as_bytes())?;
    (&s).write_all(places.as_bytes())?;
    let mut r = BufReader::with_capacity(MESSAGE + 64, s);
    let (status, length) = read_head(&mut r)?;
    if status != 200 {
        let mut body = vec![0u8; length.unwrap_or(0).min(64 << 10)];
        r.read_exact(&mut body)?;
        return Err(io::Error::other(format!(
            "the node answered {status}: {}",
            String::from_utf8_lossy(&body)
        )));
    }
    Ok(Receiver { r })
}

struct Receiver {
    r: BufReader<TcpStream>,
}

impl Receiver {
    fn receive(&mut self) -> io::Result<NodeMessage> {
        let bad = || io::Error::other("the node sent a message it should not");
        let (tag, body) = read_message(&mut self.r)?;
        let named = |body: &[u8]| -> io::Result<(String, usize)> {
            let n = *body.first().ok_or_else(bad)? as usize;
            let name = body.get(1..1 + n).ok_or_else(bad)?;
            let name = std::str::from_utf8(name).map_err(|_| bad())?;
            check_name(name).map_err(|_| bad())?;
            Ok((name.to_string(), 1 + n))
        };
        Ok(match tag {
            b'N' => {
                if body.first() != Some(&replication::VERSION) {
                    return Err(io::Error::other("the node speaks another protocol"));
                }
                NodeMessage::Node
            }
            b'H' => {
                let (name, at) = named(&body)?;
                let h = &body[at..];
                if h.len() < 26 || h[0] != replication::VERSION {
                    return Err(io::Error::other("the node speaks another protocol"));
                }
                let lineage = History::decode(&h[26..])
                    .map_err(|e| io::Error::other(e.to_string()))?
                    .lineage;
                let seq = u64_at(h, 2);
                let at = u64_at(h, 18);
                let image = match h[1] {
                    1 => {
                        let (tag, body) = read_message(&mut self.r)?;
                        let (of, at) = named(&body)?;
                        if tag != b'I' || of != name {
                            return Err(io::Error::other("the node promised an image"));
                        }
                        let mut body = body;
                        body.drain(..at);
                        Some(body)
                    }
                    _ => None,
                };
                NodeMessage::Hello {
                    name,
                    seq,
                    at,
                    lineage,
                    image,
                }
            }
            b'W' => {
                let (name, at) = named(&body)?;
                let w = &body[at..];
                let n = w.get(8..16).ok_or_else(bad)?;
                let n = u64::from_le_bytes(n.try_into().unwrap()) as usize;
                let records_at = n
                    .checked_mul(8)
                    .and_then(|b| b.checked_add(16))
                    .ok_or_else(bad)?;
                if w.len() < records_at {
                    return Err(bad());
                }
                NodeMessage::Writes {
                    name,
                    first: u64_at(w, 0),
                    times: (0..n).map(|i| u64_at(w, 16 + 8 * i)).collect(),
                    records: w[records_at..].to_vec(),
                }
            }
            b'L' => {
                let (name, at) = named(&body)?;
                let lineage = History::decode(&body[at..])
                    .map_err(|e| io::Error::other(e.to_string()))?
                    .lineage;
                NodeMessage::Lineage { name, lineage }
            }
            b'G' => {
                let (name, at) = named(&body)?;
                let when = body.get(at..at + 8).map(|_| u64_at(&body, at));
                NodeMessage::Gone(name, when)
            }
            b'K' => NodeMessage::Alive,
            b'E' => NodeMessage::End(String::from_utf8_lossy(&body).into_owned()),
            _ => return Err(bad()),
        })
    }
}

/// The node behind `url`: `http://host:port`, nothing after it -- the
/// stream of every tenant hangs off the node, not a tenant.
pub fn node_upstream(url: &str, token: String) -> Result<Upstream, String> {
    Upstream::check_node(url).map_err(|_| {
        format!(
            "a node is http://host:port, not `{url}`: its tenants are archived over its own stream"
        )
    })?;
    Upstream::new(url, token)
}

/// What a restore of every tenant did with one.
#[derive(Debug)]
pub enum Restoring {
    /// Restored, to this change, from this line (its directory's name).
    Restored(u64, String),
    /// Passed over: the node said it was gone then, from this time.
    Gone(u64),
    /// Passed over: its archive began after the moment, so it was not
    /// archived then.
    NotYet,
    /// Refused: its archive began by then, but what it held of the moment
    /// was let go of (`--keep`); its oldest image kept is from this time.
    LetGo(u64),
    Failed(String),
}

/// Every tenant archived under `root` as it stood at `to` -- the end, or a
/// moment -- into `<out>/<tenant>.fenec`, a directory a node serves: the
/// tenants the node held then, each from the line of its history that began
/// last by then. A tenant the node said was gone then -- deleted, or living
/// on another node -- is passed over, and so is one archived only after it;
/// one whose moment the archive let go of is refused, never passed over. A
/// file there already is not written over. A change names one database's,
/// not a node's, and is refused.
pub fn restore_all(
    root: &Path,
    out: &Path,
    to: crate::archive::Target,
    key: Option<&Key>,
) -> io::Result<Vec<(String, Restoring)>> {
    use crate::archive::Target;
    if let Target::Change(_) = to {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a change names one database's: a node's tenants are restored to a moment",
        ));
    }
    fs::create_dir_all(out)?;
    let mut by_tenant: BTreeMap<String, Vec<Line>> = BTreeMap::new();
    for line in lines(root)? {
        by_tenant.entry(line.tenant.clone()).or_default().push(line);
    }
    let mut done = Vec::new();
    for (name, lines) in by_tenant {
        let file = out.join(format!("{name}.fenec"));
        let outcome = match to {
            Target::Time(at) => restore_at(&lines, &file, at, key),
            _ => match lines.iter().find(|l| l.current) {
                Some(l) => match gone_at(&l.dir) {
                    Some(t) => Restoring::Gone(t),
                    None => restore_line(l, &file, to, key),
                },
                None => {
                    Restoring::Failed("no line of it is followed, only lines moved aside".into())
                }
            },
        };
        done.push((name, outcome));
    }
    Ok(done)
}

/// A tenant at moment `at`: the line that began last by then.
fn restore_at(lines: &[Line], file: &Path, at: u64, key: Option<&Key>) -> Restoring {
    let mut chosen: Option<(u64, &Line)> = None;
    for line in lines {
        let began = Archive::with_key(&line.dir, key.cloned()).and_then(|a| a.began());
        match began {
            Ok(Some(b)) if b <= at && chosen.is_none_or(|c| b > c.0) => chosen = Some((b, line)),
            Ok(_) => {}
            Err(e) => return Restoring::Failed(format!("{}: {e}", line.label)),
        }
    }
    let Some((_, line)) = chosen else {
        return Restoring::NotYet;
    };
    if let Some(t) = away_at(&line.dir, at) {
        return Restoring::Gone(t);
    }
    let first = Archive::with_key(&line.dir, key.cloned()).and_then(|a| a.first_image_time());
    match first {
        Ok(Some(f)) if f > at => Restoring::LetGo(f),
        Ok(_) => restore_line(line, file, crate::archive::Target::Time(at), key),
        Err(e) => Restoring::Failed(format!("{}: {e}", line.label)),
    }
}

fn restore_line(
    line: &Line,
    file: &Path,
    to: crate::archive::Target,
    key: Option<&Key>,
) -> Restoring {
    // Under the file's lock, asked and renamed in place, as a node installs
    // a tenant: a node serving `out` that made the name meanwhile, or a
    // shell over it, would have the file renamed from under it.
    let _held = match fenec_abi::lock::hold(file) {
        Ok(held) => held,
        Err(e) => return Restoring::Failed(format!("{}: {e}", file.display())),
    };
    if file.exists() {
        return Restoring::Failed(format!(
            "{} is there already: restore into an empty directory",
            file.display()
        ));
    }
    match Archive::with_key(&line.dir, key.cloned()).and_then(|a| a.restore(file, to)) {
        Ok(r) => Restoring::Restored(r.seq, line.label.clone()),
        Err(e) => Restoring::Failed(format!("{}: {e}", line.label)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tenant gone and back twice, the node's clock set back between: the
    /// times out of order, the last line noted by a moment decides.
    #[test]
    fn presence_is_read_past_a_clock_set_back() {
        let dir = std::env::temp_dir().join(format!(
            "fenec-presence-{}-{:?}",
            std::process::id(),
            Instant::now()
        ));
        fs::create_dir_all(&dir).unwrap();
        for (gone, at) in [(true, 100), (false, 300), (true, 250), (false, 260)] {
            note(&dir, gone, at).unwrap();
        }
        assert_eq!(away_at(&dir, 50), None);
        assert_eq!(away_at(&dir, 150), Some(100));
        assert_eq!(away_at(&dir, 255), Some(250));
        // Back by then: read up to the first line past it, gone.
        assert_eq!(away_at(&dir, 270), None);
        assert_eq!(away_at(&dir, 400), None);
        assert_eq!(gone_at(&dir), None);
        let _ = fs::remove_dir_all(dir);
    }
}
