//! `scan` — the commit-content guardrail's internal-MARKER scanner (task_782, doc_104).
//!
//! Why this exists: camshaft/fleet is a PUBLIC repo, and Amazon-internal identifiers (internal API / service /
//! operation / exception NAMES, internal hostnames, internal package prefixes) must never land in a public
//! commit. The fleet DATABASE / board is a fine store for internal detail; the leak surface is COMMITTED
//! public git. The driving incident is real and in this repo's own history: an internal-action CLI shipped
//! with internal identifier NAMES in tracked source and was reverted — a leak that carried no secret-shaped
//! string at all, so a secret scanner alone would never have caught it.
//!
//! doc_104's hybrid design splits the work: an external secret scanner (detect-secrets) owns secret /
//! credential detection, and THIS engine owns the internal-MARKER side — the vocabulary a secret scanner
//! cannot know. The marker VOCABULARY is deliberately NOT baked into this public source: it is loaded at
//! runtime from an external taxonomy file kept in a PRIVATE store (the security-specialist co-owner curates
//! it, task_1156). This engine ships only the mechanism and a placeholder example taxonomy, so the public repo
//! carries no internal vocabulary of its own.
//!
//! Matching is literal, case-insensitive substring. Internal markers are high-signal literal tokens and
//! prefixes — an internal package prefix catches a whole identifier family, an internal host suffix catches
//! every host under it — so a literal match is both sufficient and low-false-positive. A generic English
//! suffix (e.g. `Exception`, `Service`) is intentionally NOT a usable marker: it would drown the signal, so
//! the taxonomy carries specific identifiers and prefixes, not generic words.

use serde::Deserialize;

/// Severity of a marker hit. `Block` fails the scan (fail-closed on the public path); `Warn` is advisory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Block,
    Warn,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Severity::Block => "block",
            Severity::Warn => "warn",
        }
    }
}

/// One taxonomy entry as written in the TOML file: a `[[marker]]` table.
#[derive(Debug, Deserialize)]
struct MarkerSpec {
    /// Human-readable category for the report (e.g. `internal-hostname`).
    category: String,
    /// `block` (default) or `warn`. Any other value is rejected at parse time.
    #[serde(default)]
    severity: Option<String>,
    /// Literal tokens matched case-insensitively as substrings of a line.
    tokens: Vec<String>,
}

/// The on-disk taxonomy file: a list of `[[marker]]` tables.
#[derive(Debug, Deserialize)]
struct TaxonomyFile {
    #[serde(default)]
    marker: Vec<MarkerSpec>,
}

/// A compiled marker ready to match: one token with its category + severity. `token_lower` is pre-lowercased
/// for case-insensitive matching; `display_token` preserves the author-facing original.
#[derive(Debug, Clone)]
pub struct Marker {
    pub category: String,
    pub severity: Severity,
    token_lower: String,
    display_token: String,
}

/// A single hit found in scanned content. Carries the matched token (not the surrounding line) so a report
/// never echoes more of the offending content than the marker itself — and `--redact` suppresses even that.
#[derive(Debug, Clone)]
pub struct Hit {
    pub file: String,
    /// 1-based line number.
    pub line: usize,
    pub category: String,
    pub severity: Severity,
    pub token: String,
}

/// Parse a taxonomy TOML string into compiled markers. PURE (no I/O). Emits one [`Marker`] per (spec, token).
///
/// Rejects an unknown severity and an empty-string token. An empty taxonomy (zero markers) is an ERROR, not a
/// clean parse: a scan with no markers passes everything, which on the public path is fail-OPEN — the caller
/// wants that to surface as a configuration error, never as a green scan.
pub fn parse_taxonomy(toml_src: &str) -> Result<Vec<Marker>, String> {
    let parsed: TaxonomyFile =
        toml::from_str(toml_src).map_err(|e| format!("taxonomy parse error: {e}"))?;
    let mut markers = Vec::new();
    for spec in parsed.marker {
        let severity = match spec.severity.as_deref() {
            None | Some("block") => Severity::Block,
            Some("warn") => Severity::Warn,
            Some(other) => {
                return Err(format!(
                    "marker '{}' has unknown severity '{other}' (want block|warn)",
                    spec.category
                ));
            }
        };
        for token in spec.tokens {
            let t = token.trim();
            if t.is_empty() {
                return Err(format!("marker '{}' has an empty token", spec.category));
            }
            markers.push(Marker {
                category: spec.category.clone(),
                severity,
                token_lower: t.to_lowercase(),
                display_token: t.to_string(),
            });
        }
    }
    if markers.is_empty() {
        return Err(
            "taxonomy defines no markers — refusing to scan (a no-marker scan is fail-open)".to_string(),
        );
    }
    Ok(markers)
}

/// Scan one file's text for marker hits. PURE (no I/O). Case-insensitive substring match, line by line.
pub fn scan_text(file: &str, content: &str, markers: &[Marker]) -> Vec<Hit> {
    let mut hits = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let line_lower = line.to_lowercase();
        for m in markers {
            if line_lower.contains(&m.token_lower) {
                hits.push(Hit {
                    file: file.to_string(),
                    line: idx + 1,
                    category: m.category.clone(),
                    severity: m.severity,
                    token: m.display_token.clone(),
                });
            }
        }
    }
    hits
}

/// Does any hit block? This is the fail-closed signal: a blocking hit must stop a commit from landing.
pub fn has_blocking(hits: &[Hit]) -> bool {
    hits.iter().any(|h| h.severity == Severity::Block)
}

/// Resolve + load the marker taxonomy, returning compiled markers or a human-readable error. Looks at the
/// explicit path first, then `$FLEET_CONTENT_TAXONOMY`. A missing taxonomy is a hard error on the fail-closed
/// path — the error string begins `no taxonomy` so the caller can special-case `--allow-missing-taxonomy`.
pub fn load_markers(explicit: Option<&str>) -> Result<Vec<Marker>, String> {
    let path = explicit
        .map(str::to_string)
        .or_else(|| std::env::var("FLEET_CONTENT_TAXONOMY").ok())
        .ok_or_else(|| {
            "no taxonomy: pass --taxonomy <path> or set FLEET_CONTENT_TAXONOMY (the marker vocabulary lives \
             in a private store, never in this public repo)"
                .to_string()
        })?;
    let src =
        std::fs::read_to_string(&path).map_err(|e| format!("cannot read taxonomy '{path}': {e}"))?;
    parse_taxonomy(&src)
}

/// The staged content of every added/copied/modified file in the index, as (path, text) pairs, via `git`.
/// Binary / non-UTF-8 blobs are skipped (nothing text-scannable); a deleted file has no staged content.
fn staged_contents() -> Vec<(String, String)> {
    let listing = std::process::Command::new("git")
        .args(["diff", "--cached", "--name-only", "--diff-filter=ACM"])
        .output();
    let names = match listing {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => {
            eprintln!("scan-content: `git diff --cached` failed (not a git repo?)");
            return vec![];
        }
    };
    let mut out = Vec::new();
    for name in names.lines().map(str::trim).filter(|s| !s.is_empty()) {
        let Ok(b) = std::process::Command::new("git")
            .args(["show", &format!(":{name}")])
            .output()
        else {
            continue;
        };
        if !b.status.success() {
            continue;
        }
        if let Ok(text) = String::from_utf8(b.stdout) {
            out.push((name.to_string(), text));
        }
    }
    out
}

/// Options for [`scan_content`], bundled into a struct so the handler stays clippy-clean (a flat bool pile
/// trips `clippy::fn_params_excessive_bools`).
pub struct ScanOpts {
    /// Explicit taxonomy path; falls back to `$FLEET_CONTENT_TAXONOMY`.
    pub taxonomy: Option<String>,
    /// Explicit files to scan from disk; empty means scan the staged index.
    pub files: Vec<String>,
    /// Force scanning the staged index even when `--file`s are given.
    pub staged: bool,
    /// Downgrade every blocking hit to advisory (report, exit 0).
    pub warn_only: bool,
    /// Print only `file:line: category`, never the matched token (for public CI logs).
    pub redact: bool,
    /// Treat a missing taxonomy as a skip (exit 0) instead of a fail-closed config error.
    pub allow_missing_taxonomy: bool,
}

/// `fleet scan-content` — scan staged (or given) content for internal-marker hits (task_782 mechanism).
///
/// Exit codes: 0 = clean (or advisory-only / taxonomy skipped), 1 = a blocking hit (fail-closed), 2 = a
/// configuration error (no or invalid taxonomy). The non-zero blocking exit is what lets this gate a commit
/// (as a pre-commit hook) or a PR (as a CI required status check).
pub fn scan_content(opts: ScanOpts) {
    let markers = match load_markers(opts.taxonomy.as_deref()) {
        Ok(m) => m,
        Err(e) => {
            if opts.allow_missing_taxonomy && e.starts_with("no taxonomy") {
                eprintln!("scan-content: {e}; --allow-missing-taxonomy set, skipping (NOT fail-closed)");
                return;
            }
            eprintln!("scan-content: {e}");
            std::process::exit(2);
        }
    };

    let targets: Vec<(String, String)> = if !opts.files.is_empty() && !opts.staged {
        opts.files
            .iter()
            .filter_map(|f| std::fs::read_to_string(f).ok().map(|c| (f.clone(), c)))
            .collect()
    } else {
        staged_contents()
    };

    let mut all_hits = Vec::new();
    for (file, content) in &targets {
        all_hits.extend(scan_text(file, content, &markers));
    }

    if all_hits.is_empty() {
        println!(
            "scan-content: clean ({} marker(s), {} file(s) scanned)",
            markers.len(),
            targets.len()
        );
        return;
    }

    for h in &all_hits {
        if opts.redact {
            eprintln!("{}:{}: {} [{}]", h.file, h.line, h.category, h.severity.as_str());
        } else {
            eprintln!(
                "{}:{}: {} [{}] matched '{}'",
                h.file,
                h.line,
                h.category,
                h.severity.as_str(),
                h.token
            );
        }
    }

    let blocking = has_blocking(&all_hits);
    if blocking && !opts.warn_only {
        eprintln!(
            "scan-content: {} hit(s) — BLOCKED (internal content must not reach a public repo)",
            all_hits.len()
        );
        std::process::exit(1);
    }
    eprintln!("scan-content: {} hit(s) (advisory)", all_hits.len());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_taxonomy_compiles_categories_severities_and_tokens() {
        let src = r#"
            [[marker]]
            category = "example-host"
            tokens = ["example-internal.invalid"]

            [[marker]]
            category = "example-advisory"
            severity = "warn"
            tokens = ["REVIEW-ME", "ALSO-REVIEW"]
        "#;
        let markers = parse_taxonomy(src).expect("valid taxonomy parses");
        // one marker from the first spec + two from the second = three compiled markers.
        assert_eq!(markers.len(), 3);
        // default severity is block.
        assert_eq!(markers[0].severity, Severity::Block);
        assert_eq!(markers[0].category, "example-host");
        // explicit warn carries through to each token.
        assert_eq!(markers[1].severity, Severity::Warn);
        assert_eq!(markers[2].severity, Severity::Warn);
    }

    #[test]
    fn parse_taxonomy_rejects_unknown_severity() {
        let src = r#"
            [[marker]]
            category = "x"
            severity = "nuke"
            tokens = ["t"]
        "#;
        let err = parse_taxonomy(src).unwrap_err();
        assert!(err.contains("unknown severity"), "got: {err}");
    }

    #[test]
    fn parse_taxonomy_rejects_empty_token() {
        let src = r#"
            [[marker]]
            category = "x"
            tokens = ["  "]
        "#;
        let err = parse_taxonomy(src).unwrap_err();
        assert!(err.contains("empty token"), "got: {err}");
    }

    #[test]
    fn parse_taxonomy_rejects_a_no_marker_taxonomy_as_fail_open() {
        // An empty taxonomy would scan nothing and pass everything — fail-OPEN on the public path, so it must
        // be an error, not a clean parse.
        let err = parse_taxonomy("").unwrap_err();
        assert!(err.contains("no markers"), "got: {err}");
    }

    #[test]
    fn scan_text_finds_case_insensitive_substring_with_line_number() {
        let markers = parse_taxonomy(
            r#"
            [[marker]]
            category = "example-prefix"
            tokens = ["com.example.internal."]
        "#,
        )
        .unwrap();
        let content = "fn ok() {}\nlet target = \"COM.EXAMPLE.INTERNAL.SomeService\";\nfn also_ok() {}";
        let hits = scan_text("src/x.rs", content, &markers);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 2, "1-based line of the hit");
        assert_eq!(hits[0].category, "example-prefix");
        assert_eq!(hits[0].severity, Severity::Block);
    }

    #[test]
    fn scan_text_clean_content_has_no_hits() {
        let markers = parse_taxonomy(
            r#"
            [[marker]]
            category = "example-host"
            tokens = ["example-internal.invalid"]
        "#,
        )
        .unwrap();
        let hits = scan_text("src/x.rs", "fn main() { println!(\"hello\"); }", &markers);
        assert!(hits.is_empty());
        assert!(!has_blocking(&hits));
    }

    #[test]
    fn has_blocking_distinguishes_block_from_warn() {
        fn hit(severity: Severity) -> Hit {
            Hit {
                file: "a".into(),
                line: 1,
                category: "c".into(),
                severity,
                token: "t".into(),
            }
        }
        assert!(has_blocking(&[hit(Severity::Block)]));
        assert!(!has_blocking(&[hit(Severity::Warn)]));
        assert!(
            has_blocking(&[hit(Severity::Warn), hit(Severity::Block)]),
            "any blocking hit blocks"
        );
    }

    #[test]
    fn catches_an_internal_identifier_name_with_no_secret_the_task_1124_acceptance_case() {
        // task_1124 / camshaft/fleet#340: the real leak was an internal API identifier NAME carrying NO
        // secret-shaped string, so a secret scanner would miss it entirely. This proves the internal-marker
        // engine catches that class. PLACEHOLDER tokens stand in for the real (private) taxonomy vocabulary.
        let markers = parse_taxonomy(
            r#"
            [[marker]]
            category = "example-coral-target"
            tokens = ["com.example.internal.ExampleService.ExampleOperation"]
        "#,
        )
        .unwrap();
        let leak = "const TARGET: &str = \"com.example.internal.ExampleService.ExampleOperation\";";
        let hits = scan_text("src/main.rs", leak, &markers);
        assert_eq!(hits.len(), 1);
        assert!(has_blocking(&hits), "an internal identifier name is a blocking hit");
    }

    #[test]
    fn shipped_example_taxonomy_parses() {
        // Guard: the placeholder example we ship must stay valid so a developer copying it as a starting point
        // gets a working file.
        let example = include_str!("../content-taxonomy.example.toml");
        let markers = parse_taxonomy(example).expect("shipped example taxonomy parses");
        assert!(!markers.is_empty());
    }
}
