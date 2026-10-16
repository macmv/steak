//! Telling YouTube what was actually played.
//!
//! One task per playback follows the player's published state (the same
//! position, pause and end the clients see) and turns it into the web player's
//! stats pings. The pings and when they are due are `st_api::playback::stats`;
//! this is the clock and the I/O around it.
//!
//! The task is detached from the audio job on purpose: skipping a track aborts
//! the job, and the last ping, with the position reached, still has to be sent.

use std::{
  sync::Weak,
  time::{Duration, Instant},
};

use log::{debug, warn};
use st_api::playback::stats::{Ping, PlaybackTracking, State, WatchSession};
use st_proto::ipc::Transport;
use tokio::sync::watch;

use super::{Daemon, player::Published};

/// A ping that takes longer than this is dropped.
const SEND_TIMEOUT: Duration = Duration::from_secs(20);

/// What the player was doing, as far as this track is concerned.
enum Observed {
  /// Loading, or not this track yet.
  Waiting,
  Playing(f64),
  Paused(f64),
  /// This playback is over: the track ended, was skipped, stopped, or began
  /// again.
  Over,
}

fn observe(view: &Published, video_id: &str, last: f64) -> Observed {
  let view = view.current();
  if view.current().map(|track| track.video_id.as_str()) != Some(video_id) {
    return Observed::Over;
  }
  let position = view.position_ms as f64 / 1000.0;
  match view.transport {
    Transport::Loading => Observed::Waiting,
    // The position only moves forward within one playback, so a jump back is
    // the same track starting over.
    Transport::Playing | Transport::Paused if position + 1.0 < last => Observed::Over,
    Transport::Playing => Observed::Playing(position),
    Transport::Paused => Observed::Paused(position),
    Transport::Idle | Transport::Ended | Transport::Failed(_) => Observed::Over,
  }
}

/// Reports one playback of `video_id` until it is over.
pub async fn track(
  daemon: Weak<Daemon>,
  tracking: PlaybackTracking,
  video_id: String,
  mut view: watch::Receiver<Published>,
) {
  let Some(uptime) = daemon.upgrade().map(|daemon| daemon.started.elapsed().as_secs_f64()) else {
    return;
  };
  let opened = Instant::now();
  let mut session = WatchSession::new(&tracking, uptime);

  // The latest position seen, and when: the track kept playing since, if it
  // was playing, which matters when it is skipped between two looks.
  let mut last = 0.0_f64;
  let mut last_at = opened;
  let mut last_playing = false;

  loop {
    let due = session.next_flush().map(|at| opened + Duration::from_secs_f64(at));
    tokio::select! {
      changed = view.changed() => {
        if changed.is_err() {
          break;
        }
      }
      () = async {
        match due {
          Some(due) => tokio::time::sleep_until(due.into()).await,
          None => std::future::pending().await,
        }
      } => {}
    }

    let now = opened.elapsed().as_secs_f64();
    let observed = observe(&view.borrow(), &video_id, last);
    let pings = match observed {
      Observed::Waiting => Vec::new(),
      Observed::Playing(position) | Observed::Paused(position) => {
        let playing = matches!(observed, Observed::Playing(_));
        let state = if playing { State::Playing } else { State::Paused };
        last = position;
        last_at = Instant::now();
        last_playing = playing;
        session.observe(now, state, position)
      }
      Observed::Over => {
        if !session.started() {
          debug!("{video_id} was left before it played; nothing to report");
          return;
        }
        let mut position = last + if last_playing { last_at.elapsed().as_secs_f64() } else { 0.0 };
        if let Some(length) = tracking.length {
          position = position.min(length);
        }
        send(&daemon, &tracking, &video_id, session.finish(now, position)).await;
        return;
      }
    };
    send(&daemon, &tracking, &video_id, pings).await;
  }
}

async fn send(
  daemon: &Weak<Daemon>,
  tracking: &PlaybackTracking,
  video_id: &str,
  pings: Vec<Ping>,
) {
  for ping in pings {
    let Some(daemon) = daemon.upgrade() else { return };
    let kind = format!("{:?}", ping.kind);
    let result = tokio::time::timeout(SEND_TIMEOUT, daemon.backend().report(tracking, ping)).await;
    match result {
      Ok(Ok(())) => debug!("reported {kind} of {video_id}"),
      Ok(Err(error)) => warn!("could not report {kind} of {video_id}: {error}"),
      Err(_) => warn!("reporting {kind} of {video_id} took longer than {SEND_TIMEOUT:?}"),
    }
  }
}
