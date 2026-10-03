//! A cached value with an explicit lifecycle.
//!
//! Every cache in the daemon is a [`Cell`]. The lifecycle lives in [`Machine`],
//! a plain state machine with no I/O and no clock of its own, so every rule is
//! visible in one place and tested with a fake clock. [`Cell`] is the thin
//! async shell that runs the machine's decisions.
//!
//! ```text
//!   Empty --get/maintain--> Refreshing --fetch ok--> Ready
//!     ^                      |    ^  ^                |
//!     |                      |    |  |                | age >= ttl
//!     |                      |    |  +----------------+ (get/maintain)
//!     |                      |    |                   |
//!     |            fetch     |    | retry_at          |
//!     |            failed    v    | reached           |
//!     |                    Backoff-+                   |
//!     |            (retry_at = now + min_delay * 2^n)  |
//!     |                                                |
//!     +------ invalidate (from any state) <------------+
//! ```
//!
//! Rules, in the order they are checked by [`Machine::plan`]:
//!
//! - `Ready` and younger than `ttl`: serve it.
//! - `Ready` and older than `ttl`: serve it anyway if it is no older than `ttl
//!   + max_stale` (the refresh happens in the background); otherwise wait for a
//!   refresh.
//! - `Refreshing`: serve the previous value if it is still servable, otherwise
//!   wait. There is never more than one refresh in flight.
//! - `Backoff` before `retry_at`: serve the previous value if it is still
//!   servable, otherwise fail immediately with the last error. After
//!   `retry_at`, start a refresh.
//! - `Empty`: start a refresh and wait.
//!
//! Why it cannot get stuck:
//!
//! - A refresh runs in its own task under `refresh_timeout`, and a panic in it
//!   is caught and turned into a failure. `Refreshing` therefore always ends in
//!   `Ready` or `Backoff`.
//! - As a last resort, a `Refreshing` state older than twice the timeout is
//!   declared lost and treated as a failure (see [`Machine::plan`]).
//! - Waiters have their own deadline, and no lock is held across an `.await`.
//! - Every refresh carries an attempt number. A result for an attempt that was
//!   invalidated or declared lost is discarded, so a late result can never
//!   overwrite newer state.

use std::{
  future::Future,
  ops::Deref,
  pin::Pin,
  sync::{Arc, Mutex, MutexGuard},
  time::{Duration, Instant},
};

use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

/// How a cell treats its value over time. Each cell states its own policy next
/// to its definition (see `caches.rs`).
#[derive(Clone, Copy, Debug)]
pub struct Policy {
  pub name:            &'static str,
  /// A value older than this is stale: it is refreshed on next use.
  pub ttl:             Duration,
  /// Start refreshing this long before `ttl` runs out, when the daemon's
  /// background keeper sees it, so users rarely wait on a refresh.
  pub refresh_ahead:   Duration,
  /// How long past `ttl` a value may still be served while a refresh runs or
  /// after a refresh failed. `Duration::ZERO` means never serve stale.
  pub max_stale:       Duration,
  /// A single fetch is abandoned after this long.
  pub refresh_timeout: Duration,
  /// Delay after the first failure, doubling per further failure.
  pub backoff_min:     Duration,
  pub backoff_max:     Duration,
}

impl Policy {
  fn backoff(&self, failures: u32) -> Duration {
    let doubled = self.backoff_min.saturating_mul(1 << failures.saturating_sub(1).min(16));
    doubled.min(self.backoff_max)
  }
}

#[derive(Debug)]
struct Held<T> {
  value:      Arc<T>,
  generation: u64,
  filled:     Instant,
}

impl<T> Clone for Held<T> {
  fn clone(&self) -> Self {
    Self { value: self.value.clone(), generation: self.generation, filled: self.filled }
  }
}

impl<T> Held<T> {
  fn lease(&self) -> Lease<T> { Lease { value: self.value.clone(), generation: self.generation } }
}

#[derive(Debug)]
enum State<T> {
  Empty,
  Ready(Held<T>),
  Refreshing { prior: Option<Held<T>>, attempt: u64, started: Instant },
  Backoff { prior: Option<Held<T>>, error: String, retry_at: Instant },
}

/// What [`Machine::plan`] tells the caller to do.
#[derive(Debug)]
pub enum Plan<T> {
  /// Use this value.
  Serve(Lease<T>),
  /// Use this value, and start refresh `attempt` in the background.
  ServeAndRefresh(Lease<T>, u64),
  /// Start refresh `attempt`, then wait for the state to change.
  Refresh(u64),
  /// A refresh is already running; wait for the state to change.
  Wait,
  /// Nothing servable and not allowed to retry yet.
  Fail(String),
}

/// A value together with the generation that produced it. Report the
/// generation to [`Cell::invalidate`] if the value turns out not to work, so a
/// newer value is never thrown away by mistake.
#[derive(Debug)]
pub struct Lease<T> {
  pub value:      Arc<T>,
  pub generation: u64,
}

impl<T> Clone for Lease<T> {
  fn clone(&self) -> Self { Self { value: self.value.clone(), generation: self.generation } }
}

impl<T> Deref for Lease<T> {
  type Target = T;

  fn deref(&self) -> &T { &self.value }
}

/// The lifecycle itself. See the module docs for the rules.
#[derive(Debug)]
pub struct Machine<T> {
  policy:     Policy,
  state:      State<T>,
  /// Generation of the most recent successful fill.
  generation: u64,
  /// Number of the most recently started refresh.
  attempts:   u64,
  /// Consecutive failed refreshes; reset by a success.
  failures:   u32,
}

impl<T> Machine<T> {
  pub fn new(policy: Policy) -> Self {
    Self { policy, state: State::Empty, generation: 0, attempts: 0, failures: 0 }
  }

  pub fn label(&self) -> &'static str {
    match self.state {
      State::Empty => "empty",
      State::Ready(_) => "ready",
      State::Refreshing { .. } => "refreshing",
      State::Backoff { .. } => "backoff",
    }
  }

  fn age(held: &Held<T>, now: Instant) -> Duration { now.saturating_duration_since(held.filled) }

  fn is_fresh(&self, held: &Held<T>, now: Instant) -> bool {
    Self::age(held, now) < self.policy.ttl
  }

  fn is_servable(&self, held: &Held<T>, now: Instant) -> bool {
    Self::age(held, now) < self.policy.ttl + self.policy.max_stale
  }

  /// Keeps `held` as a fallback only while it may still be served.
  fn keep(&self, held: Option<Held<T>>, now: Instant) -> Option<Held<T>> {
    held.filter(|held| self.is_servable(held, now))
  }

  fn start_refresh(&mut self, prior: Option<Held<T>>, now: Instant) -> u64 {
    self.attempts += 1;
    let prior = self.keep(prior, now);
    self.state = State::Refreshing { prior, attempt: self.attempts, started: now };
    self.attempts
  }

  /// A refresh that outlived its own timeout twice over is lost (its task
  /// died without reporting). Fail it so the state cannot stay `Refreshing`.
  fn expire_lost(&mut self, now: Instant) {
    if let State::Refreshing { attempt, started, .. } = &self.state {
      if now.saturating_duration_since(*started) > self.policy.refresh_timeout * 2 {
        let attempt = *attempt;
        self.finish(attempt, Err("the refresh task was lost".into()), now);
      }
    }
  }

  /// Decides what a caller that wants the value should do right now.
  pub fn plan(&mut self, now: Instant) -> Plan<T> {
    self.expire_lost(now);

    match std::mem::replace(&mut self.state, State::Empty) {
      State::Empty => Plan::Refresh(self.start_refresh(None, now)),
      State::Ready(held) => {
        if self.is_fresh(&held, now) {
          let lease = held.lease();
          self.state = State::Ready(held);
          Plan::Serve(lease)
        } else if self.is_servable(&held, now) {
          let lease = held.lease();
          Plan::ServeAndRefresh(lease, self.start_refresh(Some(held), now))
        } else {
          Plan::Refresh(self.start_refresh(None, now))
        }
      }
      State::Refreshing { prior, attempt, started } => {
        let servable = prior.clone().filter(|held| self.is_servable(held, now));
        self.state = State::Refreshing { prior, attempt, started };
        match servable {
          Some(held) => Plan::Serve(held.lease()),
          None => Plan::Wait,
        }
      }
      State::Backoff { prior, error, retry_at } => {
        if now >= retry_at {
          Plan::Refresh(self.start_refresh(prior, now))
        } else {
          let servable = prior.clone().filter(|held| self.is_servable(held, now));
          self.state = State::Backoff { prior, error: error.clone(), retry_at };
          match servable {
            Some(held) => Plan::Serve(held.lease()),
            None => {
              let wait = retry_at.saturating_duration_since(now).as_secs();
              Plan::Fail(format!("{error} (retrying in {wait}s)"))
            }
          }
        }
      }
    }
  }

  /// For the background keeper: returns the attempt to start if the cell needs
  /// a refresh now. Never serves or waits.
  pub fn plan_background(&mut self, now: Instant) -> Option<u64> {
    self.expire_lost(now);
    let due = match &self.state {
      State::Empty => true,
      State::Ready(held) => Self::age(held, now) + self.policy.refresh_ahead >= self.policy.ttl,
      State::Backoff { retry_at, .. } => now >= *retry_at,
      State::Refreshing { .. } => false,
    };
    if !due {
      return None;
    }
    let prior = match std::mem::replace(&mut self.state, State::Empty) {
      State::Ready(held) => Some(held),
      State::Backoff { prior, .. } => prior,
      _ => None,
    };
    Some(self.start_refresh(prior, now))
  }

  /// Records the outcome of refresh `attempt`. Returns whether it was applied;
  /// a result for an attempt that is no longer current is dropped.
  pub fn finish(&mut self, attempt: u64, result: Result<T, String>, now: Instant) -> bool {
    let State::Refreshing { prior, attempt: current, .. } = &mut self.state else { return false };
    if *current != attempt {
      return false;
    }
    let prior = prior.take();
    match result {
      Ok(value) => {
        self.generation += 1;
        self.failures = 0;
        self.state = State::Ready(Held {
          value:      Arc::new(value),
          generation: self.generation,
          filled:     now,
        });
      }
      Err(error) => {
        self.failures += 1;
        let retry_at = now + self.policy.backoff(self.failures);
        let prior = self.keep(prior, now);
        self.state = State::Backoff { prior, error, retry_at };
      }
    }
    true
  }

  /// Throws away the value of `generation` (or whatever is held, for `None`),
  /// so the next use rebuilds it. A refresh that is in flight is abandoned: its
  /// inputs may be the very thing that went bad.
  pub fn invalidate(&mut self, generation: Option<u64>) -> bool {
    let held = match &self.state {
      State::Empty => return false,
      State::Ready(held) => Some(held.generation),
      State::Refreshing { prior, .. } | State::Backoff { prior, .. } => {
        prior.as_ref().map(|held| held.generation)
      }
    };
    // A generation can only be invalidated while it is still the held one.
    if generation.is_some() && held != generation {
      return false;
    }
    self.attempts += 1; // Orphans any in-flight refresh.
    self.failures = 0;
    self.state = State::Empty;
    true
  }

  /// The current generation, if a value is held. Used to bind one cell to
  /// another.
  pub fn held(&self) -> Option<Lease<T>> {
    match &self.state {
      State::Ready(held) => Some(held.lease()),
      State::Refreshing { prior, .. } | State::Backoff { prior, .. } => {
        prior.as_ref().map(Held::lease)
      }
      State::Empty => None,
    }
  }

  pub fn report(&self, now: Instant) -> CellReport {
    let (age, error, retry_in) = match &self.state {
      State::Ready(held) => (Some(Self::age(held, now)), None, None),
      State::Refreshing { prior, .. } => (prior.as_ref().map(|h| Self::age(h, now)), None, None),
      State::Backoff { prior, error, retry_at } => (
        prior.as_ref().map(|h| Self::age(h, now)),
        Some(error.clone()),
        Some(retry_at.saturating_duration_since(now)),
      ),
      State::Empty => (None, None, None),
    };
    CellReport {
      name:                 self.policy.name.to_owned(),
      state:                self.label().to_owned(),
      generation:           self.held().map(|lease| lease.generation),
      age_secs:             age.map(|age| age.as_secs()),
      consecutive_failures: self.failures,
      last_error:           error,
      retry_in_secs:        retry_in.map(|retry| retry.as_secs()),
    }
  }
}

/// A snapshot of one cell for `steak status`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CellReport {
  pub name:                 String,
  pub state:                String,
  pub generation:           Option<u64>,
  pub age_secs:             Option<u64>,
  pub consecutive_failures: u32,
  pub last_error:           Option<String>,
  pub retry_in_secs:        Option<u64>,
}

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
type Fetch<T> = Arc<dyn Fn() -> BoxFuture<Result<T, String>> + Send + Sync>;

/// Locks a mutex, ignoring poisoning. The state behind every lock in the
/// daemon is a plain value that is valid after any panic, and a poisoned lock
/// must never turn one bug into a daemon that refuses every request.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
  mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The async shell around a [`Machine`]: it runs fetches and wakes waiters.
pub struct Cell<T> {
  policy:  Policy,
  machine: Mutex<Machine<T>>,
  changed: watch::Sender<u64>,
  fetch:   Fetch<T>,
}

#[derive(Clone, Debug)]
pub struct CellError(pub String);

impl std::fmt::Display for CellError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
}
impl std::error::Error for CellError {}

impl<T: Send + Sync + 'static> Cell<T> {
  pub fn new<F, Fut>(policy: Policy, fetch: F) -> Arc<Self>
  where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<T, String>> + Send + 'static,
  {
    Arc::new(Self {
      policy,
      machine: Mutex::new(Machine::new(policy)),
      changed: watch::channel(0).0,
      fetch: Arc::new(move || Box::pin(fetch())),
    })
  }

  /// Runs `f` on the machine, logs any state change, and wakes waiters.
  fn step<R>(&self, f: impl FnOnce(&mut Machine<T>) -> R) -> R {
    let (result, before, after) = {
      let mut machine = lock(&self.machine);
      let before = machine.label();
      let result = f(&mut machine);
      (result, before, machine.label())
    };
    if before != after {
      debug!("cache {}: {before} -> {after}", self.policy.name);
      self.changed.send_modify(|version| *version += 1);
    }
    result
  }

  /// The value, refreshing it first if the lifecycle says so.
  ///
  /// Waits at most `refresh_timeout` plus a little, however many refreshes are
  /// in flight or fail meanwhile.
  pub async fn get(self: &Arc<Self>) -> Result<Lease<T>, CellError> {
    let deadline = tokio::time::Instant::now() + self.policy.refresh_timeout * 2 + WAIT_SLACK;
    loop {
      // Subscribe before planning so a change between the two is not missed.
      let mut changes = self.changed.subscribe();
      match self.step(|machine| machine.plan(Instant::now())) {
        Plan::Serve(lease) => return Ok(lease),
        Plan::ServeAndRefresh(lease, attempt) => {
          self.spawn_refresh(attempt);
          return Ok(lease);
        }
        Plan::Refresh(attempt) => self.spawn_refresh(attempt),
        Plan::Wait => {}
        Plan::Fail(error) => return Err(CellError(format!("{}: {error}", self.policy.name))),
      }
      match tokio::time::timeout_at(deadline, changes.changed()).await {
        Ok(_) => {}
        Err(_) => {
          return Err(CellError(format!("{}: timed out waiting for a refresh", self.policy.name)));
        }
      }
    }
  }

  /// Starts a refresh now if the lifecycle wants one, without waiting for it.
  pub fn maintain(self: &Arc<Self>) {
    if let Some(attempt) = self.step(|machine| machine.plan_background(Instant::now())) {
      self.spawn_refresh(attempt);
    }
  }

  /// Reports that the value of `generation` did not work. See
  /// [`Machine::invalidate`].
  pub fn invalidate(&self, generation: u64) {
    if self.step(|machine| machine.invalidate(Some(generation))) {
      info!("cache {}: generation {generation} invalidated by a consumer", self.policy.name);
    }
  }

  /// The value that is held right now, stale or not, without refreshing it.
  pub fn peek(&self) -> Option<Lease<T>> { lock(&self.machine).held() }

  pub fn report(&self) -> CellReport { lock(&self.machine).report(Instant::now()) }

  fn spawn_refresh(self: &Arc<Self>, attempt: u64) {
    let cell = self.clone();
    tokio::spawn(async move {
      let fetch = cell.fetch.clone();
      let timeout = cell.policy.refresh_timeout;
      // The fetch gets a task of its own, so a panic surfaces as a `JoinError`
      // here instead of leaving the cell in `Refreshing` forever.
      let work = tokio::spawn(async move { tokio::time::timeout(timeout, fetch()).await });
      let result = match work.await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(format!("timed out after {}s", timeout.as_secs())),
        Err(error) => Err(format!("refresh panicked: {error}")),
      };
      let name = cell.policy.name;
      match &result {
        Ok(_) => debug!("cache {name}: refresh {attempt} succeeded"),
        Err(error) => warn!("cache {name}: refresh {attempt} failed: {error}"),
      }
      cell.step(|machine| {
        if !machine.finish(attempt, result, Instant::now()) {
          debug!("cache {name}: dropped the result of abandoned refresh {attempt}");
        }
      });
    });
  }
}

const WAIT_SLACK: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests {
  use super::*;

  const POLICY: Policy = Policy {
    name:            "test",
    ttl:             Duration::from_secs(100),
    refresh_ahead:   Duration::from_secs(10),
    max_stale:       Duration::from_secs(50),
    refresh_timeout: Duration::from_secs(5),
    backoff_min:     Duration::from_secs(2),
    backoff_max:     Duration::from_secs(10),
  };

  fn secs(now: Instant, secs: u64) -> Instant { now + Duration::from_secs(secs) }

  fn ready(now: Instant) -> Machine<&'static str> {
    let mut machine = Machine::new(POLICY);
    assert!(matches!(machine.plan(now), Plan::Refresh(1)));
    assert!(machine.finish(1, Ok("v1"), now));
    machine
  }

  #[test]
  fn empty_refreshes_then_serves() {
    let now = Instant::now();
    let mut machine = Machine::new(POLICY);
    assert!(matches!(machine.plan(now), Plan::Refresh(1)));
    // A second caller waits for the refresh that is already running.
    assert!(matches!(machine.plan(now), Plan::Wait));
    assert!(machine.finish(1, Ok("v1"), now));
    assert!(matches!(machine.plan(secs(now, 99)), Plan::Serve(lease) if *lease.value == "v1"));
  }

  #[test]
  fn stale_value_is_served_while_it_refreshes() {
    let now = Instant::now();
    let mut machine = ready(now);
    let Plan::ServeAndRefresh(lease, attempt) = machine.plan(secs(now, 120)) else { panic!() };
    assert_eq!(*lease.value, "v1");
    // Everyone else keeps getting the stale value, with no second refresh.
    assert!(matches!(machine.plan(secs(now, 121)), Plan::Serve(_)));
    assert!(machine.finish(attempt, Ok("v2"), secs(now, 122)));
    let Plan::Serve(lease) = machine.plan(secs(now, 123)) else { panic!() };
    assert_eq!((*lease.value, lease.generation), ("v2", 2));
  }

  #[test]
  fn value_past_max_stale_must_be_refreshed_before_use() {
    let now = Instant::now();
    let mut machine = ready(now);
    assert!(matches!(machine.plan(secs(now, 151)), Plan::Refresh(2)));
    assert!(matches!(machine.plan(secs(now, 152)), Plan::Wait));
  }

  #[test]
  fn failure_backs_off_exponentially_and_retries_after_the_delay() {
    let now = Instant::now();
    let mut machine = Machine::<&str>::new(POLICY);
    assert!(matches!(machine.plan(now), Plan::Refresh(1)));
    machine.finish(1, Err("down".into()), now);
    assert!(matches!(machine.plan(secs(now, 1)), Plan::Fail(error) if error.contains("down")));
    assert!(matches!(machine.plan(secs(now, 2)), Plan::Refresh(2)));
    machine.finish(2, Err("down".into()), secs(now, 2));
    // Second failure: 4s.
    assert!(matches!(machine.plan(secs(now, 5)), Plan::Fail(_)));
    assert!(matches!(machine.plan(secs(now, 6)), Plan::Refresh(3)));
    machine.finish(3, Ok("up"), secs(now, 6));
    assert_eq!(machine.report(secs(now, 6)).consecutive_failures, 0);
  }

  #[test]
  fn backoff_is_capped() {
    assert_eq!(POLICY.backoff(1), Duration::from_secs(2));
    assert_eq!(POLICY.backoff(3), Duration::from_secs(8));
    assert_eq!(POLICY.backoff(4), Duration::from_secs(10));
    assert_eq!(POLICY.backoff(500), Duration::from_secs(10));
  }

  #[test]
  fn failed_refresh_falls_back_to_the_stale_value() {
    let now = Instant::now();
    let mut machine = ready(now);
    let Plan::ServeAndRefresh(_, attempt) = machine.plan(secs(now, 110)) else { panic!() };
    machine.finish(attempt, Err("down".into()), secs(now, 111));
    assert!(matches!(machine.plan(secs(now, 112)), Plan::Serve(lease) if *lease.value == "v1"));
    // Once the stale value is too old as well, the error surfaces.
    assert!(matches!(machine.plan(secs(now, 160)), Plan::Refresh(_)));
  }

  #[test]
  fn invalidate_only_hits_the_generation_it_was_given() {
    let now = Instant::now();
    let mut machine = ready(now);
    assert!(!machine.invalidate(Some(7)));
    assert_eq!(machine.label(), "ready");
    assert!(machine.invalidate(Some(1)));
    assert_eq!(machine.label(), "empty");
  }

  #[test]
  fn result_of_an_invalidated_refresh_is_dropped() {
    let now = Instant::now();
    let mut machine = Machine::<&str>::new(POLICY);
    assert!(matches!(machine.plan(now), Plan::Refresh(1)));
    assert!(machine.invalidate(None));
    assert!(!machine.finish(1, Ok("late"), now));
    assert_eq!(machine.label(), "empty");
    // The next refresh has a fresh attempt number, so it is not confused with
    // the abandoned one.
    assert!(matches!(machine.plan(now), Plan::Refresh(attempt) if attempt > 1));
  }

  #[test]
  fn a_lost_refresh_is_failed_instead_of_waited_on_forever() {
    let now = Instant::now();
    let mut machine = Machine::<&str>::new(POLICY);
    assert!(matches!(machine.plan(now), Plan::Refresh(1)));
    assert!(matches!(machine.plan(secs(now, 9)), Plan::Wait));
    // Past twice the timeout: lost, so it fails and enters backoff...
    assert!(matches!(machine.plan(secs(now, 11)), Plan::Fail(error) if error.contains("lost")));
    // ... and later retries, with the late result of attempt 1 ignored.
    assert!(matches!(machine.plan(secs(now, 13)), Plan::Refresh(2)));
    assert!(!machine.finish(1, Ok("late"), secs(now, 14)));
  }

  #[test]
  fn background_refreshes_ahead_of_expiry_only_once() {
    let now = Instant::now();
    let mut machine = ready(now);
    assert_eq!(machine.plan_background(secs(now, 80)), None);
    assert_eq!(machine.plan_background(secs(now, 91)), Some(2));
    assert_eq!(machine.plan_background(secs(now, 92)), None);
    // The old value is still served while the early refresh runs.
    assert!(matches!(machine.plan(secs(now, 93)), Plan::Serve(_)));
  }

  #[tokio::test]
  async fn concurrent_gets_share_one_fetch() {
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counter = calls.clone();
    let cell = Cell::new(POLICY, move || {
      let counter = counter.clone();
      async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
      }
    });
    let results = futures_util::future::join_all((0..8).map(|_| cell.get())).await;
    assert!(results.iter().all(|result| *result.as_ref().unwrap().value == 0));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
  }

  #[tokio::test]
  async fn a_panicking_fetch_becomes_a_failure_not_a_hang() {
    let cell = Cell::<u32>::new(POLICY, || async { panic!("boom") });
    let error = cell.get().await.unwrap_err();
    assert!(error.0.contains("panicked"), "{error}");
    assert_eq!(cell.report().state, "backoff");
  }
}
