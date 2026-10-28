//! The WSI protocols: `ferriscope_swapchain_factory_v1` /
//! `ferriscope_swapchain_v1` (the native one, spoken by `ferriscope-wsi`), and
//! gamescope's `gamescope_xwayland` and `gamescope_swapchain_factory_v2` /
//! `gamescope_swapchain` as a compatibility adapter for a stock
//! `VkLayer_FROG_gamescope_wsi`.
//!
//! The Vulkan WSI layer creates a Wayland `wl_surface` on the compositor's own
//! socket, presents the game's swapchain images to it, and tells the compositor
//! which X11 window that surface stands in for (`override_window_content`). It
//! also reports what the swapchain is (`swapchain_feedback`), its present mode
//! and HDR metadata.
//!
//! Both swapchain protocols feed one model: a request lands in
//! [`SwapchainSurface`] (kept in the surface's data map like gamescope's
//! `wlserver_wl_surface_info`) through the same dispatch, and the only
//! per-protocol code is the wire object ([`SwapchainObject`]) and the factory
//! globals. What an override surface *does* to the scene lives in
//! `shell::content`.
//!
//! References are to gamescope's `src/wlserver.cpp`.

use std::sync::Mutex;

use smithay::{
	reexports::wayland_server::protocol::wl_surface::WlSurface,
	wayland::{Dispatch2, GlobalDispatch2, compositor::with_states},
};
use wayland_server::{
	Client, DataInit, DisplayHandle, New, Resource,
	backend::{ClientId, GlobalId, ObjectId},
};

use crate::shell::FerriscopeState;

macro_rules! protocol_bindings {
	($module:ident, $xml:literal) => {
		/// Server bindings generated at compile time by the `wayland-scanner`
		/// proc macros from the vendored protocol XML.
		pub mod $module {
			#![allow(
				dead_code,
				non_camel_case_types,
				unused_unsafe,
				unused_variables,
				non_upper_case_globals,
				non_snake_case,
				unused_imports,
				missing_docs
			)]
			#![allow(clippy::all)]

			/// Server-side API of the protocol.
			pub mod server {
				use wayland_server;
				use wayland_server::protocol::*;

				pub mod __interfaces {
					use wayland_server::protocol::__interfaces::*;
					wayland_scanner::generate_interfaces!($xml);
				}
				use self::__interfaces::*;

				wayland_scanner::generate_server_code!($xml);
			}
		}
	};
}

protocol_bindings!(xwayland_protocol, "protocol/gamescope-xwayland.xml");
protocol_bindings!(gamescope_protocol, "protocol/gamescope-swapchain.xml");
protocol_bindings!(ferriscope_protocol, "protocol/ferriscope-swapchain-v1.xml");

use ferriscope_protocol::server::{
	ferriscope_swapchain_factory_v1::{self, FerriscopeSwapchainFactoryV1},
	ferriscope_swapchain_v1::{self, FerriscopeSwapchainV1},
};
use gamescope_protocol::server::{
	gamescope_swapchain::{self, GamescopeSwapchain},
	gamescope_swapchain_factory_v2::{self, GamescopeSwapchainFactoryV2},
};
use xwayland_protocol::server::gamescope_xwayland::{self, GamescopeXwayland};

/// `VK_COLOR_SPACE_SRGB_NONLINEAR_KHR`: the colour space of an SDR swapchain.
pub const VK_COLOR_SPACE_SRGB_NONLINEAR: u32 = 0;

/// What the WSI layer reported about a swapchain (`swapchain_feedback`),
/// gamescope's `wlserver_vk_swapchain_feedback`. Values are the raw Vulkan
/// enumerants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapchainFeedback {
	/// Number of images in the swapchain.
	pub image_count: u32,
	/// `VkFormat` of the swapchain images.
	pub vk_format: u32,
	/// `VkColorSpaceKHR`; anything but [`VK_COLOR_SPACE_SRGB_NONLINEAR`] means
	/// the swapchain carries HDR (PQ or scRGB) content.
	pub vk_colorspace: u32,
	/// `VkCompositeAlphaFlagBitsKHR`.
	pub vk_composite_alpha: u32,
	/// `VkSurfaceTransformFlagBitsKHR`.
	pub vk_pre_transform: u32,
	/// `clipped` (`VkBool32`).
	pub vk_clipped: bool,
	/// The application's engine name.
	pub engine_name: String,
	/// HDR mastering metadata from `vkSetHdrMetadataEXT`, if the layer sent a
	/// valid one after the feedback.
	pub hdr_metadata: Option<HdrMetadata>,
}

/// CTA-861.G HDR static metadata exactly as `set_hdr_metadata` carries it:
/// chromaticities in units of 0.00002, maximum luminance in cd/m2, minimum
/// luminance in 0.0001 cd/m2, light levels in cd/m2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HdrMetadata {
	/// Red, green and blue display primaries as `(x, y)`.
	pub primaries: [(u32, u32); 3],
	/// White point `(x, y)`.
	pub white_point: (u32, u32),
	/// Maximum mastering display luminance, cd/m2.
	pub max_display_mastering_luminance: u32,
	/// Minimum mastering display luminance, 0.0001 cd/m2.
	pub min_display_mastering_luminance: u32,
	/// Maximum content light level, cd/m2.
	pub max_cll: u32,
	/// Maximum frame-average light level, cd/m2.
	pub max_fall: u32,
}

impl HdrMetadata {
	/// gamescope drops metadata that is "garbage": no light levels or no white
	/// point (`wlserver.cpp:1036`).
	pub(crate) fn is_valid(&self) -> bool {
		self.max_cll != 0 && self.max_fall != 0 && (self.white_point.0 != 0 || self.white_point.1 != 0)
	}
}

/// A swapchain object of either protocol variant.
#[derive(Debug, Clone)]
pub enum SwapchainObject {
	/// `gamescope_swapchain`.
	Gamescope(GamescopeSwapchain),
	/// `ferriscope_swapchain_v1`.
	Ferriscope(FerriscopeSwapchainV1),
}

impl SwapchainObject {
	/// The wire object id.
	pub fn id(&self) -> ObjectId {
		match self {
			Self::Gamescope(object) => object.id(),
			Self::Ferriscope(object) => object.id(),
		}
	}

	/// `refresh_cycle`: the refresh period in nanoseconds.
	fn refresh_cycle(&self, nanos: u64) {
		let (hi, lo) = ((nanos >> 32) as u32, nanos as u32);
		match self {
			Self::Gamescope(object) => object.refresh_cycle(hi, lo),
			Self::Ferriscope(object) => object.refresh_cycle(hi, lo),
		}
	}

	/// `frame_ready`: the swapchain's FIFO queue has room (native protocol only;
	/// a gamescope layer has no such event).
	fn frame_ready(&self) {
		if let Self::Ferriscope(object) = self {
			object.frame_ready();
		}
	}

	/// `retired`: the swapchain no longer provides the window's content.
	pub fn retired(&self) {
		match self {
			Self::Gamescope(object) => object.retired(),
			Self::Ferriscope(object) => object.retired(),
		}
	}
}

/// `set_present_time`: when the application wants its next commit shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentTime {
	/// The application's presentation id.
	pub present_id: u32,
	/// Desired present time, nanoseconds on `CLOCK_MONOTONIC`; 0 means as soon
	/// as possible.
	pub desired_ns: u64,
}

/// What the layer asked for the next commit of a surface: the present mode and
/// `set_present_time` it sent since the surface last committed
/// (`PrepareCommit`, `wlserver.cpp:132`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CommitHints {
	/// `VkPresentModeKHR` from `set_present_mode`.
	pub present_mode: Option<u32>,
	/// `set_present_time`.
	pub present_time: Option<PresentTime>,
}

/// The WSI state of one `wl_surface`: gamescope's per-surface
/// `swapchain_feedback`, `oCurrentPresentMode` and `gamescope_swapchains`.
#[derive(Debug, Default)]
struct SwapchainSurface {
	feedback: Option<SwapchainFeedback>,
	hints: CommitHints,
	objects: Vec<SwapchainObject>,
	refresh_cycle_sent: u64,
}

type SurfaceSwapchainCell = Mutex<SwapchainSurface>;

/// Run `f` on `surface`'s WSI state, creating it on first use.
fn with_swapchain_surface<T>(surface: &WlSurface, f: impl FnOnce(&mut SwapchainSurface) -> T) -> T {
	with_states(surface, |states| {
		states
			.data_map
			.insert_if_missing_threadsafe(|| SurfaceSwapchainCell::new(SwapchainSurface::default()));
		let cell = states
			.data_map
			.get::<SurfaceSwapchainCell>()
			.expect("inserted just above");
		f(&mut cell.lock().unwrap())
	})
}

/// The swapchain feedback the layer last reported for `surface`.
pub fn swapchain_feedback(surface: &WlSurface) -> Option<SwapchainFeedback> {
	with_swapchain_surface(surface, |wsi| wsi.feedback.clone())
}

/// The present mode the layer set for `surface`'s next commit, without taking
/// it.
pub fn pending_present_mode(surface: &WlSurface) -> Option<u32> {
	with_swapchain_surface(surface, |wsi| wsi.hints.present_mode)
}

/// Take the hints the layer set for `surface`'s next commit; they apply to one
/// commit only.
pub fn take_commit_hints(surface: &WlSurface) -> CommitHints {
	with_swapchain_surface(surface, |wsi| std::mem::take(&mut wsi.hints))
}

/// Tell every swapchain on `surface` the refresh period, once per change
/// (`wlserver_refresh_cycle`, `wlserver.cpp:1676`).
pub fn send_refresh_cycle(surface: &WlSurface, nanos: u64) {
	with_swapchain_surface(surface, |wsi| {
		if wsi.refresh_cycle_sent == nanos {
			return;
		}
		wsi.refresh_cycle_sent = nanos;
		for object in &wsi.objects {
			object.refresh_cycle(nanos);
		}
	});
}

/// Tell every swapchain on `surface` that its FIFO queue has room.
pub fn send_frame_ready(surface: &WlSurface) {
	with_swapchain_surface(surface, |wsi| {
		for object in &wsi.objects {
			object.frame_ready();
		}
	});
}

/// Drop `id` from `surface`'s swapchain list.
pub fn forget_swapchain(surface: &WlSurface, id: &ObjectId) {
	with_swapchain_surface(surface, |wsi| wsi.objects.retain(|object| &object.id() != id));
}

/// User data of the `gamescope_xwayland` global and objects.
pub struct XwaylandData;

/// User data of the swapchain factory globals and objects.
pub struct SwapchainFactoryData;

/// User data of a swapchain object: the surface it was created for.
pub struct SwapchainData {
	surface: WlSurface,
}

/// The three globals, kept so they live as long as the compositor.
pub struct SwapchainGlobals {
	_xwayland: GlobalId,
	_gamescope: GlobalId,
	_ferriscope: GlobalId,
}

impl SwapchainGlobals {
	/// Advertise `ferriscope_swapchain_factory_v1` and, for a stock gamescope
	/// layer, `gamescope_xwayland` and `gamescope_swapchain_factory_v2`.
	///
	/// Always advertised, like gamescope (`wlserver.cpp:1536`): the layers probe
	/// the factory global to learn they are on a compositor that speaks it.
	pub fn new(display: &DisplayHandle) -> Self {
		Self {
			_xwayland: display.create_global::<FerriscopeState, GamescopeXwayland, XwaylandData>(1, XwaylandData),
			_gamescope: display.create_global::<FerriscopeState, GamescopeSwapchainFactoryV2, SwapchainFactoryData>(
				1,
				SwapchainFactoryData,
			),
			_ferriscope: display.create_global::<FerriscopeState, FerriscopeSwapchainFactoryV1, SwapchainFactoryData>(
				1,
				SwapchainFactoryData,
			),
		}
	}
}

/// The `ferriscope_swapchain_factory_v1` objects clients hold, to tell them
/// what changes (`hdr_available`, `xwayland_server`).
#[derive(Default)]
pub struct FactoryRegistry {
	factories: Vec<FerriscopeSwapchainFactoryV1>,
}

impl FactoryRegistry {
	/// Remember a newly bound factory and tell it the current capabilities.
	pub(crate) fn bind(&mut self, factory: FerriscopeSwapchainFactoryV1, hdr: bool, xwayland_server: Option<u32>) {
		factory.hdr_available(u32::from(hdr));
		factory.xwayland_server(xwayland_server.unwrap_or(0));
		self.factories.retain(Resource::is_alive);
		self.factories.push(factory);
	}

	/// The Xwayland server's identity became known.
	pub fn announce_xwayland_server(&mut self, server_id: u32) {
		self.factories.retain(Resource::is_alive);
		for factory in &self.factories {
			factory.xwayland_server(server_id);
		}
	}
}

impl GlobalDispatch2<GamescopeXwayland, FerriscopeState> for XwaylandData {
	fn bind(
		&self,
		_state: &mut FerriscopeState,
		_handle: &DisplayHandle,
		_client: &Client,
		resource: New<GamescopeXwayland>,
		data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		data_init.init(resource, XwaylandData);
	}
}

impl Dispatch2<GamescopeXwayland, FerriscopeState> for XwaylandData {
	fn request(
		&self,
		state: &mut FerriscopeState,
		_client: &Client,
		_resource: &GamescopeXwayland,
		request: gamescope_xwayland::Request,
		_handle: &DisplayHandle,
		_data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		match request {
			gamescope_xwayland::Request::Destroy => {},
			// `wlserver.cpp:883`: the request names no Xwayland server; it is
			// "server 0", and there is one Xwayland here.
			gamescope_xwayland::Request::OverrideWindowContent { surface, x11_window } => {
				state.override_window_content(None, None, surface, x11_window);
			},
		}
	}
}

impl GlobalDispatch2<GamescopeSwapchainFactoryV2, FerriscopeState> for SwapchainFactoryData {
	fn bind(
		&self,
		_state: &mut FerriscopeState,
		_handle: &DisplayHandle,
		_client: &Client,
		resource: New<GamescopeSwapchainFactoryV2>,
		data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		data_init.init(resource, SwapchainFactoryData);
	}
}

impl GlobalDispatch2<FerriscopeSwapchainFactoryV1, FerriscopeState> for SwapchainFactoryData {
	fn bind(
		&self,
		state: &mut FerriscopeState,
		_handle: &DisplayHandle,
		_client: &Client,
		resource: New<FerriscopeSwapchainFactoryV1>,
		data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		let factory = data_init.init(resource, SwapchainFactoryData);
		state.bind_swapchain_factory(factory);
	}
}

/// Register a new swapchain object on `surface`
/// (`gamescope_swapchain_factory_v2_create_swapchain`, `wlserver.cpp:1100`).
fn register_swapchain(surface: &WlSurface, object: SwapchainObject) {
	with_swapchain_surface(surface, |wsi| {
		if !wsi.objects.is_empty() {
			tracing::warn!("create_swapchain: the surface already has a swapchain");
		}
		wsi.objects.push(object);
	});
}

impl Dispatch2<GamescopeSwapchainFactoryV2, FerriscopeState> for SwapchainFactoryData {
	fn request(
		&self,
		_state: &mut FerriscopeState,
		_client: &Client,
		_resource: &GamescopeSwapchainFactoryV2,
		request: gamescope_swapchain_factory_v2::Request,
		_handle: &DisplayHandle,
		data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		match request {
			gamescope_swapchain_factory_v2::Request::Destroy => {},
			gamescope_swapchain_factory_v2::Request::CreateSwapchain { surface, callback } => {
				let object = data_init.init(
					callback,
					SwapchainData {
						surface: surface.clone(),
					},
				);
				register_swapchain(&surface, SwapchainObject::Gamescope(object));
			},
		}
	}
}

impl Dispatch2<FerriscopeSwapchainFactoryV1, FerriscopeState> for SwapchainFactoryData {
	fn request(
		&self,
		_state: &mut FerriscopeState,
		_client: &Client,
		_resource: &FerriscopeSwapchainFactoryV1,
		request: ferriscope_swapchain_factory_v1::Request,
		_handle: &DisplayHandle,
		data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		match request {
			ferriscope_swapchain_factory_v1::Request::Destroy => {},
			ferriscope_swapchain_factory_v1::Request::CreateSwapchain { surface, callback } => {
				let object = data_init.init(
					callback,
					SwapchainData {
						surface: surface.clone(),
					},
				);
				register_swapchain(&surface, SwapchainObject::Ferriscope(object));
			},
		}
	}
}

/// `swapchain_feedback` (`wlserver.cpp:986`): replaces the surface's feedback,
/// dropping any HDR metadata that belonged to the previous swapchain.
fn store_feedback(surface: &WlSurface, feedback: SwapchainFeedback) {
	tracing::debug!(
		image_count = feedback.image_count,
		vk_format = feedback.vk_format,
		vk_colorspace = feedback.vk_colorspace,
		vk_composite_alpha = feedback.vk_composite_alpha,
		vk_pre_transform = feedback.vk_pre_transform,
		vk_clipped = feedback.vk_clipped,
		engine = feedback.engine_name,
		"swapchain_feedback"
	);
	with_swapchain_surface(surface, |wsi| wsi.feedback = Some(feedback));
}

/// `set_hdr_metadata` (`wlserver.cpp:1019`): only meaningful after a
/// `swapchain_feedback`, and invalid metadata is ignored.
fn store_hdr_metadata(surface: &WlSurface, metadata: HdrMetadata) {
	if !metadata.is_valid() {
		return;
	}
	tracing::debug!(
		max_cll = metadata.max_cll,
		max_fall = metadata.max_fall,
		max_luminance = metadata.max_display_mastering_luminance,
		min_luminance = metadata.min_display_mastering_luminance,
		primaries = ?metadata.primaries,
		white_point = ?metadata.white_point,
		"set_hdr_metadata"
	);
	with_swapchain_surface(surface, |wsi| match wsi.feedback.as_mut() {
		Some(feedback) => feedback.hdr_metadata = Some(metadata),
		None => tracing::warn!("set_hdr_metadata with no swapchain_feedback"),
	});
}

/// Handle one swapchain request; the two protocol variants have identical
/// requests.
macro_rules! dispatch_swapchain {
	($state:expr, $data:expr, $object:expr, $request:expr, $module:ident, $server_id:ident) => {{
		use $module::Request;
		match $request {
			Request::Destroy => {},
			Request::OverrideWindowContent { $server_id, x11_window } => {
				$state.override_window_content(Some($object), Some($server_id), $data.surface.clone(), x11_window);
			},
			Request::SwapchainFeedback {
				image_count,
				vk_format,
				vk_colorspace,
				vk_composite_alpha,
				vk_pre_transform,
				vk_clipped,
				vk_engine_name,
			} => {
				store_feedback(
					&$data.surface,
					SwapchainFeedback {
						image_count,
						vk_format,
						vk_colorspace,
						vk_composite_alpha,
						vk_pre_transform,
						vk_clipped: vk_clipped != 0,
						engine_name: vk_engine_name,
						hdr_metadata: None,
					},
				);
				$state.swapchain_feedback_received(&$data.surface);
			},
			Request::SetPresentMode { vk_present_mode } => {
				with_swapchain_surface(&$data.surface, |wsi| wsi.hints.present_mode = Some(vk_present_mode));
			},
			Request::SetHdrMetadata {
				display_primary_red_x,
				display_primary_red_y,
				display_primary_green_x,
				display_primary_green_y,
				display_primary_blue_x,
				display_primary_blue_y,
				white_point_x,
				white_point_y,
				max_display_mastering_luminance,
				min_display_mastering_luminance,
				max_cll,
				max_fall,
			} => store_hdr_metadata(
				&$data.surface,
				HdrMetadata {
					primaries: [
						(display_primary_red_x, display_primary_red_y),
						(display_primary_green_x, display_primary_green_y),
						(display_primary_blue_x, display_primary_blue_y),
					],
					white_point: (white_point_x, white_point_y),
					max_display_mastering_luminance,
					min_display_mastering_luminance,
					max_cll,
					max_fall,
				},
			),
			Request::SetPresentTime {
				present_id,
				desired_present_time_hi,
				desired_present_time_lo,
			} => {
				let desired_ns = (u64::from(desired_present_time_hi) << 32) | u64::from(desired_present_time_lo);
				with_swapchain_surface(&$data.surface, |wsi| {
					wsi.hints.present_time = Some(PresentTime { present_id, desired_ns });
				});
			},
		}
	}};
}

impl Dispatch2<GamescopeSwapchain, FerriscopeState> for SwapchainData {
	fn request(
		&self,
		state: &mut FerriscopeState,
		_client: &Client,
		resource: &GamescopeSwapchain,
		request: gamescope_swapchain::Request,
		_handle: &DisplayHandle,
		_data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		dispatch_swapchain!(
			state,
			self,
			SwapchainObject::Gamescope(resource.clone()),
			request,
			gamescope_swapchain,
			gamescope_xwayland_server_id
		);
	}

	fn destroyed(&self, state: &mut FerriscopeState, _client: ClientId, resource: &GamescopeSwapchain) {
		state.swapchain_destroyed(&self.surface, &resource.id());
	}
}

impl Dispatch2<FerriscopeSwapchainV1, FerriscopeState> for SwapchainData {
	fn request(
		&self,
		state: &mut FerriscopeState,
		_client: &Client,
		resource: &FerriscopeSwapchainV1,
		request: ferriscope_swapchain_v1::Request,
		_handle: &DisplayHandle,
		_data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		dispatch_swapchain!(
			state,
			self,
			SwapchainObject::Ferriscope(resource.clone()),
			request,
			ferriscope_swapchain_v1,
			xwayland_server_id
		);
	}

	fn destroyed(&self, state: &mut FerriscopeState, _client: ClientId, resource: &FerriscopeSwapchainV1) {
		state.swapchain_destroyed(&self.surface, &resource.id());
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn metadata() -> HdrMetadata {
		HdrMetadata {
			primaries: [(34000, 16000), (13250, 34500), (7500, 3000)],
			white_point: (15635, 16450),
			max_display_mastering_luminance: 1000,
			min_display_mastering_luminance: 50,
			max_cll: 1000,
			max_fall: 400,
		}
	}

	#[test]
	fn valid_hdr_metadata_is_kept() {
		assert!(metadata().is_valid());
	}

	#[test]
	fn hdr_metadata_without_light_levels_or_white_point_is_garbage() {
		// `wlserver.cpp:1036`
		let no_cll = HdrMetadata {
			max_cll: 0,
			..metadata()
		};
		let no_fall = HdrMetadata {
			max_fall: 0,
			..metadata()
		};
		let no_white = HdrMetadata {
			white_point: (0, 0),
			..metadata()
		};
		let half_white = HdrMetadata {
			white_point: (0, 1),
			..metadata()
		};
		assert!(!no_cll.is_valid());
		assert!(!no_fall.is_valid());
		assert!(!no_white.is_valid());
		assert!(half_white.is_valid());
	}
}
