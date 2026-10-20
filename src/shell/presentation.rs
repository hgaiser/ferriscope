//! `wp_presentation`: tell clients when their commits reached the consumer.
//!
//! gamescope advertises the global with `CLOCK_MONOTONIC` (`wlserver.cpp:1579`)
//! and answers feedback from its vblank handler (`steamcompmgr.cpp:10335-10375`).
//! ferriscope has no vblank: a commit is *presented* at the instant the frame
//! containing it is handed to the sink, which is the real hand-off to the
//! encoder.
//!
//! Which commits count:
//!
//! - Feedback requested for a commit is held in the surface's committed state.
//!   When a later commit replaces that state before any frame is delivered,
//!   Smithay sends `discarded` for the superseded one (protocol: "the content
//!   update was superseded").
//! - Every delivery takes the committed feedback of all tracked windows, drawn
//!   or not. gamescope does the same ("we return `presented` for any window not
//!   visible ... we need this for forward progress",
//!   `steamcompmgr.cpp:10361-10364`): a client blocked in `vkWaitForPresentKHR`
//!   on a window that is not shown would otherwise never wake.
//! - A window that is not drawn is also answered on every session-rate tick,
//!   as gamescope answers from every vblank: a static scene is delivered only
//!   once a second (the keepalive), and Xwayland presents a GL window's next
//!   frame only after the previous one was presented. Without the tick, a
//!   window behind the focus ran at one frame a second, and a GL client that
//!   waits for a buffer release, as NVIDIA's does, stalled behind it.
//!
//! The reported refresh is the session interval and the sequence is the
//! delivered frame's sequence number. There is no vblank, so `VSYNC` is never
//! set. `ZERO_COPY` is set for a frame that is the client's own buffer
//! (passthrough, `render/passthrough.rs`).

use smithay::{
	desktop::utils::{OutputPresentationFeedback, take_presentation_feedback_surface_tree},
	reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind,
	utils::{Clock, ClockSource, Monotonic},
	wayland::presentation::Refresh,
};

use crate::shell::FerriscopeState;

/// The clock id `wp_presentation.clock_id` announces: `CLOCK_MONOTONIC`, as
/// gamescope (`wlserver.cpp:1579`).
pub(in crate::shell) const PRESENTATION_CLOCK_ID: u32 = Monotonic::ID as u32;

impl FerriscopeState {
	/// Answer the presentation feedback of every committed surface: the frame
	/// with `sequence` was handed to the sink just now.
	///
	/// No display is involved, so `VSYNC` and `HW_CLOCK` are never set.
	/// `zero_copy` marks a frame that is the client's own buffer: the windows'
	/// feedback reports `ZERO_COPY` (only the passed-through window is drawn).
	pub(in crate::shell) fn send_presentation_feedback(&mut self, sequence: u64, zero_copy: bool) {
		let now = Clock::<Monotonic>::new().now();
		let output = self.output.clone();
		let flags = if zero_copy { Kind::ZeroCopy } else { Kind::empty() };
		let mut feedback = OutputPresentationFeedback::new(&output);
		for window in self.windows.windows() {
			window.take_presentation_feedback(&mut feedback, |_, _| Some(output.clone()), |_, _| flags);
			// The WSI layer's override surface presents in the window's place.
			if let Some(surface) = self.content_overrides.override_of(window) {
				take_presentation_feedback_surface_tree(
					&surface,
					&mut feedback,
					|_, _| Some(output.clone()),
					|_, _| flags,
				);
			}
		}
		if let Some((surface, _)) = self.cursor_render_target() {
			take_presentation_feedback_surface_tree(
				&surface,
				&mut feedback,
				|_, _| Some(output.clone()),
				|_, _| Kind::empty(),
			);
		}
		feedback.presented::<_, Monotonic>(now, Refresh::Fixed(self.scheduler.interval()), sequence, Kind::empty());
	}

	/// Answer the presentation feedback of every tracked window that is not
	/// drawn: the session-rate tick stands in for its vblank.
	///
	/// Drawn windows are answered when their frame is delivered
	/// ([`Self::send_presentation_feedback`]).
	pub(in crate::shell) fn send_undrawn_presentation_feedback(&mut self) {
		let drawn: Vec<_> = self
			.space
			.elements()
			.cloned()
			.chain(self.overlays.iter().map(|overlay| overlay.window.clone()))
			.collect();
		let now = Clock::<Monotonic>::new().now();
		let output = self.output.clone();
		let flags = Kind::empty();
		let mut feedback = OutputPresentationFeedback::new(&output);
		for window in self.windows.windows().filter(|window| !drawn.contains(window)) {
			window.take_presentation_feedback(&mut feedback, |_, _| Some(output.clone()), |_, _| flags);
			if let Some(surface) = self.content_overrides.override_of(window) {
				take_presentation_feedback_surface_tree(
					&surface,
					&mut feedback,
					|_, _| Some(output.clone()),
					|_, _| flags,
				);
			}
		}
		// No frame of its own: the sequence is the last delivered one.
		let sequence = self.scheduler.frames_delivered().saturating_sub(1);
		feedback.presented::<_, Monotonic>(now, Refresh::Fixed(self.scheduler.interval()), sequence, flags);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_clock_is_clock_monotonic() {
		assert_eq!(PRESENTATION_CLOCK_ID, 1);
	}
}
