//! Player-script deciphering extension points.
//!
//! Some player responses encode a stream URL as `signatureCipher` and may also
//! require transforming its `n` query parameter. The player JavaScript changes
//! regularly, so keep this logic replaceable and fixture-tested.

use reqwest::Url;

/// A URL embedded in a player response before any deciphering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamUrl {
  Direct(Url),
  SignatureCipher(String),
}

/// Resolves a usable, short-lived media URL from player data.
pub trait CipherResolver {
  fn resolve(&self, player_script_url: &Url, stream: &StreamUrl) -> Result<Url, CipherError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CipherError {
  CipheredSignature,
  MissingSignature,
  InvalidUrl,
  PlayerScriptChanged,
}

impl std::fmt::Display for CipherError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::CipheredSignature => {
        f.write_str("cipher payload requires player-script signature transformation")
      }
      Self::MissingSignature => f.write_str("cipher payload has no signature"),
      Self::InvalidUrl => f.write_str("cipher payload contains an invalid URL"),
      Self::PlayerScriptChanged => f.write_str("player script extraction no longer matches"),
    }
  }
}

impl std::error::Error for CipherError {}

/// Resolves direct URLs and cipher payloads that already contain a signature.
///
/// A payload with an `s` field is deliberately rejected: it needs a transform
/// extracted from the current player JavaScript.
#[derive(Clone, Debug, Default)]
pub struct DirectUrlResolver;

impl CipherResolver for DirectUrlResolver {
  fn resolve(&self, _: &Url, stream: &StreamUrl) -> Result<Url, CipherError> {
    match stream {
      StreamUrl::Direct(url) => Ok(url.clone()),
      StreamUrl::SignatureCipher(cipher) => {
        let form = Url::parse(&format!("https://cipher.invalid/?{cipher}"))
          .map_err(|_| CipherError::InvalidUrl)?;
        let fields: std::collections::HashMap<_, _> = form.query_pairs().into_owned().collect();
        if fields.contains_key("s") {
          return Err(CipherError::CipheredSignature);
        }
        let mut url = Url::parse(fields.get("url").ok_or(CipherError::InvalidUrl)?)
          .map_err(|_| CipherError::InvalidUrl)?;
        if let Some(signature) = fields.get("sig").or_else(|| fields.get("signature")) {
          let parameter = fields.get("sp").map(String::as_str).unwrap_or("signature");
          url.query_pairs_mut().append_pair(parameter, signature);
        }
        Ok(url)
      }
    }
  }
}
