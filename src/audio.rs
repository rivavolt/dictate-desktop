use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleRate;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

const PREFERRED_RATE: u32 = 48000;
const CHANNELS: u16 = 1;

pub struct AudioConfig {
    pub device: cpal::Device,
    pub stream_config: cpal::StreamConfig,
    pub sample_rate: u32,
}

pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    host.input_devices()
        .map(|devs| devs.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

pub fn default_input_name() -> String {
    let host = cpal::default_host();
    host.default_input_device()
        .and_then(|d| d.name().ok())
        .unwrap_or_default()
}

pub fn get_audio_config(device_name: &str) -> Result<AudioConfig> {
    let host = cpal::default_host();
    let device = if device_name.is_empty() {
        host.default_input_device().context("no input device")?
    } else {
        host.input_devices()?
            .find(|d| d.name().map(|n| n == device_name).unwrap_or(false))
            .with_context(|| format!("input device '{}' not found", device_name))?
    };
    let config_range = device
        .supported_input_configs()?
        .filter(|c| c.channels() <= CHANNELS && c.sample_format() == cpal::SampleFormat::F32)
        .max_by_key(|c| {
            let (min, max) = (c.min_sample_rate().0, c.max_sample_rate().0);
            if PREFERRED_RATE >= min && PREFERRED_RATE <= max {
                (1, PREFERRED_RATE)
            } else {
                (0, max.min(PREFERRED_RATE))
            }
        })
        .context("no suitable audio config")?;

    let rate =
        if PREFERRED_RATE >= config_range.min_sample_rate().0 && PREFERRED_RATE <= config_range.max_sample_rate().0 {
            PREFERRED_RATE
        } else {
            config_range
                .max_sample_rate()
                .0
                .min(PREFERRED_RATE)
                .max(config_range.min_sample_rate().0)
        };

    let stream_config = config_range.with_sample_rate(SampleRate(rate)).config();
    tracing::info!("audio capture at {rate}Hz");
    Ok(AudioConfig {
        device,
        stream_config,
        sample_rate: rate,
    })
}

pub type SamplesBuf = Arc<std::sync::Mutex<Vec<i16>>>;

/// Capture audio into a tokio mpsc channel. Returns the cpal Stream (must be kept alive),
/// a receiver for PCM chunks, the sample rate, and a shared buffer of all samples for archival.
pub fn capture_to_channel(
    stop: Arc<AtomicBool>,
    audio_level: Arc<AtomicU32>,
    device_name: &str,
) -> Result<(cpal::Stream, mpsc::Receiver<Vec<u8>>, u32, SamplesBuf)> {
    let cfg = get_audio_config(device_name)?;
    let (tx, rx) = mpsc::channel::<Vec<u8>>(256);
    let samples_buf: SamplesBuf = Arc::new(std::sync::Mutex::new(Vec::new()));
    let samples_cb = samples_buf.clone();

    let stream = cfg.device.build_input_stream(
        &cfg.stream_config,
        move |data: &[f32], _| {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            if !data.is_empty() {
                let rms = (data.iter().map(|&s| s * s).sum::<f32>() / data.len() as f32).sqrt();
                audio_level.store(rms.to_bits(), Ordering::Relaxed);
            }
            let pcm: Vec<u8> = data
                .iter()
                .flat_map(|&s| {
                    let sample = (s * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    sample.to_le_bytes()
                })
                .collect();
            // Collect samples for archival
            let samples: Vec<i16> = data.iter().map(|&s| (s * 32767.0).clamp(-32768.0, 32767.0) as i16).collect();
            if let Ok(mut buf) = samples_cb.lock() {
                buf.extend_from_slice(&samples);
            }
            let _ = tx.try_send(pcm);
        },
        |e| tracing::error!("audio capture error: {e}"),
        None,
    )?;

    stream.play()?;
    Ok((stream, rx, cfg.sample_rate, samples_buf))
}

/// Write collected samples to a FLAC file (lossless, ~3x smaller than WAV).
pub fn save_flac(path: &std::path::Path, samples: &[i16], sample_rate: u32) -> Result<()> {
    use std::io::Write;
    // Write raw WAV to buffer, pipe through flac encoder
    let mut wav_buf = std::io::Cursor::new(Vec::new());
    {
        let spec = hound::WavSpec {
            channels: CHANNELS,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::new(&mut wav_buf, spec)?;
        for &s in samples {
            writer.write_sample(s)?;
        }
        writer.finalize()?;
    }
    let mut child = std::process::Command::new("flac")
        .args(["--silent", "--best", "-o"])
        .arg(path)
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("flac encoder not found")?;
    child.stdin.take().unwrap().write_all(wav_buf.get_ref())?;
    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("flac encoder failed");
    }
    Ok(())
}

/// Resample i16 audio using linear interpolation.
pub fn resample(samples: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
    if from_rate == to_rate || samples.is_empty() {
        return samples.to_vec();
    }
    let ratio = from_rate as f64 / to_rate as f64;
    let out_len = (samples.len() as f64 / ratio) as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 * ratio;
        let idx = src as usize;
        let frac = src - idx as f64;
        let s = if idx + 1 < samples.len() {
            samples[idx] as f64 * (1.0 - frac) + samples[idx + 1] as f64 * frac
        } else {
            samples[idx.min(samples.len() - 1)] as f64
        };
        out.push(s.clamp(-32768.0, 32767.0) as i16);
    }
    out
}

/// Detect audio MIME type from file extension.
pub fn audio_mime(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("flac") => "audio/flac",
        Some("ogg") | Some("oga") => "audio/ogg",
        Some("mp3") => "audio/mpeg",
        _ => "audio/wav",
    }
}

/// Record audio to a WAV file until stop is signaled.
pub fn record_to_file(path: &std::path::Path, stop: Arc<AtomicBool>, audio_level: Arc<AtomicU32>, device_name: &str) -> Result<()> {
    let cfg = get_audio_config(device_name)?;
    let spec = hound::WavSpec {
        channels: CHANNELS,
        sample_rate: cfg.sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer = hound::WavWriter::create(path, spec)?;
    let samples = Arc::new(std::sync::Mutex::new(Vec::<i16>::new()));
    let samples_cb = samples.clone();
    let stop_cb = stop.clone();

    let stream = cfg.device.build_input_stream(
        &cfg.stream_config,
        move |data: &[f32], _| {
            if stop_cb.load(Ordering::Relaxed) {
                return;
            }
            if !data.is_empty() {
                let rms = (data.iter().map(|&s| s * s).sum::<f32>() / data.len() as f32).sqrt();
                audio_level.store(rms.to_bits(), Ordering::Relaxed);
            }
            let mut buf = samples_cb.lock().unwrap();
            for &s in data {
                buf.push((s * 32767.0).clamp(-32768.0, 32767.0) as i16);
            }
        },
        |e| tracing::error!("audio capture error: {e}"),
        None,
    )?;

    stream.play()?;

    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let mut buf = samples.lock().unwrap();
        for &s in buf.iter() {
            writer.write_sample(s)?;
        }
        buf.clear();
    }

    let buf = samples.lock().unwrap();
    for &s in buf.iter() {
        writer.write_sample(s)?;
    }
    writer.finalize()?;
    Ok(())
}

/// Write i16 mono samples to a WAV file — used to hand a gain-normalized copy of a capture to the
/// transcription provider while the archived recording keeps the raw samples.
pub fn write_wav(path: &std::path::Path, samples: &[i16], sample_rate: u32) -> Result<()> {
    let spec = hound::WavSpec {
        channels: CHANNELS,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for &s in samples {
        writer.write_sample(s)?;
    }
    writer.finalize()?;
    Ok(())
}
