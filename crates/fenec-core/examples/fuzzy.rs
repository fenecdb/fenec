//! What `match ... fuzzy` and `match ... prefix` cost: `make fuzzy-bench`.
//!
//! 100 000 documents of 30 words over a vocabulary of made-up words --
//! syllables put together, so that as in a language many words are an edit
//! or two from another and many begin another -- the commoner words more
//! common. Each query is timed as the median of `RUNS` over 200 queries:
//!
//! - the sorted words' build, the first loose `match` after the writes, and
//!   what they hold;
//! - the walk alone: the words near one of 2 000 query words, a typo in
//!   each, at one edit and two, and the words a word begins;
//! - `match` plain, two words as written; `fuzzy` over the same two words
//!   a typo in one; `prefix` with the last word half typed; both.
//!
//! With a BEIR dataset's directory after the count, the walk is timed over
//! its words as well: `make fuzzy-bench BEIR=<dir>`.

use fenec_core::fuzzy::{typos, Dictionary};
use fenec_core::prelude::*;
use std::time::Instant;

const RUNS: usize = 11;

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
    /// A rank, the first ranks most often.
    fn zipf(&mut self, n: usize) -> usize {
        let u = (self.next() % 1_000_000) as f64 / 1_000_000.0;
        ((u * u * u) * n as f64) as usize
    }
}

const ONSETS: [&str; 43] = [
    "", "b", "c", "d", "f", "g", "h", "j", "k", "l", "m", "n", "p", "r", "s", "t", "v", "w", "z",
    "br", "cr", "dr", "fr", "gr", "pr", "tr", "bl", "cl", "fl", "gl", "pl", "sl", "sp", "st", "sh",
    "ch", "th", "wh", "sc", "sk", "sm", "sn", "sw",
];
const VOWELS: [&str; 12] = [
    "a", "e", "i", "o", "u", "ai", "ea", "ee", "ie", "oo", "ou", "y",
];
const CODAS: [&str; 17] = [
    "", "", "", "n", "r", "s", "t", "l", "m", "nd", "nt", "st", "rt", "ng", "ck", "sh", "th",
];

/// A made-up word of one to four syllables.
fn word(rng: &mut Rng) -> String {
    let mut w = String::new();
    for _ in 0..1 + rng.below(4) {
        w.push_str(ONSETS[rng.below(ONSETS.len())]);
        w.push_str(VOWELS[rng.below(VOWELS.len())]);
        w.push_str(CODAS[rng.below(CODAS.len())]);
    }
    w
}

/// `w` with a typo past its first letter: dropped, doubled, changed, or
/// swapped with the next.
fn typo(rng: &mut Rng, w: &str) -> String {
    let mut c: Vec<char> = w.chars().collect();
    if c.len() < 2 {
        return w.to_string();
    }
    let at = 1 + rng.below(c.len() - 1);
    match rng.below(4) {
        0 => {
            c.remove(at);
        }
        1 => c.insert(at, c[at]),
        2 => c[at] = (b'a' + rng.below(26) as u8) as char,
        _ if at + 1 < c.len() => c.swap(at, at + 1),
        _ => c.swap(at - 1, at),
    }
    c.into_iter().collect()
}

fn median(mut t: Vec<f64>) -> f64 {
    t.sort_by(|a, b| a.total_cmp(b));
    t[t.len() / 2]
}

/// Median milliseconds a query, over every query once a run.
fn time(db: &Database, sql: &str, queries: &[String]) -> Option<f64> {
    let stmt = fenec_ql::parse_one(sql).ok()?;
    let run = |db: &Database| -> Option<()> {
        for q in queries {
            std::hint::black_box(db.query(&stmt, &[Value::Text(q.clone())]).ok()?);
        }
        Some(())
    };
    run(db)?;
    let t = (0..RUNS)
        .map(|_| {
            let s = Instant::now();
            run(db);
            s.elapsed().as_secs_f64() * 1e3 / queries.len() as f64
        })
        .collect();
    Some(median(t))
}

fn show(what: &str, ms: Option<f64>) {
    match ms {
        Some(ms) => println!("  {what:<58} {ms:>9.4} ms"),
        None => println!("  {what:<58} {:>9}", "n/a"),
    }
}

/// The walk alone over `words`: median microseconds a query word, for
/// each bound, and how many words each found.
fn walks(words: &[&str], label: &str) {
    let t = Instant::now();
    let dict = Dictionary::of(words.iter().copied());
    let built = t.elapsed().as_secs_f64() * 1e3;
    println!(
        "{label}: {} words, sorted in {built:.1} ms into {:.1} MB",
        dict.len(),
        dict.bytes() as f64 / 1e6
    );
    let mut rng = Rng(0x5EED);
    let mut asked: [Vec<String>; 3] = Default::default();
    while asked.iter().any(|a| a.len() < 2_000) {
        let w = words[rng.zipf(words.len())];
        let q = typo(&mut rng, w);
        let k = typos(q.chars().count()) as usize;
        if asked[k].len() < 2_000 {
            asked[k].push(q);
        }
    }
    let mut out = Vec::new();
    for (k, prefix, qs, what) in [
        (1u8, false, &asked[1], "fuzzy, one edit (5 to 8 letters)"),
        (2, false, &asked[2], "fuzzy, two edits (9 letters on)"),
        (0, true, &asked[0], "prefix, under 5 letters"),
        (1, true, &asked[1], "prefix fuzzy, one edit"),
    ] {
        let mut found = 0;
        let mut refused = 0;
        let t = (0..RUNS)
            .map(|_| {
                found = 0;
                refused = 0;
                let s = Instant::now();
                for q in qs {
                    out.clear();
                    if !dict.near(q, k, prefix, 2_000, &mut out) {
                        refused += 1;
                    }
                    found += out.len();
                }
                s.elapsed().as_secs_f64() * 1e6 / qs.len() as f64
            })
            .collect();
        println!(
            "  {what:<40} {:>9.1} us a word, {:.1} words found, {refused} refused",
            median(t),
            found as f64 / qs.len() as f64
        );
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let n: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(100_000);
    let beir = args.next();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut vocab: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    while vocab.len() < 60_000 {
        let w = word(&mut rng);
        if w.len() > 1 && seen.insert(w.clone()) {
            vocab.push(w);
        }
    }
    let words: Vec<&str> = vocab.iter().map(String::as_str).collect();
    walks(&words, "made-up words");
    if let Some(dir) = &beir {
        let corpus = std::fs::read_to_string(format!("{dir}/corpus.jsonl")).expect("corpus.jsonl");
        let mut terms: Vec<String> = Vec::new();
        let mut held = std::collections::HashSet::new();
        for line in corpus.lines() {
            for t in fenec_core::text::tokenize(line) {
                if held.insert(t.clone()) {
                    terms.push(t);
                }
            }
        }
        let words: Vec<&str> = terms.iter().map(String::as_str).collect();
        walks(&words, dir);
    }

    let mut db = Database::new();
    db.execute(&fenec_ql::parse_one("create collection d (body text @text)").unwrap())
        .unwrap();
    let mut batch: Vec<Vec<(String, Expr)>> = Vec::new();
    for i in 0..n {
        let body: Vec<&str> = (0..30).map(|_| vocab[rng.zipf(vocab.len())].as_str()).collect();
        batch.push(vec![("body".into(), Expr::Lit(Value::Text(body.join(" "))))]);
        if batch.len() == 5_000 || i + 1 == n {
            db.execute(&Statement::Put {
                collection: "d".into(),
                docs: std::mem::take(&mut batch),
                insert: false,
                if_absent: false,
                docs_param: None,
                else_set: None,
                require: None,
                pick: None,
            })
            .unwrap();
        }
    }
    let stats = |db: &Database| db.collection("d").unwrap().stats().text_indexes[0].clone();
    let t = Instant::now();
    db.query(
        &fenec_ql::parse_one("get d match body \"x\" limit 1").unwrap(),
        &[],
    )
    .unwrap();
    let index = t.elapsed().as_secs_f64() * 1e3;
    let before = stats(&db);
    println!(
        "{n} documents: the text index built in {index:.0} ms, {} terms, {:.1} MB",
        before.terms,
        before.bytes as f64 / 1e6
    );
    let t = Instant::now();
    let first = db.query(
        &fenec_ql::parse_one("get d match body \"blorkatz\" fuzzy limit 1").unwrap(),
        &[],
    );
    if first.is_ok() {
        let after = stats(&db);
        println!(
            "  the words sorted by the first loose match in {:.1} ms, {:.1} MB more",
            t.elapsed().as_secs_f64() * 1e3,
            (after.bytes - before.bytes) as f64 / 1e6
        );
    }

    // Two words a query; a typo in one of them; the last half typed.
    let mut plain = Vec::new();
    let mut typod = Vec::new();
    let mut half = Vec::new();
    for _ in 0..200 {
        let a = vocab[rng.zipf(vocab.len())].clone();
        let b = vocab[rng.zipf(vocab.len())].clone();
        plain.push(format!("{a} {b}"));
        typod.push(match rng.below(2) {
            0 => format!("{} {b}", typo(&mut rng, &a)),
            _ => format!("{a} {}", typo(&mut rng, &b)),
        });
        let cut: String = b.chars().take(b.chars().count().div_ceil(2).max(3)).collect();
        half.push(format!("{a} {cut}"));
    }
    let q = |opts: &str| format!("get d match body $1{opts} limit 10");
    show("match, two words", time(&db, &q(""), &plain));
    show("match fuzzy, two words as written", time(&db, &q(" fuzzy"), &plain));
    show("match, a typo", time(&db, &q(""), &typod));
    show("match fuzzy, a typo", time(&db, &q(" fuzzy"), &typod));
    show("match fuzzy 1, a typo", time(&db, &q(" fuzzy 1"), &typod));
    show("match prefix, the last word half typed", time(&db, &q(" prefix"), &half));
    show(
        "match prefix fuzzy, the last word half typed",
        time(&db, &q(" prefix fuzzy"), &half),
    );
}
