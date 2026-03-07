mod audio;
mod config;
mod correct;
mod daemon;
mod deepgram;
mod fireworks;
mod fnkey;
mod groq;
mod ipc;
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
#[command(name = "dictate", about = "Voice-to-text dictation daemon")]
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
    /// Set or show output method (type, clipboard)
    Output {
        #[arg(value_parser = ["type", "clipboard"])]
        output: Option<String>,
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
    /// Set or show model (provider/model). Providers: deepgram, groq, fireworks
    Model {
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(config::ALL_MODELS))]
        model: Option<String>,
    },
    /// Generate shell completions
    Completions { shell: Shell },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Commands::Toggle) {
        Commands::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "dictate", &mut std::io::stdout());
            Ok(())
        }
        Commands::Daemon => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::from_default_env()
                        .add_directive("dictate=info".parse().unwrap()),
                )
                .init();
            daemon::run().await
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
                Commands::Model { model } => ipc::Request {
                    command: "model".into(),
                    arg: model,
                },
                Commands::Daemon | Commands::Completions { .. } => unreachable!(),
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
