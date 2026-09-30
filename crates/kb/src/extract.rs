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

use std::path::{Path, PathBuf};

use pdfium_render::prelude::*;
use walkdir::{DirEntry, WalkDir};

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
}

/// Classify a path by its extension (case-insensitive), or `None` if it isn't an ingestable kind.
fn classify(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "txt" | "text" | "md" | "markdown" | "rst" => Some(Kind::Text),
        "pdf" => Some(Kind::Pdf),
        _ => None,
    }
}

/// A hidden entry (dotfile or dot-directory) below the walk root — `.git`, `.DS_Store`, etc. The root itself
/// (depth 0) is never treated as hidden, so pointing the walk at a dot-named drop folder still works.
fn is_hidden(entry: &DirEntry) -> bool {
    entry.depth() > 0
        && entry
            .file_name()
            .to_str()
            .is_some_and(|s| s.starts_with('.'))
}

/// Discover every ingestable file under `root`, recursively. Hidden entries are pruned (so we never descend
/// into `.git`), non-files and unsupported extensions are dropped, and the result is sorted for a
/// deterministic total order regardless of the OS directory-read order. Unreadable entries are skipped
/// rather than failing the whole walk.
pub fn iter_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_hidden(e))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(DirEntry::into_path)
        .filter(|p| classify(p).is_some())
        .collect();
    files.sort();
    files
}

/// Extract a file's text. Text/markdown is read as UTF-8 (one page); a PDF is extracted per page. An
/// unsupported extension is an error — callers should only pass paths that came from [`iter_files`].
pub fn extract(path: &Path) -> Result<Extracted, String> {
    match classify(path) {
        Some(Kind::Text) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("extract: read {}: {e}", path.display()))?;
            Ok(Extracted { pages: vec![text] })
        }
        Some(Kind::Pdf) => Ok(Extracted {
            pages: extract_pdf(path)?,
        }),
        None => Err(format!(
            "extract: unsupported file type: {}",
            path.display()
        )),
    }
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
    Ok(doc
        .pages()
        .iter()
        .map(|page| page.text().map(|t| t.all()).unwrap_or_default())
        .collect())
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
    fn classify_by_extension_case_insensitive() {
        assert_eq!(classify(Path::new("a.md")), Some(Kind::Text));
        assert_eq!(classify(Path::new("a.MARKDOWN")), Some(Kind::Text));
        assert_eq!(classify(Path::new("a.PDF")), Some(Kind::Pdf));
        assert_eq!(classify(Path::new("a.png")), None);
        assert_eq!(classify(Path::new("noext")), None);
    }

    #[test]
    fn iter_files_finds_supported_skips_hidden_and_unsupported_sorted() {
        let d = TmpDir::new();
        d.write("b.md", "b");
        d.write("a.txt", "a");
        d.write("nested/c.pdf", "not a real pdf");
        d.write("skip.png", "img");
        d.write(".hidden.md", "secret");
        d.write(".git/config.md", "vcs");
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
        assert_eq!(rel, vec!["a.txt", "b.md", "nested/c.pdf"]);
    }

    #[test]
    fn extract_text_is_single_page() {
        let d = TmpDir::new();
        let p = d.write("note.md", "# hello\nworld");
        let got = extract(&p).unwrap();
        assert_eq!(got.pages, vec!["# hello\nworld"]);
        assert!(!got.is_empty());
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
}
