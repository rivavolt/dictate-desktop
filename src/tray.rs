use ksni::menu::*;
use ksni::{self, Tray, TrayMethods};
use tokio::sync::mpsc;

use crate::config;

pub enum TrayCommand {
    Toggle,
    SetMode(String),
    SetOutput(String),
    SetLang(String),
    SetModel(String),
    ToggleEnter,
}

const MODES: &[&str] = &["live", "batch", "vad"];
const MODE_LABELS: &[&str] = &["Live", "Batch", "VAD"];
const OUTPUTS: &[&str] = &["type", "clipboard"];
const OUTPUT_LABELS: &[&str] = &["Type", "Clipboard"];

fn sorted_languages() -> Vec<(&'static str, &'static str)> {
    let mut langs: Vec<_> = config::LANGUAGES.iter().copied().collect();
    // Keep Auto first, sort rest alphabetically by name
    langs[1..].sort_by_key(|&(_, name)| name);
    langs
}

pub struct DictateTray {
    recording: bool,
    mode: usize,
    output: usize,
    lang: usize,
    model: usize,
    enter: bool,
    langs: Vec<(&'static str, &'static str)>,
    cmd_tx: mpsc::Sender<TrayCommand>,
}

impl DictateTray {
    pub fn set_recording(&mut self, recording: bool) {
        self.recording = recording;
    }

    pub fn set_state(&mut self, state: &config::State) {
        self.mode = MODES.iter().position(|&m| m == state.mode).unwrap_or(0);
        self.output = OUTPUTS.iter().position(|&o| o == state.output).unwrap_or(0);
        self.lang = self
            .langs
            .iter()
            .position(|(c, _)| *c == state.lang)
            .unwrap_or(0);
        self.model = config::ALL_MODELS
            .iter()
            .position(|&m| m == state.model)
            .unwrap_or(0);
        self.enter = state.enter;
    }
}

impl Tray for DictateTray {
    fn id(&self) -> String {
        "dictate".into()
    }

    fn title(&self) -> String {
        if self.recording {
            "Dictate (recording)".into()
        } else {
            "Dictate".into()
        }
    }

    fn icon_name(&self) -> String {
        if self.recording {
            "media-record".into()
        } else {
            "audio-input-microphone".into()
        }
    }

    fn status(&self) -> ksni::Status {
        ksni::Status::Active
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.recording = !self.recording;
        let _ = self.cmd_tx.try_send(TrayCommand::Toggle);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mode_menu = SubMenu {
            label: "Mode".into(),
            submenu: MODES
                .iter()
                .zip(MODE_LABELS.iter())
                .enumerate()
                .map(|(i, (&val, &label))| {
                    let val = val.to_string();
                    CheckmarkItem {
                        label: label.into(),
                        checked: self.mode == i,
                        activate: Box::new(move |tray: &mut Self| {
                            tray.mode = i;
                            let _ = tray.cmd_tx.try_send(TrayCommand::SetMode(val.clone()));
                        }),
                        ..Default::default()
                    }
                    .into()
                })
                .collect(),
            ..Default::default()
        };

        let output_menu = SubMenu {
            label: "Output".into(),
            submenu: OUTPUTS
                .iter()
                .zip(OUTPUT_LABELS.iter())
                .enumerate()
                .map(|(i, (&val, &label))| {
                    let val = val.to_string();
                    CheckmarkItem {
                        label: label.into(),
                        checked: self.output == i,
                        activate: Box::new(move |tray: &mut Self| {
                            tray.output = i;
                            let _ = tray.cmd_tx.try_send(TrayCommand::SetOutput(val.clone()));
                        }),
                        ..Default::default()
                    }
                    .into()
                })
                .collect(),
            ..Default::default()
        };

        let lang_menu = SubMenu {
            label: "Language".into(),
            submenu: self
                .langs
                .iter()
                .enumerate()
                .map(|(i, &(code, name))| {
                    let code = code.to_string();
                    CheckmarkItem {
                        label: name.into(),
                        checked: self.lang == i,
                        activate: Box::new(move |tray: &mut Self| {
                            tray.lang = i;
                            let _ = tray.cmd_tx.try_send(TrayCommand::SetLang(code.clone()));
                        }),
                        ..Default::default()
                    }
                    .into()
                })
                .collect(),
            ..Default::default()
        };

        let model_menu = SubMenu {
            label: "Model".into(),
            submenu: config::ALL_MODELS
                .iter()
                .enumerate()
                .map(|(i, &m)| {
                    let val = m.to_string();
                    CheckmarkItem {
                        label: m.into(),
                        checked: self.model == i,
                        activate: Box::new(move |tray: &mut Self| {
                            tray.model = i;
                            let _ = tray.cmd_tx.try_send(TrayCommand::SetModel(val.clone()));
                        }),
                        ..Default::default()
                    }
                    .into()
                })
                .collect(),
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

        vec![
            mode_menu.into(),
            output_menu.into(),
            lang_menu.into(),
            model_menu.into(),
            MenuItem::Separator,
            enter_item.into(),
        ]
    }
}

pub async fn spawn(
    cmd_tx: mpsc::Sender<TrayCommand>,
    state: &config::State,
) -> anyhow::Result<ksni::Handle<DictateTray>> {
    let langs = sorted_languages();
    let tray = DictateTray {
        recording: false,
        mode: MODES.iter().position(|&m| m == state.mode).unwrap_or(0),
        output: OUTPUTS.iter().position(|&o| o == state.output).unwrap_or(0),
        lang: langs.iter().position(|(c, _)| *c == state.lang).unwrap_or(0),
        model: config::ALL_MODELS.iter().position(|&m| m == state.model).unwrap_or(0),
        enter: state.enter,
        langs,
        cmd_tx,
    };
    let handle = tray.spawn().await?;
    Ok(handle)
}
