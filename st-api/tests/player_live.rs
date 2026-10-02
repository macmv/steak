//! Live search-to-player smoke test.
//!
//! Run explicitly with `cargo test -p st-api --test player_live -- --ignored`.

use st_api::{
  account::session::{StoredSession, session_path},
  catalog::SearchParser,
  innertube::{
    bootstrap::{Bootstrapper, WebBootstrapper},
    client::{MusicClient, MusicClientConfig},
  },
  playback::{
    player_script::{EjsCipherResolver, StreamUrl},
    response::{AudioQuality, PlayerParser, StandardPlayerParser},
  },
};

#[tokio::test]
#[ignore = "calls live YouTube Music search and player endpoints"]
async fn search_result_has_an_audio_format_in_its_player_response() {
  let session_path = session_path();
  let session = StoredSession::load(&session_path)
    .expect("run `steak auth import-firefox` before this live test");
  let bootstrapper = WebBootstrapper::default();
  let bootstrap = bootstrapper.bootstrap().await.unwrap();
  let mut config = MusicClientConfig::default();
  bootstrap.apply_to(&mut config);
  config.headers.extend(session.auth_session().request_headers());
  let music = MusicClient::new(config);

  let search = music.search("Daft Punk Around the World", None).await.unwrap();
  let track = SearchParser
    .tracks(&search)
    .into_iter()
    .find(|track| track.watch.is_some())
    .expect("search result must include a watch endpoint");

  let watch = track.watch.as_ref().unwrap();
  let _next = music.next_for_watch(watch).await.unwrap();
  let player = music.player_for_watch(watch).await.unwrap();
  let parser = StandardPlayerParser;
  let selected =
    parser.choose_audio(parser.audio_formats(&player).unwrap(), AudioQuality::Balanced).unwrap();

  assert!(selected.mime_type.starts_with("audio/"));
  assert!(selected.bitrate > 0);
  eprintln!(
    "selected {} itag {} at {} bps ({})",
    track.title,
    selected.itag,
    selected.bitrate,
    match selected.stream_url {
      StreamUrl::Direct(_) => "direct URL",
      StreamUrl::SignatureCipher(_) => "signature cipher",
    }
  );
  if let StreamUrl::SignatureCipher(_) = &selected.stream_url {
    let script_url = StandardPlayerParser::player_script_url(&player)
      .or(bootstrapper.player_script_url(&track.video_id).await.unwrap())
      .expect("watch page must advertise its JavaScript URL for ciphered formats");
    let resolved =
      EjsCipherResolver::new().resolve(&script_url, &selected.stream_url).await.unwrap();
    assert!(resolved.query_pairs().any(|(key, _)| key == "signature" || key == "sig"));
    let n = resolved.query_pairs().find(|(key, _)| key == "n").map(|(_, value)| value.into_owned());
    assert!(n.is_some(), "resolved media URL must retain a transformed n parameter");
  }
}
