//! Authenticated request support.
//!
//! Browser cookies and request signatures must be supplied by an application
//! integration. This module defines the hand-off point without storing secrets
//! in the API client itself.

use std::{
  fs, io,
  path::{Path, PathBuf},
};

use reqwest::header::HeaderMap;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

/// The conventional local session-file location used by the CLI and tests.
pub fn session_path() -> PathBuf {
  std::env::var_os("HOME")
    .map(PathBuf::from)
    .unwrap_or_else(|| PathBuf::from("."))
    .join(".config/steak/session.json")
}

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
      if let Some(sapisid) =
        cookie_value(cookies, &["SAPISID", "__Secure-3PAPISID", "__Secure-1PAPISID"])
      {
        let timestamp = std::time::SystemTime::now()
          .duration_since(std::time::UNIX_EPOCH)
          .map(|duration| duration.as_secs())
          .unwrap_or_default();
        let digest = sapisid_hash(timestamp, sapisid, "https://music.youtube.com");
        if let Ok(value) = format!("SAPISIDHASH {timestamp}_{digest}").parse() {
          headers.insert(reqwest::header::AUTHORIZATION, value);
        }
      }
    }
    headers
  }

  pub fn from_cookie_header(cookies: String) -> Self {
    Self { cookies: Some(cookies), headers: HeaderMap::new() }
  }
}

fn cookie_value<'a>(cookies: &'a str, names: &[&str]) -> Option<&'a str> {
  cookies
    .split(';')
    .filter_map(|cookie| cookie.trim().split_once('='))
    .find_map(|(name, value)| names.contains(&name.trim()).then_some(value.trim()))
}

fn sapisid_hash(timestamp: u64, sapisid: &str, origin: &str) -> String {
  let mut hasher = Sha1::new();
  hasher.update(format!("{timestamp} {sapisid} {origin}"));
  hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A locally imported browser session. Treat it like a password.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoredSession {
  pub cookie_header: String,
}

impl StoredSession {
  pub fn auth_session(&self) -> AuthSession {
    AuthSession::from_cookie_header(self.cookie_header.clone())
  }

  /// Writes the session with owner-only permissions. It goes through a temp
  /// file and a rename, so a crash never leaves a half-written session.
  pub fn save(&self, path: &Path) -> Result<(), SessionError> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
      fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(self)?;
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let result = (|| {
      let mut options = fs::OpenOptions::new();
      options.write(true).create(true).truncate(true);
      #[cfg(unix)]
      std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
      options.open(&temp)?.write_all(&bytes)?;
      fs::rename(&temp, path)
    })();
    if result.is_err() {
      let _ = fs::remove_file(&temp);
    }
    Ok(result?)
  }

  pub fn load(path: &Path) -> Result<Self, SessionError> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
  }
}

/// Imports YouTube cookies from a Firefox profile without modifying the
/// profile.
///
/// Firefox keeps recent writes in `cookies.sqlite-wal` until it checkpoints, so
/// the log is copied along with the database; without it a fresh login can be
/// missing.
pub fn import_firefox_profile(profile: &Path) -> Result<StoredSession, SessionError> {
  static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

  let source = profile.join("cookies.sqlite");
  if !source.is_file() {
    return Err(SessionError::MissingCookiesDatabase(source));
  }
  let scratch = ScratchDir(std::env::temp_dir().join(format!(
    "steak-firefox-{}-{}",
    std::process::id(),
    COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
  )));
  fs::create_dir_all(&scratch.0)?;
  for name in ["cookies.sqlite", "cookies.sqlite-wal", "cookies.sqlite-shm"] {
    match fs::copy(profile.join(name), scratch.0.join(name)) {
      Ok(_) => {}
      // The log and shared-memory files only exist while Firefox is running.
      Err(error) if error.kind() == io::ErrorKind::NotFound && name != "cookies.sqlite" => {}
      Err(error) => return Err(error.into()),
    }
  }
  read_youtube_cookies(&scratch.0.join("cookies.sqlite"))
}

/// A private directory that is deleted when it goes out of scope.
struct ScratchDir(PathBuf);

impl Drop for ScratchDir {
  fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
}

fn read_youtube_cookies(path: &Path) -> Result<StoredSession, SessionError> {
  let connection = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
  let mut statement = connection.prepare(
    "SELECT name, value FROM moz_cookies WHERE host = 'youtube.com' OR host LIKE '%.youtube.com'",
  )?;
  let cookies = statement
    .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
    .collect::<Result<Vec<_>, _>>()?;
  if cookies.is_empty() {
    return Err(SessionError::NoYoutubeCookies);
  }
  Ok(StoredSession {
    cookie_header: cookies
      .into_iter()
      .map(|(name, value)| format!("{name}={value}"))
      .collect::<Vec<_>>()
      .join("; "),
  })
}

#[derive(Debug)]
pub enum SessionError {
  Io(io::Error),
  Sql(rusqlite::Error),
  Json(serde_json::Error),
  MissingCookiesDatabase(PathBuf),
  NoYoutubeCookies,
}

impl From<io::Error> for SessionError {
  fn from(error: io::Error) -> Self { Self::Io(error) }
}
impl From<rusqlite::Error> for SessionError {
  fn from(error: rusqlite::Error) -> Self { Self::Sql(error) }
}
impl From<serde_json::Error> for SessionError {
  fn from(error: serde_json::Error) -> Self { Self::Json(error) }
}

impl std::fmt::Display for SessionError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Io(error) => write!(f, "session file error: {error}"),
      Self::Sql(error) => write!(f, "Firefox cookie database error: {error}"),
      Self::Json(error) => write!(f, "session file JSON error: {error}"),
      Self::MissingCookiesDatabase(path) => {
        write!(f, "Firefox cookies database not found: {}", path.display())
      }
      Self::NoYoutubeCookies => f.write_str("no YouTube cookies found in Firefox profile"),
    }
  }
}
impl std::error::Error for SessionError {}

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
