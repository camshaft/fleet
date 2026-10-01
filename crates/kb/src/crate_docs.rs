//! `crate_docs` — docs.rs rustdoc-JSON ingest. Port of the Python `kb/crate_docs.py` core `ingest_crate`.
//!
//! Fetches a crate's rustdoc JSON from docs.rs (`GET /crate/{name}/{version}/json`, a zstd-compressed file),
//! extracts every documented item as `(path, kind, docstring)`, and ingests each into a per-crate collection
//! `crate.<name>.<crate_version>`: the chunk text is `"{path} \u{2014} {kind}\n\n{docs}"` (a literal em-dash,
//! matching the Python), embedded with the shared bge-large embedder and upserted with the deterministic
//! point id `chunk::id(["docs.rs", name, crate_version, path, chunk_idx])`.
//!
//! Decision #2 on epic #232 (greenlit): the rustdoc parse is a SHARED module — the standalone `crate-docs`
//! board worker folds into the pipeline (#238) `source_type="docs.rs"` branch at parity, so this module
//! exposes the reusable core ([`parse_items`] + [`ingest_crate`]) plus a thin one-shot `kb crate-docs` CLI
//! (handy for manual ingest and for parity-testing the Rust path against the still-live Python worker before
//! the cutover). IO is async `reqwest`; the CPU work (zstd decompress is tiny; embedding is heavy) keeps the
//! embedder off the reactor via `spawn_blocking`, matching the inbox worker (#439).
//!
//! FIDELITY NOTES (flagged for parity verification against the Python source, which lives on the deployment host at
//! `~/Projects/camshaft/knowledge-base` and is not reachable from this host-a session):
//! - The resolved `crate_version` from the JSON (not the requested `version`, which may be "latest") names
//!   the collection AND is the `ver` component of the point id — so ingesting "latest" is idempotent with
//!   ingesting the explicit version it resolves to.
//! - The payload `url` is the rendered-docs root `https://docs.rs/<name>/<ver>/<name>/` — the form the live
//!   Python crate_docs.py stored (confirmed by the task_237 parity byte-match: ids + vectors + all
//!   content fields incl. url match the live crate.tokio.1.53.1) and the same form pipeline.rs uses.

// Ported ahead of its pipeline caller (#238); the CLI uses it now. Some helpers read as dead code until then.
#![allow(dead_code)]

use md5::{Digest, Md5};
use serde_json::{Map, Value};

use crate::store::Store;
use crate::{chunk, config, curate, embed};

/// The payload `source` and first component of the point id — the Python `KB_*` docs.rs source tag. Shared
/// with the pipeline's docs.rs branch (#238 reconciliation) so both paths key point ids identically.
pub(crate) const SOURCE: &str = "docs.rs";
/// The payload curation `kind` for crate docs (authority 0.8, static — no recency decay). Shared with the
/// pipeline docs.rs branch.
pub(crate) const KIND: &str = "doc";
/// Items' chunks embedded + upserted per batch, bounding peak memory and the Qdrant request size on large
/// crates (`store::upsert` sends one PUT for whatever it's given) — the Python `kb.crate_docs` batch of 128.
const BATCH: usize = 128;
/// zstd frame magic (little-endian `0xFD2FB528`). docs.rs serves the JSON as a zstd file body; detecting the
/// magic lets us decompress a raw body while passing through an already-decompressed one (e.g. if a proxy or
/// reqwest's own content-encoding handling expanded it), rather than blindly decoding.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// One documented item from the rustdoc index: its `::`-joined path, its item `kind` (struct/fn/trait/...),
/// and its docstring. Only items with a non-empty docstring AND a matching `paths` entry become a `DocItem`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocItem {
    pub path: String,
    pub kind: String,
    pub docs: String,
}

/// Fetch + parse a crate's rustdoc JSON from docs.rs, ingest every documented item into
/// `crate.<name>.<crate_version>`, and return `(chunk_count, collection)`. `version` may be `"latest"`; the
/// resolved `crate_version` from the JSON names the collection. A crate with no documented items yields
/// `(0, collection)` without creating anything.
///
/// `dry_run` computes everything (ids, chunk text, payloads, and the embedding vectors) but writes NOTHING
/// to Qdrant — no `Store` connection at all — and instead prints one NDJSON line per would-be point to
/// stdout (see [`dry_run_json`]). This is the crate-docs parity harness (#237/#466): because crate_docs
/// writes to the hardcoded `crate.<name>.<crate_version>` collection with no scratch override, a dry-run is
/// the only way to diff a fresh run against the LIVE (Python-built) collection without risking an in-place
/// overwrite of it.
pub async fn ingest_crate(
    name: &str,
    version: &str,
    dry_run: bool,
) -> Result<(usize, String), String> {
    let raw = fetch_rustdoc(name, version).await?;
    let doc: Value = serde_json::from_slice(&raw)
        .map_err(|e| format!("crate_docs: rustdoc JSON for {name}@{version} did not parse: {e}"))?;

    // The resolved version (authoritative — "latest" collapses onto it) names the collection and the id key.
    let crate_version = doc
        .get("crate_version")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!("crate_docs: rustdoc JSON for {name}@{version} has no crate_version")
        })?
        .to_string();
    let collection = format!("crate.{name}.{crate_version}");

    let items = parse_items(&doc);
    if items.is_empty() {
        tracing::info!(
            "crate_docs: {name}@{crate_version} has no documented items; nothing ingested"
        );
        return Ok((0, collection));
    }

    // Build every (id, text, payload) up front; the point id keys on the item path + chunk index, so item
    // iteration order does not affect ids (a re-ingest updates in place regardless of order).
    let cfg = config::get();
    let url = docs_url(name, &crate_version);
    let mut ids: Vec<String> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    let mut payloads: Vec<Map<String, Value>> = Vec::new();
    for item in &items {
        let body = item_body(item);
        for (idx, piece) in chunk::chunk_default(&body).into_iter().enumerate() {
            ids.push(point_id(name, &crate_version, &item.path, idx));
            let mut extra = Map::new();
            extra.insert("text".into(), Value::from(piece.clone()));
            extra.insert("source".into(), Value::from(SOURCE));
            extra.insert("path".into(), Value::from(item.path.clone()));
            extra.insert("title".into(), Value::from(item.path.clone()));
            extra.insert("url".into(), Value::from(url.clone()));
            extra.insert("crate".into(), Value::from(name));
            extra.insert("crate_version".into(), Value::from(crate_version.clone()));
            payloads.push(curate::base_payload(cfg, KIND, None, extra));
            texts.push(piece);
        }
    }
    if texts.is_empty() {
        return Ok((0, collection));
    }

    // Real run connects + sizes the collection; a dry-run touches no store at all.
    let store = if dry_run {
        None
    } else {
        let store = Store::connect()?;
        // Collection dimension: computed once off-reactor (model load is CPU-heavy), like the inbox worker.
        let dim = tokio::task::spawn_blocking(embed::dim)
            .await
            .map_err(|e| format!("crate_docs: embed dim task panicked: {e}"))??;
        store.ensure_collection(&collection, dim).await?;
        Some(store)
    };

    // Embed in BATCH-sized groups off-reactor. Real run upserts the aligned (id, vector, payload) points;
    // dry-run prints each as NDJSON instead (no write). Bounds memory + request size on big crates.
    let n = texts.len();
    let mut start = 0usize;
    while start < n {
        let end = (start + BATCH).min(n);
        let batch_texts = texts[start..end].to_vec();
        let vectors = tokio::task::spawn_blocking(move || embed::embed_docs(&batch_texts))
            .await
            .map_err(|e| format!("crate_docs: embed task panicked: {e}"))??;
        match &store {
            Some(store) => {
                let points: Vec<(String, Vec<f32>, Map<String, Value>)> = ids[start..end]
                    .iter()
                    .cloned()
                    .zip(vectors)
                    .zip(payloads[start..end].iter().cloned())
                    .map(|((id, vec), pl)| (id, vec, pl))
                    .collect();
                store.upsert(&collection, &points).await?;
            }
            None => {
                for ((id, vec), pl) in ids[start..end]
                    .iter()
                    .zip(&vectors)
                    .zip(payloads[start..end].iter())
                {
                    println!("{}", dry_run_json(&collection, id, vec, pl));
                }
            }
        }
        start = end;
    }

    if dry_run {
        tracing::info!(
            "crate_docs: DRY-RUN {name}@{crate_version}: {n} would-be points for {collection} (nothing written)"
        );
    } else {
        tracing::info!(
            "crate_docs: ingested {} chunks from {} items into {collection}",
            n,
            items.len()
        );
    }
    Ok((n, collection))
}

/// Build the NDJSON line for one would-be point in `--dry-run` — the id + full payload (which carries the
/// chunk text, path, kind, etc.: the byte-match gate for a parity diff against the live collection) plus a
/// vector fingerprint: `vector_md5` (md5 of the f32 little-endian bytes, so a diff can confirm the vectors
/// are byte-identical to the live ones) and `vector_head` (first 8 dims, for a quick eyeball). Pure so the
/// shape is unit-tested; the caller `println!`s it. Nothing is written to Qdrant.
fn dry_run_json(collection: &str, id: &str, vector: &[f32], payload: &Map<String, Value>) -> Value {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for f in vector {
        bytes.extend_from_slice(&f.to_le_bytes());
    }
    let vector_md5 = format!("{:x}", Md5::digest(&bytes));
    serde_json::json!({
        "id": id,
        "collection": collection,
        "vector_dim": vector.len(),
        "vector_md5": vector_md5,
        "vector_head": vector.iter().take(8).copied().collect::<Vec<f32>>(),
        "payload": payload,
    })
}

/// The payload citation `url` for a crate's items: the rendered-docs root `https://docs.rs/<name>/<ver>/<name>/`,
/// NOT the `/crate/<name>/<ver>` JSON-fetch form. This is the form the live Python crate_docs.py stored AND the
/// form pipeline.rs's rustdoc branch uses — deployment-side parity (task_237) confirmed all 599 live tokio points
/// carry it, so matching it makes crate-docs byte-identical to live (a re-ingest updates the url in place
/// rather than clobbering it) and aligns the two docs.rs paths for the #238 reconciliation. Pure; unit-tested.
pub(crate) fn docs_url(name: &str, crate_version: &str) -> String {
    format!("https://docs.rs/{name}/{crate_version}/{name}/")
}

/// The chunk body for one documented item: `"{path} \u{2014} {kind}\n\n{docs}"` (a literal em-dash), matching
/// the Python. Single-sourced so crate_docs and the pipeline docs.rs branch (#238) build identical bodies.
pub(crate) fn item_body(item: &DocItem) -> String {
    format!("{} \u{2014} {}\n\n{}", item.path, item.kind, item.docs)
}

/// The point-id parts BEFORE the chunk index for a docs.rs item: `[SOURCE, name, crate_version, path]`. The
/// pipeline's docs.rs branch stores these as the item's `id_override_parts` and appends the chunk index, so
/// both paths key ids identically. Single-sourced here.
pub(crate) fn item_id_parts(name: &str, crate_version: &str, path: &str) -> Vec<String> {
    vec![
        SOURCE.to_string(),
        name.to_string(),
        crate_version.to_string(),
        path.to_string(),
    ]
}

/// The deterministic point id for a docs.rs chunk: `chunk::id([SOURCE, name, crate_version, path, chunk_idx])`
/// — the parity-proven live formula (task_237). Single-sourced (via [`item_id_parts`]) so `ingest_crate` and
/// the pipeline docs.rs branch produce byte-identical ids, i.e. re-ingest via either path updates the same
/// live points in place.
pub(crate) fn point_id(name: &str, crate_version: &str, path: &str, idx: usize) -> String {
    let mut parts = item_id_parts(name, crate_version, path);
    parts.push(idx.to_string());
    chunk::id(&parts.iter().map(String::as_str).collect::<Vec<_>>())
}

/// Extract documented items from rustdoc JSON — the Python `_items`. Iterates `doc["index"]` (id -> item)
/// and keeps every entry with a NON-EMPTY `docs` string whose id also has a `doc["paths"]` entry, yielding
/// `("::".join(paths[id]["path"]), paths[id]["kind"], item["docs"])`. Missing `index`/`paths` (or a paths
/// entry without a `path` array) yields nothing rather than erroring. Pure — unit-tested.
pub fn parse_items(doc: &Value) -> Vec<DocItem> {
    let (Some(index), Some(paths)) = (
        doc.get("index").and_then(Value::as_object),
        doc.get("paths").and_then(Value::as_object),
    ) else {
        return vec![];
    };
    let mut out = Vec::new();
    for (id, item) in index {
        let docs = item.get("docs").and_then(Value::as_str).unwrap_or("");
        if docs.is_empty() {
            continue;
        }
        let Some(p) = paths.get(id) else { continue };
        let Some(segments) = p.get("path").and_then(Value::as_array) else {
            continue;
        };
        let path = segments
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("::");
        let kind = p
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        out.push(DocItem {
            path,
            kind,
            docs: docs.to_string(),
        });
    }
    out
}

/// GET the crate's rustdoc JSON from docs.rs and return the decompressed bytes. The endpoint serves a
/// zstd-compressed file; we decompress when the body carries the zstd magic and pass it through otherwise.
async fn fetch_rustdoc(name: &str, version: &str) -> Result<Vec<u8>, String> {
    let url = format!("https://docs.rs/crate/{name}/{version}/json");
    let bytes = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("crate_docs: GET {url} failed: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("crate_docs: read body from {url}: {e}"))?;
    Ok(maybe_unzstd(&bytes))
}

/// Decompress `bytes` if it is a zstd frame; otherwise return it unchanged. A zstd decode failure on
/// magic-tagged bytes falls back to the raw bytes (so a corrupt-but-tagged body still surfaces as a JSON
/// parse error upstream rather than being swallowed here).
pub(crate) fn maybe_unzstd(bytes: &[u8]) -> Vec<u8> {
    if bytes.len() >= 4 && bytes[..4] == ZSTD_MAGIC {
        match zstd::decode_all(bytes) {
            Ok(v) => v,
            Err(_) => bytes.to_vec(),
        }
    } else {
        bytes.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A minimal rustdoc-JSON shape matching docs.rs format_version 61: `index` maps id -> item (with `docs`),
    /// `paths` maps id -> `{ path: [...], kind }`.
    fn sample() -> Value {
        json!({
            "crate_version": "1.2.3",
            "index": {
                "10": { "docs": "The Chain iterator.", "name": "Chain" },
                "11": { "docs": "", "name": "Undocumented" },          // empty docs -> skipped
                "12": { "docs": "A helper fn.", "name": "helper" },
                "13": { "docs": "No paths entry.", "name": "Orphan" }    // not in paths -> skipped
            },
            "paths": {
                "10": { "path": ["anyhow", "Chain"], "kind": "struct" },
                "11": { "path": ["anyhow", "Undocumented"], "kind": "struct" },
                "12": { "path": ["anyhow", "sub", "helper"], "kind": "function" }
            }
        })
    }

    #[test]
    fn parse_items_keeps_only_documented_with_paths() {
        let mut items = parse_items(&sample());
        items.sort_by(|a, b| a.path.cmp(&b.path)); // iteration order is unspecified; sort for a stable assert
        assert_eq!(
            items,
            vec![
                DocItem {
                    path: "anyhow::Chain".into(),
                    kind: "struct".into(),
                    docs: "The Chain iterator.".into()
                },
                DocItem {
                    path: "anyhow::sub::helper".into(),
                    kind: "function".into(),
                    docs: "A helper fn.".into()
                },
            ]
        );
    }

    #[test]
    fn parse_items_missing_sections_is_empty_not_error() {
        assert!(parse_items(&json!({})).is_empty());
        assert!(parse_items(&json!({ "index": {} , "paths": {} })).is_empty());
    }

    #[test]
    fn body_text_uses_em_dash_and_blank_line() {
        // The exact chunk body the Python builds: "{path} — {kind}\n\n{docs}" with a U+2014 em-dash.
        let item = DocItem {
            path: "anyhow::Chain".into(),
            kind: "struct".into(),
            docs: "The Chain iterator.".into(),
        };
        let body = format!("{} \u{2014} {}\n\n{}", item.path, item.kind, item.docs);
        assert_eq!(body, "anyhow::Chain \u{2014} struct\n\nThe Chain iterator.");
        assert!(body.contains('\u{2014}')); // em-dash, not a hyphen-minus
    }

    #[test]
    fn point_id_keys_on_docs_rs_parts_and_is_stable() {
        // The id parts the ingest uses; stable across runs -> re-ingest updates in place.
        let a = chunk::id(&["docs.rs", "anyhow", "1.2.3", "anyhow::Chain", "0"]);
        let b = chunk::id(&["docs.rs", "anyhow", "1.2.3", "anyhow::Chain", "0"]);
        assert_eq!(a, b);
        // A different chunk index -> a different id.
        assert_ne!(
            a,
            chunk::id(&["docs.rs", "anyhow", "1.2.3", "anyhow::Chain", "1"])
        );
    }

    #[test]
    fn docs_url_is_the_rendered_docs_root_form() {
        // The LIVE form (matches Python crate_docs.py + pipeline.rs), proven byte-identical by the task_237
        // parity run. NOT the `/crate/<name>/<ver>` JSON-fetch form.
        assert_eq!(
            docs_url("tokio", "1.53.1"),
            "https://docs.rs/tokio/1.53.1/tokio/"
        );
        assert_eq!(
            docs_url("anyhow", "1.0.104"),
            "https://docs.rs/anyhow/1.0.104/anyhow/"
        );
    }

    #[test]
    fn maybe_unzstd_roundtrips_and_passes_through() {
        let plain = b"{\"crate_version\":\"1.0.0\"}";
        // A real zstd frame is decompressed back to the original.
        let compressed = zstd::encode_all(&plain[..], 0).unwrap();
        assert_eq!(compressed[..4], ZSTD_MAGIC);
        assert_eq!(maybe_unzstd(&compressed), plain);
        // Non-zstd bytes pass through untouched (already-decompressed JSON).
        assert_eq!(maybe_unzstd(plain), plain);
    }

    #[test]
    fn dry_run_json_carries_id_payload_and_vector_fingerprint() {
        let payload = serde_json::json!({ "text": "hi", "path": "a::b", "kind": "struct" })
            .as_object()
            .unwrap()
            .clone();
        let vector = vec![1.0f32, 2.0, 3.0];
        let j = dry_run_json("crate.foo.1.0.0", "the-id", &vector, &payload);
        assert_eq!(j["id"], "the-id");
        assert_eq!(j["collection"], "crate.foo.1.0.0");
        assert_eq!(j["vector_dim"], 3);
        assert_eq!(j["payload"]["text"], "hi");
        assert_eq!(j["vector_head"], serde_json::json!([1.0, 2.0, 3.0]));
        // vector_md5 is the md5 of the f32 little-endian bytes — stable + independently reproducible.
        let mut bytes = Vec::new();
        for f in &vector {
            bytes.extend_from_slice(&f.to_le_bytes());
        }
        assert_eq!(j["vector_md5"], format!("{:x}", Md5::digest(&bytes)));
    }
}
