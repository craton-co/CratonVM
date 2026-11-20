// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
        2,
        "not a resolution: the collector walks the condy roots held by the \
         cache, and class unloading drops the records of the unloaded \
         classes. Migrating this would mean routing GC through a resolver, \
         which is backwards.",
    ),
    (
        "vm/src/memory/gc.rs",
        ".link_resolver",
        1,
        "not a resolution: class unloading evicts the unloaded classes' \
         reflective link records, this VM's only (gc-common w8-e; the per-class \
         resolution hook no longer fires for an unload).",
    ),
    (
        "vm/src/memory/roots.rs",
        ".resolution_cache",
        1,
        "not a resolution: root scanning, same as memory/gc.rs.",
    ),
    (
        "libcratonvm/src/lib.rs",
        ".resolution_cache",
        1,
        "not a resolution: `clear()` on VM teardown through the C ABI.",
    ),
    (
        "vm/src/vm/realms/class_realm.rs",
        "find_method_recursive(",
        2,
        "not a resolution: `ClassRealm::unique_implementor` and its \
         watermarked twin `cha_unique_target_watermark` (JIT r23 wave 4, the \
         CHA splice) SURVEY a \
         hierarchy, it does not resolve a member to call. Class-hierarchy \
         analysis asks how many loaded concrete subtypes declare a \
         non-abstract body for one (name, descriptor); the answer is a HINT \
         that the JIT lowers behind a receiver class-id compare \
         (`InlineVerdict::UniqueConcreteMethod`), never a binding. Routing it \
         through `MemberResolver` would be wrong in both directions: the \
         survey must answer silently for a subtype that declares nothing, \
         where the resolver's contract is to distinguish NoSuchMethod from \
         `not cached`, and it must NOT apply access control, because a \
         `private` or `native` selection still counts as a provider — \
         deliberately, so a hierarchy with one ordinary body and one such \
         oddity reports `many` and binds nothing. The walk is bounded by \
         `CHA_MAX_SUBTYPES`. A wrong answer here costs a compare that always \
         fails or a missed devirtualization, never a wrong target.",
    ),
    (
        "vm/src/runtime/gpu_marshal.rs",
        "find_field_recursive(",
        2,
        "not a resolution: GPU marshalling inspects the runtime \
         layout of an already identified native buffer for its address and \
         length; there is no symbolic field reference or Java caller.",
    ),
    // ---------------------------------------------------------------
    // Unit tests that exercise the primitives directly.
    // ---------------------------------------------------------------
    // ---------------------------------------------------------------
    // Migration step 1 — reflective resolution (JNI + reflection natives).
    // `MemberResolver::declared_method` / `declared_field` already implement
    // exactly what these sites do; the move is mechanical but crosses crate
    // ownership boundaries this pass does not hold.
    // ---------------------------------------------------------------
    // ---------------------------------------------------------------
    // Migration step 2 — the JIT's call-site resolution.
    // ---------------------------------------------------------------
    // One `vm/src/jit/helpers.rs` row of 4 until origin/dev `00529e979f`
    // split that file into `vm/src/jit/helpers/*.rs`: three of the four sites
    // landed in `thread.rs`, the fourth in `typecheck.rs` (next row). Same
    // sites, same total.
    (
        "vm/src/jit/helpers/typecheck.rs",
        "find_method_recursive(",
        1,
        "migration step 2: one of the JIT's four bare call-site hierarchy \
         walks (the reason is on the `vm/src/jit/helpers/thread.rs` row), \
         moved to this file by the helpers split.",
    ),
    // The `vm/src/jit/helpers.rs` `access_control::check_` row (the JIT's
    // `new` check) is gone: interpreter round i1 wave 9 (L2) routes it
    // through `interpreter::constants::class_constant_access_denial_in` and
    // `MemberResolver::class_access_denial`.
    // ---------------------------------------------------------------
    // Migration step 3 — the interpreter's own hot paths. Deliberately last:
    // `interpreter/invoke.rs` is the hottest file in the tree and these are
    // ~50 sites across three dispatch tiers. See the arch doc for why the
    // façade landed before the migration rather than after.
    // ---------------------------------------------------------------
    (
        "vm/src/runtime/interpreter/invoke.rs",
        "find_method_recursive(",
        2,
        "migration step 3a: invoke dispatch. The SEAM-02 split distributed \
         this cluster across several interpreter files; the per-needle totals \
         are pinned by `the_split_did_not_change_the_interpreter_budget` \
         below, so no row here has to restate the distribution. Briefly 4 on \
         2026-08-28: commit 1dbbe2b36 (perf(jit): bind an invokevirtual whose \
         target is final) added a final-target binding check without moving \
         this row, and lane L3 raised the number to clear a red it did not \
         cause, recording it as debt rather than permission. Commit 1fca82a5c \
         took the site out again on 2026-08-28, so the number is back where \
         it was and the debt is paid. 2 since interpreter round i1 wave 29 \
         (lane L4): `resolved_private_invokevirtual_target` asks \
         `selection::resolve_declaring` for the JVMS 5.4.3.3 resolved method.",
    ),
    (
        "vm/src/runtime/interpreter/native_override.rs",
        "find_method_recursive(",
        3,
        "migration step 3a: the override policy's share of the invoke-dispatch \
         cluster — the superclass walk that gives an abstract-class native its \
         reach has to find the method it is overriding. Relocated by the \
         SEAM-02 split, not added. 4 from JIT round 21 lane refs2 (the JIT \
         gates' census-free restatement of resolve_dispatch step 3, recorded \
         as debt) to lane refs221: `bridge_yields_to_loaded_bytecode_for_gates` \
         asks the by-name doors' own walk \
         (`vm_exec::selected_body_dispatches_bytecode`), so the debt is paid.",
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
        5,
        "migration step 3a: virtual and interface dispatch — the vtable \
         fast path, the general cached path, and the native-shadow consult \
         each resolve the target they are about to call. Relocated by the \
         SEAM-02 split, not added. Three selection sites (the vtable and \
         interface-memo re-checks, the populate lookup) moved to \
         `resolve::selection` in interpreter round i1 wave 2.",
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
        6,
        "migration step 3a: lambda dispatch resolving the implementation \
         method a bootstrap captured. Relocated by the SEAM-02 split, not \
         added (4 here at the split). 4 -> 3 on 2026-09-23: the TDigest \
         getter shortcut that made the fourth walk was deleted with its only \
         caller, the interpreter's class-name recognizer. 3 -> 6 in JIT round 20 \
         wave 12 (lanes qLam and rev12): the instance-impl lambda site and its \
         Intrinsic-target arm walk the receiver's class for the impl the \
         bootstrap named, and the JVMS 5.4.6 selection check walks the handle \
         owner and the receiver to see whether the by-name pick is overridden \
         illegally -- the same lookup the virtual arm already made, on a member \
         the bootstrap resolved.",
    ),
    (
        "vm/src/runtime/interpreter/lambda.rs",
        "find_field_recursive(",
        5,
        "migration step 3a: field peeks on the lambda fast paths (the \
         captured-argument shortcuts). All 5 of \
         invoke.rs's field peeks moved here, so that row is gone rather \
         than zeroed.",
    ),
    // The `new`-path `check_class_access` moved to `jit_bridge.rs` with
    // `resolve_jit_new_site` in the SEAM-02 split; its row moved with it (see
    // below). No row remains here, because a row with no site is permission
    // nobody is using — which is exactly what `the_allowlist_has_no_dead_rows`
    // refuses to let sit in this table.
    (
        "vm/src/runtime/interpreter/execute_entry.rs",
        "find_method_recursive(",
        2,
        "migration step 3b: `execute`'s no-`Code` rescue walks the receiver's \
         hierarchy for a concrete override or a native. Moved verbatim out of \
         `interpreter.rs` (split-execute stage 1, interpreter round i1 wave 4), \
         not added; the interpreter-wide total below is unchanged.",
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
        "vm/src/runtime/interpreter/constants.rs",
        ".resolution_cache",
        1,
        "not a resolution: nio21 wave 11 L4's `method_type_record_is_permanent` asks the cache's \
         own configuration (`linked_constants_permanent`, the \
         `CRATONVM_RESOLUTION_LINKED_PERMANENT` kill switch read at construction); \
         it resolves and records nothing.",
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
        "native-builtins/src/lang_class/executable_access_refusal_chain.rs",
        ".link_resolver",
        2,
        "migration step 1: relocated from native-builtins/src/lang_class.rs",
    ),
    (
        "native-builtins/src/lang_class/forget_lang_class_keys.rs",
        ".link_resolver",
        5,
        "migration step 1: relocated from native-builtins/src/lang_class.rs",
    ),
    (
        "native-builtins/src/lang_class/native_field_set.rs",
        ".link_resolver",
        2,
        "migration step 1: relocated from native-builtins/src/lang_class.rs",
    ),
    (
        "vm/src/jit/helpers/thread/class_was_redefined.rs",
        "find_method_recursive(",
        2,
        "migration step 2: relocated from vm/src/jit/helpers/thread.rs",
    ),
    (
        "vm/src/jit/helpers/thread/jit_service_callee_deopt_body.rs",
        "find_method_recursive(",
        1,
        "migration step 2: relocated from vm/src/jit/helpers/thread.rs",
    ),
    (
        "vm/src/native/jni/jni_empty_array_elements.rs",
        "find_method_recursive(",
        1,
        "migration step 1: relocated from vm/src/native/jni.rs",
    ),
    (
        "vm/src/native/jni/truncate_local_frames.rs",
        ".link_resolver",
        2,
        "migration step 1: relocated from vm/src/native/jni.rs",
    ),
    (
        "vm/src/native/jni/truncate_local_frames.rs",
        "find_field_recursive(",
        2,
        "migration step 1: relocated from vm/src/native/jni.rs",
    ),
    (
        "vm/src/native/jni/truncate_local_frames.rs",
        "find_method_recursive(",
        1,
        "migration step 1: relocated from vm/src/native/jni.rs",
    ),
    (
        "vm/src/runtime/interpreter/execute.rs",
        "resolve_field_ref(",
        2,
        "migration step 3b: relocated from vm/src/runtime/interpreter.rs",
    ),
    (
        "vm/src/runtime/interpreter/jit_bridge/callee_never_taken_rows.rs",
        "find_method_recursive(",
        1,
        "migration step 3a: relocated from vm/src/runtime/interpreter/jit_bridge.rs",
    ),
    (
        "vm/src/runtime/interpreter/jit_bridge/compile_optimizing_artifact.rs",
        "find_method_recursive(",
        2,
        "migration step 3a: relocated from vm/src/runtime/interpreter/jit_bridge.rs",
    ),
    (
        "vm/src/runtime/interpreter/jit_bridge/folded_handle_member_row.rs",
        "find_method_recursive(",
        1,
        "migration step 3a: relocated from vm/src/runtime/interpreter/jit_bridge.rs",
    ),
    (
        "vm/src/runtime/interpreter/jit_bridge/jit_verdicts.rs",
        "find_method_recursive(",
        2,
        "migration step 3a: relocated from vm/src/runtime/interpreter/jit_bridge.rs",
    ),
    (
        "vm/src/runtime/interpreter/jit_bridge/never_run_static_exit_pcs.rs",
        "find_method_recursive(",
        2,
        "migration step 3a: relocated from vm/src/runtime/interpreter/jit_bridge.rs",
    ),
    (
        "vm/src/runtime/interpreter/jit_bridge/osr_continuation.rs",
        "find_method_recursive(",
        2,
        "migration step 3a: relocated from vm/src/runtime/interpreter/jit_bridge.rs",
    ),
    (
        "vm/src/runtime/interpreter/jit_bridge/osr_exception_exit.rs",
        "find_method_recursive(",
        1,
        "migration step 3a: relocated from vm/src/runtime/interpreter/jit_bridge.rs",
    ),
    (
        "vm/src/runtime/interpreter/jit_bridge/try_jit_compile_callee_slow.rs",
        "find_method_recursive(",
        2,
        "migration step 3a: relocated from vm/src/runtime/interpreter/jit_bridge.rs",
    ),
    (
        "vm/src/runtime/interpreter/jit_bridge/try_osr.rs",
        "find_method_recursive(",
        1,
        "migration step 3a: relocated from vm/src/runtime/interpreter/jit_bridge.rs",
    ),
    (
        "vm/src/runtime/interpreter/obsolete_frames/tests.rs",
        ".resolution_cache",
        1,
        "not a resolution: relocated from vm/src/runtime/interpreter/obsolete_frames.rs",
    ),
    (
        "vm/src/runtime/invokedynamic/dbg_indy_all.rs",
        ".resolution_cache",
        6,
        "migration step 3c: relocated from vm/src/runtime/invokedynamic.rs",
    ),
    (
        "vm/src/runtime/invokedynamic/descriptor_param_count_and_last_is.rs",
        ".resolution_cache",
        4,
        "migration step 3c: relocated from vm/src/runtime/invokedynamic.rs",
    ),
    (
        "vm/src/runtime/invokedynamic/execute_invokedynamic_at_uncensused.rs",
        ".resolution_cache",
        1,
        "migration step 3c: relocated from vm/src/runtime/invokedynamic.rs",
    ),
    (
        "vm/src/runtime/invokedynamic/object_methods_getter_refusal.rs",
        ".resolution_cache",
        1,
        "migration step 3c: relocated from vm/src/runtime/invokedynamic.rs",
    ),
    (
        "vm/src/runtime/invokedynamic/object_methods_getter_refusal.rs",
        "find_method_recursive(",
        1,
        "migration step 3c: relocated from vm/src/runtime/invokedynamic.rs",
    ),
    (
        "vm/src/vm/vm_exec/invoke_on_class_shared_inner.rs",
        "find_method_recursive(",
        6,
        "migration step 4: relocated from vm/src/vm/vm_exec.rs",
    ),
    (
        "vm/src/vm/vm_exec/invoke_special_shared.rs",
        "find_method_recursive(",
        2,
        "migration step 4: relocated from vm/src/vm/vm_exec.rs",
    ),
    (
        "vm/src/vm/vm_exec/invoke_static_settling_with_first.rs",
        "find_method_recursive(",
        1,
        "migration step 4: relocated from vm/src/vm/vm_exec.rs",
    ),
    (
        "vm/src/vm/vm_exec/method.rs",
        "find_method_recursive(",
        1,
        "migration step 4: relocated from vm/src/vm/vm_exec.rs",
    ),
    (
        "vm/src/vm/vm_exec/native_new_initializes_class_first.rs",
        ".link_resolver",
        4,
        "migration step 4: relocated from vm/src/vm/vm_exec.rs",
    ),
    (
        "vm/src/vm/vm_exec/native_new_initializes_class_first.rs",
        "find_method_recursive(",
        1,
        "migration step 4: relocated from vm/src/vm/vm_exec.rs",
    ),
    (
        "vm/src/vm/vm_exec/proxy_resolve_declaring_method.rs",
        "find_method_recursive(",
        2,
        "migration step 4: relocated from vm/src/vm/vm_exec.rs",
    ),
    (
        "vm/src/vm/vm_init/tests/suite_basics.rs",
        ".resolution_cache",
        4,
        "unit test: relocated from vm/src/vm/tests/suite_basics.rs",
    ),
    (
        "vm/src/vm/vm_init/tests/suite_data_streams/exception.rs",
        "find_method_recursive(",
        1,
        "unit test: relocated from vm/src/vm/tests/suite_data_streams.rs",
    ),
    (
        "vm/src/vm/vm_init/tests/suite_vm_init.rs",
        ".resolution_cache",
        2,
        "not a resolution / unit test: relocated from vm/src/vm/vm_init.rs",
    ),
    (
        "vm/src/vm/vm_init/tests/suite_vm_init.rs",
        "access_control::check_",
        5,
        "not a resolution / unit test: relocated from vm/src/vm/vm_init.rs",
    ),
    (
        "vm/src/vm/vm_init/wait_site.rs",
        ".resolution_cache",
        1,
        "not a resolution / unit test: relocated from vm/src/vm/vm_init.rs",
    ),
    (
        "vm/src/vm/vm_init/zgc_relocation_permitted.rs",
        ".link_resolver",
        1,
        "not a resolution / unit test: relocated from vm/src/vm/vm_init.rs",
    ),
    (
        "vm/src/vm/vm_init/zgc_relocation_permitted.rs",
        ".resolution_cache",
        2,
        "not a resolution / unit test: relocated from vm/src/vm/vm_init.rs",
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
         `check_method_access` are reached through \
         `MemberResolver::check_member_access` with `AccessPolicy::Full` on \
         field and method resolution misses since interpreter round i1 wave \
         26 (enforced under `--real-jdk`, counted under `--compatible`) — see \
         that file's own module docs.",
    ),
    (
        "native-builtins/src/lang_class/dbg_bb_enabled.rs",
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
        // 2026-08-28, 29 -> 30 -> 29, and the round trip is worth keeping.
        // Commit `1dbbe2b36` ("perf(jit): bind an invokevirtual whose target
        // is final") added a fourth `find_method_recursive(` to
        // `interpreter/invoke.rs` -- the final-target devirtualisation check
        // -- without moving any row, so BOTH bypass guards were red on
        // pristine `dev` and every lane was blocked behind them. Lane L3
        // raised the number to unblock, and wrote down that it was DEBT and
        // not permission: the site asked the metadata table the same question
        // `MemberResolver::probe_method_ref` answers.
        //
        // Commit `1fca82a5c` took the site out again while fixing the
        // spliced-bci deopt, so the count is 29 once more -- and REMOVED, not
        // relocated to a sibling interpreter file, which is the thing this
        // per-needle total exists to catch. Lowered back by L3 on the merge
        // that brought it in.
        //
        // 2026-09-23: 29 -> 28 and `resolve_field_ref(` 13 -> 12. Interpreter
        // round i1 wave 2 (lane L1) deleted `lambda.rs`'s TDigest getter
        // shortcut with its only caller; both of its sites went, relocated
        // nowhere.
        //
        // 2026-09-23: 28 -> 25. Lane L3 of the same wave moved three
        // `dispatch_virtual.rs` selection sites into `resolve::selection`
        // (JVMS §5.4.6), the sanctioned module.
        //
        // 2026-09-28: 25 -> 24. Wave 29 (lane L4): `invoke.rs`'s private
        // invokevirtual target asks `selection::resolve_declaring` instead of
        // walking the supertypes itself; its row went 3 -> 2.
        // 24 -> 25 (interpreter round i1 wave 37, lane L2): the JIT bridge's
        // `special_site_bind_refusal` compares the lenient walk with
        // `select_special`'s owner (its `jit_bridge.rs` row says why).
        // 25 -> 26 (JIT round 15 wave 2, lane calls2): the optimizing door's
        // lookup-only binder `published_direct_callee_bind` repeats the
        // mutator door's callee lookup (its `jit_bridge.rs` row says why).
        // 26 -> 29 (JIT round 19 proposal waves 5-7): four lookup-only walks
        // from already-resolved classes in the opt-in doors of lanes mega5,
        // sync5, eager2 and interp7, one older site gone with a merge (its
        // `jit_bridge.rs` row says why).
        // 29 -> 30 (round nio21 wave 6): the never-run static exit's lookup
        // (its `jit_bridge.rs` row says why).
        // 30 -> 33 (JIT round 20 wave 12, lanes qLam and rev12): the
        // instance-impl lambda site, its Intrinsic-target arm and the JVMS
        // 5.4.6 selection check walk an already-bootstrapped member's classes
        // (the `lambda.rs` row says why).
        // 33 -> 34 (interpreter round i4 wave 93): the callee door's
        // synchronized-bit read (its `jit_bridge.rs` row says why).
        // 34 -> 33 (r21 flip review #6 residual): the never-run static exit
        // and self-copy-by-id lookup resolve through
        // `selection::resolve_declaring`; relocated nowhere.
        ("find_method_recursive(", 33),
        ("find_field_recursive(", 5),
        // 2026-09-23: 12 -> 8. Interpreter round i1 wave 3 (lane L7) folded
        // `jit_bridge.rs`'s three copies of the compile-path field peeks into
        // one `CpResolvers` set.
        // 2026-09-30: 8 -> 2. JIT round 15 wave 4 (lane fieldowner): every
        // `jit_bridge.rs` field peek goes through `MemberResolver::field_ref`
        // (`jit_resolve_field_site`); its row is gone, relocated nowhere.
        ("resolve_field_ref(", 2),
        ("resolve_method_metadata(", 2),
        // 2026-08-18: 9 -> 7. `constants.rs` is migrated — its two condy sites
        // (and the three `CONSTANT_MethodType` / `CONSTANT_MethodHandle` ones
        // that had joined them without a row of their own) now go through
        // `MemberResolver::probe_constant` / `record_constant`, so the file has
        // no bypass left and its row is gone. This is the ratchet moving in the
        // only direction it may.
        // 7 -> 8 (interpreter round i1 wave 25): the obsolete-frames unit
        // tests' `redefine_probe` sweeps a bare test VM's cache, which no
        // production path reaches (its allowlist row says why).
        // 8 -> 9 (round nio21 wave 11 L4): `constants.rs` asks the cache's
        // own configuration flag (its row says why).
        (".resolution_cache", 9),
        // 2026-09-25: 3 -> 2. Interpreter round i1 wave 9 (lane L2): the `new`
        // arm in `opcodes.rs` reaches the class check through
        // `constants::class_constant_access_denial_in` /
        // `MemberResolver::class_access_denial`.
        //
        // 2026-09-25: 2 -> 0. Wave 10 (lane L2): the JIT `new`-site bakes in
        // `interpreter.rs` and `jit_bridge.rs` ask the interpreter's verdict
        // (`constants::class_constant_fill_admitted`) instead of a raw
        // `check_class_access` copy; both rows are gone.
        ("access_control::check_", 0),
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
