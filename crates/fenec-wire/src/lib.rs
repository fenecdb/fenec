//! # fenec-wire
//!
//! The PostgreSQL v3 wire protocol, the part both of fenecdb's sides of it
//! speak: the framing ([`proto`]) and a client ([`client`]) for a real
//! PostgreSQL server.
//!
//! A crate of its own so that the importer, which reads from PostgreSQL
//! through the client, sits below the server, which runs the importer's
//! `--follow` in its own process (`fenec-pg --follow`): in fenec-pg, the
//! client made the importer depend on the server, and the server could not
//! depend on it back.

pub mod client;
pub mod proto;

/// The hashing SCRAM's client half uses: fenec-http's, which checks JWTs
/// with it too.
pub use fenec_http::crypto;
