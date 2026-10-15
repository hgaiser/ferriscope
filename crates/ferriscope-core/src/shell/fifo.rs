//! `wp_fifo_v1`: release FIFO barriers on the frame-callback cadence.
//!
//! A FIFO client's commit that waits on a barrier is held until the content
//! that set the barrier was *presented*. ferriscope has no vblank, so the
//! session-rate frame-callback tick stands in for one (gamescope does not
//! implement fifo; its clients pace on frame callbacks, which are
//! independent of delivery). The barriers of every surface that gets a frame
//! callback are signalled on that tick, so a client behind another window or
//! facing a stalled consumer advances exactly as it does with callbacks, and
//! the `GAMESCOPE_FPS_LIMIT` throttle applies to both alike.
//! Smithay's managed mode places the blockers; this only signals them and wakes
//! the clients whose commits were waiting.
//!
//! The gamescope WSI layer always presents MAILBOX to the driver and implements
//! FIFO in the compositor: a surface whose swapchain is in FIFO mode
//! (`gamescope_swapchain.set_present_mode`) has at most one commit latched per
//! vblank, later ones wait in order (`handle_done_commits_xwayland`,
//! `steamcompmgr.cpp:7987`). The tick stands in for the vblank here too, with
//! one difference: gamescope latches the first commit at the next vblank,
//! ferriscope applies it at once, so only commits beyond one per tick wait.

use std::{
	collections::{HashSet, VecDeque},
	sync::{
		Arc, Mutex,
		atomic::{AtomicBool, Ordering},
	},
};

use smithay::{
	reexports::wayland_server::{Resource, backend::ClientId, protocol::wl_surface::WlSurface},
	wayland::{
		compositor::{
			Blocker, BlockerState, CompositorHandler, TraversalAction, add_blocker, with_states,
			with_surface_tree_downward,
		},
		fifo::FifoBarrierCachedState,
	},
};

use crate::{shell::FerriscopeState, swapchain};

/// `VK_PRESENT_MODE_FIFO_KHR`.
const VK_PRESENT_MODE_FIFO_KHR: u32 = 2;

/// A WSI-FIFO commit held until a tick releases it.
struct TickBlocker(Arc<AtomicBool>);

impl Blocker for TickBlocker {
	fn state(&self) -> BlockerState {
		if self.0.load(Ordering::Acquire) {
			BlockerState::Released
		} else {
			BlockerState::Pending
		}
	}
}

/// One surface's WSI-FIFO queue.
#[derive(Default)]
struct WsiFifo {
	/// A commit was let through since the last tick.
	latched_this_tick: bool,
	/// Held commits, oldest first.
	held: VecDeque<Arc<AtomicBool>>,
}

impl WsiFifo {
	/// A FIFO commit arrives: `None` lets it through now, `Some` is the flag
	/// that releases it at a later tick.
	fn admit(&mut self) -> Option<Arc<AtomicBool>> {
		if !self.latched_this_tick && self.held.is_empty() {
			self.latched_this_tick = true;
			return None;
		}
		let flag = Arc::new(AtomicBool::new(false));
		self.held.push_back(flag.clone());
		Some(flag)
	}

	/// A tick: release the oldest held commit, if any.
	///
	/// A tick that released one has used its frame, so it reports no room: a
	/// frame the application started now would wait a whole interval behind
	/// it. Saying `frame_ready` then left a FIFO game one frame ahead for good,
	/// each tick releasing the frame it rendered right after the previous tick,
	/// a constant 16.7 ms at 60 Hz. Without it the application starts at the
	/// next tick and its frame is taken on arrival.
	fn tick(&mut self) -> FifoTick {
		match self.held.pop_front() {
			Some(flag) => {
				flag.store(true, Ordering::Release);
				self.latched_this_tick = true;
				FifoTick {
					released: true,
					has_room: false,
				}
			},
			None => {
				self.latched_this_tick = false;
				FifoTick {
					released: false,
					has_room: true,
				}
			},
		}
	}
}

type WsiFifoCell = Mutex<WsiFifo>;

/// Hold `surface`'s pending commit for a later tick if its swapchain is in FIFO
/// mode and a commit was already let through this tick. Runs as a pre-commit
/// hook.
pub(in crate::shell) fn gate_wsi_fifo(surface: &WlSurface) {
	if swapchain::pending_present_mode(surface) != Some(VK_PRESENT_MODE_FIFO_KHR) {
		return;
	}
	let held = with_states(surface, |states| {
		states.data_map.insert_if_missing_threadsafe(WsiFifoCell::default);
		let mut fifo = states
			.data_map
			.get::<WsiFifoCell>()
			.expect("inserted just above")
			.lock()
			.unwrap();
		fifo.admit().map(TickBlocker)
	});
	if let Some(blocker) = held {
		add_blocker(surface, blocker);
	}
}

/// What a tick did to a surface's WSI-FIFO queue.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct FifoTick {
	/// A held commit was released.
	released: bool,
	/// The surface has a FIFO queue, nothing is held and this tick released
	/// nothing: a frame started now is taken on arrival (`frame_ready`).
	has_room: bool,
}

/// Advance `surface`'s WSI-FIFO queue by one tick: release the oldest held
/// commit, if any.
fn tick_wsi_fifo(surface: &WlSurface) -> FifoTick {
	with_states(surface, |states| {
		let Some(cell) = states.data_map.get::<WsiFifoCell>() else {
			return FifoTick::default();
		};
		cell.lock().unwrap().tick()
	})
}

/// Whether any surface in `root`'s tree holds an unsignalled FIFO barrier.
pub(in crate::shell) fn has_fifo_barrier(root: &WlSurface) -> bool {
	let mut pending = false;
	with_surface_tree_downward(
		root,
		(),
		|_, _, ()| TraversalAction::DoChildren(()),
		|_, states, ()| {
			pending |= states
				.cached_state
				.get::<FifoBarrierCachedState>()
				.current()
				.barrier
				.is_some();
		},
		|_, _, ()| true,
	);
	pending
}

impl FerriscopeState {
	/// Signal the FIFO barriers of the committed content of `surfaces` (the
	/// windows that got frame callbacks this tick) and resume the commits that
	/// waited.
	pub(in crate::shell) fn release_fifo_barriers(&mut self, surfaces: &[WlSurface]) {
		let mut released = HashSet::<ClientId>::new();
		for root in surfaces {
			// The WSI layer's swapchain surface is the root for an Xwayland window
			// (the content override) but a subsurface of a native Wayland
			// toplevel, so every surface of the tree has its queue ticked.
			let mut tree = Vec::<WlSurface>::new();
			with_surface_tree_downward(
				root,
				(),
				|_, _, ()| TraversalAction::DoChildren(()),
				|surface, states, ()| {
					tree.push(surface.clone());
					let barrier = states
						.cached_state
						.get::<FifoBarrierCachedState>()
						.current()
						.barrier
						.take();
					if let Some(barrier) = barrier {
						barrier.signal();
						if let Some(client) = surface.client() {
							released.insert(client.id());
						}
					}
				},
				|_, _, ()| true,
			);
			for surface in &tree {
				let tick = tick_wsi_fifo(surface);
				if tick.released
					&& let Some(client) = surface.client()
				{
					released.insert(client.id());
				}
				if tick.has_room {
					swapchain::send_frame_ready(surface);
				}
			}
		}
		let dh = self.display_handle.clone();
		for root in surfaces {
			let Some(client) = root.client() else { continue };
			if released.remove(&client.id()) {
				self.client_compositor_state(&client).blocker_cleared(self, &dh);
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_frame_presented_after_frame_ready_is_taken_on_arrival() {
		let mut fifo = WsiFifo::default();
		assert!(fifo.tick().has_room, "an idle queue has room");
		assert!(
			fifo.admit().is_none(),
			"the first frame of an interval is taken at once"
		);
		// A second frame in the same interval waits for the next tick.
		let held = fifo.admit().expect("a second frame waits");
		assert!(!held.load(Ordering::Acquire));

		// That tick releases it and so has no room left: a frame started now
		// would wait a whole interval behind it.
		let tick = fifo.tick();
		assert!(tick.released && !tick.has_room);
		assert!(held.load(Ordering::Acquire));

		// The next tick is idle again and offers room; a frame presented after
		// it is taken at once instead of queueing.
		assert!(fifo.tick().has_room);
		assert!(fifo.admit().is_none());
	}

	#[test]
	fn a_held_queue_drains_one_frame_per_tick() {
		let mut fifo = WsiFifo::default();
		assert!(fifo.admit().is_none());
		let first = fifo.admit().expect("held");
		let second = fifo.admit().expect("held");
		assert!(fifo.tick().released);
		assert!(first.load(Ordering::Acquire) && !second.load(Ordering::Acquire));
		assert!(fifo.tick().released);
		assert!(second.load(Ordering::Acquire));
		assert!(fifo.tick().has_room);
	}
}
