//! The crate's integration tests, one binary: each file was a binary of
//! its own, each linked with everything under it, and on macOS each
//! waited out the system's check of a new program on its first run.
//! The files apart (Cargo.toml) read the process's own counts.

mod crash;
mod follow;
mod logs;
mod mcp;
mod otlp;
mod providers;
mod shutdown;
mod start;
mod stdin;
mod studio;
mod support;
mod tokens;
mod tracing;
mod version;
