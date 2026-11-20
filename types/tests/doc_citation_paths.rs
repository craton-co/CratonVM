// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright Craton Software Company

//! Production source citations must resolve in a public checkout.

use std::path::{Path, PathBuf};

const SKIPPED_DIRS: &[&str] = &[
    "target",
    ".git",
    "apps",
    "node_modules",
    "internal",
    "vendor",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("types/ always has a workspace root above it")
        .to_path_buf()
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if path.is_dir() {
            if !SKIPPED_DIRS.contains(&name) && !name.starts_with('.') {
                rust_sources(&path, out);
            }
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

fn is_path_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-' | b'/')
}

/// If a citation continues on the next line, the index just past the break.
///
/// Two continuations exist: a comment marker (`//`, `///`, `//!`, or a block
/// comment's `*`) and a Rust string literal's `\`-at-end-of-line. Backticks on
/// either side of a comment break are absorbed — several citations here have
/// their two halves separately code-quoted.
fn skip_break(b: &[u8], from: usize, last: Option<u8>) -> Option<usize> {
    if !matches!(last, Some(b'/') | Some(b'-')) {
        return None;
    }
    let mut i = from;
    // A string-literal continuation takes the backslash first and allows
    // nothing before it.
    if b.get(i) == Some(&b'\\') {
        i += 1;
        i = skip_newline(b, i)?;
        while matches!(b.get(i), Some(b' ') | Some(b'\t')) {
            i += 1;
        }
        return Some(i);
    }
    if b.get(i) == Some(&b'`') {
        i += 1;
    }
    while matches!(b.get(i), Some(b' ') | Some(b'\t')) {
        i += 1;
    }
    i = skip_newline(b, i)?;
    while matches!(b.get(i), Some(b' ') | Some(b'\t')) {
        i += 1;
    }
    if b.get(i) == Some(&b'/') && b.get(i + 1) == Some(&b'/') {
        i += 2;
        if matches!(b.get(i), Some(b'/') | Some(b'!')) {
            i += 1;
        }
    } else if b.get(i) == Some(&b'*') {
        i += 1;
    } else {
        return None;
    }
    while matches!(b.get(i), Some(b' ') | Some(b'\t')) {
        i += 1;
    }
    if b.get(i) == Some(&b'`') {
        i += 1;
    }
    Some(i)
}

/// Past a `\n` or `\r\n` at `i`, or `None` if there is no line break there.
fn skip_newline(b: &[u8], i: usize) -> Option<usize> {
    match b.get(i) {
        Some(b'\n') => Some(i + 1),
        Some(b'\r') if b.get(i + 1) == Some(&b'\n') => Some(i + 2),
        _ => None,
    }
}

/// Consume a path starting at `start`, rejoining comment breaks, and return
/// `(path-through-its-last-".md", index just past it)`.
fn consume_path(b: &[u8], start: usize) -> Option<(String, usize)> {
    let mut i = start;
    let mut logical = String::new();
    loop {
        match b.get(i) {
            Some(&c) if is_path_byte(c) => {
                logical.push(c as char);
                i += 1;
            }
            _ => match skip_break(b, i, logical.as_bytes().last().copied()) {
                Some(next) => i = next,
                None => break,
            },
        }
    }
    // Greedy consumption overshoots any trailing `.`/`)`/backtick, and can run
    // past the end of a wrapped path into the prose after it. Cutting at the
    // LAST `.md` is what the equivalent regex's backtracking does.
    let end = logical.rfind(".md")?;
    Some((logical[..end + 3].to_string(), start + end + 3))
}

/// Every `….md` path in `text`, rejoined across comment breaks, with the
/// 1-based line it starts on.
///
/// Both citation styles come out of here: a published one roots at `docs/`, an
/// internal one is a bare path relative to the internal tree's own root. A run
/// only starts where a path character follows a non-path character, so the
/// tail of a longer path is never mistaken for a citation of its own.
fn md_paths(text: &str) -> Vec<(String, usize)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut prev_is_path = false;
    while i < b.len() {
        let here_is_path = is_path_byte(b[i]);
        if here_is_path && !prev_is_path {
            if let Some((path, end)) = consume_path(b, i) {
                out.push((path, text[..i].matches('\n').count() + 1));
                i = end.max(i + 1);
                prev_is_path = true;
                continue;
            }
        }
        prev_is_path = here_is_path;
        i += 1;
    }
    out
}

#[test]
fn production_doc_citations_resolve_without_private_files() {
    let root = workspace_root();
    let mut sources = Vec::new();
    rust_sources(&root, &mut sources);
    let mut checked = 0;
    let mut missing = Vec::new();
    for path in sources {
        let relative = path
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if !relative.contains("/src/") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        for (cited, line) in md_paths(&text) {
            if !cited.starts_with("docs/") {
                continue;
            }
            checked += 1;
            if cited.starts_with(concat!("docs/", "internal/")) || !root.join(&cited).is_file() {
                missing.push(format!("{relative}:{line}: {cited}"));
            }
        }
    }
    assert!(
        checked > 100,
        "source citation scan checked only {checked} links"
    );
    assert!(
        missing.is_empty(),
        "public source citations do not resolve:\n{}",
        missing.join("\n")
    );
}

#[test]
fn extracts_wrapped_and_whole_citations() {
    let paths = md_paths("// docs/feature-designs/real-jdk-mode.md\n// docs/feature-designs/\n// real-frame-deopt.md");
    assert!(paths
        .iter()
        .any(|(p, _)| p == "docs/feature-designs/real-jdk-mode.md"));
    assert!(paths
        .iter()
        .any(|(p, _)| p == "docs/feature-designs/real-frame-deopt.md"));
}
