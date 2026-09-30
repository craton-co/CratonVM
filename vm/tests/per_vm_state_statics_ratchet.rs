// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Ratchets on process state in `vm/src` and `classloading/src`: the count of
//! `static` declarations may not grow, and a `SharedVm`'s address may not
//! become a cache key.
//!
//! A `static` is process state. Every VM in the process shares it: an
//! embedded VM, `cratonvm-embed`'s sequential VMs, and every unit test that
//! builds two. AGENTS.md forbids process globals for compatibility state, and
//! interpreter round i1 wave 23's audit of these two trees
//! (`docs/architecture/per-vm-state.md` §10, the reviewed inventory this
//! baseline starts from) found live two-VM bugs that had each arrived without
//! review: `ANNOTATION_PROXY_CID` (a process `AtomicU32` holding one VM's
//! `ClassId`, beside the realm's per-VM copy), `BOOTSTRAP_APPENDED_CLASSES`
//! (one VM's agent-appended class names, answered to every VM), and four
//! thread-local memos keyed by the `SharedVm`'s ADDRESS, which the allocator
//! hands to the next VM after a drop. The JIT has had the first gate since
//! 2026-09-12 (`jit/tests/process_global_statics_ratchet.rs`); this is the
//! same rule for the two crates that hold most of the VM's state
//! (`docs/internal/fixed-bugs/interpreter-L5-proposal-a-per-vm-state-ratchet-for-vm-and-classloading-FIXED-20260927.md`).
//!
//! # Gate 1: the count
//!
//! Exactly the JIT ratchet's rule, so the two numbers mean the same thing: one
//! per source line in `vm/src/**/*.rs` (and, separately, in
//! `classloading/src/**/*.rs`) whose first non-blank text declares a static
//! item: an optional `pub` / `pub(...)` and whitespace, the keyword and
//! whitespace, an optional `mut` and whitespace, an ASCII identifier, optional
//! whitespace, `:`. Module-level statics, statics inside function bodies,
//! `static mut`, and each declaration line of a `thread_local!` block count.
//! **Every file counts, `#[cfg(test)]` modules and `tests.rs` files
//! included**, as in the JIT gate: telling test statics apart textually is
//! fragile, and counting them all keeps the rule exact. So a test that needs
//! process state should leak a `Box` or use a local, not add a static.
//!
//! Not counted: comment lines, `'static` lifetimes and bounds, a
//! `static $name:` inside a `macro_rules!` pattern.
//!
//! Two baselines, not one, so a merge that moves one crate's number cannot
//! hide in the other's.
//!
//! The baselines were computed on 2026-09-27 (interpreter round i1 wave 24,
//! lane L5, on the wave-23 merge `4db772067` plus that lane's commits, which
//! add and remove no static) with the equivalent
//!
//! ```text
//! grep -rhE '^\s*(pub(\([^)]*\))?\s+)?static\s+(mut\s+)?[A-Za-z_][A-Za-z0-9_]*\s*:' \
//!     vm/src --include=*.rs | wc -l            # 1331
//! grep -rhE '...same...' classloading/src --include=*.rs | wc -l   # 69
//! ```
//!
//! run from the repository root. The same command over `jit/src` printed 861,
//! the JIT ratchet's own `BASELINE` on that tree, which is the check that the
//! command and the scanner below agree.
//!
//! # Gate 2: a VM in a cache key is `vm_identity`
//!
//! `SharedVm::vm_identity` is never reissued; the `SharedVm`'s address is, as
//! soon as the VM is dropped. A memo keyed by the address answers a later VM
//! with the dropped VM's facts about ITS class ids (wave 23: a class read
//! "initialized" and its `<clinit>` skipped). So no line of `vm/src` or
//! `classloading/src` outside a comment may cast a `SharedVm` reference to
//! `usize`, except the allowlisted sites below, which pass the address as a
//! POINTER to code that dereferences it while the VM is alive and never keep
//! it as a key. The last key, `FAST_THROW_SITES` (`runtime/exceptions.rs`),
//! moved to `vm_identity` in the same change that added this gate.
//!
//! Limitation: this matches the spelling `SharedVm as usize` (which also
//! covers `*const crate::vm::SharedVm as usize`). An address taken as
//! `as *const _ as usize`, or through `Arc::as_ptr`, is not seen; the rule
//! still applies to it.
//!
//! Both needles are assembled at runtime, like the JIT gate's keyword.

use std::path::{Path, PathBuf};

/// `static` declaration lines in `vm/src` on 2026-09-27. Lower it when a
/// static is removed; never raise it to make room for a new one without a
/// paragraph here saying why the new one is not per-VM state.
///
/// 1331 -> 1337 -> 1336 (interpreter round i1 wave 25, lane L5). The wave-24
/// merge brought six statics in beside this gate, each reviewed here as not
/// per-VM state:
///
/// * wave 24 lane L1, `debug/mod.rs`: `METHOD_ENTRY_REQUEST_VMS` (a count of
///   VMs with a JDWP `MethodEntry` request, a one-load pre-filter that only
///   over-approximates) and the thread-local `ENTERED_FRAMES` (frame depths of
///   the calling thread, consumed by the same thread's suspend point);
/// * the `dev` merge `9e252c8b2` (JIT round 11 waves 16, 18, 19 and gc-common
///   wave 36-a): `threading/monitor.rs` `INFLATED_MONITOR_CACHE` (keyed by the
///   monitor table's id and index epoch) and `BUDGET` (an environment flag
///   cache); `jit/helpers.rs` `NEW_CP_SITE_MEMO` and `MD_OURS_CLASS_MEMO`
///   (thread-local memos keyed by `vm_identity`).
///
/// Then one removed: `interpreter/constants.rs` `ANY_RESOLUTION_FAILURE_RECORDED`
/// became `ClassRealm::resolution_failure_recorded`.
///
/// 1336 -> 1337 at the wave-25 merge. Lane L4 folded `EMPTY_BODY_ELIDED` and
/// `EMPTY_BODY_FRAMED` into the process-wide diagnostic tally
/// `FRAMELESS_CENSUS`, and added the thread-local `TRIVIAL_CTOR_MEMO`. Lane L3
/// added the thread-local `NO_HISTORY_SEEN` (`obsolete_frames.rs`), keyed by
/// `vm_identity` and voided when the redefinition count moves. None of them
/// carries one VM's answers into another.
///
/// 1337 -> 1338 (interpreter round i1 wave 26, lane L7): the thread-local
/// `LOOP_POLL` (`threading/gc_barrier.rs`), each OS thread's dispatch-loop
/// poll word. Not per-VM state: each barrier the word is registered with adds
/// and removes its OWN pause count in it, and its move trigger only sends the
/// loop to a slow path that compares the stack's own count. (Per OS thread
/// rather than per `JvmThread`, so a pause touches one word per OS thread
/// instead of one per parked virtual thread.) Note that the `dev` base of
/// this wave, `ebdc885de`, already declared 1348 before this change (the
/// gengc round's statics); that difference is not reviewed here.
///
/// 1338 -> 1349 at the same merge: the `dev` statics the note above left
/// unreviewed, from the gengc round 5 wave 1 merges between `4a816af32` and
/// `ebdc885de`. `jit/conservative_roots.rs` and `native/jni.rs`: process-wide
/// diagnostic tallies (`OWN_STACK_FOREIGN_SP`, `JIT_REMAP_REFUSED_OFF_STACK`,
/// the three `NATIVE_SLOTS_*` counters, `FOREIGN_ATTACHMENTS_REAPED` /
/// `_LEAKED`) and the thread-local `FOREIGN_THREAD_BOX` (the calling foreign
/// thread's own attachment); `threading/thread_registry.rs`: the thread-local
/// `CACHED_LIMITS` (the calling thread's stack bounds);
/// `interpreter/constants.rs`, `dispatch_static.rs`, `jit_bridge.rs`: two
/// environment-flag caches (`ON`) and two diagnostic line limiters (`LINES`).
/// None holds one VM's answers.
///
/// 1349 -> 1351 at the wave-26 landing, `dev`'s gengc round 5 wave 3 merged in:
/// `jit/conservative_roots.rs` `LIVESET_CENSUS` (a process-wide diagnostic
/// tally) and `memory/gc.rs` the thread-local `RETAIN_UNLOADED_LAYOUTS` (a
/// flag the calling thread sets around its own unload sweep).
///
/// 1351 -> 1352 at the wave-27 landing, `dev`'s gengc round 5 wave 4 merged
/// in: `memory/roots.rs` the thread-local `DEFERRAL_REFUSED` (the calling
/// thread's own refusal set for one root walk, keyed by the walk's id).
///
/// 1352 -> 1404 at the merge of `dev` into JIT round 12: +52 from
/// `9e252c8b2` -> `c9dfa9208` (53 statics added and one removed, 1336 -> 1388,
/// on a base that predates this gate; 1351 -> 1403 before the wave-27 landing
/// above). Reviewed at the merge:
///
/// * 37 `OnceLock<bool>` environment-flag caches (`ON`, `G`, `FLAG`, `SCOPED`)
///   and `threading/monitor.rs` `CAP` (a spin-cap flag cache): process
///   configuration, read once;
/// * process-wide diagnostic tallies: `deopt_resume.rs` `DOOR_RERUNS` (and
///   `DEOPT_FRAME_BAILS`, re-declared with a thirteenth row: one removed, one
///   added) and `threading/monitor.rs` `CONTENTION_CENSUS`;
/// * two immutable `JitInvokeInfo` descriptors in `jit/helpers.rs`
///   (`LAMBDA_GET_DIRECT_INFO`, `DOUBLE_VALUE_OF_BOX_INFO`);
/// * thread-local memos keyed by `vm_identity` (`jit/helpers.rs`
///   `REDEFINED_CLASS_MEMO`, `CALLEE_TABLE_VERDICTS`, `LAMBDA_GET_MEMO`,
///   `OPTIMIZING_CATCH_EXITS` -- the last keyed by the `SharedVm` address
///   until this merge moved it to `vm_identity` under gate 2 -- and
///   `jit_npe_message.rs` `NPE_MESSAGE_MEMO`), and `interpreter.rs`
///   `LOOP_EXTENTS_MEMO`, keyed by the `Arc` of the frame's code it holds;
/// * per-thread scratch for one operation: `stackwalker.rs` `TRAP_CAPTURE`,
///   `jit/conservative_roots.rs` `ARMED` / `WORDS` (a G1 band-reject pin
///   capture), and a test's `native/jni.rs` `RETURNED`.
///
/// None carries one VM's answers into another.
///
/// 1404 -> 1405 at the round 12 close-out: `runtime/env_cache.rs`
/// `jit_door_sync_instance_body`'s `CACHE`, an environment-flag cache
/// (`CRATONVM_JIT_DOOR_SYNC_INSTANCE_BODY`) read on every synchronized call
/// through the JIT's bytecode-callee door, where an uncached flag read would
/// cost a hash lookup per call. Process-invariant, like every `MemoSlot` there.
///
/// 1405 -> 1406 in JIT round 13 (wave 2, lane irhash): `runtime/env_cache.rs`
/// `jit_splice_keeps_static_intrinsic`'s `MemoSlot`, a process-invariant cache
/// of the `CRATONVM_JIT_SPLICE_KEEPS_STATIC_INTRINSIC` flag read on the inline
/// resolver's path (the file's own pattern for every switch it caches).
///
/// 1406 -> 1407 in JIT round 13 (wave 6, lane sync4): `runtime/env_cache.rs`
/// `jit_self_locking_door_skip`'s `MemoSlot`, the same kind of process-invariant
/// cache of `CRATONVM_JIT_SELF_LOCKING_DOOR_SKIP`, read on every synchronized
/// call through the interpreter's JIT doors.
///
/// +1 (interpreter round i1 wave 29, lane L1): the thread-local
/// `HELD_NATIVE` (`runtime/interpreter/jvmti_events.rs`, `experimental-debug`
/// only), the native method a reflective or method-handle call is about to
/// run, named by `vm_exec::invoke_on_class_shared_inner` for the native-call
/// funnel's method-event report. Not per-VM state: `with_held_native` sets it
/// around exactly one funnel call on the calling thread and restores the
/// prior value when that call returns, and the funnel's report takes (clears)
/// it before the native runs, so it never outlives the call and never reaches
/// another VM's call.
///
/// Merge of JIT round 13 with interpreter round i1 wave 29: 1405 + 1 (irhash)
/// + 1 (sync4) + 1 (`HELD_NATIVE`) = 1408.
///
/// 1408 -> 1406 (2026-09-28, JIT round 13 wave 8, lane proxy5):
/// `runtime/proxy.rs` `stats::PROXY_INSTANCES_CREATED` and `PROXY_DISPATCHES`
/// removed with the dead WP2.5 half of that module. No production code bumped
/// either; the two census rows that read them (always 0) went with them.
///
/// 1406 -> 1407 (2026-09-28, JIT round 13 wave 10, lane sync6):
/// `runtime/env_cache.rs` `jit_self_locking_sync_static`'s `MemoSlot`, a cache
/// of the process-invariant `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC` flag, like
/// its `jit_self_locking_door_skip` neighbour.
///
/// 1407 -> 1408 (2026-09-28, JIT round 13 wave 11, lane callcost5):
/// `runtime/env_cache.rs` `native_callback_memo_static`'s `MemoSlot`, a cache of
/// the process-invariant `CRATONVM_NATIVE_CALLBACK_MEMO_STATIC` flag, beside the
/// `native_callback_memo` slot it pairs with.
///
/// 1408 -> 1410 (2026-09-28, GC defects round wave d10, merged after JIT
/// round 13): lane t (`vm/src/jit/xt_root_scan.rs`) added three process-wide DIAGNOSTIC tallies beside the existing `XT_*`
/// counters -- `XT_HELPER_WINDOW_BANDLESS_PEERS`, `XT_JIT_ONLY_SIGNALS_SKIPPED`,
/// `XT_JIT_ONLY_SIGNAL_MISSES` -- and one cached environment flag
/// (`OnceLock<bool>`); they count events across every VM for `[GC]` and audit
/// output and decide nothing. Lane j (`vm/src/native/jni.rs`) folded three
/// per-switch flag caches into one `OnceLock<JniSwitches>` (-2). No per-VM
/// state.
///
/// 1410 -> 1409 (interpreter round i1 wave 40, lane L5, merged after the
/// GC defects round): the thread-local `IN_FLIGHT` of `interpreter/constants.rs`
/// `drive_defining_loader_load_named` (loader-drive re-entry rows keyed by a
/// bare `ClassId`) became `JvmThread::loader_drives_in_flight`.
///
/// 1409 -> 1410 (JIT round 14 wave 1, lane compat): `env_cache.rs`
/// `native_create_string_hit_or_fresh` caches its kill switch
/// (`CRATONVM_NATIVE_CREATE_STRING_HIT_OR_FRESH`) in the standard env-cache
/// `MemoSlot`, like its neighbours. A cached environment flag, no per-VM state.
///
/// 1410 -> 1408 (JIT round 14 wave 2, lane statics). MISC11-1: the compiled
/// `ldc` / static-synchronized mirror slot table (`jit/helpers.rs`
/// `ldc_global_slots`, a process `OnceLock<Mutex<FxHashMap<(vm_identity,
/// holder, cp_idx), slot>>>`) became the per-VM `NativeRealm::ldc_slots`,
/// beside the JNI global-ref table its slots live in; its teardown purge
/// (`forget_vm_ldc_slots`) is gone with it. And the dead opt-in
/// `CRATONVM_JIT_SUPERSEDE_EPOCH_SKIP_USELESS` was deleted with its
/// `env_cache.rs` `MemoSlot`.
const VM_BASELINE: usize = 1408;

/// `static` declaration lines in `classloading/src` on 2026-09-27. Same rule.
///
/// 69 -> 68 (interpreter round i1 wave 25, lane L5): `ANY_ANNOTATION_PROXY_DEFINED`
/// became the per-store `StoreEpochs::annotation_proxy_defined`.
///
/// 68 -> 67 (JIT round 14 wave 2, lane statics, MISC11-2):
/// `BOOTSTRAP_APPENDED_CLASSES` (a process map keyed by `vm_identity`, one of
/// the two-VM bugs this gate was introduced over) became the `ClassManager`
/// field `bootstrap_appended_classes`, read by natives through
/// `NativeContext::is_bootstrap_appended_class`; its teardown purge is gone.
const CLASSLOADING_BASELINE: usize = 67;

/// `(path suffix, text on the line)` pairs where a `SharedVm` is cast to
/// `usize` to be passed as a pointer, not kept as a key.
const ADDRESS_CAST_ALLOWLIST: &[(&str, &str)] = &[
    // `direct_helper_table_for`: the `SharedVm` address the static-base
    // resolver helper receives back from compiled code of this VM.
    ("jit/helpers.rs", "static_base_resolver_ctx"),
    // The not-entrant hook's `vm_ptr`: the helpers' VM word, baked into this
    // VM's own not-entrant stub.
    ("vm/vm_exec.rs", "vm_ptr"),
    // The same hook armed by the interpreter-only withdrawal (interpreter
    // round i1 wave 37, lane L1): this VM's own not-entrant stub.
    ("runtime/interpreter/jvmti_events.rs", "vm_ptr"),
];

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

/// The two scanned trees: `(label, root)`.
fn scanned_roots() -> [(&'static str, PathBuf); 2] {
    let vm = Path::new(env!("CARGO_MANIFEST_DIR"));
    [
        ("vm/src", vm.join("src")),
        ("classloading/src", vm.join("..").join("classloading").join("src")),
    ]
}

/// Every `.rs` file under `root`, sorted, as `(path relative to root with
/// forward slashes, text)`. Panics on an empty tree: the scan would pass
/// vacuously.
fn sources_under(root: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    rust_sources(root, &mut files);
    files.sort();
    assert!(
        !files.is_empty(),
        "found no Rust sources under {}; the scan would pass vacuously",
        root.display()
    );
    files
        .iter()
        .map(|path| {
            let text = std::fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            let shown = path
                .strip_prefix(root)
                .unwrap_or(path.as_path())
                .to_string_lossy()
                .replace('\\', "/");
            (shown, text)
        })
        .collect()
}

/// Whether `line` declares a static item. The JIT ratchet's scanner
/// (`jit/tests/process_global_statics_ratchet.rs` `declares_static`),
/// verbatim, so the two gates count by one rule.
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

/// Whether `line` casts a `SharedVm` to `usize` outside a comment (`needle`
/// is that spelling, assembled by the caller).
fn casts_vm_address(line: &str, needle: &str) -> bool {
    let trimmed = line.trim_start();
    !trimmed.starts_with("//") && line.contains(needle)
}

fn count_and_check(label: &str, root: &Path, baseline: usize) {
    let keyword = ["sta", "tic"].concat();
    let mut count = 0usize;
    let mut per_file = Vec::new();
    for (shown, text) in sources_under(root) {
        let n = text
            .lines()
            .filter(|line| declares_static(line, &keyword))
            .count();
        if n > 0 {
            per_file.push((shown, n));
        }
        count += n;
    }
    assert!(
        count > 0,
        "counted no static declarations under {}; the scanner is broken, not the crate clean",
        root.display()
    );
    if count > baseline {
        let listing = per_file
            .iter()
            .map(|(file, n)| format!("  {n:>4}  {file}"))
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "{label} declares {count} statics; the baseline is {baseline}.\n\
             \n\
             A static is shared by every VM in the process. Per-VM state \
             belongs on the VM (a realm under `vm/src/vm/realms/`, `SharedVm`, \
             or the `ClassManager`), or in a map keyed by `vm_identity` that the \
             VM's teardown sweeps. AGENTS.md forbids process globals for \
             compatibility state; see docs/architecture/per-vm-state.md.\n\
             \n\
             If the new static is genuinely process-invariant (a `fn` address, a \
             cache of an environment flag, a diagnostic tally), remove another \
             one or raise the baseline in vm/tests/per_vm_state_statics_ratchet.rs \
             with a paragraph saying why. A test needing process state leaks a \
             `Box` instead: test modules count too.\n\
             \n\
             Per file:\n{listing}"
        );
    }
    if count < baseline {
        eprintln!(
            "note: {label} now declares {count} statics, below the baseline of \
             {baseline}. Lower it in vm/tests/per_vm_state_statics_ratchet.rs to \
             {count} so the removal cannot be undone silently."
        );
    }
}

#[test]
fn vm_static_declarations_do_not_grow() {
    let [(label, root), _] = scanned_roots();
    count_and_check(label, &root, VM_BASELINE);
}

#[test]
fn classloading_static_declarations_do_not_grow() {
    let [_, (label, root)] = scanned_roots();
    count_and_check(label, &root, CLASSLOADING_BASELINE);
}

#[test]
fn no_cache_is_keyed_by_a_shared_vm_address() {
    let needle = ["SharedVm", " as usize"].concat();
    let mut offenders = Vec::new();
    let mut allowed_seen = vec![false; ADDRESS_CAST_ALLOWLIST.len()];
    for (label, root) in scanned_roots() {
        for (shown, text) in sources_under(&root) {
            for (i, line) in text.lines().enumerate() {
                if !casts_vm_address(line, &needle) {
                    continue;
                }
                let allowed = ADDRESS_CAST_ALLOWLIST
                    .iter()
                    .position(|(suffix, marker)| shown.ends_with(suffix) && line.contains(marker));
                match allowed {
                    Some(k) => allowed_seen[k] = true,
                    None => offenders.push(format!("  {label}/{shown}:{}: {}", i + 1, line.trim())),
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a `SharedVm` address is cast to `usize` outside the pointer-passing \
         allowlist. A VM in a cache key must be `shared.vm_identity` (never \
         reissued); the address is handed to the next VM after this one is \
         dropped, which then inherits the dropped VM's answers (interpreter \
         round i1 wave 23: a class read \"initialized\" and its <clinit> was \
         skipped). If the cast is a pointer handed to code that dereferences it \
         while the VM lives, add it to ADDRESS_CAST_ALLOWLIST with a comment.\n{}",
        offenders.join("\n")
    );
    // A stale allowlist entry would let a key slip in under its marker later.
    for (k, seen) in allowed_seen.iter().enumerate() {
        assert!(
            *seen,
            "ADDRESS_CAST_ALLOWLIST entry {:?} matches nothing any more: remove it",
            ADDRESS_CAST_ALLOWLIST[k]
        );
    }
}

/// The scanners themselves, against lines whose answer is known. A scanner
/// nobody has watched fail passes vacuously.
#[test]
fn the_scanners_count_declarations_and_casts_and_nothing_else() {
    let kw = ["sta", "tic"].concat();
    let counted = [
        format!("{kw} FOO: u8 = 0;"),
        format!("    {kw} CACHE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();"),
        format!("pub {kw} BAR: AtomicU64 = AtomicU64::new(0);"),
        format!("pub(crate) {kw} BAZ: &str = \"x\";"),
        format!("pub(super) {kw} mut QUX : i32 = 1;"),
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

    let needle = ["SharedVm", " as usize"].concat();
    let vm = "SharedVm";
    assert!(casts_vm_address(
        &format!("    let key = (shared as *const {vm} as usize, cid);"),
        &needle
    ));
    assert!(casts_vm_address(
        &format!("        ctx: shared as *const crate::vm::{vm} as usize,"),
        &needle
    ));
    assert!(!casts_vm_address(
        &format!("    // keyed by `shared as *const {vm} as usize` once"),
        &needle
    ));
    assert!(!casts_vm_address("    let key = (shared.vm_identity, cid);", &needle));
}
