//! The crate's integration tests, one binary: each file was a binary of
//! its own, each linked with everything under it, and on macOS each
//! waited out the system's check of a new program on its first run.
//! The files apart (Cargo.toml) read the process's own counts, or run
//! without the indexes.

mod aggregate;
mod blocks;
mod changes;
mod collate;
mod crash;
mod explain;
mod filtered;
mod fuse;
mod lookup;
mod maintenance;
mod mapped;
mod memory;
mod paging;
mod persist;
mod quant;
mod replica;
mod rewrite;
mod sorted;
mod sparse;
mod storage;
