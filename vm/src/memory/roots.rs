// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! GC root scanning — collects all live ObjectRefs from the VM state.
//!
//! Root sources:
//! 1. Thread frames (locals + operand stacks)
//! 2. Static fields
//! 3. Class lock objects (for static synchronized methods)
//! 4. Thread printed values (test harness)

use crate::threading::jvm_thread::JvmThread;
use crate::types::{ObjectRef, Value};
use crate::vm::SharedVm;

/// The process-level half of [`conservative_locals_enabled`]: is the
/// conservative frame probe COMPILED IN for this run at all?
///
/// Split out from the per-cycle half so the A5 second pass in [`collect_roots`]
/// can ask the same opt-out question without also asking `is_active()`, which
/// is false on exactly the cycle that pass exists for.
///
/// `real_forkjoinpool` is the gate the multi-thread reclamation bug lives under
/// (see the FJP-worker test case). It DEFAULTS ON — the synthetic pool is the
/// opt-in (`CRATONVM_SYNTHETIC_FORKJOINPOOL`) — so this is true for the whole
/// app gauntlet, not the narrow opt-in lane an earlier revision of this comment
/// claimed. Any blast-radius argument that reads "off by default here" is
/// reading a default that has not existed since the flag was inverted; the
/// real bound on the probe is the per-cycle non-moving condition below.
/// `CRATONVM_NO_CONSERVATIVE_LOCALS` turns it off outright.
#[inline]
fn conservative_locals_compiled_in() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        cratonvm_types::flags::flags().natives.real_forkjoinpool
            && cratonvm_types::flags::runtime_var_os("CRATONVM_NO_CONSERVATIVE_LOCALS").is_none()
    })
}

/// Does step 14a5 run on THIS cycle?
///
/// `step1_ran` is [`collect_roots`]' own `conservative_locals`: when step 1
/// already probed the frames there is nothing left to add, and re-running it
/// would only double the root vector.
///
/// A free function rather than an inline condition so the truth table is
/// testable without driving a whole collection — the flag it reads is set from
/// inside `collect_roots` (by step 14's JIT scan), which is exactly what makes
/// the in-line form untestable.
#[inline]
fn a5_frame_pass_engages(step1_ran: bool) -> bool {
    !step1_ran
        && conservative_locals_compiled_in()
        && cratonvm_gc::gc_quiescence::unregistered_jit_frame_on_stack()
}

/// The conservative frame probe itself: every local and operand-stack slot of
/// every frame on `thread`, liveness-unfiltered and tag-independent.
///
/// Shared by step 1 (via `conservative_locals`, inline there because it
/// interleaves with the tag-filtered scan) and step 14a5. Sound ONLY where the
/// collection provably does not relocate — see [`conservative_locals_enabled`].
fn conservative_frame_pass(shared: &SharedVm, thread: &JvmThread, roots: &mut Vec<ObjectRef>) {
    for frame in thread.frames.iter() {
        // Liveness-unfiltered, for the reason `scan_local_objects_all_live`
        // documents: under this collector an extra dead reference can only
        // over-retain, while a missed live one is reclaimed in place.
        frame.scan_local_objects_all_live(roots, &shared.mem.heap);
        frame.scan_locals_conservative(roots, &shared.mem.heap);
        frame
            .stack
            .scan_object_refs_conservative(roots, &shared.mem.heap);
    }
}

/// Engagement census for the A5 conservative frame pass in [`collect_roots`]
/// (step 14a5).
///
/// A repair that fires on no cycle and a repair that fires on every cycle look
/// identical in a passing test run, and the page this pass closes was reopened
/// twice by exactly that ambiguity. These two numbers say which.
pub mod a5_frame_pass {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Collections on which the pass ran — i.e. the non-moving sweep was
    /// selected by the A5 unregistered-JIT-frame flag alone, with
    /// `gc_quiescence::is_active()` false, so step 1's probe was off.
    pub static CYCLES: AtomicU64 = AtomicU64::new(0);
    /// Roots the pass added beyond the tag-filtered scan, summed over cycles.
    pub static ROOTS: AtomicU64 = AtomicU64::new(0);

    pub(super) fn note(added: usize) {
        CYCLES.fetch_add(1, Ordering::Relaxed);
        ROOTS.fetch_add(added as u64, Ordering::Relaxed);
    }

    /// `(cycles, roots)` — for the shutdown census.
    pub fn census() -> (u64, u64) {
        (
            CYCLES.load(Ordering::Relaxed),
            ROOTS.load(Ordering::Relaxed),
        )
    }
}

/// True when interpreter-frame locals should be scanned CONSERVATIVELY (every
/// pointer-shaped slot, validated by the strict `is_object_address` header
/// probe) in addition to the tag-filtered scan. Enabled exactly when the
/// non-moving + selective-promotion sweep is the collector that will run — i.e.
/// any thread is in JIT (`gc_quiescence::is_active()`), the only mode in which a
/// false-positive root is harmless (nothing is relocated). Off on the moving
/// (no-JIT) path, where a pointer-shaped `long` rooted here would be relocated
/// and corrupted. Opt out entirely with `CRATONVM_NO_CONSERVATIVE_LOCALS`.
///
/// # This is not the only way the non-moving sweep gets selected
///
/// `is_active()` is one of the collector's TWO reasons to run the non-moving
/// sweep; the other is `gc_quiescence::unregistered_jit_frame_on_stack()` (A5 —
/// a compiled frame live without a `JitEntryGuard`). That flag cannot be read
/// here: `collect_roots` clears it at the top of the pass and only step 14's
/// `scan_active_jit_frames` re-sets it, so at step 1 it is false by
/// construction. The repair is a SECOND pass after step 14 rather than a wider
/// predicate here — see `a5_conservative_frame_pass` — because by then the
/// answer is known for THIS cycle and the non-moving sweep is guaranteed.
#[inline]
pub(crate) fn conservative_locals_enabled() -> bool {
    conservative_locals_compiled_in() && cratonvm_gc::gc_quiescence::is_active()
}

/// Is this cycle one of the safe full-mark windows in which class metadata may
/// be pinned CONDITIONALLY (weak mode) rather than rooted outright?
///
/// # The A5 term is deliberately absent
///
/// `gc_quiescence::young_marker_follows_side_tables` — the same question, asked
/// by the collector — carries an `unregistered_jit_frame_on_stack()` disjunct,
/// and its own doc records that the flag "is always `false` at the mirror call
/// site" and that this function "correctly never had the term". It DID have it
/// between 2026-08 and 2026-09-08, and the term was inert for exactly the
/// reason that note gives: [`collect_roots`] clears the flag a few statements
/// above this call and only step 14 re-sets it, so it could only ever read
/// `false` here. It is deleted rather than fixed because the honest answer at
/// this point in the pass is "not yet known", and `false` is the safe
/// direction — it keeps the conservative unconditional rooting, which
/// over-retains. A live term would also have had to survive a cycle that then
/// took the moving path, which weak mode is not sound for.
///
/// # Never while the Generational CONCURRENT old-gen mark is open
///
/// Every deferral this predicate licenses (statics, class locks, CONSTANT_Dynamic
/// values and `ClassValue` results to `metadata_pin`; user-loader mirrors to
/// `mirror_pin`) is sound only if the marker that consumes THIS root set follows
/// those side tables. The Generational non-moving young marker and `old_gen_gc`'s
/// BFS do. `ConcurrentMarker` (`gc/src/concurrent_mark.rs`) — the marker behind
/// the old-gen initial-mark / remark pauses `maybe_concurrent_gc` drives — follows
/// NONE of them: no `metadata_pin`, no `mirror_pin`, no `loader_pin`, no external
/// owner edge. Those pauses call `collect_roots` like any other, and with a JIT
/// frame live anywhere in the process `is_active()` is true, so an OLD-GEN static
/// value of a user-loader class (a Tomcat webapp or Spring Boot fat-jar singleton)
/// was left out of the remark roots, never marked, and handed to
/// `concurrent_sweep` as garbage while the static still named it.
///
/// The remark pause runs with the shared phase at `ConcurrentMark`, so
/// [`generational_concurrent_mark_open`] identifies it without a caller-side
/// change; the initial-mark pause (phase still `Idle`) may still defer, which is
/// harmless because remark re-scans the roots and drains their full closure
/// before the sweep is authorised.
///
/// # ...except for the cycle's own class-unload scans (gen r5w3/unload7)
///
/// Under `CRATONVM_GEN_CONC_CLASS_UNLOAD` the concurrent driver takes its
/// initial-mark and remark `collect_roots` under
/// `gc_quiescence::with_class_unload_marking`, and the marker that consumes
/// THOSE root sets does follow the three side tables (from a snapshot taken in
/// the same pause, `cratonvm_gc::concurrent_mark::ClassUnloadTables`). Those two
/// scans are licensed whatever the phase says; see
/// [`generational_metadata_is_conditional`].
#[inline]
pub(crate) fn conditional_loader_metadata(shared: &SharedVm) -> bool {
    if !cratonvm_native_builtins::classloader::loader_unload_enabled() {
        return false;
    }
    match shared.config.gc_algorithm {
        crate::config::GcAlgorithm::Generational => generational_metadata_is_conditional(
            generational_concurrent_mark_open(shared),
            cratonvm_gc::gc_quiescence::is_active(),
            // `explicit_full_gc_sweeps_young_in_place` is `major_gc_requested`
            // unless `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG` lets the System.gc()
            // cycle copy young — the collector reads the same function, so
            // weak mode is never granted to a cycle that then relocates
            // (gen r4w2/youngpolicy).
            cratonvm_gc::gc_quiescence::explicit_full_gc_sweeps_young_in_place(),
            // gen r5w3/unload7: the concurrent cycle's own initial-mark and
            // remark root scans, when that cycle's marker follows the side
            // tables (`CRATONVM_GEN_CONC_CLASS_UNLOAD`). Only the driver sets
            // this thread-local on Generational, and only around those two
            // scans; every other scan taken while a cycle is open is vetoed
            // as before.
            cratonvm_gc::gc_quiescence::class_unload_marking(),
        ),
        crate::config::GcAlgorithm::G1 => cratonvm_gc::gc_quiescence::class_unload_marking(),
        #[cfg(feature = "zgc")]
        crate::config::GcAlgorithm::Zgc => true,
    }
}

std::thread_local! {
    /// `(vm_identity, licence)` that [`collect_roots`] step 1 published for the
    /// cycle whose native side tables this thread is scanning right now. Set
    /// only for the duration of that scan ([`NativeScanLicence`]).
    static NATIVE_SCAN_LICENCE: std::cell::Cell<Option<(usize, bool)>> =
        const { std::cell::Cell::new(None) };
}

/// Scopes [`NATIVE_SCAN_LICENCE`] to one `native_roots::scan_all_roots` call and
/// restores whatever was there before (a nested scan on this thread cannot
/// leak its licence into the outer one).
pub(crate) struct NativeScanLicence(Option<(usize, bool)>);

impl NativeScanLicence {
    pub(crate) fn enter(vm_identity: usize, licence: bool) -> Self {
        Self(NATIVE_SCAN_LICENCE.with(|c| c.replace(Some((vm_identity, licence)))))
    }
}

impl Drop for NativeScanLicence {
    fn drop(&mut self) {
        let previous = self.0;
        NATIVE_SCAN_LICENCE.with(|c| c.set(previous));
    }
}

/// The loader-conditional licence a `native_roots` row must defer under: the
/// one [`collect_roots`] step 1 computed, published (`set_metadata_weak_mode`)
/// and -- on Generational -- turned into the in-place promise of step 14w for
/// THIS cycle.
///
/// The rows used to ask one of two other things. `class-atomic-slots`,
/// `annotation-proxies` and `class-values` recomputed
/// [`conditional_loader_metadata`] a few hundred statements later, which reads
/// live process state (`is_active()`, `major_gc_requested`, the concurrent-mark
/// phase) and so can disagree with the value step 14w promised on -- a row
/// that defers on a cycle step 1 did not license is exactly the "weak mode met
/// a moving cycle" loss. `indy-call-sites` and `osc-cache` read the
/// process-global `metadata_weak_mode()`, which another VM's `collect_roots`
/// overwrote (`common-b-process-global-root-sources-FIXED-20260923.md`; the flag itself was
/// deleted in gc-common w6-a, once this was its last kind of reader). Outside a
/// `collect_roots` scan (a unit test driving one row directly) it falls back
/// to computing the licence.
///
/// gc-common w5-b.
pub(crate) fn loader_metadata_licence(shared: &SharedVm) -> bool {
    match NATIVE_SCAN_LICENCE.with(|c| c.get()) {
        Some((vm, licence)) if vm == shared.vm_identity => licence,
        _ => conditional_loader_metadata(shared),
    }
}

/// THIS VM's `loader_pin` for `class_id`: the address a loader-conditional
/// root of that class may be deferred to, or `None` (root it).
///
/// `cratonvm_types::loader_pin` is ONE process-wide registry, and class ids
/// restart at 0 per VM, so two live VMs can both have a user-loader class at
/// one id. Its VM-less `loader_pin_addr` answers from the most recent writer,
/// which is safe for a MARKER (a foreign address is only an extra edge the
/// marker's own bounds check rejects). It is not safe HERE: a root deferral
/// REPLACES the root with a `metadata_pin` row keyed by the loader address,
/// and a row keyed by another heap's loader is followed by no marker of this
/// VM -- so VM B's static, class lock, condy value, proxy `Method`, reflection
/// slot, annotation proxy, indy `CallSite` or `ObjectStreamClass` was left
/// unrooted and unmarked whenever VM A had a user-loader class at the same id.
/// On ZGC (the default) every cycle is licensed to defer, and this crate's unit
/// tests run one `SharedVm` per test in parallel.
///
/// So every deferral asks for THIS VM's row. Since gc-common w5-b the registry
/// keeps one row per VM (`loader_pin_addr_for_vm`, exact, one read lock); w4-b
/// had cross-checked the VM-less row against `classloader::defining_loader_for`
/// instead, which rooted -- over-retained -- a colliding VM's value whenever
/// the other VM happened to be the last writer. Common case unchanged: the
/// registry's `NON_EMPTY` latch answers `None` before the lock.
///
/// `docs/known-issues/gc/common-w4b-loader-pin-collision-unroots-another-vms-statics.md`.
#[inline]
pub(crate) fn vm_loader_pin_addr(shared: &SharedVm, class_id: u32) -> Option<usize> {
    cratonvm_types::loader_pin::loader_pin_addr_for_vm(shared.vm_identity, class_id)
}

std::thread_local! {
    /// gen r5w4/conc8 — `(vm_identity, class ids)`: the classes THIS
    /// `collect_roots` scan must not defer a loader-conditional root of, because
    /// the unload transaction would never remove the slot that holds it (see
    /// [`vm_deferral_owner`]). Set for the duration of one licensed scan
    /// ([`DeferralScreen`]), like [`NATIVE_SCAN_LICENCE`].
    static DEFERRAL_REFUSED: std::cell::RefCell<Option<(usize, rustc_hash::FxHashSet<u32>)>> =
        const { std::cell::RefCell::new(None) };
}

/// Scopes [`DEFERRAL_REFUSED`] to one `collect_roots` call and restores
/// whatever was there before (a nested scan cannot leak its set outward).
pub(crate) struct DeferralScreen(Option<(usize, rustc_hash::FxHashSet<u32>)>);

impl DeferralScreen {
    pub(crate) fn enter(vm_identity: usize, refused: rustc_hash::FxHashSet<u32>) -> Self {
        Self(DEFERRAL_REFUSED.with(|c| c.replace(Some((vm_identity, refused)))))
    }
}

impl Drop for DeferralScreen {
    fn drop(&mut self) {
        let previous = self.0.take();
        let _ = DEFERRAL_REFUSED.try_with(|c| *c.borrow_mut() = previous);
    }
}

/// gen r5w4/conc8 — the classes that HAVE a `loader_pin` row but that the
/// unload transaction never unloads: a class whose namespace is built-in and
/// that is not hidden (`ClassManager::unloads_on_hint` is false), i.e. a user
/// loader's class defined into a shared namespace. Empty in the common case
/// (every pinned class is a user-namespace or hidden class). One class-manager
/// read lock and one lookup per pinned class id, once per licensed root scan,
/// taken BEFORE any per-table lock of the scan, so no deferral site nests the
/// manager's lock inside its table's.
fn classes_whose_roots_never_defer(shared: &SharedVm) -> rustc_hash::FxHashSet<u32> {
    let pinned = cratonvm_types::loader_pin::pinned_class_ids();
    if pinned.is_empty() {
        return rustc_hash::FxHashSet::default();
    }
    let cm = shared.classes.class_manager.read();
    pinned
        .into_iter()
        .filter(|&id| never_unloads(&cm, id))
        .collect()
}

/// A class the unload transaction never removes although a `loader_pin` row
/// names it: PRESENT in the class store and not `unloads_on_hint` (a built-in
/// namespace, not hidden). An id the store does not hold (a class already
/// unloaded, or a unit-test fixture with no class behind it) is not refused:
/// there is nothing the transaction would leave behind for it.
fn never_unloads(cm: &crate::classloading::ClassManager, class_id: u32) -> bool {
    let id = crate::classloading::ClassId::new(class_id);
    cm.get_class(id).is_some() && !cm.unloads_on_hint(id)
}

/// gen r5w4/conc8 — the owner a loader-conditional root of `class_id` may be
/// DEFERRED to: [`vm_loader_pin_addr`], but only for a class the unload
/// transaction removes together with its loader (`ClassManager::unloads_on_hint`:
/// a user-namespace or hidden class). `None` means root it.
///
/// # Why (`gengc-r5w3-unload7-orphaned-class-statics-deferred-to-a-dead-loader`)
///
/// A deferred value is marked only if its owner is. When the owner loader dies,
/// `memory::gc::unload_dead_class_metadata` drops the deferred rows (statics,
/// class locks, proxy `Method`s, condy values, ...) of exactly the classes it
/// UNLOADS — and it does not unload a class that lives in a built-in namespace
/// though a user loader defined it (it stays loaded behind its orphan marker).
/// Deferring such a class's statics to that loader left them unmarked when the
/// loader died: the collector freed the values while the class and its static
/// slots stayed, and with the loader's `loader_pin` row gone the next root scan
/// rooted the dangling slots outright. Rooting them instead is what the
/// transaction's own classification implies: a class that is never unloaded
/// has permanent statics. Such a class exists wherever a defining-loader row is
/// registered for a class whose namespace id is built-in
/// (`classloader::cl_define_class_basic` registers the receiver whatever
/// namespace `get_or_assign_loader_id` gave it; the orphan-marker machinery of
/// gc-common w11-e exists for exactly these classes).
///
/// Inside `collect_roots` the answer comes from the scan's precomputed set
/// ([`DeferralScreen`]); outside it (a unit test driving one table) it asks the
/// class manager directly.
pub(crate) fn vm_deferral_owner(shared: &SharedVm, class_id: u32) -> Option<usize> {
    let loader = vm_loader_pin_addr(shared, class_id)?;
    let screened = DEFERRAL_REFUSED.with(|c| {
        c.borrow().as_ref().and_then(|(vm, refused)| {
            (*vm == shared.vm_identity).then(|| refused.contains(&class_id))
        })
    });
    let refused = match screened {
        Some(refused) => refused,
        None => never_unloads(&shared.classes.class_manager.read(), class_id),
    };
    (!refused).then_some(loader)
}

/// The Generational arm of [`conditional_loader_metadata`], as a pure truth
/// table so it can be pinned by a test without driving a concurrent cycle.
///
/// `concurrent_mark_open` VETOES the two licensing terms: while it is set the
/// root set may be consumed by `ConcurrentMarker`, which (by default) follows
/// no side table.
///
/// gen r5w3/unload7: `class_unload_marking` is the fourth input, and it
/// LICENSES whatever the others say. It is `gc_quiescence::class_unload_marking()`,
/// which on Generational is set only by the concurrent driver around its
/// initial-mark and remark `collect_roots`, and only under
/// `CRATONVM_GEN_CONC_CLASS_UNLOAD` — i.e. exactly for the two root sets whose
/// consumer is a `ConcurrentMarker` armed with the side tables
/// (`ConcurrentMarker::set_class_unload_tables` / `add_class_unload_tables`),
/// which follows `loader_pin`, `mirror_pin` and `metadata_pin` as G1's marker
/// does. It is G1's rule for the same function (`GcAlgorithm::G1` above).
/// Every other root scan taken while a cycle is open — a young pause between
/// the two, a `System.gc()` — still sees the veto. Unset (the default) the
/// function is exactly the three-input one it was.
#[inline]
fn generational_metadata_is_conditional(
    concurrent_mark_open: bool,
    jit_active: bool,
    major_gc_requested: bool,
    class_unload_marking: bool,
) -> bool {
    class_unload_marking || (!concurrent_mark_open && (jit_active || major_gc_requested))
}

/// Must `collect_roots` force this cycle onto the non-moving young sweep to
/// keep the promise its `metadata_pin` weak mode was published on?
///
/// A pure truth table (pinned by
/// `tests::metadata_weak_mode_forces_in_place_only_when_term_four_is_disarmed`).
/// True exactly when weak mode is in force, the only licence was the
/// JIT-active term (`explicit_in_place` is kept by the collector on its own),
/// the collector would otherwise copy (`moving_young`), and the collector's
/// own conservative-JIT-root divert — the term that used to keep this promise
/// emergently — will not fire (`term4_armed == false`). Everything else is the
/// pre-fix behaviour, byte for byte.
#[inline]
fn metadata_weak_mode_needs_in_place_promise(
    weak_mode: bool,
    explicit_in_place: bool,
    moving_young: bool,
    term4_armed: bool,
) -> bool {
    weak_mode && !explicit_in_place && moving_young && !term4_armed
}

/// `CRATONVM_GC_NO_PEER_PIN_DIVERT` as `gen_heap::gen_no_peer_pin_divert`
/// reads it (declared flag; token `-peer-pin-divert`). Read here only to tell
/// whether the collector's term 4 will keep the weak-mode promise — see step
/// 14w of [`collect_roots`].
fn dbg_no_peer_pin_divert() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_GC_NO_PEER_PIN_DIVERT").is_some()
    })
}

/// Is a Generational concurrent old-gen mark cycle open (its shared phase at
/// `ConcurrentMark` or `Remark`)?
///
/// While it is, every root set this VM builds may be consumed by
/// `ConcurrentMarker::remark`, which follows no side table — see
/// [`conditional_loader_metadata`]. Collector-gated: G1 and ZGC run their own
/// markers (both follow the pins) and do not drive this state.
#[inline]
pub(crate) fn generational_concurrent_mark_open(shared: &SharedVm) -> bool {
    matches!(
        shared.config.gc_algorithm,
        crate::config::GcAlgorithm::Generational
    ) && shared.mem.concurrent_gc_state.is_marking_active()
}

/// May this class mirror be left OUT of the unconditional root set?
///
/// The deferral exists so a user-defined loader's classes can be unloaded: a
/// mirror dropped here is reached instead through `mirror_pin` propagation,
/// which marks every mirror a LIVE loader defined. All three inputs are
/// necessary and the third one is the whole point:
///
/// * `is_user_defined` — the permanent `ClassLoaderId::UserDefined`
///   classification. A built-in-loader class is never deferred (see
///   `conditional_loader_metadata`'s caller for why the mutable
///   `defining_loader_for` must not be used for THIS half).
/// * `deferrable` — [`cratonvm_gc::VmHeap::mirror_pin_deferrable`]: will a
///   marker that runs this cycle actually follow the propagation?
/// * `named_by_mirror_pin` — **does the propagation have a row for this
///   mirror at all?** `mirror_pin` is populated by
///   `vm_object::get_or_create_class_mirror` only when
///   `classloader::defining_loader_for` had a recorded pairing at the moment
///   the mirror was minted, so a `UserDefined` class can perfectly well have
///   no row. Without this input the first two said "defer" for such a class
///   and NOTHING rooted it: the collector freed a `java.lang.Class` live
///   bytecode was still using, the allocator re-served the address, and the
///   next read through it — a `Class`-typed field, the `ldc` constant record,
///   the mirror cache itself — returned an unrelated object.
///
/// Steps 2, 3 and 13 of [`collect_roots`] never had this hole: each RECORDS
/// the metadata pin itself and only then skips the root, so it cannot skip a
/// root it did not replace. This is the same rule for step 6.
#[inline]
fn mirror_deferral_is_covered(
    is_user_defined: bool,
    deferrable: bool,
    named_by_mirror_pin: bool,
) -> bool {
    is_user_defined && deferrable && named_by_mirror_pin
}

/// gen r5w6/conc10 — the switch of [`young_mirror_defer_licensed`] (declared;
/// token `gen-young-mirror-defer`).
const YOUNG_MIRROR_DEFER_FLAG: &str = "CRATONVM_GEN_YOUNG_MIRROR_DEFER";

/// gen r5w6/conc10 — may step 6 of [`collect_roots`] defer a YOUNG
/// user-loader mirror that `VmHeap::mirror_pin_deferrable` would root, on a
/// scan the loader-conditional licence covers (`conditional_metadata`, the
/// only place this is asked)?
///
/// # Why (`gengc-r5w6-conc10-a-young-mirror-root-is-pinned-forever-FIXED-20260929.md`)
///
/// `mirror_pin_deferrable` defers a young mirror only when the cycle is
/// CERTAIN to take the non-moving young marker at step 6
/// (`young_marker_follows_side_tables`), which under moving-young (the
/// default) is only an explicit `System.gc()`. On every other scan the young
/// mirror of a user class is a direct ROOT, and the non-moving sweep that a
/// live JIT frame forces pins every root value against selective promotion.
/// So a user mirror that is young when the JIT starts stays young for the
/// life of the process — rooted at every collection and every concurrent
/// pause, its `classLoader` field seeding its loader: the concurrent cycle can
/// never unload that loader (`GenR5W5ConcUnloadProbe` with the JIT; `--nojit`
/// copies young, so the mirror is promoted and the cycle unloads).
///
/// With the flag, a licensed Generational scan defers the young mirror too,
/// and step 14w' turns the licence into the promise the deferral needs
/// (`set_force_non_moving_jit_roots`, only when a mirror was deferred): the
/// non-moving marker then reaches the mirror through its loader's row
/// (`seed_mirror_pins_of_old_owners` for an old loader, `scan_young_object`
/// for a young one), it is no longer a root value, so it ages and is promoted
/// like any heap-reachable survivor, and once old it is deferred by the heap's
/// own rule. Excluded: the concurrent cycle's class-unload scans (their
/// consumer is not a young collection; nothing to gain), the
/// `CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER` opt-out of young mirror deferral, and
/// `CRATONVM_DBG_FORCE_MOVING` (the one switch that can carry a cycle past the
/// non-moving divert).
fn young_mirror_defer_licensed(shared: &SharedVm) -> bool {
    young_mirror_defer_rule(
        matches!(
            shared.config.gc_algorithm,
            crate::config::GcAlgorithm::Generational
        ),
        cratonvm_types::flags::runtime_flag_default_on(YOUNG_MIRROR_DEFER_FLAG),
        cratonvm_types::flags::runtime_var_os("CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER").is_none(),
        cratonvm_gc::gc_quiescence::class_unload_marking(),
        cratonvm_types::flags().gc.dbg_force_moving,
    )
}

/// gcd d2/g — does this scan publish its precise-table counts for the young
/// sweep (`CRATONVM_GEN_PRECISE_ROOT_PROMOTE`, opt-in, Generational only)?
///
/// `gengc-r5w6-conc10-selective-promotion-pins-precise-root-values-forever-20260927.md`:
/// the non-moving young sweep pins every young root value, so an object a
/// static, the interned-string pool, the class-mirror table or a JNI global
/// names DIRECTLY never leaves young under a live JIT. Those four tables are
/// rewritten by `memory::gc::update_all_roots` through the map the sweep
/// returns, so a value only they name may be promoted like any heap-reachable
/// survivor. See `cratonvm_gc::gen_heap::tenuring` (`precise_only_root_values`)
/// for the rule the sweep applies to what is published here.
fn precise_root_promote_licensed(shared: &SharedVm) -> bool {
    matches!(
        shared.config.gc_algorithm,
        crate::config::GcAlgorithm::Generational
    ) && cratonvm_gc::gen_heap::precise_root_promote_enabled()
}

/// gcd d2/g — count, per address, the entries of `roots` inside `sections`
/// (the `[start, end)` ranges of `collect_roots` steps 2, 5, 6 and 9) and
/// publish the counts on this thread for the young sweep. An address pushed
/// twice by those steps (two statics naming one object) counts twice: the sweep
/// exempts it only if the root list it is handed holds it exactly that many
/// times, i.e. nothing else — this scan's later steps, a peer's snapshot, a
/// frozen peer's registers — named it.
fn publish_precise_root_counts(roots: &[ObjectRef], sections: &[(usize, usize); 4]) {
    let mut counts: rustc_hash::FxHashMap<usize, u32> = rustc_hash::FxHashMap::default();
    for &(start, end) in sections {
        let Some(section) = roots.get(start..end) else {
            // A range that does not describe the list publishes nothing: the
            // sweep then pins every root value, the legacy behaviour.
            return;
        };
        for r in section {
            *counts.entry(r.as_ptr() as usize).or_insert(0) += 1;
        }
    }
    cratonvm_gc::gen_heap::publish_precise_root_values(counts);
}

/// The truth table of [`young_mirror_defer_licensed`], pure.
#[inline]
fn young_mirror_defer_rule(
    generational: bool,
    flag_on: bool,
    young_defer_allowed: bool,
    class_unload_scan: bool,
    force_moving: bool,
) -> bool {
    generational && flag_on && young_defer_allowed && !class_unload_scan && !force_moving
}

/// May this NON-STRONG HIDDEN class's mirror be left out of the root set?
/// (gc-common w18-d,
/// `docs/internal/gc-common-round-20260923/common-w8e-non-strong-hidden-classes-unload-only-with-their-loader-FIXED-20260923.md`.)
///
/// Such a class lives exactly as long as its mirror, and the mirror must be
/// able to die while its loader lives, so it is deliberately NOT reached from
/// the loader (`mirror_pin::exclude_mirror`). What keeps it while it is in use
/// is Java reachability (a `MemberName.clazz`, a `Lookup`, user code holding
/// the `Class`), an activation of the class (the frame scan above), and its
/// live instances, through the class's `loader_pin` row, which names the
/// mirror. [`mirror_deferral_is_covered`]'s rule applies unchanged: skip a root
/// only when the replacement edge exists. So all three must hold:
///
/// * `deferrable`: a marker running this cycle follows `loader_pin`;
/// * `registered_mirror`: the class is registered non-strong, AT this mirror
///   (`classloader::non_strong_hidden_mirrors`);
/// * `instance_edge`: THIS VM's `loader_pin` row for the class names the same
///   mirror, so a live instance marks it.
#[inline]
fn non_strong_hidden_deferral_is_covered(
    deferrable: bool,
    registered_mirror: Option<usize>,
    instance_edge: Option<usize>,
    mirror: usize,
) -> bool {
    deferrable && registered_mirror == Some(mirror) && instance_edge == Some(mirror)
}

/// Per-thread roots that live in `JvmThread` FIELDS rather than on any frame,
/// and are therefore invisible to the frame walk both peer-publish paths are
/// built around.
///
/// This exists as one function called from all THREE per-thread root paths —
/// [`collect_roots`] (the collection initiator's own scan),
/// `interpreter::update_root_snapshot` (a peer parked at a cooperative
/// safepoint) and `NativeContextImpl::deposit_root_snapshot_inner` (a peer
/// blocked in a native) — because those paths had drifted apart. Both fields
/// below were rewritten by all three post-GC remaps (`gc::update_all_roots`,
/// `interpreter::apply_pointer_map_to_thread`, and
/// `NativeContextImpl::check_post_block_gc_refs`) yet published as roots by
/// NEITHER snapshot path, so they were remapped-but-never-marked.
///
/// That asymmetry is the use-after-free `memory::native_roots`' module doc
/// describes from the other side: nothing claims the object, the collector
/// frees it, and the wake path then "relocates" the dangling address through a
/// `pointer_map` that has no entry for it — leaving the owner to read a zeroed
/// header. A peer's own frames are safe because the frame walk covers them;
/// these two categories live outside it.
///
/// A new `ObjectRef`-bearing `JvmThread` field belongs HERE, so it cannot be
/// published on one path and silently dropped on the other two.
///
/// # The one list (gc-common w4-b)
///
/// This is now the WHOLE per-thread field list, the exact scan twin of
/// [`crate::memory::gc::remap_thread_off_frame_refs`]: the two families it
/// always had (print buffer, scoped values) and the two parked throwables,
/// plus `native_pin_roots`, `native_alloc_pool`, `handle_slots`,
/// `native_pending_return`, both JIT memo caches (see below), the
/// thread's own mirror and its pending async exception. Until w4-b the last
/// group was spelled out a second time in `push_thread_field_roots` (the
/// initiator) and a third and fourth time inline in both peer deposits
/// (`interpreter::update_root_snapshot`,
/// `NativeContextImpl::deposit_root_snapshot_inner`); all three now call this.
/// `memory::gc::tests::thread_field_scan_and_remap_cover_the_same_references`
/// proves it and the remap name exactly the same references, and the census
/// `every_reference_bearing_thread_field_is_scanned_and_remapped` reads THIS
/// body for both deposits.
///
/// # The JIT memo caches: rooted while present, emptied after every pause
///
/// `jit_hashmap_string_node_cache` and `string_case_cache` are memoizers, not
/// program state, but an entry that is PRESENT is always a root here -- on
/// every path, the cross-thread takeover's own copy included -- so an entry is
/// never left naming reclaimed memory. What bounds their retention is that
/// they do not stay present: the owner empties both at its first memo probe
/// and at its first safepoint publish after a pause
/// ([`validate_jit_memo_caches`] / [`drop_stale_jit_memo_caches`]), and the
/// blocked deposit empties them before parking. A dropped 100 MB `HashMap` a
/// pooled worker looked up once used to stay reachable until 32 newer entries
/// evicted it; now it is released by the first or second collection after
/// the thread's last memo probe.
pub(crate) fn push_off_frame_thread_roots(thread: &JvmThread, roots: &mut Vec<ObjectRef>) {
    // Test-harness print buffer (`native_temp_print_int` / `_print_string`).
    for val in &thread.printed {
        if let Value::Object(Some(obj_ref)) = val {
            roots.push(*obj_ref);
        }
    }
    // Scoped-value bindings (JEP 446) — the KEY as well as the value. Pushing
    // only values would let the key object be reclaimed while its binding is
    // still live, which JDK-internal code reaches via `Carrier.get(ScopedValue)`.
    for (_key_id, key_ref, val) in &thread.scoped_values {
        if let Some(obj_ref) = key_ref {
            roots.push(*obj_ref);
        }
        if let Value::Object(Some(obj_ref)) = val {
            roots.push(*obj_ref);
        }
    }
    // The JIT's pending throwable and the launcher's parked uncaught throwable.
    // Both were pushed by `collect_roots` §10 only — i.e. only when their OWNER
    // initiated the collection — and by neither peer snapshot. The launcher
    // parks `uncaught_exception_pending` and then sleeps in a blocking region
    // joining the shutdown-hook THREADS (`lang_system::run_shutdown_hooks`), so
    // the collection that can reclaim it is by construction initiated by a
    // hook thread, which sees the launcher only through its blocked deposit.
    // `jit_pending_exception` is live across the unwind from the stashing
    // helper to the interpreter's drain, which crosses safepoint polls. The
    // remap half on every path is `memory::gc::remap_thread_off_frame_refs`.
    if let Some(obj_ref) = thread.jit_pending_exception {
        roots.push(obj_ref);
    }
    if let Some(obj_ref) = thread.uncaught_exception_pending {
        roots.push(obj_ref);
    }
    // Object args popped for `safe_native_call`.
    roots.extend(thread.native_pin_roots.iter().copied());
    // Up to 2047 objects carved in one old-gen batch and handed to native
    // callbacks one at a time (refilled exactly when young space is
    // exhausted, i.e. right before its owner initiates).
    roots.extend(thread.native_alloc_pool.iter().copied());
    // `NativeContext::handle_root`'s backing store (a `None` hole is a
    // released slot). Native handle scopes live outside interpreter frames,
    // just like the pin stack.
    roots.extend(thread.handle_slots.iter().flatten().copied());
    // A native's object return / thrown exception before its caller has it
    // somewhere the collector scans: the interpreter consumes it when it
    // pushes the value (`native_return_pushed_to_stack`); a JIT helper drops
    // it when it hands the value back to compiled code
    // (`jit::helpers::native_return_handed_to_compiled_code`, handoff-w36b).
    // Covers the GC window between the native's return and that hand-off.
    if let Some(obj_ref) = thread.native_pending_return {
        roots.push(obj_ref);
    }
    // The two JIT memo caches (see the doc above). A collection initiated by
    // ANOTHER thread sees a parked owner's caches only through its deposit,
    // and the TOMCAT-JNDIREALM-JIT.3 use-after-free was exactly a case-cache
    // entry missing from both deposits: listed here, they are published on
    // every path at once.
    for entry in &thread.jit_hashmap_string_node_cache {
        roots.push(entry.map);
        roots.push(entry.node);
        if let Some(key_object) = entry.key_object {
            roots.push(key_object);
        }
    }
    for entry in &thread.string_case_cache {
        roots.extend([entry.source, entry.first, entry.second]);
        if let Some(locale) = entry.locale {
            roots.push(locale);
        }
    }
    // The thread's own `java.lang.Thread` mirror and any pending async
    // exception. `Thread.currentThread()` hands the mirror straight back to
    // bytecode; a peer-initiated collection that neither marked nor forwarded
    // it left the next `currentThread()` an all-zero-header object (Tomcat
    // TestDigestAuthenticator).
    if let Some(obj_ref) = thread.java_thread_obj {
        roots.push(obj_ref);
    }
}

/// The class OWNER of every interpreter activation on `thread`: the defining
/// user loader of each frame's class, and for a non-strong hidden class its
/// mirror (gc-common w18-d). Deduplicated by class and by loader.
///
/// gc-common w19-a
/// (`docs/internal/gc-common-round-20260923/common-w18d-peer-interpreter-activations-do-not-keep-their-class-FIXED-20260923.md`):
/// HotSpot marks the holder of every frame's method on every thread's stack.
/// [`collect_roots`] §1 does so for the INITIATOR's frames, and for every
/// compiled activation through `jit_activation::active_class_ids`. The two peer
/// deposits (`interpreter::update_root_snapshot` for a thread parked at a
/// safepoint, `NativeContextImpl::deposit_root_snapshot_inner` for one blocked
/// in a native) published only locals and operand stacks. A peer running a
/// static method of a class whose loader nothing else reached let a
/// loader-conditional cycle (`conditional_loader_metadata`) defer the class's
/// mirror and statics to that loader, report the class dead, and unload it
/// under the running frame. Both deposits now publish this list.
///
/// Cost: one relaxed load (and no allocation) in a process that never defined
/// a user-loader or a non-strong hidden class. Otherwise one lookup per
/// DISTINCT class on the stack (a recursion asks once) and, when non-strong
/// hidden classes exist, one snapshot of this VM's rows. Both deposits run
/// only at a pause (the parked peer's safepoint arrival, the initiator's own
/// publish) or on a blocking transition, never per bytecode.
///
/// gce e1/j: inside the JNI in-native package's deposit
/// (`conservative_roots::in_native_deposit_armed`, i.e. once per JNI native
/// call) the snapshot is replaced by [`frame_class_owners_per_class`], which
/// gives the same answer without copying every row. Every other deposit takes
/// the snapshot exactly as before (gce e1b: the default path is kept
/// byte-identical to e1).
///
/// Returned rather than appended: the lookups take the defining-loader store's
/// and the hidden-class table's locks, and the deposits call this BEFORE they
/// lock their snapshot, so no lock is nested inside `root_snapshot`.
pub(crate) fn frame_class_owners(vm: usize, thread: &JvmThread) -> Vec<ObjectRef> {
    if crate::jit::conservative_roots::in_native_deposit_armed() {
        return frame_class_owners_per_class(vm, thread);
    }
    let hidden = cratonvm_native_builtins::classloader::non_strong_hidden_mirrors(vm);
    if hidden.is_empty()
        && !cratonvm_native_builtins::classloader::any_defining_loader_registered()
    {
        return Vec::new();
    }
    let mut owners: Vec<ObjectRef> = Vec::new();
    let mut asked: Vec<u32> = Vec::new();
    for frame in thread.frames.iter() {
        let class_id = frame.class_id.as_u32();
        if asked.contains(&class_id) {
            continue;
        }
        asked.push(class_id);
        if let Some(&mirror) = hidden.get(&class_id) {
            // SAFETY: a current heap address -- the table is remapped by every
            // collection's reconcile and pruned when the mirror dies.
            owners.push(unsafe { ObjectRef::from_raw(mirror as *mut u8) });
        }
        if let Some(loader) =
            cratonvm_native_builtins::classloader::defining_loader_for(vm, class_id)
        {
            if !owners.contains(&loader) {
                owners.push(loader);
            }
        }
    }
    owners
}

/// gce e1/j: [`frame_class_owners`] asked per distinct frame class
/// (`non_strong_hidden_mirror`, one keyed probe) instead of through a snapshot
/// of ALL of this VM's non-strong hidden rows (`non_strong_hidden_mirrors`:
/// the table's lock, a walk of every row of every VM and a fresh `HashMap`).
/// Every JDK `LambdaForm` / method-handle class is such a class, so that
/// snapshot was hundreds of rows copied per JNI call under the package
/// (gce e1b census: most of the deposit on Generational). Same owners, same
/// order.
fn frame_class_owners_per_class(vm: usize, thread: &JvmThread) -> Vec<ObjectRef> {
    if !cratonvm_native_builtins::classloader::any_non_strong_hidden()
        && !cratonvm_native_builtins::classloader::any_defining_loader_registered()
    {
        return Vec::new();
    }
    let mut owners: Vec<ObjectRef> = Vec::new();
    let mut asked: Vec<u32> = Vec::new();
    for frame in thread.frames.iter() {
        let class_id = frame.class_id.as_u32();
        if asked.contains(&class_id) {
            continue;
        }
        asked.push(class_id);
        if let Some(mirror) =
            cratonvm_native_builtins::classloader::non_strong_hidden_mirror(vm, class_id)
        {
            // SAFETY: as in `frame_class_owners`.
            owners.push(unsafe { ObjectRef::from_raw(mirror as *mut u8) });
        }
        if let Some(loader) =
            cratonvm_native_builtins::classloader::defining_loader_for(vm, class_id)
        {
            if !owners.contains(&loader) {
                owners.push(loader);
            }
        }
    }
    owners
}

/// The generation the per-thread JIT memo caches are validated against:
/// `GcBarrier::gc_generation`, which the barrier bumps (Release) once at the
/// end of every pause, before `stw_requested` drops and any parked mutator
/// resumes. Per-VM and monotonic.
///
/// Soundness never depends on it: every PRESENT cache entry is a root on
/// every path (see [`push_off_frame_thread_roots`]). It decides only WHEN the
/// owner empties its caches -- once per pause, at the first probe or publish
/// after it -- which bounds retention and also means no entry is ever served
/// across a pause, whatever a remap path did or did not forward.
#[inline]
pub(crate) fn jit_memo_epoch_now(shared: &SharedVm) -> u64 {
    shared
        .mem
        .gc_barrier
        .gc_generation
        .load(std::sync::atomic::Ordering::Acquire)
}

/// Publish-side twin of [`validate_jit_memo_caches`]: empty the caches if a
/// pause has completed since they were last validated, before the safepoint
/// deposit roots them. This is what releases a dropped map held by a thread
/// that stopped probing: its next safepoint publish finds the caches stale.
///
/// Skips the generation load when both caches are empty (the common case):
/// the safepoint deposit runs on every object-returning native call. (The
/// load itself no longer misses on blocking transitions: `gc_generation` has
/// its own cache line since gc-common w5-a.)
#[inline]
pub(crate) fn drop_stale_jit_memo_caches(shared: &SharedVm, thread: &mut JvmThread) {
    if thread.jit_hashmap_string_node_cache.is_empty() && thread.string_case_cache.is_empty() {
        return;
    }
    validate_jit_memo_caches(shared, thread);
}

/// Owner-side gate of the two JIT memo caches: call before EVERY read of, and
/// every insertion into, `jit_hashmap_string_node_cache` / `string_case_cache`.
///
/// When the pause generation moved since the caches were last validated, both
/// are cleared and the thread's `jit_memo_epoch` advances. A hit is address
/// identity (`entry.map == map`, `entry.source == source`), which is only as
/// good as every pause's remap of the entry; clearing at the first probe after
/// a pause means a hit never depends on that. The memo still hits within a
/// generation. One Acquire load and a compare on the hit path.
///
/// `docs/internal/gc-common-round-20260923/common-w2b-per-thread-jit-caches-retain-dropped-maps-FIXED-20260923.md` (fixed in
/// gc-common w4-b).
#[inline]
pub(crate) fn validate_jit_memo_caches(shared: &SharedVm, thread: &mut JvmThread) {
    let now = jit_memo_epoch_now(shared);
    if thread.jit_memo_epoch != now {
        thread.jit_hashmap_string_node_cache.clear();
        thread.string_case_cache.clear();
        thread.jit_memo_epoch = now;
    }
}

/// Collect all GC root ObjectRefs from the shared VM state and the current thread.
///
/// Returns a vector of all live non-null ObjectRefs reachable from:
/// - Thread frame locals and operand stacks
/// - Static fields (all classes)
/// - Class lock objects (synthetic monitors for static synchronized methods)
/// - Thread printed values (test harness output)
/// `CRATONVM_DBG_ROOT_REMAP_AUDIT` -- section marks for the vector
/// [`collect_roots`] builds.
///
/// The scan inventory and the REMAP inventory (`native_roots::remap_all_roots`)
/// are two different lists, and a source in the first but not the second keeps
/// an object alive while continuing to name the address the collector moved it
/// away from. The audit in `memory::gc` finds such a root; without these marks
/// it can only say "one of ~4000", because `root_source_of` attributes the
/// uniform native-root registry alone and not the thirty-odd sections here.
///
/// One `push` per section per collection, and only when the flag is set.
static SCAN_MARKS: parking_lot::Mutex<Vec<(usize, &'static str)>> =
    parking_lot::Mutex::new(Vec::new());

fn scan_marks_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_ROOT_REMAP_AUDIT").is_some()
            // gen r4w6/oomjit6: the old-mark root census names each root by
            // the collect_roots step that pushed it, from these marks.
            || cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_OLDMARK_ROOT_CENSUS")
    })
}

#[inline]
fn mark_scan_section(len: usize, label: &'static str) {
    if !scan_marks_enabled() {
        return;
    }
    SCAN_MARKS.lock().push((len, label));
}

/// Which section of the last [`collect_roots`] produced the root at `index`.
pub fn scan_section_of(index: usize) -> &'static str {
    if !scan_marks_enabled() {
        return "<marks-off>";
    }
    let marks = SCAN_MARKS.lock();
    let mut best = "<before-first-section>";
    for (start, label) in marks.iter() {
        if *start <= index {
            best = label;
        } else {
            break;
        }
    }
    best
}

/// `CRATONVM_DBG_JIT_ROOTSCAN` gate, resolved once. The print it guards runs
/// per collection, not per native call, so a cached bool is the whole cost on a
/// default run.
fn dbg_jit_rootscan() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JIT_ROOTSCAN").is_some())
}

/// Append to `addrs` (the accepted band roots' addresses, about to be published
/// as this thread's G1 pins) the pins owed by band words the root screen
/// REJECTED, as far as the switches allow. Round 12 wave 1, lane g1store,
/// proposal W19-1; see `conservative_roots::g1_band_reject_pins`.
///
/// Humongous words pin their span by default (`CRATONVM_G1_BAND_REJECT_PINS`);
/// Eden/Survivor/Old words pin their region only under
/// `CRATONVM_G1_BAND_REJECT_PINS_ALL`. With `census`
/// (`CRATONVM_DBG_JIT_ROOTSCAN=1`) one `[jitpins-rejects]` line says how many
/// words each kind had and how many regions they name that no accepted root
/// already pins, i.e. what the `_ALL` arm would cost this pause.
fn publish_g1_band_reject_pins(
    heap: &crate::memory::VmHeap,
    pins: &[crate::jit::conservative_roots::g1_band_reject_pins::RejectPin],
    addrs: &mut Vec<usize>,
    census: bool,
) {
    use crate::jit::conservative_roots::g1_band_reject_pins as brp;
    let census = census && heap.is_g1();
    if pins.is_empty() && !census {
        return;
    }
    if census {
        let humongous_on = brp::humongous_enabled();
        let all_on = brp::all_enabled();
        let mut root_regions: Vec<usize> = Vec::new();
        if let crate::memory::VmHeap::G1(g1) = heap {
            for &a in addrs.iter() {
                if let Some(p) = g1.band_reject_pin(a) {
                    root_regions.push(p.region);
                }
            }
        }
        root_regions.sort_unstable();
        root_regions.dedup();
        let (mut hum_words, mut region_words) = (0usize, 0usize);
        let mut hum_new: Vec<usize> = Vec::new();
        let mut region_new: Vec<usize> = Vec::new();
        for p in pins {
            let fresh = root_regions.binary_search(&p.region).is_err();
            if p.humongous {
                hum_words += 1;
                if fresh {
                    hum_new.push(p.region);
                }
            } else {
                region_words += 1;
                if fresh {
                    region_new.push(p.region);
                }
            }
        }
        hum_new.sort_unstable();
        hum_new.dedup();
        region_new.sort_unstable();
        region_new.dedup();
        eprintln!(
            "[jitpins-rejects] humongous=(words={hum_words} new_spans={} published={humongous_on}) \
             region=(words={region_words} new_regions={} published={all_on}) \
             root_regions={} new_region_ids={region_new:?}",
            hum_new.len(),
            region_new.len(),
            root_regions.len(),
        );
    }
    brp::extend_published(pins, addrs);
}

/// `CRATONVM_GC_PRECISE_ONLY_ROOTS=1` — opt back in to the precise-only root
/// branch, i.e. let a pause skip the conservative JIT frame scan when the
/// coverage proof passes. **Default OFF since 2026-08-21.**
///
/// Why it is off by default is a measurement, not a judgement. The branch fires
/// on **~0.1 % of collections**: 2 of 14 420 on `PolynomialTest` under
/// `CRATONVM_GC_STRESS`, 31 of 46 135 and 84 of 70 144 on longer runs of the
/// same shape, and 0 of 10 on an unstressed run. Whatever it saves is bounded
/// by that, because the other 99.9 % of collections already run the scan.
/// Against that: the proof it spends is a PRESENCE test, and the runtime oracle
/// the contract names as the sufficient proof cannot observe the cycles that
/// use it (see `coverage_gate_active`). A 0.1 %-engagement optimisation is not
/// worth an unobtainable soundness argument, so the default is now the safe
/// side and the flag is how anyone who wants the old behaviour measures it.
///
/// The branch it enables trades the conservative scan away on the strength of
/// `CompiledMethod::fully_oop_covered`, which is a PRESENCE test — every
/// GC-capable safepoint recorded *an* oop map — and not a completeness one.
/// The written contract for that bit names the runtime
/// `CRATONVM_DBG_VERIFY_OOP_MAPS` oracle as the sufficient proof that must gate
/// the suppression. It does not gate it, and it structurally cannot: the oracle
/// runs inside `scan_one_frame_precise`, which runs inside
/// `scan_active_jit_frames`, which is exactly what this branch skips. On the
/// cycles the bit is spent, the oracle is not looking.
///
/// This switch exists so the difference costs one binary to measure, not two.
/// See `bug-oop-map-coverage-bit-is-presence-not-completeness-20260820-FIXED.md`.
fn dbg_precise_only_roots() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_GC_PRECISE_ONLY_ROOTS").is_some()
    })
}

/// `CRATONVM_G1_PRECISE_ONLY_ROOTS=1` — let G1 take the precise-only root
/// branch again, i.e. skip the conservative JIT scan on a pause whose oop-map
/// coverage proof passed.
///
/// This is the pre-fix behaviour, and the reason it was called unsound has
/// CHANGED — the sentence that stood here said "the pin set that keeps a
/// JIT-held object from being evacuated is built from the conservative scan's
/// output, so skipping the scan leaves the pause with no protection at all
/// rather than with a different one". That is still a true description of the
/// mechanism, but it was not the defect. The defect
/// (`bug-g1-evacuates-live-jit-reference-20260819-FIXED.md`) was that G1's coverage
/// proof was VACUOUS: the frame-band verifier classifies a spill-band word with
/// `gen_heap::addr_is_movable`, G1 published neither table it reads, so every
/// word answered "not movable" and the verifier reported a frame clean without
/// inspecting it. Suppression on a proof that inspected nothing is what left
/// the pin set empty.
///
/// G1 publishes its arena envelope into `MOVABLE_BOUNDS` since 2026-09-02, so
/// the proof is now earned rather than vacuous — `root coverage: incomplete`
/// went from 100.00% of pauses to 0.00% on the probes. Measured with both
/// switches on, G1's pin set goes to ZERO (`pin_addrs` 21 -> 0 on
/// `HumongousChurn`, 0 on `G1CardChurn`/`G1ChurnPauseProbe`/`HumongousHold`)
/// with `dangling=0` and HotSpot-identical checksums throughout.
///
/// It stays OPT-IN regardless, and the reason is now the MASTER switch's rather
/// than G1's: `dbg_precise_only_roots`'s own doc records that the suppression
/// rests on `CompiledMethod::fully_oop_covered`, a PRESENCE test rather than a
/// completeness one, and that the runtime oracle which would settle it does not
/// run on the cycles the bit is spent
/// (`bug-oop-map-coverage-bit-is-presence-not-completeness-20260820-FIXED.md`). That
/// is a JIT-wide question, not a collector one, and it is what a soak would
/// have to answer before either default moves. Kept as an opt-in so the
/// difference can be A/B'd in one binary — and note that this switch alone does
/// nothing: `CRATONVM_GC_PRECISE_ONLY_ROOTS` gates it, and an arm that sets
/// only this one measures nothing.
fn dbg_g1_precise_only_roots() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_G1_PRECISE_ONLY_ROOTS").is_some()
    })
}

/// High-water mark of the last few root sets, in entries.
///
/// # Why `collect_roots` cannot just start at `Vec::new()`
///
/// It did, for a root set that on a real application runs to tens or hundreds
/// of thousands of entries appended across ~41 sections. `Vec`'s growth is
/// doubling, so that is a dozen-plus reallocations per collection — each one a
/// fresh allocation plus a `memcpy` of everything gathered so far — and every
/// one of them lands INSIDE the pause, on the initiating thread, before any
/// marking has started.
///
/// A single relaxed load and one right-sized allocation replace all of them.
/// The hint is monotone rather than an average on purpose: undershooting costs
/// a reallocation, which is the thing being removed, while overshooting costs
/// one `ObjectRef`-sized slot per unused entry of a vector that is dropped at
/// the end of the collection. It is a hint and nothing reads it for
/// correctness, so a torn or stale value cannot do worse than the `Vec::new()`
/// this replaces.
static ROOT_COUNT_HINT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

thread_local! {
    /// Addresses of the static-field SLOTS this thread's last root scan found
    /// holding an object, for the post-collection fix-up to write through.
    ///
    /// # The design this is the first instance of
    ///
    /// `GarbageCollector::collect_garbage` takes `roots: &mut [ObjectRef]` --
    /// object ADDRESSES, not the addresses of the slots holding them. A moving
    /// collector therefore cannot fix a root in place, and everything
    /// downstream follows from that: it must build a `PointerMap` with one
    /// entry per moved object, and the VM must then re-walk the whole root
    /// surface to patch through it. `update_all_roots` plus roughly
    /// thirty-five hand-written `gc_update_*` companions is that re-walk, and
    /// `pointer_map` appears in over five hundred places across `vm/` and the
    /// native crates. Forgetting one is a silent use-after-free, and the
    /// hundred-line comment block at the end of `collect_roots` is a list of
    /// the subsystems where that already happened.
    ///
    /// Statics are where that design can be tested cheaply and safely, because
    /// their slots have a property almost nothing else in the root set has:
    /// **a stable address**. A `StaticsBlock` is a leaked `Box<[Value]>` whose
    /// base is "stable for the life of the VM", and `grow_to` deliberately
    /// leaves the old block allocated precisely so a lock-free reader's pointer
    /// stays valid. The map that owns the blocks may rehash; the slots do not
    /// move.
    ///
    /// What this buys today is narrow and real: `update_all_roots` re-walked
    /// EVERY slot of EVERY class's statics, under the `statics` WRITE lock, to
    /// patch the handful that hold references. It now walks the ones the scan
    /// already found. Same slots, same `PointerMap` lookups, no second sweep of
    /// the primitive ones.
    ///
    /// What it demonstrates is the larger claim: with the slot in hand the
    /// `PointerMap` lookup is not needed either -- a relocating collector could
    /// store the new address straight through the slot. That step is not taken
    /// here, because it belongs with a collector-side change, not a VM-side one.
    ///
    /// TAKE-ONCE. `update_all_roots` drains this; a call that is not preceded
    /// by a scan on the same thread finds it empty and does the full walk. That
    /// is what makes a stale list impossible rather than merely unlikely: every
    /// `collect_roots` / `update_all_roots` pair in the tree runs on one thread
    /// (the collection initiator), and anything that breaks that pairing
    /// degrades to the old behaviour instead of patching a list from a
    /// different scan.
    ///
    /// TAGGED WITH THE SCANNING VM (gc-common w7-a). One OS thread can scan
    /// two VMs' roots (an embedder driving one VM from inside another's
    /// native; a heap walk of VM B inside VM A's pause), and the list is
    /// per-thread, not per-VM: a scan of B between A's scan and A's fix-up
    /// replaced A's list with B's, and A's fix-up then patched B's slots
    /// through A's map (a no-op) and skipped its own statics -- every moved
    /// object a static of A named was left at its vacated address. The take
    /// now answers only the VM that published, so that interleaving falls
    /// through to A's full walk, and B's list stays for B's fix-up.
    static STATIC_REF_SLOTS: std::cell::RefCell<(usize, Vec<usize>)> =
        const { std::cell::RefCell::new((0, Vec::new())) };
}

/// `CRATONVM_GC_STATIC_ROOT_SLOTS=0` -- kill switch. With it set, the scan
/// records nothing and `update_all_roots` takes the full-walk path exactly as
/// before, so the two are one binary apart rather than one build apart.
pub fn static_root_slots_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_GC_STATIC_ROOT_SLOTS").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// Static ref slots recorded across the process, and the fix-ups that had to
/// fall back to the full walk because there were none to take.
///
/// # Why the pair, and why it is not enough on its own
///
/// `slots=0` has two meanings that call for opposite next steps: the kill
/// switch is set (or something broke the scan/fix-up pairing, and every
/// collection is doing the old full walk), or this workload simply has no
/// static reference fields. `fallbacks` separates them — a run with
/// `slots=0 fallbacks=N` did N collections the old way, and a run with
/// `slots=0 fallbacks=0` never collected at all.
///
/// Neither number says whether the recorded slots were COMPLETE. That is the
/// verifier's job (`CRATONVM_DBG_STATIC_SLOT_VERIFY=1`), and no counter can
/// stand in for it: a list that is missing a slot is indistinguishable here
/// from one that is not.
static STATIC_SLOTS_RECORDED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static STATIC_SLOT_FALLBACKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `(slots_recorded, full_walk_fallbacks)` — see [`STATIC_SLOTS_RECORDED`].
pub fn static_root_slot_counts() -> (u64, u64) {
    use std::sync::atomic::Ordering;
    (
        STATIC_SLOTS_RECORDED.load(Ordering::Relaxed),
        STATIC_SLOT_FALLBACKS.load(Ordering::Relaxed),
    )
}

// ---------------------------------------------------------------------------
// The statics scan's own cost
// ---------------------------------------------------------------------------

/// Slots visited, slots that held an object, and nanoseconds, summed over every
/// collection's section 2.
///
/// # The residual this is the number for
///
/// Section 2 walks EVERY static field of EVERY loaded class on every
/// collection, young ones included, and the standing proposal is to narrow it
/// by declared type -- an `int`-declared slot cannot hold an object, so per the
/// JVM spec it need not be visited. That proposal was parked on the grounds
/// that this tree has a history of lost-tag values and long-smuggled
/// `jobject`s and the scan's tolerance for them may be load-bearing.
///
/// It is worth what the walk COSTS, and nobody had that. The pair says which
/// half of it is even addressable: `slots` is what a declared-type filter would
/// shrink, `objects` is the part that has to be visited whatever the filter
/// says, and the ratio bounds the saving before anyone reasons about whether
/// the filter is safe.
///
/// Always on: three counter updates per COLLECTION, not per slot.
static STATICS_SCAN_SLOTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static STATICS_SCAN_OBJECTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static STATICS_SCAN_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static STATICS_SCAN_PASSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn note_statics_scan(slots: u64, objects: u64, nanos: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    STATICS_SCAN_SLOTS.fetch_add(slots, Relaxed);
    STATICS_SCAN_OBJECTS.fetch_add(objects, Relaxed);
    STATICS_SCAN_NANOS.fetch_add(nanos, Relaxed);
    STATICS_SCAN_PASSES.fetch_add(1, Relaxed);
}

/// `(passes, slots, objects, nanos)` -- see [`STATICS_SCAN_SLOTS`].
pub fn statics_scan_counts() -> (u64, u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        STATICS_SCAN_PASSES.load(Relaxed),
        STATICS_SCAN_SLOTS.load(Relaxed),
        STATICS_SCAN_OBJECTS.load(Relaxed),
        STATICS_SCAN_NANOS.load(Relaxed),
    )
}

/// Count a fix-up that found no recorded list and re-walked every static.
pub fn note_static_slot_fallback() {
    STATIC_SLOT_FALLBACKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Begin recording static ref slots for a fresh scan.
fn reset_static_ref_slots() {
    STATIC_REF_SLOTS.with(|c| {
        if let Ok(mut v) = c.try_borrow_mut() {
            v.1.clear();
        }
    });
}

/// Size of the last published static-slot list: the next scan's pre-size, so
/// the local vector in `collect_roots` section 2 does not re-grow from empty on
/// every collection. A hint only; nothing reads it for correctness.
static LAST_STATIC_SLOT_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Publish this scan's static ref slots (addresses of static `Value` slots
/// found holding an object) for `update_all_roots` to take. Replaces whatever
/// the previous scan on this thread left, exactly as the old per-slot
/// `reset` + `note` pair did. `vm` is the scanning VM's identity.
fn publish_static_ref_slots(vm: usize, slots: Vec<usize>) {
    LAST_STATIC_SLOT_COUNT.store(slots.len(), std::sync::atomic::Ordering::Relaxed);
    STATIC_REF_SLOTS.with(|c| {
        if let Ok(mut v) = c.try_borrow_mut() {
            *v = (vm, slots);
        }
    });
}

/// Take this thread's recorded static ref slots for VM `vm`, leaving the store
/// empty.
///
/// `None` when nothing was recorded -- either the kill switch is set, or no
/// scan ran on this thread since the last take -- and when the recorded list
/// is ANOTHER VM's (gc-common w7-a; see `STATIC_REF_SLOTS`), which is left in
/// place for that VM's fix-up. All three mean "do the full walk".
pub fn take_static_ref_slots(vm: usize) -> Option<Vec<usize>> {
    STATIC_REF_SLOTS.with(|c| {
        let mut v = c.try_borrow_mut().ok()?;
        if v.1.is_empty() || v.0 != vm {
            return None;
        }
        STATIC_SLOTS_RECORDED.fetch_add(v.1.len() as u64, std::sync::atomic::Ordering::Relaxed);
        Some(std::mem::take(&mut v.1))
    })
}

/// The capacity to start a root set at, and the ceiling that keeps a single
/// pathological collection from pinning a large reservation for the life of the
/// process.
const ROOT_HINT_CEILING: usize = 1 << 20;

/// Step 22 of [`collect_roots`]: root a pending, queued `Reference` only where
/// one of `thread`'s interpreter frames still HOLDS it in a local slot that the
/// per-bci liveness filter (step 1) dropped.
///
/// # What this replaced, and why it had to go (gc-common w2-b, 2026-09-23)
///
/// Step 22 used to root EVERY pending queued `Reference` in the process
/// (`036f35aac`), on the theory that the JVM must keep a registered reference
/// alive until it has been enqueued. The `java.lang.ref` package contract says
/// the opposite: *"if a registered reference becomes unreachable itself, then
/// it will never be enqueued"* — HotSpot never discovers an unreachable
/// `Reference`, so it simply dies together with everything it holds.
///
/// Rooting them made each one immortal until its referent died, together with
/// every STRONG field of the `Reference` subclass — and those fields routinely
/// point back at whatever keeps the referent alive:
///
/// * `java.io.ClassCache.CacheRef` (behind every `ObjectStreamClass.lookup`,
///   and the JDK's other per-`Class` caches) is a queued `SoftReference` with a
///   strong `type` field naming the very `Class` it caches for. Rooted, it kept
///   the class, its loader and every class that loader defined alive for as
///   long as the soft referent lived — i.e. until memory pressure — and even
///   then the cleared reference sat in the cache's static queue until the next
///   lookup. It is one of the two roots behind
///   `class_loader_unload_regression`'s `liveLoaders=6 liveClasses=6
///   unloadedDelta=0` on all three collectors — the fixture's
///   `ObjectStreamClass.lookup(type)` is its whole difference from
///   `RClassUnloadSweep`, which passed. (The other is the native
///   `Class$Atomic` side store behind `reflectionData`; see
///   `docs/internal/gc-common-round-20260923/common-w2b-class-atomic-side-store-roots-every-reflected-class-FIXED-20260923.md`,
///   fixed in w3-b.)
/// * `WeakHashMap.Entry` (queued, strong `value`): a dropped `WeakHashMap`
///   kept every entry whose key was still alive elsewhere, and an entry whose
///   value names its key was immortal.
///
/// # What is kept
///
/// The case `036f35aac` was written for: `r = new WeakReference<>(x, q)`
/// followed by `System.gc(); … q.remove()`, with `r` never read again. This
/// VM's interpreter drops `r` from the root set as soon as no later bytecode
/// reads it, where HotSpot's INTERPRETER keeps a dead local's referent until the
/// slot is overwritten or the frame returns — so the program that works on
/// HotSpot lost its notification here. Rooting exactly the pending queued
/// references that a live frame still holds in a (liveness-dead) slot restores
/// HotSpot-interpreter parity for that shape without retaining a `Reference`
/// no frame, field or queue can reach.
///
/// Scope: only the initiating thread's frames are consulted (a parked peer's
/// frames are published through its own root snapshot, already
/// liveness-filtered). A dead-local reference on a PEER thread is therefore
/// not rescued; that is the behaviour a HotSpot JIT-compiled frame gives, and
/// is spec-legal.
///
/// # Cost (gen r5w1/refs5)
///
/// The frames' locals are gathered FIRST (a handful), and the processor is
/// asked only about those
/// (`ReferenceProcessor::pending_reference_objects_among`). This used to
/// materialise every pending queued `Reference` in the process — a `Vec` and
/// then an `FxHashSet` with one entry per `WeakHashMap.Entry`, per
/// `ClassCache.CacheRef`, per queued `Cleaner` — inside every collection's
/// root scan, to intersect it with those few locals. The root set, and its
/// order (frame order, slot order, duplicates kept), is unchanged; a thread
/// with no frames, or frames with no object locals, now never takes the
/// processor lock at all.
fn push_pending_references_held_by_frames(
    thread: &JvmThread,
    shared: &SharedVm,
    roots: &mut Vec<ObjectRef>,
) {
    if thread.frames.is_empty() {
        return;
    }
    let mut held: Vec<ObjectRef> = Vec::new();
    for frame in thread.frames.iter() {
        frame.scan_local_objects_all_live(&mut held, &shared.mem.heap);
    }
    if held.is_empty() {
        return;
    }
    let candidates: rustc_hash::FxHashSet<usize> =
        held.iter().map(|o| o.as_ptr() as usize).collect();
    let pending = shared
        .mem
        .ref_processor
        .lock()
        .pending_reference_objects_among(&candidates);
    if pending.is_empty() {
        return;
    }
    roots.extend(
        held.into_iter()
            .filter(|o| pending.contains(&(o.as_ptr() as usize))),
    );
}

thread_local! {
    /// Set for the duration of [`collect_roots_registry_appended`]: the caller
    /// will append `ThreadRegistry::collect_all_root_snapshots()`, so step 10b
    /// need not push the registry's thread mirrors a second time.
    static REGISTRY_MIRRORS_APPENDED_BY_CALLER: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Restores [`REGISTRY_MIRRORS_APPENDED_BY_CALLER`] on every exit, unwinding
/// included, so a panicking scan cannot leave a later plain `collect_roots` on
/// this thread without its step 10b.
struct RegistryMirrorsAppended(bool);

impl Drop for RegistryMirrorsAppended {
    fn drop(&mut self) {
        let previous = self.0;
        REGISTRY_MIRRORS_APPENDED_BY_CALLER.with(|c| c.set(previous));
    }
}

/// [`collect_roots`] for a caller that appends
/// `ThreadRegistry::collect_all_root_snapshots()` to the result before the
/// collection -- the collection pause (`run_collection_pause`).
///
/// # The one duplicate this removes, and why it is exact
///
/// Step 10b pushes `alive_thread_objects(usize::MAX)`: every registry entry
/// with `alive` set, its `java_thread_obj` if any. `collect_all_root_snapshots`
/// pushes, for every entry with `alive` set, its `java_thread_obj` if any --
/// the same filter over the same table, read within the same stop-the-world
/// pause (the quota is met before either runs, and a thread cannot start or
/// finish while the world is stopped). So each thread mirror reached the
/// marker twice, behind two registry walks; now once, behind one. The root SET
/// is unchanged; only a duplicate and its position in the vector are gone.
///
/// The larger duplicates the proposal names (the initiator's own snapshot via
/// step 11 AND its registry entry; the initiator's second conservative JIT
/// scan) are NOT removed: proving the initiator's `root_snapshot` is the same
/// `Arc` its registry entry holds needs a registry accessor that does not
/// exist, and the scan duplicate needs the snapshot-vs-direct-scan diff the
/// proposal's risk section asks for. See
/// `docs/known-issues/gc/common-b-proposal-dedupe-the-initiator-root-set.md`.
pub(crate) fn collect_roots_registry_appended(
    shared: &SharedVm,
    thread: &JvmThread,
) -> Vec<ObjectRef> {
    let _restore =
        RegistryMirrorsAppended(REGISTRY_MIRRORS_APPENDED_BY_CALLER.with(|c| c.replace(true)));
    collect_roots(shared, thread)
}

pub fn collect_roots(shared: &SharedVm, thread: &JvmThread) -> Vec<ObjectRef> {
    if scan_marks_enabled() {
        SCAN_MARKS.lock().clear();
    }
    let __rp_t0 = crate::memory::native_roots::rootprof::on().then(std::time::Instant::now);
    // See `ROOT_COUNT_HINT`. The `+ 64` covers a set that grew by a handful of
    // entries since the last collection without forcing a double.
    let hint = ROOT_COUNT_HINT
        .load(std::sync::atomic::Ordering::Relaxed)
        .saturating_add(64)
        .min(ROOT_HINT_CEILING);
    let mut roots = Vec::with_capacity(hint);
    // See `STATIC_REF_SLOTS`. Reset per scan so the list `update_all_roots`
    // takes can only ever describe THIS collection.
    let record_static_slots = static_root_slots_enabled();
    reset_static_ref_slots();

    // Stage B (precise oop maps, B-K fix): reset the movable precise-JIT-root
    // set so it reflects only THIS collection's stack. `scan_active_jit_frames`
    // below republishes the covered, rewritable JIT-frame oops; the young
    // collector then excludes them from the pin set. No-op unless precise
    // relocation is engaged (the set stays empty on the default path).
    cratonvm_gc::gc_quiescence::clear_movable_jit_roots();
    // Its veto, cleared on the same schedule and for the same reason: the set
    // is a statement about THIS collection's compiled frames. The band scan
    // below republishes an entry for every object held in a frame word
    // `band_slot_is_verifiable` refuses to inspect, and the young sweep pins
    // those whatever the movable set says.
    cratonvm_gc::gc_quiescence::clear_unrewritable_jit_roots();
    // The register-oop mask's oracle records what it EXCLUDED this pass, so it
    // is reset on the same schedule as the sets above and checked against the
    // finished root set below.
    crate::jit::conservative_roots::clear_excluded_spill_words();
    // G1 pin-in-place: reset the conservative-JIT-root pin set too, so it
    // reflects only THIS collection's stack (republished by the JIT-frame scan
    // below, under G1). See that scan site and `G1Collector::young_collection`.
    cratonvm_gc::gc_quiescence::clear_pinned_jit_roots();
    // Reset the per-cycle incomplete-JIT-coverage fallback. The JIT root scan
    // below sets it again if moving-young must use the conservative/non-moving
    // path for this collection.
    cratonvm_gc::gc_quiescence::clear_force_non_moving_jit_roots();
    // A5 fix: reset the unregistered-JIT-frame flag; `scan_active_jit_frames`
    // below re-sets it iff it finds a guard-less JIT frame on the native stack,
    // and the generational collector consults it to pick the non-moving sweep.
    cratonvm_gc::gc_quiescence::clear_unregistered_jit_frame_on_stack();
    // gcd d2/g: the precise-table counts this scan publishes after step 9
    // (`CRATONVM_GEN_PRECISE_ROOT_PROMOTE`), cleared on the same schedule as
    // the movable set so a sweep can only ever read THIS scan's.
    cratonvm_gc::gen_heap::clear_precise_root_values();
    let precise_root_promote = precise_root_promote_licensed(shared);
    // `[start, end)` of each precise, post-GC-rewritten section of `roots`.
    let mut precise_sections: [(usize, usize); 4] = [(0, 0); 4];
    let conditional_metadata = conditional_loader_metadata(shared);
    cratonvm_types::metadata_pin::set_metadata_weak_mode(shared.vm_identity, conditional_metadata);
    cratonvm_types::metadata_pin::replace_metadata_pins(shared.vm_identity, &[]);
    // gen r5w4/conc8: which pinned classes' roots may never be deferred
    // (`vm_deferral_owner`), computed once, before any table lock below. Only
    // a licensed scan defers anything, so only it pays for the set.
    let _deferral_screen = conditional_metadata.then(|| {
        DeferralScreen::enter(shared.vm_identity, classes_whose_roots_never_defer(shared))
    });

    // A live activation keeps its defining loader and class metadata alive,
    // including static methods that carry no receiver oop. Interpreter frames
    // expose ClassId directly; compiled activations are counted globally by
    // JitEntryGuard so cross-thread STW scans see them too.
    // TEMP-DIAG (CRATONVM_DBG_MIRRORPIN_WHY): name WHICH activation is rooting
    // a user loader. Both of these push a loader with no heap referrer, so a
    // retention-path walk cannot see them at all.
    let act_dbg = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_MIRRORPIN_WHY").is_some();
    let act_name = |cid: u32| -> String {
        shared
            .classes
            .class_manager
            .read()
            .get_class(cratonvm_types::ClassId::new(cid))
            .map(|c| c.name.to_string())
            .unwrap_or_default()
    };
    // gc-common w18-d: a non-strong hidden class lives as long as its MIRROR
    // (`classloader::register_non_strong_hidden_class`), so an activation of
    // one keeps the mirror, as an activation of a user-loader class keeps the
    // loader. One snapshot per scan; empty, without a lock, in a run that
    // never defined such a class. Step 6 reads it too.
    let non_strong_hidden =
        cratonvm_native_builtins::classloader::non_strong_hidden_mirrors(shared.vm_identity);
    let push_hidden_mirror = |roots: &mut Vec<ObjectRef>, class_id: u32| {
        if let Some(&mirror) = non_strong_hidden.get(&class_id) {
            // SAFETY: a current heap address -- the registry is remapped by
            // every collection's reconcile and pruned when the mirror dies.
            roots.push(unsafe { ObjectRef::from_raw(mirror as *mut u8) });
        }
    };
    for frame in &thread.frames {
        push_hidden_mirror(&mut roots, frame.class_id.as_u32());
        if let Some(loader) = cratonvm_native_builtins::classloader::defining_loader_for(
            shared.vm_identity,
            frame.class_id.as_u32(),
        ) {
            if act_dbg {
                eprintln!(
                    "[MIRRORWHY] root via FRAME class={:?} loader={:#x}",
                    act_name(frame.class_id.as_u32()),
                    loader.as_ptr() as usize
                );
            }
            roots.push(loader);
        }
    }
    for class_id in cratonvm_types::jit_activation::active_class_ids() {
        push_hidden_mirror(&mut roots, class_id);
        if let Some(loader) =
            cratonvm_native_builtins::classloader::defining_loader_for(shared.vm_identity, class_id)
        {
            if act_dbg {
                eprintln!(
                    "[MIRRORWHY] root via JIT_ACTIVATION class={:?} loader={:#x}",
                    act_name(class_id),
                    loader.as_ptr() as usize
                );
            }
            roots.push(loader);
        }
    }

    mark_scan_section(
        roots.len(),
        "1: Thread frames — scan locals and operand stacks (SoA layout)",
    );
    // 1. Thread frames — scan locals and operand stacks (SoA layout).
    //
    // Spring Boot SEGV fix (2026-05-16): `ValueStack::scan_object_refs`
    // still reports `CompactTag::Long` operand-stack slots whose bits
    // happen to look like an aligned pointer as roots without consulting
    // the heap (the bug already removed from `Frame::scan_local_objects`
    // in frame.rs). Filter operand-stack-sourced roots against
    // `heap.is_object_address` so a primitive `long` (file size, hash,
    // jboss-modules token) can no longer poison the root set and cause a
    // `0xC0000005` SEGV when the GC later dereferences the bogus pointer.
    // Multi-thread non-moving-sweep root hardening (Fork6 FJP reclamation):
    // when the non-moving + selective-promotion sweep is the collector that will
    // run (any thread in JIT → `gc_quiescence::is_active()`), conservatively
    // probe every local for a lost-tag object reference. A JIT callee's object
    // return value can reach an interpreter local under a non-object tag (e.g.
    // `main`'s `f = POOL.submit(t)`); the tag-filtered `scan_local_objects` then
    // omits it, so selective promotion neither pins nor remaps it and the young
    // slot is evacuated+zeroed → stale all-zero receiver. The conservative probe
    // roots (and thereby PINS, since the pin set is keyed by root value) such
    // slots. Sound here ONLY because this collector never relocates — a
    // false-positive can only over-retain. Opt out with
    // `CRATONVM_NO_CONSERVATIVE_LOCALS`.
    let conservative_locals = conservative_locals_enabled();
    for frame in thread.frames.iter() {
        if conservative_locals {
            // In the non-moving stress collector, local-liveness precision can
            // drop an active FJP receiver while it is also parked in a callee
            // frame. Over-retaining a dead reference is harmless here; missing
            // the receiver lets selective promotion zero it under `join()`.
            frame.scan_local_objects_all_live(&mut roots, &shared.mem.heap);
        } else {
            // The VM's type maps feed the precise-oop-map shadow only; the
            // root set is `scan_local_objects`'. Remap twin: `update_all_roots`.
            frame.scan_local_objects_mapped(
                &mut roots,
                &shared.mem.heap,
                &shared.classes.type_maps,
            );
        }
        if conservative_locals {
            frame.scan_locals_conservative(&mut roots, &shared.mem.heap);
        }
        let before = roots.len();
        frame.stack.scan_object_refs_ruled(
            &mut roots,
            &shared.mem.heap,
            shared.config.is_jdk_only(),
        );
        // `is_heap_addr`, matching the locals scan above and the other three
        // copies of this filter — see `scan_frame_roots` in
        // `runtime/interpreter/gc_and_alloc.rs`. The strict `is_object_address`
        // probe used here until 2026-08-04 dropped genuine young / mid-init
        // roots, and this is the INITIATOR's own scan: a root it drops has no
        // second chance. Compacted IN PLACE (gc-common w4-b), exactly as
        // `update_root_snapshot` does: the `split_off` this replaced allocated
        // a fresh `Vec` for every frame with an object on its operand stack,
        // inside the pause. Order and contents are identical.
        let len = roots.len();
        if len > before {
            let mut write = before;
            for read in before..len {
                let o = roots[read];
                if shared.mem.heap.is_heap_addr(o.as_ptr() as usize).is_some() {
                    roots[write] = o;
                    write += 1;
                }
            }
            roots.truncate(write);
        }
        if conservative_locals {
            frame
                .stack
                .scan_object_refs_conservative(&mut roots, &shared.mem.heap);
        }
        // A synchronized method's implicit monitorexit target. `update_all_roots`
        // has always REWRITTEN it and the blocked deposit has always PUBLISHED
        // it; this scan did neither, so the slot was remapped-but-never-marked
        // on the initiator — kept alive only by the coincidence that an instance
        // method's receiver usually still sits in local 0. A static synchronized
        // method's lock object, or a receiver slot the bytecode reused, had no
        // root at all.
        if let Some(m) = frame.monitor_on_exit {
            roots.push(m);
        }
        // The frame's block monitors (`Frame::held_monitors`, JVMS §2.11.10
        // structured locking; wave 23 lane L7). javac keeps each in a local as
        // well, hand-written bytecode need not; either way the record is
        // compared by address at `monitorexit`, so it is rooted and remapped
        // (`update_all_roots`) like `monitor_on_exit`.
        roots.extend_from_slice(frame.held_monitors.as_slice());
    }

    // `(owner loader, object)` rows steps 2 and 3 defer to `metadata_pin`,
    // published together after step 3.
    let mut metadata_deferrals: Vec<(usize, usize)> = Vec::new();
    mark_scan_section(roots.len(), "2: Static fields — all classes");
    // 2. Static fields — all classes
    //
    // The `metadata_pin_deferrable` guard (SPB.1 residual fix) decides whether
    // a value may be deferred to `metadata_pin` — see that method's doc. On
    // Generational it says yes for an old-gen value, and (gc-common w36-d)
    // for a YOUNG value only on a cycle certain to take the non-moving young
    // precise marker, which follows `metadata_pin` from young owners and
    // seeds it from old ones. A young value deferred on a MOVING young cycle
    // would be marked by nothing and could be reclaimed mid-`<clinit>`, so
    // there it is still refused. Same reasoning applies to the class-lock
    // (`3.`) and CONSTANT_Dynamic (`13.`) sections below.
    precise_sections[0].0 = roots.len();
    {
        let __st_t0 = std::time::Instant::now();
        let statics = shared.classes.statics.read();
        let mut __st_slots = 0u64;
        let mut __st_objects = 0u64;
        // Gathered locally and published to `STATIC_REF_SLOTS` ONCE after the
        // walk. The previous per-slot `note_static_ref_slot` paid a TLS lookup
        // and a `RefCell` borrow for every object-valued static in the process,
        // inside the pause; the list it produces is identical.
        let mut static_slots: Vec<usize> = if record_static_slots {
            Vec::with_capacity(LAST_STATIC_SLOT_COUNT.load(std::sync::atomic::Ordering::Relaxed))
        } else {
            Vec::new()
        };
        for (&class_id, fields) in statics.iter() {
            // gc-common w19-a: the class's loader-pin owner, asked at most once
            // per CLASS (lazily, on its first deferrable value). It was asked
            // per object-valued static slot: a `loader_pin` read lock each,
            // inside the pause, for an answer that is the same for every slot
            // of the class.
            let mut class_owner: Option<Option<usize>> = None;
            for val in fields.iter() {
                __st_slots += 1;
                if let Value::Object(Some(obj_ref)) = *val {
                    __st_objects += 1;
                    // BEFORE the deferral branch below, deliberately. The
                    // post-collection fix-up remaps every static slot that holds
                    // an object, whether or not this scan ROOTED it -- a value
                    // deferred to `metadata_pin` still moves, and its slot still
                    // has to be corrected. Recording only the rooted ones would
                    // drop exactly the slots the deferral was invented for.
                    if record_static_slots {
                        static_slots.push(val as *const Value as usize);
                    }
                    if conditional_metadata
                        && shared
                            .mem
                            .heap
                            .metadata_pin_deferrable(obj_ref.as_ptr() as usize)
                    {
                        // gen r5w4/conc8: `vm_deferral_owner`, not the bare
                        // pin -- a class the unload never removes keeps its
                        // statics rooted.
                        let owner = *class_owner.get_or_insert_with(|| {
                            vm_deferral_owner(shared, class_id.as_u32())
                        });
                        if let Some(loader) = owner {
                            metadata_deferrals.push((loader, obj_ref.as_ptr() as usize));
                            continue;
                        }
                    }
                    roots.push(obj_ref);
                }
            }
        }
        if record_static_slots {
            publish_static_ref_slots(shared.vm_identity, static_slots);
        }
        note_statics_scan(
            __st_slots,
            __st_objects,
            __st_t0.elapsed().as_nanos() as u64,
        );
    }
    precise_sections[0].1 = roots.len();

    mark_scan_section(
        roots.len(),
        "3: Class lock objects — synthetic objects for static synchroniz",
    );
    // 3. Class lock objects — synthetic objects for static synchronized methods
    {
        let class_locks = shared.classes.class_locks.read();
        for (&class_id, obj_ref) in class_locks.iter() {
            if conditional_metadata
                && shared
                    .mem
                    .heap
                    .metadata_pin_deferrable(obj_ref.as_ptr() as usize)
            {
                if let Some(loader) = vm_deferral_owner(shared, class_id.as_u32()) {
                    metadata_deferrals.push((loader, obj_ref.as_ptr() as usize));
                    continue;
                }
            }
            roots.push(*obj_ref);
        }
    }
    // Steps 2 and 3's deferrals, published in ONE registry write (gc-common
    // w19-a). They were one `add_metadata_pin` each -- a write lock on the
    // process-wide registry and a `GENERATION` tick per deferred static slot,
    // inside the pause. `replace_metadata_pins` drops this VM's rows and
    // inserts these with the same per-owner dedup `add_metadata_pin` applies;
    // this VM has no rows to drop here, because the scan emptied them above
    // (`replace_metadata_pins(.., &[])`, before step 1) and nothing between
    // that call and this one registers a pin. Steps 6b and 13 still add after
    // this, one row at a time, onto what this writes.
    if !metadata_deferrals.is_empty() {
        cratonvm_types::metadata_pin::replace_metadata_pins(
            shared.vm_identity,
            &metadata_deferrals,
        );
    }

    mark_scan_section(
        roots.len(),
        "4: Per-thread FIELD roots — push_off_frame_thread_roots (print buffe",
    );
    // 4-4d. Every reference that lives in a `JvmThread` FIELD rather than a
    //    frame: the print buffer, scoped-value bindings, the two parked
    //    throwables, native invoke pins, the native old-gen allocation pool,
    //    rooted handle slots, the native object in flight, both JIT memo
    //    caches, the thread's own mirror and its pending
    //    async exception. One list, shared with both peer deposits and the
    //    exact scan twin of `memory::gc::remap_thread_off_frame_refs` (pinned
    //    by `memory::gc::tests::thread_field_scan_and_remap_cover_the_same_references`);
    //    see `push_off_frame_thread_roots` for the per-family rationale.
    push_off_frame_thread_roots(thread, &mut roots);

    mark_scan_section(
        roots.len(),
        "5: Interned string pool — all interned String objects",
    );
    // 5. Interned string pool — all interned String objects
    precise_sections[1].0 = roots.len();
    {
        let string_pool = shared.mem.string_pool.read();
        for obj_ref in string_pool.values() {
            roots.push(*obj_ref);
        }
    }
    precise_sections[1].1 = roots.len();

    mark_scan_section(
        roots.len(),
        "6: Class mirror cache — java.lang.Class objects",
    );
    // 6. Class mirror cache — java.lang.Class objects
    //
    // A mirror's `classLoader` field is a real heap edge to its defining
    // ClassLoader, so unconditionally rooting every mirror ever created here
    // keeps that loader alive forever too — completely defeating
    // `CRATONVM_LOADER_UNLOAD` (default ON, see `gc_scan_loader_singleton_roots`)
    // for any class that ever had a mirror created (`getClass()`, reflection,
    // annotation scanning — i.e. virtually every class). Symptom: a
    // `WeakReference<Class<?>>` (e.g. Tomcat's `ManagedConcurrentWeakHashMap`
    // used by `DefaultInstanceManager`'s annotation cache) never clears for an
    // unloaded webapp class even after its ClassLoader is otherwise
    // unreachable — `TestDefaultInstanceManager.testClassUnloading` count
    // off-by-one.
    //
    // Built-in loaders (bootstrap/extension/application) are permanent for the
    // process lifetime, so their classes' mirrors stay unconditionally rooted.
    // Classes loaded by a user-defined `ClassLoader` are NOT unconditionally
    // rooted here when the gate is on — otherwise this loop would defeat
    // `defining_loader_store`'s own unloading the same way it defeated it
    // before this fix. Such a mirror still stays alive whenever its DEFINING
    // LOADER is independently reachable (built-in-loader liveness, another
    // live reference to the loader, or a live instance of one of its OTHER
    // classes via `loader_pin`) via the `cratonvm_types::mirror_pin`
    // propagation the GC marker consults for exactly this — mirroring
    // `loader_pin`'s instance→loader edge in the opposite direction, since
    // CratonVM's synthetic `ClassLoader` model has no heap-traceable
    // `ClassLoader.classes` bookkeeping to make plain reachability of the
    // mirror alone sufficient. A mirror whose loader turns out unreachable
    // this cycle is pruned post-GC by `memory::gc::reconcile_class_mirrors`.
    //
    // Conditional mirror propagation is wired into every non-moving full
    // marker: Generational's non-moving young/old closure, G1's initial/final
    // full-mark closure, and ZGC-real's mark-sweep closure. Ordinary
    // Generational moving collections and G1 evacuation pauses retain the
    // conservative unconditional roots because side-table addresses can move
    // during those scans. `conditional_loader_metadata` selects exactly these
    // safe full-mark windows.
    //
    // Classification note: whether to skip unconditional rooting MUST use a
    // PERMANENT signal — `class_manager`'s `ClassLoaderId::UserDefined(_)`,
    // set once when the class is registered and never cleared — NOT
    // `defining_loader_for` (a mutable liveness side-table pruned by
    // `gc_reconcile_defining_loaders` the moment a loader is confirmed dead).
    // Using the mutable signal here is a trap that reintroduces this exact
    // bug: the instant a user-defined class's own loader legitimately dies
    // and its `defining_loader_store` entry is pruned, `defining_loader_for`
    // starts returning `None` for it — indistinguishable from "always was a
    // built-in class" — which would flip this loop to root it
    // UNCONDITIONALLY forever from that point on (verified: this exact
    // mistake measured `expected:8 actual:9`, i.e. no improvement at all,
    // because the JSP-eviction test's whole point is a loader dying mid-run).
    // `defining_loader_for` remains the right call for `mirror_pin`'s OWN
    // bookkeeping below (rebuild_mirror_pins / gen_heap.rs) — there it
    // legitimately means "does this class currently have a live pairing,"
    // which is exactly what that machinery wants.
    //
    // gen r5w6/conc10: the young mirrors this step deferred ONLY because of
    // `CRATONVM_GEN_YOUNG_MIRROR_DEFER`; non-zero obliges step 14w to promise
    // the in-place young cycle.
    let mut young_mirrors_deferred = 0usize;
    precise_sections[2].0 = roots.len();
    {
        let class_mirrors = shared.classes.class_mirrors.read();
        // TEMP-DIAG (CRATONVM_DBG_MIRRORPIN): name the DECISION, not just the
        // outcome. "The mirror was still marked" is compatible with both
        // "rooted unconditionally here" and "genuinely reachable from a live
        // edge", and those want opposite fixes.
        let mirror_dbg = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_MIRRORPIN").is_some();
        if mirror_dbg {
            eprintln!(
                "[DBG_MIRRORPIN] roots: conditional_metadata={conditional_metadata} mirrors={}",
                class_mirrors.len()
            );
        }
        if conditional_metadata {
            let cm = shared.classes.class_manager.read();
            // WHICH MIRRORS DOES THE REPLACEMENT EDGE ACTUALLY NAME?
            //
            // The deferral below drops a mirror from the root set on the
            // promise that `mirror_pin` propagation will reach it from its
            // defining loader. The two halves were keyed off DIFFERENT facts
            // and could therefore disagree:
            //
            //   * this loop defers on `ClassLoaderId::UserDefined`, the
            //     permanent classification set when the class is registered;
            //   * `mirror_pin` is populated by `get_or_create_class_mirror`
            //     only when `classloader::defining_loader_for` has a recorded
            //     pairing for that class at the moment the mirror is minted.
            //
            // A class that is `UserDefined` but has no recorded pairing is
            // deferred to a propagation with no row for it, so NOTHING roots
            // it: the collector frees a `java.lang.Class` that live bytecode
            // is still using, the allocator re-serves the address, and every
            // holder -- a `Class`-typed field, the `ldc` constant record, the
            // mirror cache itself -- then reads an unrelated object.
            // MEASURED on `module/spring-boot-flyway …
            // ResourceProviderCustomizerBeanRegistrationAotProcessorTests`
            // with `CRATONVM_DBG=mirrorpin`: 5 `add_mirror_pin` rows against
            // 752 distinct mirrors reconciled `is_marked=false`, the cache
            // dropping from 1842 entries to 1092 in one cycle, and
            // `NoSuchMethodError: 'boolean
            // java.lang.String.isAssignableFrom(java.lang.Class)'` out the
            // other end.
            //
            // Steps 2, 3 and 13 never had this hole: each RECORDS the
            // metadata pin itself and only then `continue`s, so it can never
            // skip a root it did not replace. This is that same rule, and the
            // module doc of `cratonvm_types::mirror_pin` already states the
            // invariant it restores -- "built-in/bootstrap classes' mirrors
            // are unconditionally rooted directly ... so they never need this
            // propagation" -- which is only true of a mirror the registry
            // actually names.
            let pinned_mirrors: rustc_hash::FxHashSet<usize> =
                cratonvm_types::mirror_pin::all_pinned_mirrors()
                    .into_iter()
                    .collect();
            // gen r5w6/conc10 — `CRATONVM_GEN_YOUNG_MIRROR_DEFER` (opt-in): on a
            // Generational young-collection scan this licence covers, a YOUNG
            // mirror is deferred too, and step 14w turns that into the
            // in-place promise the deferral needs (see
            // [`young_mirror_defer_licensed`]).
            let young_mirror_defer = young_mirror_defer_licensed(shared);
            for (&class_id, obj_ref) in class_mirrors.iter() {
                let is_user_defined = cm.get_class(class_id).is_some_and(|c| {
                    matches!(
                        c.loader_id,
                        crate::classloading::ClassLoaderId::UserDefined(_)
                    )
                });
                // SPB.1 residual fix (see `mirror_pin_deferrable`'s doc): a
                // mirror may be left out of the unconditional root set only
                // when SOME marker running this cycle will actually follow
                // `mirror_pin`. That holds for an OLD-GEN mirror
                // (`old_gen_gc`'s BFS) and, additionally, for a YOUNG mirror
                // whenever the cycle is certain to take the non-moving young
                // marker — which follows `mirror_pin` as an ordinary marking
                // edge (`gen_heap::mark_young_precise_object`).
                //
                // Rooting a young mirror unconditionally — as the stricter
                // `metadata_pin_deferrable` predicate used here before did —
                // permanently defeats class unloading for any loader whose
                // classes' mirrors have not been promoted yet, because a
                // mirror's `classLoader` field is a real heap edge back to its
                // defining loader. Doc 26 / `TestDefaultInstanceManager`: the
                // evicted JSP's `Class` was the ONLY root-held anchor of the
                // entire retained JSP-compiler graph, and an explicit
                // `System.gc()` was the run's first collection, so nothing had
                // ever been promoted.
                let deferrable = shared
                    .mem
                    .heap
                    .mirror_pin_deferrable(obj_ref.as_ptr() as usize);
                // gen r5w6/conc10: only a mirror the heap itself would root
                // (young, on a cycle it cannot promise is in place) is
                // widened; every other answer is the heap's.
                let widened = young_mirror_defer && !deferrable;
                let deferrable = deferrable || widened;
                if mirror_dbg && is_user_defined {
                    let name = cm
                        .get_class(class_id)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    let pinned = pinned_mirrors.contains(&(obj_ref.as_ptr() as usize));
                    eprintln!(
                        "[DBG_MIRRORPIN] roots: user class={name:?} mirror={:#x} deferrable={deferrable} pinned={pinned} => {}",
                        obj_ref.as_ptr() as usize,
                        if mirror_deferral_is_covered(is_user_defined, deferrable, pinned) {
                            "DEFERRED"
                        } else {
                            "ROOTED"
                        }
                    );
                }
                if mirror_deferral_is_covered(
                    is_user_defined,
                    deferrable,
                    pinned_mirrors.contains(&(obj_ref.as_ptr() as usize)),
                ) {
                    young_mirrors_deferred += usize::from(widened);
                    continue;
                }
                // gc-common w18-d: a non-strong hidden class's mirror, of ANY
                // loader, is left out too: it is the class's liveness, kept by
                // Java references and by its live instances. The covering
                // edge is its own `loader_pin` row, which must name this very
                // mirror (see `non_strong_hidden_deferral_is_covered`).
                // The registry lookup only for a registered class: every
                // other mirror pays one probe of the local snapshot.
                if let Some(registered) = non_strong_hidden.get(&class_id.as_u32()).copied() {
                    if non_strong_hidden_deferral_is_covered(
                        deferrable,
                        Some(registered),
                        vm_loader_pin_addr(shared, class_id.as_u32()),
                        obj_ref.as_ptr() as usize,
                    ) {
                        young_mirrors_deferred += usize::from(widened);
                        continue;
                    }
                }
                roots.push(*obj_ref);
            }
        } else {
            for obj_ref in class_mirrors.values() {
                roots.push(*obj_ref);
            }
        }
    }
    precise_sections[2].1 = roots.len();

    mark_scan_section(
        roots.len(),
        "6b: Cached proxy-dispatch `Method` objects (see `proxy_method_ca",
    );
    // 6b. Cached proxy-dispatch `Method` objects (see `proxy_method_cache`'s
    // doc comment in `class_realm.rs`) — these are meant to be shared and
    // reused across every future dispatch to the same proxy method, so they
    // must stay alive for as long as the cache entry exists.
    //
    // gc-common w2-b: CONDITIONALLY, exactly like static values (step 2), and
    // for the same reason. The key is the `$ProxyN` class, the value a
    // `Method` whose `clazz` names the proxied interface — so for a proxy a
    // user loader defined, rooting the value outright kept that loader (and
    // every class it defined) alive for the life of the VM: the row was only
    // ever dropped by `unload_dead_class_metadata`, which could never run for
    // a loader this very row retained. When the cycle's marker follows
    // `metadata_pin`, the value is pinned to the proxy class's defining loader
    // instead; a live proxy instance keeps that loader alive (`loader_pin`), so
    // a dispatch that can still happen still finds a live `Method`. With no
    // `loader_pin` row (a bootstrap/platform proxy) it is rooted as before.
    {
        let proxy_methods = shared.classes.proxy_method_cache.read();
        for (&(class_id, _, _), obj_ref) in proxy_methods.iter() {
            let addr = obj_ref.as_ptr() as usize;
            if conditional_metadata && shared.mem.heap.metadata_pin_deferrable(addr) {
                if let Some(loader) = vm_deferral_owner(shared, class_id.as_u32()) {
                    cratonvm_types::metadata_pin::add_metadata_pin(
                        shared.vm_identity,
                        loader,
                        addr,
                    );
                    continue;
                }
            }
            roots.push(*obj_ref);
        }
    }

    mark_scan_section(
        roots.len(),
        "7: System streams (System.out, System.err, System.in)",
    );
    // 7. System streams (System.out, System.err, System.in)
    //
    // try_read, NOT read: the singleton initializers (e.g.
    // `ensure_system_stdin_object`) hold the WRITE guard across allocating
    // calls, and an allocation-triggered GC on that same thread would
    // self-deadlock on the non-reentrant RwLock (observed live: instant
    // 0-output wedge at the first stress GC inside the stdin window). A
    // locked guard means the initializer is mid-population — the in-flight
    // object is covered by its native_pin_roots pin, and the cache slot is
    // not yet (or already) consistent, so skipping the scan is sound.
    {
        if let Some(g) = shared.system_out.try_read() {
            if let Some(out_ref) = *g {
                roots.push(out_ref);
            }
        }
        if let Some(g) = shared.system_err.try_read() {
            if let Some(err_ref) = *g {
                roots.push(err_ref);
            }
        }
        // gcstress residual face fix — `system_in` was missing from both this
        // root scan and the update_all_roots remap (out/err had both): the
        // cached System.in FileInputStream went stale on the first moving
        // young GC after `ensure_system_stdin_object` populated it, and every
        // later use of the cache served a dangling ObjectRef.
        if let Some(g) = shared.system_in.try_read() {
            if let Some(in_ref) = *g {
                roots.push(in_ref);
            }
        }
    }

    mark_scan_section(
        roots.len(),
        "8: Primitive type Class mirrors (int.class, boolean.class, etc",
    );
    // 8. Primitive type Class mirrors (int.class, boolean.class, etc.)
    {
        let prim_mirrors = shared.classes.primitive_mirrors.read();
        for obj_ref in prim_mirrors.values() {
            roots.push(*obj_ref);
        }
    }

    mark_scan_section(
        roots.len(),
        "8a: Canonical java.lang.Module mirrors (one per module name). Th",
    );
    // 8a. Canonical java.lang.Module mirrors (one per module name). These are
    //     long-lived singletons handed back by `Class.getModule()`; without
    //     rooting them a moving GC would reclaim/relocate them and the cache
    //     in `shared.classes.module_mirrors` would hand out a stale ref.
    {
        let module_mirrors = shared.classes.module_mirrors.read();
        for obj_ref in module_mirrors.values() {
            roots.push(*obj_ref);
        }
    }

    mark_scan_section(
        roots.len(),
        "8b: VarHandle permanent roots (B-J). VarHandles live in `static",
    );
    // 8b. VarHandle permanent roots (B-J). VarHandles live in `static final`
    //     fields and are used for lock-free CAS; without rooting them here a
    //     moving GC reclaimed them and left their static holder slots stale.
    //     gc-common w29-d: every object of every same-hash bucket is a root
    //     (`heap_realm::VarHandleRoots::values`).
    {
        let vhs = shared.mem.var_handle_roots.read();
        for obj_ref in vhs.values() {
            roots.push(*obj_ref);
        }
    }

    mark_scan_section(
        roots.len(),
        "8c: Pre-allocated singleton OutOfMemoryError — thrown on a 100%",
    );
    // 8c. Pre-allocated singleton OutOfMemoryError — thrown on a 100%-full heap
    //     when a fresh exception cannot be materialized. Must survive every GC
    //     permanently (it is held only by `SharedVm`, not any Java field), so a
    //     moving collector cannot reclaim it and leave the OOM-fallback dangling.
    {
        if let Some(oom_ref) = *shared.mem.singleton_oom.read() {
            roots.push(oom_ref);
        }
        // gcd d2/j: and the per-message preallocated errors beside it (empty
        // unless `CRATONVM_GC_PREALLOCATED_OOME_KINDS`), held the same way.
        let kinds = shared.mem.preallocated_oome.read();
        for oom_ref in kinds.values() {
            roots.push(*oom_ref);
        }
    }

    mark_scan_section(
        roots.len(),
        "8d: Cached 'main' java.lang.ThreadGroup singleton",
    );
    // 8d. Cached "main" java.lang.ThreadGroup singleton
    //     (`NativeContextImpl::get_or_create_main_thread_group`,
    //     vm/src/vm/vm_exec.rs). This mirrors the `singleton_oom`/
    //     `system_out`/`system_err`/`system_in` entries just above: the
    //     cache is a bare `SharedVm` field, not itself reachable through any
    //     other root chain at the moment it is first published (a fresh
    //     `Thread$FieldHolder.<init>` that is about to store it into
    //     `holder.group` hasn't run yet), so without scanning it here a
    //     moving GC that fires between the cache's publish and that store
    //     can reclaim/relocate the object out from under the cache. Found
    //     live via a `cratonvm-aio-dispatch-N` SIGSEGV
    //     (`is_forwarded`/`get_header:1558` inside the `FieldHolder.<init>`
    //     field-setter that consumes this very cache's value) that survived
    //     the TOCTOU claim/wait/notify fix for the same function — the
    //     claim/wait/notify fix closes the *concurrent-double-build* race,
    //     but does nothing for a *single*, correctly-built group going
    //     stale on a *later* GC once every builder/waiter has already
    //     returned. try_read (not read): `get_or_create_main_thread_group`
    //     briefly holds the write lock only for the final store (never
    //     across an allocation), so contention here is transient, but
    //     mirror the try_read convention used by the adjacent
    //     system-streams scan rather than assume that can never coincide
    //     with a GC-safepoint poll.
    {
        if let Some(g) = shared.threads.main_thread_group.try_read() {
            if let Some(tg_ref) = *g {
                roots.push(tg_ref);
            }
        }
    }

    mark_scan_section(
        roots.len(),
        "9: JNI global references — prevent GC from collecting objects h",
    );
    // 9. JNI global references — prevent GC from collecting objects held by native code.
    precise_sections[3].0 = roots.len();
    {
        shared
            .natives
            .jni_global_refs
            .lock()
            .collect_roots(&mut roots);
    }
    precise_sections[3].1 = roots.len();
    // gcd d2/g: publish the four precise sections' counts now, while their
    // index ranges still describe `roots` (nothing below edits an entry this
    // side of step 14's `jit_scan_start`). See `publish_precise_root_counts`.
    if precise_root_promote {
        publish_precise_root_counts(&roots, &precise_sections);
    }

    mark_scan_section(
        roots.len(),
        "9a: Native upcall table — each live slot holds a `target: Object",
    );
    // 9a. Native upcall table — each live slot holds a `target: ObjectRef` for the
    //     Java callback the legacy `pe_upcall_invoke` dispatch path invokes.
    //     Un-rooted, a moving GC could reclaim/relocate the target out from under
    //     a still-registered upcall (the Panama closure registry remaps its own
    //     copy, but this table's copy was previously neither scanned nor remapped
    //     — see gc.rs section 9a counterpart).
    mark_scan_section(roots.len(), "9b: JNI LOCAL references (vm-jni-roots #1)");
    // 9b. JNI LOCAL references (vm-jni-roots #1).
    //
    //     Previously only global refs (section 9) were rooted. The per-thread
    //     `JNI_LOCAL_FRAMES` stack — pushed by PushLocalFrame and every JNI
    //     accessor that hands a fresh local jobject back to native code — held
    //     raw heap pointers that were NEVER scanned. A heap object reachable
    //     only through a JNI local ref could therefore be collected mid-native-
    //     call, or left dangling at a from-space address under the moving GC.
    //     Fold this thread's active local refs into the root set here; the
    //     matching remap after a moving collection is
    //     `crate::native::jni::update_local_refs_after_gc` (gc.rs).
    crate::native::jni::collect_local_ref_roots(&mut roots);

    // (9c, the process-global keep-alive pin set -- the `gc` crate's `pinned`
    //  module -- was deleted in gc-common w4-b. Both JNI array checkouts
    //  (`GetPrimitiveArrayCritical`, `Get<Type>ArrayElements`) hold their
    //  source array through a remappable per-VM JNI global ref
    //  (`CriticalCopy::array_gref` / `ArrayElemBuffer::array_gref`), rooted by
    //  step 9; nothing else pinned through the table.)

    mark_scan_section(roots.len(), "10: Thread-local ObjectRefs — java_thread_obj");
    // 10. Thread-local ObjectRefs. `java_thread_obj`, `jit_pending_exception`
    //     and `uncaught_exception_pending` are pushed at step 4 by
    //     `push_off_frame_thread_roots`, which BOTH peer snapshot paths also
    //     call — see there for why they had to move out of this section. The
    //     async-exception slot that sat here was removed with its dead channel
    //     (interpreter round i1 wave 4).
    // The JIT's stashed deopt / exceptional frames. Those live in `jit/`
    // thread-locals — that crate cannot depend on `vm/`, so they cannot become
    // `JvmThread` fields the way the slot above did — and are reached through
    // an on-thread visitor instead. That works for the same reason the slots
    // above do: this scan already runs ON the owning thread. Paired with the
    // remap in `gc.rs`; wiring one without the other is refused by a debug
    // assertion in the visitor. See `docs/jit/deopt-thread-local-roots.md`.
    //
    // `is_object_address` rather than a bare `ObjectRef`: the stash can name
    // an address the heap no longer owns, if an earlier collection already
    // ran while it was unrooted.
    cratonvm_jit::deopt::for_each_stashed_deopt_object(|addr| {
        if let Some(obj) = shared.mem.heap.is_object_address(addr as usize) {
            roots.push(obj);
        }
    });

    mark_scan_section(
        roots.len(),
        "10b: Registry-held java.lang.Thread mirrors of every ALIVE thread",
    );
    // 10b. Registry-held java.lang.Thread mirrors of every ALIVE thread.
    //      HotSpot semantics: a thread's mirror is a strong root while the
    //      thread lives. Natives serve these raw copies back into bytecode
    //      (`enumerate_threads`, `Thread.getAllStackTraces`) and the
    //      `unpark(Thread)` reverse index is keyed by their addresses — a
    //      mirror reachable ONLY through the registry must not be collected
    //      (a collected one resurfaces as the all-zero-header invokevirtual
    //      receiver). The matching remap is
    //      `ThreadRegistry::update_thread_objs_after_gc` (gc.rs step 21).
    //
    //      Skipped when the caller appends the registry's snapshots itself
    //      (`collect_roots_registry_appended`): that list pushes the SAME set
    //      -- every alive entry's `java_thread_obj` -- again, under a registry
    //      walk of its own. gc-common w6-a,
    //      `common-b-proposal-dedupe-the-initiator-root-set.md`.
    if !REGISTRY_MIRRORS_APPENDED_BY_CALLER.with(std::cell::Cell::get) {
        for obj_ref in shared
            .threads
            .thread_registry
            .alive_thread_objects(usize::MAX)
        {
            roots.push(obj_ref);
        }
    }

    mark_scan_section(
        roots.len(),
        "11: Root snapshot (for cross-thread GC scanning)",
    );
    // 11. Root snapshot (for cross-thread GC scanning)
    {
        let snapshot = thread.root_snapshot.lock();
        for obj_ref in snapshot.iter() {
            roots.push(*obj_ref);
        }
    }

    mark_scan_section(
        roots.len(),
        "12: Scoped value bindings (JEP 446) are published by",
    );
    // 12. Scoped value bindings (JEP 446) are published by
    //     `push_off_frame_thread_roots` at step 4, together with the print
    //     buffer — the two off-frame categories every per-thread root path
    //     must agree on.

    mark_scan_section(
        roots.len(),
        "13: Resolution cache — CONSTANT_Dynamic values may hold ObjectRe",
    );
    // 13. Resolution cache — CONSTANT_Dynamic values may hold ObjectRefs
    {
        let cache = shared.classes.resolution_cache.read();
        cache.for_each_condy_root(|class_id, object| {
            if conditional_metadata
                && shared
                    .mem
                    .heap
                    .metadata_pin_deferrable(object.as_ptr() as usize)
            {
                if let Some(loader) = vm_deferral_owner(shared, class_id.as_u32()) {
                    cratonvm_types::metadata_pin::add_metadata_pin(
                        shared.vm_identity,
                        loader,
                        object.as_ptr() as usize,
                    );
                    return;
                }
            }
            roots.push(object);
        });
    }

    mark_scan_section(
        roots.len(),
        "14: NEW-1.5 — conservative scan of every active JIT spill region",
    );
    // 14. NEW-1.5 — conservative scan of every active JIT spill region on the
    //     calling thread. Each qword in a JIT frame's stack region whose value
    //     is a valid object header is reported as a root. The semispace
    //     collector consults `any_thread_in_jit()` to skip compaction while
    //     this is in flight, so a value coincidentally equal to an object
    //     address never causes a wrong relocation.
    //
    //     This is the AUTHORITATIVE root set for the current thread's collection,
    //     so it must NOT be served from the per-thread JIT-scan cache: that cache
    //     can drop a live root that appeared on the conservatively-scanned native
    //     stack since the last boundary bump (see
    //     `invalidate_scan_cache_for_gc`). Discard the cached snapshot first so
    //     this scan is a full, current walk.
    //
    //     The verifier oracle (`CRATONVM_DBG_VERIFY_OOP_MAPS`) reads this VM's
    //     type maps for the JIT frames below; a no-op when it is unarmed.
    let _oracle_maps =
        crate::jit::conservative_roots::OracleTypeMapsScope::bind(&shared.classes.type_maps);
    crate::jit::conservative_roots::invalidate_scan_cache_for_gc();
    let jit_scan_start = roots.len();
    // Moving young gen (`CRATONVM_MOVING_YOUNG`): the shadow stack now publishes a
    // COMPLETE rewritable precise root map for every live JIT frame, so the
    // conservative frame scan is normally SUPPRESSED. Running it anyway would
    // fold un-rewritable slot values into `roots` that the moving Cheney copy
    // would relocate but could not patch — and a false-positive non-oop word
    // would pin (or mis-relocate) a random object. "Once a frame is precise, it
    // must be fully precise" (default-moving-young-gen.md, Risks): rely solely
    // on the shadow stack (folded in at 14b below) plus the
    // interpreter/statics/JNI roots. If any active JIT frame cannot prove that
    // coverage, we deliberately re-enable the conservative scan and tell the
    // collector to use the non-moving sweep for this cycle. On any non-moving
    // path this scan remains the authoritative JIT root set.
    // arch-2026-07-26 (`moving-young-precise-roots`): `moving_young_enabled()`
    // is a delegation to the CODEGEN-side gate, and reading it here also
    // republishes that decision to the GC crate (which cannot call into the JIT
    // crate). Because `collect_roots` is on the path of every collection, the
    // collector can never decide to relocate against a gate the codegen
    // disagrees with. See `conservative_roots::moving_young_enabled`.
    let moving_young = crate::jit::conservative_roots::moving_young_enabled();
    crate::jit::conservative_roots::publish_moving_young_gate();
    let moving_young_osr_fallback =
        moving_young && crate::jit::conservative_roots::moving_young_osr_shadow_fallback_needed();
    if moving_young_osr_fallback {
        cratonvm_gc::gc_quiescence::set_force_non_moving_jit_roots();
        cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete_because(
            cratonvm_gc::gc_quiescence::incomplete_reason::OSR_SHADOW,
        );
    }
    // COLLECTION-authoritative refresh, not the per-thread one: this call site
    // is the one that decides whether the collector may relocate, so it must
    // also account for JIT frames owned by OTHER threads (whose registers and
    // frame slots this collection cannot rewrite). See
    // `refresh_moving_young_coverage_for_collection`. The per-thread variant
    // remains correct — and remains used — at each mutator's root-snapshot
    // deposit, where per-thread scope is exactly the right question.
    //
    // G1 MUST NOT take this branch, and the reason is that the two collectors
    // protect a JIT-held oop by opposite means.
    //
    // Precise-only is sound for a collector whose protection is REWRITING: the
    // proof says every JIT-held oop sits in a published oop map, the collector
    // moves it, and `remap_active_jit_frames` writes the new address back into
    // the slot the map named. The generational collector is that collector.
    //
    // G1's protection is PINNING, and the set it pins is built downstream of
    // this branch from the CONSERVATIVE scan's output alone (the
    // `add_pinned_jit_root` loop below is `roots[jit_scan_start..]`). Skipping
    // the scan therefore does not leave G1 with a different protection — it
    // leaves G1 with NONE, for every reference the maps do not happen to name.
    // The pause then reports `pin_addrs=0`, which reads as "there are no JIT
    // roots" and is consumed as permission to evacuate everything.
    //
    // Measured on `PolynomialTest` under `-XX:+UseG1GC --Xmx 1g`, which is one
    // young pause with two compiled frames on the chain:
    //
    //   precise_only=true  (before)  scan_added=0   pin_addrs=0   FAIL 6/6
    //   precise_only=false (after)   scan_added=42  pin_addrs>0   PASS 3/3
    //
    // Forty-two conservative roots on a stack the proof called fully covered,
    // and `CRATONVM_DBG_VERIFY_OOP_MAPS` reports in-band object addresses that
    // no map names while the compiled method's `fully_oop_covered` is `true`.
    // So the proof's guarantee is narrower than "every live reference on this
    // stack is rewritable" — which is the guarantee this branch spends. That
    // gap is a real defect in its own right and is tracked separately; this
    // term stops G1 depending on it.
    //
    // The term is spelled `is_generational()`, not `!is_g1()`, because that is
    // what its two SIBLING suppression sites already say —
    // `interpreter::update_root_snapshot` and `vm_exec::deposit_root_snapshot`
    // both open with `heap.is_generational() && moving_young_enabled() && ...`.
    // This site is the one that drifted, and the drift is the whole bug: three
    // places decide whether to trade the conservative scan away, and only two
    // of them asked which collector was going to consume the result.
    //
    // It also excludes ZGC, which likewise publishes no young-bounds table and
    // so likewise gets a vacuous coverage proof (see the fail-closed guard in
    // `conservative_roots::moving_young_unpublished_frame_oop_present`).
    //
    // `bug-g1-evacuates-live-jit-reference-20260819-FIXED.md`
    // states this restriction as though it were already implemented ("requires
    // `is_generational()`, so under G1 it is false"). It was true of the
    // siblings and false here; this is the line that makes the record true.
    //
    // `CRATONVM_G1_PRECISE_ONLY_ROOTS=1` restores the old behaviour so the
    // difference is an A/B inside one binary.
    let g1_precise_only_roots = dbg_g1_precise_only_roots();
    // TWO decisions, and they were one `&&` chain until 2026-08-21.
    //
    // `coverage_proven` is the per-cycle PROOF: did every live compiled frame
    // publish its oops on a channel this collection can rewrite? It is a fact
    // about the frames, not about the collector, and the collector-side gates
    // that consult it (`gen_heap::collect_garbage_inner`, and `zgc`'s
    // relocation refusal) need it computed on THEIR cycles to mean anything.
    //
    // `moving_young_precise_only` is the SUPPRESSION: may this collection skip
    // the conservative JIT-frame scan? That one stays generational-only. G1
    // needs the scan for its pin set and ZGC needs it as the backstop for
    // everything the precise map does not name; the restriction is what
    // `bug-g1-evacuates-live-jit-reference-20260819-FIXED.md` asked for.
    //
    // The two were computed by one short-circuiting chain, so on a G1 or ZGC
    // cycle `refresh_moving_young_coverage_for_collection()` was NEVER CALLED
    // and the published verdict stayed at the `false` — meaning *complete* —
    // that `begin_moving_young_coverage_cycle` reset it to. Anything reading it
    // was reading a proof nobody ran. Splitting them costs one verifier pass
    // per collection on the two collectors that were skipping it, and buys a
    // verdict that is earned rather than vacuous.
    //
    // Order matters: `refresh_...` must stay AHEAD of the collector test so it
    // is not short-circuited away again.
    //
    // Whether a PIN discharges a peer's coverage obligation is a question
    // about the collector, so it is answered by the collector rather than read
    // from a process-global: two live heaps would make a published capability
    // describe whichever constructed last. It is computed ahead of the proof
    // so the proof expression carries no collector test (the Generational
    // pinned-copy take-over honours pins only when its switch is on).
    let pins_discharge_coverage = shared.mem.heap.honours_conservative_pins()
        || (shared.mem.heap.is_generational()
            && cratonvm_gc::gc_quiescence::generational_takeover_pins_honoured());
    let coverage_proven = moving_young
        && !moving_young_osr_fallback
        && crate::jit::conservative_roots::refresh_moving_young_coverage_for_collection(
            pins_discharge_coverage,
        )
        && !cratonvm_gc::gc_quiescence::moving_young_coverage_incomplete();
    // `CRATONVM_DBG_RELOCATION_BLOCKERS=1` -- pair this cycle's relocation
    // verdict with the thread-state census's own statement of the obligation.
    //
    // `ThreadStateCensus::relocation_blockers()` counts threads whose
    // `ThreadExecState` maps to `RelocationRule::Forbidden`, and its doc calls a
    // non-zero answer "the shadow-side statement of the
    // `mark_moving_young_coverage_incomplete_because` obligation". Nothing
    // outside `thread_state.rs` consults `relocation_rule()`, so the obligation
    // is discharged (or not) by unrelated code paths and no assertion pairs the
    // two. A line reading `coverage_proven=true blockers=N` with N > 0 is a
    // cycle that relocated while some thread's state said it must not.
    //
    // Diagnostic only, and read per cycle rather than latched: it is one atomic
    // census walk under a read lock on a path that is already taking a
    // safepoint.
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_RELOCATION_BLOCKERS").is_some() {
        use crate::threading::ThreadExecState as TES;
        let census = crate::threading::thread_state_census();
        // The RAW count is not the answer, and the first run of this
        // diagnostic is why: `blockers=1` on all 452 decisions across four
        // reps, relocating and not, never 0. `relocation_rule()` maps
        // `VmRunning` / `JavaRunning` to `Forbidden` and the COLLECTING thread
        // is in one of them by construction, so it counts itself. Anything
        // built on `relocation_blockers() > 0` therefore fires on every cycle
        // and discriminates nothing -- including that method's own doc, which
        // calls a non-zero answer "the shadow-side statement of the
        // `mark_moving_young_coverage_incomplete_because` obligation".
        //
        // Print the per-state breakdown so the initiator can be subtracted by
        // eye and a genuine PEER blocker (`compiled_uninterruptible`, or a
        // second running thread) is visible.
        eprintln!(
            "[reloc-blockers] coverage_proven={coverage_proven} blockers={} peer_blockers={} \
(java={} vm={} native={} deopt={} compiled_uninterruptible={} parked={} blocked={}) \
moving_young={moving_young} osr_fallback={moving_young_osr_fallback} incomplete={}",
            census.relocation_blockers(),
            // The count the obligation is actually about: `Forbidden` AND
            // `may_hold_unrewritable_object_refs`, with this thread subtracted
            // so a zero is reachable. `blockers` beside it is kept only so the
            // two can be compared — see `relocation_blockers`' own doc.
            census.peer_relocation_blockers(TES::VmRunning),
            census.get(TES::JavaRunning),
            census.get(TES::VmRunning),
            census.get(TES::NativeRunning),
            census.get(TES::Deoptimizing),
            census.get(TES::CompiledUninterruptible),
            census.get(TES::SafepointParked),
            census.get(TES::NativeBlocked),
            cratonvm_gc::gc_quiescence::moving_young_coverage_incomplete(),
        );
    }
    // The SUPPRESSION is additionally opt-in as of 2026-08-21
    // (`CRATONVM_GC_PRECISE_ONLY_ROOTS=1`), and only the suppression — the proof
    // above still runs on every collector, which is the whole point of the
    // split.
    //
    // What it spends is `CompiledMethod::fully_oop_covered`, a PRESENCE test:
    // every GC-capable safepoint recorded *an* oop map. The contract written
    // above its assignment in `jit/src/x64/driver.rs` says as much — that bit is
    // the NECESSARY codegen precondition, and the runtime
    // `CRATONVM_DBG_VERIFY_OOP_MAPS` oracle is "the SUFFICIENT proof that must
    // gate the actual backstop suppression".
    //
    // That gate did not exist and could not have: the oracle runs inside
    // `scan_one_frame_precise` <- `scan_active_jit_frames`, which is the very
    // call this suppression skips. On every cycle that SPENT the bit, the
    // instrument meant to check it was not running — so no `while_covered=0`
    // reading has ever described a suppressed cycle.
    //
    // Measured before switching the default: the branch fires on ~0.1 % of
    // collections under `CRATONVM_GC_STRESS` (2 of 14 420, 31 of 46 135, 84 of
    // 70 144). The other 99.9 % already run the scan, so that bounds what it
    // saves — and verifying it costs the same frame walk as the scan it skips.
    let moving_young_precise_only = dbg_precise_only_roots()
        && coverage_proven
        && (shared.mem.heap.is_generational() || g1_precise_only_roots);
    // Round 12 wave 1 (lane g1store, W19-1): under G1 the band scans below also
    // record the words the root screen REJECTED that still lie in a live
    // region, so their pins can be published without making them roots. See
    // `conservative_roots::g1_band_reject_pins`. Armed where pins are
    // published: here, and on the two deposit paths (`update_root_snapshot`,
    // the blocked deposit in `vm_exec.rs`). The final remark also seeds the
    // gray set from every published pin (`G1Collector::remark`, r12w2).
    use crate::jit::conservative_roots::g1_band_reject_pins::Capture as RejectPinCapture;
    let reject_capture = RejectPinCapture::arm_if_wanted(&shared.mem.heap, dbg_jit_rootscan());
    // Verify first, then suppress. When the proof holds, the roots gathered to
    // establish it are dropped and the cycle proceeds precise-only exactly as
    // before; when it is refuted they are KEPT, which is simply the
    // conservative scan the suppression was trying to avoid.
    let mut moving_young_precise_only = moving_young_precise_only;
    let mut jit_scan_done = false;
    if moving_young_precise_only && crate::jit::conservative_roots::coverage_gate_active() {
        crate::memory::native_roots::rootprof::note_scan_caller(0); // gc-roots
        if crate::jit::conservative_roots::verify_active_coverage_into(&shared.mem.heap, &mut roots)
        {
            moving_young_precise_only = false;
            jit_scan_done = true;
        } else {
            roots.truncate(jit_scan_start);
        }
    }
    // A refutation recorded by any earlier cycle stands: the offending compiled
    // method is still in the code cache.
    if crate::jit::conservative_roots::coverage_oracle_refuted() {
        moving_young_precise_only = false;
    }
    if !moving_young_precise_only && !jit_scan_done {
        // `_for_collection` below, not the bare scan: it marks this as the
        // pass the collector marks from, which is what the opt-in above-chain
        // conservative band keys on (`CRATONVM_JIT_ABOVE_CHAIN_SCAN`). Inert
        // with that flag unset, which is the default — see
        // `conservative_roots::above_chain_scan_enabled` for the measurement
        // that says why it is opt-in.
        crate::memory::native_roots::rootprof::note_scan_caller(0); // gc-roots
        crate::jit::conservative_roots::scan_active_jit_frames_for_collection(
            &shared.mem.heap,
            &mut roots,
        );
    }
    // Disarm here, whatever happened above. A precise-only cycle dropped its
    // scan's roots (`roots.truncate`), so it drops the rejects' pins too.
    let reject_pins = match reject_capture {
        Some(capture) => {
            let pins = capture.finish();
            if moving_young_precise_only {
                Vec::new()
            } else {
                pins
            }
        }
        None => Vec::new(),
    };
    // G1 pin-in-place for conservative JIT roots: the generational collector
    // protects a conservatively-scanned JIT root (a register/spill slot the
    // collector cannot rewrite) by running its NON-MOVING young sweep while any
    // thread is in JIT, so nothing moves. G1 always evacuates, so it must
    // instead PIN the regions holding these roots (exclude them from the
    // collection set) — otherwise it relocates the object and the un-rewritable
    // JIT-frame slot is left dangling (the SteadyChurn `-XX:+UseG1GC` + JIT
    // wrong-result: the `live` list head, held only in a callee-saved register
    // and its canonical frame slot, went stale after the young GC moved it).
    // 2026-09-06: THE GATE ABOVE WAS THE INITIATOR'S HALF OF THE SAME STALE
    // PREMISE the two deposit paths carried (see
    // `conservative_roots::g1_only_jit_pins`). "The generational path doesn't
    // read this set" was true when it was written and stopped being true
    // twice: ZGC has withheld the PAGE of every address in
    // `pinned_jit_roots_snapshot()` since compaction shipped (2026-08-13), and
    // the generational moving-young cycle now diverts on a pin that lands in
    // young-from (`gen_heap::collect_garbage_inner`). Gated on G1, both
    // consumers saw an EMPTY set on their own collectors -- which for a
    // pin-by-value consumer does not read as "nothing to pin", it reads as
    // "relocate everything".
    //
    // `publish_pinned_jit_roots` rather than the per-address
    // `add_pinned_jit_root` this replaces: the add form ACCUMULATES into this
    // thread's entry and only G1 ever clears the map, so on the other two
    // backends it would grow a set of pre-move addresses across every cycle of
    // the run. Replace semantics keep the initiator's pins describing THIS
    // cycle, exactly as the deposit paths already do for parked peers, and
    // publishing an empty vector removes the entry rather than leaving the
    // previous cycle's behind. For G1 the two are equivalent (it clears per
    // cycle, and this is its only add site).
    if !crate::runtime::interpreter::gc_and_alloc::g1_only_jit_pins() || shared.mem.heap.is_g1() {
        let mut addrs: Vec<usize> = roots[jit_scan_start..]
            .iter()
            .map(|r| r.as_ptr() as usize)
            .collect();
        // `CRATONVM_DBG_JIT_ROOTSCAN=1` — each DISTINCT address this pass is
        // about to pin, with the two facts that decide whether it needs to be
        // pinned at all. `scan_added=15 unrewritable=4` on the line below is a
        // pair of totals over different domains (words, then objects) and the
        // arithmetic between them is not available anywhere: 15 words dedupe to
        // 5 addresses, and which of THOSE five are held only through rewritable
        // storage is the whole of what a narrowed pin set could drop.
        if dbg_jit_rootscan() {
            let mut distinct: Vec<usize> = addrs.clone();
            distinct.sort_unstable();
            distinct.dedup();
            let mut line = String::new();
            for a in &distinct {
                line.push_str(&format!(
                    " 0x{a:x}(unrew={},movable={})",
                    cratonvm_gc::gc_quiescence::is_unrewritable_jit_root(*a) as u8,
                    cratonvm_gc::gc_quiescence::is_movable_jit_root(*a) as u8,
                ));
            }
            eprintln!(
                "[jitpins] words={} distinct={} unrew_set={} movable_set={}{}",
                addrs.len(),
                distinct.len(),
                cratonvm_gc::gc_quiescence::unrewritable_jit_root_count(),
                cratonvm_gc::gc_quiescence::movable_jit_root_count(),
                line,
            );
        }
        // The pins the rejected band words owe (empty unless G1 armed the
        // capture above). Pins only: they never enter `roots`.
        publish_g1_band_reject_pins(
            &shared.mem.heap,
            &reject_pins,
            &mut addrs,
            dbg_jit_rootscan(),
        );
        cratonvm_gc::gc_quiescence::publish_pinned_jit_roots(&addrs);
    } else if shared.mem.heap.is_generational() {
        // gen r4w3/young2 (2026-09-23): `CRATONVM_GC_G1_ONLY_JIT_PINS` on the
        // generational heap. The pins stay withheld (that is the flag), but
        // the fact that THIS thread scanned is still recorded: peers' bumps
        // were erased by `begin_moving_young_coverage_cycle`, so without this
        // `conservative_jit_scans()` read 0 and the collector's
        // `unrewritable_conservative_jit_roots` divert went false behind live
        // unrewritable peer words. The count is now what the default arm above
        // produces. NOT on ZGC, which pins by value and refuses on `== 0`:
        // with its pins withheld, "nobody published" is the truthful answer
        // there. See
        // `docs/internal/gc/gengc-plumbing-conservative-scan-reset-ordering-FIXED-20260924.md`.
        cratonvm_gc::gc_quiescence::note_conservative_jit_scan();
    }
    // `CRATONVM_DBG_VERIFY_REG_OOP_MAPS=1` — every blind-spill word the
    // register oop mask dropped, re-checked against the root set that was
    // actually built. It has to run here rather than in the band scan: the
    // question is whether anything ELSE names the object, and inside the scan
    // the answer is still being assembled.
    crate::jit::conservative_roots::verify_excluded_band_words(&roots, &shared.mem.heap);
    // 14a5. A5 CONSERVATIVE FRAME PASS — the second half of step 1, run here
    // because this is the first point in the pass at which the answer is known.
    //
    // The collector runs its non-moving sweep for either of two reasons:
    // `gc_quiescence::is_active()` (a registered JIT frame) or
    // `unregistered_jit_frame_on_stack()` (A5 — a compiled frame live without a
    // `JitEntryGuard`, e.g. the compiled entry point while an interpreted
    // callee runs). Step 1's `conservative_locals` — the probe that recovers an
    // object reference from a frame slot whose CompactValue tag was lost, and
    // that suppresses the per-bci local-liveness filter — keyed on the FIRST
    // reason only. On an A5-only cycle the sweep therefore freed on
    // `GC_FLAG_MARKED` while the pass that exists to widen its root set was
    // off: exactly the asymmetry
    // `docs/internal/springboot/bindabletests-bytebuddy-receiver-reclaimed-under-gc-stress-20260908.md`
    // names as its most specific lead.
    //
    // Widening step 1's predicate is not the repair, and that page says why:
    // `unregistered_jit_frame_on_stack()` is cleared at the top of this
    // function and re-set only by the JIT scan a few lines above, so at step 1
    // it is false by construction — reading it there answers about the wrong
    // cycle. Running the probe HERE answers about THIS one.
    //
    // Soundness — the same argument `conservative_locals_enabled` makes, and it
    // holds strictly harder here. A conservatively-recovered root may be a
    // pointer-shaped `long`, so it is only safe where nothing is relocated. The
    // scan above did not merely set the A5 flag; it also called
    // `mark_moving_young_coverage_incomplete_because(UNREGISTERED_JIT_FRAME)`,
    // and `collect_garbage_inner` honours that through
    // `divert_for_incomplete_moving_coverage`, which overrides even
    // `CRATONVM_DBG_FORCE_MOVING`. So on every cycle this branch fires, the
    // young collection provably does not move, and a false positive can only
    // over-retain.
    //
    // G1/ZGC: unreachable. Both take their own root paths, and the A5 flag is
    // the generational collector's signal — but the pass is keyed on the flag,
    // not on the backend, so the extra roots simply pin (G1 pins conservative
    // JIT roots out of the CSet; ZGC withholds their page), which is the same
    // over-retention. It is deliberately NOT folded into `pinned_jit_roots`
    // above: these are interpreter frame slots, not compiled-frame band words.
    if a5_frame_pass_engages(conservative_locals) {
        mark_scan_section(
            roots.len(),
            "14a5: A5 conservative interpreter-frame pass (unregistered JIT frame)",
        );
        let before = roots.len();
        conservative_frame_pass(shared, thread, &mut roots);
        // ENGAGEMENT, in the house style: a repair whose cycle count is
        // unknown cannot be argued about later. `cycles` is how often the A5
        // path took the non-moving sweep without step 1's probe; `roots` is
        // what this pass added on top of the tag-filtered scan.
        a5_frame_pass::note(roots.len() - before);
    }
    // 14w. METADATA WEAK MODE MUST NOT MEET A MOVING CYCLE (gen r4w3/rooting).
    //
    // Step 1 published `metadata_pin` weak mode on `conditional_metadata`, and
    // on the Generational arm that licence is `is_active() ||
    // explicit_full_gc_sweeps_young_in_place()`. Weak mode is sound only on a
    // cycle that sweeps young IN PLACE: the moving Cheney closure does not
    // follow `metadata_pin`, so a young metadata object reachable only through
    // it is neither rooted (weak mode left it out) nor copied, and from-space
    // is then reset under the side table.
    //
    // The explicit-full-GC term is a promise the collector keeps by
    // construction. The `is_active()` term was NOT: under moving-young it made
    // the cycle non-moving only through `divert_non_moving`'s term 4
    // (`!CRATONVM_GC_NO_PEER_PIN_DIVERT && conservative_jit_scans() > 0`),
    // which `CRATONVM_GC_NO_PEER_PIN_DIVERT=1` or
    // `CRATONVM_GC_PRECISE_ONLY_ROOTS=1` (no conservative scan, so a zero
    // count) disarms. Asked HERE — after the JIT scan, where this cycle's scan
    // count is known, as step 14a5 is — and, only when term 4 will not fire,
    // turned into a promise through the same per-cycle channel the OSR and
    // coverage fallbacks use. On the default configuration term 4 already
    // fires on every such cycle, so nothing changes there; see
    // `docs/internal/gc/gengc-r4w2-youngpolicy-loader-metadata-weak-mode-can-meet-a-moving-cycle-FIXED-20260923.md`.
    //
    // gcd d2/g (d1/d's consistency note): `term4_armed` below is a SUBSET of
    // the collector's term 4 inputs -- the collector also needs
    // `has_conservative_roots` and `!young_pin_ledger_clears_term4()`
    // (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`), and
    // `CRATONVM_GEN_PINNED_YOUNG_COPY` runs a term-4-only cycle as a pinned
    // MOVING one. So on those opt-ins this step can skip the promise on a cycle
    // that then moves. That is not a lost root: no YOUNG value is ever left out
    // of this scan on the strength of term 4. Every young deferral asks
    // `metadata_pin_deferrable` / `mirror_pin_deferrable`, which answer yes for
    // a young address only when `young_marker_follows_side_tables()` holds
    // (an explicit in-place full GC, or moving-young off with a compiled frame
    // live -- both divert on their own, and neither is term 4), or step 14w'
    // below forces the promise itself (`CRATONVM_GEN_YOUNG_MIRROR_DEFER`). An
    // OLD deferred value is safe on a moving young cycle (its consumer is the
    // old-gen BFS). So this promise is insurance for a future young deferral
    // keyed on the licence alone; aligning `term4_armed` with the collector
    // would only force every weak-mode cycle non-moving under those two
    // opt-ins and defeat them.
    if matches!(
        shared.config.gc_algorithm,
        crate::config::GcAlgorithm::Generational
    ) && metadata_weak_mode_needs_in_place_promise(
        conditional_metadata,
        cratonvm_gc::gc_quiescence::explicit_full_gc_sweeps_young_in_place(),
        cratonvm_gc::gc_quiescence::moving_young_enabled(),
        !dbg_no_peer_pin_divert() && cratonvm_gc::gc_quiescence::conservative_jit_scans() > 0,
    ) {
        cratonvm_gc::gc_quiescence::set_force_non_moving_jit_roots();
    }
    // 14w'. gen r5w6/conc10 — `CRATONVM_GEN_YOUNG_MIRROR_DEFER`: a YOUNG mirror
    // step 6 left out on that licence is reached only through the non-moving
    // young marker's `mirror_pin` edges (`seed_mirror_pins_of_old_owners`, and
    // `scan_young_object` from a young loader), so this cycle MUST take that
    // marker. Unlike 14w's weak mode this cannot lean on term 4: with
    // `CRATONVM_GEN_PINNED_YOUNG_COPY` a term-4-only cycle runs the MOVING
    // branch in pinned mode, which follows no side table. The forced flag is
    // `divert_for_incomplete_moving_coverage`, which also keeps the pinned copy
    // out (`term4_alone`) and survives `CRATONVM_DBG_FORCE_MOVING` (excluded
    // anyway by the licence). Only when a mirror was actually deferred.
    if young_mirrors_deferred > 0 {
        cratonvm_gc::gc_quiescence::set_force_non_moving_jit_roots();
        shared
            .mem
            .concurrent_gc_state
            .note_young_mirrors_deferred(young_mirrors_deferred);
    }
    // `CRATONVM_DBG_JIT_ROOTSCAN=1` — one line per COLLECTION naming why this
    // cycle's JIT pin set came out the size it did.
    //
    // G1's `[g1][PINS]` line reports the pin count at the point the collection
    // set is built, which is downstream of every way the count can be zero and
    // cannot tell them apart: the scan was skipped (`precise_only`), the scan
    // ran against an empty chain (`chain=0`), or the scan walked a band and
    // every candidate failed `is_object_address` (`chain>0 added=0`). Those are
    // three different defects and the collector-side line reads identically for
    // all three. See
    // `bug-g1-evacuates-live-jit-reference-20260819-FIXED.md`.
    if dbg_jit_rootscan() {
        let frames = crate::jit::conservative_roots::active_compiled_frames();
        let labels: Vec<&str> = frames.iter().map(|f| &*f.label).collect();
        // `osr_reason=` is a CUMULATIVE snapshot (bad_shadow_layout,
        // debug_disabled, bad_map_coverage, missing_exact_rbp), not a
        // per-cycle value — this line already runs at a cost only a debug
        // build accepts, and OSR fallback is checked per JIT-entry-chain
        // frame, not per collection, so there is no single-cycle count to
        // report. Read the growth between two lines, or the tail line before
        // exit. See `conservative_roots::osr_fallback_reason` for what each
        // bucket means.
        let (osr_bad_shadow, osr_debug_disabled, osr_bad_map, osr_missing_rbp) =
            crate::jit::conservative_roots::osr_fallback_reason::snapshot();
        // Same treatment for the ACTIVE_FRAME_MAP / PARENT_FRAME_MAP pair: one
        // reason code each, four ways to earn it. Cumulative, like the OSR
        // buckets above — read the growth between two lines.
        let (fc_no_slot, fc_misaligned, fc_no_map, fc_incomplete, fc_ok) =
            crate::jit::conservative_roots::frame_coverage_reason::snapshot();
        // The register-oop mask, both sides. Emit-side causes are cumulative
        // over compiles, consume-side over frame walks; read the growth.
        let ro_e = cratonvm_jit::x64::reg_oop_mask_cause::snapshot();
        let ro_u = crate::jit::conservative_roots::reg_oop_mask_census();
        let ro_o = crate::jit::conservative_roots::reg_oop_mask_oracle();
        // Which scan path ran, and what the band partition published. A pin
        // census shows neither, and both decide whether a pin is arguable.
        let bp = crate::jit::conservative_roots::band_path::snapshot();
        eprintln!(
            "[bandpath] bands={} fallback={} foreign_innermost={} a5_sweeps={} \
             a5_roots={} a5_frames={} published=(movable={} unrewritable={} \
             remapped_not_pinned={})",
            bp.0,
            bp.1,
            bp.2,
            bp.3,
            bp.4,
            bp.5,
            crate::jit::conservative_roots::movable_band_root_count(),
            crate::jit::conservative_roots::unrewritable_band_root_count(),
            crate::jit::conservative_roots::remapped_not_pinned_count(),
        );
        eprintln!(
            "[regoop] emit=(disabled={} staged={} desync={} inexact={} windows={} inline={} \
             dataflow={} PUBLISHED={}) use=(masked={} unmasked={} regwords={} \
             deadspill={} outgoing={}) \
             oracle=(words={} reachable={} UNREACHABLE={} walk_incomplete={}) frame_liveness={:?}",
            ro_e.0,
            ro_e.1,
            ro_e.2,
            ro_e.3,
            ro_e.4,
            ro_e.5,
            ro_e.6,
            ro_e.7,
            ro_u.0,
            ro_u.1,
            ro_u.2,
            ro_u.3,
            ro_u.4,
            ro_o.0,
            ro_o.1,
            ro_o.2,
            ro_o.3,
            crate::jit::conservative_roots::frame_liveness_census(),
        );
        // Engagement counter for the cross-thread coverage handshake, printed
        // beside the verdict it produces so any claim about it carries the
        // number of cycles it actually decided.
        let (xt_accepted, xt_refused, xt_deposits) =
            cratonvm_gc::gc_quiescence::peer_coverage_counters();
        // Engagement for the 2026-08-24 inline-cache frame-record republish:
        // the COMPILE-time count of optimizing-tier IC sites that got it. A
        // zero means that repair decided nothing in this run, whatever the
        // frame_cov numbers beside it say.
        let ic_fr_sites = cratonvm_jit::ic_frame_republish_sites();
        eprintln!(
            "[jitroots] precise_only={precise_only} proven={proven} moving_young={moving_young} \
             osr_fb={osr_fb} osr_reason=(shadow={osr_bad_shadow} debug={osr_debug_disabled} \
             map_coverage={osr_bad_map} exact_rbp={osr_missing_rbp}) \
             frame_cov=(no_slot={fc_no_slot} misaligned={fc_misaligned} no_map={fc_no_map} \
             incomplete={fc_incomplete} ok={fc_ok}) \
             xt_cov=(accepted={xt_accepted} refused={xt_refused} deposits={xt_deposits}) \
             ic_fr_sites={ic_fr_sites} \
             incomplete={incomplete} reason={reason} chain={chain} \
             any_jit={any_jit} \
             scan_added={added} unrewritable={unrewritable} is_g1={is_g1} \
             ybounds={ybounds} heaps={heaps} \
             bounds_representative={bounds_representative} \
             frames={labels:?}",
            precise_only = moving_young_precise_only,
            proven = coverage_proven,
            osr_fb = moving_young_osr_fallback,
            incomplete = cratonvm_gc::gc_quiescence::moving_young_coverage_incomplete(),
            // WHICH obligation blocked it, not just that one did. `incomplete`
            // alone cannot separate "this workload has a compiled frame the JIT
            // never described" from "a peer thread happened to be in compiled
            // code at this safepoint", and those want completely different
            // work: the first is a codegen gap, the second is the cross-thread
            // coverage handshake `moving-young-precise-roots.md`
            // specifies and nobody has built.
            reason = cratonvm_gc::gc_quiescence::incomplete_reason::label(
                cratonvm_gc::gc_quiescence::moving_young_incomplete_reason(),
            ),
            chain = crate::jit::conservative_roots::current_thread_jit_depth(),
            any_jit = crate::jit::conservative_roots::any_thread_in_jit(),
            added = roots.len() - jit_scan_start,
            // How many of this cycle's band roots came through a word no
            // channel can rewrite, and are therefore pinned whatever the
            // movable set claims. `scan_added>0 unrewritable=0` is a frame
            // whose every live reference sits in verified storage; a non-zero
            // count is the gap
            // `moving-young-left-a-callee-saved-register-image-unrewritten`
            // measured, being closed rather than merely detected.
            unrewritable = cratonvm_gc::gc_quiescence::unrewritable_jit_root_count(),
            is_g1 = shared.mem.heap.is_g1(),
            // Whether `gen_heap::JIT_REGION_BOUNDS` carries a young pair at
            // all. It is what the moving-young frame-band verifier's residency
            // test reads, so `ybounds=false` means that verifier inspected the
            // bands and classified nothing as young — a vacuous pass, not a
            // clean frame. See `published_young_regions_are_live`.
            ybounds = cratonvm_gc::gen_heap::published_young_regions_are_live(),
            // How many heaps are alive, and whether the published tables can
            // describe them all. Both tables are single-tenant (slot-0
            // ownership), so `heaps>1` means one heap's addresses answer
            // `false` to every residency test in the process — the same vacuous
            // verifier `ybounds=false` reports, reached from a table that IS
            // published and is simply about the other heap. Printed beside
            // `ybounds` because `ybounds=true heaps=2` is the reading neither
            // number gives on its own.
            heaps = cratonvm_gc::gen_heap::live_relocatable_heaps(),
            bounds_representative =
                cratonvm_gc::gen_heap::published_bounds_represent_every_live_heap(),
        );
    }

    mark_scan_section(
        roots.len(),
        "14b: Shadow-stack precise roots (CRATONVM_SHADOW_STACK). JIT code",
    );
    // 14b. Shadow-stack precise roots (CRATONVM_SHADOW_STACK). JIT code pushes
    //      every live oop (locals AND operand-stack entries) onto this thread's
    //      shadow stack immediately before a GC-capable call. Unlike the
    //      conservative scan above, every slot here is — by construction —
    //      exactly one object reference, so it is BOTH a precise mark root and
    //      a *rewritable* root (the matching post-move rewrite is
    //      `thread.shadow_stack.remap` in gc.rs). We still validate each value
    //      via `is_object_address`: a defensive guard against a stale slot that
    //      an abnormal unwind left above `top` is impossible (scan is bounded by
    //      `top`), but a slot holding null / a not-yet-stored value reads as a
    //      non-object and is harmlessly skipped.
    if crate::jit::conservative_roots::shadow_stack_enabled() {
        // spring-bug-10 experiment: `CRATONVM_SHADOW_PIN` publishes the
        // shadow-stack oops as PINNED marking roots instead of MOVABLE ones.
        // Rationale: the shadow stack is a LIFO scanned only between a
        // push-before-call and its reload-after-call, so an oop is pinned only
        // *transiently* (during one GC-capable call) — once popped it promotes
        // normally on a later GC. Pinned avoids the movable path's
        // evacuate→remap→reload (the null-reload SIGSEGV) entirely. The doc's
        // "pinning OOMs bt18" was reasoned for pinning the WHOLE operand stack,
        // never measured for this transient minimal set; this gate lets us
        // measure it directly.
        let pin = crate::jit::conservative_roots::shadow_pin_roots() || moving_young_osr_fallback;
        // spring-bug-10 diagnostic: log this thread's shadow-stack depth at each
        // GC. A monotonically growing depth across collections means the JIT
        // `top` is DRIFTING (a push without a paired reload) — which makes the
        // pop-only reload read above the real data and corrupt a home register.
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SHADOW_DEPTH").is_some() {
            let d = thread.shadow_stack.depth();
            if d > 0 {
                eprintln!(
                    "[SHADOW_DEPTH] tid={:?} depth={} pin={}",
                    thread.thread_id, d, pin
                );
            }
        }
        thread.shadow_stack.for_each_value(|v| {
            if let Some(obj_ref) = shared.mem.heap.is_object_address(v) {
                roots.push(obj_ref);
                if !pin {
                    // B-K kafka fix: shadow-stack oops are precise AND
                    // *rewritable* (`thread.shadow_stack.remap` rewrites them
                    // after a move, then the JIT's post-safepoint reload
                    // refreshes the register). So they may be EVACUATED rather
                    // than pinned — publish movable so the non-moving sweep's
                    // selective promotion drains them.
                    cratonvm_gc::gc_quiescence::add_movable_jit_root(obj_ref.as_ptr() as usize);
                }
            }
        });
    }

    // 15-21. VM/native side tables. The registry owns every built-in scan and
    // its matching relocation callback as one entry. The historical notes
    // below document why each registered source is a root.
    let __pre_native_roots = roots.len();
    {
        // Every loader-conditional row defers under step 1's licence, the one
        // step 14w promised on (`loader_metadata_licence`, gc-common w5-b).
        let _licence = NativeScanLicence::enter(shared.vm_identity, conditional_metadata);
        crate::memory::native_roots::scan_all_roots(shared, &mut roots);
    }
    // CRATONVM_DBG_ROOT_SOURCE, second question: which channel hands the
    // collector an address that is NOT an object start?
    //
    // The G1 evacuation-failure path learned the hard way that it gets them —
    // its kept-seed and ref-scan guards both reject INTERIOR pointers
    // (`object_start + 8`, or 0x60 into an array's payload) that arrived in
    // this very array. `is_object_address` is the strict probe, so a young or
    // mid-initialisation object can legitimately fail it; the index split is
    // what makes the output actionable. Everything below `__pre_native_roots`
    // came from this thread's frames/stack, everything at or above it from a
    // NAMED source that `root_source_of` can name.
    if crate::memory::native_roots::root_attribution_on() {
        for (i, r) in roots.iter().enumerate() {
            let addr = r.as_ptr() as usize;
            if shared.mem.heap.is_object_address(addr).is_none() {
                eprintln!(
                    "[ROOT-NOT-OBJECT] idx={i}/{} addr=0x{addr:x} phase={} source={:?}",
                    roots.len(),
                    if i < __pre_native_roots {
                        "frame/thread"
                    } else {
                        "native-source"
                    },
                    crate::memory::native_roots::root_source_of(addr),
                );
            }
        }
    }
    if let Some(t0) = __rp_t0 {
        let ns = t0.elapsed().as_nanos();
        if ns >= 20_000_000 {
            eprintln!(
                "[rootprof] collect_roots took {}ms roots={}",
                ns / 1_000_000,
                roots.len()
            );
        }
    }

    mark_scan_section(
        roots.len(),
        "15: Round-9 CRIT GC-correctness fix: process-global Integer.valu",
    );
    // 15. Round-9 CRIT GC-correctness fix: process-global Integer.valueOf
    //     (-128..=127) and Boolean.TRUE/FALSE caches. These live in
    //     `native-builtins/src/lang_math.rs` and previously used
    //     `thread_local!`, which (a) violated the JLS-mandated
    //     cross-thread `==` identity for boxed primitives and (b) was
    //     invisible to the GC root scanner — under a moving collector the
    //     cached ObjectRefs would point at relocated or reclaimed memory
    //     after the first compaction.
    mark_scan_section(
        roots.len(),
        "15a: Unsafe / Class$Atomic synthetic-offset side stores. These ho",
    );
    // 15a. Unsafe / Class$Atomic synthetic-offset side stores. These hold live
    //      `ObjectRef`s that exist in NO heap slot (the synthetic-offset scheme
    //      services load/CAS/store from a Rust-side map when the field's real
    //      heap slot is unknown to our layout), so they are unreachable through
    //      the heap graph. Chief offender: `Class$Atomic.casReflectionData`
    //      stows the `SoftReference<ReflectionData>` here — without rooting it a
    //      young GC reclaims the still-live SoftReference (all-zero header) and
    //      the reflection subgraph hanging off it decays (the Tomcat DoHead
    //      start/stop corruption flood, first victim always a
    //      `java/lang/ref/SoftReference`). Remap companion in `gc.rs`
    //      (`gc_update_unsafe_side_store_refs`). Both halves now run as
    //      `native_roots` rows: `unsafe-side-store` (THIS VM's rows only
    //      since gc-common w7-a) and `class-atomic-slots` (the `Class$Atomic`
    //      slots, per VM and loader-conditional since w3-b).

    mark_scan_section(
        roots.len(),
        "16: Round-9 perf + GC fix: process-global LambdaMetafactory Call",
    );
    // 16. Round-9 perf + GC fix: process-global LambdaMetafactory CallSite
    //     cache. Cached CallSites and their bootstrap-arg ObjectRef keys
    //     must stay live across collections; the matching post-compaction
    //     remap lives in `gc.rs` (`gc_update_lambda_callsite_cache_refs`).

    mark_scan_section(
        roots.len(),
        "16a: Zero-capture lambda proxy singleton cache (companion to the",
    );
    // 16a. Zero-capture lambda proxy singleton cache (companion to the
    //      LambdaMetafactory CallSite cache in step 16 above, but for the
    //      cached proxy INSTANCE of a non-capturing lambda rather than the
    //      CallSite metadata). Lives in `vm/src/runtime/invokedynamic.rs`.

    mark_scan_section(
        roots.len(),
        "17: Overlay-backed collections (LinkedList / LinkedHashMap / Tre",
    );
    // 17. Overlay-backed collections (LinkedList / LinkedHashMap / TreeMap /
    //     TreeSet). These keep backing arrays + nodes in Rust side-tables,
    //     invisible to ordinary field tracing. The moving/G1/ZGC paths retain
    //     the conservative global-root behavior because their marker has no
    //     stable-address overlay propagation. The Generational non-moving
    //     marker can propagate an overlay only after its OWNER has been marked
    //     (gen_heap.rs). An explicit System.gc() also selects that non-moving
    //     full-GC path so it can reclaim an otherwise-dead overlay owner rather
    //     than globally rooting its transient compiler graph.

    mark_scan_section(
        roots.len(),
        "18: Singleton built-in class loaders (app / platform). These syn",
    );
    // 18. Singleton built-in class loaders (app / platform). These synthetic
    //     `ClassLoader` objects live ONLY in process-global mutexes in
    //     `native-builtins/src/classloader.rs`, invisible to every scan above.
    //     Without rooting them, a moving young GC reclaims/relocates the cached
    //     loader and `getClassLoader()` returns a stale `ObjectRef` whose slot
    //     was reused — BouncyCastle `ClassUtil.loadClass`'s receiver then reads
    //     as a String OID ("Not able to load any cryptoProvider", intermittent
    //     / heap-size dependent). Remap companion in `gc.rs`
    //     (`gc_update_loader_singleton_refs`).

    mark_scan_section(
        roots.len(),
        "18a: Process-global `System.getenv()` / `System.getProperties()`",
    );
    // 18a. Process-global `System.getenv()` / `System.getProperties()`
    //      singletons cached in `native-builtins/src/lang_system.rs`. Like the
    //      class loaders above, these synthetic objects live ONLY in process-
    //      global mutexes, invisible to every scan above; without rooting them a
    //      moving young GC reclaims/relocates the cached Map/Properties and the
    //      next `getenv()`/`getProperties()` returns a stale `ObjectRef`. Remap
    //      companion in `gc.rs` (`gc_update_system_singleton_refs`).

    mark_scan_section(
        roots.len(),
        "18b: Process-global Locale caches (cached default Locale + synthe",
    );
    // 18b. Process-global Locale caches (cached default Locale + synthetic
    //      Locale side-tables) in native-builtins. Same stale-pointer hazard as
    //      the class loaders: a moving young GC reclaims/relocates the cached
    //      synthetic `java/util/Locale` while `Locale.getDefault()` keeps
    //      handing back the stale ObjectRef → "Stale pointer … java/util/Locale"
    //      → SIGSEGV (TestServerInfo / TestSwallowAbortedUploads). Remap
    //      companion in `gc.rs` (`gc_update_locale_refs`).

    mark_scan_section(
        roots.len(),
        "18c: `java.lang.ClassValue` memoization cache (BUG-W) — cached",
    );
    // 18c. `java.lang.ClassValue` memoization cache (BUG-W) — cached
    //      `computeValue(Class)` results live only in a process-global
    //      side-table in `native-builtins/src/phases_late.rs`, invisible to
    //      every scan above. Without rooting them a moving GC can
    //      reclaim/relocate a cached value while a later `ClassValue.get()`
    //      keeps handing back the stale `ObjectRef`. Remap companion in
    //      `gc.rs` (`gc_update_classvalue_cache_refs`).

    mark_scan_section(
        roots.len(),
        "19: JBoss MSC container-held service objects. The `ServiceContai",
    );
    // 19. JBoss MSC container-held service objects. The `ServiceContainer` Rust
    //     state machine references Java objects (the `Service` instance whose
    //     `start()`/`stop()` we invoke, the synthetic `ServiceController`
    //     mirror, the child `ServiceTarget`, the in-flight `StartContext`) only
    //     through a process-global side-table in
    //     `native-builtins/src/jboss_msc.rs`, invisible to every scan above.
    //     Without rooting them a moving GC reclaims/relocates a held service
    //     and the next `invoke_virtual(service, "start", ...)` is a
    //     use-after-free. Remap companion in `gc.rs`
    //     (`gc_update_msc_service_refs`).

    mark_scan_section(
        roots.len(),
        "19b: Round-4 B4: java.util.logging / JBoss LogManager mirrors — t",
    );
    // 19b. Round-4 B4: java.util.logging / JBoss LogManager mirrors — the
    //     LogManager / Logger / LogContext singletons and the attachments
    //     table are cached as raw addresses in process-global side-tables in
    //     `native-builtins/src/logmanager.rs` with no GC visibility. Root them
    //     so a moving GC cannot reclaim/relocate a cached logger out from under
    //     a later native lookup (use-after-free). Remap companion in `gc.rs`
    //     (`gc_update_logmanager_refs`).

    mark_scan_section(
        roots.len(),
        "20: Class-level annotation-proxy identity cache. The per-class",
    );
    // 20. Class-level annotation-proxy identity cache. The per-class
    //     `getAnnotation(X)` / `getDeclaredAnnotations()` proxies are cached in
    //     a process-global side-table in `native-builtins/src/lang_class.rs`
    //     (so repeated reads return the same instance, matching HotSpot's
    //     `Class.annotationData`), invisible to the field scan above. Root them
    //     so a moving young GC cannot reclaim/relocate a cached proxy out from
    //     under a later `getAnnotation` read (use-after-free). Remap companion
    //     in `gc.rs` (`gc_update_annotation_proxy_refs`).

    //     Synthetic `com.sun.net.httpserver` server registry: each registered
    //     `HttpHandler` ObjectRef lives only in a native map (no Java-heap edge
    //     once the test drops the returned HttpContext), so a moving young GC
    //     would otherwise reclaim/relocate it and the per-request dispatcher
    //     would invoke a stale receiver (NoSuchMethodError java/lang/Object.handle).
    //     Remap companion in `gc.rs` (`gc_update_re10_handler_refs`).

    //     Process-global InetAddress side table (`net_phase_e.rs`): each
    //     synthetic InetAddress mirror's (hostName, ipAddress) pair lives only
    //     in this ObjectRef-keyed map. Without rooting it, a moving young GC
    //     that relocates a live mirror leaves it keyed on a vacated from-space
    //     slot, and getHostAddress()/getAddress()/toString() silently fall
    //     back to reporting "0.0.0.0" (ES
    //     InetAddressRandomBinaryDocValuesRangeQueryTests CONTAINS-query false
    //     negative). Remap companion in `gc.rs` (`gc_update_inet_addr_refs`).

    //     Process-global DatagramSocket side tables (`net_phase_e.rs`):
    //     `ds_side_table` (fd / closed / timeout / connected / broadcast /
    //     reuse) and `ds_peer_table` (the connected peer) are both keyed by the
    //     socket mirror. A relocated mirror makes `ds_get` miss and answer the
    //     all-defaults `ds_default()` — fd = -1 on a live, connected socket, so
    //     send() fails as "DatagramSocket: closed" — and a dead mirror's entry
    //     survives for the allocator to collide with, handing a fresh object a
    //     dead socket's descriptor. Remap companion in `gc.rs`
    //     (`gc_update_ds_refs`).

    //     NIO SelectionKey table: channel/selector/attachment/key_obj ObjectRefs
    //     live only in `sk_table`; remap was already wired (gc.rs
    //     `sk_table_update_after_gc`) but the root SCAN was missing, so a key
    //     reachable only through sk_table could be swept before the remap ran.
    //     ScheduledThreadPoolExecutor pending runnables (stored as relocatable
    //     addresses; remap companion `scheduled_pump::gc_update_scheduled_refs`).
    //     XNIO IoFuture notifier/attachment/result refs held across allocations
    //     until the future settles (remap companion
    //     `xnio_async::gc_update_xnio_future_refs`).
    //     FFM/Panama upcall targets: the Java MethodHandle/lambda a libffi
    //     trampoline dispatches to, reachable only through the leaked upcall
    //     userdata (Step 5 GAP C; remap companion in `gc.rs`).
    //     TLS SSLContext TrustManager[] objects (t27_tls::ctx_trust_managers_table),
    //     held so the post-handshake trust check (OCSP/CRL revocation checkers,
    //     custom X509TrustManagers) can still call them long after
    //     SSLContext.init returned (remap companion
    //     `t27_tls::gc_update_tls_ctx_trust_manager_refs` in `gc.rs`).
    //     TLS SSLContext KeyManager[] objects (t27_tls::ctx_key_managers_table),
    //     held so `JavaKeyManagerResolver::resolve` can synchronously consult
    //     the real `KeyManager.chooseClientAlias` mid-handshake, long after
    //     SSLContext.init returned (remap companion
    //     `t27_tls::gc_update_tls_ctx_key_manager_refs` in `gc.rs`).
    //     The process-wide default SSLContext (t27_tls::default_ssl_context_slot),
    //     installed by SSLContext.setDefault(ctx) and returned by later
    //     SSLContext.getDefault() calls -- held long after setDefault
    //     returned (remap companion `t27_tls::gc_update_default_ssl_context_ref`
    //     in `gc.rs`).

    //     ForkJoinTask done/result side-table. Real-JDK ForkJoin overrides cache
    //     task results in Rust state keyed by task identity; cached Object
    //     results live in no heap slot, so a GC between `submit` and `get` must
    //     root them here. Remap companion in `gc.rs`.

    mark_scan_section(
        roots.len(),
        "21: Uniform native-root registry (driven above, via",
    );
    // 21. Uniform native-root registry (driven above, via
    //     `native_roots::scan_all_roots`). A native subsystem holding
    //     ObjectRefs in a side-table belongs in `native_roots::VM_ROOT_SOURCES`
    //     rather than hand-wired as another `gc_scan_*` call here — the table
    //     row cannot compile without both the scan and the remap half. Every
    //     source receives the OWNING `SharedVm`, so one backed by a static must
    //     key that static on `shared.vm_identity`. The matching post-move remap
    //     is `native_roots::remap_all_roots` in `gc.rs`.

    // 22. Reference processor: a pending, QUEUED `Reference` that one of THIS
    // thread's interpreter frames still holds in a local the per-bci liveness
    // filter dropped. See [`push_pending_references_held_by_frames`] for why it
    // is only those, and no longer every pending queued `Reference` in the
    // process.
    if !conservative_locals {
        push_pending_references_held_by_frames(thread, shared, &mut roots);
    }
    mark_scan_section(roots.len(), "22: Reference processor pending references");

    // 22b. Cleaner actions emitted but not yet run. `run_cleaner_actions`
    // DEFERS them across collections while a JIT helper holds the thread
    // borrow, and the queue holds raw addresses no marker can see: a sweeping
    // collection (Generational young/old sweep, G1 remark cleanup, ZGC sweep)
    // freed a cleanable whose last strong holder had just died — `native-io`'s
    // direct-buffer cleanable is held only by its buffer — and the drain then
    // wrote slots 0/1 of, and invoked, whatever the allocator put there next.
    // `CleanerThread::update_after_gc` (called from
    // `process_references_after_gc`) is the moving half; this is the sweeping
    // half. Same shape as the finalizer queue (`finalizable_roots` →
    // `FinalizerThread::pending_addresses`), which cannot be reused: that
    // channel is the RESURRECTION input, and `force_gc_from_native` finalizes
    // whatever it reports dead. Lock order: `pending_actions` (L3) is taken
    // after `ref_processor` (L7) above was released.
    // (gc-common w2-b, applying `handoff-d-root-pending-cleaner-actions.md`.)
    for addr in shared.mem.cleaner_thread.pending_addresses() {
        // SAFETY: submitted by `process_references_after_gc` /
        // `g1_remark_process_references` from a live, shape-screened object and
        // relocated by `CleanerThread::update_after_gc` on every collection
        // since.
        roots.push(unsafe { ObjectRef::from_raw(addr as *mut u8) });
    }
    mark_scan_section(roots.len(), "22b: Cleaner actions pending");

    if let Some(w) = crate::memory::gc::watch_addr() {
        let rooted = roots.iter().any(|o| o.as_ptr() as usize == w);
        eprintln!(
            "[watch] roots GC#{} addr=0x{w:x} rooted={rooted} frames={} pins={}",
            shared.mem.heap.collection_count(),
            thread.frames.len(),
            thread.native_pin_roots.len()
        );
        if !rooted {
            for (i, f) in thread.frames.iter().enumerate().rev().take(8) {
                eprintln!(
                    "[watch]   frame#{i} {}.{} pc={} stack_len={} locals={}",
                    f.class_name(),
                    f.method_name(),
                    f.pc,
                    f.stack.len(),
                    f.locals_len()
                );
            }
        }
    }
    // gcd d5/r (flip-gate item 5 of the precise-root page): seal the table
    // step 9 published with this scan's FINAL length, so the young sweep's
    // sealed take can refuse a table that does not describe the list it was
    // handed. A no-op when nothing was published (the flag off).
    cratonvm_gc::gen_heap::seal_precise_root_values(roots.len());
    // Feed the next collection's pre-size. Monotone: see `ROOT_COUNT_HINT`.
    let seen = roots.len().min(ROOT_HINT_CEILING);
    if seen > ROOT_COUNT_HINT.load(std::sync::atomic::Ordering::Relaxed) {
        ROOT_COUNT_HINT.store(seen, std::sync::atomic::Ordering::Relaxed);
    }
    roots
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classloading::ClassId;
    use crate::config::VmConfig;
    // FIX: `Heap` import removed — the locals/stack root tests now allocate
    // from `shared.mem.heap` (the heap actually scanned) instead of an orphan
    // `Heap::new()`, so the standalone `Heap` type is no longer referenced.
    use crate::runtime::frame::Frame;
    use crate::threading::jvm_thread::ThreadId;
    use std::sync::Arc;

    fn test_shared_vm() -> Arc<SharedVm> {
        Arc::new(SharedVm::new(VmConfig::default()))
    }

    /// **THE THIRD INPUT IS THE ONE THAT MATTERS.**
    ///
    /// A `UserDefined` class whose mirror `mirror_pin` does not name has no
    /// replacement root, so deferring it roots it NOWHERE. That is the shape
    /// measured on `module/spring-boot-flyway …
    /// ResourceProviderCustomizerBeanRegistrationAotProcessorTests`
    /// (`CRATONVM_DBG=mirrorpin`): 5 `add_mirror_pin` rows against 752
    /// distinct mirrors reconciled `is_marked=false`, and
    /// `NoSuchMethodError: 'boolean
    /// java.lang.String.isAssignableFrom(java.lang.Class)'` out the far end.
    #[test]
    fn a_user_defined_mirror_with_no_mirror_pin_row_is_not_deferred() {
        assert!(
            !mirror_deferral_is_covered(true, true, false),
            "a mirror the propagation does not name must stay in the root set; deferring it frees a `java.lang.Class` that is still in use"
        );
    }

    /// The deferral still happens when the propagation really can reach it —
    /// this is what keeps `class_loader_unload_regression` green.
    #[test]
    fn a_user_defined_mirror_the_propagation_names_is_still_deferred() {
        assert!(mirror_deferral_is_covered(true, true, true));
    }

    /// Both other inputs stay load-bearing: a built-in-loader class is rooted
    /// outright, and so is any mirror on a cycle whose marker will not follow
    /// the side tables.
    #[test]
    fn a_builtin_mirror_and_an_unfollowed_cycle_are_both_rooted() {
        assert!(!mirror_deferral_is_covered(false, true, true));
        assert!(!mirror_deferral_is_covered(true, false, true));
    }

    /// gc-common w18-d: a non-strong hidden class's mirror is deferred only
    /// when the class is registered at THIS mirror and its instance edge
    /// (`loader_pin`) names the same mirror, on a cycle whose marker follows
    /// that edge. Any one missing roots it.
    #[test]
    fn a_non_strong_hidden_mirror_is_deferred_only_with_its_instance_edge() {
        const M: usize = 0x18D_7000;
        assert!(non_strong_hidden_deferral_is_covered(true, Some(M), Some(M), M));
        assert!(!non_strong_hidden_deferral_is_covered(false, Some(M), Some(M), M));
        assert!(!non_strong_hidden_deferral_is_covered(true, None, Some(M), M));
        assert!(
            !non_strong_hidden_deferral_is_covered(true, Some(M), Some(0x18D_7100), M),
            "an instance edge naming a loader (or a stale address) does not cover the mirror"
        );
        assert!(!non_strong_hidden_deferral_is_covered(true, Some(0x18D_7100), Some(M), M));
        assert!(!non_strong_hidden_deferral_is_covered(true, Some(M), None, M));
    }

    /// gc-common w18-d
    /// (`common-w8e-non-strong-hidden-classes-unload-only-with-their-loader`):
    /// the root scan of a VM with a registered non-strong hidden class.
    /// Its mirror is NOT an unconditional root on a cycle that follows the
    /// side edges (a strong built-in class's mirror still is), it IS a root
    /// while an activation of the class is on the stack, and the class's
    /// instance edge names the mirror, so a live instance keeps the class.
    #[test]
    fn a_non_strong_hidden_mirror_lives_by_its_instances_and_activations() {
        let shared = test_shared_vm();
        let vm = shared.vm_identity;
        // Class ids no class manager of this VM holds and no other test uses.
        let hidden = ClassId::new(0x7ff1_8d01);
        let strong = ClassId::new(0x7ff1_8d02);
        let hidden_mirror = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let strong_mirror = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        {
            let mut mirrors = shared.classes.class_mirrors.write();
            mirrors.insert(hidden, hidden_mirror);
            mirrors.insert(strong, strong_mirror);
        }
        cratonvm_native_builtins::classloader::register_non_strong_hidden_mirror(
            vm,
            hidden.as_u32(),
            addr_of(hidden_mirror),
            None,
        );
        assert_eq!(
            vm_loader_pin_addr(&shared, hidden.as_u32()),
            Some(addr_of(hidden_mirror)),
            "a live instance of the class must mark its mirror"
        );

        let mut thread = JvmThread::new(ThreadId(0), "test");
        let conditional = conditional_loader_metadata(&shared);
        let deferrable = shared
            .mem
            .heap
            .mirror_pin_deferrable(addr_of(hidden_mirror));
        let roots = collect_roots(&shared, &thread);
        assert!(roots.contains(&strong_mirror), "a built-in class's mirror stays rooted");
        if conditional && deferrable {
            assert!(
                !roots.contains(&hidden_mirror),
                "a non-strong hidden class's mirror must be able to die while its loader lives"
            );
        } else {
            assert!(roots.contains(&hidden_mirror), "no deferral licensed: rooted as before");
        }

        thread.frames.push(Frame::new(
            hidden,
            "w18d/Hidden/0x1".to_string(),
            "run".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            4,
            4,
            &[],
        ));
        assert!(
            collect_roots(&shared, &thread).contains(&hidden_mirror),
            "an activation of the class keeps its mirror, whatever the cycle"
        );

        cratonvm_native_builtins::classloader::forget_vm_loader_singletons(vm);
    }

    /// gc-common w19-a
    /// (`common-w18d-peer-interpreter-activations-do-not-keep-their-class`):
    /// `frame_class_owners`, the list both peer deposits publish, names each
    /// frame's defining user loader ONCE however many frames share it, and a
    /// non-strong hidden class's mirror. A bootstrap frame adds nothing, and
    /// another VM's row at the same class id is not this VM's owner.
    #[test]
    fn frame_class_owners_names_each_activation_owner_once() {
        use cratonvm_native_builtins::classloader as cl;
        let shared = test_shared_vm();
        let other = test_shared_vm();
        let vm = shared.vm_identity;
        assert_ne!(vm, other.vm_identity);
        // Class ids no class manager holds and no other test uses.
        let user = ClassId::new(0x7ff1_9a01);
        let hidden = ClassId::new(0x7ff1_9a02);
        let loader = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let mirror = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let other_loader = other.mem.heap.alloc_object(ClassId::new(0), 0);
        cl::register_defining_loader(vm, user.as_u32(), loader);
        cl::register_defining_loader(other.vm_identity, hidden.as_u32(), other_loader);
        cl::register_non_strong_hidden_mirror(vm, hidden.as_u32(), addr_of(mirror), None);
        let frame = |cid: ClassId| {
            Frame::new(
                cid,
                "w19a/Owner".to_string(),
                "run".to_string(),
                "()V".to_string(),
                None,
                vec![],
                vec![],
                4,
                4,
                &[],
            )
        };

        let mut thread = JvmThread::new(ThreadId(0), "w19a-owners");
        assert!(frame_class_owners(vm, &thread).is_empty(), "no frame, no owner");
        thread.frames.push(frame(ClassId::new(0)));
        thread.frames.push(frame(user));
        thread.frames.push(frame(hidden));
        thread.frames.push(frame(user));
        let owners = frame_class_owners(vm, &thread);
        // gce e1/j: the in-native deposit's per-class path gives the same list.
        assert_eq!(frame_class_owners_per_class(vm, &thread), owners);
        assert_eq!(
            crate::jit::conservative_roots::with_frozen_band_memo(None, || {
                frame_class_owners(vm, &thread)
            }),
            owners
        );

        cl::forget_vm_loader_singletons(vm);
        cl::forget_vm_loader_singletons(other.vm_identity);
        cratonvm_types::loader_pin::forget_vm_loader_pins(vm);
        cratonvm_types::loader_pin::forget_vm_loader_pins(other.vm_identity);

        assert_eq!(
            owners.iter().filter(|o| **o == loader).count(),
            1,
            "two activations of one user-loader class publish its loader once: {owners:?}"
        );
        assert!(owners.contains(&mirror), "a non-strong hidden activation keeps its mirror");
        assert!(
            !owners.contains(&other_loader),
            "another VM's defining row at the same class id is not this VM's owner"
        );
        assert_eq!(owners.len(), 2, "{owners:?}");
    }

    /// **THE OWNER'S OWN SCAN ROOTS ITS NATIVE ALLOCATION POOL.**
    ///
    /// Both peer snapshots published `native_alloc_pool`; `collect_roots` did
    /// not, so a collection initiated by the pool's owner (the common case — the
    /// pool is refilled exactly when young space is exhausted) left up to 2047
    /// pre-allocated old-generation objects unrooted, and an old-generation
    /// sweep freed blocks the pool would later hand out as fresh objects.
    #[test]
    fn the_native_allocation_pool_is_an_initiator_root() {
        let shared = test_shared_vm();
        let pooled = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let mut thread = JvmThread::new(ThreadId(0), "test");

        assert!(
            !collect_roots(&shared, &thread).contains(&pooled),
            "an object with no reference must not already be a root, or the \
             assertion below proves nothing"
        );

        thread.native_alloc_pool.push(pooled);
        assert!(
            collect_roots(&shared, &thread).contains(&pooled),
            "a pooled object is live until the pool hands it out; the owner's \
             scan must root it exactly as both peer snapshots do"
        );
    }

    /// **THE TWO PARKED THROWABLES ARE PUBLISHED BY THE SHARED OFF-FRAME HELPER.**
    ///
    /// `push_off_frame_thread_roots` is the one list all three per-thread root
    /// paths call. `jit_pending_exception` and `uncaught_exception_pending` were
    /// pushed by the initiator's §10 only, so a peer-initiated collection (the
    /// shutdown-hook case: the launcher sleeps in a blocking region while hook
    /// threads allocate) never marked them.
    #[test]
    fn the_parked_throwables_are_off_frame_roots_on_every_path() {
        // SAFETY: never dereferenced — the helper only copies addresses.
        let jit_exc = unsafe { ObjectRef::from_raw(0x7000usize as *mut u8) };
        let uncaught = unsafe { ObjectRef::from_raw(0x7100usize as *mut u8) };
        let mut thread = JvmThread::new(ThreadId(0), "test");
        thread.jit_pending_exception = Some(jit_exc);
        thread.uncaught_exception_pending = Some(uncaught);

        let mut roots = Vec::new();
        push_off_frame_thread_roots(&thread, &mut roots);
        assert!(roots.contains(&jit_exc), "jit_pending_exception must be published");
        assert!(
            roots.contains(&uncaught),
            "uncaught_exception_pending must be published"
        );
    }

    /// **ANOTHER VM'S LOADER PIN NEVER UNROOTS THIS VM'S VALUE**
    /// (`common-w4b-loader-pin-collision-unroots-another-vms-statics.md`,
    /// gc-common w4-b).
    ///
    /// `loader_pin` is keyed by the bare class id, so VM A's user-loader class
    /// and VM B's class at the same id share one row. A deferral in B keyed by
    /// A's loader address is followed by no marker of B's: the value is simply
    /// unrooted. The deferral must use a pin only when THIS VM's own
    /// defining-loader table agrees.
    #[test]
    fn a_colliding_class_ids_loader_pin_is_used_only_by_its_own_vm() {
        let vm_a = test_shared_vm();
        let vm_b = test_shared_vm();
        assert_ne!(vm_a.vm_identity, vm_b.vm_identity);
        // A class id no other test registers.
        let cid: u32 = 0x7ff0_4c01;
        let loader_a = vm_a.mem.heap.alloc_object(ClassId::new(0), 0);
        cratonvm_native_builtins::classloader::register_defining_loader(
            vm_a.vm_identity,
            cid,
            loader_a,
        );
        assert_eq!(
            cratonvm_types::loader_pin::loader_pin_addr(cid),
            Some(loader_a.as_ptr() as usize),
            "the process-wide row names A's loader (non-vacuity)"
        );

        assert_eq!(
            vm_loader_pin_addr(&vm_a, cid),
            Some(loader_a.as_ptr() as usize),
            "A's own pin must still license A's deferral, or class unloading regresses"
        );
        assert_eq!(
            vm_loader_pin_addr(&vm_b, cid),
            None,
            "B has no user-loader class at this id: its value must be ROOTED, not \
             pinned to a loader in A's heap that B's marker never visits"
        );

        // gc-common w5-b: when B DOES have a user-loader class at the same id,
        // each VM defers to its OWN loader whichever wrote last -- the w4-b
        // cross-check rooted the earlier writer's values instead.
        let loader_b = vm_b.mem.heap.alloc_object(ClassId::new(0), 0);
        cratonvm_native_builtins::classloader::register_defining_loader(
            vm_b.vm_identity,
            cid,
            loader_b,
        );
        assert_eq!(
            vm_loader_pin_addr(&vm_a, cid),
            Some(loader_a.as_ptr() as usize),
            "A's pin survives B's registration at the same id"
        );
        assert_eq!(
            vm_loader_pin_addr(&vm_b, cid),
            Some(loader_b.as_ptr() as usize)
        );

        cratonvm_types::loader_pin::forget_vm_loader_pins(vm_a.vm_identity);
        cratonvm_types::loader_pin::forget_vm_loader_pins(vm_b.vm_identity);
    }

    /// gen r5w4/conc8
    /// (`gengc-r5w3-unload7-orphaned-class-statics-deferred-to-a-dead-loader`):
    /// a class with a user `loader_pin` row that the unload transaction would
    /// never unload (held by the store, `ClassManager::unloads_on_hint` false)
    /// must not have its loader-conditional roots deferred: they would die with
    /// the loader while the class, and its slots, stay. The scan's screen is
    /// what refuses it; this fixture's id is NOT in the store, so (orchestrator,
    /// r5w4 merge) the class manager itself does not refuse it -- an absent id
    /// has nothing the transaction would leave behind -- and the refusal is
    /// driven through the screen.
    #[test]
    fn a_class_the_unload_never_removes_does_not_defer_its_roots() {
        let shared = test_shared_vm();
        let vm = shared.vm_identity;
        // A class id no class manager holds and no other test uses.
        let cid: u32 = 0x7ff2_c801;
        let loader = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        cratonvm_native_builtins::classloader::register_defining_loader(vm, cid, loader);
        let pin = vm_loader_pin_addr(&shared, cid);
        let unscreened = vm_deferral_owner(&shared, cid);
        let refused_set = classes_whose_roots_never_defer(&shared);
        let screened_refusing = {
            let _screen = DeferralScreen::enter(vm, [cid].into_iter().collect());
            vm_deferral_owner(&shared, cid)
        };
        let screened_allowing = {
            let _screen = DeferralScreen::enter(vm, rustc_hash::FxHashSet::default());
            vm_deferral_owner(&shared, cid)
        };
        let foreign_screen = {
            let _screen = DeferralScreen::enter(vm ^ 1, rustc_hash::FxHashSet::default());
            vm_deferral_owner(&shared, cid)
        };
        let restored = DEFERRAL_REFUSED.with(|c| c.borrow().is_none());

        cratonvm_native_builtins::classloader::forget_vm_loader_singletons(vm);
        cratonvm_types::loader_pin::forget_vm_loader_pins(vm);

        assert_eq!(pin, Some(addr_of(loader)), "the pin exists (non-vacuity)");
        assert_eq!(
            unscreened,
            Some(addr_of(loader)),
            "an id the store does not hold is not refused by the class manager"
        );
        assert!(!refused_set.contains(&cid), "nor named by the scan's set");
        assert_eq!(screened_refusing, None);
        assert_eq!(
            screened_allowing,
            Some(addr_of(loader)),
            "a screened scan defers exactly what its set allows"
        );
        assert_eq!(
            foreign_screen,
            Some(addr_of(loader)),
            "another VM's screen does not answer for this VM (the class manager does)"
        );
        assert!(restored, "every screen restores the previous (empty) one");
    }

    /// **A JIT MEMO CACHE IS ROOTED WHILE PRESENT AND EMPTIED AFTER A PAUSE**
    /// (`docs/internal/gc-common-round-20260923/common-w2b-per-thread-jit-caches-retain-dropped-maps-FIXED-20260923.md`, gc-common
    /// w4-b).
    ///
    /// Three halves, and each is load-bearing:
    ///
    /// * a present entry IS a root -- dropping that would free a `node` the
    ///   next probe returns (the TOMCAT-JNDIREALM-JIT.3 use-after-free);
    /// * at the generation it was validated at, neither the publish-side nor
    ///   the probe-side gate touches it -- the memo still hits within one;
    /// * one pause later the publish-side gate EMPTIES it -- that is the
    ///   retention fix: a thread that stopped probing no longer keeps a
    ///   dropped `HashMap` alive until 32 newer entries evict it.
    #[test]
    fn a_jit_memo_cache_is_rooted_while_present_and_emptied_after_a_pause() {
        use crate::threading::jvm_thread::{JitHashMapStringNodeCacheEntry, StringCaseCacheEntry};
        // SAFETY: never dereferenced — the scan only copies addresses.
        let fake = |a: usize| unsafe { ObjectRef::from_raw(a as *mut u8) };
        let shared = test_shared_vm();
        let mut thread = JvmThread::new(ThreadId(0), "test");
        // Start where a thread that just probed is: validated at "now".
        validate_jit_memo_caches(&shared, &mut thread);
        assert_eq!(thread.jit_memo_epoch, jit_memo_epoch_now(&shared));
        thread
            .jit_hashmap_string_node_cache
            .push(JitHashMapStringNodeCacheEntry {
                map: fake(0x7200),
                node: fake(0x7300),
                key_object: Some(fake(0x7400)),
                key: String::new(),
                mod_count_slot: 0,
                mod_count: 0,
                chm_generation: None,
                chm_segment_id: None,
            });
        thread.string_case_cache.push(StringCaseCacheEntry {
            source: fake(0x7500),
            locale: None,
            upper: false,
            first: fake(0x7600),
            second: fake(0x7700),
            next: false,
        });

        let mut roots = Vec::new();
        push_off_frame_thread_roots(&thread, &mut roots);
        let roots: Vec<usize> = roots.iter().map(|o| o.as_ptr() as usize).collect();
        for a in [0x7200usize, 0x7300, 0x7400, 0x7500, 0x7600, 0x7700] {
            assert!(
                roots.contains(&a),
                "a present memo entry (0x{a:x}) must be a root, or the next hit \
                 returns a reclaimed node"
            );
        }

        // Same generation: both gates are no-ops.
        drop_stale_jit_memo_caches(&shared, &mut thread);
        validate_jit_memo_caches(&shared, &mut thread);
        assert_eq!(
            (thread.jit_hashmap_string_node_cache.len(), thread.string_case_cache.len()),
            (1, 1),
            "a cache validated at the current generation must still hit"
        );

        // A pause completes: the next publish empties both and re-arms.
        shared
            .mem
            .gc_barrier
            .gc_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        drop_stale_jit_memo_caches(&shared, &mut thread);
        assert!(
            thread.jit_hashmap_string_node_cache.is_empty() && thread.string_case_cache.is_empty(),
            "a memo from before the last pause must not be published as a root again"
        );
        assert_eq!(thread.jit_memo_epoch, jit_memo_epoch_now(&shared));

        // The probe-side gate does the same for a thread that probes first.
        thread.string_case_cache.push(StringCaseCacheEntry {
            source: fake(0x7800),
            locale: None,
            upper: true,
            first: fake(0x7900),
            second: fake(0x7A00),
            next: false,
        });
        shared
            .mem
            .gc_barrier
            .gc_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        validate_jit_memo_caches(&shared, &mut thread);
        assert!(
            thread.string_case_cache.is_empty(),
            "a probe after a pause must not be served from before it"
        );
    }

    /// **A SYNCHRONIZED FRAME'S MONITOR OBJECT IS A ROOT OF ITS OWNER'S SCAN.**
    ///
    /// `update_all_roots` rewrote `monitor_on_exit` and the blocked deposit
    /// published it, but the initiator's scan relied on the receiver still
    /// sitting in local 0 — which a static synchronized method's lock object
    /// never does.
    #[test]
    fn a_frames_monitor_on_exit_is_an_initiator_root() {
        let shared = test_shared_vm();
        let lock = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let mut frame = Frame::new(
            ClassId::new(0),
            "TestClass".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            5,
            &[],
        );
        frame.monitor_on_exit = Some(lock);
        thread.frames.push(frame);

        assert!(
            collect_roots(&shared, &thread).contains(&lock),
            "the object a frame will monitorexit on pop must survive the collection"
        );
    }

    /// **A FRAME'S BLOCK MONITORS ARE ROOTS OF ITS OWNER'S SCAN** (JVMS
    /// §2.11.10 structured locking, interpreter round i1 wave 23, lane L7).
    ///
    /// Hand-written bytecode (`dup; monitorenter`) keeps the locked object in
    /// no local; the frame's `held_monitors` record is then its only copy, and
    /// `monitorexit` finds the entry by address.
    #[test]
    fn a_frames_held_monitors_are_initiator_roots() {
        let shared = test_shared_vm();
        let lock = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let mut thread = JvmThread::new(ThreadId(0), "test");
        let mut frame = Frame::new(
            ClassId::new(0),
            "TestClass".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            5,
            &[],
        );
        frame.held_monitors.push(lock);
        thread.frames.push(frame);

        assert!(
            collect_roots(&shared, &thread).contains(&lock),
            "an object a frame's monitorenter locked must survive the collection"
        );
    }

    fn addr_of(o: ObjectRef) -> usize {
        o.as_ptr() as usize
    }

    /// **A QUEUED `Reference` NOTHING HOLDS IS GARBAGE** (gc-common w2-b).
    ///
    /// Step 22 used to root every pending queued `Reference` in the process,
    /// which kept `java.io.ClassCache.CacheRef` — a queued `SoftReference`
    /// whose strong `type` field names the `Class` it caches for — and with it
    /// every user class loader that ever reached `ObjectStreamClass.lookup`,
    /// alive until memory pressure. `class_loader_unload_regression` failed on
    /// all three collectors because of it. See
    /// [`push_pending_references_held_by_frames`].
    #[test]
    fn a_queued_reference_nothing_holds_is_not_a_root() {
        let shared = test_shared_vm();
        let reference = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let referent = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let queue = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        shared.mem.ref_processor.lock().discover_reference(
            cratonvm_gc::ReferenceType::Soft,
            addr_of(reference),
            addr_of(referent),
            Some(addr_of(queue)),
        );
        // Non-vacuity: the processor does report it as pending — it is the
        // ROOT SCAN that must decline it.
        assert!(
            shared
                .mem
                .ref_processor
                .lock()
                .pending_reference_object_addresses()
                .contains(&addr_of(reference)),
            "the fixture must register a pending queued reference"
        );
        let thread = JvmThread::new(ThreadId(0), "test");
        assert!(
            !collect_roots(&shared, &thread).contains(&reference),
            "an unreachable registered Reference must die (java.lang.ref: it is \
             then never enqueued), not be kept alive with every field it holds"
        );
    }

    /// **A USER-LOADER PROXY'S CACHED `Method` IS PINNED TO ITS LOADER, NOT
    /// ROOTED** (gc-common w2-b) — whenever the cycle's marker follows
    /// `metadata_pin`. Rooting it outright kept the proxy's loader alive
    /// forever (the `Method`'s `clazz` names the proxied interface), and the
    /// row was only ever dropped by the unload that rooting prevented.
    #[test]
    fn a_user_loader_proxy_method_is_pinned_to_its_loader_not_rooted() {
        let shared = test_shared_vm();
        let loader = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let method = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let bootstrap_method = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        // A class id no other test registers a loader pin for.
        let proxy_cid = ClassId::new(0x7ff0_2b01);
        let bootstrap_proxy_cid = ClassId::new(0x7ff0_2b02);
        // Through the authoritative side table (which also writes the
        // `loader_pin` row): a deferral uses a pin only when THIS VM's own
        // defining-loader row agrees (`vm_loader_pin_addr`, gc-common w4-b).
        cratonvm_native_builtins::classloader::register_defining_loader(
            shared.vm_identity,
            proxy_cid.as_u32(),
            loader,
        );
        {
            let mut cache = shared.classes.proxy_method_cache.write();
            cache.insert((proxy_cid, "go".into(), "(I)I".into()), method);
            cache.insert((bootstrap_proxy_cid, "go".into(), "(I)I".into()), bootstrap_method);
        }
        let thread = JvmThread::new(ThreadId(0), "test");
        let conditional = conditional_loader_metadata(&shared);
        let roots = collect_roots(&shared, &thread);
        assert!(
            roots.contains(&bootstrap_method),
            "a proxy with no defining-loader pin has nothing to defer to and stays rooted"
        );
        if conditional {
            assert!(
                !roots.contains(&method),
                "a user-loader proxy's Method must not be an unconditional root"
            );
            assert!(
                cratonvm_types::metadata_pin::roots_for_loader(addr_of(loader))
                    .is_some_and(|pinned| pinned.contains(&addr_of(method))),
                "...it must be pinned to the proxy class's defining loader instead"
            );
        } else {
            assert!(roots.contains(&method), "no deferral licensed: rooted as before");
        }
        cratonvm_types::loader_pin::forget_vm_loader_pins(shared.vm_identity);
        cratonvm_types::metadata_pin::replace_metadata_pins(shared.vm_identity, &[]);
    }

    /// **A DEFERRED CLEANER ACTION IS A ROOT** (handoff-d, applied in w2-b).
    ///
    /// The queue holds raw addresses across collections; without this a
    /// sweeping collection frees the cleanable and the later drain writes into
    /// and invokes reclaimed memory.
    #[test]
    fn a_deferred_cleaner_action_is_an_initiator_root() {
        let shared = test_shared_vm();
        let cleanable = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        shared.mem.cleaner_thread.submit_action(addr_of(cleanable));
        let thread = JvmThread::new(ThreadId(0), "test");
        assert!(
            collect_roots(&shared, &thread).contains(&cleanable),
            "a cleanable whose action is queued but not yet run must survive the collection"
        );
        // Once drained (the action ran) it is no longer this scan's business.
        let _ = shared.mem.cleaner_thread.drain_actions();
        assert!(!collect_roots(&shared, &thread).contains(&cleanable));
    }

    /// **…BUT ONE A FRAME STILL HOLDS IN A LIVENESS-DEAD LOCAL IS KEPT.**
    ///
    /// `r = new WeakReference<>(x, q); System.gc(); q.remove()` with `r` never
    /// read again: the per-bci liveness filter drops `r`, where HotSpot's
    /// interpreter keeps it. The rescue is limited to QUEUED references — a
    /// queue-less one has nothing to deliver, so a dead local holding it stays
    /// dead.
    #[test]
    fn a_queued_reference_in_a_liveness_dead_local_is_still_a_root() {
        let shared = test_shared_vm();
        let alloc = || shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let (queued, queued_referent, queue) = (alloc(), alloc(), alloc());
        let (unqueued, unqueued_referent) = (alloc(), alloc());
        {
            let mut rp = shared.mem.ref_processor.lock();
            rp.discover_reference(
                cratonvm_gc::ReferenceType::Weak,
                addr_of(queued),
                addr_of(queued_referent),
                Some(addr_of(queue)),
            );
            rp.discover_reference(
                cratonvm_gc::ReferenceType::Weak,
                addr_of(unqueued),
                addr_of(unqueued_referent),
                None,
            );
        }
        // `return` at pc 0: no later bytecode reads slot 1 or 2, so both are
        // dead to the liveness filter (slot 0 is always kept).
        let frame = Frame::new(
            ClassId::new(0),
            "TestClass".to_string(),
            "w2bQueuedRefInDeadLocal".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            4,
            4,
            &[
                Value::Int(0),
                Value::Object(Some(queued)),
                Value::Object(Some(unqueued)),
            ],
        );
        // Non-vacuity: the liveness-filtered scan really drops both slots, so
        // whatever roots `queued` below is step 22 and not step 1.
        let mut filtered = Vec::new();
        frame.scan_local_objects(&mut filtered, &shared.mem.heap);
        assert!(
            !filtered.contains(&queued) && !filtered.contains(&unqueued),
            "fixture: slots 1 and 2 must be liveness-dead at a `return`"
        );
        let mut thread = JvmThread::new(ThreadId(0), "test");
        thread.frames.push(frame);

        let roots = collect_roots(&shared, &thread);
        assert!(
            roots.contains(&queued),
            "a pending queued Reference held by a live frame must survive to be enqueued"
        );
        // With the conservative probe engaged step 1 roots every local anyway;
        // only the liveness-filtered configuration can show the decline.
        if !conservative_locals_enabled() {
            assert!(
                !roots.contains(&unqueued),
                "a queue-less Reference in a dead local has nothing to deliver and must not be rescued"
            );
        }
    }

    /// **THE CONCURRENT OLD-GEN MARK VETOES EVERY SIDE-TABLE DEFERRAL.**
    ///
    /// `ConcurrentMarker` follows no side table, so a root set its remark
    /// consumes must root statics, class locks, condy values, `ClassValue`
    /// results and user-loader mirrors outright — whatever the JIT or an
    /// explicit `System.gc()` would otherwise license.
    ///
    /// gen r5w3/unload7: a FOURTH input, `class_unload_marking`, now licenses
    /// the concurrent cycle's own initial-mark and remark scans when its
    /// marker follows the side tables (`CRATONVM_GEN_CONC_CLASS_UNLOAD`). The
    /// assertions below that pass `false` for it are the pre-unload7 truth
    /// table, unchanged.
    #[test]
    fn the_concurrent_old_gen_mark_vetoes_conditional_metadata() {
        // Outside a concurrent mark: the two licensing terms behave as before.
        assert!(!generational_metadata_is_conditional(
            false, false, false, false
        ));
        assert!(generational_metadata_is_conditional(false, true, false, false));
        assert!(generational_metadata_is_conditional(false, false, true, false));
        // Inside one: never conditional, whatever licenses it — unless the
        // scan is the cycle's own class-unload scan.
        for jit in [false, true] {
            for major in [false, true] {
                assert!(
                    !generational_metadata_is_conditional(true, jit, major, false),
                    "jit_active={jit} major_gc_requested={major}: a remark root \
                     set must not defer to side tables its marker never reads"
                );
                for open in [false, true] {
                    assert!(
                        generational_metadata_is_conditional(open, jit, major, true),
                        "open={open} jit_active={jit} major_gc_requested={major}: a \
                         class-unload scan's marker follows the side tables"
                    );
                }
            }
        }
    }

    #[test]
    fn metadata_weak_mode_forces_in_place_only_when_term_four_is_disarmed() {
        // The one case the fix exists for: weak mode licensed by the JIT-active
        // term alone, a moving collector, and no conservative-root divert.
        assert!(metadata_weak_mode_needs_in_place_promise(
            true, false, true, false
        ));
        // Every other combination is the pre-fix behaviour: no extra divert.
        for weak in [false, true] {
            for explicit in [false, true] {
                for moving in [false, true] {
                    for term4 in [false, true] {
                        let expected = weak && !explicit && moving && !term4;
                        assert_eq!(
                            metadata_weak_mode_needs_in_place_promise(
                                weak, explicit, moving, term4
                            ),
                            expected,
                            "weak={weak} explicit={explicit} moving={moving} term4={term4}"
                        );
                    }
                }
            }
        }
    }

    /// gen r5w6/conc10 — `CRATONVM_GEN_YOUNG_MIRROR_DEFER` widens step 6 only
    /// on a Generational scan with the flag on, young deferral not opted out,
    /// outside the concurrent cycle's class-unload scans, and never under
    /// `CRATONVM_DBG_FORCE_MOVING`.
    #[test]
    fn r5w6_young_mirror_defer_rule_truth_table() {
        assert!(young_mirror_defer_rule(true, true, true, false, false));
        for generational in [false, true] {
            for flag in [false, true] {
                for allowed in [false, true] {
                    for unload_scan in [false, true] {
                        for force_moving in [false, true] {
                            let expected =
                                generational && flag && allowed && !unload_scan && !force_moving;
                            assert_eq!(
                                young_mirror_defer_rule(
                                    generational,
                                    flag,
                                    allowed,
                                    unload_scan,
                                    force_moving
                                ),
                                expected,
                                "gen={generational} flag={flag} allowed={allowed} \
                                 unload_scan={unload_scan} force_moving={force_moving}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn roots_from_frame_locals() {
        let shared = test_shared_vm();
        // FIX: allocate from `shared.mem.heap` (the heap the scanner validates
        // against via `is_object_address`), not an orphan `Heap::new()`.
        // The frame scanner drops any slot whose address is not resident in
        // the heap being scanned — a foreign-heap object can never be a root.
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);

        let mut thread = JvmThread::new(ThreadId(0), "test");
        let frame = Frame::new(
            ClassId::new(0),
            "TestClass".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            5,
            &[Value::Object(Some(obj)), Value::Int(42)],
        );
        thread.frames.push(frame);

        let roots = collect_roots(&shared, &thread);
        assert!(roots.contains(&obj));
    }

    /// **THE A5 PASS RECOVERS WHAT THE TAG-FILTERED SCAN IS DESIGNED TO DROP.**
    ///
    /// A JIT callee's object return value can reach an interpreter local under
    /// a non-object tag. `scan_local_objects` then correctly omits it — a
    /// `long`-tagged slot is a primitive by JVM spec, and rooting it on the
    /// MOVING path would let the collector rewrite a number. The non-moving
    /// sweep has no such hazard and must not free the object, which is what
    /// `scan_locals_conservative` exists for.
    ///
    /// Both halves are asserted. "The conservative pass finds it" alone would
    /// pass just as well if the ordinary scan already did, and then the pass
    /// this test is about could be deleted without failing anything.
    #[test]
    fn the_a5_pass_recovers_a_lost_tag_local_the_tag_filtered_scan_drops() {
        let shared = test_shared_vm();
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let addr = obj.as_ptr() as usize;

        let mut thread = JvmThread::new(ThreadId(0), "test");
        thread.frames.push(Frame::new(
            ClassId::new(0),
            "TestClass".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            5,
            // The lost tag: the address is in the slot, but as a `long`.
            &[Value::Long(addr as i64), Value::Int(42)],
        ));

        assert!(
            !collect_roots(&shared, &thread).contains(&obj),
            "a long-tagged slot must NOT be a root on the ordinary path — if it \
             is, the moving collector would rewrite a primitive, and step 14a5 \
             is not what keeps this object alive"
        );

        let mut roots = Vec::new();
        conservative_frame_pass(&shared, &thread, &mut roots);
        assert!(
            roots.contains(&obj),
            "the A5 pass must recover the reference: on that cycle the sweep \
             frees on GC_FLAG_MARKED, so a slot no scan reports is a slot whose \
             object is zeroed and returned to the free list while still in use"
        );
    }

    /// **AND IT RUNS ON EXACTLY THE CYCLES THAT NEED IT.**
    ///
    /// The pass is only sound where nothing is relocated, and only useful where
    /// step 1's probe was off. Both conditions are in one predicate so the
    /// truth table can be pinned here; the flag it reads is set from inside
    /// `collect_roots`, so an end-to-end test cannot reach this state.
    #[test]
    fn the_a5_pass_engages_only_on_the_unregistered_jit_frame_path() {
        // The flag is a `thread_local!` `Cell`, so this cannot perturb a test
        // running beside it. Restore it either way.
        let restore = cratonvm_gc::gc_quiescence::unregistered_jit_frame_on_stack();

        cratonvm_gc::gc_quiescence::clear_unregistered_jit_frame_on_stack();
        assert!(
            !a5_frame_pass_engages(false),
            "with no unregistered JIT frame the young cycle may MOVE, and a \
             pointer-shaped long rooted by this pass would be relocated and \
             corrupted"
        );

        cratonvm_gc::gc_quiescence::set_unregistered_jit_frame_on_stack();
        assert_eq!(
            a5_frame_pass_engages(false),
            conservative_locals_compiled_in(),
            "with the A5 flag set the sweep is non-moving and the pass must run \
             — unless the probe is compiled out of this run entirely"
        );
        assert!(
            !a5_frame_pass_engages(true),
            "step 1 already probed these frames; running again only doubles the \
             root vector"
        );

        if !restore {
            cratonvm_gc::gc_quiescence::clear_unregistered_jit_frame_on_stack();
        }
    }

    #[test]
    fn roots_from_frame_stack() {
        let shared = test_shared_vm();
        // FIX: allocate from `shared.mem.heap` so the operand-stack scanner's
        // `is_heap_addr` validation recognizes the objects as live heap
        // residents. Objects from a disconnected `Heap::new()` are correctly
        // rejected by the scanner and would never appear as roots.
        let obj1 = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let obj2 = shared.mem.heap.alloc_object(ClassId::new(0), 0);

        let mut thread = JvmThread::new(ThreadId(0), "test");
        let mut frame = Frame::new(
            ClassId::new(0),
            "TestClass".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[],
        );
        frame.stack.push(Value::Object(Some(obj1))).unwrap();
        frame.stack.push(Value::Int(5)).unwrap();
        frame.stack.push(Value::Object(Some(obj2))).unwrap();
        thread.frames.push(frame);

        let roots = collect_roots(&shared, &thread);
        assert!(roots.contains(&obj1));
        assert!(roots.contains(&obj2));
    }

    #[test]
    fn roots_from_statics() {
        let shared = test_shared_vm();
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);

        {
            let mut statics = shared.classes.statics.write();
            statics.insert(
                ClassId::new(1),
                crate::vm::realms::class_realm::StaticsBlock::from_values(vec![
                    Value::Object(Some(obj)),
                    Value::Int(0),
                ]),
            );
        }

        let thread = JvmThread::new(ThreadId(0), "test");
        let roots = collect_roots(&shared, &thread);
        assert!(roots.contains(&obj));
    }

    /// The scan records the ADDRESS of every static slot holding an object, and
    /// `update_all_roots` takes that list exactly once.
    ///
    /// Both halves matter and they fail differently. If the list omits a slot,
    /// the post-collection fix-up leaves a live static pointing at a vacated
    /// address -- a use-after-free that surfaces arbitrarily far away. If the
    /// list can be taken twice, a collection with no scan of its own applies a
    /// list from an earlier one, which misses every slot that became
    /// object-valued in between -- the same failure by a different route. The
    /// take is what rules the second one out: an `update_all_roots` with no
    /// preceding scan gets `None` and does the full walk.
    #[test]
    fn the_scan_records_static_ref_slots_and_the_take_is_once() {
        if !static_root_slots_enabled() {
            return; // kill switch set in this environment; nothing to assert
        }
        let shared = test_shared_vm();
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);

        // Two object slots and two primitives, so a list that simply recorded
        // every slot would be distinguishable from one that recorded the
        // reference-holding ones.
        let (slot0, slot2) = {
            let mut statics = shared.classes.statics.write();
            let block = crate::vm::realms::class_realm::StaticsBlock::from_values(vec![
                Value::Object(Some(obj)),
                Value::Int(7),
                Value::Object(Some(obj)),
                Value::Long(9),
            ]);
            let base = block.base_ptr();
            statics.insert(ClassId::new(1), block);
            // SAFETY: `base` is the start of a leaked `[Value; 4]`.
            (base as usize, unsafe { base.add(2) } as usize)
        };

        let thread = JvmThread::new(ThreadId(0), "test");
        let _ = collect_roots(&shared, &thread);

        // gc-common w7-a: another VM's fix-up on this thread must not take
        // (and apply) this VM's list, and must not consume it either.
        assert!(
            take_static_ref_slots(shared.vm_identity.wrapping_add(1_000_003)).is_none(),
            "a fix-up of another VM must fall back to its own full walk"
        );
        let slots = take_static_ref_slots(shared.vm_identity)
            .expect("the scan must record the two object slots");
        assert!(
            slots.contains(&slot0) && slots.contains(&slot2),
            "both object-valued slots must be recorded; got {slots:?} want {slot0:#x} and {slot2:#x}"
        );

        assert!(
            take_static_ref_slots(shared.vm_identity).is_none(),
            "the take must be once -- a second consumer would be applying a list from a scan that is not its own"
        );
    }

    #[test]
    fn roots_from_printed() {
        let shared = test_shared_vm();
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);

        let mut thread = JvmThread::new(ThreadId(0), "test");
        thread.printed.push(Value::Object(Some(obj)));
        thread.printed.push(Value::Int(100));

        let roots = collect_roots(&shared, &thread);
        assert!(roots.contains(&obj));
    }

    #[test]
    fn roots_empty_state() {
        let shared = test_shared_vm();
        let thread = JvmThread::new(ThreadId(0), "test");
        let roots = collect_roots(&shared, &thread);
        assert!(
            roots.iter().all(|root| shared
                .mem
                .heap
                .is_object_address(root.as_ptr() as usize)
                .is_none()),
            "empty SharedVm/thread state must not contribute roots from this heap: {roots:?}",
        );
    }

    /// NEW-1.5 end-to-end: an object whose only live reference lives on the
    /// native stack (in a fake "JIT spill slot") is discovered by the
    /// conservative scanner and reported as a root, provided a JIT entry
    /// guard is active. Without the guard, the object is *not* discovered
    /// (the chain is empty) — confirming that we don't accidentally scan
    /// the entire native stack on every root collection.
    #[test]
    fn conservative_jit_root_scan_finds_spilled_object() {
        let shared = test_shared_vm();
        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 0);
        let thread = JvmThread::new(ThreadId(0), "test");

        // Place the object's address into a stack-allocated slot. The
        // conservative scanner walks `[scanner_sp .. entry_sp)` for each
        // active JIT entry, treating each qword as a possible heap pointer.
        // We push a JIT entry guard FIRST (so entry_sp is captured *above*
        // this point in the stack), then allocate the spill slot below it,
        // then run the scan from inside this same scope so the scanner's
        // own SP is even further down the stack.
        let _g = crate::jit::conservative_roots::JitEntryGuard::enter();

        // Keep the spill slot live via std::hint::black_box so the
        // optimizer can't elide it and the address remains observable.
        let mut spill_slot: usize = obj.as_ptr() as usize;
        std::hint::black_box(&mut spill_slot);

        let roots = collect_roots(&shared, &thread);
        // The object should appear in the root set via the conservative
        // JIT scan path. We can't assert exact length because the scan
        // may also pick up unrelated stack values that coincidentally
        // look like object headers — but those are filtered by
        // is_object_address so the count is bounded.
        assert!(
            roots.contains(&obj),
            "conservative JIT root scan must report the spilled object as a root \
             (heap addr = {:#x}, roots = {:?})",
            obj.as_ptr() as usize,
            roots
                .iter()
                .map(|r| r.as_ptr() as usize)
                .collect::<Vec<_>>()
        );
        // Hint to the compiler that spill_slot is still live at this point
        // — otherwise it might be reused by an earlier register before the
        // scan runs and we'd get a false negative.
        std::hint::black_box(&spill_slot);
    }

    /// Negative companion to the above: with NO active JIT entry guard,
    /// the conservative scanner sees an empty chain and must not report
    /// the object as a root (it has no other reference path).
    #[test]
    fn conservative_jit_root_scan_skips_inactive_threads() {
        let shared = test_shared_vm();
        let obj = shared
            .mem
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 0);
        let thread = JvmThread::new(ThreadId(0), "test");

        let mut spill_slot: usize = obj.as_ptr() as usize;
        std::hint::black_box(&mut spill_slot);

        let roots = collect_roots(&shared, &thread);
        assert!(
            !roots.contains(&obj),
            "with no JIT entry guard, the conservative scanner must not \
             scan the calling thread's native stack"
        );
    }

    /// NEW-1.5 GC quiescence: while a JIT entry guard is held, the
    /// process-wide quiescence flag is set, telling the GC to defer
    /// compaction.
    #[test]
    fn jit_entry_sets_gc_quiescence_flag() {
        let depth_before = cratonvm_gc::gc_quiescence::depth();
        {
            let _g = crate::jit::conservative_roots::JitEntryGuard::enter();
            assert_eq!(cratonvm_gc::gc_quiescence::depth(), depth_before + 1);
            assert!(cratonvm_gc::gc_quiescence::is_active());
        }
        assert_eq!(cratonvm_gc::gc_quiescence::depth(), depth_before);
    }
    /// **The coverage proof must be computed for EVERY collector, not only the
    /// one that consumes the suppression.**
    ///
    /// This is a source witness, and it is a source witness on purpose. The
    /// defect it guards is not a wrong value — it is a call that never happens:
    /// `refresh_moving_young_coverage_for_collection()` sat behind
    /// `heap.is_generational()` in a short-circuiting `&&` chain, so on a G1 or
    /// ZGC cycle nothing ran and `moving_young_coverage_incomplete()` kept the
    /// `false` that `begin_moving_young_coverage_cycle` had reset it to. A
    /// collector-side gate reading that verdict is reading a proof nobody ran,
    /// which is exactly how `zgc::relocate_stw`'s first per-cycle refusal was
    /// unsound. No runtime assertion in this crate can see a call that did not
    /// happen; the ORDER of the terms is the invariant, so the order is what is
    /// asserted.
    #[test]
    fn the_coverage_proof_runs_for_every_collector() {
        let src = include_str!("roots.rs");

        // Establish the corpus before concluding anything from it: a file that
        // stopped containing the decision would make this pass for the wrong
        // reason.
        assert!(
            src.contains("let coverage_proven = moving_young"),
            "roots.rs no longer computes `coverage_proven`, so this scan is \
             reading the wrong text and its verdict means nothing"
        );

        let proof = src
            .find("let coverage_proven = moving_young")
            .expect("anchor checked above");
        let suppression = src
            .find("let moving_young_precise_only =")
            .expect("roots.rs no longer computes `moving_young_precise_only`");
        assert!(
            proof < suppression,
            "the proof must be computed BEFORE, and independently of, the \
             suppression decision"
        );

        // The proof expression must not mention the collector at all. If a
        // collector test creeps back into it, `&&` short-circuits and the
        // refresh stops running for whatever the test excludes.
        let proof_expr = &src[proof..suppression];
        assert!(
            proof_expr.contains("refresh_moving_young_coverage_for_collection("),
            "`coverage_proven` no longer calls the refresh, so nothing computes \
             the verdict any collector-side gate reads"
        );
        // AN ARGUMENT IS NOT A TERM, and the difference is the whole of what
        // this guard protects.
        //
        // The refresh takes the collector's `honours_conservative_pins()` as a
        // parameter, so the proof expression does mention the collector -- and
        // must, because whether a PIN discharges a peer's coverage obligation
        // is a question only the collector can answer. What the loop below
        // forbids is a collector test as a `&&` TERM, which is a different
        // thing: `&&` short-circuits, so a term would stop the refresh RUNNING
        // for whatever it excludes, and that is precisely the defect this
        // scan was written for. A parameter cannot do that -- the call is
        // made either way.
        for forbidden in ["is_generational", "is_g1", "g1_precise_only_roots"] {
            assert!(
                !proof_expr.contains(forbidden),
                "`coverage_proven` mentions `{forbidden}`. In a short-circuiting \
                 `&&` chain that stops the refresh from running for the excluded \
                 collectors, and their verdict silently becomes a vacuous \
                 `false` meaning `complete` -- the defect this split exists to \
                 remove. Gate the SUPPRESSION on the collector, never the proof."
            );
        }
    }

    /// gc-common w6-a (`common-b-proposal-dedupe-the-initiator-root-set.md`):
    /// the step-10b skip is scoped to one scan and restored on every exit,
    /// unwinding included -- a skip left armed would drop every thread mirror
    /// from a later plain `collect_roots` on this thread whose caller does NOT
    /// append the registry.
    #[test]
    fn the_registry_mirror_skip_is_scoped_and_restored() {
        let armed = || REGISTRY_MIRRORS_APPENDED_BY_CALLER.with(std::cell::Cell::get);
        assert!(!armed());
        {
            let _g = RegistryMirrorsAppended(
                REGISTRY_MIRRORS_APPENDED_BY_CALLER.with(|c| c.replace(true)),
            );
            assert!(armed());
        }
        assert!(!armed());
        let unwound = std::panic::catch_unwind(|| {
            let _g = RegistryMirrorsAppended(
                REGISTRY_MIRRORS_APPENDED_BY_CALLER.with(|c| c.replace(true)),
            );
            panic!("a root scan that unwinds");
        });
        assert!(unwound.is_err());
        assert!(!armed(), "an unwinding scan must not leave the skip armed");
    }

    /// gc-common w6-a: the only caller of the step-10b skip appends the
    /// registry's snapshots -- the list that carries the thread mirrors the
    /// skip leaves out -- straight after the scan. A second caller, or an
    /// append that moved away, would drop every alive thread's mirror from
    /// the root set.
    #[test]
    fn the_registry_appended_scan_is_always_followed_by_the_registry_append() {
        let src = include_str!("../runtime/interpreter/gc_and_alloc.rs");
        let calls: Vec<usize> = src
            .match_indices("collect_roots_registry_appended(")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            calls.len(),
            1,
            "`collect_roots_registry_appended` must have exactly one caller, \
             `run_collection_pause`; audit any new one for the registry append"
        );
        let after = &src[calls[0]..];
        let append = after
            .find("thread_registry.collect_all_root_snapshots()")
            .expect("the registry append after the scan");
        assert!(
            after[..append].lines().count() <= 3,
            "the registry snapshots must be appended right after the scan that \
             skipped their mirrors"
        );
    }

    /// gcd d2/g (`CRATONVM_GEN_PRECISE_ROOT_PROMOTE`): the published counts
    /// are the occurrences inside the precise sections only, duplicates
    /// counted; a section that does not describe the list publishes nothing.
    #[test]
    fn gcd_d2g_precise_counts_cover_only_the_precise_sections() {
        // SAFETY: never dereferenced; only the addresses are counted.
        let at = |a: usize| unsafe { ObjectRef::from_raw(a as *mut u8) };
        // [frame word, static, static(dup), frame word, string, mirror, jni]
        let roots = vec![
            at(0x7100),
            at(0x7200),
            at(0x7200),
            at(0x7300),
            at(0x7400),
            at(0x7500),
            at(0x7600),
        ];
        cratonvm_gc::gen_heap::clear_precise_root_values();
        publish_precise_root_counts(&roots, &[(1, 3), (4, 5), (5, 6), (6, 7)]);
        let counts = cratonvm_gc::gen_heap::take_precise_root_values().expect("published");
        assert_eq!(counts.get(&0x7200), Some(&2), "two static slots name it");
        assert_eq!(counts.get(&0x7400), Some(&1));
        assert_eq!(counts.get(&0x7500), Some(&1));
        assert_eq!(counts.get(&0x7600), Some(&1));
        assert_eq!(counts.get(&0x7100), None, "outside every precise section");
        assert_eq!(counts.get(&0x7300), None);

        publish_precise_root_counts(&roots, &[(1, 3), (4, 5), (5, 6), (6, 9)]);
        assert!(
            cratonvm_gc::gen_heap::take_precise_root_values().is_none(),
            "a range past the list publishes nothing"
        );
    }
}
