//! Turns a player response's stream URL into a usable, short-lived media URL.
//!
//! Some formats carry a `signatureCipher` whose `s` value must be transformed
//! by the current player JavaScript, and most media URLs carry an `n` throttle
//! parameter that needs the same treatment. The transforms are solved by
//! [`crate::playback::solver`]; this module only fetches the player, parses the
//! cipher and rebuilds the URL.

use std::{
  collections::HashMap,
  sync::{Arc, Mutex},
};

use log::debug;
use reqwest::{Client, Url};

use super::solver::{self, Solver, SolverError};

/// A URL embedded in a player response before any deciphering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamUrl {
  Direct(Url),
  SignatureCipher(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CipherError {
  MissingSignature,
  InvalidUrl,
  /// Downloading the player script failed.
  Fetch(String),
  Solver(SolverError),
  /// The solver ran but gave no answer for this challenge.
  Unsolved,
}

impl std::fmt::Display for CipherError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::MissingSignature => f.write_str("cipher payload has no signature"),
      Self::InvalidUrl => f.write_str("cipher payload contains an invalid URL"),
      Self::Fetch(error) => write!(f, "could not download the player script: {error}"),
      Self::Solver(error) => error.fmt(f),
      Self::Unsolved => f.write_str("the player script produced no answer for a challenge"),
    }
  }
}

impl std::error::Error for CipherError {}

impl From<SolverError> for CipherError {
  fn from(error: SolverError) -> Self { Self::Solver(error) }
}

/// A `signatureCipher` payload, split into its parts.
#[derive(Debug, Eq, PartialEq)]
struct Cipher {
  url:       Url,
  /// Query parameter that receives the signature (`sp`).
  parameter: String,
  signature: Signature,
}

#[derive(Debug, Eq, PartialEq)]
enum Signature {
  /// A direct URL: there is no signature to add.
  None,
  /// Already usable as-is.
  Ready(String),
  /// The `s` value; it must be solved against the player script.
  Encrypted(String),
}

fn cipher_for(stream: &StreamUrl) -> Result<Cipher, CipherError> {
  match stream {
    StreamUrl::Direct(url) => {
      Ok(Cipher { url: url.clone(), parameter: String::new(), signature: Signature::None })
    }
    StreamUrl::SignatureCipher(cipher) => parse_cipher(cipher),
  }
}

fn parse_cipher(cipher: &str) -> Result<Cipher, CipherError> {
  let form = Url::parse(&format!("https://cipher.invalid/?{cipher}"))
    .map_err(|_| CipherError::InvalidUrl)?;
  let fields: HashMap<_, _> = form.query_pairs().into_owned().collect();
  debug!(
    "resolving signature cipher; fields: url={}, sig={}, s={}, sp={}",
    fields.contains_key("url"),
    fields.contains_key("sig") || fields.contains_key("signature"),
    fields.contains_key("s"),
    fields.contains_key("sp"),
  );
  let url = Url::parse(fields.get("url").ok_or(CipherError::InvalidUrl)?)
    .map_err(|_| CipherError::InvalidUrl)?;
  let parameter = fields.get("sp").cloned().unwrap_or_else(|| "signature".into());
  let signature = if let Some(ready) = fields.get("sig").or_else(|| fields.get("signature")) {
    Signature::Ready(ready.clone())
  } else {
    Signature::Encrypted(fields.get("s").ok_or(CipherError::MissingSignature)?.clone())
  };
  Ok(Cipher { url, parameter, signature })
}

fn query_value(url: &Url, name: &str) -> Option<String> {
  url.query_pairs().find(|(key, _)| key == name).map(|(_, value)| value.into_owned())
}

/// Rewrites the query with `name` replaced, keeping every other pair in order.
fn replace_query_value(url: &mut Url, name: &str, value: &str) {
  let pairs: Vec<(String, String)> = url
    .query_pairs()
    .map(|(key, old)| {
      let new = if key == name { value.to_owned() } else { old.into_owned() };
      (key.into_owned(), new)
    })
    .collect();
  url.set_query(None);
  url.query_pairs_mut().extend_pairs(pairs);
}

/// Fetches a player script and resolves `signatureCipher` URLs with the EJS
/// solver.
#[derive(Clone, Debug, Default)]
pub struct EjsCipherResolver {
  http:    Client,
  players: Arc<Mutex<HashMap<String, Arc<str>>>>,
}

impl EjsCipherResolver {
  pub fn new() -> Self { Self::default() }

  pub async fn resolve(
    &self,
    player_script_url: &Url,
    stream: &StreamUrl,
  ) -> Result<Url, CipherError> {
    let cipher = cipher_for(stream)?;
    let (sigs, ns) = challenges(&cipher);
    debug!(
      "media URL parameters: signature={}, n={}",
      matches!(cipher.signature, Signature::Ready(_)),
      !ns.is_empty()
    );

    let solved = if sigs.is_empty() && ns.is_empty() {
      solver::Solved::default()
    } else {
      let script = self.script(player_script_url).await?;
      tokio::task::spawn_blocking(move || solver::solve(&script, &sigs, &ns))
        .await
        .map_err(|error| SolverError::Runtime(error.to_string()))??
    };
    assemble(cipher, &solved)
  }

  /// Like [`Self::resolve`], but with the player script already in hand.
  pub fn resolve_with_script(&self, script: &str, stream: &StreamUrl) -> Result<Url, CipherError> {
    let cipher = cipher_for(stream)?;
    let (sigs, ns) = challenges(&cipher);
    if sigs.is_empty() && ns.is_empty() {
      return assemble(cipher, &solver::Solved::default());
    }
    assemble(cipher, &solver::solve(script, &sigs, &ns)?)
  }

  /// Downloads (or returns the cached copy of) the player script.
  pub async fn script(&self, player_script_url: &Url) -> Result<Arc<str>, CipherError> {
    let player_id = player_cache_key(player_script_url);
    if let Some(script) = self.players.lock().expect("player cache lock poisoned").get(&player_id) {
      debug!("using cached player JavaScript for {player_id}");
      return Ok(script.clone());
    }
    debug!(
      "fetching player JavaScript from host {:?}, path {}",
      player_script_url.host_str(),
      player_script_url.path()
    );
    let fetch = |error: reqwest::Error| CipherError::Fetch(error.to_string());
    let response = self.http.get(player_script_url.clone()).send().await.map_err(fetch)?;
    debug!("player JavaScript response status={}", response.status());
    let script: Arc<str> =
      response.error_for_status().map_err(fetch)?.text().await.map_err(fetch)?.into();
    debug!("downloaded player JavaScript ({} bytes)", script.len());
    self.players.lock().expect("player cache lock poisoned").insert(player_id, script.clone());
    Ok(script)
  }
}

/// Resolves a stream URL with an already loaded [`Solver`].
pub async fn resolve_with_solver(stream: &StreamUrl, solver: &Solver) -> Result<Url, CipherError> {
  let cipher = cipher_for(stream)?;
  let (sigs, ns) = challenges(&cipher);
  let solved = if sigs.is_empty() && ns.is_empty() {
    solver::Solved::default()
  } else {
    solver.solve(sigs, ns).await?
  };
  assemble(cipher, &solved)
}

/// Resolves a stream URL by solving against the full player script. Also
/// returns the preprocessed player, which a later [`Solver`] can load instead
/// of this slow path.
pub async fn resolve_and_preprocess(
  script: Arc<str>,
  stream: &StreamUrl,
) -> Result<(Url, Option<String>), CipherError> {
  let cipher = cipher_for(stream)?;
  let (sigs, ns) = challenges(&cipher);
  let solved = if sigs.is_empty() && ns.is_empty() {
    solver::Solved::default()
  } else {
    tokio::task::spawn_blocking(move || solver::solve(&script, &sigs, &ns))
      .await
      .map_err(|error| SolverError::Runtime(error.to_string()))??
  };
  let preprocessed = solved.preprocessed.clone();
  Ok((assemble(cipher, &solved)?, preprocessed))
}

/// Reads the player's `signatureTimestamp`.
///
/// The `player` request must send this value so the ciphers it returns match
/// the player script we will solve them with.
pub fn signature_timestamp(script: &str) -> Option<u32> {
  const MARKER: &str = "signatureTimestamp:";
  let rest = &script[script.find(MARKER)? + MARKER.len()..];
  let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
  rest[..digits].parse().ok()
}

/// The `s` and `n` values this cipher needs solved.
fn challenges(cipher: &Cipher) -> (Vec<String>, Vec<String>) {
  let sigs = match &cipher.signature {
    Signature::Encrypted(s) => vec![s.clone()],
    Signature::Ready(_) | Signature::None => vec![],
  };
  (sigs, query_value(&cipher.url, "n").into_iter().collect())
}

/// Applies solver answers to the cipher's URL.
///
/// A missing answer is an error: sending the original `n` or an unsolved `s`
/// would get the request throttled or rejected, so callers should try another
/// format instead.
fn assemble(cipher: Cipher, solved: &solver::Solved) -> Result<Url, CipherError> {
  let Cipher { mut url, parameter, signature } = cipher;
  let signature = match signature {
    Signature::None => None,
    Signature::Ready(ready) => Some(ready),
    Signature::Encrypted(s) => Some(solved.sig.get(&s).cloned().ok_or(CipherError::Unsolved)?),
  };
  if let Some(signature) = signature {
    url.query_pairs_mut().append_pair(&parameter, &signature);
  }
  if let Some(n) = query_value(&url, "n") {
    let transformed = solved.n.get(&n).ok_or(CipherError::Unsolved)?;
    replace_query_value(&mut url, "n", transformed);
    debug!("transformed n parameter ({} -> {} bytes)", n.len(), transformed.len());
  }
  Ok(url)
}

/// The player version in a script URL (`8ab5c328` in `/s/player/8ab5c328/...`).
pub fn player_cache_key(url: &Url) -> String {
  url
    .path_segments()
    .and_then(|segments| {
      segments
        .collect::<Vec<_>>()
        .windows(2)
        .find_map(|pair| (pair[0] == "player").then(|| pair[1].to_owned()))
    })
    .unwrap_or_else(|| url.as_str().to_owned())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn reads_signature_timestamp() {
    let script = include_str!("../../tests/fixtures/players/8ab5c328.js");
    assert_eq!(signature_timestamp(script), Some(20725));
    assert_eq!(signature_timestamp("a={signatureTimestamp:19999,b:1}"), Some(19999));
    assert_eq!(signature_timestamp("signatureTimestamp:abc"), None);
    assert_eq!(signature_timestamp("nothing here"), None);
  }

  #[test]
  fn direct_url_without_n_is_returned_unchanged() {
    let url = Url::parse("https://example.test/a?x=1").unwrap();
    let resolved =
      EjsCipherResolver::new().resolve_with_script("", &StreamUrl::Direct(url.clone())).unwrap();
    assert_eq!(resolved, url);
  }

  #[test]
  fn direct_url_with_n_gets_n_solved() {
    let script = include_str!("../../tests/fixtures/players/8ab5c328.js");
    let url = Url::parse("https://example.test/a?x=1&n=abcdefghijk").unwrap();
    let resolved =
      EjsCipherResolver::new().resolve_with_script(script, &StreamUrl::Direct(url)).unwrap();
    assert_eq!(resolved.query(), Some("x=1&n=VXuZqhSdiY"));
  }

  #[test]
  fn parses_encrypted_cipher() {
    let cipher = parse_cipher("url=https%3A%2F%2Fexample.test%2Fa%3Fn%3Dabc&s=xyz&sp=sig").unwrap();
    assert_eq!(cipher.parameter, "sig");
    assert_eq!(cipher.signature, Signature::Encrypted("xyz".into()));
    assert_eq!(query_value(&cipher.url, "n").as_deref(), Some("abc"));
  }

  #[test]
  fn parses_ready_cipher_with_default_parameter() {
    let cipher = parse_cipher("url=https%3A%2F%2Fexample.test%2Fa&sig=done").unwrap();
    assert_eq!(cipher.parameter, "signature");
    assert_eq!(cipher.signature, Signature::Ready("done".into()));
  }

  #[test]
  fn rejects_cipher_without_signature_or_url() {
    assert_eq!(
      parse_cipher("url=https%3A%2F%2Fexample.test%2Fa").unwrap_err(),
      CipherError::MissingSignature
    );
    assert_eq!(parse_cipher("s=xyz").unwrap_err(), CipherError::InvalidUrl);
  }

  #[test]
  fn replaces_n_and_keeps_other_pairs_in_order() {
    let mut url = Url::parse("https://example.test/a?x=1&n=abc&y=2").unwrap();
    replace_query_value(&mut url, "n", "NEW");
    assert_eq!(url.query(), Some("x=1&n=NEW&y=2"));
  }

  #[test]
  fn unsolved_challenge_is_an_error_not_a_fallback() {
    let cipher = parse_cipher("url=https%3A%2F%2Fexample.test%2Fa%3Fn%3Dabc&sig=done").unwrap();
    assert_eq!(assemble(cipher, &solver::Solved::default()).unwrap_err(), CipherError::Unsolved);
  }

  /// End to end on a saved real player. Expected values were produced by the
  /// upstream solver bundles in deno; see `solver::tests`.
  #[test]
  fn resolves_sig_and_n_on_real_player() {
    let script = include_str!("../../tests/fixtures/players/8ab5c328.js");
    let sig = "ABCDEFGHIJKLMNOPQRSTUVWXYZ".repeat(3);
    let stream = StreamUrl::SignatureCipher(format!(
      "url=https%3A%2F%2Fexample.test%2Faudio%3Fn%3Dabcdefghijk&s={sig}&sp=sig"
    ));
    let url = EjsCipherResolver::new().resolve_with_script(script, &stream).unwrap();
    assert_eq!(query_value(&url, "n").as_deref(), Some("VXuZqhSdiY"));
    assert_eq!(
      query_value(&url, "sig").as_deref(),
      Some("YXWVUTSRQPONMLKGIHGFEDCBAZYXWVUTSRQPONMLKJIHGFEDCBAZYXWVUTSRQZONMLKJIHAFEDCB")
    );
  }
}
