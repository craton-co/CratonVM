// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT compilation state: code cache, PGO profiles, tiering policy, deopt log and invalidation.
//!
//! Extracted verbatim from the former monolithic `SharedVm` struct.
//! Field types, lock types and lock levels are unchanged; only the
//! owning struct differs. Access paths are `shared.jit.<field>`.

use crate::classloading::ClassId;
use crate::jit::profile::ProfileStore;
use crate::jit::JitCache;
use parking_lot::RwLock;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::{Arc, OnceLock, Weak};

/// JIT compilation state: code cache, PGO profiles, tiering policy, deopt log and invalidation.
pub struct JitRealm {
    /// JIT compiler cache — maps method identity to compiled native code.
    pub jit_cache: JitCache,

    /// PGO profile store — branch and receiver type counts collected during
    /// interpreted warmup, consumed by the JIT at compile time.
    pub profile_store: ProfileStore,

    /// Negative cache for JIT compilation: methods that failed `jit_scan` are
    /// recorded here so subsequent invocations skip the scan entirely.
    /// T10.9.B: FxHashSet — keys are internal (class, method, desc) triples
    /// from already-loaded class files.
    pub jit_skip_set: parking_lot::RwLock<FxHashSet<(Arc<str>, Arc<str>, Arc<str>)>>,

    /// The POSITIVE half of [`Self::jit_skip_set`]: methods whose static
    /// JIT-eligibility gate was evaluated and came back **eligible**.
    ///
    /// `jit_skip_set` memoizes only the methods that FAIL the gate. That fix
    /// (2026-07-15) left the mirror case open, and it is the expensive one:
    /// a method that PASSES is recorded nowhere, so `execute()`'s
    /// `already_skipped` short-circuit can never fire for it and every later
    /// entry re-runs the whole gate — including
    /// `jit_method_calls_native_shadowed`, an O(method-bytecode) decode that
    /// does a three-string-hash `slot_for_exact` probe per invoke instruction
    /// in the body. It runs *before* the `JitCache` consult further down, so
    /// even a fully compiled, hot method pays it on every `execute()` entry.
    ///
    /// Measured on netty `AdaptiveByteBufAllocatorTest` (826 M calls): that
    /// scan reached 2.15% of CPU through `slot_for_exact` alone, against only
    /// 637 methods ever sealed for the same reason — i.e. it was re-running,
    /// not running once per method.
    ///
    /// The value is `(redefine_epoch, is_interface_default)`. The epoch is the
    /// invalidation and it is load-bearing in the UNSAFE direction, unlike the
    /// negative set: a stale *seal* only costs throughput (the method stays
    /// interpreted), but a stale *pass* would let a redefined body — whose new
    /// bytecode may call a native-shadowed target — reach the compiler, which
    /// is exactly what the seal exists to prevent. `bump_redefine_epoch()` is
    /// already called on every `redefineClass`, beside the `clear_all()` that
    /// evicts the compiled artifacts, so an entry stamped with an older epoch
    /// is simply a miss and the gate re-runs.
    ///
    /// **Keyed on `ClassId`, NOT on the class name** — and that is the same
    /// asymmetry again, not a style choice. `jit_skip_set` keys on the name and
    /// says so safely: `is_jit_bail_listed`'s comment notes that a name-only
    /// collision between two same-named classes from different loaders is
    /// benign there, because the worst case is one class's compilable method
    /// being conservatively skipped. Reuse the name here and the worst case
    /// inverts — a `PASS` recorded for one loader's class would be redeemed by
    /// a *different* class of the same name whose own bytecode does call a
    /// native-shadowed target, compiling exactly what the seal exists to
    /// refuse. Two same-named classes in different loaders is the ByteBuddy /
    /// Mockito / servlet-container shape, not a hypothetical.
    pub jit_gate_pass: RwLock<FxHashMap<(ClassId, Arc<str>, Arc<str>), (u32, bool)>>,

    /// Tiered compilation manager — decides when and at which tier to compile.
    pub tiered_manager: crate::jit::tiered::TieredCompilationManager,

    /// Compilation policy — admission, tier selection, and the per-class
    /// invalidation epoch. See `docs/jit/compilation-broker.md` and
    /// `docs/jit/broker-install-epoch.md`.
    ///
    /// This field exists so the class epoch has PRODUCERS. The broker's own
    /// doc comment says a redefine "also bumps the class's epoch" — true of
    /// the broker and false of the process, because nothing held one. An epoch
    /// nobody bumps reads as protection and is not. The bumps are wired at the
    /// four events that falsify a queued request's assumptions: JVMTI
    /// redefine, class unload, `defineClass` over an already-loaded name, and
    /// JNI `DefineClass`.
    ///
    /// `Mutex` because the broker is `&mut self`-driven and carries no
    /// interior locking — deliberately, so the integration owns the
    /// concurrency decision rather than inheriting one.
    pub compilation_broker: parking_lot::Mutex<crate::jit::tiered::CompilationBroker>,

    /// Deoptimization log — records deopt events and drives adaptive recompilation.
    pub deopt_log: parking_lot::Mutex<crate::jit::deopt::DeoptimizationLog>,

    /// deopt-osr Step 9 — per-method *live* compilation epoch (a monotonic
    /// invalidation generation), keyed by the same `"<class>.<method>:<descriptor>"`
    /// string the deopt log uses. `DeoptimizationController::deoptimize` advances
    /// it on every invalidation; a freshly installed `CompiledMethod` is stamped
    /// (`CompiledMethod::compilation_epoch`) with the value live at install time.
    /// The real-frame-deopt resume sink refuses to resume a frame whose artifact
    /// epoch has fallen behind the live epoch — a compilation superseded since it
    /// started — so an invalidated speculation is never resumed; it falls back to
    /// the safe whole-method re-run. Empty + unread unless `deopt_real_enabled()`
    /// (the resume sink that consumes it is itself gated), so production VMs are
    /// unaffected.
    ///
    /// Step 9 follow-up (a): the value is a **boxed** `AtomicU64` rather than a
    /// bare `u64` so each method's epoch lives at a STABLE heap address (a
    /// `Box`'s payload does not move when the map rehashes, and entries are never
    /// removed). [`Self::live_epoch_cell_ptr`] hands that address to the
    /// `DeoptEpochGuard` baked into the method's frame-deopt stubs, so
    /// `x64_deopt_entry` can read the live epoch lock-free, BEFORE dereferencing
    /// the deopt box, to detect a superseded compilation.
    pub method_epochs: parking_lot::RwLock<FxHashMap<String, Box<std::sync::atomic::AtomicU64>>>,
    /// Shared fail-closed epoch for methods admitted after the bounded
    /// per-method epoch table reaches capacity.
    pub(crate) method_epoch_overflow: std::sync::atomic::AtomicU64,

    /// Per-class allocation-init cache for the JIT slow-path allocators
    /// (`jit_new_object` + the guarded TLAB-refill arm): primitive-field
    /// default-init recipe + `has_finalizer`, computed once per class and
    /// then read lock-free — replaces two `class_manager.read()` round trips
    /// per slow-path allocation. See `crate::jit::alloc_class_cache`.
    pub jit_alloc_class_cache: crate::jit::alloc_class_cache::JitAllocClassCache,
}
