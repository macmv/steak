//! The wire protocol between the daemon and its clients (the CLI and the TUI).
//!
//! Every connection starts with one request, written as a line of JSON. Most
//! requests are answered with one line of JSON and the daemon closes the
//! connection. `Request::Subscribe` is the exception: the daemon keeps the
//! connection open and writes a `Response::Player` line now, and another each
//! time the player changes, until the client disconnects.
//!
//! Everything here is plain data. A client needs none of the YouTube machinery.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::catalog::{Shelf, Track};

/// The control socket: `$STEAK_SOCKET`, else
/// `$XDG_RUNTIME_DIR/steak/steak.sock`.
pub fn socket_path() -> PathBuf {
  if let Some(path) = std::env::var_os("STEAK_SOCKET") {
    return PathBuf::from(path);
  }
  match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|dir| dir.is_absolute()) {
    Some(dir) => dir.join("steak/steak.sock"),
    // No login session (a bare shell over ssh, say): keep it per user and
    // private. The daemon creates the directory 0700.
    None => std::env::temp_dir()
      .join(format!("steak-{}", std::env::var("USER").unwrap_or_else(|_| "user".into())))
      .join("steak.sock"),
  }
}

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
  /// A player command. Answered with `Response::Ok` as soon as the player has
  /// accepted it; what it did shows up in the next `Response::Player`.
  Control(Command),
  /// Streams the player's state: see the module docs.
  Subscribe,
  /// The bytes of an image (album art). Only YouTube's image hosts are served.
  Art {
    url: String,
  },
  /// Save a video's audio as an mp4 file. Answers once the file is written.
  Download {
    video_id: String,
    output:   PathBuf,
  },
  /// Import the session from this Firefox profile (or the default profile).
  AuthImport {
    profile: Option<PathBuf>,
  },
  /// Re-import the session from Firefox and verify it.
  AuthRepair,
}

/// What a client can ask the player to do. These are intents, not state: when
/// two clients race, the daemon applies them one after the other.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Command {
  /// Replace the queue with this one track, with no radio behind it.
  Play {
    video_id: String,
  },
  /// Replace the queue with the song radio of `seed`, and play the seed.
  StartRadio {
    seed: Track,
  },
  /// Replace the queue with a playlist or album, and play its first track.
  StartPlaylist {
    playlist_id: String,
  },
  /// Play the queue entry at `index`. `video_id` is what the client believes
  /// is there; if the queue changed underneath it, nothing happens.
  JumpTo {
    index:    usize,
    video_id: String,
  },
  Pause,
  Resume,
  Toggle,
  Next,
  /// Restarts the track if it has been playing for a few seconds, else goes
  /// back one.
  Previous,
  /// Stops playback. The queue is kept.
  Stop,
}

#[derive(Debug, Deserialize, Serialize)]
pub enum Response {
  Pong,
  Ok,
  Error(String),
  Status(StatusReport),
  Home { shelves: Vec<Shelf>, logged_in: bool },
  Tracks(Vec<Track>),
  Session(SessionReport),
  Player(PlayerView),
  Art(Vec<u8>),
}

/// Where the queue came from, and so how it grows.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum Source {
  /// A single track, or nothing.
  #[default]
  None,
  Radio {
    seed: String,
  },
  Playlist {
    playlist_id: String,
  },
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum Transport {
  /// Nothing is playing, and nothing was playing a moment ago.
  #[default]
  Idle,
  /// The current track is being resolved and opened.
  Loading,
  Playing,
  Paused,
  /// The last track finished and there is nothing after it.
  Ended,
  Failed(String),
}

/// Everything a client needs to draw the player. Sent whole on every change;
/// a client that falls behind just sees the latest one.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlayerView {
  /// Increases with every change, so a client can tell snapshots apart.
  pub rev:          u64,
  pub queue:        Vec<Track>,
  /// The current track's index in `queue`. Meaningless if `queue` is empty.
  pub index:        usize,
  pub source:       Source,
  pub transport:    Transport,
  /// How far into the current track playback was when this was sent. While
  /// `transport` is `Playing`, a client adds the time since it received this.
  pub position_ms:  u64,
  /// More of the queue is being fetched.
  pub loading_more: bool,
}

impl PlayerView {
  pub fn current(&self) -> Option<&Track> { self.queue.get(self.index) }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct StatusReport {
  pub version:     String,
  pub uptime_secs: u64,
  pub player:      PlayerView,
  pub session:     SessionReport,
  pub caches:      Vec<CellReport>,
}

/// A snapshot for `steak status`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionReport {
  pub state:                 String,
  pub detail:                Option<String>,
  pub source:                Option<String>,
  pub last_import_secs:      Option<u64>,
  pub next_auto_repair_secs: Option<u64>,
}

/// A snapshot of one cell for `steak status`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CellReport {
  pub name:                 String,
  pub state:                String,
  pub generation:           Option<u64>,
  pub age_secs:             Option<u64>,
  pub consecutive_failures: u32,
  pub last_error:           Option<String>,
  pub retry_in_secs:        Option<u64>,
}
