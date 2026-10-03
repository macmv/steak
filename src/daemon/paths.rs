//! Where the daemon keeps its files. Nothing here is a cache: the daemon holds
//! all cached state in memory.

use std::path::PathBuf;

fn home() -> PathBuf {
  std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub use st_proto::ipc::socket_path as socket;

/// `~/.config/steak`, or `$XDG_CONFIG_HOME/steak`.
pub fn config_dir() -> PathBuf {
  std::env::var_os("XDG_CONFIG_HOME")
    .map(PathBuf::from)
    .filter(|path| path.is_absolute())
    .unwrap_or_else(|| home().join(".config"))
    .join("steak")
}

pub fn config_file() -> PathBuf { config_dir().join("config.json") }

/// The systemd user unit written by `steak daemon install`.
pub fn unit_file() -> PathBuf {
  std::env::var_os("XDG_CONFIG_HOME")
    .map(PathBuf::from)
    .filter(|path| path.is_absolute())
    .unwrap_or_else(|| home().join(".config"))
    .join("systemd/user/steak.service")
}
