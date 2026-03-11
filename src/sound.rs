use std::process::{Command, Stdio};

const SOUNDS_DIR: &str = "/run/current-system/sw/share/sounds/freedesktop/stereo";

fn play(name: &str) {
    let path = format!("{SOUNDS_DIR}/{name}.oga");
    let Ok(child) = Command::new("pw-play")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    std::thread::spawn(move || { let _ = child.wait_with_output(); });
}

pub fn play_start() {
    play("message-new-instant");
}

pub fn play_stop() {
    play("complete");
}
