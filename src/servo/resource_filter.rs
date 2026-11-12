//! Decides whether a Tauri `on_web_resource_request` handler asked to replace a resource load.
#![allow(dead_code)]

use std::borrow::Cow;

use http::{HeaderMap, StatusCode};

/// The replacement response the handler asked for.
#[derive(Debug, PartialEq, Eq)]
pub struct Intercept {
  pub status: StatusCode,
  pub headers: HeaderMap,
  pub body: Vec<u8>,
}

/// The handler receives a default response (200, empty). If it left that untouched the load
/// proceeds; if it changed the status or filled the body, that response replaces the load.
pub fn decide(response: &http::Response<Cow<'static, [u8]>>) -> Option<Intercept> {
  if response.status() == StatusCode::OK && response.body().is_empty() {
    return None;
  }
  Some(Intercept {
    status: response.status(),
    headers: response.headers().clone(),
    body: response.body().to_vec(),
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn default_response() -> http::Response<Cow<'static, [u8]>> {
    http::Response::new(Cow::Borrowed(&[][..]))
  }

  #[test]
  fn untouched_default_response_lets_the_load_proceed() {
    assert_eq!(decide(&default_response()), None);
  }

  #[test]
  fn changed_status_intercepts_with_that_status() {
    let mut r = default_response();
    *r.status_mut() = StatusCode::NO_CONTENT;
    let i = decide(&r).expect("intercept");
    assert_eq!(i.status, StatusCode::NO_CONTENT);
    assert!(i.body.is_empty());
  }

  #[test]
  fn filled_body_intercepts_even_with_ok_status() {
    let mut r = default_response();
    *r.body_mut() = Cow::Owned(b"blocked".to_vec());
    let i = decide(&r).expect("intercept");
    assert_eq!(i.status, StatusCode::OK);
    assert_eq!(i.body, b"blocked".to_vec());
  }

  #[test]
  fn headers_only_change_does_not_intercept() {
    let mut r = default_response();
    r.headers_mut()
      .insert("x-seen", http::HeaderValue::from_static("1"));
    assert_eq!(decide(&r), None);
  }
}
