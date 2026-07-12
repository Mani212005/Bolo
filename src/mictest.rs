//! 3-second mic test: record → resample → transcribe with the given config.
//! Blocking; run it on its own thread (extracted from the retired TUI).

use crate::config::Config;
use std::time::{Duration, Instant};

pub fn run(cfg: &Config, seconds: u64) -> anyhow::Result<String> {
    let (audio_tx, audio_rx) = crossbeam_channel::unbounded::<Vec<f32>>();
    let (stream, info) = crate::audio::start_capture(audio_tx)?;
    let mut resampler = crate::resample::StreamResampler::new(info.sample_rate)?;
    let mut samples: Vec<i16> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if let Ok(chunk) = audio_rx.recv_timeout(Duration::from_millis(100)) {
            let resampled = resampler.process(&chunk)?;
            samples
                .extend(resampled.iter().map(|&s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16));
        }
    }
    drop(stream);
    anyhow::ensure!(!samples.is_empty(), "no audio captured (mic wedged?)");
    let wav = crate::stt::groq::encode_wav(&samples)?;
    let provider = crate::stt::make_provider(cfg)?;
    let runtime = tokio::runtime::Runtime::new()?;
    let transcript = runtime.block_on(provider.transcribe(wav))?;
    Ok(transcript.text)
}
