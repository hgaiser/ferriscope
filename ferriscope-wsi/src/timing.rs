//! What the compositor tells a swapchain: refresh cycle, past presentation
//! timings, retirement. The state is shared between the Wayland event handlers
//! (which write it) and the Vulkan entry points (which read it), without a
//! lock order between them.

use std::{
	collections::VecDeque,
	sync::{
		Mutex,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
};

/// Past timings kept per swapchain (`MaxPastPresentationTimes`).
pub const MAX_PAST_TIMINGS: usize = 16;

/// The refresh cycle before the compositor reports one: 60 Hz.
pub const DEFAULT_REFRESH_CYCLE_NS: u64 = 16_666_666;

/// One `past_present_timing` event (`VkPastPresentationTimingGOOGLE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PastTiming {
	pub present_id: u32,
	pub desired_present_time: u64,
	pub actual_present_time: u64,
	pub earliest_present_time: u64,
	pub present_margin: u64,
}

/// A bounded queue of timings, oldest first.
#[derive(Debug, Default)]
pub struct PastTimings {
	queue: VecDeque<PastTiming>,
}

impl PastTimings {
	/// Add a timing, dropping the oldest when the queue is full.
	pub fn push(&mut self, timing: PastTiming) {
		self.queue.push_back(timing);
		while self.queue.len() > MAX_PAST_TIMINGS {
			self.queue.pop_front();
		}
	}

	/// How many timings are waiting.
	pub fn len(&self) -> usize {
		self.queue.len()
	}

	/// Take up to `max` timings, oldest first; each is returned once.
	pub fn take(&mut self, max: usize) -> Vec<PastTiming> {
		let count = max.min(self.queue.len());
		self.queue.drain(..count).collect()
	}
}

/// How long the application takes to render a frame, from the end of the
/// previous present's wait to the present that ends it (less any time the
/// driver's acquire blocked), as a slowly decaying peak.
///
/// An application that needs most of a refresh period to render gains nothing
/// from waiting for a tick before it starts (its frames are never queued), and
/// loses throughput by it.
#[derive(Debug, Default)]
pub struct RenderPeak {
	peak_ns: u64,
}

impl RenderPeak {
	/// Fraction of the refresh period (in 1/16ths) above which the application
	/// is not held back: 15/16, about 94 %.
	const GATE_LIMIT_SIXTEENTHS: u64 = 15;

	/// Add a measured render time.
	pub fn sample(&mut self, ns: u64) {
		self.peak_ns = ns.max(self.peak_ns - self.peak_ns / 50);
	}

	/// Whether the application finishes early enough in the period that
	/// starting it on a tick pays: unknown render times count as such.
	pub fn should_gate(&self, refresh_ns: u64) -> bool {
		self.peak_ns <= refresh_ns / 16 * Self::GATE_LIMIT_SIXTEENTHS
	}
}

/// A swapchain's compositor-side state.
#[derive(Debug)]
pub struct SwapchainShared {
	/// The compositor retired the swapchain: the application must recreate it.
	pub retired: AtomicBool,
	/// The compositor's refresh period in nanoseconds.
	pub refresh_cycle_ns: AtomicU64,
	pub timings: Mutex<PastTimings>,
	/// When the current frame started (the previous present's wait ended),
	/// nanoseconds on `CLOCK_MONOTONIC`; 0 if none is outstanding.
	started_at_ns: AtomicU64,
	/// Time the driver's acquire blocked since the frame started: waiting for
	/// a free image is not the application's own frame time.
	blocked_ns: AtomicU64,
	pub render: Mutex<RenderPeak>,
	/// FIFO presents sent.
	presented: AtomicU64,
	/// `frame_ready` events seen for them (never more than `presented`).
	ready: AtomicU64,
}

impl SwapchainShared {
	pub fn new() -> Self {
		Self {
			retired: AtomicBool::new(false),
			refresh_cycle_ns: AtomicU64::new(DEFAULT_REFRESH_CYCLE_NS),
			timings: Mutex::new(PastTimings::default()),
			started_at_ns: AtomicU64::new(0),
			blocked_ns: AtomicU64::new(0),
			render: Mutex::new(RenderPeak::default()),
			presented: AtomicU64::new(0),
			ready: AtomicU64::new(0),
		}
	}

	/// The application starts a frame.
	pub fn note_frame_start(&self, now_ns: u64) {
		self.started_at_ns.store(now_ns, Ordering::Release);
		self.blocked_ns.store(0, Ordering::Release);
	}

	/// The driver's acquire blocked for `ns` within the current frame.
	pub fn note_blocked(&self, ns: u64) {
		self.blocked_ns.fetch_add(ns, Ordering::AcqRel);
	}

	/// A FIFO present was sent.
	pub fn note_present(&self, now_ns: u64) {
		self.presented.fetch_add(1, Ordering::AcqRel);
		let started = self.started_at_ns.swap(0, Ordering::AcqRel);
		let blocked = self.blocked_ns.swap(0, Ordering::AcqRel);
		if started != 0 && now_ns > started {
			self.render
				.lock()
				.unwrap_or_else(|poisoned| poisoned.into_inner())
				.sample((now_ns - started).saturating_sub(blocked));
		}
	}

	/// Whether the application should be held for `frame_ready` now.
	pub fn should_gate(&self) -> bool {
		self.render
			.lock()
			.unwrap_or_else(|poisoned| poisoned.into_inner())
			.should_gate(self.refresh_cycle_ns())
	}

	/// How many FIFO presents the compositor has not yet said it had room after.
	pub fn frames_ahead(&self) -> u64 {
		let presented = self.presented.load(Ordering::Acquire);
		presented.saturating_sub(self.ready.load(Ordering::Acquire))
	}

	/// Apply a `frame_ready` event: it answers one outstanding present, and a
	/// surplus (no present waiting) is dropped.
	pub fn on_frame_ready(&self) {
		let presented = self.presented.load(Ordering::Acquire);
		let _ = self.ready.try_update(Ordering::AcqRel, Ordering::Acquire, |ready| {
			(ready < presented).then_some(ready + 1)
		});
	}

	/// Give up waiting: count every present as answered.
	pub fn resync(&self) {
		self.ready
			.store(self.presented.load(Ordering::Acquire), Ordering::Release);
	}

	pub fn is_retired(&self) -> bool {
		self.retired.load(Ordering::Acquire)
	}

	pub fn refresh_cycle_ns(&self) -> u64 {
		self.refresh_cycle_ns.load(Ordering::Relaxed)
	}

	/// Apply a `refresh_cycle` event.
	pub fn on_refresh_cycle(&self, hi: u32, lo: u32) {
		self.refresh_cycle_ns.store(join(hi, lo), Ordering::Relaxed);
	}

	/// Apply a `retired` event.
	pub fn on_retired(&self) {
		self.retired.store(true, Ordering::Release);
	}

	/// Apply a `past_present_timing` event.
	pub fn on_past_present_timing(&self, timing: PastTiming) {
		self.timings
			.lock()
			.unwrap_or_else(|poisoned| poisoned.into_inner())
			.push(timing);
	}
}

/// A 64-bit protocol value from its two 32-bit halves.
pub fn join(hi: u32, lo: u32) -> u64 {
	(u64::from(hi) << 32) | u64::from(lo)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn timing(id: u32) -> PastTiming {
		PastTiming {
			present_id: id,
			desired_present_time: 0,
			actual_present_time: 0,
			earliest_present_time: 0,
			present_margin: 0,
		}
	}

	#[test]
	fn halves_join_into_nanoseconds() {
		assert_eq!(join(0, 16_666_666), 16_666_666);
		assert_eq!(join(1, 0), 1 << 32);
		assert_eq!(join(u32::MAX, u32::MAX), u64::MAX);
	}

	#[test]
	fn a_new_swapchain_assumes_sixty_hertz_and_is_not_retired() {
		let shared = SwapchainShared::new();
		assert_eq!(shared.refresh_cycle_ns(), 16_666_666);
		assert!(!shared.is_retired());
	}

	#[test]
	fn events_update_the_shared_state() {
		let shared = SwapchainShared::new();
		shared.on_refresh_cycle(0, 6_944_444);
		assert_eq!(shared.refresh_cycle_ns(), 6_944_444);
		shared.on_retired();
		assert!(shared.is_retired());
	}

	#[test]
	fn a_fast_renderer_is_held_for_ticks_and_a_slow_one_is_not() {
		let refresh = 16_666_666;
		let mut fast = RenderPeak::default();
		for _ in 0..10 {
			fast.sample(2_000_000);
		}
		assert!(fast.should_gate(refresh));
		let mut slow = RenderPeak::default();
		slow.sample(20_000_000);
		assert!(!slow.should_gate(refresh));
		// Unknown: hold, as a fast one would be.
		assert!(RenderPeak::default().should_gate(refresh));
	}

	#[test]
	fn the_render_peak_decays_when_the_application_speeds_up() {
		let refresh = 16_666_666;
		let mut render = RenderPeak::default();
		render.sample(20_000_000);
		assert!(!render.should_gate(refresh));
		for _ in 0..40 {
			render.sample(2_000_000);
		}
		assert!(render.should_gate(refresh));
	}

	#[test]
	fn a_frame_ready_answers_one_outstanding_present() {
		let shared = SwapchainShared::new();
		assert_eq!(shared.frames_ahead(), 0);
		shared.note_present(0);
		shared.note_present(0);
		assert_eq!(shared.frames_ahead(), 2);
		shared.on_frame_ready();
		assert_eq!(shared.frames_ahead(), 1);
	}

	#[test]
	fn a_surplus_frame_ready_does_not_build_credit() {
		let shared = SwapchainShared::new();
		shared.on_frame_ready();
		shared.on_frame_ready();
		shared.note_present(0);
		assert_eq!(shared.frames_ahead(), 1);
	}

	#[test]
	fn giving_up_resyncs_to_the_presents_sent() {
		let shared = SwapchainShared::new();
		shared.note_present(0);
		shared.note_present(0);
		shared.resync();
		assert_eq!(shared.frames_ahead(), 0);
	}

	#[test]
	fn the_timing_queue_keeps_the_newest_sixteen() {
		let shared = SwapchainShared::new();
		for id in 0..20 {
			shared.on_past_present_timing(timing(id));
		}
		let mut timings = shared.timings.lock().unwrap();
		let ids: Vec<u32> = timings.take(100).iter().map(|t| t.present_id).collect();
		assert_eq!(ids.len(), MAX_PAST_TIMINGS);
		assert_eq!(ids, (4..20).collect::<Vec<_>>());
	}

	#[test]
	fn each_timing_is_returned_once_oldest_first() {
		let mut timings = PastTimings::default();
		for id in 0..5 {
			timings.push(timing(id));
		}
		let first: Vec<u32> = timings.take(2).iter().map(|t| t.present_id).collect();
		assert_eq!(first, [0, 1]);
		let rest: Vec<u32> = timings.take(10).iter().map(|t| t.present_id).collect();
		assert_eq!(rest, [2, 3, 4]);
		assert!(timings.take(10).is_empty());
	}
}
