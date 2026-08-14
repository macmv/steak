//! Live end-to-end search smoke test.
//!
//! Run explicitly with `cargo test -p st-api --test search_live -- --ignored`.

use st_api::{
  bootstrap::{Bootstrapper, WebBootstrapper},
  catalog::SearchParser,
  music::{MusicClient, MusicClientConfig},
};

#[tokio::test]
#[ignore = "calls the live YouTube Music search endpoint"]
async fn search_returns_a_playable_catalog_row() {
  let bootstrap = WebBootstrapper::default().bootstrap().await.unwrap();
  let mut config = MusicClientConfig::default();
  bootstrap.apply_to(&mut config);
  let response = MusicClient::new(config).search("Daft Punk Around the World", None).await.unwrap();

  let tracks = SearchParser.tracks(&response);
  assert!(tracks.iter().any(|track| !track.video_id.is_empty() && !track.title.is_empty()));
}
