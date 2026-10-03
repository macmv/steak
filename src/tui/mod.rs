//! The terminal UI: a homepage of tracks, and the radio player.
//!
//! This is a client of the daemon and nothing else: the homepage, the album
//! art and the whole player state come over its socket (see `client.rs`), and
//! every key that does something is a command sent back. Any number of these
//! can be attached at once.
//!
//! The player's status is always visible along the bottom; the player screen
//! expands it to show the album art and the radio queue.

mod home;
mod player;
mod ui;

use std::{error::Error, io::stdout, time::Duration};

use crossterm::event::{
  DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
  KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use futures_util::StreamExt;
use image::DynamicImage;
use ratatui::{
  DefaultTerminal, Frame,
  layout::{Constraint, Layout, Position, Rect},
};
use ratatui_image::picker::Picker;
use st_proto::{
  catalog::{Shelf, Track},
  ipc::{PlayerView, Request, Response},
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use self::{home::Home, player::Player};
use crate::client;

/// How many pages of homepage shelves to load.
const HOME_PAGES: u32 = 3;
/// How long to wait before trying to reach the daemon again.
const RECONNECT_AFTER: Duration = Duration::from_secs(1);
const STATUS_BAR_HEIGHT: u16 = 4;

/// Results of background work, delivered to the event loop.
pub enum Msg {
  Shelves(Vec<Shelf>),
  HomeFailed(String),
  /// The daemon's player changed (or this is its state on connecting).
  Player(PlayerView),
  /// The connection to the daemon dropped; it is being re-established.
  Disconnected(String),
  CommandFailed(String),
  Art {
    video_id: String,
    image:    DynamicImage,
  },
}

/// Something on screen that the mouse can hit.
#[derive(Clone, Copy)]
pub enum Target {
  HomeColumn(usize),
  HomeItem(usize, usize),
  /// The radio queue, or one entry of it.
  Queue(Option<usize>),
  Status,
}

pub struct Hit {
  pub rect:   Rect,
  pub target: Target,
}

#[derive(Clone, Copy, PartialEq)]
enum Screen {
  Home,
  Player,
}

struct App {
  screen: Screen,
  home:   Home,
  player: Player,
  picker: Picker,
  tx:     UnboundedSender<Msg>,
  /// What the last frame drew where; the mouse is resolved against it.
  hits:   Vec<Hit>,
  quit:   bool,
}

pub async fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
  let (tx, rx) = mpsc::unbounded_channel();
  load_home(tx.clone());
  watch_player(tx.clone());

  let mut terminal = ratatui::init();
  crossterm::execute!(stdout(), EnableMouseCapture)?;
  // Asks the terminal what image protocol it speaks; needs the raw terminal.
  let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
  let app = App {
    screen: Screen::Home,
    home: Home::new(),
    player: Player::new(tx.clone()),
    picker,
    tx,
    hits: Vec::new(),
    quit: false,
  };
  let result = app.event_loop(&mut terminal, rx).await;
  let _ = crossterm::execute!(stdout(), DisableMouseCapture);
  ratatui::restore();
  result
}

fn load_home(tx: UnboundedSender<Msg>) {
  tokio::spawn(async move {
    let msg = match client::call(Request::Home { more: HOME_PAGES - 1 }).await {
      Ok(Response::Home { shelves, .. }) => Msg::Shelves(shelves),
      Ok(other) => Msg::HomeFailed(format!("the daemon sent an unexpected response: {other:?}")),
      Err(error) => Msg::HomeFailed(error.to_string()),
    };
    let _ = tx.send(msg);
  });
}

/// Keeps a subscription to the daemon's player open, forever: when the
/// connection drops (the daemon restarted, say) it says so and tries again.
fn watch_player(tx: UnboundedSender<Msg>) {
  tokio::spawn(async move {
    loop {
      let error = match client::subscribe().await {
        Ok(mut subscription) => loop {
          match subscription.next().await {
            Ok(view) => {
              if tx.send(Msg::Player(view)).is_err() {
                return; // The UI is gone.
              }
            }
            Err(error) => break error,
          }
        },
        Err(error) => error,
      };
      if tx.send(Msg::Disconnected(error.to_string())).is_err() {
        return;
      }
      tokio::time::sleep(RECONNECT_AFTER).await;
    }
  });
}

impl App {
  async fn event_loop(
    mut self,
    terminal: &mut DefaultTerminal,
    mut rx: UnboundedReceiver<Msg>,
  ) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut events = EventStream::new();
    while !self.quit {
      terminal.draw(|frame| self.draw(frame))?;
      // Redraw when the clock ticks over. Everything else redraws because a
      // key or a message arrived.
      let tick = self.player.until_next_second();
      tokio::select! {
        Some(event) = events.next() => match event? {
          Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
          Event::Mouse(mouse) => self.on_mouse(mouse),
          _ => {}
        },
        Some(msg) = rx.recv() => self.on_msg(msg),
        _ = async {
          match tick {
            Some(wait) => tokio::time::sleep(wait).await,
            None => std::future::pending().await,
          }
        } => {}
      }
    }
    Ok(())
  }

  fn draw(&mut self, frame: &mut Frame) {
    self.hits.clear();
    let [main, bar] = Layout::vertical([Constraint::Min(0), Constraint::Length(STATUS_BAR_HEIGHT)])
      .areas(frame.area());
    let hint = match self.screen {
      Screen::Home => "arrows/hjkl: move  enter: start radio  tab: player  q: quit",
      Screen::Player => "space: pause  n/p: next/prev  enter: play  esc: back  q: quit",
    };
    match self.screen {
      Screen::Home => {
        let playing = self.player.current().map(|track| track.video_id.as_str());
        self.home.render(frame, main, playing, &mut self.hits);
      }
      Screen::Player => self.player.render(frame, main, &mut self.hits),
    }
    ui::render_status_bar(frame, bar, &self.player, hint, &mut self.hits);
  }

  fn on_msg(&mut self, msg: Msg) {
    match msg {
      Msg::Shelves(shelves) => {
        self.home.add_shelves(shelves);
        self.home.loading = false;
        self.home.error = None;
      }
      Msg::HomeFailed(error) => {
        self.home.loading = false;
        self.home.error = Some(error);
      }
      Msg::Player(view) => {
        // The daemon is reachable, so a homepage that failed to load may work
        // now.
        self.reload_home_if_failed();
        self.player.on_view(view);
      }
      Msg::Disconnected(reason) => self.player.on_disconnected(reason),
      Msg::CommandFailed(error) => self.player.on_command_failed(error),
      Msg::Art { video_id, image } => {
        self.player.set_art(video_id, self.picker.new_resize_protocol(image));
      }
    }
  }

  fn reload_home_if_failed(&mut self) {
    if self.home.error.take().is_some() {
      self.home.loading = true;
      load_home(self.tx.clone());
    }
  }

  /// Starts the radio from `track` and opens the player.
  fn start_radio(&mut self, track: Track) {
    self.player.start_radio(track);
    self.screen = Screen::Player;
  }

  fn toggle_screen(&mut self) {
    self.screen = match self.screen {
      Screen::Player => Screen::Home,
      Screen::Home if self.player.current().is_some() => Screen::Player,
      Screen::Home => Screen::Home,
    };
  }

  fn on_key(&mut self, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
      self.quit = true;
      return;
    }
    match (self.screen, key.code) {
      (_, KeyCode::Char('q')) => self.quit = true,
      (_, KeyCode::Char(' ')) => self.player.toggle_pause(),
      (_, KeyCode::Char('n')) => self.player.next(),
      (_, KeyCode::Tab) => self.toggle_screen(),

      (Screen::Home, KeyCode::Down | KeyCode::Char('j')) => self.home.move_row(1),
      (Screen::Home, KeyCode::Up | KeyCode::Char('k')) => self.home.move_row(-1),
      (Screen::Home, KeyCode::Left | KeyCode::Char('h')) => self.home.move_col(-1),
      (Screen::Home, KeyCode::Right | KeyCode::Char('l')) => self.home.move_col(1),
      (Screen::Home, KeyCode::Enter) => {
        if let Some(track) = self.home.selected_track().cloned() {
          self.start_radio(track);
        }
      }

      (Screen::Player, KeyCode::Char('p')) => self.player.previous(),
      (Screen::Player, KeyCode::Down | KeyCode::Char('j')) => self.player.move_cursor(1),
      (Screen::Player, KeyCode::Up | KeyCode::Char('k')) => self.player.move_cursor(-1),
      (Screen::Player, KeyCode::Enter) => self.player.play_cursor(),
      (Screen::Player, KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('b')) => {
        self.screen = Screen::Home;
      }
      _ => {}
    }
  }

  /// The topmost thing drawn at the mouse. Items are drawn after the columns
  /// that contain them, so the last match wins.
  fn hit(&self, mouse: MouseEvent) -> Option<Target> {
    let position = Position::new(mouse.column, mouse.row);
    self.hits.iter().rev().find(|hit| hit.rect.contains(position)).map(|hit| hit.target)
  }

  fn on_mouse(&mut self, mouse: MouseEvent) {
    let Some(target) = self.hit(mouse) else { return };
    match mouse.kind {
      MouseEventKind::Down(MouseButton::Left) => match target {
        Target::HomeItem(col, idx) => {
          self.home.select(col, idx);
          if let Some(track) = self.home.track(col, idx).cloned() {
            self.start_radio(track);
          }
        }
        Target::Queue(Some(idx)) => self.player.play_index(idx),
        Target::Status => self.toggle_screen(),
        Target::HomeColumn(_) | Target::Queue(None) => {}
      },
      MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
        let delta = if mouse.kind == MouseEventKind::ScrollDown { 1 } else { -1 };
        match target {
          Target::HomeColumn(col) | Target::HomeItem(col, _) => self.home.scroll(col, delta),
          Target::Queue(_) => self.player.move_cursor(delta),
          Target::Status => {}
        }
      }
      _ => {}
    }
  }
}
