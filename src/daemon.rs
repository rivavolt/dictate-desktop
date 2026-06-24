use anyhow::Result;
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::assemblyai;
use crate::audio;
use crate::config::{self, Config, State};
use crate::correct;
use crate::overlay::DoneKind;
use crate::deepgram;
use crate::fireworks;
use crate::fnkey;
use crate::groq;
use crate::ipc;
use crate::output;
use crate::overlay;
use crate::sound;
use crate::transcript::TranscriptEvent;
use crate::tray;

/// Monotonic id per recording, so each batch capture gets its own temp file.
static REC_SEQ: AtomicU64 = AtomicU64::new(0);
/// Below this RMS (mean energy) a capture is treated as silence and skipped. RMS — not peak —
/// because peak-max grows with clip length, which systematically dropped short utterances; RMS is
/// length-independent. Lenient: push-to-talk is deliberate, so err toward keeping the capture.
const SILENCE_RMS: f32 = 12.0;

#[derive(serde::Deserialize)]
struct HistRow {
    #[serde(default)]
    mode: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    latency_ms: u64,
}

/// Estimate batch transcription latency (seconds) for `provider` at `duration_ms` from recent
/// history — an affine fit (fixed overhead + per-second rate) over the last samples for this
/// provider, falling back to the mean, or None when there isn't enough data yet.
fn estimate_eta(history_jsonl: &std::path::Path, provider: &str, duration_ms: u64) -> Option<f32> {
    let content = std::fs::read_to_string(history_jsonl).ok()?;
    let (mut xs, mut ys): (Vec<f64>, Vec<f64>) = (Vec::new(), Vec::new());
    for line in content.lines().rev() {
        let Ok(row) = serde_json::from_str::<HistRow>(line) else {
            continue;
        };
        if row.mode != "batch" || row.latency_ms == 0 {
            continue;
        }
        if config::parse_provider_model(&row.model).0 != provider {
            continue;
        }
        xs.push(row.duration_ms as f64);
        ys.push(row.latency_ms as f64);
        if xs.len() >= 20 {
            break;
        }
    }
    if xs.len() < 2 {
        return None;
    }
    let n = xs.len() as f64;
    let sx: f64 = xs.iter().sum();
    let sy: f64 = ys.iter().sum();
    let sxx: f64 = xs.iter().map(|x| x * x).sum();
    let sxy: f64 = xs.iter().zip(&ys).map(|(x, y)| x * y).sum();
    let denom = n * sxx - sx * sx;
    let pred_ms = if denom.abs() < 1.0 {
        sy / n // all samples ~same duration → mean latency
    } else {
        let b = (n * sxy - sx * sy) / denom;
        let a = (sy - b * sx) / n;
        a + b * duration_ms as f64
    };
    Some((pred_ms / 1000.0).clamp(0.4, 60.0) as f32)
}

pub(crate) async fn transcribe_with_retry(
    path: &std::path::Path,
    provider: &str,
    lang: &str,
    languages: &[String],
    model: &str,
    vocabulary: &[String],
    remove_fillers: bool,
) -> anyhow::Result<String> {
    let mut last_err = None;
    for attempt in 0..3 {
        if attempt > 0 {
            let delay = std::time::Duration::from_millis(500 * (1 << attempt));
            tracing::info!("retrying transcription (attempt {}/3, backoff {}ms)", attempt + 1, delay.as_millis());
            tokio::time::sleep(delay).await;
        }
        match match provider {
            "assemblyai" => assemblyai::transcribe_file(path, lang, languages, model, vocabulary, remove_fillers).await,
            "groq" => groq::transcribe_file(path, lang, model, vocabulary).await,
            "fireworks" => fireworks::transcribe_file(path, lang, model, vocabulary).await,
            _ => deepgram::transcribe_file(path, lang, model, vocabulary, remove_fillers).await,
        } {
            Ok(t) => return Ok(t),
            Err(e) => {
                tracing::warn!("transcription attempt {} failed: {e}", attempt + 1);
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap())
}

/// Common finalization for all modes: correct → clipboard → file → history → sound → overlay
async fn finalize_transcript(
    transcript: String,
    do_correct: bool,
    correct_hold_ms: u64,
    lang: &str,
    mode: &str,
    model: &str,
    latency_ms: u64,
    enter_after: bool,
    is_clipboard: bool,
    already_typed: bool,
    auto_paste: bool,
    overlay: &overlay::Handle,
    seq: u64,
    transcript_file: &std::path::Path,
    history_file: &std::path::Path,
    audio_dir: &std::path::Path,
    audio_samples: Option<(&[i16], u32)>,
) {
    let raw = transcript.clone();
    // One instant for this utterance: the compact form names the FLAC, the RFC3339 form is the
    // JSONL ts — same moment, so the history row and its audio file are linked by construction.
    let now = chrono::Local::now();
    let stamp = now.format("%Y%m%d-%H%M%S%.3f").to_string();
    let iso = now.format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string();

    let mut audio_name: Option<String> = None;
    let mut duration_ms: u64 = 0;
    if let Some((samples, sample_rate)) = audio_samples {
        if !samples.is_empty() {
            duration_ms = samples.len() as u64 * 1000 / (sample_rate.max(1)) as u64;
            let _ = fs::create_dir_all(audio_dir);
            let name = format!("{stamp}.flac");
            if let Err(e) = audio::save_flac(&audio_dir.join(&name), samples, sample_rate) {
                tracing::warn!("failed to archive audio: {e}");
            } else {
                audio_name = Some(name);
            }
        }
    }

    let final_text = if do_correct && !transcript.is_empty() {
        overlay.set_text(transcript.clone());
        overlay.correcting();
        match correct::correct_text(&transcript, lang).await {
            Ok(corrected) => {
                tracing::info!("corrected: {transcript:?} -> {corrected:?}");
                corrected
            }
            Err(e) => {
                tracing::warn!("correction failed, using raw: {e}");
                transcript
            }
        }
    } else {
        transcript
    };
    // Whether the transcript actually reached the focused app (vs only the clipboard) — drives the
    // done circle's icon: checkmark when delivered, clipboard when only copied.
    let mut delivered = false;
    if !final_text.is_empty() {
        overlay.set_text(final_text.clone());
        // Trail the inserted text with a space so back-to-back dictations don't butt together; the
        // stored transcript + history stay clean (unspaced). Trailing (not leading) avoids a stray
        // space at line starts — which would break code indentation and skip bash history.
        let insert_text = format!("{final_text} ");
        output::copy_to_clipboard(&insert_text);
        if !is_clipboard && !already_typed {
            let n = insert_text.chars().count();
            if output::type_text(&insert_text) {
                delivered = true;
                tracing::info!("delivered via input-method ({n} chars)");
            } else {
                tracing::info!("input-method inactive → paste fallback (auto_paste={auto_paste}, {n} chars)");
                output::paste(auto_paste);
                delivered = auto_paste;
            }
        } else if already_typed {
            delivered = true;
        }
        let _ = std::fs::write(transcript_file, &final_text);
    }
    if enter_after && !is_clipboard && !final_text.is_empty() {
        output::type_enter();
    }
    output::append_history(history_file, &final_text);
    if !final_text.is_empty() || audio_name.is_some() {
        // Structured companion to history.log: links the utterance to its archived audio and
        // keeps raw-vs-corrected, model, duration, and processing latency (feeds the ETA).
        let jsonl = history_file.with_file_name("history.jsonl");
        output::append_history_record(
            &jsonl,
            &output::HistoryRecord {
                ts: &iso,
                audio: audio_name.as_deref(),
                mode,
                model,
                lang,
                raw: &raw,
                text: &final_text,
                corrected: final_text != raw,
                duration_ms,
                latency_ms,
            },
        );
    }
    sound::play_stop();
    // Hold corrected text visible — proportional to length, capped
    if do_correct && !final_text.is_empty() && correct_hold_ms > 0 {
        let words = final_text.split_whitespace().count() as u64;
        let hold = (800 + words * 100).min(correct_hold_ms);
        tokio::time::sleep(std::time::Duration::from_millis(hold)).await;
    }
    // Resolve this utterance's circle: checkmark if delivered, clipboard if only copied, or an
    // immediate fade (no icon) when there was no speech.
    let done_kind = if final_text.is_empty() {
        DoneKind::Dismissed
    } else if delivered {
        DoneKind::Delivered
    } else {
        DoneKind::Copied
    };
    overlay.done(seq, done_kind);
}

struct DaemonState {
    config: Config,
    state: State,
    recording: bool,
    stop_flag: Option<Arc<AtomicBool>>,
    _audio_stream: Option<cpal::Stream>,
    record_handle: Option<tokio::task::JoinHandle<()>>,
    tray_handle: Option<ksni::Handle<tray::DictateTray>>,
    overlay: overlay::Handle,
}

impl DaemonState {
    fn new(config: Config, state: State, tray_handle: Option<ksni::Handle<tray::DictateTray>>, overlay: overlay::Handle) -> Self {
        Self {
            config,
            state,
            recording: false,
            stop_flag: None,
            _audio_stream: None,
            record_handle: None,
            tray_handle,
            overlay,
        }
    }

    fn sync_tray_state(&self) {
        if let Some(tray) = self.tray_handle.clone() {
            let state = self.state.clone();
            let recording = self.recording;
            tokio::spawn(async move {
                tray.update(|t| {
                    t.set_recording(recording);
                    t.set_state(&state);
                })
                .await;
            });
        }
    }

    /// Languages the session must handle: the preferred set when auto-detecting, else the
    /// single chosen language.
    fn required_langs(&self) -> Vec<String> {
        if self.state.lang == config::AUTO_LANG {
            self.state.languages.clone()
        } else {
            vec![self.state.lang.clone()]
        }
    }

    /// A compatible model for the current mode + languages, if the picked one falls short.
    fn resolve(&self) -> Option<String> {
        let req = self.required_langs();
        let req: Vec<&str> = req.iter().map(String::as_str).collect();
        config::resolve_model(&self.state.model, &self.state.mode, &req)
    }

    fn start_recording(&mut self) -> Result<String> {
        if self.recording {
            return Ok("already recording".into());
        }

        self.recording = true;
        let seq = REC_SEQ.fetch_add(1, Ordering::Relaxed);
        fs::write(&self.config.state_file, "recording")?;
        sound::play_start();
        let overlay_mode = self.state.overlay_mode();
        if overlay_mode != config::OverlayMode::Off {
            self.overlay.set_status_only(overlay_mode == config::OverlayMode::Status);
            self.overlay.start(seq);
            self.overlay.set_info(self.state.mode.clone(), self.state.lang.clone());
        }

        let stop = Arc::new(AtomicBool::new(false));
        self.stop_flag = Some(stop.clone());

        // Smart fallback: if the picked model can't handle this mode/languages, use a
        // compatible one for THIS session only — the explicit pick is preserved.
        let picked = self.state.model.clone();
        if let Some(eff) = self.resolve() {
            tracing::info!(
                "{picked} can't do mode={} langs={:?}; using {eff} this session",
                self.state.mode,
                self.required_langs()
            );
            self.state.model = eff;
        }

        let (provider, _) = config::parse_provider_model(&self.state.model);
        let provider = provider.to_string();
        tracing::info!(
            "recording: mode={} provider={} model={} lang={}",
            self.state.mode, provider, self.state.model, self.state.lang
        );

        let result = match self.state.mode.as_str() {
            "live" => self.start_live(stop, &provider, seq),
            "batch" => self.start_batch(stop, &provider, seq),
            "vad" => self.start_vad(stop, &provider, seq),
            _ => self.start_live(stop, &provider, seq),
        };
        self.state.model = picked; // restore explicit pick — the fallback is per-session
        result?;

        Ok(format!("recording ({}, {})", self.state.mode, provider))
    }

    fn start_live(&mut self, stop: Arc<AtomicBool>, provider: &str, seq: u64) -> Result<()> {
        let audio_level = self.overlay.audio_level().clone();
        let (stream, audio_rx, sample_rate, samples_buf) = audio::capture_to_channel(stop.clone(), audio_level, &self.state.input)?;
        self._audio_stream = Some(stream);

        let (tx, mut rx) = mpsc::unbounded_channel::<TranscriptEvent>();

        let is_clipboard = self.state.output == "clipboard";
        let enter_after = self.state.enter;
        let do_correct = self.state.correct;
        let correct_hold_ms = self.state.correct_hold_ms;
        let lang = self.state.lang.clone();
        let transcript_file = self.config.transcript_file.clone();
        let history_file = self.config.history_file.clone();
        let audio_dir = self.config.audio_dir.clone();
        let overlay_handle = self.overlay.clone();
        let mode = self.state.mode.clone();
        let model = self.state.model.clone();
        let auto_paste = self.state.auto_paste;
        let state = self.state.clone();
        let provider = provider.to_string();
        self.record_handle = Some(tokio::spawn(async move {
            let event_handler = tokio::spawn(async move {
                let mut last_accumulated = String::new();
                let mut last_pending = String::new();
                // Whether the input method actually took our text (a field was focused). If it
                // never did, we don't type live — finalize delivers the whole transcript via
                // clipboard/paste instead (no per-char keystrokes, so nothing drops or trips a bind).
                let mut ime_used = false;
                while let Some(event) = rx.recv().await {
                    match event {
                        TranscriptEvent::Final { delta, accumulated } => {
                            tracing::info!("transcript: {delta}");
                            overlay_handle.set_text(accumulated.clone());
                            if !is_clipboard && output::type_text(&delta) {
                                ime_used = true;
                            }
                            last_accumulated = accumulated;
                            last_pending.clear();
                        }
                        TranscriptEvent::Interim(text) => {
                            overlay_handle.set_pending(text.clone());
                            last_pending = text;
                        }
                    }
                }
                // Flush any pending text that never got finalized
                if !last_pending.is_empty() {
                    if !last_accumulated.is_empty() && !last_accumulated.ends_with(' ') {
                        last_accumulated.push(' ');
                    }
                    last_accumulated.push_str(&last_pending);
                    tracing::info!("transcript (flushed pending): {last_pending}");
                    if !is_clipboard && output::type_text(&last_pending) {
                        ime_used = true;
                    }
                }
                let samples: Vec<i16> = samples_buf.lock().unwrap().clone();
                finalize_transcript(
                    last_accumulated, do_correct, correct_hold_ms, &lang,
                    &mode, &model, 0,
                    enter_after, is_clipboard, ime_used, auto_paste,
                    &overlay_handle, seq, &transcript_file, &history_file,
                    &audio_dir, Some((&samples, sample_rate)),
                ).await;
            });

            let result = match provider.as_str() {
                "assemblyai" => {
                    assemblyai::stream_live(&state, audio_rx, stop, sample_rate, tx).await
                }
                "fireworks" => {
                    fireworks::stream_live(&state, audio_rx, stop, sample_rate, tx).await
                }
                _ => {
                    deepgram::stream_live(&state, audio_rx, stop, sample_rate, tx).await
                }
            };
            if let Err(e) = result {
                tracing::error!("live streaming error: {e}");
            }
            let _ = event_handler.await;
        }));

        Ok(())
    }

    fn start_batch(&mut self, stop: Arc<AtomicBool>, provider: &str, seq: u64) -> Result<()> {
        // Per-utterance temp file (keyed by seq): a unique path lets a fast re-trigger record into
        // its own file while the previous transcription is still reading the old one.
        let audio_file = self.config.audio_file.with_extension(format!("{seq}.wav"));
        let state = self.state.clone();
        let enter_after = self.state.enter;
        let do_correct = self.state.correct;
        let correct_hold_ms = self.state.correct_hold_ms;
        let lang = self.state.lang.clone();
        let is_clipboard = self.state.output == "clipboard";
        let transcript_file = self.config.transcript_file.clone();
        let history_file = self.config.history_file.clone();
        let audio_dir = self.config.audio_dir.clone();
        let provider = provider.to_string();
        let overlay_handle = self.overlay.clone();
        let audio_level = self.overlay.audio_level().clone();

        let input_device = self.state.input.clone();
        self.record_handle = Some(tokio::spawn(async move {
            let audio_file2 = audio_file.clone();
            let stop2 = stop.clone();
            let record = tokio::task::spawn_blocking(move || {
                audio::record_to_file(&audio_file2, stop2, audio_level, &input_device)
            });

            while !stop.load(Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }

            overlay_handle.processing(seq, 0.0);

            if let Err(e) = record.await {
                tracing::error!("batch record error: {e}");
                return;
            }

            // Read samples and sample rate for archival before transcription
            let (samples_for_archive, archive_rate) = hound::WavReader::open(&audio_file)
                .map(|r| {
                    let rate = r.spec().sample_rate;
                    let samples: Vec<i16> = r.into_samples::<i16>().filter_map(|s| s.ok()).collect();
                    (samples, rate)
                })
                .unwrap_or_default();

            // Skip silent/empty captures instead of uploading silence (which the provider
            // rejects with "no spoken audio"): just dismiss the overlay. Peak amplitude is a
            // cheap speech proxy — real speech peaks in the thousands, room noise stays low.
            let peak = samples_for_archive.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
            let rms = if samples_for_archive.is_empty() {
                0.0
            } else {
                (samples_for_archive.iter().map(|&s| (s as f64).powi(2)).sum::<f64>()
                    / samples_for_archive.len() as f64)
                    .sqrt() as f32
            };
            let dur_ms = if archive_rate > 0 {
                samples_for_archive.len() as u64 * 1000 / archive_rate as u64
            } else {
                0
            };
            tracing::info!("batch capture: {dur_ms}ms, peak {peak}, rms {rms:.0}, provider {provider}");
            if samples_for_archive.is_empty() || rms < SILENCE_RMS {
                tracing::info!("no speech detected (rms {rms:.0} < {SILENCE_RMS}) — skipping");
                overlay_handle.done(seq, DoneKind::Dismissed);
                let _ = fs::remove_file(&audio_file);
                return;
            }

            // Countdown the estimated wait (rolling per-provider latency vs this clip's length).
            let jsonl = history_file.with_file_name("history.jsonl");
            let eta = estimate_eta(&jsonl, &provider, dur_ms).unwrap_or(0.0);
            overlay_handle.processing(seq, eta);

            let (_, model) = config::parse_provider_model(&state.model);
            let t0 = std::time::Instant::now();
            let transcript = match transcribe_with_retry(&audio_file, &provider, &state.lang, &state.languages, model, &state.vocabulary, state.remove_fillers).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!("batch transcribe error: {e}");
                    overlay_handle.done(seq, DoneKind::Failed);
                    let _ = fs::remove_file(&audio_file);
                    return;
                }
            };
            let latency_ms = t0.elapsed().as_millis() as u64;
            tracing::info!("transcribed in {latency_ms}ms ({} chars) via {}", transcript.len(), state.model);

            finalize_transcript(
                transcript, do_correct, correct_hold_ms, &lang,
                &state.mode, &state.model, latency_ms,
                enter_after, is_clipboard, false, state.auto_paste,
                &overlay_handle, seq, &transcript_file, &history_file,
                &audio_dir, Some((&samples_for_archive, archive_rate)),
            ).await;
            let _ = fs::remove_file(&audio_file);
        }));

        Ok(())
    }

    fn start_vad(&mut self, stop: Arc<AtomicBool>, provider: &str, seq: u64) -> Result<()> {
        let audio_level = self.overlay.audio_level().clone();
        let (stream, audio_rx, sample_rate, samples_buf) = audio::capture_to_channel(stop.clone(), audio_level, &self.state.input)?;
        self._audio_stream = Some(stream);

        let state = self.state.clone();
        let is_clipboard = self.state.output == "clipboard";
        let enter_after = self.state.enter;
        let do_correct = self.state.correct;
        let correct_hold_ms = self.state.correct_hold_ms;
        let lang = self.state.lang.clone();
        let transcript_file = self.config.transcript_file.clone();
        let history_file = self.config.history_file.clone();
        let audio_dir = self.config.audio_dir.clone();
        let chunk_file = self.config.audio_file.with_extension("chunk.wav");
        let provider = provider.to_string();
        let overlay_handle = self.overlay.clone();

        self.record_handle = Some(tokio::spawn(async move {
            let full_transcript = match crate::vad::stream_vad(
                audio_rx, stop, sample_rate,
                &provider, &state, overlay_handle.clone(), seq,
                transcript_file.clone(), chunk_file,
            ).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!("vad error: {e}");
                    String::new()
                }
            };
            let samples: Vec<i16> = samples_buf.lock().unwrap().clone();
            finalize_transcript(
                full_transcript, do_correct, correct_hold_ms, &lang,
                &state.mode, &state.model, 0,
                enter_after, is_clipboard, false, state.auto_paste,
                &overlay_handle, seq, &transcript_file, &history_file,
                &audio_dir, Some((&samples, sample_rate)),
            ).await;
        }));

        Ok(())
    }

    fn stop_recording(&mut self) -> Result<String> {
        if !self.recording {
            return Ok("not recording".into());
        }

        if let Some(stop) = self.stop_flag.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self._audio_stream = None;

        if let Some(handle) = self.record_handle.take() {
            tokio::spawn(async move {
                let _ = handle.await;
            });
        }

        self.recording = false;
        fs::write(&self.config.state_file, "idle")?;

        Ok("stopped".into())
    }

    fn toggle_recording(&mut self) -> Result<String> {
        if self.recording {
            self.stop_recording()
        } else {
            self.start_recording()
        }
    }

    fn handle_command(&mut self, req: ipc::Request) -> ipc::Response {
        match req.command.as_str() {
            "toggle" => match self.toggle_recording() {
                Ok(msg) => ipc::Response::ok(msg),
                Err(e) => ipc::Response::err(e.to_string()),
            },
            "start" => match self.start_recording() {
                Ok(msg) => ipc::Response::ok(msg),
                Err(e) => ipc::Response::err(e.to_string()),
            },
            "stop" => match self.stop_recording() {
                Ok(msg) => ipc::Response::ok(msg),
                Err(e) => ipc::Response::err(e.to_string()),
            },
            "status" => {
                let status = if self.recording { "recording" } else { "idle" };
                ipc::Response::ok(format!(
                    "{} (mode: {}, output: {}, overlay: {}, lang: {}, model: {}, preferred: [{}])",
                    status, self.state.mode, self.state.output,
                    self.state.overlay_mode().name(),
                    self.state.lang, self.state.model,
                    self.state.languages.join(", ")
                ))
            }
            "mode" => {
                if let Some(m) = req.arg {
                    if ["live", "vad", "batch"].contains(&m.as_str()) {
                        self.state.mode = m.clone();
                        // Re-derive the overlay default for the new mode (batch shows it,
                        // live/vad direct-typing hides it).
                        self.state.overlay = None;
                        self.state.save(&self.config);
                        let mut msg = format!("mode: {m}");
                        if let Some(eff) = self.resolve() {
                            msg.push_str(&format!(
                                " ({} can't do this mode/langs — will use {eff})",
                                self.state.model
                            ));
                        }
                        ipc::Response::ok(msg)
                    } else {
                        ipc::Response::err(format!("invalid mode '{}'. use: live, vad, batch", m))
                    }
                } else {
                    ipc::Response::ok(format!(
                        "mode: {} (available: live, vad, batch)",
                        self.state.mode
                    ))
                }
            }
            "lang" => {
                if let Some(l) = req.arg {
                    let lang = if l == "multi" { config::AUTO_LANG.to_string() } else { l };
                    self.state.lang = lang.clone();
                    self.state.save(&self.config);
                    let label = config::lang_name(&lang).unwrap_or(&lang);
                    let mut msg = format!("language: {label} ({lang})");
                    if let Some(eff) = self.resolve() {
                        msg.push_str(&format!(" ({} can't do this — will use {eff})", self.state.model));
                    }
                    ipc::Response::ok(msg)
                } else {
                    let current = match config::lang_name(&self.state.lang) {
                        Some(name) => format!("{name} ({})", self.state.lang),
                        None => self.state.lang.clone(),
                    };
                    let list = config::LANGUAGES.iter()
                        .map(|(code, name)| {
                            let marker = if *code == self.state.lang { " *" } else { "" };
                            format!("  {name} ({code}){marker}")
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    ipc::Response::ok(format!("language: {current}\navailable:\n{list}"))
                }
            }
            "languages" => {
                if let Some(list) = req.arg {
                    let langs: Vec<String> = list
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    let invalid: Vec<&str> = langs
                        .iter()
                        .filter(|c| c.as_str() == config::AUTO_LANG || config::lang_name(c).is_none())
                        .map(|s| s.as_str())
                        .collect();
                    if langs.is_empty() {
                        ipc::Response::err("provide at least one language code")
                    } else if !invalid.is_empty() {
                        ipc::Response::err(format!("invalid language code(s): {}", invalid.join(", ")))
                    } else {
                        self.state.languages = langs.clone();
                        self.state.save(&self.config);
                        ipc::Response::ok(format!("preferred languages: {}", langs.join(", ")))
                    }
                } else {
                    ipc::Response::ok(format!(
                        "preferred languages: {}\n(candidate set for auto-detect where supported — currently AssemblyAI batch)",
                        self.state.languages.join(", ")
                    ))
                }
            }
            "font" => {
                if let Some(f) = req.arg {
                    match std::process::Command::new("fc-match")
                        .args(["--format", "%{family}", &f])
                        .output()
                    {
                        Ok(out) => {
                            let matched = String::from_utf8_lossy(&out.stdout).trim().to_string();
                            if !matched.to_lowercase().contains(&f.to_lowercase()) {
                                return ipc::Response::err(format!(
                                    "font '{f}' not found (fc-match resolved to '{matched}')"
                                ));
                            }
                            self.state.font = f.clone();
                            self.state.save(&self.config);
                            self.overlay.set_font(f.clone());
                            ipc::Response::ok(format!("font: {f}"))
                        }
                        Err(_) => {
                            self.state.font = f.clone();
                            self.state.save(&self.config);
                            self.overlay.set_font(f.clone());
                            ipc::Response::ok(format!("font: {f} (could not verify)"))
                        }
                    }
                } else {
                    ipc::Response::ok(format!("font: {}", self.state.font))
                }
            }
            "enter" => {
                if let Some(v) = req.arg {
                    match v.as_str() {
                        "on" | "true" => {
                            self.state.enter = true;
                            self.state.save(&self.config);
                            ipc::Response::ok("enter: on")
                        }
                        "off" | "false" => {
                            self.state.enter = false;
                            self.state.save(&self.config);
                            ipc::Response::ok("enter: off")
                        }
                        _ => ipc::Response::err("invalid value. use: on, off"),
                    }
                } else {
                    self.state.enter = !self.state.enter;
                    self.state.save(&self.config);
                    ipc::Response::ok(format!("enter: {}", if self.state.enter { "on" } else { "off" }))
                }
            }
            "correct" => {
                if let Some(v) = req.arg {
                    match v.as_str() {
                        "on" | "true" => {
                            self.state.correct = true;
                            self.state.save(&self.config);
                            ipc::Response::ok("correct: on")
                        }
                        "off" | "false" => {
                            self.state.correct = false;
                            self.state.save(&self.config);
                            ipc::Response::ok("correct: off")
                        }
                        _ => ipc::Response::err("invalid value. use: on, off"),
                    }
                } else {
                    self.state.correct = !self.state.correct;
                    self.state.save(&self.config);
                    ipc::Response::ok(format!("correct: {}", if self.state.correct { "on" } else { "off" }))
                }
            }
            "correct-hold" => {
                if let Some(v) = req.arg {
                    match v.parse::<u64>() {
                        Ok(ms) => {
                            self.state.correct_hold_ms = ms;
                            self.state.save(&self.config);
                            ipc::Response::ok(format!("correct-hold: {ms}ms"))
                        }
                        Err(_) => ipc::Response::err("invalid value, expected milliseconds"),
                    }
                } else {
                    ipc::Response::ok(format!("correct-hold: {}ms", self.state.correct_hold_ms))
                }
            }
            "output" => {
                if let Some(o) = req.arg {
                    if ["type", "clipboard"].contains(&o.as_str()) {
                        self.state.output = o.clone();
                        // Drop any explicit override so the overlay follows the new mode's
                        // default (off for type, on for clipboard).
                        self.state.overlay = None;
                        self.state.save(&self.config);
                        ipc::Response::ok(format!(
                            "output: {o} (overlay: {})",
                            self.state.overlay_mode().name()
                        ))
                    } else {
                        ipc::Response::err(format!("invalid output '{}'. use: type, clipboard", o))
                    }
                } else {
                    ipc::Response::ok(format!(
                        "output: {} (available: type, clipboard)",
                        self.state.output
                    ))
                }
            }
            "overlay" => match req.arg.as_deref() {
                None => ipc::Response::ok(format!(
                    "overlay: {} (off | status | full)",
                    self.state.overlay_mode().name()
                )),
                Some(s) => {
                    let mode = match s {
                        "off" => Some(config::OverlayMode::Off),
                        "status" => Some(config::OverlayMode::Status),
                        "full" => Some(config::OverlayMode::Full),
                        _ => None,
                    };
                    match mode {
                        Some(m) => {
                            self.state.overlay = Some(m);
                            self.state.save(&self.config);
                            ipc::Response::ok(format!("overlay: {}", m.name()))
                        }
                        None => ipc::Response::err("invalid value. use: off, status, full"),
                    }
                }
            },
            "input" => {
                if let Some(name) = req.arg {
                    if name == "default" {
                        self.state.input.clear();
                        self.state.save(&self.config);
                        ipc::Response::ok("input: default")
                    } else {
                        let devices = audio::list_input_devices();
                        if devices.iter().any(|d| d == &name) {
                            self.state.input = name.clone();
                            self.state.save(&self.config);
                            ipc::Response::ok(format!("input: {name}"))
                        } else {
                            ipc::Response::err(format!("device '{name}' not found"))
                        }
                    }
                } else {
                    let current = if self.state.input.is_empty() {
                        format!("default ({})", audio::default_input_name())
                    } else {
                        self.state.input.clone()
                    };
                    let devices = audio::list_input_devices();
                    let default_name = audio::default_input_name();
                    let list = devices.iter().map(|d| {
                        let is_current = if self.state.input.is_empty() {
                            d == &default_name
                        } else {
                            d == &self.state.input
                        };
                        let marker = if is_current { " *" } else { "" };
                        format!("  {d}{marker}")
                    }).collect::<Vec<_>>().join("\n");
                    ipc::Response::ok(format!("input: {current}\navailable:\n{list}"))
                }
            }
            "model" => {
                if let Some(m) = req.arg {
                    if !m.contains('/') {
                        return ipc::Response::err(format!(
                            "format: provider/model (providers: {})",
                            config::PROVIDERS.join(", ")
                        ));
                    }
                    let (provider, model) = config::parse_provider_model(&m);
                    if !config::PROVIDERS.contains(&provider) {
                        return ipc::Response::err(format!(
                            "unknown provider '{}'. use: {}",
                            provider,
                            config::PROVIDERS.join(", ")
                        ));
                    }
                    if model.is_empty() {
                        return ipc::Response::err("model name required after provider/");
                    }
                    if !config::provider_models(provider).contains(&model) {
                        let available = config::provider_models(provider).join(", ");
                        return ipc::Response::err(format!(
                            "unknown model '{model}' for {provider}. available: {available}"
                        ));
                    }
                    let caps = config::model_caps(provider, model);
                    let mut warnings = Vec::new();
                    if self.state.mode == "live" && caps.live.is_none() {
                        warnings.push("no live support, will fall back on record".to_string());
                    }
                    if config::is_english_only(&caps)
                        && self.state.lang != "en"
                        && self.state.lang != config::AUTO_LANG
                    {
                        warnings.push("english only".into());
                    }
                    self.state.model = m.clone();
                    self.state.save(&self.config);
                    let mut msg = format!("model: {m}");
                    if !warnings.is_empty() {
                        msg.push_str(&format!(" ({})", warnings.join(", ")));
                    }
                    ipc::Response::ok(msg)
                } else {
                    let models = config::all_models();
                    let list = models.iter().map(|m| {
                        let (p, n) = config::parse_provider_model(m);
                        let caps = config::model_caps(p, n);
                        let mut tags = Vec::new();
                        if caps.live.is_some() { tags.push("live"); }
                        if caps.batch.is_some() { tags.push("batch"); }
                        if config::is_english_only(&caps) { tags.push("en-only"); }
                        let current = if *m == self.state.model { " *" } else { "" };
                        format!("  {m} [{tags}]{current}", tags = tags.join("+"))
                    }).collect::<Vec<_>>().join("\n");
                    ipc::Response::ok(format!(
                        "model: {}\navailable:\n{}",
                        self.state.model, list
                    ))
                }
            }
            "vocab" => {
                let arg = req.arg.unwrap_or_else(|| "list".into());
                let mut lines = arg.split('\n');
                let action = lines.next().unwrap_or("list");
                let terms: Vec<String> =
                    lines.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                match action {
                    "add" => {
                        for t in terms {
                            if !self.state.vocabulary.contains(&t) {
                                self.state.vocabulary.push(t);
                            }
                        }
                        self.state.save(&self.config);
                        ipc::Response::ok(format!("vocabulary: {} term(s)", self.state.vocabulary.len()))
                    }
                    "remove" => {
                        self.state.vocabulary.retain(|t| !terms.contains(t));
                        self.state.save(&self.config);
                        ipc::Response::ok(format!("vocabulary: {} term(s)", self.state.vocabulary.len()))
                    }
                    "clear" => {
                        self.state.vocabulary.clear();
                        self.state.save(&self.config);
                        ipc::Response::ok("vocabulary cleared")
                    }
                    _ => {
                        if self.state.vocabulary.is_empty() {
                            ipc::Response::ok("vocabulary: (empty)")
                        } else {
                            ipc::Response::ok(format!(
                                "vocabulary ({}):\n  {}",
                                self.state.vocabulary.len(),
                                self.state.vocabulary.join("\n  ")
                            ))
                        }
                    }
                }
            }
            "fillers" => {
                match req.arg.as_deref() {
                    Some("on") => self.state.remove_fillers = true,
                    Some("off") => self.state.remove_fillers = false,
                    None => self.state.remove_fillers = !self.state.remove_fillers,
                    Some(_) => return ipc::Response::err("use: on, off"),
                }
                self.state.save(&self.config);
                ipc::Response::ok(format!("filler removal: {}", if self.state.remove_fillers { "on" } else { "off" }))
            }
            "paste" => {
                match req.arg.as_deref() {
                    Some("on") => self.state.auto_paste = true,
                    Some("off") => self.state.auto_paste = false,
                    None => self.state.auto_paste = !self.state.auto_paste,
                    Some(_) => return ipc::Response::err("use: on, off"),
                }
                self.state.save(&self.config);
                ipc::Response::ok(format!("auto-paste: {}", if self.state.auto_paste { "on" } else { "off" }))
            }
            other => ipc::Response::err(format!("unknown command: {}", other)),
        }
    }
}

pub async fn run() -> Result<()> {
    let config = Config::new();
    let state = State::load(&config);

    // Start the input-method client up front (see output.rs).
    output::init();

    let (tray_tx, mut tray_rx) = mpsc::channel::<tray::TrayCommand>(16);
    let tray_handle = match tray::spawn(tray_tx, &state).await {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::warn!("tray unavailable: {e}");
            None
        }
    };

    let (fn_tx, mut fn_rx) = mpsc::channel::<fnkey::KeyEvent>(16);
    tokio::spawn(async move {
        if let Err(e) = fnkey::watch_fn_key(fn_tx).await {
            tracing::warn!("Fn key watcher failed: {e}");
        }
    });

    let overlay_handle = overlay::spawn(state.font.clone())?;
    let mut daemon = DaemonState::new(config, state, tray_handle, overlay_handle);

    tracing::info!(
        "starting daemon (mode: {}, lang: {}, model: {})",
        daemon.state.mode,
        daemon.state.lang,
        daemon.state.model
    );

    let _ = fs::write(&daemon.config.state_file, "idle");
    let _ = fs::remove_file(&daemon.config.socket_path);

    let listener = ipc::bind(&daemon.config.socket_path)?;
    tracing::info!("IPC socket: {}", daemon.config.socket_path.display());

    let (ipc_tx, mut ipc_rx) = mpsc::channel::<(ipc::Request, tokio::sync::oneshot::Sender<ipc::Response>)>(16);

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let ipc_tx = ipc_tx.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_ipc_connection(stream, ipc_tx).await {
                    tracing::warn!("IPC connection error: {e}");
                }
            });
        }
    });

    tracing::info!("daemon ready");
    daemon.sync_tray_state();

    loop {
        tokio::select! {
            Some((req, resp_tx)) = ipc_rx.recv() => {
                let resp = daemon.handle_command(req);
                let _ = resp_tx.send(resp);
                daemon.sync_tray_state();
            }
            Some(cmd) = tray_rx.recv() => {
                match cmd {
                    tray::TrayCommand::Toggle => {
                        let _ = daemon.toggle_recording();
                    }
                    tray::TrayCommand::SetMode(m) => {
                        daemon.handle_command(ipc::Request { command: "mode".into(), arg: Some(m) });
                    }
                    tray::TrayCommand::SetOutput(o) => {
                        daemon.handle_command(ipc::Request { command: "output".into(), arg: Some(o) });
                    }
                    tray::TrayCommand::ToggleLang(code) => {
                        if let Some(pos) = daemon.state.languages.iter().position(|c| c == &code) {
                            if daemon.state.languages.len() > 1 {
                                daemon.state.languages.remove(pos);
                            }
                        } else {
                            daemon.state.languages.push(code);
                        }
                        // Curating the set implies detect-among-it.
                        daemon.state.lang = config::AUTO_LANG.to_string();
                        daemon.state.save(&daemon.config);
                    }
                    tray::TrayCommand::SetModel(m) => {
                        daemon.handle_command(ipc::Request { command: "model".into(), arg: Some(m) });
                    }
                    tray::TrayCommand::SetInput(d) => {
                        daemon.handle_command(ipc::Request { command: "input".into(), arg: Some(d) });
                    }
                    tray::TrayCommand::ToggleEnter => {
                        daemon.handle_command(ipc::Request { command: "enter".into(), arg: None });
                    }
                    tray::TrayCommand::ToggleCorrect => {
                        daemon.handle_command(ipc::Request { command: "correct".into(), arg: None });
                    }
                    tray::TrayCommand::ToggleFillers => {
                        daemon.handle_command(ipc::Request { command: "fillers".into(), arg: None });
                    }
                    tray::TrayCommand::ToggleAutoPaste => {
                        daemon.handle_command(ipc::Request { command: "paste".into(), arg: None });
                    }
                    tray::TrayCommand::SetOverlay(m) => {
                        daemon.handle_command(ipc::Request { command: "overlay".into(), arg: Some(m) });
                    }
                    tray::TrayCommand::CopyHistory(text) => {
                        output::copy_to_clipboard(&text);
                        daemon.overlay.toast("copied from history".into());
                    }
                }
                daemon.sync_tray_state();
            }
            Some(ev) = fn_rx.recv() => {
                match ev {
                    fnkey::KeyEvent::Start => {
                        let _ = daemon.start_recording();
                    }
                    fnkey::KeyEvent::Release => {
                        if daemon.recording {
                            let _ = daemon.stop_recording();
                        }
                    }
                }
                daemon.sync_tray_state();
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                let _ = daemon.stop_recording();
                let _ = fs::remove_file(&daemon.config.socket_path);
                break;
            }
        }
    }

    Ok(())
}

async fn handle_ipc_connection(
    stream: tokio::net::UnixStream,
    ipc_tx: mpsc::Sender<(ipc::Request, tokio::sync::oneshot::Sender<ipc::Response>)>,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    if let Some(line) = lines.next_line().await? {
        let req: ipc::Request = serde_json::from_str(&line)?;
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        ipc_tx.send((req, resp_tx)).await?;
        if let Ok(resp) = resp_rx.await {
            let mut out = serde_json::to_vec(&resp)?;
            out.push(b'\n');
            writer.write_all(&out).await?;
        }
    }
    Ok(())
}
