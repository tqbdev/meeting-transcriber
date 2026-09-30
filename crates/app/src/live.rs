//! Live transcription: Soniox settings and one Soniox stream per capture
//! stream.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
};

use anyhow::{Context as _, Result};
use capture::{AudioSink, SinkFactory, Source, StreamFormat};
use eframe::egui;
use serde::{Deserialize, Serialize};
use soniox::Event;

use crate::{secrets, transcript::Transcript};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationMode {
    #[default]
    Off,
    /// Everything into one language.
    OneWay,
    /// Between two languages, each into the other.
    TwoWay,
}

/// Transcription settings. Everything except the API key (which lives in
/// the OS credential store) is saved to `settings.json` between launches.
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    #[serde(skip)]
    pub api_key: String,
    /// Whether `api_key` matches what is in the OS credential store.
    #[serde(skip)]
    key_saved: bool,
    /// Comma-separated language codes, e.g. "en, vi".
    pub languages: String,
    /// Comma- or newline-separated names and jargon for Soniox's context.
    pub terms: String,
    pub translation: TranslationMode,
    /// One-way target.
    pub target_language: String,
    /// Two-way pair.
    pub language_a: String,
    pub language_b: String,
    /// Display only, so it can change mid-recording.
    pub show_translation: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            api_key: String::new(),
            key_saved: false,
            languages: "en, vi".to_owned(),
            terms: String::new(),
            translation: TranslationMode::Off,
            target_language: "vi".to_owned(),
            language_a: "en".to_owned(),
            language_b: "vi".to_owned(),
            show_translation: true,
        }
    }
}

fn settings_path() -> Option<PathBuf> {
    Some(
        dirs::data_local_dir()?
            .join("Meeting Transcriber")
            .join("settings.json"),
    )
}

impl Settings {
    /// Reads the saved settings and key. The key falls back to
    /// `SONIOX_API_KEY` (handy with `cargo run`; an app started from Finder
    /// or Explorer has no shell env).
    pub fn load() -> Self {
        let mut settings: Settings = settings_path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default();
        let stored = secrets::load_api_key();
        settings.key_saved = stored.is_some();
        settings.api_key = stored
            .or_else(|| std::env::var("SONIOX_API_KEY").ok())
            .unwrap_or_default();
        settings
    }

    pub fn save(&self) -> Result<()> {
        let path = settings_path().context("no app data folder")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("saving {}", path.display()))
    }

    pub fn has_key(&self) -> bool {
        !self.api_key.trim().is_empty()
    }

    fn save_key(&mut self) -> Result<()> {
        secrets::save_api_key(self.api_key.trim())?;
        self.key_saved = true;
        Ok(())
    }

    fn translation(&self) -> Option<soniox::Translation> {
        match self.translation {
            TranslationMode::Off => None,
            TranslationMode::OneWay => Some(soniox::Translation::OneWay {
                target_language: self.target_language.clone(),
            }),
            TranslationMode::TwoWay if self.language_a != self.language_b => {
                Some(soniox::Translation::TwoWay {
                    language_a: self.language_a.clone(),
                    language_b: self.language_b.clone(),
                })
            }
            TranslationMode::TwoWay => None,
        }
    }

    pub fn translating(&self) -> bool {
        self.enabled && self.translation().is_some()
    }

    /// Returns an error message if saving failed.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> Option<String> {
        let mut error = None;
        let mut changed = ui
            .checkbox(&mut self.enabled, "Live transcription (Soniox)")
            .changed();
        ui.add_enabled_ui(self.enabled, |ui| {
            egui::Grid::new("soniox")
                .num_columns(2)
                .spacing([8.0, 6.0])
                .show(ui, |ui| {
                    ui.label("API key");
                    ui.horizontal(|ui| {
                        let edit = egui::TextEdit::singleline(&mut self.api_key)
                            .password(true)
                            .hint_text("from console.soniox.com")
                            .desired_width(240.0);
                        if ui.add(edit).changed() {
                            self.key_saved = false;
                        }
                        if !self.key_saved
                            && ui
                                .button(format!("Save to {}", secrets::STORE_NAME))
                                .clicked()
                            && let Err(e) = self.save_key()
                        {
                            error = Some(format!("{e:#}"));
                        }
                    });
                    ui.end_row();

                    ui.label("Languages");
                    changed |= ui
                        .add(
                            egui::TextEdit::singleline(&mut self.languages)
                                .hint_text("en, vi")
                                .desired_width(240.0),
                        )
                        .on_hover_text(
                            "Language codes your meetings mix. Soniox handles switching mid-sentence.",
                        )
                        .changed();
                    ui.end_row();

                    ui.label("Terms");
                    changed |= ui
                        .add(
                            egui::TextEdit::singleline(&mut self.terms)
                                .hint_text("names, products, jargon (comma-separated)")
                                .desired_width(320.0),
                        )
                        .on_hover_text("Sent as context so Soniox spells these correctly.")
                        .changed();
                    ui.end_row();

                    ui.label("Translation");
                    changed |= self.translation_ui(ui);
                    ui.end_row();
                });
        });
        if changed && let Err(e) = self.save() {
            error = Some(format!("{e:#}"));
        }
        error
    }

    /// Mode picker plus the language pickers the mode needs.
    fn translation_ui(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            let label = |mode| match mode {
                TranslationMode::Off => "Off",
                TranslationMode::OneWay => "One-way",
                TranslationMode::TwoWay => "Two-way",
            };
            egui::ComboBox::from_id_salt("translation-mode")
                .selected_text(label(self.translation))
                .show_ui(ui, |ui| {
                    for (mode, hint) in [
                        (TranslationMode::Off, "Transcript only"),
                        (TranslationMode::OneWay, "Everything into one language"),
                        (
                            TranslationMode::TwoWay,
                            "Between two languages, each into the other",
                        ),
                    ] {
                        changed |= ui
                            .selectable_value(&mut self.translation, mode, label(mode))
                            .on_hover_text(hint)
                            .changed();
                    }
                });
            match self.translation {
                TranslationMode::Off => {}
                TranslationMode::OneWay => {
                    ui.label("into");
                    changed |= language_picker(ui, "target", &mut self.target_language);
                }
                TranslationMode::TwoWay => {
                    changed |= language_picker(ui, "language-a", &mut self.language_a);
                    ui.label("⇄");
                    changed |= language_picker(ui, "language-b", &mut self.language_b);
                    if self.language_a == self.language_b {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            "Pick two different languages",
                        );
                    }
                }
            }
        });
        changed
    }

    fn config(&self, source: Source, reference: String) -> soniox::Config {
        let terms: Vec<String> = split_list(&self.terms);
        soniox::Config {
            api_key: self.api_key.trim().to_owned(),
            model: soniox::DEFAULT_MODEL.to_owned(),
            language_hints: split_list(&self.languages),
            // The mic is always "me": no diarization needed, so endpoint
            // detection can split lines. On the remote side, diarization
            // matters more, and endpoint detection would weaken it.
            enable_speaker_diarization: source == Source::System,
            enable_endpoint_detection: source == Source::Mic,
            enable_language_identification: true,
            context: (!terms.is_empty()).then(|| soniox::Context {
                terms,
                ..Default::default()
            }),
            translation: self.translation(),
            client_reference_id: Some(reference),
        }
    }
}

fn language_picker(ui: &mut egui::Ui, id: &str, code: &mut String) -> bool {
    let mut changed = false;
    egui::ComboBox::from_id_salt(id)
        .selected_text(soniox::language_name(code))
        .height(320.0)
        .show_ui(ui, |ui| {
            for (value, name) in soniox::LANGUAGES {
                changed |= ui
                    .selectable_value(code, (*value).to_owned(), format!("{name} ({value})"))
                    .changed();
            }
        });
    changed
}

fn split_list(text: &str) -> Vec<String> {
    text.split([',', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Live,
    /// The connection dropped; Soniox resends the missed audio once back.
    Reconnecting,
    Finished,
    Failed,
}

/// Shared with the always-on-top window, which draws on its own schedule.
pub type SharedTranscript = Arc<Mutex<Transcript>>;

/// One recording's transcription. Kept after the next recording starts until
/// its streams finish, so the tail of the old transcript is still saved.
struct Run {
    transcript: SharedTranscript,
    streams: HashMap<Source, Status>,
    drops: HashMap<Source, Drops>,
}

/// Connection drops on one stream, for the status line.
#[derive(Default)]
struct Drops {
    count: u32,
    attempt: u32,
    last_reason: String,
}

impl Run {
    fn done(&self) -> bool {
        self.streams
            .values()
            .all(|s| matches!(s, Status::Finished | Status::Failed))
    }
}

/// Owns the async runtime the Soniox streams run on and gathers their events
/// into the transcripts on the UI thread.
pub struct Live {
    runtime: tokio::runtime::Runtime,
    events_tx: mpsc::Sender<(u64, Source, Event)>,
    events_rx: mpsc::Receiver<(u64, Source, Event)>,
    runs: BTreeMap<u64, Run>,
    current: u64,
    /// Shown before the first recording.
    empty: SharedTranscript,
}

impl Live {
    pub fn new() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("soniox")
            .enable_all()
            .build()
            .context("starting the network runtime")?;
        let (events_tx, events_rx) = mpsc::channel();
        Ok(Self {
            runtime,
            events_tx,
            events_rx,
            runs: BTreeMap::new(),
            current: 0,
            empty: SharedTranscript::default(),
        })
    }

    /// The transcript of the latest recording.
    pub fn transcript(&self) -> SharedTranscript {
        self.runs
            .get(&self.current)
            .map_or_else(|| self.empty.clone(), |run| run.transcript.clone())
    }

    /// Starts a transcript for a new recording in `out_dir` and returns the
    /// sink factory that connects each capture stream to Soniox.
    pub fn begin(
        &mut self,
        settings: &Settings,
        sources: &[Source],
        out_dir: &Path,
        ctx: &egui::Context,
    ) -> Result<SinkFactory> {
        std::fs::create_dir_all(out_dir)?;
        let transcript =
            Transcript::create(&out_dir.join("transcript.jsonl"), settings.translating())
                .context("creating transcript.jsonl")?;
        self.current += 1;
        let run_id = self.current;
        self.runs.insert(
            run_id,
            Run {
                transcript: Arc::new(Mutex::new(transcript)),
                streams: sources.iter().map(|&s| (s, Status::Connecting)).collect(),
                drops: HashMap::new(),
            },
        );

        let session = out_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let configs: HashMap<Source, soniox::Config> = sources
            .iter()
            .map(|&s| (s, settings.config(s, format!("{session}/{s:?}"))))
            .collect();
        let handle = self.runtime.handle().clone();
        let events = self.events_tx.clone();
        let ctx = ctx.clone();

        Ok(Box::new(
            move |source: Source, format: StreamFormat| -> Option<AudioSink> {
                let config = configs.get(&source)?.clone();
                let input = soniox::InputFormat {
                    sample_rate: format.sample_rate,
                    channels: format.channels,
                };
                let on_event = {
                    let (events, ctx) = (events.clone(), ctx.clone());
                    move |event| {
                        let _ = events.send((run_id, source, event));
                        ctx.request_repaint();
                    }
                };
                match soniox::start(&handle, config, input, on_event) {
                    Ok(mut feed) => Some(Box::new(move |samples: &[f32]| feed.push(samples))),
                    Err(e) => {
                        let _ = events.send((run_id, source, Event::Failed(format!("{e:#}"))));
                        None
                    }
                }
            },
        ))
    }

    /// Shows sample lines instead of a real recording (debug builds only).
    #[cfg(debug_assertions)]
    pub fn load_demo(&mut self) {
        self.current += 1;
        self.runs.insert(
            self.current,
            Run {
                transcript: Arc::new(Mutex::new(crate::transcript::demo())),
                streams: HashMap::new(),
                drops: HashMap::new(),
            },
        );
    }

    /// Call when capture failed to start: streams that never opened will not
    /// report back, so stop waiting for them.
    pub fn abandon(&mut self) {
        if let Some(run) = self.runs.get_mut(&self.current) {
            for status in run.streams.values_mut() {
                if *status == Status::Connecting {
                    *status = Status::Finished;
                }
            }
        }
    }

    /// Applies pending Soniox events. Returns any errors to show, and whether
    /// the current transcript changed.
    pub fn poll(&mut self) -> (Vec<String>, bool) {
        let mut errors = Vec::new();
        let mut changed = false;
        while let Ok((run_id, source, event)) = self.events_rx.try_recv() {
            let Some(run) = self.runs.get_mut(&run_id) else {
                continue;
            };
            changed |= run_id == self.current;
            let mut transcript = run.transcript.lock().unwrap();
            let result = match event {
                Event::Connected => {
                    run.streams.insert(source, Status::Live);
                    Ok(())
                }
                Event::Reconnecting { attempt, reason } => {
                    let previous = run.streams.insert(source, Status::Reconnecting);
                    let drops = run.drops.entry(source).or_default();
                    if previous != Some(Status::Reconnecting) {
                        drops.count += 1;
                    }
                    drops.attempt = attempt;
                    drops.last_reason = reason;
                    // The provisional tail is re-transcribed after reconnecting.
                    transcript.apply(source, &[], vec![])
                }
                Event::Tokens {
                    finals,
                    provisional,
                } => transcript.apply(source, &finals, provisional),
                Event::Finished => {
                    run.streams.insert(source, Status::Finished);
                    transcript.finish(source)
                }
                Event::Failed(message) => {
                    run.streams.insert(source, Status::Failed);
                    errors.push(format!("Soniox, {}: {message}", source.label()));
                    transcript.finish(source)
                }
            };
            if let Err(e) = result {
                errors.push(format!("Writing transcript: {e}"));
            }
        }
        let current = self.current;
        self.runs.retain(|&id, run| id == current || !run.done());
        (errors, changed)
    }

    pub fn status_ui(&self, ui: &mut egui::Ui) {
        let Some(run) = self.runs.get(&self.current) else {
            return;
        };
        for source in [Source::Mic, Source::System] {
            let Some(status) = run.streams.get(&source) else {
                continue;
            };
            let drops = run.drops.get(&source);
            let (text, color) = match status {
                Status::Connecting => ("connecting…".to_owned(), ui.visuals().weak_text_color()),
                Status::Live => match drops {
                    Some(d) => (
                        format!("live · reconnected {}×", d.count),
                        egui::Color32::from_rgb(60, 170, 90),
                    ),
                    None => ("live".to_owned(), egui::Color32::from_rgb(60, 170, 90)),
                },
                Status::Reconnecting => (
                    format!("reconnecting… (try {})", drops.map_or(1, |d| d.attempt)),
                    ui.visuals().warn_fg_color,
                ),
                Status::Finished => ("done".to_owned(), ui.visuals().weak_text_color()),
                Status::Failed => ("failed".to_owned(), ui.visuals().error_fg_color),
            };
            ui.label(format!("{}:", source.label()));
            let response = ui.colored_label(color, text);
            if let Some(d) = drops {
                response.on_hover_text(format!("Last connection drop: {}", d.last_reason));
            }
        }
    }
}
