//! The YouTube session: credentials imported from Firefox, and whether YouTube
//! still accepts them.
//!
//! [`Credentials`] is the state machine. [`Machine`] holds the pure transition
//! rules; [`Session`] is the async driver that does the I/O (reading Firefox,
//! asking YouTube) and feeds the results back in.
//!
//! ```text
//!   Absent --import + verify--> Unverified --verified signed in--> Valid
//!     ^                              |                               |
//!     |                              | signed out /                  | re-verify says signed out,
//!     |                              | rejected                      | or a request was rejected
//!     |                              v                               v
//!     |                           Invalid <--------------------------+
//!     |                              |
//!     |                              | repair: import + verify ok (new cookies)
//!     |                              v
//!     |                       Unverified / Valid
//!     |
//!     +-- (Absent is only left, never re-entered; a failed import
//!          with a working session keeps that session)
//! ```
//!
//! Rules:
//!
//! - **Usable** means `Unverified` or `Valid`. Both are handed to requests; an
//!   unverified session is given the benefit of the doubt because a network
//!   error says nothing about the cookies.
//! - **Import**: on startup, whenever Firefox's cookie files change (polled
//!   every [`SOURCE_POLL`]), and whenever the session is not usable. New
//!   cookies are verified with YouTube *before* replacing a working session, so
//!   a half-logged-out Firefox can never downgrade a good session.
//! - **Verify**: every new candidate, and the current session every
//!   [`VERIFY_INTERVAL`]. A request that is served a signed-out response
//!   reports it with [`Session::report_rejected`].
//! - **Repair** is import + verify. When the session is not usable the
//!   background keeper repairs it with exponential backoff (one minute up to
//!   [`AUTO_RETRY_MAX`]). On top of that, *every command that needs a session
//!   tries one repair itself* before failing with an explanation, so fixing
//!   Firefox's login and rerunning the command never requires waiting out a
//!   backoff.
//! - A network failure while verifying is [`Verdict::Unknown`]: it changes
//!   nothing. Only YouTube's answer can invalidate a session.

use std::{
  path::PathBuf,
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
  },
  time::{Duration, Instant, SystemTime},
};

use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use st_api::account::{
  firefox,
  session::{self, StoredSession},
};

use super::{
  cell::{BoxFuture, lock},
  config::Config,
};

/// How often the keeper looks at Firefox's cookie files for changes.
const SOURCE_POLL: Duration = Duration::from_secs(60);
/// How long a session is trusted before YouTube is asked again.
const VERIFY_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Minimum gap between background attempts to verify the current session.
const VERIFY_RETRY: Duration = Duration::from_secs(60);
const AUTO_RETRY_MIN: Duration = Duration::from_secs(60);
const AUTO_RETRY_MAX: Duration = Duration::from_secs(30 * 60);
const IMPORT_TIMEOUT: Duration = Duration::from_secs(30);
const VERIFY_TIMEOUT: Duration = Duration::from_secs(45);
const REPAIR_TIMEOUT: Duration = Duration::from_secs(120);

/// What YouTube said about a session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
  SignedIn,
  SignedOut,
  /// Could not find out (network error and the like).
  Unknown,
}

#[derive(Clone, Debug)]
pub enum Credentials {
  /// No credentials have ever been loaded or imported.
  Absent { reason: String },
  /// Credentials that have not been checked against YouTube yet.
  Unverified { session: Arc<StoredSession>, generation: u64 },
  /// YouTube served a signed-in response for these credentials.
  Valid { session: Arc<StoredSession>, generation: u64, verified: Instant },
  /// YouTube refuses these credentials, or none could be imported.
  Invalid { session: Option<Arc<StoredSession>>, reason: String, since: Instant },
}

impl Credentials {
  fn label(&self) -> &'static str {
    match self {
      Self::Absent { .. } => "absent",
      Self::Unverified { .. } => "unverified",
      Self::Valid { .. } => "valid",
      Self::Invalid { .. } => "invalid",
    }
  }

  fn session(&self) -> Option<&Arc<StoredSession>> {
    match self {
      Self::Unverified { session, .. } | Self::Valid { session, .. } => Some(session),
      Self::Invalid { session, .. } => session.as_ref(),
      Self::Absent { .. } => None,
    }
  }
}

/// Whether a freshly imported session needs YouTube's opinion.
#[derive(Debug, Eq, PartialEq)]
pub enum CandidatePlan {
  Verify,
  /// Nothing to learn: these are the cookies we already have, and we already
  /// know what YouTube thinks of them.
  Skip,
}

/// The transition rules. No I/O, no clock: every method takes `now`.
#[derive(Debug)]
pub struct Machine {
  creds:       Credentials,
  generation:  u64,
  /// Consecutive failed repairs, for the background backoff.
  failures:    u32,
  next_auto:   Instant,
  /// The last import problem or noteworthy event, for `steak status`.
  note:        Option<String>,
  last_import: Option<Instant>,
}

impl Machine {
  pub fn new(now: Instant) -> Self {
    Self {
      creds:       Credentials::Absent { reason: "no session has been imported yet".into() },
      generation:  0,
      failures:    0,
      next_auto:   now,
      note:        None,
      last_import: None,
    }
  }

  pub fn creds(&self) -> &Credentials { &self.creds }

  /// The credentials requests may use, with their generation.
  pub fn usable(&self) -> Option<(Arc<StoredSession>, u64)> {
    match &self.creds {
      Credentials::Unverified { session, generation }
      | Credentials::Valid { session, generation, .. } => Some((session.clone(), *generation)),
      _ => None,
    }
  }

  /// Credentials found on disk at startup.
  pub fn loaded(&mut self, session: StoredSession) {
    if matches!(self.creds, Credentials::Absent { .. }) {
      self.generation += 1;
      self.creds =
        Credentials::Unverified { session: Arc::new(session), generation: self.generation };
    }
  }

  pub fn plan_candidate(&self, candidate: &StoredSession) -> CandidatePlan {
    let same =
      self.creds.session().is_some_and(|current| current.cookie_header == candidate.cookie_header);
    match (&self.creds, same) {
      (Credentials::Valid { .. } | Credentials::Invalid { .. }, true) => CandidatePlan::Skip,
      _ => CandidatePlan::Verify,
    }
  }

  /// Applies YouTube's verdict on `candidate` (cookies from Firefox, or the
  /// current ones being re-checked). Returns whether the candidate became the
  /// current session, i.e. whether it should be saved.
  pub fn candidate(&mut self, candidate: StoredSession, verdict: Verdict, now: Instant) -> bool {
    self.last_import = Some(now);
    let same =
      self.creds.session().is_some_and(|current| current.cookie_header == candidate.cookie_header);
    let usable = self.usable().is_some();
    match verdict {
      Verdict::SignedIn => {
        self.generation += 1;
        self.creds = Credentials::Valid {
          session:    Arc::new(candidate),
          generation: self.generation,
          verified:   now,
        };
        self.note = None;
        true
      }
      Verdict::SignedOut if same => {
        self.invalidate("YouTube treats this session as signed out".into(), now);
        false
      }
      Verdict::SignedOut if usable => {
        self.note = Some(
          "Firefox has different cookies, but YouTube treats them as signed out; keeping the \
           current session"
            .into(),
        );
        false
      }
      Verdict::SignedOut => {
        self.creds = Credentials::Invalid {
          session: Some(Arc::new(candidate)),
          reason:  "YouTube treats the cookies in Firefox as signed out; log in to YouTube Music \
                    in Firefox"
            .into(),
          since:   now,
        };
        false
      }
      Verdict::Unknown if same || usable => false,
      Verdict::Unknown => {
        self.generation += 1;
        self.creds =
          Credentials::Unverified { session: Arc::new(candidate), generation: self.generation };
        true
      }
    }
  }

  /// The import itself failed (no profile, locked database, no cookies...).
  pub fn import_failed(&mut self, error: String) {
    self.note = Some(error.clone());
    match &self.creds {
      // A working session stays in use; the problem is only noted.
      Credentials::Unverified { .. } | Credentials::Valid { .. } => {}
      Credentials::Absent { .. } => self.creds = Credentials::Absent { reason: error },
      Credentials::Invalid { session, since, .. } => {
        self.creds =
          Credentials::Invalid { session: session.clone(), reason: error, since: *since };
      }
    }
  }

  /// A request got a signed-out response using `generation`.
  pub fn rejected(&mut self, generation: u64, now: Instant) -> bool {
    match self.usable() {
      Some((_, current)) if current == generation => {
        self.invalidate("YouTube served a signed-out response".into(), now);
        true
      }
      _ => false,
    }
  }

  fn invalidate(&mut self, reason: String, now: Instant) {
    let session = self.creds.session().cloned();
    self.creds = Credentials::Invalid { session, reason, since: now };
  }

  /// Records the outcome of a repair for the background backoff.
  pub fn repaired(&mut self, now: Instant) {
    if self.usable().is_some() {
      self.failures = 0;
      self.next_auto = now;
    } else {
      self.failures += 1;
      let delay = AUTO_RETRY_MIN.saturating_mul(1 << (self.failures - 1).min(16));
      self.next_auto = now + delay.min(AUTO_RETRY_MAX);
    }
  }

  /// Should the keeper repair the session on its own now?
  pub fn repair_due(&self, now: Instant) -> bool {
    self.usable().is_none() && now >= self.next_auto
  }

  /// Should the keeper ask YouTube about the current session again?
  pub fn verify_due(&self, now: Instant) -> bool {
    match &self.creds {
      Credentials::Unverified { .. } => true,
      Credentials::Valid { verified, .. } => {
        now.saturating_duration_since(*verified) >= VERIFY_INTERVAL
      }
      _ => false,
    }
  }
}

#[derive(Debug)]
pub struct SessionError(pub String);

impl std::fmt::Display for SessionError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
}
impl std::error::Error for SessionError {}

/// Credentials for one request, and the generation to report if YouTube turns
/// out to refuse them.
#[derive(Clone, Debug)]
pub struct SessionLease {
  pub session:    Arc<StoredSession>,
  pub generation: u64,
}

pub type Verifier =
  Arc<dyn Fn(Arc<StoredSession>) -> BoxFuture<Result<Verdict, String>> + Send + Sync>;

/// A snapshot for `steak status`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionReport {
  pub state:                 String,
  pub detail:                Option<String>,
  pub source:                Option<String>,
  pub last_import_secs:      Option<u64>,
  pub next_auto_repair_secs: Option<u64>,
}

pub struct Session {
  machine:     Mutex<Machine>,
  config:      Mutex<Config>,
  verifier:    Verifier,
  /// Serializes repairs; later callers find the work already done.
  repairing:   tokio::sync::Mutex<()>,
  /// Modification stamp of Firefox's cookie files at the last import.
  stamp:       Mutex<Option<SystemTime>>,
  polled:      Mutex<Instant>,
  verified_at: Mutex<Instant>,
  /// Whether a background job from [`Session::maintain`] is running.
  busy:        AtomicBool,
}

#[derive(Clone, Copy)]
enum Work {
  Repair,
  Verify,
  PollSource,
}

struct BusySlot<'a>(&'a AtomicBool);

impl Drop for BusySlot<'_> {
  fn drop(&mut self) { self.0.store(false, Ordering::Release); }
}

/// Returns whether `interval` has passed since `last`, and if so restarts it.
fn throttle(last: &Mutex<Instant>, now: Instant, interval: Duration) -> bool {
  let mut last = lock(last);
  if now.saturating_duration_since(*last) < interval {
    return false;
  }
  *last = now;
  true
}

impl Session {
  pub fn new(config: Config, verifier: Verifier) -> Arc<Self> {
    let now = Instant::now();
    let mut machine = Machine::new(now);
    match StoredSession::load(&session::session_path()) {
      Ok(stored) => {
        info!("loaded the saved session; it will be verified shortly");
        machine.loaded(stored);
      }
      Err(error) => debug!("no saved session: {error}"),
    }
    Arc::new(Self {
      machine: Mutex::new(machine),
      config: Mutex::new(config),
      verifier,
      repairing: tokio::sync::Mutex::new(()),
      stamp: Mutex::new(None),
      polled: Mutex::new(now),
      verified_at: Mutex::new(now.checked_sub(VERIFY_RETRY).unwrap_or(now)),
      busy: AtomicBool::new(false),
    })
  }

  /// The profile cookies are imported from.
  pub fn source(&self) -> Option<PathBuf> {
    lock(&self.config).firefox_profile.clone().or_else(firefox::discover_profile)
  }

  /// Credentials for a request. If there are none that can be used, tries one
  /// repair first; if that fails too, explains what to do.
  pub async fn lease(&self) -> Result<SessionLease, SessionError> {
    if let Some(lease) = self.usable() {
      return Ok(lease);
    }
    self.repair().await;
    self.usable().ok_or_else(|| SessionError(self.explain()))
  }

  fn usable(&self) -> Option<SessionLease> {
    lock(&self.machine).usable().map(|(session, generation)| SessionLease { session, generation })
  }

  /// A request was served a signed-out response using `lease`. Marks the
  /// session invalid and repairs it in the background.
  pub fn report_rejected(self: &Arc<Self>, lease: &SessionLease) {
    if lock(&self.machine).rejected(lease.generation, Instant::now()) {
      warn!("YouTube served a signed-out response; the session is invalid, repairing");
      let session = self.clone();
      tokio::spawn(async move { session.repair().await });
    }
  }

  /// Repairs the session if it is not usable: re-imports from Firefox and
  /// verifies the result. Never fails; the outcome is in the state.
  pub async fn repair(&self) { self.run_repair(false).await }

  /// Re-imports and verifies even if the current session looks fine.
  pub async fn repair_now(&self) { self.run_repair(true).await }

  async fn run_repair(&self, force: bool) {
    let _turn = self.repairing.lock().await;
    // Someone else's repair may have fixed it while we queued.
    if !force && self.usable().is_some() {
      return;
    }
    self.import_and_verify().await;
    lock(&self.machine).repaired(Instant::now());
  }

  /// Switches to a different Firefox profile and repairs from it.
  pub async fn set_profile(&self, profile: PathBuf) -> Result<(), SessionError> {
    if !profile.join("cookies.sqlite").is_file() {
      return Err(SessionError(format!("{} has no cookies.sqlite", profile.display())));
    }
    {
      let mut config = lock(&self.config);
      config.firefox_profile = Some(profile);
      if let Err(error) = config.save() {
        warn!("could not save the config: {error}");
      }
    }
    *lock(&self.stamp) = None;
    self.repair_now().await;
    self.usable().map(|_| ()).ok_or_else(|| SessionError(self.explain()))
  }

  /// One import-and-verify pass, bounded by [`REPAIR_TIMEOUT`]. The caller
  /// holds the repair lock.
  async fn import_and_verify(&self) {
    if tokio::time::timeout(REPAIR_TIMEOUT, self.import_and_verify_inner()).await.is_err() {
      lock(&self.machine).import_failed("repair timed out".into());
    }
  }

  async fn import_and_verify_inner(&self) {
    let Some(profile) = self.source() else {
      lock(&self.machine).import_failed(
        "no Firefox profile found; run `steak auth import-firefox <profile-dir>`".into(),
      );
      return;
    };
    let stamp = source_stamp(&profile);
    let path = profile.clone();
    let imported = tokio::time::timeout(
      IMPORT_TIMEOUT,
      tokio::task::spawn_blocking(move || session::import_firefox_profile(&path)),
    )
    .await;
    let candidate = match imported {
      Ok(Ok(Ok(candidate))) => candidate,
      Ok(Ok(Err(error))) => {
        warn!("could not import the session from {}: {error}", profile.display());
        lock(&self.machine).import_failed(error.to_string());
        return;
      }
      Ok(Err(error)) => {
        lock(&self.machine).import_failed(format!("import crashed: {error}"));
        return;
      }
      Err(_) => {
        lock(&self.machine).import_failed("import timed out".into());
        return;
      }
    };
    *lock(&self.stamp) = stamp;
    self.consider(candidate).await;
  }

  /// Asks YouTube about `session`. Any failure to get an answer is `Unknown`.
  async fn ask(&self, session: Arc<StoredSession>) -> Verdict {
    match tokio::time::timeout(VERIFY_TIMEOUT, (self.verifier)(session)).await {
      Ok(Ok(verdict)) => verdict,
      Ok(Err(error)) => {
        warn!("could not verify the session: {error}");
        Verdict::Unknown
      }
      Err(_) => {
        warn!("verifying the session timed out");
        Verdict::Unknown
      }
    }
  }

  /// Verifies `candidate` if that can teach us anything, and applies the
  /// verdict.
  async fn consider(&self, candidate: StoredSession) {
    if lock(&self.machine).plan_candidate(&candidate) == CandidatePlan::Skip {
      debug!("Firefox has the same cookies as the current session");
      lock(&self.machine).last_import = Some(Instant::now());
      return;
    }
    let verdict = self.ask(Arc::new(candidate.clone())).await;
    let installed = lock(&self.machine).candidate(candidate.clone(), verdict, Instant::now());
    info!("session verdict from YouTube: {verdict:?}");
    if installed {
      if let Err(error) = candidate.save(&session::session_path()) {
        warn!("could not save the session: {error}");
      }
    }
  }

  /// Background upkeep, called by the keeper every few seconds. Starts at most
  /// one piece of work and does not wait for it.
  pub fn maintain(self: &Arc<Self>) {
    let now = Instant::now();
    let (repair, verify) = {
      let machine = lock(&self.machine);
      (machine.repair_due(now), machine.verify_due(now))
    };
    let work = if repair {
      Work::Repair
    } else if verify {
      if !throttle(&self.verified_at, now, VERIFY_RETRY) {
        return;
      }
      Work::Verify
    } else if throttle(&self.polled, now, SOURCE_POLL) {
      Work::PollSource
    } else {
      return;
    };
    // One background job at a time; the guard frees the slot even if the job
    // panics.
    if self.busy.swap(true, Ordering::AcqRel) {
      return;
    }
    let session = self.clone();
    tokio::spawn(async move {
      let _slot = BusySlot(&session.busy);
      match work {
        Work::Repair => {
          info!("the session is not usable; repairing it in the background");
          session.repair().await;
        }
        Work::Verify => session.reverify().await,
        Work::PollSource => session.poll_source().await,
      }
    });
  }

  /// Asks YouTube about the current session again.
  async fn reverify(&self) {
    {
      let _turn = self.repairing.lock().await;
      let Some((current, _)) = lock(&self.machine).usable() else { return };
      let verdict = self.ask(current.clone()).await;
      if verdict == Verdict::Unknown {
        return;
      }
      lock(&self.machine).candidate((*current).clone(), verdict, Instant::now());
    }
    // If YouTube just turned the session down, fix it now rather than waiting
    // for the keeper's next round.
    if self.usable().is_none() {
      warn!("YouTube no longer accepts the session; repairing it");
      self.repair().await;
    }
  }

  /// Re-imports if Firefox's cookie files changed since the last import.
  async fn poll_source(&self) {
    let Some(profile) = self.source() else { return };
    if source_stamp(&profile) == *lock(&self.stamp) {
      return;
    }
    debug!("Firefox's cookies changed; re-importing");
    let _turn = self.repairing.lock().await;
    self.import_and_verify().await;
    lock(&self.machine).repaired(Instant::now());
  }

  pub fn report(&self) -> SessionReport {
    let now = Instant::now();
    let machine = lock(&self.machine);
    let detail = match machine.creds() {
      Credentials::Absent { reason } => Some(reason.clone()),
      Credentials::Invalid { reason, .. } => Some(reason.clone()),
      _ => machine.note.clone(),
    };
    SessionReport {
      state: machine.creds().label().to_owned(),
      detail,
      source: self.source().map(|path| path.display().to_string()),
      last_import_secs: machine.last_import.map(|at| now.saturating_duration_since(at).as_secs()),
      next_auto_repair_secs: machine
        .usable()
        .is_none()
        .then(|| machine.next_auto.saturating_duration_since(now).as_secs()),
    }
  }

  /// What to tell a user whose command needs a session that is not usable.
  fn explain(&self) -> String {
    let report = self.report();
    let detail = report.detail.unwrap_or_default();
    format!(
      "no usable YouTube session ({}): {detail}. Log in to YouTube Music in Firefox, then run \
       `steak auth repair`",
      report.state
    )
  }
}

/// The newest modification time among Firefox's cookie files.
fn source_stamp(profile: &std::path::Path) -> Option<SystemTime> {
  ["cookies.sqlite", "cookies.sqlite-wal"]
    .iter()
    .filter_map(|name| std::fs::metadata(profile.join(name)).and_then(|meta| meta.modified()).ok())
    .max()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn stored(cookies: &str) -> StoredSession { StoredSession { cookie_header: cookies.into() } }

  fn machine() -> (Machine, Instant) {
    let now = Instant::now();
    (Machine::new(now), now)
  }

  #[test]
  fn starts_absent_and_unusable() {
    let (machine, _) = machine();
    assert!(machine.usable().is_none());
    assert_eq!(machine.creds().label(), "absent");
  }

  #[test]
  fn loaded_session_is_usable_before_it_is_verified() {
    let (mut machine, _) = machine();
    machine.loaded(stored("a=1"));
    assert_eq!(machine.creds().label(), "unverified");
    assert!(machine.usable().is_some());
  }

  #[test]
  fn signed_in_candidate_becomes_valid() {
    let (mut machine, now) = machine();
    assert!(machine.candidate(stored("a=1"), Verdict::SignedIn, now));
    assert_eq!(machine.creds().label(), "valid");
  }

  #[test]
  fn signed_out_candidate_never_replaces_a_working_session() {
    let (mut machine, now) = machine();
    machine.candidate(stored("good=1"), Verdict::SignedIn, now);
    assert!(!machine.candidate(stored("bad=1"), Verdict::SignedOut, now));
    assert_eq!(machine.creds().label(), "valid");
    assert_eq!(machine.usable().unwrap().0.cookie_header, "good=1");
  }

  #[test]
  fn signed_out_verdict_on_the_current_cookies_invalidates_them() {
    let (mut machine, now) = machine();
    machine.candidate(stored("a=1"), Verdict::SignedIn, now);
    machine.candidate(stored("a=1"), Verdict::SignedOut, now);
    assert_eq!(machine.creds().label(), "invalid");
    assert!(machine.usable().is_none());
  }

  #[test]
  fn unknown_verdict_changes_nothing_for_a_usable_session() {
    let (mut machine, now) = machine();
    machine.candidate(stored("a=1"), Verdict::SignedIn, now);
    assert!(!machine.candidate(stored("b=1"), Verdict::Unknown, now));
    assert_eq!(machine.usable().unwrap().0.cookie_header, "a=1");
  }

  #[test]
  fn unknown_verdict_is_given_the_benefit_of_the_doubt_when_nothing_works() {
    let (mut machine, now) = machine();
    assert!(machine.candidate(stored("a=1"), Verdict::Unknown, now));
    assert_eq!(machine.creds().label(), "unverified");
  }

  #[test]
  fn rejection_only_counts_for_the_generation_that_was_used() {
    let (mut machine, now) = machine();
    machine.candidate(stored("a=1"), Verdict::SignedIn, now);
    let (_, generation) = machine.usable().unwrap();
    machine.candidate(stored("b=1"), Verdict::SignedIn, now);
    // A straggler still holding the old session cannot sink the new one.
    assert!(!machine.rejected(generation, now));
    assert_eq!(machine.creds().label(), "valid");
    let (_, current) = machine.usable().unwrap();
    assert!(machine.rejected(current, now));
    assert_eq!(machine.creds().label(), "invalid");
  }

  #[test]
  fn same_rejected_cookies_are_not_verified_again() {
    let (mut machine, now) = machine();
    machine.candidate(stored("a=1"), Verdict::SignedIn, now);
    let (_, generation) = machine.usable().unwrap();
    machine.rejected(generation, now);
    assert_eq!(machine.plan_candidate(&stored("a=1")), CandidatePlan::Skip);
    assert_eq!(machine.plan_candidate(&stored("a=2")), CandidatePlan::Verify);
  }

  #[test]
  fn import_failure_keeps_a_working_session_and_fails_an_absent_one() {
    let (mut machine, now) = machine();
    machine.import_failed("locked".into());
    assert_eq!(machine.creds().label(), "absent");
    machine.candidate(stored("a=1"), Verdict::SignedIn, now);
    machine.import_failed("locked".into());
    assert_eq!(machine.creds().label(), "valid");
  }

  #[test]
  fn background_repair_backs_off_but_recovers_immediately() {
    let (mut machine, now) = machine();
    assert!(machine.repair_due(now));
    machine.repaired(now);
    assert!(!machine.repair_due(now + Duration::from_secs(59)));
    assert!(machine.repair_due(now + Duration::from_secs(60)));
    machine.repaired(now + Duration::from_secs(60));
    assert!(!machine.repair_due(now + Duration::from_secs(60 + 119)));
    assert!(machine.repair_due(now + Duration::from_secs(60 + 120)));
    for _ in 0..40 {
      machine.repaired(now);
    }
    assert!(machine.next_auto <= now + AUTO_RETRY_MAX);
    machine.candidate(stored("a=1"), Verdict::SignedIn, now);
    machine.repaired(now);
    assert_eq!(machine.failures, 0);
  }

  #[test]
  fn verification_is_due_for_unverified_and_for_old_valid_sessions() {
    let (mut machine, now) = machine();
    machine.loaded(stored("a=1"));
    assert!(machine.verify_due(now));
    machine.candidate(stored("a=1"), Verdict::SignedIn, now);
    assert!(!machine.verify_due(now + Duration::from_secs(60)));
    assert!(machine.verify_due(now + VERIFY_INTERVAL));
  }
}
