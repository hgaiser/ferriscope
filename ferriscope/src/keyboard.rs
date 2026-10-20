//! The seat keyboard's XKB configuration.
//!
//! gamescope builds the keymap from `xkb_rule_names` taken from the
//! `XKB_DEFAULT_*` environment, with the layout and variant replaced by its
//! `xkb_layout` setting (`layout[:variant]`) when that is non-empty
//! (`wlserver_update_keymap`, `wlserver.cpp:2108-2140`). Steam pushes that
//! setting through `GAMESCOPE_KEYBOARD_LAYOUT` (`steamcompmgr.cpp:7364`).
//! [`KeyboardConfig`] takes the place of the environment as the base: an empty
//! field is left to xkbcommon, which then reads the matching `XKB_DEFAULT_*`
//! variable itself.

/// XKB rule names for the seat keyboard. An empty field defers to xkbcommon's
/// default for it (the matching `XKB_DEFAULT_*` environment variable, then the
/// built-in default).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyboardConfig {
	/// The rules file (`XKB_DEFAULT_RULES`).
	pub rules: String,
	/// The keyboard model (`XKB_DEFAULT_MODEL`).
	pub model: String,
	/// Comma-separated layouts (`XKB_DEFAULT_LAYOUT`).
	pub layout: String,
	/// Comma-separated variants, one per layout (`XKB_DEFAULT_VARIANT`).
	pub variant: String,
	/// Comma-separated options (`XKB_DEFAULT_OPTIONS`); empty for none.
	pub options: String,
}

impl KeyboardConfig {
	/// This configuration with Steam's `layout[:variant]` applied.
	///
	/// A non-empty `steam_layout` replaces both the layout and the variant, as
	/// the variant of the base "belongs to the layout it shipped with"
	/// (`wlserver.cpp:2125-2128`); an empty one leaves the base untouched.
	pub fn with_steam_layout(&self, steam_layout: &str) -> Self {
		let mut config = self.clone();
		if steam_layout.is_empty() {
			return config;
		}
		let (layout, variant) = steam_layout.split_once(':').unwrap_or((steam_layout, ""));
		config.layout = layout.to_string();
		config.variant = variant.to_string();
		config
	}

	/// The smithay configuration for this keyboard.
	pub(crate) fn xkb_config(&self) -> smithay::input::keyboard::XkbConfig<'_> {
		smithay::input::keyboard::XkbConfig {
			rules: &self.rules,
			model: &self.model,
			layout: &self.layout,
			variant: &self.variant,
			options: (!self.options.is_empty()).then(|| self.options.clone()),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn base() -> KeyboardConfig {
		KeyboardConfig {
			layout: "us".into(),
			variant: "intl".into(),
			model: "pc105".into(),
			options: "ctrl:nocaps".into(),
			..KeyboardConfig::default()
		}
	}

	#[test]
	fn an_empty_steam_layout_keeps_the_base() {
		assert_eq!(base().with_steam_layout(""), base());
	}

	#[test]
	fn a_steam_layout_replaces_layout_and_variant_only() {
		let config = base().with_steam_layout("fr:bepo");
		assert_eq!((config.layout.as_str(), config.variant.as_str()), ("fr", "bepo"));
		assert_eq!(config.model, "pc105");
		assert_eq!(config.options, "ctrl:nocaps");
	}

	#[test]
	fn a_steam_layout_without_variant_drops_the_base_variant() {
		let config = base().with_steam_layout("es");
		assert_eq!((config.layout.as_str(), config.variant.as_str()), ("es", ""));
	}

	#[test]
	fn empty_options_are_none() {
		assert_eq!(KeyboardConfig::default().xkb_config().options, None);
		assert_eq!(base().xkb_config().options.as_deref(), Some("ctrl:nocaps"));
	}
}
