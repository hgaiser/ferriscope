//! The DMA-BUF buffer pool: slot allocation, delivery generations and release fences.

use std::{
	os::fd::OwnedFd,
	time::{Duration, Instant},
};

#[cfg(feature = "test-support")]
use smithay::backend::{
	allocator::Fourcc,
	renderer::{Offscreen, gles::GlesRenderbuffer},
};
use smithay::{
	backend::{
		allocator::{
			Allocator,
			dmabuf::{AsDmabuf, Dmabuf},
		},
		renderer::{Renderer, gles::GlesRenderer, sync::SyncPoint},
	},
	reexports::calloop::{Interest, Mode, PostAction, RegistrationToken, generic::Generic},
};

use crate::{
	error::{Error, Result},
	frame::Release,
};

use crate::shell::FerriscopeState;

/// The DMA-BUF buffer pool: lazily-allocated slots plus the per-slot delivery
/// generation and release-fence state.
pub(in crate::shell) struct DmabufPool {
	/// Lazily-allocated DMA-BUF pool slots, indexed by the slot part of a
	/// buffer handle.
	pub(in crate::shell) slots: Vec<Option<Dmabuf>>,
	/// Delivery generation currently held by the consumer for each slot.
	///
	/// `Some(generation)` means the slot is busy with that delivery; a
	/// [`Release`] is applied only if its handle carries the same generation, so
	/// a late release cannot free a slot that has been reused. `None` is free.
	pub(in crate::shell) delivered_generation: Vec<Option<u64>>,
	/// The event-loop source watching each slot's release fence.
	///
	/// When a [`Release`] carries a fence, the slot stays busy and the fence fd
	/// is registered as a one-shot calloop source. It frees the slot the moment
	/// the fence signals; the compositor never blocks on it.
	pub(in crate::shell) fence_source: Vec<Option<RegistrationToken>>,
	/// When each pending release fence was received, for the stale warning.
	pub(in crate::shell) fence_since: Vec<Option<Instant>>,
	/// Whether the long-pending warning already fired for a slot; one per slot.
	pub(in crate::shell) fence_warned: Vec<bool>,
	/// Monotonic source of delivery generations; see [`encode_handle`].
	pub(in crate::shell) next_generation: u64,
}

impl DmabufPool {
	pub(in crate::shell) fn new(size: usize) -> Self {
		Self {
			slots: (0..size).map(|_| None).collect(),
			delivered_generation: vec![None; size],
			fence_source: vec![None; size],
			fence_since: vec![None; size],
			fence_warned: vec![false; size],
			next_generation: 1,
		}
	}
}

impl FerriscopeState {
	/// Apply a returned pool slot.
	///
	/// Releases are matched on `(slot, generation)`: a stale handle (an earlier
	/// delivery of a reused slot) is ignored rather than freeing the wrong
	/// buffer. A release with no fence frees the slot immediately; a release
	/// with a fence keeps the slot busy and registers the fd as a one-shot
	/// event-loop source that frees it when it signals.
	///
	/// Either way the loop is already awake (the release arrived on a calloop
	/// channel, or the fence source fired), so [`Self::service`] runs right
	/// after this dispatch and a pending frame is rendered into the freed slot
	/// without waiting for any timer.
	pub(in crate::shell) fn apply_release(&mut self, release: Release) {
		let slot = match classify_release(release.buffer_id, &self.pool.delivered_generation) {
			ReleaseCheck::Apply(slot) => slot,
			ReleaseCheck::Stale => {
				tracing::trace!(buffer_id = release.buffer_id, "ignoring stale release");
				return;
			},
			ReleaseCheck::Invalid => {
				tracing::warn!(buffer_id = release.buffer_id, "release names an out-of-range buffer id");
				return;
			},
		};

		// A slot already gated on a fence is released only by that fence
		// signalling. In particular the abandonment guard's fence-less release
		// arrives after an explicit fence-carrying release and must not free a
		// slot the consumer may still be reading.
		if self.pool.fence_source[slot].is_some() {
			tracing::trace!(
				buffer_id = release.buffer_id,
				"release for a slot with a pending fence; ignored"
			);
			return;
		}

		match release.fence {
			None => self.free_slot(slot),
			Some(fence) => self.watch_release_fence(slot, fence, release.buffer_id),
		}
	}

	/// Keep `slot` busy until `fence` signals, watching it from the event loop.
	///
	/// A fence that cannot be registered (not pollable) leaves the slot busy
	/// rather than reusing it unordered; the other slots keep the pipeline
	/// moving.
	fn watch_release_fence(&mut self, slot: usize, fence: OwnedFd, buffer_id: u64) {
		tracing::trace!(buffer_id, "release fence pending; slot stays busy");
		let source = Generic::new(fence, Interest::READ, Mode::Level);
		match self.loop_handle.insert_source(source, move |_, _, state: &mut Self| {
			state.pool.fence_source[slot] = None;
			state.free_slot(slot);
			Ok(PostAction::Remove)
		}) {
			Ok(token) => {
				self.pool.fence_source[slot] = Some(token);
				self.pool.fence_since[slot] = Some(Instant::now());
				self.pool.fence_warned[slot] = false;
			},
			Err(e) => tracing::error!(slot, error = ?e, "cannot watch release fence; slot stays busy"),
		}
	}

	/// Mark a pool slot free and clear any release-fence bookkeeping.
	pub(in crate::shell) fn free_slot(&mut self, slot: usize) {
		self.pool.delivered_generation[slot] = None;
		self.pool.fence_since[slot] = None;
		self.pool.fence_warned[slot] = false;
	}

	/// Whether the next frame has a pool slot to render into.
	///
	/// Slot availability is the only bound on frames held by the consumer. The
	/// pixel paths copy synchronously and never hold a slot.
	pub(in crate::shell) fn slot_free(&self) -> bool {
		!self.use_dmabuf || pick_free_slot(&self.pool.delivered_generation).is_some()
	}

	/// Warn once per slot for each release fence pending longer than
	/// [`RELEASE_FENCE_WARN_AFTER`].
	///
	/// Diagnostic, not a timeout: the fence still gates the slot, which stays
	/// busy until it signals. Run from the frame-callback tick.
	pub(in crate::shell) fn warn_stale_fences(&mut self) {
		for slot in 0..self.pool.fence_since.len() {
			if self.pool.fence_warned[slot] {
				continue;
			}
			let Some(since) = self.pool.fence_since[slot] else {
				continue;
			};
			let pending = since.elapsed();
			if pending >= RELEASE_FENCE_WARN_AFTER {
				self.pool.fence_warned[slot] = true;
				tracing::warn!(slot, pending = ?pending, "release fence has not fired; slot stays busy");
			}
		}
	}

	/// Drop every pool buffer and the damage ages of all render targets, for a
	/// change of buffer format.
	///
	/// A frame the consumer holds owns a clone of its buffer, so it stays
	/// readable until released; the slot keeps its delivery generation, so the
	/// release still matches, and its next use allocates in the new format.
	pub(in crate::shell) fn reset_pool_buffers(&mut self) {
		for slot in 0..self.pool.slots.len() {
			self.pool.slots[slot] = None;
			self.damage.ages.invalidate(slot);
		}
	}

	/// Allocate a DMA-BUF pool slot on first use.
	pub(in crate::shell) fn ensure_slot(&mut self, id: usize) -> Result<()> {
		if self.pool.slots[id].is_none() {
			let (code, modifier) = self.render_format;
			let buffer = self
				.allocator
				.create_buffer(self.frame_size.w as u32, self.frame_size.h as u32, code, &[modifier])
				.map_err(|e| Error::Gpu(format!("allocate dmabuf: {e}")))?;
			let dmabuf = buffer
				.export()
				.map_err(|e| Error::Gpu(format!("export dmabuf: {e:?}")))?;
			self.pool.slots[id] = Some(dmabuf);
		}
		Ok(())
	}

	/// Allocate the CPU-readable offscreen target on first use.
	#[cfg(feature = "test-support")]
	pub(in crate::shell) fn ensure_renderbuffer(&mut self) -> Result<()> {
		if self.frame_target.is_none() {
			let target: GlesRenderbuffer =
				Offscreen::create_buffer(&mut self.renderer, Fourcc::Argb8888, self.frame_size)
					.map_err(|e| Error::Gpu(format!("allocate offscreen target: {e:?}")))?;
			self.frame_target = Some(target);
		}
		Ok(())
	}
}

/// Number of low bits of a buffer handle reserved for the pool slot.
pub(in crate::shell) const SLOT_BITS: u32 = 32;

/// How long a release fence may stay pending before the compositor warns.
///
/// Diagnostic only: the slot is never freed on a timeout, since the fence is
/// the only guarantee that the consumer has stopped reading the buffer.
pub(in crate::shell) const RELEASE_FENCE_WARN_AFTER: Duration = Duration::from_secs(2);

/// Encode a pool slot and its delivery generation into a buffer handle.
///
/// The low [`SLOT_BITS`] bits are the slot; the rest are the generation. A
/// handle is opaque to the consumer and is echoed back in [`Release`].
pub(in crate::shell) fn encode_handle(slot: usize, generation: u64) -> u64 {
	(generation << SLOT_BITS) | (slot as u64 & 0xffff_ffff)
}

/// Split a buffer handle back into `(slot, generation)`.
pub(in crate::shell) fn decode_handle(handle: u64) -> (usize, u64) {
	((handle & 0xffff_ffff) as usize, handle >> SLOT_BITS)
}

/// Verdict for a release handle against the current per-slot generations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::shell) enum ReleaseCheck {
	/// The handle matches the delivery still holding the slot.
	Apply(usize),
	/// The handle names a slot whose current generation differs; ignore it.
	Stale,
	/// The handle's slot is outside the pool.
	Invalid,
}

/// Decide what to do with a release handle.
///
/// A release is applied only when its slot is currently held by the same
/// generation, so a late release for an earlier delivery cannot free a slot
/// that has since been reused. A `None` slot (never delivered, or already
/// released) is stale rather than an error.
pub(in crate::shell) fn classify_release(handle: u64, delivered: &[Option<u64>]) -> ReleaseCheck {
	let (slot, generation) = decode_handle(handle);
	if slot >= delivered.len() {
		return ReleaseCheck::Invalid;
	}
	if delivered[slot] == Some(generation) {
		ReleaseCheck::Apply(slot)
	} else {
		ReleaseCheck::Stale
	}
}

/// Export `sync` as an acquire fence for the consumer.
///
/// `SyncPoint::export` returns `None` both when the work is already complete
/// and when the renderer's EGL fence is not exportable (no native fence). In
/// the latter case the GPU work may still be in flight, so wait for it before
/// reporting `None`. This keeps `acquire_fence == None` meaning exactly "the
/// buffer is already complete".
pub(in crate::shell) fn export_acquire_fence(renderer: &mut GlesRenderer, sync: &SyncPoint) -> Result<Option<OwnedFd>> {
	if let Some(fence) = sync.export() {
		return Ok(Some(fence));
	}
	if !sync.is_reached() {
		renderer
			.wait(sync)
			.map_err(|e| Error::Render(format!("acquire fence wait: {e:?}")))?;
	}
	Ok(None)
}

/// Index of the first free pool slot, or `None` when every slot is busy.
pub(in crate::shell) fn pick_free_slot(delivered: &[Option<u64>]) -> Option<usize> {
	delivered.iter().position(|generation| generation.is_none())
}

#[cfg(test)]
mod tests {

	use super::*;

	#[test]
	fn picks_first_free_slot() {
		assert_eq!(pick_free_slot(&[Some(1), None, Some(2)]), Some(1));
		assert_eq!(pick_free_slot(&[None, None]), Some(0));
		assert_eq!(pick_free_slot(&[]), None);
	}

	#[test]
	fn skips_when_all_slots_are_busy() {
		assert_eq!(pick_free_slot(&[Some(1), Some(2), Some(3)]), None);
	}

	#[test]
	fn handle_encodes_slot_and_generation() {
		assert_eq!(decode_handle(encode_handle(0, 1)), (0, 1));
		assert_eq!(decode_handle(encode_handle(3, 7)), (3, 7));
		// The slot is the low 32 bits and must not bleed into the generation.
		assert_eq!(decode_handle(encode_handle(2, 9)), (2, 9));
	}

	#[test]
	fn current_generation_release_is_applied() {
		let delivered = [Some(5), None, Some(8)];
		assert_eq!(
			classify_release(encode_handle(0, 5), &delivered),
			ReleaseCheck::Apply(0)
		);
		assert_eq!(
			classify_release(encode_handle(2, 8), &delivered),
			ReleaseCheck::Apply(2)
		);
	}

	#[test]
	fn stale_release_is_ignored() {
		// Slot 0 is currently held by generation 5; a release for generation 4
		// (an earlier delivery) must not free it.
		let delivered = [Some(5), None, Some(8)];
		assert_eq!(classify_release(encode_handle(0, 4), &delivered), ReleaseCheck::Stale);
		// A free slot is stale, not an error.
		assert_eq!(classify_release(encode_handle(1, 5), &delivered), ReleaseCheck::Stale);
	}

	#[test]
	fn out_of_range_release_is_invalid() {
		let delivered = [Some(1)];
		assert_eq!(classify_release(encode_handle(4, 1), &delivered), ReleaseCheck::Invalid);
	}
}
