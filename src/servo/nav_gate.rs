//! Defers a navigation requested while a webview's initial load is still in flight.
#![allow(dead_code)]

use std::time::{Duration, Instant};

use url::Url;

#[derive(Debug, PartialEq, Eq)]
pub enum NavAction {
  LoadNow(Url),
  Deferred,
}

pub struct NavGate {
  created: Instant,
  timeout: Duration,
  initial_done: bool,
  pending: Option<Url>,
}

impl NavGate {
  pub fn new(created: Instant, timeout: Duration) -> Self {
    Self {
      created,
      timeout,
      initial_done: false,
      pending: None,
    }
  }

  fn timed_out(&self, now: Instant) -> bool {
    now.saturating_duration_since(self.created) >= self.timeout
  }

  /// Loads now when the initial load finished (or the timeout passed); otherwise keeps only
  /// the latest requested URL until `initial_load_complete` or `tick` releases it.
  pub fn request(&mut self, url: Url, now: Instant) -> NavAction {
    if self.initial_done || self.timed_out(now) {
      self.pending = None;
      NavAction::LoadNow(url)
    } else {
      self.pending = Some(url);
      NavAction::Deferred
    }
  }

  /// The initial load finished: returns the URL that was waiting, if any.
  pub fn initial_load_complete(&mut self) -> Option<Url> {
    self.initial_done = true;
    self.pending.take()
  }

  /// Releases a waiting URL once the timeout has passed without a completion signal.
  pub fn tick(&mut self, now: Instant) -> Option<Url> {
    if self.timed_out(now) {
      self.pending.take()
    } else {
      None
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
  }

  fn gate() -> (NavGate, Instant) {
    let t0 = Instant::now();
    (NavGate::new(t0, Duration::from_secs(2)), t0)
  }

  #[test]
  fn defers_navigation_before_initial_load() {
    let (mut g, t0) = gate();
    assert_eq!(
      g.request(url("https://a.example/"), t0 + Duration::from_millis(100)),
      NavAction::Deferred
    );
  }

  #[test]
  fn loads_immediately_after_initial_load() {
    let (mut g, t0) = gate();
    assert_eq!(g.initial_load_complete(), None);
    assert_eq!(
      g.request(url("https://a.example/"), t0 + Duration::from_millis(100)),
      NavAction::LoadNow(url("https://a.example/"))
    );
  }

  #[test]
  fn pending_url_released_on_initial_complete() {
    let (mut g, t0) = gate();
    g.request(url("https://a.example/"), t0);
    assert_eq!(g.initial_load_complete(), Some(url("https://a.example/")));
    assert_eq!(g.initial_load_complete(), None);
  }

  #[test]
  fn latest_request_wins_when_deferred_twice() {
    let (mut g, t0) = gate();
    g.request(url("https://a.example/"), t0);
    g.request(url("https://b.example/"), t0 + Duration::from_millis(50));
    assert_eq!(g.initial_load_complete(), Some(url("https://b.example/")));
  }

  #[test]
  fn timeout_releases_pending_without_complete() {
    let (mut g, t0) = gate();
    g.request(url("https://a.example/"), t0);
    assert_eq!(g.tick(t0 + Duration::from_secs(1)), None);
    assert_eq!(
      g.tick(t0 + Duration::from_secs(2)),
      Some(url("https://a.example/"))
    );
    assert_eq!(g.tick(t0 + Duration::from_secs(3)), None);
  }

  #[test]
  fn request_after_timeout_loads_now() {
    let (mut g, t0) = gate();
    assert_eq!(
      g.request(url("https://a.example/"), t0 + Duration::from_secs(5)),
      NavAction::LoadNow(url("https://a.example/"))
    );
  }
}
