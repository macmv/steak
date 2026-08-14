use std::{error::Error, process::Stdio};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
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
async fn main() {
  // Example:
  // let stream = audio_client.stream(audio_request).await?;
  // play(stream).await?;
}
