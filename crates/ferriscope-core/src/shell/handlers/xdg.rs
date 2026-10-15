//! xdg-shell and xdg-activation handlers.

use smithay::{
	desktop::{
		PopupKeyboardGrab, PopupKind, PopupPointerGrab, PopupUngrabStrategy, find_popup_root_surface,
		get_popup_toplevel_coords,
	},
	input::{Seat, pointer::Focus},
	reexports::{
		wayland_protocols::xdg::{
			decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode, shell::server::xdg_toplevel,
		},
		wayland_server::protocol::{wl_output, wl_seat, wl_surface::WlSurface},
	},
	utils::{Rectangle, SERIAL_COUNTER, Serial},
	wayland::{
		shell::xdg::{
			PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
			decoration::XdgDecorationHandler,
		},
		xdg_activation::{XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData},
	},
};

use crate::shell::{FerriscopeState, handlers::seat::FocusTarget};

impl XdgShellHandler for FerriscopeState {
	fn xdg_shell_state(&mut self) -> &mut XdgShellState {
		&mut self.xdg_shell_state
	}

	fn new_toplevel(&mut self, surface: ToplevelSurface) {
		// The toplevel is tracked with its parent (`xdg_toplevel.set_parent`, which
		// can also arrive later, see `parent_changed`); the pass decides whether it
		// is the base layer, a dialog drawn over the base, or a window waiting
		// behind it. Wayland has no Steam atoms, so it has no app id.
		//
		// Only the size is configured here; the pass fullscreens and activates the
		// window that wins, and moves the keyboard.
		let geometry = self.output_rect();
		surface.with_pending_state(|state| {
			state.states.unset(xdg_toplevel::State::Activated);
			state.size = Some(geometry.size);
		});
		surface.send_configure();
		self.track_xdg_window(&surface);
	}

	fn parent_changed(&mut self, surface: ToplevelSurface) {
		// `xdg_toplevel.set_parent` can arrive after `get_toplevel`, so a
		// transient is first tracked without a parent; once it has one, the pass
		// draws it over its parent and gives it the keyboard.
		self.xdg_parent_changed(&surface);
	}

	fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
		// The first configure goes out on the popup's initial commit
		// (`commit_popup`).
		self.unconstrain_popup(&surface);
		if let Err(e) = self.popups.track_popup(PopupKind::Xdg(surface)) {
			tracing::warn!("track popup: {e:?}");
		}
	}

	fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<wl_output::WlOutput>) {
		// Only the single fullscreen window is fullscreened; a transient asking
		// to be fullscreen stays a dialog.
		if surface.parent().is_none() {
			self.configure_fullscreen(&surface);
		}
	}

	fn popup_destroyed(&mut self, _surface: PopupSurface) {
		// A grab ends with its popup, but smithay only releases the keyboard
		// grab on the next key, leaving the keyboard on the destroyed popup.
		// Give it back to the focused window now; a grab that has not ended
		// (a parent popup is still open) ignores this.
		if let Some(keyboard) = self.seat.get_keyboard()
			&& keyboard.is_grabbed()
		{
			let focus = self.focus.keyboard_target().cloned();
			keyboard.set_focus(self, focus, SERIAL_COUNTER.next_serial());
		}
		// Likewise re-pick the pointer focus now rather than on the next motion.
		if let Some(pointer) = self.seat.get_pointer()
			&& pointer.is_grabbed()
		{
			let (under, location) = self.pointer_under(self.cursor.location);
			self.deliver_motion(&pointer, under, location);
			pointer.frame(self);
		}
	}

	/// An explicit grab (menus, dropdowns): the popup takes the keyboard, and a
	/// click outside the popup chain dismisses it.
	fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
		let Some(seat) = Seat::<Self>::from_resource(&seat) else {
			return;
		};
		let kind = PopupKind::Xdg(surface);
		let Ok(root) = find_popup_root_surface(&kind) else {
			return;
		};
		let mut grab = match self.popups.grab_popup(FocusTarget::Wayland(root), kind, &seat, serial) {
			Ok(grab) => grab,
			Err(e) => {
				tracing::debug!("popup grab denied: {e:?}");
				return;
			},
		};
		// A grab must follow the input event that triggered it; one that
		// does not match an existing grab is refused, as in anvil.
		if let Some(keyboard) = seat.get_keyboard() {
			if keyboard.is_grabbed()
				&& !(keyboard.has_grab(serial) || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
			{
				grab.ungrab(PopupUngrabStrategy::All);
				return;
			}
			keyboard.set_focus(self, grab.current_grab(), serial);
			keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
		}
		if let Some(pointer) = seat.get_pointer() {
			if pointer.is_grabbed()
				&& !(pointer.has_grab(serial)
					|| pointer.has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
			{
				grab.ungrab(PopupUngrabStrategy::All);
				return;
			}
			pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
		}
	}

	fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
		surface.with_pending_state(|state| {
			state.geometry = positioner.get_geometry();
			state.positioner = positioner;
		});
		self.unconstrain_popup(&surface);
		surface.send_repositioned(token);
		if let Err(e) = surface.send_configure() {
			tracing::debug!("reposition configure: {e:?}");
		}
	}
}

impl FerriscopeState {
	/// Track a committed popup and send its first configure: a popup
	/// cannot attach a buffer, and so never maps, until it is configured.
	pub(in crate::shell) fn commit_popup(&mut self, surface: &WlSurface) {
		self.popups.commit(surface);
		if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface)
			&& !popup.is_initial_configure_sent()
			&& let Err(e) = popup.send_configure()
		{
			tracing::debug!("initial popup configure: {e:?}");
		}
	}

	/// Keep a popup inside its toplevel. The toplevel fills the output (or is
	/// scaled to), so its geometry is the area the popup may use.
	fn unconstrain_popup(&self, popup: &PopupSurface) {
		let kind = PopupKind::Xdg(popup.clone());
		let Ok(root) = find_popup_root_surface(&kind) else {
			return;
		};
		let Some(window) = self.window_for_wl_surface(&root) else {
			return;
		};
		let mut target = Rectangle::from_size(window.geometry().size);
		target.loc -= get_popup_toplevel_coords(&kind);
		popup.with_pending_state(|state| {
			state.geometry = state.positioner.get_unconstrained_geometry(target);
		});
	}
}

/// Always server-side: ferriscope draws no decorations, so a client that
/// would otherwise draw its own title bar (a nested desktop such as
/// cosmic-comp) fills the output instead.
impl XdgDecorationHandler for FerriscopeState {
	fn new_decoration(&mut self, toplevel: ToplevelSurface) {
		set_server_side(&toplevel);
	}

	fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: Mode) {
		set_server_side(&toplevel);
	}

	fn unset_mode(&mut self, toplevel: ToplevelSurface) {
		set_server_side(&toplevel);
	}
}

fn set_server_side(toplevel: &ToplevelSurface) {
	toplevel.with_pending_state(|state| state.decoration_mode = Some(Mode::ServerSide));
	if toplevel.is_initial_configure_sent() {
		toplevel.send_pending_configure();
	}
}

impl XdgActivationHandler for FerriscopeState {
	fn activation_state(&mut self) -> &mut XdgActivationState {
		&mut self.xdg_activation_state
	}

	fn request_activation(&mut self, _token: XdgActivationToken, _data: XdgActivationTokenData, _surface: WlSurface) {
		// Activation never moves focus: it is whatever the pass computes. The
		// request only re-runs the pass, which re-focuses the window if focus was
		// cleared.
		self.mark_focus_dirty();
	}
}
