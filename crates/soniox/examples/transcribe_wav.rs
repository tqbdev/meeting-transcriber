//! Streams a WAV file to Soniox at real-time speed and prints the transcript.
//!
//! SONIOX_API_KEY=... cargo run -p soniox --example transcribe_wav -- path/to/file.wav

use std::{sync::mpsc, time::Duration};

use anyhow::{Context, Result, bail};
use soniox::{Event, InputFormat};

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: transcribe_wav <file.wav>")?;
    let api_key = std::env::var("SONIOX_API_KEY").context("set SONIOX_API_KEY")?;

    let mut reader = hound::WavReader::open(&path)?;
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|s| s as f32 / scale))
                .collect::<Result<_, _>>()?
        }
    };

    let runtime = tokio::runtime::Runtime::new()?;
    let (tx, rx) = mpsc::channel();
    let config = soniox::Config {
        api_key,
        model: soniox::DEFAULT_MODEL.to_owned(),
        language_hints: vec!["en".to_owned()],
        enable_speaker_diarization: true,
        enable_language_identification: true,
        enable_endpoint_detection: false,
        context: None,
        translation: None,
        client_reference_id: Some("transcribe_wav example".to_owned()),
    };
    let input = InputFormat {
        sample_rate: spec.sample_rate,
        channels: spec.channels,
    };
    let mut feed = soniox::start(runtime.handle(), config, input, move |event| {
        let _ = tx.send(event);
    })?;

    // 100 ms at a time, paced like live capture.
    let chunk = spec.sample_rate as usize / 10 * spec.channels as usize;
    for piece in samples.chunks(chunk) {
        feed.push(piece);
        std::thread::sleep(Duration::from_millis(100));
        print_events(&rx)?;
    }
    drop(feed);

    loop {
        match rx.recv_timeout(Duration::from_secs(20))? {
            Event::Finished => {
                println!("\n[finished]");
                return Ok(());
            }
            Event::Failed(message) => bail!(message),
            event => handle(event),
        }
    }
}

fn print_events(rx: &mpsc::Receiver<Event>) -> Result<()> {
    while let Ok(event) = rx.try_recv() {
        if let Event::Failed(message) = event {
            bail!(message);
        }
        handle(event);
    }
    Ok(())
}

fn handle(event: Event) {
    match event {
        Event::Connected => println!("[connected]"),
        Event::Reconnecting { attempt, reason } => {
            println!("\n[reconnecting, attempt {attempt}: {reason}]")
        }
        Event::Tokens { finals, .. } => {
            for token in finals {
                print!("{}", token.text);
            }
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
        Event::Finished | Event::Failed(_) => {}
    }
}
