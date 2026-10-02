//! Notes with fenec-core embedded: the database is a file this program
//! opens, no server.
//!
//!     cargo run                            seeds if empty, then lists
//!     cargo run -- add <title> <body> [tag ...]
//!     cargo run -- list [--tag T] [--open]
//!     cargo run -- search <words>
//!     cargo run -- done <id>
//!     cargo run -- smoke                   what CI runs, over a file of its own

use fenec_core::prelude::*;
use fenec_core::value::Value;

const SCHEMA: &str = include_str!("../schema.fenecql");

const SEEDS: [(&str, &str, &[&str], bool, &str); 4] = [
    ("Groceries", "Buy milk, eggs and fresh bread for the weekend.", &["home", "shopping"], false, "2026-09-28T09:00:00Z"),
    ("Release checklist", "Tag the release, publish the packages and update the docs.", &["work"], false, "2026-09-29T09:00:00Z"),
    ("Book flights", "Find cheap flights to Istanbul for the conference in spring.", &["travel", "work"], true, "2026-09-30T09:00:00Z"),
    ("Book club", "Finish the novel about the desert fox before Thursday.", &["home", "reading"], false, "2026-10-01T09:00:00Z"),
];

/// A TOY embedding, a placeholder for a real model: hashed character
/// trigrams (FNV-1a over the UTF-8 bytes) into 64 dimensions. It matches
/// spelling, not meaning. A real one is a local model (ONNX, candle) or an
/// embeddings API call, with the field's dimension changed to match.
fn embed(text: &str) -> Vec<f32> {
    let bytes = format!(" {} ", text.to_ascii_lowercase()).into_bytes();
    let mut v = vec![0f32; 64];
    for w in bytes.windows(3) {
        let h = w.iter().fold(0x811c9dc5u32, |h, &b| (h ^ b as u32).wrapping_mul(0x01000193));
        v[(h % 64) as usize] += 1.0;
    }
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

struct Notes {
    db: Database,
}

impl Notes {
    /// Opens the file and makes what the schema declares and it lacks; a
    /// difference that would lose data is refused.
    fn open(path: &str) -> Result<Notes> {
        let mut db = fenec_core::fs::open(path)?;
        let request = format!(r#"{{"format": 1, "fenecql": {}}}"#, json_string(SCHEMA));
        let outcome = fenec_abi::schema(&mut db, &request, true, None)?;
        if let Some(r) = outcome.plan.refusals.first() {
            return Err(Error::Query(format!("schema: {r:?}")));
        }
        Ok(Notes { db })
    }

    /// One statement, its values as `$1`, `$2`, ... -- never in the text.
    fn run(&mut self, text: &str, params: &[Value]) -> Result<Response> {
        let mut last = Response::Affected(0);
        for stmt in fenec_ql::parse(text)? {
            last = self.db.execute_with(&stmt, params)?;
        }
        Ok(last)
    }

    fn rows(&mut self, text: &str, params: &[Value]) -> Result<Vec<Row>> {
        match self.run(text, params)? {
            Response::Rows(rs) => Ok(rs.rows),
            _ => Ok(vec![]),
        }
    }

    fn titles(&mut self, text: &str, params: &[Value]) -> Result<Vec<String>> {
        Ok(self.rows(text, params)?.iter().map(|r| r.values[0].as_text().unwrap_or("").to_string()).collect())
    }

    fn add(&mut self, title: &str, body: &str, tags: &[&str], done: bool, at: Option<&str>) -> Result<()> {
        let tags = Value::List(tags.iter().map(|t| Value::Text(t.to_string())).collect());
        let at = match at {
            Some(t) => Value::Text(t.into()),
            None => Value::Timestamp(now_ms()),
        };
        let vector = Value::Vector(embed(&format!("{title} {body}")));
        self.run(
            "put notes {title: $1, body: $2, tags: $3, done: $4, at: $5, embed: $6}",
            &[Value::Text(title.into()), Value::Text(body.into()), tags, Value::Bool(done), at, vector],
        )?;
        self.db.sync() // the library never syncs on its own
    }

    fn count(&mut self) -> Result<usize> {
        let rows = self.rows("get notes count", &[])?;
        Ok(match rows.first().map(|r| &r.values[0]) {
            Some(Value::Int(n)) => *n as usize,
            _ => 0,
        })
    }

    fn seed(&mut self) -> Result<()> {
        if self.count()? == 0 {
            for (title, body, tags, done, at) in SEEDS {
                self.add(title, body, tags, done, Some(at))?;
            }
        }
        Ok(())
    }

    fn list(&mut self, tag: Option<&str>, open: bool) -> Result<Vec<Row>> {
        let mut filter = vec![];
        let mut params = vec![];
        if let Some(t) = tag {
            params.push(Value::Text(t.into()));
            filter.push(format!("tags has ${}", params.len()));
        }
        if open {
            filter.push("done = false".to_string());
        }
        let filter = if filter.is_empty() { String::new() } else { format!(" where {}", filter.join(" and ")) };
        self.rows(&format!("get notes select title, tags, done, at{filter} order at desc limit 20"), &params)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn show(rows: &[Row]) {
    for r in rows {
        let done = matches!(r.values[2], Value::Bool(true));
        let tags = match &r.values[1] {
            Value::List(l) => l.iter().filter_map(Value::as_text).collect::<Vec<_>>().join(", "),
            _ => String::new(),
        };
        let title = r.values[0].as_text().unwrap_or("");
        println!("[{}] {:>3}  {title:<20} {tags}", if done { "x" } else { " " }, r.id);
    }
}

fn search(notes: &mut Notes, words: &str) -> Result<(Vec<String>, Vec<String>)> {
    let w = Value::Text(words.into());
    let v = Value::Vector(embed(words));
    let matched = notes.titles("get notes select title match body $1 limit 5", &[w.clone()])?;
    let fused = notes.titles("get notes select title match body $1 near embed $2 fuse limit 5", &[w, v])?;
    Ok((matched, fused))
}

fn smoke() -> Result<()> {
    fn check(step: &str, ok: bool) {
        println!("{} {step}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            std::process::exit(1);
        }
    }
    let dir = std::env::temp_dir().join(format!("fenec-notes-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| Error::Query(e.to_string()))?;
    let path = dir.join("notes.fenec");
    let path = path.to_str().unwrap_or("notes.fenec");
    {
        let mut notes = Notes::open(path)?;
        notes.seed()?;
        check("seeded 4 notes", notes.count()? == 4);
        let hello: Vec<usize> = embed("hello").iter().enumerate().filter(|(_, x)| **x != 0.0).map(|(i, _)| i).collect();
        check("toy embedding", hello == [24, 36, 46, 48, 62]);
        let all = notes.list(None, false)?;
        check("newest first", all[0].values[0].as_text() == Some("Book club"));
        let work: Vec<_> = notes.list(Some("work"), false)?.iter().map(|r| r.values[0].as_text().unwrap_or("").to_string()).collect();
        check("by tag", work == ["Book flights", "Release checklist"]);
        check("open", notes.list(None, true)?.len() == 3);
        check("match", search(&mut notes, "release docs")?.0[0] == "Release checklist");
        let near = notes.titles("get notes select title near embed $1 limit 1", &[Value::Vector(embed("flights to Istanbul"))])?;
        check("near", near[0] == "Book flights");
        check("fuse", search(&mut notes, "desert fox")?.1[0] == "Book club");
        notes.add("Call mom", "Ask about the weekend.", &["home"], false, None)?;
        notes.run("set notes {done: true} where title = $1", &[Value::Text("Groceries".into())])?;
        notes.db.sync()?;
        check("done", notes.list(None, true)?.len() == 3);
    }
    let mut again = Notes::open(path)?;
    check("persisted across a reopen", again.count()? == 5);
    drop(again);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or("");
    if arg(0) == "smoke" {
        return smoke();
    }
    let path = std::env::var("NOTES_FILE").unwrap_or_else(|_| "notes.fenec".into());
    let mut notes = Notes::open(&path)?;
    match arg(0) {
        "add" => {
            let tags: Vec<&str> = args[3..].iter().map(String::as_str).collect();
            notes.add(arg(1), arg(2), &tags, false, None)?;
        }
        "list" => {
            let tag = args.iter().position(|a| a == "--tag").map(|i| arg(i + 1));
            show(&notes.list(tag, args.iter().any(|a| a == "--open"))?);
        }
        "search" => {
            let (matched, fused) = search(&mut notes, &args[1..].join(" "))?;
            println!("match: {}", matched.join(", "));
            println!("fuse:  {}", fused.join(", "));
        }
        "done" => {
            let id: i64 = arg(1).parse().map_err(|_| Error::Query("done <id>".into()))?;
            notes.run("set notes {done: true} where id = $1", &[Value::Int(id)])?;
            notes.db.sync()?;
        }
        _ => {
            notes.seed()?;
            show(&notes.list(None, false)?);
        }
    }
    // Saves the graph into the file, so the next open links nothing.
    notes.db.checkpoint()
}
