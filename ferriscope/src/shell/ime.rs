//! Typing for `gamescope_input_method`: keyboard events, the temporary keymap
//! and its timers (`ime.cpp`).

use std::time::{Duration, Instant};

use smithay::{
	backend::input::{InputTime, KeyState},
	input::keyboard::{
		FilterResult, Keysym,
		xkb::{self, CONTEXT_NO_FLAGS, KEYMAP_COMPILE_NO_FLAGS, Keycode},
	},
	reexports::calloop::{
		RegistrationToken,
		timer::{TimeoutAction, Timer},
	},
	utils::{Point, SERIAL_COUNTER},
};

use crate::keyboard::KeyboardConfig;
use crate::{
	ime::{KEYCODE_OFFSET, TypedKeys, action_key, keysym_from_char, protocol::server::gamescope_input_method::Action},
	seat::InputEvent,
	shell::FerriscopeState,
};

/// How long a typed key stays down before it is released (`press_key`).
const KEY_HOLD: Duration = Duration::from_millis(30);
/// How long the seat stays idle before the temporary keymap is dropped
/// (`type_text`, `perform_action`).
const KEYMAP_RESET: Duration = Duration::from_millis(100);

/// The most text bytes waiting to be typed from [`InputEvent::Text`] events;
/// the rest is ignored so a huge payload cannot flood the focused client with
/// key events.
pub(crate) const MAX_TEXT_BYTES: usize = 64 * 1024;
/// How long [`InputEvent::Text`] events are gathered before they are typed.
///
/// A paste arrives one character per event. Every new character in the
/// temporary keymap is a keymap change, and Xwayland runs `xkbcomp` for each,
/// so typing them as they come costs milliseconds each and leaves X11 clients
/// with stale keymaps. Gathering what arrives in this window lets one keymap
/// carry many characters.
const TEXT_COALESCE: Duration = Duration::from_millis(16);
/// Evdev codes of the modifier keys pressed to reach a shifted level.
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTALT: u32 = 56;

/// The input method's typing state, shared by every `gamescope_input_method`
/// object: they all drive the one seat keyboard.
#[derive(Default)]
pub(in crate::shell) struct ImeTyper {
	keys: TypedKeys,
	/// The key currently down, with the modifiers pressed for it.
	held_key: Option<u32>,
	held_mods: Vec<u32>,
	/// Whether the seat keyboard carries the temporary keymap.
	custom_keymap: bool,
	/// Text from [`InputEvent::Text`] events not yet typed, and when it is.
	pending_text: String,
	flush_at: Option<Instant>,
	/// Whether the oversized-text warning was already logged.
	warned_oversized: bool,
	release_at: Option<Instant>,
	reset_at: Option<Instant>,
	timer: Option<(RegistrationToken, Instant)>,
	/// The seat's keymap, compiled on first use to find the key that types a
	/// character.
	default_keymap: Option<xkb::Keymap>,
}

impl ImeTyper {
	fn default_keymap(&mut self, config: &KeyboardConfig) -> Option<&xkb::Keymap> {
		if self.default_keymap.is_none() {
			let context = xkb::Context::new(CONTEXT_NO_FLAGS);
			let options = (!config.options.is_empty()).then(|| config.options.clone());
			self.default_keymap = xkb::Keymap::new_from_names(
				&context,
				&config.rules,
				&config.model,
				&config.layout,
				&config.variant,
				options,
				KEYMAP_COMPILE_NO_FLAGS,
			);
		}
		self.default_keymap.as_ref()
	}

	/// Forget the compiled seat keymap after a layout change.
	pub(in crate::shell) fn invalidate_default_keymap(&mut self) {
		self.default_keymap = None;
	}
}

/// The leading part of `text` that fits [`MAX_TEXT_BYTES`] with `queued` bytes
/// already waiting, cut at a character boundary.
fn text_within_limit(queued: usize, text: &str) -> &str {
	let mut end = text.len().min(MAX_TEXT_BYTES.saturating_sub(queued));
	while !text.is_char_boundary(end) {
		end -= 1;
	}
	&text[..end]
}

/// The key and modifier keys that type `keysym` on the default keymap.
///
/// The first key and level producing exactly this keysym whose modifier mask
/// is a subset of Shift/Ctrl/Alt (`try_type_keysym`).
fn find_key(keymap: &xkb::Keymap, keysym: Keysym) -> Option<(u32, Vec<u32>)> {
	let modifier_bit = |name: &str| {
		let index = keymap.mod_get_index(name);
		(index != xkb::MOD_INVALID).then(|| 1u32 << index)
	};
	let shift = modifier_bit(xkb::MOD_NAME_SHIFT);
	let ctrl = modifier_bit(xkb::MOD_NAME_CTRL);
	let alt = modifier_bit(xkb::MOD_NAME_ALT);
	let allowed = [shift, ctrl, alt].into_iter().flatten().fold(0, |all, bit| all | bit);

	let (min, max) = (keymap.min_keycode().raw(), keymap.max_keycode().raw());
	for raw in min..=max {
		let key = Keycode::new(raw);
		for layout in 0..keymap.num_layouts_for_key(key) {
			for level in 0..keymap.num_levels_for_key(key, layout) {
				if keymap.key_get_syms_by_level(key, layout, level) != [keysym] {
					continue;
				}
				let mut masks = [0u32; 1];
				if keymap.key_get_mods_for_level(key, layout, level, &mut masks) != 1 {
					continue;
				}
				let mask = masks[0];
				if mask & allowed != mask {
					continue;
				}
				let mut mods = Vec::new();
				if shift.is_some_and(|bit| mask & bit != 0) {
					mods.push(KEY_LEFTSHIFT);
				}
				if ctrl.is_some_and(|bit| mask & bit != 0) {
					mods.push(KEY_LEFTCTRL);
				}
				if alt.is_some_and(|bit| mask & bit != 0) {
					mods.push(KEY_LEFTALT);
				}
				return raw.checked_sub(KEYCODE_OFFSET).map(|evdev| (evdev, mods));
			}
		}
	}
	None
}

impl FerriscopeState {
	/// Type `text` into the focused client (`type_text`).
	pub(crate) fn ime_type_text(&mut self, text: &str) {
		self.ime_flush_text();
		// A single character the seat keymap can type needs no keymap switch
		// (and none while a temporary keymap is still installed).
		if self.ime.keys.is_empty() && text.len() == 1 {
			if let Some(keysym) = text.chars().next().and_then(keysym_from_char) {
				if self.ime_type_default(keysym) {
					return;
				}
			}
		}
		self.ime_type_temporary(text);
	}

	/// Type `text` through the temporary keymap.
	fn ime_type_temporary(&mut self, text: &str) {
		let mut keycodes = Vec::new();
		for ch in text.chars() {
			match self.ime.keys.keycode_for(ch) {
				Some(keycode) => keycodes.push(keycode),
				None => tracing::warn!(ch = %ch.escape_unicode(), "ime: cannot type character"),
			}
		}
		if keycodes.is_empty() || !self.ime_ensure_keymap() {
			return;
		}
		for keycode in keycodes {
			self.ime_press_key(keycode, &[]);
		}
		self.ime.reset_at = Some(Instant::now() + KEYMAP_RESET);
		self.ime_arm();
	}

	/// Queue text from the embedder ([`InputEvent::Text`]) to be typed.
	///
	/// At most [`MAX_TEXT_BYTES`] wait at once. The text is typed after
	/// [`TEXT_COALESCE`], or sooner when a key or action must follow it in
	/// order.
	pub(crate) fn ime_queue_text(&mut self, text: &str) {
		let accepted = text_within_limit(self.ime.pending_text.len(), text);
		if accepted.len() < text.len() && !std::mem::replace(&mut self.ime.warned_oversized, true) {
			tracing::warn!(
				bytes = text.len(),
				limit = MAX_TEXT_BYTES,
				"ime: too much text queued, ignoring the excess"
			);
		}
		if accepted.is_empty() {
			return;
		}
		self.ime.pending_text.push_str(accepted);
		if self.ime.flush_at.is_none() {
			self.ime.flush_at = Some(Instant::now() + TEXT_COALESCE);
			self.ime_arm();
		}
	}

	/// Type the queued text, in as few keymap changes as it allows.
	///
	/// Runs of characters the seat keymap produces are typed as plain keys
	/// while no temporary keymap is installed. Anything else goes through the
	/// temporary keymap in pieces that fit its spare keys, one keymap change per
	/// piece. When the keys run out the seat keymap is restored and the
	/// bindings start over: a key rebound in place is not reliably seen by X11
	/// clients, where `keycode_from_ch` would reuse the oldest key.
	pub(crate) fn ime_flush_text(&mut self) {
		self.ime.flush_at = None;
		let text = std::mem::take(&mut self.ime.pending_text);
		let mut rest = text.as_str();
		while !rest.is_empty() {
			if self.ime.keys.is_empty() {
				rest = &rest[self.ime_type_default_run(rest)..];
				if rest.is_empty() {
					break;
				}
			}
			let end = self.ime.keys.fitting_prefix(rest);
			if end == 0 {
				self.ime_release_held();
				self.ime_restore_keymap();
				continue;
			}
			self.ime_type_temporary(&rest[..end]);
			rest = &rest[end..];
		}
		self.ime_arm();
	}

	/// Type the leading characters of `text` the seat keymap produces; returns
	/// how many bytes that was.
	fn ime_type_default_run(&mut self, text: &str) -> usize {
		for (index, ch) in text.char_indices() {
			let typed = keysym_from_char(ch).is_some_and(|keysym| self.ime_type_default(keysym));
			if !typed {
				return index;
			}
		}
		text.len()
	}

	/// Press the key for an editing action (`perform_action`).
	pub(crate) fn ime_perform_action(&mut self, action: Action) {
		self.ime_flush_text();
		let Some(key) = action_key(action) else {
			if action != Action::None {
				tracing::warn!(?action, "ime: unsupported action");
			}
			return;
		};
		if self.ime.keys.is_empty() && self.ime_type_default(key.keysym) {
			return;
		}
		// The temporary keymap always contains every action key.
		if !self.ime_ensure_keymap() {
			return;
		}
		self.ime_press_key(key.keycode, &[]);
		self.ime.reset_at = Some(Instant::now() + KEYMAP_RESET);
		self.ime_arm();
	}

	/// Release the typed key (and its modifiers) now.
	pub(crate) fn ime_release_held(&mut self) {
		self.ime.release_at = None;
		if let Some(key) = self.ime.held_key.take() {
			self.ime_key_event(key, false);
		}
		for modifier in std::mem::take(&mut self.ime.held_mods).into_iter().rev() {
			self.ime_key_event(modifier, false);
		}
	}

	/// `pointer_motion`: a relative pointer delta.
	pub(crate) fn ime_pointer_motion(&mut self, dx: f64, dy: f64) {
		self.apply_input(InputEvent::PointerMotionDelta { dx, dy });
	}

	/// `pointer_warp`: an absolute position in the focused window's
	/// surface-local space, which `wlserver_mousewarp` takes; mapped back into
	/// output space for the pointer path.
	pub(crate) fn ime_pointer_warp(&mut self, x: f64, y: f64) {
		let output = self.scene_to_output(Point::from((x, y)));
		self.apply_input(InputEvent::PointerMotion {
			x: output.x,
			y: output.y,
		});
	}

	/// `pointer_wheel`: 120 units per notch, `x` horizontal and `y` vertical,
	/// in the `wl_pointer.axis` sign convention (`wlserver_mousewheel`).
	pub(crate) fn ime_pointer_wheel(&mut self, x: i32, y: i32) {
		for (horizontal, units) in [(true, x), (false, y)] {
			if units != 0 {
				self.apply_input(InputEvent::PointerAxis {
					horizontal,
					value: f64::from(units) / 120.0,
				});
			}
		}
	}

	/// `pointer_button`: a Linux button code.
	pub(crate) fn ime_pointer_button(&mut self, code: u32, pressed: bool) {
		self.apply_input(InputEvent::PointerButton { code, pressed });
	}

	/// Type `keysym` with the seat's default keymap, if a key produces it.
	///
	/// Returns whether it was typed. A temporary keymap still installed is
	/// dropped first so the key means what it was looked up as.
	fn ime_type_default(&mut self, keysym: Keysym) -> bool {
		let config = self.keyboard_config();
		let Some(keymap) = self.ime.default_keymap(&config) else {
			return false;
		};
		let Some((keycode, mods)) = find_key(keymap, keysym) else {
			return false;
		};
		self.ime_release_held();
		self.ime_restore_keymap();
		self.ime_press_key(keycode, &mods);
		true
	}

	/// Have the temporary keymap with the typed characters installed, sending a
	/// new one to the clients only if the bindings changed since the last.
	fn ime_ensure_keymap(&mut self) -> bool {
		let changed = self.ime.keys.take_changed();
		if self.ime.custom_keymap && !changed {
			return true;
		}
		if self.ime_install_keymap() {
			return true;
		}
		self.ime.keys.mark_changed();
		false
	}

	/// Install the temporary keymap with the typed characters and actions.
	fn ime_install_keymap(&mut self) -> bool {
		let Some(text) = self.ime.keys.keymap_text() else {
			tracing::error!("ime: failed to generate keymap");
			return false;
		};
		// The key must be up before the keymap under it changes.
		self.ime_release_held();
		let Some(keyboard) = self.seat.get_keyboard() else {
			return false;
		};
		if let Err(e) = keyboard.set_keymap_from_string(self, text) {
			tracing::error!("ime: install keymap: {e:?}");
			return false;
		}
		self.ime.custom_keymap = true;
		true
	}

	/// Put the seat's default keymap back and forget the typed bindings.
	fn ime_restore_keymap(&mut self) {
		self.ime.keys.clear();
		self.ime.reset_at = None;
		if !std::mem::take(&mut self.ime.custom_keymap) {
			return;
		}
		if let Some(keyboard) = self.seat.get_keyboard() {
			let config = self.keyboard_config();
			if let Err(e) = keyboard.set_xkb_config(self, config.xkb_config()) {
				tracing::error!("ime: restore keymap: {e:?}");
			}
		}
	}

	/// Press `evdev` with `mods` held, releasing whatever was down first; the
	/// key is released by the timer (`press_key`).
	fn ime_press_key(&mut self, evdev: u32, mods: &[u32]) {
		self.ime_release_held();
		for modifier in mods {
			self.ime_key_event(*modifier, true);
		}
		self.ime_key_event(evdev, true);
		self.ime.held_key = Some(evdev);
		self.ime.held_mods = mods.to_vec();
		self.ime.release_at = Some(Instant::now() + KEY_HOLD);
		self.ime_arm();
	}

	/// Send one key event to the focused client, bypassing the hotkey filter:
	/// gamescope notifies the seat directly (`wlr_seat_keyboard_notify_key`).
	fn ime_key_event(&mut self, evdev: u32, pressed: bool) {
		let Some(keyboard) = self.seat.get_keyboard() else {
			return;
		};
		let state = if pressed { KeyState::Pressed } else { KeyState::Released };
		keyboard.input::<(), _>(
			self,
			Keycode::new(evdev + KEYCODE_OFFSET),
			state,
			SERIAL_COUNTER.next_serial(),
			InputTime::now(),
			|_, _, _| FilterResult::Forward,
		);
	}

	/// Arm the timer for the earliest pending release or reset.
	fn ime_arm(&mut self) {
		let deadline = [self.ime.release_at, self.ime.reset_at, self.ime.flush_at]
			.into_iter()
			.flatten()
			.min();
		let Some(deadline) = deadline else {
			if let Some((token, _)) = self.ime.timer.take() {
				self.loop_handle.remove(token);
			}
			return;
		};
		if let Some((token, armed)) = self.ime.timer.take() {
			if armed <= deadline {
				self.ime.timer = Some((token, armed));
				return;
			}
			self.loop_handle.remove(token);
		}
		let timer = Timer::from_deadline(deadline);
		match self.loop_handle.insert_source(timer, |_, _, state: &mut Self| {
			state.ime.timer = None;
			state.ime_on_timer();
			TimeoutAction::Drop
		}) {
			Ok(token) => self.ime.timer = Some((token, deadline)),
			Err(e) => tracing::error!("arm ime timer: {e:?}"),
		}
	}

	fn ime_on_timer(&mut self) {
		let now = Instant::now();
		if self.ime.release_at.is_some_and(|at| at <= now) {
			self.ime_release_held();
		}
		if self.ime.flush_at.is_some_and(|at| at <= now) {
			self.ime_flush_text();
		}
		if self.ime.reset_at.is_some_and(|at| at <= now) {
			self.ime_release_held();
			self.ime_restore_keymap();
		}
		self.ime_arm();
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use smithay::input::keyboard::keysyms;

	fn us_keymap() -> xkb::Keymap {
		let context = xkb::Context::new(CONTEXT_NO_FLAGS);
		xkb::Keymap::new_from_names(&context, "", "", "us", "", None, KEYMAP_COMPILE_NO_FLAGS).expect("us keymap")
	}

	#[test]
	fn queued_text_is_cut_at_the_limit_on_a_character_boundary() {
		assert_eq!(text_within_limit(0, "abc"), "abc");
		assert_eq!(text_within_limit(MAX_TEXT_BYTES - 2, "abc"), "ab");
		assert_eq!(text_within_limit(MAX_TEXT_BYTES, "abc"), "");
		// A three-byte character that does not fit whole is dropped whole.
		assert_eq!(text_within_limit(MAX_TEXT_BYTES - 4, "ab€"), "ab");
		assert_eq!(text_within_limit(MAX_TEXT_BYTES - 5, "ab€"), "ab€");
	}

	#[test]
	fn a_lowercase_letter_is_a_bare_key() {
		let (evdev, mods) = find_key(&us_keymap(), Keysym::new(keysyms::KEY_a)).expect("a");
		assert_eq!(evdev, 30, "KEY_A");
		assert!(mods.is_empty());
	}

	#[test]
	fn an_uppercase_letter_presses_shift() {
		let (evdev, mods) = find_key(&us_keymap(), Keysym::new(keysyms::KEY_A)).expect("A");
		assert_eq!(evdev, 30, "KEY_A");
		assert_eq!(mods, [KEY_LEFTSHIFT]);
	}

	#[test]
	fn shifted_punctuation_presses_shift() {
		let (evdev, mods) = find_key(&us_keymap(), Keysym::new(keysyms::KEY_exclam)).expect("!");
		assert_eq!(evdev, 2, "KEY_1");
		assert_eq!(mods, [KEY_LEFTSHIFT]);
	}

	#[test]
	fn a_keysym_the_layout_cannot_type_is_not_found() {
		assert_eq!(find_key(&us_keymap(), Keysym::new(0x0100_4e2d)), None);
	}

	#[test]
	fn action_keysyms_are_found_on_their_own_keys() {
		let keymap = us_keymap();
		let (evdev, mods) = find_key(&keymap, Keysym::new(keysyms::KEY_BackSpace)).expect("BackSpace");
		assert_eq!(evdev, crate::ime::KEY_BACKSPACE);
		assert!(mods.is_empty());
		let (evdev, _) = find_key(&keymap, Keysym::new(keysyms::KEY_Delete)).expect("Delete");
		assert_eq!(evdev, crate::ime::KEY_DELETE);
	}
}
