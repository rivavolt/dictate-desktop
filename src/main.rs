mod audio;
mod config;
mod daemon;
mod deepgram;
mod fnkey;
mod ipc;
mod output;
mod overlay;
mod sound;
mod tray;

use anyhow::Result;
use clap::{Parser, Subcommand};

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
    Mode { mode: Option<String> },
    /// Set or show language
    Lang { lang: Option<String> },
    /// Set or show output method (type, clipboard)
    Output { output: Option<String> },
    /// Set or show overlay font (e.g. "Inter", "JetBrains Mono")
    Font { font: Option<String> },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Commands::Toggle) {
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
                Commands::Font { font } => ipc::Request {
                    command: "font".into(),
                    arg: font,
                },
                Commands::Daemon => unreachable!(),
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
