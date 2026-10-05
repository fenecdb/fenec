//! A `@hash` bucket: the ids holding one value, ascending.
//!
//! A bucket was a `Vec` in the order its ids came, and a document left it by
//! `retain`, a walk of the whole bucket: with a field of a few values -- a
//! country, a device, an event's name -- each bucket held a fifth of the
//! collection, and the `@ttl` sweep held the write lock 370 to 500 ms for
//! every 1 000 rows it deleted from 594 000, every dashboard read waiting
//! behind it. Kept ascending, an id is found by binary search; and past
//! [`CHUNK`] ids the bucket is runs of at most that many, so taking one out
//! moves the rest of its run and no more, where one list moved every id
//! after it -- the sweep takes the oldest rows, which a list ascending holds
//! first. A bucket of a few ids, a `@unique` value's, stays one list, the
//! size it was. The browser module keeps every bucket one list, sorted: its
//! databases are a page's, and the runs were 1.3 KB brotli of it.

use crate::value::DocId;

/// The most ids a run holds, and the most a bucket holds as one list.
/// Taking an id out moves at most this many: 4 KB.
#[cfg(not(target_arch = "wasm32"))]
const CHUNK: usize = 512;

/// The ids under one value of a `@hash` index, ascending and each once.
pub enum Bucket {
    /// Up to [`CHUNK`] ids.
    Flat(Vec<DocId>),
    /// Runs of at most [`CHUNK`] ids, each ascending and every one of a
    /// run below every one of the next, behind a box so a bucket is the
    /// size of a `Vec`.
    #[cfg(not(target_arch = "wasm32"))]
    Runs(Box<Runs>),
}

#[cfg(not(target_arch = "wasm32"))]
pub struct Runs {
    len: usize,
    runs: Vec<Vec<DocId>>,
}

impl Default for Bucket {
    fn default() -> Self {
        Bucket::Flat(Vec::new())
    }
}

/// A bucket's ids in order: one iterator of no generic, which every reader
/// shares -- through `flatten` each was a copy of its own in the browser
/// module, and so was the map of another value type that a group's number
/// was kept in, so those keep theirs in a bucket too ([`Bucket::one`]).
pub struct Ids<'a> {
    runs: &'a [Vec<DocId>],
    at: usize,
}

impl Iterator for Ids<'_> {
    type Item = DocId;

    fn next(&mut self) -> Option<DocId> {
        loop {
            let (run, rest) = self.runs.split_first()?;
            if let Some(&id) = run.get(self.at) {
                self.at += 1;
                return Some(id);
            }
            (self.runs, self.at) = (rest, 0);
        }
    }
}

/// No ids: what a missing bucket reads as.
pub static EMPTY: Bucket = Bucket::Flat(Vec::new());

impl Bucket {
    /// A bucket of `n` alone: a number a map of the index's own type keeps
    /// under a key, read back by [`Bucket::first`].
    pub fn one(n: DocId) -> Bucket {
        Bucket::Flat(vec![n])
    }

    /// The least id held.
    pub fn first(&self) -> Option<DocId> {
        self.runs().first().and_then(|r| r.first().copied())
    }

    /// How many ids it holds.
    pub fn len(&self) -> usize {
        match self {
            Bucket::Flat(v) => v.len(),
            #[cfg(not(target_arch = "wasm32"))]
            Bucket::Runs(r) => r.len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The runs, in order: a flat bucket is one.
    pub fn runs(&self) -> &[Vec<DocId>] {
        match self {
            Bucket::Flat(v) => std::slice::from_ref(v),
            #[cfg(not(target_arch = "wasm32"))]
            Bucket::Runs(r) => &r.runs,
        }
    }

    /// The ids, ascending.
    pub fn iter(&self) -> Ids<'_> {
        Ids {
            runs: self.runs(),
            at: 0,
        }
    }

    /// The ids, ascending, in a list of their own.
    pub fn to_vec(&self) -> Vec<DocId> {
        let mut out = Vec::with_capacity(self.len());
        for r in self.runs() {
            out.extend_from_slice(r);
        }
        out
    }

    /// The bytes its lists hold on the heap.
    pub fn heap_bytes(&self) -> usize {
        let w = std::mem::size_of::<DocId>();
        match self {
            Bucket::Flat(v) => v.capacity() * w,
            #[cfg(not(target_arch = "wasm32"))]
            Bucket::Runs(r) => {
                std::mem::size_of::<Runs>()
                    + r.runs.capacity() * std::mem::size_of::<Vec<DocId>>()
                    + r.runs.iter().map(|v| v.capacity() * w).sum::<usize>()
            }
        }
    }

    /// Adds `id`; the change in the bytes the bucket holds on the heap. An
    /// id it holds already stays once. Ids mostly come ascending, which an
    /// append takes.
    pub fn add(&mut self, id: DocId) -> isize {
        let w = std::mem::size_of::<DocId>() as isize;
        match self {
            Bucket::Flat(v) => {
                let before = v.capacity() as isize * w;
                match v.last() {
                    Some(&l) if l >= id => match v.binary_search(&id) {
                        Ok(_) => return 0,
                        Err(i) => v.insert(i, id),
                    },
                    _ => v.push(id),
                }
                let after = v.capacity() as isize * w;
                // Past a run's size: the list is cut in two runs.
                #[cfg(not(target_arch = "wasm32"))]
                if v.len() > CHUNK {
                    let runs = Runs {
                        len: v.len(),
                        runs: vec![run_of(&v[..CHUNK / 2]), run_of(&v[CHUNK / 2..])],
                    };
                    *self = Bucket::Runs(Box::new(runs));
                    return self.heap_bytes() as isize - before;
                }
                after - before
            }
            #[cfg(not(target_arch = "wasm32"))]
            Bucket::Runs(r) => r.add(id),
        }
    }

    /// Takes `id` out; the change in the bytes the bucket holds on the heap.
    pub fn remove(&mut self, id: DocId) -> isize {
        let w = std::mem::size_of::<DocId>() as isize;
        match self {
            Bucket::Flat(v) => {
                if let Ok(i) = v.binary_search(&id) {
                    v.remove(i);
                }
                if v.is_empty() {
                    let gone = v.capacity() as isize * w;
                    *v = Vec::new();
                    return -gone;
                }
                0
            }
            #[cfg(not(target_arch = "wasm32"))]
            Bucket::Runs(r) => {
                let d = r.remove(id);
                if r.runs.len() > 1 {
                    return d;
                }
                // One run left: a list again, as small buckets are.
                let before = self.heap_bytes() as isize;
                let Bucket::Runs(r) = std::mem::take(self) else {
                    unreachable!()
                };
                *self = Bucket::Flat(r.runs.into_iter().next().unwrap_or_default());
                d + self.heap_bytes() as isize - before
            }
        }
    }
}

/// A run holding `ids`, with room for a whole run.
#[cfg(not(target_arch = "wasm32"))]
fn run_of(ids: &[DocId]) -> Vec<DocId> {
    let mut v = Vec::with_capacity(CHUNK);
    v.extend_from_slice(ids);
    v
}

#[cfg(not(target_arch = "wasm32"))]
impl Runs {
    /// The run `id` belongs in: the first whose last id is not below it,
    /// or the last.
    fn find(&self, id: DocId) -> usize {
        let i = self
            .runs
            .partition_point(|r| r.last().is_some_and(|&l| l < id));
        i.min(self.runs.len() - 1)
    }

    fn add(&mut self, id: DocId) -> isize {
        let w = std::mem::size_of::<DocId>() as isize;
        let outer = std::mem::size_of::<Vec<DocId>>() as isize;
        let last = self.runs.len() - 1;
        // The common case: past every id held.
        if self.runs[last].last().is_none_or(|&l| l < id) {
            self.len += 1;
            if self.runs[last].len() < CHUNK {
                self.runs[last].push(id);
                return 0;
            }
            let before = self.runs.capacity() as isize;
            self.runs.push(run_of(&[id]));
            return CHUNK as isize * w + (self.runs.capacity() as isize - before) * outer;
        }
        let at = self.find(id);
        let run = &mut self.runs[at];
        let Err(i) = run.binary_search(&id) else {
            return 0;
        };
        self.len += 1;
        if run.len() < CHUNK {
            run.insert(i, id);
            return 0;
        }
        // A full run is cut in two, `id` going in the half it falls in.
        let mut tail = run_of(&run[CHUNK / 2..]);
        run.truncate(CHUNK / 2);
        match i <= CHUNK / 2 {
            true => run.insert(i, id),
            false => tail.insert(i - CHUNK / 2, id),
        }
        let before = self.runs.capacity() as isize;
        self.runs.insert(at + 1, tail);
        CHUNK as isize * w + (self.runs.capacity() as isize - before) * outer
    }

    fn remove(&mut self, id: DocId) -> isize {
        let w = std::mem::size_of::<DocId>() as isize;
        let at = self.find(id);
        let run = &mut self.runs[at];
        let Ok(i) = run.binary_search(&id) else {
            return 0;
        };
        run.remove(i);
        self.len -= 1;
        // A run left with a quarter of its room joins the next where the
        // two fit in one, so ids taken out here and there do not leave a
        // run's room for a few of them each.
        let n = run.len();
        if n == 0 {
            let gone = self.runs.remove(at);
            return -(gone.capacity() as isize) * w;
        }
        if n < CHUNK / 4 && at + 1 < self.runs.len() && n + self.runs[at + 1].len() <= CHUNK {
            let next = self.runs.remove(at + 1);
            let gone = next.capacity() as isize;
            let run = &mut self.runs[at];
            let before = run.capacity() as isize;
            run.extend_from_slice(&next);
            return (run.capacity() as isize - before - gone) * w;
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bucket against a sorted set of the same ids, through adds and
    /// removes in every order, its heap bytes counted as its changes say.
    #[test]
    fn a_bucket_holds_what_a_sorted_set_holds() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..40 {
            let span = [10, 600, 3000, 20_000][round % 4];
            let mut b = Bucket::default();
            let mut set = std::collections::BTreeSet::new();
            let mut heap = 0isize;
            for step in 0..6000u64 {
                let id = match round % 3 {
                    // Ascending, as ids are handed out, some taken back.
                    0 => step,
                    _ => next() % span,
                };
                if next() % 3 == 0 {
                    heap += b.remove(id);
                    set.remove(&id);
                } else {
                    heap += b.add(id);
                    set.insert(id);
                }
                if step % 97 == 0 {
                    assert_eq!(b.len(), set.len());
                    assert!(
                        b.iter().eq(set.iter().copied()),
                        "round {round} step {step}"
                    );
                    assert_eq!(heap, b.heap_bytes() as isize, "round {round} step {step}");
                    assert!(b.runs().iter().all(|r| r.len() <= CHUNK));
                }
            }
            // Emptied the oldest first, as the sweep does.
            let ids: Vec<DocId> = set.iter().copied().collect();
            for id in ids {
                heap += b.remove(id);
            }
            assert!(b.is_empty());
            assert_eq!(heap, b.heap_bytes() as isize);
            assert_eq!(b.heap_bytes(), 0);
        }
    }
}
