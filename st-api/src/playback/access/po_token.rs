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

use crate::media::audio::AudioClient;

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

/// Mints PoTokens by running the `rustypipe-botguard` CLI
/// (`cargo install rustypipe-botguard`), which solves YouTube's BotGuard
/// challenge in an embedded JS runtime.
///
/// Streams are authorized with a token bound to the video ID: without it only
/// the first range of a stream is served and later ranges get a 403.
#[derive(Clone, Debug)]
pub struct BotguardCliProvider {
  binary: std::path::PathBuf,
}

impl BotguardCliProvider {
  pub const DEFAULT_BINARY: &'static str = "rustypipe-botguard";

  pub fn new(binary: impl Into<std::path::PathBuf>) -> Self { Self { binary: binary.into() } }

  /// Mints a token bound to `identifier` (a video ID). Blocking: it spawns a
  /// process and waits for it, so call it from `spawn_blocking` in async code.
  pub fn mint(&self, identifier: &str) -> Result<String, PoTokenError> {
    let output = std::process::Command::new(&self.binary)
      // Never use the snapshot: it freezes one BotGuard session, and roughly a
      // quarter of sessions mint tokens that googlevideo rejects, so a bad
      // snapshot would fail every time until it expires.
      .arg("--no-snapshot")
      // Video IDs may start with `-`, so separate them from the options.
      .args(["--", identifier])
      .output()
      .map_err(|error| {
        let hint = if error.kind() == std::io::ErrorKind::NotFound {
          " (install it with `cargo install rustypipe-botguard`, or set STEAK_BOTGUARD)".to_owned()
        } else {
          String::new()
        };
        PoTokenError::FetchFailed(format!("could not run {}: {error}{hint}", self.binary.display()))
      })?;
    if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr);
      return Err(PoTokenError::FetchFailed(format!(
        "{} exited with {}: {}",
        self.binary.display(),
        output.status,
        stderr.trim()
      )));
    }
    // Output is `<token> key=value ...`; the metadata is not needed.
    String::from_utf8_lossy(&output.stdout)
      .split_whitespace()
      .next()
      .map(str::to_owned)
      .ok_or_else(|| PoTokenError::FetchFailed("botguard printed no token".into()))
  }
}

impl Default for BotguardCliProvider {
  fn default() -> Self { Self::new(Self::DEFAULT_BINARY) }
}

/// How many fresh BotGuard sessions to try before giving up.
///
/// Roughly 40-50% of sessions mint tokens googlevideo rejects. The reason is
/// upstream of us: YouTube's `GenerateIT` endpoint sometimes judges the
/// emulated-browser attestation as not genuine and quietly returns a degraded
/// integrity token (88 or 92 characters instead of 84 in testing) whose
/// well-formed tokens are refused for every range after a stream's first
/// chunk. Nothing in the token says so, and the rate is independent of CPU
/// load, so the only reliable check is asking googlevideo. Ten attempts at
/// that rate fail about one time in a thousand.
const MAX_ATTEMPTS: usize = 10;
/// googlevideo serves the first chunk of a stream without a valid token, so a
/// probe has to land past it.
const PROBE_OFFSET: u64 = 2 * 1024 * 1024;

/// Where to probe for authorization: past the unauthenticated first chunk, but
/// inside the file.
fn probe_offset(url: &reqwest::Url) -> u64 {
  let clen = url
    .query_pairs()
    .find(|(key, _)| key == "clen")
    .and_then(|(_, value)| value.parse::<u64>().ok())
    .filter(|clen| *clen > 0);
  match clen {
    Some(clen) => (clen - 1).min(PROBE_OFFSET),
    None => PROBE_OFFSET,
  }
}

/// Asks googlevideo whether `url` is authorized for the whole stream.
async fn is_authorized(audio: &AudioClient, url: &reqwest::Url) -> Result<bool, PoTokenError> {
  match audio.probe_status(url, probe_offset(url)).await {
    Ok(status) if status.is_success() => Ok(true),
    Ok(reqwest::StatusCode::FORBIDDEN) => Ok(false),
    Ok(status) => Err(PoTokenError::FetchFailed(format!("probe returned {status}"))),
    Err(error) => Err(PoTokenError::FetchFailed(format!("probe failed: {error}"))),
  }
}

/// Returns a stream URL that googlevideo will serve in full.
///
/// A token is only minted if the plain URL is refused. A BotGuard session can
/// mint tokens googlevideo rejects (about a quarter of them in practice), and
/// there is no way to tell from the token itself, so each token is checked
/// against the server and replaced with one from a new session if refused.
pub async fn authorize_stream(
  provider: &BotguardCliProvider,
  audio: &AudioClient,
  mut url: reqwest::Url,
  video_id: &str,
) -> Result<reqwest::Url, PoTokenError> {
  if is_authorized(audio, &url).await? {
    log::debug!("stream needs no PoToken");
    return Ok(url);
  }
  let mut last_error = None;
  for attempt in 1..=MAX_ATTEMPTS {
    let (minter, id) = (provider.clone(), video_id.to_owned());
    let token = tokio::task::spawn_blocking(move || minter.mint(&id))
      .await
      .map_err(|error| PoTokenError::FetchFailed(error.to_string()))?;
    match token {
      Ok(token) => {
        apply_po_token(&mut url, &token);
        if is_authorized(audio, &url).await? {
          log::debug!("PoToken accepted on attempt {attempt}");
          return Ok(url);
        }
        log::debug!("PoToken rejected on attempt {attempt}; minting a new one");
      }
      Err(error) => {
        // Often a transient error from YouTube's BotGuard endpoint.
        log::warn!("{error} (attempt {attempt}/{MAX_ATTEMPTS})");
        last_error = Some(error);
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
      }
    }
  }
  Err(last_error.unwrap_or_else(|| {
    PoTokenError::FetchFailed(format!("googlevideo rejected {MAX_ATTEMPTS} PoTokens in a row"))
  }))
}

/// Sets the `pot` query parameter on a media URL, replacing any existing one.
///
/// The rest of the query is left byte-for-byte as it was: googlevideo URLs are
/// signed, and re-encoding their parameters gets the request rejected.
pub fn apply_po_token(url: &mut reqwest::Url, token: &str) {
  let mut query = url
    .query()
    .unwrap_or_default()
    .split('&')
    .filter(|pair| !pair.is_empty() && !pair.starts_with("pot="))
    .collect::<Vec<_>>()
    .join("&");
  if !query.is_empty() {
    query.push('&');
  }
  // Tokens are URL-safe base64, which may end in `=` padding.
  query.push_str(&format!("pot={}", token.replace('=', "%3D")));
  url.set_query(Some(&query));
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn probe_offset_stays_inside_the_file_and_past_the_first_chunk() {
    let url = |query: &str| reqwest::Url::parse(&format!("https://e.test/v?{query}")).unwrap();
    assert_eq!(probe_offset(&url("clen=3990497")), 2 * 1024 * 1024);
    assert_eq!(probe_offset(&url("clen=1000")), 999);
    assert_eq!(probe_offset(&url("itag=1")), PROBE_OFFSET);
  }

  #[test]
  fn apply_po_token_keeps_signed_query_intact() {
    let mut url =
      reqwest::Url::parse("https://example.test/v?sparams=expire,ei&pot=old&sig=a%2Fb").unwrap();
    apply_po_token(&mut url, "tok_-==");
    assert_eq!(url.query(), Some("sparams=expire,ei&sig=a%2Fb&pot=tok_-%3D%3D"));
  }
}
