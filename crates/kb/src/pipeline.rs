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

use std::path::Path;

use serde_json::{Map, Value};

use crate::board::{Board, Task};
use crate::ipfs::Ipfs;
use crate::store::Store;
use crate::{chunk, config, crate_docs, curate, embed, extract};

/// Content-type tag the uploader stamps on a ticket and the embedder dispatches on — the Python string
/// constants. `rustdoc-json` is set by the docs.rs fetch path; `pdf`/`text` come from [`content_type_for`].
pub const RUSTDOC_JSON: &str = "rustdoc-json";
pub const PDF: &str = "pdf";
pub const TEXT: &str = "text";

/// The two stage-agent roles (also the board agent ids) — the Python `UPLOADER`/`EMBEDDER`.
pub const UPLOADER: &str = "uploader";
pub const EMBEDDER: &str = "embedder";
/// User-Agent for outbound fetches — the Python `UA`.
const UA: &str = "camshaft-kb-pipeline/0.1";
/// Chunks embedded + upserted per batch — the Python `if len(txt) >= 128` flush in `handle_embed`. Bounds
/// peak memory and the Qdrant request size on a large source (`store::upsert` sends one PUT per batch).
const BATCH: usize = 128;

/// The embedder's work lock — the Python `_work_lock` (`with _work_lock, embed.gpu_lock()`). The embedder is
/// a single process, but its reactive webhook runtime dispatches one task per event concurrently, so this
/// serializes the embed+upsert critical section: one embed job at a time, no model/CPU (or, on a CUDA host,
/// VRAM) overcommit. On green the embedder runs `embed_device="cpu"` (the GTX 1080 Ti is sm_61, which the
/// bundled onnxruntime CUDA EP has no kernels for), so the Python cross-process `gpu_lock()` flock is a no-op
/// there; this in-process lock is the meaningful serialization for the single embedder agent.
static WORK_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

/// A ticket metadata string field with a default when the key is ABSENT (empty-but-present is kept) —
/// mirrors Python `meta.get(key, default)`, distinct from [`truthy_str`]'s `or`-chain semantics.
fn str_or<'a>(meta: &'a Map<String, Value>, key: &str, default: &'a str) -> &'a str {
    meta.get(key).and_then(Value::as_str).unwrap_or(default)
}

/// One unit to embed — the Python `_items_from` yield: `key` seeds the point id (`_id(collection, key,
/// chunk_idx)`), `body` is chunked + embedded, `extra` is merged into each chunk's payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub key: String,
    pub body: String,
    pub extra: Map<String, Value>,
}

/// Turn a ticket's fetched bytes into embeddable [`Item`]s, dispatching on content-type — the Python
/// `_items_from`. `rustdoc-json` walks the index/paths; `pdf` extracts per non-empty page; everything else is
/// one UTF-8 text unit. The `extra` map carries the payload fields the embedder merges (including `kind`,
/// which the caller lifts into `base_payload`).
pub fn items_from(
    content_type: &str,
    data: &[u8],
    meta: &Map<String, Value>,
) -> Result<Vec<Item>, String> {
    match content_type {
        RUSTDOC_JSON => items_from_rustdoc(data, meta),
        PDF => items_from_pdf(data, meta),
        _ => Ok(items_from_text(data, meta)),
    }
}

/// rustdoc-JSON: one item per documented index entry with a matching `paths` entry. `path` is the joined
/// `paths[id].path` (present-but-empty array -> ""), else `item.name` else the raw id; `kind` defaults to
/// `"item"` when absent. body = `"{path} \u{2014} {kind}\n\n{docs}"`. Faithful to pipeline.py (which differs
/// from crate_docs.rs's parse: different fallbacks + a different point-id formula — see decision #2).
fn items_from_rustdoc(data: &[u8], meta: &Map<String, Value>) -> Result<Vec<Item>, String> {
    let doc: Value = serde_json::from_slice(data)
        .map_err(|e| format!("pipeline: rustdoc JSON did not parse: {e}"))?;
    let crate_name = truthy_str(meta, "crate")
        .or_else(|| truthy_str(meta, "source"))
        .unwrap_or("None")
        .to_string();
    let ver = doc
        .get("crate_version")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| truthy_str(meta, "version"))
        .unwrap_or("latest")
        .to_string();
    let src_url = format!("https://docs.rs/{crate_name}/{ver}/{crate_name}/");
    let (Some(index), Some(paths)) = (
        doc.get("index").and_then(Value::as_object),
        doc.get("paths").and_then(Value::as_object),
    ) else {
        return Ok(vec![]);
    };
    let mut out = Vec::new();
    for (iid, item) in index {
        let docs = item.get("docs").and_then(Value::as_str).unwrap_or("");
        if docs.trim().is_empty() {
            continue;
        }
        let Some(p) = paths.get(iid) else { continue };
        // path = "::".join(p.path) when a "path" array is present (empty -> ""), else item.name, else id.
        let path = match p.get("path") {
            Some(Value::Array(segs)) => segs
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("::"),
            _ => item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(iid)
                .to_string(),
        };
        let kind = p.get("kind").and_then(Value::as_str).unwrap_or("item");
        let body = format!("{path} \u{2014} {kind}\n\n{docs}");
        let mut extra = Map::new();
        extra.insert("kind".into(), Value::from("doc"));
        extra.insert("source".into(), Value::from("docs.rs"));
        extra.insert("path".into(), Value::from(path.clone()));
        extra.insert("title".into(), Value::from(path.clone()));
        extra.insert("url".into(), Value::from(src_url.clone()));
        extra.insert("crate".into(), Value::from(crate_name.clone()));
        extra.insert("crate_version".into(), Value::from(ver.clone()));
        out.push(Item {
            key: path,
            body,
            extra,
        });
    }
    Ok(out)
}

/// PDF: one item per non-empty page (1-based), body = the page's trimmed text. `key = "p{n}"`, payload
/// carries `page`. Extraction is bytes-based (the embedder has no file path) + CRLF-normalized.
fn items_from_pdf(data: &[u8], meta: &Map<String, Value>) -> Result<Vec<Item>, String> {
    let title = truthy_str(meta, "filename")
        .or_else(|| truthy_str(meta, "source"))
        .unwrap_or("doc")
        .to_string();
    let source = str_or(meta, "source_type", "ipfs").to_string();
    let mut out = Vec::new();
    for (i, page) in extract::extract_pdf_bytes(data)?.into_iter().enumerate() {
        let t = page.trim();
        if t.is_empty() {
            continue;
        }
        let mut extra = Map::new();
        extra.insert("kind".into(), Value::from("doc"));
        extra.insert("source".into(), Value::from(source.clone()));
        extra.insert("path".into(), Value::from(title.clone()));
        extra.insert("title".into(), Value::from(title.clone()));
        extra.insert("page".into(), Value::from((i + 1) as i64));
        out.push(Item {
            key: format!("p{}", i + 1),
            body: t.to_string(),
            extra,
        });
    }
    Ok(out)
}

/// Plain text / markdown: a single item, the whole UTF-8 decoded body. `key = title`.
fn items_from_text(data: &[u8], meta: &Map<String, Value>) -> Vec<Item> {
    let title = truthy_str(meta, "filename")
        .or_else(|| truthy_str(meta, "source"))
        .unwrap_or("doc")
        .to_string();
    let mut extra = Map::new();
    extra.insert("kind".into(), Value::from(str_or(meta, "kind", "doc")));
    extra.insert(
        "source".into(),
        Value::from(str_or(meta, "source_type", "ipfs")),
    );
    extra.insert("path".into(), Value::from(title.clone()));
    extra.insert("title".into(), Value::from(title.clone()));
    vec![Item {
        key: title,
        body: String::from_utf8_lossy(data).into_owned(),
        extra,
    }]
}

// ---- uploader stage: source -> bytes -> IPFS ----

/// Fetch a ticket's source into `(bytes, content_type, extra_meta)` — the Python `fetch_source`. `docs.rs`
/// GETs the rustdoc JSON and zstd-decompresses it; `file`/`path` reads the file; `url` GETs (preferring a
/// `raw_url`) and sniffs the content-type. An unknown `source_type` is an error.
async fn fetch_source(
    meta: &Map<String, Value>,
) -> Result<(Vec<u8>, String, Map<String, Value>), String> {
    let st = str_or(meta, "source_type", "");
    let src = str_or(meta, "source", "");
    let http = reqwest::Client::new();
    match st {
        "docs.rs" => {
            let version = truthy_str(meta, "version").unwrap_or("latest");
            let url = format!("https://docs.rs/crate/{src}/{version}/json");
            let bytes = http
                .get(&url)
                .header(reqwest::header::USER_AGENT, UA)
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .map_err(|e| format!("pipeline: GET {url} failed: {e}"))?
                .bytes()
                .await
                .map_err(|e| format!("pipeline: read {url}: {e}"))?;
            let data = crate_docs::maybe_unzstd(&bytes);
            let mut extra = Map::new();
            extra.insert("crate".into(), Value::from(src));
            Ok((data, RUSTDOC_JSON.to_string(), extra))
        }
        "file" | "path" => {
            let data = tokio::fs::read(src)
                .await
                .map_err(|e| format!("pipeline: read file {src}: {e}"))?;
            let name = Path::new(src)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(src);
            let ct = content_type_for(name, "", &data);
            let mut extra = Map::new();
            extra.insert("filename".into(), Value::from(name));
            Ok((data, ct.to_string(), extra))
        }
        "url" => {
            let u = truthy_str(meta, "raw_url").unwrap_or(src);
            let resp = http
                .get(u)
                .header(reqwest::header::USER_AGENT, UA)
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .map_err(|e| format!("pipeline: GET {u} failed: {e}"))?;
            let header = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| format!("pipeline: read {u}: {e}"))?;
            let ct = content_type_for(u, &header, &bytes);
            let mut extra = Map::new();
            extra.insert("url".into(), Value::from(u));
            extra.insert("filename".into(), Value::from(url_filename(u)));
            Ok((bytes.to_vec(), ct.to_string(), extra))
        }
        other => Err(format!("pipeline: unknown source_type {other:?}")),
    }
}

/// Flatten a name into a safe single IPFS filename — the Python `ipfs_add_bytes` guard: replace `/` and `\`
/// with `_` (a slash makes Kubo build a DIRECTORY whose CID then 500s on `cat`), cap at 120 chars, and fall
/// back to `blob` if empty.
fn sanitize_ipfs_name(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .take(120)
        .collect();
    if replaced.is_empty() {
        "blob".to_string()
    } else {
        replaced
    }
}

/// The last path segment of a URL, or `doc` — the Python `u.rsplit("/", 1)[-1] or "doc"`.
fn url_filename(u: &str) -> String {
    match u.rsplit('/').next() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => "doc".to_string(),
    }
}

/// Uploader handler — the Python `handle_upload`: fetch the source, pin the bytes to IPFS, merge
/// `{ipfs_cid, content_type, ipfs_url, ...extra}` onto the ticket, and reassign it to the embedder. (The REST
/// `update_task` can't carry metadata like the Python MCP call, so props are merged via `set_task_props`
/// first, then the reassign — the embedder reads them off the task either way.)
pub async fn handle_upload(board: &Board, ipfs: &Ipfs, task: &Task) -> Result<(), String> {
    let meta = task.props();
    let (data, content_type, extra) = fetch_source(&meta).await?;
    let name = sanitize_ipfs_name(str_or(&meta, "source", "blob"));
    let cid = ipfs.add_bytes(&name, &data).await?.cid;
    let gateway = config::get().ipfs_gateway.trim_end_matches('/').to_string();

    let mut props = Map::new();
    props.insert("ipfs_cid".into(), Value::from(cid.clone()));
    props.insert("content_type".into(), Value::from(content_type.clone()));
    props.insert(
        "ipfs_url".into(),
        Value::from(format!("{gateway}/ipfs/{cid}")),
    );
    for (k, v) in extra {
        props.insert(k, v);
    }
    board.set_task_props(task.id, &Value::Object(props)).await?;
    board
        .update_task(task.id, Some("todo"), Some(EMBEDDER))
        .await?;
    let short: String = cid.chars().take(14).collect();
    board
        .comment_task(
            task.id,
            &format!("Pinned to IPFS ({short}..., {content_type}); handed to embedder."),
        )
        .await?;
    tracing::info!(
        "pipeline uploader: task {} pinned {short} -> embedder",
        task.id
    );
    Ok(())
}

// ---- embedder stage: IPFS -> parse -> chunk + embed -> upsert ----

/// Build the per-chunk payload — the Python `handle_embed` inner block: `base_payload(text=piece, chunk=idx,
/// **{extra without "page"})`, then re-add `page`, then stamp `ipfs_cid`/`ipfs_url`. The item's `kind` is
/// lifted out of `extra` into the `base_payload` `kind` argument (which sets `kind` + its `authority`); every
/// other `extra` field (source/path/title/url/crate/crate_version, and `page` for a PDF) is merged verbatim.
/// Pure, so it's unit-tested; the caller supplies the embedding vector separately.
fn embed_payload(
    cfg: &config::Config,
    item: &Item,
    piece: &str,
    idx: usize,
    cid: &str,
    ipfs_url: &str,
) -> Map<String, Value> {
    let kind = item
        .extra
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("doc");
    let mut extra = Map::new();
    extra.insert("text".into(), Value::from(piece));
    extra.insert("chunk".into(), Value::from(idx as i64));
    // Everything the item carried except `kind` (which becomes the base_payload argument). This includes
    // `page` for PDF items — Python excludes it from the base_payload call then re-adds it; the merged result
    // is identical, so we merge it here directly.
    for (k, v) in &item.extra {
        if k != "kind" {
            extra.insert(k.clone(), v.clone());
        }
    }
    extra.insert("ipfs_cid".into(), Value::from(cid));
    extra.insert("ipfs_url".into(), Value::from(ipfs_url));
    curate::base_payload(cfg, kind, None, extra)
}

/// Embedder handler — the Python `handle_embed`: cat the ticket's `ipfs_cid`, resolve its collection, parse it
/// into [`Item`]s by content-type, chunk + embed each into the collection, then mark the task done. The point
/// id is `chunk::id([collection, key, chunk_idx])` (the pipeline's own formula — NOTE it differs from
/// `crate_docs`'s `["docs.rs", name, ver, path, idx]`; decision #2 reconciles the two before either worker
/// retires). Embedding + upsert run under [`WORK_LOCK`] (one embed job at a time), with the model off the
/// reactor via `spawn_blocking`. Batches of [`BATCH`] bound memory + request size.
pub async fn handle_embed(board: &Board, ipfs: &Ipfs, task: &Task) -> Result<(), String> {
    let meta = task.props();
    let cid = truthy_str(&meta, "ipfs_cid")
        .ok_or_else(|| "pipeline: ticket has no ipfs_cid".to_string())?
        .to_string();
    let content_type = str_or(&meta, "content_type", TEXT).to_string();
    // cat back the exact bytes the uploader pinned (POST /api/v0/cat — the Python `ipfs_cat`; requires an
    // ipfs endpoint that allows the write-verb cat, i.e. Kubo-direct as the live Python worker uses).
    let data = ipfs.cat(&cid).await?;
    let collection = collection_for(&content_type, &data, &meta)?;
    let ipfs_url = truthy_str(&meta, "ipfs_url")
        .map(str::to_string)
        .unwrap_or_else(|| {
            let gateway = config::get().ipfs_gateway.trim_end_matches('/');
            format!("{gateway}/ipfs/{cid}")
        });

    let items = items_from(&content_type, &data, &meta)?;
    let n_items = items.len();

    // Build every (id, text, payload) up front; the point id keys on (collection, item key, chunk idx), so
    // item/chunk order does not affect ids (a re-ingest updates in place).
    let cfg = config::get();
    let mut ids: Vec<String> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    let mut payloads: Vec<Map<String, Value>> = Vec::new();
    for item in &items {
        for (idx, piece) in chunk::chunk_default(&item.body).into_iter().enumerate() {
            ids.push(chunk::id(&[
                collection.as_str(),
                item.key.as_str(),
                &idx.to_string(),
            ]));
            payloads.push(embed_payload(cfg, item, &piece, idx, &cid, &ipfs_url));
            texts.push(piece);
        }
    }
    let n_chunks = texts.len();

    // Serialize the embed+upsert critical section across concurrent webhook dispatches (single embedder).
    let _guard = WORK_LOCK.lock().await;
    let store = Store::connect()?;
    let dim = tokio::task::spawn_blocking(embed::dim)
        .await
        .map_err(|e| format!("pipeline: embed dim task panicked: {e}"))??;
    store.ensure_collection(&collection, dim).await?;

    let mut start = 0usize;
    while start < n_chunks {
        let end = (start + BATCH).min(n_chunks);
        let batch_texts = texts[start..end].to_vec();
        let vectors = tokio::task::spawn_blocking(move || embed::embed_docs(&batch_texts))
            .await
            .map_err(|e| format!("pipeline: embed task panicked: {e}"))??;
        let points: Vec<(String, Vec<f32>, Map<String, Value>)> = ids[start..end]
            .iter()
            .cloned()
            .zip(vectors)
            .zip(payloads[start..end].iter().cloned())
            .map(|((id, vec), pl)| (id, vec, pl))
            .collect();
        store.upsert(&collection, &points).await?;
        start = end;
    }
    drop(_guard);

    let props = serde_json::json!({
        "collection": collection,
        "items": n_items,
        "chunks": n_chunks,
    });
    board.set_task_props(task.id, &props).await?;
    board
        .comment_task(
            task.id,
            &format!("Embedded {n_items} items ({n_chunks} chunks) into {collection}."),
        )
        .await?;
    board.update_task(task.id, Some("done"), None).await?;
    tracing::info!(
        "pipeline embedder: task {} {n_chunks} chunks -> {collection}",
        task.id
    );
    Ok(())
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

    #[test]
    fn items_from_rustdoc_yields_documented_items_with_body_and_payload() {
        let m = meta(json!({ "source": "anyhow", "version": "1.0.104" }));
        let data = br#"{
            "crate_version": "1.0.104",
            "index": {
                "10": { "docs": "The Chain iterator.", "name": "Chain" },
                "11": { "docs": "   ", "name": "Blank" },
                "12": { "docs": "No paths entry.", "name": "Orphan" }
            },
            "paths": {
                "10": { "path": ["anyhow", "Chain"], "kind": "struct" },
                "11": { "path": ["anyhow", "Blank"], "kind": "struct" }
            }
        }"#;
        let items = items_from(RUSTDOC_JSON, data, &m).unwrap();
        // Only id 10 survives: 11 has whitespace-only docs, 12 has no paths entry.
        assert_eq!(items.len(), 1);
        let it = &items[0];
        assert_eq!(it.key, "anyhow::Chain");
        assert_eq!(
            it.body,
            "anyhow::Chain \u{2014} struct\n\nThe Chain iterator."
        );
        assert_eq!(it.extra["kind"], "doc");
        assert_eq!(it.extra["source"], "docs.rs");
        assert_eq!(it.extra["path"], "anyhow::Chain");
        assert_eq!(it.extra["crate"], "anyhow");
        assert_eq!(it.extra["crate_version"], "1.0.104");
        assert_eq!(it.extra["url"], "https://docs.rs/anyhow/1.0.104/anyhow/");
    }

    #[test]
    fn items_from_rustdoc_kind_defaults_to_item_and_path_falls_back_to_name() {
        // No "path" array on the paths entry, and no "kind": path falls back to item.name, kind -> "item".
        let m = meta(json!({ "crate": "c", "version": "0.1.0" }));
        let data = br#"{
            "index": { "7": { "docs": "d", "name": "Widget" } },
            "paths": { "7": {} }
        }"#;
        let items = items_from(RUSTDOC_JSON, data, &m).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key, "Widget");
        assert_eq!(items[0].body, "Widget \u{2014} item\n\nd");
    }

    #[test]
    fn items_from_text_is_single_utf8_unit() {
        let m = meta(json!({ "filename": "note.md", "source_type": "url", "kind": "manual" }));
        let items = items_from(TEXT, b"# hello\nworld", &m).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key, "note.md");
        assert_eq!(items[0].body, "# hello\nworld");
        assert_eq!(items[0].extra["kind"], "manual"); // meta.kind honored
        assert_eq!(items[0].extra["source"], "url");
        assert_eq!(items[0].extra["title"], "note.md");
    }

    #[test]
    fn sanitize_ipfs_name_flattens_slashes_caps_and_defaults() {
        assert_eq!(sanitize_ipfs_name("a/b\\c.pdf"), "a_b_c.pdf");
        assert_eq!(sanitize_ipfs_name(""), "blob");
        assert_eq!(sanitize_ipfs_name("plain.txt"), "plain.txt");
        // Capped at 120 chars.
        let long = "x".repeat(200);
        assert_eq!(sanitize_ipfs_name(&long).chars().count(), 120);
    }

    #[test]
    fn url_filename_takes_last_segment_or_doc() {
        assert_eq!(url_filename("https://h/a/b/readme.md"), "readme.md");
        assert_eq!(url_filename("https://h/a/b/"), "doc"); // trailing slash
        assert_eq!(url_filename("bare"), "bare");
    }

    #[test]
    fn embed_payload_lifts_kind_merges_extra_and_stamps_ipfs() {
        // A PDF-shaped item: `page` in extra must survive into the payload; `kind` becomes the base_payload
        // arg (not a duplicated extra), and ipfs_cid/ipfs_url are stamped on.
        let mut extra = Map::new();
        extra.insert("kind".into(), Value::from("doc"));
        extra.insert("source".into(), Value::from("ipfs"));
        extra.insert("path".into(), Value::from("Guide.pdf"));
        extra.insert("title".into(), Value::from("Guide.pdf"));
        extra.insert("page".into(), Value::from(3i64));
        let item = Item {
            key: "p3".into(),
            body: "unused here".into(),
            extra,
        };
        let pl = embed_payload(
            config::get(),
            &item,
            "the chunk text",
            2,
            "bafkreicid",
            "http://green-machine.lan:8080/ipfs/bafkreicid",
        );
        assert_eq!(pl["text"], "the chunk text");
        assert_eq!(pl["chunk"], 2);
        assert_eq!(pl["kind"], "doc");
        assert_eq!(pl["source"], "ipfs");
        assert_eq!(pl["path"], "Guide.pdf");
        assert_eq!(pl["page"], 3); // PDF page preserved
        assert_eq!(pl["ipfs_cid"], "bafkreicid");
        assert_eq!(
            pl["ipfs_url"],
            "http://green-machine.lan:8080/ipfs/bafkreicid"
        );
        // base_payload curation defaults are present (authority derived from kind, status active).
        assert_eq!(pl["status"], "active");
        assert!(pl.contains_key("authority"));
        assert!(pl.contains_key("created_at"));
    }

    #[test]
    fn pipeline_point_id_keys_on_collection_key_idx_and_differs_from_crate_docs() {
        // The pipeline's own id formula: chunk::id([collection, key, chunk_idx]) — stable across runs.
        let a = chunk::id(&["docs.fleet", "readme.md", "0"]);
        assert_eq!(a, chunk::id(&["docs.fleet", "readme.md", "0"]));
        assert_ne!(a, chunk::id(&["docs.fleet", "readme.md", "1"])); // different chunk idx
        // Decision #2: this is DISTINCT from crate_docs's formula ["docs.rs", name, ver, path, idx], so a
        // docs.rs source ingested via the pipeline vs `kb crate-docs` lands under different point ids until
        // the two are reconciled. This assertion pins that divergence so it can't drift silently.
        assert_ne!(
            chunk::id(&["crate.anyhow.1.0.104", "anyhow::Chain", "0"]),
            chunk::id(&["docs.rs", "anyhow", "1.0.104", "anyhow::Chain", "0"])
        );
    }
}
