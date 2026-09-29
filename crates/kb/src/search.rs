//! `search` — ranked search: vector retrieve → optional cross-encoder rerank → curation blend.
//!
//! A faithful port of Python `kb/search.py`. Two entry points: [`search`] (one collection) and [`search_all`]
//! (every collection, one globally-ranked list). Both follow the same shape — embed the query, pull vector
//! candidates, optionally rerank the top texts, blend curation signals, sort by `final`, truncate to `limit`.
//! The ordering here IS what agents see, so the blend + the bounded-rerank candidate math match Python exactly.

use serde_json::{Map, Value};

use crate::{config, curate, embed, rerank, store::Store};

/// One ranked result — the Python result dict (`id`/`collection`/`relevance`/`final`/`payload`).
#[derive(Debug, Clone)]
pub struct Hit {
    /// Point id as a string (UUID or legacy integer, stringified — see `store::Candidate::id_str`).
    pub id: String,
    pub collection: String,
    /// The reranker probability (or the raw vector score when rerank is off). Part of the result contract
    /// (the Python dict exposes it); the CLI/MCP surface currently prints only `final_score`.
    #[allow(dead_code)]
    pub relevance: f64,
    /// The blended rank actually sorted on.
    pub final_score: f64,
    pub payload: Map<String, Value>,
}

/// Search one collection — the Python `search`. `use_rerank = None` defers to `rerank_enabled`.
pub fn search(
    store: &Store,
    collection: &str,
    query: &str,
    limit: usize,
    include_outdated: bool,
    use_rerank: Option<bool>,
) -> Result<Vec<Hit>, String> {
    let cfg = config::get();
    let use_rerank = use_rerank.unwrap_or(cfg.rerank_enabled);
    let qv = embed::embed_query(query)?;
    // Rerank needs a wider candidate pool than the caller's `limit` so the cross-encoder can reorder.
    let n = if use_rerank {
        limit.max(cfg.rerank_candidates)
    } else {
        limit
    };
    let cands = store.query_candidates(collection, &qv, n, include_outdated)?;
    if cands.is_empty() {
        return Ok(vec![]);
    }

    let texts: Vec<String> = cands.iter().map(|c| text_of(&c.payload)).collect();
    let relevances: Vec<f64> = if use_rerank {
        rerank::rerank(query, &texts)?
    } else {
        cands.iter().map(|c| c.score).collect()
    };

    let mut scored: Vec<Hit> = cands
        .iter()
        .zip(relevances)
        .map(|(c, rel)| Hit {
            id: c.id_str(),
            collection: collection.to_string(),
            relevance: rel,
            final_score: curate::blend(cfg, rel, &c.payload),
            payload: c.payload.clone(),
        })
        .collect();
    sort_and_truncate(&mut scored, limit);
    Ok(scored)
}

/// Search every collection and return one globally-ranked list — the Python `search_all`. Rerank work is
/// bounded: gather `rerank_candidates` per collection, keep the globally best that many by VECTOR score,
/// then rerank/blend only those.
pub fn search_all(
    store: &Store,
    query: &str,
    limit: usize,
    include_outdated: bool,
    use_rerank: Option<bool>,
) -> Result<Vec<Hit>, String> {
    let cfg = config::get();
    let use_rerank = use_rerank.unwrap_or(cfg.rerank_enabled);
    let qv = embed::embed_query(query)?;

    // (collection, candidate) across all collections.
    let mut pool: Vec<(String, crate::store::Candidate)> = Vec::new();
    for (name, _count) in store.collections()? {
        for c in store.query_candidates(&name, &qv, cfg.rerank_candidates, include_outdated)? {
            pool.push((name.clone(), c));
        }
    }
    if pool.is_empty() {
        return Ok(vec![]);
    }

    // Keep the globally best by vector score before the (bounded) rerank.
    pool.sort_by(|a, b| {
        b.1.score
            .partial_cmp(&a.1.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let relevances: Vec<f64> = if use_rerank {
        pool.truncate(cfg.rerank_candidates);
        let texts: Vec<String> = pool.iter().map(|(_, c)| text_of(&c.payload)).collect();
        rerank::rerank(query, &texts)?
    } else {
        pool.truncate(limit);
        pool.iter().map(|(_, c)| c.score).collect()
    };

    let mut scored: Vec<Hit> = pool
        .iter()
        .zip(relevances)
        .map(|((name, c), rel)| Hit {
            id: c.id_str(),
            collection: name.clone(),
            relevance: rel,
            final_score: curate::blend(cfg, rel, &c.payload),
            payload: c.payload.clone(),
        })
        .collect();
    sort_and_truncate(&mut scored, limit);
    Ok(scored)
}

/// The payload `text` field, or "" — the Python `(c.payload or {}).get("text", "")`.
fn text_of(payload: &Map<String, Value>) -> String {
    payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Sort by `final` descending and keep the top `limit`. Python uses a stable sort; `sort_by` is stable, so
/// ties keep their pre-sort (vector-score) order.
fn sort_and_truncate(scored: &mut Vec<Hit>, limit: usize) {
    scored.sort_by(|a, b| {
        b.final_score
            .partial_cmp(&a.final_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(limit);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hit(final_score: f64) -> Hit {
        Hit {
            id: "x".into(),
            collection: "c".into(),
            relevance: final_score,
            final_score,
            payload: Map::new(),
        }
    }

    #[test]
    fn sort_is_descending_and_truncates() {
        let mut v = vec![hit(0.1), hit(0.9), hit(0.5)];
        sort_and_truncate(&mut v, 2);
        assert_eq!(v.len(), 2);
        assert!(v[0].final_score > v[1].final_score);
        assert!((v[0].final_score - 0.9).abs() < 1e-9);
    }

    #[test]
    fn text_of_defaults_empty() {
        let p = json!({ "kind": "doc" }).as_object().unwrap().clone();
        assert_eq!(text_of(&p), "");
        let p = json!({ "text": "hello" }).as_object().unwrap().clone();
        assert_eq!(text_of(&p), "hello");
    }
}
