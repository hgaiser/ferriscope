//! A text control socket for the standalone binary and the integration tests.
//!
//! Compiled only with the `test-support` feature. The in-process embedder uses
//! [`crate::InputSender`] instead; this is the line-oriented protocol the
//! `--control-socket` option and the tests drive a headless compositor with.
//!
//! # Wire protocol
//!
//! Line-oriented, UTF-8, whitespace-separated. One event per line:
//!
//! ```text
//! key <evdev-code> <0|1> KEY_* press (1) / release (0)
//! button <evdev-code> <0|1> BTN_* press (1) / release (0)
//! motion-abs <x> <y> absolute pointer position, logical output coords
//! motion-rel <dx> <dy> relative pointer delta (mouselook)
//! axis <v|h> <value> scroll wheel notches: vertical (v) or horizontal (h)
//! touch-down <slot> <x> <y> a touch point appeared, logical output coords
//! touch-motion <slot> <x> <y> a touch point moved, logical output coords
//! touch-up <slot> a touch point lifted
//! touch-cancel the touch session was cancelled
//! touch-frame end of a group of touch events
//! text <text> type the rest of the line (escapes: \\ \n \t \s for a space)
//! pen <pen|eraser> <hover|contact> <x> <y> [pressure=<v>] [distance=<v>] [tilt=<x>,<y>] [buttons=<mask>]
//! a pen sample, logical output coords
//! pen-buttons <mask> the pen's stylus buttons changed
//! pen-leave the pen left proximity
//! ```
//!
//! Blank lines and lines whose first non-space character is `#` are ignored.
//! Commands are lower-case and exact, and argument counts are strict. Numbers
//! are validated: evdev codes are `u32`, press flags are exactly `0` or `1`, and
//! coordinates are finite `f64`. A malformed line yields
//! [`crate::Error::Input`] naming the offending token.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read};
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::{
	error::{Error, Result},
	seat::{InputEvent, PenEvent, PenTool},
};

/// A source of control-stream input events.
pub trait InputSource {
	/// Decode all events currently available, returning an empty vector when the
	/// source would block.
	fn poll(&mut self) -> Result<Vec<InputEvent>>;
}

/// Decode events from a line-oriented text stream.
///
/// This is the protocol codec only: it performs no I/O and holds no state, so
/// it can be unit-tested in isolation. [`UnixControlStream`] pairs it with a
/// socket and line buffering.
pub struct TextControlStream;

impl TextControlStream {
	/// Parse one line into an event, or `None` for blank/comment lines.
	///
	/// The line may or may not include its trailing newline; surrounding
	/// whitespace is ignored. Returns [`crate::Error::Input`] with the offending
	/// token on any malformed input.
	pub fn parse_line(line: &str) -> Result<Option<InputEvent>> {
		let line = line.trim();
		if line.is_empty() || line.starts_with('#') {
			return Ok(None);
		}

		if let Some(text) = line.strip_prefix("text ") {
			return Ok(Some(InputEvent::Text(unescape_text(text)?)));
		}

		let mut tokens = line.split_whitespace();
		let Some(command) = tokens.next() else {
			return Ok(None);
		};
		let args: Vec<&str> = tokens.collect();

		let event = match command {
			"key" => {
				let (code, pressed) = parse_code_press(command, &args)?;
				InputEvent::Key { code, pressed }
			},
			"button" => {
				let (code, pressed) = parse_code_press(command, &args)?;
				InputEvent::PointerButton { code, pressed }
			},
			"motion-abs" => {
				let (x, y) = parse_pair(command, &args)?;
				InputEvent::PointerMotion { x, y }
			},
			"motion-rel" => {
				let (dx, dy) = parse_pair(command, &args)?;
				InputEvent::PointerMotionDelta { dx, dy }
			},
			"axis" => {
				let (horizontal, value) = parse_axis(command, &args)?;
				InputEvent::PointerAxis { horizontal, value }
			},
			"touch-down" => {
				let (slot, x, y) = parse_touch_point(command, &args)?;
				InputEvent::TouchDown { slot, x, y }
			},
			"touch-motion" => {
				let (slot, x, y) = parse_touch_point(command, &args)?;
				InputEvent::TouchMotion { slot, x, y }
			},
			"touch-up" => {
				expect_args(command, &args, 1)?;
				InputEvent::TouchUp {
					slot: parse_slot(command, args[0])?,
				}
			},
			"touch-cancel" => {
				expect_args(command, &args, 0)?;
				InputEvent::TouchCancel
			},
			"touch-frame" => {
				expect_args(command, &args, 0)?;
				InputEvent::TouchFrame
			},
			"pen" => InputEvent::Pen(parse_pen(command, &args)?),
			"pen-buttons" => {
				expect_args(command, &args, 1)?;
				InputEvent::PenButtons {
					buttons: parse_u32(command, args[0])?,
				}
			},
			"pen-leave" => {
				expect_args(command, &args, 0)?;
				InputEvent::PenLeave
			},
			other => return Err(Error::Input(format!("unknown command {other:?}"))),
		};

		Ok(Some(event))
	}
}

/// A control-stream source backed by a non-blocking Unix socket.
///
/// [`poll`](InputSource::poll) never blocks: it drains whatever the socket has,
/// buffers any trailing partial line, and returns the events from all complete
/// lines. If the peer has closed the socket, the next poll that finds no events
/// reports [`Error::Input`]`("control stream closed")`.
#[derive(Debug)]
pub struct UnixControlStream {
	stream: UnixStream,
	buffer: Vec<u8>,
}

impl UnixControlStream {
	/// Wrap a socket, switching it to non-blocking mode.
	pub fn new(stream: UnixStream) -> Result<Self> {
		stream.set_nonblocking(true)?;
		Ok(Self {
			stream,
			buffer: Vec::new(),
		})
	}

	/// Connect to a control-stream socket and wrap it non-blocking.
	pub fn connect(path: impl AsRef<Path>) -> Result<Self> {
		Self::new(UnixStream::connect(path)?)
	}

	/// The underlying socket, for registering with an event loop.
	pub fn stream(&self) -> &UnixStream {
		&self.stream
	}
}

impl InputSource for UnixControlStream {
	fn poll(&mut self) -> Result<Vec<InputEvent>> {
		let mut chunk = [0u8; 4096];
		let mut eof = false;
		loop {
			match self.stream.read(&mut chunk) {
				Ok(0) => {
					eof = true;
					break;
				},
				Ok(n) => self.buffer.extend_from_slice(&chunk[..n]),
				Err(e) if e.kind() == ErrorKind::WouldBlock => break,
				Err(e) if e.kind() == ErrorKind::Interrupted => continue,
				Err(e) => return Err(Error::Io(e)),
			}
		}

		let mut events = Vec::new();
		while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
			let mut line: Vec<u8> = self.buffer.drain(..=pos).collect();
			if line.last() == Some(&b'\n') {
				line.pop();
			}
			if line.last() == Some(&b'\r') {
				line.pop();
			}
			let text = std::str::from_utf8(&line)
				.map_err(|e| Error::Input(format!("control stream is not valid UTF-8: {e}")))?;
			if let Some(event) = TextControlStream::parse_line(text)? {
				events.push(event);
			}
		}

		// Preserve events decoded before EOF; a later poll reports the close.
		if eof && events.is_empty() {
			return Err(Error::Input("control stream closed".to_string()));
		}
		Ok(events)
	}
}

/// An in-memory [`InputSource`]: `poll` drains the queue, never blocks.
impl InputSource for VecDeque<InputEvent> {
	fn poll(&mut self) -> Result<Vec<InputEvent>> {
		Ok(self.drain(..).collect())
	}
}

/// Parse `<evdev-code> <0|1>` into a code/pressed pair.
fn parse_code_press(command: &str, args: &[&str]) -> Result<(u32, bool)> {
	expect_args(command, args, 2)?;
	let code = parse_u32(command, args[0])?;
	let pressed = parse_press(command, args[1])?;
	Ok((code, pressed))
}

/// Parse two finite floats.
fn parse_pair(command: &str, args: &[&str]) -> Result<(f64, f64)> {
	expect_args(command, args, 2)?;
	Ok((parse_f64(command, args[0])?, parse_f64(command, args[1])?))
}

/// Parse `<slot> <x> <y>` into a touch point.
fn parse_touch_point(command: &str, args: &[&str]) -> Result<(u32, f64, f64)> {
	expect_args(command, args, 3)?;
	Ok((
		parse_slot(command, args[0])?,
		parse_f64(command, args[1])?,
		parse_f64(command, args[2])?,
	))
}

/// Decode the argument of `text`: the rest of the line, with `\\`, `\n`,
/// `\t` and `\s` (a space, which a line's edges would otherwise lose)
/// standing for a backslash, newline, tab and space.
fn unescape_text(text: &str) -> Result<String> {
	let mut out = String::with_capacity(text.len());
	let mut chars = text.chars();
	while let Some(ch) = chars.next() {
		if ch != '\\' {
			out.push(ch);
			continue;
		}
		out.push(match chars.next() {
			Some('\\') => '\\',
			Some('n') => '\n',
			Some('t') => '\t',
			Some('s') => ' ',
			other => return Err(Error::Input(format!("text: bad escape \\{}", other.unwrap_or(' ')))),
		});
	}
	Ok(out)
}

/// Parse `<tool> <hover|contact> <x> <y>` and the optional `key=value` axes.
fn parse_pen(command: &str, args: &[&str]) -> Result<PenEvent> {
	if args.len() < 4 {
		return Err(Error::Input(format!(
			"{command}: expected at least 4 arguments, got {}",
			args.len()
		)));
	}
	let tool = match args[0] {
		"pen" => PenTool::Pen,
		"eraser" => PenTool::Eraser,
		other => {
			return Err(Error::Input(format!(
				"{command}: expected pen or eraser, got {other:?}"
			)));
		},
	};
	let contact = match args[1] {
		"contact" => true,
		"hover" => false,
		other => {
			return Err(Error::Input(format!(
				"{command}: expected hover or contact, got {other:?}"
			)));
		},
	};
	let mut event = PenEvent {
		tool,
		x: parse_f64(command, args[2])?,
		y: parse_f64(command, args[3])?,
		contact,
		pressure: None,
		distance: None,
		tilt: None,
		buttons: 0,
	};
	for option in &args[4..] {
		let Some((key, value)) = option.split_once('=') else {
			return Err(Error::Input(format!("{command}: expected key=value, got {option:?}")));
		};
		match key {
			"pressure" => event.pressure = Some(parse_f64(command, value)?),
			"distance" => event.distance = Some(parse_f64(command, value)?),
			"buttons" => event.buttons = parse_u32(command, value)?,
			"tilt" => {
				let Some((x, y)) = value.split_once(',') else {
					return Err(Error::Input(format!("{command}: expected tilt=<x>,<y>, got {value:?}")));
				};
				event.tilt = Some((parse_f64(command, x)?, parse_f64(command, y)?));
			},
			other => return Err(Error::Input(format!("{command}: unknown option {other:?}"))),
		}
	}
	Ok(event)
}

/// Parse a touch slot id, naming the offending token.
fn parse_slot(command: &str, token: &str) -> Result<u32> {
	token
		.parse::<u32>()
		.map_err(|_| Error::Input(format!("{command}: invalid touch slot {token:?}")))
}

/// Parse `<v|h> <value>` into an axis selector and a finite notch count.
fn parse_axis(command: &str, args: &[&str]) -> Result<(bool, f64)> {
	expect_args(command, args, 2)?;
	let horizontal = match args[0] {
		"h" => true,
		"v" => false,
		other => return Err(Error::Input(format!("{command}: expected v or h, got {other:?}"))),
	};
	Ok((horizontal, parse_f64(command, args[1])?))
}

/// Enforce a strict argument count, naming the command on mismatch.
fn expect_args(command: &str, args: &[&str], expected: usize) -> Result<()> {
	if args.len() == expected {
		Ok(())
	} else {
		Err(Error::Input(format!(
			"{command}: expected {expected} argument(s), got {}",
			args.len()
		)))
	}
}

/// Parse an evdev code, naming the offending token.
fn parse_u32(command: &str, token: &str) -> Result<u32> {
	token
		.parse::<u32>()
		.map_err(|_| Error::Input(format!("{command}: invalid evdev code {token:?}")))
}

/// Parse a press flag; only exactly `0` or `1` is accepted.
fn parse_press(command: &str, token: &str) -> Result<bool> {
	match token {
		"0" => Ok(false),
		"1" => Ok(true),
		_ => Err(Error::Input(format!("{command}: expected 0 or 1, got {token:?}"))),
	}
}

/// Parse a finite float, naming the offending token.
fn parse_f64(command: &str, token: &str) -> Result<f64> {
	let value = token
		.parse::<f64>()
		.map_err(|_| Error::Input(format!("{command}: invalid number {token:?}")))?;
	if value.is_finite() {
		Ok(value)
	} else {
		Err(Error::Input(format!("{command}: number must be finite, got {token:?}")))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::Write;

	/// Parse a line that must yield exactly one event.
	fn parse(line: &str) -> InputEvent {
		TextControlStream::parse_line(line)
			.expect("valid line")
			.expect("non-empty line")
	}

	/// Parse a line that must fail, returning the `Input` message.
	fn parse_err(line: &str) -> String {
		match TextControlStream::parse_line(line) {
			Err(Error::Input(msg)) => msg,
			other => panic!("expected Error::Input, got {other:?}"),
		}
	}

	#[test]
	fn parses_touch_events() {
		assert_eq!(
			parse("touch-down 2 10.5 20"),
			InputEvent::TouchDown {
				slot: 2,
				x: 10.5,
				y: 20.0
			}
		);
		assert_eq!(
			parse("touch-motion 0 -1 3.25"),
			InputEvent::TouchMotion {
				slot: 0,
				x: -1.0,
				y: 3.25
			}
		);
		assert_eq!(parse("touch-up 7"), InputEvent::TouchUp { slot: 7 });
		assert_eq!(parse("touch-cancel"), InputEvent::TouchCancel);
		assert_eq!(parse("touch-frame"), InputEvent::TouchFrame);
		assert_eq!(parse("text a b€"), InputEvent::Text("a b€".into()));
		assert_eq!(parse(r"text \s\\\n\t\s"), InputEvent::Text(" \\\n\t ".into()));
		assert!(parse_err(r"text bad\x").contains("bad escape"));
	}

	#[test]
	fn parses_pen_events() {
		assert_eq!(
			parse("pen eraser contact 10 20.5 pressure=0.5 tilt=-3,4 buttons=5"),
			InputEvent::Pen(PenEvent {
				tool: PenTool::Eraser,
				x: 10.0,
				y: 20.5,
				contact: true,
				pressure: Some(0.5),
				distance: None,
				tilt: Some((-3.0, 4.0)),
				buttons: 5,
			})
		);
		assert_eq!(
			parse("pen pen hover 1 2 distance=0.25"),
			InputEvent::Pen(PenEvent {
				tool: PenTool::Pen,
				x: 1.0,
				y: 2.0,
				contact: false,
				pressure: None,
				distance: Some(0.25),
				tilt: None,
				buttons: 0,
			})
		);
		assert_eq!(parse("pen-buttons 3"), InputEvent::PenButtons { buttons: 3 });
		assert_eq!(parse("pen-leave"), InputEvent::PenLeave);
	}

	#[test]
	fn rejects_malformed_pen_lines() {
		assert!(parse_err("pen pen hover 1").contains("at least 4"));
		assert!(parse_err("pen stylus hover 1 2").contains("pen or eraser"));
		assert!(parse_err("pen pen touch 1 2").contains("hover or contact"));
		assert!(parse_err("pen pen hover 1 2 pressure").contains("key=value"));
		assert!(parse_err("pen pen hover 1 2 tilt=3").contains("tilt"));
		assert!(parse_err("pen pen hover 1 2 colour=3").contains("unknown option"));
		assert!(parse_err("pen pen hover 1 nan").contains("finite"));
		assert!(parse_err("pen-leave 1").contains("expected 0"));
	}

	#[test]
	fn rejects_malformed_touch_lines() {
		assert!(parse_err("touch-down 1 2").contains("expected 3"));
		assert!(parse_err("touch-down -1 2 3").contains("invalid touch slot"));
		assert!(parse_err("touch-motion 1 nan 3").contains("finite"));
		assert!(parse_err("touch-up").contains("expected 1"));
		assert!(parse_err("touch-frame 1").contains("expected 0"));
	}

	#[test]
	fn parses_key_press_and_release() {
		assert_eq!(
			parse("key 30 1"),
			InputEvent::Key {
				code: 30,
				pressed: true
			}
		);
		assert_eq!(
			parse("key 30 0"),
			InputEvent::Key {
				code: 30,
				pressed: false
			}
		);
	}

	#[test]
	fn parses_button() {
		assert_eq!(
			parse("button 272 1"),
			InputEvent::PointerButton {
				code: 272,
				pressed: true
			}
		);
		assert_eq!(
			parse("button 272 0"),
			InputEvent::PointerButton {
				code: 272,
				pressed: false
			}
		);
	}

	#[test]
	fn parses_absolute_motion() {
		assert_eq!(
			parse("motion-abs 640 360"),
			InputEvent::PointerMotion { x: 640.0, y: 360.0 }
		);
	}

	#[test]
	fn parses_relative_motion_with_signs_and_decimals() {
		assert_eq!(
			parse("motion-rel -1.5 0.25"),
			InputEvent::PointerMotionDelta { dx: -1.5, dy: 0.25 }
		);
	}

	#[test]
	fn parses_both_axes_with_signs_and_decimals() {
		assert_eq!(
			parse("axis v -1"),
			InputEvent::PointerAxis {
				horizontal: false,
				value: -1.0
			}
		);
		assert_eq!(
			parse("axis h 0.5"),
			InputEvent::PointerAxis {
				horizontal: true,
				value: 0.5
			}
		);
	}

	#[test]
	fn rejects_bad_axis_selector_and_value() {
		for token in ["x", "V", "vertical", "", "1"] {
			let msg = parse_err(&format!("axis {token} 1"));
			assert!(msg.contains(token), "message {msg:?} did not name {token:?}");
		}
		for token in ["nan", "inf", "-inf", "1e", "x"] {
			let msg = parse_err(&format!("axis v {token}"));
			assert!(msg.contains(token), "message {msg:?} did not name {token:?}");
		}
	}

	#[test]
	fn tolerates_extra_whitespace_and_tabs() {
		assert_eq!(
			parse("  key\t30   1  "),
			InputEvent::Key {
				code: 30,
				pressed: true
			}
		);
	}

	#[test]
	fn ignores_blank_and_comment_lines() {
		for line in ["", "   ", "\t", "#", "# a comment", "   # indented comment"] {
			assert_eq!(TextControlStream::parse_line(line).unwrap(), None, "line {line:?}");
		}
	}

	#[test]
	fn rejects_unknown_command_with_token() {
		let msg = parse_err("frobnicate 1");
		assert!(msg.contains("frobnicate"), "message was {msg:?}");
	}

	#[test]
	fn rejects_wrong_arity_with_command_name() {
		let msg = parse_err("key 30");
		assert!(msg.contains("key"), "message was {msg:?}");
		assert!(parse_err("key 30 1 2").contains("key"));
		assert!(parse_err("motion-abs 1").contains("motion-abs"));
		assert!(parse_err("axis v").contains("axis"));
		assert!(parse_err("axis v 1 2").contains("axis"));
	}

	#[test]
	fn rejects_invalid_evdev_code_with_token() {
		for token in ["abc", "-1", "1.5", "4294967296"] {
			let msg = parse_err(&format!("key {token} 1"));
			assert!(msg.contains(token), "message {msg:?} did not name {token:?}");
		}
	}

	#[test]
	fn rejects_invalid_press_flag_with_token() {
		for token in ["true", "2", "-1", "press"] {
			let msg = parse_err(&format!("button 272 {token}"));
			assert!(msg.contains(token), "message {msg:?} did not name {token:?}");
		}
	}

	#[test]
	fn rejects_invalid_and_non_finite_numbers() {
		for token in ["1e", "nan", "inf", "-inf", "x"] {
			let msg = parse_err(&format!("motion-abs {token} 0"));
			assert!(msg.contains(token), "message {msg:?} did not name {token:?}");
		}
	}

	#[test]
	fn deque_source_drains_and_then_blocks_empty() {
		let mut queue: VecDeque<InputEvent> = VecDeque::new();
		queue.push_back(InputEvent::Key {
			code: 30,
			pressed: true,
		});
		queue.push_back(InputEvent::PointerMotionDelta { dx: 1.0, dy: 2.0 });
		let events = queue.poll().unwrap();
		assert_eq!(events.len(), 2);
		assert_eq!(queue.poll().unwrap(), Vec::new());
	}

	#[test]
	fn unix_poll_returns_empty_without_data() {
		let (reader, _writer) = UnixStream::pair().unwrap();
		let mut source = UnixControlStream::new(reader).unwrap();
		// The writer stays open, so there is no EOF, only WouldBlock.
		assert_eq!(source.poll().unwrap(), Vec::new());
	}

	#[test]
	fn unix_buffers_partial_line_across_polls() {
		let (reader, mut writer) = UnixStream::pair().unwrap();
		let mut source = UnixControlStream::new(reader).unwrap();
		writer.write_all(b"key 3").unwrap();
		assert_eq!(source.poll().unwrap(), Vec::new());
		writer.write_all(b"0 1\n").unwrap();
		assert_eq!(
			source.poll().unwrap(),
			vec![InputEvent::Key {
				code: 30,
				pressed: true
			}]
		);
	}

	#[test]
	fn unix_decodes_multiple_lines_per_poll() {
		let (reader, mut writer) = UnixStream::pair().unwrap();
		let mut source = UnixControlStream::new(reader).unwrap();
		writer
			.write_all(b"key 30 1\nbutton 272 1\nmotion-abs 10.5 -20\n")
			.unwrap();
		assert_eq!(
			source.poll().unwrap(),
			vec![
				InputEvent::Key {
					code: 30,
					pressed: true
				},
				InputEvent::PointerButton {
					code: 272,
					pressed: true
				},
				InputEvent::PointerMotion { x: 10.5, y: -20.0 },
			]
		);
	}

	#[test]
	fn unix_skips_comments_and_blank_lines() {
		let (reader, mut writer) = UnixStream::pair().unwrap();
		let mut source = UnixControlStream::new(reader).unwrap();
		writer.write_all(b"# hello\n\n   \nkey 30 1\n").unwrap();
		assert_eq!(
			source.poll().unwrap(),
			vec![InputEvent::Key {
				code: 30,
				pressed: true
			}]
		);
	}

	#[test]
	fn unix_accepts_crlf() {
		let (reader, mut writer) = UnixStream::pair().unwrap();
		let mut source = UnixControlStream::new(reader).unwrap();
		writer.write_all(b"key 30 1\r\n").unwrap();
		assert_eq!(
			source.poll().unwrap(),
			vec![InputEvent::Key {
				code: 30,
				pressed: true
			}]
		);
	}

	#[test]
	fn unix_reports_eof_when_closed() {
		let (reader, writer) = UnixStream::pair().unwrap();
		let mut source = UnixControlStream::new(reader).unwrap();
		drop(writer);
		match source.poll() {
			Err(Error::Input(msg)) => assert_eq!(msg, "control stream closed"),
			other => panic!("expected closed error, got {other:?}"),
		}
	}

	#[test]
	fn unix_preserves_events_written_before_eof() {
		let (reader, mut writer) = UnixStream::pair().unwrap();
		let mut source = UnixControlStream::new(reader).unwrap();
		writer.write_all(b"key 30 1\n").unwrap();
		drop(writer);
		assert_eq!(
			source.poll().unwrap(),
			vec![InputEvent::Key {
				code: 30,
				pressed: true
			}]
		);
		// The close is reported on the next poll.
		assert!(matches!(source.poll(), Err(Error::Input(_))));
	}

	#[test]
	fn unix_rejects_invalid_utf8() {
		let (reader, mut writer) = UnixStream::pair().unwrap();
		let mut source = UnixControlStream::new(reader).unwrap();
		writer.write_all(&[0xff, b'\n']).unwrap();
		match source.poll() {
			Err(Error::Input(msg)) => assert!(msg.contains("UTF-8"), "message was {msg:?}"),
			other => panic!("expected UTF-8 error, got {other:?}"),
		}
	}

	#[test]
	fn unix_propagates_parse_error_with_token() {
		let (reader, mut writer) = UnixStream::pair().unwrap();
		let mut source = UnixControlStream::new(reader).unwrap();
		writer.write_all(b"frobnicate 1\n").unwrap();
		match source.poll() {
			Err(Error::Input(msg)) => assert!(msg.contains("frobnicate"), "message was {msg:?}"),
			other => panic!("expected parse error, got {other:?}"),
		}
	}

	#[test]
	fn unix_recovers_after_a_parse_error() {
		let (reader, mut writer) = UnixStream::pair().unwrap();
		let mut source = UnixControlStream::new(reader).unwrap();
		writer.write_all(b"bogus\nkey 30 1\n").unwrap();
		assert!(source.poll().is_err());
		// The following good line is still buffered and decodes next poll.
		assert_eq!(
			source.poll().unwrap(),
			vec![InputEvent::Key {
				code: 30,
				pressed: true
			}]
		);
	}
}
