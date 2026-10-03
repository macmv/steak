//! The daemon's caches: what they hold, how long, and what they depend on.
//!
//! Everything is in memory; nothing is written to disk. Each cache is a
//! [`Cell`] with the lifecycle described in `cell.rs`; this file is where each
//! one states its policy and where the dependencies between them are enforced.
//!
//! | cache       | holds                                         | ttl | depends on |
//! |-------------|-----------------------------------------------|-----|------------|
//! | `bootstrap` | Music web app config: API key, client version | 6h  | nothing    |
//! | `player`    | player's signature timestamp + loaded solver  | 24h | bootstrap  |
//!
//! The session is not a cache; see `session.rs`.
//!
//! **The `player` cache is bound to `bootstrap`'s player script URL.** The
//! solver is only valid for the player script it was built from, so whenever
//! the two disagree (the bootstrap learned of a new player), the player is
//! thrown away and rebuilt from the new URL. This is checked on every use
//! ([`Caches::player_for`]) and by the background keeper
//! ([`Caches::maintain`]), so a rotated player never produces a request signed
//! with the old one.
//!
//! **Consumers report failures.** A value that fails in use (the solver thread
//! died, YouTube served a different player than the one cached) is reported
//! with the generation that was used, which invalidates exactly that value.
//! The next use rebuilds it. Nothing is ever "cleared and hoped for the best":
//! a rebuild that fails goes through the cell's backoff and surfaces its error.

use std::{sync::Arc, time::Duration};

use st_api::{
  Url,
  account::session::StoredSession,
  catalog::HomeParser,
  innertube::{
    bootstrap::{BootstrapData, Bootstrapper, WebBootstrapper},
    client::{MusicClient, MusicClientConfig},
  },
  playback::{
    player_script::{EjsCipherResolver, player_cache_key, signature_timestamp},
    solver::{self, Solver},
  },
};

use super::{
  cell::{Cell, CellError, CellReport, Lease, Policy},
  session::Verdict,
};

const BOOTSTRAP: Policy = Policy {
  name:            "bootstrap",
  ttl:             Duration::from_secs(6 * 60 * 60),
  refresh_ahead:   Duration::from_secs(30 * 60),
  max_stale:       Duration::from_secs(12 * 60 * 60),
  refresh_timeout: Duration::from_secs(30),
  backoff_min:     Duration::from_secs(5),
  backoff_max:     Duration::from_secs(5 * 60),
};

const PLAYER: Policy = Policy {
  name:            "player",
  ttl:             Duration::from_secs(24 * 60 * 60),
  refresh_ahead:   Duration::from_secs(60 * 60),
  max_stale:       Duration::from_secs(24 * 60 * 60),
  // Building parses a 2.6 MB script, which takes ~15s unoptimized.
  refresh_timeout: Duration::from_secs(180),
  backoff_min:     Duration::from_secs(10),
  backoff_max:     Duration::from_secs(10 * 60),
};

/// A challenge to solve while building the player, so the preprocessed script
/// is produced and the build proves the solver works before anyone relies on
/// it. Any `n` value will do.
const SELF_TEST_CHALLENGE: &str = "abcdefghijk";

/// What the player cache holds.
pub struct PlayerBuild {
  /// The player version, from `player_cache_key`.
  pub key:                 String,
  pub signature_timestamp: Option<u32>,
  pub solver:              Solver,
}

pub struct Caches {
  pub http:      st_api::Client,
  pub bootstrap: Arc<Cell<BootstrapData>>,
  pub player:    Arc<Cell<PlayerBuild>>,
}

impl Caches {
  pub fn new(http: st_api::Client) -> Arc<Self> {
    let bootstrap = {
      let http = http.clone();
      Cell::new(BOOTSTRAP, move || fetch_bootstrap(http.clone()))
    };
    let player = {
      let http = http.clone();
      let bootstrap = bootstrap.clone();
      Cell::new(PLAYER, move || build_player(http.clone(), bootstrap.clone()))
    };
    Arc::new(Self { http, bootstrap, player })
  }

  /// The player cache entry that matches `bootstrap`'s player script,
  /// rebuilding it if it was built for a different one.
  pub async fn player_for(
    &self,
    bootstrap: &BootstrapData,
  ) -> Result<Lease<PlayerBuild>, CellError> {
    let wanted = script_key(bootstrap)?;
    // Once is enough to replace a mismatched entry; twice means the player
    // cache keeps being built from something other than this bootstrap.
    for _ in 0..2 {
      let player = self.player.get().await?;
      if player.key == wanted {
        return Ok(player);
      }
      self.player.invalidate(player.generation);
    }
    Err(CellError(format!("player: could not build a solver for player {wanted}")))
  }

  /// Background upkeep: starts refreshes that are due, and drops a player that
  /// no longer matches the bootstrap.
  pub fn maintain(&self) {
    self.bootstrap.maintain();
    if let (Some(bootstrap), Some(player)) = (self.bootstrap.peek(), self.player.peek()) {
      if script_key(&bootstrap).is_ok_and(|wanted| wanted != player.key) {
        self.player.invalidate(player.generation);
      }
    }
    self.player.maintain();
  }

  pub fn reports(&self) -> Vec<CellReport> { vec![self.bootstrap.report(), self.player.report()] }

  /// Builds a client that sends `session`'s cookies. Cheap: it is rebuilt per
  /// request because the authorization header is time-bound.
  pub fn music_client(&self, bootstrap: &BootstrapData, session: &StoredSession) -> MusicClient {
    let mut config = MusicClientConfig::default();
    bootstrap.apply_to(&mut config);
    config.headers.extend(session.auth_session().request_headers());
    MusicClient::new(self.http.clone(), config)
  }

  /// Asks YouTube whether it treats `session` as signed in, by loading the
  /// homepage.
  pub async fn verify(&self, session: &StoredSession) -> Result<Verdict, String> {
    let bootstrap = self.bootstrap.get().await.map_err(|error| error.to_string())?;
    let client = self.music_client(&bootstrap, session);
    match client.browse("FEmusic_home", None).await {
      Ok(response) if HomeParser.logged_in(&response) => Ok(Verdict::SignedIn),
      Ok(_) => Ok(Verdict::SignedOut),
      Err(error) if error.status() == Some(reqwest_status_unauthorized()) => Ok(Verdict::SignedOut),
      Err(error) => Err(error.without_url().to_string()),
    }
  }
}

fn reqwest_status_unauthorized() -> st_api::StatusCode { st_api::StatusCode::UNAUTHORIZED }

fn script_key(bootstrap: &BootstrapData) -> Result<String, CellError> {
  bootstrap
    .script_url()
    .map(|url| player_cache_key(&url))
    .ok_or_else(|| CellError("bootstrap: no player script URL".into()))
}

async fn fetch_bootstrap(http: st_api::Client) -> Result<BootstrapData, String> {
  let bootstrapper = WebBootstrapper::new(http);
  let mut data = bootstrapper.bootstrap().await.map_err(|error| error.to_string())?;
  if data.player_script_url.is_none() {
    // The landing page does not always name the player; the watch page does.
    let url = bootstrapper.player_script_url().await.map_err(|error| error.to_string())?;
    data.player_script_url = url.map(String::from);
  }
  if data.script_url().is_none() {
    return Err("YouTube Music did not advertise a player script".into());
  }
  Ok(data)
}

async fn build_player(
  http: st_api::Client,
  bootstrap: Arc<Cell<BootstrapData>>,
) -> Result<PlayerBuild, String> {
  let bootstrap = bootstrap.get().await.map_err(|error| error.to_string())?;
  let url: Url = bootstrap.script_url().ok_or("bootstrap has no player script URL")?;
  // A resolver per build: it caches every script it downloads, and this one is
  // only needed for the build.
  let script =
    EjsCipherResolver::new(http).script(&url).await.map_err(|error| error.to_string())?;
  let signature_timestamp = signature_timestamp(&script);
  if signature_timestamp.is_none() {
    log::warn!("no signature timestamp in the player script; player responses may not match it");
  }
  let preprocessed = tokio::task::spawn_blocking(move || {
    solver::solve(&script, &[], &[SELF_TEST_CHALLENGE.to_owned()])
  })
  .await
  .map_err(|error| format!("solver crashed: {error}"))?
  .map_err(|error| error.to_string())?
  .preprocessed
  .ok_or("the solver produced no preprocessed player")?;

  let solver = Solver::spawn(preprocessed.into());
  // Prove the loaded solver answers before anyone depends on it.
  solver
    .solve(Vec::new(), vec![SELF_TEST_CHALLENGE.to_owned()])
    .await
    .map_err(|error| format!("the solver failed its self-test: {error}"))?;
  Ok(PlayerBuild { key: player_cache_key(&url), signature_timestamp, solver })
}
