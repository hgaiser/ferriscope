//! GPU integration test for compositors that share one Vulkan instance.
//!
//! An embedder that runs compositors one after another in one process gives
//! them one `VulkanInstance`, so the Vulkan loader keeps the driver loaded.
//! Without it, NVIDIA's driver fails `vkCreateInstance` after about 26 reloads
//! (its static TLS is not given back on unload). The test is `#[ignore]`d: it
//! needs a render node. Run it with:
//!
//! ```text
//! cargo test -p ferriscope --test shared_instance -- --ignored --test-threads=1
//! ```

use std::time::Duration;

use ferriscope::{Compositor, CompositorConfig, VulkanInstance};

/// More runs than NVIDIA's driver survives with an instance per run.
const RUNS: usize = 40;
/// Handshake budget.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Compositors with Xwayland, started and stopped one after another on one
/// instance, all start, and each frees its Wayland socket.
#[test]
#[ignore = "requires a GPU render node and Xwayland"]
fn compositors_one_after_another_share_an_instance() {
	let instance = VulkanInstance::new().expect("vulkan instance");
	for run in 0..RUNS {
		let config = CompositorConfig {
			width: 320,
			height: 240,
			xwayland: true,
			vulkan_instance: Some(instance.clone()),
			..CompositorConfig::default()
		};
		let (compositor, handles) = Compositor::launch(config).unwrap_or_else(|e| panic!("run {run}: launch: {e}"));
		let ready = match handles.ready.recv_timeout(READY_TIMEOUT) {
			Ok(ready) => ready,
			Err(e) => panic!("run {run}: ready handshake: {e} ({:?})", compositor.stop()),
		};
		let socket = ready.runtime_dir.join(&ready.wayland_display);
		compositor.stop().unwrap_or_else(|e| panic!("run {run}: stop: {e}"));
		assert!(
			!socket.exists(),
			"run {run}: {} outlived the compositor",
			socket.display()
		);
	}
}
