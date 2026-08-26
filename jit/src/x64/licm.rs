// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Loop-invariant code motion.
//!
//! Moved verbatim out of `x64.rs`'s `Loop-Invariant Code Motion (LICM)`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.

use super::*;
use crate::scev::{BoundSource, BoundTerm, PreheaderGuard};


/// A loop-invariant `aload X; iload Y; aaload` sequence that can be hoisted
/// out of a loop body. The hoisted value (a row pointer from an Object[][] array)
/// is computed once before the loop and cached in a spill slot.
pub(super) struct LoopHoist {
    /// Bytecode PC of the loop header (back-edge target).
    pub(super) loop_header: usize,
    /// First PC strictly after the back-edge instruction (`loop_end`).
    /// All PCs `p` with `loop_header <= p < loop_end` are part of the loop
    /// body. Tracked here so the OSR entry path can detect when a PC lives
    /// inside a hoisted loop without re-running `detect_loops`.
    pub(super) loop_end: usize,
    /// First bytecode PC of the invariant sequence (the aload instruction).
    pub(super) seq_start: usize,
    /// Bytecode PC after the invariant sequence (past the aaload).
    pub(super) seq_end: usize,
    /// Local variable index for the array reference.
    pub(super) array_local: usize,
    /// Local variable index for the array index.
    pub(super) index_local: usize,
}

/// Information about a loop-invariant FP load that can be hoisted.
/// Pattern: dload/fload of a local that is not modified within the loop body.
#[derive(Debug)]
#[allow(dead_code)]
pub(super) struct FpLoopHoist {
    /// Bytecode PC of the loop header (back-edge target).
    pub(super) loop_header: usize,
    /// Bytecode PC of the invariant dload/fload instruction.
    pub(super) load_pc: usize,
    /// Local variable index being loaded.
    pub(super) local_idx: usize,
    /// true = double (dload), false = float (fload).
    pub(super) is_double: bool,
}

/// Information about a vectorizable double-array sum reduction loop.
/// Pattern: for (i = start; i < bound; i++) sum += arr[i]
/// where arr is a double[] and sum is a double local.
#[derive(Debug)]
#[allow(dead_code)]
pub(super) struct SimdFpArraySum {
    /// Bytecode PC of the loop header.
    pub(super) header_pc: usize,
    /// Bytecode PC of the back-edge instruction.
    pub(super) back_edge_pc: usize,
    /// Local index of the induction variable (i).
    pub(super) iv_local: usize,
    /// Local index of the accumulator (sum).
    pub(super) acc_local: usize,
    /// Local index of the array reference.
    pub(super) array_local: usize,
    /// Local index of the loop bound.
    pub(super) bound_local: usize,
    /// Operation: 0x58=ADD (sum), 0x59=MUL (dot product partial).
    pub(super) sse_op: u8,
}

/// Get the byte length of a bytecode instruction at `pc`.
pub(crate) fn bytecode_len_at(code: &[u8], pc: usize) -> usize {
    match code[pc] {
        // 2-byte: bipush(0x10), ldc(0x12), iload..aload(0x15..0x19),
        // istore..astore(0x36..0x3a), ret(0xa9), newarray(0xbc).
        // `ldc` (0x12) was previously absent and fell through to the `_ => 1`
        // arm — a 1-byte under-count that misaligned every PC-stepping consumer
        // (branch-target precompute, DCE, OSR/unroll). When an `ldc` sat
        // immediately before a branch (e.g. `ldc 65536; if_icmpge exit` — the
        // standard `for (i; i<CONST; …)` header), the scan skipped the branch,
        // never marked its exit target, DCE-killed that target, and left the
        // loop-exit `if_icmpge` unpatched (rel32=0) → the loop overran its bound
        // (BC SPHINCS-256 Horst.horst_sign AIOOBE).
        0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3a | 0xa9 | 0xbc => 2,
        // 3-byte: sipush(0x11), ldc_w(0x13), ldc2_w(0x14), iinc(0x84), jsr(0xa8),
        // the if_* family, field/invoke ops, etc. ldc_w/ldc2_w were also absent.
        0x11
        | 0x13
        | 0x14
        | 0x84
        | 0xa8
        | 0x99..=0xa6
        | 0xa7
        | 0xb2
        | 0xb3
        | 0xb4
        | 0xb5
        | 0xb6
        | 0xb7
        | 0xb8
        | 0xbd
        | 0xc0
        | 0xc1
        | 0xc6
        | 0xc7 => 3,
        0xbb => 3, // new
        0xc5 => 4,
        // 5-byte instructions. invokeinterface (0xb9: opcode, cp_hi, cp_lo,
        // count, 0) and invokedynamic (0xba: opcode, cp_hi, cp_lo, 0, 0) are
        // both reachable in a compiled method today (`jit_scan` accepts
        // both). The wide-offset branches goto_w (0xc8) / jsr_w (0xc9:
        // opcode + 4-byte signed offset) are still rejected by `jit_scan`
        // (catch-all → `None`), so no compiled method contains them — but,
        // like `wide` (0xc4) below, the length table must stay correct as
        // defense-in-depth so every PC-stepping consumer (branch-target
        // precompute, DCE, OSR/unroll, instruction-start map, oop-map dataflow)
        // stays in lockstep if any is ever accepted. A missing entry
        // under-counts by 4 bytes and misaligns the walk — the same class of
        // bug as the previously-absent `ldc`. Keep the regalloc.rs `bc_len`
        // twin in sync.
        0xb9 | 0xba | 0xc8 | 0xc9 => 5,
        // wide (0xc4) — prefix modifies the following opcode to use a 2-byte
        // local index. JVMS §6.5 wide: `wide <opcode> <indexbyte1> <indexbyte2>`
        // is 4 bytes for the load/store/ret family, and `wide iinc <index>
        // <const>` is 6 bytes (extra 2-byte signed constant). The modified
        // opcode is the byte at `pc + 1`: only `iinc` (0x84) takes the 6-byte
        // form. Currently latent — `jit_scan` rejects `wide`, so no compiled
        // method contains it — but the length table must stay correct as
        // defense-in-depth so every PC-stepping consumer stays in lockstep if
        // `wide` is ever accepted. Keep the regalloc.rs `bc_len` twin in sync.
        0xc4 => {
            if pc + 1 < code.len() && code[pc + 1] == 0x84 {
                6 // wide iinc
            } else {
                4 // wide <load/store/ret>
            }
        }
        // tableswitch — variable length
        0xaa => {
            let mut p = pc + 1;
            while p % 4 != 0 {
                p += 1;
            }
            // Truncated header: a tableswitch placed near the end of `code` may
            // not carry the full 12-byte default/low/high header. Reading it
            // would index past `code_len` and panic. Return a length that
            // consumes the rest of `code` so any walker that uses this helper
            // terminates without an OOB read; the main compile loop's own
            // `pc + 12 > code_len` guard then rejects the method.
            if p + 12 > code.len() {
                return code.len() - pc;
            }
            let low = i32::from_be_bytes([code[p + 4], code[p + 5], code[p + 6], code[p + 7]]);
            let high = i32::from_be_bytes([code[p + 8], code[p + 9], code[p + 10], code[p + 11]]);
            // Checked `high - low + 1`: raw i32 arithmetic overflows on
            // attacker-controlled bounds. Such methods are already rejected by
            // `jit_scan`; if one ever reaches here, fall back to a zero count
            // (header-only length) rather than overflowing the address math.
            let count = checked_tableswitch_count(low, high).unwrap_or(0);
            (p + 12 + count * 4) - pc
        }
        // lookupswitch — variable length
        0xab => {
            let mut p = pc + 1;
            while p % 4 != 0 {
                p += 1;
            }
            // Truncated header (see tableswitch above): bail to a remainder
            // length rather than reading the 8-byte default/npairs header OOB.
            if p + 8 > code.len() {
                return code.len() - pc;
            }
            // A negative `npairs` (crafted bytecode) cast straight to usize would
            // become an enormous value and overflow the address math below; clamp
            // to 0 so the length stays sane (the main loop rejects such methods).
            let npairs = i32::from_be_bytes([code[p + 4], code[p + 5], code[p + 6], code[p + 7]])
                .max(0) as usize; // Widening: always safe
            (p + 8 + npairs * 8) - pc
        }
        _ => 1,
    }
}

/// array_receiver_local soundness fix — build a bitmap of valid instruction
/// START offsets for `code[..code_len]`.
///
/// Backward scans (e.g. reading `code[pc - 1]` and guessing the opcode) are
/// unsound: a multi-byte instruction's trailing OPERAND byte can collide with
/// a real opcode value, so a naive "previous byte" decode mis-identifies the
/// instruction. The only reliable way to know whether a given offset is an
/// instruction boundary is to walk FORWARD from PC 0 stepping by
/// [`bytecode_len_at`] (the same walk used by [`compute_branch_targets`] and
/// the oop-map dataflow). `starts[k]` is `true` iff `k` is the first byte of
/// some instruction reached by that linear walk.
///
/// Callers use this to *validate* a candidate instruction position before
/// trusting a backward-derived decode; when the candidate is not a real
/// instruction start the caller must fall back to the conservative path.
pub(super) fn instruction_start_map(code: &[u8], code_len: usize) -> Vec<bool> {
    let mut starts = vec![false; code_len];
    let mut pc = 0usize;
    while pc < code_len {
        starts[pc] = true;
        let len = bytecode_len_at(code, pc).max(1); // never advance 0 → no infinite loop
        pc += len;
    }
    starts
}

/// EC-SCALAR-SOUNDNESS (bc math-ec JIT miscompile fix) — compute the set of
/// bytecode PCs that are the TARGET of any branch (conditional, `goto`,
/// `goto_w`, `jsr`, `jsr_w`, `tableswitch`, `lookupswitch`).
///
/// The single-linear-pass escape / scalar-replacement analyses
/// (`analyze_escapes`, `plan_scalar_replacement`) carry abstract operand-stack
/// and per-local provenance straight through the bytecode without resetting at
/// basic-block boundaries. That is only sound for straight-line code: at a
/// merge point (a branch target reachable from more than one predecessor) the
/// linear state need not match the real verification-time state on every
/// incoming edge. To keep those analyses sound we treat every branch *source*
/// (handled inline at each branch opcode) AND every branch *target* (via this
/// set) as a hard barrier that drops all tracked provenance, confining any
/// scalar-replaced object to a single-entry / single-exit straight-line
/// region.
///
/// Returns a bit-set (`Vec<bool>` indexed by PC) of size `code_len`.
/// Stage 3 (precise oop maps) — process-wide gate for the *moving*-safe
/// precise-stack-map machinery (exact RBP frame registration, safepoint-id
/// slot, precise relocation).
///
/// **DEFAULT ON** (re-flipped 2026-07-07). Opt out with `CRATONVM_NO_PRECISE_JIT_MAPS=1`.
/// (`CRATONVM_PRECISE_JIT_MAPS` is now a no-op — the coverage is the default.)
///
/// The BUG-01 ~6× throughput tax that had motivated the d53c0e96 default-OFF flip
/// is **gone on current dev**: intervening JIT improvements (more inlining → far
/// fewer real call safepoints in the hot reflection/framework methods) cut the
/// precise per-safepoint cost to noise. Measured 2026-07-07 on the Linux
/// spring-core suite: `ObjectUtilsTests`/`ClassUtilsTests` are byte-for-byte the
/// same wall time precise-on vs -off even at `JIT_THRESHOLD=50`; a 40-class
/// spring-util reflection batch is 73.5 s on vs 70.5 s off (~4%) with **identical
/// pass counts (1059/1061)**. GC-root coverage verified on with it: `binarytrees
/// 14 @GC_STRESS=4096` → `3222190` clean, and the Fork6 GC_STRESS outcome A/B is
/// 14/15 ALL-OK on == off (the higher young-mark marker count under precise-on is
/// benign guard-contained over-retention, not worse outcomes). See BUG-01 doc:
/// `fixed-suite-bugs/app-jvm-bugs/bug-01-junit-reflection-heavy-jit-frame-scan-throughput.md`.
///
/// History: default-OFF (d53c0e96) for BUG-01: the per-invocation
/// `frame_record` + per-safepoint sp-id/flush codegen was a ~6× throughput tax on
/// call-heavy JIT'd code (JUnit execution: 105 s → 18 s with this off) on that
/// era's build.
///
/// TRADE-OFF (accepted, residuals tracked): when off, the GC falls back to the
/// conservative deepest-band-only JIT-frame scan, which can miss a live oop held
/// only in a callee-saved register of a *caller* frame — the GC-root-coverage-under-JIT
/// family (SB-CRASH-04 / A2 ReflRepro / A4 Fork6) this machinery was landed to fix
/// (commit 5b8864a0). Opt back in with `CRATONVM_PRECISE_JIT_MAPS=1` to restore that
/// coverage. The follow-up fix is to keep the coverage while cutting the per-call cost
/// (RBP-chain walk at GC time, or selective emission); see the BUG-01 doc.
///
/// When off, no Stage 3 codegen is emitted (byte-identical legacy path); when on,
/// bintrees16/18 == golden (14985902 / 68332206), MinRegexProbe A3 repro green.
/// ## The opt-out does not turn safepoint emission off while moving-young is on
///
/// The decision the codegen actually makes is
/// `precise_jit_maps_enabled() || moving_young_enabled()` (`x64.rs`), and
/// moving-young is default-ON — a moving young generation cannot be served by
/// the conservative fallback, so the OR is correct. The consequence is that
/// `CRATONVM_NO_PRECISE_JIT_MAPS=1` alone changes nothing about safepoint
/// emission, and used to do so **silently**: an A/B on it reads as "precise
/// maps cost nothing" when what actually happened is that both arms had them.
/// That is how an inert lever produces a confident wrong answer, so the
/// override now says so once, and names the flag that really turns it off.
pub fn precise_jit_maps_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        // Flipped back to DEFAULT-ON 2026-07-07 (see the doc comment above):
        // the BUG-01 ~6× throughput tax that motivated the d53c0e96 default-off
        // flip is gone on current dev. Opt out with CRATONVM_NO_PRECISE_JIT_MAPS=1.
        let enabled = cratonvm_types::flags::runtime_var_os("CRATONVM_NO_PRECISE_JIT_MAPS")
            .is_none();
        if !enabled && moving_young_enabled() {
            eprintln!(
                "[cratonvm] WARN: CRATONVM_NO_PRECISE_JIT_MAPS is set but a moving young \
                 generation is enabled, and moving-young requires precise safepoint maps — \
                 the codegen gate is `precise_jit_maps_enabled() || moving_young_enabled()`, \
                 so precise maps stay ON and this flag changes nothing. Add \
                 CRATONVM_NO_MOVING_YOUNG=1 to actually turn them off."
            );
        }
        enabled
    })
}

/// Whether narrow oops force every compact-field access through the helpers.
///
/// The inline compact-field fast paths bake an 8-byte reference load/store at a
/// compile-time offset. Under compressed oops a reference slot is 4 bytes
/// holding `(addr - base) >> shift`, so those emissions would read/write the
/// wrong width and the wrong value. Until the codegen learns to emit the narrow
/// load plus the base+shift transform, compressed oops disable the inline path
/// and `getfield`/`putfield` fall back to `jit_getfield` / `jit_putfield_object`,
/// which go through the width-aware `read_compact_field` / `write_compact_field`.
#[inline]
pub fn narrow_oops_block_inline_fields() -> bool {
    narrow_oops_enabled() || zgc_read_barrier_blocks_inline_fields()
}

/// Whether ZGC's read-path load barrier forces every compact-field access
/// through the helpers -- **stage (a) of `zgc-jit-load-barrier.md`**.
///
/// # The problem this closes
///
/// The inline compact-field fast paths bake a raw 8-byte load at a
/// compile-time offset and hand the result on as an object reference. Once
/// ZGC colours its reference slots, that word is `Z_COLORED_TAG | colour |
/// 42-bit offset` -- not a pointer. JIT code reading it raw would dereference
/// a wild address, or, after relocation, a stale one: a use-after-free with no
/// error path. That is exactly why `zgc_relocation_permitted` refuses to
/// relocate while the JIT is on.
///
/// # Why the fallback is a complete fix and not a stopgap
///
/// `jit_getfield` / `jit_putfield_object` go through the heap's own accessors,
/// and `ZgcRealHeap::get_array_element` (and the field twin) now run
/// `load_barrier_slot` before decoding. So a reference load routed to the
/// helper IS barriered -- forwarded, published to the marker, and self-healed
/// -- by the same code the interpreter uses. The design doc calls the
/// helper-CALL arms "the barrier's cheap escape hatch" for this reason: they
/// are correct today, and inline emission is a throughput optimisation on top,
/// not a correctness prerequisite.
///
/// This is the identical argument, and the identical mechanism, as the
/// compressed-oops clause above: a representation the inline emitter does not
/// understand disables the inline emitter rather than being half-supported.
///
/// The flag lives in `cratonvm-types` rather than `cratonvm-gc` because this
/// crate depends on the gc crate only as a dev-dependency, on purpose -- see
/// the acyclicity note in `jit/Cargo.toml`. Reaching for a real edge to read
/// one bool would trade a documented graph invariant for a convenience.
///
/// # Cost
///
/// Zero unless a ZGC cycle has armed the barrier, which nothing in a default
/// run does. The predicate is a relaxed load of a process-wide flag that stays
/// false for the whole life of an ordinary process.
#[inline]
pub fn zgc_read_barrier_blocks_inline_fields() -> bool {
    cratonvm_types::zgc_read_barrier_armed()
}

/// Whether this build's codegen HONOURS the ZGC read barrier -- the
/// capability, as distinct from
/// [`zgc_read_barrier_blocks_inline_fields`]'s runtime state.
///
/// `zgc_relocation_permitted` runs at VM init, long before any cycle arms a
/// barrier, so asking the runtime predicate there always answers "not armed"
/// and relocation would be refused forever. The question that gate actually
/// needs is *"if a cycle arms the barrier later, will the code this JIT emits
/// respect it?"* -- which is a property of the build.
///
/// A constant `true` since stage (a) landed on 2026-08-13. It is a function
/// and not a `const` so that it has somewhere to state the obligation: **if
/// inline reference emission is ever re-enabled under an armed barrier, this
/// must go back to `false`**, or `zgc_relocation_permitted` silently starts
/// allowing a relocating cycle to hand JIT code stale pointers.
///
/// # Re-examined 2026-08-18, when `JIT_READ_BOUNDS` landed
///
/// The read-side bounds table exists precisely to make the guarded inline
/// `getfield` containment check PASS under a collector that publishes no
/// region bounds -- which is the condition that had been keeping inline
/// reference reads unreachable under ZGC. So it is exactly the kind of change
/// this obligation is about, and it was checked rather than assumed.
///
/// Still `true`, by TWO independent mechanisms, either of which alone suffices:
///
/// 1. ~~**ZGC does not publish.**~~ **SUPERSEDED 2026-08-19 — ZGC publishes
///    now, and mechanism 1 was replaced rather than lost.** The reason it was
///    safe not to publish was that a compact reference slot under ZGC holds a
///    colored word; `feature-designs/zgc-reference-slot-representation.md`
///    measured that premise false for the tree that runs (*"Reference slots
///    are plain pointers; nothing in the heap stores a colored word"*), and
///    `ZgcRealHeap::set_barrier_color` — the sole writer of the colored state
///    — has no non-test caller, so the barrier this predicate is named for is
///    never armed in a real process.
///
///    What took its place is stronger where mechanism 2 is weakest: **arming
///    the barrier CLEARS `JIT_READ_BOUNDS`** (`gc/src/zgc.rs`,
///    `set_barrier_color`). The emitted containment sequence loads those words
///    at RUNTIME, so clearing them disables the inline branch in code that was
///    ALREADY COMPILED — which mechanism 2, an emission-time gate, cannot do.
///    Before 2026-08-19 nothing covered that window; it did not matter only
///    because ZGC published nothing at all.
/// 2. **Emission is blocked outright while the barrier is armed.** Every
///    inline compact-field site is gated on
///    [`narrow_oops_block_inline_fields`], which is
///    `narrow_oops_enabled() || zgc_read_barrier_blocks_inline_fields()`. An
///    armed barrier therefore suppresses the emission, not merely the branch.
///
/// Someone later DID decide ZGC should publish its `conservative_addr_span()`
/// after all — 2026-08-19, to close the 56.9M-call reference-read residual on
/// the default collector. Mechanism 2 survived that unchanged, as predicted;
/// mechanism 1 was rewritten above into the runtime clear that covers
/// already-compiled code. The obligation is unchanged and still binds, and it
/// now has a third clause: **the clear must not race a live inline sequence**,
/// so a real cycle has to arm at a safepoint. That is stated at
/// `set_barrier_color`, where the first non-test caller will read it.
#[inline]
pub fn zgc_codegen_honours_read_barrier() -> bool {
    true
}

/// Default-on inline reference-`putfield` fast path.
///
/// When on, a `putfield` of a reference field emits an inline 16-byte `Value`
/// store INSTEAD of the `jit_putfield_object` helper CALL when the field's OLD
/// value is null (`payload == 0`, so no SATB snapshot is needed). Young
/// receivers require no post barrier. Old receivers take the helper: the
/// inline atomic card mark is emitted only when `inline_card_mark_available()`
/// is true, and that predicate is deliberately a constant `false` — a WildFly
/// JIT boot audit observed an old `org/jboss/modules/Module` reference to a
/// young child left on a CLEAN card, which lets the next minor collection
/// reclaim a reachable object. `jit_putfield_object` is therefore the single
/// source of truth for old-to-young post barriers until the inline sequence
/// has end-to-end coverage. Collector-specific G1/ZGC barriers and non-null
/// old values also retain the validated helper. This is the canonical
/// fresh-object-initialisation pattern (`n.left = newChild`) that dominates
/// allocation-heavy code (object binarytrees). Opt out with
/// `CRATONVM_NO_JIT_INLINE_PUTFIELD`; the former
/// `CRATONVM_JIT_INLINE_PUTFIELD` opt-in is accepted as a compatibility no-op.
///
/// INT-6 (GC audit 2026-07-10), **as corrected by G1-2** (`audits/g1-audit.md`
/// §8.1, 2026-07-31). The previous wording claimed the guarded-getfield
/// receiver check was prepended by "both inline arms"; three emitters did not
/// have it, and the `region_bounds_addr != 0` test it named is not a backend
/// gate. The real premise, and its exceptions:
///
/// **Premise.** The YOUNG test reads `GC_FLAG_OLD_GEN`, and "young receiver ⇒
/// no post barrier" is a GENERATIONAL statement: a minor collection copies the
/// whole young space, so a young→young edge needs no record. It is NOT a G1
/// statement. Under G1 a young region is normally in the collection set and is
/// scanned wholesale — but a region held OUT of the CSet by a JNI pin is
/// reachable only through its remembered set, so an inline store that skips
/// `post_write_barrier_rset` loses that edge and the next pause frees a live
/// referent (`audits/g1-audit.md` §2, §5).
///
/// **What actually gates the backend.** NOT `helpers.region_bounds_addr != 0`:
/// that field is the ADDRESS of the process-global `JIT_REGION_BOUNDS` static
/// (`gc/src/gen_heap.rs`), assigned unconditionally by
/// `vm/src/jit/helpers.rs`, hence a constant `true`. The discriminator is the
/// table's CONTENT — only `GenerationalHeap::store_region_bounds_locked` ever
/// writes it — which is what [`region_bounds_are_live`] reads. The
/// guarded-getfield receiver check (null / alignment / published-region
/// containment) is then the machinery that turns "bounds all zero" into "every
/// receiver takes the full-barrier helper".
///
/// **Exceptions, all closed 2026-07-31 (G1-2):**
///
/// * the two top-level reference-`putfield` arms substituted
///   `emit_trusted_oop_receiver_check` — a bare null test with no containment
///   check — whenever the receiver's operand-stack type was a proven oop. That
///   substitution is now additionally conditional on
///   [`region_bounds_are_live`];
/// * `emit_inline_fresh_ctor_compact_ref_putfield` emitted no receiver guard at
///   all. It now takes the helper outright when bounds are not live, and a null
///   test when they are;
/// * `emit_inline_body_compact_ref_putfield` always had the full containment
///   check and was already safe; it now short-circuits straight to the helper
///   when bounds are not live instead of emitting a guard that can never pass.
///
/// Net effect: under a non-publishing backend (G1/ZGC) NO inline
/// reference-store fast path is reachable, so every JIT reference store runs
/// the collector's own barrier — which is the only formulation implementable in
/// the emitter, since G1's pin state (`G1Region::pin_count`, behind the
/// `regions` mutex) has no lock-free per-region byte the JIT could test.
/// `CRATONVM_NO_JIT_INLINE_PUTFIELD=1` remains the belt-and-braces kill switch.
pub fn inline_putfield_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_NO_JIT_INLINE_PUTFIELD").is_none()
    })
}

/// Does the GC backend have LIVE heap-region bounds published right now?
///
/// G1-2 (`audits/g1-audit.md` §8.1). This is the predicate the inline
/// reference-store emitters need and `helpers.region_bounds_addr != 0` is not.
/// That field holds the address of the process-global `JIT_REGION_BOUNDS`
/// static (`gc/src/gen_heap.rs`), which `vm/src/jit/helpers.rs` assigns from
/// `cratonvm_gc::jit_region_bounds_addr()` unconditionally — the static always
/// exists, so the `!= 0` test is a constant `true` in any real VM and says
/// nothing about the collector.
///
/// What distinguishes the backends is the table's CONTENT. The only writer is
/// `GenerationalHeap::store_region_bounds_locked`, called at construction and
/// at every GC start/end and never storing zeros;
/// `VmHeap::jit_card_table_info` shows the same split from the other side
/// (`Some` for `Generational`, `None` for `G1`/`Zgc`). Under G1/ZGC the six
/// words therefore stay `0` for the process's life.
///
/// Fail-safe in both race directions: a compile that observes the table before
/// the first publish, or after `GenerationalHeap::drop` re-zeroes it, merely
/// reports "not live" and the caller takes the full-barrier helper.
///
/// `bounds_addr == 0` (the JIT unit-test helper table, and any embedding that
/// never wired the field) is likewise "not live" — and checking it here is what
/// keeps a caller from baking a `MOV RDX, 0` + `CMP RAX, [RDX]` containment
/// guard that would fault at run time.
pub fn region_bounds_are_live(bounds_addr: usize) -> bool {
    use std::sync::atomic::{AtomicUsize, Ordering};
    if bounds_addr == 0 {
        return false;
    }
    // SAFETY: `bounds_addr` is non-zero here and is only ever set from
    // `cratonvm_gc::jit_region_bounds_addr()` — the address of the `'static`
    // `JIT_REGION_BOUNDS: JitRegionBoundsTable` whose sole field is
    // `[AtomicUsize; 6]` and which lives for the whole process — or, in this
    // crate's tests, from a `static [AtomicUsize; 6]`. Both are valid,
    // aligned and initialised for the six atomic loads below, and the loads
    // race-freely pair with the collector's `Release` stores.
    //
    // SAFETY: see above — `bounds_addr` is a `'static [AtomicUsize; 6]`, valid,
    // aligned and initialised for the six atomic loads that follow.
    let words = unsafe { &*(bounds_addr as *const [AtomicUsize; 6]) };
    // words = [yf_base, yf_end, yt_base, yt_end, og_base, og_end]
    (0..3).any(|i| {
        let base = words[i * 2].load(Ordering::Acquire);
        let end = words[i * 2 + 1].load(Ordering::Acquire);
        base != 0 && end > base
    })
}

/// Inline TLAB `new` — bump allocation emitted directly in compiled code.
///
/// Default-ON again (bt18-inline-tlab-regression-20260724): the emission is
/// now suspension- and walker-safe — the full object header is written
/// BEFORE the cursor-commit store, which is the single linearization point
/// (x86-64 TSO: stores are not reordered with older stores), and every
/// header field that historically relied on the "TLAB refill zeroes the
/// region" assumption is written explicitly. That closes the publication
/// race for which 1ee92e3fd demoted this path to opt-in — a demotion that
/// re-helperized the hottest allocation path and cost bt18 ~4x
/// (single-cycle 90%-fill young GC and the inline fresh-ctor stores both
/// sat on top of this path).
///
/// Opt out: `CRATONVM_NO_JIT_INLINE_TLAB_NEW=1` routes every `new` through
/// the always-correct `new_object` helper. The legacy opt-in
/// `CRATONVM_ENABLE_UNSAFE_INLINE_TLAB_NEW` remains accepted and is now
/// redundant.
pub fn inline_tlab_new_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_NO_JIT_INLINE_TLAB_NEW").is_none()
    })
}

/// Default-on removal of redundant per-object zero stores from inline TLAB
/// allocation.
///
/// Every production `VmHeap::refill_tlab` backend returns a fully zeroed
/// chunk. Reclaimed generational spans are cleared before reuse, and G1
/// applies the same contract when carving Eden. The inline allocator can
/// therefore stamp only the non-zero / shape-defining header words before
/// publishing the cursor instead of clearing every body/header word again.
/// This is especially material for allocation storms: a compact two-reference
/// node drops seven zero stores while preserving JVM default initialization.
///
/// Opt out with `CRATONVM_NO_JIT_TLAB_ZERO_ELISION=1` to restore the defensive
/// per-object clears for bisection.
pub fn inline_tlab_zero_elision_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_NO_JIT_TLAB_ZERO_ELISION").is_none()
    })
}

pub(super) fn inline_site_is_fresh_ctor_first_store(
    site: &crate::InlineSite,
    cpc: usize,
    field_index: usize,
) -> bool {
    site.method_name == "<init>"
        && !site.field_info.iter().any(|(prior_pc, prior_index, _)| {
            *prior_pc < cpc && *prior_index == field_index && site.callee_code[*prior_pc] == 0xb5
        })
}

/// Opt-IN inline `getfield` fast path (`CRATONVM_JIT_INLINE_GETFIELD`).
///
/// The raw inline path can only null-check the receiver before reading object
/// headers/field cells — no plausibility or containment validation at all.
/// Kept as an explicit opt-in for A/B measurement; the production default is
/// the GUARDED inline path below (`guarded_inline_getfield_enabled`), which
/// validates the receiver against the published heap-region bounds before the
/// raw load and falls back to the checked helper otherwise.
pub fn inline_getfield_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_INLINE_GETFIELD").is_some()
    })
}

/// Default-ON guarded inline `getfield` (perf/throughput-20260710).
///
/// The 2026-07-09 hardening (`Fix JIT getfield receiver validation`,
/// 85baa4219) routed EVERY default-path JIT `getfield` through the checked
/// `jit_getfield` helper. That closed the stale/garbage-receiver SIGSEGV
/// (Tomcat DoHead rc=139) but put a full helper round-trip — boundary note,
/// `is_object_address` region walk + header validation, 16-byte atomic cell
/// read — on one of the hottest ops the JIT emits: bintrees-16 regressed
/// 4.7x, and every field-heavy JIT workload (commons-math `Dfp`, Tomcat
/// serving) pays it on each field read.
///
/// The guarded inline path keeps the property that actually stops the SIGSEGV
/// — never dereference a receiver outside the always-mapped GC arenas — at
/// inline-check cost: null/alignment bit-tests plus the same three-region
/// `[base, end)` containment check that `is_object_address` uses as its
/// gate, reading the GC's process-global `JIT_READ_BOUNDS` table whose
/// address the helpers table carries in `read_bounds_addr`.
///
/// The READ table, not `JIT_REGION_BOUNDS`, since 2026-08-18: that table's
/// emptiness under G1/ZGC is what keeps inline reference STORES unreachable
/// (`audits/g1-audit.md` 8.1), so it could never be filled to make inline
/// READS reachable. `JIT_READ_BOUNDS` answers only the read question -- is
/// this address mapped -- and G1 fills it with its single contiguous arena
/// span. ZGC still publishes nothing, which keeps this path unreachable
/// there, as `zgc_codegen_honours_read_barrier` requires. Receivers that
/// pass are raw-loaded inline (a mapped-arena read cannot fault); everything
/// else — null, unaligned garbage, out-of-heap bits, or a backend that does
/// not publish bounds (G1/ZGC → table all zeros) — branches to the checked
/// helper, preserving its NPE / `i64::MIN`-sentinel semantics exactly.
///
/// FLIPPED BACK OFF then RE-ENABLED, same day (2026-07-10): investigating the
/// ES DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests /
/// IVFKnnFloatVectorQueryTests hang cluster
/// (docs/known-issues/elasticsearch-suite/) found `testSlicesDense` under JIT
/// SIGSEGVing almost immediately with this path default-on (dmesg: `segfault
/// at 4c`, gdb: an AALOAD bounds-check dereferencing a receiver of `0x40` — a
/// small int value used as an array pointer, i.e. a getfield RESULT feeding a
/// later array access got corrupted) — the flag was flipped to opt-in
/// (`CRATONVM_JIT_GUARDED_GETFIELD=1`) pending root-cause. That root cause
/// was found and fixed the SAME DAY, in a different investigation
/// (the WildFly Host Controller invoke-inline-cache SIGSEGV): the vm-side JIT
/// field resolvers fabricated a `(0, false)` "compact slot" for any field
/// with NO genuine registered `CompactLayout` entry, and the compact-offset
/// inline getfield arm trusted it — a REFERENCE field with the fabricated
/// `is_ref=false` fell into the int-category match arm and got a 32-bit
/// `MOVSXD` load of half a `Value` cell, producing exactly this "small-int
/// garbage used as a pointer" shape. See
/// fixed-suite-bugs/wildfly/wildfly-domain-hostcontroller-sigsegv-inline-cache-null-receiver-FIXED.md
/// for the full chain. Re-verified clean with
/// `CRATONVM_JIT_GUARDED_GETFIELD=1` against the exact IVF-KNN repro (no
/// SIGSEGV, no dmesg segfault entry — only the separate, still-OPEN,
/// already-tracked Lucene IndexWriter/STW-monitor-race hang this doc's own
/// "underlying interpreter hang" section describes) — re-enabled default-ON.
/// `CRATONVM_JIT_GETFIELD_HELPER=1` restores the helper-only path if a new
/// corruption is ever suspected here again.
pub fn guarded_inline_getfield_enabled() -> bool {
    // NOT OnceLock-cached (unlike the other flags in this file): this is a
    // JIT COMPILE-TIME gate, read once per getfield call SITE during
    // compilation, never on the runtime hot path — so re-reading the env
    // var every call has no measurable cost. Caching it would make the
    // off-switch racy against whichever test/thread first triggers ANY
    // getfield compilation in the process.
    cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_GETFIELD_HELPER").is_none()
}

/// Default-ON inline self-recursion stack check (perf/throughput-20260710).
///
/// A method with direct self-recursive call sites reserves one frame slot,
/// fills it once in the prologue from the leaf `jit_native_stack_floor`
/// helper, and each self-call site emits `CMP RSP, [rbp - floor_slot]; JA
/// <skip>` -- bypassing the `self_call_stack_guard` helper CALL (and its
/// safepoint spill / shadow push+reload bracketing) on the common path. The
/// helper call remains verbatim as the fallback and still owns the catchable
/// StackOverflowError raise; OSR trampolines initialise the slot to
/// `usize::MAX` so OSR-entered frames always take the helper.
/// `CRATONVM_JIT_INLINE_SELF_GUARD=0` restores the helper-per-call shape for
/// bisection.
pub fn inline_self_guard_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(
        || match cratonvm_types::flags::runtime_var("CRATONVM_JIT_INLINE_SELF_GUARD") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        },
    )
}

/// Let direct self-recursive entries inherit immutable per-thread cache slots
/// from their caller frame after proving the return address belongs to this
/// method's private executable buffer. External entries retain the normal TLS
/// helper initialization. Opt out with `CRATONVM_JIT_NO_SELF_CACHE_INHERIT=1`.
pub(super) fn self_cache_inherit_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_SELF_CACHE_INHERIT").is_none()
    })
}

/// Step 1 of `docs/feature-designs/precise-jit-maps-default.md` — opt-IN
/// **inline** frame-record. When on (and precise maps are on, and the OS TLS
/// probe in [`inline_rbp_tls_disp`] succeeds), the JIT stores RBP straight into
/// the precise-maps innermost-RBP mirror with one segment-relative `mov`
/// (`gs:` on Windows, `fs:` on Linux) instead of `call jit_frame_record`.
/// This removes the per-invocation CALL that is the residual ~1.68× call-heavy
/// regression after the thread-local cache (`82cf85e9`) already cut the helper
/// body cost.
///
/// **DEFAULT ON** (Step 2 flip, 2026-06-21; opt out with
/// `CRATONVM_NO_PRECISE_INLINE_FRAME_RECORD`). Validated on the inlined path:
/// bintrees10/14/16/18 == HotSpot, fib44 ~1.68× faster than the CALL path, and
/// the `CRATONVM_DBG_VERIFY_INLINE_FRAME_RECORD` self-check is clean (0
/// mismatches over billions of fib44 invocations). Off → the existing
/// `call jit_frame_record` is emitted (the pre-Step-1 default). Only meaningful
/// with precise maps on (otherwise there is no frame-record at all), so it is
/// anded with [`precise_jit_maps_enabled`]. On an unsupported target or a failed
/// TLS probe, [`inline_rbp_tls_disp`] returns 0 and the CALL path is used even
/// when this is on.
pub fn precise_inline_frame_record_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        precise_jit_maps_enabled()
            && cratonvm_types::flags::runtime_var_os("CRATONVM_NO_PRECISE_INLINE_FRAME_RECORD")
                .is_none()
    })
}

/// Debug self-check (`CRATONVM_DBG_VERIFY_INLINE_FRAME_RECORD`): when on AND
/// inline frame-record is active, the prologue emits the inline store **and** a
/// call to the verify helper (wired into the `frame_record` slot by
/// `build_helpers`) which reads the mirror back and asserts it equals RBP —
/// proving the inlined store lands exactly where the Rust GC side reads it.
/// Default off; pure validation aid, no behaviour change to the mirror value.
pub fn verify_inline_frame_record_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_VERIFY_INLINE_FRAME_RECORD").is_some()
    })
}

/// Step 1 — the segment-relative byte displacement of the OS TLS slot that
/// backs the precise-maps innermost-RBP mirror, or `0` when inline frame-record
/// is disabled or unavailable. This is the **single source of truth** shared by
/// JIT codegen and the VM-side mirror accessor in
/// `vm/src/jit/conservative_roots.rs`. Computed once, cached for the process.
///
/// Windows x86-64 stores the 64 static TLS slots in the TEB at offset `0x1480`
/// (`TlsSlots`), reachable as `gs:[0x1480 + slot*8]`. We `TlsAlloc` a slot,
/// write a unique 64-bit sentinel through the documented `TlsSetValue` API,
/// then read it back through the candidate `gs:[disp]` to **prove** the
/// displacement before trusting it (with a fallback scan of the static band in
/// case the base constant differs). If the probe fails (slot ≥ 64, unexpected
/// TEB layout, or non-Windows), it returns `0` → the CALL path is used. A wrong
/// assumption therefore degrades to the existing safe behaviour, never to a
/// silently mis-tracked mirror (the risk the design doc flags).
#[cfg(windows)]
pub fn inline_rbp_tls_disp() -> usize {
    use std::sync::OnceLock;
    static DISP: OnceLock<usize> = OnceLock::new();
    *DISP.get_or_init(|| {
        if !precise_inline_frame_record_enabled() {
            return 0;
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn TlsAlloc() -> u32;
            fn TlsSetValue(idx: u32, val: *mut core::ffi::c_void) -> i32;
        }
        const TLS_OUT_OF_INDEXES: u32 = 0xFFFF_FFFF;
        // TEB.TlsSlots[64] on x86-64. The probe below validates this, so an
        // incorrect constant disables inline rather than corrupting.
        const TEB_TLS_SLOTS_OFF: usize = 0x1480;
        // SAFETY: TlsAlloc/TlsSetValue are the documented Win32 TLS APIs;
        // read_gs_qword reads an 8-byte aligned TEB slot we just wrote.
        let disp = unsafe {
            'probe: {
                let slot = TlsAlloc();
                if slot == TLS_OUT_OF_INDEXES {
                    break 'probe 0;
                }
                // Non-canonical, slot-tagged sentinel (high bits set so it
                // cannot be mistaken for a real RBP or a small int in a
                // neighbour slot).
                let sentinel: usize = 0x5247_4250_4D52_0000 | (slot as usize & 0xFFFF);
                if TlsSetValue(slot, sentinel as *mut core::ffi::c_void) == 0 {
                    break 'probe 0;
                }
                let candidate = TEB_TLS_SLOTS_OFF + (slot as usize) * 8;
                if read_gs_qword(candidate) == sentinel {
                    // Reset the slot to 0 (matches the mirror's initial value);
                    // all other threads already see 0 at a fresh index.
                    TlsSetValue(slot, core::ptr::null_mut());
                    break 'probe candidate;
                }
                // Fallback: scan the static 64-slot band for the sentinel in
                // case the base constant differs on this Windows build.
                let mut d = TEB_TLS_SLOTS_OFF;
                let end = TEB_TLS_SLOTS_OFF + 64 * 8;
                while d < end {
                    if read_gs_qword(d) == sentinel {
                        TlsSetValue(slot, core::ptr::null_mut());
                        break 'probe d;
                    }
                    d += 8;
                }
                // Probe failed → leave inline disabled (CALL path).
                TlsSetValue(slot, core::ptr::null_mut());
                0
            }
        };
        // One-time visibility line, gated behind CRATONVM_DBG_INLINE_FR so the
        // (now default-on) path stays silent unless explicitly diagnosing.
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_INLINE_FR").is_some() {
            if disp != 0 {
                eprintln!(
                    "[INLINE-FR] inline frame-record ENABLED: storing RBP via mov gs:[{:#x}]",
                    disp
                );
            } else {
                eprintln!("[INLINE-FR] inline frame-record probe FAILED — using CALL path");
            }
        }
        disp
    })
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
thread_local! {
    /// Linux counterpart of the Windows OS-TLS slot. Generated code reaches
    /// this exact Rust TLS cell as `fs:[disp32]`; the startup probe below writes
    /// a sentinel through `Cell` and reads it back through `fs:` before the
    /// displacement is accepted.
    pub(super) static LINUX_INLINE_RBP_MIRROR: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

/// Linux x86-64 inline frame-record. ELF TLS lives at a stable signed offset
/// from the thread's FS base. Recover that offset once, encode it as the raw
/// disp32 bits expected by the x86 instruction, and sentinel-probe the exact
/// `fs:[offset]` address before enabling generated stores. Any layout or range
/// surprise returns 0 and retains the existing helper-call path.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn inline_rbp_tls_disp() -> usize {
    use std::sync::OnceLock;
    static DISP: OnceLock<usize> = OnceLock::new();
    *DISP.get_or_init(|| {
        if !precise_inline_frame_record_enabled() {
            return 0;
        }
        let disp = LINUX_INLINE_RBP_MIRROR.with(|cell| {
            // On the System V x86-64 ABI, fs:[0] is the thread-control-block
            // self pointer. Do not trust that convention blindly: the raw
            // sentinel read below proves the derived address before use.
            //
            // SAFETY: `fs:[0]` is the one segment-relative address the ABI
            // guarantees mapped on every thread with a TCB. If the convention
            // did not hold this is a wrong number, never a dereference — only
            // the sentinel probe below acts on it.
            let fs_base = unsafe { read_fs_qword(0) };
            let cell_addr = cell as *const std::cell::Cell<usize> as usize;
            let delta = (cell_addr as i128) - (fs_base as i128);
            let Ok(delta32) = i32::try_from(delta) else {
                return 0;
            };
            if delta32 == 0 {
                return 0;
            }
            let old = cell.replace(0x5242_504C_494E_5558);
            // SAFETY: reads back the address the line above just wrote a
            // sentinel to. `cell` is a live thread-local owned by this thread
            // and `delta32` is `cell_addr - fs_base` (checked to fit an `i32`),
            // so `fs:[delta32]` is the same 8 aligned bytes `cell` occupies iff
            // the fs-base convention holds — which is precisely what the
            // sentinel comparison below decides. Runs once at startup, behind
            // `precise_inline_frame_record_enabled()`, never on a hot path.
            let probed = unsafe { read_fs_qword(delta32 as isize) };
            cell.set(old);
            if probed == 0x5242_504C_494E_5558 {
                (delta32 as u32) as usize
            } else {
                0
            }
        });
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_INLINE_FR").is_some() {
            if disp != 0 {
                eprintln!(
                    "[INLINE-FR] inline frame-record ENABLED: storing RBP via mov fs:[{:#x}]",
                    disp as u32
                );
            } else {
                eprintln!("[INLINE-FR] Linux TLS probe FAILED — using CALL path");
            }
        }
        disp
    })
}

/// Unsupported targets retain the helper-call path.
#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"))))]
pub fn inline_rbp_tls_disp() -> usize {
    0
}

/// Segment override used by generated inline frame-record stores.
///
/// This is shared with the OSR trampoline emitter in `lib.rs`; keeping the
/// platform byte in one place prevents that independently emitted prologue
/// from silently retaining the Windows `gs:` prefix on Linux.
pub(crate) const fn inline_rbp_tls_segment_prefix() -> u8 {
    #[cfg(windows)]
    {
        0x65
    }
    #[cfg(not(windows))]
    {
        0x64
    }
}

/// VM-side access to the Linux TLS cell used by generated `fs:` stores.
/// `None` means the startup probe did not enable the inline path.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn inline_rbp_tls_mirror_read() -> Option<usize> {
    if inline_rbp_tls_disp() == 0 {
        None
    } else {
        Some(LINUX_INLINE_RBP_MIRROR.with(std::cell::Cell::get))
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn inline_rbp_tls_mirror_write(value: usize) -> bool {
    if inline_rbp_tls_disp() == 0 {
        false
    } else {
        LINUX_INLINE_RBP_MIRROR.with(|cell| cell.set(value));
        true
    }
}

// ---------------------------------------------------------------------------
// The compile-id mirror — the identity half of the innermost-frame record
// ---------------------------------------------------------------------------
//
// A second slot, written by the same instructions that write the RBP mirror, so
// the GC can name the method owning `exact_rbp` instead of decoding the call
// that created the frame (see the compile-id table in `lib.rs` for why the
// decode cannot work for an indirect JIT->JIT call). Everything here mirrors
// `inline_rbp_tls_disp` deliberately: same probe, same fail-to-zero rule, same
// segment prefix. A zero displacement means codegen publishes no identity and
// the scan keeps its old behaviour.
#[cfg(windows)]
pub fn inline_cm_tls_disp() -> usize {
    use std::sync::OnceLock;
    static DISP: OnceLock<usize> = OnceLock::new();
    *DISP.get_or_init(|| {
        // Only meaningful alongside the RBP mirror: the pair is what makes
        // `(rbp, id)` describe one frame.
        if inline_rbp_tls_disp() == 0 {
            return 0;
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn TlsAlloc() -> u32;
            fn TlsSetValue(idx: u32, val: *mut core::ffi::c_void) -> i32;
        }
        const TLS_OUT_OF_INDEXES: u32 = 0xFFFF_FFFF;
        const TEB_TLS_SLOTS_OFF: usize = 0x1480;
        // SAFETY: as `inline_rbp_tls_disp` — the documented Win32 TLS APIs plus
        // a sentinel round-trip that proves the displacement before it is used.
        let disp = unsafe {
            'probe: {
                let slot = TlsAlloc();
                if slot == TLS_OUT_OF_INDEXES {
                    break 'probe 0;
                }
                // A different sentinel from the RBP probe's, so a mix-up
                // between the two slots cannot round-trip successfully.
                let sentinel: usize = 0x434D_4944_5F50_0000 | (slot as usize & 0xFFFF);
                if TlsSetValue(slot, sentinel as *mut core::ffi::c_void) == 0 {
                    break 'probe 0;
                }
                let candidate = TEB_TLS_SLOTS_OFF + (slot as usize) * 8;
                if read_gs_qword(candidate) == sentinel {
                    TlsSetValue(slot, core::ptr::null_mut());
                    break 'probe candidate;
                }
                let mut d = TEB_TLS_SLOTS_OFF;
                let end = TEB_TLS_SLOTS_OFF + 64 * 8;
                while d < end {
                    if read_gs_qword(d) == sentinel {
                        TlsSetValue(slot, core::ptr::null_mut());
                        break 'probe d;
                    }
                    d += 8;
                }
                TlsSetValue(slot, core::ptr::null_mut());
                0
            }
        };
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_INLINE_FR").is_some() {
            if disp != 0 {
                eprintln!("[INLINE-FR] compile-id mirror ENABLED at gs:[{disp:#x}]");
            } else {
                eprintln!("[INLINE-FR] compile-id mirror probe FAILED — identity not published");
            }
        }
        disp
    })
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
thread_local! {
    /// Linux counterpart of the Windows compile-id slot. Generated code writes
    /// the low 32 bits of this cell as `fs:[disp32]`.
    pub(super) static LINUX_INLINE_CM_MIRROR: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn inline_cm_tls_disp() -> usize {
    use std::sync::OnceLock;
    static DISP: OnceLock<usize> = OnceLock::new();
    *DISP.get_or_init(|| {
        if inline_rbp_tls_disp() == 0 {
            return 0;
        }
        let disp = LINUX_INLINE_CM_MIRROR.with(|cell| {
            // SAFETY / rationale: identical to `inline_rbp_tls_disp`'s Linux
            // arm — derive the cell's offset from the FS base and prove it with
            // a sentinel round-trip before any generated store uses it.
            let fs_base = unsafe { read_fs_qword(0) };
            let cell_addr = cell as *const std::cell::Cell<usize> as usize;
            let delta = (cell_addr as i128) - (fs_base as i128);
            let Ok(delta32) = i32::try_from(delta) else {
                return 0;
            };
            if delta32 == 0 {
                return 0;
            }
            let old = cell.replace(0x434D_4944_5F4C_4E58);
            // SAFETY: reads back the 8 bytes the line above wrote through the
            // same live thread-local; the comparison is what decides whether
            // the derived displacement is trusted at all.
            let probed = unsafe { read_fs_qword(delta32 as isize) };
            cell.set(old);
            if probed == 0x434D_4944_5F4C_4E58 {
                (delta32 as u32) as usize
            } else {
                0
            }
        });
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_INLINE_FR").is_some() {
            if disp != 0 {
                eprintln!("[INLINE-FR] compile-id mirror ENABLED at fs:[{:#x}]", disp as u32);
            } else {
                eprintln!("[INLINE-FR] compile-id mirror probe FAILED — identity not published");
            }
        }
        disp
    })
}

/// Unsupported targets publish no identity.
#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"))))]
pub fn inline_cm_tls_disp() -> usize {
    0
}

/// VM-side access to the Linux TLS cell used by generated `fs:` identity
/// stores. `None` means the startup probe did not enable the mirror. The
/// Windows side reads `gs:[disp]` directly with its own helper, exactly as it
/// does for the RBP mirror.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn inline_cm_tls_mirror_read() -> Option<u32> {
    if inline_cm_tls_disp() == 0 {
        None
    } else {
        Some((LINUX_INLINE_CM_MIRROR.with(std::cell::Cell::get) & 0xFFFF_FFFF) as u32)
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn inline_cm_tls_mirror_write(value: u32) -> bool {
    if inline_cm_tls_disp() == 0 {
        false
    } else {
        LINUX_INLINE_CM_MIRROR.with(|cell| cell.set(value as usize));
        true
    }
}

/// Read the 8-byte value at `gs:[disp]` (Windows TEB-relative). Used only by
/// the [`inline_rbp_tls_disp`] startup probe.
///
/// # Safety
///
/// `disp` must name 8 readable, 8-byte-aligned bytes relative to this thread's
/// TEB. The caller earns that by deriving `disp` from a live thread-local's
/// address and proving it with a sentinel round-trip before trusting it.
//
// SAFETY: one aligned 8-byte load and nothing else; `nostack`,
// `preserves_flags` and `readonly` all hold for a bare `mov`. An unmapped
// `gs:[disp]` faults — it cannot corrupt.
#[cfg(windows)]
#[inline]
pub(super) unsafe fn read_gs_qword(disp: usize) -> usize {
    let val: usize;
    core::arch::asm!(
        "mov {out}, qword ptr gs:[{addr}]",
        out = out(reg) val,
        addr = in(reg) disp,
        options(nostack, preserves_flags, readonly),
    );
    val
}

/// Read the 8-byte value at `fs:[offset]` on Linux x86-64. Used only by the
/// sentinel probe for [`inline_rbp_tls_disp`].
///
/// # Safety
///
/// `offset` must name 8 readable, 8-byte-aligned bytes relative to this
/// thread's TCB. `offset == 0` is always sound (the TCB self-pointer); any
/// other value must be proven by the sentinel round-trip in
/// [`inline_rbp_tls_disp`] before its result is used.
//
// SAFETY: one aligned 8-byte load and nothing else; `nostack`,
// `preserves_flags` and `readonly` all hold for a bare `mov`. An unmapped
// `fs:[offset]` faults — it cannot corrupt.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[inline]
pub(super) unsafe fn read_fs_qword(offset: isize) -> usize {
    let val: usize;
    core::arch::asm!(
        "mov {out}, qword ptr fs:[{addr}]",
        out = out(reg) val,
        addr = in(reg) offset,
        options(nostack, preserves_flags, readonly),
    );
    val
}

/// Whether the JIT **shadow-stack** precise-roots codegen is enabled
/// (`CRATONVM_SHADOW_STACK`). When on, each GC-capable safepoint pushes every
/// live oop (operand-stack entries AND oop locals) onto the thread's shadow
/// stack and reloads them after the call, so a *moving* collector can rewrite
/// every JIT-held reference precisely (see `gc/src/shadow_stack.rs`). The
/// standalone `CRATONVM_SHADOW_STACK` knob remains opt-in; the default moving
/// young generation turns this mechanism on through [`moving_young_enabled`].
///
/// This doc used to carry an "EXPERIMENTAL — must stay default-off, the moving
/// Cheney path under-counts bt18 → 67674804" warning. That described the
/// incomplete 2026-06-22 standalone experiment. The default moving-young path
/// now publishes complete oop homes, reloads them after safepoints, and falls
/// back to a non-moving cycle for any cycle whose coverage is not proven; it
/// returns the HotSpot checksum at every bintrees depth and heap size measured.
/// See `docs/feature-designs/default-moving-young-gen.md` and
/// `default-moving-young-enabled-20260730.md`.
pub fn shadow_stack_maps_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    // 2026-07-31 — the `moving_young_enabled() &&` term is REMOVED. It used to
    // read "`CRATONVM_MOVING_YOUNG` implies the shadow-stack codegen", which is
    // true of what the moving collector NEEDS but was wrong as a definition of
    // when the codegen is REQUIRED: the shadow stack is how a JIT frame's live
    // references become visible to the collector at all, and that is not a
    // property of which young collector runs. Keyed on moving-young, the
    // documented opt-out `CRATONVM_NO_MOVING_YOUNG=1` silently withdrew root
    // publication, and that lane faulted on a zeroed heap slot within seconds
    // of real work — Hibernate `ZonedDateTimeTest` / `OffsetDateTimeTest` in
    // 1-3 s on a pristine dev build, and the Windows `DateSymbolsProbe` repro.
    // Restoring publication with `CRATONVM_SHADOW_STACK=1` and changing nothing
    // else made the same runs clean, which is the single-variable proof.
    // See `jit-no-moving-young-opt-out-unpublishes-roots-CLOSED-20260803.md`.
    //
    // The DEFAULT path is unchanged: moving-young is on by default, so this
    // already evaluated true. `CRATONVM_JIT_MY_SHADOW_EMISSION=0` remains the
    // joint opt-out and now switches the emission off outright rather than
    // switching off an implication.
    //
    // NOT scoped to `moving_young_relocates_compiled_frames`, deliberately.
    // Scoping it is *arguable* on the same invariant the admission gates use —
    // the shadow stack exists to REWRITE references and relocation is vetoed —
    // but it was measured and buys nothing: interleaved A/B on one binary,
    // `BinTreesClassic 18` at 512m over 6 reps per lane gave medians 5640 ms
    // (off) vs 6045 ms (on) with ranges 4913-7171 and 4187-7525, and
    // `ASTParserLoadingTest` 189/199 s vs 183/406 s. No signal either way.
    // Since it moves the emission side of an emission/root-scan agreement that
    // must hold exactly, it is not worth taking risk for an unmeasurable win.
    // `CRATONVM_SHADOW_STACK` is read here, in `gc` and in `vm`, which is why
    // it lives in the shared `cratonvm_types::flags()` config rather than a
    // crate-private `getenv` cache: the emission side (this file) and the
    // root-scan side (`vm::jit::conservative_roots`, `gc::gen_heap`) MUST agree,
    // or the collector walks a shadow stack the codegen never pushed to.
    // `parse::present` == the former `var_os(..).is_some()`, so this is
    // behaviour-preserving.
    // A thread pinning the moving-young policy (see `set_moving_young_override`)
    // must not read -- or, worse, POPULATE -- this process-wide cache: whichever
    // test touched it first would otherwise freeze the shadow-stack decision for
    // every other test in the binary. Recompute instead; the override is never
    // set in production, so the cached fast path is unchanged there.
    if MOVING_YOUNG_OVERRIDE.with(|c| c.get()).is_some() {
        return cratonvm_types::flags().jit.shadow_stack
            || shadow_emission_moving_implication_enabled();
    }
    *G.get_or_init(|| {
        cratonvm_types::flags().jit.shadow_stack || shadow_emission_moving_implication_enabled()
    })
}

/// Whether the **default moving / compacting young generation**
/// (`CRATONVM_MOVING_YOUNG`) is enabled. Cached on first read.
///
/// When on, the JIT publishes a *complete* rewritable precise root map at every
/// GC-capable safepoint — not just the register-invisible operand oops the
/// `CRATONVM_SHADOW_STACK`-only path publishes, but EVERY live oop (operand-stack
/// entries in registers AND frame slots, and every oop local in its register or
/// canonical frame slot) — so a moving (Cheney) young collection can relocate
/// each JIT-held reference and rewrite its home in place. This completeness is
/// what lets `gen_heap::collect_garbage_inner` run the moving cycle while JIT
/// frames are live (the conservative frame scan is then suppressed — a
/// fully-precise frame needs no conservative backstop). Enabled by default;
/// `CRATONVM_NO_MOVING_YOUNG=1` retains the non-moving compatibility path. The
/// bt18 = 68332206 invariant it was gated behind is now checked with the moving
/// cycle actually running (25 of them at `-Xmx512m`), not merely with the flag
/// requested.
///
/// See `docs/feature-designs/default-moving-young-gen.md`.
///
/// **Single source of truth.** This used to `getenv` `CRATONVM_MOVING_YOUNG`
/// into a crate-private `OnceLock`, which meant the SAME gate was parsed
/// independently in three crates (`jit::x64`, `gc::gc_quiescence`,
/// `vm::jit::conservative_roots`). Codegen and the collector could therefore
/// disagree about whether the feature was on — and the two halves of this
/// feature are only sound *together*: the JIT must publish the complete
/// rewritable root map and the collector must run the moving cycle. It now
/// reads the centralized [`cratonvm_types::flags`] field. The compiled default
/// is on, in `DEFAULT_MOVING_YOUNG`. Do NOT re-introduce a local `getenv` here.
#[inline]
thread_local! {
    /// Per-thread override for [`moving_young_enabled`]; `None` = use the
    /// process flag. Thread-local so parallel tests cannot race each other.
    pub(super) static MOVING_YOUNG_OVERRIDE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Pin [`moving_young_enabled`] for the CURRENT THREAD only.
///
/// The optimizing IR tier is gated on `!moving_young_enabled()` (IR publishes
/// no exact-RBP/safepoint map, so it must stay off while the young generation
/// can relocate). That makes every IR-routing test depend on a *deployment*
/// flag rather than on what it actually exercises — and when
/// `DEFAULT_MOVING_YOUNG` flipped to `true` those tests began asserting against
/// a pipeline that can no longer run, failing with a bare `left: 0, right: 1`.
/// Tests that mean "exercise the IR pipeline" call this to say so explicitly.
///
/// This does NOT change what production does: nothing calls it outside tests,
/// and the process flag remains the default on every thread.
pub fn set_moving_young_override(value: Option<bool>) {
    MOVING_YOUNG_OVERRIDE.with(|c| c.set(value));
}

pub fn moving_young_enabled() -> bool {
    if let Some(forced) = MOVING_YOUNG_OVERRIDE.with(|c| c.get()) {
        return forced;
    }
    cratonvm_types::flags().gc.moving_young
}

/// `CRATONVM_JIT_MY_SCRATCH_FLUSH` — opt-out for the scratch-register flush
/// `emit_pre_safepoint_spill_impl` performs at every GC-capable safepoint.
/// Default ON. **`0` is known-unsafe**, kept only as an A/B control.
///
/// It arrived as a bisect lever for a residual moving-young throughput cost,
/// and as a lever it has answered: `BinTreesClassic 18` @512m, five
/// interleaved reps, `0` moves nothing (median 4117 ms against a 4281 ms
/// default, ranges overlapping in both directions). There is no cost here
/// worth carrying a risk for.
///
/// What it does carry is ROOT VISIBILITY — it spills operand-stack values
/// living in caller-saved scratch registers into frame slots, and rewrites
/// `Compiler::stack` so the PRECISE map names them. The call site gates it
/// additionally on `moving_young_enabled()`. Making it unconditional was tried
/// as a fix for the crashing non-moving lane and reverted after that lane
/// SIGILL'd; **that attribution was wrong** — the SIGILL was the inline-PIC
/// cascade's `rel8` truncation (`7f1b1f263`), which any code-size growth
/// reproduced, and the experiment re-ran clean 3/3 on 2026-08-03 once it was
/// fixed — interleaved with a pre-fix positive control that SIGILL'd 2/2 in
/// the same batch. The term stays because the full-GPR blind spill covers the
/// non-moving lane conservatively, not because this crashes. See the call site
/// and `jit-no-moving-young-opt-out-unpublishes-roots-CLOSED-20260803.md`.
pub fn scratch_flush_at_safepoint_enabled() -> bool {
    match cratonvm_types::flags::runtime_var("CRATONVM_JIT_MY_SCRATCH_FLUSH") {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    }
}

/// `CRATONVM_JIT_MY_SHADOW_EMISSION` — bisect lever for the moving-young
/// implication in [`shadow_stack_maps_enabled`] / the matching
/// `vm::jit::conservative_roots::shadow_stack_enabled`. Default ON (current
/// behaviour); `0` drops the implication so the shadow stack is driven by
/// `CRATONVM_SHADOW_STACK` alone.
///
/// **Both sides must read this identically** — the emission side and the
/// root-scan side are halves of one agreement, and the collector otherwise
/// walks a shadow stack the codegen never pushed to. The vm side spells the
/// same expression against the same variable, and each side has a test pinning
/// the formula.
///
/// Why a third lever: a 5-lane bisect on `OffsetDateTimeTest` (quiet host,
/// 1500 s cap) eliminated the other two candidates — default, no-scratch-flush,
/// no-selfcall-proof and *both* all TIMEOUT, while `CRATONVM_NO_MOVING_YOUNG=1`
/// PASSes at 777 s. Shadow emission is what is left. An earlier measurement
/// called this scoping neutral, but it was taken on `ASTParserLoadingTest` and
/// `BinTreesClassic`, neither of which is oop-dense at its safepoints; the
/// temporal classes hold many live references per call, so
/// `collect_live_oop_homes` publishes a long home list at every one.
pub fn shadow_emission_moving_implication_enabled() -> bool {
    match cratonvm_types::flags::runtime_var("CRATONVM_JIT_MY_SHADOW_EMISSION") {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    }
}

/// `CRATONVM_JIT_MY_SELFCALL_PROOF` — bisect lever for the *stronger* proof
/// moving-young demands before a self-recursive call may skip the SB-CRASH-04
/// full register spill. Default ON (current behaviour); `0` makes the decision
/// behave as it does on the non-moving path.
///
/// **Both** users of this — `can_elide_self_call_register_spill` and its paired
/// emitter `emit_safepoint_metadata_only` — must read this same function. The
/// emitter fails the compile closed if the two disagree, so they are wired to
/// one predicate on purpose. Second candidate for the residual described on
/// [`scratch_flush_at_safepoint_enabled`]; same measurement caveat.
pub fn self_call_moving_proof_enabled() -> bool {
    if !moving_young_enabled() {
        return false;
    }
    match cratonvm_types::flags::runtime_var("CRATONVM_JIT_MY_SELFCALL_PROOF") {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    }
}

/// Can a **relocating** young collection ever observe a live *map-less*
/// compiled frame — an IR body, or one entered by a direct JIT→JIT call?
///
/// This is the question the JIT's relocation-safety admission gates actually
/// need — not "is the moving young flag on?". The two differ today, and the
/// difference is expensive.
///
/// `moving_young_enabled()` is on by default, but the collector proves coverage
/// **per frame**: `conservative_roots` matches each live frame's recorded
/// safepoint id against the method's `oop_maps` and requires
/// `moving_young_coverage_complete`, treating *no matching map* as a refusal.
/// `memory::roots::collect_roots` runs that proof on every collection and
/// `gen_heap` diverts on its verdict. The IR backend emits no `oop_maps` at
/// all, so an IR frame always fails it; a direct-call frame pushes no entry
/// guard and is not reachable from the chain the proof walks. Neither can be
/// live during a relocating cycle.
///
/// A gate that exists solely to keep such a frame away from a relocating
/// collector is therefore guarding an unreachable state whenever this returns
/// `false`. Gates that protect something else — the *emission* of precise maps,
/// which must stay in lockstep with the collector's expectations — keep reading
/// [`moving_young_enabled`] directly and are unaffected.
///
/// Both halves read the same
/// [`cratonvm_types::flags::JIT_PUBLISHES_RELOCATION_CONTRACT`], so the proof's
/// obligations and these gates cannot drift apart: see that constant for what
/// flipping it requires.
#[inline]
pub fn moving_young_relocates_compiled_frames() -> bool {
    moving_young_enabled() && cratonvm_types::flags::JIT_PUBLISHES_RELOCATION_CONTRACT
}

pub(super) fn shadow2_diag_enabled(method_label: &str) -> bool {
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SHADOW2").is_none() {
        return false;
    }
    match cratonvm_types::flags::runtime_var("CRATONVM_DBG_SHADOW2_FILTER") {
        Ok(filter) if !filter.is_empty() => method_label.contains(&filter),
        _ => true,
    }
}

/// DBG (CRATONVM_DBG_SHADOW_RELOAD): emit a bad-path-only logging call in
/// `emit_shadow_reload` that reports (thread, orig-savebase, reloaded value,
/// actual-read-address) whenever a reload loads a non-pointer (`< 0x10000`).
/// Default-off; the common (pointer) path emits only a `cmp`+`jae`.
pub(super) fn shadow_reload_dbg() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SHADOW_RELOAD").is_some())
}

/// DBG helper called from JIT-emitted reload code (see `shadow_reload_dbg`).
/// Logs the values that explain a reload that produced a non-pointer home value.
pub(super) extern "C" fn jit_dbg_shadow_reload_log(
    thread: usize,
    orig_savebase: usize,
    slotval: usize,
    read_addr: usize,
) {
    eprintln!(
        "[RELOAD] thread={:#x} orig_savebase={:#x} slotval={:#x} read_addr={:#x}",
        thread, orig_savebase, slotval, read_addr
    );
}

/// Bisect toggle (`CRATONVM_SHADOW_NOPUSH`) — when set, the shadow push/reload
/// codegen is suppressed while the prologue thread-fetch + the GC gate flip stay
/// on. Used to localize a fault to the push/reload sequences vs the rest.
pub(super) fn shadow_nopush() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_SHADOW_NOPUSH").is_some())
}

/// Bisect toggle (`CRATONVM_SHADOW_NORELOAD`) — suppress only the post-call
/// reload (keep the push). Push-only writes to the (separate) shadow buffer, so
/// if push-only is correct it cannot corrupt program state; a crash then
/// localizes to the reload (which writes back into home regs/slots).
pub(super) fn shadow_noreload() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_SHADOW_NORELOAD").is_some())
}

/// spring-bug-10 (`CRATONVM_SHADOW_PIN`) — pinned shadow marking. The shadow
/// oops are published as PINNED roots (memory/roots.rs), so a GC never moves
/// them; the operand-stack homes are callee-saved registers, preserved across
/// the call, so the post-call reload's value-restore is REDUNDANT. The reload
/// must therefore only POP the shadow `top` (balance the LIFO; skipping it
/// entirely overflows the buffer) and skip the per-home value write — which is
/// exactly the buggy path that read a stale/null slot and crashed. Correct
/// because the live value is already in its preserved register/frame slot.
pub(super) fn shadow_pin_codegen() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_SHADOW_PIN").is_some())
}

/// spring-bug-10 diagnostic (`CRATONVM_SHADOW_SENTINEL`) — pre-stamp the savebase
/// slot with a non-canonical sentinel at each push so a faulting reload reveals
/// whether the slot was skipped, externally overwritten, or fine.
pub(super) fn shadow_sentinel() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_SHADOW_SENTINEL").is_some())
}

/// spring-bug-10 watchpoint (`CRATONVM_SHADOW_WATCH`) — emit a prologue call to
/// the registered arm-helper that sets a HW data breakpoint on this frame's
/// savebase slot, so the VEH can report the PC that writes the corrupt -2.
pub(super) fn shadow_watch() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_SHADOW_WATCH").is_some())
}

/// Process-global pointer to the VM-side `jit_arm_savebase_watch(addr)` helper,
/// registered at VM init. Baked as an absolute call target by the prologue when
/// `CRATONVM_SHADOW_WATCH` is set. Avoids a `JitRuntimeHelpers` ABI change.
pub static ARM_SAVEBASE_WATCH_FN: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Register the savebase watchpoint arm-helper (called once from the VM).
pub fn set_arm_savebase_watch_fn(addr: usize) {
    ARM_SAVEBASE_WATCH_FN.store(addr, std::sync::atomic::Ordering::Relaxed);
}

/// Process-global pointer to the VM-side `jit_disarm_savebase_watch()` helper.
pub static DISARM_SAVEBASE_WATCH_FN: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Register the savebase watchpoint disarm-helper (called once from the VM).
pub fn set_disarm_savebase_watch_fn(addr: usize) {
    DISARM_SAVEBASE_WATCH_FN.store(addr, std::sync::atomic::Ordering::Relaxed);
}

/// Process-global pointer to the VM-side `jit_resolve_static_base` resolver
/// (`extern "C" fn(vm_ptr, class_id, field_index) -> i64`), registered at VM
/// init. Called by the backend **while compiling**, never from generated code,
/// so — like the savebase pair above — it avoids a `JitRuntimeHelpers` ABI
/// change entirely.
pub static STATIC_BASE_RESOLVER_FN: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// The `SharedVm` pointer passed back to [`STATIC_BASE_RESOLVER_FN`]. Latched
/// to the FIRST VM that registers.
pub static STATIC_BASE_RESOLVER_CTX: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Set once a SECOND VM registers a different context, and never cleared.
///
/// `ClassId`s are per-VM, so resolving VM A's `(class_id, field_index)` against
/// VM B's statics would bake the address of an unrelated class's slot into VM
/// A's code — the same cross-VM aliasing that made the process-global
/// `system_class_id` atomic and the unqualified `class_init_memo` wrong (see
/// `audits/vm-jit-cache-keying.md`). There is no correct answer to give once two
/// VMs share the process, so the mechanism turns itself off for BOTH and every
/// static read goes back to the helper: slower, never wrong.
static STATIC_BASE_RESOLVER_POISONED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Register the compile-time static-slot resolver (called once per VM).
pub fn set_static_base_resolver(addr: usize, vm_ctx: usize) {
    use std::sync::atomic::Ordering;
    if addr == 0 || vm_ctx == 0 {
        return;
    }
    // Store the function BEFORE claiming the context: a reader that observes a
    // non-zero context must never then read a zero function pointer.
    STATIC_BASE_RESOLVER_FN.store(addr, Ordering::Release);
    if let Err(prev) =
        STATIC_BASE_RESOLVER_CTX.compare_exchange(0, vm_ctx, Ordering::AcqRel, Ordering::Acquire)
    {
        if prev != vm_ctx {
            STATIC_BASE_RESOLVER_POISONED.store(true, Ordering::Release);
        }
    }
}

/// Ask the VM where a static field's storage base pointer lives.
///
/// `None` = "not inlineable, keep the helper": no VM registered, two VMs
/// registered, or the VM itself declined (class not initialized, the class is
/// `java/lang/System`, nothing published, index switched off).
pub fn resolve_static_base(class_id_raw: u32, field_index: usize) -> Option<usize> {
    use std::sync::atomic::Ordering;
    if STATIC_BASE_RESOLVER_POISONED.load(Ordering::Acquire) {
        return None;
    }
    let ctx = STATIC_BASE_RESOLVER_CTX.load(Ordering::Acquire);
    if ctx == 0 {
        return None;
    }
    let raw = STATIC_BASE_RESOLVER_FN.load(Ordering::Acquire);
    if raw == 0 {
        return None;
    }
    // SAFETY: the only writer of these two words is `set_static_base_resolver`,
    // which the VM calls with `jit_resolve_static_base` and its own `SharedVm`
    // pointer; the function is `extern "C" fn(i64, i64, i64) -> i64` and the
    // `SharedVm` outlives every compilation.
    let f: unsafe extern "C" fn(i64, i64, i64) -> i64 = unsafe { std::mem::transmute(raw) };
    // Cast: `usize`/`u32` inputs to the C ABI's i64 parameters.
    //
    // SAFETY: `f` and `ctx` were published together by
    // `set_static_base_resolver` and read back under `Acquire`, so the pointer
    // matches the signature transmuted above and `ctx` is the `SharedVm` that
    // resolver expects. Both outlive every compilation.
    let addr = unsafe { f(ctx as i64, class_id_raw as i64, field_index as i64) };
    if addr == 0 {
        None
    } else {
        // Cast: back to an address; `0` is the sentinel, everything else is a
        // real (positive, user-space) pointer.
        Some(addr as u64 as usize)
    }
}

/// Default-ON inline (helper-free) compiled `getstatic`.
///
/// Every `getstatic` in compiled code used to `CALL jit_getstatic` — ~35 ns
/// against HotSpot's ~1, where HotSpot emits a plain load
/// (`jit-getstatic-costs-a-helper-call-FIXED-20260803.md`).
/// With the declaring class already initialized at compile time, the helper has
/// nothing left to decide, so the backend bakes the address of the class's
/// statics base-pointer cell and emits two dependent loads instead of a call.
///
/// `CRATONVM_JIT_GETSTATIC_HELPER=1` (or `CRATONVM_JIT=getstatic-helper`)
/// restores the helper-only path, mirroring `CRATONVM_JIT_GETFIELD_HELPER`.
pub fn inline_getstatic_enabled() -> bool {
    // NOT OnceLock-cached, for the same reason as
    // `guarded_inline_getfield_enabled`: this is a compile-time gate read once
    // per call SITE during compilation, never on the runtime hot path, and
    // caching it would make the off-switch racy against whichever thread
    // triggers the first compilation in the process.
    cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_GETSTATIC_HELPER").is_none()
}

/// spring-bug-10 diagnostic (`CRATONVM_SHADOW_RAW_RELOAD`) — bypass the reload's
/// savebase bounds-guard (movable path) so a corrupt savebase faults on deref
/// (surfacing the bad value in the crash dump) instead of healing to pop-only.
pub(super) fn shadow_reload_raw() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_SHADOW_RAW_RELOAD").is_some())
}

/// Bisect toggle (`CRATONVM_JIT_SP_INLINE_IC=0`) — keep the single-pass
/// backend's inline MIC/PIC cascade off even when
/// `direct_jit_callee_calls_enabled()` allows raw JIT-to-JIT calls, so the raw
/// *virtual* edge can be isolated from the raw *static/special* one. No effect
/// when that master gate is already closed.
pub(super) fn sp_inline_ic_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_SP_INLINE_IC").as_deref(),
            Ok("0")
        )
    })
}

/// Bisect toggle (`CRATONVM_JIT_SP_TAILCALL=0`) — demote the single-pass
/// sibling tail-call (`epilogue-without-ret` + `JMP <callee entry>`) to an
/// ordinary CALL. The tail edge only exists when the raw JIT-to-JIT gate is
/// open, and it is the one raw shape that tears the caller's frame down before
/// the callee runs.
pub(super) fn sp_tailcall_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_SP_TAILCALL").as_deref(),
            Ok("0")
        )
    })
}

/// Whether the single-pass backend lowers a direct SELF tail-call by loading
/// the arguments into the parameter locals and `JMP`ping back to
/// `body_entry_offset` instead of emitting a `CALL`.
///
/// **Default OFF** (`CRATONVM_JIT_SELF_TAILCALL=1` opts back in). The `JMP`
/// form reuses one native frame for every activation, which HotSpot and this
/// VM's own interpreter do not, and the divergence is not cosmetic:
/// `StackOverflowError` becomes unreachable, and every activation is invisible
/// to `StackWalker`, `Throwable.getStackTrace()`, `Reflection.getCallerClass`
/// and anything built on them.
///
/// It was defaulted OFF once it was priced, not on principle. Measured, one
/// binary, interleaved: for a method the C1→C2 supersede claims the lowering is
/// worth **nothing** — both arms 5.4 ns/level, because the optimizing body has
/// already replaced the C1 one before the timed loop — and for a method the
/// supersede refuses (`c2_upgrade_would_engage` rejects any method that
/// allocates) it is worth **5.5x**. That second population is exactly the one
/// that keeps the C1 body for the life of the process, so it is also exactly
/// the one whose `StackOverflowError` can never fire. There is no
/// configuration that buys the speed without the permanent loss.
///
/// Unlike [`sp_tailcall_enabled`], which governs the SIBLING tail-call (a `JMP`
/// into ANOTHER method's entry), this one governs a method jumping back into
/// itself. See
/// `fixed-suite-bugs/jit/jit-eliminates-self-tail-call-frames-FIXED-20260820.md`.
pub(super) fn self_tailcall_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_SELF_TAILCALL").is_some()
    })
}

/// Diagnostic (`CRATONVM_SHADOW_OVERFLOW_DIAG`) — on a shadow-stack overflow
/// bail, also record the compiling method's label so the leak can be named.
/// The label is a leaked NUL-terminated copy, so it is only produced when this
/// is set; the overflow *counter* is always maintained.
pub(super) fn shadow_overflow_diag() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_SHADOW_OVERFLOW_DIAG").is_some()
    })
}

/// spring-bug-10 bisect toggle (`CRATONVM_SHADOW_NO_SAVEBASE`) — disable the
/// per-push saved-base frame slot and fall back to pure POP-ONLY reload (top -=
/// n*8 off the live `top`). Lets us measure whether the savebase mechanism
/// itself is the fault source (its frame slot was observed reading 0xFFFF…FFFE)
/// vs. a genuine top-drift that only savebase can correct.
pub(super) fn shadow_no_savebase() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_SHADOW_NO_SAVEBASE").is_some()
    })
}

/// Cooperative JIT safepoint polling — whether the backend emits an inline
/// poll of `helpers.safepoint_flag_addr` (the GC barrier's
/// `stw_requested` flag byte) at method entry and loop back-edges. Polling is
/// enabled by default; `CRATONVM_JIT_SAFEPOINT_POLLS=0` is the diagnostic
/// opt-out.
pub(super) fn jit_safepoint_polls_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_SAFEPOINT_POLLS")
            .and_then(|v| v.into_string().ok())
            .is_none_or(|v| v != "0")
    })
}

/// Default-on fast path for structurally GC-inert direct self recursion.
///
/// Such a method contains no allocation, no backward edge, and no call except
/// the raw self-call that `try_compile` already proved resolves to this exact
/// method. Its common recursive edge therefore cannot reach a safepoint. The
/// cold native-stack-overflow guard remains a normal safepoint and deliberately
/// publishes an incomplete moving-young map, forcing that exceptional cycle to
/// the safe non-moving fallback.
pub(super) fn gc_inert_selfrec_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_GC_INERT_SELFREC")
            .map_or(true, |v| !matches!(v.as_str(), "0" | "false" | "off"))
    })
}

/// SB-CRASH-04 (register-invisibility) — whether GC-capable safepoints blind-
/// spill every used callee-saved GPR into a reserved frame slot so the
/// conservative root scan marks register-only oops. Enabled when
/// `CRATONVM_JIT_SAFEPOINT_REG_SPILL` is set to any value. Off → byte-identical
/// default path (no slots reserved, no stores). See the field doc on
/// `safepoint_reg_spill`.
pub(super) fn safepoint_reg_spill_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_SAFEPOINT_REG_SPILL").is_some()
    })
}

/// SB-CRASH-04 diagnostic — `CRATONVM_JIT_SAFEPOINT_REG_SPILL=nostore` reserves
/// the per-safepoint register-spill slots (matching the spilling build's frame
/// layout) but emits NO stores, so an A/B vs the full spill separates the effect
/// of the stores from the effect of the frame-size change.
pub(super) fn safepoint_reg_spill_nostore() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_SAFEPOINT_REG_SPILL")
            .map(|v| v.eq_ignore_ascii_case("nostore"))
            .unwrap_or(false)
    })
}

/// Keycloak Gap 9 (register-resident root, A2 class) — `CRATONVM_JIT_SAFEPOINT_REG_SPILL=all`
/// blind-spills EVERY allocatable GPR (not just the callee-saved set) at each
/// GC-capable safepoint. The callee-saved-only spill (`=1`) leaves a live oop
/// held in a *caller-saved* / argument / RAX register at an invoke safepoint
/// invisible to the conservative root scan; spilling the full GPR file closes
/// that residual class. Fully conservative (the scanner re-validates each slot
/// via `heap.is_object_address`) and sound under the non-moving young sweep that
/// `gc_quiescence` forces while any thread is in JIT (no relocation → an
/// over-spilled non-oop bit pattern can only over-retain, never corrupt).
pub(super) fn safepoint_reg_spill_all() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_SAFEPOINT_REG_SPILL")
            .map(|v| v.eq_ignore_ascii_case("all"))
            .unwrap_or(false)
    })
}

/// Elide the SB-CRASH-04 full-GPR blind spill at a DIRECT call to a compiled
/// callee whose caller frame is provably oop-clean there
/// (`CRATONVM_JIT_CALL_SPILL_ELISION`; default `1`).
///
///  * `0` — never elide: every call keeps the unconditional 14-store spill.
///    This is the pre-2026-08-26 behaviour.
///  * `1` — direct calls to a compiled callee only, and only when no argument
///    of the call is a reference.
///  * `args` / `2` — also the `jit_invoke_dispatch` helper site,
///    and reference arguments are admitted whenever they are frame-resident at
///    the `CALL`: the direct sites copy every argument into the callee-sentinel
///    service slots, and the dispatch site stages them into the helper's args
///    buffer and NAMES the oops among them in the safepoint map
///    (`pending_staged_arg_oops`). An argument oop that is only in an ABI
///    register still refuses.
///  * `mic` / `3` (**default**) — additionally the MIC/PIC inline-dispatch
///    cascade, whose hoisted spill dominates both the inline-hit and the slow
///    path and pairs with a single shared post-safepoint reload. Both halves of
///    that pairing survive the elision: the predicate requires `precise_maps`
///    and refuses any register-homed reference local, so the shared
///    `emit_post_safepoint_reload` — which walks `local_oop_masks[pc]`, oops
///    only — has nothing to reload. This is the arm that reaches
///    `invokevirtual`/`invokeinterface`, i.e. most of the call traffic in real
///    code; `CallArgCostProbe`'s `virtRef` is 11.66 → 7.50 ns over control on
///    it.
///
/// Why this exists: the spill is 14 `mov [rbp-off], reg` at EVERY GC-capable
/// call, and `probes/CallArgCostProbe.java` prices a compiled static call at
/// ~4 ns against HotSpot's ~0. A disassembly of its `armInt1` arm
/// (`CRATONVM_DBG_JIT_DISASM=CallArgCostProbe.armInt1`) shows 26 instructions
/// of call overhead on the hot path, 14 of them this spill — in a loop whose
/// frame contains no reference at all.
///
/// The proof it reuses is `can_elide_self_call_register_spill`'s, and nothing
/// in that proof is about the callee: it establishes that every live oop in
/// THIS frame is frame-resident, so a conservative register copy publishes
/// nothing new. The one call-site-specific hazard is an oop ARGUMENT, which is
/// staged in an ABI register at the CALL and is not covered by the caller-frame
/// proof — hence the argument clause, and hence mode `1` refusing outright.
pub(super) fn call_spill_elision_mode() -> u8 {
    use std::sync::OnceLock;
    static G: OnceLock<u8> = OnceLock::new();
    *G.get_or_init(|| {
        match cratonvm_types::flags::runtime_var("CRATONVM_JIT_CALL_SPILL_ELISION") {
            Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
                "0" | "false" | "off" | "no" => 0,
                "1" => 1,
                "args" | "2" => 2,
                _ => 3,
            },
            Err(_) => 3,
        }
    })
}

/// SB-CRASH-04 default-path gap — opt-OUT for folding `precise_maps` into the
/// full-GPR safepoint register spill (see the call site in `Compiler::new`).
/// `precise_maps` has been default-on since 2026-07-07, but its own
/// `emit_pre_safepoint_spill` branch only ever wrote the safepoint-id slot for
/// oop-map lookup — several call sites' own comments (the MIC/PIC inline-
/// dispatch cascade in particular: "the caller's register-only oops must be
/// spilled BEFORE the cascade to be visible to the conservative scan")
/// document spilling registers as their purpose, but that spill only ran when
/// the SEPARATE `CRATONVM_JIT_SAFEPOINT_REG_SPILL` env var was ALSO set —
/// off by default, so the documented protection never actually happened on
/// the default path. `CRATONVM_NO_PRECISE_REG_SPILL=1` restores that
/// pre-fix, env-var-only gating for bisection.
pub(super) fn precise_reg_spill_disabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_NO_PRECISE_REG_SPILL").is_some()
    })
}

/// Keep the legacy full-register spill at otherwise eligible direct
/// self-recursive calls. This is an opt-out/bisection switch for the narrow
/// frame-rooted recursion optimization; the default remains the optimized path.
pub(super) fn full_self_call_spill_requested() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_FULL_SELF_CALL_SPILL").is_some()
    })
}

/// The full set of allocatable GPRs spilled at safepoints under the `=all` gate
/// (every integer register except RSP/RBP, which are the stack/frame pointers
/// and never hold a Java reference). Order is fixed so the reserved frame-slot
/// layout is deterministic.
pub(super) const ALL_SPILL_GPRS: [u8; 14] = [
    RAX, RCX, RDX, RBX, RSI, RDI, R8, R9, R10, R11, R12, R13, R14, R15,
];

/// The registers the inline-TLAB `new` fast path clobbers between the
/// safepoint and its slow-path exits — and therefore the only ones whose
/// blind spill cannot be sunk out of the fast path (see
/// [`alloc_spill_sink_enabled`]).
///
/// `emit_inline_tlab_new`'s preamble, under the sink, is exactly:
///
/// ```asm
/// mov  r11, imm64 / mov eax, [r11] / cmp eax, imm32 / jne slow   ; layout guard
/// mov  rax, [rbp-thread_slot] / test rax,rax / je slow           ; cached thread
/// mov  r10, rax / mov r11, [r10+cursor]                          ; cursor
/// add  r11, 7 / and r11, -8 / lea rax, [r11+size]                ; bump
/// cmp  rax, [r10+end] / ja slow                                  ; TLAB full
/// ```
///
/// which writes RAX, R10 and R11 and nothing else. The sink additionally
/// *requires* that shape: the `get_current_thread` fallback CALL the
/// un-sunk path emits when the cached slot reads null would clobber the
/// whole caller-saved file here, so under the sink a null cached thread
/// diverts to the slow path (`new_object` resolves its own thread) instead.
pub(super) const ALLOC_FAST_PATH_CLOBBERS: [u8; 3] = [RAX, R10, R11];

/// Whether the per-safepoint blind GPR spill at an inline-TLAB `new` is sunk
/// into the allocation's slow path. Default ON; opt out with
/// `CRATONVM_JIT_NO_ALLOC_SPILL_SINK=1`.
///
/// The spill exists so the conservative `[scanner_sp, entry_sp)` frame walk
/// can see an oop that lives only in a register when the collector stops the
/// world. A stop only happens at a safepoint, and the inline-TLAB fast path
/// under `skip_post_init_helper` contains **no call and no poll** — the
/// header stores and the cursor commit are plain memory writes — so no
/// collector can observe the frame between the `new` safepoint and the
/// merge point. The 11 sinkable stores are therefore dead on the fast path
/// and live only on the three slow-path edges, where they are emitted.
///
/// Measured ceiling for the whole blind spill on `CratonBench bintrees`:
/// 1.178x (per-process user CPU, 10 interleaved pairs, disjoint ranges) via
/// `CRATONVM_NO_PRECISE_REG_SPILL=1`, which removes it at every safepoint.
/// This sink claims only the allocation sites' share of that; the self-call
/// safepoints keep their spill, because a call genuinely can collect.
pub(super) fn alloc_spill_sink_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_ALLOC_SPILL_SINK").is_none()
    })
}

/// Whether any local that could hold an object reference currently lives only in
/// a register — the "must spill at this safepoint" question, factored out of
/// [`Compiler::can_elide_self_call_register_spill`] so it is unit-testable
/// without standing up a whole `Compiler`.
///
/// `plan` is `None` on compile paths that never build one (the legacy [`compile`]
/// test wrapper, OSR artifact compilation). Those fall back to the pre-2026-07-26
/// all-or-nothing answer: *any* register-homed local counts, which is strictly
/// more conservative and keeps those paths byte-identical.
///
/// See the caller's doc comment for the five-part soundness argument.
pub(super) fn reference_local_in_register(
    plan: Option<&crate::regalloc::SafepointPublishPlan>,
    local_assignments: &[Option<u8>],
) -> bool {
    match plan {
        Some(plan) => !plan.no_reference_in_registers(),
        None => local_assignments.iter().any(Option::is_some),
    }
}

/// Whether a direct self-call under moving-young may publish an empty precise
/// root map without emitting the normal local/full-GPR spill and shadow push.
///
/// Moving collection suppresses the conservative JIT-frame scan, so this is
/// sound only when the dataflow coverage proof is complete and the exact set
/// of live, rewritable oop homes is empty. Keeping this as a small predicate
/// makes both fail-closed conditions independently testable.
pub(super) fn moving_oop_free_self_call_is_publishable(
    moving_young: bool,
    coverage_complete: bool,
    live_oop_home_count: usize,
) -> bool {
    moving_young && coverage_complete && live_oop_home_count == 0
}

/// Register-only operand-stack-oop soundness — whether `flush_scratch_registers`
/// (the standard pre-call / pre-backward-branch / pre-return flush) ALSO spills
/// `StackSlot::CalleeSaved` operand-stack entries that hold an object reference
/// to a canonical frame slot.
///
/// **DEFAULT ON** (opt out with `CRATONVM_JIT_NO_CALLEE_OOP_FLUSH`). Closes a JIT
/// GC-root soundness hole that is *independent* of the env-gated
/// `CRATONVM_JIT_SAFEPOINT_REG_SPILL` family: a callee-saved register pushed onto
/// the operand stack as a "zero-cost push" (no code emitted until the value is
/// consumed) survives a GC-capable call un-spilled by ABI, so a live oop residing
/// ONLY in that register at the safepoint is invisible to the conservative
/// `[scanner_sp, entry_sp)` frame scan → the object can be reclaimed → use-after-
/// free. `flush_scratch_registers` already spills the `Scratch`/`Xmm` operand
/// entries before every call; this extends the same flush to the `CalleeSaved`
/// reference entries it previously left in registers (exactly the register homes
/// `collect_live_oop_homes` already recognizes, but which the default
/// conservative scan — shadow stack off — never sees). The spill is value-
/// preserving and consumers transparently read the new `StackSlot::Frame` home,
/// so it can only ADD a root, never remove one or change a computed value. When
/// off, the legacy (unsound) flush is restored for A/B bisection only.
pub(super) fn flush_callee_saved_oops_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_CALLEE_OOP_FLUSH").is_none()
    })
}

/// Enable graph-coloured callee-saved GPR homes for Java locals.
///
/// This is now the default when precise JIT maps are active. Every GC-capable
/// call first publishes register locals to their canonical frame slots, precise
/// oop maps describe those slots, moved references are reloaded after the call,
/// and OSR entry carries the allocator's live-in/dead-local masks. Together
/// those contracts make a callee-saved register a real local home across both
/// loop backedges and calls rather than an untracked cache.
///
/// `CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS=0` is retained as a diagnostic
/// opt-out. An explicit true value remains accepted for compatibility, but can
/// never bypass the precise-map requirement.
pub fn callee_saved_gpr_local_homes_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        let requested =
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS")
                .ok()
                .map(|v| {
                    matches!(
                        v.trim().to_ascii_lowercase().as_str(),
                        "1" | "true" | "on" | "yes"
                    )
                })
                .unwrap_or(true);
        requested && (precise_jit_maps_enabled() || moving_young_enabled())
    })
}

/// Pure-kernel callee-saved-GPR local homes (default **ON**, opt out with
/// `CRATONVM_JIT_KERNEL_REG_LOCALS=0`).
///
/// A narrow, provably-safe subset of the gated allocator above: NON-REFERENCE
/// locals of a **pure kernel** method-entry body get callee-saved register
/// homes. "Pure kernel" means the method has no invokes of any kind (no
/// invoke_info/direct_calls/MIC/PIC/indy sites), no field or static-field
/// ops, no allocation, no typechecks, no inline sites, and no speculative
/// BCE guards — i.e. nothing but arithmetic, array element access, and
/// branches (the QuickBench sieve/matrix shape). Under those constraints the
/// documented miscompile family ("live Java values kept exclusively in
/// callee-saved GPRs across calls/OSR transitions") is structurally
/// unreachable:
///  * no calls → no value survives a call in a register;
///  * reference locals are excluded (see `regalloc::find_reference_locals`),
///    so GC root scanning and every deopt/exception path that reads locals
///    from frame slots is unaffected;
///  * the body is published WITHOUT OSR entry points (`osr_pc_to_native`
///    left empty), so no OSR transition can enter it mid-loop — the
///    separately-compiled OSR artifact keeps memory-homed locals;
///  * the remaining implicit-exception paths (AIOOBE/NPE stubs) return the
///    deopt sentinel and re-execute the whole call in the interpreter from
///    the original arguments, never reading JIT frame local slots.
///
/// Requested per-compile by `try_compile` (method-entry only) via
/// [`set_kernel_reg_homes_request`]; OSR compiles (`compile_osr_artifact`)
/// and the legacy [`compile`] test wrapper never set it.
pub fn kernel_reg_locals_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_KERNEL_REG_LOCALS")
            .map(|v| {
                let v = v.trim();
                !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
            })
            .unwrap_or(true)
    })
}

thread_local! {
    /// Per-compile request flag for the pure-kernel GPR local homes (see
    /// [`kernel_reg_locals_enabled`]). Set by the method-entry compile path
    /// immediately before calling [`compile_with_param_slots`]; consumed
    /// (taken) at its entry so it can never leak into a later compile on the
    /// same thread.
    pub(super) static KERNEL_REG_HOMES_REQUEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Request pure-kernel GPR local homes for the NEXT `compile_with_param_slots`
/// call on this thread (method-entry compiles only — never OSR).
pub fn set_kernel_reg_homes_request(on: bool) {
    KERNEL_REG_HOMES_REQUEST.with(|c| c.set(on));
}

thread_local! {
    /// One-shot request from the bytecode front-end: this method has an
    /// exception handler that reads locals beyond its incoming parameters, so
    /// post-invoke exceptions must retain a precise frame until handler entry.
    pub(super) static PRECISE_EXCEPTION_FRAME_REQUEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Request precise exceptional-frame capture for the next method compile.
pub fn set_precise_exception_frame_request(on: bool) {
    PRECISE_EXCEPTION_FRAME_REQUEST.with(|c| c.set(on));
}

thread_local! {
    /// One-shot `[start_pc, end_pc)` list of the next method's exception-table
    /// protected ranges, published by the bytecode front-end.
    ///
    /// The backend is otherwise entirely exception-table-blind, and that is
    /// fine for every lowering that RETURNS to this frame: the shared exception
    /// stub re-enters the interpreter at the throwing bci and the method's own
    /// handler table takes over from there. It is NOT fine for the sibling
    /// tail-call, which tears this frame down and `JMP`s into the callee, so an
    /// exception the callee raises unwinds straight past a handler that was
    /// supposed to catch it. See `pc_is_protected`.
    pub(super) static PROTECTED_RANGES_REQUEST: std::cell::Cell<Option<Vec<(u32, u32)>>> =
        const { std::cell::Cell::new(None) };
}

/// Publish the next method compile's exception-table protected ranges.
/// One-shot, like [`set_precise_exception_frame_request`], so a bailed compile
/// cannot leak its ranges into the next unrelated method on this worker thread.
pub fn set_protected_ranges_request(ranges: Vec<(u32, u32)>) {
    PROTECTED_RANGES_REQUEST.with(|c| c.set(if ranges.is_empty() { None } else { Some(ranges) }));
}

thread_local! {
    /// OSR-tier sibling of [`KERNEL_REG_HOMES_REQUEST`] — set (only) by the
    /// interpreter's `compile_osr_artifact` (perf/halfgap-20260717).
    pub(super) static KERNEL_REG_HOMES_OSR_REQUEST: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Request pure-kernel GPR local homes for the NEXT OSR-artifact compile on
/// this thread (perf/halfgap-20260717).
///
/// The original kernel-homes rollout vetoed OSR bodies wholesale ("publishes
/// NO OSR entries") as blanket caution while the feature soaked on the
/// method-entry tier. The machinery for a register-homed OSR ENTRY has
/// always existed, though: the OSR trampoline seeds each interpreter local
/// into `osr_local_assignments[i]`'s register (that is the normal
/// graph-coloring entry contract), and with kernel homes those assignments
/// ARE the kernel's callee-saved homes (reference locals are masked back to
/// frame homes, so GC visibility is unchanged). A pure-kernel body accepted
/// by the same call/field/alloc/typecheck/spec-BCE-free conditions has no
/// in-body transition that could observe a stale frame slot. This matters
/// because once-invoked benchmark-style kernels (`benchArithmetic`,
/// `matmul`) live their entire life inside the OSR artifact and previously
/// ran memory-homed. Opt out with `CRATONVM_JIT_KERNEL_REG_OSR=0`.
pub fn set_kernel_reg_homes_osr_request(on: bool) {
    KERNEL_REG_HOMES_OSR_REQUEST.with(|c| c.set(on));
}

/// `CRATONVM_JIT_KERNEL_REG_OSR` gate (default **ON**, opt out with `=0`) —
/// see [`set_kernel_reg_homes_osr_request`].
///
/// The original 2026-07-18 experiment predated constant long-division
/// lowering, so Arithmetic was division-bound and register homes had no
/// measurable effect. Once constant `ldiv`/`lrem` stopped dominating, the
/// same pure-kernel gate became useful: loop-carried primitive locals and the
/// deferred operand cache can remain in registers. The admission predicate
/// below still excludes calls, fields, allocation, typechecks, speculative
/// BCE, and reference locals, and the OSR trampoline seeds the exact assigned
/// registers before entering the artifact.
pub(super) fn kernel_reg_osr_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_KERNEL_REG_OSR")
            .map(|v| {
                let v = v.trim();
                !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
            })
            .unwrap_or(true)
    })
}

thread_local! {
    /// Internal handshake between `compile_with_param_slots` (which decides
    /// whether the pure-kernel GPR local homes engage) and `Compiler::new`
    /// (which owns the legacy env-flag gate that would otherwise zero the
    /// register assignments). Set strictly around the `Compiler::new` call.
    pub(super) static KERNEL_REG_HOMES_ACTIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Adjacent store→load reload elision (default **ON**, opt out with
/// `CRATONVM_JIT_NO_SLOT_MIRROR=1`). See `Compiler::slot_mirror`.
pub(super) fn slot_mirror_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_SLOT_MIRROR").is_none()
    })
}

pub(super) fn compute_branch_targets(code: &[u8], code_len: usize) -> Vec<bool> {
    let mut targets = vec![false; code_len];
    let mut pc = 0usize;
    while pc < code_len {
        let op = code[pc];
        match op {
            // Conditional branches + goto + jsr: 2-byte signed offset from `pc`.
            0x99..=0xA8 | 0xC6 | 0xC7 => {
                if pc + 2 < code_len {
                    // Cast: signed offset to isize for pointer/index arithmetic
                    let off = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as isize;
                    // Cast: signed offset to isize for pointer/index arithmetic
                    let t = pc as isize + off;
                    // Cast: non-negative index/count to usize
                    if t >= 0 && (t as usize) < code_len {
                        // Cast: non-negative index/count to usize
                        targets[t as usize] = true;
                    }
                }
                pc += 3;
            }
            // goto_w / jsr_w: 4-byte signed offset from `pc`.
            0xC8 | 0xC9 => {
                if pc + 4 < code_len {
                    let off = i32::from_be_bytes([
                        code[pc + 1],
                        code[pc + 2],
                        code[pc + 3],
                        code[pc + 4],
                        // Cast: signed offset to isize for pointer/index arithmetic
                    ]) as isize;
                    // Cast: signed offset to isize for pointer/index arithmetic
                    let t = pc as isize + off;
                    // Cast: non-negative index/count to usize
                    if t >= 0 && (t as usize) < code_len {
                        // Cast: non-negative index/count to usize
                        targets[t as usize] = true;
                    }
                }
                pc += 5;
            }
            // tableswitch: default + (high-low+1) offsets, all relative to `pc`.
            0xAA => {
                let mut p = pc + 1;
                while p % 4 != 0 {
                    p += 1;
                }
                if p + 12 > code_len {
                    break;
                }
                let read_off = |code: &[u8], at: usize| -> isize {
                    i32::from_be_bytes([code[at], code[at + 1], code[at + 2], code[at + 3]])
                        // Cast: signed offset to isize for pointer/index arithmetic
                        as isize
                };
                let mark = |targets: &mut Vec<bool>, off: isize| {
                    // Cast: signed offset to isize for pointer/index arithmetic
                    let t = pc as isize + off;
                    // Cast: non-negative index/count to usize
                    if t >= 0 && (t as usize) < code_len {
                        // Cast: non-negative index/count to usize
                        targets[t as usize] = true;
                    }
                };
                mark(&mut targets, read_off(code, p)); // default
                                                       // Cast: value to i32 (encoding immediate/displacement)
                let low = read_off(code, p + 4) as i32;
                // Cast: value to i32 (encoding immediate/displacement)
                let high = read_off(code, p + 8) as i32;
                let count = checked_tableswitch_count(low, high).unwrap_or(0);
                let mut jp = p + 12;
                for _ in 0..count {
                    if jp + 4 > code_len {
                        break;
                    }
                    mark(&mut targets, read_off(code, jp));
                    jp += 4;
                }
                pc += bytecode_len_at(code, pc);
            }
            // lookupswitch: default + npairs (match, offset) pairs.
            0xAB => {
                let mut p = pc + 1;
                while p % 4 != 0 {
                    p += 1;
                }
                if p + 8 > code_len {
                    break;
                }
                let read_off = |code: &[u8], at: usize| -> isize {
                    i32::from_be_bytes([code[at], code[at + 1], code[at + 2], code[at + 3]])
                        // Cast: signed offset to isize for pointer/index arithmetic
                        as isize
                };
                let mark = |targets: &mut Vec<bool>, off: isize| {
                    // Cast: signed offset to isize for pointer/index arithmetic
                    let t = pc as isize + off;
                    // Cast: non-negative index/count to usize
                    if t >= 0 && (t as usize) < code_len {
                        // Cast: non-negative index/count to usize
                        targets[t as usize] = true;
                    }
                };
                mark(&mut targets, read_off(code, p)); // default
                let npairs = i32::from_be_bytes([code[p + 4], code[p + 5], code[p + 6], code[p + 7]]).max(0)
                        // Cast: non-negative index/count to usize
                        as usize;
                let mut jp = p + 8;
                for _ in 0..npairs {
                    if jp + 8 > code_len {
                        break;
                    }
                    // pair is (match:i32, offset:i32); offset at jp+4.
                    mark(&mut targets, read_off(code, jp + 4));
                    jp += 8;
                }
                pc += bytecode_len_at(code, pc);
            }
            _ => {
                pc += bytecode_len_at(code, pc);
            }
        }
    }
    targets
}

/// Stage 2 (precise oop maps) — enumerate the control-flow successors of the
/// instruction at `pc` (normal flow only; exception-handler edges are not
/// available to the JIT and are handled conservatively by leaving handler-only
/// PCs `unreached`). Mirrors the branch/switch decoding in
/// [`compute_branch_targets`] and the per-opcode length in [`bytecode_len_at`].
pub(super) fn oop_dataflow_successors(code: &[u8], code_len: usize, pc: usize) -> Vec<usize> {
    let op = code[pc];
    let fallthrough = pc + bytecode_len_at(code, pc);
    let read_i16 = |at: usize| -> isize {
        if at + 1 < code_len {
            // Cast: signed offset to isize for pointer/index arithmetic
            i16::from_be_bytes([code[at], code[at + 1]]) as isize
        } else {
            0
        }
    };
    let read_i32 = |at: usize| -> isize {
        if at + 3 < code_len {
            // Cast: signed offset to isize for pointer/index arithmetic
            i32::from_be_bytes([code[at], code[at + 1], code[at + 2], code[at + 3]]) as isize
        } else {
            0
        }
    };
    let target = |off: isize| -> Option<usize> {
        // Cast: signed offset to isize for pointer/index arithmetic
        let t = pc as isize + off;
        // Cast: non-negative index/count to usize
        if t >= 0 && (t as usize) < code_len {
            // Cast: non-negative index/count to usize
            Some(t as usize)
        } else {
            None
        }
    };
    match op {
        // ireturn/lreturn/freturn/dreturn/areturn/return, athrow, ret:
        // no normal successor.
        0xAC..=0xB1 | 0xBF | 0xA9 => Vec::new(),
        // goto / goto_w: unconditional, target only.
        0xA7 => target(read_i16(pc + 1)).into_iter().collect(),
        0xC8 => target(read_i32(pc + 1)).into_iter().collect(),
        // jsr / jsr_w: target + fall-through (return address pushed).
        0xA8 => {
            let mut v: Vec<usize> = target(read_i16(pc + 1)).into_iter().collect();
            if fallthrough < code_len {
                v.push(fallthrough);
            }
            v
        }
        0xC9 => {
            let mut v: Vec<usize> = target(read_i32(pc + 1)).into_iter().collect();
            if fallthrough < code_len {
                v.push(fallthrough);
            }
            v
        }
        // Conditional branches (if<cond>, if_icmp<cond>, if_acmp<cond>) and
        // ifnull/ifnonnull: target + fall-through.
        0x99..=0xA6 | 0xC6 | 0xC7 => {
            let mut v: Vec<usize> = target(read_i16(pc + 1)).into_iter().collect();
            if fallthrough < code_len {
                v.push(fallthrough);
            }
            v
        }
        // tableswitch: default + each table entry; no fall-through.
        0xAA => {
            let mut p = pc + 1;
            while p % 4 != 0 {
                p += 1;
            }
            let mut v = Vec::new();
            if p + 12 <= code_len {
                if let Some(t) = target(read_i32(p)) {
                    v.push(t);
                }
                // Cast: value to i32 (encoding immediate/displacement)
                let low = read_i32(p + 4) as i32;
                // Cast: value to i32 (encoding immediate/displacement)
                let high = read_i32(p + 8) as i32;
                let count = checked_tableswitch_count(low, high).unwrap_or(0);
                let mut jp = p + 12;
                for _ in 0..count {
                    if jp + 4 > code_len {
                        break;
                    }
                    if let Some(t) = target(read_i32(jp)) {
                        v.push(t);
                    }
                    jp += 4;
                }
            }
            v
        }
        // lookupswitch: default + each pair offset; no fall-through.
        0xAB => {
            let mut p = pc + 1;
            while p % 4 != 0 {
                p += 1;
            }
            let mut v = Vec::new();
            if p + 8 <= code_len {
                if let Some(t) = target(read_i32(p)) {
                    v.push(t);
                }
                // Cast: non-negative index/count to usize
                let npairs = read_i32(p + 4).max(0) as usize;
                let mut jp = p + 8;
                for _ in 0..npairs {
                    if jp + 8 > code_len {
                        break;
                    }
                    if let Some(t) = target(read_i32(jp + 4)) {
                        v.push(t);
                    }
                    jp += 8;
                }
            }
            v
        }
        // Everything else: fall-through only.
        _ => {
            if fallthrough < code_len {
                vec![fallthrough]
            } else {
                Vec::new()
            }
        }
    }
}

/// Stage 2 (precise oop maps) — transfer function for the per-local "must be
/// oop" dataflow: apply the store at `pc` to `mask`. `astore*` makes a local
/// definitely-oop; primitive stores (`istore`/`lstore`/`fstore`/`dstore`/
/// `iinc`) make it definitely-non-oop; long/double stores also clear the
/// category-2 high half. All other opcodes leave the local types unchanged.
pub(super) fn oop_dataflow_transfer(code: &[u8], pc: usize, mask: u64, max_locals: usize) -> u64 {
    let set_oop = |m: u64, k: usize| -> u64 {
        if k < 64 && k < max_locals {
            m | (1u64 << k)
        } else {
            m
        }
    };
    let clr = |m: u64, k: usize, span: usize| -> u64 {
        let mut m = m;
        for j in k..k + span {
            if j < 64 {
                m &= !(1u64 << j);
            }
        }
        m
    };
    let op = code[pc];
    match op {
        // Widening: u8 -> wider int (bytecode operand byte, value fits)
        0x3A => set_oop(mask, code.get(pc + 1).copied().unwrap_or(0) as usize), // astore
        // Widening: u8 -> usize (opcode-relative local index, value fits)
        0x4B..=0x4E => set_oop(mask, (op - 0x4B) as usize), // astore_0..3
        // Widening: u8 -> wider int (bytecode operand byte, value fits)
        0x36 | 0x38 => clr(mask, code.get(pc + 1).copied().unwrap_or(0) as usize, 1), // istore/fstore
        // Widening: u8 -> wider int (bytecode operand byte, value fits)
        0x37 | 0x39 => clr(mask, code.get(pc + 1).copied().unwrap_or(0) as usize, 2), // lstore/dstore
        // Widening: u8 -> usize (opcode-relative local index, value fits)
        0x3B..=0x3E => clr(mask, (op - 0x3B) as usize, 1), // istore_0..3
        // Widening: u8 -> usize (opcode-relative local index, value fits)
        0x43..=0x46 => clr(mask, (op - 0x43) as usize, 1), // fstore_0..3
        // Widening: u8 -> usize (opcode-relative local index, value fits)
        0x3F..=0x42 => clr(mask, (op - 0x3F) as usize, 2), // lstore_0..3
        // Widening: u8 -> usize (opcode-relative local index, value fits)
        0x47..=0x4A => clr(mask, (op - 0x47) as usize, 2), // dstore_0..3
        // Widening: u8 -> wider int (bytecode operand byte, value fits)
        0x84 => clr(mask, code.get(pc + 1).copied().unwrap_or(0) as usize, 1), // iinc
        0xC4 => {
            // wide <store>/<iinc>: 2-byte index.
            let wop = code.get(pc + 1).copied().unwrap_or(0);
            let idx = if pc + 3 < code.len() {
                // Cast: non-negative index/count to usize
                u16::from_be_bytes([code[pc + 2], code[pc + 3]]) as usize
            } else {
                0
            };
            match wop {
                0x3A => set_oop(mask, idx),
                0x36 | 0x38 => clr(mask, idx, 1),
                0x37 | 0x39 => clr(mask, idx, 2),
                0x84 => clr(mask, idx, 1),
                _ => mask,
            }
        }
        _ => mask,
    }
}

/// Stage 2 (precise oop maps) — forward "must be oop" dataflow over local
/// variable slots. Returns `(in_mask, reached)` each of length `code_len`:
/// `in_mask[pc] & (1<<k) != 0` iff local `k` holds an object reference on
/// EVERY normal-control-flow path reaching `pc`, and `reached[pc]` is true iff
/// the forward analysis visited `pc`.
///
/// Soundness for the *moving* GC consumer (Stage 5): a bit is set only when
/// the slot was `astore`d on every reaching path, so the slot genuinely holds
/// an oop — never a primitive (no false positive → no int rewritten). A live
/// oop that is conditionally a primitive on another path merges to "not
/// definitely oop" and is omitted; such a slot is necessarily dead-as-oop at
/// that PC (the verifier forbids reading a slot with conflicting types), so the
/// omission is safe. PCs reachable only via exception handlers (whose edges are
/// not visible here) stay `unreached`; the caller emits no precise local
/// entries there and the GC falls back to the conservative frame sweep.
///
/// `> 64` locals → empty result (caller falls back to conservative).
///
/// Stage A.4 (precise oop maps, B-K fix) — `param_oop_mask` seeds the entry
/// state: bit `k` set ⇒ JVM local slot `k` holds a REFERENCE parameter on method
/// entry (`this` + every `L…;`/`[…` declared param, per
/// [`crate`]'s `compute_param_oop_mask`). This makes an oop parameter that is
/// live across an EARLY safepoint — before any `astore` rewrites its slot —
/// precisely covered rather than left to the conservative sweep, which is the
/// last false-negative that blocked fully-covered status for reference-param
/// methods. The caller passes `0` when the precise gate is off, so the default
/// path is byte-identical (entry state empty, exactly as before).
pub(super) fn compute_local_oop_masks(
    code: &[u8],
    code_len: usize,
    max_locals: usize,
    param_oop_mask: u64,
) -> (Vec<u64>, Vec<bool>) {
    if max_locals == 0 || max_locals > 64 || code_len == 0 {
        return (Vec::new(), Vec::new());
    }
    const TOP: u64 = u64::MAX;
    let mut in_mask = vec![TOP; code_len];
    let mut reached = vec![false; code_len];
    // Entry: reference parameters are oops; everything else starts non-oop.
    // `param_oop_mask` is 0 on the default (gate-off) path → identical to the
    // historical conservative `in_mask[0] = 0` seed.
    in_mask[0] = param_oop_mask;
    reached[0] = true;
    let mut work: Vec<usize> = vec![0];
    // Bound iterations defensively against any decoding pathology.
    let mut guard = code_len.saturating_mul(64).saturating_add(64);
    while let Some(pc) = work.pop() {
        if pc >= code_len {
            continue;
        }
        guard = guard.saturating_sub(1);
        if guard == 0 {
            break;
        }
        let out = oop_dataflow_transfer(code, pc, in_mask[pc], max_locals);
        for succ in oop_dataflow_successors(code, code_len, pc) {
            if succ >= code_len {
                continue;
            }
            // First real predecessor seeds; later ones intersect (AND).
            let new_in = if reached[succ] {
                in_mask[succ] & out
            } else {
                out
            };
            if !reached[succ] || new_in != in_mask[succ] {
                in_mask[succ] = new_in;
                reached[succ] = true;
                work.push(succ);
            }
        }
    }
    // Mask off bits beyond max_locals (TOP-init residue on any unreached PC is
    // irrelevant — callers gate on `reached`).
    let valid_bits = if max_locals >= 64 {
        u64::MAX
    } else {
        (1u64 << max_locals) - 1
    };
    for m in in_mask.iter_mut() {
        *m &= valid_bits;
    }
    (in_mask, reached)
}

/// EC-DUP2-CAT2 (bc math-ec JIT miscompile fix) — reject methods whose
/// `dup2` (0x5C) operates on a CATEGORY-2 (long / double) value.
///
/// ## The bug
///
/// The JIT models the operand stack with ONE entry per value, regardless of
/// type width (a `long`/`double` is a single 64-bit stack entry; `ladd` pops
/// two entries and pushes one — see the arithmetic handlers). That single-slot
/// model is internally consistent for arithmetic, but `dup2` (0x5C) is
/// type-dependent in the JVM:
///
///   * FORM 1 — top two operands are each category-1: `[…, v1, v2]` →
///     `[…, v1, v2, v1, v2]`.
///   * FORM 2 — top operand is a single category-2 value: `[…, w]` →
///     `[…, w, w]`.
///
/// The codegen `dup2` handler unconditionally implements FORM 1: it reads
/// `stack[len-2]` and `stack[len-1]` and re-pushes both. For a single
/// category-2 operand (FORM 2) there is only ONE real value on top, so
/// `stack[len-2]` is an UNRELATED value living below the long/double. The
/// handler then duplicates that unrelated value, corrupting the operand
/// stack: a subsequent consumer reads the wrong slot, and an `int` (or other
/// primitive) ends up where an object/array reference is expected. The bad
/// "reference" (a small integer like 1 or 3) is later dereferenced by an
/// inline `getfield`/array op — observed as `EXCEPTION_ACCESS_VIOLATION`
/// reading `[1 + 0x30]` (= field-0 payload off a base of `1`) and
/// `[3 + 0xC]` (= array-length off a base of `3`) in `org.bouncycastle.
/// math.ec` AllTests, and as a bad pointer the young-gen GC scavenge later
/// follows into a SEGV.
///
/// `jit_scan` already ACCEPTS `dup2` (it only advances `pc`), and at the time
/// this was written the five other type-dependent stack ops (`pop2`,
/// `dup_x1`, `dup_x2`, `dup2_x1`, `dup2_x2`) were not implemented by codegen
/// and safely bailed, so `dup2` was the lone miscompile. **All five have
/// since grown codegen arms**, each proving its form before shuffling —
/// `dup2_x2` last, in 2026-08-18. Do not read the sentence above as a live
/// statement about the codegen's repertoire; this whole helper is retained
/// for reference only (see the NOTE below).
///
/// ## The fix
///
/// Track operand-stack category WIDTH (1 or 2 per value) with a precise
/// linear abstract interpretation. At each `dup2`, if the top value is
/// category-2 — or if the width state is in any way uncertain — reject the
/// whole method (`jit_scan` returns `None`), leaving it to the interpreter.
/// The common FORM-1 `dup2` (the `arr[i] op= x` array-update idiom, where the
/// top two operands are an arrayref + int index — both category-1) is
/// unaffected and still JITs.
///
/// Soundness: under JVMS verification the stack height AND the type-width at
/// every PC are invariant across all incoming control-flow edges, so a single
/// linear width-tracking pass observes the correct top-of-stack width at each
/// `dup2` regardless of branches. To stay safe even on bytecode shapes this
/// tracker models imprecisely, ANY uncertainty (stack underflow in the
/// abstract model, or an opcode whose exact width effect is not modeled while
/// a `dup2` is still reachable) conservatively triggers rejection.
///
/// Returns `true` when the method is SAFE to JIT w.r.t. `dup2`, `false` when
/// it must be rejected.
///
/// NOTE: no longer wired into `jit_scan` — the live `dup2` category decision
/// now happens in the codegen (`Compiler::dup2_top_cat2`), which (unlike this
/// CP-less scan) resolves field/invoke descriptors and so distinguishes FORM-1
/// from FORM-2 instead of rejecting the whole method. Retained for reference.
#[allow(dead_code)]
pub(super) fn dup2_category_safe(code: &[u8], code_len: usize) -> bool {
    // Fast path: no `dup2` anywhere → nothing to check.
    let mut has_dup2 = false;
    {
        let mut p = 0usize;
        while p < code_len {
            if code[p] == 0x5C {
                has_dup2 = true;
                break;
            }
            p += bytecode_len_at(code, p);
        }
    }
    if !has_dup2 {
        return true;
    }

    // Abstract operand stack of category widths, one entry per value:
    //   1 = category-1 (int/float/ref/returnAddress; high-half is implicit)
    //   2 = category-2 (long/double) — a SINGLE entry in this single-slot model
    //
    // An empty model (e.g. just after a `widths.clear()` on an op whose exact
    // width effect we do not track) makes a subsequent `dup2` default to
    // FORM-1 — matching the codegen — rather than rejecting, so JIT coverage
    // is preserved. We reject only when a `dup2`'s top is PROVABLY category-2.
    let mut widths: Vec<u8> = Vec::with_capacity(16);

    // Helpers ------------------------------------------------------------
    macro_rules! push {
        ($w:expr) => {
            widths.push($w)
        };
    }
    macro_rules! pop {
        () => {
            widths.pop()
        };
    }

    let mut pc = 0usize;
    while pc < code_len {
        let op = code[pc];
        match op {
            // --- pushes: category-1 producers ---
            // aconst_null, iconst*, fconst*, bipush, sipush, ldc/ldc_w,
            // iload*, fload*, aload*, i/f/a/b/c/saload, new, newarray,
            // anewarray, arraylength, instanceof, i2f/i2b/i2c/i2s/l2i/f2i/
            // d2i/l2f/d2f, fcmp/lcmp/dcmp (push int), etc.
            0x01 | 0x02..=0x08 | 0x0b..=0x0d => {
                push!(1);
                pc += 1;
            }
            0x10 => {
                push!(1);
                pc += 2;
            } // bipush
            0x11 => {
                push!(1);
                pc += 3;
            } // sipush
            0x12 => {
                push!(1);
                pc += 2;
            } // ldc
            0x13 => {
                push!(1);
                pc += 3;
            } // ldc_w
            0x15 => {
                push!(1);
                pc += 2;
            } // iload
            0x17 => {
                push!(1);
                pc += 2;
            } // fload
            0x19 => {
                push!(1);
                pc += 2;
            } // aload
            0x1a..=0x1d => {
                push!(1);
                pc += 1;
            } // iload_0..3
            0x22..=0x25 => {
                push!(1);
                pc += 1;
            } // fload_0..3
            0x2a..=0x2d => {
                push!(1);
                pc += 1;
            } // aload_0..3

            // --- pushes: category-2 producers ---
            0x09 | 0x0a | 0x0e | 0x0f => {
                push!(2);
                pc += 1;
            } // l/dconst
            0x14 => {
                push!(2);
                pc += 3;
            } // ldc2_w (long/double)
            0x16 => {
                push!(2);
                pc += 2;
            } // lload
            0x18 => {
                push!(2);
                pc += 2;
            } // dload
            0x1e..=0x21 => {
                push!(2);
                pc += 1;
            } // lload_0..3
            0x26..=0x29 => {
                push!(2);
                pc += 1;
            } // dload_0..3

            // --- array loads (pop arrayref+index, push element) ---
            // iaload/faload/aaload/baload/caload/saload → cat-1 element
            0x2e | 0x30 | 0x32 | 0x33 | 0x34 | 0x35 => {
                pop!();
                pop!();
                push!(1);
                pc += 1;
            }
            // laload (0x2f) / daload (0x31) → cat-2 element
            0x2f | 0x31 => {
                pop!();
                pop!();
                push!(2);
                pc += 1;
            }

            // --- stores (pop the value; locals are untracked) ---
            0x36 | 0x37 | 0x3a => {
                pop!();
                pc += 2;
            } // istore/fstore/astore
            0x38 | 0x39 => {
                pop!();
                pc += 2;
            } // lstore/dstore (one entry)
            0x3b..=0x3e | 0x43..=0x4e => {
                pop!();
                pc += 1;
            } // istore_/fstore_/astore_
            0x3f..=0x42 => {
                pop!();
                pc += 1;
            } // lstore_0..3 (one entry)
            // dstore_0..3 fall under 0x47..=0x4a, included in 0x43..=0x4a above

            // --- array stores (pop value, index, arrayref) ---
            0x4f..=0x56 => {
                pop!();
                pop!();
                pop!();
                pc += 1;
            }

            // --- stack manipulation (the crux) ---
            //
            // POLICY: this analyzer's ONLY purpose is to reject methods whose
            // `dup2` (0x5C) operates on a CATEGORY-2 value. When it was
            // written, the other ambiguous ops (`pop2`, `dup_x1`, `dup_x2`,
            // `dup2_x1`, `dup2_x2`) had no codegen arm — they hit the
            // `_ =>` bail in `compile_bytecode` and the method safely stayed
            // interpreted — so we did not reject for them here. They all have
            // arms now, which does not change this helper's policy (it is no
            // longer wired into `jit_scan` at all), but does mean the
            // parenthetical is history, not a fact about today's codegen. An imprecisely-modeled state is
            // handled by CLEARING the abstract stack (treat subsequent values
            // as unknown) rather than rejecting outright, preserving JIT
            // coverage up to the next ambiguity. A `dup2` reached against a
            // cleared/unknown top, however, is REJECTED (stay interpreted): we
            // cannot prove the top is category-1, and the codegen's FORM-1-only
            // `dup2` hard-aborts / desyncs on a category-2 top (the common
            // `getfield <J/D>; dup2` BigDecimal shape). Soundness beats the
            // small JIT-coverage loss — see the `_` arm under opcode 0x5c.
            //
            // pop (cat-1)
            0x57 => {
                pop!();
                pc += 1;
            }
            // pop2: two cat-1 OR one cat-2. This model only needs the height
            // roughly sane; the codegen's own `pop2` arm proves its form.
            0x58 => {
                match widths.last().copied() {
                    Some(2) => {
                        pop!();
                    }
                    _ => {
                        pop!();
                        pop!();
                    }
                }
                pc += 1;
            }
            // dup (cat-1 only by JVMS; duplicate top width).
            0x59 => {
                let w = widths.last().copied().unwrap_or(1);
                push!(w);
                pc += 1;
            }
            // dup_x1 / dup_x2 — both have codegen arms now; this model still
            // cannot follow the shuffle, so resync loosely. Clear so we do not
            // mis-evaluate a later dup2 against a now-shuffled stack we no
            // longer model precisely.
            0x5a | 0x5b => {
                widths.clear();
                pc += 1;
            }
            // dup2 — THE checked op. Reject ONLY when the top is PROVABLY
            // category-2 (FORM 2), which the codegen mis-duplicates as two
            // category-1 entries.
            0x5c => {
                match widths.last().copied() {
                    Some(2) => {
                        // FORM 2: single category-2 value on top — codegen
                        // miscompiles this. Reject the method.
                        return false;
                    }
                    Some(1) => {
                        // Top is category-1. If the second entry is also a
                        // known category-1, this is a safe FORM-1 dup2; model
                        // the duplication. If the second entry is category-2
                        // (an unusual but possible verified shape), the codegen
                        // would also mishandle it, so reject.
                        let len = widths.len();
                        if len >= 2 && widths[len - 2] == 2 {
                            return false;
                        }
                        let a = widths.get(len.wrapping_sub(2)).copied().unwrap_or(1);
                        let b = 1u8;
                        push!(a);
                        push!(b);
                    }
                    _ => {
                        // None — the abstract model is empty/cleared at this `dup2`
                        // (the top operand was produced by a preceding field/method
                        // op, branch target, or other op whose width this
                        // descriptor-less helper cannot determine, so it CLEARED the
                        // model). We cannot prove the top is category-1, so we cannot
                        // rule out FORM-2: a single category-2 long/double, the very
                        // common `getfield <J/D>; dup2` / `invokevirtual ()J; dup2`
                        // shape in BigDecimal/decimal bytecode. The codegen `dup2`
                        // implements only FORM-1 and would index `self.stack[len - 2]`
                        // with `len == 1` (a `usize` underflow → index-out-of-bounds
                        // panic / hard VM abort) or, when the real height is ≥2,
                        // duplicate an unrelated lower slot (operand-stack desync →
                        // primitive-as-reference → bad-pointer deref). Both are far
                        // worse than forgoing JIT, so reject and stay interpreted.
                        //
                        // Previously this arm optimistically assumed FORM-1, which is
                        // what let decimal/sort/rownum methods JIT-compile and then
                        // crash (deterministically on the decimal path, flakily
                        // elsewhere depending on JIT timing).
                        return false;
                    }
                }
                pc += 1;
            }
            // dup2_x1 / dup2_x2 — both have codegen arms now, but their form
            // depends on the width of entries this descriptor-less model
            // cannot see. Resync loosely by clearing, which is still sound
            // here: a later `dup2` against a cleared top is REJECTED.
            0x5d | 0x5e => {
                widths.clear();
                pc += 1;
            }
            // swap (two cat-1) — unimplemented for cat-2 mixes; just model.
            0x5f => {
                let len = widths.len();
                if len >= 2 {
                    widths.swap(len - 1, len - 2);
                }
                pc += 1;
            }

            // --- arithmetic / logic ---
            // int ops that pop 2 push 1 (cat-1): iadd/isub/imul/idiv/irem,
            // ishl/ishr/iushr/iand/ior/ixor.
            0x60 | 0x64 | 0x68 | 0x6c | 0x70 | 0x78 | 0x7a | 0x7c | 0x7e | 0x80 | 0x82 => {
                pop!(); /* top stays cat-1 */
                pc += 1;
            }
            // float ops pop2 push1 (cat-1): fadd/fsub/fmul/fdiv/frem.
            0x62 | 0x66 | 0x6a | 0x6e | 0x72 => {
                pop!();
                pc += 1;
            }
            // long ops that pop 2 push 1 (cat-2): ladd/lsub/lmul/ldiv/lrem,
            // land/lor/lxor.
            0x61 | 0x65 | 0x69 | 0x6d | 0x71 | 0x7f | 0x81 | 0x83 => {
                pop!(); /* result cat-2, top entry already cat-2 */
                pc += 1;
            }
            // double ops pop2 push1 (cat-2): dadd/dsub/dmul/ddiv/drem.
            0x63 | 0x67 | 0x6b | 0x6f | 0x73 => {
                pop!();
                pc += 1;
            }
            // long shifts: lshl/lshr/lushr pop an int shift amount (cat-1),
            // value stays cat-2.
            0x79 | 0x7b | 0x7d => {
                pop!();
                pc += 1;
            }
            // unary negate: ineg/fneg (cat-1), lneg/dneg (cat-2) — width
            // unchanged.
            0x74..=0x77 => {
                pc += 1;
            }

            // --- conversions (replace top width) ---
            0x85 => {
                pop!();
                push!(2);
                pc += 1;
            } // i2l → long  (cat-2)
            0x86 => {
                pop!();
                push!(1);
                pc += 1;
            } // i2f → float (cat-1)
            0x87 => {
                pop!();
                push!(2);
                pc += 1;
            } // i2d → double(cat-2)
            0x88 => {
                pop!();
                push!(1);
                pc += 1;
            } // l2i → int   (cat-1)
            0x89 => {
                pop!();
                push!(1);
                pc += 1;
            } // l2f → float (cat-1)
            0x8a => {
                pop!();
                push!(2);
                pc += 1;
            } // l2d → double(cat-2)
            0x8b => {
                pop!();
                push!(1);
                pc += 1;
            } // f2i → int   (cat-1)
            0x8c => {
                pop!();
                push!(2);
                pc += 1;
            } // f2l → long  (cat-2)
            0x8d => {
                pop!();
                push!(2);
                pc += 1;
            } // f2d → double(cat-2)
            0x8e => {
                pop!();
                push!(1);
                pc += 1;
            } // d2i → int   (cat-1)
            0x8f => {
                pop!();
                push!(2);
                pc += 1;
            } // d2l → long  (cat-2)
            0x90 => {
                pop!();
                push!(1);
                pc += 1;
            } // d2f → float (cat-1)
            0x91..=0x93 => {
                pop!();
                push!(1);
                pc += 1;
            } // i2b, i2c, i2s (cat-1)

            // --- comparisons → push int (cat-1). Each pops two operands;
            // cat-2 operands (lcmp/dcmp) are a single entry each, so popping
            // two entries is correct for both cat-1 and cat-2 forms. ---
            0x94 => {
                pop!();
                pop!();
                push!(1);
                pc += 1;
            } // lcmp  (long, long)
            0x95 | 0x96 => {
                pop!();
                pop!();
                push!(1);
                pc += 1;
            } // fcmpl/fcmpg
            0x97 | 0x98 => {
                pop!();
                pop!();
                push!(1);
                pc += 1;
            } // dcmpl/dcmpg

            // --- control flow: per JVMS, stack height/width is invariant at
            // each PC across edges, so a linear walk stays consistent. Pop the
            // branch operands; provenance/width of the rest is preserved. ---
            0x99..=0x9e | 0xc6 | 0xc7 => {
                pop!();
                pc += 3;
            } // if<cond>, ifnull/nonnull
            0x9f..=0xa6 => {
                pop!();
                pop!();
                pc += 3;
            } // if_icmp/if_acmp
            0xa7 => {
                pc += 3;
            } // goto
            0xa8 => {
                push!(1);
                pc += 3;
            } // jsr (pushes returnAddress)
            0xa9 => {
                pc += 2;
            } // ret

            // returns / athrow — terminate this straight-line run; the abstract
            // stack is reset implicitly because the next PC begins a new block
            // (a branch target). Pop the returned value where applicable.
            0xac | 0xae | 0xb0 => {
                pop!();
                widths.clear();
                pc += 1;
            } // ireturn/freturn/areturn
            0xad | 0xaf => {
                pop!();
                widths.clear();
                pc += 1;
            } // lreturn/dreturn (one entry)
            0xb1 => {
                widths.clear();
                pc += 1;
            } // return
            0xbf => {
                widths.clear();
                pc += 1;
            } // athrow

            // switches — pop the int key; rest invariant.
            0xaa => {
                pop!();
                pc += bytecode_len_at(code, pc);
            }
            0xab => {
                pop!();
                pc += bytecode_len_at(code, pc);
            }

            // --- field / method ops: the width effect depends on the CP
            // descriptor, which this standalone helper does not parse. Rather
            // than reject (which would needlessly forgo JITting any method that
            // mixes field/call ops with a `dup2`), CLEAR the abstract model and
            // continue. A `dup2` whose top is a *direct* call/field result is
            // exceptionally rare in real bytecode (results are stored, not
            // dup2'd), and a cleared model makes such a dup2 default to FORM-1
            // — exactly the codegen's existing behavior. This preserves JIT
            // coverage while still catching the common, locally-detectable
            // FORM-2 dup2 (e.g. `lload; dup2`, `ladd; dup2`).
            0xb2 | 0xb3 | 0xb4 | 0xb5 | 0xb6 | 0xb7 | 0xb8 | 0xb9 | 0xba => {
                widths.clear();
                pc += bytecode_len_at(code, pc);
            }

            // new / checkcast / instanceof / monitor / nop / iinc / arrays.
            0x00 => {
                pc += 1;
            } // nop
            0x84 => {
                pc += 3;
            } // iinc (no stack effect)
            0xbb => {
                push!(1);
                pc += 3;
            } // new → objectref (cat-1)
            0xbc => {
                pop!();
                push!(1);
                pc += 2;
            } // newarray
            0xbd => {
                pop!();
                push!(1);
                pc += 3;
            } // anewarray
            0xbe => {
                pop!();
                push!(1);
                pc += 1;
            } // arraylength → int
            0xc0 => {
                pc += 3;
            } // checkcast (width unchanged)
            0xc1 => {
                pop!();
                push!(1);
                pc += 3;
            } // instanceof → int
            0xc2 | 0xc3 => {
                pop!();
                pc += 1;
            } // monitorenter/exit
            // wide (0xc4), multianewarray (0xc5), goto_w/jsr_w (0xc8/0xc9), and
            // any other unmodeled opcode: clear the model and advance by the
            // correct instruction length. Clearing keeps us sound (a later
            // dup2 defaults to FORM-1 = codegen behavior) without rejecting.
            _ => {
                widths.clear();
                pc += bytecode_len_at(code, pc);
            }
        }
    }
    true
}

// ═══════════════════════════════════════════════════════════════════════
// Loop transforms — peeling and unrolling (bytecode → bytecode)
// ═══════════════════════════════════════════════════════════════════════
//
// ## The transform
//
// A bytecode-to-bytecode rewriter that duplicates a natural loop's body,
// plus the analyses that decide whether duplicating it is legal at all.
// Peeling and unrolling share one rewriter because the two outputs are
// byte-identical except for the back edge's target:
//
// ```text
//     original          peel(k)                unroll(k)
//     ────────          ───────                ─────────
//     H: body           H: body    (copy 0)    H: body    (copy 0)
//        goto H            …                      …
//                          body    (copy k-1)     body    (copy k-1)
//                       S: body    (copy k)    S: body    (copy k)
//                          goto S                 goto H
// ```
//
// Peel's back edge targets the LAST copy, so copies `0..k-1` run exactly
// once and the steady-state loop is copy `k`. Unroll's targets the FIRST,
// so all `k+1` copies run on every trip. Nothing else differs — same byte
// layout, same provenance map, same refusal set.
//
// Every copy carries the body's own exit branches, so the transform needs
// **no trip-count precondition**: a copy whose exit test fires leaves the
// loop from the middle of the unrolled group exactly as the original would
// have left it from the middle of the sequence. Trip counts 0 and 1 are not
// special cases — with trip 0 the first copy's exit test fires before any
// body effect, which is the same instruction the original would have run.
//
// ## Preconditions (each is a refusal, never a guess)
//
//  1. The back edge is an unconditional `goto` whose target is the header
//     ([`LoopXformRefusal::ConditionalBackEdge`] / `NotABackEdge`). A
//     `do { } while` back edge is a conditional whose test would have to be
//     replicated at the end of every copy; that is a different rewriter, and
//     duplicating a `do`-body *without* its test would run the body `k+1`
//     times per test. The existing x86-64 byte-copy unroller has the same
//     precondition (`x64.rs`: `if code[back_edge] != 0xa7 { return None }`).
//  2. The header dominates every instruction in `[header, back_edge_end)`
//     ([`LoopXformRefusal::Irreducible`]). This is the textbook reducibility
//     condition for this loop, and it subsumes the "branch from outside into
//     the middle of the body" shape that
//     [`find_bypassable_loop_headers`] was written for: an entry that skips
//     the header is exactly an instruction the header does not dominate.
//     An explicit edge scan reports that case as `ExternalEntry` first,
//     purely for a better diagnostic.
//  3. Every cycle strictly inside the body is itself reducible
//     ([`LoopXformRefusal::IrreducibleInnerLoop`]). Duplication would in fact
//     preserve an irreducible inner cycle's semantics — the copy is a
//     relabelling — but every downstream consumer (loop detection, LICM,
//     OSR entry selection) assumes reducibility, so we refuse rather than
//     hand them a second irreducible nest.
//  4. Every instruction in the region is reachable from method entry
//     ([`LoopXformRefusal::UnreachableInRegion`]) — an unreachable
//     instruction has no dominator and cannot be reasoned about.
//  5. No `jsr` / `ret` / `jsr_w` / `goto_w` anywhere in the method
//     ([`LoopXformRefusal::OpaqueControlFlow`]): the first two have
//     successors that are not statically known, and `goto_w` is a backward
//     branch the emitter does **not** poll (see the poll argument below).
//  6. Every `tableswitch` / `lookupswitch` in the method survives the shift
//     byte-identically ([`LoopXformRefusal::SwitchInMethod`]). Two things
//     about a switch are PC-relative and neither is re-encoded here:
//
//       * its operands are aligned to the METHOD's code base (JVMS §6.5), so
//         moving it can change its *length*, not merely its offsets; and
//       * its jump offsets are 4-byte fields, and the rewriter rewrites
//         2-byte branch offsets only (`0x99..=0xa7 | 0xc6 | 0xc7`).
//
//     So the rule is not "no switch anywhere" but "no switch the rewrite
//     would disturb". A switch is admitted exactly when
//
//       * it is not inside the region — a switch there is DUPLICATED, and the
//         copies sit at different alignments and would each need their own
//         relocated targets;
//       * `switch_pad(shift(pc)) == switch_pad(pc)` — the padding recomputed
//         at the shifted PC is unchanged. A switch before the header does not
//         move, so this is free; one after the region moves by `delta`, so it
//         holds iff `delta % 4 == 0`; and
//       * every target keeps its displacement:
//         `shift(t) - shift(pc) == t - pc`, so the un-rewritten 4-byte offset
//         still names the same instruction.
//
//     A switch entirely before the loop that branches only before the loop
//     (or to the header, which does not move) is therefore admitted, where
//     the earlier rule refused the whole method for it. Re-padding and
//     re-encoding the 4-byte offsets — which would admit the rest, including
//     switches inside the body — is still a separate change: it would change
//     the output's *length*, which every other computation here (`out_len`,
//     the span table, `bci_of`) takes to be `code_len + delta`.
//  7. No branch inside the region targets the back-edge instruction itself
//     ([`LoopXformRefusal::BranchToBackEdge`]) — the back edge exists in the
//     last copy only, so such an edge has no image in copies `0..k-1`.
//     (A `continue` written as a branch to the *header* is fine and is
//     relocated to the next copy; that is javac's `while`-loop shape.)
//  8. No exception handler lands in the region, and no protected range
//     partially overlaps it (`HandlerInRegion` /
//     `HandlerRangeStraddlesRegion`). A range that *encloses* the region is
//     fine and is widened to cover the copies.
//  9. Every rewritten branch offset still fits the 2-byte signed field
//     ([`LoopXformRefusal::OffsetOverflow`]). We refuse rather than widen to
//     `goto_w`, which would silently drop a safepoint poll (see below).
//
// ## Safepoint polls — why they survive
//
// The x86-64 emitter's rule is uniform and PC-local: at `ifeq..if_acmpne`
// (`0x99..=0xa6`), `goto` (`0xa7`), `tableswitch`/`lookupswitch`
// (`0xaa`/`0xab`) and `ifnull`/`ifnonnull` (`0xc6`/`0xc7`), if any decoded
// target is `<= pc` it emits [`emit_safepoint_poll`] *before* the compare and
// branch, so the poll runs whether or not the branch is taken.
// [`poll_bearing_opcode`] is that opcode set, transcribed. `goto_w`/`jsr_w`
// are absent from it — they are rejected by `jit_scan` today, and a rewriter
// that introduced one would introduce an unpolled backward branch.
//
// That gives a structural theorem:
//
// > In a linear bytecode CFG every edge is either a fall-through or a branch,
// > and every fall-through strictly increases the PC. So every cycle contains
// > at least one edge whose target is `<= ` its source, i.e. at least one
// > backward branch. If every backward branch in the method sits at a
// > poll-bearing opcode, every cycle is polled.
//
// [`all_backward_edges_are_polled`] checks exactly that antecedent, and the
// rewriter runs it **on its own output** before returning: a transform that
// somehow produced a poll-free cycle refuses instead of publishing itself.
// The check is not vacuous — it fails on a backward `goto_w`, on `jsr`/`ret`,
// and on a malformed branch.
//
// Per transform:
//
// * **Peel** does not touch the loop: the steady-state copy still ends in the
//   same `goto`, so the poll happens once per trip exactly as before. The
//   peeled copies execute once each, ahead of the loop, and add
//   `k * body_len` bytecodes to the one-shot span between the method-entry
//   poll ([`emit_safepoint_poll_prologue`]) and the first back-edge poll.
// * **Unroll** keeps one back edge for `k+1` bodies, so the *steady-state*
//   time-to-safepoint grows by the unroll factor. It stays bounded because
//   both factors are bounded: `body_len <= LOOP_XFORM_MAX_BODY_BYTES` and
//   `k+1 <= LOOP_XFORM_MAX_COPIES + 1`, and the product is re-checked against
//   [`LOOP_XFORM_MAX_POLL_FREE_BYTES`] per call.
//
// `LoopXform::poll_free_bytes` records the worst-case poll-free span
// (`(k+1) * body_len`) for both, which is the steady-state figure for unroll
// and the one-shot prefix for peel. It is an upper bound on straight-line
// bytecodes, not a cycle count: a body containing an inner loop polls inside
// that inner loop too.
//
// ## Deopt, OSR and provenance
//
// `LoopXform::bci_of` maps **every byte** of the output to the original
// bytecode index it was copied from — the prefix and suffix map to
// themselves, and each copy of the body maps back to the one original body.
// Two facts make that enough to reconstruct an interpreter frame from any
// point in a transformed loop:
//
//  * the rewriter copies bytes and rewrites branch *offsets* only, so no
//    local index and no operand-stack shape is ever renamed or reordered;
//  * every copy is entered with the same abstract state the original body is
//    entered with, because the copies are laid out in execution order.
//
// So the interpreter state at output PC `p` equals the state the original
// method had at `bci_of[p]` on the corresponding iteration, and a deopt maps
// through `bci_of` with the frame already correct. The test
// `deopt_into_a_transformed_loop_resolves_its_locals` asserts the strong form
// of this: the *entire* `(bci, locals)` step sequence of the transformed run
// is equal to the original's.
//
// The reverse map is one-to-many, which is the OSR hazard: a bci inside the
// region has `k+1` images. [`LoopXform::osr_entry_pc`] returns the
// steady-state one — copy `k` for peel, copy `0` for unroll. Entering a
// *peeled* copy from OSR would re-run the peeled iterations and execute the
// loop `k` times too many. This is the same class of bug as the LICM
// pre-header bypass (an entry edge that lands on the wrong side of
// duplicated code), so it is answered here explicitly rather than left to
// the consumer.
//
// It is one-to-many for the body, but one-to-**zero** for the back edge under
// unroll: the back-edge `goto` is emitted in the LAST copy only, so its bytes
// have no image in copy `0`, which is unroll's steady state. `osr_entry_pc`
// answers `None` for `bci in back_edge..back_edge_end` when
// `kind == Unroll` — the alternative, `Some(bci)`, is not a conservative
// answer but a WRONG one: output pc `back_edge` is `header + body_len`, the
// first byte of copy 1, so the entry would resume the interpreter's "about to
// execute the back edge" frame at the top of a fresh body and run one whole
// iteration too many. Peel has no such gap: its steady-state copy is the last
// one, which carries the back edge, so every bci in the region round-trips.
// The consumer's rule is therefore uniform for both kinds and needs no
// special case: an OSR request whose `osr_entry_pc` is `None` is refused, and
// the interpreter keeps running until it reaches a bci that has one (the very
// next one it reaches is the header, which always does).
//
// ## Composition with LICM and the pre-header bypass fix
//
// The transform is a *source-to-source* rewrite that runs before loop
// detection, so `detect_loops`, `find_arith_loop_hoists`,
// `find_bypassable_loop_headers` and the speculative-BCE guards all re-run on
// the transformed bytecode and see a consistent CFG. Nothing here needs to
// know about hoist slots, and the pre-header bypass guard keeps working
// unchanged. Peeling in fact *removes* bypassability of the steady-state
// loop: after peel(k) the only edge into copy `k` is the fall-through from
// copy `k-1` and the back edge, both internal, so a hoist the guard had to
// drop before can be kept. `peeling_removes_the_preheader_bypass_from_the_
// steady_state_loop` pins that, and `x64.rs`'s
// `the_planner_peels_a_bypassable_header_instead_of_skipping_it` is the planner
// arm that acts on it. (This paragraph used to cite a
// `peeled_loop_is_not_bypassable` that was never written.)

/// Largest loop body (in bytecodes) either transform will duplicate.
#[allow(dead_code)]
pub(super) const LOOP_XFORM_MAX_BODY_BYTES: usize = 256;

/// Largest number of EXTRA body copies (`k`) either transform will make.
#[allow(dead_code)]
pub(super) const LOOP_XFORM_MAX_COPIES: usize = 7;

/// Time-to-safepoint budget: the largest poll-free straight-line span, in
/// bytecodes, a transform may leave behind. See the poll argument above.
#[allow(dead_code)]
pub(super) const LOOP_XFORM_MAX_POLL_FREE_BYTES: usize = 1024;

/// `true` when the x86-64 emitter emits a cooperative safepoint poll at an
/// instruction of this opcode *whose branch target is backward*.
///
/// Transcribed from the emitter's `if target_pc <= pc { self.emit_safepoint_poll(); }`
/// sites in `x64.rs` — the `ifeq..ifle`, `if_icmpeq..if_icmple`,
/// `if_acmpeq/ne`, `goto`, `tableswitch`, `lookupswitch`, `ifnull` and
/// `ifnonnull` arms. `goto_w`/`jsr_w`/`jsr`/`ret` are deliberately absent:
/// the emitter has no poll for them (they are rejected by `jit_scan`), so a
/// backward one would be an unpolled cycle.
#[allow(dead_code)]
pub(super) fn poll_bearing_opcode(op: u8) -> bool {
    matches!(op, 0x99..=0xa7 | 0xaa | 0xab | 0xc6 | 0xc7)
}

/// `true` when an opcode's fall-through successor exists (i.e. control can
/// reach the next instruction in linear order).
pub(super) fn opcode_falls_through(op: u8) -> bool {
    !matches!(op, 0xa7 | 0xa9 | 0xaa | 0xab | 0xac..=0xb1 | 0xbf | 0xc8)
}

/// Decode the explicit branch targets of the instruction at `pc` into `out`
/// (the fall-through successor is NOT included).
///
/// Returns `false` — meaning *refuse* — when the successor set is not
/// statically known (`jsr`/`jsr_w`/`ret`) or the encoding is malformed or
/// points outside `[0, code_len)`. Callers must treat `false` as opaque, not
/// as "no targets": guessing here is how a transform loses an edge.
pub(super) fn branch_targets_at(
    code: &[u8],
    pc: usize,
    code_len: usize,
    out: &mut Vec<usize>,
) -> bool {
    if code_len > code.len() || pc >= code_len {
        return false;
    }
    // Cast: pc/target to isize for signed branch-displacement arithmetic.
    let push_t = |off: isize, out: &mut Vec<usize>| -> bool {
        let t = pc as isize + off;
        if t < 0 || t as usize >= code_len {
            return false;
        }
        out.push(t as usize); // Cast: non-negative index to usize
        true
    };
    match code[pc] {
        // `jsr`/`jsr_w` push a return address that a `ret` later consumes out
        // of a local: neither end of that pair has statically known
        // successors here.
        0xa8 | 0xa9 | 0xc9 => false,
        0x99..=0xa7 | 0xc6 | 0xc7 => {
            if pc + 2 >= code_len {
                return false;
            }
            // Cast: signed branch displacement to isize
            let off = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as isize;
            push_t(off, out)
        }
        0xc8 => {
            if pc + 4 >= code_len {
                return false;
            }
            let off =
                i32::from_be_bytes([code[pc + 1], code[pc + 2], code[pc + 3], code[pc + 4]])
                    // Cast: signed branch displacement to isize
                    as isize;
            push_t(off, out)
        }
        0xaa => {
            let mut p = pc + 1;
            while p % 4 != 0 {
                p += 1;
            }
            if p + 12 > code_len {
                return false;
            }
            let rd = |at: usize| -> isize {
                // Cast: signed branch displacement to isize
                i32::from_be_bytes([code[at], code[at + 1], code[at + 2], code[at + 3]]) as isize
            };
            if !push_t(rd(p), &mut *out) {
                return false;
            }
            // Cast: table bound to i32
            let low = rd(p + 4) as i32;
            // Cast: table bound to i32
            let high = rd(p + 8) as i32;
            let count = match checked_tableswitch_count(low, high) {
                Some(c) => c,
                None => return false,
            };
            let mut jp = p + 12;
            for _ in 0..count {
                if jp + 4 > code_len {
                    return false;
                }
                if !push_t(rd(jp), &mut *out) {
                    return false;
                }
                jp += 4;
            }
            true
        }
        0xab => {
            let mut p = pc + 1;
            while p % 4 != 0 {
                p += 1;
            }
            if p + 8 > code_len {
                return false;
            }
            let rd = |at: usize| -> isize {
                // Cast: signed branch displacement to isize
                i32::from_be_bytes([code[at], code[at + 1], code[at + 2], code[at + 3]]) as isize
            };
            if !push_t(rd(p), &mut *out) {
                return false;
            }
            let npairs = i32::from_be_bytes([code[p + 4], code[p + 5], code[p + 6], code[p + 7]]);
            if npairs < 0 {
                return false;
            }
            // Cast: non-negative count to usize
            let npairs = npairs as usize;
            let mut jp = p + 8;
            for _ in 0..npairs {
                if jp + 8 > code_len {
                    return false;
                }
                // A pair is (match:i32, offset:i32); the offset is at jp + 4.
                if !push_t(rd(jp + 4), &mut *out) {
                    return false;
                }
                jp += 8;
            }
            true
        }
        _ => true,
    }
}

/// Bytecode PCs reachable from the method entry along ORDINARY control flow —
/// fall-through plus explicit branch/switch edges.
///
/// Exception-table handler entries are deliberately NOT roots. The x86-64
/// backend has no in-method handler dispatch: an implicit exception leaves
/// through the `i64::MIN` sentinel and `athrow` lowers to the same, so a
/// compiled body is only ever resumed at one of its own handlers by the
/// interpreter (`route_jit_signal_exception` / `run_jit_callee_handler` both
/// rebuild an interpreter frame for it). Every handler body is therefore dead
/// code in the emitted image, and this map says so.
///
/// The optimizing tier settled the same question first and for the same
/// reason: `ir::IrBuilder::build` skips every PC outside
/// `ir::normally_reachable_pcs`, which is this walk over the verifier's CFG.
/// Walking handler bodies there produced orphan nodes; walking them here
/// produced a revived merge with no operand stack. Two tiers, one contract.
///
/// Returns `None` — "refuse, do not guess" — when any instruction's successor
/// set is not statically known (`jsr` / `ret` / `jsr_w`) or an encoding is
/// malformed. Callers must then keep whatever conservative behaviour they had.
///
/// Sized `code_len + 1` to match the emitter's own `branch_targets` map, so the
/// two are indexed by the same `pc`.
pub(super) fn compute_reachable_pcs(code: &[u8], code_len: usize) -> Option<Vec<bool>> {
    compute_reachable_pcs_with_roots(code, code_len, &[])
}

/// [`compute_reachable_pcs`] with extra entry points.
///
/// The only producer of extras is compiled local exception handlers: an
/// exception edge is a real predecessor that no branch instruction names, so a
/// handler body reachable ONLY that way is invisible to the walk above and
/// stays dead. Passing its `handler_pc` as a root makes the block — and
/// everything it reaches — live code, which is exactly the change from "a
/// handler body is dead code in the emitted image" to "this method runs its own
/// `catch`". An empty slice is byte-for-byte [`compute_reachable_pcs`].
pub(super) fn compute_reachable_pcs_with_roots(
    code: &[u8],
    code_len: usize,
    extra_roots: &[usize],
) -> Option<Vec<bool>> {
    if code_len > code.len() {
        return None;
    }
    let mut reachable = vec![false; code_len + 1];
    if code_len == 0 {
        return Some(reachable);
    }
    reachable[0] = true;
    let mut work = vec![0usize];
    for &root in extra_roots {
        if root < code_len && !reachable[root] {
            reachable[root] = true;
            work.push(root);
        }
    }
    let mut targets: Vec<usize> = Vec::new();
    while let Some(pc) = work.pop() {
        // A branch INTO the middle of an instruction decodes garbage from here
        // on. That cannot corrupt what the emitter reads (it only ever indexes
        // this map at real instruction boundaries) and the walk stays bounded
        // by `code_len`; such a method is rejected afterwards by
        // `patch_branches`, which finds the target has no native offset.
        let len = bytecode_len_at(code, pc).max(1);
        targets.clear();
        if !branch_targets_at(code, pc, code_len, &mut targets) {
            return None;
        }
        for &t in &targets {
            if !reachable[t] {
                reachable[t] = true;
                work.push(t);
            }
        }
        if opcode_falls_through(code[pc]) {
            let next = pc + len;
            if next < code_len && !reachable[next] {
                reachable[next] = true;
                work.push(next);
            }
        }
    }
    Some(reachable)
}

/// `true` when the emitter will emit a cooperative safepoint poll at `pc`.
#[allow(dead_code)]
pub(super) fn emits_safepoint_poll_at(code: &[u8], pc: usize, code_len: usize) -> bool {
    let op = match code.get(pc) {
        Some(&o) => o,
        None => return false,
    };
    if !poll_bearing_opcode(op) {
        return false;
    }
    let mut targets: Vec<usize> = Vec::new();
    if !branch_targets_at(code, pc, code_len, &mut targets) {
        return false;
    }
    targets.iter().any(|&t| t <= pc)
}

/// `true` when EVERY backward branch in the method sits at an opcode the
/// emitter polls — which, since every cycle in a linear bytecode CFG contains
/// a backward branch, proves every cycle is polled.
///
/// Returns `false` (i.e. "not proven") on opaque or malformed control flow
/// and on a backward `goto_w`, which the emitter does not poll.
#[allow(dead_code)]
pub(super) fn all_backward_edges_are_polled(code: &[u8], code_len: usize) -> bool {
    if code_len > code.len() {
        return false;
    }
    let mut targets: Vec<usize> = Vec::new();
    let mut pc = 0usize;
    while pc < code_len {
        targets.clear();
        if !branch_targets_at(code, pc, code_len, &mut targets) {
            return false;
        }
        if targets.iter().any(|&t| t <= pc) && !poll_bearing_opcode(code[pc]) {
            return false;
        }
        let len = bytecode_len_at(code, pc);
        if len == 0 {
            return false;
        }
        pc += len;
    }
    true
}

/// Sentinel for "no such node" / "unreachable" in [`MethodCfg`].
const CFG_NONE: usize = usize::MAX;

/// Instruction-granularity control-flow graph of one method, with immediate
/// dominators.
///
/// Built only to answer the questions a loop transform must not guess at: is
/// this region single-entry, is the loop reducible, is every instruction in
/// it reachable. Nodes are instruction start PCs in ascending order; node `0`
/// is the method entry.
#[allow(dead_code)]
pub(super) struct MethodCfg {
    /// Instruction start PCs, ascending. Node `i` is `pcs[i]`.
    pcs: Vec<usize>,
    /// `pc` → node index, [`CFG_NONE`] when `pc` is not an instruction start.
    idx_of: Vec<usize>,
    /// Reverse-post-order number, [`CFG_NONE`] when unreachable from entry.
    rpo_num: Vec<usize>,
    /// Immediate dominator node index, [`CFG_NONE`] when unknown/unreachable.
    idom: Vec<usize>,
}

/// Cooper/Harvey/Kennedy `intersect`: walk two dominator-tree paths up until
/// they meet. Returns [`CFG_NONE`] if either chain is incomplete — the caller
/// then leaves the dominator unknown, which every query treats as "does not
/// dominate" (conservative: it can only cause a refusal).
fn dom_intersect(idom: &[usize], rpo_num: &[usize], a0: usize, b0: usize) -> usize {
    let (mut a, mut b) = (a0, b0);
    let limit = idom.len().saturating_mul(2).saturating_add(8);
    let mut steps = 0usize;
    while a != b {
        steps += 1;
        if steps > limit || a >= rpo_num.len() || b >= rpo_num.len() {
            return CFG_NONE;
        }
        let (ra, rb) = (rpo_num[a], rpo_num[b]);
        if ra == CFG_NONE || rb == CFG_NONE {
            return CFG_NONE;
        }
        // Reverse-post-order numbers are unique, so `a != b` implies
        // `ra != rb` and each step strictly decreases `max(ra, rb)`.
        if ra > rb {
            let na = idom[a];
            if na == CFG_NONE || na == a {
                return CFG_NONE;
            }
            a = na;
        } else {
            let nb = idom[b];
            if nb == CFG_NONE || nb == b {
                return CFG_NONE;
            }
            b = nb;
        }
    }
    a
}

#[allow(dead_code)]
impl MethodCfg {
    /// Build the CFG, or `None` when the bytecode cannot be walked exactly
    /// (a length-table desync), a branch target is not an instruction
    /// boundary, control flow is opaque (`jsr`/`ret`), or the dominator
    /// fixpoint did not settle. Every `None` is a refusal.
    pub(super) fn build(code: &[u8], code_len: usize) -> Option<MethodCfg> {
        if code_len == 0 || code_len > code.len() {
            return None;
        }
        // Instruction starts, from the same forward walk every other
        // PC-stepping consumer uses (see `instruction_start_map`).
        let mut pcs: Vec<usize> = Vec::new();
        let mut idx_of: Vec<usize> = vec![CFG_NONE; code_len + 1];
        let mut pc = 0usize;
        while pc < code_len {
            idx_of[pc] = pcs.len();
            pcs.push(pc);
            let len = bytecode_len_at(code, pc);
            if len == 0 {
                return None;
            }
            pc += len;
        }
        if pc != code_len {
            // The walk stepped past the end: the length table and this code
            // disagree, so nothing below can be trusted.
            return None;
        }

        let n = pcs.len();
        let mut succs: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut targets: Vec<usize> = Vec::new();
        for i in 0..n {
            let at = pcs[i];
            targets.clear();
            if !branch_targets_at(code, at, code_len, &mut targets) {
                return None;
            }
            for &t in &targets {
                let ti = idx_of[t];
                if ti == CFG_NONE {
                    return None; // target lands mid-instruction
                }
                succs[i].push(ti);
            }
            if opcode_falls_through(code[at]) {
                let nxt = at + bytecode_len_at(code, at);
                if nxt < code_len {
                    let ni = idx_of[nxt];
                    if ni == CFG_NONE {
                        return None;
                    }
                    succs[i].push(ni);
                }
            }
        }
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
        for i in 0..n {
            for &s in &succs[i] {
                preds[s].push(i);
            }
        }

        // Reverse post-order from the entry node, iteratively (no recursion:
        // a deep method must not blow the compiler thread's stack).
        let mut visited = vec![false; n];
        let mut post: Vec<usize> = Vec::with_capacity(n);
        let mut stack: Vec<(usize, usize)> = Vec::new();
        visited[0] = true;
        stack.push((0, 0));
        while let Some((node, ci)) = stack.pop() {
            if ci < succs[node].len() {
                stack.push((node, ci + 1));
                let s = succs[node][ci];
                if !visited[s] {
                    visited[s] = true;
                    stack.push((s, 0));
                }
            } else {
                post.push(node);
            }
        }
        let mut rpo_num = vec![CFG_NONE; n];
        let mut order: Vec<usize> = Vec::with_capacity(post.len());
        for (k, &node) in post.iter().rev().enumerate() {
            rpo_num[node] = k;
            order.push(node);
        }
        if order.first().copied() != Some(0) {
            return None; // entry must be first in reverse post-order
        }

        // Cooper/Harvey/Kennedy iterative dominators. Capped so a malformed
        // graph refuses instead of spinning on the JIT thread.
        let mut idom = vec![CFG_NONE; n];
        idom[0] = 0;
        let mut settled = false;
        for _ in 0..(n + 2) {
            let mut changed = false;
            for &b in order.iter().skip(1) {
                let mut new_idom = CFG_NONE;
                for &p in &preds[b] {
                    if rpo_num[p] == CFG_NONE || idom[p] == CFG_NONE {
                        continue; // unreachable or not yet processed
                    }
                    new_idom = if new_idom == CFG_NONE {
                        p
                    } else {
                        dom_intersect(&idom, &rpo_num, p, new_idom)
                    };
                    if new_idom == CFG_NONE {
                        break;
                    }
                }
                if new_idom != CFG_NONE && idom[b] != new_idom {
                    idom[b] = new_idom;
                    changed = true;
                }
            }
            if !changed {
                settled = true;
                break;
            }
        }
        if !settled {
            return None;
        }

        Some(MethodCfg {
            pcs,
            idx_of,
            rpo_num,
            idom,
        })
    }

    /// Instruction start PCs, ascending.
    pub(super) fn nodes(&self) -> &[usize] {
        &self.pcs
    }

    /// Node index for an instruction start PC.
    pub(super) fn node_of(&self, pc: usize) -> Option<usize> {
        match self.idx_of.get(pc).copied() {
            Some(i) if i != CFG_NONE => Some(i),
            _ => None,
        }
    }

    /// `true` when `node` is reachable from method entry.
    pub(super) fn is_reachable(&self, node: usize) -> bool {
        self.rpo_num.get(node).copied().unwrap_or(CFG_NONE) != CFG_NONE
    }

    /// `true` when `a` dominates `b` (every path from entry to `b` passes
    /// through `a`). Unknown/unreachable answers `false`, so a caller that
    /// requires domination refuses rather than assuming it.
    pub(super) fn dominates(&self, a: usize, b: usize) -> bool {
        if a >= self.idom.len() || b >= self.idom.len() {
            return false;
        }
        if !self.is_reachable(a) || !self.is_reachable(b) {
            return false;
        }
        let mut cur = b;
        let mut steps = 0usize;
        let limit = self.idom.len() + 8;
        loop {
            if cur == a {
                return true;
            }
            steps += 1;
            if steps > limit {
                return false;
            }
            let nxt = self.idom[cur];
            if nxt == CFG_NONE || nxt == cur {
                return false; // reached the entry without meeting `a`
            }
            cur = nxt;
        }
    }
}

/// Which loop transform produced a [`LoopXform`]. See the section header for
/// the layout diagram — the two differ only in the back edge's target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(super) enum LoopXformKind {
    /// `k` copies of the body run once each, ahead of the loop.
    Peel,
    /// `k` extra copies of the body run inside the loop, on every trip.
    Unroll,
}

/// Why a loop transform refused.
///
/// Every variant is a REFUSAL — the caller keeps the original bytecode and
/// loses an optimisation. None of them is a "best effort" path: a transform
/// that cannot prove its precondition must not guess, because every guess
/// here is either a wrong loop bound, a lost interpreter local, or an
/// unpolled cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
// `pub(crate)`, not `pub(super)`: `x64::LoopRewriteRefusal::Planner` carries one
// of these and is itself reachable at crate visibility, so the narrower spelling
// made the variant's field more private than the item holding it.
pub(crate) enum LoopXformRefusal {
    /// Malformed or out-of-range inputs (bad PCs, truncated branch, oversized
    /// method), or an internal invariant that did not hold.
    BadShape,
    /// The instruction at `back_edge` does not branch to `header`.
    NotABackEdge,
    /// The back edge is conditional (`do { } while`): its test would have to
    /// be replicated at the end of every copy.
    ConditionalBackEdge,
    /// Body larger than [`LOOP_XFORM_MAX_BODY_BYTES`].
    BodyTooLarge,
    /// `k` is zero or larger than [`LOOP_XFORM_MAX_COPIES`].
    TooManyCopies,
    /// `jsr`/`ret`/`jsr_w`/`goto_w`, a mid-instruction branch target, or a
    /// bytecode walk that did not land on the method's end.
    OpaqueControlFlow,
    /// The method contains a `tableswitch`/`lookupswitch`: shifting code
    /// changes their 4-byte operand padding, hence their length.
    SwitchInMethod,
    /// A branch from outside the region targets the body below the header —
    /// the pre-header-bypass shape (see [`find_bypassable_loop_headers`]).
    ExternalEntry,
    /// The header does not dominate its own region: the loop is irreducible.
    Irreducible,
    /// A cycle strictly inside the body is irreducible.
    IrreducibleInnerLoop,
    /// An instruction in the region is unreachable from method entry.
    UnreachableInRegion,
    /// A branch targets the back-edge instruction, which exists in the last
    /// copy only.
    BranchToBackEdge,
    /// An exception handler lands inside the region.
    HandlerInRegion,
    /// A protected range partially overlaps the region.
    HandlerRangeStraddlesRegion,
    /// A rewritten branch no longer fits the 2-byte signed offset field. We
    /// refuse rather than widen to `goto_w`, which the emitter does not poll.
    OffsetOverflow,
    /// The rewritten code has a backward branch at an opcode the emitter does
    /// not poll, or the steady-state back edge lost its poll. Defensive: this
    /// should be unreachable, and is checked on the emitted bytes anyway.
    UnpolledBackEdge,
    /// The transform would leave a poll-free span longer than
    /// [`LOOP_XFORM_MAX_POLL_FREE_BYTES`] bytecodes.
    TimeToSafepointBudget,
    /// A versioning guard this rewriter cannot emit as straight-line bytecode:
    /// a term that is not an `int` local or a constant, a term whose evaluation
    /// could throw (`arraylength`, a field read), a local index that would need
    /// `wide`, a threshold too large for `sipush` (there is no constant pool to
    /// mint an `ldc` in), or a guard that is two comparisons rather than one.
    /// See [`encode_preheader_guard`].
    GuardNotEncodable,
    /// The versioning guard is decided at compile time. Both verdicts are
    /// refusals: an always-true guard means the caller wants the plain
    /// transform (a runtime compare would test a known fact), and an
    /// always-false one means the guarded version is dead code.
    GuardIsConstant,
}

/// The extra structure a GUARDED VERSIONING rewrite adds to a [`LoopXform`].
///
/// Versioning emits a pre-header check and lays down TWO images of the loop:
/// the transformed one on the guarded path and an untouched copy of the
/// original on the fallback path.
///
/// ```text
///     original            version(guard, unroll(k))
///     ────────            ─────────────────────────
///     H: body             G: <guard>   ──(fails)──┐
///        goto H           F: body      (copy 0)   │
///                            …                    │
///                            body     (copy k)    │
///                            goto F               │
///                         B: body     ◀───────────┘
///                            goto B
/// ```
///
/// The guard is straight-line bytecode ending in ONE conditional branch to
/// `fallback_base`, taken exactly when the guard FAILS. Nothing falls into `B`
/// from above: the region always ends in an unconditional `goto` (the back
/// edge, checked before anything is emitted), so the fallback copy is reachable
/// only through the guard's branch and its own back edge.
///
/// Four properties make this sound rather than merely plausible, and each is
/// pinned by a test:
///
/// * **The guard cannot throw, allocate, call, poll or write.** Only `iload`,
///   an integer constant push and one `if_icmp*` are ever emitted —
///   [`encode_preheader_guard`] refuses every other shape — so none of the four
///   sites that bake a bci into machine code (`Compiler::orig_bci`'s callers)
///   can fire at a guard PC, no safepoint map is recorded there, and the
///   sequence is stack-balanced, so the frame at the guard's first byte is the
///   frame the interpreter has at the header.
/// * **The guard's bytes carry the header's bci but are not an IMAGE of it.**
///   Provenance must stay total, so they map to the header; but
///   [`LoopXform::outputs_for_bci`] skips them, so replicating a pc-keyed side
///   table never lands a field resolution or an inline cache on synthetic
///   bytecode.
/// * **OSR enters the fallback, and never anything else.** Not the transformed
///   copies — entering one skips the guard, which is the whole point of having
///   one — and not the guard either, even at the header, where re-evaluating it
///   would be correct at the bytecode level. An OSR entry is only valid at a pc
///   whose compiled state the entry trampoline can reconstruct from the
///   interpreter frame, and the emitter publishes that state at loop headers;
///   the guard sits in the prologue's straight-line code. See
///   [`LoopXform::osr_entry_pc`], which carries the failure that taught this.
/// * **Both loops still poll.** The fast path's back edge and the fallback's are
///   each checked with [`emits_safepoint_poll_at`] on the emitted bytes.
///
/// The guard's *meaning* is the caller's business. For peel and unroll it is a
/// profitability filter only — both transforms are legal at every trip count
/// (each copy keeps the body's own exit branches), so a guard that is
/// pessimistic or optimistic costs speed and nothing else. A future transform
/// that is legal only above a minimum must supply a guard that is sound for
/// that claim; this rewriter emits what it is given and proves only that the
/// fast path is unreachable when the guard fails.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(super) struct LoopVersioning {
    /// The obligation the emitted pre-header discharges.
    pub(super) guard: PreheaderGuard,
    /// Output PC of the guard's first byte. Equal to the original header PC —
    /// the guard is inserted exactly where the loop used to start, so every
    /// edge that reached the loop now reaches the guard.
    pub(super) guard_pc: usize,
    /// Length of the emitted guard, in bytecodes.
    pub(super) guard_len: usize,
    /// Output PC of the fallback copy: an untouched image of the original
    /// region, back edge included.
    pub(super) fallback_base: usize,
}

/// A transformed method: rewritten bytecode plus everything a consumer needs
/// to keep deopt, OSR and exception dispatch correct across the rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(super) struct LoopXform {
    /// Which transform produced this.
    pub(super) kind: LoopXformKind,
    /// The rewritten bytecode.
    pub(super) code: Vec<u8>,
    /// `code.len()` — the transformed method's code length.
    pub(super) code_len: usize,
    /// Provenance: `bci_of[p]` is the ORIGINAL bytecode index byte `p` was
    /// copied from. Total — every byte has one — so a deopt at any output PC
    /// resolves to a real bci with the frame already correct.
    pub(super) bci_of: Vec<u32>,
    /// Exception ranges rewritten into output coordinates. A range enclosing
    /// the loop is widened to cover the copies.
    pub(super) exception_ranges: Vec<(usize, usize, usize)>,
    /// Loop header PC — unchanged by the rewrite (the prefix does not move).
    pub(super) header: usize,
    /// Body length in bytes (header .. back edge, excluding the back edge).
    pub(super) body_len: usize,
    /// Number of EXTRA body copies (`k`).
    pub(super) copies: usize,
    /// Output PC the back edge targets: copy `k` for peel, copy `0` for
    /// unroll.
    pub(super) loop_entry: usize,
    /// Worst-case poll-free straight-line span, in bytecodes.
    pub(super) poll_free_bytes: usize,
    /// Original `code_len`, for provenance range checks.
    orig_code_len: usize,
    /// Original PC just past the back-edge instruction.
    orig_back_edge_end: usize,
    /// Present exactly when this is a guarded VERSIONING rewrite. `None` for a
    /// plain peel or unroll, and every accessor below then behaves exactly as
    /// it did before versioning existed.
    pub(super) versioning: Option<LoopVersioning>,
    /// What an original PC at or past `orig_back_edge_end` shifts by:
    /// `guard_len + copies * body_len + fallback_len`. Equal to
    /// `copies * body_len` without versioning.
    suffix_shift: usize,
}

#[allow(dead_code)]
impl LoopXform {
    /// Output PC of the STEADY-STATE loop's back-edge instruction — the one
    /// an OSR entry's loop will execute.
    ///
    /// Under versioning the steady state is the fallback copy, so this is the
    /// fallback's back edge; [`Self::fast_back_edge_pc`] is the transformed
    /// path's. Without versioning the two are the same instruction.
    pub(super) fn back_edge_pc(&self) -> usize {
        if let Some(v) = &self.versioning {
            return v.fallback_base + self.body_len;
        }
        self.fast_back_edge_pc()
    }

    /// Output PC of the TRANSFORMED loop's back-edge instruction.
    pub(super) fn fast_back_edge_pc(&self) -> usize {
        self.fast_base() + (self.copies + 1) * self.body_len
    }

    /// Output PC of the transformed region's first copy: the header, past the
    /// guard when there is one.
    pub(super) fn fast_base(&self) -> usize {
        self.header + self.versioning.as_ref().map_or(0, |v| v.guard_len)
    }

    /// `[start, end)` of the emitted pre-header guard, or `None` when this is
    /// not a versioning rewrite.
    pub(super) fn guard_span(&self) -> Option<(usize, usize)> {
        self.versioning
            .as_ref()
            .map(|v| (v.guard_pc, v.guard_pc + v.guard_len))
    }

    /// Output PC where the steady-state copy of the body begins — the copy
    /// the back edge re-enters, and the only copy an OSR entry may land in.
    ///
    /// Under versioning that is the FALLBACK copy: it is the one image of the
    /// region that is valid to enter without the guard having run.
    pub(super) fn steady_state_base(&self) -> usize {
        if let Some(v) = &self.versioning {
            return v.fallback_base;
        }
        match self.kind {
            LoopXformKind::Peel => self.fast_base() + self.copies * self.body_len,
            LoopXformKind::Unroll => self.fast_base(),
        }
    }

    /// Original bci for an output PC.
    pub(super) fn bci_at(&self, pc: usize) -> Option<usize> {
        // Widening: u32 -> usize
        self.bci_of.get(pc).map(|&b| b as usize)
    }

    /// Original PC of the back-edge instruction (`header + body_len`).
    pub(super) fn orig_back_edge_pc(&self) -> usize {
        self.header + self.body_len
    }

    /// Every output PC that is an image of original `bci`, ascending.
    ///
    /// The inverse of [`Self::bci_at`], one-to-many by construction: a peeled
    /// or unrolled body appears `copies + 1` times. This is what a side-table
    /// replication pass needs. `compile_with_param_slots` takes **21** tables
    /// keyed by bytecode PC (`field_info`, `typecheck_info`, `invoke_info`,
    /// `mic_slots`, `pic_slots`, `ldc_info`, `inline_sites`, `indy_info`, …),
    /// and compiling the rewritten bytes means every entry inside the region
    /// must appear once per copy. Replicating one table and missing another
    /// does not fail loudly — it yields a copy that silently loses a field
    /// resolution or an inline cache — so the mapping has to come from one
    /// place rather than being open-coded per table.
    ///
    /// Sharing a pointer-carrying entry (`mic_slots`, `pic_slots`,
    /// `invoke_info`) across copies is CORRECT rather than a compromise: an
    /// inline cache keyed on a call site sees the same receiver distribution in
    /// every copy, which is exactly what happens today when a non-unrolled loop
    /// executes many times. The copies share one slot; they do not need one
    /// each.
    ///
    /// Empty for a `bci` with no image — impossible for a PC inside the
    /// rewritten region, possible for one past the end.
    /// A versioning guard's bytes are deliberately NOT images: they carry the
    /// header's bci so provenance stays total, but no original instruction was
    /// copied there, and an entry replicated onto them would key a field
    /// resolution or an inline cache to a synthetic `iload`.
    pub(super) fn outputs_for_bci(&self, bci: usize) -> Vec<usize> {
        let guard = self.guard_span();
        let mut out = Vec::new();
        for (pc, &b) in self.bci_of.iter().enumerate() {
            if let Some((from, to)) = guard {
                if pc >= from && pc < to {
                    continue;
                }
            }
            if b as usize == bci {
                out.push(pc);
            }
        }
        out
    }

    /// Rewrite one pc-keyed side table into the rewritten bytecode's PC space.
    ///
    /// `compile_with_param_slots` takes 21 of these. Every one must go through
    /// THIS function when the rewritten bytes are compiled, because replicating
    /// some and not others is silent: the affected copy loses a field
    /// resolution or an inline cache and takes a different lowering path than
    /// its siblings, with nothing to fail on.
    ///
    /// An entry whose PC has several images is duplicated once per image and
    /// the payload is CLONED, so a table carrying a raw pointer (`mic_slots`,
    /// `pic_slots`, `invoke_info`, `ldc_string_info`, `typecheck_info`) shares
    /// one target across copies. That is correct rather than a compromise: an
    /// inline cache keyed on a call site sees the same receiver distribution in
    /// every copy, exactly as it does today when a non-unrolled loop runs many
    /// times.
    ///
    /// Entries outside the rewritten region carry through their single image,
    /// so a table needs no "before"/"after" special-casing. An entry whose PC
    /// has no image is dropped: its instruction is not in the output at all.
    ///
    /// Sorted by PC, which is what every consumer of these tables assumes.
    /// Rebuild a `pc_to_native`-shaped vector from output-PC space back into
    /// original-bytecode space.
    ///
    /// The emitter records `pc_to_native[pc] = buf.pos()` once per emitted PC,
    /// so compiling the rewritten bytes yields a vector indexed by OUTPUT pc.
    /// Its consumers — OSR entry and the deopt-by-bci lookup — index it by
    /// ORIGINAL bci, so it cannot simply be handed over.
    ///
    /// A bci with several images has several native offsets, and picking the
    /// wrong one re-runs iterations. [`Self::osr_entry_pc`] already answers
    /// exactly that question — which image an entry at this bci should target —
    /// including `None` across the unrolled back-edge gap, where entering is
    /// not valid at all. This is that choice applied pointwise; a bci with no
    /// valid image keeps the vector's existing `-1` sentinel rather than
    /// inventing an offset.
    ///
    /// `out_pc_to_native` is indexed by output PC; the result is indexed by
    /// original bci and is `orig_code_len + 1` long, matching what the emitter
    /// allocates today.
    pub(super) fn rebuild_pc_to_native(
        &self,
        out_pc_to_native: &[i32],
        orig_code_len: usize,
    ) -> Vec<i32> {
        let mut rebuilt = vec![-1i32; orig_code_len + 1];
        for (bci, slot) in rebuilt.iter_mut().enumerate() {
            let Some(image) = self.osr_entry_pc(bci) else {
                continue;
            };
            if let Some(&native) = out_pc_to_native.get(image) {
                *slot = native;
            }
        }
        rebuilt
    }
    pub(super) fn replicate_pc_keyed<T: Clone>(&self, table: &[(usize, T)]) -> Vec<(usize, T)> {
        let mut out: Vec<(usize, T)> = Vec::with_capacity(table.len());
        for (pc, payload) in table {
            for image in self.outputs_for_bci(*pc) {
                out.push((image, payload.clone()));
            }
        }
        out.sort_by_key(|(pc, _)| *pc);
        out
    }

    /// Output PC an OSR entry for `bci` must use, or `None` when `bci` has no
    /// steady-state image and OSR there must be REFUSED.
    ///
    /// The reverse of [`Self::bci_at`] is one-to-many inside the region, and
    /// picking the wrong image is a real bug, not a missed optimisation:
    /// entering a *peeled* copy re-runs the peeled iterations, so the loop
    /// executes `k` times too many. This always answers with the
    /// steady-state copy — copy `k` for peel, copy `0` for unroll.
    ///
    /// `None` for two reasons, and a caller must treat both the same way (do
    /// not enter compiled code; keep interpreting):
    ///
    ///  * `bci` is not a bci of this method at all; or
    ///  * `kind == Unroll` and `bci` is one of the back-edge instruction's own
    ///    bytes. The back edge is emitted in the LAST copy only, and unroll's
    ///    steady state is copy `0`, which ends just before it — those bytes
    ///    have no steady-state image. Answering `Some(bci)` (which is what
    ///    this did before) names `header + body_len`, the first byte of copy
    ///    1, i.e. the HEADER: the entry would resume a "back edge next" frame
    ///    at the top of a fresh body and run an extra iteration. Peel's
    ///    steady-state copy carries the back edge, so it has no such gap.
    ///
    /// Refusing costs nothing: the interpreter's very next bci after the back
    /// edge is the header, which always has an entry.
    pub(super) fn osr_entry_pc(&self, bci: usize) -> Option<usize> {
        if bci >= self.orig_code_len {
            return None;
        }
        if bci < self.header {
            // Prefix: the rewrite never moves it.
            return Some(bci);
        }
        if bci >= self.orig_back_edge_end {
            // Suffix: shifted past the guard, every copy and the fallback.
            return Some(bci + self.suffix_shift);
        }
        if let Some(v) = &self.versioning {
            // EVERY bci in the region, the header included, enters the FALLBACK
            // copy. It is a full image of the region — back edge included — so
            // every bci round-trips, with none of unroll's back-edge gap.
            //
            // The header does NOT enter the guard, and that is not a missed
            // optimisation but the fix for a wrong-code bug this returned
            // before it was executed on real code. Re-evaluating the guard on
            // entry looks like exactly what a fall-through entry does, and at
            // the bytecode level it is. At the MACHINE level it is not: an OSR
            // entry is only valid at a pc whose compiled state the entry
            // trampoline can reconstruct from the interpreter frame, and the
            // emitter publishes that state at loop headers, not at arbitrary
            // straight-line pcs. The guard sits in the method's prologue, where
            // a local can legitimately live in a register the trampoline does
            // not seed — entering there ran the loop with a null `this` for the
            // receiver stored just above it.
            //
            // Cost: an OSR-entered method runs the untouched loop rather than
            // the transformed one. Only entering costs that, not calling.
            return Some(v.fallback_base + (bci - self.header));
        }
        match self.kind {
            // Peel's steady state is the last copy, which is a full image of
            // the region, back edge included.
            LoopXformKind::Peel => Some(bci + self.copies * self.body_len),
            // Unroll's steady state is copy 0, which stops at the back edge.
            LoopXformKind::Unroll if bci >= self.orig_back_edge_pc() => None,
            LoopXformKind::Unroll => Some(bci),
        }
    }

    /// `true` when every output byte carries a provenance bci inside the
    /// original method.
    pub(super) fn provenance_is_total(&self) -> bool {
        self.bci_of.len() == self.code.len()
            // Widening: u32 -> usize
            && self.bci_of.iter().all(|&b| (b as usize) < self.orig_code_len)
    }
}

/// Peel `iterations` copies of a natural loop's body out ahead of the loop.
///
/// `header`/`back_edge` are a `(header_pc, back_edge_pc)` pair as produced by
/// [`detect_loops`]. `exception_ranges` are `(start, end, handler)` triples.
/// See the section header for preconditions, the poll argument and the
/// provenance contract; every failure mode is a [`LoopXformRefusal`].
///
/// Peeling does not change the loop's per-trip safepoint behaviour at all:
/// the steady-state copy keeps the same `goto` back edge, so it still polls
/// once per iteration.
#[allow(dead_code)]
pub(super) fn plan_loop_peel(
    code: &[u8],
    code_len: usize,
    header: usize,
    back_edge: usize,
    iterations: usize,
    exception_ranges: &[(usize, usize, usize)],
) -> Result<LoopXform, LoopXformRefusal> {
    rewrite_loop_copies(
        code,
        code_len,
        header,
        back_edge,
        iterations,
        exception_ranges,
        LoopXformKind::Peel,
        None,
    )
}

/// Unroll a natural loop by `extra_copies` extra bodies (an unroll factor of
/// `extra_copies + 1`).
///
/// Same preconditions as [`plan_loop_peel`]. Each copy keeps the body's own
/// exit branches, so no trip-count precondition is needed and a trip count
/// that is not a multiple of the factor leaves from the middle of the group.
///
/// This is the one transform that lengthens time-to-safepoint: the single
/// back edge now polls once per `extra_copies + 1` iterations. The product
/// `(extra_copies + 1) * body_len` is checked against
/// [`LOOP_XFORM_MAX_POLL_FREE_BYTES`], so the span stays bounded by a
/// constant rather than by the trip count.
#[allow(dead_code)]
pub(super) fn plan_loop_unroll(
    code: &[u8],
    code_len: usize,
    header: usize,
    back_edge: usize,
    extra_copies: usize,
    exception_ranges: &[(usize, usize, usize)],
) -> Result<LoopXform, LoopXformRefusal> {
    rewrite_loop_copies(
        code,
        code_len,
        header,
        back_edge,
        extra_copies,
        exception_ranges,
        LoopXformKind::Unroll,
        None,
    )
}

/// Version a natural loop against a pre-header guard: `kind`'s transform of the
/// loop on the guarded path, an untouched copy of the original on the fallback
/// path.
///
/// This is the shape every later loop transform needs — the one that lets a
/// transform with a precondition exist at all — and it is the same shape
/// `x64::vec_emit` is written against (a guard, a fallback edge, and the
/// transformed body only on the passing side). See [`LoopVersioning`] for the
/// layout, the soundness argument and what the guard does and does not mean.
///
/// Refusals are [`plan_loop_peel`]'s, plus [`LoopXformRefusal::GuardNotEncodable`]
/// and [`LoopXformRefusal::GuardIsConstant`] from the guard encoder. A caller
/// that cannot version can always fall back to the unguarded transform: the
/// guard is not what makes peel or unroll legal.
#[allow(dead_code)]
pub(super) fn plan_loop_version(
    code: &[u8],
    code_len: usize,
    header: usize,
    back_edge: usize,
    extra: usize,
    exception_ranges: &[(usize, usize, usize)],
    kind: LoopXformKind,
    guard: &PreheaderGuard,
) -> Result<LoopXform, LoopXformRefusal> {
    rewrite_loop_copies(
        code,
        code_len,
        header,
        back_edge,
        extra,
        exception_ranges,
        kind,
        Some(guard),
    )
}

/// Encode one [`PreheaderGuard`] as straight-line bytecode ending in a
/// conditional branch taken exactly when the guard FAILS.
///
/// The branch's 2-byte offset field is left zero; [`rewrite_loop_copies`]
/// patches it once the fallback copy's PC is known. The emitted sequence is
/// always `<push term> <push threshold> if_icmp<fail> 00 00`.
///
/// ## What it will emit, and why the list is this short
///
/// Only `iload` / `iload_<n>`, an integer constant push and one `if_icmp*`.
/// Every other shape is [`LoopXformRefusal::GuardNotEncodable`]. The
/// restriction is not timidity — it is what makes the guard's provenance sound:
///
/// * **Nothing that can throw.** [`BoundSource::ArrayLength`] would need
///   `aload; arraylength`, which throws `NullPointerException` at a PC whose
///   provenance is the loop header — a throw the original method does not have
///   at that bci, reported through `Compiler::orig_bci` as if it did.
///   [`BoundSource::Field`] adds resolution and class initialisation on top of
///   that. Both are refused.
/// * **Nothing that needs a constant pool.** `ldc` / `ldc2_w` name a pool entry
///   and this rewriter cannot mint one: it rewrites bytes, it does not own the
///   class. So the threshold has to fit `sipush`.
/// * **Nothing with two comparisons.** [`PreheaderGuard::StrideInRange`] is two
///   checks and would need two fallback edges. A caller that needs it can ask
///   for the two guards separately once versioning takes a guard *set*.
///
/// ## 64-bit evaluation without 64-bit bytecode
///
/// `scev` specifies these checks in 64 bits precisely so that `base + addend`
/// is not materialised as a wrapping `int` add. This encoding never
/// materialises it: the addend is folded into the compile-time threshold
/// (`T = limit - addend`, computed in `i64`), leaving a single `int` compare
/// against a value proved to be in `i32` range. A threshold outside that range
/// is not an encoding failure but a compile-time verdict —
/// [`LoopXformRefusal::GuardIsConstant`] — because no `int` could satisfy it or
/// fail it.
fn encode_preheader_guard(guard: &PreheaderGuard) -> Result<Vec<u8>, LoopXformRefusal> {
    use LoopXformRefusal as R;
    // `at_least`: the guarded path requires `term >= threshold`. Otherwise it
    // requires `term <= threshold`.
    let (term, threshold, at_least) = match guard {
        PreheaderGuard::NonNegative(t) => (t, 0i64, true),
        // Widening: i32 limit to i64.
        PreheaderGuard::AtLeast { term, limit } => (term, *limit as i64, true),
        PreheaderGuard::AtMost { term, limit } => (term, *limit as i64, false),
        PreheaderGuard::TripCountAtLeast { term, minimum } => {
            // `prove_trip_count_at_least` refuses a minimum above `u32::MAX`,
            // so this is a defensive bound, not a live case.
            if *minimum > u32::MAX as u64 {
                return Err(R::GuardNotEncodable);
            }
            // Widening: u32-bounded minimum to i64.
            (term, *minimum as i64, true)
        }
        PreheaderGuard::LengthAtLeast(_) | PreheaderGuard::StrideInRange { .. } => {
            return Err(R::GuardNotEncodable);
        }
    };
    // Widening: i32 addend to i64. Folding it here is what keeps the check
    // 64-bit-exact without a 64-bit bytecode.
    let t = threshold - term.addend as i64;
    let local = match &term.base {
        // A compile-time term settles the guard without emitting anything.
        BoundTerm::Const(_) | BoundTerm::Bound(BoundSource::Const(_)) => {
            return Err(R::GuardIsConstant);
        }
        BoundTerm::Bound(BoundSource::Local(l)) => *l,
        // The IV's value on entry to the loop, read from its local in the
        // pre-header — which is exactly where this guard sits.
        BoundTerm::IvEntry(l) => *l,
        _ => return Err(R::GuardNotEncodable),
    };
    // A threshold no `int` can be on the wrong side of is a compile-time
    // verdict, not a guard. Widening: i32 bounds to i64.
    let decided = if at_least {
        t <= i32::MIN as i64 || t > i32::MAX as i64
    } else {
        t >= i32::MAX as i64 || t < i32::MIN as i64
    };
    if decided {
        return Err(R::GuardIsConstant);
    }
    let mut out: Vec<u8> = Vec::with_capacity(8);
    match local {
        // iload_0 .. iload_3
        0..=3 => out.push(0x1a + local as u8), // Cast: 0..=3 fits u8
        // iload <index>
        4..=255 => {
            out.push(0x15);
            out.push(local as u8); // Cast: checked <= 255
        }
        // `wide iload` would make the guard a 4-byte instruction the emitter
        // walks differently; refuse rather than special-case it.
        _ => return Err(R::GuardNotEncodable),
    }
    // Cast: proved to be in `i32` range just above.
    let t = t as i32;
    match t {
        // iconst_m1 .. iconst_5
        -1..=5 => out.push((t + 3) as u8), // Cast: -1..=5 shifted into 0x02..=0x08
        // bipush
        -128..=127 => {
            out.push(0x10);
            out.push(t as i8 as u8); // Cast: checked to fit i8
        }
        // sipush
        -32768..=32767 => {
            out.push(0x11);
            // Cast: checked to fit i16
            out.extend_from_slice(&(t as i16).to_be_bytes());
        }
        // Anything wider needs `ldc`, hence a constant-pool entry.
        _ => return Err(R::GuardNotEncodable),
    }
    // The branch fires on the FAILING condition, so the fallback edge is taken
    // exactly when the guard does not hold.
    out.push(if at_least { 0xa1 } else { 0xa3 }); // if_icmplt / if_icmpgt
    out.push(0);
    out.push(0);
    Ok(out)
}

/// Number of alignment pad bytes a `tableswitch`/`lookupswitch` at `pc`
/// carries: its operands start at `pc + 1 + switch_pad(pc)`.
///
/// JVMS §6.5 aligns switch operands to a 4-byte boundary measured from the
/// START of the method's code, so the count is a function of the switch's own
/// PC. That is the whole reason moving a switch can change its LENGTH and not
/// merely its offsets, and it is why [`rewrite_loop_copies`] recomputes this
/// at the shifted PC instead of assuming a uniform shift.
///
/// Transcribed from the `let mut p = pc + 1; while p % 4 != 0 { p += 1 }`
/// walks in [`bytecode_len_at`] and [`branch_targets_at`] — keep the three in
/// step, and note that all three measure from index 0 of the `code` slice,
/// i.e. the slice must start at the method's first bytecode.
#[allow(dead_code)]
pub(super) fn switch_pad(pc: usize) -> usize {
    let mut p = pc + 1;
    while p % 4 != 0 {
        p += 1;
    }
    p - (pc + 1)
}

/// The shared peel/unroll rewriter. See the section header.
#[allow(clippy::too_many_arguments)]
fn rewrite_loop_copies(
    code: &[u8],
    code_len: usize,
    header: usize,
    back_edge: usize,
    extra: usize,
    exception_ranges: &[(usize, usize, usize)],
    kind: LoopXformKind,
    version: Option<&PreheaderGuard>,
) -> Result<LoopXform, LoopXformRefusal> {
    use LoopXformRefusal as R;

    // ── Shape ─────────────────────────────────────────────────────────
    if extra == 0 || extra > LOOP_XFORM_MAX_COPIES {
        return Err(R::TooManyCopies);
    }
    // Widening: usize vs u32::MAX — provenance entries are u32.
    if code_len == 0 || code_len > code.len() || code_len > u32::MAX as usize {
        return Err(R::BadShape);
    }
    if header >= back_edge || back_edge + 3 > code_len {
        return Err(R::BadShape);
    }
    let body_len = back_edge - header;
    if body_len > LOOP_XFORM_MAX_BODY_BYTES {
        return Err(R::BodyTooLarge);
    }
    if code[back_edge] != 0xa7 {
        return Err(R::ConditionalBackEdge);
    }
    // Cast: signed branch displacement to isize
    let off = i16::from_be_bytes([code[back_edge + 1], code[back_edge + 2]]) as isize;
    // Cast: PCs to isize for the signed comparison
    if back_edge as isize + off != header as isize {
        return Err(R::NotABackEdge);
    }
    let back_edge_end = back_edge + 3;
    let region_len = back_edge_end - header;
    // Encoded before any structural work: it is the cheapest refusal and it
    // fixes the output's length, which every PC computed below depends on.
    let guard_bytes: Vec<u8> = match version {
        None => Vec::new(),
        Some(g) => encode_preheader_guard(g)?,
    };
    let guard_len = guard_bytes.len();
    // Versioning lays down a second, UNTOUCHED image of the region on the
    // fallback path; peel and unroll lay down none.
    let fallback_len = if version.is_some() { region_len } else { 0 };
    let delta = guard_len + extra * body_len + fallback_len;

    // Where an ORIGINAL pc outside the region ends up in the output. The
    // prefix `[0, header)` and the region itself keep their PCs — the region's
    // image here is copy 0, whose first byte is still `header` — and
    // everything from `back_edge_end` on shifts past the extra copies. Used
    // for the switch check and for the exception ranges below.
    let shift = |p: usize| -> usize {
        if p >= back_edge_end {
            p + delta
        } else {
            p
        }
    };

    // ── Structural admission ──────────────────────────────────────────
    let cfg = MethodCfg::build(code, code_len).ok_or(R::OpaqueControlFlow)?;
    let hnode = cfg.node_of(header).ok_or(R::BadShape)?;
    if cfg.node_of(back_edge).is_none() {
        return Err(R::BadShape);
    }

    // Switches: admitted only when the rewrite disturbs neither their
    // PC-dependent operand padding nor their 4-byte jump offsets, neither of
    // which is re-encoded below. See precondition 6 in the section header for
    // the argument; this is that argument, per switch.
    let mut targets: Vec<usize> = Vec::new();
    for &at in cfg.nodes() {
        if matches!(code[at], 0xaa | 0xab) {
            // Inside the region the switch is DUPLICATED: each copy lands at
            // `at + ci * body_len`, so the copies disagree on their padding
            // unless `body_len % 4 == 0`, and each copy's targets would have
            // to be relocated into that copy — in a 4-byte field the rewriter
            // does not touch. Refuse.
            if at >= header && at < back_edge_end {
                return Err(R::SwitchInMethod);
            }
            // Outside the region, the switch's bytes are copied verbatim to
            // `shift(at)`. That is a faithful encoding only if the padding
            // recomputed there is the same — otherwise the operands land at a
            // different offset from the opcode and the instruction changes
            // LENGTH, which would invalidate `out_len`, the span table and
            // every PC below.
            let new_at = shift(at);
            if switch_pad(new_at) != switch_pad(at) {
                return Err(R::SwitchInMethod);
            }
            // …and its jump offsets are copied verbatim too, so every target
            // must keep the same displacement from the switch. A target
            // strictly inside the region other than the header is a separate
            // refusal (`ExternalEntry`, below) and is not admitted by this
            // arm passing.
            targets.clear();
            if !branch_targets_at(code, at, code_len, &mut targets) {
                return Err(R::OpaqueControlFlow);
            }
            for &t in &targets {
                // Cast: PCs to isize for the signed displacement comparison
                let old_off = t as isize - at as isize;
                let new_off = shift(t) as isize - new_at as isize;
                if new_off != old_off {
                    return Err(R::SwitchInMethod);
                }
            }
        }
        // `jsr`/`ret`/`jsr_w` already fail `MethodCfg::build`; `goto_w` does
        // not, and it is the one backward branch the emitter never polls, so
        // it must not survive into a method we are about to duplicate code
        // in. Refuse the whole method rather than reason about where it is.
        if matches!(code[at], 0xa8 | 0xa9 | 0xc8 | 0xc9) {
            return Err(R::OpaqueControlFlow);
        }
    }

    // Single-entry, and no branch to the back edge itself.
    for &at in cfg.nodes() {
        targets.clear();
        if !branch_targets_at(code, at, code_len, &mut targets) {
            return Err(R::OpaqueControlFlow);
        }
        let src_inside = at >= header && at < back_edge_end;
        for &t in &targets {
            let dst_inside = t >= header && t < back_edge_end;
            if dst_inside && !src_inside && t != header {
                return Err(R::ExternalEntry);
            }
            if t == back_edge && at != back_edge {
                return Err(R::BranchToBackEdge);
            }
        }
    }

    // Reducibility of this loop: the header dominates its whole region. This
    // is the condition that makes "duplicate the region" meaningful — every
    // execution that reaches any part of the region reached the header first,
    // so every copy starts an iteration.
    for (i, &at) in cfg.nodes().iter().enumerate() {
        if at < header || at >= back_edge_end {
            continue;
        }
        if !cfg.is_reachable(i) {
            return Err(R::UnreachableInRegion);
        }
        if !cfg.dominates(hnode, i) {
            return Err(R::Irreducible);
        }
    }

    // Reducibility of every cycle strictly inside the body.
    for (i, &at) in cfg.nodes().iter().enumerate() {
        if at < header || at >= back_edge_end {
            continue;
        }
        targets.clear();
        if !branch_targets_at(code, at, code_len, &mut targets) {
            return Err(R::OpaqueControlFlow);
        }
        for &t in &targets {
            if t > at || t == header {
                continue; // forward edge, or this loop's own back edge
            }
            let tnode = cfg.node_of(t).ok_or(R::BadShape)?;
            if !cfg.dominates(tnode, i) {
                return Err(R::IrreducibleInnerLoop);
            }
        }
    }

    // ── Exception ranges ──────────────────────────────────────────────
    // `shift` is defined with the region bounds above; a range that encloses
    // the region has `s <= header` and `e >= back_edge_end`, so shifting its
    // end alone widens it over every copy.
    let mut ranges_out: Vec<(usize, usize, usize)> = Vec::with_capacity(exception_ranges.len());
    for &(s, e, h) in exception_ranges {
        if s >= e || e > code_len || h >= code_len {
            return Err(R::BadShape);
        }
        if h >= header && h < back_edge_end {
            return Err(R::HandlerInRegion);
        }
        let overlaps = s < back_edge_end && e > header;
        let encloses = s <= header && e >= back_edge_end;
        if overlaps && !encloses {
            // Partially overlapping (including wholly inside): duplicating
            // the region would need the range duplicated with it.
            return Err(R::HandlerRangeStraddlesRegion);
        }
        ranges_out.push((shift(s), shift(e), shift(h)));
    }

    // ── Emit ──────────────────────────────────────────────────────────
    let out_len = code_len + delta;
    let mut out: Vec<u8> = Vec::with_capacity(out_len);
    let mut bci_of: Vec<u32> = Vec::with_capacity(out_len);
    let push_span = |from: usize, to: usize, out: &mut Vec<u8>, bci_of: &mut Vec<u32>| {
        out.extend_from_slice(&code[from..to]);
        for p in from..to {
            // Cast: bounded by `code_len`, checked against u32::MAX above
            bci_of.push(p as u32);
        }
    };
    push_span(0, header, &mut out, &mut bci_of);
    // The guard's bytes carry the HEADER's bci. They are not an image of it —
    // `outputs_for_bci` skips them — but every output byte must resolve to a
    // real bci or `Compiler::orig_bci` is unsound, and the header is the bci
    // whose frame the guard runs with: the sequence is stack-balanced, writes
    // no local, and cannot throw, so no deopt or exception site can name a
    // guard PC in the first place.
    out.extend_from_slice(&guard_bytes);
    for _ in 0..guard_len {
        // Cast: bounded by `code_len`, checked against u32::MAX above
        bci_of.push(header as u32);
    }
    for _ in 0..extra {
        push_span(header, back_edge, &mut out, &mut bci_of);
    }
    push_span(header, back_edge_end, &mut out, &mut bci_of);
    if version.is_some() {
        push_span(header, back_edge_end, &mut out, &mut bci_of);
    }
    push_span(back_edge_end, code_len, &mut out, &mut bci_of);
    if out.len() != out_len || bci_of.len() != out_len {
        return Err(R::BadShape);
    }

    let fast_base = header + guard_len;
    let fallback_base = fast_base + extra * body_len + region_len;
    let loop_entry = match kind {
        LoopXformKind::Peel => fast_base + extra * body_len,
        LoopXformKind::Unroll => fast_base,
    };

    // Spans of the output, as (out_base, orig_from, orig_to, region), where
    // `region` is `Some((base, next_iteration))` for a copy of the loop region:
    // `base` is where that copy's image of the header sits, and
    // `next_iteration` is where a branch to the header from INSIDE that copy
    // goes. `out_pc = out_base + (orig_pc - orig_from)` inside each span.
    let mut spans: Vec<(usize, usize, usize, Option<(usize, usize)>)> =
        Vec::with_capacity(extra + 4);
    spans.push((0, 0, header, None));
    for ci in 0..extra {
        let base = fast_base + ci * body_len;
        spans.push((base, header, back_edge, Some((base, base + body_len))));
    }
    let last = fast_base + extra * body_len;
    spans.push((last, header, back_edge_end, Some((last, loop_entry))));
    if version.is_some() {
        // The fallback is a self-contained image of the original loop: its back
        // edge targets its own first byte, so it is a loop in its own right and
        // never re-enters the transformed copies.
        spans.push((
            fallback_base,
            header,
            back_edge_end,
            Some((fallback_base, fallback_base)),
        ));
    }
    spans.push((back_edge_end + delta, back_edge_end, code_len, None));

    for &(out_base, from, to, region) in &spans {
        let mut pc = from;
        while pc < to {
            let len = bytecode_len_at(code, pc);
            if len == 0 {
                return Err(R::BadShape);
            }
            if matches!(code[pc], 0x99..=0xa7 | 0xc6 | 0xc7) {
                if pc + 2 >= code_len {
                    return Err(R::BadShape);
                }
                // Cast: signed branch displacement to isize
                let boff = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as isize;
                // Cast: PC to isize for the signed target computation
                let t = pc as isize + boff;
                if t < 0 || t as usize >= code_len {
                    return Err(R::BadShape);
                }
                // Cast: non-negative index to usize
                let t = t as usize;
                let new_t = match region {
                    Some((region_base, next_iter)) => {
                        if t == header {
                            // A branch to the header from inside is "next
                            // iteration": the next copy, the loop entry from
                            // the last one (peel: the last copy itself; unroll:
                            // the first), and its own first byte from the
                            // fallback. The back-edge `goto` is exactly this
                            // case in the span that carries it.
                            next_iter
                        } else if t < header {
                            t
                        } else if t < back_edge_end {
                            // Into the region from inside it: relocate into
                            // this copy.
                            region_base + (t - header)
                        } else {
                            t + delta
                        }
                    }
                    // Prefix or suffix. An edge into the region from outside it
                    // can only target the header — every other case was refused
                    // as `ExternalEntry` — and the header's image for an
                    // OUTSIDE edge is the guard, so an entry from anywhere is
                    // guarded. Without versioning the guard is empty and this
                    // is the header itself, unmoved.
                    None => {
                        if t >= header && t < back_edge_end {
                            header
                        } else if t < header {
                            t
                        } else {
                            t + delta
                        }
                    }
                };
                let out_pc = out_base + (pc - from);
                // Cast: PCs to isize for the signed offset
                let new_off = new_t as isize - out_pc as isize;
                // Widening: i16 bounds to isize
                if new_off < i16::MIN as isize || new_off > i16::MAX as isize {
                    return Err(R::OffsetOverflow);
                }
                // Cast: checked above to fit the 2-byte signed branch field
                let enc = (new_off as i16).to_be_bytes();
                if out_pc + 2 >= out.len() {
                    return Err(R::BadShape);
                }
                out[out_pc + 1] = enc[0];
                out[out_pc + 2] = enc[1];
            }
            pc += len;
        }
    }

    // ── The guard's fallback edge ─────────────────────────────────────
    //
    // `encode_preheader_guard` always ends in a 3-byte conditional branch with
    // a zero offset field; this is where it learns what to jump to. Patched
    // after the copies so `fallback_base` is a settled PC, and re-proved by the
    // CFG build below like every other branch in the output.
    if guard_len != 0 {
        let at = fast_base - 3;
        // Cast: PCs to isize for the signed offset
        let off = fallback_base as isize - at as isize;
        // Widening: i16 bounds to isize
        if off < i16::MIN as isize || off > i16::MAX as isize {
            return Err(R::OffsetOverflow);
        }
        // Cast: checked above to fit the 2-byte signed branch field
        let enc = (off as i16).to_be_bytes();
        if at + 2 >= out.len() {
            return Err(R::BadShape);
        }
        out[at + 1] = enc[0];
        out[at + 2] = enc[1];
    }

    // ── Prove it on the emitted bytes, do not argue it ────────────────
    //
    // The output must walk exactly, keep every branch target on an
    // instruction boundary, and stay buildable as a CFG.
    if MethodCfg::build(&out, out_len).is_none() {
        return Err(R::BadShape);
    }
    // Every backward branch sits at a poll-bearing opcode ⇒ every cycle in
    // the transformed method is polled (see the section header).
    if !all_backward_edges_are_polled(&out, out_len) {
        return Err(R::UnpolledBackEdge);
    }
    // …and EVERY loop this rewrite produced has its own back-edge poll: the
    // transformed one, and the fallback when there is one. A versioned method
    // has two loops, and checking only the first would leave the fallback — the
    // copy OSR enters — unproven.
    let out_back_edge = fast_base + (extra + 1) * body_len;
    if !emits_safepoint_poll_at(&out, out_back_edge, out_len) {
        return Err(R::UnpolledBackEdge);
    }
    if version.is_some() && !emits_safepoint_poll_at(&out, fallback_base + body_len, out_len) {
        return Err(R::UnpolledBackEdge);
    }
    // The guard sits between the previous poll and the first copy, so it
    // lengthens the poll-free span by its own (tiny, bounded) size.
    let poll_free_bytes = guard_len.saturating_add(body_len.saturating_mul(extra + 1));
    if poll_free_bytes > LOOP_XFORM_MAX_POLL_FREE_BYTES {
        return Err(R::TimeToSafepointBudget);
    }

    Ok(LoopXform {
        kind,
        code: out,
        code_len: out_len,
        bci_of,
        exception_ranges: ranges_out,
        header,
        body_len,
        copies: extra,
        loop_entry,
        poll_free_bytes,
        orig_code_len: code_len,
        orig_back_edge_end: back_edge_end,
        versioning: version.map(|g| LoopVersioning {
            guard: g.clone(),
            guard_pc: header,
            guard_len,
            fallback_base,
        }),
        suffix_shift: delta,
    })
}

// ── Loop transform tests ─────────────────────────────────────────────
//
// The equivalence tests run a reference interpreter over the original and the
// transformed bytecode and compare the FULL `(original bci, locals)` step
// sequence, not just the answer. That single comparison is the acceptance
// criterion for three separate properties:
//
//  * same result on every trip count (the outcome is the last step),
//  * unchanged exception order (a throw is a step with a bci),
//  * a deopt into a transformed loop resolves its locals — at every output PC
//    the frame is the one the interpreter had at `bci_of[pc]`.
//
// The one instruction the comparison filters out is the back-edge `goto`,
// which peel/unroll elide in the copies. A `goto` has no data effect and
// cannot throw, so eliding it cannot change the sequence above; its ONE
// observable effect is the safepoint poll, and that is asserted separately
// and quantitatively in `every_transform_preserves_the_backedge_poll`.

#[cfg(test)]
mod reachability_roots {
    use super::*;

    /// `return; <handler body>` — the shape javac emits when a `try` block
    /// returns. The handler is reachable from nothing the bytecode names.
    ///
    /// Without a root it is dead, which is the pre-2026-08-20 world and why
    /// "a handler body is dead code in the emitted image" was true. With one it
    /// is live, and so is everything it falls through to — which is what makes
    /// `pc_to_native[handler_pc]` a real address for a local-handler stub to
    /// jump to instead of the `-1` that would reject the whole method.
    #[test]
    fn a_handler_root_revives_the_block_and_nothing_else_does() {
        // 0: return
        // 1: astore_0        <- handler_pc
        // 2: return
        let code = [0xb1u8, 0x4b, 0xb1];
        let without = compute_reachable_pcs(&code, code.len()).expect("statically known CFG");
        assert!(without[0]);
        assert!(!without[1], "nothing branches to a handler body");
        assert!(!without[2]);

        let with = compute_reachable_pcs_with_roots(&code, code.len(), &[1])
            .expect("statically known CFG");
        assert!(with[0]);
        assert!(with[1], "the handler root makes its own block live");
        assert!(with[2], "and everything the handler falls through to");
    }

    /// An empty root list must be the identity, because that is every compile
    /// that arms no local handlers — i.e. every compile until someone sets the
    /// flag.
    #[test]
    fn no_roots_is_the_identity() {
        // 0: iconst_0  1: ifeq +4 (->5)  4: return  5: return
        let code = [0x03u8, 0x99, 0x00, 0x04, 0xb1, 0xb1];
        assert_eq!(
            compute_reachable_pcs(&code, code.len()),
            compute_reachable_pcs_with_roots(&code, code.len(), &[]),
        );
    }
}

#[cfg(test)]
mod loop_xform_tests {
    use super::super::{detect_loops, find_bypassable_loop_headers};
    use super::*;

    /// How a fixture run ended.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Outcome {
        Return(i32),
        Void,
        /// Exception kind, and the ORIGINAL bci that threw.
        Throw(&'static str, usize),
        StepLimit,
    }

    /// One reference run.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Trace {
        /// `(original bci, locals)` before every executed instruction.
        steps: Vec<(usize, Vec<i32>)>,
        /// Execution PC of each step, in the same order. Only a VERSIONED run
        /// needs it: a guard's bytes carry the header's bci, so `steps` alone
        /// cannot tell a guard from the header, and filtering the guard out by
        /// bci would delete the header's own steps too.
        step_pcs: Vec<usize>,
        outcome: Outcome,
        heap: Vec<i32>,
    }

    /// An exception-handler body is not reachable along ordinary control flow,
    /// so nothing inside it is — including the merge point of a branch the
    /// handler body makes to itself. This is the whole content of the
    /// handler-body-merge refusal: the emitter's branch-target map said `true`
    /// for that merge, and "some instruction branches here" is not
    /// "control can reach here".
    #[test]
    fn a_handler_bodys_own_branch_targets_are_not_reachable() {
        //  0: iload_0
        //  1: ireturn
        //  2: iload_0           <- handler body (exception-table target)
        //  3: aload_1
        //  4: ifnonnull 11
        //  7: iconst_0
        //  8: goto 12
        // 11: iconst_1
        // 12: iadd
        // 13: ireturn
        let code = [
            0x1a, 0xac, 0x1a, 0x2b, 0xc7, 0x00, 0x07, 0x03, 0xa7, 0x00, 0x04, 0x04, 0x60, 0xac,
        ];
        let r = compute_reachable_pcs(&code, code.len()).expect("statically known control flow");
        assert_eq!(&r[..2], &[true, true], "the live prefix is reachable");
        assert!(
            r[2..code.len()].iter().all(|&b| !b),
            "nothing after the `ireturn` is reachable: {r:?}"
        );

        // The emitter's own question answers differently for the two merges,
        // which is exactly why it could not be used for this.
        let targets = compute_branch_targets(&code, code.len());
        assert!(targets[11] && targets[12]);
    }

    /// Both edges of a conditional, and the fall-through of everything that
    /// has one, are reachable; a `goto` has no fall-through.
    #[test]
    fn reachability_follows_both_edges_and_stops_at_a_goto() {
        //  0: iload_0
        //  1: ifeq 7
        //  4: goto 8
        //  7: iconst_1          (reached only by the `ifeq`)
        //  8: ireturn
        let code = [0x1a, 0x99, 0x00, 0x06, 0xa7, 0x00, 0x04, 0x04, 0xac];
        let r = compute_reachable_pcs(&code, code.len()).expect("statically known control flow");
        assert_eq!(
            &r[..code.len()],
            &[true, true, false, false, true, false, false, true, true],
            "only instruction boundaries on a real path are marked"
        );
    }

    /// Every arm of a switch is an edge, and the switch itself has no
    /// fall-through.
    #[test]
    fn reachability_follows_every_switch_arm() {
        //  0: iconst_0
        //  1: nop; nop          (align the tableswitch operands to 4)
        //  3: tableswitch { 0: +21 (24), 1: +23 (26), default: +25 (28) }
        // 24: iconst_1; ireturn
        // 26: iconst_2; ireturn
        // 28: iconst_3; ireturn
        let mut code: Vec<u8> = vec![0x03, 0x00, 0x00, 0xaa];
        code.extend_from_slice(&25i32.to_be_bytes()); // default -> 28
        code.extend_from_slice(&0i32.to_be_bytes()); // low
        code.extend_from_slice(&1i32.to_be_bytes()); // high
        code.extend_from_slice(&21i32.to_be_bytes()); // case 0 -> 24
        code.extend_from_slice(&23i32.to_be_bytes()); // case 1 -> 26
        code.extend_from_slice(&[0x04, 0xac, 0x05, 0xac, 0x06, 0xac]);
        assert_eq!(code.len(), 30);
        let r = compute_reachable_pcs(&code, code.len()).expect("statically known control flow");
        for pc in [0usize, 1, 2, 3, 24, 25, 26, 27, 28, 29] {
            assert!(r[pc], "pc {pc} is on a real path");
        }
        for pc in 4..24usize {
            assert!(!r[pc], "pc {pc} is switch payload, not an instruction");
        }
    }

    /// `jsr`/`ret` have no statically known successor set. The analysis refuses
    /// rather than under-approximating reachability, because an
    /// under-approximation here deletes live code.
    #[test]
    fn opaque_control_flow_refuses_rather_than_guessing() {
        let jsr = [0xa8, 0x00, 0x03, 0xac]; // jsr +3; ireturn
        assert!(compute_reachable_pcs(&jsr, jsr.len()).is_none());
        let ret = [0xa9, 0x01]; // ret 1
        assert!(compute_reachable_pcs(&ret, ret.len()).is_none());
    }

    /// Target PC of the 2-byte-offset branch at `pc`, or its fall-through.
    fn branch_to(code: &[u8], pc: usize, taken: bool) -> usize {
        if taken {
            let off = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as isize;
            (pc as isize + off) as usize
        } else {
            pc + 3
        }
    }

    /// Reference interpreter for the bytecode subset the fixtures use.
    ///
    /// `bci_of` maps an execution PC to the ORIGINAL bci (identity for an
    /// untransformed run) so two runs can be compared step for step. Local 0
    /// doubles as the fixtures' array reference: `aload_0` pushes a handle for
    /// the single `heap` array.
    fn interp(
        code: &[u8],
        code_len: usize,
        bci_of: Option<&[u32]>,
        locals_in: &[i32],
        heap_in: &[i32],
    ) -> Trace {
        let mut locals = locals_in.to_vec();
        let mut heap = heap_in.to_vec();
        let mut stack: Vec<i32> = Vec::new();
        let mut steps: Vec<(usize, Vec<i32>)> = Vec::new();
        let mut step_pcs: Vec<usize> = Vec::new();
        let mut pc = 0usize;
        let mut budget = 200_000usize;
        loop {
            if budget == 0 {
                return Trace {
                    steps,
                    step_pcs,
                    outcome: Outcome::StepLimit,
                    heap,
                };
            }
            budget -= 1;
            assert!(pc < code_len, "control ran off the end at pc {pc}");
            let bci = match bci_of {
                Some(m) => m[pc] as usize,
                None => pc,
            };
            steps.push((bci, locals.clone()));
            step_pcs.push(pc);
            let op = code[pc];
            match op {
                // nop
                0x00 => pc += 1,
                // iconst_m1 .. iconst_5
                0x02..=0x08 => {
                    stack.push(op as i32 - 3);
                    pc += 1;
                }
                // bipush
                0x10 => {
                    stack.push(code[pc + 1] as i8 as i32);
                    pc += 2;
                }
                // sipush
                0x11 => {
                    stack.push(i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32);
                    pc += 3;
                }
                // iload
                0x15 => {
                    stack.push(locals[code[pc + 1] as usize]);
                    pc += 2;
                }
                // iload_0 .. iload_3
                0x1a..=0x1d => {
                    stack.push(locals[(op - 0x1a) as usize]);
                    pc += 1;
                }
                // aload_0 — the fixtures' single array reference
                0x2a => {
                    stack.push(0);
                    pc += 1;
                }
                // iaload
                0x2e => {
                    let idx = stack.pop().expect("iaload index");
                    let _aref = stack.pop().expect("iaload arrayref");
                    if idx < 0 || idx as usize >= heap.len() {
                        return Trace {
                            steps,
                            step_pcs,
                            outcome: Outcome::Throw("ArrayIndexOutOfBounds", bci),
                            heap,
                        };
                    }
                    stack.push(heap[idx as usize]);
                    pc += 1;
                }
                // istore
                0x36 => {
                    let v = stack.pop().expect("istore value");
                    locals[code[pc + 1] as usize] = v;
                    pc += 2;
                }
                // istore_0 .. istore_3
                0x3b..=0x3e => {
                    let v = stack.pop().expect("istore_n value");
                    locals[(op - 0x3b) as usize] = v;
                    pc += 1;
                }
                // iastore
                0x4f => {
                    let v = stack.pop().expect("iastore value");
                    let idx = stack.pop().expect("iastore index");
                    let _aref = stack.pop().expect("iastore arrayref");
                    if idx < 0 || idx as usize >= heap.len() {
                        return Trace {
                            steps,
                            step_pcs,
                            outcome: Outcome::Throw("ArrayIndexOutOfBounds", bci),
                            heap,
                        };
                    }
                    heap[idx as usize] = v;
                    pc += 1;
                }
                // iadd / isub / imul
                0x60 | 0x64 | 0x68 => {
                    let b = stack.pop().expect("binop rhs");
                    let a = stack.pop().expect("binop lhs");
                    stack.push(match op {
                        0x60 => a.wrapping_add(b),
                        0x64 => a.wrapping_sub(b),
                        _ => a.wrapping_mul(b),
                    });
                    pc += 1;
                }
                // idiv
                0x6c => {
                    let b = stack.pop().expect("idiv rhs");
                    let a = stack.pop().expect("idiv lhs");
                    if b == 0 {
                        return Trace {
                            steps,
                            step_pcs,
                            outcome: Outcome::Throw("ArithmeticException", bci),
                            heap,
                        };
                    }
                    stack.push(a.wrapping_div(b));
                    pc += 1;
                }
                // iinc
                0x84 => {
                    let l = code[pc + 1] as usize;
                    locals[l] = locals[l].wrapping_add(code[pc + 2] as i8 as i32);
                    pc += 3;
                }
                // ifeq .. ifle
                0x99..=0x9e => {
                    let v = stack.pop().expect("if<cond> operand");
                    let taken = match op {
                        0x99 => v == 0,
                        0x9a => v != 0,
                        0x9b => v < 0,
                        0x9c => v >= 0,
                        0x9d => v > 0,
                        _ => v <= 0,
                    };
                    pc = branch_to(code, pc, taken);
                }
                // if_icmpeq .. if_icmple
                0x9f..=0xa4 => {
                    let b = stack.pop().expect("if_icmp rhs");
                    let a = stack.pop().expect("if_icmp lhs");
                    let taken = match op {
                        0x9f => a == b,
                        0xa0 => a != b,
                        0xa1 => a < b,
                        0xa2 => a >= b,
                        0xa3 => a > b,
                        _ => a <= b,
                    };
                    pc = branch_to(code, pc, taken);
                }
                // goto
                0xa7 => pc = branch_to(code, pc, true),
                // ireturn
                0xac => {
                    let v = stack.pop().expect("ireturn value");
                    return Trace {
                        steps,
                        step_pcs,
                        outcome: Outcome::Return(v),
                        heap,
                    };
                }
                // return
                0xb1 => {
                    return Trace {
                        steps,
                        step_pcs,
                        outcome: Outcome::Void,
                        heap,
                    }
                }
                _ => panic!("fixture uses an opcode the reference interpreter lacks: {op:#04x}"),
            }
        }
    }

    /// The step sequence with the back-edge `goto` removed (see the section
    /// comment: it has no data effect and cannot throw).
    fn steps_without_back_edge(t: &Trace, back_edge: usize) -> Vec<(usize, Vec<i32>)> {
        steps_without_back_edge_or_guard(t, back_edge, None)
    }

    /// …and with a versioning guard's synthetic bytes removed as well.
    ///
    /// The guard is filtered by output PC, not by bci: it carries the header's
    /// bci so that provenance stays total, so filtering by bci would delete the
    /// header's own steps. Removing it is sound for the same reason removing
    /// the back edge is — it writes no local, cannot throw, and its only
    /// observable effect is which version runs, which the caller asserts
    /// separately.
    fn steps_without_back_edge_or_guard(
        t: &Trace,
        back_edge: usize,
        guard: Option<(usize, usize)>,
    ) -> Vec<(usize, Vec<i32>)> {
        t.steps
            .iter()
            .zip(t.step_pcs.iter())
            .filter(|(s, pc)| {
                s.0 != back_edge && !matches!(guard, Some((from, to)) if **pc >= from && **pc < to)
            })
            .map(|(s, _)| s.clone())
            .collect()
    }

    /// How many times the back edge — hence the safepoint poll — executed.
    fn poll_count(t: &Trace, back_edge: usize) -> usize {
        t.steps.iter().filter(|(b, _)| *b == back_edge).count()
    }

    /// Every PC whose instruction has a backward branch target.
    fn backward_branch_pcs(code: &[u8], code_len: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut targets: Vec<usize> = Vec::new();
        let mut pc = 0usize;
        while pc < code_len {
            targets.clear();
            assert!(
                branch_targets_at(code, pc, code_len, &mut targets),
                "opaque control flow at {pc}"
            );
            if targets.iter().any(|&t| t <= pc) {
                out.push(pc);
            }
            pc += bytecode_len_at(code, pc);
        }
        out
    }

    /// `int i = 0, sum = 0; while (i < n) { sum += i * 2; i++; } return sum;`
    ///
    /// locals: 0 = n, 1 = i, 2 = sum. Header 4, back edge 18, length 23.
    /// This is javac's condition-at-top `while` shape.
    fn shape_a() -> Vec<u8> {
        vec![
            0x03, // 0: iconst_0
            0x3c, // 1: istore_1          i = 0
            0x03, // 2: iconst_0
            0x3d, // 3: istore_2          sum = 0
            0x1b, // 4: iload_1           <- header
            0x1a, // 5: iload_0
            0xa2, 0x00, 0x0f, // 6: if_icmpge 21
            0x1c, // 9: iload_2
            0x1b, // 10: iload_1
            0x05, // 11: iconst_2
            0x68, // 12: imul
            0x60, // 13: iadd
            0x3d, // 14: istore_2
            0x84, 0x01, 0x01, // 15: iinc 1, 1
            0xa7, 0xff, 0xf2, // 18: goto 4      <- back edge
            0x1c, // 21: iload_2
            0xac, // 22: ireturn
        ]
    }

    /// The pre-header-bypass shape: a branch from *before* the loop straight
    /// into the header. This is `AttributesImpl.ensureCapacity`'s `goto` into
    /// the `while` header — the witness the LICM pre-header bypass fix was
    /// written for. Same loop as [`shape_a`], header 11, back edge 25,
    /// length 30.
    fn shape_b() -> Vec<u8> {
        vec![
            0x03, // 0: iconst_0
            0x3c, // 1: istore_1          i = 0
            0x03, // 2: iconst_0
            0x3d, // 3: istore_2          sum = 0
            0x1a, // 4: iload_0
            0x9a, 0x00, 0x06, // 5: ifne 11      <- external edge INTO the header
            0x03, // 8: iconst_0
            0x3d, // 9: istore_2
            0x00, // 10: nop
            0x1b, // 11: iload_1          <- header
            0x1a, // 12: iload_0
            0xa2, 0x00, 0x0f, // 13: if_icmpge 28
            0x1c, // 16: iload_2
            0x1b, // 17: iload_1
            0x05, // 18: iconst_2
            0x68, // 19: imul
            0x60, // 20: iadd
            0x3d, // 21: istore_2
            0x84, 0x01, 0x01, // 22: iinc 1, 1
            0xa7, 0xff, 0xf2, // 25: goto 11     <- back edge
            0x1c, // 28: iload_2
            0xac, // 29: ireturn
        ]
    }

    /// `for (i = 0; i < n; i++) a[i] = 100 / (i - 3);`
    ///
    /// Throws ArithmeticException at `i == 3`, after three stores, and
    /// ArrayIndexOutOfBounds past the array end — two different exceptions at
    /// two different bcis, so a transform that reorders effects is caught.
    /// locals: 0 = array, 1 = n, 2 = i. Header 2, back edge 19, length 23.
    fn shape_throws() -> Vec<u8> {
        vec![
            0x03, // 0: iconst_0
            0x3d, // 1: istore_2          i = 0
            0x1c, // 2: iload_2           <- header
            0x1b, // 3: iload_1
            0xa2, 0x00, 0x12, // 4: if_icmpge 22
            0x2a, // 7: aload_0
            0x1c, // 8: iload_2
            0x10, 0x64, // 9: bipush 100
            0x1c, // 11: iload_2
            0x06, // 12: iconst_3
            0x64, // 13: isub
            0x6c, // 14: idiv
            0x4f, // 15: iastore
            0x84, 0x02, 0x01, // 16: iinc 2, 1
            0xa7, 0xff, 0xef, // 19: goto 2       <- back edge
            0xb1, // 22: return
        ]
    }

    /// `do { i++; } while (i < n);` — the back edge is the exit test itself.
    /// Header 2, back edge 7, length 12.
    fn shape_do_while() -> Vec<u8> {
        vec![
            0x03, // 0: iconst_0
            0x3c, // 1: istore_1
            0x84, 0x01, 0x01, // 2: iinc 1, 1     <- header
            0x1b, // 5: iload_1
            0x1a, // 6: iload_0
            0xa1, 0xff, 0xfb, // 7: if_icmplt 2   <- conditional back edge
            0x1b, // 10: iload_1
            0xac, // 11: ireturn
        ]
    }

    /// An irreducible loop: the cycle `L1 → L2 → L1` is entered at BOTH
    /// blocks. Its back edge is a plain `goto`, so the transform gets past
    /// every shape check and has to refuse on the entry/domination test.
    /// Header 7, back edge 17, length 21.
    fn shape_irreducible() -> Vec<u8> {
        vec![
            0x1a, // 0: iload_0
            0x99, 0x00, 0x0c, // 1: ifeq 13       -> enters the cycle at L2
            0xa7, 0x00, 0x03, // 4: goto 7        -> enters the cycle at L1
            0x84, 0x01, 0x01, // 7: L1: iinc 1, 1
            0xa7, 0x00, 0x03, // 10: goto 13
            0x1b, // 13: L2: iload_1
            0x99, 0x00, 0x06, // 14: ifeq 20
            0xa7, 0xff, 0xf6, // 17: goto 7       <- back edge
            0xb1, // 20: return
        ]
    }

    /// A reducible outer loop whose BODY contains an irreducible two-entry
    /// cycle (`A → B → A`, entered at both). The outer header dominates its
    /// whole region and there is no external entry, so only the inner
    /// reducibility test can catch this. Header 2, back edge 30, length 35.
    fn shape_irreducible_inner() -> Vec<u8> {
        vec![
            0x03, // 0: iconst_0
            0x3c, // 1: istore_1
            0x1b, // 2: iload_1            <- outer header
            0x1a, // 3: iload_0
            0xa2, 0x00, 0x1d, // 4: if_icmpge 33
            0x1b, // 7: iload_1
            0x99, 0x00, 0x0c, // 8: ifeq 20        -> enters the inner cycle at B
            0xa7, 0x00, 0x03, // 11: goto 14       -> enters the inner cycle at A
            0x84, 0x01, 0x01, // 14: A: iinc 1, 1
            0xa7, 0x00, 0x03, // 17: goto 20
            0x1b, // 20: B: iload_1
            0x99, 0x00, 0x06, // 21: ifeq 27
            0xa7, 0xff, 0xf6, // 24: goto 14       <- inner back edge
            0x84, 0x01, 0x01, // 27: iinc 1, 1
            0xa7, 0xff, 0xe4, // 30: goto 2        <- outer back edge
            0x1b, // 33: iload_1
            0xac, // 34: ireturn
        ]
    }

    /// A one-instruction loop whose header is reached by a `goto_w`, the one
    /// backward-capable branch the emitter never polls. Header 0, back edge 5.
    fn shape_with_goto_w() -> Vec<u8> {
        vec![
            0xc8, 0x00, 0x00, 0x00, 0x05, // 0: goto_w 5
            0xa7, 0xff, 0xfb, // 5: goto 0    <- back edge
        ]
    }

    #[test]
    fn fixtures_have_the_loops_the_tests_assume() {
        assert_eq!(detect_loops(&shape_a(), 23), vec![(4, 18)]);
        assert_eq!(detect_loops(&shape_b(), 30), vec![(11, 25)]);
        assert_eq!(detect_loops(&shape_throws(), 23), vec![(2, 19)]);
        assert_eq!(detect_loops(&shape_do_while(), 12), vec![(2, 7)]);
        assert_eq!(detect_loops(&shape_irreducible(), 21), vec![(7, 17)]);
        assert_eq!(
            detect_loops(&shape_irreducible_inner(), 35),
            vec![(14, 24), (2, 30)]
        );
    }

    #[test]
    fn a_transformed_loop_computes_the_same_result_on_every_trip_count() {
        for (code, header, back_edge) in [(shape_a(), 4usize, 18usize), (shape_b(), 11, 25)] {
            let len = code.len();
            for k in 1..=3usize {
                let peel = plan_loop_peel(&code, len, header, back_edge, k, &[])
                    .expect("peel should be admitted");
                let unroll = plan_loop_unroll(&code, len, header, back_edge, k, &[])
                    .expect("unroll should be admitted");
                // Trip counts 0 and 1 are the interesting ends: with 0 the
                // first copy's exit test fires before any body effect, and
                // with 1 the loop leaves from the middle of the copy group.
                for n in 0..=6i32 {
                    let locals = [n, 0, 0];
                    let base = interp(&code, len, None, &locals, &[]);
                    assert_eq!(
                        base.outcome,
                        Outcome::Return(n * (n - 1)),
                        "fixture itself is wrong for n = {n}"
                    );
                    for x in [&peel, &unroll] {
                        let t = interp(&x.code, x.code_len, Some(&x.bci_of), &locals, &[]);
                        assert_eq!(t.outcome, base.outcome, "{:?} k = {k}, n = {n}", x.kind);
                        assert_eq!(
                            steps_without_back_edge(&t, back_edge),
                            steps_without_back_edge(&base, back_edge),
                            "{:?} k = {k}, n = {n}: executed sequence diverged",
                            x.kind
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn every_transform_preserves_the_backedge_poll() {
        let code = shape_a();
        let len = code.len();
        // The emitter polls at a backward branch and nowhere else, and the
        // original loop's only backward branch is its back edge.
        assert!(emits_safepoint_poll_at(&code, 18, len));
        assert!(!emits_safepoint_poll_at(&code, 6, len));
        assert!(all_backward_edges_are_polled(&code, len));
        assert_eq!(backward_branch_pcs(&code, len), vec![18]);

        for k in 1..=3usize {
            for x in [
                plan_loop_peel(&code, len, 4, 18, k, &[]).expect("peel"),
                plan_loop_unroll(&code, len, 4, 18, k, &[]).expect("unroll"),
            ] {
                // Static half: every backward branch in the OUTPUT is at an
                // opcode the emitter polls, and there is exactly one — the
                // loop's own back edge. Since every cycle in a linear bytecode
                // CFG contains a backward branch, every cycle is polled.
                assert!(all_backward_edges_are_polled(&x.code, x.code_len));
                assert_eq!(
                    backward_branch_pcs(&x.code, x.code_len),
                    vec![x.back_edge_pc()],
                    "{:?} k = {k}",
                    x.kind
                );
                assert!(emits_safepoint_poll_at(&x.code, x.back_edge_pc(), x.code_len));

                // Time-to-safepoint: the polled cycle is one body long for
                // peel (unchanged), k + 1 bodies for unroll (bounded).
                let cycle = x.back_edge_pc() - x.steady_state_base();
                match x.kind {
                    LoopXformKind::Peel => assert_eq!(cycle, 14),
                    LoopXformKind::Unroll => assert_eq!(cycle, 14 * (k + 1)),
                }
                assert_eq!(x.poll_free_bytes, 14 * (k + 1));
                assert!(x.poll_free_bytes <= LOOP_XFORM_MAX_POLL_FREE_BYTES);

                // Dynamic half: count the polls actually executed. Peeling
                // moves k iterations out of the loop, so it drops exactly k
                // polls, once, and the steady state still polls per iteration.
                // Unrolling polls once per group, so the poll count falls by
                // at most the factor — never to zero for a long-running loop,
                // which is what "time to safepoint stays bounded" means.
                for n in 0..=9i32 {
                    let locals = [n, 0, 0];
                    let t = interp(&x.code, x.code_len, Some(&x.bci_of), &locals, &[]);
                    let polls = poll_count(&t, 18);
                    // Widening: loop trip count is non-negative here
                    let trips = n as usize;
                    match x.kind {
                        LoopXformKind::Peel => assert_eq!(polls, trips.saturating_sub(k)),
                        LoopXformKind::Unroll => assert_eq!(polls, trips / (k + 1)),
                    }
                    assert!(
                        trips <= polls * (k + 1) + 2 * (k + 1),
                        "{:?} k = {k}, n = {n}: {trips} iterations for {polls} polls",
                        x.kind
                    );
                }
            }
        }
    }

    #[test]
    fn a_backward_goto_w_is_an_unpolled_cycle_and_is_refused() {
        // The model is not vacuous: `goto_w` is a branch the emitter has no
        // poll for, so a backward one is a cycle that can never reach a
        // safepoint. `jit_scan` rejects `goto_w` today; this pins the reason.
        let spin = vec![0xc8, 0x00, 0x00, 0x00, 0x00, 0xb1];
        assert!(!all_backward_edges_are_polled(&spin, spin.len()));
        assert!(!poll_bearing_opcode(0xc8));

        // A method containing one is refused outright rather than duplicated.
        let code = shape_with_goto_w();
        assert_eq!(
            plan_loop_unroll(&code, code.len(), 0, 5, 1, &[]),
            Err(LoopXformRefusal::OpaqueControlFlow)
        );
    }

    #[test]
    fn an_irreducible_loop_is_refused() {
        let code = shape_irreducible();
        let len = code.len();
        // The second entry into the cycle IS the branch that breaks the
        // header's domination of its own region, so the single-entry scan
        // names it first. Both are refusals; neither transforms.
        assert_eq!(
            plan_loop_unroll(&code, len, 7, 17, 1, &[]),
            Err(LoopXformRefusal::ExternalEntry)
        );
        assert_eq!(
            plan_loop_peel(&code, len, 7, 17, 1, &[]),
            Err(LoopXformRefusal::ExternalEntry)
        );
        // …and the dominator machinery agrees on why: the header does not
        // dominate the other entry block.
        let cfg = MethodCfg::build(&code, len).expect("cfg builds");
        let h = cfg.node_of(7).expect("header node");
        let l2 = cfg.node_of(13).expect("L2 node");
        assert!(!cfg.dominates(h, l2));
        // A reducible loop's header does dominate its region.
        let a = shape_a();
        let acfg = MethodCfg::build(&a, a.len()).expect("cfg builds");
        let ah = acfg.node_of(4).expect("header node");
        for &pc in acfg.nodes() {
            if (4..21).contains(&pc) {
                let n = acfg.node_of(pc).expect("region node");
                assert!(acfg.dominates(ah, n), "header should dominate {pc}");
            }
        }
    }

    #[test]
    fn an_irreducible_inner_cycle_is_refused() {
        let code = shape_irreducible_inner();
        let len = code.len();
        // The outer loop is single-entry and its header dominates the whole
        // region, so only the inner reducibility test can reject this.
        let cfg = MethodCfg::build(&code, len).expect("cfg builds");
        let h = cfg.node_of(2).expect("outer header");
        for &pc in cfg.nodes() {
            if (2..33).contains(&pc) {
                let n = cfg.node_of(pc).expect("region node");
                assert!(cfg.dominates(h, n), "outer header should dominate {pc}");
            }
        }
        assert_eq!(
            plan_loop_unroll(&code, len, 2, 30, 1, &[]),
            Err(LoopXformRefusal::IrreducibleInnerLoop)
        );
        assert_eq!(
            plan_loop_peel(&code, len, 2, 30, 2, &[]),
            Err(LoopXformRefusal::IrreducibleInnerLoop)
        );
    }

    #[test]
    fn exception_order_is_unchanged() {
        let code = shape_throws();
        let len = code.len();
        let heap = vec![0i32; 6];
        // The fixture throws where we think it does, after the stores that
        // precede it.
        let base = interp(&code, len, None, &[0, 6, 0], &heap);
        assert_eq!(base.outcome, Outcome::Throw("ArithmeticException", 14));
        assert_eq!(base.heap, vec![-33, -50, -100, 0, 0, 0]);

        for k in 1..=3usize {
            for x in [
                plan_loop_peel(&code, len, 2, 19, k, &[]).expect("peel"),
                plan_loop_unroll(&code, len, 2, 19, k, &[]).expect("unroll"),
            ] {
                // n = 0..3 completes; n >= 4 throws ArithmeticException at
                // i == 3; n > 6 would throw ArrayIndexOutOfBounds first if the
                // divide were reordered past the store.
                for n in 0..=9i32 {
                    let locals = [0, n, 0];
                    let base = interp(&code, len, None, &locals, &heap);
                    let t = interp(&x.code, x.code_len, Some(&x.bci_of), &locals, &heap);
                    assert_eq!(t.outcome, base.outcome, "{:?} k = {k}, n = {n}", x.kind);
                    assert_eq!(t.heap, base.heap, "{:?} k = {k}, n = {n}", x.kind);
                    assert_eq!(
                        steps_without_back_edge(&t, 19),
                        steps_without_back_edge(&base, 19),
                        "{:?} k = {k}, n = {n}",
                        x.kind
                    );
                }
            }
        }
    }

    #[test]
    fn deopt_into_a_transformed_loop_resolves_its_locals() {
        let code = shape_a();
        let len = code.len();
        let orig_starts = instruction_start_map(&code, len);
        for k in 1..=3usize {
            for x in [
                plan_loop_peel(&code, len, 4, 18, k, &[]).expect("peel"),
                plan_loop_unroll(&code, len, 4, 18, k, &[]).expect("unroll"),
            ] {
                // Provenance is TOTAL: every output byte names an original
                // bci, so no output PC can deopt into a hole.
                assert!(x.provenance_is_total());
                let out_starts = instruction_start_map(&x.code, x.code_len);
                for (p, &is_start) in out_starts.iter().enumerate() {
                    if !is_start {
                        continue;
                    }
                    let bci = x.bci_at(p).expect("provenance for an instruction start");
                    assert!(
                        orig_starts[bci],
                        "{:?} k = {k}: output pc {p} maps to {bci}, not an instruction start",
                        x.kind
                    );
                }
                // …and the frame at that bci is the frame the interpreter
                // would have had: the whole (bci, locals) sequence matches.
                for n in 0..=5i32 {
                    let locals = [n, 0, 0];
                    let base = interp(&code, len, None, &locals, &[]);
                    let t = interp(&x.code, x.code_len, Some(&x.bci_of), &locals, &[]);
                    assert_eq!(
                        steps_without_back_edge(&t, 18),
                        steps_without_back_edge(&base, 18),
                        "{:?} k = {k}, n = {n}",
                        x.kind
                    );
                }
            }
        }
    }

    #[test]
    fn osr_entry_targets_the_steady_state_copy() {
        let code = shape_a();
        let len = code.len();
        let peel = plan_loop_peel(&code, len, 4, 18, 2, &[]).expect("peel");
        // A bci inside the body has k + 1 images. Entering a PEELED copy from
        // OSR would re-run the peeled iterations, so the answer must be the
        // last copy — the one the back edge re-enters.
        assert_eq!(peel.steady_state_base(), 4 + 2 * 14);
        assert_eq!(peel.osr_entry_pc(4), Some(4 + 2 * 14));
        assert_eq!(peel.osr_entry_pc(9), Some(9 + 2 * 14));
        assert_eq!(peel.osr_entry_pc(0), Some(0));
        assert_eq!(peel.osr_entry_pc(21), Some(21 + 2 * 14));
        assert_eq!(peel.osr_entry_pc(23), None);

        // For unroll every copy is a full iteration, so the first one — the
        // back edge's target — is the right entry.
        let unroll = plan_loop_unroll(&code, len, 4, 18, 2, &[]).expect("unroll");
        assert_eq!(unroll.steady_state_base(), 4);
        assert_eq!(unroll.osr_entry_pc(4), Some(4));
        assert_eq!(unroll.osr_entry_pc(9), Some(9));
        assert_eq!(unroll.osr_entry_pc(21), Some(21 + 2 * 14));

        // Round trip: the OSR entry for a bci carries that bci.
        for x in [&peel, &unroll] {
            for bci in [0usize, 4, 9, 15, 21] {
                let p = x.osr_entry_pc(bci).expect("osr entry");
                assert_eq!(x.bci_at(p), Some(bci), "{:?} bci {bci}", x.kind);
            }
        }
    }

    #[test]
    fn peeling_removes_the_preheader_bypass_from_the_steady_state_loop() {
        let code = shape_b();
        let len = code.len();
        // The original is exactly the shape the pre-header bypass guard
        // rejects: a branch from outside lands on the header, after the
        // pre-header the emitter would have placed there.
        let loops = detect_loops(&code, len);
        assert!(find_bypassable_loop_headers(&code, len, &loops, &[]).contains(&11));

        // Peeling moves the external entry onto the PEELED copy, so the
        // steady-state loop has no entry but its own back edge and the
        // fall-through: a hoist the guard had to drop is legal again. At every
        // factor the planner can ask for, not just one.
        for k in 1..=3usize {
            let peel = plan_loop_peel(&code, len, 11, 25, k, &[]).expect("peel");
            let ploops = detect_loops(&peel.code, peel.code_len);
            assert_eq!(ploops, vec![(peel.steady_state_base(), peel.back_edge_pc())], "k={k}");
            assert_eq!(peel.steady_state_base(), 11 + k * 14, "k={k}");
            assert!(
                find_bypassable_loop_headers(&peel.code, peel.code_len, &ploops, &[]).is_empty(),
                "k={k}"
            );
        }

        // Unrolling does NOT: its first copy IS the header, so the external
        // edge still lands on the loop. Stated so nobody assumes otherwise.
        let unroll = plan_loop_unroll(&code, len, 11, 25, 1, &[]).expect("unroll");
        let uloops = detect_loops(&unroll.code, unroll.code_len);
        assert!(find_bypassable_loop_headers(&unroll.code, unroll.code_len, &uloops, &[])
            .contains(&11));
    }

    #[test]
    fn exception_ranges_are_refused_or_widened() {
        let code = shape_a();
        let len = code.len();
        // A handler inside the region would need duplicating with it.
        assert_eq!(
            plan_loop_peel(&code, len, 4, 18, 1, &[(9, 15, 12)]),
            Err(LoopXformRefusal::HandlerInRegion)
        );
        // A protected range that covers only part of the loop cannot be
        // mapped onto the copies.
        assert_eq!(
            plan_loop_peel(&code, len, 4, 18, 1, &[(0, 10, 22)]),
            Err(LoopXformRefusal::HandlerRangeStraddlesRegion)
        );
        // A range that encloses the loop is widened to cover every copy.
        let x = plan_loop_peel(&code, len, 4, 18, 1, &[(0, 21, 21)]).expect("enclosing range");
        assert_eq!(x.exception_ranges, vec![(0, 35, 35)]);
        // A range entirely before the loop does not move.
        let y = plan_loop_peel(&code, len, 4, 18, 1, &[(0, 4, 2)]).expect("range before the loop");
        assert_eq!(y.exception_ranges, vec![(0, 4, 2)]);
    }

    #[test]
    fn shapes_the_rewriter_refuses_to_guess_at() {
        let code = shape_a();
        let len = code.len();
        // A `do { } while` back edge is the exit test: duplicating the body
        // without it would run the body k + 1 times per test.
        let dw = shape_do_while();
        assert_eq!(
            plan_loop_unroll(&dw, dw.len(), 2, 7, 1, &[]),
            Err(LoopXformRefusal::ConditionalBackEdge)
        );
        // Copy-count bounds are what keeps time-to-safepoint bounded.
        assert_eq!(
            plan_loop_unroll(&code, len, 4, 18, 0, &[]),
            Err(LoopXformRefusal::TooManyCopies)
        );
        assert_eq!(
            plan_loop_unroll(&code, len, 4, 18, LOOP_XFORM_MAX_COPIES + 1, &[]),
            Err(LoopXformRefusal::TooManyCopies)
        );
        // The pair must actually be a loop.
        assert_eq!(
            plan_loop_peel(&code, len, 4, 6, 1, &[]),
            Err(LoopXformRefusal::ConditionalBackEdge)
        );
        assert_eq!(
            plan_loop_peel(&code, len, 9, 18, 1, &[]),
            Err(LoopXformRefusal::NotABackEdge)
        );
    }

    #[test]
    fn the_rewrite_is_layout_consistent() {
        let code = shape_a();
        let len = code.len();
        for k in 1..=LOOP_XFORM_MAX_COPIES {
            for x in [
                plan_loop_peel(&code, len, 4, 18, k, &[]).expect("peel"),
                plan_loop_unroll(&code, len, 4, 18, k, &[]).expect("unroll"),
            ] {
                assert_eq!(x.code_len, len + k * 14);
                assert_eq!(x.code.len(), x.code_len);
                assert_eq!(x.back_edge_pc(), 4 + (k + 1) * 14);
                assert_eq!(x.code[x.back_edge_pc()], 0xa7);
                // The prefix never moves, so the header PC is stable and the
                // copies are byte-identical apart from their branch offsets.
                assert_eq!(&x.code[..4], &code[..4]);
                for ci in 0..=k {
                    let base = 4 + ci * 14;
                    assert_eq!(x.code[base], 0x1b, "copy {ci} must start at the header");
                    assert_eq!(x.bci_at(base), Some(4));
                }
                // Every copy's exit branch leaves to the (shifted) exit.
                for ci in 0..=k {
                    let at = 4 + ci * 14 + 2;
                    let off = i16::from_be_bytes([x.code[at + 1], x.code[at + 2]]) as isize;
                    assert_eq!(at as isize + off, (21 + k * 14) as isize);
                }
            }
        }
    }

    // ── Switch fixtures ──────────────────────────────────────────────
    //
    // Four positions for one `lookupswitch` (zero pairs, so it is 8 operand
    // bytes plus its padding), chosen to separate the two PC-relative facts
    // precondition 6 is about: the padding, which depends on the switch's own
    // PC, and the 4-byte jump offsets, which this rewriter never re-encodes.
    // The reference interpreter does not implement `lookupswitch`, so these
    // are used by the STATIC tests only — the step-sequence equivalence tests
    // keep running on the switch-free shapes.

    /// A `lookupswitch` before the loop whose only target is the loop header.
    /// Neither the switch nor its target moves, so the rewrite leaves it
    /// byte-identical and it is admitted. Header 12, back edge 26, length 31 —
    /// the same 14-byte body as [`shape_a`].
    fn shape_switch_before_loop() -> Vec<u8> {
        vec![
            0xab, // 0: lookupswitch
            0x00, 0x00, 0x00, // 1: padding to the 4-byte boundary
            0x00, 0x00, 0x00, 0x0c, // 4: default = +12 -> 12 (the header)
            0x00, 0x00, 0x00, 0x00, // 8: npairs = 0
            0x1b, // 12: iload_1          <- header
            0x1a, // 13: iload_0
            0xa2, 0x00, 0x0f, // 14: if_icmpge 29
            0x1c, // 17: iload_2
            0x1b, // 18: iload_1
            0x05, // 19: iconst_2
            0x68, // 20: imul
            0x60, // 21: iadd
            0x3d, // 22: istore_2
            0x84, 0x01, 0x01, // 23: iinc 1, 1
            0xa7, 0xff, 0xf2, // 26: goto 12     <- back edge
            0x1c, // 29: iload_2
            0xac, // 30: ireturn
        ]
    }

    /// A `lookupswitch` AFTER the loop, at a PC that is already 4-byte aligned
    /// (`switch_pad(19) == 0`). It shifts by `delta = k * 14`, so its padding
    /// survives only when `k` is even — the case the old "no switch anywhere"
    /// rule could not distinguish. Header 2, back edge 16, length 29.
    fn shape_switch_after_loop() -> Vec<u8> {
        vec![
            0x03, // 0: iconst_0
            0x3c, // 1: istore_1
            0x1b, // 2: iload_1           <- header
            0x1a, // 3: iload_0
            0xa2, 0x00, 0x0f, // 4: if_icmpge 19
            0x84, 0x01, 0x01, // 7: iinc 1, 1
            0x84, 0x01, 0x01, // 10: iinc 1, 1
            0x00, // 13: nop
            0x00, // 14: nop
            0x00, // 15: nop
            0xa7, 0xff, 0xf2, // 16: goto 2      <- back edge
            0xab, // 19: lookupswitch (no padding — 20 is already aligned)
            0x00, 0x00, 0x00, 0x09, // 20: default = +9 -> 28
            0x00, 0x00, 0x00, 0x00, // 24: npairs = 0
            0xb1, // 28: return
        ]
    }

    /// A `lookupswitch` INSIDE the loop body. Every copy would sit at a
    /// different PC, so the copies do not agree on their padding, and each
    /// copy's targets would have to be relocated in a 4-byte field the
    /// rewriter does not touch. Header 2, back edge 19, length 23.
    fn shape_switch_in_body() -> Vec<u8> {
        vec![
            0x03, // 0: iconst_0
            0x3c, // 1: istore_1
            0x1b, // 2: iload_1           <- header
            0x1a, // 3: iload_0
            0xa2, 0x00, 0x12, // 4: if_icmpge 22
            0xab, // 7: lookupswitch (no padding — 8 is already aligned)
            0x00, 0x00, 0x00, 0x09, // 8: default = +9 -> 16
            0x00, 0x00, 0x00, 0x00, // 12: npairs = 0
            0x84, 0x01, 0x01, // 16: iinc 1, 1
            0xa7, 0xff, 0xef, // 19: goto 2       <- back edge
            0xb1, // 22: return
        ]
    }

    /// A `lookupswitch` before the loop that branches ACROSS it. The switch
    /// does not move but its target does, so its un-rewritten 4-byte offset
    /// would name the wrong instruction. Header 16, back edge 27, length 32.
    /// Everything else about this method is admissible, which is what makes it
    /// a test of the displacement rule specifically.
    fn shape_switch_over_the_loop() -> Vec<u8> {
        vec![
            0x03, // 0: iconst_0
            0x3c, // 1: istore_1
            0x1a, // 2: iload_0
            0x99, 0x00, 0x0d, // 3: ifeq 16      -> the header
            0xab, // 6: lookupswitch
            0x00, // 7: padding to the 4-byte boundary
            0x00, 0x00, 0x00, 0x18, // 8: default = +24 -> 30 (past the loop)
            0x00, 0x00, 0x00, 0x00, // 12: npairs = 0
            0x1b, // 16: iload_1          <- header
            0x1a, // 17: iload_0
            0xa2, 0x00, 0x0c, // 18: if_icmpge 30
            0x84, 0x01, 0x01, // 21: iinc 1, 1
            0x00, // 24: nop
            0x00, // 25: nop
            0x00, // 26: nop
            0xa7, 0xff, 0xf5, // 27: goto 16     <- back edge
            0x1b, // 30: iload_1
            0xac, // 31: ireturn
        ]
    }

    /// Byte offset of the low byte of `shape_switch_over_the_loop`'s default
    /// offset, so a test can retarget it without re-spelling the fixture.
    const SWITCH_OVER_DEFAULT_LOW_BYTE: usize = 11;

    /// Every fixture the rewriter is expected to admit, as
    /// `(name, code, header, back_edge)`.
    fn admissible_fixtures() -> Vec<(&'static str, Vec<u8>, usize, usize)> {
        vec![
            ("shape_a", shape_a(), 4, 18),
            ("shape_b", shape_b(), 11, 25),
            ("shape_throws", shape_throws(), 2, 19),
            (
                "shape_switch_before_loop",
                shape_switch_before_loop(),
                12,
                26,
            ),
            // Admitted at even `k` only — see the fixture's doc comment.
            ("shape_switch_after_loop", shape_switch_after_loop(), 2, 16),
        ]
    }

    /// The hand-assembled switch fixtures decode the way every test below
    /// assumes: the walk lands exactly on the end, the switch is where and as
    /// long as claimed, its targets are the claimed ones, and the method is a
    /// buildable CFG with one loop and every cycle polled. Without this, a
    /// mis-typed padding byte would silently turn a refusal test into a test
    /// of a malformed method.
    #[test]
    fn the_switch_fixtures_decode_the_way_these_tests_assume() {
        for (name, code, header, back_edge, sw, sw_len, sw_targets) in [
            (
                "before",
                shape_switch_before_loop(),
                12usize,
                26usize,
                0usize,
                12usize,
                vec![12usize],
            ),
            ("after", shape_switch_after_loop(), 2, 16, 19, 9, vec![28]),
            ("in body", shape_switch_in_body(), 2, 19, 7, 9, vec![16]),
            (
                "over",
                shape_switch_over_the_loop(),
                16,
                27,
                6,
                10,
                vec![30],
            ),
        ] {
            let len = code.len();
            let starts = instruction_start_map(&code, len);
            let mut pc = 0usize;
            while pc < len {
                assert!(starts[pc], "{name}: {pc} is not an instruction start");
                let l = bytecode_len_at(&code, pc);
                assert!(l > 0, "{name}: zero-length instruction at {pc}");
                pc += l;
            }
            assert_eq!(pc, len, "{name}: the walk overran the fixture");

            assert!(matches!(code[sw], 0xaa | 0xab), "{name}: no switch at {sw}");
            assert_eq!(bytecode_len_at(&code, sw), sw_len, "{name}: switch length");
            let mut t: Vec<usize> = Vec::new();
            assert!(
                branch_targets_at(&code, sw, len, &mut t),
                "{name}: switch does not decode"
            );
            assert_eq!(t, sw_targets, "{name}: switch targets");

            assert_eq!(
                detect_loops(&code, len),
                vec![(header, back_edge)],
                "{name}"
            );
            assert_eq!(code[back_edge], 0xa7, "{name}: back edge is not a goto");
            assert!(MethodCfg::build(&code, len).is_some(), "{name}: cfg builds");
            assert!(all_backward_edges_are_polled(&code, len), "{name}");
        }
    }

    /// `bci_at` is TOTAL, and what it answers is a real instruction of the
    /// original method.
    ///
    /// This is the safety property the whole rewrite rests on. Every deopt
    /// bci, every oop-map `bytecode_pc` and every exception range recorded by
    /// a compiled method has to translate through this map; a pc it cannot
    /// answer for, or answers with a byte that is not an instruction start,
    /// is a frame the interpreter cannot resume and an oop-map the GC cannot
    /// match. Proved here over every admissible fixture, both kinds and every
    /// legal factor, not just `shape_a` at `k <= 3`.
    #[test]
    fn bci_at_is_total_over_every_fixture_and_factor() {
        let mut admitted_total = 0usize;
        for (name, code, header, back_edge) in admissible_fixtures() {
            let len = code.len();
            let orig_starts = instruction_start_map(&code, len);
            let mut admitted_here = 0usize;
            for k in 1..=LOOP_XFORM_MAX_COPIES {
                for planned in [
                    plan_loop_peel(&code, len, header, back_edge, k, &[]),
                    plan_loop_unroll(&code, len, header, back_edge, k, &[]),
                ] {
                    // A refusal publishes no map, so there is nothing for the
                    // property to hold over. `shape_switch_after_loop` refuses
                    // at odd `k`; see
                    // `a_switch_outside_the_rewritten_region_no_longer_refuses`.
                    let x = match planned {
                        Ok(x) => x,
                        Err(_) => continue,
                    };
                    admitted_here += 1;
                    admitted_total += 1;
                    let what = format!("{name} {:?} k={k}", x.kind);

                    // 1. The map covers the output byte for byte.
                    assert!(x.provenance_is_total(), "{what}");
                    assert_eq!(x.code.len(), x.code_len, "{what}");
                    assert_eq!(x.bci_of.len(), x.code_len, "{what}");

                    // 2. THE PROPERTY: every output pc that begins an
                    //    instruction resolves, and resolves to a bci that
                    //    begins an instruction in the ORIGINAL.
                    let out_starts = instruction_start_map(&x.code, x.code_len);
                    for (p, &is_start) in out_starts.iter().enumerate() {
                        if !is_start {
                            continue;
                        }
                        assert!(
                            x.bci_at(p).is_some(),
                            "{what}: no provenance for instruction start {p}"
                        );
                        let bci = x.bci_at(p).unwrap_or(usize::MAX);
                        assert!(bci < len, "{what}: pc {p} maps to {bci}, past the original");
                        assert!(
                            orig_starts[bci],
                            "{what}: pc {p} maps to {bci}, which is mid-instruction"
                        );
                    }

                    // 3. …and the instruction found there is the SAME
                    //    instruction: same opcode, same length, and every
                    //    interior byte maps to the matching interior byte. So
                    //    a pc that is NOT an instruction start cannot resolve
                    //    to a plausible-looking bci of some other instruction.
                    let mut pc = 0usize;
                    while pc < x.code_len {
                        let bci = x.bci_at(pc).unwrap_or(usize::MAX);
                        assert!(bci < len, "{what}: pc {pc} has no provenance");
                        assert_eq!(x.code[pc], code[bci], "{what}: pc {pc} vs bci {bci}");
                        let l = bytecode_len_at(&x.code, pc);
                        assert!(l > 0, "{what}: zero-length instruction at {pc}");
                        assert_eq!(
                            l,
                            bytecode_len_at(&code, bci),
                            "{what}: pc {pc} and bci {bci} disagree on length"
                        );
                        for d in 0..l {
                            assert_eq!(x.bci_at(pc + d), Some(bci + d), "{what}: pc {pc} + {d}");
                        }
                        pc += l;
                    }
                    assert_eq!(pc, x.code_len, "{what}: the output walk overran");

                    // 4. The poll proof, re-checked on this output too — every
                    //    fixture and factor, not just `shape_a`.
                    assert!(all_backward_edges_are_polled(&x.code, x.code_len), "{what}");
                    assert!(
                        emits_safepoint_poll_at(&x.code, x.back_edge_pc(), x.code_len),
                        "{what}: the back edge lost its poll"
                    );

                    // 5. `osr_entry_pc` is a partial INVERSE of `bci_at`
                    //    wherever it answers at all — including at bytes that
                    //    are not instruction starts.
                    for bci in 0..len {
                        if let Some(entry) = x.osr_entry_pc(bci) {
                            assert!(
                                entry < x.code_len,
                                "{what}: osr {bci} -> {entry}, past the end"
                            );
                            assert_eq!(x.bci_at(entry), Some(bci), "{what}: osr {bci} -> {entry}");
                        }
                    }
                }
            }
            assert!(
                admitted_here > 0,
                "{name}: never admitted, so the property held vacuously"
            );
        }
        // Three fixtures are admitted for both kinds at every factor, so the
        // property was exercised, not skipped.
        assert!(
            admitted_total >= 6 * LOOP_XFORM_MAX_COPIES,
            "only {admitted_total} plans checked"
        );
    }

    /// OSR must REFUSE the unrolled back-edge gap, not answer it.
    ///
    /// The back-edge `goto` is emitted in the last copy only, and unroll's
    /// steady state is copy 0, which stops just before it. The old answer,
    /// `Some(bci)`, named output pc `header + body_len` — the first byte of
    /// copy 1, i.e. the header — so an OSR entry there resumed a "back edge
    /// next" frame at the top of a fresh body and ran an extra iteration.
    /// `outputs_for_bci` is an exact inverse of `bci_at`, and one-to-many
    /// inside the rewritten region.
    ///
    /// This is the property a side-table replication pass rests on. If an
    /// output PC were missing from its bci's image list, that bci's entry in
    /// one of the 21 pc-keyed tables would not be replicated into that copy,
    /// and the copy would silently lose a field resolution or an inline cache.
    #[test]
    fn outputs_for_bci_is_the_exact_inverse_of_bci_at() {
        for (name, code, header, back_edge) in admissible_fixtures() {
            let len = code.len();
            for k in 1..=LOOP_XFORM_MAX_COPIES {
                for planned in [
                    plan_loop_peel(&code, len, header, back_edge, k, &[]),
                    plan_loop_unroll(&code, len, header, back_edge, k, &[]),
                ] {
                    let Ok(x) = planned else { continue };
                    // Forward then back.
                    for pc in 0..x.code.len() {
                        let Some(bci) = x.bci_at(pc) else { continue };
                        assert!(
                            x.outputs_for_bci(bci).contains(&pc),
                            "{name} k={k}: output {pc} maps to bci {bci} but is missing \
                             from its image list"
                        );
                    }
                    // Back then forward.
                    for bci in 0..len {
                        for pc in x.outputs_for_bci(bci) {
                            assert_eq!(x.bci_at(pc), Some(bci), "{name} k={k}");
                        }
                    }
                    // The header survives, so it has at least one image.
                    assert!(
                        !x.outputs_for_bci(header).is_empty(),
                        "{name} k={k}: the header must have an image"
                    );
                }
            }
        }
    }
    /// Every entry of a replicated table lands on a PC that maps back to the
    /// entry's original bci, and a body entry appears once per copy.
    ///
    /// The count property is the one that matters: a copy missing its entry
    /// is silent — it loses a field resolution or an inline cache and takes a
    /// different lowering path than its siblings, with nothing to fail on.
    #[test]
    fn a_replicated_side_table_covers_every_copy() {
        for (name, code, header, back_edge) in admissible_fixtures() {
            let len = code.len();
            for k in 1..=LOOP_XFORM_MAX_COPIES {
                for planned in [
                    plan_loop_peel(&code, len, header, back_edge, k, &[]),
                    plan_loop_unroll(&code, len, header, back_edge, k, &[]),
                ] {
                    let Ok(x) = planned else { continue };
                    // One entry per original instruction start, payload = its bci.
                    let table: Vec<(usize, usize)> =
                        (0..len).filter(|&pc| x.bci_at(pc).is_some() || pc < len)
                            .map(|pc| (pc, pc))
                            .collect();
                    let rep = x.replicate_pc_keyed(&table);

                    // Sorted, as every consumer assumes.
                    assert!(
                        rep.windows(2).all(|w| w[0].0 <= w[1].0),
                        "{name} k={k}: replicated table is not sorted by pc"
                    );

                    // Every replicated entry sits on a PC that maps back to it.
                    for (pc, orig) in &rep {
                        assert_eq!(
                            x.bci_at(*pc),
                            Some(*orig),
                            "{name} k={k}: entry moved to {pc}, which is not an image of {orig}"
                        );
                    }

                    // A body entry appears exactly once per image — the count
                    // property a missed replication would break.
                    for (pc, _) in &table {
                        let images = x.outputs_for_bci(*pc).len();
                        let landed = rep.iter().filter(|(_, o)| o == pc).count();
                        assert_eq!(
                            landed, images,
                            "{name} k={k}: bci {pc} has {images} images but {landed} entries"
                        );
                    }
                }
            }
        }
    }
    /// The rebuilt `pc_to_native` sends every original bci to a native offset
    /// belonging to the image OSR would enter, and marks the unrolled
    /// back-edge gap unmapped.
    ///
    /// Picking any other image would re-run iterations, which is the exact bug
    /// `osr_entry_pc` was fixed for; this asserts the rebuild inherits that
    /// choice rather than making its own.
    #[test]
    fn rebuilt_pc_to_native_follows_the_osr_entry_image() {
        for (name, code, header, back_edge) in admissible_fixtures() {
            let len = code.len();
            for k in 1..=LOOP_XFORM_MAX_COPIES {
                for planned in [
                    plan_loop_peel(&code, len, header, back_edge, k, &[]),
                    plan_loop_unroll(&code, len, header, back_edge, k, &[]),
                ] {
                    let Ok(x) = planned else { continue };
                    // Synthetic: native offset = output pc * 4, so a wrong
                    // image is visible as a wrong number rather than a crash.
                    let out: Vec<i32> =
                        (0..x.code.len()).map(|pc| (pc as i32) * 4).collect();
                    let rebuilt = x.rebuild_pc_to_native(&out, len);
                    assert_eq!(rebuilt.len(), len + 1, "{name} k={k}: length");
                    for bci in 0..=len {
                        match x.osr_entry_pc(bci) {
                            Some(image) if image < out.len() => assert_eq!(
                                rebuilt[bci], out[image],
                                "{name} k={k}: bci {bci} must use image {image}"
                            ),
                            _ => assert_eq!(
                                rebuilt[bci], -1,
                                "{name} k={k}: bci {bci} has no valid entry image and \
                                 must stay unmapped"
                            ),
                        }
                    }
                    // The unrolled back-edge gap is unmapped, not mapped to
                    // the header — entering there would re-run iterations.
                    if matches!(x.kind, LoopXformKind::Unroll) {
                        assert_eq!(
                            rebuilt[x.orig_back_edge_pc()],
                            -1,
                            "{name} k={k}: the unrolled back-edge gap must be unmapped"
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn osr_refuses_the_unrolled_back_edge_gap_and_answers_the_steady_state_elsewhere() {
        let code = shape_a();
        let len = code.len();
        let (header, back_edge, body_len) = (4usize, 18usize, 14usize);
        let back_edge_end = back_edge + 3;
        for k in 1..=LOOP_XFORM_MAX_COPIES {
            let peel = plan_loop_peel(&code, len, header, back_edge, k, &[]).expect("peel");
            let unroll = plan_loop_unroll(&code, len, header, back_edge, k, &[]).expect("unroll");
            assert_eq!(unroll.orig_back_edge_pc(), back_edge, "k={k}");
            assert_eq!(peel.orig_back_edge_pc(), back_edge, "k={k}");

            for bci in back_edge..back_edge_end {
                assert_eq!(unroll.osr_entry_pc(bci), None, "k={k} bci={bci}");
                // Why: the pc the old code answered carries a DIFFERENT
                // instruction's provenance.
                assert_ne!(unroll.bci_at(bci), Some(bci), "k={k} bci={bci}");
                // Peel's steady-state copy carries the back edge, so it keeps
                // an entry there and that entry round-trips.
                let p = peel.osr_entry_pc(bci).expect("peel has no gap");
                assert_eq!(p, bci + k * body_len, "k={k} bci={bci}");
                assert_eq!(peel.bci_at(p), Some(bci), "k={k} bci={bci}");
            }
            assert_eq!(unroll.bci_at(back_edge), Some(header), "k={k}");
            assert_eq!(peel.bci_at(peel.back_edge_pc()), Some(back_edge), "k={k}");

            // The gap is the ONLY refusal inside the method …
            for bci in 0..len {
                if unroll.osr_entry_pc(bci).is_none() {
                    assert!(
                        (back_edge..back_edge_end).contains(&bci),
                        "k={k}: bci {bci} refused outside the back-edge gap"
                    );
                }
                assert!(
                    peel.osr_entry_pc(bci).is_some(),
                    "k={k}: peel refused {bci}"
                );
            }
            // … and everywhere else the answer is the steady-state copy,
            // exactly as before this fix.
            for bci in header..back_edge {
                assert_eq!(unroll.osr_entry_pc(bci), Some(bci), "k={k} bci={bci}");
                assert_eq!(
                    peel.osr_entry_pc(bci),
                    Some(bci + k * body_len),
                    "k={k} bci={bci}"
                );
                let entry = unroll.osr_entry_pc(bci).unwrap_or(usize::MAX);
                assert!(
                    entry >= unroll.steady_state_base()
                        && entry < unroll.steady_state_base() + body_len,
                    "k={k} bci={bci}: {entry} is outside unroll's steady-state copy"
                );
            }
            assert_eq!(unroll.osr_entry_pc(0), Some(0), "k={k}");
            assert_eq!(peel.osr_entry_pc(0), Some(0), "k={k}");
            assert_eq!(unroll.osr_entry_pc(21), Some(21 + k * body_len), "k={k}");
            assert_eq!(unroll.osr_entry_pc(len), None, "k={k}");
        }
    }

    /// `SwitchInMethod` is now a per-switch question, not a per-method one: a
    /// switch the rewrite does not disturb is admitted. See precondition 6.
    #[test]
    fn a_switch_outside_the_rewritten_region_no_longer_refuses() {
        // (a) Before the loop, branching to the header. Neither the switch nor
        // its target moves, so its bytes are still a faithful encoding.
        let before = shape_switch_before_loop();
        let blen = before.len();
        assert_eq!(switch_pad(0), 3);
        for k in 1..=3usize {
            for x in [
                plan_loop_peel(&before, blen, 12, 26, k, &[]).expect("switch before the loop"),
                plan_loop_unroll(&before, blen, 12, 26, k, &[]).expect("switch before the loop"),
            ] {
                assert_eq!(&x.code[..12], &before[..12], "{:?} k={k}", x.kind);
                assert_eq!(bytecode_len_at(&x.code, 0), 12, "{:?} k={k}", x.kind);
                let mut t: Vec<usize> = Vec::new();
                assert!(branch_targets_at(&x.code, 0, x.code_len, &mut t));
                assert_eq!(
                    t,
                    vec![12],
                    "{:?} k={k}: the switch must still enter the header",
                    x.kind
                );
                assert_eq!(x.bci_at(12), Some(12), "{:?} k={k}", x.kind);
                assert!(all_backward_edges_are_polled(&x.code, x.code_len));
            }
        }

        // (b) After the loop it moves by `delta = 14k`, so it survives exactly
        // when the padding RECOMPUTED at the shifted pc is unchanged — which
        // for this fixture means even `k`. This is the case a "does it move at
        // all" rule cannot decide and the reason the check recomputes padding.
        let after = shape_switch_after_loop();
        let alen = after.len();
        assert_eq!(switch_pad(19), 0);
        for k in 1..=4usize {
            let planned = plan_loop_unroll(&after, alen, 2, 16, k, &[]);
            if k % 2 == 0 {
                let x = planned.expect("even k preserves the padding");
                let sw = 19 + k * 14;
                assert_eq!(switch_pad(sw), switch_pad(19), "k={k}");
                assert_eq!(x.code[sw], 0xab, "k={k}");
                assert_eq!(bytecode_len_at(&x.code, sw), 9, "k={k}");
                assert_eq!(x.bci_at(sw), Some(19), "k={k}");
                let mut t: Vec<usize> = Vec::new();
                assert!(branch_targets_at(&x.code, sw, x.code_len, &mut t));
                assert_eq!(
                    t,
                    vec![28 + k * 14],
                    "k={k}: the default must follow the shift"
                );
                // The same verdict for peel — the rule is kind-independent.
                assert!(plan_loop_peel(&after, alen, 2, 16, k, &[]).is_ok(), "k={k}");
            } else {
                assert_eq!(
                    planned.unwrap_err(),
                    LoopXformRefusal::SwitchInMethod,
                    "k={k}: shifting by {} changes the padding, hence the length",
                    14 * k
                );
                assert_eq!(
                    plan_loop_peel(&after, alen, 2, 16, k, &[]).unwrap_err(),
                    LoopXformRefusal::SwitchInMethod,
                    "k={k}"
                );
            }
        }

        // (c) Inside the region it is duplicated: the copies do not agree on
        // their padding and their targets would need relocating in a field
        // this rewriter does not touch. Still refused.
        let inside = shape_switch_in_body();
        let ilen = inside.len();
        for k in 1..=3usize {
            assert_eq!(
                plan_loop_peel(&inside, ilen, 2, 19, k, &[]).unwrap_err(),
                LoopXformRefusal::SwitchInMethod,
                "k={k}"
            );
            assert_eq!(
                plan_loop_unroll(&inside, ilen, 2, 19, k, &[]).unwrap_err(),
                LoopXformRefusal::SwitchInMethod,
                "k={k}"
            );
        }

        // (d) Outside the region but branching ACROSS it: the switch does not
        // move and its target does, so its 4-byte offset would name the wrong
        // instruction. Refused — and the SAME fixture with the default
        // retargeted at the header (which does not move) is admitted, so the
        // displacement is demonstrably the only thing being refused.
        let over = shape_switch_over_the_loop();
        let olen = over.len();
        for k in 1..=3usize {
            assert_eq!(
                plan_loop_unroll(&over, olen, 16, 27, k, &[]).unwrap_err(),
                LoopXformRefusal::SwitchInMethod,
                "k={k}"
            );
        }
        let mut retargeted = over.clone();
        retargeted[SWITCH_OVER_DEFAULT_LOW_BYTE] = 0x0a; // +24 (past the loop) -> +10 (the header)
        let mut t: Vec<usize> = Vec::new();
        assert!(branch_targets_at(&retargeted, 6, olen, &mut t));
        assert_eq!(t, vec![16], "the retarget must name the header");
        for k in 1..=3usize {
            let x = plan_loop_unroll(&retargeted, olen, 16, 27, k, &[])
                .expect("a switch that branches to the header is admitted");
            assert_eq!(&x.code[..16], &retargeted[..16], "k={k}");
            assert!(all_backward_edges_are_polled(&x.code, x.code_len), "k={k}");
        }
    }

    // ── Guarded versioning ───────────────────────────────────────────────
    //
    // The transform is described in `LoopVersioning`. These tests carry its
    // four load-bearing claims: the two versions compute the same thing, the
    // guard decides which one runs and nothing else, the guard is inert enough
    // for its provenance to be the header's, and both loops still poll.

    use crate::scev::SymBound;

    /// `trip >= minimum` on [`shape_a`]'s runtime limit (`n`, local 0) — the
    /// exact shape `CountedLoop::prove_trip_count_at_least` mints for
    /// `for (i = 0; i < n; i++)`, which is the commonest loop in Java and the
    /// one whose compile-time `trip.min` is zero.
    fn trip_guard(minimum: u64) -> PreheaderGuard {
        PreheaderGuard::TripCountAtLeast {
            term: SymBound {
                base: BoundTerm::Bound(BoundSource::Local(0)),
                addend: 0,
            },
            minimum,
        }
    }

    #[test]
    fn a_versioned_loop_runs_the_same_steps_whichever_version_it_takes() {
        let code = shape_a();
        let (len, header, back_edge, body_len) = (23usize, 4usize, 18usize, 14usize);
        for k in 1..=3usize {
            for kind in [LoopXformKind::Peel, LoopXformKind::Unroll] {
                // The minimum the planner asks for: enough trips to reach every
                // copy the transform makes.
                let guard = trip_guard(k as u64 + 1);
                let x = plan_loop_version(&code, len, header, back_edge, k, &[], kind, &guard)
                    .expect("versioning is admitted");
                let v = x.versioning.as_ref().expect("versioned");
                // Layout: prefix, guard, k + 1 copies, the untouched fallback,
                // suffix.
                assert_eq!(v.guard_pc, header);
                assert_eq!(v.guard_len, 5, "iload_0; iconst_<k+1>; if_icmplt");
                assert_eq!(x.fast_base(), header + 5);
                assert_eq!(v.fallback_base, header + 5 + (k + 1) * body_len + 3);
                assert_eq!(x.code_len, len + 5 + k * body_len + body_len + 3);
                assert!(x.provenance_is_total());

                // Trip counts either side of the guard's minimum, so both
                // versions are exercised by this comparison.
                for n in 0..=8i32 {
                    let locals = [n, 0, 0];
                    let base = interp(&code, len, None, &locals, &[]);
                    assert_eq!(base.outcome, Outcome::Return(n * (n - 1)), "fixture, n={n}");
                    let t = interp(&x.code, x.code_len, Some(&x.bci_of), &locals, &[]);
                    assert_eq!(t.outcome, base.outcome, "{kind:?} k={k} n={n}");
                    assert_eq!(
                        steps_without_back_edge_or_guard(&t, back_edge, x.guard_span()),
                        steps_without_back_edge(&base, back_edge),
                        "{kind:?} k={k} n={n}: executed sequence diverged"
                    );
                }
            }
        }
    }

    #[test]
    fn the_guard_takes_the_fast_version_exactly_when_it_holds() {
        let code = shape_a();
        let (len, header, back_edge) = (23usize, 4usize, 18usize);
        let k = 3usize;
        let x = plan_loop_version(
            &code,
            len,
            header,
            back_edge,
            k,
            &[],
            LoopXformKind::Unroll,
            &trip_guard(4),
        )
        .expect("versioning is admitted");
        let fallback = x.versioning.as_ref().expect("versioned").fallback_base;
        let fallback_end = fallback + x.body_len + 3;
        for n in 0..=8i32 {
            let t = interp(&x.code, x.code_len, Some(&x.bci_of), &[n, 0, 0], &[]);
            let ran_fast = t
                .step_pcs
                .iter()
                .any(|&pc| pc >= x.fast_base() && pc < fallback);
            let ran_fallback = t
                .step_pcs
                .iter()
                .any(|&pc| pc >= fallback && pc < fallback_end);
            // `n >= 4` IS the guard, and it is the only thing that decides
            // which body runs. Without this the test above would pass just as
            // happily if the guard were never emitted at all.
            assert_eq!(ran_fast, n >= 4, "n={n}: wrong version for the guard");
            assert_eq!(ran_fallback, n < 4, "n={n}: wrong version for the guard");
            assert!(ran_fast != ran_fallback, "n={n}: both versions ran");
        }
    }

    #[test]
    fn a_versioning_guard_writes_nothing_and_balances_the_stack() {
        // This is the property that makes the guard's provenance — the header's
        // bci — sound. Nothing it emits can throw, allocate, call, poll or
        // write a local, so no deopt point, exception-check stub, bounds-check
        // stub or `athrow` can name a guard PC, and the operand stack at the
        // guard's first byte is the stack the interpreter has at the header.
        for guard in [
            trip_guard(4),
            PreheaderGuard::NonNegative(SymBound {
                base: BoundTerm::IvEntry(1),
                addend: 0,
            }),
            PreheaderGuard::AtMost {
                term: SymBound {
                    base: BoundTerm::Bound(BoundSource::Local(2)),
                    addend: -1,
                },
                limit: 1000,
            },
            PreheaderGuard::AtLeast {
                term: SymBound {
                    base: BoundTerm::Bound(BoundSource::Local(9)),
                    addend: 7,
                },
                limit: -50,
            },
        ] {
            let bytes = encode_preheader_guard(&guard).expect("encodable");
            // Ends in exactly one conditional branch, with the placeholder
            // offset `rewrite_loop_copies` patches.
            assert_eq!(&bytes[bytes.len() - 2..], &[0, 0], "{guard:?}");
            let branch = bytes[bytes.len() - 3];
            assert!(
                matches!(branch, 0xa1 | 0xa3),
                "{guard:?}: branch opcode {branch:#04x}"
            );
            let mut pc = 0usize;
            let mut depth = 0i32;
            let mut branches = 0usize;
            while pc < bytes.len() {
                let op = bytes[pc];
                assert!(
                    matches!(op, 0x02..=0x08 | 0x10 | 0x11 | 0x15 | 0x1a..=0x1d | 0xa1 | 0xa3),
                    "{guard:?}: emitted {op:#04x}, which is not in the inert set"
                );
                depth += match op {
                    0xa1 | 0xa3 => {
                        branches += 1;
                        -2
                    }
                    _ => 1,
                };
                pc += bytecode_len_at(&bytes, pc);
            }
            assert_eq!(pc, bytes.len(), "{guard:?}: the guard does not walk exactly");
            assert_eq!(depth, 0, "{guard:?}: the guard is not stack-balanced");
            assert_eq!(branches, 1, "{guard:?}: one fallback edge, no more");
            // No backward branch, so no poll is owed and none is dropped.
            assert!(all_backward_edges_are_polled(&bytes[..bytes.len() - 3], pc - 3));
        }
    }

    #[test]
    fn the_guard_encoder_refuses_everything_it_cannot_prove_inert() {
        use LoopXformRefusal as R;
        let local = |l: usize| SymBound {
            base: BoundTerm::Bound(BoundSource::Local(l)),
            addend: 0,
        };
        let code = shape_a();
        for (guard, want, why) in [
            // Would need `aload; arraylength` — a NullPointerException at a PC
            // whose provenance is the loop header, i.e. a throw the original
            // method does not have at that bci.
            (
                PreheaderGuard::LengthAtLeast(local(0)),
                R::GuardNotEncodable,
                "length guard",
            ),
            (
                PreheaderGuard::NonNegative(SymBound {
                    base: BoundTerm::Bound(BoundSource::ArrayLength(0)),
                    addend: 0,
                }),
                R::GuardNotEncodable,
                "arraylength term",
            ),
            // A field read adds resolution and class initialisation too.
            (
                PreheaderGuard::NonNegative(SymBound {
                    base: BoundTerm::Bound(BoundSource::Field {
                        cp_index: 3,
                        receiver_local: None,
                    }),
                    addend: 0,
                }),
                R::GuardNotEncodable,
                "field term",
            ),
            // Two comparisons would need two fallback edges.
            (
                PreheaderGuard::StrideInRange {
                    local: 1,
                    headroom: local(0),
                },
                R::GuardNotEncodable,
                "stride range",
            ),
            // `wide iload`.
            (
                PreheaderGuard::NonNegative(local(256)),
                R::GuardNotEncodable,
                "wide local index",
            ),
            // Needs `ldc`, hence a constant-pool entry this rewriter cannot
            // mint: it rewrites bytes, it does not own the class.
            (
                PreheaderGuard::AtLeast {
                    term: local(0),
                    limit: 100_000,
                },
                R::GuardNotEncodable,
                "threshold past sipush",
            ),
            // Compile-time verdicts, in both directions.
            (
                PreheaderGuard::NonNegative(SymBound {
                    base: BoundTerm::Const(7),
                    addend: 0,
                }),
                R::GuardIsConstant,
                "constant term",
            ),
            (
                PreheaderGuard::AtLeast {
                    term: local(0),
                    limit: i32::MIN,
                },
                R::GuardIsConstant,
                "no int can fail it",
            ),
            (
                PreheaderGuard::TripCountAtLeast {
                    term: local(0),
                    minimum: u32::MAX as u64,
                },
                R::GuardIsConstant,
                "no int can pass it",
            ),
        ] {
            assert_eq!(encode_preheader_guard(&guard), Err(want), "{why}");
            // …and the transform refuses with the same reason rather than
            // emitting a fast path nothing guards.
            assert_eq!(
                plan_loop_version(&code, 23, 4, 18, 1, &[], LoopXformKind::Unroll, &guard),
                Err(want),
                "{why}"
            );
        }
        // Positive control: the same call site IS admitted with a guard the
        // encoder can emit, so none of the refusals above is vacuous.
        assert!(plan_loop_version(
            &code,
            23,
            4,
            18,
            1,
            &[],
            LoopXformKind::Unroll,
            &trip_guard(2)
        )
        .is_ok());
    }

    #[test]
    fn a_side_table_entry_is_never_replicated_onto_the_guard() {
        let code = shape_a();
        let x = plan_loop_version(
            &code,
            23,
            4,
            18,
            1,
            &[],
            LoopXformKind::Unroll,
            &trip_guard(2),
        )
        .expect("versioned");
        let (gfrom, gto) = x.guard_span().expect("versioned");
        let fallback = x.versioning.as_ref().expect("versioned").fallback_base;
        // The header's images are the two fast copies and the fallback — three
        // real instructions — and NOT the five synthetic guard bytes.
        let images = x.outputs_for_bci(4);
        assert_eq!(images, vec![x.fast_base(), x.fast_base() + 14, fallback]);
        assert!(images.iter().all(|&pc| pc < gfrom || pc >= gto));
        assert!(
            images.iter().all(|&pc| x.code[pc] == code[4]),
            "every image must really be the header's opcode"
        );
        // The guard's bytes still resolve to a bci — `Compiler::orig_bci` has
        // to be able to answer for every output PC — they are simply not
        // images.
        for pc in gfrom..gto {
            assert_eq!(x.bci_at(pc), Some(4));
        }
        assert!(x.provenance_is_total());
        // A pc-keyed table therefore lands one entry per real copy: two fast,
        // one fallback, plus the untouched suffix site.
        let lifted = x.replicate_pc_keyed(&[(4usize, 0xAAu8), (22, 0xBBu8)]);
        assert_eq!(lifted.len(), 4);
        assert!(lifted[..3].iter().all(|&(_, p)| p == 0xAA));
        assert_eq!(lifted[3], (22 + x.suffix_shift, 0xBB));
    }

    #[test]
    fn osr_into_a_versioned_loop_lands_in_the_fallback_and_never_the_guard() {
        let code = shape_a();
        let (len, header, back_edge) = (23usize, 4usize, 18usize);
        for kind in [LoopXformKind::Peel, LoopXformKind::Unroll] {
            for k in 1..=3usize {
                let x = plan_loop_version(
                    &code,
                    len,
                    header,
                    back_edge,
                    k,
                    &[],
                    kind,
                    &trip_guard(k as u64 + 1),
                )
                .expect("versioned");
                let v = x.versioning.clone().expect("versioned");
                // The header enters the FALLBACK, not the guard. Entering the
                // guard is correct bytecode and wrong machine code: the guard
                // is not a loop header, so it is not a pc the OSR trampoline
                // can reconstruct a compiled state for. This assertion is the
                // regression test for the null receiver that produced.
                assert_eq!(x.osr_entry_pc(header), Some(v.fallback_base));
                assert_ne!(x.osr_entry_pc(header), Some(v.guard_pc));
                // …and so does every other bci in the region: entering a
                // transformed copy would skip the guard, which is the whole
                // point of having one.
                let mut pc = header + bytecode_len_at(&code, header);
                while pc < back_edge + 3 {
                    let entry = x.osr_entry_pc(pc).expect("bci is in range");
                    assert_eq!(entry, v.fallback_base + (pc - header), "{kind:?} k={k} bci={pc}");
                    assert_eq!(x.bci_at(entry), Some(pc));
                    assert!(entry >= x.steady_state_base() && entry <= x.back_edge_pc());
                    assert!(
                        entry >= v.fallback_base,
                        "{kind:?} k={k} bci={pc}: OSR entered a guarded copy"
                    );
                    pc += bytecode_len_at(&code, pc);
                }
                // No OSR entry anywhere in the method resolves into the guard.
                let (gfrom, gto) = x.guard_span().expect("versioned");
                for bci in 0..len {
                    if let Some(entry) = x.osr_entry_pc(bci) {
                        assert!(
                            entry < gfrom || entry >= gto,
                            "{kind:?} k={k} bci={bci}: OSR entry {entry} is inside the guard"
                        );
                    }
                }
                // Versioning has no back-edge gap whatever the fast side is:
                // the fallback is a full image of the region, back edge
                // included. Unroll on its own answers `None` here.
                assert_eq!(x.osr_entry_pc(back_edge), Some(x.back_edge_pc()));
                // Outside the region, unchanged apart from the shift.
                assert_eq!(x.osr_entry_pc(0), Some(0));
                assert_eq!(x.osr_entry_pc(22), Some(22 + x.suffix_shift));
                assert_eq!(x.osr_entry_pc(len), None);
                // The published vector is one slot per interpreter bci, with no
                // refusal anywhere in the region.
                let synthetic: Vec<i32> = (0..=x.code_len as i32).collect();
                let rebuilt = x.rebuild_pc_to_native(&synthetic, len);
                assert_eq!(rebuilt.len(), len + 1);
                assert!(
                    rebuilt[header..=back_edge].iter().all(|&n| n >= 0),
                    "{kind:?} k={k}"
                );
            }
        }
    }

    #[test]
    fn both_versions_keep_a_back_edge_poll() {
        let code = shape_a();
        let (len, body_len) = (23usize, 14usize);
        for k in 1..=3usize {
            for kind in [LoopXformKind::Peel, LoopXformKind::Unroll] {
                let x =
                    plan_loop_version(&code, len, 4, 18, k, &[], kind, &trip_guard(k as u64 + 1))
                        .expect("versioned");
                let v = x.versioning.as_ref().expect("versioned");
                assert!(all_backward_edges_are_polled(&x.code, x.code_len));
                // There are TWO loops now, and both are polled: the guarded one
                // and the fallback the failing edge reaches. Checking only the
                // first would leave the copy OSR enters unproven.
                assert_eq!(
                    backward_branch_pcs(&x.code, x.code_len),
                    vec![x.fast_back_edge_pc(), x.back_edge_pc()],
                    "{kind:?} k={k}"
                );
                assert!(emits_safepoint_poll_at(&x.code, x.fast_back_edge_pc(), x.code_len));
                assert!(emits_safepoint_poll_at(&x.code, x.back_edge_pc(), x.code_len));
                assert_eq!(x.back_edge_pc(), v.fallback_base + body_len);
                assert!(x.poll_free_bytes <= LOOP_XFORM_MAX_POLL_FREE_BYTES);
                // Below the guard's minimum the fallback runs, and it polls
                // once per trip exactly like the original loop.
                for n in 0..=(k as i32) {
                    let t = interp(&x.code, x.code_len, Some(&x.bci_of), &[n, 0, 0], &[]);
                    // Widening: a non-negative trip count to usize
                    assert_eq!(poll_count(&t, 18), n as usize, "{kind:?} k={k} n={n}");
                }
            }
        }
    }

    #[test]
    fn an_enclosing_handler_range_covers_both_versions() {
        let code = shape_a();
        // A `try` that lexically encloses the loop, with its handler after it.
        let ranges = [(2usize, 21usize, 21usize)];
        let x = plan_loop_version(
            &code,
            23,
            4,
            18,
            1,
            &ranges,
            LoopXformKind::Unroll,
            &trip_guard(2),
        )
        .expect("versioned");
        let v = x.versioning.as_ref().expect("versioned");
        assert_eq!(x.exception_ranges.len(), 1);
        let (s, e, h) = x.exception_ranges[0];
        assert_eq!(s, 2, "the range still starts where it did");
        assert_eq!(e, 21 + x.suffix_shift);
        assert_eq!(h, 21 + x.suffix_shift);
        assert!(
            s <= v.guard_pc && e >= v.fallback_base + x.body_len + 3,
            "the widened range must cover the guard, every copy and the fallback"
        );
    }

    /// **Inline reference emission and an armed ZGC barrier are mutually
    /// exclusive by construction**, and that is why there is no inline barrier
    /// sequence in this backend.
    ///
    /// `zgc-jit-load-barrier.md` scopes stage (a) as "colored slots +
    /// interpreter/native barrier + x64 barrier emission (~6-7 instructions on
    /// the fast path)". The first two landed on 2026-08-13. The third was
    /// **closed by decision, not deferred**, and this test is the reason
    /// written as an assertion:
    ///
    /// `narrow_oops_block_inline_fields` — the predicate every inline
    /// compact-field arm is gated on — returns `true` whenever the barrier is
    /// armed. So the inline arm is not emitted at all in the only state where
    /// a barrier would have anything to do. An inline sequence would therefore
    /// execute exclusively with the barrier disarmed, where it is required to
    /// be the identity transform (bad mask 0, address mask all-ones, heap base
    /// 0) — six instructions of provable no-op on the hottest path in the VM.
    ///
    /// Making it worthwhile means first REMOVING the helper routing, i.e.
    /// trading a mechanism that is correct today for one that is not yet
    /// validated. That is a throughput change and it needs a suite
    /// measurement, which is the same gate every other default-on decision in
    /// the maturity plan carries.
    ///
    /// If someone lifts the ZGC clause out of
    /// `narrow_oops_block_inline_fields`, this test fails — and at that moment
    /// inline emission stops being dead code and starts being mandatory,
    /// because `zgc_codegen_honours_read_barrier` would be lying.
    #[test]
    fn an_armed_zgc_barrier_and_inline_reference_emission_cannot_coexist() {
        // Disarmed: inline emission is available (unless narrow oops, which is
        // the pre-existing clause and is not what this test is about).
        cratonvm_types::set_zgc_read_barrier_armed(false);
        let inline_blocked_when_disarmed = zgc_read_barrier_blocks_inline_fields();

        cratonvm_types::set_zgc_read_barrier_armed(true);
        let inline_blocked_when_armed = zgc_read_barrier_blocks_inline_fields();
        let gate_when_armed = narrow_oops_block_inline_fields();
        cratonvm_types::set_zgc_read_barrier_armed(false);

        assert!(
            !inline_blocked_when_disarmed,
            "a disarmed barrier must not cost the inline arms anything"
        );
        assert!(
            inline_blocked_when_armed,
            "an armed barrier must block inline reference emission"
        );
        assert!(
            gate_when_armed,
            "...and it must do so THROUGH the gate the inline arms actually              consult, or the block is decorative"
        );
        assert!(
            zgc_codegen_honours_read_barrier(),
            "the capability `zgc_relocation_permitted` trusts rests on exactly              the property asserted above"
        );
    }
}
