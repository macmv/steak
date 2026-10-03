//! Streaming access to signed audio media URLs.

use bytes::Bytes;
use futures_core::Stream;
use futures_util::StreamExt;
use log::debug;
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
    // State is the next offset to fetch; `None` once finished or failed.
    let chunks = futures_util::stream::unfold(Some(0_u64), move |offset| {
      let client = client.clone();
      let request = request.clone();
      async move {
        let start = offset.filter(|start| *start < total)?;
        let end = (start + CHUNK_SIZE).min(total) - 1;
        let mut request = request;
        request.byte_range = Some(start..=end);
        let response = client
          .request(request)
          .await
          .and_then(|response| response.error_for_status().map_err(|error| error.without_url()));
        match response {
          Ok(response) => Some((Ok(response.bytes_stream()), Some(end + 1))),
          Err(error) => Some((Err(error), None)),
        }
      }
    });
    Ok(Box::pin(chunks.flat_map(|chunk| match chunk {
      Ok(body) => body.left_stream(),
      Err(error) => futures_util::stream::once(std::future::ready(Err(error))).right_stream(),
    })))
  }
}

/// Returns whether `status` denotes a successful full or byte-range audio
/// response.
pub fn is_audio_success(status: StatusCode) -> bool {
  status == StatusCode::OK || status == StatusCode::PARTIAL_CONTENT
}
