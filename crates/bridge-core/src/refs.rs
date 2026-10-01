//! `refs` — detect the fleet's typed resource references (`doc_N`, `task_N`, `channel_N`, `project_N`) in text
//! and resolve each to its board-UI URL, so a bridge can render a bare ref as a clickable link on OUTBOUND
//! reflect (board task_768, operator ask: "if you send me a 'doc_95' it would be great if that was automatically
//! clickable ... translate it to the camshaft.dev domain ... it should be configurable what the root is").
//!
//! The ROOT origin and the per-type path segment are injected via [`RefScheme`], so the core stays board-URL
//! aware but transport- and deployment-agnostic: the Slack `<url|label>` wrapping and the configurable root live
//! in the adapter, which calls [`linkify`] with its own formatter. (Slack-, voice-, or GitHub-specifics live in
//! the transport crate, never here — see the crate root.)
//!
//! Detection is CONSERVATIVE and word-boundary aware. A ref is only recognized as a STANDALONE token — a maximal
//! run of `[A-Za-z0-9_]` that is exactly `<kind>_<digits>`. So `mydoc_5`, `task_force`, `doc_95a`, and a ref
//! already embedded in a slash/dot URL path (`.../doc/95`) are all left untouched. Fail-safe: a token whose kind
//! is unknown, or whose id does not parse, is left exactly as-is. Pure + unit-tested.

/// The typed-ref kinds the bridge linkifies. These are the fleet's standardized typed-ID prefixes; GitHub refs
/// (`owner/repo#N`) link to GitHub rather than the board root and are intentionally out of scope here (task_768
/// scopes them as a follow-on).
pub const REF_KINDS: [&str; 4] = ["doc", "task", "channel", "project"];

/// A typed resource reference found in text: its `kind` (one of [`REF_KINDS`]), its numeric `id`, and the byte
/// range `[start, end)` the token occupies in the source string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefMatch {
    pub kind: &'static str,
    pub id: u64,
    pub start: usize,
    pub end: usize,
}

/// A ref-token character: ASCII alphanumeric or underscore. Any other byte (whitespace, punctuation, `/`, `.`,
/// `<`, `>`, or a non-ASCII byte) is a token boundary — which is exactly what gives word-boundary matching.
fn is_ref_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Parse a standalone token (`"doc_95"`) into its `(kind, id)` if it is EXACTLY a known kind, a single `_`, and
/// one-or-more ASCII digits with nothing else. `None` otherwise — so `task_force` (not digits), `doc_95a` (a
/// trailing non-digit), `doc__5` (empty id segment), and `foo_5` (unknown kind) do not match. Pure.
fn parse_ref_token(tok: &str) -> Option<(&'static str, u64)> {
    let (kind_part, id_part) = tok.split_once('_')?;
    let kind = REF_KINDS.iter().copied().find(|&k| k == kind_part)?;
    if id_part.is_empty() || !id_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // All-digits and non-empty; only an overflow past u64 fails the parse (astronomically unlikely for an id).
    let id = id_part.parse::<u64>().ok()?;
    Some((kind, id))
}

/// Find every standalone typed ref in `text`, in order, with word-boundary matching. Pure; no allocation beyond
/// the result vector. Byte ranges always fall on char boundaries (a token is a run of ASCII ref-chars, so its
/// edges never split a multi-byte UTF-8 sequence).
pub fn find_refs(text: &str) -> Vec<RefMatch> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !is_ref_char(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_ref_char(bytes[i]) {
            i += 1;
        }
        let end = i;
        if let Some((kind, id)) = parse_ref_token(&text[start..end]) {
            out.push(RefMatch {
                kind,
                id,
                start,
                end,
            });
        }
    }
    out
}

/// How a typed ref resolves to a URL: the `origin` (scheme + host + any base prefix, no trailing slash, e.g.
/// `"https://camshaft.dev/board"`) and the path `segment` per kind. The adapter builds this from its configurable
/// root setting via [`RefScheme::with_root`]. The segment names are the board web router's canonical paths,
/// confirmed by the board-UI owner (v-task-board, task_768): a uniform `{origin}/{segment}/{id}` with plural
/// segments and a bare numeric id (no slug). The prod `/board` base prefix is folded into the ROOT, not the
/// segment, so segments stay clean and a different gateway mapping is just a different root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefScheme {
    /// URL prefix up to (not including) the per-type path, e.g. `"https://camshaft.dev/board"`. No trailing slash.
    pub origin: String,
    pub doc_seg: String,
    pub task_seg: String,
    pub channel_seg: String,
    pub project_seg: String,
}

impl RefScheme {
    /// Build a scheme from a configurable `root`. A bare domain (`"camshaft.dev/board"`) gets an `https://`
    /// scheme; a `root` that already carries `http://`/`https://` is kept as-is. A trailing slash is trimmed. The
    /// per-type segments are the board router's canonical plural paths (`documents`/`tasks`/`channels`/
    /// `projects`), confirmed by v-task-board; override the public `*_seg` fields only if the router changes.
    pub fn with_root(root: &str) -> Self {
        Self {
            origin: normalize_origin(root),
            doc_seg: "documents".to_string(),
            task_seg: "tasks".to_string(),
            channel_seg: "channels".to_string(),
            project_seg: "projects".to_string(),
        }
    }

    /// The path segment for `kind`, or `None` if the kind has no mapping (so it is left un-linkified: fail-safe).
    fn segment(&self, kind: &str) -> Option<&str> {
        match kind {
            "doc" => Some(&self.doc_seg),
            "task" => Some(&self.task_seg),
            "channel" => Some(&self.channel_seg),
            "project" => Some(&self.project_seg),
            _ => None,
        }
    }

    /// The resolved URL for a matched ref (`{origin}/{segment}/{id}`), or `None` if the kind does not resolve.
    pub fn url_for(&self, m: &RefMatch) -> Option<String> {
        let seg = self.segment(m.kind)?;
        Some(format!("{}/{}/{}", self.origin, seg, m.id))
    }
}

/// Normalize a configurable root into a bare origin: add `https://` to a scheme-less domain, keep an explicit
/// `http://`/`https://`, and trim a trailing slash. Pure.
fn normalize_origin(root: &str) -> String {
    let r = root.trim().trim_end_matches('/');
    if r.starts_with("http://") || r.starts_with("https://") {
        r.to_string()
    } else {
        format!("https://{r}")
    }
}

/// Rewrite `text`, replacing each RESOLVABLE typed ref with `fmt(ref_text, url)` (the adapter supplies `fmt`,
/// e.g. a Slack `<url|label>` wrapper). A ref whose kind/id does not resolve under `scheme` is left exactly as-is
/// (fail-safe), and all non-ref text is preserved byte-for-byte. Pure.
pub fn linkify(text: &str, scheme: &RefScheme, fmt: impl Fn(&str, &str) -> String) -> String {
    let refs = find_refs(text);
    if refs.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for m in &refs {
        // Fail-safe: an unresolvable ref is skipped WITHOUT advancing `last`, so its original text is carried
        // through verbatim by the next copied span.
        let Some(url) = scheme.url_for(m) else {
            continue;
        };
        out.push_str(&text[last..m.start]);
        out.push_str(&fmt(&text[m.start..m.end], &url));
        last = m.end;
    }
    out.push_str(&text[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Slack-style formatter, matching what the adapter uses, so the tests exercise a realistic render.
    fn slack(reftext: &str, url: &str) -> String {
        format!("<{url}|{reftext}>")
    }

    fn scheme() -> RefScheme {
        // The prod root folds in the board base prefix, per v-task-board (task_768).
        RefScheme::with_root("camshaft.dev/board")
    }

    #[test]
    fn finds_each_kind_with_ranges() {
        let refs = find_refs("see doc_95 and task_12 and channel_30 and project_7");
        let kinds: Vec<_> = refs.iter().map(|r| (r.kind, r.id)).collect();
        assert_eq!(
            kinds,
            vec![("doc", 95), ("task", 12), ("channel", 30), ("project", 7)]
        );
        // The first range covers exactly "doc_95".
        assert_eq!(
            &"see doc_95 and task_12 and channel_30 and project_7"[refs[0].start..refs[0].end],
            "doc_95"
        );
    }

    #[test]
    fn word_boundary_rejects_non_ref_tokens() {
        // Embedded in a larger token: not a standalone ref.
        assert!(find_refs("mydoc_5").is_empty());
        assert!(find_refs("xtask_1").is_empty());
        // Known kind but a non-numeric / mixed id.
        assert!(find_refs("task_force").is_empty());
        assert!(find_refs("doc_95a").is_empty());
        assert!(find_refs("doc_").is_empty());
        // Unknown kind.
        assert!(find_refs("foo_5").is_empty());
        // A path-form URL does NOT match: slashes split the tokens so "documents" and "95" stand alone.
        assert!(find_refs("https://camshaft.dev/board/documents/95").is_empty());
    }

    #[test]
    fn punctuation_adjacent_refs_still_match() {
        assert_eq!(
            find_refs("(task_12)")
                .iter()
                .map(|r| (r.kind, r.id))
                .collect::<Vec<_>>(),
            vec![("task", 12)]
        );
        assert_eq!(
            find_refs("see doc_95.")
                .iter()
                .map(|r| (r.kind, r.id))
                .collect::<Vec<_>>(),
            vec![("doc", 95)]
        );
        assert_eq!(find_refs("doc_95, task_1").len(), 2);
        assert_eq!(
            find_refs("channel_0 edge")
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            vec![0]
        );
    }

    #[test]
    fn linkify_wraps_resolved_refs_and_preserves_surrounding_text() {
        let out = linkify("please read doc_95 today", &scheme(), slack);
        assert_eq!(
            out,
            "please read <https://camshaft.dev/board/documents/95|doc_95> today"
        );
    }

    #[test]
    fn linkify_handles_multiple_refs_in_one_message() {
        let out = linkify("doc_1 then task_2", &scheme(), slack);
        assert_eq!(
            out,
            "<https://camshaft.dev/board/documents/1|doc_1> then <https://camshaft.dev/board/tasks/2|task_2>"
        );
    }

    #[test]
    fn linkify_leaves_text_without_refs_unchanged() {
        assert_eq!(
            linkify("no refs here at all", &scheme(), slack),
            "no refs here at all"
        );
        assert_eq!(linkify("", &scheme(), slack), "");
        // A ref-like-but-not-a-ref token is untouched.
        assert_eq!(
            linkify("talk to the task_force", &scheme(), slack),
            "talk to the task_force"
        );
    }

    #[test]
    fn configurable_root_overrides_the_default_domain() {
        let out = linkify(
            "doc_5",
            &RefScheme::with_root("board.example.internal"),
            slack,
        );
        assert_eq!(out, "<https://board.example.internal/documents/5|doc_5>");
        // An explicit scheme and a trailing slash are both honored.
        let s = RefScheme::with_root("http://localhost:8880/");
        assert_eq!(s.origin, "http://localhost:8880");
        assert_eq!(
            linkify("task_9", &s, slack),
            "<http://localhost:8880/tasks/9|task_9>"
        );
    }

    #[test]
    fn unresolvable_kind_is_left_as_is() {
        // Force a scheme whose segment lookup fails for a kind by constructing a match directly.
        let s = scheme();
        let m = RefMatch {
            kind: "unknownkind",
            id: 3,
            start: 0,
            end: 0,
        };
        assert_eq!(s.url_for(&m), None);
    }

    #[test]
    fn ranges_are_safe_around_multibyte_text() {
        // A non-ASCII char adjacent to a ref must not break slicing, and the ref still resolves.
        let text = "note\u{2014}doc_42\u{2014}end"; // em-dash on both sides
        let refs = find_refs(text);
        assert_eq!(
            refs.iter().map(|r| (r.kind, r.id)).collect::<Vec<_>>(),
            vec![("doc", 42)]
        );
        let out = linkify(text, &scheme(), slack);
        assert_eq!(
            out,
            "note\u{2014}<https://camshaft.dev/board/documents/42|doc_42>\u{2014}end"
        );
    }
}
