use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
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

pub const LANGUAGES: &[(&str, &str)] = &[
    ("auto", "Auto-detect"),
    ("en", "English"),
    ("zh", "Chinese"),
    ("es", "Spanish"),
    ("hi", "Hindi"),
    ("pt", "Portuguese"),
    ("fr", "French"),
    ("de", "German"),
    ("ja", "Japanese"),
    ("ko", "Korean"),
    ("it", "Italian"),
    ("nl", "Dutch"),
    ("pl", "Polish"),
    ("ro", "Romanian"),
    ("ru", "Russian"),
    ("sv", "Swedish"),
    ("tr", "Turkish"),
    ("uk", "Ukrainian"),
    ("ar", "Arabic"),
    ("cs", "Czech"),
    ("da", "Danish"),
    ("el", "Greek"),
    ("fi", "Finnish"),
    ("hu", "Hungarian"),
    ("id", "Indonesian"),
    ("no", "Norwegian"),
    ("th", "Thai"),
    ("vi", "Vietnamese"),
];

pub fn lang_name(code: &str) -> Option<&'static str> {
    LANGUAGES.iter().find(|(c, _)| *c == code).map(|(_, name)| *name)
}

pub fn provider_models(provider: &str) -> &'static [&'static str] {
    match provider {
        "deepgram" => &["nova-3", "nova-2", "nova-2-general", "whisper-large", "whisper-medium", "whisper-small", "whisper-tiny"],
        "groq" => &["whisper-large-v3-turbo", "whisper-large-v3", "distil-whisper-large-v3-en"],
        "fireworks" => &["fireworks-asr-large", "whisper-v3-turbo", "whisper-v3"],
        _ => &[],
    }
}

pub const ALL_MODELS: &[&str] = &[
    "deepgram/nova-3", "deepgram/nova-2", "deepgram/nova-2-general",
    "deepgram/whisper-large", "deepgram/whisper-medium", "deepgram/whisper-small", "deepgram/whisper-tiny",
    "groq/whisper-large-v3-turbo", "groq/whisper-large-v3", "groq/distil-whisper-large-v3-en",
    "fireworks/fireworks-asr-large", "fireworks/whisper-v3-turbo", "fireworks/whisper-v3",
];

pub fn all_models() -> Vec<String> {
    ALL_MODELS.iter().map(|s| s.to_string()).collect()
}

pub struct Config {
    pub config_file: PathBuf,
    pub state_file: PathBuf,
    pub transcript_file: PathBuf,
    pub audio_file: PathBuf,
    pub history_file: PathBuf,
    pub audio_dir: PathBuf,
    pub socket_path: PathBuf,
}

impl Config {
    pub fn new() -> Self {
        let runtime_dir = PathBuf::from(
            std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string()),
        );
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".config"))
            .join("dictate");
        let state_dir = dirs::state_dir()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/state"))
            .join("dictate");
        let _ = fs::create_dir_all(&config_dir);
        let _ = fs::create_dir_all(&state_dir);

        Self {
            config_file: config_dir.join("config.toml"),
            state_file: runtime_dir.join("dictate.state"),
            transcript_file: runtime_dir.join("dictate.transcript"),
            audio_file: runtime_dir.join("dictate.wav"),
            history_file: state_dir.join("history.log"),
            audio_dir: state_dir.join("audio"),
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

pub fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        let mut builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30));
        if let Ok(proxy_url) = std::env::var("DICTATE_PROXY") {
            if let Ok(proxy) = reqwest::Proxy::all(&proxy_url) {
                tracing::info!("using proxy {proxy_url}");
                builder = builder.proxy(proxy);
            }
        }
        builder.build().expect("http client")
    })
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

#[derive(Clone, Serialize, Deserialize)]
pub struct State {
    #[serde(default = "default_lang")]
    pub lang: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_output")]
    pub output: String,
    #[serde(default)]
    pub enter: bool,
    #[serde(default = "default_true")]
    pub correct: bool,
    #[serde(default = "default_correct_hold_ms")]
    pub correct_hold_ms: u64,
    #[serde(default = "default_font")]
    pub font: String,
    #[serde(default)]
    pub input: String,
}

fn default_lang() -> String { std::env::var("DICTATE_LANG").unwrap_or_else(|_| AUTO_LANG.to_string()) }
fn default_model() -> String { std::env::var("DICTATE_MODEL").unwrap_or_else(|_| "deepgram/nova-3".to_string()) }
fn default_mode() -> String { "live".to_string() }
fn default_output() -> String { "type".to_string() }
fn default_true() -> bool { true }
fn default_correct_hold_ms() -> u64 { 3000 }
fn default_font() -> String { std::env::var("DICTATE_FONT").unwrap_or_else(|_| "Inter".to_string()) }

impl State {
    pub fn load(config: &Config) -> Self {
        let mut state: State = fs::read_to_string(&config.config_file)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_else(|| toml::from_str("").unwrap());
        if state.lang == "multi" {
            state.lang = AUTO_LANG.to_string();
        }
        state
    }

    pub fn save(&self, config: &Config) {
        if let Ok(s) = toml::to_string_pretty(self) {
            let _ = fs::write(&config.config_file, s);
        }
    }
}
