//! Live check of the stats-ping inputs.
//!
//! Run explicitly with `cargo test -p st-api --test stats_live -- --ignored`.
//!
//! This builds the pings from a real player response and fetches the signed-in
//! `session_token`, but never sends one: a ping is a recorded play, and a test
//! must not write to the account's history.

use st_api::{
  account::session::{StoredSession, session_path},
  innertube::{
    bootstrap::{Bootstrapper, WebBootstrapper},
    client::{MusicClient, MusicClientConfig},
  },
  playback::stats::{PingKind, PlaybackTracking, State, WatchSession},
};

#[tokio::test]
#[ignore = "calls live YouTube Music endpoints"]
async fn player_response_yields_pings_and_the_page_yields_a_token() {
  let http = st_api::Client::new();
  let session = StoredSession::load(&session_path())
    .expect("run `steak auth import-firefox` before this live test");
  let bootstrapper = WebBootstrapper::new(http.clone());
  let bootstrap = bootstrapper.bootstrap().await.unwrap();
  let mut config = MusicClientConfig::default();
  bootstrap.apply_to(&mut config);
  config.headers.extend(session.auth_session().request_headers());
  let music = MusicClient::new(http, config);

  let player = music.player("3zYCYpbVzHk").await.unwrap();
  let tracking = PlaybackTracking::from_player(&player, 140)
    .expect("a signed-in player response carries tracking URLs");

  let mut watch = WatchSession::new(&tracking, 0.0);
  let start = watch.observe(1.4, State::Playing, 0.0);
  let end = watch.finish(5.0, 3.5);
  let client = music.stats_params();
  for ping in start.iter().chain(&end) {
    let url = tracking.url(ping, &client);
    let names: Vec<_> = url.query_pairs().map(|(name, _)| name.into_owned()).collect();
    eprintln!("{:?}: {}?{}", ping.kind, url.path(), names.join("&"));
    assert_eq!(url.host_str(), Some("music.youtube.com"));
    for required in ["cpn", "docid", "ei", "plid", "vm", "cmt", "rt", "afmt", "c", "cver"] {
      assert!(names.iter().any(|name| name == required), "{required} is missing");
    }
  }
  assert_eq!(start[0].kind, PingKind::Playback);
  assert!(end[0].is_final);

  let token = bootstrapper
    .session_token(session.auth_session().request_headers())
    .await
    .expect("the imported session must load a signed-in page");
  assert!(!token.is_empty());
}
