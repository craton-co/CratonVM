// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The optimizing tier's element-wise vector loop (round 16 matrix residual,
//! `docs/feature-designs/jit-ir-optimizations.md`,
//! MX16-5 and the "no vectorisation" line of its "What is left").
//!
//! # What it vectorizes
//!
//! A counted loop of the exact shape
//!
//! ```text
//! for (j = <entry>; j < n; j++)  A[j] = f(A[j], B[j], ..., x, y, ...)
//! ```
//!
//! where every array access in the body is at index `j` itself, every array
//! base and every scalar operand (`x`, `y`, `n`) is defined outside the loop,
//! `f` is a tree of `+ - *` (and `/` for `float`/`double`) over ONE element
//! type, there is exactly one store, and the induction variable is the
//! loop's only value phi. `R16Bench.matrix`'s `c[i][j] += x * b[k][j]` is the
//! case that motivated it, once the rows are hoisted
//! (`ir_optimize::hoist_invariant_array_loads`).
//!
//! JIT round 21 (lane vector, book
//! `docs/feature-designs/jit-ir-optimizations.md`)
//! widened the shape under three switches (opt-in then, default ON since the
//! r22 A/B), each read once per compile ([`VecOpts::from_flags`]):
//!
//! * `CRATONVM_JIT_IR_VECTOR_BITOPS` (VEC21-1): `& | ^` over `int`/`long`
//!   lanes, and shifts by a CONSTANT count (`<< >> >>>` for `int`, `<< >>>`
//!   for `long`; AVX2 has no 64-bit arithmetic shift). The count is masked the
//!   way Java masks it (`& 31` / `& 63`) before it becomes the immediate, so
//!   `x << 33` is `VPSLLD x, 1`, never the x86 "count >= width gives zero".
//! * `CRATONVM_JIT_IR_VECTOR_SUBWORD` (VEC21-3): `byte[]` lanes (32 per
//!   register) and 16-bit lanes (`short[]` / `char[]`, which may mix). A
//!   sub-word body computes in `int` in Java and stores the low 8/16 bits;
//!   only `+ - & | ^` (and `*` for 16-bit lanes) are admitted, every one of
//!   which has low result bits that depend on the low operand bits alone, so
//!   computing them in narrow lanes is exact. The `i2b`/`i2s` narrowing pair
//!   (`(x << c) >> c`, `c <= 32 - lane bits`) and a `& C` whose `C` keeps
//!   every lane bit (`i2c`'s `& 0xFFFF`) are transparent; every other shift
//!   is refused (a right shift reads bits a narrow lane does not hold).
//!   `bastore` serves `boolean[]` too, where JVMS 6.5 stores `value & 1`: the
//!   pre-loop's guards send a call whose byte-lane destination is a
//!   `boolean[]` (its header kind byte) to the scalar loop, which masks.
//! * `CRATONVM_JIT_IR_VECTOR_REDUCE` (VEC21-4): a loop with NO store whose
//!   second value phi `s` is an `int`/`long` accumulator, `s = s + f(..)` or
//!   `s = s - f(..)`. Two's-complement addition is associative and
//!   commutative, so lane partial sums folded once at the end are the scalar
//!   sum exactly. `float`/`double` accumulators stay refused: Java FP addition
//!   is not associative and HotSpot does not reassociate a strict sum either.
//!
//! JIT round 23 wave 1 (lane L8) added, each default ON with a kill switch:
//!
//! * `CRATONVM_JIT_IR_VECTOR_MINMAX` (VEC21-5): `m = Math.min/max(m, f(..))`
//!   over `int`/`long`/`float`/`double` lanes. The lanes start at the
//!   broadcast seed and the fold replaces the accumulator. `VPMAXSD` is exact;
//!   `long` compares and blends; FP takes both operand orders of `VMAXPD`
//!   and ANDs (max) / ORs (min) them, which orders `-0.0` below `+0.0`, and
//!   ORs every element's unordered compare into a NaN-seen register: a NaN
//!   anywhere (seed included) discards the pre-loop, whose only effect so far
//!   is in registers, and the scalar loop runs from the entry -- so the NaN
//!   Java returns (the first one in program order, payload and all) is the
//!   scalar loop's.
//! * `CRATONVM_JIT_IR_VECTOR_SCAN` (VEC21-8): an early-exit scan, `for (j..n)
//!   if (a[j] OP x) leave;` with no store. The pre-loop only advances `j` to
//!   the first lane whose test is true (`VPCMPEQ/GT`, `VPMOVMSKB`, `BSF`)
//!   and the scalar loop re-tests that element and leaves at that iteration.
//! * `CRATONVM_JIT_IR_VECTOR_NE_EXIT` (BCE21-5, vector side): a `j != n`
//!   loop test, admitted like `j < n`.
//!
//! The round-23 L8 close (`docs/feature-designs/jit-ir-optimizations.md`,
//! L8-3 / L8-4) added, each default ON with a kill switch:
//!
//! * `CRATONVM_JIT_IR_VECTOR_ELEM_MINMAX` (L8-3): `Math.min/max(x, y)` as an
//!   element-tree node of a store loop (`c[j] = Math.max(a[j], b[j])`) over
//!   `int`/`long`/`float`/`double` lanes. `VPMAXSD` is exact; `long` compares
//!   and blends; FP is `ir_lower::fp_minmax_avx_bytes` packed -- both operand
//!   orders combined, then each argument's NaN blended in (`y` first, `x`
//!   last), so every lane answers the JDK body's bits, NaN payloads included.
//!   Nothing is reordered, so a NaN lane needs no scalar fallback.
//! * `CRATONVM_JIT_IR_VECTOR_SCAN_WIDE` (L8-4): a scan's test may be a chain
//!   of compares joined by `||` / `&&` (`a[j] == c1 || a[j] == c2`), each
//!   side an element TREE (`(a[j] & 0xFF) == c`, `a[j] - '0' < 10`), over FP
//!   lanes too (`VCMPPD`, the predicate chosen from the `fcmpl`/`fcmpg` NaN
//!   rule), and a `char` ORDER compare (`a[j] < 0x80`: `VPMAXUW` then
//!   `VPCMPEQW`, the unsigned 16-bit order). A sub-word test whose operand is
//!   a tree loads its elements WIDENED to `int` lanes (`VPMOVSXBD` /
//!   `VPMOVSXWD` / `VPMOVZXWD`, eight per step), so the tree is evaluated
//!   value-exact, as Java evaluates it in `int`.
//!
//! The r21 book's residuals added, each default ON with a kill switch:
//!
//! * `CRATONVM_JIT_IR_VECTOR_WIDEN` (VEC21-2): in a store or reduction loop
//!   the lanes are the type the loop WRITES (its store, or its accumulator),
//!   and a load may be narrower, widened at the load ([`emit_widen_load`]):
//!   `byte`/`short`/`char` into `int` (or `byte` into 16-bit) lanes, any of
//!   them plus `int` through an `i2l` into `long` lanes, `float` through an
//!   `f2d` into `double` lanes. Each widening instruction is the scalar
//!   `xaload` extension (and `i2l` / `f2d`) lane by lane; the load reads `W`
//!   source elements, so its address scales `j` by the source width. (A
//!   scan widens by its own plan-wide rule, `Widen`.)
//! * `CRATONVM_JIT_IR_VECTOR_NARROW` (VEC21-2's narrowing half): when the
//!   loads do not fit a `byte`/`short`/`char` store's own lanes, `int` lanes
//!   truncated at the store ([`emit_narrow_store`]): each keeps its low 8/16
//!   bits, all `bastore`/`sastore`/`castore` store; a `boolean[]`
//!   destination still leaves for the scalar loop.
//! * `CRATONVM_JIT_IR_VECTOR_LONG_MUL` (VEC21-2's `a[j] * 3L`): `long` lane
//!   multiply from three `VPMULUDQ` ([`emit_long_mul`]), the wrapped product.
//! * `CRATONVM_JIT_IR_VECTOR_VARSHIFT` (VEC21-6): `int`/`long` shifts by a
//!   loop-invariant count. The count is masked (`& 31` / `& 63`) into its OWN
//!   register before the loop ([`count_load_code`]), never broadcast: the
//!   `xmm`-count form reads the whole low quadword as one count.
//!
//! # How: a vector pre-loop on the entry edge, the scalar loop untouched
//!
//! Nothing in the graph changes. On the loop's ENTRY edge, after its phi
//! copies (so `j` holds its entry value), [`crate::ir_lower`] emits:
//!
//! 1. self-guards that make every vector access in bounds on their own: `j >=
//!    0`, and for each base `base != null` and `n <= base.length`. A failed
//!    guard skips straight to the scalar loop, which then faults (or not)
//!    exactly where the bytecode does;
//! 2. a main loop of `UNROLL` vector steps per trip and a one-step loop after
//!    it, each step `W` elements (one 256-bit register), while `j + W <= n`;
//!    every trip first tests the safepoint flag and, when it is set, leaves
//!    for the scalar loop, whose own back-edge poll then stops the thread;
//! 3. `j` written back to the induction phi (register and home) -- and for a
//!    reduction the folded lanes added to the accumulator phi -- and a jump
//!    into the scalar loop's header, which runs the remaining `< W` elements
//!    and anything the guards declined.
//!
//! # Why it is exact
//!
//! * Each vector step computes, lane by lane, the same IEEE / two's-complement
//!   operation the scalar body computes for that element: `VADDPD` is `dadd`
//!   per lane, `VPMULLD` is `imul`'s low half, and so on. No FP reduction is
//!   reordered (a loop carrying an FP value phi is refused), and nothing is
//!   fused (no FMA).
//! * Every access in the body is at index `j`, so iteration `j` touches only
//!   element `j` of each array. Two bases may be the same array (`c[i]` and
//!   `b[k]` with `i == k`): a step reads elements `j..j+W` before writing them,
//!   and in the scalar order element `j+1` is likewise read (at `j+1`) before
//!   its own store and after stores to OTHER elements only. So the reads see
//!   the same values in either order. A carried dependence (`a[j] = a[j-1]
//!   + x`, `a[j + k] = ..`) has an index that is not `j` and is refused.
//! * The guards bound every lane: `0 <= j` and `j + W <= n <= length`, the
//!   trip arithmetic in 64 bits (no wrap near `Integer.MAX_VALUE`). No lane
//!   can fault, so no step needs a deopt frame, and none is emitted. An
//!   `ArrayIndexOutOfBoundsException` the loop owes is thrown by the scalar
//!   loop at its exact iteration, after exactly the scalar stores before it:
//!   a loop whose bound passes some array's length takes no vector step.
//! * No safepoint occurs inside: the flag test leaves BEFORE a step, with
//!   every element so far stored and `j` naming the first one not done --
//!   exactly the state of the scalar loop at the header with that `j`.
//!
//! What is NOT claimed: a plain store's visibility order to another thread.
//! The JMM gives plain array stores none, and the vector store writes the
//! same values.

mod avx512;

pub(crate) fn avx512_vector_loop_enabled() -> bool {
    avx512::enabled()
}

use crate::ir::{CmpOp, Graph, IrType, MemKind, NodeId, Op, ScalarOp, UnboxOp, NO_NODE};
use crate::ir_schedule::Schedule;

/// `CRATONVM_JIT_IR_VECTOR_LOOP` -- the element-wise vector pre-loop. Default
/// ON; `=0` turns it off (the scalar loop then runs every element, as before).
/// Read per compile: no static, so a test's thread override reaches it.
pub(crate) fn ir_vector_loop_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_VECTOR_BITOPS` (r21 vector, VEC21-1) -- `& | ^` and
/// constant-count shifts over `int`/`long` lanes. Default ON (flipped; `=0` kills), perf.
pub(crate) fn ir_vector_bitops_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// `CRATONVM_JIT_IR_VECTOR_SUBWORD` (r21 vector, VEC21-3) -- `byte[]` and
/// `short[]`/`char[]` lanes. Default ON (flipped; `=0` kills), perf.
pub(crate) fn ir_vector_subword_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// `CRATONVM_JIT_IR_VECTOR_REDUCE` (r21 vector, VEC21-4) -- `int`/`long` add
/// reductions. Default ON (flipped; `=0` kills), perf.
pub(crate) fn ir_vector_reduce_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// `CRATONVM_JIT_IR_VECTOR_MINMAX` (r23 wave 1 lane L8, VEC21-5) --
/// `m = Math.min/max(m, f(..))` reductions. Default ON (perf); `=0` kills it.
pub(crate) fn ir_vector_minmax_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_VECTOR_SCAN` (r23 wave 1 lane L8, VEC21-8) -- early-exit
/// scans. Default ON (perf); `=0` kills it.
pub(crate) fn ir_vector_scan_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_VECTOR_NE_EXIT` (r23 wave 1 lane L8, BCE21-5's vector
/// side) -- a `j != n` loop test admitted like `j < n`. Default ON (perf);
/// `=0` kills it.
pub(crate) fn ir_vector_ne_exit_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_VECTOR_ELEM_MINMAX` (r23 L8 close, L8-3) -- element-wise
/// `Math.min/max` in a store loop. Default ON (perf); `=0` kills it.
pub(crate) fn ir_vector_elem_minmax_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_VECTOR_SCAN_WIDE` (r23 L8 close, L8-4) -- multi-compare
/// tests, element-tree operands, FP lanes and `char` order compares in an
/// early-exit scan. Default ON (perf); `=0` kills it (the VEC21-8 planner).
pub(crate) fn ir_vector_scan_wide_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_VECTOR_VARSHIFT` (VEC21-6) -- `int`/`long` shifts by a
/// loop-invariant count. Default ON (perf); `=0` kills it.
pub(crate) fn ir_vector_varshift_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_VECTOR_WIDEN` (VEC21-2) -- loads of a narrower element
/// type than the loop's lanes, widened at the load. Default ON (perf); `=0`
/// kills it.
pub(crate) fn ir_vector_widen_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_VECTOR_LONG_MUL` (VEC21-2's `l[i] = a[i] * 3L`) -- `long`
/// lane multiply, three `VPMULUDQ`. Default ON (perf); `=0` kills it.
pub(crate) fn ir_vector_long_mul_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_VECTOR_NARROW` (VEC21-2, narrowing half) -- `int` lanes
/// truncated into a `byte[]`/`short[]`/`char[]` store (`b[j] = (byte) a[j]`),
/// [`emit_narrow_store`]. Default ON (perf); `=0` kills it.
pub(crate) fn ir_vector_narrow_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_VECTOR_SCOPED_STORE` (JIT round 24 item B1 of
/// `jit-r24-alloc-and-ffm-loop-residuals-proposals-RETIRED-20261026.md`; perf, **default ON**
/// since the round-24 close, `=0` kills): a `MemorySegment` element store loop
/// over a heap or native segment (`for (k..n) s.setAtIndex(JAVA_INT, k, v + k)`)
/// runs the vector pre-loop. Close battery (5 rotated reps, facts equal
/// HotSpot's): `R26SegLongLoops` heap-put 6173 -> 360 ns/op, native-put
/// 14548 -> 352; `R23SegMono` heapput 0.20x, nativeput 0.09x; no row lost.
/// The store is the FFM element expansion's proven scoped put
/// (`Graph::ffm_proven_heap_accesses`) at `base + start + width * k`, the
/// value a tree of invariants and the induction itself ([`VExpr::Iota`]),
/// and the pre-loop bounds every lane by the segment's own length (`n *
/// width <= length`), the test the scalar access's guards make per element.
/// HotSpot's C2 superword stores this loop at about 0.4 ns per element
/// against the scalar body's 1.1. Read per compile.
pub(crate) fn ir_vector_scoped_store_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_VECTOR_SCOPED_GET_REDUCE` (j27 wave 6, lane T1LOOPFFM,
/// VEC27-1 of `jit-r27-loops-and-ffm-residuals-proposals-RETIRED-20261101.md`; perf, **default
/// ON** since the quiet-host A/B, `=0` kills): an `int`/`long` reduction over a `MemorySegment`
/// element read (`for (k..n) h += s.getAtIndex(JAVA_INT, k)`) runs the
/// vector pre-loop. The read is the FFM element expansion's scoped get, held
/// to the store's rules (`scoped_access_shape`): proven in bounds (heap) or a
/// pre-checked session (native), little-endian, `start + i2l(k) * width`, the
/// length field on file; the pre-loop bounds every lane by the segment's own
/// length and rebases onto its start exactly as B1's store does. A reduction
/// stores nothing, so no element it reads can alias a write. `R26SegLongLoops`
/// heap-sum 5986 / native-sum 8223 ns/op against HotSpot's 2359 / 1204 (C2
/// superword); quiet host, 5 interleaved rounds of `J27T1LoopFfmRows`: heap-sum
/// 2802 -> 99, native-sum 3756 -> 118, long-sum 2722 -> 156 ns/op, 24/24 facts
/// equal HotSpot's in both modes. Read per compile.
pub(crate) fn ir_vector_scoped_get_reduce_enabled() -> bool {
    true
}

/// The planner's switches, read once per compile ([`plan_vector_loops`]) and
/// passed down, so a unit test can plan with any combination without touching
/// flag state. All `false` is the round-16 planner exactly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct VecOpts {
    /// [`ir_vector_bitops_enabled`].
    pub(crate) bitops: bool,
    /// [`ir_vector_subword_enabled`].
    pub(crate) subword: bool,
    /// [`ir_vector_reduce_enabled`].
    pub(crate) reduce: bool,
    /// [`ir_vector_minmax_enabled`] (VEC21-5).
    pub(crate) minmax: bool,
    /// [`ir_vector_scan_enabled`] (VEC21-8).
    pub(crate) scan: bool,
    /// [`ir_vector_ne_exit_enabled`] (BCE21-5, vector side).
    pub(crate) ne_exit: bool,
    /// [`ir_vector_elem_minmax_enabled`] (L8-3).
    pub(crate) elem_minmax: bool,
    /// [`ir_vector_scan_wide_enabled`] (L8-4).
    pub(crate) scan_wide: bool,
    /// [`ir_vector_varshift_enabled`] (VEC21-6).
    pub(crate) varshift: bool,
    /// [`ir_vector_widen_enabled`] (VEC21-2).
    pub(crate) widen: bool,
    /// [`ir_vector_long_mul_enabled`] (VEC21-2).
    pub(crate) long_mul: bool,
    /// [`ir_vector_narrow_enabled`] (VEC21-2, under `widen`).
    pub(crate) narrow: bool,
    /// [`ir_vector_scoped_store_enabled`] (JIT round 24 B1).
    pub(crate) scoped_store: bool,
    /// [`ir_vector_scoped_get_reduce_enabled`] (j27 VEC27-1).
    pub(crate) scoped_get: bool,
}

impl VecOpts {
    pub(crate) fn from_flags() -> Self {
        Self {
            bitops: ir_vector_bitops_enabled(),
            subword: ir_vector_subword_enabled(),
            reduce: ir_vector_reduce_enabled(),
            minmax: ir_vector_minmax_enabled(),
            scan: ir_vector_scan_enabled(),
            ne_exit: ir_vector_ne_exit_enabled(),
            elem_minmax: ir_vector_elem_minmax_enabled(),
            scan_wide: ir_vector_scan_wide_enabled(),
            varshift: ir_vector_varshift_enabled(),
            widen: ir_vector_widen_enabled(),
            long_mul: ir_vector_long_mul_enabled(),
            narrow: ir_vector_narrow_enabled(),
            scoped_store: ir_vector_scoped_store_enabled(),
            scoped_get: ir_vector_scoped_get_reduce_enabled(),
        }
    }
}

/// How many vector steps the main loop takes per trip. Four 32-byte steps keep
/// every displacement in a signed byte (`HEADER_SIZE + 3 * 32 = 112`).
pub(crate) const UNROLL: usize = 4;

/// The element type of a vectorized loop: every access and every arithmetic
/// node of the body has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VecKind {
    Double,
    Float,
    Int,
    Long,
    /// `byte[]` (and `boolean[]`, which `baload`/`bastore` share) lanes, under
    /// [`VecOpts::subword`].
    Byte,
    /// `short[]` / `char[]` 16-bit lanes, under [`VecOpts::subword`]. The two
    /// may mix: every admitted op's low 16 result bits are the same whether
    /// the operands were sign- or zero-extended.
    Short,
}

impl VecKind {
    fn of(kind: MemKind, opts: VecOpts) -> Option<Self> {
        match kind {
            MemKind::Double => Some(Self::Double),
            MemKind::Float => Some(Self::Float),
            MemKind::Int => Some(Self::Int),
            MemKind::Long => Some(Self::Long),
            MemKind::Byte if opts.subword => Some(Self::Byte),
            MemKind::Short | MemKind::Char if opts.subword => Some(Self::Short),
            _ => None,
        }
    }

    /// The IR type every element-tree node of this kind has (a sub-word
    /// element is an `int` in the graph).
    fn ir_type(self) -> IrType {
        match self {
            Self::Double => IrType::Double,
            Self::Float => IrType::Float,
            Self::Int | Self::Byte | Self::Short => IrType::Int,
            Self::Long => IrType::Long,
        }
    }

    /// Bytes per element.
    pub(crate) fn elem_bytes(self) -> u8 {
        match self {
            Self::Double | Self::Long => 8,
            Self::Float | Self::Int => 4,
            Self::Short => 2,
            Self::Byte => 1,
        }
    }

    /// Elements per 256-bit register.
    pub(crate) fn lanes(self) -> i32 {
        32 / i32::from(self.elem_bytes())
    }

    /// A lane narrower than the `int` the graph computes in.
    pub(crate) fn is_subword(self) -> bool {
        matches!(self, Self::Byte | Self::Short)
    }
}

/// One arithmetic operation of the element tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VOp {
    Add,
    Sub,
    Mul,
    Div,
    /// VEC21-1 / VEC21-3.
    And,
    Or,
    Xor,
}

impl VOp {
    fn commutative(self) -> bool {
        matches!(self, Self::Add | Self::Mul | Self::And | Self::Or | Self::Xor)
    }
}

/// A shift by an immediate (VEC21-1): `int` and `long` lanes only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VShift {
    /// `<<`: `VPSLLD` / `VPSLLQ`.
    Shl,
    /// `>>` (`int` only): `VPSRAD`.
    Sar,
    /// `>>>`: `VPSRLD` / `VPSRLQ`.
    Shr,
}

/// The element tree of the store's value (or of a reduction's addend).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VExpr {
    /// `bases[i][j]`.
    Load(usize),
    /// `invariants[i]`, broadcast to every lane once, before the loop.
    Inv(usize),
    Bin(VOp, Box<VExpr>, Box<VExpr>),
    /// `e << imm` etc., `imm` already masked to the lane width.
    Shift(VShift, u8, Box<VExpr>),
    /// `e << shift_counts[i]` etc. (VEC21-6): a loop-invariant count, masked
    /// the Java way (`& 31` / `& 63`) into its own register once, before the
    /// loop.
    ShiftBy(VShift, usize, Box<VExpr>),
    /// L8-3: `Math.max(l, r)` (`max`) / `Math.min(l, r)`, with Java's
    /// argument order kept (an FP lane answers `l`'s NaN before `r`'s).
    MinMax {
        max: bool,
        l: Box<VExpr>,
        r: Box<VExpr>,
    },
    /// JIT round 24 B1: the induction itself, lane `i` of a step at `j`
    /// holding `j + i` (`int` lanes: the phi; `long` lanes: its `i2l`). A
    /// running register advanced by `lanes` after every step
    /// ([`iota_regs`]).
    Iota,
    /// `i2l(e)` over a 4-lane `int` expression, widened via `VPMOVSXDQ`.
    I2L(Box<VExpr>),
}

/// How a reduction's accumulator is updated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RedOp {
    /// `s = s + value` (VEC21-4, int/long).
    Add,
    /// `s = s - value` (VEC21-4, int/long).
    Sub,
    /// `m = Math.min(m, value)` / `Math.min(value, m)` (VEC21-5).
    Min,
    /// `m = Math.max(m, value)` / `Math.max(value, m)` (VEC21-5).
    Max,
}

impl RedOp {
    /// A min/max reduction: lanes start at the broadcast seed (min and max are
    /// idempotent), and the fold's answer REPLACES the accumulator.
    pub(crate) fn is_minmax(self) -> bool {
        matches!(self, Self::Min | Self::Max)
    }
}

/// A reduction loop's accumulator (VEC21-4, VEC21-5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VecReduce {
    /// The accumulator phi `s`; its back value is `s + value`, `s - value`,
    /// or `Math.min/max(s, value)` per `op`.
    pub(crate) acc: NodeId,
    pub(crate) op: RedOp,
}

/// The lane compare an early-exit scan (VEC21-8) evaluates per step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScanCmp {
    /// `VPCMPEQB/W/D/Q x, y`.
    Eq,
    /// `VPCMPGTB/W/D/Q x, y` (signed `x > y`).
    Gt,
    /// L8-4: unsigned 16-bit `x > y` over `char` lanes, as `VPMAXUW t, x, y;
    /// VPCMPEQW t, t, y` -- which sets the lanes where `y >= x`, i.e. the
    /// COMPLEMENT of `x > y` (the step flips the test's sense for it).
    GtU16,
    /// L8-4: `VCMPPD/PS x, y, imm` with this AVX predicate (FP lanes). The
    /// planner folds a test's negation into the predicate (`imm ^ 4`).
    Fp(u8),
}

/// One compare of a scan's test: `cmp(lhs, rhs)`, or its negation when
/// `miss`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScanTest {
    pub(crate) cmp: ScanCmp,
    /// The compare's left operand (`lhs > rhs` for [`ScanCmp::Gt`]): an
    /// element, an invariant, or (L8-4) an element tree.
    pub(crate) lhs: VExpr,
    pub(crate) rhs: VExpr,
    /// The test is the NEGATION of the compare (`!=`, `<=`, `>=` are the
    /// negations of `==`, `>`, `<`).
    pub(crate) miss: bool,
}

/// How a scan's tests combine into its hit predicate (L8-4): `a || b`,
/// `a && b`, nested along the chain of test blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScanHit {
    Test(usize),
    And(Box<ScanHit>, Box<ScanHit>),
    Or(Box<ScanHit>, Box<ScanHit>),
}

/// An early-exit scan (VEC21-8): `for (j..n) { if (hit(j)) leave; }`, where
/// `hit(j)` is `hit` over `tests` (one compare, before L8-4) of the elements
/// at `j`. The pre-loop only ADVANCES `j` past elements whose test is false,
/// and stops at the first lane whose test is true; the scalar loop,
/// untouched, re-tests that element and leaves at that very iteration.
/// Nothing else is written, so the state the scalar loop sees is the state it
/// would have reached by itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VecScan {
    pub(crate) tests: Vec<ScanTest>,
    pub(crate) hit: ScanHit,
    /// A sub-word scan's invariant operand must be a value the lane can hold
    /// (an `int` the element's own extension reproduces), else lanes compare
    /// its low bits only. `Some((bits, signed))` asks the pre-loop to test
    /// `x == (signed ? sext : zext)(x, bits)` and leave for the scalar loop
    /// otherwise (which answers every element by itself).
    pub(crate) narrow: Option<(u8, bool)>,
}

impl VecScan {
    /// A one-compare scan (the VEC21-8 shape).
    pub(crate) fn single(cmp: ScanCmp, lhs: VExpr, rhs: VExpr, miss: bool) -> Self {
        Self {
            tests: vec![ScanTest {
                cmp,
                lhs,
                rhs,
                miss,
            }],
            hit: ScanHit::Test(0),
            narrow: None,
        }
    }

    /// Does the step's mask hold the hit lanes' COMPLEMENT (the misses)?
    /// The same rule [`scan_step_code`] applies while it combines the tests
    /// (`eval_hit`), stated without emitting: the exit must agree with it.
    pub(crate) fn mask_complemented(&self) -> bool {
        fn go(s: &VecScan, h: &ScanHit) -> bool {
            match h {
                ScanHit::Test(i) => s.tests[*i].miss ^ (s.tests[*i].cmp == ScanCmp::GtU16),
                // `a & b`: both complemented gives `~(A | B)`; otherwise the
                // `VPANDN` forms answer the plain value.
                ScanHit::And(a, b) => go(s, a) && go(s, b),
                // `a | b`: neither complemented is the plain `A | B`.
                ScanHit::Or(a, b) => go(s, a) || go(s, b),
            }
        }
        go(self, &self.hit)
    }
}

/// L8-4: a scan whose sub-word elements are loaded widened to `int` lanes,
/// so an element tree is evaluated as Java evaluates it (in `int`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Widen {
    /// `byte[]` / `boolean[]`: `VPMOVSXBD`.
    Byte,
    /// `short[]`: `VPMOVSXWD`.
    Short,
    /// `char[]`: `VPMOVZXWD`.
    Char,
}

impl Widen {
    pub(crate) fn elem_bytes(self) -> u8 {
        match self {
            Self::Byte => 1,
            Self::Short | Self::Char => 2,
        }
    }
}

/// A loop [`plan_vector_loops`] admitted.
#[derive(Clone, Debug)]
pub(crate) struct VecLoopPlan {
    /// The loop header block.
    pub(crate) header: usize,
    /// The header's forward predecessor, whose edge carries the vector loop.
    pub(crate) entry_pred: usize,
    /// The header's bytecode pc, for diagnostics.
    pub(crate) header_bci: Option<usize>,
    /// Every block of the loop (the header first).
    pub(crate) loop_blocks: Vec<usize>,
    /// The induction phi `j`.
    pub(crate) iv: NodeId,
    /// The loop-invariant `int` bound: the loop stays while `j < bound`.
    pub(crate) bound: NodeId,
    /// `Some(array)` when `bound` is `array.length` read in the header every
    /// trip (`for (j = 0; j < row.length; j++)`, MX16-2's shape) with `array`
    /// invariant: the pre-loop reads the length once, behind a null test. A
    /// length never changes, so one read answers every trip's.
    pub(crate) bound_length_of: Option<NodeId>,
    pub(crate) kind: VecKind,
    /// The distinct array bases, all defined outside the loop.
    pub(crate) bases: Vec<NodeId>,
    /// `base_kinds[i]` is the element type `bases[i]` is read (or written) as.
    /// One narrower than `kind` (VEC21-2) is widened at its load, or (a store)
    /// narrowed at the store.
    pub(crate) base_kinds: Vec<MemKind>,
    /// The distinct scalar operands (constants included), all defined outside
    /// the loop.
    pub(crate) invariants: Vec<NodeId>,
    /// The distinct loop-invariant shift counts (VEC21-6), each masked into
    /// its own register (not broadcast: `VPSLLD ymm, ymm, xmm` reads the low
    /// 64 bits of `xmm` as ONE count).
    pub(crate) shift_counts: Vec<NodeId>,
    /// `bases[store_base][j] = value`. Unused (0) for a reduction.
    pub(crate) store_base: usize,
    pub(crate) value: VExpr,
    /// `Some` for a reduction loop (VEC21-4), which has no store: each step
    /// adds `value`'s lanes into the accumulator register instead.
    pub(crate) reduce: Option<VecReduce>,
    /// `Some` for an early-exit scan (VEC21-8): no store, no reduction, and
    /// `value` unused.
    pub(crate) scan: Option<VecScan>,
    /// L8-4: `Some` when the elements (of a scan) are loaded widened into the
    /// `int` lanes of `kind` ([`VecKind::Int`]).
    pub(crate) widen: Option<Widen>,
    /// JIT round 24 B1: `value` reads [`VExpr::Iota`], which takes two pinned
    /// registers after every other pinned one ([`iota_regs`]).
    pub(crate) iota: bool,
    /// JIT round 24 B1: `Some` when the store is a proven heap scoped put
    /// (`bases[store_base]` is its array) rather than an `ArrayStore`.
    pub(crate) scoped: Option<ScopedStore>,
}

/// JIT round 24 B1: a store loop's scoped put. The element `j` lies at
/// `bases[store_base] + start + j * kind.elem_bytes()` (a heap segment), or
/// at the decoded `start + j * kind.elem_bytes()` (a native one), and every
/// lane is in the segment when `n * elem_bytes <= length`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScopedStore {
    /// The segment's start (`HeapMemorySegmentImpl.offset`, the array base
    /// offset included; `NativeMemorySegmentImpl.min`, an address or an
    /// `Unsafe` arena handle), a loop-invariant `long`.
    pub(crate) start: NodeId,
    /// The segment (the access's receiver, live through the loop as one of
    /// its inputs) and its length field `(class_id, FieldLong(slot))`, which
    /// the pre-loop reads itself: the expansion's own length read is used by
    /// the bounds guards alone, so once they leave the loop its home may be
    /// reused before the loop entry.
    pub(crate) recv: NodeId,
    pub(crate) length_field: (u32, UnboxOp),
    /// A native segment: `bases[store_base]` is the literal null and the
    /// pre-loop resolves `start` itself.
    pub(crate) native: bool,
    /// A native segment's loop-entry session pre-check result
    /// (`UnboxOp::SessionPrecheck`), which must read null at run time for the
    /// pre-loop to run; `NO_NODE` when the session input is the literal null.
    pub(crate) session: NodeId,
}

/// JIT round 24 B1: where a plan's iota registers sit in `inv_regs` (after
/// the invariants, the shift counts and a narrowing store's mask): `(running,
/// step)`. `None` for a plan without [`VExpr::Iota`].
pub(crate) fn iota_regs(plan: &VecLoopPlan, inv_regs: &[u8]) -> Option<(u8, u8)> {
    if !plan.iota {
        return None;
    }
    let at = plan.invariants.len()
        + plan.shift_counts.len()
        + usize::from(narrow_store_mask(plan).is_some());
    Some((*inv_regs.get(at)?, *inv_regs.get(at + 1)?))
}

impl VecLoopPlan {
    /// Bytes of one array element in memory.
    pub(crate) fn mem_elem_bytes(&self) -> u8 {
        self.widen.map_or(self.kind.elem_bytes(), Widen::elem_bytes)
    }

    /// Elements one vector step covers.
    pub(crate) fn uses_avx512(&self) -> bool {
        avx512::eligible(self)
    }
    pub(crate) fn ordered_fp(&self) -> bool {
        matches!(self.kind, VecKind::Double | VecKind::Float)
            && self.reduce.is_some_and(|r| !r.op.is_minmax())
    }
    pub(crate) fn unroll(&self) -> usize {
        if self.uses_avx512() { 2 } else { UNROLL }
    }
    pub(crate) fn lanes(&self) -> i32 {
        self.kind.lanes() * if self.uses_avx512() { 2 } else { 1 }
    }

    /// Bytes of each array one step covers (the displacement between steps).
    pub(crate) fn step_bytes(&self) -> i32 {
        self.lanes() * i32::from(self.mem_elem_bytes())
    }
}

/// The GPR a scan step leaves its lane mask in (`VPMOVMSKB`): `BASE_REGS[3]`,
/// which is why a scan admits at most three arrays.
pub(crate) const SCAN_MASK_REG: u8 = 9; // R9

/// At most this many distinct arrays in a scan (R9 is the mask register).
pub(crate) const MAX_SCAN_BASES: usize = 3;

/// At most this many distinct array bases (the scratch GPRs that hold them).
pub(crate) const MAX_BASES: usize = 4;

/// Every loop of `schedule` the vector pre-loop may serve, and, per refused
/// candidate header, why not (for `CRATONVM_DBG_JITC`).
///
/// `block_of_ctrl` maps a control node to the block it heads (the lowerer's
/// `ctrl_block_of`), which is how a `Region`'s inputs name its predecessors.
pub(crate) fn plan_vector_loops(
    graph: &Graph,
    schedule: &Schedule,
    block_of_ctrl: &dyn Fn(NodeId) -> Option<usize>,
) -> (Vec<VecLoopPlan>, Vec<(usize, &'static str)>) {
    plan_vector_loops_with(graph, schedule, block_of_ctrl, VecOpts::from_flags())
}

/// The loop headers (their `Merge`/`Region` nodes) [`plan_vector_loops`]
/// admits in `graph` as it stands, scheduled afresh -- for the unroller, which
/// runs long before the lowering plans (the r23 L8 close, L8-1's residual).
/// Empty when the pre-loop cannot run here (switch off, no AVX2).
pub(crate) fn vector_loop_headers(graph: &Graph) -> Vec<NodeId> {
    if !ir_vector_loop_enabled() || !crate::x64::has_avx2() {
        return Vec::new();
    }
    let schedule = crate::ir_schedule::schedule(graph);
    let (plans, _) = plan_vector_loops(graph, &schedule, &|c| {
        crate::ir_lower::ctrl_block_of(graph, &schedule, c)
    });
    plans
        .iter()
        .filter_map(|p| schedule.blocks.get(p.header).map(|b| b.ctrl))
        .collect()
}

/// [`plan_vector_loops`] with the switches as an argument.
pub(crate) fn plan_vector_loops_with(
    graph: &Graph,
    schedule: &Schedule,
    block_of_ctrl: &dyn Fn(NodeId) -> Option<usize>,
    opts: VecOpts,
) -> (Vec<VecLoopPlan>, Vec<(usize, &'static str)>) {
    let mut plans = Vec::new();
    let mut refusals = Vec::new();
    let mut users: Option<Vec<Vec<NodeId>>> = None;
    for (h, block) in schedule.blocks.iter().enumerate() {
        // A loop header: a merge one of whose predecessors it dominates (the
        // builder gives loop headers `Merge` as well as `Region`).
        if !matches!(
            graph.nodes.get(block.ctrl as usize).map(|n| &n.op),
            Some(Op::Merge | Op::Region)
        ) || !block
            .predecessors
            .iter()
            .any(|&p| schedule.dom.dominates(h, p))
        {
            continue;
        }
        let users = users.get_or_insert_with(|| {
            let mut u: Vec<Vec<NodeId>> = vec![Vec::new(); graph.nodes.len()];
            for (id, n) in graph.nodes.iter().enumerate() {
                for &i in &n.inputs {
                    if let Some(list) = u.get_mut(i as usize) {
                        // Cast: an index into the node arena is a `NodeId`.
                        list.push(id as NodeId);
                    }
                }
            }
            u
        });
        match plan_one(graph, schedule, block_of_ctrl, users, h, opts) {
            Ok(p) => plans.push(p),
            Err(why) => refusals.push((h, why)),
        }
    }
    (plans, refusals)
}

/// The constant value of `id`, if it is an integer constant.
fn const_of(graph: &Graph, id: NodeId) -> Option<i64> {
    match graph.nodes.get(id as usize).map(|n| &n.op) {
        Some(Op::Const(v)) => Some(*v),
        _ => None,
    }
}

/// A widening, `lcmp` or `long` add nothing but a frame reads: a narrowed
/// `long` induction's frames still name `i2l(phi)`, `i2l(length)`, the `lcmp`
/// of the compare and the old increment (JIT r27 LOOP27-2).
pub(crate) fn frame_only_wide(graph: &Graph, users: &[Vec<NodeId>], id: NodeId) -> bool {
    graph.nodes.get(id as usize).is_some_and(|n| {
        matches!(n.op, Op::I2L | Op::LCmp) || (n.ty == IrType::Long && matches!(n.op, Op::Add | Op::Sub))
    })
        && users[id as usize].iter().all(|&u| frame_only_wide(graph, users, u))
}

fn plan_one(
    graph: &Graph,
    schedule: &Schedule,
    block_of_ctrl: &dyn Fn(NodeId) -> Option<usize>,
    users: &[Vec<NodeId>],
    h: usize,
    opts: VecOpts,
) -> Result<VecLoopPlan, &'static str> {
    let node = |id: NodeId| graph.nodes.get(id as usize);
    let hb = &schedule.blocks[h];
    let region = node(hb.ctrl).ok_or("no header node")?;
    if region.inputs.len() != 2 || hb.predecessors.len() != 2 {
        return Err("header has other than two predecessors");
    }
    let b0 = block_of_ctrl(region.inputs[0]).ok_or("a header input has no block")?;
    let b1 = block_of_ctrl(region.inputs[1]).ok_or("a header input has no block")?;
    // Which input is the back edge: the one the header dominates. The phis'
    // inputs follow the merge's (`[merge, value for input 0, for input 1]`).
    let (entry_pred, latch, back_slot) = if schedule.dom.dominates(h, b1) {
        (b0, b1, 2)
    } else {
        (b1, b0, 1)
    };
    if !schedule.dom.dominates(h, latch) || schedule.dom.dominates(h, entry_pred) {
        return Err("header inputs are not one entry and one back edge");
    }
    // The loop is the header and ONE stay block, which is the latch.
    let term = hb.terminator.ok_or("header has no terminator")?;
    if node(term).map(|n| &n.op) != Some(&Op::If) || hb.successors.len() != 2 {
        return Err("header does not end in a two-way test");
    }
    let lb = &schedule.blocks[latch];
    let eb = &schedule.blocks[entry_pred];
    if eb.successors.as_slice() != [h] || eb.terminator.is_some() {
        return Err("entry edge is not a goto");
    }
    // VEC21-8: a test block between the header and the latch is an
    // early-exit scan's shape; it has its own planner. L8-4: so is a chain of
    // test blocks, whose edges may meet at the latch.
    let scan_shape = if opts.scan_wide {
        lb.predecessors.as_slice() != [h]
    } else {
        lb.predecessors.len() == 1 && lb.predecessors[0] != h
    };
    if opts.scan && scan_shape {
        return plan_scan(
            graph, schedule, users, h, entry_pred, latch, back_slot, opts,
        );
    }
    if !hb.successors.contains(&latch)
        || lb.successors.as_slice() != [h]
        || lb.predecessors.as_slice() != [h]
        || lb.terminator.is_some()
    {
        return Err("body is not one straight-line latch block");
    }
    let stay_true = match node(lb.ctrl).map(|n| &n.op) {
        Some(Op::Proj(0)) => true,
        Some(Op::Proj(1)) => false,
        _ => return Err("latch is not a projection of the test"),
    };
    let in_loop = |id: NodeId| {
        schedule
            .node_to_block
            .get(id as usize)
            .is_some_and(|&b| b == h || b == latch)
    };
    // Defined outside the loop, in a block every loop path has run: the
    // header's dominators. A constant is unscheduled-safe.
    let invariant = |id: NodeId| {
        let Some(n) = node(id) else {
            return false;
        };
        if matches!(n.op, Op::Const(_) | Op::ConstF(_)) {
            return true;
        }
        schedule.node_to_block.get(id as usize).is_some_and(|&b| {
            b < schedule.blocks.len() && b != h && b != latch && schedule.dom.dominates(b, h)
        })
    };
    // A pure value the schedule put in the loop but only code after the
    // loop reads (`doubleToRawLongBits(m)` for the `return`, placed at
    // the header) is no in-loop read: every evaluation of it the
    // pre-loop skips is unobservable, and the scalar header recomputes
    // it from the written-back accumulator (the r23 L8 close).
    let exit_only = |u: NodeId| {
        graph.nodes.get(u as usize).is_some_and(|n| {
            matches!(
                n.op,
                Op::ScalarIntrinsic(_)
                    | Op::I2L
                    | Op::L2I
                    | Op::I2F
                    | Op::I2D
                    | Op::L2F
                    | Op::L2D
                    | Op::F2D
                    | Op::D2F
            ) && users[u as usize].iter().all(|&x| !in_loop(x))
                // At the header, of header phis and invariants only: then the
                // value the exit reads is the one the header computes from the
                // state the pre-loop wrote back.
                && schedule.node_to_block.get(u as usize) == Some(&h)
                && n.inputs.iter().all(|&i| {
                    !in_loop(i)
                        || graph.nodes.get(i as usize).is_some_and(|x| {
                            x.op == Op::Phi && x.inputs.first() == Some(&hb.ctrl)
                        })
                })
        })
    };
    // Header phis: one Int induction (plus, for a reduction, one Int/Long
    // accumulator), any number of memory phis.
    let max_value_phis = 1 + usize::from(opts.reduce || opts.minmax);
    let mut value_phis: Vec<NodeId> = Vec::new();
    let mut mem_phis: Vec<NodeId> = Vec::new();
    for (id, n) in graph.nodes.iter().enumerate() {
        // Cast: an index into the node arena is a `NodeId`.
        let id = id as NodeId;
        if n.op != Op::Phi || n.inputs.first() != Some(&hb.ctrl) {
            continue;
        }
        if n.inputs.len() != 3 {
            return Err("a header phi with other than two inputs");
        }
        match n.ty {
            IrType::Memory => mem_phis.push(id),
            // Round 16's rule (one value phi, and it is an `int`) is kept
            // verbatim for the first: the type is judged below.
            _ if value_phis.len() < max_value_phis => value_phis.push(id),
            _ => return Err("the loop carries a value besides its induction"),
        }
    }
    // Does `phi` step by exactly one in the loop (`phi + 1`, either order)?
    let steps_by_one = |phi: NodeId| -> bool {
        let n = &graph.nodes[phi as usize];
        if n.ty != IrType::Int {
            return false;
        }
        let next = n.inputs[back_slot];
        in_loop(next)
            && node(next).is_some_and(|m| {
                m.op == Op::Add
                    && m.ty == IrType::Int
                    && m.inputs.len() == 2
                    && ((m.inputs[0] == phi && is_const(graph, m.inputs[1], 1))
                        || (m.inputs[1] == phi && is_const(graph, m.inputs[0], 1)))
            })
    };
    let (iv, acc) = match value_phis.as_slice() {
        [] => return Err("no int induction phi"),
        [only] => {
            if graph.nodes[*only as usize].ty != IrType::Int {
                return Err("the loop carries a value besides its induction");
            }
            (*only, NO_NODE)
        }
        [a, b] => match (steps_by_one(*a), steps_by_one(*b)) {
            (true, false) => (*a, *b),
            (false, true) => (*b, *a),
            (true, true) => return Err("two counters that both step by one"),
            (false, false) => return Err("the induction does not step by one in the loop"),
        },
        _ => return Err("the loop carries a value besides its induction"),
    };
    let iv_next = graph.nodes[iv as usize].inputs[back_slot];
    if !steps_by_one(iv) {
        return Err("the induction does not step by one in the loop");
    }
    // The accumulator: `acc_next = acc + e` / `e + acc` / `acc - e`, read by
    // nothing but the back edge, and `acc` read in the loop only by it.
    let mut reduce: Option<(VecReduce, NodeId, NodeId)> = None;
    if acc != NO_NODE {
        let an = &graph.nodes[acc as usize];
        let acc_next = an.inputs[back_slot];
        let nx = node(acc_next).ok_or("dangling accumulator")?;
        // VEC21-5: `Math.min/max(m, e)` (either operand order: with no NaN
        // and -0.0 ordered below +0.0 both are commutative and associative,
        // and a NaN anywhere sends the call to the scalar loop).
        let minmax = match nx.op {
            Op::ScalarIntrinsic(sop) if opts.minmax => match sop {
                ScalarOp::MinI | ScalarOp::MinL | ScalarOp::MinF | ScalarOp::MinD => {
                    Some(RedOp::Min)
                }
                ScalarOp::MaxI | ScalarOp::MaxL | ScalarOp::MaxF | ScalarOp::MaxD => {
                    Some(RedOp::Max)
                }
                _ => return Err("the accumulator is updated by a non-min/max intrinsic"),
            },
            _ => None,
        };
        if minmax.is_none() && !matches!(an.ty, IrType::Int | IrType::Long)
            && !(matches!(an.ty, IrType::Double | IrType::Float) && avx512::ordered_enabled()) {
            return Err("a reduction over a non-integer accumulator");
        }
        if minmax.is_none() && !opts.reduce {
            return Err("the loop carries a value besides its induction");
        }
        if !in_loop(acc_next) || nx.ty != an.ty || nx.inputs.len() != 2 {
            return Err("the accumulator is not updated by one add in the loop");
        }
        let (e, op) = match (&nx.op, minmax) {
            (_, Some(m)) if nx.inputs[0] == acc && nx.inputs[1] != acc => (nx.inputs[1], m),
            (_, Some(m)) if nx.inputs[1] == acc && nx.inputs[0] != acc => (nx.inputs[0], m),
            (Op::Add, None) if nx.inputs[0] == acc && nx.inputs[1] != acc => {
                (nx.inputs[1], RedOp::Add)
            }
            (Op::Add, None) if nx.inputs[1] == acc && nx.inputs[0] != acc => {
                (nx.inputs[0], RedOp::Add)
            }
            (Op::Sub, None) if nx.inputs[0] == acc && nx.inputs[1] != acc => {
                (nx.inputs[1], RedOp::Sub)
            }
            _ => return Err("the accumulator is not updated by one add in the loop"),
        };
        if minmax.is_none() && matches!(an.ty, IrType::Double | IrType::Float)
            && nx.inputs[0] != acc {
            return Err("ordered FP requires accumulator as the first operand");
        }
        if users[acc_next as usize].iter().any(|&u| u != acc) {
            return Err("the accumulated value is read besides the back edge");
        }
        if users[acc as usize]
            .iter()
            .any(|&u| u != acc_next && in_loop(u) && !exit_only(u))
        {
            return Err("the accumulator is read in the loop besides its add");
        }
        reduce = Some((VecReduce { acc, op }, acc_next, e));
    }
    let acc_next = reduce.map_or(NO_NODE, |(_, n, _)| n);
    // The test: stay while `iv < bound`.
    let cond = node(term)
        .and_then(|n| n.inputs.get(1).copied())
        .ok_or("test has no condition")?;
    let cn = node(cond).ok_or("no condition node")?;
    let Op::Cmp(op) = cn.op else {
        return Err("test is not an int compare");
    };
    if cn.inputs.len() != 2 {
        return Err("compare arity");
    }
    let stay = if stay_true { op } else { op.negate() };
    let bound = match stay {
        CmpOp::Lt if cn.inputs[0] == iv => cn.inputs[1],
        CmpOp::Gt if cn.inputs[1] == iv => cn.inputs[0],
        // BCE21-5 on the vector side: `j != n` stays exactly while `j < n`
        // for every `j` the pre-loop runs (it steps only while `j + W <= n`),
        // and a `j > n` entry takes no step: the scalar loop answers it.
        CmpOp::Ne if opts.ne_exit && cn.inputs[0] == iv => cn.inputs[1],
        CmpOp::Ne if opts.ne_exit && cn.inputs[1] == iv => cn.inputs[0],
        _ => return Err("test is not `iv < bound`"),
    };
    let bound_length_of = match node(bound) {
        Some(n) if n.op == Op::ArrayLength && in_loop(bound) => {
            let arr = match n.inputs.as_slice() {
                [a] => *a,
                [_, _, a, ..] => *a,
                _ => return Err("array length arity"),
            };
            if !invariant(arr) || users[bound as usize].iter().any(|&u| u != cond && !frame_only_wide(graph, users, u)) {
                return Err("bound is the length of a variant array");
            }
            Some(arr)
        }
        _ => None,
    };
    if bound_length_of.is_none()
        && (!invariant(bound) || node(bound).map(|n| n.ty) != Some(IrType::Int))
    {
        return Err("bound is not a loop-invariant int");
    }
    // Every node the schedule put in the loop.
    let mut kind: Option<VecKind> = None;
    let mut store = NO_NODE;
    let mut members: Vec<NodeId> = Vec::new();
    // VEC21-2: under `widen` the lanes are the store's type (or the
    // accumulator's) and a load may be narrower; collected here, judged after.
    let mut load_kinds: Vec<MemKind> = Vec::new();
    let mut store_kind: Option<MemKind> = None;
    // JIT round 24 B1: a proven heap scoped put, and the address nodes
    // (`start + i2l(iv) * width`) only it reads -- the step addresses the
    // element itself, so they are no member of the value tree.
    let mut scoped_put: Option<(NodeId, ScopedStore, MemKind)> = None;
    let mut addr_nodes: Vec<NodeId> = Vec::new();
    if opts.scoped_store {
        for id in 0..graph.nodes.len() {
            // Cast: an index into the node arena is a `NodeId`.
            let id = id as NodeId;
            if !in_loop(id) {
                continue;
            }
            if let Op::Unbox {
                op:
                    UnboxOp::ScopedPut {
                        width,
                        ordered: true,
                    },
                ..
            } = graph.nodes[id as usize].op
            {
                if scoped_put.is_some() {
                    return Err("more than one store");
                }
                let (st, mk, addr) =
                    scoped_access_shape(graph, users, id, width, iv, &in_loop, &invariant, true)?;
                scoped_put = Some((id, st, mk));
                addr_nodes = addr;
            }
        }
    }
    // j27 VEC27-1: a reduction's one scoped get, addressed like B1's put.
    let mut scoped_get: Option<(NodeId, ScopedStore, MemKind)> = None;
    if opts.scoped_get && acc != NO_NODE {
        let found = find_scoped_get(graph, users, iv, &in_loop, &invariant)?;
        if let Some((id, st, mk, addr)) = found {
            if scoped_put.is_some() {
                return Err("a reduction loop that also stores");
            }
            scoped_get = Some((id, st, mk));
            addr_nodes = addr;
        }
    }
    for id in 0..graph.nodes.len() {
        // Cast: an index into the node arena is a `NodeId`.
        let id = id as NodeId;
        if !in_loop(id) {
            continue;
        }
        let n = &graph.nodes[id as usize];
        // The loop's own control (its merge, test and projections) was
        // checked above; nothing else of it is control-flow.
        if n.op.is_control() {
            if id != hb.ctrl && id != term && id != lb.ctrl {
                return Err("control flow inside the body");
            }
            continue;
        }
        let mut set_kind = |k: MemKind| -> Result<(), &'static str> {
            let k = VecKind::of(k, opts).ok_or("element type has no vector form")?;
            match kind {
                None => kind = Some(k),
                Some(prev) if prev == k => {}
                Some(_) => return Err("mixed element types"),
            }
            Ok(())
        };
        match &n.op {
            Op::Phi => {
                if id != iv && id != acc && !mem_phis.contains(&id) {
                    return Err("a foreign phi in the loop");
                }
            }
            Op::Cmp(_) if id == cond => {}
            Op::ArrayLength if id == bound && bound_length_of.is_some() => {}
            // A narrowed `long` induction's frames still name `i2l(length)` (JIT r27 LOOP27-2).
            Op::I2L | Op::LCmp | Op::Add | Op::Sub if frame_only_wide(graph, users, id) => {}
            Op::Add if id == iv_next => {}
            Op::Add | Op::Sub if id == acc_next => {}
            Op::ScalarIntrinsic(_) if id == acc_next => {}
            // L8-3: judged per kind in `build_expr`.
            Op::ScalarIntrinsic(sop) if opts.elem_minmax && minmax_of(*sop).is_some() => {
                members.push(id)
            }
            Op::Const(_) | Op::ConstF(_) => {}
            // JIT round 24 B1: the scoped put's address arithmetic.
            _ if addr_nodes.contains(&id) => {}
            Op::Unbox {
                op: UnboxOp::ScopedPut { .. },
                ..
            } if scoped_put.is_some_and(|(s, _, _)| s == id) => {
                if store != NO_NODE {
                    return Err("more than one store");
                }
                let mk = scoped_put.map_or(MemKind::Int, |(_, _, k)| k);
                if opts.widen {
                    store_kind = Some(mk);
                } else {
                    set_kind(mk)?;
                }
                store = id;
            }
            // j27 VEC27-1: the reduction's scoped get, an element read.
            Op::Unbox {
                op: UnboxOp::ScopedGet { .. },
                ..
            } if scoped_get.is_some_and(|(g, _, _)| g == id) => {
                let mk = scoped_get.map_or(MemKind::Int, |(_, _, k)| k);
                if opts.widen {
                    load_kinds.push(mk);
                } else {
                    set_kind(mk)?;
                }
                members.push(id);
            }
            // Read after the loop only (see `exit_only`).
            _ if exit_only(id) => {}
            Op::ArrayLoad(k) => {
                if n.inputs.len() != 4 || n.inputs[3] != iv || !invariant(n.inputs[2]) {
                    return Err("an element read not at [invariant][iv]");
                }
                if opts.widen {
                    load_kinds.push(*k);
                } else {
                    set_kind(*k)?;
                }
                members.push(id);
            }
            Op::ArrayStore(k) => {
                if n.inputs.len() != 5 || n.inputs[3] != iv || !invariant(n.inputs[2]) {
                    return Err("an element store not at [invariant][iv]");
                }
                if store != NO_NODE {
                    return Err("more than one store");
                }
                if opts.widen {
                    store_kind = Some(*k);
                } else {
                    set_kind(*k)?;
                }
                store = id;
            }
            Op::Add | Op::Sub | Op::Mul | Op::Div => members.push(id),
            // VEC21-2: a widening conversion, judged in `build_expr` (only
            // directly over an element load).
            Op::I2L | Op::F2D if opts.widen => members.push(id),
            // VEC21-1 / VEC21-3: judged per kind in `build_expr`.
            Op::And | Op::Or | Op::Xor | Op::Shl | Op::Shr | Op::UShr
                if opts.bitops || opts.subword =>
            {
                members.push(id)
            }
            other => {
                if false {
                    eprintln!("[ir-vec]   n{id} {other:?} has no vector form");
                }
                return Err("an op with no vector form in the loop");
            }
        }
    }
    if opts.widen {
        // VEC21-2: the lanes are the type the loop writes (its store, or its
        // accumulator); every load must be that type or one widened into it.
        if load_kinds.is_empty() && store_kind.is_none() && reduce.is_none() {
            return Err("no array access");
        }
        let all_readable = |k: VecKind| load_kinds.iter().all(|&l| lane_readable(l, k, opts));
        let k = match (store_kind, reduce) {
            // The store's own lanes when every load fits them; else (the
            // narrowing half of VEC21-2) `int` lanes truncated into a
            // `byte`/`short`/`char` store, when every load fits those.
            (Some(s), _) => match VecKind::of(s, opts) {
                Some(sk) if all_readable(sk) => sk,
                _ if opts.narrow
                    && matches!(s, MemKind::Byte | MemKind::Short | MemKind::Char)
                    && all_readable(VecKind::Int) =>
                {
                    VecKind::Int
                }
                Some(_) => return Err("mixed element types"),
                None => return Err("element type has no vector form"),
            },
            (None, Some((r, _, _))) => match graph.nodes[r.acc as usize].ty {
                IrType::Int => VecKind::Int,
                IrType::Long => VecKind::Long,
                IrType::Float if !load_kinds.is_empty() => VecKind::Float,
                IrType::Double if !load_kinds.is_empty() => VecKind::Double,
                _ => return Err("a reduction over a non-numeric accumulator"),
            },
            (None, None) => return Err("no store"),
        };
        if !all_readable(k) {
            return Err("mixed element types");
        }
        kind = Some(k);
    }
    let kind = match (kind, reduce) {
        (Some(k), _) => k,
        (None, Some((r, _, _))) => match graph.nodes[r.acc as usize].ty {
            IrType::Int => VecKind::Int,
            IrType::Long => VecKind::Long,
            _ => return Err("no array access"),
        },
        _ => return Err("no array access"),
    };
    match reduce {
        Some((r, _, _)) => {
            if store != NO_NODE {
                return Err("a reduction loop that also stores");
            }
            // VEC21-5's min/max serves FP lanes too; a sum stays integral.
            let lanes_ok = if r.op.is_minmax() {
                matches!(
                    kind,
                    VecKind::Int | VecKind::Long | VecKind::Float | VecKind::Double
                )
            } else {
                matches!(kind, VecKind::Int | VecKind::Long)
                    || (matches!(kind, VecKind::Double | VecKind::Float) && avx512::ordered_enabled())
            };
            if !lanes_ok || graph.nodes[r.acc as usize].ty != kind.ir_type() {
                return Err("a reduction of another type than its elements");
            }
        }
        None => {
            if store == NO_NODE {
                return Err("no store");
            }
        }
    }
    // Every member is read inside the loop only (its value does not escape).
    for &m in &members {
        if users[m as usize].iter().any(|&u| !in_loop(u)) {
            return Err("a body value is used after the loop");
        }
    }
    if users[iv_next as usize].iter().any(|&u| u != iv && !frame_only_wide(graph, users, u)) {
        return Err("the stepped induction is read besides the back edge");
    }
    let mut tree = TreeOut::default();
    let (store_base, root) = match (reduce, scoped_put) {
        (Some((_, _, e)), _) => (0, e),
        // JIT round 24 B1: `[ctrl, mem, recv, scope, base, addr, value, be]`.
        (None, Some((_, _, mk))) => {
            let st = &graph.nodes[store as usize];
            (tree.base_index(st.inputs[4], mk)?, st.inputs[6])
        }
        (None, None) => {
            let st = &graph.nodes[store as usize];
            let Op::ArrayStore(sk) = st.op else {
                return Err("no store");
            };
            (tree.base_index(st.inputs[2], sk)?, st.inputs[4])
        }
    };
    let ctx = TreeCtx {
        graph,
        kind,
        opts,
        in_loop: &in_loop,
        invariant: &invariant,
        check_elems: true,
        iv,
        iota: opts.scoped_store
            || (load_kinds.is_empty() && store_kind.is_none() && reduce.is_some()),
        scoped_get: scoped_get.map(|(g, _, k)| (g, k)),
    };
    let value = build_expr(&ctx, root, &mut tree, 0)?;
    // Every member was reached from the store (nothing computed and dropped,
    // no load whose only purpose was a fault).
    if members.iter().any(|m| !tree.seen.contains(m)) {
        return Err("a body value the store does not read");
    }
    // JIT round 24 B1: a scoped store's segment may overlay any array at any
    // byte offset (`asSlice`), so an element read beside it may be a
    // carried dependence the step would reorder: none is admitted.
    if scoped_put.is_some() && (tree.bases.len() != 1 || expr_reads_element(&value)) {
        return Err("a scoped store loop that reads an array");
    }
    // j27 VEC27-1: the pre-loop rebases ONE base register (RAX) onto the
    // segment's start, so the get's base must be the plan's only one, and no
    // plain element read may share it (it would be read at the rebased RAX).
    let reads_plain_element = members
        .iter()
        .any(|&m| matches!(graph.nodes[m as usize].op, Op::ArrayLoad(_)));
    if scoped_get.is_some() && (tree.bases.len() != 1 || reads_plain_element) {
        return Err("a scoped get reduction that reads another array");
    }
    let plan = VecLoopPlan {
        header: h,
        entry_pred,
        header_bci: region.bytecode_pc,
        loop_blocks: vec![h, latch],
        iv,
        bound,
        bound_length_of,
        kind,
        bases: tree.bases,
        base_kinds: tree.base_kinds,
        invariants: tree.invariants,
        shift_counts: tree.shift_counts,
        store_base,
        value,
        reduce: reduce.map(|(r, _, _)| r),
        scan: None,
        widen: None,
        iota: tree.iota,
        scoped: scoped_put.or(scoped_get).map(|(_, s, _)| s),
    };
    if plan.ordered_fp() && !plan.uses_avx512() {
        return Err("ordered FP reduction has no admitted wide form");
    }
    Ok(plan)
}

/// j27 VEC27-1: the one little-endian scoped get (`width` 4 or 8) of the
/// loop, with what [`scoped_access_shape`] proves of it: `(get, where its
/// elements lie, its element type, its address nodes)`. `Ok(None)` when the
/// loop holds none; a second one, or one of another shape, refuses the loop
/// (the caller asked only for a reduction, which the get must feed).
fn find_scoped_get(
    graph: &Graph,
    users: &[Vec<NodeId>],
    iv: NodeId,
    in_loop: &dyn Fn(NodeId) -> bool,
    invariant: &dyn Fn(NodeId) -> bool,
) -> Result<Option<(NodeId, ScopedStore, MemKind, Vec<NodeId>)>, &'static str> {
    let mut found = None;
    for (i, n) in graph.nodes.iter().enumerate() {
        // Cast: an index into the node arena is a `NodeId`.
        let id = i as NodeId;
        let Op::Unbox {
            op: UnboxOp::ScopedGet { width, ordered, .. },
            ..
        } = n.op
        else {
            continue;
        };
        if !in_loop(id) {
            continue;
        }
        if found.is_some() {
            return Err("more than one scoped get");
        }
        if !ordered {
            return Err("a scoped get with no byte order operand");
        }
        let (st, mk, addr) =
            scoped_access_shape(graph, users, id, width, iv, in_loop, invariant, false)?;
        found = Some((id, st, mk, addr));
    }
    Ok(found)
}

/// JIT round 24 B1: a scoped put `id` the vector pre-loop may serve (j27
/// VEC27-1: or, `put == false`, a scoped get), as `(where its elements lie,
/// its element type, its address nodes)`. Inputs `[ctrl, mem, recv, scope,
/// base, addr, value, be]` (a get: `[ctrl, mem, recv, scope, base, addr,
/// be]`) with
///
/// * the access proven in bounds of a non-null primitive array
///   (`Graph::ffm_proven_heap_accesses`: the expansion's guards, the bounds
///   among them, all outside the loop by now or the loop holds a guard);
/// * a little-endian order (`be == 0`), a session that needs no check (the
///   literal null, or `Graph::ffm_never_closing_scopes`), a loop-invariant
///   receiver and base;
/// * `addr = start + i2l(iv) * width` with `start` loop-invariant, each
///   address node read by the next one alone;
/// * the segment's length field on file (`Graph::ffm_scoped_length_fields`),
///   which the pre-loop reads from the receiver itself.
#[allow(clippy::too_many_arguments)]
fn scoped_access_shape(
    graph: &Graph,
    users: &[Vec<NodeId>],
    id: NodeId,
    width: u8,
    iv: NodeId,
    in_loop: &dyn Fn(NodeId) -> bool,
    invariant: &dyn Fn(NodeId) -> bool,
    put: bool,
) -> Result<(ScopedStore, MemKind, Vec<NodeId>), &'static str> {
    let n = &graph.nodes[id as usize];
    // A B2-cached access carries its arena cache inputs after these. The
    // value whose type must be the lanes': the put's operand, the get itself.
    let (recv, scope, base, addr, value, be) = match (put, n.inputs.as_slice()) {
        (true, [_, _, r, s, b, a, v, e, ..]) => (r, s, b, a, *v, e),
        (false, [_, _, r, s, b, a, e, ..]) => (r, s, b, a, id, e),
        _ => return Err("a scoped store of another arity"),
    };
    let is_null = |x: NodeId| {
        graph
            .nodes
            .get(x as usize)
            .is_some_and(crate::ir::is_null_const)
    };
    // A native segment's put has the literal-null base (the expansion builds
    // one only with the JIT-readable arena table); its session is the
    // loop-entry pre-check, tested once by the pre-loop.
    let native = is_null(*base);
    let session = if native {
        let prechecked = graph.nodes.get(*scope as usize).is_some_and(|s| {
            matches!(
                s.op,
                Op::Unbox {
                    op: UnboxOp::SessionPrecheck,
                    ..
                }
            )
        });
        match (prechecked, is_null(*scope)) {
            (true, _) => *scope,
            (false, true) => NO_NODE,
            (false, false) => return Err("a native scoped store whose session is not pre-checked"),
        }
    } else {
        if !graph.ffm_proven_heap_accesses.contains(&id) {
            return Err("a scoped store not proven in bounds");
        }
        if !is_null(*scope) && !graph.ffm_never_closing_scopes.contains(scope) {
            return Err("a scoped store whose session needs a check");
        }
        NO_NODE
    };
    if const_of(graph, *be) != Some(0) {
        return Err("a big-endian scoped store");
    }
    if !invariant(*recv) || !invariant(*base) || (session != NO_NODE && !invariant(session)) {
        return Err("a scoped store on a variant segment");
    }
    let (mk, ty) = match width {
        4 => (MemKind::Int, IrType::Int),
        8 => (MemKind::Long, IrType::Long),
        _ => return Err("a scoped store of no vector width"),
    };
    if graph.nodes.get(value as usize).map(|v| v.ty) != Some(ty) {
        return Err("a scoped store of a non-integral value");
    }
    let node = |x: NodeId| graph.nodes.get(x as usize);
    let only_read_by = |x: NodeId, r: NodeId| users[x as usize].iter().all(|&u| u == r);
    let an = node(*addr).ok_or("dangling address")?;
    if an.op != Op::Add || an.ty != IrType::Long || an.inputs.len() != 2 || !in_loop(*addr) {
        return Err("a scoped address that is not `start + k * width`");
    }
    let (start, mul) = match (invariant(an.inputs[0]), invariant(an.inputs[1])) {
        (true, false) => (an.inputs[0], an.inputs[1]),
        (false, true) => (an.inputs[1], an.inputs[0]),
        _ => return Err("a scoped address that is not `start + k * width`"),
    };
    let mn = node(mul).ok_or("dangling address")?;
    if mn.op != Op::Mul || mn.inputs.len() != 2 || !in_loop(mul) {
        return Err("a scoped address that is not `start + k * width`");
    }
    let i2l = match (
        const_of(graph, mn.inputs[0]) == Some(i64::from(width)),
        const_of(graph, mn.inputs[1]) == Some(i64::from(width)),
    ) {
        (false, true) => mn.inputs[0],
        (true, false) => mn.inputs[1],
        _ => return Err("a scoped address that is not `start + k * width`"),
    };
    let iln = node(i2l).ok_or("dangling address")?;
    if iln.op != Op::I2L || iln.inputs.first() != Some(&iv) {
        return Err("a scoped address that is not `start + k * width`");
    }
    // A B2-cached put (`ir_optimize::ffm_arena_cache`) reads `k * width`
    // itself too, as its cache `delta` input; the scalar remainder lowers the
    // put as it is.
    let mul_reader_ok = |u: NodeId| u == *addr || (u == id && graph.ffm_arena_cached.contains(&id));
    if !only_read_by(*addr, id) || !users[mul as usize].iter().all(|&u| mul_reader_ok(u)) {
        return Err("a scoped address read besides its store");
    }
    // The `i2l` may feed a `long` value too (`VExpr::Iota`); never the
    // code after the loop.
    if users[i2l as usize].iter().any(|&u| !in_loop(u)) {
        return Err("a scoped address read besides its store");
    }
    let &length_field = graph
        .ffm_scoped_length_fields
        .get(&id)
        .ok_or("a scoped store with no length field")?;
    if !matches!(length_field.1, UnboxOp::FieldLong(_)) {
        return Err("a scoped store with no length field");
    }
    Ok((
        ScopedStore {
            start,
            recv: *recv,
            length_field,
            native,
            session,
        },
        mk,
        vec![*addr, mul, i2l],
    ))
}

/// `Some((max, lane type))` for a `Math.min/max` intrinsic (L8-3).
fn minmax_of(sop: ScalarOp) -> Option<(bool, IrType)> {
    Some(match sop {
        ScalarOp::MaxI => (true, IrType::Int),
        ScalarOp::MinI => (false, IrType::Int),
        ScalarOp::MaxL => (true, IrType::Long),
        ScalarOp::MinL => (false, IrType::Long),
        ScalarOp::MaxF => (true, IrType::Float),
        ScalarOp::MinF => (false, IrType::Float),
        ScalarOp::MaxD => (true, IrType::Double),
        ScalarOp::MinD => (false, IrType::Double),
        _ => return None,
    })
}

/// VEC21-8: plan an early-exit scan. The shape, after [`plan_one`] found the
/// entry edge and the back edge:
///
/// ```text
///   header h:  if (j < n) -> body   else -> exit
///   body   b:  if (hit(a[j], ..)) -> exit2   else -> latch     (or the mirror)
///   latch  l:  j = j + 1; goto h
/// ```
///
/// with no store, no call, no value phi but `j`, and every in-loop node one of
/// the loop's own tests, the step, an element load at `[invariant][j]` or a
/// constant -- so a trip that does not hit has no effect but `j + 1`, and the
/// pre-loop may run any number of them (guarded in bounds) before handing `j`
/// to the scalar loop. Values read in the loop may be used after it: the
/// scalar loop, which computes them, is the one that leaves.
///
/// L8-4 (`CRATONVM_JIT_IR_VECTOR_SCAN_WIDE`): `b` may be a CHAIN of test
/// blocks (and plain gotos), each test's two edges going to the latch, out of
/// the loop, or to the next test, so the hit is a formula of `||` / `&&` over
/// the compares ([`ScanHit`]); a compare's operands may be element trees, its
/// lanes FP (`fcmpl`/`fcmpg` + `if<cond>`), and a `char` compare an order
/// compare. A trip with no hit still has no effect but `j + 1`: every test of
/// it is evaluated in every lane, and none can fault (the guards keep every
/// load in bounds; a tree has no integer division).
#[allow(clippy::too_many_arguments)]
fn plan_scan(
    graph: &Graph,
    schedule: &Schedule,
    users: &[Vec<NodeId>],
    h: usize,
    entry_pred: usize,
    latch: usize,
    back_slot: usize,
    opts: VecOpts,
) -> Result<VecLoopPlan, &'static str> {
    let node = |id: NodeId| graph.nodes.get(id as usize);
    let hb = &schedule.blocks[h];
    let term = hb.terminator.ok_or("header has no terminator")?;
    let lb = &schedule.blocks[latch];
    if lb.successors.as_slice() != [h] || lb.terminator.is_some() {
        return Err("scan: body is not a test block then a latch");
    }
    // The loop's blocks: the natural loop of the back edge `latch -> h`.
    let mut body: Vec<usize> = vec![h, latch];
    let mut work = vec![latch];
    while let Some(b) = work.pop() {
        for &p in &schedule.blocks[b].predecessors {
            if !body.contains(&p) {
                if body.len() >= MAX_SCAN_BLOCKS {
                    return Err("scan: too many blocks in the loop");
                }
                body.push(p);
                work.push(p);
            }
        }
    }
    let in_body = |b: usize| body.contains(&b);
    let in_loop = |id: NodeId| {
        schedule
            .node_to_block
            .get(id as usize)
            .is_some_and(|&b| in_body(b))
    };
    let invariant = |id: NodeId| {
        let Some(n) = node(id) else {
            return false;
        };
        if matches!(n.op, Op::Const(_) | Op::ConstF(_)) {
            return true;
        }
        schedule.node_to_block.get(id as usize).is_some_and(|&b| {
            b < schedule.blocks.len() && !in_body(b) && schedule.dom.dominates(b, h)
        })
    };
    // The header's one edge into the loop, and which projection it is.
    let mut stays = hb.successors.iter().copied().filter(|&s| in_body(s));
    let first = stays
        .next()
        .ok_or("scan: the header has no edge into the loop")?;
    if stays.next().is_some() || hb.successors.len() != 2 {
        return Err("scan: the header's both edges stay in the loop");
    }
    if first == latch {
        return Err("body is not one straight-line latch block");
    }
    let stay_true = edge_is_true(graph, schedule, h, first)
        .ok_or("scan: body is not a projection of the test")?;
    // Header phis: the induction alone (and memory phis); a phi anywhere
    // else in the loop may merge memory only.
    let mut iv = NO_NODE;
    let mut mem_phis: Vec<NodeId> = Vec::new();
    for (id, n) in graph.nodes.iter().enumerate() {
        // Cast: an index into the node arena is a `NodeId`.
        let id = id as NodeId;
        if n.op != Op::Phi {
            continue;
        }
        if n.inputs.first() == Some(&hb.ctrl) {
            if n.inputs.len() != 3 {
                return Err("a header phi with other than two inputs");
            }
            match n.ty {
                IrType::Memory => mem_phis.push(id),
                _ if iv == NO_NODE => iv = id,
                _ => return Err("scan: the loop carries a value besides its induction"),
            }
        } else if in_loop(id) {
            if n.ty != IrType::Memory {
                return Err("scan: a value phi inside the loop");
            }
            mem_phis.push(id);
        }
    }
    if iv == NO_NODE || graph.nodes[iv as usize].ty != IrType::Int {
        return Err("no int induction phi");
    }
    let iv_next = graph.nodes[iv as usize].inputs[back_slot];
    let steps = in_loop(iv_next)
        && node(iv_next).is_some_and(|m| {
            m.op == Op::Add
                && m.ty == IrType::Int
                && m.inputs.len() == 2
                && ((m.inputs[0] == iv && is_const(graph, m.inputs[1], 1))
                    || (m.inputs[1] == iv && is_const(graph, m.inputs[0], 1)))
        });
    if !steps {
        return Err("the induction does not step by one in the loop");
    }
    if users[iv_next as usize].iter().any(|&u| u != iv && !frame_only_wide(graph, users, u)) {
        return Err("the stepped induction is read besides the back edge");
    }
    // The header test: stay while `iv < bound` (as `plan_one`).
    let cond = node(term)
        .and_then(|n| n.inputs.get(1).copied())
        .ok_or("test has no condition")?;
    let cn = node(cond).ok_or("no condition node")?;
    let Op::Cmp(op) = cn.op else {
        return Err("test is not an int compare");
    };
    if cn.inputs.len() != 2 {
        return Err("compare arity");
    }
    let stay = if stay_true { op } else { op.negate() };
    let bound = match stay {
        CmpOp::Lt if cn.inputs[0] == iv => cn.inputs[1],
        CmpOp::Gt if cn.inputs[1] == iv => cn.inputs[0],
        CmpOp::Ne if opts.ne_exit && cn.inputs[0] == iv => cn.inputs[1],
        CmpOp::Ne if opts.ne_exit && cn.inputs[1] == iv => cn.inputs[0],
        _ => return Err("test is not `iv < bound`"),
    };
    let bound_length_of = match node(bound) {
        Some(n) if n.op == Op::ArrayLength && in_loop(bound) => {
            let arr = match n.inputs.as_slice() {
                [a] => *a,
                [_, _, a, ..] => *a,
                _ => return Err("array length arity"),
            };
            if !invariant(arr) || users[bound as usize].iter().any(|&u| u != cond && !frame_only_wide(graph, users, u)) {
                return Err("bound is the length of a variant array");
            }
            Some(arr)
        }
        _ => None,
    };
    if bound_length_of.is_none()
        && (!invariant(bound) || node(bound).map(|n| n.ty) != Some(IrType::Int))
    {
        return Err("bound is not a loop-invariant int");
    }
    // The tests, from the header's edge on.
    let mut chain = ChainCx {
        graph,
        schedule,
        body: &body,
        latch,
        leaves: Vec::new(),
        tests_seen: Vec::new(),
    };
    let hit = match walk_chain(&mut chain, first, 0)? {
        ChainVal::Hit(f) => f,
        _ => return Err("scan: the body's test never leaves the loop"),
    };
    let ChainCx {
        leaves, tests_seen, ..
    } = chain;
    if !opts.scan_wide && (leaves.len() != 1 || body.len() != 3) {
        return Err("scan: body is not a test block then a latch");
    }
    // Control nodes of the loop: each block's own, its test, and the test's
    // projections. Nothing else of the loop is control-flow.
    let mut ctrls: Vec<NodeId> = Vec::new();
    for &b in &body {
        ctrls.push(schedule.blocks[b].ctrl);
        if let Some(t) = schedule.blocks[b].terminator {
            ctrls.push(t);
        }
    }
    let is_loop_ctrl = |id: NodeId| {
        ctrls.contains(&id)
            || node(id).is_some_and(|n| {
                matches!(n.op, Op::Proj(_))
                    && n.inputs
                        .first()
                        .is_some_and(|i| tests_seen.contains(i) || *i == term)
            })
    };
    // Every in-loop node; the loads give the element type and the bases.
    let mut mem_kind: Option<MemKind> = None;
    let mut bases: Vec<NodeId> = Vec::new();
    let mut operand_nodes: Vec<NodeId> = Vec::new();
    for id in 0..graph.nodes.len() {
        // Cast: an index into the node arena is a `NodeId`.
        let id = id as NodeId;
        if !in_loop(id) {
            continue;
        }
        let n = &graph.nodes[id as usize];
        if n.op.is_control() {
            if !is_loop_ctrl(id) {
                return Err("scan: control flow inside the body");
            }
            continue;
        }
        match &n.op {
            Op::Phi => {
                if id != iv && !mem_phis.contains(&id) {
                    return Err("a foreign phi in the loop");
                }
            }
            Op::Cmp(_) if id == cond || leaves.iter().any(|l| l.cond == id) => {}
            Op::LCmp | Op::FCmp { .. } if leaves.iter().any(|l| l.three_way == id) => {}
            Op::ArrayLength if id == bound && bound_length_of.is_some() => {}
            // A narrowed `long` induction's frames still name `i2l(length)` (JIT r27 LOOP27-2).
            Op::I2L | Op::LCmp | Op::Add | Op::Sub if frame_only_wide(graph, users, id) => {}
            Op::Add if id == iv_next => {}
            Op::Const(_) | Op::ConstF(_) => {}
            Op::ArrayLoad(k) => {
                if n.inputs.len() != 4 || n.inputs[3] != iv || !invariant(n.inputs[2]) {
                    return Err("an element read not at [invariant][iv]");
                }
                match mem_kind {
                    None => mem_kind = Some(*k),
                    Some(prev) if prev == *k => {}
                    Some(_) => return Err("mixed element types"),
                }
                // Every array the loop reads is guarded, even one the test
                // does not read: its element load must not fault either.
                if !bases.contains(&n.inputs[2]) {
                    if bases.len() == MAX_SCAN_BASES {
                        return Err("more distinct arrays than scratch registers");
                    }
                    bases.push(n.inputs[2]);
                }
            }
            // L8-4: an element tree's arithmetic, judged in `build_expr`.
            Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Shl
            | Op::Shr
            | Op::UShr
                if opts.scan_wide =>
            {
                operand_nodes.push(id)
            }
            other => {
                if false {
                    eprintln!(
                        "[ir-vec] {}   n{id} {other:?} has no scan form",
                        graph.dbg_label
                    );
                }
                return Err("scan: an op with no vector form in the loop");
            }
        }
    }
    let mem_kind = mem_kind.ok_or("no array access")?;
    let native = match mem_kind {
        MemKind::Int => VecKind::Int,
        MemKind::Long => VecKind::Long,
        MemKind::Byte => VecKind::Byte,
        MemKind::Short | MemKind::Char => VecKind::Short,
        MemKind::Double if opts.scan_wide => VecKind::Double,
        MemKind::Float if opts.scan_wide => VecKind::Float,
        _ => return Err("scan: element type has no lane compare"),
    };
    // Every compare must be of the lane type: an FP test over FP elements, an
    // integer one over integer elements.
    let fp = matches!(native, VecKind::Double | VecKind::Float);
    if leaves.iter().any(|l| l.fp.is_some() != fp) {
        return Err("scan: a compare of another type than the elements");
    }
    // Native lanes when every operand is an element or an invariant; a
    // sub-word test with a tree widens its loads to `int` lanes (L8-4).
    let mut invariants: Vec<NodeId> = Vec::new();
    let leaf_operand = |id: NodeId, invariants: &mut Vec<NodeId>| -> Result<VExpr, &'static str> {
        let n = node(id).ok_or("dangling operand")?;
        if n.ty != native.ir_type() {
            return Err("scan: a compare operand of another type");
        }
        if in_loop(id) && matches!(n.op, Op::ArrayLoad(_)) {
            let b = bases
                .iter()
                .position(|&x| x == n.inputs[2])
                .ok_or("scan: an unguarded element")?;
            return Ok(VExpr::Load(b));
        }
        if let (true, Some(c)) = (native.is_subword(), const_of(graph, id)) {
            // A constant the lane cannot hold is refused here (the guard
            // below is for a runtime value).
            // Cast: truncate and re-extend; equal iff the lane holds `c`.
            let fits = match mem_kind {
                MemKind::Byte => i64::from(c as i8) == c,
                MemKind::Short => i64::from(c as i16) == c,
                _ => i64::from(c as u16) == c,
            };
            if !fits {
                return Err("scan: a constant the lane cannot hold");
            }
        }
        if (!in_loop(id) || matches!(n.op, Op::Const(_) | Op::ConstF(_))) && invariant(id) {
            return Ok(VExpr::Inv(inv_index(invariants, id)));
        }
        Err("scan: a compare operand that is neither an element nor invariant")
    };
    let mut leaf_ops: Vec<(VExpr, VExpr)> = Vec::new();
    let mut leaf_err: Option<&'static str> = None;
    for l in &leaves {
        match (
            leaf_operand(l.lhs, &mut invariants),
            leaf_operand(l.rhs, &mut invariants),
        ) {
            (Ok(a), Ok(b)) => leaf_ops.push((a, b)),
            (Err(e), _) | (_, Err(e)) => {
                leaf_err = Some(e);
                break;
            }
        }
    }
    let (kind, widen, operands) = match leaf_err {
        None if operand_nodes.is_empty() => (native, None, leaf_ops),
        Some(e) if !opts.scan_wide => return Err(e),
        _ => {
            let widen = match mem_kind {
                MemKind::Byte => Some(Widen::Byte),
                MemKind::Short => Some(Widen::Short),
                MemKind::Char => Some(Widen::Char),
                _ => None,
            };
            let kind = if widen.is_some() {
                VecKind::Int
            } else {
                native
            };
            invariants.clear();
            let ctx = TreeCtx {
                graph,
                kind,
                opts,
                in_loop: &in_loop,
                invariant: &invariant,
                // The scan widens by its own plan-wide rule (`Widen`).
                check_elems: false,
                iv,
                iota: false,
                scoped_get: None,
            };
            let mut tree = TreeOut {
                bases: bases.clone(),
                base_kinds: vec![mem_kind; bases.len()],
                invariants: std::mem::take(&mut invariants),
                ..TreeOut::default()
            };
            let mut ops = Vec::new();
            for l in &leaves {
                let a = build_expr(&ctx, l.lhs, &mut tree, 0)?;
                let b = build_expr(&ctx, l.rhs, &mut tree, 0)?;
                ops.push((a, b));
            }
            invariants = std::mem::take(&mut tree.invariants);
            if tree.bases != bases {
                return Err("scan: an unguarded element");
            }
            if !tree.shift_counts.is_empty() {
                return Err("scan: a shift by a variable count");
            }
            if operand_nodes.iter().any(|m| !tree.seen.contains(m)) {
                return Err("scan: a body value no test reads");
            }
            (kind, widen, ops)
        }
    };
    // Each compare, in the sense the formula reads it.
    let mut tests: Vec<ScanTest> = Vec::new();
    for (l, (lx, rx)) in leaves.iter().zip(operands) {
        if !expr_reads_element(&lx) && !expr_reads_element(&rx) {
            return Err("scan: the test reads no element");
        }
        let op = if l.negated { l.op.negate() } else { l.op };
        tests.push(match l.fp {
            Some(nan_greater) => ScanTest {
                cmp: ScanCmp::Fp(fp_predicate(op, nan_greater)),
                lhs: lx,
                rhs: rx,
                miss: false,
            },
            None => {
                let (cmp, lhs, rhs, miss) = match op {
                    CmpOp::Eq => (ScanCmp::Eq, lx, rx, false),
                    CmpOp::Ne => (ScanCmp::Eq, lx, rx, true),
                    CmpOp::Gt => (ScanCmp::Gt, lx, rx, false),
                    CmpOp::Lt => (ScanCmp::Gt, rx, lx, false),
                    CmpOp::Le => (ScanCmp::Gt, lx, rx, true),
                    CmpOp::Ge => (ScanCmp::Gt, rx, lx, true),
                };
                // `VPCMPGTW` is signed: a `char` order compare in 16-bit lanes
                // is the unsigned form (L8-4); widened lanes hold the
                // zero-extended value, which the signed `int` order answers.
                let cmp = if cmp == ScanCmp::Gt && mem_kind == MemKind::Char && widen.is_none() {
                    if !opts.scan_wide {
                        return Err("scan: an unsigned char order compare");
                    }
                    ScanCmp::GtU16
                } else {
                    cmp
                };
                ScanTest {
                    cmp,
                    lhs,
                    rhs,
                    miss,
                }
            }
        });
    }
    let narrow = match (mem_kind, widen) {
        (MemKind::Byte, None) => Some((8u8, true)),
        (MemKind::Short, None) => Some((16, true)),
        (MemKind::Char, None) => Some((16, false)),
        _ => None,
    }
    .filter(|_| {
        invariants
            .iter()
            .any(|&v| !matches!(node(v).map(|n| &n.op), Some(Op::Const(_))))
    });
    Ok(VecLoopPlan {
        header: h,
        entry_pred,
        header_bci: node(hb.ctrl).and_then(|n| n.bytecode_pc),
        loop_blocks: body.clone(),
        iv,
        bound,
        bound_length_of,
        kind,
        base_kinds: vec![mem_kind; bases.len()],
        bases,
        invariants,
        shift_counts: Vec::new(),
        store_base: 0,
        value: VExpr::Load(0),
        reduce: None,
        scan: Some(VecScan { tests, hit, narrow }),
        iota: false,
        scoped: None,
        widen,
    })
}

/// At most this many blocks in a scan's loop (header and latch included).
const MAX_SCAN_BLOCKS: usize = 10;

/// At most this many compares in one scan's test (L8-4).
const MAX_SCAN_TESTS: usize = 4;

/// Does `e` read an array element?
fn expr_reads_element(e: &VExpr) -> bool {
    match e {
        VExpr::Load(_) => true,
        VExpr::Inv(_) | VExpr::Iota => false,
        VExpr::Bin(_, l, r) | VExpr::MinMax { l, r, .. } => {
            expr_reads_element(l) || expr_reads_element(r)
        }
        VExpr::Shift(_, _, e) | VExpr::ShiftBy(_, _, e) | VExpr::I2L(e) => expr_reads_element(e),
    }
}

/// The AVX compare predicate of `fcmp(l, r) OP 0` (L8-4), where `fcmp` is
/// `-1/0/1` and a NaN operand gives `+1` (`nan_greater`, the `g` forms) or
/// `-1`: an ordered (`_OQ`) predicate where a NaN must answer false, an
/// unordered (`_UQ`) negation where it must answer true. Negating a test is
/// `imm ^ 4` for every one of them (the predicate table's own symmetry).
fn fp_predicate(op: CmpOp, nan_greater: bool) -> u8 {
    match (op, nan_greater) {
        (CmpOp::Eq, _) => 0x00,     // EQ_OQ
        (CmpOp::Ne, _) => 0x04,     // NEQ_UQ
        (CmpOp::Gt, false) => 0x1E, // GT_OQ
        (CmpOp::Gt, true) => 0x06,  // NLE_UQ
        (CmpOp::Ge, false) => 0x1D, // GE_OQ
        (CmpOp::Ge, true) => 0x05,  // NLT_UQ
        (CmpOp::Lt, false) => 0x09, // NGE_UQ
        (CmpOp::Lt, true) => 0x11,  // LT_OQ
        (CmpOp::Le, false) => 0x0A, // NGT_UQ
        (CmpOp::Le, true) => 0x12,  // LE_OQ
    }
}

/// One compare of a scan's chain: `op(lhs, rhs)` as the graph states it
/// (`three_way` the `LCmp`/`FCmp` it reads through, or `NO_NODE`), and the
/// sense the formula takes it in.
struct ChainLeaf {
    cond: NodeId,
    three_way: NodeId,
    op: CmpOp,
    lhs: NodeId,
    rhs: NodeId,
    /// `Some(nan_greater)` for an FP compare.
    fp: Option<bool>,
    negated: bool,
}

struct ChainCx<'a> {
    graph: &'a Graph,
    schedule: &'a Schedule,
    body: &'a [usize],
    latch: usize,
    leaves: Vec<ChainLeaf>,
    /// The test (`If`) nodes walked, whose projections are the loop's.
    tests_seen: Vec<NodeId>,
}

/// What a block of the chain answers: leave (`True`), stay (`False`), or a
/// formula of the compares below it.
enum ChainVal {
    True,
    False,
    Hit(ScanHit),
}

/// Is `s` reached from block `b` by its test's TRUE projection? `None` when
/// `s` is not one of `b`'s two-way test's projections (or is both).
fn edge_is_true(graph: &Graph, schedule: &Schedule, b: usize, s: usize) -> Option<bool> {
    let test = schedule.blocks[b].terminator?;
    let sc = schedule.blocks.get(s)?.ctrl;
    let is_proj = |c: NodeId, k: u16| {
        graph
            .nodes
            .get(c as usize)
            .is_some_and(|n| n.op == Op::Proj(k) && n.inputs.first() == Some(&test))
    };
    let sn = graph.nodes.get(sc as usize)?;
    let reached = |k: u16| {
        is_proj(sc, k)
            || (matches!(sn.op, Op::Region | Op::Merge) && sn.inputs.iter().any(|&i| is_proj(i, k)))
    };
    match (reached(0), reached(1)) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    }
}

/// The hit formula of chain block `b` (`b ? T : F`, the latch staying and any
/// block out of the loop leaving).
fn walk_chain(cx: &mut ChainCx<'_>, b: usize, depth: usize) -> Result<ChainVal, &'static str> {
    if b == cx.latch {
        return Ok(ChainVal::False);
    }
    if !cx.body.contains(&b) {
        return Ok(ChainVal::True);
    }
    if depth > MAX_SCAN_BLOCKS {
        return Err("scan: the test chain loops");
    }
    let blk = &cx.schedule.blocks[b];
    let Some(test) = blk.terminator else {
        // A plain goto inside the chain.
        return match blk.successors.as_slice() {
            [s] => walk_chain(cx, *s, depth + 1),
            _ => Err("scan: the body block has no test"),
        };
    };
    let graph = cx.graph;
    let node = |id: NodeId| graph.nodes.get(id as usize);
    if node(test).map(|n| &n.op) != Some(&Op::If) || blk.successors.len() != 2 {
        return Err("scan: the body block does not end in a two-way test");
    }
    let (s0, s1) = (blk.successors[0], blk.successors[1]);
    let (on_true, on_false) = match edge_is_true(graph, cx.schedule, b, s0) {
        Some(true) => (s0, s1),
        Some(false) => (s1, s0),
        None => return Err("scan: body is not a projection of the test"),
    };
    if edge_is_true(graph, cx.schedule, b, on_false) != Some(false) {
        return Err("scan: body is not a projection of the test");
    }
    cx.tests_seen.push(test);
    // The compare: `Cmp(op)[l, r]`, or `Cmp(op)[LCmp/FCmp(l, r), 0]` (either
    // side) for `long` / FP elements.
    let hcond = node(test)
        .and_then(|n| n.inputs.get(1).copied())
        .ok_or("scan: the body test has no condition")?;
    let hn = node(hcond).ok_or("scan: no body condition node")?;
    let Op::Cmp(mut hop) = hn.op else {
        return Err("scan: the body test is not a compare");
    };
    if hn.inputs.len() != 2 {
        return Err("scan: body compare arity");
    }
    let (mut l, mut r) = (hn.inputs[0], hn.inputs[1]);
    let three = |id: NodeId| {
        node(id).and_then(|n| match n.op {
            Op::LCmp if n.inputs.len() == 2 => Some(None),
            Op::FCmp { nan_greater, .. } if n.inputs.len() == 2 => Some(Some(nan_greater)),
            _ => None,
        })
    };
    let mut three_way = NO_NODE;
    let mut fp = None;
    if let (Some(f), true) = (three(l), is_const(graph, r, 0)) {
        three_way = l;
        fp = f;
    } else if let (Some(f), true) = (three(r), is_const(graph, l, 0)) {
        three_way = r;
        fp = f;
        hop = hop.swapped();
    }
    if three_way != NO_NODE {
        let n = &graph.nodes[three_way as usize];
        (l, r) = (n.inputs[0], n.inputs[1]);
    }
    let t = walk_chain(cx, on_true, depth + 1)?;
    let f = walk_chain(cx, on_false, depth + 1)?;
    let leaf = |cx: &mut ChainCx<'_>, negated: bool| -> Result<ScanHit, &'static str> {
        if cx.leaves.len() == MAX_SCAN_TESTS {
            return Err("scan: more compares than the step combines");
        }
        cx.leaves.push(ChainLeaf {
            cond: hcond,
            three_way,
            op: hop,
            lhs: l,
            rhs: r,
            fp,
            negated,
        });
        Ok(ScanHit::Test(cx.leaves.len() - 1))
    };
    Ok(ChainVal::Hit(match (t, f) {
        (ChainVal::True, ChainVal::False) => leaf(cx, false)?,
        (ChainVal::False, ChainVal::True) => leaf(cx, true)?,
        (ChainVal::True, ChainVal::Hit(g)) => ScanHit::Or(Box::new(leaf(cx, false)?), Box::new(g)),
        (ChainVal::Hit(g), ChainVal::False) => {
            ScanHit::And(Box::new(leaf(cx, false)?), Box::new(g))
        }
        (ChainVal::False, ChainVal::Hit(g)) => ScanHit::And(Box::new(leaf(cx, true)?), Box::new(g)),
        (ChainVal::Hit(g), ChainVal::True) => ScanHit::Or(Box::new(leaf(cx, true)?), Box::new(g)),
        _ => return Err("scan: a test whose two edges answer alike or both branch on"),
    }))
}

fn is_const(graph: &Graph, id: NodeId, v: i64) -> bool {
    graph.nodes.get(id as usize).map(|n| &n.op) == Some(&Op::Const(v))
}

/// Bytes per element of `k`.
pub(crate) fn mem_bytes(k: MemKind) -> u8 {
    match k {
        MemKind::Long | MemKind::Double | MemKind::Ref => 8,
        MemKind::Int | MemKind::Float => 4,
        MemKind::Short | MemKind::Char => 2,
        MemKind::Byte => 1,
    }
}

/// May a load of `load` feed lanes of `kind`? Its own lane type, or (VEC21-2,
/// under [`VecOpts::widen`]) a narrower one the widening load extends exactly
/// as the scalar `xaload` (and, for `long` / `double` lanes, the `i2l` / `f2d`
/// the tree must apply to it) does: sign for `byte`/`short`/`int`, zero for
/// `char`, `VCVTPS2PD` for `float`.
fn lane_readable(load: MemKind, kind: VecKind, opts: VecOpts) -> bool {
    if VecKind::of(load, opts) == Some(kind) {
        return true;
    }
    opts.widen
        && matches!(
            (kind, load),
            (VecKind::Int, MemKind::Byte | MemKind::Short | MemKind::Char)
                | (VecKind::Short, MemKind::Byte)
                | (
                    VecKind::Long,
                    MemKind::Int | MemKind::Short | MemKind::Char | MemKind::Byte
                )
                | (VecKind::Double, MemKind::Float)
        )
}

/// What [`build_expr`] collects.
#[derive(Default)]
struct TreeOut {
    bases: Vec<NodeId>,
    /// Parallel to `bases`: the element type each is accessed as.
    base_kinds: Vec<MemKind>,
    invariants: Vec<NodeId>,
    shift_counts: Vec<NodeId>,
    seen: Vec<NodeId>,
    /// JIT round 24 B1: the tree reads [`VExpr::Iota`].
    iota: bool,
}

impl TreeOut {
    fn base_index(&mut self, b: NodeId, k: MemKind) -> Result<usize, &'static str> {
        if let Some(i) = self.bases.iter().position(|&x| x == b) {
            if self.base_kinds[i] != k {
                return Err("one array accessed as two element types");
            }
            return Ok(i);
        }
        if self.bases.len() == MAX_BASES {
            return Err("more distinct arrays than scratch registers");
        }
        self.bases.push(b);
        self.base_kinds.push(k);
        Ok(self.bases.len() - 1)
    }

    fn see(&mut self, id: NodeId) {
        // A shared subtree (`t = b[j]; c[j] = t * t`) is rebuilt per use,
        // which recomputes the same lane values.
        if !self.seen.contains(&id) {
            self.seen.push(id);
        }
    }
}

/// What [`build_expr`] reads but never changes.
struct TreeCtx<'a> {
    graph: &'a Graph,
    kind: VecKind,
    opts: VecOpts,
    in_loop: &'a dyn Fn(NodeId) -> bool,
    invariant: &'a dyn Fn(NodeId) -> bool,
    /// Judge each element's type against the lanes (`lane_readable`); a scan,
    /// which widens by its own plan-wide rule, does not.
    check_elems: bool,
    /// JIT round 24 B1: the induction phi, and whether the tree may read it
    /// ([`VExpr::Iota`]).
    iv: NodeId,
    iota: bool,
    /// j27 VEC27-1: the one scoped get the planner admitted (`(get, element
    /// type)`), read as an element of its base (input 4).
    scoped_get: Option<(NodeId, MemKind)>,
}

fn inv_index(invariants: &mut Vec<NodeId>, id: NodeId) -> usize {
    match invariants.iter().position(|&x| x == id) {
        Some(i) => i,
        None => {
            invariants.push(id);
            invariants.len() - 1
        }
    }
}

fn build_expr(
    cx: &TreeCtx<'_>,
    id: NodeId,
    out: &mut TreeOut,
    depth: usize,
) -> Result<VExpr, &'static str> {
    if depth > 16 {
        return Err("element tree too deep");
    }
    let kind = cx.kind;
    let n = cx.graph.nodes.get(id as usize).ok_or("dangling operand")?;
    if n.ty != kind.ir_type() {
        return Err("an operand of another type");
    }
    // JIT round 24 B1: the induction as a lane value, `j + i` -- the phi in
    // `int` lanes, its `i2l` in `long` lanes (`j + i < n` fits an `int`, so
    // the widened sum is the sum widened).
    if cx.iota {
        let is_iv = match kind {
            VecKind::Int => id == cx.iv,
            VecKind::Long => n.op == Op::I2L && n.inputs.first() == Some(&cx.iv),
            _ => false,
        };
        if is_iv {
            if id != cx.iv {
                out.see(id);
            }
            out.iota = true;
            return Ok(VExpr::Iota);
        }
    }
    if !(cx.in_loop)(id) {
        if !(cx.invariant)(id) {
            return Err("an operand neither in the loop nor invariant");
        }
        return Ok(VExpr::Inv(inv_index(&mut out.invariants, id)));
    }
    if matches!(n.op, Op::Const(_) | Op::ConstF(_)) {
        return Ok(VExpr::Inv(inv_index(&mut out.invariants, id)));
    }
    out.see(id);
    match &n.op {
        Op::ArrayLoad(k) => {
            // A `long` / `double` lane reads a narrower element only through
            // its `i2l` / `f2d` (the arm below); an `int` or 16-bit lane reads
            // a narrower one directly (its graph type is already `int`).
            if cx.check_elems
                && (!lane_readable(*k, kind, cx.opts)
                    || matches!(kind, VecKind::Long | VecKind::Double)
                        && mem_bytes(*k) != kind.elem_bytes())
            {
                return Err("an element of another type");
            }
            Ok(VExpr::Load(out.base_index(n.inputs[2], *k)?))
        }
        // j27 VEC27-1: the admitted scoped get, `[.., base(4), ..]`: an
        // element of its base at `start + j * width` (the pre-loop rebases).
        Op::Unbox {
            op: UnboxOp::ScopedGet { .. },
            ..
        } if cx.scoped_get.is_some_and(|(g, _)| g == id) => {
            let k = cx.scoped_get.map_or(MemKind::Int, |(_, k)| k);
            let base = *n.inputs.get(4).ok_or("a scoped get of another arity")?;
            Ok(VExpr::Load(out.base_index(base, k)?))
        }
        // VEC21-2: `(long) a[j]` / `(double) f[j]`, folded into a widening
        // load. Only directly over an element of the loop.
        Op::I2L | Op::F2D => {
            let src = *n.inputs.first().ok_or("a conversion with no operand")?;
            let sn = cx.graph.nodes.get(src as usize).ok_or("dangling operand")?;
            // j27 VEC27-1: `(long) s.getAtIndex(INT, k)`, the admitted scoped
            // get widened exactly as an `int[]` element is.
            if let (Op::I2L, Some((g, mk))) = (&n.op, cx.scoped_get) {
                if src == g {
                    if kind != VecKind::Long || mk == MemKind::Long || !lane_readable(mk, kind, cx.opts)
                    {
                        return Err("a conversion with no widening load");
                    }
                    out.see(src);
                    let base = *sn.inputs.get(4).ok_or("a scoped get of another arity")?;
                    return Ok(VExpr::Load(out.base_index(base, mk)?));
                }
            }
            if let Op::ArrayLoad(k) = sn.op {
                let ok = (cx.in_loop)(src)
                    && lane_readable(k, kind, cx.opts)
                    && match n.op {
                        Op::I2L => kind == VecKind::Long && k != MemKind::Long,
                        _ => kind == VecKind::Double && k == MemKind::Float,
                    };
                if !ok {
                    return Err("a conversion with no widening load");
                }
                out.see(src);
                return Ok(VExpr::Load(out.base_index(sn.inputs[2], k)?));
            }
            if n.op == Op::I2L
                && kind == VecKind::Long
                && cx.opts.widen
                && sn.ty == IrType::Int
                && (cx.in_loop)(src)
            {
                let inner_cx = TreeCtx {
                    kind: VecKind::Int,
                    opts: VecOpts {
                        varshift: false,
                        ..cx.opts
                    },
                    scoped_get: None,
                    ..*cx
                };
                let inner = build_expr(&inner_cx, src, out, depth + 1)?;
                return Ok(VExpr::I2L(Box::new(inner)));
            }
            Err("a widening conversion of a non-element")
        }
        Op::Add | Op::Sub | Op::Mul | Op::Div | Op::And | Op::Or | Op::Xor => {
            let op = match n.op {
                Op::Add => VOp::Add,
                Op::Sub => VOp::Sub,
                Op::Mul => VOp::Mul,
                Op::Div => VOp::Div,
                Op::And => VOp::And,
                Op::Or => VOp::Or,
                _ => VOp::Xor,
            };
            if n.inputs.len() != 2 {
                return Err("arithmetic with a guard input");
            }
            // A sub-word `x & C` whose C keeps every lane bit (`i2c`'s
            // `x & 0xFFFF`, a `& 0xFF` on a byte lane) is `x` in the lane: no
            // operation, and no invariant register for C.
            if kind.is_subword() && op == VOp::And {
                let lane_mask: i64 = (1i64 << (8 * i64::from(kind.elem_bytes()))) - 1;
                for (keep, k) in [(n.inputs[0], n.inputs[1]), (n.inputs[1], n.inputs[0])] {
                    if const_of(cx.graph, k).is_some_and(|c| c & lane_mask == lane_mask) {
                        return build_expr(cx, keep, out, depth + 1);
                    }
                }
            }
            let bitwise = matches!(op, VOp::And | VOp::Or | VOp::Xor);
            if bitwise
                && !(kind.is_subword()
                    || (cx.opts.bitops && matches!(kind, VecKind::Int | VecKind::Long)))
            {
                return Err("a bitwise op with no vector form here");
            }
            match (kind, op) {
                (VecKind::Long, VOp::Mul) if cx.opts.long_mul => {}
                (VecKind::Int, VOp::Div) | (VecKind::Long, VOp::Div | VOp::Mul) => {
                    return Err("integer op with no AVX2 form")
                }
                (VecKind::Byte, VOp::Mul | VOp::Div) | (VecKind::Short, VOp::Div) => {
                    return Err("sub-word op with no AVX2 form")
                }
                _ => {}
            }
            let l = build_expr(cx, n.inputs[0], out, depth + 1)?;
            let r = build_expr(cx, n.inputs[1], out, depth + 1)?;
            // NaN payload parity (r22 w6c): with two NaN lanes the VEX form
            // returns src1's, which `gen` fills from the left operand.
            // Under the opt-in `CRATONVM_JIT_FP_NAN_OPERAND_ORDER=1` (HotSpot's
            // interpreter rule) value2 goes there for add/mul, value1 for
            // sub/div; off (default, the C2 rule) value1 always.
            if fp_nan_order(kind) && matches!(op, VOp::Add | VOp::Mul) {
                return Ok(VExpr::Bin(op, Box::new(r), Box::new(l)));
            }
            Ok(VExpr::Bin(op, Box::new(l), Box::new(r)))
        }
        Op::Shl | Op::Shr | Op::UShr => {
            if n.inputs.len() != 2 {
                return Err("a shift with a guard input");
            }
            let count = const_of(cx.graph, n.inputs[1]);
            if kind.is_subword() {
                let count = count.ok_or("a shift by a variable count")?;
                // Only the `i2b` / `i2s` narrowing pair `(x << c) >> c` (or
                // `>>> c`) with `c` small enough to keep every lane bit: its
                // low `32 - c` bits are `x`'s.
                let lane_bits = 8 * i64::from(kind.elem_bytes());
                let inner_id = n.inputs[0];
                let inner = cx.graph.nodes.get(inner_id as usize).ok_or("dangling operand")?;
                let pair = matches!(n.op, Op::Shr | Op::UShr)
                    && inner.op == Op::Shl
                    && inner.ty == IrType::Int
                    && inner.inputs.len() == 2
                    && (cx.in_loop)(inner_id)
                    && const_of(cx.graph, inner.inputs[1]) == Some(count)
                    && (0..=32 - lane_bits).contains(&count);
                if !pair {
                    return Err("a sub-word shift that reads bits a lane does not hold");
                }
                out.see(inner_id);
                return build_expr(cx, inner.inputs[0], out, depth + 1);
            }
            if !cx.opts.bitops {
                return Err("a shift with no vector form here");
            }
            if count.is_none() && !cx.opts.varshift {
                return Err("a shift by a variable count");
            }
            let (sh, mask): (VShift, i64) = match (&n.op, kind) {
                (&Op::Shl, VecKind::Int) => (VShift::Shl, 31),
                (&Op::Shr, VecKind::Int) => (VShift::Sar, 31),
                (&Op::UShr, VecKind::Int) => (VShift::Shr, 31),
                (&Op::Shl, VecKind::Long) => (VShift::Shl, 63),
                (&Op::UShr, VecKind::Long) => (VShift::Shr, 63),
                (&Op::Shr, VecKind::Long) => {
                    return Err("a long arithmetic shift has no AVX2 form")
                }
                _ => return Err("a shift of a non-integer element"),
            };
            let e = build_expr(cx, n.inputs[0], out, depth + 1)?;
            match count {
                // Java masks the count; x86 would give zero for a count >=
                // width. Cast: masked to 0..=63, fits a byte.
                Some(c) => Ok(VExpr::Shift(sh, (c & mask) as u8, Box::new(e))),
                // VEC21-6: a loop-invariant `int` count, masked the same way
                // at run time, before the loop (`count_load_code`).
                None => {
                    let c = n.inputs[1];
                    let cn = cx.graph.nodes.get(c as usize).ok_or("dangling operand")?;
                    if (cx.in_loop)(c) || !(cx.invariant)(c) || cn.ty != IrType::Int {
                        return Err("a shift count that is not loop-invariant");
                    }
                    let i = inv_index(&mut out.shift_counts, c);
                    Ok(VExpr::ShiftBy(sh, i, Box::new(e)))
                }
            }
        }
        // L8-3: `Math.min/max` of two element-tree values of the lane type.
        // A sub-word lane is refused: its operands are `int`s the lane need
        // not hold (`Math.max(a[j] + 1, ..)`).
        Op::ScalarIntrinsic(sop) if cx.opts.elem_minmax => {
            let (max, ty) = minmax_of(*sop).ok_or("an intrinsic with no vector form")?;
            if ty != kind.ir_type() || kind.is_subword() || n.inputs.len() != 2 {
                return Err("a min/max with no lane form here");
            }
            let l = build_expr(cx, n.inputs[0], out, depth + 1)?;
            let r = build_expr(cx, n.inputs[1], out, depth + 1)?;
            Ok(VExpr::MinMax {
                max,
                l: Box::new(l),
                r: Box::new(r),
            })
        }
        _ => Err("an element-tree op with no vector form"),
    }
}

// ── Encoding ─────────────────────────────────────────────────────────────

/// The GPR holding `j` in the vector loop.
pub(crate) const IV_REG: u8 = 2; // RDX
/// The GPRs the array bases are loaded into, in base order.
pub(crate) const BASE_REGS: [u8; MAX_BASES] = [0, 1, 8, 9]; // RAX, RCX, R8, R9

/// A 3-byte VEX prefix plus `opcode`. `reg` is ModRM.reg, `vvvv` the first
/// source (0 when unused: the inverted field is then `1111`, which is also
/// how XMM0 encodes -- both are what the SDM asks for), `x`/`b` the SIB index
/// and ModRM.rm/SIB base registers.
#[allow(clippy::too_many_arguments)]
fn vex3(
    out: &mut Vec<u8>,
    reg: u8,
    x: u8,
    b: u8,
    mmmmm: u8,
    w: bool,
    vvvv: u8,
    l256: bool,
    pp: u8,
    opcode: u8,
) {
    let inv = |r: u8| u8::from(r & 8 == 0);
    out.push(0xC4);
    out.push((inv(reg) << 7) | (inv(x) << 6) | (inv(b) << 5) | mmmmm);
    out.push((u8::from(w) << 7) | ((!vvvv & 0xF) << 3) | (u8::from(l256) << 2) | pp);
    out.push(opcode);
}

/// ModRM + SIB + disp8 for `[base + IV_REG * elem + disp]`.
fn mem_operand(out: &mut Vec<u8>, reg: u8, base: u8, elem: u8, disp: i8) {
    let ss = match elem {
        8 => 3,
        4 => 2,
        2 => 1,
        _ => 0,
    };
    out.push(0x40 | ((reg & 7) << 3) | 0x04);
    out.push((ss << 6) | ((IV_REG & 7) << 3) | (base & 7));
    // Cast: the signed displacement byte, reinterpreted.
    out.push(disp as u8);
}

/// A second source: a register, or the element `[base + j*elem + disp]`.
#[derive(Clone, Copy)]
enum Src {
    Reg(u8),
    Mem { base: u8, disp: i8 },
}

/// `(mmmmm, pp, opcode)` of the packed op. The planner never builds the
/// combinations this maps to a neighbour (`int /`, `long *`, `byte *`, any
/// sub-word `/`, FP bitwise); they are listed so the table is total.
fn op_code(kind: VecKind, op: VOp) -> (u8, u8, u8) {
    match kind {
        VecKind::Double | VecKind::Float => {
            let pp = if kind == VecKind::Double { 1 } else { 0 };
            let opc = match op {
                VOp::Add => 0x58,
                VOp::Mul => 0x59,
                VOp::Sub => 0x5C,
                VOp::Div => 0x5E,
                // VANDPx / VORPx / VXORPx (unreachable from the planner).
                VOp::And => 0x54,
                VOp::Or => 0x56,
                VOp::Xor => 0x57,
            };
            (1, pp, opc)
        }
        _ => match (op, kind) {
            // VPAND / VPOR / VPXOR: 66 0F DB / EB / EF, any integer lane.
            (VOp::And, _) => (1, 1, 0xDB),
            (VOp::Or, _) => (1, 1, 0xEB),
            (VOp::Xor, _) => (1, 1, 0xEF),
            (VOp::Add, VecKind::Int) => (1, 1, 0xFE),
            (VOp::Sub, VecKind::Int) => (1, 1, 0xFA),
            // VPMULLD: 66 0F38 40.
            (_, VecKind::Int) => (2, 1, 0x40),
            (VOp::Add, VecKind::Long) => (1, 1, 0xD4),
            (_, VecKind::Long) => (1, 1, 0xFB),
            // VPADDB / VPSUBB.
            (VOp::Add, VecKind::Byte) => (1, 1, 0xFC),
            (_, VecKind::Byte) => (1, 1, 0xF8),
            // VPADDW / VPSUBW / VPMULLW.
            (VOp::Add, _) => (1, 1, 0xFD),
            (VOp::Sub, _) => (1, 1, 0xF9),
            (_, _) => (1, 1, 0xD5),
        },
    }
}

/// `CRATONVM_JIT_FP_NAN_OPERAND_ORDER` for an FP lane kind: the left operand
/// of a `VExpr::Bin` lands in VEX src1 (whose NaN wins) and must stay there.
fn fp_nan_order(kind: VecKind) -> bool {
    matches!(kind, VecKind::Double | VecKind::Float) && crate::fp_nan_operand_order_enabled()
}

fn emit_binop(out: &mut Vec<u8>, kind: VecKind, op: VOp, dst: u8, src1: u8, src2: Src) {
    emit_binop_w(out, kind, op, dst, src1, src2, true);
}

fn emit_binop_w(
    out: &mut Vec<u8>,
    kind: VecKind,
    op: VOp,
    dst: u8,
    src1: u8,
    src2: Src,
    l256: bool,
) {
    let (mm, pp, opc) = op_code(kind, op);
    match src2 {
        Src::Reg(r) => {
            vex3(out, dst, 0, r, mm, false, src1, l256, pp, opc);
            out.push(0xC0 | ((dst & 7) << 3) | (r & 7));
        }
        Src::Mem { base, disp } => {
            vex3(out, dst, IV_REG, base, mm, false, src1, l256, pp, opc);
            mem_operand(out, dst, base, kind.elem_bytes(), disp);
        }
    }
}

/// `VPSLLD/VPSRAD/VPSRLD/VPSLLQ/VPSRLQ ymm_dst, ymm_src, imm8`
/// (`VEX.256.66.0F 72|73 /6|/4|/2 ib`: the destination is VEX.vvvv, the
/// source ModRM.rm, the operation ModRM.reg). `None` for a shift AVX2 lacks.
fn emit_shift(
    out: &mut Vec<u8>,
    kind: VecKind,
    s: VShift,
    dst: u8,
    src: u8,
    imm: u8,
) -> Option<()> {
    emit_shift_w(out, kind, s, dst, src, imm, true)
}

fn emit_shift_w(
    out: &mut Vec<u8>,
    kind: VecKind,
    s: VShift,
    dst: u8,
    src: u8,
    imm: u8,
    l256: bool,
) -> Option<()> {
    let (opc, ext) = match (kind, s) {
        (VecKind::Int, VShift::Shl) => (0x72, 6),
        (VecKind::Int, VShift::Sar) => (0x72, 4),
        (VecKind::Int, VShift::Shr) => (0x72, 2),
        (VecKind::Long, VShift::Shl) => (0x73, 6),
        (VecKind::Long, VShift::Shr) => (0x73, 2),
        _ => return None,
    };
    vex3(out, ext, 0, src, 1, false, dst, l256, 1, opc);
    out.push(0xC0 | (ext << 3) | (src & 7));
    out.push(imm);
    Some(())
}

/// `VPXOR ymm_r, ymm_r, ymm_r`: a reduction accumulator's zero.
pub(crate) fn emit_zero(out: &mut Vec<u8>, r: u8) {
    vex3(out, r, 0, r, 1, false, r, true, 1, 0xEF);
    out.push(0xC0 | ((r & 7) << 3) | (r & 7));
}

/// The horizontal fold of a reduction accumulator `acc` (`int`: eight dword
/// lanes, `long`: four qword lanes) into EAX / RAX, using `tmp` (any other
/// vector register). Wrapping adds throughout, so the result is the lanes'
/// sum mod 2^32 / 2^64. `None` for a kind that has no reduction.
///
/// ```text
/// VEXTRACTI128 xmm_tmp, ymm_acc, 1      ; high half
/// VPADDD/Q     xmm_acc, xmm_acc, xmm_tmp
/// VPSHUFD      xmm_tmp, xmm_acc, 0x4E   ; swap qwords
/// VPADDD/Q     xmm_acc, xmm_acc, xmm_tmp
/// VPSHUFD      xmm_tmp, xmm_acc, 0xB1   ; (int) swap dwords
/// VPADDD       xmm_acc, xmm_acc, xmm_tmp
/// VMOVD EAX, xmm_acc / VMOVQ RAX, xmm_acc
/// ```
pub(crate) fn fold_code(kind: VecKind, acc: u8, tmp: u8) -> Option<Vec<u8>> {
    let (add, wide) = match kind {
        VecKind::Int => (0xFE, false),
        VecKind::Long => (0xD4, true),
        _ => return None,
    };
    if acc == tmp {
        return None;
    }
    let mut out = Vec::new();
    // VEXTRACTI128: VEX.256.66.0F3A.W0 39 /r ib, reg = the ymm source.
    vex3(&mut out, acc, 0, tmp, 3, false, 0, true, 1, 0x39);
    out.push(0xC0 | ((acc & 7) << 3) | (tmp & 7));
    out.push(1);
    let padd = |out: &mut Vec<u8>| {
        // VPADDD/VPADDQ xmm_acc, xmm_acc, xmm_tmp (VEX.128).
        vex3(out, acc, 0, tmp, 1, false, acc, false, 1, add);
        out.push(0xC0 | ((acc & 7) << 3) | (tmp & 7));
    };
    padd(&mut out);
    let shuf: &[u8] = if wide { &[0x4E] } else { &[0x4E, 0xB1] };
    for &imm in shuf {
        // VPSHUFD xmm_tmp, xmm_acc, imm: VEX.128.66.0F 70 /r ib.
        vex3(&mut out, tmp, 0, acc, 1, false, 0, false, 1, 0x70);
        out.push(0xC0 | ((tmp & 7) << 3) | (acc & 7));
        out.push(imm);
        padd(&mut out);
    }
    // VMOVD r32, xmm / VMOVQ r64, xmm: VEX.128.66.0F.W0|W1 7E /r, reg = xmm.
    vex3(&mut out, acc, 0, 0, 1, wide, 0, false, 1, 0x7E);
    out.push(0xC0 | ((acc & 7) << 3));
    Some(out)
}

/// `VMOVUPD`/`VMOVUPS`/`VMOVDQU ymm, [base + j*elem + disp]`.
pub(crate) fn emit_load(out: &mut Vec<u8>, kind: VecKind, dst: u8, base: u8, disp: i8) {
    let (pp, opc) = match kind {
        VecKind::Double => (1, 0x10),
        VecKind::Float => (0, 0x10),
        VecKind::Int | VecKind::Long | VecKind::Byte | VecKind::Short => (2, 0x6F),
    };
    vex3(out, dst, IV_REG, base, 1, false, 0, true, pp, opc);
    mem_operand(out, dst, base, kind.elem_bytes(), disp);
}

/// Does `bases[b]` need a widening load (VEC21-2): is its element narrower
/// than the plan's lanes?
fn base_widens(plan: &VecLoopPlan, b: usize) -> bool {
    // A scan widens by its own plan-wide rule (`VecLoopPlan::widen`).
    plan.scan.is_none()
        && plan.widen.is_none()
        && plan
            .base_kinds
            .get(b)
            .is_some_and(|&k| mem_bytes(k) != plan.kind.elem_bytes())
}

/// VEC21-2: the widening load of `W = kind.lanes()` elements of type `src`
/// at the step whose lane-kind displacement is `disp`, into `ymm_dst`:
///
/// ```text
/// int   <- byte  VPMOVSXBD   ymm, m64      short <- byte  VPMOVSXBW ymm, m128
/// int   <- short VPMOVSXWD   ymm, m128     long  <- int   VPMOVSXDQ ymm, m128
/// int   <- char  VPMOVZXWD   ymm, m128     long  <- short VPMOVSXWQ ymm, m64
/// double<- float VCVTPS2PD   ymm, m128     long  <- char  VPMOVZXWQ ymm, m64
///                                          long  <- byte  VPMOVSXBQ ymm, m32
/// ```
///
/// Each is, lane by lane, the scalar `baload`/`saload`/`caload` extension
/// (plus `i2l`) or `f2d`. The element address scales `j` by the SOURCE width,
/// and step `u`'s displacement is `HEADER_SIZE + u * W * src_bytes`. `None`
/// for a pair that has no widening form.
pub(crate) fn emit_widen_load(
    out: &mut Vec<u8>,
    kind: VecKind,
    src: MemKind,
    dst: u8,
    base: u8,
    disp: i8,
) -> Option<()> {
    let (mm, pp, opc) = match (kind, src) {
        (VecKind::Int, MemKind::Byte) => (2, 1, 0x21),
        (VecKind::Int, MemKind::Short) => (2, 1, 0x23),
        (VecKind::Int, MemKind::Char) => (2, 1, 0x33),
        (VecKind::Short, MemKind::Byte) => (2, 1, 0x20),
        (VecKind::Long, MemKind::Int) => (2, 1, 0x25),
        (VecKind::Long, MemKind::Short) => (2, 1, 0x24),
        (VecKind::Long, MemKind::Char) => (2, 1, 0x34),
        (VecKind::Long, MemKind::Byte) => (2, 1, 0x22),
        (VecKind::Double, MemKind::Float) => (1, 0, 0x5A),
        _ => return None,
    };
    let h = i32::try_from(cratonvm_types::HEADER_SIZE).ok()?;
    let s = i32::from(mem_bytes(src));
    let src_disp = h + (i32::from(disp) - h) * s / i32::from(kind.elem_bytes());
    let src_disp = i8::try_from(src_disp).ok()?;
    vex3(out, dst, IV_REG, base, mm, false, 0, true, pp, opc);
    mem_operand(out, dst, base, mem_bytes(src), src_disp);
    Some(())
}

/// VEC21-6: `VPSLLD/VPSRAD/VPSRLD/VPSLLQ/VPSRLQ ymm_dst, ymm_src, xmm_count`
/// (`VEX.256.66.0F F2|E2|D2|F3|D3 /r`: ModRM.reg the destination, VEX.vvvv
/// the source, ModRM.rm the count register, whose low 64 bits are the ONE
/// count every lane shifts by). `None` for a shift AVX2 lacks.
pub(crate) fn emit_shift_var(
    out: &mut Vec<u8>,
    kind: VecKind,
    s: VShift,
    dst: u8,
    src: u8,
    count: u8,
) -> Option<()> {
    let opc = match (kind, s) {
        (VecKind::Int, VShift::Shl) => 0xF2,
        (VecKind::Int, VShift::Sar) => 0xE2,
        (VecKind::Int, VShift::Shr) => 0xD2,
        (VecKind::Long, VShift::Shl) => 0xF3,
        (VecKind::Long, VShift::Shr) => 0xD3,
        _ => return None,
    };
    vex_rr(out, 1, 1, false, true, opc, dst, src, count);
    Some(())
}

/// VEC21-6: a shift-count register from the count in R11D, emitted once
/// before the loop. Java masks the count (`& 31` for `int` lanes, `& 63` for
/// `long`), and the masked value must be the WHOLE low quadword of the
/// register: x86 shifts every lane to zero for a count >= the lane width, and
/// a broadcast count (`(k & 31) | (k & 31) << 32`) is such a count.
///
/// ```text
/// AND  R11D, 31 | 63
/// VMOVD xmm_count, R11D        ; zeroes bits 32..255
/// ```
pub(crate) fn count_load_code(kind: VecKind, count: u8) -> Vec<u8> {
    let mask = if kind == VecKind::Long { 63 } else { 31 };
    let mut out = vec![0x41, 0x83, 0xE3, mask]; // AND R11D, imm8
                                                // VMOVD xmm, r32: VEX.128.66.0F.W0 6E /r, ModRM.reg = xmm, rm = R11.
    vex_rr(&mut out, 1, 1, false, false, 0x6E, count, 0, 11);
    out
}

/// `long` lanes have no `VPMULLQ` before AVX-512 (VEC21-2's `a[j] * 3L`):
/// `dst = dst * b` (mod 2^64) from three 32x32->64 `VPMULUDQ`, using `t1`,
/// `t2`. With `a = ah:al`, `a * b = al*bl + ((ah*bl + al*bh) << 32)` mod
/// 2^64, which is the scalar `lmul` exactly.
///
/// ```text
/// VPSRLQ   t1, dst, 32   ; ah
/// VPMULUDQ t1, t1, b     ; ah*bl
/// VPSRLQ   t2, b, 32     ; bh
/// VPMULUDQ t2, t2, dst   ; bh*al
/// VPADDQ   t1, t1, t2
/// VPSLLQ   t1, t1, 32
/// VPMULUDQ dst, dst, b   ; al*bl
/// VPADDQ   dst, dst, t1
/// ```
pub(crate) fn emit_long_mul(out: &mut Vec<u8>, dst: u8, b: u8, t1: u8, t2: u8) -> Option<()> {
    emit_shift(out, VecKind::Long, VShift::Shr, t1, dst, 32)?;
    vex_rr(out, 1, 1, false, true, 0xF4, t1, t1, b);
    emit_shift(out, VecKind::Long, VShift::Shr, t2, b, 32)?;
    vex_rr(out, 1, 1, false, true, 0xF4, t2, t2, dst);
    vex_rr(out, 1, 1, false, true, 0xD4, t1, t1, t2);
    emit_shift(out, VecKind::Long, VShift::Shl, t1, t1, 32)?;
    vex_rr(out, 1, 1, false, true, 0xF4, dst, dst, b);
    vex_rr(out, 1, 1, false, true, 0xD4, dst, dst, t1);
    Some(())
}

/// VEC21-2's narrowing store: the `VPSHUFB` control (both 128-bit halves) of
/// a plan whose `int` lanes are stored into a narrower array, `None` for any
/// other plan. Byte `k` of each half picks the low byte (`byte` store) or the
/// low word (`short`/`char`) of each dword; the second quadword repeats the
/// first, and only the first is ever stored. The lowerer broadcasts it into
/// `inv_regs[invariants.len() + shift_counts.len()]`.
pub(crate) fn narrow_store_mask(plan: &VecLoopPlan) -> Option<u64> {
    if plan.reduce.is_some() || plan.scan.is_some() || plan.kind != VecKind::Int {
        return None;
    }
    match plan.base_kinds.get(plan.store_base)? {
        MemKind::Byte => Some(u64::from_le_bytes([0, 4, 8, 12, 0x80, 0x80, 0x80, 0x80])),
        MemKind::Short | MemKind::Char => Some(u64::from_le_bytes([0, 1, 4, 5, 8, 9, 12, 13])),
        _ => None,
    }
}

/// The narrowing store of eight `int` lanes in `ymm_src` into `dst_kind`
/// elements at the step whose `int`-lane displacement is `disp`, using `u`
/// and the shuffle control in `mask` ([`narrow_store_mask`]). `src` is
/// clobbered. Truncation is exactly `bastore`/`sastore`/`castore` of each
/// lane (they keep the low 8/16 bits); a `boolean[]` destination, which masks
/// `& 1`, is sent to the scalar loop by the lowerer's kind-byte guard.
///
/// ```text
/// VPSHUFB      ymm_src, ymm_src, ymm_mask   ; each half: its 4 lanes, packed low
/// VEXTRACTI128 xmm_u, ymm_src, 1
/// VPUNPCKLDQ   xmm_src, xmm_src, xmm_u      ; byte: lanes 0-3 then 4-7 (8 bytes)
/// (VPUNPCKLQDQ for 16-bit: 8 words, 16 bytes)
/// VMOVQ / VMOVDQU [base + j*w + disp'], xmm_src
/// ```
pub(crate) fn emit_narrow_store(
    out: &mut Vec<u8>,
    dst_kind: MemKind,
    src: u8,
    u: u8,
    mask: u8,
    base: u8,
    disp: i8,
) -> Option<()> {
    let w = mem_bytes(dst_kind);
    if !matches!(w, 1 | 2) || src == u || src == mask || u == mask {
        return None;
    }
    // VPSHUFB ymm, ymm, ymm: VEX.256.66.0F38 00 /r.
    vex_rr(out, 2, 1, false, true, 0x00, src, src, mask);
    // VEXTRACTI128 xmm_u, ymm_src, 1: VEX.256.66.0F3A 39 /r ib, reg = ymm_src.
    vex3(out, src, 0, u, 3, false, 0, true, 1, 0x39);
    out.push(0xC0 | ((src & 7) << 3) | (u & 7));
    out.push(1);
    // VPUNPCKLDQ / VPUNPCKLQDQ xmm_src, xmm_src, xmm_u: VEX.128.66.0F 62 / 6C.
    vex_rr(
        out,
        1,
        1,
        false,
        false,
        if w == 1 { 0x62 } else { 0x6C },
        src,
        src,
        u,
    );
    let h = i32::try_from(cratonvm_types::HEADER_SIZE).ok()?;
    let d = h + (i32::from(disp) - h) * i32::from(w) / 4;
    let d = i8::try_from(d).ok()?;
    if w == 1 {
        // VMOVQ m64, xmm: VEX.128.66.0F D6 /r.
        vex3(out, src, IV_REG, base, 1, false, 0, false, 1, 0xD6);
    } else {
        // VMOVDQU m128, xmm: VEX.128.F3.0F 7F /r.
        vex3(out, src, IV_REG, base, 1, false, 0, false, 2, 0x7F);
    }
    mem_operand(out, src, base, w, d);
    Some(())
}

/// The narrowing-store mask's register from the mask in R11: `VMOVQ xmm, r11`
/// then `VPBROADCASTQ ymm, xmm`.
pub(crate) fn narrow_mask_load_code(reg: u8) -> Vec<u8> {
    let mut out = Vec::new();
    // VMOVQ xmm, r64: VEX.128.66.0F.W1 6E /r.
    vex_rr(&mut out, 1, 1, true, false, 0x6E, reg, 0, 11);
    // VPBROADCASTQ ymm, xmm: VEX.256.66.0F38.W0 59 /r.
    vex_rr(&mut out, 2, 1, false, true, 0x59, reg, 0, reg);
    out
}

/// `VMOVUPD`/`VMOVUPS`/`VMOVDQU [base + j*elem + disp], ymm`.
pub(crate) fn emit_store(out: &mut Vec<u8>, kind: VecKind, src: u8, base: u8, disp: i8) {
    let (pp, opc) = match kind {
        VecKind::Double => (1, 0x11),
        VecKind::Float => (0, 0x11),
        VecKind::Int | VecKind::Long | VecKind::Byte | VecKind::Short => (2, 0x7F),
    };
    vex3(out, src, IV_REG, base, 1, false, 0, true, pp, opc);
    mem_operand(out, src, base, kind.elem_bytes(), disp);
}

/// `VMOVAPS ymm_dst, ymm_src`.
fn emit_move(out: &mut Vec<u8>, dst: u8, src: u8) {
    if dst != src {
        vex3(out, dst, 0, src, 1, false, 0, true, 0, 0x28);
        out.push(0xC0 | ((dst & 7) << 3) | (src & 7));
    }
}

/// `VBROADCASTSD` / `VBROADCASTSS` / `VPBROADCASTD` / `VPBROADCASTQ` /
/// `VPBROADCASTB` / `VPBROADCASTW ymm_dst, xmm_src` (the AVX2 register forms;
/// the sub-word ones take the low byte / word of `src`).
pub(crate) fn emit_broadcast(out: &mut Vec<u8>, kind: VecKind, dst: u8, src: u8) {
    let opc = match kind {
        VecKind::Double => 0x19,
        VecKind::Float => 0x18,
        VecKind::Int => 0x58,
        VecKind::Long => 0x59,
        VecKind::Byte => 0x78,
        VecKind::Short => 0x79,
    };
    vex3(out, dst, 0, src, 2, false, 0, true, 1, opc);
    out.push(0xC0 | ((dst & 7) << 3) | (src & 7));
}

/// The registers one vector step computes in, and where the invariants sit.
pub(crate) struct StepRegs<'a> {
    /// Registers free for temporaries.
    pub(crate) temps: &'a [u8],
    /// `inv_regs[i]` holds `invariants[i]`, broadcast; after them,
    /// `inv_regs[invariants.len() + c]` holds `shift_counts[c]`, masked
    /// ([`count_load_code`], VEC21-6), then a narrowing store's shuffle
    /// control ([`narrow_store_mask`]).
    pub(crate) inv_regs: &'a [u8],
    /// A reduction's accumulator register (VEC21-4); `None` otherwise.
    pub(crate) acc: Option<u8>,
    /// An FP min/max reduction's NaN-seen register (VEC21-5): every step ORs
    /// its unordered-compare lanes in; `None` otherwise.
    pub(crate) nan: Option<u8>,
}

pub(crate) fn wide_broadcast(out: &mut Vec<u8>, kind: VecKind, dst: u8, src: u8) {
    avx512::broadcast(out, kind, dst, src);
}
pub(crate) fn scalar_vector_move(dst: u8, src: u8) -> Vec<u8> {
    avx512::scalar_move(dst, src)
}

/// The machine code of ONE vector step at byte displacement `disp` (the
/// element offset `HEADER_SIZE + step * 32` folded in), or `None` when the
/// tree needs more temporaries than `regs.temps` has (or a reduction has no
/// accumulator register).
/// [`avx512::masked_fill_tail`]: a wide fill's sub-step tail as one masked store.
pub(crate) fn masked_fill_tail_code(plan: &VecLoopPlan, regs: &StepRegs<'_>, disp: i8) -> Option<Vec<u8>> {
    avx512::masked_fill_tail(plan, regs, disp)
}

pub(crate) fn step_code(plan: &VecLoopPlan, regs: &StepRegs<'_>, disp: i8) -> Option<Vec<u8>> {
    if plan.uses_avx512() {
        return avx512::step(plan, regs, disp);
    }
    if plan.ordered_fp() {
        return None;
    }
    avx2_step_code(plan, regs, disp)
}

fn avx2_step_code(plan: &VecLoopPlan, regs: &StepRegs<'_>, disp: i8) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut free: Vec<u8> = regs.temps.iter().rev().copied().collect();
    if plan.scan.is_some() {
        return scan_step_code(plan, regs, disp);
    }
    if let Some(r) = plan.reduce {
        let acc = regs.acc?;
        if r.op.is_minmax() {
            minmax_step(
                plan,
                regs,
                acc,
                r.op == RedOp::Max,
                &mut free,
                disp,
                &mut out,
            )?;
        } else {
            let op = if r.op == RedOp::Sub {
                VOp::Sub
            } else {
                VOp::Add
            };
            match leaf_src(plan, regs, &plan.value, disp) {
                Some(src) => emit_binop(&mut out, plan.kind, op, acc, acc, src),
                None => {
                    let dst = free.pop()?;
                    gen(plan, regs, &plan.value, dst, &mut free, disp, &mut out)?;
                    emit_binop(&mut out, plan.kind, op, acc, acc, Src::Reg(dst));
                }
            }
        }
        // j27 wave 6 (lane T1LOOPFFM): a reduction may read the induction
        // too (`s += a[k] + k`, planned since round 24 B1 lets every tree
        // read `VExpr::Iota`); its lanes must advance like a store loop's,
        // or every step after the first re-adds the first step's `j + i`.
        push_iota_advance(plan, regs, &mut out);
        return Some(out);
    }
    let dst = free.pop()?;
    gen(plan, regs, &plan.value, dst, &mut free, disp, &mut out)?;
    match narrow_store_mask(plan) {
        // VEC21-2: `int` lanes into a narrower array; the shuffle mask sits
        // after the invariants and the shift counts in `inv_regs`.
        Some(_) => {
            let mask = *regs
                .inv_regs
                .get(plan.invariants.len() + plan.shift_counts.len())?;
            let u = free.pop()?;
            emit_narrow_store(
                &mut out,
                plan.base_kinds[plan.store_base],
                dst,
                u,
                mask,
                BASE_REGS[plan.store_base],
                disp,
            )?;
        }
        None => emit_store(&mut out, plan.kind, dst, BASE_REGS[plan.store_base], disp),
    }
    push_iota_advance(plan, regs, &mut out);
    Some(out)
}

/// JIT round 24 B1: the next step's `j + i` (`run += step`), after every
/// step of a plan that reads [`VExpr::Iota`] -- store and reduction alike.
fn push_iota_advance(plan: &VecLoopPlan, regs: &StepRegs<'_>, out: &mut Vec<u8>) {
    if let Some((run, step)) = iota_regs(plan, regs.inv_regs) {
        emit_binop(out, plan.kind, VOp::Add, run, run, Src::Reg(step));
    }
}

/// JIT round 24 B1: set a plan's iota registers up before the vector loop
/// (entered with `RDX` = `j`, sign-extended): `run` = `j + [0, 1, .., W-1]`,
/// `step` = `W` in every lane. Clobbers R11, XMM0 and XMM1; `None` for a
/// lane kind with no iota form or when `run` / `step` is XMM0 / XMM1.
///
/// ```text
///   mov r11, imm64 ; vmovq xmm0, r11 ; mov r11, imm64 ; vpinsrq xmm0, xmm0, r11, 1
///   mov r11, imm64 ; vmovq xmm1, r11 ; mov r11, imm64 ; vpinsrq xmm1, xmm1, r11, 1
///   vinserti128 run, ymm0, xmm1, 1                 ; [0, 1, .., W-1]
///   vmovq xmm0, rdx ; vpbroadcastd/q ymm1, xmm0 ; vpaddd/q run, run, ymm1
///   mov r11, W ; vmovq xmm0, r11 ; vpbroadcastd/q step, xmm0
/// ```
pub(crate) fn iota_init_code(kind: VecKind, run: u8, step: u8) -> Option<Vec<u8>> {
    let quads: [u64; 4] = match kind {
        VecKind::Int => [
            0x0000_0001_0000_0000,
            0x0000_0003_0000_0002,
            0x0000_0005_0000_0004,
            0x0000_0007_0000_0006,
        ],
        VecKind::Long => [0, 1, 2, 3],
        _ => return None,
    };
    if [run, step].iter().any(|&r| r <= 1) {
        return None;
    }
    let mut out = Vec::new();
    let mov_r11 = |out: &mut Vec<u8>, v: u64| {
        out.extend_from_slice(&[0x49, 0xBB]);
        out.extend_from_slice(&v.to_le_bytes());
    };
    // VMOVQ xmm, r64: VEX.128.66.0F.W1 6E /r.
    let vmovq = |out: &mut Vec<u8>, x: u8, g: u8| {
        vex3(out, x, 0, g, 1, true, 0, false, 1, 0x6E);
        out.push(0xC0 | ((x & 7) << 3) | (g & 7));
    };
    // VPINSRQ x, x, r64, 1: VEX.128.66.0F3A.W1 22 /r ib.
    let vpinsrq_hi = |out: &mut Vec<u8>, x: u8, g: u8| {
        vex3(out, x, 0, g, 3, true, x, false, 1, 0x22);
        out.push(0xC0 | ((x & 7) << 3) | (g & 7));
        out.push(1);
    };
    const R11: u8 = 11;
    mov_r11(&mut out, quads[0]);
    vmovq(&mut out, 0, R11);
    mov_r11(&mut out, quads[1]);
    vpinsrq_hi(&mut out, 0, R11);
    mov_r11(&mut out, quads[2]);
    vmovq(&mut out, 1, R11);
    mov_r11(&mut out, quads[3]);
    vpinsrq_hi(&mut out, 1, R11);
    // VINSERTI128 run, ymm0, xmm1, 1: VEX.256.66.0F3A.W0 38 /r ib.
    vex3(&mut out, run, 0, 1, 3, false, 0, true, 1, 0x38);
    out.push(0xC0 | ((run & 7) << 3) | 1);
    out.push(1);
    vmovq(&mut out, 0, IV_REG);
    emit_broadcast(&mut out, kind, 1, 0);
    emit_binop(&mut out, kind, VOp::Add, run, run, Src::Reg(1));
    // Cast: at most 32 lanes.
    mov_r11(&mut out, kind.lanes() as u64);
    vmovq(&mut out, 0, R11);
    emit_broadcast(&mut out, kind, step, 0);
    Some(out)
}

fn leaf_src(plan: &VecLoopPlan, regs: &StepRegs<'_>, e: &VExpr, disp: i8) -> Option<Src> {
    match e {
        // A widened element (L8-4, VEC21-2) is loaded by `VPMOVSX/ZX`,
        // never a memory operand of the lane op.
        VExpr::Load(b) if base_widens(plan, *b) => None,
        VExpr::Load(b) if plan.widen.is_none() => Some(Src::Mem {
            base: BASE_REGS[*b],
            disp,
        }),
        VExpr::Inv(i) => regs.inv_regs.get(*i).map(|&r| Src::Reg(r)),
        VExpr::Iota => iota_regs(plan, regs.inv_regs).map(|(r, _)| Src::Reg(r)),
        VExpr::Load(_)
        | VExpr::Bin(..)
        | VExpr::Shift(..)
        | VExpr::ShiftBy(..)
        | VExpr::MinMax { .. }
        | VExpr::I2L(_) => None,
    }
}

/// An element of `plan` into `dst`: `emit_load`, or (L8-4) the widening
/// `VPMOVSXBD ymm, m64` / `VPMOVSXWD ymm, m128` / `VPMOVZXWD ymm, m128`
/// (`VEX.256.66.0F38 21 / 23 / 33 /r`), eight elements into `int` lanes.
pub(crate) fn emit_elem_load(out: &mut Vec<u8>, plan: &VecLoopPlan, dst: u8, base: u8, disp: i8) {
    match plan.widen {
        None => emit_load(out, plan.kind, dst, base, disp),
        Some(w) => {
            let opc = match w {
                Widen::Byte => 0x21,
                Widen::Short => 0x23,
                Widen::Char => 0x33,
            };
            vex3(out, dst, IV_REG, base, 2, false, 0, true, 1, opc);
            mem_operand(out, dst, base, w.elem_bytes(), disp);
        }
    }
}

fn gen(
    plan: &VecLoopPlan,
    regs: &StepRegs<'_>,
    e: &VExpr,
    dst: u8,
    free: &mut Vec<u8>,
    disp: i8,
    out: &mut Vec<u8>,
) -> Option<()> {
    match e {
        VExpr::Load(b) if base_widens(plan, *b) => {
            emit_widen_load(
                out,
                plan.kind,
                plan.base_kinds[*b],
                dst,
                BASE_REGS[*b],
                disp,
            )?;
        }
        VExpr::Load(b) => emit_elem_load(out, plan, dst, BASE_REGS[*b], disp),
        VExpr::Inv(i) => emit_move(out, dst, *regs.inv_regs.get(*i)?),
        VExpr::Iota => emit_move(out, dst, iota_regs(plan, regs.inv_regs)?.0),
        VExpr::I2L(inner) => {
            gen_i32_x4(plan, regs, inner, dst, free, disp, out)?;
            // VPMOVSXDQ ymm_dst, xmm_dst: sign-extend 4 packed i32 -> 4 i64.
            vex3(out, dst, 0, dst, 2, false, 0, true, 1, 0x25);
            out.push(0xC0 | ((dst & 7) << 3) | (dst & 7));
        }
        VExpr::MinMax { max, l, r } => {
            elem_minmax(plan, regs, *max, l, r, dst, free, disp, out)?;
        }
        VExpr::Shift(s, imm, inner) => {
            gen(plan, regs, inner, dst, free, disp, out)?;
            emit_shift(out, plan.kind, *s, dst, dst, *imm)?;
        }
        // VEC21-6: the count registers follow the invariants' in `inv_regs`.
        VExpr::ShiftBy(s, c, inner) => {
            let count = *regs.inv_regs.get(plan.invariants.len() + *c)?;
            gen(plan, regs, inner, dst, free, disp, out)?;
            emit_shift_var(out, plan.kind, *s, dst, dst, count)?;
        }
        // VEC21-2: `long` lanes multiply in three `VPMULUDQ` (no `VPMULLQ`).
        VExpr::Bin(VOp::Mul, l, r) if plan.kind == VecKind::Long => {
            let (l, r) = if leaf_src(plan, regs, l, disp).is_some()
                && leaf_src(plan, regs, r, disp).is_none()
            {
                (r, l)
            } else {
                (l, r)
            };
            gen(plan, regs, l, dst, free, disp, out)?;
            let (b, b_temp) = match leaf_src(plan, regs, r, disp) {
                Some(Src::Reg(x)) => (x, false),
                _ => {
                    let t = free.pop()?;
                    gen(plan, regs, r, t, free, disp, out)?;
                    (t, true)
                }
            };
            let t1 = free.pop()?;
            let t2 = free.pop()?;
            emit_long_mul(out, dst, b, t1, t2)?;
            free.push(t2);
            free.push(t1);
            if b_temp {
                free.push(b);
            }
        }
        VExpr::Bin(op, l, r)
            if fp_nan_order(plan.kind)
                && leaf_src(plan, regs, l, disp).is_some()
                && leaf_src(plan, regs, r, disp).is_none() =>
        {
            // r22 w7: under the opt-in NaN operand order the left operand must
            // stay VEX src1, so the commuting trick below is off. Compute the
            // tree into `dst` first and name the leaf as src1 (`vop dst, l, dst`):
            // an invariant's own register, or one temporary for a load. Without
            // this the order cost a second temporary and declined the pre-loop.
            //
            // A load leaf with no temporary left (Windows: the pool is XMM0 /
            // XMM1 alone, and an invariant can take XMM1) falls back to the
            // commuted form `vop dst, dst, [l]` for a commuting op: the opt-in
            // must not turn vectorization off (coordinator, w7), so that one
            // op answers value1's NaN for two NaN lanes, the C2 / default rule.
            gen(plan, regs, r, dst, free, disp, out)?;
            match leaf_src(plan, regs, l, disp)? {
                Src::Reg(lr) => emit_binop(out, plan.kind, *op, dst, lr, Src::Reg(dst)),
                mem @ Src::Mem { .. } if free.is_empty() && op.commutative() => {
                    emit_binop(out, plan.kind, *op, dst, dst, mem);
                }
                Src::Mem { .. } => {
                    let t = free.pop()?;
                    gen(plan, regs, l, t, free, disp, out)?;
                    emit_binop(out, plan.kind, *op, dst, t, Src::Reg(dst));
                    free.push(t);
                }
            }
        }
        VExpr::Bin(op, l, r) => {
            // A leaf on the left and a tree on the right: compute the tree
            // first when the op commutes, so the leaf is a memory/register
            // operand and no temporary is needed.
            let (l, r) = if op.commutative()
                && !fp_nan_order(plan.kind)
                && leaf_src(plan, regs, l, disp).is_some()
                && leaf_src(plan, regs, r, disp).is_none()
            {
                (r, l)
            } else {
                (l, r)
            };
            gen(plan, regs, l, dst, free, disp, out)?;
            match leaf_src(plan, regs, r, disp) {
                Some(src) => emit_binop(out, plan.kind, *op, dst, dst, src),
                None => {
                    let t = free.pop()?;
                    gen(plan, regs, r, t, free, disp, out)?;
                    emit_binop(out, plan.kind, *op, dst, dst, Src::Reg(t));
                    free.push(t);
                }
            }
        }
    }
    Some(())
}

// ── L8-3: element-wise Math.min/max ──────────────────────────────────────

/// `dst = Math.max(l, r)` / `Math.min(l, r)` lane by lane (L8-3).
///
/// `int`: `VPMAXSD` / `VPMINSD`, exact. `long`: [`minmax_long`]'s compare and
/// blend, exact (a tie is one value). FP: [`fp_minmax_packed`], Java's answer
/// bit for bit in every lane, NaNs included.
#[allow(clippy::too_many_arguments)]
fn elem_minmax(
    plan: &VecLoopPlan,
    regs: &StepRegs<'_>,
    max: bool,
    l: &VExpr,
    r: &VExpr,
    dst: u8,
    free: &mut Vec<u8>,
    disp: i8,
    out: &mut Vec<u8>,
) -> Option<()> {
    match plan.kind {
        VecKind::Int => {
            // Commutative and exact: compute a tree first, keep a leaf as
            // the memory / register operand.
            let (l, r) = if leaf_src(plan, regs, l, disp).is_some()
                && leaf_src(plan, regs, r, disp).is_none()
            {
                (r, l)
            } else {
                (l, r)
            };
            gen(plan, regs, l, dst, free, disp, out)?;
            let opc = if max { 0x3D } else { 0x39 };
            match leaf_src(plan, regs, r, disp) {
                Some(src) => vex_src(out, 2, 1, opc, dst, dst, src, 4),
                None => {
                    let t = free.pop()?;
                    gen(plan, regs, r, t, free, disp, out)?;
                    vex_src(out, 2, 1, opc, dst, dst, Src::Reg(t), 4);
                    free.push(t);
                }
            }
        }
        VecKind::Long => {
            gen(plan, regs, l, dst, free, disp, out)?;
            let x = free.pop()?;
            gen(plan, regs, r, x, free, disp, out)?;
            let m = free.pop()?;
            minmax_long(out, true, max, dst, x, m);
            free.push(m);
            free.push(x);
        }
        VecKind::Double | VecKind::Float => {
            let a = free.pop()?;
            gen(plan, regs, l, a, free, disp, out)?;
            let b = free.pop()?;
            gen(plan, regs, r, b, free, disp, out)?;
            let m = free.pop()?;
            fp_minmax_packed(out, plan.kind == VecKind::Double, max, dst, a, b, m);
            free.push(m);
            free.push(b);
            free.push(a);
        }
        VecKind::Byte | VecKind::Short => return None,
    }
    Some(())
}

/// `ir_lower::fp_minmax_avx_bytes` over 256-bit registers (L8-3): Java's FP
/// `Math.min(a, b)` / `Math.max(a, b)` per lane into `dst`, with `m` as the
/// mask scratch; `a` and `b` are read, never written, and must differ from
/// `dst` and `m`.
///
/// ```text
///   VMINPD    dst, b, a       ; b < a ? b : a   (== -> a)
///   VMINPD    m, a, b         ; a < b ? a : b   (== -> b)
///   VORPD     dst, dst, m     ; equal zeros: -0.0 if either is
///   VCMPPD    m, b, b, UNORD  ; b is NaN?
///   VBLENDVPD dst, dst, b, m  ; then b
///   VCMPPD    m, a, a, UNORD  ; a is NaN?
///   VBLENDVPD dst, dst, a, m  ; then a (last: a NaN wins over b's)
/// ```
///
/// `max` is `VMAXPD` and `VANDPD`. A NaN is selected, never computed, so its
/// payload comes back exactly as stored -- the JDK body's answer.
#[allow(clippy::too_many_arguments)]
fn fp_minmax_packed(out: &mut Vec<u8>, dbl: bool, max: bool, dst: u8, a: u8, b: u8, m: u8) {
    let pp = u8::from(dbl);
    let op = if max { 0x5F } else { 0x5D };
    let bit = if max { 0x54 } else { 0x56 };
    let blendv = if dbl { 0x4B } else { 0x4A };
    vex_rr(out, 1, pp, false, true, op, dst, b, a);
    vex_rr(out, 1, pp, false, true, op, m, a, b);
    vex_rr(out, 1, pp, false, true, bit, dst, dst, m);
    for x in [b, a] {
        vex_rr(out, 1, pp, false, true, 0xC2, m, x, x);
        out.push(0x03); // UNORD_Q
        vex_rr(out, 3, 1, false, true, blendv, dst, dst, x);
        out.push(m << 4); // is4: the mask register
    }
}

// ── VEC21-5: min/max reductions ──────────────────────────────────────────

/// A register-register VEX op `opc reg, vvvv, rm` (`l256`: YMM, else XMM).
#[allow(clippy::too_many_arguments)]
fn vex_rr(
    out: &mut Vec<u8>,
    mm: u8,
    pp: u8,
    w: bool,
    l256: bool,
    opc: u8,
    reg: u8,
    vvvv: u8,
    rm: u8,
) {
    vex3(out, reg, 0, rm, mm, w, vvvv, l256, pp, opc);
    out.push(0xC0 | ((reg & 7) << 3) | (rm & 7));
}

/// `opc ymm_dst, ymm_src1, src2` (256-bit), `src2` a register or the element
/// operand `[base + j*elem + disp]`.
#[allow(clippy::too_many_arguments)]
fn vex_src(out: &mut Vec<u8>, mm: u8, pp: u8, opc: u8, dst: u8, src1: u8, src2: Src, elem: u8) {
    match src2 {
        Src::Reg(r) => vex_rr(out, mm, pp, false, true, opc, dst, src1, r),
        Src::Mem { base, disp } => {
            vex3(out, dst, IV_REG, base, mm, false, src1, true, pp, opc);
            mem_operand(out, dst, base, elem, disp);
        }
    }
}

/// `long` lanes have no `VPMAXSQ` before AVX-512: compare and blend.
///
/// ```text
/// max: VPCMPGTQ  m, x, acc        ; x > acc
/// min: VPCMPGTQ  m, acc, x        ; acc > x
///      VBLENDVPD acc, acc, x, m   ; the lanes the mask picks take x
/// ```
///
/// Exact: two's-complement signed order, and on a tie either operand is the
/// same value.
fn minmax_long(out: &mut Vec<u8>, l256: bool, is_max: bool, acc: u8, x: u8, m: u8) {
    if is_max {
        vex_rr(out, 2, 1, false, l256, 0x37, m, x, acc);
    } else {
        vex_rr(out, 2, 1, false, l256, 0x37, m, acc, x);
    }
    // VBLENDVPD: VEX.66.0F3A.W0 4B /r is4 -- `m ? rm : vvvv`.
    vex_rr(out, 3, 1, false, l256, 0x4B, acc, acc, x);
    out.push(m << 4);
}

/// Java's FP `Math.max` / `Math.min` of NaN-free lanes, `acc = op(acc, x)`,
/// using `t` as scratch; with `nan`, first ORs `x`'s unordered lanes into it.
///
/// `VMAXPD a, b` is `a > b ? a : b`: its SECOND operand on a tie (`+0.0` vs
/// `-0.0` compare equal) and on any NaN. Both orders, then AND (max) / OR
/// (min) of the two answers: when the values differ both orders agree; when
/// they tie the bits combine to `+0.0` unless both are `-0.0` (max) or to
/// `-0.0` if either is (min), and equal non-zero values are the same bits.
/// A NaN lane is NOT answered right -- the NaN-seen register records it and
/// the pre-loop is discarded (see `ir_lower::emit_vector_preloop`).
///
/// ```text
/// VCMPUNORDPD t, x, x       ; (nan) x is NaN?
/// VORPD       nan, nan, t
/// VMAXPD      t, acc, x     ; tie -> x
/// VMAXPD      acc, x, acc   ; tie -> acc
/// VANDPD      acc, acc, t   ; (min: VMINPD / VORPD)
/// ```
#[allow(clippy::too_many_arguments)]
fn minmax_fp(
    out: &mut Vec<u8>,
    dbl: bool,
    l256: bool,
    is_max: bool,
    acc: u8,
    x: u8,
    t: u8,
    nan: Option<u8>,
) {
    let pp = u8::from(dbl);
    if let Some(nan) = nan {
        vex_rr(out, 1, pp, false, l256, 0xC2, t, x, x);
        out.push(0x03); // UNORD_Q
        vex_rr(out, 1, pp, false, l256, 0x56, nan, nan, t);
    }
    let op = if is_max { 0x5F } else { 0x5D };
    vex_rr(out, 1, pp, false, l256, op, t, acc, x);
    vex_rr(out, 1, pp, false, l256, op, acc, x, acc);
    let bit = if is_max { 0x54 } else { 0x56 };
    vex_rr(out, 1, pp, false, l256, bit, acc, acc, t);
}

/// One min/max reduction step: `acc = op(acc, value)` lane by lane.
fn minmax_step(
    plan: &VecLoopPlan,
    regs: &StepRegs<'_>,
    acc: u8,
    is_max: bool,
    free: &mut Vec<u8>,
    disp: i8,
    out: &mut Vec<u8>,
) -> Option<()> {
    match plan.kind {
        VecKind::Int => {
            // VPMAXSD / VPMINSD: 66 0F38 3D / 39, exact as they stand.
            let opc = if is_max { 0x3D } else { 0x39 };
            let src = match leaf_src(plan, regs, &plan.value, disp) {
                Some(s) => s,
                None => {
                    let t = free.pop()?;
                    gen(plan, regs, &plan.value, t, free, disp, out)?;
                    free.push(t);
                    Src::Reg(t)
                }
            };
            vex_src(out, 2, 1, opc, acc, acc, src, 4);
        }
        VecKind::Long => {
            let x = free.pop()?;
            gen(plan, regs, &plan.value, x, free, disp, out)?;
            let m = free.pop()?;
            minmax_long(out, true, is_max, acc, x, m);
            free.push(m);
            free.push(x);
        }
        VecKind::Double | VecKind::Float => {
            let nan = regs.nan?;
            let x = free.pop()?;
            gen(plan, regs, &plan.value, x, free, disp, out)?;
            let t = free.pop()?;
            minmax_fp(
                out,
                plan.kind == VecKind::Double,
                true,
                is_max,
                acc,
                x,
                t,
                Some(nan),
            );
            free.push(t);
            free.push(x);
        }
        VecKind::Byte | VecKind::Short => return None,
    }
    Some(())
}

/// `VCMPUNORDPD/PS ymm_nan, ymm_acc, ymm_acc`: the NaN-seen register's seed
/// (an FP min/max reduction whose seed is NaN answers the seed, which the
/// scalar loop does by itself).
pub(crate) fn minmax_nan_init_code(kind: VecKind, acc: u8, nan: u8) -> Vec<u8> {
    let mut out = Vec::new();
    vex_rr(
        &mut out,
        1,
        u8::from(kind == VecKind::Double),
        false,
        true,
        0xC2,
        nan,
        acc,
        acc,
    );
    out.push(0x03);
    out
}

/// `VPTEST ymm_nan, ymm_nan`: ZF clear when some lane saw a NaN.
pub(crate) fn minmax_nan_test_code(nan: u8) -> Vec<u8> {
    let mut out = Vec::new();
    vex_rr(&mut out, 2, 1, false, true, 0x17, nan, 0, nan);
    out
}

/// The horizontal fold of a min/max accumulator (VEC21-5), `s0`/`s1` scratch:
/// the answer in EAX (`int`), RAX (`long`) or XMM0's low lane (FP). The lanes
/// were seeded with the accumulator's entry value, so the fold IS the new
/// accumulator; an FP fold runs only on NaN-free lanes.
pub(crate) fn minmax_fold_code(
    kind: VecKind,
    is_max: bool,
    acc: u8,
    s0: u8,
    s1: u8,
) -> Option<Vec<u8>> {
    // `int` needs one scratch (`s1` is then unused and may equal `s0`).
    let needs_s1 = kind != VecKind::Int;
    if acc == s0 || acc == 0 || kind.is_subword() || (needs_s1 && (acc == s1 || s0 == s1)) {
        return None;
    }
    let mut out = Vec::new();
    // VEXTRACTI128 xmm_s0, ymm_acc, 1.
    vex3(&mut out, acc, 0, s0, 3, false, 0, true, 1, 0x39);
    out.push(0xC0 | ((acc & 7) << 3) | (s0 & 7));
    out.push(1);
    let combine = |out: &mut Vec<u8>| match kind {
        VecKind::Int => {
            vex_rr(
                out,
                2,
                1,
                false,
                false,
                if is_max { 0x3D } else { 0x39 },
                acc,
                acc,
                s0,
            );
        }
        VecKind::Long => minmax_long(out, false, is_max, acc, s0, s1),
        _ => minmax_fp(
            out,
            kind == VecKind::Double,
            false,
            is_max,
            acc,
            s0,
            s1,
            None,
        ),
    };
    combine(&mut out);
    let shuf: &[u8] = if kind.elem_bytes() == 8 {
        &[0x4E]
    } else {
        &[0x4E, 0xB1]
    };
    for &imm in shuf {
        // VPSHUFD xmm_s0, xmm_acc, imm.
        vex3(&mut out, s0, 0, acc, 1, false, 0, false, 1, 0x70);
        out.push(0xC0 | ((s0 & 7) << 3) | (acc & 7));
        out.push(imm);
        combine(&mut out);
    }
    match kind {
        // VMOVD EAX, xmm_acc / VMOVQ RAX, xmm_acc.
        VecKind::Int | VecKind::Long => {
            vex3(
                &mut out,
                acc,
                0,
                0,
                1,
                kind == VecKind::Long,
                0,
                false,
                1,
                0x7E,
            );
            out.push(0xC0 | ((acc & 7) << 3));
        }
        // VMOVAPS xmm0, xmm_acc.
        _ => vex_rr(&mut out, 1, 0, false, false, 0x28, 0, 0, acc),
    }
    Some(out)
}

// ── VEC21-8: early-exit scans ────────────────────────────────────────────

/// One scan step at `disp`: the lane compare(s) into a temporary, its byte
/// mask into R9D ([`SCAN_MASK_REG`]), and `JNZ rel32` (placeholder: the LAST
/// FOUR BYTES, which the caller patches to this step's exit) when some lane
/// hits.
///
/// ```text
/// VPCMPEQx/GTx t, x, y      ; x a register (an invariant, or the element loaded)
/// VPMOVMSKB    r9d, t
/// TEST r9d, r9d             ; or, for a miss mask, CMP r9d, -1
/// JNZ  exit_k
/// ```
///
/// L8-4: a test of several compares evaluates each into a register and
/// combines them along [`ScanHit`] with `VPAND` / `VPOR` / `VPANDN` (a
/// complemented side folds into `VPANDN`, so no all-ones register is needed);
/// the mask then holds the hits or, when [`VecScan::mask_complemented`], the
/// misses.
pub(crate) fn scan_step_code(plan: &VecLoopPlan, regs: &StepRegs<'_>, disp: i8) -> Option<Vec<u8>> {
    let s = plan.scan.as_ref()?;
    let mut free: Vec<u8> = regs.temps.iter().rev().copied().collect();
    let t = free.pop()?;
    let mut out = Vec::new();
    let complemented = eval_hit(plan, regs, s, &s.hit, t, &mut free, disp, &mut out)?;
    debug_assert_eq!(complemented, s.mask_complemented());
    // VPMOVMSKB r9d, ymm_t: VEX.256.66.0F D7 /r.
    vex_rr(&mut out, 1, 1, false, true, 0xD7, SCAN_MASK_REG, 0, t);
    if complemented {
        out.extend_from_slice(&[0x41, 0x83, 0xF9, 0xFF]); // CMP R9D, -1
    } else {
        out.extend_from_slice(&[0x45, 0x85, 0xC9]); // TEST R9D, R9D
    }
    out.extend_from_slice(&[0x0F, 0x85, 0, 0, 0, 0]); // JNZ exit_k
    Some(out)
}

/// `hit` into `dst`; `Some(true)` when `dst` holds its complement.
#[allow(clippy::too_many_arguments)]
fn eval_hit(
    plan: &VecLoopPlan,
    regs: &StepRegs<'_>,
    s: &VecScan,
    hit: &ScanHit,
    dst: u8,
    free: &mut Vec<u8>,
    disp: i8,
    out: &mut Vec<u8>,
) -> Option<bool> {
    const VPAND: u8 = 0xDB;
    const VPANDN: u8 = 0xDF;
    const VPOR: u8 = 0xEB;
    let (a, b, and) = match hit {
        ScanHit::Test(i) => {
            return scan_test_code(plan, regs, s.tests.get(*i)?, dst, free, disp, out)
        }
        ScanHit::And(a, b) => (a, b, true),
        ScanHit::Or(a, b) => (a, b, false),
    };
    let ca = eval_hit(plan, regs, s, a, dst, free, disp, out)?;
    let t = free.pop()?;
    let cb = eval_hit(plan, regs, s, b, t, free, disp, out)?;
    free.push(t);
    // `VPANDN d, x, y` is `~x & y`. With A = dst, B = t (each the hit or its
    // complement): AND of two complements is the complement of their OR, OR
    // of two complements the complement of their AND, and a mixed pair is
    // one VPANDN either way.
    let (opc, src1, src2, complemented) = match (and, ca, cb) {
        (true, false, false) => (VPAND, dst, t, false),
        (true, true, false) => (VPANDN, dst, t, false),
        (true, false, true) => (VPANDN, t, dst, false),
        (true, true, true) => (VPOR, dst, t, true),
        (false, false, false) => (VPOR, dst, t, false),
        (false, true, true) => (VPAND, dst, t, true),
        // ~A | B = ~(A & ~B) = ~(~B & A)
        (false, true, false) => (VPANDN, t, dst, true),
        // A | ~B = ~(~A & B)
        (false, false, true) => (VPANDN, dst, t, true),
    };
    vex_rr(out, 1, 1, false, true, opc, dst, src1, src2);
    Some(complemented)
}

/// One compare of a scan into `dst`; `Some(true)` when `dst` holds the
/// test's complement (a `miss` test, or [`ScanCmp::GtU16`]'s inverted sense).
#[allow(clippy::too_many_arguments)]
fn scan_test_code(
    plan: &VecLoopPlan,
    regs: &StepRegs<'_>,
    test: &ScanTest,
    dst: u8,
    free: &mut Vec<u8>,
    disp: i8,
    out: &mut Vec<u8>,
) -> Option<bool> {
    let (mut x, mut y) = (&test.lhs, &test.rhs);
    // `==` (and an FP `==` / `!=`) commutes: an invariant register or a
    // computed tree as the left operand, an element as the memory one.
    let commutes = match test.cmp {
        ScanCmp::Eq => true,
        ScanCmp::Fp(p) => matches!(p, 0x00 | 0x04),
        _ => false,
    };
    if commutes
        && leaf_src(plan, regs, x, disp).is_some()
        && (matches!(y, VExpr::Inv(_)) || leaf_src(plan, regs, y, disp).is_none())
    {
        std::mem::swap(&mut x, &mut y);
    }
    let xr = match x {
        VExpr::Inv(i) => *regs.inv_regs.get(*i)?,
        _ => {
            gen(plan, regs, x, dst, free, disp, out)?;
            dst
        }
    };
    let mut held: Option<u8> = None;
    let ysrc = match leaf_src(plan, regs, y, disp) {
        Some(src) => src,
        None => {
            let t = free.pop()?;
            gen(plan, regs, y, t, free, disp, out)?;
            held = Some(t);
            Src::Reg(t)
        }
    };
    let elem = plan.kind.elem_bytes();
    let complemented = match (test.cmp, plan.kind) {
        (ScanCmp::GtU16, VecKind::Short) => {
            // VPMAXUW dst, x, y (66 0F38 3E); VPCMPEQW dst, dst, y (66 0F 75):
            // the lanes where y >= x (unsigned) -- the complement of x > y.
            vex_src(out, 2, 1, 0x3E, dst, xr, ysrc, elem);
            vex_src(out, 1, 1, 0x75, dst, dst, ysrc, elem);
            !test.miss
        }
        (ScanCmp::Fp(pred), VecKind::Double | VecKind::Float) => {
            // VCMPPD/PS dst, x, y, pred: 66/none 0F C2 /r ib.
            vex_src(
                out,
                1,
                u8::from(plan.kind == VecKind::Double),
                0xC2,
                dst,
                xr,
                ysrc,
                elem,
            );
            out.push(pred);
            test.miss
        }
        (ScanCmp::Eq | ScanCmp::Gt, _) => {
            let (mm, opc) = match (test.cmp, plan.kind) {
                (ScanCmp::Eq, VecKind::Byte) => (1, 0x74),
                (ScanCmp::Eq, VecKind::Short) => (1, 0x75),
                (ScanCmp::Eq, VecKind::Int) => (1, 0x76),
                (ScanCmp::Eq, VecKind::Long) => (2, 0x29),
                (ScanCmp::Gt, VecKind::Byte) => (1, 0x64),
                (ScanCmp::Gt, VecKind::Short) => (1, 0x65),
                (ScanCmp::Gt, VecKind::Int) => (1, 0x66),
                (ScanCmp::Gt, VecKind::Long) => (2, 0x37),
                _ => return None,
            };
            vex_src(out, mm, 1, opc, dst, xr, ysrc, elem);
            test.miss
        }
        _ => return None,
    };
    if let Some(t) = held {
        free.push(t);
    }
    Some(complemented)
}

/// Step `k`'s exit (entered with R9D its mask): `j` += `k * lanes` + the first
/// hit lane, so the scalar loop resumes AT the element that leaves. The
/// caller appends the jump to the write-back.
///
/// ```text
/// NOT r9d                   ; (miss mask) hits become set bits
/// BSF r9d, r9d              ; first hit byte
/// SHR r9d, log2(elem)       ; -> lane
/// ADD rdx, r9
/// ADD rdx, k * lanes
/// ```
pub(crate) fn scan_exit_code(kind: VecKind, miss: bool, k_elems: i32) -> Vec<u8> {
    let mut out = Vec::new();
    if miss {
        out.extend_from_slice(&[0x41, 0xF7, 0xD1]); // NOT R9D
    }
    out.extend_from_slice(&[0x45, 0x0F, 0xBC, 0xC9]); // BSF R9D, R9D
    let sh = match kind.elem_bytes() {
        8 => 3,
        4 => 2,
        2 => 1,
        _ => 0,
    };
    if sh != 0 {
        out.extend_from_slice(&[0x41, 0xC1, 0xE9, sh]); // SHR R9D, sh
    }
    out.extend_from_slice(&[0x4C, 0x01, 0xCA]); // ADD RDX, R9
    if k_elems != 0 {
        out.extend_from_slice(&[0x48, 0x81, 0xC2]); // ADD RDX, imm32
        out.extend_from_slice(&k_elems.to_le_bytes());
    }
    out
}

fn gen_i32_x4(
    plan: &VecLoopPlan,
    regs: &StepRegs<'_>,
    e: &VExpr,
    dst: u8,
    free: &mut Vec<u8>,
    disp: i8,
    out: &mut Vec<u8>,
) -> Option<()> {
    match e {
        VExpr::Iota => {
            let (run, _) = iota_regs(plan, regs.inv_regs)?;
            let t = free.pop()?;
            vex3(out, run, 0, t, 3, false, 0, true, 1, 0x39);
            out.push(0xC0 | ((run & 7) << 3) | (t & 7));
            out.push(1);
            vex3(out, dst, 0, t, 1, false, run, false, 0, 0xC6);
            out.push(0xC0 | ((dst & 7) << 3) | (t & 7));
            out.push(0x88);
            free.push(t);
        }
        VExpr::Inv(i) => {
            let r = *regs.inv_regs.get(*i)?;
            vex3(out, dst, 0, r, 1, false, 0, false, 1, 0x70);
            out.push(0xC0 | ((dst & 7) << 3) | (r & 7));
            out.push(0);
        }
        VExpr::Load(b) => {
            let k = *plan.base_kinds.get(*b)?;
            let base = BASE_REGS[*b];
            let h = i32::try_from(cratonvm_types::HEADER_SIZE).ok()?;
            let s = i32::from(mem_bytes(k));
            let src_disp =
                i8::try_from(h + (i32::from(disp) - h) * s / i32::from(plan.kind.elem_bytes()))
                    .ok()?;
            let (mm, pp, opc) = match k {
                MemKind::Int => (1, 2, 0x6F),
                MemKind::Byte => (2, 1, 0x21),
                MemKind::Short => (2, 1, 0x23),
                MemKind::Char => (2, 1, 0x33),
                _ => return None,
            };
            vex3(out, dst, IV_REG, base, mm, false, 0, false, pp, opc);
            mem_operand(out, dst, base, mem_bytes(k), src_disp);
        }
        VExpr::Shift(s, imm, inner) => {
            gen_i32_x4(plan, regs, inner, dst, free, disp, out)?;
            emit_shift_w(out, VecKind::Int, *s, dst, dst, *imm, false)?;
        }
        VExpr::Bin(op, l, r) => {
            if *op == VOp::Div {
                return None;
            }
            gen_i32_x4(plan, regs, l, dst, free, disp, out)?;
            let t = free.pop()?;
            gen_i32_x4(plan, regs, r, t, free, disp, out)?;
            emit_binop_w(out, VecKind::Int, *op, dst, dst, Src::Reg(t), false);
            free.push(t);
        }
        VExpr::MinMax { max, l, r } => {
            gen_i32_x4(plan, regs, l, dst, free, disp, out)?;
            let t = free.pop()?;
            gen_i32_x4(plan, regs, r, t, free, disp, out)?;
            let opc = if *max { 0x3D } else { 0x39 };
            vex3(out, dst, 0, t, 2, false, dst, false, 1, opc);
            out.push(0xC0 | ((dst & 7) << 3) | (t & 7));
            free.push(t);
        }
        VExpr::ShiftBy(..) | VExpr::I2L(_) => return None,
    }
    Some(())
}

#[cfg(test)]
mod tests;

/// j27 wave 6 (lane T1LOOPFFM): VEC27-1's scoped-get reduction planner and
/// the reduction step's induction lanes.
#[cfg(test)]
mod j27_t1loopffm_tests;

#[cfg(test)]
mod i2l_and_pure_iv_tests;
