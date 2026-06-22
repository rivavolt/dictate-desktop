use std::io::Write;
use std::sync::{Mutex, OnceLock};

use wl_clipboard_rs::copy::{MimeType, Options, Source};
use wrtype::WrtypeClient;

use crate::inputmethod;

static CLIENT: std::sync::OnceLock<Mutex<Option<WrtypeClient>>> = std::sync::OnceLock::new();

// Text output prefers the Wayland input-method protocol: it commits the transcript as a
// string straight to the focused field, with no synthetic keystrokes — so it can't trip a
// keybind (the bare-key dropdown toggle) and isn't subject to wrtype's keycode-vs-layout
// mismatch. We fall back to wrtype only when no text-input is active (XWayland apps and apps
// without text-input-v3 never activate the input method).
static INPUT_METHOD: OnceLock<Option<inputmethod::Handle>> = OnceLock::new();

fn input_method() -> Option<&'static inputmethod::Handle> {
    INPUT_METHOD
        .get_or_init(|| match inputmethod::spawn() {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::warn!("input-method unavailable, falling back to wrtype: {e}");
                None
            }
        })
        .as_ref()
}

/// Start the input-method client at daemon boot so a text-input is already associated by the
/// time the first transcript lands (avoids an unnecessary wrtype fallback on first use).
pub fn init() {
    let _ = input_method();
}

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

/// Deliver `text` via the Wayland input method if a text-input is focused. Returns true if
/// committed — the whole string in one atomic `commit_string`, so no per-character keystrokes
/// (no dropped chars, nothing that can trip a keybind). Returns false if no input method is
/// active (kitty/TUIs and XWayland never activate one), so the caller falls back to `paste`.
pub fn type_text(text: &str) -> bool {
    if text.is_empty() {
        return true;
    }
    if let Some(im) = input_method() {
        if im.is_active() {
            let out = if text.ends_with(' ') { text.to_string() } else { format!("{} ", text) };
            im.commit(out);
            return true;
        }
    }
    false
}

/// Fallback when no input method is active: the transcript is already on the clipboard (caller
/// copied it); optionally paste it with Ctrl+Shift+V (kitty's paste, and paste-plain in most
/// GUI apps), and always toast. Only the paste *chord* is synthesized — never per-character
/// typing — so chars can't drop and no bare-key bind (the dropdown) can be tripped.
pub fn paste(auto_paste: bool) {
    // The transcript is already on the clipboard (the caller copied it). Optionally synthesize
    // the paste chord; the daemon shows the toast in the overlay. Only the chord is sent —
    // never per-character typing — so chars can't drop and no bare-key bind can be tripped.
    if !auto_paste {
        return;
    }
    // Synthesize the paste chord through the compositor (hyprctl), not a wrtype virtual
    // keyboard. wrtype re-uploads its XKB keymap on the "v" KeyPress, and that upload races the
    // held-modifier state, so Ctrl+Shift intermittently drops and a bare "v" leaks into the app.
    // Hyprland's send_shortcut uses the real seat keymap and modifier handling, so it lands intact.
    let out = std::process::Command::new("hyprctl")
        .args([
            "dispatch",
            r#"hl.dsp.send_shortcut({ mods = "CTRL SHIFT", key = "V", window = "activewindow" })"#,
        ])
        .output();
    match out {
        Ok(o) => {
            let reply = String::from_utf8_lossy(&o.stdout);
            if reply.trim() != "ok" {
                tracing::error!("paste: hyprctl send_shortcut: {}", reply.trim());
            }
        }
        Err(e) => tracing::error!("paste: hyprctl spawn failed: {e}"),
    }
}

pub fn type_enter() {
    // Enter stays on wrtype — the input-method protocol commits text, not key events, and a
    // committed "\n" won't trigger submit. Return isn't a bare-key bind, so no collision risk.
    if let Ok(mut guard) = get_client().lock() {
        if let Some(client) = guard.as_mut() {
            if let Err(e) = client.type_key("Return") {
                tracing::error!("type_enter failed: {e}");
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

/// One structured history row. `audio` is the FLAC filename within the audio dir (linked by
/// a shared timestamp), so each transcript points at exactly the recording it came from.
#[derive(serde::Serialize)]
pub struct HistoryRecord<'a> {
    pub ts: &'a str,
    pub audio: Option<&'a str>,
    pub mode: &'a str,
    pub model: &'a str,
    pub lang: &'a str,
    pub raw: &'a str,
    pub text: &'a str,
    pub corrected: bool,
    pub duration_ms: u64,
    pub latency_ms: u64,
}

pub fn append_history_record(path: &std::path::Path, record: &HistoryRecord) {
    if let Ok(line) = serde_json::to_string(record) {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
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
