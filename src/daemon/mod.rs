//! The steak daemon: the one long-lived process that talks to YouTube Music.
//!
//! The CLI is a thin client (like `mpc` for MPD); everything stateful lives
//! here, in memory: the session, the caches, and what is playing.
//!
//! # State machines
//!
//! Every piece of state has an explicit lifecycle, written as a pure state
//! machine with tests, in its own file:
//!
//! | file          | state                         | states                                       |
//! |---------------|-------------------------------|----------------------------------------------|
//! | `session.rs`  | the YouTube session           | Absent, Unverified, Valid, Invalid           |
//! | `cell.rs`     | every cache (generic)         | Empty, Refreshing, Ready, Backoff            |
//! | `caches.rs`   | which caches exist, and their ttl/backoff/dependencies | |
//! | `playback.rs` | what is playing               | Idle, Resolving, Playing, Failed             |
//!
//! # Robustness
//!
//! The daemon is never meant to be restarted by hand. The rules that make that
//! true, each enforced where it lives:
//!
//! 1. **Every wait has a deadline.** Fetches, imports, verifications, streams,
//!    and requests all time out, and no state machine can stay in a transient
//!    state past its timeout (`Refreshing`, `Resolving`, repairs).
//! 2. **Every task is isolated.** A request, a cache refresh, a playback job
//!    and a background job each run in their own task; a panic becomes an error
//!    for that one operation and a state-machine failure, never a dead loop.
//! 3. **No lock is held across an `.await`**, and poisoned locks are ignored.
//! 4. **Failures are state, not exits.** A cache that cannot refresh backs off
//!    and retries; a session that cannot be repaired is `Invalid` and retried
//!    (on a timer, and by every command that needs it). Nothing here calls
//!    `exit` on an error.
//! 5. **Stale results are discarded.** Refreshes, repairs and playback jobs
//!    carry generations, so a late finisher cannot clobber newer state.
//! 6. **Background loops are supervised** (`supervise`) and restarted if they
//!    die.
//! 7. **A watchdog thread** outside the async runtime notices if the runtime
//!    itself stops making progress and exits, so the service manager restarts
//!    the process (`steak daemon install` sets `Restart=always`).

pub mod api;
pub mod audio;
pub mod caches;
pub mod cell;
pub mod config;
pub mod paths;
pub mod playback;
pub mod server;
pub mod service;
pub mod session;

use std::{
  sync::Arc,
  time::{Duration, Instant},
};

use futures_util::FutureExt;
use log::{error, info, warn};

use crate::ipc::{Request, Response, StatusReport};
use caches::Caches;
use playback::Playback;
use session::Session;

/// How often the keeper starts due refreshes and repairs.
const KEEPER_INTERVAL: Duration = Duration::from_secs(15);

pub struct Daemon {
  pub caches:   Arc<Caches>,
  pub session:  Arc<Session>,
  pub playback: Arc<Playback>,
  started:      Instant,
}

impl Daemon {
  pub fn new() -> Arc<Self> {
    let caches = Caches::new(st_api::http_client());
    let verifier: session::Verifier = {
      let caches = caches.clone();
      Arc::new(move |session| {
        let caches = caches.clone();
        async move { caches.verify(&session).await }.boxed()
      })
    };
    let session = Session::new(config::Config::load(), verifier);
    Arc::new(Self { caches, session, playback: Playback::new(), started: Instant::now() })
  }

  pub async fn handle(self: Arc<Self>, request: Request) -> Response {
    fn failed(error: impl std::fmt::Display) -> Response { Response::Error(error.to_string()) }

    match request {
      Request::Ping => Response::Pong,
      Request::Status => Response::Status(self.status()),
      Request::Home { more } => match self.home(more).await {
        Ok((shelves, logged_in)) => Response::Home { shelves, logged_in },
        Err(error) => failed(error),
      },
      Request::Radio { video_id, more } => match self.radio(&video_id, more).await {
        Ok(tracks) => Response::Tracks(tracks),
        Err(error) => failed(error),
      },
      Request::Playlist { playlist_id, more } => match self.playlist(&playlist_id, more).await {
        Ok(tracks) => Response::Tracks(tracks),
        Err(error) => failed(error),
      },
      Request::Play { video_id } => self.play(video_id).await,
      Request::Stop => {
        self.playback.stop();
        Response::Stopped
      }
      Request::AuthImport { profile } => {
        let result = match profile.or_else(|| self.session.source()) {
          Some(profile) => self.session.set_profile(profile).await,
          None => Err(session::SessionError(
            "no Firefox profile found; pass the profile directory".into(),
          )),
        };
        match result {
          Ok(()) => Response::Session(self.session.report()),
          Err(error) => failed(error),
        }
      }
      Request::AuthRepair => {
        self.session.repair_now().await;
        Response::Session(self.session.report())
      }
    }
  }

  /// Starts playing `video_id` and answers once audio is coming out or it
  /// failed to start.
  async fn play(self: &Arc<Self>, video_id: String) -> Response {
    let daemon = self.clone();
    let id = video_id.clone();
    let generation = self.playback.start(video_id.clone(), move |started| async move {
      info!("playing {id}");
      let audio = daemon.open_audio(&id).await.map_err(|error| error.to_string())?;
      audio::play_stream(audio, || started.mark()).await
    });
    match self.playback.wait_for_start(generation, Duration::from_secs(190)).await {
      Ok(()) => Response::Playing { video_id },
      Err(error) => Response::Error(error),
    }
  }

  pub fn status(&self) -> StatusReport {
    StatusReport {
      version:     env!("CARGO_PKG_VERSION").into(),
      uptime_secs: self.started.elapsed().as_secs(),
      playback:    self.playback.state(),
      session:     self.session.report(),
      caches:      self.caches.reports(),
    }
  }

  /// One round of background upkeep. Starts work; never waits for it.
  fn keep(&self) {
    self.caches.maintain();
    self.session.maintain();
  }
}

/// Runs the daemon until it is told to stop.
pub async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
  let socket = paths::socket();
  let listener = server::bind(&socket).await?;
  info!("steak daemon {} listening on {}", env!("CARGO_PKG_VERSION"), socket.display());

  let daemon = Daemon::new();
  watchdog(Duration::from_secs(10), Duration::from_secs(60));
  {
    let daemon = daemon.clone();
    supervise("keeper", move || {
      let daemon = daemon.clone();
      async move {
        loop {
          daemon.keep();
          tokio::time::sleep(KEEPER_INTERVAL).await;
        }
      }
    });
  }

  server::serve(listener, daemon.clone(), shutdown_signal()).await;

  info!("shutting down");
  daemon.playback.stop();
  let _ = std::fs::remove_file(&socket);
  Ok(())
}

async fn shutdown_signal() {
  use tokio::signal::unix::{SignalKind, signal};
  match (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) {
    (Ok(mut terminate), Ok(mut interrupt)) => {
      tokio::select! { _ = terminate.recv() => {}, _ = interrupt.recv() => {} }
    }
    // Without signal handlers the daemon can only be killed, which is fine.
    _ => std::future::pending().await,
  }
}

/// Runs `make()` as a background task forever: if it panics or returns, it is
/// started again after a second.
fn supervise<F, Fut>(name: &'static str, make: F)
where
  F: Fn() -> Fut + Send + 'static,
  Fut: std::future::Future<Output = ()> + Send + 'static,
{
  tokio::spawn(async move {
    loop {
      match tokio::spawn(make()).await {
        Ok(()) => warn!("background task {name} returned; restarting it"),
        Err(error) => error!("background task {name} crashed: {error}; restarting it"),
      }
      tokio::time::sleep(Duration::from_secs(1)).await;
    }
  });
}

/// Exits the process if the async runtime stops making progress.
///
/// Every state machine bounds its own waits, so this should never fire. It is
/// the backstop for the bug nobody anticipated (a blocking call on a runtime
/// thread, a deadlock): a thread outside the runtime asks it to run a trivial
/// task, and if that does not happen for `limit`, the process exits so that
/// the service manager starts a fresh one.
fn watchdog(every: Duration, limit: Duration) {
  let runtime = tokio::runtime::Handle::current();
  let spawned = std::thread::Builder::new().name("watchdog".into()).spawn(move || {
    loop {
      std::thread::sleep(every);
      let (alive, answer) = std::sync::mpsc::channel();
      runtime.spawn(async move {
        let _ = alive.send(());
      });
      if answer.recv_timeout(limit).is_err() {
        error!(
          "the runtime made no progress for {}s; exiting so it can be restarted",
          limit.as_secs()
        );
        std::process::exit(70);
      }
    }
  });
  if let Err(error) = spawned {
    warn!("could not start the watchdog: {error}");
  }
}
