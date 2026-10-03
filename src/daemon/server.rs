//! The control socket.
//!
//! One request per connection (see `ipc.rs`). Every request runs in its own
//! task under a deadline, so a slow or panicking request costs exactly one
//! connection: the accept loop, the other requests and the state machines never
//! notice.

use std::{future::Future, path::Path, sync::Arc, time::Duration};

use log::{debug, info, warn};
use tokio::{
  io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
  net::{
    UnixListener, UnixStream,
    unix::{OwnedReadHalf, OwnedWriteHalf},
  },
  sync::Semaphore,
};

use super::Daemon;
use st_proto::ipc::{Request, Response};

/// Requests are one short line; anything bigger is not a client of ours.
const MAX_REQUEST_BYTES: u64 = 64 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONNECTIONS: usize = 64;

/// How long each kind of request may run before it is answered with an error.
fn deadline(request: &Request) -> Duration {
  match request {
    Request::Ping | Request::Status | Request::Control(_) | Request::Subscribe => {
      Duration::from_secs(10)
    }
    Request::Art { .. } => Duration::from_secs(30),
    Request::Home { .. } | Request::Radio { .. } | Request::Playlist { .. } => {
      Duration::from_secs(90)
    }
    Request::Download { .. } => Duration::from_secs(30 * 60),
    Request::AuthImport { .. } | Request::AuthRepair => Duration::from_secs(150),
  }
}

/// Binds the control socket. Fails if another daemon is already serving it; a
/// socket left behind by a dead daemon is replaced.
pub async fn bind(path: &Path) -> std::io::Result<UnixListener> {
  use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

  if let Some(parent) = path.parent() {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
  }
  let listener = match UnixListener::bind(path) {
    Ok(listener) => listener,
    Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
      let alive = tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(path)).await;
      if matches!(alive, Ok(Ok(_))) {
        return Err(std::io::Error::new(
          std::io::ErrorKind::AddrInUse,
          format!("another steak daemon is already running on {}", path.display()),
        ));
      }
      info!("removing the stale socket {}", path.display());
      std::fs::remove_file(path)?;
      UnixListener::bind(path)?
    }
    Err(error) => return Err(error),
  };
  std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
  Ok(listener)
}

/// Serves until `shutdown` resolves.
pub async fn serve(
  listener: UnixListener,
  daemon: Arc<Daemon>,
  shutdown: impl Future<Output = ()>,
) {
  let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
  tokio::pin!(shutdown);
  loop {
    tokio::select! {
      _ = &mut shutdown => return,
      accepted = listener.accept() => match accepted {
        Ok((stream, _)) => {
          let Ok(slot) = slots.clone().try_acquire_owned() else {
            warn!("too many connections; dropping one");
            continue;
          };
          let daemon = daemon.clone();
          tokio::spawn(async move {
            connection(stream, daemon).await;
            drop(slot);
          });
        }
        // Out of file descriptors and the like. Back off and keep serving.
        Err(error) => {
          warn!("accept failed: {error}");
          tokio::time::sleep(Duration::from_millis(200)).await;
        }
      },
    }
  }
}

async fn connection(stream: UnixStream, daemon: Arc<Daemon>) {
  let (reader, mut writer) = stream.into_split();
  let mut reader = BufReader::new(reader);
  let mut line = String::new();
  let read =
    tokio::time::timeout(READ_TIMEOUT, (&mut reader).take(MAX_REQUEST_BYTES).read_line(&mut line))
      .await;
  let response = match read {
    Ok(Ok(0)) | Err(_) => return, // Connected and said nothing.
    Ok(Err(error)) => Response::Error(format!("could not read the request: {error}")),
    Ok(Ok(_)) => match serde_json::from_str::<Request>(&line) {
      Ok(Request::Subscribe) => return subscribe(reader, writer, daemon).await,
      Ok(request) => run(request, daemon).await,
      Err(error) => Response::Error(format!("malformed request: {error}")),
    },
  };
  if write_response(&mut writer, &response).await {
    let _ = writer.shutdown().await;
  }
}

/// Writes one response line. False if the client is gone or stopped reading.
async fn write_response(writer: &mut OwnedWriteHalf, response: &Response) -> bool {
  let bytes = match serde_json::to_vec(response) {
    Ok(mut bytes) => {
      bytes.push(b'\n');
      bytes
    }
    Err(error) => {
      format!("{{\"Error\":\"could not encode the response: {error}\"}}\n").into_bytes()
    }
  };
  match tokio::time::timeout(WRITE_TIMEOUT, writer.write_all(&bytes)).await {
    Ok(Ok(())) => true,
    // The client went away (Ctrl-C on a slow command, say). Nothing to do.
    Ok(Err(error)) => {
      debug!("client went away before the response: {error}");
      false
    }
    Err(_) => {
      debug!("client did not read the response");
      false
    }
  }
}

/// Streams the player's view until the client goes away.
///
/// The view comes from a `watch` channel, so a client that reads slowly skips
/// the snapshots it missed instead of making anyone wait or buffering them; a
/// client that stops reading altogether hits the write timeout and is dropped.
async fn subscribe(
  mut reader: BufReader<OwnedReadHalf>,
  mut writer: OwnedWriteHalf,
  daemon: Arc<Daemon>,
) {
  debug!("a client subscribed to the player");
  let mut view = daemon.player.subscribe();
  let mut ignored = String::new();
  loop {
    let snapshot = view.borrow_and_update().current();
    if !write_response(&mut writer, &Response::Player(snapshot)).await {
      return;
    }
    tokio::select! {
      changed = view.changed() => if changed.is_err() { return },
      // The client said something more, or hung up. A subscriber has nothing
      // more to say, so either way it is done.
      _ = reader.read_line(&mut ignored) => return,
    }
  }
}

/// Runs one request in an isolated task under its deadline.
async fn run(request: Request, daemon: Arc<Daemon>) -> Response {
  let limit = deadline(&request);
  let name = format!("{request:?}");
  debug!("request: {name}");
  let mut task = tokio::spawn(async move { daemon.handle(request).await });
  match tokio::time::timeout(limit, &mut task).await {
    Ok(Ok(response)) => response,
    Ok(Err(error)) => {
      warn!("request {name} crashed: {error}");
      Response::Error("internal error: the request crashed (see the daemon log)".into())
    }
    Err(_) => {
      task.abort();
      warn!("request {name} exceeded {}s", limit.as_secs());
      Response::Error(format!("the request took longer than {}s", limit.as_secs()))
    }
  }
}
