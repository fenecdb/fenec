//! # fenec-server
//!
//! The process that serves a fenecdb file, or a directory of them, over
//! HTTP: the endpoint itself is `fenec-http`'s, and this crate is what runs
//! around it -- when writes reach the disk and what a shutdown does
//! ([`durability`]), and `--follow`'s mirror of a PostgreSQL table
//! ([`mirror`]).
//!
//! One process writes one file: the HTTP endpoint, a replica's follower,
//! the graph keeper and the mirror are threads of it over the same
//! database, since two processes over a file corrupt it.

pub mod durability;
pub mod mirror;

pub use durability::SyncPolicy;
