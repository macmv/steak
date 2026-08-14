//! Player-script deciphering extension points.
//!
//! Some player responses encode a stream URL as `signatureCipher` and may also
//! require transforming its `n` query parameter. The player JavaScript changes
//! regularly, so keep this logic replaceable and fixture-tested.

use boa_ast::scope::Scope;
use boa_engine::{Context, Source};
use boa_interner::Interner;
use boa_parser::{Parser, Source as ParserSource};
use log::{debug, warn};
use reqwest::{Client, Url};
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
  MissingFunction,
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
      Self::MissingFunction => {
        f.write_str("could not find a signature function in player JavaScript")
      }
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

/// Fetches a player script and resolves a `signatureCipher` URL with Boa.
#[derive(Clone, Debug, Default)]
pub struct BoaCipherResolver {
  http: Client,
}

impl BoaCipherResolver {
  pub fn new() -> Self { Self { http: Client::new() } }

  pub async fn resolve(
    &self,
    player_script_url: &Url,
    stream: &StreamUrl,
  ) -> Result<Url, CipherError> {
    debug!(
      "fetching player JavaScript from host {:?}, path {}",
      player_script_url.host_str(),
      player_script_url.path()
    );
    let response = self
      .http
      .get(player_script_url.clone())
      .send()
      .await
      .map_err(|error| CipherError::JavaScript(error.to_string()))?;
    debug!("player JavaScript response status={}", response.status());
    let script = response
      .error_for_status()
      .map_err(|error| CipherError::JavaScript(error.to_string()))?
      .text()
      .await
      .map_err(|error| CipherError::JavaScript(error.to_string()))?;
    debug!("downloaded player JavaScript ({} bytes)", script.len());
    self.resolve_script(&script, stream)
  }

  pub fn resolve_script(&self, script: &str, stream: &StreamUrl) -> Result<Url, CipherError> {
    log::debug!(
      "evaluating player script: bytes={}, split={}, join={}, function={}",
      script.len(),
      script.contains(".split"),
      script.contains(".join"),
      script.contains("function"),
    );
    match stream {
      StreamUrl::Direct(url) => Ok(url.clone()),
      StreamUrl::SignatureCipher(cipher) => {
        let form = Url::parse(&format!("https://cipher.invalid/?{cipher}"))
          .map_err(|_| CipherError::InvalidUrl)?;
        let fields: std::collections::HashMap<_, _> = form.query_pairs().into_owned().collect();
        debug!(
          "resolving signature cipher; fields: url={}, sig={}, s={}, sp={}",
          fields.contains_key("url"),
          fields.contains_key("sig") || fields.contains_key("signature"),
          fields.contains_key("s"),
          fields.contains_key("sp"),
        );
        let mut url = Url::parse(fields.get("url").ok_or(CipherError::InvalidUrl)?)
          .map_err(|_| CipherError::InvalidUrl)?;
        debug!("media URL parameters: signature={}, n={}", url.query_pairs().any(|(k, _)| k == "sig" || k == "signature"), url.query_pairs().any(|(k, _)| k == "n"));
        if let Some(signature) = fields.get("sig").or_else(|| fields.get("signature")) {
          debug!("signature cipher already has a signature; no JavaScript transformation needed");
          let parameter = fields.get("sp").map(String::as_str).unwrap_or("signature");
          url.query_pairs_mut().append_pair(parameter, signature);
          return Ok(url);
        }
        let encrypted = fields.get("s").ok_or(CipherError::MissingSignature)?;
        let function = match extract_signature_function(script) {
          Some(function) => {
            debug!("extracted a {}-byte signature transformation function", function.len());
            function
          }
          None => {
            warn!(
              "no supported signature transformation was found in player JavaScript ({} bytes; function={}, split={}, join={})",
              script.len(),
              script.contains("function"),
              script.contains(".split("),
              script.contains(".join("),
            );
            return Err(CipherError::MissingFunction);
          }
        };
        let signature = evaluate_signature(function, encrypted)?;
        debug!(
          "signature transformation evaluated successfully ({} output bytes)",
          signature.len()
        );
        let parameter = fields.get("sp").map(String::as_str).unwrap_or("signature");
        url.query_pairs_mut().append_pair(parameter, &signature);
        Ok(url)
      }
    }
  }
}

/// Extracts the common unary split/transform/join signature function shape.
/// This intentionally fails closed when the player deploys a different shape.
pub fn extract_signature_function(script: &str) -> Option<&str> {
  let mut offset = 0;
  let mut first_candidate = None;
  while let Some(split_offset) = script[offset..].find(".split") {
    let split_offset = offset + split_offset;
    let Some(function_start) = script[..split_offset].rfind("function") else {
      offset = split_offset + ".split".len();
      continue;
    };
    let Some(open_brace) = script[function_start..].find('{').map(|n| n + function_start) else {
      offset = split_offset + ".split".len();
      continue;
    };
    let Some(close_brace) = matching_brace(script, open_brace) else {
      offset = split_offset + ".split".len();
      continue;
    };
    let candidate = &script[function_start..=close_brace];
    first_candidate.get_or_insert(candidate);
    if ast_looks_like_signature(candidate) {
      return Some(candidate);
    }
    offset = split_offset + ".split".len();
  }
  first_candidate
}

fn ast_looks_like_signature(candidate: &str) -> bool {
  let wrapped = format!("({candidate});");
  let mut parser = Parser::new(ParserSource::from_bytes(wrapped.as_bytes()));
  let mut interner = Interner::default();
  let scope = Scope::new_global();
  match parser.parse_script(&scope, &mut interner) {
    Ok(_) => candidate.contains("split") && candidate.contains("join"),
    Err(_) => false,
  }
}

fn matching_brace(script: &str, open: usize) -> Option<usize> {
  let bytes = script.as_bytes();
  let mut depth = 0_u32;
  let mut quote = None;
  let mut escaped = false;
  for (index, byte) in bytes.iter().enumerate().skip(open) {
    if let Some(current) = quote {
      if escaped {
        escaped = false;
        continue;
      }
      if *byte == b'\\' {
        escaped = true;
        continue;
      }
      if *byte == current {
        quote = None;
      }
      continue;
    }
    match *byte {
      b'\'' | b'"' | b'`' => quote = Some(*byte),
      b'{' => depth += 1,
      b'}' => {
        depth = depth.checked_sub(1)?;
        if depth == 0 {
          return Some(index);
        }
      }
      _ => {}
    }
  }
  None
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

  #[test]
  fn extracts_common_signature_function() {
    let script = r#"var x=function(a){a=a.split("");a.reverse();return a.join("")};"#;
    let function = extract_signature_function(script).unwrap();
    assert_eq!(evaluate_signature(function, "abc").unwrap(), "cba");
  }
}
