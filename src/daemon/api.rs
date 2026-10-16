//! What the daemon does for a client: the operations behind each request.
//!
//! Everything the daemon needs from outside the process (YouTube, the audio
//! device, ffmpeg) sits behind the [`Backend`] trait, so the daemon itself is
//! plain plumbing that can run against a fake in tests. [`YouTube`] is the real
//! backend.
//!
//! [`YouTube`] only assembles the pieces. Credentials come from [`Session`],
//! cached web app state from [`Caches`], and each reports its own failures;
//! this layer decides when a failure means "rebuild and try once more".

use std::{
  error::Error,
  future::Future,
  path::{Path, PathBuf},
  pin::Pin,
  sync::{Arc, Mutex},
  time::{Duration, Instant},
};

use bytes::Bytes;
use futures_util::{FutureExt, Stream, StreamExt};
use log::{info, warn};
use st_api::{
  Url,
  catalog::{HomeParser, PlaylistParser, QueueParser},
  innertube::bootstrap::WebBootstrapper,
  media::audio::{AudioClient, AudioRequest},
  playback::{
    access::po_token::authorize_stream,
    player_script::{player_cache_key, resolve_with_solver},
    response::{AudioQuality, PlayerParser, StandardPlayerParser},
    stats::{Ping, PlaybackTracking, with_cpn},
  },
};

use st_proto::{
  catalog::{Shelf, Track},
  ipc::{CellReport, SessionReport, Source},
};

use super::{
  art::ArtCache,
  audio,
  caches::Caches,
  download,
  machine::Batch,
  session::{Session, SessionError, Verifier},
};

pub type BoxError = Box<dyn Error + Send + Sync>;

/// A boxed future that may borrow from the backend.
pub type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Called once the audio is audible, with the process ID to signal for
/// pause/resume (if there is a process).
pub type OnStarted = Box<dyn FnOnce(Option<u32>) + Send>;

/// Everything the daemon needs from the outside world.
pub trait Backend: Send + Sync + 'static {
  /// The homepage shelves, and whether YouTube served them to a signed-in
  /// account.
  fn home(&self, more: u32) -> Fut<'_, Result<(Vec<Shelf>, bool), BoxError>>;
  fn radio<'a>(&'a self, video_id: &'a str, more: u32) -> Fut<'a, Result<Vec<Track>, BoxError>>;
  fn playlist<'a>(
    &'a self,
    playlist_id: &'a str,
    more: u32,
  ) -> Fut<'a, Result<Vec<Track>, BoxError>>;
  /// One batch of a player queue: the first (`token: None`) or the one after
  /// `token`.
  fn load_queue<'a>(
    &'a self,
    source: &'a Source,
    token: Option<String>,
  ) -> Fut<'a, Result<Batch, BoxError>>;
  /// Opens the audio of `video_id` as a stream of chunks.
  fn open_audio<'a>(&'a self, video_id: &'a str) -> Fut<'a, Result<Opened, BoxError>>;
  /// Tells YouTube about a playback's progress (see `stats.rs`). Nothing to do
  /// by default.
  fn report<'a>(&'a self, _: &'a PlaybackTracking, _: Ping) -> Fut<'a, Result<(), BoxError>> {
    Box::pin(async { Ok(()) })
  }
  /// The bytes of an image (album art).
  fn art<'a>(&'a self, url: &'a str) -> Fut<'a, Result<Vec<u8>, BoxError>>;

  /// Plays `audio` out loud and returns when it has finished. Dropping the
  /// future stops the playback.
  fn play(&self, audio: AudioChunks, on_started: OnStarted) -> Fut<'_, Result<(), String>>;
  /// Saves `audio` as an mp4 file at `output`.
  fn save<'a>(&'a self, audio: AudioChunks, output: &'a Path) -> Fut<'a, Result<(), String>>;

  fn session_report(&self) -> SessionReport;
  /// Imports the session from a Firefox profile (or the default one).
  fn import_session(&self, profile: Option<PathBuf>) -> Fut<'_, Result<SessionReport, String>>;
  /// Re-imports the session and verifies it.
  fn repair_session(&self) -> Fut<'_, SessionReport>;
  fn cache_reports(&self) -> Vec<CellReport>;
  /// One round of background upkeep. Starts work; never waits for it.
  fn maintain(&self);
}

/// The audio of one track, as it downloads.
pub type AudioChunks = std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, String>> + Send>>;

/// An opened track: its audio, and what is needed to report it as played.
pub struct Opened {
  pub chunks:   AudioChunks,
  /// `None` when YouTube gave no tracking URLs, or for a fake.
  pub tracking: Option<PlaybackTracking>,
}

/// Each stage of getting audio is bounded, so playback can be slow to start
/// but never hang.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(150);
const AUTHORIZE_TIMEOUT: Duration = Duration::from_secs(150);
const OPEN_TIMEOUT: Duration = Duration::from_secs(60);
/// The signed-in page's token is reused this long before it is fetched again.
const SESSION_TOKEN_TTL: Duration = Duration::from_secs(60 * 60);

/// The real backend: YouTube Music, `ffplay` and `ffmpeg`.
pub struct YouTube {
  pub caches:  Arc<Caches>,
  pub session: Arc<Session>,
  art:         ArtCache,
  /// The signed-in page's `XSRF_TOKEN`, for the session generation it was
  /// fetched with.
  token:       Mutex<Option<SessionToken>>,
}

struct SessionToken {
  generation: u64,
  fetched:    Instant,
  value:      String,
}

impl YouTube {
  pub fn new() -> Self {
    let caches = Caches::new(st_api::http_client());
    let verifier: Verifier = {
      let caches = caches.clone();
      Arc::new(move |session| {
        let caches = caches.clone();
        async move { caches.verify(&session).await }.boxed()
      })
    };
    let session = Session::new(super::config::Config::load(), verifier);
    Self { caches, session, art: ArtCache::default(), token: Mutex::new(None) }
  }
}

impl Backend for YouTube {
  fn home(&self, more: u32) -> Fut<'_, Result<(Vec<Shelf>, bool), BoxError>> {
    Box::pin(self.fetch_home(more))
  }

  fn radio<'a>(&'a self, video_id: &'a str, more: u32) -> Fut<'a, Result<Vec<Track>, BoxError>> {
    Box::pin(self.fetch_radio(video_id, more))
  }

  fn playlist<'a>(
    &'a self,
    playlist_id: &'a str,
    more: u32,
  ) -> Fut<'a, Result<Vec<Track>, BoxError>> {
    Box::pin(self.fetch_playlist(playlist_id, more))
  }

  fn load_queue<'a>(
    &'a self,
    source: &'a Source,
    token: Option<String>,
  ) -> Fut<'a, Result<Batch, BoxError>> {
    Box::pin(self.fetch_queue(source, token))
  }

  fn open_audio<'a>(&'a self, video_id: &'a str) -> Fut<'a, Result<Opened, BoxError>> {
    Box::pin(self.fetch_audio(video_id))
  }

  fn report<'a>(
    &'a self,
    tracking: &'a PlaybackTracking,
    ping: Ping,
  ) -> Fut<'a, Result<(), BoxError>> {
    Box::pin(self.send_ping(tracking, ping))
  }

  fn art<'a>(&'a self, url: &'a str) -> Fut<'a, Result<Vec<u8>, BoxError>> {
    Box::pin(self.art.fetch(&self.caches.http, url))
  }

  fn play(&self, audio: AudioChunks, on_started: OnStarted) -> Fut<'_, Result<(), String>> {
    Box::pin(audio::play_stream(audio, on_started))
  }

  fn save<'a>(&'a self, audio: AudioChunks, output: &'a Path) -> Fut<'a, Result<(), String>> {
    Box::pin(download::save(audio, output))
  }

  fn session_report(&self) -> SessionReport { self.session.report() }

  fn import_session(&self, profile: Option<PathBuf>) -> Fut<'_, Result<SessionReport, String>> {
    Box::pin(async move {
      let result = match profile.or_else(|| self.session.source()) {
        Some(profile) => self.session.set_profile(profile).await,
        None => Err(SessionError("no Firefox profile found; pass the profile directory".into())),
      };
      result.map(|()| self.session.report()).map_err(|error| error.to_string())
    })
  }

  fn repair_session(&self) -> Fut<'_, SessionReport> {
    Box::pin(async move {
      self.session.repair_now().await;
      self.session.report()
    })
  }

  fn cache_reports(&self) -> Vec<CellReport> { self.caches.reports() }

  fn maintain(&self) {
    self.caches.maintain();
    self.session.maintain();
  }
}

impl YouTube {
  async fn fetch_home(&self, more: u32) -> Result<(Vec<Shelf>, bool), BoxError> {
    let parser = HomeParser;
    // If YouTube serves a signed-out page the session is bad: report it, which
    // starts a repair, and try once more with whatever the repair produced.
    for attempt in 0..2 {
      let (client, lease) = self.client().await?;
      let mut response = client.browse("FEmusic_home", None).await?;
      let logged_in = parser.logged_in(&response);
      if !logged_in && attempt == 0 {
        self.session.report_rejected(&lease);
        continue;
      }

      let mut shelves = Vec::new();
      for page in 0..=more {
        shelves.extend(parser.shelves(&response));
        if page == more {
          break;
        }
        let Some(token) = parser.continuation(&response) else { break };
        response = client.browse_continuation(token).await?;
      }
      return Ok((shelves, logged_in));
    }
    unreachable!("the second attempt always returns")
  }

  /// The song radio YouTube Music builds from `video_id`.
  async fn fetch_radio(&self, video_id: &str, more: u32) -> Result<Vec<Track>, BoxError> {
    let (client, _) = self.client().await?;
    let parser = QueueParser;
    let mut response = client.next(video_id, Some(format!("RDAMVM{video_id}"))).await?;
    let mut all = Vec::new();
    for page in 0..=more {
      let tracks = parser.tracks(&response);
      if tracks.is_empty() && page == 0 {
        return Err(format!("no radio found for {video_id}").into());
      }
      all.extend(tracks);
      if page == more {
        break;
      }
      let Some(token) = parser.continuation(&response) else { break };
      response = client.next_continuation(token).await?;
    }
    Ok(all)
  }

  /// The tracks of a playlist or album.
  async fn fetch_playlist(&self, playlist_id: &str, more: u32) -> Result<Vec<Track>, BoxError> {
    let (client, _) = self.client().await?;
    let parser = PlaylistParser;
    // Browse IDs are the playlist ID behind a `VL` prefix.
    let playlist_id = playlist_id.strip_prefix("VL").unwrap_or(playlist_id);
    let mut response = client.browse(format!("VL{playlist_id}"), None).await?;
    let mut all = Vec::new();
    for page in 0..=more {
      let tracks = parser.tracks(&response);
      if tracks.is_empty() && page == 0 {
        return Err(format!("no tracks found for playlist {playlist_id}").into());
      }
      all.extend(tracks);
      if page == more {
        break;
      }
      let Some(token) = parser.continuation(&response) else { break };
      response = client.browse_continuation(token).await?;
    }
    Ok(all)
  }

  /// One batch of a player queue: the first (`token: None`) or the one after
  /// `token`.
  async fn fetch_queue(&self, source: &Source, token: Option<String>) -> Result<Batch, BoxError> {
    let (client, _) = self.client().await?;
    match source {
      Source::None => Err("this queue does not grow".into()),
      Source::Radio { seed } => {
        let response = match token {
          Some(token) => client.next_continuation(token).await?,
          None => client.next(seed, Some(format!("RDAMVM{seed}"))).await?,
        };
        Ok(Batch {
          tracks:       QueueParser.tracks(&response),
          continuation: QueueParser.continuation(&response),
        })
      }
      Source::Playlist { playlist_id } => {
        let playlist_id = playlist_id.strip_prefix("VL").unwrap_or(playlist_id);
        let response = match token {
          Some(token) => client.browse_continuation(token).await?,
          None => client.browse(format!("VL{playlist_id}"), None).await?,
        };
        let tracks = PlaylistParser.tracks(&response);
        Ok(Batch { tracks, continuation: PlaylistParser.continuation(&response) })
      }
    }
  }

  /// Resolves `video_id` to a signed URL that googlevideo serves in full, and
  /// opens it as a stream of chunks.
  async fn fetch_audio(&self, video_id: &str) -> Result<Opened, BoxError> {
    let (url, tracking) =
      timeout(RESOLVE_TIMEOUT, "resolving the stream", self.resolve_stream(video_id)).await??;
    // The web player tells googlevideo which playback a stream belongs to; the
    // stats pings carry the same `cpn`.
    let url = match &tracking {
      Some(tracking) => with_cpn(url, &tracking.cpn),
      None => url,
    };
    // googlevideo refuses a stream past its first chunk unless it carries a
    // valid PoToken. This is outside the cache-rebuild logic on purpose: a
    // PoToken failure says nothing about the caches.
    let audio = AudioClient::new(self.caches.http.clone());
    let url = timeout(AUTHORIZE_TIMEOUT, "authorizing the stream", async {
      authorize_stream(&audio, url, video_id).await.map_err(BoxError::from)
    })
    .await??;
    let stream = timeout(OPEN_TIMEOUT, "opening the stream", async {
      audio.stream_chunked(AudioRequest::new(url)).await.map_err(|e| BoxError::from(e.to_string()))
    })
    .await??;
    let chunks = Box::pin(stream.map(|chunk| chunk.map_err(|error| error.to_string())));
    Ok(Opened { chunks, tracking })
  }

  /// Sends one stats ping as the signed-in account.
  async fn send_ping(&self, tracking: &PlaybackTracking, ping: Ping) -> Result<(), BoxError> {
    let (client, lease) = self.client().await?;
    let token = self.session_token(&lease).await?;
    let url = tracking.url(&ping, &client.stats_params());
    client.send_stats(url, &token).await.map_err(|error| error.without_url())?;
    Ok(())
  }

  /// The signed-in page's `XSRF_TOKEN`, fetched once per session and hour.
  async fn session_token(&self, lease: &super::session::SessionLease) -> Result<String, BoxError> {
    {
      let cached = self.token.lock().unwrap();
      if let Some(token) = cached.as_ref() {
        if token.generation == lease.generation && token.fetched.elapsed() < SESSION_TOKEN_TTL {
          return Ok(token.value.clone());
        }
      }
    }
    let value = WebBootstrapper::new(self.caches.http.clone())
      .session_token(lease.session.auth_session().request_headers())
      .await?;
    *self.token.lock().unwrap() = Some(SessionToken {
      generation: lease.generation,
      fetched:    Instant::now(),
      value:      value.clone(),
    });
    Ok(value)
  }

  /// Resolves `video_id` to a signed audio URL.
  ///
  /// If a cached piece turns out not to fit (YouTube moved to a new player, or
  /// the solver died), that piece is invalidated and the whole thing is tried
  /// once more with a rebuilt one. A second failure is reported as is.
  async fn resolve_stream(
    &self,
    video_id: &str,
  ) -> Result<(Url, Option<PlaybackTracking>), BoxError> {
    match self.try_resolve(video_id).await? {
      Ok(resolved) => return Ok(resolved),
      Err(stale) => warn!("{stale}; rebuilt the affected cache, trying once more"),
    }
    match self.try_resolve(video_id).await? {
      Ok(resolved) => Ok(resolved),
      Err(stale) => Err(stale.into()),
    }
  }

  /// One attempt. The outer error is final; the inner one means a cache was
  /// stale (and has been invalidated), so a retry is worthwhile.
  async fn try_resolve(
    &self,
    video_id: &str,
  ) -> Result<Result<(Url, Option<PlaybackTracking>), String>, BoxError> {
    let bootstrap = self.caches.bootstrap.get().await?;
    let player = self.caches.player_for(&bootstrap).await?;
    let (client, _) = self.client().await?;

    let response =
      client.player_with_signature_timestamp(video_id, player.signature_timestamp).await?;
    if let Some(served) = StandardPlayerParser::player_script_url(&response) {
      if player_cache_key(&served) != player.key {
        // YouTube has moved on; learn the new player from the web app.
        self.caches.bootstrap.invalidate(bootstrap.generation);
        self.caches.player.invalidate(player.generation);
        return Ok(Err(format!(
          "YouTube served player {} but the cache holds {}",
          player_cache_key(&served),
          player.key
        )));
      }
    }

    let parser = StandardPlayerParser;
    let format = parser.choose_audio(parser.audio_formats(&response)?, AudioQuality::Balanced)?;
    match resolve_with_solver(&format.stream_url, &player.solver).await {
      Ok(url) => {
        info!("resolved {video_id}: {} at {} bps", format.mime_type, format.bitrate);
        let tracking = PlaybackTracking::from_player(&response, format.itag);
        if tracking.is_none() {
          warn!(
            "the player response for {video_id} has no tracking URLs; this play will not be reported"
          );
        }
        Ok(Ok((url, tracking)))
      }
      Err(error) => {
        self.caches.player.invalidate(player.generation);
        Ok(Err(format!("the cached solver failed: {error}")))
      }
    }
  }

  /// A client with the current bootstrap and session, and the session lease to
  /// report if YouTube rejects it.
  async fn client(
    &self,
  ) -> Result<(st_api::innertube::client::MusicClient, super::session::SessionLease), BoxError> {
    let bootstrap = self.caches.bootstrap.get().await?;
    let lease = self.session.lease().await?;
    Ok((self.caches.music_client(&bootstrap, &lease.session), lease))
  }
}

/// `tokio::time::timeout` with an error that says what timed out.
async fn timeout<T>(
  limit: Duration,
  what: &str,
  future: impl std::future::Future<Output = T>,
) -> Result<T, BoxError> {
  tokio::time::timeout(limit, future)
    .await
    .map_err(|_| format!("{what} took longer than {}s", limit.as_secs()).into())
}
