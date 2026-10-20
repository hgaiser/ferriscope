//! Layer state: the Wayland connection to ferriscope, and the maps from Vulkan
//! handles to what the layer knows about them.
//!
//! Lock rules: every map hands out an `Arc` and drops its lock before the
//! caller does anything else, so no map lock is held across a Wayland call.
//! Wayland event handlers write only into a swapchain's [`SwapchainShared`]
//! (reached through the proxy's user data) and into [`Caps`], never into a map.

use std::{
	collections::HashMap,
	sync::{
		Arc, Mutex, MutexGuard, OnceLock, RwLock,
		atomic::{AtomicBool, AtomicU32, Ordering},
	},
	time::{Duration, Instant},
};

use ash::vk;
use wayland_client::{
	Connection, Dispatch, EventQueue, QueueHandle,
	globals::GlobalListContents,
	protocol::{wl_compositor::WlCompositor, wl_registry::WlRegistry, wl_surface::WlSurface},
};

use crate::{
	bypass::{Bypass, Rect, can_bypass},
	config::Flags,
	dispatch::{DeviceDispatch, InstanceDispatch},
	proto::{
		ferriscope_swapchain_factory_v1::{self, FerriscopeSwapchainFactoryV1},
		ferriscope_swapchain_v1::{self, FerriscopeSwapchainV1},
	},
	timing::{PastTiming, SwapchainShared, join},
	xcb::X11,
};

/// A mutex that survives a panicked holder.
pub trait MutexExt<T> {
	fn force_lock(&self) -> MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
	fn force_lock(&self) -> MutexGuard<'_, T> {
		self.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
	}
}

// ---------------------------------------------------------------------------
// The compositor connection
// ---------------------------------------------------------------------------

/// What the compositor said about itself (`ferriscope_swapchain_factory_v1`'s
/// events), live.
#[derive(Debug, Default)]
pub struct Caps {
	hdr_available: AtomicBool,
	xwayland_server: AtomicU32,
}

impl Caps {
	/// Whether HDR surface formats may be offered.
	pub fn hdr_available(&self) -> bool {
		self.hdr_available.load(Ordering::Relaxed)
	}

	/// The id of the compositor's Xwayland server, 0 for none.
	pub fn xwayland_server(&self) -> u32 {
		self.xwayland_server.load(Ordering::Relaxed)
	}
}

/// The dispatch state of the layer's event queues.
pub struct WaylandState {
	pub caps: Arc<Caps>,
}

impl Dispatch<WlRegistry, GlobalListContents> for WaylandState {
	fn event(
		_: &mut Self,
		_: &WlRegistry,
		_: wayland_client::protocol::wl_registry::Event,
		_: &GlobalListContents,
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
	}
}

impl Dispatch<WlCompositor, ()> for WaylandState {
	fn event(
		_: &mut Self,
		_: &WlCompositor,
		_: wayland_client::protocol::wl_compositor::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
	}
}

impl Dispatch<WlSurface, ()> for WaylandState {
	fn event(
		_: &mut Self,
		_: &WlSurface,
		_: wayland_client::protocol::wl_surface::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
	}
}

impl Dispatch<FerriscopeSwapchainFactoryV1, ()> for WaylandState {
	fn event(
		state: &mut Self,
		_: &FerriscopeSwapchainFactoryV1,
		event: ferriscope_swapchain_factory_v1::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		match event {
			ferriscope_swapchain_factory_v1::Event::HdrAvailable { available } => {
				crate::log_info!("compositor hdr_available={available}");
				state.caps.hdr_available.store(available != 0, Ordering::Relaxed);
			},
			ferriscope_swapchain_factory_v1::Event::XwaylandServer { server_id } => {
				crate::log_info!("compositor xwayland_server={server_id}");
				state.caps.xwayland_server.store(server_id, Ordering::Relaxed);
			},
		}
	}
}

/// The user data of a swapchain proxy: its compositor-side state.
impl Dispatch<FerriscopeSwapchainV1, Arc<SwapchainShared>> for WaylandState {
	fn event(
		_: &mut Self,
		_: &FerriscopeSwapchainV1,
		event: ferriscope_swapchain_v1::Event,
		shared: &Arc<SwapchainShared>,
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
		use ferriscope_swapchain_v1::Event;
		match event {
			Event::RefreshCycle {
				refresh_cycle_hi,
				refresh_cycle_lo,
			} => {
				shared.on_refresh_cycle(refresh_cycle_hi, refresh_cycle_lo);
				crate::log_info!(
					"swapchain refresh cycle {:.2} ms",
					join(refresh_cycle_hi, refresh_cycle_lo) as f64 / 1e6
				);
			},
			Event::FrameReady => shared.on_frame_ready(),
			Event::Retired => {
				shared.on_retired();
				crate::log_info!("swapchain retired by the compositor");
			},
			Event::PastPresentTiming {
				present_id,
				desired_present_time_hi,
				desired_present_time_lo,
				actual_present_time_hi,
				actual_present_time_lo,
				earliest_present_time_hi,
				earliest_present_time_lo,
				present_margin_hi,
				present_margin_lo,
			} => shared.on_past_present_timing(PastTiming {
				present_id,
				desired_present_time: join(desired_present_time_hi, desired_present_time_lo),
				actual_present_time: join(actual_present_time_hi, actual_present_time_lo),
				earliest_present_time: join(earliest_present_time_hi, earliest_present_time_lo),
				present_margin: join(present_margin_hi, present_margin_lo),
			}),
		}
	}
}

/// A live connection to ferriscope, with the protocol objects the layer needs.
pub struct Wayland {
	pub connection: Connection,
	/// `wl_compositor`, bound on the layer's own connection only: a native
	/// Wayland surface already exists.
	pub compositor: Option<WlCompositor>,
	pub factory: FerriscopeSwapchainFactoryV1,
	pub qh: QueueHandle<WaylandState>,
	pub caps: Arc<Caps>,
	queue: Mutex<EventQueue<WaylandState>>,
	dead: AtomicBool,
}

impl Wayland {
	pub fn new(
		connection: Connection,
		compositor: Option<WlCompositor>,
		factory: FerriscopeSwapchainFactoryV1,
		queue: EventQueue<WaylandState>,
		caps: Arc<Caps>,
	) -> Self {
		Self {
			connection,
			compositor,
			factory,
			qh: queue.handle(),
			caps,
			queue: Mutex::new(queue),
			dead: AtomicBool::new(false),
		}
	}

	pub fn is_dead(&self) -> bool {
		self.dead.load(Ordering::Relaxed)
	}

	fn kill(&self, what: &str, error: &dyn std::fmt::Display) {
		if !self.dead.swap(true, Ordering::Relaxed) {
			crate::log_warn!("Wayland {what} failed: {error}; the compositor connection is lost");
		}
	}

	/// Block until the compositor has answered everything sent so far, so the
	/// events that follow a bind have arrived.
	pub fn roundtrip(&self) -> bool {
		let mut state = WaylandState {
			caps: self.caps.clone(),
		};
		match self.queue.force_lock().roundtrip(&mut state) {
			Ok(_) => true,
			Err(error) => {
				self.kill("roundtrip", &error);
				false
			},
		}
	}

	/// Flush requests to the compositor.
	pub fn flush(&self) {
		if let Err(error) = self.connection.flush() {
			self.kill("flush", &error);
		}
	}

	/// Take in what the compositor has sent, without blocking (gamescope's
	/// `waylandPumpEvents`). Returns `false` once the connection is lost.
	pub fn pump(&self) -> bool {
		self.pump_for(Duration::ZERO)
	}

	/// [`Self::pump`], but wait up to `timeout` for the compositor to say
	/// something if it has not already.
	pub fn pump_for(&self, timeout: Duration) -> bool {
		if self.is_dead() {
			return false;
		}
		let mut queue = if timeout.is_zero() {
			// Another thread pumping already is as good as this one.
			let Ok(queue) = self.queue.try_lock() else {
				return true;
			};
			queue
		} else {
			self.queue.force_lock()
		};
		let mut state = WaylandState {
			caps: self.caps.clone(),
		};
		for _ in 0..8 {
			if let Err(error) = queue.dispatch_pending(&mut state) {
				self.kill("dispatch", &error);
				return false;
			}
			let Some(guard) = self.connection.prepare_read() else {
				// Events are queued for dispatch; do that first.
				continue;
			};
			if fd_readable(&guard, timeout)
				&& let Err(error) = guard.read()
				&& !matches!(&error, wayland_client::backend::WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock)
			{
				self.kill("read", &error);
				return false;
			}
			break;
		}
		if let Err(error) = queue.dispatch_pending(&mut state) {
			self.kill("dispatch", &error);
			return false;
		}
		drop(queue);
		self.flush();
		!self.is_dead()
	}
}

/// Whether the connection's socket has data, without waiting.
fn fd_readable(guard: &wayland_client::backend::ReadEventsGuard, timeout: Duration) -> bool {
	use std::os::fd::AsRawFd;
	let mut poll = libc::pollfd {
		fd: guard.connection_fd().as_raw_fd(),
		events: libc::POLLIN,
		revents: 0,
	};
	// SAFETY: one valid pollfd.
	unsafe { libc::poll(&mut poll, 1, timeout.as_millis().min(1000) as i32) > 0 }
}

// ---------------------------------------------------------------------------
// Handle keys
// ---------------------------------------------------------------------------

/// Dispatch-table key for the instance map (from `VkInstance`/`VkPhysicalDevice`).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct InstanceKey(pub(crate) usize);

/// Dispatch-table key for the device map (from `VkDevice`/`VkQueue`).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceKey(pub(crate) usize);

// ---------------------------------------------------------------------------
// Per-instance, per-device state
// ---------------------------------------------------------------------------

/// What the application told Vulkan about itself.
pub struct AppInfo {
	pub engine_name: String,
	pub flags: Flags,
}

pub struct InstanceData {
	pub dispatch: InstanceDispatch,
	/// The connection to ferriscope; `None` when the layer is passing through
	/// (not under ferriscope, or the connection failed).
	pub wayland: Option<Arc<Wayland>>,
	pub app: AppInfo,
}

pub struct DeviceData {
	pub dispatch: DeviceDispatch,
	pub instance: InstanceKey,
	pub physical_device: vk::PhysicalDevice,
	/// Whether `VK_EXT_swapchain_maintenance1` was enabled on the device, which
	/// the present-mode control needs.
	pub has_maintenance1: bool,
}

// ---------------------------------------------------------------------------
// Per-surface state
// ---------------------------------------------------------------------------

/// A surface the layer manages: an X11 window presented through a Wayland
/// surface of its own, or a native Wayland surface (gamescope's
/// `GamescopeSurfaceData`).
pub struct SurfaceData {
	/// The connection the surface's swapchain objects live on: the layer's own
	/// for X11, the application's for native Wayland.
	pub wayland: Arc<Wayland>,
	pub wl_surface: WlSurface,
	/// Whether the layer made `wl_surface` (and so destroys it).
	pub owns_wl_surface: bool,
	/// The plain XCB surface for the same window, used when the bypass is not
	/// safe; null for native Wayland.
	pub fallback: vk::SurfaceKHR,
	/// The X11 connection and window; `None` for native Wayland.
	pub x11: Option<(X11, u32)>,
	pub flags: Flags,
	/// The last bypass decision and when it was made.
	last_check: Mutex<Option<(Instant, bool, Bypass)>>,
}

impl SurfaceData {
	pub fn new(
		wayland: Arc<Wayland>,
		wl_surface: WlSurface,
		owns_wl_surface: bool,
		fallback: vk::SurfaceKHR,
		x11: Option<(X11, u32)>,
		flags: Flags,
	) -> Self {
		Self {
			wayland,
			wl_surface,
			owns_wl_surface,
			fallback,
			x11,
			flags,
			last_check: Mutex::new(None),
		}
	}

	pub fn is_wayland(&self) -> bool {
		self.x11.is_none()
	}

	/// The X11 window, 0 for native Wayland.
	pub fn window(&self) -> u32 {
		self.x11.map_or(0, |(_, window)| window)
	}

	/// Whether HDR surface formats may be offered (`shouldExposeHDR`).
	pub fn should_expose_hdr(&self) -> bool {
		self.wayland.caps.hdr_available() && !self.flags.contains(Flags::DISABLE_HDR)
	}

	/// Whether the window can present through the Wayland surface now
	/// (`canBypassXWayland`). A decision younger than `max_age` for the same
	/// colour space is reused: the check is several X11 round trips.
	pub fn check_bypass(&self, hdr_colourspace: bool, max_age: Duration) -> Bypass {
		let Some((x11, window)) = self.x11 else {
			return Bypass {
				allowed: true,
				rect: None,
			};
		};
		let mut last = self.last_check.force_lock();
		if let Some((at, hdr, bypass)) = *last
			&& hdr == hdr_colourspace
			&& at.elapsed() < max_age
		{
			return bypass;
		}
		let bypass = can_bypass(&x11, window, hdr_colourspace, self.flags);
		if !bypass.allowed {
			crate::log_debug!("window 0x{window:x} cannot bypass Xwayland (hdr={hdr_colourspace})");
		}
		*last = Some((Instant::now(), hdr_colourspace, bypass));
		bypass
	}

	pub fn can_bypass(&self, hdr_colourspace: bool) -> bool {
		self.check_bypass(hdr_colourspace, Duration::ZERO).allowed
	}

	/// The window's current rectangle, fresh.
	pub fn window_rect(&self) -> Option<Rect> {
		use crate::bypass::WindowQuery;
		let (x11, window) = self.x11?;
		x11.rect(window)
	}

	/// The surface the driver should be given: the Wayland one when the bypass
	/// is allowed, else the plain XCB surface.
	pub fn driver_surface(&self, surface: vk::SurfaceKHR, bypass: bool) -> vk::SurfaceKHR {
		if bypass || self.fallback == vk::SurfaceKHR::null() {
			surface
		} else {
			self.fallback
		}
	}
}

// ---------------------------------------------------------------------------
// Per-swapchain state
// ---------------------------------------------------------------------------

pub struct SwapchainData {
	/// The surface the application created the swapchain on (the Wayland-side
	/// one, whatever the driver was given).
	pub surface: vk::SurfaceKHR,
	/// The compositor-side swapchain; `None` when presenting through the
	/// fallback surface.
	pub object: Option<FerriscopeSwapchainV1>,
	pub wayland: Arc<Wayland>,
	pub shared: Arc<SwapchainShared>,
	pub is_wayland: bool,
	/// Whether the swapchain presents through the Wayland surface rather than
	/// Xwayland.
	pub bypassing: bool,
	pub hdr_colourspace: bool,
	/// The present mode the application asked for.
	pub present_mode: vk::PresentModeKHR,
	pub extent: vk::Extent2D,
	/// The Xwayland server id named in `override_window_content`.
	pub server_id: u32,
	/// The X11 window named in `override_window_content`, 0 for native Wayland.
	pub window: u32,
}

impl Drop for SwapchainData {
	fn drop(&mut self) {
		if let Some(object) = self.object.take() {
			object.destroy();
			self.wayland.flush();
		}
	}
}

// ---------------------------------------------------------------------------
// The maps
// ---------------------------------------------------------------------------

macro_rules! handle_map {
	($map:ident, $key:ty, $value:ty, $insert:ident, $remove:ident, $get:ident) => {
		fn $map() -> &'static RwLock<HashMap<$key, Arc<$value>>> {
			static MAP: OnceLock<RwLock<HashMap<$key, Arc<$value>>>> = OnceLock::new();
			MAP.get_or_init(Default::default)
		}

		pub fn $insert(key: $key, value: $value) -> Arc<$value> {
			let value = Arc::new(value);
			$map()
				.write()
				.unwrap_or_else(|poisoned| poisoned.into_inner())
				.insert(key, value.clone());
			value
		}

		pub fn $remove(key: $key) -> Option<Arc<$value>> {
			$map()
				.write()
				.unwrap_or_else(|poisoned| poisoned.into_inner())
				.remove(&key)
		}

		pub fn $get(key: $key) -> Option<Arc<$value>> {
			$map()
				.read()
				.unwrap_or_else(|poisoned| poisoned.into_inner())
				.get(&key)
				.cloned()
		}
	};
}

handle_map!(
	instance_map,
	InstanceKey,
	InstanceData,
	insert_instance,
	remove_instance,
	get_instance
);
handle_map!(
	device_map,
	DeviceKey,
	DeviceData,
	insert_device,
	remove_device,
	get_device
);
handle_map!(
	surface_map,
	u64,
	SurfaceData,
	insert_surface,
	remove_surface,
	get_surface
);
handle_map!(
	swapchain_map,
	u64,
	SwapchainData,
	insert_swapchain,
	remove_swapchain,
	get_swapchain
);
