use anyhow::Result;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::audio;
use crate::config::{Config, State};
use crate::deepgram;
use crate::fnkey;
use crate::ipc;
use crate::output;
use crate::sound;
use crate::tray;

struct DaemonState {
    config: Config,
    state: State,
    recording: bool,
    stop_flag: Option<Arc<AtomicBool>>,
    /// Keep the cpal Stream alive while recording in live mode
    _audio_stream: Option<cpal::Stream>,
    record_handle: Option<tokio::task::JoinHandle<()>>,
    tray_handle: ksni::Handle<tray::DictateTray>,
}

impl DaemonState {
    fn new(tray_handle: ksni::Handle<tray::DictateTray>) -> Self {
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
        }
    }

    fn start_recording(&mut self) -> Result<String> {
        if self.recording {
            return Ok("already recording".into());
        }

        self.recording = true;
        fs::write(&self.config.state_file, "recording")?;
        sound::play_start();
        let tray = self.tray_handle.clone();
        tokio::spawn(async move { tray.update(|t| t.set_recording(true)).await; });

        let stop = Arc::new(AtomicBool::new(false));
        self.stop_flag = Some(stop.clone());

        match self.state.mode.as_str() {
            "live" => self.start_live(stop)?,
            "batch" => self.start_batch(stop)?,
            "vad" => self.start_vad(stop)?,
            _ => self.start_live(stop)?,
        }

        Ok(format!("recording ({})", self.state.mode))
    }

    fn start_live(&mut self, stop: Arc<AtomicBool>) -> Result<()> {
        let (stream, audio_rx, sample_rate) = audio::capture_to_channel(stop.clone())?;
        self._audio_stream = Some(stream);

        let state = self.state.clone();
        self.record_handle = Some(tokio::spawn(async move {
            if let Err(e) = deepgram::stream_live(&state, audio_rx, stop, sample_rate).await {
                tracing::error!("live streaming error: {e}");
            }
        }));

        Ok(())
    }

    fn start_batch(&mut self, stop: Arc<AtomicBool>) -> Result<()> {
        let audio_file = self.config.audio_file.clone();
        let state = self.state.clone();
        let transcript_file = self.config.transcript_file.clone();

        self.record_handle = Some(tokio::spawn(async move {
            // Record in a blocking thread
            let audio_file2 = audio_file.clone();
            let stop2 = stop.clone();
            let record = tokio::task::spawn_blocking(move || {
                audio::record_to_file(&audio_file2, stop2)
            });

            // Wait for stop signal
            while !stop.load(Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }

            if let Err(e) = record.await {
                tracing::error!("batch record error: {e}");
                return;
            }

            // Transcribe
            match deepgram::transcribe_file(&audio_file, &state.lang, &state.model).await {
                Ok(transcript) if !transcript.is_empty() => {
                    output::type_text(&transcript);
                    let _ = fs::write(&transcript_file, &transcript);
                    output::copy_to_clipboard(&transcript);
                }
                Err(e) => tracing::error!("batch transcribe error: {e}"),
                _ => {}
            }

            let _ = fs::remove_file(&audio_file);
        }));

        Ok(())
    }

    fn start_vad(&mut self, stop: Arc<AtomicBool>) -> Result<()> {
        let audio_file = self.config.audio_file.with_extension("chunk.wav");
        let state = self.state.clone();
        let transcript_file = self.config.transcript_file.clone();

        self.record_handle = Some(tokio::spawn(async move {
            let mut full_transcript = String::new();
            let _ = fs::write(&transcript_file, "");

            while !stop.load(Ordering::Relaxed) {
                let chunk = audio_file.clone();
                let status = tokio::task::spawn_blocking(move || {
                    std::process::Command::new("sox")
                        .args(["-d", chunk.to_str().unwrap()])
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

                match deepgram::transcribe_file(&audio_file, &state.lang, &state.model).await {
                    Ok(transcript) if !transcript.is_empty() => {
                        output::type_text(&transcript);
                        full_transcript.push_str(&transcript);
                        full_transcript.push(' ');
                        let _ = fs::write(&transcript_file, &full_transcript);
                        output::copy_to_clipboard(&full_transcript);
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

        // Give the record handle a moment to finish
        if let Some(handle) = self.record_handle.take() {
            let _ = tokio::spawn(async move {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(3), handle).await;
            });
        }

        self.recording = false;
        fs::write(&self.config.state_file, "idle")?;
        sound::play_stop();
        let tray = self.tray_handle.clone();
        tokio::spawn(async move { tray.update(|t| t.set_recording(false)).await; });

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
                    "{} (mode: {}, lang: {}, model: {})",
                    status, self.state.mode, self.state.lang, self.state.model
                ))
            }
            "mode" => {
                if let Some(m) = req.arg {
                    if ["live", "vad", "batch"].contains(&m.as_str()) {
                        let _ = fs::write(&self.config.mode_file, &m);
                        self.state.mode = m.clone();
                        ipc::Response::ok(format!("mode: {}", m))
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
                    let _ = fs::write(&self.config.lang_file, &l);
                    self.state.lang = l.clone();
                    ipc::Response::ok(format!("language: {}", l))
                } else {
                    ipc::Response::ok(format!("language: {}", self.state.lang))
                }
            }
            other => ipc::Response::err(format!("unknown command: {}", other)),
        }
    }
}

pub async fn run() -> Result<()> {
    // Spawn system tray
    let (tray_tx, mut tray_rx) = mpsc::channel::<()>(4);
    let tray_handle = tray::spawn(tray_tx).await?;

    // Spawn Fn key watcher (evdev)
    let (fn_tx, mut fn_rx) = mpsc::channel::<fnkey::KeyEvent>(16);
    tokio::spawn(async move {
        if let Err(e) = fnkey::watch_fn_key(fn_tx).await {
            tracing::warn!("Fn key watcher failed: {e}");
        }
    });

    let mut daemon = DaemonState::new(tray_handle);

    tracing::info!(
        "starting daemon (mode: {}, lang: {}, model: {})",
        daemon.state.mode,
        daemon.state.lang,
        daemon.state.model
    );

    // Clean up stale state
    let _ = fs::write(&daemon.config.state_file, "idle");
    let _ = fs::remove_file(&daemon.config.socket_path);

    let listener = ipc::bind(&daemon.config.socket_path)?;
    tracing::info!("IPC socket: {}", daemon.config.socket_path.display());

    // Spawn IPC acceptor that forwards commands
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
