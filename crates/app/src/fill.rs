//! Fills the parts of a recording the live transcript missed, from the WAV
//! files, with Soniox's file (async) API. Only the missing time is sent.
//!
//! A gap is either recorded by the live stream (`{"type": "gap"}` in
//! `transcript.jsonl`: a stream that gave up, or an outage too long to
//! resend) or found by scanning: a long stretch with speech-level audio but
//! no transcript lines, which catches stalls in recordings made before gaps
//! were recorded.

use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use capture::Source;
use serde::{Deserialize, Serialize};
use soniox::{Token, Translation, files};

use crate::transcript::{Transcript, clock, parse_source, source_name};

/// Audio around each gap sent along, so words at the edges aren't cut.
/// Only words starting inside the gap are kept.
const PAD_MS: u64 = 3_000;
/// Loudness is measured in windows of this length.
const WINDOW_MS: u64 = 10_000;
/// A window this loud counts as speech (speech is about -20 to -35 dBFS,
/// room noise below -60).
const SPEECH_DB: f32 = -45.0;
/// An untranscribed stretch this long is a gap if it holds this much
/// speech. Not "mostly speech": you may talk only now and then (a mic track
/// is often silent over half the meeting), and those words still count.
const MIN_SPAN_MS: u64 = 60_000;
const MIN_SPAN_SPEECH_MS: u64 = 20_000;
/// After the last transcribed line, a shorter stretch with less speech
/// already counts: that's what a stream that stopped early looks like.
const MIN_TAIL_MS: u64 = 30_000;
const MIN_TAIL_SPEECH_MS: u64 = 10_000;
/// Gaps shorter than this aren't worth a request.
const MIN_GAP_MS: u64 = 2_000;

/// About $0.10 per hour of audio (async price), plus output text: roughly
/// $0.05 per hour of speech, doubled when translating.
const USD_PER_AUDIO_HOUR: f64 = 0.045;
const USD_PER_TEXT_HOUR: f64 = 0.0525;

/// What a recording was made with, saved as `recording.json` so filling
/// uses the same languages and translation even if settings changed since.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingMeta {
    pub started_at: String,
    pub build: String,
    pub language_hints: Vec<String>,
    pub terms: Vec<String>,
    pub translation: Option<Translation>,
}

impl RecordingMeta {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join("recording.json")
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        fs::write(Self::path(dir), serde_json::to_string_pretty(self)?)
            .context("writing recording.json")
    }

    /// `None` for recordings made before `recording.json` existed.
    pub fn load(dir: &Path) -> Option<Self> {
        let json = fs::read_to_string(Self::path(dir)).ok()?;
        serde_json::from_str(&json).ok()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gap {
    pub source: Source,
    pub from_ms: u64,
    pub to_ms: u64,
}

impl Gap {
    pub fn len_ms(&self) -> u64 {
        self.to_ms - self.from_ms
    }
}

pub fn total_minutes(gaps: &[Gap]) -> f64 {
    gaps.iter().map(Gap::len_ms).sum::<u64>() as f64 / 60_000.0
}

pub fn cost_usd(gaps: &[Gap], translating: bool) -> f64 {
    let padded_ms: u64 = gaps.iter().map(|g| g.len_ms() + 2 * PAD_MS).sum();
    let hours = padded_ms as f64 / 3_600_000.0;
    let text = if translating { 2.0 } else { 1.0 };
    hours * (USD_PER_AUDIO_HOUR + USD_PER_TEXT_HOUR * text)
}

fn wav_path(dir: &Path, source: Source) -> PathBuf {
    dir.join(format!("{}.wav", source_name(source)))
}

fn transcript_path(dir: &Path) -> PathBuf {
    dir.join("transcript.jsonl")
}

/// The records of `transcript.jsonl`, or none if it doesn't exist.
fn read_records(dir: &Path) -> Result<Vec<serde_json::Value>> {
    let path = transcript_path(dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = fs::File::open(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect())
}

/// Finds the missing time per track. Reads the WAV files once to measure
/// loudness, so run it off the UI thread.
pub fn find_gaps(dir: &Path) -> Result<Vec<Gap>> {
    let records = read_records(dir)?;
    let ms = |r: &serde_json::Value, key: &str| r[key].as_u64();

    struct Track {
        source: Source,
        loudness: Loudness,
        /// Time already transcribed, live or by an earlier fill.
        covered: Vec<(u64, u64)>,
        /// Gaps the live stream recorded itself.
        recorded: Vec<(u64, u64)>,
    }
    let mut tracks = Vec::new();
    for source in [Source::Mic, Source::System] {
        let wav = wav_path(dir, source);
        if !wav.exists() {
            continue;
        }
        let loudness = measure_loudness(&wav)?;
        let of_source =
            |r: &&serde_json::Value| r["source"].as_str().and_then(parse_source) == Some(source);
        let covered = records
            .iter()
            .filter(of_source)
            .filter_map(|r| match r["type"].as_str() {
                None | Some("line") => Some((ms(r, "start_ms")?, ms(r, "end_ms")?)),
                Some("fill") => Some((ms(r, "from_ms")?, ms(r, "to_ms")?)),
                _ => None,
            })
            .collect();
        let recorded = records
            .iter()
            .filter(of_source)
            .filter(|r| r["type"] == "gap")
            .filter_map(|r| {
                let from = ms(r, "from_ms")?;
                Some((from, ms(r, "to_ms").unwrap_or(loudness.duration_ms)))
            })
            .collect();
        tracks.push(Track {
            source,
            loudness,
            covered,
            recorded,
        });
    }

    let mut gaps = Vec::new();
    for track in &tracks {
        let duration_ms = track.loudness.duration_ms;
        let other = tracks.iter().find(|t| t.source != track.source);
        let mut found: Vec<(u64, u64)> = track
            .recorded
            .iter()
            .flat_map(|&(from, to)| subtract(&[(from, to.min(duration_ms))], &track.covered))
            .collect();

        for (from, to) in subtract(&[(0, duration_ms)], &track.covered) {
            let (min_len, min_speech) = if to >= duration_ms {
                (MIN_TAIL_MS, MIN_TAIL_SPEECH_MS)
            } else {
                (MIN_SPAN_MS, MIN_SPAN_SPEECH_MS)
            };
            if to - from < min_len || track.loudness.speech_ms(from, to) < min_speech {
                continue;
            }
            // A stall stops both streams, which share the network. If the
            // other track was transcribed meanwhile, this track was fine
            // too, and the "speech" is likely the other side leaking into
            // the mic (open-ear headphones) or noise, not missing words.
            let other_covered = other.map_or(0, |o| overlap_ms(from, to, &o.covered));
            if other.is_some() && other_covered * 10 > to - from {
                continue;
            }
            found.push((from, to));
        }

        for (from_ms, to_ms) in merge(found) {
            if to_ms - from_ms >= MIN_GAP_MS {
                gaps.push(Gap {
                    source: track.source,
                    from_ms,
                    to_ms,
                });
            }
        }
    }
    Ok(gaps)
}

/// How much of `from..to` the ranges cover.
fn overlap_ms(from: u64, to: u64, ranges: &[(u64, u64)]) -> u64 {
    let clipped: Vec<(u64, u64)> = ranges
        .iter()
        .filter_map(|&(a, b)| {
            let (a, b) = (a.max(from), b.min(to));
            (a < b).then_some((a, b))
        })
        .collect();
    merge(clipped).iter().map(|(a, b)| b - a).sum()
}

/// `ranges` minus `holes`, both as `(from, to)` in ms.
fn subtract(ranges: &[(u64, u64)], holes: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut holes = holes.to_vec();
    holes.sort_unstable();
    let mut out = Vec::new();
    for &(from, to) in ranges {
        let mut start = from;
        for &(hole_from, hole_to) in &holes {
            if hole_to <= start || hole_from >= to {
                continue;
            }
            if hole_from > start {
                out.push((start, hole_from));
            }
            start = start.max(hole_to);
        }
        if start < to {
            out.push((start, to));
        }
    }
    out
}

/// Joins overlapping or touching ranges.
fn merge(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    ranges.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (from, to) in ranges {
        match out.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    out
}

struct Loudness {
    duration_ms: u64,
    /// RMS in dBFS per window.
    windows: Vec<f32>,
}

impl Loudness {
    /// Roughly how much of `from_ms..to_ms` is speech-level audio.
    fn speech_ms(&self, from_ms: u64, to_ms: u64) -> u64 {
        let first = (from_ms / WINDOW_MS) as usize;
        let last = to_ms.div_ceil(WINDOW_MS) as usize;
        let windows = &self.windows[first.min(self.windows.len())..last.min(self.windows.len())];
        windows.iter().filter(|&&db| db > SPEECH_DB).count() as u64 * WINDOW_MS
    }
}

fn measure_loudness(path: &Path) -> Result<Loudness> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = reader.spec();
    let rate = spec.sample_rate as u64;
    let duration_ms = reader.duration() as u64 * 1000 / rate;
    let per_window = (rate * WINDOW_MS / 1000) as usize * spec.channels as usize;
    let mut windows = Vec::new();
    let (mut sum, mut count) = (0f64, 0usize);
    let mut add = |sample: f32| {
        sum += (sample as f64).powi(2);
        count += 1;
        if count == per_window {
            windows.push(db(sum, count));
            (sum, count) = (0.0, 0);
        }
    };
    match spec.sample_format {
        hound::SampleFormat::Float => {
            for sample in reader.samples::<f32>() {
                add(sample?);
            }
        }
        hound::SampleFormat::Int => {
            let scale = (1u64 << (spec.bits_per_sample - 1)) as f32;
            for sample in reader.samples::<i32>() {
                add(sample? as f32 / scale);
            }
        }
    }
    if count > 0 {
        windows.push(db(sum, count));
    }
    Ok(Loudness {
        duration_ms,
        windows,
    })
}

fn db(sum_of_squares: f64, count: usize) -> f32 {
    let rms = (sum_of_squares / count as f64).sqrt();
    (20.0 * rms.max(1e-5).log10()) as f32
}

/// Reads `from_ms..to_ms` of a WAV file as interleaved f32.
fn read_clip(path: &Path, from_ms: u64, to_ms: u64) -> Result<(Vec<f32>, u32, u16)> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    let rate = spec.sample_rate as u64;
    let first = (from_ms * rate / 1000) as u32;
    let frames = ((to_ms - from_ms) * rate / 1000) as usize;
    reader.seek(first.min(reader.duration()))?;
    let wanted = frames * spec.channels as usize;
    let samples = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .take(wanted)
            .collect::<Result<Vec<_>, _>>()?,
        hound::SampleFormat::Int => {
            let scale = (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .take(wanted)
                .map(|s| s.map(|s| s as f32 / scale))
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    Ok((samples, spec.sample_rate, spec.channels))
}

/// Settings for filling, from `recording.json` or the current settings.
#[derive(Clone)]
pub struct FillOptions {
    pub api_key: String,
    pub meta: RecordingMeta,
}

/// Where filling is: clip `index` of `count`, and its stage at Soniox.
#[derive(Clone, Debug)]
pub struct Progress {
    pub index: usize,
    pub count: usize,
    pub gap: Gap,
    pub stage: files::Stage,
}

pub struct FillOutcome {
    /// Lines added, or why filling stopped (lines added before that stay).
    pub lines: Result<usize>,
    /// Clips or jobs Soniox still stores; retry deleting them later.
    pub leftovers: Vec<files::Leftover>,
}

/// Transcribes each gap and merges the lines into `transcript.jsonl`, one
/// gap at a time so a failure keeps what already worked.
pub async fn fill(
    dir: PathBuf,
    gaps: Vec<Gap>,
    options: FillOptions,
    mut on_progress: impl FnMut(Progress) + Send,
) -> FillOutcome {
    let mut leftovers = Vec::new();
    let mut lines = 0;
    let count = gaps.len();
    for (index, gap) in gaps.into_iter().enumerate() {
        match fill_one(&dir, &gap, index, count, &options, &mut on_progress).await {
            Ok((added, left)) => {
                lines += added;
                leftovers.extend(left);
            }
            Err((error, left)) => {
                leftovers.extend(left);
                return FillOutcome {
                    lines: Err(error),
                    leftovers,
                };
            }
        }
    }
    FillOutcome {
        lines: Ok(lines),
        leftovers,
    }
}

type Failed = (anyhow::Error, Vec<files::Leftover>);

async fn fill_one(
    dir: &Path,
    gap: &Gap,
    index: usize,
    count: usize,
    options: &FillOptions,
    on_progress: &mut (impl FnMut(Progress) + Send),
) -> Result<(usize, Vec<files::Leftover>), Failed> {
    let no_leftovers = |e: anyhow::Error| (e, Vec::new());
    let clip_from = gap.from_ms.saturating_sub(PAD_MS);
    let clip_to = gap.to_ms + PAD_MS;
    let wav = {
        let path = wav_path(dir, gap.source);
        tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            let (samples, rate, channels) = read_clip(&path, clip_from, clip_to)?;
            files::wav_clip(&samples, rate, channels)
        })
        .await
        .map_err(|e| no_leftovers(e.into()))?
        .map_err(no_leftovers)?
    };

    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let meta = &options.meta;
    let config = files::FileConfig {
        model: files::MODEL.to_owned(),
        language_hints: meta.language_hints.clone(),
        // As live: the mic is always "me", the system track has the speakers.
        enable_speaker_diarization: gap.source == Source::System,
        enable_language_identification: true,
        context: (!meta.terms.is_empty()).then(|| soniox::Context {
            terms: meta.terms.clone(),
            ..Default::default()
        }),
        translation: meta.translation.clone(),
        client_reference_id: Some(format!("fill/{name}/{}", source_name(gap.source))),
    };
    let file_name = format!("{name}-{}-{}.wav", source_name(gap.source), gap.from_ms);
    let outcome = files::transcribe(&options.api_key, wav, &file_name, &config, |stage| {
        on_progress(Progress {
            index,
            count,
            gap: gap.clone(),
            stage,
        })
    })
    .await;
    let leftovers = outcome.leftovers;
    let tokens = match outcome.tokens {
        Ok(tokens) => tokens,
        Err(e) => return Err((e, leftovers)),
    };

    let lines = lines_for_gap(gap, clip_from, tokens, meta.translation.is_some());
    let added = lines.len();
    let dir = dir.to_owned();
    let gap = gap.clone();
    tokio::task::spawn_blocking(move || merge_into_transcript(&dir, &gap, lines))
        .await
        .map_err(|e| (e.into(), leftovers.clone()))?
        .map_err(|e| (e, leftovers.clone()))?;
    Ok((added, leftovers))
}

/// Turns a clip's tokens into transcript lines on the recording's timeline,
/// keeping only words that start inside the gap.
fn lines_for_gap(
    gap: &Gap,
    clip_from: u64,
    tokens: Vec<Token>,
    translating: bool,
) -> Vec<serde_json::Value> {
    let mut kept = Vec::new();
    // Translated tokens have no timestamps; they belong with the spoken
    // words just before them.
    let mut keeping = false;
    for mut token in tokens {
        if !token.is_translation() {
            token.start_ms += clip_from;
            token.end_ms += clip_from;
            keeping = (gap.from_ms..gap.to_ms).contains(&token.start_ms);
        }
        if keeping {
            // The file model numbers speakers per clip: keep them apart
            // from the live stream's S1, S2…
            token.speaker = token.speaker.map(|s| format!("F{s}"));
            token.is_final = true;
            kept.push(token);
        }
    }
    let mut segmenter = Transcript::for_filling(translating);
    let _ = segmenter.apply(gap.source, &kept, Vec::new());
    segmenter
        .into_segments()
        .into_iter()
        .map(|mut segment| {
            segment.filled = true;
            segment.record()
        })
        .collect()
}

/// Adds filled lines and a `fill` record, keeping the untouched live
/// transcript as `transcript.live.jsonl` the first time.
fn merge_into_transcript(dir: &Path, gap: &Gap, lines: Vec<serde_json::Value>) -> Result<()> {
    let path = transcript_path(dir);
    let backup = dir.join("transcript.live.jsonl");
    if path.exists() && !backup.exists() {
        fs::copy(&path, &backup).context("backing up transcript.jsonl")?;
    }
    let mut records = read_records(dir)?;
    records.extend(lines);
    records.push(serde_json::json!({
        "type": "fill",
        "source": source_name(gap.source),
        "from_ms": gap.from_ms,
        "to_ms": gap.to_ms,
        "model": files::MODEL,
        "at": chrono::Local::now().to_rfc3339(),
    }));
    let time = |r: &serde_json::Value| {
        ["start_ms", "at_ms", "from_ms"]
            .iter()
            .find_map(|key| r[*key].as_u64())
            .unwrap_or(0)
    };
    records.sort_by_key(time);
    let mut out = String::new();
    for record in &records {
        out.push_str(&record.to_string());
        out.push('\n');
    }
    // Write-then-rename, so a crash never leaves a half-written transcript.
    let tmp = dir.join("transcript.jsonl.tmp");
    fs::write(&tmp, out)?;
    fs::rename(&tmp, &path).context("replacing transcript.jsonl")?;
    Ok(())
}

pub fn describe(gaps: &[Gap]) -> String {
    let per_source = |source| {
        let ms: u64 = gaps
            .iter()
            .filter(|g| g.source == source)
            .map(Gap::len_ms)
            .sum();
        (ms > 0).then(|| {
            let who = if source == Source::Mic { "Me" } else { "Them" };
            format!("{who} {:.1} min", ms as f64 / 60_000.0)
        })
    };
    [per_source(Source::Mic), per_source(Source::System)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ")
}

/// Text for one gap, e.g. "Them 61:18 to 86:12".
pub fn gap_label(gap: &Gap) -> String {
    let who = if gap.source == Source::Mic {
        "Me"
    } else {
        "Them"
    };
    format!("{who} {} to {}", clock(gap.from_ms), clock(gap.to_ms))
}

/// Items Soniox still stores for us, deleted on the next launch.
pub fn pending_deletes_path() -> Option<PathBuf> {
    Some(
        dirs::data_local_dir()?
            .join("Meeting Transcriber")
            .join("pending-deletes.json"),
    )
}

pub fn load_pending_deletes() -> Vec<files::Leftover> {
    pending_deletes_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

pub fn save_pending_deletes(items: &[files::Leftover]) -> Result<()> {
    let Some(path) = pending_deletes_path() else {
        bail!("no app data folder");
    };
    if items.is_empty() {
        let _ = fs::remove_file(path);
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, serde_json::to_string(items)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtract_and_merge_ranges() {
        assert_eq!(
            subtract(&[(0, 100)], &[(10, 20), (15, 30), (90, 200)]),
            [(0, 10), (30, 90)]
        );
        assert_eq!(
            merge(vec![(5, 10), (0, 6), (20, 30), (30, 31)]),
            [(0, 10), (20, 31)]
        );
    }

    fn token(text: &str, start_ms: u64, speaker: Option<&str>) -> Token {
        serde_json::from_value(serde_json::json!({
            "text": text, "start_ms": start_ms, "end_ms": start_ms + 300, "speaker": speaker,
        }))
        .unwrap()
    }

    fn translated(text: &str, speaker: Option<&str>) -> Token {
        serde_json::from_value(serde_json::json!({
            "text": text, "speaker": speaker, "translation_status": "translation",
        }))
        .unwrap()
    }

    #[test]
    fn keeps_only_words_inside_the_gap_on_the_recording_timeline() {
        let gap = Gap {
            source: Source::System,
            from_ms: 60_000,
            to_ms: 70_000,
        };
        // The clip starts 3 s before the gap.
        let tokens = vec![
            token("edge", 1_000, Some("1")), // 58.0 s: before the gap
            translated(" cạnh", Some("1")),
            token("Hello", 4_000, Some("1")), // 61.0 s
            token(" team", 4_400, Some("1")),
            translated("Xin chào nhóm", Some("1")),
            token("later", 12_000, Some("2")),  // 69.0 s
            token(" after", 14_000, Some("2")), // 71.0 s: after the gap
        ];
        let lines = lines_for_gap(&gap, 57_000, tokens, true);
        let got: Vec<_> = lines
            .iter()
            .map(|l| {
                (
                    l["text"].as_str().unwrap().to_owned(),
                    l["start_ms"].as_u64().unwrap(),
                    l["speaker"].as_str().unwrap().to_owned(),
                    l["translation"].as_str().unwrap_or("").to_owned(),
                    l["filled"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (
                    "Hello team".into(),
                    61_000,
                    "F1".into(),
                    "Xin chào nhóm".into(),
                    true
                ),
                ("later".into(), 69_000, "F2".into(), String::new(), true),
            ]
        );
    }

    /// Writes a 16 kHz mono float WAV: `speech` seconds loud, then silence.
    fn write_wav(path: &Path, loud_s: &[(u32, bool)]) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for &(seconds, loud) in loud_s {
            for n in 0..seconds * 16_000 {
                let v = if loud {
                    (n as f32 * 0.05).sin() * 0.1
                } else {
                    0.0
                };
                w.write_sample(v).unwrap();
            }
        }
        w.finalize().unwrap();
    }

    #[test]
    fn finds_recorded_gaps_and_untranscribed_speech_but_not_silence() {
        let dir = std::env::temp_dir().join(format!("fill-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // System: speech throughout for 200 s, transcribed only 0..60 s.
        write_wav(&wav_path(&dir, Source::System), &[(200, true)]);
        // Mic: 30 s speech, then 170 s of silence, plus a recorded gap.
        write_wav(&wav_path(&dir, Source::Mic), &[(30, true), (170, false)]);
        let records = [
            r#"{"source":"system","start_ms":0,"end_ms":60000,"text":"a"}"#,
            r#"{"source":"mic","start_ms":0,"end_ms":30000,"text":"b"}"#,
            r#"{"type":"gap","source":"mic","from_ms":100000,"to_ms":110000}"#,
        ];
        fs::write(transcript_path(&dir), records.join("\n")).unwrap();

        let gaps = find_gaps(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(
            gaps,
            [
                // Recorded by the live stream, even though it's silent.
                Gap {
                    source: Source::Mic,
                    from_ms: 100_000,
                    to_ms: 110_000
                },
                // Untranscribed speech to the end.
                Gap {
                    source: Source::System,
                    from_ms: 60_000,
                    to_ms: 200_000
                },
            ]
        );
    }

    #[test]
    fn sporadic_speech_after_the_last_line_is_a_gap() {
        let dir = std::env::temp_dir().join(format!("sporadic-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // Talk, then the stream stops at 60 s; later a few remarks among silence.
        write_wav(
            &wav_path(&dir, Source::Mic),
            &[
                (60, true),
                (100, false),
                (20, true),
                (100, false),
                (10, true),
                (10, false),
            ],
        );
        fs::write(
            transcript_path(&dir),
            r#"{"source":"mic","start_ms":0,"end_ms":60000,"text":"a"}"#,
        )
        .unwrap();
        let gaps = find_gaps(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(
            gaps,
            [Gap {
                source: Source::Mic,
                from_ms: 60_000,
                to_ms: 300_000
            }]
        );
    }

    #[test]
    fn speech_on_one_track_while_the_other_was_transcribed_is_not_a_gap() {
        let dir = std::env::temp_dir().join(format!("leak-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // Both tracks loud for 300 s, the mic picking up the other side.
        write_wav(&wav_path(&dir, Source::Mic), &[(300, true)]);
        write_wav(&wav_path(&dir, Source::System), &[(300, true)]);
        // The mic has lines only at the start and end; the system track is
        // transcribed throughout, so the connection was fine.
        let records = [
            r#"{"source":"mic","start_ms":0,"end_ms":10000,"text":"a"}"#,
            r#"{"source":"mic","start_ms":290000,"end_ms":300000,"text":"b"}"#,
            r#"{"source":"system","start_ms":0,"end_ms":300000,"text":"c"}"#,
        ];
        fs::write(transcript_path(&dir), records.join("\n")).unwrap();
        let gaps = find_gaps(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(gaps, []);
    }

    #[test]
    fn merge_keeps_a_backup_and_marks_the_range_filled() {
        let dir = std::env::temp_dir().join(format!("merge-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            transcript_path(&dir),
            "{\"source\":\"system\",\"start_ms\":0,\"end_ms\":500,\"text\":\"live\"}\n{\"source\":\"system\",\"start_ms\":90000,\"end_ms\":91000,\"text\":\"later\"}\n",
        )
        .unwrap();
        let gap = Gap {
            source: Source::System,
            from_ms: 60_000,
            to_ms: 80_000,
        };
        let line = serde_json::json!({"source":"system","start_ms":61000,"end_ms":62000,"text":"filled","filled":true});
        merge_into_transcript(&dir, &gap, vec![line]).unwrap();

        let texts: Vec<String> = read_records(&dir)
            .unwrap()
            .iter()
            .map(|r| {
                r["text"]
                    .as_str()
                    .or_else(|| r["type"].as_str())
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(texts, ["live", "fill", "filled", "later"]);
        assert!(dir.join("transcript.live.jsonl").exists());
        // The filled range is covered now, so nothing is found again.
        assert!(subtract(&[(60_000, 80_000)], &[(60_000, 80_000)]).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
