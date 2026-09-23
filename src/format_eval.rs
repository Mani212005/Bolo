//! `bolo eval-format`: measures the local code check against labeled cases,
//! alone and with Jev deciding the pieces it is unsure about, using the same
//! request the daemon sends. Answers "does Jev earn its latency?" with data.

use crate::config::Config;
use crate::format::{classify_code, language_of, Plan, Verdict, ACT_ABOVE};
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

pub async fn run(cfg: &Config, use_jev: bool) -> anyhow::Result<()> {
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
