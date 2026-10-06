//! Merges the "me" and "them" Soniox streams into one transcript: finished
//! segments in time order, plus one live line per stream that may still change.
//! With translation on, each segment also collects its translated text.
//!
//! `transcript.jsonl` holds one record per line. Spoken lines have no `type`
//! field; other records are `{"type": "marker"}` (a note such as a dropped
//! connection), `{"type": "gap"}` (audio the live stream never transcribed)
//! and `{"type": "fill"}` (a range later transcribed from the recording).
//! Times are milliseconds from the start of the recording.

use std::{
    collections::HashMap,
    fs::File,
    io::{self, BufWriter, Write},
    path::Path,
};

use capture::Source;
use eframe::egui::{self, Color32, FontId, TextFormat, text::LayoutJob};
use soniox::Token;

/// A pause this long between final tokens starts a new segment. The "them"
/// stream runs without endpoint detection (it hurts diarization), so pauses
/// and speaker changes are what split its lines.
const GAP_MS: u64 = 1500;

#[derive(Clone, Debug)]
pub struct Segment {
    id: u64,
    pub source: Source,
    pub speaker: Option<String>,
    pub language: Option<String>,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    /// Translated text, empty when translation is off or the segment was
    /// already in the target language.
    pub translation: String,
    /// Transcribed afterwards from the recording, not live.
    pub filled: bool,
}

impl Segment {
    /// The line as stored in `transcript.jsonl`.
    pub fn record(&self) -> serde_json::Value {
        let mut line = serde_json::json!({
            "source": source_name(self.source),
            "speaker": self.speaker,
            "language": self.language,
            "start_ms": self.start_ms,
            "end_ms": self.end_ms,
            "text": self.text,
        });
        let translation = self.translation.trim();
        if !translation.is_empty() {
            line["translation"] = translation.into();
        }
        if self.filled {
            line["filled"] = true.into();
        }
        line
    }
}

/// A note in the timeline, such as a dropped connection.
#[derive(Clone, Debug)]
struct Marker {
    source: Source,
    at_ms: u64,
    text: String,
}

pub fn source_name(source: Source) -> &'static str {
    match source {
        Source::Mic => "mic",
        Source::System => "system",
    }
}

pub fn parse_source(name: &str) -> Option<Source> {
    match name {
        "mic" => Some(Source::Mic),
        "system" => Some(Source::System),
        _ => None,
    }
}

/// `mm:ss`, or `h:mm:ss` past an hour.
pub fn clock(ms: u64) -> String {
    let secs = ms / 1000;
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    } else {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    }
}

impl Segment {
    fn label(source: Source, speaker: Option<&str>) -> String {
        match (source, speaker) {
            (Source::Mic, _) => "Me".to_owned(),
            // Filled lines use F1, F2… (see `fill`).
            (Source::System, Some(speaker)) if speaker.starts_with('F') => {
                format!("Them · {speaker}")
            }
            (Source::System, Some(speaker)) => format!("Them · S{speaker}"),
            (Source::System, None) => "Them".to_owned(),
        }
    }
}

#[derive(Default)]
struct Lane {
    /// Final tokens since the last segment break.
    open: Option<Segment>,
    provisional: Vec<Token>,
    provisional_translation: Vec<Token>,
    /// The last closed segment, held back from the log because its
    /// translation can still be arriving (translation lags the speech).
    unwritten: Option<u64>,
}

#[derive(Default)]
pub struct Transcript {
    segments: Vec<Segment>,
    markers: Vec<Marker>,
    lanes: HashMap<Source, Lane>,
    log: Option<BufWriter<File>>,
    translating: bool,
    next_id: u64,
    /// Shown above the transcript, e.g. "live transcript paused".
    pub banner: Option<String>,
}

impl Transcript {
    /// Starts an empty transcript that appends each finished segment to
    /// `path` as a JSON line.
    pub fn create(path: &Path, translating: bool) -> io::Result<Self> {
        Ok(Self {
            log: Some(BufWriter::new(File::create(path)?)),
            translating,
            ..Default::default()
        })
    }

    /// Reads a finished recording's transcript for viewing.
    pub fn load(path: &Path) -> io::Result<Self> {
        let mut t = Transcript::default();
        for line in std::fs::read_to_string(path)?.lines() {
            let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(source) = record["source"].as_str().and_then(parse_source) else {
                continue;
            };
            let text = |key: &str| record[key].as_str().unwrap_or_default().to_owned();
            let ms = |key: &str| record[key].as_u64().unwrap_or_default();
            match record["type"].as_str() {
                None | Some("line") => {
                    t.next_id += 1;
                    t.segments.push(Segment {
                        id: t.next_id,
                        source,
                        speaker: record["speaker"].as_str().map(str::to_owned),
                        language: record["language"].as_str().map(str::to_owned),
                        start_ms: ms("start_ms"),
                        end_ms: ms("end_ms"),
                        text: text("text"),
                        translation: text("translation"),
                        filled: record["filled"].as_bool().unwrap_or(false),
                    });
                }
                Some("marker") => t.markers.push(Marker {
                    source,
                    at_ms: ms("at_ms"),
                    text: text("text"),
                }),
                _ => {}
            }
        }
        t.segments.sort_by_key(|s| s.start_ms);
        t.markers.sort_by_key(|m| m.at_ms);
        Ok(t)
    }

    /// Adds a note to the timeline and the log.
    pub fn add_marker(&mut self, source: Source, at_ms: u64, text: String) -> io::Result<()> {
        if let Some(log) = &mut self.log {
            let record = serde_json::json!({
                "type": "marker", "source": source_name(source), "at_ms": at_ms, "text": text,
            });
            writeln!(log, "{record}")?;
            log.flush()?;
        }
        let at = self.markers.partition_point(|m| m.at_ms <= at_ms);
        self.markers.insert(
            at,
            Marker {
                source,
                at_ms,
                text,
            },
        );
        Ok(())
    }

    /// Records audio the live stream won't transcribe; `to_ms: None` means
    /// to the end of the recording. The gap-filler reads these back.
    pub fn add_gap(&mut self, source: Source, from_ms: u64, to_ms: Option<u64>) -> io::Result<()> {
        if let Some(log) = &mut self.log {
            let record = serde_json::json!({
                "type": "gap", "source": source_name(source), "from_ms": from_ms, "to_ms": to_ms,
            });
            writeln!(log, "{record}")?;
        }
        let text = match to_ms {
            Some(to) => format!(
                "{} to {} not transcribed live · fill it from Recordings",
                clock(from_ms),
                clock(to)
            ),
            None => "live transcript stopped here · fill the rest from Recordings".to_owned(),
        };
        self.add_marker(source, from_ms, text)
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
            && self.markers.is_empty()
            && self.lanes.values().all(|lane| {
                lane.open.is_none()
                    && lane.provisional.is_empty()
                    && lane.provisional_translation.is_empty()
            })
    }

    pub fn apply(
        &mut self,
        source: Source,
        finals: &[Token],
        provisional: Vec<Token>,
    ) -> io::Result<()> {
        for token in finals {
            if token.is_translation() {
                self.add_translation(source, token);
                continue;
            }
            if token.is_marker() {
                self.close(source)?;
                continue;
            }
            let lane = self.lanes.entry(source).or_default();
            let breaks = lane.open.as_ref().is_some_and(|open| {
                open.speaker != token.speaker
                    || token.start_ms.saturating_sub(open.end_ms) >= GAP_MS
            });
            if breaks {
                self.close(source)?;
            }
            let next_id = &mut self.next_id;
            let lane = self.lanes.entry(source).or_default();
            let open = lane.open.get_or_insert_with(|| {
                *next_id += 1;
                Segment {
                    id: *next_id,
                    source,
                    speaker: token.speaker.clone(),
                    language: token.language.clone(),
                    start_ms: token.start_ms,
                    end_ms: token.end_ms,
                    text: String::new(),
                    translation: String::new(),
                    filled: false,
                }
            });
            open.text.push_str(&token.text);
            open.end_ms = open.end_ms.max(token.end_ms);
            if open.language.is_none() {
                open.language = token.language.clone();
            }
        }

        let (translated, spoken): (Vec<Token>, Vec<Token>) = provisional
            .into_iter()
            .filter(|t| !t.is_marker())
            .partition(Token::is_translation);
        let lane = self.lanes.entry(source).or_default();
        lane.provisional = spoken;
        lane.provisional_translation = translated;
        Ok(())
    }

    /// Translated tokens have no timestamps and trail the speech, so attach
    /// them to the newest segment from the same stream and speaker. That is
    /// the open one while the speaker is still talking, or the one that just
    /// closed when someone else has started.
    fn add_translation(&mut self, source: Source, token: &Token) {
        let lane = self.lanes.entry(source).or_default();
        if let Some(open) = &mut lane.open
            && open.speaker == token.speaker
        {
            open.translation.push_str(&token.text);
            return;
        }
        if let Some(segment) = self
            .segments
            .iter_mut()
            .rev()
            .find(|s| s.source == source && s.speaker == token.speaker)
        {
            segment.translation.push_str(&token.text);
            return;
        }
        if let Some(open) = &mut lane.open {
            open.translation.push_str(&token.text);
        }
    }

    /// Closes the stream's open segment once Soniox has sent everything.
    pub fn finish(&mut self, source: Source) -> io::Result<()> {
        self.close(source)?;
        if let Some(id) = self.lanes.remove(&source).and_then(|lane| lane.unwritten) {
            self.write(id)?;
        }
        if let Some(log) = &mut self.log {
            log.flush()?;
        }
        Ok(())
    }

    fn close(&mut self, source: Source) -> io::Result<()> {
        let Some(lane) = self.lanes.get_mut(&source) else {
            return Ok(());
        };
        let Some(mut segment) = lane.open.take() else {
            return Ok(());
        };
        segment.text = segment.text.trim().to_owned();
        if segment.text.is_empty() {
            return Ok(());
        }
        let id = segment.id;
        // Without translation nothing more arrives for a closed segment.
        let previous = if self.translating {
            lane.unwritten.replace(id)
        } else {
            Some(id)
        };
        // Streams arrive independently, so keep the list sorted by start time.
        let at = self
            .segments
            .partition_point(|s| s.start_ms <= segment.start_ms);
        self.segments.insert(at, segment);
        if let Some(previous) = previous {
            self.write(previous)?;
        }
        Ok(())
    }

    fn write(&mut self, id: u64) -> io::Result<()> {
        let (Some(log), Some(segment)) = (
            &mut self.log,
            self.segments.iter().rev().find(|s| s.id == id),
        ) else {
            return Ok(());
        };
        writeln!(log, "{}", segment.record())?;
        log.flush()
    }

    /// Flushes and closes `transcript.jsonl`, so the gap-filler can replace
    /// it (Windows can't replace a file that's still open).
    pub fn close_log(&mut self) {
        if let Some(mut log) = self.log.take() {
            let _ = log.flush();
        }
    }

    /// Closes every stream and hands back the finished segments, for
    /// building filled lines outside a live recording.
    pub fn into_segments(mut self) -> Vec<Segment> {
        for source in [Source::Mic, Source::System] {
            let _ = self.close(source);
        }
        self.segments
    }

    /// A segmenter for filled lines: same rules as live, no log.
    pub fn for_filling(translating: bool) -> Self {
        Transcript {
            translating,
            ..Default::default()
        }
    }

    pub fn ui(&self, ui: &mut egui::Ui, font_size: f32, show_translation: bool) {
        if let Some(banner) = &self.banner {
            ui.colored_label(ui.visuals().warn_fg_color, banner);
            ui.add_space(4.0);
        }
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                let mut markers = self.markers.iter().peekable();
                for segment in &self.segments {
                    while let Some(marker) = markers.next_if(|m| m.at_ms <= segment.start_ms) {
                        marker_line(ui, font_size, marker);
                    }
                    let speaker = segment.speaker.as_deref();
                    ui.label(line(
                        ui,
                        font_size,
                        segment.start_ms,
                        segment.source,
                        speaker,
                        segment.filled,
                        &segment.text,
                        "",
                    ));
                    if show_translation {
                        translation_line(ui, font_size, segment.id, &segment.translation, "");
                    }
                    ui.add_space(2.0);
                }
                for marker in markers {
                    marker_line(ui, font_size, marker);
                }
                for source in [Source::Mic, Source::System] {
                    let Some(lane) = self.lanes.get(&source) else {
                        continue;
                    };
                    let pending_translation: String = lane
                        .provisional_translation
                        .iter()
                        .map(|t| t.text.as_str())
                        .collect();
                    let first = lane
                        .open
                        .as_ref()
                        .map(|o| (o.start_ms, o.speaker.as_deref()));
                    let first = first.or_else(|| {
                        lane.provisional
                            .first()
                            .map(|t| (t.start_ms, t.speaker.as_deref()))
                    });
                    let settled = lane.open.as_ref().map_or("", |o| o.text.trim_start());
                    if let Some((start_ms, speaker)) = first {
                        let pending: String =
                            lane.provisional.iter().map(|t| t.text.as_str()).collect();
                        let pending = if settled.is_empty() {
                            pending.trim_start()
                        } else {
                            &pending
                        };
                        ui.label(line(
                            ui, font_size, start_ms, source, speaker, false, settled, pending,
                        ));
                    }
                    if show_translation {
                        let settled = lane.open.as_ref().map_or("", |o| o.translation.as_str());
                        let salt = u64::MAX - source as u64;
                        translation_line(ui, font_size, salt, settled, &pending_translation);
                    }
                    ui.add_space(2.0);
                }
            });
    }
}

#[allow(clippy::too_many_arguments)]
fn line(
    ui: &egui::Ui,
    font_size: f32,
    start_ms: u64,
    source: Source,
    speaker: Option<&str>,
    filled: bool,
    settled: &str,
    pending: &str,
) -> LayoutJob {
    let visuals = ui.visuals();
    let body = FontId::proportional(font_size);
    let mut job = LayoutJob::default();
    job.append(
        &format!("{}  ", clock(start_ms)),
        0.0,
        TextFormat::simple(
            FontId::monospace(font_size - 2.0),
            visuals.weak_text_color(),
        ),
    );
    job.append(
        &format!("{}: ", Segment::label(source, speaker)),
        0.0,
        TextFormat::simple(
            body.clone(),
            speaker_color(source, speaker, visuals.dark_mode),
        ),
    );
    if filled {
        job.append(
            "(filled) ",
            0.0,
            TextFormat::simple(
                FontId::proportional(font_size - 2.0),
                visuals.weak_text_color(),
            ),
        );
    }
    job.append(
        settled,
        0.0,
        TextFormat::simple(body.clone(), visuals.text_color()),
    );
    job.append(
        pending,
        0.0,
        TextFormat {
            font_id: body,
            color: visuals.weak_text_color(),
            italics: true,
            ..Default::default()
        },
    );
    job
}

/// A timeline note, e.g. "— Them: connection dropped 15:11, reconnected after 4 s —".
fn marker_line(ui: &mut egui::Ui, font_size: f32, marker: &Marker) {
    let who = match marker.source {
        Source::Mic => "Me",
        Source::System => "Them",
    };
    let text = format!("{}  — {who}: {} —", clock(marker.at_ms), marker.text);
    ui.label(
        egui::RichText::new(text)
            .size(font_size - 2.0)
            .italics()
            .color(ui.visuals().warn_fg_color),
    );
    ui.add_space(2.0);
}

/// The translation under its segment, indented and in a muted colour.
fn translation_line(ui: &mut egui::Ui, font_size: f32, salt: u64, settled: &str, pending: &str) {
    let settled = settled.trim_start();
    let pending = if settled.is_empty() {
        pending.trim_start()
    } else {
        pending
    };
    if settled.is_empty() && pending.is_empty() {
        return;
    }
    let color = if ui.visuals().dark_mode {
        Color32::from_rgb(150, 175, 200)
    } else {
        Color32::from_rgb(70, 95, 120)
    };
    let body = FontId::proportional(font_size - 1.0);
    let mut job = LayoutJob::default();
    job.append(settled, 0.0, TextFormat::simple(body.clone(), color));
    job.append(
        pending,
        0.0,
        TextFormat {
            font_id: body,
            color: ui.visuals().weak_text_color(),
            italics: true,
            ..Default::default()
        },
    );
    ui.indent(("translation", salt), |ui| ui.label(job));
}

fn speaker_color(source: Source, speaker: Option<&str>, dark: bool) -> Color32 {
    const LIGHT: [Color32; 5] = [
        Color32::from_rgb(30, 100, 200),
        Color32::from_rgb(200, 90, 20),
        Color32::from_rgb(30, 140, 70),
        Color32::from_rgb(150, 60, 170),
        Color32::from_rgb(180, 40, 70),
    ];
    const DARK: [Color32; 5] = [
        Color32::from_rgb(110, 170, 255),
        Color32::from_rgb(255, 160, 90),
        Color32::from_rgb(100, 210, 130),
        Color32::from_rgb(210, 140, 240),
        Color32::from_rgb(255, 120, 150),
    ];
    let palette = if dark { DARK } else { LIGHT };
    // "Me" gets the first colour; remote speakers cycle through the rest.
    let index = match source {
        Source::Mic => 0,
        Source::System => {
            let n: usize = speaker
                .map(|s| s.trim_start_matches('F'))
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);
            1 + n.saturating_sub(1) % 4
        }
    };
    palette[index]
}

/// Sample lines for checking rendering without recording
/// (`MEETING_TRANSCRIBER_DEMO=1 cargo run`, debug builds only).
#[cfg(debug_assertions)]
pub fn demo() -> Transcript {
    let token = |text: &str, start_ms: u64, speaker: Option<&str>, is_final: bool| -> Token {
        serde_json::from_value(serde_json::json!({
            "text": text, "start_ms": start_ms, "end_ms": start_ms + 400,
            "is_final": is_final, "speaker": speaker,
        }))
        .unwrap()
    };
    let translated = |text: &str, speaker: Option<&str>, is_final: bool| -> Token {
        serde_json::from_value(serde_json::json!({
            "text": text, "is_final": is_final, "speaker": speaker,
            "translation_status": "translation",
        }))
        .unwrap()
    };
    let mut t = Transcript {
        translating: true,
        ..Default::default()
    };
    let lines = [
        (
            Source::System,
            Some("1"),
            0,
            "Chào đồng bào, nhân dân Tổ quốc Việt Nam, các đồng chí lão thành cách mạng.",
            "Greetings to our compatriots, the people of Vietnam, and the veteran revolutionaries.",
        ),
        (
            Source::Mic,
            None,
            6_000,
            "Mình nghe rõ, tiếp tục đi anh.",
            "I can hear you clearly, please go on.",
        ),
        (
            Source::System,
            Some("2"),
            9_000,
            "Next item: the Q3 roadmap and the ClickUp migration.",
            "Mục tiếp theo: lộ trình quý 3 và việc chuyển sang ClickUp.",
        ),
    ];
    for (source, speaker, start, text, translation) in lines {
        let _ = t.apply(
            source,
            &[
                token(text, start, speaker, true),
                translated(translation, speaker, true),
            ],
            vec![],
        );
        let _ = t.finish(source);
    }
    let _ = t.apply(
        Source::System,
        &[
            token("Quyết định cuối cùng", 15_000, Some("1"), true),
            translated("The final decision", Some("1"), true),
        ],
        vec![
            token(
                " là chúng ta sẽ triển khai vào tuần sau",
                16_000,
                Some("1"),
                false,
            ),
            translated(" is that we will roll out next week", Some("1"), false),
        ],
    );
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(text: &str, start_ms: u64, end_ms: u64, speaker: Option<&str>) -> Token {
        serde_json::from_value(serde_json::json!({
            "text": text, "start_ms": start_ms, "end_ms": end_ms,
            "is_final": true, "speaker": speaker,
        }))
        .unwrap()
    }

    fn translated(text: &str, speaker: Option<&str>) -> Token {
        serde_json::from_value(serde_json::json!({
            "text": text, "is_final": true, "speaker": speaker,
            "translation_status": "translation",
        }))
        .unwrap()
    }

    #[test]
    fn splits_on_endpoint_speaker_change_and_pause() {
        let mut t = Transcript::default();
        t.apply(
            Source::Mic,
            &[
                token(" Hello", 0, 300, None),
                token(" there", 300, 600, None),
                token("<end>", 0, 0, None),
            ],
            vec![],
        )
        .unwrap();
        t.apply(
            Source::System,
            &[
                token("Hi", 100, 400, Some("1")),
                token(" Bao", 500, 800, Some("2")),
                token(" again", 5000, 5300, Some("2")),
            ],
            vec![],
        )
        .unwrap();
        t.finish(Source::System).unwrap();

        let lines: Vec<_> = t
            .segments
            .iter()
            .map(|s| (s.start_ms, s.text.as_str()))
            .collect();
        assert_eq!(
            lines,
            [
                (0, "Hello there"),
                (100, "Hi"),
                (500, "Bao"),
                (5000, "again")
            ]
        );
    }

    #[test]
    fn provisional_tokens_replace_each_other() {
        let mut t = Transcript::default();
        t.apply(Source::Mic, &[], vec![token(" Hel", 0, 100, None)])
            .unwrap();
        t.apply(Source::Mic, &[], vec![token(" Hello", 0, 200, None)])
            .unwrap();
        assert_eq!(t.lanes[&Source::Mic].provisional.len(), 1);
        assert!(t.segments.is_empty());
    }

    #[test]
    fn late_translation_goes_to_its_speakers_segment_and_the_log() {
        let path =
            std::env::temp_dir().join(format!("transcript-test-{}.jsonl", std::process::id()));
        let mut t = Transcript::create(&path, true).unwrap();
        // Speaker 1 talks, speaker 2 starts before speaker 1's translation lands.
        t.apply(
            Source::System,
            &[
                token("Xin chào", 0, 500, Some("1")),
                token(" Thanks", 600, 900, Some("2")),
                translated("Hello", Some("1")),
                translated(" Cảm ơn", Some("2")),
            ],
            vec![],
        )
        .unwrap();
        t.finish(Source::System).unwrap();

        let pairs: Vec<_> = t
            .segments
            .iter()
            .map(|s| (s.text.as_str(), s.translation.trim()))
            .collect();
        assert_eq!(pairs, [("Xin chào", "Hello"), ("Thanks", "Cảm ơn")]);

        let log = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let lines: Vec<serde_json::Value> = log
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["translation"], "Hello");
        assert_eq!(lines[1]["translation"], "Cảm ơn");
    }

    #[test]
    fn provisional_translation_is_kept_apart_from_speech() {
        let mut t = Transcript::default();
        t.apply(
            Source::Mic,
            &[],
            vec![token(" Chào", 0, 100, None), translated(" Hi", None)],
        )
        .unwrap();
        let lane = &t.lanes[&Source::Mic];
        assert_eq!(lane.provisional.len(), 1);
        assert_eq!(lane.provisional_translation.len(), 1);
    }
}
