//! Low-level building blocks for a YouTube Music client.
//!
//! The Music API is an undocumented Innertube protocol. This crate deliberately
//! exposes its responses as JSON at the boundary, so renderer-tree changes do
//! not leak into the application's domain model.

pub mod account;
pub mod cache;
pub mod catalog;
pub mod innertube;
pub mod media;
pub mod playback;

/// The URL type used throughout this crate's API.
pub use reqwest::Url;

/// The HTTP client type shared by every API in this crate. Build one and clone
/// it into each API so they share a connection pool.
pub use reqwest::Client;
