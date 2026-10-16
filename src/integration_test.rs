//! The daemon and its clients, end to end and in one process.
//!
//! A real [`Daemon`] serves a real Unix socket, and the real [`client`] talks
//! to it. The only fake is the [`Backend`]: [`Fake`] stands in for YouTube, the
//! audio device and ffmpeg, so nothing here touches the network, a
//! session, or a sound card. Each test gets its own daemon and socket, so they
//! run in parallel.
//!
//! Audio is a channel the test holds the other end of: a track "plays" until
//! the test calls [`Fake::finish`] (it ends) or [`Fake::fail`] (the download
//! breaks), which is how tests decide when a track ends.

use std::{
  collections::HashMap,
  path::{Path, PathBuf},
  sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
  },
  time::Duration,
};

use bytes::Bytes;
use futures_util::StreamExt;
use st_api::playback::stats::{Ping, PingKind, PlaybackTracking, State};
use st_proto::{
  catalog::{Shelf, ShelfItem, Track},
  ipc::{CellReport, Command, PlayerView, Request, Response, SessionReport, Source, Transport},
};
use tokio::{
  io::{AsyncReadExt, AsyncWriteExt},
  net::UnixStream,
  sync::mpsc,
  task::JoinHandle,
};

use crate::{
  client::{self, BoxError as ClientError, Subscription},
  daemon::{
    Daemon,
    api::{AudioChunks, Backend, BoxError, Fut, OnStarted, Opened},
    machine::Batch,
    server,
  },
};

/// How long a test waits for the daemon to get somewhere before it fails.
const PATIENCE: Duration = Duration::from_secs(5);

fn track(id: &str) -> Track {
  Track {
    video_id:   id.into(),
    title:      format!("Song {id}"),
    artists:    Vec::new(),
    album:      None,
    duration:   None,
    thumbnails: Vec::new(),
    watch:      None,
  }
}

type Chunk = Result<Bytes, String>;

/// A backend with canned answers.
///
/// - A radio from seed `s` is `s`, then `s1`, `s2`, and (on its second page)
///   `s3`, `s4`.
/// - The playlist `pl` is `p1`, `p2`, `p3`; any other playlist does not exist.
/// - The video `broken` cannot be opened.
#[derive(Default)]
struct Fake {
  /// The sending end of each open audio stream, by video ID.
  streams: Mutex<HashMap<String, mpsc::UnboundedSender<Chunk>>>,
  /// Every video ID that was opened, in order.
  opened:  Mutex<Vec<String>>,
  /// Every stats ping the daemon reported, in order.
  pings:   Mutex<Vec<Ping>>,
}

impl Fake {
  /// The audio of `id` ends, as if the whole track downloaded.
  fn finish(&self, id: &str) {
    self.streams.lock().unwrap().remove(id).expect("that track is not playing");
  }

  /// The audio of `id` breaks off.
  fn fail(&self, id: &str, error: &str) {
    let stream = self.streams.lock().unwrap().remove(id).expect("that track is not playing");
    stream.send(Err(error.into())).unwrap();
  }

  /// Whether nothing is listening to the audio of `id` any more (it is
  /// stopped, or never started).
  fn is_closed(&self, id: &str) -> bool {
    self.streams.lock().unwrap().get(id).is_none_or(|stream| stream.is_closed())
  }

  fn opened(&self) -> Vec<String> { self.opened.lock().unwrap().clone() }

  fn pings(&self) -> Vec<Ping> { self.pings.lock().unwrap().clone() }

  /// What a player response's tracking block holds, for any video.
  fn tracking(video_id: &str) -> PlaybackTracking {
    let base = |name: &str| {
      format!(
        "https://s.youtube.com/api/stats/{name}?docid={video_id}&ei=e&ns=yt&el=detailpage&len=183"
      )
    };
    let response = serde_json::json!({"playbackTracking": {
      "videostatsPlaybackUrl": {"baseUrl": base("playback")},
      "videostatsWatchtimeUrl": {"baseUrl": base("watchtime")},
      "videostatsScheduledFlushWalltimeSeconds": [10, 20],
      "videostatsDefaultFlushIntervalSeconds": 40,
    }});
    PlaybackTracking::from_player(&response, 140).unwrap()
  }

  fn radio_page(seed: &str, token: Option<&str>) -> Batch {
    match token {
      None => Batch {
        tracks:       [seed.to_owned(), format!("{seed}1"), format!("{seed}2")]
          .iter()
          .map(|id| track(id))
          .collect(),
        continuation: Some("page2".into()),
      },
      Some(_) => Batch {
        tracks:       [format!("{seed}3"), format!("{seed}4")].iter().map(|id| track(id)).collect(),
        continuation: None,
      },
    }
  }

  fn playlist_tracks(id: &str) -> Result<Vec<Track>, BoxError> {
    match id {
      "pl" => Ok(["p1", "p2", "p3"].iter().map(|id| track(id)).collect()),
      other => Err(format!("no tracks found for playlist {other}").into()),
    }
  }

  fn session(source: Option<&str>) -> SessionReport {
    SessionReport {
      state:                 "valid".into(),
      detail:                None,
      source:                source.map(Into::into),
      last_import_secs:      Some(1),
      next_auto_repair_secs: None,
    }
  }
}

impl Backend for Fake {
  fn home(&self, more: u32) -> Fut<'_, Result<(Vec<Shelf>, bool), BoxError>> {
    Box::pin(async move {
      let shelf = |title: String| Shelf {
        title,
        items: vec![ShelfItem::Track(track("a")), ShelfItem::Track(track("b"))],
      };
      let shelves = (0..=more)
        .map(|page| shelf(if page == 0 { "Quick picks".into() } else { format!("More {page}") }))
        .collect();
      Ok((shelves, true))
    })
  }

  fn radio<'a>(&'a self, video_id: &'a str, _more: u32) -> Fut<'a, Result<Vec<Track>, BoxError>> {
    Box::pin(async move { Ok(Self::radio_page(video_id, None).tracks) })
  }

  fn playlist<'a>(
    &'a self,
    playlist_id: &'a str,
    _more: u32,
  ) -> Fut<'a, Result<Vec<Track>, BoxError>> {
    Box::pin(async move { Self::playlist_tracks(playlist_id) })
  }

  fn load_queue<'a>(
    &'a self,
    source: &'a Source,
    token: Option<String>,
  ) -> Fut<'a, Result<Batch, BoxError>> {
    Box::pin(async move {
      match source {
        Source::None => Err("this queue does not grow".into()),
        Source::Radio { seed } => Ok(Self::radio_page(seed, token.as_deref())),
        Source::Playlist { playlist_id } => {
          Ok(Batch { tracks: Self::playlist_tracks(playlist_id)?, continuation: None })
        }
      }
    })
  }

  fn open_audio<'a>(&'a self, video_id: &'a str) -> Fut<'a, Result<Opened, BoxError>> {
    Box::pin(async move {
      if video_id == "broken" {
        return Err("this video is unavailable".into());
      }
      let (sender, receiver) = mpsc::unbounded_channel();
      sender.send(Ok(Bytes::from_static(b"audio"))).unwrap();
      self.streams.lock().unwrap().insert(video_id.to_owned(), sender);
      self.opened.lock().unwrap().push(video_id.to_owned());
      let stream = futures_util::stream::unfold(receiver, |mut receiver| async {
        receiver.recv().await.map(|chunk| (chunk, receiver))
      });
      Ok(Opened {
        chunks:   Box::pin(stream) as AudioChunks,
        tracking: Some(Self::tracking(video_id)),
      })
    })
  }

  fn report<'a>(&'a self, _: &'a PlaybackTracking, ping: Ping) -> Fut<'a, Result<(), BoxError>> {
    Box::pin(async move {
      self.pings.lock().unwrap().push(ping);
      Ok(())
    })
  }

  fn art<'a>(&'a self, url: &'a str) -> Fut<'a, Result<Vec<u8>, BoxError>> {
    Box::pin(async move {
      match url.contains("missing") {
        true => Err(format!("{url} is not a YouTube image").into()),
        false => Ok(format!("image of {url}").into_bytes()),
      }
    })
  }

  /// "Plays" the audio by reading it all, and is audible straight away.
  fn play(&self, mut audio: AudioChunks, on_started: OnStarted) -> Fut<'_, Result<(), String>> {
    Box::pin(async move {
      on_started(None);
      while let Some(chunk) = audio.next().await {
        chunk.map_err(|error| format!("the audio download failed: {error}"))?;
      }
      Ok(())
    })
  }

  /// Saves the audio as it arrived, with no transcoding.
  fn save<'a>(&'a self, mut audio: AudioChunks, output: &'a Path) -> Fut<'a, Result<(), String>> {
    Box::pin(async move {
      let mut bytes = Vec::new();
      while let Some(chunk) = audio.next().await {
        bytes.extend_from_slice(&chunk?);
      }
      std::fs::write(output, bytes).map_err(|error| error.to_string())
    })
  }

  fn session_report(&self) -> SessionReport { Self::session(None) }

  fn import_session(&self, profile: Option<PathBuf>) -> Fut<'_, Result<SessionReport, String>> {
    Box::pin(async move {
      let profile = profile.ok_or("no Firefox profile found; pass the profile directory")?;
      Ok(Self::session(Some(&profile.display().to_string())))
    })
  }

  fn repair_session(&self) -> Fut<'_, SessionReport> { Box::pin(async { Self::session(None) }) }

  fn cache_reports(&self) -> Vec<CellReport> {
    vec![CellReport {
      name:                 "bootstrap".into(),
      state:                "ready".into(),
      generation:           Some(1),
      age_secs:             Some(0),
      consecutive_failures: 0,
      last_error:           None,
      retry_in_secs:        None,
    }]
  }

  fn maintain(&self) {}
}

/// A running daemon on its own socket, and the fake behind it.
struct Rig {
  socket: PathBuf,
  fake:   Arc<Fake>,
  server: JoinHandle<()>,
}

impl Rig {
  async fn start() -> Self {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
      "steak-it-{}-{}",
      std::process::id(),
      NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let socket = dir.join("steak.sock");
    let listener = server::bind(&socket).await.unwrap();
    let fake = Arc::new(Fake::default());
    let daemon = Daemon::new(fake.clone());
    let server = tokio::spawn(server::serve(listener, daemon, std::future::pending()));
    Self { socket, fake, server }
  }

  async fn call(&self, request: Request) -> Result<Response, ClientError> {
    client::call_at(&self.socket, request).await
  }

  async fn command(&self, command: Command) {
    assert!(matches!(self.call(Request::Control(command)).await, Ok(Response::Ok)));
  }

  async fn subscribe(&self) -> Watch { Watch(client::subscribe_at(&self.socket).await.unwrap()) }

  /// Starts a radio from `seed`, and returns once it has loaded both of its
  /// pages and is playing the seed. Also returns the view it settled on.
  async fn radio(&self, seed: &str) -> (Watch, PlayerView) {
    let mut watch = self.subscribe().await;
    self.command(Command::StartRadio { seed: track(seed) }).await;
    let view = watch
      .until(|view| {
        view.transport == Transport::Playing
          && view.current().is_some_and(|t| t.video_id == seed)
          && view.queue.len() == 5
          && !view.loading_more
      })
      .await;
    assert_eq!(view.source, Source::Radio { seed: seed.into() });
    (watch, view)
  }
}

impl Drop for Rig {
  fn drop(&mut self) {
    self.server.abort();
    if let Some(dir) = self.socket.parent() {
      let _ = std::fs::remove_dir_all(dir);
    }
  }
}

/// A subscription that can wait for the player to reach a state.
struct Watch(Subscription);

impl Watch {
  /// Reads views until one satisfies `done`, and returns it. Fails the test if
  /// that takes too long, showing the last view.
  async fn until(&mut self, done: impl Fn(&PlayerView) -> bool) -> PlayerView {
    let mut last = None;
    let result = tokio::time::timeout(PATIENCE, async {
      loop {
        let view = self.0.next().await.expect("the daemon hung up");
        if done(&view) {
          return view;
        }
        last = Some(view);
      }
    })
    .await;
    result.unwrap_or_else(|_| panic!("the player never got there; it was last at {last:#?}"))
  }

  /// Waits until `id` is the current track and is playing.
  async fn playing(&mut self, id: &str) -> PlayerView {
    self
      .until(|view| {
        view.transport == Transport::Playing && view.current().is_some_and(|t| t.video_id == id)
      })
      .await
  }
}

fn ids(view: &PlayerView) -> Vec<&str> { view.queue.iter().map(|t| t.video_id.as_str()).collect() }

#[tokio::test]
async fn ping_and_status() {
  let rig = Rig::start().await;

  assert!(matches!(rig.call(Request::Ping).await, Ok(Response::Pong)));

  let Ok(Response::Status(status)) = rig.call(Request::Status).await else { panic!() };
  assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
  assert_eq!(status.session.state, "valid");
  assert_eq!(status.caches[0].name, "bootstrap");
  assert_eq!(status.player, PlayerView::default());
}

#[tokio::test]
async fn catalog_requests_are_answered_by_the_backend() {
  let rig = Rig::start().await;

  let Ok(Response::Home { shelves, logged_in }) = rig.call(Request::Home { more: 1 }).await else {
    panic!()
  };
  assert!(logged_in);
  assert_eq!(
    shelves.iter().map(|shelf| shelf.title.as_str()).collect::<Vec<_>>(),
    ["Quick picks", "More 1"]
  );

  let Ok(Response::Tracks(tracks)) =
    rig.call(Request::Radio { video_id: "r".into(), more: 0 }).await
  else {
    panic!()
  };
  assert_eq!(tracks.iter().map(|t| t.video_id.as_str()).collect::<Vec<_>>(), ["r", "r1", "r2"]);

  let Ok(Response::Tracks(tracks)) =
    rig.call(Request::Playlist { playlist_id: "pl".into(), more: 0 }).await
  else {
    panic!()
  };
  assert_eq!(tracks.len(), 3);

  let Ok(Response::Art(image)) =
    rig.call(Request::Art { url: "https://i.ytimg.com/x".into() }).await
  else {
    panic!()
  };
  assert_eq!(image, b"image of https://i.ytimg.com/x");
}

#[tokio::test]
async fn backend_errors_reach_the_client_as_errors() {
  let rig = Rig::start().await;

  let error = rig.call(Request::Playlist { playlist_id: "nope".into(), more: 0 }).await;
  assert_eq!(error.unwrap_err().to_string(), "no tracks found for playlist nope");

  let error = rig.call(Request::Art { url: "https://missing".into() }).await;
  assert!(error.unwrap_err().to_string().contains("is not a YouTube image"));

  // The daemon carries on.
  assert!(matches!(rig.call(Request::Ping).await, Ok(Response::Pong)));
}

#[tokio::test]
async fn a_new_subscriber_starts_with_the_current_state() {
  let rig = Rig::start().await;
  let mut watch = rig.subscribe().await;
  assert_eq!(watch.0.next().await.unwrap(), PlayerView::default());

  // And one that arrives late is not left with the old state.
  rig.command(Command::Play { video_id: "a".into() }).await;
  watch.playing("a").await;
  let mut late = rig.subscribe().await;
  assert_eq!(ids(&late.0.next().await.unwrap()), ["a"]);
}

#[tokio::test]
async fn a_track_loads_plays_and_ends() {
  let rig = Rig::start().await;
  let mut watch = rig.subscribe().await;

  rig.command(Command::Play { video_id: "a".into() }).await;
  let view = watch.playing("a").await;
  assert_eq!(ids(&view), ["a"]);
  assert_eq!(view.source, Source::None);
  assert_eq!(rig.fake.opened(), ["a"]);

  rig.fake.finish("a");
  watch.until(|view| view.transport == Transport::Ended).await;
}

#[tokio::test]
async fn a_radio_fills_the_queue_and_plays_through_it() {
  let rig = Rig::start().await;
  let (mut watch, view) = rig.radio("r").await;

  // Both pages arrived, and the seed was not queued twice.
  assert_eq!(ids(&view), ["r", "r1", "r2", "r3", "r4"]);

  // A finished track moves on to the next one.
  rig.fake.finish("r");
  watch.playing("r1").await;
  assert_eq!(rig.fake.opened(), ["r", "r1"]);

  rig.command(Command::Next).await;
  watch.playing("r2").await;

  // Previous goes back a track, since this one has only just started.
  rig.command(Command::Previous).await;
  watch.playing("r1").await;

  // A jump with a stale idea of the queue does nothing: the next track is
  // still r2, not r1.
  rig.command(Command::JumpTo { index: 0, video_id: "not-r".into() }).await;
  rig.command(Command::Next).await;
  watch.playing("r2").await;

  rig.command(Command::JumpTo { index: 4, video_id: "r4".into() }).await;
  watch.playing("r4").await;

  // The last track ending ends the player, and keeps the queue.
  rig.fake.finish("r4");
  let view = watch.until(|view| view.transport == Transport::Ended).await;
  assert_eq!(ids(&view), ["r", "r1", "r2", "r3", "r4"]);
}

#[tokio::test]
async fn a_playlist_plays_from_its_first_track() {
  let rig = Rig::start().await;
  let mut watch = rig.subscribe().await;

  rig.command(Command::StartPlaylist { playlist_id: "pl".into() }).await;
  let view = watch.playing("p1").await;
  assert_eq!(ids(&view), ["p1", "p2", "p3"]);
  assert_eq!(view.source, Source::Playlist { playlist_id: "pl".into() });
}

#[tokio::test]
async fn a_playlist_that_does_not_exist_fails_the_player() {
  let rig = Rig::start().await;
  let mut watch = rig.subscribe().await;

  rig.command(Command::StartPlaylist { playlist_id: "nope".into() }).await;
  let view = watch.until(|view| matches!(view.transport, Transport::Failed(_))).await;
  assert_eq!(view.transport, Transport::Failed("no tracks found for playlist nope".into()));

  // Failing is not terminal.
  rig.command(Command::Play { video_id: "a".into() }).await;
  watch.playing("a").await;
}

#[tokio::test]
async fn pause_and_resume() {
  let rig = Rig::start().await;
  let mut watch = rig.subscribe().await;
  rig.command(Command::Play { video_id: "a".into() }).await;
  watch.playing("a").await;

  rig.command(Command::Pause).await;
  let paused = watch.until(|view| view.transport == Transport::Paused).await;

  // The clock is stopped while paused.
  tokio::time::sleep(Duration::from_millis(100)).await;
  assert_eq!(
    rig
      .call(Request::Status)
      .await
      .map(|r| match r {
        Response::Status(status) => status.player.position_ms,
        other => panic!("{other:?}"),
      })
      .unwrap(),
    paused.position_ms
  );

  rig.command(Command::Toggle).await;
  watch.playing("a").await;
  rig.command(Command::Toggle).await;
  watch.until(|view| view.transport == Transport::Paused).await;
  rig.command(Command::Resume).await;
  watch.playing("a").await;
}

/// Waits until the daemon has reported `count` pings, and returns them.
async fn pings(rig: &Rig, count: usize) -> Vec<Ping> {
  tokio::time::timeout(PATIENCE, async {
    loop {
      let pings = rig.fake.pings();
      if pings.len() >= count {
        return pings;
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .unwrap_or_else(|_| panic!("expected {count} pings, got {:?}", rig.fake.pings()))
}

#[tokio::test]
async fn a_playback_is_reported_as_it_happens() {
  let rig = Rig::start().await;
  let mut watch = rig.subscribe().await;
  rig.command(Command::Play { video_id: "a".into() }).await;
  watch.playing("a").await;
  assert_eq!(pings(&rig, 1).await[0].kind, PingKind::Playback);

  tokio::time::sleep(Duration::from_millis(50)).await;
  rig.command(Command::Pause).await;
  let paused = &pings(&rig, 2).await[1];
  assert_eq!(paused.kind, PingKind::Watchtime);
  assert_eq!(paused.state, State::Paused);
  assert!(!paused.is_final);
  assert_eq!(paused.segments.len(), 1);

  rig.command(Command::Resume).await;
  watch.playing("a").await;
  rig.command(Command::Stop).await;
  let all = pings(&rig, 3).await;
  assert_eq!(all.len(), 3);
  assert!(all[2].is_final);
  assert_eq!(all[2].kind, PingKind::Watchtime);
}

#[tokio::test]
async fn a_track_left_before_it_played_is_not_reported() {
  let rig = Rig::start().await;
  let mut watch = rig.subscribe().await;
  rig.command(Command::Play { video_id: "broken".into() }).await;
  watch.until(|view| matches!(view.transport, Transport::Failed(_))).await;
  tokio::time::sleep(Duration::from_millis(100)).await;
  assert!(rig.fake.pings().is_empty());
}

#[tokio::test]
async fn stop_ends_the_audio_and_keeps_the_queue() {
  let rig = Rig::start().await;
  let (mut watch, _) = rig.radio("r").await;
  assert!(!rig.fake.is_closed("r"));

  rig.command(Command::Stop).await;
  let view = watch.until(|view| view.transport == Transport::Idle).await;
  assert_eq!(view.queue.len(), 5);

  // The audio job was cancelled, so nothing is still reading the stream.
  tokio::time::timeout(PATIENCE, async {
    while !rig.fake.is_closed("r") {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .expect("the audio of a stopped track is still being played");
}

#[tokio::test]
async fn audio_that_cannot_be_opened_or_breaks_fails_the_player_not_the_daemon() {
  let rig = Rig::start().await;
  let mut watch = rig.subscribe().await;

  rig.command(Command::Play { video_id: "broken".into() }).await;
  let view = watch.until(|view| matches!(view.transport, Transport::Failed(_))).await;
  assert_eq!(view.transport, Transport::Failed("this video is unavailable".into()));

  rig.command(Command::Play { video_id: "a".into() }).await;
  watch.playing("a").await;
  rig.fake.fail("a", "connection reset");
  let view = watch.until(|view| matches!(view.transport, Transport::Failed(_))).await;
  assert_eq!(
    view.transport,
    Transport::Failed("the audio download failed: connection reset".into())
  );

  rig.command(Command::Play { video_id: "b".into() }).await;
  watch.playing("b").await;
}

#[tokio::test]
async fn every_client_sees_the_same_player() {
  let rig = Rig::start().await;
  let mut tui = rig.subscribe().await;
  let mut other = rig.subscribe().await;
  let gone = rig.subscribe().await;
  drop(gone);

  // One client's command is another's news.
  rig.command(Command::StartRadio { seed: track("r") }).await;
  let (a, b) = (
    tui.until(|view| view.queue.len() == 5 && view.transport == Transport::Playing).await,
    other.until(|view| view.queue.len() == 5 && view.transport == Transport::Playing).await,
  );
  assert_eq!((a.queue, a.index, a.source), (b.queue, b.index, b.source));

  // Clients racing are applied one after another: whatever the order, the
  // player ends up on one of the tracks, and both clients agree on which.
  tokio::join!(
    rig.command(Command::JumpTo { index: 2, video_id: "r2".into() }),
    rig.command(Command::JumpTo { index: 3, video_id: "r3".into() })
  );
  let settled =
    |view: &PlayerView| view.transport == Transport::Playing && matches!(view.index, 2 | 3);
  let (a, b) = (tui.until(settled).await, other.until(settled).await);
  assert_eq!(a.current(), b.current());
}

#[tokio::test]
async fn a_download_is_saved_through_the_backend() {
  let rig = Rig::start().await;
  let output = rig.socket.with_file_name("a.mp4");

  let download = {
    let (socket, output) = (rig.socket.clone(), output.clone());
    tokio::spawn(async move {
      client::call_at(&socket, Request::Download { video_id: "a".into(), output }).await
    })
  };
  // The download is still running while the audio is.
  tokio::time::timeout(PATIENCE, async {
    while rig.fake.opened().is_empty() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .unwrap();
  assert!(!download.is_finished());

  rig.fake.finish("a");
  assert!(matches!(download.await.unwrap(), Ok(Response::Ok)));
  assert_eq!(std::fs::read(&output).unwrap(), b"audio");
}

#[tokio::test]
async fn auth_requests_are_answered_by_the_backend() {
  let rig = Rig::start().await;

  let Ok(Response::Session(report)) =
    rig.call(Request::AuthImport { profile: Some("/some/profile".into()) }).await
  else {
    panic!()
  };
  assert_eq!(report.source.as_deref(), Some("/some/profile"));

  let error = rig.call(Request::AuthImport { profile: None }).await;
  assert!(error.unwrap_err().to_string().contains("no Firefox profile found"));

  assert!(matches!(rig.call(Request::AuthRepair).await, Ok(Response::Session(_))));
}

#[tokio::test]
async fn a_malformed_request_costs_one_connection() {
  let rig = Rig::start().await;

  let mut stream = UnixStream::connect(&rig.socket).await.unwrap();
  stream.write_all(b"this is not json\n").await.unwrap();
  let mut reply = String::new();
  stream.read_to_string(&mut reply).await.unwrap();
  assert!(reply.contains("malformed request"), "{reply}");

  assert!(matches!(rig.call(Request::Ping).await, Ok(Response::Pong)));
}

#[tokio::test]
async fn without_a_daemon_the_client_says_how_to_start_one() {
  let rig = Rig::start().await;
  let nowhere = rig.socket.with_file_name("nobody-home.sock");

  let error = client::call_at(&nowhere, Request::Ping).await.unwrap_err();
  assert!(error.to_string().contains("steak daemon"), "{error}");
}
