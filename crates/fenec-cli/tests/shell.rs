//! The shell reading statements written over several lines: piped, given
//! with `-c`, and typed or pasted at a terminal (a pseudo-terminal the test
//! drives). It ran each line whose brackets were closed as it came, so
//! `del notes` with `where id = 3` on the next line deleted every row and
//! then refused the `where`; each test here failed so, but the two that
//! hold what stayed -- `-c` reading a text whole, and a read typed on one
//! line answering at Enter.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_fenec");

/// A directory of its own, removed at the end.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let d = std::env::temp_dir().join(format!("fenec-cli-shell-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }

    /// A file holding `notes`: ids 1 to 5, topics 4 3 4 3 4, `posted` 1 to 5.
    fn notes(&self) -> PathBuf {
        let f = self.0.join("notes.fenec");
        let out = c(
            &f,
            "create collection notes (body text, topic int @hash, posted int @sorted) \
             put notes [{body: \"a\", topic: 4, posted: 1}, {body: \"b\", topic: 3, posted: 2}, \
             {body: \"c\", topic: 4, posted: 3}, {body: \"d\", topic: 3, posted: 4}, \
             {body: \"e\", topic: 4, posted: 5}]",
        );
        assert!(out.status.success(), "{}", text(&out.stderr));
        f
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn c(file: &Path, cmd: &str) -> Output {
    Command::new(BIN)
        .arg(file)
        .args(["-c", cmd])
        .output()
        .unwrap()
}

/// `input` piped into the shell over `file`.
fn piped(file: &Path, input: &str) -> Output {
    use std::io::Write;
    let mut child = Command::new(BIN)
        .arg(file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// The rows of each table printed, each row's cells joined by a space.
fn tables(out: &str) -> Vec<Vec<String>> {
    let lines: Vec<&str> = out.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut found = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if !l.is_empty() && l.chars().all(|c| c == '-' || c == ' ') && i > 0 {
            let rows = lines[i + 1..]
                .iter()
                .take_while(|r| !r.starts_with('('))
                .map(|r| r.split_whitespace().collect::<Vec<_>>().join(" "))
                .collect();
            found.push(rows);
        }
    }
    found
}

fn count(file: &Path, filter: &str) -> i64 {
    let out = c(file, &format!("get notes {filter} count"));
    assert!(out.status.success(), "{}", text(&out.stderr));
    tables(&text(&out.stdout))[0][0].parse().unwrap()
}

#[test]
fn a_read_written_over_lines_is_read_whole() {
    let d = Dir::new("read");
    let f = d.notes();
    let out = piped(
        &f,
        "get notes select body\n  where topic = 4\n  order posted desc\n",
    );
    assert_eq!(text(&out.stderr), "");
    assert_eq!(tables(&text(&out.stdout)), [["e", "c", "a"]]);
}

#[test]
fn a_delete_written_over_lines_deletes_what_it_says() {
    let d = Dir::new("del");
    let f = d.notes();
    let out = piped(&f, "del notes\n  where id = 3\n");
    assert_eq!(text(&out.stderr), "");
    assert_eq!(count(&f, ""), 4);
    assert_eq!(count(&f, "where id = 3"), 0);
}

/// A script of statements, each over several lines -- a schema, a
/// document, a `set`, a `del` that picks its row, reads -- runs as
/// written, and a blank line between two lines of a statement is white
/// space, as in `-c`.
#[test]
fn a_script_of_statements_over_lines_runs_as_written() {
    let d = Dir::new("script");
    let f = d.0.join("script.fenec");
    let out = piped(
        &f,
        "create collection notes (
  body   text,
  topic  int @hash,
  posted int @sorted
)
put notes [
  {body: \"a\", topic: 4, posted: 1},
  {body: \"b\", topic: 3, posted: 2},
  {body: \"c\", topic: 4, posted: 3}
]
set notes {topic: 9}

  where body = \"b\"
del notes
  where topic = 4
  order posted
  limit 1
get notes select body, topic
  order posted
",
    );
    assert_eq!(text(&out.stderr), "");
    assert_eq!(tables(&text(&out.stdout)), [["b 9", "c 4"]]);
}

/// A `;`, a keyword or a quote inside a string or a comment ends nothing
/// and opens nothing: a comment's apostrophe once kept the shell reading
/// to the end of its input, every statement after it dropped.
#[test]
fn strings_and_comments_hold_no_end() {
    let d = Dir::new("strings");
    let f = d.notes();
    let out = piped(
        &f,
        "put notes {body: \"a; where id = 3\", topic: 7}
put notes {body: 'del notes; -- no', topic: 7}
-- it's a comment; del notes
# a \"quoted\" where
del notes -- the ones of topic 7; it's fine
  where topic = 7
  and body ~ \"where\"
get notes select body where topic = 7
",
    );
    assert_eq!(text(&out.stderr), "");
    assert_eq!(tables(&text(&out.stdout)), [["del notes; -- no"]]);
    assert_eq!(count(&f, ""), 6);
}

/// A word that begins no statement may go on with the statement before
/// it: `wher` for `where` refuses the `del` too, rather than leave it to
/// delete every row.
#[test]
fn a_word_that_begins_no_statement_holds_back_the_one_before() {
    let d = Dir::new("typo");
    let f = d.notes();
    let out = piped(&f, "del notes\nwher id = 3\nget notes count\n");
    let said = text(&out.stderr);
    assert!(said.contains("unknown command `wher`"), "{said}");
    assert!(
        said.contains("not run either") && said.contains("del notes"),
        "{said}"
    );
    assert_eq!(tables(&text(&out.stdout)), [["5"]]);
    // Refused for what was read, the next statement runs.
    let out = piped(&f, "get notes limit \"x\"\nput notes {body: \"f\"}\n");
    let said = text(&out.stderr);
    assert!(said.contains("`limit` takes a whole number"), "{said}");
    assert!(text(&out.stdout).contains("1 records"));
    assert_eq!(count(&f, ""), 6);
}

/// `-c` reads its text whole, as `POST /query` does -- a statement over
/// lines is one -- and a `;` ends a statement there too: no clause goes on
/// past it, so a `del notes;` with a `where` after it is refused rather
/// than read as one.
#[test]
fn minus_c_reads_its_text_whole() {
    let d = Dir::new("c");
    let f = d.notes();
    let out = c(
        &f,
        "get notes select body\n  where topic = 4\n  order posted desc",
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(tables(&text(&out.stdout)), [["e", "c", "a"]]);
    let out = c(&f, "del notes\n  where id = 3");
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(count(&f, ""), 4);
    let out = c(&f, "del notes; where id = 2");
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("unknown command `where`"));
    assert_eq!(count(&f, ""), 4);
}

/// Typed at a terminal: a read on one line answers at Enter; a write waits
/// for the next line, which goes on with it -- a `.` command between them
/// leaves it waiting -- and an empty line runs it; a line that begins no
/// statement after a read that ran says why; `.quit` leaves a write that
/// waits unrun.
#[cfg(unix)]
#[test]
fn typed_a_read_answers_at_enter_and_a_write_waits_for_its_next_line() {
    let d = Dir::new("typed");
    let f = d.notes();
    let mut t = pty::Shell::start(&f);
    t.typed("get notes count\n");
    t.wait_for("(1 rows", 1);
    t.typed("del notes\n");
    t.typed(".tables\n");
    t.typed("  where id = 3\n");
    t.typed("\n");
    t.typed("get notes count\n");
    t.wait_for("(1 rows", 2);
    t.typed("get notes select body\n");
    t.typed("  where topic = 4\n");
    t.typed("put notes {body: \"z\"}\n");
    t.typed(".quit\n");
    let out = t.end();
    assert!(out.contains("1 records"), "{out}");
    assert!(out.contains("not run: put notes {body: \"z\"}"), "{out}");
    let counts: Vec<Vec<String>> = tables(&out)
        .into_iter()
        .filter(|t| t.len() == 1 && t[0].parse::<i64>().is_ok())
        .collect();
    assert_eq!(counts, [["5"], ["4"]], "{out}");
    assert!(
        out.contains("`where` begins no statement: the one before it ran as its line ended"),
        "{out}"
    );
    assert_eq!(count(&f, ""), 4);
}

/// Pasted, the lines come at once: a statement over several lines is read
/// whole as a pipe's is, and the paste's last statement runs at the empty
/// line after it.
#[cfg(unix)]
#[test]
fn pasted_statements_over_lines_run_as_written() {
    let d = Dir::new("pasted");
    let f = d.notes();
    let mut t = pty::Shell::start(&f);
    t.pasted("get notes select body\n  where topic = 4\n  order posted desc\ndel notes\n  where id = 2\n");
    t.typed("\n");
    t.typed("get notes count\n");
    t.wait_for("(1 rows", 1);
    t.typed(".quit\n");
    let out = t.end();
    assert!(!out.contains("error"), "{out}");
    assert_eq!(tables(&out)[0], ["e", "c", "a"], "{out}");
    assert_eq!(count(&f, ""), 4);
    assert_eq!(count(&f, "where id = 2"), 0);
}

/// A pseudo-terminal the shell is started on, as a user's terminal: lines
/// typed one after another, each once the shell has asked for it, or
/// pasted in one write.
#[cfg(unix)]
mod pty {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    extern "C" {
        fn posix_openpt(flags: i32) -> i32;
        fn grantpt(fd: i32) -> i32;
        fn unlockpt(fd: i32) -> i32;
        fn ptsname(fd: i32) -> *const std::ffi::c_char;
    }
    const O_RDWR: i32 = 2;
    #[cfg(target_vendor = "apple")]
    const O_NOCTTY: i32 = 0x20000;
    #[cfg(not(target_vendor = "apple"))]
    const O_NOCTTY: i32 = 0o400;
    /// `ptsname` answers in a buffer of its own: one test at a time.
    static OPENING: Mutex<()> = Mutex::new(());
    /// How long anything that must happen may take: a bound, never a pace.
    const WITHIN: Duration = Duration::from_secs(60);

    pub struct Shell {
        master: File,
        child: Child,
        out: Arc<(Mutex<String>, Condvar)>,
        reader: std::thread::JoinHandle<()>,
        prompts: usize,
    }

    impl Shell {
        pub fn start(file: &Path) -> Shell {
            let (master, slave) = {
                let _one = OPENING.lock().unwrap();
                // SAFETY: plain calls on a descriptor this function owns.
                let m = unsafe { posix_openpt(O_RDWR | O_NOCTTY) };
                assert!(m >= 0, "{}", std::io::Error::last_os_error());
                assert_eq!(unsafe { grantpt(m) }, 0);
                assert_eq!(unsafe { unlockpt(m) }, 0);
                // SAFETY: `ptsname` answers a C string, read before the
                // next call under the lock.
                let name = unsafe { std::ffi::CStr::from_ptr(ptsname(m)) }
                    .to_str()
                    .unwrap()
                    .to_owned();
                // SAFETY: `m` is open and owned by nothing else.
                let master = unsafe { File::from_raw_fd(m) };
                use std::os::unix::fs::OpenOptionsExt;
                let slave = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(O_NOCTTY)
                    .open(name)
                    .unwrap();
                (master, slave)
            };
            let child = Command::new(super::BIN)
                .arg(file)
                .stdin(Stdio::from(slave.try_clone().unwrap()))
                .stdout(Stdio::from(slave.try_clone().unwrap()))
                .stderr(Stdio::from(slave))
                .spawn()
                .unwrap();
            let out = Arc::new((Mutex::new(String::new()), Condvar::new()));
            let (sink, mut from) = (out.clone(), master.try_clone().unwrap());
            let reader = std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                // The end of the shell's output reads as an error on Linux
                // (EIO) and as an end on macOS.
                while let Ok(n @ 1..) = from.read(&mut buf) {
                    let (text, said) = &*sink;
                    text.lock()
                        .unwrap()
                        .push_str(&String::from_utf8_lossy(&buf[..n]));
                    said.notify_all();
                }
            });
            let mut shell = Shell {
                master,
                child,
                out,
                reader,
                prompts: 0,
            };
            shell.prompt();
            shell
        }

        /// Waits until `needle` has been written `times` times.
        pub fn wait_for(&self, needle: &str, times: usize) {
            self.until(|out| out.matches(needle).count() >= times);
        }

        fn until(&self, done: impl Fn(&str) -> bool) {
            let deadline = Instant::now() + super::pty::WITHIN;
            let (text, said) = &*self.out;
            let mut out = text.lock().unwrap();
            while !done(&out) {
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(!left.is_zero(), "waited in vain; the shell wrote:\n{out}");
                out = said.wait_timeout(out, left).unwrap().0;
            }
        }

        /// Waits for the shell to ask for its next line.
        fn prompt(&mut self) {
            self.prompts += 1;
            let n = self.prompts;
            self.until(|out| out.matches("fenec> ").count() + out.matches("  ... ").count() >= n);
        }

        /// A line typed once the shell asks for it.
        pub fn typed(&mut self, line: &str) {
            self.master.write_all(line.as_bytes()).unwrap();
            if !line.starts_with(".quit") {
                self.prompt();
            }
        }

        /// Lines pasted, in one write.
        pub fn pasted(&mut self, lines: &str) {
            self.master.write_all(lines.as_bytes()).unwrap();
            for _ in 0..lines.matches('\n').count() {
                self.prompt();
            }
        }

        /// What the shell wrote, once it has quit and its terminal has
        /// handed over the last of it.
        pub fn end(mut self) -> String {
            let status = self.child.wait().unwrap();
            assert!(status.success(), "{status}");
            self.reader.join().unwrap();
            let (text, _) = &*self.out;
            let out = text.lock().unwrap().replace('\r', "");
            out
        }
    }
}
