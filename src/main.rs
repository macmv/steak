mod tui;

use std::{
  error::Error,
  pin::Pin,
  process::Stdio,
  sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
  },
};

use bytes::Bytes;
use clap::{Args, Parser, Subcommand};
use futures_util::{Stream, StreamExt};
use log::{debug, error, info, warn};
use st_api::{
  Url,
  cache::Cache,
  catalog::{HomeParser, PlaylistParser, QueueParser, ShelfItem, Track},
  innertube::{
    bootstrap::{Bootstrapper, WebBootstrapper},
    client::{MusicClient, MusicClientConfig},
  },
  media::audio::{AudioClient, AudioRequest},
  playback::{
    access::po_token::authorize_stream,
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
  #[command(subcommand)]
  command: Option<CliCommand>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
  /// Manage the authenticated YouTube session used for playback.
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

  Play {
    /// YouTube video ID to play.
    video_id: String,
  },
  /// Download a video's audio track as an mp4 file, using `ffmpeg`.
  Download {
    /// YouTube video ID to download.
    video_id: String,
    /// Output file (defaults to `<video_id>.mp4`).
    #[arg(short, long)]
    output:   Option<std::path::PathBuf>,
  },
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
///
/// With `quiet`, ffplay's own output is discarded (it would corrupt a TUI), and
/// its process ID is published to `pid` so the caller can pause it.
pub async fn play<S, E>(
  audio: S,
  quiet: bool,
  pid: &AtomicU32,
) -> Result<(), Box<dyn Error + Send + Sync>>
where
  S: Stream<Item = Result<Bytes, E>> + Send + 'static,
  E: Error + Send + Sync + 'static,
{
  pipe_to_ffmpeg(
    "ffplay",
    &[
      "-nodisp".as_ref(),
      "-autoexit".as_ref(),
      "-loglevel".as_ref(),
      "error".as_ref(),
      "-i".as_ref(),
      "pipe:0".as_ref(),
    ],
    audio,
    quiet,
    pid,
  )
  .await
}

/// Transcodes a compressed audio byte stream into an mp4 file with `ffmpeg`.
///
/// The audio is re-encoded to AAC, because the stream may be Opus/WebM, which
/// isn't reliably playable inside an mp4.
pub async fn download<S, E>(
  audio: S,
  output: &std::path::Path,
) -> Result<(), Box<dyn Error + Send + Sync>>
where
  S: Stream<Item = Result<Bytes, E>> + Send + 'static,
  E: Error + Send + Sync + 'static,
{
  let args: [&std::ffi::OsStr; 12] = [
    "-y".as_ref(),
    "-loglevel".as_ref(),
    "error".as_ref(),
    "-i".as_ref(),
    "pipe:0".as_ref(),
    "-vn".as_ref(),
    "-c:a".as_ref(),
    "aac".as_ref(),
    "-b:a".as_ref(),
    "256k".as_ref(),
    "-f".as_ref(),
    "mp4".as_ref(),
  ];
  let mut all = args.to_vec();
  all.push(output.as_os_str());
  pipe_to_ffmpeg("ffmpeg", &all, audio, false, &AtomicU32::new(0)).await
}

/// Feeds `audio` to the stdin of `program` (an FFmpeg tool) and waits for it
/// to exit.
async fn pipe_to_ffmpeg<S, E>(
  program: &str,
  args: &[&std::ffi::OsStr],
  audio: S,
  quiet: bool,
  pid: &AtomicU32,
) -> Result<(), Box<dyn Error + Send + Sync>>
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

  debug!("starting {program} process");
  let mut command = Command::new(program);
  command
    .args(args)
    .stdin(Stdio::piped())
    // Dropping this future (to skip a track) must not leave the process running.
    .kill_on_drop(true);
  if quiet {
    command.stdout(Stdio::null()).stderr(Stdio::null());
  }
  let mut player = command.spawn()?;
  pid.store(player.id().unwrap_or(0), Ordering::SeqCst);
  let mut stdin = player.stdin.take().expect("stdin was piped");
  let mut played_bytes = 0_u64;

  while let Some(chunk) = chunks.recv().await {
    match chunk {
      Ok(chunk) => {
        played_bytes += chunk.len() as u64;
        debug!("writing {} bytes to {program} ({} total)", chunk.len(), played_bytes);
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
  drop(stdin); // EOF tells the process there are no more encoded bytes.

  fetch_task.await?;
  let status = player.wait().await?;
  pid.store(0, Ordering::SeqCst);
  debug!("{program} exited with {status} after receiving {played_bytes} bytes");
  if !status.success() {
    return Err(format!("{program} exited unsuccessfully").into());
  }
  Ok(())
}

fn setup_logging(to_file: bool) -> Result<(), Box<dyn Error + Send + Sync>> {
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
    .chain(if to_file {
      // The TUI owns the terminal, so logs go to a file instead.
      let dir = Cache::open().dir().to_owned();
      std::fs::create_dir_all(&dir)?;
      fern::Output::from(fern::log_file(dir.join("steak.log"))?)
    } else {
      fern::Output::from(std::io::stderr())
    })
    .apply()?;
  Ok(())
}

/// Resolves `video_id` to a signed audio URL.
///
/// The web app config, signature timestamp and challenge solver are the same
/// for every video, so they come from the cache when present. Anything missing
/// is rebuilt the slow way and saved.
async fn resolve_stream(
  http: &st_api::Client,
  cache: &Cache,
  video_id: &str,
) -> Result<Url, Box<dyn Error + Send + Sync>> {
  let bootstrapper = WebBootstrapper::new(http.clone());
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
  let resolver = EjsCipherResolver::new(http.clone());
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

  let player_response = MusicClient::new(http.clone(), config)
    .player_with_signature_timestamp(video_id, signature_timestamp)
    .await?;
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

/// Resolves `video_id` to an audio URL that googlevideo will serve in full.
async fn authorized_stream(
  http: &st_api::Client,
  video_id: &str,
) -> Result<(AudioClient, Url), Box<dyn Error + Send + Sync>> {
  let cache = Cache::open();
  let populated = cache.is_populated();
  let stream_url = match resolve_stream(http, &cache, video_id).await {
    Ok(url) => url,
    // Something cached no longer works (YouTube rotated the player, say). Drop
    // all of it and rebuild from scratch, once.
    Err(error) if populated => {
      warn!("cached state failed ({error}); clearing it and starting over");
      cache.clear();
      resolve_stream(http, &cache, video_id).await?
    }
    Err(error) => return Err(error),
  };

  // googlevideo refuses a stream past its first chunk unless it carries a
  // valid PoToken. This is outside the cache fallback above on purpose: a
  // PoToken failure says nothing about the cache.
  let audio_client = AudioClient::new(http.clone());
  let stream_url = authorize_stream(&audio_client, stream_url, video_id).await?;
  Ok((audio_client, stream_url))
}

type AudioStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// Resolves `video_id` and opens its audio as a byte stream.
async fn open_audio(
  http: &st_api::Client,
  video_id: &str,
) -> Result<AudioStream, Box<dyn Error + Send + Sync>> {
  let (audio_client, stream_url) = authorized_stream(http, video_id).await?;
  Ok(audio_client.stream_chunked(AudioRequest::new(stream_url)).await?)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
  let cli = Cli::parse();
  setup_logging(cli.command.is_none())?;
  let http = st_api::Client::new();
  match cli.command {
    Some(CliCommand::Auth(auth)) => {
      return match auth.command {
        AuthSubcommand::ImportFirefox { firefox_profile, session_file } => {
          auth_command(&firefox_profile, session_file)
        }
      };
    }

    Some(CliCommand::Home { more }) => return home_command(&http, more).await,
    Some(CliCommand::Radio { video_id, more }) => {
      return radio_command(&http, &video_id, more).await;
    }
    Some(CliCommand::Playlist { playlist_id, more }) => {
      return playlist_command(&http, &playlist_id, more).await;
    }
    Some(CliCommand::Play { video_id }) => {
      info!("starting playback request for YouTube video {video_id}");
      let (audio_client, stream_url) = authorized_stream(&http, &video_id).await?;
      let audio = audio_client.stream_chunked(AudioRequest::new(stream_url)).await?;
      play(audio, false, &AtomicU32::new(0)).await?;
    }
    Some(CliCommand::Download { video_id, output }) => {
      info!("starting download request for YouTube video {video_id}");
      let output = output.unwrap_or_else(|| format!("{video_id}.mp4").into());
      let (audio_client, stream_url) = authorized_stream(&http, &video_id).await?;
      let audio = audio_client.stream_chunked(AudioRequest::new(stream_url)).await?;
      download(audio, &output).await?;
      info!("saved {}", output.display());
    }
    None => return tui::run(http).await,
  }

  Ok(())
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

/// Builds an authenticated Music client from the cached (or fresh) web app
/// configuration and the stored session.
pub async fn music_client(
  http: &st_api::Client,
  cache: &Cache,
) -> Result<MusicClient, Box<dyn Error + Send + Sync>> {
  let bootstrap = match cache.bootstrap() {
    Some(bootstrap) => bootstrap,
    None => {
      let bootstrap = WebBootstrapper::new(http.clone()).bootstrap().await?;
      cache.store_bootstrap(&bootstrap);
      bootstrap
    }
  };
  let mut config = MusicClientConfig::default();
  bootstrap.apply_to(&mut config);
  let session =
    st_api::account::session::StoredSession::load(&st_api::account::session::session_path())?;
  config.headers.extend(session.auth_session().request_headers());
  Ok(MusicClient::new(http.clone(), config))
}

async fn home_command(
  http: &st_api::Client,
  more: u32,
) -> Result<(), Box<dyn Error + Send + Sync>> {
  let cache = Cache::open();
  let client = music_client(&http, &cache).await?;
  let parser = HomeParser;

  let mut response = client.browse("FEmusic_home", None).await?;
  if !parser.logged_in(&response) {
    warn!(
      "YouTube treated this session as signed out, so the homepage is generic; re-import your \
       session with `steak auth import-firefox`"
    );
  }
  for page in 0..=more {
    for shelf in parser.shelves(&response) {
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
    if page == more {
      break;
    }
    let Some(token) = parser.continuation(&response) else { break };
    response = client.browse_continuation(token).await?;
  }
  Ok(())
}

fn print_track(track: &Track) {
  let artists = track.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>();
  let duration = track.duration.as_deref().map(|d| format!(" {d}")).unwrap_or_default();
  println!("  {} -- {} [{}]{duration}", track.title, artists.join(", "), track.video_id);
}

/// Lists the song radio for `video_id`: YouTube's `RDAMVM` playlist, which the
/// web app opens when you start a mix from a song.
async fn radio_command(
  http: &st_api::Client,
  video_id: &str,
  more: u32,
) -> Result<(), Box<dyn Error + Send + Sync>> {
  let cache = Cache::open();
  let client = music_client(http, &cache).await?;
  let parser = QueueParser;

  let mut response = client.next(video_id, Some(format!("RDAMVM{video_id}"))).await?;
  for page in 0..=more {
    let tracks = parser.tracks(&response);
    if tracks.is_empty() && page == 0 {
      return Err(format!("no radio found for {video_id}").into());
    }
    tracks.iter().for_each(print_track);
    if page == more {
      break;
    }
    let Some(token) = parser.continuation(&response) else { break };
    response = client.next_continuation(token).await?;
  }
  Ok(())
}

async fn playlist_command(
  http: &st_api::Client,
  playlist_id: &str,
  more: u32,
) -> Result<(), Box<dyn Error + Send + Sync>> {
  let cache = Cache::open();
  let client = music_client(http, &cache).await?;
  let parser = PlaylistParser;

  // Browse IDs are the playlist ID behind a `VL` prefix.
  let playlist_id = playlist_id.strip_prefix("VL").unwrap_or(playlist_id);
  let mut response = client.browse(format!("VL{playlist_id}"), None).await?;
  for page in 0..=more {
    let tracks = parser.tracks(&response);
    if tracks.is_empty() && page == 0 {
      return Err(format!("no tracks found for playlist {playlist_id}").into());
    }
    tracks.iter().for_each(print_track);
    if page == more {
      break;
    }
    let Some(token) = parser.continuation(&response) else { break };
    response = client.browse_continuation(token).await?;
  }
  Ok(())
}
