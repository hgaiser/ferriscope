//! Input injection, pointer hit-testing and cursor clamping.

use std::time::Instant;

use smithay::{
	backend::input::{Axis, AxisSource, ButtonState, InputTime, KeyState, Keycode},
	desktop::{Window, WindowSurfaceType, space::SpaceElement},
	input::{
		keyboard::FilterResult,
		pointer::{AxisFrame, ButtonEvent, MotionEvent, PointerHandle, RelativeMotionEvent},
	},
	reexports::wayland_server::protocol::wl_surface::WlSurface,
	utils::{Logical, Point, Rectangle, SERIAL_COUNTER},
	wayland::{
		compositor::RectangleKind,
		pointer_constraints::{PointerConstraint, with_pointer_constraint},
	},
};

use crate::{
	focus::{PointerCapture, RegionRect, clamp_to_region, clip_region_to_bounds, surface_region},
	scaling::OutputFill,
	seat::InputEvent,
};

#[cfg(feature = "test-support")]
use crate::control::InputSource;
use crate::shell::FerriscopeState;
use crate::shell::OverlayLayer;
use crate::shell::geometry::overlay_pointer_point;
use crate::shell::geometry::union_rect;
use crate::shell::geometry::window_wl_surface;

/// A pointer-focus target: the surface under the pointer and its origin in the
/// frame the `MotionEvent` uses.
pub(in crate::shell) type PointerFocus = Option<(WlSurface, Point<f64, Logical>)>;

/// Continuous `wl_pointer.axis` units per wheel notch.
///
/// A `PointerAxis` carries whole detents (see `seat.rs`); this expands one to the
/// continuous value a client reads from `wl_pointer.axis`. gamescope emits
/// exactly one continuous unit per detent: every input path normalizes to
/// notches and `wlserver_mousewheel` passes that straight to
/// `wlr_seat_pointer_notify_axis` with `source = WHEEL`, while the discrete step
/// is notches × `WLR_POINTER_AXIS_DISCRETE_STEP` = notches × 120
/// (`wlserver.cpp:3131-3138`; libinput `v120 / 120`, nested Wayland
/// `nValue120 / 120`, EIS `discrete / 120`, IME `wheel / 120`, SDL wheel
/// clicks). The `axis_value120` step is therefore `120` per notch; both values
/// scale together, so continuous units per notch is `1.0`.
pub(in crate::shell) const AXIS_UNITS_PER_NOTCH: f64 = 1.0;

impl FerriscopeState {
	/// Resolve the pointer focus under an output-space point.
	///
	/// Returns the focused surface and its origin in the frame the
	/// `MotionEvent` must use: output space for the unscaled overlays and
	/// non-role windows, scene space for the role holder when the output-fill
	/// transform is active. The second element is that
	/// `MotionEvent` location.
	pub(in crate::shell) fn pointer_under(&self, pos: Point<f64, Logical>) -> (PointerFocus, Point<f64, Logical>) {
		// Overlays are above the space and below the cursor, so pointer focus
		// resolves to them first. Notifications sit above the interactive
		// overlay, matching the composited order.
		//
		// An overlay is drawn with its *own* content-to-output fit
		// (`render_frame_inner`), centred, independent of the role holder; its
		// input region must therefore be hit-tested in the same content space
		// the fit maps from. Hit-testing the raw scene rectangle would put the
		// focusable region somewhere other than where the overlay is drawn, so
		// a pointer over it could fall through to the window beneath.
		let output = self.output_rect().size;
		for role in [OverlayLayer::Notification, OverlayLayer::Interactive] {
			for overlay in self.overlays.iter().rev().filter(|overlay| overlay.role == role) {
				let fill = self
					.window_source_size(&overlay.window)
					.and_then(|size| OutputFill::for_source(output, size));
				let local = overlay_pointer_point(pos, fill, overlay.location);
				if let Some((surface, p)) = overlay.window.surface_under(local, WindowSurfaceType::ALL) {
					return (Some((surface, (p + overlay.location).to_f64())), pos);
				}
			}
		}

		// The role holder may be scaled to fill the output, so its
		// input region is the whole output, not its unscaled scene rectangle.
		// Hit-test it in scene space. A plain peer normal window is excluded
		// from the hit-test entirely while a role holder exists: it is
		// composited but never takes focus, so it cannot shadow the role by
		// sorting above it. A transient/dialog or popup above the role is
		// still found and wins.
		let role = self.focus.fullscreen();
		let fill = self.output_fill();
		let under = self.element_under_pointer(pos);
		let under_index = under
			.as_ref()
			.and_then(|(window, _)| self.space.elements().position(|element| element == *window));
		let role_index = fill
			.and(role)
			.and_then(|role_window| self.space.elements().position(|element| element == role_window));
		// The fill transform maps the content onto the output, so a point in
		// the content's output extent is on the role. Inverse-map it into
		// content space and clamp to the half-open content extent: without the
		// clamp a point on the far edge maps exactly onto `source`, the
		// exclusive edge of the surface hit-test, and the role loses focus at
		// the boundary even though the output pixel is still drawn from it.
		// The clamped scene point is reused as the reported location so the
		// delivered coordinate stays inside the surface and matches the hit.
		let role_scene = match (fill, role) {
			(Some(fill), Some(_)) => {
				let source = self.role_source_size().unwrap_or_default();
				let (cx, cy) = fill.output_to_content_clamped(pos.x, pos.y, source);
				Some(Point::from((cx, cy)) + self.role_origin())
			},
			_ => None,
		};
		let role_hit = match (role, role_scene) {
			(Some(role_window), Some(scene)) => self.space.element_location(role_window).and_then(|location| {
				self.window_surface_under(role_window, scene - location.to_f64())
					.map(|(surface, p)| (surface, (p + location).to_f64()))
			}),
			_ => None,
		};
		// `>=` so the role holder itself is still mapped through the fill
		// transform when the unscaled hit-test already found it.
		let role_on_top = match (under_index, role_index) {
			(Some(under), Some(role)) => role >= under,
			(None, Some(_)) => true,
			_ => false,
		};
		let (hit, point) = if role_on_top && role_hit.is_some() {
			(role_hit, role_scene.unwrap_or_else(|| self.output_to_scene(pos)))
		} else {
			// A dialog/popup on top, or no output-fill transform: the unscaled
			// element keeps output-space coordinates.
			let hit = under.and_then(|(window, location)| {
				self.window_surface_under(window, pos - location.to_f64())
					.map(|(surface, p)| (surface, (p + location).to_f64()))
			});
			(hit, pos)
		};
		(hit, point)
	}

	/// The topmost `space` element under `pos` that may take pointer focus, with
	/// its render location.
	///
	/// `space` holds exactly the windows being drawn (the focus window and its
	/// override, the override above), so this is [`Space::element_under`] with
	/// one adjustment: an element is hit by its bounding box and input region
	/// in the same render frame it is drawn in. The cache already decided who is
	/// in `space`; nothing here touches the X server.
	pub(in crate::shell) fn element_under_pointer(
		&self,
		pos: Point<f64, Logical>,
	) -> Option<(&Window, Point<i32, Logical>)> {
		for window in self.space.elements().rev() {
			let Some(location) = self.space.element_location(window) else {
				continue;
			};
			// `element_under` offsets by the element's render location, which
			// accounts for the window's own geometry origin.
			let render_location = location - window.geometry().loc;
			if self.content_overrides.presented(window).is_some() {
				// See `window_surface_under`: hit by the window's geometry.
				if Rectangle::new(location, window.geometry().size).to_f64().contains(pos) {
					return Some((window, render_location));
				}
				continue;
			}
			let Some(bbox) = self.space.element_bbox(window) else {
				continue;
			};
			if !bbox.to_f64().contains(pos) {
				continue;
			}
			if window.is_in_input_region(&(pos - render_location.to_f64())) {
				return Some((window, render_location));
			}
		}
		None
	}

	/// The surface of `window` under the window-local `point`.
	///
	/// A window the WSI layer presents for (gamescope's Xwayland bypass) shows
	/// the layer's surface, while Xwayland's own surface for it may never get a
	/// buffer and so has no input region. Input still belongs to the X window,
	/// so such a window is hit by its geometry and the event goes to its own
	/// surface, which Xwayland delivers to the X client. gamescope likewise
	/// routes input to the focused window, not the bypass surface.
	fn window_surface_under(
		&self,
		window: &Window,
		point: Point<f64, Logical>,
	) -> Option<(WlSurface, Point<i32, Logical>)> {
		window.surface_under(point, WindowSurfaceType::ALL).or_else(|| {
			self.content_overrides.presented(window)?;
			Rectangle::from_size(window.geometry().size)
				.to_f64()
				.contains(point)
				.then(|| window_wl_surface(window).map(|surface| (surface, Point::default())))
				.flatten()
		})
	}

	/// Pointer motion or a button: show the cursor and restart its idle timer
	/// (`wlserver_mousemotion`, `wlserver_mousewarp`, `wlserver_mousebutton`:
	/// `ulLastMovedCursorTime = now`, cursor shown,
	/// `wlserver.cpp:3070`, `:3097`, `:3122`). A locked pointer's relative
	/// motion returns before this in gamescope (`wlserver_apply_constraint`).
	pub(in crate::shell) fn note_pointer_moved(&mut self) {
		self.cursor_suspension.moved(Instant::now());
	}

	/// `MouseCursor::checkSuspension` (`steamcompmgr.cpp:2013-2053`): hide the
	/// cursor once idle past the configured time and, when the input focus
	/// window is the Steam UI, warp the pointer to its bottom-right corner so
	/// Steam sees no hover (`wlserver_fake_mouse_pos`, `wlserver.cpp:3115`).
	pub(in crate::shell) fn check_cursor_suspension(&mut self, now: Instant) {
		if !self.cursor_suspension.check(now) {
			return;
		}
		let steam_input = self
			.focus
			.input_focus_id()
			.and_then(|id| self.windows.get(id))
			.filter(|managed| managed.meta.is_steam())
			.map(|managed| (managed.meta.geometry.w, managed.meta.geometry.h));
		let Some((width, height)) = steam_input else {
			return;
		};
		// `wlserver_mousehide`: forget the last move so the hide is not undone.
		self.cursor_suspension.hide_now();
		let (under, _) = self.pointer_under(self.cursor.location);
		let Some((surface, origin)) = under else {
			return;
		};
		// The seat pointer moves; the compositor's own cursor position does not.
		let location = origin + Point::from((f64::from(width) - 1.0, f64::from(height) - 1.0));
		if let Some(pointer) = self.seat.get_pointer() {
			pointer.motion(
				self,
				Some((surface, origin)),
				&MotionEvent {
					location,
					serial: SERIAL_COUNTER.next_serial(),
					time: InputTime::now(),
				},
			);
			pointer.frame(self);
		}
	}

	#[cfg(feature = "test-support")]
	pub(in crate::shell) fn pump_control_stream(&mut self) {
		let events = match self.control.as_mut() {
			Some(source) => match source.poll() {
				Ok(events) => events,
				Err(e) => {
					tracing::warn!("control stream: {e}");
					return;
				},
			},
			None => return,
		};
		for event in events {
			tracing::debug!(?event, "control stream event");
			self.apply_input(event);
		}
	}

	pub(in crate::shell) fn apply_input(&mut self, event: InputEvent) {
		match event {
			InputEvent::Key { code, pressed } => {
				// Text queued before this key is typed before it, and its last
				// key is up by then.
				self.ime_flush_text();
				self.ime_release_held();
				let serial = SERIAL_COUNTER.next_serial();
				let state = if pressed { KeyState::Pressed } else { KeyState::Released };
				// Smithay/xkbcommon keycodes are X keycodes (evdev + 8); the
				// control stream carries raw evdev. Without the offset the
				// keysym derived below is wrong, and hotkeys would never match.
				let keycode = Keycode::new(code + 8);
				if let Some(keyboard) = self.seat.get_keyboard() {
					keyboard.input::<(), _>(self, keycode, state, serial, InputTime::now(), |data, _, key| {
						// gamescope action bindings: match the held
						// keysym set against armed bindings and let a blocking
						// match swallow the chord.
						let intercept = data.action_bindings.on_key(code + 8, key.modified_sym(), pressed);
						if intercept {
							FilterResult::Intercept(())
						} else {
							FilterResult::Forward
						}
					});
				}
			},
			InputEvent::Text(text) => self.ime_queue_text(&text),
			InputEvent::PointerButton { code, pressed } => self.pointer_button(code, pressed),
			InputEvent::PointerMotion { x, y } => self.warp_pointer(x, y),
			InputEvent::PointerMotionDelta { dx, dy } => {
				// Relative motion is the mouselook path and always flows: a
				// client with a `zwp_relative_pointer` object receives it even
				// while locked. The delta is a device/surface-local
				// delta and is forwarded **unscaled**; gamescope does the same
				// (`wlserver_mousemotion` -> `wlserver_perform_rel_pointer_motion`,
				// `wlserver.cpp:3055-3062`) and only the absolute cursor is
				// mapped through the output-fill transform. Scaling the relative
				// delta by the fill factor reduced the look range.
				let (under, _) = self.pointer_under(self.cursor.location);
				let on_role = under
					.as_ref()
					.is_some_and(|(surface, _)| self.surface_is_fullscreen_window(surface));
				// A locked pointer does not move: gamescope returns from
				// `wlserver_apply_constraint` before the advance, so only the
				// relative event flows. Otherwise — confined **or
				// unconstrained** — gamescope advances its surface-local cursor
				// by the delta, clamps it (`wlserver_clampcursor`,
				// `wlserver.cpp:2796-2801`), and notifies the absolute position
				// on the same event (`wlr_seat_pointer_notify_motion`,
				// `wlserver.cpp:3073-3085`), so a client that tracks the
				// absolute pointer (Steam Big Picture's hover) keeps up.
				//
				// The advance is in surface-local units, mapped back to output
				// space through the fill transform (identity without fill), and
				// clamped by `clamp_to_capture`. Its unconstrained branch is
				// the focused-window clamp, so the absolute notify cannot
				// reposition the cursor off the output once a capture is
				// released (`capture == None`); the bound lives in the clamp, not
				// in skipping the cursor advance.
				let mut absolute = None;
				if !self.capture.is_locked() {
					self.note_pointer_moved();
					let (ox, oy) = if on_role {
						self.cursor_input_delta(dx, dy)
					} else {
						(dx, dy)
					};
					let proposed = self.cursor.location + Point::from((ox, oy));
					self.cursor.location = self.clamp_to_capture(proposed);
					absolute = Some(self.pointer_under(self.cursor.location));
				}
				// A **locked** pointer skips the advance above, so a
				// relative-only client (Moonlight's default mouse mode) would
				// never get `wl_pointer.enter`: with no pointer focus its
				// `zwp_relative_pointer` and pointer buttons go nowhere and only
				// keyboard input works.
				//
				// Seed focus at the current cursor position when it is not on the
				// surface under the cursor and no advance notified one (a non-locked
				// pointer already notified above), mirroring `new_constraint`'s lock
				// seed. The seeded point is the already-clamped cursor, so the
				// `enter` lands on the role rather than at a stale (0,0).
				//
				// The focus can be stale rather than absent: a window that maps under
				// a still pointer and locks it (a Proton game on winewayland hides its
				// cursor while loading) leaves the focus on the window that was there
				// before. Relative motion goes to the focused client, so the game
				// would never get `enter`, and winewayland only enables relative
				// motion for the window it has pointer focus on.
				let seed_focus = self.seat.get_pointer().is_some_and(|pointer| {
					pointer.current_focus() != under.as_ref().map(|(surface, _)| surface.clone())
				});
				let mut seeded = None;
				if seed_focus && absolute.is_none() {
					seeded = Some(self.pointer_under(self.cursor.location));
				}
				tracing::debug!(
					delivered_dx = dx,
					delivered_dy = dy,
					on_role,
					focused = under.is_some(),
					abs_delivered = absolute.is_some(),
					focus_seeded = seeded.is_some(),
					capture = ?self.capture,
					location = ?self.cursor.location,
					fill = ?self.output_fill(),
					"relative motion"
				);
				if let Some(pointer) = self.seat.get_pointer() {
					// The cursor advance (confined or unconstrained) and the
					// locked-focus seed both notify the absolute pointer; the
					// seed is gated on `absolute.is_none()` so the two never
					// fire together.
					if let Some((under, event_location)) = absolute.or(seeded) {
						self.deliver_motion(&pointer, under, event_location);
					}
					// The client's relative pointer gets the device delta
					// unchanged (gamescope parity).
					pointer.relative_motion(
						self,
						under,
						&RelativeMotionEvent {
							delta: Point::from((dx, dy)),
							delta_unaccel: Point::from((dx, dy)),
							time: InputTime::now(),
						},
					);
					pointer.frame(self);
				}
			},
			InputEvent::TouchDown { slot, x, y } => self.touch_down(slot, x, y),
			InputEvent::TouchMotion { slot, x, y } => self.touch_motion(slot, x, y),
			InputEvent::TouchUp { slot } => self.touch_up(slot),
			InputEvent::TouchCancel => self.touch_cancel(),
			InputEvent::TouchFrame => self.touch_frame(),
			InputEvent::Pen(event) => self.pen_input(event),
			InputEvent::PenButtons { buttons } => self.pen_buttons(buttons),
			InputEvent::PenLeave => self.pen_leave(),
			InputEvent::PointerAxis { horizontal, value } => {
				// Scroll reaches the focused pointer only, like gamescope's
				// `wlserver_mousewheel` (`wlserver.cpp:3131-3138`) which calls
				// `wlr_seat_pointer_notify_axis` with `WL_POINTER_AXIS_SOURCE_WHEEL`
				// and ends the frame. ferriscope receives one axis per `PointerAxis`
				// event, so it emits a single-axis frame. `notify_axis` reaches the
				// focused surface; with no pointer focus (no `enter` yet) it goes
				// nowhere, so the adapter/test must establish focus first.
				//
				// `value` is in wheel notches with the Wayland sign convention
				// (positive = right / down; the host adapter already flipped
				// Moonlight's positive-up vertical). It is expanded to the
				// continuous value at `AXIS_UNITS_PER_NOTCH` per notch and to the
				// discrete step `axis_value120` (`120` per notch). This is
				// gamescope's exact mapping: one continuous unit and 120
				// `axis_value120` per detent (`wlserver.cpp:3131-3138`). Unlike
				// the pointer cursor, scroll is not scaled by the
				// output-fill transform: it is a device-level delta (gamescope passes
				// the value through unchanged).
				let axis = if horizontal { Axis::Horizontal } else { Axis::Vertical };
				let frame = AxisFrame::new(InputTime::now())
					.source(AxisSource::Wheel)
					.value(axis, value * AXIS_UNITS_PER_NOTCH)
					.v120(axis, (value * 120.0).round() as i32)
					.stop(axis);
				if let Some(pointer) = self.seat.get_pointer() {
					pointer.axis(self, frame);
					pointer.frame(self);
				}
			},
		}
	}
}

impl FerriscopeState {
	/// Press or release a pointer button at the current pointer position.
	pub(in crate::shell) fn pointer_button(&mut self, code: u32, pressed: bool) {
		self.note_pointer_moved();
		let serial = SERIAL_COUNTER.next_serial();
		let state = if pressed {
			ButtonState::Pressed
		} else {
			ButtonState::Released
		};
		if let Some(pointer) = self.seat.get_pointer() {
			pointer.button(
				self,
				&ButtonEvent {
					serial,
					time: InputTime::now(),
					button: code,
					state,
				},
			);
			pointer.frame(self);
			// Latency tooling: the button reached the focused client.
			tracing::trace!(target: "ferriscope::latency", code, pressed, "button");
		}
	}

	/// Move the pointer to an absolute output-space position.
	pub(in crate::shell) fn warp_pointer(&mut self, x: f64, y: f64) {
		self.note_pointer_moved();
		// Absolute motion is delivered even to a locked pointer. The
		// lock constraint only stops the **relative** path from
		// advancing the cursor;
		// gamescope's absolute path, `wlserver_mousewarp`, sets the
		// surface-local cursor and calls `wlr_seat_pointer_notify_motion`
		// without consulting the constraint (`wlserver.cpp:3088-3108`).
		//
		// So a client that only sends absolute motion (absolute mouse
		// mode, or a windowed/uncaptured client) can still look while the
		// game holds a lock: the event must not be dropped on
		// `is_locked()`. The absolute position is clamped to the focused
		// window and its visibility is the client's business.
		let location = self.clamp_to_capture(Point::from((x, y)));
		self.cursor.location = location;
		let (under, event_location) = self.pointer_under(location);
		tracing::debug!(
			?location,
			under_role = under
				.as_ref()
				.is_some_and(|(surface, _)| self.surface_is_fullscreen_window(surface)),
			focused = under.is_some(),
			capture = ?self.capture,
			"pointer motion"
		);
		if let Some(pointer) = self.seat.get_pointer() {
			self.deliver_motion(&pointer, under, event_location);
			pointer.frame(self);
		}
	}

	/// Deliver an absolute pointer motion.
	///
	/// A popup grab whose popup is gone releases itself on the next motion
	/// and drops that motion, leaving the pointer focus on the destroyed
	/// popup, so the next click would be lost. Resend the motion once the
	/// grab has released.
	pub(in crate::shell) fn deliver_motion(
		&mut self,
		pointer: &PointerHandle<Self>,
		under: PointerFocus,
		location: Point<f64, Logical>,
	) {
		let grabbed = pointer.is_grabbed();
		let event = |location| MotionEvent {
			location,
			serial: SERIAL_COUNTER.next_serial(),
			time: InputTime::now(),
		};
		pointer.motion(self, under.clone(), &event(location));
		if grabbed && !pointer.is_grabbed() {
			pointer.motion(self, under, &event(location));
		}
	}
}

/// The active pointer capture on `surface`, if its constraint is active.
pub(in crate::shell) fn active_capture(
	surface: &WlSurface,
	pointer: &PointerHandle<FerriscopeState>,
) -> PointerCapture {
	with_pointer_constraint(surface, pointer, |constraint| match constraint {
		Some(constraint) if constraint.is_active() => match &*constraint {
			PointerConstraint::Locked(_) => PointerCapture::Locked,
			PointerConstraint::Confined(_) => PointerCapture::Confined,
		},
		_ => PointerCapture::None,
	})
}

impl FerriscopeState {
	pub(in crate::shell) fn set_capture(&mut self, surface: WlSurface, capture: PointerCapture) {
		tracing::debug!(?capture, "set pointer capture");
		self.capture = capture;
		self.capture_surface = capture.is_active().then_some(surface);
	}

	/// Clamp a proposed absolute pointer position to the active confine region,
	/// returning the nearest allowed output-space point.
	///
	/// The region is in the constrained surface's own (surface-local) space, so
	/// the pointer is mapped through the same output-fill inverse that
	/// `pointer_under` uses and mapped back on the way out (identity without
	/// scaling). The effective region is the client's `wl_region` intersected
	/// with the surface; a missing region means the whole surface (the protocol
	/// default and what Xwayland sends for an X confine-to grab). Clamping to
	/// the surface keeps the pointer on it, so an edge clamp never drops focus
	/// and releases the capture.
	pub(in crate::shell) fn clamp_to_capture(&self, proposed: Point<f64, Logical>) -> Point<f64, Logical> {
		if !self.capture.is_confined() {
			// gamescope always clamps its surface-local cursor to the focused
			// surface extent (`wlserver_clampcursor` against
			// `wlserver_get_cursor_bounds`), constraint or not. The role holder
			// is fit-scaled into the output, so its rendered content
			// rect — the output-fill extent `offset..offset+source*scale` — is
			// narrower than the output; clamping to the output rect instead let
			// the cursor drift into the pillarbox/letterbox bars while no
			// constraint was active.
			// Clamp to the focused window's rendered rect, matching gamescope,
			// so a focused overlay/dialog on top stays reachable.
			let (min_x, min_y, max_x, max_y) = self.unconstrained_cursor_bounds().unwrap_or_else(|| {
				let output = self.output_rect();
				(
					0.0,
					0.0,
					f64::from(output.size.w.saturating_sub(1)),
					f64::from(output.size.h.saturating_sub(1)),
				)
			});
			return Point::from((proposed.x.clamp(min_x, max_x), proposed.y.clamp(min_y, max_y)));
		}
		let Some(surface) = self.capture_surface.as_ref() else {
			return proposed;
		};
		let Some(pointer) = self.seat.get_pointer() else {
			return proposed;
		};
		let scene = self.output_to_scene(proposed);
		let origin = self.window_origin_for_surface(surface).unwrap_or_default();
		let local = (scene - origin).to_f64();
		let bounds = self.surface_logical_size(surface).unwrap_or_default();
		let clamped = with_pointer_constraint(surface, &pointer, |constraint| {
			let constraint = constraint.filter(|constraint| constraint.is_active())?;
			let rects: Vec<RegionRect> = match constraint.region() {
				// Intersect a client region with the surface, as wlroots does
				// (`pointer_constraint_commit`), so a region larger than the
				// surface cannot clamp the pointer off it.
				Some(region) => clip_region_to_bounds(
					&region
						.rects
						.iter()
						.map(|(kind, rect)| {
							(
								matches!(kind, RectangleKind::Add),
								rect.loc.x,
								rect.loc.y,
								rect.size.w,
								rect.size.h,
							)
						})
						.collect::<Vec<_>>(),
					bounds.w,
					bounds.h,
				),
				// No region is the whole surface.
				None => surface_region(bounds.w, bounds.h),
			};
			Some(clamp_to_region((local.x, local.y), &rects))
		});
		match clamped {
			Some((x, y)) => self.scene_to_output(origin + Point::from((x, y))),
			None => proposed,
		}
	}

	/// The inclusive output-space bounds the unconstrained cursor may occupy:
	/// the focused window's rendered content rect, or the role holder's when
	/// nothing else holds focus.
	///
	/// gamescope clamps its cursor to the focused surface's extent plus the
	/// active dropdowns (`wlserver_get_cursor_bounds`, `wlserver.cpp:2781-2793`).
	/// ferriscope's analog is the role holder's output-fill content rect
	///expanded to the focused overlay/dialog when one is on top so
	/// the cursor can still reach it. Returns `None` with no role holder, so the
	/// caller keeps the whole-output clamp.
	pub(in crate::shell) fn unconstrained_cursor_bounds(&self) -> Option<(f64, f64, f64, f64)> {
		let role = self.focus.fullscreen()?;
		let mut bounds = self.rendered_window_rect(role);
		if let Some(focused) = self.focus.keyboard() {
			if focused != role {
				bounds = union_rect(bounds, self.rendered_window_rect(focused));
			}
		}
		// Half-open to inclusive: gamescope clamps to `extent - 1`.
		Some((
			bounds.0,
			bounds.1,
			(bounds.2 - 1.0).max(bounds.0),
			(bounds.3 - 1.0).max(bounds.1),
		))
	}
}
