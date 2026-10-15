//! Accent color: follows the desktop theme when possible, KDE Plasma on
//! Linux, the DWM accent on Windows.
//!
//! Sources, in order:
//! 1. `REF_ACCENT=#RRGGBB` env override (handy for testing / non-KDE; keeps
//!    the color fixed and disables the watcher below).
//! 2. Windows: `HKCU\Software\Microsoft\Windows\DWM\AccentColor` (the swatch
//!    picked in Settings → Personalization → Colors, "automatic" included).
//!    Elsewhere `kdeglobals`: `[General] AccentColor` (Plasma 6 custom
//!    accent), falling back to `[Colors:Selection] BackgroundNormal`
//!    (Plasma 5, or a scheme without an explicit accent).
//! 3. The built-in blue the app shipped with.
//!
//! The source is re-read about once a second, so changing the accent in
//! System Settings updates the app live.

use bevy::prelude::*;

const FALLBACK: [f32; 3] = [0.05, 0.6, 1.0];

/// The app's accent color, in sRGB 0..1 components.
#[derive(Resource, Clone, Copy, PartialEq)]
pub struct Accent {
    rgb: [f32; 3],
}

impl Default for Accent {
    fn default() -> Self {
        let rgb = accent_override()
            .or_else(read_desktop_accent)
            .unwrap_or(FALLBACK);
        log_rgb(&rgb);
        Self { rgb }
    }
}

impl Accent {
    pub fn rgb(&self) -> [f32; 3] {
        self.rgb
    }

    /// Opaque accent: selection outline, handle borders.
    pub fn base(&self) -> Color {
        Color::srgb(self.rgb[0], self.rgb[1], self.rgb[2])
    }

    /// Accent with an alpha: marquee outline/fill, tinted hovers.
    pub fn alpha(&self, a: f32) -> Color {
        Color::srgba(self.rgb[0], self.rgb[1], self.rgb[2], a)
    }

    /// True when the accent is light enough that dark text reads better on it.
    /// Relative luminance (sRGB → linear); threshold picked so Breeze's
    /// #3DAEE9 gets dark text while deeper blues keep white.
    pub fn is_light(&self) -> bool {
        let lin = |c: f32| {
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(self.rgb[0]) + 0.7152 * lin(self.rgb[1]) + 0.0722 * lin(self.rgb[2]) > 0.35
    }
}

pub struct ThemePlugin;

impl Plugin for ThemePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Accent>()
            .add_systems(Update, watch_desktop_accent);
    }
}

/// Re-read the desktop accent about once a second; cheap (a 4 KB file that
/// only changes when the theme changes).
fn watch_desktop_accent(time: Res<Time>, mut accent: ResMut<Accent>, mut poll: Local<f32>) {
    // A fixed `REF_ACCENT` wins over whatever the desktop says.
    if std::env::var_os("REF_ACCENT").is_some() {
        return;
    }
    *poll += time.delta_secs();
    if *poll < 1.0 {
        return;
    }
    *poll = 0.0;
    let Some(rgb) = read_desktop_accent() else {
        return;
    };
    if rgb != accent.rgb {
        log_rgb(&rgb);
        accent.rgb = rgb;
    }
}

fn accent_override() -> Option<[f32; 3]> {
    let value = std::env::var("REF_ACCENT").ok()?;
    match parse_hex(&value) {
        Some(rgb) => Some(rgb),
        None => {
            warn!("REF_ACCENT={value:?} is not a hex color like #RRGGBB, ignoring");
            None
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn read_kde_accent() -> Option<[f32; 3]> {
    let text = std::fs::read_to_string(kdeglobals_path()?).ok()?;
    parse_kde_accent(&text)
}

/// The desktop's accent, per platform: Windows reads the DWM registry value,
/// everything else the KDE config.
#[cfg(not(target_os = "windows"))]
fn read_desktop_accent() -> Option<[f32; 3]> {
    read_kde_accent()
}

/// The DWM `AccentColor` DWORD (`0xAABBGGRR`; alpha ignored, the app applies
/// its own).
#[cfg(target_os = "windows")]
fn read_desktop_accent() -> Option<[f32; 3]> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let subkey = wide("Software\\Microsoft\\Windows\\DWM");
    let name = wide("AccentColor");
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut value as *mut u32).cast(),
            &mut size,
        )
    };
    if status != 0 || size != 4 {
        return None;
    }
    let r = (value & 0xFF) as u8;
    let g = ((value >> 8) & 0xFF) as u8;
    let b = ((value >> 16) & 0xFF) as u8;
    Some([r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0])
}

#[cfg(not(target_os = "windows"))]
fn kdeglobals_path() -> Option<std::path::PathBuf> {
    if let Some(kdehome) = std::env::var_os("KDEHOME") {
        return Some(std::path::PathBuf::from(kdehome).join("share/config/kdeglobals"));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(std::path::PathBuf::from(xdg).join("kdeglobals"));
    }
    std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config/kdeglobals"))
}

/// `[General] AccentColor` first, else the selection background.
#[cfg(not(target_os = "windows"))]
fn parse_kde_accent(text: &str) -> Option<[f32; 3]> {
    let mut section = "";
    let mut accent = None;
    let mut selection = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match (section, key.trim()) {
            ("General", "AccentColor") => accent = parse_rgb_csv(value),
            ("Colors:Selection", "BackgroundNormal") => selection = parse_rgb_csv(value),
            _ => {}
        }
    }
    accent.or(selection)
}

/// KDE writes colors as `R,G,B` (0-255).
#[cfg(not(target_os = "windows"))]
fn parse_rgb_csv(value: &str) -> Option<[f32; 3]> {
    let mut parts = value.split(',');
    let r: u8 = parts.next()?.trim().parse().ok()?;
    let g: u8 = parts.next()?.trim().parse().ok()?;
    let b: u8 = parts.next()?.trim().parse().ok()?;
    Some([r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0])
}

/// `#RRGGBB` or `RRGGBB`.
fn parse_hex(value: &str) -> Option<[f32; 3]> {
    let hex = value.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some([
        byte(0)? as f32 / 255.0,
        byte(2)? as f32 / 255.0,
        byte(4)? as f32 / 255.0,
    ])
}

fn log_rgb(rgb: &[f32; 3]) {
    info!(
        "accent: rgb({}, {}, {})",
        (rgb[0] * 255.0).round() as u8,
        (rgb[1] * 255.0).round() as u8,
        (rgb[2] * 255.0).round() as u8,
    );
}
