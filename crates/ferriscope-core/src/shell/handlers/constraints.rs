//! Pointer-constraint handler.

use smithay::{
	backend::input::InputTime,
	input::pointer::{MotionEvent, PointerHandle},
	reexports::wayland_server::protocol::wl_surface::WlSurface,
	utils::{Logical, Point, SERIAL_COUNTER},
	wayland::pointer_constraints::{ConstraintRemove, PointerConstraintsHandler, with_pointer_constraint},
};

use crate::focus::PointerCapture;

use crate::shell::FerriscopeState;
use crate::shell::geometry::root_surface;
use crate::shell::input::active_capture;

impl PointerConstraintsHandler for FerriscopeState {
	fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
		// A constraint only applies to the single fullscreen window; a client
		// cannot capture the pointer through an overlay.
		if !self.surface_is_fullscreen_window(surface) {
			tracing::debug!(
				surface = smithay::reexports::wayland_server::Resource::id(surface).protocol_id(),
				"pointer constraint on a surface other than the fullscreen window; ignored"
			);
			return;
		}

		// Activate immediately: the window is the only focusable target, so it
		// does not need to wait for a pointer enter to honour lock/confine.
		with_pointer_constraint(surface, pointer, |constraint| {
			if let Some(constraint) = constraint {
				constraint.activate();
			}
		});
		let capture = active_capture(surface, pointer);
		self.set_capture(surface.clone(), capture);
		tracing::debug!(
			?capture,
			bounds = ?self.surface_logical_size(surface),
			role_source = ?self.role_source_size(),
			fill = ?self.output_fill(),
			origin = ?self.window_origin_for_surface(surface),
			"new constraint"
		);

		// Confinement guarantees the pointer is inside the region once the
		// `confined` event is sent; if focus was outside it, warp to the
		// nearest allowed point. (A locked pointer is held where it is.)
		if capture.is_confined() {
			let clamped = self.clamp_to_capture(self.cursor.location);
			if clamped != self.cursor.location {
				self.cursor.location = clamped;
				let (under, event_location) = self.pointer_under(clamped);
				let serial = SERIAL_COUNTER.next_serial();
				pointer.motion(
					self,
					under,
					&MotionEvent {
						location: event_location,
						serial,
						time: InputTime::now(),
					},
				);
				pointer.frame(self);
			}
		}

		// A locked pointer with no prior enter would receive no relative motion
		// at all: the relative-pointer route needs a pointer focus. Seed it with
		// an absolute position inside the window; the cursor is hidden anyway.
		// The focus may also still be on another window (the window mapped
		// under a pointer that has not moved since), which is as good as none:
		// relative motion goes to the focused client.
		let focused_here = pointer
			.current_focus()
			.is_some_and(|focus| root_surface(&focus) == root_surface(surface));
		if capture.is_locked() && !focused_here {
			// `window_center_for_surface` is in scene space; the cursor lives in
			// output space, so map it back through the fill transform
			// (identity without scaling).
			let center_scene = self.window_center_for_surface(surface).unwrap_or(self.cursor.location);
			let center = self.scene_to_output(center_scene);
			let (under, event_location) = self.pointer_under(center);
			let serial = SERIAL_COUNTER.next_serial();
			self.cursor.location = center;
			pointer.motion(
				self,
				under,
				&MotionEvent {
					location: event_location,
					serial,
					time: InputTime::now(),
				},
			);
			pointer.frame(self);
		}
	}

	fn remove_constraint(
		&mut self,
		surface: &WlSurface,
		_pointer: &PointerHandle<Self>,
		_constraint_remove: ConstraintRemove,
	) {
		tracing::debug!(
			?surface,
			capture = ?self.capture,
			was_capture_surface = self.capture_surface.as_ref() == Some(surface),
			location = ?self.cursor.location,
			"remove constraint"
		);
		// The constraint was deactivated by smithay before this callback.
		if self.capture_surface.as_ref() == Some(surface) {
			self.capture = PointerCapture::None;
			self.capture_surface = None;
		}

		// Restore the pointer to the client's committed hint, if it gave one.
		// The hint is surface-local, so the scene point is mapped back through
		// the output-fill transform (identity without scaling) before it
		// becomes the output-space cursor position.
		if let Some((hint_surface, hint)) = self.cursor_position_hint.take() {
			if &hint_surface == surface {
				if let Some(origin) = self.window_origin_for_surface(surface) {
					self.cursor.location = self.scene_to_output(origin + hint);
				}
			}
		}
	}

	fn cursor_position_hint(
		&mut self,
		surface: &WlSurface,
		_pointer: &PointerHandle<Self>,
		location: Point<f64, Logical>,
	) {
		tracing::debug!(?surface, ?location, capture = ?self.capture, "cursor position hint");
		if self.capture.is_active() {
			self.cursor_position_hint = Some((surface.clone(), location));
		}
	}
}
