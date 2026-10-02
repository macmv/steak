use std::{error::Error, process::Stdio, sync::Arc};

use bytes::Bytes;
use clap::{Args, Parser, Subcommand};
use futures_util::{Stream, StreamExt};
use log::{debug, error, info, warn};
use st_api::{
  Url,
  cache::Cache,
  innertube::{
    bootstrap::{Bootstrapper, WebBootstrapper},
    client::{MusicClient, MusicClientConfig},
  },
  media::audio::{AudioClient, AudioRequest},
  playback::{
    access::po_token::{BotguardCliProvider, authorize_stream},
    player_script::{
      EjsCipherResolver, player_cache_key, resolve_and_preprocess, resolve_with_solver,
      signature_timestamp,
    },
    response::{AudioQuality, PlayerParser, StandardPlayerParser},
    solver::Solver,
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

/// Resolves `video_id` to a signed audio URL.
///
/// The web app config, signature timestamp and challenge solver are the same
/// for every video, so they come from the cache when present. Anything missing
/// is rebuilt the slow way and saved.
async fn resolve_stream(
  cache: &Cache,
  video_id: &str,
) -> Result<Url, Box<dyn Error + Send + Sync>> {
  let bootstrapper = WebBootstrapper::default();
  let bootstrap = match cache.bootstrap() {
    Some(bootstrap) => {
      debug!("using cached YouTube Music client configuration");
      bootstrap
    }
    None => {
      // The API key and client version change with the Music web app, so
      // retrieve them rather than baking a stale value into the executable.
      let mut bootstrap = bootstrapper.bootstrap().await?;
      debug!("received current YouTube Music client configuration");
      if bootstrap.player_script_url.is_none() {
        debug!("Music page did not advertise a JavaScript URL; fetching the watch page");
        let url = bootstrapper.player_script_url(video_id).await?;
        bootstrap.player_script_url = url.map(String::from);
      }
      if bootstrap.player_script_url.is_none() {
        warn!("no player JavaScript URL was advertised for {video_id}");
      }
      cache.store_bootstrap(&bootstrap);
      bootstrap
    }
  };
  let script_url = bootstrap.script_url().ok_or("no player JavaScript URL was advertised")?;

  let mut config = MusicClientConfig::default();
  bootstrap.apply_to(&mut config);
  let session =
    st_api::account::session::StoredSession::load(&st_api::account::session::session_path())?;
  config.headers.extend(session.auth_session().request_headers());

  // The player request must name the player script's signature timestamp. With
  // a cached solver, load it in the background while the request is in flight;
  // otherwise download the full script and solve from it after the request.
  let resolver = EjsCipherResolver::new();
  let (signature_timestamp, solve) = match (cache.signature_timestamp(), cache.solver()) {
    (Some(timestamp), Some(solver)) => {
      debug!("using cached solver for signature timestamp {timestamp}");
      (Some(timestamp), Solve::Cached(Solver::spawn(solver.into())))
    }
    _ => {
      let script = resolver.script(&script_url).await?;
      let timestamp = signature_timestamp(&script);
      match timestamp {
        Some(timestamp) => {
          debug!("player signature timestamp is {timestamp}");
          cache.store_signature_timestamp(timestamp);
        }
        None => warn!("no signature timestamp; the player response may not match the script"),
      }
      (timestamp, Solve::Script(script))
    }
  };

  let player_response =
    MusicClient::new(config).player_with_signature_timestamp(video_id, signature_timestamp).await?;
  debug!("received player response for {video_id}");
  if let (Solve::Cached(_), Some(url)) =
    (&solve, StandardPlayerParser::player_script_url(&player_response))
  {
    if player_cache_key(&url) != player_cache_key(&script_url) {
      return Err("the player script changed since the cache was written".into());
    }
  }
  let parser = StandardPlayerParser;
  let format =
    parser.choose_audio(parser.audio_formats(&player_response)?, AudioQuality::Balanced)?;
  let stream_url = match solve {
    Solve::Cached(solver) => resolve_with_solver(&format.stream_url, &solver).await?,
    Solve::Script(script) => {
      let (url, preprocessed) = resolve_and_preprocess(script, &format.stream_url).await?;
      if let Some(preprocessed) = preprocessed {
        cache.store_solver(&preprocessed);
      }
      url
    }
  };

  info!("playing {video_id}: {} at {} bps", format.mime_type, format.bitrate);
  Ok(stream_url)
}

enum Solve {
  /// A solver loaded from the cache.
  Cached(Solver),
  /// The full player script, which must be solved the slow way.
  Script(Arc<str>),
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

  let cache = Cache::open();
  let populated = cache.is_populated();
  let stream_url = match resolve_stream(&cache, &video_id).await {
    Ok(url) => url,
    // Something cached no longer works (YouTube rotated the player, say). Drop
    // all of it and rebuild from scratch, once.
    Err(error) if populated => {
      warn!("cached state failed ({error}); clearing it and starting over");
      cache.clear();
      resolve_stream(&cache, &video_id).await?
    }
    Err(error) => return Err(error),
  };

  // googlevideo refuses a stream past its first chunk unless it carries a
  // valid PoToken. This is outside the cache fallback above on purpose: a
  // PoToken failure says nothing about the cache.
  let audio_client = AudioClient::default();
  let botguard = match std::env::var_os("STEAK_BOTGUARD") {
    Some(binary) => BotguardCliProvider::new(binary),
    None => BotguardCliProvider::default(),
  };
  let stream_url = authorize_stream(&botguard, &audio_client, stream_url, &video_id).await?;
  let audio = audio_client.stream_chunked(AudioRequest::new(stream_url)).await?;
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
