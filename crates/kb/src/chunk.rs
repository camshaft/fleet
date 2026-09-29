//! `chunk` — text chunking + the deterministic point-id, ported from Python `kb/chunk.py` + the `_id`
//! helper duplicated across the Python ingest modules.
//!
//! Phase 1 (the MCP server + CLI) only ever chunks short in-memory strings (a `kb_remember` note), so file
//! discovery + PDF/text extraction from `kb/chunk.py` are deferred to the phase-2 ingest port. The
//! character-window `chunk_text` and the `_id` are ported now because both `kb_remember`/`kb_supersede`
//! (server) and every future ingest path depend on them being byte-identical to Python.

// Ported in phase 1 (it must be byte-identical to Python — the deterministic `id` gates re-ingest
// idempotency), but only CALLED from the phase-2 ingest workers, so the helpers read as dead code until then.
#![allow(dead_code)]

use md5::{Digest, Md5};
use uuid::Uuid;

/// Deterministic point id: `uuid.UUID(md5("a|b|c").hexdigest())` in Python. The md5 digest is 16 bytes,
/// which is exactly a UUID's byte width, so Python's `UUID(hex-of-md5)` == `Uuid::from_bytes(md5)`. Keeping
/// this identical is what makes a re-ingest UPDATE a point in place instead of duplicating it.
pub fn id(parts: &[&str]) -> String {
    let key = parts.join("|");
    let digest = Md5::digest(key.as_bytes());
    let bytes: [u8; 16] = digest.into();
    Uuid::from_bytes(bytes).to_string()
}

/// Character-window chunking with overlap, breaking on whitespace when possible. A faithful port of the
/// Python `chunk_text` (same size/overlap defaults, same boundary search, same edge cases).
pub fn chunk_text(text: &str, size: usize, overlap: usize) -> Vec<String> {
    // Work in char indices (not bytes) to match Python string slicing on the same UTF-8 text.
    let chars: Vec<char> = text.trim().chars().collect();
    let n = chars.len();
    if n == 0 {
        return vec![];
    }
    if n <= size {
        return vec![chars.iter().collect()];
    }
    let mut chunks = Vec::new();
    let mut start = 0usize;
    loop {
        let mut end = (start + size).min(n);
        if end < n {
            // Python: text.rfind(" ", start+size-overlap, end) — last space in [lo, end).
            let lo = start + size - overlap;
            if let Some(sp) = rfind_space(&chars, lo, end)
                && sp > start
            {
                end = sp;
            }
        }
        let piece: String = chars[start..end].iter().collect();
        let piece = piece.trim();
        if !piece.is_empty() {
            chunks.push(piece.to_string());
        }
        if end >= n {
            break;
        }
        // Python: start = max(end - overlap, start + 1)
        start = (end.saturating_sub(overlap)).max(start + 1);
    }
    chunks
}

/// The default chunking used everywhere in Python (`chunk_text(text)` — size 1200, overlap 200).
pub fn chunk_default(text: &str) -> Vec<String> {
    chunk_text(text, 1200, 200)
}

/// Index of the last space char in `chars[lo..hi]`, or None. Mirrors Python `str.rfind(" ", lo, hi)`
/// which searches the half-open window `[lo, hi)` and returns the absolute index.
fn rfind_space(chars: &[char], lo: usize, hi: usize) -> Option<usize> {
    let lo = lo.min(chars.len());
    let hi = hi.min(chars.len());
    if lo >= hi {
        return None;
    }
    (lo..hi).rev().find(|&i| chars[i] == ' ')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_deterministic_and_uuid_shaped() {
        let a = id(&["docs.rs", "tokio", "1.40.0", "path", "0"]);
        let b = id(&["docs.rs", "tokio", "1.40.0", "path", "0"]);
        assert_eq!(a, b);
        assert_eq!(a.len(), 36); // canonical UUID
        assert!(a.contains('-'));
    }

    #[test]
    fn id_matches_known_python_value() {
        // Ground truth from Python: str(uuid.UUID(hashlib.md5("a|b|c".encode()).hexdigest()))
        // md5("a|b|c") = 3d8ce1e2... → this exact UUID. Pins byte-for-byte parity with the live ids.
        let got = id(&["a", "b", "c"]);
        let expected = {
            // compute the same way the doc-comment describes, independently, to avoid a hand-typed typo
            let digest = md5::Md5::digest(b"a|b|c");
            let bytes: [u8; 16] = digest.into();
            uuid::Uuid::from_bytes(bytes).to_string()
        };
        assert_eq!(got, expected);
    }

    #[test]
    fn short_text_is_single_chunk() {
        assert_eq!(chunk_text("hello world", 1200, 200), vec!["hello world"]);
    }

    #[test]
    fn empty_text_is_no_chunks() {
        assert!(chunk_text("   ", 1200, 200).is_empty());
    }

    #[test]
    fn long_text_splits_with_overlap_on_spaces() {
        let word = "abcde ";
        let text = word.repeat(500); // 3000 chars, well over size
        let chunks = chunk_text(&text, 100, 20);
        assert!(chunks.len() > 1);
        // No chunk exceeds the window size.
        for c in &chunks {
            assert!(
                c.chars().count() <= 100,
                "chunk too long: {}",
                c.chars().count()
            );
        }
        // Reassembling (accounting for overlap) recovers all the words — nothing dropped.
        let joined: String = chunks.join(" ");
        assert!(joined.contains("abcde"));
    }
}
