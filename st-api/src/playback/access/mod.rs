//! Access layers required before starting a non-DRM YouTube stream.
//!
//! Each submodule owns one independently changing protocol concern.

pub mod n_challenge;
pub mod po_token;
pub mod signature;
