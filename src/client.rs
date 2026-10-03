//! The CLI side of the daemon protocol.

use std::{error::Error, io::ErrorKind, time::Duration};

use tokio::{
  io::{AsyncReadExt, AsyncWriteExt},
  net::UnixStream,
};

use crate::{
  daemon::paths,
  ipc::{Request, Response},
};

/// The daemon bounds every request itself (the longest is 200s); this only
/// guards against a daemon that is not answering at all.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(230);

/// Sends one request to the daemon. A `Response::Error` becomes an `Err`.
pub async fn call(request: Request) -> Result<Response, Box<dyn Error + Send + Sync>> {
  let socket = paths::socket();
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

  let mut line = serde_json::to_vec(&request)?;
  line.push(b'\n');
  stream.write_all(&line).await?;
  stream.shutdown().await?;

  let mut reply = Vec::new();
  tokio::time::timeout(RESPONSE_TIMEOUT, stream.read_to_end(&mut reply))
    .await
    .map_err(|_| "the daemon did not answer")??;
  match serde_json::from_slice(&reply)? {
    Response::Error(message) => Err(message.into()),
    response => Ok(response),
  }
}
