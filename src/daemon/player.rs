//! The player actor: the one task that owns the
//! [`State`](super::machine::State) and runs its effects.
//!
//! Clients never touch the state. They send [`Command`]s into a queue, which
//! this task applies one at a time through [`reduce`], so two clients racing
//! are simply ordered. After every change the new [`PlayerView`] is published
//! on a `watch` channel: each subscriber reads the latest one at its own pace,
//! so a slow client can never hold the player, or any other client, back.
//!
//! Effects (an audio job, a queue fetch) run in their own tasks and report back
//! as events, which re-enter through the same queue. A panic in the reducer
//! resets the player to idle instead of killing the task.

use std::{
  panic::{AssertUnwindSafe, catch_unwind},
  sync::{
    Arc, Weak,
    atomic::{AtomicU32, Ordering},
  },
  time::{Duration, Instant},
};

use log::{error, info, warn};
use st_proto::ipc::{Command, PlayerView};
use tokio::{
  sync::{mpsc, watch},
  task::JoinHandle,
};

use super::{
  Daemon,
  audio::{self, AbortOnDrop},
  machine::{Effect, Event, Input, State, reduce},
};

/// A queue fetch (a radio batch, a playlist page) may take this long.
const LOAD_QUEUE_TIMEOUT: Duration = Duration::from_secs(90);

/// The handle to the player. Cheap to use from anywhere.
pub struct Player {
  input: mpsc::UnboundedSender<Input>,
  view:  watch::Receiver<PlayerView>,
}

impl Player {
  /// Starts the actor. `daemon` is how it reaches YouTube; it is weak because
  /// the daemon owns the player.
  pub fn spawn(daemon: Weak<Daemon>) -> Self {
    let (input, inbox) = mpsc::unbounded_channel();
    let state = State::default();
    let (published, view) = watch::channel(state.view(Instant::now()));
    let actor = Actor { state, daemon, events: input.clone(), published, job: None };
    tokio::spawn(actor.run(inbox));
    Self { input, view }
  }

  pub fn command(&self, command: Command) { let _ = self.input.send(Input::Command(command)); }

  pub fn view(&self) -> PlayerView { self.view.borrow().clone() }

  /// Each receiver sees the latest view whenever it next looks.
  pub fn subscribe(&self) -> watch::Receiver<PlayerView> { self.view.clone() }
}

/// An audio job: the task that plays one track, and the process to signal.
struct Job {
  task: JoinHandle<()>,
  pid:  Arc<AtomicU32>,
}

struct Actor {
  state:     State,
  daemon:    Weak<Daemon>,
  /// Where effects send their events back to.
  events:    mpsc::UnboundedSender<Input>,
  published: watch::Sender<PlayerView>,
  job:       Option<Job>,
}

impl Actor {
  async fn run(mut self, mut inbox: mpsc::UnboundedReceiver<Input>) {
    while let Some(input) = inbox.recv().await {
      let now = Instant::now();
      let before = self.state.rev();
      let effects = catch_unwind(AssertUnwindSafe(|| reduce(&mut self.state, input, now)))
        .unwrap_or_else(|_| {
          error!("the player state machine panicked; resetting the player");
          self.state.reset();
          vec![Effect::StopAudio]
        });
      if self.state.rev() != before {
        self.published.send_replace(self.state.view(now));
      }
      for effect in effects {
        self.run_effect(effect);
      }
    }
  }

  fn run_effect(&mut self, effect: Effect) {
    match effect {
      Effect::StartAudio { generation, video_id } => self.start_audio(generation, video_id),
      Effect::StopAudio => self.stop_audio(),
      Effect::PauseAudio => self.signal(libc::SIGSTOP),
      Effect::ResumeAudio => self.signal(libc::SIGCONT),
      Effect::LoadQueue { load_id, source, token } => {
        let (daemon, events) = (self.daemon.clone(), self.events.clone());
        let more = token.is_some();
        tokio::spawn(async move {
          let result = match daemon.upgrade() {
            Some(daemon) => {
              match tokio::time::timeout(LOAD_QUEUE_TIMEOUT, daemon.load_queue(&source, token))
                .await
              {
                Ok(result) => result.map_err(|error| error.to_string()),
                Err(_) => Err(format!("loading the queue took longer than {LOAD_QUEUE_TIMEOUT:?}")),
              }
            }
            None => Err("the daemon is shutting down".into()),
          };
          let _ = events.send(Input::Event(Event::QueueLoaded { load_id, more, result }));
        });
      }
    }
  }

  fn stop_audio(&mut self) {
    // Aborting the supervising task drops the job guard, which stops the job
    // (and with it ffplay).
    if let Some(job) = self.job.take() {
      job.task.abort();
    }
  }

  fn signal(&self, signal: i32) {
    let Some(job) = &self.job else { return };
    let pid = job.pid.load(Ordering::SeqCst);
    if pid != 0 {
      // SAFETY: a plain syscall; the pid is reset to 0 when the job ends, and a
      // stale one only fails.
      unsafe { libc::kill(pid as i32, signal) };
    }
  }

  fn start_audio(&mut self, generation: u64, video_id: String) {
    self.stop_audio();
    let pid = Arc::new(AtomicU32::new(0));
    let (daemon, events) = (self.daemon.clone(), self.events.clone());
    let job_pid = pid.clone();
    let task = tokio::spawn(async move {
      info!("playing {video_id}");
      // The job runs in its own task so a panic is reported rather than
      // silently ending this supervisor; the guard stops it if the
      // supervisor is aborted.
      let mut job = AbortOnDrop(tokio::spawn({
        let (events, pid) = (events.clone(), job_pid.clone());
        async move {
          let daemon = daemon.upgrade().ok_or("the daemon is shutting down")?;
          let audio = daemon.open_audio(&video_id).await.map_err(|error| error.to_string())?;
          audio::play_stream(audio, |child| {
            pid.store(child.unwrap_or(0), Ordering::SeqCst);
            let _ = events.send(Input::Event(Event::AudioStarted { generation }));
          })
          .await
        }
      }));
      let event = match (&mut job.0).await {
        Ok(Ok(())) => Event::TrackEnded { generation },
        Ok(Err(error)) => {
          warn!("playing failed: {error}");
          Event::AudioFailed { generation, error }
        }
        Err(error) if error.is_cancelled() => return,
        Err(error) => {
          Event::AudioFailed { generation, error: format!("playback crashed: {error}") }
        }
      };
      job_pid.store(0, Ordering::SeqCst);
      let _ = events.send(Input::Event(event));
    });
    self.job = Some(Job { task, pid });
  }
}
