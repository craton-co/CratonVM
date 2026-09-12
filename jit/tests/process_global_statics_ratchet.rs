// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Ratchet: the JIT may not grow new `static` declarations.
//!
//! A `static` in this crate is process state. Every VM in the process shares
//! it, including an embedded VM and a test that builds two. AGENTS.md forbids
//! process globals for compatibility state, and a 2026-09-12 JIT review found
//! the rule broken by the state it exists for:
//!
//! * `JIT_COMPATIBILITY_MODE` (`lib.rs`) latched with `fetch_max`, so a VM
//!   created in `--jdk-only` mode made every later VM in the process
//!   over-strict, forever;
//! * `DESPEC_SET` (`deopt.rs`) let one VM's despeculation verdicts strip
//!   speculations from another VM's compiles;
//! * `BACKGROUND_COMPILER` (`tiered.rs`) drained only the first VM's compile
//!   queue.
//!
//! All three are gone. The latch was deleted, because the policy is already a
//! per-compilation argument. The despeculation set is a `DespecRegistry` owned
//! by the VM's `JitRealm`. The compile workers are owned by each
//! `TieredCompilationManager`. See
//! `jit-compatibility-and-despec-state-per-vm-FIXED.md`.
//!
//! Most of the remaining statics are legitimate: helper addresses that are
//! process-invariant `fn` pointers, `OnceLock` caches of environment flags,
//! metrics counters, thread-locals. This test does not judge them. It only stops
//! the count from growing, so each new one has to be argued for in review
//! instead of arriving unnoticed.
//!
//! # What counts
//!
//! One per source line in `jit/src/**/*.rs` whose first non-blank text declares
//! a static item. That is, in order:
//!
//! 1. an optional visibility, `pub` or `pub(...)`, followed by whitespace;
//! 2. the keyword, followed by whitespace;
//! 3. an optional `mut` followed by whitespace;
//! 4. an ASCII identifier;
//! 5. optional whitespace, then `:`.
//!
//! That covers module-level statics, statics inside function bodies (the
//! `static CACHE: OnceLock<..>` inside a getter), `static mut`, and each
//! declaration line of a `thread_local!` block. Every file under `jit/src`
//! counts, `#[cfg(test)]` modules and `tests.rs` files included. Telling test
//! statics apart textually is fragile, and counting them all keeps the rule
//! exact and the number stable.
//!
//! Not counted: comment lines, `'static` lifetimes and bounds (never at the
//! start of a line), and a `static $name:` inside a `macro_rules!` pattern,
//! which is not an identifier. A declaration split so that its name sits on a
//! later line than the keyword would also be missed; rustfmt never produces one.
//!
//! The baseline was computed on 2026-09-12, after the two statics above were
//! removed, with the equivalent
//!
//! ```text
//! grep -rhE '^\s*(pub(\([^)]*\))?\s+)?static\s+(mut\s+)?[A-Za-z_][A-Za-z0-9_]*\s*:' \
//!     jit/src --include=*.rs | wc -l
//! ```
//!
//! run from the repository root, which printed 744.
//!
//! The keyword is assembled at runtime. This file is under `jit/tests/`,
//! outside the scanned tree, but a literal needle would be one copy-paste away
//! from counting itself.

use std::path::{Path, PathBuf};

/// `static` declaration lines in `jit/src` on 2026-09-12. Lower it when a
/// static is removed; never raise it to make room for a new one.
// 2026-09-12: 744 -> 747 on merging into the JIT review branch. The three are
// test fixtures merged alongside this ratchet, not process state: the
// `HITS` marker-helper counters and the `FLAG` byte in the instanceof and
// return-narrowing tests (`ir_lower.rs` and `x64/tests.rs` test modules).
const BASELINE: usize = 747;

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

/// Whether `line` declares a static item, by the rule in the module doc.
///
/// `keyword` is the item keyword, passed in so the literal never appears here.
fn declares_static(line: &str, keyword: &str) -> bool {
    let mut rest = line.trim_start();

    // 1. Optional `pub` / `pub(...)`, which must be followed by whitespace.
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

    // 2. The keyword, followed by whitespace (so `statics` or `static_x` is not it).
    let Some(after_keyword) = rest.strip_prefix(keyword) else {
        return false;
    };
    let trimmed = after_keyword.trim_start();
    if trimmed.len() == after_keyword.len() {
        return false;
    }
    rest = trimmed;

    // 3. Optional `mut` followed by whitespace. `mutex` is a name, not `mut`.
    if let Some(after_mut) = rest.strip_prefix("mut") {
        let trimmed = after_mut.trim_start();
        if trimmed.len() != after_mut.len() {
            rest = trimmed;
        }
    }

    // 4. An ASCII identifier.
    let ident_end = rest
        .char_indices()
        .take_while(|&(i, c)| c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    if ident_end == 0 {
        return false;
    }

    // 5. Optional whitespace, then the type annotation's colon.
    rest[ident_end..].trim_start().starts_with(':')
}

#[test]
fn jit_static_declarations_do_not_grow() {
    let keyword = ["sta", "tic"].concat();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&root, &mut files);
    files.sort();
    assert!(
        !files.is_empty(),
        "found no Rust sources under {}; the scan would pass vacuously",
        root.display()
    );

    let mut count = 0usize;
    let mut per_file = Vec::new();
    for path in &files {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let n = text
            .lines()
            .filter(|line| declares_static(line, &keyword))
            .count();
        if n > 0 {
            let shown = path
                .strip_prefix(&root)
                .unwrap_or(path.as_path())
                .display()
                .to_string();
            per_file.push((shown, n));
        }
        count += n;
    }
    assert!(
        count > 0,
        "counted no static declarations under {}; the scanner is broken, not the crate clean",
        root.display()
    );

    if count > BASELINE {
        let listing = per_file
            .iter()
            .map(|(file, n)| format!("  {n:>4}  {file}"))
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "jit/src declares {count} statics; the baseline is {BASELINE}.\n\
             \n\
             A static in cratonvm-jit is shared by every VM in the process. \
             New per-VM or compatibility state belongs on the VM's JIT state \
             (`JitRealm` on the VM side, or `JitCache`), threaded into the \
             compile request as an argument, not in a static. AGENTS.md forbids \
             process globals for compatibility state. See \
             jit-compatibility-and-despec-state-per-vm-FIXED.md.\n\
             \n\
             If the new static is genuinely process-invariant (a `fn` address, a \
             cache of an environment flag), remove another one or make the case \
             in review. When a static is removed, lower BASELINE in \
             jit/tests/process_global_statics_ratchet.rs to the new count.\n\
             \n\
             Per file:\n{listing}"
        );
    }
    if count < BASELINE {
        eprintln!(
            "note: jit/src now declares {count} statics, below the baseline of \
             {BASELINE}. Lower BASELINE in \
             jit/tests/process_global_statics_ratchet.rs to {count} so the \
             removal cannot be undone silently."
        );
    }
}

/// The scanner itself, against lines whose answer is known. A scanner nobody
/// has watched fail passes vacuously.
#[test]
fn the_scanner_counts_declarations_and_nothing_else() {
    let kw = ["sta", "tic"].concat();
    let counted = [
        format!("{kw} FOO: u8 = 0;"),
        format!("    {kw} CACHE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();"),
        format!("pub {kw} BAR: AtomicU64 = AtomicU64::new(0);"),
        format!("pub(crate) {kw} BAZ: &str = \"x\";"),
        format!("pub(super) {kw} mut QUX : i32 = 1;"),
        format!("    pub {kw} mut __jit_debug_descriptor: JitDescriptor = JitDescriptor {{"),
        format!("        {kw} DEPTH: Cell<usize> = const {{ Cell::new(0) }};"),
        format!("{kw} mutex: Mutex<()> = Mutex::new(());"),
        format!("{kw} STR: &'{kw} str = \"\";"),
    ];
    for line in &counted {
        assert!(declares_static(line, &kw), "must count: {line:?}");
    }
    let ignored = [
        format!("// {kw} FOO: u8 = 0;"),
        format!("    /// {kw} FOO: u8 = 0;"),
        format!("fn f() -> &'{kw} str {{ \"\" }}"),
        format!("fn g<T: '{kw}>(t: T) {{}}"),
        format!("    {kw} $name: $ty = $init;"),
        format!("{kw}s: u8"),
        format!("let x = {kw}_value;"),
        format!("publish {kw} FOO: u8 = 0;"),
        format!("pub{kw} FOO: u8 = 0;"),
        String::new(),
    ];
    for line in &ignored {
        assert!(!declares_static(line, &kw), "must not count: {line:?}");
    }
}
