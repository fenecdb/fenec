//! `fenec backup`, `archive`, `restore`, `prune` and `verify` given an empty
//! token or key: `--token "$REPL"` with REPL unset, an environment file's
//! line left empty. An empty FENEC_REPLICATION_TOKEN is no token -- the
//! primary's ask for one is said before anything is sent -- and an empty
//! `--token` is refused; an empty key file, from the flag or FENEC_KEY_FILE,
//! is refused rather than taken for none, which would write in the clear.

use std::process::{Command, Output};

fn fenec(args: &[&str], env: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fenec"))
        .env_remove("FENEC_REPLICATION_TOKEN")
        .env_remove("FENEC_KEY_FILE")
        .envs(env.iter().copied())
        .args(args)
        .output()
        .unwrap()
}

fn said(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

// A port nothing listens on: a backup that got as far as connecting would
// say so, rather than what is asserted.
const PRIMARY: &str = "http://127.0.0.1:9";

#[test]
fn an_empty_replication_token_is_none_or_refused() {
    let dir = std::env::temp_dir().join(format!("fenec-cli-tokens-{}", std::process::id()));
    let out_file = dir.join("out.fenec");
    let out_file = out_file.to_str().unwrap();

    let out = fenec(
        &["backup", PRIMARY, out_file],
        &[("FENEC_REPLICATION_TOKEN", "")],
    );
    assert_eq!(out.status.code(), Some(2), "{}", said(&out));
    assert!(
        said(&out).contains("asks for its replication token"),
        "an empty FENEC_REPLICATION_TOKEN was sent as a token: {}",
        said(&out)
    );

    let out = fenec(&["backup", PRIMARY, out_file, "--token", ""], &[]);
    assert_eq!(out.status.code(), Some(2), "{}", said(&out));
    assert!(said(&out).contains("--token is empty"), "{}", said(&out));
}

#[test]
fn an_empty_key_file_is_refused() {
    let dir = std::env::temp_dir().join(format!("fenec-cli-keys-{}", std::process::id()));
    let dir = dir.to_str().unwrap();
    for (args, env) in [
        (vec!["verify", dir], vec![("FENEC_KEY_FILE", "")]),
        (vec!["verify", dir, "--key-file", ""], vec![]),
    ] {
        let out = fenec(&args, &env);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", said(&out));
        assert!(
            said(&out).contains("the key file is empty"),
            "{args:?}: {}",
            said(&out)
        );
    }
}
