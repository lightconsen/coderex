//! Theme system — configurable terminal UI color themes.
//!
//! Themes define the color palette for all UI elements: background, foreground,
//! accents, status bar, and syntax highlighting. Themes can be loaded from JSON
//! config files and switched at runtime.

use ratatui::style::Color;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

/// A complete UI theme configuration.
#[derive(Debug, Clone)]
pub struct Theme {
    /// Human-readable name of the theme.
    pub name: String,
    /// Background color.
    pub background: Color,
    /// Primary foreground (text) color.
    pub foreground: Color,
    /// Dim/subtle text color.
    pub dim: Color,
    /// Primary accent color for highlights and active elements.
    pub accent: Color,
    /// Secondary accent color.
    pub accent_secondary: Color,
    /// User prompt marker/title color.
    pub user: Color,
    /// Assistant marker/title color.
    pub assistant: Color,
    /// Tool call/result marker color.
    pub tool: Color,
    /// Status bar background.
    pub statusbar_bg: Color,
    /// Status bar text.
    pub statusbar_fg: Color,
    /// Error color.
    pub error: Color,
    /// Warning color.
    pub warning: Color,
    /// Success color.
    pub success: Color,
    /// Border color.
    pub border: Color,
    /// Selection background.
    pub selection_bg: Color,
    /// Selection foreground.
    pub selection_fg: Color,
    /// Input area background.
    pub input_bg: Color,
    /// Input area text.
    pub input_fg: Color,
    /// Overlay/modal background.
    pub overlay_bg: Color,
    /// Overlay/modal border.
    pub overlay_border: Color,
    /// Vim normal mode indicator color.
    pub vim_normal: Color,
    /// Vim insert mode indicator color.
    pub vim_insert: Color,
    /// Notification info color.
    pub notif_info: Color,
    /// Notification warning color.
    pub notif_warning: Color,
    /// Notification error color.
    pub notif_error: Color,
}

/// Internal struct for JSON serialization of themes.
#[derive(serde::Serialize, serde::Deserialize)]
struct ThemeData {
    name: String,
    background: String,
    foreground: String,
    dim: String,
    accent: String,
    accent_secondary: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    assistant: Option<String>,
    #[serde(default)]
    tool: Option<String>,
    statusbar_bg: String,
    statusbar_fg: String,
    error: String,
    warning: String,
    success: String,
    border: String,
    selection_bg: String,
    selection_fg: String,
    input_bg: String,
    input_fg: String,
    overlay_bg: String,
    overlay_border: String,
    vim_normal: String,
    vim_insert: String,
    notif_info: String,
    notif_warning: String,
    notif_error: String,
}

impl From<&Theme> for ThemeData {
    fn from(t: &Theme) -> Self {
        Self {
            name: t.name.clone(),
            background: color_to_string(t.background),
            foreground: color_to_string(t.foreground),
            dim: color_to_string(t.dim),
            accent: color_to_string(t.accent),
            accent_secondary: color_to_string(t.accent_secondary),
            user: Some(color_to_string(t.user)),
            assistant: Some(color_to_string(t.assistant)),
            tool: Some(color_to_string(t.tool)),
            statusbar_bg: color_to_string(t.statusbar_bg),
            statusbar_fg: color_to_string(t.statusbar_fg),
            error: color_to_string(t.error),
            warning: color_to_string(t.warning),
            success: color_to_string(t.success),
            border: color_to_string(t.border),
            selection_bg: color_to_string(t.selection_bg),
            selection_fg: color_to_string(t.selection_fg),
            input_bg: color_to_string(t.input_bg),
            input_fg: color_to_string(t.input_fg),
            overlay_bg: color_to_string(t.overlay_bg),
            overlay_border: color_to_string(t.overlay_border),
            vim_normal: color_to_string(t.vim_normal),
            vim_insert: color_to_string(t.vim_insert),
            notif_info: color_to_string(t.notif_info),
            notif_warning: color_to_string(t.notif_warning),
            notif_error: color_to_string(t.notif_error),
        }
    }
}

impl TryFrom<ThemeData> for Theme {
    type Error = String;
    fn try_from(d: ThemeData) -> Result<Self, Self::Error> {
        Ok(Self {
            name: d.name,
            background: parse_color_or(&d.background, Color::Reset, "background")?,
            foreground: parse_color_or(&d.foreground, Color::Reset, "foreground")?,
            dim: parse_color_or(&d.dim, Color::DarkGray, "dim")?,
            accent: parse_color_or(&d.accent, Color::Blue, "accent")?,
            accent_secondary: parse_color_or(&d.accent_secondary, Color::Cyan, "accent_secondary")?,
            user: parse_optional_color_or(d.user.as_ref(), Color::Blue, "user")?,
            assistant: parse_optional_color_or(d.assistant.as_ref(), Color::Green, "assistant")?,
            tool: parse_optional_color_or(d.tool.as_ref(), Color::Magenta, "tool")?,
            statusbar_bg: parse_color_or(&d.statusbar_bg, Color::DarkGray, "statusbar_bg")?,
            statusbar_fg: parse_color_or(&d.statusbar_fg, Color::White, "statusbar_fg")?,
            error: parse_color_or(&d.error, Color::Red, "error")?,
            warning: parse_color_or(&d.warning, Color::Yellow, "warning")?,
            success: parse_color_or(&d.success, Color::Green, "success")?,
            border: parse_color_or(&d.border, Color::DarkGray, "border")?,
            selection_bg: parse_color_or(&d.selection_bg, Color::DarkGray, "selection_bg")?,
            selection_fg: parse_color_or(&d.selection_fg, Color::White, "selection_fg")?,
            input_bg: parse_color_or(&d.input_bg, Color::Reset, "input_bg")?,
            input_fg: parse_color_or(&d.input_fg, Color::Reset, "input_fg")?,
            overlay_bg: parse_color_or(&d.overlay_bg, Color::Black, "overlay_bg")?,
            overlay_border: parse_color_or(&d.overlay_border, Color::Gray, "overlay_border")?,
            vim_normal: parse_color_or(&d.vim_normal, Color::Cyan, "vim_normal")?,
            vim_insert: parse_color_or(&d.vim_insert, Color::Green, "vim_insert")?,
            notif_info: parse_color_or(&d.notif_info, Color::Blue, "notif_info")?,
            notif_warning: parse_color_or(&d.notif_warning, Color::Yellow, "notif_warning")?,
            notif_error: parse_color_or(&d.notif_error, Color::Red, "notif_error")?,
        })
    }
}

fn color_to_string(c: Color) -> String {
    match c {
        Color::Black => "black".into(),
        Color::Red => "red".into(),
        Color::Green => "green".into(),
        Color::Yellow => "yellow".into(),
        Color::Blue => "blue".into(),
        Color::Magenta => "magenta".into(),
        Color::Cyan => "cyan".into(),
        Color::Gray => "gray".into(),
        Color::DarkGray => "darkgray".into(),
        Color::White => "white".into(),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(i) => format!("rgb_{i}"),
        _ => "reset".into(),
    }
}

/// Whether the terminal advertises 24-bit colour.
///
/// macOS Terminal.app reports 256 colours and nothing else; sending it
/// `48;2;r;g;b` makes its background rendering unreliable. Every other
/// terminal keeps the existing 24-bit behaviour unless it explicitly says
/// otherwise.
#[must_use]
pub fn supports_truecolor() -> bool {
    truecolor_for(
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        std::env::var("COLORTERM").ok().as_deref(),
        std::env::var("TERM").ok().as_deref(),
        std::env::var_os("WT_SESSION").is_some(),
    )
}

fn truecolor_for(
    term_program: Option<&str>,
    colorterm: Option<&str>,
    term: Option<&str>,
    wt_session: bool,
) -> bool {
    if colorterm.is_some_and(|value| {
        value.eq_ignore_ascii_case("truecolor") || value.eq_ignore_ascii_case("24bit")
    }) {
        return true;
    }
    if term.is_some_and(|value| value.contains("direct")) {
        return true;
    }
    if wt_session {
        return true;
    }
    if term_program.is_some_and(|value| value == "Apple_Terminal") {
        return false;
    }
    // Unknown terminals have received 24-bit sequences so far; keep that.
    true
}

/// The xterm-256 cube levels, used to approximate an RGB colour.
const XTERM_CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

fn nearest_cube_level(value: u8) -> usize {
    XTERM_CUBE
        .iter()
        .enumerate()
        .min_by_key(|(_, level)| u32::from(**level).abs_diff(u32::from(value)))
        .map_or(0, |(index, _)| index)
}

/// The nearest xterm-256 palette index for an RGB colour.
fn nearest_xterm256(r: u8, g: u8, b: u8) -> u8 {
    let ri = nearest_cube_level(r);
    let gi = nearest_cube_level(g);
    let bi = nearest_cube_level(b);
    let cube = (XTERM_CUBE[ri], XTERM_CUBE[gi], XTERM_CUBE[bi]);
    let distance = |(cr, cg, cb): (u8, u8, u8)| {
        let d = |a: u8, b: u8| u32::from(a).abs_diff(u32::from(b)).pow(2);
        d(cr, r) + d(cg, g) + d(cb, b)
    };
    let cube_index = 16 + 36 * ri as u8 + 6 * gi as u8 + bi as u8;

    // The 24-step grey ramp is often closer than the cube for muted colours.
    let average = (u32::from(r) + u32::from(g) + u32::from(b)) / 3;
    let grey_step = average.saturating_sub(8).div_ceil(10).min(23);
    let grey_index = 232 + grey_step as u8;
    let grey_level = 8 + 10 * grey_step as u8;

    if distance((grey_level, grey_level, grey_level)) < distance(cube) {
        grey_index
    } else {
        cube_index
    }
}

/// Map a 24-bit colour to the nearest xterm-256 entry.
///
/// Colours that are already palette entries pass through unchanged.
#[must_use]
pub fn quantize_color(color: Color) -> Color {
    match color {
        Color::Rgb(r, g, b) => Color::Indexed(nearest_xterm256(r, g, b)),
        other => other,
    }
}

fn parse_color_or(s: &str, default: Color, _field: &str) -> Result<Color, String> {
    parse_color(s)
        .ok_or_else(|| format!("invalid color: {s}"))
        .or(Ok(default))
}

fn parse_optional_color_or(
    s: Option<&String>,
    default: Color,
    field: &str,
) -> Result<Color, String> {
    s.map_or(Ok(default), |value| parse_color_or(value, default, field))
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

impl Theme {
    /// The default dark theme (terminal native).
    #[must_use]
    pub fn dark() -> Self {
        Self {
            name: "dark".into(),
            background: Color::Rgb(24, 24, 27),
            foreground: Color::Rgb(229, 231, 235),
            dim: Color::DarkGray,
            accent: Color::Rgb(96, 165, 250),
            accent_secondary: Color::Rgb(45, 212, 191),
            user: Color::Rgb(96, 165, 250),
            assistant: Color::Rgb(74, 222, 128),
            tool: Color::Rgb(45, 212, 191),
            statusbar_bg: Color::Reset,
            statusbar_fg: Color::Rgb(229, 231, 235),
            error: Color::Rgb(248, 113, 113),
            warning: Color::Rgb(251, 191, 36),
            success: Color::Rgb(34, 197, 94),
            border: Color::Rgb(75, 85, 99),
            selection_bg: Color::Rgb(31, 41, 55),
            selection_fg: Color::Rgb(229, 231, 235),
            input_bg: Color::Reset,
            input_fg: Color::Rgb(229, 231, 235),
            overlay_bg: Color::Reset,
            overlay_border: Color::Rgb(75, 85, 99),
            vim_normal: Color::Rgb(45, 212, 191),
            vim_insert: Color::Rgb(74, 222, 128),
            notif_info: Color::Rgb(96, 165, 250),
            notif_warning: Color::Rgb(251, 191, 36),
            notif_error: Color::Rgb(248, 113, 113),
        }
    }

    /// A light theme suitable for bright terminals.
    #[must_use]
    pub fn light() -> Self {
        Self {
            name: "light".into(),
            background: Color::Rgb(255, 255, 255),
            foreground: Color::Rgb(17, 24, 39),
            dim: Color::Rgb(107, 114, 128),
            accent: Color::Rgb(37, 99, 235),
            accent_secondary: Color::Rgb(13, 148, 136),
            user: Color::Rgb(37, 99, 235),
            assistant: Color::Rgb(22, 163, 74),
            tool: Color::Rgb(13, 148, 136),
            statusbar_bg: Color::Reset,
            statusbar_fg: Color::Rgb(17, 24, 39),
            error: Color::Rgb(220, 38, 38),
            warning: Color::Rgb(180, 83, 9),
            success: Color::Rgb(22, 163, 74),
            border: Color::Rgb(209, 213, 219),
            selection_bg: Color::Rgb(243, 244, 246),
            selection_fg: Color::Rgb(17, 24, 39),
            input_bg: Color::Reset,
            input_fg: Color::Rgb(17, 24, 39),
            overlay_bg: Color::Reset,
            overlay_border: Color::Rgb(209, 213, 219),
            vim_normal: Color::Rgb(37, 99, 235),
            vim_insert: Color::Rgb(22, 163, 74),
            notif_info: Color::Rgb(37, 99, 235),
            notif_warning: Color::Rgb(180, 83, 9),
            notif_error: Color::Rgb(220, 38, 38),
        }
    }

    /// A high-contrast dark theme for accessibility.
    #[must_use]
    pub fn high_contrast() -> Self {
        Self {
            name: "high-contrast".into(),
            background: Color::Black,
            foreground: Color::White,
            dim: Color::Gray,
            accent: Color::Yellow,
            accent_secondary: Color::Cyan,
            user: Color::Yellow,
            assistant: Color::Green,
            tool: Color::Cyan,
            statusbar_bg: Color::Black,
            statusbar_fg: Color::Yellow,
            error: Color::Red,
            warning: Color::Yellow,
            success: Color::Green,
            border: Color::White,
            selection_bg: Color::Yellow,
            selection_fg: Color::Black,
            input_bg: Color::Black,
            input_fg: Color::White,
            overlay_bg: Color::Black,
            overlay_border: Color::Yellow,
            vim_normal: Color::Yellow,
            vim_insert: Color::Green,
            notif_info: Color::Cyan,
            notif_warning: Color::Yellow,
            notif_error: Color::Red,
        }
    }

    /// A theme inspired by Tokyo Night.
    #[must_use]
    pub fn tokyo_night() -> Self {
        Self {
            name: "tokyo-night".into(),
            background: Color::Rgb(30, 30, 46),
            foreground: Color::Rgb(192, 202, 245),
            dim: Color::Rgb(86, 95, 137),
            accent: Color::Rgb(122, 162, 247),
            accent_secondary: Color::Rgb(125, 207, 255),
            user: Color::Rgb(122, 162, 247),
            assistant: Color::Rgb(158, 206, 106),
            tool: Color::Rgb(125, 207, 255),
            statusbar_bg: Color::Rgb(46, 46, 76),
            statusbar_fg: Color::Rgb(192, 202, 245),
            error: Color::Rgb(247, 118, 142),
            warning: Color::Rgb(224, 175, 104),
            success: Color::Rgb(158, 206, 106),
            border: Color::Rgb(65, 72, 104),
            selection_bg: Color::Rgb(55, 56, 94),
            selection_fg: Color::Rgb(192, 202, 245),
            input_bg: Color::Rgb(26, 27, 38),
            input_fg: Color::Rgb(192, 202, 245),
            overlay_bg: Color::Rgb(26, 27, 38),
            overlay_border: Color::Rgb(122, 162, 247),
            vim_normal: Color::Rgb(125, 207, 255),
            vim_insert: Color::Rgb(158, 206, 106),
            notif_info: Color::Rgb(122, 162, 247),
            notif_warning: Color::Rgb(224, 175, 104),
            notif_error: Color::Rgb(247, 118, 142),
        }
    }

    /// Replace every 24-bit colour with its nearest xterm-256 entry.
    ///
    /// Used for terminals that do not support 24-bit SGR (macOS
    /// Terminal.app). Names, layout and every non-colour property are kept.
    #[must_use]
    pub fn quantized(&self) -> Self {
        Self {
            name: self.name.clone(),
            background: quantize_color(self.background),
            foreground: quantize_color(self.foreground),
            dim: quantize_color(self.dim),
            accent: quantize_color(self.accent),
            accent_secondary: quantize_color(self.accent_secondary),
            user: quantize_color(self.user),
            assistant: quantize_color(self.assistant),
            tool: quantize_color(self.tool),
            statusbar_bg: quantize_color(self.statusbar_bg),
            statusbar_fg: quantize_color(self.statusbar_fg),
            error: quantize_color(self.error),
            warning: quantize_color(self.warning),
            success: quantize_color(self.success),
            border: quantize_color(self.border),
            selection_bg: quantize_color(self.selection_bg),
            selection_fg: quantize_color(self.selection_fg),
            input_bg: quantize_color(self.input_bg),
            input_fg: quantize_color(self.input_fg),
            overlay_bg: quantize_color(self.overlay_bg),
            overlay_border: quantize_color(self.overlay_border),
            vim_normal: quantize_color(self.vim_normal),
            vim_insert: quantize_color(self.vim_insert),
            notif_info: quantize_color(self.notif_info),
            notif_warning: quantize_color(self.notif_warning),
            notif_error: quantize_color(self.notif_error),
        }
    }

    /// Parse a theme from JSON string.
    pub fn from_json(json: &str) -> Result<Self, String> {
        let data: ThemeData = serde_json::from_str(json).map_err(|e| e.to_string())?;
        Self::try_from(data)
    }

    /// Serialize the theme to JSON string.
    pub fn to_json(&self) -> Result<String, String> {
        let data = ThemeData::from(self);
        serde_json::to_string_pretty(&data).map_err(|e| e.to_string())
    }

    /// Stable signature used to invalidate cached rendered TUI lines.
    #[must_use]
    pub fn render_signature(&self) -> u64 {
        let colors = [
            &self.foreground,
            &self.dim,
            &self.accent,
            &self.accent_secondary,
            &self.user,
            &self.assistant,
            &self.tool,
            &self.error,
            &self.warning,
            &self.success,
            &self.border,
            &self.selection_bg,
            &self.selection_fg,
            &self.input_fg,
            &self.overlay_border,
        ];
        let mut signature = 14_695_981_039_346_656_037_u64;
        for color in colors {
            for byte in color_to_string(*color).as_bytes() {
                signature ^= u64::from(*byte);
                signature = signature.wrapping_mul(1_099_511_628_211);
            }
        }
        signature
    }
}

/// Parse a Color from a string representation.
///
/// Supports: named colors (e.g., "red", "blue"), hex (e.g., "#1e1e2e"),
/// and "reset".
pub fn parse_color(s: &str) -> Option<Color> {
    match s.to_lowercase().as_str() {
        "reset" => Some(Color::Reset),
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" | "purple" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" | "grey" => Some(Color::Gray),
        "darkgray" | "darkgrey" => Some(Color::DarkGray),
        "white" => Some(Color::White),
        _ if s.starts_with('#') => parse_hex_color(s),
        _ if s.len() == 6 && s.chars().all(|c| c.is_ascii_hexdigit()) => parse_hex_color(s),
        _ => None,
    }
}

/// Parse a hex color string like "#1e1e2e" or "1e1e2e".
fn parse_hex_color(s: &str) -> Option<Color> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

/// Handle for a theme file watcher. Stops watching when dropped.
#[derive(Debug)]
pub struct ThemeWatchHandle {
    stop_tx: mpsc::Sender<()>,
}

impl Drop for ThemeWatchHandle {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(());
    }
}

/// Theme manager — loads, stores, and switches themes.
#[derive(Debug)]
pub struct ThemeManager {
    /// Currently active theme.
    active: Theme,
    /// Available named themes.
    themes: HashMap<String, Theme>,
    /// Names reserved by built-in themes; plugin registration may not shadow them.
    builtin_names: HashSet<String>,
    /// Theme directory path.
    theme_dir: Option<PathBuf>,
}

impl Default for ThemeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ThemeManager {
    /// Create a new theme manager with built-in themes.
    #[must_use]
    pub fn new() -> Self {
        let mut themes = HashMap::new();
        let dark = Theme::dark();
        let light = Theme::light();
        let hc = Theme::high_contrast();
        let tn = Theme::tokyo_night();
        let builtin_names: HashSet<String> = [&dark, &light, &hc, &tn]
            .iter()
            .map(|t| t.name.clone())
            .collect();
        themes.insert(dark.name.clone(), dark);
        themes.insert(light.name.clone(), light);
        themes.insert(hc.name.clone(), hc);
        themes.insert(tn.name.clone(), tn);

        Self {
            active: Theme::dark(),
            themes,
            builtin_names,
            theme_dir: None,
        }
    }

    /// Set the theme directory for loading custom themes.
    pub fn set_theme_dir(&mut self, dir: PathBuf) {
        self.theme_dir = Some(dir);
    }

    /// Load custom themes from the theme directory.
    ///
    /// Reads all `.json` files in the directory as theme configs.
    pub fn load_custom_themes(&mut self) -> std::io::Result<usize> {
        let Some(dir) = &self.theme_dir else {
            return Ok(0);
        };
        if !dir.exists() {
            return Ok(0);
        }

        let mut count = 0;
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(theme) = Theme::from_json(&content) {
                    self.themes.insert(theme.name.clone(), theme);
                    count += 1;
                }
            }
        }
        Ok(count)
    }

    /// Switch to a theme by name.
    ///
    /// Returns `true` if the theme was found and applied.
    pub fn switch(&mut self, name: &str) -> bool {
        if let Some(theme) = self.themes.get(name) {
            self.active = theme.clone();
            true
        } else {
            false
        }
    }

    /// Get the currently active theme.
    #[must_use]
    pub const fn active(&self) -> &Theme {
        &self.active
    }

    /// Get the name of the currently active theme.
    #[must_use]
    pub fn active_name(&self) -> &str {
        &self.active.name
    }

    /// List all available theme names.
    #[must_use]
    pub fn list_themes(&self) -> Vec<&str> {
        self.themes
            .keys()
            .map(std::string::String::as_str)
            .collect()
    }

    /// Get a theme by name.
    #[must_use]
    pub fn get_theme(&self, name: &str) -> Option<&Theme> {
        self.themes.get(name)
    }

    /// Register a custom theme.
    pub fn register(&mut self, theme: Theme) {
        self.themes.insert(theme.name.clone(), theme);
    }

    /// Register a plugin-provided theme from a JSON value under `name`.
    ///
    /// The JSON is validated with [`Theme::from_json`]. The registry name is
    /// authoritative: any `name` field inside `json` is overwritten with
    /// `name`, so plugins can namespace their themes (for example
    /// `"github/coral-night"`) without editing the JSON payload.
    ///
    /// Built-in theme names are reserved and yield
    /// `Err("theme '<name>' is built in")`. Registering an existing
    /// non-built-in name replaces that theme.
    pub fn register_json(&mut self, name: &str, json: &serde_json::Value) -> Result<(), String> {
        if self.builtin_names.contains(name) {
            return Err(format!("theme '{name}' is built in"));
        }
        let mut theme = Theme::from_json(&json.to_string())?;
        theme.name = name.to_string();
        self.themes.insert(name.to_string(), theme);
        Ok(())
    }

    /// Names of all registered themes, sorted alphabetically.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.themes.keys().cloned().collect();
        names.sort();
        names
    }

    /// Whether a theme with the given name is registered.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.themes.contains_key(name)
    }

    /// Cycle to the next theme.
    pub fn next_theme(&mut self) -> &Theme {
        let names: Vec<String> = self.themes.keys().cloned().collect();
        if names.is_empty() {
            return &self.active;
        }
        let current_idx = names
            .iter()
            .position(|n| n == &self.active.name)
            .unwrap_or(0);
        let next_idx = (current_idx + 1) % names.len();
        let _ = self.switch(&names[next_idx]);
        &self.active
    }

    /// Start watching the theme directory for file changes.
    ///
    /// Spawns a background thread that watches `.json` files in the theme directory.
    /// On change, calls `on_reload` with the new theme count.
    /// Returns a handle that stops watching when dropped.
    pub fn start_watcher<F>(&self, on_reload: F) -> ThemeWatchHandle
    where
        F: Fn(usize) + Send + 'static,
    {
        let dir = self.theme_dir.clone();
        let (tx, rx) = mpsc::channel();
        let handle = ThemeWatchHandle {
            stop_tx: tx.clone(),
        };

        thread::spawn(move || {
            use notify::{EventKind, RecursiveMode, Watcher as _};

            let Some(dir) = dir else { return };
            if !dir.exists() {
                return;
            }

            let mut watcher =
                match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                    if let Ok(event) = res {
                        if matches!(
                            event.kind,
                            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                        ) {
                            let has_json = event
                                .paths
                                .iter()
                                .any(|p| p.extension().and_then(|e| e.to_str()) == Some("json"));
                            if has_json {
                                let _ = tx.send(());
                            }
                        }
                    }
                }) {
                    Ok(w) => w,
                    Err(e) => {
                        tracing::warn!("Failed to create theme watcher: {e}");
                        return;
                    }
                };

            if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
                tracing::warn!("Failed to watch theme directory: {e}");
                return;
            }

            // Block on rx until stop signal
            let _ = rx.recv();
            let _ = watcher.unwatch(&dir);
        });

        // Also do an initial reload
        Self::reload_and_notify(&on_reload);

        handle
    }

    /// Reload themes from directory and notify.
    fn reload_and_notify<F>(on_reload: &F)
    where
        F: Fn(usize),
    {
        // We can't mutate self here, so the caller should handle the reload
        // in response to the file change signal
        on_reload(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_theme_is_dark() {
        let theme = Theme::default();
        assert_eq!(theme.name, "dark");
        // The background is painted, not left to the terminal: switching
        // themes has to be visible whatever the terminal's own palette is.
        assert_eq!(theme.background, Color::Rgb(24, 24, 27));
        assert_eq!(theme.foreground, Color::Rgb(229, 231, 235));
    }

    /// The two themes users switch between must differ in the field the frame
    /// paint uses, or the switch would only re-tint accents.
    #[test]
    fn dark_and_light_have_distinct_backgrounds() {
        assert_ne!(Theme::dark().background, Theme::light().background);
        assert_ne!(Theme::dark().foreground, Theme::light().foreground);
    }

    /// macOS Terminal.app reports only 256 colours; 24-bit sequences must be
    /// approximated there. Explicit capability signals win over the guess.
    #[test]
    fn truecolor_detection() {
        assert!(!truecolor_for(
            Some("Apple_Terminal"),
            None,
            Some("xterm-256color"),
            false
        ));
        assert!(truecolor_for(
            Some("Apple_Terminal"),
            Some("truecolor"),
            None,
            false
        ));
        assert!(truecolor_for(
            Some("iTerm.app"),
            None,
            Some("xterm-256color"),
            false
        ));
        assert!(truecolor_for(None, None, Some("xterm-direct"), false));
        assert!(truecolor_for(None, None, Some("xterm-256color"), true));
    }

    #[test]
    fn quantize_maps_rgb_to_the_nearest_xterm_entry() {
        assert_eq!(
            quantize_color(Color::Rgb(255, 255, 255)),
            Color::Indexed(231)
        );
        assert_eq!(quantize_color(Color::Rgb(0, 0, 0)), Color::Indexed(16));
        // #18181b (the dark theme background) is closest to the grey ramp.
        assert_eq!(quantize_color(Color::Rgb(24, 24, 27)), Color::Indexed(234));
        // Palette colours pass through.
        assert_eq!(quantize_color(Color::Blue), Color::Blue);
    }

    #[test]
    fn a_quantized_theme_has_no_24_bit_colours() {
        let theme = Theme::dark().quantized();
        assert_eq!(theme.name, "dark");
        assert!(!matches!(theme.background, Color::Rgb(..)));
        assert!(!matches!(theme.foreground, Color::Rgb(..)));
        assert!(!matches!(theme.accent, Color::Rgb(..)));
        assert!(!matches!(theme.assistant, Color::Rgb(..)));
        assert!(!matches!(theme.border, Color::Rgb(..)));
    }

    #[test]
    fn built_in_themes_exist() {
        let mgr = ThemeManager::new();
        assert!(mgr.get_theme("dark").is_some());
        assert!(mgr.get_theme("light").is_some());
        assert!(mgr.get_theme("high-contrast").is_some());
        assert!(mgr.get_theme("tokyo-night").is_some());
    }

    #[test]
    fn switch_theme() {
        let mut mgr = ThemeManager::new();
        assert_eq!(mgr.active_name(), "dark");
        assert!(mgr.switch("light"));
        assert_eq!(mgr.active_name(), "light");
        assert!(!mgr.switch("nonexistent"));
        assert_eq!(mgr.active_name(), "light"); // unchanged
    }

    #[test]
    fn list_themes() {
        let mgr = ThemeManager::new();
        let themes = mgr.list_themes();
        assert!(themes.contains(&"dark"));
        assert!(themes.contains(&"light"));
        assert!(themes.contains(&"tokyo-night"));
    }

    #[test]
    fn next_theme_cycles() {
        let mut mgr = ThemeManager::new();
        let first_name = mgr.active_name().to_string();
        let next = mgr.next_theme();
        assert_ne!(next.name, first_name);
        // Cycle all the way back
        let _ = mgr.switch(&first_name);
        assert_eq!(mgr.active_name(), first_name);
    }

    #[test]
    fn parse_named_colors() {
        assert_eq!(parse_color("red"), Some(Color::Red));
        assert_eq!(parse_color("blue"), Some(Color::Blue));
        assert_eq!(parse_color("reset"), Some(Color::Reset));
        assert_eq!(parse_color("purple"), Some(Color::Magenta));
        assert_eq!(parse_color("grey"), Some(Color::Gray));
    }

    #[test]
    fn parse_hex_colors() {
        assert_eq!(parse_color("#ff0000"), Some(Color::Rgb(255, 0, 0)));
        assert_eq!(parse_color("#00ff00"), Some(Color::Rgb(0, 255, 0)));
        assert_eq!(parse_color("#0000ff"), Some(Color::Rgb(0, 0, 255)));
        assert_eq!(parse_color("ff0000"), Some(Color::Rgb(255, 0, 0)));
    }

    #[test]
    fn parse_invalid_color() {
        assert!(parse_color("not-a-color").is_none());
        assert!(parse_color("#xyz").is_none());
        assert!(parse_color("#12345").is_none());
        assert!(parse_color("#1234567").is_none());
    }

    #[test]
    fn theme_json_roundtrip() {
        let theme = Theme::dark();
        let json = theme.to_json().unwrap();
        let parsed = Theme::from_json(&json).unwrap();
        assert_eq!(parsed.name, theme.name);
        assert_eq!(parsed.background, theme.background);
        assert_eq!(parsed.accent, theme.accent);
    }

    #[test]
    fn register_custom_theme() {
        let mut mgr = ThemeManager::new();
        let custom = Theme {
            name: "custom".into(),
            background: Color::Rgb(10, 10, 10),
            ..Theme::dark()
        };
        mgr.register(custom);
        assert!(mgr.get_theme("custom").is_some());
    }

    fn valid_theme_json() -> serde_json::Value {
        serde_json::json!({
            "name": "payload-name",
            "background": "#112233",
            "foreground": "#eeeeee",
            "dim": "#666666",
            "accent": "#abcdef",
            "accent_secondary": "#00bcd4",
            "statusbar_bg": "#333333",
            "statusbar_fg": "#ffffff",
            "error": "#e74c3c",
            "warning": "#f39c12",
            "success": "#2ecc71",
            "border": "#444444",
            "selection_bg": "#2a2a4a",
            "selection_fg": "#ffffff",
            "input_bg": "#111111",
            "input_fg": "#eeeeee",
            "overlay_bg": "#111111",
            "overlay_border": "#666666",
            "vim_normal": "#4a9eff",
            "vim_insert": "#2ecc71",
            "notif_info": "#4a9eff",
            "notif_warning": "#f39c12",
            "notif_error": "#e74c3c"
        })
    }

    #[test]
    fn register_json_registers_plugin_theme_by_name() {
        let mut mgr = ThemeManager::new();
        mgr.register_json("plugin/coral", &valid_theme_json())
            .unwrap();

        assert!(mgr.contains("plugin/coral"));
        let theme = mgr.get_theme("plugin/coral").unwrap();
        // The registry name wins over the JSON `name` field.
        assert_eq!(theme.name, "plugin/coral");
        // Colors from the plugin payload are preserved.
        assert_eq!(theme.background, Color::Rgb(0x11, 0x22, 0x33));
        assert_eq!(theme.accent, Color::Rgb(0xab, 0xcd, 0xef));
        assert_eq!(theme.error, Color::Rgb(0xe7, 0x4c, 0x3c));

        // Re-registering a non-built-in name replaces it.
        let mut replacement = valid_theme_json();
        replacement["background"] = serde_json::json!("#445566");
        mgr.register_json("plugin/coral", &replacement).unwrap();
        assert_eq!(
            mgr.get_theme("plugin/coral").unwrap().background,
            Color::Rgb(0x44, 0x55, 0x66)
        );
    }

    #[test]
    fn register_json_refuses_builtin_name() {
        let mut mgr = ThemeManager::new();
        let err = mgr.register_json("dark", &valid_theme_json()).unwrap_err();
        assert_eq!(err, "theme 'dark' is built in");
        // The built-in is untouched.
        assert_eq!(
            mgr.get_theme("dark").unwrap().background,
            Color::Rgb(24, 24, 27)
        );
    }

    #[test]
    fn register_json_rejects_invalid_json() {
        let mut mgr = ThemeManager::new();
        assert!(
            mgr.register_json("bad", &serde_json::json!("nope"))
                .is_err()
        );
        assert!(
            mgr.register_json("bad", &serde_json::json!({"name": "partial"}))
                .is_err()
        );
        assert!(!mgr.contains("bad"));
    }

    #[test]
    fn names_are_sorted_and_contains_works() {
        let mut mgr = ThemeManager::new();
        mgr.register_json("zzz", &valid_theme_json()).unwrap();
        mgr.register_json("aaa", &valid_theme_json()).unwrap();

        assert_eq!(
            mgr.names(),
            vec![
                "aaa",
                "dark",
                "high-contrast",
                "light",
                "tokyo-night",
                "zzz"
            ]
        );
        assert!(mgr.contains("aaa"));
        assert!(mgr.contains("tokyo-night"));
        assert!(!mgr.contains("missing"));
    }

    #[test]
    fn load_custom_themes_from_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let theme_json = r##"{"name":"my-theme","background":"#1a1a2e","foreground":"#e0e0e0","dim":"#666666","accent":"#4a9eff","accent_secondary":"#00bcd4","statusbar_bg":"#333333","statusbar_fg":"#ffffff","error":"#e74c3c","warning":"#f39c12","success":"#2ecc71","border":"#444444","selection_bg":"#2a2a4a","selection_fg":"#ffffff","input_bg":"#111111","input_fg":"#eeeeee","overlay_bg":"#111111","overlay_border":"#666666","vim_normal":"#4a9eff","vim_insert":"#2ecc71","notif_info":"#4a9eff","notif_warning":"#f39c12","notif_error":"#e74c3c"}"##;
        std::fs::write(tmp.path().join("my-theme.json"), theme_json).unwrap();

        let mut mgr = ThemeManager::new();
        mgr.set_theme_dir(tmp.path().to_path_buf());
        let count = mgr.load_custom_themes().unwrap();
        assert_eq!(count, 1);
        assert!(mgr.get_theme("my-theme").is_some());
    }

    #[test]
    fn no_theme_dir_returns_zero() {
        let mut mgr = ThemeManager::new();
        let count = mgr.load_custom_themes().unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn tokyo_night_has_rgb_colors() {
        let theme = Theme::tokyo_night();
        assert!(matches!(theme.background, Color::Rgb(_, _, _)));
        assert_eq!(theme.background, Color::Rgb(30, 30, 46));
    }
}
