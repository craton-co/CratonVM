// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The repository check that rejects new metadata-table bypasses.
//!
//! C2 review P0 acceptance: *"No direct metadata-table bypass remains; a
//! repository check rejects new bypasses."* This module is the second half.
//!
//! # What counts as a bypass
//!
//! Seven literals, each one a way to answer "what member does this symbolic
//! reference name" without going through [`super::MemberResolver`]:
//!
//! | needle | what it reaches |
//! |---|---|
//! | `find_method_recursive(` | the class store's method hierarchy walk |
//! | `find_field_recursive(` | the class store's field hierarchy walk |
//! | `.resolution_cache` | the per-`(class, cp-index)` `ResolutionCache` |
//! | `.link_resolver` | the reflective `(class, name, descriptor)` `LinkResolver` |
//! | `access_control::check_` | any `classloading::access_control` entry point |
//! | `resolve_method_metadata(` | the interpreter's method-resolution core |
//! | `resolve_field_ref(` | the interpreter's field-resolution core |
//!
//! # Why counts and not just presence
//!
//! [`ALLOWED`] carries an exact count per `(file, needle)` rather than a bare
//! "this file may bypass". Presence-only would let a file that already has one
//! bypass grow ten more silently, which is precisely the drift this gate
//! exists to stop — and this codebase has an inventory of 27
//! `find_method_recursive(` calls in a single file, so "the file is already on
//! the list" is not a useful unit of permission.
//!
//! An exact count also fails when a bypass is *removed*, which is deliberate:
//! the row is then stale and must be tightened, exactly as
//! `flag_declaration_guard.rs::the_allowlist_has_no_dead_rows` prunes an
//! exemption whose read site is gone. Migrating a call site is supposed to be a
//! two-line diff — the migration, and the number.
//!
//! # Precision
//!
//! Three rules, each of which was needed to make the scan usable on this tree:
//!
//! 1. **Whole-line comments are skipped.** This tree carries hundreds of
//!    `/// … find_method_recursive …` doc comments and block-comment
//!    continuation lines that discuss the resolution paths at length. The
//!    comment test is copied from `types/tests/flag_declaration_guard.rs`
//!    including its warning: a bare `starts_with('*')` is *wrong*, because
//!    `*ON.get_or_init(…)` is a deref and not a block-comment continuation.
//! 2. **A declaration is not a call.** A line containing `fn <needle>` is the
//!    definition, so `pub(crate) fn resolve_field_ref(` in
//!    `interpreter/field_access.rs` does not count against
//!    `interpreter/field_access.rs`. Without this the owner of a resolution
//!    core would need an allowlist row to declare itself.
//! 3. **The field needles carry their leading dot.** `.resolution_cache`
//!    matches `shared.classes.resolution_cache` and does **not** match
//!    `shared.classes.initiating_resolution_cache` — a different cache
//!    (the JVMS §5.4.3 initiating-loader table) that is not a member-resolution
//!    bypass. Prose that says "the resolution_cache" has no leading dot and is
//!    skipped as a comment anyway.
//!
//! Two directories are excluded from the walk by rule rather than by
//! allowlist row, because they *own* the things the needles name:
//!
//! * `classloading/` — defines `find_*_recursive`, `ResolutionCache`,
//!   `LinkResolver` and `access_control`. A crate is allowed to use its own
//!   internals.
//! * `vm/src/runtime/resolve/` — this module and [`super`], which are the
//!   sanctioned wrapper. Excluding it also keeps this file's own pattern
//!   literals from counting as hits.
//!
//! Known blind spot, stated rather than papered over: a bypass reached through
//! an alias (`use cratonvm_classloading::find_method_recursive as walk;`) or
//! through a re-export under a different name is invisible to a literal scan,
//! exactly as `flag_declaration_guard.rs` cannot judge a `format!`-assembled
//! flag name. The needles were chosen to be the names that actually appear at
//! all 30 known sites.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Directory names never scanned.
const SKIPPED_DIRS: &[&str] = &["target", ".git", "apps", "node_modules"];

/// Path prefixes (workspace-relative, forward slashes) excluded by rule.
/// See the module docs for why each of these owns the needles rather than
/// bypassing them.
const OWNER_PREFIXES: &[&str] = &["classloading/", "vm/src/runtime/resolve/"];

/// The literals that name a direct metadata-table access.
const NEEDLES: &[&str] = &[
    "find_method_recursive(",
    "find_field_recursive(",
    ".resolution_cache",
    ".link_resolver",
    "access_control::check_",
    "resolve_method_metadata(",
    "resolve_field_ref(",
];

/// One permitted bypass: `(path, needle, count, reason)`.
///
/// Paths are workspace-relative with forward slashes. `count` is the exact
/// number of non-comment, non-declaration occurrences.
///
/// The reason must say **why the site has not been migrated** and, where a
/// migration is planned, name its step in
/// `docs/architecture/member-resolution.md` §"Migration order". "It was here
/// first" is not a reason; every row below either names a step or says why the
/// site is not a resolution site at all.
const ALLOWED: &[(&str, &str, usize, &str)] = &[
    // ---------------------------------------------------------------
    // Not resolution at all — cache lifecycle and GC.
    // ---------------------------------------------------------------
    (
        "vm/src/memory/gc.rs",
        ".resolution_cache",
        1,
        "not a resolution: the collector walks the condy roots held by the \
         cache. Migrating this would mean routing GC through a resolver, \
         which is backwards.",
    ),
    (
        "vm/src/memory/roots.rs",
        ".resolution_cache",
        1,
        "not a resolution: root scanning, same as memory/gc.rs.",
    ),
    (
        "vm/src/vm/vm_init.rs",
        ".resolution_cache",
        1,
        "not a resolution: the redefine invalidation adapter drops entries for \
         a class. Invalidation is a cache-lifecycle operation and belongs with \
         the hook that owns it.",
    ),
    (
        "vm/src/vm/vm_init.rs",
        ".link_resolver",
        1,
        "not a resolution: the same redefine invalidation adapter, LinkResolver \
         half.",
    ),
    (
        "libcratonvm/src/lib.rs",
        ".resolution_cache",
        1,
        "not a resolution: `clear()` on VM teardown through the C ABI.",
    ),
    (
        "vm-cli/src/main.rs",
        ".resolution_cache",
        1,
        "not a resolution: `clear()`, same shape as libcratonvm.",
    ),
    // ---------------------------------------------------------------
    // Unit tests that exercise the primitives directly.
    // ---------------------------------------------------------------
    (
        // Was `vm/src/vm.rs` until 2026-08-10, when that file's 72k-line
        // `#[cfg(all(test, feature = "synthetic-jdk"))] mod tests` moved to
        // its own file. Same sites, same count, new path.
        "vm/src/vm/tests.rs",
        ".resolution_cache",
        4,
        "unit test: `ResolutionCache` call-site put/get round trip. A test of \
         the cache must touch the cache.",
    ),
    (
        "vm/src/vm/tests.rs",
        "find_method_recursive(",
        1,
        "unit test: `m2_default_interface_method_resolution` pins that the \
         hierarchy walk finds default interface methods.",
    ),
    (
        "vm/src/vm/vm_init.rs",
        "access_control::check_",
        5,
        "unit tests: the JPMS module-access assertions (`s14_*`). They call \
         the checker directly on purpose, to test the checker.",
    ),
    // ---------------------------------------------------------------
    // Migration step 1 — reflective resolution (JNI + reflection natives).
    // `MemberResolver::declared_method` / `declared_field` already implement
    // exactly what these sites do; the move is mechanical but crosses crate
    // ownership boundaries this pass does not hold.
    // ---------------------------------------------------------------
    (
        "vm/src/native/jni.rs",
        ".link_resolver",
        2,
        "migration step 1: `GetMethodID` / `GetFieldID`. Replace the \
         `resolve_or_compute` block with `MemberResolver::declared_method` / \
         `declared_field`. Out of this pass's edit scope (vm/src/native/).",
    ),
    (
        "vm/src/native/jni.rs",
        "find_method_recursive(",
        1,
        "migration step 1: the walk inside the `GetMethodID` \
         `resolve_or_compute` closure; it goes away with the row above.",
    ),
    (
        "vm/src/native/jni.rs",
        "find_field_recursive(",
        2,
        "migration step 1: the walk inside the `GetFieldID` closure. ONE walk,          written as two calls since 2026-08-28: a field is identified by name          AND descriptor, so the closure calls          `find_field_recursive_by_descriptor` first and falls back to the          name-only `find_field_recursive` (counted by          `FIELD_RESOLUTION_DESCRIPTOR_FALLBACKS`) when no field of that exact          pair exists. The empty-signature NULL case keeps the name-only search          outright, which is the second occurrence. Both go away with the same          `MemberResolver::declared_field` migration as the row above, which          already implements the same two-step.",
    ),
    (
        "vm/src/vm/vm_exec.rs",
        ".link_resolver",
        4,
        "migration step 1: the four `NativeContextImpl::link_resolver_*` \
         bridge methods. These are also the ambiguous-`None` site — \
         `link_resolver_get_method` returns `None` for both \"cache cold\" and \
         \"cached: absent\" and says so in its own comment at :6791. \
         `MemberResolver::probe_declared` returns `CacheProbe` precisely to \
         end that. Out of edit scope (vm/src/vm/).",
    ),
    (
        "native-builtins/src/lang_class.rs",
        ".link_resolver",
        8,
        "migration step 1: `Class.getDeclaredMethod` / `getMethod` / \
         `getDeclaredField` / `getField` reach the LinkResolver through the \
         `NativeContext` bridge above; they move when it does. Separate \
         crate, out of edit scope.",
    ),
    // ---------------------------------------------------------------
    // Migration step 2 — the JIT's call-site resolution.
    // ---------------------------------------------------------------
    (
        "vm/src/jit/helpers.rs",
        "find_method_recursive(",
        5,
        "migration step 2: the JIT resolves call-site targets with a bare \
         hierarchy walk and no cache, so a JIT-resolved target and an \
         interpreter-resolved target agree only by construction. Out of edit \
         scope (vm/src/jit/).",
    ),
    (
        "vm/src/jit/helpers.rs",
        "access_control::check_",
        1,
        "migration step 2: `check_class_access` for the JIT's `new`. Moves \
         with the row above onto `MemberResolver::check_member_access`.",
    ),
    // ---------------------------------------------------------------
    // Migration step 3 — the interpreter's own hot paths. Deliberately last:
    // `interpreter/invoke.rs` is the hottest file in the tree and these are
    // ~50 sites across three dispatch tiers. See the arch doc for why the
    // façade landed before the migration rather than after.
    // ---------------------------------------------------------------
    (
        "vm/src/runtime/interpreter/invoke.rs",
        "find_method_recursive(",
        3,
        "migration step 3a: invoke dispatch. The SEAM-02 split distributed \
         this cluster across several interpreter files; the per-needle totals \
         are pinned by `the_split_did_not_change_the_interpreter_budget` \
         below, so no row here has to restate the distribution.",
    ),
    (
        "vm/src/runtime/interpreter/native_override.rs",
        "find_method_recursive(",
        3,
        "migration step 3a: the override policy's share of the invoke-dispatch \
         cluster — the superclass walk that gives an abstract-class native its \
         reach has to find the method it is overriding. Relocated by the \
         SEAM-02 split, not added.",
    ),
    (
        "vm/src/runtime/interpreter/invoke.rs",
        "resolve_method_metadata(",
        1,
        "migration step 3a: the two in-module callers of the method core. \
         They stay until 3a lands, at which point the core becomes private to \
         `runtime::resolve` again.",
    ),
    (
        "vm/src/runtime/interpreter/invoke.rs",
        ".resolution_cache",
        2,
        "migration step 3a: direct cache probes on the invoke fast paths. \
         `MemberResolver::probe_method_ref` is the replacement.",
    ),
    (
        "vm/src/runtime/interpreter/dispatch_virtual.rs",
        "find_method_recursive(",
        8,
        "migration step 3a: virtual and interface dispatch — the vtable \
         fast path, the general cached path, and the native-shadow consult \
         each resolve the target they are about to call. Relocated by the \
         SEAM-02 split, not added.",
    ),
    (
        "vm/src/runtime/interpreter/dispatch_virtual.rs",
        ".resolution_cache",
        2,
        "migration step 3a: direct cache probes on the virtual fast paths. \
         MemberResolver::probe_method_ref is the replacement. Relocated by \
         the SEAM-02 split, not added.",
    ),
    (
        "vm/src/runtime/interpreter/dispatch_static.rs",
        "find_method_recursive(",
        1,
        "migration step 3a: static dispatch resolving its target once \
         before the call-site cache takes over. Relocated by the SEAM-02 \
         split, not added.",
    ),
    (
        "vm/src/runtime/interpreter/dispatch_static.rs",
        "resolve_method_metadata(",
        1,
        "migration step 3a: one of the two in-module callers of the method \
         core. They stay until 3a lands, at which point the core becomes \
         private to runtime::resolve again.",
    ),
    (
        "vm/src/runtime/interpreter/lambda.rs",
        "find_method_recursive(",
        4,
        "migration step 3a: lambda dispatch resolving the implementation \
         method a bootstrap captured. Relocated by the SEAM-02 split, not \
         added: 12 (invoke) + 8 (jit_bridge) + 3 (native_override) + 4 here \
         = 27, the pre-split total.",
    ),
    (
        "vm/src/runtime/interpreter/lambda.rs",
        "find_field_recursive(",
        5,
        "migration step 3a: field peeks on the lambda fast paths (the \
         captured-argument and tdigest getter shortcuts). All 5 of \
         invoke.rs's field peeks moved here, so that row is gone rather \
         than zeroed.",
    ),
    (
        "vm/src/runtime/interpreter/lambda.rs",
        "resolve_field_ref(",
        1,
        "migration step 3a: the last of invoke.rs's field-ref bypasses; the \
         other 10 are in jit_bridge.rs. 1 + 10 = 11, the pre-split total.",
    ),
    // The `new`-path `check_class_access` moved to `jit_bridge.rs` with
    // `resolve_jit_new_site` in the SEAM-02 split; its row moved with it (see
    // below). No row remains here, because a row with no site is permission
    // nobody is using — which is exactly what `the_allowlist_has_no_dead_rows`
    // refuses to let sit in this table.
    (
        "vm/src/runtime/interpreter/jit_bridge.rs",
        "find_method_recursive(",
        8,
        "migration step 3a: the JIT bridge's share of the invoke-dispatch cluster — \
         callee resolution for a compile request, the OSR artifact's own \
         lookup, and the native-shadow probes. Relocated by the split, not \
         added: see the arithmetic on the `invoke.rs` row above.",
    ),
    (
        "vm/src/runtime/interpreter/jit_bridge.rs",
        "resolve_field_ref(",
        10,
        "migration step 3a: field peeks the compiler needs before it can bake an offset \
         — the getter/setter inline sites and the elidable-construction \
         analysis. Relocated by the split, not added.",
    ),
    (
        "vm/src/runtime/interpreter/jit_bridge.rs",
        "access_control::check_",
        1,
        "migration step 3a: `check_class_access` on the `new` path, which is inside \
         `resolve_jit_new_site`. Relocated by the split, not added.",
    ),
    (
        "vm/src/runtime/interpreter.rs",
        "find_method_recursive(",
        2,
        "migration step 3b: the opcode dispatch loop's own walks.",
    ),
    (
        "vm/src/runtime/interpreter.rs",
        "resolve_field_ref(",
        2,
        "migration step 3b: `getfield`/`putfield` opcode handlers.",
    ),
    (
        "vm/src/runtime/interpreter.rs",
        "access_control::check_",
        1,
        "migration step 3b: `check_class_access` for `new` (the one member of \
         the access-control surface that IS wired) plus one probe.",
    ),
    (
        "vm/src/runtime/interpreter/opcodes.rs",
        "access_control::check_",
        1,
        "migration step 3b: check_class_access on the new opcode arm, which \
         moved to opcodes.rs with execute_instruction. Relocated by the \
         SEAM-02 split, not added.",
    ),
    (
        "vm/src/runtime/interpreter/field_access.rs",
        ".resolution_cache",
        3,
        "migration step 3b: the field core's own cache probe and writes. \
         These are the resolution core itself; they move into \
         `runtime::resolve` when the core does.",
    ),
    (
        "vm/src/runtime/invokedynamic.rs",
        ".resolution_cache",
        7,
        "migration step 3c: the call-site cache for `invokedynamic` and \
         method handles. This is NOT the gap constants.rs had, which is now \
         migrated: `MemberResolver::probe_constant` records a resolved \
         CONSTANT_* value, whereas `ResolvedCallSite` is a third member kind \
         alongside method and field, and giving it a resolver method is its \
         own design step.",
    ),
    // ---------------------------------------------------------------
    // Migration step 4 — hard-coded field offsets in the VM's own plumbing.
    // ---------------------------------------------------------------
    (
        "vm/src/vm/vm_util.rs",
        "find_field_recursive(",
        3,
        "migration step 4: `FileDescriptor.fd` / `.handle` / \
         `FileInputStream.fd` offset lookups during boot. Low risk, low \
         value, and it runs before much of the VM exists — last on purpose.",
    ),
    (
        "vm/src/vm/vm_exec.rs",
        "find_method_recursive(",
        9,
        "migration step 4: `NativeContext` helper walks (virtual dispatch \
         from natives, `toString` lookups). Out of edit scope (vm/src/vm/).",
    ),
];

/// The `(file, needle)` sites that define an access-control implementation.
///
/// The P0 brief asks for **one** access-control implementation. There are two,
/// and the second is not a mistake so much as a fork: reflection grew its own
/// in `native-builtins`. This list makes the count visible and stops it
/// becoming three.
const ACCESS_CONTROL_IMPLEMENTATIONS: &[(&str, &str)] = &[
    (
        "classloading/src/access_control.rs",
        "the JVMS §5.4.4 implementation. `check_field_access` and \
         `check_method_access` have zero production callers — see that file's \
         own module docs — so today it enforces module readability and \
         nothing else.",
    ),
    (
        "native-builtins/src/lang_class.rs",
        "the reflection-only implementation (`fn check_field_access` at :525), \
         reached from `Field.get*` / `Field.set*`. It is the ONLY live member \
         access check in the VM. Folding it onto \
         `MemberResolver::check_member_access` with `AccessPolicy::Full` is \
         migration step 5, and it is behaviour-affecting: the two \
         implementations do not agree (this one consults \
         `setAccessible`, the JVMS one consults nestmates and packages).",
    ),
];

// ---------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("vm/ always has a workspace root above it")
        .to_path_buf()
}

/// Whether `line` is entirely a comment, so any needle in it is prose.
///
/// Copied from `types/tests/flag_declaration_guard.rs` together with its
/// warning: the `*` cases are spelled out rather than written as a bare
/// `starts_with('*')`, because this tree is full of `*CELL.get_or_init(…)`
/// derefs at the start of a line and a leading-`*` rule silently swallows
/// every one of them.
fn is_comment_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("//") || t == "*" || t.starts_with("* ") || t.starts_with("*/")
}

/// Count occurrences of `needle` that are real accesses.
///
/// Returns 0 for a comment line, and 0 for the line that *declares* the
/// function `needle` names.
fn hits_in_line(line: &str, needle: &str) -> usize {
    if is_comment_line(line) {
        return 0;
    }
    if needle.ends_with('(') {
        let declaration = format!("fn {needle}");
        if line.contains(&declaration) {
            return 0;
        }
    }
    let mut count = 0usize;
    let mut rest = line;
    while let Some(at) = rest.find(needle) {
        count += 1;
        // Step one byte past the match start so an overlapping second
        // occurrence on the same line is still reachable.
        rest = &rest[at + 1..];
    }
    count
}

fn rust_sources(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
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

/// `(workspace-relative path, needle) -> count` for the whole tree.
fn scan() -> BTreeMap<(String, &'static str), usize> {
    let root = workspace_root();
    let mut sources = Vec::new();
    rust_sources(&root, &mut sources);
    assert!(
        sources.len() > 100,
        "only found {} Rust sources under {} — the walk is not reaching the \
         workspace, so this guard would pass vacuously",
        sources.len(),
        root.display()
    );

    let mut hits: BTreeMap<(String, &'static str), usize> = BTreeMap::new();
    for path in &sources {
        let relative = path.strip_prefix(&root).unwrap_or(path);
        let shown = relative.display().to_string().replace('\\', "/");
        if OWNER_PREFIXES.iter().any(|p| shown.starts_with(p)) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for needle in NEEDLES {
            let needle: &'static str = *needle;
            if !text.contains(needle) {
                continue;
            }
            let count: usize = text.lines().map(|line| hits_in_line(line, needle)).sum();
            if count > 0 {
                *hits.entry((shown.clone(), needle)).or_default() += count;
            }
        }
    }
    hits
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn no_unallowlisted_metadata_table_bypass_exists() {
    let hits = scan();
    let mut offenders: Vec<String> = Vec::new();

    for ((path, needle), count) in &hits {
        let allowed = ALLOWED
            .iter()
            .find(|(p, n, _, _)| *p == path.as_str() && *n == *needle)
            .map(|(_, _, c, _)| *c);
        match allowed {
            Some(expected) if expected == *count => {}
            Some(expected) => offenders.push(format!(
                "{path}\n      `{needle}` appears {count} time(s); the \
                 allowlist says {expected}"
            )),
            None => offenders.push(format!(
                "{path}\n      `{needle}` appears {count} time(s) and the \
                 file has no allowlist row for it"
            )),
        }
    }

    assert!(
        offenders.is_empty(),
        "{} metadata-table bypass site(s) do not match the allowlist.\n\n\
         Resolution has ONE entry point: `crate::runtime::resolve::\
         MemberResolver`. It requires a VM identity (so a `ClassId` from \
         another VM cannot be resolved against this one — ClassIds are \
         allocated per VM), it distinguishes NoSuchMethod / NoSuchField / \
         IllegalAccess / NoClassDefFound from \"not cached\", and it applies \
         access control exactly once.\n\n\
         Either route the new site through `MemberResolver`, or — if you \
         MIGRATED a site and the count went down — lower the number in \
         `ALLOWED` in this file. A stale row is as much a defect as a missing \
         one: it is permission nobody is using.\n\n  {}",
        offenders.len(),
        offenders.join("\n  ")
    );
}

#[test]
fn the_allowlist_has_no_dead_rows() {
    let hits = scan();
    let mut stale: Vec<String> = Vec::new();
    for (path, needle, count, reason) in ALLOWED {
        match hits.get(&((*path).to_string(), *needle)) {
            None => stale.push(format!(
                "{path} / `{needle}`: no site left, but {count} allowed \
                 ({reason})"
            )),
            Some(actual) if actual != count => stale.push(format!(
                "{path} / `{needle}`: {actual} site(s) left, {count} allowed \
                 ({reason})"
            )),
            Some(_) => {}
        }
    }
    assert!(
        stale.is_empty(),
        "the bypass allowlist has {} row(s) that no longer describe the tree. \
         An allowlist nobody prunes stops being a record of what is left to \
         do and becomes a blanket permission — delete or tighten \
         them:\n  {}",
        stale.len(),
        stale.join("\n  ")
    );
}

/// A file split may move bypass sites between files. It may not create them.
///
/// Every SEAM-02 step relocates rows in `ALLOWED`, and the first few restated
/// the arithmetic ("16 + 8 + 3 = 27") in a reason string that the next step
/// made stale. The invariant is per-needle and per-subtree, not per-row, so
/// state it once here and let the rows carry only their justification.
///
/// The numbers below are the pre-split totals, measured on the unmodified tree
/// at the commit this lane branched from. **A migration lowers them; nothing
/// raises them.** If you are migrating sites to `MemberResolver`, lower the
/// number here in the same commit as the row you shrink — that is the ratchet.
///
/// The exact edit that trips this: add a bypass to any interpreter file and
/// give it a row, without taking the count off another row.
#[test]
fn the_split_did_not_change_the_interpreter_budget() {
    // (needle, total permitted across `vm/src/runtime/interpreter*`)
    const INTERPRETER_TOTALS: &[(&str, usize)] = &[
        ("find_method_recursive(", 29),
        ("find_field_recursive(", 5),
        ("resolve_field_ref(", 13),
        ("resolve_method_metadata(", 2),
        // 2026-08-18: 9 -> 7. `constants.rs` is migrated — its two condy sites
        // (and the three `CONSTANT_MethodType` / `CONSTANT_MethodHandle` ones
        // that had joined them without a row of their own) now go through
        // `MemberResolver::probe_constant` / `record_constant`, so the file has
        // no bypass left and its row is gone. This is the ratchet moving in the
        // only direction it may.
        (".resolution_cache", 7),
        ("access_control::check_", 3),
    ];
    for (needle, expected) in INTERPRETER_TOTALS {
        let total: usize = ALLOWED
            .iter()
            .filter(|(path, n, _, _)| {
                n == needle && path.replace('\\', "/").contains("/runtime/interpreter")
            })
            .map(|(_, _, allowed, _)| *allowed)
            .sum();
        assert_eq!(
            total, *expected,
            "the interpreter's `{needle}` bypass budget is {total}, and the \
             pre-SEAM-02 total was {expected}. A file split moves these \
             between rows; it does not change the sum. If this is a real \
             migration to `MemberResolver`, lower the expected total here in \
             the same commit."
        );
    }
}

/// Every row must carry a reason, and the reason must not be a shrug.
#[test]
fn every_allowlist_row_states_a_reason() {
    for (path, needle, _, reason) in ALLOWED {
        assert!(
            reason.len() > 30,
            "{path} / `{needle}`: the reason is too short to be one"
        );
        let says_why = reason.contains("migration step")
            || reason.contains("not a resolution")
            || reason.contains("unit test");
        assert!(
            says_why,
            "{path} / `{needle}`: a reason must classify the row as a \
             `migration step N`, `not a resolution`, or a `unit test`. \
             Got: {reason}"
        );
    }
}

/// The P0 brief asks for one access-control implementation. There are two.
/// This pins the count so it does not become three while the fold-together is
/// pending.
#[test]
fn the_access_control_implementations_are_the_two_known_ones() {
    let root = workspace_root();
    let mut sources = Vec::new();
    rust_sources(&root, &mut sources);

    let mut found: Vec<String> = Vec::new();
    for path in &sources {
        let relative = path.strip_prefix(&root).unwrap_or(path);
        let shown = relative.display().to_string().replace('\\', "/");
        // This file names both declarations as literals, and `super` is the
        // sanctioned wrapper rather than a third implementation. Same
        // exclusion rule as the bypass scan.
        if OWNER_PREFIXES
            .iter()
            .any(|p| *p == "vm/src/runtime/resolve/" && shown.starts_with(p))
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let declares = text.lines().any(|line| {
            !is_comment_line(line)
                && (line.contains("fn check_field_access(")
                    || line.contains("fn check_method_access("))
        });
        if declares {
            found.push(shown);
        }
    }
    found.sort();
    found.dedup();

    let mut expected: Vec<String> = ACCESS_CONTROL_IMPLEMENTATIONS
        .iter()
        .map(|(p, _)| (*p).to_string())
        .collect();
    expected.sort();

    assert_eq!(
        found, expected,
        "the set of member access-control implementations changed.\n\n\
         There are supposed to be two, and that is already one too many: \
         `classloading::access_control` implements JVMS §5.4.4 (and is not \
         called from the bytecode path), while \
         `native-builtins/src/lang_class.rs` implements the reflection-only \
         check (and is the only one that actually runs). Folding them onto \
         `MemberResolver::check_member_access` is migration step 5.\n\n\
         Do not add a third."
    );
}

/// The three precision rules, exercised on synthetic source rather than on the
/// tree, so a change to either one is caught even when the tree happens not to
/// contain a case that distinguishes them.
#[test]
fn the_scanner_counts_calls_and_not_prose() {
    // Positive: an ordinary call site.
    assert_eq!(
        hits_in_line(
            "    let m = find_method_recursive(cid, name, desc, store);",
            "find_method_recursive("
        ),
        1
    );
    // Positive: a field access through the VM.
    assert_eq!(
        hits_in_line(
            "    shared.classes.resolution_cache.write().clear();",
            ".resolution_cache"
        ),
        1
    );
    // Rule 1 — prose is skipped, in all three comment shapes.
    assert!(is_comment_line(
        "/// walks via find_method_recursive(cid, …)"
    ));
    assert!(is_comment_line("    // .resolution_cache is taken here"));
    assert!(is_comment_line("     * find_field_recursive( is the walk"));
    assert_eq!(
        hits_in_line(
            "// uses find_method_recursive(a, b)",
            "find_method_recursive("
        ),
        0
    );
    // …and the regression the flag guard's docs describe: a deref at the
    // start of a line is not a block-comment continuation.
    assert!(!is_comment_line(
        "    *CELL.get_or_init(|| shared.classes.resolution_cache.read().len())"
    ));
    assert_eq!(
        hits_in_line(
            "    *CELL.get_or_init(|| shared.classes.resolution_cache.read().len())",
            ".resolution_cache"
        ),
        1
    );
    // Rule 2 — a declaration is not a call.
    assert_eq!(
        hits_in_line("pub(crate) fn resolve_field_ref(", "resolve_field_ref("),
        0
    );
    assert_eq!(
        hits_in_line(
            "fn find_method_recursive(cid: ClassId) {",
            "find_method_recursive("
        ),
        0
    );
    // The limit of rule 2, recorded rather than discovered later: a
    // *generic* declaration writes `find_method_recursive<'a>(`, in which the
    // needle does not occur at all, so it is invisible to the scan for a
    // different reason than the rule. That is fine for this tree — the only
    // such definition is in `classloading/`, which the walk excludes — but if
    // a generic resolution core is ever added inside the scanned area, the
    // scan will not see its declaration OR its call sites written the same
    // way. Add a needle for it rather than assuming this rule covers it.
    assert_eq!(
        hits_in_line(
            "pub fn find_method_recursive<'a>(",
            "find_method_recursive("
        ),
        0
    );
    // Rule 3 — the leading dot keeps the initiating-loader table out.
    assert_eq!(
        hits_in_line(
            "    shared.classes.initiating_resolution_cache.read();",
            ".resolution_cache"
        ),
        0
    );
    // Two on one line are both counted.
    assert_eq!(
        hits_in_line(
            "    a.resolution_cache.read(); b.resolution_cache.write();",
            ".resolution_cache"
        ),
        2
    );
}

/// A planted bypass is caught, and the allowlist tolerates the sites it names.
///
/// This exercises the same comparison [`no_unallowlisted_metadata_table_bypass_exists`]
/// runs, against a synthetic hit set, so the assertion does not depend on the
/// tree's current contents.
#[test]
fn a_planted_bypass_is_caught_and_the_allowlist_is_tolerated() {
    fn verdict(
        hits: &BTreeMap<(String, &'static str), usize>,
        allowed: &[(&str, &str, usize, &str)],
    ) -> Vec<String> {
        let mut offenders = Vec::new();
        for ((path, needle), count) in hits {
            match allowed
                .iter()
                .find(|(p, n, _, _)| *p == path.as_str() && *n == *needle)
                .map(|(_, _, c, _)| *c)
            {
                Some(expected) if expected == *count => {}
                Some(expected) => offenders.push(format!("{path}/{needle}: {count} vs {expected}")),
                None => offenders.push(format!("{path}/{needle}: {count}, unlisted")),
            }
        }
        offenders
    }

    let allowed: &[(&str, &str, usize, &str)] = &[(
        "vm/src/runtime/old_site.rs",
        "find_method_recursive(",
        2,
        "migration step 9: pretend",
    )];

    // The allowlisted site at its allowlisted count is tolerated.
    let mut hits: BTreeMap<(String, &'static str), usize> = BTreeMap::new();
    hits.insert(("vm/src/runtime/old_site.rs".to_string(), NEEDLES[0]), 2);
    assert!(verdict(&hits, allowed).is_empty());

    // A brand-new file with a bypass is caught.
    hits.insert(("vm/src/runtime/new_site.rs".to_string(), NEEDLES[0]), 1);
    let offenders = verdict(&hits, allowed);
    assert_eq!(offenders.len(), 1);
    assert!(offenders[0].contains("new_site.rs"), "{offenders:?}");

    // A new bypass added to an ALREADY-allowlisted file is caught too — this
    // is the case a presence-only allowlist would miss.
    let mut hits: BTreeMap<(String, &'static str), usize> = BTreeMap::new();
    hits.insert(("vm/src/runtime/old_site.rs".to_string(), NEEDLES[0]), 3);
    let offenders = verdict(&hits, allowed);
    assert_eq!(offenders.len(), 1);
    assert!(offenders[0].contains("3 vs 2"), "{offenders:?}");

    // And a MIGRATED site leaves a stale row, which is also a failure.
    let mut hits: BTreeMap<(String, &'static str), usize> = BTreeMap::new();
    hits.insert(("vm/src/runtime/old_site.rs".to_string(), NEEDLES[0]), 1);
    let offenders = verdict(&hits, allowed);
    assert_eq!(offenders.len(), 1);
    assert!(offenders[0].contains("1 vs 2"), "{offenders:?}");
}
