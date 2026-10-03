//! Running the daemon at login, as a systemd user service.

use std::process::Command;

use super::paths;

fn unit(exe: &std::path::Path) -> String {
  format!(
    "[Unit]\n\
     Description=Steak, a YouTube Music player daemon\n\
     After=network-online.target\n\
     \n\
     [Service]\n\
     ExecStart={exe} daemon\n\
     # The daemon never exits on its own over an error. If it does die (or its\n\
     # watchdog gives up on a stuck runtime), start a fresh one.\n\
     Restart=always\n\
     RestartSec=2\n\
     \n\
     [Install]\n\
     WantedBy=default.target\n",
    exe = exe.display()
  )
}

fn systemctl(args: &[&str]) -> Result<(), String> {
  let status = Command::new("systemctl")
    .arg("--user")
    .args(args)
    .status()
    .map_err(|error| format!("could not run systemctl: {error}"))?;
  status
    .success()
    .then_some(())
    .ok_or_else(|| format!("systemctl --user {} failed", args.join(" ")))
}

/// Writes the unit for the running executable, then enables and starts it.
pub fn install() -> Result<(), String> {
  let exe =
    std::env::current_exe().map_err(|error| format!("cannot find this executable: {error}"))?;
  let path = paths::unit_file();
  std::fs::create_dir_all(path.parent().expect("the unit path has a parent"))
    .and_then(|()| std::fs::write(&path, unit(&exe)))
    .map_err(|error| format!("could not write {}: {error}", path.display()))?;
  eprintln!("Wrote {}", path.display());
  systemctl(&["daemon-reload"])?;
  systemctl(&["enable", "--now", "steak.service"])?;
  eprintln!("The steak daemon is running and will start at login.");
  Ok(())
}

pub fn uninstall() -> Result<(), String> {
  let _ = systemctl(&["disable", "--now", "steak.service"]);
  let path = paths::unit_file();
  match std::fs::remove_file(&path) {
    Ok(()) => eprintln!("Removed {}", path.display()),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
    Err(error) => return Err(format!("could not remove {}: {error}", path.display())),
  }
  systemctl(&["daemon-reload"])
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn unit_runs_the_daemon_subcommand_and_restarts_it() {
    let unit = unit(std::path::Path::new("/usr/bin/steak"));
    assert!(unit.contains("ExecStart=/usr/bin/steak daemon\n"));
    assert!(unit.contains("Restart=always"));
    assert!(unit.contains("WantedBy=default.target"));
  }
}
