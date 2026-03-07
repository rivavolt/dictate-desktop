use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::config;
use crate::transcript::TranscriptEvent;

#[derive(Deserialize)]
struct StreamResponse {
    words: Option<Vec<Word>>,
    checkpoint_id: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct Word {
    word: String,
    is_final: bool,
}

pub async fn stream_live(
    state: &config::State,
    mut audio_rx: mpsc::Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
    sample_rate: u32,
    tx: mpsc::UnboundedSender<TranscriptEvent>,
) -> Result<()> {
    let api_key = config::get_api_key("fireworks")?;
    let (_, model) = config::parse_provider_model(&state.model);

    let mut params = vec![
        format!("model={model}"),
        format!("sample_rate={sample_rate}"),
        format!("encoding=pcm_s16le"),
    ];
    if !state.lang.is_empty() && state.lang != config::AUTO_LANG {
        params.push(format!("language={}", state.lang));
    }
    let query = params.join("&");
    let ws_url = format!(
        "wss://audio-streaming.api.fireworks.ai/v1/audio/transcriptions/streaming?{query}"
    );

    let request = tokio_tungstenite::tungstenite::http::Request::builder()
        .method("GET")
        .uri(&ws_url)
        .header("Host", "audio-streaming.api.fireworks.ai")
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("Sec-WebSocket-Version", "13")
        .header("Authorization", &api_key)
        .body(())?;

    tracing::debug!("connecting to Fireworks (model: {model}, lang: {})", state.lang);
    let (ws_stream, _) = connect_async(request)
        .await
        .context("Fireworks WebSocket connect failed")?;
    tracing::debug!("Fireworks WebSocket connected");
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
        let _ = ws_tx
            .send(Message::Text(r#"{"checkpoint_id":"final"}"#.into()))
            .await;
    });

    let receiver_task = tokio::spawn(async move {
        let mut prev_final_text = String::new();
        let mut last_pending = String::new();

        loop {
            let msg = match ws_rx.next().await {
                Some(Ok(msg)) => msg,
                _ => break,
            };
            let Message::Text(text) = msg else {
                continue;
            };
            let Ok(resp) = serde_json::from_str::<StreamResponse>(&text) else {
                continue;
            };

            if let Some(err) = resp.error {
                tracing::error!("fireworks error: {err}");
                break;
            }

            let is_final_checkpoint = resp.checkpoint_id.as_deref() == Some("final");

            let Some(words) = resp.words else {
                if is_final_checkpoint { break; }
                continue;
            };

            let mut final_words = String::new();
            let mut pending_words = String::new();
            for w in &words {
                if w.is_final {
                    if !final_words.is_empty() {
                        final_words.push(' ');
                    }
                    final_words.push_str(&w.word);
                } else {
                    if !pending_words.is_empty() {
                        pending_words.push(' ');
                    }
                    pending_words.push_str(&w.word);
                }
            }

            if is_final_checkpoint && !pending_words.is_empty() {
                if !final_words.is_empty() { final_words.push(' '); }
                final_words.push_str(&pending_words);
                pending_words.clear();
            }

            if final_words.len() > prev_final_text.len() {
                let new_text = final_words[prev_final_text.len()..].trim();
                if !new_text.is_empty() {
                    let _ = tx.send(TranscriptEvent::Final {
                        delta: new_text.to_string(),
                        accumulated: final_words.clone(),
                    });
                }
                prev_final_text = final_words;
            }

            if !pending_words.is_empty() {
                let _ = tx.send(TranscriptEvent::Interim(pending_words.clone()));
            }
            last_pending = pending_words;

            if is_final_checkpoint { break; }
        }

        if !last_pending.is_empty() {
            if !prev_final_text.is_empty() { prev_final_text.push(' '); }
            prev_final_text.push_str(&last_pending);
            let _ = tx.send(TranscriptEvent::Final {
                delta: last_pending,
                accumulated: prev_final_text,
            });
        }
    });

    while !stop.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let sender_abort = sender_task.abort_handle();
    let receiver_abort = receiver_task.abort_handle();
    if tokio::time::timeout(Duration::from_secs(3), async {
        let _ = sender_task.await;
        let _ = receiver_task.await;
    })
    .await
    .is_err()
    {
        sender_abort.abort();
        receiver_abort.abort();
    }

    Ok(())
}

pub async fn transcribe_file(path: &Path, lang: &str, model: &str) -> Result<String> {
    let api_key = config::get_api_key("fireworks")?;

    let file_bytes = tokio::fs::read(path).await?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("audio.wav")
        .to_string();

    let file_part = reqwest::multipart::Part::bytes(file_bytes)
        .file_name(file_name)
        .mime_str(crate::audio::audio_mime(path))?;

    let mut form = reqwest::multipart::Form::new()
        .part("file", file_part)
        .text("model", model.to_string())
        .text("response_format", "json");

    if !lang.is_empty() && lang != config::AUTO_LANG {
        form = form.text("language", lang.to_string());
    }

    let resp = config::http_client()
        .post("https://audio-prod.api.fireworks.ai/v1/audio/transcriptions")
        .header("Authorization", format!("Bearer {api_key}"))
        .multipart(form)
        .send()
        .await?
        .text()
        .await?;

    let json: serde_json::Value = serde_json::from_str(&resp)?;
    if let Some(err) = json.get("error") {
        anyhow::bail!("Fireworks: {}", err);
    }
    let text = json["text"].as_str().unwrap_or("").to_string();
    Ok(text)
}
