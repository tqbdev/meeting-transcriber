//! Converts captured audio into what we send Soniox: 16 kHz mono `pcm_s16le`.

use anyhow::{Result, anyhow};
use audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

pub(crate) const SAMPLE_RATE: u32 = 16_000;

/// Frames fed to the resampler per call.
const CHUNK_FRAMES: usize = 1024;

pub(crate) struct PcmEncoder {
    channels: usize,
    /// `None` when the input is already 16 kHz.
    resampler: Option<Fft<f32>>,
    /// Mono input frames waiting for a full resampler chunk.
    pending: Vec<f32>,
    scratch: Vec<f32>,
}

impl PcmEncoder {
    pub(crate) fn new(sample_rate: u32, channels: u16) -> Result<Self> {
        if channels == 0 {
            return Err(anyhow!("audio stream has no channels"));
        }
        let resampler = if sample_rate == SAMPLE_RATE {
            None
        } else {
            Some(Fft::<f32>::new(
                sample_rate as usize,
                SAMPLE_RATE as usize,
                CHUNK_FRAMES,
                1,
                FixedSync::Input,
            )?)
        };
        let scratch = vec![0.0; resampler.as_ref().map_or(0, |r| r.output_frames_max())];
        Ok(Self {
            channels: channels as usize,
            resampler,
            pending: Vec::new(),
            scratch,
        })
    }

    /// Takes interleaved f32 samples at the device rate and appends the
    /// encoded 16 kHz mono bytes to `out`.
    pub(crate) fn push(&mut self, interleaved: &[f32], out: &mut Vec<u8>) -> Result<()> {
        let channels = self.channels;
        self.pending.extend(
            interleaved
                .chunks_exact(channels)
                .map(|frame| frame.iter().sum::<f32>() / channels as f32),
        );

        let Some(resampler) = &mut self.resampler else {
            write_s16le(&self.pending, out);
            self.pending.clear();
            return Ok(());
        };

        let mut consumed = 0;
        loop {
            let needed = resampler.input_frames_next();
            if self.pending.len() - consumed < needed {
                break;
            }
            let input =
                InterleavedSlice::new(&self.pending[consumed..consumed + needed], 1, needed)?;
            let frames_out = self.scratch.len();
            let mut output = InterleavedSlice::new_mut(&mut self.scratch, 1, frames_out)?;
            let (_, written) = resampler
                .process_into_buffer(&input, &mut output, None)
                .map_err(|e| anyhow!("resampling: {e}"))?;
            write_s16le(&self.scratch[..written], out);
            consumed += needed;
        }
        self.pending.drain(..consumed);
        Ok(())
    }
}

impl PcmEncoder {
    /// Flushes audio still waiting for a full resampler chunk, padding the
    /// last chunk with silence.
    pub(crate) fn finish(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let Some(resampler) = &self.resampler else {
            return Ok(());
        };
        if self.pending.is_empty() {
            return Ok(());
        }
        let needed = resampler.input_frames_next();
        let padding = needed.saturating_sub(self.pending.len()) * self.channels;
        self.push(&vec![0.0; padding], out)
    }
}

fn write_s16le(samples: &[f32], out: &mut Vec<u8>) {
    out.reserve(samples.len() * 2);
    for &sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        out.extend_from_slice(&value.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmixes_and_resamples_48k_stereo_to_16k_mono() {
        let mut encoder = PcmEncoder::new(48_000, 2).unwrap();
        let mut out = Vec::new();
        // One second of stereo audio, pushed in uneven pieces like the writer thread does.
        let second = vec![0.25f32; 48_000 * 2];
        for piece in second.chunks(960 * 2 + 6) {
            encoder.push(piece, &mut out).unwrap();
        }
        let samples = out.len() / 2;
        // Input still waiting for a full resampler chunk isn't encoded yet,
        // and the resampler holds back its filter delay.
        let consumed = 48_000 - encoder.pending.len();
        let delay = encoder.resampler.as_ref().unwrap().output_delay();
        let expected = consumed / 3 - delay;
        assert!(samples.abs_diff(expected) <= 8, "{samples} vs {expected}");
    }

    #[test]
    fn passes_16k_mono_through() {
        let mut encoder = PcmEncoder::new(16_000, 1).unwrap();
        let mut out = Vec::new();
        encoder.push(&[0.0, 0.5, -1.0, 2.0], &mut out).unwrap();
        let values: Vec<i16> = out
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&b| i16::from_le_bytes(b))
            .collect();
        assert_eq!(values, [0, 16383, -32767, 32767]);
    }
}
