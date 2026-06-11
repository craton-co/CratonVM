// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT machine-code disassembly dump (diagnostic).
//!
//! `CRATONVM_DBG_JIT_DISASM` — comma-separated list of case-sensitive
//! substrings matched against `Class.method`; a method whose qualified name
//! contains any entry is dumped to stderr as NASM-formatted x86-64 right
//! after a successful compile. `1` or `*` dumps every compiled method.
//!
//! The gap that motivated this (docs/gaps/gap-jit-fastmath-transform-miscompile.md
//! Bug 3) could not be pinned without seeing the emitted code: the JIT has no
//! other way to inspect its output. iced-x86 is decode-only here (no encoder),
//! and the whole module is inert unless the env var is set.

use std::sync::OnceLock;

/// Parsed `CRATONVM_DBG_JIT_DISASM` filter. `None` = disabled.
fn filter() -> Option<&'static Vec<String>> {
    static CACHE: OnceLock<Option<Vec<String>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let v = std::env::var("CRATONVM_DBG_JIT_DISASM").ok()?;
            if v.is_empty() || v == "0" {
                return None;
            }
            Some(v.split(',').map(|s| s.trim().to_string()).collect())
        })
        .as_ref()
}

/// Dump `code` (mapped at `entry` — addresses in the listing are real) to
/// stderr if `Class.method` matches the `CRATONVM_DBG_JIT_DISASM` filter.
/// `what` labels the compile path (`upgrade`, `osr`, `full`, ...).
pub fn maybe_dump(what: &str, class_name: &str, method_name: &str, descriptor: &str, entry: *const u8, code: &[u8]) {
    let Some(pats) = filter() else { return };
    let qualified = format!("{class_name}.{method_name}");
    let matches = pats
        .iter()
        .any(|p| p == "1" || p == "*" || qualified.contains(p.as_str()));
    if !matches {
        return;
    }

    use iced_x86::{Decoder, DecoderOptions, Formatter, NasmFormatter};
    let ip = entry as u64;
    let mut decoder = Decoder::with_ip(64, code, ip, DecoderOptions::NONE);
    let mut formatter = NasmFormatter::new();
    let mut out = String::with_capacity(code.len() * 8);
    out.push_str(&format!(
        "[cratonvm-jit-disasm] {what} {qualified}{descriptor} entry={entry:p} len={}\n",
        code.len()
    ));
    let mut text = String::new();
    let mut insn = iced_x86::Instruction::default();
    while decoder.can_decode() {
        decoder.decode_out(&mut insn);
        text.clear();
        formatter.format(&insn, &mut text);
        let start = (insn.ip() - ip) as usize;
        let bytes: String = code[start..start + insn.len()]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        out.push_str(&format!("  {:>6x}: {:<24} {}\n", insn.ip() - ip, bytes, text));
    }
    eprintln!("{out}");
}
