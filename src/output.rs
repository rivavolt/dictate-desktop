use std::io::Write;
use std::process::Command;
use std::sync::Mutex;

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
    if let Ok(mut child) = Command::new("wl-copy")
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}
