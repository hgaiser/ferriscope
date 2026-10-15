//! Colour space and HDR mastering metadata reported to the sink.
//!
//! ferriscope does not convert colour: the encoder does. The
//! compositor only *describes* the application's colour space and mastering
//! metadata so the pipeline can decide what to do with it.
//!
//! The vocabulary mirrors the DRM/CEA HDMI infoframe enumerations, because that
//! is what the gamescope reference reports and what the encoder consumes.

/// Colour primaries (the RGB gamut) of the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColourPrimaries {
	/// ITU-R BT.709 / sRGB primaries. The SDR default.
	Bt709,
	/// ITU-R BT.2020 primaries. The HDR10 default.
	Bt2020,
}

/// Opto-electronic transfer function of the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferFunction {
	/// Traditional SDR transfer (BT.1886 / sRGB).
	Sdr,
	/// SMPTE ST 2084 (PQ), used by HDR10.
	Pq,
	/// Hybrid log-gamma.
	Hlg,
	/// Linear light.
	Linear,
}

/// Matrix coefficients used for YCbCr, when the frame is not RGB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixCoefficients {
	/// RGB, no matrix. The compositor's normal case.
	Identity,
	/// BT.709 YCbCr.
	Bt709,
	/// BT.2020 non-constant luminance YCbCr.
	Bt2020Ncl,
}

/// Quantisation range of the encoded values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColourRange {
	/// Studio/limited range (16-235 for 8-bit).
	Limited,
	/// Full range (0-255 for 8-bit).
	Full,
}

/// A CIE 1931 chromaticity coordinate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chromaticity {
	/// CIE x.
	pub x: f32,
	/// CIE y.
	pub y: f32,
}

impl Chromaticity {
	/// Construct a chromaticity from its coordinates.
	pub const fn new(x: f32, y: f32) -> Self {
		Self { x, y }
	}
}

/// HDR mastering display metadata (SMPTE ST 2086).
///
/// Chromaticities are in CIE 1931; luminances are in cd/m².
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MasteringDisplay {
	/// Red primary.
	pub red: Chromaticity,
	/// Green primary.
	pub green: Chromaticity,
	/// Blue primary.
	pub blue: Chromaticity,
	/// White point.
	pub white_point: Chromaticity,
	/// Minimum mastering luminance, in cd/m².
	pub min_luminance: f32,
	/// Maximum mastering luminance, in cd/m².
	pub max_luminance: f32,
}

impl MasteringDisplay {
	/// The BT.2020 / D65 mastering display used by HDR10 content.
	pub const fn bt2020_d65(max_luminance: f32, min_luminance: f32) -> Self {
		Self {
			red: Chromaticity::new(0.708, 0.292),
			green: Chromaticity::new(0.170, 0.797),
			blue: Chromaticity::new(0.131, 0.046),
			white_point: Chromaticity::new(0.3127, 0.3290),
			min_luminance,
			max_luminance,
		}
	}
}

/// Content light level metadata (CTA-861.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentLightLevel {
	/// Maximum content light level, in cd/m².
	pub max_cll: u16,
	/// Maximum frame-average light level, in cd/m².
	pub max_fall: u16,
}

/// A complete description of a frame's colour space.
///
/// The default is SDR BT.709 full-range RGB, which is what an unannotated
/// application is assumed to produce.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColourMetadata {
	/// Colour primaries.
	pub primaries: ColourPrimaries,
	/// Transfer function.
	pub transfer: TransferFunction,
	/// Matrix coefficients.
	pub matrix: MatrixCoefficients,
	/// Quantisation range.
	pub range: ColourRange,
	/// HDR mastering display metadata, if any.
	pub mastering: Option<MasteringDisplay>,
	/// Content light level, if any.
	pub content_light: Option<ContentLightLevel>,
}

impl Default for ColourMetadata {
	fn default() -> Self {
		Self::sdr()
	}
}

impl ColourMetadata {
	/// SDR BT.709 full-range RGB: the unannotated default.
	pub const fn sdr() -> Self {
		Self {
			primaries: ColourPrimaries::Bt709,
			transfer: TransferFunction::Sdr,
			matrix: MatrixCoefficients::Identity,
			range: ColourRange::Full,
			mastering: None,
			content_light: None,
		}
	}

	/// HDR10: PQ transfer, BT.2020 primaries, with mastering + CLL metadata.
	pub const fn hdr10(mastering: MasteringDisplay, content_light: Option<ContentLightLevel>) -> Self {
		Self {
			primaries: ColourPrimaries::Bt2020,
			transfer: TransferFunction::Pq,
			matrix: MatrixCoefficients::Identity,
			range: ColourRange::Limited,
			mastering: Some(mastering),
			content_light,
		}
	}

	/// True if the frame is HDR: PQ, HLG, or the extended-range linear light
	/// of scRGB (BT.709 primaries, [`TransferFunction::Linear`]).
	pub fn is_hdr(&self) -> bool {
		matches!(
			self.transfer,
			TransferFunction::Pq | TransferFunction::Hlg | TransferFunction::Linear
		)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn default_is_sdr() {
		let meta = ColourMetadata::default();
		assert_eq!(meta.primaries, ColourPrimaries::Bt709);
		assert_eq!(meta.transfer, TransferFunction::Sdr);
		assert_eq!(meta.matrix, MatrixCoefficients::Identity);
		assert_eq!(meta.range, ColourRange::Full);
		assert!(!meta.is_hdr());
	}

	#[test]
	fn hdr10_is_hdr_with_metadata() {
		let meta = ColourMetadata::hdr10(MasteringDisplay::bt2020_d65(1000.0, 0.005), None);
		assert!(meta.is_hdr());
		assert_eq!(meta.primaries, ColourPrimaries::Bt2020);
		assert_eq!(meta.transfer, TransferFunction::Pq);
		assert_eq!(meta.mastering.unwrap().max_luminance, 1000.0);
	}

	#[test]
	fn hlg_is_hdr() {
		let mut meta = ColourMetadata::sdr();
		meta.transfer = TransferFunction::Hlg;
		assert!(meta.is_hdr());
	}

	#[test]
	fn content_light_roundtrips() {
		let cll = ContentLightLevel {
			max_cll: 1000,
			max_fall: 400,
		};
		let meta = ColourMetadata::hdr10(MasteringDisplay::bt2020_d65(1000.0, 0.005), Some(cll));
		assert_eq!(meta.content_light, Some(cll));
	}
}
