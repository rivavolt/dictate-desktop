use anyhow::Result;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::audio;
use crate::config::{self, Config, State};
use crate::correct;
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

pub(crate) async fn transcribe_with_retry(
    path: &std::path::Path,
    provider: &str,
    lang: &str,
    model: &str,
) -> anyhow::Result<String> {
    let mut last_err = None;
    for attempt in 0..3 {
        if attempt > 0 {
            let delay = std::time::Duration::from_millis(500 * (1 << attempt));
            tracing::info!("retrying transcription (attempt {}/3, backoff {}ms)", attempt + 1, delay.as_millis());
            tokio::time::sleep(delay).await;
        }
        match match provider {
            "groq" => groq::transcribe_file(path, lang, model).await,
            "fireworks" => fireworks::transcribe_file(path, lang, model).await,
            _ => deepgram::transcribe_file(path, lang, model).await,
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
    enter_after: bool,
    is_clipboard: bool,
    already_typed: bool,
    overlay: &overlay::Handle,
    transcript_file: &std::path::Path,
    history_file: &std::path::Path,
    audio_dir: &std::path::Path,
    audio_samples: Option<(&[i16], u32)>,
) {
    // Archive audio with timestamp
    if let Some((samples, sample_rate)) = audio_samples {
        if !samples.is_empty() {
            let _ = fs::create_dir_all(audio_dir);
            let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let path = audio_dir.join(format!("{ts}.flac"));
            if let Err(e) = audio::save_flac(&path, samples, sample_rate) {
                tracing::warn!("failed to archive audio: {e}");
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
    if !final_text.is_empty() {
        overlay.set_text(final_text.clone());
        output::copy_to_clipboard(&final_text);
        if !already_typed && !is_clipboard {
            output::type_text(&final_text);
        }
        let _ = std::fs::write(transcript_file, &final_text);
    }
    if enter_after && !is_clipboard && !final_text.is_empty() {
        output::type_enter();
    }
    output::append_history(history_file, &final_text);
    sound::play_stop();
    // Hold corrected text visible — proportional to length, capped
    if do_correct && !final_text.is_empty() && correct_hold_ms > 0 {
        let words = final_text.split_whitespace().count() as u64;
        let hold = (800 + words * 100).min(correct_hold_ms);
        tokio::time::sleep(std::time::Duration::from_millis(hold)).await;
    }
    overlay.copied();
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

    fn start_recording(&mut self) -> Result<String> {
        if self.recording {
            return Ok("already recording".into());
        }

        self.recording = true;
        fs::write(&self.config.state_file, "recording")?;
        sound::play_start();
        self.overlay.show();
        self.overlay.set_info(self.state.mode.clone(), self.state.lang.clone());

        let stop = Arc::new(AtomicBool::new(false));
        self.stop_flag = Some(stop.clone());

        if let Some(new_model) = config::resolve_model(&self.state.model, &self.state.mode, &self.state.lang) {
            tracing::info!("{} incompatible with mode={} lang={}, switching to {new_model}",
                self.state.model, self.state.mode, self.state.lang);
            self.state.model = new_model;
            let _ = fs::write(&self.config.model_file, &self.state.model);
        }

        let (provider, _model) = config::parse_provider_model(&self.state.model);
        let provider = provider.to_string();

        match self.state.mode.as_str() {
            "live" => self.start_live(stop, &provider)?,
            "batch" => self.start_batch(stop, &provider)?,
            "vad" => self.start_vad(stop, &provider)?,
            _ => self.start_live(stop, &provider)?,
        }

        Ok(format!("recording ({}, {})", self.state.mode, provider))
    }

    fn start_live(&mut self, stop: Arc<AtomicBool>, provider: &str) -> Result<()> {
        let audio_level = self.overlay.audio_level().clone();
        let (stream, audio_rx, sample_rate, samples_buf) = audio::capture_to_channel(stop.clone(), audio_level)?;
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
        let state = self.state.clone();
        let provider = provider.to_string();
        self.record_handle = Some(tokio::spawn(async move {
            let event_handler = tokio::spawn(async move {
                let mut last_accumulated = String::new();
                let mut last_pending = String::new();
                while let Some(event) = rx.recv().await {
                    match event {
                        TranscriptEvent::Final { delta, accumulated } => {
                            tracing::info!("transcript: {delta}");
                            overlay_handle.set_text(accumulated.clone());
                            if !is_clipboard {
                                output::type_text(&delta);
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
                    if !is_clipboard {
                        output::type_text(&last_pending);
                    }
                }
                let samples: Vec<i16> = samples_buf.lock().unwrap().clone();
                finalize_transcript(
                    last_accumulated, do_correct, correct_hold_ms, &lang,
                    enter_after, is_clipboard, !is_clipboard,
                    &overlay_handle, &transcript_file, &history_file,
                    &audio_dir, Some((&samples, sample_rate)),
                ).await;
            });

            let result = match provider.as_str() {
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

    fn start_batch(&mut self, stop: Arc<AtomicBool>, provider: &str) -> Result<()> {
        let audio_file = self.config.audio_file.clone();
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

        self.record_handle = Some(tokio::spawn(async move {
            let audio_file2 = audio_file.clone();
            let stop2 = stop.clone();
            let record = tokio::task::spawn_blocking(move || {
                audio::record_to_file(&audio_file2, stop2, audio_level)
            });

            while !stop.load(Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }

            overlay_handle.processing();

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

            let (_, model) = config::parse_provider_model(&state.model);
            let transcript = match transcribe_with_retry(&audio_file, &provider, &state.lang, model).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!("batch transcribe error: {e}");
                    String::new()
                }
            };

            finalize_transcript(
                transcript, do_correct, correct_hold_ms, &lang,
                enter_after, is_clipboard, false,
                &overlay_handle, &transcript_file, &history_file,
                &audio_dir, Some((&samples_for_archive, archive_rate)),
            ).await;
            let _ = fs::remove_file(&audio_file);
        }));

        Ok(())
    }

    fn start_vad(&mut self, stop: Arc<AtomicBool>, provider: &str) -> Result<()> {
        let audio_level = self.overlay.audio_level().clone();
        let (stream, audio_rx, sample_rate, samples_buf) = audio::capture_to_channel(stop.clone(), audio_level)?;
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
                &provider, &state, overlay_handle.clone(),
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
                enter_after, is_clipboard, !is_clipboard,
                &overlay_handle, &transcript_file, &history_file,
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
                    "{} (mode: {}, output: {}, lang: {}, model: {})",
                    status, self.state.mode, self.state.output, self.state.lang, self.state.model
                ))
            }
            "mode" => {
                if let Some(m) = req.arg {
                    if ["live", "vad", "batch"].contains(&m.as_str()) {
                        let _ = fs::write(&self.config.mode_file, &m);
                        self.state.mode = m.clone();
                        let mut msg = format!("mode: {m}");
                        if let Some(new_model) = config::resolve_model(&self.state.model, &self.state.mode, &self.state.lang) {
                            self.state.model = new_model.clone();
                            let _ = fs::write(&self.config.model_file, &self.state.model);
                            msg.push_str(&format!(" (switched model to {new_model})"));
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
                    let _ = fs::write(&self.config.lang_file, &lang);
                    self.state.lang = lang.clone();
                    let label = config::lang_name(&lang).unwrap_or(&lang);
                    let mut msg = format!("language: {label} ({lang})");
                    if let Some(new_model) = config::resolve_model(&self.state.model, &self.state.mode, &self.state.lang) {
                        self.state.model = new_model.clone();
                        let _ = fs::write(&self.config.model_file, &self.state.model);
                        msg.push_str(&format!(" (switched model to {new_model})"));
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
                            let _ = fs::write(&self.config.font_file, &f);
                            self.state.font = f.clone();
                            self.overlay.set_font(f.clone());
                            ipc::Response::ok(format!("font: {f}"))
                        }
                        Err(_) => {
                            let _ = fs::write(&self.config.font_file, &f);
                            self.state.font = f.clone();
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
                            let _ = fs::write(&self.config.enter_file, "true");
                            ipc::Response::ok("enter: on")
                        }
                        "off" | "false" => {
                            self.state.enter = false;
                            let _ = fs::write(&self.config.enter_file, "false");
                            ipc::Response::ok("enter: off")
                        }
                        _ => ipc::Response::err("invalid value. use: on, off"),
                    }
                } else {
                    // Toggle
                    self.state.enter = !self.state.enter;
                    let _ = fs::write(&self.config.enter_file, if self.state.enter { "true" } else { "false" });
                    ipc::Response::ok(format!("enter: {}", if self.state.enter { "on" } else { "off" }))
                }
            }
            "correct" => {
                if let Some(v) = req.arg {
                    match v.as_str() {
                        "on" | "true" => {
                            self.state.correct = true;
                            let _ = fs::write(&self.config.correct_file, "true");
                            ipc::Response::ok("correct: on")
                        }
                        "off" | "false" => {
                            self.state.correct = false;
                            let _ = fs::write(&self.config.correct_file, "false");
                            ipc::Response::ok("correct: off")
                        }
                        _ => ipc::Response::err("invalid value. use: on, off"),
                    }
                } else {
                    self.state.correct = !self.state.correct;
                    let _ = fs::write(&self.config.correct_file, if self.state.correct { "true" } else { "false" });
                    ipc::Response::ok(format!("correct: {}", if self.state.correct { "on" } else { "off" }))
                }
            }
            "correct-hold" => {
                if let Some(v) = req.arg {
                    match v.parse::<u64>() {
                        Ok(ms) => {
                            self.state.correct_hold_ms = ms;
                            let _ = fs::write(&self.config.correct_hold_file, &v);
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
                        let _ = fs::write(&self.config.output_file, &o);
                        self.state.output = o.clone();
                        ipc::Response::ok(format!("output: {}", o))
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
                    if self.state.mode == "live" && !caps.live {
                        warnings.push(format!("no live support, will resolve on record"));
                    }
                    if caps.lang == config::LangSupport::EnglishOnly && self.state.lang != "en" {
                        warnings.push("english only".into());
                    }
                    let _ = fs::write(&self.config.model_file, &m);
                    self.state.model = m.clone();
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
                        if caps.live { tags.push("live"); }
                        if caps.batch { tags.push("batch"); }
                        if caps.lang == config::LangSupport::EnglishOnly { tags.push("en-only"); }
                        let current = if *m == self.state.model { " *" } else { "" };
                        format!("  {m} [{tags}]{current}", tags = tags.join("+"))
                    }).collect::<Vec<_>>().join("\n");
                    ipc::Response::ok(format!(
                        "model: {}\navailable:\n{}",
                        self.state.model, list
                    ))
                }
            }
            other => ipc::Response::err(format!("unknown command: {}", other)),
        }
    }
}

pub async fn run() -> Result<()> {
    let config = Config::new();
    let state = State::load(&config);

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
                    tray::TrayCommand::SetLang(l) => {
                        daemon.handle_command(ipc::Request { command: "lang".into(), arg: Some(l) });
                    }
                    tray::TrayCommand::SetModel(m) => {
                        daemon.handle_command(ipc::Request { command: "model".into(), arg: Some(m) });
                    }
                    tray::TrayCommand::ToggleEnter => {
                        daemon.handle_command(ipc::Request { command: "enter".into(), arg: None });
                    }
                    tray::TrayCommand::ToggleCorrect => {
                        daemon.handle_command(ipc::Request { command: "correct".into(), arg: None });
                    }
                }
                daemon.sync_tray_state();
            }
            Some(ev) = fn_rx.recv() => {
                match ev {
                    fnkey::KeyEvent::Start => {
                        let _ = daemon.toggle_recording();
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
