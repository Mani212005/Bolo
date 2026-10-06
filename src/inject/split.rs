/// Per-paste budget that every supported terminal agent shows verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasteBudget {
    /// UTF-16 code units (Claude Code measures JavaScript string length).
    pub max_units: usize,
    /// Line breaks inside one paste.
    pub max_breaks: usize,
}

/// Splits `text` into pieces that each fit `budget`, so a terminal agent shows
/// every piece verbatim instead of collapsing it into a placeholder.
///
/// Rules: a newline always leads the next piece and never ends one (Antigravity
/// drops a paste's trailing newline); long lines break just after a space; a
/// word longer than the budget is cut at a char boundary. The pieces always
/// concatenate back to `text` exactly.
pub fn split_for_terminal(text: &str, budget: PasteBudget) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut units = 0;
        let mut breaks = 0;
        let mut end = 0; // rest[..end] is the longest prefix within the budget
        let mut cut = 0; // best cut point so far; 0 = none
        let mut prev = None;
        for (i, ch) in rest.char_indices() {
            let next_units = units + ch.len_utf16();
            let next_breaks = breaks + usize::from(ch == '\n');
            if next_units > budget.max_units || next_breaks > budget.max_breaks {
                break;
            }
            if ch == '\n' && i > 0 && prev != Some('\n') {
                cut = i; // before a newline run: the newline leads the next piece
            }
            units = next_units;
            breaks = next_breaks;
            end = i + ch.len_utf8();
            if ch == ' ' {
                cut = end; // just after a space
            }
            prev = Some(ch);
        }
        if end == rest.len() {
            pieces.push(rest);
            break;
        }
        if rest[end..].starts_with('\n') && prev != Some('\n') {
            cut = end; // the budget ran out right at a newline: cut there
        }
        if cut == 0 {
            // One word longer than the budget, or a newline run longer than
            // max_breaks. Always make progress, even with a zero budget.
            cut = if end == 0 {
                rest.chars().next().map_or(rest.len(), char::len_utf8)
            } else {
                end
            };
        }
        pieces.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    const CC: PasteBudget = PasteBudget {
        max_units: 800,
        max_breaks: 2,
    };

    fn units(s: &str) -> usize {
        s.encode_utf16().count()
    }

    fn check(text: &str, budget: PasteBudget) -> Vec<&str> {
        let pieces = split_for_terminal(text, budget);
        assert_eq!(pieces.concat(), text, "pieces must rejoin exactly");
        for (k, p) in pieces.iter().enumerate() {
            assert!(!p.is_empty());
            if budget.max_units > 0 {
                assert!(units(p) <= budget.max_units, "piece {k} over units: {p:?}");
            }
            assert!(p.matches('\n').count() <= budget.max_breaks.max(1));
        }
        pieces
    }

    #[test]
    fn short_text_is_one_piece() {
        assert_eq!(check("hello world", CC), vec!["hello world"]);
        assert_eq!(check("", CC), Vec::<&str>::new());
    }

    #[test]
    fn newline_leads_the_next_piece() {
        let text = "Para one.\n\nPara two.\n\n- a\n- b\n- c\n\nEnd.";
        assert_eq!(
            check(text, CC),
            vec![
                "Para one.\n\nPara two.",
                "\n\n- a",
                "\n- b\n- c",
                "\n\nEnd."
            ]
        );
    }

    #[test]
    fn long_line_breaks_after_a_space() {
        let text = "abcd ".repeat(400); // 2000 units, no newlines
        let pieces = check(&text, CC);
        assert_eq!(pieces.len(), 3);
        assert!(pieces[..2]
            .iter()
            .all(|p| p.ends_with(' ') && units(p) == 800));
    }

    #[test]
    fn counts_utf16_units_like_claude_code() {
        let text = "\u{1F600}".repeat(401); // 401 chars, 802 UTF-16 units
        let pieces = check(&text, CC);
        assert_eq!(pieces.len(), 2);
        assert_eq!(units(pieces[0]), 800);
    }

    #[test]
    fn exactly_at_the_limits_is_one_piece() {
        let text = format!(
            "{}\n{}\n{}",
            "a".repeat(300),
            "b".repeat(300),
            "c".repeat(198)
        );
        assert_eq!(units(&text), 800);
        assert_eq!(check(&text, CC).len(), 1);
    }

    #[test]
    fn huge_word_is_hard_cut_on_a_char_boundary() {
        let text = "é".repeat(1700);
        assert_eq!(check(&text, CC).len(), 3);
    }

    #[test]
    fn newline_run_longer_than_budget_still_progresses() {
        check("a\n\n\n\nb", CC);
    }

    #[test]
    fn no_piece_ends_with_newline_for_formatted_dictation() {
        let para = "This is a sentence that goes on for a while. ".repeat(6);
        let text = [para.as_str(); 7].join("\n\n") + "\n\n- one\n- two\n- three";
        for p in check(&text, CC) {
            assert!(!p.ends_with('\n'), "{p:?}");
        }
    }

    #[test]
    fn fuzz_invariants() {
        // Small deterministic LCG so the tests need no dependencies.
        let mut seed: u64 = 7;
        let mut next = |n: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        let alphabet = ['a', 'b', ' ', ' ', '\n', 'é', '\u{1F600}', '.', 'x'];
        for _ in 0..20_000 {
            let len = next(2500) as usize;
            let text: String = (0..len)
                .map(|_| alphabet[next(alphabet.len() as u64) as usize])
                .collect();
            let budget = PasteBudget {
                max_units: [10, 50, 150, 800][next(4) as usize],
                max_breaks: [1, 2, 14][next(3) as usize],
            };
            check(&text, budget);
        }
    }
}
