//! The crate's integration tests, one binary: each file was a binary of
//! its own, each linked with everything under it, and on macOS each
//! waited out the system's check of a new program on its first run.
//! The files apart (Cargo.toml) read the process's own counts, or run
//! without the indexes.

mod access;
mod api;
mod archive;
mod changes;
mod replication;
