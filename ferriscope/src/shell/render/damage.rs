//! Damage tracking across the buffer pool: one tracker plus per-slot buffer age.

use smithay::{
	backend::renderer::damage::OutputDamageTracker,
	utils::{Physical, Size, Transform},
};

/// How many frames ago each target was last rendered.
///
/// The consumer receives whole buffers, so a slot's contents are only correct
/// if the damage rendered into it covers everything that changed since the slot
/// was last drawn. The age of a slot is the number of frames the tracker has
/// seen since then, which is exactly Smithay's `buffer_age`: 1 means the
/// previous frame, 0 means unknown contents and forces a full redraw.
pub(in crate::shell) struct BufferAges {
	/// Frames rendered so far; the tracker records one history entry per frame.
	frame: u64,
	/// The frame counter value at which each slot was last rendered, `None` when
	/// its contents are unknown (never rendered, newly allocated, or the last
	/// render failed).
	rendered_at: Vec<Option<u64>>,
}

impl BufferAges {
	pub(in crate::shell) fn new(slots: usize) -> Self {
		Self {
			frame: 0,
			rendered_at: vec![None; slots],
		}
	}

	/// The buffer age to render `slot` with, 0 when its contents are unknown.
	pub(in crate::shell) fn age(&self, slot: usize) -> usize {
		match self.rendered_at.get(slot).copied().flatten() {
			// The frame being rendered is `self.frame + 1`.
			Some(at) => (self.frame + 1 - at) as usize,
			None => 0,
		}
	}

	/// Record that a frame was rendered into `slot` (a successful render).
	pub(in crate::shell) fn rendered(&mut self, slot: usize) {
		self.frame += 1;
		if let Some(entry) = self.rendered_at.get_mut(slot) {
			*entry = Some(self.frame);
		}
	}

	/// Record a failed render: the tracker still advanced, but `slot` holds
	/// unknown contents.
	pub(in crate::shell) fn failed(&mut self, slot: usize) {
		self.frame += 1;
		self.invalidate(slot);
	}

	/// Forget `slot`'s contents (it was allocated or reallocated), so the next
	/// render of it is a full redraw.
	pub(in crate::shell) fn invalidate(&mut self, slot: usize) {
		if let Some(entry) = self.rendered_at.get_mut(slot) {
			*entry = None;
		}
	}
}

/// The single damage tracker and the per-slot ages that drive it.
pub(in crate::shell) struct FrameDamage {
	pub(in crate::shell) tracker: OutputDamageTracker,
	pub(in crate::shell) ages: BufferAges,
}

impl FrameDamage {
	/// A tracker for an output of `size` with `slots` render targets. The
	/// CPU-readback path uses a single target (slot 0).
	pub(in crate::shell) fn new(size: Size<i32, Physical>, slots: usize) -> Self {
		Self {
			tracker: OutputDamageTracker::new(size, 1.0, Transform::Normal),
			ages: BufferAges::new(slots.max(1)),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn unrendered_slot_is_unknown() {
		let ages = BufferAges::new(3);
		assert_eq!(ages.age(0), 0);
		assert_eq!(ages.age(2), 0);
	}

	#[test]
	fn single_slot_has_age_one() {
		let mut ages = BufferAges::new(1);
		ages.rendered(0);
		assert_eq!(ages.age(0), 1);
		ages.rendered(0);
		assert_eq!(ages.age(0), 1);
	}

	#[test]
	fn rotating_slots_age_by_pool_size() {
		let mut ages = BufferAges::new(3);
		for slot in 0..3 {
			assert_eq!(ages.age(slot), 0);
			ages.rendered(slot);
		}
		// Slot 0 was rendered three frames ago, slot 2 one frame ago.
		assert_eq!(ages.age(0), 3);
		assert_eq!(ages.age(1), 2);
		assert_eq!(ages.age(2), 1);
		ages.rendered(0);
		assert_eq!(ages.age(0), 1);
		assert_eq!(ages.age(1), 3);
	}

	#[test]
	fn skipped_slots_age_further() {
		let mut ages = BufferAges::new(2);
		ages.rendered(0);
		ages.rendered(1);
		ages.rendered(1);
		ages.rendered(1);
		assert_eq!(ages.age(0), 4);
	}

	#[test]
	fn invalidated_and_failed_slots_redraw_fully() {
		let mut ages = BufferAges::new(2);
		ages.rendered(0);
		ages.rendered(1);
		ages.invalidate(0);
		assert_eq!(ages.age(0), 0);
		assert_eq!(ages.age(1), 1);
		ages.failed(1);
		assert_eq!(ages.age(1), 0);
		ages.rendered(1);
		assert_eq!(ages.age(1), 1);
		// Slot 0 stays unknown until it is rendered again.
		assert_eq!(ages.age(0), 0);
	}

	#[test]
	fn out_of_range_slot_is_unknown() {
		let mut ages = BufferAges::new(1);
		ages.rendered(5);
		assert_eq!(ages.age(5), 0);
	}
}
