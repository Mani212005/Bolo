//! Jev model decision client. Calls TypeSafe's System One API directly by
//! default, or OpenRouter's alpha decisions API when configured.
//!
//! Provides ultra-fast (sub-400ms) predictive decision-making for:
//! - Determining if text is source code vs prose (`is_code` noul)
//! - Classifying programming language (`language` choice)
//! - Determining semantic layout layering (`layout` choice: code block, bullet list, task list, multi-paragraph)

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const TYPESAFE_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
pub const OPENROUTER_ENDPOINT: &str = "https://openrouter.ai/api/alpha/decisions";
pub const DEFAULT_TIMEOUT_MS: u64 = 400;

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JevFormattingDecision {
    /// True if the text represents source code, SQL, or shell commands
    pub is_code: bool,
    /// Probability score from the noul decision (0.0 to 1.0)
    pub code_probability: f64,
    /// Detected language: rust, python, javascript, typescript, sql, bash, html_css, json, c_cpp, other
    pub language: String,
    /// Detected layout: code_block, bullet_list, task_list, multi_paragraph, single_block
    pub layout: String,
}

/// Builds the JSON request payload; both providers accept the same shape.
pub fn build_decision_request(
    text: &str,
    frontmost_app: Option<&str>,
    model: &str,
) -> serde_json::Value {
    let mut state = serde_json::Map::new();
    state.insert(
        "text".to_string(),
        serde_json::Value::String(text.to_string()),
    );
    if let Some(app) = frontmost_app {
        state.insert(
            "frontmost_app".to_string(),
            serde_json::Value::String(app.to_string()),
        );
    }

    serde_json::json!({
        "model": model,
        "state": state,
        "questions": {
            "is_code": {
                "type": "noul",
                "instructions": "Probability that the text is source code, SQL query, or shell commands."
            },
            "language": {
                "type": "choice",
                "instructions": "Detect programming language of the snippet.",
                "criteria": {
                    "rust": "Rust programming language",
                    "python": "Python programming language",
                    "javascript": "JavaScript language",
                    "typescript": "TypeScript language",
                    "sql": "SQL database query",
                    "bash": "Bash or shell command script",
                    "html_css": "HTML or CSS code",
                    "json": "JSON structured data",
                    "c_cpp": "C or C++ programming language",
                    "other": "Other programming language or natural language text"
                }
            },
            "layout": {
                "type": "choice",
                "instructions": "Layout category for formatting the output text.",
                "criteria": {
                    "code_block": "Code block or terminal command sequence",
                    "bullet_list": "Unordered list of items or bullet points",
                    "task_list": "Checklist or todo task items",
                    "multi_paragraph": "Multiple paragraphs of prose or descriptive text",
                    "single_block": "Single short sentence or block of text"
                }
            }
        }
    })
}

/// Parses a decisions response from either provider into a JevFormattingDecision.
pub fn parse_decision_response(val: &serde_json::Value) -> Result<JevFormattingDecision> {
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

    let answers = val
        .get("answers")
        .or_else(|| val.get("decisions"))
        .unwrap_or(val);

    // 1. Parse is_code noul
    let code_prob = match answers.get("is_code") {
        Some(v) if v.is_number() => v.as_f64().unwrap_or(0.0),
        Some(v) if v.is_boolean() => {
            if v.as_bool().unwrap_or(false) {
                1.0
            } else {
                0.0
            }
        }
        Some(v) => v
            .get("noul")
            .or_else(|| v.get("value"))
            .or_else(|| v.get("probability"))
            .and_then(|p| p.as_f64())
            .unwrap_or(0.0),
        None => 0.0,
    };

    // 2. Parse language choice
    let language = match answers.get("language") {
        Some(v) if v.is_string() => v.as_str().unwrap_or("other").to_string(),
        Some(v) => v
            .get("choice")
            .or_else(|| v.get("value"))
            .and_then(|s| s.as_str())
            .unwrap_or("other")
            .to_string(),
        None => "other".to_string(),
    };

    // 3. Parse layout choice
    let layout = match answers.get("layout") {
        Some(v) if v.is_string() => v.as_str().unwrap_or("single_block").to_string(),
        Some(v) => v
            .get("choice")
            .or_else(|| v.get("value"))
            .and_then(|s| s.as_str())
            .unwrap_or("single_block")
            .to_string(),
        None => "single_block".to_string(),
    };

    let is_code = code_prob >= 0.5 || layout == "code_block";

    Ok(JevFormattingDecision {
        is_code,
        code_probability: code_prob,
        language: language.to_lowercase(),
        layout: layout.to_lowercase(),
    })
}

/// Makes a predictive formatting decision via Jev with an async timeout.
pub async fn decide_formatting(
    text: &str,
    frontmost_app: Option<&str>,
    api_key: &str,
    timeout_ms: u64,
    provider: JevProvider,
    model: &str,
) -> Result<JevFormattingDecision> {
    let fut = async {
        let client = reqwest::Client::new();
        let payload = build_decision_request(text, frontmost_app, model);
        let resp = client
            .post(provider.endpoint())
            .bearer_auth(api_key)
            .json(&payload)
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

        parse_decision_response(&body)
    };

    tokio::time::timeout(Duration::from_millis(timeout_ms), fut)
        .await
        .map_err(|_| anyhow::anyhow!("Jev decision timed out after {timeout_ms}ms"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_decision_request_structure() {
        let req = build_decision_request(
            "fn main() { println!(\"hello\"); }",
            Some("Visual Studio Code"),
            "jev-latest",
        );

        assert_eq!(req["model"], "jev-latest");
        assert_eq!(req["state"]["text"], "fn main() { println!(\"hello\"); }");
        assert_eq!(req["state"]["frontmost_app"], "Visual Studio Code");
        assert_eq!(req["questions"]["is_code"]["type"], "noul");
        assert_eq!(req["questions"]["language"]["type"], "choice");
        assert_eq!(req["questions"]["layout"]["type"], "choice");
        assert!(req["questions"]["language"]["criteria"]["rust"].is_string());
    }

    #[test]
    fn test_parse_decision_response_standard() {
        let json_str = r#"{
            "answers": {
                "is_code": {
                    "type": "noul",
                    "noul": 0.96
                },
                "language": {
                    "type": "choice",
                    "choice": "rust",
                    "confidence": 0.92
                },
                "layout": {
                    "type": "choice",
                    "choice": "code_block",
                    "confidence": 0.88
                }
            }
        }"#;

        let val: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let dec = parse_decision_response(&val).unwrap();

        assert!(dec.is_code);
        assert!((dec.code_probability - 0.96).abs() < 1e-6);
        assert_eq!(dec.language, "rust");
        assert_eq!(dec.layout, "code_block");
    }

    #[test]
    fn test_parse_decision_response_prose_list() {
        let json_str = r#"{
            "answers": {
                "is_code": {
                    "type": "noul",
                    "noul": 0.05
                },
                "language": {
                    "type": "choice",
                    "choice": "other"
                },
                "layout": {
                    "type": "choice",
                    "choice": "bullet_list"
                }
            }
        }"#;

        let val: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let dec = parse_decision_response(&val).unwrap();

        assert!(!dec.is_code);
        assert_eq!(dec.language, "other");
        assert_eq!(dec.layout, "bullet_list");
    }

    #[test]
    fn test_parse_decision_response_alternative_shape() {
        let json_str = r#"{
            "decisions": {
                "is_code": 0.82,
                "language": "python",
                "layout": "code_block"
            }
        }"#;

        let val: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let dec = parse_decision_response(&val).unwrap();

        assert!(dec.is_code);
        assert_eq!(dec.language, "python");
        assert_eq!(dec.layout, "code_block");
    }

    #[test]
    fn test_parse_decision_response_error() {
        let json_str = r#"{
            "error": {
                "message": "Model typesafe/jev-1.13 is overloaded"
            }
        }"#;

        let val: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let err = parse_decision_response(&val).unwrap_err();
        assert!(err.to_string().contains("overloaded"));
    }

    #[test]
    fn test_parse_decision_response_typesafe_error() {
        let val = serde_json::json!({
            "detail": {"error_type": "api_usage_error", "message": "Unknown model: typesafe/jev-1.13"}
        });
        let err = parse_decision_response(&val).unwrap_err();
        assert!(err.to_string().contains("Unknown model"));
    }

    #[test]
    fn test_provider_endpoints_and_models() {
        assert_eq!(JevProvider::default(), JevProvider::TypeSafe);
        assert_eq!(JevProvider::TypeSafe.endpoint(), TYPESAFE_ENDPOINT);
        assert_eq!(JevProvider::TypeSafe.default_model(), "jev-latest");
        assert_eq!(JevProvider::OpenRouter.endpoint(), OPENROUTER_ENDPOINT);
        assert_eq!(JevProvider::OpenRouter.default_model(), "typesafe/jev-1.13");
    }

    #[test]
    fn test_provider_for_key() {
        assert_eq!(
            JevProvider::for_key("sk-or-v1-abc"),
            JevProvider::OpenRouter
        );
        assert_eq!(JevProvider::for_key("apikey-abc"), JevProvider::TypeSafe);
    }

    #[tokio::test]
    async fn test_decide_formatting_timeout() {
        // A 1 ms timeout cannot complete a network round trip.
        let result = decide_formatting(
            "test text",
            None,
            "dummy_key",
            1,
            JevProvider::TypeSafe,
            "jev-latest",
        )
        .await;

        assert!(result.is_err());
    }
}
