//! The crate's integration tests, one binary: each file was a binary of
//! its own, each linked with everything under it, and on macOS each
//! waited out the system's check of a new program on its first run.
//! The files apart (Cargo.toml) read the process's own counts, or hold
//! so many threads that beside the rest the process would pass macOS's
//! 2 048 threads a process.

mod cors;
mod fanout;
mod leases;
mod listen;
mod no_thread;
mod region_leases;
mod regions;
mod replicas;
mod router;
mod standby;
mod tokens;
mod upstream;
mod version;
