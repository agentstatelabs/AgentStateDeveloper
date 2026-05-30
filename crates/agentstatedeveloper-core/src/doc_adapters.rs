//! Document adapters: produce [`SearchDoc`] chunks from non-code files.
//!
//! Each adapter takes a file path + raw content and returns `Vec<SearchDoc>`.
//! Adapters are intentionally thin — no deep structural parsing, just enough
//! text extraction to make files discoverable in `asd search`.
//!
//! Supported file types:
//!   - Markdown (.md, .markdown) — each H1/H2/H3 heading becomes a chunk
//!   - Config (JSON, TOML, YAML) — key-path flattening for leaf values
//!   - HTML (.html, .htm) — strip tags, extract title + heading text
//!   - CSS (.css) — extract selector names and custom property names
//!   - Manifests (Package.swift, Cargo.toml, pubspec.yaml, package.json) —
//!     extract name + description + top-level keys
//!   - Build scripts (Makefile, Fastfile, Podfile, Gemfile) — extract target/lane names

use std::path::Path;

use crate::search_fts::{DocKind, SearchDoc};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Detect the kind for a file path and, if recognised, parse it into chunks.
/// Returns `None` when the file is not handled by any adapter.
pub fn adapt_document(path: &Path, content: &str) -> Option<Vec<SearchDoc>> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    // Manifests — check by specific filename before extension.
    let is_manifest = matches!(
        name.as_str(),
        "package.swift"
            | "cargo.toml"
            | "pubspec.yaml"
            | "pubspec.yml"
            | "package.json"
            | "package-lock.json"
            | "podfile"
            | "gemfile"
            | "makefile"
            | "fastfile"
            | "appfile"
            | "deliverfile"
    );
    if is_manifest {
        let kind = if matches!(
            name.as_str(),
            "makefile" | "fastfile" | "podfile" | "gemfile"
        ) {
            DocKind::BuildScript
        } else {
            DocKind::Manifest
        };
        return Some(adapt_manifest(path, content, kind));
    }

    match ext.as_str() {
        "md" | "markdown" => Some(adapt_markdown(path, content)),
        "json" => Some(adapt_json(path, content)),
        "toml" => Some(adapt_toml(path, content)),
        "yaml" | "yml" => Some(adapt_yaml(path, content)),
        "html" | "htm" => Some(adapt_html(path, content)),
        "css" => Some(adapt_css(path, content)),
        _ => None,
    }
}

/// Returns true if this path should be offered to document adapters.
/// Used to quickly gate file walking before reading content.
pub fn is_doc_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    matches!(
        name.as_str(),
        "package.swift"
            | "cargo.toml"
            | "pubspec.yaml"
            | "pubspec.yml"
            | "package.json"
            | "makefile"
            | "fastfile"
            | "podfile"
            | "gemfile"
    ) || matches!(
        ext.as_str(),
        "md" | "markdown" | "json" | "toml" | "yaml" | "yml" | "html" | "htm" | "css"
    )
}

// ---------------------------------------------------------------------------
// Markdown adapter
// ---------------------------------------------------------------------------

fn adapt_markdown(path: &Path, content: &str) -> Vec<SearchDoc> {
    let path_str = path.to_string_lossy();
    let mut docs: Vec<SearchDoc> = Vec::new();
    let mut current_title = String::new();
    let mut current_body: Vec<&str> = Vec::new();
    let mut current_line: u32 = 0;
    let mut chunk_start: u32 = 1;

    let flush = |title: &str, body: &[&str], start: u32, docs: &mut Vec<SearchDoc>| {
        let body_text = body.join(" ").trim().to_string();
        if !title.is_empty() || !body_text.is_empty() {
            docs.push(SearchDoc::new(
                DocKind::Markdown,
                path_str.as_ref(),
                Some(start),
                title,
                body_text,
            ));
        }
    };

    for line in content.lines() {
        current_line += 1;
        let trimmed = line.trim();

        if trimmed.starts_with("# ") || trimmed.starts_with("## ") || trimmed.starts_with("### ") {
            // Flush previous chunk.
            flush(&current_title, &current_body, chunk_start, &mut docs);
            current_title = trimmed.trim_start_matches('#').trim().to_string();
            current_body = Vec::new();
            chunk_start = current_line;
        } else if !trimmed.is_empty() && !trimmed.starts_with("```") {
            current_body.push(line);
        }
    }
    // Flush last chunk.
    flush(&current_title, &current_body, chunk_start, &mut docs);

    // If no headings found, emit a single whole-file chunk using the filename as title.
    if docs.is_empty() {
        let title = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        docs.push(SearchDoc::new(
            DocKind::Markdown,
            path_str.as_ref(),
            None,
            title,
            content.chars().take(2000).collect::<String>(),
        ));
    }
    docs
}

// ---------------------------------------------------------------------------
// JSON adapter
// ---------------------------------------------------------------------------

fn adapt_json(path: &Path, content: &str) -> Vec<SearchDoc> {
    let path_str = path.to_string_lossy();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    // For small JSON files (≤4KB) emit a single chunk with flattened key paths.
    let body = flatten_json_text(content, 3);
    if body.is_empty() {
        return vec![];
    }
    vec![SearchDoc::new(
        DocKind::Config,
        path_str.as_ref(),
        None,
        name,
        body,
    )]
}

/// Flatten up to `max_depth` levels of a JSON object into "key: value" lines.
/// Avoids serde_json entirely — simple line-based heuristic for discovery text.
fn flatten_json_text(content: &str, _max_depth: usize) -> String {
    // Strip string values, keep keys + short scalar values for searchability.
    content
        .lines()
        .map(|l| l.trim())
        .filter(|l| l.contains(':') || l.contains('"'))
        .take(100)
        .map(|l| l.trim_matches(|c: char| c == ',' || c == '{' || c == '}' || c == '[' || c == ']'))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(2000)
        .collect()
}

// ---------------------------------------------------------------------------
// TOML adapter
// ---------------------------------------------------------------------------

fn adapt_toml(path: &Path, content: &str) -> Vec<SearchDoc> {
    let path_str = path.to_string_lossy();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    // Emit key = value lines as body; section headers ([section]) become subchunks.
    let mut docs: Vec<SearchDoc> = Vec::new();
    let mut current_section = name.clone();
    let mut body_lines: Vec<&str> = Vec::new();
    let mut chunk_start: u32 = 0;
    let mut line_no: u32 = 0;

    for line in content.lines() {
        line_no += 1;
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let section_body = body_lines.join(" ");
            if !section_body.trim().is_empty() {
                docs.push(SearchDoc::new(
                    DocKind::Manifest,
                    path_str.as_ref(),
                    Some(chunk_start),
                    &current_section,
                    section_body,
                ));
            }
            current_section = format!(
                "{} {}",
                name,
                trimmed.trim_matches(|c| c == '[' || c == ']')
            );
            body_lines = Vec::new();
            chunk_start = line_no;
        } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
            body_lines.push(line);
        }
    }
    let section_body = body_lines.join(" ");
    if !section_body.trim().is_empty() {
        docs.push(SearchDoc::new(
            DocKind::Manifest,
            path_str.as_ref(),
            Some(chunk_start),
            &current_section,
            section_body,
        ));
    }
    if docs.is_empty() {
        docs.push(SearchDoc::new(
            DocKind::Config,
            path_str.as_ref(),
            None,
            name,
            content.chars().take(1000).collect::<String>(),
        ));
    }
    docs
}

// ---------------------------------------------------------------------------
// YAML adapter
// ---------------------------------------------------------------------------

fn adapt_yaml(path: &Path, content: &str) -> Vec<SearchDoc> {
    let path_str = path.to_string_lossy();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    // Flatten top-level keys + values into a single searchable chunk.
    let body: String = content
        .lines()
        .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .take(80)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(2000)
        .collect();
    if body.is_empty() {
        return vec![];
    }
    vec![SearchDoc::new(
        DocKind::Config,
        path_str.as_ref(),
        None,
        name,
        body,
    )]
}

// ---------------------------------------------------------------------------
// HTML adapter
// ---------------------------------------------------------------------------

fn adapt_html(path: &Path, content: &str) -> Vec<SearchDoc> {
    let path_str = path.to_string_lossy();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    // Strip HTML tags, collapse whitespace.
    let text = strip_html_tags(content);
    if text.trim().is_empty() {
        return vec![];
    }
    vec![SearchDoc::new(
        DocKind::Html,
        path_str.as_ref(),
        None,
        name,
        text.chars().take(2000).collect::<String>(),
    )]
}

fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    let mut in_script = false;
    let mut prev_space = true;
    let sl = s.to_lowercase();
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        // Detect <script> blocks.
        if !in_tag && i + 8 <= bytes.len() && &sl[i..i + 7] == "<script" {
            in_script = true;
        }
        if in_script {
            if i + 9 <= bytes.len() && &sl[i..i + 9] == "</script>" {
                in_script = false;
                i += 9;
                continue;
            }
            i += 1;
            continue;
        }
        match bytes[i] {
            b'<' => {
                in_tag = true;
            }
            b'>' => {
                in_tag = false;
                if !prev_space {
                    out.push(' ');
                    prev_space = true;
                }
            }
            _ if !in_tag => {
                let c = s[i..].chars().next().unwrap_or(' ');
                if c.is_whitespace() {
                    if !prev_space {
                        out.push(' ');
                        prev_space = true;
                    }
                } else {
                    out.push(c);
                    prev_space = false;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// CSS adapter
// ---------------------------------------------------------------------------

fn adapt_css(path: &Path, content: &str) -> Vec<SearchDoc> {
    let path_str = path.to_string_lossy();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    // Extract selector names and custom property declarations (--var-name).
    let mut selectors: Vec<String> = Vec::new();
    let mut custom_props: Vec<String> = Vec::new();
    for line in content.lines() {
        let t = line.trim();
        if t.ends_with('{') && !t.starts_with("/*") {
            let sel = t.trim_end_matches('{').trim();
            if !sel.is_empty() {
                selectors.push(sel.to_string());
            }
        }
        if t.starts_with("--") {
            let prop = t.split(':').next().unwrap_or("").trim();
            if !prop.is_empty() {
                custom_props.push(prop.to_string());
            }
        }
    }
    let body = format!("{} {}", selectors.join(" "), custom_props.join(" "));
    if body.trim().is_empty() {
        return vec![];
    }
    vec![SearchDoc::new(
        DocKind::Css,
        path_str.as_ref(),
        None,
        name,
        body.chars().take(2000).collect::<String>(),
    )]
}

// ---------------------------------------------------------------------------
// Manifest / build-script adapter
// ---------------------------------------------------------------------------

fn adapt_manifest(path: &Path, content: &str, kind: DocKind) -> Vec<SearchDoc> {
    let path_str = path.to_string_lossy();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    // For build scripts, extract target/lane names.
    if kind == DocKind::BuildScript {
        let targets: Vec<String> = content
            .lines()
            .filter_map(|l| {
                let t = l.trim();
                // Makefile targets: "target:"
                if t.ends_with(':') && !t.starts_with('\t') && !t.starts_with('#') {
                    let target = t.trim_end_matches(':').trim();
                    if !target.is_empty() && !target.contains(' ') {
                        return Some(target.to_string());
                    }
                }
                // Fastlane lanes: "lane :name do"
                if t.starts_with("lane :") || t.starts_with("desc \"") {
                    return Some(t.to_string());
                }
                None
            })
            .take(50)
            .collect();
        let body = targets.join(" ");
        if body.is_empty() {
            return vec![SearchDoc::new(
                kind,
                path_str.as_ref(),
                None,
                name,
                content.chars().take(500).collect::<String>(),
            )];
        }
        return vec![SearchDoc::new(kind, path_str.as_ref(), None, name, body)];
    }

    // Generic manifest: flatten all key = value / "key": "value" lines.
    let body: String = content
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with("//") && !t.starts_with('#')
        })
        .take(60)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(2000)
        .collect();
    if body.is_empty() {
        return vec![];
    }
    vec![SearchDoc::new(kind, path_str.as_ref(), None, name, body)]
}
