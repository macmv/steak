//! Authenticated request support.
//!
//! Browser cookies and request signatures must be supplied by an application
//! integration. This module defines the hand-off point without storing secrets
//! in the API client itself.

use reqwest::header::HeaderMap;

/// Authentication material for a single request origin.
#[derive(Clone, Debug, Default)]
pub struct AuthSession {
  /// Browser-equivalent cookies, including any secure account cookies.
  pub cookies: Option<String>,
  /// Additional headers, such as an authorization or identity token.
  pub headers: HeaderMap,
}

impl AuthSession {
  /// Combines the session data into headers suitable for `music.youtube.com`.
  pub fn request_headers(&self) -> HeaderMap {
    let mut headers = self.headers.clone();
    if let Some(cookies) = &self.cookies {
      if let Ok(value) = cookies.parse() {
        headers.insert(reqwest::header::COOKIE, value);
      }
    }
    headers
  }
}

/// Builds the per-request authorization header required by an authenticated
/// browser session (for example, a time-bound SAPISID hash).
pub trait RequestAuthorizer {
  fn authorize(&self, origin: &str, now_unix_seconds: u64) -> Result<HeaderMap, AuthError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthError {
  NotImplemented,
  MissingCookie(&'static str),
  InvalidHeader,
}

impl std::fmt::Display for AuthError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::NotImplemented => f.write_str("request authorization is not implemented"),
      Self::MissingCookie(name) => write!(f, "required cookie is missing: {name}"),
      Self::InvalidHeader => f.write_str("generated an invalid authorization header"),
    }
  }
}

impl std::error::Error for AuthError {}
