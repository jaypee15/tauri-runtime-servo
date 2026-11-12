//! App-level filter for resource loads (tracker/ad blocking).
//!
//! Tauri's `on_web_resource_request` is consumed by Tauri itself and only sees `tauri://` asset
//! requests, so it can never block network loads. The host registers a filter here instead; the
//! webview delegate asks it about every load (libservo's `WebViewDelegate::load_web_resource`).
#![allow(dead_code)]

use std::sync::{Arc, RwLock};

use http::{HeaderMap, Method, StatusCode};
use url::Url;

/// What to do with one resource load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceAction {
  /// Let the load proceed.
  Allow,
  /// Replace the load with an empty `204 No Content`.
  Block,
}

/// A replacement response for a load.
#[derive(Debug, PartialEq, Eq)]
pub struct Intercept {
  pub status: StatusCode,
  pub headers: HeaderMap,
  pub body: Vec<u8>,
}

pub fn intercept_for(action: ResourceAction) -> Option<Intercept> {
  match action {
    ResourceAction::Allow => None,
    ResourceAction::Block => Some(Intercept {
      status: StatusCode::NO_CONTENT,
      headers: HeaderMap::new(),
      body: Vec::new(),
    }),
  }
}

/// What the filter is told about a load.
pub struct ResourceRequestInfo<'a> {
  /// The Tauri label of the webview that issued the load.
  pub webview_label: &'a str,
  pub url: &'a Url,
  pub method: &'a Method,
  pub is_main_frame: bool,
}

pub type ResourceFilter = Arc<dyn Fn(&ResourceRequestInfo<'_>) -> ResourceAction + Send + Sync>;

/// Holder for the filter (testable without the process-wide global).
pub struct FilterSlot {
  filter: RwLock<Option<ResourceFilter>>,
}

impl FilterSlot {
  pub const fn new() -> Self {
    FilterSlot {
      filter: RwLock::new(None),
    }
  }
  pub fn set(&self, filter: ResourceFilter) {
    if let Ok(mut slot) = self.filter.write() {
      *slot = Some(filter);
    }
  }

  /// `Allow` when no filter is set.
  pub fn decide(&self, info: &ResourceRequestInfo<'_>) -> ResourceAction {
    // Clone the Arc so the filter runs without holding the lock.
    let filter = self.filter.read().ok().and_then(|slot| slot.clone());
    match filter {
      Some(filter) => filter(info),
      None => ResourceAction::Allow,
    }
  }
}

static FILTER: FilterSlot = FilterSlot::new();

/// Registers (or replaces) the app's resource filter. Call before pages load.
pub fn set_resource_filter(filter: ResourceFilter) {
  FILTER.set(filter);
}

pub(crate) fn filter_action(info: &ResourceRequestInfo<'_>) -> ResourceAction {
  FILTER.decide(info)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn info<'a>(label: &'a str, url: &'a Url, method: &'a Method) -> ResourceRequestInfo<'a> {
    ResourceRequestInfo {
      webview_label: label,
      url,
      method,
      is_main_frame: false,
    }
  }

  #[test]
  fn intercept_for_allow_is_none() {
    assert_eq!(intercept_for(ResourceAction::Allow), None);
  }

  #[test]
  fn intercept_for_block_is_204_with_empty_body() {
    let i = intercept_for(ResourceAction::Block).expect("intercept");
    assert_eq!(i.status, StatusCode::NO_CONTENT);
    assert!(i.body.is_empty());
  }

  #[test]
  fn slot_without_filter_allows() {
    let url = Url::parse("https://tracker.example/t.js").unwrap();
    let slot = FilterSlot::new();
    assert_eq!(
      slot.decide(&info("content-1", &url, &Method::GET)),
      ResourceAction::Allow
    );
  }

  #[test]
  fn slot_applies_the_filter_and_a_replacement() {
    let url = Url::parse("https://tracker.example/t.js").unwrap();
    let slot = FilterSlot::new();
    slot.set(Arc::new(|i| {
      if i.url.host_str() == Some("tracker.example") {
        ResourceAction::Block
      } else {
        ResourceAction::Allow
      }
    }));
    assert_eq!(
      slot.decide(&info("content-1", &url, &Method::GET)),
      ResourceAction::Block
    );
    slot.set(Arc::new(|_| ResourceAction::Allow));
    assert_eq!(
      slot.decide(&info("content-1", &url, &Method::GET)),
      ResourceAction::Allow
    );
  }

  #[test]
  fn filter_receives_the_webview_label_and_url() {
    let url = Url::parse("https://example.com/a.js").unwrap();
    let slot = FilterSlot::new();
    slot.set(Arc::new(|i| {
      assert_eq!(i.webview_label, "content-tab-7");
      assert_eq!(i.url.path(), "/a.js");
      ResourceAction::Allow
    }));
    slot.decide(&info("content-tab-7", &url, &Method::GET));
  }
}
