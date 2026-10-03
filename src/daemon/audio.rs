//! Playing an encoded audio stream through `ffplay`.

use std::{process::Stdio, time::Duration};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use log::{debug, warn};
use tokio::{io::AsyncWriteExt, process::Command, sync::mpsc, task::JoinHandle};

/// How long the network may go without delivering the next chunk.
const FETCH_STALL: Duration = Duration::from_secs(30);
/// How long `ffplay` may go without accepting more input. It only blocks this
/// long if it is wedged: its input buffer drains at playback speed.
const WRITE_STALL: Duration = Duration::from_secs(60);
/// Chunks the fetch task may run ahead of playback.
const READ_AHEAD: usize = 64;

/// Aborts a task when dropped, so a cancelled playback cannot leave its fetch
/// task behind.
pub struct AbortOnDrop<T>(pub JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
  fn drop(&mut self) { self.0.abort(); }
}

/// Plays `audio` through `ffplay` and returns when it has finished.
///
/// `ffplay` probes the bytes itself, so any container/codec it supports works
/// (WebM/Opus, MP4/AAC, MP3, ...). `on_started` runs once the player process
/// exists, with its process ID.
///
/// This future can be dropped at any time to stop playback: the `ffplay`
/// process is killed with it, and so is the fetch task. Nothing in it can wait
/// forever except the tail of the audio itself: network stalls and a wedged
/// player both time out.
pub async fn play_stream<S, E>(audio: S, on_started: impl FnOnce(Option<u32>)) -> Result<(), String>
where
  S: Stream<Item = Result<Bytes, E>> + Send + 'static,
  E: std::fmt::Display + Send + 'static,
{
  let mut player = Command::new("ffplay")
    .args(["-nodisp", "-autoexit", "-loglevel", "error", "-i", "pipe:0"])
    .stdin(Stdio::piped())
    .kill_on_drop(true)
    .spawn()
    .map_err(|error| match error.kind() {
      std::io::ErrorKind::NotFound => {
        "ffplay was not found; install FFmpeg (for example `sudo pacman -S ffmpeg`)".to_owned()
      }
      _ => format!("could not start ffplay: {error}"),
    })?;
  let mut stdin = player.stdin.take().expect("ffplay stdin was piped");
  on_started(player.id());

  // The fetch task runs ahead of ffplay by a bounded amount; `send` waits once
  // the buffer is full, which is what applies backpressure to the download.
  let (sender, mut chunks) = mpsc::channel::<Result<Bytes, String>>(READ_AHEAD);
  let _fetch = AbortOnDrop(tokio::spawn(async move {
    futures_util::pin_mut!(audio);
    loop {
      let next = match tokio::time::timeout(FETCH_STALL, audio.next()).await {
        Ok(next) => next,
        Err(_) => {
          let _ = sender.send(Err("the audio download stalled".into())).await;
          return;
        }
      };
      let Some(chunk) = next else { return };
      let chunk = chunk.map_err(|error| format!("the audio download failed: {error}"));
      let failed = chunk.is_err();
      if sender.send(chunk).await.is_err() || failed {
        return;
      }
    }
  }));

  let mut played = 0_u64;
  while let Some(chunk) = chunks.recv().await {
    let chunk = chunk?;
    played += chunk.len() as u64;
    match tokio::time::timeout(WRITE_STALL, stdin.write_all(&chunk)).await {
      Ok(Ok(())) => {}
      Ok(Err(error)) => {
        // ffplay died or closed its input; its exit status says why.
        let status = player.try_wait().ok().flatten();
        return Err(format!("ffplay stopped accepting audio ({error}); exit status {status:?}"));
      }
      Err(_) => return Err("ffplay stopped accepting audio".into()),
    }
  }
  drop(stdin); // EOF tells ffplay there is no more audio.

  let status = player.wait().await.map_err(|error| format!("waiting for ffplay: {error}"))?;
  debug!("ffplay exited with {status} after {played} bytes");
  if !status.success() {
    warn!("ffplay exited with {status}");
    return Err(format!("ffplay exited with {status}"));
  }
  Ok(())
}
