//! `wiki_sync` — the board-wiki → KB auto-sync connector core (task_1089).
//!
//! Keeps the KB current with the board wiki: when a document version is APPROVED for publish, the approved
//! markdown is (re-)ingested into a dedicated board-wiki collection; when a document is archived/deleted, its
//! points are removed. Point ids are keyed on (document path, chunk index) and are VERSION-INDEPENDENT, so a
//! re-approval overwrites the prior text in place (idempotent re-ingest) and a shrinking document leaves only
//! a stale tail of higher-index points to cull.
//!
//! This module is the pure, runtime-free core — event classification, the scope gate, and the cull decision —
//! mirroring how `webhook` landed its tested core ahead of its workers. The reactive worker loop (a board-wide
//! "doc" subscription + a reconcile-poll backfill), the Qdrant upsert/delete, and the `kb wiki-sync` CLI role
//! build on these functions and land next, so the surface reads as dead code until then.
//!
//! Trigger design (cameron, comment_5099 / comment_5110): "update when versions get approved for publish" ⇒
//! the primary trigger is `document.approved` (verified in the live event log — it carries the top-level
//! `document_id`, an `approved_version_id`, and `status: "approved"`). A `document.version_published` also
//! fires for an in-REVIEW publish, so it is only an ingest trigger when it carries `status: "approved"`. Every
//! ingest trigger is just a signal to LOOK: the worker re-reads the document (get_document) and applies the
//! scope gate before any write, exactly as the `webhook` workers re-read their task.

// The pure core is landed ahead of its callers (the worker loop + CLI role), so the surface reads as dead
// code until they land — the same staging the sibling `webhook`/`chunk` modules use.
#![allow(dead_code)]

use serde_json::Value;

/// The payload `source` tag and id-namespace for board-wiki points. Stable and distinct from the
/// inbox/pipeline/crate sources, so a point id is version-independent per document.
pub const SOURCE: &str = "board-wiki";

/// Per-repo scratch tree, excluded from the KB — approval is the gate, and these are uncurated drafts.
const EXCLUDED_PREFIX: &str = "repos/";

/// A board document event, parsed from a webhook POST body or an SSE `/events` frame. The board carries the
/// document id both top-level and inside `data`; the version/status details ride in `data`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocEvent {
    pub event_type: String,
    pub doc_id: Option<i64>,
    pub data: Value,
}

/// What to do with the KB in response to a document event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocAction {
    /// (Re-)ingest the document's approved version. The worker re-reads the doc with get_document and applies
    /// the scope gate ([`in_scope`]) before writing — an event only triggers the look, never a blind write.
    Ingest { doc_id: i64 },
    /// Remove all of the document's points (archived / deleted / deprecated).
    Remove { doc_id: i64 },
    /// Not relevant to the KB (a draft edit, an in-review publish, an unrelated event type, no doc id).
    Ignore,
}

/// Parse a board event body into a [`DocEvent`]. Reads the doc id from the top-level `document_id` (the shape
/// the board emits), falling back to `data.document_id` / `data.id`.
pub fn parse_doc_event(body: &str) -> Result<DocEvent, String> {
    let v: Value =
        serde_json::from_str(body).map_err(|e| format!("wiki_sync: body was not JSON: {e}"))?;
    let event_type = v
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("wiki_sync: event has no `type`: {v}"))?
        .to_string();
    let data = v.get("data").cloned().unwrap_or(Value::Null);
    let doc_id = v
        .get("document_id")
        .and_then(Value::as_i64)
        .or_else(|| data.get("document_id").and_then(Value::as_i64))
        .or_else(|| data.get("id").and_then(Value::as_i64));
    Ok(DocEvent {
        event_type,
        doc_id,
        data,
    })
}

/// Classify a document event into a KB action. `document.approved` is the primary ingest trigger; a
/// `document.version_published` is a trigger ONLY when it carries `status: "approved"` (the approved version
/// being re-published) — a plain in-review publish is ignored. Archive/delete/deprecate remove the doc's
/// points. Everything else is ignored. The worker still re-reads the document and applies [`in_scope`] before
/// any write, so a false Ingest is a wasted look, not a bad write.
pub fn classify(event: &DocEvent) -> DocAction {
    let status = event.data.get("status").and_then(Value::as_str);
    match event.event_type.as_str() {
        "document.approved" => ingest_or_ignore(event.doc_id),
        "document.version_published" if status == Some("approved") => {
            ingest_or_ignore(event.doc_id)
        }
        "document.archived" | "document.deleted" | "document.deprecated" => match event.doc_id {
            Some(id) => DocAction::Remove { doc_id: id },
            None => DocAction::Ignore,
        },
        _ => DocAction::Ignore,
    }
}

fn ingest_or_ignore(doc_id: Option<i64>) -> DocAction {
    match doc_id {
        Some(id) => DocAction::Ingest { doc_id: id },
        None => DocAction::Ignore,
    }
}

/// Whether a document belongs in the KB board-wiki collection: it must have an approved version and must not
/// be filed under the excluded `repos/` scratch tree. Approval is the gate — the curated canon (charters,
/// tenets, runbooks, designs, guides, roles, capabilities) carries an approved version; drafts do not. An
/// unfiled document (no wiki path) is out of scope.
pub fn in_scope(wiki_path: Option<&str>, has_approved_version: bool) -> bool {
    has_approved_version && matches!(wiki_path, Some(p) if !is_excluded_path(p))
}

/// A path under the excluded per-repo scratch tree (leading slash tolerated).
fn is_excluded_path(path: &str) -> bool {
    path.trim_start_matches('/').starts_with(EXCLUDED_PREFIX)
}

/// Deterministic, VERSION-INDEPENDENT point id for chunk `chunk_index` of the wiki doc at `wiki_path`:
/// `uuid(md5("board-wiki|<path>|0|<chunk>"))`. Because the id ignores the version, a re-approval overwrites
/// the prior text's points in place (idempotent re-ingest), and only the stale tail of a shrunk document
/// needs culling ([`stale_chunk_indices`]). The `0` page component matches the text path's `page=None` shape.
pub fn point_id(wiki_path: &str, chunk_index: usize) -> String {
    crate::chunk::id(&[SOURCE, wiki_path, "0", &chunk_index.to_string()])
}

/// The chunk indices whose points are now stale after a re-ingest changed a document from `old_chunk_count`
/// to `new_chunk_count` chunks. The new write overwrites indices `[0, new)` in place; the tail `[new, old)`
/// is the old document's leftover and must be deleted. Empty when the document grew or stayed the same size.
pub fn stale_chunk_indices(
    old_chunk_count: usize,
    new_chunk_count: usize,
) -> std::ops::Range<usize> {
    new_chunk_count..old_chunk_count.max(new_chunk_count)
}

/// The point ids to delete to cull a shrunk document's stale tail (see [`stale_chunk_indices`]).
pub fn stale_point_ids(
    wiki_path: &str,
    old_chunk_count: usize,
    new_chunk_count: usize,
) -> Vec<String> {
    stale_chunk_indices(old_chunk_count, new_chunk_count)
        .map(|i| point_id(wiki_path, i))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(body: serde_json::Value) -> DocEvent {
        parse_doc_event(&body.to_string()).unwrap()
    }

    #[test]
    fn parse_reads_type_and_top_level_doc_id() {
        let e = ev(json!({
            "type": "document.approved",
            "document_id": 3354,
            "data": {"approved_version_id": 5172, "status": "approved", "title": "x"}
        }));
        assert_eq!(e.event_type, "document.approved");
        assert_eq!(e.doc_id, Some(3354));
    }

    #[test]
    fn parse_falls_back_to_data_doc_id() {
        let e = ev(json!({"type": "document.approved", "data": {"document_id": 42}}));
        assert_eq!(e.doc_id, Some(42));
        let e2 = ev(json!({"type": "document.approved", "data": {"id": 7}}));
        assert_eq!(e2.doc_id, Some(7));
    }

    #[test]
    fn parse_missing_type_is_error_missing_data_is_null() {
        assert!(parse_doc_event(r#"{"document_id":1}"#).is_err());
        assert!(parse_doc_event("not json").is_err());
        let e = ev(json!({"type": "document.approved", "document_id": 1}));
        assert_eq!(e.data, Value::Null);
        assert_eq!(e.doc_id, Some(1));
    }

    #[test]
    fn approved_is_ingest() {
        let e = ev(json!({"type": "document.approved", "document_id": 3354,
            "data": {"status": "approved"}}));
        assert_eq!(classify(&e), DocAction::Ingest { doc_id: 3354 });
    }

    #[test]
    fn version_published_ingests_only_when_approved() {
        let approved = ev(
            json!({"type": "document.version_published", "document_id": 9,
            "data": {"status": "approved", "version_no": 3}}),
        );
        assert_eq!(classify(&approved), DocAction::Ingest { doc_id: 9 });

        // An in-review publish is NOT an ingest trigger — approval is the gate.
        let in_review = ev(
            json!({"type": "document.version_published", "document_id": 9,
            "data": {"status": "in_review", "version_no": 3}}),
        );
        assert_eq!(classify(&in_review), DocAction::Ignore);
    }

    #[test]
    fn archive_delete_deprecate_remove() {
        for t in [
            "document.archived",
            "document.deleted",
            "document.deprecated",
        ] {
            let e = ev(json!({"type": t, "document_id": 11}));
            assert_eq!(classify(&e), DocAction::Remove { doc_id: 11 }, "type {t}");
        }
    }

    #[test]
    fn unrelated_or_idless_events_ignored() {
        for t in [
            "document.created",
            "document.updated",
            "document.submitted_for_operator_review",
        ] {
            let e = ev(json!({"type": t, "document_id": 1}));
            assert_eq!(classify(&e), DocAction::Ignore, "type {t}");
        }
        // An approved event with no resolvable doc id can't be acted on.
        let no_id = ev(json!({"type": "document.approved", "data": {"status": "approved"}}));
        assert_eq!(classify(&no_id), DocAction::Ignore);
    }

    #[test]
    fn scope_requires_approved_and_excludes_repos() {
        assert!(in_scope(Some("charters/v-nix"), true));
        assert!(in_scope(Some("tenets/async-io"), true));
        // Not approved -> out, whatever the path.
        assert!(!in_scope(Some("charters/v-nix"), false));
        // repos/ scratch tree -> out even when approved.
        assert!(!in_scope(Some("repos/cadenza/scratch"), true));
        assert!(!in_scope(Some("/repos/x"), true)); // leading slash tolerated
        // No wiki path -> out.
        assert!(!in_scope(None, true));
    }

    #[test]
    fn point_id_is_deterministic_and_version_independent() {
        let a = point_id("charters/v-nix", 0);
        let b = point_id("charters/v-nix", 0);
        assert_eq!(a, b); // same path+index -> same id, regardless of version
        assert_ne!(a, point_id("charters/v-nix", 1)); // different chunk -> different id
        assert_ne!(a, point_id("tenets/async-io", 0)); // different doc -> different id
        assert_eq!(a.len(), 36); // canonical UUID
    }

    #[test]
    fn stale_indices_cover_only_the_shrunk_tail() {
        assert_eq!(stale_chunk_indices(5, 2), 2..5); // shrank 5 -> 2: delete 2,3,4
        assert!(stale_chunk_indices(2, 5).is_empty()); // grew: nothing stale
        assert!(stale_chunk_indices(3, 3).is_empty()); // same size: nothing stale
        assert!(stale_chunk_indices(0, 0).is_empty());
    }

    #[test]
    fn stale_point_ids_match_point_id_for_the_tail() {
        let ids = stale_point_ids("charters/v-nix", 5, 2);
        assert_eq!(ids.len(), 3);
        assert_eq!(ids[0], point_id("charters/v-nix", 2));
        assert_eq!(ids[2], point_id("charters/v-nix", 4));
        assert!(stale_point_ids("charters/v-nix", 2, 5).is_empty());
    }
}
