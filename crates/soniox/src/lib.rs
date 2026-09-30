//! Soniox real-time speech-to-text over WebSocket.
//!
//! API reference: <https://soniox.com/docs/stt/api-reference/websocket-api>
//!
//! One [`AudioFeed`] is one Soniox stream. Push captured audio into it; it is
//! encoded to 16 kHz mono `pcm_s16le` and sent in ~100 ms frames. Dropping
//! the feed ends the stream gracefully: Soniox finalizes the remaining audio
//! and reports [`Event::Finished`].
//!
//! If the connection drops, the client reconnects on its own and re-sends
//! the audio Soniox had not finalized yet, shifting the new session's
//! timestamps so the transcript stays on one timeline.

mod encoder;
mod languages;

pub use languages::{LANGUAGES, language_name};

use std::{collections::VecDeque, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::{runtime::Handle, sync::mpsc, time::Instant};
use tokio_tungstenite::tungstenite::Message;

use encoder::PcmEncoder;

const URL: &str = "wss://stt-rt.soniox.com/transcribe-websocket";
pub const DEFAULT_MODEL: &str = "stt-rt-v5";

/// 100 ms of 16 kHz 16-bit mono audio.
const FRAME_BYTES: usize = encoder::SAMPLE_RATE as usize / 10 * 2;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for Soniox to finalize after the audio ends.
const FINISH_TIMEOUT: Duration = Duration::from_secs(15);
/// Bytes of encoded audio per millisecond.
const BYTES_PER_MS: u64 = encoder::SAMPLE_RATE as u64 / 1000 * 2;
/// Reconnect attempts allowed after the audio has ended. While audio is
/// still coming there is no limit: it is buffered until the network is back.
const RETRIES_AFTER_END: u32 = 3;

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
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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
    Connected,
    /// The connection dropped; the client is reconnecting and will resend
    /// the audio that was not transcribed yet.
    Reconnecting {
        attempt: u32,
        reason: String,
    },
    /// `finals` are settled: append them. `provisional` replaces the previous
    /// provisional tail and may still change.
    Tokens {
        finals: Vec<Token>,
        provisional: Vec<Token>,
    },
    /// All audio was transcribed and the stream closed cleanly.
    Finished,
    Failed(String),
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
    start_at(URL, runtime, config, input, on_event)
}

fn start_at(
    url: &'static str,
    runtime: &Handle,
    config: Config,
    input: InputFormat,
    mut on_event: impl FnMut(Event) + Send + 'static,
) -> Result<AudioFeed> {
    let encoder = PcmEncoder::new(input.sample_rate, input.channels)?;
    // Unbounded so a slow network delays the transcript instead of losing
    // audio. At 32 KB/s even minutes of backlog are small.
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    runtime.spawn(async move {
        let event = match run(url, config, audio_rx, &mut on_event).await {
            Ok(()) => Event::Finished,
            Err(e) => Event::Failed(format!("{e:#}")),
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
struct Stream {
    /// Audio sent but not yet finalized, resent after a reconnect.
    unfinalized: VecDeque<u8>,
    /// Position of `unfinalized[0]` on the stream's timeline, in ms.
    unfinalized_start_ms: u64,
    audio_ended: bool,
    sent_any_audio: bool,
}

impl Stream {
    /// Soniox finalized this session's audio up to `session_ms`; drop what
    /// no longer needs resending.
    fn finalized_up_to(&mut self, session_start_ms: u64, session_ms: u64) {
        let finalized = session_start_ms + session_ms;
        let drop_ms = finalized.saturating_sub(self.unfinalized_start_ms);
        let drop_bytes = (drop_ms * BYTES_PER_MS).min(self.unfinalized.len() as u64) as usize;
        self.unfinalized.drain(..drop_bytes);
        self.unfinalized_start_ms += drop_bytes as u64 / BYTES_PER_MS;
    }
}

async fn run(
    url: &str,
    config: Config,
    mut audio: mpsc::UnboundedReceiver<Vec<u8>>,
    on_event: &mut (impl FnMut(Event) + Send),
) -> Result<()> {
    let mut stream = Stream {
        unfinalized: VecDeque::new(),
        unfinalized_start_ms: 0,
        audio_ended: false,
        sent_any_audio: false,
    };
    let mut attempt = 0;
    loop {
        match session(url, &config, &mut audio, &mut stream, on_event).await {
            Ok(()) => return Ok(()),
            Err(SessionError::Fatal(e)) => return Err(e),
            Err(SessionError::Retry(e)) => {
                attempt += 1;
                if stream.audio_ended && attempt > RETRIES_AFTER_END {
                    return Err(e);
                }
                on_event(Event::Reconnecting {
                    attempt,
                    reason: format!("{e:#}"),
                });
                // 0.5 s, 1 s, 2 s, … capped at 15 s. Audio keeps queueing meanwhile.
                let backoff = Duration::from_millis(500 << (attempt - 1).min(5));
                tokio::time::sleep(backoff.min(Duration::from_secs(15))).await;
            }
        }
    }
}

/// One WebSocket connection. Returns `Ok` once Soniox reports `finished`.
async fn session(
    url: &str,
    config: &Config,
    audio: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    stream: &mut Stream,
    on_event: &mut (impl FnMut(Event) + Send),
) -> Result<(), SessionError> {
    let (socket, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url))
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
    tx.send(Message::Text(serde_json::to_string(&start)?.into()))
        .await
        .context("sending the stream config")?;
    on_event(Event::Connected);

    // This session's timestamps start at the first audio it receives.
    let session_start_ms = stream.unfinalized_start_ms;
    let replay: Vec<u8> = stream.unfinalized.iter().copied().collect();
    for frame in replay.chunks(FRAME_BYTES) {
        tx.send(Message::Binary(frame.to_vec().into()))
            .await
            .context("resending audio")?;
    }

    let end_stream = |deadline: &mut Option<Instant>| {
        *deadline = Some(Instant::now() + FINISH_TIMEOUT);
        // An empty frame tells Soniox the audio is complete. It must be a
        // text frame: the docs allow binary too, but Soniox ignored an empty
        // binary frame in testing and never sent `finished` (nor finalized
        // the last words).
        Message::Text(String::new().into())
    };
    // Set once the audio has ended: the latest we wait for `finished`.
    let mut deadline: Option<Instant> = None;
    if stream.audio_ended {
        tx.send(end_stream(&mut deadline))
            .await
            .context("ending the stream")?;
    }

    loop {
        tokio::select! {
            chunk = audio.recv(), if !stream.audio_ended => match chunk {
                Some(bytes) => {
                    stream.unfinalized.extend(&bytes);
                    stream.sent_any_audio = true;
                    tx.send(Message::Binary(bytes.into())).await.context("sending audio")?;
                }
                // Nothing was sent, so there is nothing to finalize.
                None if !stream.sent_any_audio => {
                    let _ = tx.close().await;
                    return Ok(());
                }
                None => {
                    stream.audio_ended = true;
                    tx.send(end_stream(&mut deadline)).await.context("ending the stream")?;
                }
            },
            message = rx.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    let response: Response = serde_json::from_str(&text)
                        .with_context(|| format!("unexpected message from Soniox: {text}"))?;
                    if let Some(code) = response.error_code {
                        let message = response.error_message.unwrap_or_default();
                        let error = anyhow!("Soniox error {code}: {message}");
                        // 408 timeout, 413 max stream length, 429 rate limit
                        // and 5xx are transient; the rest need the user.
                        return Err(match code {
                            408 | 413 | 429 | 500.. => SessionError::Retry(error),
                            _ => SessionError::Fatal(error),
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
                Some(Err(e)) => return Err(anyhow::Error::from(e).context("reading from Soniox").into()),
                None => return Err(anyhow!("Soniox closed the connection").into()),
            },
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                return Err(anyhow!("Soniox did not finish within {} s", FINISH_TIMEOUT.as_secs()).into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake Soniox that finalizes the first 500 ms, drops the connection,
    /// then expects the rest of the audio again on a second connection.
    #[tokio::test(flavor = "multi_thread")]
    async fn reconnects_and_resends_unfinalized_audio() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url: &'static str =
            Box::leak(format!("ws://{}", listener.local_addr().unwrap()).into_boxed_str());

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
        let config = Config {
            api_key: "key".into(),
            model: DEFAULT_MODEL.into(),
            language_hints: vec![],
            enable_speaker_diarization: false,
            enable_language_identification: false,
            enable_endpoint_detection: false,
            context: None,
            translation: None,
            client_reference_id: None,
        };
        let input = InputFormat {
            sample_rate: 16_000,
            channels: 1,
        };
        let mut feed = start_at(url, &Handle::current(), config, input, move |e| {
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
                Event::Failed(e) => panic!("stream failed: {e}"),
                Event::Connected => {}
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
