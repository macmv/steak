//! The player as the TUI sees it: the daemon's state, and what is local to
//! this screen (the cursor, the scroll position, the decoded album art).
//!
//! Nothing here plays anything. Keys become [`Command`]s for the daemon, and
//! the state comes back as [`PlayerView`] snapshots, so any number of TUIs (and
//! the CLI) can drive the same player.

use std::{
  collections::{HashMap, HashSet},
  time::{Duration, Instant},
};

use ratatui::{
  Frame,
  layout::{Constraint, Layout, Rect},
  style::{Color, Style},
  widgets::Paragraph,
};
use ratatui_image::{Resize, StatefulImage, protocol::StatefulProtocol};
use st_proto::{
  catalog::{Thumbnail, Track},
  ipc::{Command, PlayerView, Request, Source, Transport},
};
use tokio::sync::mpsc::UnboundedSender;

use super::{Hit, Msg, Target, ui};
use crate::client;

/// Each queue entry takes two rows: title, then artists.
const ITEM_HEIGHT: u16 = 2;
/// The cover beside each entry: as tall as the entry, and twice as wide in
/// cells because cells are about twice as tall as they are wide.
const COVER_WIDTH: u16 = 4;
const COVER_GAP: u16 = 1;
/// How long the window must stay the same size before images are drawn again.
const RESIZE_SETTLE: Duration = Duration::from_millis(150);

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
  Idle,
  Loading,
  Playing,
  Paused,
  Finished,
  Failed(String),
  /// The daemon is not reachable. The view on screen is the last one seen.
  Disconnected(String),
}

pub struct Player {
  tx:    UnboundedSender<Msg>,
  view:  PlayerView,
  /// When `view` arrived, which is what its `position_ms` is relative to.
  seen:  Instant,
  /// Why the daemon cannot be reached, or why the last command failed.
  down:  Option<String>,
  error: Option<String>,

  /// The queue entry the keyboard has selected; follows the current track
  /// when it changes.
  cursor: usize,
  offset: usize,

  art:        Option<(String, StatefulProtocol)>,
  /// The track whose art is loading or loaded, so it is requested once.
  art_wanted: Option<String>,

  /// Small covers for the queue entries, by video ID.
  covers:        HashMap<String, StatefulProtocol>,
  /// The covers loading or loaded, so each is requested once.
  covers_wanted: HashSet<String>,

  /// Images are not drawn until this passes. Every size they are drawn at
  /// costs a re-encode and a transfer to the terminal, so while the window is
  /// being dragged they stay out, and come back once it settles.
  hidden_until: Option<Instant>,
}

impl Player {
  pub fn new(tx: UnboundedSender<Msg>) -> Self {
    Self {
      tx,
      view: PlayerView::default(),
      seen: Instant::now(),
      down: None,
      error: None,
      cursor: 0,
      offset: 0,
      art: None,
      art_wanted: None,
      covers: HashMap::new(),
      covers_wanted: HashSet::new(),
      hidden_until: None,
    }
  }

  pub fn current(&self) -> Option<&Track> { self.view.current() }

  pub fn status(&self) -> Status {
    if let Some(reason) = &self.down {
      return Status::Disconnected(reason.clone());
    }
    if let Some(error) = &self.error {
      return Status::Failed(error.clone());
    }
    match &self.view.transport {
      Transport::Idle => Status::Idle,
      Transport::Loading => Status::Loading,
      Transport::Playing => Status::Playing,
      Transport::Paused => Status::Paused,
      Transport::Ended => Status::Finished,
      Transport::Failed(error) => Status::Failed(error.clone()),
    }
  }

  /// How far into the track playback is: the daemon's position when the view
  /// arrived, plus the time since if it is playing.
  pub fn elapsed(&self) -> Duration {
    let base = Duration::from_millis(self.view.position_ms);
    match (&self.view.transport, &self.down) {
      (Transport::Playing, None) => base + self.seen.elapsed(),
      _ => base,
    }
  }

  /// How long until the elapsed clock shows its next whole second, if it is
  /// running. Redrawing exactly then keeps the seconds evenly spaced.
  pub fn until_next_second(&self) -> Option<Duration> {
    match (&self.view.transport, &self.down) {
      (Transport::Playing, None) => {
        // A hair late, so the clock has certainly passed the boundary.
        Some(
          Duration::from_millis(1000 - u64::from(self.elapsed().subsec_millis()))
            + Duration::from_millis(2),
        )
      }
      _ => None,
    }
  }

  /// The window changed size: take the images off screen until it stops.
  pub fn on_resize(&mut self) { self.hidden_until = Some(Instant::now() + RESIZE_SETTLE); }

  fn images_visible(&self) -> bool { self.hidden_until.is_none_or(|until| Instant::now() >= until) }

  /// How long until the images come back after a resize, if they are away.
  pub fn until_images_return(&self) -> Option<Duration> {
    let wait = self.hidden_until?.checked_duration_since(Instant::now())?;
    // A hair late, so the deadline has certainly passed.
    Some(wait + Duration::from_millis(2))
  }

  pub fn duration(&self) -> Option<Duration> {
    ui::parse_duration(self.current()?.duration.as_deref()?)
  }

  /// Takes a new state from the daemon.
  pub fn on_view(&mut self, view: PlayerView) {
    let before = self.current().map(|track| track.video_id.clone());
    let queue_before = self.view.queue.first().map(|track| track.video_id.clone());
    self.view = view;
    self.seen = Instant::now();
    self.down = None;
    self.error = None;

    let after = self.current().map(|track| track.video_id.clone());
    let queue_after = self.view.queue.first().map(|track| track.video_id.clone());
    if before != after || queue_before != queue_after {
      self.cursor = self.view.index;
    }
    self.cursor = self.cursor.min(self.view.queue.len().saturating_sub(1));
    self.request_art();
  }

  /// The daemon went away. The last state stays on screen, marked as stale.
  pub fn on_disconnected(&mut self, reason: String) { self.down = Some(reason); }

  pub fn on_command_failed(&mut self, error: String) { self.error = Some(error); }

  fn request_art(&mut self) {
    let wanted = self.current().map(|track| track.video_id.clone());
    if wanted == self.art_wanted {
      return;
    }
    if self.art.as_ref().is_some_and(|(id, _)| Some(id) != wanted.as_ref()) {
      self.art = None;
    }
    self.art_wanted = wanted.clone();
    let (Some(video_id), Some(url)) =
      (wanted, self.current().and_then(|t| art_url(&t.thumbnails, ART_SIZE)))
    else {
      return;
    };
    let tx = self.tx.clone();
    tokio::spawn(async move {
      match fetch_art(url).await {
        Ok(image) => {
          let _ = tx.send(Msg::Art { video_id, image });
        }
        Err(error) => log::warn!("album art failed: {error}"),
      }
    });
  }

  /// Starts loading the cover of `track`, unless that was already done.
  fn request_cover(&mut self, track: &Track) {
    let Some(url) = art_url(&track.thumbnails, COVER_SIZE) else { return };
    if !self.covers_wanted.insert(track.video_id.clone()) {
      return;
    }
    let tx = self.tx.clone();
    let video_id = track.video_id.clone();
    tokio::spawn(async move {
      match fetch_art(url).await {
        Ok(image) => {
          let _ = tx.send(Msg::Cover { video_id, image });
        }
        Err(error) => log::warn!("cover failed: {error}"),
      }
    });
  }

  pub fn set_cover(&mut self, video_id: String, protocol: StatefulProtocol) {
    self.covers.insert(video_id, protocol);
  }

  pub fn set_art(&mut self, video_id: String, protocol: StatefulProtocol) {
    if self.current().is_some_and(|track| track.video_id == video_id) {
      self.art = Some((video_id, protocol));
    }
  }

  /// Sends a command to the daemon. What it does arrives as the next view.
  pub fn send(&self, command: Command) {
    let tx = self.tx.clone();
    tokio::spawn(async move {
      if let Err(error) = client::call(Request::Control(command)).await {
        let _ = tx.send(Msg::CommandFailed(error.to_string()));
      }
    });
  }

  /// Replaces the queue with the song radio of `seed`.
  pub fn start_radio(&self, seed: Track) { self.send(Command::StartRadio { seed }); }

  pub fn play_index(&self, index: usize) {
    if let Some(track) = self.view.queue.get(index) {
      self.send(Command::JumpTo { index, video_id: track.video_id.clone() });
    }
  }

  pub fn play_cursor(&self) { self.play_index(self.cursor); }

  pub fn next(&self) { self.send(Command::Next); }

  pub fn previous(&self) { self.send(Command::Previous); }

  pub fn toggle_pause(&self) { self.send(Command::Toggle); }

  pub fn move_cursor(&mut self, delta: isize) {
    self.cursor =
      self.cursor.saturating_add_signed(delta).min(self.view.queue.len().saturating_sub(1));
  }

  /// Draws the expanded player: album art on the left, the queue on the right.
  pub fn render(&mut self, frame: &mut Frame, area: Rect, hits: &mut Vec<Hit>) {
    let [left, right] = Layout::horizontal([Constraint::Percentage(50); 2]).areas(area);

    let album = self.current().and_then(|t| t.album.as_ref()).map(|a| a.name.clone());
    let left = Rect { width: left.width.saturating_sub(2), ..left };
    let inner = ui::render_section(frame, left, album.as_deref().unwrap_or("Album art"), false);
    let show = self.images_visible();
    match &mut self.art {
      Some((_, protocol)) if show => {
        // `Scale`, not the default `Fit`: `Fit` never enlarges an image past
        // its own pixel size, so the art could not grow with the window.
        let image = StatefulImage::default().resize(Resize::Scale(None));
        frame.render_stateful_widget(image, centered_square(inner), protocol)
      }
      // Hidden while the window is being resized.
      Some(_) => {}
      None => frame.render_widget(
        Paragraph::new("No album art").style(Style::default().fg(Color::DarkGray)).centered(),
        inner,
      ),
    }

    let seed = self.view.queue.first().map_or("", |track| track.title.as_str());
    let title = match &self.view.source {
      Source::Radio { .. } => format!("Radio: {seed}"),
      Source::Playlist { .. } => "Playlist".to_owned(),
      Source::None => "Queue".to_owned(),
    };
    let inner = ui::render_section(frame, right, &title, true);
    hits.push(Hit { rect: right, target: Target::Queue(None) });

    let rows = ((inner.height / ITEM_HEIGHT) as usize).max(1);
    if self.cursor < self.offset {
      self.offset = self.cursor;
    } else if self.cursor >= self.offset + rows {
      self.offset = self.cursor + 1 - rows;
    }
    for (slot, idx) in (self.offset..self.view.queue.len()).take(rows).enumerate() {
      let rect = Rect {
        x:      inner.x,
        y:      inner.y + slot as u16 * ITEM_HEIGHT,
        width:  inner.width,
        height: ITEM_HEIGHT.min(inner.height),
      };
      let track = self.view.queue[idx].clone();
      self.request_cover(&track);
      let cover = Rect { width: COVER_WIDTH.min(rect.width), ..rect };
      if let Some(protocol) = self.covers.get_mut(&track.video_id).filter(|_| show) {
        frame.render_stateful_widget(StatefulImage::default(), cover, protocol);
      }
      let text_x = COVER_WIDTH + COVER_GAP;
      let text = Rect {
        x: rect.x + text_x.min(rect.width),
        width: rect.width.saturating_sub(text_x),
        ..rect
      };
      ui::render_track(frame, text, &track, idx == self.cursor, idx == self.view.index);
      hits.push(Hit { rect, target: Target::Queue(Some(idx)) });
    }
  }
}

/// The biggest square that is 80% of `area`'s width (or less, if it would not
/// fit the height), centered in it. Cells are about twice as tall as they are
/// wide, so a square is twice as many columns as rows.
fn centered_square(area: Rect) -> Rect {
  let width = (area.width * 4 / 5).min(area.height.saturating_mul(2));
  let height = width / 2;
  Rect {
    x: area.x + (area.width - width) / 2,
    y: area.y + (area.height - height) / 2,
    width,
    height,
  }
}

const ART_SIZE: u32 = 1200;
const COVER_SIZE: u32 = 120;

/// The largest thumbnail, asked for at a size worth putting on screen.
///
/// Track rows only link a tiny thumbnail, but Google's image host will resize
/// to whatever the `=w..-h..` suffix says.
fn art_url(thumbnails: &[Thumbnail], size: u32) -> Option<String> {
  let best = thumbnails.iter().max_by_key(|thumbnail| thumbnail.width.unwrap_or(0))?;
  match best.url.rsplit_once('=') {
    Some((base, suffix)) if suffix.starts_with('w') => {
      Some(format!("{base}=w{size}-h{size}-l90-rj"))
    }
    _ => Some(best.url.clone()),
  }
}

async fn fetch_art(url: String) -> Result<image::DynamicImage, client::BoxError> {
  let bytes = match client::call(Request::Art { url }).await? {
    st_proto::ipc::Response::Art(bytes) => bytes,
    other => return Err(format!("the daemon sent an unexpected response: {other:?}").into()),
  };
  Ok(tokio::task::spawn_blocking(move || image::load_from_memory(&bytes)).await??)
}
