//! CPU frame recorders for the standalone binary and the integration tests.
//!
//! Compiled only with the `test-support` feature. A recorder reads each
//! composited frame back to CPU pixels instead of handing out DMA-BUFs, so the
//! compositor is developable and testable with no consumer attached.

use std::{
	fs::{self, File},
	io::{BufWriter, Write},
	path::PathBuf,
};

use crate::{
	error::Result,
	frame::{Frame, FrameData, FrameInfo, PixelBuffer},
	sink::Sink,
};

/// Sink that discards every frame.
#[derive(Debug, Default)]
pub(crate) struct NullSink;

impl Sink for NullSink {
	fn submit(&mut self, _frame: Frame) -> Result<()> {
		Ok(())
	}
}

/// Writes each frame to disk as a binary PPM (`P6`, 8 bits per channel) under a
/// directory.
///
/// Files are named `frame_<sequence>.ppm`. When `max_frames` is
/// reached the recorder stops writing and accepts frames cheaply, which keeps a
/// long soak from filling the disk while still exercising the frame path.
pub(crate) struct FileRecorder {
	dir: PathBuf,
	max_frames: Option<u64>,
	frames_written: u64,
}

impl FileRecorder {
	/// Create a recorder writing into `dir`, which is created if missing.
	pub(crate) fn new(dir: impl Into<PathBuf>, max_frames: Option<u64>) -> Result<Self> {
		let dir = dir.into();
		fs::create_dir_all(&dir)?;
		Ok(Self {
			dir,
			max_frames,
			frames_written: 0,
		})
	}

	/// How many frames have been written so far.
	#[cfg(test)]
	pub(crate) fn frames_written(&self) -> u64 {
		self.frames_written
	}

	fn path_for(&self, sequence: u64) -> PathBuf {
		self.dir.join(format!("frame_{sequence:06}.ppm"))
	}

	fn write_pixels(&self, info: &FrameInfo, pixels: &PixelBuffer) -> Result<()> {
		let expected = pixels.stride.saturating_mul(info.height as usize);
		if pixels.data.len() < expected {
			return Err(crate::Error::Sink(format!(
				"pixel buffer too short: {} < {}",
				pixels.data.len(),
				expected
			)));
		}

		let file = File::create(self.path_for(info.sequence))?;
		let mut out = BufWriter::new(file);

		write!(out, "P6\n{} {}\n255\n", info.width, info.height)?;
		let bpp = pixels.format.bytes_per_pixel();
		let mut row = Vec::with_capacity(info.width as usize * 3);
		for y in 0..info.height as usize {
			row.clear();
			let start = y * pixels.stride;
			for x in 0..info.width as usize {
				let off = start + x * bpp;
				row.extend_from_slice(&pixels.format.to_rgb(&pixels.data[off..off + bpp]));
			}
			out.write_all(&row)?;
		}

		out.flush()?;
		Ok(())
	}
}

impl Sink for FileRecorder {
	fn submit(&mut self, frame: Frame) -> Result<()> {
		if let Some(max) = self.max_frames {
			if self.frames_written >= max {
				return Ok(());
			}
		}

		match frame.data {
			FrameData::Pixels(pixels) => {
				self.write_pixels(&frame.info, &pixels)?;
				self.frames_written += 1;
				Ok(())
			},
			FrameData::DmaBuf(_) => Err(crate::Error::Sink("file recorder only accepts CPU pixel frames".into())),
		}
	}
}

#[cfg(test)]
mod tests {
	use std::time::{Instant, SystemTime, UNIX_EPOCH};

	use super::*;
	use crate::colour_meta::ColourMetadata;
	use crate::frame::PixelFormat;

	fn temp_dir(tag: &str) -> PathBuf {
		let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
		let dir = std::env::temp_dir().join(format!("ferriscope-sink-{tag}-{}-{nanos}", std::process::id()));
		fs::create_dir_all(&dir).unwrap();
		dir
	}

	fn info(sequence: u64, width: u32, height: u32) -> FrameInfo {
		FrameInfo {
			sequence,
			captured_at: Instant::now(),
			width,
			height,
			colour: ColourMetadata::sdr(),
		}
	}

	#[test]
	fn writes_ppm_with_converted_pixels() {
		let dir = temp_dir("ppm");
		let mut recorder = FileRecorder::new(&dir, None).unwrap();

		// 2x2 BGRA: red, green, blue, white.
		let data = vec![
			0, 0, 255, 255, // red
			0, 255, 0, 255, // green
			255, 0, 0, 255, // blue
			255, 255, 255, 255, // white
		];
		let pixels = PixelBuffer {
			format: PixelFormat::Bgra8,
			stride: 8,
			data,
		};
		recorder
			.submit(Frame {
				info: info(0, 2, 2),
				data: FrameData::Pixels(pixels),
			})
			.unwrap();

		let written = fs::read(recorder.path_for(0)).unwrap();
		assert!(written.starts_with(b"P6\n2 2\n255\n"));
		let body = &written[written.len() - 12..];
		assert_eq!(body, &[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]);
		assert_eq!(recorder.frames_written(), 1);
		fs::remove_dir_all(&dir).ok();
	}

	#[test]
	fn stops_at_max_frames() {
		let dir = temp_dir("max");
		let mut recorder = FileRecorder::new(&dir, Some(2)).unwrap();
		for sequence in 0..5 {
			let pixels = PixelBuffer {
				format: PixelFormat::Bgra8,
				stride: 4,
				data: vec![0, 0, 0, 255],
			};
			recorder
				.submit(Frame {
					info: info(sequence, 1, 1),
					data: FrameData::Pixels(pixels),
				})
				.unwrap();
		}
		assert_eq!(recorder.frames_written(), 2);
		assert!(!recorder.path_for(2).exists());
		fs::remove_dir_all(&dir).ok();
	}

	#[test]
	fn rejects_short_pixel_buffer() {
		let dir = temp_dir("short");
		let mut recorder = FileRecorder::new(&dir, None).unwrap();
		let pixels = PixelBuffer {
			format: PixelFormat::Bgra8,
			stride: 4,
			data: vec![0, 0, 0],
		};
		let err = recorder
			.submit(Frame {
				info: info(0, 1, 1),
				data: FrameData::Pixels(pixels),
			})
			.unwrap_err();
		assert!(matches!(err, crate::Error::Sink(_)));
		fs::remove_dir_all(&dir).ok();
	}

	#[test]
	fn null_sink_accepts_frames() {
		let mut sink = NullSink;
		assert!(!sink.wants_dmabuf());
		sink.submit(Frame {
			info: info(0, 1, 1),
			data: FrameData::Pixels(PixelBuffer {
				format: PixelFormat::Bgra8,
				stride: 4,
				data: vec![0; 4],
			}),
		})
		.unwrap();
	}

	#[test]
	fn file_recorder_delivers_pixels() {
		let dir = temp_dir("delivered");
		let mut recorder = FileRecorder::new(&dir, None).unwrap();
		assert!(!recorder.wants_dmabuf());
		recorder
			.submit(Frame {
				info: info(0, 1, 1),
				data: FrameData::Pixels(PixelBuffer {
					format: PixelFormat::Rgba8,
					stride: 4,
					data: vec![0; 4],
				}),
			})
			.unwrap();
		fs::remove_dir_all(&dir).ok();
	}
}
