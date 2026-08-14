//! Player-response and audio-format data contracts.

use crate::cipher::StreamUrl;

/// The selected audio stream, ready for cipher resolution and playback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioFormat {
  pub itag:           u32,
  pub mime_type:      String,
  pub bitrate:        u32,
  pub content_length: Option<u64>,
  pub stream_url:     StreamUrl,
}

/// Policy used to choose among audio-only adaptive formats.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioQuality {
  LowestBandwidth,
  Balanced,
  HighestBitrate,
}

/// Extracts audio formats from a raw `player` endpoint response.
pub trait PlayerParser {
  fn audio_formats(
    &self,
    player_response: &serde_json::Value,
  ) -> Result<Vec<AudioFormat>, PlayerError>;
  fn choose_audio(
    &self,
    formats: Vec<AudioFormat>,
    quality: AudioQuality,
  ) -> Result<AudioFormat, PlayerError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlayerError {
  NotImplemented,
  PlaybackUnavailable,
  NoAudioFormats,
  InvalidFormat,
}

impl std::fmt::Display for PlayerError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::NotImplemented => f.write_str("player response parsing is not implemented"),
      Self::PlaybackUnavailable => f.write_str("player response does not permit playback"),
      Self::NoAudioFormats => f.write_str("player response has no usable audio formats"),
      Self::InvalidFormat => f.write_str("player response contains an invalid audio format"),
    }
  }
}

impl std::error::Error for PlayerError {}

/// Placeholder parser.
#[derive(Clone, Debug, Default)]
pub struct UnimplementedPlayerParser;

impl PlayerParser for UnimplementedPlayerParser {
  fn audio_formats(&self, _: &serde_json::Value) -> Result<Vec<AudioFormat>, PlayerError> {
    Err(PlayerError::NotImplemented)
  }

  fn choose_audio(&self, _: Vec<AudioFormat>, _: AudioQuality) -> Result<AudioFormat, PlayerError> {
    Err(PlayerError::NotImplemented)
  }
}
