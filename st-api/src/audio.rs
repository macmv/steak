//! Streaming access to signed audio media URLs.

use bytes::Bytes;
use futures_core::Stream;
use reqwest::{Client, Response, StatusCode, Url, header};

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
  /// Creates a client using `user_agent` for audio requests.
  pub fn new(user_agent: header::HeaderValue) -> Self { Self { client: Client::new(), user_agent } }

  /// Creates a client with the supplied reqwest client and user agent.
  pub fn with_client(client: Client, user_agent: header::HeaderValue) -> Self {
    Self { client, user_agent }
  }

  /// Sends the audio request and returns its HTTP response.
  ///
  /// Use [`Self::stream`] when only the body stream is needed. The caller must
  /// retain/use the response status and headers if it needs range metadata.
  pub async fn request(&self, request: AudioRequest) -> reqwest::Result<Response> {
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
    builder.send().await
  }

  /// Sends an audio request and yields body chunks as they arrive.
  ///
  /// The returned stream yields an error for network or body-decoding failures.
  pub async fn stream(
    &self,
    request: AudioRequest,
  ) -> reqwest::Result<impl Stream<Item = reqwest::Result<Bytes>> + use<>> {
    Ok(self.request(request).await?.bytes_stream())
  }
}

impl Default for AudioClient {
  fn default() -> Self { Self::new(header::HeaderValue::from_static("st-api/0.1")) }
}

/// Returns whether `status` denotes a successful full or byte-range audio
/// response.
pub fn is_audio_success(status: StatusCode) -> bool {
  status == StatusCode::OK || status == StatusCode::PARTIAL_CONTENT
}
