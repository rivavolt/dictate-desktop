use std::io::Write;
use std::process::{Command, Stdio};

fn tone_wav(freq: f32, duration_ms: u32, sample_rate: u32) -> Vec<u8> {
    let num_samples = sample_rate * duration_ms / 1000;
    let mut buf = Vec::with_capacity(44 + num_samples as usize * 2);

    let data_size = num_samples * 2;
    let file_size = 36 + data_size;

    // WAV header
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&file_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // mono
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    buf.extend_from_slice(&2u16.to_le_bytes()); // block align
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_size.to_le_bytes());

    for i in 0..num_samples {
        let t = i as f32 / sample_rate as f32;
        // Fade envelope to avoid clicks
        let fade_samples = sample_rate * 5 / 1000; // 5ms fade
        let envelope = if i < fade_samples {
            i as f32 / fade_samples as f32
        } else if i > num_samples - fade_samples {
            (num_samples - i) as f32 / fade_samples as f32
        } else {
            1.0
        };
        let sample = (t * freq * 2.0 * std::f32::consts::PI).sin() * 0.1 * envelope;
        let pcm = (sample * 32767.0) as i16;
        buf.extend_from_slice(&pcm.to_le_bytes());
    }

    buf
}

fn play_wav(data: &[u8]) {
    let Ok(mut child) = Command::new("pw-play")
        .arg("--rate=48000")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(data);
    }
}

pub fn play_start() {
    play_wav(&tone_wav(880.0, 100, 48000));
}

pub fn play_stop() {
    play_wav(&tone_wav(440.0, 100, 48000));
}
