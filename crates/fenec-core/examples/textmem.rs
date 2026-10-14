//! What a text index holds in memory, counted by the allocator: `make
//! text-memory`.
//!
//! ```text
//! cargo run --release -p fenec-core --example textmem -- [ROWS] [WORDS]
//! cargo run --release -p fenec-core --example textmem -- beir <dataset dir>...
//! ```
//!
//! A tenant's notes: `ROWS` texts (200) of `WORDS` words (30), drawn from a
//! Zipf vocabulary of 20 000 made-up words and then made up each, as
//! `fenec-server --dir`'s tenants are measured in Docker. Or a BEIR
//! dataset's corpus, title and text. Each is indexed as a first read builds
//! it -- every document inserted, then shrunk to fit -- and what the
//! allocator holds after against before is printed: the bytes asked for,
//! the allocations, the bytes musl's mallocng gives them (its slots, a
//! static binary's allocator), and `TextIndex::memory_bytes`, which
//! `--max-memory`, the quotas and `/_admin/stats` read.

use fenec_core::schema::TextIndexSpec;
use fenec_core::text::TextIndex;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;

static HEAP: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static MUSL: AtomicUsize = AtomicUsize::new(0);

/// The slot musl's mallocng gives a request of `n` bytes: its size classes
/// in 16-byte units, 4 bytes of header in the slot, a page-rounded map past
/// 128 KiB.
fn musl(n: usize) -> usize {
    const C: [usize; 48] = [
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 15, 18, 20, 25, 31, 36, 42, 50, 63, 72, 84, 102, 127,
        146, 170, 204, 255, 292, 340, 409, 511, 584, 682, 818, 1023, 1169, 1364, 1637, 2047, 2340,
        2730, 3276, 4095, 4680, 5460, 6552, 8191,
    ];
    if n > 131_052 {
        return (n + 4).div_ceil(4096) * 4096;
    }
    let mut u = (n + 3) >> 4;
    if u < 10 {
        return C[u] * 16;
    }
    u += 1;
    let mut i = (28 - (u as u32).leading_zeros() as usize) * 4 + 8;
    if u > C[i + 1] {
        i += 2;
    }
    if u > C[i] {
        i += 1;
    }
    C[i] * 16
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            HEAP.fetch_add(l.size(), Ordering::Relaxed);
            LIVE.fetch_add(1, Ordering::Relaxed);
            MUSL.fetch_add(musl(l.size()), Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        HEAP.fetch_sub(l.size(), Ordering::Relaxed);
        LIVE.fetch_sub(1, Ordering::Relaxed);
        MUSL.fetch_sub(musl(l.size()), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// A word of 3 to 9 letters.
    fn word(&mut self) -> String {
        let n = 3 + self.below(7);
        (0..n)
            .map(|_| (b'a' + self.below(26) as u8) as char)
            .collect()
    }
}

fn counts() -> (usize, usize, usize) {
    (
        HEAP.load(Ordering::Relaxed),
        LIVE.load(Ordering::Relaxed),
        MUSL.load(Ordering::Relaxed),
    )
}

/// `texts` indexed, and what that holds.
fn measure(what: &str, texts: &[String]) {
    let data: usize = texts.iter().map(String::len).sum();
    let before = counts();
    let mut ix = TextIndex::new(TextIndexSpec::default());
    for (i, t) in texts.iter().enumerate() {
        ix.insert(i as u64 + 1, t);
    }
    ix.shrink_to_fit();
    let after = counts();
    println!(
        "{what}: {} texts, {} KB: {} terms, {} postings",
        texts.len(),
        data / 1024,
        ix.terms(),
        ix.postings_count()
    );
    println!(
        "  {} KB asked for in {} allocations, {} KB in musl's slots; memory_bytes {} KB",
        (after.0 - before.0) / 1024,
        after.1 - before.1,
        (after.2 - before.2) / 1024,
        ix.memory_bytes() / 1024
    );
}

/// A BEIR corpus's texts, each its title and its text.
fn corpus(dir: &str) -> Vec<String> {
    use fenec_core::value::Value;
    let field = |o: &Value, key: &str| match o {
        Value::Object(o) => o
            .iter()
            .find(|(k, _)| k.as_str() == key)
            .and_then(|(_, v)| match v {
                Value::Text(t) => Some(t.clone()),
                _ => None,
            })
            .unwrap_or_default(),
        _ => String::new(),
    };
    let text = std::fs::read_to_string(format!("{dir}/corpus.jsonl")).expect("corpus.jsonl");
    text.lines()
        .filter_map(|l| fenec_core::json::parse_json(l).ok())
        .map(|o| format!("{} {}", field(&o, "title"), field(&o, "text")))
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("beir") {
        for dir in &args[1..] {
            let texts = corpus(dir);
            measure(dir, &texts);
        }
        return;
    }
    let rows: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(200);
    let words: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let vocab: Vec<String> = (0..20_000).map(|_| rng.word()).collect();
    // Rank r drawn with weight 1/(r+1).
    let h: f64 = (1..=vocab.len()).map(|r| 1.0 / r as f64).sum();
    let mut cdf = Vec::with_capacity(vocab.len());
    let mut acc = 0.0;
    for r in 1..=vocab.len() {
        acc += 1.0 / r as f64 / h;
        cdf.push(acc);
    }
    let zipf = |rng: &mut Rng| {
        let u = rng.unit();
        vocab[cdf.partition_point(|&c| c < u).min(vocab.len() - 1)].clone()
    };
    let texts: Vec<String> = (0..rows)
        .map(|_| {
            (0..words)
                .map(|_| zipf(&mut rng))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    measure("a Zipf vocabulary", &texts);
    let texts: Vec<String> = (0..rows)
        .map(|_| (0..words).map(|_| rng.word()).collect::<Vec<_>>().join(" "))
        .collect();
    measure("made-up words", &texts);
}
