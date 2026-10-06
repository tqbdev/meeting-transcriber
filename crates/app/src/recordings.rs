//! The Recordings list: past recordings, how much of each the live
//! transcript missed, and filling that from the recording.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::mpsc,
};

use eframe::egui;
use soniox::files::{Leftover, Stage};

use crate::{
    fill::{self, FillOptions, FillOutcome, Gap, Progress},
    platform,
};

enum GapState {
    Unknown,
    Checking,
    Found(Vec<Gap>),
    Failed(String),
}

enum FillState {
    Idle,
    Running(String),
    Done(String),
    Failed(String),
}

struct Item {
    dir: PathBuf,
    name: String,
    minutes: f64,
    gaps: GapState,
    fill: FillState,
}

enum FillEvent {
    Progress(Progress),
    Finished(FillOutcome),
}

/// What the list asks the app to do.
pub enum Action {
    View(PathBuf),
}

/// Things the app should handle after [`Recordings::poll`].
#[derive(Default)]
pub struct Changes {
    /// Still stored on Soniox; add to the retry list.
    pub leftovers: Vec<Leftover>,
    /// Recordings whose transcript was just filled.
    pub filled: Vec<PathBuf>,
    pub errors: Vec<String>,
}

pub struct Recordings {
    root: PathBuf,
    runtime: tokio::runtime::Handle,
    ctx: egui::Context,
    items: Vec<Item>,
    checks_tx: mpsc::Sender<(PathBuf, Result<Vec<Gap>, String>)>,
    checks_rx: mpsc::Receiver<(PathBuf, Result<Vec<Gap>, String>)>,
    fills_tx: mpsc::Sender<(PathBuf, FillEvent)>,
    fills_rx: mpsc::Receiver<(PathBuf, FillEvent)>,
    checking: bool,
    /// Recordings to fill without asking once their check finishes.
    auto: HashMap<PathBuf, FillOptions>,
    confirm: Option<(PathBuf, FillOptions)>,
}

impl Recordings {
    pub fn new(root: PathBuf, runtime: tokio::runtime::Handle, ctx: egui::Context) -> Self {
        let (checks_tx, checks_rx) = mpsc::channel();
        let (fills_tx, fills_rx) = mpsc::channel();
        let mut recordings = Self {
            root,
            runtime,
            ctx,
            items: Vec::new(),
            checks_tx,
            checks_rx,
            fills_tx,
            fills_rx,
            checking: false,
            auto: HashMap::new(),
            confirm: None,
        };
        recordings.refresh();
        recordings
    }

    /// Re-reads the folder list, keeping what's known about each recording.
    pub fn refresh(&mut self) {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            self.items.clear();
            return;
        };
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        // Folder names are timestamps, so this is newest first.
        dirs.sort_unstable_by(|a, b| b.cmp(a));
        let mut old: HashMap<PathBuf, Item> =
            self.items.drain(..).map(|i| (i.dir.clone(), i)).collect();
        self.items = dirs
            .into_iter()
            .map(|dir| {
                old.remove(&dir).unwrap_or_else(|| Item {
                    name: dir
                        .file_name()
                        .map(|n| n.to_string_lossy().replace('_', " "))
                        .unwrap_or_default(),
                    minutes: wav_minutes(&dir),
                    dir,
                    gaps: GapState::Unknown,
                    fill: FillState::Idle,
                })
            })
            .collect();
    }

    /// Checks `dir` again, e.g. after its recording finished.
    pub fn recheck(&mut self, dir: &Path) {
        self.refresh();
        if let Some(item) = self.items.iter_mut().find(|i| i.dir == dir) {
            item.minutes = wav_minutes(dir);
            item.gaps = GapState::Unknown;
        }
    }

    /// Checks `dir`, then fills whatever is missing without asking.
    pub fn auto_fill(&mut self, dir: &Path, options: FillOptions) {
        self.auto.insert(dir.to_owned(), options);
        self.recheck(dir);
    }

    fn start_checks(&mut self, busy: Option<&Path>) {
        if self.checking {
            return;
        }
        let dirs: Vec<PathBuf> = self
            .items
            .iter_mut()
            .filter(|i| matches!(i.gaps, GapState::Unknown) && Some(i.dir.as_path()) != busy)
            .map(|i| {
                i.gaps = GapState::Checking;
                i.dir.clone()
            })
            .collect();
        if dirs.is_empty() {
            return;
        }
        self.checking = true;
        let (tx, ctx) = (self.checks_tx.clone(), self.ctx.clone());
        // One at a time: each check reads both WAV files once.
        std::thread::spawn(move || {
            for dir in dirs {
                let result = fill::find_gaps(&dir).map_err(|e| format!("{e:#}"));
                let _ = tx.send((dir, result));
                ctx.request_repaint();
            }
        });
    }

    fn start_fill(&mut self, dir: PathBuf, gaps: Vec<Gap>, options: FillOptions) {
        if let Some(item) = self.items.iter_mut().find(|i| i.dir == dir) {
            item.fill = FillState::Running("starting…".to_owned());
        }
        let (tx, ctx) = (self.fills_tx.clone(), self.ctx.clone());
        let task_dir = dir.clone();
        self.runtime.spawn(async move {
            let progress = {
                let (tx, ctx, dir) = (tx.clone(), ctx.clone(), task_dir.clone());
                move |p| {
                    let _ = tx.send((dir.clone(), FillEvent::Progress(p)));
                    ctx.request_repaint();
                }
            };
            let outcome = fill::fill(task_dir.clone(), gaps, options, progress).await;
            let _ = tx.send((task_dir, FillEvent::Finished(outcome)));
            ctx.request_repaint();
        });
    }

    pub fn poll(&mut self) -> Changes {
        let mut changes = Changes::default();
        while let Ok((dir, result)) = self.checks_rx.try_recv() {
            let auto = self.auto.remove(&dir);
            if let Some(item) = self.items.iter_mut().find(|i| i.dir == dir) {
                item.gaps = match result {
                    Ok(gaps) => GapState::Found(gaps),
                    Err(e) => GapState::Failed(e),
                };
            }
            if let (Some(options), Some(gaps)) = (auto, self.gaps(&dir))
                && !gaps.is_empty()
            {
                let gaps = gaps.to_vec();
                self.start_fill(dir, gaps, options);
            }
        }
        if !self
            .items
            .iter()
            .any(|i| matches!(i.gaps, GapState::Checking))
        {
            self.checking = false;
        }
        while let Ok((dir, event)) = self.fills_rx.try_recv() {
            let Some(item) = self.items.iter_mut().find(|i| i.dir == dir) else {
                continue;
            };
            match event {
                FillEvent::Progress(Progress {
                    index,
                    count,
                    gap,
                    stage,
                }) => {
                    let stage = match stage {
                        Stage::Uploading => "uploading",
                        Stage::Queued => "queued at Soniox",
                        Stage::Transcribing => "transcribing",
                        Stage::Downloading => "downloading",
                    };
                    item.fill = FillState::Running(format!(
                        "{} of {count}: {} · {stage}…",
                        index + 1,
                        fill::gap_label(&gap)
                    ));
                }
                FillEvent::Finished(outcome) => {
                    changes.leftovers.extend(outcome.leftovers);
                    changes.filled.push(dir.clone());
                    item.fill = match outcome.lines {
                        Ok(lines) => FillState::Done(format!("filled · {lines} lines added")),
                        Err(e) => {
                            let message = format!("{e:#}");
                            changes
                                .errors
                                .push(format!("Filling {}: {message}", item.name));
                            FillState::Failed(message)
                        }
                    };
                    // Whatever is still missing now.
                    item.gaps = GapState::Unknown;
                }
            }
        }
        changes
    }

    fn gaps(&self, dir: &Path) -> Option<&[Gap]> {
        self.items
            .iter()
            .find(|i| i.dir == dir)
            .and_then(|i| match &i.gaps {
                GapState::Found(gaps) => Some(gaps.as_slice()),
                _ => None,
            })
    }

    /// `busy` is the recording still in progress, which can't be filled yet.
    /// `options` builds the fill settings for a recording when asked.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        busy: Option<&Path>,
        has_key: bool,
        options: impl Fn(&Path) -> FillOptions,
    ) -> Option<Action> {
        self.start_checks(busy);
        let mut action = None;
        let mut fill_request = None;
        if self.items.is_empty() {
            ui.weak("No recordings yet.");
        }
        egui::ScrollArea::vertical()
            .id_salt("recordings")
            .max_height(180.0)
            .show(ui, |ui| {
                egui::Grid::new("recordings-grid")
                    .num_columns(4)
                    .spacing([12.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for item in &self.items {
                            let is_busy = Some(item.dir.as_path()) == busy;
                            ui.label(&item.name);
                            ui.weak(format!("{:.0} min", item.minutes));
                            match (&item.fill, &item.gaps) {
                                (FillState::Running(stage), _) => {
                                    ui.colored_label(ui.visuals().warn_fg_color, stage);
                                }
                                _ if is_busy => {
                                    ui.weak("recording…");
                                }
                                (_, GapState::Unknown | GapState::Checking) => {
                                    ui.weak("checking…");
                                }
                                (_, GapState::Failed(e)) => {
                                    ui.weak("couldn't check").on_hover_text(e);
                                }
                                (fill_state, GapState::Found(gaps)) if gaps.is_empty() => {
                                    match fill_state {
                                        FillState::Done(text) => ui.weak(text),
                                        _ => ui.weak("complete"),
                                    };
                                }
                                (fill_state, GapState::Found(gaps)) => {
                                    let text = format!("missing: {}", fill::describe(gaps));
                                    let response =
                                        ui.colored_label(ui.visuals().warn_fg_color, text);
                                    if let FillState::Failed(e) = fill_state {
                                        response.on_hover_text(format!("Last fill failed: {e}"));
                                    }
                                }
                            }
                            ui.horizontal(|ui| {
                                if ui.small_button("View").clicked() {
                                    action = Some(Action::View(item.dir.clone()));
                                }
                                let can_fill = !is_busy
                                    && matches!(&item.gaps, GapState::Found(g) if !g.is_empty())
                                    && !matches!(item.fill, FillState::Running(_));
                                if can_fill
                                    && ui
                                        .add_enabled(
                                            has_key,
                                            egui::Button::new("Fill gaps…").small(),
                                        )
                                        .on_disabled_hover_text("Add a Soniox API key first")
                                        .clicked()
                                {
                                    fill_request = Some(item.dir.clone());
                                }
                                if ui.small_button(platform::REVEAL_LABEL).clicked() {
                                    platform::reveal(&item.dir);
                                }
                            });
                            ui.end_row();
                        }
                    });
            });
        if let Some(dir) = fill_request {
            let opts = options(&dir);
            self.confirm = Some((dir, opts));
        }
        self.confirm_ui(ui.ctx());
        action
    }

    fn confirm_ui(&mut self, ctx: &egui::Context) {
        let Some((dir, options)) = &self.confirm else {
            return;
        };
        let Some(gaps) = self.gaps(dir).map(<[Gap]>::to_vec) else {
            self.confirm = None;
            return;
        };
        let translating = options.meta.translation.is_some();
        let mut decision = None;
        egui::Window::new("Fill gaps")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("These parts have no transcript:");
                for gap in &gaps {
                    ui.label(format!("  • {}", fill::gap_label(gap)));
                }
                ui.add_space(4.0);
                ui.label(format!(
                    "{:.1} min of audio, about ${:.2}{}.",
                    fill::total_minutes(&gaps),
                    fill::cost_usd(&gaps, translating),
                    if translating { " with translation" } else { "" }
                ));
                ui.weak(
                    "Only these parts are uploaded to Soniox, and deleted there once \
                     transcribed. Filled lines are marked, and the live transcript is kept \
                     as transcript.live.jsonl.",
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Fill").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });
        match decision {
            Some(true) => {
                if let Some((dir, options)) = self.confirm.take() {
                    self.start_fill(dir, gaps, options);
                }
            }
            Some(false) => self.confirm = None,
            None => {}
        }
    }
}

/// Length of the longer track, from the WAV headers.
fn wav_minutes(dir: &Path) -> f64 {
    ["system.wav", "mic.wav"]
        .iter()
        .filter_map(|name| hound::WavReader::open(dir.join(name)).ok())
        .map(|r| r.duration() as f64 / r.spec().sample_rate as f64 / 60.0)
        .fold(0.0, f64::max)
}
