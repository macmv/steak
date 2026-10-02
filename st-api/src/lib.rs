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
