use crate::config::EnhanceConfig;
use anyhow::{anyhow, Context};
use std::time::Instant;

const ENDPOINT: &str = "https://api.groq.com/openai/v1/chat/completions";

const SYSTEM_PROMPT: &str = "You rewrite dictated, spoken-style text into a clear, \
well-structured prompt for a large language model. Preserve the meaning and every \
concrete detail exactly — links, names, numbers, file paths, quoted text. Fix \
filler words, false starts, and rambling structure. Do not add new requirements, \
opinions, or pleasantries. Output ONLY the rewritten prompt, with no preamble or \
explanation.";

/// Rewrite a transcript as a better LLM prompt via Groq's chat API.
/// Uses GROQ_API_KEY from the environment (env only, never config).
pub async fn enhance(cfg: &EnhanceConfig, text: &str) -> anyhow::Result<String> {
    let api_key = std::env::var("GROQ_API_KEY")
        .map_err(|_| anyhow!("GROQ_API_KEY is not set (Enhance uses Groq's LLM)"))?;
    // ~/.config/bolo/enhance_prompt.txt lets the user standardize the shape
    // of enhanced prompts; the built-in prompt is the fallback.
    let custom = crate::userdata::enhance_prompt();
    let system_prompt = custom.as_deref().unwrap_or(SYSTEM_PROMPT);
    let t0 = Instant::now();
    let response = reqwest::Client::new()
        .post(ENDPOINT)
        .bearer_auth(&api_key)
        .json(&serde_json::json!({
            "model": cfg.model,
            "temperature": 0.2,
            "messages": [
                { "role": "system", "content": system_prompt },
                { "role": "user", "content": text },
            ],
        }))
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(anyhow!("groq {status}: enhance failed\nraw body: {body}"));
    }
    let json: serde_json::Value =
        serde_json::from_str(&body).context("groq returned non-JSON body")?;
    let enhanced = json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| anyhow!("no message content in groq response: {body}"))?
        .trim()
        .to_string();
    eprintln!(
        "[enhance] model={} prompt={} latency_ms={} in_chars={} out_chars={}",
        cfg.model,
        if custom.is_some() { "custom" } else { "default" },
        t0.elapsed().as_millis(),
        text.chars().count(),
        enhanced.chars().count()
    );
    if enhanced.is_empty() {
        return Err(anyhow!("enhance returned empty text"));
    }
    Ok(enhanced)
}
