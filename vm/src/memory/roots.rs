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
#[inline]
fn conditional_loader_metadata(shared: &SharedVm) -> bool {
    if !cratonvm_native_builtins::classloader::loader_unload_enabled() {
        return false;
    }
    match shared.config.gc_algorithm {
        crate::config::GcAlgorithm::Generational => {
            cratonvm_gc::gc_quiescence::is_active()
                || cratonvm_gc::gc_quiescence::major_gc_requested()
        }
        crate::config::GcAlgorithm::G1 => cratonvm_gc::gc_quiescence::class_unload_marking(),
        #[cfg(feature = "zgc")]
        crate::config::GcAlgorithm::Zgc => true,
    }
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

/// `CRATONVM_G1_PRECISE_ONLY_ROOTS=1` вЂ” let G1 take the precise-only root
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
    static STATIC_REF_SLOTS: std::cell::RefCell<Vec<usize>> =
        const { std::cell::RefCell::new(Vec::new()) };
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
static STATIC_SLOTS_RECORDED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static STATIC_SLOT_FALLBACKS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

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
            v.clear();
        }
    });
}

/// Record the address of a static slot found holding an object.
#[inline]
fn note_static_ref_slot(addr: usize) {
    STATIC_REF_SLOTS.with(|c| {
        if let Ok(mut v) = c.try_borrow_mut() {
            v.push(addr);
        }
    });
}

/// Take this thread's recorded static ref slots, leaving the store empty.
///
/// `None` when nothing was recorded -- either the kill switch is set, or no
/// scan ran on this thread since the last take. Both mean "do the full walk".
pub fn take_static_ref_slots() -> Option<Vec<usize>> {
    STATIC_REF_SLOTS.with(|c| {
        let mut v = c.try_borrow_mut().ok()?;
        if v.is_empty() {
            return None;
        }
        STATIC_SLOTS_RECORDED.fetch_add(v.len() as u64, std::sync::atomic::Ordering::Relaxed);
        Some(std::mem::take(&mut *v))
    })
}

/// The capacity to start a root set at, and the ceiling that keeps a single
/// pathological collection from pinning a large reservation for the life of the
/// process.
const ROOT_HINT_CEILING: usize = 1 << 20;

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
    let conditional_metadata = conditional_loader_metadata(shared);
    cratonvm_types::metadata_pin::set_metadata_weak_mode(shared.vm_identity, conditional_metadata);
    cratonvm_types::metadata_pin::replace_metadata_pins(shared.vm_identity, &[]);

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
    for frame in &thread.frames {
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
            frame.scan_local_objects(&mut roots, &shared.mem.heap);
        }
        if conservative_locals {
            frame.scan_locals_conservative(&mut roots, &shared.mem.heap);
        }
        let before = roots.len();
        frame.stack.scan_object_refs(&mut roots, &shared.mem.heap);
        if roots.len() > before {
            let added = roots.split_off(before);
            for o in added {
                let addr = o.as_ptr() as usize;
                // `is_heap_addr`, matching the locals scan above and the other
                // three copies of this filter — see `scan_frame_roots` in
                // `runtime/interpreter/gc_and_alloc.rs`. The strict
                // `is_object_address` probe used here until 2026-08-04 dropped
                // genuine young / mid-init roots, and this is the INITIATOR's
                // own scan: a root it drops has no second chance.
                if shared.mem.heap.is_heap_addr(addr).is_some() {
                    roots.push(o);
                }
            }
        }
        if conservative_locals {
            frame
                .stack
                .scan_object_refs_conservative(&mut roots, &shared.mem.heap);
        }
    }

    mark_scan_section(roots.len(), "2: Static fields — all classes");
    // 2. Static fields — all classes
    //
    // The `metadata_pin_deferrable` guard (SPB.1 residual fix) additionally
    // requires the value to already be in old gen before deferring to
    // `metadata_pin` — see that method's doc for why: the Generational
    // backend's `metadata_pin` consumer runs only inside the old-gen BFS, so
    // a still-young static value deferred here would never be marked by
    // anything and could be reclaimed mid-`<clinit>`. Same reasoning applies
    // to the class-lock (`3.`) and CONSTANT_Dynamic (`13.`) sections below.
    {
        let __st_t0 = std::time::Instant::now();
        let statics = shared.classes.statics.read();
        let mut __st_slots = 0u64;
        let mut __st_objects = 0u64;
        for (&class_id, fields) in statics.iter() {
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
                        note_static_ref_slot(val as *const Value as usize);
                    }
                    if conditional_metadata
                        && shared
                            .mem
                            .heap
                            .metadata_pin_deferrable(obj_ref.as_ptr() as usize)
                    {
                        if let Some(loader) =
                            cratonvm_types::loader_pin::loader_pin_addr(class_id.as_u32())
                        {
                            cratonvm_types::metadata_pin::add_metadata_pin(
                                shared.vm_identity,
                                loader,
                                obj_ref.as_ptr() as usize,
                            );
                            continue;
                        }
                    }
                    roots.push(obj_ref);
                }
            }
        }
        note_statics_scan(
            __st_slots,
            __st_objects,
            __st_t0.elapsed().as_nanos() as u64,
        );
    }

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
                if let Some(loader) = cratonvm_types::loader_pin::loader_pin_addr(class_id.as_u32())
                {
                    cratonvm_types::metadata_pin::add_metadata_pin(
                        shared.vm_identity,
                        loader,
                        obj_ref.as_ptr() as usize,
                    );
                    continue;
                }
            }
            roots.push(*obj_ref);
        }
    }

    mark_scan_section(
        roots.len(),
        "4: Off-frame per-thread roots — the test-harness print buffer a",
    );
    // 4. Off-frame per-thread roots — the test-harness print buffer and the
    //    scoped-value bindings. Shared with both peer-publish paths; see
    //    `push_off_frame_thread_roots`.
    push_off_frame_thread_roots(thread, &mut roots);

    mark_scan_section(
        roots.len(),
        "4b: Native invoke pins — object args popped off the operand stac",
    );
    // 4b. Native invoke pins — object args popped off the operand stack for
    //     `safe_native_call` (see `JvmThread::native_pin_roots`).
    for obj_ref in &thread.native_pin_roots {
        roots.push(*obj_ref);
    }

    // ---- handle scope support (arch/handles) ----
    // 4b'. Rooted-handle slots — `NativeContext::handle_root`'s per-thread
    //      backing store (`JvmThread::handle_slots`, see that field's doc
    //      comment). Same shape as the `native_pin_roots` splice just above:
    //      a live (`Some`) slot is a GC root exactly like a pin. A `None`
    //      hole is an already-released slot and contributes nothing.
    //
    //      Moving-GC remap is paired across all ownership paths: the current
    //      collector in `memory::gc::update_all_roots`, a safepoint peer in
    //      `interpreter::apply_pointer_map_to_thread`, and a native-blocked
    //      peer in `NativeContextImpl::check_post_block_gc_refs`. The leaked-
    //      blocked-region fallback (`apply_pending_blocked_fixups`) consumes
    //      the same fixup chain and remaps them too. Both snapshot-producing
    //      peer paths publish these slots in their deposited root snapshots.
    for slot in &thread.handle_slots {
        if let Some(obj_ref) = slot {
            roots.push(*obj_ref);
        }
    }

    mark_scan_section(
        roots.len(),
        "4c: Native object in flight — object return before the interpret",
    );
    // 4c. Native object in flight — object return before the interpreter pushes
    //     it onto the operand stack, or native-thrown exception before it is
    //     routed into a Java handler / uncaught dispatch.
    if let Some(obj_ref) = thread.native_pending_return {
        roots.push(obj_ref);
    }

    mark_scan_section(
        roots.len(),
        "4d: Direct JIT HashMap node cache. Both refs remain valid across",
    );
    // 4d. Direct JIT HashMap node cache. Both refs remain valid across a
    // moving collection because this scan and gc.rs remap them with the thread.
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

    mark_scan_section(
        roots.len(),
        "5: Interned string pool — all interned String objects",
    );
    // 5. Interned string pool — all interned String objects
    {
        let string_pool = shared.mem.string_pool.read();
        for obj_ref in string_pool.values() {
            roots.push(*obj_ref);
        }
    }

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
                if mirror_dbg && is_user_defined {
                    let name = cm
                        .get_class(class_id)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    eprintln!(
                        "[DBG_MIRRORPIN] roots: user class={name:?} mirror={:#x} deferrable={deferrable} => {}",
                        obj_ref.as_ptr() as usize,
                        if deferrable { "DEFERRED" } else { "ROOTED" }
                    );
                }
                if is_user_defined && deferrable {
                    continue;
                }
                roots.push(*obj_ref);
            }
        } else {
            for obj_ref in class_mirrors.values() {
                roots.push(*obj_ref);
            }
        }
    }

    mark_scan_section(
        roots.len(),
        "6b: Cached proxy-dispatch `Method` objects (see `proxy_method_ca",
    );
    // 6b. Cached proxy-dispatch `Method` objects (see `proxy_method_cache`'s
    // doc comment in `class_realm.rs`) — these are meant to be shared and
    // reused across every future dispatch to the same proxy method, so they
    // must stay alive unconditionally for as long as the cache entry exists,
    // exactly like `class_mirrors` above.
    {
        for obj_ref in shared.classes.proxy_method_cache.read().values() {
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
    {
        shared
            .natives
            .jni_global_refs
            .lock()
            .collect_roots(&mut roots);
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

    mark_scan_section(
        roots.len(),
        "9c: JNI keep-alive pin set (INT-10): arrays checked out via",
    );
    // 9c. JNI keep-alive pin set (INT-10): arrays checked out via
    //     GetPrimitiveArrayCritical / Get<Type>ArrayElements. Native code
    //     holds a detached COPY of the body (so relocation is safe), but the
    //     copy-back at Release targets the OBJECT — which must therefore
    //     stay alive even when its only other reference was dropped while
    //     the native held the copy. Previously this set was spliced only
    //     into the semispace `Heap` backend; generational/G1/ZGC relied on
    //     the (initiator-only) JNI-local scan above. The matching re-key
    //     after a move is `cratonvm_gc::pinned::update_after_gc` (gc.rs).
    if cratonvm_gc::pinned::any_pinned() {
        for addr in cratonvm_gc::pinned::pinned_addrs() {
            if let Some(obj) = shared.mem.heap.is_object_address(addr) {
                roots.push(obj);
            }
        }
    }

    mark_scan_section(
        roots.len(),
        "10: Thread-local ObjectRefs — java_thread_obj, pending_async_exc",
    );
    // 10. Thread-local ObjectRefs — java_thread_obj, pending_async_exception,
    //     jit_pending_exception
    if let Some(ref obj_ref) = thread.java_thread_obj {
        roots.push(*obj_ref);
    }
    if let Some(ref obj_ref) = thread.pending_async_exception {
        roots.push(*obj_ref);
    }
    // The JIT's pending throwable. It used to live in a thread-local `Cell`,
    // where the collector could not reach it at all — TLS is invisible from a
    // collecting thread, which is precisely why the two slots above are fields.
    // Without this push the remap half is half-wired: the reference would be
    // relocated but never kept alive.
    if let Some(ref obj_ref) = thread.jit_pending_exception {
        roots.push(*obj_ref);
    }
    // The uncaught throwable the launcher is holding across shutdown-hook
    // execution. A hook allocates and can collect, and until this slot existed
    // the throwable was reachable only from a Rust local — so the render after
    // the hooks read a zeroed header and printed `java/lang/Object`. See
    // `JvmThread::uncaught_exception_pending`.
    if let Some(ref obj_ref) = thread.uncaught_exception_pending {
        roots.push(*obj_ref);
    }
    // The JIT's stashed deopt / exceptional frames. Those live in `jit/`
    // thread-locals — that crate cannot depend on `vm/`, so they cannot become
    // `JvmThread` fields the way the slot above did — and are reached through
    // an on-thread visitor instead. That works for the same reason the slots
    // above do: this scan already runs ON the owning thread. Paired with the
    // remap in `gc.rs`; wiring one without the other is refused by a debug
    // assertion in the visitor. See `docs/jit/deopt-thread-local-roots.md`.
    //
    // `is_object_address` rather than a bare `ObjectRef`, matching the
    // `pinned_addrs` block above: the stash can name an address the heap no
    // longer owns, if an earlier collection already ran while it was unrooted.
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
    for obj_ref in shared
        .threads
        .thread_registry
        .alive_thread_objects(usize::MAX)
    {
        roots.push(obj_ref);
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
                if let Some(loader) = cratonvm_types::loader_pin::loader_pin_addr(class_id.as_u32())
                {
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
    // the scan therefore does not leave G1 with a different protection вЂ” it
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
    // stack is rewritable" вЂ” which is the guarantee this branch spends. That
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
    let coverage_proven = moving_young
        && !moving_young_osr_fallback
        && crate::jit::conservative_roots::refresh_moving_young_coverage_for_collection(
            // Whether a PIN discharges a peer's coverage obligation is a
            // question about the collector, so it is answered by the collector
            // rather than read from a process-global: two live heaps would make
            // a published capability describe whichever constructed last.
            shared.mem.heap.honours_conservative_pins(),
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
        let addrs: Vec<usize> = roots[jit_scan_start..]
            .iter()
            .map(|r| r.as_ptr() as usize)
            .collect();
        cratonvm_gc::gc_quiescence::publish_pinned_jit_roots(&addrs);
    }
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
    // `docs/known-issues/springboot/bindabletests-bytebuddy-receiver-reclaimed-under-gc-stress-20260908.md`
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
        let labels: Vec<&str> = frames.iter().map(|f| f.label.as_str()).collect();
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
    crate::memory::native_roots::scan_all_roots(shared, &mut roots);
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
    //      (`gc_update_unsafe_side_store_refs`).

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

    /// **A JNI CRITICAL PIN IS A ROOT, AND SECTION 9c IS WHAT MAKES IT ONE.**
    ///
    /// `GetPrimitiveArrayCritical` takes two pins at one call site and they do
    /// different jobs. `VmHeap::pin_critical_region` pins the object against
    /// MOVEMENT -- a G1 region, or, on ZGC, an address the relocation-set
    /// filter reads. `pin_critical_array` pins it against COLLECTION, through
    /// `cratonvm_gc::pinned`, and section 9c above is the only thing that turns
    /// that set into roots for the generational, G1 and ZGC backends; before it
    /// existed they relied on the initiator-only JNI-local scan.
    ///
    /// Nothing tested the second half. That is how it comes to be read as the
    /// first: `ZgcRealHeap::critical_pins` has exactly one reader, inside
    /// `relocate_stw`, so a reviewer tracing liveness from THAT end finds a pin
    /// that keeps nothing alive and concludes the contract is broken. It is
    /// not -- the keep-alive lives here, one crate away -- and this test is the
    /// evidence at the point where deleting it would do the damage.
    ///
    /// The negative assertions are the load-bearing ones. A test that only
    /// checked "pinned implies root" would still pass if section 9c pushed
    /// every address it was handed, or if this object were a root by some other
    /// path; asserting it is NOT a root before the pin and NOT a root after the
    /// release is what makes the middle assertion mean the pin. They are keyed
    /// on this object's own address, so a pin taken by a test running beside
    /// this one cannot perturb them.
    #[test]
    fn a_jni_critical_pin_keeps_its_object_alive_with_no_other_reference() {
        let shared = test_shared_vm();
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        // No frame, no stack, no field: unreachable except through the pin.
        let thread = JvmThread::new(ThreadId(0), "test");

        assert!(
            !collect_roots(&shared, &thread).contains(&obj),
            "an object with no reference must not already be a root, or the \
             assertion below proves nothing"
        );

        let token = cratonvm_gc::pinned::pin_tokened(obj.as_ptr() as usize)
            .expect("a non-null heap address must be pinnable");
        assert!(
            collect_roots(&shared, &thread).contains(&obj),
            "a checked-out array must survive a collection taken while native \
             code holds the copy -- the copy-back at Release targets the \
             OBJECT, so reclaiming it writes into recycled memory"
        );

        cratonvm_gc::pinned::unpin_token(token);
        assert!(
            !collect_roots(&shared, &thread).contains(&obj),
            "Release must give the object back to the collector; a pin that \
             outlives its critical section holds the array and its whole \
             transitive closure for the life of the process"
        );
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

        let slots = take_static_ref_slots().expect("the scan must record the two object slots");
        assert!(
            slots.contains(&slot0) && slots.contains(&slot2),
            "both object-valued slots must be recorded; got {slots:?} want {slot0:#x} and {slot2:#x}"
        );

        assert!(
            take_static_ref_slots().is_none(),
            "the take must be once -- a second consumer would be applying a list              from a scan that is not its own"
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
}
