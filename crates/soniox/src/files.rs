//! Soniox file (async) transcription, used to fill gaps from a recording:
//! upload a clip, transcribe it, fetch the tokens, then delete the clip and
//! the job on Soniox.
//!
//! API reference: <https://soniox.com/docs/stt/async/async-transcription>
//! Needs a key with "Speech-to-text, async: Write" and "Files: Write".

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use reqwest::{Client, Response, StatusCode, multipart};
use serde::{Deserialize, Serialize};

use crate::{Context, Token, Translation};

const API: &str = "https://api.soniox.com/v1";
pub const MODEL: &str = "stt-async-v5";
const POLL_EVERY: Duration = Duration::from_secs(2);
const GIVE_UP_AFTER: Duration = Duration::from_secs(30 * 60);

/// Options for one file. Mirrors the live [`crate::Config`] minus the audio
/// format (Soniox detects it) and endpoint detection (live only).
#[derive(Clone, Serialize)]
pub struct FileConfig {
    pub model: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub language_hints: Vec<String>,
    pub enable_speaker_diarization: bool,
    pub enable_language_identification: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Context>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub translation: Option<Translation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_reference_id: Option<String>,
}

/// Something still stored on Soniox that should be deleted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Leftover {
    File(String),
    Transcription(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Uploading,
    Queued,
    Transcribing,
    Downloading,
}

pub struct Outcome {
    /// Timestamps are relative to the start of the uploaded clip.
    pub tokens: Result<Vec<Token>>,
    /// What couldn't be deleted (e.g. while Soniox was still processing).
    /// Retry later with [`delete`].
    pub leftovers: Vec<Leftover>,
}

#[derive(Serialize)]
struct CreateRequest<'a> {
    #[serde(flatten)]
    config: &'a FileConfig,
    file_id: &'a str,
}

#[derive(Deserialize)]
struct Created {
    id: String,
}

#[derive(Deserialize)]
struct Status {
    status: String,
    error_message: Option<String>,
}

#[derive(Deserialize)]
struct Transcript {
    tokens: Vec<Token>,
}

/// Encodes interleaved samples as a 16 kHz mono 16-bit WAV file, the
/// smallest format that keeps full speech quality for Soniox.
pub fn wav_clip(interleaved: &[f32], sample_rate: u32, channels: u16) -> Result<Vec<u8>> {
    let mut encoder = crate::encoder::PcmEncoder::new(sample_rate, channels)?;
    let mut pcm = Vec::new();
    encoder.push(interleaved, &mut pcm)?;
    encoder.finish(&mut pcm)?;
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: crate::encoder::SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wav = std::io::Cursor::new(Vec::with_capacity(pcm.len() + 44));
    {
        let mut writer = hound::WavWriter::new(&mut wav, spec)?;
        for sample in pcm.as_chunks::<2>().0 {
            writer.write_sample(i16::from_le_bytes(*sample))?;
        }
        writer.finalize()?;
    }
    Ok(wav.into_inner())
}

/// Transcribes one WAV clip, and cleans up on Soniox whether or not it
/// worked.
pub async fn transcribe(
    api_key: &str,
    wav: Vec<u8>,
    file_name: &str,
    config: &FileConfig,
    mut on_stage: impl FnMut(Stage) + Send,
) -> Outcome {
    let client = Client::new();
    let mut created = Vec::new();
    let tokens = run(
        &client,
        api_key,
        wav,
        file_name,
        config,
        &mut on_stage,
        &mut created,
    )
    .await;
    // The job first: deleting the file while the job runs would fail it.
    created.reverse();
    let mut leftovers = Vec::new();
    for item in created {
        if delete_with(&client, api_key, &item).await.is_err() {
            leftovers.push(item);
        }
    }
    Outcome { tokens, leftovers }
}

async fn run(
    client: &Client,
    api_key: &str,
    wav: Vec<u8>,
    file_name: &str,
    config: &FileConfig,
    on_stage: &mut (impl FnMut(Stage) + Send),
    created: &mut Vec<Leftover>,
) -> Result<Vec<Token>> {
    on_stage(Stage::Uploading);
    let part = multipart::Part::bytes(wav)
        .file_name(file_name.to_owned())
        .mime_str("audio/wav")?;
    let mut form = multipart::Form::new().part("file", part);
    if let Some(reference) = &config.client_reference_id {
        form = form.text("client_reference_id", reference.clone());
    }
    let file: Created = ok(client
        .post(format!("{API}/files"))
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .await
        .context("uploading the clip")?)
    .await?
    .json()
    .await?;
    created.push(Leftover::File(file.id.clone()));

    let job: Created = ok(client
        .post(format!("{API}/transcriptions"))
        .bearer_auth(api_key)
        .json(&CreateRequest {
            config,
            file_id: &file.id,
        })
        .send()
        .await
        .context("starting the transcription")?)
    .await?
    .json()
    .await?;
    created.push(Leftover::Transcription(job.id.clone()));

    let started = tokio::time::Instant::now();
    loop {
        let status: Status = ok(client
            .get(format!("{API}/transcriptions/{}", job.id))
            .bearer_auth(api_key)
            .send()
            .await
            .context("checking the transcription")?)
        .await?
        .json()
        .await?;
        match status.status.as_str() {
            "completed" => break,
            // The docs use both names for a failed job.
            "error" | "failed" => bail!(
                "Soniox couldn't transcribe the clip: {}",
                status.error_message.unwrap_or_default()
            ),
            "queued" => on_stage(Stage::Queued),
            _ => on_stage(Stage::Transcribing),
        }
        if started.elapsed() > GIVE_UP_AFTER {
            bail!("Soniox took over {} min", GIVE_UP_AFTER.as_secs() / 60);
        }
        tokio::time::sleep(POLL_EVERY).await;
    }

    on_stage(Stage::Downloading);
    let transcript: Transcript = ok(client
        .get(format!("{API}/transcriptions/{}/transcript", job.id))
        .bearer_auth(api_key)
        .send()
        .await
        .context("downloading the transcript")?)
    .await?
    .json()
    .await?;
    Ok(transcript.tokens)
}

/// Deletes something left on Soniox. Already gone counts as done.
pub async fn delete(api_key: &str, item: &Leftover) -> Result<()> {
    delete_with(&Client::new(), api_key, item).await
}

async fn delete_with(client: &Client, api_key: &str, item: &Leftover) -> Result<()> {
    let url = match item {
        Leftover::File(id) => format!("{API}/files/{id}"),
        Leftover::Transcription(id) => format!("{API}/transcriptions/{id}"),
    };
    let response = client.delete(url).bearer_auth(api_key).send().await?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(());
    }
    ok(response).await?;
    Ok(())
}

/// Turns an HTTP error status into an error carrying Soniox's message.
async fn ok(response: Response) -> Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(anyhow!("Soniox returned {status}: {body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_request_matches_the_api() {
        let config = FileConfig {
            model: MODEL.into(),
            language_hints: vec!["vi".into()],
            enable_speaker_diarization: true,
            enable_language_identification: true,
            context: None,
            translation: Some(Translation::OneWay {
                target_language: "en".into(),
            }),
            client_reference_id: Some("rec/system".into()),
        };
        let json = serde_json::to_value(CreateRequest {
            config: &config,
            file_id: "f-1",
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "model": "stt-async-v5",
                "language_hints": ["vi"],
                "enable_speaker_diarization": true,
                "enable_language_identification": true,
                "translation": { "type": "one_way", "target_language": "en" },
                "client_reference_id": "rec/system",
                "file_id": "f-1",
            })
        );
    }

    #[test]
    fn wav_clip_is_16k_mono_and_keeps_the_length() {
        let second_48k_stereo = vec![0.2f32; 48_000 * 2];
        let wav = wav_clip(&second_48k_stereo, 48_000, 2).unwrap();
        let reader = hound::WavReader::new(std::io::Cursor::new(wav)).unwrap();
        let spec = reader.spec();
        assert_eq!(
            (spec.sample_rate, spec.channels, spec.bits_per_sample),
            (16_000, 1, 16)
        );
        // One second, give or take the resampler's padding and delay.
        assert!(
            (15_900..=16_400).contains(&reader.duration()),
            "{}",
            reader.duration()
        );
    }

    #[test]
    fn leftovers_round_trip_for_the_retry_file() {
        let items = vec![
            Leftover::File("a".into()),
            Leftover::Transcription("b".into()),
        ];
        let json = serde_json::to_string(&items).unwrap();
        assert_eq!(
            json,
            r#"[{"kind":"file","id":"a"},{"kind":"transcription","id":"b"}]"#
        );
        assert_eq!(serde_json::from_str::<Vec<Leftover>>(&json).unwrap(), items);
    }
}
