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
use crate::overlay::{self, DoneKind};

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

/// Estimate how long this utterance's transcription will take, for the per-utterance countdown.
/// Reuses the daemon's batch-history regression when the provider has history; otherwise a light
/// duration-based guess so the countdown still shows (VAD chunks are short, the provider fast).
fn chunk_eta(history_file: &std::path::Path, provider: &str, n_samples: usize, sample_rate: u32) -> f32 {
    let dur_ms = n_samples as u64 * 1000 / (sample_rate.max(1) as u64);
    daemon::estimate_eta(history_file, provider, dur_ms)
        .unwrap_or_else(|| (0.5 + dur_ms as f32 / 1000.0 * 0.1).clamp(0.4, 10.0))
}

/// Continuous voice-activity dictation: detect each utterance (speech bounded by silence) and, the
/// moment a pause ends it, transcribe + deliver it live — one overlay bubble per utterance, exactly
/// like batch mode but back-to-back. Detection never blocks on transcription: finished utterances
/// are handed to a serialized worker that transcribes and types them strictly in order, so the loop
/// keeps listening (and spawning the next bubble) while the previous utterance is still in flight.
pub async fn stream_vad(
    mut audio_rx: mpsc::Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
    sample_rate: u32,
    provider: &str,
    state: &config::State,
    overlay: overlay::Handle,
    _seq: u64,
    transcript_file: PathBuf,
    chunk_file: PathBuf,
    history_file: PathBuf,
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
    let mut current_seq: Option<u64> = None;

    let _ = std::fs::write(&transcript_file, "");

    // Worker: transcribe + deliver utterances strictly in order. Detection sends finished
    // utterances here and keeps going, so the network round-trip never blocks the next bubble,
    // while ordered delivery means typed text can't get scrambled.
    let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel::<(u64, Vec<i16>)>();
    let w_overlay = overlay.clone();
    let w_provider = provider.to_string();
    let w_lang = state.lang.clone();
    let w_languages = state.languages.clone();
    let (_, w_model) = config::parse_provider_model(&state.model);
    let w_model = w_model.to_string();
    let w_vocab = state.vocabulary.clone();
    let w_remove = state.remove_fillers;
    let w_output = state.output.clone();
    let w_auto_paste = state.auto_paste;
    let w_chunk_file = chunk_file.clone();
    let w_transcript_file = transcript_file.clone();
    let worker = tokio::spawn(async move {
        let mut full = String::new();
        while let Some((useq, samples)) = chunk_rx.recv().await {
            transcribe_and_deliver(
                &samples,
                sample_rate,
                &w_chunk_file,
                &w_provider,
                &w_lang,
                &w_languages,
                &w_model,
                &w_vocab,
                w_remove,
                &w_overlay,
                useq,
                &w_output,
                w_auto_paste,
                &w_transcript_file,
                &mut full,
            )
            .await;
        }
        full
    });

    tracing::info!("vad: stream started, listening");
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
                        // New utterance → its own Recording bubble, like a fresh batch capture.
                        let useq = daemon::REC_SEQ.fetch_add(1, Ordering::Relaxed);
                        overlay.start(useq);
                        tracing::info!("vad: utterance start (seq {useq})");
                        current_seq = Some(useq);
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
                        if let Some(useq) = current_seq.take() {
                            if speech_samples.len() >= min_speech_samples {
                                // Utterance ended: flip its bubble to processing and hand it to the
                                // worker, then keep listening immediately (don't block).
                                tracing::info!("vad: utterance end ({} samples), transcribing", speech_samples.len());
                                let eta = chunk_eta(&history_file, provider, speech_samples.len(), sample_rate);
                                overlay.processing(useq, eta);
                                let _ = chunk_tx.send((useq, std::mem::take(&mut speech_samples)));
                            } else {
                                overlay.done(useq, DoneKind::Dismissed);
                            }
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

    // Flush the in-progress utterance at stop through the same live path.
    if let Some(useq) = current_seq.take() {
        if speech_samples.len() >= min_speech_samples {
            let eta = chunk_eta(&history_file, provider, speech_samples.len(), sample_rate);
            overlay.processing(useq, eta);
            let _ = chunk_tx.send((useq, std::mem::take(&mut speech_samples)));
        } else {
            overlay.done(useq, DoneKind::Dismissed);
        }
    }

    drop(chunk_tx);
    Ok(worker.await.unwrap_or_default())
}

/// Transcribe one utterance and deliver it immediately (live), driving its overlay bubble to a Done
/// state. Each chunk is a whole utterance (the VAD cuts at silences, never mid-word), so per-chunk
/// transcription preserves accuracy. NOTE: AssemblyAI's batch API rejects short/no-speech chunks
/// ("language_detection cannot be performed on files with no spoken audio"), so this live mode is
/// meant for a short-chunk-tolerant, fast provider like groq/whisper-large-v3-turbo — but the
/// configured model/provider is used as-is, never hardcoded.
#[allow(clippy::too_many_arguments)]
async fn transcribe_and_deliver(
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
    output_mode: &str,
    auto_paste: bool,
    transcript_file: &PathBuf,
    full_transcript: &mut String,
) {
    let wav = encode_wav(samples, sample_rate);
    if let Err(e) = tokio::fs::write(chunk_file, &wav).await {
        tracing::error!("failed to write chunk: {e}");
        overlay.done(seq, DoneKind::Failed);
        return;
    }

    match daemon::transcribe_with_retry(chunk_file, provider, lang, languages, model, vocabulary, remove_fillers).await {
        Ok(transcript) if !transcript.trim().is_empty() => {
            let text = transcript.trim();
            if !full_transcript.is_empty() && !full_transcript.ends_with(' ') {
                full_transcript.push(' ');
            }
            full_transcript.push_str(text);
            let _ = tokio::fs::write(transcript_file, full_transcript.as_str()).await;

            // Deliver this utterance now; trailing space so back-to-back utterances don't merge.
            let insert = format!("{text} ");
            output::copy_to_clipboard(&insert);
            let kind = if output_mode == "clipboard" {
                DoneKind::Copied
            } else if output::type_text(&insert) {
                DoneKind::Delivered
            } else {
                output::paste(auto_paste);
                if auto_paste {
                    DoneKind::Delivered
                } else {
                    DoneKind::Copied
                }
            };
            overlay.done(seq, kind);
        }
        Ok(_) => overlay.done(seq, DoneKind::Dismissed),
        Err(e) => {
            tracing::error!("vad transcribe error: {e}");
            overlay.done(seq, DoneKind::Failed);
        }
    }

    let _ = tokio::fs::remove_file(chunk_file).await;
}
