//! `extract` — phase-2 ingest file discovery + text extraction. Port of the file-walk + PDF/text reading in
//! Python `kb/chunk.py` that was deferred out of phase 1 (see the `chunk` module header).
//!
//! Two entry points the inbox / pipeline workers (#236 / #238) build on:
//! - [`iter_files`] walks a drop-folder root and returns the ingestable files (a supported extension,
//!   hidden entries skipped) in a deterministic total order, so a re-ingest visits files the same way on
//!   every run — which, with the deterministic point id (`chunk::id`), keeps re-ingest idempotent.
//! - [`extract`] reads one file's text: a PDF yields one string PER PAGE (so a chunk's citation can carry a
//!   `#page=N` anchor — the `mcp::cite` path already appends one), a text/markdown file yields a single page.
//!
//! Page numbering is 1-based at the citation layer: page `N` is `Extracted::pages[N - 1]`.

// Ported ahead of its callers (the phase-2 ingest workers), so the helpers read as dead code until then.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use pdfium_render::prelude::*;
use walkdir::{DirEntry, WalkDir};

use crate::config;

/// Render scale for OCR: image-only pages are rendered at this factor of their point size before OCR, so the
/// rasterized glyphs are large enough for tesseract to read reliably. 2x is a good legibility/size balance.
const OCR_RENDER_SCALE: f32 = 2.0;

/// The extracted text of one source file, split into pages. A PDF has one entry per page (document order); a
/// text/markdown file has exactly one entry (the whole file). Empty when the file held no extractable text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extracted {
    pub pages: Vec<String>,
}

impl Extracted {
    /// True when there is no non-whitespace text on any page — the worker skips such files.
    pub fn is_empty(&self) -> bool {
        self.pages.iter().all(|p| p.trim().is_empty())
    }
}

/// The file kinds the ingest path understands. Anything else is not ingestable and [`iter_files`] drops it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// UTF-8 text read verbatim (txt / md / markdown / rst / text).
    Text,
    /// A PDF, extracted per page.
    Pdf,
    /// A .docx (OOXML WordprocessingML), extracted as one text unit.
    Docx,
}

/// Ingestable text extensions — the Python `chunk.DOC_EXT`. Read verbatim as UTF-8. (Deliberately NOT the
/// invented "text"/"markdown" — this matches the live worker's set exactly, incl. config/data formats.)
const DOC_EXT: &[&str] = &[
    "md", "txt", "rst", "cfg", "conf", "ini", "toml", "yaml", "yml", "json", "nix",
];

/// Directory names pruned during the walk — the Python `chunk.iter_files` skip substrings. Only these three
/// (not every dotfile): a `.env.md` or other dot-named FILE is still ingestable, matching the live worker.
const SKIP_DIRS: &[&str] = &[".git", "node_modules", "__pycache__"];

/// Classify a path by its extension (case-insensitive), or `None` if it isn't an ingestable kind. Matches
/// the Python default (`{.pdf} | DOC_EXT`); CODE_EXT is opt-in there and not ingested by the drop-folder.
fn classify(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if ext == "pdf" {
        Some(Kind::Pdf)
    } else if ext == "docx" {
        Some(Kind::Docx)
    } else if DOC_EXT.contains(&ext.as_str()) {
        Some(Kind::Text)
    } else {
        None
    }
}

/// A pruned directory below the walk root (`.git` / `node_modules` / `__pycache__`) — the Python skip set.
/// The root itself (depth 0) is never pruned, so pointing the walk at such a dir still works.
fn is_skipped(entry: &DirEntry) -> bool {
    entry.depth() > 0
        && entry
            .file_name()
            .to_str()
            .is_some_and(|s| SKIP_DIRS.contains(&s))
}

/// Discover every ingestable file under `root`, recursively — the Python `chunk.iter_files`. `.git` /
/// `node_modules` / `__pycache__` are pruned, non-files and unsupported extensions are dropped, and the
/// result is sorted for a deterministic total order regardless of the OS directory-read order. Unreadable
/// entries are skipped rather than failing the whole walk.
pub fn iter_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_skipped(e))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(DirEntry::into_path)
        .filter(|p| classify(p).is_some())
        .collect();
    files.sort();
    files
}

/// Yield each extractable unit as `(page, text)` — the Python `chunk.extract`. A text/data file is a single
/// unit with page `None`; a PDF yields one unit per 1-based page. The inbox worker keys chunks on
/// `(page, chunk-index)`, so the page numbering (and `None` for non-paginated text) must be preserved
/// exactly — `str(None) == "None"` lands in the point id.
pub fn extract_units(path: &Path) -> Result<Vec<(Option<i64>, String)>, String> {
    match classify(path) {
        Some(Kind::Text) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("extract: read {}: {e}", path.display()))?;
            Ok(vec![(None, text)])
        }
        Some(Kind::Pdf) => Ok(extract_pdf(path)?
            .into_iter()
            .enumerate()
            .map(|(i, text)| (Some(i as i64 + 1), text))
            .collect()),
        Some(Kind::Docx) => Ok(vec![(None, extract_docx(path)?)]),
        None => Err(format!(
            "extract: unsupported file type: {}",
            path.display()
        )),
    }
}

/// Extract a file's text as pages (page numbers dropped) — a thin view over [`extract_units`]. A text/data
/// file is one page; a PDF is per page.
pub fn extract(path: &Path) -> Result<Extracted, String> {
    Ok(Extracted {
        pages: extract_units(path)?.into_iter().map(|(_, t)| t).collect(),
    })
}

/// Extract a PDF's text per page via PDFium (pdfium-render, greenlit decision #1). Binds libpdfium at
/// runtime from the system library path (provided by the worker roles' LD_LIBRARY_PATH); a missing library
/// or an unreadable PDF is an error. A page whose text can't be read contributes an empty page rather than
/// failing the whole document.
fn extract_pdf(path: &Path) -> Result<Vec<String>, String> {
    let pdfium = Pdfium::new(Pdfium::bind_to_system_library().map_err(|e| {
        format!("extract: pdfium bind failed (is libpdfium on the library path?): {e}")
    })?);
    let doc = pdfium
        .load_pdf_from_file(path, None)
        .map_err(|e| format!("extract: pdf {}: {e}", path.display()))?;
    let min_chars = config::get().pdf_ocr_min_chars;
    Ok(doc
        .pages()
        .iter()
        .map(|page| page_text(&page, min_chars))
        .collect())
}

/// Extract a PDF's text per page from in-memory bytes — the bytes analogue of [`extract_pdf`], for the
/// pipeline embedder which cats a PDF's bytes back from IPFS (it has no file path). Same PDFium bind, per-page
/// text, and CRLF->LF normalization. A missing libpdfium or an unreadable byte stream is an error.
pub fn extract_pdf_bytes(data: &[u8]) -> Result<Vec<String>, String> {
    let pdfium = Pdfium::new(Pdfium::bind_to_system_library().map_err(|e| {
        format!("extract: pdfium bind failed (is libpdfium on the library path?): {e}")
    })?);
    let doc = pdfium
        .load_pdf_from_byte_slice(data, None)
        .map_err(|e| format!("extract: pdf from bytes ({} bytes): {e}", data.len()))?;
    let min_chars = config::get().pdf_ocr_min_chars;
    Ok(doc
        .pages()
        .iter()
        .map(|page| page_text(&page, min_chars))
        .collect())
}

/// Extract a .docx file's text — a thin wrapper over [`extract_docx_bytes`] that reads the file into memory
/// first. The whole document is one text unit (page `None`), like a plain-text file.
fn extract_docx(path: &Path) -> Result<String, String> {
    let data = std::fs::read(path).map_err(|e| format!("extract: read {}: {e}", path.display()))?;
    extract_docx_bytes(&data)
}

/// Extract a .docx's text from in-memory bytes — the bytes analogue of [`extract_docx`], for the pipeline
/// embedder which cats a docx's bytes back from IPFS (no file path). A .docx is a ZIP whose `word/document.xml`
/// holds the body; [`docx_xml_to_text`] pulls the readable text, CRLF-normalized like the PDF path. A non-zip
/// input or a missing `word/document.xml` is an error (surfaced, never a panic).
pub fn extract_docx_bytes(data: &[u8]) -> Result<String, String> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data)).map_err(|e| {
        format!(
            "extract: docx is not a valid zip ({} bytes): {e}",
            data.len()
        )
    })?;
    let mut xml = String::new();
    zip.by_name("word/document.xml")
        .map_err(|e| format!("extract: docx has no word/document.xml: {e}"))?
        .read_to_string(&mut xml)
        .map_err(|e| format!("extract: docx read document.xml: {e}"))?;
    Ok(normalize_newlines(&docx_xml_to_text(&xml)))
}

/// Pull readable text from a WordprocessingML `word/document.xml`: text lives in `<w:t>` runs; a paragraph
/// close (`</w:p>`) and `<w:br>`/`<w:cr>` become newlines, `<w:tab/>` a tab, and every other tag is dropped.
/// XML entities inside runs are unescaped ([`xml_unescape`]). A deliberately small hand parser (so the crate
/// takes no XML dependency) matched to the one structure Word emits — it assumes the standard `w:` text
/// namespace prefix. Pure; unit-tested.
fn docx_xml_to_text(xml: &str) -> String {
    let mut out = String::new();
    let mut rest = xml;
    while let Some(lt) = rest.find('<') {
        let after = &rest[lt + 1..];
        let Some(gt) = after.find('>') else { break };
        let tag = after[..gt].trim();
        let is_close = tag.starts_with('/');
        let is_self = tag.ends_with('/');
        let name = tag
            .trim_start_matches('/')
            .trim_end_matches('/')
            .split([' ', '\t', '\r', '\n'])
            .next()
            .unwrap_or("");
        // Absolute byte offset of the first char AFTER this tag's '>'.
        let past_tag = lt + 1 + gt + 1;
        if name == "w:t" && !is_close && !is_self {
            // Text run: take the char data up to the matching </w:t>.
            if let Some(close) = rest[past_tag..].find("</w:t>") {
                out.push_str(&xml_unescape(&rest[past_tag..past_tag + close]));
                rest = &rest[past_tag + close + "</w:t>".len()..];
                continue;
            }
            break; // unterminated run (malformed) — stop rather than spin
        }
        match name {
            "w:tab" => out.push('\t'),
            "w:br" | "w:cr" => out.push('\n'),
            "w:p" if is_close => out.push('\n'),
            _ => {}
        }
        rest = &rest[past_tag..];
    }
    out
}

/// Unescape the five XML predefined entities plus numeric (`&#NN;` / `&#xHH;`) character references in run
/// text; an unknown entity is left verbatim. Pure; unit-tested.
fn xml_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let Some(semi) = tail.find(';') else {
            out.push_str(tail);
            return out;
        };
        let ent = &tail[1..semi];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                u32::from_str_radix(&ent[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => out.push(c),
            None => out.push_str(&tail[..=semi]), // unknown entity: keep "&...;" verbatim
        }
        rest = &tail[semi + 1..];
    }
    out.push_str(rest);
    out
}

/// Count of non-whitespace characters — the "how much real text is on this page" measure the OCR threshold
/// keys on (whitespace-only extraction from an image-only page scores 0).
fn nonws(s: &str) -> usize {
    s.chars().filter(|c| !c.is_whitespace()).count()
}

/// OCR-trigger garbage threshold: a PDF page whose extracted content is more than this FRACTION non-text
/// "junk" is treated as an image/binary-stream page and becomes an OCR candidate even when it has plenty of
/// characters. Set conservatively — real document text in ANY language is dominated by alphanumeric +
/// punctuation + whitespace, so legitimate prose scores far below this and never trips it; the safe failure
/// mode is a MISS (unchanged behavior), never a false OCR of real text. This catches the BINARY-junk class of
/// image-only pages (the sparse-only check missed them — see task_40 comment_1922, docs.ldo_awd). The harder
/// printable-but-high-entropy case (decoded bytes that happen to land in printable ASCII, e.g. "igmJ,di4>i$G")
/// is NOT separable from real prose by this ratio and is deferred to the fixture-calibrated deployment-side slice,
/// where the real docs.ldo_awd / Rapido-Plus page text is available to tune against.
const OCR_GARBAGE_RATIO: f32 = 0.35;

/// Fraction of a page's NON-WHITESPACE characters that are "junk" — neither alphanumeric (Unicode-aware, so
/// accented and CJK letters and digits all count as real text) nor ASCII punctuation. Control chars, the
/// U+FFFD replacement char, and stray symbol/other-category codepoints — the hallmark of a decoded
/// image/binary stream — count as junk. Whitespace is ignored (neither signal nor junk). Returns 0.0 for an
/// all-whitespace or empty page (the sparse check owns those). Pure; unit-tested.
fn junk_ratio(s: &str) -> f32 {
    let mut content = 0usize;
    let mut junk = 0usize;
    for c in s.chars() {
        if c.is_whitespace() {
            continue;
        }
        content += 1;
        if !(c.is_alphanumeric() || c.is_ascii_punctuation()) {
            junk += 1;
        }
    }
    if content == 0 {
        return 0.0;
    }
    junk as f32 / content as f32
}

/// Whether a page's extracted text looks image-only and should be OCR'd. OCR must be enabled
/// (`min_chars > 0`; `0` disables it — the default — so this is always false then), AND the page is EITHER:
///  - sparse: fewer than `min_chars` non-whitespace characters (an image-only page yields ~nothing), OR
///  - garbage: it has enough characters but more than [`OCR_GARBAGE_RATIO`] of its content chars are non-text
///    junk (a decoded image/binary stream — many chars, but not real text), which the sparse check misses.
///
/// Pure; unit-tested.
fn page_needs_ocr(text: &str, min_chars: usize) -> bool {
    if min_chars == 0 {
        return false;
    }
    nonws(text) < min_chars || junk_ratio(text) > OCR_GARBAGE_RATIO
}

/// One page's text: the embedded text (CRLF-normalized), except a page that looks image-only
/// ([`page_needs_ocr`]) is rendered + OCR'd, and the OCR result is used when it recovers MORE non-whitespace
/// text than the sparse embedded layer. OCR is best-effort ([`ocr_page`] returns `None` on any failure ->
/// fall back to the embedded text). With `min_chars == 0` (the default) this is exactly the pre-OCR behavior.
fn page_text(page: &PdfPage, min_chars: usize) -> String {
    let embedded = normalize_newlines(&page.text().map(|t| t.all()).unwrap_or_default());
    if !page_needs_ocr(&embedded, min_chars) {
        return embedded;
    }
    match ocr_page(page) {
        Some(ocr) if nonws(&ocr) > nonws(&embedded) => ocr,
        _ => embedded,
    }
}

/// Render a page to an image and OCR it with the `tesseract` CLI (task_40). Best-effort: returns `None` on any
/// failure (tesseract not on PATH, render/encode/spawn error, non-zero exit, or empty output) so the caller
/// keeps the embedded text. Renders at [`OCR_RENDER_SCALE`], PNG-encodes, and pipes the image to
/// `tesseract stdin stdout`. Blocking subprocess — the PDF extractors already run under `spawn_blocking`.
fn ocr_page(page: &PdfPage) -> Option<String> {
    let render = PdfRenderConfig::new().scale_page_by_factor(OCR_RENDER_SCALE);
    let bitmap = page.render_with_config(&render).ok()?;
    let img = bitmap.as_image().ok()?;
    let mut png: Vec<u8> = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    let mut child = Command::new("tesseract")
        .args(["stdin", "stdout", "-l", "eng"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Write the PNG to stdin, then drop the handle so tesseract sees EOF and proceeds.
    child.stdin.take()?.write_all(&png).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = normalize_newlines(String::from_utf8_lossy(&out.stdout).trim());
    (!text.is_empty()).then_some(text)
}

/// Normalize line endings to `\n` — PDFium's text extraction emits `\r\n` (and can emit lone `\r`), whereas
/// the rest of the KB corpus (and the Python pymupdf path) uses `\n`. Normalizing keeps ingested PDF text
/// consistent with every other source and removes one source of pdfium-vs-pymupdf drift if an existing
/// pymupdf-built PDF collection is ever re-ingested (board task #468). Order matters: collapse `\r\n` first,
/// then map any remaining lone `\r`.
fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A throwaway temp dir unique to one test; removed on drop.
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new() -> TmpDir {
            let p =
                std::env::temp_dir().join(format!("kb-extract-{}", uuid::Uuid::new_v4().simple()));
            fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
        fn write(&self, rel: &str, body: &str) -> PathBuf {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, body).unwrap();
            p
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn classify_matches_doc_ext_and_pdf() {
        assert_eq!(classify(Path::new("a.md")), Some(Kind::Text));
        assert_eq!(classify(Path::new("a.TOML")), Some(Kind::Text)); // case-insensitive, real DOC_EXT
        assert_eq!(classify(Path::new("a.nix")), Some(Kind::Text));
        assert_eq!(classify(Path::new("a.PDF")), Some(Kind::Pdf));
        // Not in DOC_EXT: the invented "markdown" alias and CODE_EXT are not ingested by default.
        assert_eq!(classify(Path::new("a.markdown")), None);
        assert_eq!(classify(Path::new("a.rs")), None);
        assert_eq!(classify(Path::new("a.png")), None);
        assert_eq!(classify(Path::new("noext")), None);
    }

    #[test]
    fn iter_files_prunes_git_but_keeps_dot_named_files_sorted() {
        let d = TmpDir::new();
        d.write("b.md", "b");
        d.write("a.txt", "a");
        d.write("nested/c.pdf", "not a real pdf");
        d.write("skip.png", "img"); // unsupported extension
        d.write(".env.md", "dotfile but ingestable"); // dot-named FILE is NOT skipped (only .git/etc dirs)
        d.write(".git/config.md", "vcs"); // under .git -> pruned
        d.write("node_modules/pkg.md", "dep"); // under node_modules -> pruned
        let got = iter_files(&d.0);
        let rel: Vec<String> = got
            .iter()
            .map(|p| {
                p.strip_prefix(&d.0)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(rel, vec![".env.md", "a.txt", "b.md", "nested/c.pdf"]);
    }

    #[test]
    fn extract_text_is_single_page_none() {
        let d = TmpDir::new();
        let p = d.write("note.md", "# hello\nworld");
        let got = extract(&p).unwrap();
        assert_eq!(got.pages, vec!["# hello\nworld"]);
        assert!(!got.is_empty());
        // extract_units tags a text file as one unit with page None (str(None) -> "None" in the id key).
        assert_eq!(
            extract_units(&p).unwrap(),
            vec![(None, "# hello\nworld".to_string())]
        );
    }

    #[test]
    fn extract_unsupported_is_error() {
        let d = TmpDir::new();
        let p = d.write("x.png", "img");
        assert!(extract(&p).is_err());
    }

    #[test]
    fn extract_invalid_pdf_errors_gracefully() {
        let d = TmpDir::new();
        let p = d.write("bad.pdf", "this is not a pdf");
        // Must surface an Err, not panic.
        assert!(extract(&p).is_err());
    }

    #[test]
    fn is_empty_detects_whitespace_only() {
        assert!(
            Extracted {
                pages: vec!["   \n".into()]
            }
            .is_empty()
        );
        assert!(Extracted { pages: vec![] }.is_empty());
        assert!(
            !Extracted {
                pages: vec!["x".into()]
            }
            .is_empty()
        );
    }

    #[test]
    fn normalize_newlines_maps_crlf_and_lone_cr_to_lf() {
        // CRLF -> LF, lone CR -> LF, existing LF untouched, no doubling.
        assert_eq!(normalize_newlines("a\r\nb\rc\nd"), "a\nb\nc\nd");
        assert_eq!(normalize_newlines("no breaks"), "no breaks");
        assert_eq!(normalize_newlines("trailing\r\n"), "trailing\n");
        // A bare \r\n does not become \n\n.
        assert_eq!(normalize_newlines("x\r\ny"), "x\ny");
    }

    #[test]
    fn extract_pdf_bytes_errors_gracefully_on_non_pdf() {
        // Non-PDF bytes must surface an Err (bad load or missing libpdfium), never panic.
        assert!(extract_pdf_bytes(b"this is not a pdf").is_err());
    }

    #[test]
    fn page_needs_ocr_only_when_enabled_and_sparse() {
        // Disabled (0) never triggers OCR -> the default is a pure no-op / pre-OCR behavior.
        assert!(!page_needs_ocr("", 0));
        assert!(!page_needs_ocr("plenty of text here", 0));
        // Even an all-junk page is NOT flagged while OCR is disabled (min_chars == 0).
        assert!(!page_needs_ocr(&"\u{FFFD}".repeat(50), 0));
        // Enabled: fewer than min_chars NON-WHITESPACE chars -> image-only candidate.
        assert!(page_needs_ocr("   \n  \t", 5)); // 0 non-ws < 5 (image-only page yields ~whitespace)
        assert!(page_needs_ocr("ab", 5)); // 2 < 5
        assert!(!page_needs_ocr("abcde", 5)); // 5 is NOT < 5 (boundary)
        assert!(!page_needs_ocr("a b c d e f", 5)); // 6 non-ws (spaces ignored) >= 5
    }

    #[test]
    fn page_needs_ocr_flags_binary_garbage_not_just_sparse() {
        // NOT sparse (plenty of non-ws chars) but mostly non-text junk -> a decoded image/binary stream page
        // the sparse-only check missed (task_40 comment_1922). min_chars=10, so these clear the sparse gate.
        assert!(page_needs_ocr(&"\u{FFFD}".repeat(50), 10)); // decoded-bytes replacement chars
        let soup = "\u{1}\u{2}\u{3}\u{7f}\u{80}\u{9c}\u{ad}\u{feff}".repeat(10); // control/other-category soup
        assert!(page_needs_ocr(&soup, 10));
        // Real prose with digits, punctuation, and ASCII operators is NOT flagged (junk ratio ~0).
        assert!(!page_needs_ocr(
            "The Rapido Plus hotend ships with a 60W heater cartridge (24V). Torque: 1.2 N*m.",
            10
        ));
        // Non-English / accented prose is real text (Unicode-aware alphanumeric), so the degree/middle-dot
        // symbols stay a tiny minority -> not flagged.
        assert!(!page_needs_ocr(
            "Fixez la t\u{ea}te \u{e0} 1,2 N\u{b7}m; temp\u{e9}rature max 300 \u{b0}C.",
            10
        ));
    }

    #[test]
    fn junk_ratio_separates_text_from_binary() {
        assert_eq!(junk_ratio(""), 0.0);
        assert_eq!(junk_ratio("   \n\t "), 0.0); // whitespace-only -> no content chars -> 0
        assert!(junk_ratio("plain english text") < 0.05);
        assert!(junk_ratio("digits 123 and punct .,;:!?()[]{}") < 0.05);
        assert!(junk_ratio(&"\u{FFFD}".repeat(20)) > 0.9); // all replacement chars -> ~all junk
    }

    /// Build a minimal .docx (a ZIP with one `word/document.xml`) around a WordML `<w:body>` fragment. Stored
    /// (uncompressed) so the test needs no deflate feature; the reader handles stored + deflated alike.
    fn docx_with(body: &str) -> Vec<u8> {
        use std::io::{Cursor, Write};
        let xml = format!(
            "<?xml version=\"1.0\"?><w:document xmlns:w=\"x\"><w:body>{body}</w:body></w:document>"
        );
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zw.start_file("word/document.xml", opts).unwrap();
            zw.write_all(xml.as_bytes()).unwrap();
            let _ = zw.finish().unwrap();
        }
        buf
    }

    #[test]
    fn classify_recognizes_docx() {
        assert_eq!(classify(Path::new("a.docx")), Some(Kind::Docx));
        assert_eq!(classify(Path::new("A.DOCX")), Some(Kind::Docx)); // case-insensitive
        assert_eq!(classify(Path::new("a.doc")), None); // legacy binary .doc is NOT supported
    }

    #[test]
    fn extract_docx_bytes_pulls_runs_and_paragraph_breaks() {
        let docx = docx_with(
            "<w:p><w:r><w:t>Hello</w:t></w:r><w:r><w:t xml:space=\"preserve\"> world</w:t></w:r></w:p>\
             <w:p><w:r><w:t>Line &amp; two</w:t></w:r></w:p>",
        );
        // Two paragraphs: runs concatenate, each </w:p> is a line break; the &amp; entity is unescaped.
        assert_eq!(
            extract_docx_bytes(&docx).unwrap(),
            "Hello world\nLine & two\n"
        );
    }

    #[test]
    fn extract_docx_bytes_handles_tab_and_break() {
        let docx =
            docx_with("<w:p><w:r><w:t>a</w:t><w:tab/><w:t>b</w:t><w:br/><w:t>c</w:t></w:r></w:p>");
        assert_eq!(extract_docx_bytes(&docx).unwrap(), "a\tb\nc\n");
    }

    #[test]
    fn extract_docx_bytes_errors_on_non_zip() {
        // Surfaces an Err (never panics) for a leading-PK non-zip and for arbitrary bytes.
        assert!(extract_docx_bytes(b"PK not really a zip").is_err());
        assert!(extract_docx_bytes(b"totally not a docx").is_err());
    }

    #[test]
    fn extract_docx_bytes_errors_when_no_document_xml() {
        use std::io::{Cursor, Write};
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zw.start_file("other.xml", opts).unwrap();
            zw.write_all(b"<x/>").unwrap();
            let _ = zw.finish().unwrap();
        }
        // A valid zip but without word/document.xml -> error, not panic.
        assert!(extract_docx_bytes(&buf).is_err());
    }

    #[test]
    fn extract_units_docx_is_single_page_none() {
        let d = TmpDir::new();
        let p = d.0.join("doc.docx");
        fs::write(&p, docx_with("<w:p><w:r><w:t>body text</w:t></w:r></w:p>")).unwrap();
        // docx is one unit with page None (like a text file), str(None) -> "None" in the id key.
        assert_eq!(
            extract_units(&p).unwrap(),
            vec![(None, "body text\n".to_string())]
        );
    }

    #[test]
    fn xml_unescape_decodes_named_and_numeric() {
        assert_eq!(
            xml_unescape("a&amp;b&lt;c&gt;d&quot;e&apos;f"),
            "a&b<c>d\"e'f"
        );
        assert_eq!(xml_unescape("&#65;&#x42;"), "AB"); // decimal + hex char refs
        assert_eq!(xml_unescape("no entities"), "no entities");
        assert_eq!(
            xml_unescape("keep &unknown; verbatim"),
            "keep &unknown; verbatim"
        );
    }
}
