//! `inbox` — the drop-folder ingest worker (`kb inbox`). A faithful port of Python `kb.inbox`.
//!
//! Oneshot drain of the configured inbox dir: every ingestable file is chunked into the knowledge base and
//! pinned to IPFS, then (on success) deleted; failures are quarantined under `_failed/<rel>` so the drain
//! never aborts. The first path component under the inbox is the file's collection (sanitized); a file in
//! the root goes to the default collection.
//!
//! Byte-identical to the Python worker where it matters: the point id is `chunk::id("inbox", rel, page,
//! idx)` with `page` the 1-based PDF page or the literal `"None"` for non-paginated text, and the payload is
//! `curate::base_payload("memory", …)` with the same fields. IO is async (store/ipfs over reqwest);
//! embedding runs off-reactor via `spawn_blocking` (#439).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::ipfs::Ipfs;
use crate::store::Store;
use crate::{chunk, config, curate, embed, extract};

/// The `_id` first part + payload `source` — the live `KB_INBOX_SOURCE` default. Not overridden by the role.
const SOURCE: &str = "inbox";
/// The payload `kind` — the live `KB_INBOX_KIND` default. Not overridden by the drop-folder role.
const KIND: &str = "memory";

/// Drain the inbox once — the Python `process_inbox`. Ingests every file (deleting on success, quarantining
/// failures to `_failed/`), then prunes emptied collection folders. Returns after one pass.
pub async fn run() -> Result<(), String> {
    let cfg = config::get();
    let inbox = PathBuf::from(&cfg.inbox_dir);
    let failed = inbox.join("_failed");
    let store = Store::connect()?;
    let ipfs = Ipfs::connect();
    // The embedding dimension (sizes a new collection). Computed once, off-reactor (model load is CPU-heavy).
    let dim = tokio::task::spawn_blocking(embed::dim)
        .await
        .map_err(|e| format!("embed dim task panicked: {e}"))??;

    // Files in a deterministic order, skipping anything already quarantined under `_failed/`.
    let files: Vec<PathBuf> = extract::iter_files(&inbox)
        .into_iter()
        .filter(|f| !f.starts_with(&failed))
        .collect();

    let (mut chunks_ok, mut files_ok, mut failures) = (0usize, 0usize, 0usize);
    for f in files {
        let rel = match f.strip_prefix(&inbox) {
            Ok(r) => r.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        let parts: Vec<&str> = rel.split('/').collect();
        let collection = collection_for(&parts, &cfg.inbox_default_collection);
        match ingest_file(&store, &ipfs, cfg, dim, &f, &rel, &collection).await {
            Ok((0, _)) => {
                // No extractable text — quarantine (not retryable, but keep for inspection).
                tracing::warn!("inbox: no text in {rel}; moving to _failed");
                move_to_failed(&f, &failed, &rel);
                failures += 1;
            }
            Ok((n, None)) => {
                // Ingested, but the IPFS pin failed — keep the original under _failed for a later retry.
                tracing::warn!(
                    "inbox: {rel} ingested ({n} chunks) but IPFS pin failed; kept in _failed"
                );
                move_to_failed(&f, &failed, &rel);
                chunks_ok += n;
            }
            Ok((n, Some(_cid))) => {
                chunks_ok += n;
                files_ok += 1;
                if let Err(e) = std::fs::remove_file(&f) {
                    tracing::warn!("inbox: ingested {rel} but could not delete it: {e}");
                }
            }
            Err(e) => {
                tracing::error!("inbox: ERROR {rel}: {e}");
                move_to_failed(&f, &failed, &rel);
                failures += 1;
            }
        }
    }
    prune_empty_dirs(&inbox, &failed);
    tracing::info!("inbox drain complete: {files_ok} files, {chunks_ok} chunks, {failures} failed");
    Ok(())
}

/// Ingest one file — the Python `ingest_file`. Extract → chunk → embed (batch) → one upsert → IPFS pin →
/// stamp `{cid, ipfs_url}` onto the file's points. Returns `(chunk_count, cid)`; `cid` is `None` when the
/// pin failed (the file's chunks are still ingested). Extract/embed/upsert errors propagate.
async fn ingest_file(
    store: &Store,
    ipfs: &Ipfs,
    cfg: &config::Config,
    dim: usize,
    f: &Path,
    rel: &str,
    collection: &str,
) -> Result<(usize, Option<String>), String> {
    // Extract off-reactor (file read + PDFium are blocking), then chunk each (page, text) unit.
    let f_owned = f.to_path_buf();
    let units = tokio::task::spawn_blocking(move || extract::extract_units(&f_owned))
        .await
        .map_err(|e| format!("extract task panicked: {e}"))??;

    let stem = Path::new(rel)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let abs_path = f.to_string_lossy().to_string();

    let mut ids: Vec<String> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    let mut payloads: Vec<Map<String, Value>> = Vec::new();
    for (page, text) in &units {
        // `str(None) == "None"` and 1-based page numbers are part of the id key — preserve exactly.
        let page_key = match page {
            Some(p) => p.to_string(),
            None => "None".to_string(),
        };
        for (idx, piece) in chunk::chunk_default(text).into_iter().enumerate() {
            let idx_str = idx.to_string();
            ids.push(chunk::id(&[SOURCE, rel, &page_key, &idx_str]));
            let mut extra = Map::new();
            extra.insert("text".into(), Value::from(piece.clone()));
            extra.insert("source".into(), Value::from(SOURCE));
            extra.insert("path".into(), Value::from(rel));
            extra.insert("abs_path".into(), Value::from(abs_path.clone()));
            extra.insert("title".into(), Value::from(stem.clone()));
            if let Some(p) = page {
                extra.insert("page".into(), Value::from(*p));
            }
            extra.insert("chunk".into(), Value::from(idx as i64));
            payloads.push(curate::base_payload(cfg, KIND, None, extra));
            texts.push(piece);
        }
    }
    if texts.is_empty() {
        return Ok((0, None));
    }

    // Embed the whole file's chunks in one batch, off-reactor.
    let to_embed = texts.clone();
    let vectors = tokio::task::spawn_blocking(move || embed::embed_docs(&to_embed))
        .await
        .map_err(|e| format!("embed task panicked: {e}"))??;

    store.ensure_collection(collection, dim).await?;
    let points: Vec<(String, Vec<f32>, Map<String, Value>)> = ids
        .iter()
        .cloned()
        .zip(vectors)
        .zip(payloads)
        .map(|((id, vec), pl)| (id, vec, pl))
        .collect();
    store.upsert(collection, &points).await?;

    // Pin the whole file to IPFS and stamp the resulting CID onto every chunk's point. A pin failure is
    // non-fatal (the chunks are already searchable) — return cid=None so the caller keeps the file for retry.
    match ipfs.add(f).await {
        Ok(added) => {
            let ipfs_url = format!(
                "{}/ipfs/{}",
                cfg.ipfs_gateway.trim_end_matches('/'),
                added.cid
            );
            let mut patch = Map::new();
            patch.insert("cid".into(), Value::from(added.cid.clone()));
            patch.insert("ipfs_url".into(), Value::from(ipfs_url));
            for id in &ids {
                if let Err(e) = store.set_payload(collection, id, patch.clone()).await {
                    tracing::warn!("inbox: set cid payload on {id} failed: {e}");
                }
            }
            Ok((texts.len(), Some(added.cid)))
        }
        Err(e) => {
            tracing::warn!("inbox: IPFS FAILED {rel}: {e}");
            Ok((texts.len(), None))
        }
    }
}

/// The collection for a file — the Python `_collection_for`: the sanitized top folder, or the default
/// collection when the file sits directly in the inbox root (no subfolder).
fn collection_for(rel_parts: &[&str], default: &str) -> String {
    if rel_parts.len() > 1 {
        sanitize(rel_parts[0], default)
    } else {
        default.to_string()
    }
}

/// The Python `_sanitize`: replace each run of chars outside `[A-Za-z0-9_.-]` with a single `-`, strip
/// leading/trailing `-_.`, lowercase; fall back to `default` if the result is empty.
fn sanitize(name: &str, default: &str) -> String {
    let mut s = String::with_capacity(name.len());
    let mut in_dash_run = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
            s.push(c);
            in_dash_run = false;
        } else if !in_dash_run {
            s.push('-');
            in_dash_run = true;
        }
    }
    let s = s
        .trim_matches(|c| matches!(c, '-' | '_' | '.'))
        .to_ascii_lowercase();
    if s.is_empty() { default.to_string() } else { s }
}

/// Move a file to `<failed>/<rel>` (creating parents), so a failed ingest is quarantined out of the next
/// drain rather than reprocessed. Best-effort — a move failure is logged, not fatal.
fn move_to_failed(f: &Path, failed: &Path, rel: &str) {
    let dest = failed.join(rel);
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::rename(f, &dest) {
        tracing::warn!("inbox: could not quarantine {rel} to _failed: {e}");
    }
}

/// Remove now-empty collection directories left behind after a drain — the Python `_prune_empty_dirs`. The
/// inbox root and the `_failed` tree are never removed. Best-effort (a non-empty or busy dir is skipped).
fn prune_empty_dirs(inbox: &Path, failed: &Path) {
    // contents_first so a child dir is visited (and possibly removed) before its parent.
    for entry in walkdir::WalkDir::new(inbox)
        .contents_first(true)
        .into_iter()
        .filter_map(Result::ok)
    {
        let p = entry.path();
        if !entry.file_type().is_dir() || p == inbox || p.starts_with(failed) {
            continue;
        }
        // remove_dir only succeeds on an empty directory — exactly the prune we want.
        let _ = std::fs::remove_dir(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_for_uses_top_folder_or_default() {
        // A subfolder -> its sanitized name; a root-level file -> default.
        assert_eq!(collection_for(&["cadenza", "foo.md"], "inbox"), "cadenza");
        assert_eq!(collection_for(&["foo.md"], "inbox"), "inbox");
    }

    #[test]
    fn sanitize_matches_python_rules() {
        // Runs of disallowed chars collapse to a single '-'; leading/trailing -_. stripped; lowercased.
        assert_eq!(sanitize("My Docs!!", "inbox"), "my-docs");
        assert_eq!(sanitize("__weird__", "inbox"), "weird");
        // Dots are preserved (namespaced collection names).
        assert_eq!(sanitize("camshaft.cadenza", "inbox"), "camshaft.cadenza");
        // Empties fall back to the default.
        assert_eq!(sanitize("///", "inbox"), "inbox");
        assert_eq!(sanitize("---", "inbox"), "inbox");
    }
}
