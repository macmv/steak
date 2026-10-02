//! Access layers required before starting a non-DRM YouTube stream.
//!
//! Each submodule owns one independently changing protocol concern. Signature
//! and `n` challenges are handled by `playback::solver`.

pub mod po_token;
