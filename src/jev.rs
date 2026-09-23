//! Jev client. Calls TypeSafe's System One API directly by default, or
//! OpenRouter's alpha decisions API when configured. `format.rs` builds the
//! questions; this module only sends one request and returns its answers.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const TYPESAFE_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
pub const OPENROUTER_ENDPOINT: &str = "https://openrouter.ai/api/alpha/decisions";
/// TypeSafe answers in about 1.2s (median over 26 live calls, max 1.3s), so
/// 400ms timed out every call. Calls only happen for pieces the local check
/// cannot settle, so a generous budget costs little.
pub const DEFAULT_TIMEOUT_MS: u64 = 2500;

/// Which API serves Jev. The two use different model ids and API keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JevProvider {
    /// TypeSafe System One API (`TYPESAFE_API_KEY`); requests show on the TypeSafe dashboard.
    #[default]
    TypeSafe,
    /// OpenRouter decisions API (`OPENROUTER_API_KEY`, `sk-or-...`).
    OpenRouter,
}

impl JevProvider {
    pub fn endpoint(self) -> &'static str {
        match self {
            JevProvider::TypeSafe => TYPESAFE_ENDPOINT,
            JevProvider::OpenRouter => OPENROUTER_ENDPOINT,
        }
    }

    /// Model id this provider accepts; TypeSafe rejects OpenRouter's `typesafe/` ids.
    pub fn default_model(self) -> &'static str {
        match self {
            JevProvider::TypeSafe => "jev-latest",
            JevProvider::OpenRouter => "typesafe/jev-1.13",
        }
    }

    /// Environment variable (also read from `~/.env`) holding this provider's key.
    pub fn api_key_env(self) -> &'static str {
        match self {
            JevProvider::TypeSafe => "TYPESAFE_API_KEY",
            JevProvider::OpenRouter => "OPENROUTER_API_KEY",
        }
    }

    /// Infers the provider from a key's shape: OpenRouter keys start with `sk-or-`.
    pub fn for_key(key: &str) -> Self {
        if key.trim().starts_with("sk-or-") {
            JevProvider::OpenRouter
        } else {
            JevProvider::TypeSafe
        }
    }
}

/// Pulls the `answers` object out of a decisions response, turning provider
/// error bodies into errors.
pub fn answers_from_response(val: &serde_json::Value) -> Result<serde_json::Value> {
    if let Some(err) = val.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown Jev error");
        anyhow::bail!("Jev error: {msg}");
    }
    if let Some(detail) = val.get("detail") {
        let msg = detail
            .get("message")
            .and_then(|m| m.as_str())
            .or_else(|| detail.as_str())
            .unwrap_or("unknown TypeSafe error");
        anyhow::bail!("TypeSafe error: {msg}");
    }
    val.get("answers")
        .or_else(|| val.get("decisions"))
        .cloned()
        .context("Jev response has no answers")
}

/// Sends one decisions request and returns its answers, within `timeout_ms`.
pub async fn evaluate(
    request: &serde_json::Value,
    api_key: &str,
    timeout_ms: u64,
    provider: JevProvider,
) -> Result<serde_json::Value> {
    let fut = async {
        let resp = reqwest::Client::new()
            .post(provider.endpoint())
            .bearer_auth(api_key)
            .json(request)
            .send()
            .await
            .with_context(|| format!("failed to send Jev request to {}", provider.endpoint()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Jev endpoint {} returned {status}: {err_body}",
                provider.endpoint()
            );
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .context("failed to parse Jev decisions response as JSON")?;
        answers_from_response(&body)
    };

    tokio::time::timeout(Duration::from_millis(timeout_ms), fut)
        .await
        .map_err(|_| anyhow::anyhow!("Jev decision timed out after {timeout_ms}ms"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_extracted() {
        let val = serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {"code_0": {"type": "noul", "noul": 0.98}}
        });
        let answers = answers_from_response(&val).unwrap();
        assert_eq!(answers["code_0"]["noul"], 0.98);
    }

    #[test]
    fn provider_errors_become_errors() {
        let typesafe = serde_json::json!({
            "detail": {"error_type": "api_usage_error", "message": "Unknown model: typesafe/jev-1.13"}
        });
        assert!(answers_from_response(&typesafe)
            .unwrap_err()
            .to_string()
            .contains("Unknown model"));
        let openrouter = serde_json::json!({"error": {"message": "Model is overloaded"}});
        assert!(answers_from_response(&openrouter)
            .unwrap_err()
            .to_string()
            .contains("overloaded"));
        assert!(answers_from_response(&serde_json::json!({})).is_err());
    }

    #[test]
    fn provider_endpoints_and_models() {
        assert_eq!(JevProvider::default(), JevProvider::TypeSafe);
        assert_eq!(JevProvider::TypeSafe.endpoint(), TYPESAFE_ENDPOINT);
        assert_eq!(JevProvider::TypeSafe.default_model(), "jev-latest");
        assert_eq!(JevProvider::OpenRouter.endpoint(), OPENROUTER_ENDPOINT);
        assert_eq!(JevProvider::OpenRouter.default_model(), "typesafe/jev-1.13");
    }

    #[test]
    fn provider_for_key() {
        assert_eq!(
            JevProvider::for_key("sk-or-v1-abc"),
            JevProvider::OpenRouter
        );
        assert_eq!(JevProvider::for_key("apikey-abc"), JevProvider::TypeSafe);
    }

    #[tokio::test]
    async fn evaluate_times_out() {
        // A 1 ms timeout cannot complete a network round trip.
        let req = serde_json::json!({"model": "jev-latest", "state": "x", "questions": {}});
        let result = evaluate(&req, "dummy_key", 1, JevProvider::TypeSafe).await;
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }
}
