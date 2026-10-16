//! Playback statistics: the pings the web player sends so YouTube counts a
//! play toward history and recommendations.
//!
//! Everything session-specific comes from the player response's
//! `playbackTracking` block: its base URLs already carry the video's `docid`,
//! `ei`, `plid`, `of`, `osid`, `vm`, `cl`, `fexp`, `len` and `uga`, so none of
//! them are built here. This module adds what the player itself knows: a
//! client playback nonce (`cpn`), how far playback got, and which stretches
//! were actually played.
//!
//! The pings are `POST`s to `music.youtube.com/api/stats/...` with the
//! signed-in page's `XSRF_TOKEN` as a `session_token` form field (see
//! `WebBootstrapper::session_token`). The server answers 204.
//!
//! [`WatchSession`] is the pure part: it is told what the player is doing and
//! when, and says which pings are due. It does no I/O and reads no clock, so
//! it is tested with made-up times.

use std::hash::{BuildHasher, RandomState};

use reqwest::Url;
use serde_json::Value;

const CPN_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// The host the web player sends its pings to. The response names
/// `s.youtube.com`, which the browser reaches through this one.
const STATS_HOST: &str = "music.youtube.com";

/// A fresh client playback nonce: 16 characters of URL-safe base64, chosen once
/// per playback. It ties the stream requests to the pings.
pub fn new_cpn() -> String {
  let mut cpn = String::with_capacity(16);
  for round in 0..2_u64 {
    // Not secret and not cryptographic; it only has to differ per playback.
    let bits = RandomState::new().hash_one(round);
    for index in 0..8 {
      cpn.push(CPN_ALPHABET[((bits >> (index * 6)) & 63) as usize] as char);
    }
  }
  cpn
}

/// Adds `cpn` to a media URL. The rest of the query stays byte-for-byte as it
/// was, since googlevideo URLs are signed.
pub fn with_cpn(mut url: Url, cpn: &str) -> Url {
  let mut query = url.query().unwrap_or_default().to_owned();
  if query.split('&').any(|pair| pair.starts_with("cpn=")) {
    return url;
  }
  if !query.is_empty() {
    query.push('&');
  }
  query.push_str("cpn=");
  query.push_str(cpn);
  url.set_query(Some(&query));
  url
}

/// What a ping says about the player.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
  Playing,
  Paused,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PingKind {
  /// Playback began.
  Playback,
  /// Progress so far.
  Watchtime,
}

/// One statistics report, before it is turned into a request.
#[derive(Clone, Debug, PartialEq)]
pub struct Ping {
  pub kind:     PingKind,
  /// `cmt`: where playback is in the track, in seconds.
  pub media:    f64,
  /// `rt`: seconds since this session was opened.
  pub elapsed:  f64,
  /// `rtn`: seconds the client has been running.
  pub uptime:   f64,
  pub state:    State,
  /// `st`/`et`: the stretches of the track played since the last ping.
  pub segments: Vec<(f64, f64)>,
  /// The last ping of this playback.
  pub is_final: bool,
}

/// The tracking URLs of one playback, from its player response.
#[derive(Clone, Debug)]
pub struct PlaybackTracking {
  pub cpn:    String,
  playback:   Url,
  watchtime:  Url,
  itag:       u32,
  /// Seconds into the session at which the first watchtime pings are due.
  scheduled:  Vec<f64>,
  /// The gap between pings once `scheduled` runs out.
  every:      f64,
  /// The track's length, from the base URLs' `len`.
  pub length: Option<f64>,
}

impl PlaybackTracking {
  /// Reads the tracking block of a `player` response. `itag` is the audio
  /// format being played. `None` if the response does not carry one (a
  /// signed-out session, say), in which case there is nothing to report.
  pub fn from_player(response: &Value, itag: u32) -> Option<Self> {
    let url = |name: &str| {
      response
        .pointer(&format!("/playbackTracking/{name}/baseUrl"))
        .and_then(Value::as_str)
        .and_then(|url| Url::parse(url).ok())
    };
    let playback = url("videostatsPlaybackUrl")?;
    let watchtime = url("videostatsWatchtimeUrl")?;
    let tracking = response.get("playbackTracking")?;
    let scheduled = tracking
      .get("videostatsScheduledFlushWalltimeSeconds")
      .and_then(Value::as_array)
      .map(|seconds| seconds.iter().filter_map(Value::as_f64).collect())
      .unwrap_or_default();
    let every = tracking
      .get("videostatsDefaultFlushIntervalSeconds")
      .and_then(Value::as_f64)
      .filter(|every| *every > 0.0)
      .unwrap_or(40.0);
    let length = playback
      .query_pairs()
      .find(|(name, _)| name == "len")
      .and_then(|(_, value)| value.parse().ok());
    Some(Self { cpn: new_cpn(), playback, watchtime, itag, scheduled, every, length })
  }

  /// The URL to `POST` for `ping`. `client` is the web client's identity (see
  /// `MusicClient::stats_params`).
  pub fn url(&self, ping: &Ping, client: &[(&'static str, String)]) -> Url {
    let mut url = match ping.kind {
      PingKind::Playback => self.playback.clone(),
      PingKind::Watchtime => self.watchtime.clone(),
    };
    let _ = url.set_host(Some(STATS_HOST));
    let present: Vec<String> = url.query_pairs().map(|(name, _)| name.into_owned()).collect();

    let mut params: Vec<(&str, String)> = vec![
      ("ns", "yt".into()),
      ("el", "detailpage".into()),
      ("cpn", self.cpn.clone()),
      ("ver", "2".into()),
      ("cmt", seconds(ping.media)),
      ("fmt", "0".into()),
      ("fs", "0".into()),
      ("rt", seconds(ping.elapsed)),
      ("euri", String::new()),
    ];
    let count = ping.segments.len().max(1);
    let each = |value: &str| vec![value; count].join(",");
    match ping.kind {
      PingKind::Playback => {
        params.push(("mos", "0".into()));
        params.push(("volume", "100".into()));
      }
      PingKind::Watchtime => {
        params
          .push(("state", if ping.state == State::Playing { "playing" } else { "paused" }.into()));
        params.push(("volume", each("100")));
      }
    }
    params.extend(client.iter().map(|(name, value)| (*name, value.clone())));
    params.push(("rtn", seconds(ping.uptime.max(ping.elapsed))));
    params.push(("afmt", self.itag.to_string()));
    if ping.kind == PingKind::Watchtime {
      params.push(("rti", format!("{}", ping.elapsed.round() as u64)));
      let join = |pick: fn(&(f64, f64)) -> f64| {
        ping.segments.iter().map(|segment| seconds(pick(segment))).collect::<Vec<_>>().join(",")
      };
      params.push(("st", join(|segment| segment.0)));
      params.push(("et", join(|segment| segment.1)));
    }
    params.push(("muted", each("0")));
    params.push(("vis", each("10")));
    if ping.is_final {
      params.push(("final", "1".into()));
    }

    let mut query = url.query_pairs_mut();
    for (name, value) in params {
      if !present.iter().any(|existing| existing == name) {
        query.append_pair(name, &value);
      }
    }
    drop(query);
    url
  }
}

/// Seconds as the player writes them: three decimals, no trailing zeros.
fn seconds(value: f64) -> String {
  let text = format!("{:.3}", value.max(0.0));
  text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// Decides which pings are due, from what the player reports.
///
/// Times (`now`) are seconds since the session was opened.
pub struct WatchSession {
  scheduled:    Vec<f64>,
  every:        f64,
  /// Seconds the client had been running when the session opened.
  uptime_start: f64,
  started:      bool,
  state:        State,
  /// Where the stretch being played began, while one is open.
  open:         Option<f64>,
  segments:     Vec<(f64, f64)>,
  flushes:      usize,
  next_flush:   Option<f64>,
  finished:     bool,
}

impl WatchSession {
  pub fn new(tracking: &PlaybackTracking, uptime_start: f64) -> Self {
    Self {
      scheduled: tracking.scheduled.clone(),
      every: tracking.every,
      uptime_start,
      started: false,
      state: State::Playing,
      open: None,
      segments: Vec::new(),
      flushes: 0,
      next_flush: None,
      finished: false,
    }
  }

  /// When the next periodic watchtime ping is due, if the track is playing.
  pub fn next_flush(&self) -> Option<f64> {
    (self.state == State::Playing && !self.finished).then_some(self.next_flush).flatten()
  }

  pub fn started(&self) -> bool { self.started }

  /// Feeds the player's state at `now`, with the track `position` in seconds.
  /// Returns the pings that are due.
  pub fn observe(&mut self, now: f64, state: State, position: f64) -> Vec<Ping> {
    if self.finished {
      return Vec::new();
    }
    if !self.started {
      if state != State::Playing {
        return Vec::new();
      }
      self.started = true;
      self.state = State::Playing;
      self.open = Some(position);
      self.next_flush = Some(self.flush_time(0));
      return vec![self.ping(PingKind::Playback, now, position, false)];
    }

    let mut pings = Vec::new();
    match (self.state, state) {
      (State::Playing, State::Paused) => {
        self.close(position);
        self.state = State::Paused;
        pings.push(self.flush(now, position, false));
      }
      (State::Paused, State::Playing) => {
        self.state = State::Playing;
        self.open = Some(position);
        self.next_flush = Some(self.flush_time(self.flushes).max(now + 1.0));
      }
      (State::Playing, State::Playing) if self.next_flush.is_some_and(|due| now >= due) => {
        self.close(position);
        self.open = Some(position);
        pings.push(self.flush(now, position, false));
      }
      _ => {}
    }
    pings
  }

  /// Ends the session at `position`, with the last ping. Nothing is sent if
  /// playback never began.
  pub fn finish(&mut self, now: f64, position: f64) -> Vec<Ping> {
    if self.finished || !self.started {
      self.finished = true;
      return Vec::new();
    }
    self.close(position);
    if self.segments.is_empty() {
      self.segments.push((position, position));
    }
    let ping = self.flush(now, position, true);
    self.finished = true;
    vec![ping]
  }

  fn close(&mut self, position: f64) {
    if let Some(start) = self.open.take() {
      if position > start {
        self.segments.push((start, position));
      }
    }
  }

  fn flush(&mut self, now: f64, position: f64, is_final: bool) -> Ping {
    let ping = self.ping(PingKind::Watchtime, now, position, is_final);
    self.segments.clear();
    self.flushes += 1;
    self.next_flush = Some(self.flush_time(self.flushes).max(now + 1.0));
    ping
  }

  /// When flush number `index` is due: the schedule first, then a steady gap.
  fn flush_time(&self, index: usize) -> f64 {
    match self.scheduled.get(index) {
      Some(at) => *at,
      None => {
        self.scheduled.last().copied().unwrap_or(0.0)
          + self.every * (index + 1 - self.scheduled.len()) as f64
      }
    }
  }

  fn ping(&self, kind: PingKind, now: f64, position: f64, is_final: bool) -> Ping {
    Ping {
      kind,
      media: position,
      elapsed: now,
      uptime: self.uptime_start + now,
      state: self.state,
      segments: self.segments.clone(),
      is_final,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn player() -> Value {
    serde_json::json!({"playbackTracking": {
      "videostatsPlaybackUrl": {"baseUrl":
        "https://s.youtube.com/api/stats/playback?cl=1&docid=abc&ei=e&ns=yt&plid=p&el=detailpage&len=183&of=o&osid=s&uga=m32&vm=v"},
      "videostatsWatchtimeUrl": {"baseUrl":
        "https://s.youtube.com/api/stats/watchtime?cl=1&docid=abc&ei=e&ns=yt&plid=p&el=detailpage&len=183&of=o&osid=s&uga=m32&vm=v"},
      "videostatsScheduledFlushWalltimeSeconds": [10, 20],
      "videostatsDefaultFlushIntervalSeconds": 40,
    }})
  }

  fn tracking() -> PlaybackTracking { PlaybackTracking::from_player(&player(), 140).unwrap() }

  fn query(url: &Url, name: &str) -> Option<String> {
    url.query_pairs().find(|(key, _)| key == name).map(|(_, value)| value.into_owned())
  }

  #[test]
  fn cpn_is_sixteen_url_safe_characters_and_differs_per_playback() {
    let (a, b) = (new_cpn(), new_cpn());
    assert_eq!(a.len(), 16);
    assert!(a.bytes().all(|byte| CPN_ALPHABET.contains(&byte)));
    assert_ne!(a, b);
  }

  #[test]
  fn cpn_is_appended_without_touching_the_signed_query() {
    let url = Url::parse("https://e.test/v?sparams=a,b&sig=x%2Fy").unwrap();
    assert_eq!(with_cpn(url, "ABC").query(), Some("sparams=a,b&sig=x%2Fy&cpn=ABC"));
    let url = Url::parse("https://e.test/v?cpn=old").unwrap();
    assert_eq!(with_cpn(url, "ABC").query(), Some("cpn=old"));
  }

  #[test]
  fn tracking_needs_both_urls() {
    assert!(PlaybackTracking::from_player(&serde_json::json!({}), 140).is_none());
    let mut response = player();
    response["playbackTracking"].as_object_mut().unwrap().remove("videostatsWatchtimeUrl");
    assert!(PlaybackTracking::from_player(&response, 140).is_none());
    assert_eq!(tracking().length, Some(183.0));
  }

  #[test]
  fn watchtime_url_keeps_the_base_and_adds_progress() {
    let tracking = tracking();
    let ping = Ping {
      kind:     PingKind::Watchtime,
      media:    17.537,
      elapsed:  19.001,
      uptime:   29.0,
      state:    State::Playing,
      segments: vec![(7.573, 7.734), (7.734, 17.537)],
      is_final: false,
    };
    let client = [("c", "WEB_REMIX".to_owned())];
    let url = tracking.url(&ping, &client);
    assert_eq!(url.host_str(), Some("music.youtube.com"));
    assert_eq!(url.path(), "/api/stats/watchtime");
    assert_eq!(query(&url, "docid").as_deref(), Some("abc"));
    assert_eq!(query(&url, "cpn").as_deref(), Some(tracking.cpn.as_str()));
    assert_eq!(query(&url, "cmt").as_deref(), Some("17.537"));
    assert_eq!(query(&url, "rt").as_deref(), Some("19.001"));
    assert_eq!(query(&url, "rti").as_deref(), Some("19"));
    assert_eq!(query(&url, "st").as_deref(), Some("7.573,7.734"));
    assert_eq!(query(&url, "et").as_deref(), Some("7.734,17.537"));
    assert_eq!(query(&url, "volume").as_deref(), Some("100,100"));
    assert_eq!(query(&url, "muted").as_deref(), Some("0,0"));
    assert_eq!(query(&url, "state").as_deref(), Some("playing"));
    assert_eq!(query(&url, "afmt").as_deref(), Some("140"));
    assert_eq!(query(&url, "c").as_deref(), Some("WEB_REMIX"));
    assert_eq!(query(&url, "final"), None);
    // Parameters already in the base are not repeated.
    assert_eq!(url.query_pairs().filter(|(key, _)| key == "el").count(), 1);
  }

  #[test]
  fn playback_url_has_no_segments() {
    let ping = Ping {
      kind:     PingKind::Playback,
      media:    0.008,
      elapsed:  1.447,
      uptime:   5.0,
      state:    State::Playing,
      segments: Vec::new(),
      is_final: false,
    };
    let url = tracking().url(&ping, &[]);
    assert_eq!(url.path(), "/api/stats/playback");
    assert_eq!(query(&url, "cmt").as_deref(), Some("0.008"));
    assert_eq!(query(&url, "st"), None);
    assert_eq!(query(&url, "mos").as_deref(), Some("0"));
  }

  #[test]
  fn seconds_are_written_like_the_player_writes_them() {
    assert_eq!(seconds(0.008), "0.008");
    assert_eq!(seconds(29.0), "29");
    assert_eq!(seconds(7.5730001), "7.573");
    assert_eq!(seconds(-1.0), "0");
  }

  fn session() -> WatchSession { WatchSession::new(&tracking(), 100.0) }

  #[test]
  fn nothing_is_sent_until_playback_starts() {
    let mut session = session();
    assert!(session.observe(0.5, State::Paused, 0.0).is_empty());
    assert!(session.finish(1.0, 0.0).is_empty());
  }

  #[test]
  fn starting_pings_once_then_flushes_on_the_schedule() {
    let mut session = session();
    let start = session.observe(1.4, State::Playing, 0.0);
    assert_eq!(start.len(), 1);
    assert_eq!(start[0].kind, PingKind::Playback);
    assert_eq!(start[0].uptime, 101.4);
    assert_eq!(session.next_flush(), Some(10.0));

    assert!(session.observe(9.0, State::Playing, 7.6).is_empty());
    let first = session.observe(10.0, State::Playing, 8.6);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].kind, PingKind::Watchtime);
    assert_eq!(first[0].segments, vec![(0.0, 8.6)]);
    assert_eq!(session.next_flush(), Some(20.0));

    let second = session.observe(20.0, State::Playing, 18.6);
    assert_eq!(second[0].segments, vec![(8.6, 18.6)]);
    // Past the schedule, the default gap applies.
    assert_eq!(session.next_flush(), Some(60.0));
  }

  #[test]
  fn pausing_flushes_and_resuming_opens_a_new_stretch() {
    let mut session = session();
    session.observe(1.0, State::Playing, 0.0);
    let paused = session.observe(6.0, State::Paused, 5.0);
    assert_eq!(paused.len(), 1);
    assert_eq!(paused[0].state, State::Paused);
    assert_eq!(paused[0].segments, vec![(0.0, 5.0)]);
    assert_eq!(session.next_flush(), None);

    assert!(session.observe(30.0, State::Paused, 5.0).is_empty());
    assert!(session.observe(31.0, State::Playing, 5.0).is_empty());
    let end = session.finish(36.0, 10.0);
    assert_eq!(end[0].segments, vec![(5.0, 10.0)]);
    assert!(end[0].is_final);
    assert_eq!(end[0].state, State::Playing);
  }

  #[test]
  fn finishing_reports_the_unflushed_tail_once() {
    let mut session = session();
    session.observe(1.0, State::Playing, 0.0);
    let end = session.finish(4.0, 3.0);
    assert_eq!(end.len(), 1);
    assert_eq!(end[0].segments, vec![(0.0, 3.0)]);
    assert!(end[0].is_final);
    assert!(session.finish(5.0, 4.0).is_empty());
    assert!(session.observe(6.0, State::Playing, 5.0).is_empty());
  }
}
