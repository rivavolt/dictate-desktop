use std::io::Write;
use std::sync::Mutex;

use wl_clipboard_rs::copy::{MimeType, Options, Source};
use wrtype::WrtypeClient;

static CLIENT: std::sync::OnceLock<Mutex<Option<WrtypeClient>>> = std::sync::OnceLock::new();

fn get_client() -> &'static Mutex<Option<WrtypeClient>> {
    CLIENT.get_or_init(|| {
        match WrtypeClient::new() {
            Ok(c) => Mutex::new(Some(c)),
            Err(e) => {
                tracing::error!("virtual keyboard unavailable: {e}");
                Mutex::new(None)
            }
        }
    })
}

pub fn type_text(text: &str) {
    if !text.is_empty() {
        if let Ok(mut guard) = get_client().lock() {
            if let Some(client) = guard.as_mut() {
                let out = if text.ends_with(' ') { text.to_string() } else { format!("{} ", text) };
                if let Err(e) = client.type_text(&out) {
                    tracing::error!("type_text failed: {e}");
                }
            }
        }
    }
}

pub fn append_history(path: &std::path::Path, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
        let _ = writeln!(f, "[{ts}] {text}");
    }
}

pub fn copy_to_clipboard(text: &str) {
    static TX: std::sync::OnceLock<std::sync::mpsc::Sender<String>> = std::sync::OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                while let Ok(text) = rx.recv() {
                    let opts = Options::new();
                    if let Err(e) = opts.copy(Source::Bytes(text.into_bytes().into()), MimeType::Text) {
                        tracing::error!("clipboard copy failed: {e}");
                    }
                }
            })
            .expect("clipboard thread");
        tx
    });
    let _ = tx.send(text.to_string());
}
