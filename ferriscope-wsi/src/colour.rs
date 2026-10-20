//! Surface formats, colour spaces and HDR metadata: the pure parts.
//!
//! The layer offers the application HDR surface formats the driver does not
//! know for a Wayland surface, and presents every swapchain to the driver as
//! sRGB-nonlinear (gamescope's `CreateSwapchainKHR`); the colour space the
//! application actually chose reaches the compositor in `swapchain_feedback`.

use ash::vk::{ColorSpaceKHR, Format, Result as VkResult};

/// The extra surface formats offered when the compositor can show HDR
/// (`s_ExtraHDRSurfaceFormat2s`).
pub const HDR_SURFACE_FORMATS: [(Format, ColorSpaceKHR); 3] = [
	(Format::A2B10G10R10_UNORM_PACK32, ColorSpaceKHR::HDR10_ST2084_EXT),
	(Format::A2R10G10B10_UNORM_PACK32, ColorSpaceKHR::HDR10_ST2084_EXT),
	(Format::R16G16B16A16_SFLOAT, ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT),
];

/// Whether `space` is one of the colour spaces [`HDR_SURFACE_FORMATS`] offers.
/// Other non-sRGB colour spaces are still SDR content that Wine can blit.
pub fn is_hdr_colour_space(space: ColorSpaceKHR) -> bool {
	HDR_SURFACE_FORMATS.iter().any(|&(_, hdr_space)| hdr_space == space)
}

/// The colour space every swapchain is created with at the driver.
pub const DRIVER_COLOUR_SPACE: ColorSpaceKHR = ColorSpaceKHR::SRGB_NONLINEAR;

/// Run a Vulkan two-call enumeration (`call`) and append `extra` to its
/// results (`vkroots::helpers::append`).
///
/// # Safety
///
/// `count` must be valid, and `out` null or valid for `*count` elements.
pub unsafe fn append_extra<T: Copy>(
	extra: &[T],
	count: *mut u32,
	out: *mut T,
	call: impl FnOnce(*mut u32, *mut T) -> VkResult,
) -> VkResult {
	unsafe {
		if out.is_null() {
			let result = call(count, out);
			if result != VkResult::SUCCESS {
				return result;
			}
			*count += extra.len() as u32;
			return VkResult::SUCCESS;
		}

		// Leave room for the extras so the driver does not fill the whole buffer.
		let caller_count = *count;
		*count = caller_count.saturating_sub(extra.len() as u32);

		let result = call(count, out);
		if result != VkResult::SUCCESS && result != VkResult::INCOMPLETE {
			return result;
		}

		let base = *count as usize;
		let room = (caller_count as usize).saturating_sub(base);
		let copied = room.min(extra.len());
		for (i, item) in extra.iter().take(copied).enumerate() {
			*out.add(base + i) = *item;
		}
		*count = (base + copied) as u32;

		if copied < extra.len() {
			VkResult::INCOMPLETE
		} else {
			result
		}
	}
}

/// Write `items` into a Vulkan two-call output array (`vkroots::helpers::array`).
///
/// # Safety
///
/// `count` must be valid, and `out` null or valid for `*count` elements.
pub unsafe fn write_array<T: Copy>(items: &[T], count: *mut u32, out: *mut T) -> VkResult {
	unsafe {
		if out.is_null() {
			*count = items.len() as u32;
			return VkResult::SUCCESS;
		}
		let copied = (*count as usize).min(items.len());
		for (i, item) in items.iter().take(copied).enumerate() {
			*out.add(i) = *item;
		}
		*count = copied as u32;
		if copied < items.len() {
			VkResult::INCOMPLETE
		} else {
			VkResult::SUCCESS
		}
	}
}

/// CIE 1931 coordinate to the protocol's unit of 0.00002 (`color_xy_to_u16`).
pub fn colour_xy_to_u16(value: f32) -> u32 {
	(value * 50_000.0).round().clamp(0.0, 65_535.0) as u32
}

/// Luminance in cd/m2 to the protocol's 1 cd/m2 unit (`nits_to_u16`).
pub fn nits_to_u16(value: f32) -> u32 {
	value.round().clamp(0.0, 65_535.0) as u32
}

/// Minimum luminance in cd/m2 to the protocol's 0.0001 cd/m2 unit
/// (`nits_to_u16_dark`).
pub fn nits_to_u16_dark(value: f32) -> u32 {
	(value * 10_000.0).round().clamp(0.0, 65_535.0) as u32
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A fake driver enumeration over `formats`, following the two-call idiom.
	fn driver(formats: &[u32]) -> impl Fn(*mut u32, *mut u32) -> VkResult + '_ {
		move |count, out| unsafe {
			if out.is_null() {
				*count = formats.len() as u32;
				return VkResult::SUCCESS;
			}
			let copied = (*count as usize).min(formats.len());
			for (i, format) in formats.iter().take(copied).enumerate() {
				*out.add(i) = *format;
			}
			*count = copied as u32;
			if copied < formats.len() {
				VkResult::INCOMPLETE
			} else {
				VkResult::SUCCESS
			}
		}
	}

	#[test]
	fn only_the_offered_colour_spaces_are_hdr() {
		assert!(is_hdr_colour_space(ColorSpaceKHR::HDR10_ST2084_EXT));
		assert!(is_hdr_colour_space(ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT));
		assert!(!is_hdr_colour_space(ColorSpaceKHR::SRGB_NONLINEAR));
		assert!(!is_hdr_colour_space(ColorSpaceKHR::DISPLAY_P3_NONLINEAR_EXT));
		assert!(!is_hdr_colour_space(ColorSpaceKHR::HDR10_HLG_EXT));
	}

	#[test]
	fn the_driver_always_gets_srgb() {
		assert_eq!(DRIVER_COLOUR_SPACE, ColorSpaceKHR::SRGB_NONLINEAR);
	}

	#[test]
	fn the_hdr_list_has_pq_ten_bit_and_scrgb_half_float() {
		assert!(HDR_SURFACE_FORMATS.contains(&(Format::A2B10G10R10_UNORM_PACK32, ColorSpaceKHR::HDR10_ST2084_EXT)));
		assert!(HDR_SURFACE_FORMATS.contains(&(Format::A2R10G10B10_UNORM_PACK32, ColorSpaceKHR::HDR10_ST2084_EXT)));
		assert!(HDR_SURFACE_FORMATS.contains(&(Format::R16G16B16A16_SFLOAT, ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT)));
	}

	#[test]
	fn the_count_query_adds_the_extras() {
		let mut count = 0u32;
		let result = unsafe { append_extra(&[100, 101, 102], &mut count, std::ptr::null_mut(), driver(&[1, 2])) };
		assert_eq!(result, VkResult::SUCCESS);
		assert_eq!(count, 5);
	}

	#[test]
	fn a_full_buffer_gets_the_driver_formats_then_the_extras() {
		let mut out = [0u32; 5];
		let mut count = 5u32;
		let result = unsafe { append_extra(&[100, 101, 102], &mut count, out.as_mut_ptr(), driver(&[1, 2])) };
		assert_eq!(result, VkResult::SUCCESS);
		assert_eq!(count, 5);
		assert_eq!(out, [1, 2, 100, 101, 102]);
	}

	#[test]
	fn a_short_buffer_is_incomplete_and_keeps_the_extras_first_in_line() {
		let mut out = [0u32; 3];
		let mut count = 3u32;
		let result = unsafe { append_extra(&[100, 101, 102], &mut count, out.as_mut_ptr(), driver(&[1, 2])) };
		assert_eq!(result, VkResult::INCOMPLETE);
		assert_eq!(count, 3);
		assert_eq!(out, [100, 101, 102]);
	}

	#[test]
	fn a_driver_error_is_passed_on() {
		let mut count = 4u32;
		let mut out = [0u32; 4];
		let result = unsafe {
			append_extra(&[100], &mut count, out.as_mut_ptr(), |_, _| {
				VkResult::ERROR_SURFACE_LOST_KHR
			})
		};
		assert_eq!(result, VkResult::ERROR_SURFACE_LOST_KHR);
	}

	#[test]
	fn write_array_answers_the_two_call_idiom() {
		let mut count = 0u32;
		assert_eq!(
			unsafe { write_array(&[7u32, 8, 9], &mut count, std::ptr::null_mut()) },
			VkResult::SUCCESS
		);
		assert_eq!(count, 3);

		let mut out = [0u32; 2];
		let mut count = 2u32;
		assert_eq!(
			unsafe { write_array(&[7u32, 8, 9], &mut count, out.as_mut_ptr()) },
			VkResult::INCOMPLETE
		);
		assert_eq!(out, [7, 8]);
		assert_eq!(count, 2);
	}

	#[test]
	fn chromaticities_use_units_of_two_hundred_thousandths() {
		assert_eq!(colour_xy_to_u16(0.0), 0);
		assert_eq!(colour_xy_to_u16(1.0), 50_000);
		// BT.2020 red.
		assert_eq!(colour_xy_to_u16(0.708), 35_400);
		assert_eq!(colour_xy_to_u16(0.292), 14_600);
		assert_eq!(colour_xy_to_u16(-1.0), 0);
		assert_eq!(colour_xy_to_u16(2.0), 65_535);
	}

	#[test]
	fn luminance_conversions_round_and_clamp() {
		assert_eq!(nits_to_u16(0.0), 0);
		assert_eq!(nits_to_u16(1000.0), 1000);
		assert_eq!(nits_to_u16(100_000.0), 65_535);
		assert_eq!(nits_to_u16_dark(0.0), 0);
		assert_eq!(nits_to_u16_dark(1.0), 10_000);
		// A typical OLED black.
		assert_eq!(nits_to_u16_dark(0.005), 50);
	}
}
