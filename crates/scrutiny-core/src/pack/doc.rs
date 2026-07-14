use anyhow::Result;
use std::path::Path;

use crate::config::PackConfig;

use super::{show_file, DocDigest};

pub(crate) fn build_doc_digest(
    root: &Path,
    head: &str,
    path: &str,
    cfg: &PackConfig,
) -> Result<DocDigest> {
    let text = show_file(root, head, path).unwrap_or_default();
    let mut headings = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('#') {
            headings.push(t.to_string());
            if headings.len() >= 30 {
                break;
            }
        }
    }
    let preview: String = text
        .lines()
        .take(cfg.doc_digest_lines)
        .collect::<Vec<_>>()
        .join("\n");
    Ok(DocDigest {
        path: path.to_string(),
        headings,
        preview,
    })
}
