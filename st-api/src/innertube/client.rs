//! Client for the YouTube Music Innertube HTTP protocol.
//!
//! This module only sends JSON requests and returns raw JSON responses. Account
//! authentication, player-response deciphering, and conversion from renderer
//! trees into application models intentionally belong in higher layers.

use log::debug;
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

/// Navigation state carried by a search or queue item's watch endpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WatchEndpoint {
  pub video_id:              String,
  pub playlist_id:           Option<String>,
  pub params:                Option<String>,
  pub click_tracking_params: Option<String>,
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
  /// Numeric client identifier used by the matching request header.
  #[serde(skip)]
  pub client_id:      u32,
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
      client_id:      67,
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
  pub context:         ClientContext,
  /// Remaining fields from `INNERTUBE_CONTEXT.client`, such as browser and
  /// platform metadata. Core fields in [`ClientContext`] take precedence.
  pub client_context:  Map<String, Value>,
  /// Remaining top-level `INNERTUBE_CONTEXT` fields, such as `user` and
  /// `request`.
  pub request_context: Map<String, Value>,
  pub api_key:         Option<String>,
  pub user_agent:      header::HeaderValue,
  /// Cookies and authorization headers for authenticated requests.
  pub headers:         header::HeaderMap,
}

impl Default for MusicClientConfig {
  fn default() -> Self {
    Self {
      context:         ClientContext::default(),
      client_context:  Map::new(),
      request_context: Map::new(),
      api_key:         None,
      user_agent:      header::HeaderValue::from_static(
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
      ),
      headers:         header::HeaderMap::new(),
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
  pub fn new(http: Client, config: MusicClientConfig) -> Self { Self { http, config } }

  /// Calls an endpoint, adding the required `context` member to `payload`.
  pub async fn call(
    &self,
    endpoint: Endpoint,
    payload: Map<String, Value>,
  ) -> reqwest::Result<Value> {
    debug!("calling YouTube Music Innertube endpoint {}", endpoint.path());
    self.call_with_context(endpoint, payload, Map::new()).await
  }

  async fn call_with_context(
    &self,
    endpoint: Endpoint,
    mut payload: Map<String, Value>,
    context_overrides: Map<String, Value>,
  ) -> reqwest::Result<Value> {
    let mut client = self.config.client_context.clone();
    let core_client = serde_json::to_value(&self.config.context)
      .expect("ClientContext must serialize")
      .as_object()
      .expect("ClientContext serializes to an object")
      .clone();
    client.extend(core_client);
    let mut context = self.config.request_context.clone();
    context.extend(context_overrides);
    context.insert("client".into(), Value::Object(client));
    payload.insert("context".into(), Value::Object(context));

    let mut url = Url::parse(BASE_URL).expect("valid YouTube Music base URL");
    // `BASE_URL` ends in `/`; appending with `path_segments_mut().push()`
    // would preserve its final empty segment and produce `/v1//player`.
    url.set_path(&format!("/youtubei/v1/{}", endpoint.path()));
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
      .header("X-YouTube-Client-Name", self.config.context.client_id)
      .header("X-YouTube-Client-Version", &self.config.context.client_version)
      .header(header::ORIGIN, "https://music.youtube.com")
      .header(header::REFERER, "https://music.youtube.com/")
      .headers(self.config.headers.clone())
      .json(&payload);
    if let Some(visitor_data) = &self.config.context.visitor_data {
      request = request.header("X-Goog-Visitor-Id", visitor_data);
    }
    let response = request.send().await?;
    debug!("Innertube {} responded with {}", endpoint.path(), response.status());
    response.error_for_status()?.json().await
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

  /// Loads the next page of a browse response from its continuation token.
  pub async fn browse_continuation(&self, token: impl Into<String>) -> reqwest::Result<Value> {
    let body = Map::from_iter([(String::from("continuation"), Value::String(token.into()))]);
    self.call(Endpoint::Browse, body).await
  }

  /// Loads the next batch of a `next` queue from its continuation token.
  pub async fn next_continuation(&self, token: impl Into<String>) -> reqwest::Result<Value> {
    let body = Map::from_iter([(String::from("continuation"), Value::String(token.into()))]);
    self.call(Endpoint::Next, body).await
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
    self.player_with_signature_timestamp(video_id, None).await
  }

  /// Fetches player data, telling YouTube which player script's ciphers to
  /// return (see `playback::player_script::signature_timestamp`).
  pub async fn player_with_signature_timestamp(
    &self,
    video_id: impl Into<String>,
    signature_timestamp: Option<u32>,
  ) -> reqwest::Result<Value> {
    let mut body = player_body(video_id.into(), None, None);
    if let Some(timestamp) = signature_timestamp {
      body["playbackContext"]["contentPlaybackContext"]["signatureTimestamp"] = timestamp.into();
    }
    self.call(Endpoint::Player, body).await
  }

  /// Fetches queue/watch-page data using navigation state from a result row.
  pub async fn next_for_watch(&self, watch: &WatchEndpoint) -> reqwest::Result<Value> {
    let mut body =
      Map::from_iter([(String::from("videoId"), Value::String(watch.video_id.clone()))]);
    if let Some(playlist_id) = &watch.playlist_id {
      body.insert("playlistId".into(), Value::String(playlist_id.clone()));
    }
    if let Some(params) = &watch.params {
      body.insert("params".into(), Value::String(params.clone()));
    }
    self.call_with_context(Endpoint::Next, body, click_tracking_context(watch)).await
  }

  /// Fetches player data using navigation state from a result row.
  pub async fn player_for_watch(&self, watch: &WatchEndpoint) -> reqwest::Result<Value> {
    self
      .call_with_context(
        Endpoint::Player,
        player_body(watch.video_id.clone(), watch.playlist_id.clone(), watch.params.clone()),
        click_tracking_context(watch),
      )
      .await
  }
}

fn player_body(
  video_id: String,
  playlist_id: Option<String>,
  params: Option<String>,
) -> Map<String, Value> {
  let mut body = Map::from_iter([
    (String::from("videoId"), Value::String(video_id)),
    (
      String::from("playbackContext"),
      json!({ "contentPlaybackContext": { "html5Preference": "HTML5_PREF_WANTS" } }),
    ),
    (String::from("racyCheckOk"), Value::Bool(true)),
    (String::from("contentCheckOk"), Value::Bool(true)),
  ]);
  if let Some(playlist_id) = playlist_id {
    body.insert("playlistId".into(), Value::String(playlist_id));
  }
  if let Some(params) = params {
    body.insert("params".into(), Value::String(params));
  }
  body
}

fn click_tracking_context(watch: &WatchEndpoint) -> Map<String, Value> {
  watch
    .click_tracking_params
    .as_ref()
    .map(|params| {
      Map::from_iter([(String::from("clickTracking"), json!({ "clickTrackingParams": params }))])
    })
    .unwrap_or_default()
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

  #[test]
  fn player_body_has_a_playback_context_to_attach_the_timestamp_to() {
    let body = player_body("id".into(), None, None);
    assert!(body["playbackContext"]["contentPlaybackContext"].is_object());
  }

  #[test]
  fn player_url_has_no_double_slash() {
    let mut url = Url::parse(BASE_URL).unwrap();
    url.set_path(&format!("/youtubei/v1/{}", Endpoint::Player.path()));
    assert_eq!(url.as_str(), "https://music.youtube.com/youtubei/v1/player");
  }
}
