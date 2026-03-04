use anyhow::Result;
use evdev::{Device, EventType, InputEventKind, Key};
use futures::StreamExt;
use std::path::PathBuf;
use std::pin::Pin;
use tokio::sync::mpsc;

/// Key events sent from evdev to the daemon
pub enum KeyEvent {
    /// Start recording (hold began or double-tap toggle)
    Start,
    /// Stop recording (hold released)
    Release,
}

const DOUBLE_TAP_MS: u64 = 300;
const HOLD_THRESHOLD_MS: u64 = 200;

pub async fn watch_fn_key(tx: mpsc::Sender<KeyEvent>) -> Result<()> {
    let target_key = Key::KEY_FN;
    let devices = enumerate_key_devices(target_key)?;

    if devices.is_empty() {
        anyhow::bail!("no input device found with KEY_FN capability");
    }

    tracing::info!("watching {} device(s) for Fn key", devices.len());

    let mut handles = Vec::new();
    for path in devices {
        let tx = tx.clone();
        handles.push(tokio::spawn(watch_device(path, target_key, tx)));
    }

    futures::future::join_all(handles).await;
    Ok(())
}

fn enumerate_key_devices(key: Key) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let readdir = std::fs::read_dir("/dev/input")
        .map_err(|e| anyhow::anyhow!("can't read /dev/input: {e}"))?;

    for entry in readdir.flatten() {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !name.starts_with("event") {
            continue;
        }
        if let Ok(dev) = Device::open(&path) {
            if dev
                .supported_keys()
                .map_or(false, |keys| keys.contains(key))
            {
                tracing::debug!(
                    "found device: {} ({})",
                    path.display(),
                    dev.name().unwrap_or("?")
                );
                paths.push(path);
            }
        }
    }
    Ok(paths)
}

async fn watch_device(path: PathBuf, key: Key, tx: mpsc::Sender<KeyEvent>) {
    let dev = match Device::open(&path) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("failed to open {}: {e}", path.display());
            return;
        }
    };

    let mut stream = match dev.into_event_stream() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("failed to create stream for {}: {e}", path.display());
            return;
        }
    };

    let mut last_press = std::time::Instant::now() - std::time::Duration::from_secs(10);
    let mut held = false;
    let mut hold_active = false;
    let mut hold_timer: Option<Pin<Box<tokio::time::Sleep>>> = None;

    loop {
        tokio::select! {
            ev = stream.next() => {
                let Some(Ok(ev)) = ev else { break };
                if ev.event_type() != EventType::KEY || ev.kind() != InputEventKind::Key(key) {
                    continue;
                }

                match ev.value() {
                    1 => {
                        // Key down
                        let now = std::time::Instant::now();
                        held = true;

                        let since_last = now.duration_since(last_press).as_millis() as u64;
                        if since_last < DOUBLE_TAP_MS {
                            // Double-tap: toggle start/stop
                            let _ = tx.send(KeyEvent::Start).await;
                            hold_timer = None;
                        } else {
                            // Wait to see if it becomes a hold
                            hold_timer = Some(Box::pin(tokio::time::sleep(
                                std::time::Duration::from_millis(HOLD_THRESHOLD_MS),
                            )));
                        }
                        last_press = now;
                    }
                    0 => {
                        // Key up
                        held = false;
                        hold_timer = None;

                        if hold_active {
                            hold_active = false;
                            let _ = tx.send(KeyEvent::Release).await;
                        }
                    }
                    _ => {} // repeat, ignore
                }
            }
            _ = async {
                if let Some(ref mut timer) = hold_timer {
                    timer.await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                // Hold threshold reached while key still down
                hold_timer = None;
                if held {
                    hold_active = true;
                    let _ = tx.send(KeyEvent::Start).await;
                }
            }
        }
    }

    tracing::debug!("device stream ended: {}", path.display());
}
