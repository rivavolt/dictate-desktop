use std::sync::Mutex;

use wl_clipboard_rs::copy::{MimeType, Options, Source};
use wrtype::WrtypeClient;

static CLIENT: std::sync::OnceLock<Mutex<WrtypeClient>> = std::sync::OnceLock::new();

fn get_client() -> &'static Mutex<WrtypeClient> {
    CLIENT.get_or_init(|| {
        Mutex::new(WrtypeClient::new().expect("failed to connect to Wayland virtual keyboard"))
    })
}

pub fn type_text(text: &str) {
    if !text.is_empty() {
        if let Ok(mut client) = get_client().lock() {
            if let Err(e) = client.type_text(&format!("{} ", text)) {
                tracing::error!("type_text failed: {e}");
            }
        }
    }
}

pub fn copy_to_clipboard(text: &str) {
    let text = text.to_string();
    std::thread::spawn(move || {
        let opts = Options::new();
        if let Err(e) = opts.copy(Source::Bytes(text.into_bytes().into()), MimeType::Text) {
            tracing::error!("clipboard copy failed: {e}");
        }
    });
}
