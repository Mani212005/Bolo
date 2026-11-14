//! Spoken lists: lays out a dictation that enumerates items as a Markdown
//! list without changing the speaker's words.
//!
//! A cheap local gate (`is_candidate`) nominates dictations that look like a
//! list. `format.rs` then asks Jev, in its one request, whether the dictation
//! is a list and which role each sentence plays (lead-in, item, before or
//! after the list). This module does everything else in code: it splits items
//! at the speaker's own enumerators ("first", "number two", "one is ..."),
//! strips only those enumerator words, and refuses any layout that would drop
//! other words, so every character of the output comes from the dictation.

use regex::Regex;
use std::sync::LazyLock;

/// Longest dictation (in sentences) sent to Jev as a list candidate, so one
/// long monologue never makes a huge request.
pub const MAX_SENTENCES: usize = 24;
/// Minimum Choice confidence for Jev's "item" role on at least one sentence.
const ITEM_CONFIDENCE: f64 = 0.6;

/// What a sentence does in a dictated list, as Jev judges it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    LeadIn,
    Item,
    Before,
    After,
}

impl Role {
    /// Jev choice ids with their criteria.
    pub const CHOICES: [(Role, &'static str, &'static str); 4] = [
        (
            Role::LeadIn,
            "lead_in",
            "Introduces or announces the list that follows, without being one of its items \
             (for example 'a couple of things', 'make a list for this', 'two issues I have', \
             'things to do today').",
        ),
        (
            Role::Item,
            "item",
            "Is one of the listed items, or part of one: a separate request, task, issue, point \
             or thing the speaker enumerates.",
        ),
        (
            Role::Before,
            "before",
            "Comes before the list and neither introduces it nor belongs to it.",
        ),
        (
            Role::After,
            "after",
            "Comes after the list has ended: a closing remark, request or comment about the items \
             that is not itself one of them.",
        ),
    ];

    pub fn from_id(id: &str) -> Option<Role> {
        Self::CHOICES
            .iter()
            .find(|(_, name, _)| *name == id)
            .map(|(role, _, _)| *role)
    }
}

/// How list items are marked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// `1.`, `2.`: the speaker counted the items.
    Numbered,
    /// `- `
    Bullets,
    /// `- [ ] `, for an explicit to-do list.
    Tasks,
}

// ---------------------------------------------------------------- local gate

/// "A couple of things", "two issues", "the following", "things to do".
static ANNOUNCER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:(?:a\s+couple\s+of|a\s+few|several|two|three|four|five|six|seven|eight|nine|ten|\d+)\s+(?:\w+\s+)?(?:things|issues|points|items|steps|reasons|problems|changes|questions|ideas|tasks|bugs|features|options|suggestions|concerns|priorities)\b|the\s+following\b|things\s+(?:to|i\s+need\s+to|we\s+need\s+to)\s+(?:do|fix|buy|check|change))",
    )
    .unwrap()
});
/// Naming a list in a way that announces one. The bare word "list" is
/// mostly an instruction to an agent ("list down the tools") and is not here.
static LIST_PHRASE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:(?:make|making|write|here\s+is|here's|this\s+is|that\s+is)\s+(?:a|my|the|this)\s+(?:\w+\s+)?list\b|put\s+(?:that|this|it|them|these|those)\s+(?:in|into)\s+a\s+list\b)",
    )
    .unwrap()
});
/// "Number one", "step one", "point one": counting out loud.
static COUNTER_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:number|step|point|item)\s+(?:one|1)\b").unwrap());
/// Words that enumerate; two different ones make a list likely enough to ask.
static ENUMERATOR_WORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(first|second|third|fourth|fifth)(?:ly)?\b|\b(finally|lastly)\b|\b(number\s+(?:one|two|three))\b|\b(one\s+is)\b|\b(the\s+other\s+one)\b",
    )
    .unwrap()
});

/// True when `text` may be a dictated list worth asking Jev about. This only
/// nominates; a list is never formatted on these words alone.
pub fn is_candidate(text: &str, sentences: &[String]) -> bool {
    if sentences.is_empty() || sentences.len() > MAX_SENTENCES {
        return false;
    }
    if ANNOUNCER.is_match(text) || LIST_PHRASE.is_match(text) || COUNTER_START.is_match(text) {
        return true;
    }
    let mut classes: Vec<String> = ENUMERATOR_WORD
        .captures_iter(text)
        .filter_map(|c| (1..=5).find_map(|i| c.get(i)))
        .map(|m| {
            m.as_str()
                .to_lowercase()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    classes.sort();
    classes.dedup();
    classes.len() >= 2 || counted_markers(sentences) >= 2
}

/// Distinct counted values among the enumerators found at clause starts, so
/// "the issues are, one, ..., two, ..." counts as an enumeration.
fn counted_markers(sentences: &[String]) -> usize {
    let mut values: Vec<u32> = Vec::new();
    let mut expected = 1;
    for sentence in sentences {
        for seg in scan_sentence(sentence, &mut expected, false) {
            if let Some(Count::Nth(n)) = seg.marker {
                values.push(n);
            }
        }
    }
    values.sort();
    values.dedup();
    values.len()
}

// ---------------------------------------------------------------- enumerators

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Count {
    /// The speaker's own count: "second", "number two", "two is".
    Nth(u32),
    /// "The other one is", after "one is".
    Other,
    /// "Finally", "lastly", "last,".
    Last,
    /// "Then", "also", "next,": a new item without a count.
    Sequence,
}

impl Count {
    fn counted(self) -> bool {
        matches!(self, Count::Nth(_) | Count::Other)
    }
}

const NUMBERS: &[&str] = &[
    "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
];
const ORDINALS: &[&str] = &[
    "first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth", "ninth", "tenth",
];
/// Nouns that only name the slot in an enumeration ("first thing,", "the
/// second one is", "second step,"), so they go with the enumerator.
const SLOT_NOUNS: &str = "one|thing|point|item|step|issue|problem|reason|task|idea|question|change|option|priority|phase|stage|part|checkpoint|bug|feature|request|suggestion|concern|goal|note|topic";
/// Words that can follow a bare "First" when it is an adverb ("First I
/// opened"), unlike "First draft" where it is part of the item.
const AFTER_BARE_ORDINAL: &[&str] = &[
    "i", "we", "you", "they", "he", "she", "it", "the", "a", "an", "my", "our", "your", "their",
    "this", "that", "these", "those", "there", "let's", "let", "please", "i'll", "we'll", "i'm",
    "we're", "you'll", "you'd", "we'd", "i'd", "so",
];

static ORDINAL_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)^(?:and\s+)?(?P<the>the\s+)?(?P<ord>{})(?P<ly>ly)?(?:\s+(?P<noun>{SLOT_NOUNS}))?(?P<verb>\s+(?:is|being|was|would\s+be|will\s+be))?(?P<punct>\s*[,:])?\s+",
        ORDINALS.join("|")
    ))
    .unwrap()
});
static LAST_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:and\s+)?(?:(?P<always>finally|lastly|last\s+but\s+not\s+least)|last(?:\s+(?:one|thing|point|item|step))?)(?P<verb>\s+(?:is|being))?(?P<punct>\s*[,:])?\s+",
    )
    .unwrap()
});
static COUNTER_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)^(?:and\s+)?(?:number|step|point|item)\s+(?P<n>{}|\d+)(?:\s+(?:is|being))?\s*[,:]?\s+",
        NUMBERS.join("|")
    ))
    .unwrap()
});
static CARDINAL_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)^(?:and\s+)?(?P<n>{})(?P<verb>\s+(?:is|being))?(?P<punct>\s*[,:])?\s+",
        NUMBERS.join("|")
    ))
    .unwrap()
});
static OTHER_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:and\s+)?the\s+other\s+(?:one\s+)?(?:is|being)\s*,?\s+").unwrap()
});
static SEQUENCE_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?:and\s+)?(?:then|also|plus|additionally|after\s+that)\s*,?\s+|^(?:and\s+)?next\s*,\s+",
    )
    .unwrap()
});

fn number_value(word: &str) -> Option<u32> {
    let w = word.to_lowercase();
    NUMBERS
        .iter()
        .position(|n| *n == w)
        .map(|i| i as u32 + 1)
        .or_else(|| w.parse().ok())
}

/// The enumerator at the very start of `s`, as (length, count). `expected`
/// is the next number in the speaker's count; a bare cardinal ("two is")
/// only counts when it is that number.
fn marker_at(s: &str, expected: u32, allow_sequence: bool) -> Option<(usize, Count)> {
    if let Some(c) = ORDINAL_MARKER.captures(s) {
        let next_word = s[c.get(0).unwrap().end()..]
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_lowercase();
        let has_verb = c.name("verb").is_some();
        let has_punct = c.name("punct").is_some();
        let valid = if c.name("the").is_some() {
            c.name("noun").is_some() || has_verb
        } else if c.name("ly").is_some() || has_verb || has_punct {
            true
        } else {
            c.name("noun").is_none() && AFTER_BARE_ORDINAL.contains(&next_word.as_str())
        };
        if valid {
            let ord = c.name("ord").unwrap().as_str().to_lowercase();
            let n = ORDINALS.iter().position(|o| *o == ord).unwrap() as u32 + 1;
            return Some((c.get(0).unwrap().end(), Count::Nth(n)));
        }
    }
    if let Some(c) = COUNTER_MARKER.captures(s) {
        if let Some(n) = number_value(c.name("n").unwrap().as_str()) {
            return Some((c.get(0).unwrap().end(), Count::Nth(n)));
        }
    }
    if let Some(m) = OTHER_MARKER.find(s) {
        return Some((m.end(), Count::Other));
    }
    if let Some(c) = CARDINAL_MARKER.captures(s) {
        let n = number_value(c.name("n").unwrap().as_str());
        let shaped = c.name("verb").is_some() || c.name("punct").is_some();
        if shaped && n == Some(expected) {
            return Some((c.get(0).unwrap().end(), Count::Nth(expected)));
        }
    }
    if let Some(c) = LAST_MARKER.captures(s) {
        if c.name("always").is_some() || c.name("verb").is_some() || c.name("punct").is_some() {
            return Some((c.get(0).unwrap().end(), Count::Last));
        }
    }
    if allow_sequence {
        if let Some(m) = SEQUENCE_MARKER.find(s) {
            return Some((m.end(), Count::Sequence));
        }
    }
    None
}

/// Byte offsets inside `s` where a new clause may start: after ", " or "; ",
/// or at the word "and".
fn clause_starts(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut prev: Option<char> = None;
    let mut before_space: Option<char> = None;
    for (i, c) in s.char_indices() {
        if !c.is_whitespace() && prev.is_some_and(char::is_whitespace) && i > 0 {
            let after_comma = matches!(before_space, Some(',' | ';'));
            let is_and = s[i..]
                .get(..4)
                .is_some_and(|w| w.eq_ignore_ascii_case("and "));
            if after_comma || is_and {
                out.push(i);
            }
        }
        if !c.is_whitespace() {
            before_space = Some(c);
        }
        prev = Some(c);
    }
    out
}

/// A piece of a sentence: the enumerator that opens it (if any) and the
/// speaker's words after it.
#[derive(Debug, Clone, PartialEq)]
struct Segment<'a> {
    marker: Option<Count>,
    text: &'a str,
}

/// Splits one sentence at its enumerators. The first segment covers the
/// start of the sentence; later ones start at clause-level enumerators that
/// continue the count ("first is X, second is Y and third one is Z").
fn scan_sentence<'a>(
    sentence: &'a str,
    expected: &mut u32,
    allow_sequence: bool,
) -> Vec<Segment<'a>> {
    let s = sentence.trim();
    let mut cuts: Vec<(usize, usize, Count)> = Vec::new();
    let opening = marker_at(s, *expected, allow_sequence);
    let mut next = *expected;
    if let Some((len, count)) = opening {
        cuts.push((0, len, count));
        next = advance(next, count);
    }
    let mut inner: Vec<(usize, usize, Count)> = Vec::new();
    let mut counted_so_far = opening.is_some_and(|(_, c)| c.counted());
    for start in clause_starts(s) {
        if cuts.first().is_some_and(|(_, end, _)| start < *end) {
            continue;
        }
        let Some((len, count)) = marker_at(&s[start..], next, false) else {
            continue;
        };
        let fits = match count {
            Count::Nth(n) => n == next,
            Count::Other => counted_so_far,
            Count::Last => counted_so_far,
            Count::Sequence => false,
        };
        if fits {
            inner.push((start, start + len, count));
            next = advance(next, count);
            counted_so_far = true;
        }
    }
    // One stray "one," inside a sentence is not an enumeration.
    let opened_with_count = opening.is_some_and(|(_, c)| c.counted());
    if opened_with_count || inner.len() >= 2 {
        cuts.extend(inner);
    } else {
        next = opening.map_or(*expected, |(_, c)| advance(*expected, c));
    }
    *expected = next;

    let mut segments = Vec::new();
    if cuts.first().is_none_or(|(start, _, _)| *start > 0) {
        let end = cuts.first().map_or(s.len(), |(start, _, _)| *start);
        segments.push(Segment {
            marker: None,
            text: &s[..end],
        });
    }
    for (i, (_, end, count)) in cuts.iter().enumerate() {
        let stop = cuts.get(i + 1).map_or(s.len(), |(start, _, _)| *start);
        segments.push(Segment {
            marker: Some(*count),
            text: &s[*end..stop],
        });
    }
    segments
}

fn advance(expected: u32, count: Count) -> u32 {
    match count {
        Count::Nth(n) => n + 1,
        Count::Other => expected + 1,
        Count::Last | Count::Sequence => expected,
    }
}

/// True when the sentence itself counts out two or more items.
fn holds_enumeration(sentence: &str) -> bool {
    let mut expected = 1;
    scan_sentence(sentence, &mut expected, false)
        .iter()
        .filter(|seg| seg.marker.is_some_and(Count::counted))
        .count()
        >= 2
}

/// True when the sentence opens with an enumerator, for keeping a spoken
/// enumeration in one paragraph.
pub fn opens_with_enumerator(sentence: &str) -> bool {
    marker_at(sentence.trim(), 1, true).is_some()
}

// ---------------------------------------------------------------- layout

/// A dictation laid out as prose, a list, and more prose.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub lead: String,
    pub items: Vec<String>,
    pub tail: String,
    /// The speaker counted the items ("first", "number two", "one is").
    pub counted: bool,
}

impl Layout {
    /// The list as text, with `prose` applied to the lead-in and the tail.
    pub fn render(&self, style: Style, prose: impl Fn(&str) -> String) -> String {
        let list = self
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| match style {
                Style::Numbered => format!("{}. {item}", i + 1),
                Style::Bullets => format!("- {item}"),
                Style::Tasks => format!("- [ ] {item}"),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut blocks = Vec::new();
        if !self.lead.is_empty() {
            blocks.push(prose(&self.lead));
        }
        blocks.push(list);
        if !self.tail.is_empty() {
            blocks.push(prose(&self.tail));
        }
        blocks.join("\n\n")
    }
}

/// Lays out sentences whose roles Jev chose. None unless the roles describe
/// one clear list: every role answered, at least one confident item, and at
/// least two items once split.
pub fn layout_from_roles(sentences: &[String], roles: &[Option<(Role, f64)>]) -> Option<Layout> {
    if sentences.is_empty() || roles.len() != sentences.len() || roles.iter().any(Option::is_none) {
        return None;
    }
    // A sentence that counts out several items itself ("the issues are, one,
    // ..., two, ...") is an enumeration whatever role Jev gave it.
    let roles: Vec<(Role, f64)> = roles
        .iter()
        .flatten()
        .zip(sentences)
        .map(|(&(role, c), s)| {
            if holds_enumeration(s) {
                (Role::Item, 1.0)
            } else {
                (role, c)
            }
        })
        .collect();
    let first = roles.iter().position(|(r, _)| *r == Role::Item)?;
    let last = roles.iter().rposition(|(r, _)| *r == Role::Item)?;
    if !roles
        .iter()
        .any(|(r, c)| *r == Role::Item && *c >= ITEM_CONFIDENCE)
    {
        return None;
    }
    // A sentence Jev put between two items belongs to the list (usually a
    // continuation of the item before it).
    let segments: Vec<(&str, Role)> = sentences
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let role = if i < first {
                Role::LeadIn
            } else if i <= last {
                Role::Item
            } else {
                Role::After
            };
            (s.as_str(), role)
        })
        .collect();
    layout(&segments)
}

/// Lays out ordered text segments. Lead-in and "before" segments open the
/// text, "after" segments close it, and items are split at the speaker's
/// enumerators. Returns None when fewer than two items come out or the
/// layout would lose any word other than an enumerator.
pub fn layout(segments: &[(&str, Role)]) -> Option<Layout> {
    let mut lead: Vec<String> = Vec::new();
    let mut tail: Vec<String> = Vec::new();
    let mut items: Vec<String> = Vec::new();

    let item_sentences: Vec<&str> = segments
        .iter()
        .filter(|(_, r)| *r == Role::Item)
        .map(|(s, _)| *s)
        .collect();
    let mut expected = 1;
    let scanned: Vec<Vec<Segment>> = item_sentences
        .iter()
        .map(|s| scan_sentence(s, &mut expected, true))
        .collect();
    let counted = scanned
        .iter()
        .flatten()
        .any(|seg| seg.marker.is_some_and(Count::counted));

    let mut seen_item = false;
    let mut scanned = scanned.into_iter();
    for (text, role) in segments {
        match role {
            Role::LeadIn | Role::Before if !seen_item => lead.push(text.trim().to_string()),
            Role::LeadIn | Role::Before | Role::After if seen_item => {
                tail.push(text.trim().to_string())
            }
            Role::After => tail.push(text.trim().to_string()),
            Role::Item => {
                seen_item = true;
                let segs = scanned.next()?;
                let whole_sentence = segs.len() == 1;
                for seg in segs {
                    let text = seg.text.trim();
                    match seg.marker {
                        Some(_) => items.push(text.to_string()),
                        None if whole_sentence && (!counted || items.is_empty()) => {
                            items.push(text.to_string())
                        }
                        // The words before the first enumerator of a sentence
                        // that holds the whole list are its lead-in.
                        None if items.is_empty() => lead.push(text.to_string()),
                        None => {
                            let last = items.last_mut().unwrap();
                            last.push(' ');
                            last.push_str(text);
                        }
                    }
                }
            }
            _ => unreachable!(),
        }
    }

    let mut items: Vec<String> = items.iter().map(|i| clean_item(i)).collect();
    if items.len() == 1 && !counted {
        if let Some(series) = comma_series(&items[0]) {
            items = series;
        }
    }
    if items.len() < 2 || items.iter().any(String::is_empty) {
        return None;
    }
    let mut lead = lead.join(" ");
    if let Some(stripped) = lead.strip_suffix(',') {
        lead = format!("{stripped}:");
    }
    let layout = Layout {
        lead,
        items,
        tail: tail.join(" "),
        counted,
    };
    let original: Vec<&str> = segments.iter().map(|(s, _)| *s).collect();
    keeps_words(&original.join(" "), &layout).then_some(layout)
}

/// Tidies one item: no joining comma or "and" at its end, no closing period
/// on a one-sentence item, and a capital first letter.
fn clean_item(item: &str) -> String {
    let mut s = item.trim();
    loop {
        let before = s;
        s = s.trim_end_matches([',', ';']).trim_end();
        if let Some(rest) = s.strip_suffix(" and").or_else(|| s.strip_suffix(" And")) {
            s = rest.trim_end();
        }
        if s == before {
            break;
        }
    }
    let mut out = s.to_string();
    if out.ends_with('.') && !out.ends_with("..") && !out[..out.len() - 1].contains(". ") {
        out.pop();
    }
    capitalize(&out)
}

/// Capitalizes a plain lowercase first word ("the" -> "The"); words with
/// other capitals or symbols ("iPhone", "npm-cli") are left alone.
fn capitalize(s: &str) -> String {
    let first_word = s.split_whitespace().next().unwrap_or("");
    let plain = !first_word.is_empty()
        && first_word
            .trim_end_matches([',', ':', '.', '?', '!'])
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == '\'');
    if !plain {
        return s.to_string();
    }
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Splits "Apples, oranges, bananas and grapes" into short items. Only a
/// series of three or more short parts counts, so a long sentence with a
/// few commas is never chopped up.
fn comma_series(item: &str) -> Option<Vec<String>> {
    static JOIN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i),\s+(?:and\s+)?|\s+and\s+").unwrap());
    let parts: Vec<&str> = JOIN.split(item).map(str::trim).collect();
    let short = parts
        .iter()
        .all(|p| !p.is_empty() && p.split_whitespace().count() <= 4);
    (parts.len() >= 3 && short).then(|| parts.iter().map(|p| clean_item(p)).collect())
}

/// Words the layout may drop: enumerators and the joining words around them.
fn strippable(word: &str) -> bool {
    const JOINERS: &[&str] = &[
        "and",
        "the",
        "other",
        "is",
        "being",
        "was",
        "would",
        "will",
        "be",
        "then",
        "also",
        "plus",
        "additionally",
        "after",
        "that",
        "next",
        "finally",
        "lastly",
        "last",
        "but",
        "not",
        "least",
        "number",
        "step",
        "point",
        "item",
    ];
    let ordinal = ORDINALS
        .iter()
        .any(|o| word == *o || word.strip_suffix("ly") == Some(o));
    JOINERS.contains(&word)
        || ordinal
        || NUMBERS.contains(&word)
        || SLOT_NOUNS.split('|').any(|n| n == word)
        || word.chars().all(|c| c.is_ascii_digit())
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// True when the layout holds every word of `original` in order, except
/// enumerator words that were stripped.
fn keeps_words(original: &str, layout: &Layout) -> bool {
    let kept = [layout.lead.as_str(), &layout.items.join(" "), &layout.tail].join(" ");
    only_enumerators_dropped(original, &kept)
}

/// True when `output` holds every word of `original` in order and nothing
/// else, except enumerator words ("first", "number two", "one is") that were
/// dropped. List markup ("1.", "- ") carries no words.
pub fn only_enumerators_dropped(original: &str, output: &str) -> bool {
    let mut kept = words(output)
        .into_iter()
        .filter(|w| !w.chars().all(|c| c.is_ascii_digit()))
        .peekable();
    for word in words(original) {
        if kept.peek() == Some(&word) {
            kept.next();
        } else if !strippable(&word) {
            return false;
        }
    }
    kept.next().is_none()
}

// ---------------------------------------------------------------- transcript cleanup

/// Drops the "- " Whisper sometimes puts before a transcript (a dialogue
/// marker), which would otherwise show as a one-item bullet list. Only a
/// single line of prose loses it; text with real list lines is kept.
pub fn strip_stray_dash(text: &str) -> &str {
    let trimmed = text.trim_start();
    match trimmed.strip_prefix("- ") {
        Some(rest)
            if !rest.contains('\n') && rest.chars().next().is_some_and(char::is_alphanumeric) =>
        {
            rest
        }
        _ => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(list: &[&str]) -> Vec<String> {
        list.iter().map(|x| x.to_string()).collect()
    }

    fn roles(list: &[Role]) -> Vec<Option<(Role, f64)>> {
        list.iter().map(|r| Some((*r, 0.95))).collect()
    }

    use Role::*;

    #[test]
    fn markers_strip_only_enumerator_words() {
        let cases = [
            ("First is cost", Some("cost")),
            ("Secondly, the colors clash.", Some("the colors clash.")),
            ("The first one is that it is slow", Some("that it is slow")),
            ("Number three, bread.", Some("bread.")),
            ("Step two, turn on dark mode.", Some("turn on dark mode.")),
            ("Second step, the schema.", Some("the schema.")),
            (
                "And lastly, the spacing is uneven.",
                Some("the spacing is uneven."),
            ),
            ("First I opened the app", Some("I opened the app")),
            ("Then book the flights.", Some("book the flights.")),
            // Pointing at an object, not counting.
            ("The first image is red", None),
            ("First draft is ready", None),
            ("The second I saw it, I knew", None),
            ("Last week we shipped", None),
            ("Next week we ship", None),
        ];
        for (sentence, want) in cases {
            let got = marker_at(sentence, 1, true).map(|(len, _)| &sentence[len..]);
            assert_eq!(got, want, "{sentence}");
        }
    }

    #[test]
    fn a_sentence_splits_at_continued_counts_only() {
        let mut expected = 1;
        let segs = scan_sentence(
            "One is B2B, the second one is B2C and third one is both.",
            &mut expected,
            true,
        );
        let texts: Vec<&str> = segs.iter().map(|s| s.text.trim()).collect();
        assert_eq!(texts, ["B2B,", "B2C", "both."]);

        let mut expected = 1;
        let segs = scan_sentence(
            "The issues are, one, the login fails, two, the page is slow, three, the logo is blurry.",
            &mut expected,
            true,
        );
        assert_eq!(segs[0].marker, None);
        assert_eq!(segs.len(), 4);

        // "the fourth slide" is not a clause start; one "one," is not a count.
        let mut expected = 1;
        assert_eq!(
            scan_sentence(
                "I really like the fourth slide, and the first one too.",
                &mut expected,
                true
            )
            .len(),
            1
        );
        let mut expected = 1;
        assert_eq!(
            scan_sentence("I have one, maybe more ideas.", &mut expected, true).len(),
            1
        );
    }

    #[test]
    fn roles_lay_out_lead_items_and_tail() {
        let sentences = s(&[
            "Okay, a couple of things here.",
            "Firstly, the voiceover is not working.",
            "Secondly, I cannot drag clips.",
            "Thirdly, undo does not work.",
            "Fix these and tell me when done.",
        ]);
        let layout =
            layout_from_roles(&sentences, &roles(&[LeadIn, Item, Item, Item, After])).unwrap();
        assert_eq!(layout.lead, "Okay, a couple of things here.");
        assert_eq!(
            layout.items,
            [
                "The voiceover is not working",
                "I cannot drag clips",
                "Undo does not work"
            ]
        );
        assert_eq!(layout.tail, "Fix these and tell me when done.");
        assert!(layout.counted);
        assert_eq!(
            layout.render(Style::Numbered, str::to_string),
            "Okay, a couple of things here.\n\n1. The voiceover is not working\n2. I cannot drag clips\n3. Undo does not work\n\nFix these and tell me when done."
        );
    }

    #[test]
    fn unmarked_sentences_continue_a_counted_item() {
        let sentences = s(&[
            "Firstly, the export fails.",
            "It shows a blank error.",
            "Secondly, the app is slow.",
        ]);
        let layout = layout_from_roles(&sentences, &roles(&[Item, Item, Item])).unwrap();
        assert_eq!(
            layout.items,
            [
                "The export fails. It shows a blank error.",
                "The app is slow"
            ]
        );
    }

    #[test]
    fn a_sentence_that_counts_items_is_a_list_whatever_its_role() {
        let sentences = s(&["The issues are, one, the login fails, two, the page is slow."]);
        let layout = layout_from_roles(&sentences, &[Some((LeadIn, 0.49))]).unwrap();
        assert_eq!(layout.lead, "The issues are:");
        assert_eq!(layout.items, ["The login fails", "The page is slow"]);
        // One counted clause is not enough to override Jev.
        let sentences = s(&["Make a list.", "I have only tried the first one before."]);
        assert!(layout_from_roles(&sentences, &roles(&[LeadIn, After])).is_none());
    }

    #[test]
    fn inconsistent_or_unsure_roles_are_refused() {
        let sentences = s(&["Firstly, the export fails.", "Secondly, the app is slow."]);
        // Missing role.
        assert!(layout_from_roles(&sentences, &[Some((Item, 0.9)), None]).is_none());
        // No confident item.
        assert!(layout_from_roles(&sentences, &[Some((Item, 0.4)), Some((Item, 0.5))]).is_none());
        // Only one item.
        assert!(layout_from_roles(&sentences, &roles(&[LeadIn, Item])).is_none());
        // No item at all.
        assert!(layout_from_roles(&sentences, &roles(&[LeadIn, After])).is_none());
    }

    #[test]
    fn any_role_pattern_keeps_every_non_enumerator_word() {
        let sentences = s(&[
            "Two issues I have with the first draft.",
            "The first one is that the login page is slow.",
            "It takes ten seconds.",
            "The second one is that logout does nothing.",
            "Please fix both, then tell me.",
        ]);
        let all = [LeadIn, Item, Before, After];
        let mut checked = 0;
        for a in all {
            for b in all {
                for c in all {
                    for d in all {
                        for e in all {
                            let r = roles(&[a, b, c, d, e]);
                            if let Some(layout) = layout_from_roles(&sentences, &r) {
                                assert!(keeps_words(&sentences.join(" "), &layout));
                                checked += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(checked > 0);
    }

    #[test]
    fn comma_series_splits_short_parts_only() {
        assert_eq!(
            comma_series("Apples, oranges, bananas and grapes."),
            Some(s(&["Apples", "Oranges", "Bananas", "Grapes"]))
        );
        assert_eq!(comma_series("I want to buy milk, eggs and bread"), None);
        assert_eq!(comma_series("Milk and eggs"), None);
    }

    #[test]
    fn words_lost_beyond_enumerators_refuse_the_layout() {
        let layout = Layout {
            lead: String::new(),
            items: s(&["Cost", "UX"]),
            tail: String::new(),
            counted: true,
        };
        assert!(keeps_words("First is cost, second is UX.", &layout));
        assert!(!keeps_words("First is cost, second is good UX.", &layout));
        assert!(!keeps_words("First is cost.", &layout));
    }

    #[test]
    fn gate_nominates_announced_or_counted_dictations_only() {
        let gate = |text: &str| is_candidate(text, &crate::format::split_sentences(text));
        assert!(gate("A couple of things here. The voiceover is broken."));
        assert!(gate("Two issues I have. The login is slow."));
        assert!(gate("Here is my grocery list. Number one, milk."));
        assert!(gate("First, fix the timeout. Second, add stats."));
        assert!(gate(
            "The issues are, one, the login fails, two, the page is slow."
        ));
        assert!(gate(
            "Things to do today. Call the bank. Then book the flights."
        ));
        // The bare word "list" is usually an instruction to an agent.
        assert!(!gate("List down all the statuses for me."));
        assert!(!gate("Give me just a list of names."));
        assert!(!gate("Firstly I want to say this build is fast."));
        assert!(!gate("Thanks a lot, that works perfectly now."));
        let long = "This is one sentence. ".repeat(MAX_SENTENCES + 1) + "A couple of things.";
        assert!(!gate(&long));
    }

    #[test]
    fn stray_dash_is_dropped_from_single_line_prose() {
        assert_eq!(
            strip_stray_dash("- Okay, I'm testing Bolo."),
            "Okay, I'm testing Bolo."
        );
        assert_eq!(strip_stray_dash("Okay, so"), "Okay, so");
        assert_eq!(strip_stray_dash("-5 degrees outside"), "-5 degrees outside");
        assert_eq!(strip_stray_dash("- one\n- two"), "- one\n- two");
        assert_eq!(strip_stray_dash("- -"), "- -");
        assert_eq!(strip_stray_dash("- "), "- ");
    }
}
