use std::{error::Error, process::Stdio};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use st_api::{
  audio::{AudioClient, AudioRequest},
  bootstrap::{Bootstrapper, WebBootstrapper},
  cipher::StreamUrl,
  music::{MusicClient, MusicClientConfig},
  player::{AudioQuality, PlayerParser, StandardPlayerParser},
};
use tokio::{io::AsyncWriteExt, process::Command, sync::mpsc};

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
    futures_util::pin_mut!(audio);
    while let Some(chunk) = audio.next().await {
      if sender.send(chunk).await.is_err() {
        break; // Playback ended, so stop downloading.
      }
    }
  });

  let mut player = Command::new("ffplay")
    .args(["-nodisp", "-autoexit", "-loglevel", "error", "-i", "pipe:0"])
    .stdin(Stdio::piped())
    .spawn()?;
  let mut stdin = player.stdin.take().expect("ffplay stdin was piped");

  while let Some(chunk) = chunks.recv().await {
    match chunk {
      Ok(chunk) => stdin.write_all(&chunk).await?,
      Err(error) => {
        let _ = player.kill().await;
        let _ = fetch_task.await;
        return Err(error.into());
      }
    }
  }
  drop(stdin); // EOF tells ffplay there are no more encoded bytes.

  fetch_task.await?;
  if !player.wait().await?.success() {
    return Err("ffplay exited unsuccessfully".into());
  }
  Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
  let args: Vec<_> = std::env::args_os().collect();
  if args.get(1).and_then(|value| value.to_str()) == Some("auth") {
    return auth_command(&args[2..]);
  }
  let (video_id, session_path) = match args.as_slice() {
    [_, video_id] => (video_id.to_string_lossy().into_owned(), None),
    [_, flag, path, video_id] if flag == "--session" => {
      (video_id.to_string_lossy().into_owned(), Some(std::path::PathBuf::from(path)))
    }
    _ => return Err("usage: steak [--session <session-file>] <youtube-video-id>".into()),
  };

  // The API key and client version change with the Music web app, so retrieve
  // them rather than baking a stale value into the executable.
  let bootstrap = WebBootstrapper::default().bootstrap().await?;
  let mut config = MusicClientConfig::default();
  bootstrap.apply_to(&mut config);
  if let Some(path) = session_path {
    let session = st_api::auth::StoredSession::load(&path)?;
    config.headers.extend(session.auth_session().request_headers());
  }

  let player_response = MusicClient::new(config).player(&video_id).await?;
  let parser = StandardPlayerParser;
  let format =
    parser.choose_audio(parser.audio_formats(&player_response)?, AudioQuality::Balanced)?;
  let stream_url = match format.stream_url {
    // Direct URLs are short-lived, authorized media URLs returned by YouTube.
    StreamUrl::Direct(url) => url,
    StreamUrl::SignatureCipher(_) => {
      return Err(
        "YouTube returned a ciphered media URL; player-script deciphering is not wired up yet"
          .into(),
      );
    }
  };

  eprintln!("playing {video_id}: {} at {} bps", format.mime_type, format.bitrate);
  let audio = AudioClient::default().stream(AudioRequest::new(stream_url)).await?;
  play(audio).await
}

fn auth_command(args: &[std::ffi::OsString]) -> Result<(), Box<dyn Error + Send + Sync>> {
  if args.first().and_then(|value| value.to_str()) != Some("import-firefox") || args.len() != 3 {
    return Err("usage: steak auth import-firefox <firefox-profile> <session-file>".into());
  }
  let session = st_api::auth::import_firefox_profile(std::path::Path::new(&args[1]))?;
  session.save(std::path::Path::new(&args[2]))?;
  eprintln!("Imported YouTube session. Keep the session file private.");
  Ok(())
}
