use std::sync::{mpsc, Mutex, OnceLock};

use wl_clipboard_rs::copy::{MimeType, Options, ServeRequests, Source};
use wrtype::WrtypeClient;

static CLIENT: OnceLock<Mutex<WrtypeClient>> = OnceLock::new();

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

static CLIPBOARD_TX: OnceLock<mpsc::Sender<String>> = OnceLock::new();

fn clipboard_sender() -> &'static mpsc::Sender<String> {
    CLIPBOARD_TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            while let Ok(text) = rx.recv() {
                let mut opts = Options::new();
                opts.serve_requests(ServeRequests::Only(1));
                if let Err(e) = opts.copy(Source::Bytes(text.into_bytes().into()), MimeType::Text) {
                    tracing::error!("clipboard copy failed: {e}");
                }
            }
        });
        tx
    })
}

pub fn copy_to_clipboard(text: &str) {
    let _ = clipboard_sender().send(text.to_string());
}
