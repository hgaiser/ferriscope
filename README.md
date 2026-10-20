# ferriscope

A headless, single-application Wayland compositor whose output is an encoder.
It hosts one fullscreen application (a native or Proton game, or the Steam UI),
composites its output, and hands frames to a consumer, such as a video encoder,
as DMA-BUFs. It is a Rust port of
[gamescope](https://github.com/ValveSoftware/gamescope) on
[smithay](https://github.com/Smithay/smithay). It is a derivative work of
gamescope and is BSD-2-Clause licensed, as gamescope is.

There is no display, no modeset and no scanout, and no vblank. The session rate
(`CompositorConfig::rate`) is the only clock in the path.

## Crates

| Crate | Contents |
|---|---|
| `ferriscope` | The library (compositor, frame delivery, focus, Steam, Wayland and X11 handlers) and the thin `ferriscope` binary (command line and logging). Embedders use `default-features = false` to skip the binary's dependencies. |
| `ferriscope-wsi` | The Vulkan WSI layer `VK_LAYER_FERRISCOPE_wsi` (x86_64). |

## Features

- **Event-driven delivery.** A frame is composited when the scene changes, a
  pool slot is free and a delivery token is available. Commits and frame drops
  wake the loop directly.
- **Explicit sync and slot return.** `linux-drm-syncobj-v1` is advertised.
  Pooled DMA-BUF slots are returned when the consumer drops its frame, with a
  generation so a late return cannot free a reused slot.
- **Focus.** A stateless port of gamescope's focus pass over a per-window metadata
  cache. With `CompositorConfig::steam` Steam chooses the base layer
  (Steam-controlled focus).
- **Output fill.** Clients keep their resolution and are scaled, aspect preserved
  and centred, to fill the output.
- **HDR.** With `CompositorConfig::hdr`, `wp_color_management_v1` and
  `frog_color_management_v1` are advertised. The pool follows the focused content:
  10-bit PQ (`Abgr2101010`) or FP16 scRGB (`Abgr16161616f`). SDR cursor and
  overlays are converted in a GLES shader.
- **WSI.** `ferriscope_swapchain_v1` (native) and gamescope's
  `gamescope_xwayland` / `gamescope_swapchain_factory_v2` (compatibility) cover
  override surfaces, swapchain feedback, HDR metadata and present mode.
- **Presentation and FIFO.** `wp_presentation` (answered at hand-off) and `wp_fifo_v1`.
- **Input.** XKB keyboard, pointer, scroll, touch with gamescope's click modes,
  pen input (`zwp_tablet_v2`, a ferriscope extension: gamescope has no tablet
  support), the Steam on-screen keyboard (`gamescope_input_method`) and hotkeys
  (`gamescope_action_binding`). The cursor hides after `cursor_hide_time`.
- **Steam integration.** Off by default (`CompositorConfig::steam`). When on, the
  focused app is published to the X root for Steam, and the `GAMESCOPE_FPS_LIMIT`
  frame-callback throttle is honoured. Xwayland starts with
  `-force-xrandr-emulation`.

## Build and test

```sh
cargo build --workspace
cargo test --workspace                  # unit tests, no GPU needed
```

The integration suite needs a GPU, Xwayland and the DRM render node. It is
`#[ignore]`d, so run it explicitly:

```sh
cargo test -p ferriscope --no-fail-fast -- --ignored --test-threads=1
```

Minimum Rust version 1.87.

## Install the WSI layer

```sh
tools/install-wsi.sh --prefix /usr         # run as yourself; sudo is used only to install
```

## Library use

```rust
use ferriscope::{ChannelOutput, Compositor, CompositorConfig, FrameOutput};

let config = CompositorConfig {
    width: 1920,
    height: 1080,
    rate: 60.0,
    command: Some(vec!["vkcube".into()]),
    output: FrameOutput::Channel(ChannelOutput::default()),
    ..CompositorConfig::default()
};
let (compositor, handles) = Compositor::launch(config)?;
let ready = handles.ready.recv().expect("compositor ready");
// frames: handles.frames; input: handles.input.
```

`Compositor::launch` runs the compositor on its own thread and returns the
producer handle and the consumer endpoints (`CompositorHandles`).
`ferriscope::run(config)` is the standalone form. Every frame is composited
to RGB, and `handles.frames` keeps only the newest undelivered one.
The `test-support` feature (CPU PPM recorder, control socket) is enabled by the
binary and the tests; embedders leave it off.

## Running the binary

```sh
cargo run -p ferriscope -- --width 1280 --height 720 --xwayland -- vkcube
```

`--help` lists the flags (`--steam`, `--hdr`, `--record-dir`, ...).

## Copyright

Gamescope's copyright (Valve Corporation and contributors)
applies to the derived code.
