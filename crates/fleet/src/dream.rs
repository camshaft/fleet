//! `dream` — the dream pass (task_827), the Rust port of `dream_analyze.py` (task_956: fleet tooling is
//! Rust, not Python). PROPOSE-ONLY: it reads board-backed memory (a JSONL corpus) and EMITS a reviewed
//! worklist (a dream-report); it NEVER applies a change. Disposition is a separate authenticated step
//! (`dream_apply`, a follow-on increment of task_956).
//!
//! The worklist schema is task_827's librarian-blessed `dream-report/v1`, with all four markups folded in:
//! widened protected triggers (operator-directive / tenet / `MEMORY.md` / every `index-*` sub-index / the
//! kb-reconciliation ledger / canon pointers), any-target-protected propagation, verified-dangling
//! staleness, and the supersede-preserving lane.
//!
//! Detectors ported: the protected-class classifier, the within-repo backlink index, the exact-duplicate
//! detector (within-repo merge / cross-repo twin / orphan add-links), the MinHash+LSH near-duplicate
//! detector, and the write-later advisory. The deterministic detectors are byte-parity-validated against the
//! Python; the near-duplicate detector is a heuristic prefilter that in the Python relies on a salted
//! `hash()` (not deterministic even run-to-run), so this port uses a fixed FNV-1a shingle hash and a
//! fixed-seed MinHash — deterministic across runs, but validated by clustering behavior, not byte-equality.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::memory::sha256_hex;

/// The librarian's dangling-forward-refs tracker slug (comment_3944): slugs it catalogues are intentional
/// write-later markers and are suppressed from the write-later advisory.
const FORWARD_REF_TRACKER: &str = "librarian-dangling-forward-refs-in-peer-notes-2026-08-03";

/// A memory record as loaded from the JSONL corpus (board-identical bodies, verified during the task_826
/// migration). Only the fields the detectors read are typed; unknown fields are ignored.
#[derive(Deserialize, Clone)]
struct Rec {
    slug: String,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, rename = "type")]
    rtype: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    doc_id: Option<Value>,
    #[serde(default)]
    provenance: Option<Provenance>,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Deserialize, Clone, Default)]
struct Provenance {
    #[serde(default)]
    author: Option<Value>,
    #[serde(default)]
    timestamp: Option<Value>,
}

impl Rec {
    fn repo(&self) -> &str {
        self.repo.as_deref().unwrap_or("?")
    }
    fn path(&self) -> &str {
        self.path.as_deref().unwrap_or("")
    }
    fn body(&self) -> &str {
        self.body.as_deref().unwrap_or("")
    }
}

/// Collect every `[[name]]` wiki-link capture in `body`, in order and WITH duplicates — the Python
/// `LINK_RE.findall` (`\[\[([^\]|#]+)`: capture one-or-more chars up to the first `]`, `|`, or `#`). Callers
/// trim/dedup as the Python does at each site. Pure.
fn find_links(body: &str) -> Vec<String> {
    let b = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'[' && b[i + 1] == b'[' {
            let start = i + 2;
            let mut j = start;
            while j < b.len() && !matches!(b[j], b']' | b'|' | b'#') {
                j += 1;
            }
            if j > start {
                out.push(body[start..j].to_string());
                i = j;
            } else {
                i += 2;
            }
        } else {
            i += 1;
        }
    }
    out
}

/// Whether `s` is a real memory slug — the Python `SLUG_RE` `^[a-z0-9][a-z0-9-]{2,}$` (lowercase kebab, a
/// `[a-z0-9]` lead then 2+ of `[a-z0-9-]`, so 3+ chars total). Pure.
fn is_slug(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 3 {
        return false;
    }
    let lead_ok = b[0].is_ascii_lowercase() || b[0].is_ascii_digit();
    lead_ok
        && b[1..]
            .iter()
            .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// Whitespace-normalized body for exact-duplicate identity: trim the whole body, then right-trim each line
/// and rejoin with `\n` (ignores trailing/indent churn). Mirrors the Python `norm_body`. Pure.
fn norm_body(body: &str) -> String {
    body.trim()
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The reasons a memory is protected-class canon (librarian-only disposition), in the Python's append order.
/// Conservative — over-protect rather than under. An empty list means standard lane. Pure.
fn protected_reasons(rec: &Rec) -> Vec<String> {
    let slug = &rec.slug;
    let name = rec.name.as_deref().unwrap_or("").to_lowercase();
    let typ = rec.rtype.as_deref().unwrap_or("").to_lowercase();
    let body = rec.body();
    let mut reasons = Vec::new();
    if slug.starts_with("index-") {
        reasons.push("sub-index canon (index-*.md, single-writer)".to_string());
    }
    if slug.contains("kb-reconciliation-ledger") {
        reasons.push("kb-reconciliation ledger (librarian-owned)".to_string());
    }
    if typ == "tenet" || slug.contains("tenet") {
        reasons.push("tenet memory".to_string());
    }
    if (slug.contains("operator") && (slug.contains("directive") || slug.contains("standing")))
        || name.contains("operator-standing-directive")
    {
        reasons.push("operator-directive memory".to_string());
    }
    // canon-pointer heuristic: a thin index whose body is mostly [[wiki-links]] with little prose.
    let links = find_links(body);
    let nonblank = body.lines().filter(|l| !l.trim().is_empty()).count();
    if !links.is_empty()
        && nonblank > 0
        && links.len() >= 8
        && (links.len() as f64) >= 0.6 * (nonblank as f64)
    {
        reasons.push("canon pointer (body is predominantly wiki-link pointers)".to_string());
    }
    reasons
}

/// Within-repo backlink index: `(repo, slug) -> [referencing slugs in the same repo]`. Memory `[[links]]`
/// resolve only WITHIN a repo's store (separate link namespaces per repo), so only same-repo references
/// count. Pure.
fn build_backlinks(recs: &[Rec]) -> HashMap<(String, String), Vec<String>> {
    let mut slugs_by_repo: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    for r in recs {
        slugs_by_repo
            .entry(r.repo())
            .or_default()
            .insert(r.slug.as_str());
    }
    let mut backlinks: HashMap<(String, String), Vec<String>> = HashMap::new();
    for r in recs {
        let repo = r.repo();
        let mut seen = BTreeSet::new();
        for link in find_links(r.body()) {
            let link = link.trim().to_string();
            if !seen.insert(link.clone()) {
                continue; // the Python iterates set(findall(...)): each distinct link once
            }
            if link != r.slug
                && slugs_by_repo
                    .get(repo)
                    .is_some_and(|s| s.contains(link.as_str()))
            {
                backlinks
                    .entry((repo.to_string(), link))
                    .or_default()
                    .push(r.slug.clone());
            }
        }
    }
    backlinks
}

/// The per-target descriptor the report embeds for each memory in a proposal (doc_id/path/name/type/repo +
/// its within-repo backlink count and up to 12 referencing slugs). Mirrors the Python `_target`.
fn target_json(r: &Rec, backlinks: &HashMap<(String, String), Vec<String>>) -> Value {
    let refs = backlinks
        .get(&(r.repo().to_string(), r.slug.clone()))
        .cloned()
        .unwrap_or_default();
    let mut sorted_refs = refs.clone();
    sorted_refs.sort();
    sorted_refs.truncate(12);
    json!({
        "doc_id": r.doc_id.clone().unwrap_or(Value::Null),
        "path": r.path(),
        "name": r.name,
        "type": r.rtype,
        "repo": r.repo(),
        "backlink_count": refs.len(),
        "backlinks": sorted_refs,
    })
}

/// Character count (Python `len(str)` is code points, not bytes) — used for the survivor sort keys so the
/// choice matches the Python exactly on multibyte descriptions/paths.
fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// The merge-survivor of a duplicate/near-duplicate group: richest metadata (longest description) then
/// shortest path, ties broken by original order — the Python `sorted(group, key=(-len(desc), len(path)))[0]`
/// (a stable sort picks the first). Pure.
fn pick_survivor<'a>(group: &[&'a Rec]) -> &'a Rec {
    group
        .iter()
        .enumerate()
        .min_by(|(ia, a), (ib, b)| {
            let ka = (
                Reverse(char_len(a.description.as_deref().unwrap_or(""))),
                char_len(a.path()),
                *ia,
            );
            let kb = (
                Reverse(char_len(b.description.as_deref().unwrap_or(""))),
                char_len(b.path()),
                *ib,
            );
            ka.cmp(&kb)
        })
        .map(|(_, r)| *r)
        .unwrap()
}

// ---- MinHash + LSH near-duplicate detector -----------------------------------------------------
// A FUZZY near-duplicate prefilter: MinHash signatures (m hashes) banded into LSH buckets surface candidate
// pairs; each candidate is then verified with the true Jaccard of its k-word shingle sets. Unlike the Python
// (which hashes shingles with the salted built-in `hash()`, non-deterministic even run-to-run), this uses a
// fixed FNV-1a shingle hash and a fixed-seed SplitMix64 for the MinHash coefficients, so the Rust output is
// deterministic. The clustering is a heuristic prefilter either way; byte-parity vs the Python is neither
// achievable nor meaningful, so this detector is validated by its clustering behavior, not byte-equality.

const MINHASH_M: usize = 64;
const LSH_BANDS: usize = 16;
const NEAR_DUP_THRESH: f64 = 0.80;
const SHINGLE_K: usize = 5;
const SHINGLE_CAP: usize = 1500;
const MINHASH_PRIME: u64 = (1 << 61) - 1;

/// Lowercase `\w+` word tokens (Unicode-aware alphanumerics plus `_`), mirroring the Python
/// `re.findall(r"\w+", body.lower())`. Pure.
fn word_tokens(body: &str) -> Vec<String> {
    body.to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// A fixed, deterministic 64-bit FNV-1a hash of `s` (the shingle fingerprint). Pure.
fn fnv1a_64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The set of k-word shingle hashes of a normalized body (a lexical fingerprint for Jaccard similarity),
/// bounded to the `SHINGLE_CAP` smallest hashes (a bottom-k sketch, itself Jaccard-preserving). A `BTreeSet`
/// keeps the smallest-k selection and the set operations deterministic. Mirrors the Python `_shingles`. Pure.
fn shingles(body: &str) -> BTreeSet<u64> {
    let toks = word_tokens(body);
    if toks.len() < SHINGLE_K {
        return if toks.is_empty() {
            BTreeSet::new()
        } else {
            BTreeSet::from([fnv1a_64(&toks.join(" "))])
        };
    }
    let mut sh: BTreeSet<u64> = (0..=toks.len() - SHINGLE_K)
        .map(|i| fnv1a_64(&toks[i..i + SHINGLE_K].join(" ")))
        .collect();
    if sh.len() > SHINGLE_CAP {
        sh = sh.into_iter().take(SHINGLE_CAP).collect();
    }
    sh
}

/// Deterministic SplitMix64 step — advances `state` and returns the next pseudo-random u64. Pure (given the
/// mutable state), used only to generate the fixed MinHash coefficients.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The fixed MinHash coefficients `(a, b)` with `a in [1, PRIME)` and `b in [0, PRIME)` — a fixed seed makes
/// the signatures deterministic across runs (the role Python gives `random.Random(1729)`). Pure.
fn minhash_coeffs() -> Vec<(u64, u64)> {
    let mut st: u64 = 1729;
    (0..MINHASH_M)
        .map(|_| {
            let a = splitmix64(&mut st) % (MINHASH_PRIME - 1) + 1;
            let b = splitmix64(&mut st) % MINHASH_PRIME;
            (a, b)
        })
        .collect()
}

/// The MinHash signature of a shingle set: for each coefficient `(a, b)`, the minimum of `(a*s + b) mod PRIME`
/// over the shingles. `sh` must be non-empty (the caller signals an empty-shingle record separately). Pure.
fn minhash_sig(sh: &BTreeSet<u64>, coeffs: &[(u64, u64)]) -> Vec<u64> {
    coeffs
        .iter()
        .map(|&(a, b)| {
            sh.iter()
                .map(|&s| {
                    let s = u128::from(s % MINHASH_PRIME);
                    ((u128::from(a) * s + u128::from(b)) % u128::from(MINHASH_PRIME)) as u64
                })
                .min()
                .expect("shingle set is non-empty")
        })
        .collect()
}

/// Jaccard similarity of two shingle sets (`|a & b| / |a | b|`). Pure.
fn jaccard(a: &BTreeSet<u64>, b: &BTreeSet<u64>) -> f64 {
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// Round to 3 decimals (the Python `round(j, 3)` on similarities/confidence).
fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Detect fuzzy near-duplicate clusters via MinHash+LSH then true-Jaccard verification, skipping pairs
/// already caught as exact duplicates. Within-repo clusters -> a human-review merge candidate; cross-repo
/// clusters -> a keep-both cross-repo-twin-fuzzy annotation. Mirrors the Python `detect_near_duplicates`;
/// the clustering is deterministic (fixed hash + seed) but is a heuristic, not byte-parity with the Python.
fn detect_near_duplicates(
    recs: &[Rec],
    prot: &HashMap<String, Vec<String>>,
    backlinks: &HashMap<(String, String), Vec<String>>,
    exact_hashes: &HashMap<String, String>,
) -> Vec<Value> {
    let coeffs = minhash_coeffs();
    let rows = MINHASH_M / LSH_BANDS;

    let mut shing: HashMap<String, BTreeSet<u64>> = HashMap::new();
    let mut sigs: HashMap<String, Option<Vec<u64>>> = HashMap::new();
    let mut by_path: HashMap<&str, &Rec> = HashMap::new();
    for r in recs {
        by_path.insert(r.path(), r);
        let sh = shingles(r.body());
        let sig = if sh.is_empty() {
            None
        } else {
            Some(minhash_sig(&sh, &coeffs))
        };
        shing.insert(r.path().to_string(), sh);
        sigs.insert(r.path().to_string(), sig);
    }

    // LSH: candidate pairs share a band's row-slice bucket in some band.
    let mut candidates: BTreeSet<(String, String)> = BTreeSet::new();
    for band in 0..LSH_BANDS {
        let lo = band * rows;
        let mut buckets: HashMap<Vec<u64>, Vec<&str>> = HashMap::new();
        for (p, sig) in &sigs {
            if let Some(sig) = sig {
                buckets
                    .entry(sig[lo..lo + rows].to_vec())
                    .or_default()
                    .push(p);
            }
        }
        for grp in buckets.values() {
            if grp.len() > 1 {
                for i in 0..grp.len() {
                    for j in (i + 1)..grp.len() {
                        let (a, b) = (grp[i], grp[j]);
                        let pair = if a <= b {
                            (a.to_string(), b.to_string())
                        } else {
                            (b.to_string(), a.to_string())
                        };
                        candidates.insert(pair);
                    }
                }
            }
        }
    }

    // Verify candidates with true Jaccard; drop exact-dup pairs (handled by the exact-dup detector).
    let mut adj: HashMap<String, BTreeSet<String>> = HashMap::new();
    let mut sim: HashMap<(String, String), f64> = HashMap::new();
    for (a, b) in &candidates {
        let (ha, hb) = (exact_hashes.get(a), exact_hashes.get(b));
        if ha.is_some() && ha == hb {
            continue;
        }
        let (sa, sb) = (&shing[a], &shing[b]);
        if sa.is_empty() || sb.is_empty() {
            continue;
        }
        let j = jaccard(sa, sb);
        if j >= NEAR_DUP_THRESH {
            adj.entry(a.clone()).or_default().insert(b.clone());
            adj.entry(b.clone()).or_default().insert(a.clone());
            sim.insert((a.clone(), b.clone()), round3(j));
        }
    }

    // Connected components -> clusters (sorted iteration for deterministic output).
    let mut keys: Vec<&String> = adj.keys().collect();
    keys.sort();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for start in keys {
        if seen.contains(start) {
            continue;
        }
        let mut comp: Vec<String> = Vec::new();
        let mut stack = vec![start.clone()];
        while let Some(x) = stack.pop() {
            if seen.contains(&x) {
                continue;
            }
            seen.insert(x.clone());
            comp.push(x.clone());
            for n in &adj[&x] {
                if !seen.contains(n) {
                    stack.push(n.clone());
                }
            }
        }
        if comp.len() < 2 {
            continue;
        }
        comp.sort();
        let comp_set: BTreeSet<&String> = comp.iter().collect();

        let group: Vec<&Rec> = comp.iter().map(|p| by_path[p.as_str()]).collect();
        let targets: Vec<Value> = group.iter().map(|r| target_json(r, backlinks)).collect();
        let any_prot: Vec<&&Rec> = group
            .iter()
            .filter(|r| prot.contains_key(r.path()))
            .collect();
        let lane = if any_prot.is_empty() {
            "standard"
        } else {
            "protected"
        };
        let prot_reason = {
            let mut set = BTreeSet::new();
            for r in &any_prot {
                for rsn in &prot[r.path()] {
                    set.insert(rsn.clone());
                }
            }
            set.into_iter().collect::<Vec<_>>().join("; ")
        };
        let repos: BTreeSet<&str> = group.iter().map(|r| r.repo()).collect();
        let cross = repos.len() > 1;

        // pair similarities among this component's members (sorted keys -> deterministic map).
        let mut pair_pairs: Vec<(&(String, String), &f64)> = sim
            .iter()
            .filter(|((a, b), _)| comp_set.contains(a) && comp_set.contains(b))
            .collect();
        pair_pairs.sort_by(|x, y| x.0.cmp(y.0));
        let mut pair_sims = serde_json::Map::new();
        let mut sim_vals: Vec<f64> = Vec::new();
        for ((a, b), s) in &pair_pairs {
            pair_sims.insert(format!("{a} ~ {b}"), json!(*s));
            sim_vals.push(**s);
        }
        let max_sim = sim_vals.iter().copied().fold(NEAR_DUP_THRESH, f64::max);
        let min_sim = sim_vals.iter().copied().fold(f64::INFINITY, f64::min);
        let cid = &sha256_hex(&comp.join("|"))[..12];

        let rationale = format!(
            "{} memories form a fuzzy near-duplicate cluster (Jaccard {:.2}-{:.2}){}",
            group.len(),
            min_sim,
            max_sim,
            if cross {
                let repos_py = format!(
                    "[{}]",
                    repos
                        .iter()
                        .map(|r| format!("'{r}'"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                format!("; spans repos {repos_py} -> keep-both/keep-in-sync, not merge")
            } else {
                "; within-repo -> human-review merge candidate".to_string()
            }
        );

        let proposed_change = if cross {
            json!({
                "op": "annotate",
                "diff": {
                    "flag": "cross-repo-twin-fuzzy",
                    "default_disposition": "keep-both / keep-in-sync",
                    "paths": targets.iter().map(|t| t["path"].clone()).collect::<Vec<_>>(),
                    "pair_similarities": Value::Object(pair_sims.clone()),
                },
                "reversible_via": "n/a (annotation only)",
            })
        } else {
            let survivor = pick_survivor(&group);
            let strands: Vec<Value> = targets
                .iter()
                .filter(|t| t["backlink_count"].as_u64().unwrap_or(0) > 0)
                .map(|t| t["path"].clone())
                .collect();
            json!({
                "op": "merge",
                "diff": {
                    "cluster_paths": targets.iter().map(|t| t["path"].clone()).collect::<Vec<_>>(),
                    "pair_similarities": Value::Object(pair_sims.clone()),
                    "survivor_path": survivor.path(),
                    "note": "NOT identical -- a human picks the canonical and reconciles the body diff; propose-only",
                    "strands_backlinks": strands,
                },
                "reversible_via": "restore_document",
            })
        };

        out.push(json!({
            "proposal_id": format!("{}{cid}", if cross { "dp-xtwin-fuzzy-" } else { "dp-neardup-" }),
            "kind": if cross { "cross_repo_twin" } else { "near_duplicate" },
            "lane": lane,
            "protected_reason": prot_reason,
            "confidence": round3(max_sim),
            "rationale": rationale,
            "targets": targets,
            "proposed_change": proposed_change,
            "status": "proposed",
        }));
    }
    out
}

/// Detect memories whose normalized bodies are byte-identical. Within-repo groups -> a merge proposal
/// (survivor = richest metadata then shortest path); cross-repo groups -> a keep-both cross-repo-twin
/// annotation plus, for a truly orphaned copy in a repo that HAS an index layer, a routed add-links
/// suggestion. Mirrors the Python `detect_exact_duplicates`.
fn detect_exact_duplicates(
    recs: &[Rec],
    prot: &HashMap<String, Vec<String>>,
    backlinks: &HashMap<(String, String), Vec<String>>,
) -> Vec<Value> {
    // Which repos have an index layer (an index-*.md to attach an orphan backlink to).
    let mut repo_has_index: HashMap<&str, bool> = HashMap::new();
    for r in recs {
        if r.slug.starts_with("index-") {
            repo_has_index.insert(r.repo(), true);
        }
    }

    // Group by normalized-body sha256, preserving first-seen order (deterministic proposal order).
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, r) in recs.iter().enumerate() {
        let h = sha256_hex(&norm_body(r.body()));
        groups.entry(h.clone()).or_insert_with(|| {
            order.push(h.clone());
            Vec::new()
        });
        groups.get_mut(&h).unwrap().push(idx);
    }

    let mut out = Vec::new();
    for h in &order {
        let idxs = &groups[h];
        if idxs.len() < 2 {
            continue;
        }
        let group: Vec<&Rec> = idxs.iter().map(|&i| &recs[i]).collect();
        let targets: Vec<Value> = group.iter().map(|r| target_json(r, backlinks)).collect();

        let any_prot: Vec<&&Rec> = group
            .iter()
            .filter(|r| prot.contains_key(r.path()))
            .collect();
        let lane = if any_prot.is_empty() {
            "standard"
        } else {
            "protected"
        };
        let prot_reason = {
            let mut set = BTreeSet::new();
            for r in &any_prot {
                for rsn in &prot[r.path()] {
                    set.insert(rsn.clone());
                }
            }
            set.into_iter().collect::<Vec<_>>().join("; ")
        };
        let repos: BTreeSet<&str> = group.iter().map(|r| r.repo()).collect();
        let hp = &h[..12];

        if repos.len() == 1 {
            // within-repo: merge-safe. survivor = max description length, then shortest path (stable).
            let survivor = pick_survivor(&group);
            let superseded: Vec<&&Rec> = group
                .iter()
                .filter(|r| r.path() != survivor.path())
                .collect();
            let stranded: Vec<&Value> = targets
                .iter()
                .filter(|t| {
                    t["path"].as_str() != Some(survivor.path())
                        && t["backlink_count"].as_u64().unwrap_or(0) > 0
                })
                .collect();
            let rationale = format!(
                "{} within-repo memories share a byte-identical normalized body (sha256 {}){}",
                group.len(),
                hp,
                if stranded.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; WARNING {} superseded copy has local backlinks (merge would strand them)",
                        stranded.len()
                    )
                }
            );
            let retained_provenance: Vec<Value> = superseded
                .iter()
                .map(|r| {
                    let p = r.provenance.clone().unwrap_or_default();
                    json!({
                        "path": r.path(),
                        "author": p.author.unwrap_or(Value::Null),
                        "timestamp": p.timestamp.unwrap_or(Value::Null),
                        "source": r.source,
                        "why": "exact-duplicate of survivor",
                    })
                })
                .collect();
            out.push(json!({
                "lane": lane,
                "protected_reason": prot_reason,
                "targets": targets,
                "status": "proposed",
                "proposal_id": format!("dp-exdup-{hp}"),
                "kind": "near_duplicate",
                "confidence": 1.0,
                "rationale": rationale,
                "proposed_change": {
                    "op": "merge",
                    "diff": {
                        "survivor_path": survivor.path(),
                        "superseded_paths": superseded.iter().map(|r| r.path()).collect::<Vec<_>>(),
                        "merged_body_diff": "(identical bodies: no body change; survivor retained, superseded archived)",
                        "retained_provenance": retained_provenance,
                        "strands_backlinks": stranded.iter().map(|t| t["path"].clone()).collect::<Vec<_>>(),
                    },
                    "reversible_via": "restore_document",
                },
            }));
        } else {
            // cross-repo twin: keep-both, annotate for drift; do not merge.
            let orphans: Vec<&Value> = targets
                .iter()
                .filter(|t| t["backlink_count"].as_u64().unwrap_or(0) == 0)
                .collect();
            // Render the repo list in Python list style (`['a', 'b']`) so the rationale matches the Python.
            let repos_py = format!(
                "[{}]",
                repos
                    .iter()
                    .map(|r| format!("'{r}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let rationale = format!(
                "byte-identical copies across repos {} (sha256 {}); cross-repo presence is usually intentional and [[links]] resolve within-repo only -- keep both, watch for drift{}",
                repos_py,
                hp,
                if orphans.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; NOTE {} copy has zero local backlinks (possible orphan)",
                        orphans.len()
                    )
                }
            );
            out.push(json!({
                "lane": lane,
                "protected_reason": prot_reason,
                "targets": targets,
                "status": "proposed",
                "proposal_id": format!("dp-xtwin-{hp}"),
                "kind": "cross_repo_twin",
                "confidence": 0.9,
                "rationale": rationale,
                "proposed_change": {
                    "op": "annotate",
                    "diff": {
                        "flag": "cross-repo-twin",
                        "default_disposition": "keep-both / keep-in-sync",
                        "paths": targets.iter().map(|t| t["path"].clone()).collect::<Vec<_>>(),
                        "possible_orphans": orphans.iter().map(|t| t["path"].clone()).collect::<Vec<_>>(),
                    },
                    "reversible_via": "n/a (annotation only)",
                },
            }));
            // An orphaned cross-repo-twin copy is fixed in-place with add_links in its OWN repo, routed to
            // that repo's curation owner — only when the repo HAS an index layer to attach to.
            for o in &orphans {
                let o_repo = o["repo"].as_str().unwrap_or("");
                let o_path = o["path"].as_str().unwrap_or("");
                if !repo_has_index.get(o_repo).copied().unwrap_or(false) {
                    continue; // flat store: backlink-less is the expected resting state, not an orphan
                }
                let oprot = prot.get(o_path);
                out.push(json!({
                    "proposal_id": format!("dp-orphan-{}", &sha256_hex(o_path)[..12]),
                    "kind": "cross_link",
                    "lane": if oprot.is_some() { "protected" } else { "standard" },
                    "protected_reason": oprot.map(|r| r.join("; ")).unwrap_or_default(),
                    "confidence": 0.6,
                    "rationale": format!(
                        "cross-repo twin copy {o_path} has zero local backlinks in repo {o_repo} -- fix the orphan in-place by backlinking it from {o_repo}'s index/relevant log (domain home stays as-is; not a canonical flip)"
                    ),
                    "targets": [ (*o).clone() ],
                    "proposed_change": {
                        "op": "add_links",
                        "diff": {
                            "orphan_path": o_path,
                            "add_backlink_from": format!("{o_repo} index or relevant log (specific file = domain-curation call)"),
                            "route_to": format!("{o_repo} curation owner, else librarian"),
                        },
                        "reversible_via": "version-history",
                    },
                    "status": "proposed",
                }));
            }
        }
    }
    out
}

/// Slugs the librarian's forward-ref tracker already catalogues — suppressed from the write-later advisory.
/// Collects the tracker memory's `[[links]]` plus long kebab tokens (3+ segments) mentioned in its prose.
fn load_forward_ref_catalogue(recs: &[Rec]) -> BTreeSet<String> {
    let mut cat = BTreeSet::new();
    for r in recs {
        if r.slug == FORWARD_REF_TRACKER {
            let body = r.body();
            for l in find_links(body) {
                cat.insert(l.trim().to_string());
            }
            for tok in kebab_tokens(body) {
                cat.insert(tok);
            }
        }
    }
    cat
}

/// Long kebab tokens in `text` — the Python `\b[a-z0-9]+(?:-[a-z0-9]+){2,}\b` (3+ hyphen-joined lowercase
/// segments, bounded by non-word chars). Used to catch dangling slugs the tracker lists in prose. Pure.
fn kebab_tokens(text: &str) -> BTreeSet<String> {
    fn is_word(c: u8) -> bool {
        c.is_ascii_alphanumeric() || c == b'_'
    }
    let b = text.as_bytes();
    let mut out = BTreeSet::new();
    let mut i = 0;
    while i < b.len() {
        // A token must start at a word boundary with a lowercase/digit char.
        let at_boundary = i == 0 || !is_word(b[i - 1]);
        if at_boundary && (b[i].is_ascii_lowercase() || b[i].is_ascii_digit()) {
            let start = i;
            let mut j = i;
            while j < b.len()
                && (b[j].is_ascii_lowercase() || b[j].is_ascii_digit() || b[j] == b'-')
            {
                j += 1;
            }
            // Right boundary: the char after the run must be a non-word char (or end). If it is a word char
            // (e.g. uppercase/underscore), this is not a clean \b match — skip past the whole run.
            let right_ok = j >= b.len() || !is_word(b[j]);
            let run = &text[start..j];
            if right_ok {
                // Trim trailing hyphens, then require 3+ non-empty single-hyphen segments (2+ hyphens).
                let trimmed = run.trim_matches('-');
                let segs: Vec<&str> = trimmed.split('-').collect();
                if segs.len() >= 3 && segs.iter().all(|s| !s.is_empty()) {
                    out.insert(trimmed.to_string());
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

/// Detect canon/index memories that link to slugs with no file yet — a gentle, low-confidence write-later
/// ADVISORY (not a fix queue): in this store a dangling `[[link]]` is usually a deliberate forward-ref.
/// Suppresses self/resolved/non-slug/`MEMORY`/tracker-catalogued links and short placeholders. Mirrors the
/// Python `detect_write_later_candidates`.
fn detect_write_later_candidates(
    recs: &[Rec],
    prot: &HashMap<String, Vec<String>>,
    backlinks: &HashMap<(String, String), Vec<String>>,
    catalogued: &BTreeSet<String>,
) -> Vec<Value> {
    let mut slugs_by_repo: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    for r in recs {
        slugs_by_repo
            .entry(r.repo())
            .or_default()
            .insert(r.slug.as_str());
    }
    let mut out = Vec::new();
    for r in recs {
        let Some(reasons) = prot.get(r.path()) else {
            continue; // canon/index only
        };
        let repo = r.repo();
        let links: BTreeSet<String> = find_links(r.body())
            .iter()
            .map(|l| l.trim().to_string())
            .collect();
        let mut candidates: Vec<String> = links
            .iter()
            .filter(|l| {
                !l.is_empty()
                    && l.as_str() != r.slug
                    && !slugs_by_repo
                        .get(repo)
                        .is_some_and(|s| s.contains(l.as_str()))
                    && is_slug(l)
                    && l.as_str() != "MEMORY"
                    && !catalogued.contains(*l)
                    && l.matches('-').count() >= 2
                    && l.chars().count() >= 12
            })
            .cloned()
            .collect();
        candidates.sort();
        if candidates.is_empty() {
            continue;
        }
        out.push(json!({
            "proposal_id": format!("dp-writelater-{}", &sha256_hex(r.path())[..12]),
            "kind": "write_later_candidate",
            "lane": "protected",
            "protected_reason": reasons.join("; "),
            "confidence": 0.2,
            "rationale": format!(
                "canon memory {} links to {} slug(s) with no file yet -- likely intentional 'write-later' markers; write them or leave as markers (NOT a defect)",
                r.path(),
                candidates.len()
            ),
            "targets": [ target_json(r, backlinks) ],
            "proposed_change": {
                "op": "annotate",
                "diff": {
                    "index_path": r.path(),
                    "write_later_slugs": candidates,
                    "note": "advisory: these [[links]] have no target memory yet; librarian may write or keep as markers",
                },
                "reversible_via": "n/a (annotation only)",
            },
            "status": "proposed",
        }));
    }
    out
}

/// Source/doc file extensions a path-shaped token must end in to be a staleness candidate (task_1141).
/// Restricting to real source/doc extensions is a first-cut false-positive guard: a bare word or a prose
/// ratio like `and/or` has no such extension and is never a candidate.
const STALE_SOURCE_EXTS: &[&str] = &[
    "rs", "toml", "md", "nix", "py", "lean", "cdz", "sh", "json", "yaml", "yml", "lock", "txt",
    "rb", "go", "ts", "tsx", "js", "jsx", "c", "h", "cc", "cpp", "hpp", "wit", "proto", "sql",
    "cfg", "ron", "wat", "wast", "ll",
];

/// Drop fenced code blocks (``` or ~~~ delimited) from `body`: a path appearing only inside an illustrative
/// snippet is not a live reference, so it is never flagged (the librarian's caution, comment on task_1141).
/// Inline `backtick` spans are KEPT -- a path in backticks is the normal way a memory cites a real file;
/// `extract_file_path_refs` trims the backtick chars off the token instead. Pure.
fn strip_fenced_code(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut in_fence = false;
    for line in body.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue; // drop the fence delimiter line itself
        }
        if !in_fence {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Extract repo-relative file-path references from `body` (the caller strips fenced code first). A candidate
/// must: contain a `/`; after trimming surrounding markdown punctuation (backticks, quotes, brackets, parens,
/// trailing sentence punctuation) and a trailing `:line[:col]` locator, end in a known source/doc extension;
/// not be a URL (`://` or an `http` lead); not be absolute or an explicit relative climb (`/`, `~`, `./`,
/// `../` -- a host path, not a repo-root-relative artifact); and contain only plausible path chars. A bare
/// filename with no `/` (e.g. `MEMORY.md`) is NOT a candidate -- prose names a filename far more often than it
/// names a real missing path. Pure.
fn extract_file_path_refs(body: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for raw in body.split(char::is_whitespace) {
        let mut t = raw.trim_matches(|c| {
            matches!(
                c,
                '`' | '"'
                    | '\''
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '<'
                    | '>'
                    | ','
                    | ';'
                    | '!'
                    | '?'
                    | '*'
                    | '|'
            )
        });
        // Strip a trailing `:123` or `:123:45` line/col locator (the clickable file:line[:col] form).
        for _ in 0..2 {
            if let Some(colon) = t.rfind(':') {
                let tail = &t[colon + 1..];
                if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
                    t = &t[..colon];
                    continue;
                }
            }
            break;
        }
        let t = t.trim_end_matches(['.', ',', ';', ':']);
        if t.len() < 3 || !t.contains('/') {
            continue;
        }
        if t.contains("://") || t.starts_with("http") {
            continue;
        }
        if t.starts_with('/') || t.starts_with('~') || t.starts_with("./") || t.starts_with("../") {
            continue;
        }
        if !t
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_'))
        {
            continue;
        }
        let ext = t.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        if ext == t || !STALE_SOURCE_EXTS.contains(&ext.as_str()) {
            continue;
        }
        out.insert(t.to_string());
    }
    out
}

/// Return the subset of `paths` that the repo's ignore rules match, via one `git -C <root> check-ignore
/// --stdin` (so a gitignored, regenerable path -- e.g. cadenza's `implementation/` tree -- is suppressed, not
/// flagged as stale). `Err` on any git failure (root is not a git repo, git missing) so the caller can skip
/// the detector rather than risk flagging regenerable paths. `check-ignore` exits 0 when >=1 path is ignored,
/// 1 when none are, 128 on error.
fn git_check_ignore(repo_root: &Path, paths: &[String]) -> Result<BTreeSet<String>, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    if paths.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["check-ignore", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("git check-ignore spawn failed: {e}"))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or("git check-ignore: no stdin handle")?;
        stdin
            .write_all(paths.join("\n").as_bytes())
            .map_err(|e| format!("git check-ignore write: {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("git check-ignore wait: {e}"))?;
    match out.status.code() {
        Some(0) | Some(1) => Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()),
        other => Err(format!(
            "git check-ignore exited {other:?} (is {} a git repo?)",
            repo_root.display()
        )),
    }
}

/// Detect verified-dangling FILE-PATH references (task_1141, doc_102 A4): a memory body cites a repo-relative
/// source path that does NOT exist on disk under `repo_root` AND is not gitignored. PROPOSE-ONLY, confidence
/// 0.5 -- verified-absent is high-signal but prose extraction can misread, so a human disposes. Three
/// false-positive guards (doc_102 A4 + the librarian's caution): fenced code blocks are stripped (illustrative
/// snippets), a bare filename with no `/` is never a candidate, and a gitignored/regenerable path (cadenza's
/// `implementation/` tree) is suppressed. If the gitignore check cannot run (root is not a git repo), the
/// detector emits NOTHING rather than risk flagging regenerable paths. The caller runs this ONLY when
/// `--repo-root` is given -- with no worktree there is nothing to resolve against, so it never runs on a
/// corpus-only pass.
fn detect_stale_refs(
    recs: &[Rec],
    prot: &HashMap<String, Vec<String>>,
    backlinks: &HashMap<(String, String), Vec<String>>,
    repo_root: &Path,
) -> Vec<Value> {
    let mut missing_by_rec: Vec<(usize, Vec<String>)> = Vec::new();
    let mut all_missing: BTreeSet<String> = BTreeSet::new();
    for (i, r) in recs.iter().enumerate() {
        let stripped = strip_fenced_code(r.body());
        let mut missing: Vec<String> = extract_file_path_refs(&stripped)
            .into_iter()
            .filter(|p| !repo_root.join(p).exists())
            .collect();
        missing.sort();
        if !missing.is_empty() {
            all_missing.extend(missing.iter().cloned());
            missing_by_rec.push((i, missing));
        }
    }
    if missing_by_rec.is_empty() {
        return Vec::new();
    }
    let all_missing_vec: Vec<String> = all_missing.into_iter().collect();
    let ignored = match git_check_ignore(repo_root, &all_missing_vec) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("dream-analyze: staleness detector skipped -- {e}");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for (i, missing) in missing_by_rec {
        let flagged: Vec<String> = missing
            .into_iter()
            .filter(|p| !ignored.contains(p))
            .collect();
        if flagged.is_empty() {
            continue;
        }
        let r = &recs[i];
        let lane = if prot.contains_key(r.path()) {
            "protected"
        } else {
            "standard"
        };
        let prot_reason = prot.get(r.path()).map(|v| v.join("; ")).unwrap_or_default();
        out.push(json!({
            "proposal_id": format!("dp-staleref-{}", &sha256_hex(r.path())[..12]),
            "kind": "stale_file_ref",
            "lane": lane,
            "protected_reason": prot_reason,
            "confidence": 0.5,
            "rationale": format!(
                "memory {} cites {} file path(s) absent from the repo worktree and matching no .gitignore rule -- a recalled memory that names a file should be verified before it is recommended; update or remove the ref (verified-dangling, NOT auto-applied)",
                r.path(),
                flagged.len()
            ),
            "targets": [ target_json(r, backlinks) ],
            "proposed_change": {
                "op": "annotate",
                "diff": {
                    "memory_path": r.path(),
                    "dangling_file_refs": flagged,
                    "checked_under": repo_root.display().to_string(),
                    "note": "advisory: these path refs resolve to no file in the repo worktree and match no .gitignore rule; update the memory or confirm the path",
                },
                "reversible_via": "n/a (annotation only)",
            },
            "status": "proposed",
        }));
    }
    out
}

/// Concurrency window for the value-contradiction detector: two differing-value memories are a contradiction
/// candidate only when their provenance timestamps are within this gap. A wider gap reads as temporal
/// evolution (a budget/threshold that legitimately changed -- the rcdzc test-size limit, the MEMORY.md byte
/// caps), which belongs to the supersede/staleness lane, NOT the contradiction surface. The librarian asked
/// for a STRICT gate; 7 days is deliberately tight (tunable).
const CONTRADICTION_WINDOW_SECS: i64 = 7 * 24 * 60 * 60;

/// A precise value contradiction is among a FEW memories about ONE specific subject. A (unit, subject-token)
/// bucket shared by more than this many distinct memories is a common identifier, not a distinctive subject,
/// so it is skipped: this sharpens precision AND bounds the pairwise work. Without it a hot bucket on a large
/// corpus produces O(k^2) pairs -- the cadenza-scale ~958MB report that 502'd its publish (task_1123 co-verify).
const MAX_VALUE_CONTRADICTION_BUCKET: usize = 6;

/// Hard per-scope cap on emitted value-contradiction proposals -- a safety bound so no scope yields a
/// pathological report even if many small buckets contradict. 50 is a generous single review batch.
const MAX_VALUE_CONTRADICTIONS: usize = 50;

/// Parse a provenance timestamp `Value` to unix seconds: an RFC3339 string, or a number read as epoch
/// seconds. `None` when absent or unparseable -- the value-contradiction detector treats `None` as "cannot
/// prove concurrency" and suppresses, so a missing timestamp never produces a flag.
pub(crate) fn provenance_unix_secs(ts: Option<&Value>) -> Option<i64> {
    match ts? {
        Value::String(s) => {
            time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                .ok()
                .map(|t| t.unix_timestamp())
        }
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
}

/// Extract `(normalized_unit, value)` quantities from `text`: a numeric literal (digits, optional single
/// decimal point) immediately followed -- no space, or exactly one space -- by a unit token (1-6 ASCII
/// letters, or a single `%`). The unit is lowercased. A bare number with no unit is ignored (too ambiguous to
/// key a contradiction on), so `task_827`, `comment_3869`, dates, and version strings never produce a
/// quantity. Pure.
fn extract_quantities(text: &str) -> Vec<(String, f64)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let number_start =
            b[i].is_ascii_digit() && (i == 0 || !(b[i - 1].is_ascii_digit() || b[i - 1] == b'.'));
        if !number_start {
            i += 1;
            continue;
        }
        let nstart = i;
        let mut j = i;
        let mut seen_dot = false;
        while j < b.len()
            && (b[j].is_ascii_digit()
                || (b[j] == b'.' && !seen_dot && j + 1 < b.len() && b[j + 1].is_ascii_digit()))
        {
            if b[j] == b'.' {
                seen_dot = true;
            }
            j += 1;
        }
        let num_str = &text[nstart..j];
        let mut k = j;
        if k < b.len() && b[k] == b' ' {
            k += 1;
        }
        let ustart = k;
        if k < b.len() && b[k] == b'%' {
            if let Ok(v) = num_str.parse::<f64>() {
                out.push(("%".to_string(), v));
            }
            i = k + 1;
            continue;
        }
        while k < b.len() && b[k].is_ascii_alphabetic() {
            k += 1;
        }
        if k > ustart {
            let unit = text[ustart..k].to_ascii_lowercase();
            if (1..=6).contains(&unit.len())
                && let Ok(v) = num_str.parse::<f64>()
            {
                out.push((unit, v));
            }
        }
        i = k.max(j);
    }
    out
}

/// Whether a `kebab_tokens` token is usable as a value-contradiction SUBJECT: it must contain at least one
/// ASCII-alphabetic character, so a date (`2026-09-26`) or a pure-number token is never treated as a subject
/// (the librarian's date-keying false-positive finding). Pure.
fn is_subject_token(tok: &str) -> bool {
    tok.bytes().any(|b| b.is_ascii_alphabetic())
}

/// Detect VALUE contradictions (task_1141, librarian-confirmed scope B): two CONCURRENT memories that assert a
/// different value for the same keyed quantity (same unit) on a shared subject. PROPOSE-ONLY, confidence 0.3,
/// FYI. The precision gates, in order: (1) subject -- the two memories must share a distinctive multi-segment
/// `kebab_tokens` identifier that contains a letter (`is_subject_token`, so a date like `2026-09-26` or a
/// pure-number token is never a subject), AND the quantity must co-occur with that token on the SAME LINE, so
/// the value is textually ABOUT the subject rather than a coincidental unit elsewhere in the body (the
/// metric-conflation fix); (2) same unit, different value; (3) STRICT concurrency -- both provenance
/// timestamps present and within `CONTRADICTION_WINDOW_SECS` (a wider age gap is temporal evolution -- a
/// budget that changed -- suppressed, the supersede/staleness lane's concern; a missing timestamp suppresses).
/// VOLUME is bounded by `MAX_VALUE_CONTRADICTION_BUCKET` (skip an over-shared, non-distinctive bucket) and a
/// per-scope `MAX_VALUE_CONTRADICTIONS` cap. No LLM: a low-precision candidate surfacer for human review, never
/// an adjudication.
fn detect_value_contradictions(
    recs: &[Rec],
    prot: &HashMap<String, Vec<String>>,
    backlinks: &HashMap<(String, String), Vec<String>>,
) -> Vec<Value> {
    // Index entries under a composite (unit, subject-token) key: only memories sharing BOTH a unit and a
    // distinctive subject token are ever compared, which bounds the work and is the precision gate.
    struct Entry {
        idx: usize,
        value: f64,
        ts: Option<i64>,
    }
    let mut by_key: HashMap<(String, String), Vec<Entry>> = HashMap::new();
    for (idx, r) in recs.iter().enumerate() {
        let body = strip_fenced_code(r.body());
        let ts = provenance_unix_secs(r.provenance.as_ref().and_then(|p| p.timestamp.as_ref()));
        // Key (unit, subject-token) -> values this memory states for that metric, with value<->subject
        // co-location gated to the SAME LINE: a quantity counts for a subject token only when the number and
        // the token appear on one line, so the value is textually ABOUT that subject rather than a
        // coincidental unit elsewhere in the body (the metric-conflation fix). A subject token must contain a
        // letter (is_subject_token), so a date like 2026-09-26 or a pure-number token is never a subject.
        let mut keyed: HashMap<(String, String), BTreeSet<u64>> = HashMap::new();
        for line in body.lines() {
            let toks: Vec<String> = kebab_tokens(line)
                .into_iter()
                .filter(|t| is_subject_token(t))
                .collect();
            if toks.is_empty() {
                continue;
            }
            let quants = extract_quantities(line);
            if quants.is_empty() {
                continue;
            }
            for (unit, value) in &quants {
                for tok in &toks {
                    keyed
                        .entry((unit.clone(), tok.clone()))
                        .or_default()
                        .insert(value.to_bits());
                }
            }
        }
        for ((unit, tok), vals) in keyed {
            // The memory itself states two different values for this (unit, subject) -- ambiguous, skip it.
            if vals.len() != 1 {
                continue;
            }
            let value = f64::from_bits(*vals.iter().next().unwrap());
            by_key
                .entry((unit, tok))
                .or_default()
                .push(Entry { idx, value, ts });
        }
    }

    // Emit one proposal per contradicting memory pair (deduped across keys by sorted path pair).
    let mut seen_pairs: BTreeSet<(String, String)> = BTreeSet::new();
    let mut out = Vec::new();
    let mut keys: Vec<&(String, String)> = by_key.keys().collect();
    keys.sort();
    for key in keys {
        if out.len() >= MAX_VALUE_CONTRADICTIONS {
            eprintln!(
                "value_contradiction: hit the per-scope cap ({MAX_VALUE_CONTRADICTIONS}); remaining buckets unexamined"
            );
            break;
        }
        let entries = &by_key[key];
        // Skip a bucket with nothing to pair, OR one shared by too many memories: a (unit, token) in more than
        // MAX_VALUE_CONTRADICTION_BUCKET distinct memories is a common identifier, not a distinctive subject,
        // and pairing it is O(k^2) noise -- this guards the cadenza-scale combinatorial blowup.
        if entries.len() < 2 || entries.len() > MAX_VALUE_CONTRADICTION_BUCKET {
            continue;
        }
        let (unit, token) = key;
        for a in 0..entries.len() {
            for b in (a + 1)..entries.len() {
                let (ea, eb) = (&entries[a], &entries[b]);
                if ea.idx == eb.idx || (ea.value - eb.value).abs() < f64::EPSILON {
                    continue;
                }
                // STRICT concurrency gate: both timestamps present and within the window.
                let (Some(ta), Some(tb)) = (ea.ts, eb.ts) else {
                    continue;
                };
                if (ta - tb).abs() > CONTRADICTION_WINDOW_SECS {
                    continue; // temporal evolution, not a contradiction
                }
                let (ra, rb) = (&recs[ea.idx], &recs[eb.idx]);
                let (pa, pb) = (ra.path().to_string(), rb.path().to_string());
                let pair = if pa <= pb {
                    (pa.clone(), pb.clone())
                } else {
                    (pb.clone(), pa.clone())
                };
                if !seen_pairs.insert(pair.clone()) {
                    continue;
                }
                let (first, second) = (&pair.0, &pair.1);
                let (first_rec, second_rec, first_val, second_val) = if first == &pa {
                    (ra, rb, ea.value, eb.value)
                } else {
                    (rb, ra, eb.value, ea.value)
                };
                let mut reasons = BTreeSet::new();
                for p in [first.as_str(), second.as_str()] {
                    if let Some(rs) = prot.get(p) {
                        for r in rs {
                            reasons.insert(r.clone());
                        }
                    }
                }
                let lane = if reasons.is_empty() {
                    "standard"
                } else {
                    "protected"
                };
                out.push(json!({
                    "proposal_id": format!("dp-valueconflict-{}", &sha256_hex(&format!("{first}|{second}|{unit}|{token}"))[..12]),
                    "kind": "value_contradiction",
                    "lane": lane,
                    "protected_reason": reasons.into_iter().collect::<Vec<_>>().join("; "),
                    "confidence": 0.3,
                    "rationale": format!(
                        "two concurrent memories assert different '{unit}' values on a shared subject ('{token}'): {first} says {first_val}, {second} says {second_val} -- possible contradiction, verify (timestamps within {}d, so NOT a temporal change; reconcile or confirm both are current)",
                        CONTRADICTION_WINDOW_SECS / 86_400
                    ),
                    "targets": [ target_json(first_rec, backlinks), target_json(second_rec, backlinks) ],
                    "proposed_change": {
                        "op": "annotate",
                        "diff": {
                            "memory_a": first,
                            "value_a": first_val,
                            "memory_b": second,
                            "value_b": second_val,
                            "unit": unit,
                            "shared_subject": token,
                            "note": "advisory: concurrent memories disagree on a keyed value; reconcile or confirm both are current (NOT auto-applied)",
                        },
                        "reversible_via": "n/a (annotation only)",
                    },
                    "status": "proposed",
                }));
            }
        }
    }
    out
}

/// Load the JSONL corpus (one memory record per line). An empty line is skipped; a malformed line is an
/// error (naming the line number) so a corrupt corpus never silently drops memories.
fn load_corpus(path: &Path) -> Result<Vec<Rec>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read corpus {}: {e}", path.display()))?;
    let mut recs = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut rec: Rec =
            serde_json::from_str(line).map_err(|e| format!("corpus line {}: {e}", n + 1))?;
        if rec.path.is_none() {
            rec.path = Some(format!("repos/{}/{}", rec.repo(), rec.slug));
        }
        recs.push(rec);
    }
    Ok(recs)
}

/// A browser-like User-Agent for board GETs — the public CF edge 403s a non-browser UA; harmless on loopback.
const BOARD_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) fleet-dream";

/// Percent-encode a wiki path prefix (the board `/wiki?prefix=` query): keep RFC3986 unreserved bytes, encode
/// everything else (notably `/`). Mirrors the shim's `jq @uri`. Pure.
fn encode_prefix(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// One board REST GET returning parsed JSON (transient 5xx are the caller's concern; a background pass
/// tolerates a slow board). `path` begins with `/` and is appended to `board_api`.
fn board_get_json(board_api: &str, path: &str) -> Result<Value, String> {
    let url = format!("{}{}", board_api.trim_end_matches('/'), path);
    let resp = ureq::get(&url)
        .set("accept", "application/json")
        .set("user-agent", BOARD_UA)
        .call()
        .map_err(|e| format!("board GET {path} failed: {e}"))?;
    let raw = resp
        .into_string()
        .map_err(|e| format!("board GET {path} read failed: {e}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("board GET {path}: not JSON: {e}"))
}

/// Build the analyzer corpus from the LIVE board for one scope (a wiki path prefix, e.g. `repos/<repo>` or
/// `agents/<agent>`): list the scope's memory docs via `/wiki?prefix=`, then fetch each doc's body + metadata
/// via `/documents/<id>?include_body=true`, assembling the analyzer record. Replaces the --corpus JSONL for
/// scheduled runs so dreaming reflects current board state. Within-repo link semantics are preserved by
/// keying `repo` to the scope's namespace segment (the second path segment).
fn load_corpus_from_board(board_api: &str, scope: &str) -> Result<Vec<Rec>, String> {
    let index = board_get_json(board_api, &format!("/wiki?prefix={}", encode_prefix(scope)))?;
    let items = index
        .as_array()
        .ok_or_else(|| format!("board /wiki?prefix={scope}: expected an array, got {index}"))?;
    let mut recs = Vec::with_capacity(items.len());
    for it in items {
        let id = it.get("id").ok_or("wiki entry missing id")?;
        let id_str = match id {
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            other => {
                return Err(format!(
                    "wiki entry id is neither number nor string: {other}"
                ));
            }
        };
        let doc = board_get_json(board_api, &format!("/documents/{id_str}?include_body=true"))?;
        let path = doc
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if path.is_empty() {
            continue; // a doc with no filed path is not a memory in a scope
        }
        let slug = path.rsplit('/').next().unwrap_or(&path).to_string();
        // namespace = the second path segment (repos/<X>/.. or agents/<X>/..), so [[links]] resolve within it.
        let namespace = path.split('/').nth(1).unwrap_or("?").to_string();
        let md = doc.get("metadata").cloned().unwrap_or(Value::Null);
        let provenance: Option<Provenance> = md
            .get("provenance")
            .cloned()
            .and_then(|p| serde_json::from_value(p).ok());
        recs.push(Rec {
            slug,
            repo: Some(namespace),
            name: doc.get("title").and_then(Value::as_str).map(str::to_string),
            description: md
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            rtype: md.get("type").and_then(Value::as_str).map(str::to_string),
            body: doc.get("body").and_then(Value::as_str).map(str::to_string),
            source: md
                .get("provenance")
                .and_then(|p| p.get("source"))
                .and_then(Value::as_str)
                .map(str::to_string),
            doc_id: Some(id.clone()),
            provenance,
            path: Some(path),
        });
    }
    Ok(recs)
}

/// Normalize text to the board's ASCII-only content rule (the board 400s on any non-ASCII char): apply the
/// board's suggested substitutions (em/en dash, curly quotes, ellipsis, arrows) then drop any other
/// non-ASCII. Keeps a published dream doc from being rejected for a stray Unicode char in a memory rationale.
fn to_ascii(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\u{2014}' | '\u{2013}' => out.push('-'),
            '\u{2018}' | '\u{2019}' => out.push('\''),
            '\u{201C}' | '\u{201D}' => out.push('"'),
            '\u{2026}' => out.push_str("..."),
            '\u{2192}' => out.push_str("->"),
            '\u{2190}' => out.push_str("<-"),
            c if c.is_ascii() => out.push(c),
            _ => {}
        }
    }
    out
}

/// A stable fingerprint of a proposal's SUBSTANCE (its `proposed_change`): the dedup key for "materially
/// changed". The proposal_id is already content-derived for most kinds, but a write-later advisory keeps its
/// id (hashed from the memory path) while its slug list can shift -- the fingerprint catches that. Pure.
fn proposal_fingerprint(p: &Value) -> String {
    let pc = p.get("proposed_change").cloned().unwrap_or(Value::Null);
    let s = serde_json::to_string(&pc).unwrap_or_default();
    sha256_hex(&s)[..16].to_string()
}

/// Parse the machine-tracked `dream-state` block from a prior published doc body: a map
/// `proposal_id -> (fingerprint, disposition)`. Absent/garbled block => empty (treated as a first run). Pure.
fn parse_dream_state(body: &str) -> HashMap<String, (String, String)> {
    let mut m = HashMap::new();
    let Some(start) = body.find("<!-- dream-state") else {
        return m;
    };
    let after = &body[start..];
    let Some(nl) = after.find('\n') else { return m };
    let rest = &after[nl + 1..];
    let Some(end) = rest.find("-->") else {
        return m;
    };
    let json_str = rest[..end].trim();
    if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(json_str) {
        for (id, v) in o {
            let h = v
                .get("hash")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let d = v
                .get("disposition")
                .and_then(Value::as_str)
                .unwrap_or("new")
                .to_string();
            m.insert(id, (h, d));
        }
    }
    m
}

/// Render the review-surface markdown for a dream-report (sectioned, each proposal with id / confidence /
/// evidence / carried disposition) plus the trailing machine-tracked `dream-state` block. `state` is the
/// per-proposal `(id, fingerprint, disposition)` to record. Pure.
fn render_dream_doc(scope: &str, report: &Value, state: &[(String, String, String)]) -> String {
    let disp: HashMap<&str, &str> = state
        .iter()
        .map(|(id, _, d)| (id.as_str(), d.as_str()))
        .collect();
    let empty = Vec::new();
    let proposals = report
        .get("proposals")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut out = String::new();
    out.push_str(&format!("# dream: {scope}\n\n"));
    out.push_str(&format!(
        "Propose-only dream-report (v-agent-memory, automatic pass). Apply via the human-gated dream-apply lane; nothing here is auto-applied. Corpus {} memories, {} proposals.\n",
        report.get("corpus_size").and_then(Value::as_u64).unwrap_or(0),
        proposals.len()
    ));
    for (sec, title) in [
        ("actionable", "## Actionable"),
        ("cross_repo_twin", "## Cross-repo twins"),
        ("fyi", "## FYI (write-later advisories)"),
    ] {
        let in_sec: Vec<&Value> = proposals
            .iter()
            .filter(|p| p.get("section").and_then(Value::as_str) == Some(sec))
            .collect();
        if in_sec.is_empty() {
            continue;
        }
        out.push_str(&format!("\n{title}\n"));
        for p in in_sec {
            let id = p.get("proposal_id").and_then(Value::as_str).unwrap_or("?");
            let kind = p.get("kind").and_then(Value::as_str).unwrap_or("?");
            let conf = p.get("confidence").and_then(Value::as_f64).unwrap_or(0.0);
            let d = disp.get(id).copied().unwrap_or("new");
            out.push_str(&format!(
                "\n### {id}  ({kind}, confidence {conf})  [disposition: {d}]\n"
            ));
            if let Some(r) = p.get("rationale").and_then(Value::as_str) {
                out.push_str(r);
                out.push('\n');
            }
            let paths: Vec<&str> = p
                .get("targets")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t.get("path").and_then(Value::as_str))
                        .collect()
                })
                .unwrap_or_default();
            if !paths.is_empty() {
                out.push_str(&format!("Targets: {}\n", paths.join(", ")));
            }
        }
    }
    // Machine-tracked state block: set a disposition to accepted|declined|deferred to stop it re-notifying.
    let state_obj: serde_json::Map<String, Value> = state
        .iter()
        .map(|(id, h, d)| (id.clone(), json!({ "hash": h, "disposition": d })))
        .collect();
    let state_json =
        serde_json::to_string(&Value::Object(state_obj)).unwrap_or_else(|_| "{}".into());
    out.push_str("\n<!-- dream-state v1 (machine-tracked; set a disposition to accepted|declined|deferred to stop a proposal re-notifying)\n");
    out.push_str(&state_json);
    out.push_str("\n-->\n");
    out
}

/// Run the `board-memory` shim with `args` and an optional stdin body.
fn run_board_memory(
    bin: &str,
    args: &[&str],
    stdin: Option<&str>,
) -> std::io::Result<std::process::Output> {
    use std::io::Write;
    let mut cmd = std::process::Command::new(bin);
    cmd.args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        });
    let mut child = cmd.spawn()?;
    if let Some(body) = stdin {
        child
            .stdin
            .take()
            .expect("stdin piped")
            .write_all(body.as_bytes())?;
    }
    child.wait_with_output()
}

/// Publish the dream-report to the versioned board doc `dreams/<scope>` (via the board-memory shim),
/// carrying the librarian's prior dispositions forward by stable id+fingerprint so a declined/deferred
/// proposal never re-notifies. Returns `(new_count, new_ids)` -- the notify-on-new signal. A proposal is NEW
/// when its id is unseen OR its fingerprint changed (materially different). Mutating the live board, so it
/// runs only under `--publish-board`.
fn publish_board(scope: &str, report: &Value, bin: &str) -> Result<(usize, Vec<String>), String> {
    let doc_path = format!("dreams/{scope}");
    // Read the prior doc (if any) to recover dispositions. A missing doc (exit 4) => empty prior.
    let prior_body = match run_board_memory(bin, &["get", "--path", &doc_path], None) {
        Ok(o) if o.status.success() => {
            let v: Value = serde_json::from_slice(&o.stdout).unwrap_or(Value::Null);
            v.get("body")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        }
        Ok(_) => String::new(), // no prior doc
        Err(e) => return Err(format!("board-memory get failed to spawn: {e}")),
    };
    let prior = parse_dream_state(&prior_body);

    let empty = Vec::new();
    let proposals = report
        .get("proposals")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut state: Vec<(String, String, String)> = Vec::with_capacity(proposals.len());
    let mut new_ids: Vec<String> = Vec::new();
    for p in proposals {
        let id = p
            .get("proposal_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let fp = proposal_fingerprint(p);
        match prior.get(&id) {
            Some((prior_fp, disp)) if *prior_fp == fp => state.push((id, fp, disp.clone())),
            _ => {
                new_ids.push(id.clone());
                state.push((id, fp, "new".to_string()));
            }
        }
    }

    let body = to_ascii(&render_dream_doc(scope, report, &state));
    let name = format!("dream: {scope}");
    let args = [
        "write",
        "--path",
        &doc_path,
        "--name",
        &name,
        "--type",
        "reference",
        "--tags",
        "dream-report",
        "--author",
        "v-agent-memory",
    ];
    match run_board_memory(bin, &args, Some(&body)) {
        Ok(o) if o.status.success() => Ok((new_ids.len(), new_ids)),
        Ok(o) => Err(format!(
            "board-memory write failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => Err(format!("board-memory write failed to spawn: {e}")),
    }
}

/// Section + rank the proposals for the librarian's review surface: tag each with a `section` -- `actionable`
/// (within-repo merges + orphan add-links), `cross_repo_twin` (the distinct cross-cutting twins), or `fyi`
/// (the low-confidence write-later advisories) -- then order by section (actionable, then twins, then FYI),
/// confidence descending, then id for stability. Mutates each proposal in place; returns the per-section
/// counts. "Rank, do not suppress" per the librarian.
fn rank_and_section(proposals: &mut [Value]) -> (usize, usize, usize) {
    fn section_of(kind: &str) -> &'static str {
        match kind {
            "cross_repo_twin" => "cross_repo_twin",
            // write_later (intentional forward-refs) + stale_file_ref (verified-dangling file nudge) +
            // value_contradiction (low-precision concurrent-value surfacer) are advisory, not structural
            // edits -- keep them FYI below the merges/add-links.
            "write_later_candidate" | "stale_file_ref" | "value_contradiction" => "fyi",
            _ => "actionable", // near_duplicate (merges) + cross_link (orphan add-links)
        }
    }
    fn rank(section: &str) -> u8 {
        match section {
            "actionable" => 0,
            "cross_repo_twin" => 1,
            _ => 2,
        }
    }
    for p in proposals.iter_mut() {
        let kind = p.get("kind").and_then(Value::as_str).unwrap_or("");
        let section = section_of(kind);
        if let Some(obj) = p.as_object_mut() {
            obj.insert("section".to_string(), json!(section));
        }
    }
    proposals.sort_by(|a, b| {
        let sa = a.get("section").and_then(Value::as_str).unwrap_or("");
        let sb = b.get("section").and_then(Value::as_str).unwrap_or("");
        let ca = a.get("confidence").and_then(Value::as_f64).unwrap_or(0.0);
        let cb = b.get("confidence").and_then(Value::as_f64).unwrap_or(0.0);
        rank(sa)
            .cmp(&rank(sb))
            .then(cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| {
                a.get("proposal_id")
                    .and_then(Value::as_str)
                    .cmp(&b.get("proposal_id").and_then(Value::as_str))
            })
    });
    let count = |s: &str| {
        proposals
            .iter()
            .filter(|p| p.get("section").and_then(Value::as_str) == Some(s))
            .count()
    };
    (count("actionable"), count("cross_repo_twin"), count("fyi"))
}

/// The per-scope dream-pass outcome: the full report (for the caller to summarize/sample) plus the
/// notify-on-new signal. Shared by `dream-analyze` (one scope) and `dream-run` (every scope).
struct ScopeOutcome {
    report: Value,
    new_count: usize,
    new_ids: Vec<String>,
}

/// Structural backstops (task_1379). A new or materially-changed propose-only detector is meant to be
/// dry-run read-only against the real board corpus before ship (the charter discipline), but these caps are
/// the can't-be-forgotten half: a misbehaving detector (e.g. the combinatorial O(k^2) blowup value_contradiction
/// hit on its first real run -- a ~958MB report that 502'd the board + a 1656-proposal flood) must never
/// serialize a giant report or flood the review surface, even if the dry-run was skipped. `MAX_PROPOSALS_PER_SCOPE`
/// caps total proposals across all detectors (overflow dropped lowest-priority, a `capped` diagnostic naming the
/// over-generator); `MAX_REPORT_BYTES` is the ultimate guard against a pathological per-proposal size -- over it
/// the proposals are suppressed to a minimal diagnostic so a publish cannot pressure the board. Healthy scopes
/// sit at 0-50 proposals / a few KB, so these ceilings are generous and only trip on a real malfunction.
const MAX_PROPOSALS_PER_SCOPE: usize = 500;
const MAX_REPORT_BYTES: usize = 2 * 1024 * 1024; // 2 MiB

/// Apply the per-scope proposal-count backstop (task_1379). If `proposals` is within the cap, return
/// `Value::Null`; otherwise truncate it (callers pass it already ordered actionable-first, so the dropped
/// overflow is the lowest-priority) and return a `capped` diagnostic naming the over-generating detector via
/// per-kind counts of the pre-cap set.
fn cap_proposals(proposals: &mut Vec<Value>) -> Value {
    let total_before_cap = proposals.len();
    if total_before_cap <= MAX_PROPOSALS_PER_SCOPE {
        return Value::Null;
    }
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for p in proposals.iter() {
        let kind = p.get("kind").and_then(Value::as_str).unwrap_or("unknown");
        *by_kind.entry(kind.to_string()).or_default() += 1;
    }
    proposals.truncate(MAX_PROPOSALS_PER_SCOPE);
    json!({
        "limit": MAX_PROPOSALS_PER_SCOPE,
        "total_before_cap": total_before_cap,
        "suppressed": total_before_cap - MAX_PROPOSALS_PER_SCOPE,
        "by_kind_before_cap": by_kind,
        "note": "a detector over-generated for this scope (likely a combinatorial blowup); the lowest-priority overflow was suppressed. Do NOT trust this scope's output until the top by_kind detector is investigated (task_1379).",
    })
}

/// Run every detector over `recs`, write the dream-report JSON to `out`, and -- when `publish` is set --
/// publish the versioned `dreams/<scope>` board doc, returning the report and the notify-on-new signal. This
/// is the shared single-scope core: `analyze_cmd` wraps it for one scope, `run_cmd` loops it over every
/// board scope. `scope` is required when `publish` is set (it is the `dreams/<scope>` doc path); `repo_root`
/// enables the verified-dangling staleness detector (skipped when `None`).
#[allow(clippy::too_many_arguments)]
fn run_scope_pass(
    recs: &[Rec],
    scope: Option<&str>,
    out: &Path,
    publish: bool,
    board_memory: &str,
    repo_root: Option<&Path>,
    value_contradictions: bool,
) -> Result<ScopeOutcome, String> {
    let mut prot: HashMap<String, Vec<String>> = HashMap::new();
    for r in recs {
        let reasons = protected_reasons(r);
        if !reasons.is_empty() {
            prot.insert(r.path().to_string(), reasons);
        }
    }
    let backlinks = build_backlinks(recs);

    // Exact-body sha256 per path — the exact-dup identity, reused by the near-dup detector to skip pairs
    // that are already exact duplicates.
    let exact_hashes: HashMap<String, String> = recs
        .iter()
        .map(|r| (r.path().to_string(), sha256_hex(&norm_body(r.body()))))
        .collect();

    let mut proposals = Vec::new();
    proposals.extend(detect_exact_duplicates(recs, &prot, &backlinks));
    proposals.extend(detect_near_duplicates(
        recs,
        &prot,
        &backlinks,
        &exact_hashes,
    ));
    let catalogued = load_forward_ref_catalogue(recs);
    proposals.extend(detect_write_later_candidates(
        recs,
        &prot,
        &backlinks,
        &catalogued,
    ));
    // value_contradiction is OFF by default (opt-in): the first scheduled co-verify showed 0 true positives
    // across ~59 samples even after date-exclusion + same-line proximity + caps -- a numeric VALUE in a memory
    // is almost always a measurement/snapshot (a %, a size, a count), not an asserted invariant, so a
    // deterministic detector cannot tell a genuine contradiction from two different measurements. Kept behind
    // this flag for a future LLM-assisted / much-narrower redesign (task_1123 co-verify, librarian-confirmed).
    if value_contradictions {
        proposals.extend(detect_value_contradictions(recs, &prot, &backlinks));
    }
    // Verified-dangling file refs -- only with a repo worktree to resolve against (never on a corpus-only run).
    if let Some(root) = repo_root {
        proposals.extend(detect_stale_refs(recs, &prot, &backlinks, root));
    }

    rank_and_section(&mut proposals); // sorts actionable-first + stamps `section`; counts recomputed post-cap below
    // Structural backstop (task_1379): cap total proposals/scope (truncates the lowest-priority overflow,
    // since rank_and_section ordered actionable-first) and name the over-generator in a `capped` diagnostic.
    let capped = cap_proposals(&mut proposals);
    let count_section = |s: &str| {
        proposals
            .iter()
            .filter(|p| p.get("section").and_then(Value::as_str) == Some(s))
            .count()
    };
    let (sec_actionable, sec_twin, sec_fyi) = (
        count_section("actionable"),
        count_section("cross_repo_twin"),
        count_section("fyi"),
    );
    let standard = proposals.iter().filter(|p| p["lane"] == "standard").count();
    let protected = proposals
        .iter()
        .filter(|p| p["lane"] == "protected")
        .count();
    let mut detectors_run = vec![
        "exact_duplicate",
        "orphan_add_links",
        "near_duplicate_minhash",
        "write_later_candidate",
    ];
    if value_contradictions {
        detectors_run.push("value_contradiction");
    }
    if repo_root.is_some() {
        detectors_run.push("stale_file_ref");
    }
    let report = json!({
        "schema": "dream-report/v1 (task_827 comment_3869, librarian-blessed comment_3873)",
        "generated_by": "v-agent-memory/dream analyze (Rust port, task_956)",
        "corpus_size": recs.len(),
        "protected_memories": prot.len(),
        "detectors_run": detectors_run,
        "proposal_count": proposals.len(),
        "by_lane": { "standard": standard, "protected": protected },
        "by_section": { "actionable": sec_actionable, "cross_repo_twin": sec_twin, "fyi": sec_fyi },
        "capped": capped,
        "proposals": proposals,
    });

    let mut json = serde_json::to_string_pretty(&report)
        .map_err(|e| format!("report serialize failed: {e}"))?;
    // Final size backstop (task_1379): a count cap cannot bound a pathological per-proposal size, so if the
    // serialized report still exceeds the byte ceiling, suppress the proposals to a minimal diagnostic -- a
    // publish must never pressure/502 the board (the value_contradiction first real run serialized to ~958MB).
    let report = if json.len() > MAX_REPORT_BYTES {
        let diag = json!({
            "schema": report["schema"].clone(),
            "generated_by": report["generated_by"].clone(),
            "corpus_size": recs.len(),
            "protected_memories": prot.len(),
            "detectors_run": report["detectors_run"].clone(),
            "proposal_count": 0,
            "by_section": { "actionable": sec_actionable, "cross_repo_twin": sec_twin, "fyi": sec_fyi },
            "size_suppressed": {
                "serialized_bytes": json.len(),
                "limit": MAX_REPORT_BYTES,
                "note": "the full report exceeded the size ceiling and was suppressed to protect the board; a detector is pathological (task_1379). Proposals omitted -- investigate before trusting this scope.",
            },
            "proposals": [],
        });
        json = serde_json::to_string_pretty(&diag)
            .map_err(|e| format!("diag report serialize failed: {e}"))?;
        diag
    } else {
        report
    };
    std::fs::write(out, &json)
        .map_err(|e| format!("cannot write report {}: {e}", out.display()))?;

    let (new_count, new_ids) = if publish {
        let scope = scope.ok_or("publish requires a scope (the dreams/<scope> doc path)")?;
        publish_board(scope, &report, board_memory)?
    } else {
        (0, Vec::new())
    };
    Ok(ScopeOutcome {
        report,
        new_count,
        new_ids,
    })
}

/// Run the dream analyzer and write the dream-report JSON to `out`. The corpus comes from either a `--corpus`
/// JSONL file or, when `from_board` is set, a live-board pull of `scope` (a wiki path prefix). Returns the
/// process exit code; `sample` prints that many proposals to stderr for a quick eyeball.
#[allow(clippy::too_many_arguments)]
pub fn analyze_cmd(
    corpus: Option<&Path>,
    from_board: bool,
    scope: Option<&str>,
    board_api: &str,
    out: &Path,
    sample: usize,
    publish_board_doc: bool,
    board_memory: &str,
    repo_root: Option<&Path>,
    value_contradictions: bool,
) -> i32 {
    let recs = if from_board {
        let Some(scope) = scope else {
            eprintln!(
                "dream-analyze: --from-board requires --scope <wiki-prefix> (e.g. repos/<repo> or agents/<agent>)"
            );
            return 2;
        };
        match load_corpus_from_board(board_api, scope) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        }
    } else {
        let Some(corpus) = corpus else {
            eprintln!("dream-analyze: pass --corpus <jsonl> or --from-board --scope <wiki-prefix>");
            return 2;
        };
        match load_corpus(corpus) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        }
    };

    if publish_board_doc && scope.is_none() {
        eprintln!("dream-analyze: --publish-board requires --scope (the dreams/<scope> doc path)");
        return 2;
    }
    let outcome = match run_scope_pass(
        &recs,
        scope,
        out,
        publish_board_doc,
        board_memory,
        repo_root,
        value_contradictions,
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let report = &outcome.report;

    eprintln!(
        "corpus: {} memories; protected-class: {}",
        report["corpus_size"], report["protected_memories"]
    );
    eprintln!(
        "proposals: {} (standard {}, protected {})",
        report["proposal_count"], report["by_lane"]["standard"], report["by_lane"]["protected"]
    );
    eprintln!("report: {}", out.display());

    if publish_board_doc {
        let scope = scope.unwrap_or("");
        eprintln!(
            "published: dreams/{scope} ({} new proposal(s))",
            outcome.new_count
        );
        // The scheduler notifies the librarian on this stdout signal, ONLY when new>0.
        if outcome.new_count > 0 {
            println!(
                "DREAM-NEW scope={scope} new={} ids={}",
                outcome.new_count,
                outcome.new_ids.join(",")
            );
        }
    }

    if sample > 0
        && let Some(arr) = report["proposals"].as_array()
    {
        for p in arr.iter().take(sample) {
            let s = p.to_string();
            eprintln!("{}", &s[..s.len().min(400)]);
        }
    }
    0
}

/// The per-scope report filename under `<state-dir>/dreams/`: the scope with `/` folded to `-`, then the
/// date, e.g. `repos-camshaft-cadenza-2026-10-02.json`. Pure.
fn scope_report_filename(scope: &str, date: &str) -> String {
    format!("{}-{}.json", scope.replace('/', "-"), date)
}

/// Candidate local-checkout paths for a repo scope `<slug>` under `base`, most-specific first, so the
/// all-scopes runner can hand the staleness detector a repo-root per scope (task_1123). A board scope slug is
/// `<org>-<name>` (e.g. `camshaft-cadenza`) while a checkout lives at `<base>/<org>/<name>`; the slug's FIRST
/// `-` is the org/name boundary, so `camshaft-s2n-quic` -> `<base>/camshaft/s2n-quic` (only the first `-` is
/// split). An exact `<base>/<slug>` is the fallback for a slug with no org prefix. The caller picks the first
/// candidate that exists on disk; if none exist the scope has no local checkout and the staleness detector
/// skips it (exactly as when no base is configured) — a wrong root would mis-flag refs, so resolution never
/// guesses past these deterministic candidates. Pure; unit-tested.
fn repo_checkout_candidates(base: &Path, repo_slug: &str) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some((org, name)) = repo_slug.split_once('-') {
        candidates.push(base.join(org).join(name));
    }
    candidates.push(base.join(repo_slug));
    candidates
}

/// Today's UTC date as `YYYY-MM-DD` for the per-scope report filename.
fn today_utc_date() -> String {
    time::OffsetDateTime::now_utc().date().to_string()
}

/// The default board channel for dream notify-on-new posts: a dedicated, watchable, pageable feed (the
/// librarian's convention, channel_218), isolated from 1:1 coordination DMs.
pub const DREAM_NOTIFY_CHANNEL: &str = "dream-reports";

/// Format the dream notify-on-new post: one light line per scope with new proposals (scope + count + the
/// `dreams/<scope>` review doc), scannable and low-noise. Only called when at least one scope has new>0;
/// dispositions carry forward so a declined proposal never re-pings. Pure.
fn format_dream_notify_post(new_by_scope: &[(String, usize)]) -> String {
    let total: usize = new_by_scope.iter().map(|(_, n)| n).sum();
    let mut body = format!(
        "Automatic dreaming surfaced {total} new proposal(s) across {} scope(s) -- review the dreams/<scope> doc for each (declined/deferred proposals carry forward and will not re-ping):\n",
        new_by_scope.len()
    );
    for (scope, n) in new_by_scope {
        body.push_str(&format!("- {scope}: {n} new -> dreams/{scope}\n"));
    }
    body
}

/// `fleet dream-run` (task_1123): the automatic-dreaming RUNNER. Pulls every `repos/*` memory from the live
/// board in one pass, groups by repo, and runs the analyze+publish pass per repo scope -- refreshing each
/// versioned `dreams/<scope>` review doc and writing a per-scope report under `<state-dir>/dreams/`. Emits a
/// `DREAM-NEW scope=... new=... ids=...` line to stdout per scope that surfaced new proposals, and posts a
/// light notify-on-new message to the librarian's `notify_channel` (dedicated `dream-reports` feed) when any
/// scope is new. When `repo_root_base` is set, each `repos/<org>-<name>` scope is mapped to its local checkout
/// under the base (see [`repo_checkout_candidates`]) and the verified-dangling staleness detector runs against
/// it; a scope with no local checkout stays corpus-only, and with no base every scope is corpus-only. Returns
/// a nonzero exit if any scope (or the notify) failed, so a failed run is visible to the timer, while still
/// processing the other scopes.
#[allow(clippy::too_many_arguments)]
pub fn run_cmd(
    board_api: &str,
    state_dir: &Path,
    board_memory: &str,
    notify_channel: &str,
    repo_root_base: Option<&Path>,
    value_contradictions: bool,
) -> i32 {
    let date = today_utc_date();
    let recs = match load_corpus_from_board(board_api, "repos") {
        Ok(r) => r,
        Err(e) => {
            eprintln!("dream-run: cannot pull the repos scope from the board: {e}");
            return 1;
        }
    };
    // Group by repo namespace (the second path segment, set by load_corpus_from_board). BTreeMap => scopes
    // run in a deterministic, sorted order.
    let mut by_repo: std::collections::BTreeMap<String, Vec<Rec>> =
        std::collections::BTreeMap::new();
    for r in recs {
        by_repo.entry(r.repo().to_string()).or_default().push(r);
    }
    by_repo.remove("?"); // drop any doc whose path had no namespace segment
    if by_repo.is_empty() {
        eprintln!("dream-run: no repo-scoped memories found under repos/");
        return 0;
    }

    let dreams_dir = state_dir.join("dreams");
    if let Err(e) = std::fs::create_dir_all(&dreams_dir) {
        eprintln!("dream-run: cannot create {}: {e}", dreams_dir.display());
        return 1;
    }

    let mut new_by_scope: Vec<(String, usize)> = Vec::new();
    let mut had_error = false;
    for (repo, group) in &by_repo {
        let scope = format!("repos/{repo}");
        let out = dreams_dir.join(scope_report_filename(&scope, &date));
        // Resolve the scope to a local checkout under the configured base, if any; the staleness detector
        // runs only when the checkout exists on disk (a wrong root would mis-flag refs, so an unresolved
        // scope stays corpus-only rather than guessing).
        let repo_root = repo_root_base.and_then(|base| {
            repo_checkout_candidates(base, repo)
                .into_iter()
                .find(|p| p.is_dir())
        });
        if let Some(root) = &repo_root {
            eprintln!(
                "dream-run: {scope} -- staleness enabled against {}",
                root.display()
            );
        }
        match run_scope_pass(
            group,
            Some(&scope),
            &out,
            true,
            board_memory,
            repo_root.as_deref(),
            value_contradictions,
        ) {
            Ok(o) => {
                eprintln!(
                    "dream-run: {scope} -- {} memories, {} new proposal(s)",
                    group.len(),
                    o.new_count
                );
                if o.new_count > 0 {
                    // Journaled signal (also lets the service debug/trace), plus the collected notify set.
                    println!(
                        "DREAM-NEW scope={scope} new={} ids={}",
                        o.new_count,
                        o.new_ids.join(",")
                    );
                    new_by_scope.push((scope, o.new_count));
                }
            }
            Err(e) => {
                eprintln!("dream-run: {scope} FAILED: {e}");
                had_error = true;
            }
        }
    }

    // Notify-on-new: a light post to the librarian's dedicated `dream-reports` channel, ONLY when a scope
    // surfaced new proposals (dispositions carry forward, so a declined proposal never re-pings).
    if !new_by_scope.is_empty() {
        let board = crate::board::Board::with_base(board_api);
        match board.create_or_get_channel(notify_channel, "v-agent-memory") {
            Ok(ch) => {
                let body = to_ascii(&format_dream_notify_post(&new_by_scope));
                if let Err(e) = board.post_to_channel(ch, "v-agent-memory", &body) {
                    eprintln!("dream-run: notify post to {notify_channel} failed: {e}");
                    had_error = true;
                } else {
                    eprintln!(
                        "dream-run: notified {notify_channel} of {} scope(s) with new proposals",
                        new_by_scope.len()
                    );
                }
            }
            Err(e) => {
                eprintln!("dream-run: cannot resolve notify channel {notify_channel}: {e}");
                had_error = true;
            }
        }
    }

    eprintln!(
        "dream-run: {} scope(s) processed, {} with new proposals",
        by_repo.len(),
        new_by_scope.len()
    );
    if had_error { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(slug: &str, repo: &str, body: &str) -> Rec {
        Rec {
            slug: slug.into(),
            repo: Some(repo.into()),
            name: None,
            description: None,
            rtype: None,
            body: Some(body.into()),
            source: None,
            doc_id: None,
            provenance: None,
            path: Some(format!("repos/{repo}/{slug}")),
        }
    }

    #[test]
    fn find_links_keeps_order_and_duplicates_cut_at_delimiters() {
        assert_eq!(
            find_links("[[a]] [[b|x]] [[a#r]] [[a]]"),
            vec!["a", "b", "a", "a"]
        );
        assert!(find_links("no links").is_empty());
        // Nested bracket inside the capture (']' only stops it) is preserved like the regex.
        assert_eq!(find_links("[[a[[b]]"), vec!["a[[b"]);
    }

    #[test]
    fn is_slug_matches_lowercase_kebab_3plus() {
        assert!(is_slug("abc"));
        assert!(is_slug("a-b-c"));
        assert!(is_slug("index-foo"));
        assert!(!is_slug("ab")); // too short
        assert!(!is_slug("Abc")); // uppercase lead
        assert!(!is_slug("a_b")); // underscore not allowed
        assert!(!is_slug("-ab")); // lead must be [a-z0-9]
    }

    #[test]
    fn norm_body_trims_and_rstrips_lines() {
        // The whole body is trimmed first (so line one's leading spaces go), then each line is right-trimmed.
        assert_eq!(
            norm_body("\n  line one  \n  line two\t\n\n"),
            "line one\n  line two"
        );
    }

    #[test]
    fn protected_flags_index_and_operator_directive() {
        let idx = rec("index-foo", "r", "body");
        assert!(
            protected_reasons(&idx)
                .iter()
                .any(|s| s.contains("sub-index canon"))
        );
        let op = rec("operator-standing-directive-x", "r", "b");
        assert!(
            protected_reasons(&op)
                .iter()
                .any(|s| s.contains("operator-directive"))
        );
        let plain = rec("a-plain-memory", "r", "nothing special");
        assert!(protected_reasons(&plain).is_empty());
    }

    #[test]
    fn protected_canon_pointer_needs_8plus_links_and_60pct() {
        // 8 links on 8 nonblank lines -> 100% >= 60% and >=8 -> canon pointer.
        let body = (0..8)
            .map(|i| format!("[[link-{i}]]"))
            .collect::<Vec<_>>()
            .join("\n");
        let r = rec("mostly-pointers", "r", &body);
        assert!(
            protected_reasons(&r)
                .iter()
                .any(|s| s.contains("canon pointer"))
        );
        // 7 links is below the 8 floor.
        let body7 = (0..7)
            .map(|i| format!("[[link-{i}]]"))
            .collect::<Vec<_>>()
            .join("\n");
        let r7 = rec("few-pointers", "r", &body7);
        assert!(
            !protected_reasons(&r7)
                .iter()
                .any(|s| s.contains("canon pointer"))
        );
    }

    #[test]
    fn exact_duplicate_cross_repo_twin_is_annotate_keep_both() {
        let recs = vec![
            rec("shared-note", "repo-a", "identical body here"),
            rec("shared-note", "repo-b", "identical body here"),
        ];
        let backlinks = build_backlinks(&recs);
        let props = detect_exact_duplicates(&recs, &HashMap::new(), &backlinks);
        // One cross-repo-twin proposal; no index layer in either repo so no orphan add-links proposal.
        assert_eq!(props.len(), 1);
        assert_eq!(props[0]["kind"], "cross_repo_twin");
        assert_eq!(props[0]["proposed_change"]["op"], "annotate");
        assert_eq!(props[0]["confidence"], 0.9);
    }

    #[test]
    fn exact_duplicate_within_repo_is_merge_with_survivor() {
        let mut a = rec("note-long-path-xxxx", "r", "same body");
        a.description = Some("longer description wins".into());
        let b = rec("n", "r", "same body"); // shorter path, empty desc
        let recs = vec![a, b];
        let backlinks = build_backlinks(&recs);
        let props = detect_exact_duplicates(&recs, &HashMap::new(), &backlinks);
        assert_eq!(props.len(), 1);
        assert_eq!(props[0]["kind"], "near_duplicate");
        assert_eq!(props[0]["proposed_change"]["op"], "merge");
        // Richer description wins the survivor slot even though its path is longer.
        assert_eq!(
            props[0]["proposed_change"]["diff"]["survivor_path"],
            "repos/r/note-long-path-xxxx"
        );
    }

    #[test]
    fn kebab_tokens_requires_three_segments() {
        let t = kebab_tokens("see also-foo and alpha-beta-gamma plus x-y here");
        assert!(t.contains("alpha-beta-gamma"));
        assert!(!t.contains("also-foo")); // only 2 segments
        assert!(!t.contains("x-y"));
    }

    #[test]
    fn shingles_jaccard_is_1_for_identical_and_0_for_disjoint() {
        let a = shingles("the quick brown fox jumps over the lazy dog today here");
        let b = shingles("the quick brown fox jumps over the lazy dog today here");
        assert_eq!(jaccard(&a, &b), 1.0);
        let c = shingles("completely different words with nothing shared at all between them");
        assert_eq!(jaccard(&a, &c), 0.0);
        // A short body (< k tokens) still yields one shingle, not an empty set.
        assert_eq!(shingles("two words").len(), 1);
        assert!(shingles("").is_empty());
    }

    #[test]
    fn minhash_signature_is_deterministic_and_sized() {
        let sh = shingles("the quick brown fox jumps over the lazy dog eats food");
        let coeffs = minhash_coeffs();
        let s1 = minhash_sig(&sh, &coeffs);
        let s2 = minhash_sig(&sh, &coeffs);
        assert_eq!(s1, s2); // deterministic
        assert_eq!(s1.len(), MINHASH_M);
        // Fresh coeffs are the same fixed seed -> identical.
        assert_eq!(minhash_coeffs(), coeffs);
    }

    fn near_rec(slug: &str, repo: &str, body: &str) -> Rec {
        let mut r = rec(slug, repo, body);
        r.description = Some(format!("desc for {slug}"));
        r
    }

    // A long body (60 distinct tokens) with one token swapped yields Jaccard ~0.84 (above the 0.80 floor)
    // under k=5 shingles — changing a single word perturbs only ~5 of ~56 shingles.
    fn long_body_pair() -> (String, String) {
        let words: Vec<String> = (0..60).map(|i| format!("tokenword{i}")).collect();
        let base = words.join(" ");
        let mut nearw = words.clone();
        nearw[30] = "swappedtoken".to_string();
        (base, nearw.join(" "))
    }

    #[test]
    fn near_duplicate_within_repo_is_a_merge_candidate() {
        let (base, near) = long_body_pair();
        let recs = vec![
            near_rec("effect-lowering-note-one", "r1", &base),
            near_rec("effect-lowering-note-two", "r1", &near),
        ];
        let backlinks = build_backlinks(&recs);
        let exact: HashMap<String, String> = recs
            .iter()
            .map(|r| (r.path().to_string(), sha256_hex(&norm_body(r.body()))))
            .collect();
        let props = detect_near_duplicates(&recs, &HashMap::new(), &backlinks, &exact);
        assert_eq!(props.len(), 1, "one within-repo near-dup cluster");
        assert_eq!(props[0]["kind"], "near_duplicate");
        assert_eq!(props[0]["proposed_change"]["op"], "merge");
        assert!(props[0]["confidence"].as_f64().unwrap() >= NEAR_DUP_THRESH);
    }

    #[test]
    fn near_duplicate_skips_exact_duplicate_pairs() {
        // Byte-identical bodies are the exact-dup detector's job; near-dup must skip them.
        let body = "identical bodies are handled by the exact duplicate detector not the fuzzy one here now";
        let recs = vec![
            near_rec("twin-a-slug", "r1", body),
            near_rec("twin-b-slug", "r1", body),
        ];
        let backlinks = build_backlinks(&recs);
        let exact: HashMap<String, String> = recs
            .iter()
            .map(|r| (r.path().to_string(), sha256_hex(&norm_body(r.body()))))
            .collect();
        let props = detect_near_duplicates(&recs, &HashMap::new(), &backlinks, &exact);
        assert!(
            props.is_empty(),
            "exact-dup pair is skipped by the near-dup detector"
        );
    }

    #[test]
    fn dream_state_round_trips_through_render_and_parse() {
        let report = json!({
            "corpus_size": 3,
            "proposals": [
                {"proposal_id":"dp-x-1","kind":"near_duplicate","confidence":1.0,"section":"actionable",
                 "rationale":"two memories share a body","targets":[{"path":"repos/r/a"},{"path":"repos/r/b"}]}
            ]
        });
        let state = vec![(
            "dp-x-1".to_string(),
            "abc123def456".to_string(),
            "declined".to_string(),
        )];
        let body = render_dream_doc("repos/r", &report, &state);
        // Rendered doc shows the section + the carried disposition inline.
        assert!(body.contains("## Actionable"));
        assert!(body.contains("[disposition: declined]"));
        assert!(body.contains("Targets: repos/r/a, repos/r/b"));
        // The machine state block round-trips back to the same (fingerprint, disposition).
        let parsed = parse_dream_state(&body);
        assert_eq!(
            parsed.get("dp-x-1"),
            Some(&("abc123def456".to_string(), "declined".to_string()))
        );
        // A body with no state block parses empty (first run).
        assert!(parse_dream_state("# just a doc, no state").is_empty());
    }

    #[test]
    fn proposal_fingerprint_is_stable_and_tracks_substance() {
        let a = json!({"proposed_change": {"op":"merge","diff":{"survivor_path":"x"}}});
        let b = json!({"proposed_change": {"op":"merge","diff":{"survivor_path":"y"}}});
        assert_eq!(proposal_fingerprint(&a), proposal_fingerprint(&a)); // stable
        assert_ne!(proposal_fingerprint(&a), proposal_fingerprint(&b)); // changes with the diff
    }

    #[test]
    fn rank_and_section_orders_actionable_then_twins_then_fyi_by_confidence() {
        let mut props = vec![
            json!({"proposal_id": "w1", "kind": "write_later_candidate", "confidence": 0.2}),
            json!({"proposal_id": "t1", "kind": "cross_repo_twin", "confidence": 0.9}),
            json!({"proposal_id": "m1", "kind": "near_duplicate", "confidence": 0.85}),
            json!({"proposal_id": "m2", "kind": "near_duplicate", "confidence": 1.0}),
            json!({"proposal_id": "o1", "kind": "cross_link", "confidence": 0.6}),
        ];
        let (actionable, twins, fyi) = rank_and_section(&mut props);
        assert_eq!((actionable, twins, fyi), (3, 1, 1)); // m1,m2,o1 | t1 | w1
        // sections tagged
        assert_eq!(props[0]["section"], "actionable");
        // actionable block first, by confidence desc: m2 (1.0), m1 (0.85), o1 (0.6)
        let ids: Vec<&str> = props
            .iter()
            .map(|p| p["proposal_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["m2", "m1", "o1", "t1", "w1"]);
        // the cross-repo twin is its own section, write-later is FYI last.
        assert_eq!(props[3]["section"], "cross_repo_twin");
        assert_eq!(props[4]["section"], "fyi");
    }

    #[test]
    fn near_duplicate_cross_repo_is_keep_both_annotation() {
        let (base, near) = long_body_pair();
        let recs = vec![
            near_rec("arena-per-request-note", "repo-a", &base),
            near_rec("arena-per-request-note", "repo-b", &near),
        ];
        let backlinks = build_backlinks(&recs);
        let exact: HashMap<String, String> = recs
            .iter()
            .map(|r| (r.path().to_string(), sha256_hex(&norm_body(r.body()))))
            .collect();
        let props = detect_near_duplicates(&recs, &HashMap::new(), &backlinks, &exact);
        assert_eq!(props.len(), 1);
        assert_eq!(props[0]["kind"], "cross_repo_twin");
        assert_eq!(props[0]["proposed_change"]["op"], "annotate");
        assert_eq!(
            props[0]["proposed_change"]["diff"]["flag"],
            "cross-repo-twin-fuzzy"
        );
    }

    #[test]
    fn extract_file_path_refs_rules() {
        let refs = extract_file_path_refs(
            "see `crates/fleet/src/dream.rs` and src/main.rs:1551 but not MEMORY.md or and/or; \
             skip https://x.com/a/b.rs and /abs/host/path.rs and ./rel/x.rs; keep spec/syntax/foo.md.",
        );
        assert!(refs.contains("crates/fleet/src/dream.rs")); // backtick-wrapped, kept
        assert!(refs.contains("src/main.rs")); // trailing :1551 locator stripped
        assert!(refs.contains("spec/syntax/foo.md")); // trailing sentence '.' trimmed
        assert!(!refs.contains("MEMORY.md")); // no '/', a bare filename is never a candidate
        assert!(!refs.iter().any(|r| r.contains("x.com"))); // URL (scheme) excluded
        assert!(!refs.iter().any(|r| r.starts_with("/abs"))); // absolute/host path excluded
        assert!(!refs.iter().any(|r| r.contains("rel/x.rs"))); // ./ relative climb excluded
        assert!(!refs.contains("and/or")); // has '/' but no source extension -> not a candidate
    }

    #[test]
    fn strip_fenced_code_drops_fenced_keeps_inline() {
        let body = "real `inline/path.rs` here\n```\nfenced/example.rs\n```\nafter line\n";
        let out = strip_fenced_code(body);
        assert!(out.contains("inline/path.rs")); // inline-code path survives (a real citation)
        assert!(!out.contains("fenced/example.rs")); // fenced snippet is illustrative, dropped
        assert!(out.contains("after line"));
    }

    #[test]
    fn detect_stale_refs_flags_missing_but_not_existing_or_gitignored() {
        use std::process::Command;
        let root = std::env::temp_dir().join(format!("dream-stale-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let inited = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["init", "-q"])
            .status();
        if inited.map(|s| !s.success()).unwrap_or(true) {
            // No usable git here: the detector is designed to skip (git_check_ignore -> Err), nothing to assert.
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        std::fs::write(root.join(".gitignore"), "implementation/\n").unwrap();
        std::fs::write(root.join("src/exists.rs"), "// present\n").unwrap();
        // src/missing.rs is never created; implementation/seed/gen.rs is both absent and gitignored.
        let body =
            "live `src/exists.rs`, dangling src/missing.rs, regen implementation/seed/gen.rs.";
        let recs = vec![rec("mem-with-refs", "r", body)];
        let backlinks = build_backlinks(&recs);
        let props = detect_stale_refs(&recs, &HashMap::new(), &backlinks, &root);
        assert_eq!(props.len(), 1, "one memory carries a real dangling ref");
        assert_eq!(props[0]["kind"], "stale_file_ref");
        assert_eq!(props[0]["lane"], "standard");
        assert_eq!(props[0]["confidence"], 0.5);
        let flagged: Vec<&str> = props[0]["proposed_change"]["diff"]["dangling_file_refs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        // Only the genuine dangle: exists.rs is present, implementation/ is gitignored (regenerable).
        assert_eq!(flagged, vec!["src/missing.rs"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn ts_rec(slug: &str, body: &str, name: &str, ts: &str) -> Rec {
        let mut r = rec(slug, "r", body);
        r.name = Some(name.into());
        r.provenance = Some(Provenance {
            author: None,
            timestamp: Some(json!(ts)),
        });
        r
    }

    #[test]
    fn extract_quantities_parses_number_unit_pairs() {
        let q = extract_quantities(
            "limit 512KB, size 1 MB, ratio 60%, window 7 days, id task_827, date 2026-10-02",
        );
        assert!(q.contains(&("kb".to_string(), 512.0)));
        assert!(q.contains(&("mb".to_string(), 1.0)));
        assert!(q.contains(&("%".to_string(), 60.0)));
        assert!(q.contains(&("days".to_string(), 7.0)));
        // task_827 (no adjacent unit) and the date (no unit) produce nothing: exactly the four real quantities.
        assert_eq!(q.len(), 4);
    }

    #[test]
    fn provenance_unix_secs_parses_rfc3339_and_epoch() {
        let s = provenance_unix_secs(Some(&json!("2026-01-01T00:00:00Z"))).unwrap();
        assert!(s > 0);
        assert_eq!(provenance_unix_secs(Some(&json!(s))), Some(s)); // epoch-number form round-trips
        assert!(provenance_unix_secs(Some(&json!("not a date"))).is_none());
        assert!(provenance_unix_secs(None).is_none());
    }

    #[test]
    fn detect_value_contradictions_flags_concurrent_differing_values() {
        let recs = vec![
            ts_rec(
                "limit-note-a",
                "the rcdzc-test-size-limit is 512KB today",
                "a",
                "2026-09-01T00:00:00Z",
            ),
            ts_rec(
                "limit-note-b",
                "the rcdzc-test-size-limit is 1024KB now",
                "b",
                "2026-09-03T00:00:00Z",
            ),
        ];
        let backlinks = build_backlinks(&recs);
        let props = detect_value_contradictions(&recs, &HashMap::new(), &backlinks);
        assert_eq!(
            props.len(),
            1,
            "one concurrent differing-value pair on a shared subject"
        );
        assert_eq!(props[0]["kind"], "value_contradiction");
        assert_eq!(props[0]["lane"], "standard");
        assert_eq!(props[0]["confidence"], 0.3);
        assert_eq!(props[0]["proposed_change"]["diff"]["unit"], "kb");
        assert_eq!(
            props[0]["proposed_change"]["diff"]["shared_subject"],
            "rcdzc-test-size-limit"
        );
    }

    #[test]
    fn detect_value_contradictions_suppresses_temporal_gap() {
        // Same subject + differing value, but ~8 months apart -> temporal evolution, not a contradiction.
        let recs = vec![
            ts_rec(
                "limit-note-a",
                "the rcdzc-test-size-limit is 512KB",
                "a",
                "2026-01-01T00:00:00Z",
            ),
            ts_rec(
                "limit-note-b",
                "the rcdzc-test-size-limit is 1024KB",
                "b",
                "2026-09-01T00:00:00Z",
            ),
        ];
        let backlinks = build_backlinks(&recs);
        assert!(detect_value_contradictions(&recs, &HashMap::new(), &backlinks).is_empty());
    }

    #[test]
    fn detect_value_contradictions_needs_timestamps_and_shared_subject() {
        // Concurrent + differing value + shared subject, but NO timestamp -> cannot prove concurrency.
        let no_ts = vec![
            {
                let mut r = rec("a", "r", "the rcdzc-test-size-limit is 512KB");
                r.name = Some("a".into());
                r
            },
            {
                let mut r = rec("b", "r", "the rcdzc-test-size-limit is 1024KB");
                r.name = Some("b".into());
                r
            },
        ];
        let bl = build_backlinks(&no_ts);
        assert!(detect_value_contradictions(&no_ts, &HashMap::new(), &bl).is_empty());

        // Concurrent + same unit + differing value, but NO shared subject token -> not about the same thing.
        let no_subject = vec![
            ts_rec(
                "c",
                "alpha-beta-gamma budget is 512KB",
                "c",
                "2026-09-01T00:00:00Z",
            ),
            ts_rec(
                "d",
                "delta-epsilon-zeta cap is 1024KB",
                "d",
                "2026-09-02T00:00:00Z",
            ),
        ];
        let bl2 = build_backlinks(&no_subject);
        assert!(detect_value_contradictions(&no_subject, &HashMap::new(), &bl2).is_empty());
    }

    #[test]
    fn detect_value_contradictions_skips_oversized_buckets() {
        // A (unit, subject-token) bucket shared by MORE than MAX_VALUE_CONTRADICTION_BUCKET distinct memories
        // (all concurrent, all distinct KB values on the same shared token) is a common identifier, not a
        // distinctive subject -- skipped, so no O(k^2) blowup. This is the cadenza-scale guard.
        let n = MAX_VALUE_CONTRADICTION_BUCKET + 1;
        let recs: Vec<Rec> = (0..n)
            .map(|i| {
                ts_rec(
                    &format!("mem-{i}"),
                    &format!("the common-shared-subject-token is {}KB", 100 + i),
                    &format!("m{i}"),
                    "2026-09-01T00:00:00Z",
                )
            })
            .collect();
        let bl = build_backlinks(&recs);
        assert!(
            detect_value_contradictions(&recs, &HashMap::new(), &bl).is_empty(),
            "a bucket of {n} (> cap {MAX_VALUE_CONTRADICTION_BUCKET}) is skipped, not O(k^2)-paired"
        );

        // Exactly 2 in the bucket (<= cap) still produces the contradiction -- the cap bounds, not disables.
        let pair = &recs[..2];
        let bl2 = build_backlinks(pair);
        assert_eq!(
            detect_value_contradictions(pair, &HashMap::new(), &bl2).len(),
            1,
            "a small bucket still surfaces the real contradiction"
        );
    }

    #[test]
    fn detect_value_contradictions_excludes_date_subject_tokens() {
        // The ONLY shared same-line kebab token is a date (2026-09-26); the alpha subjects differ. A date is
        // not a subject (is_subject_token requires a letter), so there is no shared subject -> no flag.
        // Without the exclusion the shared date would key them and emit a false contradiction.
        let recs = vec![
            ts_rec(
                "a",
                "the alpha-beta-one metric 2026-09-26 is 3 d",
                "a",
                "2026-09-01T00:00:00Z",
            ),
            ts_rec(
                "b",
                "the gamma-delta-two metric 2026-09-26 is 1 d",
                "b",
                "2026-09-02T00:00:00Z",
            ),
        ];
        let bl = build_backlinks(&recs);
        assert!(detect_value_contradictions(&recs, &HashMap::new(), &bl).is_empty());
    }

    #[test]
    fn detect_value_contradictions_requires_same_line_proximity() {
        // Shared alpha subject token + same unit + different concurrent values, but the value is on a
        // DIFFERENT line from the subject token -> not textually about that subject -> no flag (the
        // metric-conflation fix). Whole-body matching would have flagged it.
        let recs = vec![
            ts_rec(
                "a",
                "about project-foo-bar notes\nheap is 4 gib",
                "a",
                "2026-09-01T00:00:00Z",
            ),
            ts_rec(
                "b",
                "about project-foo-bar notes\nheap is 494 gib",
                "b",
                "2026-09-02T00:00:00Z",
            ),
        ];
        let bl = build_backlinks(&recs);
        assert!(detect_value_contradictions(&recs, &HashMap::new(), &bl).is_empty());

        // Same subject + value ON the same line still flags (proximity gate bounds, does not disable).
        let same_line = vec![
            ts_rec(
                "c",
                "project-foo-bar heap is 4 gib",
                "c",
                "2026-09-01T00:00:00Z",
            ),
            ts_rec(
                "d",
                "project-foo-bar heap is 494 gib",
                "d",
                "2026-09-02T00:00:00Z",
            ),
        ];
        let bl2 = build_backlinks(&same_line);
        assert_eq!(
            detect_value_contradictions(&same_line, &HashMap::new(), &bl2).len(),
            1
        );
    }

    #[test]
    fn scope_report_filename_folds_slashes_and_appends_date() {
        assert_eq!(
            scope_report_filename("repos/camshaft-cadenza", "2026-10-02"),
            "repos-camshaft-cadenza-2026-10-02.json"
        );
        assert_eq!(
            scope_report_filename("repos/fleet", "2026-01-01"),
            "repos-fleet-2026-01-01.json"
        );
    }

    #[test]
    fn repo_checkout_candidates_splits_org_on_first_dash_then_falls_back_exact() {
        let base = Path::new("/p");
        // org-prefixed slug: first `-` is the org/name boundary; only the first `-` splits, so a
        // multi-dash name (s2n-quic) stays intact under the org dir.
        assert_eq!(
            repo_checkout_candidates(base, "camshaft-cadenza"),
            vec![
                PathBuf::from("/p/camshaft/cadenza"),
                PathBuf::from("/p/camshaft-cadenza")
            ]
        );
        assert_eq!(
            repo_checkout_candidates(base, "camshaft-s2n-quic"),
            vec![
                PathBuf::from("/p/camshaft/s2n-quic"),
                PathBuf::from("/p/camshaft-s2n-quic")
            ]
        );
        // no org prefix: exact `<base>/<slug>` is the only candidate.
        assert_eq!(
            repo_checkout_candidates(base, "membrain"),
            vec![PathBuf::from("/p/membrain")]
        );
    }

    #[test]
    fn format_dream_notify_post_lists_scopes_and_totals() {
        let body = format_dream_notify_post(&[
            ("repos/camshaft-cadenza".to_string(), 3),
            ("repos/fleet".to_string(), 1),
        ]);
        assert!(body.contains("4 new proposal(s) across 2 scope(s)")); // 3 + 1 total
        assert!(body.contains("- repos/camshaft-cadenza: 3 new -> dreams/repos/camshaft-cadenza"));
        assert!(body.contains("- repos/fleet: 1 new -> dreams/repos/fleet"));
        assert!(body.contains("will not re-ping")); // the no-noise disposition note
    }

    #[test]
    fn cap_proposals_under_limit_is_a_noop() {
        let mut proposals: Vec<Value> = (0..50)
            .map(|i| json!({ "proposal_id": format!("p{i}"), "kind": "near_duplicate_minhash" }))
            .collect();
        let capped = cap_proposals(&mut proposals);
        assert_eq!(proposals.len(), 50); // unchanged
        assert!(capped.is_null()); // no diagnostic when within the cap
    }

    #[test]
    fn cap_proposals_over_limit_truncates_and_names_the_over_generator() {
        // Simulate a combinatorial blowup: one detector floods, a few genuine proposals ride along.
        let mut proposals: Vec<Value> = Vec::new();
        for i in 0..550 {
            proposals
                .push(json!({ "proposal_id": format!("vc{i}"), "kind": "value_contradiction" }));
        }
        for i in 0..50 {
            proposals
                .push(json!({ "proposal_id": format!("nd{i}"), "kind": "near_duplicate_minhash" }));
        }
        let total = proposals.len(); // 600
        let capped = cap_proposals(&mut proposals);
        // Overflow truncated to the ceiling.
        assert_eq!(proposals.len(), MAX_PROPOSALS_PER_SCOPE);
        // Diagnostic names the over-generator with pre-cap counts and the suppressed delta.
        assert_eq!(capped["total_before_cap"], json!(total));
        assert_eq!(capped["limit"], json!(MAX_PROPOSALS_PER_SCOPE));
        assert_eq!(capped["suppressed"], json!(total - MAX_PROPOSALS_PER_SCOPE));
        assert_eq!(
            capped["by_kind_before_cap"]["value_contradiction"],
            json!(550)
        );
        assert_eq!(
            capped["by_kind_before_cap"]["near_duplicate_minhash"],
            json!(50)
        );
        assert!(capped["note"].as_str().unwrap().contains("task_1379"));
    }
}
