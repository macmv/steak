//! Resolution of the media URL's `n` challenge.
//!
//! Requirements:
//! - the exact player JavaScript URL/version that produced the media URL;
//! - the original `n` value;
//! - an EJS-compatible JavaScript challenge solver and runtime.
//!
//! Resolution must return a replacement `n`. Never send the original value as
//! a fallback after a solve failure; reject this offer and try another one.

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NChallengeError {
  Unsupported,
  SolveFailed(String),
}
impl std::fmt::Display for NChallengeError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Unsupported => f.write_str("no n-challenge solver is configured"),
      Self::SolveFailed(e) => write!(f, "n challenge solve failed: {e}"),
    }
  }
}
impl std::error::Error for NChallengeError {}

pub trait NChallengeResolver {
  fn solve_n(&self, player_url: &reqwest::Url, challenge: &str) -> Result<String, NChallengeError>;
}

/// Placeholder until the EJS solver integration is installed.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnsupportedNChallengeResolver;
impl NChallengeResolver for UnsupportedNChallengeResolver {
  fn solve_n(&self, _: &reqwest::Url, _: &str) -> Result<String, NChallengeError> {
    Err(NChallengeError::Unsupported)
  }
}
