//! Finding the Firefox profile whose cookies hold the YouTube session.

use std::path::{Path, PathBuf};

/// The directories that can hold `profiles.ini`, in the order they are tried:
/// a regular install, the XDG location newer Firefox uses, then snap and
/// flatpak.
fn roots() -> Vec<PathBuf> {
  let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return Vec::new() };
  vec![
    home.join(".mozilla/firefox"),
    home.join(".config/mozilla/firefox"),
    home.join("snap/firefox/common/.mozilla/firefox"),
    home.join(".var/app/org.mozilla.firefox/.mozilla/firefox"),
  ]
}

/// The profile Firefox itself would open by default, if it can be found.
pub fn discover_profile() -> Option<PathBuf> { discover_profile_in(&roots()) }

pub fn discover_profile_in(roots: &[PathBuf]) -> Option<PathBuf> {
  roots.iter().find_map(|root| {
    let text = std::fs::read_to_string(root.join("profiles.ini")).ok()?;
    default_profile(root, &text).filter(|path| path.join("cookies.sqlite").is_file())
  })
}

/// Picks the default profile out of a `profiles.ini`.
///
/// Firefox records the profile of each installation in an `[Install...]`
/// section; older versions flag one `[Profile...]` section with `Default=1`.
/// If neither is present, the first profile is the best guess.
fn default_profile(root: &Path, text: &str) -> Option<PathBuf> {
  #[derive(Default)]
  struct Profile {
    path:        Option<String>,
    is_relative: bool,
    is_default:  bool,
  }
  let mut install_default = None;
  let mut profiles: Vec<Profile> = Vec::new();
  let mut section = String::new();
  for line in text.lines().map(str::trim) {
    if let Some(name) = line.strip_prefix('[').and_then(|line| line.strip_suffix(']')) {
      section = name.to_owned();
      if section.starts_with("Profile") {
        profiles.push(Profile { is_relative: true, ..Profile::default() });
      }
      continue;
    }
    let Some((key, value)) = line.split_once('=') else { continue };
    if section.starts_with("Install") && key == "Default" {
      install_default.get_or_insert_with(|| value.to_owned());
    } else if section.starts_with("Profile") {
      let profile = profiles.last_mut()?;
      match key {
        "Path" => profile.path = Some(value.to_owned()),
        "IsRelative" => profile.is_relative = value != "0",
        "Default" => profile.is_default = value == "1",
        _ => {}
      }
    }
  }
  let resolve = |path: &str, is_relative: bool| {
    if is_relative { root.join(path) } else { PathBuf::from(path) }
  };
  if let Some(path) = install_default {
    // The install section does not say whether the path is relative; Firefox
    // always writes it relative to the profiles root.
    return Some(resolve(&path, !path.starts_with('/')));
  }
  let chosen = profiles.iter().find(|profile| profile.is_default).or(profiles.first())?;
  Some(resolve(chosen.path.as_deref()?, chosen.is_relative))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn install_section_wins() {
    let ini = "[Profile0]\nName=old\nIsRelative=1\nPath=old.default\nDefault=1\n\n\
               [Install4F96D1932A9F858E]\nDefault=Profiles/abc.default-release\nLocked=1\n\n\
               [Profile1]\nName=release\nIsRelative=1\nPath=Profiles/abc.default-release\n";
    assert_eq!(
      default_profile(Path::new("/ff"), ini),
      Some(PathBuf::from("/ff/Profiles/abc.default-release"))
    );
  }

  #[test]
  fn falls_back_to_the_flagged_then_the_first_profile() {
    let flagged =
      "[Profile0]\nPath=a\nIsRelative=1\n[Profile1]\nPath=/abs/b\nIsRelative=0\nDefault=1\n";
    assert_eq!(default_profile(Path::new("/ff"), flagged), Some(PathBuf::from("/abs/b")));
    let plain = "[Profile0]\nPath=a\nIsRelative=1\n[Profile1]\nPath=b\nIsRelative=1\n";
    assert_eq!(default_profile(Path::new("/ff"), plain), Some(PathBuf::from("/ff/a")));
    assert_eq!(default_profile(Path::new("/ff"), "[General]\nVersion=2\n"), None);
  }
}
