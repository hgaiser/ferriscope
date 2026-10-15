//! `vkCreateDevice` / `vkDestroyDevice`.
//!
//! On an active instance the layer turns on `VK_EXT_swapchain_maintenance1`
//! (`VkDeviceOverrides::CreateDevice`, gamescope): it is how a swapchain
//! created for MAILBOX is given its present mode per present.

use ash::vk;

use crate::{
	dispatch::*,
	state::{DeviceData, get_instance, insert_device, remove_device},
};

pub unsafe extern "C" fn create_device(
	physical_device: VkPhysicalDevice,
	p_create_info: *const VkDeviceCreateInfo,
	p_allocator: *const VkAllocationCallbacks,
	p_device: *mut VkDevice,
) -> VkResult {
	unsafe {
		let chain_info = find_layer_link::<VkLayerDeviceCreateInfo>(
			(*p_create_info).p_next,
			VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO,
		);
		if chain_info.is_null() {
			return VK_ERROR_INITIALIZATION_FAILED;
		}
		let link = (*chain_info).p_layer_info;
		let next_get_device_proc_addr = (*link).pfn_next_get_device_proc_addr;
		let next_get_instance_proc_addr = (*link).pfn_next_get_instance_proc_addr;
		(*chain_info).p_layer_info = (*link).p_next;

		let instance_key = instance_key_of(physical_device);
		let active = get_instance(instance_key).is_some_and(|instance| instance.wayland.is_some());

		let next_create_device: unsafe extern "C" fn(
			VkPhysicalDevice,
			*const VkDeviceCreateInfo,
			*const VkAllocationCallbacks,
			*mut VkDevice,
		) -> VkResult = {
			let pfn = next_get_instance_proc_addr(VkInstance::null(), c"vkCreateDevice".as_ptr());
			std::mem::transmute(pfn.expect("the next layer provides vkCreateDevice"))
		};

		let mut has_maintenance1 = false;
		let mut result = VK_ERROR_INITIALIZATION_FAILED;
		if active {
			let create_info = &*p_create_info;
			let mut extensions: Vec<*const std::ffi::c_char> = std::slice::from_raw_parts(
				create_info.pp_enabled_extension_names,
				create_info.enabled_extension_count as usize,
			)
			.to_vec();
			if !has_extension(&extensions, vk::EXT_SWAPCHAIN_MAINTENANCE1_NAME) {
				extensions.push(vk::EXT_SWAPCHAIN_MAINTENANCE1_NAME.as_ptr());
			}
			let mut modified = *create_info;
			modified.enabled_extension_count = extensions.len() as u32;
			modified.pp_enabled_extension_names = extensions.as_ptr();

			let mut node = vk::PhysicalDeviceSwapchainMaintenance1FeaturesEXT::default();
			let patch = patch_chain(
				&mut modified.p_next,
				vk::StructureType::PHYSICAL_DEVICE_SWAPCHAIN_MAINTENANCE_1_FEATURES_EXT,
				&mut node,
				vk::PhysicalDeviceSwapchainMaintenance1FeaturesEXT::default(),
				|features| {
					crate::log_info!("forcing on VK_EXT_swapchain_maintenance1");
					features.swapchain_maintenance1 = vk::TRUE;
				},
			);
			result = next_create_device(physical_device, &modified, p_allocator, p_device);
			patch.restore();
			has_maintenance1 = result == VK_SUCCESS;
			if !has_maintenance1 {
				crate::log_warn!("VK_EXT_swapchain_maintenance1 is not available ({result:?}); retrying without");
			}
		}
		if !has_maintenance1 {
			result = next_create_device(physical_device, p_create_info, p_allocator, p_device);
		}
		if result != VK_SUCCESS {
			return result;
		}

		let device = *p_device;
		insert_device(
			device_key_of(device),
			DeviceData {
				dispatch: build_device_dispatch(device, next_get_device_proc_addr),
				instance: instance_key,
				physical_device,
				has_maintenance1,
			},
		);
		VK_SUCCESS
	}
}

pub unsafe extern "C" fn destroy_device(device: VkDevice, p_allocator: *const VkAllocationCallbacks) {
	unsafe {
		crate::log_debug!("vkDestroyDevice");
		if let Some(data) = remove_device(device_key_of(device)) {
			(data.dispatch.destroy_device)(device, p_allocator);
		}
	}
}

unsafe fn build_device_dispatch(
	device: VkDevice,
	next_get_device_proc_addr: PFN_vkGetDeviceProcAddr,
) -> DeviceDispatch {
	unsafe {
		macro_rules! load {
			($name:literal) => {{
				let pfn = next_get_device_proc_addr(device, concat!($name, "\0").as_ptr().cast());
				std::mem::transmute(pfn.expect(concat!("failed to load ", $name)))
			}};
			(opt: $name:literal) => {{
				let pfn = next_get_device_proc_addr(device, concat!($name, "\0").as_ptr().cast());
				pfn.map(|f| std::mem::transmute(f))
			}};
		}

		DeviceDispatch {
			get_device_proc_addr: next_get_device_proc_addr,
			destroy_device: load!("vkDestroyDevice"),
			create_swapchain: load!(opt: "vkCreateSwapchainKHR"),
			destroy_swapchain: load!(opt: "vkDestroySwapchainKHR"),
			queue_present: load!(opt: "vkQueuePresentKHR"),
			queue_submit: load!(opt: "vkQueueSubmit"),
			get_swapchain_images: load!(opt: "vkGetSwapchainImagesKHR"),
			acquire_next_image: load!(opt: "vkAcquireNextImageKHR"),
			set_hdr_metadata: load!(opt: "vkSetHdrMetadataEXT"),
			acquire_next_image2: load!(opt: "vkAcquireNextImage2KHR"),
			get_refresh_cycle_duration: load!(opt: "vkGetRefreshCycleDurationGOOGLE"),
			get_past_presentation_timing: load!(opt: "vkGetPastPresentationTimingGOOGLE"),
		}
	}
}
