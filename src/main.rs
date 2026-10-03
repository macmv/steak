//! `steak`: a command-line YouTube Music player.
//!
//! This binary is both halves of a small client/server pair, like MPD. `steak
//! daemon` is the server (see `daemon/`); every other subcommand is a thin
//! client that sends the daemon one request and prints the answer.

mod client;
mod daemon;
#[cfg(test)]
mod integration_test;
mod tui;

use std::{error::Error, path::PathBuf};

use clap::{Args, Parser, Subcommand};
use st_proto::{
  catalog::{ShelfItem, Track},
  ipc::{CellReport, Command, PlayerView, Request, Response, SessionReport, Transport},
};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug, Parser)]
#[command(about = "A YouTube Music player. Needs the `steak daemon` running.")]
struct Cli {
  /// Without a subcommand, opens the terminal UI.
  #[command(subcommand)]
  command: Option<CliCommand>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
  /// Run the daemon, or install it as a service that starts at login.
  Daemon(DaemonCommand),
  /// Show what the daemon is doing: session, caches and playback.
  Status,
  /// Manage the YouTube session the daemon uses.
  Auth(AuthCommand),
  /// Show the YouTube Music homepage: "Quick picks", "Listen again", and so on.
  Home {
    /// Number of additional pages of shelves to load after the first.
    #[arg(long, default_value_t = 0)]
    more: u32,
  },
  /// List the auto-mix (song radio) YouTube Music builds from a song.
  Radio {
    /// YouTube video ID to start the radio from.
    video_id: String,
    /// Number of additional batches of tracks to load after the first.
    #[arg(long, default_value_t = 0)]
    more:     u32,
  },
  /// List the tracks of a playlist or album.
  Playlist {
    /// Playlist ID, or the browse ID shown by `steak home` (`VL...`).
    playlist_id: String,
    /// Number of additional pages of tracks to load after the first.
    #[arg(long, default_value_t = 0)]
    more:        u32,
  },
  /// Play a song, replacing whatever is playing.
  Play {
    /// YouTube video ID to play.
    video_id: String,
  },
  /// Play a song, and the radio YouTube Music builds from it after it.
  PlayRadio {
    /// YouTube video ID to start the radio from.
    video_id: String,
  },
  /// Play a playlist or album, replacing whatever is playing.
  PlayPlaylist {
    /// Playlist ID, or the browse ID shown by `steak home` (`VL...`).
    playlist_id: String,
  },
  /// Pause playback.
  Pause,
  /// Resume playback.
  Resume,
  /// Pause if playing, resume if paused.
  Toggle,
  /// Skip to the next track.
  Next,
  /// Go back to the previous track, or restart this one if it just started.
  Prev,
  /// Stop playback.
  Stop,
  /// Show the player: what is playing, and the queue.
  Queue,
  /// Save a song's audio as an mp4 file, using `ffmpeg`.
  Download {
    /// YouTube video ID to download.
    video_id: String,
    /// Output file (defaults to `<video_id>.mp4`).
    #[arg(short, long)]
    output:   Option<PathBuf>,
  },
}

#[derive(Debug, Args)]
struct DaemonCommand {
  #[command(subcommand)]
  command: Option<DaemonSubcommand>,
}

#[derive(Debug, Subcommand)]
enum DaemonSubcommand {
  /// Run the daemon in the foreground (the default).
  Run,
  /// Install and start a systemd user service so the daemon runs at login.
  Install,
  /// Stop and remove the systemd user service.
  Uninstall,
}

#[derive(Debug, Args)]
struct AuthCommand {
  #[command(subcommand)]
  command: AuthSubcommand,
}

#[derive(Debug, Subcommand)]
enum AuthSubcommand {
  /// Import the YouTube session from a Firefox profile, and keep using that
  /// profile from now on.
  ImportFirefox {
    /// Firefox profile directory containing cookies.sqlite. Defaults to the
    /// profile Firefox opens by default.
    firefox_profile: Option<PathBuf>,
  },
  /// Re-import the session from Firefox and check that YouTube accepts it.
  Repair,
}

fn setup_logging(default: log::LevelFilter) -> std::result::Result<(), log::SetLoggerError> {
  let level =
    std::env::var("RUST_LOG").ok().and_then(|value| value.parse().ok()).unwrap_or(default);
  let start = std::time::Instant::now();
  fern::Dispatch::new()
    .level(level)
    .format(move |out, message, record| {
      out.finish(format_args!(
        "[{:>7.3}s {} {}] {message}",
        start.elapsed().as_secs_f64(),
        record.level(),
        record.target()
      ))
    })
    .chain(std::io::stderr())
    .apply()
}

#[tokio::main]
async fn main() {
  if let Err(error) = run().await {
    eprintln!("error: {error}");
    std::process::exit(1);
  }
}

async fn run() -> Result<()> {
  let cli = Cli::parse();
  let run_daemon = matches!(
    &cli.command,
    Some(CliCommand::Daemon(DaemonCommand { command: None | Some(DaemonSubcommand::Run) }))
  );
  // Anything logged to stderr would be drawn over the TUI.
  let level = match (&cli.command, run_daemon) {
    (_, true) => log::LevelFilter::Info,
    (None, _) => log::LevelFilter::Off,
    _ => log::LevelFilter::Warn,
  };
  setup_logging(level)?;

  let Some(command) = cli.command else { return tui::run().await };
  match command {
    CliCommand::Daemon(DaemonCommand { command }) => match command {
      None | Some(DaemonSubcommand::Run) => daemon::run().await,
      Some(DaemonSubcommand::Install) => daemon::service::install().map_err(Into::into),
      Some(DaemonSubcommand::Uninstall) => daemon::service::uninstall().map_err(Into::into),
    },
    CliCommand::Status => match client::call(Request::Status).await? {
      Response::Status(status) => {
        print_status(&status);
        Ok(())
      }
      other => unexpected(other),
    },
    CliCommand::Auth(AuthCommand { command }) => {
      let request = match command {
        AuthSubcommand::ImportFirefox { firefox_profile } => {
          Request::AuthImport { profile: firefox_profile }
        }
        AuthSubcommand::Repair => Request::AuthRepair,
      };
      match client::call(request).await? {
        Response::Session(session) => {
          print_session(&session);
          Ok(())
        }
        other => unexpected(other),
      }
    }
    CliCommand::Home { more } => match client::call(Request::Home { more }).await? {
      Response::Home { shelves, logged_in } => {
        if !logged_in {
          eprintln!(
            "warning: YouTube treated this session as signed out, so the homepage is generic. \
             Log in to YouTube Music in Firefox, then run `steak auth repair`."
          );
        }
        for shelf in shelves {
          println!("{}", shelf.title);
          for item in shelf.items {
            match item {
              ShelfItem::Track(track) => print_track(&track),
              ShelfItem::Card(card) => {
                let id = card
                  .watch
                  .as_ref()
                  .map(|watch| watch.video_id.as_str())
                  .or(card.browse_id.as_deref())
                  .unwrap_or("?");
                println!("  {} -- {} [{id}]", card.title, card.subtitle);
              }
            }
          }
          println!();
        }
        Ok(())
      }
      other => unexpected(other),
    },
    CliCommand::Radio { video_id, more } => {
      tracks(client::call(Request::Radio { video_id, more }).await?)
    }
    CliCommand::Playlist { playlist_id, more } => {
      tracks(client::call(Request::Playlist { playlist_id, more }).await?)
    }
    CliCommand::Play { video_id } => control(Command::Play { video_id }).await,
    CliCommand::PlayRadio { video_id } => {
      // The daemon builds the queue; the seed only needs its ID to start.
      let seed = Track {
        title: video_id.clone(),
        video_id,
        artists: Vec::new(),
        album: None,
        duration: None,
        thumbnails: Vec::new(),
        watch: None,
      };
      control(Command::StartRadio { seed }).await
    }
    CliCommand::PlayPlaylist { playlist_id } => {
      control(Command::StartPlaylist { playlist_id }).await
    }
    CliCommand::Pause => control(Command::Pause).await,
    CliCommand::Resume => control(Command::Resume).await,
    CliCommand::Toggle => control(Command::Toggle).await,
    CliCommand::Next => control(Command::Next).await,
    CliCommand::Prev => control(Command::Previous).await,
    CliCommand::Stop => control(Command::Stop).await,
    CliCommand::Queue => match client::call(Request::Status).await? {
      Response::Status(status) => {
        print_player(&status.player, true);
        Ok(())
      }
      other => unexpected(other),
    },
    CliCommand::Download { video_id, output } => {
      let output = output.unwrap_or_else(|| format!("{video_id}.mp4").into());
      // The daemon has its own working directory.
      let output = std::path::absolute(output)?;
      match client::call(Request::Download { video_id, output: output.clone() }).await? {
        Response::Ok => {
          println!("saved {}", output.display());
          Ok(())
        }
        other => unexpected(other),
      }
    }
  }
}

/// Sends a player command and waits for the daemon to accept it.
async fn control(command: Command) -> Result<()> {
  match client::call(Request::Control(command)).await? {
    Response::Ok => Ok(()),
    other => unexpected(other),
  }
}

fn unexpected(response: Response) -> Result<()> {
  Err(format!("the daemon sent an unexpected response: {response:?}").into())
}

fn tracks(response: Response) -> Result<()> {
  match response {
    Response::Tracks(tracks) => {
      tracks.iter().for_each(print_track);
      Ok(())
    }
    other => unexpected(other),
  }
}

fn print_track(track: &Track) {
  let artists = track.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>();
  let duration = track.duration.as_deref().map(|d| format!(" {d}")).unwrap_or_default();
  println!("  {} -- {} [{}]{duration}", track.title, artists.join(", "), track.video_id);
}

fn print_status(status: &st_proto::ipc::StatusReport) {
  println!("steak daemon {} (up {})", status.version, duration(status.uptime_secs));
  print_player(&status.player, false);
  print_session(&status.session);
  println!("caches:");
  for cache in &status.caches {
    print_cache(cache);
  }
}

fn print_player(player: &PlayerView, queue: bool) {
  let title = player.current().map_or("nothing".to_owned(), |track| {
    let artists = track.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>();
    format!("{} -- {}", track.title, artists.join(", "))
  });
  match &player.transport {
    Transport::Idle => println!("playback: idle"),
    Transport::Loading => println!("playback: starting {title}"),
    Transport::Playing => println!("playback: playing {title} ({})", clock(player.position_ms)),
    Transport::Paused => println!("playback: paused {title} ({})", clock(player.position_ms)),
    Transport::Ended => println!("playback: finished {title}"),
    Transport::Failed(error) => println!("playback: failed to play {title}: {error}"),
  }
  if queue {
    for (index, track) in player.queue.iter().enumerate() {
      let marker = if index == player.index { ">" } else { " " };
      print!("{marker}");
      print_track(track);
    }
  } else if !player.queue.is_empty() {
    println!("  track {} of {} in the queue", player.index + 1, player.queue.len());
  }
}

fn clock(ms: u64) -> String { format!("{}:{:02}", ms / 60_000, ms / 1000 % 60) }

fn print_session(session: &SessionReport) {
  println!("session: {}", session.state);
  if let Some(detail) = &session.detail {
    println!("  {detail}");
  }
  if let Some(source) = &session.source {
    println!("  imports from {source}");
  }
  if let Some(secs) = session.last_import_secs {
    println!("  last import {} ago", duration(secs));
  }
  if let Some(secs) = session.next_auto_repair_secs {
    println!("  next automatic repair in {}", duration(secs));
  }
}

fn print_cache(cache: &CellReport) {
  let age = cache.age_secs.map(|secs| format!(", {} old", duration(secs))).unwrap_or_default();
  println!("  {}: {}{age}", cache.name, cache.state);
  if let Some(error) = &cache.last_error {
    let retry = cache.retry_in_secs.map(|secs| format!(", retry in {}", duration(secs)));
    println!(
      "    last error: {error} ({} failures{})",
      cache.consecutive_failures,
      retry.unwrap_or_default()
    );
  }
}

fn duration(secs: u64) -> String {
  match secs {
    0..60 => format!("{secs}s"),
    60..3600 => format!("{}m", secs / 60),
    _ => format!("{}h{}m", secs / 3600, secs % 3600 / 60),
  }
}
