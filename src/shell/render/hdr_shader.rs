//! SDR content composited over an HDR output.
//!
//! The cursor, Steam overlays and notifications are sRGB surfaces. When the
//! output carries HDR10 PQ or scRGB they are converted to the output encoding
//! as they are drawn, so SDR white lands at the user's SDR white level
//! (`GAMESCOPE_SDR_ON_HDR_CONTENT_BRIGHTNESS`, default 203 nits).
//!
//! gamescope draws SDR content in an HDR output as gamma 2.2 with the SDR
//! white luminance (`tonemapping.g22_luminance = flSDROnHDRBrightness`,
//! `steamcompmgr.cpp:375`), converts BT.709 to BT.2020 for PQ, and scales
//! scRGB so 1.0 is 80 nits. The same maths run here in a custom texture
//! program of smithay's Vulkan renderer (`shader/sdr_on_hdr.comp`).

use smithay::{
	backend::renderer::{
		element::{Element, Id, Kind, RenderElement, UnderlyingStorage},
		utils::{CommitCounter, DamageSet, OpaqueRegions},
		vulkan::{Error as VulkanError, VulkanFrame, VulkanRenderer, VulkanTexProgram},
	},
	utils::{Buffer, Physical, Point, Rectangle, Scale, Transform, user_data::UserDataMap},
};

use crate::{
	error::{Error, Result},
	hdr::ContentKind,
};

/// The conversion shader, compiled by `shader/compile.sh`.
const SPIRV: &[u8] = include_bytes!("../../../shader/sdr_on_hdr.spv");

/// The shader's custom uniforms, after the renderer's own push constants.
///
/// Matches the `sdrWhiteNits` and `pqOutput` members of the shader's push
/// constant block.
#[derive(Debug, Clone, Copy)]
struct Uniforms {
	/// SDR white in nits.
	sdr_white_nits: f32,
	/// 1.0 for a PQ output, 0.0 for scRGB.
	pq_output: f32,
}

impl Uniforms {
	const SIZE: usize = 8;

	fn to_bytes(self) -> Vec<u8> {
		[self.sdr_white_nits, self.pq_output]
			.iter()
			.flat_map(|value| value.to_ne_bytes())
			.collect()
	}
}

/// Compile the conversion shader.
pub(in crate::shell) fn compile(renderer: &mut VulkanRenderer) -> Result<VulkanTexProgram> {
	let words: Vec<u32> = SPIRV
		.as_chunks::<4>()
		.0
		.iter()
		.map(|word| u32::from_le_bytes(*word))
		.collect();
	renderer
		.compile_custom_texture_shader(&words, Uniforms::SIZE)
		.map_err(|e| Error::Gpu(format!("compile the SDR-on-HDR shader: {e:?}")))
}

/// The conversion to apply to SDR elements: a compiled shader and its uniforms.
#[derive(Debug, Clone)]
pub(in crate::shell) struct Conversion {
	program: VulkanTexProgram,
	uniforms: Vec<u8>,
}

impl Conversion {
	/// The conversion into an `output` encoding with SDR white at `sdr_white_nits`;
	/// `None` for an SDR output, which needs none.
	pub(in crate::shell) fn new(program: &VulkanTexProgram, output: ContentKind, sdr_white_nits: f32) -> Option<Self> {
		let pq_output = match output {
			ContentKind::Sdr => return None,
			ContentKind::Pq => 1.0f32,
			ContentKind::ScRgb => 0.0,
		};
		Some(Self {
			program: program.clone(),
			uniforms: Uniforms {
				sdr_white_nits,
				pq_output,
			}
			.to_bytes(),
		})
	}

	/// Draw the following textures of `frame` through this conversion.
	fn apply(&self, frame: &mut VulkanFrame<'_, '_>) -> std::result::Result<(), VulkanError> {
		frame.override_default_tex_program(self.program.clone(), self.uniforms.clone())
	}
}

/// An element drawn through [`Conversion`] when there is one.
#[derive(Debug)]
pub(in crate::shell) struct SdrOnHdr<E> {
	inner: E,
	conversion: Option<Conversion>,
}

impl<E> SdrOnHdr<E> {
	pub(in crate::shell) fn new(inner: E, conversion: Option<Conversion>) -> Self {
		Self { inner, conversion }
	}
}

impl<E: Element> Element for SdrOnHdr<E> {
	fn id(&self) -> &Id {
		self.inner.id()
	}

	fn current_commit(&self) -> CommitCounter {
		self.inner.current_commit()
	}

	fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
		self.inner.location(scale)
	}

	fn src(&self) -> Rectangle<f64, Buffer> {
		self.inner.src()
	}

	fn transform(&self) -> Transform {
		self.inner.transform()
	}

	fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
		self.inner.geometry(scale)
	}

	fn damage_since(&self, scale: Scale<f64>, commit: Option<CommitCounter>) -> DamageSet<i32, Physical> {
		self.inner.damage_since(scale, commit)
	}

	fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
		self.inner.opaque_regions(scale)
	}

	fn alpha(&self) -> f32 {
		self.inner.alpha()
	}

	fn kind(&self) -> Kind {
		self.inner.kind()
	}
}

impl<E: RenderElement<VulkanRenderer>> RenderElement<VulkanRenderer> for SdrOnHdr<E> {
	fn draw(
		&self,
		frame: &mut VulkanFrame<'_, '_>,
		src: Rectangle<f64, Buffer>,
		dst: Rectangle<i32, Physical>,
		damage: &[Rectangle<i32, Physical>],
		opaque_regions: &[Rectangle<i32, Physical>],
		cache: Option<&UserDataMap>,
	) -> std::result::Result<(), VulkanError> {
		let Some(conversion) = &self.conversion else {
			return self.inner.draw(frame, src, dst, damage, opaque_regions, cache);
		};
		conversion.apply(frame)?;
		let result = self.inner.draw(frame, src, dst, damage, opaque_regions, cache);
		frame.clear_tex_program_override();
		result
	}

	fn underlying_storage(&self, renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
		self.inner.underlying_storage(renderer)
	}
}

/// The conversion in Rust, for checking the shader against.
#[cfg(test)]
pub(in crate::shell) mod reference {
	/// SMPTE ST 2084 inverse EOTF of `nits`.
	pub fn pq(nits: f64) -> f64 {
		let (m1, m2) = (0.1593017578125, 78.84375);
		let (c1, c2, c3) = (0.8359375, 18.8515625, 18.6875);
		let y = (nits / 10000.0).clamp(0.0, 1.0).powf(m1);
		((c1 + c2 * y) / (1.0 + c3 * y)).powf(m2)
	}

	/// Linear BT.709 to linear BT.2020.
	pub fn bt709_to_bt2020(c: [f64; 3]) -> [f64; 3] {
		let row = |m: [f64; 3]| m[0] * c[0] + m[1] * c[1] + m[2] * c[2];
		[
			row([0.6274039, 0.3292830, 0.0433131]),
			row([0.0690973, 0.9195404, 0.0113623]),
			row([0.0163914, 0.0880133, 0.8955953]),
		]
	}

	/// An opaque sRGB pixel as it is drawn into a PQ output.
	pub fn sdr_to_pq(srgb: [f64; 3], sdr_white_nits: f64) -> [f64; 3] {
		let linear = srgb.map(|c| c.powf(2.2));
		bt709_to_bt2020(linear).map(|c| pq(c * sdr_white_nits))
	}

	/// An opaque sRGB pixel as it is drawn into a scRGB output, where 1.0 is
	/// 80 nits (IEC 61966-2-2).
	pub fn sdr_to_scrgb(srgb: [f64; 3], sdr_white_nits: f64) -> [f64; 3] {
		srgb.map(|c| c.powf(2.2) * sdr_white_nits / 80.0)
	}

	#[test]
	fn pq_anchors() {
		// ST 2084: 0 maps to (almost) 0, 10000 nits to 1, and 203 nits (BT.2408
		// reference white) to about 0.58.
		assert!(pq(0.0).abs() < 1e-5);
		assert!((pq(10000.0) - 1.0).abs() < 1e-9);
		assert!((pq(203.0) - 0.5807).abs() < 1e-3, "{}", pq(203.0));
	}

	#[test]
	fn white_stays_neutral_and_lands_at_the_sdr_white_level() {
		let white = sdr_to_pq([1.0, 1.0, 1.0], 203.0);
		for channel in white {
			assert!((channel - pq(203.0)).abs() < 1e-4, "{white:?}");
		}
		assert_eq!(sdr_to_scrgb([1.0, 1.0, 1.0], 80.0), [1.0, 1.0, 1.0]);
		assert!((sdr_to_scrgb([1.0, 0.0, 0.0], 160.0)[0] - 2.0).abs() < 1e-9);
	}
}

#[cfg(test)]
mod gpu_tests {
	use smithay::{
		backend::{
			allocator::{
				Allocator, Fourcc,
				dmabuf::{AsDmabuf, Dmabuf},
				gbm::GbmAllocator,
			},
			renderer::{Bind, ExportMem, Frame, ImportMem, Renderer},
		},
		utils::{Size, Transform},
	};

	use super::*;
	use crate::shell::render::elements::RenderFormat;

	const SIZE: i32 = 8;

	fn target(allocator: &mut GbmAllocator<std::sync::Arc<std::fs::File>>, format: &RenderFormat) -> Dmabuf {
		allocator
			.create_buffer(SIZE as u32, SIZE as u32, format.code, &format.modifiers)
			.expect("allocate the target")
			.export()
			.expect("export the target")
	}

	/// An IEEE 754 half-precision float.
	fn f16_to_f32(bits: u16) -> f32 {
		let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
		let exponent = i32::from((bits >> 10) & 0x1f);
		let mantissa = f32::from(bits & 0x3ff);
		sign * match exponent {
			0 => mantissa * 2f32.powi(-24),
			31 => f32::INFINITY,
			_ => (1.0 + mantissa / 1024.0) * 2f32.powi(exponent - 15),
		}
	}

	/// Draw an opaque 8x8 texture of `srgb` (RGBA bytes, premultiplied) through
	/// the conversion into an `output` target and read the RGBA floats back.
	fn convert(output: ContentKind, sdr_white_nits: f32, srgb: [u8; 4]) -> [f32; 4] {
		let (mut renderer, mut allocator) = crate::shell::render::gpu::test_renderer();
		let program = compile(&mut renderer).expect("compile");
		let conversion = Conversion::new(&program, output, sdr_white_nits).expect("an HDR output");
		let texture = renderer
			.import_memory(
				&srgb.repeat((SIZE * SIZE) as usize),
				Fourcc::Abgr8888,
				(SIZE, SIZE).into(),
				false,
			)
			.expect("import the texture");

		let format =
			crate::shell::render::elements::pick_hdr_format(&renderer, output).expect("a renderable HDR format");
		let mut dmabuf = target(&mut allocator, &format);
		let mut bound = renderer.bind(&mut dmabuf).expect("bind the target");
		{
			let size = Size::<i32, smithay::utils::Physical>::from((SIZE, SIZE));
			let mut frame = renderer.render(&mut bound, size, Transform::Normal).expect("frame");
			conversion.apply(&mut frame).expect("override the program");
			let full = smithay::utils::Rectangle::from_size(size);
			frame
				.render_texture_from_to(
					&texture,
					smithay::utils::Rectangle::from_size((SIZE as f64, SIZE as f64).into()),
					full,
					&[full],
					&[],
					Transform::Normal,
					1.0,
				)
				.expect("draw");
			frame.finish().expect("finish").wait().expect("wait");
		}

		let region = smithay::utils::Rectangle::from_size((SIZE, SIZE).into());
		let mapping = renderer
			.copy_framebuffer(&bound, region, format.code)
			.expect("read the target back");
		let bytes = renderer.map_texture(&mapping).expect("map the readback");
		match format.code {
			Fourcc::Abgr2101010 | Fourcc::Xbgr2101010 => {
				let word = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
				[
					(word & 0x3ff) as f32 / 1023.0,
					((word >> 10) & 0x3ff) as f32 / 1023.0,
					((word >> 20) & 0x3ff) as f32 / 1023.0,
					(word >> 30) as f32 / 3.0,
				]
			},
			_ => std::array::from_fn(|channel| {
				f16_to_f32(u16::from_le_bytes([bytes[channel * 2], bytes[channel * 2 + 1]]))
			}),
		}
	}

	fn srgb8(pixel: [u8; 4]) -> [f64; 3] {
		[pixel[0], pixel[1], pixel[2]].map(|c| f64::from(c) / 255.0)
	}

	#[test]
	#[ignore = "requires GPU"]
	fn the_pq_conversion_matches_the_reference() {
		for (pixel, nits) in [
			([255, 255, 255, 255], 203.0f32),
			([255, 0, 0, 255], 203.0),
			([40, 120, 200, 255], 300.0),
			([128, 128, 128, 255], 100.0),
		] {
			let got = convert(ContentKind::Pq, nits, pixel);
			let want = reference::sdr_to_pq(srgb8(pixel), f64::from(nits));
			for channel in 0..3 {
				// One 10-bit step plus the float rounding of the shader.
				assert!(
					(f64::from(got[channel]) - want[channel]).abs() < 1.5 / 1023.0,
					"{pixel:?} at {nits} nits, channel {channel}: {got:?} vs {want:?}"
				);
			}
			assert!((got[3] - 1.0).abs() < 1e-6);
		}
	}

	#[test]
	#[ignore = "requires GPU"]
	fn the_scrgb_conversion_matches_the_reference_and_exceeds_one() {
		for (pixel, nits) in [
			([255, 255, 255, 255], 203.0f32),
			([255, 0, 0, 255], 80.0),
			([200, 100, 50, 255], 400.0),
		] {
			let got = convert(ContentKind::ScRgb, nits, pixel);
			let want = reference::sdr_to_scrgb(srgb8(pixel), f64::from(nits));
			for channel in 0..3 {
				assert!(
					(f64::from(got[channel]) - want[channel]).abs() < 2e-3 * want[channel].max(1.0),
					"{pixel:?} at {nits} nits, channel {channel}: {got:?} vs {want:?}"
				);
			}
		}
		// SDR white at 203 nits is above scRGB's 1.0.
		assert!(convert(ContentKind::ScRgb, 203.0, [255, 255, 255, 255])[0] > 2.5);
	}

	#[test]
	#[ignore = "requires GPU"]
	fn partially_transparent_pixels_convert_their_colour_not_their_coverage() {
		// Premultiplied 50% white: the colour is white, the coverage half.
		let got = convert(ContentKind::ScRgb, 80.0, [128, 128, 128, 128]);
		assert!((got[0] - 128.0 / 255.0).abs() < 3e-3, "{got:?}");
		assert!((got[3] - 128.0 / 255.0).abs() < 3e-3, "{got:?}");
	}
}
