//! Plain data shared by the daemon and its clients: catalog objects.

use serde::{Deserialize, Serialize};

/// Navigation state carried by a search or queue item's watch endpoint.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WatchEndpoint {
  pub video_id:              String,
  pub playlist_id:           Option<String>,
  pub params:                Option<String>,
  pub click_tracking_params: Option<String>,
}

/// A track returned by YouTube Music search.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Track {
  pub video_id:   String,
  pub title:      String,
  pub artists:    Vec<Artist>,
  pub album:      Option<Album>,
  pub duration:   Option<String>,
  pub thumbnails: Vec<Thumbnail>,
  pub watch:      Option<WatchEndpoint>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Artist {
  pub id:   Option<String>,
  pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Album {
  pub id:   Option<String>,
  pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Thumbnail {
  pub url:    String,
  pub width:  Option<u32>,
  pub height: Option<u32>,
}

/// A row of the Music homepage, such as "Quick picks" or "Listen again".
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Shelf {
  pub title: String,
  pub items: Vec<ShelfItem>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ShelfItem {
  Track(Track),
  Card(Card),
}

/// A shelf entry that is not a plain track row: an album, playlist, artist,
/// or a video/radio card.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Card {
  pub title:     String,
  pub subtitle:  String,
  pub browse_id: Option<String>,
  pub watch:     Option<WatchEndpoint>,
}
