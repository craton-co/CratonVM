// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Integration coverage for the GPU-offload features landed 2026-07-11:
//!
//!   (a) reduction dispatch — `DispatchOutcome::HandledWithValue`,
//!       `SerializedResult::Scalar*`, integer-only gating (`vm/src/runtime/offload.rs`).
//!   (b) invoke-cache suppression — `DispatchOutcome::FallThroughKeepHooked`
//!       for below-`--gpu-min-work` calls (`vm/src/runtime/offload.rs`).
//!   (c) the JIT admission gate's public behavior
//!       (`vm/src/runtime/offload_jit_gate.rs`).
//!   (d) bulk i8/i16 marshalling (`vm/src/runtime/gpu_marshal.rs`).
//!   (e) `poll_submission_status(shared, handle) -> Option<PollOutcome>`
//!       (`vm/src/runtime/offload.rs`), landing concurrently in this same
//!       multi-agent session — see the note on that test below.
//!
//! # Harness
//!
//! Contrary to the older scaffold in `gpu_async_stub.rs` (which claimed no
//! "SharedVm-with-classpath" harness existed), a perfectly usable one does:
//! `VmConfig::new().with_classpath(vec![dir])` + `SharedVm::new(config)` /
//! `Vm::new(config)`, exactly as `vm/tests/clinit_order_tests.rs` and dozens
//! of other integration tests already do. Pointed at `test_classes/gpu/`
//! (the same fixtures `OffloadCache`'s own inline unit tests load raw
//! `.class` bytes from), this is enough to drive the real public offload
//! API — `offload::try_dispatch`, `offload::dispatch_method_from_native` —
//! against real compiled kernels on a CUDA box, and to exercise every
//! stub-mode (no-device) contract on this dev box.
//!
//! # File-level gating
//!
//! Everything here is gated behind the `gpu-offload` Cargo feature, same as
//! every other GPU-offload source file and `gpu_async_stub.rs`. Without the
//! feature this file compiles to an empty translation unit.
//!
//! # Stub-mode vs. device tests
//!
//! Tests that need only a `SharedVm` (no CUDA driver) run unconditionally.
//! Tests that need a compiled kernel and a real launch are `#[ignore]`d
//! with `"requires NVIDIA GPU"`; run them explicitly on the RTX 2060 box:
//!
//! ```text
//! bash test_classes/gpu/build-fixtures.sh          <- REQUIRED FIRST
//! cargo test -p cratonvm-vm --features gpu-offload -- --ignored --test-threads=1
//! ```
//!
//! The first line is not optional and used to be missing from these docs.
//! These tests load their kernels through a real classpath rooted at
//! `test_classes/gpu/`, and `.gitignore` keeps `test_classes/**/*.class`
//! out of the repository — the `.java` is the source of truth and the
//! `.class` is a build artefact. Skip it and all four fail with
//! `ClassNotFound`, which reads like a broken class loader.
//! (plus whatever additionally activates `cuda-bridge`'s real `cuda`
//! backend on that host — see `cratonvm-embed/Cargo.toml`'s `gpu-driver`
//! feature for the alias other crates in this workspace use).
//!
//! **`--test-threads=1` is required, not optional, for the `--ignored`
//! run.** Each `#[ignore]`d test below constructs its own `Vm::new()`
//! (and thus its own `DeviceContext`), but `cudarc::CudaDevice::new(0)`
//! resolves to the SAME reference-counted CUDA primary context for
//! device 0 across all of them within one process. Running these tests
//! in parallel (`cargo test`'s default) lets one test's `Vm` teardown
//! release/invalidate that shared primary context while another test's
//! still-in-flight async submission (see
//! `device_submission_completes_spontaneously_without_any_poll_call`)
//! is finalizing on a background thread — observed on real hardware as
//! a `cudarc::driver::safe::core::CudaStream::drop` panic
//! (`CUDA_ERROR_NOT_PERMITTED`) on an unrelated, unnamed thread. It
//! does not fail the test it happens to interrupt (the panic is on a
//! detached thread, not the test's own), and it reproduces ONLY under
//! parallel execution — confirmed absent both running each test alone
//! and running all four with `--test-threads=1`. This is a shared
//! multi-`Vm`-per-process test-harness hazard, not a defect in the
//! completion-reaper logic itself (2026-07-12); a real, single-VM
//! production process never constructs more than one `DeviceContext`
//! for the same device concurrently.
//!
//! # Honesty note (per the task's instructions)
//!
//! `poll_submission_status` was still being written by a concurrent agent
//! in this same session at the time this file was authored — `grep -r
//! poll_submission_status vm/src` came back empty. Its test below is
//! written strictly to the documented contract ("unknown handle -> None")
//! and deliberately never names the `PollOutcome` type, so it depends on
//! nothing but the function's existence and its `Option`-shaped return —
//! the part of the signature the task pinned down with confidence.

#![cfg(feature = "gpu-offload")]

use std::sync::Arc;

use cratonvm_types::{ArrayElementType, ClassId, Value};
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::runtime::gpu_marshal::{
    host_view_i16, host_view_i8, write_back_i16, write_back_i8,
};
use cratonvm_vm::runtime::offload::{
    self, dispatch_method_from_native, finalize_submission, lookup_submission, release_submission,
    DispatchOutcome, LookupOutcome, OffloadCacheRegistry,
};
use cratonvm_vm::runtime::offload_jit_gate::{caller_blocks_jit, caller_blocks_jit_by_name};
use cratonvm_vm::vm::{ensure_class_initialized_shared, SharedVm, Vm};

// ── Shared test helpers ──────────────────────────────────────────────

/// `test_classes/gpu/` — the same fixture directory `OffloadCache`'s own
/// inline unit tests (`vm/src/runtime/offload.rs`) load raw `.class`
/// bytes from directly. Used here as a `VmConfig` classpath entry so the
/// real class-loading path (`SharedVm::load_class_concurrent`) can find
/// `EligibleVectorAdd`/`EligibleDotProduct` by simple name.
fn gpu_fixtures_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    std::path::Path::new(manifest_dir)
        .parent()
        .expect("workspace root is vm/..")
        .join("test_classes")
        .join("gpu")
        .to_string_lossy()
        .into_owned()
}

fn fixture_class_available(class_name: &str) -> bool {
    std::path::Path::new(&gpu_fixtures_dir())
        .join(format!("{class_name}.class"))
        .exists()
}

/// `--gpu`-equivalent config: offload enabled, classpath pointed at the
/// GPU fixtures directory. On this no-GPU dev box `gpu_offload_enabled =
/// true` still yields `ctx = None` (no CUDA driver), matching the exact
/// setup `OffloadCache`'s own `offload_cache_skips_when_no_device`-style
/// tests use.
fn gpu_config() -> VmConfig {
    let mut config = VmConfig::new().with_classpath(vec![gpu_fixtures_dir()]);
    config.gpu_offload_enabled = true;
    config
}

// ═════════════════════════════════════════════════════════════════════
// (a)/(b) DispatchOutcome variant contracts — pure host-side, no VM.
// ═════════════════════════════════════════════════════════════════════
//
// `try_dispatch`'s own doc comment is explicit that callers MUST treat
// `FallThroughKeepHooked` differently from `FallThrough` (the former
// forbids invoke-cache promotion of the call site; the latter says
// nothing about it) and that `HandledWithValue` carries a payload
// `Handled` does not. `DispatchOutcome` derives `PartialEq`, so these are
// exactly the contracts a caller's `match`/`==` logic depends on — pin
// them down independent of any device or VM state.

#[test]
fn dispatch_outcome_fallthrough_and_fallthrough_keep_hooked_are_distinct() {
    // The invoke-cache-suppression contract (item b) hinges entirely on
    // these two variants comparing unequal: an interpreter call site
    // that only ever sees `FallThrough` is free to promote into the
    // invoke cache, but one that sees `FallThroughKeepHooked` must not.
    // If a future refactor accidentally merged these into one variant
    // (or `PartialEq` started treating them as equal), that
    // distinction would silently vanish.
    assert_ne!(
        DispatchOutcome::FallThrough,
        DispatchOutcome::FallThroughKeepHooked
    );
}

#[test]
fn dispatch_outcome_handled_and_handled_with_value_are_distinct() {
    assert_ne!(
        DispatchOutcome::Handled,
        DispatchOutcome::HandledWithValue(Value::Int(0))
    );
}

#[test]
fn dispatch_outcome_handled_with_value_distinguishes_int_and_long_zero() {
    // Part E gates reductions to `)I`/`)J` only (never `)F`/`)D` — see
    // the module doc comment on `try_dispatch`). The interpreter's
    // return-value push path (`coerce_value_for_return` /
    // `push_invoke_return_value`) is tag-exact, so an `Int(0)` payload
    // must never compare equal to a `Long(0)` payload even though the
    // numeric value coincides — a mixup here would silently corrupt the
    // operand stack's category-1/category-2 slot accounting for a
    // reduction that happens to sum to zero.
    assert_ne!(
        DispatchOutcome::HandledWithValue(Value::Int(0)),
        DispatchOutcome::HandledWithValue(Value::Long(0))
    );
}

#[test]
fn dispatch_outcome_handled_with_value_equality_is_by_payload() {
    assert_eq!(
        DispatchOutcome::HandledWithValue(Value::Long(42)),
        DispatchOutcome::HandledWithValue(Value::Long(42))
    );
    assert_ne!(
        DispatchOutcome::HandledWithValue(Value::Long(42)),
        DispatchOutcome::HandledWithValue(Value::Long(43))
    );
}

// ═════════════════════════════════════════════════════════════════════
// (c) JIT admission gate — public-API behavior through-through a real
// SharedVm, per the task's explicit ask: "gate returns false when
// gpu_offload_enabled=false regardless of bytecode."
// ═════════════════════════════════════════════════════════════════════
//
// `offload_jit_gate.rs`'s own inline unit tests deliberately stop at the
// pure bytecode scanner / constant-pool resolver (its comment: exercising
// `caller_blocks_jit`/`compute` needs "a real SharedVm with a populated
// ClassManager", which it calls out-of-file-scope). That's exactly what
// we add here.

#[test]
fn caller_blocks_jit_false_when_offload_disabled_regardless_of_class() {
    // `gpu_offload_enabled` defaults to false. The gate must short-circuit
    // on that flag alone, before ever touching the class manager -- so an
    // unresolvable/never-loaded `ClassId` must not matter at all.
    let config = VmConfig::new();
    let shared = Arc::new(SharedVm::new(config));
    let bogus_class_id = ClassId::new(0xFFFF_FFFE);

    assert!(!caller_blocks_jit(&shared, bogus_class_id, 0));
    assert!(!caller_blocks_jit_by_name(
        &shared,
        bogus_class_id,
        "whatever",
        "()V"
    ));
}

#[test]
fn caller_blocks_jit_false_without_device_even_when_offload_enabled() {
    // `gpu_offload_enabled = true` but this dev box has no CUDA driver:
    // `compute`'s `registry.has_device()` short-circuit must return false
    // before ever scanning bytecode -- true for ANY class_id, loaded or
    // not, exactly the "regardless of bytecode" framing from the task.
    let mut config = VmConfig::new();
    config.gpu_offload_enabled = true;
    let shared = Arc::new(SharedVm::new(config));
    let bogus_class_id = ClassId::new(0xFFFF_FFFD);

    assert!(!caller_blocks_jit(&shared, bogus_class_id, 0));
    assert!(!caller_blocks_jit_by_name(
        &shared,
        bogus_class_id,
        "whatever",
        "()V"
    ));
}

#[test]
fn caller_blocks_jit_by_name_fails_open_on_unresolvable_method() {
    // Load a real class but ask about a method name that does not exist
    // on it. `caller_blocks_jit_by_name`'s doc comment states this must
    // fail open (return false, never panic/deny) -- an admission gate
    // must not deny compilation of methods it couldn't even identify.
    let config = gpu_config();
    let shared = Arc::new(SharedVm::new(config));
    if !fixture_class_available("EligibleVectorAdd") {
        eprintln!("skipping: EligibleVectorAdd.class fixture not found");
        return;
    }
    let class_id = shared
        .load_class_concurrent("EligibleVectorAdd")
        .unwrap_or_else(|e| panic!(
            "EligibleVectorAdd did not load from the fixtures classpath ({e:?}). \
             `.gitignore` keeps test_classes/**/*.class out of the repo, so a fresh \
             checkout has none: run `bash test_classes/gpu/build-fixtures.sh` first."
        ));

    assert!(!caller_blocks_jit_by_name(
        &shared,
        class_id,
        "thisMethodDoesNotExist",
        "()V"
    ));
}

// ═════════════════════════════════════════════════════════════════════
// OffloadCacheRegistry — Skip behavior without a device, exercised
// through the registry wrapper a real SharedVm uses
// (`shared.offload_registry.get_or_create(...)`), not by constructing
// `OffloadCache::new` directly the way offload.rs's own inline tests do.
// ═════════════════════════════════════════════════════════════════════

#[test]
fn offload_cache_registry_reuses_same_arc_per_ordinal() {
    let config = gpu_config();
    let registry = OffloadCacheRegistry::new();
    assert!(
        registry.get(0).is_none(),
        "no cache constructed for ordinal 0 yet"
    );

    let c1 = registry.get_or_create(0, &config);
    let c2 = registry.get_or_create(0, &config);
    assert!(
        Arc::ptr_eq(&c1, &c2),
        "repeated get_or_create for the same ordinal must reuse the cached Arc"
    );
    assert!(!c1.has_device(), "no CUDA driver on this box");

    let fetched = registry
        .get(0)
        .expect("get() must now find the cache created above");
    assert!(Arc::ptr_eq(&c1, &fetched));
}

#[test]
fn offload_cache_registry_different_ordinals_are_independent() {
    let config = gpu_config();
    let registry = OffloadCacheRegistry::new();
    let c0 = registry.get_or_create(0, &config);
    let c1 = registry.get_or_create(1, &config);
    assert!(
        !Arc::ptr_eq(&c0, &c1),
        "different device ordinals must not share a cache"
    );
}

#[test]
fn offload_cache_registry_skip_for_eligible_method_without_device() {
    // Same "no device -> Skip, never Blacklisted" contract as
    // `OffloadCache`'s own inline unit tests
    // (`offload_cache_skips_eligible_method_without_device`), but
    // reached the way real production code reaches it: through a live
    // `SharedVm`'s `offload_registry` field, with the method resolved
    // from a class the normal class-loading path loaded (not the
    // `read_class`-on-raw-bytes shortcut `offload.rs`'s own tests use).
    if !fixture_class_available("EligibleVectorAdd") {
        eprintln!("skipping: EligibleVectorAdd.class fixture not found");
        return;
    }
    let config = gpu_config();
    let shared = Arc::new(SharedVm::new(config));
    let cache = shared
        .offload_registry
        .get_or_create(shared.config.gpu_device_ordinal, &shared.config);
    assert!(!cache.has_device());

    let class_id = shared
        .load_class_concurrent("EligibleVectorAdd")
        .unwrap_or_else(|e| panic!(
            "EligibleVectorAdd did not load from the fixtures classpath ({e:?}). \
             `.gitignore` keeps test_classes/**/*.class out of the repo, so a fresh \
             checkout has none: run `bash test_classes/gpu/build-fixtures.sh` first."
        ));

    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(class_id).expect("class must be resolvable");
    let method_index = class
        .methods
        .iter()
        .position(|m| &*m.name == "vectorAdd" && &*m.descriptor == "([I[I[I)V")
        .expect("vectorAdd must be present on EligibleVectorAdd") as u16;

    match cache.lookup_or_compile(
        class_id,
        "EligibleVectorAdd",
        method_index,
        &class.methods[method_index as usize],
        &class.constant_pool,
    ) {
        LookupOutcome::Skip => {}
        LookupOutcome::Blacklisted => panic!("expected Skip on no-device path, got Blacklisted"),
        LookupOutcome::Hit(_) => {
            panic!("expected Skip on no-device path, got Hit (machine has a GPU?)")
        }
    }
}

// ═════════════════════════════════════════════════════════════════════
// Async submission registry lifecycle + (e) poll_submission_status.
//
// These exercise `dispatch_method_from_native`'s no-device failure path
// end to end through the fully public registry API
// (`register_submission`/`lookup_submission`/`release_submission`/
// `finalize_submission`) -- the exact scaffolding
// `gpu_async_stub.rs`'s `#[ignore]`d `submission_handle_unique` /
// `submit_returns_failed_future_no_device` PHASE3-GUESS comments
// envisioned, now implemented for real against the harness that turned
// out to already exist. `gpu_async_stub.rs` itself is left untouched per
// this task's scope.
// ═════════════════════════════════════════════════════════════════════

#[test]
fn dispatch_method_from_native_no_device_yields_failed_submission() {
    if !fixture_class_available("EligibleVectorAdd") {
        eprintln!("skipping: EligibleVectorAdd.class fixture not found");
        return;
    }
    let shared = Arc::new(SharedVm::new(gpu_config()));

    // `lookup_or_compile`'s no-device fast path returns `Skip` before
    // ever touching `java_args`, so an empty (arity-mismatched) args
    // slice is safe here -- the function returns from the Skip arm long
    // before the marshal loop would look at it.
    let handle =
        dispatch_method_from_native(&shared, "EligibleVectorAdd", "vectorAdd", "([I[I[I)V", &[]);
    assert!(
        handle > 0,
        "dispatch_method_from_native must hand back a handle even on failure"
    );

    let submission = lookup_submission(handle).expect("just-registered submission must be found");
    match finalize_submission(&shared, &submission) {
        Err(msg) => assert!(
            msg.contains("not offloadable") || msg.contains("Skip"),
            "expected a no-device Skip failure message, got: {msg}"
        ),
        Ok(()) => panic!("expected a Failed submission on this no-device box, got Ok(())"),
    }

    release_submission(handle);
    assert!(
        lookup_submission(handle).is_none(),
        "release_submission must make the handle unresolvable"
    );
}

#[test]
fn dispatch_method_from_native_handles_are_unique_per_call() {
    if !fixture_class_available("EligibleVectorAdd") {
        eprintln!("skipping: EligibleVectorAdd.class fixture not found");
        return;
    }
    let shared = Arc::new(SharedVm::new(gpu_config()));

    let h1 =
        dispatch_method_from_native(&shared, "EligibleVectorAdd", "vectorAdd", "([I[I[I)V", &[]);
    let h2 =
        dispatch_method_from_native(&shared, "EligibleVectorAdd", "vectorAdd", "([I[I[I)V", &[]);
    assert_ne!(h1, h2, "two submissions must never share a handle");
    assert!(lookup_submission(h1).is_some());
    assert!(lookup_submission(h2).is_some());

    release_submission(h1);
    release_submission(h2);
}

#[test]
fn release_submission_on_unknown_handle_is_a_safe_no_op() {
    // Documented as idempotent / safe-on-unknown per `release_submission`'s
    // own doc comment. Use a sentinel far outside any handle the
    // process-wide monotonic counter will realistically reach during a
    // test run.
    // Nothing is registered under the sentinel to begin with.
    assert!(
        lookup_submission(u64::MAX).is_none(),
        "precondition: the sentinel handle must not be registered"
    );
    release_submission(u64::MAX);
    release_submission(u64::MAX); // idempotent
                                  // The assertion this test was missing. Two bare calls could only fail by
                                  // panicking; "safe no-op" also means the release must not LEAVE anything
                                  // behind — a release path that inserted a tombstone, or that mutated the
                                  // table under an unknown key, would pass the old body and fail here.
    assert!(
        lookup_submission(u64::MAX).is_none(),
        "releasing an unknown handle must leave the submission table unchanged"
    );
}

#[test]
fn lookup_submission_unknown_handle_returns_none() {
    assert!(lookup_submission(u64::MAX).is_none());
}

/// (e) `poll_submission_status(shared, handle) -> Option<PollOutcome>`.
/// Written strictly to the one contract the task pinned down with
/// confidence -- see the "Honesty note" in the file doc comment above for
/// why this deliberately never names `PollOutcome`.
#[test]
fn poll_submission_status_unknown_handle_returns_none() {
    let shared = Arc::new(SharedVm::new(gpu_config()));
    let outcome = offload::poll_submission_status(&shared, u64::MAX);
    assert!(
        outcome.is_none(),
        "polling an unregistered submission handle must return None"
    );
}

// ═════════════════════════════════════════════════════════════════════
// (d) Bulk i8/i16 marshalling at SharedVm/heap altitude.
//
// `gpu_marshal.rs`'s own inline unit tests are already thorough (small,
// large-1024, odd-length bulk-copy, zero-length, and G1-humongous-scale
// round trips) but exclusively against a free-standing `VmHeap::new(...)`
// + a free-standing `SafepointToken` bound to a local `AtomicU32`. What's
// missing at integration altitude is the same round trip through a real
// `SharedVm`'s heap and its real `enter_gpu_critical()` (the
// process-wide `GPU_CRITICAL_COUNT` gate `dispatch_method_from_native`
// actually uses via `GcCriticalGuard`/`shared.mem.heap.enter_gpu_critical()`),
// rather than a synthetic per-test counter. Added here.
//
// NOTE on "via ResidencyTracker if applicable" (task item d): it is not
// applicable. `gpu_residency::PrimitiveType` has exactly four variants
// (I32, I64, F32, F64) -- there is no Short/Byte variant, so a
// `craton.gpu.GpuArray`/`ResidencyTracker` handle cannot carry a `short[]`
// or `byte[]` today. The i16/i8 marshalling added in
// `vm/src/runtime/gpu_marshal.rs` is reachable only through the direct
// array-arg path in `dispatch_method_from_native`'s marshal loop
// (`Value::Object` -> `array_element_type` -> `marshal_array_arg`), never
// through `Native.arrayWrapInt`-style residency wrapping. Flagging this
// as a real (if minor) API-surface gap rather than silently working
// around it.
// ═════════════════════════════════════════════════════════════════════

#[test]
fn host_view_i16_round_trips_through_shared_vm_heap() {
    let shared = Arc::new(SharedVm::new(VmConfig::new()));
    let token = shared.mem.heap.enter_gpu_critical();
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(1), ArrayElementType::Short, 6);

    let src: Vec<i16> = vec![0, -1, 12345, i16::MIN, i16::MAX, 7];
    write_back_i16(arr, &shared.mem.heap, &src, &token);
    let view = host_view_i16(arr, &shared.mem.heap, &token);
    assert_eq!(view, src);

    // Cross-check the tail element through the value-based accessor too
    // (mirrors gpu_marshal.rs's own bulk-path cross-check), so a
    // SharedVm-heap-specific aliasing bug wouldn't hide behind a
    // matching-length comparison alone.
    assert_eq!(
        shared
            .mem
            .heap
            .get_array_element(arr, src.len() - 1)
            .unwrap(),
        Value::Int(*src.last().unwrap() as i32)
    );
    drop(token);
}

#[test]
fn host_view_i8_round_trips_through_shared_vm_heap() {
    let shared = Arc::new(SharedVm::new(VmConfig::new()));
    let token = shared.mem.heap.enter_gpu_critical();
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(1), ArrayElementType::Byte, 5);

    let src: Vec<i8> = vec![0, -1, 1, i8::MIN, i8::MAX];
    write_back_i8(arr, &shared.mem.heap, &src, &token);
    let view = host_view_i8(arr, &shared.mem.heap, &token);
    assert_eq!(view, src);

    assert_eq!(
        shared
            .mem
            .heap
            .get_array_element(arr, src.len() - 1)
            .unwrap(),
        Value::Int(*src.last().unwrap() as i32)
    );
    drop(token);
}

#[test]
fn gpu_critical_count_reflects_live_tokens_on_shared_vm_heap() {
    // `enter_gpu_critical`'s doc comment: the counter is process-wide,
    // shared across every `VmHeap` in the process. Verify a token
    // acquired via a real `SharedVm`'s heap actually increments/decrements
    // it -- this is the exact gate `dispatch_method_from_native`'s
    // `GcCriticalGuard` and the GC's `wait_for_gpu_critical_drain` depend
    // on to avoid moving arrays out from under an in-flight kernel.
    let shared = Arc::new(SharedVm::new(VmConfig::new()));
    let before = shared.mem.heap.gpu_critical_count();
    {
        let _token = shared.mem.heap.enter_gpu_critical();
        assert_eq!(shared.mem.heap.gpu_critical_count(), before + 1);
    }
    assert_eq!(
        shared.mem.heap.gpu_critical_count(),
        before,
        "the token's Drop must release the process-wide critical count"
    );
}

// ═════════════════════════════════════════════════════════════════════
// Device tests — require a real CUDA-capable GPU (the RTX 2060 box).
// Ignored by default; run explicitly with `-- --ignored`.
// ═════════════════════════════════════════════════════════════════════

/// Dispatch `EligibleVectorAdd.vectorAdd` through the real transparent
/// offload entry point (`offload::try_dispatch` — the exact function
/// `execute_invokestatic`'s interpreter hook calls) and verify both the
/// `Handled` outcome and the device-computed output against a
/// host-computed reference. Array length (8192) comfortably clears the
/// default `--gpu-min-work` (4096) so the Hit arm's per-call gate does
/// not short-circuit into `FallThroughKeepHooked`.
#[test]
#[ignore = "requires NVIDIA GPU"]
fn device_vector_add_handled_with_correct_output() {
    let n = 8192usize;
    let config = gpu_config();
    let mut vm = Vm::new(config);

    let class_id = vm
        .shared
        .load_class_concurrent("EligibleVectorAdd")
        .unwrap_or_else(|e| panic!(
            "EligibleVectorAdd did not load from the fixtures classpath ({e:?}). \
             `.gitignore` keeps test_classes/**/*.class out of the repo, so a fresh \
             checkout has none: run `bash test_classes/gpu/build-fixtures.sh` first."
        ));
    ensure_class_initialized_shared(&vm.shared, &mut vm.main_thread, class_id)
        .expect("EligibleVectorAdd must initialize cleanly (no <clinit> to fail)");

    let a = vm
        .shared
        .mem
        .heap
        .alloc_array(ClassId::new(1), ArrayElementType::Int, n);
    let b = vm
        .shared
        .mem
        .heap
        .alloc_array(ClassId::new(1), ArrayElementType::Int, n);
    let out = vm
        .shared
        .mem
        .heap
        .alloc_array(ClassId::new(1), ArrayElementType::Int, n);
    let mut expected = vec![0i32; n];
    for i in 0..n {
        let av = i as i32;
        let bv = 2 * i as i32;
        vm.shared
            .mem
            .heap
            .set_array_element(a, i, Value::Int(av))
            .unwrap();
        vm.shared
            .mem
            .heap
            .set_array_element(b, i, Value::Int(bv))
            .unwrap();
        expected[i] = av.wrapping_add(bv);
    }

    let args = [
        Value::Object(Some(a)),
        Value::Object(Some(b)),
        Value::Object(Some(out)),
    ];
    let outcome = offload::try_dispatch(
        &vm.shared,
        &mut vm.main_thread,
        0,
        "EligibleVectorAdd",
        "vectorAdd",
        "([I[I[I)V",
        &args,
    )
    .unwrap_or_else(|e| panic!("try_dispatch returned an error: {e:?}"));

    assert_eq!(
        outcome,
        DispatchOutcome::Handled,
        "a void kernel above --gpu-min-work must return Handled"
    );

    for i in 0..n {
        assert_eq!(
            vm.shared.mem.heap.get_array_element(out, i).unwrap(),
            Value::Int(expected[i]),
            "out[{i}] mismatch"
        );
    }
}

/// Dispatch `EligibleDotProduct.dot` (a proven `)J` integer reduction —
/// see the class doc comment on `EligibleDotProduct.java`) through
/// `try_dispatch` and verify `HandledWithValue(Value::Long(..))` matches
/// a host-computed `long` reference, bit-exact (int/long atomic-add on
/// the device is order-independent — see `try_dispatch`'s doc comment on
/// why only `)I`/`)J` reductions are offered transparently).
#[test]
#[ignore = "requires NVIDIA GPU"]
fn device_dot_product_handled_with_value_matches_host_reference() {
    let n = 8192usize;
    let config = gpu_config();
    let mut vm = Vm::new(config);

    let class_id = vm
        .shared
        .load_class_concurrent("EligibleDotProduct")
        .unwrap_or_else(|e| panic!(
            "EligibleDotProduct did not load from the fixtures classpath ({e:?}). \
             `.gitignore` keeps test_classes/**/*.class out of the repo, so a fresh \
             checkout has none: run `bash test_classes/gpu/build-fixtures.sh` first."
        ));
    ensure_class_initialized_shared(&vm.shared, &mut vm.main_thread, class_id)
        .expect("EligibleDotProduct must initialize cleanly (no <clinit> to fail)");

    let a = vm
        .shared
        .mem
        .heap
        .alloc_array(ClassId::new(1), ArrayElementType::Int, n);
    let b = vm
        .shared
        .mem
        .heap
        .alloc_array(ClassId::new(1), ArrayElementType::Int, n);
    let mut expected: i64 = 0;
    for i in 0..n {
        let av = (i % 13) as i32 - 6;
        let bv = (i % 7) as i32 - 3;
        vm.shared
            .mem
            .heap
            .set_array_element(a, i, Value::Int(av))
            .unwrap();
        vm.shared
            .mem
            .heap
            .set_array_element(b, i, Value::Int(bv))
            .unwrap();
        expected += (av as i64) * (bv as i64);
    }

    let args = [Value::Object(Some(a)), Value::Object(Some(b))];
    let outcome = offload::try_dispatch(
        &vm.shared,
        &mut vm.main_thread,
        0,
        "EligibleDotProduct",
        "dot",
        "([I[I)J",
        &args,
    )
    .unwrap_or_else(|e| panic!("try_dispatch returned an error: {e:?}"));

    assert_eq!(
        outcome,
        DispatchOutcome::HandledWithValue(Value::Long(expected)),
        "proven )J reduction must return HandledWithValue with the exact host-computed sum"
    );
}

/// (b) Below `--gpu-min-work`: even a compiled, eligible kernel must NOT
/// launch. The Hit arm's per-call gate must return
/// `FallThroughKeepHooked` (never plain `FallThrough` — the interpreter's
/// invoke-cache-suppression contract depends on the distinction, see the
/// host-side `dispatch_outcome_fallthrough_and_fallthrough_keep_hooked_are_distinct`
/// test above) so the call site is never promoted into the invoke cache
/// and a later, larger call at the same site can still offload.
#[test]
#[ignore = "requires NVIDIA GPU"]
fn device_vector_add_below_min_work_keeps_call_site_hooked() {
    let config = gpu_config(); // default --gpu-min-work = 4096
    let mut vm = Vm::new(config);
    let n = 16usize; // well below the default gate

    let class_id = vm
        .shared
        .load_class_concurrent("EligibleVectorAdd")
        .unwrap_or_else(|e| panic!(
            "EligibleVectorAdd did not load from the fixtures classpath ({e:?}). \
             `.gitignore` keeps test_classes/**/*.class out of the repo, so a fresh \
             checkout has none: run `bash test_classes/gpu/build-fixtures.sh` first."
        ));
    ensure_class_initialized_shared(&vm.shared, &mut vm.main_thread, class_id)
        .expect("EligibleVectorAdd must initialize cleanly");

    let a = vm
        .shared
        .mem
        .heap
        .alloc_array(ClassId::new(1), ArrayElementType::Int, n);
    let b = vm
        .shared
        .mem
        .heap
        .alloc_array(ClassId::new(1), ArrayElementType::Int, n);
    let out = vm
        .shared
        .mem
        .heap
        .alloc_array(ClassId::new(1), ArrayElementType::Int, n);
    for i in 0..n {
        vm.shared
            .mem
            .heap
            .set_array_element(a, i, Value::Int(i as i32))
            .unwrap();
        vm.shared
            .mem
            .heap
            .set_array_element(b, i, Value::Int(1))
            .unwrap();
    }

    let args = [
        Value::Object(Some(a)),
        Value::Object(Some(b)),
        Value::Object(Some(out)),
    ];
    let outcome = offload::try_dispatch(
        &vm.shared,
        &mut vm.main_thread,
        0,
        "EligibleVectorAdd",
        "vectorAdd",
        "([I[I[I)V",
        &args,
    )
    .unwrap_or_else(|e| panic!("try_dispatch returned an error: {e:?}"));

    assert_eq!(outcome, DispatchOutcome::FallThroughKeepHooked);
}

/// Known-issues followups item 3 (2026-07-12) — hardware validation of
/// the completion reaper. Unlike every other test in this file,
/// dispatches through `dispatch_method_from_native` directly (the same
/// entry point `Native.submitMethod` uses) rather than `try_dispatch`,
/// because `try_dispatch`'s transparent path finalizes synchronously
/// before returning `Handled` — it would trivially "pass" this test
/// regardless of whether the reaper does anything, since the calling
/// thread itself would be the one finalizing.
///
/// After dispatch, this makes **zero** calls to `poll_submission_status`
/// / `finalize_submission` / anything that could itself drive
/// completion — it only sleeps, then reads `StreamSubmission::status`
/// directly (bypassing the registry's public accessors entirely) to
/// observe whatever state the completion reaper left it in. `Completed`
/// there can only mean the reaper's host callback woke
/// `completion_reaper_loop`, which called `finalize_submission` on its
/// own thread — nothing else in this test's call graph could have done
/// it.
#[test]
#[ignore = "requires NVIDIA GPU"]
fn device_submission_completes_spontaneously_without_any_poll_call() {
    use cratonvm_vm::runtime::offload::SubmissionStatus;

    let n = 1 << 22; // comfortably above --gpu-min-work, real device time
    let config = gpu_config();
    let mut vm = Vm::new(config);

    let class_id = vm
        .shared
        .load_class_concurrent("EligibleVectorAdd")
        .unwrap_or_else(|e| panic!(
            "EligibleVectorAdd did not load from the fixtures classpath ({e:?}). \
             `.gitignore` keeps test_classes/**/*.class out of the repo, so a fresh \
             checkout has none: run `bash test_classes/gpu/build-fixtures.sh` first."
        ));
    ensure_class_initialized_shared(&vm.shared, &mut vm.main_thread, class_id)
        .expect("EligibleVectorAdd must initialize cleanly");

    let a = vm
        .shared
        .mem
        .heap
        .alloc_array(ClassId::new(1), ArrayElementType::Int, n);
    let b = vm
        .shared
        .mem
        .heap
        .alloc_array(ClassId::new(1), ArrayElementType::Int, n);
    let out = vm
        .shared
        .mem
        .heap
        .alloc_array(ClassId::new(1), ArrayElementType::Int, n);
    let mut expected = vec![0i32; n];
    for i in 0..n {
        let av = i as i32;
        let bv = 2 * i as i32;
        vm.shared
            .mem
            .heap
            .set_array_element(a, i, Value::Int(av))
            .unwrap();
        vm.shared
            .mem
            .heap
            .set_array_element(b, i, Value::Int(bv))
            .unwrap();
        expected[i] = av.wrapping_add(bv);
    }

    // Warmup through the same entry point, fully finalized, so the
    // timed dispatch below hits an already-compiled kernel.
    let warm_args = [
        Value::Object(Some(a)),
        Value::Object(Some(b)),
        Value::Object(Some(out)),
    ];
    let warm_handle = dispatch_method_from_native(
        &vm.shared,
        "EligibleVectorAdd",
        "vectorAdd",
        "([I[I[I)V",
        &warm_args,
    );
    let warm_sub = lookup_submission(warm_handle).expect("warmup submission must be registered");
    finalize_submission(&vm.shared, &warm_sub).expect("warmup finalize must succeed");
    release_submission(warm_handle);

    let handle = dispatch_method_from_native(
        &vm.shared,
        "EligibleVectorAdd",
        "vectorAdd",
        "([I[I[I)V",
        &warm_args,
    );
    let submission = lookup_submission(handle).expect("submission must be registered");

    // NO poll_submission_status / finalize_submission / futureIsDone
    // equivalent call between here and the status read below — only a
    // sleep. Give the device comfortably more time than an N=2^22
    // vectorAdd needs (single-digit ms per the item-2 hardware
    // validation numbers in the known-issues doc).
    std::thread::sleep(std::time::Duration::from_millis(500));

    let status_after_sleep = match &*submission.status.lock() {
        SubmissionStatus::Running => "Running",
        SubmissionStatus::Completed { .. } => "Completed",
        SubmissionStatus::Failed { message, kind } => {
            panic!("submission failed without any poll call: {kind:?}: {message}")
        }
    };
    assert_eq!(
        status_after_sleep, "Completed",
        "completion reaper did not finalize the submission spontaneously within 500ms; \
         status is still {status_after_sleep} despite zero poll/get calls",
    );

    for i in 0..n {
        assert_eq!(
            vm.shared.mem.heap.get_array_element(out, i).unwrap(),
            Value::Int(expected[i]),
            "out[{i}] mismatch — reaper-driven finalize must drain writebacks correctly, not just flip status",
        );
    }

    release_submission(handle);
}

// ═════════════════════════════════════════════════════════════════════
// Known gaps this file does NOT cover (documented per the task's
// "be honest rather than fabricating" instruction):
//
// - Float/double reduction fall-through ("integer-only gating", item a):
//   `try_dispatch`'s Hit arm only special-cases `)I`/`)J`; a `)F`/`)D`
//   scalar-return kernel falls through to `FallThrough` unconditionally.
//   Exercising this needs a compiled, analyzer-proven float/double
//   reduction fixture; none exists under `test_classes/gpu/` today (the
//   closest, `EligibleLdcFloat`/`EligibleLdcDouble`, are ldc-constant
//   fixtures, not reductions). Adding one is a `javac`/fixture-authoring
//   task outside this file's "no builds" remit.
// - `caller_blocks_jit`'s positive case (a caller whose bytecode actually
//   contains an `invokestatic` to an eligible target, on a REAL device)
//   is only reachable with a compiled kernel — i.e. it needs the same
//   GPU box as the device tests above, plus a caller fixture (e.g.
//   `Benchmark.class`, which does contain such an invokestatic to
//   `EligibleVectorAdd.vectorAdd`) and a way to resolve its
//   `(ClassId, method_index)` from Rust. Not attempted here to keep this
//   file's device section anchored to the two dispatch entry points the
//   task named explicitly.
// - The interpreter-side half of "Handled sites never promoted" (item b)
//   -- i.e. that `execute_invokestatic`'s own invoke-cache actually
//   respects `FallThroughKeepHooked`/never promotes a `Handled` site --
//   lives entirely inside `interpreter.rs`'s dispatch loop, which this
//   task explicitly puts off-limits (owned by a different concurrently-
//   editing agent). Only the `DispatchOutcome`-level contract those call
//   sites depend on is tested here.
// ═════════════════════════════════════════════════════════════════════
