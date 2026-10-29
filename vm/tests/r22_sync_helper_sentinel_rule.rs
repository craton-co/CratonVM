// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 22, lane sync (SYNC22-2): the `i64::MIN` sentinel rule over
//! `vm/src/jit/helpers.rs`.
//!
//! `vm/src/jit/helper_guard.rs`' module doc, "Two `i64::MIN` sentinel checks":
//! `jit_bridge`'s outer gate treats an `i64::MIN` return as a sentinel only
//! beside a raised signal, so a helper arm that answers "should be
//! unreachable" with `i64::MIN` must raise one, or the gate returns the
//! sentinel as the method's real value (a wild reference for a reference
//! helper). Seven arms needed that one-line fix over rounds 10 to 22.
//!
//! # What is checked
//!
//! Every arm opened by `... jit_thread_mut() else {` (no live JIT thread) or
//! by `if vm_ptr == 0 ... {` (no VM) whose FIRST `return` is `i64::MIN` or
//! `Some(i64::MIN)` must call, at or before that return, one of
//! `set_jit_deopt_pending(`, `set_jit_pending_*`, `stash_helper_failure(` or
//! `stash_jit_monitor_error(`. An arm's body runs from the line after its
//! opener to the first `}` line at the opener's indentation (rustfmt's shape).
//! Comment lines are ignored. Arms whose failure value is not `i64::MIN`
//! (`0`, `-1`, a NaN, `None`) are outside the rule.
//!
//! On the tree it was written on (2026-10-20) the scan sees 12 such arms, all
//! compliant after the round-22 fixes; `MIN_ARMS` keeps it from passing
//! vacuously if the shapes it recognises are reformatted away.

use std::path::Path;

const SIGNALS: &[&str] = &[
    "set_jit_deopt_pending(",
    "set_jit_pending_",
    "stash_helper_failure(",
    "stash_jit_monitor_error(",
];

/// Fewer recognised arms than this means the scan no longer sees the shapes.
const MIN_ARMS: usize = 10;

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with("//")
}

fn opens_guarded_arm(line: &str) -> bool {
    if is_comment(line) {
        return false;
    }
    let t = line.trim_end();
    if !t.ends_with('{') {
        return false;
    }
    if t.contains("jit_thread_mut()") && t.trim_end_matches('{').trim_end().ends_with("else") {
        return true;
    }
    let s = t.trim_start();
    s.starts_with("if vm_ptr == 0 ") || s.starts_with("if vm_ptr == 0{")
}

fn returns_sentinel(line: &str) -> bool {
    line.contains("return i64::MIN;") || line.contains("return Some(i64::MIN);")
}

#[test]
fn every_unreachable_sentinel_arm_raises_a_signal() {
    // `vm/src/jit/helpers.rs` was split into `vm/src/jit/helpers/*.rs` by
    // origin/dev `00529e979f`; scan every part (an arm never spans two files).
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/jit/helpers");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("rs"))
        .collect();
    paths.sort();
    assert!(
        paths.len() > 1,
        "found {} Rust files under {}: the split helper sources are not where this scan looks",
        paths.len(),
        dir.display()
    );
    let mut arms = 0usize;
    let mut bad = Vec::new();
    for path in &paths {
        let file = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let lines: Vec<&str> = text.lines().collect();
        for (i, &opener) in lines.iter().enumerate() {
            if !opens_guarded_arm(opener) {
                continue;
            }
            let ind = indent(opener);
            let mut code: Vec<&str> = Vec::new();
            for &line in lines.iter().skip(i + 1).take(60) {
                if line.trim_start().starts_with('}') && indent(line) == ind {
                    break;
                }
                if !is_comment(line) {
                    code.push(line);
                }
            }
            let Some(first) = code.iter().position(|l| l.contains("return ")) else {
                continue;
            };
            if !returns_sentinel(code[first]) {
                continue;
            }
            arms += 1;
            let signalled = code[..=first]
                .iter()
                .any(|l| SIGNALS.iter().any(|s| l.contains(s)));
            if !signalled {
                bad.push(format!("helpers/{file}:{}: {}", i + 1, opener.trim()));
            }
        }
    }
    assert!(
        arms >= MIN_ARMS,
        "only {arms} guarded `i64::MIN` arms recognised: the scan no longer sees the shapes"
    );
    assert!(
        bad.is_empty(),
        "these arms return the `i64::MIN` sentinel with no signal beside it (add \
         `set_jit_deopt_pending();` -- see helper_guard.rs, \"Two `i64::MIN` sentinel checks\"):\n{}",
        bad.join("\n")
    );
}
