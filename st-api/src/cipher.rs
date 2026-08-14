//! Player-script deciphering extension points.
//!
//! Some player responses encode a stream URL as `signatureCipher` and may also
//! require transforming its `n` query parameter. The player JavaScript changes
//! regularly, so keep this logic replaceable and fixture-tested.

use reqwest::Url;

/// A URL embedded in a player response before any deciphering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamUrl {
  Direct(Url),
  SignatureCipher(String),
}

/// Resolves a usable, short-lived media URL from player data.
pub trait CipherResolver {
  fn resolve(&self, player_script_url: &Url, stream: &StreamUrl) -> Result<Url, CipherError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CipherError {
  NotImplemented,
  MissingSignature,
  InvalidUrl,
  PlayerScriptChanged,
}

impl std::fmt::Display for CipherError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::NotImplemented => f.write_str("player cipher resolution is not implemented"),
      Self::MissingSignature => f.write_str("cipher payload has no signature"),
      Self::InvalidUrl => f.write_str("cipher payload contains an invalid URL"),
      Self::PlayerScriptChanged => f.write_str("player script extraction no longer matches"),
    }
  }
}

impl std::error::Error for CipherError {}

/// Placeholder resolver, useful while wiring the rest of a client.
#[derive(Clone, Debug, Default)]
pub struct UnimplementedCipherResolver;

impl CipherResolver for UnimplementedCipherResolver {
  fn resolve(&self, _: &Url, _: &StreamUrl) -> Result<Url, CipherError> {
    Err(CipherError::NotImplemented)
  }
}
