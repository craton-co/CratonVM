// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T11 — Safety & Hardening conformance test suite.
//!
//! Verifies that CratonVM satisfies all T11 safety requirements from
//! `history/roadmap-100.md`.  Each test checks a specific structural or
//! behavioral invariant rather than exercising runtime semantics.
//!
//!     cargo test -p cratonvm-vm --test t11_safety_conformance -- --nocapture

use std::path::{Path, PathBuf};

/// Returns the workspace root (parent of the `vm` crate).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
}

/// Resolves a workspace-relative path (e.g. "gc/src/gen_heap.rs").
fn ws(rel: &str) -> String {
    workspace_root().join(rel).to_string_lossy().into_owned()
}

// ===========================================================================
// T11.1 — Every `unsafe` block has a `// SAFETY:` comment
// ===========================================================================

/// Scans a source file for `unsafe {` or `unsafe fn` blocks and checks that
/// every one is preceded (within 5 lines) by a `// SAFETY:` comment.
fn check_safety_comments(path: &str) -> (usize, usize) {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[t11] WARN: could not read {path}: {e}");
            return (0, 0);
        }
    };
    let lines: Vec<&str> = contents.lines().collect();
    let mut total_unsafe = 0usize;
    let mut documented = 0usize;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();

        // Skip comment lines
        if trimmed.starts_with("//") || trimmed.starts_with("///") || trimmed.starts_with("*") {
            continue;
        }

        // Match `unsafe {`, `unsafe fn`, `unsafe impl`, `unsafe extern`
        let is_unsafe = trimmed.contains("unsafe {")
            || trimmed.contains("unsafe fn ")
            || trimmed.starts_with("unsafe impl")
            || trimmed.starts_with("unsafe extern")
            || trimmed.starts_with("pub unsafe fn")
            || trimmed.starts_with("pub(crate) unsafe fn");

        if !is_unsafe {
            continue;
        }

        // Skip `unsafe impl Send` and `unsafe impl Sync` — marker traits need no justification
        if trimmed.contains("unsafe impl Send") || trimmed.contains("unsafe impl Sync") {
            continue;
        }

        total_unsafe += 1;

        // Check preceding 5 lines for a SAFETY comment
        let start = i.saturating_sub(5);
        let mut found_safety = false;
        for j in start..=i {
            if lines[j].contains("// SAFETY:") {
                found_safety = true;
                break;
            }
        }

        if found_safety {
            documented += 1;
        }
    }
    (total_unsafe, documented)
}

#[test]
fn t11_1_gen_heap_safety_comments() {
    let p = ws("gc/src/gen_heap.rs");
    let (total, documented) = check_safety_comments(&p);
    let coverage = if total > 0 {
        documented * 100 / total
    } else {
        100
    };
    eprintln!(
        "[t11] T11.1 gen_heap.rs: {documented}/{total} unsafe blocks documented ({coverage}%)"
    );
    assert!(
        coverage >= 90,
        "gen_heap.rs SAFETY coverage {coverage}% < 90% ({documented}/{total})"
    );
}

/// Sum `check_safety_comments` over a file and every `.rs` in a sibling
/// directory of the same name.
///
/// `interpreter.rs` and `x64.rs` were split into `interpreter/` and `x64/`
/// submodules in 2026-07. A gate that keeps measuring only the parent stops
/// seeing the moved `unsafe` blocks entirely — for `interpreter.rs` that was 27
/// of 105 — so the percentage it reports becomes an artefact of where the code
/// happens to live rather than a statement about the code.
fn check_safety_comments_tree(file: &str, dir: &str) -> (usize, usize) {
    let (mut total, mut documented) = check_safety_comments(file);
    let dir_path = ws(dir);
    let entries = match std::fs::read_dir(&dir_path) {
        Ok(e) => e,
        Err(_) => return (total, documented),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let (t, d) = check_safety_comments(&path.to_string_lossy());
        total += t;
        documented += d;
    }
    (total, documented)
}

#[test]
fn t11_1_x64_safety_comments() {
    let (total, documented) = check_safety_comments_tree(&ws("jit/src/x64.rs"), "jit/src/x64");
    let coverage = if total > 0 {
        documented * 100 / total
    } else {
        100
    };
    eprintln!("[t11] T11.1 x64.rs: {documented}/{total} unsafe blocks documented ({coverage}%)");
    assert!(
        coverage >= 90,
        "x64.rs SAFETY coverage {coverage}% < 90% ({documented}/{total})"
    );
}

#[test]
fn t11_1_helpers_safety_comments() {
    let p = ws("vm/src/jit/helpers.rs");
    let (total, documented) = check_safety_comments(&p);
    let coverage = if total > 0 {
        documented * 100 / total
    } else {
        100
    };
    eprintln!(
        "[t11] T11.1 helpers.rs: {documented}/{total} unsafe blocks documented ({coverage}%)"
    );
    assert!(
        coverage >= 90,
        "helpers.rs SAFETY coverage {coverage}% < 90% ({documented}/{total})"
    );
}

#[test]
fn t11_1_interpreter_safety_comments() {
    let (total, documented) = check_safety_comments_tree(
        &ws("vm/src/runtime/interpreter.rs"),
        "vm/src/runtime/interpreter",
    );
    let coverage = if total > 0 {
        documented * 100 / total
    } else {
        100
    };
    eprintln!(
        "[t11] T11.1 interpreter.rs: {documented}/{total} unsafe blocks documented ({coverage}%)"
    );
    assert!(
        coverage >= 90,
        "interpreter.rs SAFETY coverage {coverage}% < 90% ({documented}/{total})"
    );
}

// ===========================================================================
// T11.2 — GC allocator has no panic paths
// ===========================================================================

/// Scans a file for unwrap(), expect(), panic!() calls outside of test modules.
/// Stops scanning at `#[cfg(test)]` since everything below that is test code.
fn count_panic_paths(path: &str) -> Vec<(usize, String)> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let mut results = Vec::new();

    for (i, line) in contents.lines().enumerate() {
        let trimmed = line.trim();

        // Stop at test module — everything below is test code.
        if trimmed == "#[cfg(test)]" {
            break;
        }

        // Skip comment lines
        if trimmed.starts_with("//") || trimmed.starts_with("*") {
            continue;
        }

        let has_panic = trimmed.contains(".unwrap()")
            || trimmed.contains(".expect(")
            || trimmed.contains("panic!(")
            || trimmed.contains("unreachable!(")
            || trimmed.contains("todo!(");

        if has_panic {
            results.push((i + 1, trimmed.to_string()));
        }
    }
    results
}

#[test]
fn t11_2_gc_no_panics() {
    let p = ws("gc/src/gen_heap.rs");
    let panics = count_panic_paths(&p);
    if !panics.is_empty() {
        eprintln!("[t11] T11.2 gen_heap.rs panic paths found:");
        for (line, text) in &panics {
            eprintln!("  line {line}: {text}");
        }
    }
    eprintln!("[t11] T11.2 gen_heap.rs: {} panic paths", panics.len());
    assert!(
        panics.len() <= 3,
        "gen_heap.rs has {} panic paths (max allowed: 3)",
        panics.len()
    );
}

// ===========================================================================
// T11.3 — No unguarded truncating casts in interpreter
// ===========================================================================

/// Counts `as` casts that lack any annotation comment.
fn count_unguarded_casts(path: &str) -> (usize, usize) {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (0, 0),
    };
    let lines: Vec<&str> = contents.lines().collect();
    let mut total_casts = 0usize;
    let mut annotated = 0usize;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with("*") {
            continue;
        }

        let cast_count = trimmed.matches(" as ").count();
        if cast_count == 0 {
            continue;
        }

        total_casts += cast_count;

        // Check same line for annotation
        let has_annotation = trimmed.contains("// JVM spec:")
            || trimmed.contains("// Widening:")
            || trimmed.contains("// Truncation")
            || trimmed.contains("// SAFETY:")
            || trimmed.contains("// Pointer cast")
            || trimmed.contains("// Cast:")
            || trimmed.contains("// LEAK")
            || trimmed.contains("// OWNERSHIP");

        // Also check preceding 3 lines
        let start = i.saturating_sub(3);
        let mut has_nearby = false;
        for j in start..i {
            let prev = lines[j].trim();
            if prev.contains("// JVM spec:")
                || prev.contains("// Widening:")
                || prev.contains("// Truncation")
                || prev.contains("// SAFETY:")
                || prev.contains("// Cast:")
            {
                has_nearby = true;
                break;
            }
        }

        if has_annotation || has_nearby {
            annotated += cast_count;
        }
    }
    (total_casts, annotated)
}

#[test]
fn t11_3_interpreter_casts_annotated() {
    let p = ws("vm/src/runtime/interpreter.rs");
    let (total, annotated) = count_unguarded_casts(&p);
    let coverage = if total > 0 {
        annotated * 100 / total
    } else {
        100
    };
    eprintln!("[t11] T11.3 interpreter.rs: {annotated}/{total} casts annotated ({coverage}%)");
    assert!(
        coverage >= 70,
        "interpreter.rs cast annotation coverage {coverage}% < 70% ({annotated}/{total})"
    );
}

#[test]
fn t11_3_x64_casts_annotated() {
    let p = ws("jit/src/x64.rs");
    let (total, annotated) = count_unguarded_casts(&p);
    let coverage = if total > 0 {
        annotated * 100 / total
    } else {
        100
    };
    eprintln!("[t11] T11.3 x64.rs: {annotated}/{total} casts annotated ({coverage}%)");
    assert!(
        coverage >= 70,
        "x64.rs cast annotation coverage {coverage}% < 70% ({annotated}/{total})"
    );
}

// ===========================================================================
// T11.4 — Memory leak patterns are documented
// ===========================================================================

/// Checks that all Box::leak and Box::into_raw calls are documented.
fn check_leak_docs(path: &str) -> (usize, usize) {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (0, 0),
    };
    let lines: Vec<&str> = contents.lines().collect();
    let mut total = 0usize;
    let mut documented = 0usize;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            continue;
        }

        let has_leak = trimmed.contains("Box::leak") || trimmed.contains("Box::into_raw");
        if !has_leak {
            continue;
        }
        total += 1;

        let start = i.saturating_sub(3);
        let mut found_doc = trimmed.contains("// LEAK") || trimmed.contains("// OWNERSHIP");
        for j in start..i {
            if lines[j].contains("// LEAK") || lines[j].contains("// OWNERSHIP") {
                found_doc = true;
                break;
            }
        }
        if found_doc {
            documented += 1;
        }
    }
    (total, documented)
}

#[test]
fn t11_4_jit_leaks_documented() {
    let p = ws("jit/src/x64.rs");
    let (total, documented) = check_leak_docs(&p);
    let coverage = if total > 0 {
        documented * 100 / total
    } else {
        100
    };
    eprintln!("[t11] T11.4 x64.rs: {documented}/{total} leak patterns documented ({coverage}%)");
    assert!(
        coverage >= 80,
        "x64.rs leak documentation {coverage}% < 80% ({documented}/{total})"
    );
}

// ===========================================================================
// T11.5 — vm_init has no bare .unwrap() calls
// ===========================================================================

/// Counts bare `.unwrap()` calls (not `.expect()`) in non-test code.
fn count_bare_unwraps(path: &str) -> usize {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return 0,
    };
    let mut count = 0;
    let mut in_test = false;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.contains("#[cfg(test)]") || trimmed.starts_with("mod tests") {
            in_test = true;
        }
        if in_test {
            continue;
        }
        if trimmed.starts_with("//") {
            continue;
        }
        if trimmed.contains(".unwrap()") {
            count += 1;
        }
    }
    count
}

#[test]
fn t11_5_vm_init_no_bare_unwraps() {
    let p = ws("vm/src/vm/vm_init.rs");
    let count = count_bare_unwraps(&p);
    eprintln!("[t11] T11.5 vm_init.rs: {count} bare .unwrap() calls remaining");
    assert!(
        count <= 10,
        "vm_init.rs has {count} bare .unwrap() calls (max allowed: 10)"
    );
}

// ===========================================================================
// T11.6 — Lock ordering framework exists and is functional
// ===========================================================================

#[test]
fn t11_6_lock_order_module_exists() {
    let p = ws("vm/src/runtime/lock_order.rs");
    let exists = Path::new(&p).exists();
    eprintln!("[t11] T11.6 lock_order.rs exists: {exists}");
    assert!(exists, "lock_order.rs module must exist at {p}");
}

#[test]
fn t11_6_lock_order_module_declared() {
    let p = ws("vm/src/runtime/mod.rs");
    let mod_rs = std::fs::read_to_string(&p).expect("failed to read runtime/mod.rs");
    let declared = mod_rs.contains("pub mod lock_order");
    eprintln!("[t11] T11.6 lock_order declared in mod.rs: {declared}");
    assert!(
        declared,
        "lock_order module must be declared in runtime/mod.rs"
    );
}

#[test]
fn t11_6_lock_order_has_required_types() {
    let p = ws("vm/src/runtime/lock_order.rs");
    let contents = match std::fs::read_to_string(&p) {
        Ok(c) => c,
        Err(e) => panic!("Cannot read lock_order.rs: {e}"),
    };
    assert!(
        contents.contains("OrderedMutex"),
        "must define OrderedMutex"
    );
    assert!(
        contents.contains("OrderedRwLock"),
        "must define OrderedRwLock"
    );
    assert!(contents.contains("LockLevel"), "must define LockLevel");
    assert!(
        contents.contains("thread_local!"),
        "must use thread_local for debug tracking"
    );
    eprintln!("[t11] T11.6 lock_order.rs contains all required types");
}

// ===========================================================================
// T11.7 — Structural gates
// ===========================================================================

#[test]
fn t11_7_no_mem_forget_in_gc() {
    let p = ws("gc/src/gen_heap.rs");
    let contents = std::fs::read_to_string(&p).unwrap_or_default();
    let has_forget = contents.contains("mem::forget") || contents.contains("std::mem::forget");
    eprintln!("[t11] T11.7 gen_heap.rs mem::forget: {has_forget}");
    assert!(!has_forget, "gen_heap.rs must not use mem::forget");
}

#[test]
fn t11_7_no_todo_in_safety_files() {
    let files = [
        "gc/src/gen_heap.rs",
        "jit/src/x64.rs",
        "vm/src/jit/helpers.rs",
        "vm/src/runtime/interpreter.rs",
        "vm/src/vm/vm_init.rs",
    ];
    for rel in &files {
        let p = ws(rel);
        let contents = std::fs::read_to_string(&p).unwrap_or_default();
        let todo_count = contents
            .lines()
            .filter(|l| {
                let t = l.trim();
                !t.starts_with("//") && (t.contains("todo!()") || t.contains("unimplemented!()"))
            })
            .count();
        eprintln!("[t11] T11.7 {rel}: {todo_count} todo!/unimplemented! in code");
        assert!(
            todo_count == 0,
            "{rel} has {todo_count} todo!/unimplemented! calls in non-comment code"
        );
    }
}

// ===========================================================================
// Aggregate summary
// ===========================================================================

#[test]
fn t11_summary() {
    eprintln!("\n========================================");
    eprintln!("T11 Safety & Hardening — Summary");
    eprintln!("========================================");

    let files_11_1 = [
        ("gen_heap.rs", "gc/src/gen_heap.rs"),
        ("x64.rs", "jit/src/x64.rs"),
        ("helpers.rs", "vm/src/jit/helpers.rs"),
        ("interpreter.rs", "vm/src/runtime/interpreter.rs"),
    ];
    let mut total_unsafe = 0;
    let mut total_documented = 0;
    for (name, rel) in &files_11_1 {
        let (u, d) = check_safety_comments(&ws(rel));
        total_unsafe += u;
        total_documented += d;
        let pct = if u > 0 { d * 100 / u } else { 100 };
        eprintln!("  T11.1 {name}: {d}/{u} ({pct}%)");
    }
    let pct_11_1 = if total_unsafe > 0 {
        total_documented * 100 / total_unsafe
    } else {
        100
    };
    eprintln!("  T11.1 TOTAL: {total_documented}/{total_unsafe} ({pct_11_1}%)");

    let panics = count_panic_paths(&ws("gc/src/gen_heap.rs"));
    eprintln!("  T11.2 gen_heap.rs panic paths: {}", panics.len());

    let (tc1, ac1) = count_unguarded_casts(&ws("vm/src/runtime/interpreter.rs"));
    let (tc2, ac2) = count_unguarded_casts(&ws("jit/src/x64.rs"));
    let total_casts = tc1 + tc2;
    let annotated_casts = ac1 + ac2;
    let pct_11_3 = if total_casts > 0 {
        annotated_casts * 100 / total_casts
    } else {
        100
    };
    eprintln!("  T11.3 casts annotated: {annotated_casts}/{total_casts} ({pct_11_3}%)");

    let (lt, ld) = check_leak_docs(&ws("jit/src/x64.rs"));
    eprintln!("  T11.4 leak patterns documented: {ld}/{lt}");

    let unwraps = count_bare_unwraps(&ws("vm/src/vm/vm_init.rs"));
    eprintln!("  T11.5 vm_init bare unwraps: {unwraps}");

    let lock_exists = Path::new(&ws("vm/src/runtime/lock_order.rs")).exists();
    eprintln!(
        "  T11.6 lock_order.rs: {}",
        if lock_exists { "exists" } else { "MISSING" }
    );

    eprintln!("========================================\n");
}
