//! Conversion of Music search and home renderer trees into stable catalog
//! objects.

use crate::innertube::client::WatchEndpoint;
use serde_json::Value;

/// A track returned by YouTube Music search.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Track {
  pub video_id:   String,
  pub title:      String,
  pub artists:    Vec<Artist>,
  pub album:      Option<Album>,
  pub duration:   Option<String>,
  pub thumbnails: Vec<Thumbnail>,
  pub watch:      Option<WatchEndpoint>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Artist {
  pub id:   Option<String>,
  pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Album {
  pub id:   Option<String>,
  pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Thumbnail {
  pub url:    String,
  pub width:  Option<u32>,
  pub height: Option<u32>,
}

/// Extracts song/video rows from a raw `search` response.
#[derive(Clone, Debug, Default)]
pub struct SearchParser;

impl SearchParser {
  pub fn tracks(&self, response: &Value) -> Vec<Track> {
    let mut rows = Vec::new();
    find_music_rows(response, &mut rows);
    rows.into_iter().filter_map(parse_track).collect()
  }
}

/// A row of the Music homepage, such as "Quick picks" or "Listen again".
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Shelf {
  pub title: String,
  pub items: Vec<ShelfItem>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShelfItem {
  Track(Track),
  Card(Card),
}

/// A shelf entry that is not a plain track row: an album, playlist, artist,
/// or a video/radio card.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Card {
  pub title:     String,
  pub subtitle:  String,
  pub browse_id: Option<String>,
  pub watch:     Option<WatchEndpoint>,
}

/// Extracts shelves from a raw `browse` response for the home page, or from one
/// of its continuations.
#[derive(Clone, Debug, Default)]
pub struct HomeParser;

impl HomeParser {
  pub fn shelves(&self, response: &Value) -> Vec<Shelf> {
    let mut shelves = Vec::new();
    find_shelves(response, &mut shelves);
    shelves.into_iter().filter_map(parse_shelf).collect()
  }

  /// The token that loads the next batch of shelves, if there are more.
  pub fn continuation(&self, response: &Value) -> Option<String> {
    find_string(response, "continuation").map(str::to_owned)
  }

  /// Whether the response was served to a signed-in account. Anonymous
  /// responses are generic and lack personal shelves like "Listen again".
  pub fn logged_in(&self, response: &Value) -> bool {
    response
      .pointer("/responseContext/serviceTrackingParams")
      .and_then(Value::as_array)
      .into_iter()
      .flatten()
      .flat_map(|service| service.get("params").and_then(Value::as_array).into_iter().flatten())
      .any(|param| {
        param.get("key").and_then(Value::as_str) == Some("logged_in")
          && param.get("value").and_then(Value::as_str) == Some("1")
      })
  }
}

fn find_shelves<'a>(value: &'a Value, shelves: &mut Vec<&'a Value>) {
  match value {
    Value::Array(values) => values.iter().for_each(|value| find_shelves(value, shelves)),
    Value::Object(object) => {
      if let Some(shelf) = object.get("musicCarouselShelfRenderer") {
        shelves.push(shelf);
      } else {
        object.values().for_each(|value| find_shelves(value, shelves));
      }
    }
    _ => {}
  }
}

fn parse_shelf(shelf: &Value) -> Option<Shelf> {
  let title = runs_text(shelf.pointer("/header/musicCarouselShelfBasicHeaderRenderer/title/runs")?);
  let items = shelf
    .get("contents")?
    .as_array()?
    .iter()
    .filter_map(|item| {
      if let Some(row) = item.get("musicResponsiveListItemRenderer") {
        parse_track(row).map(ShelfItem::Track)
      } else {
        item.get("musicTwoRowItemRenderer").and_then(parse_card).map(ShelfItem::Card)
      }
    })
    .collect();
  Some(Shelf { title, items })
}

fn parse_card(item: &Value) -> Option<Card> {
  let title = runs_text(item.pointer("/title/runs")?);
  let subtitle = item.pointer("/subtitle/runs").map(runs_text).unwrap_or_default();
  let navigation = item.get("navigationEndpoint")?;
  let browse_id =
    navigation.pointer("/browseEndpoint/browseId").and_then(Value::as_str).map(str::to_owned);
  let watch = find_watch_endpoint(navigation);
  Some(Card { title, subtitle, browse_id, watch })
}

fn runs_text(runs: &Value) -> String {
  runs
    .as_array()
    .into_iter()
    .flatten()
    .filter_map(|run| run.get("text").and_then(Value::as_str))
    .collect()
}

fn find_music_rows<'a>(value: &'a Value, rows: &mut Vec<&'a Value>) {
  match value {
    Value::Array(values) => values.iter().for_each(|value| find_music_rows(value, rows)),
    Value::Object(object) => {
      if let Some(row) = object.get("musicResponsiveListItemRenderer") {
        rows.push(row);
      }
      object.values().for_each(|value| find_music_rows(value, rows));
    }
    _ => {}
  }
}

fn parse_track(row: &Value) -> Option<Track> {
  let columns = row.get("flexColumns")?.as_array()?;
  let title = column_text(columns.first()?)?;
  let video_id = find_string(row, "videoId")?;
  let watch = find_watch_endpoint(row);
  let metadata = columns.get(1).and_then(column_runs).unwrap_or_default();
  let mut artists = Vec::new();
  let mut album = None;
  for run in &metadata {
    let text = run.get("text").and_then(Value::as_str)?;
    let browse_id =
      run.pointer("/navigationEndpoint/browseEndpoint/browseId").and_then(Value::as_str);
    if browse_id.is_some_and(|id| id.starts_with("MPRE")) {
      album = Some(Album { id: browse_id.map(str::to_owned), name: text.into() });
    } else if browse_id.is_some_and(|id| id.starts_with("UC")) {
      artists.push(Artist { id: browse_id.map(str::to_owned), name: text.into() });
    }
  }
  let duration = row
    .get("fixedColumns")
    .and_then(Value::as_array)
    .and_then(|columns| columns.first())
    .and_then(column_text);
  let thumbnails = row
    .pointer("/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails")
    .and_then(Value::as_array)
    .into_iter()
    .flatten()
    .filter_map(|thumbnail| {
      Some(Thumbnail {
        url:    thumbnail.get("url")?.as_str()?.into(),
        width:  thumbnail
          .get("width")
          .and_then(Value::as_u64)
          .and_then(|value| value.try_into().ok()),
        height: thumbnail
          .get("height")
          .and_then(Value::as_u64)
          .and_then(|value| value.try_into().ok()),
      })
    })
    .collect();
  Some(Track { video_id: video_id.into(), title, artists, album, duration, thumbnails, watch })
}

fn find_watch_endpoint(row: &Value) -> Option<WatchEndpoint> {
  let endpoint = find_object(row, "watchEndpoint")?;
  Some(WatchEndpoint {
    video_id:              endpoint.get("videoId")?.as_str()?.into(),
    playlist_id:           endpoint.get("playlistId").and_then(Value::as_str).map(str::to_owned),
    params:                endpoint.get("params").and_then(Value::as_str).map(str::to_owned),
    click_tracking_params: find_string(row, "clickTrackingParams").map(str::to_owned),
  })
}

fn column_text(column: &Value) -> Option<String> {
  column_runs(column)?
    .iter()
    .filter_map(|run| run.get("text").and_then(Value::as_str))
    .collect::<String>()
    .into()
}

fn column_runs(column: &Value) -> Option<Vec<&Value>> {
  column
    .pointer("/musicResponsiveListItemFlexColumnRenderer/text/runs")?
    .as_array()
    .map(|runs| runs.iter().collect())
}

fn find_string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
  match value {
    Value::Object(object) => object
      .get(key)
      .and_then(Value::as_str)
      .or_else(|| object.values().find_map(|value| find_string(value, key))),
    Value::Array(values) => values.iter().find_map(|value| find_string(value, key)),
    _ => None,
  }
}

fn find_object<'a>(value: &'a Value, key: &str) -> Option<&'a serde_json::Map<String, Value>> {
  match value {
    Value::Object(object) => object
      .get(key)
      .and_then(Value::as_object)
      .or_else(|| object.values().find_map(|value| find_object(value, key))),
    Value::Array(values) => values.iter().find_map(|value| find_object(value, key)),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn extracts_track_from_search_fixture() {
    let response = serde_json::json!({"contents":[{"musicResponsiveListItemRenderer": {
      "flexColumns":[
        {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"Around the World", "navigationEndpoint":{"watchEndpoint":{"videoId":"LKYPYj2XX80","playlistId":"RDexample","params":"params"}}}]}}},
        {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[
          {"text":"Daft Punk", "navigationEndpoint":{"browseEndpoint":{"browseId":"UCartist"}}},
          {"text":" • "},
          {"text":"Homework", "navigationEndpoint":{"browseEndpoint":{"browseId":"MPREalbum"}}}
        ]}}}
      ],
      "fixedColumns":[{"musicResponsiveListItemFixedColumnRenderer":{"text":{"runs":[{"text":"7:09"}]}}}],
      "playlistItemData":{"videoId":"LKYPYj2XX80"},
      "thumbnail":{"musicThumbnailRenderer":{"thumbnail":{"thumbnails":[{"url":"https://img.example/cover.jpg","width":544,"height":544}]}}}
    }}]});
    let tracks = SearchParser.tracks(&response);
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].title, "Around the World");
    assert_eq!(tracks[0].artists[0].name, "Daft Punk");
    assert_eq!(tracks[0].album.as_ref().unwrap().name, "Homework");
    assert_eq!(tracks[0].watch.as_ref().unwrap().playlist_id.as_deref(), Some("RDexample"));
  }

  #[test]
  fn extracts_home_shelves() {
    let response = serde_json::json!({
      "responseContext":{"serviceTrackingParams":[{"params":[{"key":"logged_in","value":"1"}]}]},
      "contents":[{"musicCarouselShelfRenderer":{
        "header":{"musicCarouselShelfBasicHeaderRenderer":{"title":{"runs":[{"text":"Listen again"}]}}},
        "contents":[
          {"musicTwoRowItemRenderer":{
            "title":{"runs":[{"text":"Hotlist"}]},
            "subtitle":{"runs":[{"text":"Playlist"},{"text":" \u{2022} "},{"text":"Someone"}]},
            "navigationEndpoint":{"browseEndpoint":{"browseId":"VLRDplaylist"}}}},
          {"musicTwoRowItemRenderer":{
            "title":{"runs":[{"text":"A Song"}]},
            "navigationEndpoint":{"watchEndpoint":{"videoId":"abc"}}}}
        ]}}],
      "continuations":[{"nextContinuationData":{"continuation":"token"}}]
    });
    let shelves = HomeParser.shelves(&response);
    assert_eq!(shelves.len(), 1);
    assert_eq!(shelves[0].title, "Listen again");
    let ShelfItem::Card(card) = &shelves[0].items[0] else { panic!("expected a card") };
    assert_eq!(card.subtitle, "Playlist \u{2022} Someone");
    assert_eq!(card.browse_id.as_deref(), Some("VLRDplaylist"));
    let ShelfItem::Card(song) = &shelves[0].items[1] else { panic!("expected a card") };
    assert_eq!(song.watch.as_ref().unwrap().video_id, "abc");
    assert_eq!(HomeParser.continuation(&response).as_deref(), Some("token"));
    assert!(HomeParser.logged_in(&response));
  }
}
