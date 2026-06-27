use std::fs::File;
use std::io::BufReader;

const SOUNDS_DIR: &str = "/run/current-system/sw/share/sounds/freedesktop/stereo";

/// Play a freedesktop theme sound by event name, in-process via rodio.
///
/// Spawning `pw-play` proved unreliable from the daemon — it plays these files fine in isolation
/// but produced no audible output when launched by the long-running process. Decoding and rendering
/// the `.oga` directly sidesteps the spawn. Each call opens its own output stream on a detached
/// thread that lives until the clip finishes, so it's fire-and-forget and never blocks the caller.
fn play(name: &str) {
    let path = format!("{SOUNDS_DIR}/{name}.oga");
    std::thread::spawn(move || {
        let Ok((_stream, handle)) = rodio::OutputStream::try_default() else {
            return;
        };
        let Ok(file) = File::open(&path) else {
            return;
        };
        let Ok(source) = rodio::Decoder::new(BufReader::new(file)) else {
            return;
        };
        let Ok(sink) = rodio::Sink::try_new(&handle) else {
            return;
        };
        // A gentle cue, not a klaxon — the freedesktop clips play hot at full volume. There's no
        // system "alert volume" on pipewire to defer to, so scale it here.
        sink.set_volume(0.4);
        sink.append(source);
        // OutputStream isn't Send, so it lives here on the play thread (not a shared static); the
        // thread stays alive until the clip finishes.
        sink.sleep_until_end();
    });
}

pub fn play_start() {
    play("message-new-instant");
}

pub fn play_stop() {
    play("complete");
}
