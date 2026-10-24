//! The crate's integration tests, one binary: each file was a binary of
//! its own, each linked with everything under it, and on macOS each
//! waited out the system's check of a new program on its first run.
//! The files apart (Cargo.toml) read the process's own counts, or run
//! without the indexes.

mod aggregate;
mod alter;
mod analytics;
mod analytics_docs;
mod approx;
mod autocompact;
mod blocks;
mod buckets;
mod changes;
mod claims;
mod collate;
mod composite;
mod composite_lookup;
mod counts;
mod crash;
mod derived;
mod exact_places;
mod explain;
mod facets;
mod filtered;
mod fuse;
mod fuzzy;
mod geo;
mod geo_docs;
mod handover;
mod having;
mod highlight;
mod insert;
mod json;
mod language_file;
mod limit;
mod lookup;
mod lookup_aggregate;
mod maintenance;
mod mapped;
mod mcp_docs;
mod memory;
// Its fold is held to Python's by the fold's own functions, which a build
// without the feature has no fold to call.
#[cfg(feature = "nocase")]
mod nocase;
mod order_expr;
mod paging;
mod persist;
mod pinned;
mod prepared;
mod quant;
mod query_json;
mod queues_docs;
mod redis_docs;
mod replica;
mod require;
mod returning;
mod rewrite;
mod sorted;
mod sparse;
mod spill;
mod sql_docs;
mod storage;
mod subquery;
mod synclog;
mod ttl;
mod unique;
mod upsert;
mod wakes;
mod within;
mod writes;
#[cfg(feature = "analytics")]
mod zones;
