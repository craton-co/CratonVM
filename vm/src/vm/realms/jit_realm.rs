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

/// One row of [`JitRealm::execute_reentry_records`] (interpreter round i3
/// wave 79, lane L7).
pub struct ExecuteReentryRecord {
    /// The method's `Code` bytes the record was built from (a clone of the
    /// class's own view). A hit requires the class's current method to hold
    /// the same allocation and range: while this clone keeps the allocation
    /// alive no other class file can occupy it, so a redefinition (EMCP
    /// included), a synthetic class upgraded in place, or another class under
    /// the id can only miss.
    pub code_view: cratonvm_reader::ByteView,
    /// The record a frame is pushed from (`Frame::new_cached_unpooled`).
    pub cached: Arc<crate::classloading::resolution::CachedBytecodeMethod>,
}

impl ExecuteReentryRecord {
    /// Whether this row was built from exactly `code` (same bytes in the same
    /// allocation).
    #[inline]
    pub fn built_from(&self, code: &cratonvm_reader::ByteView) -> bool {
        let mine: &[u8] = &self.code_view;
        let theirs: &[u8] = code;
        std::ptr::eq(mine.as_ptr(), theirs.as_ptr()) && mine.len() == theirs.len()
    }
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

    /// The constructors the finalizer compile bar refused at `execute`'s gate
    /// (`exception_dispatch::finalizer_bars_compiling`), per declaring class
    /// id, their descriptors (JIT round 21 wave 14, lane finalize; item 3 of
    /// `r21-review-w14-findings-20261019.md`). Wave 13 sealed the refusal
    /// into the NAME-keyed `jit_skip_set`, so a same-named class of another
    /// loader, finalizable or not, was never compiled at that door. Rows of
    /// unloaded classes are dropped by the unload sweep (`memory/gc.rs`,
    /// beside [`Self::jit_gate_pass`]). Since JIT round 21 wave 18 each row
    /// carries the `JitCache::redefine_epoch` it was filled in: a redefinition
    /// can make a refused constructor compilable again, and under
    /// `CRATONVM_FINALIZER_BAR_REFUSAL_EPOCH` a row of another epoch is
    /// re-asked (`exception_dispatch::finalizer_bars_compiling_memoized`).
    pub finalizer_bar_refusals: RwLock<FxHashMap<ClassId, (u32, Vec<Arc<str>>)>>,

    /// The method records `interpreter::execute` pushes CACHED frames from
    /// under `CRATONVM_EXECUTE_CACHED_REENTRY_FRAME` (interpreter round i3
    /// wave 79, lane L7; `VmConfig::execute_cached_reentry_frame`), keyed by
    /// `(class id, declared method index)`. Per VM because a `ClassId` is. A
    /// row answers only while the class's method at that index still holds
    /// the very `Code` bytes the record was built from
    /// ([`ExecuteReentryRecord::code_view`]); anything else rebuilds and
    /// replaces it. Rows of unloaded classes are dropped by the unload sweep
    /// (`memory/gc.rs`, beside [`Self::jit_gate_pass`]); ids are not reused, so
    /// a missed row would be a leak, never a wrong answer. Empty with the
    /// switch off.
    pub execute_reentry_records: RwLock<FxHashMap<(ClassId, u32), ExecuteReentryRecord>>,

    /// The declared method index `interpreter::execute` found for a `(name,
    /// descriptor)` of a class, under `CRATONVM_EXECUTE_METHOD_INDEX_MEMO`
    /// (interpreter round i6 wave 115, lane L7; `VmConfig::execute_method_index_memo`):
    /// keyed by `(class id, FxHash of name and descriptor)`
    /// (`interpreter::execute_method_index_key`), so a native re-entry into a
    /// class that declares hundreds of methods (`DirectMethodHandle$Holder`:
    /// 217 in JDK 25) skips the linear by-name scan. A row is used only when
    /// the class's method at that index still has exactly that name and
    /// descriptor, so a redefinition that reorders the methods, or a hash
    /// collision, is a miss and is rescanned and refiled. Rows of unloaded
    /// classes are dropped by the unload sweep (`memory/gc.rs`, beside
    /// [`Self::execute_reentry_records`]). Empty with the switch off.
    pub execute_method_index: RwLock<FxHashMap<(ClassId, u64), u32>>,

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

    /// nio21 wave 7, lane L3: this VM's `Unsafe` inline state -- the
    /// instance-field cell table the compiled RMW prefixes read (W6-L3-1) and
    /// the `compareAndExchange*` rows' policy memos (W6-L3-2). Per VM, not a
    /// process static: a cell names this VM's class ids, and a memo caches
    /// this VM's native resolution. See [`UnsafeInlineCells`].
    pub(crate) unsafe_inline: UnsafeInlineCells,

    /// nio21 wave 8, lane L2: this VM's memos for the thin direct-call
    /// helpers' policy gates (`jit::helpers::DirectHelperGate`), one
    /// `NativeCallSite` per gate ordinal, plus the per-VM latch of
    /// `mark_direct_call_helper_natives_incomplete`. Both were process
    /// statics holding per-VM answers. See [`DirectHelperGateSites`].
    pub(crate) direct_helper_gates: DirectHelperGateSites,

    /// nio21 wave 11, lane L5 (W9L2-2): this VM's constant-`VarHandle` plans,
    /// filed when a class finishes `<clinit>` and read by both tiers'
    /// `VarHandle` field-access prefixes at compile time. Per VM: a plan names
    /// this VM's class ids. See [`VhConstPlans`].
    pub(crate) vh_const_plans: VhConstPlans,

    /// j27 wave 2 (lane VHCOMP, `CRATONVM_JIT_VH_CONST_STATIC`): this VM's
    /// static constant-`VarHandle` cells, read by compiled code at run time.
    /// Per VM: a cell names this VM's statics. See
    /// [`crate::jit::vh_static_cells::VhStaticCells`].
    pub(crate) vh_static_cells: crate::jit::vh_static_cells::VhStaticCells,

    /// nio21 wave 12, lane L2: this VM's JIT-readable supers displays, the
    /// subclass test of the constant-`VarHandle` prefixes. Per VM: a display
    /// names this VM's class ids. See [`SupersDisplays`].
    pub(crate) supers_displays: SupersDisplays,

    /// JIT round 21 wave 1, lane reflect (REFLECT21-4): this VM's class ids the
    /// compiled reflective links compare per call (the accessor classes, the
    /// eight primitive wrappers), filed on first use. Per VM: they are this
    /// VM's ids. See [`ReflectLinkIds`].
    pub(crate) reflect_link_ids: ReflectLinkIds,
    /// JIT round 21, lane hash2 (`CRATONVM_JIT_OBJECT_HASH_CLASS_TABLE`): this
    /// VM's "inherits `Object.hashCode`" verdict byte per class, read by both
    /// tiers' compiled `Object.hashCode()` identity arm. Per VM: a byte names
    /// this VM's class ids. See [`IdentityHashClassBytes`].
    pub(crate) identity_hash_classes: IdentityHashClassBytes,

    /// JIT round 21, lane hashlock (HASH2-21-1,
    /// `CRATONVM_JIT_OBJECT_EQUALS_IDENTITY_ARM`): this VM's "inherits
    /// `Object.equals(Object)`" verdict byte per class, read by both tiers'
    /// compiled `Object.equals(Object)` identity arm. The same layout, values
    /// and write-once rule as [`Self::identity_hash_classes`]; filed by the same
    /// identity-hash leaves (`jit::helpers::note_identity_hash_receiver`) and
    /// by the compile-time equals query. Per VM: a byte names this VM's class
    /// ids.
    pub(crate) identity_equals_classes: IdentityHashClassBytes,
}

/// JIT round 21 wave 1, lane reflect (REFLECT21-4,
/// `CRATONVM_JIT_REFLECT_ACCESSOR_LINK` / `CRATONVM_JIT_REFLECT_CTOR_LINK`):
/// class ids the reflective link helpers (`jit::helpers`
/// `reflect_accessor_link_target`, `reflect_accessor_arg`) would otherwise
/// look up by name on every call (a class-manager read lock each). `0` means
/// not filed yet; a filed id is the unique class of that name in this VM (the
/// JDK classes named here have one definition), so it never changes. Filed by
/// the first call that resolves the name; relaxed atomics (an unfiled slot
/// only costs the lookup).
#[derive(Default)]
pub(crate) struct ReflectLinkIds {
    /// `jdk/internal/reflect/DirectMethodHandleAccessor`, then
    /// `DirectConstructorHandleAccessor`.
    pub(crate) accessors: [std::sync::atomic::AtomicU32; 2],
    /// The wrapper classes in `IJZBSCFD` order (see `reflect_accessor_wrapper`).
    pub(crate) wrappers: [std::sync::atomic::AtomicU32; 8],
    /// JIT round 22 wave 1, lane runtime (flip review R1 / R3): the spun
    /// `LambdaForm$MH` adapter each linked member's accessor target runs
    /// through, by the member's interned leaf (`JitInvokeInfo` address,
    /// leaked and never reused), filed on the leaf's first linked call
    /// (`helpers::reflect_link_note_adapter`). A linked call runs the member
    /// directly under the compiled `invokeImpl`, so the adapter's frame, which
    /// HotSpot's `SHOW_HIDDEN_FRAMES` walks and `-XX:+ShowHiddenFrames` traces
    /// list, exists only as this record: the hidden stand-in table names it
    /// (`stackwalker::hidden_invoke_standin_frames_with`).
    ///
    /// Review w3 item 3 (lane sync): keyed by the MEMBER (declaring class id,
    /// name, descriptor, kind), not by the leaf's address: leaves are interned
    /// per thread, so an address key filed one row per (thread, member) and a
    /// thread pool's churn grew the map without bound. One row per member now
    /// (the same adapter whichever thread links it); class unload prunes it.
    pub(crate) adapters: parking_lot::RwLock<
        std::collections::HashMap<ReflectLinkAdapterKey, ReflectLinkAdapter>,
    >,
    /// JIT round 22 wave 2, lane runtime: `CRATONVM_JIT_REFLECT_LINK_HIDDEN_LATCH`
    /// (default ON), read once per VM (`helpers::reflect_link_hidden_latch_enabled`).
    pub(crate) hidden_latch_switch: std::sync::OnceLock<bool>,
    /// JIT round 22 wave 2, lane runtime: `CRATONVM_JIT_REFLECT_LINK_HIDDEN_STANDINS`
    /// (default ON since j27 2026-10-28, `=0` kills), read once per VM
    /// (`helpers::reflect_link_hidden_standins_enabled`).
    pub(crate) hidden_standins_switch: std::sync::OnceLock<bool>,
}

/// The key of [`ReflectLinkIds::adapters`]: the linked member's declaring
/// class id, name, descriptor and invoke kind (review w3 item 3).
pub(crate) type ReflectLinkAdapterKey = (u32, &'static str, &'static str, u8);

/// One row of [`ReflectLinkIds::adapters`]: the linked member (its declaring
/// class id and its names, as the leaf states them) and the accessor target's
/// `form.vmentry` (the spun adapter class's id and the entry method's name).
#[derive(Clone, Debug)]
pub(crate) struct ReflectLinkAdapter {
    pub(crate) declaring_class_id: u32,
    pub(crate) method_name: &'static str,
    pub(crate) descriptor: &'static str,
    pub(crate) adapter_class_id: u32,
    pub(crate) adapter_method: std::sync::Arc<str>,
    /// JIT round 22 wave 2, lane runtime: the leaf's `invoke_kind` (0
    /// virtual, 1 special, 2 interface, 3 static), i.e. the accessor handle's
    /// reference kind, which names the `DirectMethodHandle$Holder` frame.
    pub(crate) holder_kind: u8,
}

/// JIT round 21, lane hash2 (page
/// `docs/known-issues/jit/jr21-hash2-object-hashcode-class-table-20261014.md`):
/// one verdict byte per class id, read by the compiled `Object.hashCode()`
/// identity arm of both tiers
/// (`cratonvm_jit::ir_lower::identity_hash_class_table_test_bytes`).
///
/// Layout, pinned by that reader: `[cap: u64 LE, verdict: u8; cap]`, `cap` =
/// `cratonvm_jit::ir_lower::IDENTITY_HASH_CLASS_TABLE_CAPACITY`; byte
/// `8 + class_id` is `IDENTITY_HASH_CLASS_UNKNOWN` (0), `_PROVEN` (1) or
/// `_REFUSED` (2). Allocated (zeroed, so untouched pages are never committed)
/// on the first compile that asks for its address, never moved, freed with
/// the VM.
///
/// Written once per class (`compare_exchange` from 0), by
/// `jit::helpers::note_identity_hash_receiver` when an identity-hash leaf
/// serves an instance of the class, and by the compile-time identity-class
/// query; the verdict is `jit::helpers::class_hash_code_identity_verdict`'s,
/// a property of the class (class ids are never reused, a redefinition cannot
/// add a method, and a stub upgraded in place was refused before and stays
/// refused). A class unload leaves its byte: no instance of it remains.
pub(crate) struct IdentityHashClassBytes {
    bytes: OnceLock<Box<[std::sync::atomic::AtomicU8]>>,
}

impl IdentityHashClassBytes {
    /// An unallocated table: every verdict reads `0`, every record is a no-op.
    pub(crate) const fn new() -> Self {
        Self {
            bytes: OnceLock::new(),
        }
    }

    fn alloc() -> Box<[std::sync::atomic::AtomicU8]> {
        let header = cratonvm_jit::ir_lower::IDENTITY_HASH_CLASS_TABLE_HEADER;
        let cap = cratonvm_jit::ir_lower::IDENTITY_HASH_CLASS_TABLE_CAPACITY;
        // `vec![0; n]` is `alloc_zeroed`.
        let zeroed: Box<[u8]> = vec![0u8; header + cap].into_boxed_slice();
        // SAFETY: `AtomicU8` has the same size, alignment and bit validity as
        // `u8` (documented on `AtomicU8`), and the slice-pointer cast keeps
        // the length, so the box is reinterpreted element for element and
        // freed with the layout it was allocated with.
        let bytes: Box<[std::sync::atomic::AtomicU8]> =
            unsafe { Box::from_raw(Box::into_raw(zeroed) as *mut [std::sync::atomic::AtomicU8]) };
        let cap_word = u64::try_from(cap).unwrap_or(0).to_le_bytes();
        for (cell, b) in bytes.iter().zip(cap_word) {
            cell.store(b, std::sync::atomic::Ordering::Relaxed);
        }
        bytes
    }

    /// The table's address for compiled code to bake, allocating it on first
    /// use. Stable for the VM's life.
    pub(crate) fn base_addr(&self) -> usize {
        // Cast: the allocation's address, for compiled code to bake.
        self.bytes.get_or_init(Self::alloc).as_ptr() as usize
    }

    /// `true` once [`Self::base_addr`] has allocated the table.
    pub(crate) fn is_allocated(&self) -> bool {
        self.bytes.get().is_some()
    }

    fn cell(&self, class_id: u32) -> Option<&std::sync::atomic::AtomicU8> {
        let index = usize::try_from(class_id).ok()?;
        if index >= cratonvm_jit::ir_lower::IDENTITY_HASH_CLASS_TABLE_CAPACITY {
            return None;
        }
        self.bytes
            .get()?
            .get(cratonvm_jit::ir_lower::IDENTITY_HASH_CLASS_TABLE_HEADER + index)
    }

    /// `class_id`'s verdict byte, `0` (unknown) without a table or past the
    /// capacity.
    pub(crate) fn verdict(&self, class_id: u32) -> u8 {
        self.cell(class_id)
            .map_or(0, |c| c.load(std::sync::atomic::Ordering::Acquire))
    }

    /// Record `class_id`'s verdict, once: a decided byte is never rewritten.
    /// A no-op before the table exists and past the capacity.
    pub(crate) fn record(&self, class_id: u32, verdict: u8) {
        if let Some(c) = self.cell(class_id) {
            let _ = c.compare_exchange(
                0,
                verdict,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            );
        }
    }
}

/// nio21 wave 12, lane L2 (page
/// `r21-varhandle-inline-declines-subclass-receivers-and-subtype-values-FIXED-20261012.md`):
/// a JIT-readable superclass display per initialised CLASS (interfaces have
/// none), HotSpot's primary-supers check, read by
/// `cratonvm_jit::ir_lower::vh_const_display_test`.
///
/// Layout, pinned by that reader: `cell` holds the address of the current
/// table `[cap: u64, entry[0..cap]: u64]`; `entry[c]` is `0` or the address of
/// class `c`'s display `[len: u32, ids: [u32; len]]`, its superclass chain from
/// `java/lang/Object` (index 0) down to `c` itself. A display is published
/// once, when the class's `<clinit>` completes, and never changes (a class's
/// superclass chain is fixed for its life, and class ids are not reused). A
/// table that runs out of room is REPLACED by a larger copy and kept, so a
/// compiled reader holding the old one sees a table that merely lacks the
/// newest classes, which it declines. Nothing is freed before the VM is.
///
/// nio21 wave 14, lane L4: the display is followed by the class's transitive
/// interface set, `[n: u32, ifaces: [u32; n]]` at `4 + 4 * len` (the
/// secondary supers, read by `ir_lower::vh_const_iface_test`); and a display
/// is also published on first need -- a compile asking a class's depth, or a
/// thin helper serving a receiver / value the prefix declined -- so a class
/// whose instances are used during its own `<clinit>` is served before that
/// `<clinit>` completes.
pub(crate) struct SupersDisplays {
    cell: Box<std::sync::atomic::AtomicUsize>,
    inner: parking_lot::Mutex<SupersDisplaysInner>,
    /// `CRATONVM_JIT_VH_CONST_INLINE` and `CRATONVM_JIT_VH_CONST_SUBTYPE`,
    /// read once when the VM is built (the lazy publishers sit on helper
    /// paths, where a per-call flag read is not affordable).
    enabled: bool,
    /// JIT r21 wave 1, lane typecheck (TYPECHECK21-4):
    /// `CRATONVM_JIT_CHECKCAST_ENSURE_DISPLAY` (opt-in), read once when the VM
    /// is built: `jit_checkcast` publishes a plain receiver's missing display,
    /// as `jit_instanceof` always has. See [`Self::publish_on_checkcast`].
    publish_on_checkcast: bool,

    /// JIT round 21, lane dispatch (`CRATONVM_JIT_TYPECHECK_ROWS`): the
    /// address of this VM's type-check rows (`inner.rows`), `0` until a
    /// compile first asks for them ([`Self::typecheck_rows_addr`]).
    rows: std::sync::atomic::AtomicUsize,
    /// JIT round 21, lane tcarray (`CRATONVM_JIT_TYPECHECK_ARRAY_MEMO`), read
    /// once when the VM is built: the type-check helpers memoize a
    /// reference-array receiver's `true` ([`Self::array_cast_memo`]).
    array_cast_memo: bool,
}

#[derive(Default)]
struct SupersDisplaysInner {
    /// Every table ever published, the current one last.
    tables: Vec<Box<[std::sync::atomic::AtomicU64]>>,
    /// Every display, kept for the VM's life.
    displays: Vec<Box<[u32]>>,
    /// The class id of each entry of `displays`, in the same order (JIT round
    /// 21, lane dispatch: the type-check rows' backfill reads them).
    ids: Vec<u32>,
    /// `class id -> depth` of every published display.
    depths: FxHashMap<u32, u32>,
    /// JIT r25 wave 8 (W8-2): `target class id -> its instanceof memo`
    /// (`cratonvm_jit::typecheck_miss_memo_cell`), kept for the VM's life: a
    /// compiled site bakes the memo's address, as it bakes `cell`'s.
    miss_memos: FxHashMap<u32, Box<[std::sync::atomic::AtomicU32; 4]>>,

    /// JIT round 21, lane dispatch: the type-check rows, once asked for.
    rows: Option<TypecheckRowsBuf>,
}

/// JIT round 21, lane dispatch (`CRATONVM_JIT_TYPECHECK_ROWS`, page
/// `docs/internal/jit/jr21-dispatch-typecheck-rows-FIXED-20261029.md`): the
/// owned, zero-initialised, `ROW_BYTES`-aligned buffer of this VM's
/// type-check rows, whose layout `cratonvm_jit::typecheck_rows` pins (the
/// compiled `instanceof` / `checkcast` arms read it at a baked address).
/// Allocated with `alloc_zeroed`, so a page is committed only once a row on it
/// is written; never moved, and freed only with the VM, like the display
/// tables. A row is filled once per class, from the facts of its display,
/// under the displays' mutex: words 1.. first, the status word last.
struct TypecheckRowsBuf {
    ptr: std::ptr::NonNull<u8>,
    layout: std::alloc::Layout,
}

// SAFETY: plain memory this value owns; every Rust access is an atomic one
// (under the displays' mutex for writes), compiled code only loads from it.
unsafe impl Send for TypecheckRowsBuf {}
// SAFETY: as above.
unsafe impl Sync for TypecheckRowsBuf {}

impl TypecheckRowsBuf {
    fn new() -> Option<Self> {
        use cratonvm_jit::typecheck_rows::{ROWS_BYTES, ROW_BYTES};
        let layout = std::alloc::Layout::from_size_align(ROWS_BYTES, ROW_BYTES).ok()?;
        // SAFETY: `layout` has a non-zero size.
        let raw = unsafe { std::alloc::alloc_zeroed(layout) };
        std::ptr::NonNull::new(raw).map(|ptr| Self { ptr, layout })
    }

    fn addr(&self) -> usize {
        self.ptr.as_ptr() as usize // Cast: the buffer's address
    }

    /// Fill `class_id`'s row (no-op past `ROWS_CAP`): words 1.. Relaxed, then
    /// the status word with Release, so a reader that sees the status sees
    /// the row (`cratonvm_jit::typecheck_rows`, "Why a row is sound").
    fn fill(&self, class_id: u32, chain: &[u32], interfaces: &[u32], hierarchy_only: bool) {
        use cratonvm_jit::typecheck_rows::{row_words, ROWS_CAP, ROW_BYTES};
        use std::sync::atomic::{AtomicU32, Ordering};
        if class_id >= ROWS_CAP {
            return;
        }
        let words = row_words(chain, interfaces, hierarchy_only);
        // Cast: `class_id < ROWS_CAP`, a row index.
        let offset = class_id as usize * ROW_BYTES;
        // SAFETY: `class_id < ROWS_CAP`, so the row lies inside the buffer
        // (`ROWS_CAP` rows of `ROW_BYTES` bytes), and it is 4-byte aligned
        // (the buffer is `ROW_BYTES`-aligned). `AtomicU32` has `u32`'s
        // layout, and the buffer is only ever accessed through atomics or by
        // compiled loads.
        let row = unsafe { self.ptr.as_ptr().add(offset) } as *const AtomicU32;
        for (i, w) in words.iter().enumerate().skip(1) {
            // SAFETY: `i < ROW_WORDS`, a word of this row.
            unsafe { &*row.add(i) }.store(*w, Ordering::Relaxed);
        }
        // SAFETY: word 0 of the row, as above.
        unsafe { &*row }.store(words[0], Ordering::Release);
    }
}

impl Drop for TypecheckRowsBuf {
    fn drop(&mut self) {
        // SAFETY: allocated by `alloc_zeroed` with this very layout, freed once.
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

/// A published display's `(chain, interfaces, hierarchy_only)`, from its
/// words `[len, chain.., n, ifaces.., verdict]`; `None` for a malformed one.
fn split_display(d: &[u32]) -> Option<(&[u32], &[u32], bool)> {
    let len = usize::try_from(*d.first()?).ok()?;
    let chain = d.get(1..1 + len)?;
    let n = usize::try_from(*d.get(1 + len)?).ok()?;
    let interfaces = d.get(2 + len..2 + len + n)?;
    let verdict = *d.get(2 + len + n)?;
    Some((chain, interfaces, verdict == 1))
}

impl SupersDisplays {
    pub(crate) fn new() -> Self {
        Self {
            cell: Box::new(std::sync::atomic::AtomicUsize::new(0)),
            inner: parking_lot::Mutex::new(SupersDisplaysInner::default()),
            enabled: cratonvm_jit::ir_lower::vh_const_inline_enabled()
                && cratonvm_jit::ir_lower::vh_const_subtype_enabled(),
            publish_on_checkcast: cratonvm_types::flags::runtime_flag_on(
                "CRATONVM_JIT_CHECKCAST_ENSURE_DISPLAY",
            ),

            rows: std::sync::atomic::AtomicUsize::new(0),
            array_cast_memo: cratonvm_jit::typecheck_rows::typecheck_array_memo_enabled(),
        }
    }

    /// JIT round 21, lane tcarray (`CRATONVM_JIT_TYPECHECK_ARRAY_MEMO`): may
    /// `jit_checkcast` / `jit_instanceof` memoize a reference-array receiver's
    /// proven `true`? Per VM, read when it was built (a helper-path question).
    pub(crate) fn array_cast_memo(&self) -> bool {
        self.array_cast_memo
    }

    /// JIT round 21, lane dispatch (`CRATONVM_JIT_TYPECHECK_ROWS`): the
    /// address of this VM's type-check rows, allocating them (and filling a
    /// row for every display already published) on the first ask; `0` when
    /// the VM keeps no displays or the allocation failed (a later ask
    /// retries). Asked by the compile, never on a mutator's fast path.
    pub(crate) fn typecheck_rows_addr(&self) -> usize {
        use std::sync::atomic::Ordering;
        if !self.enabled {
            return 0;
        }
        let addr = self.rows.load(Ordering::Acquire);
        if addr != 0 {
            return addr;
        }
        let mut inner = self.inner.lock();
        if let Some(rows) = inner.rows.as_ref() {
            return rows.addr();
        }
        let Some(rows) = TypecheckRowsBuf::new() else {
            return 0;
        };
        for (id, d) in inner.ids.iter().zip(&inner.displays) {
            if let Some((chain, interfaces, hierarchy_only)) = split_display(d) {
                rows.fill(*id, chain, interfaces, hierarchy_only);
            }
        }
        let addr = rows.addr();
        inner.rows = Some(rows);
        self.rows.store(addr, Ordering::Release);
        addr
    }

    /// JIT r21 wave 1 (lane typecheck, TYPECHECK21-4): does `jit_checkcast`
    /// publish the display of a plain receiver that has none, so the site's
    /// next execution answers inline (`CRATONVM_JIT_CHECKCAST_ENSURE_DISPLAY`,
    /// opt-in; read once per VM)? `false` whenever the VM keeps no displays.
    pub(crate) fn publish_on_checkcast(&self) -> bool {
        self.enabled && self.publish_on_checkcast
    }

    /// Does this VM keep displays at all?
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Is `class_id`'s display published? Lock-free: the same three loads the
    /// compiled reader makes.
    pub(crate) fn is_published(&self, class_id: u32) -> bool {
        use std::sync::atomic::{AtomicU64, Ordering};
        let table = self.cell.load(Ordering::Acquire) as *const AtomicU64;
        if table.is_null() {
            return false;
        }
        // SAFETY: a non-zero cell holds the address of a table this value
        // owns (`inner.tables`, never freed or shrunk while `self` lives),
        // whose word 0 is its capacity and which has `cap + 1` words.
        unsafe {
            let cap = (*table).load(Ordering::Acquire) as usize; // Cast: a capacity
            let idx = class_id as usize; // Cast: a class id indexes the table
            idx < cap && (*table.add(1 + idx)).load(Ordering::Acquire) != 0
        }
    }

    /// The cell's address, baked by the compiled prefixes.
    pub(crate) fn cell_addr(&self) -> usize {
        &*self.cell as *const std::sync::atomic::AtomicUsize as usize
    }

    /// JIT r25 wave 8 (W8-2): the address of the `instanceof` memo of
    /// `target` in this VM (`cratonvm_jit::typecheck_miss_memo_cell` says
    /// what its four ways prove), every way starting at `target`; made on
    /// first ask and kept, at one address, for the VM's life. `0` when this
    /// VM keeps no displays (the memo's facts are display facts) or for class
    /// id 0. It replaced a process `static` keyed by the display cell's
    /// address, which a later VM could reuse.
    pub(crate) fn miss_memo_addr(&self, target: u32) -> usize {
        use std::sync::atomic::AtomicU32;
        if !self.enabled || target == 0 {
            return 0;
        }
        let mut inner = self.inner.lock();
        let memo = inner.miss_memos.entry(target).or_insert_with(|| {
            Box::new([
                AtomicU32::new(target),
                AtomicU32::new(target),
                AtomicU32::new(target),
                AtomicU32::new(target),
            ])
        });
        memo.as_ptr() as usize // Cast: a data address baked by compiled code
    }

    /// The published depth of `class_id` (its index in its own display), or
    /// `None` when it has none.
    pub(crate) fn depth_of(&self, class_id: u32) -> Option<u32> {
        self.inner.lock().depths.get(&class_id).copied()
    }

    /// Publish `chain` (the superclass chain root first, ending with
    /// `class_id`) and `interfaces` (its transitive interface set) as
    /// `class_id`'s display, unless one is already published. Its verdict
    /// word reads `0` (see [`Self::publish_with_verdict`]).
    pub(crate) fn publish(&self, class_id: u32, chain: &[u32], interfaces: &[u32]) {
        self.publish_with_verdict(class_id, chain, interfaces, false);
    }

    /// [`Self::publish`], with the display's trailing VERDICT word (JIT r20
    /// wave 17, lane pAlloc2): the display is `[len, chain.., n, ifaces..,
    /// verdict]`, the verdict at `4 * (len + n + 2)`. `1` (`hierarchy_only`)
    /// states that a compiled `instanceof` / `checkcast` against an instance
    /// of this class is answered by its hierarchy alone -- no instance-
    /// admitted shape (a `java/lang/reflect/Proxy` subclass or shim, an
    /// `AnnotationProxy`, an `Unmodifiable*` stamp), so a display MISS is the
    /// helper's `false` (`cratonvm_jit::ir_lower::typecheck_display_miss_bytes`,
    /// which screens the target-side admissions itself). `0` declines every
    /// miss to the helper, the behaviour before the word existed. Readers of
    /// the chain or the interface set never reach the word.
    pub(crate) fn publish_with_verdict(
        &self,
        class_id: u32,
        chain: &[u32],
        interfaces: &[u32],
        hierarchy_only: bool,
    ) {
        use std::sync::atomic::{AtomicU64, Ordering};
        if chain.last() != Some(&class_id) {
            return;
        }
        let (Ok(len), Ok(n_ifaces)) = (u32::try_from(chain.len()), u32::try_from(interfaces.len()))
        else {
            return;
        };
        let mut inner = self.inner.lock();
        if inner.depths.contains_key(&class_id) {
            return;
        }
        let idx = class_id as usize; // Cast: a class id indexes the table
        let cap = inner
            .tables
            .last()
            .map_or(0, |t| t[0].load(Ordering::Relaxed) as usize); // Cast: a capacity
        if idx >= cap {
            let new_cap = (idx + 1).max(cap.saturating_mul(2)).max(1024);
            let table: Box<[AtomicU64]> = (0..=new_cap).map(|_| AtomicU64::new(0)).collect();
            table[0].store(new_cap as u64, Ordering::Relaxed); // Cast: a capacity
            if let Some(old) = inner.tables.last() {
                for i in 1..=cap {
                    table[i].store(old[i].load(Ordering::Relaxed), Ordering::Relaxed);
                }
            }
            let addr = table.as_ptr() as usize; // Cast: the table's address
            inner.tables.push(table);
            // Release: a reader that sees the new table sees its contents.
            self.cell.store(addr, Ordering::Release);
        }
        let mut words: Vec<u32> = Vec::with_capacity(chain.len() + interfaces.len() + 3);
        words.push(len);
        words.extend_from_slice(chain);
        words.push(n_ifaces);
        words.extend_from_slice(interfaces);
        words.push(u32::from(hierarchy_only));
        let display: Box<[u32]> = words.into_boxed_slice();
        let ptr = display.as_ptr() as u64; // Cast: the display's address
        inner.displays.push(display);
        inner.ids.push(class_id);
        inner.depths.insert(class_id, len - 1);
        if let Some(table) = inner.tables.last() {
            // Release: a reader that sees the entry sees the display.
            table[1 + idx].store(ptr, Ordering::Release);
        }
        // JIT round 21, lane dispatch: the class's type-check row, from the
        // same facts, once the rows exist (before that, their backfill reads
        // this display).
        if let Some(rows) = inner.rows.as_ref() {
            rows.fill(class_id, chain, interfaces, hierarchy_only);
        }
    }
}

/// nio21 wave 11, lane L5 (W9L2-2, page
/// `r21-varhandle-constant-handle-field-access-inline-FIXED-20261012.md`): the
/// constant-`VarHandle` plans of this VM, keyed by the `static final` field
/// that holds the handle, `(declaring class id, static field index)` -- the
/// coordinates a compiled `getstatic` names.
///
/// Filed by `jit::helpers::note_class_initialized_varhandles` on the mutator
/// that finished the class's `<clinit>` (`vh_const_file_plan`), read by the
/// compile through `jit_well_known_class_id` (`cratonvm_jit::ir_lower::
/// vh_const_plan` / `vh_const_ref_facts`). Rows are only ever INSERTED, and
/// never replaced (a `static final` is assigned once), so the address of a
/// reference row and of its class name stays valid for the VM's life -- the
/// property the compile relies on when it reads a row through its address.
pub(crate) struct VhConstPlans {
    rows: RwLock<FxHashMap<(u32, u32), VhConstPlanRow>>,
}

/// One filed plan: the `cratonvm_jit::ir_lower::VhConstPlan` word and, for a
/// reference field, its boxed value-admission row and the class name the row
/// points into.
struct VhConstPlanRow {
    word: i64,
    ref_row: Option<Box<cratonvm_jit::ir_lower::VhConstRefRow>>,
    _name: Option<Box<str>>,
}

impl VhConstPlans {
    pub(crate) fn new() -> Self {
        Self {
            rows: RwLock::new(FxHashMap::default()),
        }
    }

    /// File `word` (and, for a reference field, `(flags, declared class id,
    /// proving class name)`) under `key`, unless a row is already filed there.
    pub(crate) fn file(
        &self,
        key: (u32, u32),
        word: i64,
        ref_facts: Option<(u32, u32, Option<Box<str>>)>,
    ) {
        let mut rows = self.rows.write();
        if rows.contains_key(&key) {
            return;
        }
        let (ref_row, name) = match ref_facts {
            Some((flags, type_class_id, name)) => {
                // The boxed `str`'s payload never moves, so the row may point
                // into it for as long as this entry lives.
                let (name_ptr, name_len) = name
                    .as_deref()
                    .map_or((0, 0), |n| (n.as_ptr() as usize, n.len()));
                let row = cratonvm_jit::ir_lower::VhConstRefRow {
                    flags,
                    type_class_id,
                    name_ptr,
                    name_len,
                };
                (Some(Box::new(row)), name)
            }
            None => (None, None),
        };
        rows.insert(
            key,
            VhConstPlanRow {
                word,
                ref_row,
                _name: name,
            },
        );
    }

    /// The plan word filed under `key`, `0` when none (or while a filing holds
    /// the table: the compile then keeps the call).
    pub(crate) fn plan_word(&self, key: (u32, u32)) -> i64 {
        self.rows
            .try_read()
            .and_then(|rows| rows.get(&key).map(|row| row.word))
            .unwrap_or(0)
    }

    /// The address of the reference row filed under `key`, `0` when none.
    pub(crate) fn ref_row_addr(&self, key: (u32, u32)) -> i64 {
        self.rows
            .try_read()
            .and_then(|rows| {
                rows.get(&key).and_then(|row| {
                    row.ref_row.as_deref().map(|r| {
                        // Cast: a heap address handed across the C ABI as i64.
                        r as *const cratonvm_jit::ir_lower::VhConstRefRow as usize as i64
                    })
                })
            })
            .unwrap_or(0)
    }
}

/// nio21 wave 8, lane L2 (page
/// `r21-direct-helper-gates-are-process-statics-holding-per-vm-memos-FIXED-20261009.md`):
/// the per-VM state behind the thin direct-call helpers' policy gates.
///
/// # The gate memos
///
/// `sites[g]` memoises gate `g`'s triple in THIS VM's `NativeMethodRegistry`
/// (`g` is the ordinal `direct_helper_gates!` assigns in
/// `vm/src/jit/helpers.rs`). Each gate used to carry its memo in its own
/// `static`, so two VMs in one process shared it: correct, because the memo
/// word encodes the registry generation and that is banded per registry, but
/// every call from the second VM missed and paid the three-string hash the
/// memo exists to remove, and the shape is the one AGENTS.md forbids ("No
/// process globals for compatibility state"). The `compareAndExchange*` rows'
/// memos (`UnsafeInlineCells::cae_sites`) were the first in this shape.
///
/// # The marked-generation latch
///
/// `mark_direct_call_helper_natives_incomplete` marks the natives the thin
/// helpers bypass as "invocation count incomplete", once per registry
/// generation. Its latch was three process statics naming the LAST VM marked,
/// so two VMs compiling in turn re-marked on every compile. Here it is one
/// word: `0` never marked, else `1 << 32 | generation`.
///
/// # The `Matcher` leaf memos
///
/// `matcher_leaf_sites[i]` memoises the `java/util/regex/Matcher` triple at
/// `jit::helpers::matcher_site_index` `i` for `admit_matcher_leaf_native`.
/// They were a process `static` array (`MATCHER_LEAF_SITES`), the same shape
/// as the gate memos.
pub struct DirectHelperGateSites {
    sites: [cratonvm_native_api::NativeCallSite; crate::jit::helpers::DIRECT_HELPER_GATE_COUNT],
    matcher_leaf_sites:
        [cratonvm_native_api::NativeCallSite; crate::jit::helpers::MATCHER_LEAF_SITE_COUNT],
    helper_natives_marked: std::sync::atomic::AtomicU64,
}

impl Default for DirectHelperGateSites {
    fn default() -> Self {
        Self::new()
    }
}

impl DirectHelperGateSites {
    /// Every memo empty, the latch unset.
    pub(crate) fn new() -> Self {
        Self {
            sites: std::array::from_fn(|_| cratonvm_native_api::NativeCallSite::new()),
            matcher_leaf_sites: std::array::from_fn(|_| cratonvm_native_api::NativeCallSite::new()),
            helper_natives_marked: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Gate `ordinal`'s memo in this VM.
    #[inline]
    pub(crate) fn site(&self, ordinal: usize) -> &cratonvm_native_api::NativeCallSite {
        &self.sites[ordinal]
    }

    /// The `Matcher` leaf's memo for `matcher_site_index` `index` in this VM.
    #[inline]
    pub(crate) fn matcher_leaf_site(&self, index: usize) -> &cratonvm_native_api::NativeCallSite {
        &self.matcher_leaf_sites[index]
    }

    /// Has this VM's registry already been marked at `generation`?
    #[inline]
    pub(crate) fn helper_natives_marked_at(&self, generation: u32) -> bool {
        self.helper_natives_marked
            .load(std::sync::atomic::Ordering::Acquire)
            == ((1u64 << 32) | u64::from(generation))
    }

    /// Record that the marks for `generation` are in place. Stored after the
    /// marks, with `Release`, so a reader that sees it sees them.
    #[inline]
    pub(crate) fn note_helper_natives_marked(&self, generation: u32) {
        self.helper_natives_marked.store(
            (1u64 << 32) | u64::from(generation),
            std::sync::atomic::Ordering::Release,
        );
    }
}

/// nio21 wave 7, lane L3 (W6-L3-1, W6-L3-2): the per-VM state behind the
/// compiled `Unsafe` RMW prefixes' instance-field arm and their
/// `compareAndExchange*` rows.
///
/// # The cell table
///
/// `1 + cratonvm_jit::direct_helpers::UNSAFE_FIELD_CELL_COUNT` words at a
/// stable address (a boxed slice, never resized), baked by every compile that
/// has a field arm (`DirectHelperTable::unsafe_field_cells`, through
/// [`Self::base_addr_for_compile`]):
///
/// * word 0: the process layout-replacement epoch
///   (`cratonvm_types::layout_replace_epoch`) every cell below was published
///   under. The compiled screen compares it with the live epoch and declines
///   on a mismatch, so a cell can never name a displacement from a layout its
///   class no longer has.
/// * words `1..`: `cratonvm_jit::direct_helpers::unsafe_field_cell_word`s,
///   `class_id << 32 | (slot << 1 | wide) << 16 | disp`, direct-mapped by
///   `unsafe_field_cell_index`. `0` is an empty cell.
///
/// The only writer is [`Self::note_compact_field`], called from
/// `vm_exec::hw_atomic_addr`'s COMPACT arm for an `int` / `long` field, i.e.
/// exactly where the natives (and the thin CAS helper) are about to do ONE
/// hardware atomic on `object + disp`. A compiled `LOCK CMPXCHG` / `LOCK XADD`
/// / `XCHG` on the same bytes is therefore in their atomicity domain, and a
/// field the VM serves any other way (a legacy cell, a mismatched descriptor,
/// a narrow storage) is never published, so its sites keep their call.
///
/// Readers are compiled code only, which takes no lock: stores and resets are
/// serialised by `publish` and ordered so a reader that sees a reset's epoch
/// also sees its cleared cells (the epoch is stored last, with `Release`; x86
/// loads are not reordered with older loads, and the field arm is x86-64
/// only). A collision keeps the FIRST cell: a loser's sites keep their call
/// rather than the two fields' natives fighting over the slot under the lock.
///
/// # The `compareAndExchange*` policy memos
///
/// `cae_sites[i]` memoises the registry resolution of
/// `cratonvm_jit::direct_helpers::UnsafeInlineRmw::CAE_GATE_BITS[i]`, which
/// the VM asks at compile time to fill admission bits 16 and 17. These two
/// rows have no thin helper and so no `DirectHelperGate`; the sixteen older
/// rows' memos live per VM too since nio21 wave 8 L2, in
/// [`DirectHelperGateSites`] at their gates' ordinals.
///
/// # The reference cells (JIT round 20 wave 12, lane qUnsafe, AT-4)
///
/// Words `1 + COUNT ..= 2 * COUNT` are a second, identically keyed cell
/// array for COMPACT REFERENCE fields (`unsafe_field_cell_word(class, slot,
/// true, disp)` at index `unsafe_field_cell_index(class, slot, true)`),
/// published by [`Self::note_compact_ref_field`] from `hw_atomic_addr`'s
/// compact arm for `HwAtomicKind::Reference` -- where
/// `compare_and_swap_field_shared` is about to do one hardware
/// `compare_exchange` on the 8-byte pointer -- on a Generational or G1 heap
/// only, and only once a compile baked the table with
/// `CRATONVM_JIT_UNSAFE_QU_REF_CAS_FIELD` on ([`Self::arm_ref_cells`]). The
/// compiled `compareAndSetReference` field arm reads them
/// (`cratonvm_jit::direct_helpers::unsafe_inline_ref_field_cell_bytes`). They
/// share word 0, so a reset clears both halves; an int/long screen never
/// reads past word `COUNT`.
///
/// # The sub-word cells (JIT round 21, lane atomics, UNSAFE21-2)
///
/// Words `1 + 2 * COUNT..=3 * COUNT`: a third identically keyed cell array for
/// COMPACT `boolean` / `byte` (`unsafe_field_cell_word(class, slot, false,
/// disp)`) and `short` / `char` (`.., true, ..`) fields, published by
/// [`Self::note_compact_subword_field`] from `vm_exec::hw_cas_subword`'s
/// compact arm (one hardware `LOCK CMPXCHG` of the field's width), read by the
/// compiled sub-word CAS prefixes
/// (`cratonvm_jit::direct_helpers::unsafe_inline_subword_field_cell_bytes`).
/// Same epoch word, reset and two-way rule as the other halves; armed only by
/// a compile with `CRATONVM_JIT_UNSAFE_SUBWORD_RMW_INLINE` on.
///
/// # The static entries (JIT round 21, lane atomics, STATICCAS21-2)
///
/// After the sub-word cells, `cratonvm_jit::direct_helpers::UNSAFE_STATIC_CELL_COUNT`
/// four-word entries `[key, mirror, base cell, class_id << 32 | field_index *
/// 16]` for the compiled RMW prefixes' static arm
/// (`cratonvm_jit::direct_helpers::unsafe_inline_static_cell_bytes`, whose
/// section comment states the atomicity and base-proof argument), published
/// by [`Self::note_static_target`] from the natives' mirror-base whole-static
/// CAS. Write-once (first owner keeps the entry, as for the cells) and NOT
/// cleared by a layout-epoch reset: a static's storage is not an instance
/// layout. The mirror word is weak (ATOMICS21-5): the collector re-keys it
/// ([`Self::remap_static_mirrors`]) and an unload withdraws the entry
/// ([`Self::forget_static_class`]), as for the compiled `getClass()` words.
pub struct UnsafeInlineCells {
    /// Word 0 the epoch, words `1..=COUNT` the int/long cells, words
    /// `1 + COUNT..=2 * COUNT` the reference cells, words
    /// `1 + 2 * COUNT..=3 * COUNT` the sub-word cells (see the type doc), then
    /// the static entries.
    words: Box<[std::sync::atomic::AtomicU64]>,
    /// Serialises table resets and cell stores. Never taken by a reader, nor
    /// on the publisher's fast path (a cell already published, or owned by a
    /// colliding field).
    publish: parking_lot::Mutex<()>,
    /// Set once a compile baked the table's address; until then the
    /// publisher returns after one relaxed load.
    armed: std::sync::atomic::AtomicBool,
    /// The `compareAndExchange{Int,Long}` rows' policy memos (see the type doc).
    pub(crate) cae_sites: [cratonvm_native_api::NativeCallSite; 2],
    /// JIT round 20 wave 12, lane qUnsafe (AT-4): set once a compile baked
    /// the table with the reference-CAS field arm on; until then
    /// [`Self::ref_epoch_if_armed`] answers `None` and no reference cell is
    /// published (the `=0` arm publishes none, exactly as before).
    ref_armed: std::sync::atomic::AtomicBool,
    /// JIT round 20 wave 12, lane qUnsafe (L2-2): the policy memos of
    /// `cratonvm_jit::direct_helpers::UNSAFE_QU_EXTRA_GATE_BITS` (the
    /// `boolean` / `float` / `double` accessor natives, which have no thin
    /// helper), asked at compile time like [`Self::cae_sites`].
    pub(crate) qu_extra_sites: [cratonvm_native_api::NativeCallSite; 6],
    /// JIT round 21 wave 1, lane updaters
    /// (`CRATONVM_JIT_UNSAFE_VOLATILE_PUBLISHES_FIELD_CELL`, opt-in): set by a
    /// compile that baked the table with that switch on (read there, once per
    /// compile, so the thin helpers never read the environment). Then the
    /// volatile get / put thin helpers publish the int/long cell of the field
    /// they serve ([`Self::volatile_publish_armed`]), so a field only ever
    /// read or written -- never CASed -- gets the inline get / put arm too.
    volatile_publish: std::sync::atomic::AtomicBool,
    /// JIT round 21 wave 1, lane updaters (A/B fix): the REFERENCE half of
    /// that publication, armed only when the same compile also had
    /// `CRATONVM_JIT_UNSAFE_REF_VOLATILE_FIELD` on -- the only arm that reads
    /// a reference cell a get / put published. Without it a reference
    /// get / put thin call paid a publication attempt for no reader
    /// (`R21UpdaterRows` rfu-set 117 -> 365 ns).
    ref_volatile_publish: std::sync::atomic::AtomicBool,
    /// JIT round 21 wave 1, lane updaters (A/B fix): the BACKSTOP for refused
    /// publication attempts whose refusal could not be recorded in the key's
    /// own cell (`note_field_refusal`: a stale table, a slot past the cell
    /// key, an epoch that moved during the attempt) and for TRANSIENT
    /// refusals, which must not settle the key (r21 wave 6, lane updaters:
    /// `vm_exec::note_unsafe_volatile_field_cell`). Since lane fuzz2's page
    /// (half 2) a refusal settles its own key only; this budget is spent only
    /// by those, and at `0` the helpers stop trying altogether.
    publish_budget: std::sync::atomic::AtomicU32,
    /// JIT round 23 wave 9 (lane W9-L2, CF-W9-1;
    /// `cratonvm_jit::direct_helpers::unsafe_field_cell_two_way_enabled`,
    /// default ON): a key whose first cell another field owns is published at
    /// its second cell (`unsafe_field_cell_index2`), which the two-way
    /// compiled screen probes after a first-cell miss. Read once per VM.
    two_way: bool,
    /// JIT round 21, lane atomics (STATICCAS21-2): set once a compile baked
    /// the table with `CRATONVM_JIT_UNSAFE_STATIC_RMW_INLINE` on; until then
    /// [`Self::static_publish_wanted`] answers `false` after one relaxed load.
    static_armed: std::sync::atomic::AtomicBool,
    /// JIT round 21 proposal ATOMICS21-5: set by the first static entry, so
    /// the collector's per-pause re-key and the unload withdrawal cost one
    /// relaxed load in a VM that never published one.
    static_published: std::sync::atomic::AtomicBool,
    /// JIT round 21, lane atomics (UNSAFE21-2): set once a compile baked the
    /// table with `CRATONVM_JIT_UNSAFE_SUBWORD_RMW_INLINE` on; until then
    /// [`Self::subword_epoch_if_armed`] answers `None` and no sub-word cell is
    /// published.
    subword_armed: std::sync::atomic::AtomicBool,
    /// JIT round 21, lane atomics (UNSAFE21-2): the policy memos of
    /// `cratonvm_jit::direct_helpers::UnsafeSubwordRmw::GATE_BITS` (the
    /// sub-word CAS / compareAndExchange natives, which have no thin helper),
    /// asked at compile time like [`Self::cae_sites`].
    pub(crate) subword_sites: [cratonvm_native_api::NativeCallSite; 4],
}

impl Default for UnsafeInlineCells {
    fn default() -> Self {
        Self::new()
    }
}

impl UnsafeInlineCells {
    /// An empty, unarmed table whose epoch word is the current
    /// layout-replacement epoch.
    pub(crate) fn new() -> Self {
        // JIT round 20 wave 12, lane qUnsafe (AT-4): plus the reference half;
        // JIT round 21, lane atomics: plus the sub-word cells (UNSAFE21-2) and
        // the static entries (STATICCAS21-2), which start at
        // `UNSAFE_STATIC_CELL_BASE`.
        let n = cratonvm_jit::direct_helpers::UNSAFE_STATIC_CELL_BASE / 8
            + cratonvm_jit::direct_helpers::UNSAFE_STATIC_CELL_COUNT
                * cratonvm_jit::direct_helpers::UNSAFE_STATIC_CELL_WORDS;
        let words: Box<[std::sync::atomic::AtomicU64]> = (0..n)
            .map(|_| std::sync::atomic::AtomicU64::new(0))
            .collect();
        words[0].store(
            u64::from(cratonvm_types::layout_replace_epoch()),
            std::sync::atomic::Ordering::Relaxed,
        );
        Self {
            words,
            publish: parking_lot::Mutex::new(()),
            armed: std::sync::atomic::AtomicBool::new(false),
            cae_sites: [
                cratonvm_native_api::NativeCallSite::new(),
                cratonvm_native_api::NativeCallSite::new(),
            ],
            ref_armed: std::sync::atomic::AtomicBool::new(false),
            qu_extra_sites: std::array::from_fn(|_| cratonvm_native_api::NativeCallSite::new()),
            volatile_publish: std::sync::atomic::AtomicBool::new(false),
            ref_volatile_publish: std::sync::atomic::AtomicBool::new(false),
            publish_budget: std::sync::atomic::AtomicU32::new(1 << 16),
            two_way: cratonvm_jit::direct_helpers::unsafe_field_cell_two_way_enabled(),
            static_armed: std::sync::atomic::AtomicBool::new(false),
            static_published: std::sync::atomic::AtomicBool::new(false),
            subword_armed: std::sync::atomic::AtomicBool::new(false),
            subword_sites: std::array::from_fn(|_| cratonvm_native_api::NativeCallSite::new()),
        }
    }

    /// The word index of `offset`'s static entry (its key word).
    #[inline]
    fn static_entry_word(offset: usize) -> usize {
        Self::static_entry_index(cratonvm_jit::direct_helpers::unsafe_static_cell_index(
            offset,
        ))
    }

    /// JIT round 21, lane atomics (UNSAFE21-2): [`Self::epoch_if_armed`] for
    /// the sub-word half.
    #[inline]
    pub(crate) fn subword_epoch_if_armed(&self) -> Option<u32> {
        self.subword_armed
            .load(std::sync::atomic::Ordering::Relaxed)
            .then(cratonvm_types::layout_replace_epoch)
    }

    /// JIT round 21, lane atomics (UNSAFE21-2): publish `(class_id, slot,
    /// width) -> disp` for a COMPACT `boolean` / `byte` (`short = false`) or
    /// `short` / `char` field the caller is about to compare-and-swap with one
    /// hardware `LOCK CMPXCHG` of that width at `object + disp`, resolved
    /// under `epoch` (from [`Self::subword_epoch_if_armed`], read before the
    /// resolution), into the sub-word half. Same word, index, first-owner,
    /// two-way and epoch rules as [`Self::note_compact_field`].
    #[inline]
    pub(crate) fn note_compact_subword_field(
        &self,
        class_id: u32,
        slot: usize,
        short: bool,
        disp: usize,
        epoch: u32,
    ) {
        let Some(word) =
            cratonvm_jit::direct_helpers::unsafe_field_cell_word(class_id, slot, short, disp)
        else {
            return;
        };
        let half = 1 + 2 * cratonvm_jit::direct_helpers::UNSAFE_FIELD_CELL_COUNT;
        self.publish_cell(
            half + cratonvm_jit::direct_helpers::unsafe_field_cell_index(class_id, slot, short),
            half + cratonvm_jit::direct_helpers::unsafe_field_cell_index2(class_id, slot, short),
            word,
            epoch,
        );
    }

    /// JIT round 21, lane atomics (STATICCAS21-2): should the caller resolve
    /// and publish `offset`'s static entry? `false` -- after one relaxed load
    /// -- until a compile armed the static half, and once `offset`'s entry
    /// word is taken (by `offset` or by a colliding offset, which keeps it).
    #[inline]
    pub(crate) fn static_publish_wanted(&self, offset: usize) -> bool {
        use std::sync::atomic::Ordering;
        self.static_armed.load(Ordering::Relaxed)
            && (cratonvm_jit::direct_helpers::UNSAFE_STATIC_OFFSET_FIRST
                ..=cratonvm_jit::direct_helpers::UNSAFE_STATIC_OFFSET_LAST)
                .contains(&offset)
            && self.words[Self::static_entry_word(offset)].load(Ordering::Relaxed) == 0
    }

    /// JIT round 21, lane atomics (STATICCAS21-2): publish the static entry
    /// for the `Unsafe` offset `offset` of `class_id`'s static declared `J`
    /// (`wide`) or `I`: `mirror` the class's mirror's address, read from
    /// `class_mirrors` under the read lock the caller still holds (so the
    /// collector's re-key, [`Self::remap_static_mirrors`], cannot interleave),
    /// `base_cell` the statics index's base-pointer cell of the class,
    /// `field_index` the static's index in its block. Nothing is published for
    /// an offset outside the static band, a zero address or a taken entry
    /// (first owner wins). The three payload words are stored before the key,
    /// which is stored with `Release`, so a reader that matches the key reads
    /// a complete entry.
    pub(crate) fn note_static_target(
        &self,
        offset: usize,
        wide: bool,
        mirror: usize,
        base_cell: usize,
        class_id: u32,
        field_index: usize,
    ) {
        use std::sync::atomic::Ordering;
        if !(cratonvm_jit::direct_helpers::UNSAFE_STATIC_OFFSET_FIRST
            ..=cratonvm_jit::direct_helpers::UNSAFE_STATIC_OFFSET_LAST)
            .contains(&offset)
            || mirror == 0
            || base_cell == 0
        {
            return;
        }
        // Cast: a positive constant. A sanity bound: no statics block is
        // anywhere near 2 GiB.
        let Some(disp) = field_index
            .checked_mul(16)
            .filter(|d| *d <= i32::MAX as usize)
        else {
            return;
        };
        let i = Self::static_entry_word(offset);
        let _guard = self.publish.lock();
        if self.words[i].load(Ordering::Relaxed) != 0 {
            return;
        }
        // Casts: two addresses and a displacement below 2^31, as `u64` words.
        self.words[i + 1].store(mirror as u64, Ordering::Relaxed);
        self.words[i + 2].store(base_cell as u64, Ordering::Relaxed);
        self.words[i + 3].store((u64::from(class_id) << 32) | disp as u64, Ordering::Relaxed);
        self.static_published
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.words[i].store(
            cratonvm_jit::direct_helpers::unsafe_static_cell_key(offset, wide),
            Ordering::Release,
        );
    }

    /// JIT round 21 proposal ATOMICS21-5: the collector moved class mirrors
    /// (`memory::gc::update_all_roots` step 6, under the `class_mirrors` WRITE
    /// lock with every mutator parked). Every static entry whose mirror word
    /// `moved` maps to a new address follows it -- the entries' mirror words
    /// are weak, like the compiled `getClass()` words re-keyed in the same
    /// step, never roots. One relaxed load when no entry was ever published.
    pub(crate) fn remap_static_mirrors(&self, moved: &dyn Fn(usize) -> Option<usize>) {
        use std::sync::atomic::Ordering;
        if !self.static_published.load(Ordering::Relaxed) {
            return;
        }
        let _guard = self.publish.lock();
        for e in 0..cratonvm_jit::direct_helpers::UNSAFE_STATIC_CELL_COUNT {
            let i = Self::static_entry_index(e);
            if self.words[i].load(Ordering::Relaxed) == 0 {
                continue;
            }
            // Cast: the word holds an address.
            let old = self.words[i + 1].load(Ordering::Relaxed) as usize;
            if let Some(new) = moved(old) {
                self.words[i + 1].store(new as u64, Ordering::Release);
            }
        }
    }

    /// JIT round 21 proposal ATOMICS21-5: `class_id` unloads (its mirror is
    /// removed from `class_mirrors`, under that WRITE lock): withdraw every
    /// static entry naming it, key first, so the entry is free for a later
    /// offset and nothing compiled can match the dead mirror or the class's
    /// statics cell (which a re-used class id would own).
    pub(crate) fn forget_static_class(&self, class_id: u32) {
        use std::sync::atomic::Ordering;
        if !self.static_published.load(Ordering::Relaxed) {
            return;
        }
        let _guard = self.publish.lock();
        for e in 0..cratonvm_jit::direct_helpers::UNSAFE_STATIC_CELL_COUNT {
            let i = Self::static_entry_index(e);
            if self.words[i].load(Ordering::Relaxed) == 0
                || (self.words[i + 3].load(Ordering::Relaxed) >> 32) as u32 != class_id
            {
                continue;
            }
            self.words[i].store(0, Ordering::Release);
            for k in 1..cratonvm_jit::direct_helpers::UNSAFE_STATIC_CELL_WORDS {
                self.words[i + k].store(0, Ordering::Relaxed);
            }
        }
    }

    /// The key word index of static entry number `e`.
    #[inline]
    fn static_entry_index(e: usize) -> usize {
        cratonvm_jit::direct_helpers::UNSAFE_STATIC_CELL_BASE / 8
            + cratonvm_jit::direct_helpers::UNSAFE_STATIC_CELL_WORDS * e
    }

    /// The static entry of `offset` (key, mirror slot, base cell,
    /// displacement), for tests.
    #[cfg(test)]
    pub(crate) fn static_snapshot(&self, offset: usize) -> [u64; 4] {
        use std::sync::atomic::Ordering;
        let i = Self::static_entry_word(offset);
        std::array::from_fn(|k| self.words[i + k].load(Ordering::Relaxed))
    }

    /// JIT round 21 wave 1, lane updaters: may the reference get / put thin
    /// helpers publish a reference cell (see the field doc)? One relaxed load.
    #[inline]
    pub(crate) fn ref_volatile_publish_armed(&self) -> bool {
        self.ref_volatile_publish
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// JIT round 21 wave 1, lane updaters: is there nothing left to publish
    /// for `(class_id, slot, wide)` in the int/long half (`reference ==
    /// false`) or the reference half? Under the table's CURRENT layout epoch
    /// (word 0 equals `layout_replace_epoch()`): `true` when the cell is
    /// non-zero -- this field's published cell, its recorded refusal
    /// ([`Self::note_field_refusal`]), or a colliding owner's, which the
    /// publisher keeps. A colliding key's REFUSAL word does not settle this
    /// key (r21 wave 6, lane updaters, review item 6): a refusal owns nothing
    /// (`publish_slow` overwrites it), so before, one unfit field left every
    /// field sharing its index without the get / put arm unless a CAS
    /// published it. Under a stale epoch nothing is settled, so the caller
    /// reaches `hw_atomic_addr`, whose publication resets the table (lane
    /// fuzz2, page `r21-fz-updater-settled-screen-ignores-the-table-epoch-20261019.md`,
    /// half 1: a stale word read "settled" left the field arms dark after a
    /// layout replacement). Always `true` for a slot past the cell key, and
    /// when the backstop budget is spent. Two or three loads.
    #[inline]
    pub(crate) fn field_cell_settled(
        &self,
        class_id: u32,
        slot: usize,
        wide: bool,
        reference: bool,
    ) -> bool {
        use std::sync::atomic::Ordering;
        if slot > cratonvm_jit::direct_helpers::UNSAFE_FIELD_CELL_MAX_SLOT
            || self.publish_budget.load(Ordering::Relaxed) == 0
        {
            return true;
        }
        // Cast: the epoch word holds a `u32`.
        let fresh = self.words[0].load(Ordering::Acquire) as u32
            == cratonvm_types::layout_replace_epoch();
        if !fresh {
            return false;
        }
        let cur = self.words[Self::cell_index(class_id, slot, wide, reference)].load(Ordering::Relaxed);
        // A published cell (`disp != 0`, this key's or a colliding owner's),
        // or THIS key's refusal word.
        cur != 0 && (cur & 0xFFFF != 0 || Self::refusal_word(class_id, slot, wide) == Some(cur))
    }

    /// The word index of `(class_id, slot, wide)` in the int/long or the
    /// reference half (callers pass a slot within the cell key).
    #[inline]
    fn cell_index(class_id: u32, slot: usize, wide: bool, reference: bool) -> usize {
        let half = if reference {
            cratonvm_jit::direct_helpers::UNSAFE_FIELD_CELL_COUNT
        } else {
            0
        };
        1 + half + cratonvm_jit::direct_helpers::unsafe_field_cell_index(class_id, slot, wide)
    }

    /// The REFUSAL word of `(class_id, slot, wide)`: its cell word with
    /// displacement `0`. The compiled screens decline on it (class and key
    /// match, then `disp == 0` is "empty"), and the publishers overwrite it
    /// ([`Self::publish_slow`]): a refusal is a note for the thin helpers'
    /// publication attempts only, never a claim on the cell.
    fn refusal_word(class_id: u32, slot: usize, wide: bool) -> Option<u64> {
        cratonvm_jit::direct_helpers::unsafe_field_cell_word(class_id, slot, wide, 1)
            .map(|w| w & !0xFFFF)
    }

    /// JIT round 21 wave 1, lane updaters (lane fuzz2's page, half 2): record
    /// that the thin helpers' attempt to publish `(class_id, slot, wide)`
    /// found nothing to publish (`hw_atomic_addr` refused the field), so this
    /// KEY reads settled from now on -- per key, not per VM: one hot unfit
    /// field no longer spends the budget every later field needed. Stored only
    /// into an empty cell under the current epoch. A cell that is already
    /// non-zero under the current epoch (this key's refusal or cell, or a
    /// colliding owner, all of which read settled) needs nothing more and
    /// spends nothing -- a repeated refusal of one key must not drain the
    /// budget (the cvm-r21w3 test failure). Only an unrecordable refusal (a
    /// stale table, a slot past the cell key) spends one backstop attempt.
    ///
    /// r21 wave 6, lane updaters (review item 6): callers pass only a
    /// refusal that is fixed for the class's lifetime or for its layout
    /// epoch (`vm_exec::note_unsafe_volatile_field_cell` classifies it), and
    /// `epoch` is the layout epoch read BEFORE the attempt: the refusal is
    /// stored only while the table's word 0 and the live epoch both still
    /// equal it, checked under `publish`, so a layout replacement (and the
    /// reset it causes) during the attempt can never leave a refusal
    /// computed against the old layout settling the key under the new one.
    /// A colliding key's refusal word is overwritten (it owns nothing, and
    /// no longer reads settled for this key): two colliding unfit fields
    /// each re-attempt when the other refused last, which costs those two
    /// keys a lookup per alternation and nothing anyone else needs.
    pub(crate) fn note_field_refusal(
        &self,
        class_id: u32,
        slot: usize,
        wide: bool,
        reference: bool,
        epoch: u32,
    ) {
        let recorded = slot <= cratonvm_jit::direct_helpers::UNSAFE_FIELD_CELL_MAX_SLOT
            && Self::refusal_word(class_id, slot, wide).is_some_and(|word| {
                self.store_refusal(Self::cell_index(class_id, slot, wide, reference), word, epoch)
            });
        if !recorded {
            self.spend_publish_attempt();
        }
    }

    /// [`Self::note_field_refusal`]'s store: `true` when cell `i` now settles
    /// the refused key (its refusal word, or a published cell), `false` when
    /// the table or the live epoch is not `epoch` (unrecordable).
    fn store_refusal(&self, i: usize, word: u64, epoch: u32) -> bool {
        use std::sync::atomic::Ordering;
        // Cast: the epoch word holds a `u32`.
        if self.words[0].load(Ordering::Acquire) as u32 != epoch {
            return false;
        }
        let cur = self.words[i].load(Ordering::Relaxed);
        // Fast path, no lock: already this key's refusal, or a published
        // cell (this key's or a colliding owner's), settled either way -- a
        // repeated refusal of one key takes no lock and spends nothing.
        if cur == word || cur & 0xFFFF != 0 {
            return true;
        }
        // Empty, or a colliding key's refusal: under the lock `publish_slow`
        // resets the table with, so no reset can interleave.
        let _guard = self.publish.lock();
        if cratonvm_types::layout_replace_epoch() != epoch
            || self.words[0].load(Ordering::Relaxed) as u32 != epoch
        {
            return false;
        }
        let cur = self.words[i].load(Ordering::Relaxed);
        if cur & 0xFFFF == 0 {
            self.words[i].store(word, Ordering::Release);
        }
        true
    }

    /// Spend one attempt of the backstop budget (see
    /// [`Self::note_field_refusal`]); saturates at `0`.
    #[inline]
    pub(crate) fn spend_publish_attempt(&self) {
        use std::sync::atomic::Ordering;
        let _ = self
            .publish_budget
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
    }

    /// JIT round 21 wave 1, lane updaters: may the volatile get / put thin
    /// helpers publish a cell (see the field doc)? One relaxed load.
    #[inline]
    pub(crate) fn volatile_publish_armed(&self) -> bool {
        self.volatile_publish
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// JIT round 20 wave 12, lane qUnsafe (AT-4): a compile that baked this
    /// table (`base_addr_for_compile` answered non-zero) with the JIT's
    /// `CRATONVM_JIT_UNSAFE_QU_REF_CAS_FIELD` on arms the reference half's
    /// publisher for the VM's life.
    pub(crate) fn arm_ref_cells(&self) {
        self.ref_armed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// [`Self::epoch_if_armed`] for the reference half.
    #[inline]
    pub(crate) fn ref_epoch_if_armed(&self) -> Option<u32> {
        self.ref_armed
            .load(std::sync::atomic::Ordering::Relaxed)
            .then(cratonvm_types::layout_replace_epoch)
    }

    /// Publish `(class_id, slot) -> disp` for a COMPACT 8-byte REFERENCE
    /// field the caller is about to compare-and-swap with one hardware atomic
    /// at `object + disp`, resolved under `epoch` (from
    /// [`Self::ref_epoch_if_armed`], read before the resolution), into the
    /// reference half. Same word, index, first-owner and epoch rules as
    /// [`Self::note_compact_field`].
    #[inline]
    pub(crate) fn note_compact_ref_field(
        &self,
        class_id: u32,
        slot: usize,
        disp: usize,
        epoch: u32,
    ) {
        let Some(word) =
            cratonvm_jit::direct_helpers::unsafe_field_cell_word(class_id, slot, true, disp)
        else {
            return;
        };
        let half = 1 + cratonvm_jit::direct_helpers::UNSAFE_FIELD_CELL_COUNT;
        self.publish_cell(
            half + cratonvm_jit::direct_helpers::unsafe_field_cell_index(class_id, slot, true),
            half + cratonvm_jit::direct_helpers::unsafe_field_cell_index2(class_id, slot, true),
            word,
            epoch,
        );
    }

    /// CF-W9-1: publish `word` at word `first` (its key's first cell), or, on
    /// a two-way table whose `first` another field owns, at word `second`
    /// (its key's second cell). A cell that already holds `word` publishes
    /// nothing; one another field owns is kept (first owner wins in each
    /// cell). With the table one-way, `second` is never touched: exactly the
    /// pre-wave-9 rule.
    #[inline]
    fn publish_cell(&self, first: usize, second: usize, word: u64, epoch: u32) {
        use std::sync::atomic::Ordering;
        // A refusal word (`disp == 0`, lane updaters) owns nothing.
        let owned = |cur: u64| cur != 0 && cur & 0xFFFF != 0;
        // Cast: the epoch word holds a `u32`.
        if self.words[0].load(Ordering::Acquire) as u32 == epoch {
            let cur = self.words[first].load(Ordering::Relaxed);
            if cur == word {
                return;
            }
            if owned(cur) {
                if !self.two_way || second == first {
                    return;
                }
                let cur2 = self.words[second].load(Ordering::Relaxed);
                if cur2 == word || owned(cur2) {
                    return;
                }
                self.publish_slow(second, word, epoch);
                return;
            }
        }
        self.publish_slow(first, word, epoch);
    }

    /// The table's address for `DirectHelperTable::unsafe_field_cells`, or
    /// `0` (no field arm in this compile): not x86-64, the JIT's
    /// `CRATONVM_JIT_UNSAFE_FIELD_RMW_INLINE=0`, or `CRATONVM_DBG_AQS_TRACE`
    /// (whose per-CAS ledger a compiled field CAS would skip; the thin CAS
    /// helper declines under it for the same reason). A non-zero answer arms
    /// the publisher for the VM's life.
    pub(crate) fn base_addr_for_compile(&self) -> usize {
        if !cfg!(target_arch = "x86_64")
            || !cratonvm_jit::direct_helpers::unsafe_field_rmw_inline_enabled()
            || cratonvm_native_builtins::util_concurrent_ext::aqs_trace_enabled()
        {
            return 0;
        }
        self.armed.store(true, std::sync::atomic::Ordering::Relaxed);
        // JIT round 21 wave 1, lane updaters: monotone like `armed` (a later
        // compile with the switch unset does not disarm what one armed).
        // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
        if cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JIT_UNSAFE_VOLATILE_PUBLISHES_FIELD_CELL",
        ) {
            self.volatile_publish
                .store(true, std::sync::atomic::Ordering::Relaxed);
            if cratonvm_jit::direct_helpers::unsafe_ref_volatile_field_enabled() {
                self.ref_volatile_publish
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        // JIT round 21, lane atomics: this compile's RMW prefixes may carry
        // the static arm (STATICCAS21-2) and its sub-word CAS sites the
        // sub-word arm (UNSAFE21-2), so the natives start publishing.
        if cratonvm_jit::direct_helpers::unsafe_static_rmw_inline_enabled() {
            self.static_armed
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if cratonvm_jit::direct_helpers::unsafe_subword_rmw_inline_enabled() {
            self.subword_armed
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.words.as_ptr() as usize
    }

    /// JIT round 21 wave 1, lane updaters (UPDATERS21-2): would
    /// [`Self::base_addr_for_compile`] answer non-zero? Its own tests, with no
    /// side effect (the publisher stays as it is).
    pub(crate) fn would_bake_for_compile(&self) -> bool {
        cfg!(target_arch = "x86_64")
            && cratonvm_jit::direct_helpers::unsafe_field_rmw_inline_enabled()
            && !cratonvm_native_builtins::util_concurrent_ext::aqs_trace_enabled()
    }

    /// The layout-replacement epoch to publish under, read BEFORE the caller
    /// resolves a displacement, or `None` when no compile has baked the table
    /// (the publisher's whole cost then: this one relaxed load).
    #[inline]
    pub(crate) fn epoch_if_armed(&self) -> Option<u32> {
        self.armed
            .load(std::sync::atomic::Ordering::Relaxed)
            .then(cratonvm_types::layout_replace_epoch)
    }

    /// Publish `(class_id, slot, width) -> disp` for a COMPACT `int` (`wide =
    /// false`) or `long` field the caller is about to update with one hardware
    /// atomic at `object + disp`, resolved under layout epoch `epoch` (from
    /// [`Self::epoch_if_armed`], read before the resolution). A shape that
    /// does not fit a cell word publishes nothing.
    #[inline]
    pub(crate) fn note_compact_field(
        &self,
        class_id: u32,
        slot: usize,
        wide: bool,
        disp: usize,
        epoch: u32,
    ) {
        let Some(word) =
            cratonvm_jit::direct_helpers::unsafe_field_cell_word(class_id, slot, wide, disp)
        else {
            return;
        };
        // CF-W9-1: the first cell, or (two-way) the second one when another
        // field owns the first.
        self.publish_cell(
            1 + cratonvm_jit::direct_helpers::unsafe_field_cell_index(class_id, slot, wide),
            1 + cratonvm_jit::direct_helpers::unsafe_field_cell_index2(class_id, slot, wide),
            word,
            epoch,
        );
    }

    #[cold]
    fn publish_slow(&self, i: usize, word: u64, epoch: u32) {
        use std::sync::atomic::Ordering;
        let _guard = self.publish.lock();
        // Re-checked under the lock: an epoch that moved since the caller
        // resolved `disp` may have replaced the layout it came from. The
        // epoch is monotone, so once a reset to a newer epoch has run, every
        // later store here sees that epoch and refuses an older `disp`.
        if cratonvm_types::layout_replace_epoch() != epoch {
            return;
        }
        if self.words[0].load(Ordering::Relaxed) as u32 != epoch {
            // Cast: the epoch word holds a `u32`. The int/long, reference and
            // sub-word cells only: the static entries (STATICCAS21-2) name no
            // instance layout and stay.
            let cells = 3 * cratonvm_jit::direct_helpers::UNSAFE_FIELD_CELL_COUNT;
            for cell in &self.words[1..=cells] {
                cell.store(0, Ordering::Relaxed);
            }
            // Last, with Release: a reader that sees the new epoch sees the
            // cleared cells.
            self.words[0].store(u64::from(epoch), Ordering::Release);
        }
        // A refusal word (`disp == 0`, lane updaters) is overwritten.
        let cur = self.words[i].load(Ordering::Relaxed);
        if cur == 0 || cur & 0xFFFF == 0 {
            self.words[i].store(word, Ordering::Release);
        }
    }

    /// Cell `i` (0-based, after the epoch word) and the epoch word, for tests.
    #[cfg(test)]
    pub(crate) fn snapshot(&self, i: usize) -> (u64, u64) {
        use std::sync::atomic::Ordering;
        (
            self.words[1 + i].load(Ordering::Relaxed),
            self.words[0].load(Ordering::Relaxed),
        )
    }

    /// Whether a compile has armed the publisher, for tests.
    #[cfg(test)]
    pub(crate) fn is_armed(&self) -> bool {
        self.armed.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod nio21_w7_unsafe_inline_cells_tests {
    use super::UnsafeInlineCells;
    use cratonvm_jit::direct_helpers::{unsafe_field_cell_index, unsafe_field_cell_word};

    /// Nothing is published until a compile arms the table; once armed, a
    /// cell is published once, a colliding field does not evict it, and a
    /// shape that does not fit a word publishes nothing.
    #[test]
    fn nio21_w7_cells_publish_once_armed_and_keep_the_first_owner() {
        let cells = UnsafeInlineCells::new();
        let epoch = cratonvm_types::layout_replace_epoch();
        assert!(cells.epoch_if_armed().is_none());
        if cells.base_addr_for_compile() == 0 {
            return; // not x86-64, or a kill switch is set
        }
        assert!(cells.is_armed());
        assert_eq!(cells.epoch_if_armed(), Some(epoch));
        let i = unsafe_field_cell_index(40, 3, false);
        cells.note_compact_field(40, 3, false, 12, epoch);
        let first = cells.snapshot(i);
        // A different (class, key) with the same index keeps the first owner.
        // `40 ^ 6 == 46 ^ 0`: class 46, slot 0, int.
        assert_eq!(unsafe_field_cell_index(46, 0, false), i);
        cells.note_compact_field(46, 0, false, 8, epoch);
        let after_collision = cells.snapshot(i).0;
        // A slot past the key: nothing.
        let j = unsafe_field_cell_index(41, 0x8000, false);
        cells.note_compact_field(41, 0x8000, false, 12, epoch);
        let unfit = cells.snapshot(j).0;
        // A stale epoch never publishes.
        let k = unsafe_field_cell_index(77, 1, true);
        cells.note_compact_field(77, 1, true, 16, epoch.wrapping_sub(1));
        let stale = cells.snapshot(k).0;
        // Another test in this process may replace a layout (a class unload
        // does) and move the epoch under us; then the refusals above were
        // the right answers for a different reason, and nothing is pinned.
        if cratonvm_types::layout_replace_epoch() != epoch {
            return;
        }
        let want = unsafe_field_cell_word(40, 3, false, 12).unwrap();
        assert_eq!(first, (want, u64::from(epoch)));
        assert_eq!(after_collision, want);
        assert_eq!(unfit, 0);
        assert_eq!(stale, 0);
    }

    /// JIT r23 wave 9 (lane W9-L2, CF-W9-1): on a two-way table a key whose
    /// first cell another field owns is published at its SECOND cell, and the
    /// first owner keeps its cell. One-way (`=0`) keeps the old rule: the
    /// loser publishes nothing. The pair is the one `R21CfChain` hit:
    /// `ConcurrentHashMap.sizeCtl` (145, 5) first, then `UniApply.status`
    /// (667, 0).
    #[test]
    fn cf_w9_a_colliding_key_takes_its_second_cell_on_a_two_way_table() {
        use cratonvm_jit::direct_helpers::{unsafe_field_cell_index2, UNSAFE_FIELD_CELL_COUNT};
        const KEY: &str = "CRATONVM_JIT_UNSAFE_FIELD_CELL_TWO_WAY";
        for two_way in [true, false] {
            let cells = cratonvm_types::flags::with_thread_overrides(
                &[(KEY, if two_way { None } else { Some("0") })],
                UnsafeInlineCells::new,
            );
            let epoch = cratonvm_types::layout_replace_epoch();
            if cells.base_addr_for_compile() == 0 {
                return; // not x86-64, or a kill switch is set
            }
            let first = unsafe_field_cell_index(667, 0, false);
            assert_eq!(first, unsafe_field_cell_index(145, 5, false));
            let second = unsafe_field_cell_index2(667, 0, false);
            assert_ne!(first, second);
            assert!(second < UNSAFE_FIELD_CELL_COUNT);
            cells.note_compact_field(145, 5, false, 28, epoch);
            cells.note_compact_field(667, 0, false, 8, epoch);
            // Published twice more: no duplicate, no eviction.
            cells.note_compact_field(667, 0, false, 8, epoch);
            if cratonvm_types::layout_replace_epoch() != epoch {
                return; // another test moved the epoch: nothing is pinned
            }
            let chm = unsafe_field_cell_word(145, 5, false, 28).unwrap();
            let uni = unsafe_field_cell_word(667, 0, false, 8).unwrap();
            assert_eq!(cells.snapshot(first).0, chm, "the first owner keeps its cell");
            assert_eq!(
                cells.snapshot(second).0,
                if two_way { uni } else { 0 },
                "two_way={two_way}"
            );
        }
    }

    /// JIT round 20 wave 12, lane qUnsafe (AT-4): the reference half publishes
    /// only once armed, into its own words, never into the int/long half.
    #[test]
    fn r20_qu_reference_cells_publish_into_their_own_half_once_armed() {
        use cratonvm_jit::direct_helpers::UNSAFE_FIELD_CELL_COUNT;
        let cells = UnsafeInlineCells::new();
        let epoch = cratonvm_types::layout_replace_epoch();
        assert!(cells.ref_epoch_if_armed().is_none());
        let i = unsafe_field_cell_index(52, 2, true);
        // Baking the table alone does not arm the reference half.
        if cells.base_addr_for_compile() == 0 {
            return;
        }
        assert!(cells.ref_epoch_if_armed().is_none());
        if !cfg!(target_arch = "x86_64") {
            return;
        }
        cells.arm_ref_cells();
        assert_eq!(cells.ref_epoch_if_armed(), Some(epoch));
        cells.note_compact_ref_field(52, 2, 24, epoch);
        if cratonvm_types::layout_replace_epoch() != epoch {
            return;
        }
        assert_eq!(
            cells.snapshot(UNSAFE_FIELD_CELL_COUNT + i).0,
            unsafe_field_cell_word(52, 2, true, 24).unwrap()
        );
        assert_eq!(cells.snapshot(i).0, 0, "the int/long half is untouched");
    }

    /// JIT round 21, lane atomics (UNSAFE21-2): the sub-word half publishes
    /// only once armed, into its own words (never the int/long or reference
    /// halves), keyed with the "2-byte field" bit.
    #[test]
    fn jr21_atomics_subword_cells_publish_into_their_own_half_once_armed() {
        use cratonvm_jit::direct_helpers::{
            unsafe_subword_rmw_inline_enabled, UNSAFE_FIELD_CELL_COUNT,
        };
        let cells = UnsafeInlineCells::new();
        let epoch = cratonvm_types::layout_replace_epoch();
        assert!(cells.subword_epoch_if_armed().is_none());
        if cells.base_addr_for_compile() == 0 || !unsafe_subword_rmw_inline_enabled() {
            return;
        }
        assert_eq!(cells.subword_epoch_if_armed(), Some(epoch));
        let i = unsafe_field_cell_index(61, 4, true);
        cells.note_compact_subword_field(61, 4, true, 22, epoch);
        if cratonvm_types::layout_replace_epoch() != epoch {
            return;
        }
        assert_eq!(
            cells.snapshot(2 * UNSAFE_FIELD_CELL_COUNT + i).0,
            unsafe_field_cell_word(61, 4, true, 22).unwrap()
        );
        assert_eq!(cells.snapshot(i).0, 0, "the int/long half is untouched");
        assert_eq!(
            cells.snapshot(UNSAFE_FIELD_CELL_COUNT + i).0,
            0,
            "the reference half is untouched"
        );
    }

    /// JIT round 21, lane atomics (STATICCAS21-2): a static entry is wanted
    /// only once a compile armed the half and only for a band offset; it is
    /// published whole, key last, once (a colliding offset keeps the first
    /// owner), and a zero address publishes nothing.
    #[test]
    fn jr21_atomics_static_entries_publish_once_in_band() {
        use cratonvm_jit::direct_helpers::{
            unsafe_static_cell_index, unsafe_static_cell_key, unsafe_static_rmw_inline_enabled,
            UNSAFE_STATIC_OFFSET_FIRST,
        };
        let cells = UnsafeInlineCells::new();
        let off = UNSAFE_STATIC_OFFSET_FIRST + 5;
        assert!(!cells.static_publish_wanted(off), "not armed yet");
        if cells.base_addr_for_compile() == 0 || !unsafe_static_rmw_inline_enabled() {
            return;
        }
        assert!(cells.static_publish_wanted(off));
        assert!(!cells.static_publish_wanted(5), "outside the static band");
        cells.note_static_target(off, true, 0x1000, 0x2000, 7, 3);
        assert_eq!(
            cells.static_snapshot(off),
            [
                unsafe_static_cell_key(off, true),
                0x1000,
                0x2000,
                (7 << 32) | 48
            ]
        );
        assert!(!cells.static_publish_wanted(off), "taken");
        let other = (off + 1..)
            .find(|&o| unsafe_static_cell_index(o) == unsafe_static_cell_index(off))
            .expect("a colliding offset");
        cells.note_static_target(other, false, 0x3000, 0x4000, 8, 1);
        assert_eq!(
            cells.static_snapshot(off)[0],
            unsafe_static_cell_key(off, true),
            "the first owner keeps the entry"
        );
        let free = (off + 1..)
            .find(|&o| unsafe_static_cell_index(o) != unsafe_static_cell_index(off))
            .expect("another entry");
        cells.note_static_target(free, false, 0, 0x4000, 8, 1);
        assert_eq!(cells.static_snapshot(free), [0; 4], "no mirror, no entry");
        assert!(cells.static_publish_wanted(free));
    }

    /// JIT round 21, lane atomics (STATICCAS21-2): a layout-epoch reset
    /// clears the three cell halves and leaves the static entries.
    #[test]
    fn jr21_atomics_an_epoch_reset_keeps_the_static_entries() {
        use cratonvm_jit::direct_helpers::{unsafe_static_cell_key, UNSAFE_STATIC_OFFSET_FIRST};
        let cells = UnsafeInlineCells::new();
        if cells.base_addr_for_compile() == 0
            || !cratonvm_jit::direct_helpers::unsafe_static_rmw_inline_enabled()
        {
            return;
        }
        let off = UNSAFE_STATIC_OFFSET_FIRST + 9;
        cells.note_static_target(off, false, 0x1000, 0x2000, 9, 1);
        // A stale word 0 forces the reset on the next publication.
        cells.words[0].store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
        let epoch = cratonvm_types::layout_replace_epoch();
        cells.note_compact_field(3, 1, false, 24, epoch);
        assert_eq!(
            cells.static_snapshot(off),
            [
                unsafe_static_cell_key(off, false),
                0x1000,
                0x2000,
                (9 << 32) | 16
            ]
        );
    }

    /// JIT round 21 proposal ATOMICS21-5: the entries' mirror words are weak
    /// -- a moved mirror's word follows it, an unmoved one stays, and an
    /// unloaded class's entries are withdrawn whole (the entry is free again).
    #[test]
    fn jr21_atomics_static_mirrors_follow_the_collector_and_die_with_the_class() {
        use cratonvm_jit::direct_helpers::{
            unsafe_static_cell_index, unsafe_static_cell_key, UNSAFE_STATIC_OFFSET_FIRST,
        };
        let cells = UnsafeInlineCells::new();
        if cells.base_addr_for_compile() == 0
            || !cratonvm_jit::direct_helpers::unsafe_static_rmw_inline_enabled()
        {
            return;
        }
        let a = UNSAFE_STATIC_OFFSET_FIRST + 3;
        let b = (a + 1..)
            .find(|&o| unsafe_static_cell_index(o) != unsafe_static_cell_index(a))
            .expect("another entry");
        cells.note_static_target(a, true, 0xA000, 0x2000, 11, 0);
        cells.note_static_target(b, false, 0xB000, 0x3000, 12, 2);
        cells.remap_static_mirrors(&|old| (old == 0xA000).then_some(0xA800));
        assert_eq!(cells.static_snapshot(a)[1], 0xA800, "moved");
        assert_eq!(cells.static_snapshot(b)[1], 0xB000, "not moved");
        cells.forget_static_class(11);
        assert_eq!(cells.static_snapshot(a), [0; 4], "withdrawn");
        assert!(cells.static_publish_wanted(a), "free again");
        assert_eq!(
            cells.static_snapshot(b),
            [
                unsafe_static_cell_key(b, false),
                0xB000,
                0x3000,
                (12 << 32) | 32
            ]
        );
    }
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
            // Still advanced by default: it drops queued compile requests and
            // expires every optimizing OSR memo entry, OSR loop budget and
            // verdict-registry flush row of the VM.
            //
            // Interpreter round i6 wave 117, lane L3 (stage 3 of
            // `i14-L3-proposal-class-scoped-redefinition-invalidation`;
            // `CRATONVM_JIT_SCOPED_REDEFINITION_KEEPS_CODE_STATE`, default
            // off, performance): a scoped redefinition leaves the epoch
            // alone. What it guarded is covered per class:
            // * a queued request names a method key, and its worker fetches
            //   the bytecode after opening its compile witness; a witness
            //   older than this redefinition's per-class barrier
            //   (`fence_redefinition_publications`) cannot publish a body that
            //   declares or inlined the class, and a class some compile copied
            //   never takes this arm (`scoped_redefinition_admitted`);
            // * an optimizing OSR memo entry of the class misses on its
            //   redefine generation, one marked withdrawn above is forgotten,
            //   and one that inlined the class fails the dependency log
            //   (`RedefinedClass`, logged by the fence); an optimizing OSR
            //   build that spans this redefinition is refused by its own
            //   redefinition stamp (`jit_bridge::OsrBuildStamps`);
            // * the class's own tiering state, OSR denials and verdicts reset
            //   in `on_class_redefined`, and the redefine epoch below still
            //   expires the verdicts about inlined callees.
            // Every unrelated compiled loop keeps its memo, budget and
            // verdicts, which is the point.
            if !scoped_redefinition_keeps_code_state() {
                self.advance_code_state_epoch();
            }
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
    /// (`docs/feature-designs/deopt-osr.md`).
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
                let listed: FxHashSet<u64> = bodies.iter().map(|body| body.artifact_id).collect();
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
    /// `docs/feature-designs/jdk-only-mode.md`).
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
        self.jit_cache.mark_withdrawn_by_redefinition(&osr_bodies);
        let bodies: Vec<Arc<cratonvm_jit::CompiledMethod>> =
            stale.iter().chain(&osr_bodies).cloned().collect();
        // Wave 47 (lane L2): the interpreter-only pass, which forces a body an
        // earlier redefinition spared as its class's obsolete activation when
        // its exit can be resumed
        // (`cratonvm_jit::not_entrant::INTERPRETER_ONLY_OBSOLETE_BODIES_LEAVE`).
        let forced = self
            .jit_cache
            .force_exit_polls_for_the_interpreter(&bodies, no_name);
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
    /// `docs/feature-designs/jvmti-delivery-threading.md`):
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
        self.jit_cache.mark_withdrawn_by_redefinition(&osr_bodies);
        let bodies: Vec<Arc<cratonvm_jit::CompiledMethod>> =
            stale.iter().chain(&osr_bodies).cloned().collect();
        let forced = self
            .jit_cache
            .force_exit_polls_for_the_interpreter(&bodies, "<breakpoint>");
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
/// `docs/feature-designs/jdk-only-mode.md`).
/// `true`: a redefinition withdraws only the compiled bodies it can make
/// stale when [`JitRealm::scoped_redefinition_admitted`] holds. `false`: every
/// redefinition flushes the VM's whole code cache, the pre-wave-20 behaviour.
pub(crate) const SCOPED_REDEFINITION_EVICTION_ENABLED: bool = true;

/// `CRATONVM_JIT_SCOPED_REDEFINITION_KEEPS_CODE_STATE` (default off;
/// performance): a scoped redefinition does not advance the VM's code-state
/// epoch (`JitRealm::redefine_and_flush`). Read once per redefinition, a rare
/// event. Interpreter round i6 wave 117, lane L3 (stage 3 of
/// `i14-L3-proposal-class-scoped-redefinition-invalidation`).
pub(crate) fn scoped_redefinition_keeps_code_state() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_SCOPED_REDEFINITION_KEEPS_CODE_STATE")
}

/// Kill switch for making the compiled bodies a redefinition makes stale NOT
/// ENTRANT (interpreter round i1 wave 22, lane L6; `jit/src/not_entrant.rs`,
/// `docs/feature-designs/class-resolution-and-caching.md`).
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
        assert!(
            !running.is_withdrawn_by_redefinition(),
            "no scan reached it"
        );
        assert!(jit.jit_cache.body_withdrawn_by_redefinition(&running));
        assert!(
            running.is_withdrawn_by_redefinition(),
            "the answer is recorded for the exit sinks, which read the mark"
        );
        let fresh = i20_l1_body();
        assert!(!jit.jit_cache.body_withdrawn_by_redefinition(&fresh));
    }
}

/// nio21 wave 11 L5 (W9L2-2): the constant-`VarHandle` plan table.
#[cfg(test)]
mod nio21w11_vh_const_plans_tests {
    use super::VhConstPlans;
    use cratonvm_jit::ir_lower::{
        vh_const_query, vh_const_query_decode, VhConstPlan, VhConstRefRow,
        VH_CONST_REF_NAME_PROVES, WELL_KNOWN_VH_CONST_PLAN_TAG, WELL_KNOWN_VH_CONST_REF_ROW_TAG,
    };

    /// A filed word reads back; an unfiled key answers 0; a second filing
    /// under the same key does not replace the first (the row addresses the
    /// compile reads must not move).
    #[test]
    fn a_plan_is_filed_once_and_reads_back() {
        let plans = VhConstPlans::new();
        let word = VhConstPlan {
            receiver_class_id: 77,
            slot: 3,
            kind: b'J',
            read_only: false,
        }
        .to_word();
        assert_eq!(plans.plan_word((5, 1)), 0);
        plans.file((5, 1), word, None);
        assert_eq!(plans.plan_word((5, 1)), word);
        assert_eq!(
            plans.ref_row_addr((5, 1)),
            0,
            "a primitive field has no row"
        );
        plans.file((5, 1), word + 1, None);
        assert_eq!(plans.plan_word((5, 1)), word, "first filing wins");
        assert_eq!(VhConstPlan::from_word(word).map(|p| p.slot), Some(3));
    }

    /// A reference row is boxed, names its class through a stable pointer, and
    /// keeps its address when more rows are filed (the map may rehash).
    #[test]
    fn a_reference_row_is_stable_and_names_its_class() {
        let plans = VhConstPlans::new();
        let word = VhConstPlan {
            receiver_class_id: 9,
            slot: 0,
            kind: b'L',
            read_only: false,
        }
        .to_word();
        plans.file(
            (9, 2),
            word,
            Some((VH_CONST_REF_NAME_PROVES, 41, Some(Box::from("a/b/Node")))),
        );
        let addr = plans.ref_row_addr((9, 2));
        assert_ne!(addr, 0);
        for i in 0..512u32 {
            plans.file((1000 + i, 0), word, Some((0, 0, None)));
        }
        assert_eq!(plans.ref_row_addr((9, 2)), addr, "the row did not move");
        // SAFETY: `addr` names the boxed row `plans` owns, alive here.
        let row = unsafe { &*(addr as usize as *const VhConstRefRow) };
        assert_eq!(row.flags, VH_CONST_REF_NAME_PROVES);
        assert_eq!(row.type_class_id, 41);
        // SAFETY: the row's name describes the boxed `str` the same entry owns.
        let name = unsafe { std::slice::from_raw_parts(row.name_ptr as *const u8, row.name_len) };
        assert_eq!(name, b"a/b/Node");
    }

    /// JIT r21 wave 1 (lane typecheck, TYPECHECK21-4): `jit_checkcast`'s
    /// display publication is opt-in, and needs the displays themselves.
    #[test]
    fn r21_checkcast_display_publication_is_opt_in() {
        use super::SupersDisplays;
        let d = SupersDisplays::new();
        if cratonvm_types::flags::runtime_var("CRATONVM_JIT_CHECKCAST_ENSURE_DISPLAY").is_err() {
            assert!(!d.publish_on_checkcast());
        }
        if !d.enabled() {
            assert!(!d.publish_on_checkcast());
        }
    }

    /// JIT r25 wave 8 (W8-2): one `instanceof` memo per (target, VM), at one
    /// address across asks (recompiles share it), every way starting at the
    /// target's own id; another VM's displays own another memo; none for
    /// class id 0 or a VM that keeps no displays.
    #[test]
    fn r25_w8_a_miss_memo_is_per_target_and_vm_and_starts_at_the_target() {
        use super::SupersDisplays;
        use std::sync::atomic::{AtomicU32, Ordering};
        let (a, b) = (SupersDisplays::new(), SupersDisplays::new());
        assert_eq!(a.miss_memo_addr(0), 0);
        if !a.enabled() {
            assert_eq!(a.miss_memo_addr(77), 0, "no displays, no memo");
            return;
        }
        let memo = a.miss_memo_addr(77);
        assert_ne!(memo, 0);
        assert_eq!(memo % std::mem::align_of::<AtomicU32>(), 0);
        assert_eq!(memo, a.miss_memo_addr(77), "recompiles share it");
        assert_ne!(memo, a.miss_memo_addr(78), "per target");
        assert_ne!(memo, b.miss_memo_addr(77), "per VM");
        // SAFETY: the address of a `[AtomicU32; 4]` that `a` owns, alive here.
        let ways = unsafe { &*(memo as *const [AtomicU32; 4]) };
        for way in ways {
            assert_eq!(way.load(Ordering::Relaxed), 77);
        }
    }

    /// nio21 wave 12 L2: a published display reads back through the cell in
    /// the layout the compiled test reads; growth keeps old entries and the
    /// old table; a second publish is ignored.
    #[test]
    fn supers_displays_publish_grow_and_read_back() {
        use super::SupersDisplays;
        let d = SupersDisplays::new();
        assert_eq!(d.depth_of(7), None);
        d.publish(7, &[1, 7], &[]);
        d.publish(9, &[1, 9, 5], &[]); // not ending with the class: ignored
        assert_eq!(d.depth_of(9), None);
        let first_table = {
            // SAFETY: the cell holds a table `d` owns, alive here.
            unsafe { *(d.cell_addr() as *const usize) }
        };
        d.publish(5000, &[1, 7, 5000], &[40, 41]);
        d.publish(7, &[1, 2, 7], &[]);
        assert_eq!(d.depth_of(7), Some(1), "first publish wins");
        assert_eq!(d.depth_of(5000), Some(2));
        // nio21 wave 14 L4: the lock-free probe the lazy publishers ask.
        assert!(d.is_published(5000) && d.is_published(7));
        assert!(!d.is_published(9) && !d.is_published(1_000_000));
        // SAFETY: as above.
        let table = unsafe { *(d.cell_addr() as *const usize) } as *const u64;
        assert_ne!(table as usize, first_table, "grown");
        // SAFETY: the table and displays are owned by `d`, alive here; the
        // indices are inside the published capacity.
        unsafe {
            let cap = *table;
            assert!(cap > 5000);
            let entry = *table.add(1 + 5000) as *const u32;
            assert_eq!(
                std::slice::from_raw_parts(entry, 7),
                &[3, 1, 7, 5000, 2, 40, 41]
            );
            let entry7 = *table.add(1 + 7) as *const u32;
            assert_eq!(std::slice::from_raw_parts(entry7, 3), &[2, 1, 7]);
            assert_eq!(*table.add(1 + 9), 0);
            // The old table still holds what it held.
            let old = first_table as *const u64;
            assert_eq!(*(*old.add(1 + 7) as *const u32), 2);
        }
        // JIT r20 wave 17 (lane pAlloc2): the trailing verdict word, `0`
        // through `publish`, as given through `publish_with_verdict`.
        d.publish_with_verdict(6000, &[1, 7, 6000], &[40], true);
        // SAFETY: as above; 6000 is inside the grown capacity.
        unsafe {
            let table = *(d.cell_addr() as *const usize) as *const u64;
            let entry = *table.add(1 + 6000) as *const u32;
            assert_eq!(
                std::slice::from_raw_parts(entry, 7),
                &[3, 1, 7, 6000, 1, 40, 1]
            );
            let entry5000 = *table.add(1 + 5000) as *const u32;
            assert_eq!(*entry5000.add(7), 0, "publish writes verdict 0");
        }
    }

    /// The query word round-trips through the VM's decode, and no small
    /// well-known code decodes as one.
    #[test]
    fn the_query_words_round_trip() {
        for tag in [
            WELL_KNOWN_VH_CONST_PLAN_TAG,
            WELL_KNOWN_VH_CONST_REF_ROW_TAG,
        ] {
            let which = vh_const_query(tag, u32::MAX, u16::MAX);
            assert_eq!(vh_const_query_decode(which), Some((tag, u32::MAX, 0xFFFF)));
            let which = vh_const_query(tag, 0x1234, 7);
            assert_eq!(vh_const_query_decode(which), Some((tag, 0x1234, 7)));
        }
        for small in [0i64, 1, 2, 3, 8, 0x5E55_0040, 0x1D_0001, -1] {
            assert_eq!(vh_const_query_decode(small), None, "{small:#x}");
        }
    }
}

/// Interpreter round i3 wave 79, lane L7: an execute re-entry record answers
/// only for the very `Code` bytes it was built from.
#[cfg(test)]
mod i79_l7_execute_reentry_record_tests {
    use super::ExecuteReentryRecord;
    use crate::classloading::resolution::CachedBytecodeMethod;
    use crate::classloading::ClassId;
    use cratonvm_reader::ByteView;
    use std::sync::Arc;

    fn record(code: &ByteView) -> ExecuteReentryRecord {
        let cached = CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: ClassId::new(790_101),
            class_name: Arc::from("i79/Reentry"),
            method_name: Arc::from("m"),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            code: crate::runtime::frame::padded_bytecode(code),
            exception_table: Arc::from(Vec::new().into_boxed_slice()),
            max_stack: 1,
            max_locals: 0,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
        });
        ExecuteReentryRecord {
            code_view: code.clone(),
            cached: Arc::new(cached),
        }
    }

    #[test]
    fn a_record_answers_for_its_own_code_view_only() {
        let code = ByteView::from_vec(vec![0xb1]);
        let row = record(&code);
        // The class's own view (a clone shares the allocation): a hit.
        assert!(row.built_from(&code.clone()));
        // Equal bytes in another allocation (a redefinition, an EMCP reload,
        // another class under the id): a miss.
        assert!(!row.built_from(&ByteView::from_vec(vec![0xb1])));
        assert!(!row.built_from(&ByteView::from_vec(vec![0x04, 0xac])));
    }
}

#[cfg(test)]
mod r21_updaters_cell_settled_tests {
    use super::UnsafeInlineCells;
    use std::sync::atomic::Ordering;

    /// JIT round 21 wave 1, lane updaters (cvm-r21w1 A/B fix; lane fuzz2's
    /// page, both halves): a published cell, in either half, is settled; an
    /// empty one is not; an unfit slot always is; a refusal settles its OWN
    /// key only, never another key's; and the reference half is armed only
    /// on request.
    #[test]
    fn a_published_or_refused_key_is_settled_and_a_refusal_is_per_key() {
        let cells = UnsafeInlineCells::new();
        assert!(!cells.ref_volatile_publish_armed());
        assert!(cells.field_cell_settled(3, 0x8000, false, false));
        assert!(!cells.field_cell_settled(40, 3, false, false));
        if cells.base_addr_for_compile() == 0 {
            return; // not x86-64, or a kill switch is set
        }
        let epoch = cratonvm_types::layout_replace_epoch();
        cells.note_compact_field(40, 3, false, 12, epoch);
        // A hot unfit field: refused many times, it settles only itself.
        for _ in 0..(1u32 << 17) {
            cells.note_field_refusal(60, 5, true, false, epoch);
        }
        if cratonvm_types::layout_replace_epoch() != epoch {
            return; // another test moved the epoch: nothing is pinned
        }
        assert!(cells.field_cell_settled(40, 3, false, false));
        // The reference half of the same key is a different word.
        assert!(!cells.field_cell_settled(40, 3, true, true));
        assert!(cells.field_cell_settled(60, 5, true, false), "the refused key");
        assert!(!cells.field_cell_settled(61, 2, false, false), "another key still tries");
        assert!(!cells.field_cell_settled(41, 2, true, true), "the budget was not spent");
        // A COLLIDING key reads settled by design (the publisher keeps the
        // first owner): `(46, 0, int)` indexes `46 ^ 0 == 40 ^ (3 << 1)`, the
        // published `(40, 3, int)` cell.
        assert_eq!(
            cratonvm_jit::direct_helpers::unsafe_field_cell_index(46, 0, false),
            cratonvm_jit::direct_helpers::unsafe_field_cell_index(40, 3, false)
        );
        assert!(cells.field_cell_settled(46, 0, false, false), "a colliding owner holds the cell");
        // Refusing it spends nothing either.
        cells.note_field_refusal(46, 0, false, false, epoch);
        assert!(!cells.field_cell_settled(41, 2, true, true), "still no budget spent");
        // A real publication overwrites the refusal of its key.
        cells.note_compact_field(60, 5, true, 24, epoch);
        let i = cratonvm_jit::direct_helpers::unsafe_field_cell_index(60, 5, true);
        if cratonvm_types::layout_replace_epoch() == epoch {
            assert_eq!(
                cells.snapshot(i).0,
                cratonvm_jit::direct_helpers::unsafe_field_cell_word(60, 5, true, 24).unwrap()
            );
        }
    }

    /// Lane fuzz2's page, half 1: under a table whose epoch word is not the
    /// live layout epoch, nothing is settled (the caller reaches
    /// `hw_atomic_addr`, whose publication resets the table), and no refusal
    /// is recorded into it.
    #[test]
    fn a_stale_table_is_never_settled() {
        let cells = UnsafeInlineCells::new();
        if cells.base_addr_for_compile() == 0 {
            return;
        }
        let epoch = cratonvm_types::layout_replace_epoch();
        cells.note_compact_field(40, 3, false, 12, epoch);
        // As if a layout replacement moved the epoch past the table's.
        cells.words[0].store(u64::from(epoch.wrapping_sub(1)), Ordering::Release);
        assert!(!cells.field_cell_settled(40, 3, false, false));
        cells.note_field_refusal(41, 2, false, false, epoch);
        let j = cratonvm_jit::direct_helpers::unsafe_field_cell_index(41, 2, false);
        assert_eq!(cells.snapshot(j).0, 0, "no refusal into a stale table");
        // The next publication resets the table to the live epoch.
        cells.note_compact_field(40, 3, false, 12, epoch);
        if cratonvm_types::layout_replace_epoch() == epoch {
            assert!(cells.field_cell_settled(40, 3, false, false));
        }
    }

    /// The backstop budget saturates and, once spent, settles every key.
    #[test]
    fn the_backstop_budget_saturates() {
        let cells = UnsafeInlineCells::new();
        for _ in 0..(1u32 << 16) + 1 {
            cells.spend_publish_attempt();
        }
        assert!(cells.field_cell_settled(41, 2, true, true));
    }

    /// r21 wave 6, lane updaters (review item 6): a colliding key's refusal
    /// word settles only its own key; a refusal over a colliding refusal
    /// takes the cell and spends nothing; a refusal attempted under an epoch
    /// that is no longer the table's records nothing and spends one attempt;
    /// and a publication owns the cell for both colliding keys, which a
    /// later refusal of the collider does not evict.
    #[test]
    fn a_colliding_refusal_settles_only_its_own_key() {
        use cratonvm_jit::direct_helpers::{unsafe_field_cell_index, unsafe_field_cell_word};
        let cells = UnsafeInlineCells::new();
        let epoch = cratonvm_types::layout_replace_epoch();
        if cells.snapshot(0).1 != u64::from(epoch) {
            return; // another test moved the epoch: nothing is pinned
        }
        // `(55, 0, int)` indexes `55 ^ 0 == 60 ^ (5 << 1 | 1)`.
        assert_eq!(unsafe_field_cell_index(55, 0, false), unsafe_field_cell_index(60, 5, true));
        let budget = || cells.publish_budget.load(Ordering::Relaxed);
        let full = budget();
        cells.note_field_refusal(60, 5, true, false, epoch);
        if cratonvm_types::layout_replace_epoch() != epoch {
            return;
        }
        assert!(cells.field_cell_settled(60, 5, true, false), "the refused key");
        assert!(
            !cells.field_cell_settled(55, 0, false, false),
            "a colliding key's refusal settles nothing"
        );
        cells.note_field_refusal(55, 0, false, false, epoch);
        if cratonvm_types::layout_replace_epoch() != epoch {
            return;
        }
        assert!(cells.field_cell_settled(55, 0, false, false), "the later refusal owns the note");
        assert!(!cells.field_cell_settled(60, 5, true, false), "the earlier one re-attempts");
        assert_eq!(budget(), full, "a recorded refusal spends nothing");
        // An attempt that began under an epoch the table no longer has: its
        // refusal may describe a replaced layout, so it is not recorded.
        cells.note_field_refusal(61, 2, false, false, epoch.wrapping_sub(1));
        assert!(!cells.field_cell_settled(61, 2, false, false));
        assert_eq!(budget(), full - 1, "an unrecordable refusal spends one attempt");
        if cells.base_addr_for_compile() == 0 {
            return; // not x86-64, or a kill switch is set
        }
        cells.note_compact_field(55, 0, false, 16, epoch);
        if cratonvm_types::layout_replace_epoch() != epoch {
            return;
        }
        assert!(cells.field_cell_settled(55, 0, false, false));
        assert!(cells.field_cell_settled(60, 5, true, false), "a colliding owner holds the cell");
        cells.note_field_refusal(60, 5, true, false, epoch);
        assert_eq!(
            cells.snapshot(unsafe_field_cell_index(55, 0, false)).0,
            unsafe_field_cell_word(55, 0, false, 16).unwrap(),
            "a refusal never evicts a published cell"
        );
        assert_eq!(budget(), full - 1);
    }
}

/// JIT round 21, lane dispatch (`CRATONVM_JIT_TYPECHECK_ROWS`): the type-check
/// rows are backfilled from the displays published before the first ask,
/// filled for every later one, never moved, and laid out as
/// `cratonvm_jit::typecheck_rows` pins.
#[cfg(test)]
mod jr21_dispatch_typecheck_rows_tests {
    use super::SupersDisplays;
    use cratonvm_jit::typecheck_rows::{
        iface_slot_word, ROWS_CAP, ROW_BYTES, ROW_WORDS, STATUS_DEPTH_SHIFT, STATUS_HIERARCHY_ONLY,
        STATUS_MASK, STATUS_PUBLISHED,
    };

    fn row(addr: usize, id: u32) -> [u32; ROW_WORDS] {
        let mut out = [0u32; ROW_WORDS];
        // Cast: a row index below `ROWS_CAP`.
        let p = (addr + id as usize * ROW_BYTES) as *const u32;
        for (i, w) in out.iter_mut().enumerate() {
            // SAFETY: the rows `d` owns are alive in the caller; `id <
            // ROWS_CAP` and `i < ROW_WORDS` keep the read inside the buffer.
            *w = unsafe { p.add(i).read_volatile() };
        }
        out
    }

    #[test]
    fn rows_are_backfilled_then_filled_and_never_move() {
        let d = SupersDisplays::new();
        if !d.enabled() {
            return; // displays switched off in this environment
        }
        d.publish_with_verdict(7, &[1, 7], &[40], true);
        let addr = d.typecheck_rows_addr();
        assert_ne!(addr, 0);
        assert_eq!(addr % ROW_BYTES, 0);
        let r7 = row(addr, 7);
        assert_eq!(r7[0] & STATUS_MASK, STATUS_HIERARCHY_ONLY, "backfilled");
        // JIT round 21, lane tcmiss: the class's own depth rides in byte 1.
        assert_eq!(r7[0] >> STATUS_DEPTH_SHIFT, 1);
        assert_eq!(r7[1], 7);
        assert_eq!(r7[iface_slot_word(40)], 40);
        d.publish_with_verdict(9, &[1, 7, 9], &[41], false);
        assert_eq!(d.typecheck_rows_addr(), addr, "the rows do not move");
        let r9 = row(addr, 9);
        assert_eq!(r9[0] & STATUS_MASK, STATUS_PUBLISHED);
        assert_eq!(r9[0] >> STATUS_DEPTH_SHIFT, 2);
        assert_eq!(&r9[1..3], &[7, 9]);
        assert_eq!(r9[iface_slot_word(41)], 41);
        // A class past the rows gets a display and no row.
        d.publish_with_verdict(ROWS_CAP + 3, &[1, ROWS_CAP + 3], &[], true);
        assert_eq!(d.depth_of(ROWS_CAP + 3), Some(1));
        assert_eq!(row(addr, 8), [0u32; ROW_WORDS], "untouched row");
        // A second publication of a class changes nothing (first one wins).
        d.publish_with_verdict(7, &[1, 9, 7], &[], false);
        assert_eq!(row(addr, 7), r7, "a row is written once");
    }
}
