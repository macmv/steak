//! Bootstrap data obtained from the YouTube Music web application.
//!
//! The bootstrapper fetches the Music landing page and extracts its current
//! Innertube configuration. It does not log a user in or persist cookies.

use crate::music::MusicClientConfig;
use reqwest::{Client, header};
use serde_json::Value;

/// Values that change with the Music web application deployment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BootstrapData {
  pub api_key:           String,
  pub client_version:    String,
  pub visitor_data:      Option<String>,
  pub player_script_url: Option<String>,
}

impl BootstrapData {
  /// Applies the request-related values to a Music client configuration.
  pub fn apply_to(&self, config: &mut MusicClientConfig) {
    config.api_key = Some(self.api_key.clone());
    config.context.client_version = self.client_version.clone();
    config.context.visitor_data = self.visitor_data.clone();
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
    Ok(BootstrapData { api_key, client_version, visitor_data, player_script_url })
  }
}

impl Default for WebBootstrapper {
  fn default() -> Self { Self::new(header::HeaderValue::from_static("st-api/0.1")) }
}

impl Bootstrapper for WebBootstrapper {
  fn bootstrap(
    &self,
  ) -> impl std::future::Future<Output = Result<BootstrapData, BootstrapError>> + Send {
    async move {
      let html = self
        .http
        .get("https://music.youtube.com/")
        .header(header::USER_AGENT, &self.user_agent)
        .header(header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
        .send()
        .await
        .map_err(BootstrapError::Request)?
        .error_for_status()
        .map_err(BootstrapError::Request)?
        .text()
        .await
        .map_err(BootstrapError::Request)?;
      Self::parse(&html)
    }
  }
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
