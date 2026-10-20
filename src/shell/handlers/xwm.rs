//! Xwayland startup and the X11 window-manager handlers.

use std::process::Stdio;

use smithay::{
	reexports::{
		calloop::{Interest, Mode, PostAction, generic::Generic},
		wayland_server::protocol::wl_surface::WlSurface,
	},
	utils::{Logical, Rectangle},
	wayland::{
		xwayland_keyboard_grab::XWaylandKeyboardGrabHandler,
		xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
	},
	xwayland::{
		X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
		xwm::{Reorder, ResizeEdge, WmWindowProperty, X11Window},
	},
};

use crate::{
	error::{Error, Result},
	overlay::{IcccmState, SteamAtoms},
};

use crate::shell::FerriscopeState;
use crate::shell::geometry::window_wl_surface;
use crate::shell::handlers::seat::FocusTarget;

impl FerriscopeState {
	pub(in crate::shell) fn start_xwayland(&mut self) -> Result<()> {
		if !self.config.xwayland {
			return Ok(());
		}

		let (xwayland, client) = XWayland::spawn(
			&self.display_handle,
			None,
			// smithay clears Xwayland's environment except PATH and
			// XDG_RUNTIME_DIR, which drops the variables that let it find a GPU
			// driver installed outside the default paths (LD_LIBRARY_PATH,
			// __EGL_VENDOR_LIBRARY_DIRS, ...); without them it has no hardware
			// GLX. Hand back the compositor's own environment. These are applied
			// after smithay's WAYLAND_SOCKET, so an inherited one must not
			// replace it.
			std::env::vars_os().filter(|(key, _)| key != "WAYLAND_SOCKET"),
			// gamescope sets `force_xrandr_emulation` (`wlserver.cpp:1878`), so a
			// client can pick an emulated mode and is scaled to the output. It
			// also turns touch pointer emulation off (`g_bNoTouchPointerEmulation`,
			// `main.cpp:323`): touches reach X clients as XI2 touch events, and a
			// click-mode touch is already a pointer event.
			["-force-xrandr-emulation", "-noTouchPointerEmulation"],
			true,
			Stdio::null(),
			Stdio::null(),
			|_| (),
		)
		.map_err(|e| Error::Xwayland(format!("spawn: {e:?}")))?;

		let display_handle = self.display_handle.clone();
		let loop_handle = self.loop_handle.clone();
		self.loop_handle
			.insert_source(xwayland, move |event, _, state: &mut Self| match event {
				XWaylandEvent::Ready {
					x11_socket,
					display_number,
				} => match X11Wm::start_wm(loop_handle.clone(), &display_handle, x11_socket, client.clone()) {
					Ok(wm) => {
						tracing::info!(display_number, "Xwayland ready");
						state.xwm = Some(wm);
						// The Steam atoms, Wine styles and Steam's focus control are
						// not exposed by the pinned Smithay from the xwm's own
						// connection; read them over a separate one.
						match SteamAtoms::connect(display_number) {
							Ok(atoms) => {
								// Watch X focus escapes on the classification
								// connection: gamescope corrects a focus stolen
								// out of the keyboard-focus window from its
								// `FocusOut` handler (`steamcompmgr.cpp:8602`).
								if let Some(fd) = atoms.duplicate_fd()
									&& let Err(e) = loop_handle.insert_source(
										Generic::new(fd, Interest::READ, Mode::Level),
										|_, _, state: &mut Self| {
											state.poll_x11_events();
											Ok(PostAction::Continue)
										},
									) {
									tracing::warn!("insert X focus source: {e:?}");
								}
								state.focus_control = atoms.read_focus_control();
								// The WSI layer exposes HDR swapchain formats only when
								// the server says the output can show HDR.
								atoms.set_hdr_output_feedback(state.hdr);
								let server_id = crate::steam::display_id_to_server_id(display_number);
								state.xwayland_server_id = Some(server_id);
								state.swapchain_factories.announce_xwayland_server(server_id);
								state.sdr_white_nits = atoms.read_sdr_on_hdr_brightness();
								state.steam = Some(atoms);
							},
							Err(e) => tracing::warn!(
								display_number,
								"Steam atom connection failed; Steam overlay and app-id focus disabled: {e}"
							),
						}
						// Signal ready only once the WM exists, so a `Ready`
						// always means the X display is usable. The X display
						// number only exists here, so it is carried on this path.
						state.signal_ready(Some(display_number));
						if let Some(argv) = state.config.command.clone()
							&& let Err(e) = state.spawn_app(&argv, Some(display_number))
						{
							tracing::error!("failed to spawn hosted application: {e}");
						}
					},
					Err(e) => {
						// Do not send a misleading `Ready`; a consumer waiting on
						// the handshake must not be told Xwayland is up.
						tracing::error!("failed to start X11 window manager, stopping: {e:?}");
						state.request_stop();
					},
				},
				XWaylandEvent::Error => {
					tracing::error!("Xwayland failed on startup, stopping");
					state.request_stop();
				},
			})
			.map_err(|e| Error::Xwayland(format!("insert source: {e:?}")))?;

		Ok(())
	}
}

impl XWaylandShellHandler for FerriscopeState {
	fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
		&mut self.xwayland_shell_state
	}

	fn surface_associated(&mut self, _xwm: smithay::xwayland::xwm::XwmId, wl_surface: WlSurface, surface: X11Surface) {
		tracing::debug!(
			window = surface.window_id(),
			surface = smithay::reexports::wayland_server::Resource::id(&wl_surface).protocol_id(),
			"x11 surface associated"
		);
		// The X11 window is only viewable once its `wl_surface` is associated; an
		// early `XSetInputFocus` can be a no-op (`BadMatch`), so re-assert it here.
		// gamescope does the same when a window's surface id arrives late
		// (`handle_wl_surface_id`, `steamcompmgr.cpp:6316`). The pass also retries
		// a Wayland keyboard focus that could not be bound before.
		let id = surface.window_id();
		if self.focus.x_keyboard() == Some(id) {
			self.force_x11_input_focus(id);
		}
		self.mark_focus_dirty();
	}
}

impl XWaylandKeyboardGrabHandler for FerriscopeState {
	fn keyboard_focus_for_xsurface(&self, surface: &WlSurface) -> Option<Self::KeyboardFocus> {
		// Only a window being drawn (the focus window and its override) can take
		// the keyboard through an X keyboard grab; a hidden window cannot.
		self.space
			.elements()
			.find(|w| window_wl_surface(w).as_ref() == Some(surface))
			.and_then(|window| window.x11_surface().cloned().map(FocusTarget::X11))
	}
}

impl XwmHandler for FerriscopeState {
	fn xwm_state(&mut self, _xwm: smithay::xwayland::xwm::XwmId) -> &mut X11Wm {
		self.xwm.as_mut().expect("xwm callback without a window manager")
	}

	fn new_window(&mut self, _xwm: smithay::xwayland::xwm::XwmId, window: X11Surface) {
		tracing::debug!(window = window.window_id(), geometry = ?window.geometry(), "x11 new window");
	}

	fn new_override_redirect_window(&mut self, _xwm: smithay::xwayland::xwm::XwmId, window: X11Surface) {
		tracing::debug!(window = window.window_id(), geometry = ?window.geometry(), "x11 new override-redirect window");
	}

	fn map_window_request(&mut self, _xwm: smithay::xwayland::xwm::XwmId, window: X11Surface) {
		tracing::debug!(
			window = window.window_id(),
			title = %window.title(),
			class = %window.class(),
			geometry = ?window.geometry(),
			"x11 map request"
		);
		if let Err(e) = window.set_mapped(true) {
			tracing::warn!("x11 set_mapped: {e:?}");
		}
		// The Steam UI is gamescope's force-fullscreen window: size it to the
		// output as it maps, even when it maps behind a game, so it covers the
		// output when it comes forward. The pass keeps it there.
		let is_steam_ui = self.track_x11_window(window.clone());
		if is_steam_ui {
			self.resize_steam_ui_to_output(&window);
		}
	}

	fn mapped_override_redirect_window(&mut self, _xwm: smithay::xwayland::xwm::XwmId, window: X11Surface) {
		tracing::debug!(
			window = window.window_id(),
			title = %window.title(),
			geometry = ?window.geometry(),
			"x11 override-redirect mapped"
		);
		// Override-redirect windows (menus, tooltips) are tracked like any other;
		// whether one is the override (drawn over the base, owning the keyboard on
		// the Wayland side) is decided by the pass.
		self.track_x11_window(window);
	}

	fn unmapped_window(&mut self, _xwm: smithay::xwayland::xwm::XwmId, window: X11Surface) {
		tracing::debug!(window = window.window_id(), title = %window.title(), "x11 unmapped");
		// Dropping the entry is the whole teardown: the next pass hands focus to
		// the best remaining window, or clears it, so the focused-app contract
		// never names a dead window (gamescope's `unmap_win`, `:5800`).
		self.forget_window(window.window_id());
		if !window.is_override_redirect() {
			let _ = window.set_mapped(false);
		}
	}

	fn destroyed_window(&mut self, _xwm: smithay::xwayland::xwm::XwmId, window: X11Surface) {
		tracing::debug!(window = window.window_id(), title = %window.title(), "x11 destroyed");
		self.forget_window_overrides(&window);
		self.forget_window(window.window_id());
	}

	fn configure_request(
		&mut self,
		_xwm: smithay::xwayland::xwm::XwmId,
		window: X11Surface,
		x: Option<i32>,
		y: Option<i32>,
		w: Option<u32>,
		h: Option<u32>,
		_reorder: Option<Reorder>,
	) {
		tracing::trace!(window = window.window_id(), ?x, ?y, ?w, ?h, "x11 configure request");
		// The client's requested geometry is honoured for every window: the
		// focus window keeps its own resolution and is scaled to the output at
		// composite time, and overlays and dropdowns size themselves over it.
		let mut geometry = window.geometry();
		if let Some(x) = x {
			geometry.loc.x = x;
		}
		if let Some(y) = y {
			geometry.loc.y = y;
		}
		if let Some(w) = w {
			geometry.size.w = w as i32;
		}
		if let Some(h) = h {
			geometry.size.h = h as i32;
		}
		if let Err(e) = window.configure(geometry) {
			tracing::debug!("overlay configure: {e:?}");
		}
		self.mark_focus_dirty();
	}

	fn active_window_request(
		&mut self,
		_xwm: smithay::xwayland::xwm::XwmId,
		_window: X11Surface,
		_timestamp: u32,
		_currently_active_window: Option<X11Surface>,
	) {
		// `_NET_ACTIVE_WINDOW`: gamescope does not act on activation requests;
		// focus is whatever the pass computes. The request only re-runs the pass,
		// which re-asserts the X input focus if it was lost.
		self.mark_focus_dirty();
	}

	fn demands_attention_request(&mut self, _xwm: smithay::xwayland::xwm::XwmId, window: X11Surface) {
		// There is exactly one fullscreen window and it is always focused, so a
		// demands-attention request is already satisfied; there is no second
		// window to raise.
		tracing::debug!(title = %window.title(), "demands_attention ignored for single-window focus");
	}

	fn undemands_attention_request(&mut self, _xwm: smithay::xwayland::xwm::XwmId, _window: X11Surface) {}

	fn minimize_request(&mut self, _xwm: smithay::xwayland::xwm::XwmId, window: X11Surface) {
		// `WM_CHANGE_STATE` to `IconicState` (`handle_wm_change_state`,
		// `steamcompmgr.cpp:6431`): the window is not unmapped, only `WM_STATE`
		// is faked so the client's own state tracking is satisfied. It is taken
		// out again when the window next takes the keyboard.
		//
		// A window that already holds the X input focus is restored at once,
		// as a window manager restores a window the user brings back. Wine
		// iconifies a game that lost focus to the Steam overlay, and under
		// `steamcompmgr` that request can arrive after the overlay closed and
		// the game got the focus back, so no later focus change would restore
		// it. The request is acknowledged first: Wine waits for the state it
		// asked for and ignores focus until it sees it, so answering with
		// `NormalState` straight away made it ask again, endlessly.
		let id = window.window_id();
		if self.windows.get(id).is_none() {
			return;
		}
		if let Some(steam) = self.steam.as_ref() {
			steam.set_wm_state(id, IcccmState::Iconic);
			if self.focus.x_keyboard() == Some(id) {
				steam.set_wm_state(id, IcccmState::Normal);
				self.focus.forget_iconified(id);
			} else {
				self.focus.mark_iconified(id);
			}
		}
	}

	fn configure_notify(
		&mut self,
		_xwm: smithay::xwayland::xwm::XwmId,
		window: X11Surface,
		geometry: Rectangle<i32, Logical>,
		above: Option<X11Window>,
	) {
		tracing::trace!(window = window.window_id(), ?geometry, "x11 configure notify");
		self.note_x11_configure(&window, geometry, above);
	}

	fn property_notify(&mut self, _xwm: smithay::xwayland::xwm::XwmId, window: X11Surface, property: WmWindowProperty) {
		self.refresh_x11_property(&window, property);
	}

	fn fullscreen_request(&mut self, _xwm: smithay::xwayland::xwm::XwmId, window: X11Surface) {
		tracing::debug!(window = window.window_id(), "x11 fullscreen request");
		self.set_x11_fullscreen_hint(&window, true);
	}

	fn unfullscreen_request(&mut self, _xwm: smithay::xwayland::xwm::XwmId, window: X11Surface) {
		tracing::debug!(window = window.window_id(), "x11 unfullscreen request");
		self.set_x11_fullscreen_hint(&window, false);
	}

	fn resize_request(
		&mut self,
		_xwm: smithay::xwayland::xwm::XwmId,
		_window: X11Surface,
		_button: u32,
		_resize_edge: ResizeEdge,
	) {
	}

	fn move_request(&mut self, _xwm: smithay::xwayland::xwm::XwmId, _window: X11Surface, _button: u32) {}

	fn disconnected(&mut self, _xwm: smithay::xwayland::xwm::XwmId) {
		self.xwm = None;
	}
}
