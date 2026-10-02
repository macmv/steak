//! The authorization and delivery decision tree for a non-DRM stream.
//!
//! This module intentionally contains no YouTube protocol implementation.
//! Its small traits are the stubs to fill in for signature, n-challenge,
//! PoToken, SABR, and direct-URL support.

use reqwest::Url;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Delivery {
  Sabr,
  Direct,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamOffer {
  pub delivery:              Delivery,
  pub url:                   Url,
  pub is_drm_protected:      bool,
  pub encrypted_signature:   Option<String>,
  pub n_challenge:           Option<String>,
  pub requires_gvs_po_token: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamFlowError {
  DrmProtected,
  Signature(String),
  NChallenge(String),
  PoToken(String),
  Delivery(String),
}

impl std::fmt::Display for StreamFlowError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::DrmProtected => f.write_str("DRM-protected streams are not supported"),
      Self::Signature(error) => write!(f, "signature resolution failed: {error}"),
      Self::NChallenge(error) => write!(f, "n challenge resolution failed: {error}"),
      Self::PoToken(error) => write!(f, "GVS PoToken acquisition failed: {error}"),
      Self::Delivery(error) => write!(f, "stream delivery setup failed: {error}"),
    }
  }
}

impl std::error::Error for StreamFlowError {}

/// Protocol-specific operations. Implement these independently; the flow below
/// ensures they are invoked only when the selected offer requires them.
pub trait StreamRequirements {
  fn resolve_signature(&self, url: Url, encrypted: &str) -> Result<Url, String>;
  fn solve_n(&self, player_url: &Url, challenge: &str) -> Result<String, String>;
  fn fetch_gvs_po_token(&self, offer: &StreamOffer) -> Result<String, String>;
}

/// The two non-DRM delivery implementations.
pub trait StreamDelivery {
  type Output;

  fn start_sabr(&self, url: Url) -> Result<Self::Output, String>;
  fn start_direct(&self, url: Url) -> Result<Self::Output, String>;
}

/// Applies all authorization layers, then starts the offer's delivery method.
///
/// There is deliberately no fallback to an unsolved `n` value: it is an
/// invalid request, not a best-effort optimization. Call this once per offer;
/// a higher-level selector can try the next non-DRM offer after an error.
pub fn start_non_drm_stream<R, D>(
  player_url: &Url,
  offer: &StreamOffer,
  requirements: &R,
  delivery: &D,
) -> Result<D::Output, StreamFlowError>
where
  R: StreamRequirements,
  D: StreamDelivery,
{
  if offer.is_drm_protected {
    return Err(StreamFlowError::DrmProtected);
  }

  let mut url = offer.url.clone();
  if let Some(encrypted) = &offer.encrypted_signature {
    url = requirements.resolve_signature(url, encrypted).map_err(StreamFlowError::Signature)?;
  }
  if let Some(challenge) = &offer.n_challenge {
    let solved =
      requirements.solve_n(player_url, challenge).map_err(StreamFlowError::NChallenge)?;
    replace_query_parameter(&mut url, "n", &solved);
  }
  if offer.requires_gvs_po_token {
    let token = requirements.fetch_gvs_po_token(offer).map_err(StreamFlowError::PoToken)?;
    replace_query_parameter(&mut url, "pot", &token);
  }

  match offer.delivery {
    Delivery::Sabr => delivery.start_sabr(url),
    Delivery::Direct => delivery.start_direct(url),
  }
  .map_err(StreamFlowError::Delivery)
}

pub(crate) fn replace_query_parameter(url: &mut Url, name: &str, replacement: &str) {
  let pairs = url
    .query_pairs()
    .map(|(key, value)| (key.into_owned(), value.into_owned()))
    .filter(|(key, _)| key != name)
    .chain(std::iter::once((name.to_owned(), replacement.to_owned())))
    .collect::<Vec<_>>();
  url.set_query(None);
  for (key, value) in pairs {
    url.query_pairs_mut().append_pair(&key, &value);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::cell::RefCell;

  struct Requirements(RefCell<Vec<&'static str>>);
  impl StreamRequirements for Requirements {
    fn resolve_signature(&self, url: Url, _: &str) -> Result<Url, String> {
      self.0.borrow_mut().push("signature");
      Ok(url)
    }
    fn solve_n(&self, _: &Url, _: &str) -> Result<String, String> {
      self.0.borrow_mut().push("n");
      Ok("solved-n".into())
    }
    fn fetch_gvs_po_token(&self, _: &StreamOffer) -> Result<String, String> {
      self.0.borrow_mut().push("pot");
      Ok("token".into())
    }
  }
  struct DeliveryStub;
  impl StreamDelivery for DeliveryStub {
    type Output = Url;
    fn start_sabr(&self, url: Url) -> Result<Url, String> { Ok(url) }
    fn start_direct(&self, _: Url) -> Result<Url, String> {
      Err("unexpected direct delivery".into())
    }
  }

  #[test]
  fn sabr_offer_runs_all_required_authorization_steps() {
    let offer = StreamOffer {
      delivery:              Delivery::Sabr,
      url:                   Url::parse("https://example.test/stream?n=old").unwrap(),
      is_drm_protected:      false,
      encrypted_signature:   Some("encrypted".into()),
      n_challenge:           Some("old".into()),
      requires_gvs_po_token: true,
    };
    let requirements = Requirements(RefCell::default());
    let result = start_non_drm_stream(
      &Url::parse("https://example.test/player.js").unwrap(),
      &offer,
      &requirements,
      &DeliveryStub,
    )
    .unwrap();
    assert_eq!(*requirements.0.borrow(), ["signature", "n", "pot"]);
    assert_eq!(result.query_pairs().find(|(key, _)| key == "n").unwrap().1, "solved-n");
    assert_eq!(result.query_pairs().find(|(key, _)| key == "pot").unwrap().1, "token");
  }
}
