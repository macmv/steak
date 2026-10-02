//! Solves YouTube's `sig` and `n` challenges with yt-dlp's EJS solver.
//!
//! The solver is yt-dlp-ejs's JavaScript (vendored in `vendor/ejs`), run in an
//! embedded QuickJS. It parses the player with a real JS parser and finds the
//! relevant functions structurally, so it survives minifier renames. Updating
//! it means replacing the vendored files, not editing extraction code.

use std::{
  collections::HashMap,
  sync::{Arc, Mutex, mpsc},
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
  pub sig:          HashMap<String, String>,
  pub n:            HashMap<String, String>,
  /// The player reduced to just its solver functions. Loading this into a
  /// [`Solver`] skips parsing the full player, which is the slow part.
  pub preprocessed: Option<String>,
}

#[derive(Deserialize)]
struct Output {
  #[serde(rename = "type")]
  kind:                String,
  error:               Option<String>,
  #[serde(default)]
  responses:           Vec<Response>,
  preprocessed_player: Option<String>,
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
  let input = json!({
    "type": "player",
    "player": player_js,
    "output_preprocessed": true,
    "requests": requests,
  });

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
    run().map_err(|e| js_error(&ctx, e))
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
  let mut solved = Solved { preprocessed: output.preprocessed_player, ..Solved::default() };
  if !sigs.is_empty() {
    solved.sig = take(responses.next())?;
  }
  if !ns.is_empty() {
    solved.n = take(responses.next())?;
  }
  Ok(solved)
}

fn js_error(ctx: &rquickjs::Ctx<'_>, error: rquickjs::Error) -> SolverError {
  match error {
    rquickjs::Error::Exception => {
      let thrown: rquickjs::Value = ctx.catch();
      SolverError::Runtime(format!("{thrown:?}"))
    }
    error => SolverError::Runtime(error.to_string()),
  }
}

fn take(response: Option<Response>) -> Result<HashMap<String, String>, SolverError> {
  let response = response.ok_or_else(|| SolverError::BadOutput("missing response".into()))?;
  if response.kind == "error" {
    return Err(SolverError::Solver(response.error.unwrap_or_default()));
  }
  Ok(response.data)
}

/// Runs the same challenges as a request to the EJS solver, but against the
/// functions already defined by a preprocessed player.
const SOLVE: &str = r#"(function (input) {
  const out = {};
  for (const kind of ["sig", "n"]) {
    if (!input[kind].length) continue;
    const solve = steak[kind];
    if (!solve) throw new Error("Failed to extract " + kind + " function");
    out[kind] = Object.fromEntries(input[kind].map((c) => [c, solve(c)]));
  }
  return JSON.stringify(out);
})(JSON.parse(steak_input))"#;

#[derive(Default, Deserialize)]
struct Answers {
  #[serde(default)]
  sig: HashMap<String, String>,
  #[serde(default)]
  n:   HashMap<String, String>,
}

struct Job {
  sigs:  Vec<String>,
  ns:    Vec<String>,
  reply: tokio::sync::oneshot::Sender<Result<Solved, SolverError>>,
}

/// A solver loaded from a preprocessed player (see [`Solved::preprocessed`]).
///
/// QuickJS is not `Send`, so the engine lives on its own thread. Loading starts
/// immediately in [`Solver::spawn`] and overlaps with whatever the caller does
/// next; [`Solver::solve`] then only has to call two functions. Cloning shares
/// the engine, and the thread exits when the last clone is dropped.
#[derive(Clone)]
pub struct Solver {
  jobs: mpsc::Sender<Job>,
}

impl Solver {
  pub fn spawn(preprocessed: Arc<str>) -> Self {
    let (jobs, queue) = mpsc::channel();
    std::thread::spawn(move || serve(&preprocessed, queue));
    Self { jobs }
  }

  pub async fn solve(&self, sigs: Vec<String>, ns: Vec<String>) -> Result<Solved, SolverError> {
    let stopped = || SolverError::Runtime("solver thread stopped".into());
    let (reply, answer) = tokio::sync::oneshot::channel();
    self.jobs.send(Job { sigs, ns, reply }).map_err(|_| stopped())?;
    answer.await.map_err(|_| stopped())?
  }
}

fn serve(preprocessed: &str, queue: mpsc::Receiver<Job>) {
  let deadline = Arc::new(Mutex::new(Instant::now() + DEADLINE));
  let loaded = load(preprocessed, &deadline);
  for job in queue {
    *deadline.lock().expect("deadline lock poisoned") = Instant::now() + DEADLINE;
    let result = match &loaded {
      Ok((_runtime, context)) => run_job(context, &job),
      Err(error) => Err(error.clone()),
    };
    let _ = job.reply.send(result);
  }
}

fn load(
  preprocessed: &str,
  deadline: &Arc<Mutex<Instant>>,
) -> Result<(Runtime, Context), SolverError> {
  let runtime = Runtime::new().map_err(|e| SolverError::Runtime(e.to_string()))?;
  runtime.set_memory_limit(MEMORY_LIMIT);
  let deadline = deadline.clone();
  runtime.set_interrupt_handler(Some(Box::new(move || {
    Instant::now() > *deadline.lock().expect("deadline lock poisoned")
  })));
  let context = Context::full(&runtime).map_err(|e| SolverError::Runtime(e.to_string()))?;
  context.with(|ctx| {
    let run = || -> rquickjs::Result<()> {
      ctx.globals().set("steak_player", preprocessed)?;
      ctx.eval::<(), _>(
        "globalThis.steak = { n: null, sig: null };\
         Function('_result', steak_player)(steak);\
         delete globalThis.steak_player;",
      )
    };
    run().map_err(|e| js_error(&ctx, e))
  })?;
  Ok((runtime, context))
}

fn run_job(context: &Context, job: &Job) -> Result<Solved, SolverError> {
  let input = json!({ "sig": job.sigs, "n": job.ns }).to_string();
  let raw: String = context.with(|ctx| {
    let run = || -> rquickjs::Result<String> {
      ctx.globals().set("steak_input", input)?;
      ctx.eval::<String, _>(SOLVE)
    };
    run().map_err(|e| js_error(&ctx, e))
  })?;
  let answers: Answers =
    serde_json::from_str(&raw).map_err(|e| SolverError::BadOutput(e.to_string()))?;
  Ok(Solved { sig: answers.sig, n: answers.n, preprocessed: None })
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

  #[tokio::test]
  async fn preprocessed_solver_matches_the_raw_solve() {
    let sig = "ABCDEFGHIJKLMNOPQRSTUVWXYZ".repeat(3);
    let raw = solve(PLAYER, &[sig.clone()], &["abcdefghijk".into()]).unwrap();
    let solver = Solver::spawn(raw.preprocessed.clone().unwrap().into());
    for _ in 0..2 {
      let start = Instant::now();
      let again = solver.solve(vec![sig.clone()], vec!["abcdefghijk".into()]).await.unwrap();
      eprintln!("preprocessed solve took {:?}", start.elapsed());
      assert_eq!(again.sig, raw.sig);
      assert_eq!(again.n, raw.n);
    }
  }

  #[tokio::test]
  async fn bad_preprocessed_player_is_an_error() {
    let solver = Solver::spawn("this is not javascript(".into());
    assert!(solver.solve(vec![], vec!["abc".into()]).await.is_err());
  }

  #[test]
  fn nothing_to_solve_is_empty() {
    assert_eq!(solve("", &[], &[]).unwrap(), Solved::default());
  }
}
