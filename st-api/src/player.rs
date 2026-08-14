//! Player-response and audio-format data contracts.

use crate::cipher::StreamUrl;
use log::{debug, warn};

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

/// Standard parser for the documented `streamingData.adaptiveFormats` shape.
#[derive(Clone, Debug, Default)]
pub struct StandardPlayerParser;

impl PlayerParser for StandardPlayerParser {
  fn audio_formats(&self, response: &serde_json::Value) -> Result<Vec<AudioFormat>, PlayerError> {
    let status = response.pointer("/playabilityStatus/status").and_then(serde_json::Value::as_str);
    if status != Some("OK") {
      // This usually explains whether the video is private, age/region
      // restricted, requires login, or simply has no playback entitlement.
      warn!(
        "YouTube playback is unavailable: status={:?}, reason={:?}",
        status,
        response.pointer("/playabilityStatus/reason").and_then(serde_json::Value::as_str),
      );
      debug!(
        "full YouTube playability status: {}",
        response.pointer("/playabilityStatus").unwrap_or(&serde_json::Value::Null),
      );
      return Err(PlayerError::PlaybackUnavailable);
    }
    let formats = response
      .pointer("/streamingData/adaptiveFormats")
      .and_then(serde_json::Value::as_array)
      .ok_or(PlayerError::NoAudioFormats)?;
    let mut audio = Vec::new();
    for format in formats {
      let mime_type = format
        .get("mimeType")
        .and_then(serde_json::Value::as_str)
        .ok_or(PlayerError::InvalidFormat)?;
      if !mime_type.starts_with("audio/") {
        continue;
      }
      let itag = format
        .get("itag")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| value.try_into().ok())
        .ok_or(PlayerError::InvalidFormat)?;
      let bitrate = format
        .get("bitrate")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| value.try_into().ok())
        .ok_or(PlayerError::InvalidFormat)?;
      let content_length = match format.get("contentLength") {
        Some(serde_json::Value::String(value)) => value.parse().ok(),
        Some(value) => value.as_u64(),
        None => None,
      };
      let stream_url = if let Some(url) = format.get("url").and_then(serde_json::Value::as_str) {
        StreamUrl::Direct(reqwest::Url::parse(url).map_err(|_| PlayerError::InvalidFormat)?)
      } else if let Some(cipher) = format
        .get("signatureCipher")
        .or_else(|| format.get("cipher"))
        .and_then(serde_json::Value::as_str)
      {
        StreamUrl::SignatureCipher(cipher.into())
      } else {
        return Err(PlayerError::InvalidFormat);
      };
      audio.push(AudioFormat {
        itag,
        mime_type: mime_type.into(),
        bitrate,
        content_length,
        stream_url,
      });
    }
    debug!("extracted {} audio-only player formats", audio.len());
    if audio.is_empty() { Err(PlayerError::NoAudioFormats) } else { Ok(audio) }
  }

  fn choose_audio(
    &self,
    mut formats: Vec<AudioFormat>,
    quality: AudioQuality,
  ) -> Result<AudioFormat, PlayerError> {
    if formats.is_empty() {
      return Err(PlayerError::NoAudioFormats);
    }
    formats.sort_by_key(|format| format.bitrate);
    let index = match quality {
      AudioQuality::LowestBandwidth => 0,
      AudioQuality::HighestBitrate => formats.len() - 1,
      AudioQuality::Balanced => formats
        .iter()
        .enumerate()
        .min_by_key(|(_, format)| format.bitrate.abs_diff(160_000))
        .map(|(index, _)| index)
        .expect("formats is not empty"),
    };
    let selected = formats.swap_remove(index);
    debug!(
      "selected audio format itag={}, mime={}, bitrate={}",
      selected.itag, selected.mime_type, selected.bitrate
    );
    Ok(selected)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn extracts_audio_only_formats() {
    let response = serde_json::json!({"playabilityStatus":{"status":"OK"},"streamingData":{"adaptiveFormats":[
      {"itag": 251, "mimeType":"audio/webm; codecs=opus", "bitrate":160000, "contentLength":"42", "url":"https://example.test/audio"},
      {"itag": 137, "mimeType":"video/mp4", "bitrate":400000, "url":"https://example.test/video"}
    ]}});
    let formats = StandardPlayerParser.audio_formats(&response).unwrap();
    assert_eq!(formats.len(), 1);
    assert_eq!(formats[0].content_length, Some(42));
  }
}
