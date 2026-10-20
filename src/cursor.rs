//! Client-supplied cursor images, software-composited into the frame.
//!
//! ferriscope has no hardware cursor plane, so
//! a visible cursor is blended by the composite pass at
//! [`CursorState::image_location`]. Only client-supplied images are
//! drawn — there is no Xcursor theme to load — so a named cursor has no image
//! and renders nothing.
//!
//! Visibility policy: the cursor is drawn iff a
//! client has set an image and has not hidden it. Pointer capture
//! (`zwp_pointer_constraints` lock/confine) never affects visibility — gamescope
//! hides only on `bCursorHidden || m_imageEmpty` (`steamcompmgr.hpp:89`), and a
//! constraint only moves/clamps the position. A named cursor keeps the previous
//! image rather than clearing it, because there is no theme to load and Smithay
//! emits a named cursor on every pointer `leave`/`replace`.
//!
//! [`CursorState`] owns only the image and its visibility; the pointer position
//! lives in [`Cursor`]. Both Wayland and X11 clients reach this module
//! through the same route: Xwayland is a Wayland client and supplies its cursor
//! image via `wl_pointer.set_cursor`.

use std::time::{Duration, Instant};

use smithay::{
	input::pointer::{CursorImageStatus, CursorImageSurfaceData},
	reexports::wayland_server::protocol::wl_surface::WlSurface,
	utils::{Logical, Point},
	wayland::compositor,
};

/// A client-supplied cursor image: the surface to draw and its hotspot.
///
/// Generic over the surface type only so the state machine can be unit-tested
/// without a live Wayland display; production code uses [`CursorState`] with
/// `S = WlSurface`.
#[derive(Debug, Clone)]
pub struct CursorImage<S = WlSurface> {
	/// The surface the client set as its cursor.
	pub surface: S,
	/// Hotspot within the image, in logical coordinates.
	pub hotspot: Point<i32, Logical>,
}

/// gamescope's default `cursorHideTime` (`steamcompmgr.cpp:1171`).
pub const DEFAULT_CURSOR_HIDE_TIME: Duration = Duration::from_secs(10);

/// Cursor auto-hide: `wlserver.bCursorHidden` and `ulLastMovedCursorTime` with
/// the `MouseCursor::checkSuspension` rule (`steamcompmgr.cpp:2013-2053`).
///
/// A pointer move or button press shows the cursor and stamps the time
/// (`wlserver_mousemotion`/`wlserver_mousewarp`/`wlserver_mousebutton`,
/// `wlserver.cpp:3070`, `:3097`, `:3122`); once `hide_after` has passed without
/// one, [`Self::check`] hides it. A cursor that has never moved counts as long
/// idle (`ulLastMovedCursorTime` starts at 0, `wlserver.cpp:2846`).
#[derive(Debug, Clone, Copy)]
pub struct CursorSuspension {
	hide_after: Duration,
	last_moved: Option<Instant>,
	hidden: bool,
}

impl CursorSuspension {
	/// A suspension that hides the cursor after `hide_after` without motion.
	pub fn new(hide_after: Duration) -> Self {
		Self {
			hide_after,
			last_moved: None,
			hidden: false,
		}
	}

	/// Whether the cursor is currently suspended (`bCursorHidden`).
	pub fn hidden(&self) -> bool {
		self.hidden
	}

	/// Pointer motion or a button at `now`: the cursor shows again.
	///
	/// gamescope sets `bCursorHidden = !bCursorHasImage` here; ferriscope
	/// receives a client's cursor image only once the pointer has entered a
	/// surface, so a missing image is handled by the renderer (nothing to draw)
	/// and never latches the cursor hidden.
	pub fn moved(&mut self, now: Instant) {
		self.last_moved = Some(now);
		self.hidden = false;
	}

	/// Hide immediately and forget the last move (`wlserver_mousehide`,
	/// `wlserver.cpp:2844`).
	pub fn hide_now(&mut self) {
		self.last_moved = None;
		self.hidden = true;
	}

	/// The `checkSuspension` transition at `now`: returns `true` exactly when
	/// the cursor becomes hidden by idleness.
	pub fn check(&mut self, now: Instant) -> bool {
		if self.hidden {
			return false;
		}
		let idle = match self.last_moved {
			Some(moved) => now.saturating_duration_since(moved) > self.hide_after,
			None => true,
		};
		if idle {
			self.hidden = true;
		}
		idle
	}

	/// When [`Self::check`] next needs to run, or `None` while hidden.
	pub fn deadline(&self, now: Instant) -> Option<Instant> {
		if self.hidden {
			return None;
		}
		Some(
			self.last_moved
				.map_or(now, |moved| moved + self.hide_after + Duration::from_millis(1)),
		)
	}
}

/// The compositor's cursor-image state.
#[derive(Debug)]
pub struct CursorState<S = WlSurface> {
	image: Option<CursorImage<S>>,
}

impl<S> Default for CursorState<S> {
	fn default() -> Self {
		Self { image: None }
	}
}

impl<S> CursorState<S> {
	/// A cursor with no image.
	pub fn new() -> Self {
		Self::default()
	}

	/// Replace (or clear, with `None`) the current image.
	///
	/// Private: production callers go through [`CursorState::set_status`], which
	/// reads the hotspot off the surface.
	fn set_image(&mut self, image: Option<CursorImage<S>>) {
		self.image = image;
	}

	/// The current image, if a client has set one.
	pub fn image(&self) -> Option<&CursorImage<S>> {
		self.image.as_ref()
	}

	/// Apply the image-independent part of a client cursor request: `Hidden`
	/// clears the image, `Named` keeps it.
	///
	/// Kept generic so the policy is unit-testable without a live `WlSurface`;
	/// [`CursorState::set_status`] adds the `Surface` arm.
	fn set_hidden_or_named(&mut self, hidden: bool) {
		if hidden {
			self.set_image(None);
		}
	}

	/// Top-left of the image for a pointer at `location`, or `None` when the
	/// cursor is not visible.
	///
	/// Subtracting the hotspot turns the pointer position into the image origin;
	/// the value is in output logical coordinates, like [`Cursor::location`].
	pub fn image_location(&self, location: Point<f64, Logical>) -> Option<Point<f64, Logical>> {
		let image = self.image.as_ref()?;
		Some(location - image.hotspot.to_f64())
	}
}

impl CursorState<WlSurface> {
	/// Apply a client cursor request.
	///
	/// A surface installs (or replaces) the image; `Hidden` clears it. A named
	/// cursor has no image here because there is no Xcursor theme to load
	///so it is treated as "keep the current image": Smithay emits
	/// `Named(Default)` on a pointer `leave`/`replace`, and clearing on it
	/// would erase a client image the client never replaces (Xwayland only
	/// re-sets its cursor on its own pointer enter), leaving no cursor at all
	/// after the pointer crosses a window edge. gamescope likewise keeps
	/// drawing the last known image (`MouseCursor::getTexture`).
	pub fn set_status(&mut self, status: &CursorImageStatus) {
		match status {
			CursorImageStatus::Surface(surface) => {
				self.set_image(Some(CursorImage {
					hotspot: hotspot_of(surface),
					surface: surface.clone(),
				}));
			},
			CursorImageStatus::Hidden => self.set_hidden_or_named(true),
			// A named cursor keeps the previous image: Smithay emits
			// `Named(Default)` on every pointer `leave`/`replace`, and the
			// client (Xwayland re-sets only on its own pointer enter) would
			// otherwise be left with no cursor.
			CursorImageStatus::Named(_) => self.set_hidden_or_named(false),
		}
	}
}

/// The hotspot the client attached to `surface`, or the origin if unset.
///
/// Smithay stores it in the surface's cursor-image data when `set_cursor` runs.
/// A poisoned lock is treated as "no hotspot" rather than panicking.
fn hotspot_of(surface: &WlSurface) -> Point<i32, Logical> {
	compositor::with_states(surface, |states| {
		states
			.data_map
			.get::<CursorImageSurfaceData>()
			.map(|attributes| {
				attributes
					.lock()
					.map(|attributes| attributes.hotspot)
					.unwrap_or_default()
			})
			.unwrap_or_default()
	})
}

/// Cursor state owned by the compositor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cursor {
	/// Pointer position in output logical coordinates.
	pub location: Point<f64, Logical>,
	/// Hotspot within the cursor image.
	pub hotspot: Point<i32, Logical>,
	/// Whether the cursor should be drawn.
	pub visible: bool,
}

impl Default for Cursor {
	fn default() -> Self {
		Self {
			location: Point::from((0.0, 0.0)),
			hotspot: Point::from((0, 0)),
			visible: false,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A cursor whose image payload is `()` so no Wayland display is needed.
	fn state() -> CursorState<()> {
		CursorState::default()
	}

	fn image(hotspot: (i32, i32)) -> CursorImage<()> {
		CursorImage {
			surface: (),
			hotspot: Point::from(hotspot),
		}
	}

	#[test]
	fn hidden_until_an_image_is_set() {
		let state = state();
		assert!(state.image().is_none());
		assert!(state.image().is_none());
		assert_eq!(state.image_location(Point::from((10.0, 20.0))), None);
	}

	#[test]
	fn setting_an_image_makes_it_visible() {
		let mut state = state();
		state.set_image(Some(image((0, 0))));
		assert!(state.image().is_some());
		assert!(state.image().is_some());
	}

	#[test]
	fn hotspot_offsets_the_image_location() {
		let mut state = state();
		state.set_image(Some(image((4, 6))));
		assert_eq!(
			state.image_location(Point::from((100.0, 50.0))),
			Some(Point::from((96.0, 44.0)))
		);
	}

	#[test]
	fn a_replacement_image_takes_effect() {
		let mut state = state();
		state.set_image(Some(image((0, 0))));
		state.set_image(Some(image((2, 3))));
		assert_eq!(state.image().expect("image").hotspot, Point::from((2, 3)));
		assert_eq!(
			state.image_location(Point::from((10.0, 10.0))),
			Some(Point::from((8.0, 7.0)))
		);
	}

	#[test]
	fn clearing_the_image_hides_the_cursor() {
		let mut state = state();
		state.set_image(Some(image((0, 0))));
		state.set_image(None);
		assert!(state.image().is_none());
		assert!(state.image().is_none());
	}

	#[test]
	fn an_image_is_drawn_regardless_of_pointer_capture() {
		// gamescope parity: visibility depends only on the client
		// status. Capture (None/Confined/Locked) never reaches `CursorState`, so
		// any set image is drawn; this asserts the client-status half.
		let mut state = state();
		state.set_image(Some(image((1, 1))));
		assert!(state.image().is_some());
		assert_eq!(
			state.image_location(Point::from((10.0, 10.0))),
			Some(Point::from((9.0, 9.0)))
		);
	}

	#[test]
	fn hidden_status_has_no_image() {
		let mut state = CursorState::<WlSurface>::default();
		state.set_status(&CursorImageStatus::Hidden);
		assert!(state.image().is_none());
		assert!(state.image().is_none());
	}

	#[test]
	fn named_status_with_no_image_stays_hidden() {
		let mut state = CursorState::<WlSurface>::default();
		state.set_status(&CursorImageStatus::default_named());
		assert!(state.image().is_none());
		assert!(state.image().is_none());
	}

	#[test]
	fn named_status_keeps_a_client_image() {
		// Smithay emits `Named(Default)` on every pointer leave/replace; a
		// client surface set earlier must survive it. Driven through
		// the generic policy hook so no live Wayland surface is needed.
		let mut state = state();
		state.set_image(Some(image((2, 3))));
		state.set_hidden_or_named(false);
		assert!(
			state.image().is_some(),
			"a named cursor must not discard a client image"
		);
		assert_eq!(state.image().expect("image").hotspot, Point::from((2, 3)));
	}

	#[test]
	fn hidden_status_clears_a_client_image() {
		let mut state = state();
		state.set_image(Some(image((2, 3))));
		state.set_hidden_or_named(true);
		assert!(state.image().is_none());
		assert!(state.image().is_none());
	}

	#[test]
	fn suspension_hides_after_the_idle_time_and_motion_shows_it() {
		let start = Instant::now();
		let mut cursor = CursorSuspension::new(Duration::from_secs(10));
		cursor.moved(start);
		assert!(!cursor.check(start + Duration::from_secs(10)));
		assert!(!cursor.hidden());
		assert!(cursor.check(start + Duration::from_secs(11)));
		assert!(cursor.hidden());
		// Already hidden: no second transition.
		assert!(!cursor.check(start + Duration::from_secs(12)));
		cursor.moved(start + Duration::from_secs(13));
		assert!(!cursor.hidden());
	}

	#[test]
	fn a_never_moved_cursor_is_idle() {
		let mut cursor = CursorSuspension::new(Duration::from_secs(10));
		assert!(cursor.check(Instant::now()));
	}

	#[test]
	fn hide_now_forgets_the_last_move() {
		let now = Instant::now();
		let mut cursor = CursorSuspension::new(Duration::from_secs(10));
		cursor.moved(now);
		cursor.hide_now();
		assert!(cursor.hidden());
		assert_eq!(cursor.deadline(now), None);
	}

	#[test]
	fn deadline_is_the_idle_expiry() {
		let now = Instant::now();
		let mut cursor = CursorSuspension::new(Duration::from_secs(10));
		cursor.moved(now);
		let deadline = cursor.deadline(now).expect("armed while shown");
		assert!(deadline > now + Duration::from_secs(10));
		assert!(cursor.check(deadline));
	}
}
