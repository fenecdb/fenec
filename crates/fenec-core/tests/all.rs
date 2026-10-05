//! The crate's integration tests, one binary: each file was a binary of
//! its own, each linked with everything under it, and on macOS each
//! waited out the system's check of a new program on its first run.
//! The files apart (Cargo.toml) read the process's own counts, or run
//! without the indexes.

mod aggregate;
mod alter;
mod analytics;
mod analytics_docs;
mod autocompact;
mod blocks;
mod buckets;
mod changes;
mod collate;
mod crash;
mod derived;
mod explain;
mod facets;
mod filtered;
mod fuse;
mod handover;
mod highlight;
mod insert;
mod json;
mod lookup;
mod maintenance;
mod mapped;
mod memory;
mod paging;
mod persist;
mod quant;
mod query_json;
mod redis_docs;
mod replica;
mod require;
mod rewrite;
mod sorted;
mod sparse;
mod spill;
mod storage;
mod subquery;
mod synclog;
mod ttl;
mod unique;
mod upsert;
mod within;
mod writes;
