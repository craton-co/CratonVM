// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT machine-code disassembly dump (diagnostic).
//!
//! `CRATONVM_DBG_JIT_DISASM` — comma-separated list of case-sensitive
//! substrings matched against `Class.method`; a method whose qualified name
//! contains any entry is dumped to stderr as NASM-formatted x86-64 right
//! after a successful compile. `1` or `*` dumps every compiled method.
//!
//! The gap that motivated this (gap-jit-fastmath-transform-miscompile.md
//! Bug 3) could not be pinned without seeing the emitted code: the JIT has no
//! other way to inspect its output. iced-x86 is decode-only here (no encoder),
//! and the whole module is inert unless the env var is set.

use std::sync::OnceLock;

/// Parsed `CRATONVM_DBG_JIT_DISASM` filter. `None` = disabled.
fn filter() -> Option<&'static Vec<String>> {
    static CACHE: OnceLock<Option<Vec<String>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let v = cratonvm_types::flags::runtime_var("CRATONVM_DBG_JIT_DISASM").ok()?;
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
pub fn maybe_dump(
    what: &str,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    entry: *const u8,
    code: &[u8],
) {
    maybe_dump_annotated(
        what,
        class_name,
        method_name,
        descriptor,
        entry,
        code,
        None,
        None,
    );
}

/// x86-64 register name for a JIT local-home register number.
fn reg_name(r: u8) -> &'static str {
    match r {
        0 => "rax",
        1 => "rcx",
        2 => "rdx",
        3 => "rbx",
        4 => "rsp",
        5 => "rbp",
        6 => "rsi",
        7 => "rdi",
        8 => "r8",
        9 => "r9",
        10 => "r10",
        11 => "r11",
        12 => "r12",
        13 => "r13",
        14 => "r14",
        15 => "r15",
        _ => "r?",
    }
}

/// Like [`maybe_dump`] but also prints the local→register map and labels each
/// native block with its originating bytecode PC. `pc_to_native[bc_pc]` is the
/// native offset where bytecode PC `bc_pc` begins (-1 if unmapped);
/// `local_assignments[i]` is the callee-saved register local `i` is colored to
/// (None = frame-resident). Gated by the same `CRATONVM_DBG_JIT_DISASM` filter.
pub fn maybe_dump_annotated(
    what: &str,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    entry: *const u8,
    code: &[u8],
    pc_to_native: Option<&[i32]>,
    local_assignments: Option<&[Option<u8>]>,
) {
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
    // Local-variable home map: which local index lives in which callee-saved reg.
    if let Some(locals) = local_assignments {
        let homes: Vec<String> = locals
            .iter()
            .enumerate()
            .map(|(i, a)| match a {
                Some(r) => format!("L{i}={}", reg_name(*r)),
                None => format!("L{i}=frame"),
            })
            .collect();
        out.push_str(&format!("  ; locals: {}\n", homes.join(" ")));
    }
    // Invert pc_to_native: native offset -> bytecode PC(s) that start there.
    let native_to_pc: Option<std::collections::BTreeMap<usize, Vec<usize>>> =
        pc_to_native.map(|m| {
            let mut inv: std::collections::BTreeMap<usize, Vec<usize>> = Default::default();
            for (bc_pc, &nat) in m.iter().enumerate() {
                if nat >= 0 {
                    inv.entry(nat as usize).or_default().push(bc_pc);
                }
            }
            inv
        });
    let mut text = String::new();
    let mut insn = iced_x86::Instruction::default();
    while decoder.can_decode() {
        decoder.decode_out(&mut insn);
        let start = (insn.ip() - ip) as usize;
        if let Some(ref inv) = native_to_pc {
            if let Some(bc_pcs) = inv.get(&start) {
                let labels: Vec<String> = bc_pcs.iter().map(|p| format!("bc@{p}")).collect();
                out.push_str(&format!("  ; --- {} ---\n", labels.join(",")));
            }
        }
        text.clear();
        formatter.format(&insn, &mut text);
        let bytes: String = code[start..start + insn.len()]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        out.push_str(&format!(
            "  {:>6x}: {:<24} {}\n",
            insn.ip() - ip,
            bytes,
            text
        ));
    }
    eprintln!("{out}");
}
