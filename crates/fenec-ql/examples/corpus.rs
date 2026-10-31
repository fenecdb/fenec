//! FenecQL's corpus, parsed: `tools/fenecql_corpus.py` (`make
//! fenecql-corpus BASE=origin/main`) builds this in the tree it checks and
//! in BASE's, hands it every FenecQL text the repository holds, one JSON
//! string a line on stdin, and compares the lines it writes.
//!
//! Each line out is the text read by `parse`, a tab, and by `parse_exact`:
//! `ok <the statements' Debug form>` or `err <the refusal>`, tabs and line
//! breaks escaped. It reads nothing but what both trees export, since the
//! same file is copied into BASE's to build there.

use std::io::{BufRead, Write};

fn one_line(s: String) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn read(r: fenec_core::error::Result<Vec<fenec_core::query::Statement>>) -> String {
    match r {
        Ok(s) => one_line(format!("ok {s:?}")),
        Err(e) => one_line(format!("err {e}")),
    }
}

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for line in stdin.lock().lines() {
        let line = line.expect("a line of stdin");
        let text = match fenec_core::json::parse(&line) {
            Ok(fenec_core::value::Value::Text(t)) => t.to_string(),
            other => panic!("not a JSON string: {line}: {other:?}"),
        };
        let parsed = read(fenec_ql::parse(&text));
        let exact = read(fenec_ql::parse_exact(&text));
        writeln!(out, "{parsed}\t{exact}").expect("stdout");
    }
}
