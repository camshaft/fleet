//! Render a board reply into plain, speakable text for the TTS engine.
//!
//! George (the board-native voice agent, #316) posts ordinary board messages, which may carry Markdown —
//! headings, lists, links, emphasis, fenced code. The synthesizer should read *words*, not syntax, so this
//! strips Markdown down to prose before synthesis. Best-effort and conservative: when a construct is
//! ambiguous the text is kept rather than dropped, and a reply that is only a code block renders empty (the
//! caller simply speaks nothing). Pure and unit-tested — no audio, no board I/O.

use bridge_core::OutboundReflect;

/// Render a board reflect's body to plain speakable text (Markdown stripped).
pub fn render_reply(reflect: &OutboundReflect) -> String {
    strip_markdown(&reflect.body)
}

/// Strip Markdown to plain prose suitable for speaking. Fenced code blocks are dropped entirely (reading
/// code aloud is noise); inline markup (emphasis, inline code, links, headings, list/quote markers) is
/// reduced to its visible text. Lines are joined into sentences with a period inserted only where one
/// isn't already present, so list items and paragraphs get a spoken pause without doubling punctuation.
pub fn strip_markdown(input: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut in_fence = false;
    for raw in input.lines() {
        let t = raw.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue; // the fence marker line itself is never spoken
        }
        if in_fence {
            continue; // code content is not spoken
        }
        let line = strip_line(raw);
        let line = line.trim();
        if !line.is_empty() {
            lines.push(line.to_string());
        }
    }
    join_sentences(&lines)
}

/// Join cleaned lines into one spoken string. Insert ". " between two lines only when the previous one
/// doesn't already end in sentence-ish punctuation, so prose flows and list items still get a pause.
fn join_sentences(lines: &[String]) -> String {
    let mut out = String::new();
    for line in lines {
        if let Some(last) = out.chars().last() {
            if matches!(last, '.' | '!' | '?' | ':' | ';' | ',') {
                out.push(' ');
            } else {
                out.push_str(". ");
            }
        }
        out.push_str(line);
    }
    out
}

/// Strip one line's leading block markers (blockquote, heading, list bullet) and inline markup.
fn strip_line(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    // Leading blockquote markers (possibly nested: "> > quote").
    while let Some(rest) = s.strip_prefix('>') {
        s = rest.trim_start().to_string();
    }
    // Leading heading hashes ("### Title" -> "Title").
    while let Some(rest) = s.strip_prefix('#') {
        s = rest.to_string();
    }
    let s = s.trim_start();
    // Leading list marker: "- ", "* ", "+ ", or an ordered "N." / "N)".
    let s = strip_list_marker(s);
    // Inline: links to their text, then the remaining emphasis/code markup characters.
    let s = unlink(&s);
    strip_marks(&s)
}

/// Drop a single leading list marker if present, else return the string unchanged.
fn strip_list_marker(s: &str) -> String {
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = s.strip_prefix(bullet) {
            return rest.trim_start().to_string();
        }
    }
    // Ordered list: leading ASCII digits then "." or ")" then a space.
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let rest = &s[digits.len()..];
        for sep in [". ", ") "] {
            if let Some(after) = rest.strip_prefix(sep) {
                return after.trim_start().to_string();
            }
        }
    }
    s.to_string()
}

/// Replace Markdown links/images with their visible text: `[text](url)` -> `text`, `![alt](url)` -> `alt`.
/// A `[text]` with no immediately-following `(...)` is left as-is (a reference link or literal brackets).
fn unlink(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < n {
        // An image `![alt](url)`: skip the leading '!' and let the '[' arm handle the rest.
        if chars[i] == '!' && i + 1 < n && chars[i + 1] == '[' {
            i += 1;
            continue;
        }
        if chars[i] == '['
            && let Some(close) = find_from(&chars, i + 1, ']')
            && close + 1 < n
            && chars[close + 1] == '('
            && let Some(paren) = find_from(&chars, close + 2, ')')
        {
            out.extend(&chars[i + 1..close]); // the visible link text
            i = paren + 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Index of the next `target` char at or after `from`, if any.
fn find_from(chars: &[char], from: usize, target: char) -> Option<usize> {
    (from..chars.len()).find(|&j| chars[j] == target)
}

/// Remove the residual inline-markup characters (emphasis, inline code, strikethrough) that carry no
/// spoken meaning. Conservative: only these four, so ordinary punctuation and words are untouched.
fn strip_marks(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '*' | '_' | '`' | '~'))
        .collect()
}

/// Split already-cleaned speakable text into sentence-sized chunks for INCREMENTAL synthesis, so the
/// first chunk can start playing while later chunks are still synthesized — the batch Kokoro synth of a
/// whole reply is the dominant reply→speaker latency (#454), and this drops time-to-first-audio to the
/// first sentence. Splits after a sentence terminator (`.`/`!`/`?`) that is followed by whitespace or the
/// end of the text, so decimals ("3.14") and most inline dots don't split mid-token; a run of terminators
/// ("?!") stays with its sentence. Whitespace-only pieces are dropped; text with no terminator returns as
/// a single chunk. Pure.
pub fn split_sentences(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        cur.push(c);
        if matches!(c, '.' | '!' | '?') {
            let next = chars.get(i + 1);
            // End the sentence only when the terminator is the LAST of any run ("?!") and what follows is
            // whitespace or the end — so "3.14" and "e.g.x" don't split, but "Here. Now" does.
            let next_is_terminator = matches!(next, Some('.' | '!' | '?'));
            let ends = next.is_none_or(|n| n.is_whitespace());
            if ends && !next_is_terminator {
                let s = cur.trim();
                if !s.is_empty() {
                    out.push(s.to_string());
                }
                cur.clear();
            }
        }
    }
    let tail = cur.trim();
    if !tail.is_empty() {
        out.push(tail.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reflect(body: &str) -> OutboundReflect {
        OutboundReflect {
            channel_id: 1,
            post_seq: 1,
            author: "george".into(),
            body: body.into(),
            reply_to: None,
            external_author: None,
            metadata: None,
            parent_metadata: None,
        }
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(
            render_reply(&reflect("The build is green.")),
            "The build is green."
        );
    }

    #[test]
    fn strips_emphasis_and_inline_code() {
        assert_eq!(
            strip_markdown("Run **cargo test** with the `runtime` feature"),
            "Run cargo test with the runtime feature"
        );
    }

    #[test]
    fn links_become_their_text() {
        assert_eq!(
            strip_markdown("See [the PR](https://example.com/pr/1) for details"),
            "See the PR for details"
        );
    }

    #[test]
    fn images_become_their_alt_text() {
        assert_eq!(
            strip_markdown("![a diagram](x.png) shows it"),
            "a diagram shows it"
        );
    }

    #[test]
    fn reference_style_brackets_are_left_alone() {
        // No `(...)` follows, so it isn't a link — keep the literal text (minus the stripped marks).
        assert_eq!(strip_markdown("the array[0] value"), "the array[0] value");
    }

    #[test]
    fn split_sentences_breaks_on_terminators() {
        assert_eq!(
            split_sentences("Yes, I'm here. What do you need?"),
            vec!["Yes, I'm here.", "What do you need?"]
        );
    }

    #[test]
    fn split_sentences_single_chunk_when_no_terminator() {
        assert_eq!(split_sentences("Salt Lake City"), vec!["Salt Lake City"]);
    }

    #[test]
    fn split_sentences_keeps_a_terminator_run_together() {
        assert_eq!(split_sentences("Really?! Okay."), vec!["Really?!", "Okay."]);
    }

    #[test]
    fn split_sentences_does_not_split_decimals_or_inline_dots() {
        // A dot not followed by whitespace (a decimal) must not split.
        assert_eq!(
            split_sentences("It is 3.14 today."),
            vec!["It is 3.14 today."]
        );
    }

    #[test]
    fn split_sentences_drops_empty_and_whitespace_pieces() {
        assert!(split_sentences("").is_empty());
        assert!(split_sentences("   ").is_empty());
        assert_eq!(split_sentences("Done.   "), vec!["Done."]);
    }

    #[test]
    fn split_sentences_trailing_text_without_terminator_is_kept() {
        assert_eq!(
            split_sentences("First. Then a tail"),
            vec!["First.", "Then a tail"]
        );
    }

    #[test]
    fn headings_and_bullets_lose_their_markers_and_get_pauses() {
        let md = "# Status\n- built\n- tested";
        assert_eq!(strip_markdown(md), "Status. built. tested");
    }

    #[test]
    fn ordered_list_markers_are_stripped() {
        assert_eq!(strip_markdown("1. first\n2) second"), "first. second");
    }

    #[test]
    fn blockquote_markers_are_stripped() {
        assert_eq!(strip_markdown("> quoted line"), "quoted line");
    }

    #[test]
    fn fenced_code_blocks_are_dropped() {
        let md = "Here is how:\n```rust\nlet x = 1;\n```\nThat's it.";
        assert_eq!(strip_markdown(md), "Here is how: That's it.");
    }

    #[test]
    fn a_reply_that_is_only_code_renders_empty() {
        assert_eq!(strip_markdown("```\nls -la\n```"), "");
    }

    #[test]
    fn sentence_join_does_not_double_punctuation() {
        // First line already ends with '.', so no extra period is inserted before the next.
        assert_eq!(strip_markdown("Done.\nShipping now"), "Done. Shipping now");
    }

    #[test]
    fn multiline_prose_without_punctuation_gets_periods() {
        assert_eq!(strip_markdown("line one\nline two"), "line one. line two");
    }
}
