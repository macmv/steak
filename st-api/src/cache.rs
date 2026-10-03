//! On-disk cache for state that is the same for every video: the Music web
//! app's config, and the player's signature timestamp and challenge solver.
//!
//! Each fact is its own file under `~/.cache/steak/`. A file that does not
//! parse is deleted; callers delete files that turn out not to work (see
//! [`Cache::clear`]). Writes go through a temp file and a rename, so a crash
//! never leaves a half-written file behind.

use std::{
  fs,
  io::Write,
  path::{Path, PathBuf},
  time::Duration,
};

use log::{debug, warn};

use crate::innertube::bootstrap::BootstrapData;

const BOOTSTRAP: &str = "bootstrap.json";
const SIGNATURE_TIMESTAMP: &str = "signature_timestamp";
const SOLVER: &str = "solver.js";

/// How long the web app config is trusted. The player files are tied to the
/// script URL stored in it instead: see [`Cache::store_bootstrap`].
const BOOTSTRAP_TTL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Clone, Debug)]
pub struct Cache {
  dir: PathBuf,
}

impl Cache {
  pub fn new(dir: PathBuf) -> Self { Self { dir } }

  pub fn dir(&self) -> &std::path::Path { &self.dir }

  /// `$XDG_CACHE_HOME/steak`, or `~/.cache/steak`.
  pub fn open() -> Self {
    let base = std::env::var_os("XDG_CACHE_HOME")
      .map(PathBuf::from)
      .filter(|path| path.is_absolute())
      .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
      .unwrap_or_else(|| PathBuf::from(".cache"));
    Self::new(base.join("steak"))
  }

  /// The cached web app config, if it is fresh and names a player script.
  pub fn bootstrap(&self) -> Option<BootstrapData> {
    let data: BootstrapData = self.read_json(BOOTSTRAP)?;
    if data.player_script_url.is_none() {
      self.remove(BOOTSTRAP);
      return None;
    }
    // A stale file is left in place: `store_bootstrap` compares against it.
    let age = fs::metadata(self.path(BOOTSTRAP)).and_then(|m| m.modified()).ok()?.elapsed().ok()?;
    if age > BOOTSTRAP_TTL {
      debug!("cached bootstrap is {}s old; refetching", age.as_secs());
      return None;
    }
    Some(data)
  }

  /// Saves a freshly fetched config. `player_script_url` must be filled in.
  ///
  /// If it names a different player than the previous config did, the solver
  /// and signature timestamp belong to the old player, so they are deleted.
  pub fn store_bootstrap(&self, data: &BootstrapData) {
    if data.player_script_url.is_none() {
      return;
    }
    let old: Option<BootstrapData> = self.read_json(BOOTSTRAP);
    if old.is_some_and(|old| old.player_script_url != data.player_script_url) {
      debug!("player script changed; dropping the cached solver");
      self.remove(SIGNATURE_TIMESTAMP);
      self.remove(SOLVER);
    }
    match serde_json::to_vec(data) {
      Ok(bytes) => self.write(BOOTSTRAP, &bytes),
      Err(error) => warn!("could not serialize the bootstrap cache: {error}"),
    }
  }

  pub fn signature_timestamp(&self) -> Option<u32> {
    let text = fs::read_to_string(self.path(SIGNATURE_TIMESTAMP)).ok()?;
    let parsed = text.trim().parse().ok();
    if parsed.is_none() {
      self.remove(SIGNATURE_TIMESTAMP);
    }
    parsed
  }

  pub fn store_signature_timestamp(&self, timestamp: u32) {
    self.write(SIGNATURE_TIMESTAMP, timestamp.to_string().as_bytes());
  }

  /// The preprocessed player (see `playback::solver::Solved::preprocessed`).
  pub fn solver(&self) -> Option<String> {
    let text = fs::read_to_string(self.path(SOLVER)).ok()?;
    if text.is_empty() {
      self.remove(SOLVER);
      return None;
    }
    Some(text)
  }

  pub fn store_solver(&self, preprocessed: &str) { self.write(SOLVER, preprocessed.as_bytes()); }

  /// Whether any cache file exists.
  pub fn is_populated(&self) -> bool {
    [BOOTSTRAP, SIGNATURE_TIMESTAMP, SOLVER].iter().any(|name| self.path(name).exists())
  }

  /// Deletes every cache file. Used when something built from the cache failed.
  pub fn clear(&self) {
    for name in [BOOTSTRAP, SIGNATURE_TIMESTAMP, SOLVER] {
      self.remove(name);
    }
  }

  fn path(&self, name: &str) -> PathBuf { self.dir.join(name) }

  fn read_json<T: serde::de::DeserializeOwned>(&self, name: &str) -> Option<T> {
    let bytes = fs::read(self.path(name)).ok()?;
    match serde_json::from_slice(&bytes) {
      Ok(value) => Some(value),
      Err(error) => {
        warn!("deleting unreadable cache file {name}: {error}");
        self.remove(name);
        None
      }
    }
  }

  fn remove(&self, name: &str) { let _ = fs::remove_file(self.path(name)); }

  fn write(&self, name: &str, bytes: &[u8]) {
    if let Err(error) = write_atomic(&self.dir, name, bytes) {
      warn!("could not write cache file {name}: {error}");
    }
  }
}

fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
  fs::create_dir_all(dir)?;
  let temp = dir.join(format!("{name}.{}.tmp", std::process::id()));
  let result = fs::File::create(&temp)
    .and_then(|mut file| file.write_all(bytes))
    .and_then(|()| fs::rename(&temp, dir.join(name)));
  if result.is_err() {
    let _ = fs::remove_file(&temp);
  }
  result
}

#[cfg(test)]
mod tests {
  use serde_json::Map;

  use super::*;

  fn data(url: Option<&str>) -> BootstrapData {
    BootstrapData {
      api_key:           "key".into(),
      client_version:    "1.2".into(),
      visitor_data:      None,
      player_script_url: url.map(str::to_owned),
      request_context:   Map::new(),
      cookies:           None,
    }
  }

  fn cache(name: &str) -> Cache {
    let dir = std::env::temp_dir().join(format!("steak-cache-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    Cache::new(dir)
  }

  #[test]
  fn round_trips_each_file() {
    let cache = cache("round-trip");
    assert!(!cache.is_populated());
    cache.store_bootstrap(&data(Some("https://x/s/player/a/base.js")));
    cache.store_signature_timestamp(20725);
    cache.store_solver("js");
    assert_eq!(cache.bootstrap(), Some(data(Some("https://x/s/player/a/base.js"))));
    assert_eq!(cache.signature_timestamp(), Some(20725));
    assert_eq!(cache.solver().as_deref(), Some("js"));
    cache.clear();
    assert!(!cache.is_populated());
  }

  #[test]
  fn bootstrap_without_a_player_url_is_not_stored() {
    let cache = cache("no-url");
    cache.store_bootstrap(&data(None));
    assert!(!cache.is_populated());
  }

  #[test]
  fn changed_player_url_drops_the_solver_but_same_url_keeps_it() {
    let cache = cache("url-change");
    cache.store_bootstrap(&data(Some("https://x/s/player/a/base.js")));
    cache.store_signature_timestamp(1);
    cache.store_solver("js");
    cache.store_bootstrap(&data(Some("https://x/s/player/a/base.js")));
    assert!(cache.solver().is_some());
    cache.store_bootstrap(&data(Some("https://x/s/player/b/base.js")));
    assert!(cache.solver().is_none());
    assert!(cache.signature_timestamp().is_none());
    assert!(cache.bootstrap().is_some());
  }

  #[test]
  fn unreadable_files_are_deleted() {
    let cache = cache("corrupt");
    cache.write(BOOTSTRAP, b"{not json");
    cache.write(SIGNATURE_TIMESTAMP, b"abc");
    assert!(cache.bootstrap().is_none());
    assert!(cache.signature_timestamp().is_none());
    assert!(!cache.is_populated());
  }
}
