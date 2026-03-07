use std::io::Write;
use std::process::{Command, Stdio};

fn write_wav_header(buf: &mut Vec<u8>, num_samples: u32, sample_rate: u32) {
    let data_size = num_samples * 2;
    let file_size = 36 + data_size;
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&file_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // mono
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes());
    buf.extend_from_slice(&16u16.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_size.to_le_bytes());
}

fn triangle(phase: f32) -> f32 {
    let p = phase.fract();
    if p < 0.25 { p * 4.0 }
    else if p < 0.75 { 2.0 - p * 4.0 }
    else { p * 4.0 - 4.0 }
}

fn chord_wav(freqs: &[(f32, f32)], duration_ms: u32, sample_rate: u32) -> Vec<u8> {
    let num_samples = sample_rate * duration_ms / 1000;
    let mut buf = Vec::with_capacity(44 + num_samples as usize * 2);
    write_wav_header(&mut buf, num_samples, sample_rate);

    let fade_in = sample_rate * 8 / 1000;
    let fade_out = sample_rate * duration_ms / 2 / 1000; // long fade-out for smoothness

    for i in 0..num_samples {
        let t = i as f32 / sample_rate as f32;
        let envelope = if i < fade_in {
            i as f32 / fade_in as f32
        } else if i > num_samples - fade_out {
            let remaining = (num_samples - i) as f32 / fade_out as f32;
            remaining * remaining // quadratic fade-out
        } else {
            1.0
        };
        let sample: f32 = freqs.iter()
            .map(|&(freq, amp)| triangle(t * freq) * amp)
            .sum();
        let pcm = (sample * envelope * 32767.0) as i16;
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
    std::thread::spawn(move || { let _ = child.wait(); });
}

pub fn play_start() {
    // Ascending two-note chord: C5 + E5 (major third, bright)
    play_wav(&chord_wav(&[(523.25, 0.07), (659.25, 0.05)], 160, 48000));
}

pub fn play_stop() {
    // Descending: E5 + C5 (reversed emphasis, softer close)
    play_wav(&chord_wav(&[(659.25, 0.05), (523.25, 0.07)], 180, 48000));
}
