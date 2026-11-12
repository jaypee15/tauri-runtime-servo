//! Engine-level options shared by every webview of the process.
#![allow(dead_code)]

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct EngineOptions {
  /// Exit the process when a Servo thread panics (Servo's own default is `true`; a browser
  /// should show a failed page for that tab instead, so our default is `false`).
  pub hard_fail: bool,
}

/// Set-once holder (testable without touching the process-wide global).
pub struct OptionsCell {
  cell: std::sync::OnceLock<EngineOptions>,
}

impl OptionsCell {
  pub const fn new() -> Self {
    OptionsCell {
      cell: std::sync::OnceLock::new(),
    }
  }

  /// First call wins; returns whether this call set the value.
  pub fn set(&self, options: EngineOptions) -> bool {
    self.cell.set(options).is_ok()
  }

  pub fn get(&self) -> EngineOptions {
    self.cell.get().copied().unwrap_or_default()
  }
}

static GLOBAL: OptionsCell = OptionsCell::new();

/// Call before the first webview is created; returns whether the options were applied.
pub fn set_engine_options(options: EngineOptions) -> bool {
  GLOBAL.set(options)
}

pub fn engine_options() -> EngineOptions {
  GLOBAL.get()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn engine_options_default_is_not_hard_fail() {
    assert!(!OptionsCell::new().get().hard_fail);
    assert!(!EngineOptions::default().hard_fail);
  }

  #[test]
  fn set_engine_options_first_call_wins() {
    let cell = OptionsCell::new();
    assert!(cell.set(EngineOptions { hard_fail: true }));
    assert!(!cell.set(EngineOptions { hard_fail: false }));
    assert!(cell.get().hard_fail);
  }
}
