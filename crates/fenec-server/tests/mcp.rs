//! `fenec-server --mcp` as a process: the endpoint its flags turn on, and
//! the file's lock, which a second process over the same file -- another
//! server, `fenec mcp --file`, an app -- is refused by.

use super::support::{start, tmp, try_start};

#[test]
fn a_second_process_over_a_served_file_is_refused() {
    let path = tmp("mcp-lock", "data.fenec");
    let p = path.to_str().unwrap();
    let first = start(&["--file", p, "--mcp"]);
    let second = try_start(&["--file", p]);
    let log = second.err().expect("a second server over the file");
    // The words the desktop app reads, then the holder, named.
    assert!(log.contains("it is open already"), "{log}");
    let holder = format!("(held by fenec-server, pid {})", first.child.id());
    assert!(log.contains(&holder), "{log}");
    // The lock is the native library's and `fenec mcp`'s: this process
    // takes it no more than the server lets it.
    assert!(matches!(
        fenec_abi::lock::hold(p),
        Err(fenec_abi::lock::Refused::Held)
    ));
    let (code, _) = first.terminate();
    assert_eq!(code, 0);
    // Let go at the exit: the next server opens it.
    let again = start(&["--file", p]);
    drop(again);
}

#[test]
fn the_flags_turn_the_endpoint_and_its_tools_on() {
    // A token, since the harness's `Host` names no address: an open
    // server's endpoint answers this machine's names alone.
    let s = start(&[
        "--mcp-memory",
        "memories",
        "--mcp-max-rows",
        "7",
        "--http-token",
        "tok",
    ]);
    let mut h = s.http().with_token("tok");
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#;
    let a = h.ask_with(
        "POST",
        "/_mcp",
        body,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/list"),
        ],
    );
    assert_eq!(a.status, 200, "{}", a.body);
    for tool in ["\"schema\"", "\"recall\"", "\"remember\"", "\"forget\""] {
        assert!(a.body.contains(tool), "{tool}: {}", a.body);
    }
    // --mcp-memory implies --mcp, and alone it lists no write tool.
    assert!(!a.body.contains("\"name\":\"write\""));
    assert!(a.body.contains("\"maximum\":7"), "{}", a.body);
    // The metrics name the tools once a server serves MCP.
    let m = h.ask("GET", "/_metrics", "");
    assert!(m.body.contains("fenec_mcp_tool_calls_total"), "{}", m.body);

    // Without --mcp, /_mcp says so; and /mcp is a collection's REST path,
    // as any name without an underscore is.
    let plain = start(&["--http-token", "tok"]);
    let mut h = plain.http().with_token("tok");
    let a = h.ask("POST", "/_mcp", body);
    assert_eq!(a.status, 404, "{}", a.body);
    assert!(a.body.contains("--mcp"), "{}", a.body);
    h.ask(
        "POST",
        "/query",
        r#"{"query":"create collection mcp (a int)"}"#,
    );
    let a = h.ask("POST", "/mcp", r#"{"a": 1}"#);
    assert_eq!(a.status, 201, "{}", a.body);
    let m = h.ask("GET", "/_metrics", "");
    assert_eq!(m.status, 200);
    assert!(!m.body.contains("fenec_mcp"), "{}", m.body);
    let s = start(&["--mcp", "--http-token", "tok"]);
    let mut h = s.http().with_token("tok");
    h.ask(
        "POST",
        "/query",
        r#"{"query":"create collection mcp (a int)"}"#,
    );
    let a = h.ask("POST", "/mcp", r#"{"a": 1}"#);
    assert_eq!(a.status, 201, "with --mcp too: {}", a.body);
}
