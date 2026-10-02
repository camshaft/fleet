//! MCP server exposing the curated knowledge base to agents over streamable-HTTP.
//!
//! A faithful port of the Python `kb/server.py` tool surface. The tool NAMES, argument names/defaults, and
//! the plain-text output (esp. the `[id=… col=…]` head + `source:` citation of `kb_search`) are FROZEN — live
//! agents and the registered `kb-mcp` MCP call them and parse that text, so any drift breaks callers.
//!
//! Tools:
//!   kb_search        semantic search (rerank + curation blend); returns TEXT + citation + id
//!   kb_read_pages    read a doc's pages IN ORDER (walk a manual/section sequentially)
//!   kb_remember      store a durable memory
//!   kb_feedback      up/down-vote a result's usefulness (id comes from kb_search)
//!   kb_mark_outdated hide an item from results
//!   kb_supersede     replace an evolving fact with a new version
//!   kb_update        edit an item (text/status/quality/tags)
//!   kb_collections   list collections + counts
//!
//! Qdrant I/O is async (`store` over reqwest), awaited directly in each tool body. Embedding and reranking
//! (fastembed/ONNX) are CPU-bound, so those specific calls run off the reactor via `spawn_blocking` (the
//! [`blocking`] helper) — operator directive #439: no blocking on the tokio runtime.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ProtocolVersion, ServerCapabilities, ServerConfig,
};
use rmcp::{ErrorData as McpError, ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Map, Value};

use base64::Engine;

use crate::ipfs::Ipfs;
use crate::{config, curate, embed, search, store::Store};

/// The MCP handler. Stateless — every tool opens a fresh `Store` (a Qdrant REST call is sessionless and a
/// `reqwest::Client` is cheap to build), and the embedder/reranker are process-global singletons in their modules.
#[derive(Clone)]
pub struct Kb {
    #[allow(dead_code)]
    tool_router: ToolRouter<Kb>,
}

fn text_result(s: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::text(s)]))
}

fn map_err(e: String) -> McpError {
    McpError::internal_error(e, None)
}

/// Run a CPU-bound blocking closure (embed/rerank) off the reactor, flattening the join error.
async fn blocking<F, T>(f: F) -> Result<T, McpError>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| McpError::internal_error(format!("task panicked: {e}"), None))?
        .map_err(map_err)
}

// --- string/payload helpers (ports of the Python inline formatting) ---

fn ps(p: &Map<String, Value>, key: &str) -> Option<String> {
    p.get(key).and_then(Value::as_str).map(str::to_string)
}

fn ptext(p: &Map<String, Value>, key: &str) -> String {
    p.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// `page` as an integer if present and non-zero (Python treats 0/absent as "no page").
fn ppage(p: &Map<String, Value>) -> Option<i64> {
    match p.get("page") {
        Some(Value::Number(n)) => n.as_i64().filter(|&v| v != 0),
        _ => None,
    }
}

/// Build a citation string — the Python `_cite`.
fn cite(p: &Map<String, Value>) -> String {
    let title = ps(p, "title")
        .or_else(|| ps(p, "path"))
        .unwrap_or_else(|| "?".to_string());
    let mut loc = ps(p, "path").unwrap_or_else(|| "?".to_string());
    if let Some(page) = ppage(p) {
        loc.push_str(&format!(" p.{page}"));
    }
    let mut src = format!("{title} — {loc}");
    if let Some(mut url) = ps(p, "ipfs_url") {
        if let Some(page) = ppage(p) {
            url.push_str(&format!("#page={page}"));
        }
        src.push_str(&format!("\nipfs: {url}"));
    } else if let Some(abs) = ps(p, "abs_path") {
        src.push_str(&format!("\nfile: {abs}"));
    }
    src
}

/// Normalize a submitted confidentiality class to the fail-closed set: anything that is not exactly "public"
/// (case-insensitive) is treated as "internal". Internal content must never reach a public IPFS gateway or
/// public web (operator boundary, task_1185); it is fine on the board (internal CAS resolve) and on
/// enterprise Slack (byte upload).
fn normalize_image_class(s: &str) -> &'static str {
    if s.trim().eq_ignore_ascii_case("public") {
        "public"
    } else {
        "internal"
    }
}

/// The inline board-comment embed for a CAS image: a CID-only Markdown image. The board renderer and the
/// Slack bridge each resolve the CID internally, so the comment never carries a hard-coded gateway URL
/// (operator-preferred, task_1185 comment_5737). `alt` is sanitized of `]`/newline so it can't break the
/// Markdown image syntax.
fn image_embed(alt: &str, cid: &str) -> String {
    let safe_alt: String = alt
        .chars()
        .map(|c| match c {
            ']' | '\r' | '\n' => ' ',
            other => other,
        })
        .collect();
    format!("![{}](ipfs://{cid})", safe_alt.trim())
}

// --- argument structs (schemars-derived; defaults mirror the Python signatures) ---

fn default_search_collection() -> String {
    "all".to_string()
}
fn default_limit() -> i64 {
    5
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    pub query: String,
    #[serde(default = "default_search_collection")]
    pub collection: String,
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub include_outdated: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadPagesArgs {
    pub collection: String,
    pub path: String,
    pub start_page: i64,
    #[serde(default)]
    pub end_page: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RememberArgs {
    pub text: String,
    /// Defaults to the configured memory collection when omitted.
    #[serde(default)]
    pub collection: Option<String>,
    #[serde(default)]
    pub tags: Option<String>,
    #[serde(default)]
    pub authority: Option<f64>,
    #[serde(default)]
    pub confidence: Option<f64>,
    /// Curation kind (default "memory"). Drives the default authority and recency: "memory" DECAYS over time,
    /// while any non-"memory" kind is treated as a static reference (recency 1.0, no decay). Use "tenet" for
    /// durable operator law (authority 1.0, non-decaying) — see the task_538 tenets store.
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PromoteArgs {
    /// The board-canonical source path of the memory being promoted — e.g. `agents/<agent>/<slug>` or
    /// `repos/<repo>/<slug>`. This is the projection identity: the deterministic point id is keyed on it, so a
    /// re-promote of the same source UPDATES in place (1:1 with the board canonical, never a drifting copy).
    pub source_path: String,
    pub text: String,
    /// Optional board document reference for the canonical source (e.g. "doc_123"), recorded for traceability.
    #[serde(default)]
    pub source_doc: Option<String>,
    /// Curation kind (default "promoted": non-decaying, authority 0.9). Pass "tenet" for operator
    /// directives/tenets (authority 1.0). "memory" is rejected — promoted memories are durable invariants that
    /// must not age out of recall.
    #[serde(default)]
    pub kind: Option<String>,
    /// Optional display title.
    #[serde(default)]
    pub title: Option<String>,
    /// Optional tags.
    #[serde(default)]
    pub tags: Option<String>,
    /// Override the promoted-memory collection (defaults to the configured promoted collection).
    #[serde(default)]
    pub collection: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FeedbackArgs {
    pub id: String,
    pub helpful: bool,
    /// Defaults to the configured default search collection when omitted.
    #[serde(default)]
    pub collection: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MarkOutdatedArgs {
    pub id: String,
    /// Defaults to the configured memory collection when omitted (like kb_remember / kb_update /
    /// kb_supersede). Pass the `col` from a kb_search hit to mark an item in another collection outdated.
    #[serde(default)]
    pub collection: Option<String>,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SupersedeArgs {
    pub old_id: String,
    pub new_text: String,
    #[serde(default)]
    pub collection: Option<String>,
    #[serde(default)]
    pub tags: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateArgs {
    pub id: String,
    #[serde(default)]
    pub collection: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub quality: Option<f64>,
    #[serde(default)]
    pub tags: Option<String>,
}

/// Merge an optional string field into a payload extras map only when present — the Python idiom of passing
/// `tags=tags` and having `base_payload` drop `None` values (`if v is not None`).
fn put_opt_str(m: &mut Map<String, Value>, key: &str, v: Option<String>) {
    if let Some(v) = v {
        m.insert(key.to_string(), Value::from(v));
    }
}
fn put_opt_f64(m: &mut Map<String, Value>, key: &str, v: Option<f64>) {
    if let Some(v) = v {
        m.insert(key.to_string(), Value::from(v));
    }
}

fn default_image_class() -> String {
    "internal".to_string()
}

/// Args for `cas_add_image` — submit a generated image to the fleet CAS for inline board/Slack embedding.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CasAddImageArgs {
    /// Base64-encoded image bytes (the generated graph/table, e.g. a PNG or SVG).
    pub data_base64: String,
    /// A filename for the image (e.g. "throughput.png"); used only as the CAS object name.
    pub filename: String,
    /// Confidentiality class: "internal" (default, fail-closed) or "public". Internal content stays on the
    /// board + enterprise Slack and never reaches a public IPFS gateway / public web.
    #[serde(default = "default_image_class")]
    pub class: String,
    /// Optional alt text for the embed; defaults to the filename.
    #[serde(default)]
    pub alt: Option<String>,
}

#[tool_router]
impl Kb {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Semantic search over the knowledge base. By default searches ALL collections (manuals, this shop's printers/build, and stored memories) in one shot — you do not need to know which collection holds a fact. Pass a specific collection name only to narrow. Returns each matching passage's TEXT (self-contained) plus a citation and its `id`/`col` (pass both to kb_feedback / kb_mark_outdated)."
    )]
    async fn kb_search(
        &self,
        Parameters(a): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = a.limit.max(0) as usize;
        let store = Store::connect().map_err(map_err)?;
        let results = if a.collection == "all" {
            search::search_all(&store, &a.query, limit, a.include_outdated, None).await
        } else {
            search::search(
                &store,
                &a.collection,
                &a.query,
                limit,
                a.include_outdated,
                None,
            )
            .await
        }
        .map_err(map_err)?;
        if results.is_empty() {
            return text_result(format!("No results in '{}'.", a.collection));
        }
        let blocks: Vec<String> = results
            .iter()
            .map(|r| {
                let p = &r.payload;
                let head = format!(
                    "[id={} col={} score={:.3} kind={} status={}]",
                    r.id,
                    r.collection,
                    r.final_score,
                    ptext(p, "kind"),
                    ptext(p, "status"),
                );
                format!("{head}\nsource: {}\n\n{}", cite(p), ptext(p, "text").trim())
            })
            .collect();
        text_result(blocks.join("\n\n---\n\n"))
    }

    #[tool(
        description = "Read a paginated document's pages IN ORDER — for walking through any document sequentially (e.g. \"what's the next step?\", reading a section start to finish). kb_search finds the single best-matching passage but not what comes after it; use this to read forward. Workflow: kb_search to locate the right page, then pass that result's `col` as `collection` and its source `path` + page number here. Pages are inclusive; `end_page` defaults to a short window after `start_page`."
    )]
    async fn kb_read_pages(
        &self,
        Parameters(a): Parameters<ReadPagesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let store = Store::connect().map_err(map_err)?;
        let start = a.start_page;
        // end_page defaults to start+4, then is clamped to [start, start+24] (bound the span).
        let mut end = a.end_page.unwrap_or(start + 4);
        end = end.min(start + 24).max(start);

        let mut rows = store
            .read_pages(&a.collection, &a.path, start, end, 1000)
            .await
            .map_err(map_err)?;
        if rows.is_empty() {
            // Tolerate path variants (same doc ingested under different paths).
            let want = a.path.to_lowercase();
            if !want.is_empty() {
                rows = store
                    .scroll_page_range(&a.collection, start, end, 2000)
                    .await
                    .map_err(map_err)?
                    .into_iter()
                    .filter(|p| {
                        let path = ptext(p, "path").to_lowercase();
                        let title = ptext(p, "title").to_lowercase();
                        path.ends_with(&want) || want.ends_with(&path) || title == want
                    })
                    .collect();
            }
        }
        if rows.is_empty() {
            return text_result(format!(
                "No pages {start}-{end} found for '{}' in '{}'. Run kb_search first and pass the exact \
                 `col` and source path it cites.",
                a.path, a.collection
            ));
        }
        // Order by (page, chunk), both defaulting to 0.
        rows.sort_by_key(|p| (pnum(p, "page"), pnum(p, "chunk")));

        let mut out = String::new();
        let mut cur: Option<i64> = None;
        for p in &rows {
            let page = pnum(p, "page");
            if Some(page) != cur {
                cur = Some(page);
                out.push_str(&format!("\n--- p.{page} ---\n"));
            }
            out.push_str(ptext(p, "text").trim());
            out.push('\n');
        }
        let first = &rows[0];
        let last = &rows[rows.len() - 1];
        let title = ps(first, "title")
            .or_else(|| ps(first, "path"))
            .unwrap_or_else(|| a.path.clone());
        let lo = pnum(first, "page");
        let hi = pnum(last, "page");
        text_result(format!("{title} — pages {lo}–{hi}:\n{}", out.trim_end()))
    }

    #[tool(
        description = "Store a durable fact/note any future agent can retrieve. Use for knowledge worth keeping across sessions. Optional `kind` (default \"memory\", which decays); pass a non-memory kind like \"tenet\" for a non-decaying reference (operator tenets: kind=\"tenet\" gives authority 1.0 + no decay)."
    )]
    async fn kb_remember(
        &self,
        Parameters(a): Parameters<RememberArgs>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = config::get();
        let collection = a
            .collection
            .unwrap_or_else(|| cfg.memory_collection.clone());
        let store = Store::connect().map_err(map_err)?;
        let dim = blocking(embed::dim).await?;
        store
            .ensure_collection(&collection, dim)
            .await
            .map_err(map_err)?;
        let text = a.text;
        let (text, vec) = blocking(move || {
            let vec = embed::embed_docs(std::slice::from_ref(&text))?
                .pop()
                .ok_or("embed produced no vector")?;
            Ok((text, vec))
        })
        .await?;
        let pid = uuid::Uuid::new_v4().to_string();
        let kind = a.kind.as_deref().unwrap_or("memory");
        let mut extra = Map::new();
        extra.insert("text".into(), Value::from(text));
        extra.insert("source".into(), Value::from("memory"));
        put_opt_str(&mut extra, "tags", a.tags);
        put_opt_f64(&mut extra, "confidence", a.confidence);
        let payload = curate::base_payload(cfg, kind, a.authority, extra);
        store
            .upsert(&collection, &[(pid.clone(), vec, payload)])
            .await
            .map_err(map_err)?;
        text_result(format!("Stored memory {pid} in '{collection}'."))
    }

    #[tool(
        description = "Promote a durable, broadly-useful agent memory into the shared KB so other agents can recall it. The point is a traceable PROJECTION of its board-canonical source: pass `source_path` (agents/<agent>/<slug> or repos/<repo>/<slug>) and the deterministic id is keyed on it, so re-promoting the same source updates in place. Writes to the dedicated promoted-memory collection with a NON-decaying kind (default \"promoted\"; pass \"tenet\" for operator directives/tenets). Promote only durable, cross-agent, verified-correct, pure-text memories."
    )]
    async fn kb_promote(
        &self,
        Parameters(a): Parameters<PromoteArgs>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = config::get();
        let collection = a
            .collection
            .unwrap_or_else(|| cfg.promoted_collection.clone());
        let kind = a.kind.as_deref().unwrap_or("promoted");
        // Promoted memories are durable invariants; kind="memory" DECAYS, so it is not valid for a promotion.
        if kind == "memory" {
            return Err(map_err(
                "kb_promote: kind \"memory\" decays over time; promote with a non-decaying kind such as \"promoted\" or \"tenet\"".to_string(),
            ));
        }
        let store = Store::connect().map_err(map_err)?;
        let dim = blocking(embed::dim).await?;
        store
            .ensure_collection(&collection, dim)
            .await
            .map_err(map_err)?;
        let text = a.text;
        let (text, vec) = blocking(move || {
            let vec = embed::embed_docs(std::slice::from_ref(&text))?
                .pop()
                .ok_or("embed produced no vector")?;
            Ok((text, vec))
        })
        .await?;
        // Deterministic id keyed on the board-canonical source path, so a re-promote of the same source
        // updates in place rather than duplicating (byte-identical id — the projection stays 1:1 with source).
        let pid = crate::chunk::id(&["promoted", &a.source_path]);
        // Citation points back at the board canonical (the doc ref if given, else the source path).
        let citation = a
            .source_doc
            .clone()
            .unwrap_or_else(|| a.source_path.clone());
        let mut extra = Map::new();
        extra.insert("text".into(), Value::from(text));
        extra.insert("source".into(), Value::from("promoted"));
        // Source linkage: record the board canonical so the promoted point is a traceable projection.
        extra.insert("source_path".into(), Value::from(a.source_path));
        put_opt_str(&mut extra, "source_doc", a.source_doc);
        extra.insert("url".into(), Value::from(citation));
        put_opt_str(&mut extra, "title", a.title);
        put_opt_str(&mut extra, "tags", a.tags);
        let payload = curate::base_payload(cfg, kind, None, extra);
        store
            .upsert(&collection, &[(pid.clone(), vec, payload)])
            .await
            .map_err(map_err)?;
        text_result(format!(
            "Promoted memory {pid} into '{collection}' as kind '{kind}'."
        ))
    }

    #[tool(
        description = "Record whether a result (by its id from kb_search) was actually helpful. Nudges ranking so proven items surface higher."
    )]
    async fn kb_feedback(
        &self,
        Parameters(a): Parameters<FeedbackArgs>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = config::get();
        let collection = a
            .collection
            .unwrap_or_else(|| cfg.default_collection.clone());
        let store = Store::connect().map_err(map_err)?;
        let Some(pt) = store.get_point(&collection, &a.id).await.map_err(map_err)? else {
            return text_result(format!("No item {} in '{collection}'.", a.id));
        };
        let p = &pt.payload;
        let key = if a.helpful { "helpful" } else { "unhelpful" };
        let now = curate::now_iso();
        let mut patch = Map::new();
        patch.insert(key.into(), Value::from(pnum(p, key) + 1));
        patch.insert("use_count".into(), Value::from(pnum(p, "use_count") + 1));
        patch.insert("last_verified".into(), Value::from(now.clone()));
        patch.insert("updated_at".into(), Value::from(now));
        store
            .set_payload(&collection, &a.id, patch)
            .await
            .map_err(map_err)?;
        text_result(format!(
            "Recorded {} for {}.",
            if a.helpful { "helpful" } else { "not-helpful" },
            a.id
        ))
    }

    #[tool(description = "Hide an item from future results (marks it 'outdated').")]
    async fn kb_mark_outdated(
        &self,
        Parameters(a): Parameters<MarkOutdatedArgs>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = config::get();
        // Default to the memory collection, consistent with kb_remember/kb_update/kb_supersede (this is a
        // memory-lifecycle op; a bare id is usually a remembered fact). A search-result mark passes `col`.
        let collection = a
            .collection
            .unwrap_or_else(|| cfg.memory_collection.clone());
        let store = Store::connect().map_err(map_err)?;
        if store
            .get_point(&collection, &a.id)
            .await
            .map_err(map_err)?
            .is_none()
        {
            return text_result(format!("No item {} in '{collection}'.", a.id));
        }
        let mut patch = Map::new();
        patch.insert("status".into(), Value::from("outdated"));
        patch.insert("updated_at".into(), Value::from(curate::now_iso()));
        if !a.reason.is_empty() {
            patch.insert("outdated_reason".into(), Value::from(a.reason));
        }
        store
            .set_payload(&collection, &a.id, patch)
            .await
            .map_err(map_err)?;
        text_result(format!("Marked {} outdated.", a.id))
    }

    #[tool(
        description = "Replace an evolving fact: mark old_id superseded and store new_text as active."
    )]
    async fn kb_supersede(
        &self,
        Parameters(a): Parameters<SupersedeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = config::get();
        let collection = a
            .collection
            .unwrap_or_else(|| cfg.memory_collection.clone());
        let store = Store::connect().map_err(map_err)?;
        let old = store
            .get_point(&collection, &a.old_id)
            .await
            .map_err(map_err)?;
        let kind = old
            .as_ref()
            .and_then(|o| ps(&o.payload, "kind"))
            .unwrap_or_else(|| "memory".to_string());
        let dim = blocking(embed::dim).await?;
        store
            .ensure_collection(&collection, dim)
            .await
            .map_err(map_err)?;
        let new_text = a.new_text;
        let (new_text, vec) = blocking(move || {
            let vec = embed::embed_docs(std::slice::from_ref(&new_text))?
                .pop()
                .ok_or("embed produced no vector")?;
            Ok((new_text, vec))
        })
        .await?;
        let new_id = uuid::Uuid::new_v4().to_string();
        let mut extra = Map::new();
        extra.insert("text".into(), Value::from(new_text));
        extra.insert("source".into(), Value::from("memory"));
        extra.insert("supersedes".into(), Value::from(a.old_id.clone()));
        put_opt_str(&mut extra, "tags", a.tags);
        let payload = curate::base_payload(cfg, &kind, None, extra);
        store
            .upsert(&collection, &[(new_id.clone(), vec, payload)])
            .await
            .map_err(map_err)?;
        if old.is_some() {
            let mut patch = Map::new();
            patch.insert("status".into(), Value::from("superseded"));
            patch.insert("superseded_by".into(), Value::from(new_id.clone()));
            patch.insert("updated_at".into(), Value::from(curate::now_iso()));
            store
                .set_payload(&collection, &a.old_id, patch)
                .await
                .map_err(map_err)?;
        }
        text_result(format!("Superseded {} -> {}.", a.old_id, new_id))
    }

    #[tool(
        description = "Edit an item. If text changes it is re-embedded; otherwise just metadata."
    )]
    async fn kb_update(
        &self,
        Parameters(a): Parameters<UpdateArgs>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = config::get();
        let collection = a
            .collection
            .unwrap_or_else(|| cfg.memory_collection.clone());
        let store = Store::connect().map_err(map_err)?;
        let Some(pt) = store.get_point(&collection, &a.id).await.map_err(map_err)? else {
            return text_result(format!("No item {} in '{collection}'.", a.id));
        };
        let mut patch = Map::new();
        patch.insert("updated_at".into(), Value::from(curate::now_iso()));
        if let Some(status) = a.status {
            patch.insert("status".into(), Value::from(status));
        }
        if let Some(quality) = a.quality {
            patch.insert("quality".into(), Value::from(quality));
        }
        if let Some(tags) = a.tags {
            patch.insert("tags".into(), Value::from(tags));
        }
        if let Some(text) = a.text {
            // Re-embed: merge existing payload + patch + new text, then upsert in place (same id).
            let mut newp = pt.payload.clone();
            for (k, v) in &patch {
                newp.insert(k.clone(), v.clone());
            }
            newp.insert("text".into(), Value::from(text.clone()));
            let vec = blocking(move || {
                embed::embed_docs(&[text])?
                    .pop()
                    .ok_or_else(|| "embed produced no vector".to_string())
            })
            .await?;
            store
                .upsert(&collection, &[(a.id.clone(), vec, newp)])
                .await
                .map_err(map_err)?;
            return text_result(format!("Updated {} (re-embedded).", a.id));
        }
        store
            .set_payload(&collection, &a.id, patch)
            .await
            .map_err(map_err)?;
        text_result(format!("Updated {}.", a.id))
    }

    #[tool(description = "List all collections and their item counts.")]
    async fn kb_collections(&self) -> Result<CallToolResult, McpError> {
        let store = Store::connect().map_err(map_err)?;
        let cols = store.collections().await.map_err(map_err)?;
        if cols.is_empty() {
            return text_result("No collections yet.".to_string());
        }
        text_result(
            cols.iter()
                .map(|(n, c)| format!("{n}: {c} items"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    #[tool(
        description = "Submit a generated image (graph/table) to the fleet CAS (IPFS) so it can be embedded inline in a board comment. Base64-encode the image bytes and pass them as `data_base64` with a `filename`. Returns the content id (CID) and a ready-to-paste Markdown embed `![alt](ipfs://<cid>)` -- paste that into a board comment and it renders inline (the board and the Slack bridge each resolve the CID internally; never hard-code a gateway URL). CONFIDENTIALITY: `class` defaults to \"internal\" (membrain / amazon-internal content), which stays on the board and enterprise Slack and never reaches a public IPFS gateway; pass class=\"public\" only for a genuinely public or generic image."
    )]
    async fn cas_add_image(
        &self,
        Parameters(a): Parameters<CasAddImageArgs>,
    ) -> Result<CallToolResult, McpError> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(a.data_base64.trim().as_bytes())
            .map_err(|e| {
                map_err(format!(
                    "cas_add_image: data_base64 was not valid base64: {e}"
                ))
            })?;
        if bytes.is_empty() {
            return Err(map_err(
                "cas_add_image: decoded image was empty".to_string(),
            ));
        }
        let class = normalize_image_class(&a.class);
        let alt = a.alt.unwrap_or_else(|| a.filename.clone());
        // Add + pin to the fleet-internal Kubo. The node's reprovide stays off for internal content
        // (task_833), so the CID is not announced to the public DHT; the board resolves it via its internal
        // /api/ipfs route and the Slack bridge byte-uploads it, so it never reaches a public gateway.
        let added = Ipfs::connect()
            .add_bytes(&a.filename, &bytes)
            .await
            .map_err(map_err)?;
        let embed = image_embed(&alt, &added.cid);
        text_result(format!(
            "Stored to CAS and pinned.\ncid: {}\nclass: {}\nsize: {} bytes\n\nPaste this into a board comment to embed inline:\n{}",
            added.cid, class, added.size, embed
        ))
    }
}

/// A payload numeric field as i64 (page/chunk/counters), defaulting to 0 — matches the Python `p.get(k) or 0`.
fn pnum(p: &Map<String, Value>, key: &str) -> i64 {
    match p.get(key) {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

#[tool_handler]
impl ServerHandler for Kb {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "Curated knowledge base: semantic search over manuals, shop docs, and durable memories. \
                 kb_search (all collections by default) returns passages with an id/col; feed those back \
                 via kb_feedback / kb_mark_outdated. kb_read_pages walks a document in order. kb_remember \
                 stores a durable fact."
                    .to_string(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::schemars::schema_for;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn cite_uses_title_then_path_and_page() {
        let p =
            obj(json!({ "title": "Voron 2.4 Manual", "path": "voron/assembly.pdf", "page": 12 }));
        assert_eq!(cite(&p), "Voron 2.4 Manual — voron/assembly.pdf p.12");
    }

    #[test]
    fn cite_ipfs_appends_page_anchor() {
        let p = obj(json!({ "path": "a.pdf", "page": 3, "ipfs_url": "ipfs://cid/a.pdf" }));
        let c = cite(&p);
        assert!(c.contains("ipfs: ipfs://cid/a.pdf#page=3"), "{c}");
    }

    #[test]
    fn cite_missing_everything_is_question_marks() {
        let p = Map::new();
        assert_eq!(cite(&p), "? — ?");
    }

    #[test]
    fn ppage_treats_zero_as_absent() {
        assert_eq!(ppage(&obj(json!({ "page": 0 }))), None);
        assert_eq!(ppage(&obj(json!({ "page": 5 }))), Some(5));
        assert_eq!(ppage(&Map::new()), None);
    }

    // Every tool arg struct with no required-object fields still needs valid schemas; spot-check that the
    // schema generation doesn't panic and search's required field is present.
    #[test]
    fn search_schema_has_query() {
        let schema = serde_json::to_value(schema_for!(SearchArgs)).unwrap();
        assert!(schema.pointer("/properties/query").is_some(), "{schema}");
    }

    #[test]
    fn image_class_is_fail_closed_internal() {
        assert_eq!(normalize_image_class("public"), "public");
        assert_eq!(normalize_image_class("PUBLIC"), "public");
        assert_eq!(normalize_image_class("  public "), "public");
        // Anything else -> internal (default, typo, empty, unknown).
        assert_eq!(normalize_image_class("internal"), "internal");
        assert_eq!(normalize_image_class(""), "internal");
        assert_eq!(normalize_image_class("publik"), "internal");
        assert_eq!(normalize_image_class("Public graphs ok"), "internal");
    }

    #[test]
    fn image_embed_is_cid_only_and_alt_safe() {
        assert_eq!(
            image_embed("throughput", "bafyxyz"),
            "![throughput](ipfs://bafyxyz)"
        );
        // alt cannot break out of the Markdown image syntax or inject a newline.
        assert_eq!(image_embed("a]b\nc", "QmX"), "![a b c](ipfs://QmX)");
        // No hard-coded gateway anywhere — only the ipfs:// CID scheme.
        assert!(!image_embed("x", "QmX").contains("http"));
    }

    #[test]
    fn cas_add_image_schema_requires_data_and_filename() {
        let schema = serde_json::to_value(schema_for!(CasAddImageArgs)).unwrap();
        assert!(
            schema.pointer("/properties/data_base64").is_some(),
            "{schema}"
        );
        assert!(schema.pointer("/properties/filename").is_some(), "{schema}");
        assert!(schema.pointer("/properties/class").is_some(), "{schema}");
    }
}
