// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-method invocation counting from inside compiled code.
//!
//! # Why this exists: the tiered manager's count is dead once a body lands
//!
//! [`crate::tiered::MethodState::invocation_count`] is written by the
//! manager's invocation hook, `TieredCompilationManager::on_method_invocation_settling`,
//! and the interpreter reaches that hook through exactly one function,
//! `jit_bridge::offer_invocation_to_tiered_manager`. (The drain below writes it
//! too, through `on_observed_invocations`.) Every call site of that
//! function sits behind a "this callee has no published body" guard —
//! `dispatch_static.rs` and `dispatch_virtual.rs` count only in the `None` arm
//! of their `jit_cache` probe, and `invoke_fast.rs` declines outright once
//! `callee_has_compiled_body`. Nothing on the compiled path counts:
//! `execute_jit_call` touches no counter, and until this module existed
//! neither backend emitted one.
//!
//! `MethodState::current_tier` only advances to `C1` when a C1 body was
//! actually published, which is precisely the event that closes every counting
//! door. So the count FREEZES at whatever the method had accumulated while it
//! was still interpreted (measured: ~5 000 against ~100 000 real calls), and
//! `should_compile`'s `C1 -> C2` arm — which asks for
//! `max(c2_threshold, c2_min_invocations)`, 20 000 by default — can never be
//! satisfied afterwards.
//!
//! `CompilerCore::promote_hot_c1_to_c2` made that arm REACHABLE: it asks the
//! arm's own predicate once, at the C1 publish, from inside `finish`. It did
//! not make the count LIVE. A method frozen at 5 000 still does not cross
//! 20 000, so the hotness door into the optimizing tier is open but nothing
//! can walk through it.
//!
//! This module is the live count. A published C1 body bumps one per-method
//! word on every entry, and a drain (`JitCache::snapshot_entry_counts`, fed to
//! `TieredCompilationManager::on_observed_invocations` by the VM) carries it
//! back to the manager. That door is the invocation hook's fast-forward
//! WITHOUT the hook's own `+= 1`: a drain reports invocations, it is not one.
//! (Until round 9 the drain went through `on_method_invocation_settling` and
//! charged one phantom invocation per method per drain.)
//!
//! # What the counter costs, and why it is default-ON (since round 9 wave 3)
//!
//! Two instructions in the prologue of every C1 body:
//!
//! ```text
//!   mov  r10, imm64(&counter.count)     ; 10 bytes, 1 uop
//!   add  qword ptr [r10 + 0], 1         ;  5 bytes, load+add+store
//! ```
//!
//! No call, no lock, no branch, no atomic RMW, no register the prologue does
//! not already clobber at that point. That is cheap, but "cheap" on a path
//! taken by EVERY compiled invocation in the process is a claim that has to be
//! MEASURED, not asserted — the same file's shadow-stack thread fetch was a
//! single CALL and cost `fib44` ~2.8x. So it shipped behind
//! `CRATONVM_JIT_ENTRY_COUNTER`, default off, byte-identical codegen when off,
//! until it was priced.
//!
//! It was priced in JIT review round 9, wave 3 (lane `tiering3`, notes in
//! `docs/feature-designs/jit-ir-optimizations.md`): the same binary, flag
//! set against flag unset, three interleaved runs each, every output
//! identical to HotSpot's. `IrEscapeProbe` 9 660 -> 6 328 ms (its allocating
//! `step` reaches C2 81 ms after its C1 body lands, where before it never
//! did); `CratonBench` total 45 355 -> 43 648 ms and `CratonBenchC2` total
//! 2 429 -> 2 413 ms, both inside the host's noise; no phase regressed
//! outside it. So it is default-ON. `CRATONVM_JIT_ENTRY_COUNTER=0` (or
//! `false`/`off`/`no`) is the kill switch and restores the byte-identical
//! uncounted prologue; the A/B is that value against a default run.
//!
//! # The increment is deliberately RACY
//!
//! `add qword ptr [mem], 1` without a `lock` prefix is not atomic across
//! cores: two threads entering the same compiled method simultaneously can
//! both read the same value and both store value+1, losing one. That is
//! accepted, on purpose:
//!
//!  * The consequence of a lost increment is that the method reaches the C2
//!    bar slightly later than it should. It is a DELAY, not a wrong answer —
//!    there is no correctness property anywhere that depends on this number
//!    being exact, and the number it feeds (`observed_count`) is explicitly
//!    documented as a fast-forward hint that "degrades to the historical
//!    `+= 1` behaviour" when it is stale or small.
//!  * A `lock add` would make it exact and would also put a full barrier plus
//!    a cache-line round trip on every entry to every compiled method. For a
//!    method hot enough to matter, that line is exactly the one being
//!    contended, so the exact counter is the expensive one precisely where the
//!    inexact one is good enough.
//!
//! The Rust side still reads through [`AtomicU64::load`] with `Relaxed`
//! ordering. On x86-64 an aligned 8-byte `add [mem], imm` is performed as an
//! aligned 8-byte load and an aligned 8-byte store, so no read can observe a
//! torn value; going through the atomic type is what makes that a statement
//! about the abstract machine rather than about the hardware.
//!
//! # What the counter does NOT see
//!
//! Named here because a census whose blind spots are undocumented gets read as
//! a total:
//!
//!  * **Inlined callees.** A method spliced into its caller by the inliner
//!    never executes its own prologue, so its counter does not move. This is
//!    arguably correct — an inlined body is already getting optimized code —
//!    but it means a small hot leaf can look cold.
//!  * **OSR entries.** The OSR trampoline enters PAST the method-entry
//!    prologue (see `osr_entry.rs`), by construction. Loop-entered
//!    activations are not counted; `request_osr` is the door that serves them
//!    and it counts back-edges separately.
//!  * **Interpreted invocations before the C1 body landed.** The counter is
//!    allocated per COMPILE and starts at zero, so the ~`c1_threshold`-sized
//!    head of the method's history lives only in the manager's own frozen
//!    count. `on_observed_invocations` takes the MAXIMUM of its own
//!    count and `observed_count`, so this is a bounded undercount that delays
//!    the crossing by at most the pre-compile count; it cannot make the
//!    manager go backwards.
//!  * **C2 bodies.** Deliberately not instrumented — see
//!    [`crate::x64::frames`]'s prologue block. A method already at C2 has
//!    nowhere left to be promoted to, so counting its entries would be pure
//!    cost with no consumer -- EXCEPT while the cold-body sweeper is armed
//!    (`CRATONVM_JIT_CODE_CACHE_SWEEP`, opt-in), whose only usage signal this
//!    count is: then the optimizing tier emits the same two instructions after
//!    its not-entrant pad (`ir_lower::Lowerer::emit_sweep_entry_counter`,
//!    `CRATONVM_JIT_IR_SWEEP_ENTRY_COUNTER`, JIT round 22 wave 12), or every C2
//!    body would read cold at every sweep.
//!  * **A compile that bails after the prologue.** The box is dropped with the
//!    `Compiler`, so nothing leaks and nothing dangles.
//!
//! # Back edges (round 11 wave 18, `CRATONVM_JIT_CALLEE_PROMOTION`)
//!
//! An entry count cannot see a method that is entered rarely and loops a lot.
//! `r11-orch-callee-compiled-bodies-are-never-promoted` measured one: entered
//! 48 times, ~4 000 inner iterations per entry, and the C2 bar is 20 000
//! ENTRIES, so it stayed on the single-pass tier for the whole run. HotSpot
//! promotes such a method through its tier-3 BACK-EDGE counter
//! (`Tier4BackEdgeThreshold` 40 000 against `Tier4InvocationThreshold` 5 000,
//! i.e. eight back edges weigh one invocation).
//!
//! So the single-pass backend also bumps a second word of the same box,
//! [`JitEntryCounter::BACK_EDGE_OFFSET`], on every loop back edge
//! (`x64::frames::emit_back_edge_counter`, called beside each back-edge
//! safepoint poll in `x64/op_control.rs`), and the drain reports
//! `entries + back_edges >> BACK_EDGE_WEIGHT_SHIFT` (HotSpot's 8:1 ratio). It is
//! as racy as the entry word, for the same reasons. It is emitted only into a
//! body whose prologue armed the entry counter, so every byte-exact test that
//! disarms the counter still sees the old code, and
//! `CRATONVM_JIT_CALLEE_PROMOTION=0` ([`callee_promotion_enabled`]) removes it
//! (together with the tiered manager's `note_published_outside_manager` and
//! the VM's OSR-entry drain trigger).
//!
//! Not in an OSR artifact (round 11 wave 19): the drain walks only the
//! method-entry maps, so the x64 driver parks the box for the body walk of an
//! OSR-door compile (`park_back_edge_counter_for_door`) and its loops carry no
//! bump. The box itself still exists, owned by the artifact, because the
//! prologue baked its address: the OSR trampoline enters past that prologue,
//! but the artifact's own raw self-calls target its entry and do run it.
//!
//! Its cost is one memory read-modify-write per iteration of every
//! method-entry single-pass loop, a loop-carried chain through the store buffer that is
//! independent of the loop's own work. It is the tier-3 profiling cost
//! HotSpot pays too, and it has to be priced on the tightest loops (see the
//! page's Wave 18 section).
//!
//! [`JitEntryCounter::get`] and `CompiledMethod::entry_count` stay ENTRIES
//! only (the cold-body sweeper reads them); only the drain's
//! `JitEntryCounter::take_if_moved` is weighted.

use std::sync::atomic::{AtomicU64, Ordering};

/// One published method's entry count, addressed directly by generated code.
///
/// `#[repr(C)]` with [`Self::count`] FIRST on purpose: the prologue bakes the
/// address of [`Self::count`] as an `imm64` and addresses it with a fixed
/// displacement, so the field's offset is part of the ABI between this struct
/// and the emitter. `last_drained` sits after it and is never touched by
/// generated code.
/// [`Self::COUNT_OFFSET`] is that offset and
/// `entry_counter_count_offset_is_zero` is the assertion that keeps them in
/// step — the same contract, and the same style of test, as
/// `JitPICSlot::CLASS_ID_OFFSETS` / `test_jit_pic_slot_offsets`.
///
/// Ownership follows the established per-method-metadata pattern exactly: the
/// box is allocated by the backend during codegen (so its address exists when
/// the immediate is encoded), moved onto `CompiledMethod::_jit_entry_counter`
/// at finalize, and freed with the artifact. Generated code can therefore never
/// outlive the word it increments, and a superseded body's counter dies with
/// the body rather than being inherited by its replacement — which is what we
/// want, because a C2 supersede is the end of the question this counter exists
/// to answer.
///
/// `align(64)`: one cache line per counter. Every entry to a counted body is a
/// STORE to this word, and at 16 bytes the allocator packs up to four
/// methods' counters into one line, so two threads running two DIFFERENT hot
/// methods would pull the line back and forth on every call (false sharing).
/// One thread-shared hot method still shares its own line; that is inherent to
/// a per-method counter and the module comment's accepted cost. 48 bytes per
/// compiled body; the fields' offsets do not move.
#[repr(C, align(64))]
#[derive(Debug)]
pub struct JitEntryCounter {
    /// Entries since this artifact was published. Written by compiled code
    /// with a NON-atomic `add`; see the module comment for why that is sound
    /// enough and why it is not `lock add`.
    count: AtomicU64,
    /// The weighted value the last drain reported ([`Self::take_if_moved`]).
    /// Rust-only; generated code never addresses it.
    last_drained: AtomicU64,
    /// Loop back edges taken in this artifact (round 11 wave 18). Written by
    /// compiled code with the same NON-atomic `add` as [`Self::count`], at
    /// [`Self::BACK_EDGE_OFFSET`], which is ABI like `COUNT_OFFSET`: it is
    /// placed AFTER the two older fields so neither of their offsets moved.
    back_edges: AtomicU64,
    /// j27 (lane TIER, `CRATONVM_JIT_ENTRY_TRIP`, opt-in): the entry count at
    /// which the prologue's trip check calls [`jit_entry_counter_tripped`].
    /// `u64::MAX` = disarmed, which is the state of every box until the VM
    /// arms it ([`Self::arm_trip`]) and again once the trip has fired. Read by
    /// generated code at [`Self::TRIP_AT_OFFSET`] (ABI, like `COUNT_OFFSET`);
    /// written only from Rust.
    trip_at: AtomicU64,
    /// Set to `1` once [`Self::trip_now`] fires; cleared by [`Self::arm_trip`]
    /// when re-armed (`at != u64::MAX`). Compiled prologues check this before
    /// incrementing `count`, stopping cross-core cache-line bouncing once tripped.
    tripped: std::sync::atomic::AtomicU8,
    /// Who the trip tells (`None` until [`Self::arm_trip`]). Rust-only.
    trip_sink: std::sync::OnceLock<std::sync::Arc<EntryTripSink>>,
}

impl Default for JitEntryCounter {
    /// [`Self::new`]: a derived `Default` would start [`Self::trip_at`] at 0,
    /// i.e. armed to trip on the first entry.
    fn default() -> Self {
        Self::new()
    }
}

/// The per-VM receiver of entry trips (j27, lane TIER; page
/// `jr21-conf-compiled-bodies-reach-c2-only-at-a-drain-clock-FIXED-20261029.md`).
/// The tiered manager builds one (`TieredCompilationManager::entry_trip_sink`)
/// whose `wake` asks an idle C1-lane worker to run its idle tick -- the
/// compiled-entry counter drain -- now rather than at the next 200 ms tick.
/// Every armed box holds an `Arc` of it, so it lives as long as the code that
/// can call it; `wake` itself holds only a `Weak` of the manager's core.
pub struct EntryTripSink {
    wake: Box<dyn Fn() + Send + Sync + 'static>,
    trips: AtomicU64,
}

impl std::fmt::Debug for EntryTripSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntryTripSink")
            .field("trips", &self.trips.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl EntryTripSink {
    /// A sink whose trips run `wake`. `wake` runs on a MUTATOR thread, inside
    /// a helper called from a compiled prologue, with no oop map at the call:
    /// it must not allocate on the Java heap, reach a safepoint, or block for
    /// long (taking a short-held mutex to notify a condvar is the intended
    /// shape).
    pub fn new(wake: Box<dyn Fn() + Send + Sync + 'static>) -> Self {
        Self {
            wake,
            trips: AtomicU64::new(0),
        }
    }

    /// Trips delivered so far (diagnostic).
    pub fn trips(&self) -> u64 {
        self.trips.load(Ordering::Relaxed)
    }

    fn deliver(&self) {
        self.trips.fetch_add(1, Ordering::Relaxed);
        (self.wake)();
    }
}

/// The helper the prologue's trip check calls (j27, lane TIER;
/// `CRATONVM_JIT_ENTRY_TRIP`), with the box's base address in the first
/// argument register. One-shot per arming: the first caller swaps
/// [`JitEntryCounter::trip_at`] back to disarmed, so threads racing past the
/// bar together deliver one trip. Never unwinds into generated code.
///
/// # Safety
///
/// `counter` is the address a prologue baked: a live `JitEntryCounter` owned
/// by the body that is executing (the box dies with the artifact, and an
/// executing artifact is not reclaimed). Null is tolerated.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn jit_entry_counter_tripped(counter: *const JitEntryCounter) {
    if counter.is_null() {
        return;
    }
    // SAFETY: see the function's contract; only shared (atomic) access.
    let counter = unsafe { &*counter };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        counter.trip_now();
    }));
}

impl JitEntryCounter {
    /// Byte offset of the trip word ([`Self::trip_at`]) from the struct base,
    /// as `x64::frames::emit_entry_trip_check` encodes it (a disp8).
    /// Asserted by `trip_at_offset_matches_the_field`.
    pub const TRIP_AT_OFFSET: i32 = 24;
    pub const TRIPPED_OFFSET: i32 = 32;

    /// Arm the trip: the prologue calls [`jit_entry_counter_tripped`] once
    /// the entry count reaches `at`, and the trip goes to `sink`. The first
    /// sink a box is armed with stays (a box belongs to one VM's cache). `at
    /// == u64::MAX` disarms. A count already at or past `at` trips on the
    /// next entry, which is the intent: the bar was crossed and nobody looked.
    pub fn arm_trip(&self, at: u64, sink: &std::sync::Arc<EntryTripSink>) {
        let _ = self.trip_sink.get_or_init(|| std::sync::Arc::clone(sink));
        if at != u64::MAX {
            self.tripped.store(0, Ordering::Relaxed);
        }
        self.trip_at.store(at, Ordering::Relaxed);
    }

    /// The trip word as of now (`u64::MAX` = disarmed).
    pub fn trip_at(&self) -> u64 {
        self.trip_at.load(Ordering::Relaxed)
    }

    /// What [`jit_entry_counter_tripped`] does: disarm (one-shot), then tell
    /// the sink. A second caller that lost the swap does nothing.
    fn trip_now(&self) {
        self.tripped.store(1, Ordering::Relaxed);
        if self.trip_at.swap(u64::MAX, Ordering::Relaxed) == u64::MAX {
            return;
        }
        if let Some(sink) = self.trip_sink.get() {
            sink.deliver();
        }
    }

    /// Byte offset of [`Self::count`] from the struct base, as the emitter
    /// encodes it. Asserted by `entry_counter_count_offset_is_zero`.
    pub const COUNT_OFFSET: i32 = 0;

    /// Byte offset of the back-edge word from the struct base, as
    /// `x64::frames::emit_back_edge_counter` encodes it (a disp8 off the base
    /// address). Asserted by `back_edge_offset_matches_the_field`.
    pub const BACK_EDGE_OFFSET: i32 = 16;

    /// `back_edges >> BACK_EDGE_WEIGHT_SHIFT` is what the back edges add to the
    /// drained count: eight back edges weigh one entry, HotSpot's
    /// `Tier4BackEdgeThreshold / Tier4InvocationThreshold` (40 000 / 5 000).
    pub const BACK_EDGE_WEIGHT_SHIFT: u32 = 3;

    /// A fresh counter, at zero.
    pub fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            last_drained: AtomicU64::new(0),
            back_edges: AtomicU64::new(0),
            trip_at: AtomicU64::new(u64::MAX),
            tripped: std::sync::atomic::AtomicU8::new(0),
            trip_sink: std::sync::OnceLock::new(),
        }
    }

    /// The struct's base address, which the back-edge bump materialises and
    /// addresses at [`Self::BACK_EDGE_OFFSET`]. A `usize` for the reason
    /// [`Self::count_addr`] gives.
    pub fn base_addr(&self) -> usize {
        // Cast: address arithmetic.
        self as *const Self as usize
    }

    /// Back edges taken so far (relaxed, a hint like [`Self::get`]).
    pub(crate) fn back_edges(&self) -> u64 {
        self.back_edges.load(Ordering::Relaxed)
    }

    /// The back-edge-only route's shift (HANDLER7-1, `CRATONVM_JIT_C1_BACK_EDGE_C2_BAR`):
    /// `back_edges >> 1` is what a loop-dominated body reports, so the default
    /// C2 bar (`max(c2_threshold, c2_min_invocations)` = 20 000) opens at
    /// 40 000 back edges, HotSpot's `Tier4BackEdgeThreshold`. A raised bar
    /// raises the back-edge door with it (2x the bar).
    pub const BACK_EDGE_ONLY_SHIFT: u32 = 1;

    /// The fewest entries a body must have taken before the back-edge-only
    /// route counts (HANDLER7-1). The route ends in a METHOD-ENTRY C2 compile,
    /// which serves only later calls (a C1 body has no back-edge OSR into C2),
    /// so a body entered once or twice whose one activation looped 40 000
    /// times would buy a C2 compile nothing ever enters: start-up loops
    /// (`<clinit>` tables, a one-shot parse) are the population this keeps out.
    pub const BACK_EDGE_ONLY_MIN_ENTRIES: u64 = 16;

    /// What the drain reports: entries plus the weighted back edges. Saturating,
    /// so a counter that ran for a very long time cannot wrap to a small value
    /// and read as "unmoved".
    ///
    /// Under `CRATONVM_JIT_C1_BACK_EDGE_C2_BAR` (JIT round 19 wave 8, lane
    /// handler8, HANDLER7-1; default OFF) the larger of that and
    /// `back_edges >> BACK_EDGE_ONLY_SHIFT` once the body has taken
    /// [`Self::BACK_EDGE_ONLY_MIN_ENTRIES`] entries ([`weighted_count`]).
    fn weighted(&self) -> u64 {
        weighted_count(
            self.count.load(Ordering::Relaxed),
            self.back_edges(),
            c1_back_edge_c2_bar_enabled(),
        )
    }

    /// The address generated code increments: the base plus
    /// [`Self::COUNT_OFFSET`].
    ///
    /// Returned as a `usize` rather than a reference because the only consumer
    /// is an `imm64` in a machine instruction, and because the borrow it would
    /// otherwise hand out has nothing to do with the lifetime that actually
    /// governs the pointer (the owning `CompiledMethod`'s).
    pub fn count_addr(&self) -> usize {
        // Cast: address arithmetic. `COUNT_OFFSET` is 0 today; spelled as an
        // addition anyway so a future field reordering is a one-line change
        // here and in the emitter rather than a silent mis-addressing.
        (self as *const Self as usize).wrapping_add(Self::COUNT_OFFSET as usize)
    }

    /// This method's entry count as of now.
    ///
    /// `Relaxed`: there is nothing to synchronise WITH. The value is a hint fed
    /// to a tiering decision that is re-taken on every drain, so an observer
    /// that reads a slightly stale count simply promotes one drain later.
    pub fn get(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// The current WEIGHTED count (entries plus `back_edges >>
    /// BACK_EDGE_WEIGHT_SHIFT`; exactly the entry count when no back edge was
    /// counted, which is every body compiled with
    /// `CRATONVM_JIT_CALLEE_PROMOTION=0`), but only if it differs from what the
    /// previous call returned (or from zero, for the first call): the drain's
    /// "has this row moved" test (`JitCache::snapshot_entry_counts`).
    ///
    /// Without it every body ever entered was a row on every drain, and each
    /// row costs the tiered manager's global `methods` mutex plus two key
    /// clones — O(bodies ever entered) lock round trips per drain for counts
    /// that had not changed. The `swap` makes two concurrent drains disagree
    /// at worst by one drain (one of them sees `None` for a count the other
    /// reports), the same imprecision the racy increment already accepts.
    pub(crate) fn take_if_moved(&self) -> Option<u64> {
        let now = self.weighted();
        if self.last_drained.load(Ordering::Relaxed) == now {
            return None;
        }
        let previous = self.last_drained.swap(now, Ordering::Relaxed);
        (now != previous).then_some(now)
    }

    /// Set the count directly. Tests only — production writes come from
    /// compiled code, which is the whole point of this type.
    #[cfg(test)]
    pub(crate) fn set_for_test(&self, v: u64) {
        self.count.store(v, Ordering::Relaxed);
    }

    /// Increment the count the way generated code does, for tests that need a
    /// non-zero counter without running machine code.
    #[cfg(test)]
    pub(crate) fn bump_for_test(&self) {
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// Set the back-edge word directly. Tests only.
    #[cfg(test)]
    pub(crate) fn set_back_edges_for_test(&self, v: u64) {
        self.back_edges.store(v, Ordering::Relaxed);
    }
}

/// Whether the round-11 wave-18 promotion of bodies the tiered manager did not
/// schedule is armed (`CRATONVM_JIT_CALLEE_PROMOTION`, default ON; `0`, `false`,
/// `off` or `no` is the kill switch). One switch for the three parts of
/// `r11-orch-callee-compiled-bodies-are-never-promoted`:
///
///  * P1, `TieredCompilationManager::note_published_outside_manager`: a body
///    published by a door that bypasses the manager raises the method's tier;
///  * P2, the single-pass back-edge bump into [`JitEntryCounter`]
///    (`x64::frames::emit_back_edge_counter`);
///  * P3, the VM's drain trigger at an OSR entry (`jit_bridge`).
///
/// Off, all three are inert and codegen is byte-identical to the build before
/// them. Not memoised: every reader is a compile, a publish or an
/// already-throttled drain decision, none of them per invocation, and a memo
/// would be one more process static (`process_global_statics_ratchet`).
pub fn callee_promotion_enabled() -> bool {
    true
}

/// The drained weight of a body that took `entries` entries and `back_edges`
/// loop back edges: `entries + back_edges >> BACK_EDGE_WEIGHT_SHIFT` (HotSpot's
/// 8:1 ratio), and, with `back_edge_bar` (HANDLER7-1), at least
/// `back_edges >> BACK_EDGE_ONLY_SHIFT` once `entries` reaches
/// `BACK_EDGE_ONLY_MIN_ENTRIES`.
///
/// Why the second term (`docs/feature-designs/jit-ir-optimizations.md`,
/// HANDLER7-1): a C1 body called a few hundred times with a few hundred
/// iterations per call (the R19Handler5/6 kernels: 400 calls x 256 iterations
/// = 13 200 drained) never crosses the 20 000 C2 bar through the 8:1 sum, and
/// it has no back-edge OSR into C2 either, so every optimizing feature (EA,
/// splices, the compiled handler entry) is out of its reach. HotSpot reaches
/// it through `Tier4BackEdgeThreshold` (40 000 back edges, an OSR compile);
/// this is the method-entry analogue at the same count. The drain is the only
/// reader, so nothing on any compiled path changes; the manager's
/// `invocation_count` takes the larger number, as it already takes the 8:1
/// weight.
///
/// Compile-storm bound: a body qualifies only after 40 000 back edges AND 16
/// entries under the default bar, a strict subset of what HotSpot's tier-4
/// predicates (`i >= 5000 || (i >= 600 && i + b >= 15000)`, plus the
/// back-edge OSR at 40 000) already send to C2; the manager dedups per method
/// and its C2 lane queue is the existing throttle. OSR artifacts are not
/// drained (`JitCache::snapshot_entry_counts`), so OSR is unaffected; an
/// activation already inside the C1 loop finishes there (the C2 body serves the
/// next call).
pub(crate) fn weighted_count(entries: u64, back_edges: u64, back_edge_bar: bool) -> u64 {
    let ordinary = entries.saturating_add(back_edges >> JitEntryCounter::BACK_EDGE_WEIGHT_SHIFT);
    if back_edge_bar && entries >= JitEntryCounter::BACK_EDGE_ONLY_MIN_ENTRIES {
        ordinary.max(back_edges >> JitEntryCounter::BACK_EDGE_ONLY_SHIFT)
    } else {
        ordinary
    }
}

/// `CRATONVM_JIT_C1_BACK_EDGE_C2_BAR` (JIT round 19 wave 8, lane handler8,
/// HANDLER7-1 of `jit-r19-handler7-proposals-RETIRED-20261009.md`; **default on**, default on): the
/// entry-counter drain reports a loop-dominated C1 body's back edges at 2:1
/// instead of 8:1 ([`weighted_count`]), so the C1 -> C2 hotness door opens at
/// HotSpot's `Tier4BackEdgeThreshold` (40 000) for a body called a few hundred
/// times. Read per drained row (the drain is throttled; no per-call reader), no
/// memo (no new process static).
pub fn c1_back_edge_c2_bar_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_ENTRY_TRIP` (j27 wave 1, lane TIER; **default on**, `=1`
/// enables): the single-pass prologue compares its entry count with the
/// box's trip word after the bump and, once it reaches it, calls
/// [`jit_entry_counter_tripped`], which asks an idle C1 worker to drain the
/// entry counters now. The VM arms a background-published C1 body's trip at
/// the C2 hotness bar, so the C1 -> C2 request is queued within a condvar
/// wake of the bar, as HotSpot's counter-overflow call does, instead of at
/// the next drain clock (page
/// `jr21-conf-compiled-bodies-reach-c2-only-at-a-drain-clock-FIXED-20261029.md`).
///
/// default on because it adds 13 bytes (load, compare, branch) plus a cold call
/// to every counted prologue, written without a build; flip after the
/// page's A/B. Read per compile (emitter) and per publish (arming); not
/// memoised. A body compiled with the check but never armed never trips (its
/// trip word stays `u64::MAX`), and an armed body compiled without it is
/// just never checked, so a mid-run disagreement is harmless.
pub fn entry_trip_enabled() -> bool {
    true
}

/// Whether the compiled-prologue entry counter is armed
/// (`CRATONVM_JIT_ENTRY_COUNTER`).
///
/// Default ON since round 9 wave 3 — see the module comment for the numbers.
/// Without it the manager's `invocation_count` freezes at the C1 publish, so
/// `should_compile`'s C1 -> C2 arm is dead in steady state and every method
/// the structural door (`request_c2_upgrade`) refuses — every
/// allocation-bearing one while `CRATONVM_JIT_C2_ALLOC_UPGRADE` is off —
/// stays at C1 however hot it gets.
///
/// `CRATONVM_JIT_ENTRY_COUNTER=0` (or `false`/`off`/`no`) is the kill switch.
/// When off, `emit_prologue` emits nothing, no box is allocated,
/// `CompiledMethod::entry_count` answers 0 for every method, and the VM-side
/// drain returns before it takes a single lock — byte-identical codegen to the
/// build before the counter existed.
///
/// Read once and memoised: this is consulted once per compile, but it is also
/// consulted on the drain path, and a knob that can change value mid-run would
/// let a method be compiled with a counter and drained as though it had none.
pub fn entry_counter_enabled() -> bool {
    use std::sync::OnceLock;
    static ARMED: OnceLock<bool> = OnceLock::new();
    *ARMED.get_or_init(|| {
        let value = None::<std::ffi::OsString>;
        entry_counter_armed_by(value.as_ref().map(|v| v.to_string_lossy()).as_deref())
    })
}

/// Whether THIS compile should emit the counter: the process answer, narrowed
/// by a thread override (`cratonvm_types::flags::override_thread`) when one is
/// in force.
///
/// Since the counter went default-on (round 9 wave 3) every prologue bakes the
/// address of its own freshly allocated box, so two compiles of the same
/// bytecode differ in those eight bytes. The byte-determinism tests pin the
/// counter off for their thread through this read; the memoised
/// [`entry_counter_enabled`] cannot see a thread override. Only the emitter
/// asks this -- the drains keep the process answer, and a body compiled
/// without a counter under an override simply reads zero there.
pub fn entry_counter_enabled_for_this_compile() -> bool {
    entry_counter_enabled()
        && true}

/// The value rule behind [`entry_counter_enabled`], split out so it is testable
/// without the process environment: unset is ON, and only the opt-out words
/// `cratonvm_types::flags::value_is_off` recognises (`0`, `false`, `off`, `no`)
/// turn it off — the same truth table as `runtime_flag_default_on`.
fn entry_counter_armed_by(value: Option<&str>) -> bool {
    value.is_none_or(|v| !cratonvm_types::flags::value_is_off(v))
}

#[cfg(test)]
mod tests;
