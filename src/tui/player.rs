//! The radio: a queue of tracks, the audio task playing the current one, and
//! the expanded player screen.

use std::{
  sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
  },
  time::{Duration, Instant},
};

use ratatui::{
  Frame,
  layout::{Constraint, Layout, Rect},
  style::{Color, Style},
  widgets::{Block, Paragraph},
};
use ratatui_image::{StatefulImage, protocol::StatefulProtocol};
use st_api::{
  catalog::{QueueParser, Thumbnail, Track},
  innertube::client::MusicClient,
};
use tokio::{sync::mpsc::UnboundedSender, task::JoinHandle};

use super::{Hit, Msg, Target, ui};

/// Start loading more of the radio when this few tracks remain.
const LOAD_MORE_WITHIN: usize = 3;
/// Past this much playback, "previous" restarts the track instead.
const RESTART_AFTER: Duration = Duration::from_secs(3);
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
}

pub struct Player {
  http:   st_api::Client,
  tx:     UnboundedSender<Msg>,
  client: Option<MusicClient>,

  queue:        Vec<Track>,
  index:        usize,
  /// The queue entry the keyboard has selected; follows `index` on track
  /// change.
  cursor:       usize,
  offset:       usize,
  continuation: Option<String>,
  loading_more: bool,
  /// Distinguishes radios, so a slow response for an abandoned one is ignored.
  radio_id:     u64,

  /// Distinguishes tracks, for the same reason.
  generation: u64,
  task:       Option<JoinHandle<()>>,
  /// ffplay's process ID once it is running, else 0.
  pid:        Arc<AtomicU32>,
  begun:      bool,
  paused:     bool,
  started:    Option<Instant>,
  accum:      Duration,
  error:      Option<String>,
  finished:   bool,

  art: Option<(String, StatefulProtocol)>,
}

impl Player {
  pub fn new(http: st_api::Client, tx: UnboundedSender<Msg>) -> Self {
    Self {
      http,
      tx,
      client: None,
      queue: Vec::new(),
      index: 0,
      cursor: 0,
      offset: 0,
      continuation: None,
      loading_more: false,
      radio_id: 0,
      generation: 0,
      task: None,
      pid: Arc::new(AtomicU32::new(0)),
      begun: false,
      paused: false,
      started: None,
      accum: Duration::ZERO,
      error: None,
      finished: false,
      art: None,
    }
  }

  pub fn current(&self) -> Option<&Track> { self.queue.get(self.index) }

  pub fn status(&self) -> Status {
    if let Some(error) = &self.error {
      Status::Failed(error.clone())
    } else if self.queue.is_empty() {
      Status::Idle
    } else if self.finished {
      Status::Finished
    } else if self.paused {
      Status::Paused
    } else if !self.begun {
      Status::Loading
    } else {
      Status::Playing
    }
  }

  pub fn elapsed(&self) -> Duration {
    self.accum + self.started.map_or(Duration::ZERO, |started| started.elapsed())
  }

  pub fn duration(&self) -> Option<Duration> {
    ui::parse_duration(self.current()?.duration.as_deref()?)
  }

  /// Replaces the queue with the song radio of `seed` and starts playing it.
  ///
  /// The seed plays right away; the rest of the radio fills in behind it.
  pub fn start_radio(&mut self, client: MusicClient, seed: Track) {
    self.radio_id += 1;
    self.client = Some(client.clone());
    self.continuation = None;
    self.loading_more = false;
    self.queue = vec![seed.clone()];
    self.fetch(client, seed.video_id, None);
    self.play_index(0);
  }

  fn fetch(&self, client: MusicClient, seed_id: String, token: Option<String>) {
    let tx = self.tx.clone();
    let radio_id = self.radio_id;
    let more = token.is_some();
    tokio::spawn(async move {
      let result = async {
        let response = match token {
          Some(token) => client.next_continuation(token).await?,
          None => client.next(&seed_id, Some(format!("RDAMVM{seed_id}"))).await?,
        };
        Ok::<_, reqwest::Error>((
          QueueParser.tracks(&response),
          QueueParser.continuation(&response),
        ))
      }
      .await
      .map_err(|error| error.to_string());
      let _ = tx.send(Msg::Radio { radio_id, more, result });
    });
  }

  pub fn on_radio(
    &mut self,
    radio_id: u64,
    more: bool,
    result: Result<(Vec<Track>, Option<String>), String>,
  ) {
    if radio_id != self.radio_id {
      return;
    }
    self.loading_more = false;
    let (tracks, continuation) = match result {
      Ok(batch) => batch,
      Err(error) => {
        log::warn!("loading the radio failed: {error}");
        return;
      }
    };
    if more {
      let before = self.queue.len();
      let fresh: Vec<Track> = tracks
        .into_iter()
        .filter(|track| !self.queue.iter().any(|queued| queued.video_id == track.video_id))
        .collect();
      self.queue.extend(fresh);
      // A batch with nothing new would otherwise be requested again forever.
      self.continuation = continuation.filter(|_| self.queue.len() > before);
    } else {
      // The radio normally opens with the seed, which is already playing.
      let seed = self.queue[0].clone();
      let rest: Vec<Track> =
        tracks.into_iter().filter(|track| track.video_id != seed.video_id).collect();
      self.queue = std::iter::once(seed).chain(rest).collect();
      self.continuation = continuation;
    }
    self.maybe_load_more();
  }

  fn maybe_load_more(&mut self) {
    if self.loading_more || self.index + LOAD_MORE_WITHIN < self.queue.len() {
      return;
    }
    let (Some(client), Some(token)) = (self.client.clone(), self.continuation.take()) else {
      return;
    };
    self.loading_more = true;
    self.fetch(client, self.queue[0].video_id.clone(), Some(token));
  }

  pub fn play_index(&mut self, index: usize) {
    let Some(track) = self.queue.get(index) else { return };
    let video_id = track.video_id.clone();
    let art_url = art_url(&track.thumbnails);
    self.index = index;
    self.cursor = index;

    // Dropping the old task also kills its ffplay.
    if let Some(task) = self.task.take() {
      task.abort();
    }
    self.generation += 1;
    self.pid = Arc::new(AtomicU32::new(0));
    self.begun = false;
    self.paused = false;
    self.started = None;
    self.accum = Duration::ZERO;
    self.error = None;
    self.finished = false;

    let (http, tx, generation, pid) =
      (self.http.clone(), self.tx.clone(), self.generation, self.pid.clone());
    let id = video_id.clone();
    self.task = Some(tokio::spawn(async move {
      let result = async {
        let audio = crate::open_audio(&http, &id).await?;
        crate::play(audio, true, &pid).await
      }
      .await
      .map_err(|error| error.to_string());
      let _ = tx.send(Msg::Finished { generation, result });
    }));

    if self.art.as_ref().is_some_and(|(id, _)| *id != video_id) {
      self.art = None;
    }
    if let Some(url) = art_url {
      let (http, tx) = (self.http.clone(), self.tx.clone());
      tokio::spawn(async move {
        match fetch_art(&http, &url).await {
          Ok(image) => {
            let _ = tx.send(Msg::Art { video_id, image });
          }
          Err(error) => log::warn!("album art failed: {error}"),
        }
      });
    }
    self.maybe_load_more();
  }

  pub fn set_art(&mut self, video_id: String, protocol: StatefulProtocol) {
    if self.current().is_some_and(|track| track.video_id == video_id) {
      self.art = Some((video_id, protocol));
    }
  }

  pub fn on_finished(&mut self, generation: u64, result: Result<(), String>) {
    if generation != self.generation {
      return;
    }
    match result {
      Err(error) => {
        log::error!("playback failed: {error}");
        self.error = Some(error);
      }
      Ok(()) if self.index + 1 < self.queue.len() => self.play_index(self.index + 1),
      Ok(()) => {
        self.finished = true;
        self.started = None;
      }
    }
  }

  pub fn next(&mut self) {
    if self.index + 1 < self.queue.len() {
      self.play_index(self.index + 1);
    }
  }

  pub fn previous(&mut self) {
    if self.index == 0 || self.elapsed() > RESTART_AFTER {
      self.play_index(self.index);
    } else {
      self.play_index(self.index - 1);
    }
  }

  pub fn play_cursor(&mut self) { self.play_index(self.cursor); }

  pub fn move_cursor(&mut self, delta: isize) {
    self.cursor = self.cursor.saturating_add_signed(delta).min(self.queue.len().saturating_sub(1));
  }

  pub fn toggle_pause(&mut self) {
    if self.current().is_none() || self.finished || self.error.is_some() {
      return;
    }
    self.paused = !self.paused;
    if !self.begun {
      return; // `tick` applies it once ffplay is running.
    }
    if self.paused {
      self.accum = self.elapsed();
      self.started = None;
      self.signal(libc::SIGSTOP);
    } else {
      self.started = Some(Instant::now());
      self.signal(libc::SIGCONT);
    }
  }

  fn signal(&self, signal: i32) {
    let pid = self.pid.load(Ordering::SeqCst);
    if pid != 0 {
      // SAFETY: plain syscall; a stale pid only fails.
      unsafe { libc::kill(pid as i32, signal) };
    }
  }

  /// Notices when ffplay has started, which is when the clock starts.
  pub fn tick(&mut self) {
    if !self.begun && self.pid.load(Ordering::SeqCst) != 0 {
      self.begun = true;
      if self.paused {
        self.signal(libc::SIGSTOP);
      } else {
        self.started = Some(Instant::now());
      }
    }
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

    let seed = self.queue.first().map_or("", |track| track.title.as_str());
    let block = Block::bordered()
      .title(format!(" Radio: {seed} "))
      .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(right);
    frame.render_widget(block, right);
    hits.push(Hit { rect: right, target: Target::Queue(None) });

    let rows = ((inner.height / ITEM_HEIGHT) as usize).max(1);
    if self.cursor < self.offset {
      self.offset = self.cursor;
    } else if self.cursor >= self.offset + rows {
      self.offset = self.cursor + 1 - rows;
    }
    for (slot, idx) in (self.offset..self.queue.len()).take(rows).enumerate() {
      let rect = Rect {
        x:      inner.x,
        y:      inner.y + slot as u16 * ITEM_HEIGHT,
        width:  inner.width,
        height: ITEM_HEIGHT.min(inner.height),
      };
      ui::render_track(frame, rect, &self.queue[idx], idx == self.cursor, idx == self.index);
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

async fn fetch_art(
  http: &st_api::Client,
  url: &str,
) -> Result<image::DynamicImage, Box<dyn std::error::Error + Send + Sync>> {
  let bytes = http.get(url).send().await?.error_for_status()?.bytes().await?;
  Ok(tokio::task::spawn_blocking(move || image::load_from_memory(&bytes)).await??)
}
