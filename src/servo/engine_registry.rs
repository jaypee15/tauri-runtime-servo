//! Counts the live webviews that use the shared Servo engine.
//!
//! libservo's options can be initialized only once per process, so the engine cannot be dropped and
//! recreated while the process runs: it lives until the event loop ends and is released then, once
//! no webview remains (`is_idle`). The count also lets teardown tests assert that closing tabs and
//! windows leaves nothing behind.
#![allow(dead_code)]

#[derive(Default)]
pub struct EngineRegistry {
  live: usize,
}

impl EngineRegistry {
  pub fn new() -> Self {
    Self::default()
  }

  /// A webview started using the engine; returns the live count after.
  pub fn acquire(&mut self) -> usize {
    self.live += 1;
    self.live
  }

  /// A webview stopped using the engine; returns the live count after (never below zero).
  pub fn release(&mut self) -> usize {
    self.live = self.live.saturating_sub(1);
    self.live
  }

  pub fn is_idle(&self) -> bool {
    self.live == 0
  }

  pub fn live(&self) -> usize {
    self.live
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn engine_released_only_when_last_webview_closes() {
    let mut r = EngineRegistry::new();
    assert_eq!(r.acquire(), 1);
    assert_eq!(r.acquire(), 2);
    assert!(!r.is_idle());
    assert_eq!(r.release(), 1);
    assert!(!r.is_idle());
    assert_eq!(r.release(), 0);
    assert!(r.is_idle());
  }

  #[test]
  fn acquire_after_idle_starts_a_new_count() {
    let mut r = EngineRegistry::new();
    r.acquire();
    r.release();
    assert!(r.is_idle());
    assert_eq!(r.acquire(), 1);
    assert!(!r.is_idle());
    assert_eq!(r.live(), 1);
  }

  #[test]
  fn release_never_underflows() {
    let mut r = EngineRegistry::new();
    assert_eq!(r.release(), 0);
    assert!(r.is_idle());
    assert_eq!(r.acquire(), 1);
  }
}
