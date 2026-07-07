use crate::config::PIPELINE_SAMPLE_RATE;
use anyhow::Context;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

const INPUT_CHUNK: usize = 1024;

/// Streaming native-rate mono f32 -> 16kHz mono f32 resampler.
/// Accepts arbitrary-length input; buffers internally to rubato's fixed
/// input chunk size.
pub struct StreamResampler {
    inner: Option<SincFixedIn<f32>>, // None when input is already 16kHz
    pending: Vec<f32>,
}

impl StreamResampler {
    pub fn new(input_rate: u32) -> anyhow::Result<Self> {
        let inner = if input_rate == PIPELINE_SAMPLE_RATE {
            None
        } else {
            let params = SincInterpolationParameters {
                sinc_len: 128,
                f_cutoff: 0.95,
                interpolation: SincInterpolationType::Linear,
                oversampling_factor: 256,
                window: WindowFunction::BlackmanHarris2,
            };
            Some(
                SincFixedIn::<f32>::new(
                    PIPELINE_SAMPLE_RATE as f64 / input_rate as f64,
                    2.0,
                    params,
                    INPUT_CHUNK,
                    1,
                )
                .context("failed to create resampler")?,
            )
        };
        Ok(Self { inner, pending: Vec::new() })
    }

    /// Feed native-rate samples, get whatever 16kHz output is ready.
    pub fn process(&mut self, input: &[f32]) -> anyhow::Result<Vec<f32>> {
        let Some(resampler) = self.inner.as_mut() else {
            return Ok(input.to_vec());
        };
        self.pending.extend_from_slice(input);
        let mut out = Vec::new();
        while self.pending.len() >= INPUT_CHUNK {
            let chunk: Vec<f32> = self.pending.drain(..INPUT_CHUNK).collect();
            let mut result = resampler.process(&[chunk], None)?;
            out.append(&mut result[0]);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec §2.1 sanity check: a 440Hz sine at 48kHz must come out of the
    /// resampler still at 440Hz (measured by zero-crossing rate at 16kHz).
    #[test]
    fn sine_440hz_survives_resampling() {
        let input_rate = 48_000u32;
        let secs = 2.0;
        let input: Vec<f32> = (0..(input_rate as f64 * secs) as usize)
            .map(|i| (2.0 * std::f64::consts::PI * 440.0 * i as f64 / input_rate as f64).sin() as f32)
            .collect();

        let mut rs = StreamResampler::new(input_rate).unwrap();
        let out = rs.process(&input).unwrap();

        let expected_len = (PIPELINE_SAMPLE_RATE as f64 * secs) as usize;
        assert!(
            (out.len() as i64 - expected_len as i64).unsigned_abs() < 2048,
            "expected ~{expected_len} samples at 16kHz, got {}",
            out.len()
        );

        // 440Hz has 880 zero crossings/sec; skip the filter warm-up at the start.
        let steady = &out[PIPELINE_SAMPLE_RATE as usize / 4..];
        let crossings = steady.windows(2).filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0)).count();
        let hz = crossings as f64 / 2.0 / (steady.len() as f64 / PIPELINE_SAMPLE_RATE as f64);
        assert!(
            (hz - 440.0).abs() < 5.0,
            "expected ~440Hz after resampling, measured {hz:.1}Hz"
        );
    }
}
