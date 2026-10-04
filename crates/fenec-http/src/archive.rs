//! Backups, and restoring to a moment.
//!
//! An archive is the replication stream written to a directory instead of
//! applied: base images, and after them every write with the time the
//! primary appended it. A restore takes the latest image at or before the
//! point asked for and appends the writes after it up to that point -- a
//! fenecdb file is an image and the writes after it, so that is the whole
//! restore -- then forks the result's history, since it leaves the primary's
//! history at that point (see [`fenec_core::history`]).
//!
//! The archive is fed like a replica, so it holds only writes that were on
//! the primary's disk, and a `compact` on the primary loses it nothing: the
//! writes were archived as they reached the disk, before any compact folded
//! them into an image.
//!
//! ```text
//! image-<seq>-<time>.fenec   a base image: the database at change <seq>,
//!                            taken at <time> (ms since the epoch)
//! writes-<first>.log         "FENECLG\x01", then per write [time u64][record],
//!                            numbered on from <first>
//! history                    the history the archive is on
//! ```
//!
//! With a key (`fenec archive --key-file`) every one of them is sealed
//! (`seal.rs`): an image and the history whole, a segment a frame for each
//! batch of writes the primary sent, so a crash cuts off at most the frame
//! it was writing, as it cut a record short before. An archive is sealed or
//! it is not: a file of the other kind in it is refused, rather than a key
//! given for nothing or a sealed file read as garbage.

use crate::replication::{fresh_id, Message, Upstream};
use crate::seal::{self, Key};
use fenec_core::codec::get_uvarint;
use fenec_core::history::History;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A segment is closed at this size and the next begins.
const SEGMENT: usize = 64 << 20;

const LOG_MAGIC: &[u8; 8] = b"FENECLG\x01";

/// How often the archive's own writes are fsynced. A write archived and lost
/// to a crash of the archiver is sent again: it resumes from what its disk
/// holds.
const SYNC_EVERY: Duration = Duration::from_millis(250);

/// Where a restore stops.
#[derive(Clone, Copy, Debug)]
pub enum Target {
    /// Everything archived.
    End,
    /// Up to and including this change.
    Change(u64),
    /// Up to the last write appended at or before this time, ms since the
    /// epoch.
    Time(u64),
}

/// What a restore put in its file.
#[derive(Debug)]
pub struct Restored {
    /// The change the file ends at, the image it started from, and when the
    /// last write it holds was appended (none when it holds only the image).
    pub seq: u64,
    pub image: u64,
    pub time: Option<u64>,
}

/// What `verify` found an archive holds, whole.
#[derive(Debug)]
pub struct Verified {
    pub images: usize,
    pub segments: usize,
    /// The change the oldest image is at and when it was taken: nothing
    /// before it can be restored.
    pub first: u64,
    pub from: u64,
    /// The last change, and when it was appended (none when only images).
    pub last: u64,
    pub to: Option<u64>,
    /// Bytes of a last record cut short at the end of the last segment --
    /// an archiver stopped mid-write, or a copy taken while it wrote --
    /// which the next archive run cuts off and a restore passes over.
    pub torn: usize,
    /// Changes no segment holds between two images, first to last: a
    /// restore to the end passes over them from the later image, but not
    /// to a moment inside them. An archiver that fell behind the primary's
    /// feed was sent an image and left one; a segment gone leaves one too.
    pub gaps: Vec<(u64, u64)>,
}

pub struct Archive {
    dir: PathBuf,
    /// The key its files are sealed with, if they are.
    key: Option<Key>,
    /// Set once an image was taken here: the segment being written ends
    /// at its next write, so the writes before the image are in segments of
    /// their own, which `prune` can let go of whole -- in the segment going
    /// on, they stayed until it reached its 64 MB.
    roll: AtomicBool,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn corrupt(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// A file of the other kind than the archive: a sealed one with no key, or
/// a plain one where the archive is sealed.
fn mixed(path: &Path, sealed: bool) -> io::Error {
    corrupt(match sealed {
        true => format!(
            "{}: sealed; give the key it was sealed with (--key-file)",
            path.display()
        ),
        false => format!("{}: not sealed, where the archive is", path.display()),
    })
}

/// A write's record, whole, from the front of `bytes`: its length, or
/// `None` when it is cut short.
fn record_len(bytes: &[u8]) -> Option<usize> {
    let mut pos = 1;
    get_uvarint(bytes, &mut pos).ok()?;
    let len = get_uvarint(bytes, &mut pos).ok()? as usize;
    (bytes.len() >= pos + len && !bytes.is_empty()).then_some(pos + len)
}

/// `(time, start, end, writes)` of each record in a segment's bytes: a
/// statement's writes are one record, a block's too, numbered on from the
/// write before it.
type Entries = Vec<(u64, usize, usize, u64)>;

/// The writes a record holds -- one, when it cannot say.
fn writes(record: &[u8]) -> u64 {
    fenec_core::engine::writes_in(record).unwrap_or(1).max(1)
}

/// A segment's records, and where the last whole one ends -- a crash can
/// leave a torn one after it.
fn entries(data: &[u8]) -> io::Result<(Entries, usize)> {
    if data.len() < LOG_MAGIC.len() || &data[..LOG_MAGIC.len()] != LOG_MAGIC {
        return Err(corrupt("not an archive segment".into()));
    }
    let mut out = Vec::new();
    let mut pos = LOG_MAGIC.len();
    while pos + 8 < data.len() {
        let time = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
        let Some(len) = record_len(&data[pos + 8..]) else {
            break;
        };
        let n = writes(&data[pos + 8..pos + 8 + len]);
        out.push((time, pos + 8, pos + 8 + len, n));
        pos += 8 + len;
    }
    Ok((out, pos))
}

/// The segment being written.
struct Segment {
    file: File,
    /// The number the next write gets, and the bytes so far.
    next: u64,
    len: usize,
    dirty: bool,
    synced: Instant,
    /// Sealed: the key, the next frame's index and the writes waiting to be
    /// sealed as one frame of whole records.
    sealed: Option<(Key, u64, Vec<u8>)>,
}

impl Segment {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.len += bytes.len();
        self.dirty = true;
        match &mut self.sealed {
            Some((_, _, pending)) => {
                pending.extend_from_slice(bytes);
                Ok(())
            }
            None => self.file.write_all(bytes),
        }
    }

    /// Seals what waits as a frame, and writes it.
    fn flush(&mut self) -> io::Result<()> {
        if let Some((key, index, pending)) = &mut self.sealed {
            if !pending.is_empty() {
                let mut frame = Vec::with_capacity(pending.len() + 32);
                seal::seal_frame(key, *index, false, pending, &mut frame);
                self.file.write_all(&frame)?;
                *index += 1;
                pending.clear();
            }
        }
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.flush()?;
        if self.dirty {
            self.file.sync_data()?;
            self.dirty = false;
        }
        self.synced = Instant::now();
        Ok(())
    }
}

/// A segment's writes, and for a sealed one where its last whole frame
/// ends and how many frames it holds.
struct Read {
    plain: Vec<u8>,
    sealed: Option<(usize, u64)>,
    /// The file's own length.
    len: usize,
}

impl Archive {
    pub fn new(dir: impl AsRef<Path>) -> io::Result<Archive> {
        Archive::with_key(dir, None)
    }

    /// An archive whose files are sealed with `key`, or plain without one.
    pub fn with_key(dir: impl AsRef<Path>, key: Option<Key>) -> io::Result<Archive> {
        fs::create_dir_all(dir.as_ref())?;
        Ok(Archive {
            dir: dir.as_ref().to_path_buf(),
            key,
            roll: AtomicBool::new(false),
        })
    }

    /// A file of the kind this archive is, as it was written.
    fn read_whole(&self, path: &Path) -> io::Result<Vec<u8>> {
        let bytes = fs::read(path)?;
        match (seal::is_sealed(&bytes), &self.key) {
            (true, Some(key)) => seal::open_whole(key, &bytes)
                .map_err(|e| corrupt(format!("{}: {e}", path.display()))),
            (false, None) => Ok(bytes),
            (sealed, _) => Err(mixed(path, sealed)),
        }
    }

    fn read_segment(&self, path: &Path) -> io::Result<Read> {
        let bytes = fs::read(path)?;
        let len = bytes.len();
        match (seal::is_sealed(&bytes), &self.key) {
            (true, Some(key)) => {
                let (plain, end, frames) = seal::open_appended(key, &bytes)
                    .map_err(|e| corrupt(format!("{}: {e}", path.display())))?;
                Ok(Read {
                    plain,
                    sealed: Some((end, frames)),
                    len,
                })
            }
            (false, None) => Ok(Read {
                plain: bytes,
                sealed: None,
                len,
            }),
            (sealed, _) => Err(mixed(path, sealed)),
        }
    }

    /// `(seq, time)` of each base image, oldest first.
    fn images(&self) -> io::Result<Vec<(u64, u64)>> {
        let mut out = Vec::new();
        for e in fs::read_dir(&self.dir)? {
            let name = e?.file_name().to_string_lossy().into_owned();
            let Some(rest) = name
                .strip_prefix("image-")
                .and_then(|r| r.strip_suffix(".fenec"))
            else {
                continue;
            };
            if let Some((seq, time)) = rest.split_once('-') {
                if let (Ok(seq), Ok(time)) = (seq.parse(), time.parse()) {
                    out.push((seq, time));
                }
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    /// The first change of each segment, oldest first.
    fn segments(&self) -> io::Result<Vec<u64>> {
        let mut out = Vec::new();
        for e in fs::read_dir(&self.dir)? {
            let name = e?.file_name().to_string_lossy().into_owned();
            if let Some(first) = name
                .strip_prefix("writes-")
                .and_then(|r| r.strip_suffix(".log"))
                .and_then(|r| r.parse().ok())
            {
                out.push(first);
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    fn image_path(&self, seq: u64, time: u64) -> PathBuf {
        self.dir.join(format!("image-{seq:020}-{time:013}.fenec"))
    }

    fn segment_path(&self, first: u64) -> PathBuf {
        self.dir.join(format!("writes-{first:020}.log"))
    }

    /// Writes `bytes` under `path` whole or not at all: a side file,
    /// fsynced, then renamed over.
    fn put(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let tmp = path.with_extension("tmp");
        let mut f = File::create(&tmp)?;
        match &self.key {
            Some(key) => f.write_all(&seal::seal_whole(key, bytes))?,
            None => f.write_all(bytes)?,
        }
        f.sync_all()?;
        fs::rename(&tmp, path)
    }

    /// Adds a base image of the database at change `seq`. Taking one now and
    /// then keeps a restore from replaying every write since the first.
    pub fn add_image(&self, seq: u64, image: &[u8]) -> io::Result<()> {
        self.put(&self.image_path(seq, now_ms()), image)
    }

    /// The last change the archive holds, the history it is on, and the
    /// segment that ends there, opened to go on -- its torn tail, if a crash
    /// left one, cut off.
    fn position(&self) -> io::Result<(u64, History, Option<Segment>)> {
        let history = match self.read_whole(&self.dir.join("history")) {
            Ok(b) => History::decode(&b).map_err(|e| corrupt(e.to_string()))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => History::default(),
            Err(e) => return Err(e),
        };
        let mut at = self.images()?.last().map_or(0, |i| i.0);
        let mut open = None;
        if let Some(&first) = self.segments()?.last() {
            let path = self.segment_path(first);
            let read = self.read_segment(&path)?;
            let (list, end) = entries(&read.plain)?;
            let last = first + list.iter().map(|e| e.3).sum::<u64>() - 1;
            if !list.is_empty() && last >= at {
                at = last;
                // Plain, a record cut short is cut off; sealed, a frame is,
                // and a frame holds whole records.
                let cut = match read.sealed {
                    Some(_) if end < read.plain.len() => {
                        return Err(corrupt(format!(
                            "{}: a record cut short inside a whole frame",
                            path.display()
                        )))
                    }
                    Some((whole, _)) => whole,
                    None => end,
                };
                let mut file = OpenOptions::new().write(true).open(&path)?;
                file.set_len(cut as u64)?;
                file.seek(io::SeekFrom::End(0))?;
                open = Some(Segment {
                    file,
                    next: last + 1,
                    len: end,
                    dirty: false,
                    synced: Instant::now(),
                    sealed: self
                        .key
                        .clone()
                        .zip(read.sealed)
                        .map(|(k, (_, frames))| (k, frames, Vec::new())),
                });
            }
        }
        Ok((at, history, open))
    }

    /// Follows `upstream` into the archive until `stop` is set, connecting
    /// again whenever the stream ends. `report` hears each thing worth
    /// telling an operator.
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
                Err(e) if e.kind() == io::ErrorKind::InvalidData => return Err(e),
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
        let (mut at, history, mut segment) = self.position()?;
        // A restore starts from an image, so an archive without one asks for
        // one, even where the primary could have sent every write instead.
        let first = self.images()?.is_empty();
        let mut rx = upstream.open(at, history.current(), first)?;
        let Message::Hello {
            image,
            seq,
            lineage,
            ..
        } = rx.receive()?
        else {
            return Err(io::Error::other("the primary did not say hello"));
        };
        if image {
            let Message::Image(bytes) = rx.receive()? else {
                return Err(io::Error::other("the primary promised an image"));
            };
            self.add_image(seq, &bytes)?;
            report(&format!(
                "image at change {seq} ({:.1} MB)",
                bytes.len() as f64 / 1e6
            ));
            at = seq;
            segment = None;
        } else if seq != at {
            return Err(io::Error::other("the primary started somewhere else"));
        }
        let history = History {
            lineage,
            following: true,
        };
        self.put(&self.dir.join("history"), &history.encode())?;

        let mut written = 0u64;
        let mut told = Instant::now();
        loop {
            if stop.load(Ordering::SeqCst) {
                if let Some(s) = &mut segment {
                    s.sync()?;
                }
                return Ok(());
            }
            match rx.receive()? {
                Message::Writes {
                    first,
                    times,
                    records,
                } => {
                    if first != at + 1 {
                        return Err(io::Error::other(format!(
                            "the primary sent write {first}, the archive is at {at}"
                        )));
                    }
                    let mut pos = 0;
                    for time in times {
                        let len = record_len(&records[pos..])
                            .ok_or_else(|| io::Error::other("a write record cut short"))?;
                        // Numbered by its first write here, as a segment's
                        // name is: a block's writes are one record.
                        let n = writes(&records[pos..pos + len]);
                        let seq = at + 1;
                        let rolled = self.roll.swap(false, Ordering::SeqCst);
                        let s = match &mut segment {
                            Some(s) if s.next == seq && s.len < SEGMENT && !rolled => s,
                            _ => {
                                if let Some(s) = &mut segment {
                                    s.sync()?;
                                }
                                let mut file = File::create(self.segment_path(seq))?;
                                // Sealed, the magic is a frame of its own, so a
                                // segment the archiver made and died in reads
                                // as an empty one.
                                let sealed = match &self.key {
                                    Some(key) => {
                                        let mut head = seal::MAGIC.to_vec();
                                        seal::seal_frame(key, 0, false, LOG_MAGIC, &mut head);
                                        file.write_all(&head)?;
                                        Some((key.clone(), 1, Vec::new()))
                                    }
                                    None => {
                                        file.write_all(LOG_MAGIC)?;
                                        None
                                    }
                                };
                                segment.insert(Segment {
                                    file,
                                    next: seq,
                                    len: LOG_MAGIC.len(),
                                    dirty: true,
                                    synced: Instant::now(),
                                    sealed,
                                })
                            }
                        };
                        s.write(&time.to_le_bytes())?;
                        s.write(&records[pos..pos + len])?;
                        s.next += n;
                        pos += len;
                        at = seq + n - 1;
                        written += n;
                    }
                    if let Some(s) = &mut segment {
                        // The batch's writes a frame, whole records in it.
                        s.flush()?;
                        if s.synced.elapsed() >= SYNC_EVERY {
                            s.sync()?;
                        }
                    }
                }
                Message::Alive { .. } => {
                    if let Some(s) = &mut segment {
                        s.sync()?;
                    }
                }
                Message::End(why) => return Err(io::Error::other(why)),
                _ => return Err(io::Error::other("the primary sent a message it should not")),
            }
            if told.elapsed() >= Duration::from_secs(10) && written > 0 {
                report(&format!("{written} writes archived, at change {at}"));
                written = 0;
                told = Instant::now();
            }
        }
    }

    /// Writes the database as it stood at `to` into `out`, a new file.
    pub fn restore(&self, out: &Path, to: Target) -> io::Result<Restored> {
        let tmp = out.with_extension("restoring");
        let r = self.assemble(&tmp, to)?;
        // Opened to check it whole, and forked: from here its writes are no
        // longer the primary's. The checkpoint lands the graph the open just
        // built in the file, as a clean shutdown would, so the database's
        // first real open does not build it a second time.
        let mut db = fenec_core::fs::open(&tmp).map_err(|e| corrupt(e.to_string()))?;
        if db.change_seq() != r.seq {
            return Err(corrupt(format!(
                "the restored file is at change {}, not {}",
                db.change_seq(),
                r.seq
            )));
        }
        db.fork(fresh_id())
            .and_then(|_| db.checkpoint())
            .map_err(|e| io::Error::other(e.to_string()))?;
        drop(db);
        // A log left beside `out` by a database there before would be
        // another file's (`fenec_core::fs`): gone before the rename.
        fenec_core::fs::forget_sync_log(out)?;
        fs::rename(&tmp, out)?;
        Ok(r)
    }

    /// The last change the segments hold and when it was appended: the
    /// last segment's last whole record.
    fn last_write(&self) -> io::Result<Option<(u64, u64)>> {
        let Some(&first) = self.segments()?.last() else {
            return Ok(None);
        };
        let bytes = self.read_segment(&self.segment_path(first))?.plain;
        let (list, _) = entries(&bytes)?;
        let writes: u64 = list.iter().map(|e| e.3).sum();
        Ok(list.last().map(|e| (first + writes - 1, e.0)))
    }

    /// Takes an image of the database as the archive's end holds it -- its
    /// last image and the writes after it, opened -- and adds it, named by
    /// when its last write was appended: the database stood so from then
    /// on. Nothing is asked of the primary. `None` when no write came after
    /// the last image.
    ///
    /// Without it an archive kept its first image for good: a restore
    /// replayed every write since, and nothing could be let go.
    pub fn consolidate(&self) -> io::Result<Option<u64>> {
        let Some(&(last_image, _)) = self.images()?.last() else {
            return Ok(None);
        };
        let Some((end, time)) = self.last_write()? else {
            return Ok(None);
        };
        if end <= last_image {
            return Ok(None);
        }
        let tmp = self.dir.join("consolidating.fenec");
        let r = self.assemble(&tmp, Target::End);
        let image = r.and_then(|r| {
            let db = fenec_core::fs::open_in_memory(&tmp).map_err(|e| corrupt(e.to_string()))?;
            if db.change_seq() != r.seq {
                return Err(corrupt(format!(
                    "the archive's end is change {}, the database opened at {}",
                    r.seq,
                    db.change_seq()
                )));
            }
            Ok((r.seq, db.snapshot()))
        });
        let _ = fs::remove_file(&tmp);
        let (seq, bytes) = image?;
        // The segments may have gone on while this read them: the image is
        // of the change it holds, timed by that change's write.
        let time = if seq == end {
            time
        } else {
            self.time_of(seq)?.unwrap_or(time)
        };
        self.put(&self.image_path(seq, time), &bytes)?;
        self.roll.store(true, Ordering::SeqCst);
        Ok(Some(seq))
    }

    /// When change `seq` was appended, from the segment that holds it.
    fn time_of(&self, seq: u64) -> io::Result<Option<u64>> {
        let segments = self.segments()?;
        let Some(&first) = segments.iter().rev().find(|&&f| f <= seq) else {
            return Ok(None);
        };
        let bytes = self.read_segment(&self.segment_path(first))?.plain;
        let (list, _) = entries(&bytes)?;
        let mut at = first;
        for &(time, _, _, n) in &list {
            if seq < at + n {
                return Ok(Some(time));
            }
            at += n;
        }
        Ok(None)
    }

    /// Lets go of what a restore to any moment in the last `keep_ms` does
    /// not need: the images before the newest one taken by then -- a
    /// restore to that moment starts from it -- and the segments whose
    /// writes all came before the oldest image kept. The segment being
    /// written is never among them. Returns the images and segments removed.
    pub fn prune(&self, keep_ms: u64, now_ms: u64) -> io::Result<(usize, usize)> {
        let images = self.images()?;
        let cutoff = now_ms.saturating_sub(keep_ms);
        // The newest image at or before the cutoff, or the oldest there is.
        let base = match images.iter().rposition(|i| i.1 <= cutoff) {
            Some(k) => k,
            None => return Ok((0, 0)),
        };
        let oldest = images[base].0;
        let mut removed = (0, 0);
        for &(seq, time) in &images[..base] {
            fs::remove_file(self.image_path(seq, time))?;
            removed.0 += 1;
        }
        let segments = self.segments()?;
        for k in 0..segments.len().saturating_sub(1) {
            // A segment's writes end where the next one's begin.
            if segments[k + 1] - 1 <= oldest {
                fs::remove_file(self.segment_path(segments[k]))?;
                removed.1 += 1;
            }
        }
        Ok(removed)
    }

    /// Beside `follow`: an image of the archive's end every `every` once a
    /// write came after the last, and what a restore within `keep` does not
    /// need let go after it, until `stop` is set.
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
            match self.consolidate() {
                Ok(Some(seq)) => report(&format!("image at change {seq}, taken here")),
                Ok(None) => {}
                Err(e) => report(&format!("no image taken: {e}")),
            }
            if let Some(keep) = keep {
                match self.prune(keep.as_millis() as u64, now_ms()) {
                    Ok((0, 0)) => {}
                    Ok((i, s)) => report(&format!("let go of {i} images and {s} segments")),
                    Err(e) => report(&format!("nothing let go: {e}")),
                }
            }
        }
        Ok(())
    }

    /// Reads the whole archive the way a restore would, and says what it
    /// holds: each image opens and is at the change its name says, the
    /// segments run on from the oldest image with no change missing -- a
    /// gap is the start of a later image, where the primary sent one --
    /// only the last segment may end in a record cut short, and a restore
    /// to the end opens. A backup no restore can be made from is not one,
    /// and this is how to know before it is needed.
    pub fn verify(&self) -> io::Result<Verified> {
        let images = self.images()?;
        let Some(&(first, from)) = images.first() else {
            return Err(corrupt(
                "no image: a restore has nothing to start from".into(),
            ));
        };
        for &(seq, time) in &images {
            let path = self.image_path(seq, time);
            let db = match &self.key {
                None => fenec_core::fs::open_read_only(&path)
                    .map_err(|e| corrupt(format!("{}: {e}", path.display())))?,
                Some(_) => {
                    let mut db = fenec_core::engine::Database::new();
                    db.load(&self.read_whole(&path)?)
                        .map_err(|e| corrupt(format!("{}: {e}", path.display())))?;
                    db
                }
            };
            if db.change_seq() != seq {
                return Err(corrupt(format!(
                    "{}: the image is at change {}, not {seq}",
                    path.display(),
                    db.change_seq()
                )));
            }
        }
        let segments = self.segments()?;
        let (mut covered, mut to, mut torn) = (first, None, 0);
        let mut gaps = Vec::new();
        for (k, &start) in segments.iter().enumerate() {
            let path = self.segment_path(start);
            let read = self.read_segment(&path)?;
            let (list, end) =
                entries(&read.plain).map_err(|e| corrupt(format!("{}: {e}", path.display())))?;
            // Cut short: a record, plain, or a frame, sealed.
            let cut = match read.sealed {
                Some(_) if end < read.plain.len() => {
                    return Err(corrupt(format!(
                        "{}: a record cut short inside a whole frame",
                        path.display()
                    )))
                }
                Some((whole, _)) => read.len - whole,
                None => read.plain.len() - end,
            };
            if cut > 0 {
                if k + 1 < segments.len() {
                    return Err(corrupt(format!(
                        "{}: cut short {cut} bytes before its end, with segments after it",
                        path.display(),
                    )));
                }
                torn = cut;
            }
            if start > covered + 1 {
                if !images.iter().any(|i| i.0 + 1 == start) {
                    return Err(corrupt(format!(
                        "changes {} to {} are missing: no segment holds them and no image starts after them",
                        covered + 1,
                        start - 1
                    )));
                }
                gaps.push((covered + 1, start - 1));
            }
            let writes: u64 = list.iter().map(|e| e.3).sum();
            if writes > 0 {
                covered = covered.max(start + writes - 1);
                to = list.last().map(|e| e.0);
            }
        }
        let last = covered.max(images.last().map_or(0, |i| i.0));
        let probe = std::env::temp_dir().join(format!(
            "fenec-verify-{}-{}.fenec",
            std::process::id(),
            now_ms()
        ));
        let restored = self.restore(&probe, Target::End);
        let _ = fs::remove_file(&probe);
        let r = restored?;
        if r.seq != last {
            return Err(corrupt(format!(
                "a restore to the end reached change {}, the archive holds {last}",
                r.seq
            )));
        }
        Ok(Verified {
            images: images.len(),
            segments: segments.len(),
            first,
            from,
            last,
            to,
            torn,
            gaps,
        })
    }

    /// The image `to` starts from and the writes after it up to there,
    /// copied into `tmp`: a fenecdb file, not yet opened.
    ///
    /// Two passes over the segments, one in memory at a time: the first
    /// finds where to stop, the second copies the writes up to there.
    fn assemble(&self, tmp: &Path, to: Target) -> io::Result<Restored> {
        let images = self.images()?;
        let segments = self.segments()?;
        let last_image = images.last().map_or(0, |i| i.0);

        // Where to stop, and when the last write kept was appended. A
        // record's writes are kept or left together: a block landed whole,
        // so the database never stood at a change inside one, and a change
        // asked for there is the one before the block.
        let mut last = last_image;
        let mut kept: Option<(u64, u64)> = None;
        // The change before the record that holds the one asked for, when
        // that one is not the record's last.
        let mut inside: Option<u64> = None;
        let mut past = false;
        for &first in &segments {
            let bytes = self.read_segment(&self.segment_path(first))?.plain;
            let (list, _) = entries(&bytes)?;
            let mut seq = first - 1;
            for &(time, _, _, n) in &list {
                let from = seq + 1;
                seq += n;
                last = last.max(seq);
                if let Target::Change(c) = to {
                    if from <= c && c < seq {
                        inside = Some(from - 1);
                    }
                }
                if let Target::Time(t) = to {
                    // The first write appended after `t` ends it.
                    past |= time > t;
                    if !past {
                        kept = Some((seq, time));
                    }
                }
            }
        }
        let (stop, time) = match to {
            Target::End => (last, None),
            Target::Change(c) if c > last => {
                return Err(io::Error::other(format!(
                    "the archive holds changes up to {last}, not {c}"
                )))
            }
            Target::Change(c) => (inside.unwrap_or(c), None),
            Target::Time(t) => match kept {
                Some((seq, time)) => (seq, Some(time)),
                // Before any write, an image taken by then is the answer.
                None => match images.iter().rev().find(|i| i.1 <= t) {
                    Some(i) => (i.0, None),
                    None => {
                        return Err(io::Error::other(
                            "the archive holds nothing from before that time",
                        ))
                    }
                },
            },
        };
        let Some(&(image, taken)) = images.iter().rev().find(|i| i.0 <= stop) else {
            return Err(io::Error::other(format!(
                "no image at or before change {stop}: restoring needs one to start from"
            )));
        };

        // The image, then the writes after it: that is a fenecdb file.
        let _ = fs::remove_file(tmp);
        match &self.key {
            None => {
                fs::copy(self.image_path(image, taken), tmp)?;
            }
            Some(_) => fs::write(tmp, self.read_whole(&self.image_path(image, taken))?)?,
        }
        let mut f = OpenOptions::new().append(true).open(tmp)?;
        let mut want = image + 1;
        for (k, &first) in segments.iter().enumerate() {
            let next = segments.get(k + 1).copied().unwrap_or(u64::MAX);
            if want > stop || next <= want || first > stop {
                continue;
            }
            let bytes = self.read_segment(&self.segment_path(first))?.plain;
            let (list, _) = entries(&bytes)?;
            let mut seq = first;
            for &(_, start, end, n) in &list {
                let (from, to) = (seq, seq + n - 1);
                seq += n;
                if to < want {
                    continue;
                }
                if to > stop || from != want {
                    break;
                }
                f.write_all(&bytes[start..end])?;
                want = to + 1;
            }
        }
        if want != stop + 1 {
            return Err(io::Error::other(format!(
                "the archive lacks change {want}: between image {image} and change \
                 {stop} it holds no write there"
            )));
        }
        f.sync_all()?;
        drop(f);
        Ok(Restored {
            seq: stop,
            image,
            time,
        })
    }
}

/// One image of the database behind `upstream`, taken while it runs, into
/// `out`: a file, or an archive directory. A file gets its history forked,
/// as a restore's does, so it can be opened as a primary of its own.
pub fn backup(upstream: &Upstream, out: &Path, key: Option<&Key>) -> io::Result<u64> {
    let mut rx = upstream.open(0, 0, true)?;
    let Message::Hello {
        image: true,
        seq,
        lineage,
        ..
    } = rx.receive()?
    else {
        return Err(io::Error::other("the primary sent no image"));
    };
    let Message::Image(mut bytes) = rx.receive()? else {
        return Err(io::Error::other("the primary promised an image"));
    };
    if out.is_dir() {
        Archive::with_key(out, key.cloned())?.add_image(seq, &bytes)?;
    } else {
        let history = History {
            lineage,
            following: false,
        };
        bytes.extend_from_slice(&history.forked(fresh_id(), seq).record());
        if let Some(key) = key {
            bytes = seal::seal_whole(key, &bytes);
        }
        let tmp = out.with_extension("tmp");
        let mut f = File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        fenec_core::fs::forget_sync_log(out)?;
        fs::rename(&tmp, out)?;
    }
    Ok(seq)
}

/// A backup file sealed with `key` (`backup` with a key), opened into
/// `out`: the database file it holds.
pub fn unseal(sealed: &Path, key: &Key, out: &Path) -> io::Result<u64> {
    let bytes = seal::open_whole(key, &fs::read(sealed)?)
        .map_err(|e| corrupt(format!("{}: {e}", sealed.display())))?;
    let tmp = out.with_extension("tmp");
    fs::write(&tmp, &bytes)?;
    // Opened to check it whole, as a restore checks its file.
    let seq = fenec_core::fs::open_read_only(&tmp)
        .map_err(|e| corrupt(format!("{}: {e}", sealed.display())))?
        .change_seq();
    File::open(&tmp)?.sync_all()?;
    fenec_core::fs::forget_sync_log(out)?;
    fs::rename(&tmp, out)?;
    Ok(seq)
}
