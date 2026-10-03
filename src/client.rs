//! The client side of the daemon protocol, for the CLI and the TUI.
//!
//! This is the only way either of them reaches the outside world: a Unix
//! socket to the daemon. Nothing here knows about YouTube.

use std::{error::Error, io::ErrorKind, time::Duration};

use st_proto::ipc::{PlayerView, Request, Response, socket_path};
use tokio::{
  io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines},
  net::{
    UnixStream,
    unix::{OwnedReadHalf, OwnedWriteHalf},
  },
};

pub type BoxError = Box<dyn Error + Send + Sync>;

/// The daemon bounds every request itself (the longest short one is 200s); this
/// only guards against a daemon that is not answering at all.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(230);
/// Saving a track can take as long as the download does.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(31 * 60);

async fn connect(request: &Request) -> Result<UnixStream, BoxError> {
  let socket = socket_path();
  let connect = tokio::time::timeout(Duration::from_secs(3), UnixStream::connect(&socket)).await;
  let mut stream = match connect {
    Ok(Ok(stream)) => stream,
    Ok(Err(error))
      if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) =>
    {
      return Err(
        "the steak daemon is not running. Start it with `steak daemon`, or run `steak daemon \
         install` to have it start at login."
          .into(),
      );
    }
    Ok(Err(error)) => return Err(format!("could not reach the daemon: {error}").into()),
    Err(_) => return Err("the daemon did not accept the connection".into()),
  };
  let mut line = serde_json::to_vec(request)?;
  line.push(b'\n');
  stream.write_all(&line).await?;
  Ok(stream)
}

/// Sends one request to the daemon. A `Response::Error` becomes an `Err`.
pub async fn call(request: Request) -> Result<Response, BoxError> {
  let limit = match request {
    Request::Download { .. } => DOWNLOAD_TIMEOUT,
    _ => RESPONSE_TIMEOUT,
  };
  let mut stream = connect(&request).await?;
  stream.shutdown().await?;

  let mut reply = Vec::new();
  tokio::time::timeout(limit, stream.read_to_end(&mut reply))
    .await
    .map_err(|_| "the daemon did not answer")??;
  match serde_json::from_slice(&reply)? {
    Response::Error(message) => Err(message.into()),
    response => Ok(response),
  }
}

/// A live feed of the player's state.
pub struct Subscription {
  lines:   Lines<BufReader<OwnedReadHalf>>,
  /// Held so the daemon sees the connection as open until this is dropped.
  _writer: OwnedWriteHalf,
}

/// Starts watching the player. The first `next` is its current state.
pub async fn subscribe() -> Result<Subscription, BoxError> {
  let stream = connect(&Request::Subscribe).await?;
  let (reader, writer) = stream.into_split();
  Ok(Subscription { lines: BufReader::new(reader).lines(), _writer: writer })
}

impl Subscription {
  /// The next state of the player. An error means the connection is gone, and
  /// the caller should subscribe again.
  pub async fn next(&mut self) -> Result<PlayerView, BoxError> {
    let line = self.lines.next_line().await?.ok_or("the daemon closed the connection")?;
    match serde_json::from_str(&line)? {
      Response::Player(view) => Ok(view),
      Response::Error(message) => Err(message.into()),
      other => Err(format!("the daemon sent an unexpected response: {other:?}").into()),
    }
  }
}
