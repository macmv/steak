//! The playback state machine.
//!
//! ```text
//!            start(id)                  on_started
//!   Idle ---------------> Resolving ---------------> Playing
//!    ^ ^                      |                         |
//!    | |                      | job failed              | job failed
//!    | |                      v                         v
//!    | |                   Failed <----------------------+
//!    | |
//!    | +-- stop(), or a newer start() (from any state)
//!    |
//!    +---- job finished ok (from Playing)
//! ```
//!
//! Rules:
//!
//! - There is at most one playback job. Starting a new one, or stopping, aborts
//!   the old one outright (dropping it kills `ffplay`).
//! - Every job has a generation. Only the current generation may change the
//!   state, so an aborted job, or one that finishes late, can never overwrite
//!   what a newer one reported.
//! - A job that fails or panics lands in `Failed`, which holds until the next
//!   `start` or `stop`; it never wedges the machine.
//! - `Resolving` and `Playing` are only entered by `start` and the job's own
//!   `on_started`, and only left through `finish`, `stop` or a newer `start`.

use std::{
  future::Future,
  sync::{Arc, Mutex},
  time::Duration,
};

use log::{info, warn};
use tokio::{sync::watch, task::JoinHandle};

use super::cell::lock;
use crate::ipc::PlaybackState;

struct Inner {
  generation: u64,
  state:      PlaybackState,
  task:       Option<JoinHandle<()>>,
}

pub struct Playback {
  inner:   Mutex<Inner>,
  changed: watch::Sender<u64>,
}

/// Handed to a job so it can report that audio is now playing.
pub struct Started {
  playback:   Arc<Playback>,
  generation: u64,
  video_id:   String,
}

impl Started {
  pub fn mark(self) {
    let mut inner = lock(&self.playback.inner);
    if inner.generation == self.generation {
      inner.state = PlaybackState::Playing { video_id: self.video_id };
      drop(inner);
      self.playback.notify();
    }
  }
}

/// Aborts the job when dropped, so aborting the supervising task also stops
/// the job.
struct AbortOnDrop(JoinHandle<Result<(), String>>);

impl Drop for AbortOnDrop {
  fn drop(&mut self) { self.0.abort(); }
}

impl Playback {
  pub fn new() -> Arc<Self> {
    Arc::new(Self {
      inner:   Mutex::new(Inner {
        generation: 0,
        state:      PlaybackState::Idle,
        task:       None,
      }),
      changed: watch::channel(0).0,
    })
  }

  fn notify(&self) { self.changed.send_modify(|version| *version += 1); }

  pub fn state(&self) -> PlaybackState { lock(&self.inner).state.clone() }

  /// Replaces whatever is playing with `job`. Returns the new generation, for
  /// [`Self::wait_for_start`].
  pub fn start<F, Fut>(self: &Arc<Self>, video_id: String, job: F) -> u64
  where
    F: FnOnce(Started) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), String>> + Send + 'static,
  {
    let mut inner = lock(&self.inner);
    inner.generation += 1;
    let generation = inner.generation;
    if let Some(old) = inner.task.take() {
      old.abort();
    }
    inner.state = PlaybackState::Resolving { video_id: video_id.clone() };

    let playback = self.clone();
    let started = Started { playback: self.clone(), generation, video_id: video_id.clone() };
    inner.task = Some(tokio::spawn(async move {
      // The job runs in its own task so a panic is reported rather than
      // silently ending this supervisor; the guard stops it if we are aborted.
      let mut job = AbortOnDrop(tokio::spawn(job(started)));
      let result = match (&mut job.0).await {
        Ok(result) => result,
        Err(error) if error.is_cancelled() => return,
        Err(error) => Err(format!("playback crashed: {error}")),
      };
      playback.finish(generation, video_id, result);
    }));
    drop(inner);
    self.notify();
    generation
  }

  fn finish(&self, generation: u64, video_id: String, result: Result<(), String>) {
    let mut inner = lock(&self.inner);
    if inner.generation != generation {
      return;
    }
    inner.state = match result {
      Ok(()) => {
        info!("finished playing {video_id}");
        PlaybackState::Idle
      }
      Err(error) => {
        warn!("playing {video_id} failed: {error}");
        PlaybackState::Failed { video_id, error }
      }
    };
    inner.task = None;
    drop(inner);
    self.notify();
  }

  pub fn stop(&self) {
    let mut inner = lock(&self.inner);
    inner.generation += 1;
    if let Some(task) = inner.task.take() {
      task.abort();
    }
    inner.state = PlaybackState::Idle;
    drop(inner);
    self.notify();
  }

  /// Waits until generation `generation` is no longer resolving: playing,
  /// finished, or failed. Returns the failure, or an error if a newer `start`
  /// or `stop` replaced it, or `timeout` passed.
  pub async fn wait_for_start(&self, generation: u64, timeout: Duration) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
      let mut changes = self.changed.subscribe();
      {
        let inner = lock(&self.inner);
        if inner.generation != generation {
          return Err("playback was replaced by a newer request".into());
        }
        match &inner.state {
          PlaybackState::Resolving { .. } => {}
          PlaybackState::Failed { error, .. } => return Err(error.clone()),
          PlaybackState::Playing { .. } | PlaybackState::Idle => return Ok(()),
        }
      }
      if tokio::time::timeout_at(deadline, changes.changed()).await.is_err() {
        return Err("timed out waiting for playback to start".into());
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const SECOND: Duration = Duration::from_secs(1);

  #[tokio::test]
  async fn start_reaches_playing_then_idle() {
    let playback = Playback::new();
    let (finish, finished) = tokio::sync::oneshot::channel::<()>();
    let generation = playback.start("a".into(), |started| async move {
      started.mark();
      let _ = finished.await;
      Ok(())
    });
    playback.wait_for_start(generation, SECOND).await.unwrap();
    assert!(matches!(playback.state(), PlaybackState::Playing { .. }));
    finish.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(matches!(playback.state(), PlaybackState::Idle));
  }

  #[tokio::test]
  async fn failure_and_panic_land_in_failed() {
    let playback = Playback::new();
    let generation = playback.start("a".into(), |_| async { Err("no audio".to_owned()) });
    assert_eq!(playback.wait_for_start(generation, SECOND).await.unwrap_err(), "no audio");

    let generation = playback.start("b".into(), |_| async { panic!("boom") });
    let error = playback.wait_for_start(generation, SECOND).await.unwrap_err();
    assert!(error.contains("crashed"), "{error}");
    // Failed is not terminal: the next start works.
    let generation = playback.start("c".into(), |started| async move {
      started.mark();
      Ok(())
    });
    playback.wait_for_start(generation, SECOND).await.unwrap();
  }

  #[tokio::test]
  async fn stop_aborts_the_job_and_a_newer_start_wins() {
    let playback = Playback::new();
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    struct Flag(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Flag {
      fn drop(&mut self) { self.0.store(true, std::sync::atomic::Ordering::SeqCst); }
    }
    let flag = Flag(dropped.clone());
    let first = playback.start("a".into(), move |_| async move {
      let _flag = flag;
      std::future::pending::<()>().await;
      Ok(())
    });
    let second = playback.start("b".into(), |started| async move {
      started.mark();
      std::future::pending::<()>().await;
      Ok(())
    });
    assert!(playback.wait_for_start(first, SECOND).await.is_err());
    playback.wait_for_start(second, SECOND).await.unwrap();
    // The first job's future was dropped (which is what kills ffplay).
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    playback.stop();
    assert!(matches!(playback.state(), PlaybackState::Idle));
  }
}
