//! The wire protocol between the CLI and the daemon.
//!
//! One request per connection: the client writes a line of JSON, the daemon
//! answers with a line of JSON and closes. Every response is plain data, so the
//! CLI needs none of the YouTube machinery.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use st_api::catalog::{Shelf, Track};

use crate::daemon::{cell::CellReport, session::SessionReport};

#[derive(Debug, Deserialize, Serialize)]
pub enum Request {
  Ping,
  Status,
  Home {
    more: u32,
  },
  Radio {
    video_id: String,
    more:     u32,
  },
  Playlist {
    playlist_id: String,
    more:        u32,
  },
  /// Start playing, replacing whatever is playing. Answers once playback has
  /// started or failed to.
  Play {
    video_id: String,
  },
  Stop,
  /// Import the session from this Firefox profile (or the default profile).
  AuthImport {
    profile: Option<PathBuf>,
  },
  /// Re-import the session from Firefox and verify it.
  AuthRepair,
}

#[derive(Debug, Deserialize, Serialize)]
pub enum Response {
  Pong,
  Error(String),
  Status(StatusReport),
  Home { shelves: Vec<Shelf>, logged_in: bool },
  Tracks(Vec<Track>),
  Playing { video_id: String },
  Stopped,
  Session(SessionReport),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum PlaybackState {
  Idle,
  Resolving { video_id: String },
  Playing { video_id: String },
  Failed { video_id: String, error: String },
}

#[derive(Debug, Deserialize, Serialize)]
pub struct StatusReport {
  pub version:     String,
  pub uptime_secs: u64,
  pub playback:    PlaybackState,
  pub session:     SessionReport,
  pub caches:      Vec<CellReport>,
}
