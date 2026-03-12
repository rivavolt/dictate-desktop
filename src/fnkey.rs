use anyhow::Result;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// Key events sent from keyd to the daemon
pub enum KeyEvent {
    /// Start recording (hold began or double-tap toggle)
    Start,
    /// Stop recording (hold released)
    Release,
}

pub async fn watch_fn_key(tx: mpsc::Sender<KeyEvent>) -> Result<()> {
    let combo_name = match std::env::var("DICTATE_TRIGGER").as_deref() {
        Ok("d") => "fn+d",
        Ok("f") => "fn+f",
        Ok("space") => "fn+space",
        _ => "fn+d",
    };

    let socket_path = keyd_socket_path();
    tracing::info!("connecting to keyd at {socket_path}, watching for {combo_name}");

    loop {
        match connect_and_watch(&socket_path, combo_name, &tx).await {
            Ok(()) => break,
            Err(e) => {
                tracing::warn!("keyd connection error: {e}, reconnecting in 2s");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }

    Ok(())
}

async fn connect_and_watch(
    socket_path: &str,
    combo_name: &str,
    tx: &mpsc::Sender<KeyEvent>,
) -> Result<()> {
    let stream = UnixStream::connect(socket_path).await?;
    let reader = BufReader::new(stream);
    let mut lines = reader.lines();

    let mut active = false;

    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };

        let Some(combo) = msg.get("combo").and_then(|v| v.as_str()) else {
            continue;
        };
        if combo != combo_name {
            continue;
        }

        let Some(event) = msg.get("event").and_then(|v| v.as_str()) else {
            continue;
        };

        match event {
            "hold_start" => {
                if !active {
                    active = true;
                    let _ = tx.send(KeyEvent::Start).await;
                } else {
                    // Toggle off on second activation
                    active = false;
                    let _ = tx.send(KeyEvent::Release).await;
                }
            }
            "hold_end" => {
                if active {
                    active = false;
                    let _ = tx.send(KeyEvent::Release).await;
                }
            }
            _ => {}
        }
    }

    Ok(())
}

fn keyd_socket_path() -> String {
    if let Ok(path) = std::env::var("KEYD_SOCKET") {
        return path;
    }
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        format!("{xdg}/keyd.sock")
    } else {
        "/tmp/keyd.sock".to_string()
    }
}
