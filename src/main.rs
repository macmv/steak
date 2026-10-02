use std::{error::Error, process::Stdio};

use bytes::Bytes;
use clap::{Args, Parser, Subcommand};
use futures_util::{Stream, StreamExt};
use log::{debug, error, info, warn};
use st_api::{
  innertube::{
    bootstrap::{Bootstrapper, WebBootstrapper},
    client::{MusicClient, MusicClientConfig},
  },
  media::audio::{AudioClient, AudioRequest},
  playback::{
    player_script::{EjsCipherResolver, StreamUrl},
    response::{AudioQuality, PlayerParser, StandardPlayerParser},
  },
};
use tokio::{io::AsyncWriteExt, process::Command, sync::mpsc};

#[derive(Debug, Parser)]
#[command(about = "Play a YouTube video's audio track")]
struct Cli {
  /// YouTube video ID to play.
  #[arg(value_name = "YOUTUBE_VIDEO_ID")]
  video_id: Option<String>,

  #[command(subcommand)]
  command: Option<CliCommand>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
  /// Manage the authenticated YouTube session used for playback.
  Auth(AuthCommand),
}

#[derive(Debug, Args)]
struct AuthCommand {
  #[command(subcommand)]
  command: AuthSubcommand,
}

#[derive(Debug, Subcommand)]
enum AuthSubcommand {
  /// Import YouTube cookies from a Firefox profile.
  ImportFirefox {
    /// Firefox profile directory containing cookies.sqlite.
    firefox_profile: std::path::PathBuf,
    /// Where to save the imported session (defaults to Steak's session path).
    session_file:    Option<std::path::PathBuf>,
  },
}

/// Plays a compressed audio byte stream through Linux's `ffplay` utility.
///
/// `ffplay` probes the bytes itself, so this supports the usual stream
/// container/codec combinations (for example WebM/Opus, MP4/AAC, and MP3)
/// without the application having to know the format up front.  Install it
/// with your distribution's FFmpeg package (e.g. `sudo apt install ffmpeg`).
///
/// A fetch task stays up to 64 chunks ahead of the playback task. The bounded
/// queue both provides modest read-ahead and prevents an unlimited amount of
/// audio from accumulating in memory. `ffplay` exits when the input ends.
pub async fn play<S, E>(audio: S) -> Result<(), Box<dyn Error + Send + Sync>>
where
  S: Stream<Item = Result<Bytes, E>> + Send + 'static,
  E: Error + Send + Sync + 'static,
{
  // Keep network reads independent from writes to the player. If ffplay falls
  // behind, `send` waits once this buffer is full and applies backpressure.
  let (sender, mut chunks) = mpsc::channel::<Result<Bytes, E>>(64);
  let fetch_task = tokio::spawn(async move {
    debug!("audio fetch task started");
    futures_util::pin_mut!(audio);
    while let Some(chunk) = audio.next().await {
      match &chunk {
        Ok(bytes) => debug!("fetched {} encoded audio bytes", bytes.len()),
        Err(_) => warn!("audio fetch task received a stream error"),
      }
      if sender.send(chunk).await.is_err() {
        debug!("playback receiver closed; stopping audio fetch");
        break; // Playback ended, so stop downloading.
      }
    }
    debug!("audio fetch task finished");
  });

  debug!("starting ffplay process");
  let mut player = Command::new("ffplay")
    .args(["-nodisp", "-autoexit", "-loglevel", "error", "-i", "pipe:0"])
    .stdin(Stdio::piped())
    .spawn()?;
  let mut stdin = player.stdin.take().expect("ffplay stdin was piped");
  let mut played_bytes = 0_u64;

  while let Some(chunk) = chunks.recv().await {
    match chunk {
      Ok(chunk) => {
        played_bytes += chunk.len() as u64;
        debug!("writing {} bytes to ffplay ({} total)", chunk.len(), played_bytes);
        stdin.write_all(&chunk).await?;
      }
      Err(error) => {
        error!("audio stream failed: {error}");
        let _ = player.kill().await;
        let _ = fetch_task.await;
        return Err(error.into());
      }
    }
  }
  drop(stdin); // EOF tells ffplay there are no more encoded bytes.

  fetch_task.await?;
  let status = player.wait().await?;
  debug!("ffplay exited with {status} after receiving {played_bytes} bytes");
  if !status.success() {
    return Err("ffplay exited unsuccessfully".into());
  }
  Ok(())
}

fn setup_logging() -> Result<(), log::SetLoggerError> {
  let level = std::env::var("RUST_LOG")
    .ok()
    .and_then(|value| value.parse().ok())
    .unwrap_or(log::LevelFilter::Debug);
  fern::Dispatch::new()
    .level(level)
    .format(|out, message, record| {
      out.finish(format_args!("[{} {}] {message}", record.level(), record.target()))
    })
    .chain(std::io::stderr())
    .apply()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
  setup_logging()?;
  let cli = Cli::parse();
  if let Some(CliCommand::Auth(auth)) = cli.command {
    return match auth.command {
      AuthSubcommand::ImportFirefox { firefox_profile, session_file } => {
        auth_command(&firefox_profile, session_file)
      }
    };
  };
  let video_id =
    cli.video_id.ok_or("a YouTube video ID is required; run `steak --help` for usage")?;
  info!("starting playback request for YouTube video {video_id}");

  // The API key and client version change with the Music web app, so retrieve
  // them rather than baking a stale value into the executable.
  let bootstrapper = WebBootstrapper::default();
  let bootstrap = bootstrapper.bootstrap().await?;
  debug!("received current YouTube Music client configuration");
  let mut config = MusicClientConfig::default();
  bootstrap.apply_to(&mut config);
  let session =
    st_api::account::session::StoredSession::load(&st_api::account::session::session_path())?;
  config.headers.extend(session.auth_session().request_headers());

  let player_response = MusicClient::new(config).player(&video_id).await?;
  debug!("received player response for {video_id}");
  let parser = StandardPlayerParser;
  let format =
    parser.choose_audio(parser.audio_formats(&player_response)?, AudioQuality::Balanced)?;
  let stream_url = match format.stream_url {
    // Direct URLs are short-lived, authorized media URLs returned by YouTube.
    StreamUrl::Direct(url) => url,
    StreamUrl::SignatureCipher(cipher) => {
      let stream = StreamUrl::SignatureCipher(cipher);
      let script_url = if let Some(url) = StandardPlayerParser::player_script_url(&player_response)
      {
        debug!("using player JavaScript URL from the player response");
        Some(url)
      } else {
        debug!("player response did not advertise a JavaScript URL; fetching the watch page");
        let url = bootstrapper.player_script_url(&video_id).await?;
        if url.is_some() {
          debug!("using player JavaScript URL from the watch page");
        } else {
          warn!("watch page did not advertise PLAYER_JS_URL for {video_id}");
        }
        url
      };
      let script_url = script_url.ok_or("no player JavaScript URL was advertised")?;
      EjsCipherResolver::new().resolve(&script_url, &stream).await?
    }
  };

  info!("playing {video_id}: {} at {} bps", format.mime_type, format.bitrate);
  let audio = AudioClient::default().stream(AudioRequest::new(stream_url)).await?;
  play(audio).await
}

fn auth_command(
  firefox_profile: &std::path::Path,
  output: Option<std::path::PathBuf>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
  let session = st_api::account::session::import_firefox_profile(firefox_profile)?;
  let output = output.unwrap_or_else(st_api::account::session::session_path);
  session.save(&output)?;
  eprintln!("Imported YouTube session to {}. Keep it private.", output.display());
  Ok(())
}
