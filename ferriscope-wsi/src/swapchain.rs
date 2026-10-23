//! Swapchain creation, presentation, and the feedback calls. Ports gamescope's
//! `VkDeviceOverrides` (`layer/VkLayer_FROG_gamescope_wsi.cpp`).
//!
//! The driver always gets a MAILBOX swapchain in sRGB; the present mode and
//! colour space the application chose reach ferriscope through
//! `set_present_mode` and `swapchain_feedback`, and ferriscope implements FIFO
//! itself (one commit per tick), exactly as gamescope does.

use ash::vk::{self, Handle as _};

use crate::{
	colour::{DRIVER_COLOUR_SPACE, colour_xy_to_u16, is_hdr_colour_space, nits_to_u16, nits_to_u16_dark, write_array},
	config::{self, Flags},
	dispatch::*,
	state::{SwapchainData, get_device, get_instance, get_surface, get_swapchain, insert_swapchain, remove_swapchain},
	surface::PRESENT_CHECK_AGE,
	timing::SwapchainShared,
};

use std::sync::Arc;

fn swapchain_key(swapchain: VkSwapchain) -> u64 {
	swapchain.as_raw()
}

pub unsafe extern "C" fn create_swapchain(
	device: VkDevice,
	p_create_info: *const VkSwapchainCreateInfoKHR,
	p_allocator: *const VkAllocationCallbacks,
	p_swapchain: *mut VkSwapchain,
) -> VkResult {
	unsafe {
		let create_info = &*p_create_info;
		let Some(dev) = get_device(device_key_of(device)) else {
			return VK_ERROR_INITIALIZATION_FAILED;
		};
		let Some(next) = dev.dispatch.create_swapchain else {
			return VK_ERROR_INITIALIZATION_FAILED;
		};
		let Some(surface) = get_surface(create_info.surface.as_raw()) else {
			// Not a surface the layer manages: not our business.
			return next(device, p_create_info, p_allocator, p_swapchain);
		};
		let Some(instance) = get_instance(dev.instance) else {
			crate::log_warn!("CreateSwapchainKHR: the instance was already destroyed");
			return vk::Result::ERROR_SURFACE_LOST_KHR;
		};

		// Only the colour spaces the layer offers count as HDR; other non-sRGB
		// spaces are SDR content that Wine can blit.
		let hdr_colourspace = is_hdr_colour_space(create_info.image_color_space);
		let bypass = surface.can_bypass(hdr_colourspace);

		let mut info = *create_info;
		if create_info.old_swapchain != VkSwapchain::null()
			&& let Some(old) = get_swapchain(swapchain_key(create_info.old_swapchain))
		{
			old.shared.on_retired();
			// Bypassing or not changes the surface, and a swapchain cannot be
			// replaced across surfaces.
			if old.bypassing != bypass {
				info.old_swapchain = VkSwapchain::null();
			}
		}

		if surface.flags.contains(Flags::FORCE_SWAPCHAIN_EXTENT) && !surface.is_wayland() {
			let Some(rect) = surface.window_rect() else {
				return vk::Result::ERROR_SURFACE_LOST_KHR;
			};
			info.image_extent = vk::Extent2D {
				width: rect.width,
				height: rect.height,
			};
		}

		// Without the bypass the window presents through plain XCB.
		info.surface = surface.driver_surface(create_info.surface, bypass);

		// ferriscope implements FIFO: the driver only ever sees MAILBOX, in sRGB.
		let mailbox = [vk::PresentModeKHR::MAILBOX];
		let mut modes_node = vk::SwapchainPresentModesCreateInfoEXT::default();
		let modes_patch = dev.has_maintenance1.then(|| {
			patch_chain(
				&mut info.p_next,
				vk::StructureType::SWAPCHAIN_PRESENT_MODES_CREATE_INFO_EXT,
				&mut modes_node,
				vk::SwapchainPresentModesCreateInfoEXT::default(),
				|modes| {
					modes.present_mode_count = mailbox.len() as u32;
					modes.p_present_modes = mailbox.as_ptr();
				},
			)
		});
		info.image_color_space = DRIVER_COLOUR_SPACE;
		info.present_mode = vk::PresentModeKHR::MAILBOX;

		let env = |name: &str| std::env::var(name).ok();
		if config::ensure_min_image_count(&env) {
			info.min_image_count = info.min_image_count.max(config::min_image_count(&env));
		}

		crate::log_info!(
			"creating swapchain for window 0x{:x}: old {:?}, minImageCount {} (asked {}), format {:?}, colourspace {:?}, bypass {}",
			surface.window(),
			create_info.old_swapchain,
			info.min_image_count,
			create_info.min_image_count,
			create_info.image_format,
			create_info.image_color_space,
			bypass
		);

		// The format must exist on the surface the driver is given.
		if !format_supported(&dev, &instance, info.surface, info.image_format) {
			crate::log_warn!(
				"refusing a swapchain with unsupported format {:?} (window 0x{:x}, bypass {bypass})",
				create_info.image_format,
				surface.window()
			);
			if let Some(patch) = modes_patch {
				patch.restore();
			}
			return VK_ERROR_INITIALIZATION_FAILED;
		}

		let server_id = if surface.is_wayland() {
			!0
		} else {
			match surface.wayland.caps.xwayland_server() {
				0 => {
					if let Some(patch) = modes_patch {
						patch.restore();
					}
					crate::log_warn!("no Xwayland server id; failing swapchain creation");
					return vk::Result::ERROR_SURFACE_LOST_KHR;
				},
				id => id,
			}
		};

		let result = next(device, &info, p_allocator, p_swapchain);
		if let Some(patch) = modes_patch {
			patch.restore();
		}
		if result != VK_SUCCESS {
			crate::log_warn!("the driver failed to create the swapchain: {result:?}");
			return result;
		}
		let swapchain = *p_swapchain;

		let image_count = swapchain_image_count(&dev, device, swapchain);
		let shared = Arc::new(SwapchainShared::new());
		// Presenting through the fallback needs no compositor object.
		let object = (surface.is_wayland() || bypass).then(|| {
			let object =
				surface
					.wayland
					.factory
					.create_swapchain(&surface.wl_surface, &surface.wayland.qh, shared.clone());
			object.swapchain_feedback(
				image_count,
				create_info.image_format.as_raw() as u32,
				create_info.image_color_space.as_raw() as u32,
				create_info.composite_alpha.as_raw(),
				create_info.pre_transform.as_raw(),
				create_info.clipped,
				instance.app.engine_name.clone(),
			);
			object
		});
		surface.wayland.flush();

		crate::log_info!(
			"created swapchain {:#x}: {image_count} images, mode {:?}, hdr {hdr_colourspace}",
			swapchain.as_raw(),
			create_info.present_mode
		);
		insert_swapchain(
			swapchain_key(swapchain),
			SwapchainData {
				surface: create_info.surface,
				object,
				wayland: surface.wayland.clone(),
				shared,
				is_wayland: surface.is_wayland(),
				bypassing: bypass,
				hdr_colourspace,
				present_mode: create_info.present_mode,
				extent: create_info.image_extent,
				server_id,
				window: surface.window(),
			},
		);
		VK_SUCCESS
	}
}

unsafe fn swapchain_image_count(dev: &crate::state::DeviceData, device: VkDevice, swapchain: VkSwapchain) -> u32 {
	unsafe {
		let mut count = 0u32;
		if let Some(get_images) = dev.dispatch.get_swapchain_images {
			let _ = get_images(device, swapchain, &mut count, std::ptr::null_mut());
		}
		count
	}
}

/// Whether `format` is one of the formats the driver offers for `surface`.
unsafe fn format_supported(
	dev: &crate::state::DeviceData,
	instance: &crate::state::InstanceData,
	surface: VkSurface,
	format: vk::Format,
) -> bool {
	unsafe {
		let Some(get_formats) = instance.dispatch.get_physical_device_surface_formats else {
			return true;
		};
		let mut count = 0u32;
		if get_formats(dev.physical_device, surface, &mut count, std::ptr::null_mut()) != VK_SUCCESS {
			return true;
		}
		let mut formats = vec![VkSurfaceFormatKHR::default(); count as usize];
		if get_formats(dev.physical_device, surface, &mut count, formats.as_mut_ptr()) < VK_SUCCESS {
			return true;
		}
		formats.truncate(count as usize);
		formats.iter().any(|supported| supported.format == format)
	}
}

pub unsafe extern "C" fn destroy_swapchain(
	device: VkDevice,
	swapchain: VkSwapchain,
	p_allocator: *const VkAllocationCallbacks,
) {
	unsafe {
		crate::log_info!("destroying swapchain {:#x}", swapchain.as_raw());
		// The compositor object goes first (its drop sends the destructor).
		remove_swapchain(swapchain_key(swapchain));
		if let Some(destroy) = get_device(device_key_of(device)).and_then(|dev| dev.dispatch.destroy_swapchain) {
			destroy(device, swapchain, p_allocator);
		}
	}
}

/// Whether FIFO applications are paced ([`config::pacing`]), read once.
fn pacing() -> bool {
	static PACING: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
	*PACING.get_or_init(|| config::pacing(&|name: &str| std::env::var(name).ok()))
}

/// Hold a FIFO application until the compositor can take its next frame in
/// time (see `ferriscope_swapchain_v1.frame_ready`), so it starts rendering
/// right after a tick instead of running ahead of the display.
fn wait_for_frame_ready(data: &SwapchainData, timeout_ns: u64) {
	if timeout_ns == 0 || data.object.is_none() || !pacing() || !data.shared.should_gate() {
		return;
	}
	let budget = std::time::Duration::from_nanos((2 * data.shared.refresh_cycle_ns()).min(timeout_ns));
	let start = std::time::Instant::now();
	while data.shared.frames_ahead() > 0 && !data.shared.is_retired() {
		let waited = start.elapsed();
		if waited >= budget {
			// The compositor is not ticking for this surface (yet): do not stall.
			crate::log_debug!("no frame_ready within {:?}; not waiting", waited);
			data.shared.resync();
			return;
		}
		if !data.wayland.pump_for(budget - waited) {
			return;
		}
	}
}

/// `CLOCK_MONOTONIC` now, in nanoseconds.
fn monotonic_ns() -> u64 {
	let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
	// SAFETY: a valid timespec.
	unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
	time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}

/// The driver's acquire, called at `started_ns`, returned. The frame started
/// when the present wait ended, before it, so the time the driver blocked
/// (waiting for a free image) is taken out of the frame time. Counting it once
/// made a game look too slow to hold, and unheld it stayed in the state that
/// kept it blocked.
fn note_blocked(swapchain: VkSwapchain, started_ns: u64) {
	if pacing()
		&& let Some(data) = get_swapchain(swapchain_key(swapchain))
	{
		data.shared.note_blocked(monotonic_ns().saturating_sub(started_ns));
	}
}

pub unsafe extern "C" fn acquire_next_image(
	device: VkDevice,
	swapchain: VkSwapchain,
	timeout: u64,
	semaphore: vk::Semaphore,
	fence: vk::Fence,
	p_image_index: *mut u32,
) -> VkResult {
	unsafe {
		if let Some(data) = get_swapchain(swapchain_key(swapchain))
			&& data.shared.is_retired()
		{
			return VK_ERROR_OUT_OF_DATE_KHR;
		}
		let started = monotonic_ns();
		let result = match get_device(device_key_of(device)).and_then(|dev| dev.dispatch.acquire_next_image) {
			Some(next) => next(device, swapchain, timeout, semaphore, fence, p_image_index),
			None => VK_ERROR_INITIALIZATION_FAILED,
		};
		note_blocked(swapchain, started);
		result
	}
}

pub unsafe extern "C" fn acquire_next_image2(
	device: VkDevice,
	p_acquire_info: *const vk::AcquireNextImageInfoKHR,
	p_image_index: *mut u32,
) -> VkResult {
	unsafe {
		if let Some(data) = get_swapchain(swapchain_key((*p_acquire_info).swapchain))
			&& data.shared.is_retired()
		{
			return VK_ERROR_OUT_OF_DATE_KHR;
		}
		let started = monotonic_ns();
		let result = match get_device(device_key_of(device)).and_then(|dev| dev.dispatch.acquire_next_image2) {
			Some(next) => next(device, p_acquire_info, p_image_index),
			None => VK_ERROR_INITIALIZATION_FAILED,
		};
		note_blocked((*p_acquire_info).swapchain, started);
		result
	}
}

/// A present that fails with `VK_ERROR_OUT_OF_DATE_KHR` must still perform its
/// queue operations. A retired swapchain's present is never forwarded, so the
/// semaphores are waited and any `VkSwapchainPresentFenceInfoEXT` fences
/// signalled with an empty submit.
unsafe fn present_retired(dev: &crate::state::DeviceData, queue: VkQueue, present_info: &VkPresentInfoKHR) -> VkResult {
	unsafe {
		let wait_stages = vec![vk::PipelineStageFlags::ALL_COMMANDS; present_info.wait_semaphore_count as usize];
		let mut fences = Vec::new();
		if let Some(fence_info) = find_in_chain::<vk::SwapchainPresentFenceInfoEXT>(
			present_info.p_next,
			vk::StructureType::SWAPCHAIN_PRESENT_FENCE_INFO_EXT,
		) {
			for i in 0..(*fence_info).swapchain_count as usize {
				let fence = *(*fence_info).p_fences.add(i);
				if fence != vk::Fence::null() {
					fences.push(fence);
				}
			}
		}
		if let Some(submit) = dev.dispatch.queue_submit
			&& (present_info.wait_semaphore_count > 0 || !fences.is_empty())
		{
			let first = vk::SubmitInfo {
				wait_semaphore_count: present_info.wait_semaphore_count,
				p_wait_semaphores: present_info.p_wait_semaphores,
				p_wait_dst_stage_mask: wait_stages.as_ptr(),
				..Default::default()
			};
			let mut result = submit(queue, 1, &first, fences.first().copied().unwrap_or(vk::Fence::null()));
			// Fence signals are ordered after everything earlier in submission
			// order, so the others ride empty submits.
			for fence in fences.iter().skip(1) {
				if result != VK_SUCCESS {
					break;
				}
				result = submit(queue, 1, &vk::SubmitInfo::default(), *fence);
			}
			if result.as_raw() < 0 {
				return result;
			}
		}
		if !present_info.p_results.is_null() {
			for i in 0..present_info.swapchain_count as usize {
				*present_info.p_results.add(i) = VK_ERROR_OUT_OF_DATE_KHR;
			}
		}
		VK_ERROR_OUT_OF_DATE_KHR
	}
}

pub unsafe extern "C" fn queue_present(queue: VkQueue, p_present_info: *const VkPresentInfoKHR) -> VkResult {
	unsafe {
		let Some(dev) = get_device(device_key_of(queue)) else {
			return VK_ERROR_DEVICE_LOST;
		};
		let Some(next) = dev.dispatch.queue_present else {
			return VK_ERROR_DEVICE_LOST;
		};
		let mut info = *p_present_info;
		let count = info.swapchain_count as usize;
		let swapchains = if count > 0 {
			std::slice::from_raw_parts(info.p_swapchains, count)
		} else {
			&[]
		};
		let tracked: Vec<Option<Arc<SwapchainData>>> = swapchains
			.iter()
			.map(|swapchain| get_swapchain(swapchain_key(*swapchain)))
			.collect();
		let Some(wayland) = tracked.iter().flatten().map(|data| data.wayland.clone()).next() else {
			return next(queue, p_present_info);
		};
		if tracked.iter().flatten().any(|data| data.shared.is_retired()) {
			return present_retired(&dev, queue, &info);
		}

		let times =
			find_in_chain::<vk::PresentTimesInfoGOOGLE>(info.p_next, vk::StructureType::PRESENT_TIMES_INFO_GOOGLE)
				.filter(|times| !(**times).p_times.is_null() && (**times).swapchain_count as usize >= count);
		let original_modes = find_in_chain::<vk::SwapchainPresentModeInfoEXT>(
			info.p_next,
			vk::StructureType::SWAPCHAIN_PRESENT_MODE_INFO_EXT,
		)
		.filter(|modes| !(**modes).p_present_modes.is_null() && (**modes).swapchain_count as usize >= count);

		// Take in `refresh_cycle`, `retired` and timings before deciding anything.
		wayland.pump();

		for (i, data) in tracked.iter().enumerate() {
			let Some(data) = data else { continue };
			let Some(object) = &data.object else { continue };
			if let Some(times) = times {
				let time = *(*times).p_times.add(i);
				object.set_present_time(
					time.present_id,
					(time.desired_present_time >> 32) as u32,
					time.desired_present_time as u32,
				);
			}
			if !data.is_wayland {
				object.override_window_content(data.server_id, data.window);
			}
			let mode = original_modes.map_or(data.present_mode, |modes| *(*modes).p_present_modes.add(i));
			object.set_present_mode(mode.as_raw() as u32);
			if mode == vk::PresentModeKHR::FIFO {
				data.shared.note_present(monotonic_ns());
			}
		}
		wayland.flush();

		// The driver is always given MAILBOX for the swapchains that are ours.
		let modes: Vec<vk::PresentModeKHR> = tracked
			.iter()
			.enumerate()
			.map(|(i, data)| match (data, original_modes) {
				(Some(_), _) => vk::PresentModeKHR::MAILBOX,
				(None, Some(modes)) => *(*modes).p_present_modes.add(i),
				(None, None) => vk::PresentModeKHR::FIFO,
			})
			.collect();
		let mut modes_node = vk::SwapchainPresentModeInfoEXT::default();
		let patch = dev.has_maintenance1.then(|| {
			patch_chain(
				&mut info.p_next,
				vk::StructureType::SWAPCHAIN_PRESENT_MODE_INFO_EXT,
				&mut modes_node,
				vk::SwapchainPresentModeInfoEXT::default(),
				|node| {
					node.swapchain_count = modes.len() as u32;
					node.p_present_modes = modes.as_ptr();
				},
			)
		});
		let mut result = next(queue, &info);
		if let Some(patch) = patch {
			patch.restore();
		}

		for (i, data) in tracked.iter().enumerate() {
			let Some(data) = data else { continue };
			let mut update = |new_result: VkResult| {
				if !info.p_results.is_null() && (*info.p_results.add(i)).as_raw() >= 0 {
					*info.p_results.add(i) = new_result;
				}
				if result.as_raw() >= 0 {
					result = new_result;
				}
			};
			let Some(surface) = get_surface(data.surface.as_raw()) else {
				crate::log_warn!("QueuePresentKHR: the surface of swapchain {i} was already destroyed");
				continue;
			};
			let check = surface.check_bypass(data.hdr_colourspace, PRESENT_CHECK_AGE);
			if check.allowed != data.bypassing {
				if check.allowed {
					if !surface.flags.contains(Flags::NO_SUBOPTIMAL) {
						update(VK_SUBOPTIMAL_KHR);
					}
				} else {
					update(VK_ERROR_OUT_OF_DATE_KHR);
				}
			}
			// Emulate an X11 swapchain noticing that the window's extent changed.
			if !surface.is_wayland()
				&& !surface.flags.contains(Flags::FORCE_SWAPCHAIN_EXTENT)
				&& let Some(rect) = check.rect
				&& (rect.width != data.extent.width || rect.height != data.extent.height)
			{
				update(VK_ERROR_OUT_OF_DATE_KHR);
			}
		}

		// Hold the game here rather than in its next acquire: it reads input
		// after this returns, so the frame it then renders shows input from
		// right after the tick instead of input a frame old.
		if pacing() {
			for (i, data) in tracked.iter().enumerate() {
				let Some(data) = data else { continue };
				let mode = original_modes.map_or(data.present_mode, |modes| *(*modes).p_present_modes.add(i));
				if mode == vk::PresentModeKHR::FIFO {
					wait_for_frame_ready(data, u64::MAX);
					data.shared.note_frame_start(monotonic_ns());
				}
			}
		}
		result
	}
}

pub unsafe extern "C" fn set_hdr_metadata(
	device: VkDevice,
	swapchain_count: u32,
	p_swapchains: *const VkSwapchain,
	p_metadata: *const VkHdrMetadataEXT,
) {
	unsafe {
		let swapchains = std::slice::from_raw_parts(p_swapchains, swapchain_count as usize);
		let metadata = std::slice::from_raw_parts(p_metadata, swapchain_count as usize);
		for (swapchain, md) in swapchains.iter().zip(metadata) {
			crate::log_debug!(
				"vkSetHdrMetadataEXT: luminance {}..{} cd/m2, max CLL {}, max FALL {}",
				md.min_luminance,
				md.max_luminance,
				md.max_content_light_level,
				md.max_frame_average_light_level
			);
			match get_swapchain(swapchain_key(*swapchain)) {
				Some(data) => {
					if let Some(object) = &data.object {
						object.set_hdr_metadata(
							colour_xy_to_u16(md.display_primary_red.x),
							colour_xy_to_u16(md.display_primary_red.y),
							colour_xy_to_u16(md.display_primary_green.x),
							colour_xy_to_u16(md.display_primary_green.y),
							colour_xy_to_u16(md.display_primary_blue.x),
							colour_xy_to_u16(md.display_primary_blue.y),
							colour_xy_to_u16(md.white_point.x),
							colour_xy_to_u16(md.white_point.y),
							nits_to_u16(md.max_luminance),
							nits_to_u16_dark(md.min_luminance),
							nits_to_u16(md.max_content_light_level),
							nits_to_u16(md.max_frame_average_light_level),
						);
						data.wayland.flush();
					}
				},
				// Not ours: the driver may take it.
				None => {
					if let Some(Some(next)) = get_device(device_key_of(device)).map(|dev| dev.dispatch.set_hdr_metadata)
					{
						next(device, 1, swapchain, md);
					}
				},
			}
		}
	}
}

pub unsafe extern "C" fn get_refresh_cycle_duration(
	device: VkDevice,
	swapchain: VkSwapchain,
	p_properties: *mut vk::RefreshCycleDurationGOOGLE,
) -> VkResult {
	unsafe {
		match get_swapchain(swapchain_key(swapchain)) {
			Some(data) => {
				if !data.wayland.pump() {
					return vk::Result::ERROR_SURFACE_LOST_KHR;
				}
				(*p_properties).refresh_duration = data.shared.refresh_cycle_ns();
				VK_SUCCESS
			},
			None => match get_device(device_key_of(device)).and_then(|dev| dev.dispatch.get_refresh_cycle_duration) {
				Some(next) => next(device, swapchain, p_properties),
				None => vk::Result::ERROR_SURFACE_LOST_KHR,
			},
		}
	}
}

pub unsafe extern "C" fn get_past_presentation_timing(
	device: VkDevice,
	swapchain: VkSwapchain,
	p_count: *mut u32,
	p_timings: *mut vk::PastPresentationTimingGOOGLE,
) -> VkResult {
	unsafe {
		let Some(data) = get_swapchain(swapchain_key(swapchain)) else {
			return match get_device(device_key_of(device)).and_then(|dev| dev.dispatch.get_past_presentation_timing) {
				Some(next) => next(device, swapchain, p_count, p_timings),
				None => vk::Result::ERROR_SURFACE_LOST_KHR,
			};
		};
		// Take in the latest timings.
		if !data.wayland.pump() {
			return vk::Result::ERROR_SURFACE_LOST_KHR;
		}
		let mut queue = data
			.shared
			.timings
			.lock()
			.unwrap_or_else(|poisoned| poisoned.into_inner());
		if p_timings.is_null() {
			*p_count = queue.len() as u32;
			return VK_SUCCESS;
		}
		let wanted = *p_count as usize;
		let taken = queue.take(wanted);
		let converted: Vec<vk::PastPresentationTimingGOOGLE> = taken
			.iter()
			.map(|timing| vk::PastPresentationTimingGOOGLE {
				present_id: timing.present_id,
				desired_present_time: timing.desired_present_time,
				actual_present_time: timing.actual_present_time,
				earliest_present_time: timing.earliest_present_time,
				present_margin: timing.present_margin,
			})
			.collect();
		let mut count = *p_count;
		let result = write_array(&converted, &mut count, p_timings);
		*p_count = count;
		// More were waiting than the caller had room for.
		if queue.len() > 0 { VK_INCOMPLETE } else { result }
	}
}
