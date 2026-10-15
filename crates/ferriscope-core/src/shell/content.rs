//! Override surfaces: a `wl_surface` that supplies an X11 window's content.
//!
//! The gamescope WSI layer presents a game's swapchain on a `wl_surface` of its
//! own and names the X11 window it replaces (`override_window_content`,
//! `wlserver.cpp:795`). gamescope links that surface to the window's
//! `wlserver_x11_surface_info::override_surface`, and from then on the window's
//! content is the override surface (`current_surface()`,
//! `steamcompmgr_shared.hpp:20`): its buffer is what is painted and its size is
//! the window's source size (`wlserver_get_surface_extent`,
//! `wlserver.cpp:2678`), its commits are the window's commits, and frame
//! callbacks go to it. Everything else about the window (input, geometry,
//! focus classification) still belongs to Xwayland's own surface.
//!
//! ferriscope keeps the link in [`ContentOverrides`], keyed by the X11 window
//! id like gamescope's `content_overrides`, and consults it where a window's
//! surface is read: the render path, the output-fill source size, the scene
//! signature, frame callbacks, buffer holding and the commit path.

use std::{
	collections::{HashMap, HashSet},
	time::Instant,
};

use smithay::{
	backend::renderer::utils::{CommitCounter, RendererSurfaceStateUserData},
	desktop::Window,
	reexports::wayland_server::{Resource, backend::ObjectId, protocol::wl_surface::WlSurface},
	utils::{Buffer as BufferCoord, Rectangle},
	wayland::compositor::{HookId, add_destruction_hook, remove_destruction_hook, with_states},
};

use crate::{
	focus::WindowId,
	shell::{FerriscopeState, geometry::window_wl_surface},
	swapchain::{
		self, SwapchainObject, VK_COLOR_SPACE_SRGB_NONLINEAR,
		ferriscope_protocol::server::ferriscope_swapchain_factory_v1::FerriscopeSwapchainFactoryV1,
	},
};

/// One window's override surface (`struct wlserver_content_override`,
/// `wlserver.cpp:95`).
struct ContentOverride {
	surface: WlSurface,
	/// The swapchain that installed it; `None` for the older `gamescope_xwayland`
	/// request. Told `retired` when the override is replaced.
	swapchain: Option<SwapchainObject>,
	hook: HookId,
	/// Whether the override surface is currently the window's presented content.
	///
	/// gamescope paints the window's most recent accepted commit, whichever of
	/// its two surfaces made it (`update_wayland_res`, `steamcompmgr.cpp:8242`):
	/// a window starts on its own surface and switches to the override with the
	/// override's first commit.
	presented: bool,
	/// The main surface's commit counter at its last accepted commit, to measure
	/// what the next one damaged.
	last_main_commit: Option<CommitCounter>,
}

/// Every override surface, keyed by X11 window id.
#[derive(Default)]
pub(in crate::shell) struct ContentOverrides {
	entries: HashMap<u32, ContentOverride>,
	/// Windows that ever presented a non-sRGB swapchain (`bHasHadNonSRGBColorSpace`,
	/// `steamcompmgr.cpp:8266`): only the WSI layer's commits are accepted for them.
	non_srgb: HashSet<WindowId>,
}

impl ContentOverrides {
	/// The key `window`'s override is stored under: its own id, or the frame
	/// the X server reparented it into when the layer named that.
	fn key_for(&self, window: &Window) -> Option<u32> {
		let surface = window.x11_surface()?;
		[Some(surface.window_id()), surface.mapped_window_id()]
			.into_iter()
			.flatten()
			.find(|key| self.entries.contains_key(key))
	}

	fn entry_for(&self, window: &Window) -> Option<&ContentOverride> {
		self.key_for(window).and_then(|key| self.entries.get(&key))
	}

	/// The override surface standing in for `window`'s content, once it has
	/// presented.
	pub fn presented(&self, window: &Window) -> Option<WlSurface> {
		self.entry_for(window)
			.filter(|entry| entry.presented)
			.map(|entry| entry.surface.clone())
	}

	/// The override surface installed for `window`, presented yet or not: it
	/// needs frame callbacks before its first commit.
	pub fn override_of(&self, window: &Window) -> Option<WlSurface> {
		self.entry_for(window).map(|entry| entry.surface.clone())
	}

	fn key_of_surface(&self, surface: &WlSurface) -> Option<u32> {
		self.entries
			.iter()
			.find(|(_, entry)| &entry.surface == surface)
			.map(|(key, _)| *key)
	}
}

/// How a committed surface relates to the windows' content.
pub(in crate::shell) enum CommitRoute {
	/// The commit updates this window's content.
	Window(WindowId),
	/// A window with an override surface committed its own surface and
	/// gamescope would not take it (`steamcompmgr.cpp:8288`).
	Ignored,
	/// Not a window's content.
	Unrouted,
}

/// Whether the bounding box of `damage` is larger than 2x2 in both axes
/// (`bHasDamage`, `steamcompmgr.cpp:8274`).
fn has_real_damage(damage: &[Rectangle<i32, BufferCoord>]) -> bool {
	let mut bounds: Option<(i32, i32, i32, i32)> = None;
	for rect in damage {
		let (x1, y1) = (rect.loc.x, rect.loc.y);
		let (x2, y2) = (x1 + rect.size.w, y1 + rect.size.h);
		bounds = Some(match bounds {
			Some((bx1, by1, bx2, by2)) => (bx1.min(x1), by1.min(y1), bx2.max(x2), by2.max(y2)),
			None => (x1, y1, x2, y2),
		});
	}
	bounds.is_some_and(|(x1, y1, x2, y2)| x2 - x1 > 2 && y2 - y1 > 2)
}

/// Whether a commit of a window's own surface is taken while an override
/// surface exists (`update_wayland_res`, `steamcompmgr.cpp:8268-8291`): not once
/// the window used a non-sRGB swapchain, not a tiny glamor commit, and not one
/// without damage.
fn main_commit_accepted(non_srgb: bool, buffer_size: (i32, i32), damaged: bool) -> bool {
	let bogus = buffer_size.0 <= 2 || buffer_size.1 <= 2;
	!(non_srgb || bogus || !damaged)
}

impl FerriscopeState {
	/// The refresh period reported to swapchains, in nanoseconds.
	fn refresh_cycle_ns(&self) -> u64 {
		(1_000_000_000.0 / self.config.rate.max(1.0)) as u64
	}

	/// `x11_find_toplevel_for_xid` plus the lookup of the tracked window
	/// (`handle_override_window_content`, `wlserver.cpp:795`): the WSI layer may
	/// name a child of the game's top-level window, as Wine does.
	fn override_key(&self, xid: u32) -> u32 {
		let top = self
			.steam
			.as_ref()
			.and_then(|steam| steam.toplevel_of(xid))
			.unwrap_or(xid);
		self.windows.resolve_sibling(top).unwrap_or(top)
	}

	/// `override_window_content` from any protocol. `server_id` is the Xwayland
	/// server the layer saw on its X11 connection; a layer on some other
	/// server's display must not take over a window here
	/// (`wlserver_get_xwayland_server`, `wlserver.cpp:981`). `None` names none
	/// (`gamescope_xwayland`).
	pub(crate) fn override_window_content(
		&mut self,
		swapchain: Option<SwapchainObject>,
		server_id: Option<u32>,
		surface: WlSurface,
		xid: u32,
	) {
		if let Some(id) = server_id
			&& self.xwayland_server_id != Some(id)
		{
			tracing::warn!(
				xid,
				server_id = id,
				ours = ?self.xwayland_server_id,
				"override_window_content names another Xwayland server; ignored"
			);
			return;
		}
		let key = self.override_key(xid);
		let swapchain_id = swapchain.as_ref().map(SwapchainObject::id);
		if let Some(existing) = self.content_overrides.entries.get(&key) {
			if existing.swapchain.as_ref().map(SwapchainObject::id) == swapchain_id && existing.surface == surface {
				return;
			}
			self.drop_override(key, true);
		}
		// A surface stands in for one window at a time
		// (`gamescope_swapchain_destroy_co`, `wlserver.cpp:940`).
		let stale: Vec<u32> = self
			.content_overrides
			.entries
			.iter()
			.filter(|(_, entry)| entry.surface == surface)
			.map(|(key, _)| *key)
			.collect();
		for stale_key in stale {
			self.drop_override(stale_key, false);
		}

		tracing::debug!(xid, key, surface = ?surface.id(), "override_window_content");
		let hook = add_destruction_hook::<FerriscopeState, _>(&surface, |state, surface| {
			state.override_surface_destroyed(surface);
		});
		self.content_overrides.entries.insert(
			key,
			ContentOverride {
				surface: surface.clone(),
				swapchain,
				hook,
				presented: false,
				last_main_commit: None,
			},
		);
		swapchain::send_refresh_cycle(&surface, self.refresh_cycle_ns());

		// Commits made before the link took effect count as the window's
		// (`g_PendingCommits`, `wlserver.cpp:835`).
		if surface_has_buffer(&surface) {
			self.accept_override_commit(key, &surface);
		}
	}

	/// Remove an override. `retire` tells its swapchain (`destroy_content_override`,
	/// `wlserver.cpp:728`).
	fn drop_override(&mut self, key: u32, retire: bool) {
		let Some(entry) = self.content_overrides.entries.remove(&key) else {
			return;
		};
		remove_destruction_hook(&entry.surface, &entry.hook);
		if retire && let Some(swapchain) = &entry.swapchain {
			swapchain.retired();
		}
		// The window is back on its own surface.
		self.scheduler.commit(Instant::now());
	}

	/// The override surface was destroyed (`content_override_handle_surface_destroy`,
	/// `wlserver.cpp:785`).
	fn override_surface_destroyed(&mut self, surface: &WlSurface) {
		if let Some(key) = self.content_overrides.key_of_surface(surface) {
			self.drop_override(key, true);
		}
	}

	/// A swapchain object went away (`gamescope_swapchain_handle_resource_destroy`,
	/// `wlserver.cpp:956`): its override goes with it, unless a newer swapchain
	/// has taken the window over since.
	pub(crate) fn swapchain_destroyed(&mut self, surface: &WlSurface, id: &ObjectId) {
		swapchain::forget_swapchain(surface, id);
		let orphaned: Vec<u32> = self
			.content_overrides
			.entries
			.iter()
			.filter(|(_, entry)| {
				&entry.surface == surface && entry.swapchain.as_ref().is_none_or(|swapchain| &swapchain.id() == id)
			})
			.map(|(key, _)| *key)
			.collect();
		for key in orphaned {
			self.drop_override(key, false);
		}
	}

	/// A client bound `ferriscope_swapchain_factory_v1`: it learns whether HDR
	/// is available and which Xwayland server is ours.
	pub(crate) fn bind_swapchain_factory(&mut self, factory: FerriscopeSwapchainFactoryV1) {
		self.swapchain_factories
			.bind(factory, self.hdr, self.xwayland_server_id);
	}

	/// `swapchain_feedback` arrived: the refresh period goes back to the layer.
	pub(crate) fn swapchain_feedback_received(&mut self, surface: &WlSurface) {
		swapchain::send_refresh_cycle(surface, self.refresh_cycle_ns());
	}

	/// An Xwayland window was destroyed: its override goes with it
	/// (`wlserver_x11_surface_info_finish`, `wlserver.cpp:3539`).
	pub(in crate::shell) fn forget_window_overrides(&mut self, window: &smithay::xwayland::X11Surface) {
		for key in [Some(window.window_id()), window.mapped_window_id()]
			.into_iter()
			.flatten()
		{
			self.drop_override(key, false);
		}
	}

	/// The surface whose buffer is `window`'s content: the override surface once
	/// it has presented, else the window's own.
	pub(in crate::shell) fn window_content_surface(&self, window: &Window) -> Option<WlSurface> {
		self.content_overrides
			.presented(window)
			.or_else(|| window_wl_surface(window))
	}

	/// A surface committed. Decides which window, if any, the commit updates and
	/// whether it is taken.
	pub(in crate::shell) fn route_commit(&mut self, root: &WlSurface) -> CommitRoute {
		if let Some(key) = self.content_overrides.key_of_surface(root) {
			return match self.accept_override_commit(key, root) {
				Some(id) => CommitRoute::Window(id),
				None => CommitRoute::Unrouted,
			};
		}
		let Some(id) = self.windows.id_of_root_surface(root) else {
			return CommitRoute::Unrouted;
		};
		let Some(key) = self
			.windows
			.window(id)
			.and_then(|window| self.content_overrides.key_for(window))
		else {
			return CommitRoute::Window(id);
		};
		let last = self
			.content_overrides
			.entries
			.get(&key)
			.and_then(|entry| entry.last_main_commit);
		let (buffer_size, damaged, counter) = with_states(root, |states| {
			let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() else {
				return ((0, 0), false, None);
			};
			let data = data.lock().unwrap();
			let size = data.buffer_size().map_or((0, 0), |size| (size.w, size.h));
			(
				size,
				has_real_damage(&data.damage_since(last)),
				Some(data.current_commit()),
			)
		});
		if !main_commit_accepted(self.content_overrides.non_srgb.contains(&id), buffer_size, damaged) {
			return CommitRoute::Ignored;
		}
		if let Some(entry) = self.content_overrides.entries.get_mut(&key) {
			entry.presented = false;
			entry.last_main_commit = counter;
		}
		CommitRoute::Window(id)
	}

	/// The override surface of window `key` committed. Returns the tracked
	/// window it updates.
	fn accept_override_commit(&mut self, key: u32, surface: &WlSurface) -> Option<WindowId> {
		let hints = swapchain::take_commit_hints(surface);
		tracing::trace!(
			present_mode = ?hints.present_mode,
			present_time = ?hints.present_time,
			"override surface commit"
		);
		let id = self.windows.resolve_sibling(key);
		if let Some(entry) = self.content_overrides.entries.get_mut(&key)
			&& surface_has_buffer(surface)
		{
			entry.presented = true;
		}
		if let Some(id) = id
			&& swapchain::swapchain_feedback(surface)
				.is_some_and(|feedback| feedback.vk_colorspace != VK_COLOR_SPACE_SRGB_NONLINEAR)
		{
			self.content_overrides.non_srgb.insert(id);
		}
		id
	}
}

/// Whether `surface` has a committed buffer.
fn surface_has_buffer(surface: &WlSurface) -> bool {
	with_states(surface, |states| {
		states
			.data_map
			.get::<RendererSurfaceStateUserData>()
			.is_some_and(|data| data.lock().unwrap().buffer_size().is_some())
	})
}

#[cfg(test)]
mod tests {
	use smithay::utils::{Point, Size};

	use super::*;

	fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, BufferCoord> {
		Rectangle::new(Point::from((x, y)), Size::from((w, h)))
	}

	#[test]
	fn damage_must_exceed_two_pixels_in_both_axes() {
		assert!(!has_real_damage(&[]));
		assert!(!has_real_damage(&[rect(0, 0, 2, 100)]));
		assert!(!has_real_damage(&[rect(0, 0, 100, 2)]));
		assert!(has_real_damage(&[rect(0, 0, 3, 3)]));
	}

	#[test]
	fn damage_is_measured_by_its_bounding_box() {
		// Two 1x1 patches far apart span more than 2x2.
		assert!(has_real_damage(&[rect(0, 0, 1, 1), rect(10, 10, 1, 1)]));
		assert!(!has_real_damage(&[rect(5, 5, 1, 1), rect(6, 6, 1, 1)]));
	}

	#[test]
	fn an_untouched_window_takes_a_big_damaged_commit() {
		assert!(main_commit_accepted(false, (1280, 720), true));
	}

	#[test]
	fn small_undamaged_or_hdr_main_commits_are_ignored() {
		// `steamcompmgr.cpp:8268-8291`
		assert!(!main_commit_accepted(false, (2, 720), true), "tiny glamor commit");
		assert!(!main_commit_accepted(false, (1280, 1), true), "tiny glamor commit");
		assert!(!main_commit_accepted(false, (1280, 720), false), "no damage");
		assert!(
			!main_commit_accepted(true, (1280, 720), true),
			"window used a non-sRGB swapchain"
		);
	}
}
