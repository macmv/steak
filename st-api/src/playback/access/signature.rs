//! Resolution of YouTube's encrypted `s` signature parameter.
//!
//! Requirements:
//! - the current player JavaScript;
//! - the encrypted `s` value and its target query parameter (`sp`);
//! - a JavaScript evaluator.
//!
//! The player contains the transform. Extract the transform and evaluate it
//! against `s`, then append its output using `sp` (normally `signature`).

use boa_ast::scope::Scope;
use boa_engine::{Context, Source};
use boa_interner::Interner;
use boa_parser::{Parser, Source as ParserSource};
use serde_json::to_string;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SignatureError {
  MissingFunction,
  JavaScript(String),
}

impl std::fmt::Display for SignatureError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::MissingFunction => {
        f.write_str("could not find a signature function in player JavaScript")
      }
      Self::JavaScript(error) => write!(f, "signature JavaScript failed: {error}"),
    }
  }
}
impl std::error::Error for SignatureError {}

pub trait SignatureResolver {
  fn resolve_signature(
    &self,
    player_script: &str,
    encrypted: &str,
  ) -> Result<String, SignatureError>;
}

/// The existing supported signature path: extract a simple transform and run
/// it with Boa. Replace this implementation if the player shape changes.
#[derive(Clone, Copy, Debug, Default)]
pub struct BoaSignatureResolver;

impl SignatureResolver for BoaSignatureResolver {
  fn resolve_signature(
    &self,
    player_script: &str,
    encrypted: &str,
  ) -> Result<String, SignatureError> {
    let function =
      extract_signature_function(player_script).ok_or(SignatureError::MissingFunction)?;
    evaluate_signature(function, encrypted)
  }
}

pub fn evaluate_signature(
  function_expression: &str,
  input: &str,
) -> Result<String, SignatureError> {
  let input = to_string(input).map_err(|error| SignatureError::JavaScript(error.to_string()))?;
  let source = format!("({function_expression})({input})");
  let mut context = Context::default();
  let value = context
    .eval(Source::from_bytes(source.as_bytes()))
    .map_err(|error| SignatureError::JavaScript(error.to_string()))?;
  value
    .to_string(&mut context)
    .map(|value| value.to_std_string_escaped())
    .map_err(|error| SignatureError::JavaScript(error.to_string()))
}

pub fn extract_signature_function(script: &str) -> Option<&str> {
  if !boa_parses_script(script) {
    return None;
  }
  let mut offset = 0;
  while let Some(found) = script[offset..].find(".split") {
    let split = offset + found;
    let start = script[..split].rfind("function")?;
    let open = script[start..].find('{')? + start;
    let close = matching_brace(script, open)?;
    let candidate = &script[start..=close];
    if candidate.contains(".join") && boa_parses_script(&format!("({candidate});")) {
      return Some(candidate);
    }
    offset = split + ".split".len();
  }
  None
}

fn boa_parses_script(script: &str) -> bool {
  let mut parser = Parser::new(ParserSource::from_bytes(script.as_bytes()));
  let mut interner = Interner::default();
  parser.parse_script(&Scope::new_global(), &mut interner).is_ok()
}

fn matching_brace(script: &str, open: usize) -> Option<usize> {
  let mut depth = 0_u32;
  let mut quote = None;
  let mut escaped = false;
  for (index, byte) in script.as_bytes().iter().enumerate().skip(open) {
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

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn boa_resolves_a_common_signature_transform() {
    let player = r#"var f=function(value){return value.split('').reverse().join('')};"#;
    assert_eq!(BoaSignatureResolver.resolve_signature(player, "abc").unwrap(), "cba");
  }
}
