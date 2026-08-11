//! Terminal palette and automatic light/dark mode selection.

use std::{env, error::Error, fmt, str::FromStr};

use nanocodex::Thinking;
use ratatui::style::Color;

const COLORFGBG_ENV: &str = "COLORFGBG";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ThemeMode {
    #[default]
    Auto,
    Light,
    Dark,
}

impl ThemeMode {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    pub(super) const fn cycle(self) -> Self {
        match self {
            Self::Auto => Self::Light,
            Self::Light => Self::Dark,
            Self::Dark => Self::Auto,
        }
    }
}

impl fmt::Display for ThemeMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ThemeMode {
    type Err = ParseThemeModeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" | "system" => Ok(Self::Auto),
            "light" => Ok(Self::Light),
            "dark" => Ok(Self::Dark),
            _ => Err(ParseThemeModeError(value.to_owned())),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ParseThemeModeError(String);

impl fmt::Display for ParseThemeModeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid theme mode {:?}; expected auto, light, or dark",
            self.0
        )
    }
}

impl Error for ParseThemeModeError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Palette {
    text: Color,
    border: Color,
    muted: Color,
    accent: Color,
    code_text: Color,
    success: Color,
    warning: Color,
    error: Color,
    code_background: Color,
    selection: Color,
    thinking_low: Color,
    thinking_medium: Color,
    thinking_high: Color,
    thinking_xhigh: Color,
    thinking_max: Color,
}

impl Palette {
    // Semantic code and effort colors are adapted from clabby/tact at
    // 4df68c820427643216d6f2d61c58af89acc27a30 (Apache-2.0).
    const fn light() -> Self {
        Self {
            text: Color::Black,
            border: Color::Gray,
            muted: Color::DarkGray,
            accent: Color::Blue,
            code_text: Color::Rgb(0x26, 0x26, 0x26),
            success: Color::Rgb(0x00, 0x68, 0x2F),
            warning: Color::Rgb(0x8A, 0x5A, 0x00),
            error: Color::Red,
            code_background: Color::Rgb(0xEE, 0xEE, 0xEE),
            selection: Color::Rgb(0xD7, 0xE8, 0xFA),
            thinking_low: Color::DarkGray,
            thinking_medium: Color::Rgb(0x00, 0x78, 0x78),
            thinking_high: Color::Rgb(0x9A, 0x67, 0x00),
            thinking_xhigh: Color::Red,
            thinking_max: Color::Magenta,
        }
    }

    const fn dark() -> Self {
        Self {
            text: Color::White,
            border: Color::DarkGray,
            muted: Color::Gray,
            accent: Color::Cyan,
            code_text: Color::Rgb(0xD7, 0xD7, 0xD7),
            success: Color::Green,
            warning: Color::Yellow,
            error: Color::Red,
            code_background: Color::Rgb(0x26, 0x26, 0x26),
            selection: Color::Indexed(8),
            thinking_low: Color::Gray,
            thinking_medium: Color::Cyan,
            thinking_high: Color::Yellow,
            thinking_xhigh: Color::Red,
            thinking_max: Color::Magenta,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Theme {
    mode: ThemeMode,
    resolved_mode: ThemeMode,
    palette: Palette,
}

impl Default for Theme {
    fn default() -> Self {
        Self::new(ThemeMode::Auto)
    }
}

// Renderers adopt the semantic getters incrementally; keep the complete palette API available.
#[allow(dead_code)]
impl Theme {
    pub(super) fn new(mode: ThemeMode) -> Self {
        let colorfgbg = env::var(COLORFGBG_ENV).ok();
        let resolved_mode = match mode {
            ThemeMode::Auto => detect_colorfgbg_mode(colorfgbg.as_deref())
                .or_else(detect_system_mode)
                .unwrap_or(ThemeMode::Dark),
            explicit => explicit,
        };
        Self::from_resolved(mode, resolved_mode)
    }

    pub(super) fn from_colorfgbg(mode: ThemeMode, colorfgbg: Option<&str>) -> Self {
        let resolved_mode = match mode {
            ThemeMode::Auto => detect_colorfgbg_mode(colorfgbg).unwrap_or(ThemeMode::Dark),
            explicit => explicit,
        };
        Self::from_resolved(mode, resolved_mode)
    }

    const fn from_resolved(mode: ThemeMode, resolved_mode: ThemeMode) -> Self {
        let palette = match resolved_mode {
            ThemeMode::Light => Palette::light(),
            ThemeMode::Auto | ThemeMode::Dark => Palette::dark(),
        };
        Self {
            mode,
            resolved_mode,
            palette,
        }
    }

    pub(super) const fn mode(&self) -> ThemeMode {
        self.mode
    }

    pub(super) const fn resolved_mode(&self) -> ThemeMode {
        self.resolved_mode
    }

    pub(super) fn set_mode(&mut self, mode: ThemeMode) -> bool {
        let replacement = Self::new(mode);
        if *self == replacement {
            return false;
        }
        *self = replacement;
        true
    }

    pub(super) fn cycle(&mut self) -> ThemeMode {
        let mode = self.mode.cycle();
        self.set_mode(mode);
        mode
    }

    pub(super) fn refresh_auto(&mut self) -> bool {
        if self.mode != ThemeMode::Auto {
            return false;
        }
        let colorfgbg = env::var(COLORFGBG_ENV).ok();
        let resolved_mode = detect_colorfgbg_mode(colorfgbg.as_deref())
            .or_else(detect_system_mode)
            .unwrap_or(ThemeMode::Dark);
        if resolved_mode == self.resolved_mode {
            return false;
        }
        *self = Self::from_resolved(ThemeMode::Auto, resolved_mode);
        true
    }

    pub(super) const fn text(&self) -> Color {
        self.palette.text
    }

    pub(super) const fn border(&self) -> Color {
        self.palette.border
    }

    pub(super) const fn muted(&self) -> Color {
        self.palette.muted
    }

    pub(super) const fn accent(&self) -> Color {
        self.palette.accent
    }

    pub(super) const fn code_text(&self) -> Color {
        self.palette.code_text
    }

    pub(super) const fn success(&self) -> Color {
        self.palette.success
    }

    pub(super) const fn warning(&self) -> Color {
        self.palette.warning
    }

    pub(super) const fn error(&self) -> Color {
        self.palette.error
    }

    pub(super) const fn code_background(&self) -> Color {
        self.palette.code_background
    }

    pub(super) const fn selection(&self) -> Color {
        self.palette.selection
    }

    pub(super) const fn thinking_low(&self) -> Color {
        self.palette.thinking_low
    }

    pub(super) const fn thinking_medium(&self) -> Color {
        self.palette.thinking_medium
    }

    pub(super) const fn thinking_high(&self) -> Color {
        self.palette.thinking_high
    }

    pub(super) const fn thinking_xhigh(&self) -> Color {
        self.palette.thinking_xhigh
    }

    pub(super) const fn thinking_max(&self) -> Color {
        self.palette.thinking_max
    }

    pub(super) const fn effort(&self, effort: Thinking) -> Color {
        match effort {
            Thinking::None => self.muted(),
            Thinking::Low => self.thinking_low(),
            Thinking::Medium => self.thinking_medium(),
            Thinking::High => self.thinking_high(),
            Thinking::Xhigh => self.thinking_xhigh(),
            Thinking::Max => self.thinking_max(),
        }
    }
}

fn detect_system_mode() -> Option<ThemeMode> {
    match dark_light::detect().ok()? {
        dark_light::Mode::Light => Some(ThemeMode::Light),
        dark_light::Mode::Dark => Some(ThemeMode::Dark),
        dark_light::Mode::Unspecified => None,
    }
}

fn detect_colorfgbg_mode(value: Option<&str>) -> Option<ThemeMode> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    let mut fields = value.rsplit([';', ':']).map(str::trim);
    let background = fields.find(|part| !part.is_empty())?.parse::<u8>().ok()?;
    let (red, green, blue) = indexed_rgb(background);
    let luminance = u32::from(red) * 299 + u32::from(green) * 587 + u32::from(blue) * 114;
    Some(if luminance >= 160_000 {
        ThemeMode::Light
    } else {
        ThemeMode::Dark
    })
}

fn indexed_rgb(index: u8) -> (u8, u8, u8) {
    const ANSI: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00),
        (0x80, 0x00, 0x00),
        (0x00, 0x80, 0x00),
        (0x80, 0x80, 0x00),
        (0x00, 0x00, 0x80),
        (0x80, 0x00, 0x80),
        (0x00, 0x80, 0x80),
        (0xC0, 0xC0, 0xC0),
        (0x80, 0x80, 0x80),
        (0xFF, 0x00, 0x00),
        (0x00, 0xFF, 0x00),
        (0xFF, 0xFF, 0x00),
        (0x00, 0x00, 0xFF),
        (0xFF, 0x00, 0xFF),
        (0x00, 0xFF, 0xFF),
        (0xFF, 0xFF, 0xFF),
    ];
    match index {
        0..=15 => ANSI[usize::from(index)],
        16..=231 => {
            let cube = index - 16;
            let component = |value: u8| if value == 0 { 0 } else { 55 + value * 40 };
            (
                component(cube / 36),
                component((cube % 36) / 6),
                component(cube % 6),
            )
        }
        232..=255 => {
            let gray = 8 + (index - 232) * 10;
            (gray, gray, gray)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use ratatui::style::Color;

    use super::{Theme, ThemeMode, detect_colorfgbg_mode};

    #[test]
    fn theme_mode_is_clap_compatible_and_cycles() {
        assert_eq!(ThemeMode::from_str("auto").unwrap(), ThemeMode::Auto);
        assert_eq!(ThemeMode::from_str("SYSTEM").unwrap(), ThemeMode::Auto);
        assert_eq!(ThemeMode::from_str("light").unwrap(), ThemeMode::Light);
        assert_eq!(ThemeMode::from_str("dark").unwrap(), ThemeMode::Dark);
        assert!(ThemeMode::from_str("sepia").is_err());
        assert_eq!(ThemeMode::Dark.to_string(), "dark");
        assert_eq!(ThemeMode::Auto.cycle(), ThemeMode::Light);
        assert_eq!(ThemeMode::Light.cycle(), ThemeMode::Dark);
        assert_eq!(ThemeMode::Dark.cycle(), ThemeMode::Auto);
    }

    #[test]
    fn invalid_theme_mode_diagnostic_preserves_the_original_input() {
        let error = ThemeMode::from_str(" Sepia ").unwrap_err();

        assert_eq!(
            error.to_string(),
            "invalid theme mode \" Sepia \"; expected auto, light, or dark"
        );
    }

    #[test]
    fn auto_uses_the_colorfgbg_background_and_falls_back_to_dark() {
        assert_eq!(detect_colorfgbg_mode(Some("15;0")), Some(ThemeMode::Dark));
        assert_eq!(detect_colorfgbg_mode(Some("0;15")), Some(ThemeMode::Light));
        assert_eq!(detect_colorfgbg_mode(Some("0:231")), Some(ThemeMode::Light));
        assert_eq!(
            detect_colorfgbg_mode(Some("15;default;0")),
            Some(ThemeMode::Dark)
        );
        assert_eq!(detect_colorfgbg_mode(Some("15;default")), None);
        assert_eq!(detect_colorfgbg_mode(Some("15;999")), None);
        assert_eq!(
            detect_colorfgbg_mode(Some(" 0; 255 ")),
            Some(ThemeMode::Light)
        );
        assert_eq!(detect_colorfgbg_mode(Some("")), None);
        assert_eq!(detect_colorfgbg_mode(Some("unknown")), None);

        let theme = Theme::from_colorfgbg(ThemeMode::Auto, None);
        assert_eq!(theme.mode(), ThemeMode::Auto);
        assert_eq!(theme.resolved_mode(), ThemeMode::Dark);
        assert_eq!(theme.code_background(), Color::Rgb(0x26, 0x26, 0x26));
    }

    #[test]
    fn explicit_modes_ignore_colorfgbg() {
        let light = Theme::from_colorfgbg(ThemeMode::Light, Some("15;0"));
        let dark = Theme::from_colorfgbg(ThemeMode::Dark, Some("0;15"));

        assert_eq!(light.resolved_mode(), ThemeMode::Light);
        assert_eq!(dark.resolved_mode(), ThemeMode::Dark);
        assert_eq!(light.text(), Color::Black);
        assert_eq!(dark.text(), Color::White);
    }

    #[test]
    fn palettes_expose_semantic_colors() {
        let light = Theme::from_colorfgbg(ThemeMode::Light, None);
        let dark = Theme::from_colorfgbg(ThemeMode::Dark, None);

        assert_eq!(light.border(), Color::Gray);
        assert_eq!(light.muted(), Color::DarkGray);
        assert_eq!(light.accent(), Color::Blue);
        assert_eq!(light.code_text(), Color::Rgb(0x26, 0x26, 0x26));
        assert_eq!(light.success(), Color::Rgb(0x00, 0x68, 0x2F));
        assert_eq!(light.warning(), Color::Rgb(0x8A, 0x5A, 0x00));
        assert_eq!(light.error(), Color::Red);
        assert_eq!(light.code_background(), Color::Rgb(0xEE, 0xEE, 0xEE));
        assert_eq!(light.selection(), Color::Rgb(0xD7, 0xE8, 0xFA));
        assert_eq!(light.thinking_low(), Color::DarkGray);
        assert_eq!(light.thinking_medium(), Color::Rgb(0x00, 0x78, 0x78));
        assert_eq!(light.thinking_high(), Color::Rgb(0x9A, 0x67, 0x00));
        assert_eq!(light.thinking_xhigh(), Color::Red);
        assert_eq!(light.thinking_max(), Color::Magenta);

        assert_eq!(dark.border(), Color::DarkGray);
        assert_eq!(dark.muted(), Color::Gray);
        assert_eq!(dark.accent(), Color::Cyan);
        assert_eq!(dark.code_text(), Color::Rgb(0xD7, 0xD7, 0xD7));
        assert_eq!(dark.success(), Color::Green);
        assert_eq!(dark.warning(), Color::Yellow);
        assert_eq!(dark.error(), Color::Red);
        assert_eq!(dark.code_background(), Color::Rgb(0x26, 0x26, 0x26));
        assert_eq!(dark.selection(), Color::Indexed(8));
        assert_eq!(dark.thinking_low(), Color::Gray);
        assert_eq!(dark.thinking_medium(), Color::Cyan);
        assert_eq!(dark.thinking_high(), Color::Yellow);
        assert_eq!(dark.thinking_xhigh(), Color::Red);
        assert_eq!(dark.thinking_max(), Color::Magenta);
    }

    #[test]
    fn effort_maps_every_nanocodex_reasoning_level() {
        use nanocodex::Thinking;

        let theme = Theme::from_colorfgbg(ThemeMode::Dark, None);

        assert_eq!(theme.effort(Thinking::None), theme.muted());
        assert_eq!(theme.effort(Thinking::Low), theme.thinking_low());
        assert_eq!(theme.effort(Thinking::Medium), theme.thinking_medium());
        assert_eq!(theme.effort(Thinking::High), theme.thinking_high());
        assert_eq!(theme.effort(Thinking::Xhigh), theme.thinking_xhigh());
        assert_eq!(theme.effort(Thinking::Max), theme.thinking_max());
    }

    #[test]
    fn theme_cycle_updates_mode_and_palette() {
        let mut theme = Theme::from_colorfgbg(ThemeMode::Auto, Some("15;0"));

        assert_eq!(theme.cycle(), ThemeMode::Light);
        assert_eq!(theme.resolved_mode(), ThemeMode::Light);
        assert_eq!(theme.cycle(), ThemeMode::Dark);
        assert_eq!(theme.resolved_mode(), ThemeMode::Dark);
    }
}
