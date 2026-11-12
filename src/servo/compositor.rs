//! Ordered registry of the child webviews of one native window.
//!
//! Pure bookkeeping (no GL): which children exist, the order they are drawn in (back to
//! front), which are visible, and which child a pointer position belongs to, so drawing and
//! input hit-testing always agree.
#![allow(dead_code)]

/// Identifies one child webview of a window.
pub type ChildId = u64;

/// A child's rectangle inside its window, in physical pixels (origin top-left).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
  pub x: i32,
  pub y: i32,
  pub width: u32,
  pub height: u32,
}

impl Bounds {
  /// Edges are half-open: `x + width` and `y + height` are outside.
  fn contains(&self, x: i32, y: i32) -> bool {
    let (x, y) = (i64::from(x), i64::from(y));
    let (left, top) = (i64::from(self.x), i64::from(self.y));
    x >= left && y >= top && x < left + i64::from(self.width) && y < top + i64::from(self.height)
  }
}

struct Slot {
  id: ChildId,
  bounds: Bounds,
  visible: bool,
}

/// Children ordered back to front.
#[derive(Default)]
pub struct Compositor {
  slots: Vec<Slot>,
}

impl Compositor {
  pub fn new() -> Self {
    Self::default()
  }

  /// Adds a visible child on top. Re-adding an existing id replaces it, on top.
  pub fn add(&mut self, id: ChildId, bounds: Bounds) {
    self.remove(id);
    self.slots.push(Slot {
      id,
      bounds,
      visible: true,
    });
  }

  /// Removes a child; returns whether it existed.
  pub fn remove(&mut self, id: ChildId) -> bool {
    let before = self.slots.len();
    self.slots.retain(|slot| slot.id != id);
    self.slots.len() != before
  }

  pub fn set_visible(&mut self, id: ChildId, visible: bool) {
    if let Some(slot) = self.slots.iter_mut().find(|slot| slot.id == id) {
      slot.visible = visible;
    }
  }

  pub fn set_bounds(&mut self, id: ChildId, bounds: Bounds) {
    if let Some(slot) = self.slots.iter_mut().find(|slot| slot.id == id) {
      slot.bounds = bounds;
    }
  }

  /// Moves a child to the front of the draw order.
  pub fn bring_to_front(&mut self, id: ChildId) {
    if let Some(index) = self.slots.iter().position(|slot| slot.id == id) {
      let slot = self.slots.remove(index);
      self.slots.push(slot);
    }
  }

  /// Visible children, back to front.
  pub fn draw_order(&self) -> Vec<ChildId> {
    self
      .slots
      .iter()
      .filter(|slot| slot.visible)
      .map(|slot| slot.id)
      .collect()
  }

  /// The topmost visible child containing the point.
  pub fn hit_test(&self, x: i32, y: i32) -> Option<ChildId> {
    self
      .slots
      .iter()
      .rev()
      .find(|slot| slot.visible && slot.bounds.contains(x, y))
      .map(|slot| slot.id)
  }

  pub fn len(&self) -> usize {
    self.slots.len()
  }

  pub fn is_empty(&self) -> bool {
    self.slots.is_empty()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn b(x: i32, y: i32, width: u32, height: u32) -> Bounds {
    Bounds {
      x,
      y,
      width,
      height,
    }
  }

  #[test]
  fn new_children_draw_on_top_in_creation_order() {
    let mut c = Compositor::new();
    c.add(1, b(0, 0, 100, 100));
    c.add(2, b(0, 0, 100, 100));
    c.add(3, b(0, 0, 100, 100));
    assert_eq!(c.draw_order(), vec![1, 2, 3]);
  }

  #[test]
  fn hidden_children_are_not_drawn_or_hit() {
    let mut c = Compositor::new();
    c.add(1, b(0, 0, 100, 100));
    c.add(2, b(0, 0, 100, 100));
    c.set_visible(2, false);
    assert_eq!(c.draw_order(), vec![1]);
    assert_eq!(c.hit_test(10, 10), Some(1));
    c.set_visible(2, true);
    assert_eq!(c.draw_order(), vec![1, 2]);
    assert_eq!(c.hit_test(10, 10), Some(2));
  }

  #[test]
  fn hit_test_returns_topmost_overlapping_visible_child() {
    let mut c = Compositor::new();
    c.add(1, b(0, 0, 200, 200)); // toolbar-like, full area
    c.add(2, b(0, 100, 200, 100)); // content below
    assert_eq!(c.hit_test(50, 150), Some(2));
    assert_eq!(c.hit_test(50, 50), Some(1));
    assert_eq!(c.hit_test(500, 500), None);
  }

  #[test]
  fn bring_to_front_changes_draw_and_hit_order() {
    let mut c = Compositor::new();
    c.add(1, b(0, 0, 200, 200));
    c.add(2, b(0, 100, 200, 100));
    c.bring_to_front(1);
    assert_eq!(c.draw_order(), vec![2, 1]);
    assert_eq!(c.hit_test(50, 150), Some(1));
  }

  #[test]
  fn remove_drops_child_everywhere() {
    let mut c = Compositor::new();
    c.add(1, b(0, 0, 100, 100));
    c.add(2, b(0, 0, 100, 100));
    assert_eq!(c.len(), 2);
    assert!(c.remove(2));
    assert!(!c.remove(2));
    assert_eq!(c.len(), 1);
    assert_eq!(c.draw_order(), vec![1]);
    assert_eq!(c.hit_test(10, 10), Some(1));
  }

  #[test]
  fn hit_test_edges_are_half_open() {
    let mut c = Compositor::new();
    c.add(1, b(10, 20, 100, 50));
    assert_eq!(c.hit_test(10, 20), Some(1));
    assert_eq!(c.hit_test(109, 69), Some(1));
    assert_eq!(c.hit_test(110, 40), None);
    assert_eq!(c.hit_test(50, 70), None);
    assert_eq!(c.hit_test(9, 40), None);
  }
}
