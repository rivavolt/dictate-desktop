use anyhow::Result;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::audio;
use crate::config::{self, Config, State};
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
    fn new(tray_handle: Option<ksni::Handle<tray::DictateTray>>, overlay: overlay::Handle) -> Self {
        let config = Config::new();
        let state = State::load(&config);
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

    fn start_recording(&mut self) -> Result<String> {
        if self.recording {
            return Ok("already recording".into());
        }

        self.recording = true;
        fs::write(&self.config.state_file, "recording")?;
        sound::play_start();
        if let Some(tray) = self.tray_handle.clone() {
            tokio::spawn(async move { tray.update(|t| t.set_recording(true)).await; });
        }
        if self.state.output == "clipboard" {
            self.overlay.show();
        }

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
        let (stream, audio_rx, sample_rate) = audio::capture_to_channel(stop.clone())?;
        self._audio_stream = Some(stream);

        let (tx, mut rx) = mpsc::unbounded_channel::<TranscriptEvent>();

        let output_mode = self.state.output.clone();
        let transcript_file = self.config.transcript_file.clone();
        let overlay_handle = self.overlay.clone();
        let state = self.state.clone();
        let provider = provider.to_string();
        self.record_handle = Some(tokio::spawn(async move {
            let is_clipboard = output_mode == "clipboard";
            let event_handler = tokio::spawn(async move {
                let mut last_accumulated = String::new();
                let mut last_pending = String::new();
                while let Some(event) = rx.recv().await {
                    match event {
                        TranscriptEvent::Final { delta, accumulated } => {
                            tracing::info!("transcript: {delta}");
                            output::copy_to_clipboard(&accumulated);
                            if is_clipboard {
                                overlay_handle.set_text(accumulated.clone());
                            } else {
                                output::type_text(&delta);
                            }
                            let _ = std::fs::write(&transcript_file, &accumulated);
                            last_accumulated = accumulated;
                            last_pending.clear();
                        }
                        TranscriptEvent::Interim(text) => {
                            if is_clipboard {
                                overlay_handle.set_pending(text.clone());
                            }
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
                    if is_clipboard {
                        output::copy_to_clipboard(&last_accumulated);
                        overlay_handle.set_text(last_accumulated.clone());
                    } else {
                        output::type_text(&last_pending);
                        output::copy_to_clipboard(&last_accumulated);
                    }
                    let _ = std::fs::write(&transcript_file, &last_accumulated);
                }
                if is_clipboard {
                    overlay_handle.copied();
                }
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
            // tx dropped here — wait for event handler to drain remaining events
            let _ = event_handler.await;
        }));

        Ok(())
    }

    fn start_batch(&mut self, stop: Arc<AtomicBool>, provider: &str) -> Result<()> {
        let audio_file = self.config.audio_file.clone();
        let state = self.state.clone();
        let transcript_file = self.config.transcript_file.clone();
        let provider = provider.to_string();

        self.record_handle = Some(tokio::spawn(async move {
            let audio_file2 = audio_file.clone();
            let stop2 = stop.clone();
            let record = tokio::task::spawn_blocking(move || {
                audio::record_to_file(&audio_file2, stop2)
            });

            while !stop.load(Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }

            if let Err(e) = record.await {
                tracing::error!("batch record error: {e}");
                return;
            }

            let (_, model) = config::parse_provider_model(&state.model);
            let result = match provider.as_str() {
                "groq" => groq::transcribe_file(&audio_file, &state.lang, model).await,
                "fireworks" => fireworks::transcribe_file(&audio_file, &state.lang, model).await,
                _ => deepgram::transcribe_file(&audio_file, &state.lang, model).await,
            };

            match result {
                Ok(transcript) if !transcript.is_empty() => {
                    if state.output == "clipboard" {
                        output::copy_to_clipboard(&transcript);
                    } else {
                        output::type_text(&transcript);
                        output::copy_to_clipboard(&transcript);
                    }
                    let _ = fs::write(&transcript_file, &transcript);
                }
                Err(e) => tracing::error!("batch transcribe error: {e}"),
                _ => {}
            }

            let _ = fs::remove_file(&audio_file);
        }));

        Ok(())
    }

    fn start_vad(&mut self, stop: Arc<AtomicBool>, provider: &str) -> Result<()> {
        let audio_file = self.config.audio_file.with_extension("chunk.wav");
        let state = self.state.clone();
        let transcript_file = self.config.transcript_file.clone();
        let provider = provider.to_string();

        self.record_handle = Some(tokio::spawn(async move {
            let mut full_transcript = String::new();
            let _ = fs::write(&transcript_file, "");

            while !stop.load(Ordering::Relaxed) {
                let chunk = audio_file.clone();
                let status = tokio::task::spawn_blocking(move || {
                    std::process::Command::new("sox")
                        .args(["-d", &chunk.to_string_lossy()])
                        .args(["silence", "1", "0.1", "1%", "1", "0.8", "1%"])
                        .stderr(std::process::Stdio::null())
                        .status()
                })
                .await;

                let ok = matches!(status, Ok(Ok(s)) if s.success());
                if !ok || stop.load(Ordering::Relaxed) {
                    break;
                }

                let size = fs::metadata(&audio_file).map(|m| m.len()).unwrap_or(0);
                if size < 1000 {
                    continue;
                }

                let (_, model) = config::parse_provider_model(&state.model);
                let result = match provider.as_str() {
                    "groq" => groq::transcribe_file(&audio_file, &state.lang, model).await,
                    "fireworks" => {
                        fireworks::transcribe_file(&audio_file, &state.lang, model).await
                    }
                    _ => deepgram::transcribe_file(&audio_file, &state.lang, model).await,
                };

                match result {
                    Ok(transcript) if !transcript.is_empty() => {
                        full_transcript.push_str(&transcript);
                        if !full_transcript.ends_with(' ') {
                            full_transcript.push(' ');
                        }
                        if state.output == "clipboard" {
                            output::copy_to_clipboard(&full_transcript);
                        } else {
                            output::type_text(&transcript);
                            output::copy_to_clipboard(&full_transcript);
                        }
                        let _ = fs::write(&transcript_file, &full_transcript);
                    }
                    Err(e) => tracing::error!("vad transcribe error: {e}"),
                    _ => {}
                }

                let _ = fs::remove_file(&audio_file);
            }

            let _ = fs::remove_file(&audio_file);
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
                let _ = tokio::time::timeout(std::time::Duration::from_secs(6), handle).await;
            });
        }

        self.recording = false;
        fs::write(&self.config.state_file, "idle")?;
        sound::play_stop();
        if let Some(tray) = self.tray_handle.clone() {
            tokio::spawn(async move { tray.update(|t| t.set_recording(false)).await; });
        }

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
                    let mut msg = format!("language: {lang}");
                    if let Some(new_model) = config::resolve_model(&self.state.model, &self.state.mode, &self.state.lang) {
                        self.state.model = new_model.clone();
                        let _ = fs::write(&self.config.model_file, &self.state.model);
                        msg.push_str(&format!(" (switched model to {new_model})"));
                    }
                    ipc::Response::ok(msg)
                } else {
                    ipc::Response::ok(format!("language: {}", self.state.lang))
                }
            }
            "font" => {
                if let Some(f) = req.arg {
                    let _ = fs::write(&self.config.font_file, &f);
                    self.state.font = f.clone();
                    ipc::Response::ok(format!("font: {} (restart daemon to apply)", f))
                } else {
                    ipc::Response::ok(format!("font: {}", self.state.font))
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
    let (tray_tx, mut tray_rx) = mpsc::channel::<()>(4);
    let tray_handle = match tray::spawn(tray_tx).await {
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

    let config = Config::new();
    let state = State::load(&config);

    let overlay_handle = overlay::spawn(state.font.clone())?;

    let mut daemon = DaemonState::new(tray_handle, overlay_handle);

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

    loop {
        tokio::select! {
            Some((req, resp_tx)) = ipc_rx.recv() => {
                let resp = daemon.handle_command(req);
                let _ = resp_tx.send(resp);
            }
            Some(()) = tray_rx.recv() => {
                let _ = daemon.toggle_recording();
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
