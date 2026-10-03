//! Widgets shared by every screen.

use std::time::Duration;

use ratatui::{
  Frame,
  layout::{Constraint, Layout, Rect},
  style::{Color, Modifier, Style},
  text::{Line, Span},
  widgets::{Block, LineGauge, Paragraph},
};
use st_proto::catalog::Track;

use super::{
  Hit, Target,
  player::{Player, Status},
};

pub fn artists(track: &Track) -> String {
  track.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
}

/// Parses a "m:ss" or "h:mm:ss" duration.
pub fn parse_duration(text: &str) -> Option<Duration> {
  let mut seconds = 0_u64;
  for part in text.trim().split(':') {
    seconds = seconds * 60 + part.parse::<u64>().ok()?;
  }
  Some(Duration::from_secs(seconds))
}

pub fn format_time(time: Duration) -> String {
  let seconds = time.as_secs();
  match seconds / 3600 {
    0 => format!("{}:{:02}", seconds / 60, seconds % 60),
    hours => format!("{hours}:{:02}:{:02}", seconds / 60 % 60, seconds % 60),
  }
}

/// Draws a two line track entry: the title, then the artists and length.
pub fn render_track(frame: &mut Frame, area: Rect, track: &Track, selected: bool, playing: bool) {
  let mut title = Style::default().add_modifier(Modifier::BOLD);
  let mut detail = Style::default().fg(Color::Gray);
  if playing {
    title = title.fg(Color::Green);
  }
  if selected {
    title = title.bg(Color::DarkGray);
    detail = detail.bg(Color::DarkGray);
  }
  let marker = if playing { "> " } else { "  " };
  let mut subtitle = artists(track);
  if let Some(duration) = &track.duration {
    subtitle.push_str(&format!("  {duration}"));
  }
  let lines = vec![
    Line::from(vec![Span::styled(marker, title), Span::styled(track.title.as_str(), title)]),
    Line::from(vec![Span::styled("  ", detail), Span::styled(subtitle, detail)]),
  ];
  // Fill the whole row so the selection highlight spans the column.
  frame.render_widget(
    Paragraph::new(lines).style(if selected { detail } else { Style::default() }),
    area,
  );
}

/// The always-visible player status at the bottom of the screen.
pub fn render_status_bar(
  frame: &mut Frame,
  area: Rect,
  player: &Player,
  hint: &str,
  hits: &mut Vec<Hit>,
) {
  hits.push(Hit { rect: area, target: Target::Status });
  let block = Block::bordered().title(" steak ").title_bottom(Line::from(hint).right_aligned());
  let inner = block.inner(area);
  frame.render_widget(block, area);
  let [top, bottom] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);

  let (state, style) = match player.status() {
    Status::Idle => ("Nothing playing".to_owned(), Style::default().fg(Color::Gray)),
    Status::Loading => ("Loading...".to_owned(), Style::default().fg(Color::Yellow)),
    Status::Playing => ("Playing".to_owned(), Style::default().fg(Color::Green)),
    Status::Paused => ("Paused".to_owned(), Style::default().fg(Color::Yellow)),
    Status::Finished => ("Finished".to_owned(), Style::default().fg(Color::Gray)),
    Status::Failed(error) => (format!("Error: {error}"), Style::default().fg(Color::Red)),
    Status::Disconnected(reason) => {
      (format!("Daemon unreachable ({reason}); retrying"), Style::default().fg(Color::Red))
    }
  };
  let mut spans = vec![Span::styled(format!("{state}  "), style)];
  if let Some(track) = player.current() {
    spans.push(Span::styled(track.title.as_str(), Style::default().add_modifier(Modifier::BOLD)));
    spans.push(Span::raw(format!(" -- {}", artists(track))));
  }
  frame.render_widget(Paragraph::new(Line::from(spans)), top);

  let elapsed = player.elapsed();
  let total = player.duration();
  let ratio = match total {
    Some(total) if !total.is_zero() => {
      (elapsed.as_secs_f64() / total.as_secs_f64()).clamp(0.0, 1.0)
    }
    _ => 0.0,
  };
  let [left, bar, right] =
    Layout::horizontal([Constraint::Length(8), Constraint::Min(1), Constraint::Length(8)])
      .areas(bottom);
  frame.render_widget(Paragraph::new(format_time(elapsed)), left);
  frame.render_widget(
    LineGauge::default()
      .ratio(ratio)
      .label("")
      .filled_style(Style::default().fg(Color::Green))
      .unfilled_style(Style::default().fg(Color::DarkGray)),
    bar,
  );
  let total = total.map(format_time).unwrap_or_else(|| "--:--".into());
  frame.render_widget(Paragraph::new(total).right_aligned(), right);
}
