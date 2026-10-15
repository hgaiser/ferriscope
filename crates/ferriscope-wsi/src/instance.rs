//! `vkCreateInstance` / `vkDestroyInstance`, and the connection to ferriscope.
//!
//! The layer is active for an instance when `WAYLAND_DISPLAY` names a socket
//! whose compositor offers `ferriscope_swapchain_factory_v1`. Under
//! pressure-vessel `WAYLAND_DISPLAY` is one of the two socket variables the
//! container remaps (the other is `GAMESCOPE_WAYLAND_DISPLAY`), which is why
//! ferriscope's children get ferriscope's socket there. Anything else is a
//! pass-through instance, so the layer is harmless outside ferriscope.

use std::{ffi::c_char, os::unix::net::UnixStream, path::PathBuf, sync::Arc};

use wayland_client::{
	Connection, Proxy, backend::ObjectId, globals::registry_queue_init, protocol::wl_surface::WlSurface,
};

use crate::{
	config,
	dispatch::*,
	proto::ferriscope_swapchain_factory_v1::FerriscopeSwapchainFactoryV1,
	state::{AppInfo, Caps, InstanceData, Wayland, WaylandState, insert_instance, remove_instance},
};

/// The socket `WAYLAND_DISPLAY` names, resolved against `XDG_RUNTIME_DIR` like
/// libwayland does. `None` when it is unset, or when a socket was inherited
/// (`WAYLAND_SOCKET`): that connection is the application's own and not ours.
fn socket_path(wayland_display: Option<&str>, runtime_dir: Option<&str>, inherited_socket: bool) -> Option<PathBuf> {
	if inherited_socket {
		return None;
	}
	let name = wayland_display.filter(|name| !name.is_empty())?;
	if name.starts_with('/') {
		return Some(PathBuf::from(name));
	}
	Some(PathBuf::from(runtime_dir.filter(|dir| !dir.is_empty())?).join(name))
}

/// Bind the factory (and `wl_compositor` when `want_compositor`) on
/// `connection` and wait for the compositor's capability events.
fn bind_compositor(connection: Connection, want_compositor: bool) -> Option<Arc<Wayland>> {
	let caps = Arc::new(Caps::default());
	let (globals, queue) = registry_queue_init::<WaylandState>(&connection).ok()?;
	let qh = queue.handle();
	let factory: FerriscopeSwapchainFactoryV1 = match globals.bind(&qh, 1..=1, ()) {
		Ok(factory) => factory,
		Err(error) => {
			crate::log_info!("the compositor does not offer ferriscope_swapchain_factory_v1: {error}");
			return None;
		},
	};
	let compositor = if want_compositor {
		match globals.bind(&qh, 4..=6, ()) {
			Ok(compositor) => Some(compositor),
			Err(error) => {
				crate::log_warn!("the compositor has no usable wl_compositor: {error}");
				return None;
			},
		}
	} else {
		None
	};
	let wayland = Arc::new(Wayland::new(connection, compositor, factory, queue, caps));
	// The factory's `hdr_available` and `xwayland_server` follow the bind.
	wayland.roundtrip().then_some(wayland)
}

/// Connect to ferriscope on its socket.
fn connect_to_compositor() -> Option<Arc<Wayland>> {
	let env = |name: &str| std::env::var(name).ok();
	let path = socket_path(
		env("WAYLAND_DISPLAY").as_deref(),
		env("XDG_RUNTIME_DIR").as_deref(),
		env("WAYLAND_SOCKET").is_some(),
	)?;
	let stream = match UnixStream::connect(&path) {
		Ok(stream) => stream,
		Err(error) => {
			crate::log_info!("cannot connect to {}: {error}", path.display());
			return None;
		},
	};
	let connection = Connection::from_socket(stream).ok()?;
	bind_compositor(connection, true)
}

/// Bind the factory on the application's own `wl_display`, for a native
/// Wayland surface: the swapchain object must live on the same connection as
/// the surface. The display stays the application's.
///
/// # Safety
///
/// `display` and `surface` must be the `wl_display*` and `wl_surface*` of a
/// live connection.
pub unsafe fn connect_foreign(
	display: *mut std::ffi::c_void,
	surface: *mut std::ffi::c_void,
) -> Option<(Arc<Wayland>, WlSurface)> {
	unsafe {
		if display.is_null() || surface.is_null() {
			return None;
		}
		let backend = wayland_backend::sys::client::Backend::from_foreign_display(display.cast());
		let connection = Connection::from_backend(backend);
		let wayland = bind_compositor(connection.clone(), false)?;
		let id = ObjectId::from_ptr(WlSurface::interface(), surface.cast()).ok()?;
		let wl_surface = WlSurface::from_id(&connection, id).ok()?;
		Some((wayland, wl_surface))
	}
}

/// Move a proxy to the display's default event queue, so the driver's own
/// dispatching sees its events. The layer's queue is never dispatched by the
/// driver.
///
/// # Safety
///
/// `proxy` must be a live `wl_proxy*`.
pub unsafe fn move_to_default_queue(proxy: *mut std::ffi::c_void) {
	unsafe {
		wayland_sys::ffi_dispatch!(
			wayland_sys::client::wayland_client_handle(),
			wl_proxy_set_queue,
			proxy.cast(),
			std::ptr::null_mut()
		);
	}
}

/// `setenv(name, value, overwrite = 0)`.
fn set_default_env(name: &std::ffi::CStr, value: &std::ffi::CStr) {
	// SAFETY: both are valid C strings; the layer runs in the application's
	// process, where this is what gamescope's layer does too.
	unsafe { libc::setenv(name.as_ptr(), value.as_ptr(), 0) };
}

/// Mesa and DXVK knobs gamescope's layer sets for an instance it handles.
fn set_wsi_defaults() {
	set_default_env(c"vk_xwayland_wait_ready", c"false");
	set_default_env(c"vk_khr_present_wait", c"true");
	// Works around Mesa's implementation being broken
	// (https://gitlab.freedesktop.org/mesa/mesa/-/merge_requests/31134).
	set_default_env(c"vk_wsi_force_swapchain_to_current_extent", c"false");
}

pub unsafe extern "C" fn create_instance(
	p_create_info: *const VkInstanceCreateInfo,
	p_allocator: *const VkAllocationCallbacks,
	p_instance: *mut VkInstance,
) -> VkResult {
	unsafe {
		// Advance the layer chain (shared by the active and pass-through paths).
		let chain_info = find_layer_link::<VkLayerInstanceCreateInfo>(
			(*p_create_info).p_next,
			VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO,
		);
		if chain_info.is_null() {
			return VK_ERROR_INITIALIZATION_FAILED;
		}
		let link = (*chain_info).p_layer_info;
		let next_get_proc_addr = (*link).pfn_next_get_instance_proc_addr;
		(*chain_info).p_layer_info = (*link).p_next;

		let create_info = &*p_create_info;
		let wayland = connect_to_compositor();
		crate::log_info!(
			"layer instance: {}",
			if wayland.is_some() {
				"active"
			} else {
				"passing through (not under ferriscope)"
			}
		);

		let mut extensions: Vec<*const c_char> = std::slice::from_raw_parts(
			create_info.pp_enabled_extension_names,
			create_info.enabled_extension_count as usize,
		)
		.to_vec();
		let mut modified = *create_info;
		if wayland.is_some() {
			for extension in [
				ash::vk::KHR_SURFACE_NAME,
				ash::vk::KHR_WAYLAND_SURFACE_NAME,
				ash::vk::KHR_XCB_SURFACE_NAME,
			] {
				if !has_extension(&extensions, extension) {
					extensions.push(extension.as_ptr());
				}
			}
			modified.enabled_extension_count = extensions.len() as u32;
			modified.pp_enabled_extension_names = extensions.as_ptr();
			set_wsi_defaults();
		}

		let next_create_instance = load_next_create_instance(next_get_proc_addr);
		let mut result = next_create_instance(&modified, p_allocator, p_instance);
		let mut wayland = wayland;
		// A driver without the extensions we added: fall back to a plain
		// instance rather than failing the application.
		if result == VK_ERROR_EXTENSION_NOT_PRESENT && wayland.is_some() {
			crate::log_warn!("the driver rejected the WSI extensions the layer added; passing through");
			wayland = None;
			result = next_create_instance(p_create_info, p_allocator, p_instance);
		}
		if result != VK_SUCCESS {
			return result;
		}

		let instance = *p_instance;
		let dispatch = build_instance_dispatch(instance, next_get_proc_addr);

		let engine_name =
			if create_info.p_application_info.is_null() || (*create_info.p_application_info).p_engine_name.is_null() {
				String::new()
			} else {
				crate::xcb::c_name((*create_info.p_application_info).p_engine_name)
			};
		let env = |name: &str| std::env::var(name).ok();
		let flags = config::default_flags(&env, config::app_id(&env), &config::executable_name());
		if wayland.is_some() && flags.contains(config::Flags::DISABLE_HDR) {
			// Force DXVK's HDR off if we already know the application cannot cope.
			libc::setenv(c"DXVK_HDR".as_ptr(), c"0".as_ptr(), 1);
		}

		insert_instance(
			instance_key_of(instance),
			InstanceData {
				dispatch,
				wayland,
				app: AppInfo { engine_name, flags },
			},
		);
		VK_SUCCESS
	}
}

pub unsafe extern "C" fn destroy_instance(instance: VkInstance, p_allocator: *const VkAllocationCallbacks) {
	unsafe {
		crate::log_debug!("vkDestroyInstance");
		if let Some(data) = remove_instance(instance_key_of(instance)) {
			(data.dispatch.destroy_instance)(instance, p_allocator);
		}
	}
}

unsafe fn load_next_create_instance(
	next_get_proc_addr: PFN_vkGetInstanceProcAddr,
) -> unsafe extern "C" fn(*const VkInstanceCreateInfo, *const VkAllocationCallbacks, *mut VkInstance) -> VkResult {
	unsafe {
		let pfn = next_get_proc_addr(VkInstance::null(), c"vkCreateInstance".as_ptr());
		std::mem::transmute(pfn.expect("the next layer provides vkCreateInstance"))
	}
}

unsafe fn build_instance_dispatch(
	instance: VkInstance,
	next_get_proc_addr: PFN_vkGetInstanceProcAddr,
) -> InstanceDispatch {
	unsafe {
		macro_rules! load {
			($name:literal) => {{
				let pfn = next_get_proc_addr(instance, concat!($name, "\0").as_ptr().cast());
				std::mem::transmute(pfn.expect(concat!("failed to load ", $name)))
			}};
			(opt: $name:literal) => {{
				let pfn = next_get_proc_addr(instance, concat!($name, "\0").as_ptr().cast());
				pfn.map(|f| std::mem::transmute(f))
			}};
		}

		InstanceDispatch {
			get_instance_proc_addr: next_get_proc_addr,
			destroy_instance: load!("vkDestroyInstance"),
			create_wayland_surface: load!(opt: "vkCreateWaylandSurfaceKHR"),
			create_xcb_surface: load!(opt: "vkCreateXcbSurfaceKHR"),
			destroy_surface: load!(opt: "vkDestroySurfaceKHR"),
			get_physical_device_surface_formats: load!(opt: "vkGetPhysicalDeviceSurfaceFormatsKHR"),
			get_physical_device_surface_formats2: load!(opt: "vkGetPhysicalDeviceSurfaceFormats2KHR"),
			get_physical_device_surface_capabilities: load!(opt: "vkGetPhysicalDeviceSurfaceCapabilitiesKHR"),
			get_physical_device_surface_capabilities2: load!(opt: "vkGetPhysicalDeviceSurfaceCapabilities2KHR"),
			get_physical_device_wayland_presentation_support: load!(opt: "vkGetPhysicalDeviceWaylandPresentationSupportKHR"),
			get_physical_device_xcb_presentation_support: load!(opt: "vkGetPhysicalDeviceXcbPresentationSupportKHR"),
			get_physical_device_xlib_presentation_support: load!(opt: "vkGetPhysicalDeviceXlibPresentationSupportKHR"),
			enumerate_device_extension_properties: load!(opt: "vkEnumerateDeviceExtensionProperties"),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_relative_display_name_lives_in_the_runtime_dir() {
		assert_eq!(
			socket_path(Some("wayland-2"), Some("/run/user/1000"), false),
			Some(PathBuf::from("/run/user/1000/wayland-2"))
		);
	}

	#[test]
	fn an_absolute_display_name_is_the_socket() {
		assert_eq!(
			socket_path(Some("/run/pressure-vessel/gamescope-socket"), None, false),
			Some(PathBuf::from("/run/pressure-vessel/gamescope-socket"))
		);
	}

	#[test]
	fn no_display_or_no_runtime_dir_means_no_socket() {
		assert_eq!(socket_path(None, Some("/run/user/1000"), false), None);
		assert_eq!(socket_path(Some(""), Some("/run/user/1000"), false), None);
		assert_eq!(socket_path(Some("wayland-0"), None, false), None);
		assert_eq!(socket_path(Some("wayland-0"), Some(""), false), None);
	}

	#[test]
	fn an_inherited_connection_is_not_ours_to_open_again() {
		assert_eq!(socket_path(Some("wayland-0"), Some("/run/user/1000"), true), None);
	}
}
