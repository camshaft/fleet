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
use std::collections::{BTreeSet, HashMap};
use std::path::Path;

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
            if link != r.slug && slugs_by_repo.get(repo).is_some_and(|s| s.contains(link.as_str())) {
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
            let ka = (Reverse(char_len(a.description.as_deref().unwrap_or(""))), char_len(a.path()), *ia);
            let kb = (Reverse(char_len(b.description.as_deref().unwrap_or(""))), char_len(b.path()), *ib);
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
        let sig = if sh.is_empty() { None } else { Some(minhash_sig(&sh, &coeffs)) };
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
                buckets.entry(sig[lo..lo + rows].to_vec()).or_default().push(p);
            }
        }
        for grp in buckets.values() {
            if grp.len() > 1 {
                for i in 0..grp.len() {
                    for j in (i + 1)..grp.len() {
                        let (a, b) = (grp[i], grp[j]);
                        let pair = if a <= b { (a.to_string(), b.to_string()) } else { (b.to_string(), a.to_string()) };
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
        let any_prot: Vec<&&Rec> = group.iter().filter(|r| prot.contains_key(r.path())).collect();
        let lane = if any_prot.is_empty() { "standard" } else { "protected" };
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
                    repos.iter().map(|r| format!("'{r}'")).collect::<Vec<_>>().join(", ")
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

        let any_prot: Vec<&&Rec> = group.iter().filter(|r| prot.contains_key(r.path())).collect();
        let lane = if any_prot.is_empty() { "standard" } else { "protected" };
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
            let superseded: Vec<&&Rec> =
                group.iter().filter(|r| r.path() != survivor.path()).collect();
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
                repos.iter().map(|r| format!("'{r}'")).collect::<Vec<_>>().join(", ")
            );
            let rationale = format!(
                "byte-identical copies across repos {} (sha256 {}); cross-repo presence is usually intentional and [[links]] resolve within-repo only -- keep both, watch for drift{}",
                repos_py,
                hp,
                if orphans.is_empty() {
                    String::new()
                } else {
                    format!("; NOTE {} copy has zero local backlinks (possible orphan)", orphans.len())
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
            while j < b.len() && (b[j].is_ascii_lowercase() || b[j].is_ascii_digit() || b[j] == b'-') {
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
        slugs_by_repo.entry(r.repo()).or_default().insert(r.slug.as_str());
    }
    let mut out = Vec::new();
    for r in recs {
        let Some(reasons) = prot.get(r.path()) else {
            continue; // canon/index only
        };
        let repo = r.repo();
        let links: BTreeSet<String> = find_links(r.body()).iter().map(|l| l.trim().to_string()).collect();
        let mut candidates: Vec<String> = links
            .iter()
            .filter(|l| {
                !l.is_empty()
                    && l.as_str() != r.slug
                    && !slugs_by_repo.get(repo).is_some_and(|s| s.contains(l.as_str()))
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

/// Load the JSONL corpus (one memory record per line). An empty line is skipped; a malformed line is an
/// error (naming the line number) so a corrupt corpus never silently drops memories.
fn load_corpus(path: &Path) -> Result<Vec<Rec>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read corpus {}: {e}", path.display()))?;
    let mut recs = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut rec: Rec = serde_json::from_str(line)
            .map_err(|e| format!("corpus line {}: {e}", n + 1))?;
        if rec.path.is_none() {
            rec.path = Some(format!("repos/{}/{}", rec.repo(), rec.slug));
        }
        recs.push(rec);
    }
    Ok(recs)
}

/// Run the dream analyzer over `corpus` and write the dream-report JSON to `out`. Returns the process exit
/// code. `sample` prints that many proposals to stderr for a quick eyeball.
pub fn analyze_cmd(corpus: &Path, out: &Path, sample: usize) -> i32 {
    let recs = match load_corpus(corpus) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    let mut prot: HashMap<String, Vec<String>> = HashMap::new();
    for r in &recs {
        let reasons = protected_reasons(r);
        if !reasons.is_empty() {
            prot.insert(r.path().to_string(), reasons);
        }
    }
    let backlinks = build_backlinks(&recs);

    // Exact-body sha256 per path — the exact-dup identity, reused by the near-dup detector to skip pairs
    // that are already exact duplicates.
    let exact_hashes: HashMap<String, String> = recs
        .iter()
        .map(|r| (r.path().to_string(), sha256_hex(&norm_body(r.body()))))
        .collect();

    let mut proposals = Vec::new();
    proposals.extend(detect_exact_duplicates(&recs, &prot, &backlinks));
    proposals.extend(detect_near_duplicates(&recs, &prot, &backlinks, &exact_hashes));
    let catalogued = load_forward_ref_catalogue(&recs);
    proposals.extend(detect_write_later_candidates(&recs, &prot, &backlinks, &catalogued));

    let standard = proposals.iter().filter(|p| p["lane"] == "standard").count();
    let protected = proposals.iter().filter(|p| p["lane"] == "protected").count();
    let report = json!({
        "schema": "dream-report/v1 (task_827 comment_3869, librarian-blessed comment_3873)",
        "generated_by": "v-agent-memory/dream analyze (Rust port, task_956)",
        "corpus_size": recs.len(),
        "protected_memories": prot.len(),
        "detectors_run": ["exact_duplicate", "orphan_add_links", "near_duplicate_minhash", "write_later_candidate"],
        "proposal_count": proposals.len(),
        "by_lane": { "standard": standard, "protected": protected },
        "proposals": proposals,
    });

    let json = match serde_json::to_string_pretty(&report) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("report serialize failed: {e}");
            return 1;
        }
    };
    if let Err(e) = std::fs::write(out, json) {
        eprintln!("cannot write report {}: {e}", out.display());
        return 1;
    }

    eprintln!("corpus: {} memories; protected-class: {}", recs.len(), prot.len());
    eprintln!(
        "proposals: {} (standard {standard}, protected {protected})",
        report["proposal_count"]
    );
    eprintln!("report: {}", out.display());
    if sample > 0 && let Some(arr) = report["proposals"].as_array() {
        for p in arr.iter().take(sample) {
            let s = p.to_string();
            eprintln!("{}", &s[..s.len().min(400)]);
        }
    }
    0
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
        assert_eq!(norm_body("\n  line one  \n  line two\t\n\n"), "line one\n  line two");
    }

    #[test]
    fn protected_flags_index_and_operator_directive() {
        let idx = rec("index-foo", "r", "body");
        assert!(protected_reasons(&idx).iter().any(|s| s.contains("sub-index canon")));
        let op = rec("operator-standing-directive-x", "r", "b");
        assert!(protected_reasons(&op).iter().any(|s| s.contains("operator-directive")));
        let plain = rec("a-plain-memory", "r", "nothing special");
        assert!(protected_reasons(&plain).is_empty());
    }

    #[test]
    fn protected_canon_pointer_needs_8plus_links_and_60pct() {
        // 8 links on 8 nonblank lines -> 100% >= 60% and >=8 -> canon pointer.
        let body = (0..8).map(|i| format!("[[link-{i}]]")).collect::<Vec<_>>().join("\n");
        let r = rec("mostly-pointers", "r", &body);
        assert!(protected_reasons(&r).iter().any(|s| s.contains("canon pointer")));
        // 7 links is below the 8 floor.
        let body7 = (0..7).map(|i| format!("[[link-{i}]]")).collect::<Vec<_>>().join("\n");
        let r7 = rec("few-pointers", "r", &body7);
        assert!(!protected_reasons(&r7).iter().any(|s| s.contains("canon pointer")));
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
        let recs = vec![near_rec("twin-a-slug", "r1", body), near_rec("twin-b-slug", "r1", body)];
        let backlinks = build_backlinks(&recs);
        let exact: HashMap<String, String> = recs
            .iter()
            .map(|r| (r.path().to_string(), sha256_hex(&norm_body(r.body()))))
            .collect();
        let props = detect_near_duplicates(&recs, &HashMap::new(), &backlinks, &exact);
        assert!(props.is_empty(), "exact-dup pair is skipped by the near-dup detector");
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
        assert_eq!(props[0]["proposed_change"]["diff"]["flag"], "cross-repo-twin-fuzzy");
    }
}
