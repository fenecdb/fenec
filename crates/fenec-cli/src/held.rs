//! `<file>.lock`, taken before the shell or a subcommand opens or writes a
//! database file, as `fenec-server --file`, `fenec mcp --file` and the
//! native library take it (`fenec_abi::lock`): two writers over one mapped
//! file corrupt it. Taken by nothing, a shell beside a server appended its
//! write where the server appended its own next one, and the server's
//! records written over the shell's left the rest of the shell's body
//! behind them: "1 records", exit 0, and the next open refused the file
//! (`corrupt: unknown record kind`). A tool that only looks -- `fenec
//! types`, `--read-only` -- opens with `fs::open_read_only` and takes none.

pub use fenec_abi::lock::FileLock;
use fenec_abi::lock::{self, Refused};

/// `<path>.lock`, held for as long as the guard lives; refused, the text
/// to say: who holds the file, and `advice`, what to do instead.
pub fn take(path: &str, advice: &str) -> Result<FileLock, String> {
    match lock::hold(path) {
        Ok(l) => Ok(l),
        Err(Refused::Held) => Err(match lock::holder(path) {
            Some(h) if h.pid == Some(std::process::id()) => {
                format!("{path} is open in this process already\n  {advice}")
            }
            Some(h) if h.is_known() => format!(
                "{path} is open in another process ({h}), and two writers over one file \
                 corrupt it\n  {advice}"
            ),
            // Let go since, or a holder the system does not name: refused
            // all the same, not tried again, which a race could win.
            _ => format!(
                "{path} is open in another process, and two writers over one file corrupt \
                 it\n  {advice}"
            ),
        }),
        Err(Refused::Io(e)) => Err(format!("could not take {path}.lock: {e}")),
    }
}
