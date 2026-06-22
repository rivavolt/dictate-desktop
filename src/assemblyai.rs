use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::config;
use crate::transcript::TranscriptEvent;

// Universal-Streaming wants 16 kHz mono PCM; we resample the 48 kHz capture down before sending.
const STREAM_RATE: u32 = 16000;
const STREAM_MODEL: &str = "u3-rt-pro";
// Per-message audio bounds: send at ~100 ms, never flush a tail under the 50 ms floor.
const MIN_SEND_BYTES: usize = STREAM_RATE as usize * 2 / 10;
const FLUSH_MIN_BYTES: usize = STREAM_RATE as usize * 2 / 20;

#[derive(Deserialize)]
struct TurnMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default)]
    transcript: String,
    #[serde(default)]
    end_of_turn: bool,
    #[serde(default)]
    turn_is_formatted: bool,
    #[serde(default)]
    error: Option<String>,
}

pub async fn stream_live(
    state: &config::State,
    mut audio_rx: mpsc::Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
    sample_rate: u32,
    tx: mpsc::UnboundedSender<TranscriptEvent>,
) -> Result<()> {
    let api_key = config::get_api_key("assemblyai")?;
    let (_, app_model) = config::parse_provider_model(&state.model);
    // whisper-rt streams ~99 languages (auto-detect, incl. Romanian); u3-rt-pro is the
    // higher-accuracy default for the 6 European languages it covers.
    let speech_model = if app_model == "whisper-rt" { "whisper-rt" } else { STREAM_MODEL };
    let params = format!(
        "sample_rate={STREAM_RATE}&encoding=pcm_s16le&speech_model={speech_model}&format_turns=true"
    );
    let ws_url = format!("wss://streaming.assemblyai.com/v3/ws?{params}");

    let request = tokio_tungstenite::tungstenite::http::Request::builder()
        .method("GET")
        .uri(&ws_url)
        .header("Host", "streaming.assemblyai.com")
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("Sec-WebSocket-Version", "13")
        .header("Authorization", &api_key)
        .body(())?;

    tracing::debug!("connecting to AssemblyAI (model: {speech_model})");
    let (ws_stream, _) = tokio::time::timeout(Duration::from_secs(10), connect_async(request))
        .await
        .context("AssemblyAI WebSocket connect timed out")?
        .context("AssemblyAI WebSocket connect failed")?;
    tracing::debug!("AssemblyAI WebSocket connected");
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    let stop_sender = stop.clone();
    let sender_task = tokio::spawn(async move {
        // Universal-Streaming requires each audio message to carry 50–1000 ms; the 48 kHz
        // capture arrives in ~42 ms callbacks, so accumulate to ~100 ms before sending.
        let mut buf: Vec<u8> = Vec::with_capacity(MIN_SEND_BYTES * 2);
        while !stop_sender.load(Ordering::Relaxed) {
            match tokio::time::timeout(Duration::from_millis(50), audio_rx.recv()).await {
                Ok(Some(data)) if !data.is_empty() => {
                    buf.extend_from_slice(&resample_pcm(&data, sample_rate, STREAM_RATE));
                    if buf.len() >= MIN_SEND_BYTES
                        && ws_tx.send(Message::Binary(std::mem::take(&mut buf))).await.is_err()
                    {
                        break;
                    }
                }
                Ok(None) => break,
                _ => {}
            }
        }
        // Flush a trailing chunk only if it still clears the 50 ms floor.
        if buf.len() >= FLUSH_MIN_BYTES {
            let _ = ws_tx.send(Message::Binary(buf)).await;
        }
        // Ask AssemblyAI to finalize the in-flight turn and close the session.
        let _ = ws_tx
            .send(Message::Text(r#"{"type":"Terminate"}"#.into()))
            .await;
    });

    let receiver_task = tokio::spawn(async move {
        let mut finalized = String::new();
        loop {
            let msg = match ws_rx.next().await {
                Some(Ok(msg)) => msg,
                _ => break,
            };
            let Message::Text(text) = msg else {
                continue;
            };
            let Ok(m) = serde_json::from_str::<TurnMessage>(&text) else {
                continue;
            };
            if let Some(err) = &m.error {
                tracing::error!("assemblyai error: {err}");
                break;
            }
            match m.msg_type.as_str() {
                "Turn" => {
                    if m.transcript.is_empty() {
                        continue;
                    }
                    // With format_turns=true, each turn first arrives unformatted
                    // (treated as interim) and again formatted on end_of_turn — only the
                    // formatted version is typed, so we never double-type a turn.
                    if m.end_of_turn && m.turn_is_formatted {
                        let needs_space =
                            !finalized.is_empty() && !m.transcript.starts_with(char::is_whitespace);
                        let delta = if needs_space {
                            format!(" {}", m.transcript)
                        } else {
                            m.transcript.clone()
                        };
                        finalized.push_str(&delta);
                        let _ = tx.send(TranscriptEvent::Final {
                            delta,
                            accumulated: finalized.clone(),
                        });
                    } else {
                        let _ = tx.send(TranscriptEvent::Interim(m.transcript));
                    }
                }
                "Termination" => break,
                _ => {}
            }
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

fn resample_pcm(data: &[u8], from_rate: u32, to_rate: u32) -> Vec<u8> {
    if from_rate == to_rate {
        return data.to_vec();
    }
    let samples: Vec<i16> = data
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    crate::audio::resample(&samples, from_rate, to_rate)
        .iter()
        .flat_map(|s| s.to_le_bytes())
        .collect()
}

pub async fn transcribe_file(path: &Path, lang: &str, expected: &[String], _model: &str) -> Result<String> {
    let api_key = config::get_api_key("assemblyai")?;
    let client = config::http_client();

    // AssemblyAI has no synchronous file endpoint: upload, submit a job, then poll.
    let audio_data = tokio::fs::read(path).await?;
    let upload: Value = client
        .post("https://api.assemblyai.com/v2/upload")
        .header("Authorization", &api_key)
        .header("Content-Type", "application/octet-stream")
        .body(audio_data)
        .send()
        .await?
        .json()
        .await?;
    let upload_url = upload["upload_url"]
        .as_str()
        .context("AssemblyAI: no upload_url in response")?;

    let mut body = json!({ "audio_url": upload_url });
    if lang == config::AUTO_LANG {
        body["language_detection"] = json!(true);
        // Restrict detection to the user's preferred languages, so a few-language speaker
        // isn't matched against all 99 (and mis-detected). fallback=auto picks the highest
        // confidence among them when detection is uncertain.
        if !expected.is_empty() {
            body["language_detection_options"] = json!({
                "expected_languages": expected,
                "fallback_language": "auto",
            });
        }
    } else {
        body["language_code"] = json!(lang);
    }

    let submit: Value = client
        .post("https://api.assemblyai.com/v2/transcript")
        .header("Authorization", &api_key)
        .json(&body)
        .send()
        .await?
        .json()
        .await?;
    if let Some(err) = submit.get("error").and_then(|e| e.as_str()) {
        bail!("AssemblyAI: {err}");
    }
    let id = submit["id"]
        .as_str()
        .context("AssemblyAI: no transcript id in response")?;
    let poll_url = format!("https://api.assemblyai.com/v2/transcript/{id}");

    for _ in 0..120 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let t: Value = client
            .get(&poll_url)
            .header("Authorization", &api_key)
            .send()
            .await?
            .json()
            .await?;
        match t["status"].as_str() {
            Some("completed") => return Ok(t["text"].as_str().unwrap_or("").to_string()),
            Some("error") => bail!(
                "AssemblyAI: {}",
                t.get("error").and_then(|e| e.as_str()).unwrap_or("unknown")
            ),
            _ => {}
        }
    }
    bail!("AssemblyAI: transcription timed out")
}
