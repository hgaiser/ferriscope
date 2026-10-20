//! The focus pass: gamescope's `determine_and_apply_focus`, applied.
//!
//! [`crate::focus::pick`] decides, from the metadata cache alone, which window
//! is the base layer, the override, the overlay and the notification and where
//! the keyboard and the X input focus go. This module diffs that decision
//! against what the previous pass applied ([`AppliedFocus`]) and applies only
//! the difference. Event handlers never apply focus: they update the cache and
//! call [`FerriscopeState::mark_focus_dirty`], exactly as gamescope's handlers
//! call `MakeFocusDirty`, and the pass runs only while the serial is dirty
//! (`steamcompmgr.cpp:5030`).
//!
//! The pass reads no X property: everything it needs is in
//! [`crate::shell::windows::WindowTable`]. Its X writes are `XSetInputFocus`,
//! the restack of the input window, the Steam UI resize and the root-window
//! contract.

use std::collections::HashSet;

use smithay::{
	desktop::Window,
	reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
	utils::{IsAlive, Logical, Point, Rectangle, SERIAL_COUNTER, Size},
	wayland::shell::xdg::ToplevelSurface,
	xwayland::X11Surface,
};

use crate::{
	focus::{FocusGeometry, FocusStrategy, Picked, PreviousFocus, WindowId, focus_geometry},
	overlay::IcccmState,
	steam::{FocusCandidate, FocusedApp, focusable_apps, focusable_windows},
};

use crate::shell::FerriscopeState;
use crate::shell::OverlayLayer;
use crate::shell::OverlayWindow;
use crate::shell::geometry::root_surface;
use crate::shell::geometry::window_wl_surface;
use crate::shell::handlers::seat::FocusTarget;

/// What the last pass left behind, so the next one applies only a difference.
#[derive(Default)]
pub(in crate::shell) struct AppliedFocus {
	/// The ids the last pass decided (gamescope's `ctx->focus` / `pFocus`).
	picked: Picked,
	/// The presented base layer, the only window drawn fit to the output.
	base: Option<Window>,
	/// The window that carries the fullscreen state.
	role: Option<Window>,
	/// The window the X input focus was last moved to.
	x_keyboard: Option<WindowId>,
	/// The seat keyboard's target and the window behind it.
	keyboard: Option<(FocusTarget, Window)>,
	/// The window last raised in X stacking.
	raised: Option<WindowId>,
	/// What the Steam backchannel last carried.
	published: Option<PublishedFocus>,
	/// X windows whose `WM_CHANGE_STATE` iconify request was acknowledged with
	/// `WM_STATE = IconicState` and that have not been taken out of it since.
	iconified: HashSet<WindowId>,
}

/// The `--steam` contract as last written to the root window.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PublishedFocus {
	app: FocusedApp,
	window: u32,
	apps: Vec<u32>,
	windows: Vec<u32>,
}

impl AppliedFocus {
	/// Remember that `id` was acknowledged as iconic.
	pub fn mark_iconified(&mut self, id: WindowId) {
		self.iconified.insert(id);
	}

	/// Drop the iconic record of a window that is gone.
	pub fn forget_iconified(&mut self, id: WindowId) {
		self.iconified.remove(&id);
	}

	/// The window presented as the base layer (gamescope's `focusWindow`).
	pub fn fullscreen(&self) -> Option<&Window> {
		self.base.as_ref()
	}

	/// The window input goes to (`global.inputFocusWindow`): the interactive
	/// Steam overlay when it holds input, otherwise the focus window.
	pub fn input_focus_id(&self) -> Option<WindowId> {
		self.picked.global.input_focus
	}

	/// The Xwayland-side focus window (`ctx->focus.focusWindow`).
	pub fn ctx_focus(&self) -> Option<WindowId> {
		self.picked.ctx.focus
	}

	/// The window the X input focus belongs to.
	pub fn x_keyboard(&self) -> Option<WindowId> {
		self.x_keyboard
	}

	/// The window holding the seat keyboard (override, interactive overlay or
	/// the base itself).
	pub fn keyboard(&self) -> Option<&Window> {
		self.keyboard.as_ref().map(|(_, window)| window)
	}

	/// The keyboard target applied to the seat.
	pub fn keyboard_target(&self) -> Option<&FocusTarget> {
		self.keyboard.as_ref().map(|(target, _)| target)
	}
}

impl FerriscopeState {
	pub(in crate::shell) fn steam_ui_fills_output_id(&self, id: WindowId) -> bool {
		self.windows.get(id).is_some_and(|managed| managed.meta.is_steam())
	}

	/// Size a Steam UI X11 window to the output rect at the output origin,
	/// mirroring gamescope's `XResizeWindow` of the focused Steam UI
	/// (`steamcompmgr.cpp:4816-4831`, `window_is_fullscreen` includes
	/// `window_is_steam`, `:1921`).
	///
	/// This is deliberately *not* the rule: a Proton game window keeps
	/// its own resolution and is scaled at composite time. Only the Steam UI
	/// (which gamescope hard-codes fullscreen) is resized, and never via
	/// `set_fullscreen`, so no `_NET_WM_STATE_FULLSCREEN` is written, matching
	/// gamescope's bare `XResizeWindow`. A no-op when it already fills the
	/// output at the origin.
	pub(in crate::shell) fn resize_steam_ui_to_output(&self, window: &X11Surface) {
		if window.is_override_redirect() {
			return;
		}
		let target = self.output_rect();
		let current = window.last_configure();
		if current.loc == target.loc && current.size == target.size {
			return;
		}
		tracing::debug!(window = window.window_id(), ?target, "Steam UI sized to the output");
		if let Err(e) = window.configure(target) {
			tracing::debug!("Steam UI resize failed: {e:?}");
		}
	}

	pub(in crate::shell) fn configure_fullscreen(&mut self, surface: &ToplevelSurface) {
		// The role holder is told the output size whether or not it ever calls
		// `set_fullscreen`, so a Wayland toplevel that maps below the output is
		// sized to fill it.
		surface.with_pending_state(|state| {
			state.states.set(xdg_toplevel::State::Fullscreen);
			state.states.set(xdg_toplevel::State::Activated);
			state.size = Some(self.output_rect().size);
		});
		surface.send_configure();
	}

	/// `MakeFocusDirty()` (`steamcompmgr.cpp:992`).
	pub(in crate::shell) fn mark_focus_dirty(&mut self) {
		self.focus_dirty.mark();
	}

	/// Whether a focus pass is due (gamescope's `focus_t::IsDirty`).
	pub(in crate::shell) fn focus_dirty(&self) -> bool {
		self.focus_dirty.is_dirty()
	}

	/// The single focus pass, gamescope's `determine_and_apply_focus`
	/// (`steamcompmgr.cpp:5005`).
	///
	/// 1. [`crate::focus::pick`] recomputes the whole focus from the cache;
	/// 2. the layers follow it: the overlay and notification, and the space,
	///    which holds exactly the base layer and its override;
	/// 3. the fullscreen state, X input focus and seat keyboard move if their
	///    target changed;
	/// 4. the focus-window geometry rule is re-applied (`:4816`), so a drifted
	///    Steam window is corrected on the next dirty pass;
	/// 5. the input window is raised in X stacking (`:4784`);
	/// 6. the `--steam` contract is republished if it changed.
	///
	/// The serial is read before the pass and recorded after it, so a change made
	/// during the pass (a configure round-trip) leaves the next pass due rather
	/// than being swallowed.
	pub(in crate::shell) fn determine_and_apply_focus(&mut self) {
		if !self.focus_dirty() {
			return;
		}
		let serial = self.focus_dirty.serial();
		let picked = self.pick_focus();
		self.apply_layers(&picked);
		self.apply_role(&picked);
		self.apply_x_focus(&picked);
		self.apply_keyboard_focus(&picked);
		self.apply_focus_geometry(&picked);
		self.apply_input_stacking(&picked);
		self.publish_steam_focus(&picked);
		self.focus.picked = picked;
		self.focus_dirty.applied(serial);
	}

	/// Run the pure selection over the cache.
	fn pick_focus(&self) -> Picked {
		let x11 = self.windows.x11_metas();
		let xdg = self.windows.xdg_metas();
		// `steam` is the one switch: Steam mode is Steam-controlled focus.
		let strategy = if self.config.steam {
			FocusStrategy::SteamControlled
		} else {
			FocusStrategy::SingleApplication
		};
		crate::focus::pick(
			&x11,
			&xdg,
			self.output_rect().size.w,
			&self.focus_control,
			strategy,
			PreviousFocus {
				ctx_focus: self.focus.picked.ctx.focus,
				global_focus: self.focus.picked.global.focus,
			},
		)
	}

	/// The scene origin of a tracked window: the X11 geometry from the cache, a
	/// Wayland toplevel's own geometry origin.
	fn window_location(&self, id: WindowId) -> Option<Point<i32, Logical>> {
		let managed = self.windows.get(id)?;
		Some(if managed.window.x11_surface().is_some() {
			Point::from((managed.meta.geometry.x, managed.meta.geometry.y))
		} else {
			Self::window_map_origin(&managed.window)
		})
	}

	/// Follow the pick with the composited layers.
	///
	/// gamescope draws the base window, its override, the overlay and the
	/// notification and nothing else, so `space` holds exactly the base and the
	/// override (the override above the base), and `overlays` the overlay and
	/// notification. Everything else stays tracked but is neither drawn nor
	/// hit-tested.
	fn apply_layers(&mut self, picked: &Picked) {
		let mut overlays = Vec::new();
		for (id, layer) in [
			(picked.global.notification, OverlayLayer::Notification),
			(picked.global.overlay, OverlayLayer::Interactive),
		] {
			let Some(id) = id else {
				continue;
			};
			let (Some(window), Some(location)) = (self.windows.window(id), self.window_location(id)) else {
				continue;
			};
			overlays.push(OverlayWindow {
				id,
				window: window.clone(),
				role: layer,
				location,
			});
		}
		self.overlays = overlays;

		let mut desired: Vec<(Window, Point<i32, Logical>)> = Vec::new();
		for id in [picked.global.focus, picked.global.override_window]
			.into_iter()
			.flatten()
		{
			if let (Some(window), Some(location)) = (self.windows.window(id), self.window_location(id)) {
				if !desired.iter().any(|(existing, _)| existing == window) {
					desired.push((window.clone(), location));
				}
			}
		}
		let current: Vec<Window> = self.space.elements().cloned().collect();
		let unchanged = current.len() == desired.len()
			&& current.iter().zip(&desired).all(|(window, (wanted, location))| {
				window == wanted && self.space.element_location(window) == Some(*location)
			});
		if !unchanged {
			// Re-map in order (the override above the base) without `activate`: a
			// space activation would set `_NET_WM_STATE_FOCUSED` on the override.
			for window in &current {
				self.space.unmap_elem(window);
			}
			for (window, location) in desired {
				self.space.map_element(window, location, false);
			}
		}
		self.focus.base = picked.global.focus.and_then(|id| self.windows.window(id)).cloned();
	}

	/// Move the fullscreen state to the window that should carry it.
	///
	/// A dialog that took the focus window keeps its parent fullscreen: the state
	/// belongs to the window picked *before* transient links were followed.
	fn apply_role(&mut self, picked: &Picked) {
		let desired = picked.global.role.and_then(|id| self.windows.window(id)).cloned();
		if desired == self.focus.role {
			return;
		}
		if let Some(previous) = self.focus.role.take() {
			self.leave_role(&previous);
		}
		if let Some(window) = desired {
			self.enter_role(&window);
			self.focus.role = Some(window);
		}
	}

	/// Fullscreen `window`, the **application** half of the role.
	///
	/// An X11 window's `_NET_WM_STATE` is left to the client, as gamescope
	/// does (it only records the client's requests, `handle_net_wm_state`,
	/// `steamcompmgr.cpp:6382`). Proton under `steamcompmgr` never asks for
	/// fullscreen and waits for the window manager to apply exactly the
	/// `_NET_WM_STATE` it asked for; a `_NET_WM_STATE_FULLSCREEN` or
	/// `_NET_WM_STATE_FOCUSED` added here kept that update pending, and Wine
	/// will not restore a minimized window while one is: a Proton game drew
	/// black after the Steam overlay.
	fn enter_role(&mut self, window: &Window) {
		if window.x11_surface().is_none()
			&& let Some(toplevel) = window.toplevel()
		{
			self.configure_fullscreen(toplevel);
		}
	}

	/// A window lost the role: it stays mapped but is no longer fullscreen or
	/// active. The fullscreen state is cleared so only the holder carries it.
	fn leave_role(&mut self, window: &Window) {
		if !window.alive() {
			return;
		}
		if window.x11_surface().is_some() {
			// `_NET_WM_STATE` is the client's (see `enter_role`).
		} else if let Some(toplevel) = window.toplevel() {
			toplevel.with_pending_state(|state| {
				state.states.unset(xdg_toplevel::State::Fullscreen);
				state.states.unset(xdg_toplevel::State::Activated);
			});
			toplevel.send_configure();
		}
	}

	/// Move the X input focus (`XSetInputFocus`, `steamcompmgr.cpp:4693`) and
	/// `_NET_WM_STATE_FOCUSED` to the Xwayland keyboard window.
	///
	/// `_NET_ACTIVE_WINDOW` follows from the `FocusIn` the move produces. An
	/// interactive overlay gets the input focus but not `_NET_WM_STATE_FOCUSED`,
	/// matching gamescope, which never writes that state.
	fn apply_x_focus(&mut self, picked: &Picked) {
		let desired = picked.ctx.keyboard_focus;
		if desired == self.focus.x_keyboard {
			return;
		}
		self.focus.x_keyboard = desired;
		let Some(id) = desired else {
			return;
		};
		self.take_out_of_iconic(id);
		if self.windows.get(id).is_none() {
			return;
		}
		// gamescope moves the X input focus on every focus transition instead of
		// trusting the client's ICCCM input model. See `force_x11_input_focus`.
		self.force_x11_input_focus(id);
	}

	/// A window taking the keyboard is taken out of iconic: Wine iconifies a
	/// fullscreen game that loses the foreground (the Steam overlay) and
	/// restores it only when `WM_STATE` goes back to `NormalState`, as a window
	/// manager does on activation. gamescope does this when the focus window
	/// changes (`steamcompmgr.cpp:4591`); the overlay keeps the focus window, so
	/// the keyboard moving back is the transition ferriscope keys it on.
	fn take_out_of_iconic(&mut self, id: WindowId) {
		if self.focus.iconified.remove(&id) {
			if let Some(steam) = self.steam.as_ref() {
				steam.set_wm_state(id, IcccmState::Normal);
			}
		}
	}

	/// The seat keyboard target for a window.
	///
	/// The Xwayland keyboard window is focused as an [`X11Surface`] so smithay's
	/// ICCCM handling runs (`WM_TAKE_FOCUS`). Any other X11 window — an override
	/// the keyboard follows while X input focus stays on the base — is focused by
	/// its plain `wl_surface`, so smithay does not move the X input focus to it.
	fn keyboard_target(&self, id: WindowId, picked: &Picked) -> Option<(FocusTarget, Window)> {
		let window = self.windows.window(id)?.clone();
		let target = match window.x11_surface() {
			Some(surface) if picked.ctx.keyboard_focus == Some(id) => FocusTarget::X11(surface.clone()),
			Some(surface) => FocusTarget::Wayland(surface.wl_surface()?),
			None => FocusTarget::Wayland(window.toplevel()?.wl_surface().clone()),
		};
		Some((target, window))
	}

	/// Move the Wayland keyboard to the picked keyboard window
	/// (`wlserver_keyboardfocus`, `steamcompmgr.cpp:5229`).
	fn apply_keyboard_focus(&mut self, picked: &Picked) {
		let desired = match picked.global.keyboard_focus {
			Some(id) => match self.keyboard_target(id, picked) {
				Some(target) => Some(target),
				// Not bound to a `wl_surface` yet; the association re-runs the pass.
				None => return,
			},
			None => None,
		};
		if desired.as_ref().map(|(target, _)| target) == self.focus.keyboard.as_ref().map(|(target, _)| target) {
			return;
		}
		if let Some(id) = picked.global.keyboard_focus {
			self.take_out_of_iconic(id);
		}
		if let Some((_, previous)) = self.focus.keyboard.take() {
			if let Some(toplevel) = previous.toplevel().filter(|_| previous.alive()) {
				toplevel.with_pending_state(|state| {
					state.states.unset(xdg_toplevel::State::Activated);
				});
				toplevel.send_configure();
			}
		}
		match desired {
			Some((target, window)) => {
				if let Some(toplevel) = window.toplevel() {
					toplevel.with_pending_state(|state| {
						state.states.set(xdg_toplevel::State::Activated);
					});
					toplevel.send_configure();
				}
				self.set_keyboard_focus(Some(target.clone()));
				self.focus.keyboard = Some((target, window));
			},
			None => self.set_keyboard_focus(None),
		}
	}

	/// Re-apply gamescope's focus-window geometry rule
	/// (`steamcompmgr.cpp:4816-4841`), plus the Steam UI when it is the
	/// interactive overlay holding input.
	fn apply_focus_geometry(&mut self, picked: &Picked) {
		if let Some(id) = picked.ctx.focus {
			self.apply_geometry_to(id);
		}
		// An interactive Steam UI overlay holds input without being the focus
		// window; size it to the output too.
		if let (Some(overlay), Some(input)) = (picked.global.overlay, picked.global.input_focus) {
			if overlay == input && self.steam_ui_fills_output_id(overlay) {
				self.apply_geometry_to(overlay);
			}
		}
	}

	/// Apply the geometry branch to one window, from the cache.
	fn apply_geometry_to(&mut self, id: WindowId) {
		let Some(managed) = self.windows.get(id) else {
			return;
		};
		let Some(surface) = managed.window.x11_surface().cloned() else {
			return;
		};
		let meta = &managed.meta;
		let requested = meta.size_hints_specified.then_some(meta.requested_size);
		let is_steam = meta.is_steam();
		let is_overlay = meta.is_overlay;
		match focus_geometry(
			meta.has_game_id(),
			meta.is_fullscreen_window(),
			false,
			requested.is_some(),
		) {
			// gamescope's XResizeWindow(focusWindow, root). Scoped to the Steam
			// UI: a non-Steam game keeps its own resolution.
			FocusGeometry::FillOutput => {
				if is_steam {
					self.resize_steam_ui_to_output(&surface);
				}
			},
			// The windowed requested-size branch (`:4839`): restore the size the
			// client's `WM_NORMAL_HINTS` insist on. Never an overlay: an overlay
			// is not the focus window, and shrinking it would leave it composited
			// scaled-down.
			FocusGeometry::Requested => {
				if is_overlay {
					return;
				}
				if let Some((w, h)) = requested {
					self.resize_x11_to(&surface, Size::from((w as i32, h as i32)));
				}
			},
			FocusGeometry::Keep => {},
		}
	}

	/// Resize an X11 window to `size`, preserving its origin, skipping a
	/// redundant configure. Mirrors gamescope's `XResizeWindow` (size only, not
	/// a move), so re-running the pass cannot ping-pong a window between its
	/// requested size and a configure round-trip.
	pub(in crate::shell) fn resize_x11_to(&self, surface: &X11Surface, size: Size<i32, Logical>) {
		if surface.is_override_redirect() {
			return;
		}
		let current = surface.last_configure();
		if current.size == size {
			return;
		}
		let target = Rectangle::new(current.loc, size);
		if let Err(e) = surface.configure(target) {
			tracing::debug!("window resize failed: {e:?}");
		}
	}

	/// Keep the window that should receive input on top of Xwayland's own X
	/// stacking (`inputFocus->Raise()`, `steamcompmgr.cpp:4789`). Xwayland routes
	/// pointer motion/buttons by the X window under its X pointer (`QueryTree`
	/// stacking), **not** by the Wayland pointer focus resolved by
	/// [`Self::pointer_under`]. Big Picture maps before the game launches, so the
	/// game's XWM frame ends up above the overlay's and Xwayland delivers motion
	/// and clicks to the game — which sits under the overlay and ignores them —
	/// even though ferriscope focuses the overlay. Keyboard reaches the overlay
	/// because the X input focus is set directly, which is why only the pointer
	/// would appear broken.
	///
	/// Only a change of target restacks, so a client-raised menu above the base is
	/// not yanked down on every pass.
	fn apply_input_stacking(&mut self, picked: &Picked) {
		let target = picked.ctx.raise;
		if target == self.focus.raised {
			return;
		}
		self.focus.raised = target;
		let Some(id) = target else {
			return;
		};
		if self.windows.top_x11() == Some(id) {
			return;
		}
		let Some(surface) = self.windows.window(id).and_then(|window| window.x11_surface()).cloned() else {
			return;
		};
		let Some(xwm) = self.xwm.as_mut() else {
			return;
		};
		match xwm.raise_window(&surface) {
			Ok(()) => {
				// The XWM reports no ConfigureNotify for its own restack, so
				// record it here; otherwise the stale stack still has the
				// previous window on top and the raise back is skipped.
				self.windows.raise_to_top(id);
				tracing::debug!(window = id, "raised the input-focus window in X stacking");
			},
			Err(e) => tracing::debug!(window = id, "input-focus raise failed: {e:?}"),
		}
	}

	/// Publish the focused-window contract to Steam (`steamcompmgr.cpp:5376-5395`).
	///
	/// A no-op unless `--steam` is on, and a no-op when nothing changed since the
	/// last write. `GAMESCOPE_FOCUSED_APP` is the input-focus window's app id (the
	/// Steam UI's, 769, while an interactive overlay holds input — the reference
	/// commit's `set_focused_app_split`), `_GFX` the base window's, and
	/// `GAMESCOPE_FOCUSED_WINDOW` the base window's X11 id. A zero id deletes the
	/// property, matching gamescope.
	fn publish_steam_focus(&mut self, picked: &Picked) {
		if !self.config.steam {
			return;
		}
		let Some(steam) = self.steam.as_ref() else {
			return;
		};
		let meta = |id: Option<WindowId>| id.and_then(|id| self.windows.get(id)).map(|managed| &managed.meta);
		let base = meta(picked.global.focus);
		let input = meta(picked.global.input_focus);
		let base_window = base
			.filter(|meta| meta.kind == crate::focus::WindowKind::Xwayland)
			.map(|meta| meta.id)
			.unwrap_or(0);
		let owner_is_overlay = input.is_some_and(|meta| meta.is_overlay);
		// gamescope derives an app id for every window from its process, so a
		// dialog of a game has the game's. ferriscope defers that derivation, so a
		// base window without an id of its own (a dialog that took the focus
		// window) reports the id of the window it belongs to.
		let base_app_id = base
			.map(|meta| meta.app_id)
			.filter(|id| *id != 0)
			.or_else(|| meta(picked.global.role).map(|meta| meta.app_id))
			.unwrap_or(0);
		let app = FocusedApp::for_owner(
			input.map(|meta| meta.app_id).unwrap_or(0),
			owner_is_overlay,
			base_app_id,
		);
		let candidates: Vec<FocusCandidate> = picked
			.focusable
			.iter()
			.filter_map(|id| self.windows.get(*id))
			.map(|managed| FocusCandidate {
				window_id: managed.meta.id,
				app_id: managed.meta.app_id,
				pid: managed.meta.pid,
			})
			.collect();
		let published = PublishedFocus {
			app,
			window: base_window,
			apps: focusable_apps(&candidates, owner_is_overlay),
			windows: focusable_windows(&candidates, owner_is_overlay),
		};
		if self.focus.published.as_ref() == Some(&published) {
			return;
		}
		steam.set_focused_app(published.app, published.window);
		steam.set_focusable(&published.apps, &published.windows);
		self.focus.published = Some(published);
	}

	/// Move keyboard focus, or clear it, in one place.
	pub(in crate::shell) fn set_keyboard_focus(&mut self, focus: Option<FocusTarget>) {
		let serial = SERIAL_COUNTER.next_serial();
		if let Some(keyboard) = self.seat.get_keyboard() {
			keyboard.set_focus(self, focus, serial);
		}
	}

	/// Force the X input focus to a window, the way gamescope does.
	///
	/// gamescope calls `XSetInputFocus` unconditionally for its focus window
	/// (`steamcompmgr.cpp:4693`), ignoring the client's ICCCM input model.
	/// Smithay's `X11Surface::enter` only sends `WM_TAKE_FOCUS` for a
	/// `GloballyActive` client (`WM_HINTS.input = False`, e.g. a Proton/Wine
	/// game) and never moves the X input focus, so ferriscope's seat keyboard
	/// target and the X input focus diverge: the seat sends keys to the game's
	/// Xwayland surface, but Xwayland delivers them to the still-focused
	/// previous window (the Steam UI), so e.g. ESC goes to Steam while the
	/// focused-app contract already names the game. The resulting `FocusIn`
	/// drives `_NET_ACTIVE_WINDOW` through the xwm.
	pub(in crate::shell) fn force_x11_input_focus(&self, id: WindowId) {
		if let Some(steam) = self.steam.as_ref() {
			if self
				.windows
				.window(id)
				.is_some_and(|window| window.x11_surface().is_some())
			{
				steam.set_input_focus(id);
			}
		}
	}

	/// Drain the X connection: correct a stolen X input focus and pick up
	/// Steam's focus control.
	///
	/// The `FocusOut` half mirrors gamescope's handler
	/// (`steamcompmgr.cpp:8602-8638`): it watches `FocusChange` on its current
	/// keyboard-focus window and, when the focus escapes to a different toplevel,
	/// sets it back. Without this, an X client that re-focuses its own subwindow
	/// after the compositor focuses the game (Steam's CEF does exactly this)
	/// leaves the seat keyboard target and the X input focus diverged: the seat
	/// sends keys to the game's Xwayland surface, but Xwayland delivers them to
	/// the still-focused Steam window, so ESC reaches Steam instead of the game.
	/// A focus that moved inside the same toplevel (a CEF dropdown) is left alone.
	///
	/// The `QueryTree` / `GetInputFocus` round trips here run only on a
	/// `FocusOut`, never on the input path.
	pub(in crate::shell) fn poll_x11_events(&mut self) {
		let Some(steam) = self.steam.as_ref() else {
			return;
		};
		let events = steam.poll_events();
		let control = events.control_changed.then(|| steam.read_focus_control());
		let fps_limit = events.fps_limit_changed.then(|| steam.read_fps_limit());
		let keyboard_layout = events.keyboard_layout_changed.then(|| steam.read_keyboard_layout());
		if let Some(control) = control {
			self.focus_control = control;
			self.mark_focus_dirty();
		}
		if let Some(fps_limit) = fps_limit {
			self.fps_limit = fps_limit;
			tracing::info!(fps_limit, "GAMESCOPE_FPS_LIMIT");
		}
		if let Some(layout) = keyboard_layout {
			self.steam_keyboard_layout = layout;
			self.apply_keyboard_layout();
		}
		if events.sdr_brightness_changed {
			if let Some(nits) = self.steam.as_ref().map(|steam| steam.read_sdr_on_hdr_brightness()) {
				if nits != self.sdr_white_nits {
					self.sdr_white_nits = nits;
					// Every SDR element is drawn differently now, though none of
					// them changed, so the damage tracker cannot see it.
					for slot in 0..self.pool.slots.len() {
						self.damage.ages.invalidate(slot);
					}
					self.scheduler.commit(std::time::Instant::now());
				}
			}
		}
		if events.touch_click_mode_changed {
			if let Some(mode) = self.steam.as_ref().map(|steam| steam.read_touch_click_mode()) {
				self.set_touch_click_mode(mode);
			}
		}
		let Some(steam) = self.steam.as_ref() else {
			return;
		};
		if !events.focus_out {
			return;
		}
		let Some(owner) = self.focus.x_keyboard else {
			return;
		};
		let within = steam
			.input_focus()
			.is_some_and(|focus| focus == owner || steam.focus_is_within(focus, owner));
		if !within {
			steam.set_input_focus(owner);
		}
	}

	/// Rebuild the seat keymap from the configured base and Steam's layout
	/// (`wlserver_update_keymap`, `wlserver.cpp:2108`). A keymap that fails to
	/// compile leaves the current one in place, as gamescope does.
	/// The seat keyboard's configuration: the configured rule names with Steam's
	/// `GAMESCOPE_KEYBOARD_LAYOUT` applied.
	pub(in crate::shell) fn keyboard_config(&self) -> crate::keyboard::KeyboardConfig {
		self.config.seat.keyboard.with_steam_layout(&self.steam_keyboard_layout)
	}

	fn apply_keyboard_layout(&mut self) {
		// The IME looks keys up on the seat keymap, so recompile it on next use.
		self.ime.invalidate_default_keymap();
		let Some(keyboard) = self.seat.get_keyboard() else {
			return;
		};
		let config = self.keyboard_config();
		if let Err(e) = keyboard.set_xkb_config(self, config.xkb_config()) {
			tracing::error!(layout = %config.layout, variant = %config.variant, "keymap failed to compile ({e:?}); keeping the current layout");
		}
	}

	/// Drop windows whose client went away without an unmap callback (a native
	/// Wayland toplevel destroyed by its client) and re-run the pass, so focus
	/// cannot dangle on a dead surface.
	pub(in crate::shell) fn clear_dead_focus(&mut self) {
		if self.windows.prune_dead() {
			self.mark_focus_dirty();
		}
		if self.focus.keyboard.as_ref().is_some_and(|(_, window)| !window.alive())
			|| self.focus.base.as_ref().is_some_and(|window| !window.alive())
		{
			self.mark_focus_dirty();
		}
	}

	/// Whether `surface` (or a subsurface of it) is the presented base layer.
	pub(in crate::shell) fn surface_is_fullscreen_window(
		&self,
		surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
	) -> bool {
		match self.focus.fullscreen() {
			Some(window) => window_wl_surface(window).as_ref() == Some(&root_surface(surface)),
			None => false,
		}
	}
}
