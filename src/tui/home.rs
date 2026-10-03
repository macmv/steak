//! The homepage: one column per shelf, each listing its tracks.

use ratatui::{
  Frame,
  layout::{Constraint, Layout, Rect},
  widgets::Paragraph,
};
use st_proto::catalog::{Artist, Card, Shelf, ShelfItem, Track};

use super::{Hit, Target, ui};

/// Columns are never narrower than this; extra shelves scroll sideways.
const MIN_COLUMN_WIDTH: u16 = 36;
/// Each track takes two rows: title, then artists.
const ITEM_HEIGHT: u16 = 2;

struct Column {
  title:    String,
  tracks:   Vec<Track>,
  selected: usize,
  offset:   usize,
}

/// Turns a card for a single song or video into a track. Cards that open a
/// browse page (albums, playlists, artists) are not playable, so they give
/// `None`.
fn card_track(card: Card) -> Option<Track> {
  if card.browse_id.is_some() {
    return None;
  }
  let watch = card.watch?;
  // Subtitles look like "Song \u{2022} Artist" or "730t \u{2022} 35K views".
  let mut parts = card.subtitle.split(" \u{2022} ");
  let kind = parts.next().unwrap_or_default();
  let artists = if kind == "Song" {
    parts.next().map(|name| Artist { id: None, name: name.to_owned() }).into_iter().collect()
  } else {
    Vec::new()
  };
  Some(Track {
    video_id: watch.video_id.clone(),
    title: card.title,
    artists,
    album: None,
    duration: None,
    thumbnails: Vec::new(),
    watch: Some(watch),
  })
}

#[derive(Default)]
pub struct Home {
  columns:     Vec<Column>,
  col:         usize,
  first:       usize,
  pub loading: bool,
  pub error:   Option<String>,
}

impl Home {
  pub fn new() -> Self { Self { loading: true, ..Self::default() } }

  /// Adds the shelves' tracks, including cards for single songs and videos;
  /// albums and playlists have nothing to play directly, so they are skipped.
  pub fn add_shelves(&mut self, shelves: Vec<Shelf>) {
    for shelf in shelves {
      let tracks: Vec<Track> = shelf
        .items
        .into_iter()
        .filter_map(|item| match item {
          ShelfItem::Track(track) => Some(track),
          ShelfItem::Card(card) => card_track(card),
        })
        .collect();
      if !tracks.is_empty() {
        self.columns.push(Column { title: shelf.title, tracks, selected: 0, offset: 0 });
      }
    }
  }

  pub fn selected_track(&self) -> Option<&Track> {
    let column = self.columns.get(self.col)?;
    column.tracks.get(column.selected)
  }

  pub fn track(&self, col: usize, idx: usize) -> Option<&Track> {
    self.columns.get(col)?.tracks.get(idx)
  }

  pub fn select(&mut self, col: usize, idx: usize) {
    if let Some(column) = self.columns.get_mut(col) {
      self.col = col;
      column.selected = idx.min(column.tracks.len().saturating_sub(1));
    }
  }

  pub fn move_row(&mut self, delta: isize) { self.scroll(self.col, delta); }

  /// Moves a column's selection, as the mouse wheel does.
  pub fn scroll(&mut self, col: usize, delta: isize) {
    if let Some(column) = self.columns.get_mut(col) {
      column.selected =
        column.selected.saturating_add_signed(delta).min(column.tracks.len().saturating_sub(1));
    }
  }

  pub fn move_col(&mut self, delta: isize) {
    if !self.columns.is_empty() {
      self.col = self.col.saturating_add_signed(delta).min(self.columns.len() - 1);
    }
  }

  pub fn render(
    &mut self,
    frame: &mut Frame,
    area: Rect,
    playing: Option<&str>,
    hits: &mut Vec<Hit>,
  ) {
    if self.columns.is_empty() {
      let text = match (&self.error, self.loading) {
        (Some(error), _) => format!("Could not load the homepage: {error}"),
        (None, true) => "Loading the homepage...".into(),
        (None, false) => "The homepage has no tracks.".into(),
      };
      frame.render_widget(Paragraph::new(text).centered(), area);
      return;
    }

    let visible = ((area.width / MIN_COLUMN_WIDTH) as usize).clamp(1, self.columns.len());
    if self.col < self.first {
      self.first = self.col;
    } else if self.col >= self.first + visible {
      self.first = self.col + 1 - visible;
    }
    self.first = self.first.min(self.columns.len() - visible);

    let areas = Layout::horizontal(vec![Constraint::Ratio(1, visible as u32); visible]).split(area);
    for (area, index) in areas.iter().zip(self.first..) {
      let active = index == self.col;
      let column = &mut self.columns[index];
      // Pad the column so neighbors are separated by whitespace instead of
      // borders.
      let padded = Rect { width: area.width.saturating_sub(2), ..*area };
      let inner = ui::render_section(frame, padded, &column.title, active);
      hits.push(Hit { rect: *area, target: Target::HomeColumn(index) });

      let rows = ((inner.height / ITEM_HEIGHT) as usize).max(1);
      if column.selected < column.offset {
        column.offset = column.selected;
      } else if column.selected >= column.offset + rows {
        column.offset = column.selected + 1 - rows;
      }
      for (slot, idx) in (column.offset..column.tracks.len()).take(rows).enumerate() {
        let rect = Rect {
          x:      inner.x,
          y:      inner.y + slot as u16 * ITEM_HEIGHT,
          width:  inner.width,
          height: ITEM_HEIGHT.min(inner.height),
        };
        let track = &column.tracks[idx];
        let is_playing = playing == Some(track.video_id.as_str());
        ui::render_track(frame, rect, track, active && idx == column.selected, is_playing);
        hits.push(Hit { rect, target: Target::HomeItem(index, idx) });
      }
    }
  }
}
