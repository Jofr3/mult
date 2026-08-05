//! The drawing palette: the resolved colour set a frame is drawn with, plus
//! the WCAG contrast maths that keeps text legible on a user's own palette.
//!
//! The hex parsing (and the fallback for a key that does not parse) lives in
//! `config`, which runs it once when the config is built; everything here is a
//! field-for-field copy plus arithmetic, so drawing a frame costs no parsing
//! (D9).

use ratatui::style::{Color, Modifier, Style};

use crate::config;

#[derive(Debug, Clone, Copy)]
pub(super) struct Palette {
    pub(super) nc: Color,
    pub(super) base: Color,
    pub(super) muted: Color,
    pub(super) text: Color,
    pub(super) love: Color,
    pub(super) gold: Color,
    pub(super) pine: Color,
    pub(super) foam: Color,
    pub(super) iris: Color,
    pub(super) highlight_med: Color,
    pub(super) cursor: Color,
    pub(super) success: Color,
    /// Set when `$NO_COLOR` turned every color into `Color::Reset` (E10). The
    /// few places that carry meaning in a *background* rather than in text —
    /// the sidebar selection, a prompt's selected row — switch to a reversed
    /// modifier, so the UI stays navigable without emitting a single color.
    pub(super) monochrome: bool,
}

impl Palette {
    /// The palette a frame should be drawn with, honouring `$NO_COLOR`.
    pub(super) fn from_config(config: &config::Config) -> Self {
        if config.color_enabled() {
            Self::from_colors(config.colors())
        } else {
            Self::monochrome()
        }
    }

    /// Every color replaced by the terminal's own default (`$NO_COLOR`).
    pub(super) fn monochrome() -> Self {
        Self {
            nc: Color::Reset,
            base: Color::Reset,
            muted: Color::Reset,
            text: Color::Reset,
            love: Color::Reset,
            gold: Color::Reset,
            pine: Color::Reset,
            foam: Color::Reset,
            iris: Color::Reset,
            highlight_med: Color::Reset,
            cursor: Color::Reset,
            success: Color::Reset,
            monochrome: true,
        }
    }

    /// How a selected list row is drawn. Bold-on-highlight normally; reversed
    /// under `NO_COLOR`, where a highlight background does not exist.
    pub(super) fn selection_style(self) -> Style {
        if self.monochrome {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
                .bg(self.highlight_med)
                .add_modifier(Modifier::BOLD)
        }
    }

    /// Adapt an already-resolved colorscheme to ratatui colors.
    ///
    /// The hex parsing (and the fallback for a key that does not parse) lives in
    /// `config`, which runs it once when the config is built; this is a plain
    /// field-for-field copy so drawing a frame costs no parsing (D9).
    pub(super) fn from_colors(colors: &config::ColorScheme) -> Self {
        Self {
            nc: config_color_to_ratatui(colors.nc),
            base: config_color_to_ratatui(colors.base),
            muted: config_color_to_ratatui(colors.muted),
            text: config_color_to_ratatui(colors.text),
            love: config_color_to_ratatui(colors.love),
            gold: config_color_to_ratatui(colors.gold),
            pine: config_color_to_ratatui(colors.pine),
            foam: config_color_to_ratatui(colors.foam),
            iris: config_color_to_ratatui(colors.iris),
            highlight_med: config_color_to_ratatui(colors.highlight_med),
            cursor: config_color_to_ratatui(colors.cursor),
            success: config_color_to_ratatui(colors.success),
            monochrome: false,
        }
    }
}

pub(super) fn config_color_to_ratatui(color: config::Rgb) -> Color {
    Color::Rgb(color.red, color.green, color.blue)
}

/// Relative luminance per WCAG 2.x (sRGB). Non-RGB colors are treated as dark.
pub(super) fn relative_luminance(color: Color) -> f64 {
    let Color::Rgb(red, green, blue) = color else {
        return 0.0;
    };
    fn linearize(channel: u8) -> f64 {
        let channel = f64::from(channel) / 255.0;
        if channel <= 0.039_28 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * linearize(red) + 0.7152 * linearize(green) + 0.0722 * linearize(blue)
}

/// WCAG contrast ratio between two colors (1.0 = identical, up to 21.0).
pub(super) fn contrast_ratio(a: Color, b: Color) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (high, low) = if la >= lb { (la, lb) } else { (lb, la) };
    (high + 0.05) / (low + 0.05)
}

/// Foreground for text drawn on `background`: keep `preferred` while it stays
/// legible there, otherwise fall back to black or white. This preserves the
/// default (dark) theme's exact look while staying readable on light or
/// inverted user palettes, where a fixed dark foreground would wash out.
pub(super) fn readable_fg(preferred: Color, background: Color) -> Color {
    const MIN_CONTRAST: f64 = 4.5; // WCAG AA for normal-size text
    if contrast_ratio(preferred, background) >= MIN_CONTRAST {
        preferred
    } else if relative_luminance(background) > 0.179 {
        Color::Rgb(0, 0, 0)
    } else {
        Color::Rgb(255, 255, 255)
    }
}

pub(super) fn pane_style(focused: bool, palette: Palette) -> Style {
    if focused {
        Style::default().fg(palette.text).bg(palette.base)
    } else {
        Style::default().fg(palette.text).bg(palette.nc)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::test_palette;
    use super::*;

    #[test]
    fn renderer_defaults_match_the_configured_colorscheme_defaults() {
        // The Rosé Pine Moon defaults used to exist twice — as hex strings in
        // `config.rs` and as `Color::Rgb` constants here — with nothing tying
        // them together (F20). This fails if the two ever drift apart again.
        let palette = Palette::from_colors(&config::DEFAULT_COLOR_SCHEME);
        let strings = config::ColorSchemeConfig::default();
        let (parsed, errors) = strings.resolve();

        assert!(errors.is_empty(), "the built-in defaults must all parse");
        assert_eq!(palette.nc, Color::Rgb(0x1f, 0x1d, 0x30));
        assert_eq!(strings.nc, "#1f1d30");
        assert_eq!(Palette::from_colors(&parsed).nc, palette.nc);
        assert_eq!(Palette::from_colors(&parsed).base, palette.base);
        assert_eq!(Palette::from_colors(&parsed).success, palette.success);
        assert_eq!(test_palette().highlight_med, palette.highlight_med);
    }
    #[test]
    fn no_color_replaces_every_palette_entry_with_the_terminal_default() {
        let monochrome = Palette::monochrome();

        for color in [
            monochrome.nc,
            monochrome.base,
            monochrome.text,
            monochrome.love,
            monochrome.gold,
            monochrome.highlight_med,
            monochrome.cursor,
            monochrome.success,
        ] {
            assert_eq!(color, Color::Reset);
        }
        // A selection drawn as a background would vanish along with the
        // background, so it becomes a reversed modifier instead.
        assert_eq!(
            monochrome.selection_style(),
            Style::default().add_modifier(Modifier::REVERSED)
        );
        assert_ne!(
            Palette::from_colors(&config::DEFAULT_COLOR_SCHEME).selection_style(),
            monochrome.selection_style()
        );
    }
    #[test]
    fn readable_fg_keeps_legible_preferred_but_swaps_when_washed_out() {
        let dark = Color::Rgb(31, 29, 48); // moon `nc`
        let light = Color::Rgb(156, 207, 216); // moon `foam`

        // Dark-on-light (the default selection/cursor) is legible, so the
        // preferred foreground is kept verbatim — the default theme is unchanged.
        assert_eq!(readable_fg(dark, light), dark);
        // A light foreground on a light background would wash out: flip to black.
        assert_eq!(readable_fg(light, light), Color::Rgb(0, 0, 0));
        // A dark foreground on a dark background flips to white.
        assert_eq!(readable_fg(dark, dark), Color::Rgb(255, 255, 255));
    }
    #[test]
    fn custom_cursor_and_success_colors_are_themable() {
        let mut colorscheme = config::Config::default().colorscheme;
        colorscheme.cursor = "#010203".to_string();
        colorscheme.success = "#0a0b0c".to_string();

        let (colors, errors) = colorscheme.resolve();
        let palette = Palette::from_colors(&colors);

        assert_eq!(palette.cursor, Color::Rgb(1, 2, 3));
        assert_eq!(palette.success, Color::Rgb(10, 11, 12));
        assert!(errors.is_empty());
    }
}
