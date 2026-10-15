//! The per-window metadata cache.
//!
//! gamescope reads a window's properties **once** (at map, and again on a
//! `PropertyNotify`) into its `steamcompmgr_win_t`, and every later decision —
//! the focus pass, the input path — reads that struct. [`WindowTable`] is the
//! same thing: one [`WindowMeta`] per tracked window, filled by
//! [`FerriscopeState::track_x11_window`] / [`FerriscopeState::track_xdg_window`]
//! and refreshed by the events below. Nothing outside this module talks to the X
//! server about a window.
//!
//! | Event | Refreshes |
//! |---|---|
//! | map (`map_window_request`, `mapped_override_redirect_window`) | everything: Steam atoms, app id, pid, opacity, window type, transient-for, size hints, state flags, Wine styles, geometry, `map_seq` |
//! | `PropertyNotify` `STEAM_*` / `GAMESCOPE_EXTERNAL_OVERLAY` / Wine styles / `_NET_WM_STATE` | that atom's fields (`apply_watched`) |
//! | `PropertyNotify` opacity / transient-for / normal hints / window type | those fields |
//! | `ConfigureNotify` | `geometry` and the stacking order |
//! | commit | `has_commits`, `damage_seq` |
//! | unmap / destroy / client gone | the entry is dropped |

use std::{collections::HashMap, time::Instant};

use smithay::{
	desktop::Window,
	reexports::wayland_server::protocol::wl_surface::WlSurface,
	utils::IsAlive,
	wayland::{compositor::with_states, shell::xdg::ToplevelSurface},
	xwayland::{X11Surface, xwm::WmWindowProperty},
};

use smithay::backend::renderer::utils::RendererSurfaceStateUserData;

use crate::{
	focus::{Geometry, OPAQUE, STEAM_BIG_PICTURE_APP_ID, WAYLAND_ID_BASE, WindowId, WindowKind, WindowMeta},
	overlay::{WatchedAtom, WindowProps},
};

use crate::shell::FerriscopeState;
use crate::shell::geometry::{root_surface, window_wl_surface};

/// One tracked window: the smithay handle plus its cached metadata.
pub(in crate::shell) struct ManagedWindow {
	pub window: Window,
	pub meta: WindowMeta,
	/// When the window last received frame callbacks that it had requested
	/// (`last_commit_first_latch_time`, `steamcompmgr.cpp:6686`), the base of the
	/// FPS-limit schedule.
	pub last_frame_done: Option<Instant>,
}

/// Every tracked window, keyed by [`WindowId`], with gamescope's list order.
#[derive(Default)]
pub(in crate::shell) struct WindowTable {
	windows: HashMap<WindowId, ManagedWindow>,
	/// Xwayland windows, topmost first (`ctx->list`).
	x11_stack: Vec<WindowId>,
	/// Native Wayland toplevels in creation order (`g_steamcompmgr_xdg_wins`).
	xdg_order: Vec<WindowId>,
	next_map_seq: u64,
	next_damage_seq: u64,
	next_xdg_id: u32,
}

impl WindowTable {
	/// Track an Xwayland window, topmost; a stale entry with the same id is
	/// replaced. Takes the next `map_sequence` (`map_win`, `:5771`).
	pub fn insert_x11(&mut self, window: Window, mut meta: WindowMeta) {
		self.remove(meta.id);
		self.next_map_seq += 1;
		meta.map_seq = self.next_map_seq;
		meta.damage_seq = 0;
		self.x11_stack.insert(0, meta.id);
		self.windows.insert(
			meta.id,
			ManagedWindow {
				window,
				meta,
				last_frame_done: None,
			},
		);
	}

	/// Track a native Wayland toplevel and return its synthetic id.
	pub fn insert_xdg(&mut self, window: Window, mut meta: WindowMeta) -> WindowId {
		self.next_xdg_id += 1;
		self.next_map_seq += 1;
		meta.id = WAYLAND_ID_BASE + self.next_xdg_id;
		meta.map_seq = self.next_map_seq;
		self.xdg_order.push(meta.id);
		let id = meta.id;
		self.windows.insert(
			id,
			ManagedWindow {
				window,
				meta,
				last_frame_done: None,
			},
		);
		id
	}

	/// Drop a window; `true` if it was tracked.
	pub fn remove(&mut self, id: WindowId) -> bool {
		self.x11_stack.retain(|stacked| *stacked != id);
		self.xdg_order.retain(|ordered| *ordered != id);
		self.windows.remove(&id).is_some()
	}

	pub fn get(&self, id: WindowId) -> Option<&ManagedWindow> {
		self.windows.get(&id)
	}

	pub fn get_mut(&mut self, id: WindowId) -> Option<&mut ManagedWindow> {
		self.windows.get_mut(&id)
	}

	/// The smithay handle of a window.
	pub fn window(&self, id: WindowId) -> Option<&Window> {
		self.windows.get(&id).map(|managed| &managed.window)
	}

	/// The id of the window whose root surface is `root`.
	pub fn id_of_root_surface(&self, root: &WlSurface) -> Option<WindowId> {
		self.windows
			.iter()
			.find(|(_, managed)| window_wl_surface(&managed.window).as_ref() == Some(root))
			.map(|(id, _)| *id)
	}

	/// The Xwayland windows, topmost first.
	pub fn x11_metas(&self) -> Vec<&WindowMeta> {
		self.x11_stack
			.iter()
			.filter_map(|id| self.windows.get(id))
			.map(|managed| &managed.meta)
			.collect()
	}

	/// The native Wayland toplevels, oldest first.
	pub fn xdg_metas(&self) -> Vec<&WindowMeta> {
		self.xdg_order
			.iter()
			.filter_map(|id| self.windows.get(id))
			.map(|managed| &managed.meta)
			.collect()
	}

	/// The topmost Xwayland window (`ctx->list[0]`).
	pub fn top_x11(&self) -> Option<WindowId> {
		self.x11_stack.first().copied()
	}

	/// Every tracked window handle, for presentation feedback.
	pub fn windows(&self) -> impl Iterator<Item = &Window> {
		self.windows.values().map(|managed| &managed.window)
	}

	/// Every tracked window, for frame callbacks.
	pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut ManagedWindow> {
		self.windows.values_mut()
	}

	/// The tracked window an X sibling id names: its own id, or the frame
	/// Smithay stacks it by (`ConfigureNotify` reports the frame of a managed
	/// window and the frames of its siblings).
	pub fn resolve_sibling(&self, sibling: u32) -> Option<WindowId> {
		self.windows.iter().find_map(|(id, managed)| {
			let surface = managed.window.x11_surface()?;
			(surface.window_id() == sibling || surface.mapped_window_id() == Some(sibling)).then_some(*id)
		})
	}

	/// `restack_win` (`steamcompmgr.cpp:6086`): put `id` directly above `above`,
	/// or at the bottom when `above` is `None` / unknown.
	pub fn restack(&mut self, id: WindowId, above: Option<WindowId>) -> bool {
		let Some(from) = self.x11_stack.iter().position(|stacked| *stacked == id) else {
			return false;
		};
		let current_above = self.x11_stack.get(from + 1).copied();
		if current_above == above {
			return false;
		}
		self.x11_stack.remove(from);
		let to = above
			.and_then(|above| self.x11_stack.iter().position(|stacked| *stacked == above))
			.unwrap_or(self.x11_stack.len());
		self.x11_stack.insert(to, id);
		true
	}

	/// Put `id` on top of the X stacking, after ferriscope raised it itself.
	pub fn raise_to_top(&mut self, id: WindowId) {
		if let Some(from) = self.x11_stack.iter().position(|stacked| *stacked == id) {
			self.x11_stack.remove(from);
			self.x11_stack.insert(0, id);
		}
	}

	/// Take the next damage sequence number (`damageSequence++`, `:6295`).
	fn next_damage(&mut self) -> u64 {
		self.next_damage_seq += 1;
		self.next_damage_seq
	}

	/// Drop windows whose client went away without an unmap callback; returns
	/// whether anything was dropped.
	pub fn prune_dead(&mut self) -> bool {
		let dead: Vec<WindowId> = self
			.windows
			.iter()
			.filter(|(_, managed)| !managed.window.alive())
			.map(|(id, _)| *id)
			.collect();
		for id in &dead {
			self.remove(*id);
		}
		!dead.is_empty()
	}
}

/// The app id a window has apart from `STEAM_BIGPICTURE`: its `STEAM_GAME`, else
/// its window id (no `--steam`) or the one derived from its process (`--steam`,
/// `add_win`, `steamcompmgr.cpp:5942-5950`; 0 when the process has none).
fn own_app_id(meta: &WindowMeta, steam_mode: bool) -> u32 {
	if meta.steam_app_id != 0 {
		meta.steam_app_id
	} else if steam_mode {
		meta.pid_app_id
	} else {
		meta.id
	}
}

/// `map_win` (`steamcompmgr.cpp:5695-5750`) applied to a fresh [`WindowMeta`].
///
/// `steam_mode` is gamescope's `steamMode`: without it a window that carries no
/// `STEAM_GAME` has its window id as `appID` (`:5724`); with it such a window
/// keeps the app id its process gave it (`meta.pid_app_id`, set by the caller
/// before this), if any.
pub(in crate::shell) fn apply_map_props(meta: &mut WindowMeta, props: &WindowProps, steam_mode: bool) {
	meta.opacity = props.opacity.unwrap_or(OPAQUE);
	meta.is_steam_legacy_big_picture = props.big_picture;
	meta.input_focus_mode = props.input_focus;
	meta.steam_app_id = props.game;
	// `STEAM_GAME` is authoritative whenever the window carries it. gamescope
	// only honours it at map time under `steamMode` (`:5714`) but always on a
	// later `PropertyNotify` (`:6927`); ferriscope applies the second rule at map
	// time too, so a window tagged before it maps is classified the same as one
	// tagged after.
	meta.app_id = own_app_id(meta, steam_mode);
	if meta.is_steam_legacy_big_picture {
		meta.app_id = STEAM_BIG_PICTURE_APP_ID;
	}
	meta.is_overlay = props.overlay;
	meta.is_external_overlay = props.external_overlay;
	if meta.is_external_overlay {
		meta.app_id = 0;
	}
	apply_size_hints(meta, props);
	meta.is_fullscreen = props.fullscreen;
	meta.skip_taskbar = props.skip_taskbar;
	meta.skip_pager = props.skip_pager;
	meta.transient_for = props.transient_for;
	apply_window_type(meta, props);
	meta.hwnd_style = props.hwnd_style;
	meta.hwnd_style_ex = props.hwnd_style_ex;
}

/// `get_size_hints` (`steamcompmgr.cpp:5496`).
fn apply_size_hints(meta: &mut WindowMeta, props: &WindowProps) {
	meta.maybe_a_dropdown = props.size_hints.static_position;
	match props.size_hints.fixed_size {
		Some(size) => {
			meta.requested_size = size;
			meta.size_hints_specified = true;
		},
		None => meta.size_hints_specified = false,
	}
}

/// `get_win_type` (`steamcompmgr.cpp:5473`): a transient-for window is a dialog
/// unless `_NET_WM_WINDOW_TYPE` says otherwise.
fn apply_window_type(meta: &mut WindowMeta, props: &WindowProps) {
	meta.is_dialog = props.dialog.unwrap_or(meta.transient_for.is_some());
}

/// The `PropertyNotify` handler for one watched atom
/// (`handle_property_notify`, `steamcompmgr.cpp:6790-7640`).
pub(in crate::shell) fn apply_watched(meta: &mut WindowMeta, atom: WatchedAtom, props: &WindowProps, steam_mode: bool) {
	match atom {
		// :6948
		WatchedAtom::Overlay => {
			meta.is_overlay = props.overlay;
			if meta.is_external_overlay {
				meta.app_id = 0;
			}
		},
		// :6836
		WatchedAtom::InputFocus => meta.input_focus_mode = props.input_focus,
		// :6827 gamescope never undoes the 769 when the atom is cleared
		// (`:6831`); ferriscope falls back to the window's own id instead, which
		// is what a window that never carried the atom has.
		WatchedAtom::BigPicture => {
			meta.is_steam_legacy_big_picture = props.big_picture;
			if meta.is_steam_legacy_big_picture {
				meta.app_id = STEAM_BIG_PICTURE_APP_ID;
			} else if meta.app_id == STEAM_BIG_PICTURE_APP_ID {
				meta.app_id = own_app_id(meta, steam_mode);
			}
		},
		// :6919 the `STEAM_GAME` value replaces the app id, even with zero.
		// gamescope then leaves the window without one; ferriscope falls back to
		// the process-derived id `add_win` gave it (`:5946`), as in Steam mode a
		// window never loses that.
		WatchedAtom::Game => {
			meta.steam_app_id = props.game;
			meta.app_id = if props.game == 0 && steam_mode {
				meta.pid_app_id
			} else {
				props.game
			};
			if meta.is_external_overlay {
				meta.app_id = 0;
			}
		},
		// :6960
		WatchedAtom::ExternalOverlay => {
			meta.is_external_overlay = props.external_overlay;
			if meta.is_external_overlay {
				meta.app_id = 0;
			}
		},
		// gamescope learns these from `_NET_WM_STATE` client messages; the
		// fullscreen bit is ferriscope's own to write, so only the skip hints
		// follow the property.
		WatchedAtom::WmState => {
			meta.skip_taskbar = props.skip_taskbar;
			meta.skip_pager = props.skip_pager;
		},
		// :7624
		WatchedAtom::HwndStyle => meta.hwnd_style = Some(props.hwnd_style.unwrap_or(0)),
		// :7635
		WatchedAtom::HwndStyleEx => meta.hwnd_style_ex = Some(props.hwnd_style_ex.unwrap_or(0)),
	}
}

impl FerriscopeState {
	/// Read an Xwayland window's watched properties over the second X
	/// connection; the defaults when there is none. The only X round trip a
	/// window costs, taken at map time and on a property change.
	fn read_x11_props(&self, id: WindowId) -> WindowProps {
		self.steam
			.as_ref()
			.map(|steam| steam.read_window(id))
			.unwrap_or_default()
	}

	/// Start tracking an Xwayland window that just mapped: read its properties
	/// once into a [`WindowMeta`] and take a `map_sequence`.
	///
	/// Returns whether the window is the Steam UI, which gamescope sizes to the
	/// output (`window_is_steam`, `:1289`).
	pub(in crate::shell) fn track_x11_window(&mut self, surface: X11Surface) -> bool {
		let id = surface.window_id();
		let props = self.read_x11_props(id);
		let mut meta = WindowMeta::new(id, WindowKind::Xwayland);
		meta.override_redirect = surface.is_override_redirect();
		// The origin the client asked for (an override-redirect window never gets
		// a `ConfigureNotify` before it maps) and the size it has.
		let size = surface.geometry().size;
		let origin = surface.last_configure().loc;
		meta.geometry = Geometry {
			x: origin.x,
			y: origin.y,
			w: size.w,
			h: size.h,
		};
		// gamescope reads the client pid with XRes (`get_win_pid`, `:5858`, called
		// from `add_win` `:5921`); `_NET_WM_PID` is the fallback. In Steam mode
		// the pid also names the app (`:5942-5950`), cached per pid. The
		// `/proc` walk is a few small reads, so it runs inline.
		meta.pid = surface.get_client_pid().ok().unwrap_or(props.pid);
		if self.config.steam && meta.pid != 0 {
			meta.pid_app_id = self.pid_app_ids.get(meta.pid);
		}
		apply_map_props(&mut meta, &props, self.config.steam);
		tracing::debug!(pid = meta.pid, pid_app_id = meta.pid_app_id, "x11 window tracked");
		// An overlay that is narrower than the output and wants no input is a
		// notification, which is never resized (it is not the focus window).
		let notification_like =
			meta.is_overlay && meta.geometry.w < self.output_rect().size.w && meta.input_focus_mode == 0;
		let is_steam_ui = meta.is_steam() && !meta.override_redirect && !notification_like;
		let window = Window::new_x11_window(surface);
		// An override surface that presented before the window mapped is the
		// window's first commit (`g_PendingCommits`, `wlserver.cpp:835`).
		let presented = self.content_overrides.presented(&window);
		self.windows.insert_x11(window, meta);
		self.mark_focus_dirty();
		if let Some(content) = presented {
			self.note_commit(id, &content);
		}
		is_steam_ui
	}

	/// A client asked for `_NET_WM_STATE_FULLSCREEN` to change
	/// (`handle_net_wm_state`, `steamcompmgr.cpp:6381`): remember it as a hint;
	/// ferriscope writes the state itself.
	pub(in crate::shell) fn set_x11_fullscreen_hint(&mut self, surface: &X11Surface, fullscreen: bool) {
		if let Some(managed) = self.windows.get_mut(surface.window_id()) {
			managed.meta.is_fullscreen = fullscreen;
			self.mark_focus_dirty();
		}
	}

	/// Start tracking a native Wayland toplevel.
	pub(in crate::shell) fn track_xdg_window(&mut self, surface: &ToplevelSurface) -> WindowId {
		let window = Window::new_wayland_window(surface.clone());
		let geometry = window.geometry();
		let mut meta = WindowMeta::new(0, WindowKind::Xdg);
		meta.geometry = Geometry {
			x: geometry.loc.x,
			y: geometry.loc.y,
			w: geometry.size.w,
			h: geometry.size.h,
		};
		meta.transient_for = surface
			.parent()
			.and_then(|parent| self.windows.id_of_root_surface(&root_surface(&parent)));
		meta.is_dialog = meta.transient_for.is_some();
		// A native Wayland window has no `STEAM_GAME`: its app id comes from the
		// client's process, as gamescope's (`wlserver.cpp:2020`). A Proton game
		// with `PROTON_ENABLE_WAYLAND=1` gets its Steam app id this way.
		meta.pid = smithay::reexports::wayland_server::Resource::client(surface.wl_surface())
			.and_then(|client| client.get_credentials(&self.display_handle).ok())
			.map_or(0, |credentials| credentials.pid.max(0) as u32);
		if meta.pid != 0 {
			meta.app_id = self.pid_app_ids.get(meta.pid);
		}
		tracing::debug!(pid = meta.pid, app_id = meta.app_id, "xdg toplevel tracked");
		let id = self.windows.insert_xdg(window, meta);
		self.mark_focus_dirty();
		id
	}

	/// A native Wayland toplevel's parent changed (`xdg_toplevel.set_parent`).
	pub(in crate::shell) fn xdg_parent_changed(&mut self, surface: &ToplevelSurface) {
		let Some(id) = self.windows.id_of_root_surface(surface.wl_surface()) else {
			return;
		};
		let parent = surface
			.parent()
			.and_then(|parent| self.windows.id_of_root_surface(&root_surface(&parent)));
		if let Some(managed) = self.windows.get_mut(id) {
			managed.meta.transient_for = parent;
			managed.meta.is_dialog = parent.is_some();
		}
		self.mark_focus_dirty();
	}

	/// An Xwayland window unmapped or was destroyed.
	pub(in crate::shell) fn forget_window(&mut self, id: WindowId) {
		self.focus.forget_iconified(id);
		if self.windows.remove(id) {
			self.mark_focus_dirty();
		}
	}

	/// An Xwayland property changed: refresh the cache entry.
	///
	/// Smithay forwards the atom of properties it has no typed accessor for as
	/// [`WmWindowProperty::Other`]; the typed ones arrive as their own variant.
	/// Either way the value is read here, once, into [`WindowMeta`].
	pub(in crate::shell) fn refresh_x11_property(&mut self, surface: &X11Surface, property: WmWindowProperty) {
		let id = surface.window_id();
		if self.windows.get(id).is_none() {
			// Pre-map changes are picked up by the map-time read.
			return;
		}
		let watched = match property {
			WmWindowProperty::Other(atom) => {
				let Some(watched) = self.steam.as_ref().and_then(|steam| steam.watched(atom)) else {
					return;
				};
				Some(watched)
			},
			WmWindowProperty::Opacity
			| WmWindowProperty::TransientFor
			| WmWindowProperty::NormalHints
			| WmWindowProperty::WindowType => None,
			_ => return,
		};
		let props = self.read_x11_props(id);
		let steam_mode = self.config.steam;
		let Some(managed) = self.windows.get_mut(id) else {
			return;
		};
		let meta = &mut managed.meta;
		match (watched, property) {
			(Some(atom), _) => apply_watched(meta, atom, &props, steam_mode),
			// :6730
			(None, WmWindowProperty::Opacity) => meta.opacity = props.opacity.unwrap_or(OPAQUE),
			(None, WmWindowProperty::TransientFor) => {
				meta.transient_for = props.transient_for;
				apply_window_type(meta, &props);
			},
			(None, WmWindowProperty::NormalHints) => apply_size_hints(meta, &props),
			(None, WmWindowProperty::WindowType) => apply_window_type(meta, &props),
			(None, _) => {},
		}
		self.mark_focus_dirty();
	}

	/// An Xwayland window changed size or position (`configure_win`,
	/// `steamcompmgr.cpp:6044`) or was restacked.
	pub(in crate::shell) fn note_x11_configure(
		&mut self,
		surface: &X11Surface,
		geometry: smithay::utils::Rectangle<i32, smithay::utils::Logical>,
		above: Option<u32>,
	) {
		let id = surface.window_id();
		let Some(managed) = self.windows.get_mut(id) else {
			return;
		};
		managed.meta.geometry = Geometry {
			x: geometry.loc.x,
			y: geometry.loc.y,
			w: geometry.size.w,
			h: geometry.size.h,
		};
		// An unknown sibling (a window ferriscope does not track) says nothing about
		// where this one sits among the tracked windows.
		match above.map(|sibling| self.windows.resolve_sibling(sibling)) {
			Some(None) => {},
			Some(sibling) => {
				self.windows.restack(id, sibling);
			},
			None => {
				self.windows.restack(id, None);
			},
		}
		self.mark_focus_dirty();
	}

	/// A window's content surface committed (`damage_win`, `steamcompmgr.cpp:6265`);
	/// `content` is the committing surface, the override surface for a window
	/// the WSI layer drives.
	///
	/// A commit that attached a buffer gives the window its `damage_sequence`
	/// and, the first time, `window_has_commits`; either can change who the best
	/// window is, so the focus is re-evaluated (`MakeFocusDirty`, `:6294`).
	pub(in crate::shell) fn note_commit(&mut self, id: WindowId, content: &WlSurface) {
		if self.windows.get(id).is_none() {
			return;
		}
		let has_buffer = with_states(content, |states| {
			states
				.data_map
				.get::<RendererSurfaceStateUserData>()
				.is_some_and(|data| data.lock().unwrap().buffer_size().is_some())
		});
		if !has_buffer {
			return;
		}
		let damage = self.windows.next_damage();
		let focus_damage = self
			.focus
			.ctx_focus()
			.and_then(|focus| self.windows.get(focus))
			.map(|focus| focus.meta.damage_seq);
		let Some(managed) = self.windows.get_mut(id) else {
			return;
		};
		let meta = &mut managed.meta;
		// :6286 an overlay that is invisible is not worth a pass.
		if (meta.is_overlay || meta.is_external_overlay) && meta.opacity == 0 {
			meta.damage_seq = damage;
			meta.has_commits = true;
			return;
		}
		let first = !meta.has_commits;
		let first_damage = meta.damage_seq == 0;
		meta.damage_seq = damage;
		meta.has_commits = true;
		let cares = !meta.is_useless() && !meta.is_overlay && !meta.is_external_overlay;
		// :6293 the first damage, or passing the focus window's.
		let passes_focus = Some(id) != self.focus.ctx_focus() && focus_damage.is_some_and(|seq| damage > seq);
		if first || (cares && (first_damage || passes_focus)) {
			self.mark_focus_dirty();
			if first {
				// The window can now be presented; do not make it wait a tick.
				self.determine_and_apply_focus();
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn props() -> WindowProps {
		WindowProps::default()
	}

	fn meta() -> WindowMeta {
		WindowMeta::new(42, WindowKind::Xwayland)
	}

	#[test]
	fn without_steam_mode_the_app_id_is_the_window_id() {
		let mut meta = meta();
		apply_map_props(&mut meta, &props(), false);
		assert_eq!(meta.app_id, 42);
	}

	#[test]
	fn in_steam_mode_a_process_derived_id_stands_in_for_a_missing_steam_game() {
		let mut meta = meta();
		meta.pid_app_id = 730;
		apply_map_props(&mut meta, &props(), true);
		assert_eq!(meta.app_id, 730);
		// An explicit STEAM_GAME wins, and clearing it falls back.
		apply_watched(
			&mut meta,
			WatchedAtom::Game,
			&WindowProps { game: 440, ..props() },
			true,
		);
		assert_eq!(meta.app_id, 440);
		apply_watched(&mut meta, WatchedAtom::Game, &props(), true);
		assert_eq!(meta.app_id, 730);
		// Without Steam mode the window id is the app id, never the process's.
		let mut plain = self::meta();
		plain.pid_app_id = 730;
		apply_map_props(&mut plain, &props(), false);
		assert_eq!(plain.app_id, 42);
	}

	#[test]
	fn the_steam_game_atom_is_authoritative_with_or_without_steam_mode() {
		let with_game = WindowProps { game: 440, ..props() };
		let mut meta = meta();
		apply_map_props(&mut meta, &with_game, false);
		assert_eq!(meta.app_id, 440);
		assert_eq!(meta.steam_app_id, 440);
	}

	#[test]
	fn in_steam_mode_the_steam_game_atom_is_authoritative_and_absent_means_none() {
		let mut meta = meta();
		apply_map_props(&mut meta, &props(), true);
		assert_eq!(meta.app_id, 0);
		let with_game = WindowProps { game: 440, ..props() };
		apply_map_props(&mut meta, &with_game, true);
		assert_eq!(meta.app_id, 440);
		assert_eq!(meta.steam_app_id, 440);
	}

	#[test]
	fn steam_bigpicture_is_app_id_769_and_beats_steam_game() {
		let mut meta = meta();
		let big = WindowProps {
			big_picture: true,
			game: 440,
			..props()
		};
		apply_map_props(&mut meta, &big, true);
		assert_eq!(meta.app_id, STEAM_BIG_PICTURE_APP_ID);
		assert!(meta.is_steam());
	}

	#[test]
	fn an_external_overlay_never_has_an_app_id() {
		let mut meta = meta();
		let external = WindowProps {
			external_overlay: true,
			game: 440,
			..props()
		};
		apply_map_props(&mut meta, &external, true);
		assert_eq!(meta.app_id, 0);
		apply_watched(&mut meta, WatchedAtom::Game, &WindowProps { game: 7, ..props() }, true);
		assert_eq!(meta.app_id, 0);
	}

	#[test]
	fn a_steam_game_change_replaces_the_app_id_even_without_steam_mode() {
		// :6927 `w->appID = appID` regardless of `steamMode`.
		let mut meta = meta();
		apply_map_props(&mut meta, &props(), false);
		apply_watched(
			&mut meta,
			WatchedAtom::Game,
			&WindowProps { game: 440, ..props() },
			false,
		);
		assert_eq!(meta.app_id, 440);
		apply_watched(&mut meta, WatchedAtom::Game, &props(), false);
		assert_eq!(meta.app_id, 0, "clearing the atom clears the id, like gamescope");
	}

	#[test]
	fn clearing_steam_bigpicture_restores_the_windows_own_app_id() {
		// gamescope leaves the 769 in place (:6831); ferriscope falls back to the
		// id the window would have had without the atom.
		let mut meta = meta();
		apply_map_props(&mut meta, &WindowProps { game: 440, ..props() }, true);
		apply_watched(
			&mut meta,
			WatchedAtom::BigPicture,
			&WindowProps {
				big_picture: true,
				game: 440,
				..props()
			},
			true,
		);
		assert_eq!(meta.app_id, STEAM_BIG_PICTURE_APP_ID);
		apply_watched(&mut meta, WatchedAtom::BigPicture, &props(), true);
		assert!(!meta.is_steam_legacy_big_picture);
		assert_eq!(meta.app_id, 440);
	}

	#[test]
	fn the_overlay_and_input_focus_atoms_update_their_fields() {
		let mut meta = meta();
		apply_watched(
			&mut meta,
			WatchedAtom::Overlay,
			&WindowProps {
				overlay: true,
				..props()
			},
			false,
		);
		assert!(meta.is_overlay);
		apply_watched(
			&mut meta,
			WatchedAtom::InputFocus,
			&WindowProps {
				input_focus: 2,
				..props()
			},
			false,
		);
		assert_eq!(meta.input_focus_mode, 2);
		apply_watched(&mut meta, WatchedAtom::Overlay, &props(), false);
		assert!(!meta.is_overlay);
	}

	#[test]
	fn a_transient_window_is_a_dialog_unless_the_window_type_says_normal() {
		let mut meta = meta();
		let transient = WindowProps {
			transient_for: Some(7),
			..props()
		};
		apply_map_props(&mut meta, &transient, false);
		assert!(meta.is_dialog);
		let normal = WindowProps {
			transient_for: Some(7),
			dialog: Some(false),
			..props()
		};
		apply_map_props(&mut meta, &normal, false);
		assert!(!meta.is_dialog);
		let dialog = WindowProps {
			dialog: Some(true),
			..props()
		};
		apply_map_props(&mut meta, &dialog, false);
		assert!(meta.is_dialog);
	}

	#[test]
	fn size_hints_drive_the_dropdown_flag_and_the_requested_size() {
		use crate::overlay::SizeHints;
		let mut meta = meta();
		let hints = WindowProps {
			size_hints: SizeHints {
				static_position: true,
				fixed_size: Some((800, 600)),
			},
			..props()
		};
		apply_map_props(&mut meta, &hints, false);
		assert!(meta.maybe_a_dropdown);
		assert!(meta.size_hints_specified);
		assert_eq!(meta.requested_size, (800, 600));
		apply_size_hints(&mut meta, &props());
		assert!(!meta.maybe_a_dropdown);
		assert!(!meta.size_hints_specified);
	}

	#[test]
	fn wine_styles_are_present_only_once_the_property_exists() {
		let mut meta = meta();
		apply_map_props(&mut meta, &props(), false);
		assert_eq!(meta.hwnd_style, None);
		apply_watched(&mut meta, WatchedAtom::HwndStyle, &props(), false);
		assert_eq!(meta.hwnd_style, Some(0));
		apply_watched(
			&mut meta,
			WatchedAtom::HwndStyleEx,
			&WindowProps {
				hwnd_style_ex: Some(0x80000),
				..props()
			},
			false,
		);
		assert_eq!(meta.hwnd_style_ex, Some(0x80000));
	}

	#[test]
	fn the_skip_hints_follow_the_wm_state_property_but_fullscreen_does_not() {
		let mut meta = meta();
		meta.is_fullscreen = true;
		apply_watched(
			&mut meta,
			WatchedAtom::WmState,
			&WindowProps {
				skip_taskbar: true,
				skip_pager: true,
				..props()
			},
			false,
		);
		assert!(meta.skip_taskbar && meta.skip_pager);
		assert!(meta.is_fullscreen, "ferriscope writes the fullscreen state itself");
	}

	#[test]
	fn restack_puts_a_window_directly_above_its_sibling() {
		let mut table = WindowTable {
			x11_stack: vec![1, 2, 3],
			..WindowTable::default()
		};
		assert!(table.restack(3, Some(1)));
		assert_eq!(table.x11_stack, vec![3, 1, 2]);
		// Already directly above: nothing to do.
		assert!(!table.restack(3, Some(1)));
		// No sibling: to the bottom.
		assert!(table.restack(3, None));
		assert_eq!(table.x11_stack, vec![1, 2, 3]);
	}

	#[test]
	fn raise_to_top_moves_a_window_to_the_front() {
		let mut table = WindowTable {
			x11_stack: vec![1, 2, 3],
			..WindowTable::default()
		};
		table.raise_to_top(3);
		assert_eq!(table.x11_stack, vec![3, 1, 2]);
		// An untracked window changes nothing.
		table.raise_to_top(9);
		assert_eq!(table.x11_stack, vec![3, 1, 2]);
	}
}
