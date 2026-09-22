//! Stopping a command that runs until interrupted -- `fenec archive`,
//! `fenec import --follow` -- between two of its steps rather than in the
//! middle of one.

use std::sync::atomic::{AtomicBool, Ordering};

/// Set by SIGINT or SIGTERM.
pub static STOP: AtomicBool = AtomicBool::new(false);

/// Routes SIGINT and SIGTERM to [`STOP`]; libc's `signal` is declared here,
/// as `fenec-pg` does, to add no dependency.
pub fn on_signals() {
    extern "C" {
        fn signal(sig: i32, handler: usize) -> usize;
    }
    extern "C" fn stop(_sig: i32) {
        STOP.store(true, Ordering::SeqCst);
    }
    unsafe {
        for sig in [2, 15] {
            signal(sig, stop as *const () as usize);
        }
    }
}
