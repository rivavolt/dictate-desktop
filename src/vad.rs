use anyhow::Result;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::audio;
use crate::config;
use crate::daemon;
use crate::output;
use crate::overlay;

const VAD_RATE: u32 = 16000;
const VAD_FRAME_SAMPLES: usize = 256; // earshot requires exactly 256 samples (16ms at 16kHz)
const VAD_FRAME_MS: u32 = 16;
const VOICE_THRESHOLD: f32 = 0.5;
const SILENCE_THRESHOLD: usize = 51; // ~816ms (51 * 16ms)
const PRE_SPEECH_FRAMES: usize = 10; // ~160ms
const DEBOUNCE_FRAMES: usize = 4; // ~64ms

fn encode_wav(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = std::io::Cursor::new(Vec::new());
    let mut writer = hound::WavWriter::new(&mut cursor, spec).unwrap();
    for &s in samples {
        writer.write_sample(s).unwrap();
    }
    writer.finalize().unwrap();
    cursor.into_inner()
}

pub async fn stream_vad(
    mut audio_rx: mpsc::Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
    sample_rate: u32,
    provider: &str,
    state: &config::State,
    overlay: overlay::Handle,
    seq: u64,
    transcript_file: PathBuf,
    chunk_file: PathBuf,
) -> Result<String> {
    let mut detector = earshot::Detector::default();

    // Native-rate frame size matching VAD frame duration
    let native_frame_samples = (sample_rate * VAD_FRAME_MS / 1000) as usize;
    let min_speech_samples = (sample_rate as usize) * 300 / 1000; // 300ms at native rate

    let mut sample_buf: Vec<i16> = Vec::new();
    let mut speech_active = false;
    let mut silence_count: usize = 0;
    let mut voice_count: usize = 0;
    let mut speech_samples: Vec<i16> = Vec::new();
    let mut pre_buffer: VecDeque<Vec<i16>> = VecDeque::new();
    let mut full_transcript = String::new();

    let _ = std::fs::write(&transcript_file, "");
    let (_, model) = config::parse_provider_model(&state.model);
    let model = model.to_string();

    while let Some(chunk) = audio_rx.recv().await {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // Decode bytes to native-rate i16 samples
        for c in chunk.chunks_exact(2) {
            sample_buf.push(i16::from_le_bytes([c[0], c[1]]));
        }

        while sample_buf.len() >= native_frame_samples {
            let native_frame: Vec<i16> = sample_buf.drain(..native_frame_samples).collect();

            // Resample to 16kHz for VAD detection
            let vad_samples = audio::resample(&native_frame, sample_rate, VAD_RATE);
            let is_voice = vad_samples.len() >= VAD_FRAME_SAMPLES
                && detector.predict_i16(&vad_samples[..VAD_FRAME_SAMPLES]) >= VOICE_THRESHOLD;

            if !speech_active {
                if is_voice {
                    voice_count += 1;
                    if voice_count >= DEBOUNCE_FRAMES {
                        speech_active = true;
                        silence_count = 0;
                        for pre_frame in pre_buffer.drain(..) {
                            speech_samples.extend_from_slice(&pre_frame);
                        }
                        speech_samples.extend_from_slice(&native_frame);
                    } else {
                        pre_buffer.push_back(native_frame);
                        if pre_buffer.len() > PRE_SPEECH_FRAMES {
                            pre_buffer.pop_front();
                        }
                    }
                } else {
                    voice_count = 0;
                    pre_buffer.push_back(native_frame);
                    if pre_buffer.len() > PRE_SPEECH_FRAMES {
                        pre_buffer.pop_front();
                    }
                }
            } else {
                speech_samples.extend_from_slice(&native_frame);
                if is_voice {
                    silence_count = 0;
                } else {
                    silence_count += 1;
                    if silence_count >= SILENCE_THRESHOLD {
                        if speech_samples.len() >= min_speech_samples {
                            transcribe_chunk(
                                &speech_samples,
                                sample_rate,
                                &chunk_file,
                                provider,
                                &state.lang,
                                &state.languages,
                                &model,
                                &state.vocabulary,
                                state.remove_fillers,
                                &overlay,
                                seq,
                                &state.output,
                                &transcript_file,
                                &mut full_transcript,
                            )
                            .await;
                        }
                        speech_samples.clear();
                        speech_active = false;
                        silence_count = 0;
                        voice_count = 0;
                        pre_buffer.clear();
                        detector.reset();
                    }
                }
            }
        }
    }

    // Flush remaining speech
    if speech_samples.len() >= min_speech_samples {
        transcribe_chunk(
            &speech_samples,
            sample_rate,
            &chunk_file,
            provider,
            &state.lang,
            &state.languages,
            &model,
            &state.vocabulary,
            state.remove_fillers,
            &overlay,
            seq,
            &state.output,
            &transcript_file,
            &mut full_transcript,
        )
        .await;
    }

    Ok(full_transcript)
}

async fn transcribe_chunk(
    samples: &[i16],
    sample_rate: u32,
    chunk_file: &PathBuf,
    provider: &str,
    lang: &str,
    languages: &[String],
    model: &str,
    vocabulary: &[String],
    remove_fillers: bool,
    overlay: &overlay::Handle,
    seq: u64,
    _output_mode: &str,
    transcript_file: &PathBuf,
    full_transcript: &mut String,
) {
    let wav = encode_wav(samples, sample_rate);
    if let Err(e) = tokio::fs::write(chunk_file, &wav).await {
        tracing::error!("failed to write chunk: {e}");
        return;
    }

    overlay.processing(seq, 0.0);

    match daemon::transcribe_with_retry(chunk_file, provider, lang, languages, model, vocabulary, remove_fillers).await {
        Ok(transcript) if !transcript.is_empty() => {
            if !full_transcript.is_empty() && !full_transcript.ends_with(' ') {
                full_transcript.push(' ');
            }
            full_transcript.push_str(&transcript);
            output::copy_to_clipboard(full_transcript);
            // Delivery happens once in finalize (input-method commit or clipboard paste), so it
            // works in apps without Wayland text-input — no per-chunk char typing here.
            let _ = tokio::fs::write(transcript_file, full_transcript.as_str()).await;
            overlay.set_text(full_transcript.clone());
        }
        Err(e) => tracing::error!("vad transcribe error: {e}"),
        _ => {}
    }

    let _ = tokio::fs::remove_file(chunk_file).await;
}
