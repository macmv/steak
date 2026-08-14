//! Client for the YouTube Music Innertube HTTP protocol.
//!
//! This module only sends JSON requests and returns raw JSON responses. Account
//! authentication, player-response deciphering, and conversion from renderer
//! trees into application models intentionally belong in higher layers.

use reqwest::{Client, Url, header};
use serde::Serialize;
use serde_json::{Map, Value, json};

const BASE_URL: &str = "https://music.youtube.com/youtubei/v1/";

/// The Innertube endpoint to invoke.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Endpoint {
  Search,
  SearchSuggestions,
  Browse,
  Next,
  Player,
  Queue,
  PlaylistEdit,
}

impl Endpoint {
  fn path(self) -> &'static str {
    match self {
      Self::Search => "search",
      Self::SearchSuggestions => "music/get_search_suggestions",
      Self::Browse => "browse",
      Self::Next => "next",
      Self::Player => "player",
      Self::Queue => "music/get_queue",
      Self::PlaylistEdit => "playlist/edit",
    }
  }
}

/// Identifies the web client represented in each Innertube request.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientContext {
  pub client_name:    String,
  pub client_version: String,
  pub hl:             String,
  pub gl:             String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub visitor_data:   Option<String>,
}

impl Default for ClientContext {
  fn default() -> Self {
    Self {
      client_name:    "WEB_REMIX".into(),
      // Deliberately configurable: this identifier changes frequently.
      client_version: "1.20260101.00.00".into(),
      hl:             "en".into(),
      gl:             "US".into(),
      visitor_data:   None,
    }
  }
}

/// Configuration for [`MusicClient`].
#[derive(Clone, Debug)]
pub struct MusicClientConfig {
  pub context:    ClientContext,
  pub api_key:    Option<String>,
  pub user_agent: header::HeaderValue,
  /// Cookies and authorization headers for authenticated requests.
  pub headers:    header::HeaderMap,
}

impl Default for MusicClientConfig {
  fn default() -> Self {
    Self {
      context:    ClientContext::default(),
      api_key:    None,
      user_agent: header::HeaderValue::from_static("st-api/0.1"),
      headers:    header::HeaderMap::new(),
    }
  }
}

/// A raw Innertube client.
#[derive(Clone, Debug)]
pub struct MusicClient {
  http:   Client,
  config: MusicClientConfig,
}

impl MusicClient {
  pub fn new(config: MusicClientConfig) -> Self { Self::with_http_client(Client::new(), config) }

  pub fn with_http_client(http: Client, config: MusicClientConfig) -> Self { Self { http, config } }

  /// Calls an endpoint, adding the required `context` member to `payload`.
  pub async fn call(
    &self,
    endpoint: Endpoint,
    mut payload: Map<String, Value>,
  ) -> reqwest::Result<Value> {
    payload.insert("context".into(), json!({ "client": self.config.context }));

    let mut url = Url::parse(BASE_URL).expect("valid YouTube Music base URL");
    url.path_segments_mut().expect("base URL cannot be a base").push(endpoint.path());
    {
      let mut query = url.query_pairs_mut();
      query.append_pair("prettyPrint", "false");
      if let Some(key) = &self.config.api_key {
        query.append_pair("key", key);
      }
    }

    let mut request = self
      .http
      .post(url)
      .header(header::USER_AGENT, &self.config.user_agent)
      .header(header::ORIGIN, "https://music.youtube.com")
      .header(header::REFERER, "https://music.youtube.com/")
      .headers(self.config.headers.clone())
      .json(&payload);
    if let Some(visitor_data) = &self.config.context.visitor_data {
      request = request.header("X-Goog-Visitor-Id", visitor_data);
    }
    request.send().await?.error_for_status()?.json().await
  }

  pub async fn search(
    &self,
    query: impl Into<String>,
    params: Option<String>,
  ) -> reqwest::Result<Value> {
    let mut body = Map::from_iter([(String::from("query"), Value::String(query.into()))]);
    if let Some(params) = params {
      body.insert("params".into(), Value::String(params));
    }
    self.call(Endpoint::Search, body).await
  }

  pub async fn search_suggestions(&self, input: impl Into<String>) -> reqwest::Result<Value> {
    self
      .call(
        Endpoint::SearchSuggestions,
        Map::from_iter([(String::from("input"), Value::String(input.into()))]),
      )
      .await
  }

  pub async fn browse(
    &self,
    browse_id: impl Into<String>,
    params: Option<String>,
  ) -> reqwest::Result<Value> {
    let mut body = Map::from_iter([(String::from("browseId"), Value::String(browse_id.into()))]);
    if let Some(params) = params {
      body.insert("params".into(), Value::String(params));
    }
    self.call(Endpoint::Browse, body).await
  }

  pub async fn next(
    &self,
    video_id: impl Into<String>,
    playlist_id: Option<String>,
  ) -> reqwest::Result<Value> {
    let mut body = Map::from_iter([(String::from("videoId"), Value::String(video_id.into()))]);
    if let Some(playlist_id) = playlist_id {
      body.insert("playlistId".into(), Value::String(playlist_id));
    }
    self.call(Endpoint::Next, body).await
  }

  pub async fn player(&self, video_id: impl Into<String>) -> reqwest::Result<Value> {
    self
      .call(
        Endpoint::Player,
        Map::from_iter([(String::from("videoId"), Value::String(video_id.into()))]),
      )
      .await
  }
}

impl Default for MusicClient {
  fn default() -> Self { Self::new(MusicClientConfig::default()) }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn context_uses_innertube_field_names() {
    let value = serde_json::to_value(ClientContext::default()).unwrap();
    assert_eq!(value["clientName"], "WEB_REMIX");
    assert_eq!(value["clientVersion"], "1.20260101.00.00");
    assert!(value.get("visitorData").is_none());
  }

  #[test]
  fn endpoint_paths_are_music_paths() {
    assert_eq!(Endpoint::SearchSuggestions.path(), "music/get_search_suggestions");
    assert_eq!(Endpoint::Queue.path(), "music/get_queue");
  }
}
