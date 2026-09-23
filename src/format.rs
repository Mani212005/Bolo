//! Dictation formatting: turns transcript pieces into the text Bolo types.
//!
//! Local checks settle almost every piece. Only genuinely ambiguous ones are
//! sent to Jev, batched into at most one request per dictation, so plain
//! speech never waits on the network. Output is rendered for the destination:
//! Markdown fences where they render (chat apps, terminals running agent
//! CLIs), raw code in editors where fences would land in the source file.

use crate::config::FormattingConfig;
use crate::vocab::{ActiveApp, DeveloperAppProfile, TranscriptPiece};
use regex::Regex;
use serde_json::{json, Value};
use std::sync::{LazyLock, Mutex};

/// How code blocks are written into the destination app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeStyle {
    /// Wrapped in ```lang fences, for apps that render Markdown.
    Fenced,
    /// Bare code on its own lines, for code editors.
    Raw,
}

/// Editors not covered by `DeveloperAppProfile`; matched against the lowercased
/// app name (exact for short names, which are too generic to substring-match).
const EDITOR_NAMES_EXACT: &[&str] = &["zed", "nova", "vim", "emacs", "helix"];
const EDITOR_NAMES_CONTAINS: &[&str] = &[
    "sublime text",
    "intellij",
    "pycharm",
    "webstorm",
    "goland",
    "clion",
    "rustrover",
    "android studio",
    "bbedit",
    "textmate",
];
const EDITOR_ID_PREFIXES: &[&str] = &[
    "dev.zed.",
    "com.sublimetext.",
    "com.jetbrains.",
    "com.google.android.studio",
    "com.todesktop.230313mzl4w4u92", // Cursor
    "com.exafunction.windsurf",
    "com.panic.nova",
    "com.barebones.bbedit",
    "com.macromates.textmate",
];

/// Picks the code style for the frontmost app. Config overrides win, then
/// editors get raw code, and everything else (chat apps, terminals, unknown)
/// gets fences.
pub fn code_style(app: Option<&ActiveApp>, cfg: &FormattingConfig) -> CodeStyle {
    let name = app
        .and_then(|a| a.name.as_deref())
        .unwrap_or("")
        .to_lowercase();
    let id = app
        .and_then(|a| a.bundle_id.as_deref())
        .unwrap_or("")
        .to_lowercase();
    let matches = |pattern: &String| {
        let p = pattern.trim().to_lowercase();
        !p.is_empty() && (name.contains(&p) || id.contains(&p))
    };
    if cfg.fenced_code_apps.iter().any(matches) {
        return CodeStyle::Fenced;
    }
    if cfg.raw_code_apps.iter().any(matches) {
        return CodeStyle::Raw;
    }
    let profile = DeveloperAppProfile::infer(
        app.and_then(|a| a.bundle_id.as_deref()),
        app.and_then(|a| a.name.as_deref()),
    );
    let is_editor = profile == DeveloperAppProfile::Editor
        || EDITOR_NAMES_EXACT.contains(&name.as_str())
        || EDITOR_NAMES_CONTAINS.iter().any(|e| name.contains(e))
        || EDITOR_ID_PREFIXES.iter().any(|p| id.starts_with(p));
    if is_editor {
        CodeStyle::Raw
    } else {
        CodeStyle::Fenced
    }
}

// ---------------------------------------------------------------- local code check

/// Local verdict on whether a pasted piece is code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Prose,
    Code,
    /// Too close to call locally; a candidate for Jev.
    Unsure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Code,
    /// Counts as code only when the piece also has real code lines, so a
    /// Markdown heading or a lone `# note` does not make prose look like code.
    Comment,
    /// Weak signal, such as a `key: value` line that could be YAML or prose.
    Maybe,
    Prose,
}

/// Line starts that mark code. Case-sensitive on purpose: dictated and
/// pasted prose starts sentences with a capital ("If you...").
const CODE_LINE_STARTS: &[&str] = &[
    "def ",
    "class ",
    "fn ",
    "pub ",
    "impl ",
    "struct ",
    "enum ",
    "use ",
    "import ",
    "from ",
    "export ",
    "const ",
    "let ",
    "var ",
    "function ",
    "return",
    "async ",
    "await ",
    "#include",
    "package ",
    "func ",
    "public ",
    "private ",
    "protected ",
    "static ",
    "interface ",
    "type ",
    "try:",
    "except",
    "finally:",
    "with ",
    "lambda ",
    "print(",
    "console.",
    "SELECT ",
    "INSERT ",
    "UPDATE ",
    "DELETE ",
    "CREATE ",
    "ALTER ",
    "DROP ",
    "WHERE ",
    "FROM ",
    "echo ",
    "sudo ",
    "cd ",
    "git ",
    "npm ",
    "npx ",
    "cargo ",
    "pip ",
    "curl ",
    "docker ",
    "kubectl ",
    "brew ",
    "yarn ",
    "pnpm ",
    "export ",
    "$ ",
    "FROM ",
    "RUN ",
    "COPY ",
    "WORKDIR ",
    "CMD ",
    "ENV ",
    "EXPOSE ",
    "ENTRYPOINT ",
    "ARG ",
    "ADD ",
];

/// Control-flow starts that only count as code with a code-shaped ending,
/// so "for example..." in lowercase prose is not mistaken for a loop.
const CONTROL_STARTS: &[&str] = &[
    "if ", "if(", "elif ", "else", "for ", "while ", "switch ", "match ",
];

const STRONG_MARKERS: &[&str] = &[
    "=>", "->", "::", "==", "!=", "&&", "||", "</", "/>", "();", "${", "):", "{}", "[]",
];

static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^[A-Za-z_$][\w$.\[\]'"]*\s*([+\-*/%|&]?=|:=)\s*[^=\s]"#).unwrap()
});
static CALL_STATEMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z_$][\w$.]*\(.*\)[;,]?$").unwrap());
static KEY_VALUE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^"?[\w\-.]+"?\s*:\s*\S*$"#).unwrap());

fn line_kind(line: &str) -> LineKind {
    let t = line.trim();
    if t.starts_with("#!") {
        return LineKind::Code;
    }
    if t == "#"
        || t.starts_with("# ")
        || t.starts_with("//")
        || t.starts_with("/*")
        || t.starts_with("*/")
        || t.starts_with("* ")
        || t.starts_with("-- ")
    {
        return LineKind::Comment;
    }
    if CODE_LINE_STARTS.iter().any(|s| t.starts_with(s)) {
        return LineKind::Code;
    }
    let code_ending = t.ends_with(':')
        || t.ends_with('{')
        || t.ends_with(';')
        || t.ends_with('}')
        || t.ends_with(')')
        || t.ends_with(',');
    if CONTROL_STARTS.iter().any(|s| t.starts_with(s)) && (code_ending || t.contains('(')) {
        return LineKind::Code;
    }
    if matches!(
        t,
        "{" | "}" | "};" | "]" | "];" | ")" | ");" | "end" | "fi" | "done"
    ) {
        return LineKind::Code;
    }
    if t.ends_with(';') || t.ends_with('{') || t.ends_with("});") {
        return LineKind::Code;
    }
    if ASSIGNMENT.is_match(t) || CALL_STATEMENT.is_match(t) {
        return LineKind::Code;
    }
    if STRONG_MARKERS.iter().any(|m| t.contains(m)) {
        return LineKind::Code;
    }
    let indented = line.starts_with("  ") || line.starts_with('\t');
    if indented && t.chars().any(|c| ";{}()=:.[]".contains(c)) {
        return LineKind::Code;
    }
    if KEY_VALUE.is_match(t) && t.split_whitespace().count() <= 3 {
        return LineKind::Maybe;
    }
    LineKind::Prose
}

/// Classifies a pasted piece from its lines alone.
pub fn classify_code(text: &str) -> Verdict {
    let trimmed = text.trim();
    if trimmed.starts_with("```") {
        return Verdict::Code;
    }
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return Verdict::Prose;
    }
    let kinds: Vec<LineKind> = lines.iter().map(|l| line_kind(l)).collect();
    if lines.len() == 1 {
        // A single line is prose unless it is clearly code; even then a
        // one-liner is ambiguous enough (a command? a sentence with `=`?)
        // to ask rather than guess.
        return if kinds[0] == LineKind::Code {
            Verdict::Unsure
        } else {
            Verdict::Prose
        };
    }
    let real = kinds.iter().filter(|k| **k == LineKind::Code).count();
    let comments = kinds.iter().filter(|k| **k == LineKind::Comment).count();
    let maybe = kinds.iter().filter(|k| **k == LineKind::Maybe).count();
    let mut score = real as f64 + 0.5 * maybe as f64;
    if real > 0 {
        score += comments as f64;
    }
    let ratio = score / lines.len() as f64;
    if ratio >= 0.6 {
        Verdict::Code
    } else if ratio <= 0.25 {
        Verdict::Prose
    } else {
        Verdict::Unsure
    }
}

/// Splits a pasted piece that mixes prose and code (such as a copied chat
/// message) into prose and code parts. Returns None unless it finds at least
/// one run of 2+ code lines next to prose.
fn split_mixed(text: &str) -> Option<Vec<Part>> {
    let lines: Vec<&str> = text.lines().collect();
    let mut parts: Vec<(bool, Vec<&str>)> = Vec::new();
    for line in lines {
        let is_code = match line_kind(line) {
            LineKind::Code | LineKind::Comment => true,
            LineKind::Maybe | LineKind::Prose => false,
        };
        // Blank lines stay with the run they are in.
        let is_code = if line.trim().is_empty() {
            parts.last().map(|(c, _)| *c).unwrap_or(false)
        } else {
            is_code
        };
        match parts.last_mut() {
            Some((c, run)) if *c == is_code => run.push(line),
            _ => parts.push((is_code, vec![line])),
        }
    }
    let code_runs = parts
        .iter()
        .filter(|(c, run)| *c && run.iter().filter(|l| !l.trim().is_empty()).count() >= 2)
        .count();
    let has_prose = parts
        .iter()
        .any(|(c, run)| !*c && run.iter().any(|l| !l.trim().is_empty()));
    if code_runs == 0 || !has_prose {
        return None;
    }
    let mut out = Vec::new();
    for (is_code, run) in parts {
        let body = run.join("\n").trim().to_string();
        if body.is_empty() {
            continue;
        }
        let code_lines = run.iter().filter(|l| !l.trim().is_empty()).count();
        if is_code && code_lines >= 2 {
            out.push(Part::Code {
                lang: language_of(&body),
                text: body,
            });
        } else {
            out.push(Part::Block(body));
        }
    }
    Some(out)
}

static TOML_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\[[\w.\-]+\]\s*$").unwrap());
static YAML_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*[\w\-]+:(\s|$)").unwrap());

/// Best local guess at a code block's language tag, or "" when unknown.
/// Formats the generic detector confuses are checked first.
pub fn language_of(code: &str) -> String {
    let lines: Vec<&str> = code
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let first = lines.first().copied().unwrap_or("");
    if first.starts_with("FROM ")
        && lines
            .iter()
            .any(|l| l.starts_with("RUN ") || l.starts_with("COPY ") || l.starts_with("CMD "))
    {
        return "dockerfile".to_string();
    }
    if code.contains("func ")
        && (code.contains("package ")
            || code.contains(":=")
            || code.contains("*http.")
            || code.contains("fmt."))
    {
        return "go".to_string();
    }
    if TOML_HEADER.is_match(code) && code.contains(" = ") {
        return "toml".to_string();
    }
    let yaml_keys = YAML_KEY.find_iter(code).count();
    if yaml_keys >= 2 && !code.contains('{') && !code.contains(';') && yaml_keys * 2 >= lines.len()
    {
        return "yaml".to_string();
    }
    crate::vocab::detect_code_language(code)
        .unwrap_or("")
        .to_string()
}

// ---------------------------------------------------------------- spoken prose

/// A list the speaker asked for by name; formatted without asking Jev.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    Bullets,
    Tasks,
}

static BULLET_CUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(bullet(?:ed)?\s+(?:points?|list)|as\s+bullets|in\s+bullets)\b").unwrap()
});
static TASK_CUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(to-?do\s+list|to\s+do\s+list|checklist|task\s+list)\b").unwrap()
});
static ORDINAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(first(?:ly)?|second(?:ly)?|third(?:ly)?|fourth|fifth|finally|lastly|number\s+(?:one|two|three))\b").unwrap()
});
static SENTENCE_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"[.?!]+["')\]]?\s+"#).unwrap());

/// Words that usually open a new thought in dictation.
const TRANSITIONS: &[&str] = &[
    "so",
    "also",
    "now",
    "okay",
    "ok",
    "next",
    "another",
    "but",
    "however",
    "anyway",
    "then",
    "finally",
    "alright",
    "right",
    "and then",
    "after that",
    "apart from that",
    "secondly",
    "lastly",
    "additionally",
    "meanwhile",
];

/// Finds an explicit list request and where the items start.
fn explicit_list(text: &str) -> Option<(ListKind, usize)> {
    let (kind, cue) = if let Some(m) = TASK_CUE.find(text) {
        (ListKind::Tasks, m)
    } else {
        let m = BULLET_CUE.find(text)?;
        (ListKind::Bullets, m)
    };
    // Items start after the cue's sentence or colon: "Make a todo list: a, b."
    let after = &text[cue.end()..];
    let offset = after
        .find([':', '.', '?', '!'])
        .map(|i| cue.end() + i + 1)
        .unwrap_or(cue.end());
    Some((kind, offset))
}

/// Three or more distinct ordinal words ("first... second... finally") make a
/// dictated list likely, but prose uses them too, so this only nominates the
/// run for a Jev question.
fn ordinal_list_candidate(text: &str) -> bool {
    let mut seen: Vec<String> = ORDINAL
        .find_iter(text)
        .map(|m| m.as_str().to_lowercase())
        .collect();
    seen.sort();
    seen.dedup();
    seen.len() >= 3
}

fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut last = 0;
    for m in SENTENCE_END.find_iter(text) {
        // Only split before a capital letter or digit, so "v1.79 is" and
        // "e.g. this" stay intact.
        let next = text[m.end()..].chars().next();
        if next.is_some_and(|c| c.is_uppercase() || c.is_ascii_digit()) {
            out.push(text[last..m.end()].trim().to_string());
            last = m.end();
        }
    }
    let tail = text[last..].trim();
    if !tail.is_empty() {
        out.push(tail.to_string());
    }
    out
}

fn opens_new_thought(sentence: &str) -> bool {
    let lower = sentence.to_lowercase();
    TRANSITIONS.iter().any(|t| {
        lower
            .strip_prefix(t)
            .is_some_and(|rest| rest.starts_with(',') || rest.starts_with(' ') || rest.is_empty())
    })
}

/// Breaks long dictated prose into paragraphs: a new one starts at a
/// transition word once the current paragraph has 2+ sentences, or after 4
/// sentences regardless. Short dictations (under 4 sentences) are unchanged.
pub fn paragraphize(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.contains('\n') {
        return trimmed.to_string();
    }
    let sentences = split_sentences(trimmed);
    if sentences.len() < 4 {
        return trimmed.to_string();
    }
    let mut paragraphs: Vec<Vec<String>> = vec![Vec::new()];
    for sentence in sentences {
        let current = paragraphs.last().unwrap();
        let start_new = !current.is_empty()
            && (current.len() >= 4 || (current.len() >= 2 && opens_new_thought(&sentence)));
        if start_new {
            paragraphs.push(Vec::new());
        }
        paragraphs.last_mut().unwrap().push(sentence);
    }
    paragraphs
        .into_iter()
        .map(|p| p.join(" "))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Formats list items after a lead-in, keeping the lead-in as prose.
fn listify(text: &str, items_start: usize, kind: ListKind) -> Option<String> {
    let lead = text[..items_start].trim();
    let rest = text[items_start..].trim();
    if rest.is_empty() {
        return None;
    }
    let list = match kind {
        ListKind::Bullets => crate::vocab::format_bullet_list(rest),
        ListKind::Tasks => crate::vocab::format_task_list(rest),
    };
    if list.lines().count() < 2 {
        return None;
    }
    // Spoken items end in sentence periods; list items read better without.
    let list = list
        .lines()
        .map(|l| match l.strip_suffix('.') {
            Some(item) if !item.ends_with('.') => item,
            _ => l,
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(if lead.is_empty() {
        list
    } else {
        format!("{lead}\n\n{list}")
    })
}

/// Splits dictation that enumerates with ordinals into a lead-in and bullets.
fn listify_ordinals(text: &str) -> Option<String> {
    let first = ORDINAL.find(text)?;
    listify(text, first.start(), ListKind::Bullets)
}

// ---------------------------------------------------------------- the plan

/// Options that shape formatting, taken from `[formatting]`.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub smart_code: bool,
    pub paragraphs: bool,
    pub list_cues: bool,
}

impl Options {
    pub fn from_config(cfg: &FormattingConfig) -> Self {
        Self {
            smart_code: cfg.smart_code,
            paragraphs: cfg.paragraphs,
            list_cues: cfg.list_cues,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Part {
    /// Flowing prose: consecutive speech plus short inline pastes.
    Prose {
        text: String,
        ordinal_question: Option<usize>,
    },
    Code {
        text: String,
        lang: String,
    },
    /// A multi-line paste that is not code; kept on its own lines verbatim.
    Block(String),
    /// A paste Jev will judge; the index is its question slot.
    Unsure {
        text: String,
        question: usize,
    },
}

/// A dictation broken into parts, plus the questions (if any) to ask Jev.
#[derive(Debug, Clone)]
pub struct Plan {
    parts: Vec<Part>,
    /// Texts behind each question slot, in slot order.
    questions: Vec<Question>,
    options: Options,
    pieces: usize,
}

#[derive(Debug, Clone)]
enum Question {
    IsCode(String),
    IsList(String),
}

/// Jev's p(code) and optional (language, confidence) for one paste.
type CodeAnswer = (f64, Option<(String, f64)>);

/// Answers from Jev, keyed by question slot.
#[derive(Debug, Clone, Default)]
pub struct Answers {
    code: Vec<Option<CodeAnswer>>,
    list: Vec<Option<f64>>,
}

/// Probability a Jev yes/no answer must reach before Bolo acts on it; below
/// it the text is left exactly as it came.
pub const ACT_ABOVE: f64 = 0.7;
/// Minimum Choice confidence to trust Jev's language over the local guess.
const LANGUAGE_CONFIDENCE: f64 = 0.5;

const JEV_LANGUAGES: &[(&str, &str)] = &[
    ("python", "Python"),
    ("javascript", "JavaScript"),
    ("typescript", "TypeScript, including TSX"),
    ("rust", "Rust"),
    ("go", "Go"),
    ("java", "Java"),
    ("kotlin", "Kotlin"),
    ("swift", "Swift"),
    ("c", "C"),
    ("cpp", "C++"),
    ("csharp", "C#"),
    ("ruby", "Ruby"),
    ("php", "PHP"),
    ("bash", "Shell commands or a shell script (bash, zsh, sh)"),
    ("sql", "SQL query or schema"),
    ("html", "HTML markup"),
    ("css", "CSS or SCSS styles"),
    ("json", "JSON data"),
    ("yaml", "YAML configuration"),
    ("toml", "TOML configuration"),
    ("dockerfile", "Dockerfile"),
    ("markdown", "Markdown document"),
    ("none", "Not code, or a language not listed here"),
];

impl Plan {
    /// Builds the plan from raw pieces. Consecutive speech and single-line
    /// pastes (a URL, a name) flow together as prose; everything else is its
    /// own part.
    pub fn new(pieces: &[TranscriptPiece], options: Options) -> Self {
        let mut parts: Vec<Part> = Vec::new();
        let mut questions: Vec<Question> = Vec::new();
        let mut prose = String::new();

        let flush = |prose: &mut String, parts: &mut Vec<Part>, questions: &mut Vec<Question>| {
            let text = prose.trim().to_string();
            prose.clear();
            if text.is_empty() {
                return;
            }
            let ordinal_question = if options.list_cues
                && explicit_list(&text).is_none()
                && ordinal_list_candidate(&text)
            {
                questions.push(Question::IsList(text.clone()));
                Some(questions.len() - 1)
            } else {
                None
            };
            parts.push(Part::Prose {
                text,
                ordinal_question,
            });
        };

        for piece in pieces {
            match piece {
                TranscriptPiece::Spoken(s) => {
                    push_inline(&mut prose, s);
                }
                TranscriptPiece::Inserted(s) => {
                    let text = s.trim();
                    if text.is_empty() {
                        continue;
                    }
                    let multi_line = text.contains('\n');
                    let verdict = if options.smart_code {
                        classify_code(text)
                    } else {
                        Verdict::Prose
                    };
                    match verdict {
                        Verdict::Prose if !multi_line => push_inline(&mut prose, text),
                        Verdict::Prose => {
                            flush(&mut prose, &mut parts, &mut questions);
                            parts.push(Part::Block(text.to_string()));
                        }
                        Verdict::Code => {
                            flush(&mut prose, &mut parts, &mut questions);
                            // Mostly code can still open or close with a
                            // sentence ("Here is the fix:"); keep that prose
                            // out of the code block.
                            match split_mixed(text) {
                                Some(split) if !text.starts_with("```") => parts.extend(split),
                                _ => parts.push(code_part(text)),
                            }
                        }
                        Verdict::Unsure => {
                            flush(&mut prose, &mut parts, &mut questions);
                            if let Some(split) = split_mixed(text) {
                                parts.extend(split);
                            } else {
                                questions.push(Question::IsCode(text.to_string()));
                                parts.push(Part::Unsure {
                                    text: text.to_string(),
                                    question: questions.len() - 1,
                                });
                            }
                        }
                    }
                }
            }
        }
        flush(&mut prose, &mut parts, &mut questions);

        Self {
            parts,
            questions,
            options,
            pieces: pieces.len(),
        }
    }

    /// A plan that asks Jev about one pasted text regardless of the local
    /// verdict, so `bolo eval-format` can compare Jev against the local check.
    pub fn for_eval(text: &str) -> Self {
        Self {
            parts: vec![Part::Unsure {
                text: text.to_string(),
                question: 0,
            }],
            questions: vec![Question::IsCode(text.to_string())],
            options: Options {
                smart_code: true,
                paragraphs: false,
                list_cues: false,
            },
            pieces: 1,
        }
    }

    /// Jev's code probability and language for the eval plan's one question.
    pub fn eval_answer(&self, answers: &Answers) -> Option<(f64, Option<String>)> {
        let (p, lang) = answers.code.first().cloned().flatten()?;
        let lang = lang
            .filter(|(l, c)| l != "none" && *c >= LANGUAGE_CONFIDENCE)
            .map(|(l, _)| l);
        Some((p, lang))
    }

    /// True when Jev has something to decide.
    pub fn needs_jev(&self) -> bool {
        !self.questions.is_empty()
    }

    /// The single batched request body for every open question, or None.
    pub fn jev_request(&self, app_name: Option<&str>, model: &str) -> Option<Value> {
        if self.questions.is_empty() {
            return None;
        }
        let pieces: Vec<Value> = self
            .questions
            .iter()
            .map(|q| match q {
                Question::IsCode(t) => json!({"kind": "pasted into a dictation", "text": t}),
                Question::IsList(t) => json!({"kind": "spoken dictation", "text": t}),
            })
            .collect();
        let mut state = serde_json::Map::new();
        state.insert("pieces".into(), Value::Array(pieces));
        if let Some(app) = app_name {
            state.insert("destination_app".into(), Value::String(app.to_string()));
        }

        let languages: serde_json::Map<String, Value> = JEV_LANGUAGES
            .iter()
            .map(|(id, desc)| (id.to_string(), Value::String(desc.to_string())))
            .collect();
        let mut questions = serde_json::Map::new();
        for (i, q) in self.questions.iter().enumerate() {
            match q {
                Question::IsCode(_) => {
                    questions.insert(
                        format!("code_{i}"),
                        json!({
                            "type": "noul",
                            "instructions": format!(
                                "Is `pieces[{i}].text` source code, a shell command, a query, markup, \
                                 or a configuration or data file that a reader would want shown as a \
                                 code block? Answer no for natural-language prose, including prose \
                                 that talks about code, a quoted error message inside a sentence, \
                                 a URL, or a list of names."
                            )
                        }),
                    );
                    questions.insert(
                        format!("language_{i}"),
                        json!({
                            "type": "choice",
                            "instructions": format!(
                                "Which language is `pieces[{i}].text` written in, for a Markdown \
                                 code block tag?"
                            ),
                            "criteria": languages.clone()
                        }),
                    );
                }
                Question::IsList(_) => {
                    questions.insert(
                        format!("list_{i}"),
                        json!({
                            "type": "noul",
                            "instructions": format!(
                                "Did the speaker dictate `pieces[{i}].text` as a list of separate \
                                 items, enumerated with words like first, second, next and finally, \
                                 that should be written as bullet points? Answer no when those \
                                 words narrate steps, reasons or events in flowing prose."
                            )
                        }),
                    );
                }
            }
        }
        Some(json!({
            "model": model,
            "state": Value::Object(state),
            "questions": Value::Object(questions),
        }))
    }

    /// Reads a Jev response's `answers` into per-slot decisions. Missing or
    /// malformed answers become None, which renders as the local fallback.
    pub fn parse_answers(&self, answers: &Value) -> Answers {
        let mut out = Answers {
            code: vec![None; self.questions.len()],
            list: vec![None; self.questions.len()],
        };
        for (i, q) in self.questions.iter().enumerate() {
            match q {
                Question::IsCode(_) => {
                    let p = noul(answers.get(format!("code_{i}")));
                    let lang = choice(answers.get(format!("language_{i}")));
                    out.code[i] = p.map(|p| (p, lang));
                }
                Question::IsList(_) => {
                    out.list[i] = noul(answers.get(format!("list_{i}")));
                }
            }
        }
        out
    }

    /// Renders the final text. `answers` is None when Jev was skipped, failed
    /// or timed out; open questions then fall back to the local result.
    pub fn render(&self, answers: Option<&Answers>, style: CodeStyle) -> String {
        let mut blocks: Vec<String> = Vec::new();
        for part in &self.parts {
            let block = match part {
                Part::Prose {
                    text,
                    ordinal_question,
                } => self.render_prose(text, *ordinal_question, answers),
                Part::Code { text, lang } => render_code(text, lang, style),
                Part::Block(text) => text.clone(),
                Part::Unsure { text, question } => {
                    match answers.and_then(|a| a.code.get(*question).cloned().flatten()) {
                        Some((p, lang)) if p >= ACT_ABOVE => {
                            let lang = lang
                                .filter(|(l, c)| l != "none" && *c >= LANGUAGE_CONFIDENCE)
                                .map(|(l, _)| l)
                                .unwrap_or_else(|| language_of(text));
                            render_code(text, &lang, style)
                        }
                        // Not code, unsure, or no answer: keep it verbatim on
                        // its own lines rather than guess.
                        _ => text.clone(),
                    }
                }
            };
            if !block.is_empty() {
                blocks.push(block);
            }
        }
        blocks.join("\n\n")
    }

    fn render_prose(
        &self,
        text: &str,
        ordinal_question: Option<usize>,
        answers: Option<&Answers>,
    ) -> String {
        if self.options.list_cues {
            if let Some((kind, start)) = explicit_list(text) {
                if let Some(list) = listify(text, start, kind) {
                    return list;
                }
            }
            if let Some(q) = ordinal_question {
                let is_list = answers
                    .and_then(|a| a.list.get(q).copied().flatten())
                    .is_some_and(|p| p >= ACT_ABOVE);
                if is_list {
                    if let Some(list) = listify_ordinals(text) {
                        return list;
                    }
                }
            }
        }
        if self.options.paragraphs {
            paragraphize(text)
        } else {
            text.to_string()
        }
    }

    /// One-line summary for the daemon log.
    pub fn summary(&self) -> String {
        let count = |f: fn(&Part) -> bool| self.parts.iter().filter(|p| f(p)).count();
        format!(
            "pieces={} prose={} code={} unsure={} questions={}",
            self.pieces,
            count(|p| matches!(p, Part::Prose { .. })),
            count(|p| matches!(p, Part::Code { .. })),
            count(|p| matches!(p, Part::Unsure { .. })),
            self.questions.len()
        )
    }
}

fn push_inline(prose: &mut String, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    if !prose.is_empty() {
        prose.push(' ');
    }
    prose.push_str(text);
}

fn code_part(text: &str) -> Part {
    let text = text.trim();
    if let Some(inner) = strip_fences(text) {
        let (lang, body) = inner;
        return Part::Code {
            lang: if lang.is_empty() {
                language_of(&body)
            } else {
                lang
            },
            text: body,
        };
    }
    Part::Code {
        lang: language_of(text),
        text: text.to_string(),
    }
}

/// Returns (language tag, body) for text wrapped in a single ``` fence.
fn strip_fences(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix("```")?;
    let body = rest.strip_suffix("```")?;
    let (first, body) = body.split_once('\n').unwrap_or(("", body));
    if body.contains("```") {
        return None;
    }
    Some((first.trim().to_string(), body.trim_end().to_string()))
}

fn render_code(text: &str, lang: &str, style: CodeStyle) -> String {
    match style {
        CodeStyle::Fenced => format!("```{lang}\n{}\n```", text.trim_end()),
        CodeStyle::Raw => text.trim_end().to_string(),
    }
}

fn noul(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    v.as_f64()
        .or_else(|| v.get("noul").and_then(Value::as_f64))
        .filter(|p| (0.0..=1.0).contains(p))
}

fn choice(v: Option<&Value>) -> Option<(String, f64)> {
    let v = v?;
    let c = v.get("choice").and_then(Value::as_str)?;
    let confidence = v.get("confidence").and_then(Value::as_f64).unwrap_or(0.0);
    Some((c.to_lowercase(), confidence))
}

// ---------------------------------------------------------------- stats

/// Jev usage since the daemon started, shown on the dashboard.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct JevStats {
    pub dictations: u64,
    pub local_only: u64,
    pub calls: u64,
    pub fallbacks: u64,
    pub total_latency_ms: u64,
}

static STATS: LazyLock<Mutex<JevStats>> = LazyLock::new(|| Mutex::new(JevStats::default()));

/// Records one formatted dictation: `call` is None when no request was made,
/// otherwise the request latency and whether it produced usable answers.
pub fn record(call: Option<(u64, bool)>) {
    let mut s = STATS.lock().unwrap();
    s.dictations += 1;
    match call {
        None => s.local_only += 1,
        Some((latency_ms, ok)) => {
            s.calls += 1;
            s.total_latency_ms += latency_ms;
            if !ok {
                s.fallbacks += 1;
            }
        }
    }
}

pub fn stats() -> JevStats {
    *STATS.lock().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Options {
        Options {
            smart_code: true,
            paragraphs: true,
            list_cues: true,
        }
    }

    fn cfg() -> FormattingConfig {
        FormattingConfig::default()
    }

    fn app(name: &str, id: &str) -> ActiveApp {
        ActiveApp {
            name: Some(name.to_string()),
            bundle_id: Some(id.to_string()),
        }
    }

    const LEAP_YEAR: &str = r#"year = 2000

# To get year (integer input) from the user
# year = int(input("Enter a year: "))

# divided by 100 means century year (ending with 00)
# century year divided by 400 is leap year
if (year % 400 == 0) and (year % 100 == 0):
    print("{0} is a leap year".format(year))

# not divided by 100 means not a century year
# year divided by 4 is a leap year
elif (year % 4 ==0) and (year % 100 != 0):
    print("{0} is a leap year".format(year))

# if not divided by both 400 (century year) and 4 (not century year)
# year is not leap year
else:
    print("{0} is not a leap year".format(year))"#;

    #[test]
    fn comment_heavy_python_is_code_locally() {
        // The exact snippet from the captain's test, which the old detector
        // scored as prose.
        assert_eq!(classify_code(LEAP_YEAR), Verdict::Code);
        assert_eq!(language_of(LEAP_YEAR), "python");
    }

    #[test]
    fn prose_is_prose() {
        let prose = "I think the agent is done. Could you tell me what it has done?\n\
                     Also let me know if the class of problems is fixed.";
        assert_eq!(classify_code(prose), Verdict::Prose);
        assert_eq!(
            classify_code("Check https://example.com/a?b=c for details"),
            Verdict::Prose
        );
    }

    #[test]
    fn one_line_code_is_unsure_not_guessed() {
        assert_eq!(classify_code("const x = compute(a, b);"), Verdict::Unsure);
        assert_eq!(classify_code("Thanks, that works"), Verdict::Prose);
    }

    #[test]
    fn yaml_like_config_is_unsure() {
        let yaml = "name: bolo\nversion: 2\nNotes about the release follow here\nowner: mani";
        assert_eq!(classify_code(yaml), Verdict::Unsure);
    }

    #[test]
    fn plain_speech_makes_no_jev_request() {
        let pieces = vec![TranscriptPiece::Spoken(
            "Okay, great. So now I want to put Jev at it. It is a fast model. Let me test it."
                .to_string(),
        )];
        let plan = Plan::new(&pieces, opts());
        assert!(!plan.needs_jev());
        assert!(plan.jev_request(None, "jev-latest").is_none());
    }

    #[test]
    fn captain_leap_year_dictation_renders_fenced_on_own_lines() {
        let pieces = vec![
            TranscriptPiece::Spoken("I want to see if this works or not.".to_string()),
            TranscriptPiece::Inserted(LEAP_YEAR.to_string()),
            TranscriptPiece::Spoken("I've spliced in something.".to_string()),
        ];
        let plan = Plan::new(&pieces, opts());
        assert!(
            !plan.needs_jev(),
            "an obvious paste must not cost a request"
        );
        let out = plan.render(None, CodeStyle::Fenced);
        assert!(out.starts_with("I want to see if this works or not.\n\n```python\nyear = 2000\n"));
        assert!(out.ends_with("```\n\nI've spliced in something."));
    }

    #[test]
    fn editors_get_raw_code() {
        let pieces = vec![
            TranscriptPiece::Spoken("Here it is.".to_string()),
            TranscriptPiece::Inserted(LEAP_YEAR.to_string()),
        ];
        let out = Plan::new(&pieces, opts()).render(None, CodeStyle::Raw);
        assert!(!out.contains("```"));
        assert!(out.starts_with("Here it is.\n\nyear = 2000"));
    }

    #[test]
    fn inline_url_paste_stays_in_the_sentence() {
        let pieces = vec![
            TranscriptPiece::Spoken("Open".to_string()),
            TranscriptPiece::Inserted("https://github.com/Mani212005/Bolo/pull/17".to_string()),
            TranscriptPiece::Spoken("and review it.".to_string()),
        ];
        let out = Plan::new(&pieces, opts()).render(None, CodeStyle::Fenced);
        assert_eq!(
            out,
            "Open https://github.com/Mani212005/Bolo/pull/17 and review it."
        );
    }

    #[test]
    fn unsure_pastes_share_one_request() {
        let pieces = vec![
            TranscriptPiece::Spoken("Look at these.".to_string()),
            TranscriptPiece::Inserted("const x = compute(a, b);".to_string()),
            TranscriptPiece::Spoken("and".to_string()),
            TranscriptPiece::Inserted("SELECT id FROM users;".to_string()),
        ];
        let plan = Plan::new(&pieces, opts());
        let req = plan.jev_request(Some("Claude"), "jev-latest").unwrap();
        let q = req["questions"].as_object().unwrap();
        assert_eq!(q.len(), 4, "two pieces x (code, language) in ONE request");
        assert!(q.contains_key("code_0") && q.contains_key("code_1"));
        assert_eq!(req["state"]["destination_app"], "Claude");
        assert_eq!(req["model"], "jev-latest");
    }

    #[test]
    fn jev_answers_decide_unsure_pastes() {
        let pieces = vec![
            TranscriptPiece::Spoken("Run".to_string()),
            TranscriptPiece::Inserted("const x = compute(a, b);".to_string()),
        ];
        let plan = Plan::new(&pieces, opts());
        let yes = plan.parse_answers(&json!({
            "code_0": {"type": "noul", "noul": 0.93},
            "language_0": {"type": "choice", "choice": "javascript", "confidence": 0.8}
        }));
        assert_eq!(
            plan.render(Some(&yes), CodeStyle::Fenced),
            "Run\n\n```javascript\nconst x = compute(a, b);\n```"
        );
        let no = plan.parse_answers(&json!({"code_0": {"type": "noul", "noul": 0.1}}));
        assert_eq!(
            plan.render(Some(&no), CodeStyle::Fenced),
            "Run\n\nconst x = compute(a, b);"
        );
        // No answer (timeout): verbatim, never guessed.
        assert_eq!(
            plan.render(None, CodeStyle::Fenced),
            "Run\n\nconst x = compute(a, b);"
        );
    }

    #[test]
    fn long_speech_gets_paragraphs() {
        let text =
            "I want you to work on aideos. There are a few things left. The editor needs work. \
                    So the first thing is the timeline. It drops frames. It also lags. \
                    Also check the audio sync. It drifts after a minute.";
        let out = paragraphize(text);
        assert_eq!(out.matches("\n\n").count(), 2, "{out}");
        assert!(out.contains("The editor needs work.\n\nSo the first thing"));
        assert!(out.contains("It also lags.\n\nAlso check"));
    }

    #[test]
    fn short_speech_is_untouched() {
        let text = "Yeah, run No Mistakes pipeline. And open up the PR.";
        assert_eq!(paragraphize(text), text);
        // Version numbers and abbreviations do not split sentences.
        assert_eq!(
            split_sentences("Update to v1.79 now. Then e.g. restart it.").len(),
            2
        );
    }

    #[test]
    fn explicit_todo_cue_formats_locally() {
        let pieces = vec![TranscriptPiece::Spoken(
            "Make a todo list: buy milk. Call the bank. Ship the Bolo PR.".to_string(),
        )];
        let plan = Plan::new(&pieces, opts());
        assert!(!plan.needs_jev());
        assert_eq!(
            plan.render(None, CodeStyle::Fenced),
            "Make a todo list:\n\n- [ ] buy milk\n- [ ] Call the bank\n- [ ] Ship the Bolo PR"
        );
    }

    #[test]
    fn ordinal_speech_asks_jev_and_respects_the_answer() {
        let text = "Here is the plan. First, fix the timeout. Second, batch the calls. Finally, add stats.";
        let pieces = vec![TranscriptPiece::Spoken(text.to_string())];
        let plan = Plan::new(&pieces, opts());
        assert!(plan.needs_jev());
        let yes = plan.parse_answers(&json!({"list_0": {"type": "noul", "noul": 0.9}}));
        assert_eq!(
            plan.render(Some(&yes), CodeStyle::Fenced),
            "Here is the plan.\n\n- fix the timeout\n- batch the calls\n- add stats"
        );
        // Not a list: it stays prose (with the usual paragraph breaks).
        let no = plan.parse_answers(&json!({"list_0": {"type": "noul", "noul": 0.2}}));
        assert_eq!(
            plan.render(Some(&no), CodeStyle::Fenced),
            paragraphize(text)
        );
        assert!(!plan.render(Some(&no), CodeStyle::Fenced).contains("- "));
    }

    #[test]
    fn list_cues_off_never_lists() {
        let options = Options {
            list_cues: false,
            ..opts()
        };
        let pieces = vec![TranscriptPiece::Spoken(
            "Make a todo list: buy milk. Call the bank.".to_string(),
        )];
        let plan = Plan::new(&pieces, options);
        assert!(!plan.render(None, CodeStyle::Fenced).contains("- [ ]"));
    }

    #[test]
    fn mixed_paste_is_split_locally() {
        let paste = "Here is the fix I used for the loop:\n\
                     for item in items:\n    total += item.price\nreturn total\n\
                     That solved the crash for me.";
        let pieces = vec![TranscriptPiece::Inserted(paste.to_string())];
        let plan = Plan::new(&pieces, opts());
        assert!(!plan.needs_jev());
        let out = plan.render(None, CodeStyle::Fenced);
        assert!(
            out.contains("Here is the fix I used for the loop:\n\n```"),
            "{out}"
        );
        assert!(out.contains("return total\n```\n\nThat solved the crash for me."));
    }

    #[test]
    fn prefenced_paste_is_restyled_for_editors() {
        let pieces = vec![TranscriptPiece::Inserted(
            "```rust\nfn main() {}\nlet x = 1;\n```".to_string(),
        )];
        let plan = Plan::new(&pieces, opts());
        assert_eq!(
            plan.render(None, CodeStyle::Fenced),
            "```rust\nfn main() {}\nlet x = 1;\n```"
        );
        assert_eq!(
            plan.render(None, CodeStyle::Raw),
            "fn main() {}\nlet x = 1;"
        );
    }

    #[test]
    fn smart_code_off_leaves_pastes_alone() {
        let options = Options {
            smart_code: false,
            ..opts()
        };
        let pieces = vec![TranscriptPiece::Inserted(LEAP_YEAR.to_string())];
        let out = Plan::new(&pieces, options).render(None, CodeStyle::Fenced);
        assert!(!out.contains("```"));
    }

    #[test]
    fn code_style_per_app() {
        let c = cfg();
        assert_eq!(
            code_style(Some(&app("Claude", "com.anthropic.claudefordesktop")), &c),
            CodeStyle::Fenced
        );
        assert_eq!(
            code_style(Some(&app("Ghostty", "com.mitchellh.ghostty")), &c),
            CodeStyle::Fenced
        );
        assert_eq!(
            code_style(Some(&app("Code", "com.microsoft.VSCode")), &c),
            CodeStyle::Raw
        );
        assert_eq!(
            code_style(Some(&app("Zed", "dev.zed.Zed")), &c),
            CodeStyle::Raw
        );
        assert_eq!(
            code_style(Some(&app("PyCharm", "com.jetbrains.pycharm")), &c),
            CodeStyle::Raw
        );
        assert_eq!(code_style(None, &c), CodeStyle::Fenced);
    }

    #[test]
    fn code_style_overrides_win() {
        let mut c = cfg();
        c.fenced_code_apps = vec!["vscode".to_string()];
        c.raw_code_apps = vec!["ghostty".to_string()];
        assert_eq!(
            code_style(Some(&app("Code", "com.microsoft.VSCode")), &c),
            CodeStyle::Fenced
        );
        assert_eq!(
            code_style(Some(&app("Ghostty", "com.mitchellh.ghostty")), &c),
            CodeStyle::Raw
        );
    }

    #[test]
    fn stats_count_local_and_remote() {
        let before = stats();
        record(None);
        record(Some((900, true)));
        record(Some((1500, false)));
        let after = stats();
        assert_eq!(after.dictations - before.dictations, 3);
        assert_eq!(after.local_only - before.local_only, 1);
        assert_eq!(after.calls - before.calls, 2);
        assert_eq!(after.fallbacks - before.fallbacks, 1);
    }
}
