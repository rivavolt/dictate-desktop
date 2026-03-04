use anyhow::{bail, Context, Result};
use std::fs;
use std::path::PathBuf;

pub const PROVIDERS: &[&str] = &["deepgram", "groq", "fireworks"];

pub const AUTO_LANG: &str = "auto";

#[derive(Clone, Copy, PartialEq)]
pub enum LangSupport {
    All,
    EnglishOnly,
}

pub struct ModelCaps {
    pub live: bool,
    pub batch: bool,
    pub lang: LangSupport,
}

pub fn model_caps(provider: &str, model: &str) -> ModelCaps {
    match (provider, model) {
        ("groq", _) => ModelCaps {
            live: false,
            batch: true,
            lang: if model == "distil-whisper-large-v3-en" {
                LangSupport::EnglishOnly
            } else {
                LangSupport::All
            },
        },
        ("fireworks", "fireworks-asr-large") => ModelCaps {
            live: true,
            batch: true,
            lang: LangSupport::All,
        },
        ("fireworks", _) => ModelCaps {
            live: false,
            batch: true,
            lang: LangSupport::All,
        },
        _ => ModelCaps {
            live: true,
            batch: true,
            lang: LangSupport::All,
        },
    }
}

/// Find best compatible model given mode/lang constraints. Returns None if current model is fine.
pub fn resolve_model(current: &str, mode: &str, lang: &str) -> Option<String> {
    let (provider, model) = parse_provider_model(current);

    let needs_live = mode == "live";
    let needs_multilang = lang != "en" && lang != "auto";

    let compatible = |p: &str, m: &str| -> bool {
        let c = model_caps(p, m);
        (!needs_live || c.live) && (!needs_multilang || c.lang != LangSupport::EnglishOnly)
    };

    if compatible(provider, model) {
        return None;
    }

    // same provider first
    for m in provider_models(provider) {
        if compatible(provider, m) {
            return Some(format!("{provider}/{m}"));
        }
    }

    Some("deepgram/nova-3".to_string())
}

pub fn provider_models(provider: &str) -> &'static [&'static str] {
    match provider {
        "deepgram" => &["nova-3", "nova-2", "nova-2-general", "whisper-large", "whisper-medium", "whisper-small", "whisper-tiny"],
        "groq" => &["whisper-large-v3-turbo", "whisper-large-v3", "distil-whisper-large-v3-en"],
        "fireworks" => &["fireworks-asr-large", "whisper-v3-turbo", "whisper-v3"],
        _ => &[],
    }
}

pub fn all_models() -> Vec<String> {
    PROVIDERS.iter().flat_map(|p| {
        provider_models(p).iter().map(move |m| format!("{p}/{m}"))
    }).collect()
}

pub struct Config {
    pub state_file: PathBuf,
    pub transcript_file: PathBuf,
    pub audio_file: PathBuf,
    pub lang_file: PathBuf,
    pub mode_file: PathBuf,
    pub output_file: PathBuf,
    pub font_file: PathBuf,
    pub model_file: PathBuf,
    pub socket_path: PathBuf,
}

impl Config {
    pub fn new() -> Self {
        let runtime_dir = PathBuf::from(
            std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string()),
        );
        let state_dir = dirs::state_dir()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/state"))
            .join("dictate");
        let _ = fs::create_dir_all(&state_dir);

        Self {
            state_file: runtime_dir.join("dictate.state"),
            transcript_file: runtime_dir.join("dictate.transcript"),
            audio_file: runtime_dir.join("dictate.wav"),
            lang_file: state_dir.join("language"),
            mode_file: state_dir.join("mode"),
            output_file: state_dir.join("output"),
            font_file: state_dir.join("font"),
            model_file: state_dir.join("model"),
            socket_path: runtime_dir.join("dictate.sock"),
        }
    }
}

/// Split "provider/model" into (provider, model). Returns ("deepgram", full) if no slash.
pub fn parse_provider_model(s: &str) -> (&str, &str) {
    match s.split_once('/') {
        Some((provider, model)) => (provider, model),
        None => ("deepgram", s),
    }
}

pub fn get_api_key(provider: &str) -> Result<String> {
    let env_var = match provider {
        "deepgram" => "DEEPGRAM_API_KEY",
        "groq" => "GROQ_API_KEY",
        "fireworks" => "FIREWORKS_API_KEY",
        other => bail!("unknown provider: {other}"),
    };

    std::env::var(env_var).context(format!("{env_var} not set"))
}

#[derive(Clone)]
pub struct State {
    pub lang: String,
    pub model: String,
    pub mode: String,
    pub output: String,
    pub font: String,
}

impl State {
    pub fn load(config: &Config) -> Self {
        let mut lang = fs::read_to_string(&config.lang_file)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| {
                std::env::var("DICTATE_LANG").unwrap_or_else(|_| AUTO_LANG.to_string())
            });
        if lang == "multi" {
            lang = AUTO_LANG.to_string();
        }

        let model = fs::read_to_string(&config.model_file)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| {
                std::env::var("DICTATE_MODEL").unwrap_or_else(|_| "deepgram/nova-3".to_string())
            });

        let mode = fs::read_to_string(&config.mode_file)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "live".to_string());

        let output = fs::read_to_string(&config.output_file)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "type".to_string());

        let font = fs::read_to_string(&config.font_file)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| {
                std::env::var("DICTATE_FONT").unwrap_or_else(|_| "Inter".to_string())
            });

        Self { lang, model, mode, output, font }
    }
}
