// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Ratchet on process state in the crates the two existing gates do not scan.
//!
//! `jit/tests/process_global_statics_ratchet.rs` freezes `jit/src`, and
//! `vm/tests/per_vm_state_statics_ratchet.rs` freezes `vm/src` and
//! `classloading/src`. The collectors, the native layer and this crate held
//! about 3,100 more `static` declarations with no gate, and an unreviewed one
//! is how a per-VM fact becomes process state: on 2026-11-11 two thread-local
//! memos keyed by a bare `ClassId` were found there
//! (`native-builtins/src/vector_support_intrinsics.rs` `OP_INFO_SLOT`,
//! `native-collections/src/lib.rs` `slot_declared_descriptor`), each answering
//! a second VM on the same thread with the first VM's field layout.
//!
//! The rule is the other two gates' rule, so every number means the same
//! thing: one per source line whose first non-blank text declares a static
//! item, test modules included. The baselines are what
//!
//! ```text
//! grep -rhE '^\s*(pub(\([^)]*\))?\s+)?static\s+(mut\s+)?[A-Za-z_][A-Za-z0-9_]*\s*:' \
//!     <crate>/src --include=*.rs | wc -l
//! ```
//!
//! printed on 2026-11-11; the same command printed the other two gates' own
//! baselines (1526, 78, 867) on that tree. It lives in `types/tests` because
//! this crate builds in seconds and already hosts the workspace-wide source
//! scans (`flag_declaration_guard.rs`).
//!
//! `native-builtins` 1397 -> 1399 at the merge of `dev` `03d235c8c6`, which
//! had added four and removed one since this gate's base. Reviewed:
//! `RAW_MEMORY_ANY_GATE` (`capability_gate.rs`) is a `(vm_identity, state)`
//! thread-local; `LAST_REAL_BLOCK` (`unsafe_natives_ext.rs`) holds an address
//! in the process-wide `Unsafe` arena and that arena's generation. The two
//! `CLASS_MODULE_SLOT` atomics held ONE VM's `Class.module` slot for the whole
//! process; they are now one thread-local keyed by `vm_identity`
//! (`class_module_slot` in `native-builtins/src/lib.rs`).
//!
//! Lower a number when a static is removed. Raise one only with a line here
//! saying why the new static is not per-VM state
//! (`docs/architecture/per-vm-state.md` section 7 has the three questions).

use std::path::{Path, PathBuf};

/// `(crate directory, static declaration lines in its src/ on 2026-11-11)`.
const BASELINES: &[(&str, usize)] = &[
    ("gc", 744),
    ("native-builtins", 1399),
    ("native-api", 238),
    ("native-io", 219),
    ("native-collections", 185),
    ("native-awt", 30),
    ("types", 207),
    ("jfr", 92),
    ("reader", 17),
    ("jit-api", 1),
    ("native-builtins-security", 1),
];

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().and_then(|x| x.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Whether `line` declares a static item: the scanner of the other two gates
/// (`declares_static` there), so the three count by one rule. `keyword` is
/// passed in so this file does not count itself.
fn declares_static(line: &str, keyword: &str) -> bool {
    let mut rest = line.trim_start();
    if let Some(after_pub) = rest.strip_prefix("pub") {
        let after_vis = match after_pub.strip_prefix('(') {
            Some(inner) => match inner.find(')') {
                Some(close) => &inner[close + 1..],
                None => return false,
            },
            None => after_pub,
        };
        let trimmed = after_vis.trim_start();
        if trimmed.len() == after_vis.len() {
            return false;
        }
        rest = trimmed;
    }
    let Some(after_keyword) = rest.strip_prefix(keyword) else {
        return false;
    };
    let trimmed = after_keyword.trim_start();
    if trimmed.len() == after_keyword.len() {
        return false;
    }
    rest = trimmed;
    if let Some(after_mut) = rest.strip_prefix("mut") {
        let trimmed = after_mut.trim_start();
        if trimmed.len() != after_mut.len() {
            rest = trimmed;
        }
    }
    let ident_end = rest
        .char_indices()
        .take_while(|&(i, c)| c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    ident_end != 0 && rest[ident_end..].trim_start().starts_with(':')
}

/// `(count, per-file listing)` for one crate's `src/`.
fn count_statics(krate: &str) -> (usize, String) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(krate)
        .join("src");
    let keyword = ["sta", "tic"].concat();
    let mut files = Vec::new();
    rust_sources(&root, &mut files);
    files.sort();
    let mut count = 0;
    let mut listing = String::new();
    for path in &files {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let n = text
            .lines()
            .filter(|l| declares_static(l, &keyword))
            .count();
        if n > 0 {
            listing.push_str(&format!("  {n:>4}  {}\n", path.display()));
        }
        count += n;
    }
    (count, listing)
}

#[test]
fn static_declarations_do_not_grow_outside_the_gated_crates() {
    let mut over = String::new();
    for &(krate, baseline) in BASELINES {
        let (count, listing) = count_statics(krate);
        assert!(
            count > 0,
            "counted no statics in {krate}/src: the scanner is broken"
        );
        if count > baseline {
            over.push_str(&format!(
                "{krate}/src declares {count} statics; the baseline is {baseline}.\n{listing}\n"
            ));
        } else if count < baseline {
            eprintln!(
                "note: {krate}/src is at {count}, below its baseline {baseline}; lower it here."
            );
        }
    }
    assert!(
        over.is_empty(),
        "{over}A static is shared by every VM in the process. Keep per-VM state on the VM, or key it \
         by `NativeContext::vm_identity()` (`VmScoped` in native-api does both). If the new static \
         is process-invariant, remove another one or raise the baseline in \
         types/tests/process_statics_ratchet.rs with a line saying why."
    );
}

#[test]
fn the_scanner_counts_declarations_and_nothing_else() {
    let k = ["sta", "tic"].concat();
    for yes in [
        format!("{k} A: u8 = 0;"),
        format!("    pub(crate) {k} mut B_2 : AtomicU8 = AtomicU8::new(0);"),
        format!("        {k} MEMO: Cell<u8> = const {{ Cell::new(0) }};"),
    ] {
        assert!(declares_static(&yes, &k), "{yes}");
    }
    for no in [
        format!("// {k} A: u8 = 0;"),
        format!("fn f(x: &'{k} str) {{}}"),
        format!("let {k}s: u8 = 0;"),
        format!("({k} $name:ident : $t:ty) => {{}};"),
    ] {
        assert!(!declares_static(&no, &k), "{no}");
    }
}
