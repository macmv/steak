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
use std::{
  collections::HashMap,
  io::Write,
  process::{Command, Stdio},
  sync::{Arc, Mutex},
};

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
  MissingNFunction,
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
      Self::MissingNFunction => {
        f.write_str("could not find an n-parameter function in player JavaScript")
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
  http:     Client,
  players:  Arc<Mutex<HashMap<String, Arc<str>>>>,
  n_values: Arc<Mutex<HashMap<(String, String), String>>>,
}

impl BoaCipherResolver {
  pub fn new() -> Self {
    Self { http: Client::new(), players: Arc::default(), n_values: Arc::default() }
  }

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
    let player_id = player_cache_key(player_script_url);
    let cached = self.players.lock().expect("player cache lock poisoned").get(&player_id).cloned();
    let script = if let Some(script) = cached {
      debug!("using cached player JavaScript for {player_id}");
      script
    } else {
      let response = self
        .http
        .get(player_script_url.clone())
        .send()
        .await
        .map_err(|error| CipherError::JavaScript(error.to_string()))?;
      debug!("player JavaScript response status={}", response.status());
      let script: Arc<str> = response
        .error_for_status()
        .map_err(|error| CipherError::JavaScript(error.to_string()))?
        .text()
        .await
        .map_err(|error| CipherError::JavaScript(error.to_string()))?
        .into();
      self
        .players
        .lock()
        .expect("player cache lock poisoned")
        .insert(player_id.clone(), script.clone());
      script
    };
    debug!("downloaded player JavaScript ({} bytes)", script.len());
    self.resolve_script_cached(&player_id, &script, stream)
  }

  pub fn resolve_script(&self, script: &str, stream: &StreamUrl) -> Result<Url, CipherError> {
    self.resolve_script_cached("inline-player", script, stream)
  }

  fn resolve_script_cached(
    &self,
    player_id: &str,
    script: &str,
    stream: &StreamUrl,
  ) -> Result<Url, CipherError> {
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
        debug!(
          "media URL parameters: signature={}, n={}",
          url.query_pairs().any(|(k, _)| k == "sig" || k == "signature"),
          url.query_pairs().any(|(k, _)| k == "n")
        );
        if let Some(signature) = fields.get("sig").or_else(|| fields.get("signature")) {
          debug!("signature cipher already has a signature; no JavaScript transformation needed");
          let parameter = fields.get("sp").map(String::as_str).unwrap_or("signature");
          url.query_pairs_mut().append_pair(parameter, signature);
        } else {
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
        }
        if url.query_pairs().any(|(key, _)| key == "n") {
          let n = url
            .query_pairs()
            .find(|(key, _)| key == "n")
            .map(|(_, value)| value.into_owned())
            .ok_or(CipherError::MissingNFunction)?;
          let cache_key = (player_id.to_owned(), n.clone());
          let transformed = if let Some(value) =
            self.n_values.lock().expect("n cache lock poisoned").get(&cache_key).cloned()
          {
            value
          } else {
            // This follows yt-dlp's current strategy. The `alr=yes` URL-builder is
            // the stable call site; one of its URL methods applies the n challenge.
            // The Node program retains declarations/assignments only, so loading a
            // player cannot start playback or perform network requests.
            let value = solve_n_with_node(script, &n)?.ok_or(CipherError::MissingNFunction)?;
            self.n_values.lock().expect("n cache lock poisoned").insert(cache_key, value.clone());
            value
          };
          let pairs = url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .map(|(key, value)| if key == "n" { (key, transformed.clone()) } else { (key, value) })
            .collect::<Vec<_>>();
          url.set_query(None);
          for (key, value) in pairs {
            url.query_pairs_mut().append_pair(&key, &value);
          }
          debug!("transformed n parameter ({} -> {} bytes)", n.len(), transformed.len());
        }
        Ok(url)
      }
    }
  }
}

fn player_cache_key(url: &Url) -> String {
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

/// Runs the player-derived n closure in Node. The bundled player is not
/// executed as-is: its outer IIFE is split into top-level statements and only
/// declarations/assignments are retained. This preserves the URL builder and
/// its dependencies while dropping startup side effects.
fn solve_n_with_node(script: &str, n: &str) -> Result<Option<String>, CipherError> {
  const SOLVER: &str = r#"
const fs = require('fs'), vm = require('vm');
const { player, n } = JSON.parse(fs.readFileSync(0, 'utf8'));
function bodyOf(s) { const start=s.indexOf('(function(g)'); const open=start < 0 ? -1 : s.indexOf('{', start); const end=s.lastIndexOf('})(_yt_player);'); return open >= 0 && end > open ? s.slice(open + 1, end) : s; }
function statements(s) { let out=[], start=0, depth=0, quote='', esc=false; for(let i=0;i<s.length;i++){const c=s[i]; if(quote){if(esc)esc=false;else if(c==='\\')esc=true;else if(c===quote)quote='';continue} if(c==='"'||c==="'"||c==='`'){quote=c;continue} if(c==='{')depth++; else if(c==='}')depth--; else if(c===';'&&depth===0){out.push(s.slice(start,i+1));start=i+1}} return out; }
const body=bodyOf(player);
const marker=/([A-Za-z_$][\w$]*)\s*=\s*function\s*\([^)]*\)\s*\{[^{}]*?\.set\(\s*["']alr["']\s*,\s*["']yes["']\s*\)/g;
let names=[]; for(const m of body.matchAll(marker)) names.push(m[1]); if(!names.length) throw new Error('n URL-builder call site not found');
const kept=statements(body).filter(x => /^\s*(?:var|let|const|function|class)\b/.test(x) || /^\s*[A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*)+\s*=/.test(x) || /^\s*[A-Za-z_$][\w$]*\s*=\s*(?:function|class)/.test(x));
const ctx={_yt_player:{}, URL, URLSearchParams, XMLHttpRequest:{prototype:{}}, document:Object.create(null), navigator:Object.create(null), location:new URL('https://www.youtube.com/watch?v=steak'), console:{log(){},warn(){},error(){}}}; ctx.window=ctx;ctx.self=ctx;
vm.runInNewContext('var g=_yt_player;'+kept.join('\n'),ctx,{timeout:5000});
for(const name of names){ const make=ctx[name]; if(typeof make!=='function') continue; const url=make('https://googlevideo.example/videoplayback','s'); if(!url||typeof url.set!=='function'||typeof url.get!=='function') continue; url.set('n',n); const proto=Object.getPrototypeOf(url); for(const key of [...Object.keys(proto),...Object.getOwnPropertyNames(proto)]) if(!['constructor','set','get','clone'].includes(key)&&typeof url[key]==='function'){ url[key](); const value=url.get('n'); if(typeof value==='string'&&value&&value!==n){process.stdout.write(JSON.stringify(value));process.exit(0)} } }
process.stdout.write('null');
"#;
  let mut child = Command::new("node")
    .args(["--input-type=commonjs", "--eval", SOLVER])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .map_err(|error| CipherError::JavaScript(format!("Node runtime unavailable: {error}")))?;
  let input = serde_json::json!({ "player": script, "n": n });
  child
    .stdin
    .take()
    .expect("stdin was piped")
    .write_all(input.to_string().as_bytes())
    .map_err(|error| CipherError::JavaScript(error.to_string()))?;
  let output =
    child.wait_with_output().map_err(|error| CipherError::JavaScript(error.to_string()))?;
  if !output.status.success() {
    return Err(CipherError::JavaScript(String::from_utf8_lossy(&output.stderr).trim().to_owned()));
  }
  serde_json::from_slice(&output.stdout).map_err(|error| CipherError::JavaScript(error.to_string()))
}

/// Extracts the common unary split/transform/join signature function shape.
/// This intentionally fails closed when the player deploys a different shape.
pub fn extract_signature_function(script: &str) -> Option<&str> {
  transform_candidates(script).into_iter().next()
}

fn transform_candidates(script: &str) -> Vec<&str> {
  if !boa_parses_script(script) {
    return Vec::new();
  }
  let mut offset = 0;
  let mut candidates = Vec::new();
  while let Some(split_offset) = script[offset..].find(".split") {
    let split_offset = offset + split_offset;
    let Some(function_start) = script[..split_offset].rfind("function") else {
      offset = split_offset + 6;
      continue;
    };
    let Some(open_brace) = script[function_start..].find('{').map(|n| n + function_start) else {
      offset = split_offset + 6;
      continue;
    };
    let Some(close_brace) = matching_brace(script, open_brace) else {
      offset = split_offset + 6;
      continue;
    };
    let candidate = &script[function_start..=close_brace];
    if candidate.contains(".join")
      && ast_looks_like_signature(candidate)
      && !candidates.iter().any(|existing| *existing == candidate)
    {
      candidates.push(candidate);
    }
    offset = split_offset + 6;
  }
  candidates
}

fn boa_parses_script(script: &str) -> bool {
  let mut parser = Parser::new(ParserSource::from_bytes(script.as_bytes()));
  let mut interner = Interner::default();
  parser.parse_script(&Scope::new_global(), &mut interner).is_ok()
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

  #[test]
  fn solves_n_through_player_url_builder() {
    let script = include_str!("../../tests/fixtures/player_transforms.js");
    let signature = extract_signature_function(script).unwrap();
    assert_eq!(evaluate_signature(signature, "abc").unwrap(), "cba");
    assert_eq!(solve_n_with_node(script, "abc").unwrap().as_deref(), Some("CBA"));
  }

  #[test]
  fn resolves_signature_and_n_from_fixture() {
    let script = include_str!("../../tests/fixtures/player_transforms.js");
    let stream = StreamUrl::SignatureCipher(
      "url=https%3A%2F%2Fexample.test%2Faudio%3Fn=abc&s=xyz&sp=sig".into(),
    );
    let resolved = BoaCipherResolver::new().resolve_script(script, &stream).unwrap();
    assert_eq!(resolved.query_pairs().find(|(k, _)| k == "n").unwrap().1, "CBA");
    assert_eq!(resolved.query_pairs().find(|(k, _)| k == "sig").unwrap().1, "zyx");
  }
}
