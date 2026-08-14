//! Bootstrap data obtained from the YouTube Music web application.
//!
//! The bootstrapper fetches the Music landing page and extracts its current
//! Innertube configuration and preserves anonymous cookies from that response.

use crate::music::MusicClientConfig;
use log::{debug, warn};
use reqwest::{Client, header};
use serde_json::{Map, Value};

/// Values that change with the Music web application deployment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BootstrapData {
  pub api_key:           String,
  pub client_version:    String,
  pub visitor_data:      Option<String>,
  pub player_script_url: Option<String>,
  /// The complete non-cookie request context supplied by the Music web page.
  pub request_context:   Map<String, Value>,
  /// Anonymous response cookies, suitable for the immediately following API
  /// requests. This is not an authenticated browser session.
  pub cookies:           Option<String>,
}

impl BootstrapData {
  /// Applies the request-related values to a Music client configuration.
  pub fn apply_to(&self, config: &mut MusicClientConfig) {
    config.api_key = Some(self.api_key.clone());
    config.context.client_version = self.client_version.clone();
    config.context.visitor_data = self.visitor_data.clone();
    let mut request_context = self.request_context.clone();
    config.client_context = request_context
      .remove("client")
      .and_then(|client| client.as_object().cloned())
      .unwrap_or_default();
    config.request_context = request_context;
    if let Some(cookies) = &self.cookies {
      if let Ok(value) = cookies.parse() {
        config.headers.insert(header::COOKIE, value);
      }
    }
  }
}

/// A source capable of retrieving current bootstrap data.
pub trait Bootstrapper {
  fn bootstrap(
    &self,
  ) -> impl std::future::Future<Output = Result<BootstrapData, BootstrapError>> + Send;
}

/// A bootstrap fetch or configuration-extraction failure.
#[derive(Debug)]
pub enum BootstrapError {
  Request(reqwest::Error),
  Json(serde_json::Error),
  MissingField(&'static str),
  InvalidConfig,
}

impl std::fmt::Display for BootstrapError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Request(error) => write!(f, "bootstrap request failed: {error}"),
      Self::Json(error) => write!(f, "bootstrap JSON was invalid: {error}"),
      Self::MissingField(field) => write!(f, "bootstrap configuration is missing {field}"),
      Self::InvalidConfig => f.write_str("bootstrap configuration is invalid"),
    }
  }
}

impl std::error::Error for BootstrapError {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    match self {
      Self::Request(error) => Some(error),
      Self::Json(error) => Some(error),
      _ => None,
    }
  }
}

/// Fetches bootstrap data from the public YouTube Music landing page.
#[derive(Clone, Debug)]
pub struct WebBootstrapper {
  http:       Client,
  user_agent: header::HeaderValue,
}

impl WebBootstrapper {
  pub fn new(user_agent: header::HeaderValue) -> Self { Self { http: Client::new(), user_agent } }

  pub fn with_http_client(http: Client, user_agent: header::HeaderValue) -> Self {
    Self { http, user_agent }
  }

  /// Fetches a watch page when the player response does not carry its script
  /// URL.
  pub async fn player_script_url(
    &self,
    video_id: &str,
  ) -> Result<Option<reqwest::Url>, BootstrapError> {
    debug!("fetching YouTube Music watch page for player JavaScript (video {video_id})");
    let response = self
      .http
      .get("https://music.youtube.com/watch")
      .query(&[("v", video_id)])
      .header(header::USER_AGENT, &self.user_agent)
      .send()
      .await
      .map_err(BootstrapError::Request)?;
    debug!("watch page response status={}", response.status());
    let response = response.error_for_status().map_err(BootstrapError::Request)?;
    let html = response.text().await.map_err(BootstrapError::Request)?;
    debug!("received YouTube Music watch page ({} bytes)", html.len());
    let config = extract_config(&html)?;
    let script_url = config
      .get("PLAYER_JS_URL")
      .and_then(Value::as_str)
      .map(normalize_player_url)
      .transpose()
      .map(|url| url.and_then(|url| reqwest::Url::parse(&url).ok()))?;
    if script_url.is_none() {
      warn!("watch-page ytcfg has no usable PLAYER_JS_URL");
    }
    Ok(script_url)
  }

  /// Parses HTML previously fetched from `https://music.youtube.com/`.
  pub fn parse(html: &str) -> Result<BootstrapData, BootstrapError> {
    let config = extract_config(html)?;
    let api_key = string_field(&config, "INNERTUBE_API_KEY")?;
    let client_version = config
      .pointer("/INNERTUBE_CONTEXT/client/clientVersion")
      .and_then(Value::as_str)
      .map(str::to_owned)
      .or_else(|| config.get("INNERTUBE_CLIENT_VERSION").and_then(Value::as_str).map(str::to_owned))
      .ok_or(BootstrapError::MissingField("INNERTUBE_CLIENT_VERSION"))?;
    let visitor_data = config.get("VISITOR_DATA").and_then(Value::as_str).map(str::to_owned);
    let player_script_url =
      config.get("PLAYER_JS_URL").and_then(Value::as_str).map(normalize_player_url).transpose()?;
    let request_context = config.as_object().cloned().ok_or(BootstrapError::InvalidConfig)?;
    Ok(BootstrapData {
      api_key,
      client_version,
      visitor_data,
      player_script_url,
      request_context,
      cookies: None,
    })
  }
}

impl Default for WebBootstrapper {
  fn default() -> Self {
    Self::new(header::HeaderValue::from_static(
      "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
    ))
  }
}

impl Bootstrapper for WebBootstrapper {
  fn bootstrap(
    &self,
  ) -> impl std::future::Future<Output = Result<BootstrapData, BootstrapError>> + Send {
    async move {
      debug!("fetching YouTube Music bootstrap page");
      let response = self
        .http
        .get("https://music.youtube.com/")
        .header(header::USER_AGENT, &self.user_agent)
        .header(header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
        .send()
        .await
        .map_err(BootstrapError::Request)?
        .error_for_status()
        .map_err(BootstrapError::Request)?;
      let cookies = response_cookies(response.headers());
      let html = response.text().await.map_err(BootstrapError::Request)?;
      debug!("received YouTube Music bootstrap page ({} bytes)", html.len());
      let mut data = Self::parse(&html)?;
      data.cookies = cookies;
      Ok(data)
    }
  }
}

fn response_cookies(headers: &header::HeaderMap) -> Option<String> {
  let cookies: Vec<_> = headers
    .get_all(header::SET_COOKIE)
    .iter()
    .filter_map(|value| value.to_str().ok())
    .filter_map(|value| value.split(';').next())
    .filter(|value| !value.is_empty())
    .collect();
  (!cookies.is_empty()).then(|| cookies.join("; "))
}

fn string_field(config: &Value, field: &'static str) -> Result<String, BootstrapError> {
  config
    .get(field)
    .and_then(Value::as_str)
    .map(str::to_owned)
    .ok_or(BootstrapError::MissingField(field))
}

fn normalize_player_url(value: &str) -> Result<String, BootstrapError> {
  if value.starts_with("https://") {
    return Ok(value.into());
  }
  if value.starts_with('/') {
    return Ok(format!("https://music.youtube.com{value}"));
  }
  Err(BootstrapError::InvalidConfig)
}

fn extract_config(html: &str) -> Result<Value, BootstrapError> {
  let marker = "ytcfg.set(";
  let mut remainder = html;
  while let Some(offset) = remainder.find(marker) {
    let after_marker = &remainder[offset + marker.len()..];
    if let Some(object) = balanced_object(after_marker) {
      let config: Value = serde_json::from_str(object).map_err(BootstrapError::Json)?;
      if config.get("INNERTUBE_API_KEY").is_some() {
        return Ok(config);
      }
    }
    remainder = after_marker;
  }
  Err(BootstrapError::MissingField("ytcfg"))
}

fn balanced_object(input: &str) -> Option<&str> {
  let start = input.find('{')?;
  let bytes = input.as_bytes();
  let mut depth = 0_u32;
  let mut escaped = false;
  let mut quoted = false;
  for (index, byte) in bytes.iter().enumerate().skip(start) {
    if quoted {
      if escaped {
        escaped = false;
      } else if *byte == b'\\' {
        escaped = true;
      } else if *byte == b'"' {
        quoted = false;
      }
      continue;
    }
    match byte {
      b'"' => quoted = true,
      b'{' => depth += 1,
      b'}' => {
        depth -= 1;
        if depth == 0 {
          return input.get(start..=index);
        }
      }
      _ => {}
    }
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_embedded_ytcfg() {
    let data = WebBootstrapper::parse(r#"<script>ytcfg.set({"INNERTUBE_API_KEY":"key","INNERTUBE_CONTEXT":{"client":{"clientVersion":"1.2"}},"VISITOR_DATA":"visitor","PLAYER_JS_URL":"/s/player/a/base.js"});</script>"#).unwrap();
    assert_eq!(data.api_key, "key");
    assert_eq!(data.client_version, "1.2");
    assert_eq!(
      data.player_script_url.as_deref(),
      Some("https://music.youtube.com/s/player/a/base.js")
    );
  }
}
