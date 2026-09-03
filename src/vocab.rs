use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeveloperAppProfile {
    General,
    Terminal,
    Editor,
    Ai,
}

impl DeveloperAppProfile {
    pub fn infer(bundle_identifier: Option<&str>, application_name: Option<&str>) -> Self {
        let identifier = bundle_identifier.map(|s| s.to_lowercase());
        let name = application_name.map(|s| s.to_lowercase());

        let terminal_names = ["terminal", "iterm2", "ghostty", "warp", "kitty", "wezterm"];
        let terminal_ids = [
            "com.apple.terminal",
            "com.googlecode.iterm2",
            "com.mitchellh.ghostty",
        ];

        let editor_names = ["xcode", "visual studio code", "cursor", "windsurf", "neovim"];
        let editor_ids = ["com.apple.dt.xcode", "com.microsoft.vscode"];

        let ai_names = ["chatgpt", "claude", "codex"];

        if name.as_ref().map_or(false, |n| terminal_names.iter().any(|t| n.contains(t)))
            || identifier.as_ref().map_or(false, |id| terminal_ids.contains(&id.as_str()))
        {
            return DeveloperAppProfile::Terminal;
        }

        if name.as_ref().map_or(false, |n| editor_names.iter().any(|e| n.contains(e)))
            || identifier.as_ref().map_or(false, |id| editor_ids.contains(&id.as_str()))
        {
            return DeveloperAppProfile::Editor;
        }

        if name.as_ref().map_or(false, |n| ai_names.iter().any(|a| n.contains(a))) {
            return DeveloperAppProfile::Ai;
        }

        DeveloperAppProfile::General
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveApp {
    pub name: Option<String>,
    pub bundle_id: Option<String>,
}

pub fn detect_frontmost_app() -> Option<ActiveApp> {
    #[cfg(target_os = "macos")]
    {
        let applescript = r#"tell application "System Events" to set frontApp to first application process whose frontmost is true
tell application "System Events" to get {name of frontApp, bundle identifier of frontApp}"#;
        let output = std::process::Command::new("osascript")
            .arg("-e")
            .arg(applescript)
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if stdout.is_empty() {
            return None;
        }

        let parts: Vec<&str> = stdout.split(", ").collect();
        let name = parts.first().map(|s| s.to_string());
        let bundle_id = parts.get(1).map(|s| s.to_string());

        Some(ActiveApp { name, bundle_id })
    }

    #[cfg(target_os = "linux")]
    {
        // 1. Try hyprctl for Hyprland
        if let Ok(output) = std::process::Command::new("hyprctl")
            .args(&["activewindow", "-j"])
            .output()
        {
            if output.status.success() {
                if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                    let class_name = val.get("class").and_then(|v| v.as_str()).map(|s| s.to_string());
                    let title = val.get("title").and_then(|v| v.as_str()).map(|s| s.to_string());
                    if class_name.is_some() || title.is_some() {
                        return Some(ActiveApp {
                            name: title,
                            bundle_id: class_name,
                        });
                    }
                }
            }
        }

        // 2. Try swaymsg for Sway
        if let Ok(output) = std::process::Command::new("swaymsg")
            .args(&["-t", "get_tree"])
            .output()
        {
            if output.status.success() {
                if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                    fn find_focused(node: &serde_json::Value) -> Option<ActiveApp> {
                        if node.get("focused").and_then(|v| v.as_bool()) == Some(true) {
                            let name = node.get("name").and_then(|v| v.as_str()).map(|s| s.to_string());
                            let app_id = node.get("app_id").and_then(|v| v.as_str()).map(|s| s.to_string());
                            return Some(ActiveApp { name, bundle_id: app_id });
                        }
                        if let Some(nodes) = node.get("nodes").and_then(|v| v.as_array()) {
                            for child in nodes {
                                if let Some(found) = find_focused(child) {
                                    return Some(found);
                                }
                            }
                        }
                        None
                    }
                    if let Some(found) = find_focused(&val) {
                        return Some(found);
                    }
                }
            }
        }

        // 3. Fallback: xdotool for X11 / XWayland
        if let Ok(output) = std::process::Command::new("xdotool")
            .args(&["getactivewindow", "getwindowclassname"])
            .output()
        {
            if output.status.success() {
                let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !name.is_empty() {
                    return Some(ActiveApp {
                        name: Some(name.clone()),
                        bundle_id: Some(name),
                    });
                }
            }
        }

        None
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

pub static SPOKEN_ACRONYMS: &[(&str, &str)] = &[
    ("n p m", "npm"),
    ("n p x", "npx"),
    ("g i t h u b", "GitHub"),
    ("j s o n", "JSON"),
    ("a p i", "API"),
    ("c l i", "CLI"),
    ("s d k", "SDK"),
];

pub static DEV_TERMS: &[(&str, &str)] = &[
    ("javascript", "JavaScript"),
    ("typescript", "TypeScript"),
    ("swiftui", "SwiftUI"),
    ("nextjs", "Next.js"),
    ("next.js", "Next.js"),
    ("postgresql", "PostgreSQL"),
    ("postgres", "Postgres"),
    ("mongodb", "MongoDB"),
    ("supabase", "Supabase"),
    ("graphql", "GraphQL"),
    ("github", "GitHub"),
    ("gitlab", "GitLab"),
    ("bitbucket", "Bitbucket"),
    ("macos", "macOS"),
    ("ios", "iOS"),
    ("ipados", "iPadOS"),
    ("watchos", "watchOS"),
    ("xcode", "Xcode"),
    ("appkit", "AppKit"),
    ("coregraphics", "CoreGraphics"),
    ("avfoundation", "AVFoundation"),
    ("openai", "OpenAI"),
    ("chatgpt", "ChatGPT"),
    ("pytorch", "PyTorch"),
    ("tensorflow", "TensorFlow"),
    ("onnx", "ONNX"),
    ("parakeet", "Parakeet"),
    ("whisper", "Whisper"),
    ("fluid audio", "FluidAudio"),
    ("api", "API"),
    ("sdk", "SDK"),
    ("cli", "CLI"),
    ("ide", "IDE"),
    ("orm", "ORM"),
    ("cdn", "CDN"),
    ("dns", "DNS"),
    ("ssl", "SSL"),
    ("tls", "TLS"),
    ("ssh", "SSH"),
    ("html", "HTML"),
    ("css", "CSS"),
    ("xml", "XML"),
    ("sql", "SQL"),
    ("jwt", "JWT"),
    ("csv", "CSV"),
    ("pdf", "PDF"),
    ("svg", "SVG"),
    ("png", "PNG"),
    ("json", "JSON"),
    ("yaml", "YAML"),
    ("toml", "TOML"),
    ("uuid", "UUID"),
    ("http", "HTTP"),
    ("https", "HTTPS"),
    ("cors", "CORS"),
    ("crud", "CRUD"),
    ("rest", "REST"),
    ("grpc", "gRPC"),
    ("tcp", "TCP"),
    ("udp", "UDP"),
    ("vpn", "VPN"),
    ("cpu", "CPU"),
    ("gpu", "GPU"),
    ("npm", "npm"),
    ("npx", "npx"),
    ("aws", "AWS"),
    ("gcp", "GCP"),
    ("ec2", "EC2"),
    ("s3", "S3"),
    ("llm", "LLM"),
    ("gpt", "GPT"),
    ("rag", "RAG"),
    ("nlp", "NLP"),
    ("mps", "MPS"),
    ("ai", "AI"),
];

pub static AMBIGUOUS_TERMS: &[&str] = &["rest", "rag", "crud", "whisper", "parakeet", "ai"];

fn is_word_boundary_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.' || c == '/' || c == '-'
}

pub fn replace_whole_phrase(source: &str, replacement: &str, text: &str) -> String {
    if source.is_empty() || text.is_empty() {
        return text.to_string();
    }

    let pattern = format!("(?i){}", regex::escape(source));
    let Ok(re) = regex::Regex::new(&pattern) else {
        return text.to_string();
    };

    let mut result = String::with_capacity(text.len());
    let mut last_end = 0;

    for mat in re.find_iter(text) {
        let start = mat.start();
        let end = mat.end();

        // Check character before start
        let prev_char = text[..start].chars().next_back();
        if let Some(c) = prev_char {
            if is_word_boundary_char(c) {
                continue;
            }
        }

        // Check character after end
        let next_char = text[end..].chars().next();
        if let Some(c) = next_char {
            if is_word_boundary_char(c) {
                continue;
            }
        }

        result.push_str(&text[last_end..start]);
        result.push_str(replacement);
        last_end = end;
    }

    result.push_str(&text[last_end..]);
    result
}

pub fn clean_text(text: &str, app: Option<&ActiveApp>, user_terms: &[String]) -> String {
    if text.is_empty() {
        return text.to_string();
    }

    let bundle_id = app.and_then(|a| a.bundle_id.as_deref());
    let app_name = app.and_then(|a| a.name.as_deref());
    let profile = DeveloperAppProfile::infer(bundle_id, app_name);

    let is_dev_app = matches!(
        profile,
        DeveloperAppProfile::Terminal | DeveloperAppProfile::Editor | DeveloperAppProfile::Ai
    );

    let mut result = text.to_string();
    let ambiguous_set: HashSet<&str> = AMBIGUOUS_TERMS.iter().copied().collect();

    // 1. Spoken acronyms (applied ONLY in dev apps)
    if is_dev_app {
        for (source, replacement) in SPOKEN_ACRONYMS {
            let actual_replacement = user_terms
                .iter()
                .find(|t| t.eq_ignore_ascii_case(replacement) || t.eq_ignore_ascii_case(source))
                .map(|t| t.as_str())
                .unwrap_or(replacement);
            result = replace_whole_phrase(source, actual_replacement, &result);
        }
    }

    // 2. Dev term casing
    for (source, replacement) in DEV_TERMS {
        let is_ambiguous = ambiguous_set.contains(source);
        let user_entry = user_terms.iter().find(|t| t.eq_ignore_ascii_case(source));

        // Skip ambiguous terms in general profile UNLESS user explicitly entered it in vocabulary.txt
        if !is_dev_app && is_ambiguous && user_entry.is_none() {
            continue;
        }

        let actual_replacement = user_entry.map(|t| t.as_str()).unwrap_or(replacement);
        result = replace_whole_phrase(source, actual_replacement, &result);
    }

    // 3. User vocabulary terms that were not in DEV_TERMS
    for user_term in user_terms {
        let lower = user_term.to_lowercase();
        let in_dev_terms = DEV_TERMS.iter().any(|(s, _)| s.eq_ignore_ascii_case(&lower));
        let in_acronyms = SPOKEN_ACRONYMS
            .iter()
            .any(|(s, r)| s.eq_ignore_ascii_case(&lower) || r.eq_ignore_ascii_case(&lower));
        if !in_dev_terms && !in_acronyms {
            result = replace_whole_phrase(&lower, user_term, &result);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_profile_infer() {
        assert_eq!(
            DeveloperAppProfile::infer(Some("com.googlecode.iterm2"), None),
            DeveloperAppProfile::Terminal
        );
        assert_eq!(
            DeveloperAppProfile::infer(None, Some("Ghostty")),
            DeveloperAppProfile::Terminal
        );
        assert_eq!(
            DeveloperAppProfile::infer(Some("com.apple.dt.xcode"), None),
            DeveloperAppProfile::Editor
        );
        assert_eq!(
            DeveloperAppProfile::infer(None, Some("Cursor")),
            DeveloperAppProfile::Editor
        );
        assert_eq!(
            DeveloperAppProfile::infer(None, Some("ChatGPT")),
            DeveloperAppProfile::Ai
        );
        assert_eq!(
            DeveloperAppProfile::infer(Some("com.apple.Safari"), Some("Safari")),
            DeveloperAppProfile::General
        );
    }

    #[test]
    fn test_replace_whole_phrase() {
        assert_eq!(
            replace_whole_phrase("n p m", "npm", "run n p m install"),
            "run npm install"
        );
        assert_eq!(
            replace_whole_phrase("swiftui", "SwiftUI", "building with swiftui today"),
            "building with SwiftUI today"
        );
        // Word boundary check: "myswiftui" shouldn't match "swiftui"
        assert_eq!(
            replace_whole_phrase("swiftui", "SwiftUI", "myswiftui app"),
            "myswiftui app"
        );
        // "json-file" has '-' which is a word boundary char, so "json" won't replace inside "json-file"
        assert_eq!(
            replace_whole_phrase("json", "JSON", "parse json-file"),
            "parse json-file"
        );
    }

    #[test]
    fn test_clean_text_dev_vs_general_context() {
        let dev_app = ActiveApp {
            name: Some("Ghostty".to_string()),
            bundle_id: Some("com.mitchellh.ghostty".to_string()),
        };
        let general_app = ActiveApp {
            name: Some("Mail".to_string()),
            bundle_id: Some("com.apple.mail".to_string()),
        };

        // Acronym expansion in terminal
        let cleaned_dev = clean_text("n p m install g i t h u b repo", Some(&dev_app), &[]);
        assert_eq!(cleaned_dev, "npm install GitHub repo");

        // Acronym expansion skipped in general app
        let cleaned_gen = clean_text("n p m install g i t h u b repo", Some(&general_app), &[]);
        assert_eq!(cleaned_gen, "n p m install g i t h u b repo");

        // Non-ambiguous term "github" casing applied in general app
        assert_eq!(
            clean_text("check github repo", Some(&general_app), &[]),
            "check GitHub repo"
        );

        // Ambiguous term "rest": in terminal -> REST, in general -> rest
        assert_eq!(
            clean_text("using rest api", Some(&dev_app), &[]),
            "using REST API"
        );
        assert_eq!(
            clean_text("taking a rest api", Some(&general_app), &[]),
            "taking a rest API"
        );
    }

    #[test]
    fn test_user_vocabulary_precedence() {
        let dev_app = ActiveApp {
            name: Some("VS Code".to_string()),
            bundle_id: Some("com.microsoft.vscode".to_string()),
        };

        // Default dev term for npm is "npm"
        assert_eq!(clean_text("run npm", Some(&dev_app), &[]), "run npm");

        // User explicit entry "NPM" overrides default "npm"
        let user_terms = vec!["NPM".to_string(), "MyCustomLib".to_string()];
        assert_eq!(
            clean_text("run n p m with mycustomlib", Some(&dev_app), &user_terms),
            "run NPM with MyCustomLib"
        );
    }
}
