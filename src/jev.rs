//! Jev model decision client via OpenRouter alpha decisions API.
//!
//! Provides ultra-fast (sub-400ms) predictive decision-making for:
//! - Determining if text is source code vs prose (`is_code` noul)
//! - Classifying programming language (`language` choice)
//! - Determining semantic layout layering (`layout` choice: code block, bullet list, task list, multi-paragraph)

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const DECISIONS_ENDPOINT: &str = "https://openrouter.ai/api/alpha/decisions";
pub const DEFAULT_MODEL: &str = "typesafe/jev-1.13";
pub const DEFAULT_TIMEOUT_MS: u64 = 400;

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

/// Builds the JSON request payload for OpenRouter's decisions endpoint.
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

/// Parses the JSON response from OpenRouter decisions API into a JevFormattingDecision.
pub fn parse_decision_response(val: &serde_json::Value) -> Result<JevFormattingDecision> {
    if let Some(err) = val.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown OpenRouter error");
        anyhow::bail!("OpenRouter error: {msg}");
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

/// Core implementation of decide_formatting calling the OpenRouter endpoint.
pub async fn decide_formatting_with_model(
    text: &str,
    frontmost_app: Option<&str>,
    api_key: &str,
    timeout_ms: u64,
    model: &str,
) -> Result<JevFormattingDecision> {
    let fut = async {
        let client = reqwest::Client::new();
        let payload = build_decision_request(text, frontmost_app, model);
        let resp = client
            .post(DECISIONS_ENDPOINT)
            .bearer_auth(api_key)
            .json(&payload)
            .send()
            .await
            .context("failed to send request to OpenRouter decisions endpoint")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_body = resp.text().await.unwrap_or_default();
            anyhow::bail!("OpenRouter decisions endpoint returned {status}: {err_body}");
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .context("failed to parse OpenRouter decisions response as JSON")?;

        parse_decision_response(&body)
    };

    tokio::time::timeout(Duration::from_millis(timeout_ms), fut)
        .await
        .map_err(|_| anyhow::anyhow!("Jev decision timed out after {timeout_ms}ms"))?
}

/// Makes a predictive formatting decision via Jev on OpenRouter with async timeout.
pub async fn decide_formatting(
    text: &str,
    frontmost_app: Option<&str>,
    api_key: &str,
    timeout_ms: u64,
) -> Result<JevFormattingDecision> {
    decide_formatting_with_model(text, frontmost_app, api_key, timeout_ms, DEFAULT_MODEL).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_decision_request_structure() {
        let req = build_decision_request(
            "fn main() { println!(\"hello\"); }",
            Some("Visual Studio Code"),
            DEFAULT_MODEL,
        );

        assert_eq!(req["model"], DEFAULT_MODEL);
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

    #[tokio::test]
    async fn test_decide_formatting_timeout() {
        // Calling decide_formatting with an unreachable port and very short timeout should time out
        let result = decide_formatting_with_model(
            "test text",
            None,
            "dummy_key",
            1, // 1 ms timeout
            DEFAULT_MODEL,
        )
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_decide_formatting_wrapper() {
        let result =
            decide_formatting("let x = 10;", None, "invalid_key", DEFAULT_TIMEOUT_MS).await;
        assert!(result.is_err());
    }
}
