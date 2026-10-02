//! Solves YouTube's `sig` and `n` challenges with yt-dlp's EJS solver.
//!
//! The solver is yt-dlp-ejs's JavaScript (vendored in `vendor/ejs`), run in an
//! embedded QuickJS. It parses the player with a real JS parser and finds the
//! relevant functions structurally, so it survives minifier renames. Updating
//! it means replacing the vendored files, not editing extraction code.

use std::{
  collections::HashMap,
  time::{Duration, Instant},
};

use rquickjs::{Context, Runtime};
use serde::Deserialize;
use serde_json::json;

const LIB: &str = include_str!("../../vendor/ejs/lib.min.js");
const CORE: &str = include_str!("../../vendor/ejs/core.min.js");

const MEMORY_LIMIT: usize = 512 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SolverError {
  /// QuickJS failed to start, or the script threw.
  Runtime(String),
  /// The solver ran but reported an error (usually: the player changed shape).
  Solver(String),
  /// The solver returned something this code does not understand.
  BadOutput(String),
}

impl std::fmt::Display for SolverError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Runtime(e) => write!(f, "JS runtime error: {e}"),
      Self::Solver(e) => write!(f, "challenge solver error: {e}"),
      Self::BadOutput(e) => write!(f, "unexpected challenge solver output: {e}"),
    }
  }
}
impl std::error::Error for SolverError {}

/// Answers keyed by the challenge string that was passed in.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Solved {
  pub sig: HashMap<String, String>,
  pub n:   HashMap<String, String>,
}

#[derive(Deserialize)]
struct Output {
  #[serde(rename = "type")]
  kind:      String,
  error:     Option<String>,
  #[serde(default)]
  responses: Vec<Response>,
}

#[derive(Deserialize)]
struct Response {
  #[serde(rename = "type")]
  kind:  String,
  error: Option<String>,
  #[serde(default)]
  data:  HashMap<String, String>,
}

/// Solves every `s` in `sigs` and every `n` in `ns` against one player script.
///
/// This is blocking and CPU-heavy (it parses the whole player); call it from
/// `spawn_blocking` in async code.
pub fn solve(player_js: &str, sigs: &[String], ns: &[String]) -> Result<Solved, SolverError> {
  let mut requests = Vec::new();
  if !sigs.is_empty() {
    requests.push(json!({ "type": "sig", "challenges": sigs }));
  }
  if !ns.is_empty() {
    requests.push(json!({ "type": "n", "challenges": ns }));
  }
  if requests.is_empty() {
    return Ok(Solved::default());
  }
  let input = json!({ "type": "player", "player": player_js, "requests": requests });

  let runtime = Runtime::new().map_err(|e| SolverError::Runtime(e.to_string()))?;
  runtime.set_memory_limit(MEMORY_LIMIT);
  let start = Instant::now();
  runtime.set_interrupt_handler(Some(Box::new(move || start.elapsed() > DEADLINE)));
  let context = Context::full(&runtime).map_err(|e| SolverError::Runtime(e.to_string()))?;

  let raw: String = context.with(|ctx| {
    let run = || -> rquickjs::Result<String> {
      ctx.globals().set("steak_input", input.to_string())?;
      ctx.eval::<(), _>(LIB)?;
      ctx.eval::<(), _>("Object.assign(globalThis, lib);")?;
      ctx.eval::<(), _>(CORE)?;
      ctx.eval::<String, _>("JSON.stringify(jsc(JSON.parse(steak_input)))")
    };
    run().map_err(|e| match e {
      rquickjs::Error::Exception => {
        let thrown: rquickjs::Value = ctx.catch();
        SolverError::Runtime(format!("{thrown:?}"))
      }
      e => SolverError::Runtime(e.to_string()),
    })
  })?;

  let output: Output =
    serde_json::from_str(&raw).map_err(|e| SolverError::BadOutput(e.to_string()))?;
  if output.kind == "error" {
    return Err(SolverError::Solver(output.error.unwrap_or_default()));
  }
  if output.responses.len() != (!sigs.is_empty() as usize + !ns.is_empty() as usize) {
    return Err(SolverError::BadOutput(format!("{} responses", output.responses.len())));
  }
  let mut responses = output.responses.into_iter();
  let mut solved = Solved::default();
  if !sigs.is_empty() {
    solved.sig = take(responses.next())?;
  }
  if !ns.is_empty() {
    solved.n = take(responses.next())?;
  }
  Ok(solved)
}

fn take(response: Option<Response>) -> Result<HashMap<String, String>, SolverError> {
  let response = response.ok_or_else(|| SolverError::BadOutput("missing response".into()))?;
  if response.kind == "error" {
    return Err(SolverError::Solver(response.error.unwrap_or_default()));
  }
  Ok(response.data)
}

#[cfg(test)]
mod tests {
  use super::*;

  const PLAYER: &str = include_str!("../../tests/fixtures/players/8ab5c328.js");

  /// Expected values come from running the upstream yt-dlp-ejs bundles in deno
  /// (the runtime yt-dlp uses) on the same player and inputs.
  #[test]
  fn matches_deno_on_real_player() {
    let sig = "ABCDEFGHIJKLMNOPQRSTUVWXYZ".repeat(3);
    let start = Instant::now();
    let solved = solve(PLAYER, &[sig.clone()], &["abcdefghijk".into()]).unwrap();
    eprintln!("solve took {:?}", start.elapsed());
    assert_eq!(
      solved.sig[&sig],
      "YXWVUTSRQPONMLKGIHGFEDCBAZYXWVUTSRQPONMLKJIHGFEDCBAZYXWVUTSRQZONMLKJIHAFEDCB"
    );
    assert_eq!(solved.n["abcdefghijk"], "VXuZqhSdiY");
  }

  #[test]
  fn nothing_to_solve_is_empty() {
    assert_eq!(solve("", &[], &[]).unwrap(), Solved::default());
  }
}
