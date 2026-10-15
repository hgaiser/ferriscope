//! What a window's content is, colour-wise, and how that is reported.
//!
//! Two sources describe the colour of a surface and they are combined with
//! gamescope's precedence: the WSI swapchain's `VkColorSpaceKHR` and
//! `vkSetHdrMetadataEXT` data (`swapchain`) are authoritative when the layer
//! declares an HDR colour space; the `wp_color_management_v1` /
//! `frog_color_management_v1` image description (`colour_management`) is used
//! otherwise. gamescope maps a colour space to a texture colour space in
//! `VkColorSpaceToGamescopeAppTextureColorSpace` (`rendervulkan.hpp:109`) and
//! treats scRGB and HDR10 PQ as HDR (`ColorspaceIsHDR`, `gamescope_shared.h:35`).

use crate::{
	colour_meta::{
		Chromaticity, ColourMetadata, ColourPrimaries, ColourRange, ContentLightLevel, MasteringDisplay,
		MatrixCoefficients, TransferFunction,
	},
	swapchain::{HdrMetadata, SwapchainFeedback},
};

/// `VK_COLOR_SPACE_EXTENDED_SRGB_LINEAR_EXT`: scRGB.
pub const VK_COLOR_SPACE_EXTENDED_SRGB_LINEAR: u32 = 1_000_104_002;
/// `VK_COLOR_SPACE_HDR10_ST2084_EXT`: HDR10 PQ.
pub const VK_COLOR_SPACE_HDR10_ST2084: u32 = 1_000_104_008;

/// The SDR white level when Steam has not set one: BT.2408's 203 nits, what
/// gamescope uses for `flSDROnHDRBrightness` (`steamcompmgr.cpp:728`).
pub const DEFAULT_SDR_WHITE_NITS: f32 = 203.0;

/// The SDR white level in nits from `GAMESCOPE_SDR_ON_HDR_CONTENT_BRIGHTNESS`,
/// an `f32` stored as its bit pattern in a `CARDINAL`. A missing property or a
/// value below one nit means the default (`set_sdr_on_hdr_brightness`,
/// `steamcompmgr.cpp:723`).
pub fn sdr_white_nits(raw: Option<u32>) -> f32 {
	match raw.map(f32::from_bits) {
		Some(nits) if nits >= 1.0 => nits,
		_ => DEFAULT_SDR_WHITE_NITS,
	}
}

/// The encoding of a surface's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContentKind {
	/// sRGB / BT.709 primaries.
	#[default]
	Sdr,
	/// HDR10: BT.2020 primaries, SMPTE ST 2084 (PQ).
	Pq,
	/// scRGB: BT.709 primaries, extended-range linear light where 1.0 is
	/// 80 cd/m2.
	ScRgb,
}

impl ContentKind {
	/// Whether the content is HDR.
	pub fn is_hdr(self) -> bool {
		self != Self::Sdr
	}

	/// The kind a swapchain's `VkColorSpaceKHR` stands for. Anything that is
	/// not scRGB or HDR10 PQ is handled as SDR, like gamescope.
	pub fn from_vk_colorspace(colorspace: u32) -> Self {
		match colorspace {
			VK_COLOR_SPACE_EXTENDED_SRGB_LINEAR => Self::ScRgb,
			VK_COLOR_SPACE_HDR10_ST2084 => Self::Pq,
			_ => Self::Sdr,
		}
	}
}

/// HDR static metadata where each part may be missing, as the colour
/// management protocols let a client set the parts separately.
///
/// Units follow CTA-861.G like [`HdrMetadata`]: chromaticities in 0.00002,
/// luminances in cd/m2 except the minimum, in 0.0001 cd/m2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContentMetadata {
	/// Mastering display red, green and blue primaries.
	pub mastering_primaries: Option<[(u32, u32); 3]>,
	/// Mastering display white point.
	pub white_point: Option<(u32, u32)>,
	/// Mastering display `(min, max)` luminance.
	pub mastering_luminance: Option<(u32, u32)>,
	/// Maximum content light level.
	pub max_cll: Option<u32>,
	/// Maximum frame-average light level.
	pub max_fall: Option<u32>,
}

impl From<HdrMetadata> for ContentMetadata {
	fn from(metadata: HdrMetadata) -> Self {
		Self {
			mastering_primaries: Some(metadata.primaries),
			white_point: Some(metadata.white_point),
			mastering_luminance: Some((
				metadata.min_display_mastering_luminance,
				metadata.max_display_mastering_luminance,
			)),
			max_cll: Some(metadata.max_cll),
			max_fall: Some(metadata.max_fall),
		}
	}
}

impl ContentMetadata {
	/// Whether nothing is set.
	pub fn is_empty(&self) -> bool {
		*self == Self::default()
	}

	/// The mastering display, when the metadata names a luminance range.
	/// Missing primaries and white point default to BT.2020 / D65, the HDR10
	/// mastering display.
	fn mastering_display(&self) -> Option<MasteringDisplay> {
		let (min, max) = self.mastering_luminance?;
		let mut display = MasteringDisplay::bt2020_d65(max as f32, min as f32 * 0.0001);
		let chroma = |(x, y): (u32, u32)| Chromaticity::new(x as f32 * 0.00002, y as f32 * 0.00002);
		if let Some([red, green, blue]) = self.mastering_primaries {
			display.red = chroma(red);
			display.green = chroma(green);
			display.blue = chroma(blue);
		}
		if let Some(white) = self.white_point {
			display.white_point = chroma(white);
		}
		Some(display)
	}

	fn content_light(&self) -> Option<ContentLightLevel> {
		if self.max_cll.is_none() && self.max_fall.is_none() {
			return None;
		}
		let saturate = |value: Option<u32>| value.map_or(0, |value| u16::try_from(value).unwrap_or(u16::MAX));
		Some(ContentLightLevel {
			max_cll: saturate(self.max_cll),
			max_fall: saturate(self.max_fall),
		})
	}
}

/// A surface's resolved colour: its encoding and any HDR metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContentColour {
	/// How the pixels are encoded.
	pub kind: ContentKind,
	/// HDR metadata; empty for SDR.
	pub metadata: ContentMetadata,
}

impl ContentColour {
	/// Combine the WSI swapchain's report with the image description a client
	/// set through a colour management protocol.
	///
	/// A swapchain that declares scRGB or HDR10 decides the encoding; its
	/// metadata wins over the description's. A swapchain that is sRGB leaves
	/// the decision to the description.
	pub fn resolve(swapchain: Option<&SwapchainFeedback>, described: Option<ContentColour>) -> Self {
		let swapchain_kind = swapchain.map(|feedback| ContentKind::from_vk_colorspace(feedback.vk_colorspace));
		let kind = match swapchain_kind {
			Some(kind) if kind.is_hdr() => kind,
			_ => described.map_or(ContentKind::Sdr, |described| described.kind),
		};
		if !kind.is_hdr() {
			return Self::default();
		}
		let metadata = swapchain
			.and_then(|feedback| feedback.hdr_metadata)
			.map(ContentMetadata::from)
			.or_else(|| {
				described
					.map(|described| described.metadata)
					.filter(|metadata| !metadata.is_empty())
			})
			.unwrap_or_default();
		Self { kind, metadata }
	}

	/// The colour space reported with a frame whose pixels hold this content
	/// unconverted (the frame carries the encoding of its pool format).
	pub fn frame_metadata(&self) -> ColourMetadata {
		let (primaries, transfer) = match self.kind {
			ContentKind::Sdr => return ColourMetadata::sdr(),
			ContentKind::Pq => (ColourPrimaries::Bt2020, TransferFunction::Pq),
			ContentKind::ScRgb => (ColourPrimaries::Bt709, TransferFunction::Linear),
		};
		ColourMetadata {
			primaries,
			transfer,
			matrix: MatrixCoefficients::Identity,
			range: ColourRange::Full,
			mastering: self.metadata.mastering_display(),
			content_light: self.metadata.content_light(),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::swapchain::VK_COLOR_SPACE_SRGB_NONLINEAR;

	fn feedback(colorspace: u32, metadata: Option<HdrMetadata>) -> SwapchainFeedback {
		SwapchainFeedback {
			image_count: 3,
			vk_format: 64,
			vk_colorspace: colorspace,
			vk_composite_alpha: 1,
			vk_pre_transform: 1,
			vk_clipped: true,
			engine_name: String::new(),
			hdr_metadata: metadata,
		}
	}

	fn swapchain_metadata() -> HdrMetadata {
		HdrMetadata {
			primaries: [(34000, 16000), (13250, 34500), (7500, 3000)],
			white_point: (15635, 16450),
			max_display_mastering_luminance: 1000,
			min_display_mastering_luminance: 50,
			max_cll: 900,
			max_fall: 300,
		}
	}

	fn described(kind: ContentKind) -> ContentColour {
		ContentColour {
			kind,
			metadata: ContentMetadata {
				max_cll: Some(4000),
				..ContentMetadata::default()
			},
		}
	}

	#[test]
	fn vulkan_colour_spaces_map_like_gamescope() {
		assert_eq!(
			ContentKind::from_vk_colorspace(VK_COLOR_SPACE_SRGB_NONLINEAR),
			ContentKind::Sdr
		);
		assert_eq!(
			ContentKind::from_vk_colorspace(VK_COLOR_SPACE_HDR10_ST2084),
			ContentKind::Pq
		);
		assert_eq!(
			ContentKind::from_vk_colorspace(VK_COLOR_SPACE_EXTENDED_SRGB_LINEAR),
			ContentKind::ScRgb
		);
		// Display P3 and friends are not HDR to gamescope.
		assert_eq!(ContentKind::from_vk_colorspace(1_000_104_001), ContentKind::Sdr);
	}

	#[test]
	fn the_sdr_white_level_defaults_like_gamescope() {
		assert_eq!(sdr_white_nits(None), 203.0);
		assert_eq!(sdr_white_nits(Some(0)), 203.0);
		assert_eq!(sdr_white_nits(Some(0.5f32.to_bits())), 203.0);
		assert_eq!(sdr_white_nits(Some(300.0f32.to_bits())), 300.0);
		assert_eq!(sdr_white_nits(Some(f32::NAN.to_bits())), 203.0);
	}

	#[test]
	fn nothing_declared_is_sdr() {
		assert_eq!(ContentColour::resolve(None, None), ContentColour::default());
		let sdr = feedback(VK_COLOR_SPACE_SRGB_NONLINEAR, Some(swapchain_metadata()));
		assert_eq!(ContentColour::resolve(Some(&sdr), None).kind, ContentKind::Sdr);
		// Metadata alone never makes content HDR.
		assert!(ContentColour::resolve(Some(&sdr), None).metadata.is_empty());
	}

	#[test]
	fn an_hdr_swapchain_beats_the_description() {
		let pq = feedback(VK_COLOR_SPACE_HDR10_ST2084, Some(swapchain_metadata()));
		let resolved = ContentColour::resolve(Some(&pq), Some(described(ContentKind::ScRgb)));
		assert_eq!(resolved.kind, ContentKind::Pq);
		assert_eq!(resolved.metadata.max_cll, Some(900));
	}

	#[test]
	fn the_description_decides_when_the_swapchain_is_sdr() {
		let sdr = feedback(VK_COLOR_SPACE_SRGB_NONLINEAR, None);
		let resolved = ContentColour::resolve(Some(&sdr), Some(described(ContentKind::Pq)));
		assert_eq!(resolved.kind, ContentKind::Pq);
		assert_eq!(resolved.metadata.max_cll, Some(4000));
	}

	#[test]
	fn the_description_supplies_metadata_the_swapchain_lacks() {
		let pq = feedback(VK_COLOR_SPACE_HDR10_ST2084, None);
		let resolved = ContentColour::resolve(Some(&pq), Some(described(ContentKind::Pq)));
		assert_eq!(resolved.metadata.max_cll, Some(4000));
	}

	#[test]
	fn frame_metadata_describes_the_encoding() {
		assert_eq!(ContentColour::default().frame_metadata(), ColourMetadata::sdr());

		let pq = ContentColour {
			kind: ContentKind::Pq,
			metadata: swapchain_metadata().into(),
		}
		.frame_metadata();
		assert_eq!(pq.primaries, ColourPrimaries::Bt2020);
		assert_eq!(pq.transfer, TransferFunction::Pq);
		let mastering = pq.mastering.expect("mastering display");
		assert_eq!(mastering.max_luminance, 1000.0);
		assert!((mastering.min_luminance - 0.005).abs() < 1e-6);
		assert!((mastering.red.x - 0.68).abs() < 1e-4);
		assert!((mastering.white_point.y - 0.329).abs() < 1e-4);
		assert_eq!(
			pq.content_light,
			Some(ContentLightLevel {
				max_cll: 900,
				max_fall: 300
			})
		);

		let scrgb = ContentColour {
			kind: ContentKind::ScRgb,
			metadata: ContentMetadata::default(),
		}
		.frame_metadata();
		assert_eq!(scrgb.primaries, ColourPrimaries::Bt709);
		assert_eq!(scrgb.transfer, TransferFunction::Linear);
		assert!(scrgb.is_hdr());
		assert_eq!(scrgb.mastering, None);
		assert_eq!(scrgb.content_light, None);
	}

	#[test]
	fn light_levels_saturate_to_sixteen_bits() {
		let metadata = ContentMetadata {
			max_cll: Some(100_000),
			..ContentMetadata::default()
		};
		assert_eq!(
			metadata.content_light(),
			Some(ContentLightLevel {
				max_cll: u16::MAX,
				max_fall: 0
			})
		);
	}
}
