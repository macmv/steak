//! Acquisition of a GVS Proof-of-Origin (`pot`) token.
//!
//! Requirements:
//! - the Innertube client identity and its GVS PoToken policy;
//! - the authenticated browser/session context when the policy requires it;
//! - a PoToken provider for that context.
//!
//! Add the returned token as `pot` to an HTTPS media URL (or in the manifest
//! path for HLS/DASH). If a required token cannot be acquired, reject the
//! offer and try another non-DRM offer.

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PoTokenError {
  Unsupported,
  FetchFailed(String),
}
impl std::fmt::Display for PoTokenError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Unsupported => f.write_str("no GVS PoToken provider is configured"),
      Self::FetchFailed(e) => write!(f, "PoToken fetch failed: {e}"),
    }
  }
}
impl std::error::Error for PoTokenError {}

pub trait PoTokenProvider<O> {
  fn fetch_gvs_po_token(&self, offer: &O) -> Result<String, PoTokenError>;
}

/// Placeholder until an authenticated PoToken provider is implemented.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnsupportedPoTokenProvider;
impl<O> PoTokenProvider<O> for UnsupportedPoTokenProvider {
  fn fetch_gvs_po_token(&self, _: &O) -> Result<String, PoTokenError> {
    Err(PoTokenError::Unsupported)
  }
}
