//! `curate` — curation metadata: payload defaults, timestamps, and the ranking blend.
//!
//! A faithful port of Python `kb/curate.py`. The blend + weights DEFINE result ordering, so every constant
//! and formula here matches the Python one-for-one (the `#[cfg(test)]` cases pin the arithmetic).

use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::config::Config;

/// Current UTC time as an RFC3339 string — the Python `now_iso()`.
pub fn now_iso() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// Parse an RFC3339 timestamp to unix seconds; `None` if absent/unparseable — the Python `_epoch`.
fn epoch(iso: Option<&str>) -> Option<f64> {
    let iso = iso?;
    if iso.is_empty() {
        return None;
    }
    OffsetDateTime::parse(iso, &Rfc3339)
        .ok()
        .map(|t| t.unix_timestamp() as f64)
}

/// Build a payload with curation defaults (the Python `base_payload`). `extra` is merged in on top; the
/// caller is responsible for having already dropped None-valued extras (mirrors Python's `if v is not None`).
pub fn base_payload(
    cfg: &Config,
    kind: &str,
    authority: Option<f64>,
    extra: Map<String, Value>,
) -> Map<String, Value> {
    let ts = now_iso();
    let mut p = Map::new();
    p.insert("kind".into(), Value::from(kind));
    p.insert(
        "authority".into(),
        Value::from(authority.unwrap_or_else(|| cfg.authority_for(kind))),
    );
    p.insert("status".into(), Value::from("active"));
    p.insert("quality".into(), Value::from(0.0));
    p.insert("helpful".into(), Value::from(0));
    p.insert("unhelpful".into(), Value::from(0));
    p.insert("use_count".into(), Value::from(0));
    p.insert("created_at".into(), Value::from(ts.clone()));
    p.insert("updated_at".into(), Value::from(ts.clone()));
    p.insert("last_verified".into(), Value::from(ts));
    for (k, v) in extra {
        p.insert(k, v);
    }
    p
}

/// A JSON payload value as f64, tolerating ints/floats/strings/null — the Python `float(p.get(...) or 0)`.
fn num(p: &Map<String, Value>, key: &str) -> f64 {
    match p.get(key) {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// `tanh(net_votes / 3)` in [-1, 1] — the Python `vote_signal`.
pub fn vote_signal(p: &Map<String, Value>) -> f64 {
    let net = num(p, "helpful") - num(p, "unhelpful");
    (net / 3.0).tanh()
}

/// Recency score in (0, 1]. Only memories decay; manuals/docs are static references (always 1.0). Ported
/// from the Python `recency_score`.
pub fn recency_score(cfg: &Config, p: &Map<String, Value>) -> f64 {
    if p.get("kind").and_then(Value::as_str) != Some("memory") {
        return 1.0;
    }
    let ref_epoch = epoch(
        p.get("last_verified")
            .and_then(Value::as_str)
            .or_else(|| p.get("created_at").and_then(Value::as_str)),
    );
    let Some(ref_epoch) = ref_epoch else {
        return 0.5;
    };
    let age_days = ((now_epoch() - ref_epoch) / 86400.0).max(0.0);
    (-age_days / cfg.recency_halflife_days.max(1.0)).exp()
}

fn now_epoch() -> f64 {
    OffsetDateTime::now_utc().unix_timestamp() as f64
}

/// Combine the relevance score with curation signals into a final rank — the Python `blend`.
pub fn blend(cfg: &Config, relevance: f64, p: &Map<String, Value>) -> f64 {
    let q = num(p, "quality").tanh();
    let a = match p.get("authority") {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.5),
        _ => 0.5,
    };
    relevance
        + cfg.w_quality * q
        + cfg.w_authority * a
        + cfg.w_votes * vote_signal(p)
        + cfg.w_recency * recency_score(cfg, p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> Config {
        Config::default()
    }

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn vote_signal_matches_tanh() {
        let p = obj(json!({"helpful": 3, "unhelpful": 0}));
        assert!((vote_signal(&p) - (1.0f64).tanh()).abs() < 1e-9);
        let p = obj(json!({"helpful": 0, "unhelpful": 0}));
        assert_eq!(vote_signal(&p), 0.0);
    }

    #[test]
    fn non_memory_recency_is_one() {
        let p = obj(json!({"kind": "manual"}));
        assert_eq!(recency_score(&cfg(), &p), 1.0);
        let p = obj(json!({"kind": "doc"}));
        assert_eq!(recency_score(&cfg(), &p), 1.0);
    }

    #[test]
    fn memory_without_timestamp_is_half() {
        let p = obj(json!({"kind": "memory"}));
        assert_eq!(recency_score(&cfg(), &p), 0.5);
    }

    #[test]
    fn blend_static_doc_is_relevance_plus_authority_terms() {
        // A pristine doc: quality 0, no votes, authority 0.8, recency 1.0.
        // blend = rel + 0.15*tanh(0) + 0.10*0.8 + 0.15*tanh(0) + 0.05*1.0 = rel + 0.08 + 0.05
        let p = obj(
            json!({"kind": "doc", "quality": 0.0, "authority": 0.8, "helpful": 0, "unhelpful": 0}),
        );
        let got = blend(&cfg(), 0.5, &p);
        assert!((got - (0.5 + 0.08 + 0.05)).abs() < 1e-9, "got {got}");
    }

    #[test]
    fn base_payload_has_defaults_and_merges_extra() {
        let extra = obj(json!({"text": "hi", "path": "a/b.md"}));
        let p = base_payload(&cfg(), "manual", None, extra);
        assert_eq!(p.get("kind").unwrap(), "manual");
        assert_eq!(p.get("authority").unwrap(), 1.0); // manual → 1.0
        assert_eq!(p.get("status").unwrap(), "active");
        assert_eq!(p.get("text").unwrap(), "hi");
        assert!(p.contains_key("created_at") && p.contains_key("last_verified"));
    }
}
