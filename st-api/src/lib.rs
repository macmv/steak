//! Low-level building blocks for a YouTube Music client.
//!
//! The Music API is an undocumented Innertube protocol. This crate deliberately
//! exposes its responses as JSON at the boundary, so renderer-tree changes do
//! not leak into the application's domain model.

pub mod account;
pub mod catalog;
pub mod innertube;
pub mod media;
pub mod playback;

/// The URL type used throughout this crate's API.
pub use reqwest::{StatusCode, Url};

/// The HTTP client type shared by every API in this crate. Build one and clone
/// it into each API so they share a connection pool.
pub use reqwest::Client;

/// Builds the shared HTTP client.
///
/// Only connecting is bounded here. A whole-request or read timeout would also
/// cut off a media body that the player is slow to consume, so callers bound
/// the waits they care about themselves (see [`innertube::client`] for API
/// calls).
pub fn http_client() -> Client {
  Client::builder()
    .connect_timeout(std::time::Duration::from_secs(10))
    .tcp_keepalive(std::time::Duration::from_secs(30))
    .build()
    .expect("the HTTP client configuration is valid")
}
