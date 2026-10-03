//! Streaming access to signed audio media URLs.

use bytes::Bytes;
use futures_core::Stream;
use futures_util::StreamExt;
use log::{debug, warn};
use reqwest::{Client, Response, StatusCode, Url, header};
use std::pin::Pin;

/// Inputs for an audio request.
#[derive(Clone, Debug)]
pub struct AudioRequest {
  /// A time-limited, signed media URL obtained through an authorized source.
  pub url:        Url,
  /// Optional inclusive byte range to fetch.
  pub byte_range: Option<std::ops::RangeInclusive<u64>>,
  /// Any additional headers required by the media endpoint.
  pub headers:    header::HeaderMap,
}

impl AudioRequest {
  pub fn new(url: Url) -> Self { Self { url, byte_range: None, headers: header::HeaderMap::new() } }
}

/// An HTTP client for streaming audio from a supplied signed URL.
#[derive(Clone, Debug)]
pub struct AudioClient {
  client:     Client,
  user_agent: header::HeaderValue,
}

impl AudioClient {
  /// Creates a client that sends audio requests through `client`.
  pub fn new(client: Client) -> Self {
    Self::with_user_agent(client, header::HeaderValue::from_static("st-api/0.1"))
  }

  /// Creates a client with the supplied reqwest client and user agent.
  pub fn with_user_agent(client: Client, user_agent: header::HeaderValue) -> Self {
    Self { client, user_agent }
  }

  /// Sends the audio request and returns its HTTP response.
  ///
  /// Use [`Self::stream`] when only the body stream is needed. The caller must
  /// retain/use the response status and headers if it needs range metadata.
  pub async fn request(&self, request: AudioRequest) -> reqwest::Result<Response> {
    debug!(
      "requesting audio from host {:?}, byte range {:?}",
      request.url.host_str(),
      request.byte_range
    );
    let mut builder = self
      .client
      .get(request.url)
      .header(header::USER_AGENT, &self.user_agent)
      .headers(request.headers)
      .header(header::ORIGIN, "https://music.youtube.com")
      .header(header::REFERER, "https://music.youtube.com/");

    if let Some(byte_range) = request.byte_range {
      builder =
        builder.header(header::RANGE, format!("bytes={}-{}", byte_range.start(), byte_range.end()));
    }
    let response = builder.send().await?;
    debug!(
      "audio response status={}, content-length={:?}",
      response.status(),
      response.content_length()
    );
    Ok(response)
  }

  /// Requests the single byte at `offset` and returns the response status.
  ///
  /// Used to check that a URL is authorized for the whole stream without
  /// downloading any of it.
  pub async fn probe_status(&self, url: &Url, offset: u64) -> reqwest::Result<StatusCode> {
    let mut request = AudioRequest::new(url.clone());
    request.byte_range = Some(offset..=offset);
    Ok(self.request(request).await.map_err(|e| e.without_url())?.status())
  }

  /// Sends an audio request and yields body chunks as they arrive.
  ///
  /// Fails up front on an HTTP error status, so a rejected URL (for example a
  /// 403) is reported instead of looking like an empty stream. The returned
  /// stream yields an error for network or body-decoding failures.
  pub async fn stream(
    &self,
    request: AudioRequest,
  ) -> reqwest::Result<impl Stream<Item = reqwest::Result<Bytes>> + use<>> {
    // The URL carries signed, short-lived credentials, so keep it out of
    // errors.
    let response = self.request(request).await?.error_for_status().map_err(|e| e.without_url())?;
    Ok(response.bytes_stream())
  }
}

/// Size of each ranged request made by [`AudioClient::stream_chunked`].
const CHUNK_SIZE: u64 = 4 * 1024 * 1024;

impl AudioClient {
  /// Like [`Self::stream`], but fetches the media in consecutive ranged
  /// requests, the way the web player does.
  ///
  /// googlevideo throttles and eventually resets one long unranged download,
  /// while ranged chunks are served at full speed. The total size comes from
  /// the URL's `clen` parameter; without it this falls back to
  /// [`Self::stream`].
  pub async fn stream_chunked(
    &self,
    request: AudioRequest,
  ) -> reqwest::Result<Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>> {
    let total = request
      .url
      .query_pairs()
      .find(|(key, _)| key == "clen")
      .and_then(|(_, value)| value.parse::<u64>().ok())
      .filter(|total| *total > 0);
    let Some(total) = total.filter(|_| request.byte_range.is_none()) else {
      return Ok(Box::pin(self.stream(request).await?));
    };

    let client = self.clone();
    let state = ChunkState { offset: 0, body: None, retries: 0 };
    let stream = futures_util::stream::unfold(state, move |mut state| {
      let client = client.clone();
      let request = request.clone();
      async move {
        loop {
          if let Some(body) = state.body.as_mut() {
            match body.next().await {
              Some(Ok(bytes)) => {
                state.offset += bytes.len() as u64;
                state.retries = 0;
                return Some((Ok(bytes), state));
              }
              Some(Err(error)) => {
                state.body = None;
                if !state.should_retry(&error) {
                  state.offset = total;
                  return Some((Err(error.without_url()), state));
                }
                warn!("audio body failed at byte {} ({error}); resuming", state.offset);
                backoff(state.retries).await;
              }
              // Chunk finished; the next iteration requests the following one.
              None => state.body = None,
            }
            continue;
          }

          if state.offset >= total {
            return None;
          }
          let end = (state.offset + CHUNK_SIZE).min(total) - 1;
          let mut request = request.clone();
          request.byte_range = Some(state.offset..=end);
          let response = client
            .request(request)
            .await
            .and_then(|response| response.error_for_status().map_err(|e| e.without_url()));
          match response {
            Ok(response) => state.body = Some(response.bytes_stream().boxed()),
            Err(error) if state.should_retry(&error) => {
              warn!("audio request failed at byte {} ({error}); retrying", state.offset);
              backoff(state.retries).await;
            }
            Err(error) => {
              state.offset = total;
              return Some((Err(error), state));
            }
          }
        }
      }
    });
    Ok(Box::pin(stream))
  }
}

/// Maximum consecutive failures tolerated before giving up on a stream.
const MAX_RETRIES: u32 = 5;

struct ChunkState {
  /// Next byte to deliver; resuming after a failure restarts here.
  offset:  u64,
  body:    Option<Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>>,
  /// Consecutive failures since the last byte was received.
  retries: u32,
}

impl ChunkState {
  /// Records a failure and returns whether it is worth retrying. HTTP error
  /// statuses (a rejected URL) are final; network and decoding errors are not.
  fn should_retry(&mut self, error: &reqwest::Error) -> bool {
    self.retries += 1;
    error.status().is_none() && self.retries <= MAX_RETRIES
  }
}

async fn backoff(retries: u32) {
  tokio::time::sleep(std::time::Duration::from_millis(250 * u64::from(retries))).await;
}

/// Returns whether `status` denotes a successful full or byte-range audio
/// response.
pub fn is_audio_success(status: StatusCode) -> bool {
  status == StatusCode::OK || status == StatusCode::PARTIAL_CONTENT
}
