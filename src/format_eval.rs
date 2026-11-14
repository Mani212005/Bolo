//! `bolo eval-format`: measures the local code check against labeled cases,
//! alone and with Jev deciding the pieces it is unsure about, using the same
//! request the daemon sends. Answers "does Jev earn its latency?" with data.
//!
//! `bolo eval-format --lists` does the same for spoken lists on
//! `format_list_cases.json`, and checks the acceptance bar.

use crate::config::Config;
use crate::format::{classify_code, language_of, CodeStyle, Options, Plan, Verdict, ACT_ABOVE};
use crate::vocab::TranscriptPiece;
use serde::Deserialize;
use std::time::Instant;

#[derive(Deserialize)]
struct Case {
    name: String,
    code: bool,
    lang: Option<String>,
    text: String,
}

struct Outcome {
    local: Verdict,
    jev: Option<(f64, Option<String>)>,
    latency_ms: Option<u64>,
}

pub async fn run(cfg: &Config, use_jev: bool, lists: bool) -> anyhow::Result<()> {
    if lists {
        return run_lists(cfg, use_jev).await;
    }
    let cases: Vec<Case> = serde_json::from_str(include_str!("format_eval_cases.json"))?;
    let target = if use_jev {
        let t = cfg
            .formatting
            .jev
            .resolve()
            .ok_or_else(|| anyhow::anyhow!("--jev needs a Jev API key"))?;
        println!(
            "Jev: {:?} model={} timeout={}ms",
            t.provider, t.model, cfg.formatting.jev.timeout_ms
        );
        Some(t)
    } else {
        None
    };

    let mut outcomes = Vec::new();
    for case in &cases {
        let local = classify_code(&case.text);
        let (jev, latency_ms) = match &target {
            Some(t) => {
                let plan = Plan::for_eval(&case.text);
                let request = plan
                    .jev_request(None, &t.model)
                    .expect("eval plan has a question");
                let started = Instant::now();
                let raw = crate::jev::evaluate(
                    &request,
                    &t.api_key,
                    cfg.formatting.jev.timeout_ms,
                    t.provider,
                )
                .await;
                let ms = started.elapsed().as_millis() as u64;
                match raw {
                    Ok(raw) => (plan.eval_answer(&plan.parse_answers(&raw)), Some(ms)),
                    Err(e) => {
                        eprintln!("  {}: Jev failed: {e:#}", case.name);
                        (None, Some(ms))
                    }
                }
            }
            None => (None, None),
        };
        outcomes.push(Outcome {
            local,
            jev,
            latency_ms,
        });
    }

    println!(
        "\n{:<34} {:>5} {:>8} {:>13} {:>8} {:>10}",
        "case", "code", "local", "jev p(code)", "lang ok", "latency"
    );
    for (case, o) in cases.iter().zip(&outcomes) {
        let local = match o.local {
            Verdict::Code => "code",
            Verdict::Prose => "prose",
            Verdict::Unsure => "unsure",
        };
        let jev = o
            .jev
            .as_ref()
            .map_or("-".to_string(), |(p, _)| format!("{p:.2}"));
        let lang_ok = match (&case.lang, case.code) {
            (Some(want), true) => {
                let local_lang = language_of(&case.text);
                let jev_lang = o
                    .jev
                    .as_ref()
                    .and_then(|(_, l)| l.clone())
                    .unwrap_or_default();
                format!(
                    "{}/{}",
                    mark(&local_lang == want),
                    if target.is_some() {
                        mark(&jev_lang == want)
                    } else {
                        "-"
                    }
                )
            }
            _ => "".to_string(),
        };
        let latency = o.latency_ms.map_or("-".to_string(), |ms| format!("{ms}ms"));
        println!(
            "{:<34} {:>5} {:>8} {:>13} {:>8} {:>10}",
            truncate(&case.name, 34),
            case.code,
            local,
            jev,
            lang_ok,
            latency
        );
    }

    let n = cases.len() as f64;
    let local_only = |c: &Case, o: &Outcome| (o.local == Verdict::Code) == c.code;
    let unsure = outcomes
        .iter()
        .filter(|o| o.local == Verdict::Unsure)
        .count();
    println!(
        "\nlocal only (unsure left as-is): {:.0}% correct, {unsure} unsure",
        pct(&cases, &outcomes, local_only, n)
    );
    if target.is_some() {
        let jev_says = |o: &Outcome| o.jev.as_ref().map(|(p, _)| *p >= ACT_ABOVE);
        let combined = |c: &Case, o: &Outcome| match o.local {
            Verdict::Unsure => jev_says(o).unwrap_or(false) == c.code,
            v => (v == Verdict::Code) == c.code,
        };
        let jev_alone = |c: &Case, o: &Outcome| jev_says(o) == Some(c.code);
        println!(
            "local + Jev for unsure (shipped): {:.0}% correct",
            pct(&cases, &outcomes, combined, n)
        );
        println!(
            "Jev alone on every case:          {:.0}% correct",
            pct(&cases, &outcomes, jev_alone, n)
        );
        let lat: Vec<u64> = outcomes.iter().filter_map(|o| o.latency_ms).collect();
        if !lat.is_empty() {
            let mut sorted = lat.clone();
            sorted.sort();
            println!(
                "Jev latency: median {}ms, max {}ms over {} calls",
                sorted[sorted.len() / 2],
                sorted[sorted.len() - 1],
                sorted.len()
            );
        }
    }
    Ok(())
}

fn pct(cases: &[Case], outcomes: &[Outcome], ok: impl Fn(&Case, &Outcome) -> bool, n: f64) -> f64 {
    100.0 * cases.iter().zip(outcomes).filter(|(c, o)| ok(c, o)).count() as f64 / n
}

fn mark(ok: bool) -> &'static str {
    if ok {
        "y"
    } else {
        "n"
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max - 1).collect::<String>() + "…"
    }
}

// ---------------------------------------------------------------- spoken lists

/// One synthetic dictation for the list eval. `roles` and `style` are the
/// answers an ideal Jev gives, for tests that run without the network.
#[derive(Deserialize, Debug)]
pub struct ListCase {
    pub name: String,
    pub list: bool,
    pub text: String,
    #[serde(default)]
    #[cfg_attr(not(test), allow(dead_code))]
    pub roles: Vec<String>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub style: Option<String>,
    pub want: Option<String>,
    #[serde(default)]
    pub alts: Vec<String>,
    /// Why a real list is expected to stay prose (out of scope).
    pub known_miss: Option<String>,
}

pub fn list_cases() -> Vec<ListCase> {
    serde_json::from_str(include_str!("format_list_cases.json")).expect("valid list cases")
}

/// Bar a change to the list path must meet on the cases before it ships.
const BAR_LISTS: usize = 14;

/// Formatting options the daemon uses by default.
fn list_options() -> Options {
    Options {
        smart_code: true,
        paragraphs: true,
        list_cues: true,
    }
}

pub fn list_plan(case: &ListCase) -> Plan {
    Plan::new(
        &[TranscriptPiece::Spoken(case.text.clone())],
        list_options(),
    )
}

fn same_words_and_spacing(a: &str, b: &str) -> bool {
    a.split_whitespace().eq(b.split_whitespace())
}

/// How one rendered case scores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListScore {
    /// A list came out exactly as wanted, or a non-list was left as spoken.
    pub correct: bool,
    /// The text differs from the dictation beyond whitespace.
    pub changed: bool,
    /// Every word except enumerators survived, in order.
    pub words_kept: bool,
}

pub fn score(case: &ListCase, out: &str) -> ListScore {
    let changed = !same_words_and_spacing(out, &case.text);
    let correct = if case.list {
        case.want.as_deref() == Some(out) || case.alts.iter().any(|a| a == out)
    } else {
        !changed
    };
    ListScore {
        correct,
        changed,
        words_kept: crate::spoken_list::only_enumerators_dropped(&case.text, out),
    }
}

async fn run_lists(cfg: &Config, use_jev: bool) -> anyhow::Result<()> {
    let cases = list_cases();
    let target = if use_jev {
        let t = cfg
            .formatting
            .jev
            .resolve()
            .ok_or_else(|| anyhow::anyhow!("--jev needs a Jev API key"))?;
        println!(
            "Jev: {:?} model={} timeout={}ms",
            t.provider, t.model, cfg.formatting.jev.timeout_ms
        );
        Some(t)
    } else {
        println!("Local only (no Jev): candidates stay as spoken.");
        None
    };

    let mut rows = Vec::new();
    let mut latencies = Vec::new();
    for case in &cases {
        let plan = list_plan(case);
        let mut p_list = "-".to_string();
        let mut latency = "-".to_string();
        let mut raw_answers = None;
        let answers = match (&target, plan.jev_request(None, "")) {
            (Some(t), Some(_)) => {
                let request = plan.jev_request(None, &t.model).unwrap();
                let started = Instant::now();
                let raw = crate::jev::evaluate(
                    &request,
                    &t.api_key,
                    cfg.formatting.jev.timeout_ms,
                    t.provider,
                )
                .await;
                let ms = started.elapsed().as_millis() as u64;
                latencies.push(ms);
                latency = format!("{ms}ms");
                match raw {
                    Ok(raw) => {
                        let answers = plan.parse_answers(&raw);
                        if let Some(p) = answers.list_probability(0) {
                            p_list = format!("{p:.2}");
                        }
                        raw_answers = Some(raw);
                        Some(answers)
                    }
                    Err(e) => {
                        eprintln!("  {}: Jev failed: {e:#}", case.name);
                        None
                    }
                }
            }
            _ => None,
        };
        let out = plan.render(answers.as_ref(), CodeStyle::Fenced);
        let sc = score(case, &out);
        if !sc.correct {
            if let Some(raw) = &raw_answers {
                eprintln!("  {}: Jev answered {raw}", case.name);
            }
        }
        rows.push((case, plan.needs_jev(), p_list, latency, sc, out));
    }

    println!(
        "\n{:<46} {:>5} {:>6} {:>7} {:>8} {:>6} {:>8}",
        "case", "list", "asked", "p(list)", "result", "words", "latency"
    );
    for (case, asked, p, latency, sc, _) in &rows {
        let result = match (case.list, sc.correct) {
            (true, true) => "ok",
            (true, false) if case.known_miss.is_some() => "miss*",
            (true, false) => "MISS",
            (false, true) => "ok",
            (false, false) => "CHANGED",
        };
        println!(
            "{:<46} {:>5} {:>6} {:>7} {:>8} {:>6} {:>8}",
            truncate(&case.name, 46),
            case.list,
            if *asked { "yes" } else { "no" },
            p,
            result,
            if sc.words_kept { "ok" } else { "LOST" },
            latency
        );
    }
    println!("\n(miss* = known miss, out of scope)\n\nOutputs that are lists or wrong:");
    for (case, _, _, _, sc, out) in &rows {
        if sc.changed || !sc.correct {
            println!(
                "--- {} ({})",
                case.name,
                if sc.correct { "ok" } else { "wrong" }
            );
            println!("{out}");
            if !sc.correct {
                if let Some(want) = &case.want {
                    println!("  want:\n{want}");
                }
            }
        }
    }

    let lists = rows.iter().filter(|r| r.0.list).count();
    let lists_ok = rows.iter().filter(|r| r.0.list && r.4.correct).count();
    let others = rows.len() - lists;
    let changed = rows.iter().filter(|r| !r.0.list && r.4.changed).count();
    let lost = rows.iter().filter(|r| !r.4.words_kept).count();
    println!("\nlists formatted as wanted: {lists_ok}/{lists}");
    println!("non-lists changed:         {changed}/{others}");
    println!("cases that lost words:     {lost}");
    if !latencies.is_empty() {
        latencies.sort();
        println!(
            "Jev latency: median {}ms, max {}ms over {} calls",
            latencies[latencies.len() / 2],
            latencies[latencies.len() - 1],
            latencies.len()
        );
    }
    if target.is_some() {
        let met = lists_ok >= BAR_LISTS && changed == 0 && lost == 0;
        println!(
            "bar (>= {BAR_LISTS}/{lists} lists, 0 non-lists changed, 0 lost words): {}",
            if met { "met" } else { "NOT met" }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    /// The answers an ideal Jev gives for a case: list or not, its style,
    /// and the case's per-sentence roles.
    fn ideal_answers(case: &ListCase) -> Value {
        let mut answers = serde_json::Map::new();
        let p = if case.list { 0.95 } else { 0.05 };
        answers.insert("list_0".into(), json!({"type": "noul", "noul": p}));
        if let Some(style) = &case.style {
            answers.insert(
                "list_style_0".into(),
                json!({"type": "choice", "choice": style, "confidence": 0.9}),
            );
        }
        for (k, role) in case.roles.iter().enumerate() {
            answers.insert(
                format!("role_0_S{}", k + 1),
                json!({"type": "choice", "choice": role, "confidence": 0.9}),
            );
        }
        Value::Object(answers)
    }

    #[test]
    fn case_roles_match_the_sentences_jev_sees() {
        for case in list_cases() {
            if case.roles.is_empty() {
                continue;
            }
            let plan = list_plan(&case);
            let req = plan
                .jev_request(None, "jev-latest")
                .unwrap_or_else(|| panic!("{}: a list with roles must reach Jev", case.name));
            let sentences = req["state"]["pieces"][0]["sentences"].as_object().unwrap();
            assert_eq!(sentences.len(), case.roles.len(), "{}", case.name);
        }
    }

    #[test]
    fn ideal_jev_answers_render_every_in_scope_list() {
        let mut ok = 0;
        for case in list_cases().iter().filter(|c| c.list) {
            let plan = list_plan(case);
            let answers = plan.parse_answers(&ideal_answers(case));
            let out = plan.render(Some(&answers), CodeStyle::Fenced);
            let sc = score(case, &out);
            assert!(sc.words_kept, "{}: lost words\n{out}", case.name);
            if case.known_miss.is_some() {
                assert_eq!(
                    out, case.text,
                    "{}: known miss must stay as spoken",
                    case.name
                );
            } else {
                assert_eq!(Some(out.as_str()), case.want.as_deref(), "{}", case.name);
                ok += 1;
            }
        }
        assert!(ok >= BAR_LISTS, "only {ok} lists render");
    }

    #[test]
    fn long_answers_to_nine_questions_keep_every_item_whole() {
        for case in list_cases().iter().filter(|c| c.name.starts_with('L')) {
            let plan = list_plan(case);
            let answers = plan.parse_answers(&ideal_answers(case));
            let out = plan.render(Some(&answers), CodeStyle::Fenced);
            let items: Vec<&str> = out
                .lines()
                .filter(|l| {
                    l.split_once(". ")
                        .is_some_and(|(n, _)| n.parse::<u32>().is_ok())
                })
                .collect();
            assert_eq!(items.len(), 9, "{}:\n{out}", case.name);
            for (i, item) in items.iter().enumerate() {
                let body = item.split_once(". ").unwrap().1;
                assert_eq!(item.split_once(". ").unwrap().0, (i + 1).to_string());
                let first = body.split_whitespace().next().unwrap().to_lowercase();
                // No stray slot noun, enumerator or half sentence in front.
                assert!(
                    !["thing", "point", "question", "number", "is", "and", "so", "okay"]
                        .contains(&first.as_str()),
                    "{}: item {} starts with {first:?}",
                    case.name,
                    i + 1
                );
                assert!(
                    !body.to_lowercase().contains("thing is"),
                    "{}: {body}",
                    case.name
                );
            }
        }
    }

    #[test]
    fn prose_full_of_ordinals_stays_as_spoken_even_if_every_sentence_is_called_an_item() {
        for case in list_cases().iter().filter(|c| c.name.starts_with("N12")) {
            let plan = list_plan(case);
            let n = plan.jev_request(None, "m").unwrap()["state"]["pieces"][0]["sentences"]
                .as_object()
                .unwrap()
                .len();
            let mut answers = serde_json::Map::new();
            answers.insert("list_0".into(), json!({"type": "noul", "noul": 0.99}));
            for k in 1..=n {
                answers.insert(
                    format!("role_0_S{k}"),
                    json!({"type": "choice", "choice": "item", "confidence": 0.99}),
                );
            }
            let out = plan.render(
                Some(&plan.parse_answers(&Value::Object(answers))),
                CodeStyle::Fenced,
            );
            assert!(!score(case, &out).changed, "{out}");
        }
    }

    #[test]
    fn non_lists_are_never_changed() {
        for case in list_cases().iter().filter(|c| !c.list) {
            let plan = list_plan(case);
            // Jev says no, and even if Jev wrongly said yes with no roles.
            for answers in [
                ideal_answers(case),
                json!({"list_0": {"type": "noul", "noul": 0.99}}),
            ] {
                let out = plan.render(Some(&plan.parse_answers(&answers)), CodeStyle::Fenced);
                assert!(!score(case, &out).changed, "{}:\n{out}", case.name);
            }
        }
    }

    #[test]
    fn without_jev_dictations_stay_as_spoken() {
        for case in list_cases() {
            let plan = list_plan(&case);
            let out = plan.render(None, CodeStyle::Fenced);
            let sc = score(&case, &out);
            assert!(sc.words_kept, "{}", case.name);
            // Only an explicit "bullet list" cue formats without Jev.
            if case.name.starts_with("P11") {
                assert!(sc.correct, "{}:\n{out}", case.name);
            } else {
                assert!(!sc.changed, "{}:\n{out}", case.name);
            }
        }
    }

    #[test]
    fn wrong_roles_never_lose_words() {
        let patterns: [&[&str]; 4] = [
            &["item"],
            &["lead_in"],
            &["after", "item"],
            &["item", "lead_in", "item", "after"],
        ];
        for case in list_cases() {
            let plan = list_plan(&case);
            if !plan.needs_jev() {
                continue;
            }
            let n = plan.jev_request(None, "m").unwrap()["state"]["pieces"][0]["sentences"]
                .as_object()
                .unwrap()
                .len();
            for pattern in patterns {
                let mut answers = serde_json::Map::new();
                answers.insert("list_0".into(), json!({"type": "noul", "noul": 0.9}));
                for k in 0..n {
                    answers.insert(
                        format!("role_0_S{}", k + 1),
                        json!({"type": "choice", "choice": pattern[k % pattern.len()], "confidence": 0.9}),
                    );
                }
                let out = plan.render(
                    Some(&plan.parse_answers(&Value::Object(answers))),
                    CodeStyle::Fenced,
                );
                assert!(
                    score(&case, &out).words_kept,
                    "{} with {pattern:?}:\n{out}",
                    case.name
                );
            }
        }
    }
}
