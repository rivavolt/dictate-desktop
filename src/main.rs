mod assemblyai;
mod audio;
mod config;
mod correct;
mod daemon;
mod deepgram;
mod fireworks;
mod fnkey;
mod groq;
mod inputmethod;
mod ipc;
mod keyinject;
mod output;
mod overlay;
mod sound;
mod transcript;
mod tray;
mod vad;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

#[derive(Parser)]
#[command(name = "dictate-desktop", about = "Voice-to-text dictation daemon")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the daemon (IPC server + recording)
    Daemon,
    /// Toggle recording on/off
    Toggle,
    /// Start recording
    Start,
    /// Stop recording
    Stop,
    /// Show current status
    Status,
    /// Set or show transcription mode (live, batch, vad)
    Mode {
        #[arg(value_parser = ["live", "batch", "vad"])]
        mode: Option<String>,
    },
    /// Set or show language
    Lang {
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(config::LANGUAGES.iter().map(|(c, _)| *c)))]
        lang: Option<String>,
    },
    /// Set or show output method (type, clipboard) — legacy alias for `delivery`
    Output {
        #[arg(value_parser = ["type", "clipboard"])]
        output: Option<String>,
    },
    /// Set or show delivery mode (auto, type, clipboard). auto = type via input-method else
    /// paste-chord; type = input-method only (else clipboard); clipboard = never inject, accumulate
    Delivery {
        #[arg(value_parser = ["auto", "type", "clipboard"])]
        mode: Option<String>,
    },
    /// Toggle or set enter-after-type (on, off). Presses Enter after typing in type mode
    Enter {
        #[arg(value_parser = ["on", "off"])]
        state: Option<String>,
    },
    /// Toggle or set LLM correction (on, off). Post-processes transcription with Haiku 4.5
    Correct {
        #[arg(value_parser = ["on", "off"])]
        state: Option<String>,
    },
    /// Set or show correction hold time in milliseconds
    CorrectHold { ms: Option<String> },
    /// Set or show overlay font (e.g. "Inter", "JetBrains Mono")
    Font { font: Option<String> },
    /// Set or show audio input device
    Input { device: Option<String> },
    /// Set or show model (provider/model). Providers: assemblyai, deepgram, groq, fireworks
    Model {
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(config::ALL_MODELS))]
        model: Option<String>,
    },
    /// Set or show preferred languages (candidate set for auto-detect; passed to APIs that support it, e.g. AssemblyAI batch)
    Languages {
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(config::LANGUAGES.iter().filter(|(c, _)| *c != config::AUTO_LANG).map(|(c, _)| *c)))]
        langs: Vec<String>,
    },
    /// Set or show the overlay (off, status, full). status = recording animation only, no text panel
    Overlay {
        #[arg(value_parser = ["off", "status", "full"])]
        mode: Option<String>,
    },
    /// Manage custom vocabulary (keyterm boosting): improves recognition of names/jargon/terms
    Vocab {
        #[command(subcommand)]
        action: Option<VocabAction>,
    },
    /// Toggle or set provider-side filler-word (um/uh) removal (on, off)
    Fillers {
        #[arg(value_parser = ["on", "off"])]
        state: Option<String>,
    },
    /// Toggle or set auto-paste fallback (on, off). For apps without Wayland input-method
    /// support (kitty/TUIs, XWayland): on = paste via Ctrl+Shift+V, off = copy + toast only
    Paste {
        #[arg(value_parser = ["on", "off"])]
        state: Option<String>,
    },
    /// Show recent transcription history (most recent last)
    History {
        /// How many recent entries to show (default 20)
        count: Option<usize>,
    },
    /// Generate shell completions
    Completions { shell: Shell },
}

#[derive(Subcommand)]
enum VocabAction {
    /// Add one or more terms (quote multi-word terms)
    Add { terms: Vec<String> },
    /// Remove one or more terms
    Remove { terms: Vec<String> },
    /// List current vocabulary
    List,
    /// Remove all terms
    Clear,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Commands::Toggle) {
        Commands::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "dictate-desktop", &mut std::io::stdout());
            Ok(())
        }
        Commands::Daemon => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::from_default_env()
                        .add_directive("dictate_desktop=info".parse().unwrap()),
                )
                .init();
            daemon::run().await
        }
        Commands::History { count } => {
            // Read the structured history directly (no daemon round-trip needed).
            let config = config::Config::new();
            let path = config.history_file.with_file_name("history.jsonl");
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            let lines: Vec<&str> = content.lines().collect();
            let n = count.unwrap_or(20).min(lines.len());
            for line in &lines[lines.len() - n..] {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    let ts = v.get("ts").and_then(|x| x.as_str()).unwrap_or("");
                    let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("");
                    let ts = ts.split('.').next().unwrap_or(ts).replace('T', " ");
                    println!("{ts}  {text}");
                }
            }
            Ok(())
        }
        cmd => {
            let config = config::Config::new();
            let req = match cmd {
                Commands::Toggle => ipc::Request {
                    command: "toggle".into(),
                    arg: None,
                },
                Commands::Start => ipc::Request {
                    command: "start".into(),
                    arg: None,
                },
                Commands::Stop => ipc::Request {
                    command: "stop".into(),
                    arg: None,
                },
                Commands::Status => ipc::Request {
                    command: "status".into(),
                    arg: None,
                },
                Commands::Mode { mode } => ipc::Request {
                    command: "mode".into(),
                    arg: mode,
                },
                Commands::Lang { lang } => ipc::Request {
                    command: "lang".into(),
                    arg: lang,
                },
                Commands::Output { output } => ipc::Request {
                    command: "output".into(),
                    arg: output,
                },
                Commands::Delivery { mode } => ipc::Request {
                    command: "delivery".into(),
                    arg: mode,
                },
                Commands::Enter { state } => ipc::Request {
                    command: "enter".into(),
                    arg: state,
                },
                Commands::Correct { state } => ipc::Request {
                    command: "correct".into(),
                    arg: state,
                },
                Commands::CorrectHold { ms } => ipc::Request {
                    command: "correct-hold".into(),
                    arg: ms,
                },
                Commands::Font { font } => ipc::Request {
                    command: "font".into(),
                    arg: font,
                },
                Commands::Input { device } => ipc::Request {
                    command: "input".into(),
                    arg: device,
                },
                Commands::Model { model } => ipc::Request {
                    command: "model".into(),
                    arg: model,
                },
                Commands::Languages { langs } => ipc::Request {
                    command: "languages".into(),
                    arg: if langs.is_empty() { None } else { Some(langs.join(",")) },
                },
                Commands::Overlay { mode } => ipc::Request {
                    command: "overlay".into(),
                    arg: mode,
                },
                Commands::Vocab { action } => ipc::Request {
                    command: "vocab".into(),
                    // Encode action + terms newline-separated so multi-word terms survive.
                    arg: Some(match action {
                        Some(VocabAction::Add { terms }) => format!("add\n{}", terms.join("\n")),
                        Some(VocabAction::Remove { terms }) => format!("remove\n{}", terms.join("\n")),
                        Some(VocabAction::Clear) => "clear".into(),
                        Some(VocabAction::List) | None => "list".into(),
                    }),
                },
                Commands::Fillers { state } => ipc::Request {
                    command: "fillers".into(),
                    arg: state,
                },
                Commands::Paste { state } => ipc::Request {
                    command: "paste".into(),
                    arg: state,
                },
                Commands::Daemon | Commands::Completions { .. } | Commands::History { .. } => unreachable!(),
            };

            let resp = ipc::send(&config.socket_path, &req).await?;
            if let Some(msg) = resp.message {
                println!("{}", msg);
            }
            if !resp.ok {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}
