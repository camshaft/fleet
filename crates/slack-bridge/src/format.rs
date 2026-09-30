//! Message shaping between the board and Slack — pure, no I/O, unit-tested. Ported/adapted from the
//! cadenza reference `format.rs` (which shaped the file-hub inbox protocol) to the board's post model.
//!
//! Two directions:
//!   board → Slack:  a `channel.outbound_reflect` event ([`bridge_core::OutboundReflect`]) is rendered as
//!                   a readable Slack-mrkdwn line — author (+ external-author attribution) then body,
//!                   with the content HTML-escaped and length-capped so a large/awkward body can't make
//!                   the Slack post fail. A degraded plain variant + the `bridge_core::relay` escalation
//!                   keep the outbound relay from head-of-line-blocking on a message Slack rejects.
//!   Slack → board:  an operator's Slack line is parsed into an [`Intent`] — a leading `@agent` retargets
//!                   the recipient (strict, traversal-safe slug), otherwise it routes to the default agent.
//!
//! All rendering/parsing here is transport-agnostic and network-free; the async Slack transport and the
//! board client call into it.

use bridge_core::OutboundReflect;

// ── Slack text limits + escaping ─────────────────────────────────────────────────────────────────

/// Slack's `chat.postMessage` rejects a `text` longer than 40000 chars, and a mrkdwn *section block* caps
/// at 3000. We post as plain `text` (not a section block), so cap well under 40000 with headroom for the
/// mrkdwn markup. An over-limit post would FAIL — and the outbound relay would then retry the same message
/// forever, blocking the queue. Truncating keeps delivery robust; the full text still lives on the board.
const SLACK_TEXT_CAP: usize = 3500;

/// Escape the three Slack `text` CONTROL characters (`&`, `<`, `>`) as HTML entities. Slack opens
/// link/entity parsing on these, so an unescaped one (e.g. a body with `<512KiB` or a generic `Rc<[T]>`)
/// mis-parses or makes `chat.postMessage` return `internal_error`. Escape only message CONTENT, never the
/// mrkdwn markers we add. `&` MUST go first, else we'd re-escape the `&` in the `&lt;`/`&gt;` we produce.
fn escape_slack_entities(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Truncate `s` to [`SLACK_TEXT_CAP`] characters (not bytes — never split a multibyte char), appending an
/// elision marker when cut. A no-op for the common short message.
fn cap_for_slack(s: String) -> String {
    if s.chars().count() <= SLACK_TEXT_CAP {
        return s;
    }
    let marker = "\n…[truncated — full text on the board]";
    let keep = SLACK_TEXT_CAP.saturating_sub(marker.chars().count());
    let head: String = s.chars().take(keep).collect();
    format!("{head}{marker}")
}

// ── board → Slack render ─────────────────────────────────────────────────────────────────────────

/// Render an outbound-reflect (a board post the concierge authorized to mirror OUT) as a Slack-mrkdwn
/// string: the board author, an optional external-author attribution (when the board post was itself
/// attributed to an external human), then the body. Content is HTML-escaped and the whole thing is
/// length-capped so a large body can't make the Slack post fail. (`reply_to` is a threading concern for
/// the transport layer, not the rendered text.)
pub fn render_outbound_reflect(r: &OutboundReflect) -> String {
    let author = escape_slack_entities(if r.author.is_empty() { "unknown" } else { &r.author });
    let mut head = format!("*{author}*");
    if let Some(ext) = r.external_author.as_deref().filter(|e| !e.is_empty()) {
        head.push_str(&format!(" _(via {})_", escape_slack_entities(ext)));
    }
    let body = r.body.trim();
    let out = if body.is_empty() {
        head
    } else {
        format!("{head}\n{}", escape_slack_entities(body))
    };
    cap_for_slack(out)
}

// ── DEGRADED render (board → Slack) ──────────────────────────────────────────────────────────────
//
// The relay-resilience ESCALATION policy (when to retry / degrade / quarantine) is transport-agnostic and
// lives in `bridge_core::relay` (`relay_plan` / `RelayPlan` / the thresholds). This module supplies only the
// Slack-specific degraded RENDER the relay falls back to once the rich render keeps failing on content.

/// The proven-safe length for the degraded post: Slack rejected a ~2KB rendered mrkdwn message with
/// `internal_error` while a 400-char truncation of the same posted fine — so the degraded variant
/// truncates to this. A message the operator still SEES (with a pointer to the board) beats one dropped.
const PLAIN_TEXT_CAP: usize = 400;

/// Replace every character Slack's mrkdwn/entity parser treats as control (formatting `*_~`` `, entity/link
/// markup `<>&|`) with a space and collapse whitespace runs. Neutralizing the trigger in the CONTENT (vs a
/// per-post `mrkdwn=false` flag the transport may not expose) keeps the degraded post from re-triggering
/// the parse quirk that failed the rich render.
fn strip_mrkdwn(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        let c = if matches!(c, '*' | '_' | '~' | '`' | '<' | '>' | '&' | '|') || c.is_whitespace() {
            ' '
        } else {
            c
        };
        if c == ' ' {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

/// Render an outbound-reflect as a DEGRADED, plain, mrkdwn-safe, hard-truncated Slack string — the relay's
/// fallback when the normal render deterministically fails to post. No mrkdwn markup, control chars
/// stripped from every field, capped at [`PLAIN_TEXT_CAP`] scalars with a marker pointing at the board.
pub fn render_outbound_reflect_plain(r: &OutboundReflect) -> String {
    let author = strip_mrkdwn(if r.author.is_empty() { "unknown" } else { &r.author });
    let mut head = format!("[plain] {author}");
    if let Some(ext) = r.external_author.as_deref() {
        let ext = strip_mrkdwn(ext);
        if !ext.is_empty() {
            head.push_str(&format!(" (via {ext})"));
        }
    }
    let body = strip_mrkdwn(&r.body);
    let out = if body.is_empty() {
        head
    } else {
        format!("{head} — {body}")
    };
    if out.chars().count() <= PLAIN_TEXT_CAP {
        return out;
    }
    let marker = " …[truncated — full text on the board]";
    let keep = PLAIN_TEXT_CAP.saturating_sub(marker.chars().count());
    let head: String = out.chars().take(keep).collect();
    format!("{head}{marker}")
}

// ── Slack → board parse ────────────────────────────────────────────────────────────────────────────

/// A parsed operator Slack line: which agent it's addressed to, and the message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    /// The recipient agent (a leading `@agent`, else `default_to`). Always a valid agent slug.
    pub to: String,
    /// The message body (the text after any `@agent` prefix, trimmed).
    pub body: String,
}

/// Whether `name` is a valid agent slug: `[A-Za-z0-9][A-Za-z0-9-]*` (leading alphanumeric, then
/// alphanumerics/hyphens). No dots/slashes/separators. SECURITY: a permissive charset would let a
/// `@..`/`@../x` retarget parse as a traversal-shaped id; the strict slug closes that at the parse side.
pub fn is_valid_agent_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Parse an operator's Slack line into an [`Intent`].
///
/// Grammar:
///   `@pr-sync are you merging?` → to=pr-sync,      body="are you merging?"
///   `just some words`          → to=<default_to>,  body="just some words"
///
/// `default_to` is the agent used when no `@agent` is given. The `@agent` name is a strict slug
/// (`[A-Za-z0-9][A-Za-z0-9-]*`); a non-matching `@…` (e.g. `@..`) is left as literal body text and the
/// message routes to the default, so a traversal-shaped name can never become the recipient.
pub fn parse_operator_message(text: &str, default_to: &str) -> Intent {
    let mut rest = text.trim();
    let mut to = default_to.to_string();

    if let Some(stripped) = rest.strip_prefix('@') {
        let starts_slug = stripped
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        if starts_slug {
            let end = stripped
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                .unwrap_or(stripped.len());
            to = stripped[..end].to_string();
            rest = stripped[end..].trim_start();
        }
    }

    Intent {
        to,
        body: rest.trim().to_string(),
    }
}

/// A short usage/help string shown when the operator sends an empty message or `help`.
pub fn help_text(default_to: &str) -> String {
    format!(
        "*Fleet board bridge* — you're talking to the fleet over the board. Default recipient: *{default_to}*.\n\
         • plain text → routed to {default_to}\n\
         • `@agent …` → address a different agent (e.g. `@pr-sync status`)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reflect(author: &str, body: &str) -> OutboundReflect {
        OutboundReflect {
            channel_id: 1,
            post_seq: 1,
            author: author.to_string(),
            body: body.to_string(),
            reply_to: None,
            external_author: None,
            metadata: None,
            parent_metadata: None,
        }
    }

    // ── board → Slack render ─────────────────────────────────────────────────────────────────────

    #[test]
    fn render_shows_author_and_body() {
        let s = render_outbound_reflect(&reflect("concierge", "ship it"));
        assert!(s.contains("*concierge*"), "author in bold: {s}");
        assert!(s.contains("ship it"));
    }

    #[test]
    fn render_shows_external_author_attribution() {
        let mut r = reflect("slack-bridge", "hello from a human");
        r.external_author = Some("slack:U9".into());
        let s = render_outbound_reflect(&r);
        assert!(s.contains("via slack:U9"), "external author attributed: {s}");
    }

    #[test]
    fn render_empty_body_is_just_the_header() {
        let s = render_outbound_reflect(&reflect("concierge", "   "));
        assert_eq!(s, "*concierge*", "no trailing newline / empty body line");
    }

    #[test]
    fn render_empty_author_falls_back_to_unknown() {
        let s = render_outbound_reflect(&reflect("", "x"));
        assert!(s.contains("*unknown*"));
    }

    #[test]
    fn render_caps_a_huge_body_for_slack() {
        // A multi-KB body must not produce an over-limit post (which would fail + retry forever).
        let s = render_outbound_reflect(&reflect("v-x", &"x".repeat(20_000)));
        assert!(s.chars().count() <= SLACK_TEXT_CAP, "capped: {}", s.chars().count());
        assert!(s.contains("truncated"), "elision marker present");
        assert!(s.contains("*v-x*"), "header survives the cap");
    }

    #[test]
    fn render_caps_astral_chars_by_scalar_no_split() {
        let s = render_outbound_reflect(&reflect("v-x", &"👍".repeat(4000)));
        assert!(s.chars().count() <= SLACK_TEXT_CAP, "capped by scalar");
        assert!(s.contains("truncated"));
        assert!(!s.contains('\u{FFFD}'), "no replacement char");
    }

    #[test]
    fn render_does_not_touch_a_normal_message() {
        let s = render_outbound_reflect(&reflect("pr-sync", "all green"));
        assert!(!s.contains("truncated"), "short message untouched");
    }

    #[test]
    fn render_escapes_slack_control_entities_in_content() {
        // `&`, `<`, `>` are Slack `text` control chars; unescaped they mis-parse / `internal_error`.
        let r = reflect("v-x", "split file <512KiB, keep Rc<[T]> & Vec<T>");
        let s = render_outbound_reflect(&r);
        assert!(s.contains("&lt;512KiB"), "< escaped: {s}");
        assert!(s.contains("Vec&lt;T&gt;"), "generics escaped: {s}");
        assert!(s.contains("&amp;"), "& escaped: {s}");
        assert!(!s.contains('<') && !s.contains('>'), "no raw angle brackets: {s}");
        assert!(
            s.match_indices('&').all(|(i, _)| s[i..].starts_with("&amp;")
                || s[i..].starts_with("&lt;")
                || s[i..].starts_with("&gt;")),
            "every & is an entity head: {s}"
        );
        assert!(s.contains("*v-x*"), "the mrkdwn structure we add is untouched");
    }

    #[test]
    fn render_does_not_double_escape_ampersand() {
        let s = render_outbound_reflect(&reflect("v-x", "A && B < C"));
        assert!(s.contains("A &amp;&amp; B &lt; C"), "single-escaped: {s}");
        assert!(!s.contains("&amp;lt;"), "no double-escape: {s}");
    }

    // ── DEGRADED plain render ──────────────────────────────────────────────────────────────────────

    #[test]
    fn plain_render_strips_mrkdwn_and_is_bounded() {
        let mut r = reflect("pr-sync", "use <https://x> & _emphasis_ ~strike~ | pipe *bold* `code`");
        r.external_author = Some("slack:U1".into());
        let s = render_outbound_reflect_plain(&r);
        for bad in ['*', '_', '~', '`', '<', '>', '&', '|'] {
            assert!(!s.contains(bad), "plain render must not contain {bad:?}: {s}");
        }
        assert!(s.contains("pr-sync"), "keeps author: {s}");
        assert!(s.chars().count() <= PLAIN_TEXT_CAP);
    }

    #[test]
    fn plain_render_truncates_a_huge_body_with_marker() {
        let s = render_outbound_reflect_plain(&reflect("v-x", &"x".repeat(9000)));
        assert!(s.chars().count() <= PLAIN_TEXT_CAP, "capped: {}", s.chars().count());
        assert!(s.contains("truncated"), "elision marker present");
        assert!(s.contains("v-x"), "header survives the cap");
    }

    #[test]
    fn plain_render_caps_astral_by_scalar_no_split() {
        let s = render_outbound_reflect_plain(&reflect("v-x", &"👍".repeat(4000)));
        assert!(s.chars().count() <= PLAIN_TEXT_CAP);
        assert!(!s.contains('\u{FFFD}'), "no replacement char");
    }

    // ── Slack → board parse ────────────────────────────────────────────────────────────────────────

    #[test]
    fn plain_text_routes_to_default() {
        let i = parse_operator_message("what's the status?", "concierge");
        assert_eq!(i.to, "concierge");
        assert_eq!(i.body, "what's the status?");
    }

    #[test]
    fn at_agent_retargets() {
        let i = parse_operator_message("@pr-sync are you merging?", "concierge");
        assert_eq!(i.to, "pr-sync");
        assert_eq!(i.body, "are you merging?");
    }

    #[test]
    fn emptyish_message_has_empty_body_but_valid_recipient() {
        let i = parse_operator_message("@concierge   ", "concierge");
        assert_eq!(i.to, "concierge");
        assert_eq!(i.body, "");
    }

    #[test]
    fn help_names_default_and_prefix() {
        let h = help_text("concierge");
        assert!(h.contains("concierge"));
        assert!(h.contains("@agent"));
    }

    // ── SECURITY: a `@..`-style retarget must not parse as a traversal-shaped id ─────────────────────

    #[test]
    fn traversal_retarget_does_not_become_the_recipient() {
        let i = parse_operator_message("@.. hi", "concierge");
        assert_eq!(i.to, "concierge", "traversal name never becomes the recipient");
        assert!(is_valid_agent_name(&i.to));

        let i2 = parse_operator_message("@../../etc pwn", "concierge");
        assert_eq!(i2.to, "concierge");
    }

    #[test]
    fn dotted_name_stops_at_the_dot() {
        let i = parse_operator_message("@a.b rest", "concierge");
        assert_eq!(i.to, "a");
        assert!(is_valid_agent_name(&i.to));
        assert!(i.body.starts_with(".b"));
    }

    #[test]
    fn every_parsed_recipient_is_a_valid_agent_name() {
        for msg in ["@pr-sync go", "@.. x", "@a/b y", "plain", "@-bad z", "@design-jsx do"] {
            let i = parse_operator_message(msg, "concierge");
            assert!(
                is_valid_agent_name(&i.to),
                "parser produced an unsafe recipient {:?} from {msg:?}",
                i.to
            );
        }
    }

    #[test]
    fn is_valid_agent_name_contract() {
        assert!(is_valid_agent_name("concierge"));
        assert!(is_valid_agent_name("v-slack-bridge"));
        assert!(is_valid_agent_name("a"));
        assert!(is_valid_agent_name("agent0"));
        assert!(!is_valid_agent_name(""));
        assert!(!is_valid_agent_name("-lead"));
        assert!(!is_valid_agent_name(".."));
        assert!(!is_valid_agent_name("a.b"));
        assert!(!is_valid_agent_name("a/b"));
        assert!(!is_valid_agent_name("a b"));
    }
}
