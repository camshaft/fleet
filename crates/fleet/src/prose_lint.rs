//! `prose_lint` -- the clean-prose comment lint (task_1319): a gate that flags filler emphatics and
//! caps-for-emphasis in Rust comments and in the loops markdown, so the house write-literally style
//! (operator directive, 2026-08-07) cannot regress in code.
//!
//! Two rules run over one pass:
//!   - emphatics: a case-insensitive substring match of each comment line against a wordlist that is a
//!     checked-in projection of the live board banned-phrases list. The board stays the one source;
//!     public CI and a pre-commit hook cannot reach it, so the list is synced into a committed file
//!     (see prose-style.toml) and the lint reads that file.
//!   - caps-for-emphasis: a structural check for an all-caps alphabetic word used in prose, with an
//!     allow-list of acceptable all-caps tokens (acronyms). A word that carries a digit or an
//!     underscore is an identifier, not emphasis, so it is never flagged.
//!
//! The scope is comments, not code. A listed word inside a string literal or an identifier is out of
//! scope, so the lint does not fire on a user-facing string. Markdown prose lines are scanned whole,
//! with fenced code blocks skipped.
//!
//! Matching logic is pure (no file access), so it is unit-tested apart from the file walk and the CLI.

use std::collections::BTreeSet;

use serde::Deserialize;

/// Which rule produced a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// A banned filler phrase from the projected board wordlist.
    Emphatic,
    /// An all-caps word used for emphasis.
    CapsEmphasis,
}

impl Rule {
    fn as_str(self) -> &'static str {
        match self {
            Rule::Emphatic => "emphatic",
            Rule::CapsEmphasis => "caps-for-emphasis",
        }
    }
}

/// One lint hit: the file, its 1-based line, the rule, and the exact matched text (the phrase or the
/// word), never the surrounding line, so a report echoes only what tripped the rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub file: String,
    pub line: usize,
    pub rule: Rule,
    pub token: String,
}

/// A comment line pulled from a source file: its 1-based line and the comment prose (the text after the
/// marker). The rule matchers see only this prose, not the code around it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentLine {
    pub line: usize,
    pub text: String,
}

/// The ruleset file shape: an emphatics wordlist plus a caps allow-list.
#[derive(Debug, Deserialize)]
struct RulesetFile {
    #[serde(default)]
    emphatics: Vec<String>,
    #[serde(default)]
    caps_allow: Vec<String>,
}

/// A compiled ruleset ready to match: emphatics pre-lowercased for a case-insensitive match, and the
/// caps allow-list upper-cased into a set for a direct membership test.
#[derive(Debug, Clone)]
pub struct Ruleset {
    emphatics_lower: Vec<String>,
    caps_allow: BTreeSet<String>,
}

/// Parse a ruleset TOML string into a compiled [`Ruleset`]. Pure (no file access). An empty emphatics
/// list is allowed: the caps rule still runs, so the lint is not fail-open with an empty wordlist.
pub fn parse_ruleset(toml_src: &str) -> Result<Ruleset, String> {
    let parsed: RulesetFile =
        toml::from_str(toml_src).map_err(|e| format!("ruleset parse error: {e}"))?;
    let mut emphatics_lower = Vec::new();
    for p in parsed.emphatics {
        let t = p.trim();
        if t.is_empty() {
            return Err("ruleset has an empty emphatic phrase".to_string());
        }
        emphatics_lower.push(t.to_lowercase());
    }
    let caps_allow = parsed
        .caps_allow
        .into_iter()
        .map(|t| t.trim().to_uppercase())
        .filter(|t| !t.is_empty())
        .collect();
    Ok(Ruleset {
        emphatics_lower,
        caps_allow,
    })
}

/// The emphatic phrases found in one line of prose, as the matched phrases. Case-insensitive substring.
pub fn match_emphatics(text: &str, rs: &Ruleset) -> Vec<String> {
    let lower = text.to_lowercase();
    rs.emphatics_lower
        .iter()
        .filter(|p| lower.contains(p.as_str()))
        .cloned()
        .collect()
}

/// Is this word an all-caps emphasis word under the allow-list? True when it is two or more characters,
/// every character is an ASCII uppercase letter (so a digit or an underscore rules it out as an
/// identifier), and it is not in the allow-list.
fn is_caps_emphasis(word: &str, allow: &BTreeSet<String>) -> bool {
    word.len() >= 2 && word.chars().all(|c| c.is_ascii_uppercase()) && !allow.contains(word)
}

/// The caps-for-emphasis words found in one line of prose. A word is a maximal run of ASCII letters,
/// digits, and underscores, so a token such as `FLEET_HOST` or `HTTP2` is one word and, carrying an
/// underscore or a digit, is skipped as an identifier.
pub fn caps_emphasis_words(text: &str, rs: &Ruleset) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut Vec<String>| {
        if !word.is_empty() {
            if is_caps_emphasis(word, &rs.caps_allow) {
                out.push(word.clone());
            }
            word.clear();
        }
    };
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// Run both rules over a set of comment lines from one file, collecting findings.
pub fn lint_comment_lines(file: &str, lines: &[CommentLine], rs: &Ruleset) -> Vec<Finding> {
    let mut out = Vec::new();
    for cl in lines {
        for phrase in match_emphatics(&cl.text, rs) {
            out.push(Finding {
                file: file.to_string(),
                line: cl.line,
                rule: Rule::Emphatic,
                token: phrase,
            });
        }
        for word in caps_emphasis_words(&cl.text, rs) {
            out.push(Finding {
                file: file.to_string(),
                line: cl.line,
                rule: Rule::CapsEmphasis,
                token: word,
            });
        }
    }
    out
}

/// Pull comment prose out of Rust source. Pure. Handles line comments (`//`, including `///` and
/// `//!`), block comments (`/* ... */`, including across lines), string literals (so a `//` inside a
/// string is not a comment), and the common char-literal forms (so a quote inside a char literal does
/// not open a string). A lifetime tick is skipped without opening a string.
pub fn extract_comments_rs(content: &str) -> Vec<CommentLine> {
    let mut out = Vec::new();
    let mut in_block = false;
    for (idx, line) in content.lines().enumerate() {
        let lineno = idx + 1;
        if in_block {
            if let Some(end) = line.find("*/") {
                push_text(&mut out, lineno, &line[..end]);
                in_block = false;
                scan_rs_line(&mut out, lineno, &line[end + 2..], &mut in_block);
            } else {
                push_text(&mut out, lineno, line);
            }
        } else {
            scan_rs_line(&mut out, lineno, line, &mut in_block);
        }
    }
    out
}

/// Scan one line of Rust (outside any open block comment) for comment text, tracking string and char
/// literals so a marker inside a literal is ignored. Sets `in_block` when a block comment opens and is
/// not closed on this line.
fn scan_rs_line(out: &mut Vec<CommentLine>, lineno: usize, line: &str, in_block: &mut bool) {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut in_str = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_str = true;
            i += 1;
            continue;
        }
        if c == b'\'' {
            // A char literal in one of its common forms is stepped over whole so a quote it contains
            // does not open a string; anything else is a lifetime tick, which is skipped.
            if i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                if let Some(rel) = find_byte(&bytes[i + 2..], b'\'') {
                    i = i + 2 + rel + 1;
                    continue;
                }
            } else if i + 2 < bytes.len() && bytes[i + 2] == b'\'' {
                i += 3;
                continue;
            }
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            push_text(out, lineno, &line[i + 2..]);
            return;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            if let Some(rel) = line[i + 2..].find("*/") {
                let end = i + 2 + rel;
                push_text(out, lineno, &line[i + 2..end]);
                i = end + 2;
                continue;
            }
            push_text(out, lineno, &line[i + 2..]);
            *in_block = true;
            return;
        }
        i += 1;
    }
}

/// The index of the first `needle` byte in `hay`, or None.
fn find_byte(hay: &[u8], needle: u8) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}

/// Push a comment line if its text carries any non-space character; a blank comment never has a finding.
/// Leading comment-marker characters (`/`, `*`, `!`) are stripped first, so a doc comment (`///`, `//!`)
/// and a block continuation line (` * ...`) yield plain prose with no marker noise in the matched token.
fn push_text(out: &mut Vec<CommentLine>, line: usize, text: &str) {
    let cleaned =
        text.trim_start_matches(|c: char| c == '/' || c == '*' || c == '!' || c.is_whitespace());
    if !cleaned.trim().is_empty() {
        out.push(CommentLine {
            line,
            text: cleaned.to_string(),
        });
    }
}

/// Treat markdown as prose: every line is scannable, except lines inside a fenced code block (between
/// ``` fences), where command examples legitimately carry acronyms and shell text.
pub fn markdown_lines(content: &str) -> Vec<CommentLine> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for (idx, line) in content.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if !line.trim().is_empty() {
            out.push(CommentLine {
                line: idx + 1,
                text: line.to_string(),
            });
        }
    }
    out
}

/// Lint one file's content, dispatching on extension: `.rs` reads comments, `.md` reads prose lines.
pub fn lint_file(path: &str, content: &str, rs: &Ruleset) -> Vec<Finding> {
    let lines = if path.ends_with(".md") {
        markdown_lines(content)
    } else {
        extract_comments_rs(content)
    };
    lint_comment_lines(path, &lines, rs)
}

/// Resolve + load the ruleset from an explicit path or `$FLEET_PROSE_RULESET`.
pub fn load_ruleset(explicit: Option<&str>) -> Result<Ruleset, String> {
    let path = explicit
        .map(str::to_string)
        .or_else(|| std::env::var("FLEET_PROSE_RULESET").ok())
        .ok_or_else(|| {
            "no ruleset: pass --ruleset <path> or set FLEET_PROSE_RULESET (the projection of the board \
             banned-phrases list, e.g. crates/fleet/prose-style.toml)"
                .to_string()
        })?;
    let src =
        std::fs::read_to_string(&path).map_err(|e| format!("cannot read ruleset '{path}': {e}"))?;
    parse_ruleset(&src)
}

/// Walk a directory, collecting the paths of every `.rs` and `.md` file under it.
fn walk_sources(dir: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(dir)];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Some(ext) = p.extension().and_then(|e| e.to_str())
                && (ext == "rs" || ext == "md")
                && let Some(s) = p.to_str()
            {
                out.push(s.to_string());
            }
        }
    }
    out.sort();
    out
}

/// Options for [`lint_prose`].
pub struct LintOpts {
    /// Explicit ruleset path; falls back to `$FLEET_PROSE_RULESET`.
    pub ruleset: Option<String>,
    /// Explicit files to lint; classified by extension.
    pub files: Vec<String>,
    /// Directories to walk for `.rs` and `.md` sources.
    pub dirs: Vec<String>,
    /// Report findings but exit 0 (advisory).
    pub warn_only: bool,
}

/// `fleet lint-prose` -- lint comments and loops markdown for filler emphatics and caps-for-emphasis.
///
/// Exit codes: 0 when clean (or advisory-only), 1 on a finding, 2 on a config error (no or invalid
/// ruleset). The non-zero finding exit is what lets this gate a PR as a required status check.
pub fn lint_prose(opts: LintOpts) {
    let rs = match load_ruleset(opts.ruleset.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("lint-prose: {e}");
            std::process::exit(2);
        }
    };

    let mut paths = opts.files.clone();
    for d in &opts.dirs {
        paths.extend(walk_sources(d));
    }

    let mut findings = Vec::new();
    let mut scanned = 0usize;
    for path in &paths {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        scanned += 1;
        findings.extend(lint_file(path, &content, &rs));
    }

    for f in &findings {
        eprintln!("{}:{}: {} '{}'", f.file, f.line, f.rule.as_str(), f.token);
    }

    if findings.is_empty() {
        println!("lint-prose: clean ({scanned} file(s) scanned)");
        return;
    }
    if opts.warn_only {
        eprintln!("lint-prose: {} finding(s) (advisory)", findings.len());
        return;
    }
    eprintln!(
        "lint-prose: {} finding(s) -- blocked (comments must read as clean prose)",
        findings.len()
    );
    std::process::exit(1);
}

/// The header written at the top of the generated ruleset file. The committed prose-style.toml is the
/// output of `fleet prose-sync`, so this header is reproduced verbatim and the drift check compares the
/// whole file byte for byte.
const RULESET_HEADER: &str = "\
# prose-style.toml -- the clean-prose lint ruleset (task_1319). Generated by `fleet prose-sync`.
#
# `emphatics` is a projection of the live board banned-phrases list, the one authoritative source. Do not
# edit it by hand: run `fleet prose-sync` to refresh it, and `fleet prose-sync --check` (a board-host
# maintenance tick) fails when this committed copy falls behind the board. The lint reads this file so the
# gate runs in public CI and in a pre-commit hook, neither of which can reach the internal board.
#
# `caps_allow` is the structural caps-for-emphasis rule's allow-list of acceptable all-caps tokens. It is
# hand-maintained here (the sync preserves it) and kept conservative, extended from real false positives. A
# token carrying a digit or an underscore is an identifier and is never flagged, so it does not belong here.
";

/// Quote one string as a TOML double-quoted array entry with a trailing comma, escaping a backslash and a
/// double quote. The phrases carry apostrophes, which need no escaping inside a double-quoted string.
fn toml_entry(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("  \"{escaped}\",")
}

/// Render a ruleset file deterministically from the two lists (each sorted and deduped, one entry per
/// line). Pure. The committed prose-style.toml is exactly this output, so the drift check is a comparison
/// of this render against the file on disk.
pub fn render_ruleset_toml(emphatics: &[String], caps_allow: &[String]) -> String {
    let render_list = |name: &str, items: &[String]| {
        let mut v = items.to_vec();
        v.sort();
        v.dedup();
        let mut block = format!("{name} = [\n");
        for item in &v {
            block.push_str(&toml_entry(item));
            block.push('\n');
        }
        block.push_str("]\n");
        block
    };
    let mut s = String::new();
    s.push_str(RULESET_HEADER);
    s.push('\n');
    s.push_str(&render_list("emphatics", emphatics));
    s.push('\n');
    s.push_str(&render_list("caps_allow", caps_allow));
    s
}

/// The raw (original-case, trimmed) emphatics and caps_allow arrays from a ruleset file. The sync uses this
/// to preserve the hand-maintained caps_allow while it refreshes emphatics from the board.
pub fn parse_raw_lists(toml_src: &str) -> Result<(Vec<String>, Vec<String>), String> {
    let parsed: RulesetFile =
        toml::from_str(toml_src).map_err(|e| format!("ruleset parse error: {e}"))?;
    let trim = |v: Vec<String>| v.into_iter().map(|s| s.trim().to_string()).collect();
    Ok((trim(parsed.emphatics), trim(parsed.caps_allow)))
}

/// Options for [`prose_sync`].
pub struct SyncOpts {
    /// Path to the committed ruleset file to refresh or check (else `$FLEET_PROSE_RULESET`).
    pub ruleset: Option<String>,
    /// Drift mode: compare the committed file against a fresh render and exit 1 on a mismatch, writing
    /// nothing. This runs on a board-host maintenance tick, not in public CI, which cannot reach the board.
    pub check: bool,
    /// Explicit board base URL; falls back to the configured board.
    pub board_api: Option<String>,
}

/// `fleet prose-sync` -- refresh (or, with `--check`, verify) the committed emphatics projection from the
/// live board banned-phrases list, preserving the hand-maintained caps allow-list.
///
/// Exit codes: 0 in sync or written, 1 on drift under `--check`, 2 on a config or board error.
pub fn prose_sync(opts: SyncOpts) {
    let path = match opts
        .ruleset
        .or_else(|| std::env::var("FLEET_PROSE_RULESET").ok())
    {
        Some(p) => p,
        None => {
            eprintln!(
                "prose-sync: no ruleset path: pass --ruleset <path> or set FLEET_PROSE_RULESET"
            );
            std::process::exit(2);
        }
    };
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("prose-sync: cannot read ruleset '{path}': {e}");
            std::process::exit(2);
        }
    };
    let caps_allow = match parse_raw_lists(&existing) {
        Ok((_emph, caps)) => caps,
        Err(e) => {
            eprintln!("prose-sync: {e}");
            std::process::exit(2);
        }
    };
    let board = match opts.board_api.as_deref() {
        Some(b) => crate::board::Board::with_base(b),
        None => match crate::board::Board::connect() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("prose-sync: {e}");
                std::process::exit(2);
            }
        },
    };
    let emphatics = match board.banned_phrases() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("prose-sync: {e}");
            std::process::exit(2);
        }
    };
    let rendered = render_ruleset_toml(&emphatics, &caps_allow);
    if rendered == existing {
        println!("prose-sync: in sync ({} emphatic(s))", emphatics.len());
        return;
    }
    if opts.check {
        eprintln!(
            "prose-sync: drift -- the committed {path} is behind the board banned-phrases list; run \
             `fleet prose-sync --ruleset {path}` to refresh it"
        );
        std::process::exit(1);
    }
    if let Err(e) = std::fs::write(&path, &rendered) {
        eprintln!("prose-sync: cannot write ruleset '{path}': {e}");
        std::process::exit(2);
    }
    println!(
        "prose-sync: refreshed {path} ({} emphatic(s))",
        emphatics.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rs() -> Ruleset {
        parse_ruleset(
            r#"
            emphatics = ["load-bearing", "leverage", "it's worth noting"]
            caps_allow = ["CI", "HTTP", "JSON"]
        "#,
        )
        .unwrap()
    }

    #[test]
    fn parse_ruleset_lowercases_emphatics_and_uppercases_allow() {
        let r = parse_ruleset(
            r#"
            emphatics = ["Load-Bearing"]
            caps_allow = ["ci"]
        "#,
        )
        .unwrap();
        assert_eq!(r.emphatics_lower, vec!["load-bearing"]);
        assert!(r.caps_allow.contains("CI"));
    }

    #[test]
    fn parse_ruleset_rejects_empty_phrase() {
        let err = parse_ruleset(r#"emphatics = ["  "]"#).unwrap_err();
        assert!(err.contains("empty emphatic"), "got: {err}");
    }

    #[test]
    fn parse_ruleset_allows_empty_lists() {
        let r = parse_ruleset("").unwrap();
        assert!(r.emphatics_lower.is_empty());
        assert!(r.caps_allow.is_empty());
    }

    #[test]
    fn emphatics_match_is_case_insensitive_substring() {
        let hits = match_emphatics("this is a Load-Bearing comment", &rs());
        assert_eq!(hits, vec!["load-bearing"]);
    }

    #[test]
    fn emphatics_match_multiword_phrase() {
        let hits = match_emphatics("it's worth noting that it works", &rs());
        assert_eq!(hits, vec!["it's worth noting"]);
    }

    #[test]
    fn caps_emphasis_flags_an_english_word_in_caps() {
        let words = caps_emphasis_words("this runs the SAME gate but NOT twice", &rs());
        assert_eq!(words, vec!["SAME", "NOT"]);
    }

    #[test]
    fn caps_emphasis_allows_listed_acronyms() {
        let words = caps_emphasis_words("the CI job posts JSON over HTTP", &rs());
        assert!(
            words.is_empty(),
            "allow-listed acronyms are not emphasis: {words:?}"
        );
    }

    #[test]
    fn caps_emphasis_skips_identifier_shaped_tokens() {
        // A token with an underscore or a digit is an identifier, not emphasis.
        let words = caps_emphasis_words("set FLEET_HOST and HTTP2 and P0 here", &rs());
        assert!(
            words.is_empty(),
            "identifier-shaped tokens are skipped: {words:?}"
        );
    }

    #[test]
    fn caps_emphasis_ignores_single_letters() {
        let words = caps_emphasis_words("a plain sentence with I in it", &rs());
        assert!(words.is_empty());
    }

    #[test]
    fn caps_emphasis_ignores_mixed_case_acronym_plurals() {
        // A trailing lowercase letter means the word is not all caps, so it is not flagged.
        let words = caps_emphasis_words("several APIs and PRs here", &rs());
        assert!(
            words.is_empty(),
            "mixed-case plurals are not all-caps: {words:?}"
        );
    }

    #[test]
    fn extract_comments_line_comment() {
        let src = "let x = 1; // keep it SIMPLE\n";
        let c = extract_comments_rs(src);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].line, 1);
        assert_eq!(c[0].text.trim(), "keep it SIMPLE");
    }

    #[test]
    fn extract_comments_ignores_slashes_inside_a_string() {
        let src = "let u = \"http://example.invalid/a//b\"; // real NOTE\n";
        let c = extract_comments_rs(src);
        assert_eq!(c.len(), 1, "only the real comment is extracted: {c:?}");
        assert_eq!(c[0].text.trim(), "real NOTE");
    }

    #[test]
    fn extract_comments_doc_comment() {
        let src = "/// a doc comment that is ALWAYS read\nfn f() {}\n";
        let c = extract_comments_rs(src);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].text.trim(), "a doc comment that is ALWAYS read");
    }

    #[test]
    fn extract_comments_block_across_lines() {
        let src = "/* first REALLY\n   second line */ let x = 1;\n";
        let c = extract_comments_rs(src);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].text.trim(), "first REALLY");
        assert_eq!(c[1].text.trim(), "second line");
    }

    #[test]
    fn extract_comments_code_after_block_close() {
        let src = "/* a */ let s = \"not // a comment\"; // yes HERE\n";
        let c = extract_comments_rs(src);
        // the inline block "a", then the real line comment; the string is not scanned.
        assert_eq!(c.len(), 2, "got: {c:?}");
        assert_eq!(c[0].text.trim(), "a");
        assert_eq!(c[1].text.trim(), "yes HERE");
    }

    #[test]
    fn extract_comments_char_literal_with_quote_does_not_open_string() {
        let src = "let q = '\"'; // after a char literal NOW\n";
        let c = extract_comments_rs(src);
        assert_eq!(c.len(), 1, "got: {c:?}");
        assert_eq!(c[0].text.trim(), "after a char literal NOW");
    }

    #[test]
    fn markdown_scans_prose_but_skips_fenced_code() {
        let md = "a HEADING line\n```\nFENCED code NOT scanned\n```\nmore PROSE here\n";
        let lines = markdown_lines(md);
        let texts: Vec<_> = lines.iter().map(|l| l.text.as_str()).collect();
        assert!(texts.iter().any(|t| t.contains("HEADING")));
        assert!(texts.iter().any(|t| t.contains("PROSE")));
        assert!(
            !texts.iter().any(|t| t.contains("FENCED")),
            "fenced code is skipped"
        );
    }

    #[test]
    fn lint_file_rs_reports_rule_and_line() {
        let src = "// leverage the SAME thing\nfn f() {}\n";
        let found = lint_file("src/x.rs", src, &rs());
        assert_eq!(found.len(), 2, "got: {found:?}");
        assert!(
            found
                .iter()
                .any(|f| f.rule == Rule::Emphatic && f.token == "leverage")
        );
        assert!(
            found
                .iter()
                .any(|f| f.rule == Rule::CapsEmphasis && f.token == "SAME")
        );
        assert!(found.iter().all(|f| f.line == 1));
    }

    #[test]
    fn lint_file_clean_comment_has_no_findings() {
        let src = "// this reads as plain prose about the CI job\nfn f() {}\n";
        let found = lint_file("src/x.rs", src, &rs());
        assert!(found.is_empty(), "got: {found:?}");
    }

    #[test]
    fn lint_file_does_not_fire_on_code_identifiers() {
        // A listed word here is an identifier in code, not comment prose, so it is out of scope.
        let src = "let leverage = 1; let robust = 2;\n";
        let found = lint_file("src/x.rs", src, &rs());
        assert!(
            found.is_empty(),
            "code identifiers are out of scope: {found:?}"
        );
    }

    #[test]
    fn shipped_ruleset_parses_and_carries_the_board_phrases() {
        let shipped = include_str!("../prose-style.toml");
        let r = parse_ruleset(shipped).expect("shipped ruleset parses");
        assert!(r.emphatics_lower.iter().any(|p| p == "load-bearing"));
        assert!(r.caps_allow.contains("HTTP"));
    }

    #[test]
    fn render_sorts_dedups_and_round_trips_through_parse() {
        let emph = vec![
            "leverage".to_string(),
            "load-bearing".to_string(),
            "leverage".to_string(),
        ];
        let caps = vec!["HTTP".to_string(), "CI".to_string()];
        let out = render_ruleset_toml(&emph, &caps);
        // emphatics appear sorted and deduped.
        let i_load = out.find("\"load-bearing\"").unwrap();
        let i_lev = out.find("\"leverage\"").unwrap();
        assert!(i_lev < i_load, "sorted: leverage before load-bearing");
        assert_eq!(out.matches("\"leverage\"").count(), 1, "deduped");
        // the render parses back as a valid ruleset.
        let r = parse_ruleset(&out).expect("rendered ruleset parses");
        assert!(r.emphatics_lower.iter().any(|p| p == "leverage"));
        assert!(r.caps_allow.contains("CI"));
    }

    #[test]
    fn render_is_idempotent() {
        let emph = vec!["it's worth noting".to_string(), "robust".to_string()];
        let caps = vec!["API".to_string()];
        let once = render_ruleset_toml(&emph, &caps);
        let (e2, c2) = parse_raw_lists(&once).unwrap();
        let twice = render_ruleset_toml(&e2, &c2);
        assert_eq!(once, twice, "a render of a parsed render is identical");
    }

    #[test]
    fn render_escapes_quotes_and_backslashes() {
        let out = render_ruleset_toml(&[r#"a "quote" and \slash"#.to_string()], &[]);
        assert!(out.contains(r#""a \"quote\" and \\slash","#), "got: {out}");
        parse_ruleset(&out).expect("escaped render parses");
    }

    #[test]
    fn parse_raw_lists_preserves_case_for_caps_allow() {
        let (emph, caps) = parse_raw_lists(
            r#"
            emphatics = ["Load-Bearing"]
            caps_allow = ["ASCII", "CI"]
        "#,
        )
        .unwrap();
        assert_eq!(emph, vec!["Load-Bearing"]);
        assert_eq!(caps, vec!["ASCII", "CI"]);
    }

    #[test]
    fn a_changed_board_list_makes_render_differ_so_drift_is_detectable() {
        let caps = vec!["CI".to_string()];
        let before = render_ruleset_toml(&["robust".to_string()], &caps);
        let after = render_ruleset_toml(&["robust".to_string(), "seamless".to_string()], &caps);
        assert_ne!(
            before, after,
            "an added board phrase changes the render (drift)"
        );
    }
}
