//! Settings the user chose, as opposed to state the daemon works out itself.

use std::path::PathBuf;

use log::warn;
use serde::{Deserialize, Serialize};

use super::paths;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Config {
  /// The Firefox profile to import the YouTube session from. When unset, the
  /// profile Firefox opens by default is used.
  #[serde(default)]
  pub firefox_profile: Option<PathBuf>,
}

impl Config {
  /// Loads the config. A missing file is the default; a broken one is reported
  /// and ignored, so a bad edit can never keep the daemon from starting.
  pub fn load() -> Self {
    let path = paths::config_file();
    match std::fs::read(&path) {
      Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        warn!("ignoring unreadable config {}: {error}", path.display());
        Self::default()
      }),
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
      Err(error) => {
        warn!("could not read config {}: {error}", path.display());
        Self::default()
      }
    }
  }

  pub fn save(&self) -> std::io::Result<()> {
    let path = paths::config_file();
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let result = std::fs::write(&temp, serde_json::to_vec_pretty(self)?)
      .and_then(|()| std::fs::rename(&temp, &path));
    if result.is_err() {
      let _ = std::fs::remove_file(&temp);
    }
    result
  }
}
