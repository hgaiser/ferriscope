//! GPU integration test for `InputEvent::Text`: a clipboard paste typed one
//! character per event, as Moonlight sends it.
//!
//! A few thousand characters (ASCII, shifted punctuation and characters no
//! layout key produces) are sent as one `Text` event each, back to back. The
//! test asserts every character reaches the focused client, in order, and that
//! the compositor keeps delivering frames while it types (the pointer is moved
//! throughout so there is damage to compose). The measured paste duration and
//! worst frame gap are printed.
//!
//! It is `#[ignore]`d: it needs a render node (and Xwayland and `xev` for the
//! X11 variant). Run it alone:
//!
//! ```text
//! timeout 300 cargo test -p ferriscope-core --test text_paste -- --ignored --test-threads=1 --nocapture
//! ```

use std::{
	io::{BufRead, BufReader},
	process::{Command, Stdio},
	sync::{
		Arc, Mutex,
		atomic::{AtomicBool, Ordering},
	},
	thread,
	time::{Duration, Instant},
};

use ferriscope_core::{Compositor, CompositorConfig, FrameReceiver, InputEvent, InputSender, Ready};

use smithay::input::keyboard::xkb::{
	self, CONTEXT_NO_FLAGS, KEYMAP_COMPILE_NO_FLAGS, KEYMAP_FORMAT_TEXT_V1, KeyDirection, Keycode,
};
use support::{KeyboardEvent, Started, Target};

mod support;

/// Budget for the window to map and receive keyboard focus.
const FOCUS_TIMEOUT: Duration = Duration::from_secs(15);
/// Budget for the whole paste to surface in the client.
const PASTE_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest acceptable pause between frames while typing.
const MAX_FRAME_GAP: Duration = Duration::from_millis(250);
/// Pause between pointer moves, which supply the damage that makes frames.
const MOTION_PERIOD: Duration = Duration::from_millis(8);

/// A paste of roughly `length` characters.
///
/// Each round starts with a counter so a reordered or dropped character shows,
/// and mixes plain letters, shifted punctuation and characters that are on no
/// layout key.
fn paste_text(length: usize) -> String {
	const ROUND: &str =
		"The Quick Brown fox: 'jumps' over (the) lazy dog! #@$%^&*_+{}|<>? \"q\" ~` [x] \\ / é ß € 日 ü ñ 한 ";
	let mut text = String::new();
	let mut round = 0;
	while text.chars().count() < length {
		text.push_str(&format!("{round:04} {ROUND}"));
		round += 1;
	}
	text
}

/// The frame arrival times, collected on a thread until stopped.
struct FrameTimes {
	stop: Arc<AtomicBool>,
	times: Arc<Mutex<Vec<Instant>>>,
	thread: Option<thread::JoinHandle<()>>,
}

impl FrameTimes {
	fn start(frames: FrameReceiver) -> Self {
		let stop = Arc::new(AtomicBool::new(false));
		let times = Arc::new(Mutex::new(Vec::new()));
		let (thread_stop, thread_times) = (Arc::clone(&stop), Arc::clone(&times));
		let thread = thread::spawn(move || {
			while !thread_stop.load(Ordering::Acquire) {
				// Dropping a frame returns its buffer to the compositor.
				if frames.recv_timeout(Duration::from_millis(50)).is_ok() {
					thread_times.lock().expect("times lock").push(Instant::now());
				}
			}
		});
		Self {
			stop,
			times,
			thread: Some(thread),
		}
	}

	/// The frame times so far.
	fn times(&self) -> Vec<Instant> {
		self.times.lock().expect("times lock").clone()
	}

	fn finish(&mut self) {
		self.stop.store(true, Ordering::Release);
		if let Some(thread) = self.thread.take() {
			let _ = thread.join();
		}
	}
}

/// Moves the pointer back and forth until stopped.
struct Mover {
	stop: Arc<AtomicBool>,
	thread: Option<thread::JoinHandle<()>>,
}

impl Mover {
	fn start(input: InputSender) -> Self {
		let stop = Arc::new(AtomicBool::new(false));
		let thread_stop = Arc::clone(&stop);
		let thread = thread::spawn(move || {
			let mut x = 20.0;
			while !thread_stop.load(Ordering::Acquire) {
				x = if x > 200.0 { 20.0 } else { x + 7.0 };
				if input.send(InputEvent::PointerMotion { x, y: 100.0 }).is_err() {
					break;
				}
				thread::sleep(MOTION_PERIOD);
			}
		});
		Self {
			stop,
			thread: Some(thread),
		}
	}

	fn finish(&mut self) {
		self.stop.store(true, Ordering::Release);
		if let Some(thread) = self.thread.take() {
			let _ = thread.join();
		}
	}
}

/// The longest gap between frames over `[from, to]`, counting the edges.
fn worst_gap(times: &[Instant], from: Instant, to: Instant) -> Duration {
	let mut previous = from;
	let mut worst = Duration::ZERO;
	for time in times.iter().filter(|time| **time > from && **time <= to) {
		worst = worst.max(time.duration_since(previous));
		previous = *time;
	}
	worst.max(to.duration_since(previous))
}

/// Send `text` one character per event, `pace` apart.
fn paste(input: &InputSender, text: &str, pace: Duration) {
	for ch in text.chars() {
		input.send(InputEvent::Text(ch.to_string())).expect("send text");
		if !pace.is_zero() {
			thread::sleep(pace);
		}
	}
}

/// The text a Wayland client types from its keyboard events: each key press is
/// run through the keymap that was current, with the modifier state the earlier
/// events built up.
fn typed_text(log: &support::Log) -> String {
	let context = xkb::Context::new(CONTEXT_NO_FLAGS);
	let compile = |text: &String| {
		xkb::Keymap::new_from_string(&context, text.clone(), KEYMAP_FORMAT_TEXT_V1, KEYMAP_COMPILE_NO_FLAGS)
			.expect("the client's keymap compiles")
	};
	let mut state: Option<xkb::State> = None;
	let mut typed = String::new();
	for event in &log.keyboard_events {
		match event {
			KeyboardEvent::Keymap(index) => state = Some(xkb::State::new(&compile(&log.keymaps[*index]))),
			KeyboardEvent::Key { evdev, pressed } => {
				let state = state.as_mut().expect("a keymap before the first key");
				let keycode = Keycode::new(evdev + 8);
				if *pressed {
					typed.push_str(&state.key_get_utf8(keycode));
					state.update_key(keycode, KeyDirection::Down);
				} else {
					state.update_key(keycode, KeyDirection::Up);
				}
			},
		}
	}
	typed
}

fn first_difference(typed: &str, expected: &str) -> String {
	let index = typed
		.chars()
		.zip(expected.chars())
		.position(|(a, b)| a != b)
		.unwrap_or_else(|| typed.chars().count().min(expected.chars().count()));
	let context = |text: &str| text.chars().skip(index.saturating_sub(10)).take(30).collect::<String>();
	format!(
		"first difference at char {index}: typed {:?} vs expected {:?} ({} vs {} chars)",
		context(typed),
		context(expected),
		typed.chars().count(),
		expected.chars().count()
	)
}

fn report(what: &str, count: usize, duration: Duration, gap: Duration, frames: usize) {
	println!("{what}: {count} chars in {duration:?}, worst frame gap {gap:?} ({frames} frames)");
}

/// Types `text` into the native Wayland client and checks what it receives.
fn wayland_paste(text: &str, pace: Duration) {
	let mut started: Started = support::start_configured(support::ClientOptions::default(), |_| {});
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");
	assert!(
		support::wait_log(&started.client, FOCUS_TIMEOUT, |log| log
			.keyboard_enter
			.contains(&parent)),
		"the window did not receive keyboard focus"
	);
	let input = started._handles.input.clone();
	// A cursor image makes pointer motion damage the frame, so there are
	// frames to time.
	input
		.send(InputEvent::PointerMotion { x: 100.0, y: 100.0 })
		.expect("send motion");
	assert!(
		support::wait_log(&started.client, FOCUS_TIMEOUT, |log| log
			.pointer_enter
			.contains(&parent)),
		"the pointer did not enter the window"
	);
	assert!(started.client.command(support::CommandKind::SetCursor), "set cursor");
	let mut frames = FrameTimes::start(started._handles.frames);
	let mut mover = Mover::start(input.clone());
	// Frames while idle, before the paste.
	thread::sleep(Duration::from_millis(500));

	let count = text.chars().count();
	let begin = Instant::now();
	paste(&input, text, pace);
	// The window's key events are counted as presses.
	let arrived = support::wait_log(&started.client, PASTE_TIMEOUT, |log| {
		let presses = log
			.keyboard_events
			.iter()
			.filter(|event| matches!(event, KeyboardEvent::Key { pressed: true, .. }))
			.count();
		// Upper-case and shifted characters add a Shift press of their own, so
		// compare the decoded text once enough presses are in.
		presses >= count && typed_text(log).chars().count() >= count
	});
	let end = Instant::now();
	// Let the last key release and any keymap reset land.
	thread::sleep(Duration::from_millis(400));
	mover.finish();
	frames.finish();

	let log = started.client.snapshot();
	let typed = typed_text(&log);
	let times = frames.times();
	let gap = worst_gap(&times, begin, end);
	report("wayland paste", count, end - begin, gap, times.len());
	eprintln!("wayland paste: {} keymaps sent", log.keymaps.len());
	assert!(arrived, "the paste did not arrive: {}", first_difference(&typed, text));
	assert_eq!(typed, text, "{}", first_difference(&typed, text));
	assert!(
		gap <= MAX_FRAME_GAP,
		"frames stalled for {gap:?} during the paste (limit {MAX_FRAME_GAP:?})"
	);
	// Everything idle again: the original keymap is back for the next key.
	assert_eq!(
		log.keymaps.last(),
		log.keymaps.first(),
		"the seat keymap was not restored after the paste"
	);

	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

#[test]
#[ignore = "requires GPU; native Wayland client"]
fn pasted_text_arrives_in_order_and_frames_keep_flowing() {
	wayland_paste(&paste_text(3000), Duration::ZERO);
}

#[test]
#[ignore = "requires GPU; native Wayland client"]
fn paced_paste_arrives_in_order() {
	// About what a network delivers: a character every couple of milliseconds.
	wayland_paste(&paste_text(600), Duration::from_millis(2));
}

/// Text is gathered for a few milliseconds before it is typed; a key sent after
/// it must still come after it.
#[test]
#[ignore = "requires GPU; native Wayland client"]
fn a_key_after_text_arrives_after_it() {
	const KEY_A: u32 = 30;
	const KEY_B: u32 = 48;
	const KEY_ENTER: u32 = 28;
	let mut started: Started = support::start_configured(support::ClientOptions::default(), |_| {});
	let parent = started.client.surface_id(Target::Parent).expect("parent surface id");
	assert!(
		support::wait_log(&started.client, FOCUS_TIMEOUT, |log| log
			.keyboard_enter
			.contains(&parent)),
		"the window did not receive keyboard focus"
	);
	let input = &started._handles.input;
	input.send(InputEvent::Text("a".into())).expect("send text");
	input.send(InputEvent::Text("b".into())).expect("send text");
	input
		.send(InputEvent::Key {
			code: KEY_ENTER,
			pressed: true,
		})
		.expect("send key");
	input
		.send(InputEvent::Key {
			code: KEY_ENTER,
			pressed: false,
		})
		.expect("send key");
	let expected = [
		(KEY_A, true),
		(KEY_A, false),
		(KEY_B, true),
		(KEY_B, false),
		(KEY_ENTER, true),
		(KEY_ENTER, false),
	];
	assert!(
		support::wait_log(&started.client, Duration::from_secs(10), |log| log.keys == expected),
		"keys were {:?}",
		started.client.snapshot().keys
	);
	started.client.shutdown();
	assert!(started.guard.stop(), "compositor stopped cleanly");
}

/// A paste into an X11 client under Xwayland, which only learns a keymap change
/// after Xwayland has run `xkbcomp` for it: `xev` decodes keys it has not read
/// yet with the keymap it has by then, so a burst that changes the keymap along
/// the way is not reliable to check. This one has to stay on the seat keymap
/// (no keymap change at all) for a long, fast paste.
fn x11_paste(text: &str, pace: Duration) {
	let config = CompositorConfig {
		width: 320,
		height: 240,
		xwayland: true,
		..CompositorConfig::default()
	};
	let (compositor, handles) = Compositor::launch(config).expect("launch compositor");
	let ready: Ready = handles
		.ready
		.recv_timeout(Duration::from_secs(20))
		.expect("ready handshake");
	let xdisplay = ready.xdisplay.expect("xwayland reports an X display");

	let mut xev = Command::new("xev")
		.env("DISPLAY", format!(":{xdisplay}"))
		.stdout(Stdio::piped())
		.stderr(Stdio::null())
		.spawn()
		.expect("spawn xev");
	let stdout = xev.stdout.take().expect("xev stdout");
	let started_window = Arc::new(AtomicBool::new(false));
	let keysyms = Arc::new(Mutex::new(Vec::<u32>::new()));
	let (reader_window, reader_keysyms) = (Arc::clone(&started_window), Arc::clone(&keysyms));
	let reader = thread::spawn(move || {
		let mut parser = KeysymParser::default();
		for line in BufReader::new(stdout).lines().map_while(Result::ok) {
			if line.contains("Outer window is") {
				reader_window.store(true, Ordering::Release);
			}
			if let Some(keysym) = parser.line(&line) {
				reader_keysyms.lock().expect("keysyms lock").push(keysym);
			}
		}
	});

	let count = text.chars().count();
	let expected: Vec<u32> = text
		.chars()
		.map(|ch| xkb::utf32_to_keysym(u32::from(ch)).raw())
		.collect();
	let mut frames = FrameTimes::start(handles.frames);
	let mut mover = None;
	let mut timing = None;
	let result = (|| {
		let deadline = Instant::now() + Duration::from_secs(20);
		while !started_window.load(Ordering::Acquire) {
			if Instant::now() > deadline {
				return Err("xev did not start".to_string());
			}
			thread::sleep(Duration::from_millis(50));
		}
		mover = Some(Mover::start(handles.input.clone()));
		thread::sleep(Duration::from_millis(500));
		let begin = Instant::now();
		paste(&handles.input, text, pace);
		let deadline = Instant::now() + PASTE_TIMEOUT;
		let typed = loop {
			let typed = keysyms.lock().expect("keysyms lock").clone();
			if typed.len() >= count || Instant::now() > deadline {
				break typed;
			}
			thread::sleep(Duration::from_millis(25));
		};
		timing = Some((begin, Instant::now()));
		if typed != expected {
			let index = typed
				.iter()
				.zip(&expected)
				.position(|(a, b)| a != b)
				.unwrap_or(typed.len());
			return Err(format!(
				"X11 client got {} of {count} keysyms; first difference at {index}: got {:#x?}, expected {:#x?}",
				typed.len(),
				typed.get(index),
				expected.get(index)
			));
		}
		Ok(())
	})();
	if let Some(mover) = mover.as_mut() {
		mover.finish();
	}
	frames.finish();
	let _ = xev.kill();
	let _ = xev.wait();
	let _ = reader.join();
	if let Some((begin, end)) = timing {
		let times = frames.times();
		let gap = worst_gap(&times, begin, end);
		report("x11 paste", count, end - begin, gap, times.len());
		if result.is_ok() {
			assert!(
				gap <= MAX_FRAME_GAP,
				"frames stalled for {gap:?} during the paste (limit {MAX_FRAME_GAP:?})"
			);
		}
	}
	let _ = compositor.stop();
	if let Err(message) = result {
		panic!("{message}");
	}
}

#[test]
#[ignore = "requires GPU, Xwayland and xev"]
fn a_fast_layout_paste_reaches_an_x11_client() {
	// Letters, digits and shifted punctuation: plain keys of the seat keymap.
	let text: String = paste_text(1500)
		.chars()
		.filter(|ch| ch.is_ascii() && !ch.is_ascii_control())
		.collect();
	x11_paste(&text, Duration::ZERO);
}

#[test]
#[ignore = "requires GPU, Xwayland and xev"]
fn a_paste_with_other_characters_reaches_an_x11_client() {
	// Characters no layout key produces need the temporary keymap; paced like a
	// network so xev keeps up with each keymap change.
	x11_paste("Héllo wörld, ça coûte 5 € -- 日本語 ß ok", Duration::from_millis(40));
}

/// Picks the keysyms of `KeyPress` events (modifiers left out) out of xev's
/// output, line by line.
#[derive(Default)]
struct KeysymParser {
	in_press: bool,
}

impl KeysymParser {
	fn line(&mut self, line: &str) -> Option<u32> {
		if line.starts_with("KeyPress") {
			self.in_press = true;
		} else if line.starts_with(|ch: char| ch.is_ascii_alphabetic()) {
			self.in_press = false;
		} else if self.in_press {
			let rest = line.split("(keysym 0x").nth(1)?;
			self.in_press = false;
			let (hex, name) = rest.split_once(", ").unwrap_or((rest, ""));
			if !name.starts_with("Shift") {
				return u32::from_str_radix(hex, 16).ok();
			}
		}
		None
	}
}
