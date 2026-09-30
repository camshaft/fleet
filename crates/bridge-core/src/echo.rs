//! `echo` — board-origin marker so a bidirectional bridge doesn't re-ingest its own reflected posts (board
//! task #430, the Josh/jokoo-bot pattern).
//!
//! THE HAZARD: once the auto-relay runs continuously, a board-originated message reflected OUT to the
//! external channel is later read back by the inbound poller and re-posted to the board -> an infinite echo
//! loop. Message subtype / bot-id filtering is not enough: a `chat.postMessage` sent with a user token +
//! `username` override can come back on `conversations.history` looking like a plain user message (posted by
//! the token owner), with no `bot_id` to filter on.
//!
//! THE FIX: stamp EVERY board-origin outbound message with a per-bot identity marker — a `(<BotName> <emoji>)`
//! prefix and an `<emoji>` suffix — and have the inbound side DROP any message carrying it. The marker doubles
//! as a human-visible "this was posted by a bot" badge. The emoji is a deliberate carve-out from the fleet's
//! emoji ban (#368/#308), operator-granted for exactly this bot-identity marker.
//!
//! Detection is CONSERVATIVE: it matches the *structured* leading `(<name> <emoji>)` marker, not a bare emoji,
//! so a human message that merely happens to contain the emoji is never dropped. Pure + unit-tested.

/// The reserved bridge-origin marker emoji (emoji-ban carve-out for the #430 bot-identity marker). A human
/// essentially never opens a message with the exact `(<name> <emoji>)` structure, so this is a safe sentinel.
pub const MARK_EMOJI: &str = "\u{1F916}"; // robot face

/// Stamp a board-origin outbound body with the bot-identity marker: `"(<bot> 🤖) <body> 🤖"`. The inbound
/// poller recognizes this via [`is_board_origin`] and drops it, so the message never loops back into the board.
pub fn mark_board_origin(bot: &str, body: &str) -> String {
    format!("({bot} {MARK_EMOJI}) {body} {MARK_EMOJI}")
}

/// Whether `text` is a bridge-posted board-origin message (carries the [`mark_board_origin`] marker) and must
/// be DROPPED on inbound to break the echo loop. Conservative: requires the structured leading
/// `(<something> 🤖)` marker (a leading `(` … `)` group containing the reserved emoji), so a human message that
/// merely contains the emoji elsewhere is NOT dropped. Pure.
pub fn is_board_origin(text: &str) -> bool {
    let t = text.trim_start();
    let Some(rest) = t.strip_prefix('(') else {
        return false;
    };
    match rest.find(')') {
        // The parenthesized marker group at the very start must carry the reserved emoji.
        Some(close) => rest[..close].contains(MARK_EMOJI),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_and_detects_round_trip() {
        let marked = mark_board_origin("Frank", "what do you think?");
        assert_eq!(marked, "(Frank \u{1F916}) what do you think? \u{1F916}");
        assert!(is_board_origin(&marked), "the bridge recognizes its own marked message");
    }

    #[test]
    fn plain_human_messages_are_not_board_origin() {
        assert!(!is_board_origin("hey frank what do you think?"));
        assert!(!is_board_origin("just a normal message"));
        assert!(!is_board_origin(""));
        // A human message that merely CONTAINS the emoji (not as the leading marker) is NOT dropped.
        assert!(!is_board_origin("i love robots \u{1F916} they are cool"));
        // A leading paren group WITHOUT the reserved emoji is not a marker.
        assert!(!is_board_origin("(just a parenthetical) then text"));
    }

    #[test]
    fn detects_despite_leading_whitespace() {
        let marked = format!("   {}", mark_board_origin("George", "hi"));
        assert!(is_board_origin(&marked));
    }

    #[test]
    fn different_bot_names_all_detected() {
        assert!(is_board_origin(&mark_board_origin("Frank", "a")));
        assert!(is_board_origin(&mark_board_origin("assistant-bot", "b")));
    }
}
