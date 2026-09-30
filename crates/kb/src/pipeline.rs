//! `pipeline` — the board-driven ingest pipeline (`kb pipeline --role uploader|embedder`). Port of the
//! Python `kb/pipeline.py`: two reactive stage-agents behind one task queue.
//!
//! A ticket = "get a source into the KB", flowing by assignee (the board pushes events to a task's
//! assignee — no polling): a producer files `task(assignee="uploader", metadata={source_type, source,
//! collection?})`; the **uploader** fetches/produces bytes, pins them to IPFS, records `{ipfs_cid,
//! content_type}`, and reassigns to `embedder`; the **embedder** cats the CID, parses by content-type,
//! chunks + embeds into the resolved collection, and marks the task done. The embedder is a single agent,
//! so GPU work is serial by construction.
//!
//! This module is being ported incrementally (decision #2 / board #238); this first piece is the PURE
//! routing/parse core — [`content_type_for`] (PDF detection) and [`collection_for`] (where a ticket's points
//! land) — which is the correctness-critical part (a wrong collection puts points in the wrong place), so it
//! is pinned to the Python behavior by unit tests. The IO layer (fetch_source, ipfs, the two handlers, and
//! the reactive webhook runtime) lands on top of the existing infra (board.rs/webhook.rs/ipfs.rs/extract.rs)
//! in follow-ups. Landed ahead of its callers, so it reads as dead code until then.
#![allow(dead_code)]

use serde_json::{Map, Value};

/// Content-type tag the uploader stamps on a ticket and the embedder dispatches on — the Python string
/// constants. `rustdoc-json` is set by the docs.rs fetch path; `pdf`/`text` come from [`content_type_for`].
pub const RUSTDOC_JSON: &str = "rustdoc-json";
pub const PDF: &str = "pdf";
pub const TEXT: &str = "text";

/// A ticket metadata string field, but only when present AND non-empty — mirrors Python truthiness
/// (`meta.get(k)` / `... or ...`, where `""` is falsy), which the collection/version fallbacks rely on.
fn truthy_str<'a>(meta: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    meta.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// Detect PDFs by extension, Content-Type header, or the `%PDF-` magic — the Python `_content_type_for`. A
/// PDF fetched over a URL must run page-wise text extraction, not be embedded as raw bytes. Everything else
/// is `text`. (`rustdoc-json` is tagged by the docs.rs fetch path, not here.)
pub fn content_type_for(name: &str, header: &str, data: &[u8]) -> &'static str {
    if name.to_lowercase().ends_with(".pdf")
        || header.to_lowercase().contains("application/pdf")
        || data.starts_with(b"%PDF-")
    {
        PDF
    } else {
        TEXT
    }
}

/// Resolve the collection a ticket's points land in — the Python `_collection_for`:
/// 1. an explicit `metadata.collection` wins;
/// 2. `rustdoc-json` → `crate.<crate>.<ver>` (crate = `meta.crate` else `meta.source`; ver =
///    `doc.crate_version` else `meta.version` else `latest`);
/// 3. otherwise derive `docs.<slug>` — the GitHub repo name for a github(usercontent) URL, else the whole
///    source, run through [`slugify`].
///
/// Only the rustdoc branch needs `data` (to read `crate_version`); a parse failure there is an error.
pub fn collection_for(
    content_type: &str,
    data: &[u8],
    meta: &Map<String, Value>,
) -> Result<String, String> {
    if let Some(c) = truthy_str(meta, "collection") {
        return Ok(c.to_string());
    }
    if content_type == RUSTDOC_JSON {
        let doc: Value = serde_json::from_slice(data)
            .map_err(|e| format!("pipeline: rustdoc JSON did not parse for collection: {e}"))?;
        // crate = meta.crate or meta.source; str(None) == "None" if truly absent (a malformed ticket).
        let crate_name = truthy_str(meta, "crate")
            .or_else(|| truthy_str(meta, "source"))
            .unwrap_or("None");
        let ver = doc
            .get("crate_version")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| truthy_str(meta, "version"))
            .unwrap_or("latest");
        return Ok(format!("crate.{crate_name}.{ver}"));
    }
    let src = truthy_str(meta, "raw_url")
        .or_else(|| truthy_str(meta, "url"))
        .or_else(|| truthy_str(meta, "source"))
        .unwrap_or("misc");
    let base = github_repo(src).unwrap_or_else(|| src.to_string());
    Ok(format!("docs.{}", slugify(&base)))
}

/// The GitHub repo name from a URL — the Python regex `github(?:usercontent)?\.com/[^/]+/([^/]+)` (owner
/// then repo). Returns the repo segment (group 1), or `None` if the URL isn't a github(usercontent) URL with
/// both an owner and a repo segment. Implemented by hand to avoid pulling the `regex` crate.
fn github_repo(src: &str) -> Option<String> {
    // "github.com" is not a substring of "githubusercontent.com", so the two markers are unambiguous.
    for marker in ["github.com/", "githubusercontent.com/"] {
        if let Some(pos) = src.find(marker) {
            let after = &src[pos + marker.len()..];
            let mut segs = after.splitn(3, '/'); // owner / repo / rest
            let owner = segs.next().unwrap_or("");
            let repo = segs.next().unwrap_or("");
            if !owner.is_empty() && !repo.is_empty() {
                return Some(repo.to_string());
            }
        }
    }
    None
}

/// Slugify for a `docs.<slug>` collection — the Python `re.sub(r"[^a-z0-9._-]+", "-", base.lower()).strip("-._")
/// or "misc"`: lowercase, collapse each run of chars outside `[a-z0-9._-]` to a single `-`, strip leading /
/// trailing `-._`, and fall back to `misc` if nothing is left.
fn slugify(base: &str) -> String {
    let mut s = String::with_capacity(base.len());
    let mut in_run = false;
    for c in base.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            s.push(c);
            in_run = false;
        } else if !in_run {
            s.push('-');
            in_run = true;
        }
    }
    let trimmed = s.trim_matches(|c| matches!(c, '-' | '.' | '_'));
    if trimmed.is_empty() {
        "misc".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn content_type_detects_pdf_by_ext_header_or_magic() {
        assert_eq!(content_type_for("a.pdf", "", b""), PDF);
        assert_eq!(content_type_for("A.PDF", "", b""), PDF); // case-insensitive
        assert_eq!(
            content_type_for("x", "application/pdf; charset=binary", b""),
            PDF
        );
        assert_eq!(content_type_for("x", "APPLICATION/PDF", b""), PDF); // header case-insensitive
        assert_eq!(content_type_for("noext", "", b"%PDF-1.7\n..."), PDF);
        assert_eq!(
            content_type_for("readme.md", "text/markdown", b"# hi"),
            TEXT
        );
        assert_eq!(content_type_for("", "", b""), TEXT);
    }

    #[test]
    fn github_repo_captures_repo_segment() {
        assert_eq!(
            github_repo("https://github.com/camshaft/fleet/blob/main/x.md").as_deref(),
            Some("fleet")
        );
        assert_eq!(
            github_repo("https://raw.githubusercontent.com/camshaft/cadenza/main/README.md")
                .as_deref(),
            Some("cadenza")
        );
        assert_eq!(
            github_repo("https://example.com/owner/repo").as_deref(),
            None
        );
        assert_eq!(github_repo("github.com/owner").as_deref(), None); // no repo segment
    }

    #[test]
    fn slugify_matches_python_rules() {
        assert_eq!(slugify("My Cool Repo!!"), "my-cool-repo");
        assert_eq!(slugify("__weird__"), "weird");
        assert_eq!(slugify("keep.dots_and-dashes"), "keep.dots_and-dashes");
        assert_eq!(slugify("///"), "misc");
        assert_eq!(slugify("Foo/Bar Baz"), "foo-bar-baz");
    }

    #[test]
    fn collection_for_explicit_wins() {
        let m =
            meta(json!({ "collection": "camshaft.notes", "source_type": "url", "source": "x" }));
        assert_eq!(collection_for(TEXT, b"", &m).unwrap(), "camshaft.notes");
    }

    #[test]
    fn collection_for_rustdoc_uses_crate_and_resolved_version() {
        // crate from meta.source, version from the doc's crate_version (not meta).
        let m = meta(json!({ "source": "anyhow", "version": "latest" }));
        let data = br#"{"crate_version":"1.0.104","index":{},"paths":{}}"#;
        assert_eq!(
            collection_for(RUSTDOC_JSON, data, &m).unwrap(),
            "crate.anyhow.1.0.104"
        );
        // meta.crate beats meta.source; version falls back to meta.version when the doc lacks it.
        let m2 = meta(json!({ "crate": "tokio", "source": "ignored", "version": "1.53.1" }));
        assert_eq!(
            collection_for(RUSTDOC_JSON, br#"{"index":{},"paths":{}}"#, &m2).unwrap(),
            "crate.tokio.1.53.1"
        );
    }

    #[test]
    fn collection_for_derives_docs_slug_from_url_or_source() {
        // GitHub URL -> docs.<repo>.
        let m = meta(
            json!({ "source_type": "url", "source": "https://github.com/camshaft/fleet/x.md" }),
        );
        assert_eq!(collection_for(TEXT, b"", &m).unwrap(), "docs.fleet");
        // raw_url preferred over source.
        let m2 = meta(json!({
            "raw_url": "https://raw.githubusercontent.com/camshaft/cadenza/main/R.md",
            "source": "whatever"
        }));
        assert_eq!(collection_for(TEXT, b"", &m2).unwrap(), "docs.cadenza");
        // Non-URL source -> slugified whole.
        let m3 = meta(json!({ "source": "My Local Doc.txt" }));
        assert_eq!(
            collection_for(TEXT, b"", &m3).unwrap(),
            "docs.my-local-doc.txt"
        );
        // Nothing usable -> docs.misc.
        let m4 = meta(json!({ "source_type": "url" }));
        assert_eq!(collection_for(TEXT, b"", &m4).unwrap(), "docs.misc");
    }
}
