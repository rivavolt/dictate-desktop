use anyhow::{Context, Result};
use evdev::{Device, EventType, Key};
use std::path::PathBuf;
use tokio::sync::mpsc;

/// Key events sent from the hotkey watcher to the daemon.
pub enum KeyEvent {
    /// Trigger pressed — begin recording (push-to-talk).
    Start,
    /// Trigger released — stop recording.
    Release,
}

/// The push-to-talk trigger key. Defaults to Fn/Globe (`KEY_FN`), which the Apple
/// internal keyboard emits as a normal press/release at the evdev layer. Override with
/// `DICTATE_TRIGGER` for keyboards without a usable Fn key.
fn trigger_key() -> Key {
    match std::env::var("DICTATE_TRIGGER").as_deref() {
        Ok("f24") => Key::KEY_F24,
        Ok("rightalt") => Key::KEY_RIGHTALT,
        Ok("rightctrl") => Key::KEY_RIGHTCTRL,
        Ok("rightmeta") => Key::KEY_RIGHTMETA,
        Ok("compose") | Ok("menu") => Key::KEY_COMPOSE,
        _ => Key::KEY_FN,
    }
}

/// Watch the trigger key for press/release and forward push-to-talk events. We only
/// read the device (never grab it), so the key keeps working normally for everything
/// else — and a bare Fn press does nothing in the compositor anyway.
pub async fn watch_fn_key(tx: mpsc::Sender<KeyEvent>) -> Result<()> {
    let key = trigger_key();
    loop {
        if let Err(e) = watch_once(key, &tx).await {
            tracing::warn!("hotkey watcher error: {e}, retrying in 2s");
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// First input device that advertises the trigger key. `KEY_FN` is unique to the
/// Apple keyboard; common fallback triggers may match several devices, so we take the first.
fn find_device(key: Key) -> Option<(PathBuf, Device)> {
    let mut candidates: Vec<(PathBuf, Device)> = evdev::enumerate()
        .filter(|(_, dev)| dev.supported_keys().map_or(false, |keys| keys.contains(key)))
        .collect();
    // Prefer keyd's virtual keyboard: when keyd grabs the physical device and remaps the
    // trigger (Fn→F24), the remapped key only surfaces on keyd's output — the physical
    // device is grabbed and yields nothing.
    candidates.sort_by_key(|(_, dev)| {
        let is_keyd = dev.name().map_or(false, |n| n.to_lowercase().contains("keyd"));
        u8::from(!is_keyd)
    });
    candidates.into_iter().next()
}

async fn watch_once(key: Key, tx: &mpsc::Sender<KeyEvent>) -> Result<()> {
    let (path, device) =
        find_device(key).with_context(|| format!("no input device exposes {key:?}"))?;
    tracing::info!("watching {} for {key:?} (push-to-talk)", path.display());

    let mut events = device.into_event_stream()?;
    loop {
        let ev = events.next_event().await?;
        if ev.event_type() != EventType::KEY || ev.code() != key.code() {
            continue;
        }
        match ev.value() {
            1 => {
                let _ = tx.send(KeyEvent::Start).await;
            }
            0 => {
                let _ = tx.send(KeyEvent::Release).await;
            }
            _ => {} // 2 = autorepeat while held — ignore
        }
    }
}
