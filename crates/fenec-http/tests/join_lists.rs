//! A SQL join read as a list, over HTTP: where the list is tried first and
//! passes its bound, it is walked once and the join answered through the
//! `lookup`'s probe -- under the lock and pinned. `query_json` declined the
//! probe, which holds a `lookup`, and `query` answered the statement again,
//! walking the list a second time.
//!
//! A binary of its own: it reads the process's count of lists refused past
//! their bound (`fenec_core::engine::lists_past_bound`).

use fenec_core::prelude::*;
use fenec_http::{Config, Server};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// A server over `users` -- 100 010 rows, `s` ordered and `n` not -- and
/// `orders` of `count` rows, `user_id` with no index, so a pinned read
/// reaches no index; every read pinned where `pinned` names one, which is
/// held to be.
fn start(count: i64, pinned: Option<&str>) -> u16 {
    let mut db = Database::new();
    for sql in [
        "create collection users (n int, s int @sorted)",
        "create collection orders (user_id int, total int)",
    ] {
        db.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
    }
    let put = |db: &mut Database, coll: &str, docs: Vec<Value>| {
        for chunk in docs.chunks(20_000) {
            let st = fenec_ql::parse_one(&format!("put {coll} $1")).unwrap();
            db.execute_with(&st, &[Value::List(chunk.to_vec())])
                .unwrap();
        }
    };
    let row = |pairs: &[(&str, i64)]| {
        Value::object(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), Value::Int(*v)))
                .collect(),
        )
        .unwrap()
    };
    put(
        &mut db,
        "users",
        (0..100_010).map(|i| row(&[("n", i), ("s", i)])).collect(),
    );
    // User 5 is `n` 4; the rest are past every user.
    let orders = (0..count)
        .map(|i| match i {
            0 => row(&[("user_id", 5), ("total", 1)]),
            1 => row(&[("user_id", 100_001), ("total", 2)]),
            _ => row(&[("user_id", 200_000 + i), ("total", 3)]),
        })
        .collect();
    put(&mut db, "orders", orders);
    if let Some(sql) = pinned {
        db.set_pin_at(0);
        let st = fenec_ql::parse_one(sql).unwrap();
        assert!(db.pin(&[(&st, &[][..])]).is_some(), "{sql} is not pinned");
    }
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        ..Config::default()
    };
    let server = Server::new(Arc::new(RwLock::new(db)), cfg);
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    port
}

fn query(port: u16, sql: &str) -> (u16, String) {
    let mut body = String::from("{\"query\":");
    fenec_core::json::escape_into(&mut body, sql);
    body.push('}');
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(
        s,
        "POST /query HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let body = out
        .split_once("\r\n\r\n")
        .map_or("", |(_, b)| b)
        .to_string();
    (out[9..12].parse().unwrap(), body)
}

/// The lists refused past their bound while `sql` was answered, and its
/// answer.
fn walks(port: u16, sql: &str) -> (u64, String) {
    let before = fenec_core::engine::lists_past_bound();
    let (status, body) = query(port, sql);
    assert_eq!(status, 200, "{sql}: {body}");
    (fenec_core::engine::lists_past_bound() - before, body)
}

/// One test: the count is the process's, and two at once would mix theirs.
#[test]
fn a_list_tried_first_is_walked_once() {
    // A range of the ordered index names the users, how many unsaid: the
    // list first, under the lock (a pin declines a range).
    let port = start(3, None);
    let sql = "SELECT o.total FROM orders o JOIN users u ON u.id = o.user_id WHERE u.s != 4 \
               AND u.s >= 0 ORDER BY o.id";
    let (n, body) = walks(port, sql);
    assert_eq!((n, body.as_str()), (1, r#"[{"total":2}]"#));
    // A plain `get` lists nothing past its bound.
    let plain = "get orders select total where user_id in (get users select id where s < 10) \
                 order id";
    let (n, body) = walks(port, plain);
    assert_eq!((n, body.as_str()), (0, r#"[{"total":1}]"#));

    // No index names the users and the orders are more than an eighth of
    // them: the list first, read pinned.
    let sql = "SELECT o.total FROM orders o JOIN users u ON u.id = o.user_id WHERE u.n != 4 \
               ORDER BY o.id";
    let port = start(13_000, Some(sql));
    let (n, body) = walks(port, sql);
    assert_eq!((n, body.as_str()), (1, r#"[{"total":2}]"#));
}
