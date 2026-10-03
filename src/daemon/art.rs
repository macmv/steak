//! Album art, fetched on behalf of clients so that they never touch the
//! network themselves.

use std::{collections::VecDeque, sync::Mutex};

use st_api::Url;

use super::{api::BoxError, cell::lock};

/// Images are a few tens of kilobytes; this is for a runaway response.
const MAX_BYTES: usize = 8 * 1024 * 1024;
/// How many images to keep. A queue of thumbnails is re-requested as the
/// current track changes, and by every attached client.
const KEEP: usize = 64;

/// The hosts YouTube serves thumbnails from. The socket is private, but there
/// is no reason for the daemon to fetch arbitrary URLs for anyone.
fn allowed(url: &Url) -> bool {
  let Some(host) = url.host_str() else { return false };
  url.scheme() == "https"
    && ["ytimg.com", "ggpht.com", "googleusercontent.com"]
      .iter()
      .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

#[derive(Default)]
pub struct ArtCache {
  /// Oldest first.
  images: Mutex<VecDeque<(String, Vec<u8>)>>,
}

impl ArtCache {
  fn get(&self, url: &str) -> Option<Vec<u8>> {
    lock(&self.images).iter().find(|(cached, _)| cached == url).map(|(_, image)| image.clone())
  }

  fn put(&self, url: String, image: Vec<u8>) {
    let mut images = lock(&self.images);
    if images.len() >= KEEP {
      images.pop_front();
    }
    images.push_back((url, image));
  }

  pub async fn fetch(&self, http: &st_api::Client, url: &str) -> Result<Vec<u8>, BoxError> {
    if let Some(image) = self.get(url) {
      return Ok(image);
    }
    let parsed = Url::parse(url).map_err(|error| format!("not a URL: {error}"))?;
    if !allowed(&parsed) {
      return Err(format!("{url} is not a YouTube image").into());
    }
    let mut response = http.get(parsed).send().await?.error_for_status()?;
    let mut image = Vec::new();
    while let Some(chunk) = response.chunk().await? {
      image.extend_from_slice(&chunk);
      if image.len() > MAX_BYTES {
        return Err("the image is too large".into());
      }
    }
    self.put(url.to_owned(), image.clone());
    Ok(image)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn only_youtube_image_hosts_over_https_are_allowed() {
    let url = |s: &str| Url::parse(s).unwrap();
    assert!(allowed(&url("https://i.ytimg.com/vi/x/hq.jpg")));
    assert!(allowed(&url("https://lh3.googleusercontent.com/abc=w60")));
    assert!(allowed(&url("https://yt3.ggpht.com/abc")));
    assert!(!allowed(&url("http://i.ytimg.com/vi/x/hq.jpg")));
    assert!(!allowed(&url("https://evil.com/ytimg.com")));
    assert!(!allowed(&url("https://notytimg.com/x")));
    assert!(!allowed(&url("https://127.0.0.1/x")));
  }

  #[test]
  fn the_cache_evicts_the_oldest() {
    let cache = ArtCache::default();
    for n in 0..KEEP + 1 {
      cache.put(n.to_string(), vec![n as u8]);
    }
    assert!(cache.get("0").is_none());
    assert_eq!(cache.get("1"), Some(vec![1]));
  }
}
