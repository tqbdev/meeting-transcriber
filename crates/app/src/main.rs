//! Meeting Transcriber: records mic ("me") and system audio ("them") to
//! separate WAV files and transcribes both live with Soniox.

// No console window behind the app in Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod live;
mod overlay;
mod platform;
mod secrets;
mod transcript;

use std::{path::PathBuf, sync::Arc, time::Duration};

use capture::{DeviceInfo, Recording, Session, SessionOptions, Source, SystemOutput, TrackLevel};
use eframe::egui;

/// Meters show -60 dBFS..0 dBFS.
const FLOOR_DB: f32 = -60.0;
/// Below this a track counts as silent for the permission hint.
const SILENCE_DB: f32 = -50.0;
/// How long a track may stay silent before we suggest checking permissions.
const SILENCE_HINT_AFTER: f32 = 5.0;

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for platform::FontFile {
        name,
        path,
        family,
        primary,
    } in platform::fonts()
    {
        if !fonts.font_data.contains_key(name) {
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            fonts
                .font_data
                .insert(name.to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
        }
        let list = fonts.families.entry(family).or_default();
        if primary {
            list.insert(0, name.to_owned());
        } else {
            // After our primary font, before egui's own (kept for emoji).
            list.insert(1.min(list.len()), name.to_owned());
        }
    }
    ctx.set_fonts(fonts);
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Meeting Transcriber")
            .with_inner_size([820.0, 760.0])
            .with_min_inner_size([520.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Meeting Transcriber",
        options,
        Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))),
    )
}

/// Display state for one level meter.
struct Meter {
    level: TrackLevel,
    display_db: f32,
    silent_for: f32,
}

struct App {
    ctx: egui::Context,
    settings: live::Settings,
    /// `None` only if the network runtime failed to start.
    live: Option<live::Live>,
    overlay: overlay::Overlay,
    /// Whether "Transcription settings" starts expanded.
    settings_open: bool,
    mics: Vec<DeviceInfo>,
    mic_device: Option<String>,
    capture_mic: bool,
    capture_system: bool,
    output: Option<SystemOutput>,
    session: Option<Session>,
    meters: Vec<Meter>,
    last_recordings: Vec<Recording>,
    errors: Vec<String>,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        install_fonts(ctx);
        let mut errors = Vec::new();
        let live = live::Live::new()
            .map_err(|e| errors.push(format!("Live transcription unavailable: {e:#}")))
            .ok();
        let mut app = Self {
            ctx: ctx.clone(),
            settings: live::Settings::load(),
            live,
            overlay: overlay::Overlay::new(),
            settings_open: false,
            mics: Vec::new(),
            mic_device: None,
            capture_mic: true,
            capture_system: true,
            output: None,
            session: None,
            meters: Vec::new(),
            last_recordings: Vec::new(),
            errors,
        };
        app.refresh_devices();
        #[cfg(debug_assertions)]
        if std::env::var_os("MEETING_TRANSCRIBER_DEMO").is_some()
            && let Some(live) = &mut app.live
        {
            live.load_demo();
            app.overlay.set_open(true);
            // In memory only, so the demo doesn't overwrite saved settings.
            app.settings.translation = live::TranslationMode::TwoWay;
            app.settings_open = true;
        }
        app
    }

    fn refresh_devices(&mut self) {
        match capture::input_devices() {
            Ok(mics) => self.mics = mics,
            Err(e) => self.errors.push(format!("Listing microphones: {e:#}")),
        }
        if let Some(id) = &self.mic_device
            && !self.mics.iter().any(|d| &d.id == id)
        {
            self.mic_device = None;
        }
        self.output = capture::system_output();
    }

    fn start(&mut self) {
        self.refresh_devices();
        let out_dir =
            recordings_root().join(chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string());
        let sources: Vec<Source> = [
            (self.capture_mic, Source::Mic),
            (self.capture_system, Source::System),
        ]
        .into_iter()
        .filter_map(|(on, source)| on.then_some(source))
        .collect();

        let mut sinks = None;
        if self.settings.enabled
            && self.settings.has_key()
            && let Some(live) = &mut self.live
        {
            match live.begin(&self.settings, &sources, &out_dir, &self.ctx) {
                Ok(factory) => sinks = Some(factory),
                Err(e) => self.errors.push(format!("Live transcription: {e:#}")),
            }
        }

        let options = SessionOptions {
            out_dir,
            mic: self.capture_mic,
            mic_device: self.mic_device.clone(),
            system: self.capture_system,
            sinks,
        };
        match Session::start(options) {
            Ok(session) => {
                self.meters = session
                    .levels()
                    .into_iter()
                    .map(|level| Meter {
                        level,
                        display_db: FLOOR_DB,
                        silent_for: 0.0,
                    })
                    .collect();
                self.session = Some(session);
            }
            Err(e) => {
                self.errors.push(format!("Could not start: {e:#}"));
                if let Some(live) = &mut self.live {
                    live.abandon();
                }
            }
        }
    }

    fn stop(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        self.errors.extend(session.take_errors());
        match session.stop() {
            Ok(recordings) => self.last_recordings = recordings,
            Err(e) => self.errors.push(format!("Stopping: {e:#}")),
        }
        self.meters.clear();
    }

    /// Pulls the latest peaks from the capture threads into the meters.
    fn poll(&mut self, dt: f32) {
        if let Some(live) = &mut self.live {
            let (errors, changed) = live.poll();
            self.errors.extend(errors);
            if changed && self.overlay.is_open() {
                self.ctx.request_repaint_of(overlay::id());
            }
        }
        let Some(session) = &self.session else {
            return;
        };
        self.errors.extend(session.take_errors());
        for (meter, level) in self.meters.iter_mut().zip(session.levels()) {
            let db = 20.0 * level.peak.max(1e-6).log10();
            // Rise instantly, fall at 24 dB/s so the bar is readable.
            meter.display_db = db.max(meter.display_db - 24.0 * dt).max(FLOOR_DB);
            meter.silent_for = if db > SILENCE_DB {
                0.0
            } else {
                meter.silent_for + dt
            };
            meter.level = level;
        }
    }

    fn sources_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_enabled_ui(self.session.is_none(), |ui| {
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.capture_mic, "Microphone");
                let selected = self
                    .mic_device
                    .as_ref()
                    .and_then(|id| self.mics.iter().find(|d| &d.id == id))
                    .map_or("System default".to_owned(), |d| d.name.clone());
                egui::ComboBox::from_id_salt("mic")
                    .selected_text(selected)
                    .width(220.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.mic_device, None, "System default");
                        for device in &self.mics {
                            ui.selectable_value(
                                &mut self.mic_device,
                                Some(device.id.clone()),
                                &device.name,
                            );
                        }
                    });
                if ui
                    .button("Refresh")
                    .on_hover_text("Re-scan audio devices")
                    .clicked()
                {
                    self.refresh_devices();
                }
            });

            ui.horizontal(|ui| {
                ui.checkbox(&mut self.capture_system, "System audio");
                match &self.output {
                    Some(output) => ui.weak(format!("from {}", output.name)),
                    None => ui.colored_label(ui.visuals().warn_fg_color, "no output device"),
                };
            });
            let mic = match &self.mic_device {
                Some(id) => self.mics.iter().find(|d| &d.id == id),
                None => self.mics.iter().find(|d| d.is_default),
            };
            if self.capture_mic
                && let Some(mic) = mic
                && mic.bluetooth
            {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!(
                        "{} is a Bluetooth mic. Recording from it switches the headset to call \
                         mode, which lowers playback and system-audio quality. A built-in \
                         microphone avoids this.",
                        mic.name
                    ),
                );
            }
            if let Some(output) = &self.output
                && output.has_input
                && self.capture_system
            {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "This output device also has a microphone, so the system track would \
                     record that microphone instead of the audio playing through it. \
                     Switch the output to speakers or another device to test.",
                );
            }
        });
    }

    fn record_button_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(elapsed) = self.session.as_ref().map(Session::elapsed) {
                let stop = egui::Button::new(egui::RichText::new("Stop").strong())
                    .fill(egui::Color32::from_rgb(190, 40, 40));
                if ui.add_sized([120.0, 34.0], stop).clicked() {
                    self.stop();
                    return;
                }
                let secs = elapsed.as_secs();
                ui.label(
                    egui::RichText::new(format!("● REC {:02}:{:02}", secs / 60, secs % 60))
                        .color(egui::Color32::from_rgb(220, 60, 60))
                        .monospace(),
                );
            } else {
                let can_start = self.capture_mic || self.capture_system;
                if self.settings.enabled && !self.settings.has_key() {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        "Add a Soniox API key to transcribe; recording still works.",
                    );
                }
                let start = egui::Button::new(egui::RichText::new("Start recording").strong());
                if ui.add_enabled(can_start, start).clicked() {
                    self.start();
                }
            }
        });
    }

    fn meters_ui(&self, ui: &mut egui::Ui) {
        for meter in &self.meters {
            let source = meter.level.source;
            ui.label(format!(
                "{}  ·  {}",
                source.label(),
                meter.level.device_name
            ));
            let fraction = (meter.display_db - FLOOR_DB) / -FLOOR_DB;
            let color = if meter.display_db > -3.0 {
                egui::Color32::from_rgb(220, 60, 60)
            } else {
                egui::Color32::from_rgb(60, 170, 90)
            };
            ui.add(
                egui::ProgressBar::new(fraction)
                    .fill(color)
                    .desired_height(14.0)
                    .text(format!("{:>4.0} dB", meter.display_db)),
            );
            if meter.level.dropped_samples > 0 {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!(
                        "{} samples dropped (disk too slow)",
                        meter.level.dropped_samples
                    ),
                );
            }
            if meter.level.glitches > 0 {
                ui.weak(format!(
                    "{} audio glitch{} reported by the system",
                    meter.level.glitches,
                    if meter.level.glitches == 1 { "" } else { "es" }
                ));
            }
            if meter.silent_for > SILENCE_HINT_AFTER {
                let hint = match source {
                    Source::Mic => platform::MIC_SILENT_HINT,
                    Source::System => platform::SYSTEM_SILENT_HINT,
                };
                ui.weak(hint);
            }
            ui.add_space(6.0);
        }
    }

    fn recordings_ui(&self, ui: &mut egui::Ui) {
        if self.last_recordings.is_empty() {
            return;
        }
        ui.separator();
        ui.strong("Last recording");
        for recording in &self.last_recordings {
            let file = recording
                .path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            ui.label(format!(
                "{}: {file} ({:.1} s)",
                recording.source.label(),
                recording.seconds
            ));
        }
        if let Some(dir) = self.last_recordings[0].path.parent()
            && ui.button(platform::REVEAL_LABEL).clicked()
        {
            platform::reveal(dir);
        }
    }

    fn errors_ui(&mut self, ui: &mut egui::Ui) {
        if self.errors.is_empty() {
            return;
        }
        ui.separator();
        for error in self.errors.iter().rev().take(5) {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        if ui.small_button("Clear").clicked() {
            self.errors.clear();
        }
    }
}

impl eframe::App for App {
    /// Runs even while the main window is minimized, so the overlay keeps updating.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let dt = ctx.input(|i| i.stable_dt);
        self.poll(dt);
        if self.session.is_some() {
            ctx.request_repaint_after(Duration::from_millis(33));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Finalize the WAV files if the window is closed mid-recording.
        if ui.input(|i| i.viewport().close_requested()) {
            self.stop();
        }

        egui::Panel::top("controls").show(ui, |ui| {
            ui.add_space(6.0);
            ui.heading("Meeting Transcriber");
            ui.add_space(6.0);

            self.sources_ui(ui);
            ui.add_space(4.0);
            let recording = self.session.is_some();
            egui::CollapsingHeader::new("Transcription settings")
                .default_open(self.settings_open || !self.settings.has_key())
                .show(ui, |ui| {
                    ui.add_enabled_ui(!recording, |ui| {
                        if let Some(error) = self.settings.ui(ui) {
                            self.errors.push(error);
                        }
                    });
                });
            ui.add_space(8.0);
            self.record_button_ui(ui);
            ui.horizontal(|ui| {
                if let Some(live) = &self.live {
                    live.status_ui(ui);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut open = self.overlay.is_open();
                    ui.add(egui::Slider::new(&mut self.overlay.font_size, 12.0..=28.0).text("size"))
                        .on_hover_text("Text size in the always-on-top window");
                    if ui
                        .toggle_value(&mut open, "Always-on-top transcript")
                        .on_hover_text("A small window that stays above other apps, including full-screen calls")
                        .changed()
                    {
                        self.overlay.set_open(open);
                    }
                    // Display only, so it stays usable while recording.
                    if self.settings.translation != live::TranslationMode::Off
                        && ui
                            .checkbox(&mut self.settings.show_translation, "Show translations")
                            .changed()
                        && let Err(e) = self.settings.save()
                    {
                        self.errors.push(format!("{e:#}"));
                    }
                });
            });
            ui.add_space(8.0);
            self.meters_ui(ui);
            self.recordings_ui(ui);
            self.errors_ui(ui);
            ui.weak(format!("Saving to {}", recordings_root().display()));
            ui.add_space(6.0);
        });

        let transcript = self.live.as_ref().map(live::Live::transcript);
        egui::CentralPanel::default().show(ui, |ui| {
            let transcript = transcript.as_ref().map(|t| t.lock().unwrap());
            match transcript {
                Some(t) if !t.is_empty() => t.ui(ui, 14.0, self.settings.show_translation),
                _ => {
                    ui.centered_and_justified(|ui| {
                        ui.weak("The live transcript appears here while you record.");
                    });
                }
            }
        });

        if let Some(transcript) = transcript {
            self.overlay
                .show(ui.ctx(), transcript, self.settings.show_translation);
        }
    }
}

/// `~/Library/Application Support/…` on macOS, `%LOCALAPPDATA%\…` on
/// Windows (local, not roaming: recordings are large).
fn recordings_root() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Meeting Transcriber")
        .join("Recordings")
}
