// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT compilation state: code cache, PGO profiles, tiering policy, deopt log and invalidation.
//!
//! Extracted verbatim from the former monolithic `SharedVm` struct.
//! Field types, lock types and lock levels are unchanged; only the
//! owning struct differs. Access paths are `shared.jit.<field>`.

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

    /// Tiered compilation manager — decides when and at which tier to compile.
    pub tiered_manager: crate::jit::tiered::TieredCompilationManager,

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

    /// Invalidation manager — tracks class-hierarchy assumptions and invalidates
    /// dependent compiled methods when class loading breaks those assumptions.
    pub invalidation_manager: parking_lot::Mutex<cratonvm_jit::deopt::InvalidationManager>,

    /// Per-class allocation-init cache for the JIT slow-path allocators
    /// (`jit_new_object` + the guarded TLAB-refill arm): primitive-field
    /// default-init recipe + `has_finalizer`, computed once per class and
    /// then read lock-free — replaces two `class_manager.read()` round trips
    /// per slow-path allocation. See `crate::jit::alloc_class_cache`.
    pub jit_alloc_class_cache: crate::jit::alloc_class_cache::JitAllocClassCache,
}
