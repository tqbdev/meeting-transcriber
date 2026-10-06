//! Soniox real-time speech-to-text over WebSocket.
//!
//! API reference: <https://soniox.com/docs/stt/api-reference/websocket-api>
//!
//! One [`AudioFeed`] is one Soniox stream. Push captured audio into it; it is
//! encoded to 16 kHz mono `pcm_s16le` and sent in ~100 ms frames. Dropping
//! the feed ends the stream gracefully: Soniox finalizes the remaining audio
//! and reports [`Event::Finished`].
//!
//! If the connection drops, or Soniox stops responding, the client reconnects
//! on its own and re-sends the audio Soniox had not finalized yet (up to the
//! last minute), shifting the new session's timestamps so the transcript
//! stays on one timeline. Older untranscribed audio is reported as a gap to
//! fill later from the recording with the file API (see [`files`]).
//!
//! Timestamps are milliseconds since the feed started, which is also the
//! position in the recorded WAV file.

mod encoder;
pub mod files;
mod languages;

pub use languages::{LANGUAGES, language_name};

use std::{collections::VecDeque, error::Error as StdError, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use futures_util::{Sink, SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::{runtime::Handle, sync::mpsc, time::Instant};
use tokio_tungstenite::tungstenite::Message;

use encoder::PcmEncoder;

const URL: &str = "wss://stt-rt.soniox.com/transcribe-websocket";
pub const DEFAULT_MODEL: &str = "stt-rt-v5";

/// 100 ms of 16 kHz 16-bit mono audio.
const FRAME_BYTES: usize = encoder::SAMPLE_RATE as usize / 10 * 2;
/// Bytes of encoded audio per millisecond.
const BYTES_PER_MS: u64 = encoder::SAMPLE_RATE as u64 / 1000 * 2;

#[derive(Clone, Copy, Debug)]
struct Timing {
    connect: Duration,
    /// No message from Soniox for this long means the connection is dead.
    /// Soniox reports progress about once a second, even during silence
    /// (measured), so 10 s leaves a wide margin.
    stall: Duration,
    /// How long to wait for `finished` after the audio ends.
    finish: Duration,
    backoff_first: Duration,
    backoff_max: Duration,
    /// How long to keep reconnecting after the recording has stopped.
    retry_after_end: Duration,
    /// Most untranscribed audio kept for resending. Soniox works through a
    /// backlog at only ~1.1x real time (measured), so a live stream catches
    /// up at ~0.1 s per second: a minute takes ~10 min, anything longer
    /// would leave the live transcript far behind. Older audio becomes a gap.
    max_resend_ms: u64,
}

impl Timing {
    const LIVE: Timing = Timing {
        connect: Duration::from_secs(10),
        stall: Duration::from_secs(10),
        finish: Duration::from_secs(15),
        backoff_first: Duration::from_millis(500),
        backoff_max: Duration::from_secs(15),
        retry_after_end: Duration::from_secs(120),
        max_resend_ms: 60_000,
    };
}

/// Stream options. Audio format fields are filled in by the client.
#[derive(Clone, Serialize)]
pub struct Config {
    pub api_key: String,
    pub model: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub language_hints: Vec<String>,
    pub enable_speaker_diarization: bool,
    pub enable_language_identification: bool,
    /// Emits `<end>` tokens at utterance boundaries. Soniox notes this lowers
    /// diarization accuracy, so leave it off on multi-speaker streams.
    pub enable_endpoint_detection: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Context>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub translation: Option<Translation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_reference_id: Option<String>,
}

/// Live translation, returned in the same stream as the transcript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Translation {
    /// Everything spoken is translated into `target_language`.
    OneWay { target_language: String },
    /// `language_a` is translated to `language_b`, and back.
    TwoWay {
        language_a: String,
        language_b: String,
    },
}

/// Hints that improve recognition of names and jargon (max 8000 tokens).
#[derive(Clone, Debug, Default, Serialize)]
pub struct Context {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub general: Vec<KeyValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub terms: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
}

#[derive(Serialize)]
struct StartRequest<'a> {
    #[serde(flatten)]
    config: &'a Config,
    audio_format: &'static str,
    sample_rate: u32,
    num_channels: u32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Token {
    pub text: String,
    #[serde(default)]
    pub start_ms: u64,
    #[serde(default)]
    pub end_ms: u64,
    #[serde(default)]
    pub confidence: f32,
    #[serde(default)]
    pub is_final: bool,
    pub speaker: Option<String>,
    pub language: Option<String>,
    /// `"original"` or `"none"` for spoken words, `"translation"` for
    /// translated ones. Translated tokens have no timestamps.
    pub translation_status: Option<String>,
}

impl Token {
    pub fn is_translation(&self) -> bool {
        self.translation_status.as_deref() == Some("translation")
    }

    /// `<end>` (endpoint detected) or `<fin>` (manual finalize completed).
    pub fn is_marker(&self) -> bool {
        self.text == "<end>" || self.text == "<fin>"
    }
}

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    tokens: Vec<Token>,
    #[serde(default)]
    finished: bool,
    /// Audio (ms from the start of this session) covered by final tokens.
    final_audio_proc_ms: Option<u64>,
    error_code: Option<u16>,
    error_message: Option<String>,
}

#[derive(Debug)]
pub enum Event {
    /// Soniox accepted the stream and is responding. On a reconnect, `after`
    /// is how long the outage lasted and `resent_ms` how much untranscribed
    /// audio was sent again.
    Connected {
        after: Option<Duration>,
        resent_ms: u64,
    },
    /// The connection dropped or stalled; reconnecting. `at_ms` is how far
    /// the stream had got.
    Reconnecting {
        attempt: u32,
        reason: String,
        down_for: Duration,
        at_ms: u64,
    },
    /// Audio in `from_ms..to_ms` will not be transcribed live: the outage
    /// outlasted what is worth resending. Fill it from the recording.
    AudioDropped { from_ms: u64, to_ms: u64 },
    /// `finals` are settled: append them. `provisional` replaces the previous
    /// provisional tail and may still change.
    Tokens {
        finals: Vec<Token>,
        provisional: Vec<Token>,
    },
    /// All audio was transcribed and the stream closed cleanly.
    Finished,
    /// The stream gave up. Audio from `transcribed_ms` on has no transcript.
    Failed { reason: String, transcribed_ms: u64 },
}

/// Format of the audio passed to [`AudioFeed::push`].
#[derive(Clone, Copy, Debug)]
pub struct InputFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// Opens a Soniox stream on `runtime`. Audio pushed before the connection is
/// up is buffered, so capture can start immediately.
pub fn start(
    runtime: &Handle,
    config: Config,
    input: InputFormat,
    on_event: impl FnMut(Event) + Send + 'static,
) -> Result<AudioFeed> {
    start_at(URL, Timing::LIVE, runtime, config, input, on_event)
}

fn start_at(
    url: &'static str,
    timing: Timing,
    runtime: &Handle,
    config: Config,
    input: InputFormat,
    mut on_event: impl FnMut(Event) + Send + 'static,
) -> Result<AudioFeed> {
    let encoder = PcmEncoder::new(input.sample_rate, input.channels)?;
    // Unbounded so capture never blocks; while disconnected the stream task
    // keeps draining it into its own capped queue.
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    runtime.spawn(async move {
        let mut stream = Stream::default();
        let event = match run(url, timing, &config, audio_rx, &mut stream, &mut on_event).await {
            Ok(()) => Event::Finished,
            Err(e) => Event::Failed {
                reason: format!("{e:#}"),
                transcribed_ms: stream.queue_start_ms,
            },
        };
        on_event(event);
    });
    Ok(AudioFeed {
        encoder,
        audio_tx,
        buffer: Vec::with_capacity(FRAME_BYTES * 2),
        failed: false,
    })
}

/// Sends audio to one Soniox stream. Drop it to end the stream.
pub struct AudioFeed {
    encoder: PcmEncoder,
    audio_tx: mpsc::UnboundedSender<Vec<u8>>,
    buffer: Vec<u8>,
    failed: bool,
}

impl AudioFeed {
    /// Takes interleaved samples in the stream's [`InputFormat`].
    pub fn push(&mut self, interleaved: &[f32]) {
        if self.failed {
            return;
        }
        if self.encoder.push(interleaved, &mut self.buffer).is_err() {
            self.failed = true;
            return;
        }
        if self.buffer.len() >= FRAME_BYTES {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let frame = std::mem::replace(&mut self.buffer, Vec::with_capacity(FRAME_BYTES * 2));
        // Fails only once the stream task has stopped, which reports its own error.
        let _ = self.audio_tx.send(frame);
    }
}

impl Drop for AudioFeed {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Why a session ended without `finished`.
enum SessionError {
    /// Worth reconnecting: network trouble, timeouts, server-side limits.
    Retry(anyhow::Error),
    /// Reconnecting won't help: bad key, no credit, bad request.
    Fatal(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for SessionError {
    fn from(e: E) -> Self {
        SessionError::Retry(e.into())
    }
}

/// What carries over between sessions of one stream.
#[derive(Default)]
struct Stream {
    /// Audio received but not yet finalized by Soniox, sent or waiting.
    queue: VecDeque<u8>,
    /// Timeline position of `queue[0]`: everything before it is transcribed
    /// (or was reported as dropped).
    queue_start_ms: u64,
    received_any: bool,
    /// When the feed was dropped, i.e. the recording stopped.
    ended_at: Option<Instant>,
}

impl Stream {
    fn received_ms(&self) -> u64 {
        self.queue_start_ms + self.queue.len() as u64 / BYTES_PER_MS
    }

    fn push(&mut self, bytes: &[u8]) {
        self.queue.extend(bytes);
        self.received_any = true;
    }

    /// Soniox finalized this session's audio up to `session_ms`; drop what
    /// no longer needs resending.
    fn finalized_up_to(&mut self, session_start_ms: u64, session_ms: u64) {
        let finalized = session_start_ms + session_ms;
        let drop_ms = finalized.saturating_sub(self.queue_start_ms);
        let drop_bytes = (drop_ms * BYTES_PER_MS).min(self.queue.len() as u64) as usize;
        self.queue.drain(..drop_bytes);
        self.queue_start_ms += drop_bytes as u64 / BYTES_PER_MS;
    }

    /// Keeps at most `max_ms` of the newest audio. Returns the range dropped.
    fn trim_to(&mut self, max_ms: u64) -> Option<(u64, u64)> {
        let len_ms = self.queue.len() as u64 / BYTES_PER_MS;
        let excess_ms = len_ms.checked_sub(max_ms).filter(|&ms| ms > 0)?;
        let from = self.queue_start_ms;
        self.queue.drain(..(excess_ms * BYTES_PER_MS) as usize);
        self.queue_start_ms += excess_ms;
        Some((from, self.queue_start_ms))
    }
}

async fn run(
    url: &str,
    timing: Timing,
    config: &Config,
    mut audio: mpsc::UnboundedReceiver<Vec<u8>>,
    stream: &mut Stream,
    on_event: &mut (impl FnMut(Event) + Send),
) -> Result<()> {
    let mut attempt = 0;
    // Set while the stream is down: when the outage began.
    let mut outage: Option<Instant> = None;
    loop {
        let error = match session(
            url,
            timing,
            config,
            &mut audio,
            stream,
            &mut outage,
            &mut attempt,
            on_event,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(SessionError::Fatal(e)) => return Err(e),
            Err(SessionError::Retry(e)) => e,
        };
        if stream
            .ended_at
            .is_some_and(|ended| ended.elapsed() >= timing.retry_after_end)
        {
            return Err(error);
        }
        let down_since = *outage.get_or_insert_with(Instant::now);
        attempt += 1;
        on_event(Event::Reconnecting {
            attempt,
            reason: format!("{error:#}"),
            down_for: down_since.elapsed(),
            at_ms: stream.received_ms(),
        });
        let backoff = timing
            .backoff_first
            .saturating_mul(1 << (attempt - 1).min(6))
            .min(timing.backoff_max);
        wait_and_queue(&mut audio, stream, timing, backoff, on_event).await;
    }
}

/// Sits out a backoff while still taking in audio, so capture never backs
/// up and the resend cap applies during the outage.
async fn wait_and_queue(
    audio: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    stream: &mut Stream,
    timing: Timing,
    backoff: Duration,
    on_event: &mut (impl FnMut(Event) + Send),
) {
    let wake = Instant::now() + backoff;
    let mut dropped: Option<(u64, u64)> = None;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(wake) => break,
            chunk = audio.recv(), if stream.ended_at.is_none() => match chunk {
                Some(bytes) => {
                    stream.push(&bytes);
                    if let Some((from, to)) = stream.trim_to(timing.max_resend_ms) {
                        dropped = Some((dropped.map_or(from, |(start, _)| start), to));
                    }
                }
                None => stream.ended_at = Some(Instant::now()),
            },
        }
    }
    if let Some((from_ms, to_ms)) = dropped {
        on_event(Event::AudioDropped { from_ms, to_ms });
    }
}

/// Sends one message, treating a send that hangs (a stalled connection
/// whose buffers are full) the same as a failed one.
async fn send<S>(tx: &mut S, message: Message, timing: Timing) -> Result<(), SessionError>
where
    S: Sink<Message> + Unpin,
    S::Error: StdError + Send + Sync + 'static,
{
    tokio::time::timeout(timing.stall, tx.send(message))
        .await
        .map_err(|_| anyhow!("sending to Soniox stalled for {} s", timing.stall.as_secs()))?
        .context("sending to Soniox")?;
    Ok(())
}

/// One WebSocket connection. Returns `Ok` once Soniox reports `finished`.
#[allow(clippy::too_many_arguments)]
async fn session(
    url: &str,
    timing: Timing,
    config: &Config,
    audio: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    stream: &mut Stream,
    outage: &mut Option<Instant>,
    attempt: &mut u32,
    on_event: &mut (impl FnMut(Event) + Send),
) -> Result<(), SessionError> {
    // Stopped, and everything received is already transcribed.
    if stream.ended_at.is_some() && stream.queue.is_empty() {
        return Ok(());
    }
    let (socket, _) = tokio::time::timeout(timing.connect, tokio_tungstenite::connect_async(url))
        .await
        .context("connecting to Soniox timed out")?
        .context("connecting to Soniox")?;
    let (mut tx, mut rx) = socket.split();

    let start = StartRequest {
        config,
        audio_format: "pcm_s16le",
        sample_rate: encoder::SAMPLE_RATE,
        num_channels: 1,
    };
    send(
        &mut tx,
        Message::Text(serde_json::to_string(&start)?.into()),
        timing,
    )
    .await?;

    // This session's timestamps start at the first audio it receives.
    let session_start_ms = stream.queue_start_ms;
    let resent_ms = stream.queue.len() as u64 / BYTES_PER_MS;
    let replay: Vec<u8> = stream.queue.iter().copied().collect();
    for frame in replay.chunks(FRAME_BYTES) {
        send(&mut tx, Message::Binary(frame.to_vec().into()), timing).await?;
    }

    // An empty frame tells Soniox the audio is complete. It must be a text
    // frame: the docs allow binary too, but Soniox ignored an empty binary
    // frame in testing and never sent `finished` (nor finalized the last
    // words).
    let end_of_audio = || Message::Text(String::new().into());
    // Set once the audio has ended: the latest we wait for `finished`.
    let mut deadline: Option<Instant> = None;
    if stream.ended_at.is_some() {
        send(&mut tx, end_of_audio(), timing).await?;
        deadline = Some(Instant::now() + timing.finish);
    }

    let mut last_heard = Instant::now();
    let mut heard_any = false;
    loop {
        tokio::select! {
            chunk = audio.recv(), if stream.ended_at.is_none() => match chunk {
                Some(bytes) => {
                    stream.push(&bytes);
                    send(&mut tx, Message::Binary(bytes.into()), timing).await?;
                }
                None => {
                    stream.ended_at = Some(Instant::now());
                    // Nothing was ever sent, so there is nothing to finalize.
                    if !stream.received_any {
                        let _ = tx.close().await;
                        return Ok(());
                    }
                    send(&mut tx, end_of_audio(), timing).await?;
                    deadline = Some(Instant::now() + timing.finish);
                }
            },
            message = rx.next() => {
                last_heard = Instant::now();
                match message {
                    Some(Ok(Message::Text(text))) => {
                        let response: Response = serde_json::from_str(&text)
                            .with_context(|| format!("unexpected message from Soniox: {text}"))?;
                        if let Some(code) = response.error_code {
                            let message = response.error_message.unwrap_or_default();
                            let error = anyhow!("Soniox error {code}: {message}");
                            // 408 timeout, 413 max stream length, 429 rate
                            // limit and 5xx are transient; the rest need the user.
                            return Err(match code {
                                408 | 413 | 429 | 500.. => SessionError::Retry(error),
                                _ => SessionError::Fatal(error),
                            });
                        }
                        if !heard_any {
                            // Only now is the stream really up: a server that
                            // accepts and then drops us shouldn't reset backoff.
                            heard_any = true;
                            *attempt = 0;
                            on_event(Event::Connected {
                                after: outage.take().map(|since| since.elapsed()),
                                resent_ms,
                            });
                        }
                        if !response.tokens.is_empty() {
                            let (finals, provisional) = response
                                .tokens
                                .into_iter()
                                .map(|mut token| {
                                    // Translated tokens carry no timestamps.
                                    if !token.is_translation() {
                                        token.start_ms += session_start_ms;
                                        token.end_ms += session_start_ms;
                                    }
                                    token
                                })
                                .partition(|t: &Token| t.is_final);
                            on_event(Event::Tokens { finals, provisional });
                        }
                        if let Some(ms) = response.final_audio_proc_ms {
                            stream.finalized_up_to(session_start_ms, ms);
                        }
                        if response.finished {
                            return Ok(());
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        return Err(anyhow!("Soniox closed the connection: {frame:?}").into());
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        return Err(anyhow::Error::from(e).context("reading from Soniox").into());
                    }
                    None => return Err(anyhow!("Soniox closed the connection").into()),
                }
            },
            _ = tokio::time::sleep_until(last_heard + timing.stall) => {
                return Err(anyhow!("no response from Soniox for {} s", timing.stall.as_secs()).into());
            }
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                return Err(anyhow!("Soniox did not finish within {} s", timing.finish.as_secs()).into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Short timeouts so the fake-server tests run in well under a second.
    const FAST: Timing = Timing {
        connect: Duration::from_secs(2),
        stall: Duration::from_millis(300),
        finish: Duration::from_secs(2),
        backoff_first: Duration::from_millis(50),
        backoff_max: Duration::from_millis(200),
        retry_after_end: Duration::from_secs(2),
        max_resend_ms: 60_000,
    };

    fn config() -> Config {
        Config {
            api_key: "key".into(),
            model: DEFAULT_MODEL.into(),
            language_hints: vec![],
            enable_speaker_diarization: false,
            enable_language_identification: false,
            enable_endpoint_detection: false,
            context: None,
            translation: None,
            client_reference_id: None,
        }
    }

    fn url_of(listener: &tokio::net::TcpListener) -> &'static str {
        Box::leak(format!("ws://{}", listener.local_addr().unwrap()).into_boxed_str())
    }

    /// Reads audio frames until `bytes` have arrived or the end-of-audio frame.
    async fn read_audio<S>(ws: &mut S, bytes: usize) -> usize
    where
        S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        let mut received = 0;
        while received < bytes {
            match ws.next().await.unwrap().unwrap() {
                Message::Binary(b) => received += b.len(),
                Message::Text(t) if t.is_empty() => break,
                _ => {}
            }
        }
        received
    }

    /// The failure seen in a real meeting: the connection stays open but
    /// Soniox goes quiet. The client must give up on it and reconnect.
    #[tokio::test(flavor = "multi_thread")]
    async fn reconnects_when_soniox_goes_quiet() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = url_of(&listener);
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut quiet = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _config = quiet.next().await.unwrap().unwrap();
            quiet
                .send(Message::Text(
                    r#"{"tokens":[],"final_audio_proc_ms":0}"#.into(),
                ))
                .await
                .unwrap();
            // Say nothing more, but keep the connection open.
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _config = ws.next().await.unwrap().unwrap();
            read_audio(&mut ws, usize::MAX).await;
            ws.send(Message::Text(r#"{"tokens":[],"finished":true}"#.into()))
                .await
                .unwrap();
            drop(quiet);
        });

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let input = InputFormat {
            sample_rate: 16_000,
            channels: 1,
        };
        let mut feed = start_at(url, FAST, &Handle::current(), config(), input, move |e| {
            let _ = tx.send(e);
        })
        .unwrap();
        feed.push(&[0.1; 1600]);

        let mut saw = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                Event::Reconnecting { reason, .. } => {
                    assert!(reason.contains("no response"), "{reason}");
                    saw.push("reconnecting");
                    drop(std::mem::replace(&mut feed, dummy_feed()));
                }
                Event::Connected { after, .. } => saw.push(if after.is_some() {
                    "reconnected"
                } else {
                    "connected"
                }),
                Event::Finished => break,
                Event::Failed { reason, .. } => panic!("stream failed: {reason}"),
                _ => {}
            }
        }
        server.await.unwrap();
        assert_eq!(saw, ["connected", "reconnecting", "reconnected"]);
    }

    /// A feed with no stream behind it, to swap in when a test drops the real one.
    fn dummy_feed() -> AudioFeed {
        AudioFeed {
            encoder: PcmEncoder::new(16_000, 1).unwrap(),
            audio_tx: mpsc::unbounded_channel().0,
            buffer: Vec::new(),
            failed: true,
        }
    }

    #[test]
    fn resend_queue_keeps_the_newest_minute() {
        let mut stream = Stream::default();
        stream.push(&vec![0; (90_000 * BYTES_PER_MS) as usize]);
        assert_eq!(stream.trim_to(60_000), Some((0, 30_000)));
        assert_eq!(stream.queue_start_ms, 30_000);
        assert_eq!(stream.received_ms(), 90_000);
        assert_eq!(stream.trim_to(60_000), None);

        // Finalizing moves the start forward without losing the tail.
        stream.finalized_up_to(30_000, 10_000);
        assert_eq!(
            (stream.queue_start_ms, stream.received_ms()),
            (40_000, 90_000)
        );
    }

    /// A fake Soniox that finalizes the first 500 ms, drops the connection,
    /// then expects the rest of the audio again on a second connection.
    #[tokio::test(flavor = "multi_thread")]
    async fn reconnects_and_resends_unfinalized_audio() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = url_of(&listener);

        let server = tokio::spawn(async move {
            // First connection: take a second of audio, finalize 500 ms, hang up.
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _config = ws.next().await.unwrap().unwrap();
            let mut received = 0;
            while received < 1000 * BYTES_PER_MS as usize {
                if let Message::Binary(b) = ws.next().await.unwrap().unwrap() {
                    received += b.len();
                }
            }
            let tokens = r#"{"tokens":[{"text":"Hello","start_ms":0,"end_ms":400,"is_final":true}],"final_audio_proc_ms":500}"#;
            ws.send(Message::Text(tokens.into())).await.unwrap();
            ws.flush().await.unwrap();
            drop(ws); // no closing handshake

            // Second connection: collect audio until the end-of-stream frame.
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _config = ws.next().await.unwrap().unwrap();
            let mut resent = 0;
            loop {
                match ws.next().await.unwrap().unwrap() {
                    Message::Binary(b) => resent += b.len(),
                    Message::Text(t) if t.is_empty() => break,
                    _ => {}
                }
            }
            let tokens = r#"{"tokens":[{"text":" world","start_ms":100,"end_ms":300,"is_final":true}],"final_audio_proc_ms":500}"#;
            ws.send(Message::Text(tokens.into())).await.unwrap();
            ws.send(Message::Text(r#"{"tokens":[],"finished":true}"#.into()))
                .await
                .unwrap();
            resent
        });

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let input = InputFormat {
            sample_rate: 16_000,
            channels: 1,
        };
        let mut feed = start_at(url, FAST, &Handle::current(), config(), input, move |e| {
            let _ = tx.send(e);
        })
        .unwrap();
        feed.push(&vec![0.1; 16_000]); // one second
        drop(feed);

        let mut finals = Vec::new();
        let mut reconnects = 0;
        loop {
            match tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                Event::Tokens { finals: f, .. } => finals.extend(f),
                Event::Reconnecting { .. } => reconnects += 1,
                Event::Finished => break,
                Event::Failed { reason, .. } => panic!("stream failed: {reason}"),
                Event::Connected { .. } | Event::AudioDropped { .. } => {}
            }
        }

        assert_eq!(reconnects, 1);
        // Only the 500 ms after the finalized part is sent again.
        assert_eq!(server.await.unwrap(), 500 * BYTES_PER_MS as usize);
        let words: Vec<_> = finals
            .iter()
            .map(|t| (t.text.as_str(), t.start_ms, t.end_ms))
            .collect();
        // The second session's timestamps continue from 500 ms.
        assert_eq!(words, [("Hello", 0, 400), (" world", 600, 800)]);
    }

    #[test]
    fn start_request_has_audio_format_and_skips_empty_fields() {
        let config = Config {
            api_key: "key".into(),
            model: DEFAULT_MODEL.into(),
            language_hints: vec!["en".into(), "vi".into()],
            enable_speaker_diarization: true,
            enable_language_identification: true,
            enable_endpoint_detection: false,
            context: Some(Context {
                terms: vec!["Soniox".into()],
                ..Default::default()
            }),
            translation: Some(Translation::TwoWay {
                language_a: "en".into(),
                language_b: "vi".into(),
            }),
            client_reference_id: None,
        };
        let json = serde_json::to_value(StartRequest {
            config: &config,
            audio_format: "pcm_s16le",
            sample_rate: 16_000,
            num_channels: 1,
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "api_key": "key",
                "model": "stt-rt-v5",
                "language_hints": ["en", "vi"],
                "enable_speaker_diarization": true,
                "enable_language_identification": true,
                "enable_endpoint_detection": false,
                "context": { "terms": ["Soniox"] },
                "translation": { "type": "two_way", "language_a": "en", "language_b": "vi" },
                "audio_format": "pcm_s16le",
                "sample_rate": 16000,
                "num_channels": 1,
            })
        );
    }

    #[test]
    fn parses_tokens_and_errors() {
        let response: Response = serde_json::from_str(
            r#"{"tokens":[{"text":" Hi","start_ms":120,"end_ms":300,"confidence":0.97,"is_final":true,"speaker":"1","language":"en"},{"text":"<end>","is_final":true}],"final_audio_proc_ms":300,"total_audio_proc_ms":540}"#,
        )
        .unwrap();
        assert_eq!(response.tokens.len(), 2);
        assert_eq!(response.tokens[0].speaker.as_deref(), Some("1"));
        assert!(response.tokens[1].is_marker());
        assert!(!response.tokens[0].is_translation());
        assert!(!response.finished);

        let error: Response = serde_json::from_str(
            r#"{"tokens":[],"error_code":401,"error_type":"unauthenticated","error_message":"Invalid API key."}"#,
        )
        .unwrap();
        assert_eq!(error.error_code, Some(401));
    }
}
