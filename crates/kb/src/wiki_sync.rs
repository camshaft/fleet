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

use serde_json::{Map, Value};

use crate::{board, chunk, config, curate, embed, store};

/// The payload `source` tag and id-namespace for board-wiki points. Stable and distinct from the
/// inbox/pipeline/crate sources, so a point id is version-independent per document.
pub const SOURCE: &str = "board-wiki";

/// Payload `kind` for board-wiki points — the curated canon is documentation (authority 0.8, non-decaying).
const KIND: &str = "doc";

/// All board-wiki chunks are stamped `page = 0` (a doc is one logical unit, like the text ingest path), so a
/// per-document scroll filtered on `page == 0` recovers exactly the doc's chunk set for the cull count.
const PAGE: i64 = 0;

/// Upper bound on a single document's chunk count when scrolling its existing points for the cull — far above
/// any real board doc, so the scroll returns the whole set in one page.
const MAX_DOC_CHUNKS: usize = 10_000;

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

/// Ingest a document's approved version into the board-wiki collection, culling any stale tail — the Ingest
/// arm of the connector. Fetches the doc with its body, applies the [`in_scope`] gate (returns `Ok(None)`
/// when out of scope — a draft, an unfiled doc, or the `repos/` scratch tree), then chunks + embeds the
/// markdown and upserts with version-independent ids ([`point_id`]) so a re-approval overwrites in place.
/// After writing the new `[0, new)` chunks it deletes the old document's leftover `[new, old)` tail
/// ([`stale_point_ids`]). Returns `Ok(Some(chunk_count))` when ingested, `Ok(None)` when skipped.
pub async fn ingest_document(
    docs: &board::Documents,
    store: &store::Store,
    doc_id: i64,
) -> Result<Option<usize>, String> {
    let doc = docs.get_document(doc_id, true).await?;
    if !in_scope(doc.path.as_deref(), doc.has_approved_version) {
        return Ok(None);
    }
    // in_scope guaranteed a path.
    let path = doc.path.clone().expect("in_scope requires a path");
    let body = doc.body.ok_or_else(|| {
        format!("wiki_sync: document {doc_id} ({path}) returned no body to ingest")
    })?;

    let cfg = config::get();
    let collection = cfg.wiki_collection.clone();

    // Existing chunk count for this doc (page==0, exact path), to know what tail to cull after the rewrite.
    let old_count = store
        .read_pages(&collection, &path, PAGE, PAGE, MAX_DOC_CHUNKS)
        .await?
        .len();

    let chunks = chunk::chunk_default(&body);
    let new_count = chunks.len();

    if new_count > 0 {
        // Embed off the reactor (CPU-heavy model) — the pipeline embedder's spawn_blocking pattern (#439).
        let texts = chunks.clone();
        let vectors = tokio::task::spawn_blocking(move || embed::embed_docs(&texts))
            .await
            .map_err(|e| format!("wiki_sync: embed task panicked: {e}"))??;
        let dim = embed::dim()?;
        store.ensure_collection(&collection, dim).await?;

        // The shared per-document payload, assembled once and merged into each chunk.
        let meta = doc_meta(
            doc_id,
            &path,
            &doc.title,
            doc.version_id,
            doc.current_version_cid.as_deref(),
        );
        let points: Vec<(String, Vec<f32>, Map<String, Value>)> = chunks
            .into_iter()
            .zip(vectors)
            .enumerate()
            .map(|(idx, (text, vector))| {
                (
                    point_id(&path, idx),
                    vector,
                    chunk_payload(cfg, &meta, &text, idx),
                )
            })
            .collect();
        store.upsert(&collection, &points).await?;
    }

    // Cull the shrunk tail (empty when the doc grew/stayed the same, or on a first ingest).
    let stale = stale_point_ids(&path, old_count, new_count);
    store.delete_points(&collection, &stale).await?;

    Ok(Some(new_count))
}

/// Remove all of a document's points from the board-wiki collection — the Remove arm (archived / deleted /
/// deprecated). Resolves the doc's path, counts its existing chunks, and deletes `point_id(path, 0..old)`.
/// Returns the number of points removed. A doc with no resolvable path removes nothing (nothing to key on).
pub async fn remove_document(
    docs: &board::Documents,
    store: &store::Store,
    doc_id: i64,
) -> Result<usize, String> {
    let doc = docs.get_document(doc_id, false).await?;
    let Some(path) = doc.path else {
        tracing::warn!("wiki_sync: document {doc_id} has no path; nothing to remove");
        return Ok(0);
    };
    let cfg = config::get();
    let collection = cfg.wiki_collection.clone();
    let old_count = store
        .read_pages(&collection, &path, PAGE, PAGE, MAX_DOC_CHUNKS)
        .await?
        .len();
    let ids: Vec<String> = (0..old_count).map(|i| point_id(&path, i)).collect();
    store.delete_points(&collection, &ids).await?;
    Ok(old_count)
}

/// One reconcile pass over the board wiki: (re)ingest every approved, in-scope document whose current version
/// is not already present, skipping unchanged docs. This is both the first-run backfill and the ongoing drift
/// catch-up. The scope gate is applied from the wiki INDEX (one `list_wiki` call) so only a changed or new
/// approved doc incurs a body fetch + embed; an already-ingested doc is detected by matching the stored
/// `version_id` on its chunk 0 against the index's `current_version_id`. Returns (ingested, scanned).
pub async fn reconcile(
    docs: &board::Documents,
    store: &store::Store,
) -> Result<(usize, usize), String> {
    let collection = &config::get().wiki_collection;
    let entries = docs.list_wiki(None).await?;
    let scanned = entries.len();
    let mut ingested = 0usize;
    for e in entries {
        // Scope gate straight off the index: an approved version + a path not under the excluded tree.
        if !in_scope(e.path.as_deref(), e.has_approved_version()) {
            continue;
        }
        let path = e.path.as_deref().expect("in_scope requires a path");
        // Skip if chunk 0 is already present at this version (the cheap change check — no body fetch).
        if let Some(c) = store.get_point(collection, &point_id(path, 0)).await? {
            let stored = c.payload.get("version_id").and_then(Value::as_i64);
            if stored.is_some() && stored == e.current_version_id {
                continue;
            }
        }
        match ingest_document(docs, store, e.id).await {
            Ok(Some(n)) => {
                ingested += 1;
                tracing::info!("wiki_sync: ingested {path} (doc {}, {n} chunks)", e.id);
            }
            Ok(None) => {} // raced out of scope between the index read and the fetch
            Err(err) => tracing::warn!("wiki_sync: ingest {path} (doc {}) failed: {err}", e.id),
        }
    }
    Ok((ingested, scanned))
}

/// Run the board-wiki -> KB auto-sync worker (`kb wiki-sync`): an initial backfill then a reconcile poll on
/// the configured interval. A reactive doc-event webhook path (cutting freshness latency from the poll
/// interval to instant via [`classify`]) is a follow-on; the poll is the robust core and the sole backstop
/// while cross-host webhook wake is unreliable (task_1047).
pub async fn run() -> Result<(), String> {
    let docs = board::Documents::connect();
    let store = store::Store::connect()?;
    let poll = config::get().pipeline_poll_secs.max(60);
    let collection = config::get().wiki_collection.clone();
    tracing::info!("kb wiki-sync: reconcile poll every {poll}s into '{collection}'");
    loop {
        match reconcile(&docs, &store).await {
            Ok((ingested, scanned)) => tracing::info!(
                "wiki_sync: reconcile done ({ingested} (re)ingested / {scanned} wiki docs scanned)"
            ),
            Err(e) => tracing::warn!("wiki_sync: reconcile pass failed: {e}"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(poll)).await;
    }
}

/// The per-document payload fields shared by every chunk (identity + provenance + citation): assembled once
/// per ingest and merged into each chunk's `base_payload`.
fn doc_meta(
    doc_id: i64,
    path: &str,
    title: &Option<String>,
    version_id: Option<i64>,
    version_cid: Option<&str>,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("source".into(), Value::from(SOURCE));
    m.insert("source_doc".into(), Value::from(format!("doc_{doc_id}")));
    m.insert("path".into(), Value::from(path));
    m.insert("page".into(), Value::from(PAGE));
    if let Some(t) = title {
        m.insert("title".into(), Value::from(t.as_str()));
    }
    // The change key a reconcile pass reads off chunk 0 to skip a doc already ingested at this version.
    if let Some(vid) = version_id {
        m.insert("version_id".into(), Value::from(vid));
    }
    if let Some(cid) = version_cid {
        m.insert("version_cid".into(), Value::from(cid));
    }
    m
}

/// Build one chunk's payload: the shared `doc_meta` plus this chunk's `text` and `chunk` index, stamped with
/// the curation defaults ([`curate::base_payload`], `kind = "doc"`).
fn chunk_payload(
    cfg: &config::Config,
    doc_meta: &Map<String, Value>,
    text: &str,
    chunk_idx: usize,
) -> Map<String, Value> {
    let mut extra = doc_meta.clone();
    extra.insert("text".into(), Value::from(text));
    extra.insert("chunk".into(), Value::from(chunk_idx as i64));
    curate::base_payload(cfg, KIND, None, extra)
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
    fn chunk_payload_carries_identity_provenance_and_curation() {
        let cfg = config::Config::default();
        let meta = doc_meta(
            3354,
            "charters/v-nix",
            &Some("Charter: v-nix".to_string()),
            Some(5190),
            Some("QmAbc"),
        );
        let p = chunk_payload(&cfg, &meta, "the chunk text", 2);
        // chunk-specific
        assert_eq!(p.get("text").unwrap(), "the chunk text");
        assert_eq!(p.get("chunk").unwrap(), 2);
        // shared identity + provenance
        assert_eq!(p.get("source").unwrap(), SOURCE);
        assert_eq!(p.get("source_doc").unwrap(), "doc_3354");
        assert_eq!(p.get("path").unwrap(), "charters/v-nix");
        assert_eq!(p.get("page").unwrap(), 0);
        assert_eq!(p.get("title").unwrap(), "Charter: v-nix");
        assert_eq!(p.get("version_id").unwrap(), 5190);
        assert_eq!(p.get("version_cid").unwrap(), "QmAbc");
        // curation defaults from base_payload: kind=doc (authority 0.8), active
        assert_eq!(p.get("kind").unwrap(), "doc");
        assert_eq!(p.get("status").unwrap(), "active");
        assert!((p.get("authority").unwrap().as_f64().unwrap() - 0.8).abs() < 1e-9);
    }

    #[test]
    fn doc_meta_omits_absent_title_and_cid() {
        let m = doc_meta(7, "tenets/x", &None, None, None);
        assert_eq!(m.get("source_doc").unwrap(), "doc_7");
        assert!(!m.contains_key("title"));
        assert!(!m.contains_key("version_id"));
        assert!(!m.contains_key("version_cid"));
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
