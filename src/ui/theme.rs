//! Colour: the built-in palette, user colour parsing, the WCAG contrast maths
//! that keeps text legible over whatever background it lands on, and `NO_COLOR`.

use ratatui::style::{Color, Modifier, Style};

use crate::config::ColorSchemeConfig;

/// The built-in Rosé Pine Moon colors, derived at compile time from the very
/// hex strings [`config::DEFAULT_COLOR_SCHEME`] hands the user. There is no
/// second copy of these values to drift: a change there is a change here, and a
/// malformed entry fails the build rather than silently falling back.
mod moon {
    use ratatui::style::Color;

    use super::default_color;
    use crate::config::DEFAULT_COLOR_SCHEME as SCHEME;

    pub const NC: Color = default_color(SCHEME.nc);
    pub const BASE: Color = default_color(SCHEME.base);
    pub const MUTED: Color = default_color(SCHEME.muted);
    pub const TEXT: Color = default_color(SCHEME.text);
    pub const LOVE: Color = default_color(SCHEME.love);
    pub const GOLD: Color = default_color(SCHEME.gold);
    pub const PINE: Color = default_color(SCHEME.pine);
    pub const FOAM: Color = default_color(SCHEME.foam);
    pub const IRIS: Color = default_color(SCHEME.iris);
    pub const HIGHLIGHT_MED: Color = default_color(SCHEME.highlight_med);
    pub const CURSOR: Color = default_color(SCHEME.cursor);
    pub const SUCCESS: Color = default_color(SCHEME.success);
}

/// Const-evaluable `#rrggbb` parse, used only for the built-in defaults. User
/// input goes through [`parse_color`], which reports failure instead.
const fn default_color(hex: &str) -> Color {
    let bytes = hex.as_bytes();
    let offset = match bytes.len() {
        6 => 0,
        7 if bytes[0] == b'#' => 1,
        _ => panic!("a default colorscheme entry must be 6 hex digits, optionally `#`-prefixed"),
    };

    Color::Rgb(
        hex_byte(bytes[offset], bytes[offset + 1]),
        hex_byte(bytes[offset + 2], bytes[offset + 3]),
        hex_byte(bytes[offset + 4], bytes[offset + 5]),
    )
}

const fn hex_byte(high: u8, low: u8) -> u8 {
    hex_digit(high) * 16 + hex_digit(low)
}

const fn hex_digit(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => panic!("a default colorscheme entry contains a non-hex digit"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
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
    /// Set when `NO_COLOR` is in the environment (E10). Every colour is
    /// `Color::Reset`, so `mult` emits no SGR colour at all, and the overlays
    /// that were carrying meaning in a background colour — the sidebar
    /// selection, the palette's highlighted row, a text selection, the terminal
    /// cursor — switch to reverse video instead of a hardcoded RGB fallback.
    pub(super) monochrome: bool,
}

/// A colorscheme key whose configured value did not parse. The palette keeps
/// the built-in default for that key and hands the failure back rather than
/// swallowing it.
///
/// This is the seam a later slice reports startup configuration warnings
/// through; nothing surfaces these yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorParseIssue {
    /// The key as written in `config.json` (note `_nc`, not `nc`).
    pub key: &'static str,
    pub value: String,
}

impl Palette {
    pub(crate) fn from_colorscheme(colorscheme: &ColorSchemeConfig) -> Self {
        Self::from_colorscheme_reporting(colorscheme).0
    }

    pub(crate) fn from_colorscheme_reporting(
        colorscheme: &ColorSchemeConfig,
    ) -> (Self, Vec<ColorParseIssue>) {
        let mut issues = Vec::new();
        let mut parse = |key: &'static str, value: &str, fallback: Color| match parse_color(value) {
            Some(color) => color,
            None => {
                issues.push(ColorParseIssue {
                    key,
                    value: value.to_string(),
                });
                fallback
            }
        };

        let palette = Self {
            nc: parse("_nc", &colorscheme.nc, moon::NC),
            base: parse("base", &colorscheme.base, moon::BASE),
            muted: parse("muted", &colorscheme.muted, moon::MUTED),
            text: parse("text", &colorscheme.text, moon::TEXT),
            love: parse("love", &colorscheme.love, moon::LOVE),
            gold: parse("gold", &colorscheme.gold, moon::GOLD),
            pine: parse("pine", &colorscheme.pine, moon::PINE),
            foam: parse("foam", &colorscheme.foam, moon::FOAM),
            iris: parse("iris", &colorscheme.iris, moon::IRIS),
            highlight_med: parse(
                "highlight_med",
                &colorscheme.highlight_med,
                moon::HIGHLIGHT_MED,
            ),
            cursor: parse("cursor", &colorscheme.cursor, moon::CURSOR),
            success: parse("success", &colorscheme.success, moon::SUCCESS),
            monochrome: false,
        };

        (palette, issues)
    }

    /// The palette used when `NO_COLOR` is set: nothing but the terminal's own
    /// default foreground and background.
    pub(crate) fn monochrome() -> Self {
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

    /// A style that has to stand out from the pane around it: a selected row, a
    /// highlighted match, a cursor overlay. With colour it is `preferred` on
    /// `background`; without it, reverse video, which every terminal has.
    pub(super) fn emphasis(self, preferred: Color, background: Color) -> Style {
        if self.monochrome {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
                .fg(readable_fg(preferred, background))
                .bg(background)
        }
    }

    /// The sidebar's selected row. Background (or reverse video) only, and
    /// deliberately no foreground: each row's own status glyph has already
    /// chosen one, and overriding it would put the selected pane's state back
    /// on colour alone (E8).
    pub(super) fn selection_highlight(self) -> Style {
        if self.monochrome {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
                .bg(self.highlight_med)
                .add_modifier(Modifier::BOLD)
        }
    }

    /// Emphasis for a whole selected list row in a prompt, where the row is a
    /// single uniform piece of text.
    pub(super) fn selected_row(self) -> Style {
        if self.monochrome {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
                .fg(self.text)
                .bg(self.highlight_med)
                .add_modifier(Modifier::BOLD)
        }
    }

    /// Foreground for a semantic accent (an error, a hint, a status glyph).
    /// Without colour the glyph itself carries the meaning (E8), so the only
    /// thing left to say is "this is louder than body text".
    pub(super) fn accent(self, color: Color, emphatic: bool) -> Style {
        if self.monochrome {
            if emphatic {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            }
        } else {
            Style::default().fg(color)
        }
    }
}

/// Whether `NO_COLOR` is set to a non-empty value.
///
/// Read once: the environment cannot change under a running process in any way
/// this should react to, and `draw` runs on every frame. Tests drive
/// [`draw_with_palette`] with [`Palette::monochrome`] directly rather than
/// mutating a process global.
pub(super) fn no_color_is_set() -> bool {
    static NO_COLOR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *NO_COLOR.get_or_init(|| {
        std::env::var_os("NO_COLOR").is_some_and(|value| !value.as_encoded_bytes().is_empty())
    })
}

fn parse_color(input: &str) -> Option<Color> {
    let hex = input.trim().strip_prefix('#').unwrap_or(input.trim());
    if hex.len() != 6 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }

    let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(red, green, blue))
}

/// Relative luminance per WCAG 2.x (sRGB). Non-RGB colors are treated as dark.
fn relative_luminance(color: Color) -> f64 {
    let Color::Rgb(red, green, blue) = color else {
        return 0.0;
    };
    pub(super) fn linearize(channel: u8) -> f64 {
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
fn contrast_ratio(a: Color, b: Color) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (high, low) = if la >= lb { (la, lb) } else { (lb, la) };
    (high + 0.05) / (low + 0.05)
}

/// Foreground for text drawn on `background`: keep `preferred` while it stays
/// legible there, otherwise fall back to black or white. This preserves the
/// default (dark) theme's exact look while staying readable on light or
/// inverted user palettes, where a fixed dark foreground would wash out.
fn readable_fg(preferred: Color, background: Color) -> Color {
    const MIN_CONTRAST: f64 = 4.5; // WCAG AA for normal-size text
    if contrast_ratio(preferred, background) >= MIN_CONTRAST {
        preferred
    } else if relative_luminance(background) > 0.179 {
        Color::Rgb(0, 0, 0)
    } else {
        Color::Rgb(255, 255, 255)
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::app::App;
    use crate::app::NavItem;

    use crate::config;

    use crate::pty::PtyRuntime;
    use crate::ui::test_support::*;

    #[test]
    pub(super) fn no_color_emits_no_color_at_all_and_keeps_overlays_distinguishable() {
        let mut app = App::default();
        let selected = app
            .nav_items()
            .iter()
            .position(|item| matches!(item, NavItem::Terminal { .. }))
            .expect("seed state has a terminal");
        app.select_nav_index(selected);

        let buffer = render_buffer_with_palette(
            &app,
            &PtyRuntime::new_offline(),
            &config::Config::default(),
            Palette::monochrome(),
            100,
            30,
        );

        // Not one truecolor or indexed escape: with `NO_COLOR` the terminal's
        // own defaults are the only colours used.
        for y in buffer.area().top()..buffer.area().bottom() {
            for x in buffer.area().left()..buffer.area().right() {
                let cell = buffer.cell((x, y)).expect("cell is in bounds");
                assert_eq!(cell.fg, Color::Reset, "cell ({x},{y}) painted a foreground");
                assert_eq!(cell.bg, Color::Reset, "cell ({x},{y}) painted a background");
            }
        }

        // The selected sidebar row is still marked — by an attribute, not by a
        // hardcoded RGB fallback, which is the trap E10 exists to avoid.
        let reversed_rows = (buffer.area().top()..buffer.area().bottom())
            .filter(|y| {
                buffer
                    .cell((0, *y))
                    .is_some_and(|cell| cell.modifier.contains(Modifier::REVERSED))
            })
            .count();
        assert_eq!(
            reversed_rows, 1,
            "exactly the selected sidebar row must be reverse video"
        );
    }

    #[test]
    pub(super) fn readable_fg_keeps_legible_preferred_but_swaps_when_washed_out() {
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
    pub(super) fn custom_cursor_and_success_colors_are_themable() {
        let colorscheme = colorscheme_with(|colorscheme| {
            colorscheme.cursor = "#010203".to_string();
            colorscheme.success = "#0a0b0c".to_string();
        });

        let palette = Palette::from_colorscheme(&colorscheme);

        assert_eq!(palette.cursor, Color::Rgb(1, 2, 3));
        assert_eq!(palette.success, Color::Rgb(10, 11, 12));
    }

    #[test]
    pub(super) fn built_in_palette_matches_the_default_colorscheme_strings() {
        // The two representations of Rosé Pine Moon — the hex strings the
        // config layer hands users and the `Color`s the renderer falls back to
        // — are derived from one constant; this fails if a future edit
        // reintroduces a second copy of either.
        let from_strings = Palette::from_colorscheme(&ColorSchemeConfig::default());

        assert_eq!(
            from_strings,
            Palette {
                nc: moon::NC,
                base: moon::BASE,
                muted: moon::MUTED,
                text: moon::TEXT,
                love: moon::LOVE,
                gold: moon::GOLD,
                pine: moon::PINE,
                foam: moon::FOAM,
                iris: moon::IRIS,
                highlight_med: moon::HIGHLIGHT_MED,
                cursor: moon::CURSOR,
                success: moon::SUCCESS,
                monochrome: false,
            }
        );
        // ...and every default parses, so no key is silently on a fallback.
        assert_eq!(
            Palette::from_colorscheme_reporting(&ColorSchemeConfig::default()).1,
            Vec::new()
        );
    }

    #[test]
    pub(super) fn unparseable_colors_keep_the_default_and_are_reported_per_key() {
        let colorscheme = colorscheme_with(|colorscheme| {
            colorscheme.nc = "not-a-color".to_string();
            colorscheme.gold = "#12345".to_string();
        });

        let (palette, issues) = Palette::from_colorscheme_reporting(&colorscheme);

        assert_eq!(palette.nc, moon::NC);
        assert_eq!(palette.gold, moon::GOLD);
        assert_eq!(
            issues,
            vec![
                ColorParseIssue {
                    key: "_nc",
                    value: "not-a-color".to_string(),
                },
                ColorParseIssue {
                    key: "gold",
                    value: "#12345".to_string(),
                },
            ]
        );
    }
}
