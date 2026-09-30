//! `mention` — a pure text wake-word detector so a bridge only rouses a named agent when it is ADDRESSED,
//! not on every channel message (board task #429 — the jokoo-bot "hey frank ..." behavior).
//!
//! A membrain-style bridge mirrors a whole external channel onto the board, but a participating agent
//! (`frank`, …) should stay silent unless a human addresses it by name. This module answers the one pure
//! question that gate needs: *does this message mention this agent by name?* It is transport-agnostic (Slack,
//! voice, GitHub all reuse it) and knows nothing about how the wake is delivered — the caller decides that.
//!
//! Matching rule: the agent name must appear as a WHOLE, case-insensitive, alphanumeric-delimited token.
//! So `frank`, `Frank`, `@frank`, `hey frank`, `frank:`, `frank?`, and `frank's` all match; `frankly` and
//! `frankfurt` do NOT (substrings never trigger). This mirrors how a person reads "being mentioned by name":
//! a leading `@` or trailing punctuation is a delimiter, but a longer word that merely contains the name is
//! not an address. Names are matched token-wise, so a name is treated as a single alphanumeric token.

/// Whether `text` addresses the agent named `name` — i.e. `name` occurs as a standalone, case-insensitive,
/// alphanumeric-delimited token (`@frank`, `hey frank`, `frank:` match; `frankly`/`frankfurt` do not). Empty
/// `name` never matches. This is the #429 text wake-word: the bridge wakes the agent only when it is mentioned.
pub fn is_mentioned(text: &str, name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() {
        return false;
    }
    let name_lower = name.to_lowercase();
    // Tokenize on any non-alphanumeric boundary (so `@`, whitespace, `:`, `,`, `?`, `'` all delimit) and
    // compare each token to the name, case-insensitively. Unicode-aware alphanumerics so non-ASCII names work.
    text.split(|c: char| !c.is_alphanumeric())
        .any(|tok| !tok.is_empty() && tok.eq_ignore_ascii_case(&name_lower))
}

/// The first of `names` that `text` addresses (see [`is_mentioned`]), or `None` if it mentions none. A bridge
/// uses this to pick which of a channel's participating agents (its `outbound_authors`) a message wakes.
pub fn mentioned_agent<'a, I>(text: &str, names: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    names.into_iter().find(|name| is_mentioned(text, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_name_and_wake_phrase_match() {
        assert!(is_mentioned("frank what do you think?", "frank"));
        assert!(is_mentioned("hey frank, thoughts?", "frank"));
        assert!(
            is_mentioned("so, Frank — any ideas", "frank"),
            "case-insensitive"
        );
        assert!(
            is_mentioned("cc @frank please", "frank"),
            "leading @ is a delimiter"
        );
        assert!(
            is_mentioned("frank: go", "frank"),
            "trailing colon delimits"
        );
        assert!(
            is_mentioned("what's frank's take", "frank"),
            "possessive: frank is its own token"
        );
        assert!(is_mentioned("FRANK!!!", "Frank"), "punctuation + all-caps");
    }

    #[test]
    fn substrings_do_not_match() {
        assert!(
            !is_mentioned("frankly I disagree", "frank"),
            "frankly is not a mention"
        );
        assert!(!is_mentioned("I flew to frankfurt", "frank"));
        assert!(!is_mentioned("the framework is fine", "frank"));
        assert!(!is_mentioned("no mention here at all", "frank"));
    }

    #[test]
    fn empty_inputs_never_match() {
        assert!(!is_mentioned("", "frank"));
        assert!(!is_mentioned("frank", ""));
        assert!(!is_mentioned("frank", "   "));
    }

    #[test]
    fn name_anywhere_in_the_message() {
        assert!(is_mentioned(
            "I really think that frank should weigh in",
            "frank"
        ));
        assert!(
            is_mentioned("start\nnewline then frank\nend", "frank"),
            "newline delimits"
        );
    }

    #[test]
    fn mentioned_agent_picks_the_addressed_one() {
        let agents = ["frank", "george"];
        assert_eq!(mentioned_agent("hey george, look", agents), Some("george"));
        assert_eq!(
            mentioned_agent("frank and george", agents),
            Some("frank"),
            "first match wins"
        );
        assert_eq!(mentioned_agent("nobody addressed", agents), None);
    }

    #[test]
    fn multichar_and_unicode_names() {
        assert!(
            !is_mentioned("ping bot-1 now", "bot-1"),
            "a hyphenated name is two tokens; not matched as one"
        );
        // A single-token unicode name matches whole-token.
        assert!(is_mentioned("hola andré", "André"));
        assert!(!is_mentioned("andrés is someone else", "andré"));
    }
}
