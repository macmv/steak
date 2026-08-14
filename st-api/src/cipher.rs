//! Player-script deciphering extension points.
//!
//! Some player responses encode a stream URL as `signatureCipher` and may also
//! require transforming its `n` query parameter. The player JavaScript changes
//! regularly, so keep this logic replaceable and fixture-tested.

use boa_engine::{Context, Source};
use reqwest::Url;
use serde_json::to_string;

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
  JavaScript(String),
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
      Self::JavaScript(error) => write!(f, "player cipher JavaScript failed: {error}"),
    }
  }
}

/// Evaluates an already-extracted signature function with Boa.
///
/// Function extraction is intentionally separate because YouTube changes the
/// surrounding minified player code. `function_expression` must evaluate to a
/// unary JavaScript function accepting and returning a string.
pub fn evaluate_signature(function_expression: &str, input: &str) -> Result<String, CipherError> {
  let input = to_string(input).map_err(|error| CipherError::JavaScript(error.to_string()))?;
  let source = format!("({function_expression})({input})");
  let mut context = Context::default();
  let value = context
    .eval(Source::from_bytes(source.as_bytes()))
    .map_err(|error| CipherError::JavaScript(error.to_string()))?;
  value
    .to_string(&mut context)
    .map(|value| value.to_std_string_escaped())
    .map_err(|error| CipherError::JavaScript(error.to_string()))
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

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn boa_evaluates_signature_function() {
    let output = evaluate_signature(
      "function(value) { return value.split('').reverse().join(''); }",
      "abcdef",
    )
    .unwrap();
    assert_eq!(output, "fedcba");
  }
}
