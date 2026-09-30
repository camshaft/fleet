//! `ipfs` — a thin async client for a Kubo (go-ipfs) node's HTTP RPC API. Port of the Python KB inbox
//! worker's IPFS pinning, which drove the Kubo `/api/v0/*` RPC over `requests`.
//!
//! Phase-2 infra (#233): the inbox worker pins each ingested file into IPFS and records its CID so a
//! citation can carry an `ipfs://<cid>/<name>` URL (the `mcp::cite` path already consumes an `ipfs_url`
//! payload field). Three operations mirror the Python: [`Ipfs::add`] (pin a file on disk), [`Ipfs::add_bytes`]
//! (pin in-memory bytes), and [`Ipfs::cat`] (fetch content back by CID).
//!
//! IO is async `reqwest` — operator directive: no blocking IO on the tokio runtime (#439). The Kubo RPC is
//! POST-only and takes uploads as `multipart/form-data`; rather than enable reqwest's `multipart` feature we
//! hand-build the single-part body in [`multipart_body`] (a pure, unit-testable function) and send it as a
//! raw body with the boundary Content-Type.

use std::path::Path;

use serde_json::Value;

/// A handle to a Kubo node's HTTP RPC (`/api/v0`). Stateless — each call is one request, matching `store`.
pub struct Ipfs {
    base: String,
    http: reqwest::Client,
}

/// The result of pinning content: the object Kubo reports from `/api/v0/add`. `cid` is Kubo's `Hash` (the
/// content id); `size` is its `Size` string (Kubo reports it as a decimal string, not a number).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Added {
    pub name: String,
    pub cid: String,
    pub size: String,
}

impl Ipfs {
    /// Build a client for the configured `ipfs_url` (the Kubo RPC base, e.g. `http://127.0.0.1:5001`).
    pub fn connect() -> Ipfs {
        Ipfs {
            base: crate::config::get()
                .ipfs_url
                .trim_end_matches('/')
                .to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// Pin a file from disk — read its bytes and delegate to [`add_bytes`](Ipfs::add_bytes), using the file's
    /// own name as the multipart filename (Kubo echoes it back as `Name`). The Python `ipfs_add(path)`.
    pub async fn add(&self, path: &Path) -> Result<Added, String> {
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| format!("ipfs add: read {}: {e}", path.display()))?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        self.add_bytes(&name, &bytes).await
    }

    /// Pin in-memory bytes under a logical `name` — `POST /api/v0/add`. Kubo pins added content by default,
    /// so no explicit pin call is needed. Returns the reported name/CID/size. The Python `ipfs_add_bytes`.
    pub async fn add_bytes(&self, name: &str, bytes: &[u8]) -> Result<Added, String> {
        let boundary = format!("kbboundary{}", uuid::Uuid::new_v4().simple());
        let body = multipart_body(&boundary, name, bytes);
        let url = format!("{}/api/v0/add", self.base);
        let text = self
            .http
            .post(&url)
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("ipfs POST /api/v0/add failed: {e}"))?
            .text()
            .await
            .map_err(|e| format!("ipfs add: read response: {e}"))?;
        parse_add_response(&text)
    }

    /// Fetch content back by CID — `POST /api/v0/cat?arg=<cid>`, returning the raw bytes. The Python
    /// `ipfs_cat(cid)`.
    pub async fn cat(&self, cid: &str) -> Result<Vec<u8>, String> {
        let url = format!("{}/api/v0/cat", self.base);
        let bytes = self
            .http
            .post(&url)
            .query(&[("arg", cid)])
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("ipfs POST /api/v0/cat?arg={cid} failed: {e}"))?
            .bytes()
            .await
            .map_err(|e| format!("ipfs cat: read body for {cid}: {e}"))?;
        Ok(bytes.to_vec())
    }
}

/// Build the `multipart/form-data` request body for a single `file` part. The boundary is chosen by the
/// caller (see [`Ipfs::add_bytes`]) and must not occur in `bytes`; a random boundary makes that collision
/// astronomically unlikely. The filename is sanitized so a name containing a quote or newline can't break
/// out of the `Content-Disposition` header — the on-wire filename Kubo echoes as `Name`.
fn multipart_body(boundary: &str, name: &str, bytes: &[u8]) -> Vec<u8> {
    let safe_name: String = name
        .chars()
        .map(|c| match c {
            '"' | '\r' | '\n' => '_',
            other => other,
        })
        .collect();
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{safe_name}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// Parse Kubo's `/api/v0/add` response. Kubo streams newline-delimited JSON objects (one per file plus the
/// wrapping dir); for a single file there is one object. We take the LAST non-empty line — for a wrapped add
/// that is the top-level entry — and read its `Name`/`Hash`/`Size`. A missing `Hash` is an error.
fn parse_add_response(text: &str) -> Result<Added, String> {
    let last = text
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .ok_or_else(|| "ipfs add: empty response".to_string())?;
    let v: Value =
        serde_json::from_str(last).map_err(|e| format!("ipfs add: bad JSON {last:?}: {e}"))?;
    let cid = v
        .get("Hash")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("ipfs add: response has no Hash: {last:?}"))?
        .to_string();
    let name = v
        .get("Name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // Kubo reports Size as a decimal string; keep it as-is (accept a bare number too, defensively).
    let size = match v.get("Size") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    Ok(Added { name, cid, size })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multipart_body_has_boundary_disposition_and_payload() {
        let body = multipart_body("BND", "a.pdf", b"hello");
        let s = String::from_utf8(body).unwrap();
        assert!(s.starts_with("--BND\r\n"), "{s:?}");
        assert!(
            s.contains("Content-Disposition: form-data; name=\"file\"; filename=\"a.pdf\"\r\n"),
            "{s:?}"
        );
        assert!(s.contains("\r\n\r\nhello\r\n--BND--\r\n"), "{s:?}");
    }

    #[test]
    fn multipart_filename_cannot_break_the_header() {
        let body = multipart_body("BND", "ev\"il\r\n.pdf", b"x");
        let s = String::from_utf8(body).unwrap();
        assert!(s.contains("filename=\"ev_il__.pdf\""), "{s:?}");
        // Exactly one CRLFCRLF (the header/body separator) — the injected CRLF was neutralized.
        assert_eq!(s.matches("\r\n\r\n").count(), 1, "{s:?}");
    }

    #[test]
    fn parse_add_single_object() {
        let a = parse_add_response(r#"{"Name":"a.pdf","Hash":"QmABC","Size":"12"}"#).unwrap();
        assert_eq!(
            a,
            Added {
                name: "a.pdf".into(),
                cid: "QmABC".into(),
                size: "12".into()
            }
        );
    }

    #[test]
    fn parse_add_takes_last_line_of_ndjson() {
        // A wrapped add streams the file entry then the top-level dir; the last line is the top entry.
        let text = "{\"Name\":\"a.pdf\",\"Hash\":\"QmFile\",\"Size\":\"5\"}\n\
                    {\"Name\":\"\",\"Hash\":\"QmRoot\",\"Size\":\"70\"}\n";
        let a = parse_add_response(text).unwrap();
        assert_eq!(a.cid, "QmRoot");
    }

    #[test]
    fn parse_add_numeric_size_is_accepted() {
        let a = parse_add_response(r#"{"Name":"x","Hash":"Qm1","Size":99}"#).unwrap();
        assert_eq!(a.size, "99");
    }

    #[test]
    fn parse_add_missing_hash_is_error() {
        assert!(parse_add_response(r#"{"Name":"x","Size":"1"}"#).is_err());
        assert!(parse_add_response("").is_err());
    }
}
