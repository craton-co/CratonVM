// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Ratchet: `jit/src` may not grow a new private bytecode switch decoder.
//!
//! Every analysis used to decode instruction lengths, branch targets and switch
//! tables with its own copy, and the copies drifted: one length table had no
//! `ldc`, another no `jsr`/`ret`, a successor function dropped switch edges.
//! `jit/src/bytecode_analysis.rs` is now the one decoder
//! (`two-optimizing-front-ends-duplicate-bytecode-analyses-FIXED-20260912.md`), and this
//! test keeps it that way.
//!
//! # The rule
//!
//! A function counts when its body contains BOTH
//!
//! * switch-padding arithmetic — `% 4 != 0`, `% 4) % 4` (any number of closing
//!   parentheses before the second `%`), or `& !3` — and
//! * a `tableswitch`/`lookupswitch` opcode literal, `0xaa` or `0xab` in either
//!   case.
//!
//! That pair is what decoding a switch's operands by hand looks like. Bodies
//! are found by brace matching from each `fn` line; text after `//` on a line
//! is ignored. `bytecode_analysis.rs` is excluded.
//!
//! # What is allowed to remain, on 2026-09-12
//!
//! * `aarch64_backend.rs` `compile_pass` and `x64/bytecode_walk.rs`
//!   `compile_bytecode` — the emitters. They read a switch's operands to lay
//!   out its jump table in machine code.
//! * `x64/bytecode_compat.rs` `jit_scan` — the admission scanner, which
//!   validates each switch's operands before a method is compiled.
//! * `x64/tests.rs` `build_tableswitch_bytecode` and
//!   `build_lookupswitch_bytecode` — test fixtures that ASSEMBLE switch
//!   bytecode, padding included.
//!
//! [`ALLOWED`] pins the count in both directions: a new decoder fails, and so
//! does migrating one of the five until the number is lowered. The needles are
//! assembled at runtime, so this file cannot count itself.

use std::path::{Path, PathBuf};

const ALLOWED: usize = 5;

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => panic!("cannot read {}: {e}", dir.display()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().and_then(|x| x.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// The part of `line` before any `//`.
fn code_of(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

/// The name declared by a `fn` line, if `line` is one.
fn fn_name(line: &str) -> Option<&str> {
    let code = code_of(line);
    let at = code.find("fn ")?;
    let before = &code[..at];
    if !before.split_whitespace().all(|w| {
        matches!(
            w,
            "pub" | "pub(crate)" | "pub(super)" | "const" | "unsafe" | "async"
        ) || w.starts_with("extern")
    }) {
        return None;
    }
    let rest = &code[at + 3..];
    let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))?;
    (end > 0 && rest[end..].starts_with(['(', '<'])).then(|| &rest[..end])
}

fn has_padding(body: &str, rem: &str, not3: &str) -> bool {
    if body.contains(&format!("{rem} != 0")) || body.contains(not3) {
        return true;
    }
    // `% 4` then one or more `)` then ` % 4`.
    let mut from = 0;
    while let Some(i) = body[from..].find(rem) {
        let after = &body[from + i + rem.len()..];
        let closes = after.chars().take_while(|&c| c == ')').count();
        if closes > 0 && after[closes..].starts_with(&format!(" {rem}")) {
            return true;
        }
        from += i + rem.len();
    }
    false
}

/// `(file, fn)` for every function that decodes a switch by hand.
fn switch_decoders(root: &Path) -> Vec<(String, String)> {
    let rem = format!("% {}", 4);
    let not3 = format!("& !{}", 3);
    let opcodes = [
        format!("0x{}a", "a"),
        format!("0x{}b", "a"),
        format!("0x{}A", "A"),
        format!("0x{}B", "A"),
    ];
    let mut files = Vec::new();
    rust_sources(root, &mut files);
    files.sort();
    let mut found = Vec::new();
    for path in files {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if rel == "bytecode_analysis.rs" {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{rel}: {e}"));
        let lines: Vec<&str> = text.lines().collect();
        let mut i = 0;
        while i < lines.len() {
            let Some(name) = fn_name(lines[i]) else {
                i += 1;
                continue;
            };
            let mut depth: i64 = 0;
            let mut seen = false;
            let mut j = i;
            let mut body = String::new();
            while j < lines.len() {
                let code = code_of(lines[j]);
                body.push_str(code);
                body.push('\n');
                depth += code.matches('{').count() as i64 - code.matches('}').count() as i64;
                seen |= code.contains('{');
                if seen && depth <= 0 {
                    break;
                }
                j += 1;
            }
            if has_padding(&body, &rem, &not3)
                && opcodes.iter().any(|op| body.contains(op.as_str()))
            {
                found.push((rel.clone(), name.to_string()));
            }
            i = if seen { j + 1 } else { i + 1 };
        }
    }
    found
}

#[test]
fn no_new_private_bytecode_switch_decoders() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let found = switch_decoders(&root);
    let listing: Vec<String> = found.iter().map(|(f, n)| format!("  {f} {n}")).collect();
    assert!(
        found.len() <= ALLOWED,
        "{} functions in jit/src decode a switch by hand; the allowance is {ALLOWED}.\n\
         Use bytecode_analysis (insn_len/step, switch_table, switch_targets_lenient,\n\
         explicit_targets, InsnCfg) instead of a private decoder:\n{}",
        found.len(),
        listing.join("\n")
    );
    assert!(
        found.len() >= ALLOWED,
        "{} functions decode a switch by hand, fewer than the allowance of {ALLOWED}.\n\
         Lower ALLOWED in jit/tests/single_bytecode_decoder_ratchet.rs:\n{}",
        found.len(),
        listing.join("\n")
    );
}

#[test]
fn the_rule_sees_each_padding_spelling_and_ignores_comments() {
    let dir = std::env::temp_dir().join(format!("decoder-ratchet-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let pad_a = format!("while p % {} != 0 {{ p += 1; }}", 4);
    let pad_b = format!("let pad = (4 - ((pc + 1) % {})) % {};", 4, 4);
    let pad_c = format!("let base = (pc + 4) & !{};", 3);
    let sw = format!("0x{}a", "a");
    let src = [
        format!("fn a(code: &[u8]) {{ if code[0] == {sw} {{ {pad_a} }} }}"),
        format!("pub(crate) fn b(code: &[u8]) {{\n    match code[0] {{ {sw} => {{ {pad_b} }} _ => {{}} }}\n}}"),
        format!("fn c(code: &[u8]) {{ let _ = {sw}; {pad_c} }}"),
        format!("fn only_a_comment() {{ // {sw} {pad_a}\n}}"),
        format!("fn no_opcode() {{ {pad_a} }}"),
    ]
    .join("\n");
    std::fs::write(dir.join("probe.rs"), src).expect("write probe");
    let names: Vec<String> = switch_decoders(&dir).into_iter().map(|(_, n)| n).collect();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(names, vec!["a", "b", "c"]);
}
