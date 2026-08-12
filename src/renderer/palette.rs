//! Color Palette / Theme
//!
//! Console-mode rendering can't control the *font* (see module docs on
//! why zoom/font-picker aren't meaningful here), but it absolutely can
//! control *color* -- we already own every RGB value we send to the
//! terminal via SGR truecolor escapes. This module maps the "theme" the
//! user picks (dark/light) onto a concrete 16-color ANSI palette and a
//! pair of default foreground/background colors, independent of whatever
//! color scheme the *host* terminal emulator happens to be configured
//! with -- so HyperTerm's own idea of "dark" or "light" is consistent
//! regardless of the user's Windows Terminal/console profile.
//!
//! Only `Color::Indexed(0..=15)` (the standard 16 ANSI colors) and
//! `Color::Default` are theme-dependent. `Color::Indexed(16..=255)` (the
//! xterm 256-color cube) and `Color::Rgb` are passed through unchanged --
//! matches how real terminal theme switching works (256-color/truecolor
//! output is the application's explicit choice, not something a theme
//! should override).

use crate::virtual_buffer::Color as VColor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

pub struct Palette {
    ansi16: [Rgb; 16],
    default_fg: Rgb,
    default_bg: Rgb,
}

impl Palette {
    pub fn for_theme(theme: crate::config::Theme) -> Self {
        match theme {
            crate::config::Theme::Dark => Self::dark(),
            crate::config::Theme::Light => Self::light(),
        }
    }

    /// A standard, high-contrast dark palette (loosely "One Dark"-ish,
    /// chosen for legibility rather than matching any one specific
    /// popular scheme exactly).
    pub fn dark() -> Self {
        Self {
            ansi16: [
                Rgb(0x28, 0x2c, 0x34), // 0 black
                Rgb(0xe0, 0x6c, 0x75), // 1 red
                Rgb(0x98, 0xc3, 0x79), // 2 green
                Rgb(0xe5, 0xc0, 0x7b), // 3 yellow
                Rgb(0x61, 0xaf, 0xef), // 4 blue
                Rgb(0xc6, 0x78, 0xdd), // 5 magenta
                Rgb(0x56, 0xb6, 0xc2), // 6 cyan
                Rgb(0xab, 0xb2, 0xbf), // 7 white
                Rgb(0x5c, 0x63, 0x70), // 8 bright black
                Rgb(0xe0, 0x6c, 0x75), // 9 bright red
                Rgb(0x98, 0xc3, 0x79), // 10 bright green
                Rgb(0xe5, 0xc0, 0x7b), // 11 bright yellow
                Rgb(0x61, 0xaf, 0xef), // 12 bright blue
                Rgb(0xc6, 0x78, 0xdd), // 13 bright magenta
                Rgb(0x56, 0xb6, 0xc2), // 14 bright cyan
                Rgb(0xff, 0xff, 0xff), // 15 bright white
            ],
            default_fg: Rgb(0xab, 0xb2, 0xbf),
            default_bg: Rgb(0x1e, 0x22, 0x27),
        }
    }

    /// A high-contrast light palette. ANSI colors are darkened relative
    /// to the dark palette so text stays legible against a light
    /// background (the naive approach of reusing the same 16 colors on a
    /// white background makes yellow/cyan nearly unreadable, which is a
    /// real, common complaint about lazy light-theme implementations).
    pub fn light() -> Self {
        Self {
            ansi16: [
                Rgb(0xfa, 0xfa, 0xfa), // 0 black (shown light so it's visible on a light bg)
                Rgb(0xca, 0x1f, 0x2e), // 1 red
                Rgb(0x22, 0x7a, 0x37), // 2 green
                Rgb(0x8a, 0x6a, 0x00), // 3 yellow (darkened for contrast)
                Rgb(0x1c, 0x5f, 0xc4), // 4 blue
                Rgb(0x8f, 0x3f, 0xb8), // 5 magenta
                Rgb(0x14, 0x7d, 0x87), // 6 cyan (darkened for contrast)
                Rgb(0x38, 0x3a, 0x42), // 7 white (shown dark)
                Rgb(0x69, 0x6c, 0x77), // 8 bright black
                Rgb(0xca, 0x1f, 0x2e), // 9 bright red
                Rgb(0x22, 0x7a, 0x37), // 10 bright green
                Rgb(0x8a, 0x6a, 0x00), // 11 bright yellow
                Rgb(0x1c, 0x5f, 0xc4), // 12 bright blue
                Rgb(0x8f, 0x3f, 0xb8), // 13 bright magenta
                Rgb(0x14, 0x7d, 0x87), // 14 bright cyan
                Rgb(0x1a, 0x1a, 0x1a), // 15 bright white (shown near-black)
            ],
            default_fg: Rgb(0x38, 0x3a, 0x42),
            default_bg: Rgb(0xfa, 0xfa, 0xfa),
        }
    }

    /// Resolves a cell's logical color to a concrete RGB triple for
    /// this palette. `is_fg` picks which default applies for
    /// `Color::Default`.
    pub fn resolve(&self, color: VColor, is_fg: bool) -> Rgb {
        match color {
            VColor::Default => {
                if is_fg {
                    self.default_fg
                } else {
                    self.default_bg
                }
            }
            VColor::Indexed(i) if i < 16 => self.ansi16[i as usize],
            VColor::Indexed(_) | VColor::Rgb(_, _, _) => raw_passthrough(color),
        }
    }
}

/// For colors the theme doesn't touch (256-cube indices, explicit
/// truecolor), resolves to the same RGB xterm itself would use for the
/// index, or the literal RGB given.
fn raw_passthrough(color: VColor) -> Rgb {
    match color {
        VColor::Rgb(r, g, b) => Rgb(r, g, b),
        VColor::Indexed(i) => xterm_256_to_rgb(i),
        VColor::Default => Rgb(0, 0, 0), // unreachable via call sites above
    }
}

/// Standard xterm 256-color cube math (indices 16-231 = 6x6x6 color cube,
/// 232-255 = grayscale ramp), matching what every terminal emulator uses.
fn xterm_256_to_rgb(i: u8) -> Rgb {
    if i < 16 {
        // Shouldn't be reached (handled by the theme palette above), but
        // provide a sane fallback rather than panicking if it ever is.
        return Rgb(0, 0, 0);
    }
    if i >= 232 {
        let level = 8 + (i - 232) * 10;
        return Rgb(level, level, level);
    }
    let i = i - 16;
    let r = i / 36;
    let g = (i % 36) / 6;
    let b = i % 6;
    let scale = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
    Rgb(scale(r), scale(g), scale(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_fg_bg_differ_between_themes() {
        let dark = Palette::dark();
        let light = Palette::light();
        assert_ne!(
            dark.resolve(VColor::Default, true),
            light.resolve(VColor::Default, true)
        );
        assert_ne!(
            dark.resolve(VColor::Default, false),
            light.resolve(VColor::Default, false)
        );
    }

    #[test]
    fn light_theme_bg_is_lighter_than_dark_theme_bg() {
        let dark = Palette::dark();
        let light = Palette::light();
        let Rgb(dr, dg, db) = dark.resolve(VColor::Default, false);
        let Rgb(lr, lg, lb) = light.resolve(VColor::Default, false);
        let dark_luma = dr as u32 + dg as u32 + db as u32;
        let light_luma = lr as u32 + lg as u32 + lb as u32;
        assert!(
            light_luma > dark_luma,
            "light theme background should be brighter than dark theme's"
        );
    }

    #[test]
    fn indexed_256_cube_is_theme_independent() {
        let dark = Palette::dark();
        let light = Palette::light();
        assert_eq!(
            dark.resolve(VColor::Indexed(196), true),
            light.resolve(VColor::Indexed(196), true)
        );
    }

    #[test]
    fn explicit_rgb_passes_through_unchanged_in_both_themes() {
        let dark = Palette::dark();
        let light = Palette::light();
        let c = VColor::Rgb(12, 34, 56);
        assert_eq!(dark.resolve(c, true), Rgb(12, 34, 56));
        assert_eq!(light.resolve(c, true), Rgb(12, 34, 56));
    }

    #[test]
    fn ansi_16_colors_differ_by_theme() {
        let dark = Palette::dark();
        let light = Palette::light();
        for i in 0..16u8 {
            assert_ne!(
                dark.resolve(VColor::Indexed(i), true),
                light.resolve(VColor::Indexed(i), true),
                "index {i} should differ between themes"
            );
        }
    }

    #[test]
    fn xterm_grayscale_ramp_is_monotonic() {
        let mut last = 0u8;
        for i in 232..=255u8 {
            let Rgb(r, _, _) = xterm_256_to_rgb(i);
            assert!(r >= last, "grayscale ramp should be non-decreasing");
            last = r;
        }
    }
}
