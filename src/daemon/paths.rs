//! Where the daemon keeps its files. Nothing here is a cache: the daemon holds
//! all cached state in memory.

use std::path::PathBuf;

fn home() -> PathBuf {
  std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// The control socket: `$STEAK_SOCKET`, else
/// `$XDG_RUNTIME_DIR/steak/steak.sock`.
pub fn socket() -> PathBuf {
  if let Some(path) = std::env::var_os("STEAK_SOCKET") {
    return PathBuf::from(path);
  }
  match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|dir| dir.is_absolute()) {
    Some(dir) => dir.join("steak/steak.sock"),
    // No login session (a bare shell over ssh, say): keep it per user and
    // private. The directory is created 0700 by the server.
    None => std::env::temp_dir()
      .join(format!("steak-{}", std::env::var("USER").unwrap_or_else(|_| "user".into())))
      .join("steak.sock"),
  }
}

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
