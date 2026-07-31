// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Loop-invariant code motion.
//!
//! Moved verbatim out of `x64.rs`'s `Loop-Invariant Code Motion (LICM)`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.

use super::*;


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
/// `docs/internal/app-jvm-bugs/bug-01-junit-reflection-heavy-jit-frame-scan-throughput.md`.
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
pub fn precise_jit_maps_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        // Flipped back to DEFAULT-ON 2026-07-07 (see the doc comment above):
        // the BUG-01 ~6× throughput tax that motivated the d53c0e96 default-off
        // flip is gone on current dev. Opt out with CRATONVM_NO_PRECISE_JIT_MAPS=1.
        cratonvm_types::flags::runtime_var_os("CRATONVM_NO_PRECISE_JIT_MAPS").is_none()
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
    narrow_oops_enabled()
}

/// Default-on inline reference-`putfield` fast path.
///
/// When on, a `putfield` of a reference field emits an inline 16-byte `Value`
/// store INSTEAD of the `jit_putfield_object` helper CALL when the field's OLD
/// value is null (`payload == 0`, so no SATB snapshot is needed). Young
/// receivers require no post barrier; old generational receivers use the
/// inline atomic card mark. Collector-specific G1/ZGC barriers and non-null
/// old values retain the validated helper. This is the canonical
/// fresh-object-initialisation pattern (`n.left = newChild`) that dominates
/// allocation-heavy code (object binarytrees). Opt out with
/// `CRATONVM_NO_JIT_INLINE_PUTFIELD`; the former
/// `CRATONVM_JIT_INLINE_PUTFIELD` opt-in is accepted as a compatibility no-op.
///
/// INT-6 (GC audit 2026-07-10), **as corrected by G1-2** (`docs/gc/g1-audit.md`
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
/// referent (`docs/gc/g1-audit.md` §2, §5).
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
/// G1-2 (`docs/gc/g1-audit.md` §8.1). This is the predicate the inline
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
/// gate, reading the GC's process-global `JIT_REGION_BOUNDS` table whose
/// address the helpers table carries in `region_bounds_addr`. Receivers that
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
/// docs/internal/wildfly-domain-hostcontroller-sigsegv-inline-cache-null-receiver-FIXED.md
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

/// Read the 8-byte value at `gs:[disp]` (Windows TEB-relative). Used only by
/// the [`inline_rbp_tls_disp`] startup probe.
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
/// `docs/internal/default-moving-young-enabled-20260730.md`.
pub fn shadow_stack_maps_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    // `CRATONVM_MOVING_YOUNG` implies the shadow-stack codegen: the moving young
    // gen requires a COMPLETE, rewritable precise root map (see
    // `moving_young_enabled` and `collect_live_oop_homes`), so turning it on also
    // turns on the push/reload emission.
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
            || (moving_young_enabled() && shadow_emission_moving_implication_enabled());
    }
    *G.get_or_init(|| {
        cratonvm_types::flags().jit.shadow_stack
            || (moving_young_enabled() && shadow_emission_moving_implication_enabled())
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

/// `CRATONVM_JIT_MY_SCRATCH_FLUSH` — bisect lever for the scratch-register
/// flush `emit_pre_safepoint_spill_impl` performs at every GC-capable safepoint
/// under moving-young. Default ON (current behaviour); `0` drops it.
///
/// Exists to attribute the residual moving-young throughput cost that the
/// relocation-scoped admission gates do NOT remove — see the call site. Do not
/// flip the default without a quiet-host measurement on the `type.temporal`
/// Hibernate classes, which are the workload that shows the residual.
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

/// spring-bug-10 diagnostic (`CRATONVM_SHADOW_RAW_RELOAD`) — bypass the reload's
/// savebase bounds-guard (movable path) so a corrupt savebase faults on deref
/// (surfacing the bad value in the crash dump) instead of healing to pop-only.
pub(super) fn shadow_reload_raw() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_SHADOW_RAW_RELOAD").is_some())
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
/// `jit_scan` already ACCEPTS `dup2` (it only advances `pc`), and the five
/// other type-dependent stack ops (`pop2`, `dup_x1`, `dup_x2`, `dup2_x1`,
/// `dup2_x2`) are not implemented by codegen and safely bail. Only `dup2` is
/// both accepted AND (mis-)implemented, so it is the lone miscompile.
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
            // `dup2` (0x5C, the single ambiguous stack op the codegen actually
            // implements) operates on a CATEGORY-2 value. The other ambiguous
            // ops (`pop2`, `dup_x1`, `dup_x2`, `dup2_x1`, `dup2_x2`) are NOT
            // implemented by codegen — they hit the `_ => return false` bail in
            // `compile_bytecode` and the method safely stays interpreted, so we
            // do NOT reject for them here. An imprecisely-modeled state is
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
            // pop2: two cat-1 OR one cat-2 — codegen's `pop2` is unimplemented
            // (bails), so we only need to keep the model's height roughly sane.
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
            // dup_x1 / dup_x2 — unimplemented by codegen (safe bail); just
            // resync the model loosely. Clear so we do not mis-evaluate a later
            // dup2 against a now-shuffled stack we no longer model precisely.
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
            // dup2_x1 / dup2_x2 — unimplemented by codegen (safe bail); resync
            // the model loosely by clearing.
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
