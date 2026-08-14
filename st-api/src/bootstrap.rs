//! Bootstrap data obtained from the YouTube Music web application.
//!
//! A production bootstrapper should fetch the Music landing page, extract its
//! current Innertube configuration, and retain the player JavaScript URL. That
//! extraction is intentionally left as an integration-specific implementation.

use crate::music::MusicClientConfig;

/// Values that change with the Music web application deployment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BootstrapData {
  pub api_key:           String,
  pub client_version:    String,
  pub visitor_data:      Option<String>,
  pub player_script_url: Option<String>,
}

impl BootstrapData {
  /// Applies the request-related values to a Music client configuration.
  pub fn apply_to(&self, config: &mut MusicClientConfig) {
    config.api_key = Some(self.api_key.clone());
    config.context.client_version = self.client_version.clone();
    config.context.visitor_data = self.visitor_data.clone();
  }
}

/// A source capable of retrieving current bootstrap data.
pub trait Bootstrapper {
  fn bootstrap(
    &self,
  ) -> impl std::future::Future<Output = Result<BootstrapData, BootstrapError>> + Send;
}

/// Bootstrap is not implemented by the transport-only sketch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BootstrapError {
  NotImplemented,
  MissingField(&'static str),
  InvalidConfig,
}

impl std::fmt::Display for BootstrapError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::NotImplemented => f.write_str("YouTube Music bootstrap extraction is not implemented"),
      Self::MissingField(field) => write!(f, "bootstrap configuration is missing {field}"),
      Self::InvalidConfig => f.write_str("bootstrap configuration is invalid"),
    }
  }
}

impl std::error::Error for BootstrapError {}

/// Placeholder web bootstrapper.
#[derive(Clone, Debug, Default)]
pub struct WebBootstrapper;

impl Bootstrapper for WebBootstrapper {
  fn bootstrap(
    &self,
  ) -> impl std::future::Future<Output = Result<BootstrapData, BootstrapError>> + Send {
    async { Err(BootstrapError::NotImplemented) }
  }
}
