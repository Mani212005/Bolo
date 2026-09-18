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

        let editor_names = [
            "xcode",
            "visual studio code",
            "cursor",
            "windsurf",
            "neovim",
        ];
        let editor_ids = ["com.apple.dt.xcode", "com.microsoft.vscode"];

        let ai_names = ["chatgpt", "claude", "codex"];

        if name
            .as_ref()
            .is_some_and(|n| terminal_names.iter().any(|t| n.contains(t)))
            || identifier
                .as_ref()
                .is_some_and(|id| terminal_ids.contains(&id.as_str()))
        {
            return DeveloperAppProfile::Terminal;
        }

        if name
            .as_ref()
            .is_some_and(|n| editor_names.iter().any(|e| n.contains(e)))
            || identifier
                .as_ref()
                .is_some_and(|id| editor_ids.contains(&id.as_str()))
        {
            return DeveloperAppProfile::Editor;
        }

        if name
            .as_ref()
            .is_some_and(|n| ai_names.iter().any(|a| n.contains(a)))
        {
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
                    let class_name = val
                        .get("class")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let title = val
                        .get("title")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
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
                            let name = node
                                .get("name")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            let app_id = node
                                .get("app_id")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            return Some(ActiveApp {
                                name,
                                bundle_id: app_id,
                            });
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

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
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
            if is_word_char(c) {
                continue;
            }
        }

        // Check character after end
        let next_char = text[end..].chars().next();
        if let Some(c) = next_char {
            if is_word_char(c) {
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
        let in_dev_terms = DEV_TERMS
            .iter()
            .any(|(s, _)| s.eq_ignore_ascii_case(&lower));
        let in_acronyms = SPOKEN_ACRONYMS
            .iter()
            .any(|(s, r)| s.eq_ignore_ascii_case(&lower) || r.eq_ignore_ascii_case(&lower));
        if !in_dev_terms && !in_acronyms {
            result = replace_whole_phrase(&lower, user_term, &result);
        }
    }

    result
}

pub const CODE_KEYWORDS: &[&str] = &[
    "function ", "def ", "fn ", "const ", "let ", "var ", "class ", "struct ",
    "impl ", "pub ", "import ", "export ", "from ", "return ", "if ", "for ",
    "while ", "switch ", "case ", "SELECT ", "INSERT ", "UPDATE ", "DELETE ",
    "CREATE ", "WHERE ", "async ", "await ", "typedef ", "interface ", "enum ",
    "package ", "namespace ", "#include", "val ", "using ", "echo ", "console.",
];

pub const CODE_SYNTAX_MARKERS: &[&str] = &[
    ";", "{", "}", "=>", "->", "()", "[]", "==", "!=", "===", "!==", "&&", "||", ":=", "</",
    "/>", "/*", "*/", "//", "#!/", "$ ",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptPiece {
    Spoken(String),
    Inserted(String),
}

/// Detects programming language of a code snippet from syntax and keywords.
pub fn detect_code_language(text: &str) -> Option<&'static str> {
    let lower = text.to_lowercase();

    if lower.contains("<html") || lower.contains("<!doctype") || lower.contains("</div>") || lower.contains("</span>") {
        return Some("html");
    }

    if (lower.contains("px;") || lower.contains("rem;") || lower.contains("display:") || lower.contains("color:")) && lower.contains('{') {
        return Some("css");
    }

    if text.contains("fn ") || text.contains("pub fn ") || text.contains("impl ") || text.contains("let mut ") || text.contains("println!") || text.contains("Vec<") || text.contains("Result<") || text.contains("use std::") {
        return Some("rust");
    }

    if text.contains("def ") || text.contains("elif ") || text.contains("print(") || text.contains("__name__") || text.contains("self.") || text.contains("lambda ") {
        return Some("python");
    }

    if text.contains(": string") || text.contains(": number") || text.contains(": boolean") || text.contains(": void") || text.contains("interface ") || text.contains("type ") || text.contains("as const") || text.contains("export interface") || text.contains("export type") {
        return Some("typescript");
    }

    if text.contains("console.log") || text.contains("const ") || text.contains("let ") || text.contains("var ") || text.contains("function ") || text.contains("=>") {
        return Some("javascript");
    }

    let upper = text.to_uppercase();
    if upper.contains("SELECT ") || upper.contains("INSERT INTO ") || upper.contains("UPDATE ") || upper.contains("DELETE FROM ") || upper.contains("CREATE TABLE ") {
        return Some("sql");
    }

    if text.contains("#!/bin/") || text.contains("echo ") || text.contains("grep ") || text.contains("sudo ") || text.contains("export ") || text.contains("chmod ") {
        return Some("bash");
    }

    let trimmed = text.trim();
    if (trimmed.starts_with('{') && trimmed.ends_with('}')) || (trimmed.starts_with('[') && trimmed.ends_with(']')) {
        if trimmed.contains("\":") || trimmed.contains("\": ") {
            return Some("json");
        }
    }

    if text.contains("#include <") || text.contains("std::") || text.contains("cout <<") || text.contains("nullptr") {
        return Some("cpp");
    }
    if text.contains("printf(") || text.contains("int main(") {
        return Some("c");
    }

    None
}

/// Determines if a single line possesses code patterns, keywords, or syntax.
pub fn is_code_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return true;
    }
    if trimmed.starts_with("```") {
        return true;
    }
    for kw in CODE_KEYWORDS {
        if trimmed.starts_with(kw) || trimmed.contains(&format!(" {kw}")) {
            return true;
        }
    }
    for marker in CODE_SYNTAX_MARKERS {
        if trimmed.contains(marker) {
            return true;
        }
    }
    if (line.starts_with("  ") || line.starts_with('\t')) && !trimmed.is_empty() {
        if trimmed.contains(';') || trimmed.contains('{') || trimmed.contains('}') || trimmed.contains('(') || trimmed.contains(')') || trimmed.contains('=') || trimmed.contains(':') || trimmed.contains('.') {
            return true;
        }
    }
    if trimmed == "{" || trimmed == "}" || trimmed == "};" || trimmed == "]" || trimmed == "];" || trimmed == ")" || trimmed == ");" || trimmed == "else:" || trimmed == "else {" {
        return true;
    }
    false
}

/// Detects if the text represents a multi-line code snippet.
/// If it does and is not already wrapped in triple backticks, wraps it in ```\n...\n```.
/// If conversational prose surrounds code, isolates the code block so prose is not swallowed.
pub fn format_smart_code(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return text.to_string();
    }
    if trimmed.starts_with("```") && trimmed.ends_with("```") {
        return text.to_string();
    }
    let lines: Vec<&str> = trimmed.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.len() >= 2 {
        let first_is_code = is_code_line(lines[0]);
        let last_is_code = is_code_line(lines[lines.len() - 1]);
        if !first_is_code || !last_is_code {
            let isolated = isolate_embedded_code(trimmed);
            if isolated != trimmed {
                return isolated;
            }
        }
    }
    if is_code_snippet(trimmed) {
        format!("```\n{}\n```", trimmed)
    } else {
        text.to_string()
    }
}

pub fn is_code_snippet(text: &str) -> bool {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() < 2 {
        return false;
    }

    let mut code_line_score = 0;
    let mut strong_signal_count = 0;

    for line in &lines {
        let mut is_line_code = false;
        for kw in CODE_KEYWORDS {
            if line.starts_with(kw) || line.contains(&format!(" {kw}")) {
                is_line_code = true;
                strong_signal_count += 1;
                break;
            }
        }
        if !is_line_code {
            for marker in CODE_SYNTAX_MARKERS {
                if line.contains(marker) {
                    is_line_code = true;
                    break;
                }
            }
        }
        if is_line_code {
            code_line_score += 1;
        }
    }

    // Must have at least 2 strong signals or >= 60% of lines matching code patterns
    let ratio = (code_line_score as f64) / (lines.len() as f64);
    (strong_signal_count >= 2 && ratio >= 0.5) || ratio >= 0.7
}

/// Detects and isolates embedded multi-line code inside spoken or mixed text.
/// Conversational words before and after remain natural prose, while only the
/// code region is fenced with language-tagged backticks.
pub fn isolate_embedded_code(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return text.to_string();
    }
    if trimmed.contains("```") {
        let mut in_code = false;
        let mut out: Vec<String> = Vec::new();
        for line in text.lines() {
            let t = line.trim();
            if t.starts_with("```") {
                in_code = !in_code;
                out.push(t.to_string());
            } else if in_code {
                out.push(line.trim_end().to_string());
            } else if t.is_empty() {
                if !out.last().map_or(true, |prev| prev.is_empty()) {
                    out.push(String::new());
                }
            } else {
                out.push(t.to_string());
            }
        }
        return out.join("\n").trim().to_string();
    }

    let lines: Vec<&str> = text.lines().collect();
    if lines.len() < 2 {
        return text.to_string();
    }

    enum Block<'a> {
        Prose(Vec<&'a str>),
        Code(Vec<&'a str>),
    }

    let mut blocks: Vec<Block> = Vec::new();
    let mut current_is_code: Option<bool> = None;
    let mut current_lines: Vec<&str> = Vec::new();

    for line in &lines {
        let code_flag = is_code_line(line);
        let effective_code = if line.trim().is_empty() {
            current_is_code.unwrap_or(false)
        } else {
            code_flag
        };

        match current_is_code {
            Some(is_code) if is_code == effective_code => {
                current_lines.push(line);
            }
            Some(is_code) => {
                if is_code {
                    blocks.push(Block::Code(std::mem::take(&mut current_lines)));
                } else {
                    blocks.push(Block::Prose(std::mem::take(&mut current_lines)));
                }
                current_is_code = Some(effective_code);
                current_lines.push(line);
            }
            None => {
                current_is_code = Some(effective_code);
                current_lines.push(line);
            }
        }
    }

    if let Some(is_code) = current_is_code {
        if !current_lines.is_empty() {
            if is_code {
                blocks.push(Block::Code(current_lines));
            } else {
                blocks.push(Block::Prose(current_lines));
            }
        }
    }

    let mut validated_blocks: Vec<Block> = Vec::new();
    for block in blocks {
        match block {
            Block::Code(lines) => {
                let snippet = lines.join("\n");
                if is_code_snippet(&snippet) {
                    validated_blocks.push(Block::Code(lines));
                } else {
                    validated_blocks.push(Block::Prose(lines));
                }
            }
            Block::Prose(lines) => {
                validated_blocks.push(Block::Prose(lines));
            }
        }
    }

    let has_code = validated_blocks.iter().any(|b| matches!(b, Block::Code(_)));
    if !has_code {
        return text.to_string();
    }

    let mut merged: Vec<Block> = Vec::new();
    for b in validated_blocks {
        match b {
            Block::Prose(lines) => {
                if let Some(Block::Prose(prev_lines)) = merged.last_mut() {
                    prev_lines.extend(lines);
                } else {
                    merged.push(Block::Prose(lines));
                }
            }
            Block::Code(lines) => {
                merged.push(Block::Code(lines));
            }
        }
    }

    let mut output_parts: Vec<String> = Vec::new();
    for block in merged {
        match block {
            Block::Prose(lines) => {
                let prose = lines.join("\n").trim().to_string();
                if !prose.is_empty() {
                    output_parts.push(prose);
                }
            }
            Block::Code(lines) => {
                let code_content = lines.join("\n").trim().to_string();
                let tag = detect_code_language(&code_content).unwrap_or("");
                let fenced = if tag.is_empty() {
                    format!("```\n{}\n```", code_content)
                } else {
                    format!("```{tag}\n{}\n```", code_content)
                };
                output_parts.push(fenced);
            }
        }
    }

    output_parts.join("\n\n")
}

/// Assembles multiple pieces of dictation and spliced snippets according to piece kind.
/// Speech pieces remain conversational prose outside code fences, while inserted
/// code snippets are formatted in language-tagged markdown code blocks, separated
/// by double-newlines.
pub fn assemble_transcript_pieces(
    pieces: &[TranscriptPiece],
    smart_code: bool,
) -> String {
    if pieces.is_empty() {
        return String::new();
    }

    struct FormattedPiece {
        text: String,
        is_code_block: bool,
    }

    let mut formatted: Vec<FormattedPiece> = Vec::new();

    for piece in pieces {
        match piece {
            TranscriptPiece::Spoken(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let isolated = isolate_embedded_code(trimmed);
                let is_code = isolated.starts_with("```")
                    && isolated.ends_with("```")
                    && !isolated[3..isolated.len().saturating_sub(3)].contains("```");
                formatted.push(FormattedPiece {
                    text: isolated,
                    is_code_block: is_code,
                });
            }
            TranscriptPiece::Inserted(snippet) => {
                let trimmed = snippet.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if smart_code && (is_code_snippet(trimmed) || trimmed.starts_with("```")) {
                    let code_text = if trimmed.starts_with("```") {
                        trimmed.to_string()
                    } else {
                        let tag = detect_code_language(trimmed).unwrap_or("");
                        if tag.is_empty() {
                            format!("```\n{}\n```", trimmed)
                        } else {
                            format!("```{tag}\n{}\n```", trimmed)
                        }
                    };
                    formatted.push(FormattedPiece {
                        text: code_text,
                        is_code_block: true,
                    });
                } else {
                    formatted.push(FormattedPiece {
                        text: trimmed.to_string(),
                        is_code_block: false,
                    });
                }
            }
        }
    }

    if formatted.is_empty() {
        return String::new();
    }

    let mut result = String::new();
    for (i, p) in formatted.iter().enumerate() {
        if i == 0 {
            result.push_str(&p.text);
        } else {
            let prev = &formatted[i - 1];
            if prev.is_code_block || p.is_code_block {
                result.push_str("\n\n");
            } else {
                result.push(' ');
            }
            result.push_str(&p.text);
        }
    }

    result
}


/// Maps Jev detected language to a canonical markdown code block language tag.
pub fn map_jev_language(lang: &str, text: &str) -> String {
    match lang.trim().to_lowercase().as_str() {
        "rust" => "rust".to_string(),
        "python" => "python".to_string(),
        "javascript" | "js" => "javascript".to_string(),
        "typescript" | "ts" => "typescript".to_string(),
        "sql" => "sql".to_string(),
        "bash" | "shell" | "sh" | "zsh" => "bash".to_string(),
        "html_css" => {
            let lower = text.to_lowercase();
            if lower.contains("<html")
                || lower.contains("<!doctype")
                || lower.contains("</div>")
                || text.contains('<')
            {
                "html".to_string()
            } else if text.contains('{')
                && (text.contains(':') || text.contains("px") || text.contains("color"))
            {
                "css".to_string()
            } else {
                "html".to_string()
            }
        }
        "html" => "html".to_string(),
        "css" => "css".to_string(),
        "json" => "json".to_string(),
        "c_cpp" | "c++" | "cpp" => "cpp".to_string(),
        "c" => "c".to_string(),
        "other" | "text" | "none" => "".to_string(),
        other => {
            let sanitized: String = other
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            sanitized
        }
    }
}

fn strip_bullet_prefix(s: &str) -> &str {
    let mut trimmed = s.trim();
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("• "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        trimmed = rest.trim();
    }
    if let Some(pos) = trimmed.find(['.', ')']) {
        if pos > 0 && pos < 4 && trimmed[..pos].chars().all(|c| c.is_ascii_digit()) {
            trimmed = trimmed[pos + 1..].trim();
        }
    }
    for prefix in &["bullet: ", "dash: ", "item: "] {
        if trimmed.to_lowercase().starts_with(prefix) {
            trimmed = trimmed[prefix.len()..].trim();
            break;
        }
    }
    trimmed
}

fn strip_task_prefix(s: &str) -> (&str, bool) {
    let mut trimmed = s.trim();
    if let Some(rest) = trimmed
        .strip_prefix("- [x] ")
        .or_else(|| trimmed.strip_prefix("- [X] "))
    {
        return (rest.trim(), true);
    }
    if let Some(rest) = trimmed.strip_prefix("- [ ] ") {
        return (rest.trim(), false);
    }
    if let Some(rest) = trimmed
        .strip_prefix("[x] ")
        .or_else(|| trimmed.strip_prefix("[X] "))
    {
        return (rest.trim(), true);
    }
    if let Some(rest) = trimmed.strip_prefix("[ ] ") {
        return (rest.trim(), false);
    }
    trimmed = strip_bullet_prefix(trimmed);
    (trimmed, false)
}

fn split_list_items(text: &str) -> Vec<String> {
    if text.contains('\n') {
        return text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
    }

    let re_bullet_words = regex::Regex::new(r"(?i)\s*\b(?:bullet|dash)\b\s*").unwrap();
    let matches: Vec<&str> = re_bullet_words
        .split(text)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if matches.len() > 1 {
        return matches.into_iter().map(String::from).collect();
    }

    let re_numbers = regex::Regex::new(r"(?:\s+|^)\d+[\.\)]\s+").unwrap();
    let num_parts: Vec<&str> = re_numbers
        .split(text)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if num_parts.len() > 1 {
        return num_parts.into_iter().map(String::from).collect();
    }

    let re_ordinals = regex::Regex::new(
        r"(?i)(?:\s+|^)(?:firstly|secondly|thirdly|first|second|third|fourth|fifth|finally),?\s+",
    )
    .unwrap();
    let ord_parts: Vec<&str> = re_ordinals
        .split(text)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if ord_parts.len() > 1 {
        return ord_parts.into_iter().map(String::from).collect();
    }

    let re_sentences = regex::Regex::new(r"[.?!]\s+").unwrap();
    let sent_parts: Vec<&str> = re_sentences
        .split(text)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if sent_parts.len() > 1 {
        return sent_parts
            .into_iter()
            .map(|s| s.trim_end_matches('.').trim().to_string())
            .collect();
    }

    vec![text.trim().to_string()]
}

pub fn format_bullet_list(text: &str) -> String {
    let items = split_list_items(text);
    if items.is_empty() {
        return text.to_string();
    }
    let mut out = Vec::new();
    for item in items {
        let cleaned = strip_bullet_prefix(&item);
        if !cleaned.is_empty() {
            out.push(format!("- {cleaned}"));
        }
    }
    if out.is_empty() {
        text.to_string()
    } else {
        out.join("\n")
    }
}

pub fn format_task_list(text: &str) -> String {
    let items = split_list_items(text);
    if items.is_empty() {
        return text.to_string();
    }
    let mut out = Vec::new();
    for item in items {
        let (cleaned, checked) = strip_task_prefix(&item);
        if !cleaned.is_empty() {
            if checked {
                out.push(format!("- [x] {cleaned}"));
            } else {
                out.push(format!("- [ ] {cleaned}"));
            }
        }
    }
    if out.is_empty() {
        text.to_string()
    } else {
        out.join("\n")
    }
}

pub fn format_multi_paragraph(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return text.to_string();
    }
    if trimmed.contains("\n\n") {
        let paragraphs: Vec<&str> = trimmed
            .split("\n\n")
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        return paragraphs.join("\n\n");
    }
    if trimmed.contains('\n') {
        let paragraphs: Vec<&str> = trimmed
            .lines()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        return paragraphs.join("\n\n");
    }

    let re_new_para = regex::Regex::new(r"(?i)\s*\b(?:new paragraph|next paragraph)\b\s*").unwrap();
    if re_new_para.is_match(trimmed) {
        let parts: Vec<&str> = re_new_para
            .split(trimmed)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        if parts.len() > 1 {
            return parts.join("\n\n");
        }
    }

    let re_sentence = regex::Regex::new(r"([^.?!]+[.?!]+)\s*").unwrap();
    let sentences: Vec<&str> = re_sentence
        .find_iter(trimmed)
        .map(|m| m.as_str().trim())
        .collect();

    if sentences.len() >= 4 {
        let mut paras = Vec::new();
        for chunk in sentences.chunks(2) {
            paras.push(chunk.join(" "));
        }
        return paras.join("\n\n");
    }

    trimmed.to_string()
}

/// Applies Jev predictive formatting decision to the transcript text.
pub fn format_with_jev_decision(
    text: &str,
    decision: &crate::jev::JevFormattingDecision,
) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return text.to_string();
    }

    if decision.is_code || decision.layout == "code_block" {
        if trimmed.starts_with("```") && trimmed.ends_with("```") {
            return text.to_string();
        }
        let lines: Vec<&str> = trimmed.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        if lines.len() >= 2 && (!is_code_line(lines[0]) || !is_code_line(lines[lines.len() - 1])) {
            let isolated = isolate_embedded_code(trimmed);
            if isolated != trimmed {
                return isolated;
            }
        }
        let tag = map_jev_language(&decision.language, trimmed);
        if tag.is_empty() {
            format!("```\n{}\n```", trimmed)
        } else {
            format!("```{tag}\n{}\n```", trimmed)
        }
    } else {
        match decision.layout.as_str() {
            "bullet_list" => format_bullet_list(text),
            "task_list" => format_task_list(text),
            "multi_paragraph" => format_multi_paragraph(text),
            _ => text.to_string(),
        }
    }
}

/// Formats text using Jev decision if present; otherwise cleanly falls back to format_smart_code.
pub fn format_with_fallback(
    text: &str,
    decision: Option<&crate::jev::JevFormattingDecision>,
    smart_code: bool,
) -> String {
    if let Some(dec) = decision {
        format_with_jev_decision(text, dec)
    } else if smart_code {
        format_smart_code(text)
    } else {
        text.to_string()
    }
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
            replace_whole_phrase("n p m", "npm", "Run n p m."),
            "Run npm."
        );
        assert_eq!(
            replace_whole_phrase("swiftui", "SwiftUI", "building with swiftui today"),
            "building with SwiftUI today"
        );
        assert_eq!(
            replace_whole_phrase("swiftui", "SwiftUI", "building with swiftui."),
            "building with SwiftUI."
        );
        assert_eq!(
            replace_whole_phrase("swiftui", "SwiftUI", "is it swiftui? yes, swiftui!"),
            "is it SwiftUI? yes, SwiftUI!"
        );
        // Word boundary check: "myswiftui" shouldn't match "swiftui"
        assert_eq!(
            replace_whole_phrase("swiftui", "SwiftUI", "myswiftui app"),
            "myswiftui app"
        );
        assert_eq!(
            replace_whole_phrase("swiftui", "SwiftUI", "swiftui2 app"),
            "swiftui2 app"
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

        // Punctuation boundary checks at end of sentence
        assert_eq!(
            clean_text("I love swiftui.", Some(&dev_app), &[]),
            "I love SwiftUI."
        );
        assert_eq!(
            clean_text("Check github, then use typescript!", Some(&dev_app), &[]),
            "Check GitHub, then use TypeScript!"
        );
        assert_eq!(
            clean_text("Is it json? Yes, parse json.", Some(&dev_app), &[]),
            "Is it JSON? Yes, parse JSON."
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

    #[test]
    fn test_smart_code_detection_and_formatting() {
        let code_sample =
            "const total = items.reduce((acc, x) => acc + x.price, 0);\nreturn total;";
        assert!(is_code_snippet(code_sample));
        assert_eq!(
            format_smart_code(code_sample),
            "```\nconst total = items.reduce((acc, x) => acc + x.price, 0);\nreturn total;\n```"
        );

        let python_sample = "def calculate_sum(a, b):\n    return a + b";
        assert!(is_code_snippet(python_sample));
        assert_eq!(
            format_smart_code(python_sample),
            "```\ndef calculate_sum(a, b):\n    return a + b\n```"
        );

        let prose_sample =
            "Hey captain, the build succeeded.\nLet's deploy the update to production.";
        assert!(!is_code_snippet(prose_sample));
        assert_eq!(format_smart_code(prose_sample), prose_sample);

        // Already formatted with backticks should remain unchanged
        let already_formatted = "```\nconst a = 1;\nconst b = 2;\n```";
        assert_eq!(format_smart_code(already_formatted), already_formatted);
    }

    #[test]
    fn test_bullet_and_task_prefix_stripping() {
        assert_eq!(strip_bullet_prefix(".env"), ".env");
        assert_eq!(strip_bullet_prefix(".NET"), ".NET");
        assert_eq!(strip_bullet_prefix("1. test"), "test");
        assert_eq!(strip_bullet_prefix("2) test"), "test");
        assert_eq!(strip_bullet_prefix("- test"), "test");
        assert_eq!(strip_bullet_prefix("* test"), "test");
        assert_eq!(strip_bullet_prefix("bullet: test"), "test");

        let input = ".env config\n.NET 8\n1. first item\n- second item";
        assert_eq!(
            format_bullet_list(input),
            "- .env config\n- .NET 8\n- first item\n- second item"
        );
    }

    #[test]
    fn test_format_with_jev_decision_language_tagging() {
        let rust_code = "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}";
        let rust_dec = crate::jev::JevFormattingDecision {
            is_code: true,
            code_probability: 0.98,
            language: "rust".to_string(),
            layout: "code_block".to_string(),
        };
        assert_eq!(
            format_with_jev_decision(rust_code, &rust_dec),
            "```rust\nfn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n```"
        );

        let py_code = "def greet(name):\n    print(f'Hello {name}')";
        let py_dec = crate::jev::JevFormattingDecision {
            is_code: true,
            code_probability: 0.95,
            language: "python".to_string(),
            layout: "code_block".to_string(),
        };
        assert_eq!(
            format_with_jev_decision(py_code, &py_dec),
            "```python\ndef greet(name):\n    print(f'Hello {name}')\n```"
        );

        let sql_code = "SELECT id, name FROM users WHERE active = 1;";
        let sql_dec = crate::jev::JevFormattingDecision {
            is_code: true,
            code_probability: 0.9,
            language: "sql".to_string(),
            layout: "code_block".to_string(),
        };
        assert_eq!(
            format_with_jev_decision(sql_code, &sql_dec),
            "```sql\nSELECT id, name FROM users WHERE active = 1;\n```"
        );

        let other_code = "some unusual code snippet";
        let other_dec = crate::jev::JevFormattingDecision {
            is_code: true,
            code_probability: 0.85,
            language: "other".to_string(),
            layout: "code_block".to_string(),
        };
        assert_eq!(
            format_with_jev_decision(other_code, &other_dec),
            "```\nsome unusual code snippet\n```"
        );

        // Already wrapped code block should not be double wrapped
        let already_wrapped = "```rust\nlet x = 1;\n```";
        assert_eq!(
            format_with_jev_decision(already_wrapped, &rust_dec),
            already_wrapped
        );
    }

    #[test]
    fn test_format_with_jev_decision_layout_structuring() {
        // Bullet list
        let bullet_input = "item one\nitem two\nitem three";
        let bullet_dec = crate::jev::JevFormattingDecision {
            is_code: false,
            code_probability: 0.02,
            language: "other".to_string(),
            layout: "bullet_list".to_string(),
        };
        assert_eq!(
            format_with_jev_decision(bullet_input, &bullet_dec),
            "- item one\n- item two\n- item three"
        );

        // Task list
        let task_input = "first finish tests\nsecond review PR";
        let task_dec = crate::jev::JevFormattingDecision {
            is_code: false,
            code_probability: 0.01,
            language: "other".to_string(),
            layout: "task_list".to_string(),
        };
        assert_eq!(
            format_with_jev_decision(task_input, &task_dec),
            "- [ ] first finish tests\n- [ ] second review PR"
        );

        // Multi-paragraph text
        let multi_para_input = "First paragraph content.\nSecond paragraph content.";
        let multi_dec = crate::jev::JevFormattingDecision {
            is_code: false,
            code_probability: 0.05,
            language: "other".to_string(),
            layout: "multi_paragraph".to_string(),
        };
        assert_eq!(
            format_with_jev_decision(multi_para_input, &multi_dec),
            "First paragraph content.\n\nSecond paragraph content."
        );

        // Single block text
        let single_input = "Just a single sentence of plain text.";
        let single_dec = crate::jev::JevFormattingDecision {
            is_code: false,
            code_probability: 0.05,
            language: "other".to_string(),
            layout: "single_block".to_string(),
        };
        assert_eq!(
            format_with_jev_decision(single_input, &single_dec),
            "Just a single sentence of plain text."
        );
    }

    #[test]
    fn test_format_with_fallback() {
        let code_text = "const total = items.reduce((acc, x) => acc + x.price, 0);\nreturn total;";
        let prose_text = "Hello team, please review the documentation.";

        // 1. With Jev decision: uses Jev decision formatting
        let jev_dec = crate::jev::JevFormattingDecision {
            is_code: true,
            code_probability: 0.99,
            language: "typescript".to_string(),
            layout: "code_block".to_string(),
        };
        assert_eq!(
            format_with_fallback(code_text, Some(&jev_dec), true),
            format!("```typescript\n{code_text}\n```")
        );

        // 2. Without Jev decision (fallback): uses format_smart_code when smart_code is true
        assert_eq!(
            format_with_fallback(code_text, None, true),
            format!("```\n{code_text}\n```")
        );

        // 3. Without Jev decision (fallback): leaves text untouched when smart_code is false
        assert_eq!(format_with_fallback(code_text, None, false), code_text);

        // 4. Prose fallback
        assert_eq!(format_with_fallback(prose_text, None, true), prose_text);
    }

    #[test]
    fn test_detect_code_language() {
        assert_eq!(
            detect_code_language("const x: number = 42;\nconsole.log(x);"),
            Some("typescript")
        );
        assert_eq!(
            detect_code_language("def greet(name):\n    print(f'hello {name}')"),
            Some("python")
        );
        assert_eq!(
            detect_code_language("fn main() {\n    println!(\"hello\");\n}"),
            Some("rust")
        );
        assert_eq!(
            detect_code_language("SELECT * FROM users WHERE id = 1;"),
            Some("sql")
        );
        assert_eq!(
            detect_code_language("#!/bin/bash\necho 'hello world'"),
            Some("bash")
        );
    }

    #[test]
    fn test_isolate_embedded_code_preserves_conversational_speech() {
        let input = "Here is the implementation of the function:\ndef calculate_sum(a, b):\n    return a + b\nWhat do you think of this approach?";
        let formatted = isolate_embedded_code(input);
        let expected = "Here is the implementation of the function:\n\n```python\ndef calculate_sum(a, b):\n    return a + b\n```\n\nWhat do you think of this approach?";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_assemble_transcript_pieces_multi_piece() {
        let pieces = vec![
            TranscriptPiece::Spoken("Here is the function to handle the request:".to_string()),
            TranscriptPiece::Inserted("const x: number = 42;\nconsole.log(x);".to_string()),
            TranscriptPiece::Spoken("Please test and review it.".to_string()),
        ];
        let assembled = assemble_transcript_pieces(&pieces, true);
        let expected = "Here is the function to handle the request:\n\n```typescript\nconst x: number = 42;\nconsole.log(x);\n```\n\nPlease test and review it.";
        assert_eq!(assembled, expected);
    }

    #[test]
    fn test_assemble_transcript_pieces_spoken_before_and_after() {
        let pieces = vec![
            TranscriptPiece::Spoken("Speech before".to_string()),
            TranscriptPiece::Inserted("const a: number = 1;\nconsole.log(a);".to_string()),
            TranscriptPiece::Spoken("Speech after".to_string()),
        ];
        let assembled = assemble_transcript_pieces(&pieces, true);
        let expected = "Speech before\n\n```typescript\nconst a: number = 1;\nconsole.log(a);\n```\n\nSpeech after";
        assert_eq!(assembled, expected);
    }

    #[test]
    fn test_format_smart_code_with_embedded_prose() {
        let mixed = "Here is the code:\ndef add(a, b):\n    return a + b\nLet me know.";
        let result = format_smart_code(mixed);
        assert_eq!(
            result,
            "Here is the code:\n\n```python\ndef add(a, b):\n    return a + b\n```\n\nLet me know."
        );
    }
}

