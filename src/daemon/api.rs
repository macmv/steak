//! What the daemon does for a client: the operations behind each request.
//!
//! These only assemble the pieces. Credentials come from [`Session`], cached
//! web app state from [`Caches`], and each reports its own failures; this layer
//! decides when a failure means "rebuild and try once more".

use std::{error::Error, time::Duration};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use log::{info, warn};
use st_api::{
  Url,
  catalog::{HomeParser, PlaylistParser, QueueParser, Shelf, Track},
  media::audio::{AudioClient, AudioRequest},
  playback::{
    access::po_token::authorize_stream,
    player_script::{player_cache_key, resolve_with_solver},
    response::{AudioQuality, PlayerParser, StandardPlayerParser},
  },
};

use super::Daemon;

pub type BoxError = Box<dyn Error + Send + Sync>;

/// The audio of one track, as it downloads.
pub type AudioChunks = std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, String>> + Send>>;

/// Each stage of getting audio is bounded, so playback can be slow to start
/// but never hang.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(150);
const AUTHORIZE_TIMEOUT: Duration = Duration::from_secs(150);
const OPEN_TIMEOUT: Duration = Duration::from_secs(60);

impl Daemon {
  /// The homepage shelves, and whether YouTube served them to a signed-in
  /// account.
  pub async fn home(&self, more: u32) -> Result<(Vec<Shelf>, bool), BoxError> {
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
  pub async fn radio(&self, video_id: &str, more: u32) -> Result<Vec<Track>, BoxError> {
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
  pub async fn playlist(&self, playlist_id: &str, more: u32) -> Result<Vec<Track>, BoxError> {
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

  /// Resolves `video_id` to a signed URL that googlevideo serves in full, and
  /// opens it as a stream of chunks.
  pub async fn open_audio(&self, video_id: &str) -> Result<AudioChunks, BoxError> {
    let url =
      timeout(RESOLVE_TIMEOUT, "resolving the stream", self.resolve_stream(video_id)).await??;
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
    Ok(Box::pin(stream.map(|chunk| chunk.map_err(|error| error.to_string()))))
  }

  /// Resolves `video_id` to a signed audio URL.
  ///
  /// If a cached piece turns out not to fit (YouTube moved to a new player, or
  /// the solver died), that piece is invalidated and the whole thing is tried
  /// once more with a rebuilt one. A second failure is reported as is.
  async fn resolve_stream(&self, video_id: &str) -> Result<Url, BoxError> {
    match self.try_resolve(video_id).await? {
      Ok(url) => return Ok(url),
      Err(stale) => warn!("{stale}; rebuilt the affected cache, trying once more"),
    }
    match self.try_resolve(video_id).await? {
      Ok(url) => Ok(url),
      Err(stale) => Err(stale.into()),
    }
  }

  /// One attempt. The outer error is final; the inner one means a cache was
  /// stale (and has been invalidated), so a retry is worthwhile.
  async fn try_resolve(&self, video_id: &str) -> Result<Result<Url, String>, BoxError> {
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
        Ok(Ok(url))
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
