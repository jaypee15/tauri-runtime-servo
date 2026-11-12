//! Per-webview routing table for custom-protocol handlers.
//!
//! The Servo engine is shared, so a protocol scheme is registered with it once; each request then
//! has to be routed to the handler of the webview that issued it (`target_webview_id`). A webview
//! creation registers *provisional* handlers first: its initial load can fire a request before its
//! Servo id exists, and exactly one webview is being created at a time.
#![allow(dead_code)]

use std::collections::HashMap;
use std::hash::Hash;

pub struct ProtocolRoutes<K, V> {
  by_webview: HashMap<K, HashMap<String, V>>,
  provisional: HashMap<String, V>,
}

impl<K: Hash + Eq + Clone, V: Clone> ProtocolRoutes<K, V> {
  pub fn new() -> Self {
    Self {
      by_webview: HashMap::new(),
      provisional: HashMap::new(),
    }
  }

  pub fn set_provisional(&mut self, handlers: HashMap<String, V>) {
    self.provisional = handlers;
  }

  /// Moves the provisional handlers to `webview` (which now has an id).
  pub fn promote(&mut self, webview: K) {
    let handlers = std::mem::take(&mut self.provisional);
    self.by_webview.insert(webview, handlers);
  }

  pub fn unregister(&mut self, webview: &K) {
    self.by_webview.remove(webview);
  }

  /// A known webview resolves only against its own handlers; an unknown or missing one falls back
  /// to the provisional handlers (a webview that is still being created).
  pub fn resolve(&self, webview: Option<&K>, scheme: &str) -> Option<V> {
    match webview.and_then(|k| self.by_webview.get(k)) {
      Some(handlers) => handlers.get(scheme).cloned(),
      None => self.provisional.get(scheme).cloned(),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn handlers(pairs: &[(&str, u32)]) -> HashMap<String, u32> {
    pairs.iter().map(|(s, v)| (s.to_string(), *v)).collect()
  }

  #[test]
  fn routes_resolve_by_webview_and_scheme() {
    let mut r: ProtocolRoutes<u64, u32> = ProtocolRoutes::new();
    r.set_provisional(handlers(&[("ipc", 1), ("tauri", 2)]));
    r.promote(10);
    r.set_provisional(handlers(&[("ipc", 3)]));
    r.promote(20);
    assert_eq!(r.resolve(Some(&10), "ipc"), Some(1));
    assert_eq!(r.resolve(Some(&10), "tauri"), Some(2));
    assert_eq!(r.resolve(Some(&20), "ipc"), Some(3));
    assert_eq!(r.resolve(Some(&20), "tauri"), None);
  }

  #[test]
  fn unregister_removes_all_schemes_of_webview() {
    let mut r: ProtocolRoutes<u64, u32> = ProtocolRoutes::new();
    r.set_provisional(handlers(&[("ipc", 1), ("tauri", 2)]));
    r.promote(10);
    r.unregister(&10);
    assert_eq!(r.resolve(Some(&10), "ipc"), None);
    assert_eq!(r.resolve(Some(&10), "tauri"), None);
  }

  #[test]
  fn provisional_serves_unknown_webview_until_promoted() {
    let mut r: ProtocolRoutes<u64, u32> = ProtocolRoutes::new();
    r.set_provisional(handlers(&[("ipc", 7)]));
    assert_eq!(r.resolve(Some(&99), "ipc"), Some(7));
    r.promote(99);
    assert_eq!(r.resolve(Some(&99), "ipc"), Some(7));
    // after promotion nothing provisional remains for other webviews
    assert_eq!(r.resolve(Some(&5), "ipc"), None);
  }

  #[test]
  fn missing_target_webview_uses_provisional_else_none() {
    let mut r: ProtocolRoutes<u64, u32> = ProtocolRoutes::new();
    assert_eq!(r.resolve(None, "ipc"), None);
    r.set_provisional(handlers(&[("ipc", 4)]));
    assert_eq!(r.resolve(None, "ipc"), Some(4));
  }

  #[test]
  fn known_webview_never_falls_back_to_provisional() {
    let mut r: ProtocolRoutes<u64, u32> = ProtocolRoutes::new();
    r.set_provisional(handlers(&[("ipc", 1)]));
    r.promote(10);
    r.set_provisional(handlers(&[("ipc", 2), ("tauri", 3)]));
    // webview 10 has no "tauri" route of its own; it must not see the other webview's one
    assert_eq!(r.resolve(Some(&10), "tauri"), None);
  }
}
