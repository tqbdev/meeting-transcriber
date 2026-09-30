//! A small always-on-top window with the live transcript, for catching up at
//! a glance while the meeting app has focus.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use eframe::egui;

use crate::live::SharedTranscript;

const TITLE: &str = "Live transcript";

pub fn id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("live-transcript-overlay")
}

pub struct Overlay {
    /// Cleared by the overlay itself when its window is closed.
    open: Arc<AtomicBool>,
    pub font_size: f32,
    /// Whether the macOS window has been set to show on every Space.
    pinned: bool,
}

impl Overlay {
    pub fn new() -> Self {
        Self {
            open: Arc::new(AtomicBool::new(false)),
            font_size: 17.0,
            pinned: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }

    pub fn set_open(&mut self, open: bool) {
        self.open.store(open, Ordering::Relaxed);
        if !open {
            self.pinned = false;
        }
    }

    /// Call every frame of the main window. The overlay runs as a deferred
    /// viewport, so it keeps repainting when the main window is minimized.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        transcript: SharedTranscript,
        show_translation: bool,
    ) {
        if !self.is_open() {
            return;
        }
        let builder = egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([560.0, 220.0])
            .with_min_inner_size([280.0, 90.0])
            .with_always_on_top();
        let open = self.open.clone();
        let font_size = self.font_size;
        ctx.show_viewport_deferred(id(), builder, move |ui, _class| {
            if ui.input(|i| i.viewport().close_requested()) {
                open.store(false, Ordering::Relaxed);
            }
            egui::CentralPanel::default().show(ui, |ui| {
                let transcript = transcript.lock().unwrap();
                if transcript.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.weak("Waiting for speech…");
                    });
                } else {
                    transcript.ui(ui, font_size, show_translation);
                }
            });
        });

        if !self.pinned {
            self.pinned = pin_to_all_spaces();
        }
    }
}

/// Lets the overlay follow you across Spaces and float over full-screen apps
/// (a full-screen Zoom or Teams call is its own Space). egui has no option
/// for this, so find the window by title and set it on the NSWindow.
/// Returns false until the window exists.
#[cfg(target_os = "macos")]
fn pin_to_all_spaces() -> bool {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSWindowCollectionBehavior};

    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let app = NSApplication::sharedApplication(mtm);
    for window in app.windows().iter() {
        if window.title().to_string() == TITLE {
            window.setCollectionBehavior(
                window.collectionBehavior()
                    | NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::FullScreenAuxiliary,
            );
            return true;
        }
    }
    false
}

/// Elsewhere always-on-top is enough: on Windows a topmost window already
/// stays above full-screen (borderless) meeting windows.
#[cfg(not(target_os = "macos"))]
fn pin_to_all_spaces() -> bool {
    true
}
