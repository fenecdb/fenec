//! Retrieval quality on BEIR: `make beir BEIR=<dataset dir>`
//!
//! Needs a BEIR dataset -- `corpus.jsonl`, `queries.jsonl`, `qrels/test.tsv`
//! as the BEIR distribution ships them -- and the vectors `beir/embed.mjs`
//! writes beside it. The corpus goes into fenecdb, the text under `@text`
//! and the vector under `@hnsw`, and every way of retrieving ten documents
//! is scored by nDCG@10 over the test queries: BM25 alone, the vectors alone
//! (the graph, and an exact scan), `match` then `rerank`, and `match` with
//! `near` fused by reciprocal rank.
//!
//! A dataset with no vectors beside it -- a corpus in a language the
//! embedding model does not read -- is scored by BM25 alone, in the order of
//! `corpus.jsonl` and `queries.jsonl`.
//!
//! With the SPLADE vectors `beir/splade.mjs` writes there as well, each
//! document's goes into a `sparse<30522>` field, its `@inverted` index built
//! afterwards and timed, and `near` over it -- alone, and fused with
//! `match` -- is scored the same way.

use fenec_core::prelude::*;
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

fn read(dir: &Path, file: &str) -> String {
    std::fs::read_to_string(dir.join(file)).unwrap_or_else(|e| {
        panic!(
            "{}: {e} (run beir/embed.mjs first)",
            dir.join(file).display()
        )
    })
}

/// What `splade.mjs` writes: per text a count, the vocabulary ids, then the
/// weights, little-endian.
fn sparse_vectors(dir: &Path, file: &str) -> Option<Vec<Vec<(u32, f32)>>> {
    let bytes = std::fs::read(dir.join(file)).ok()?;
    let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let n = word(at) as usize;
        let ids = at + 4;
        let weights = ids + 4 * n;
        out.push(
            (0..n)
                .map(|k| (word(ids + 4 * k), f32::from_bits(word(weights + 4 * k))))
                .collect(),
        );
        at = weights + 4 * n;
    }
    Some(out)
}

/// SPLADE's vocabulary: BERT's WordPiece.
const VOCAB: u32 = 30_522;

fn vectors(dir: &Path, file: &str) -> Vec<f32> {
    let bytes = std::fs::read(dir.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
    let (floats, _) = bytes.as_chunks::<4>();
    floats.iter().map(|b| f32::from_le_bytes(*b)).collect()
}

/// A string field of a JSON line. BEIR's lines carry a `metadata` object
/// fenecdb's JSON -- which has no nested objects -- will not parse, so the
/// three strings wanted are read out directly.
fn json_str(line: &str, key: &str) -> String {
    let Some(at) = line.find(&format!("\"{key}\"")) else {
        return String::new();
    };
    let rest = line[at + key.len() + 2..].trim_start();
    let Some(rest) = rest.strip_prefix(':').map(str::trim_start) else {
        return String::new();
    };
    let Some(rest) = rest.strip_prefix('"') else {
        return String::new();
    };
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('b') | Some('f') => out.push(' '),
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    let code = u32::from_str_radix(&hex, 16).unwrap_or(0xfffd);
                    out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                }
                Some(other) => out.push(other),
                None => break,
            },
            c => out.push(c),
        }
    }
    out
}

/// `_id` and the text of each JSON line, the title before the text.
fn documents(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines().filter(|l| !l.is_empty()) {
        let body = format!("{} {}", json_str(line, "title"), json_str(line, "text"));
        out.insert(json_str(line, "_id"), body.trim().to_string());
    }
    out
}

/// A seeded generator, a query's own: the same typo for the same query in
/// every run.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn upto(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// The keys beside each letter on a QWERTY keyboard: what a finger lands
/// on instead of the one it meant.
fn beside(c: char) -> &'static str {
    match c {
        'q' => "wa",
        'w' => "qeas",
        'e' => "wrsd",
        'r' => "etdf",
        't' => "ryfg",
        'y' => "tugh",
        'u' => "yihj",
        'i' => "uojk",
        'o' => "ipkl",
        'p' => "ol",
        'a' => "qwsz",
        's' => "awedxz",
        'd' => "serfcx",
        'f' => "drtgvc",
        'g' => "ftyhbv",
        'h' => "gyujnb",
        'j' => "huikmn",
        'k' => "jiolm",
        'l' => "kop",
        'z' => "asx",
        'x' => "zsdc",
        'c' => "xdfv",
        'v' => "cfgb",
        'b' => "vghn",
        'n' => "bhjm",
        'm' => "njk",
        _ => "",
    }
}

/// `query` with one typo, as a person makes one (Damerau 1964): in one
/// word of four letters or more, a letter dropped, one beside it on the
/// keyboard typed as well or instead, or two letters swapped -- and in the
/// first letter only 7% of the time, as Pollock and Zamora found it in 50
/// 000 misspellings. A query with no such word is left as it is.
fn typo(query: &str, seed: u64) -> String {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut words: Vec<Vec<char>> = query.split(' ').map(|w| w.chars().collect()).collect();
    let long: Vec<usize> = (0..words.len())
        .filter(|&i| words[i].iter().filter(|c| c.is_ascii_lowercase()).count() >= 4)
        .collect();
    if long.is_empty() {
        return query.to_string();
    }
    let w = &mut words[long[rng.upto(long.len())]];
    let letters: Vec<usize> = (0..w.len())
        .filter(|&i| w[i].is_ascii_alphabetic())
        .collect();
    let at = match rng.upto(100) < 7 {
        true => letters[0],
        false => letters[1 + rng.upto(letters.len() - 1)],
    };
    let c = w[at].to_ascii_lowercase();
    let near = beside(c).as_bytes();
    let key = match near.is_empty() {
        true => c,
        false => near[rng.upto(near.len())] as char,
    };
    match rng.upto(4) {
        0 => {
            w.remove(at);
        }
        1 => w.insert(at + rng.upto(2), key),
        2 => w[at] = key,
        _ => match at + 1 < w.len() {
            true => w.swap(at, at + 1),
            false => w.swap(at - 1, at),
        },
    }
    let out: Vec<String> = words.iter().map(|w| w.iter().collect()).collect();
    out.join(" ")
}

/// `query` as a search box holds it while its last word is being typed:
/// that word cut to half its letters, rounded up.
fn half_typed(query: &str) -> String {
    let trimmed = query.trim_end_matches(|c: char| !c.is_alphanumeric());
    let start = (trimmed.char_indices().rev())
        .find(|(_, c)| !c.is_alphanumeric())
        .map_or(0, |(i, c)| i + c.len_utf8());
    let last: Vec<char> = trimmed[start..].chars().collect();
    let keep = last.len().div_ceil(2);
    format!(
        "{}{}",
        &trimmed[..start],
        last[..keep].iter().collect::<String>()
    )
}

fn ndcg10(ranked: &[String], rels: &HashMap<String, u32>) -> f64 {
    let gain = |i: usize, rel: u32| rel as f64 / (i as f64 + 2.0).log2();
    let dcg: f64 = ranked
        .iter()
        .take(10)
        .enumerate()
        .map(|(i, d)| gain(i, *rels.get(d).unwrap_or(&0)))
        .sum();
    let mut ideal: Vec<u32> = rels.values().copied().collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    let idcg: f64 = ideal
        .iter()
        .take(10)
        .enumerate()
        .map(|(i, r)| gain(i, *r))
        .sum();
    if idcg == 0.0 {
        0.0
    } else {
        dcg / idcg
    }
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("usage: beir <BEIR dataset dir>");
    let dir = Path::new(&dir);

    let corpus = read(dir, "corpus.jsonl");
    let texts = documents(&corpus);
    let dense = dir.join("corpus.f32").exists();
    let in_order = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|l| !l.is_empty())
            .map(|l| json_str(l, "_id"))
            .collect()
    };
    let ids: Vec<String> = match dense {
        true => read(dir, "corpus.ids").lines().map(String::from).collect(),
        false => in_order(&corpus),
    };
    let flat = if dense {
        vectors(dir, "corpus.f32")
    } else {
        Vec::new()
    };
    let dim = flat.len() / ids.len();

    let splade = sparse_vectors(dir, "corpus.sparse");
    if let Some(s) = &splade {
        assert_eq!(
            s.len(),
            ids.len(),
            "corpus.sparse is not corpus.ids' length"
        );
    }
    let mut db = Database::new();
    let t = Instant::now();
    let vector = match dense {
        true => format!(", v vector<{dim}> @hnsw(cosine)"),
        false => String::new(),
    };
    // `@text`'s options, to measure one: `FENECBENCH_TEXT=chars`,
    // `FENECBENCH_TEXT=prefix=6`.
    let text = match std::env::var("FENECBENCH_TEXT") {
        Ok(args) if !args.is_empty() => format!("@text({args})"),
        _ => "@text".to_string(),
    };
    db.execute(
        &fenec_ql::parse_one(&format!(
            "create collection d (doc text, body text {text}{vector}, s sparse<{VOCAB}>)"
        ))
        .unwrap(),
    )
    .unwrap();
    for (n, chunk) in ids.chunks(1_000).enumerate() {
        let docs = chunk
            .iter()
            .enumerate()
            .map(|(k, id)| {
                let mut doc = vec![
                    ("doc".to_string(), Expr::Lit(Value::Text(id.clone()))),
                    (
                        "body".to_string(),
                        Expr::Lit(Value::Text(texts[id].clone())),
                    ),
                ];
                if dense {
                    let at = (n * 1_000 + k) * dim;
                    let v = flat[at..at + dim].to_vec();
                    doc.push(("v".to_string(), Expr::Lit(Value::Vector(v))));
                }
                if let Some(s) = &splade {
                    let e = s[n * 1_000 + k].clone();
                    doc.push(("s".to_string(), Expr::Lit(Value::Sparse(VOCAB, e))));
                }
                doc
            })
            .collect();
        db.execute(&Statement::Put {
            collection: "d".into(),
            docs,
            insert: false,
            if_absent: false,
            docs_param: None,
            else_set: None,
            require: None,
            pick: None,
        })
        .unwrap();
    }
    eprintln!(
        "{}: {} documents x {dim} loaded and indexed in {:.1} s",
        dir.display(),
        ids.len(),
        t.elapsed().as_secs_f64()
    );
    for st in &db.collection("d").unwrap().stats().text_indexes {
        eprintln!(
            "the text index: {} terms, {} postings, {:.1} MB",
            st.terms,
            st.postings,
            st.bytes as f64 / 1e6
        );
    }
    if splade.is_some() {
        let t = Instant::now();
        db.execute(&fenec_ql::parse_one("create index on d (s) @inverted").unwrap())
            .unwrap();
        let d = db.collection("d").unwrap();
        let ix = d.sparse_index("s").unwrap().unwrap();
        eprintln!(
            "SPLADE: the inverted index in {:.2} s, {} postings over {} dimensions, {:.1} MB",
            t.elapsed().as_secs_f64(),
            ix.postings_count(),
            ix.dimensions(),
            ix.memory_bytes() as f64 / 1e6
        );
    }

    let mut qrels: HashMap<String, HashMap<String, u32>> = HashMap::new();
    for line in read(dir, "qrels/test.tsv").lines().skip(1) {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() == 3 {
            let rel: u32 = f[2].trim().parse().unwrap_or(0);
            if rel > 0 {
                qrels
                    .entry(f[0].to_string())
                    .or_default()
                    .insert(f[1].to_string(), rel);
            }
        }
    }
    let queries = read(dir, "queries.jsonl");
    let qids: Vec<String> = match dense {
        true => read(dir, "queries.ids").lines().map(String::from).collect(),
        false => in_order(&queries),
    };
    let qflat = if dense {
        vectors(dir, "queries.f32")
    } else {
        Vec::new()
    };
    let qsparse = sparse_vectors(dir, "queries.sparse");
    let qtext: HashMap<String, String> = queries
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| (json_str(l, "_id"), json_str(l, "text")))
        .collect();

    // Each query as typed with a typo ($4), and with its last word half
    // typed ($5), beside it as written ($1).
    let mut methods: Vec<(String, String)> = vec![(
        "match (BM25)".into(),
        "get d select doc match body $1 limit 10".into(),
    )];
    for (name, sql) in [
        (
            "match fuzzy",
            "get d select doc match body $1 fuzzy limit 10",
        ),
        (
            "match fuzzy 1",
            "get d select doc match body $1 fuzzy 1 limit 10",
        ),
        ("match, a typo", "get d select doc match body $4 limit 10"),
        (
            "match fuzzy, a typo",
            "get d select doc match body $4 fuzzy limit 10",
        ),
        (
            "match fuzzy 1, a typo",
            "get d select doc match body $4 fuzzy 1 limit 10",
        ),
        (
            "match prefix",
            "get d select doc match body $1 prefix limit 10",
        ),
        (
            "match, half typed",
            "get d select doc match body $5 limit 10",
        ),
        (
            "match prefix, half typed",
            "get d select doc match body $5 prefix limit 10",
        ),
        (
            "match prefix fuzzy, half typed",
            "get d select doc match body $5 prefix fuzzy limit 10",
        ),
    ] {
        if fenec_ql::parse_one(sql).is_ok() {
            methods.push((name.into(), sql.into()));
        }
    }
    let dense_methods = methods.len();
    methods.push((
        "near, the graph".into(),
        "get d select doc near v $2 limit 10".into(),
    ));
    methods.push((
        "near, exact scan".into(),
        "get d select doc near v $2 exact limit 10".into(),
    ));
    for c in [50, 200, 1_000] {
        methods.push((
            format!("match -> rerank, {c} candidates"),
            format!("get d select doc match body $1 rerank v $2 candidates {c} limit 10"),
        ));
    }
    for c in [10, 20, 50, 100, 200, 1_000] {
        methods.push((
            format!("match + near, fuse, {c} a side"),
            format!("get d select doc match body $1 near v $2 fuse candidates {c} limit 10"),
        ));
    }
    // The rank offset, at the default depth.
    for k in [10, 30, 60, 120] {
        methods.push((
            format!("match + near, fuse, k {k}"),
            format!("get d select doc match body $1 near v $2 fuse k {k} candidates 20 limit 10"),
        ));
    }
    if !dense {
        methods.truncate(dense_methods);
    }
    if qsparse.is_some() {
        methods.push((
            "near, sparse (SPLADE)".into(),
            "get d select doc near s $3 limit 10".into(),
        ));
        methods.push((
            "match + sparse near, fuse".into(),
            "get d select doc match body $1 near s $3 fuse limit 10".into(),
        ));
    }

    // The typos made, to say how many queries one changed.
    let typod: HashMap<&String, String> = qids
        .iter()
        .enumerate()
        .map(|(i, q)| (q, typo(&qtext[q], i as u64)))
        .collect();
    let changed = qids
        .iter()
        .filter(|q| qrels.contains_key(*q) && typod[q] != qtext[*q])
        .count();
    eprintln!("a typo in {changed} of {} queries", qrels.len());

    // `FENECBENCH_ONLY=match`: the methods whose names begin so, to time
    // one against another build.
    if let Ok(only) = std::env::var("FENECBENCH_ONLY") {
        methods.retain(|(name, _)| name.starts_with(&only));
    }
    println!(
        "{:<36} {:>8} {:>10} {:>10}",
        "", "nDCG@10", "p50 ms", "p99 ms"
    );
    for (name, sql) in &methods {
        let stmt = fenec_ql::parse_one(sql).unwrap();
        let mut total = 0.0;
        let mut times = Vec::with_capacity(qids.len());
        let mut refused = 0;
        for (i, qid) in qids.iter().enumerate() {
            let Some(rels) = qrels.get(qid) else { continue };
            let params = [
                Value::Text(qtext[qid].clone()),
                match dense {
                    true => Value::Vector(qflat[i * dim..(i + 1) * dim].to_vec()),
                    false => Value::Null,
                },
                match &qsparse {
                    Some(q) => Value::Sparse(VOCAB, q[i].clone()),
                    None => Value::Null,
                },
                Value::Text(typod[qid].clone()),
                Value::Text(half_typed(&qtext[qid])),
            ];
            let t = Instant::now();
            // A word half typed may reach more words than a query takes:
            // refused, it finds nothing.
            let Ok(r) = db.query(&stmt, &params) else {
                refused += 1;
                continue;
            };
            times.push(t.elapsed().as_secs_f64() * 1e3);
            let ranked: Vec<String> = r
                .rows()
                .unwrap()
                .rows
                .iter()
                .map(|row| match &row.values[0] {
                    Value::Text(t) => t.clone(),
                    _ => String::new(),
                })
                .collect();
            total += ndcg10(&ranked, rels);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "{name:<36} {:>8.4} {:>10.3} {:>10.3}{}",
            total / qrels.len() as f64,
            times[times.len() / 2],
            times[times.len() * 99 / 100],
            match refused {
                0 => String::new(),
                n => format!("  ({n} refused)"),
            }
        );
    }
}
