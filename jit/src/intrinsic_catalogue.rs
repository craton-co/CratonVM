// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The intrinsic catalogue: the closed set of call-site intrinsics this JIT
//! knows how to emit, the sentinel encoding that carries one in a
//! `JitDirectCall.entry`, and the frozen `MATH_*_INTRINSIC` aliases that
//! encoding has to keep honouring.
//!
//! Split out of `lib.rs` on 2026-09-16, following `jfr_compile_decision`,
//! `osr_entry`, `inline_cache_pic`, `ea_ir_bridge`, `exec_memory` and
//! `descriptor_shapes`.
//!
//! # Why this is a real boundary
//!
//! [`JitIntrinsic`] is not an ordinary enum: its DISCRIMINANTS are part of the
//! crate's ABI with itself. A variant is handed to the emitter as
//! `usize::MAX - (variant as usize)` -- an address in kernel space that can
//! never be a real code pointer -- so the `callee_entry` dispatch in the
//! backends stays one integer comparison instead of a tagged union. Three
//! consequences follow, and all three are enforced in this file:
//!
//!   * the first fourteen variants (the `java.lang.Math` family) may not be
//!     reordered or renumbered, because the deprecated `MATH_*_INTRINSIC`
//!     constants at the bottom of this file are DEFINED as those sentinels and
//!     are still named by older call sites;
//!   * [`JitIntrinsic::as_entry`] and `JitIntrinsic::from_entry` are exact
//!     inverses, and `from_entry` is the only place that decides whether a
//!     `usize` near the top of the address space is an intrinsic or a pointer,
//!     so the round trip is checkable by reading one file;
//!   * families are appended strictly between their `INTRINSIC REGION
//!     BEGIN/END` markers, which is what lets several people add intrinsic
//!     families at once without colliding in a 450-variant enum.
//!
//! Interleaved with the compilation driver, the ordering rule read as a comment
//! somebody might not scroll to. Here, the rule, the encoding it protects and
//! the aliases that depend on it are the entire contents of the file.
//!
//! [`ffm_kind_for_descriptor`] travelled with the enum rather than staying with
//! the FFM registration doors: it maps a `ValueLayout` subtype named in a
//! descriptor onto the `FFM_KIND_*` code the emitted load carries, which makes
//! it the same kind of object as the rest of this file -- a fixed table
//! translating a source-level name into a number that generated code compares
//! against. It consults `ffm_intrinsic_disabled` (imported below, still owned by
//! the crate root with the other feature gates) so that the kill switch is read
//! in the ONE gate both the registration doors and the emitter share; that
//! single read is the only thing in this file that is not a pure function of
//! its arguments.
//!
//! Glob-re-exported from the crate root, so every path a caller used before the
//! split still resolves -- the code moved, the API did not. That includes the
//! `MATH_*_INTRINSIC` aliases, which are re-exported twice: once out of the
//! private `math_intrinsic_aliases` module into this one, and once more out of
//! this module by the crate root's glob.

use crate::ffm_intrinsic_disabled;

/// Enumeration of every JIT call-site intrinsic.
///
/// Each variant is mapped onto the `JitDirectCall.entry` sentinel space via
/// [`JitIntrinsic::as_entry`] (`usize::MAX - (variant as usize)`). These
/// sentinels are never valid code pointers (kernel address space), so the
/// `callee_entry` dispatch in `x64.rs` stays a single integer comparison.
///
/// **Variant ordering is load-bearing.** The first 14 variants — the
/// `java.lang.Math` family — MUST keep their declaration order so that the
/// deprecated `MATH_*_INTRINSIC` const aliases below resolve to exactly the
/// same `usize::MAX - N` values they had before the enum was introduced.
///
/// Per-family regions are marked with `INTRINSIC REGION BEGIN/END: <TAG>`
/// comment pairs. A follow-up agent adding family `<TAG>` appends its
/// variants strictly between that family's BEGIN/END markers and nowhere
/// else, so 8 agents editing 8 disjoint regions never collide.
#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JitIntrinsic {
    // --- java.lang.Math family (variants 0..=13 — ORDER IS LOAD-BEARING) ---
    MathSqrt = 0,
    MathFloor = 1,
    MathCeil = 2,
    MathRint = 3,
    MathAbsDouble = 4,
    MathAbsFloat = 5,
    MathAbsInt = 6,
    MathAbsLong = 7,
    MathFmaDouble = 8,
    MathFmaFloat = 9,
    MathMinInt = 10,
    MathMaxInt = 11,
    MathMinLong = 12,
    MathMaxLong = 13,
    /// `Math/StrictMath.multiplyHigh(JJ)J` — high 64 bits of the SIGNED
    /// 128-bit product, emitted as a one-operand `IMUL r64` (RDX:RAX = RAX*r),
    /// result taken from RDX. Hottest leaf in the SunEC P-256 field multiply.
    MathMultiplyHigh = 14,
    /// `Math/StrictMath.unsignedMultiplyHigh(JJ)J` — high 64 bits of the
    /// UNSIGNED 128-bit product, emitted as a one-operand `MUL r64`.
    MathUnsignedMultiplyHigh = 15,
    /// `Math/StrictMath.min(FF)F` / `max(FF)F` / `min(DD)D` / `max(DD)D`.
    ///
    /// The `int` and `long` forms have been intrinsics since Round-8; these
    /// four had not, so every `Math.min(float,float)` from compiled code ran
    /// the JDK's Java body — which is not a one-liner. It tests for NaN,
    /// then tests both arguments against zero, then calls
    /// `Float.floatToRawIntBits` and reads a `static final long`, before
    /// finally doing the comparison.
    ///
    /// Measured on the four-sphere ray tracer (three `Math.min(float,float)`
    /// per pixel, 307,200 pixels): replacing the calls with plain ternaries
    /// in the Java source cut that kernel's CratonVM CPU time from 145 ms to
    /// 77 ms. Roughly **47% of the kernel was inside `Math.min`**, about
    /// 74 ns per call. The same probe with `Math.sqrt` removed showed no
    /// change, confirming the cost was these calls and not FP work.
    ///
    /// Lowered inline — see the `MATH_MIN_FLOAT_INTRINSIC` arm in
    /// `x64/op_invoke.rs` for the SSE sequence and, more importantly,
    /// for why `MINSS` alone is NOT `Math.min`: it implements neither the
    /// NaN rule nor the signed-zero rule.
    MathMinFloat = 16,
    MathMaxFloat = 17,
    MathMinDouble = 18,
    MathMaxDouble = 19,

    // ===== INTRINSIC REGION BEGIN: INT_BITS =====
    // java.lang.Integer bit-manipulation intrinsics (Phase 1a). Variant
    // ordering within this region is local and not externally observed —
    // only the Math family's declaration order is load-bearing.
    IntBitCount,
    IntNumberOfLeadingZeros,
    IntNumberOfTrailingZeros,
    IntReverseBytes,
    IntHighestOneBit,
    IntLowestOneBit,
    IntReverse,
    IntCompare,
    /// `Integer.rotateLeft(II)I` / `rotateRight(II)I` — `ROL`/`ROR r32, CL`.
    /// x86 masks `CL & 0x1f` for a 32-bit rotate, which is byte-identical to
    /// the JDK definition (rotation is mod 32), so no distance masking is
    /// needed. Hot in ChaCha/Salsa/Blake/SHA inner loops (e.g. BC SPHINCS).
    IntRotateLeft,
    IntRotateRight,
    // ===== INTRINSIC REGION END: INT_BITS =====

    // ===== INTRINSIC REGION BEGIN: LONG_BITS =====
    // java.lang.Long bit-manipulation intrinsics (Phase 1b). Variant
    // ordering within this region is local and not externally observed —
    // only the Math family's declaration order is load-bearing.
    LongBitCount,
    LongNumberOfLeadingZeros,
    LongNumberOfTrailingZeros,
    LongReverseBytes,
    LongHighestOneBit,
    LongLowestOneBit,
    LongCompare,
    /// `Long.rotateLeft(JI)J` / `rotateRight(JI)J` — `ROL`/`ROR r64, CL`.
    /// x86 masks `CL & 0x3f` for a 64-bit rotate, byte-identical to the JDK
    /// definition (rotation is mod 64). Note the descriptor takes a `long`
    /// value and an `int` distance.
    LongRotateLeft,
    LongRotateRight,
    // ===== INTRINSIC REGION END: LONG_BITS =====

    // ===== INTRINSIC REGION BEGIN: ATOMIC_INT =====
    // `java.util.concurrent.atomic.AtomicInteger` read-modify-write family.
    //
    // All six are ONE instruction — `LOCK XADD [value], r32` — differing only
    // in the addend and in whether the result is the pre- or post-add value.
    // `XADD` returns the OLD value in the source register, so the `*AndGet`
    // forms just add the delta back afterwards.
    //
    // Why these matter: `getAndIncrement` is a REGISTERED NATIVE
    // (`native-builtins/src/phases_early.rs`), so every increment from compiled
    // code paid a full native dispatch — measured at ~250 ns/op against
    // HotSpot's ~11 ns, i.e. one uncontended `lock xadd` behind ~1000x of call
    // overhead. See
    // `docs/known-issues/netty/fastthreadlocal-2e9-iteration-throughput-wall-20260812.md`.
    //
    // The native keeps its state in the receiver's field slot 0 via
    // `get_field_volatile` / `compare_and_swap_field` — the SAME memory this
    // intrinsic addresses — so an interpreted caller and a compiled caller
    // still agree. That is what makes the swap sound; if the native had used a
    // side table these could not be intrinsified at all.
    //
    // Variant ordering within this region is local and not externally observed.
    AtomicIntGetAndIncrement, // getAndIncrement()I  -> old
    /// `get()I` / `getPlain()I` / `getAcquire()I` -- a plain aligned load.
    ///
    /// On x86-64 TSO an ordinary `MOV` IS a correct volatile/acquire load:
    /// loads are not reordered with older loads, so no fence is owed. The
    /// weaker two are no stronger than `get`, so one emitter arm serves all
    /// three. (`set` is deliberately NOT here: a volatile STORE owes
    /// StoreLoad, which is an `XCHG`/`MFENCE`, not a `MOV`.)
    AtomicIntGet,
    AtomicIntGetAndDecrement, // getAndDecrement()I  -> old
    AtomicIntIncrementAndGet, // incrementAndGet()I  -> old + 1
    AtomicIntDecrementAndGet, // decrementAndGet()I  -> old - 1
    AtomicIntGetAndAdd,       // getAndAdd(I)I       -> old
    AtomicIntAddAndGet,       // addAndGet(I)I       -> old + delta
    /// `compareAndSet(II)Z` / `weakCompareAndSet(II)Z` — one `LOCK CMPXCHG`.
    ///
    /// Unlike the six `XADD` forms above this one has a RESULT that is not the
    /// field: `CMPXCHG` reports success in ZF, so the arm ends `SETZ`/`MOVZX`
    /// rather than moving the payload out. It also has to place its operands
    /// differently — `CMPXCHG` compares against RAX implicitly, so the
    /// receiver moves to RCX and the expected value takes RAX.
    ///
    /// `weakCompareAndSet` is the same instruction: the spec permits it to fail
    /// spuriously and `LOCK CMPXCHG` never does, and a strictly-stronger
    /// implementation is a legal one (the registered native makes the same
    /// choice — both triples call `compare_and_swap_field`).
    AtomicIntCompareAndSet,
    // ===== INTRINSIC REGION END: ATOMIC_INT =====

    // ===== INTRINSIC REGION BEGIN: ATOMIC_LONG =====
    // The 64-bit twin of the region above, emitted as one REX.W `LOCK XADD`.
    //
    // It exists because the `AtomicInteger` ladder was measured and its
    // `AtomicLong` counterpart was not written: the exact census
    // (`--nojit CRATONVM_DISABLE_INTRINSICS=1`) of
    // `HashedWheelTimerTest#testExecutionOnTime` puts
    // `AtomicLong.incrementAndGet` and `.decrementAndGet` at **1.00 call per
    // expired task each**, and netty's `HashedWheelTimer.pendingTimeouts` is
    // exactly that pair — one increment per `newTimeout`, one decrement per
    // expiry. Each was a full native dispatch, measured at ~154 ns/call on
    // this branch against the ~1 ns an uncontended `lock xadd` costs.
    //
    // Soundness rests on the same three things the 32-bit region names, and
    // each was re-checked for this class rather than assumed:
    //   * the registered natives keep their state in the SAME memory —
    //     `native_atomic_long_get` is `get_field_volatile(this, 0)` and
    //     `native_atomic_long_increment_and_get` is
    //     `atomic_fetch_add_long(this, 0, 1)` — so an interpreted caller and a
    //     compiled caller still agree on one location;
    //   * the receiver class-id guard, because `AtomicLong` is not final;
    //   * the per-object COMPACT/LEGACY branch.
    AtomicLongGetAndIncrement, // getAndIncrement()J  -> old
    /// `get()J` / `getPlain()J` / `getAcquire()J` — a plain aligned 64-bit
    /// load. On x86-64 TSO an aligned `MOV` IS a correct volatile/acquire load
    /// and is atomic at 8 bytes, so no fence and no `LOCK` is owed. (`set` is
    /// deliberately NOT here: a volatile STORE owes StoreLoad.)
    AtomicLongGet,
    AtomicLongGetAndDecrement, // getAndDecrement()J  -> old
    AtomicLongIncrementAndGet, // incrementAndGet()J  -> old + 1
    AtomicLongDecrementAndGet, // decrementAndGet()J  -> old - 1
    AtomicLongGetAndAdd,       // getAndAdd(J)J       -> old
    AtomicLongAddAndGet,       // addAndGet(J)J       -> old + delta
    /// The 64-bit twin of [`JitIntrinsic::AtomicIntCompareAndSet`] — one
    /// REX.W `LOCK CMPXCHG`.
    ///
    /// MEASURED before this existed, `probes/AtomicCasCost.java` on this host:
    /// `AtomicLong.compareAndSet` **361.3 ns/op against HotSpot's 13.25**, a
    /// 27x gap, while its `get` (2.41) and `getAndIncrement` (12.02) siblings —
    /// which ARE in this region — sit at 1.3-5x. The whole difference between
    /// them was membership of this list.
    ///
    /// It is also the measured blocker for retiring the `java.util.Random`
    /// shadow: `Random.next(int)` is a `get` + `compareAndSet` loop, and
    /// running that loop on a real `AtomicLong` cost 2265.6 ns against the
    /// VM's side-table `nextInt` at 273.4. See
    /// `known-issues/hibernate/jpalargeblobtest-per-native-call-floor-20260830.md`.
    AtomicLongCompareAndSet,
    // ===== INTRINSIC REGION END: ATOMIC_LONG =====

    // ===== INTRINSIC REGION BEGIN: BOX_UNBOX =====
    // `java.lang.Long.longValue()` / `java.lang.Integer.intValue()` — the
    // UNBOX half of autoboxing, emitted as the same aligned `MOV` the
    // `AtomicLongGet` / `AtomicIntGet` arms above already emit. `Long.value`
    // and `Integer.value` are `private final` at field slot 0, so this is a
    // plain load behind a null check; no `LOCK`, no fence, nothing to order.
    //
    // # Why a thin direct bind was not enough
    //
    // Both already HAVE one (`LONG_LONG_VALUE_DIRECT_FN`,
    // `INTEGER_INT_VALUE_DIRECT_FN`, 2026-07 and 2026-08-19), and both are
    // engaged — a `CRATONVM_DBG=jit-method-stats` run of
    // `probes/BlobStreamCostCpu.java` reports `Long.valueOf=4
    // Long.longValue=2` sites bound. A bind still emits a CALL with
    // `needs_context: true`, so it pays argument marshalling, the
    // Rust<->JIT boundary note, and a non-inlinable call; MEASURED on that
    // probe, boxing cost **2266 ns/byte** WITH the binds live, against a
    // primitive counter's 3.4. The remaining cost is the call itself, and
    // only inline emission removes a call.
    //
    // # Soundness
    //
    // `java.lang.Long` and `java.lang.Integer` are FINAL, so unlike the
    // `Atomic*` regions above there is no override a guard has to protect
    // against. The exact class-id guard is emitted anyway and its miss
    // deopts: it costs one compare, and it is what makes a mis-resolved
    // constant-pool class (or a future non-final receiver reaching this
    // matcher) fail closed instead of loading slot 0 of something else.
    //
    // The registered natives keep the value in the SAME memory this reads —
    // `lang_math.rs::register_wrapper_natives` builds a boxed `Long` by
    // writing field 0 — so an interpreted caller and a compiled caller agree
    // on one location, which is the same argument the `ATOMIC_INT` region
    // makes and the reason it says a side table would have made
    // intrinsification impossible.
    LongLongValue,   // java/lang/Long.longValue()J       -> field 0, 8 bytes
    IntegerIntValue, // java/lang/Integer.intValue()I     -> field 0, 4 bytes
    // ===== INTRINSIC REGION END: BOX_UNBOX =====

    // ===== INTRINSIC REGION BEGIN: ARRAYCOPY =====
    /// `java.lang.System.arraycopy(Object,int,Object,int,int)` (Phase 2).
    ///
    /// The descriptor is type-erased — the element kind is only known at
    /// runtime. The codegen inlines a primitive-array fast path (null
    /// checks, fused bounds checks, then a memmove-correct `REP MOVSB`)
    /// and routes every uncertain case — null receiver, non-array,
    /// reference array, mismatched element types, or any out-of-bounds
    /// position — through the site's ordinary `invoke_dispatch` CALL
    /// (registered at the same pc alongside the intrinsic), so the native
    /// `System.arraycopy` raises `NullPointerException` /
    /// `ArrayStoreException` / `ArrayIndexOutOfBoundsException` and applies
    /// the GC store barrier exactly, with normal call semantics. The
    /// uncommon-trap deopt stub is only the fallback when that dispatch info
    /// is missing. (This doc used to name the trap as the ONLY path; the
    /// emitter moved off it because a whole-method re-run could
    /// double-execute side effects — see the ARRAYCOPY region in
    /// `x64/op_invoke.rs`.)
    ArraycopyPrimitive,
    // ===== INTRINSIC REGION END: ARRAYCOPY =====

    // (Round-10 `r10-intr` retired the `SCOPED_MEMORY_UNALIGNED` region that
    // used to sit here: four `ScopedMemoryGet{Short,Char,Int,Long}Unaligned`
    // variants that were declared but never registered by any matcher in
    // `lib.rs` and never matched by any `x64::op_invoke` codegen arm, so no
    // sentinel for them could ever be produced. The live feature —
    // `ScopedMemoryAccess.*Unaligned`, get AND put, all four widths — is
    // served by `direct_helpers::ScopedMemoryOp` /
    // `direct_helpers::is_scoped_memory_op`, wired into `lib.rs` and
    // `vm/src/runtime/interpreter/jit_bridge.rs`; that path was already live
    // and made this enum family redundant. See
    // `docs/internal/retired/r10-intr-scoped-memory-unaligned-dead-variants-RETIRED-20260921.md`
    // for the audit that established nothing outside this file ever named
    // them.)

    // ===== INTRINSIC REGION BEGIN: STRING_ACCESS =====
    // java.lang.String access intrinsics (Phase 3a). The foundation waves
    // (commits 06bfac0 / 544cbea) added inline getfield + array-access
    // codegen and the `StringFieldLayout` API, so these are now inlined
    // when a `StringFieldLayout` with a `coder` field is available. The
    // matcher only registers them when the layout resolves; otherwise the
    // call falls back to normal native dispatch. Variant ordering within
    // this region is local and not externally observed.
    StringLength,   // length()I
    StringIsEmpty,  // isEmpty()Z
    StringCharAt,   // charAt(I)C
    StringHashCode, // hashCode()I
    // ===== INTRINSIC REGION END: STRING_ACCESS =====

    // ===== INTRINSIC REGION BEGIN: STRINGBUILDER_ACCESS =====
    // java.lang.StringBuilder, behind a `StringBuilderFieldLayout` and an
    // exact `[recv+0] == StringBuilder` class-id guard. See that struct for
    // the measurement (`length()` 194 ns, `append(char)` 349 ns, against 2 ns
    // for the already-intrinsified `String.length()`), and for why the guard
    // is what keeps `StringBuffer`'s `synchronized` + `toStringCache`
    // obligations off these paths.
    //
    // `append(char)` is the only MUTATING call-site intrinsic in this file,
    // and its slow edges go to the ordinary native call rather than to an
    // uncommon trap. A full payload is not an uncommon event — it is what
    // every growing builder does O(log n) times — and a deopt there would
    // re-run the whole method in the interpreter each time it grew.
    StringBuilderLength,     // length()I
    StringBuilderAppendChar, // append(C)Ljava/lang/StringBuilder;
    /// `charAt(I)C` — bounds-checked, coder-branched read of `value[index]`,
    /// same decline-on-miss shape as the two members above (an out-of-range
    /// index is what every caller that reaches the end of a builder does
    /// once, not an uncommon event worth a deopt). See
    /// `lane-stringbuilder-jit-intrinsic-RETIRED-20260917.md`: MEASURED on this tree,
    /// `charAt()` in a tight loop was 3701 ns/op with the natives standing
    /// and 11232 ns/op with `CRATONVM_ENFORCE_NATIVE_SHADOW` armed over
    /// `StringBuilder`+`AbstractStringBuilder` — the one member of this
    /// family with no call-site intrinsic at all before this, so every call
    /// paid full dispatch either way.
    StringBuilderCharAt, // charAt(I)C
    /// `append(Ljava/lang/String;)Ljava/lang/StringBuilder;` — the shape
    /// almost every real caller has (`SbLayoutBench.appendString`), and the
    /// one most of a builder's growth happens through. Same decline-on-miss
    /// shape as `append(char)`: capacity, a coder mismatch this path cannot
    /// serve (narrowing UTF16 into a LATIN1 destination), or a null `value`
    /// on either side all decline into the ordinary call, which is what
    /// grows or inflates the array today. A same-coder copy and a
    /// LATIN1-source-into-UTF16-destination WIDEN copy are both served
    /// in-place; only the reverse (narrowing) declines.
    StringBuilderAppendString, // append(Ljava/lang/String;)Ljava/lang/StringBuilder;
    /// `append(I)Ljava/lang/StringBuilder;` — javac's own emission for
    /// string concatenation of an int (`SbLayoutBench.appendInt`, the one
    /// shape [`StringBuilderAppendString`] left at 2.3x-2.6x). NON-NEGATIVE
    /// values only: the digits are computed into a small stack scratch
    /// buffer (unsigned divide-by-ten, so there is no `Integer.MIN_VALUE`
    /// magnitude-overflow edge to get wrong) and then handed to the exact
    /// same in-place copy tail `append(String)` uses, with the scratch
    /// buffer standing in for a LATIN1 `String.value`. A negative value
    /// declines into the ordinary call rather than teaching this path a
    /// sign, matching the family's existing rule that an uncommon shape
    /// declines rather than growing this file's edge count.
    StringBuilderAppendInt, // append(I)Ljava/lang/StringBuilder;
    // ===== INTRINSIC REGION END: STRINGBUILDER_ACCESS =====

    // ===== INTRINSIC REGION BEGIN: STRING_SEARCH =====
    // java.lang.String search/compare intrinsics (Phase 3b). `equals` is
    // inlined as a coder+length-guarded raw byte compare (deopts to native
    // on a coder mismatch or a non-String argument).
    //
    // Phase 3b follow-up: `compareTo` and `indexOf(String)` are now ALSO
    // inlined. Unlike `equals` (which can byte-compare only when the
    // coders match), these decode each receiver/argument character
    // through a per-string `coder` branch (0 LATIN1 = 1 byte/char, 1 UTF16
    // = 2 LE bytes/char), so EVERY coder combination — including mixed —
    // is handled inline with no coder-mismatch deopt. The deopt stub is
    // still used for the genuinely uncertain cases (null receiver, null
    // String argument, null backing `value` array). Variant ordering here
    // is local and not externally observed.
    StringEquals,    // equals(Ljava/lang/Object;)Z
    StringCompareTo, // compareTo(Ljava/lang/String;)I
    // `indexOf(I)I` — handed out again (E27-1 N2b, 2026-08-18), but only for a
    // call site whose needle the backend can prove is a compile-time constant
    // in `0..=0xFFFF`. The inline body scans for one UTF-16 code unit, which is
    // the JDK's answer on exactly that range and NOT outside it (the gate is
    // `Character.isValidCodePoint` before any narrowing, and a supplementary
    // `ch` matches a surrogate PAIR). The screen is
    // `x64/bytecode_walk.rs::prev_insn_int_const`; a site that fails it is not
    // intrinsified and dispatches normally, with no deopt involved.
    StringIndexOfChar, // indexOf(I)I — constant BMP needles only
    StringIndexOfStr,  // indexOf(Ljava/lang/String;)I
    // ===== INTRINSIC REGION END: STRING_SEARCH =====

    // ===== INTRINSIC REGION BEGIN: ARRAYS_OPS =====
    // java.util.Arrays.fill / Arrays.equals intrinsics (Phase 4a). Variant
    // ordering within this region is local and not externally observed.
    //
    // `fill` variants are keyed by element width: 1-byte (byte/boolean),
    // 2-byte (char/short), 4-byte (int), 8-byte (long). `fill([FF)V` and
    // `fill([DD)V` are intentionally NOT registered — they are bailed (see
    // try_resolve_intrinsic) so the matcher never registers an intrinsic
    // whose codegen would have to special-case an FP fill value arriving in
    // an XMM stack slot. The 3-arg ranged `fill([IIII)V` overloads are out
    // of scope.
    ArraysFill1, // fill([BB)V, fill([ZZ)V — REP STOSB
    ArraysFill2, // fill([CC)V, fill([SS)V — REP STOSW
    ArraysFill4, // fill([II)V             — REP STOSD
    ArraysFill8, // fill([JJ)V             — REP STOSQ
    // `equals` variants are likewise keyed by element width. The INTEGRAL
    // `equals` overloads reduce to a raw byte-wise compare of length*width
    // bytes (boolean arrays store 0/1, so a byte compare is exact). The
    // `float[]`/`double[]` overloads must NOT be registered here: the JDK
    // compares them through `floatToIntBits`/`doubleToLongBits`, which
    // canonicalises every NaN, so two NaNs with different payloads are EQUAL
    // to `Arrays.equals` and unequal to a byte compare.
    ArraysEquals1, // equals([B[B)Z, equals([Z[Z)Z
    ArraysEquals2, // equals([C[C)Z, equals([S[S)Z
    ArraysEquals4, // equals([I[I)Z
    ArraysEquals8, // equals([J[J)Z
    // ===== INTRINSIC REGION END: ARRAYS_OPS =====

    // ===== INTRINSIC REGION BEGIN: ARRAYS_SORT =====
    // java.util.Arrays.sort for primitive integral arrays (Phase 4b). One
    // variant per element width; the emitted insertion sort differs only in
    // the element load/store encoding (scale + sign/zero extension). Variant
    // ordering within this region is local and not externally observed.
    ArraysSortInt,
    ArraysSortLong,
    ArraysSortChar,
    ArraysSortShort,
    ArraysSortByte,
    // ===== INTRINSIC REGION END: ARRAYS_SORT =====

    // ===== INTRINSIC REGION BEGIN: CRC32 =====
    // java.util.zip.CRC32 / CRC32C `update` call-site intrinsics (Phase 4c).
    //
    // Both classes hold a single `private int crc` at instance field slot 0
    // (`CRC_FIELD_SLOT`), the running (uncomplemented) CRC state — see
    // crc_layout_contract.md and native-builtins/src/
    // zip_crc32c.rs. Each intrinsic threads that slot: load slot 0, fold the
    // input byte(s), store back. `update(I)V` folds one byte; `update([BII)V`
    // folds a `byte[]` range (with inline null + bounds guards). A receiver
    // class-id guard (the receiver's dynamic class must be exactly the
    // declared CRC32/CRC32C class — a subclass could override `update`)
    // precedes every variant; on mismatch codegen deopts to normal dispatch.
    //
    //   * Crc32cUpdate* — Castagnoli CRC-32C (reflected poly 0x82F63B78).
    //     Emitted with the hardware `CRC32` instruction, which computes
    //     exactly this polynomial. Gated on `x64::has_sse42()`.
    //   * The IEEE `CRC32` class has no variant: its matcher arm registers no
    //     intrinsic (real JDK CRC32 keeps its public value in `crc`, not the
    //     running state), so the variants and their bit-loop codegen were
    //     never produced and were deleted on 2026-09-12.
    Crc32cUpdateByte,  // CRC32C.update(I)V
    Crc32cUpdateBytes, // CRC32C.update([BII)V
    // ===== INTRINSIC REGION END: CRC32 =====

    // ===== INTRINSIC REGION BEGIN: FP_BITS =====
    // `Double.doubleToRawLongBits` / `Double.longBitsToDouble` — the two
    // halves of a bit reinterpretation, one `MOVQ` each.
    //
    // Added 2026-08-19 from a `--dump-native-registry` census of
    // `PSquarePercentileTest`, which reported 466,400,490 native BRIDGE
    // invocations for the class and named these two as 361M of them:
    //
    //     203,434,476  java/lang/Double.doubleToRawLongBits(D)J
    //     157,756,624  java/lang/Double.longBitsToDouble(J)D
    //
    // Both were `kind: "bridge"` -- the checked native funnel -- for an
    // operation that is a single register move.
    //
    // **RAW only, and that is load-bearing.** `doubleToRawLongBits` is
    // specified to hand back the exact bit pattern, NaN payload included,
    // which is what `MOVQ` does. Its sibling `doubleToLongBits`
    // CANONICALISES every NaN to `0x7ff8000000000000` and must NOT be
    // matched here; the resolver names one method and not the other on
    // purpose, and a test pins that.
    DoubleToRawLongBits, // Double.doubleToRawLongBits(D)J
    LongBitsToDouble,    // Double.longBitsToDouble(J)D
    // ===== INTRINSIC REGION END: FP_BITS =====

    // ===== INTRINSIC REGION BEGIN: FFM_SEGMENT =====
    // `MemorySegment.getAtIndex` / `setAtIndex` — the FFM ELEMENT accessors.
    //
    // Any segment-backed array drives these one element at a time
    // (`ShortArray.get` -> `TornadoMemorySegment.getShortAtIndex` ->
    // `MemorySegment.getAtIndex`), and through the ordinary native dispatch
    // funnel they measure ~1158 ns/element against ~0.8 ns for a `short[]`
    // element. `Unsafe.getShort(long)`, a maximally lean native through the
    // SAME funnel, costs ~303 ns — so ~300 ns is the funnel and the remaining
    // ~850 ns is the segment native's ~10 `NativeContext` round-trips for
    // scope liveness, address and size.
    //
    // ONE variant per direction, not one per element kind: the element width
    // comes from the call site's DESCRIPTOR, which names the `ValueLayout`
    // subtype (`getAtIndex:(Ljava/lang/foreign/ValueLayout$OfShort;J)S`), and
    // the emitter re-reads it from the site's `JitInvokeInfo`. Fourteen
    // variants would encode the same fact twice.
    //
    // Lowered as a CALL to `JitRuntimeHelpers::ffm_segment_get`/`_set`, not as
    // inline machine code, and that is deliberate. The segment liveness model
    // spans two synthetic classes whose slot conventions are owned by two
    // different files, and a second copy of one of its slot indices has
    // already made that check silently DEAD once (the W7-89 note on
    // `PE_ARENA_CLASS`). The helper asks the native for a verdict instead of
    // re-deriving one; see `cratonvm_native_builtins::ffm_fast`. A helper that
    // DECLINES returns 0 and the emitted code falls through to the unchanged
    // native dispatch for the same site, so every unrecognised case — a
    // heap-backed carrier, a closed scope, an out-of-bounds index — keeps
    // today's behaviour and today's exceptions.
    FfmSegmentGetAtIndex, // MemorySegment.getAtIndex(ValueLayout$OfX, J)X
    FfmSegmentSetAtIndex, // MemorySegment.setAtIndex(ValueLayout$OfX, J, X)V
    // ===== INTRINSIC REGION END: FFM_SEGMENT =====

    // ===== INTRINSIC REGION BEGIN: ARRAYLIST_ACCESS =====
    // `java.util.ArrayList.get(int)` / `size()` for a receiver whose header
    // class id is EXACTLY `java/util/ArrayList`, behind an
    // [`ArrayListFieldLayout`]. Round 9 wave 8 (`arraylist8`),
    // `perf-collection-natives-cost-a-generic-native-dispatch-per-call`.
    //
    // Both are native-shadowed (`native_al_get` / `native_al_size`), so every
    // call from compiled code was a MIC helper round trip plus the native's
    // checked heap reads. The inline form answers exactly what the native's
    // wave-3 fast path (`al_fast_state` + an in-range index) answers, and
    // DECLINES into the site's unchanged virtual/interface dispatch on every
    // other outcome -- never an uncommon trap, because an out-of-range index is
    // ordinary control flow in real code.
    //
    // The single-pass tier does NOT carry these as `JitDirectCall` sentinels:
    // it recognises the site from its `JitInvokeInfo` and emits the fast path
    // as a guarded PREFIX of the ordinary MIC/PIC dispatch (the shape the
    // guarded-virtual inline uses), so no compile door registers anything and
    // registration and emission cannot disagree. The variants exist so a tier
    // that does want a sentinel (the optimizing tier's lowering, a cross-lane
    // request of `NOTES-w8-arraylist8.md`) has one, and so
    // [`try_resolve_arraylist_intrinsic`] has the family's usual answer shape.
    ArrayListGet, // java/util/ArrayList|List.get(I)Ljava/lang/Object;
    ArrayListSize, // java/util/ArrayList|List|Collection.size()I
                  // ===== INTRINSIC REGION END: ARRAYLIST_ACCESS =====
}

/// `java/util/ArrayList`'s `elementData` / `size` addresses, for the
/// ARRAYLIST_ACCESS intrinsics.
///
/// Same two-offsets-per-field discipline as `StringFieldLayout` and
/// `StringBuilderFieldLayout`, for the same reason: a class with a registered
/// `CompactLayout` may still have LEGACY-laid-out instances, so the emitted code
/// dispatches per object on the `GC_FLAG_COMPACT` header bit.
///
/// The legacy arm additionally carries each cell's TAG offset: a legacy field
/// is a 16-byte `Value` cell, and the fast path reads `size`'s payload only when
/// the cell says `Int` and `elementData`'s only when it says `Object` (the rule
/// `FIELD_CELL_TAG_OBJECT`'s doc gives: a pointer read out of a cell that may
/// not hold one is a wild pointer).
///
/// # Refusals (construction returns `None`)
///
/// Every refusal is HERE, so a site can never be claimed by one party and
/// refused by another:
///
/// * `class_id == 0` -- no exact guard is possible;
/// * `CRATONVM_NO_JIT_ARRAYLIST_INTRINSICS=1` -- the family's kill switch and
///   the B arm of an in-binary A/B;
/// * `CRATONVM_DBG_ALTRACE` set -- the native's trace must still see every call;
/// * narrow oops -- the element and `elementData` loads are 8-byte raw
///   pointers, the representation `StringBuilderFieldLayout::new` also insists on;
/// * an armed ZGC read barrier at construction time -- the refusal the inline
///   `getfield` takes;
/// * a registered compact storage width other than 4 for `size` or 8 for
///   `elementData`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArrayListFieldLayout {
    /// Abstract field slot index of `ArrayList.elementData`.
    pub data_field_index: usize,
    /// Abstract field slot index of `ArrayList.size`.
    pub size_field_index: usize,
    /// Byte offset of `size`'s 4-byte payload in a COMPACT instance.
    pub size_compact_offset: i32,
    /// Byte offset of `size`'s 4-byte payload in a LEGACY instance.
    pub size_legacy_offset: i32,
    /// Byte offset of `size`'s cell TAG word in a LEGACY instance.
    pub size_legacy_tag_offset: i32,
    /// Byte offset of `elementData`'s 8-byte pointer in a COMPACT instance.
    pub data_compact_offset: i32,
    /// Byte offset of `elementData`'s 8-byte pointer payload in a LEGACY
    /// instance.
    pub data_legacy_offset: i32,
    /// Byte offset of `elementData`'s cell TAG word in a LEGACY instance.
    pub data_legacy_tag_offset: i32,
    /// `max(data_field_index, size_field_index) + 1`: a LEGACY receiver whose
    /// header `num_slots` is below this does not have both cells, and is
    /// declined (the native's `object_num_fields` test).
    pub min_num_slots: u32,
    /// `ObjectHeader` class id of `java/util/ArrayList`, the EXACT receiver
    /// guard. Never 0. A subclass (which may override `get`), `Vector`, a view
    /// carrier or any other `List` fails it and takes the ordinary dispatch.
    pub class_id: u32,
}

impl ArrayListFieldLayout {
    /// Build the layout from `elementData`'s and `size`'s abstract field
    /// indices and `java/util/ArrayList`'s class id, applying every refusal
    /// listed on the type.
    pub fn new(data_field_index: usize, size_field_index: usize, class_id: u32) -> Option<Self> {
        if class_id == 0 || data_field_index == size_field_index {
            return None;
        }
        if arraylist_intrinsics_disabled() {
            return None;
        }
        // `native-collections`' `altrace_enabled` is `runtime_var_os(..).is_some()`,
        // so this asks the same question the same way.
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_ALTRACE").is_some() {
            return None;
        }
        if cratonvm_types::narrow_oop::narrow_oops_enabled() {
            return None;
        }
        if cratonvm_types::zgc_read_barrier_armed() {
            return None;
        }
        let cell = |idx: usize| -> Option<i32> {
            i32::try_from(cratonvm_types::HEADER_SIZE + idx.checked_mul(cratonvm_types::SLOT_SIZE)?)
                .ok()
        };
        let data_cell = cell(data_field_index)?;
        let size_cell = cell(size_field_index)?;
        let data_legacy_offset = data_cell + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET as i32; // Cast: small header constant
        let size_legacy_offset = size_cell + cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET as i32; // Cast: small header constant
        let tag = cratonvm_types::FIELD_CELL_TAG_OFFSET as i32; // Cast: small header constant
                                                                // COMPACT: the registered `CompactLayout` body offset IS the payload
                                                                // address. With no registered layout (or compact fields globally off)
                                                                // no instance can carry `GC_FLAG_COMPACT`, so the compact arm is
                                                                // unreachable and pointing it at the legacy payload keeps it harmless
                                                                // rather than wild if that invariant ever slips -- the rule both String
                                                                // layouts follow.
        let mut size_compact_offset = size_legacy_offset;
        let mut data_compact_offset = data_legacy_offset;
        if cratonvm_types::compact_ref_fields_enabled() {
            if let Some((body_off, storage)) =
                cratonvm_types::compact_field_storage(class_id, size_field_index)
            {
                if storage.size_runtime() != 4 {
                    return None;
                }
                size_compact_offset = i32::try_from(cratonvm_types::HEADER_SIZE + body_off).ok()?;
            }
            if let Some((body_off, storage)) =
                cratonvm_types::compact_field_storage(class_id, data_field_index)
            {
                if storage.size_runtime() != 8 {
                    return None;
                }
                data_compact_offset = i32::try_from(cratonvm_types::HEADER_SIZE + body_off).ok()?;
            }
        }
        let min_num_slots = u32::try_from(data_field_index.max(size_field_index) + 1).ok()?;
        Some(Self {
            data_field_index,
            size_field_index,
            size_compact_offset,
            size_legacy_offset,
            size_legacy_tag_offset: size_cell + tag,
            data_compact_offset,
            data_legacy_offset,
            data_legacy_tag_offset: data_cell + tag,
            min_num_slots,
            class_id,
        })
    }
}

/// `CRATONVM_NO_JIT_ARRAYLIST_INTRINSICS=1` -- keep every `ArrayList.get` /
/// `size` site on its ordinary dispatch. Read at layout construction (once per
/// compile), which is the one gate every consumer goes through.
pub fn arraylist_intrinsics_disabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_NO_JIT_ARRAYLIST_INTRINSICS")
}

/// The ARRAYLIST_ACCESS answer for a call site declared `(class, name,
/// descriptor)`: `(sentinel entry, JLS parameter count, return tag, guard class
/// id)`, the shape of the family's other resolvers. `None` when the layout is
/// absent or the site is not one of the two members.
///
/// Site classes: `java/util/ArrayList` and `java/util/List` for `get` and
/// `size`, plus `java/util/Collection` for `size`. Every one of them reaches
/// `ArrayList.get`/`size` for an exact `java/util/ArrayList` receiver, and the
/// guard (never 0) admits nothing else.
pub fn try_resolve_arraylist_intrinsic(
    class: &str,
    name: &str,
    descriptor: &str,
    layout: Option<ArrayListFieldLayout>,
) -> Option<(usize, usize, u8, u32)> {
    let layout = layout?;
    if layout.class_id == 0 {
        return None;
    }
    let (intrinsic, num_params, ret) = match (class, name, descriptor) {
        ("java/util/ArrayList" | "java/util/List", "get", "(I)Ljava/lang/Object;") => {
            (JitIntrinsic::ArrayListGet, 1usize, b'L')
        }
        ("java/util/ArrayList" | "java/util/List" | "java/util/Collection", "size", "()I") => {
            (JitIntrinsic::ArrayListSize, 0usize, b'I')
        }
        _ => return None,
    };
    Some((intrinsic.as_entry(), num_params, ret, layout.class_id))
}

/// The element-kind code for an FFM accessor descriptor, or `None` if the
/// descriptor is not one this fast path handles.
///
/// # This is the ONLY gate, and it has to be
///
/// Registration and emission must agree exactly. A site registered here but
/// declined by the emitter leaves a `JitDirectCall` whose `entry` is an
/// intrinsic SENTINEL (`usize::MAX - variant`), and the ordinary direct-call
/// path then CALLS it: measured, `EXCEPTION_ACCESS_VIOLATION at
/// pc=0xFFFFFFFFFFFFFFB1`, which is that sentinel. So the emitter must not
/// carry a second, narrower filter — it asks this function and nothing else.
///
/// The codes are `cratonvm_vm::jit::helpers`' `FFM_KIND_*`, and the mapping is
/// driven by the `ValueLayout` subtype the descriptor NAMES — which is what
/// makes the element width a compile-time constant instead of a runtime layout
/// probe. `ValueLayout$OfBoolean` and `$OfAddress` are deliberately absent: the
/// first has a domain narrower than its byte (the JDK reads the byte and
/// compares it to zero) and the second returns a fresh zero-length segment,
/// which is an allocation and not a load.
pub fn ffm_kind_for_descriptor(descriptor: &str) -> Option<i64> {
    // `CRATONVM_JIT_NO_FFM_INTRINSIC=1` — keep every FFM element accessor on
    // ordinary native dispatch. The B arm of an IN-BINARY A/B: cross-run wall
    // time on this host is not a measurement (a concurrent build moved every
    // absolute number by ~2x while this was being developed), and it is also
    // the bisect lever if a miscompile is ever suspected. Consulted HERE, in
    // the one gate both registration doors and the emitter share, so the switch
    // cannot disable half the feature and leave a sentinel behind.
    if ffm_intrinsic_disabled() {
        return None;
    }
    // The layout type is the first parameter in both the get and set forms.
    Some(
        if descriptor.starts_with("(Ljava/lang/foreign/ValueLayout$OfByte;") {
            0 // FFM_KIND_BYTE
        } else if descriptor.starts_with("(Ljava/lang/foreign/ValueLayout$OfShort;") {
            1 // FFM_KIND_SHORT
        } else if descriptor.starts_with("(Ljava/lang/foreign/ValueLayout$OfChar;") {
            2 // FFM_KIND_CHAR
        } else if descriptor.starts_with("(Ljava/lang/foreign/ValueLayout$OfInt;") {
            3 // FFM_KIND_INT
        } else if descriptor.starts_with("(Ljava/lang/foreign/ValueLayout$OfLong;") {
            4 // FFM_KIND_LONG
        } else if descriptor.starts_with("(Ljava/lang/foreign/ValueLayout$OfFloat;") {
            5 // FFM_KIND_FLOAT
        } else if descriptor.starts_with("(Ljava/lang/foreign/ValueLayout$OfDouble;") {
            6 // FFM_KIND_DOUBLE
        } else {
            // `$OfBoolean` and `$OfAddress` stay absent: the first has a domain
            // narrower than its byte (the JDK reads the byte and compares it to
            // zero) and the second returns a fresh zero-length segment, which is
            // an allocation and not a load.
            return None;
        },
    )
}

impl JitIntrinsic {
    /// Map this intrinsic onto the `JitDirectCall.entry` sentinel space.
    ///
    /// Returns `usize::MAX - (self as usize)`, a value that can never be a
    /// valid code pointer. The `x64.rs` codegen ladder compares
    /// `callee_entry` against `JitIntrinsic::Foo.as_entry()` to recognise
    /// an intrinsic call site.
    pub const fn as_entry(self) -> usize {
        usize::MAX - (self as usize)
    }

    /// The LAST-declared variant — the one with the largest discriminant, and
    /// so the one whose [`Self::as_entry`] is the LOWEST sentinel. Every
    /// sentinel lies in `LAST.as_entry()..=usize::MAX`.
    ///
    /// Must name the final variant of the enum. A family appended after it
    /// without updating this would put its sentinels below the range, where
    /// [`Self::is_sentinel_entry`] reads them as ordinary code addresses;
    /// `sentinel_range_covers_every_family` pins one variant per family.
    pub const LAST: JitIntrinsic = JitIntrinsic::ArrayListSize;

    /// Whether `entry` is an intrinsic SENTINEL rather than a code address.
    ///
    /// A backend that reaches its plain direct-`CALL` arm with an entry for
    /// which this is `true` has failed to emit an intrinsic it was handed —
    /// a registration/emission disagreement — and must refuse the compile:
    /// calling the sentinel is `EXCEPTION_ACCESS_VIOLATION at
    /// pc=0xFFFFFFFFFFFFFFxx`, measured once already on the FFM family. The
    /// range sits at the very top of the address space, which no user-mode
    /// mapping on either supported OS reaches.
    pub const fn is_sentinel_entry(entry: usize) -> bool {
        entry >= Self::LAST.as_entry()
    }

    /// True for the CRC32/CRC32C `update` call-site intrinsics.
    ///
    /// These are the only `invokevirtual` intrinsics that need a runtime
    /// receiver class-id guard, so their codegen depends on a resolved
    /// `JitDirectCall::guard_class_id`. The resolution loop in `try_compile`
    /// uses this to skip registering a CRC32 intrinsic whose declared class
    /// id could not be resolved (`guard_class_id == 0`), letting the site
    /// fall through to normal virtual dispatch instead of inlining unsoundly.
    pub const fn is_crc32_family(self) -> bool {
        matches!(
            self,
            JitIntrinsic::Crc32cUpdateByte | JitIntrinsic::Crc32cUpdateBytes
        )
    }

    /// Recover a [`JitIntrinsic`] from a `JitDirectCall::entry` sentinel, if
    /// the value is in fact an intrinsic sentinel. Used by the resolution
    /// loop to classify a freshly-matched entry without re-running the
    /// (class, name, descriptor) matcher.
    pub fn from_entry(entry: usize) -> Option<JitIntrinsic> {
        // The sentinel space is `usize::MAX - (variant as usize)`. The last
        // declared variant bounds the valid offset range. (This bound used to
        // name `Crc32cUpdateBytes`, which stopped being last when FP_BITS and
        // FFM_SEGMENT were appended; harmless only because this function
        // classifies CRC32 alone.)
        let offset = usize::MAX.checked_sub(entry)?;
        if offset > JitIntrinsic::LAST as usize {
            return None;
        }
        // Exhaustive map — keeps this in lockstep with the enum so a new
        // variant fails to compile until added here.
        Some(match offset {
            x if x == JitIntrinsic::Crc32cUpdateByte as usize => JitIntrinsic::Crc32cUpdateByte,
            x if x == JitIntrinsic::Crc32cUpdateBytes as usize => JitIntrinsic::Crc32cUpdateBytes,
            // Non-CRC32 intrinsic — the resolution loop only needs CRC32
            // classification, so any other in-range sentinel is reported as
            // "not a CRC32 intrinsic" via the `is_crc32_family` check below.
            _ => return None,
        })
    }
}

/// Sentinel `entry` values for JitDirectCall indicating inlined Math intrinsics.
///
/// Backward-compatible aliases for the [`JitIntrinsic`] Math variants so
/// existing `x64.rs` comparisons and the VM interpreter keep compiling
/// unchanged. New code should use [`JitIntrinsic`] variants and
/// [`JitIntrinsic::as_entry`] directly.
mod math_intrinsic_aliases {
    use super::JitIntrinsic;
    pub const MATH_SQRT_INTRINSIC: usize = JitIntrinsic::MathSqrt.as_entry();
    pub const MATH_FLOOR_INTRINSIC: usize = JitIntrinsic::MathFloor.as_entry();
    pub const MATH_CEIL_INTRINSIC: usize = JitIntrinsic::MathCeil.as_entry();
    pub const MATH_RINT_INTRINSIC: usize = JitIntrinsic::MathRint.as_entry();
    pub const MATH_ABS_DOUBLE_INTRINSIC: usize = JitIntrinsic::MathAbsDouble.as_entry();
    pub const MATH_ABS_FLOAT_INTRINSIC: usize = JitIntrinsic::MathAbsFloat.as_entry();
    pub const MATH_ABS_INT_INTRINSIC: usize = JitIntrinsic::MathAbsInt.as_entry();
    pub const MATH_ABS_LONG_INTRINSIC: usize = JitIntrinsic::MathAbsLong.as_entry();
    pub const MATH_FMA_DOUBLE_INTRINSIC: usize = JitIntrinsic::MathFmaDouble.as_entry();
    pub const MATH_FMA_FLOAT_INTRINSIC: usize = JitIntrinsic::MathFmaFloat.as_entry();
    pub const MATH_MIN_INT_INTRINSIC: usize = JitIntrinsic::MathMinInt.as_entry();
    pub const MATH_MAX_INT_INTRINSIC: usize = JitIntrinsic::MathMaxInt.as_entry();
    pub const MATH_MIN_LONG_INTRINSIC: usize = JitIntrinsic::MathMinLong.as_entry();
    pub const MATH_MAX_LONG_INTRINSIC: usize = JitIntrinsic::MathMaxLong.as_entry();
    pub const MATH_MULTIPLY_HIGH_INTRINSIC: usize = JitIntrinsic::MathMultiplyHigh.as_entry();
    pub const MATH_UNSIGNED_MULTIPLY_HIGH_INTRINSIC: usize =
        JitIntrinsic::MathUnsignedMultiplyHigh.as_entry();
    pub const MATH_MIN_FLOAT_INTRINSIC: usize = JitIntrinsic::MathMinFloat.as_entry();
    pub const MATH_MAX_FLOAT_INTRINSIC: usize = JitIntrinsic::MathMaxFloat.as_entry();
    pub const MATH_MIN_DOUBLE_INTRINSIC: usize = JitIntrinsic::MathMinDouble.as_entry();
    pub const MATH_MAX_DOUBLE_INTRINSIC: usize = JitIntrinsic::MathMaxDouble.as_entry();
}
pub use math_intrinsic_aliases::*;

#[cfg(test)]
mod sentinel_range_tests {
    use super::JitIntrinsic;

    /// One variant from EVERY family region, first and last of the enum
    /// included. Each must fall inside the sentinel range, and `LAST` must
    /// have the largest discriminant of them — the property
    /// `is_sentinel_entry` rests on.
    #[test]
    fn sentinel_range_covers_every_family() {
        let one_per_family = [
            JitIntrinsic::MathSqrt,
            JitIntrinsic::MathMaxDouble,
            JitIntrinsic::IntRotateRight,
            JitIntrinsic::LongRotateRight,
            JitIntrinsic::AtomicIntCompareAndSet,
            JitIntrinsic::AtomicLongCompareAndSet,
            JitIntrinsic::IntegerIntValue,
            JitIntrinsic::ArraycopyPrimitive,
            JitIntrinsic::StringHashCode,
            JitIntrinsic::StringBuilderAppendInt,
            JitIntrinsic::StringIndexOfStr,
            JitIntrinsic::ArraysEquals8,
            JitIntrinsic::ArraysSortByte,
            JitIntrinsic::Crc32cUpdateBytes,
            JitIntrinsic::LongBitsToDouble,
            JitIntrinsic::FfmSegmentGetAtIndex,
            JitIntrinsic::FfmSegmentSetAtIndex,
            JitIntrinsic::ArrayListGet,
            JitIntrinsic::ArrayListSize,
        ];
        for v in one_per_family {
            assert!(
                JitIntrinsic::is_sentinel_entry(v.as_entry()),
                "{v:?}'s sentinel lies below the range `is_sentinel_entry` accepts; \
                 `JitIntrinsic::LAST` no longer names the last variant"
            );
            assert!(
                (v as usize) <= (JitIntrinsic::LAST as usize),
                "{v:?} has a larger discriminant than `JitIntrinsic::LAST`"
            );
        }
    }

    #[test]
    fn an_ordinary_code_address_is_not_a_sentinel() {
        for addr in [0usize, 0x1000, 0x7FFF_FFFF_FFFF, usize::MAX / 2] {
            assert!(!JitIntrinsic::is_sentinel_entry(addr), "{addr:#x}");
        }
    }

    /// `from_entry` still classifies the CRC32 pair, and only them.
    #[test]
    fn from_entry_still_classifies_only_the_crc32_family() {
        assert_eq!(
            JitIntrinsic::from_entry(JitIntrinsic::Crc32cUpdateByte.as_entry()),
            Some(JitIntrinsic::Crc32cUpdateByte)
        );
        assert_eq!(
            JitIntrinsic::from_entry(JitIntrinsic::FfmSegmentSetAtIndex.as_entry()),
            None
        );
        assert_eq!(JitIntrinsic::from_entry(0x1000), None);
    }
}
