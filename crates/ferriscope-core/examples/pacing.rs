//! Pacing harness: commit-to-delivery latency and delivered fps against a real client.
//!
//! Launches the compositor in-process, runs `vkcube` (Wayland) or `glxgears`
//! (Xwayland, vsync off), and consumes DMA-BUF frames, holding each for
//! `hold_ms` before releasing it, like an encoder would. Commit-to-delivery
//! latency comes from the compositor's `ferriscope::cadence` trace events
//! (`commit_to_delivery_us`).
//!
//! ```text
//! cargo run --release -p ferriscope-core --example pacing -- vkcube 3 4 10
//! #                                                          app pool hold_ms seconds
//! ```
//!
//! `VKCUBE_ARGS` passes extra arguments to vkcube (`--present_mode 0` for an
//! unthrottled client). `MOTION_HZ` moves the pointer in a circle at that rate
//! while measuring, as a mouse does; `MOTION_SECS` stops it after that many
//! seconds, so the rest of the run shows how delivery recovers.
//!
//! `render_us` (CPU time of the composite, including the sink hand-off) comes
//! from the same trace, and the process CPU time covers the compositor and this
//! consumer.

use std::{
	process::{Command, Stdio},
	sync::{Arc, Mutex},
	time::{Duration, Instant},
};

use ferriscope_core::{Compositor, CompositorConfig, InputEvent, RecvTimeoutError};
use tracing::field::{Field, Visit};
use tracing_subscriber::{Layer, layer::SubscriberExt};

/// Collects `commit_to_delivery_us`, `render_us` and `composite_gpu_us` from
/// the compositor's cadence events.
struct LatencyLayer(Arc<Mutex<Vec<u64>>>, Arc<Mutex<Vec<u64>>>, Arc<Mutex<Vec<u64>>>);

struct LatencyVisitor(Option<u64>, Option<u64>, Option<u64>);

impl Visit for LatencyVisitor {
	fn record_u64(&mut self, field: &Field, value: u64) {
		match field.name() {
			"commit_to_delivery_us" => self.0 = Some(value),
			"render_us" => self.1 = Some(value),
			"composite_gpu_us" => self.2 = Some(value),
			_ => {},
		}
	}

	fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
}

impl<S: tracing::Subscriber> Layer<S> for LatencyLayer {
	fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
		let mut visitor = LatencyVisitor(None, None, None);
		event.record(&mut visitor);
		if let Some(us) = visitor.0 {
			self.0.lock().unwrap().push(us);
		}
		if let Some(us) = visitor.1 {
			self.1.lock().unwrap().push(us);
		}
		if let Some(us) = visitor.2 {
			self.2.lock().unwrap().push(us);
		}
	}
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
	if sorted.is_empty() {
		return 0;
	}
	sorted[(((sorted.len() as f64) * p).ceil() as usize)
		.saturating_sub(1)
		.min(sorted.len() - 1)]
}

/// Process CPU time (user + system) in seconds, from `/proc/self/stat`.
fn cpu_seconds() -> f64 {
	let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
	let after_comm = stat.rsplit(')').next().unwrap_or("");
	let fields: Vec<&str> = after_comm.split_whitespace().collect();
	// utime and stime are fields 14 and 15 of the stat line; `after_comm` starts at field 3.
	let ticks: u64 = [11, 12]
		.iter()
		.filter_map(|index| fields.get(*index).and_then(|field| field.parse::<u64>().ok()))
		.sum();
	ticks as f64 / 100.0
}

fn main() {
	let args: Vec<String> = std::env::args().collect();
	let app = args.get(1).map(String::as_str).unwrap_or("vkcube").to_string();
	let pool: usize = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(3);
	let hold_ms: u64 = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(4);
	let secs: u64 = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(10);

	let size_env = |name: &str, default: u32| {
		std::env::var(name)
			.ok()
			.and_then(|value| value.parse().ok())
			.unwrap_or(default)
	};
	let (width, height) = (size_env("WIDTH", 1280), size_env("HEIGHT", 720));

	let latencies = Arc::new(Mutex::new(Vec::new()));
	let renders = Arc::new(Mutex::new(Vec::new()));
	let gpu = Arc::new(Mutex::new(Vec::new()));
	let log = std::env::var_os("PACING_LOG").map(|_| {
		tracing_subscriber::fmt::layer().with_filter(
			tracing_subscriber::filter::Targets::new().with_target("ferriscope::cadence", tracing::Level::TRACE),
		)
	});
	let subscriber = tracing_subscriber::registry().with(log).with(
		LatencyLayer(latencies.clone(), renders.clone(), gpu.clone()).with_filter(
			tracing_subscriber::filter::Targets::new().with_target("ferriscope::cadence", tracing::Level::TRACE),
		),
	);
	tracing::subscriber::set_global_default(subscriber).expect("subscriber");

	let config = CompositorConfig {
		width,
		height,
		xwayland: app == "glxgears",
		command: None,
		output: ferriscope_core::FrameOutput::Channel(ferriscope_core::ChannelOutput { pool_size: pool }),
		..CompositorConfig::default()
	};
	let (compositor, handles) = Compositor::launch(config).expect("launch");
	let ready = handles.ready.recv_timeout(Duration::from_secs(15)).expect("ready");

	let mut command = if app == "glxgears" {
		let mut c = Command::new("glxgears");
		c.env("DISPLAY", format!(":{}", ready.xdisplay.expect("xdisplay")))
			.env("vblank_mode", "0");
		c
	} else {
		let mut c = Command::new("vkcube");
		c.args([
			"--wsi",
			"wayland",
			"--width",
			&width.to_string(),
			"--height",
			&height.to_string(),
		]);
		// Extra arguments, e.g. `--present_mode 0` for an unthrottled client.
		if let Ok(extra) = std::env::var("VKCUBE_ARGS") {
			c.args(extra.split_whitespace());
		}
		c
	};
	command
		.env("WAYLAND_DISPLAY", &ready.wayland_display)
		.envs(ready.env.iter().cloned())
		.stdout(Stdio::null())
		.stderr(Stdio::null());
	let mut child = command.spawn().expect("spawn app");

	// Warm-up: wait for the first frame, then measure for `secs`.
	let _ = handles
		.frames
		.recv_timeout(Duration::from_secs(15))
		.expect("first frame");
	std::thread::sleep(Duration::from_millis(500));
	latencies.lock().unwrap().clear();
	renders.lock().unwrap().clear();
	gpu.lock().unwrap().clear();
	let cpu_start = cpu_seconds();

	let stop_motion = Arc::new(std::sync::atomic::AtomicBool::new(false));
	let motion = std::env::var("MOTION_HZ")
		.ok()
		.and_then(|hz| hz.parse::<f64>().ok())
		.map(|hz| {
			let input = handles.input;
			let stop = stop_motion.clone();
			let (w, h) = (f64::from(width), f64::from(height));
			let until = std::env::var("MOTION_SECS")
				.ok()
				.and_then(|secs| secs.parse::<f64>().ok())
				.map(|secs| Instant::now() + Duration::from_secs_f64(secs));
			std::thread::spawn(move || {
				let period = Duration::from_secs_f64(1.0 / hz);
				let mut angle = 0.0f64;
				while !stop.load(std::sync::atomic::Ordering::Relaxed)
					&& until.is_none_or(|until| Instant::now() < until)
				{
					angle += 0.01;
					let (x, y) = (w / 2.0 + w / 4.0 * angle.cos(), h / 2.0 + h / 4.0 * angle.sin());
					if input.send(InputEvent::PointerMotion { x, y }).is_err() {
						break;
					}
					std::thread::sleep(period);
				}
			})
		});

	let start = Instant::now();
	let mut arrivals: Vec<Instant> = Vec::new();
	let hold = Duration::from_millis(hold_ms);
	while start.elapsed() < Duration::from_secs(secs) {
		match handles.frames.recv_timeout(Duration::from_millis(200)) {
			Ok(frame) => {
				arrivals.push(Instant::now());
				std::thread::sleep(hold);
				// Dropping the frame returns its pool slot.
				drop(frame);
			},
			Err(RecvTimeoutError::Timeout) => {},
			Err(RecvTimeoutError::Disconnected) => break,
		}
	}
	let elapsed = start.elapsed().as_secs_f64();
	let cpu = cpu_seconds() - cpu_start;
	stop_motion.store(true, std::sync::atomic::Ordering::Relaxed);
	if let Some(motion) = motion {
		let _ = motion.join();
	}
	let _ = child.kill();
	let _ = child.wait();
	let _ = compositor.stop();

	let mut gaps: Vec<u64> = arrivals
		.windows(2)
		.map(|w| w[1].duration_since(w[0]).as_micros() as u64)
		.collect();
	gaps.sort_unstable();
	let mut lat = latencies.lock().unwrap().clone();
	lat.sort_unstable();
	let mut render = renders.lock().unwrap().clone();
	render.sort_unstable();
	let mut gpu = gpu.lock().unwrap().clone();
	gpu.sort_unstable();
	println!(
		"composite_gpu_us[n={} p50={} p95={}] cpu_pct={:.1} render_us[p50={} p95={} max={}]",
		gpu.len(),
		percentile(&gpu, 0.5),
		percentile(&gpu, 0.95),
		cpu / elapsed * 100.0,
		percentile(&render, 0.5),
		percentile(&render, 0.95),
		render.last().copied().unwrap_or(0),
	);
	println!(
		"app={app} pool={pool} hold_ms={hold_ms} fps={:.1} frames={} gap_us[p50={} p95={} max={}] commit_to_delivery_us[n={} p50={} p95={} p99={} max={}]",
		arrivals.len() as f64 / elapsed,
		arrivals.len(),
		percentile(&gaps, 0.5),
		percentile(&gaps, 0.95),
		gaps.last().copied().unwrap_or(0),
		lat.len(),
		percentile(&lat, 0.5),
		percentile(&lat, 0.95),
		percentile(&lat, 0.99),
		lat.last().copied().unwrap_or(0),
	);
}
