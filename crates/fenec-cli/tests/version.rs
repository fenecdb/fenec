//! `fenec --version` and `-V`: the line a package's check and a script
//! read, on stdout, with nothing opened. Before them `--version` was an
//! unknown option, and `fenec version` opened a file named `version` --
//! which it still does: a word is a file, and `version` is no subcommand.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_fenec");

/// A directory of its own, the shell's working directory, removed at the
/// end.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let d =
            std::env::temp_dir().join(format!("fenec-cli-version-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }

    /// The names in it.
    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fenec(cwd: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn version_and_v_print_the_name_and_version_on_stdout() {
    let dir = Dir::new("flag");
    let want = format!("fenec {}\n", env!("CARGO_PKG_VERSION"));
    for flag in ["--version", "-V"] {
        let out = fenec(&dir.0, &[flag]);
        assert_eq!(out.status.code(), Some(0), "{flag}: {}", text(&out.stderr));
        assert_eq!(text(&out.stdout), want, "{flag}");
        assert_eq!(text(&out.stderr), "", "{flag}");
    }
    assert_eq!(dir.names(), Vec::<String>::new(), "a version opened a file");
}

#[test]
fn a_version_after_a_file_answers_and_opens_it_not() {
    // Read as `--help` is, wherever a flag stands: the file it would have
    // opened is never made.
    let dir = Dir::new("after");
    let want = format!("fenec {}\n", env!("CARGO_PKG_VERSION"));
    for args in [
        &["notes.fenec", "--version"][..],
        &["notes.fenec", "-c", "create collection t (n int)", "-V"][..],
    ] {
        let out = fenec(&dir.0, args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{args:?}: {}",
            text(&out.stderr)
        );
        assert_eq!(text(&out.stdout), want, "{args:?}");
    }
    assert_eq!(dir.names(), Vec::<String>::new(), "notes.fenec was opened");
}

#[test]
fn minus_c_takes_a_v_as_its_text() {
    // `-c -V` is a statement that reads "-V", run as any other text is (and
    // refused, since it is none), not a version asked for.
    let dir = Dir::new("text");
    let out = fenec(&dir.0, &["-c", "-V"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        !text(&out.stdout).contains(env!("CARGO_PKG_VERSION")),
        "{}",
        text(&out.stdout)
    );
    assert!(!text(&out.stderr).is_empty());
}

#[test]
fn version_without_dashes_is_still_a_file() {
    let dir = Dir::new("word");
    let out = fenec(&dir.0, &["version", "-c", "create collection t (n int)"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        !text(&out.stdout).contains(env!("CARGO_PKG_VERSION")),
        "{}",
        text(&out.stdout)
    );
    assert!(dir.0.join("version").is_file(), "{:?}", dir.names());

    // And it is a database: the collection is there when it is opened
    // again, where a read of one missing fails.
    let out = fenec(&dir.0, &["version", "-c", "get t"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = fenec(&dir.0, &["version", "-c", "get missing"]);
    assert!(!out.status.success(), "{}", text(&out.stdout));
}
