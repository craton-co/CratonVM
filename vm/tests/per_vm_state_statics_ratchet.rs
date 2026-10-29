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
//! (`docs/feature-designs/jdk-only-mode.md`).
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
///
/// 1408 -> 1409 (G1 round, merged on `dev` 2026-09-30 before JIT round 15):
/// `runtime/interpreter/gc_and_alloc.rs` `g1_force_full_fresh_cycle`'s
/// `OnceLock<bool>`, a cache of the process-invariant
/// `CRATONVM_G1_FORCE_FULL_FRESH_CYCLE` flag. `dev` was red at 1409 from that
/// commit; recorded here by the JIT round 15 orchestrator.
///
/// 1409 -> 1410 (JIT round 15 wave 5, lane throw1): `runtime/exceptions.rs`
/// `throw1_switches` caches the process-invariant `CRATONVM_THROWABLE_CTOR_MEMO`
/// / `CRATONVM_THROWABLE_STANDIN_OPCODE_SCREEN` flags in one `OnceLock`; the
/// cast-forensics `SEEN` table was replaced in place (count unchanged). No
/// per-VM state.
///
/// 1410 -> 1413 (JIT round 16 wave 3, lane mh, MH15-5): `jit/helpers.rs`
/// `mh_constant_bind_enabled` caches the process-invariant
/// `CRATONVM_JIT_MH_CONSTANT_BIND` flag (`OnceLock<bool>`), and two
/// thread-local tables serve the bound constant-handle sites:
/// `MH_BIND_SITES` (keyed by the caller's `JitInvokeInfo` address, each line
/// carrying the `vm_identity` and collection epoch it was bound under) and
/// `MH_BIND_LEAVES` (immutable interned `JitInvokeInfo`s, VM-independent).
/// No per-VM state.
// 1413 -> 1415 (JIT round 16 wave 6, lane strings, STR16-6): two immutable
// `static DECIMAL_DIGITS_LATIN1_{INT,LONG}_INFO: JitInvokeInfo` in
// `vm/src/jit/helpers.rs`, the fixed call descriptors of the decimal-digits
// direct helpers (`CRATONVM_JIT_DECIMAL_DIGITS_DIRECT`). Constant data, no
// per-VM or compatibility state.
// 1415 -> 1416 (JIT round 16 wave 8, lane sync): the standard env-cache
// `MemoSlot` of `runtime/env_cache.rs` `jit_door_sync_calling_body`
// (`CRATONVM_JIT_DOOR_SYNC_CALLING_BODY`), a cached process-invariant flag.
// No per-VM state.
// 1416 -> 1417 (JIT round 16 wave 9, lane satb): the env-cache `MemoSlot` of
// `runtime/env_cache.rs` `native_store_satb` (`CRATONVM_NATIVE_STORE_SATB`), a
// cached process-invariant flag. `vm_exec.rs`'s pin-imbalance report budget
// moved from `PinFloorGuard::drop` to module level (count unchanged). No
// per-VM state.
// 1417 -> 1418 (JIT round 16 wave 12, lane nativearr, BN2-1): the env-cache
// `OnceLock<u8>` of `runtime/env_cache.rs` `native_array_tlab_mode`
// (`CRATONVM_NATIVE_ARRAY_TLAB` / `_REFILL`), a cached process-invariant flag
// pair. No per-VM state.
// 1417 -> 1434 (2026-10-02, merge of `dev` into the gcg GC round, recounted
// with this file's own `declares_static` over the merged `vm/src`): `dev`'s
// tip already counted 1438 against its own 1417, i.e. 21 declarations over.
// Since the merge base (count 1409) `dev` added 29, of which the paragraphs
// above record 7; all 29 look process-invariant by inspection --
// `JitInvokeInfo` call descriptors and thread-local memo tables in
// `jit/helpers.rs` (+16), `native_callee_memo.rs`'s poly memo and flag cache
// (+4), `vm_exec.rs` flag latches and diagnostic tallies (+4), `env_cache.rs`
// `MemoSlot`s (+2), one each in `conservative_roots.rs`,
// `interpreter/invoke.rs` and `vm/vm_object.rs`. The gcg round removed 4
// (`alloc_class_cache.rs`, `instrument.rs`, `interpreter/jvmti_events.rs`,
// `runtime/jvmti.rs`: per-VM reads replacing process caches): 1438 - 4 = 1434.
// Owners of the `dev` additions should confirm none holds per-VM state and
// lower this as they retire any.
// 1434 -> 1435 (2026-10-02, second merge of `dev` into the gcg round): `dev`'s
// `native_array_tlab_mode` env-cache `OnceLock<u8>` above (+1).
// 1435 -> 1437 (strings round 2, `str17`): two immutable `JitInvokeInfo`
// statics in `vm/src/jit/helpers.rs`, `VECTORIZED_HASH_CODE_INFO` and
// `BYTE_ARRAY_CLONE_INFO` (renamed `CLONE_VIRTUAL_INFO` in j27, now shared by
// every virtual clone helper's decline), the fixed call descriptors of the
// `ArraysSupport.vectorizedHashCode` and `byte[].clone()` direct helpers' cold
// arms (`CRATONVM_JIT_VECTORIZED_HASH_DIRECT`,
// `CRATONVM_JIT_BYTE_ARRAY_CLONE_DIRECT`). Constant data, no per-VM or
// compatibility state.
// 1437 -> 1436 (gci GC round wave i11, lane rp, R7): `runtime/interpreter/
// gc_and_alloc.rs` `REFERENT_RESTORE_REFUSALS`, a process-wide count of refused
// referent restores that summed every VM's, became the per-VM
// `ReferenceProcessor::restore_refusals` (`gc/src/reference.rs`).
// 1436 -> 1433 (gci GC round wave i12, lane sx): three process `OnceLock`
// latches of GC policy switches now read the flag snapshot per call (each is
// read once per pause, root collection or forced full cycle), so a
// per-thread test override reaches them: `memory/roots.rs`
// `dbg_no_peer_pin_divert` (with its `gen_heap.rs` twin, which must answer
// alike), `jit/xt_root_scan.rs` `takeover_signal_jit_only_enabled`, and
// `runtime/interpreter/gc_and_alloc.rs` `g1_force_full_fresh_cycle`.
// NOTE: this file's own scanner counted 1463 declarations in `vm/src` before
// that change (1464 at `dev` `bc3abb3d08`, whose baseline was 1437): the
// `dev` additions since 2026-10-06 (`6e233c42f1`, count 1438) were never
// recorded here, so this gate was already red by 27 on `dev`. See
// `docs/feature-designs/jdk-only-mode.md`.
// 1433 -> 1432 (gci GC round wave i13, lane jc): `native/jni.rs`'s
// `thread_local!` `JNI_CRITICAL_COPIES` became the per-VM
// `JniGlobalRefs::critical_copies`; no other wave-i13 lane added a static
// (this scanner: 1460 at snapshot `gci-snap-i10a`, 1459 after wave i13 -- the
// 27-declaration gap from `dev` noted above is unchanged).
// 1432 -> 1450 (gci GC round wave i14, lane sx): the reviewed count. This
// scanner read 1459 before the lane (the 27 unrecorded `dev` declarations
// over 1432). Nine process `OnceLock<bool>` latches became `SharedVm` fields
// read once at construction, so a VM built under a per-thread override sees
// it and a second VM no longer inherits the first reader's answer:
// `runtime/interpreter/lambda.rs`'s five lambda switches and
// `invoke_fast.rs`'s `lambda_door_compiled_enabled` (all six now
// `SharedVm::lambda_switches`), `runtime/interpreter.rs`'s `FROM_OUTERMOST`
// (`SharedVm::deopt_chain_run_from_outermost`), `jit/helpers.rs`'s
// `ffm_direct_door_enabled` (`SharedVm::jit_ffm_direct_door`) and
// `vm/vm_exec.rs`'s `g1_gray_prov_dbg` (`SharedVm::dbg_g1_gray_prov`):
// 1459 - 9 = 1450. The 18 left over 1432 are the `dev` declarations added
// since `6e233c42f1` that this lane reviewed and kept, none holding one VM's
// answers for another:
// * per-thread scratch for one call, or per-thread memos keyed by
//   `vm_identity`: `interpreter.rs` `EXECUTE_METHOD_HINT` (validated by name
//   and descriptor at use), `lambda.rs` `LAMBDA_CAST_PASS_MEMO`,
//   `LAMBDA_DOOR_SITES`, `LAMBDA_DOOR_LIVE`, `LAMBDA_IMPL_REFUSED`,
//   `LAMBDA_ONESHOT_SITE_CACHE`, `opcodes.rs` `system_stream_field`'s `MEMO`,
//   `jit/helpers.rs` `FFM_DIRECT_DOOR_SITES` and `LEAN_NATIVE_SITES`,
//   `vm/vm_exec.rs` `LEAF_CALL`, `stackwalker.rs` `REPORTED` (a debug line
//   budget), `jit_bridge.rs` `REFUSED_OPTIMIZING_PUBLICATION` (handed from the
//   offering thread to its own background task),
//   `jvmti/native_env/agent_functions.rs` `PENDING_AGENT_BODY` (handed to the
//   one thread start the same call makes);
// * process configuration over the immutable flag snapshot, read where no VM
//   is at hand: `env_cache.rs` `profile_memo_wide` / `jit_tl_raw_fields`
//   `MemoSlot`s, `stackwalker.rs` `plain_cap_build_enabled` ("the door has no
//   VM"), `jit/helpers.rs` `local_handler_lookup_dbg` (a JIT helper with only
//   the thread pointer), `conservative_roots.rs` `lazy_blocked_jit_switches`
//   and one more flag latch (net), `gc_and_alloc.rs` `DBG_ZGC_WILD` (the last two files
//   belonged to other wave-i14 lanes, so they were not moved here);
// * process-wide diagnostic tally: `conservative_roots.rs`
//   `LAZY_CHAIN_CENSUS`;
// * process-wide by design: `launcher_signals.rs` `PENDING` (a signal
//   handler's counters, async-signal-safe, delivered to the VM the launcher
//   guard names), `unified_logging.rs` `VERBOSE_LOGGER` / `VERBOSE_FLAGS`
//   (the `-Xlog` / JVMTI `SetVerboseFlag` logger is one per process, like
//   HotSpot's), `jvmti/native_env/events_more.rs` `ARMED_VMS`,
//   `GLOBAL_ARMED`, `SERVICE`, `SERVICE_WAKE`, `SERVICE_STARTED` (the router
//   for JVMTI events posted where no VM is in hand -- collector hooks, code
//   publication, `SIGQUIT`, the service thread -- whose rows carry each VM's
//   identity and a `Weak<SharedVm>`);
// * test-only: `serviceability.rs` `WINDOWS_ENDPOINT_TEST_LOCK` (one attach
//   endpoint per PID on Windows), `field_fast.rs` `body_test_vm`'s `VM` (a
//   per-call VM would be built thousands of times by the decoder loops).
// The count moves by statics removed elsewhere since `6e233c42f1` too
// (`MARKED_*`, `MATCHER_LEAF_SITES`, nine `NCS_*` call sites, and the GC
// rounds' latches recorded above). See
// `docs/feature-designs/jdk-only-mode.md`.
// 1450 -> 1473 (gci GC round wave i20, lane ob + the i20 integration, at the
// merge of `dev` `c7a40bcb0e`): `dev` counted 1493 against its own 1437 (red
// by 56 on `dev` itself); the merged tree counted 1480, i.e. the merge brought
// 30 declarations over 1450. i20/ob removed seven process flag latches, now
// read from the flag snapshot per call: the ZGC observability latches of
// `gc_events.rs` (three) and `unified_logging.rs` (three), and
// `vm/vm_init.rs` `zgc_uncommit_jfr`'s second latch of
// `CRATONVM_ZGC_JFR_EVENTS` (hunk X3). The other 23 were reviewed and kept,
// none holding one VM's answers for another:
// * per-thread scratch for one call or pause: `interpreter.rs`
//   `SOFT_CLEAR_ALL_APPLIED`, `COMPILED_THROW_SITE`; `gc_events.rs`
//   `ZGC_FORCED_DOOR_OCCUPANCY`; `gc_and_alloc.rs` `SOFT_PRECISE_ROOT_NAMED`;
//   `memory/roots.rs` `CONSERVATIVE_ROOT_RANGES`; `native/jni.rs`
//   `FOREIGN_DEFAULT_NAME` (this OS thread's own attachment);
// * per-thread memos and report latches keyed by `vm_identity`:
//   `interpreter/invoke.rs` `JAVA_OK`; `jit/helpers.rs`
//   `BYTE_BUFFER_VIEW_READ_LAYOUT`, `VH_ARRAY_CLASS_MEMO` and the five
//   `*_REPORTED_VM` cells;
// * process configuration (flag latches on hot paths): `interpreter.rs`
//   `zgc_multiarray_tlab_enabled`, `jit/helpers.rs`
//   `varhandle_array_element_fast_enabled`;
// * immutable descriptors: `jit/helpers.rs` `SYSTEM_ARRAYCOPY_INFO`,
//   `VH_BYTE_BUFFER_VIEW_WRITE_INFOS`;
// * process-wide diagnostic tallies: `jit/helpers.rs`
//   `COMPACT_ELIGIBLE_RECEIVERS`, `exec_sampler.rs` `INCLUSIVE`;
// * test-only: `vm_exec.rs` `CALLS`, `SEEN`, `TEST_OOM_RESTARTABLE`.
// The other i20 lanes added no static. See
// `docs/feature-designs/jdk-only-mode.md` and
// `i20-integration-report.md`.
// gci GC round wave i23: unchanged at 1473. Lane r6's process flag latch
// (`conservative_roots.rs` `generation_keyed_memo_enabled`) was replaced by a
// per-call flag read before the build, so no static was added; rf grew
// `LIVESET_CENSUS` from 7 to 9 entries on the same declaration line. See
// `docs/feature-designs/jdk-only-mode.md`.
// 1473 -> 1470 (gci GC round wave i27, lane kd): three process `OnceLock`
// latches became `SharedVm` fields read once at construction, so a VM built
// under a per-thread override keeps its own answer: `native/jni.rs`
// `jni_switches`'s `SWITCHES` (the JNI in-native package,
// `SharedVm::jni_switches`, resolved per call from the thread's JNI context
// or foreign attachment; `gch-h2jr-review-findings-RETIRED-RULING-20261018` item 9),
// `runtime/interpreter.rs` `zgc_multiarray_tlab_enabled`'s `ON`
// (`SharedVm::zgc_multiarray_tlab`) and `jit/helpers.rs`
// `varhandle_array_element_fast_enabled`'s `G`
// (`SharedVm::jit_varhandle_array_element_fast`). The other 21 of i20/ob's
// table stay as classified above. See
// `docs/feature-designs/jdk-only-mode.md`.
//
// 1470 -> 1472 (merge of origin/dev, 2026-10-16): dev's interpreter i5 wave 100 L1
// adds two per-thread buffers (`thread_local!` `LEAVE_KEY_ROOTS` / `LEAVE_KEY_SPARE`,
// the quiet blocking-leave key's recycled lists); per thread, no process state.
//
// 1472 -> 1477 (merge of origin/dev into interpreter round i5, 2026-10-16): dev's
// netty compression-wall JIT follow-up (`00a5334746`) adds five in `jit/helpers.rs`:
// the `thread_local!` memos `CHECKCAST_PASS_MEMO` / `CHECKCAST_PASS_MEMO_NEXT` and
// `GATE_DECISION_MEMO`, whose rows carry the VM's identity (`vm: usize`) and are
// per thread, and two `OnceLock<bool>` caches of their switches
// (`CRATONVM_JIT_CHECKCAST_OBJECT_MEMO`, `CRATONVM_JIT_GATE_DECISION_MEMO`), whose
// arms give the same answer. No per-VM compatibility state.
//
// 1477 -> 1476 (interpreter round i5 wave 105, lane L5,
// `i101-L5-remaining-loader-family-process-globals`): `vm_exec.rs`
// `allocate_loader_id`'s process counter `NEXT_LOADER_ID` became the per-VM
// `SharedVm::next_loader_id`. Every table keyed by a namespace id is per VM
// or carries the VM's identity in its key (`cglib_enhancer.rs`'s two tables
// gained it the same wave), so a second VM re-using ids aliases nothing.
//
// 1476 -> 1477 (merge of the gci round, 2026-10-17; gci after i46, 2026-10-16): `threading/gc_barrier.rs`'s test module
// `coverage_slot_tests_serialised`'s `SLOT_TESTS` mutex. Test-only: the sixteen
// coverage-slot tests share the process-wide coverage slot, so they take one lock
// instead of racing under the parallel harness
// (`docs/feature-designs/jdk-only-mode.md`).
//
// 1477 -> 1474 (JIT round 21 wave 4, lane compat; rev-vmjit item 3 of
// `docs/feature-designs/jit-ir-optimizations.md`): the three r21
// compat switch caches (`jit/helpers.rs` `thin_helper_asks_resolver_enabled`,
// `native_override.rs` `will_run_asks_resolver_enabled` /
// `forced_seal_asks_resolver_enabled`) became `SharedVm` fields read once at
// construction.
//
// 1474 -> 1467 (JIT round 21 wave 13, lane vm-tests): a recount, no static
// removed this wave. The scanner read 1470 at wave 4's parent `ada5f61a92`
// (the 1477 above was already 7 high) and 1467 once wave 5 `05a3933378` made
// the three caches above `SharedVm` fields (`jit/helpers.rs` 282 -> 281,
// `native_override.rs` 16 -> 14); `e1aff0645e` still reads 1467.
//
// 1467 -> 1484 (JIT round 21 wave 26, lane units): the merge of `dev`
// `4c834913d8` (`bb5ca7d516`). All seventeen are `dev`'s: the scanner reads
// 1465 at the merge base `f79d328992`, 1482 on `dev` (whose own baseline is
// 1477, so `dev` is already five over its gate) and 1467 on this branch's side
// (its two `jit/helpers.rs` thread-local `ON` caches, keyed by `vm_identity`,
// were inside the 1467 above). Waves 22-25 after the merge add and remove
// none. `dev`'s eighteen added and one removed, reviewed here:
//
// * environment-flag caches, process configuration read once: `vm_object.rs`
//   two `OnceLock<bool>` `ON` (`CRATONVM_ZGC_HIDDEN_NAME_UNPOOLED`,
//   `CRATONVM_ZGC_CLASS_NAME_UNPOOLED`), `vm_exec.rs` two `ON`
//   (`CRATONVM_ZGC_LAYER_MODULE_PRUNE_ROWS_FIRST`, `_LOADER_OWNED`) and a
//   `MemoSlot` `SLOT`, `jit/helpers.rs` a `MemoSlot` `SLOT`;
// * `jit/helpers.rs` the thread-local `DISPATCH_MEMO_CLASS_NAMES`, a flush
//   key whose first field is the `vm_identity`;
// * `runtime/frame.rs` `PADDED_CODE_LIMIT` (the entry limit of the already
//   process-wide padded-code memo, a sizing hint that changes no answer) and
//   the diagnostic tallies `PADDED_CODE_EVICTIONS` / `_EVICTED` / `_CLEARS`;
// * `runtime/vtable.rs` `EMPTY`, one immutable empty exception table shared
//   by every snapshot (no answer can differ, nothing compares by address);
// * test-only thread-local hooks and counters: `runtime/native_oom.rs`
//   `FORCED_DOOR` / `FORCED_TRY` / `FORCED_TRY_ALL` (forced-refusal arms),
//   `vm_exec.rs` `OBSERVER` and `CONTROL_CALLS`, `vm_object.rs`
//   `zs9nd8_adoption_name_allocs::N`;
// * removed: `gc_and_alloc.rs` `DBG_ZGC_WILD`.
//
// None carries one VM's answers into another.
//
// 1484 -> 1501 (JIT round 22 wave 7, lane review, 2026-10-20). All seventeen
// are `dev`'s (`dev` `e82bd964e0` scans 1501; round 22's waves 1-7 add none,
// the wave-6 tree scans 1501 too). By a line diff against `9e732a0e5f`, where
// 1484 was set:
// * environment-flag caches, process configuration read once: five `MemoSlot`
//   `SLOT`s -- `memory/native_roots.rs` one (`defer_or_root_freed_old_filter`),
//   `runtime/interpreter.rs` three, `vm/vm_exec.rs` one; `runtime/stackwalker.rs`
//   `OnceLock<bool>` `G` (`plain_cap_build_enabled`,
//   `CRATONVM_THROWABLE_PLAIN_CAP_BUILD`; the door has no VM to latch it on);
// * per-thread facts, not per-VM: `runtime/interpreter.rs` thread-locals
//   `REENTRY_STACK_RANGE` (this OS thread's stack bounds, queried once) and
//   `REENTRY_STACK_GUARD_SUSPENDED`; `interpreter/invoke.rs` thread-local
//   `MH_SITE_LINKED`, keyed by `(vm_identity, caller class id, door)`;
// * process-wide OS state: `runtime/crash_handler.rs` `PREV_UNHANDLED_FILTER`
//   (the previous Windows unhandled-exception filter, chained to; one per
//   process by the OS's own rule);
// * diagnostic process tallies, printed under a debug flag and read by no
//   answer: `memory/native_roots.rs` `ITL_PENDING_FREED_OLD_DROPPED`,
//   `interpreter/gc_and_alloc.rs` `STW_PROBE_WALKS` / `_ABANDONED` /
//   `_VISITED` / `_WALK_NS` (gcj j3/x2), `interpreter/jit_bridge.rs`
//   `INVOKESPECIAL_SELECTION_CENSUS` and `C2_FELL_THROUGH_ROWLESS_FIELD_COUNT`;
// * re-typed, the same declarations (no change in the count):
//   `jit/helpers.rs` `MH_LINKED_SITES` (now split over two lines),
//   `runtime/launcher_signals.rs` `PENDING` (3 -> 4 signal cells).
// Per file: native_roots +2, crash_handler +1, interpreter +5, gc_and_alloc +4,
// invoke +1, jit_bridge +2, stackwalker +1, vm_exec +1 = 17.
// None carries one VM's answers into another.
//
// 1501 -> 1504 (zt t24, the ZGC round merged with dev 8049fd274a; the round's
// own, counted by the m5 merge lane's mirror of this scan): `lib.rs`
// `ZGC_READ_GATE_TEST_LOCK` (a `#[cfg(test)]` lock serialising the process-wide
// read-gate test switch), `env_cache.rs` a macro-arm `MemoSlot` `CACHE`
// (process configuration read once), `gc_events.rs` the thread-local
// `GC_NOTIFICATION_REDRAIN_DEPTH` (this thread's re-drain recursion depth,
// transient). None carries one VM's answers into another.
//
// 1504 -> 1506 (zt t26/hunt9, the t24/t25 lanes' two, which the merge left
// above the baseline so this gate failed on 78f1131826): `gc_and_alloc.rs`
// the thread-local `ZGC_MARK_CONTINUE_DOOR` (zt t24/zc8: whether THIS thread's
// collection request came through a door that may release a Pause Mark End
// attempt; scoped by an RAII guard, transient), and `jit/helpers.rs`
// `JIT_FIELD_ARRAY_RECEIVER_REFUSALS` (zt t25/hunt8: a diagnostic engagement
// counter beside the process-wide corrupt-cell census, never read to decide
// anything). Neither carries one VM's answers into another.
//
// 1501 -> 1516 (interpreter round i7, waves 135-142, orchestrator review,
// 2026-10-21). By a per-file count against `dev` `242191c8d3`:
// * per-thread facts, not per-VM: `debug/pop_frames.rs` thread-locals
//   `DOOR_ENTRY_DEPTH`, `DOOR_ENTRY_COMPILED`, `DOOR_RESTART` (this thread's
//   lambda-door entry frame and its pending JDWP restart);
//   `interpreter/jvmti_events.rs` thread-local `REINVOKABLE_ENTRY_DEPTH`;
//   `jit/helpers.rs` thread-local pass memos `IFACE_SPECIAL_GUARD_PASSED`,
//   `IFACE_SPECIAL_GUARD_CLASS_SENDER` and `interpreter/dispatch_virtual.rs`
//   `DOOR_YIELD_MEMO`, all keyed by `(vm_identity, ..)`;
// * environment-flag caches read once: `interpreter/constants.rs`
//   `self_name_resolves_to_self_enabled`'s `OnceLock<bool>`;
// * a diagnostic, printed under `CRATONVM_DBG_DOOR_CYCLES` and read by no
//   answer: `interpreter/jit_bridge.rs` `door_cycles` `CALLS`, `DOOR`, `BODY`,
//   `FIRST_TICK`, `FIRST_NS`, its thread-local `DEPTH` and its flag cache `ON`.
// Per file: pop_frames +3, jvmti_events +1, helpers +2, dispatch_virtual +1,
// constants +1, jit_bridge +7 = 15. Wave 138's `instrument.rs` removed-prefix
// table was a static too; wave 142 moved it into the VM's own transformer
// chain row. None carries one VM's answers into another.
//
// 1516 + 1506 - 1501 = 1521 (the round-i7 merge of dev a11c963ea9: the two
// counts above are disjoint files' additions on the same 1501 base).
//
// 1521 -> 1523 (gcj j4/rf, merged with interpreter round i7 at dev ee56a213bd):
// two override-aware `MemoSlot` caches of environment flags, read once --
// `interpreter.rs` `gen_young_pause_keeps_old_referents`
// (`CRATONVM_GEN_YOUNG_PAUSE_KEEPS_OLD_REFERENTS`) and `gc_and_alloc.rs`
// `gen_skip_shape_row_prune_enabled` (`CRATONVM_GEN_SKIP_SHAPE_ROW_PRUNE`).
// Process configuration, not per-VM answers.
//
// 1523 -> 1524 (JIT round 23 wave 8, lane W8-L2): `jit/helpers.rs`
// `gate_policy_half_memo_enabled`'s `OnceLock<bool>`, a cache of the
// `CRATONVM_JIT_GATE_POLICY_HALF_MEMO` environment switch, the same pattern as
// its neighbour `gate_decision_memo_enabled`. The memo it gates is keyed by
// `vm_identity`, so no VM's answer reaches another.
//
// 1524 -> 1525 (zt t30/cv, merged with dev adca67f852):
// `threading/gc_barrier.rs` `perf_vm_thread_cpu_on`'s `MemoSlot`, a cache of
// the `CRATONVM_PERF_VM_THREAD_CPU` environment switch read once, like the
// debug flags beside it. The CPU it gates is booked on the VM's own
// `GcBarrier`, so no VM's answer reaches another.
//
// 1525 -> 1526 (zt t31/x10, page `zgc-zt13x10-review-residuals-CLOSED-20261019`
// item 30h): `runtime/interpreter/gc_events.rs` `FULL_GC_DUMP_SCOPE`, a
// `thread_local!` holding the full-collection heap dumps ONE thread's door
// owes the `-Xlog` writer of its own collection (kept lines, the owed
// after-dump), set and taken by that thread within one door. Per-thread,
// per-collection scratch like its neighbour `ZGC_FORCED_DOOR_OCCUPANCY`; no
// VM's state reaches another, and the per-VM dump sequence and limit live on
// `HeapRealm::heap_dumps`.
//
// 1526 -> 1520 (t1 wave 4, lane Y2). The tree read 1530 at 68ad0e8031 (four
// unreviewed statics from the t1 waves 1-3 merges, above this baseline). The
// interpreter's `--jdk-only-report` observation sink in `vm/vm_exec.rs` (the
// eleven `JDK_ONLY_NATIVE_SHADOW*` / `JDK_ONLY_INTRINSIC_SHADOW_EXEMPT*`
// statics) became the per-VM `NativeRealm::native_shadow_sink`, with one
// `OnceLock` left for resolver calls made with no VM in hand
// (`process_native_shadow_sink`, which no VM's report reads): -11 + 1.
//
// 1520 -> 1526 (t1 wave 13, orchestrator, reviewing the commits since this
// baseline; the ratchet was red on dev at `54bce9533f`). Per-file counts against
// `389ff660c9` name exactly six, every one a kind the header admits:
// * `runtime/env_cache.rs` +1: an `env_flag!` arm's `MemoSlot`, an
//   environment-flag memo read once like its neighbours;
// * `runtime/interpreter/site_cache.rs` +1: the `MemoSlot` of
//   `CRATONVM_SITE_GLOBAL_RESOLUTION_STAMP` (t1 wave 10, lane L71), a switch
//   cache (the stamp it gates is the per-VM store slot's counter);
// * `runtime/interpreter/exception_dispatch.rs` +2: a `DBG_` switch's
//   `OnceLock<bool>` and its `COUNTS` diagnostic tally, process-wide by design
//   and printed only under that debug flag;
// * `runtime/interpreter/jit_bridge.rs` +1 `IR_CHAIN_TREE`: a `thread_local!`
//   flag a nested inline resolution sets and takes within one call on one
//   compiling thread (`e5e8962d25`), per-compile scratch no VM shares;
// * `runtime/perf_data_file.rs` +1 `BUILD_STAMP`: the binary's build time and
//   compiler (`sun.rt.internalVersion`), set once by the launcher: a property of
//   the executable, identical for every VM in the process.
//
// 1526 -> 1527 (g1ep4): `jit/xt_root_scan.rs` +1 `PASS_BAND`, a `thread_local!`
// copy buffer the helper-window pass keeps on the thread that ran it, so the
// next pass does not map and fault in a fresh 8 MiB band. Scratch whose
// contents no reader trusts past the words one copy wrote; no VM state.
//
// 1527 -> 1523 (j27 wave 7, lane T1IC): four `OnceLock<bool>` switch latches
// in `vm/src/jit/helpers/` became per-call reads on their cold paths:
// `dispatch.rs` `offload_site_late_register` (gpu-offload site resolution),
// `fast_native.rs` `lambda_resume_failure_raises` (a failed resume),
// `thread.rs` `division_guard_retier_enabled` (a division-guard deopt trap),
// `typecheck.rs` `jit_typecheck_null_name_trace` (a typed snapshot field).
// The same change turned 132 more latches in `vm/src/jit/**` into
// `MemoSlot`s (`cratonvm_jit::memo_flag::memo_bool`), one for one, so they
// do not move this count. NOTE: the j27 tree read 1531 before this change
// (four statics from earlier j27 waves sit above the old baseline, not
// reviewed here), so it reads 1527 after it and this gate stays red until
// those four are reviewed.
//
// 1523 -> 1526 (j27 wave 8, builder TRIA): the four were reviewed. One was
// j27's own (`osr_reentry_pending_reask.rs` `PENDING_HITS`) and is removed:
// its count is now a field on the OSR re-entry memo entry. Three came with
// the round base from dev and stay, justified:
// `conservative_roots.rs` `SELF_BLOCK_STAMP_SLOT` (a per-thread GC
// quiescence registration that unregisters on Drop, no VM state);
// `fast_native.rs` `OBJECT_CLONE_INFO` (immutable `JitInvokeInfo` data);
// `fast_native.rs` `OBJECT_CLONE_PLAIN` (a thread-local memo keyed by VM
// identity, address epoch and class id, holding no code pointer).
//
// 1526 -> 1529 (j27 merge of origin/dev 65f27899d3): dev's f69bbcc728 added
// three statics to `jit/conservative_roots.rs`, none of which holds VM state:
// `PRE_ENTRY_STALE` (a per-thread record of the JIT words a thread inherited
// on a reused stack, bounded by `PRE_ENTRY_STALE_MAX`);
// `PRE_ENTRY_STALE_THREADS` (a diagnostic tally); and the `OnceLock` latch
// of `CRATONVM_JIT_UNREG_PRE_ENTRY_STALE` (a cache of an environment flag).
const VM_BASELINE: usize = 1529;

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
///
/// 67 -> 68 (JIT round 16 wave 13, lane annorder, PX8-3 step 1): the
/// `MemoSlot` of `method_override.rs` `runtime_package_crosscheck`, a cached
/// process-invariant debug flag (`CRATONVM_DBG_RUNTIME_PACKAGE_CROSSCHECK`).
/// No per-VM state.
///
/// 68 -> 72 (recorded in gci GC round wave i14, lane sx; the scanner read 72
/// already at `dev` `bc3abb3d08`): the verifier round's four (`48ed8e15b7`,
/// `dbb6877804`, merged after this baseline was last edited), all per-thread
/// and VM-independent: `verify_insn.rs`
/// `SIG_CACHE` and `FIELD_TYPE_CACHE` (descriptor text to parsed signature /
/// name-typed `VType`, no class id), `verify_frame.rs` `LOCALS_EPOCH` (an
/// epoch compared only within one method's walk) and `vtype.rs` `WELL_KNOWN`
/// (immutable interned names). No per-VM state.
///
/// 72 -> 77 (gci GC round wave i20, lane ob, at the merge of `dev`
/// `c7a40bcb0e`): interpreter round i4 wave 95's `define_census.rs`, the
/// thread-local arm flags `NATIVE_DEFINE_ARMED`, `DEFINE_INIT_ARMED`,
/// `DEFINE_LINK_ARMED` (set around one define on the calling thread) and the
/// process-wide diagnostic tallies `PHASE_NS`, `PHASE_COUNT`. No per-VM state.
///
/// 77 -> 75 (interpreter round i5 wave 103, lane L5,
/// `i101-L5-remaining-loader-family-process-globals`): `loaders.rs`
/// `USER_LOADER_PARENTS` and its `USER_LOADER_PARENTS_NONEMPTY` latch became
/// the per-VM `UserLoaderParents`, a `ClassManager` field shared with the VM's
/// loader-identity tables.
///
/// 75 -> 76 (JIT round 23 wave 2, proxy-define page; recorded by lane L6 in
/// wave 3): `class_manager.rs` `field_trace_enabled`'s `static SLOT: MemoSlot`,
/// a cache of the process-invariant `CRATON_FIELD_TRACE` diagnostic variable
/// (asked on every define; the uncached environment read was ~1 % of the
/// define arm). `MemoSlot` re-derives under a test's thread override, so it
/// holds no per-VM state; the classloading crate has no VM context at
/// `compute_field_layout` to hang it on instead.
///
/// 76 -> 78 (interpreter round i7 wave 141, lane L5): `class_manager.rs`
/// `supertype_shape_module_clause_enabled` and
/// `interface_super_object_check_enabled`, each a `OnceLock<bool>` cache of a
/// parity switch read once per process. No per-VM state.
const CLASSLOADING_BASELINE: usize = 78;

/// `(path suffix, text on the line)` pairs where a `SharedVm` is cast to
/// `usize` to be passed as a pointer, not kept as a key.
const ADDRESS_CAST_ALLOWLIST: &[(&str, &str)] = &[
    // `direct_helper_table_for`: the `SharedVm` address the static-base
    // resolver helper receives back from compiled code of this VM.
    // (`jit/helpers.rs` until the origin/dev `00529e979f` split.)
    ("jit/helpers/table.rs", "static_base_resolver_ctx"),
    // The not-entrant hook's `vm_ptr`: the helpers' VM word, baked into this
    // VM's own not-entrant stub.
    ("vm/vm_exec.rs", "vm_ptr"),
    // The same hook armed by the interpreter-only withdrawal (interpreter
    // round i1 wave 37, lane L1): this VM's own not-entrant stub.
    ("runtime/interpreter/jvmti_events.rs", "vm_ptr"),
    // The same hook armed by a late-bind recompile retiring the body it
    // displaced (JIT round 16 wave 14, lane throw4): this VM's own stub.
    (
        "runtime/interpreter/jit_bridge.rs",
        "vm_ptr: shared as *const SharedVm",
    ),
    // A first-call guard cell's `ctx` (interpreter round i1 wave 50, lane
    // L1): the cell lives in this VM's own `jit.bridge.first_call_cells` and
    // is read only by this VM's compiled code, which hands it back to
    // `jit_first_call_resolve` while the VM lives; it keys nothing.
    ("runtime/interpreter/jit_bridge.rs", "ctx: shared as"),
    // The same hook armed by the first exact `VarHandle` mint's sweep (JIT
    // round 15 wave 5, lane vh7, `retire_varhandle_thin_bound_bodies`): this
    // VM's own not-entrant stub. (`jit/helpers.rs` until origin/dev
    // `00529e979f` split it into `jit/helpers/*.rs`.)
    ("jit/helpers/fast_native.rs", "vm_ptr"),
    // The same hook armed when a trapped body is evicted
    // (`make_trap_evicted_body_not_entrant`): this VM's own not-entrant stub.
    // The pre-split `jit/helpers.rs` row covered it too, by sharing the file.
    ("jit/helpers/thread.rs", "vm_ptr"),
    // The OSR door's judged thin `VarHandle` sites (JIT round 15 wave 6, lane
    // vh8, `compile_osr_body`): the helper's `SharedVm` word baked into this
    // VM's own `VarHandleJudgedSite` records, owned by the compiled method.
    (
        "runtime/interpreter/jit_bridge.rs",
        "the helper's `SharedVm` word",
    ),
    // The INDY21-1 guarded-leaf test (`invokedynamic.rs` test module,
    // `a_guarded_leaf_cell_dispatches_its_leaf_or_hands_back_its_owner`): the
    // helper's `SharedVm` word handed to `jit_indy_guarded_leaf`, which
    // dereferences it while the test's VM lives. The cells it builds carry
    // `vm.vm_identity`, not this word, so it keys nothing.
    ("runtime/invokedynamic.rs", "let vm_ptr = vm as *const SharedVm"),
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
        (
            "classloading/src",
            vm.join("..").join("classloading").join("src"),
        ),
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
    assert!(!casts_vm_address(
        "    let key = (shared.vm_identity, cid);",
        &needle
    ));
}
