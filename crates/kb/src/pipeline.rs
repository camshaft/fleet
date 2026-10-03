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
//! The correctness-critical routing/parse core — [`content_type_for`] (PDF detection) and [`collection_for`]
//! (where a ticket's points land, a wrong answer puts points in the wrong place) — is pinned to the Python
//! behavior by unit tests. The IO layer (fetch_source + the two stage handlers) and the reactive webhook
//! runtime ([`run_role`]) build on the existing infra (board.rs / webhook.rs / ipfs.rs / extract.rs). Run one
//! role per process: `kb pipeline --role uploader` / `kb pipeline --role embedder`.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::board::{self, Board, Task};
use crate::ipfs::Ipfs;
use crate::store::Store;
use crate::webhook::{self, BusySet};
use crate::{chunk, config, crate_docs, curate, embed, extract};

/// Content-type tag the uploader stamps on a ticket and the embedder dispatches on — the Python string
/// constants. `rustdoc-json` is set by the docs.rs fetch path; `pdf`/`text` come from [`content_type_for`].
pub const RUSTDOC_JSON: &str = "rustdoc-json";
pub const PDF: &str = "pdf";
pub const DOCX: &str = "docx";
pub const TEXT: &str = "text";
/// Source/id namespace for an internal crate's rustdoc docs (doc_101/task_828): rustdoc-json that did NOT
/// come from docs.rs, so it must not be tagged or cited as a docs.rs page.
const INTERNAL_CRATE: &str = "internal-crate";

/// An explicit `content_type` carried in ticket metadata, honored for `file`/`url` ingests so an externally
/// produced artifact (e.g. a rustdoc-json blob built outside the fleet — doc_101/task_828) reaches the
/// matching parse path without the uploader sniffing it. Only a known content type is accepted; anything else
/// (or its absence) returns `None`, so the caller falls back to sniffing. The docs.rs branch sets its own
/// content type and is unaffected.
fn content_type_override(meta: &Map<String, Value>) -> Option<String> {
    match truthy_str(meta, "content_type") {
        Some(ct) if ct == RUSTDOC_JSON || ct == PDF || ct == DOCX || ct == TEXT => {
            Some(ct.to_string())
        }
        _ => None,
    }
}

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
/// VRAM) overcommit. On the deployment host the embedder runs `embed_device="cpu"` (its GPU is an sm_61 card, which the
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

/// Detect PDFs (by extension, Content-Type header, or the `%PDF-` magic) and .docx (by extension or the
/// WordprocessingML Content-Type) — extends the Python `_content_type_for`. A PDF/docx fetched over a URL must
/// run document-aware text extraction, not be embedded as raw bytes. Everything else is `text`. Docx is NOT
/// sniffed by magic (its PK zip header is shared with every OOXML/zip format), so it is recognized only by the
/// `.docx` name or an explicit WordprocessingML header. (`rustdoc-json` is tagged by the docs.rs fetch path.)
pub fn content_type_for(name: &str, header: &str, data: &[u8]) -> &'static str {
    let lname = name.to_lowercase();
    let lheader = header.to_lowercase();
    if lname.ends_with(".pdf") || lheader.contains("application/pdf") || data.starts_with(b"%PDF-")
    {
        PDF
    } else if lname.ends_with(".docx") || lheader.contains("wordprocessingml.document") {
        DOCX
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

/// One unit to embed — the Python `_items_from` yield: `key` seeds the point id, `body` is chunked +
/// embedded, `extra` is merged into each chunk's payload.
///
/// `id_override_parts`: normally a chunk's point id is `chunk::id([collection, key, chunk_idx])`. A docs.rs
/// item instead sets this to the canonical crate_docs id parts `["docs.rs", name, ver, path]` (see the #238
/// reconciliation), so `chunk::id(parts + [chunk_idx])` reproduces the parity-proven live docs.rs ids exactly
/// — collection-independent, byte-identical to `kb crate-docs`. `None` for every other source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub key: String,
    pub body: String,
    pub extra: Map<String, Value>,
    pub id_override_parts: Option<Vec<String>>,
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
        DOCX => items_from_docx(data, meta),
        _ => Ok(items_from_text(data, meta)),
    }
}

/// rustdoc-JSON: DELEGATES to the shared `crate_docs` core (the #238 decision-2 reconciliation), so a docs.rs
/// source ingested via the pipeline is byte-identical to `kb crate-docs` and to the live Python-built
/// `crate.<name>.<ver>` collections (proven at parity, task_237): same item selection ([`crate_docs::parse_items`]:
/// non-empty docs AND a matching `paths` entry with a `path` array), same body ([`crate_docs::item_body`]),
/// same payload url ([`crate_docs::docs_url`], the rendered-docs root), and — crucially — the same point id via
/// [`Item::id_override_parts`] = [`crate_docs::item_id_parts`] `["docs.rs", name, ver, path]`, NOT the
/// pipeline's own `[collection, key, idx]` formula. `crate` is `meta.crate` else `meta.source`; `ver` is the
/// doc's `crate_version` else `meta.version` else `latest`.
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
    // docs.rs is the default so an existing docs.rs ingest keeps byte-identical ids/url/source (the live
    // parity guardrail). An internal crate (doc_101/task_828) reaches this path via a content-type override on
    // a file/url ingest (source_type != "docs.rs"): it has no docs.rs page, so the citation is a
    // producer-supplied `citation_url` or omitted, the source is tagged internal-crate, and its point ids live
    // in a distinct namespace rather than the docs.rs one.
    let is_docs_rs = str_or(meta, "source_type", "docs.rs") == "docs.rs";
    let (source, url) = if is_docs_rs {
        (
            crate_docs::SOURCE,
            Some(crate_docs::docs_url(&crate_name, &ver)),
        )
    } else {
        (
            INTERNAL_CRATE,
            truthy_str(meta, "citation_url").map(String::from),
        )
    };
    let out = crate_docs::parse_items(&doc)
        .into_iter()
        .map(|item| {
            let mut extra = Map::new();
            extra.insert("kind".into(), Value::from(crate_docs::KIND));
            extra.insert("source".into(), Value::from(source));
            extra.insert("path".into(), Value::from(item.path.clone()));
            extra.insert("title".into(), Value::from(item.path.clone()));
            if let Some(u) = &url {
                extra.insert("url".into(), Value::from(u.clone()));
            }
            extra.insert("crate".into(), Value::from(crate_name.clone()));
            extra.insert("crate_version".into(), Value::from(ver.clone()));
            let id_override_parts = if is_docs_rs {
                crate_docs::item_id_parts(&crate_name, &ver, &item.path)
            } else {
                vec![
                    INTERNAL_CRATE.to_string(),
                    crate_name.clone(),
                    ver.clone(),
                    item.path.clone(),
                ]
            };
            Item {
                body: crate_docs::item_body(&item),
                id_override_parts: Some(id_override_parts),
                key: item.path,
                extra,
            }
        })
        .collect();
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
            id_override_parts: None,
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
        id_override_parts: None,
    }]
}

/// docx (OOXML WordprocessingML): a single item, the document's extracted text — bytes-based (the embedder has
/// no file path) via [`extract::extract_docx_bytes`]. `key = title`, `kind = "doc"` (overridable). A non-docx
/// or corrupt byte stream surfaces the extractor's error.
fn items_from_docx(data: &[u8], meta: &Map<String, Value>) -> Result<Vec<Item>, String> {
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
    Ok(vec![Item {
        key: title,
        body: extract::extract_docx_bytes(data)?,
        extra,
        id_override_parts: None,
    }])
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
            let ct = content_type_override(meta)
                .unwrap_or_else(|| content_type_for(name, "", &data).to_string());
            let mut extra = Map::new();
            extra.insert("filename".into(), Value::from(name));
            Ok((data, ct, extra))
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
            let ct = content_type_override(meta)
                .unwrap_or_else(|| content_type_for(u, &header, &bytes).to_string());
            let mut extra = Map::new();
            extra.insert("url".into(), Value::from(u));
            extra.insert("filename".into(), Value::from(url_filename(u)));
            Ok((bytes.to_vec(), ct, extra))
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

/// The deterministic point id for one chunk. A docs.rs item carries `id_override_parts` = the canonical
/// crate_docs parts `["docs.rs", name, ver, path]`, so its id is `chunk::id(parts + [idx])` — byte-identical
/// to `kb crate-docs` and the live store (#238), collection-independent. Every other item keys on
/// `[collection, key, idx]` (the pipeline's own formula, for net-new pdf/text/url sources). Pure; unit-tested.
fn point_id(collection: &str, item: &Item, idx: usize) -> String {
    let idx = idx.to_string();
    match &item.id_override_parts {
        Some(parts) => {
            let mut refs: Vec<&str> = parts.iter().map(String::as_str).collect();
            refs.push(&idx);
            chunk::id(&refs)
        }
        None => chunk::id(&[collection, item.key.as_str(), &idx]),
    }
}

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

    // Parse + extract OFF the reactor: PDF parsing binds pdfium and may shell out to tesseract (a blocking
    // native call + subprocess), and rustdoc/text parse is CPU-bound — none of it may run on the async
    // runtime (fleet no-blocking-IO policy, task_809). The inbox worker wraps the equivalent the same way.
    let items = tokio::task::spawn_blocking(move || items_from(&content_type, &data, &meta))
        .await
        .map_err(|e| format!("pipeline: parse task panicked: {e}"))??;
    let n_items = items.len();

    // Build every (id, text, payload) up front; the point id keys on the item's id parts + chunk idx, so
    // item/chunk order does not affect ids (a re-ingest updates in place). A docs.rs item overrides the parts
    // to the canonical crate_docs formula (#238); everything else keys on [collection, key, idx].
    let cfg = config::get();
    let mut ids: Vec<String> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    let mut payloads: Vec<Map<String, Value>> = Vec::new();
    for item in &items {
        for (idx, piece) in chunk::chunk_default(&item.body).into_iter().enumerate() {
            ids.push(point_id(&collection, item, idx));
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

// ---- reactive agent runtime ----

/// The reserved webhook port for each role — the Python `KB_UPLOADER_PORT` / `KB_EMBEDDER_PORT` defaults. The
/// board POSTs task events to `http://<advertise_host>:<port>/`, where the advertised host is the routable
/// address the worker registers (see [`advertise_host`]) — loopback when co-resident with the board, else the
/// worker's board-routable IP.
const UPLOADER_PORT: u16 = 8075;
const EMBEDDER_PORT: u16 = 8074;

/// Reduce a board base URL to a `host:port` suitable for a UDP route-detect connect. Strips the scheme and
/// any path; defaults to `:80` when the URL carries no port. Pure, so it is unit-tested.
fn board_connect_target(board_url: &str) -> String {
    let after = board_url.split("://").nth(1).unwrap_or(board_url);
    let hostport = after
        .split('/')
        .next()
        .unwrap_or(after)
        .trim_end_matches('/');
    if hostport.contains(':') {
        hostport.to_string()
    } else {
        format!("{hostport}:80")
    }
}

/// Detect the local IP the OS would use to reach the board, via a connected UDP socket — `connect` only
/// selects the route, no packet is sent. Returns `None` if the board host can't be resolved or the chosen
/// address is loopback/unspecified (i.e. nothing routable to advertise).
fn detect_routable_ip(board_url: &str) -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect(board_connect_target(board_url)).ok()?;
    let ip = sock.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then(|| ip.to_string())
}

/// The host a worker advertises in its registered `webhook_url` so the board can POST events back. Uses the
/// configured `webhook_advertise_host` when set; otherwise auto-detects the IP routable toward the board, and
/// falls back to loopback (correct only for a co-resident board) with a warning when detection fails.
fn advertise_host(cfg: &config::Config) -> String {
    if !cfg.webhook_advertise_host.is_empty() {
        return cfg.webhook_advertise_host.clone();
    }
    detect_routable_ip(&cfg.board_url).unwrap_or_else(|| {
        tracing::warn!(
            "pipeline: no routable IP detected toward the board ({}); advertising a loopback webhook_url — a \
             non-co-resident board will not reach this worker. Set webhook_advertise_host to fix.",
            cfg.board_url
        );
        "127.0.0.1".to_string()
    })
}

/// Run one pipeline stage-agent reactively — the Python `run()`. Registers `role` (also the board agent id)
/// with its webhook, catches up on any `todo` task already assigned to it, then serves the webhook forever,
/// dispatching each actionable (deduped) task to [`process`]. `role` is `uploader` or `embedder`; anything
/// else is an error. The role names match the Python pipeline's assignees, so this is a drop-in swap: existing
/// producers keep filing to `uploader` and in-flight tickets keep their `uploader`/`embedder` assignees.
pub async fn run_role(role: &str) -> Result<(), String> {
    let port = match role {
        UPLOADER => UPLOADER_PORT,
        EMBEDDER => EMBEDDER_PORT,
        other => {
            return Err(format!(
                "pipeline: unknown role {other:?} (want uploader|embedder)"
            ));
        }
    };
    let board = Arc::new(board::connect(role));
    let ipfs = Arc::new(Ipfs::connect());
    let busy = BusySet::new();

    // A poll-only worker registers no `webhook_url` and relies on the periodic catch-up poll below for wake,
    // used when the board cannot reach this worker's advertised host — e.g. a board webhook-host guard that
    // rejects the worker's private/loopback LAN address. A reactive worker registers its advertised hook.
    let poll_only = config::get().poll_only;
    if poll_only && config::get().pipeline_poll_secs == 0 {
        return Err(format!(
            "pipeline {role}: poll_only is set with pipeline_poll_secs = 0, so the worker would register \
             no webhook_url and never poll; set pipeline_poll_secs > 0"
        ));
    }
    let hook = (!poll_only).then(|| format!("http://{}:{port}/", advertise_host(config::get())));
    board
        .register(
            hook.as_deref(),
            &serde_json::json!({ "kind": "worker", "display_name": role }),
        )
        .await?;

    // Startup catch-up: dispatch any todo task already assigned to me (e.g. filed while I was down).
    catch_up(&board, &ipfs, role, &busy).await;

    // Periodic catch-up backstop: re-poll on an interval so a MISSED webhook delivery still gets picked up —
    // e.g. when the board and this worker are not co-resident, the loopback `webhook_url` registered above is
    // unreachable from the board, so pure-reactive delivery silently drops. catch_up is claim-guarded, so a
    // poll never races the webhook into a double-dispatch. `pipeline_poll_secs == 0` disables it (pure reactive).
    let poll_secs = config::get().pipeline_poll_secs;
    if poll_secs > 0 {
        let board_poll = Arc::clone(&board);
        let ipfs_poll = Arc::clone(&ipfs);
        let busy_poll = Arc::clone(&busy);
        let role_poll = role.to_string();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(poll_secs));
            tick.tick().await; // consume the immediate first tick — the startup catch-up above already ran
            loop {
                tick.tick().await;
                catch_up(&board_poll, &ipfs_poll, &role_poll, &busy_poll).await;
            }
        });
        tracing::info!("pipeline {role}: catch-up poll every {poll_secs}s");
    }

    // A poll-only worker has no webhook to serve; the catch-up poll above is its sole wake path, so block
    // here to keep the worker and its spawned poll task alive.
    if poll_only {
        tracing::info!(
            "pipeline {role}: poll-only, no webhook_url registered; waking via the {poll_secs}s catch-up poll"
        );
        std::future::pending::<()>().await;
        return Ok(());
    }

    // Serve the webhook; the receiver parses + classifies + dedups and hands (task_id, guard) over the channel.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(i64, webhook::BusyGuard)>(64);
    let receiver = tokio::spawn(webhook::run_receiver(
        port,
        role.to_string(),
        Arc::clone(&busy),
        tx,
    ));
    tracing::info!(
        "pipeline {role}: reactive on {}",
        hook.as_deref().unwrap_or_default()
    );
    while let Some((id, guard)) = rx.recv().await {
        spawn_process(&board, &ipfs, role, id, guard);
    }
    // The channel only closes once the receiver ends (bind failure / shutdown); surface its result.
    receiver
        .await
        .map_err(|e| format!("pipeline {role}: receiver task panicked: {e}"))?
}

/// Dispatch every `todo` task currently assigned to `role`, claiming each so a racing webhook redelivery (or
/// an overlapping poll pass) does not double-dispatch it — the guard is released when processing ends. Shared
/// by the startup catch-up and the periodic poll backstop in [`run_role`].
async fn catch_up(board: &Arc<Board>, ipfs: &Arc<Ipfs>, role: &str, busy: &Arc<BusySet>) {
    match board.list_tasks(Some(role), Some("todo")).await {
        Ok(tasks) => {
            for t in tasks {
                if let Some(guard) = busy.claim(t.id) {
                    spawn_process(board, ipfs, role, t.id, guard);
                }
            }
        }
        Err(e) => tracing::warn!("pipeline {role}: catch-up list_tasks failed: {e}"),
    }
}

/// Spawn the processing of one claimed task, releasing its busy-claim (`guard`) when done — the Python
/// per-event `threading.Thread(target=_process, ...)`. The guard is held for the whole processing and dropped
/// at the end, so a task is never dispatched twice concurrently.
fn spawn_process(
    board: &Arc<Board>,
    ipfs: &Arc<Ipfs>,
    role: &str,
    task_id: i64,
    guard: webhook::BusyGuard,
) {
    let board = Arc::clone(board);
    let ipfs = Arc::clone(ipfs);
    let role = role.to_string();
    tokio::spawn(async move {
        process(&board, &ipfs, &role, task_id).await;
        drop(guard);
    });
}

/// Process one task id for `role` — the Python `_process` body (dedup is the caller's busy claim). Re-reads
/// the task and verifies it is still `todo` and assigned to this role (an event can race a reassignment or a
/// peer claim), marks it `in_progress`, runs the role's handler, and on error comments the failure + moves it
/// to `blocked`. A get_task / mark failure is logged and abandoned (a later redelivery retries).
async fn process(board: &Board, ipfs: &Ipfs, role: &str, task_id: i64) {
    let task = match board.get_task(task_id).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("pipeline {role}: get_task {task_id} failed: {e}");
            return;
        }
    };
    if task.assignee.as_deref() != Some(role) || task.status != "todo" {
        return; // not mine / already claimed or moved on
    }
    if let Err(e) = board.update_task(task_id, Some("in_progress"), None).await {
        tracing::warn!("pipeline {role}: mark {task_id} in_progress failed: {e}");
        return;
    }
    let result = match role {
        UPLOADER => handle_upload(board, ipfs, &task).await,
        EMBEDDER => handle_embed(board, ipfs, &task).await,
        other => Err(format!("unknown role {other}")),
    };
    if let Err(e) = result {
        let _ = board
            .comment_task(task_id, &format!("{role} failed: {e}"))
            .await;
        let _ = board.update_task(task_id, Some("blocked"), None).await;
        tracing::error!("pipeline {role}: task {task_id} FAILED: {e}");
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
    fn board_connect_target_strips_scheme_and_path_and_defaults_port() {
        assert_eq!(
            board_connect_target("http://192.0.2.10:8081/api"),
            "192.0.2.10:8081"
        );
        assert_eq!(
            board_connect_target("http://127.0.0.1:8079/api"),
            "127.0.0.1:8079"
        );
        assert_eq!(
            board_connect_target("https://board.lan/api"),
            "board.lan:80"
        ); // no port -> :80
        assert_eq!(board_connect_target("10.0.0.5:9000"), "10.0.0.5:9000"); // bare host:port
    }

    #[test]
    fn advertise_host_prefers_explicit_config_over_detection() {
        let cfg = config::Config {
            webhook_advertise_host: "198.51.100.7".to_string(),
            ..Default::default()
        };
        assert_eq!(advertise_host(&cfg), "198.51.100.7"); // explicit value used verbatim, no detection
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
        // docx by extension or the WordprocessingML header; NOT sniffed by the shared PK zip magic.
        assert_eq!(content_type_for("guide.docx", "", b"PK\x03\x04"), DOCX);
        assert_eq!(content_type_for("GUIDE.DOCX", "", b""), DOCX); // case-insensitive
        assert_eq!(
            content_type_for(
                "x",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                b""
            ),
            DOCX
        );
        assert_eq!(content_type_for("archive.zip", "", b"PK\x03\x04"), TEXT); // a plain zip is not docx
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
                "11": { "docs": "", "name": "Blank" },
                "12": { "docs": "No paths entry.", "name": "Orphan" }
            },
            "paths": {
                "10": { "path": ["anyhow", "Chain"], "kind": "struct" },
                "11": { "path": ["anyhow", "Blank"], "kind": "struct" }
            }
        }"#;
        let items = items_from(RUSTDOC_JSON, data, &m).unwrap();
        // Delegates to crate_docs::parse_items (#238): id 10 survives; 11 has empty docs (skipped), 12 has no
        // paths entry (skipped). Byte-identical selection to `kb crate-docs`.
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
        // docs.rs items carry the canonical crate_docs id parts, NOT the pipeline's [collection, key, idx].
        assert_eq!(
            it.id_override_parts,
            Some(vec![
                "docs.rs".to_string(),
                "anyhow".to_string(),
                "1.0.104".to_string(),
                "anyhow::Chain".to_string(),
            ])
        );
    }

    #[test]
    fn items_from_rustdoc_skips_entries_without_a_path_array() {
        // The shared crate_docs::parse_items requires a `paths` entry WITH a `path` array; an entry lacking it
        // is skipped -- no name/id fallback (that was the pipeline's old divergent variant, dropped in #238).
        let m = meta(json!({ "crate": "c", "version": "0.1.0" }));
        let data = br#"{
            "index": { "7": { "docs": "d", "name": "Widget" } },
            "paths": { "7": {} }
        }"#;
        let items = items_from(RUSTDOC_JSON, data, &m).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn content_type_override_honors_known_types_else_none() {
        // An explicit, known content_type on a file/url ingest is honored (so an externally produced
        // rustdoc-json artifact reaches the rustdoc parse path — task_828).
        assert_eq!(
            content_type_override(&meta(json!({ "content_type": "rustdoc-json" }))),
            Some(RUSTDOC_JSON.to_string())
        );
        assert_eq!(
            content_type_override(&meta(json!({ "content_type": "pdf" }))),
            Some(PDF.to_string())
        );
        assert_eq!(
            content_type_override(&meta(json!({ "content_type": "text" }))),
            Some(TEXT.to_string())
        );
        assert_eq!(
            content_type_override(&meta(json!({ "content_type": "docx" }))),
            Some(DOCX.to_string())
        );
        // Unknown or absent -> None, so the uploader falls back to sniffing.
        assert_eq!(
            content_type_override(&meta(json!({ "content_type": "bogus" }))),
            None
        );
        assert_eq!(content_type_override(&meta(json!({}))), None);
    }

    #[test]
    fn items_from_rustdoc_internal_crate_is_not_cited_as_docsrs() {
        // Same rustdoc JSON, but reached via a non-docs.rs source (an externally produced artifact, task_828):
        // no docs.rs url, source tagged internal-crate, and a point id in the internal-crate namespace. The
        // docs.rs path (default) stays byte-identical — see items_from_rustdoc_yields_... above.
        let data = br#"{
            "crate_version": "0.3.0",
            "index": { "10": { "docs": "An internal widget.", "name": "Widget" } },
            "paths": { "10": { "path": ["membrain_core", "Widget"], "kind": "struct" } }
        }"#;
        let m = meta(json!({ "source_type": "file", "crate": "membrain_core" }));
        let items = items_from(RUSTDOC_JSON, data, &m).unwrap();
        assert_eq!(items.len(), 1);
        let it = &items[0];
        assert_eq!(it.extra["source"], "internal-crate");
        assert_eq!(it.extra["crate"], "membrain_core");
        assert_eq!(it.extra["crate_version"], "0.3.0");
        assert!(!it.extra.contains_key("url")); // no docs.rs citation, and none supplied
        assert_eq!(
            it.id_override_parts,
            Some(vec![
                "internal-crate".to_string(),
                "membrain_core".to_string(),
                "0.3.0".to_string(),
                "membrain_core::Widget".to_string(),
            ])
        );
        // A producer-supplied internal citation_url is honored when present.
        let m2 = meta(json!({
            "source_type": "file",
            "crate": "membrain_core",
            "citation_url": "https://internal.example/membrain_core"
        }));
        let items2 = items_from(RUSTDOC_JSON, data, &m2).unwrap();
        assert_eq!(
            items2[0].extra["url"],
            "https://internal.example/membrain_core"
        );
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
    fn items_from_docx_yields_single_doc_item() {
        use std::io::{Cursor, Write};
        // Build a minimal .docx (ZIP + word/document.xml), Stored so no deflate feature is needed.
        let xml = "<?xml version=\"1.0\"?><w:document xmlns:w=\"x\"><w:body>\
            <w:p><w:r><w:t>Level 5 expectations</w:t></w:r></w:p></w:body></w:document>";
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zw.start_file("word/document.xml", opts).unwrap();
            zw.write_all(xml.as_bytes()).unwrap();
            let _ = zw.finish().unwrap();
        }
        let m = meta(json!({ "filename": "glg.docx", "source_type": "ipfs" }));
        let items = items_from(DOCX, &buf, &m).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key, "glg.docx");
        assert_eq!(items[0].body, "Level 5 expectations\n");
        assert_eq!(items[0].extra["kind"], "doc");
        assert_eq!(items[0].extra["title"], "glg.docx");
        // A corrupt docx surfaces an error rather than panicking.
        assert!(items_from(DOCX, b"not a docx", &m).is_err());
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
            id_override_parts: None,
        };
        let pl = embed_payload(
            config::get(),
            &item,
            "the chunk text",
            2,
            "bafkreicid",
            "http://host-b.lan:8080/ipfs/bafkreicid",
        );
        assert_eq!(pl["text"], "the chunk text");
        assert_eq!(pl["chunk"], 2);
        assert_eq!(pl["kind"], "doc");
        assert_eq!(pl["source"], "ipfs");
        assert_eq!(pl["path"], "Guide.pdf");
        assert_eq!(pl["page"], 3); // PDF page preserved
        assert_eq!(pl["ipfs_cid"], "bafkreicid");
        assert_eq!(pl["ipfs_url"], "http://host-b.lan:8080/ipfs/bafkreicid");
        // base_payload curation defaults are present (authority derived from kind, status active).
        assert_eq!(pl["status"], "active");
        assert!(pl.contains_key("authority"));
        assert!(pl.contains_key("created_at"));
    }

    #[test]
    fn point_id_uses_crate_docs_formula_for_docs_rs_and_collection_key_for_others() {
        // Non-docs.rs item (id_override_parts None): id = chunk::id([collection, key, idx]) — the pipeline's
        // own formula for net-new pdf/text/url sources.
        let text_item = Item {
            key: "readme.md".into(),
            body: "b".into(),
            extra: Map::new(),
            id_override_parts: None,
        };
        assert_eq!(
            point_id("docs.fleet", &text_item, 0),
            chunk::id(&["docs.fleet", "readme.md", "0"])
        );
        assert_ne!(
            point_id("docs.fleet", &text_item, 0),
            point_id("docs.fleet", &text_item, 1) // different chunk idx -> different id
        );

        // #238 reconciliation: a docs.rs item's id_override_parts drive the id to the canonical crate_docs
        // formula, byte-identical to crate_docs::point_id / `kb crate-docs` / the live store, and INDEPENDENT
        // of the collection arg.
        let docs_item = Item {
            key: "anyhow::Chain".into(),
            body: "b".into(),
            extra: Map::new(),
            id_override_parts: Some(crate_docs::item_id_parts(
                "anyhow",
                "1.0.104",
                "anyhow::Chain",
            )),
        };
        assert_eq!(
            point_id("crate.anyhow.1.0.104", &docs_item, 0),
            crate_docs::point_id("anyhow", "1.0.104", "anyhow::Chain", 0)
        );
        // Collection-independent: the same id no matter what collection is passed.
        assert_eq!(
            point_id("ignored.collection", &docs_item, 0),
            crate_docs::point_id("anyhow", "1.0.104", "anyhow::Chain", 0)
        );
    }
}
