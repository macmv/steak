//! Conversion of Music search renderer trees into stable catalog objects.

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
  Some(Track { video_id: video_id.into(), title, artists, album, duration, thumbnails })
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

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn extracts_track_from_search_fixture() {
    let response = serde_json::json!({"contents":[{"musicResponsiveListItemRenderer": {
      "flexColumns":[
        {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"Around the World"}]}}},
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
  }
}
