//! Bootstrap data obtained from the YouTube Music web application.
//!
//! The bootstrapper fetches the Music landing page and extracts its current
//! Innertube configuration and preserves anonymous cookies from that response.

use crate::innertube::client::MusicClientConfig;
use log::{debug, warn};
use reqwest::{Client, header};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Values that change with the Music web application deployment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
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
  /// The player script URL from the Music page config, if it advertised one.
  pub fn script_url(&self) -> Option<reqwest::Url> {
    self.player_script_url.as_deref().and_then(|url| reqwest::Url::parse(url).ok())
  }

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
  /// Creates a bootstrapper that presents a desktop browser user agent.
  pub fn new(http: Client) -> Self {
    Self::with_user_agent(
      http,
      header::HeaderValue::from_static(
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
      ),
    )
  }

  pub fn with_user_agent(http: Client, user_agent: header::HeaderValue) -> Self {
    Self { http, user_agent }
  }

  /// Fetches a watch page when the landing page does not carry the player
  /// script URL. The player script is the same for every video, so no video ID
  /// is needed.
  pub async fn player_script_url(&self) -> Result<Option<reqwest::Url>, BootstrapError> {
    debug!("fetching YouTube Music watch page for player JavaScript");
    let response = self
      .http
      .get("https://music.youtube.com/watch")
      .header(header::USER_AGENT, &self.user_agent)
      .send()
      .await
      .map_err(BootstrapError::Request)?;
    debug!("watch page response status={}", response.status());
    let response = response.error_for_status().map_err(BootstrapError::Request)?;
    let html = response.text().await.map_err(BootstrapError::Request)?;
    debug!("received YouTube Music watch page ({} bytes)", html.len());
    let script_url = embedded_string_field(&html, "PLAYER_JS_URL")
      .or_else(|| embedded_string_field(&html, "JS_URL"))
      .or_else(|| player_path_in_html(&html))
      .map(|value| normalize_player_url(&value))
      .transpose()?
      .map(|url| reqwest::Url::parse(&url).map_err(|_| BootstrapError::InvalidConfig))
      .transpose()?;
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
    return Ok(format!("https://www.youtube.com{value}"));
  }
  Err(BootstrapError::InvalidConfig)
}

fn embedded_string_field(html: &str, field: &str) -> Option<String> {
  let marker = "ytcfg.set(";
  let mut remainder = html;
  while let Some(offset) = remainder.find(marker) {
    let after_marker = &remainder[offset + marker.len()..];
    if let Some(object) = balanced_object(after_marker) {
      if let Ok(config) = serde_json::from_str::<Value>(object) {
        if let Some(value) = config.get(field).and_then(Value::as_str) {
          return Some(value.to_owned());
        }
      }
    }
    remainder = after_marker;
  }
  None
}

fn player_path_in_html(html: &str) -> Option<String> {
  let start = html.find("/s/player/")?;
  let tail = &html[start..];
  let end = tail.find(|character: char| matches!(character, '"' | '\'' | '\\' | '?' | '#'))?;
  Some(tail[..end].to_owned())
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
      Some("https://www.youtube.com/s/player/a/base.js")
    );
  }
}
