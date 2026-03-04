use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::config;
use crate::output;
use crate::overlay;

#[derive(Serialize, Deserialize)]
struct LiveResponse {
    #[serde(rename = "type")]
    message_type: Option<String>,
    channel: Option<LiveChannel>,
    is_final: Option<bool>,
}

#[derive(Serialize, Deserialize)]
struct LiveChannel {
    alternatives: Vec<LiveAlternative>,
}

#[derive(Serialize, Deserialize)]
struct LiveAlternative {
    transcript: String,
}

pub async fn stream_live(
    state: &config::State,
    mut audio_rx: mpsc::Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
    sample_rate: u32,
    overlay: overlay::Handle,
) -> Result<()> {
    let api_key = config::get_api_key()?;

    let params = format!(
        "model={}&language={}&encoding=linear16&sample_rate={}&channels=1&smart_format=true&interim_results=true&endpointing=300",
        state.model, state.lang, sample_rate
    );
    let ws_url = format!("wss://api.deepgram.com/v1/listen?{}", params);

    let request = tokio_tungstenite::tungstenite::http::Request::builder()
        .method("GET")
        .uri(&ws_url)
        .header("Host", "api.deepgram.com")
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("Sec-WebSocket-Version", "13")
        .header("Authorization", format!("Token {}", api_key))
        .body(())?;

    tracing::debug!("connecting to Deepgram (model: {}, lang: {})", state.model, state.lang);
    let (ws_stream, _) = connect_async(request)
        .await
        .context("WebSocket connect failed")?;
    tracing::debug!("Deepgram WebSocket connected");
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    let stop_sender = stop.clone();
    let sender_task = tokio::spawn(async move {
        while !stop_sender.load(Ordering::Relaxed) {
            match tokio::time::timeout(Duration::from_millis(50), audio_rx.recv()).await {
                Ok(Some(data)) if !data.is_empty() => {
                    if ws_tx.send(Message::Binary(data)).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                _ => {}
            }
        }
    });

    let cfg = config::Config::new();
    let transcript_file = cfg.transcript_file.clone();
    let mut full_transcript = String::new();
    let output_mode = state.output.clone();
    let overlay_handle = overlay.clone();

    let receiver_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_rx.next().await {
            let Message::Text(text) = msg else {
                continue;
            };
            let Ok(resp) = serde_json::from_str::<LiveResponse>(&text) else {
                continue;
            };
            if resp.message_type.as_deref() != Some("Results") {
                continue;
            }
            let Some(alt) = resp
                .channel
                .and_then(|c| c.alternatives.into_iter().next())
            else {
                continue;
            };
            if alt.transcript.is_empty() {
                continue;
            }

            if resp.is_final.unwrap_or(false) {
                tracing::info!("transcript: {}", alt.transcript);
                full_transcript.push_str(&alt.transcript);
                full_transcript.push(' ');
                if output_mode == "clipboard" {
                    output::copy_to_clipboard(&full_transcript);
                    overlay_handle.set_text(full_transcript.clone());
                } else {
                    output::type_text(&alt.transcript);
                    output::copy_to_clipboard(&full_transcript);
                }
                let _ = std::fs::write(&transcript_file, &full_transcript);
            } else if output_mode == "clipboard" {
                overlay_handle.set_pending(alt.transcript.clone());
            }
        }
    });

    // Wait for stop signal
    while !stop.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        let _ = sender_task.await;
        let _ = receiver_task.await;
    })
    .await;

    Ok(())
}

pub async fn transcribe_file(path: &std::path::Path, lang: &str, model: &str) -> Result<String> {
    let api_key = config::get_api_key()?;
    let url = format!(
        "https://api.deepgram.com/v1/listen?model={}&language={}&detect_language=true&smart_format=true",
        model, lang
    );

    let audio_data = tokio::fs::read(path).await?;
    let client = reqwest::Client::new();
    let response = client
        .post(&url)
        .header("Authorization", format!("Token {}", api_key))
        .header("Content-Type", "audio/wav")
        .body(audio_data)
        .send()
        .await?
        .text()
        .await?;

    let json: serde_json::Value = serde_json::from_str(&response)?;
    let transcript = json["results"]["channels"][0]["alternatives"][0]["transcript"]
        .as_str()
        .unwrap_or("")
        .to_string();

    Ok(transcript)
}
