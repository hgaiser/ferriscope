//! Surface creation and surface queries.
//!
//! An X11 window the layer manages gets a Wayland surface of its own on
//! ferriscope's socket (the "bypass" surface, which the application sees as
//! its `VkSurfaceKHR`) and a plain XCB surface for the same window (the
//! "fallback", used while the bypass is unsafe). A native Wayland surface is
//! tracked as it is. Ports gamescope's `VkInstanceOverrides`.

use std::{marker::PhantomData, time::Duration};

use ash::vk::{self, Handle as _};
use wayland_client::Proxy;

use crate::{
	colour::{HDR_SURFACE_FORMATS, append_extra, write_array},
	config,
	dispatch::*,
	instance::{connect_foreign, move_to_default_queue},
	state::{InstanceKey, SurfaceData, get_instance, get_surface, insert_surface, remove_surface},
	xcb::{X11, xlib_to_xcb_connection},
};

/// The root-window property that identifies ferriscope's Xwayland server;
/// compared with `ferriscope_swapchain_factory_v1.xwayland_server`.
const SERVER_ID_PROPERTY: &str = "FERRISCOPE_XWAYLAND_SERVER_ID";

fn surface_key(surface: VkSurface) -> u64 {
	surface.as_raw()
}

// ---------------------------------------------------------------------------
// Creation
// ---------------------------------------------------------------------------

pub unsafe extern "C" fn create_wayland_surface(
	instance: VkInstance,
	p_create_info: *const VkWaylandSurfaceCreateInfoKHR,
	p_allocator: *const VkAllocationCallbacks,
	p_surface: *mut VkSurface,
) -> VkResult {
	unsafe {
		let key = instance_key_of(instance);
		let Some(data) = get_instance(key) else {
			return VK_ERROR_INITIALIZATION_FAILED;
		};
		let Some(next) = data.dispatch.create_wayland_surface else {
			return VK_ERROR_FEATURE_NOT_PRESENT;
		};
		let result = next(instance, p_create_info, p_allocator, p_surface);
		if result != VK_SUCCESS || data.wayland.is_none() {
			return result;
		}

		// The application's surface lives on its own connection, so the
		// swapchain factory is bound there.
		let create_info = &*p_create_info;
		match connect_foreign(create_info.display, create_info.surface) {
			Some((wayland, wl_surface)) => {
				crate::log_info!("native Wayland surface tracked");
				insert_surface(
					surface_key(*p_surface),
					SurfaceData::new(wayland, wl_surface, false, VkSurface::null(), None, data.app.flags),
				);
			},
			None => crate::log_info!("native Wayland surface not tracked: the compositor is not ferriscope"),
		}
		VK_SUCCESS
	}
}

/// A plain XCB surface through the next layer.
unsafe fn plain_xcb_surface(
	instance: VkInstance,
	key: InstanceKey,
	info: *const VkXcbSurfaceCreateInfoKHR,
	allocator: *const VkAllocationCallbacks,
	out: *mut VkSurface,
) -> VkResult {
	unsafe {
		match get_instance(key).and_then(|data| data.dispatch.create_xcb_surface) {
			Some(next) => next(instance, info, allocator, out),
			None => VK_ERROR_FEATURE_NOT_PRESENT,
		}
	}
}

pub unsafe extern "C" fn create_xcb_surface(
	instance: VkInstance,
	p_create_info: *const VkXcbSurfaceCreateInfoKHR,
	p_allocator: *const VkAllocationCallbacks,
	p_surface: *mut VkSurface,
) -> VkResult {
	unsafe {
		let key = instance_key_of(instance);
		let create_info = &*p_create_info;
		let passthrough = || plain_xcb_surface(instance, key, p_create_info, p_allocator, p_surface);

		let Some(data) = get_instance(key) else {
			return VK_ERROR_INITIALIZATION_FAILED;
		};
		let (Some(wayland), Some(x11)) = (data.wayland.clone(), X11::new(create_info.connection)) else {
			return passthrough();
		};
		let window = create_info.window;

		// The window must be on ferriscope's own Xwayland.
		let server = wayland.caps.xwayland_server();
		let theirs = x11.root_cardinal(window, SERVER_ID_PROPERTY);
		if server == 0 || theirs != Some(server) {
			crate::log_info!("window 0x{window:x} is not on ferriscope's Xwayland (ours {server}, theirs {theirs:?})");
			return passthrough();
		}
		let (Some(compositor), true) = (wayland.compositor.as_ref(), !wayland.is_dead()) else {
			return passthrough();
		};

		crate::log_info!("creating surface for window 0x{window:x}");
		let wl_surface = compositor.create_surface(&wayland.qh, ());
		wayland.flush();
		let display = wayland.connection.backend().display_ptr() as *mut std::ffi::c_void;
		let wl_surface_ptr = wl_surface.id().as_ptr() as *mut std::ffi::c_void;
		// The driver dispatches the default queue; the layer's is its own.
		move_to_default_queue(wl_surface_ptr);

		let wayland_info = VkWaylandSurfaceCreateInfoKHR {
			s_type: vk::StructureType::WAYLAND_SURFACE_CREATE_INFO_KHR,
			p_next: std::ptr::null(),
			flags: vk::WaylandSurfaceCreateFlagsKHR::empty(),
			display,
			surface: wl_surface_ptr,
			_marker: PhantomData,
		};
		let Some(create_wayland) = data.dispatch.create_wayland_surface else {
			wl_surface.destroy();
			return passthrough();
		};
		let result = create_wayland(instance, &wayland_info, p_allocator, p_surface);
		if result != VK_SUCCESS {
			crate::log_warn!("the driver cannot make a Wayland surface ({result:?}); using the plain X11 surface");
			wl_surface.destroy();
			return passthrough();
		}

		let mut fallback = VkSurface::null();
		let result = plain_xcb_surface(instance, key, p_create_info, p_allocator, &mut fallback);
		if result != VK_SUCCESS {
			crate::log_warn!("the driver cannot make a plain X11 surface ({result:?})");
			if let Some(destroy) = data.dispatch.destroy_surface {
				destroy(instance, *p_surface, p_allocator);
			}
			wl_surface.destroy();
			return result;
		}

		insert_surface(
			surface_key(*p_surface),
			SurfaceData::new(wayland, wl_surface, true, fallback, Some((x11, window)), data.app.flags),
		);
		VK_SUCCESS
	}
}

pub unsafe extern "C" fn create_xlib_surface(
	instance: VkInstance,
	p_create_info: *const VkXlibSurfaceCreateInfoKHR,
	p_allocator: *const VkAllocationCallbacks,
	p_surface: *mut VkSurface,
) -> VkResult {
	unsafe {
		let create_info = &*p_create_info;
		let connection = xlib_to_xcb_connection(create_info.dpy);
		if connection.is_null() {
			return VK_ERROR_FEATURE_NOT_PRESENT;
		}
		let xcb_info = VkXcbSurfaceCreateInfoKHR {
			s_type: vk::StructureType::XCB_SURFACE_CREATE_INFO_KHR,
			p_next: std::ptr::null(),
			flags: vk::XcbSurfaceCreateFlagsKHR::empty(),
			connection,
			window: create_info.window as u32,
			_marker: PhantomData,
		};
		create_xcb_surface(instance, &xcb_info, p_allocator, p_surface)
	}
}

pub unsafe extern "C" fn destroy_surface(
	instance: VkInstance,
	surface: VkSurface,
	p_allocator: *const VkAllocationCallbacks,
) {
	unsafe {
		let Some(data) = get_instance(instance_key_of(instance)) else {
			return;
		};
		let Some(destroy) = data.dispatch.destroy_surface else {
			return;
		};
		if let Some(tracked) = remove_surface(surface_key(surface)) {
			if tracked.fallback != VkSurface::null() {
				destroy(instance, tracked.fallback, p_allocator);
			}
			// The Vulkan surface goes first: it holds the wl_surface.
			destroy(instance, surface, p_allocator);
			if tracked.owns_wl_surface {
				tracked.wl_surface.destroy();
				tracked.wayland.flush();
			}
			return;
		}
		destroy(instance, surface, p_allocator);
	}
}

// ---------------------------------------------------------------------------
// Capabilities and formats
// ---------------------------------------------------------------------------

/// The window's current size replaces the Wayland surface's undefined one, and
/// the minimum image count is the layer's.
fn adjust_capabilities(data: &SurfaceData, caps: &mut vk::SurfaceCapabilitiesKHR) -> VkResult {
	{
		if !data.is_wayland() {
			let Some(rect) = data.window_rect() else {
				return vk::Result::ERROR_SURFACE_LOST_KHR;
			};
			caps.current_extent = vk::Extent2D {
				width: rect.width,
				height: rect.height,
			};
		}
		let env = |name: &str| std::env::var(name).ok();
		caps.min_image_count = config::min_image_count(&env);
		VK_SUCCESS
	}
}

pub unsafe extern "C" fn get_physical_device_surface_capabilities(
	physical_device: VkPhysicalDevice,
	surface: VkSurface,
	p_caps: *mut VkSurfaceCapabilitiesKHR,
) -> VkResult {
	unsafe {
		let Some(instance) = get_instance(instance_key_of(physical_device)) else {
			return VK_ERROR_INITIALIZATION_FAILED;
		};
		let Some(next) = instance.dispatch.get_physical_device_surface_capabilities else {
			return VK_ERROR_FEATURE_NOT_PRESENT;
		};
		let result = next(physical_device, surface, p_caps);
		match get_surface(surface_key(surface)) {
			Some(data) if result == VK_SUCCESS => adjust_capabilities(&data, &mut *p_caps),
			_ => result,
		}
	}
}

pub unsafe extern "C" fn get_physical_device_surface_capabilities2(
	physical_device: VkPhysicalDevice,
	p_surface_info: *const VkPhysicalDeviceSurfaceInfo2KHR,
	p_caps: *mut VkSurfaceCapabilities2KHR,
) -> VkResult {
	unsafe {
		let Some(instance) = get_instance(instance_key_of(physical_device)) else {
			return VK_ERROR_INITIALIZATION_FAILED;
		};
		let Some(next) = instance.dispatch.get_physical_device_surface_capabilities2 else {
			return VK_ERROR_FEATURE_NOT_PRESENT;
		};
		let result = next(physical_device, p_surface_info, p_caps);
		match get_surface(surface_key((*p_surface_info).surface)) {
			Some(data) if result == VK_SUCCESS => adjust_capabilities(&data, &mut (*p_caps).surface_capabilities),
			_ => result,
		}
	}
}

/// What the format queries decide for a surface: which surface the driver
/// answers for, and whether the HDR formats are added.
struct FormatPlan {
	driver_surface: VkSurface,
	expose_hdr: bool,
}

fn plan_formats(surface: VkSurface, data: &SurfaceData) -> FormatPlan {
	let can_bypass = data.can_bypass(false);
	// The HDR formats appear whenever an HDR colour space could bypass, even if
	// SDR currently cannot: HDR skips the Wine offscreen gate.
	let expose_hdr = data.should_expose_hdr() && (can_bypass || data.can_bypass(true));
	FormatPlan {
		driver_surface: data.driver_surface(surface, can_bypass),
		expose_hdr,
	}
}

pub unsafe extern "C" fn get_physical_device_surface_formats(
	physical_device: VkPhysicalDevice,
	surface: VkSurface,
	p_count: *mut u32,
	p_formats: *mut VkSurfaceFormatKHR,
) -> VkResult {
	unsafe {
		let Some(instance) = get_instance(instance_key_of(physical_device)) else {
			return VK_ERROR_INITIALIZATION_FAILED;
		};
		let Some(next) = instance.dispatch.get_physical_device_surface_formats else {
			return VK_ERROR_FEATURE_NOT_PRESENT;
		};
		let Some(data) = get_surface(surface_key(surface)) else {
			return next(physical_device, surface, p_count, p_formats);
		};
		let plan = plan_formats(surface, &data);
		let call = |count, out| next(physical_device, plan.driver_surface, count, out);
		if !plan.expose_hdr {
			return call(p_count, p_formats);
		}
		let extra = HDR_SURFACE_FORMATS.map(|(format, color_space)| VkSurfaceFormatKHR { format, color_space });
		append_extra(&extra, p_count, p_formats, call)
	}
}

pub unsafe extern "C" fn get_physical_device_surface_formats2(
	physical_device: VkPhysicalDevice,
	p_surface_info: *const VkPhysicalDeviceSurfaceInfo2KHR,
	p_count: *mut u32,
	p_formats: *mut VkSurfaceFormat2KHR,
) -> VkResult {
	unsafe {
		let Some(instance) = get_instance(instance_key_of(physical_device)) else {
			return VK_ERROR_INITIALIZATION_FAILED;
		};
		let Some(next) = instance.dispatch.get_physical_device_surface_formats2 else {
			return VK_ERROR_FEATURE_NOT_PRESENT;
		};
		let Some(data) = get_surface(surface_key((*p_surface_info).surface)) else {
			return next(physical_device, p_surface_info, p_count, p_formats);
		};
		let plan = plan_formats((*p_surface_info).surface, &data);
		let mut info = *p_surface_info;
		info.surface = plan.driver_surface;
		let call = |count, out| next(physical_device, &info, count, out);
		if !plan.expose_hdr {
			return call(p_count, p_formats);
		}
		let extra = HDR_SURFACE_FORMATS.map(|(format, color_space)| VkSurfaceFormat2KHR {
			surface_format: VkSurfaceFormatKHR { format, color_space },
			..Default::default()
		});
		append_extra(&extra, p_count, p_formats, call)
	}
}

// ---------------------------------------------------------------------------
// Presentation support
// ---------------------------------------------------------------------------

/// The Wayland presentation support of the layer's own display
/// (`GetPhysicalDeviceGamescopePresentationSupport`): the X11 connection the
/// application asks about is irrelevant, the layer presents on Wayland.
unsafe fn wayland_presentation_support(
	physical_device: VkPhysicalDevice,
	queue_family: u32,
) -> Option<ash::vk::Bool32> {
	unsafe {
		let instance = get_instance(instance_key_of(physical_device))?;
		let wayland = instance.wayland.as_ref()?;
		let next = instance.dispatch.get_physical_device_wayland_presentation_support?;
		let display = wayland.connection.backend().display_ptr() as *mut std::ffi::c_void;
		Some(next(physical_device, queue_family, display))
	}
}

pub unsafe extern "C" fn get_physical_device_xcb_presentation_support(
	physical_device: VkPhysicalDevice,
	queue_family: u32,
	connection: *mut std::ffi::c_void,
	visual_id: u32,
) -> ash::vk::Bool32 {
	unsafe {
		if let Some(supported) = wayland_presentation_support(physical_device, queue_family) {
			return supported;
		}
		get_instance(instance_key_of(physical_device))
			.and_then(|instance| instance.dispatch.get_physical_device_xcb_presentation_support)
			.map_or(vk::FALSE, |next| {
				next(physical_device, queue_family, connection, visual_id)
			})
	}
}

pub unsafe extern "C" fn get_physical_device_xlib_presentation_support(
	physical_device: VkPhysicalDevice,
	queue_family: u32,
	display: *mut std::ffi::c_void,
	visual_id: libc::c_ulong,
) -> ash::vk::Bool32 {
	unsafe {
		if let Some(supported) = wayland_presentation_support(physical_device, queue_family) {
			return supported;
		}
		get_instance(instance_key_of(physical_device))
			.and_then(|instance| instance.dispatch.get_physical_device_xlib_presentation_support)
			.map_or(vk::FALSE, |next| {
				next(physical_device, queue_family, display, visual_id)
			})
	}
}

// ---------------------------------------------------------------------------
// Device extensions the layer provides
// ---------------------------------------------------------------------------

/// `VK_EXT_hdr_metadata` and `VK_GOOGLE_display_timing`, implemented by the
/// layer whether or not the driver has them.
fn layer_extensions() -> [vk::ExtensionProperties; 2] {
	let make = |name: &std::ffi::CStr, version| {
		vk::ExtensionProperties::default()
			.extension_name(name)
			.expect("short extension name")
			.spec_version(version)
	};
	[
		make(vk::EXT_HDR_METADATA_NAME, 2),
		make(vk::GOOGLE_DISPLAY_TIMING_NAME, 1),
	]
}

pub unsafe extern "C" fn enumerate_device_extension_properties(
	physical_device: VkPhysicalDevice,
	p_layer_name: *const std::ffi::c_char,
	p_count: *mut u32,
	p_properties: *mut vk::ExtensionProperties,
) -> VkResult {
	unsafe {
		let Some(instance) = get_instance(instance_key_of(physical_device)) else {
			return VK_ERROR_INITIALIZATION_FAILED;
		};
		let Some(next) = instance.dispatch.enumerate_device_extension_properties else {
			return VK_ERROR_FEATURE_NOT_PRESENT;
		};
		let extensions = layer_extensions();
		if !p_layer_name.is_null() {
			if std::ffi::CStr::from_ptr(p_layer_name).to_bytes() == crate::LAYER_NAME.as_bytes() {
				return write_array(&extensions, p_count, p_properties);
			}
			return next(physical_device, p_layer_name, p_count, p_properties);
		}
		append_extra(&extensions, p_count, p_properties, |count, out| {
			next(physical_device, p_layer_name, count, out)
		})
	}
}

/// How long a bypass decision is reused on the present path.
pub const PRESENT_CHECK_AGE: Duration = Duration::from_millis(100);

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_layer_provides_hdr_metadata_and_display_timing() {
		let names: Vec<String> = layer_extensions()
			.iter()
			.map(|extension| {
				unsafe { std::ffi::CStr::from_ptr(extension.extension_name.as_ptr()) }
					.to_string_lossy()
					.into_owned()
			})
			.collect();
		assert_eq!(names, ["VK_EXT_hdr_metadata", "VK_GOOGLE_display_timing"]);
	}
}
