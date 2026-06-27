use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

pub const PROVIDERS: &[&str] = &["assemblyai", "deepgram", "groq", "fireworks"];

pub const AUTO_LANG: &str = "auto";

/// Language coverage of a model in one mode, matched to what our request code actually
/// sends (verified against each provider's 2026 docs — see the per-model notes below).
#[derive(Clone, Copy, PartialEq)]
pub enum LangCoverage {
    English,                      // English only
    Set(&'static [&'static str]), // code-switch / auto-detect among these codes
    All,                          // ~99 languages, auto-detect, any mix
}

impl LangCoverage {
    /// Whether this coverage can handle every required language together ("auto" is a
    /// wildcard — it just means "detect", which any coverage can do for its own set).
    pub fn covers(&self, required: &[&str]) -> bool {
        match self {
            LangCoverage::All => true,
            LangCoverage::English => required.iter().all(|l| *l == "en" || *l == AUTO_LANG),
            LangCoverage::Set(s) => required.iter().all(|l| *l == AUTO_LANG || s.contains(l)),
        }
    }
}

pub struct ModelCaps {
    pub live: Option<LangCoverage>,  // None = no streaming
    pub batch: Option<LangCoverage>, // None = no file/batch
    pub rank: u32,                   // fallback preference, lower = preferred
}

impl ModelCaps {
    pub fn coverage(&self, mode: &str) -> Option<LangCoverage> {
        if mode == "live" { self.live } else { self.batch }
    }
}

/// True if the model handles only English in every mode it supports.
pub fn is_english_only(caps: &ModelCaps) -> bool {
    let eng = |c: &Option<LangCoverage>| matches!(c, None | Some(LangCoverage::English));
    (caps.live.is_some() || caps.batch.is_some()) && eng(&caps.live) && eng(&caps.batch)
}

// AssemblyAI u3-rt-pro streaming code-switch set.
const AAI_EUR6: &[&str] = &["en", "es", "de", "fr", "pt", "it"];
// Deepgram nova multilingual (`language=multi`) set — what our deepgram path sends for auto.
const DG_MULTI10: &[&str] = &["en", "es", "fr", "de", "hi", "ru", "pt", "ja", "it", "nl"];

pub fn model_caps(provider: &str, model: &str) -> ModelCaps {
    use LangCoverage::{All, English, Set};
    match (provider, model) {
        // u3-rt-pro streams 6 European langs; batch (Universal-2) detects all 99 with expected_languages.
        ("assemblyai", _) => ModelCaps { live: Some(Set(AAI_EUR6)), batch: Some(All), rank: 0 },
        // nova-* stream/batch via language=multi (10 langs); ro reachable only as a forced single language.
        ("deepgram", "nova-3") | ("deepgram", "nova-2") | ("deepgram", "nova-2-general") => {
            ModelCaps { live: Some(Set(DG_MULTI10)), batch: Some(Set(DG_MULTI10)), rank: 10 }
        }
        // Deepgram-hosted Whisper: batch only.
        ("deepgram", _) => ModelCaps { live: None, batch: Some(Set(DG_MULTI10)), rank: 40 },
        // Fireworks streaming ASR: live + batch, full 99-language auto-detect.
        ("fireworks", "fireworks-asr-large") => ModelCaps { live: Some(All), batch: Some(All), rank: 15 },
        ("fireworks", _) => ModelCaps { live: None, batch: Some(All), rank: 30 },
        // Groq Whisper: batch only; distil is English-only.
        ("groq", "distil-whisper-large-v3-en") => ModelCaps { live: None, batch: Some(English), rank: 50 },
        ("groq", _) => ModelCaps { live: None, batch: Some(All), rank: 25 },
        _ => ModelCaps { live: Some(All), batch: Some(All), rank: 100 },
    }
}

/// Smart fallback: if `current` can't satisfy the mode + required languages, return the
/// best compatible model id — preferring the same provider, then overall rank. None means
/// the current pick is fine. `required` is the preferred-language set (for auto) or the
/// single chosen language.
pub fn resolve_model(current: &str, mode: &str, required: &[&str]) -> Option<String> {
    let compatible = |id: &str| {
        let (p, m) = parse_provider_model(id);
        model_caps(p, m).coverage(mode).is_some_and(|c| c.covers(required))
    };
    if compatible(current) {
        return None;
    }
    let cur_provider = parse_provider_model(current).0;
    ALL_MODELS
        .iter()
        .filter(|id| compatible(id))
        .min_by_key(|id| {
            let (p, m) = parse_provider_model(id);
            (u32::from(p != cur_provider), model_caps(p, m).rank)
        })
        .map(|s| s.to_string())
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
        "assemblyai" => &["universal"],
        "deepgram" => &["nova-3", "nova-2", "nova-2-general"],
        "groq" => &["whisper-large-v3-turbo", "whisper-large-v3"],
        "fireworks" => &["fireworks-asr-large"],
        _ => &[],
    }
}

pub const ALL_MODELS: &[&str] = &[
    "assemblyai/universal",
    "deepgram/nova-3", "deepgram/nova-2", "deepgram/nova-2-general",
    "groq/whisper-large-v3-turbo", "groq/whisper-large-v3",
    "fireworks/fireworks-asr-large",
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
    /// Nix-managed read-only defaults (defaults.toml), layered over config.toml in `load`.
    pub defaults_file: PathBuf,
}

impl Config {
    pub fn new() -> Self {
        let runtime_dir = PathBuf::from(
            std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string()),
        );
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".config"))
            .join("dictate-desktop");
        let state_dir = dirs::state_dir()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/state"))
            .join("dictate-desktop");
        let _ = fs::create_dir_all(&config_dir);
        let _ = fs::create_dir_all(&state_dir);

        Self {
            config_file: config_dir.join("config.toml"),
            state_file: runtime_dir.join("dictate-desktop.state"),
            transcript_file: runtime_dir.join("dictate-desktop.transcript"),
            audio_file: runtime_dir.join("dictate-desktop.wav"),
            history_file: state_dir.join("history.log"),
            audio_dir: state_dir.join("audio"),
            socket_path: runtime_dir.join("dictate-desktop.sock"),
            defaults_file: config_dir.join("defaults.toml"),
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
        "assemblyai" => "ASSEMBLYAI_API_KEY",
        "deepgram" => "DEEPGRAM_API_KEY",
        "groq" => "GROQ_API_KEY",
        "fireworks" => "FIREWORKS_API_KEY",
        other => bail!("unknown provider: {other}"),
    };

    std::env::var(env_var).context(format!("{env_var} not set"))
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OverlayMode {
    /// No overlay.
    Off,
    /// Pill/status animation only (recording waveform, processing) — no text panel.
    Status,
    /// Pill that expands into the live transcript panel.
    Full,
}

impl OverlayMode {
    pub fn name(self) -> &'static str {
        match self {
            OverlayMode::Off => "off",
            OverlayMode::Status => "status",
            OverlayMode::Full => "full",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct State {
    #[serde(default = "default_lang")]
    pub lang: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_delivery")]
    pub delivery: String,
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
    #[serde(default = "default_languages")]
    pub languages: Vec<String>,
    /// Overlay mode override; None follows the output mode (see `overlay_mode`).
    #[serde(default)]
    pub overlay: Option<OverlayMode>,
    /// Custom vocabulary / keyterms — biases recognition toward these names/jargon. Passed as
    /// keyterms_prompt (AssemblyAI) / keyterm (Deepgram Nova-3) / keywords (Nova-2) / prompt
    /// (Groq, Fireworks — soft hint only). Fixes acoustic mis-recognition the LLM pass can't.
    #[serde(default)]
    pub vocabulary: Vec<String>,
    /// Remove filler words (um/uh) at the provider level — useful when the LLM pass is off.
    /// Maps to AssemblyAI `disfluencies=false` / Deepgram `filler_words=false`.
    #[serde(default = "default_true")]
    pub remove_fillers: bool,
    /// Superseded by `delivery`; deserialized only to migrate pre-`delivery` configs in `load`,
    /// and never written back (skip_serializing).
    #[serde(default, skip_serializing)]
    output: Option<String>,
    #[serde(default, skip_serializing)]
    auto_paste: Option<bool>,
}

fn default_lang() -> String { std::env::var("DICTATE_LANG").unwrap_or_else(|_| AUTO_LANG.to_string()) }
fn default_model() -> String { std::env::var("DICTATE_MODEL").unwrap_or_else(|_| "groq/whisper-large-v3-turbo".to_string()) }
fn default_mode() -> String { "live".to_string() }
fn default_delivery() -> String { std::env::var("DICTATE_DELIVERY").unwrap_or_else(|_| "auto".to_string()) }
fn default_true() -> bool { true }
fn default_correct_hold_ms() -> u64 { 3000 }
fn default_font() -> String { std::env::var("DICTATE_FONT").unwrap_or_else(|_| "Inter".to_string()) }
fn default_languages() -> Vec<String> {
    std::env::var("DICTATE_LANGUAGES")
        .ok()
        .map(|s| s.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect::<Vec<String>>())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec!["en".to_string(), "fr".to_string(), "ro".to_string()])
}

impl State {
    pub fn load(config: &Config) -> Self {
        // Two layers: the Nix-managed defaults.toml is overlaid OVER the runtime config.toml, so a
        // Home-Manager-forced key (mkForce) re-asserts on every start, while config.toml (written by
        // tray/CLI changes) owns every key Nix did not set.
        let user_raw = fs::read_to_string(&config.config_file).unwrap_or_default();
        let defaults_raw = fs::read_to_string(&config.defaults_file).unwrap_or_default();
        let mut merged: toml::Table = toml::from_str(&user_raw).unwrap_or_default();
        if let Ok(defaults) = toml::from_str::<toml::Table>(&defaults_raw) {
            merged.extend(defaults);
        }
        let has_delivery = merged.contains_key("delivery");
        let merged_str = toml::to_string(&merged).unwrap_or_default();
        let mut state: State = toml::from_str(&merged_str).unwrap_or_else(|_| toml::from_str("").unwrap());
        if state.lang == "multi" {
            state.lang = AUTO_LANG.to_string();
        }
        // Migrate pre-`delivery` configs: old `output` + `auto_paste` collapse into `delivery`, but
        // only when neither layer set a `delivery` key (a config already on the new model is left be).
        if !has_delivery {
            state.delivery = match (state.output.as_deref(), state.auto_paste) {
                (Some("clipboard"), _) => "clipboard",
                (_, Some(false)) => "type",
                _ => "auto",
            }
            .to_string();
        }
        state
    }

    pub fn save(&self, config: &Config) {
        if let Ok(s) = toml::to_string_pretty(self) {
            let _ = fs::write(&config.config_file, s);
        }
    }

    /// Resolved overlay mode: explicit override, else the delivery-mode default — a status-only
    /// pill for the injecting modes (auto/type land text in the focused app, so no panel), the
    /// full text panel for clipboard (where the panel is the only view of the transcript).
    pub fn overlay_mode(&self) -> OverlayMode {
        self.overlay
            .unwrap_or(if self.delivery == "clipboard" { OverlayMode::Full } else { OverlayMode::Status })
    }
}
