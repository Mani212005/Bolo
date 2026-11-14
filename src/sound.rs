//! Sound cues: start and stop of a dictation, and a screenshot being taken.
//!
//! The cues are synthesised by `scripts/sounds/generate.py` and committed as
//! `assets/sounds/*.wav`. Playing one must be instant, so nothing happens on
//! the play path but queueing: one cpal output stream on the default output
//! device is opened when the daemon starts and stays open, its callback mixes
//! the queued cues into silence, and the cues are decoded and converted to the
//! device's rate once, ahead of time. There is no process to spawn (the old
//! `afplay` path cost tens of milliseconds before the first sample) and no
//! file to read. A watcher thread reopens the stream when the default output
//! device or its format changes (headphones in, Bluetooth profile switch).
//!
//! The cues go through the normal output device, so the system's mute and
//! volume apply; `[daemon] sounds` and `sound_volume` apply on top, live.

use crate::config::{DaemonConfig, DEFAULT_SOUND_VOLUME};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use crossbeam_channel::{Receiver, Sender};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const START_WAV: &[u8] = include_bytes!("../assets/sounds/start.wav");
const STOP_WAV: &[u8] = include_bytes!("../assets/sounds/stop.wav");
const CAPTURE_WAV: &[u8] = include_bytes!("../assets/sounds/capture.wav");

/// Output buffer to ask for, in frames: 2.7 ms at 48 kHz. Smaller than the
/// default 512 for a few ms less delay; idle CPU is the same (about 0.6% of a core).
const BUFFER_FRAMES: u32 = 128;
/// How often the watcher checks that the stream still matches the default device.
const WATCH_EVERY: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cue {
    /// Dictation started (also when resumed from a pause).
    Start,
    /// Dictation stopped.
    Stop,
    /// A screenshot was taken.
    Capture,
}

impl Cue {
    const ALL: [Cue; 3] = [Cue::Start, Cue::Stop, Cue::Capture];

    fn name(self) -> &'static str {
        match self {
            Cue::Start => "start",
            Cue::Stop => "stop",
            Cue::Capture => "capture",
        }
    }

    fn index(self) -> usize {
        self as usize
    }

    fn wav(self) -> &'static [u8] {
        match self {
            Cue::Start => START_WAV,
            Cue::Stop => STOP_WAV,
            Cue::Capture => CAPTURE_WAV,
        }
    }
}

// ---------------------------------------------------------------------------
// Settings

static ENABLED: AtomicBool = AtomicBool::new(true);
static VOLUME_BITS: AtomicU32 = AtomicU32::new(0);

/// Applies `[daemon] sounds` and `sound_volume`; takes effect on the next cue.
pub fn configure(daemon: &DaemonConfig) {
    ENABLED.store(daemon.sounds, Ordering::Relaxed);
    VOLUME_BITS.store(
        sanitize_volume(daemon.sound_volume).to_bits(),
        Ordering::Relaxed,
    );
}

/// A volume from the config file, forced into 0..=1 (NaN gets the default).
fn sanitize_volume(volume: f32) -> f32 {
    if volume.is_finite() {
        volume.clamp(0.0, 1.0)
    } else {
        DEFAULT_SOUND_VOLUME
    }
}

fn volume() -> f32 {
    match VOLUME_BITS.load(Ordering::Relaxed) {
        0 => DEFAULT_SOUND_VOLUME,
        bits => f32::from_bits(bits),
    }
}

/// Linear gain for a 0..=1 volume setting. Squared, so the slider follows
/// loudness (the cues peak at -6 dBFS or lower before this).
fn gain_for(volume: f32) -> f32 {
    let v = volume.clamp(0.0, 1.0);
    v * v
}

// ---------------------------------------------------------------------------
// Engine

/// A cue being mixed out.
struct Voice {
    cue: Cue,
    samples: Arc<[f32]>,
    pos: usize,
    gain: f32,
    /// When the caller asked for the cue.
    requested: Instant,
    /// Whether its first buffer has been handed to the device yet.
    reported: bool,
}

/// What it took to get a cue's first sample to the device.
struct Timing {
    cue: Cue,
    /// Linear gain it plays at (the volume setting, squared).
    gain: f32,
    /// Request until the callback that carries the first sample.
    queued: Duration,
    /// That callback until the sample leaves the device.
    output: Duration,
}

/// The cues converted to the open stream's rate and channel count.
struct Rendered {
    cues: [Arc<[f32]>; 3],
}

struct Engine {
    voices: Mutex<Vec<Voice>>,
    rendered: Mutex<Option<Rendered>>,
    /// Set by the stream's error callback; the watcher then reopens it.
    broken: AtomicBool,
}

static ENGINE: OnceLock<Arc<Engine>> = OnceLock::new();

/// Opens the output stream and starts the watcher. Idempotent; the daemon
/// calls it at startup so the first cue does not pay for it.
pub fn init() {
    engine();
}

fn engine() -> &'static Arc<Engine> {
    ENGINE.get_or_init(|| {
        let engine = Arc::new(Engine {
            voices: Mutex::new(Vec::new()),
            rendered: Mutex::new(None),
            broken: AtomicBool::new(false),
        });
        let (timing_tx, timing_rx) = crossbeam_channel::unbounded();
        let watcher = Arc::clone(&engine);
        let spawned = std::thread::Builder::new()
            .name("bolo-sound".into())
            .spawn(move || watch(watcher, timing_tx, timing_rx));
        if let Err(e) = spawned {
            eprintln!("[sound] cannot start the audio thread: {e}");
        }
        engine
    })
}

/// Plays `cue` now, if sounds are on. Never blocks.
pub fn play(cue: Cue) {
    play_since(cue, Instant::now());
}

/// Plays `cue`, measuring its latency from `trigger`: the moment the user's
/// action was noticed, which may be earlier than this call.
pub fn play_since(cue: Cue, trigger: Instant) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let gain = gain_for(volume());
    if gain <= 0.0 {
        return;
    }
    let engine = engine();
    let samples = match engine.rendered.lock() {
        Ok(rendered) => rendered.as_ref().map(|r| Arc::clone(&r.cues[cue.index()])),
        Err(_) => None,
    };
    let Some(samples) = samples else {
        eprintln!("[sound] {} cue dropped: no output device open", cue.name());
        return;
    };
    if let Ok(mut voices) = engine.voices.lock() {
        voices.push(Voice {
            cue,
            samples,
            pos: 0,
            gain,
            requested: trigger,
            reported: false,
        });
    }
}

/// Owns the stream (a cpal `Stream` is not `Send`): opens it, and reopens it
/// when the default output device or its format changes or the stream dies.
fn watch(engine: Arc<Engine>, timing_tx: Sender<Timing>, timing_rx: Receiver<Timing>) {
    let decoded = decode_cues();
    let mut open: Option<(cpal::Stream, DeviceKey)> = None;
    loop {
        let current = probe();
        let stale = match (&open, &current) {
            (Some((_, key)), Some(now)) => key != now || engine.broken.load(Ordering::Relaxed),
            (None, Some(_)) => true,
            (Some(_), None) => true,
            (None, None) => false,
        };
        if stale {
            open = None;
            *engine.rendered.lock().unwrap() = None;
            engine.broken.store(false, Ordering::Relaxed);
            if let Some(key) = current {
                match build_stream(&engine, &timing_tx, &key, &decoded) {
                    Ok(stream) => {
                        eprintln!(
                            "[sound] output \"{}\" {} Hz {} ch",
                            key.name, key.rate, key.channels
                        );
                        open = Some((stream, key));
                    }
                    Err(e) => eprintln!("[sound] cannot open the output device: {e:#}"),
                }
            }
        }
        match timing_rx.recv_timeout(WATCH_EVERY) {
            Ok(t) => report(&t),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        }
        // Drain whatever else arrived before polling the device again.
        while let Ok(t) = timing_rx.try_recv() {
            report(&t);
        }
    }
}

fn report(t: &Timing) {
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    eprintln!(
        "[sound] {} cue at {:.0} dB: {:.1} ms to the first buffer + {:.1} ms output = {:.1} ms from trigger to speaker",
        t.cue.name(),
        20.0 * t.gain.log10(),
        ms(t.queued),
        ms(t.output),
        ms(t.queued + t.output)
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeviceKey {
    name: String,
    rate: u32,
    channels: u16,
}

fn probe() -> Option<DeviceKey> {
    let device = cpal::default_host().default_output_device()?;
    let config = device.default_output_config().ok()?;
    Some(DeviceKey {
        name: device.name().unwrap_or_else(|_| "<unknown>".into()),
        rate: config.sample_rate().0,
        channels: config.channels(),
    })
}

fn build_stream(
    engine: &Arc<Engine>,
    timing_tx: &Sender<Timing>,
    key: &DeviceKey,
    decoded: &[Vec<f32>; 3],
) -> anyhow::Result<cpal::Stream> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no default output device"))?;
    let supported = device.default_output_config()?;
    let mut config: cpal::StreamConfig = supported.config();
    if let cpal::SupportedBufferSize::Range { min, max } = supported.buffer_size() {
        if (*min..=*max).contains(&BUFFER_FRAMES) {
            config.buffer_size = cpal::BufferSize::Fixed(BUFFER_FRAMES);
        }
    }
    let rendered = Rendered {
        cues: Cue::ALL.map(|cue| {
            Arc::from(render_for(
                &decoded[cue.index()],
                SOURCE_RATE,
                key.rate,
                key.channels as usize,
            ))
        }),
    };
    let stream = match supported.sample_format() {
        SampleFormat::F32 => output_stream::<f32>(&device, &config, engine, timing_tx)?,
        SampleFormat::I16 => output_stream::<i16>(&device, &config, engine, timing_tx)?,
        SampleFormat::U16 => output_stream::<u16>(&device, &config, engine, timing_tx)?,
        SampleFormat::I32 => output_stream::<i32>(&device, &config, engine, timing_tx)?,
        other => anyhow::bail!("unsupported output sample format {other:?}"),
    };
    stream.play()?;
    *engine.rendered.lock().unwrap() = Some(rendered);
    Ok(stream)
}

fn output_stream<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    engine: &Arc<Engine>,
    timing_tx: &Sender<Timing>,
) -> anyhow::Result<cpal::Stream> {
    let callback_engine = Arc::clone(engine);
    let error_engine = Arc::clone(engine);
    let timing_tx = timing_tx.clone();
    let mut mix: Vec<f32> = Vec::with_capacity(16_384);
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            mix.clear();
            mix.resize(data.len(), 0.0);
            // A cue queued this very instant waits one buffer rather than
            // blocking the audio thread on the lock.
            if let Ok(mut voices) = callback_engine.voices.try_lock() {
                let stamp = info.timestamp();
                let output = stamp
                    .playback
                    .duration_since(&stamp.callback)
                    .unwrap_or_default();
                mix_voices(&mut voices, &mut mix, |voice| {
                    let _ = timing_tx.try_send(Timing {
                        cue: voice.cue,
                        gain: voice.gain,
                        queued: voice.requested.elapsed(),
                        output,
                    });
                });
            }
            for (out, &sample) in data.iter_mut().zip(mix.iter()) {
                *out = T::from_sample(sample.clamp(-1.0, 1.0));
            }
        },
        move |e| {
            eprintln!("[sound] output stream error: {e}");
            error_engine.broken.store(true, Ordering::Relaxed);
        },
        None,
    )?;
    Ok(stream)
}

/// Adds each voice's next slice into `out` and drops finished voices.
/// `first_buffer` is called once per voice as its first slice goes out.
fn mix_voices(voices: &mut Vec<Voice>, out: &mut [f32], mut first_buffer: impl FnMut(&Voice)) {
    for voice in voices.iter_mut() {
        if !voice.reported {
            voice.reported = true;
            first_buffer(voice);
        }
        let n = out.len().min(voice.samples.len() - voice.pos);
        for (o, &s) in out[..n].iter_mut().zip(&voice.samples[voice.pos..]) {
            *o += s * voice.gain;
        }
        voice.pos += n;
    }
    voices.retain(|voice| voice.pos < voice.samples.len());
}

// ---------------------------------------------------------------------------
// Decoding and conversion

/// Sample rate the cue WAVs are written at.
const SOURCE_RATE: u32 = 48_000;

fn decode_cues() -> [Vec<f32>; 3] {
    Cue::ALL.map(|cue| decode_wav(cue.wav()).unwrap_or_default())
}

/// Mono f32 samples of a 16-bit WAV.
fn decode_wav(bytes: &[u8]) -> anyhow::Result<Vec<f32>> {
    let mut reader = hound::WavReader::new(std::io::Cursor::new(bytes))?;
    let spec = reader.spec();
    anyhow::ensure!(
        spec.channels == 1 && spec.bits_per_sample == 16 && spec.sample_rate == SOURCE_RATE,
        "cue WAVs are 48 kHz 16-bit mono"
    );
    Ok(reader
        .samples::<i16>()
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|s| s as f32 / 32768.0)
        .collect())
}

/// Converts a mono clip to `rate`, then spreads it over `channels` interleaved.
fn render_for(mono: &[f32], from: u32, to: u32, channels: usize) -> Vec<f32> {
    let converted = if from == to {
        mono.to_vec()
    } else {
        resample(mono, from, to)
    };
    let mut out = Vec::with_capacity(converted.len() * channels);
    for s in converted {
        out.extend(std::iter::repeat_n(s, channels));
    }
    out
}

/// Windowed-sinc resampler for a whole short clip. Done once per device
/// change, so it favours quality over speed; unlike a streaming resampler it
/// adds no delay before the first sample.
fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    const HALF_TAPS: i64 = 32;
    let ratio = to as f64 / from as f64;
    let cutoff = ratio.min(1.0) * 0.97;
    let out_len = (input.len() as f64 * ratio).round() as usize;
    let sinc = |x: f64| {
        if x.abs() < 1e-9 {
            1.0
        } else {
            (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
        }
    };
    (0..out_len)
        .map(|i| {
            let centre = i as f64 / ratio;
            let base = centre.floor() as i64;
            let mut acc = 0.0f64;
            for k in (base - HALF_TAPS + 1)..=(base + HALF_TAPS) {
                if k < 0 || k as usize >= input.len() {
                    continue;
                }
                let d = centre - k as f64;
                let w = 0.42
                    + 0.5 * (std::f64::consts::PI * d / HALF_TAPS as f64).cos()
                    + 0.08 * (2.0 * std::f64::consts::PI * d / HALF_TAPS as f64).cos();
                acc += input[k as usize] as f64 * cutoff * sinc(cutoff * d) * w;
            }
            acc as f32
        })
        .collect()
}

// ---------------------------------------------------------------------------
// `bolo sound-test`

/// Plays each requested cue `repeat` times through the daemon's engine and
/// prints how long each took from request to speaker.
pub fn self_test(cues: &[Cue], repeat: usize) {
    init();
    // Wait for the stream to open.
    let deadline = Instant::now() + Duration::from_secs(3);
    while engine()
        .rendered
        .lock()
        .map(|r| r.is_none())
        .unwrap_or(true)
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    for _ in 0..repeat {
        for &cue in cues {
            play(cue);
            std::thread::sleep(Duration::from_millis(450));
        }
    }
}

pub fn parse_cue(name: &str) -> Option<Cue> {
    Cue::ALL.into_iter().find(|cue| cue.name() == name)
}

pub fn cue_names() -> &'static str {
    "start, stop, capture"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(cue: Cue) -> Vec<f32> {
        decode_wav(cue.wav()).expect("cue WAV decodes")
    }

    fn peak(s: &[f32]) -> f32 {
        s.iter().fold(0.0, |m, v| m.max(v.abs()))
    }

    /// RMS of the samples above 2% of the peak: the generator's loudness measure.
    fn active_rms_db(s: &[f32]) -> f32 {
        let floor = 0.02 * peak(s);
        let active: Vec<f32> = s.iter().copied().filter(|v| v.abs() >= floor).collect();
        let mean = active.iter().map(|v| v * v).sum::<f32>() / active.len() as f32;
        10.0 * mean.log10()
    }

    #[test]
    fn cues_are_short_quiet_and_clickless() {
        for cue in Cue::ALL {
            let s = samples(cue);
            let ms = s.len() as f32 * 1000.0 / SOURCE_RATE as f32;
            assert!((80.0..=250.0).contains(&ms), "{}: {ms} ms", cue.name());
            assert!(peak(&s) <= 0.5, "{}: peak over -6 dBFS", cue.name());
            assert_eq!(s[0], 0.0, "{}: must start at zero", cue.name());
            assert_eq!(*s.last().unwrap(), 0.0, "{}: must end at zero", cue.name());
            let step = s
                .windows(2)
                .map(|w| (w[1] - w[0]).abs())
                .fold(0.0, f32::max);
            assert!(step < 0.2, "{}: click risk, step {step}", cue.name());
            // Sound within 1.5 ms: playback adds no silence of its own.
            let audible = s.iter().position(|v| v.abs() > 0.01 * peak(&s)).unwrap();
            assert!(
                audible < 72,
                "{}: first audible sample {audible}",
                cue.name()
            );
        }
    }

    #[test]
    fn cues_are_loudness_matched_and_stop_is_quieter() {
        let start = active_rms_db(&samples(Cue::Start));
        let capture = active_rms_db(&samples(Cue::Capture));
        let stop = active_rms_db(&samples(Cue::Stop));
        assert!(
            (start - capture).abs() < 0.5,
            "start {start} dB, capture {capture} dB"
        );
        assert!(
            (start - stop - 3.0).abs() < 0.5,
            "start {start} dB, stop {stop} dB"
        );
    }

    #[test]
    fn volume_is_perceptual_and_clamped() {
        assert_eq!(gain_for(0.0), 0.0);
        assert_eq!(gain_for(1.0), 1.0);
        assert!((gain_for(0.5) - 0.25).abs() < 1e-6);
        assert_eq!(gain_for(7.0), 1.0);
        assert_eq!(gain_for(-1.0), 0.0);
    }

    #[test]
    fn config_volume_is_clamped_and_nan_gets_the_default() {
        assert_eq!(sanitize_volume(0.3), 0.3);
        assert_eq!(sanitize_volume(9.0), 1.0);
        assert_eq!(sanitize_volume(-2.0), 0.0);
        assert_eq!(sanitize_volume(f32::NAN), DEFAULT_SOUND_VOLUME);
    }

    #[test]
    fn resampling_keeps_pitch_length_and_level() {
        // 1 kHz sine at 48 kHz -> 44.1 kHz.
        let input: Vec<f32> = (0..4800)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48_000.0).sin() * 0.5)
            .collect();
        let out = resample(&input, 48_000, 44_100);
        assert_eq!(out.len(), 4410);
        // Compare against the ideal sine, away from the clip edges.
        let worst = (200..out.len() - 200)
            .map(|i| {
                let ideal = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 44_100.0).sin() * 0.5;
                (out[i] - ideal).abs()
            })
            .fold(0.0, f32::max);
        assert!(worst < 0.02, "resampled sine off by {worst}");
        assert_eq!(render_for(&input, 48_000, 48_000, 2).len(), input.len() * 2);
    }

    fn voice(cue: Cue, len: usize, gain: f32) -> Voice {
        Voice {
            cue,
            samples: Arc::from(vec![1.0f32; len]),
            pos: 0,
            gain,
            requested: Instant::now(),
            reported: false,
        }
    }

    #[test]
    fn mixer_sums_voices_applies_gain_reports_once_and_retires() {
        let mut voices = vec![voice(Cue::Start, 6, 0.5), voice(Cue::Capture, 3, 0.25)];
        let mut reported = Vec::new();
        let mut out = [0.0f32; 4];
        mix_voices(&mut voices, &mut out, |v| reported.push(v.cue));
        assert_eq!(out, [0.75, 0.75, 0.75, 0.5]);
        assert_eq!(reported, [Cue::Start, Cue::Capture]);
        assert_eq!(voices.len(), 1, "the 3-sample voice is finished");
        let mut out = [0.0f32; 4];
        mix_voices(&mut voices, &mut out, |_| panic!("already reported"));
        assert_eq!(out, [0.5, 0.5, 0.0, 0.0]);
        assert!(voices.is_empty());
    }

    #[test]
    fn cue_names_round_trip() {
        for cue in Cue::ALL {
            assert_eq!(parse_cue(cue.name()), Some(cue));
        }
        assert_eq!(parse_cue("shutter"), None);
    }
}
