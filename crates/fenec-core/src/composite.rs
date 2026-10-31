//! The index over several fields behind `@sorted(owner, created desc)` and
//! `@unique(org, slug)`.
//!
//! The commonest page an app reads -- a user's latest twenty, a tenant's
//! inbox, `where owner = $1 order created desc limit 20` -- had two plans,
//! both wrong for it: walk `created`'s ordered index testing `owner` row by
//! row, which gives up past an eighth of the collection, or read `owner`'s
//! hash bucket and sort every row of it. An index whose keys are `(owner,
//! created)` holds that page as a run of its entries, found by two searches
//! and walked as far as the page.
//!
//! A key is its parts' bytes laid end to end, each part encoded so that the
//! bytes order as the values do -- an int's sign flipped, a float's bits
//! made to order, a text escaped and ended -- and a descending part's every
//! byte inverted. So two keys compare as their bytes do, the first part
//! first, and an equality on the first parts is a prefix of the bytes. A
//! text in a collation (`collate tr`) has no such bytes: its part is kept
//! as written behind a tag naming the collation, and an index holding one
//! compares part by part ([`cmp_keys`]), each such part by its collation.
//! The entries are `(key, id)` in [`Chunked`]'s chunks, an ordered index's
//! own, so ties are in ascending id, the order the scan leaves them in.
//!
//! Every answer is the scan's, row for row: `null` is a part's lowest
//! value, as `cmp_value` puts it below every value, and a value the order
//! has no place for -- a float's `NaN`, which compares equal to everything,
//! or a json part's boolean, list, object or int past 2^53 -- puts its row
//! apart: while one is held no walk answers, and a range's rows take the
//! rows apart with them as candidates to test.
//!
//! Derived data like the other ordered index: built by the first statement
//! that reads it after an open and by `create index`, kept on write, never
//! in the file.

// Without the `composite` feature the index's code is here but unused
// (`off.rs`); the encoding the planner shares stays.
#![cfg_attr(not(feature = "composite"), allow(dead_code, unused_imports))]
use crate::case::CaseFold;
use crate::collate::{Collation, TextOrder};
use crate::sorted::{float_key, int_key, json_slot, Chunked, Entry, JsonSlot};
use crate::value::{DataType, DocId, Value};
use std::cmp::Ordering;
use std::ops::Bound;

/// A part holding `null`: below every value of the part.
const NULL: u8 = 0x01;
/// A bound's alone, never a part's: above a part's `null`, below each of
/// its values -- the start of a range, which no `null` is in.
const ABOVE_NULL: u8 = 0x02;
/// A number: the 8 bytes of its ordered key, big-endian.
const NUM: u8 = 0x10;
const FALSE: u8 = 0x11;
const TRUE: u8 = 0x12;
/// A text: its bytes, a zero byte written as `00 FF`, then `00 01`.
const TEXT: u8 = 0x20;
/// A text in a collation, the collation's code added (`0x21` `tr`, `0x22`
/// `und`): its length as 4 bytes, then its bytes as written. Compared by
/// the collation, so never inverted past its tag.
const COLLATED: u8 = 0x20;
/// A value the order has no place for: its encoding's length as 4 bytes,
/// then the encoding -- found by its bytes, for a unique index.
const APART: u8 = 0x30;
/// A bound's alone: above every part.
const TOP: u8 = 0xFF;

/// What a part keys its field's values as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Int,
    /// A timestamp, keyed as an int is, and met by the text it parses
    /// from.
    Time,
    Float,
    Bool,
    Text,
    /// A json field or a path into one: a number as the `f64` it is, a
    /// text by its bytes, every number below every text.
    Json,
}

/// One part of an index's keys.
#[derive(Clone, Copy, Debug)]
pub struct Part {
    pub kind: Kind,
    pub desc: bool,
    /// The collation a text part compares in.
    pub coll: Option<Collation>,
    /// The fold a `nocase` text part is keyed by: its bytes are the fold's,
    /// so an equality finds every spelling and a `@unique` one refuses a
    /// second.
    pub fold: Option<CaseFold>,
}

impl Part {
    pub fn of(ty: &DataType, desc: bool, order: TextOrder) -> Part {
        let kind = match ty {
            DataType::Int => Kind::Int,
            DataType::Timestamp => Kind::Time,
            DataType::Float => Kind::Float,
            DataType::Bool => Kind::Bool,
            DataType::Text => Kind::Text,
            _ => Kind::Json,
        };
        Part {
            kind,
            desc,
            coll: order.coll.filter(|_| kind == Kind::Text),
            fold: order.fold.filter(|_| kind == Kind::Text),
        }
    }
}

/// How many bytes of a key its entry holds itself: 22 makes the entry 32
/// bytes, an id's 8 and 24 for the key, and holds an int and a time (18
/// bytes) or a short text and a time.
const INLINE: usize = 22;

/// A key's bytes: in the entry where they fit, on the heap past that. Each
/// boxed, a million keys of an int and a time took 48 MB counted, the
/// allocator's own rounding and headers besides, and the build allocated a
/// million times.
#[derive(Clone)]
pub enum KeyBytes {
    Inline(u8, [u8; INLINE]),
    Heap(Box<[u8]>),
}

impl KeyBytes {
    pub fn new(b: &[u8]) -> KeyBytes {
        match b.len() <= INLINE {
            true => {
                let mut a = [0; INLINE];
                a[..b.len()].copy_from_slice(b);
                KeyBytes::Inline(b.len() as u8, a)
            }
            false => KeyBytes::Heap(b.into()),
        }
    }

    /// The bytes it holds on the heap.
    fn heap(&self) -> usize {
        match self {
            KeyBytes::Inline(..) => 0,
            KeyBytes::Heap(b) => b.len(),
        }
    }
}

impl std::ops::Deref for KeyBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            KeyBytes::Inline(n, a) => &a[..*n as usize],
            KeyBytes::Heap(b) => b,
        }
    }
}

impl PartialEq for KeyBytes {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl Eq for KeyBytes {}
impl PartialOrd for KeyBytes {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for KeyBytes {
    /// Two keys held in their entries compare as two numbers, their bytes
    /// big-endian, and then by length: the bytes past a key's end are zero,
    /// the least a byte is, so that is the order of the bytes themselves --
    /// a key that is the start of another the lower. A call to `memcmp`
    /// each, the comparisons were most of a write's search.
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (KeyBytes::Inline(m, a), KeyBytes::Inline(n, b)) => {
                let wide = |x: &[u8; INLINE]| {
                    let (hi, lo) = x.split_at(16);
                    let mut l = [0u8; 8];
                    l[..lo.len()].copy_from_slice(lo);
                    (
                        u128::from_be_bytes(hi.try_into().unwrap_or([0; 16])),
                        u64::from_be_bytes(l),
                    )
                };
                wide(a).cmp(&wide(b)).then(m.cmp(n))
            }
            _ => (**self).cmp(&**other),
        }
    }
}

impl std::fmt::Debug for KeyBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (**self).fmt(f)
    }
}

/// A key's entry: its bytes and its row's id.
pub type KeyEntry = (KeyBytes, DocId);

/// Where a run of entries starts and ends.
pub type Span = (Bound<KeyEntry>, Bound<KeyEntry>);

/// A row's key, and what [`CompositeIndex`] needs to know of it.
#[derive(Clone)]
pub struct RowKey {
    pub bytes: KeyBytes,
    /// A part holds a value the order has no place for.
    pub apart: bool,
    /// A part holds `null`: no value, to a unique index.
    pub null: bool,
}

/// The values a key's part is made of: `v` as `p` keys it, appended to
/// `out` -- and whether it has no place in the order.
fn put_part(out: &mut Vec<u8>, p: &Part, v: &Value) -> bool {
    let at = out.len();
    let apart = match (p.kind, v) {
        (_, Value::Null) => {
            out.push(NULL);
            false
        }
        (Kind::Int | Kind::Time, Value::Int(i) | Value::Timestamp(i)) => num(out, int_key(*i)),
        (Kind::Float, Value::Float(f)) if !f.is_nan() => num(out, float_key(*f)),
        (Kind::Bool, Value::Bool(b)) => {
            out.push(if *b { TRUE } else { FALSE });
            false
        }
        (Kind::Text, Value::Text(t)) => match p.fold {
            Some(f) => text(out, &crate::case::fold(t, f), p.coll),
            None => text(out, t, p.coll),
        },
        (Kind::Json, v) => match json_slot(Some(v)) {
            JsonSlot::Num(k) => num(out, k),
            JsonSlot::Text(t) => text(out, &t, None),
            JsonSlot::Null => {
                out.push(NULL);
                false
            }
            JsonSlot::Apart => apart(out, v),
        },
        (_, v) => apart(out, v),
    };
    if p.desc {
        // A collated text is compared by its collation, which reads it as
        // written: its tag alone is inverted, which orders it against a
        // `null` the other way round.
        let end = match p.coll.is_some() && out[at] > COLLATED && out[at] < APART {
            true => at + 1,
            false => out.len(),
        };
        for b in &mut out[at..end] {
            *b = !*b;
        }
    }
    apart
}

fn num(out: &mut Vec<u8>, k: u64) -> bool {
    out.push(NUM);
    out.extend_from_slice(&k.to_be_bytes());
    false
}

fn text(out: &mut Vec<u8>, t: &str, coll: Option<Collation>) -> bool {
    match coll {
        Some(c) => {
            out.push(COLLATED + c.code());
            out.extend_from_slice(&(t.len() as u32).to_be_bytes());
            out.extend_from_slice(t.as_bytes());
        }
        None => {
            out.push(TEXT);
            for &b in t.as_bytes() {
                out.push(b);
                if b == 0 {
                    out.push(0xFF);
                }
            }
            out.extend_from_slice(&[0, 1]);
        }
    }
    false
}

fn apart(out: &mut Vec<u8>, v: &Value) -> bool {
    out.push(APART);
    let at = out.len();
    out.extend_from_slice(&[0; 4]);
    crate::codec::encode_value(out, v);
    let n = (out.len() - at - 4) as u32;
    out[at..at + 4].copy_from_slice(&n.to_be_bytes());
    true
}

/// The 4 bytes of a length at `at`, inverted back where its part is.
fn len_at(k: &[u8], at: usize, inverted: bool) -> usize {
    let mut b = [0u8; 4];
    if let Some(s) = k.get(at..at + 4) {
        b.copy_from_slice(s);
    }
    let n = u32::from_be_bytes(b);
    (if inverted { !n } else { n }) as usize
}

/// How many bytes the part at `i` of a key takes.
fn part_len(k: &[u8], i: usize) -> usize {
    let t = k[i];
    let desc = t >= 0x80;
    let tag = if desc { !t } else { t };
    match tag {
        NUM => 9,
        TEXT => {
            let (zero, end) = if desc { (0xFF, 0xFE) } else { (0, 1) };
            let mut j = i + 1;
            while j + 1 < k.len() {
                if k[j] != zero {
                    j += 1;
                } else if k[j + 1] == end {
                    return j + 2 - i;
                } else {
                    j += 2;
                }
            }
            k.len() - i
        }
        // The length of a collated text is never inverted.
        t if t > COLLATED && t < APART => 5 + len_at(k, i + 1, false),
        APART => 5 + len_at(k, i + 1, desc),
        _ => 1,
    }
}

/// How many bytes the first `parts` parts of a key take.
fn head_len(k: &[u8], parts: usize) -> usize {
    let mut at = 0;
    for _ in 0..parts {
        if at >= k.len() {
            break;
        }
        at += part_len(k, at);
    }
    at.min(k.len())
}

/// The text of a collated part `l` bytes long at `at`, past its tag and
/// length.
fn collated_at(k: &[u8], at: usize, l: usize) -> &str {
    k.get(at + 5..at + l)
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or("")
}

/// Two keys of an index holding a collated text part, part by part: a part
/// of each tag compares as its bytes do, but a collated text, which
/// compares by its collation. A key that ends first is the lower, which is
/// how a bound made of a prefix stands below every key it begins.
pub(crate) fn cmp_keys(a: &[u8], b: &[u8]) -> Ordering {
    let (mut i, mut j) = (0, 0);
    loop {
        let (ta, tb) = match (a.get(i), b.get(j)) {
            (Some(&x), Some(&y)) => (x, y),
            (x, y) => return x.is_some().cmp(&y.is_some()),
        };
        if ta != tb {
            return ta.cmp(&tb);
        }
        let (la, lb) = (part_len(a, i), part_len(b, j));
        let desc = ta >= 0x80;
        let tag = if desc { !ta } else { ta };
        let coll = match tag > COLLATED && tag < APART {
            true => Collation::from_code(tag - COLLATED),
            false => None,
        };
        let o = match coll {
            Some(c) => {
                let o = c.compare(collated_at(a, i, la), collated_at(b, j, lb));
                if desc {
                    o.reverse()
                } else {
                    o
                }
            }
            None => a.get(i..i + la).cmp(&b.get(j..j + lb)),
        };
        if o != Ordering::Equal {
            return o;
        }
        i += la;
        j += lb;
    }
}

/// An entry orders by its key's bytes, then by id; in an index holding a
/// collated part ([`Chunked`]'s collation set), part by part.
impl Entry for KeyEntry {
    #[inline]
    fn order(&self, other: &Self, coll: Option<Collation>) -> Ordering {
        match coll {
            None => self.0.cmp(&other.0),
            Some(_) => cmp_keys(&self.0, &other.0),
        }
        .then(self.1.cmp(&other.1))
    }
}

/// The range of one part's values a filter's comparisons leave, in the
/// order the part's bytes stand in -- a descending part's the other way
/// round.
pub struct PartRange {
    pub lo: Bound<Vec<u8>>,
    pub hi: Bound<Vec<u8>>,
}

/// The index: its parts, and every row's key.
#[cfg(feature = "composite")]
pub struct CompositeIndex {
    parts: Box<[Part]>,
    keys: Chunked<KeyEntry>,
    /// The rows a part of holds a value the order has no place for,
    /// ascending. Their entries are among the keys too, for a unique index
    /// to find by their bytes; while any is held no walk answers.
    apart: Vec<DocId>,
    /// The bytes the keys take, kept as a running sum: `memory_bytes` is
    /// read before every write under `--max-memory`.
    heap: usize,
}

#[cfg(feature = "composite")]
impl CompositeIndex {
    pub fn new(parts: Vec<Part>) -> CompositeIndex {
        // The collation passed in says only that the keys compare part by
        // part; each part's own is in its tag.
        let collated = parts.iter().find_map(|p| p.coll);
        CompositeIndex {
            parts: parts.into(),
            keys: Chunked::new(collated),
            apart: Vec::new(),
            heap: 0,
        }
    }

    pub fn parts(&self) -> &[Part] {
        &self.parts
    }

    /// Built from every row's key at once, handed in id order: natively
    /// sorted once and cut into chunks, a million keys of an int and a time
    /// in ... ms against ... inserted one at a time; the browser module
    /// inserts them, a sort of these entries being code of its own there.
    pub fn build(parts: Vec<Part>, rows: Vec<(DocId, RowKey)>) -> CompositeIndex {
        let mut ix = CompositeIndex::new(parts);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let coll = ix.parts.iter().find_map(|p| p.coll);
            let mut entries: Vec<KeyEntry> = Vec::with_capacity(rows.len());
            for (id, k) in rows {
                if k.apart {
                    ix.apart.push(id);
                }
                ix.heap += k.bytes.heap();
                entries.push((k.bytes, id));
            }
            ix.apart.sort_unstable();
            entries.sort_unstable_by(|a, b| a.order(b, coll));
            ix.keys = Chunked::from_sorted(entries, coll);
        }
        #[cfg(target_arch = "wasm32")]
        for (id, k) in rows {
            ix.insert(id, k);
        }
        ix
    }

    /// The key of a row whose parts hold `vals`, in order.
    pub fn key(&self, vals: &[&Value]) -> RowKey {
        let mut out = Vec::with_capacity(16 * vals.len());
        let (mut apart, mut null) = (false, false);
        for (p, v) in self.parts.iter().zip(vals) {
            null |= v.is_null();
            apart |= put_part(&mut out, p, v);
        }
        RowKey {
            bytes: KeyBytes::new(&out),
            apart,
            null,
        }
    }

    pub fn insert(&mut self, id: DocId, k: RowKey) {
        let len = k.bytes.heap();
        if k.apart {
            if let Err(at) = self.apart.binary_search(&id) {
                self.apart.insert(at, id);
            }
        }
        if self.keys.insert((k.bytes, id)) {
            self.heap += len;
        }
    }

    /// Takes out the entry [`Self::insert`] put in for the same key.
    pub fn remove(&mut self, id: DocId, k: RowKey) {
        let len = k.bytes.heap();
        if k.apart {
            if let Ok(at) = self.apart.binary_search(&id) {
                self.apart.remove(at);
            }
        }
        if self.keys.remove(&(k.bytes, id)) {
            self.heap -= len;
        }
    }

    /// The rows holding a value a part has no place for in its order.
    pub fn apart(&self) -> &[DocId] {
        &self.apart
    }

    pub fn len(&self) -> usize {
        self.keys.len
    }

    pub fn is_empty(&self) -> bool {
        self.keys.len == 0
    }

    /// Allocated bytes, estimated as an ordered index's are: the entries a
    /// quarter again, the keys too long for them, and the rows apart.
    pub fn memory_bytes(&self) -> usize {
        self.keys.len * std::mem::size_of::<KeyEntry>() * 5 / 4
            + self.heap
            + self.apart.capacity() * 8
    }

    /// The rows whose key is `key`, ascending: what a unique index asks
    /// before a write.
    pub fn holders(&self, key: &[u8]) -> Vec<DocId> {
        let lo = Bound::Included((KeyBytes::new(key), 0));
        let hi = Bound::Included((KeyBytes::new(key), DocId::MAX));
        self.keys.range(&lo, &hi).map(|e| e.1).collect()
    }

    /// Two rows holding one key with no `null` in it: what `create index
    /// ... @unique` over the documents there is refused for, naming them.
    pub fn shared(&self) -> Option<(DocId, DocId)> {
        let mut last: Option<&KeyEntry> = None;
        for e in self.keys.iter() {
            if let Some(l) = last {
                if l.0 == e.0 && !self.has_null(&e.0) {
                    return Some((l.1, e.1));
                }
            }
            last = Some(e);
        }
        None
    }

    fn has_null(&self, k: &[u8]) -> bool {
        let mut at = 0;
        while at < k.len() {
            if k[at] == NULL || k[at] == !NULL {
                return true;
            }
            at += part_len(k, at);
        }
        false
    }

    /// Part `part` of a key for a literal `v` a filter compares it with,
    /// as the row it equals is keyed, a descending part's inverted --
    /// `None` where the part's keys cannot say it exactly, for the
    /// comparison to be tested row by row: a value of another type than the
    /// part's (as `SortedIndex::bound` has it), and `null`.
    pub fn literal(&self, part: usize, v: &Value) -> Option<Vec<u8>> {
        const EXACT: u64 = 1 << 53;
        let p = self.parts.get(part)?;
        // An int and a timestamp meet as two f64s, exact up to 2^53; a
        // timestamp meets a text it parses; a float meets an int as an
        // f64 -- as `cmp_value` has them, and `SortedIndex::bound`.
        let v = match (p.kind, v) {
            (Kind::Int, Value::Int(_)) => v.clone(),
            (Kind::Int | Kind::Time, Value::Int(i) | Value::Timestamp(i))
                if i.unsigned_abs() <= EXACT =>
            {
                v.clone()
            }
            (Kind::Time, Value::Text(t)) => Value::Timestamp(crate::time::parse(t).ok()?),
            (Kind::Float, Value::Float(f)) if !f.is_nan() => v.clone(),
            (Kind::Float, Value::Int(i)) => Value::Float(*i as f64),
            (Kind::Bool, Value::Bool(_)) | (Kind::Text, Value::Text(_)) => v.clone(),
            (Kind::Json, v)
                if matches!(json_slot(Some(v)), JsonSlot::Num(_) | JsonSlot::Text(_)) =>
            {
                v.clone()
            }
            _ => return None,
        };
        let mut out = Vec::new();
        put_part(&mut out, p, &v);
        Some(out)
    }

    /// Part `part`'s type as `SortedIndex::by_kind` reads it: a json part,
    /// whose values are of every kind -- and a part of a type the parts key
    /// as json -- says nothing of them.
    pub fn part_type(&self, part: usize) -> Option<DataType> {
        Some(match self.parts.get(part)?.kind {
            Kind::Int => DataType::Int,
            Kind::Time => DataType::Timestamp,
            Kind::Float => DataType::Float,
            Kind::Bool => DataType::Bool,
            Kind::Text => DataType::Text,
            Kind::Json => return None,
        })
    }

    /// Narrows `r`, the range on part `part`, by `field <op> v`; `false`,
    /// narrowing nothing, where the part's keys cannot say `v` exactly.
    pub fn narrow(
        &self,
        r: &mut PartRange,
        part: usize,
        op: crate::query::CmpOp,
        v: &Value,
    ) -> bool {
        use crate::query::CmpOp::*;
        let Some(x) = self.literal(part, v) else {
            return false;
        };
        let desc = self.parts[part].desc;
        let (low, high) = match op {
            Eq => (Some(Bound::Included(x.clone())), Some(Bound::Included(x))),
            Ge => (Some(Bound::Included(x)), None),
            Gt => (Some(Bound::Excluded(x)), None),
            Le => (None, Some(Bound::Included(x))),
            Lt => (None, Some(Bound::Excluded(x))),
            Ne => return false,
        };
        // A descending part's bytes stand the other way round: a lower
        // bound on its values is an upper one on its bytes.
        let (to_lo, to_hi) = match desc {
            false => (low, high),
            true => (high, low),
        };
        if let Some(b) = to_lo {
            if tighter(&b, &r.lo, true) {
                r.lo = b;
            }
        }
        if let Some(b) = to_hi {
            if tighter(&b, &r.hi, false) {
                r.hi = b;
            }
        }
        true
    }

    /// The span of the entries whose first parts are `prefix`, the parts
    /// equalities fix, laid end to end as the keys hold them, and whose
    /// next part lies in `range` -- with no range, every entry of the
    /// prefix, its `null`s in that part among them; with one, none of them,
    /// as no comparison takes a `null`.
    pub fn span(&self, prefix: &[u8], part: usize, range: Option<&PartRange>) -> Span {
        let with = |tail: &[&[u8]], id: DocId| {
            let mut k = prefix.to_vec();
            for t in tail {
                k.extend_from_slice(t);
            }
            (KeyBytes::new(&k), id)
        };
        let Some(r) = range else {
            return (
                Bound::Included(with(&[], 0)),
                Bound::Included(with(&[&[TOP]], DocId::MAX)),
            );
        };
        let desc = self.parts.get(part).is_some_and(|p| p.desc);
        let lo = match &r.lo {
            Bound::Included(x) => Bound::Included(with(&[x], 0)),
            Bound::Excluded(x) => Bound::Excluded(with(&[x, &[TOP]], DocId::MAX)),
            // An ascending part's nulls stand first: the range starts past
            // them.
            Bound::Unbounded if !desc => Bound::Included(with(&[&[ABOVE_NULL]], 0)),
            Bound::Unbounded => Bound::Included(with(&[], 0)),
        };
        let hi = match &r.hi {
            Bound::Included(x) => Bound::Included(with(&[x, &[TOP]], DocId::MAX)),
            Bound::Excluded(x) => Bound::Excluded(with(&[x], 0)),
            // A descending part's nulls stand last: it ends before them.
            Bound::Unbounded if desc => Bound::Excluded(with(&[&[!ABOVE_NULL]], 0)),
            Bound::Unbounded => Bound::Included(with(&[&[TOP]], DocId::MAX)),
        };
        (lo, hi)
    }

    /// How many entries a span holds: two searches and the chunks between.
    pub fn count(&self, s: &Span) -> usize {
        self.keys.count_within(&s.0, &s.1)
    }

    /// The ids of a span's entries onto `out`, `false` once more than `cap`
    /// would be there.
    pub fn ids(&self, s: &Span, cap: usize, out: &mut Vec<DocId>) -> bool {
        for e in self.keys.range(&s.0, &s.1) {
            if out.len() >= cap {
                return false;
            }
            out.push(e.1);
        }
        true
    }

    /// The ids of the entries whose key starts with `prefix` -- whole
    /// parts, each part's bytes ending where it does -- in the keys' order,
    /// handed to `emit` until it returns `false`: a `lookup`'s parent's
    /// children. One search, the run read until a key leaves it, where a
    /// span searches for its end as well: over a key of three ints, each
    /// search a score of comparisons of keys on the heap, the second was a
    /// fifth of a page of 20 x 3 x 5.
    pub fn run(
        &self,
        prefix: &[u8],
        emit: &mut dyn FnMut(DocId) -> crate::error::Result<bool>,
    ) -> crate::error::Result<()> {
        let lo = Bound::Included((KeyBytes::new(prefix), 0));
        for e in self.keys.range(&lo, &Bound::Unbounded) {
            if !e.0.starts_with(prefix) || !emit(e.1)? {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(feature = "composite")]
impl CompositeIndex {
    /// Walks the spans' ids in the order their keys' parts from `fixed` to
    /// `ties` put them -- the parts an `order` reads past the ones the
    /// equalities fix -- or the other way round (`back`), calling `emit`
    /// until it returns `false`. Rows equal in those parts are a tie, and
    /// come out in ascending id either way, the order the scan leaves them
    /// in. Several spans -- the prefixes an `in` makes, `owner in [..] order
    /// created desc` -- are merged: each step takes, of every span's next
    /// entry, those whose order parts come first, and every entry after
    /// them in its span holding the same, a tie put in id order. Gathered
    /// and sorted instead, a feed over ten owners read and sorted all their
    /// rows for a page of twenty. One span read forwards over every part
    /// is in that order as it stands, ties and all.
    pub fn walk(
        &self,
        spans: &[Span],
        fixed: usize,
        back: bool,
        ties: usize,
        emit: &mut dyn FnMut(DocId) -> crate::error::Result<bool>,
    ) -> crate::error::Result<()> {
        if let ([s], false, true) = (spans, back, ties >= self.parts.len()) {
            for e in self.keys.range(&s.0, &s.1) {
                if !emit(e.1)? {
                    break;
                }
            }
            return Ok(());
        }
        let collated = self.parts.iter().any(|p| p.coll.is_some());
        let order = |k: &[u8]| -> std::ops::Range<usize> { head_len(k, fixed)..head_len(k, ties) };
        let cmp = |a: &[u8], b: &[u8]| match collated {
            true => cmp_keys(a, b),
            false => a.cmp(b),
        };
        let mut cursors: Vec<std::iter::Peekable<Box<dyn Iterator<Item = &KeyEntry>>>> = spans
            .iter()
            .map(|s| {
                let r = self.keys.range(&s.0, &s.1);
                let it: Box<dyn Iterator<Item = &KeyEntry>> = match back {
                    true => Box::new(r.rev()),
                    false => Box::new(r),
                };
                it.peekable()
            })
            .collect();
        let mut run: Vec<DocId> = Vec::new();
        loop {
            let mut best: Option<&[u8]> = None;
            for c in cursors.iter_mut() {
                let Some(&e) = c.peek() else {
                    continue;
                };
                let o = &e.0[order(&e.0)];
                let first = match best {
                    None => true,
                    Some(b) => match back {
                        false => cmp(o, b) == Ordering::Less,
                        true => cmp(o, b) == Ordering::Greater,
                    },
                };
                if first {
                    best = Some(o);
                }
            }
            let Some(b) = best else {
                return Ok(());
            };
            for c in cursors.iter_mut() {
                while let Some(&e) = c.peek() {
                    if cmp(&e.0[order(&e.0)], b) != Ordering::Equal {
                        break;
                    }
                    run.push(e.1);
                    c.next();
                }
            }
            if !flush(&mut run, emit)? {
                return Ok(());
            }
        }
    }
}

/// A tie's ids handed over in ascending id; `false` once `emit` stopped.
fn flush(
    run: &mut Vec<DocId>,
    emit: &mut dyn FnMut(DocId) -> crate::error::Result<bool>,
) -> crate::error::Result<bool> {
    run.sort_unstable();
    for &id in run.iter() {
        if !emit(id)? {
            return Ok(false);
        }
    }
    run.clear();
    Ok(true)
}

/// Whether bound `b` admits less than `old` on its side -- the lower side
/// when `lower`, in the bytes' order.
fn tighter(b: &Bound<Vec<u8>>, old: &Bound<Vec<u8>>, lower: bool) -> bool {
    let (x, y) = match (b, old) {
        (_, Bound::Unbounded) => return true,
        (Bound::Unbounded, _) => return false,
        (Bound::Included(x) | Bound::Excluded(x), Bound::Included(y) | Bound::Excluded(y)) => {
            (x, y)
        }
    };
    let o = cmp_keys(x, y);
    let o = match o {
        Ordering::Equal => {
            // At one key an exclusive bound admits less.
            return matches!(b, Bound::Excluded(_)) && matches!(old, Bound::Included(_));
        }
        o => o,
    };
    (o == Ordering::Greater) == lower
}

#[cfg(not(feature = "composite"))]
pub use crate::off::CompositeIndex;

#[cfg(all(test, feature = "composite"))]
mod tests {
    use super::*;

    fn ix(parts: &[(Kind, bool)]) -> CompositeIndex {
        CompositeIndex::new(
            parts
                .iter()
                .map(|&(kind, desc)| Part {
                    kind,
                    desc,
                    coll: None,
                    fold: None,
                })
                .collect(),
        )
    }

    /// Keys order as their values do, part by part, a descending part the
    /// other way round, `null` lowest, through every kind -- against
    /// `cmp_value` over the same tuples.
    #[test]
    fn keys_order_as_their_values() {
        let vals = [
            Value::Null,
            Value::Int(-5),
            Value::Int(0),
            Value::Int(7),
            Value::Int(i64::MAX),
        ];
        let texts = [
            Value::Null,
            Value::Text(String::new()),
            Value::Text("a".into()),
            Value::Text("a\0".into()),
            Value::Text("a\0b".into()),
            Value::Text("ab".into()),
            Value::Text("b".into()),
        ];
        for desc in [(false, false), (false, true), (true, false), (true, true)] {
            let i = ix(&[(Kind::Int, desc.0), (Kind::Text, desc.1)]);
            let mut rows = Vec::new();
            for a in &vals {
                for b in &texts {
                    rows.push((a.clone(), b.clone(), i.key(&[a, b]).bytes));
                }
            }
            for x in &rows {
                for y in &rows {
                    let mut want = x.0.cmp_value(&y.0);
                    if desc.0 {
                        want = want.reverse();
                    }
                    let mut second = x.1.cmp_value(&y.1);
                    if desc.1 {
                        second = second.reverse();
                    }
                    let want = want.then(second);
                    assert_eq!(x.2.cmp(&y.2), want, "{x:?} {y:?} {desc:?}");
                    assert_eq!((*x.2).cmp(&*y.2), want, "{x:?} {y:?} {desc:?}");
                    assert_eq!(cmp_keys(&x.2, &y.2), want, "{x:?} {y:?} {desc:?}");
                }
            }
        }
    }

    #[test]
    fn an_entry_is_32_bytes() {
        assert_eq!(std::mem::size_of::<KeyEntry>(), 32);
        let long = [7u8; 40];
        assert_eq!(&*KeyBytes::new(&long), &long[..]);
        assert_eq!(&*KeyBytes::new(&long[..INLINE]), &long[..INLINE]);
        assert!(KeyBytes::new(&long[..3]) < KeyBytes::new(&long));
        // Held in the entry, they compare as their bytes do: a start of
        // another below it, a zero byte above the end.
        let keys: Vec<Vec<u8>> = vec![
            vec![],
            vec![0],
            vec![0, 0],
            vec![0, 1],
            vec![1],
            vec![1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            vec![1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            vec![255; 21],
            vec![255; 22],
            vec![255; 23],
            [vec![255; 16], vec![0; 6]].concat(),
            [vec![255; 16], vec![0; 5]].concat(),
        ];
        for a in &keys {
            for b in &keys {
                assert_eq!(
                    KeyBytes::new(a).cmp(&KeyBytes::new(b)),
                    a.cmp(b),
                    "{a:?} {b:?}"
                );
            }
        }
    }

    #[test]
    fn a_collated_part_compares_in_its_collation() {
        for desc in [false, true] {
            let i = CompositeIndex::new(vec![
                Part {
                    kind: Kind::Text,
                    desc,
                    coll: Some(Collation::Turkish),
                    fold: None,
                },
                Part {
                    kind: Kind::Int,
                    desc: false,
                    coll: None,
                    fold: None,
                },
            ]);
            let words = ["", "ağaç", "Ağaç", "çay", "cay", "zeytin", "ılık", "ilik"];
            let mut keys = Vec::new();
            for w in words {
                for n in [-1, 4] {
                    keys.push((w, n, i.key(&[&Value::Text(w.into()), &Value::Int(n)]).bytes));
                }
            }
            keys.push(("", 0, i.key(&[&Value::Null, &Value::Int(0)]).bytes));
            for x in &keys {
                for y in &keys {
                    let null = |k: &(&str, i64, KeyBytes)| k.2[0] == NULL || k.2[0] == !NULL;
                    let first = match (null(x), null(y)) {
                        (true, true) => Ordering::Equal,
                        (true, false) => Ordering::Less,
                        (false, true) => Ordering::Greater,
                        _ => Collation::Turkish.compare(x.0, y.0),
                    };
                    let first = if desc { first.reverse() } else { first };
                    let want = first.then(x.1.cmp(&y.1));
                    assert_eq!(
                        cmp_keys(&x.2, &y.2),
                        want,
                        "{:?} {:?}",
                        (x.0, x.1),
                        (y.0, y.1)
                    );
                }
            }
        }
    }
}

#[cfg(all(test, feature = "composite"))]
mod build_tests {
    use super::*;

    /// Built at once, the index holds what inserting each row would: the
    /// same entries, the rows apart among them.
    #[test]
    fn a_build_holds_what_inserts_would() {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let parts = vec![
            Part {
                kind: Kind::Int,
                desc: false,
                coll: None,
                fold: None,
            },
            Part {
                kind: Kind::Float,
                desc: true,
                coll: None,
                fold: None,
            },
            Part {
                kind: Kind::Text,
                desc: false,
                coll: None,
                fold: None,
            },
        ];
        let mut one = CompositeIndex::new(parts.clone());
        let mut rows = Vec::new();
        for id in 1..20_000u64 {
            let a = Value::Int((next() % 50) as i64);
            let b = match next() % 9 {
                0 => Value::Null,
                1 => Value::Float(f64::NAN),
                n => Value::Float(n as f64 / 4.0),
            };
            let c = Value::Text(format!("t{}", next() % 300).repeat((next() % 4) as usize));
            let k = one.key(&[&a, &b, &c]);
            rows.push((id, k.clone()));
            one.insert(id, k);
        }
        let all = CompositeIndex::build(parts, rows);
        assert!(all.keys.iter().eq(one.keys.iter()));
        assert_eq!(all.apart, one.apart);
        assert_eq!(all.memory_bytes(), one.memory_bytes());
    }
}
