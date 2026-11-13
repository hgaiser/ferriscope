//! Steam screenshots: `GAMESCOPECTRL_REQUEST_SCREENSHOT`.
//!
//! Steam asks for a screenshot by setting that property on the X root window.
//! gamescope composites the next frame, writes it to `/tmp/gamescope.png` and
//! deletes the property (`steamcompmgr.cpp:7221`, `:4029`). Steam reads the file
//! once the property is gone. Without an answer Steam plays its shutter sound,
//! but shows no popup and stores no screenshot.
//!
//! ferriscope always captures the full composition, whatever the requested
//! screenshot type, so the base-plane-only and screen-buffer types give the
//! same image. A request that cannot be satisfied (an HDR pool format the
//! readback cannot convert, a write error) is logged and still finished, so
//! Steam does not wait on it.

use std::{fs::File, io::BufWriter, path::Path};

use smithay::{
	backend::renderer::{Bind, ExportMem},
	utils::{Buffer as BufferCoord, Rectangle, Size},
};

use crate::{
	error::{Error, Result},
	shell::{
		FerriscopeState,
		render::{STEAM_SCREENSHOT_PATH, copy_top_down},
	},
};

impl FerriscopeState {
	/// Read the composited pool slot `id` back and finish the pending
	/// screenshot with it.
	///
	/// `sync` is the composite's GPU work; it is waited on before the read.
	pub(in crate::shell) fn capture_pool_slot_screenshot(
		&mut self,
		id: usize,
		sync: &smithay::backend::renderer::sync::SyncPoint,
		size: Size<i32, BufferCoord>,
	) {
		let pixels = self.read_pool_slot(id, sync, size);
		self.finish_screenshot(size.w as u32, size.h as u32, pixels.as_deref().map_err(|e| e.to_string()));
	}

	fn read_pool_slot(
		&mut self,
		id: usize,
		sync: &smithay::backend::renderer::sync::SyncPoint,
		size: Size<i32, BufferCoord>,
	) -> Result<Vec<u8>> {
		if !sync.is_reached() {
			sync.wait().map_err(|e| Error::Render(format!("screenshot fence wait: {e:?}")))?;
		}
		let dmabuf = self.pool.slots[id]
			.as_mut()
			.ok_or_else(|| Error::Render("dmabuf slot missing".into()))?;
		let target = Bind::bind(&mut self.renderer, dmabuf).map_err(|e| Error::Render(format!("bind dmabuf: {e:?}")))?;
		let region = Rectangle::<i32, BufferCoord>::from_size(size);
		let mapping = self
			.renderer
			.copy_framebuffer(&target, region, smithay::backend::allocator::Fourcc::Abgr8888)
			.map_err(|e| Error::Render(format!("readback: {e:?}")))?;
		drop(target);
		let slice = self
			.renderer
			.map_texture(&mapping)
			.map_err(|e| Error::Render(format!("map: {e:?}")))?;
		Ok(copy_top_down(slice, size.w as usize, size.h as usize))
	}

	/// Write the screenshot (RGBA8 pixels, top row first) and tell Steam the
	/// request is finished, whether or not it succeeded.
	pub(in crate::shell) fn finish_screenshot<E: std::fmt::Display>(
		&mut self,
		width: u32,
		height: u32,
		pixels: std::result::Result<&[u8], E>,
	) {
		self.pending_screenshot = false;
		match pixels {
			Ok(pixels) => match write_png(Path::new(STEAM_SCREENSHOT_PATH), width, height, pixels) {
				Ok(()) => tracing::info!(path = STEAM_SCREENSHOT_PATH, width, height, "screenshot written"),
				Err(e) => tracing::warn!(path = STEAM_SCREENSHOT_PATH, "screenshot not written: {e}"),
			},
			Err(e) => tracing::warn!("screenshot not captured: {e}"),
		}
		if let Some(steam) = self.steam.as_ref() {
			steam.finish_screenshot();
		}
	}
}

/// Encode RGBA8 `pixels` as an opaque RGB PNG at `path`.
///
/// The file is written beside the target and renamed over it, so Steam never
/// reads a half-written image.
fn write_png(path: &Path, width: u32, height: u32, pixels: &[u8]) -> std::io::Result<()> {
	let expected = width as usize * height as usize * 4;
	if pixels.len() < expected {
		return Err(std::io::Error::other(format!(
			"pixel buffer too short: {} < {expected}",
			pixels.len()
		)));
	}
	let temp = path.with_extension("png.tmp");
	{
		let mut encoder = png::Encoder::new(BufWriter::new(File::create(&temp)?), width, height);
		encoder.set_color(png::ColorType::Rgb);
		encoder.set_depth(png::BitDepth::Eight);
		encoder.set_compression(png::Compression::Fast);
		let mut writer = encoder.write_header().map_err(std::io::Error::other)?;
		let rgb: Vec<u8> = pixels[..expected]
			.as_chunks::<4>()
			.0
			.iter()
			.flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
			.collect();
		writer.write_image_data(&rgb).map_err(std::io::Error::other)?;
		writer.finish().map_err(std::io::Error::other)?;
	}
	std::fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_png_round_trips_through_the_writer() {
		let dir = std::env::temp_dir().join(format!("ferriscope-shot-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("shot.png");
		// 2x1: red, then green; alpha is dropped.
		write_png(&path, 2, 1, &[255, 0, 0, 255, 0, 255, 0, 7]).unwrap();
		let decoder = png::Decoder::new(std::io::BufReader::new(File::open(&path).unwrap()));
		let mut reader = decoder.read_info().unwrap();
		let mut buf = vec![0; reader.output_buffer_size().unwrap()];
		let info = reader.next_frame(&mut buf).unwrap();
		assert_eq!((info.width, info.height), (2, 1));
		assert_eq!(&buf[..6], &[255, 0, 0, 0, 255, 0]);
		std::fs::remove_dir_all(&dir).unwrap();
	}

	#[test]
	fn a_short_buffer_is_an_error() {
		let path = std::env::temp_dir().join("ferriscope-short.png");
		assert!(write_png(&path, 4, 4, &[0; 8]).is_err());
	}
}
