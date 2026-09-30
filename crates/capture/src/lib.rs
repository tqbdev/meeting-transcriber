//! Audio capture for the meeting transcriber.
//!
//! The microphone ("me") and system audio ("them") are recorded as two
//! independent streams, each to its own WAV file, so later stages always know
//! which lines belong to the local user.
//!
//! System audio uses cpal's loopback support: opening an *input* stream on an
//! output device makes cpal create a Core Audio process tap plus a private
//! aggregate device (macOS 14.6+). No virtual driver is needed.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
mod transport;

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Which side of the conversation a stream carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    Mic,
    System,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Mic => "Me (microphone)",
            Source::System => "Them (system audio)",
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Source::Mic => "mic.wav",
            Source::System => "system.wav",
        }
    }
}

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    /// Stable cpal device id, passed back in [`SessionOptions::mic_device`].
    pub id: String,
    pub name: String,
    pub is_default: bool,
    /// Opening a Bluetooth headset's mic switches it into call mode, which
    /// lowers playback quality and changes the output's sample rate.
    pub bluetooth: bool,
}

/// Lists the devices that can act as the microphone.
pub fn input_devices() -> Result<Vec<DeviceInfo>> {
    let host = cpal::default_host();
    let default_id = host.default_input_device().and_then(|d| d.id().ok());
    // cpal reports connection types on Windows but not on macOS.
    #[cfg(target_os = "macos")]
    let bluetooth_uids = transport::bluetooth_uids();
    #[cfg(target_os = "macos")]
    let is_bluetooth = |_: &cpal::Device, id: &cpal::DeviceId| bluetooth_uids.contains(id.id());
    #[cfg(not(target_os = "macos"))]
    let is_bluetooth = |device: &cpal::Device, _: &cpal::DeviceId| {
        device
            .description()
            .is_ok_and(|d| d.interface_type() == cpal::InterfaceType::Bluetooth)
    };
    let mut devices = Vec::new();
    for device in host.input_devices()? {
        let id = device.id()?;
        devices.push(DeviceInfo {
            is_default: Some(&id) == default_id.as_ref(),
            bluetooth: is_bluetooth(&device, &id),
            id: id.to_string(),
            name: device_name(&device),
        });
    }
    Ok(devices)
}

/// The output device whose audio the system stream will tap.
#[derive(Clone, Debug)]
pub struct SystemOutput {
    pub name: String,
    /// cpal only taps devices that have no input channels. For a device that
    /// has both (some USB headsets), it records that device's microphone
    /// instead of what is playing through it.
    pub has_input: bool,
}

pub fn system_output() -> Option<SystemOutput> {
    let device = cpal::default_host().default_output_device()?;
    Some(SystemOutput {
        name: device_name(&device),
        has_input: device.supports_input(),
    })
}

fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|_| "Unknown device".to_owned())
}

/// Sample layout of a captured stream, as delivered to an [`AudioSink`].
#[derive(Clone, Copy, Debug)]
pub struct StreamFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// Receives a copy of a stream's interleaved f32 samples, always whole
/// frames. It runs on the stream's writer thread, not the real-time audio
/// thread, so it may allocate. It is dropped when the stream stops.
pub type AudioSink = Box<dyn FnMut(&[f32]) + Send>;

/// Called once per stream after it opens; may return a sink for its audio.
pub type SinkFactory = Box<dyn FnMut(Source, StreamFormat) -> Option<AudioSink>>;

pub struct SessionOptions {
    pub out_dir: PathBuf,
    pub mic: bool,
    /// `None` uses the system default microphone.
    pub mic_device: Option<String>,
    pub system: bool,
    pub sinks: Option<SinkFactory>,
}

/// Live level and progress for one stream.
#[derive(Clone, Debug)]
pub struct TrackLevel {
    pub source: Source,
    pub device_name: String,
    /// Peak sample magnitude (0.0..=1.0) since the previous call to [`Session::levels`].
    pub peak: f32,
    pub seconds_written: f64,
    /// Samples lost because the writer thread fell behind. Should stay 0.
    pub dropped_samples: u64,
    /// Breaks in the audio the OS reported (xruns). A few are normal when a
    /// device changes; a steady climb means audio is being lost.
    pub glitches: u64,
}

/// A finished WAV file.
#[derive(Clone, Debug)]
pub struct Recording {
    pub source: Source,
    pub path: PathBuf,
    pub seconds: f64,
}

type ErrorLog = Arc<Mutex<Vec<String>>>;

/// A running capture. Dropping it stops the streams and finalizes the files.
pub struct Session {
    tracks: Vec<Track>,
    started: Instant,
    errors: ErrorLog,
    out_dir: PathBuf,
}

impl Session {
    pub fn start(mut opts: SessionOptions) -> Result<Self> {
        if !opts.mic && !opts.system {
            bail!("pick at least one source to record");
        }
        std::fs::create_dir_all(&opts.out_dir)
            .with_context(|| format!("creating {}", opts.out_dir.display()))?;

        let host = cpal::default_host();
        let errors = ErrorLog::default();
        let mut tracks = Vec::new();

        if opts.mic {
            let device = match &opts.mic_device {
                Some(id) => host
                    .device_by_id(&id.parse()?)
                    .ok_or_else(|| anyhow!("the selected microphone is no longer connected"))?,
                None => host.default_input_device().context("no microphone found")?,
            };
            let config = device
                .default_input_config()
                .context("reading the microphone format")?;
            tracks.push(Track::start(
                Source::Mic,
                &device,
                config.config(),
                &opts.out_dir,
                &errors,
                &mut opts.sinks,
            )?);
        }

        if opts.system {
            tracks.push(start_system_track(
                &host,
                &opts.out_dir,
                &errors,
                &mut opts.sinks,
            )?);
        }

        Ok(Self {
            tracks,
            started: Instant::now(),
            errors,
            out_dir: opts.out_dir,
        })
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn out_dir(&self) -> &Path {
        &self.out_dir
    }

    /// Reads and resets each track's peak meter.
    pub fn levels(&self) -> Vec<TrackLevel> {
        self.tracks.iter().map(Track::level).collect()
    }

    /// Stream errors reported by Core Audio since the last call.
    pub fn take_errors(&self) -> Vec<String> {
        std::mem::take(&mut *self.errors.lock().unwrap())
    }

    pub fn stop(mut self) -> Result<Vec<Recording>> {
        let mut recordings = Vec::new();
        for track in &mut self.tracks {
            track.finish()?;
            recordings.push(Recording {
                source: track.source,
                path: track.path.clone(),
                seconds: track.seconds_written(),
            });
        }
        Ok(recordings)
    }
}

/// Opens the system-audio tap, retrying while the output device settles.
///
/// Opening a Bluetooth headset's mic switches it into call mode, which
/// changes its output sample rate a moment later. cpal then sees a stream
/// format that disagrees with the device's rate and fails with "Sample rate
/// update timed out". Re-reading the format after a short wait succeeds.
fn start_system_track(
    host: &cpal::Host,
    dir: &Path,
    errors: &ErrorLog,
    sinks: &mut Option<SinkFactory>,
) -> Result<Track> {
    const ATTEMPTS: u32 = 4;
    let mut attempt = 1;
    loop {
        let result = host
            .default_output_device()
            .context("no output device found")
            .and_then(|device| {
                let config = device
                    .default_output_config()
                    .context("reading the output device format")?;
                Track::start(Source::System, &device, config.config(), dir, errors, sinks)
            });
        match result {
            Ok(track) => return Ok(track),
            Err(_) if attempt < ATTEMPTS => {
                thread::sleep(Duration::from_millis(750));
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Counters shared between the audio callback, the writer thread and the UI.
#[derive(Default)]
struct Meter {
    /// Bits of a non-negative f32. For non-negative floats the bit patterns
    /// sort the same way as the values, so `fetch_max` works on them.
    peak_bits: AtomicU32,
    samples_written: AtomicU64,
    dropped_samples: AtomicU64,
    glitches: AtomicU64,
}

struct Track {
    source: Source,
    device_name: String,
    path: PathBuf,
    sample_rate: u32,
    channels: u16,
    meter: Arc<Meter>,
    stop: Arc<AtomicBool>,
    stream: Option<cpal::Stream>,
    /// Windows only: a silent output stream that keeps loopback flowing.
    keep_alive: Option<cpal::Stream>,
    writer: Option<JoinHandle<Result<()>>>,
}

impl Track {
    fn start(
        source: Source,
        device: &cpal::Device,
        config: cpal::StreamConfig,
        dir: &Path,
        errors: &ErrorLog,
        sinks: &mut Option<SinkFactory>,
    ) -> Result<Self> {
        let path = dir.join(source.file_name());
        let spec = hound::WavSpec {
            channels: config.channels,
            sample_rate: config.sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let wav = hound::WavWriter::create(&path, spec)
            .with_context(|| format!("creating {}", path.display()))?;

        // Two seconds of slack between the real-time callback and the disk.
        let capacity = config.sample_rate as usize * config.channels as usize * 2;
        let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(capacity);
        let meter = Arc::new(Meter::default());
        let stop = Arc::new(AtomicBool::new(false));
        let channels = config.channels as usize;

        let keep_alive = if cfg!(windows) && source == Source::System {
            Some(play_silence(device, errors)?)
        } else {
            None
        };

        let callback_meter = meter.clone();
        let error_meter = meter.clone();
        let error_log = errors.clone();
        let stream = device
            .build_input_stream(
                config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let peak = data.iter().fold(0f32, |max, s| max.max(s.abs()));
                    callback_meter
                        .peak_bits
                        .fetch_max(peak.to_bits(), Ordering::Relaxed);

                    // Only whole frames, so channels stay interleaved correctly.
                    let fits = producer.slots().min(data.len()) / channels * channels;
                    if let Ok(chunk) = producer.write_chunk_uninit(fits) {
                        chunk.fill_from_iter(data[..fits].iter().copied());
                    }
                    if fits < data.len() {
                        callback_meter
                            .dropped_samples
                            .fetch_add((data.len() - fits) as u64, Ordering::Relaxed);
                    }
                },
                move |err: cpal::Error| {
                    // Counted, not shown: a message per glitch would flood the UI.
                    if matches!(err.kind(), cpal::ErrorKind::Xrun) {
                        error_meter.glitches.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    error_log
                        .lock()
                        .unwrap()
                        .push(format!("{}: {err}", source.label()));
                },
                None,
            )
            .with_context(|| format!("opening {}", source.label()))?;
        stream
            .play()
            .with_context(|| format!("starting {}", source.label()))?;

        // Only now that audio is flowing, so a failed open never starts a
        // transcription stream.
        let format = StreamFormat {
            sample_rate: config.sample_rate,
            channels: config.channels,
        };
        let sink = sinks.as_mut().and_then(|make| make(source, format));
        let writer = spawn_writer(source, consumer, wav, sink, stop.clone(), meter.clone())?;

        Ok(Self {
            source,
            device_name: device_name(device),
            path,
            sample_rate: config.sample_rate,
            channels: config.channels,
            meter,
            stop,
            stream: Some(stream),
            keep_alive,
            writer: Some(writer),
        })
    }

    fn level(&self) -> TrackLevel {
        TrackLevel {
            source: self.source,
            device_name: self.device_name.clone(),
            peak: f32::from_bits(self.meter.peak_bits.swap(0, Ordering::Relaxed)),
            seconds_written: self.seconds_written(),
            dropped_samples: self.meter.dropped_samples.load(Ordering::Relaxed),
            glitches: self.meter.glitches.load(Ordering::Relaxed),
        }
    }

    fn seconds_written(&self) -> f64 {
        let samples = self.meter.samples_written.load(Ordering::Relaxed);
        samples as f64 / (self.sample_rate as f64 * self.channels as f64)
    }

    /// Stops the stream, drains the ring buffer and finalizes the WAV header.
    fn finish(&mut self) -> Result<()> {
        // Dropping the stream stops callbacks (and tears down the loopback
        // tap) before the writer is told to stop, so it sees every sample.
        drop(self.stream.take());
        drop(self.keep_alive.take());
        self.stop.store(true, Ordering::Release);
        match self.writer.take() {
            Some(writer) => writer
                .join()
                .map_err(|_| anyhow!("{} writer thread panicked", self.source.label()))?,
            None => Ok(()),
        }
    }
}

impl Drop for Track {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// Plays silence on `device`. Windows only delivers loopback audio while
/// something is rendering: during quiet stretches the system track would
/// otherwise get no samples at all, so it falls out of step with the mic
/// track and Soniox closes the stream after 20 s without audio. Other
/// recorders (OBS, for one) use the same workaround.
fn play_silence(device: &cpal::Device, errors: &ErrorLog) -> Result<cpal::Stream> {
    let config = device
        .default_output_config()
        .context("reading the output device format")?;
    let error_log = errors.clone();
    let stream = device
        .build_output_stream(
            config.config(),
            |data: &mut [f32], _: &cpal::OutputCallbackInfo| data.fill(0.0),
            move |err: cpal::Error| {
                if !matches!(err.kind(), cpal::ErrorKind::Xrun) {
                    error_log
                        .lock()
                        .unwrap()
                        .push(format!("Silent keep-alive stream: {err}"));
                }
            },
            None,
        )
        .context("opening the silent keep-alive stream")?;
    stream
        .play()
        .context("starting the silent keep-alive stream")?;
    Ok(stream)
}

fn spawn_writer(
    source: Source,
    mut consumer: rtrb::Consumer<f32>,
    mut wav: hound::WavWriter<std::io::BufWriter<std::fs::File>>,
    mut sink: Option<AudioSink>,
    stop: Arc<AtomicBool>,
    meter: Arc<Meter>,
) -> Result<JoinHandle<Result<()>>> {
    let handle = thread::Builder::new()
        .name(format!("{source:?} wav writer"))
        .spawn(move || -> Result<()> {
            let mut last_flush = Instant::now();
            loop {
                // Read the flag before the buffer: once it is set, the stream
                // is gone and everything it produced is already visible.
                let stopping = stop.load(Ordering::Acquire);
                let available = consumer.slots();
                if available > 0 {
                    let chunk = consumer.read_chunk(available)?;
                    let (first, second) = chunk.as_slices();
                    for &sample in first.iter().chain(second) {
                        wav.write_sample(sample)?;
                    }
                    // Both halves hold whole frames: the buffer capacity and
                    // every write are multiples of the channel count.
                    if let Some(sink) = &mut sink {
                        sink(first);
                        sink(second);
                    }
                    chunk.commit_all();
                    meter
                        .samples_written
                        .fetch_add(available as u64, Ordering::Relaxed);
                } else if stopping {
                    break;
                } else {
                    thread::sleep(Duration::from_millis(20));
                }

                // Keep the header current so a crash still leaves a playable file.
                if last_flush.elapsed() >= Duration::from_secs(1) {
                    wav.flush()?;
                    last_flush = Instant::now();
                }
            }
            wav.finalize()?;
            Ok(())
        })?;
    Ok(handle)
}
