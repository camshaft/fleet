//! `store` — a thin async Qdrant client over its plain REST API. Port of Python `kb/store.py`.
//!
//! The Python used the `qdrant-client` package (gRPC/tonic). Here we talk to Qdrant's REST API directly with
//! async `reqwest` (operator directive #439: no blocking IO on the tokio runtime), which keeps the dep tree
//! light (no tonic/gRPC) and matches the workspace's async HTTP client. Every method maps one Python `store`
//! function to one REST endpoint; the request/response shapes below are Qdrant's documented ones.
//!
//! IDs are Qdrant point ids: our curated points use UUID strings (see `chunk::id`), but legacy collections
//! may hold integer ids, so a [`Candidate`]'s `id` is kept as a raw JSON value and only stringified for
//! display/citation — never reinterpreted.

use serde_json::{Map, Value, json};

/// One vector-search / scroll hit: the point id (raw, string-or-int), its vector score (0 for scroll), and
/// its payload. Mirrors the fields the Python code reads off a qdrant `ScoredPoint`/`Record`.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: Value,
    pub score: f64,
    pub payload: Map<String, Value>,
}

impl Candidate {
    /// The id as a string for display/citation — a bare number renders as its digits, a UUID as itself.
    pub fn id_str(&self) -> String {
        match &self.id {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }
}

/// A handle to a Qdrant instance over REST (stateless — each call is one request). IO is async `reqwest`
/// (operator directive: no blocking IO on the tokio runtime, #439).
pub struct Store {
    base: String,
    http: reqwest::Client,
}

impl Store {
    /// Build a client for the configured `qdrant_url` (the Python `client()` `@lru_cache`).
    pub fn connect() -> Result<Store, String> {
        Ok(Store {
            base: crate::config::get()
                .qdrant_url
                .trim_end_matches('/')
                .to_string(),
            http: reqwest::Client::new(),
        })
    }

    /// Whether a collection exists — `GET /collections/{name}` (200 → true, 404 → false). The Python
    /// `collection_exists`; guards the read paths so a missing collection is empty, not an error.
    pub async fn collection_exists(&self, name: &str) -> Result<bool, String> {
        let url = format!("{}/collections/{}", self.base, name);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("qdrant GET /collections/{name} failed: {e}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            Ok(false)
        } else if resp.status().is_success() {
            Ok(true)
        } else {
            Err(format!(
                "qdrant GET /collections/{name} failed: status {}",
                resp.status()
            ))
        }
    }

    /// Create the collection with the embedder's dim + Cosine distance if absent — the Python
    /// `ensure_collection`. `PUT /collections/{name}`.
    pub async fn ensure_collection(&self, name: &str, dim: usize) -> Result<(), String> {
        if self.collection_exists(name).await? {
            return Ok(());
        }
        let url = format!("{}/collections/{}", self.base, name);
        let body = json!({ "vectors": { "size": dim, "distance": "Cosine" } });
        self.http
            .put(&url)
            .json(&body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("qdrant create collection {name} failed: {e}"))?;
        Ok(())
    }

    /// Upsert points (id, vector, payload), waiting for the write to be applied — the Python `upsert`.
    /// `PUT /collections/{name}/points?wait=true`.
    pub async fn upsert(
        &self,
        name: &str,
        points: &[(String, Vec<f32>, Map<String, Value>)],
    ) -> Result<(), String> {
        if points.is_empty() {
            return Ok(());
        }
        let arr: Vec<Value> = points
            .iter()
            .map(|(id, vector, payload)| json!({ "id": id, "vector": vector, "payload": payload }))
            .collect();
        let url = format!("{}/collections/{}/points?wait=true", self.base, name);
        self.http
            .put(&url)
            .json(&json!({ "points": arr }))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("qdrant upsert into {name} failed: {e}"))?;
        Ok(())
    }

    /// Vector search; by default only `status == "active"` items (hides outdated/superseded) — the Python
    /// `query_candidates`. `POST /collections/{name}/points/query`. A missing collection yields no hits.
    pub async fn query_candidates(
        &self,
        name: &str,
        query_vector: &[f32],
        limit: usize,
        include_outdated: bool,
    ) -> Result<Vec<Candidate>, String> {
        if !self.collection_exists(name).await? {
            return Ok(vec![]);
        }
        let mut body = json!({
            "query": query_vector,
            "limit": limit,
            "with_payload": true,
        });
        if !include_outdated {
            body["filter"] =
                json!({ "must": [ { "key": "status", "match": { "value": "active" } } ] });
        }
        let url = format!("{}/collections/{}/points/query", self.base, name);
        let resp: Value = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("qdrant query {name} failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("qdrant query {name}: response was not JSON: {e}"))?;
        // Query API shape: { "result": { "points": [ { id, score, payload }, ... ] } }
        let points = resp
            .get("result")
            .and_then(|r| r.get("points"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(points.iter().map(scored_point).collect())
    }

    /// Retrieve one point by id (payload only) — the Python `get_point`. `POST /collections/{name}/points`.
    pub async fn get_point(&self, name: &str, id: &str) -> Result<Option<Candidate>, String> {
        if !self.collection_exists(name).await? {
            return Ok(None);
        }
        let url = format!("{}/collections/{}/points", self.base, name);
        let body = json!({ "ids": [id], "with_payload": true, "with_vector": false });
        let resp: Value = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("qdrant retrieve from {name} failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("qdrant retrieve from {name}: response was not JSON: {e}"))?;
        let first = resp
            .get("result")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .cloned();
        Ok(first.map(|p| scored_point(&p)))
    }

    /// Merge `patch` into a point's payload, waiting for the write — the Python `set_payload`.
    /// `POST /collections/{name}/points/payload?wait=true`.
    pub async fn set_payload(
        &self,
        name: &str,
        id: &str,
        patch: Map<String, Value>,
    ) -> Result<(), String> {
        let url = format!(
            "{}/collections/{}/points/payload?wait=true",
            self.base, name
        );
        let body = json!({ "payload": patch, "points": [id] });
        self.http
            .post(&url)
            .json(&body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("qdrant set_payload on {name} failed: {e}"))?;
        Ok(())
    }

    /// One document's chunk payloads within a page range, matched on exact `path` — the Python `read_pages`.
    /// `POST /collections/{name}/points/scroll`.
    pub async fn read_pages(
        &self,
        name: &str,
        path: &str,
        start_page: i64,
        end_page: i64,
        limit: usize,
    ) -> Result<Vec<Map<String, Value>>, String> {
        let filter = json!({ "must": [
            { "key": "path", "match": { "value": path } },
            { "key": "page", "range": { "gte": start_page, "lte": end_page } },
        ] });
        self.scroll(name, filter, limit).await
    }

    /// Every chunk payload in a page range across the whole collection — the Python `scroll_page_range`. A
    /// fallback for when the caller's `path` doesn't match exactly (a doc ingested under variant paths).
    pub async fn scroll_page_range(
        &self,
        name: &str,
        start_page: i64,
        end_page: i64,
        limit: usize,
    ) -> Result<Vec<Map<String, Value>>, String> {
        let filter = json!({ "must": [
            { "key": "page", "range": { "gte": start_page, "lte": end_page } },
        ] });
        self.scroll(name, filter, limit).await
    }

    /// Shared scroll helper: returns payloads only (the Python read paths only use `p.payload`). A missing
    /// collection yields an empty list.
    async fn scroll(
        &self,
        name: &str,
        filter: Value,
        limit: usize,
    ) -> Result<Vec<Map<String, Value>>, String> {
        if !self.collection_exists(name).await? {
            return Ok(vec![]);
        }
        let url = format!("{}/collections/{}/points/scroll", self.base, name);
        let body =
            json!({ "filter": filter, "with_payload": true, "with_vector": false, "limit": limit });
        let resp: Value = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("qdrant scroll {name} failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("qdrant scroll {name}: response was not JSON: {e}"))?;
        // Scroll shape: { "result": { "points": [ { id, payload }, ... ], "next_page_offset": ... } }
        let points = resp
            .get("result")
            .and_then(|r| r.get("points"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(points.iter().map(payload_of).collect())
    }

    /// Every collection name paired with its exact point count — the Python `collections`.
    /// `GET /collections` then `POST /collections/{name}/points/count { exact: true }`.
    pub async fn collections(&self) -> Result<Vec<(String, u64)>, String> {
        let url = format!("{}/collections", self.base);
        let resp: Value = self
            .http
            .get(&url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("qdrant GET /collections failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("qdrant GET /collections: response was not JSON: {e}"))?;
        let names: Vec<String> = resp
            .get("result")
            .and_then(|r| r.get("collections"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|c| c.get("name").and_then(Value::as_str).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let count = self.count(&name).await?;
            out.push((name, count));
        }
        Ok(out)
    }

    /// Exact point count for one collection — `POST /collections/{name}/points/count { exact: true }`.
    pub async fn count(&self, name: &str) -> Result<u64, String> {
        let url = format!("{}/collections/{}/points/count", self.base, name);
        let resp: Value = self
            .http
            .post(&url)
            .json(&json!({ "exact": true }))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("qdrant count {name} failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("qdrant count {name}: response was not JSON: {e}"))?;
        Ok(resp
            .get("result")
            .and_then(|r| r.get("count"))
            .and_then(Value::as_u64)
            .unwrap_or(0))
    }
}

/// Parse a Qdrant point object `{ id, score?, payload? }` into a [`Candidate`]. `score` defaults to 0
/// (scroll/retrieve responses carry no score).
fn scored_point(p: &Value) -> Candidate {
    Candidate {
        id: p.get("id").cloned().unwrap_or(Value::Null),
        score: p.get("score").and_then(Value::as_f64).unwrap_or(0.0),
        payload: payload_of(p),
    }
}

/// The `payload` object off a point, or an empty map (the Python `c.payload or {}`).
fn payload_of(p: &Value) -> Map<String, Value> {
    p.get("payload")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scored_point_reads_id_score_payload() {
        let p = json!({ "id": "abc", "score": 0.42, "payload": { "text": "hi", "kind": "doc" } });
        let c = scored_point(&p);
        assert_eq!(c.id_str(), "abc");
        assert!((c.score - 0.42).abs() < 1e-9);
        assert_eq!(c.payload.get("text").unwrap(), "hi");
    }

    #[test]
    fn scored_point_tolerates_missing_score_and_payload() {
        // A scroll/retrieve record: no score, maybe no payload → 0.0 and an empty map, never a panic.
        let p = json!({ "id": 7 });
        let c = scored_point(&p);
        assert_eq!(c.id_str(), "7"); // integer id renders as its digits
        assert_eq!(c.score, 0.0);
        assert!(c.payload.is_empty());
    }

    #[test]
    fn payload_of_defaults_empty() {
        assert!(payload_of(&json!({ "id": 1 })).is_empty());
        assert_eq!(
            payload_of(&json!({ "payload": { "a": 1 } }))
                .get("a")
                .unwrap(),
            1
        );
    }
}
