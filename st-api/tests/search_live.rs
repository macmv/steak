//! Live end-to-end search smoke test.
//!
//! Run explicitly with `cargo test -p st-api --test search_live -- --ignored`.

use st_api::{
  catalog::SearchParser,
  innertube::{
    bootstrap::{Bootstrapper, WebBootstrapper},
    client::{MusicClient, MusicClientConfig},
  },
};

#[tokio::test]
#[ignore = "calls the live YouTube Music search endpoint"]
async fn search_returns_a_playable_catalog_row() {
  let http = st_api::Client::new();
  let bootstrap = WebBootstrapper::new(http.clone()).bootstrap().await.unwrap();
  let mut config = MusicClientConfig::default();
  bootstrap.apply_to(&mut config);
  let response = MusicClient::new(http.clone(), config)
    .search("Daft Punk Around the World", None)
    .await
    .unwrap();

  let tracks = SearchParser.tracks(&response);
  assert!(tracks.iter().any(|track| !track.video_id.is_empty() && !track.title.is_empty()));
}
