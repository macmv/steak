//! Saving a track's audio to a file with `ffmpeg`.

use std::{path::Path, process::Stdio, time::Duration};

use futures_util::StreamExt;
use tokio::{io::AsyncWriteExt, process::Command};

use super::api::AudioChunks;

/// How long the network may go without delivering the next chunk.
const FETCH_STALL: Duration = Duration::from_secs(30);
/// How long `ffmpeg` may go without accepting more input.
const WRITE_STALL: Duration = Duration::from_secs(60);

/// Transcodes `audio` into an mp4 file at `output`.
///
/// The audio is re-encoded to AAC, because the stream may be Opus/WebM, which
/// isn't reliably playable inside an mp4. A partial file is removed on failure.
pub async fn save(mut audio: AudioChunks, output: &Path) -> Result<(), String> {
  let result = transcode(&mut audio, output).await;
  if result.is_err() {
    let _ = std::fs::remove_file(output);
  }
  result
}

async fn transcode(audio: &mut AudioChunks, output: &Path) -> Result<(), String> {
  let mut ffmpeg = Command::new("ffmpeg")
    .args(["-y", "-loglevel", "error", "-i", "pipe:0", "-vn", "-c:a", "aac", "-b:a", "256k"])
    .args(["-f", "mp4"])
    .arg(output)
    .stdin(Stdio::piped())
    .kill_on_drop(true)
    .spawn()
    .map_err(|error| match error.kind() {
      std::io::ErrorKind::NotFound => {
        "ffmpeg was not found; install FFmpeg (for example `sudo pacman -S ffmpeg`)".to_owned()
      }
      _ => format!("could not start ffmpeg: {error}"),
    })?;
  let mut stdin = ffmpeg.stdin.take().expect("ffmpeg stdin was piped");

  loop {
    let next = tokio::time::timeout(FETCH_STALL, audio.next())
      .await
      .map_err(|_| "the audio download stalled".to_owned())?;
    let Some(chunk) = next else { break };
    let chunk = chunk.map_err(|error| format!("the audio download failed: {error}"))?;
    match tokio::time::timeout(WRITE_STALL, stdin.write_all(&chunk)).await {
      Ok(Ok(())) => {}
      Ok(Err(error)) => return Err(format!("ffmpeg stopped accepting audio ({error})")),
      Err(_) => return Err("ffmpeg stopped accepting audio".into()),
    }
  }
  drop(stdin); // EOF tells ffmpeg there is no more audio.

  let status = ffmpeg.wait().await.map_err(|error| format!("waiting for ffmpeg: {error}"))?;
  status.success().then_some(()).ok_or_else(|| format!("ffmpeg exited with {status}"))
}
