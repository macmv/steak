//! The player state machine: the whole of what is playing, as a pure reducer.
//!
//! ```text
//!                 start_track                  AudioStarted
//!   Idle/Ended ---------------> Loading ---------------------> Playing <--+
//!        ^                         |  ^                           |  |     |
//!        |                         |  | next/prev/jump            |  | Pause/Resume
//!        |      AudioFailed        v  | (from any state)          |  v     |
//!        |   Failed <--------------+--+---------------------------+ Paused-+
//!        |                                                        |
//!        +------ Stop (any state)       TrackEnded: next track, or Ended
//! ```
//!
//! [`reduce`] takes the current state and one [`Input`] (a client's command,
//! or something that happened in the outside world) and returns the
//! [`Effect`]s to carry out. It does no I/O, spawns nothing and reads no
//! clock (the time is an argument), so every rule here is tested below. The
//! actor in `player.rs` is the only thing that owns a [`State`] and the only
//! thing that runs effects, and it feeds what happens back in as [`Event`]s.
//!
//! Rules:
//!
//! - There is at most one audio job. Every job has a generation; an event for
//!   any other generation is stale and ignored, so a job that is aborted or
//!   finishes late can never overwrite what a newer one reported.
//! - Every queue load has a `load_id`, for the same reason: starting a new
//!   radio makes the answer to the old one irrelevant.
//! - `Failed` holds until the next command that starts a track. It never wedges
//!   the machine.
//! - `rev` goes up on every change that clients could see.

use std::time::{Duration, Instant};

use st_proto::{
  catalog::Track,
  ipc::{Command, PlayerView, Source, Transport},
};

/// Start loading more of the queue when this few tracks remain after the
/// current one.
pub const LOAD_MORE_WITHIN: usize = 3;
/// Past this much playback, "previous" restarts the track instead.
pub const RESTART_AFTER: Duration = Duration::from_secs(3);

#[derive(Debug, Default)]
pub struct State {
  queue:        Vec<Track>,
  index:        usize,
  source:       Source,
  /// The token for the next batch of the queue, if there is one.
  continuation: Option<String>,
  loading_more: bool,
  load_id:      u64,
  transport:    Transport,
  generation:   u64,
  /// Pause was asked for while the track was still loading.
  want_paused:  bool,
  /// Playback time before `since`.
  accum:        Duration,
  /// When the clock last started running; `None` while it is stopped.
  since:        Option<Instant>,
  rev:          u64,
}

#[derive(Debug)]
pub enum Input {
  Command(Command),
  Event(Event),
}

/// Things that happened outside the machine.
#[derive(Debug)]
pub enum Event {
  /// The audio job for `generation` is producing sound (or is able to).
  AudioStarted {
    generation: u64,
  },
  /// The audio job ran to the end of the track.
  TrackEnded {
    generation: u64,
  },
  AudioFailed {
    generation: u64,
    error:      String,
  },
  QueueLoaded {
    load_id: u64,
    more:    bool,
    result:  Result<Batch, String>,
  },
}

#[derive(Debug)]
pub struct Batch {
  pub tracks:       Vec<Track>,
  pub continuation: Option<String>,
}

/// Work for the actor to do. Each is fire-and-forget; the outcome comes back
/// as an [`Event`].
#[derive(Debug, PartialEq)]
pub enum Effect {
  /// Stops any audio job, and starts one for this track.
  StartAudio {
    generation: u64,
    video_id:   String,
  },
  StopAudio,
  /// Freeze or unfreeze the running audio job.
  PauseAudio,
  ResumeAudio,
  /// Fetch a batch of the queue: the first (`token: None`) or the next.
  LoadQueue {
    load_id: u64,
    source:  Source,
    token:   Option<String>,
  },
}

impl State {
  pub fn position(&self, now: Instant) -> Duration {
    self.accum + self.since.map_or(Duration::ZERO, |since| now.saturating_duration_since(since))
  }

  pub fn view(&self, now: Instant) -> PlayerView {
    PlayerView {
      rev:          self.rev,
      queue:        self.queue.clone(),
      index:        self.index,
      source:       self.source.clone(),
      transport:    self.transport.clone(),
      position_ms:  self.position(now).as_millis() as u64,
      loading_more: self.loading_more,
    }
  }

  pub fn rev(&self) -> u64 { self.rev }

  #[cfg(test)]
  pub fn transport(&self) -> &Transport { &self.transport }

  #[cfg(test)]
  pub fn generation(&self) -> u64 { self.generation }

  /// Forgets everything but the revision, which only ever goes up. For
  /// recovering from a bug.
  pub fn reset(&mut self) {
    *self = Self { rev: self.rev + 1, generation: self.generation + 1, ..Self::default() };
  }
}

/// Applies `input` to `state` at time `now`.
pub fn reduce(state: &mut State, input: Input, now: Instant) -> Vec<Effect> {
  let mut effects = Vec::new();
  let changed = match input {
    Input::Command(command) => command_input(state, command, now, &mut effects),
    Input::Event(event) => event_input(state, event, now, &mut effects),
  };
  if changed {
    state.rev += 1;
  }
  effects
}

fn command_input(
  state: &mut State,
  command: Command,
  now: Instant,
  effects: &mut Vec<Effect>,
) -> bool {
  match command {
    Command::Play { video_id } => {
      state.reset_queue(Source::None);
      state.queue = vec![stub(video_id)];
      state.start_track(0, effects);
    }
    Command::StartRadio { seed } => {
      state.reset_queue(Source::Radio { seed: seed.video_id.clone() });
      state.queue = vec![seed];
      effects.push(Effect::LoadQueue {
        load_id: state.load_id,
        source:  state.source.clone(),
        token:   None,
      });
      state.start_track(0, effects);
    }
    Command::StartPlaylist { playlist_id } => {
      state.reset_queue(Source::Playlist { playlist_id });
      // Nothing to play until the first batch arrives.
      state.stop_clock();
      state.transport = Transport::Loading;
      state.generation += 1;
      effects.push(Effect::StopAudio);
      effects.push(Effect::LoadQueue {
        load_id: state.load_id,
        source:  state.source.clone(),
        token:   None,
      });
    }
    Command::JumpTo { index, video_id } => {
      if state.queue.get(index).is_none_or(|track| track.video_id != video_id) {
        return false;
      }
      state.start_track(index, effects);
    }
    Command::Next => {
      if state.index + 1 >= state.queue.len() {
        return false;
      }
      state.start_track(state.index + 1, effects);
    }
    Command::Previous => {
      if state.queue.is_empty() {
        return false;
      }
      let target = if state.index == 0 || state.position(now) > RESTART_AFTER {
        state.index
      } else {
        state.index - 1
      };
      state.start_track(target, effects);
    }
    Command::Pause => return state.pause(now, effects),
    Command::Resume => return state.resume(now, effects),
    Command::Toggle => {
      let paused = matches!(state.transport, Transport::Paused)
        || (matches!(state.transport, Transport::Loading) && state.want_paused);
      return if paused { state.resume(now, effects) } else { state.pause(now, effects) };
    }
    Command::Stop => {
      if matches!(state.transport, Transport::Idle) {
        return false;
      }
      state.generation += 1;
      state.stop_clock();
      state.accum = Duration::ZERO;
      state.want_paused = false;
      state.transport = Transport::Idle;
      effects.push(Effect::StopAudio);
    }
  }
  true
}

fn event_input(state: &mut State, event: Event, now: Instant, effects: &mut Vec<Effect>) -> bool {
  match event {
    Event::AudioStarted { generation } => {
      if generation != state.generation || state.transport != Transport::Loading {
        return false;
      }
      if state.want_paused {
        state.transport = Transport::Paused;
        effects.push(Effect::PauseAudio);
      } else {
        state.transport = Transport::Playing;
        state.since = Some(now);
      }
    }
    Event::TrackEnded { generation } => {
      if generation != state.generation {
        return false;
      }
      if state.index + 1 < state.queue.len() {
        state.start_track(state.index + 1, effects);
      } else {
        state.accum = state.position(now);
        state.stop_clock();
        state.transport = Transport::Ended;
      }
    }
    Event::AudioFailed { generation, error } => {
      if generation != state.generation {
        return false;
      }
      state.accum = state.position(now);
      state.stop_clock();
      state.transport = Transport::Failed(error);
    }
    Event::QueueLoaded { load_id, more, result } => {
      if load_id != state.load_id {
        return false;
      }
      state.loading_more = false;
      let batch = match result {
        Ok(batch) => batch,
        Err(error) => {
          log::warn!("loading the queue failed: {error}");
          // Nothing to play after all (a playlist that would not load).
          if state.queue.is_empty() {
            state.transport = Transport::Failed(error);
          }
          return true;
        }
      };
      if more {
        let before = state.queue.len();
        state.append_new(batch.tracks);
        // A batch with nothing new would otherwise be requested again forever.
        state.continuation = batch.continuation.filter(|_| state.queue.len() > before);
        // The queue had run dry while this was loading: carry on into it.
        if state.transport == Transport::Ended && state.index + 1 < state.queue.len() {
          state.start_track(state.index + 1, effects);
        }
      } else if matches!(state.source, Source::Playlist { .. }) {
        state.queue = batch.tracks;
        state.continuation = batch.continuation;
        if state.queue.is_empty() {
          state.transport = Transport::Failed("the playlist has no tracks".into());
        } else {
          state.start_track(0, effects);
        }
      } else {
        // A radio opens with its seed, which is already queued (and playing).
        let seed = state.queue.first().map(|track| track.video_id.clone());
        state.append_new(batch.tracks.into_iter().filter(|t| Some(&t.video_id) != seed.as_ref()));
        state.continuation = batch.continuation;
        state.maybe_load_more(effects);
      }
    }
  }
  true
}

impl State {
  /// Empties the queue for a new source, and invalidates any load in flight.
  fn reset_queue(&mut self, source: Source) {
    self.queue.clear();
    self.index = 0;
    self.source = source;
    self.continuation = None;
    self.loading_more = false;
    self.load_id += 1;
  }

  fn append_new(&mut self, tracks: impl IntoIterator<Item = Track>) {
    for track in tracks {
      if !self.queue.iter().any(|queued| queued.video_id == track.video_id) {
        self.queue.push(track);
      }
    }
  }

  fn stop_clock(&mut self) { self.since = None; }

  /// Makes `index` the current track and starts its audio.
  fn start_track(&mut self, index: usize, effects: &mut Vec<Effect>) {
    self.index = index;
    self.generation += 1;
    self.transport = Transport::Loading;
    self.want_paused = false;
    self.accum = Duration::ZERO;
    self.since = None;
    effects.push(Effect::StartAudio {
      generation: self.generation,
      video_id:   self.queue[index].video_id.clone(),
    });
    self.maybe_load_more(effects);
  }

  fn maybe_load_more(&mut self, effects: &mut Vec<Effect>) {
    if self.loading_more || self.index + LOAD_MORE_WITHIN < self.queue.len() {
      return;
    }
    let Some(token) = self.continuation.take() else { return };
    self.loading_more = true;
    effects.push(Effect::LoadQueue {
      load_id: self.load_id,
      source:  self.source.clone(),
      token:   Some(token),
    });
  }

  fn pause(&mut self, now: Instant, effects: &mut Vec<Effect>) -> bool {
    match self.transport {
      Transport::Playing => {
        self.accum = self.position(now);
        self.stop_clock();
        self.transport = Transport::Paused;
        effects.push(Effect::PauseAudio);
        true
      }
      // Applied when the audio starts.
      Transport::Loading if !self.want_paused => {
        self.want_paused = true;
        true
      }
      _ => false,
    }
  }

  fn resume(&mut self, now: Instant, effects: &mut Vec<Effect>) -> bool {
    match self.transport {
      Transport::Paused => {
        self.since = Some(now);
        self.transport = Transport::Playing;
        effects.push(Effect::ResumeAudio);
        true
      }
      Transport::Loading if self.want_paused => {
        self.want_paused = false;
        true
      }
      _ => false,
    }
  }
}

/// A queue entry for a video the daemon knows only by ID.
fn stub(video_id: String) -> Track {
  Track {
    title: video_id.clone(),
    video_id,
    artists: Vec::new(),
    album: None,
    duration: None,
    thumbnails: Vec::new(),
    watch: None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn track(id: &str) -> Track { stub(id.into()) }

  fn batch(ids: &[&str], continuation: Option<&str>) -> Batch {
    Batch {
      tracks:       ids.iter().map(|id| track(id)).collect(),
      continuation: continuation.map(Into::into),
    }
  }

  struct Rig {
    state: State,
    now:   Instant,
  }

  impl Rig {
    fn new() -> Self { Self { state: State::default(), now: Instant::now() } }

    fn send(&mut self, input: Input) -> Vec<Effect> { reduce(&mut self.state, input, self.now) }

    fn command(&mut self, command: Command) -> Vec<Effect> { self.send(Input::Command(command)) }

    fn event(&mut self, event: Event) -> Vec<Effect> { self.send(Input::Event(event)) }

    fn advance(&mut self, secs: u64) { self.now += Duration::from_secs(secs); }

    fn started(&mut self) -> Vec<Effect> {
      let generation = self.state.generation();
      self.event(Event::AudioStarted { generation })
    }

    fn ids(&self) -> Vec<&str> { self.state.queue.iter().map(|t| t.video_id.as_str()).collect() }

    fn current(&self) -> &str { &self.state.queue[self.state.index].video_id }

    /// A radio with `ids` loaded, playing its first track.
    fn radio(ids: &[&str]) -> Self {
      let mut rig = Self::new();
      rig.command(Command::StartRadio { seed: track(ids[0]) });
      rig.event(Event::QueueLoaded {
        load_id: rig.state.load_id,
        more:    false,
        result:  Ok(batch(ids, None)),
      });
      rig.started();
      rig
    }
  }

  #[test]
  fn play_loads_then_plays_then_ends() {
    let mut rig = Rig::new();
    let effects = rig.command(Command::Play { video_id: "a".into() });
    assert_eq!(effects, vec![Effect::StartAudio { generation: 1, video_id: "a".into() }]);
    assert_eq!(*rig.state.transport(), Transport::Loading);

    rig.started();
    assert_eq!(*rig.state.transport(), Transport::Playing);
    rig.advance(10);
    assert_eq!(rig.state.position(rig.now), Duration::from_secs(10));

    rig.event(Event::TrackEnded { generation: 1 });
    assert_eq!(*rig.state.transport(), Transport::Ended);
    // The clock stops at the end of the track.
    rig.advance(10);
    assert_eq!(rig.state.position(rig.now), Duration::from_secs(10));
  }

  #[test]
  fn stale_events_change_nothing() {
    let mut rig = Rig::new();
    rig.command(Command::Play { video_id: "a".into() });
    rig.command(Command::Play { video_id: "b".into() });
    let rev = rig.state.rev;
    // Generation 1 was replaced by 2.
    assert!(rig.event(Event::AudioStarted { generation: 1 }).is_empty());
    rig.event(Event::AudioFailed { generation: 1, error: "late".into() });
    rig.event(Event::TrackEnded { generation: 1 });
    assert_eq!(*rig.state.transport(), Transport::Loading);
    assert_eq!(rig.state.rev, rev);
  }

  #[test]
  fn failure_is_not_terminal() {
    let mut rig = Rig::new();
    rig.command(Command::Play { video_id: "a".into() });
    rig.event(Event::AudioFailed { generation: 1, error: "no audio".into() });
    assert_eq!(*rig.state.transport(), Transport::Failed("no audio".into()));
    rig.command(Command::Play { video_id: "b".into() });
    assert_eq!(*rig.state.transport(), Transport::Loading);
  }

  #[test]
  fn pause_and_resume_stop_and_restart_the_clock() {
    let mut rig = Rig::radio(&["a", "b"]);
    rig.advance(5);
    assert_eq!(rig.command(Command::Pause), vec![Effect::PauseAudio]);
    rig.advance(100);
    assert_eq!(rig.state.position(rig.now), Duration::from_secs(5));
    assert_eq!(rig.command(Command::Resume), vec![Effect::ResumeAudio]);
    rig.advance(2);
    assert_eq!(rig.state.position(rig.now), Duration::from_secs(7));
    // Pausing twice, or resuming a track that is playing, does nothing.
    rig.command(Command::Pause);
    let rev = rig.state.rev;
    assert!(rig.command(Command::Pause).is_empty());
    assert_eq!(rig.state.rev, rev);
  }

  #[test]
  fn pause_while_loading_applies_once_audio_starts() {
    let mut rig = Rig::new();
    rig.command(Command::Play { video_id: "a".into() });
    assert!(rig.command(Command::Toggle).is_empty());
    assert_eq!(rig.started(), vec![Effect::PauseAudio]);
    assert_eq!(*rig.state.transport(), Transport::Paused);
    rig.advance(5);
    assert_eq!(rig.state.position(rig.now), Duration::ZERO);
    rig.command(Command::Toggle);
    assert_eq!(*rig.state.transport(), Transport::Playing);
  }

  #[test]
  fn toggling_back_while_loading_cancels_the_pause() {
    let mut rig = Rig::new();
    rig.command(Command::Play { video_id: "a".into() });
    rig.command(Command::Toggle);
    rig.command(Command::Toggle);
    assert!(rig.started().is_empty());
    assert_eq!(*rig.state.transport(), Transport::Playing);
  }

  #[test]
  fn radio_fills_in_behind_the_seed_without_duplicates() {
    let mut rig = Rig::new();
    let effects = rig.command(Command::StartRadio { seed: track("a") });
    assert_eq!(effects.len(), 2);
    assert!(matches!(effects[0], Effect::LoadQueue { token: None, .. }));
    assert!(matches!(effects[1], Effect::StartAudio { .. }));

    rig.event(Event::QueueLoaded {
      load_id: rig.state.load_id,
      more:    false,
      result:  Ok(batch(&["a", "b", "c", "d", "e", "b"], Some("t1"))),
    });
    assert_eq!(rig.ids(), ["a", "b", "c", "d", "e"]);
    assert_eq!(rig.state.continuation.as_deref(), Some("t1"));
  }

  #[test]
  fn nearing_the_end_loads_more_once() {
    let mut rig = Rig::new();
    rig.command(Command::StartRadio { seed: track("a") });
    let load_id = rig.state.load_id;
    let effects = rig.event(Event::QueueLoaded {
      load_id,
      more: false,
      result: Ok(batch(&["a", "b", "c"], Some("t1"))),
    });
    // Only two tracks remain, so the next batch is requested right away.
    assert_eq!(
      effects,
      vec![Effect::LoadQueue {
        load_id,
        source: Source::Radio { seed: "a".into() },
        token: Some("t1".into())
      }]
    );
    assert!(rig.state.loading_more);
    // Skipping around while it loads does not ask again.
    assert!(rig.command(Command::Next).iter().all(|e| !matches!(e, Effect::LoadQueue { .. })));

    rig.event(Event::QueueLoaded {
      load_id,
      more: true,
      result: Ok(batch(&["c", "d", "e", "f", "g"], Some("t2"))),
    });
    assert_eq!(rig.ids(), ["a", "b", "c", "d", "e", "f", "g"]);
    assert!(!rig.state.loading_more);
  }

  #[test]
  fn a_batch_with_nothing_new_ends_the_loading() {
    let mut rig = Rig::radio(&["a", "b"]);
    rig.state.continuation = Some("t".into());
    rig.state.loading_more = true;
    rig.event(Event::QueueLoaded {
      load_id: rig.state.load_id,
      more:    true,
      result:  Ok(batch(&["a", "b"], Some("t-again"))),
    });
    assert_eq!(rig.state.continuation, None);
  }

  #[test]
  fn a_new_radio_discards_the_old_ones_answer() {
    let mut rig = Rig::new();
    rig.command(Command::StartRadio { seed: track("a") });
    let old = rig.state.load_id;
    rig.command(Command::StartRadio { seed: track("x") });
    assert!(
      rig
        .event(Event::QueueLoaded {
          load_id: old,
          more:    false,
          result:  Ok(batch(&["a", "b"], None)),
        })
        .is_empty()
    );
    assert_eq!(rig.ids(), ["x"]);
  }

  #[test]
  fn running_dry_continues_into_the_batch_that_arrives() {
    let mut rig = Rig::radio(&["a"]);
    rig.state.continuation = None;
    rig.state.loading_more = true;
    let generation = rig.state.generation();
    rig.event(Event::TrackEnded { generation });
    assert_eq!(*rig.state.transport(), Transport::Ended);
    let effects = rig.event(Event::QueueLoaded {
      load_id: rig.state.load_id,
      more:    true,
      result:  Ok(batch(&["b", "c", "d", "e"], None)),
    });
    assert_eq!(
      effects,
      vec![Effect::StartAudio { generation: generation + 1, video_id: "b".into() }]
    );
  }

  #[test]
  fn tracks_advance_automatically() {
    let mut rig = Rig::radio(&["a", "b", "c", "d", "e"]);
    let generation = rig.state.generation();
    let effects = rig.event(Event::TrackEnded { generation });
    assert_eq!(
      effects,
      vec![Effect::StartAudio { generation: generation + 1, video_id: "b".into() }]
    );
    assert_eq!(rig.state.position(rig.now), Duration::ZERO);
  }

  #[test]
  fn next_stops_at_the_end_of_the_queue() {
    let mut rig = Rig::radio(&["a", "b"]);
    rig.command(Command::Next);
    assert_eq!(rig.current(), "b");
    let rev = rig.state.rev;
    assert!(rig.command(Command::Next).is_empty());
    assert_eq!(rig.state.rev, rev);
  }

  #[test]
  fn previous_restarts_late_and_goes_back_early() {
    let mut rig = Rig::radio(&["a", "b", "c"]);
    rig.command(Command::Next);
    rig.started();
    rig.advance(10);
    rig.command(Command::Previous);
    assert_eq!(rig.current(), "b", "restarts after a few seconds");
    rig.started();
    rig.advance(1);
    rig.command(Command::Previous);
    assert_eq!(rig.current(), "a", "goes back when just started");
    rig.started();
    rig.command(Command::Previous);
    assert_eq!(rig.current(), "a", "stays on the first track");
  }

  #[test]
  fn jump_checks_that_the_client_saw_the_same_queue() {
    let mut rig = Rig::radio(&["a", "b", "c"]);
    assert!(rig.command(Command::JumpTo { index: 2, video_id: "b".into() }).is_empty());
    assert!(rig.command(Command::JumpTo { index: 9, video_id: "b".into() }).is_empty());
    rig.command(Command::JumpTo { index: 2, video_id: "c".into() });
    assert_eq!(rig.current(), "c");
  }

  #[test]
  fn stop_keeps_the_queue_and_invalidates_the_job() {
    let mut rig = Rig::radio(&["a", "b"]);
    let generation = rig.state.generation();
    assert_eq!(rig.command(Command::Stop), vec![Effect::StopAudio]);
    assert_eq!(*rig.state.transport(), Transport::Idle);
    assert_eq!(rig.ids(), ["a", "b"]);
    // The aborted job's last words are ignored.
    assert!(rig.event(Event::TrackEnded { generation }).is_empty());
    assert_eq!(*rig.state.transport(), Transport::Idle);
    assert!(rig.command(Command::Stop).is_empty());
  }

  #[test]
  fn a_playlist_plays_once_its_first_batch_arrives() {
    let mut rig = Rig::new();
    let effects = rig.command(Command::StartPlaylist { playlist_id: "PL1".into() });
    assert!(matches!(effects[..], [Effect::StopAudio, Effect::LoadQueue { token: None, .. }]));
    assert_eq!(*rig.state.transport(), Transport::Loading);
    let effects = rig.event(Event::QueueLoaded {
      load_id: rig.state.load_id,
      more:    false,
      result:  Ok(batch(&["a", "b"], None)),
    });
    assert!(matches!(&effects[..], [Effect::StartAudio { video_id, .. }] if video_id == "a"));
    assert_eq!(rig.ids(), ["a", "b"]);
  }

  #[test]
  fn a_playlist_that_fails_to_load_fails() {
    let mut rig = Rig::new();
    rig.command(Command::StartPlaylist { playlist_id: "PL1".into() });
    rig.event(Event::QueueLoaded {
      load_id: rig.state.load_id,
      more:    false,
      result:  Err("nope".into()),
    });
    assert_eq!(*rig.state.transport(), Transport::Failed("nope".into()));
  }

  #[test]
  fn a_failed_radio_load_does_not_stop_the_seed() {
    let mut rig = Rig::new();
    rig.command(Command::StartRadio { seed: track("a") });
    rig.started();
    rig.event(Event::QueueLoaded {
      load_id: rig.state.load_id,
      more:    false,
      result:  Err("offline".into()),
    });
    assert_eq!(*rig.state.transport(), Transport::Playing);
  }

  #[test]
  fn views_carry_the_position_and_a_rising_rev() {
    let mut rig = Rig::radio(&["a"]);
    rig.advance(4);
    let first = rig.state.view(rig.now);
    assert_eq!(first.position_ms, 4000);
    assert_eq!(first.transport, Transport::Playing);
    rig.command(Command::Pause);
    assert!(rig.state.view(rig.now).rev > first.rev);
  }

  #[test]
  fn reset_recovers_to_idle_and_keeps_the_rev_rising() {
    let mut rig = Rig::radio(&["a", "b"]);
    let rev = rig.state.rev;
    let generation = rig.state.generation();
    rig.state.reset();
    assert_eq!(*rig.state.transport(), Transport::Idle);
    assert!(rig.state.rev > rev);
    assert!(rig.state.generation() > generation);
  }
}
