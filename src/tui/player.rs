//! The player as the TUI sees it: the daemon's state, and what is local to
//! this screen (the cursor, the scroll position, the decoded album art).
//!
//! Nothing here plays anything. Keys become [`Command`]s for the daemon, and
//! the state comes back as [`PlayerView`] snapshots, so any number of TUIs (and
//! the CLI) can drive the same player.

use std::time::{Duration, Instant};

use ratatui::{
  Frame,
  layout::{Constraint, Layout, Rect},
  style::{Color, Style},
  widgets::{Block, Paragraph},
};
use ratatui_image::{StatefulImage, protocol::StatefulProtocol};
use st_proto::{
  catalog::{Thumbnail, Track},
  ipc::{Command, PlayerView, Request, Source, Transport},
};
use tokio::sync::mpsc::UnboundedSender;

use super::{Hit, Msg, Target, ui};
use crate::client;

/// Each queue entry takes two rows: title, then artists.
const ITEM_HEIGHT: u16 = 2;

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
    let (Some(video_id), Some(url)) = (wanted, self.current().and_then(|t| art_url(&t.thumbnails)))
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
    let block = Block::bordered().title(format!(" {} ", album.as_deref().unwrap_or("Album art")));
    let inner = block.inner(left);
    frame.render_widget(block, left);
    match &mut self.art {
      Some((_, protocol)) => {
        frame.render_stateful_widget(StatefulImage::default(), inner, protocol)
      }
      None => frame.render_widget(
        Paragraph::new("No album art").style(Style::default().fg(Color::DarkGray)).centered(),
        inner,
      ),
    }

    let seed = self.view.queue.first().map_or("", |track| track.title.as_str());
    let title = match &self.view.source {
      Source::Radio { .. } => format!(" Radio: {seed} "),
      Source::Playlist { .. } => " Playlist ".to_owned(),
      Source::None => " Queue ".to_owned(),
    };
    let block = Block::bordered().title(title).border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(right);
    frame.render_widget(block, right);
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
      ui::render_track(
        frame,
        rect,
        &self.view.queue[idx],
        idx == self.cursor,
        idx == self.view.index,
      );
      hits.push(Hit { rect, target: Target::Queue(Some(idx)) });
    }
  }
}

/// The largest thumbnail, asked for at a size worth putting on screen.
///
/// Track rows only link a tiny thumbnail, but Google's image host will resize
/// to whatever the `=w..-h..` suffix says.
fn art_url(thumbnails: &[Thumbnail]) -> Option<String> {
  let best = thumbnails.iter().max_by_key(|thumbnail| thumbnail.width.unwrap_or(0))?;
  match best.url.rsplit_once('=') {
    Some((base, size)) if size.starts_with('w') => Some(format!("{base}=w544-h544-l90-rj")),
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
