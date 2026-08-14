//! Live smoke test for the YouTube Music bootstrap page.
//!
//! Run explicitly because the page and its network availability are external:
//! `cargo test -p st-api --test bootstrap_live -- --ignored`

use st_api::bootstrap::{Bootstrapper, WebBootstrapper};

#[tokio::test]
#[ignore = "fetches the live YouTube Music bootstrap page"]
async fn live_music_homepage_contains_innertube_bootstrap_data() {
  let bootstrap = WebBootstrapper::default().bootstrap().await.unwrap();

  assert!(!bootstrap.api_key.is_empty(), "INNERTUBE_API_KEY must be present");
  assert!(!bootstrap.client_version.is_empty(), "client version must be present");
  assert!(
    bootstrap
      .player_script_url
      .as_deref()
      .is_none_or(|url| url.starts_with("https://music.youtube.com/")),
    "a player script URL, when present, must refer to the Music origin"
  );
}
