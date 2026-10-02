//! `board` — kb's board client. Task CRUD is a thin re-export of the shared `bridge-core` task-client
//! (task_355). This module adds a small async document client for the wiki-sync connector (task_1089): the
//! board exposes a document/wiki REST surface the shared task-client does not cover, so kb reads it directly
//! with async `reqwest` (operator directive #439: no blocking IO on the tokio runtime — unlike the fleet
//! dreamer, which reads the same routes with blocking `ureq` in a background CLI pass).
//!
//! Routes (verified against the live board + the fleet dreamer's reader): `GET /wiki?prefix=<p>` returns the
//! wiki index as a JSON array of `{id, path, ...}`; `GET /documents/{id}?include_body=true` returns one
//! document with `path`, `title`, `status`, `approved_version_id`, `metadata`, `current_version`, and (with
//! `include_body`) the current version's markdown as `body`, fetched server-side from its pinned CID.

pub use bridge_core::task_client::{Board, Task};

use serde_json::Value;

/// Build a task client for the configured `board_url`, acting as `agent_id`.
pub fn connect(agent_id: impl Into<String>) -> Board {
    Board::with_base(&crate::config::get().board_url, agent_id)
}

/// One board wiki-index entry (`GET /wiki`): the document id and its filed wiki path. The index carries more,
/// but the wiki-sync reconcile pass only needs the id (to fetch) and the path (for the scope gate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiEntry {
    pub id: i64,
    pub path: Option<String>,
}

/// A board document (`GET /documents/{id}`), reduced to what the wiki-sync connector needs: identity + path +
/// the approval signal for the scope gate, plus the body/CID for ingest when `include_body` was requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub id: i64,
    pub path: Option<String>,
    pub title: Option<String>,
    pub status: Option<String>,
    /// Whether the document has an approved version (`approved_version_id` present / `status == "approved"`).
    /// This is the wiki-sync scope gate — approval is what admits a doc into the KB board-wiki collection.
    pub has_approved_version: bool,
    /// The current version's markdown, present only when fetched with `include_body=true`.
    pub body: Option<String>,
    /// The current version's content id, used to stamp provenance / detect an unchanged re-ingest.
    pub current_version_cid: Option<String>,
}

/// Coerce a wiki-index entry id, which the board emits as a number or a numeric string, to i64.
fn id_as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Parse the `GET /wiki` response (a JSON array of entries) into [`WikiEntry`] rows, skipping any entry with
/// no resolvable id. Pure, so the worker's listing is unit-tested without a live board.
pub fn parse_wiki_index(v: &Value) -> Vec<WikiEntry> {
    v.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|it| {
                    let id = id_as_i64(it.get("id")?)?;
                    let path = it.get("path").and_then(Value::as_str).map(str::to_string);
                    Some(WikiEntry { id, path })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a `GET /documents/{id}` response into a [`Document`]. Returns `None` if the id is missing/unparseable.
/// `has_approved_version` is true when `approved_version_id` is non-null OR `status == "approved"`; `body` and
/// `current_version_cid` are read when present (body only arrives with `include_body=true`). Pure + tested.
pub fn parse_document(v: &Value) -> Option<Document> {
    let id = id_as_i64(v.get("id")?)?;
    let status = v.get("status").and_then(Value::as_str).map(str::to_string);
    let approved = v
        .get("approved_version_id")
        .map(|a| !a.is_null())
        .unwrap_or(false)
        || status.as_deref() == Some("approved");
    let current_version_cid = v
        .get("current_version")
        .and_then(|cv| cv.get("cid"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(Document {
        id,
        path: v.get("path").and_then(Value::as_str).map(str::to_string),
        title: v.get("title").and_then(Value::as_str).map(str::to_string),
        status,
        has_approved_version: approved,
        body: v.get("body").and_then(Value::as_str).map(str::to_string),
        current_version_cid,
    })
}

/// Async reader for the board document/wiki REST surface (stateless — each call is one request), used by the
/// wiki-sync connector (task_1089). Carries `allow(dead_code)` until the worker (next) calls it, the same
/// staging the `wiki_sync` core and `Store::delete_points` use.
#[allow(dead_code)]
pub struct Documents {
    base: String,
    http: reqwest::Client,
}

#[allow(dead_code)]
impl Documents {
    /// Build a reader for the configured `board_url`.
    pub fn connect() -> Documents {
        Documents {
            base: crate::config::get()
                .board_url
                .trim_end_matches('/')
                .to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// List the wiki index, optionally restricted to a path `prefix` — `GET /wiki[?prefix=<p>]`. The reconcile
    /// pass lists the whole wiki (prefix `None`) and filters to approved docs via [`parse_document`].
    pub async fn list_wiki(&self, prefix: Option<&str>) -> Result<Vec<WikiEntry>, String> {
        let url = format!("{}/wiki", self.base);
        let mut req = self.http.get(&url);
        if let Some(p) = prefix {
            req = req.query(&[("prefix", p)]);
        }
        let v: Value = req
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("board GET /wiki failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("board GET /wiki: response was not JSON: {e}"))?;
        Ok(parse_wiki_index(&v))
    }

    /// Fetch one document — `GET /documents/{id}[?include_body=true]`. With `include_body`, the current
    /// version's markdown is inlined as `body` (fetched server-side from its pinned CID).
    pub async fn get_document(&self, id: i64, include_body: bool) -> Result<Document, String> {
        let mut url = format!("{}/documents/{}", self.base, id);
        if include_body {
            url.push_str("?include_body=true");
        }
        let v: Value = self
            .http
            .get(&url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("board GET /documents/{id} failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("board GET /documents/{id}: response was not JSON: {e}"))?;
        parse_document(&v).ok_or_else(|| format!("board GET /documents/{id}: no id in response"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wiki_index_reads_id_and_path_skips_idless() {
        let v = json!([
            {"id": 3354, "path": "charters/v-nix", "title": "x"},
            {"id": "3357", "path": "reference/membrain-metrics"}, // numeric string id
            {"path": "no-id-entry"},                               // skipped
        ]);
        let rows = parse_wiki_index(&v);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            WikiEntry {
                id: 3354,
                path: Some("charters/v-nix".into())
            }
        );
        assert_eq!(rows[1].id, 3357);
        assert_eq!(rows[1].path.as_deref(), Some("reference/membrain-metrics"));
    }

    #[test]
    fn wiki_index_non_array_is_empty() {
        assert!(parse_wiki_index(&json!({"error": "nope"})).is_empty());
    }

    #[test]
    fn document_approved_via_approved_version_id() {
        let v = json!({
            "id": 3354, "path": "charters/v-nix", "title": "Charter",
            "status": "in_review", "approved_version_id": 5172,
            "current_version": {"cid": "QmAbc"}
        });
        let d = parse_document(&v).unwrap();
        assert_eq!(d.id, 3354);
        assert_eq!(d.path.as_deref(), Some("charters/v-nix"));
        assert!(d.has_approved_version); // approved_version_id present
        assert_eq!(d.current_version_cid.as_deref(), Some("QmAbc"));
        assert!(d.body.is_none()); // no include_body
    }

    #[test]
    fn document_approved_via_status_when_no_version_field() {
        let v = json!({"id": 1, "status": "approved"});
        assert!(parse_document(&v).unwrap().has_approved_version);
    }

    #[test]
    fn document_draft_is_not_approved_and_reads_body() {
        let v = json!({
            "id": 3357, "path": "reference/membrain-metrics", "status": "draft",
            "approved_version_id": null, "body": "# Metrics\n...", "current_version": {"cid": "QmQr2"}
        });
        let d = parse_document(&v).unwrap();
        assert!(!d.has_approved_version); // draft, null approved_version_id
        assert_eq!(d.body.as_deref(), Some("# Metrics\n..."));
        assert_eq!(d.current_version_cid.as_deref(), Some("QmQr2"));
    }

    #[test]
    fn document_missing_id_is_none() {
        assert!(parse_document(&json!({"path": "x"})).is_none());
    }
}
