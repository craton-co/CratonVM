// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Which `invokestatic` targets are GPU kernels the dispatcher can launch.
//!
//! # Why the compiler needs to know
//!
//! GPU offload is transparent: the interpreter's `execute_invokestatic` slow
//! path consults `runtime::offload::try_dispatch`, and a call whose target is
//! an offload-eligible kernel with large enough arrays runs on the device
//! instead of the CPU. Compiled code has no such hook, so a JIT-compiled
//! caller silently stops offloading — permanently, for the life of the
//! process.
//!
//! What stood in for a fix was `runtime::offload_jit_gate` refusing to compile
//! the caller at all. Measured on `GpuHookOverheadBench` under `--gpu`, one
//! binary, the refusal switched off as the control arm:
//!
//! | arm | `base_ns_per_call` |
//! |---|---:|
//! | `--gpu`, caller refused (the old default) | 407.9 |
//! | `--gpu`, refusal off | 10.2 |
//! | no `--gpu` | 9.0 |
//!
//! `base_ns_per_call` is a loop calling an **ineligible** target, so the hook
//! itself is not in it. The 40x is purely the enclosing method being denied
//! compilation, and it is charged to every line of that method — not just the
//! kernel call it was refused for.
//!
//! # What this registry is
//!
//! `offload_jit_gate` already resolves every `invokestatic` in a method it
//! judges and decides whether the target is a kernel the DISPATCHER can
//! actually launch (`target_can_ever_dispatch`). It now records those targets
//! here instead of refusing the caller. Two consumers read it:
//!
//! * **`lib.rs`'s direct-call planning** skips such a site, so it keeps
//!   lowering through `jit_invoke_dispatch` rather than a raw `CALL` to the
//!   callee's entry. That is the one door in this backend that would bypass
//!   the helper for a statically-bound call; single-pass statics already go
//!   through it, and its inline caches are virtual-only.
//! * **`jit_invoke_dispatch`** tries the offload hook before dispatching.
//!
//! # Unarmed is free, and armed is the only way to be wrong
//!
//! [`is_kernel`] reads one relaxed `bool` and returns `false` when the run is
//! not doing GPU offload at all, which is every run without `--gpu`. Armed, it
//! costs a read lock and a hash on a path that is already several probes deep.
//!
//! Missing a door is not a correctness bug: the site simply does not offload,
//! which is exactly what a compiled caller did before any of this existed. The
//! failure mode is a lost optimisation, and
//! `gpu_compiled_offload_census` is what makes it visible rather than silent.
//!
//! # AUDIT 2026-09-06: armed means "offload is live", not "something was
//! registered"
//!
//! [`any_kernels`] used to be true only once `note_kernel` had inserted a row,
//! and `jit_invoke_dispatch` reads it to decide whether to consult the hook at
//! all. That made the registry unable to learn: a program whose ONLY kernel is
//! forward-referenced (its class not yet loaded when its caller was scanned)
//! registered nothing, so the flag stayed false, so the compiled dispatch
//! helper never looked, so nothing ever registered it. The empty case was
//! self-sealing.
//!
//! [`arm`] now also sets it from `runtime::offload_jit_gate` the first time
//! that gate runs with a real device, so a `--gpu` run is armed whether or not
//! a caller scan happened to find a kernel — and the per-site resolution in
//! `try_compiled_offload` gets a chance to ask the gate about a target the scan
//! could not judge. A run WITHOUT `--gpu` never reaches `arm`, and still pays
//! exactly one relaxed bool per compiled static dispatch.
//!
//! See `internal/gpu/compiled-caller-gate-refused-ldc-kernels-FIXED-20260905.md`.

use parking_lot::RwLock;
use rustc_hash::FxHashSet;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::OnceLock;

/// Is GPU offload live in this process? Read on every compiled static
/// dispatch, so it is a plain relaxed bool rather than a lock.
///
/// Set by [`note_kernel`] and by [`arm`] — see the AUDIT note in the module
/// docs for why registration alone is not enough to arm.
static ARMED: AtomicBool = AtomicBool::new(false);

/// `--gpu-min-work`, published by [`arm`] so [`could_ever_dispatch`] can
/// mirror the runtime gate exactly rather than guessing at its threshold.
///
/// `u32::MAX` is the "not armed yet" sentinel and never a real setting.
static MIN_WORK: AtomicU32 = AtomicU32::new(u32::MAX);

type Key = (Box<str>, Box<str>, Box<str>);

fn table() -> &'static RwLock<FxHashSet<Key>> {
    static T: OnceLock<RwLock<FxHashSet<Key>>> = OnceLock::new();
    T.get_or_init(|| RwLock::new(FxHashSet::default()))
}

/// Record that `class.method descriptor` is a kernel the dispatcher can
/// launch, so a compiled call site targeting it keeps its hook.
///
/// Called from `offload_jit_gate`'s per-method scan, which is memoized per
/// `(vm, class, method index)`, so this runs once per caller method rather
/// than once per call.
pub fn note_kernel(class_name: &str, method_name: &str, descriptor: &str) {
    let key: Key = (
        class_name.into(),
        method_name.into(),
        descriptor.into(),
    );
    let mut t = table().write();
    if t.insert(key) {
        // Published only after the entry is visible, so a reader that sees
        // `true` cannot then miss the row that set it.
        ARMED.store(true, Ordering::Release);
    }
}

/// Arm the hook because GPU offload is live, without registering anything.
///
/// Called once from `runtime::offload_jit_gate`'s scan, after it has confirmed
/// a usable device. Without this the registry cannot learn about a kernel no
/// caller scan was able to judge — see the AUDIT note in the module docs.
pub fn arm(gpu_min_work: u32) {
    // Published BEFORE the armed flag, so a reader that sees `true` cannot
    // then read the `u32::MAX` sentinel and mis-answer `could_ever_dispatch`.
    MIN_WORK.store(gpu_min_work, Ordering::Relaxed);
    // Relaxed-store-then-Release is not needed here: unlike `note_kernel`
    // there is no table row this publication has to order after.
    ARMED.store(true, Ordering::Release);
}

/// Is GPU offload live — i.e. is it worth asking about a call site at all?
///
/// One relaxed bool, and the only thing a run without `--gpu` pays per
/// compiled static dispatch. [`is_kernel`] allocates to build its key,
/// so nothing calls it on a per-call path -- see `CompiledOffloadSite`
/// in `vm/src/jit/helpers.rs` for the per-site memo that keeps it to
/// once.
#[inline]
pub fn any_kernels() -> bool {
    ARMED.load(Ordering::Acquire)
}

/// Is this call target a kernel the dispatcher can launch?
#[inline]
pub fn is_kernel(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    if !ARMED.load(Ordering::Acquire) {
        return false;
    }
    // The allocation is why this is not on a per-call path in the emitter:
    // it is consulted once per compiled dispatch, which under `--gpu` is
    // already a helper call several probes deep, and once per call site at
    // compile time.
    let key: Key = (
        class_name.into(),
        method_name.into(),
        descriptor.into(),
    );
    table().read().contains(&key)
}

/// Could a call to this descriptor EVER be offloaded, judged from the
/// descriptor alone?
///
/// The descriptor-only half of `offload_jit_gate::target_can_ever_dispatch`,
/// mirrored here because the two compile-time doors below need it and the
/// `jit` crate cannot see the `vm` crate. `try_dispatch` launches only a `)V`
/// map or a proven `)I`/`)J` reduction, and above `--gpu-min-work` 0 the work
/// estimate is structurally zero unless a parameter can be an array.
///
/// Deliberately does NOT ask whether the target is a reduction: that needs the
/// analyzer, which needs the class loaded, which is the very thing this
/// predicate exists to not depend on. So `)I`/`)J` are admitted on shape and
/// the analyzer sorts them out later, at a moment when it can.
fn could_ever_dispatch(descriptor: &str) -> bool {
    let shape_ok = descriptor.ends_with(")V")
        || descriptor.ends_with(")I")
        || descriptor.ends_with(")J");
    if !shape_ok {
        return false;
    }
    let min_work = MIN_WORK.load(Ordering::Relaxed);
    if min_work == 0 {
        return true;
    }
    // `ParamKind::from_field` admits only primitives and primitive arrays, so
    // "the parameter list contains `[`" is exactly "an argument can be an
    // array" — and without one, `largest_primitive_array_len` is 0 on every
    // call, for every argument value, so the threshold can never be cleared.
    let Some(open) = descriptor.find('(') else {
        return false;
    };
    let Some(close) = descriptor.find(')') else {
        return false;
    };
    close > open && descriptor[open + 1..close].contains('[')
}

/// Must a compiled call site to this target KEEP its dispatch helper?
///
/// This is the question the two irreversible compile-time doors in the backend
/// have to ask — `lib.rs`'s direct-call planning and `bytecode_walk`'s inliner
/// — and it is not the same question as [`is_kernel`].
///
/// # AUDIT 2026-09-07: a registry miss is not "not a kernel"
///
/// Both doors used to gate on [`is_kernel`] alone. A miss there means one of
/// two things — the target is genuinely not a kernel, or **the registry could
/// not have known yet** — and the doors treat the two identically while making
/// a decision that is one-way for the life of the process.
///
/// The second case is routine, not exotic. `offload_jit_gate` cannot judge a
/// target whose declaring class is not loaded when the caller is scanned, and
/// a caller whose hot loop runs BEFORE its first call to the kernel is
/// compiled at exactly that moment. `bench-gpu/GpuComputeWarm.java` is the
/// shape: `main` fills two 2^24 arrays (hot, so it is compiled), and only then
/// calls `GpuCompute.heavy` in another class. The site was bound directly, the
/// dispatch helper was gone, `try_compiled_offload`'s late registration never
/// got a site to run on, and the kernel never appeared in
/// `--print-gpu-decisions` at all — not rejected, never asked. It read 2,934 ms
/// on the CPU against 7 ms on the device, silently, with the census printing
/// zeroes.
///
/// So the miss now falls back to [`could_ever_dispatch`], which is decidable
/// from the descriptor with no class loading and no locks. The asymmetry is
/// the point: **keeping a helper is reversible and cheap, binding directly is
/// neither.** A site kept unnecessarily costs one dispatch-helper call that a
/// non-GPU run would not have paid; a site bound wrongly costs the device for
/// the rest of the run.
///
/// Narrow by construction. Unarmed — every run without `--gpu` — it is the
/// same single relaxed bool it always was. Armed, it additionally keeps the
/// helper only for `)V`/`)I`/`)J` targets that take an array parameter, which
/// is the shape a kernel has and almost nothing else does.
#[inline]
pub fn keeps_dispatch_helper(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    if !ARMED.load(Ordering::Acquire) {
        return false;
    }
    // Cheapest first: no allocation, no lock. Nearly every static call site in
    // a program fails this and stops here.
    if !could_ever_dispatch(descriptor) {
        return false;
    }
    // Known kernel, or a shape that could still turn out to be one. Both keep
    // the helper, so the registry lookup is only worth doing for the census —
    // and `is_kernel` allocates, so skip it entirely.
    let _ = (class_name, method_name);
    true
}

/// Test-only: forget every registration and disarm.
#[doc(hidden)]
pub fn reset_for_test() {
    table().write().clear();
    ARMED.store(false, Ordering::Release);
    MIN_WORK.store(u32::MAX, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// Every test here drives the same process-wide table and the same two
    /// atomics, and `reset_for_test` clears them — so under `cargo test`'s
    /// default thread pool they interleave and wipe each other's rows. Take
    /// this first in every test and they are serial regardless.
    ///
    /// Poison is not interesting here: a panicking test has already failed, and
    /// the next one resets whatever state it would have inherited.
    fn exclusive() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn unarmed_says_no_without_touching_the_table() {
        let _g = exclusive();
        reset_for_test();
        assert!(!is_kernel("A", "f", "([I)V"));
        // The compile-time door is the same single bool when offload is off,
        // which is what keeps a non-`--gpu` run paying nothing for any of this.
        assert!(!keeps_dispatch_helper("A", "f", "([I)V"));
    }

    #[test]
    fn a_registered_target_is_recognised_exactly() {
        let _g = exclusive();
        reset_for_test();
        note_kernel("com/example/K", "vectorAdd", "([I[I)V");
        assert!(is_kernel("com/example/K", "vectorAdd", "([I[I)V"));
        // All three components are part of the identity: an overload with a
        // different descriptor is a different method, and matching it would
        // keep a hook on a site that can never offload.
        assert!(!is_kernel("com/example/K", "vectorAdd", "([J[J)V"));
        assert!(!is_kernel("com/example/K", "other", "([I[I)V"));
        assert!(!is_kernel("com/other/K", "vectorAdd", "([I[I)V"));
        reset_for_test();
    }

    /// The regression this predicate exists for: `GpuComputeWarm`'s shape, where
    /// the caller is compiled before the callee's class is loaded, so the
    /// registry cannot yet know `GpuCompute.heavy` is a kernel. Gated on
    /// `is_kernel`, that site was bound directly and could never offload again.
    #[test]
    fn an_unregistered_kernel_shape_still_keeps_its_helper() {
        let _g = exclusive();
        reset_for_test();
        arm(1024);
        assert!(!is_kernel("bench/GpuCompute", "heavy", "([I[I[I)V"));
        assert!(keeps_dispatch_helper("bench/GpuCompute", "heavy", "([I[I[I)V"));
        reset_for_test();
    }

    #[test]
    fn armed_the_door_still_refuses_shapes_that_can_never_dispatch() {
        let _g = exclusive();
        reset_for_test();
        arm(1024);
        // `try_dispatch` launches only a `)V` map or a proven `)I`/`)J`
        // reduction.
        assert!(!keeps_dispatch_helper("A", "f", "([I)D"));
        assert!(!keeps_dispatch_helper("A", "f", "([I)Ljava/lang/Object;"));
        // No array parameter, so `largest_primitive_array_len` is 0 on every
        // call, for every argument value, and `--gpu-min-work` can never be
        // cleared.
        assert!(!keeps_dispatch_helper("A", "f", "(II)V"));
        assert!(!keeps_dispatch_helper("A", "f", "()V"));
        // The shapes that survive.
        assert!(keeps_dispatch_helper("A", "f", "([I[I)V"));
        assert!(keeps_dispatch_helper("A", "f", "([DI)I"));
        assert!(keeps_dispatch_helper("A", "f", "([J)J"));
        reset_for_test();
    }

    /// `--gpu-min-work 0` turns the work threshold off, so the array test that
    /// mirrors it has to come off too — otherwise this door would be stricter
    /// than the runtime gate it stands in for, and refuse a site the gate would
    /// have accepted.
    #[test]
    fn min_work_zero_drops_the_array_requirement() {
        let _g = exclusive();
        reset_for_test();
        arm(0);
        assert!(keeps_dispatch_helper("A", "f", "(II)V"));
        assert!(!keeps_dispatch_helper("A", "f", "(II)D"));
        reset_for_test();
    }
}
