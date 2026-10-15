//! Seat handler, keyboard-focus target and data-device handlers.

use std::borrow::Cow;

use smithay::{
	backend::input::{InputTime, KeyState},
	desktop::PopupKind,
	input::{
		Seat, SeatHandler, SeatState,
		dnd::DndGrabHandler,
		keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
		pointer::CursorImageStatus,
	},
	reexports::wayland_server::{Resource, backend::ObjectId, protocol::wl_surface::WlSurface},
	utils::{IsAlive, Serial},
	wayland::{
		compositor::with_states,
		seat::WaylandFocus,
		selection::{
			SelectionHandler,
			data_device::{DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus},
		},
	},
	xwayland::X11Surface,
};

use crate::shell::FerriscopeState;

/// The keyboard-focus target of the single fullscreen window.
///
/// X11 windows are focused as an [`X11Surface`] so smithay's ICCCM handling
/// runs: on `enter` it sets the X input focus and sends `WM_TAKE_FOCUS`
/// according to the window's input model, instead of ferriscope reimplementing
/// that. Wayland windows focus their `wl_surface` directly. Overlays are never
/// a variant: they never take focus.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum FocusTarget {
	/// A Wayland toplevel surface.
	Wayland(WlSurface),
	/// An X11 (Xwayland) window.
	X11(X11Surface),
}

impl FocusTarget {
	/// The `wl_surface` behind the target, if it is bound.
	fn wl_surface(&self) -> Option<WlSurface> {
		match self {
			FocusTarget::Wayland(surface) => Some(surface.clone()),
			FocusTarget::X11(surface) => surface.wl_surface(),
		}
	}
}

/// A popup grab focuses the popup's own surface.
impl From<PopupKind> for FocusTarget {
	fn from(popup: PopupKind) -> Self {
		FocusTarget::Wayland(popup.wl_surface().clone())
	}
}

/// The popup pointer grab moves the pointer to its keyboard targets: the
/// popups and their xdg root, always [`FocusTarget::Wayland`].
impl From<FocusTarget> for WlSurface {
	fn from(target: FocusTarget) -> Self {
		match target {
			FocusTarget::Wayland(surface) => surface,
			FocusTarget::X11(surface) => surface.wl_surface().expect("popup grabs only target Wayland surfaces"),
		}
	}
}

impl IsAlive for FocusTarget {
	fn alive(&self) -> bool {
		match self {
			FocusTarget::Wayland(surface) => surface.alive(),
			FocusTarget::X11(surface) => surface.alive(),
		}
	}
}

impl WaylandFocus for FocusTarget {
	fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
		match self {
			FocusTarget::Wayland(surface) => Some(Cow::Borrowed(surface)),
			FocusTarget::X11(surface) => surface.wl_surface().map(Cow::Owned),
		}
	}

	fn same_client_as(&self, object_id: &ObjectId) -> bool {
		match self {
			FocusTarget::Wayland(surface) => surface.same_client_as(object_id),
			FocusTarget::X11(surface) => surface.same_client_as(object_id),
		}
	}
}

impl KeyboardTarget<FerriscopeState> for FocusTarget {
	fn enter(
		&self,
		seat: &Seat<FerriscopeState>,
		data: &mut FerriscopeState,
		keys: Vec<KeysymHandle<'_>>,
		serial: Serial,
	) {
		match self {
			FocusTarget::Wayland(surface) => KeyboardTarget::enter(surface, seat, data, keys, serial),
			FocusTarget::X11(surface) => KeyboardTarget::enter(surface, seat, data, keys, serial),
		}
	}

	fn leave(&self, seat: &Seat<FerriscopeState>, data: &mut FerriscopeState, serial: Serial) {
		match self {
			FocusTarget::Wayland(surface) => KeyboardTarget::leave(surface, seat, data, serial),
			FocusTarget::X11(surface) => KeyboardTarget::leave(surface, seat, data, serial),
		}
	}

	fn key(
		&self,
		seat: &Seat<FerriscopeState>,
		data: &mut FerriscopeState,
		key: KeysymHandle<'_>,
		state: KeyState,
		serial: Serial,
		time: InputTime,
	) {
		match self {
			FocusTarget::Wayland(surface) => KeyboardTarget::key(surface, seat, data, key, state, serial, time),
			FocusTarget::X11(surface) => KeyboardTarget::key(surface, seat, data, key, state, serial, time),
		}
	}

	fn modifiers(
		&self,
		seat: &Seat<FerriscopeState>,
		data: &mut FerriscopeState,
		modifiers: ModifiersState,
		serial: Serial,
	) {
		match self {
			FocusTarget::Wayland(surface) => KeyboardTarget::modifiers(surface, seat, data, modifiers, serial),
			FocusTarget::X11(surface) => KeyboardTarget::modifiers(surface, seat, data, modifiers, serial),
		}
	}
}

impl SeatHandler for FerriscopeState {
	type KeyboardFocus = FocusTarget;
	type PointerFocus = WlSurface;
	type TouchFocus = WlSurface;

	fn seat_state(&mut self) -> &mut SeatState<Self> {
		&mut self.seat_state
	}

	fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&FocusTarget>) {
		let client = focused
			.and_then(FocusTarget::wl_surface)
			.and_then(|surface| self.display_handle.get_client(surface.id()).ok());
		set_data_device_focus(&self.display_handle, seat, client);
	}

	fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
		// Both Wayland and Xwayland clients arrive here: Xwayland supplies its
		// (Xcursor-loaded) image through `wl_pointer.set_cursor`.
		match &image {
			CursorImageStatus::Surface(surface) => {
				let (has_buffer, scale) = with_states(surface, |states| {
					let mut cache = states
						.cached_state
						.get::<smithay::wayland::compositor::SurfaceAttributes>();
					let attrs = cache.current();
					(attrs.buffer.is_some(), attrs.buffer_scale)
				});
				tracing::debug!(
					cursor_status = "Surface",
					surface = ?surface.id(),
					has_buffer,
					buffer_scale = scale,
					"client set cursor image"
				);
			},
			CursorImageStatus::Hidden => {
				tracing::debug!(cursor_status = "Hidden", "client set cursor image");
			},
			CursorImageStatus::Named(icon) => {
				tracing::debug!(cursor_status = "Named", ?icon, "client set cursor image");
			},
		}
		self.cursor_state.set_status(&image);
	}
}

impl SelectionHandler for FerriscopeState {
	type SelectionUserData = ();
}

impl DataDeviceHandler for FerriscopeState {
	fn data_device_state(&mut self) -> &mut DataDeviceState {
		&mut self.data_device_state
	}
}

impl DndGrabHandler for FerriscopeState {}

impl WaylandDndGrabHandler for FerriscopeState {}
