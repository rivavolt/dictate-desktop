use anyhow::Result;
use std::path::Path;

use crate::config;

pub async fn transcribe_file(path: &Path, lang: &str, model: &str) -> Result<String> {
    let api_key = config::get_api_key("groq")?;

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
        .post("https://api.groq.com/openai/v1/audio/transcriptions")
        .header("Authorization", format!("Bearer {api_key}"))
        .multipart(form)
        .send()
        .await?
        .text()
        .await?;

    let json: serde_json::Value = serde_json::from_str(&resp)?;
    if let Some(err) = json.get("error") {
        anyhow::bail!("Groq: {}", err);
    }
    let text = json["text"].as_str().unwrap_or("").to_string();
    Ok(text)
}
