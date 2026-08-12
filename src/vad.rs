use crate::config::{VadConfig, PIPELINE_SAMPLE_RATE, VAD_CHUNK_SIZE};
use crate::resample::StreamResampler;
use anyhow::Context;
use crossbeam_channel::Receiver;
use voice_activity_detector::VoiceActivityDetector;

/// Milliseconds represented by one VAD chunk (512 samples @ 16kHz = 32ms).
const CHUNK_MS: u64 = (VAD_CHUNK_SIZE as u64 * 1000) / PIPELINE_SAMPLE_RATE as u64;
const TRACE_EVERY_CHUNKS: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Silence,
    Force,
    MaxCap,
    /// Segment ended by Alt+P; the dictation session stays open.
    Pause,
    /// Segment cut mid-speech to splice clipboard without dropping audio stream.
    Splice,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::Silence => "silence",
            StopReason::Force => "force",
            StopReason::MaxCap => "maxcap",
            StopReason::Pause => "pause",
            StopReason::Splice => "splice",
        }
    }
}

pub struct Utterance {
    /// Full utterance (preroll + speech + trailing audio), 16kHz mono i16.
    pub samples_16k: Vec<i16>,
    /// Mono samples received from the device at its native rate, for the
    /// [capture] evidence line.
    pub native_samples: u64,
    pub speech_ms: u64,
    pub total_ms: u64,
    pub reason: StopReason,
}

pub enum Control {
    ForceStop,
    Pause,
    CutSegment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    WaitingForSpeech,
    Speaking,
    TrailingSilence,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::WaitingForSpeech => "WaitingForSpeech",
            State::Speaking => "Speaking",
            State::TrailingSilence => "TrailingSilence",
        }
    }
}

/// Runs the VAD endpointing loop until an endpoint is reached, then returns
/// the utterance. Blocks; intended for a dedicated worker thread.
///
/// `endpointing` false = M1 manual mode: VAD state is still traced, but only
/// ForceStop (or the max-utterance cap) ends the capture.
pub fn run_endpointer(
    audio_rx: Receiver<Vec<f32>>,
    control_rx: Receiver<Control>,
    config: &VadConfig,
    input_rate: u32,
    endpointing: bool,
) -> anyhow::Result<Utterance> {
    let mut vad = VoiceActivityDetector::builder()
        .sample_rate(PIPELINE_SAMPLE_RATE as i64)
        .chunk_size(VAD_CHUNK_SIZE)
        .build()
        .context("failed to build Silero VAD")?;
    let mut resampler = StreamResampler::new(input_rate)?;

    let preroll_samples = (config.preroll_ms * PIPELINE_SAMPLE_RATE as u64 / 1000) as usize;
    let mut preroll: Vec<i16> = Vec::new(); // rolling, capped at preroll_samples
    let mut utterance: Vec<i16> = Vec::new();
    let mut chunk_buf: Vec<i16> = Vec::new();

    let mut state = State::WaitingForSpeech;
    let mut speech_ms: u64 = 0;
    let mut silence_ms: u64 = 0;
    let mut total_ms: u64 = 0;
    let mut chunks: u64 = 0;
    let mut native_samples: u64 = 0;

    let trace = |t_ms: u64, prob: f32, state: State, speech_ms: u64, silence_ms: u64| {
        eprintln!(
            "[vad] t={:.2}s prob={:.2} state={} speech_ms={} silence_ms={}",
            t_ms as f64 / 1000.0,
            prob,
            state.as_str(),
            speech_ms,
            silence_ms
        );
    };

    let finish = |reason: StopReason,
                  native_samples: u64,
                  mut preroll: Vec<i16>,
                  utterance: Vec<i16>,
                  speech_ms: u64,
                  total_ms: u64| {
        // If we never left WaitingForSpeech, the audio so far lives in preroll.
        let samples_16k = if utterance.is_empty() {
            preroll
        } else {
            let mut all = std::mem::take(&mut preroll);
            all.extend_from_slice(&utterance);
            // preroll was already folded in at speech onset; `preroll` is
            // empty here, this just avoids a clone.
            all
        };
        eprintln!(
            "[endpoint] reason={} total_ms={} speech_ms={}",
            reason.as_str(),
            total_ms,
            speech_ms
        );
        Utterance { samples_16k, native_samples, speech_ms, total_ms, reason }
    };

    loop {
        crossbeam_channel::select! {
            recv(control_rx) -> msg => {
                match msg {
                    Ok(Control::ForceStop) => {
                        return Ok(finish(StopReason::Force, native_samples, preroll, utterance, speech_ms, total_ms));
                    }
                    Ok(Control::Pause) => {
                        return Ok(finish(StopReason::Pause, native_samples, preroll, utterance, speech_ms, total_ms));
                    }
                    Ok(Control::CutSegment) => {
                        return Ok(finish(StopReason::Splice, native_samples, preroll, utterance, speech_ms, total_ms));
                    }
                    Err(_) => {}
                }
            }
            recv(audio_rx) -> msg => {
                let Ok(native) = msg else {
                    // Audio stream ended unexpectedly; treat as force stop.
                    return Ok(finish(StopReason::Force, native_samples, preroll, utterance, speech_ms, total_ms));
                };
                native_samples += native.len() as u64;
                let resampled = resampler.process(&native)?;
                chunk_buf.extend(resampled.iter().map(|&s| {
                    (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
                }));

                while chunk_buf.len() >= VAD_CHUNK_SIZE {
                    let chunk: Vec<i16> = chunk_buf.drain(..VAD_CHUNK_SIZE).collect();
                    let prob = vad.predict(chunk.clone());
                    let is_speech = prob > config.speech_threshold;
                    chunks += 1;
                    total_ms += CHUNK_MS;

                    let prev_state = state;
                    match state {
                        State::WaitingForSpeech => {
                            if is_speech {
                                // Prepend preroll so the first word isn't clipped.
                                utterance.append(&mut preroll);
                                utterance.extend_from_slice(&chunk);
                                state = State::Speaking;
                                speech_ms += CHUNK_MS;
                            } else {
                                preroll.extend_from_slice(&chunk);
                                let excess = preroll.len().saturating_sub(preroll_samples);
                                if excess > 0 {
                                    preroll.drain(..excess);
                                }
                            }
                        }
                        State::Speaking | State::TrailingSilence => {
                            utterance.extend_from_slice(&chunk);
                            if is_speech {
                                speech_ms += CHUNK_MS;
                                silence_ms = 0;
                                state = State::Speaking;
                            } else {
                                silence_ms += CHUNK_MS;
                                let armed = speech_ms >= config.min_speech_ms;
                                if armed {
                                    state = State::TrailingSilence;
                                }
                                if endpointing && armed && silence_ms >= config.endpoint_silence_ms {
                                    trace(total_ms, prob, state, speech_ms, silence_ms);
                                    return Ok(finish(
                                        StopReason::Silence, native_samples, preroll, utterance, speech_ms, total_ms,
                                    ));
                                }
                            }
                        }
                    }

                    if state != prev_state || chunks % TRACE_EVERY_CHUNKS == 0 {
                        trace(total_ms, prob, state, speech_ms, silence_ms);
                    }

                    if total_ms >= config.max_utterance_ms {
                        return Ok(finish(StopReason::MaxCap, native_samples, preroll, utterance, speech_ms, total_ms));
                    }
                }
            }
        }
    }
}
