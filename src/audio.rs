use anyhow::{anyhow, Context};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::Sender;

pub struct CaptureInfo {
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Starts capturing from the default input device. Mono-downmixed f32 frames
/// at the native device rate are pushed into `tx`.
///
/// The returned `cpal::Stream` is !Send: the caller must keep it alive on the
/// thread that called this function until capture should stop (grill item #5).
pub fn start_capture(tx: Sender<Vec<f32>>) -> anyhow::Result<(cpal::Stream, CaptureInfo)> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("no default input device"))?;
    let device_name = device.name().unwrap_or_else(|_| "<unknown>".into());
    let config = device
        .default_input_config()
        .context("no default input config")?;

    let info = CaptureInfo {
        device_name,
        sample_rate: config.sample_rate().0,
        channels: config.channels(),
    };
    let channels = info.channels as usize;
    let err_fn = |e| eprintln!("[audio] stream error: {e}");
    let stream_config: cpal::StreamConfig = config.clone().into();

    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &stream_config,
            move |data: &[f32], _| send_mono(&tx, data.iter().copied(), channels),
            err_fn,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_input_stream(
            &stream_config,
            move |data: &[i16], _| {
                send_mono(&tx, data.iter().map(|&s| s as f32 / 32768.0), channels)
            },
            err_fn,
            None,
        )?,
        cpal::SampleFormat::U16 => device.build_input_stream(
            &stream_config,
            move |data: &[u16], _| {
                send_mono(&tx, data.iter().map(|&s| (s as f32 - 32768.0) / 32768.0), channels)
            },
            err_fn,
            None,
        )?,
        other => return Err(anyhow!("unsupported sample format: {other:?}")),
    };
    stream.play().context("failed to start input stream")?;
    Ok((stream, info))
}

/// Downmix interleaved samples to mono (average across channels) and send.
fn send_mono(tx: &Sender<Vec<f32>>, samples: impl Iterator<Item = f32>, channels: usize) {
    let samples: Vec<f32> = samples.collect();
    let mono: Vec<f32> = if channels <= 1 {
        samples
    } else {
        samples
            .chunks_exact(channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
            .collect()
    };
    // Receiver gone means we're shutting down; drop silently.
    let _ = tx.send(mono);
}
