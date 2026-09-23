// SPDX-License-Identifier: Apache-2.0
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
//! `docs/internal/jit-review-r9/NOTES-w3-tiering3.md`): the same binary, flag
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
//!    cost with no consumer.
//!  * **A compile that bails after the prologue.** The box is dropped with the
//!    `Compiler`, so nothing leaks and nothing dangles.

use std::sync::atomic::{AtomicU64, Ordering};

/// One published method's entry count, addressed directly by generated code.
///
/// `#[repr(C)]` and single-field on purpose: the prologue bakes the address of
/// [`Self::count`] as an `imm64` and addresses it with a fixed displacement, so
/// the field's offset is part of the ABI between this struct and the emitter.
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
#[repr(C)]
#[derive(Debug, Default)]
pub struct JitEntryCounter {
    /// Entries since this artifact was published. Written by compiled code
    /// with a NON-atomic `add`; see the module comment for why that is sound
    /// enough and why it is not `lock add`.
    count: AtomicU64,
}

impl JitEntryCounter {
    /// Byte offset of [`Self::count`] from the struct base, as the emitter
    /// encodes it. Asserted by `entry_counter_count_offset_is_zero`.
    pub const COUNT_OFFSET: i32 = 0;

    /// A fresh counter, at zero.
    pub fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
        }
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
        let value = cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_ENTRY_COUNTER");
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
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_ENTRY_COUNTER")
}

/// The value rule behind [`entry_counter_enabled`], split out so it is testable
/// without the process environment: unset is ON, and only the opt-out words
/// `cratonvm_types::flags::value_is_off` recognises (`0`, `false`, `off`, `no`)
/// turn it off — the same truth table as `runtime_flag_default_on`.
fn entry_counter_armed_by(value: Option<&str>) -> bool {
    value.is_none_or(|v| !cratonvm_types::flags::value_is_off(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The emitter bakes `&counter.count` and addresses it at
    /// `COUNT_OFFSET`. If a field is ever added ahead of `count`, the
    /// prologue would increment the wrong word — silently, since the value it
    /// corrupts is only read by a heuristic. This is that guard, and it is the
    /// same shape as `test_jit_pic_slot_offsets`.
    #[test]
    fn entry_counter_count_offset_is_zero() {
        let c = JitEntryCounter::new();
        let base = &c as *const JitEntryCounter as usize;
        assert_eq!(
            c.count_addr() - base,
            JitEntryCounter::COUNT_OFFSET as usize,
            "count_addr must be base + COUNT_OFFSET"
        );
        assert_eq!(
            JitEntryCounter::COUNT_OFFSET,
            0,
            "the emitter encodes COUNT_OFFSET as a disp8; a non-zero value \
             needs the emitter updated in the same commit"
        );
    }

    /// The word the emitter increments is 8 bytes wide and 8-byte aligned.
    /// Both halves matter: the instruction is a 64-bit `add`, and the
    /// no-tearing argument in the module comment is an argument about ALIGNED
    /// accesses.
    #[test]
    fn entry_counter_word_is_an_aligned_u64() {
        assert_eq!(std::mem::size_of::<JitEntryCounter>(), 8);
        assert_eq!(std::mem::align_of::<JitEntryCounter>(), 8);
        let c = JitEntryCounter::new();
        assert_eq!(c.count_addr() % 8, 0, "counter word must be 8-byte aligned");
    }

    /// Default-ON with an explicit kill switch (round 9 wave 3). Unset — the
    /// default configuration — must arm the counter, or the hotness door into
    /// the optimizing tier is dead again for every method the structural
    /// door refuses; the opt-out words must disarm it, or the kill switch is
    /// not one.
    #[test]
    fn the_entry_counter_is_default_on_with_a_kill_switch() {
        assert!(entry_counter_armed_by(None), "unset must arm the counter");
        for on in ["1", "true", "on", "yes", ""] {
            assert!(entry_counter_armed_by(Some(on)), "{on:?} must arm it");
        }
        for off in ["0", "false", "off", "no", " OFF ", "No"] {
            assert!(!entry_counter_armed_by(Some(off)), "{off:?} must disarm it");
        }
    }

    #[test]
    fn a_fresh_counter_reads_zero() {
        assert_eq!(JitEntryCounter::new().get(), 0);
        assert_eq!(JitEntryCounter::default().get(), 0);
    }

    #[test]
    fn bumping_is_visible_to_the_reader() {
        let c = JitEntryCounter::new();
        c.bump_for_test();
        c.bump_for_test();
        assert_eq!(c.get(), 2);
        c.set_for_test(19_999);
        assert_eq!(c.get(), 19_999);
    }

    /// A boxed counter's address is stable across moves of the `Box`, which is
    /// the property that makes it legal to bake into an immediate at codegen
    /// time and then move the box onto the `CompiledMethod`. If this ever
    /// stopped holding, every published body would be incrementing a freed
    /// word.
    #[test]
    fn boxing_keeps_the_address_stable() {
        let boxed = Box::new(JitEntryCounter::new());
        let addr = boxed.count_addr();
        let moved = boxed;
        assert_eq!(moved.count_addr(), addr);
        let in_a_vec = vec![moved];
        assert_eq!(in_a_vec[0].count_addr(), addr);
    }
}
