use ksni::menu::*;
use ksni::{self, Icon, Tray, TrayMethods};
use tokio::sync::mpsc;

use crate::audio;
use crate::config;

// Phosphor Icons (MIT) — CursorText bold/fill for dictation (distinct from mic and assistant chat)
const ICON_BOLD_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" fill="FILL"><path d="M188,208a12,12,0,0,1-12,12H160a44.05,44.05,0,0,1-32-13.85A44.05,44.05,0,0,1,96,220H80a12,12,0,0,1,0-24H96a20,20,0,0,0,20-20V140H104a12,12,0,0,1,0-24h12V80A20,20,0,0,0,96,60H80a12,12,0,0,1,0-24H96a44.05,44.05,0,0,1,32,13.85A44.05,44.05,0,0,1,160,36h16a12,12,0,0,1,0,24H160a20,20,0,0,0-20,20v36h12a12,12,0,0,1,0,24H140v36a20,20,0,0,0,20,20h16A12,12,0,0,1,188,208Z"/></svg>"#;
const ICON_FILL_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" fill="FILL"><path d="M188,208a12,12,0,0,1-12,12H160a44.05,44.05,0,0,1-32-13.85A44.05,44.05,0,0,1,96,220H80a12,12,0,0,1,0-24H96a20,20,0,0,0,20-20V140H104a12,12,0,0,1,0-24h12V80A20,20,0,0,0,96,60H80a12,12,0,0,1,0-24H96a44.05,44.05,0,0,1,32,13.85A44.05,44.05,0,0,1,160,36h16a12,12,0,0,1,0,24H160a20,20,0,0,0-20,20v36h12a12,12,0,0,1,0,24H140v36a20,20,0,0,0,20,20h16A12,12,0,0,1,188,208Z"/></svg>"#;

fn render_icon(svg_template: &str, color: &str, size: u32) -> Icon {
    let svg = svg_template.replace("FILL", color);
    let tree = resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default())
        .expect("embedded SVG is valid");
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).unwrap();
    let sx = size as f32 / tree.size().width();
    let sy = size as f32 / tree.size().height();
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(sx, sy), &mut pixmap.as_mut());
    // tiny-skia premultiplied RGBA → non-premultiplied ARGB (network byte order)
    let mut argb = Vec::with_capacity((size * size * 4) as usize);
    for pixel in pixmap.pixels() {
        let a = pixel.alpha();
        let (r, g, b) = if a > 0 && a < 255 {
            let a_f = a as f32 / 255.0;
            (
                (pixel.red() as f32 / a_f).min(255.0) as u8,
                (pixel.green() as f32 / a_f).min(255.0) as u8,
                (pixel.blue() as f32 / a_f).min(255.0) as u8,
            )
        } else {
            (pixel.red(), pixel.green(), pixel.blue())
        };
        argb.push(a);
        argb.push(r);
        argb.push(g);
        argb.push(b);
    }
    Icon { width: size as i32, height: size as i32, data: argb }
}

struct TrayIcons {
    idle: Vec<Icon>,
    recording: Vec<Icon>,
}

fn make_icons() -> TrayIcons {
    let sizes = [20, 40];
    TrayIcons {
        idle: sizes.iter().map(|&s| render_icon(ICON_BOLD_SVG, "#FFFFFF", s)).collect(),
        recording: sizes.iter().map(|&s| render_icon(ICON_FILL_SVG, "#E04040", s)).collect(),
    }
}

pub enum TrayCommand {
    Toggle,
    SetMode(String),
    SetOutput(String),
    ToggleLang(String),
    SetModel(String),
    SetInput(String),
    ToggleEnter,
    ToggleCorrect,
    ToggleFillers,
    ToggleAutoPaste,
    SetOverlay(String),
    CopyHistory(String),
}

const MODES: &[&str] = &["live", "batch", "vad"];
const MODE_LABELS: &[&str] = &["Live", "Batch", "VAD"];
const OUTPUTS: &[&str] = &["type", "clipboard"];
const OUTPUT_LABELS: &[&str] = &["Type", "Clipboard"];
const OVERLAY_MODES: &[&str] = &["off", "status", "full"];
const OVERLAY_LABELS: &[&str] = &["Off", "Status only", "Full"];

fn sorted_languages() -> Vec<(&'static str, &'static str)> {
    let mut langs: Vec<_> = config::LANGUAGES.iter().copied().collect();
    // Keep Auto first, sort rest alphabetically by name
    langs[1..].sort_by_key(|&(_, name)| name);
    langs
}

/// Most-recent `n` transcriptions from history.jsonl, newest first, as (menu label, full text).
fn recent_history(path: &std::path::Path, n: usize) -> Vec<(String, String)> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    content
        .lines()
        .rev()
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            let text = v.get("text")?.as_str()?.trim().to_string();
            if text.is_empty() {
                return None;
            }
            let label = if text.chars().count() > 48 {
                format!("{}…", text.chars().take(48).collect::<String>())
            } else {
                text.clone()
            };
            Some((label, text))
        })
        .take(n)
        .collect()
}

pub struct DictateTray {
    recording: bool,
    mode: usize,
    output: usize,
    languages: Vec<String>,
    model: usize,
    input: usize,
    input_devices: Vec<String>,
    enter: bool,
    correct: bool,
    remove_fillers: bool,
    auto_paste: bool,
    overlay: usize,
    langs: Vec<(&'static str, &'static str)>,
    history_path: std::path::PathBuf,
    icons: TrayIcons,
    cmd_tx: mpsc::Sender<TrayCommand>,
}

impl DictateTray {
    pub fn set_recording(&mut self, recording: bool) {
        self.recording = recording;
    }

    pub fn set_state(&mut self, state: &config::State) {
        self.mode = MODES.iter().position(|&m| m == state.mode).unwrap_or(0);
        self.output = OUTPUTS.iter().position(|&o| o == state.output).unwrap_or(0);
        self.languages = state.languages.clone();
        self.model = config::ALL_MODELS
            .iter()
            .position(|&m| m == state.model)
            .unwrap_or(0);
        self.input_devices = audio::list_input_devices();
        let match_name = if state.input.is_empty() { audio::default_input_name() } else { state.input.clone() };
        self.input = self.input_devices.iter().position(|d| d == &match_name).unwrap_or(0);
        self.enter = state.enter;
        self.correct = state.correct;
        self.remove_fillers = state.remove_fillers;
        self.auto_paste = state.auto_paste;
        self.overlay = OVERLAY_MODES.iter().position(|&m| m == state.overlay_mode().name()).unwrap_or(0);
    }
}

impl Tray for DictateTray {
    fn id(&self) -> String {
        "dictate-desktop".into()
    }

    fn title(&self) -> String {
        if self.recording {
            "Dictate (recording)".into()
        } else {
            "Dictate".into()
        }
    }

    fn icon_name(&self) -> String {
        String::new()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        self.icons.idle.clone()
    }

    fn attention_icon_pixmap(&self) -> Vec<Icon> {
        self.icons.recording.clone()
    }

    fn status(&self) -> ksni::Status {
        if self.recording {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.cmd_tx.try_send(TrayCommand::Toggle);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let tx = self.cmd_tx.clone();
        let mode_menu = SubMenu {
            label: "Mode".into(),
            submenu: vec![RadioGroup {
                selected: self.mode,
                select: Box::new(move |tray: &mut Self, idx| {
                    tray.mode = idx;
                    if let Some(&m) = MODES.get(idx) {
                        let _ = tx.try_send(TrayCommand::SetMode(m.into()));
                    }
                }),
                options: MODE_LABELS
                    .iter()
                    .map(|&l| RadioItem {
                        label: l.into(),
                        ..Default::default()
                    })
                    .collect(),
            }
            .into()],
            ..Default::default()
        };

        let tx = self.cmd_tx.clone();
        let output_menu = SubMenu {
            label: "Output".into(),
            submenu: vec![RadioGroup {
                selected: self.output,
                select: Box::new(move |tray: &mut Self, idx| {
                    tray.output = idx;
                    if let Some(&o) = OUTPUTS.get(idx) {
                        let _ = tx.try_send(TrayCommand::SetOutput(o.into()));
                    }
                }),
                options: OUTPUT_LABELS
                    .iter()
                    .map(|&l| RadioItem {
                        label: l.into(),
                        ..Default::default()
                    })
                    .collect(),
            }
            .into()],
            ..Default::default()
        };

        // Preferred-language set as checkboxes: tick the languages you speak; auto-detect
        // runs among them (and they become the candidate set for APIs that support it).
        let lang_menu = SubMenu {
            label: "Languages".into(),
            submenu: self
                .langs
                .iter()
                .filter(|(c, _)| *c != config::AUTO_LANG)
                .map(|&(code, name)| {
                    let code = code.to_string();
                    let checked = self.languages.iter().any(|c| c == &code);
                    CheckmarkItem {
                        label: name.into(),
                        checked,
                        activate: Box::new(move |tray: &mut Self| {
                            if let Some(pos) = tray.languages.iter().position(|c| c == &code) {
                                if tray.languages.len() > 1 {
                                    tray.languages.remove(pos);
                                }
                            } else {
                                tray.languages.push(code.clone());
                            }
                            let _ = tray.cmd_tx.try_send(TrayCommand::ToggleLang(code.clone()));
                        }),
                        ..Default::default()
                    }
                    .into()
                })
                .collect(),
            ..Default::default()
        };

        let tx = self.cmd_tx.clone();
        let model_menu = SubMenu {
            label: "Model".into(),
            submenu: vec![RadioGroup {
                selected: self.model,
                select: Box::new(move |tray: &mut Self, idx| {
                    tray.model = idx;
                    if let Some(&m) = config::ALL_MODELS.get(idx) {
                        let _ = tx.try_send(TrayCommand::SetModel(m.into()));
                    }
                }),
                options: config::ALL_MODELS
                    .iter()
                    .map(|&m| RadioItem {
                        label: m.into(),
                        ..Default::default()
                    })
                    .collect(),
            }
            .into()],
            ..Default::default()
        };

        let input_devices = self.input_devices.clone();
        let tx = self.cmd_tx.clone();
        let input_menu = SubMenu {
            label: "Input".into(),
            submenu: vec![RadioGroup {
                selected: self.input,
                select: Box::new(move |tray: &mut Self, idx| {
                    tray.input = idx;
                    if let Some(name) = input_devices.get(idx) {
                        let _ = tx.try_send(TrayCommand::SetInput(name.clone()));
                    }
                }),
                options: self.input_devices
                    .iter()
                    .map(|d| RadioItem {
                        label: d.clone(),
                        ..Default::default()
                    })
                    .collect(),
            }
            .into()],
            ..Default::default()
        };

        let enter_item = CheckmarkItem {
            label: "Press Enter".into(),
            checked: self.enter,
            activate: Box::new(|tray: &mut Self| {
                tray.enter = !tray.enter;
                let _ = tray.cmd_tx.try_send(TrayCommand::ToggleEnter);
            }),
            ..Default::default()
        };

        let correct_item = CheckmarkItem {
            label: "LLM Correct".into(),
            checked: self.correct,
            activate: Box::new(|tray: &mut Self| {
                tray.correct = !tray.correct;
                let _ = tray.cmd_tx.try_send(TrayCommand::ToggleCorrect);
            }),
            ..Default::default()
        };

        let fillers_item = CheckmarkItem {
            label: "Remove fillers (um/uh)".into(),
            checked: self.remove_fillers,
            activate: Box::new(|tray: &mut Self| {
                tray.remove_fillers = !tray.remove_fillers;
                let _ = tray.cmd_tx.try_send(TrayCommand::ToggleFillers);
            }),
            ..Default::default()
        };

        let auto_paste_item = CheckmarkItem {
            label: "Auto-paste (no IME apps)".into(),
            checked: self.auto_paste,
            activate: Box::new(|tray: &mut Self| {
                tray.auto_paste = !tray.auto_paste;
                let _ = tray.cmd_tx.try_send(TrayCommand::ToggleAutoPaste);
            }),
            ..Default::default()
        };

        let tx = self.cmd_tx.clone();
        let overlay_menu = SubMenu {
            label: "Overlay".into(),
            submenu: vec![RadioGroup {
                selected: self.overlay,
                select: Box::new(move |tray: &mut Self, idx| {
                    tray.overlay = idx;
                    if let Some(&m) = OVERLAY_MODES.get(idx) {
                        let _ = tx.try_send(TrayCommand::SetOverlay(m.into()));
                    }
                }),
                options: OVERLAY_LABELS
                    .iter()
                    .map(|&l| RadioItem {
                        label: l.into(),
                        ..Default::default()
                    })
                    .collect(),
            }
            .into()],
            ..Default::default()
        };

        let history_menu = SubMenu {
            label: "History".into(),
            submenu: recent_history(&self.history_path, 12)
                .into_iter()
                .map(|(label, full)| {
                    StandardItem {
                        label,
                        activate: Box::new(move |tray: &mut Self| {
                            let _ = tray.cmd_tx.try_send(TrayCommand::CopyHistory(full.clone()));
                        }),
                        ..Default::default()
                    }
                    .into()
                })
                .collect(),
            ..Default::default()
        };

        vec![
            mode_menu.into(),
            output_menu.into(),
            lang_menu.into(),
            model_menu.into(),
            input_menu.into(),
            history_menu.into(),
            MenuItem::Separator,
            enter_item.into(),
            correct_item.into(),
            fillers_item.into(),
            auto_paste_item.into(),
            overlay_menu.into(),
        ]
    }
}

pub async fn spawn(
    cmd_tx: mpsc::Sender<TrayCommand>,
    state: &config::State,
) -> anyhow::Result<ksni::Handle<DictateTray>> {
    let langs = sorted_languages();
    let input_devices = audio::list_input_devices();
    let match_name = if state.input.is_empty() { audio::default_input_name() } else { state.input.clone() };
    let input_idx = input_devices.iter().position(|d| d == &match_name).unwrap_or(0);
    let tray = DictateTray {
        recording: false,
        mode: MODES.iter().position(|&m| m == state.mode).unwrap_or(0),
        output: OUTPUTS.iter().position(|&o| o == state.output).unwrap_or(0),
        languages: state.languages.clone(),
        model: config::ALL_MODELS.iter().position(|&m| m == state.model).unwrap_or(0),
        input: input_idx,
        input_devices,
        enter: state.enter,
        correct: state.correct,
        remove_fillers: state.remove_fillers,
        auto_paste: state.auto_paste,
        overlay: OVERLAY_MODES.iter().position(|&m| m == state.overlay_mode().name()).unwrap_or(0),
        langs,
        history_path: config::Config::new().history_file.with_file_name("history.jsonl"),
        icons: make_icons(),
        cmd_tx,
    };
    let handle = tray.spawn().await?;
    Ok(handle)
}
