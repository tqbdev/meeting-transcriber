//! The few UI details that differ between macOS and Windows.

use std::{path::Path, path::PathBuf, process::Command};

use eframe::egui::FontFamily;

/// A font file to load, and whether it goes first in its family (primary)
/// or after the primary as a fallback.
pub struct FontFile {
    pub name: &'static str,
    pub path: PathBuf,
    pub family: FontFamily,
    pub primary: bool,
}

/// System fonts with full Vietnamese coverage (egui's built-in fonts lack
/// letters like ồ, ớ, ệ), plus a CJK fallback. Missing files are skipped.
pub fn fonts() -> Vec<FontFile> {
    let font = |name, path: PathBuf, family, primary| FontFile {
        name,
        path,
        family,
        primary,
    };
    if cfg!(target_os = "macos") {
        let fonts = Path::new("/System/Library/Fonts");
        let unicode = fonts.join("Supplemental/Arial Unicode.ttf");
        vec![
            font("sf", fonts.join("SFNS.ttf"), FontFamily::Proportional, true),
            font(
                "sf-mono",
                fonts.join("SFNSMono.ttf"),
                FontFamily::Monospace,
                true,
            ),
            font(
                "arial-unicode",
                unicode.clone(),
                FontFamily::Proportional,
                false,
            ),
            font("arial-unicode", unicode, FontFamily::Monospace, false),
        ]
    } else if cfg!(windows) {
        let windir =
            std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        let fonts = windir.join("Fonts");
        let cjk = fonts.join("msyh.ttc");
        vec![
            font(
                "segoe-ui",
                fonts.join("segoeui.ttf"),
                FontFamily::Proportional,
                true,
            ),
            font(
                "consolas",
                fonts.join("consola.ttf"),
                FontFamily::Monospace,
                true,
            ),
            font("yahei", cjk.clone(), FontFamily::Proportional, false),
            font("yahei", cjk, FontFamily::Monospace, false),
        ]
    } else {
        Vec::new()
    }
}

pub const REVEAL_LABEL: &str = if cfg!(target_os = "macos") {
    "Show in Finder"
} else if cfg!(windows) {
    "Show in Explorer"
} else {
    "Open folder"
};

pub fn reveal(dir: &Path) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = Command::new(opener).arg(dir).spawn();
}

pub const MIC_SILENT_HINT: &str = if cfg!(target_os = "macos") {
    "Silent. Say something. If it stays flat, allow this app under \
     System Settings › Privacy & Security › Microphone."
} else if cfg!(windows) {
    "Silent. Say something. If it stays flat, turn on \
     Settings › Privacy & security › Microphone › Let desktop apps access your microphone."
} else {
    "Silent. Say something. If it stays flat, check the microphone permissions."
};

pub const SYSTEM_SILENT_HINT: &str = if cfg!(target_os = "macos") {
    "Silent. Play some audio. If it stays flat, allow this app under \
     System Settings › Privacy & Security › Screen & System Audio Recording."
} else {
    // Windows loopback needs no permission; silence means nothing is playing
    // on the default output device.
    "Silent. Play some audio through the default output device."
};
