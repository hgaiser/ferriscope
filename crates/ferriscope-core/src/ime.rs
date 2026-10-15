//! `gamescope_input_method`: text from Steam's on-screen keyboard.
//!
//! Steam's virtual keyboard binds the gamescope-private
//! `gamescope_input_method_manager` global, creates an input method and sends
//! text (`set_string`) or editing actions (`set_action`), applied on `commit`.
//! The compositor turns that into keyboard events for the focused client, the
//! way gamescope does (`ime.cpp`): a printable ASCII character that the
//! seat's own keymap can produce is typed with that key (plus Shift/Ctrl/Alt
//! when its level needs them); anything else is typed through a temporary
//! keymap that binds each character's keysym to a spare evdev key, installed
//! for the duration of the burst and reset after an idle period.
//!
//! This module owns the protocol bindings, the protocol dispatch and the pure
//! text-to-key mapping ([`TypedKeys`]). The stateful typing (timers, the seat
//! keyboard) lives in `shell/ime.rs`. The same protocol also carries pointer
//! requests for Steam Input (`pointer_motion`, `pointer_warp`,
//! `pointer_wheel`, `pointer_button`), which are forwarded to the seat pointer.

use std::{collections::VecDeque, sync::Mutex};

use smithay::{
	input::keyboard::{
		Keysym, keysyms,
		xkb::{keysym_get_name, utf32_to_keysym},
	},
	wayland::{Dispatch2, GlobalDispatch2},
};
use wayland_server::{
	Client, DataInit, DisplayHandle, New, WEnum,
	backend::{ClientId, GlobalId},
};

use crate::shell::FerriscopeState;

/// Server bindings for the vendored `gamescope_input_method` protocol.
pub mod protocol {
	#![allow(
		dead_code,
		non_camel_case_types,
		unused_unsafe,
		unused_variables,
		non_upper_case_globals,
		non_snake_case,
		unused_imports,
		missing_docs
	)]
	#![allow(clippy::all)]

	/// Server-side API of the protocol.
	pub mod server {
		use wayland_server;
		use wayland_server::protocol::*;

		pub mod __interfaces {
			use wayland_server::protocol::__interfaces::*;
			wayland_scanner::generate_interfaces!("protocol/gamescope-input-method.xml");
		}
		use self::__interfaces::*;

		wayland_scanner::generate_server_code!("protocol/gamescope-input-method.xml");
	}
}

use protocol::server::gamescope_input_method::{self, Action, ButtonState, GamescopeInputMethod};
use protocol::server::gamescope_input_method_manager::{self, GamescopeInputMethodManager};

/// The protocol version gamescope advertises (`IME_MANAGER_VERSION`).
const MANAGER_VERSION: u32 = 3;

/// Evdev codes of the keys the temporary keymap may use for characters.
///
/// Some clients interpret keycodes as evdev, so only keys that normally
/// produce characters are used (`ime.cpp` `allow_keycodes`): the digit row,
/// the three letter rows with their punctuation, and the comma/dot/slash keys.
pub(crate) const ALLOW_KEYCODES: [u32; 47] = [
	2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, // KEY_1 .. KEY_EQUAL
	16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, // KEY_Q .. KEY_RIGHTBRACE
	30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 43, // KEY_A .. KEY_GRAVE, KEY_BACKSLASH
	44, 45, 46, 47, 48, 49, 50, 51, 52, 53, // KEY_Z .. KEY_SLASH
];

/// Evdev codes (`linux/input-event-codes.h`) of the action keys.
pub(crate) const KEY_BACKSPACE: u32 = 14;
const KEY_ENTER: u32 = 28;
const KEY_UP: u32 = 103;
const KEY_LEFT: u32 = 105;
const KEY_RIGHT: u32 = 106;
const KEY_DOWN: u32 = 108;
pub(crate) const KEY_DELETE: u32 = 111;
/// The lowest and highest evdev codes the temporary keymap declares
/// (`KEY_1` and `KEY_DELETE`), the keymap's `minimum`/`maximum` minus 8.
const MIN_KEYCODE: u32 = 2;
const MAX_KEYCODE: u32 = KEY_DELETE;

/// The offset between evdev codes and XKB keycodes.
pub(crate) const KEYCODE_OFFSET: u32 = 8;

/// An evdev key together with the keysym it types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImeKey {
	/// Evdev keycode.
	pub(crate) keycode: u32,
	/// The keysym the key produces.
	pub(crate) keysym: Keysym,
}

/// The key an editing action presses (`ime.cpp` `actions`); `None` for
/// `Action::None` and values the protocol does not define.
pub(crate) fn action_key(action: Action) -> Option<ImeKey> {
	let (keycode, keysym) = match action {
		Action::Submit => (KEY_ENTER, keysyms::KEY_Return),
		Action::DeleteLeft => (KEY_BACKSPACE, keysyms::KEY_BackSpace),
		Action::DeleteRight => (KEY_DELETE, keysyms::KEY_Delete),
		Action::MoveLeft => (KEY_LEFT, keysyms::KEY_Left),
		Action::MoveRight => (KEY_RIGHT, keysyms::KEY_Right),
		Action::MoveUp => (KEY_UP, keysyms::KEY_Up),
		Action::MoveDown => (KEY_DOWN, keysyms::KEY_Down),
		_ => return None,
	};
	Some(ImeKey {
		keycode,
		keysym: Keysym::new(keysym),
	})
}

/// Every action key, for the keymap (it always contains all of them).
const ACTION_KEYS: [Action; 7] = [
	Action::Submit,
	Action::DeleteLeft,
	Action::DeleteRight,
	Action::MoveLeft,
	Action::MoveRight,
	Action::MoveUp,
	Action::MoveDown,
];

/// The keysym a character types (`keysym_from_ch`).
///
/// Returns `None` when the character has no keysym. The Euro sign is mapped
/// explicitly (libxkbcommon's conversion has been wrong for it), and a few
/// characters CEF does not accept as legacy keysyms (Hangul compatibility
/// jamo, a subset of Hangul jamo, per-mille) use the `0x1000000 | ucs`
/// Unicode keysym form instead.
pub(crate) fn keysym_from_char(ch: char) -> Option<Keysym> {
	let ucs = u32::from(ch);
	if ucs == 0x20ac {
		return Some(Keysym::new(keysyms::KEY_EuroSign));
	}
	let cef_unicode_only = (0x3130..=0x318f).contains(&ucs)
		|| (0x11a8..=0x11c2).contains(&ucs)
		|| matches!(ucs, 0x11eb | 0x11f0 | 0x11f9 | 0x2030);
	if cef_unicode_only {
		return Some(Keysym::new(ucs | 0x0100_0000));
	}
	let keysym = utf32_to_keysym(ucs);
	(keysym.raw() != keysyms::KEY_NoSymbol).then_some(keysym)
}

/// The spare evdev keys currently bound to typed characters.
///
/// Characters are assigned the next key of [`ALLOW_KEYCODES`] in order; a
/// character that is still bound reuses its key (so `"ll"` presses one key
/// twice); when all keys are bound the oldest binding is dropped and its
/// key is reused (`keycode_from_ch`).
#[derive(Debug, Default)]
pub(crate) struct TypedKeys {
	keys: VecDeque<ImeKey>,
	next_index: usize,
	/// Whether a binding was added or replaced since the keymap was last
	/// generated.
	changed: bool,
}

impl TypedKeys {
	/// Whether no character is bound, i.e. the seat keymap is the default.
	pub(crate) fn is_empty(&self) -> bool {
		self.keys.is_empty()
	}

	/// Forget every binding; the next character starts at the first key.
	pub(crate) fn clear(&mut self) {
		self.keys.clear();
		self.next_index = 0;
		self.changed = false;
	}

	/// The byte length of the longest prefix of `text` whose characters all
	/// fit the spare keys, binding the unbound ones to new keys.
	///
	/// Characters without a keysym cost nothing (they are skipped when typed).
	/// `0` when even the first character with a keysym does not fit.
	pub(crate) fn fitting_prefix(&self, text: &str) -> usize {
		let room = ALLOW_KEYCODES.len() - self.keys.len();
		let mut missing: Vec<Keysym> = Vec::new();
		for (index, ch) in text.char_indices() {
			let Some(keysym) = keysym_from_char(ch) else {
				continue;
			};
			if self.keys.iter().any(|key| key.keysym == keysym) || missing.contains(&keysym) {
				continue;
			}
			if missing.len() == room {
				return index;
			}
			missing.push(keysym);
		}
		text.len()
	}

	/// Whether the bindings changed since the last call, i.e. the installed
	/// keymap is stale. A character that is already bound leaves it as is.
	pub(crate) fn take_changed(&mut self) -> bool {
		std::mem::take(&mut self.changed)
	}

	/// Mark the bindings as not yet installed (an install failed).
	pub(crate) fn mark_changed(&mut self) {
		self.changed = true;
	}

	/// The evdev key that types `ch`, binding one if needed.
	///
	/// `None` when the character has no keysym.
	pub(crate) fn keycode_for(&mut self, ch: char) -> Option<u32> {
		let keysym = keysym_from_char(ch)?;
		// A character that is still bound reuses its key, not only a repeat of
		// the previous one (gamescope reuses just that): the keymap then stays
		// as installed, which keeps a long paste from re-sending it to the
		// client for every character.
		if let Some(bound) = self.keys.iter().rev().find(|key| key.keysym == keysym) {
			return Some(bound.keycode);
		}
		if self.keys.len() >= ALLOW_KEYCODES.len() {
			tracing::warn!("ime: key codes wrapped within the reset window");
			self.keys.pop_front();
		}
		let keycode = ALLOW_KEYCODES[self.next_index % ALLOW_KEYCODES.len()];
		self.next_index += 1;
		self.keys.push_back(ImeKey { keycode, keysym });
		self.changed = true;
		Some(keycode)
	}

	/// The XKB text keymap binding the typed characters and every action key
	/// (`generate_keymap`).
	///
	/// `None` when a keysym has no name. The compat section carries one
	/// `interpret` so a libxkbcommon that serializes only matched interprets
	/// still gives Xwayland something to read.
	pub(crate) fn keymap_text(&self) -> Option<String> {
		use std::fmt::Write;
		let bound: Vec<ImeKey> = self
			.keys
			.iter()
			.copied()
			.chain(ACTION_KEYS.iter().filter_map(|action| action_key(*action)))
			.collect();
		let mut text = String::new();
		let _ = write!(
			text,
			"xkb_keymap {{\n\nxkb_keycodes \"(unnamed)\" {{\n\tminimum = {};\n\tmaximum = {};\n",
			KEYCODE_OFFSET + MIN_KEYCODE,
			KEYCODE_OFFSET + MAX_KEYCODE
		);
		for key in &bound {
			let _ = writeln!(text, "\t<K{}> = {};", key.keycode, key.keycode + KEYCODE_OFFSET);
		}
		text.push_str(
			"};\n\nxkb_types \"(unnamed)\" { include \"complete\" };\n\n\
			 xkb_compatibility \"(unnamed)\" {\n\tinclude \"complete\"\n\
			 \tinterpret Any+AnyOfOrNone(all) { action= NoAction(); };\n};\n\n\
			 xkb_symbols \"(unnamed)\" {\n",
		);
		for key in &bound {
			let name = keysym_get_name(key.keysym);
			if name.is_empty() {
				return None;
			}
			let _ = writeln!(text, "\tkey <K{}> {{[ {name} ]}};", key.keycode);
		}
		text.push_str("};\n\n};\n");
		Some(text)
	}
}

/// State carried by one `gamescope_input_method` object: the double-buffered
/// request state.
#[derive(Debug, Default)]
pub struct ImeData {
	pending: Mutex<Pending>,
}

#[derive(Debug, Default)]
struct Pending {
	string: Option<String>,
	action: Option<Action>,
}

/// The serial sent in `done` and required back in `commit`; gamescope never
/// changes it (`ime->serial = 1`).
const SERIAL: u32 = 1;

/// Global data for the manager.
#[derive(Debug)]
pub struct ImeManagerData;

/// Create the `gamescope_input_method_manager` global.
pub fn global(display: &DisplayHandle) -> GlobalId {
	display
		.create_global::<FerriscopeState, GamescopeInputMethodManager, ImeManagerData>(MANAGER_VERSION, ImeManagerData)
}

impl GlobalDispatch2<GamescopeInputMethodManager, FerriscopeState> for ImeManagerData {
	fn bind(
		&self,
		_state: &mut FerriscopeState,
		_handle: &DisplayHandle,
		_client: &Client,
		resource: New<GamescopeInputMethodManager>,
		data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		data_init.init(resource, ImeManagerData);
	}
}

impl Dispatch2<GamescopeInputMethodManager, FerriscopeState> for ImeManagerData {
	fn request(
		&self,
		_state: &mut FerriscopeState,
		_client: &Client,
		_resource: &GamescopeInputMethodManager,
		request: gamescope_input_method_manager::Request,
		_handle: &DisplayHandle,
		data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		match request {
			gamescope_input_method_manager::Request::Destroy => {},
			gamescope_input_method_manager::Request::CreateInputMethod { input_method, .. } => {
				let object = data_init.init(input_method, ImeData::default());
				object.done(SERIAL);
			},
		}
	}
}

impl Dispatch2<GamescopeInputMethod, FerriscopeState> for ImeData {
	fn request(
		&self,
		state: &mut FerriscopeState,
		_client: &Client,
		_resource: &GamescopeInputMethod,
		request: gamescope_input_method::Request,
		_handle: &DisplayHandle,
		_data_init: &mut DataInit<'_, FerriscopeState>,
	) {
		use gamescope_input_method::Request;
		match request {
			Request::Destroy => {},
			Request::SetString { text } => self.pending.lock().expect("ime pending lock").string = Some(text),
			Request::SetAction { action } => {
				self.pending.lock().expect("ime pending lock").action = match action {
					WEnum::Value(action) => Some(action),
					WEnum::Unknown(_) => None,
				};
			},
			Request::Commit { serial } => {
				if serial != SERIAL {
					return;
				}
				let pending = std::mem::take(&mut *self.pending.lock().expect("ime pending lock"));
				if let Some(text) = pending.string {
					state.ime_type_text(&text);
				}
				if let Some(action) = pending.action {
					state.ime_perform_action(action);
				}
			},
			Request::PointerMotion { dx, dy } => state.ime_pointer_motion(dx, dy),
			Request::PointerWarp { x, y } => state.ime_pointer_warp(x, y),
			Request::PointerWheel { x, y } => state.ime_pointer_wheel(x, y),
			Request::PointerButton {
				button,
				state: button_state,
			} => {
				state.ime_pointer_button(button, matches!(button_state, WEnum::Value(ButtonState::Pressed)));
			},
		}
	}

	fn destroyed(&self, state: &mut FerriscopeState, _client: ClientId, _resource: &GamescopeInputMethod) {
		// A key still held when the input method goes away would stay down.
		state.ime_release_held();
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use smithay::input::keyboard::xkb::{
		self, CONTEXT_NO_FLAGS, KEYMAP_COMPILE_NO_FLAGS, KEYMAP_FORMAT_TEXT_V1, Keycode,
	};

	fn compile(text: String) -> xkb::Keymap {
		let context = xkb::Context::new(CONTEXT_NO_FLAGS);
		xkb::Keymap::new_from_string(&context, text, KEYMAP_FORMAT_TEXT_V1, KEYMAP_COMPILE_NO_FLAGS)
			.expect("the generated keymap compiles")
	}

	/// The keysym an evdev key types in `keymap` (first layout, first level).
	fn typed_sym(keymap: &xkb::Keymap, evdev: u32) -> Keysym {
		let syms = keymap.key_get_syms_by_level(Keycode::new(evdev + KEYCODE_OFFSET), 0, 0);
		assert_eq!(syms.len(), 1, "evdev {evdev} should type exactly one keysym");
		syms[0]
	}

	#[test]
	fn plain_characters_map_to_their_keysyms() {
		assert_eq!(keysym_from_char('a'), Some(Keysym::new(keysyms::KEY_a)));
		assert_eq!(keysym_from_char('A'), Some(Keysym::new(keysyms::KEY_A)));
		assert_eq!(keysym_from_char(' '), Some(Keysym::new(keysyms::KEY_space)));
	}

	#[test]
	fn euro_sign_and_cef_exceptions_have_dedicated_keysyms() {
		assert_eq!(keysym_from_char('€'), Some(Keysym::new(keysyms::KEY_EuroSign)));
		// Hangul compatibility jamo and per-mille use the Unicode keysym form.
		assert_eq!(keysym_from_char('\u{3131}'), Some(Keysym::new(0x0100_3131)));
		assert_eq!(keysym_from_char('\u{2030}'), Some(Keysym::new(0x0100_2030)));
		assert_eq!(keysym_from_char('\u{11a8}'), Some(Keysym::new(0x0100_11a8)));
	}

	#[test]
	fn characters_get_consecutive_spare_keys_and_repeats_reuse() {
		let mut keys = TypedKeys::default();
		assert_eq!(keys.keycode_for('é'), Some(ALLOW_KEYCODES[0]));
		assert_eq!(keys.keycode_for('€'), Some(ALLOW_KEYCODES[1]));
		// A bound character reuses its key, wherever it was bound.
		assert_eq!(keys.keycode_for('€'), Some(ALLOW_KEYCODES[1]));
		assert_eq!(keys.keycode_for('é'), Some(ALLOW_KEYCODES[0]));
		assert_eq!(keys.keycode_for('ü'), Some(ALLOW_KEYCODES[2]));
		keys.clear();
		assert!(keys.is_empty());
		assert_eq!(keys.keycode_for('ü'), Some(ALLOW_KEYCODES[0]));
	}

	#[test]
	fn the_keymap_is_stale_only_when_a_binding_is_new() {
		let mut keys = TypedKeys::default();
		assert!(!keys.take_changed());
		keys.keycode_for('é');
		assert!(keys.take_changed());
		keys.keycode_for('é');
		assert!(!keys.take_changed(), "a bound character changes nothing");
		keys.keycode_for('€');
		assert!(keys.take_changed());
	}

	#[test]
	fn a_prefix_fits_while_the_distinct_characters_fit_the_spare_keys() {
		let mut keys = TypedKeys::default();
		let all: String = ('\u{00c0}'..).take(ALLOW_KEYCODES.len() + 5).collect();
		let end = keys.fitting_prefix(&all);
		assert_eq!(all[..end].chars().count(), ALLOW_KEYCODES.len());
		// Repeats and bound characters cost nothing, a control character is skipped.
		assert_eq!(keys.fitting_prefix("éé\u{1}é€"), "éé\u{1}é€".len());
		for ch in all[..end].chars() {
			keys.keycode_for(ch);
		}
		assert_eq!(keys.fitting_prefix("\u{00c0}\u{00c1}€"), "\u{00c0}\u{00c1}".len());
		assert_eq!(keys.fitting_prefix("€"), 0);
	}

	#[test]
	fn running_out_of_keys_reuses_the_oldest() {
		let mut keys = TypedKeys::default();
		let text: Vec<char> = ('\u{00c0}'..).take(ALLOW_KEYCODES.len() + 1).collect();
		let codes: Vec<u32> = text.iter().map(|ch| keys.keycode_for(*ch).expect("keysym")).collect();
		assert_eq!(codes[ALLOW_KEYCODES.len()], ALLOW_KEYCODES[0]);
		// The oldest binding was dropped, so the reused key now types the newest.
		let keymap = compile(keys.keymap_text().expect("keymap"));
		assert_eq!(
			typed_sym(&keymap, ALLOW_KEYCODES[0]),
			keysym_from_char(text[ALLOW_KEYCODES.len()]).expect("keysym")
		);
	}

	#[test]
	fn keymap_types_the_bound_characters_and_all_actions() {
		let mut keys = TypedKeys::default();
		let euro = keys.keycode_for('€').expect("euro");
		let e_acute = keys.keycode_for('é').expect("e acute");
		let hangul = keys.keycode_for('\u{3131}').expect("hangul");
		let keymap = compile(keys.keymap_text().expect("keymap"));

		assert_eq!(typed_sym(&keymap, euro), Keysym::new(keysyms::KEY_EuroSign));
		assert_eq!(typed_sym(&keymap, e_acute), Keysym::new(keysyms::KEY_eacute));
		assert_eq!(typed_sym(&keymap, hangul), Keysym::new(0x0100_3131));
		for action in ACTION_KEYS {
			let key = action_key(action).expect("action key");
			assert_eq!(typed_sym(&keymap, key.keycode), key.keysym, "{action:?}");
		}
	}

	#[test]
	fn an_empty_keymap_still_carries_the_actions() {
		let keymap = compile(TypedKeys::default().keymap_text().expect("keymap"));
		assert_eq!(typed_sym(&keymap, KEY_ENTER), Keysym::new(keysyms::KEY_Return));
		assert_eq!(typed_sym(&keymap, KEY_BACKSPACE), Keysym::new(keysyms::KEY_BackSpace));
	}

	#[test]
	fn action_none_has_no_key() {
		assert_eq!(action_key(Action::None), None);
	}

	#[test]
	fn spare_keys_stay_inside_the_declared_keycode_range() {
		assert!(
			ALLOW_KEYCODES
				.iter()
				.all(|code| (MIN_KEYCODE..=MAX_KEYCODE).contains(code))
		);
		assert_eq!(ALLOW_KEYCODES[0], MIN_KEYCODE);
	}
}
