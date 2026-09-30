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

thread_local! {
    /// The class an in-place redefinition on this thread is replacing, while
    /// `ClassManager::redefine_class` runs under `redefine_class_with`
    /// (`vm/src/vm/vm_exec.rs`). See [`InPlaceRedefinitionScope`].
    static IN_PLACE_REDEFINITION: std::cell::Cell<Option<ClassId>> =
        const { std::cell::Cell::new(None) };
}

/// Marks, on this thread, that `redefine_class_with` is replacing `class_id`
/// in place and will flush ITS VM's compiled code itself
/// ([`JitRealm::note_class_redefinition_of`], right after the class manager
/// returns, still under its writer).
///
/// `ClassManager::redefine_class` fires the JIT invalidate hook for the class
/// (step 8), whose adapter (`vm_init.rs::jit_invalidate_adapter`) had no VM
/// identity until wave 18 and so flushed EVERY live VM's code cache and
/// advanced every VM's code-state epoch — and then `redefine_class_with`
/// flushed its own VM a second time. (Since wave 18 the hook carries the
/// firing store's layout domain and the adapter reaches the owning VM only;
/// this scope still spares that VM the duplicate flush.) Interpreter round i1 wave 17: under this scope the adapter
/// leaves the flush to `redefine_class_with`. A redefinition keeps the field
/// layout (`redefine_class` refuses a field change), and another VM's class
/// with the same id is a different class, so neither flush was needed; the
/// double flush also left the redefinition census reading an already-empty
/// cache. The adapter still flushes for the hook's layout-changing callers
/// (`upgrade_synthetic_class`, `recompute_subclass_layouts`), and for a
/// redefinition of any other class this thread's hook may report.
pub(crate) struct InPlaceRedefinitionScope {
    prev: Option<ClassId>,
}

impl InPlaceRedefinitionScope {
    /// Enter the scope for `class_id`; the previous value is restored on drop.
    pub(crate) fn enter(class_id: ClassId) -> Self {
        let prev = IN_PLACE_REDEFINITION.with(|c| c.replace(Some(class_id)));
        Self { prev }
    }
}

impl Drop for InPlaceRedefinitionScope {
    fn drop(&mut self) {
        IN_PLACE_REDEFINITION.with(|c| c.set(self.prev));
    }
}

/// Whether this thread is inside an [`InPlaceRedefinitionScope`] for
/// `class_id`, i.e. whether the JIT invalidate hook firing for it may leave
/// the code-cache flush to the redefinition. One thread-local read.
pub(crate) fn in_place_redefinition_of(class_id: ClassId) -> bool {
    IN_PLACE_REDEFINITION.with(|c| c.get()) == Some(class_id)
}

/// Whether a successful define (or class load) that answered `class_id` must
/// still do what every define door used to do unconditionally: withdraw, BY
/// NAME, the compiled bodies that inlined a class of that name, and reset
/// that class's profile and tiering state (round 12 wave 5, lane tier3;
/// `r12w4-withdraw-define-withdraws-inliners-of-every-same-named-class`).
///
/// A define replaces a class a compiled body can depend on only when it
/// answers an id that was already in the store (a synthetic stub upgraded in
/// place, whose layout hook also flushes the whole cache), or under
/// `allow_redefine`, which re-keys the name in the SAME loader to a new id.
/// Every other define mints a fresh id (`ClassStore::next_id`; ids are never
/// reused), and a second define of a name in one loader is refused
/// (`DuplicateClassDefinition`). So such a define sits BESIDE a same-named
/// class of another loader and cannot replace it: the by-name withdrawal
/// only recompiled that other class's inliners, and the reset expired every
/// settled call site in the VM, for nothing.
///
/// `next_id_before` is `ClassStore::next_id()` read under the class-manager
/// writer that performed the define. `CRATONVM_JIT_DEFINE_SCOPED_WITHDRAWAL=0`
/// (default on) restores the by-name withdrawal for every define.
pub(crate) fn define_withdraws_by_name(
    class_id: ClassId,
    next_id_before: ClassId,
    allow_redefine: bool,
) -> bool {
    static SCOPED: OnceLock<bool> = OnceLock::new();
    let scoped = *SCOPED.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_DEFINE_SCOPED_WITHDRAWAL")
    });
    define_withdraws_by_name_when(class_id, next_id_before, allow_redefine, scoped)
}

/// [`define_withdraws_by_name`] with the switch passed in.
fn define_withdraws_by_name_when(
    class_id: ClassId,
    next_id_before: ClassId,
    allow_redefine: bool,
    scoped: bool,
) -> bool {
    !scoped || allow_redefine || class_id.as_u32() < next_id_before.as_u32()
}

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
    /// is exactly what the seal exists to prevent. `JitCache::bump_redefine_epoch` is
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

    /// Deoptimization log — records deopt events and drives adaptive recompilation.
    pub deopt_log: parking_lot::Mutex<crate::jit::deopt::DeoptimizationLog>,

    /// Per-bci de-speculation registry: the `(method_key, bci)` speculation
    /// sites THIS VM has given up on. Written by the real-frame-deopt resume
    /// sink (`deopt_resume.rs`), read by every compile this VM requests (passed
    /// as `Some(&shared.jit.despec_registry)`) and by
    /// `DeoptimizationLog::recommend_action_at_bci`.
    ///
    /// Was the process-global `DESPEC_SET` in `jit/src/deopt.rs` until
    /// 2026-09-12, so an embedded or second VM inherited despeculation verdicts
    /// it never earned. An `Arc` because the x64 `Compiler` holds it for the
    /// duration of a compile, which may run on a background compile worker.
    pub despec_registry: Arc<crate::jit::deopt::DespecRegistry>,

    /// Per-method runtime de-speculation verdicts: the methods THIS VM has
    /// given up compiling because their speculations keep failing at runtime
    /// (`DeoptAction::MakeNotCompilable`). Read by
    /// `cratonvm_jit::compile_gate::admit` at the method-entry and
    /// callee-dispatch doors and deliberately NOT at the OSR door — a loop in a
    /// method may be perfectly compilable while one call site elsewhere in it
    /// keeps trapping.
    ///
    /// Was the process-global `RUNTIME_DESPECULATED` map in
    /// `jit/src/compile_gate.rs` until `NOTES-deopt2.md` M7. Its
    /// `(ClassId, class, method, descriptor)` key keeps two LOADERS apart, which
    /// is what it was designed for, but not two VMs: `ClassId`s are allocated
    /// per `ClassStore`, from 0, so two VMs in one process routinely produce the
    /// same tuple for two different methods and one VM's give-up decision shut
    /// the other's compile doors. That is the class of bug `AGENTS.md` forbids
    /// process globals for per-VM state to prevent.
    ///
    /// An `Arc` for the same reason `despec_registry` is one: a compile may run
    /// on a background worker and holds this for its duration.
    pub runtime_despec: Arc<cratonvm_jit::compile_gate::RuntimeDespecRegistry>,

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

    /// The interpreter↔JIT bridge's memos (the callee negative cache, the
    /// optimizing-OSR artifact memos, the drain/sweep ticks). Were
    /// process-global statics in `interpreter/jit_bridge.rs`, keyed by
    /// per-VM `ClassId`s. See `JitBridgeState`.
    pub bridge: crate::runtime::interpreter::JitBridgeState,

    /// This VM's code-state epoch: advanced by every event of THIS VM that
    /// makes its compiled code or compile verdicts obsolete — a class
    /// redefinition ([`Self::note_class_redefinition_of`]) and a code-cache flush
    /// ([`Self::flush_code_cache`]) — and by nothing any other VM does.
    /// Starts at 1, so a `0` stamp is never current.
    ///
    /// Read by the tiered manager (OSR-denial and queued-request stamps; the
    /// same `Arc` is its install-epoch source), this VM's two verdict
    /// registries (the same `Arc` is their flush epoch, `cratonvm_jit::FlushEpoch`,
    /// since interpreter round i1 wave 15 — each kept a copy advanced beside
    /// this one until then), the cross-activation OSR
    /// refusal budget (`osr_loop_offer_allowed`) and the optimizing OSR memo's
    /// staleness check. Those read the process-wide
    /// `cratonvm_jit::jit_install_epoch()` until interpreter round i1 wave 14,
    /// so another VM's redefinition or flush expired this VM's state. The
    /// process counter still stamps compiled artifacts, where it orders a
    /// publication against a flush of the SAME cache (`JitCache::flush_barrier`).
    pub(crate) code_state_epoch: Arc<std::sync::atomic::AtomicU64>,

    /// This VM's redefine epoch: advanced once per class redefinition of THIS
    /// VM ([`Self::note_class_redefinition_of`]), after the code-cache flush.
    /// The same `Arc` is the redefine epoch of both verdict registries
    /// (`cratonvm_jit::RedefineEpoch::Shared`; interpreter round i1 wave 17 —
    /// each kept a copy that this function advanced through its own
    /// `note_redefinition` until then). `JitCache::redefine_epoch` is the
    /// inline-cache twin: moved in the same function, kept inside the cache
    /// because its readers are the inline-cache helpers, where one load beats
    /// a load through an `Arc`.
    pub(crate) redefine_epoch: Arc<std::sync::atomic::AtomicU32>,

    /// This VM's class store's layout domain (`ClassStore::layout_domain`),
    /// copied at construction so it is readable without the class-manager
    /// lock. The JIT invalidate hook carries the firing store's domain as an
    /// owner token, and `jit_invalidate_adapter` flushes only the VM whose
    /// domain matches — the hook fires under that VM's class-manager writer,
    /// so the adapter cannot read the domain there. Interpreter round i1
    /// wave 18 (`interpreter-layout-change-hook-flushes-every-live-vm-FIXED`).
    pub(crate) class_layout_domain: u32,

    /// This VM's C1→C2 supersede epoch: advanced by this VM's background
    /// compile worker after it publishes an optimizing body that replaced a
    /// published one ([`Self::bump_supersede_epoch`]). Every `Jit` invoke-cache
    /// entry holds it through a `SupersedeGate` snapshot ([`Self::supersede_gate`])
    /// and goes stale when it moves. A process static in `classloading` until
    /// interpreter round i1 wave 19, so one VM's C2 publish evicted every
    /// other VM's compiled call-site entries.
    ///
    /// Leaked, four bytes per VM ([`Self::new_supersede_epoch`]): the gate
    /// holds a `&'static` so cloning a `Jit` entry on an invoke-cache hit
    /// touches no shared refcount (see `SupersedeGate`).
    pub(crate) supersede_epoch: &'static std::sync::atomic::AtomicU32,
}

impl JitRealm {
    /// A fresh supersede epoch for [`Self::supersede_epoch`]. Leaked on
    /// purpose: see the field.
    pub(crate) fn new_supersede_epoch() -> &'static std::sync::atomic::AtomicU32 {
        Box::leak(Box::new(std::sync::atomic::AtomicU32::new(0)))
    }

    /// A fresh code-state epoch for [`Self::code_state_epoch`], to be shared
    /// with the tiered manager this realm is built with.
    pub(crate) fn new_code_state_epoch() -> Arc<std::sync::atomic::AtomicU64> {
        Arc::new(std::sync::atomic::AtomicU64::new(1))
    }

    /// A fresh redefine epoch for [`Self::redefine_epoch`], to be shared with
    /// the tiered manager's verdict registry and the runtime de-speculation
    /// registry this realm is built with.
    pub(crate) fn new_redefine_epoch() -> Arc<std::sync::atomic::AtomicU32> {
        // Once per process in effect (the first registration wins): compiled
        // code bakes the class-redefinition count it was compiled at next to
        // each `(holder, cp index)` helper argument, so a body still running
        // after its class was redefined can translate the index
        // (`cratonvm_jit::cp_holder_word`, `jit::helpers::cp_site_index`;
        // interpreter round i1 wave 20, lane L1). Registered while the VM is
        // built, before it compiles anything.
        cratonvm_jit::register_cp_stamp_source(crate::classloading::class_redefinition_count);
        Arc::new(std::sync::atomic::AtomicU32::new(0))
    }

    /// This VM's supersede epoch (one acquire load). See the field.
    #[inline]
    pub(crate) fn supersede_epoch(&self) -> u32 {
        self.supersede_epoch
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// A `Jit` invoke-cache entry's supersede gate on this VM's epoch. Take
    /// it BEFORE reading the artifact from the jit cache (see
    /// `SupersedeGate::snapshot`).
    #[inline]
    pub(crate) fn supersede_gate(&self) -> crate::classloading::resolution::SupersedeGate {
        crate::classloading::resolution::SupersedeGate::snapshot(self.supersede_epoch)
    }

    /// Record that this VM published an optimizing body that replaced a
    /// published one: every `Jit` invoke-cache entry of this VM re-resolves
    /// at its next hit, and no other VM's does.
    pub(crate) fn bump_supersede_epoch(&self) {
        self.supersede_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    /// This VM's code-state epoch (one acquire load). See the field.
    #[inline]
    pub(crate) fn code_state_epoch(&self) -> u64 {
        self.code_state_epoch
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Advance the code-state epoch AFTER the event it records, so anything
    /// stamped while the event ran reads as stale (one more compile or OSR
    /// attempt, never a wrong answer).
    fn advance_code_state_epoch(&self) {
        self.code_state_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    /// Everything a class redefinition of this VM invalidates on the JIT
    /// side, in order: every inline cache (`JitCache::bump_redefine_epoch`),
    /// every compiled body (`JitCache::clear_all`, which also arms the flush
    /// barrier), this VM's compile verdicts and runtime de-speculation
    /// verdicts (the redefine epoch, which both registries share), and the
    /// code-state epoch, which is also both registries' flush epoch. Returns
    /// the number of bodies evicted.
    ///
    /// The caller still owes the per-class resets (`on_class_redefined`,
    /// `ProfileStore::invalidate_class`); see `jit::redefinition_invalidation`.
    /// A caller that knows which class it redefined uses
    /// [`Self::note_class_redefinition_of`], which also feeds the census; the
    /// production redefinition path does (`redefine_class_with`), so this
    /// class-less form is the tests' spelling.
    #[cfg(test)]
    pub(crate) fn note_class_redefinition(&self) -> usize {
        self.redefine_and_flush(None, false)
    }

    /// `note_class_redefinition` for a redefinition of the class
    /// `(class_id, class_name)`, recording it in the redefinition census
    /// (`JitVerdictRegistry::note_redefinition_census`, printed on the
    /// `jit-method-stats` line): the bodies evicted and, when that line is
    /// armed, how many of them a class-scoped eviction would also have
    /// evicted (`JitCache::redefinition_dependents`). What
    /// `redefine_class_with` calls, under the class-manager writer that
    /// replaced the class.
    ///
    /// Interpreter round i1 wave 20, lane L1 (stage 2b of
    /// `i14-L3-proposal-class-scoped-redefinition-invalidation`): when
    /// [`Self::scoped_redefinition_admitted`] holds, only the bodies the
    /// redefinition can make stale are withdrawn
    /// (`JitCache::invalidate_for_redefinition`) and every other compiled
    /// body survives; otherwise the whole cache is flushed, as before.
    /// `has_registered_natives`: some method of the class has a registered
    /// native (see [`Self::scoped_redefinition_admitted`]).
    pub(crate) fn note_class_redefinition_of(
        &self,
        class_id: ClassId,
        class_name: &str,
        has_registered_natives: bool,
    ) -> usize {
        self.note_class_redefinition_renumbering(
            class_id,
            class_name,
            has_registered_natives,
            false,
        )
    }

    /// [`Self::note_class_redefinition_of`] for a redefinition that has
    /// replaced the class already and knows whether it `renumbered` the
    /// class's constant pool (`RedefinitionHistory::last_redefinition_moved_constants`):
    /// the OSR bodies of the class's own bytecode are then forced to leave
    /// rather than spared (`JitCache::force_withdrawn_exit_polls_after`;
    /// interpreter round i1 wave 45, lane L2). What `redefine_class_with`
    /// calls; the install fence's early withdrawal, which runs before any
    /// class is replaced, keeps the form above.
    pub(crate) fn note_class_redefinition_renumbering(
        &self,
        class_id: ClassId,
        class_name: &str,
        has_registered_natives: bool,
        renumbered: bool,
    ) -> usize {
        self.redefine_and_flush(
            Some((class_id, class_name, has_registered_natives)),
            renumbered,
        )
    }

    /// May a redefinition of `(class_id, class_name)` withdraw only its
    /// dependents rather than flush every body? Not when
    /// [`SCOPED_REDEFINITION_EVICTION_ENABLED`] is off, and not when some
    /// compiled code may depend on the class in a way no body records:
    ///
    /// * `java/lang/Object`: every constructor's elided `Object.<init>` call
    ///   (both tiers elide it by name) depends on it;
    /// * a class some compile copied bytecode of
    ///   (`JitCache::bytecode_was_copied`): the IR tier's splices, nested
    ///   single-pass splices and elided empty constructors leave no
    ///   `inlined_methods` entry, and an inherited method's body the by-name
    ///   callee door publishes under the RECEIVER's key names another class;
    /// * a class with a registered native (`has_registered_natives`): a
    ///   redefined class's bytecode wins over its registered natives
    ///   (`redefine_state::native_shadow_suppressed_in`), and a compiled
    ///   caller may have bound the native at compile time.
    ///
    /// Must be asked under the class-manager writer that replaced the class:
    /// a compile marks a class copied under the read guard it reads the
    /// bytecode under, so the mark is visible here or the copy read the new
    /// bytecode.
    pub(crate) fn scoped_redefinition_admitted(
        &self,
        class_id: ClassId,
        class_name: &str,
        has_registered_natives: bool,
    ) -> bool {
        SCOPED_REDEFINITION_EVICTION_ENABLED
            && class_name != "java/lang/Object"
            && !has_registered_natives
            && !self.jit_cache.bytecode_was_copied(class_id)
    }

    fn redefine_and_flush(&self, class: Option<(ClassId, &str, bool)>, renumbered: bool) -> usize {
        let stats = cratonvm_types::flags().jit.method_stats;
        let scoped = class.filter(|&(class_id, class_name, natives)| {
            self.scoped_redefinition_admitted(class_id, class_name, natives)
        });
        // Taken before either eviction empties the maps it reads, patched after
        // it: every body the redefinition makes stale by its own record,
        // published or only reachable through a baked caller, stops being
        // entrant, so a running compiled caller's next call into it runs the
        // new bytecode (interpreter round i1 wave 22, lane L6). Wave 23: EVERY
        // body when the redefinition flushes the whole cache (some compiled
        // code depends on the class without a record), and the optimizing OSR
        // memo's stale bodies, which the cache never holds, are marked
        // withdrawn so a frame running one leaves at its next back-edge poll.
        let whole_cache = scoped.is_none();
        // Wave 24 (lane L6): stop the old bytecode's in-flight compiles from
        // publishing BEFORE the scan, so none publishes between the scan and
        // the eviction, where it would be withdrawn but never patched.
        if let Some((class_id, class_name, _)) = class {
            if NOT_ENTRANT_ON_REDEFINITION_ENABLED {
                self.jit_cache
                    .fence_redefinition_publications(class_id, class_name, whole_cache);
            }
        }
        let stale = self.not_entrant_candidates(class, whole_cache);
        let stale_osr_memo = self.withdrawn_osr_memo_candidates(class, whole_cache);
        if let Some((class_id, class_name, _)) = scoped {
            // The epoch flushes each inline cache at its next helper visit;
            // `invalidate_for_redefinition` clears the slots and cells that
            // hold a stale body now (a filled slot never visits its helper,
            // wave 21). The bodies the class's redefinition cannot reach stay
            // published.
            self.jit_cache.bump_redefine_epoch();
            let (evicted, withdrawn) = self
                .jit_cache
                .invalidate_for_redefinition(class_id, class_name);
            self.demote_withdrawn(&withdrawn);
            self.redefine_epoch
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            // Still advanced: it drops queued compile requests (which may
            // carry the old bytecode) and expires the optimizing OSR memo,
            // whose bodies are never in the cache the scan reads. Scoping it
            // is stage 3 of the proposal.
            self.advance_code_state_epoch();
            // Scoped, the evicted bodies ARE the dependents.
            self.tiered_manager
                .verdicts()
                .note_redefinition_census(evicted, stats.then_some(evicted));
            self.jit_cache.make_not_entrant(&stale);
            self.jit_cache
                .mark_withdrawn_by_redefinition(&stale_osr_memo);
            self.force_withdrawn_exit_polls(class, &stale, &stale_osr_memo, renumbered);
            return evicted;
        }
        // Before the flush, which is what empties the cache the scan reads.
        // Only when the report that prints it is armed: it costs a scan.
        let dependent = class.filter(|_| stats).map(|(class_id, class_name, _)| {
            self.jit_cache.redefinition_dependents(class_id, class_name)
        });
        self.jit_cache.bump_redefine_epoch();
        let (evicted, withdrawn) = self.jit_cache.write().clear_all_collecting();
        self.demote_withdrawn(&withdrawn);
        // Both registries' redefine epoch; their shared flush epoch is the
        // code-state epoch advanced below. Neither registry's own
        // `note_redefinition` is called: on shared cells it moves nothing.
        self.redefine_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.advance_code_state_epoch();
        self.tiered_manager
            .verdicts()
            .note_redefinition_census(evicted, dependent);
        // Interpreter round i1 wave 24, lane L6: every body compiled before
        // this flush is withdrawn, including the ones no scan reaches -- an
        // optimizing OSR body the memo already forgot while a frame still runs
        // it, a body published between the candidate scan above and the flush.
        // The safepoint slow path asks `JitCache::body_withdrawn_by_redefinition`.
        if class.is_some() && NOT_ENTRANT_ON_REDEFINITION_ENABLED {
            self.jit_cache.note_whole_cache_redefinition();
        }
        self.jit_cache.make_not_entrant(&stale);
        self.jit_cache
            .mark_withdrawn_by_redefinition(&stale_osr_memo);
        self.force_withdrawn_exit_polls(class, &stale, &stale_osr_memo, renumbered);
        evicted
    }

    /// Force the exit polls of every body this redefinition withdrew
    /// (`JitCache::force_withdrawn_exit_polls`; interpreter round i1 wave 25,
    /// lane L6), after the not-entrant pass and the OSR memo's marks: a frame
    /// running one of them then leaves for the interpreter at its next
    /// exit-capable back edge whether or not it polls during the loop-exit
    /// handshake `redefine_class_with` takes -- a thread that was inside a call
    /// then (blocked, or this very thread, returning from
    /// `RetransformClasses`) leaves when the call returns
    /// (`docs/known-issues/interpreter/i24-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-20260927.md`).
    /// Nothing for a class-less flush, or with the not-entrant kill switch
    /// off.
    fn force_withdrawn_exit_polls(
        &self,
        class: Option<(ClassId, &str, bool)>,
        stale: &[Arc<cratonvm_jit::CompiledMethod>],
        stale_osr_memo: &[Arc<cratonvm_jit::CompiledMethod>],
        renumbered: bool,
    ) {
        let Some((class_id, class_name, _)) = class else {
            return;
        };
        if !NOT_ENTRANT_ON_REDEFINITION_ENABLED {
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] exit polls forced: bodies=0 (not-entrant kill switch off) \
                     redefined={class_name}"
                );
            }
            return;
        }
        // Wave 25 follow-up (lane L6b): an empty candidate list still reaches
        // the cache, which prints the pass's line either way.
        let bodies: Vec<Arc<cratonvm_jit::CompiledMethod>> =
            stale.iter().chain(stale_osr_memo).cloned().collect();
        self.jit_cache
            .force_withdrawn_exit_polls_after(&bodies, class_id, class_name, renumbered);
    }

    /// The bodies a redefinition of `class` makes stale by their own record
    /// (`JitCache::redefinition_stale_bodies`), or every body when it flushes
    /// the whole cache (`whole_cache`, wave 23), for
    /// `JitCache::make_not_entrant` once the eviction has withdrawn them.
    /// Empty for a class-less flush, and with the `const` kill switch
    /// [`NOT_ENTRANT_ON_REDEFINITION_ENABLED`] off, which restores the
    /// pre-wave-22 behaviour (a baked caller keeps entering the old body).
    /// The patch itself needs the cache armed with this VM's helper
    /// (`JitCache::arm_not_entrant`, done by `redefine_class_with`); an unarmed
    /// cache patches nothing, which is what the unit tests below see.
    fn not_entrant_candidates(
        &self,
        class: Option<(ClassId, &str, bool)>,
        whole_cache: bool,
    ) -> Vec<Arc<cratonvm_jit::CompiledMethod>> {
        match class {
            Some((class_id, class_name, _)) if NOT_ENTRANT_ON_REDEFINITION_ENABLED => self
                .jit_cache
                .redefinition_stale_bodies(class_id, class_name, whole_cache),
            _ => Vec::new(),
        }
    }

    /// The optimizing OSR memo's bodies (`JitBridgeState::osr_optimizing_bodies`)
    /// a redefinition of `class` makes stale -- all of them for a whole-cache
    /// flush -- for `JitCache::mark_withdrawn_by_redefinition` (interpreter
    /// round i1 wave 23, lane L6). The memo is expired by the code-state epoch
    /// the redefinition advances, which stops a new entry; the mark is for a
    /// frame already running one. Same kill switch and class-less rule as
    /// [`Self::not_entrant_candidates`].
    fn withdrawn_osr_memo_candidates(
        &self,
        class: Option<(ClassId, &str, bool)>,
        whole_cache: bool,
    ) -> Vec<Arc<cratonvm_jit::CompiledMethod>> {
        match class {
            Some((class_id, class_name, _)) if NOT_ENTRANT_ON_REDEFINITION_ENABLED => {
                let mut bodies = self.bridge.osr_optimizing_bodies();
                // Interpreter round i1 wave 26, lane L6: and every optimizing
                // OSR body still alive that the memo already forgot (its
                // code-state epoch moved, a later build replaced it) while a
                // frame may still run it. Such a body was withdrawn by a
                // whole-cache redefinition's barrier, but listed nowhere, so
                // its polls were not forced and a frame inside a call during
                // the handshake kept its old splice after the call returned.
                let listed: FxHashSet<u64> =
                    bodies.iter().map(|body| body.artifact_id).collect();
                bodies.extend(
                    self.jit_cache
                        .live_unpublished_exit_poll_bodies()
                        .into_iter()
                        .filter(|body| !listed.contains(&body.artifact_id)),
                );
                if !whole_cache {
                    bodies.retain(|body| {
                        cratonvm_jit::JitCache::is_stale_for_redefinition(
                            body, class_id, class_name,
                        )
                    });
                }
                bodies
            }
            _ => Vec::new(),
        }
    }

    /// Send every method whose body was withdrawn back to the interpreter
    /// tier (`TieredCompilationManager::note_body_withdrawn`), so it can be
    /// offered for compilation again: `current_tier` only advances, so a
    /// method at C2 whose body a flush or a scoped eviction withdrew was
    /// never recompiled by the tiered path. The redefined class's own
    /// methods are reset by the caller as well; demoting them twice is
    /// idempotent.
    ///
    /// A no-op when the cache reports its own withdrawals to this VM's
    /// manager (`JitCache::install_withdrawal_sink`, tier proposal W3-1,
    /// round 12 wave 4), which is the default: it has already demoted every
    /// key here that lost its method-entry body. Kept for
    /// `CRATONVM_JIT_WITHDRAWAL_DEMOTES=0`, which leaves the cache unarmed.
    fn demote_withdrawn(&self, withdrawn: &[cratonvm_jit::WithdrawnMethodKey]) {
        if self.jit_cache.reports_withdrawals() {
            return;
        }
        for (class_name, method_name, descriptor, class_id) in withdrawn {
            self.tiered_manager
                .note_body_withdrawn(&crate::jit::tiered::MethodKey::with_class_id(
                    *class_id,
                    Arc::clone(class_name),
                    Arc::clone(method_name),
                    Arc::clone(descriptor),
                ));
        }
    }

    /// Withdraw EVERY compiled body of this VM so that compiled code already
    /// running goes back to the interpreter, for a debugger or agent request
    /// that needs every method interpreted (interpreter round i1 wave 37, lane
    /// L1; the "Wave 29 note" of
    /// `docs/known-issues/interpreter/i18-L1-proposal-per-thread-interpreter-only-mode-20260925.md`).
    /// The whole-cache arm of [`Self::redefine_and_flush`] without the
    /// redefinition: no class changed, so the redefine epochs and verdicts
    /// stay, and no body is spared as an obsolete activation.
    ///
    /// In order: the flush barrier raised before the scan (no compile in
    /// flight publishes between the scan and the flush); every published
    /// body, and every body a baked caller or a forward still reaches, listed
    /// (`JitCache::redefinition_stale_bodies` with `whole_cache`); the
    /// optimizing OSR bodies, which the cache never publishes; the flush
    /// (`clear_all_collecting`), the tier demotions and the code-state epoch
    /// (queued compile requests and the OSR memo expire, as for
    /// [`Self::flush_code_cache`]); then every listed body made not entrant
    /// (a baked call re-dispatches through the interpreter's invoke path) and
    /// marked withdrawn, and the exit polls and post-call exit sites of every
    /// such body forced, so a frame already running one leaves at its next
    /// exit-capable point whether or not a pause reaches its thread
    /// (`jvmti_events::withdrawn_body_may_leave`). The not-entrant hook must
    /// be armed first (`JitCache::arm_not_entrant`), or nothing is patched.
    ///
    /// The cost is HotSpot's for the same request: every hot method is
    /// compiled again once the request ends. Returns `(evicted, made not
    /// entrant, bodies whose exits were forced)`.
    pub(crate) fn withdraw_every_body_for_the_interpreter(&self) -> (usize, usize, usize) {
        // No class is redefined: a class id no body is compiled from, and a
        // name no label starts with, so nothing is spared as its own class's
        // obsolete activation. The name is what the passes' debug lines print.
        let no_class = ClassId::new(u32::MAX);
        let no_name = "<interpreter-only>";
        self.jit_cache
            .fence_redefinition_publications(no_class, no_name, true);
        let stale = self
            .jit_cache
            .redefinition_stale_bodies(no_class, no_name, true);
        let mut osr_bodies = self.bridge.osr_optimizing_bodies();
        let listed: FxHashSet<u64> = osr_bodies.iter().map(|body| body.artifact_id).collect();
        osr_bodies.extend(
            self.jit_cache
                .live_unpublished_exit_poll_bodies()
                .into_iter()
                .filter(|body| !listed.contains(&body.artifact_id)),
        );
        let (evicted, withdrawn) = self.jit_cache.write().clear_all_collecting();
        self.demote_withdrawn(&withdrawn);
        self.advance_code_state_epoch();
        let patched = self.jit_cache.make_not_entrant(&stale);
        self.jit_cache
            .mark_withdrawn_by_redefinition(&osr_bodies);
        let bodies: Vec<Arc<cratonvm_jit::CompiledMethod>> =
            stale.iter().chain(&osr_bodies).cloned().collect();
        let forced = self
            .jit_cache
            .force_withdrawn_exit_polls(&bodies, no_class, no_name);
        (evicted, patched, forced)
    }

    /// The part of [`Self::withdraw_every_body_for_the_interpreter`] that
    /// concerns compiles not yet published, for an arm of the interpreter-only
    /// mode that finds nothing published since the last withdrawal
    /// (interpreter round i1 wave 39, lane L1;
    /// `jvmti_events::note_every_method_needs_the_interpreter`): the flush
    /// barrier, so a compile already begun does not publish afterwards, and
    /// the code-state epoch, so a compile request queued meanwhile (an OSR or
    /// eager door during the linger) expires. Nothing is evicted, made not
    /// entrant or forced: the last withdrawal did that to every body there is.
    #[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
    pub(crate) fn fence_compiles_for_the_interpreter(&self) {
        self.jit_cache.fence_redefinition_publications(
            ClassId::new(u32::MAX),
            "<interpreter-only>",
            true,
        );
        self.advance_code_state_epoch();
    }

    /// Withdraw the compiled bodies that would keep a breakpoint just set in a
    /// method of the class `(class_id, class_name)` from being hit
    /// (interpreter round i1 wave 38, lane L1; the scoped withdrawal of
    /// `docs/internal/fixed-bugs/interpreter-L1-a-breakpoint-in-a-compiled-callee-of-a-running-compiled-caller-is-missed-FIXED-20261004.md`):
    /// the class's own bodies, the bodies that inlined one of its methods or
    /// copied its bytecode, and their baked callers
    /// (`JitCache::withdraw_class_for_the_interpreter`). Those it lists are made
    /// not entrant (a compiled caller's baked call re-dispatches through the
    /// interpreter's invoke path, where the doors refuse the breakpoint's
    /// method) and have their exits forced (a frame already running one
    /// leaves at its next exit-capable point, `jvmti_events::withdrawn_body_may_leave`);
    /// the optimizing OSR bodies of the class, which the cache never
    /// publishes, are marked withdrawn (the OSR memo then forgets them). Every
    /// other compiled body stays. HotSpot deoptimizes the breakpoint method's
    /// dependents for the same request (`JvmtiBreakpoint::set`).
    ///
    /// The scoped redefinition's eviction without what makes it a
    /// redefinition: no per-class redefinition barrier, no redefine epoch, no
    /// census, no code-state epoch (the OSR memo drops a marked body at its
    /// next lookup). The exit polls are forced under a class id no body is
    /// compiled from, so no body of the class is spared as an obsolete
    /// activation: none is, the class was not redefined.
    ///
    /// When a scoped eviction may miss a dependent -- some compile copied the
    /// class's bytecode without a record, or the class is `java/lang/Object`
    /// ([`Self::scoped_redefinition_admitted`]) -- every body is withdrawn
    /// instead ([`Self::withdraw_every_body_for_the_interpreter`]). The
    /// not-entrant hook must be armed first. Returns `(evicted, made not
    /// entrant, bodies whose exits were forced, scoped)`.
    #[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
    pub(crate) fn withdraw_class_for_the_interpreter(
        &self,
        class_id: ClassId,
        class_name: &str,
    ) -> (usize, usize, usize, bool) {
        if !self.scoped_redefinition_admitted(class_id, class_name, false) {
            let (evicted, patched, forced) = self.withdraw_every_body_for_the_interpreter();
            return (evicted, patched, forced, false);
        }
        let (evicted, withdrawn, stale) = self
            .jit_cache
            .withdraw_class_for_the_interpreter(class_id, class_name);
        self.demote_withdrawn(&withdrawn);
        let mut osr_bodies = self.bridge.osr_optimizing_bodies();
        let listed: FxHashSet<u64> = osr_bodies.iter().map(|body| body.artifact_id).collect();
        osr_bodies.extend(
            self.jit_cache
                .live_unpublished_exit_poll_bodies()
                .into_iter()
                .filter(|body| !listed.contains(&body.artifact_id)),
        );
        osr_bodies.retain(|body| {
            cratonvm_jit::JitCache::is_stale_for_redefinition(body, class_id, class_name)
        });
        let patched = self.jit_cache.make_not_entrant(&stale);
        self.jit_cache
            .mark_withdrawn_by_redefinition(&osr_bodies);
        let bodies: Vec<Arc<cratonvm_jit::CompiledMethod>> =
            stale.iter().chain(&osr_bodies).cloned().collect();
        let forced = self.jit_cache.force_withdrawn_exit_polls(
            &bodies,
            ClassId::new(u32::MAX),
            "<breakpoint>",
        );
        (evicted, patched, forced, true)
    }

    /// Flush this VM's code cache (`JitCache::clear_all`) and expire what a
    /// flush expires: the compile verdicts that depend on compile-time state,
    /// the runtime de-speculation verdicts, and everything else stamped with
    /// the code-state epoch — one counter, which both registries read as
    /// their flush epoch (interpreter round i1 wave 15). Returns the number of
    /// bodies evicted.
    ///
    /// Every production `clear_all` of a VM's cache goes through here or
    /// [`Self::note_class_redefinition_of`]: the registries document that a
    /// flush retires their verdicts, and until interpreter round i1 wave 14
    /// the layout-upgrade flush (`jit_invalidate_adapter`) moved only the
    /// process epoch, so after wave 13 gave the registries their own epochs it
    /// retired nothing there.
    pub(crate) fn flush_code_cache(&self) -> usize {
        let (evicted, withdrawn) = self.jit_cache.write().clear_all_collecting();
        self.demote_withdrawn(&withdrawn);
        self.advance_code_state_epoch();
        evicted
    }
}

/// Kill switch for the class-scoped redefinition eviction (interpreter round
/// i1 wave 20, lane L1; stage 2b of
/// `docs/known-issues/interpreter/i14-L3-proposal-class-scoped-redefinition-invalidation-20260925.md`).
/// `true`: a redefinition withdraws only the compiled bodies it can make
/// stale when [`JitRealm::scoped_redefinition_admitted`] holds. `false`: every
/// redefinition flushes the VM's whole code cache, the pre-wave-20 behaviour.
pub(crate) const SCOPED_REDEFINITION_EVICTION_ENABLED: bool = true;

/// Kill switch for making the compiled bodies a redefinition makes stale NOT
/// ENTRANT (interpreter round i1 wave 22, lane L6; `jit/src/not_entrant.rs`,
/// `docs/internal/fixed-bugs/interpreter-L6-baked-calls-outside-retire-cells-reach-a-redefined-callees-old-body-FIXED-20260926.md`).
/// `true`: after its eviction a redefinition patches the entry of every stale
/// body so a baked caller's next call re-dispatches to the new bytecode.
/// `false`: a caller that baked a stale body keeps entering it, the
/// pre-wave-22 behaviour.
pub(crate) const NOT_ENTRANT_ON_REDEFINITION_ENABLED: bool = true;

/// Interpreter round i1 wave 14, lane L3: one code-state epoch per VM, moved
/// by that VM's redefinitions and flushes only
/// (`interpreter-L3-per-vm-state-still-keyed-by-the-process-install-epoch-FIXED`).
#[cfg(test)]
mod tests {
    use crate::classloading::ClassId;
    use crate::config::VmConfig;
    use crate::jit::tiered::MethodKey;
    use crate::vm::SharedVm;

    /// The tiered manager stamps with the realm's epoch: another VM's
    /// redefinition or flush neither moves it nor expires an OSR denial, and
    /// this VM's own flush does both.
    #[test]
    fn another_vms_redefinition_does_not_expire_this_vms_osr_denials() {
        let first = SharedVm::new(VmConfig::default());
        let second = SharedVm::new(VmConfig::default());
        let tiered = &first.jit.tiered_manager;
        assert_eq!(tiered.install_epoch(), first.jit.code_state_epoch());
        let key = MethodKey::with_class_id(ClassId::new(0x1403), "l3/Denied", "loop", "()V");
        tiered.mark_osr_denied(key.clone());
        let before = first.jit.code_state_epoch();

        // What `redefine_class_with` and `jit_invalidate_adapter` do to the
        // second VM; both move the process-wide install epoch too.
        second.jit.note_class_redefinition();
        second.jit.flush_code_cache();
        assert_eq!(first.jit.code_state_epoch(), before);
        assert_eq!(tiered.install_epoch(), before, "queued requests stay fresh");
        assert!(tiered.is_osr_denied(&key));

        first.jit.flush_code_cache();
        assert!(first.jit.code_state_epoch() > before);
        assert_eq!(tiered.install_epoch(), first.jit.code_state_epoch());
        assert!(
            !tiered.is_osr_denied(&key),
            "this VM's own flush expires it"
        );
    }

    /// A layout-upgrade flush (`flush_code_cache`) retires the compile verdicts
    /// that depend on compile-time state and the runtime de-speculation
    /// verdicts, as a `clear_all` did while they read the process epoch; a
    /// redefinition retires every verdict.
    #[test]
    fn a_flush_retires_this_vms_flush_dependent_verdicts() {
        let shared = SharedVm::new(VmConfig::default());
        let cid = ClassId::new(0x1404);
        shared
            .jit
            .runtime_despec
            .mark(cid, "l3/Spec", "spec", "()V");
        let bailed = || {
            shared
                .jit
                .runtime_despec
                .is_not_compilable(cid, "l3/Spec", "spec", "()V")
        };
        assert!(bailed());
        shared.jit.flush_code_cache();
        assert!(
            !bailed(),
            "a flush retires a runtime de-speculation verdict"
        );

        let verdicts = shared.jit.tiered_manager.verdicts();
        verdicts.mark_bail_listed(cid, "l3/Bailed", "run", "()V");
        assert!(verdicts.is_bail_listed(cid, "l3/Bailed", "run", "()V"));
        shared.jit.note_class_redefinition();
        assert!(!verdicts.is_bail_listed(cid, "l3/Bailed", "run", "()V"));
    }

    /// Interpreter round i1 wave 15, lane L4: the registries' flush epoch IS
    /// the code-state epoch. A flush advances it once and retires the runtime
    /// de-speculation verdict through it (`flush_code_cache` no longer calls
    /// the registries); a registry's own flush hook does not move it.
    #[test]
    fn the_verdict_registries_flush_on_the_code_state_epoch() {
        let shared = SharedVm::new(VmConfig::default());
        let cid = ClassId::new(0x1505);
        let despec = &shared.jit.runtime_despec;
        despec.mark(cid, "l4/Spec", "spec", "()V");
        let before = shared.jit.code_state_epoch();
        despec.note_code_cache_flush();
        shared.jit.tiered_manager.verdicts().note_code_cache_flush();
        assert_eq!(shared.jit.code_state_epoch(), before, "owned by the realm");
        assert!(despec.is_not_compilable(cid, "l4/Spec", "spec", "()V"));

        shared.jit.flush_code_cache();
        assert_eq!(
            shared.jit.code_state_epoch(),
            before + 1,
            "one counter, one move"
        );
        assert!(!despec.is_not_compilable(cid, "l4/Spec", "spec", "()V"));
    }

    /// Interpreter round i1 wave 17, lane L5
    /// (`i15-L4-proposal-one-redefine-epoch-per-vm`): both registries share the
    /// realm's redefine epoch. Their own redefinition hooks move nothing; one
    /// `note_class_redefinition` advances it once and expires a bytecode-only
    /// verdict (the bail list, which a flush alone keeps) in the compile
    /// registry and the runtime de-speculation verdict, and is counted by the
    /// redefinition census.
    #[test]
    fn the_verdict_registries_share_the_realms_redefine_epoch() {
        use std::sync::atomic::Ordering;
        let shared = SharedVm::new(VmConfig::default());
        let cid = ClassId::new(0x1705);
        let verdicts = shared.jit.tiered_manager.verdicts();
        let despec = &shared.jit.runtime_despec;
        verdicts.mark_bail_listed(cid, "l5/Bailed", "run", "()V");
        despec.mark(cid, "l5/Spec", "spec", "()V");
        let before = shared.jit.redefine_epoch.load(Ordering::Acquire);
        let census_before = verdicts.redefinition_census();

        verdicts.note_redefinition();
        despec.note_redefinition();
        assert_eq!(
            shared.jit.redefine_epoch.load(Ordering::Acquire),
            before,
            "owned by the realm"
        );
        shared.jit.flush_code_cache();
        assert!(
            verdicts.is_bail_listed(cid, "l5/Bailed", "run", "()V"),
            "a flush keeps a bytecode-only verdict"
        );

        shared
            .jit
            .note_class_redefinition_of(cid, "l5/Bailed", false);
        assert_eq!(
            shared.jit.redefine_epoch.load(Ordering::Acquire),
            before + 1,
            "one counter, one move"
        );
        assert!(!verdicts.is_bail_listed(cid, "l5/Bailed", "run", "()V"));
        assert!(!despec.is_not_compilable(cid, "l5/Spec", "spec", "()V"));
        let census = verdicts.redefinition_census();
        assert_eq!(census.0, census_before.0 + 1, "one redefinition counted");
    }

    /// Interpreter round i1 wave 19, lane L5
    /// (`interpreter-L5-c2-supersede-epoch-is-process-wide-FIXED`): the supersede epoch is
    /// per VM. A C2 publish in one VM (the worker's `bump_supersede_epoch`)
    /// stales that VM's `Jit` invoke-cache gates and leaves another VM's
    /// current.
    #[test]
    fn a_supersede_in_one_vm_leaves_another_vms_jit_entries_current() {
        let publisher = SharedVm::new(VmConfig::default());
        let bystander = SharedVm::new(VmConfig::default());
        let (in_publisher, in_bystander) = (
            publisher.jit.supersede_gate(),
            bystander.jit.supersede_gate(),
        );
        let bystander_before = bystander.jit.supersede_epoch();

        publisher.jit.bump_supersede_epoch();

        assert!(in_publisher.is_stale(), "the publishing VM re-resolves");
        assert!(
            !in_bystander.is_stale(),
            "another VM's entries cannot hold the replaced body"
        );
        assert_eq!(bystander.jit.supersede_epoch(), bystander_before);
        assert!(
            !publisher.jit.supersede_gate().is_stale(),
            "a gate taken after the publish is current"
        );
    }

    /// Interpreter round i1 wave 17, lane L5: the scope the JIT invalidate
    /// hook's adapter reads to leave an in-place redefinition's flush to
    /// `redefine_class_with` names exactly the class being redefined, on this
    /// thread only, and restores the outer value when it ends.
    #[test]
    fn the_in_place_redefinition_scope_names_one_class_on_one_thread() {
        use super::{in_place_redefinition_of, InPlaceRedefinitionScope};
        let (redefined, other) = (ClassId::new(0x1706), ClassId::new(0x1707));
        assert!(!in_place_redefinition_of(redefined));
        {
            let _outer = InPlaceRedefinitionScope::enter(redefined);
            assert!(in_place_redefinition_of(redefined));
            assert!(
                !in_place_redefinition_of(other),
                "only the class being redefined"
            );
            let seen_elsewhere = std::thread::spawn(move || in_place_redefinition_of(redefined))
                .join()
                .expect("the probe thread runs");
            assert!(!seen_elsewhere, "only on the redefining thread");
            {
                let _inner = InPlaceRedefinitionScope::enter(other);
                assert!(in_place_redefinition_of(other));
            }
            assert!(
                in_place_redefinition_of(redefined),
                "the outer scope is restored"
            );
        }
        assert!(!in_place_redefinition_of(redefined));
    }

    /// A one-`RET` body, publishable under any key.
    fn i20_l1_body() -> cratonvm_jit::CompiledMethod {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&[0xC3]); // RET
        cratonvm_jit::CompiledMethod::new(buf)
    }

    /// Publish a body for `(id, class, method, "()V")` and mark the method
    /// compiled at C1 in the tiered manager, as a compile door would.
    fn i20_l1_publish(shared: &SharedVm, id: ClassId, class: &str, method: &str) -> MethodKey {
        use crate::jit::tiered::CompilationTier;
        use std::sync::Arc;
        let key = MethodKey::with_class_id(id, class, method, "()V");
        let tiered = &shared.jit.tiered_manager;
        tiered.on_method_invocation(&key);
        tiered.compilation_complete(&key, CompilationTier::C1, 1);
        shared.jit.jit_cache.put(
            Arc::from(class),
            Arc::from(method),
            Arc::from("()V"),
            id,
            i20_l1_body(),
        );
        key
    }

    /// Interpreter round i1 wave 20, lane L1 (stage 2b of
    /// `i14-L3-proposal-class-scoped-redefinition-invalidation`): a
    /// redefinition of a class nothing copied withdraws the class's own body
    /// and keeps an unrelated one published and at its tier; the withdrawn
    /// method goes back to the interpreter tier so it can be compiled again.
    #[test]
    fn a_scoped_redefinition_keeps_unrelated_bodies_and_demotes_what_it_withdrew() {
        use crate::jit::tiered::CompilationTier;
        if !super::SCOPED_REDEFINITION_EVICTION_ENABLED {
            return;
        }
        let shared = SharedVm::new(VmConfig::default());
        let (redefined, other) = (ClassId::new(0x2041), ClassId::new(0x2042));
        let own = i20_l1_publish(&shared, redefined, "i20/l1/Redefined", "own");
        let kept = i20_l1_publish(&shared, other, "i20/l1/Other", "kept");
        assert!(shared
            .jit
            .scoped_redefinition_admitted(redefined, "i20/l1/Redefined", false));
        let before = shared.jit.code_state_epoch();

        let evicted = shared
            .jit
            .note_class_redefinition_of(redefined, "i20/l1/Redefined", false);

        assert_eq!(evicted, 1, "only the redefined class's body");
        let cache = &shared.jit.jit_cache;
        assert!(cache.get("i20/l1/Other", "kept", "()V", other).is_some());
        assert!(cache
            .get("i20/l1/Redefined", "own", "()V", redefined)
            .is_none());
        let tiered = &shared.jit.tiered_manager;
        assert_eq!(tiered.current_tier(&own), CompilationTier::Interpreter);
        assert_eq!(tiered.current_tier(&kept), CompilationTier::C1);
        assert!(
            shared.jit.code_state_epoch() > before,
            "queued requests still drop (stage 3 would scope this)"
        );
    }

    /// Round 12 wave 4 (lane withdraw, tier proposal W3-1): a VM's cache is
    /// armed with its tiered manager, so a door with no demotion of its own
    /// (here the class-hierarchy change `define_class` runs) still sends the
    /// method it withdrew back to the interpreter tier.
    #[test]
    fn a_class_hierarchy_withdrawal_demotes_through_the_vms_own_cache() {
        use crate::jit::tiered::CompilationTier;
        use std::sync::Arc;
        let shared = SharedVm::new(VmConfig::default());
        let jit = &shared.jit;
        if !jit.jit_cache.reports_withdrawals() {
            // `CRATONVM_JIT_WITHDRAWAL_DEMOTES=0` in this process.
            return;
        }
        let id = ClassId::new(0x1204);
        let key = MethodKey::with_class_id(id, "w4/Caller", "hot", "()V");
        jit.tiered_manager.on_method_invocation(&key);
        jit.tiered_manager
            .compilation_complete(&key, CompilationTier::C2, 1);
        let mut body = i20_l1_body();
        body.inlined_methods = vec![("w4/Shape".into(), "area".into(), "()I".into())];
        jit.jit_cache.put(
            Arc::from("w4/Caller"),
            Arc::from("hot"),
            Arc::from("()V"),
            id,
            body,
        );

        assert_eq!(jit.jit_cache.invalidate_for_class_change("w4/Shape"), 1);
        assert_eq!(
            jit.tiered_manager.current_tier(&key),
            CompilationTier::Interpreter
        );
    }

    /// The redefinitions a scoped eviction cannot bound keep the full flush:
    /// `java/lang/Object`, a class with a registered native, and a class some
    /// compile copied bytecode of. The full flush also demotes every method
    /// it withdrew, which it did not before wave 20 (such a method stayed at
    /// its tier with no body, so the tiered path never offered it again).
    #[test]
    fn a_redefinition_no_body_records_takes_the_full_flush_and_demotes() {
        use crate::jit::tiered::CompilationTier;
        let shared = SharedVm::new(VmConfig::default());
        let (spliced, other) = (ClassId::new(0x2043), ClassId::new(0x2044));
        let jit = &shared.jit;
        assert!(!jit.scoped_redefinition_admitted(other, "java/lang/Object", false));
        assert!(!jit.scoped_redefinition_admitted(other, "i20/l1/Native", true));
        jit.jit_cache.note_bytecode_copied(spliced);
        assert!(!jit.scoped_redefinition_admitted(spliced, "i20/l1/Spliced", false));

        let unrelated = i20_l1_publish(&shared, other, "i20/l1/Unrelated", "run");
        let evicted = jit.note_class_redefinition_of(spliced, "i20/l1/Spliced", false);
        assert_eq!(evicted, 1, "the full flush withdraws the unrelated body");
        assert!(jit.jit_cache.is_empty());
        assert_eq!(
            jit.tiered_manager.current_tier(&unrelated),
            CompilationTier::Interpreter,
            "and sends its method back to the interpreter tier"
        );
    }

    /// Round 12 wave 5 (lane tier3): only a define that could have replaced a
    /// class withdraws by name. A fresh id (at or above the store's next id
    /// before the define) sits beside another loader's same-named class; an
    /// id that was already in the store is a stub upgraded in place, and
    /// `allow_redefine` re-keys the name in its own loader.
    #[test]
    fn a_define_withdraws_by_name_only_when_it_can_replace_a_class() {
        use super::define_withdraws_by_name_when as withdraws;
        let before = ClassId::new(0x300);
        let fresh = ClassId::new(0x300);
        let fresh_after_supertypes = ClassId::new(0x305);
        let upgraded_stub = ClassId::new(0x2ff);
        assert!(!withdraws(fresh, before, false, true));
        assert!(!withdraws(fresh_after_supertypes, before, false, true));
        assert!(withdraws(upgraded_stub, before, false, true));
        assert!(withdraws(fresh, before, true, true), "allow_redefine");
        // The kill switch restores the by-name withdrawal for every define.
        assert!(withdraws(fresh, before, false, false));
        assert!(withdraws(upgraded_stub, before, false, false));
    }

    /// Interpreter round i1 wave 24, lane L6: a whole-cache redefinition
    /// withdraws a body neither the cache nor the optimizing OSR memo lists --
    /// an optimizing OSR body a frame still runs after the memo forgot it --
    /// so the safepoint slow path tells its back-edge poll to leave; a body
    /// compiled after the redefinition is not withdrawn. Before wave 24 such a
    /// body was never marked, and the only bodies marked were the ones a scan
    /// reached.
    #[test]
    fn a_whole_cache_redefinition_withdraws_a_body_nothing_reaches() {
        use std::sync::Arc;
        let shared = SharedVm::new(VmConfig::default());
        let jit = &shared.jit;
        let spliced = ClassId::new(0x2440);
        let running = Arc::new(i20_l1_body());
        assert!(!jit.jit_cache.body_withdrawn_by_redefinition(&running));
        let before = jit.jit_cache.bodies_withdrawn_by_redefinition();
        jit.jit_cache.note_bytecode_copied(spliced);
        assert!(!jit.scoped_redefinition_admitted(spliced, "i24/l6/Spliced", false));

        let _ = jit.note_class_redefinition_of(spliced, "i24/l6/Spliced", false);

        assert!(
            jit.jit_cache.bodies_withdrawn_by_redefinition() > before,
            "the redefinition path's loop-exit handshake trigger moves"
        );
        assert!(!running.is_withdrawn_by_redefinition(), "no scan reached it");
        assert!(jit.jit_cache.body_withdrawn_by_redefinition(&running));
        assert!(
            running.is_withdrawn_by_redefinition(),
            "the answer is recorded for the exit sinks, which read the mark"
        );
        let fresh = i20_l1_body();
        assert!(!jit.jit_cache.body_withdrawn_by_redefinition(&fresh));
    }
}
