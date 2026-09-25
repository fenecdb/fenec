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

use crate::replication::{fresh_id, Message, Upstream};
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

pub struct Archive {
    dir: PathBuf,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn corrupt(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
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
}

impl Segment {
    fn sync(&mut self) -> io::Result<()> {
        if self.dirty {
            self.file.sync_data()?;
            self.dirty = false;
        }
        self.synced = Instant::now();
        Ok(())
    }
}

impl Archive {
    pub fn new(dir: impl AsRef<Path>) -> io::Result<Archive> {
        fs::create_dir_all(dir.as_ref())?;
        Ok(Archive {
            dir: dir.as_ref().to_path_buf(),
        })
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
        f.write_all(bytes)?;
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
        let history = match fs::read(self.dir.join("history")) {
            Ok(b) => History::decode(&b).map_err(|e| corrupt(e.to_string()))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => History::default(),
            Err(e) => return Err(e),
        };
        let mut at = self.images()?.last().map_or(0, |i| i.0);
        let mut open = None;
        if let Some(&first) = self.segments()?.last() {
            let path = self.segment_path(first);
            let data = fs::read(&path)?;
            let (list, end) = entries(&data)?;
            let last = first + list.iter().map(|e| e.3).sum::<u64>() - 1;
            if !list.is_empty() && last >= at {
                at = last;
                let mut file = OpenOptions::new().write(true).open(&path)?;
                file.set_len(end as u64)?;
                file.seek(io::SeekFrom::End(0))?;
                open = Some(Segment {
                    file,
                    next: last + 1,
                    len: end,
                    dirty: false,
                    synced: Instant::now(),
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
                        let s = match &mut segment {
                            Some(s) if s.next == seq && s.len < SEGMENT => s,
                            _ => {
                                if let Some(s) = &mut segment {
                                    s.sync()?;
                                }
                                let mut file = File::create(self.segment_path(seq))?;
                                file.write_all(LOG_MAGIC)?;
                                segment.insert(Segment {
                                    file,
                                    next: seq,
                                    len: LOG_MAGIC.len(),
                                    dirty: true,
                                    synced: Instant::now(),
                                })
                            }
                        };
                        s.file.write_all(&time.to_le_bytes())?;
                        s.file.write_all(&records[pos..pos + len])?;
                        s.len += 8 + len;
                        s.next += n;
                        s.dirty = true;
                        pos += len;
                        at = seq + n - 1;
                        written += n;
                    }
                    if let Some(s) = &mut segment {
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
    ///
    /// Two passes over the segments, one in memory at a time: the first
    /// finds where to stop, the second copies the writes up to there.
    pub fn restore(&self, out: &Path, to: Target) -> io::Result<Restored> {
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
            let bytes = fs::read(self.segment_path(first))?;
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
        let tmp = out.with_extension("restoring");
        let _ = fs::remove_file(&tmp);
        fs::copy(self.image_path(image, taken), &tmp)?;
        let mut f = OpenOptions::new().append(true).open(&tmp)?;
        let mut want = image + 1;
        for (k, &first) in segments.iter().enumerate() {
            let next = segments.get(k + 1).copied().unwrap_or(u64::MAX);
            if want > stop || next <= want || first > stop {
                continue;
            }
            let bytes = fs::read(self.segment_path(first))?;
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

        // Opened to check it whole, and forked: from here its writes are no
        // longer the primary's. The checkpoint lands the graph the open just
        // built in the file, as a clean shutdown would, so the database's
        // first real open does not build it a second time.
        let mut db = fenec_core::fs::open(&tmp).map_err(|e| corrupt(e.to_string()))?;
        if db.change_seq() != stop {
            return Err(corrupt(format!(
                "the restored file is at change {}, not {stop}",
                db.change_seq()
            )));
        }
        db.fork(fresh_id())
            .and_then(|_| db.checkpoint())
            .map_err(|e| io::Error::other(e.to_string()))?;
        drop(db);
        fs::rename(&tmp, out)?;
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
pub fn backup(upstream: &Upstream, out: &Path) -> io::Result<u64> {
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
        Archive::new(out)?.add_image(seq, &bytes)?;
    } else {
        let history = History {
            lineage,
            following: false,
        };
        bytes.extend_from_slice(&history.forked(fresh_id(), seq).record());
        let tmp = out.with_extension("tmp");
        let mut f = File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, out)?;
    }
    Ok(seq)
}
