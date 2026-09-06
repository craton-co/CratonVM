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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Is GPU offload live in this process? Read on every compiled static
/// dispatch, so it is a plain relaxed bool rather than a lock.
///
/// Set by [`note_kernel`] and by [`arm`] — see the AUDIT note in the module
/// docs for why registration alone is not enough to arm.
static ARMED: AtomicBool = AtomicBool::new(false);

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
pub fn arm() {
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

/// Test-only: forget every registration and disarm.
#[doc(hidden)]
pub fn reset_for_test() {
    table().write().clear();
    ARMED.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unarmed_says_no_without_touching_the_table() {
        reset_for_test();
        assert!(!is_kernel("A", "f", "([I)V"));
    }

    #[test]
    fn a_registered_target_is_recognised_exactly() {
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
}
