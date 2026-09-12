// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Structured **bytecode** differential fuzzer for CratonVM's JIT tiers.
//!
//! Every other generator in this crate emits Java source and relies on `javac`.
//! That caps what it can reach: `javac` folds `Integer.MIN_VALUE / -1`, never
//! emits `swap`, uses `dup2_x2` only for compound assignment to `long[]`
//! elements, and never widens a local that fits in a byte. Those are exactly
//! the shapes a JIT's instruction selection, stack-to-register mapping and
//! constant folding get wrong. So this module writes class files directly,
//! through the typed assembler in [`crate::classgen`], and runs them under the
//! interpreter and each requested JIT mode.
//!
//! ## The program shape
//!
//! Each generated class has two methods:
//!
//! * `static long t(int a, long b, float f, double d)` — a sequence of
//!   *snippets*, each drawn from a [`Category`] and each folding one `long`
//!   result into an FNV-style accumulator. Every snippet starts and ends with
//!   an empty operand stack, so any subset of snippets is still a valid method —
//!   which is what makes minimization a list operation.
//! * `main` — prints `t` for every input once, then calls `t` over every input
//!   for `rounds` iterations (so the JIT tiers actually compile it, and OSR has
//!   a hot loop to enter), and declares the accumulated result as a
//!   `##DIFFTEST-CHECKSUM## jitfuzz <value>` line.
//!
//! ## The oracle
//!
//! The reference mode (`nojit`, the interpreter) is the oracle; no JDK is
//! involved. A mode whose output differs from it on any gated dimension
//! (stdout, checksum, exit code, uncaught exception) is a finding. See
//! [`crate::crossmode`] for why a CratonVM-vs-CratonVM split needs no reference
//! JDK to be a defect.
//!
//! ## Determinism
//!
//! Generation depends only on the seed: the PRNG is [`Rng`] seeded through
//! [`splitmix64`], nothing iterates a hash map, and nothing reads the clock or
//! the environment. `generation_is_byte_identical_per_seed` pins that.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::classgen::VType::{Double, Float, Int, Long};
use crate::classgen::{self, op, Asm, ClassVersion, CodeBody, ConstantPool, Frame, Label, MethodDef, VType};
use crate::generate::Rng;
use crate::ledger::Observation;
use crate::oracle::{self, ChannelDiff, Normalizer, Verdict};
use crate::runner::{self, Mode, RunError};

/// Descriptor of the method under test.
pub const TEST_DESC: &str = "(IJFD)J";
/// The `fuzz-jit --modes` default: the interpreter oracle plus the JIT
/// execution paths and the moving young-gen collector.
pub const DEFAULT_MODES: &str = "nojit,direct-emit,ir-jit,osr-eager,forced-deopt,moving-gc";
/// Default `main` loop trip count.
pub const DEFAULT_ROUNDS: u32 = 256;
/// The checksum name `main` declares.
pub const CHECKSUM_NAME: &str = "jitfuzz";

// ---------------------------------------------------------------------------
// Categories
// ---------------------------------------------------------------------------

/// The semantic families the generator deliberately covers. Each is a place
/// JIT tiers have historically disagreed with the JVMS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    /// `MIN_VALUE / -1`, `MIN_VALUE % -1` for `int` and `long` (x86 `idiv` traps).
    DivRemOverflow,
    /// Division / remainder by zero caught by an `ArithmeticException` handler.
    DivByZeroCaught,
    /// Shift counts -1, 0, 31, 32, 33, 63, 64, constant and variable.
    ShiftCounts,
    /// `fcmpl`/`fcmpg`/`dcmpl`/`dcmpg` with NaN and -0.0, as values and fused
    /// into branches.
    FpCompare,
    /// `f2i`/`f2l`/`d2i`/`d2l` with NaN, ±Infinity and out-of-range values.
    FpToInt,
    /// `frem` / `drem` (no x86 instruction; a runtime call in most JITs).
    FpRem,
    /// `i2b` / `i2c` / `i2s`.
    IntNarrowing,
    /// `tableswitch` / `lookupswitch` including `MIN_VALUE`/`MAX_VALUE` keys.
    Switches,
    /// Counted loops entered with `i > n`, zero-trip loops, and
    /// `n == Integer.MAX_VALUE` inclusive loops (bounded by a guard counter).
    LoopBounds,
    /// `int[]` sums into a `long` accumulator that overflow 32 bits.
    ArrayLongSum,
    /// `double[]` sums whose result depends on evaluation order
    /// (1e16, 1, -1e16, 1).
    DoubleSumOrder,
    /// Aliased arrays (`a == b`) in a recurrence loop.
    ArrayAlias,
    /// Short-circuit ternaries and `x * (c ? y : 2.0)` inside a loop.
    Ternary,
    /// A try/catch whose handler nulls a local before the join.
    HandlerNullJoin,
    /// `dup_x2`, `dup2_x1`, `dup2_x2` (all forms), `swap`, `pop2`.
    StackShuffles,
    /// `wide` loads, stores and `iinc`, on small and on >255 local indices.
    WideLocals,
}

impl Category {
    /// Every category, in generation round-robin order.
    pub const ALL: [Category; 16] = [
        Category::DivRemOverflow,
        Category::DivByZeroCaught,
        Category::ShiftCounts,
        Category::FpCompare,
        Category::FpToInt,
        Category::FpRem,
        Category::IntNarrowing,
        Category::Switches,
        Category::LoopBounds,
        Category::ArrayLongSum,
        Category::DoubleSumOrder,
        Category::ArrayAlias,
        Category::Ternary,
        Category::HandlerNullJoin,
        Category::StackShuffles,
        Category::WideLocals,
    ];

    /// Stable kebab-case label for reports.
    pub fn label(self) -> &'static str {
        match self {
            Category::DivRemOverflow => "div-rem-overflow",
            Category::DivByZeroCaught => "div-by-zero-caught",
            Category::ShiftCounts => "shift-counts",
            Category::FpCompare => "fp-compare",
            Category::FpToInt => "fp-to-int",
            Category::FpRem => "fp-rem",
            Category::IntNarrowing => "int-narrowing",
            Category::Switches => "switches",
            Category::LoopBounds => "loop-bounds",
            Category::ArrayLongSum => "array-long-sum",
            Category::DoubleSumOrder => "double-sum-order",
            Category::ArrayAlias => "array-alias",
            Category::Ternary => "ternary",
            Category::HandlerNullJoin => "handler-null-join",
            Category::StackShuffles => "stack-shuffles",
            Category::WideLocals => "wide-locals",
        }
    }
}

// ---------------------------------------------------------------------------
// Program model
// ---------------------------------------------------------------------------

/// One argument tuple for `t`.
#[derive(Debug, Clone, Copy)]
pub struct Input {
    pub a: i32,
    pub b: i64,
    pub f: f32,
    pub d: f64,
    /// XOR `a` with `round & 3` inside `main`'s hot loop, so a JIT cannot
    /// constant-fold every call site.
    pub vary: bool,
}

/// One code fragment of `t`. The fragment's details are re-derived from `seed`
/// at build time, so removing a snippet never changes another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snippet {
    pub category: Category,
    pub seed: u64,
}

/// A generated program: everything needed to rebuild its class bytes.
#[derive(Debug, Clone)]
pub struct JitProgram {
    pub seed: u64,
    pub name: String,
    pub snippets: Vec<Snippet>,
    pub inputs: Vec<Input>,
    pub rounds: u32,
}

impl JitProgram {
    /// The distinct categories this program exercises, in first-use order.
    pub fn categories(&self) -> Vec<Category> {
        let mut out = Vec::new();
        for s in &self.snippets {
            if !out.contains(&s.category) {
                out.push(s.category);
            }
        }
        out
    }
}

/// SplitMix64 finalizer — decorrelates adjacent seeds before they reach the
/// xorshift state (xorshift from small consecutive seeds starts correlated).
pub fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn pick<T: Copy>(r: &mut Rng, xs: &[T]) -> T {
    xs[r.below(xs.len())]
}

const INT_POOL: &[i32] = &[
    i32::MIN,
    i32::MIN + 1,
    -65,
    -1,
    0,
    1,
    2,
    7,
    31,
    32,
    33,
    63,
    64,
    255,
    65536,
    i32::MAX - 1,
    i32::MAX,
];
const LONG_POOL: &[i64] = &[
    i64::MIN,
    i64::MIN + 1,
    -1,
    0,
    1,
    31,
    64,
    1 << 32,
    (1 << 32) + 7,
    -(1 << 31),
    (i32::MAX as i64) + 1,
    i64::MAX,
];
const FLOAT_POOL: &[f32] = &[
    f32::NAN,
    -0.0,
    0.0,
    f32::INFINITY,
    f32::NEG_INFINITY,
    1.5,
    -2.5,
    3.0e9,
    -3.0e9,
    2_147_483_520.0,
    1.0e-45,
    f32::MAX,
];
const DOUBLE_POOL: &[f64] = &[
    f64::NAN,
    -0.0,
    0.0,
    f64::INFINITY,
    f64::NEG_INFINITY,
    1e16,
    -1e16,
    9.3e18,
    -9.3e18,
    2_147_483_648.0,
    -2_147_483_649.0,
    0.1,
    f64::MIN_POSITIVE,
];

/// Generate the program for `seed`.
///
/// The first snippet's category is `Category::ALL[seed % 16]`, so any 16
/// consecutive seeds cover every category at least once; the rest are drawn
/// from the seeded PRNG.
pub fn generate_program(seed: u64, rounds: u32) -> JitProgram {
    let mut r = Rng::new(splitmix64(seed));
    let n = 3 + r.below(4);
    let mut snippets = Vec::with_capacity(n);
    for i in 0..n {
        let category = if i == 0 {
            Category::ALL[(seed % Category::ALL.len() as u64) as usize]
        } else {
            Category::ALL[r.below(Category::ALL.len())]
        };
        snippets.push(Snippet {
            category,
            seed: r.next_u64(),
        });
    }
    let n_inputs = 6 + r.below(5);
    let inputs = (0..n_inputs)
        .map(|_| Input {
            a: pick(&mut r, INT_POOL),
            b: pick(&mut r, LONG_POOL),
            f: pick(&mut r, FLOAT_POOL),
            d: pick(&mut r, DOUBLE_POOL),
            vary: r.below(3) == 0,
        })
        .collect();
    JitProgram {
        seed,
        name: format!("JitFuzz_{seed}"),
        snippets,
        inputs,
        rounds: rounds.max(1),
    }
}

// ---------------------------------------------------------------------------
// Class building
// ---------------------------------------------------------------------------

// Local layout of `t(IJFD)J`. Every slot is initialized in the prologue, before
// the first branch, so one declared frame layout is valid at every label.
const A: usize = 0;
const B: usize = 1;
const F: usize = 3;
const D: usize = 4;
const ACC: usize = 6;
const I: usize = 8;
const J: usize = 9;
const K: usize = 10;
const Y: usize = 11;
const O: usize = 13;
const IA: usize = 14;
const IB: usize = 15;
const DA: usize = 16;
const G: usize = 17;
const Z: usize = 18;
const X: usize = 20;
/// The high `wide` slot, present only when a [`Category::WideLocals`] snippet is.
const WIDE_HI: usize = 300;

fn obj_t() -> VType {
    VType::obj("java/lang/Object")
}

fn declared_locals(wide_hi: bool) -> Vec<VType> {
    let mut l = vec![
        Int,                   // 0 a
        Long,                  // 1 b
        VType::Top,            // 2
        Float,                 // 3 f
        Double,                // 4 d
        VType::Top,            // 5
        Long,                  // 6 acc
        VType::Top,            // 7
        Int,                   // 8 i
        Int,                   // 9 j
        Int,                   // 10 k
        Long,                  // 11 y
        VType::Top,            // 12
        obj_t(),               // 13 o
        VType::obj("[I"),      // 14 ia
        VType::obj("[I"),      // 15 ib
        VType::obj("[D"),      // 16 da
        Int,                   // 17 g
        Double,                // 18 z
        VType::Top,            // 19
        Float,                 // 20 x
    ];
    debug_assert_eq!(l.len(), X + 1);
    if wide_hi {
        l.resize(WIDE_HI, VType::Top);
        l.push(Int);
    }
    l
}

/// Emission context: the assembler plus the declared frame layout.
struct Em<'cp> {
    a: Asm<'cp>,
    locals: Vec<VType>,
}

impl Em<'_> {
    fn frame(&self, stack: &[VType]) -> Frame {
        Frame {
            locals: self.locals.clone(),
            stack: stack.to_vec(),
        }
    }
    fn label(&mut self) -> Label {
        self.a.new_label()
    }
    fn bind(&mut self, l: Label, stack: &[VType]) {
        let f = self.frame(stack);
        self.a.bind(l, f);
    }
    fn ld(&mut self, t: VType, slot: usize) {
        self.a.load(t, slot, false);
    }
    fn st(&mut self, t: VType, slot: usize) {
        self.a.store(t, slot, false);
    }
    fn aload(&mut self, slot: usize) {
        self.a.load(obj_t(), slot, false);
    }
    fn astore(&mut self, slot: usize) {
        self.a.store(obj_t(), slot, false);
    }
    fn ib(&mut self, o: u8) {
        self.a.simple(o, &[Int, Int], &[Int]);
    }
    fn lb(&mut self, o: u8) {
        self.a.simple(o, &[Long, Long], &[Long]);
    }
    fn lsh(&mut self, o: u8) {
        self.a.simple(o, &[Long, Int], &[Long]);
    }
    fn cv(&mut self, o: u8, from: VType, to: VType) {
        self.a.simple(o, &[from], &[to]);
    }
    fn i2l(&mut self) {
        self.cv(op::I2L, Int, Long);
    }
    fn l2i(&mut self) {
        self.cv(op::L2I, Long, Int);
    }
    /// `Float.floatToIntBits` (NaN-canonicalizing: a JIT may legally produce a
    /// different NaN payload than the interpreter), widened to `long`.
    fn fbits(&mut self) {
        self.a
            .invokestatic("java/lang/Float", "floatToIntBits", "(F)I");
        self.i2l();
    }
    /// `Double.doubleToLongBits` (NaN-canonicalizing).
    fn dbits(&mut self) {
        self.a
            .invokestatic("java/lang/Double", "doubleToLongBits", "(D)J");
    }
    fn iaload(&mut self) {
        self.a.simple(op::IALOAD, &[VType::obj("[I"), Int], &[Int]);
    }
    fn iastore(&mut self) {
        self.a.simple(op::IASTORE, &[VType::obj("[I"), Int, Int], &[]);
    }
    fn daload(&mut self) {
        self.a.simple(op::DALOAD, &[VType::obj("[D"), Int], &[Double]);
    }
    fn dastore(&mut self) {
        self.a
            .simple(op::DASTORE, &[VType::obj("[D"), Int, Double], &[]);
    }
}

/// Assemble `t` for `p`.
pub fn build_test_method(cp: &mut ConstantPool, p: &JitProgram) -> Result<CodeBody, String> {
    let wide_hi = p
        .snippets
        .iter()
        .any(|s| s.category == Category::WideLocals);
    let locals = declared_locals(wide_hi);
    let max_locals = locals.len();
    let mut em = Em {
        a: Asm::new_static(cp, TEST_DESC, max_locals),
        locals,
    };

    // Prologue: initialize every declared slot.
    em.a.lconst(0);
    em.st(Long, ACC);
    for slot in [I, J, K, G] {
        em.a.iconst(0);
        em.st(Int, slot);
    }
    em.a.lconst(0);
    em.st(Long, Y);
    em.a.aconst_null();
    em.astore(O);
    for (slot, atype) in [(IA, op::T_INT), (IB, op::T_INT), (DA, op::T_DOUBLE)] {
        em.a.iconst(8);
        em.a.newarray(atype);
        em.astore(slot);
    }
    em.a.dconst(0.0);
    em.st(Double, Z);
    em.a.fconst(0.0);
    em.st(Float, X);
    if wide_hi {
        em.a.iconst(0);
        em.st(Int, WIDE_HI);
    }

    for s in &p.snippets {
        let mut r = Rng::new(splitmix64(s.seed));
        emit_snippet(&mut em, s.category, &mut r, wide_hi);
        let st = em.a.stack();
        if st.len() != 1 || st[0] != Long {
            return Err(format!(
                "snippet {} left {:?} on the stack",
                s.category.label(),
                em.a.stack()
            ));
        }
        // Fold: acc = (acc ^ y) * FNV_PRIME.
        em.st(Long, Y);
        em.ld(Long, ACC);
        em.ld(Long, Y);
        em.lb(op::LXOR);
        em.a.lconst(0x0000_0100_0000_01B3);
        em.lb(op::LMUL);
        em.st(Long, ACC);
    }
    em.ld(Long, ACC);
    em.a.ret();
    em.a.finish()
}

fn emit_snippet(em: &mut Em, c: Category, r: &mut Rng, wide_hi: bool) {
    match c {
        Category::DivRemOverflow => div_rem_overflow(em, r),
        Category::DivByZeroCaught => div_by_zero_caught(em, r),
        Category::ShiftCounts => shift_counts(em, r),
        Category::FpCompare => fp_compare(em, r),
        Category::FpToInt => fp_to_int(em, r),
        Category::FpRem => fp_rem(em, r),
        Category::IntNarrowing => int_narrowing(em, r),
        Category::Switches => switches(em, r),
        Category::LoopBounds => loop_bounds(em, r),
        Category::ArrayLongSum => array_long_sum(em, r),
        Category::DoubleSumOrder => double_sum_order(em, r),
        Category::ArrayAlias => array_alias(em, r),
        Category::Ternary => ternary(em, r),
        Category::HandlerNullJoin => handler_null_join(em, r),
        Category::StackShuffles => stack_shuffles(em, r),
        Category::WideLocals => wide_locals(em, r, wide_hi),
    }
}

fn div_rem_overflow(em: &mut Em, r: &mut Rng) {
    for n in 0..2 {
        match r.below(8) {
            0 => {
                em.a.iconst(i32::MIN);
                em.a.iconst(-1);
                em.ib(op::IDIV);
                em.i2l();
            }
            1 => {
                em.ld(Int, A);
                em.a.iconst(-1);
                em.ib(op::IDIV);
                em.i2l();
            }
            2 => {
                em.ld(Int, A);
                em.a.iconst(-1);
                em.ib(op::IREM);
                em.i2l();
            }
            3 => {
                em.ld(Long, B);
                em.a.lconst(-1);
                em.lb(op::LDIV);
            }
            4 => {
                em.ld(Long, B);
                em.a.lconst(-1);
                em.lb(op::LREM);
            }
            5 => {
                // MIN / (a | 1): a non-constant divisor that is -1 for a == -1.
                em.a.iconst(i32::MIN);
                em.ld(Int, A);
                em.a.iconst(1);
                em.ib(op::IOR);
                em.ib(op::IDIV);
                em.i2l();
            }
            6 => {
                em.a.lconst(i64::MIN);
                em.ld(Long, B);
                em.a.lconst(1);
                em.lb(op::LOR);
                em.lb(op::LREM);
            }
            _ => {
                em.ld(Int, A);
                em.cv(op::INEG, Int, Int);
                em.ld(Int, A);
                em.a.iconst(1);
                em.ib(op::IOR);
                em.ib(op::IDIV);
                em.i2l();
            }
        }
        if n == 1 {
            em.lb(op::LXOR);
        }
    }
}

fn div_by_zero_caught(em: &mut Em, r: &mut Rng) {
    let handler = em.label();
    let join = em.label();
    let t = em.a.try_begin(handler, "java/lang/ArithmeticException");
    match r.below(4) {
        0 => {
            em.ld(Int, A);
            em.ld(Long, B);
            em.l2i();
            em.ib(op::IDIV);
            em.i2l();
        }
        1 => {
            em.ld(Int, A);
            em.ld(Long, B);
            em.l2i();
            em.ib(op::IREM);
            em.i2l();
        }
        2 => {
            em.ld(Long, B);
            em.ld(Int, A);
            em.i2l();
            em.lb(op::LDIV);
        }
        _ => {
            em.ld(Long, B);
            em.ld(Int, A);
            em.i2l();
            em.lb(op::LREM);
        }
    }
    em.st(Long, Y);
    em.a.try_end(t);
    em.a.goto(join);
    em.bind(handler, &[VType::obj("java/lang/ArithmeticException")]);
    em.a.pop1();
    em.a.lconst(-0x5A5A_5A5A);
    em.st(Long, Y);
    em.bind(join, &[]);
    em.ld(Long, Y);
}

const SHIFT_COUNTS: &[i32] = &[-1, 0, 31, 32, 33, 63, 64];

fn shift_counts(em: &mut Em, r: &mut Rng) {
    for n in 0..2 {
        let c = pick(r, SHIFT_COUNTS);
        match r.below(8) {
            v @ 0..=2 => {
                em.ld(Int, A);
                em.a.iconst(c);
                em.ib([op::ISHL, op::ISHR, op::IUSHR][v]);
                em.i2l();
            }
            v @ 3..=5 => {
                em.ld(Long, B);
                em.a.iconst(c);
                em.lsh([op::LSHL, op::LSHR, op::LUSHR][v - 3]);
            }
            6 => {
                // Variable count straight from an input.
                em.ld(Long, B);
                em.ld(Int, A);
                let o = pick(r, &[op::LSHL, op::LSHR, op::LUSHR]);
                em.lsh(o);
            }
            _ => {
                let v = pick(r, &[1, -1, i32::MIN]);
                em.a.iconst(v);
                em.ld(Int, A);
                em.a.iconst(c);
                em.ib(op::IADD);
                let o = pick(r, &[op::ISHL, op::ISHR, op::IUSHR]);
                em.ib(o);
                em.i2l();
            }
        }
        if n == 1 {
            em.lb(op::LXOR);
        }
    }
}

const FCMP_CONSTS: &[f32] = &[f32::NAN, -0.0, 0.0, 1.5, f32::INFINITY];
const DCMP_CONSTS: &[f64] = &[f64::NAN, -0.0, 0.0, 1.5, f64::NEG_INFINITY];

/// Push an `fcmp*`/`dcmp*` result (an int).
fn push_fp_compare(em: &mut Em, r: &mut Rng) {
    let reversed = r.below(3) == 0;
    if r.below(2) == 0 {
        let k = pick(r, FCMP_CONSTS);
        if reversed {
            em.a.fconst(k);
            em.ld(Float, F);
        } else {
            em.ld(Float, F);
            em.a.fconst(k);
        }
        let o = pick(r, &[op::FCMPL, op::FCMPG]);
        em.a.simple(o, &[Float, Float], &[Int]);
    } else {
        let k = pick(r, DCMP_CONSTS);
        if reversed {
            em.a.dconst(k);
            em.ld(Double, D);
        } else {
            em.ld(Double, D);
            em.a.dconst(k);
        }
        let o = pick(r, &[op::DCMPL, op::DCMPG]);
        em.a.simple(o, &[Double, Double], &[Int]);
    }
}

fn fp_compare(em: &mut Em, r: &mut Rng) {
    for n in 0..2 {
        if r.below(2) == 0 {
            push_fp_compare(em, r);
            em.i2l();
        } else {
            // Compare fused into a branch — the form instruction selection
            // rewrites into flags tests, where the NaN polarity goes wrong.
            push_fp_compare(em, r);
            let other = em.label();
            let join = em.label();
            let cond = pick(
                r,
                &[op::IFEQ, op::IFNE, op::IFLT, op::IFGE, op::IFGT, op::IFLE],
            );
            em.a.if_int(cond, other);
            em.a.lconst(0x11);
            em.a.goto(join);
            em.bind(other, &[]);
            em.a.lconst(0x22);
            em.bind(join, &[Long]);
        }
        if n == 1 {
            em.lb(op::LXOR);
        }
    }
}

const F2X_CONSTS: &[f32] = &[
    f32::NAN,
    f32::INFINITY,
    f32::NEG_INFINITY,
    3.0e9,
    -3.0e9,
    9.3e18,
    -9.3e18,
    2_147_483_647.0,
    -2_147_483_648.0,
    0.99,
];
const D2X_CONSTS: &[f64] = &[
    f64::NAN,
    f64::INFINITY,
    f64::NEG_INFINITY,
    1e19,
    -1e19,
    2_147_483_648.0,
    -2_147_483_649.0,
    9.223_372_036_854_775_807e18,
    -0.5,
];

fn fp_to_int(em: &mut Em, r: &mut Rng) {
    for n in 0..2 {
        match r.below(7) {
            0 => {
                em.ld(Float, F);
                em.cv(op::F2I, Float, Int);
                em.i2l();
            }
            1 => {
                em.ld(Float, F);
                em.cv(op::F2L, Float, Long);
            }
            2 => {
                em.ld(Double, D);
                em.cv(op::D2I, Double, Int);
                em.i2l();
            }
            3 => {
                em.ld(Double, D);
                em.cv(op::D2L, Double, Long);
            }
            4 => {
                let k = pick(r, F2X_CONSTS);
                em.a.fconst(k);
                if r.below(2) == 0 {
                    em.cv(op::F2I, Float, Int);
                    em.i2l();
                } else {
                    em.cv(op::F2L, Float, Long);
                }
            }
            5 => {
                let k = pick(r, D2X_CONSTS);
                em.a.dconst(k);
                if r.below(2) == 0 {
                    em.cv(op::D2I, Double, Int);
                    em.i2l();
                } else {
                    em.cv(op::D2L, Double, Long);
                }
            }
            _ => {
                em.ld(Double, D);
                em.cv(op::D2F, Double, Float);
                em.cv(op::F2I, Float, Int);
                em.i2l();
            }
        }
        if n == 1 {
            em.lb(op::LXOR);
        }
    }
}

fn fp_rem(em: &mut Em, r: &mut Rng) {
    for n in 0..2 {
        match r.below(4) {
            v @ 0..=1 => {
                let k = pick(r, &[0.0f32, -0.0, 1.5, -2.5, f32::INFINITY, f32::NAN, 1.0e-45, 3.0]);
                if v == 0 {
                    em.ld(Float, F);
                    em.a.fconst(k);
                } else {
                    em.a.fconst(k);
                    em.ld(Float, F);
                }
                em.a.simple(op::FREM, &[Float, Float], &[Float]);
                em.fbits();
            }
            v => {
                let k = pick(r, &[0.0f64, -0.0, 1.5, -2.5, f64::INFINITY, f64::NAN, 1e-300, 3.0]);
                if v == 2 {
                    em.ld(Double, D);
                    em.a.dconst(k);
                } else {
                    em.a.dconst(k);
                    em.ld(Double, D);
                }
                em.a.simple(op::DREM, &[Double, Double], &[Double]);
                em.dbits();
            }
        }
        if n == 1 {
            em.lb(op::LXOR);
        }
    }
}

fn int_narrowing(em: &mut Em, r: &mut Rng) {
    const NARROW: &[u8] = &[op::I2B, op::I2C, op::I2S];
    for n in 0..2 {
        match r.below(6) {
            0 => {
                em.ld(Int, A);
                let o = pick(r, NARROW);
                em.cv(o, Int, Int);
            }
            1 => {
                em.ld(Int, A);
                let k = pick(r, &[200, 40000, 128, -129, 65535]);
                em.a.iconst(k);
                em.ib(op::IADD);
                let o = pick(r, NARROW);
                em.cv(o, Int, Int);
            }
            2 => {
                em.ld(Long, B);
                em.l2i();
                let o = pick(r, NARROW);
                em.cv(o, Int, Int);
            }
            3 => {
                em.ld(Int, A);
                em.cv(op::I2C, Int, Int);
                em.cv(op::I2S, Int, Int);
            }
            4 => {
                em.ld(Int, A);
                em.cv(op::I2B, Int, Int);
                em.cv(op::I2C, Int, Int);
            }
            _ => {
                em.ld(Int, A);
                let o1 = pick(r, NARROW);
                em.cv(o1, Int, Int);
                em.ld(Int, A);
                let o2 = pick(r, NARROW);
                em.cv(o2, Int, Int);
                em.ib(op::IMUL);
            }
        }
        em.i2l();
        if n == 1 {
            em.lb(op::LXOR);
        }
    }
}

fn switches(em: &mut Em, r: &mut Rng) {
    let join = em.label();
    let default = em.label();
    let mut cases: Vec<Label> = Vec::new();
    if r.below(2) == 0 {
        let (low, n) = pick(
            r,
            &[(-1, 4usize), (0, 3), (i32::MAX - 2, 3), (i32::MIN, 3), (30, 4)],
        );
        em.ld(Int, A);
        if r.below(2) == 0 {
            let k = pick(r, &[1, -1, i32::MIN]);
            em.a.iconst(k);
            em.ib(op::IADD);
        }
        cases = (0..n).map(|_| em.label()).collect();
        em.a.tableswitch(low, default, &cases);
    } else {
        const KEYS: &[i32] = &[i32::MIN, -1, 0, 1, 31, 32, 64, i32::MAX];
        let mut keys: Vec<i32> = KEYS.iter().copied().filter(|_| r.below(2) == 0).collect();
        if keys.is_empty() {
            keys.push(0);
        }
        if r.below(2) == 0 {
            em.ld(Int, A);
        } else {
            em.ld(Long, B);
            em.l2i();
        }
        cases = keys.iter().map(|_| em.label()).collect();
        let pairs: Vec<(i32, Label)> = keys.iter().copied().zip(cases.iter().copied()).collect();
        em.a.lookupswitch(default, &pairs);
    }
    for (i, &c) in cases.iter().enumerate() {
        em.bind(c, &[]);
        em.a.lconst(1000 + i as i64);
        em.a.goto(join);
    }
    em.bind(default, &[]);
    em.ld(Int, A);
    em.i2l();
    em.a.lconst(-1);
    em.lb(op::LXOR);
    em.bind(join, &[Long]);
}

fn loop_body(em: &mut Em) {
    em.ld(Long, Y);
    em.a.lconst(31);
    em.lb(op::LMUL);
    em.ld(Int, I);
    em.i2l();
    em.lb(op::LADD);
    em.st(Long, Y);
}

/// Bound every loop that could otherwise run ~2^32 times.
fn loop_guard(em: &mut Em, exit: Label) {
    em.a.iinc(G, 1, false);
    em.ld(Int, G);
    em.a.iconst(64);
    em.a.if_icmp(op::IF_ICMPGE, exit);
}

fn loop_bounds(em: &mut Em, r: &mut Rng) {
    let head = em.label();
    let exit = em.label();
    let variant = r.below(5);
    let start = match variant {
        0 => pick(r, &[0, 5, 100, i32::MAX - 3, i32::MIN]),
        1 => pick(r, &[i32::MAX - 5, 0, -3, i32::MIN]),
        2 => pick(r, &[0, 7, 40]),
        3 => pick(r, &[10, 50, i32::MAX - 1]),
        _ => pick(r, &[i32::MIN + 3, 3, -1]),
    };
    em.a.iconst(start);
    em.st(Int, I);
    em.a.iconst(0);
    em.st(Int, G);
    em.a.lconst(0);
    em.st(Long, Y);
    match variant {
        0 => {
            // for (i = start; i < a; i++) — entered with i > n when a is small.
            em.bind(head, &[]);
            em.ld(Int, I);
            em.ld(Int, A);
            em.a.if_icmp(op::IF_ICMPGE, exit);
            loop_body(em);
            loop_guard(em, exit);
            em.a.iinc(I, 1, false);
            em.a.goto(head);
        }
        1 => {
            // for (i = start; i <= n; i++) with n possibly Integer.MAX_VALUE:
            // the induction variable wraps, so the trip count is not n-start+1.
            em.bind(head, &[]);
            em.ld(Int, I);
            if r.below(2) == 0 {
                em.ld(Int, A);
            } else {
                em.a.iconst(i32::MAX);
            }
            em.a.if_icmp(op::IF_ICMPGT, exit);
            loop_body(em);
            loop_guard(em, exit);
            em.a.iinc(I, 1, false);
            em.a.goto(head);
        }
        2 => {
            // A pure counted loop (no guard): 0..=31 trips, zero-trip when
            // start > (a & 31).
            let step = pick(r, &[1, 2, 3]);
            em.bind(head, &[]);
            em.ld(Int, I);
            em.ld(Int, A);
            em.a.iconst(31);
            em.ib(op::IAND);
            em.a.if_icmp(op::IF_ICMPGE, exit);
            loop_body(em);
            em.a.iinc(I, step, false);
            em.a.goto(head);
        }
        3 => {
            // do { } while (i < (a & 15)) entered with i > n: exactly one trip.
            em.bind(head, &[]);
            loop_body(em);
            loop_guard(em, exit);
            em.a.iinc(I, 1, false);
            em.ld(Int, I);
            em.ld(Int, A);
            em.a.iconst(15);
            em.ib(op::IAND);
            em.a.if_icmp(op::IF_ICMPLT, head);
        }
        _ => {
            // for (i = start; i >= n; i--) — wraps below MIN_VALUE.
            em.bind(head, &[]);
            em.ld(Int, I);
            if r.below(2) == 0 {
                em.ld(Int, A);
            } else {
                em.a.iconst(i32::MIN);
            }
            em.a.if_icmp(op::IF_ICMPLT, exit);
            loop_body(em);
            loop_guard(em, exit);
            em.a.iinc(I, -1, false);
            em.a.goto(head);
        }
    }
    em.bind(exit, &[]);
    // The exit values of the induction and guard variables are observable too.
    em.ld(Long, Y);
    em.ld(Int, I);
    em.i2l();
    em.lb(op::LXOR);
    em.ld(Int, G);
    em.i2l();
    em.lb(op::LADD);
}

/// `for (k = 0; k < 8; k++) ia[k] = xor ? a ^ k : a + k * c;`
fn fill_ia(em: &mut Em, c: i32, xor: bool) {
    let head = em.label();
    let done = em.label();
    em.a.iconst(0);
    em.st(Int, K);
    em.bind(head, &[]);
    em.ld(Int, K);
    em.a.iconst(8);
    em.a.if_icmp(op::IF_ICMPGE, done);
    em.aload(IA);
    em.ld(Int, K);
    em.ld(Int, A);
    em.ld(Int, K);
    if xor {
        em.ib(op::IXOR);
    } else {
        em.a.iconst(c);
        em.ib(op::IMUL);
        em.ib(op::IADD);
    }
    em.iastore();
    em.a.iinc(K, 1, false);
    em.a.goto(head);
    em.bind(done, &[]);
}

/// Leaves `sum(ia)` as a `long` (or, with `int_acc`, the wrapped `int` sum
/// widened afterwards).
fn sum_ia(em: &mut Em, int_acc: bool) {
    let head = em.label();
    let done = em.label();
    if int_acc {
        em.a.iconst(0);
        em.st(Int, J);
    } else {
        em.a.lconst(0);
        em.st(Long, Y);
    }
    em.a.iconst(0);
    em.st(Int, K);
    em.bind(head, &[]);
    em.ld(Int, K);
    em.aload(IA);
    em.a.arraylength();
    em.a.if_icmp(op::IF_ICMPGE, done);
    if int_acc {
        em.ld(Int, J);
        em.aload(IA);
        em.ld(Int, K);
        em.iaload();
        em.ib(op::IADD);
        em.st(Int, J);
    } else {
        em.ld(Long, Y);
        em.aload(IA);
        em.ld(Int, K);
        em.iaload();
        em.i2l();
        em.lb(op::LADD);
        em.st(Long, Y);
    }
    em.a.iinc(K, 1, false);
    em.a.goto(head);
    em.bind(done, &[]);
    if int_acc {
        em.ld(Int, J);
        em.i2l();
    } else {
        em.ld(Long, Y);
    }
}

fn array_long_sum(em: &mut Em, r: &mut Rng) {
    let c = pick(r, &[0x3FFF_FFFF, 0x7FFF_FFFF, 0x4000_0001, -0x7FFF_FFFF]);
    fill_ia(em, c, false);
    sum_ia(em, r.below(3) == 0);
}

fn double_sum_order(em: &mut Em, r: &mut Rng) {
    const ORDERS: [[f64; 4]; 3] = [
        [1e16, 1.0, -1e16, 1.0],
        [1.0, 1e16, 1.0, -1e16],
        [1e16, -1e16, 1.0, 1.0],
    ];
    let order = ORDERS[r.below(ORDERS.len())];
    for (i, v) in order.iter().enumerate() {
        em.aload(DA);
        em.a.iconst(i as i32);
        em.a.dconst(*v);
        em.dastore();
    }
    if r.below(2) == 0 {
        em.aload(DA);
        em.a.iconst(4);
        em.ld(Double, D);
        em.dastore();
    }
    let head = em.label();
    let done = em.label();
    em.a.dconst(0.0);
    em.st(Double, Z);
    let reverse = r.below(2) == 0;
    em.a.iconst(if reverse { 7 } else { 0 });
    em.st(Int, K);
    em.bind(head, &[]);
    em.ld(Int, K);
    if reverse {
        em.a.if_int(op::IFLT, done);
    } else {
        em.aload(DA);
        em.a.arraylength();
        em.a.if_icmp(op::IF_ICMPGE, done);
    }
    em.ld(Double, Z);
    em.aload(DA);
    em.ld(Int, K);
    em.daload();
    em.a.simple(op::DADD, &[Double, Double], &[Double]);
    em.st(Double, Z);
    em.a.iinc(K, if reverse { -1 } else { 1 }, false);
    em.a.goto(head);
    em.bind(done, &[]);
    em.ld(Double, Z);
    em.dbits();
}

fn array_alias(em: &mut Em, r: &mut Rng) {
    fill_ia(em, 0, true);
    if r.below(2) == 0 {
        em.aload(IA);
        em.astore(IB);
    } else {
        // Alias only when a < 0: the loop below must not assume either way.
        let other = em.label();
        let st = em.label();
        em.ld(Int, A);
        em.a.if_int(op::IFGE, other);
        em.aload(IA);
        em.a.goto(st);
        em.bind(other, &[]);
        em.aload(IB);
        em.bind(st, &[VType::obj("[I")]);
        em.astore(IB);
    }
    // for (k = 1; k < 8; k++) ia[k] = ib[k - 1] + c;
    let head = em.label();
    let done = em.label();
    let c = pick(r, &[3, -1, i32::MIN]);
    em.a.iconst(1);
    em.st(Int, K);
    em.bind(head, &[]);
    em.ld(Int, K);
    em.a.iconst(8);
    em.a.if_icmp(op::IF_ICMPGE, done);
    em.aload(IA);
    em.ld(Int, K);
    em.aload(IB);
    em.ld(Int, K);
    em.a.iconst(1);
    em.ib(op::ISUB);
    em.iaload();
    em.a.iconst(c);
    em.ib(op::IADD);
    em.iastore();
    em.a.iinc(K, 1, false);
    em.a.goto(head);
    em.bind(done, &[]);
    sum_ia(em, false);
    em.aload(IB);
    em.a.iconst(7);
    em.iaload();
    em.i2l();
    em.lb(op::LXOR);
}

fn ternary(em: &mut Em, r: &mut Rng) {
    match r.below(3) {
        0 => {
            // (c && a < (int) b) ? a : (int) b
            let els = em.label();
            let join = em.label();
            em.ld(Int, A);
            let mask = pick(r, &[1, 2, 0x100, -1]);
            em.a.iconst(mask);
            em.ib(op::IAND);
            em.a.if_int(op::IFEQ, els);
            em.ld(Int, A);
            em.ld(Long, B);
            em.l2i();
            em.a.if_icmp(op::IF_ICMPGE, els);
            em.ld(Int, A);
            em.a.goto(join);
            em.bind(els, &[]);
            em.ld(Long, B);
            em.l2i();
            em.bind(join, &[Int]);
            em.i2l();
        }
        1 => {
            // (a < 0 || a > (int) b) ? b - a : b * 3
            let tr = em.label();
            let els = em.label();
            let join = em.label();
            em.ld(Int, A);
            em.a.if_int(op::IFLT, tr);
            em.ld(Int, A);
            em.ld(Long, B);
            em.l2i();
            em.a.if_icmp(op::IF_ICMPLE, els);
            em.bind(tr, &[]);
            em.ld(Long, B);
            em.ld(Int, A);
            em.i2l();
            em.lb(op::LSUB);
            em.a.goto(join);
            em.bind(els, &[]);
            em.ld(Long, B);
            em.a.lconst(3);
            em.lb(op::LMUL);
            em.bind(join, &[Long]);
        }
        _ => {
            // for (k = 0; k < 16; k++) z = z * ((k & a) != 0 ? d : 2.0);
            let head = em.label();
            let exit = em.label();
            let two = em.label();
            let mul = em.label();
            em.a.dconst(1.0);
            em.st(Double, Z);
            em.a.iconst(0);
            em.st(Int, K);
            em.bind(head, &[]);
            em.ld(Int, K);
            em.a.iconst(16);
            em.a.if_icmp(op::IF_ICMPGE, exit);
            em.ld(Double, Z);
            em.ld(Int, K);
            em.ld(Int, A);
            em.ib(op::IAND);
            em.a.if_int(op::IFEQ, two);
            em.ld(Double, D);
            em.a.goto(mul);
            em.bind(two, &[Double]);
            em.a.dconst(2.0);
            em.bind(mul, &[Double, Double]);
            em.a.simple(op::DMUL, &[Double, Double], &[Double]);
            em.st(Double, Z);
            em.a.iinc(K, 1, false);
            em.a.goto(head);
            em.bind(exit, &[]);
            em.ld(Double, Z);
            em.dbits();
        }
    }
}

fn handler_null_join(em: &mut Em, r: &mut Rng) {
    // o = ia; try { j = a % (int) b; } catch (ArithmeticException e) { o = null; j = -1; }
    // join: o == null ? j - 1000 : ((int[]) o)[0] + j
    let handler = em.label();
    let join = em.label();
    let is_null = em.label();
    let out = em.label();
    em.aload(IA);
    em.astore(O);
    let t = em.a.try_begin(handler, "java/lang/ArithmeticException");
    em.ld(Int, A);
    em.ld(Long, B);
    em.l2i();
    em.ib(if r.below(2) == 0 { op::IREM } else { op::IDIV });
    em.st(Int, J);
    em.a.try_end(t);
    em.a.goto(join);
    em.bind(handler, &[VType::obj("java/lang/ArithmeticException")]);
    em.a.pop1();
    em.a.aconst_null();
    em.astore(O);
    em.a.iconst(-1);
    em.st(Int, J);
    em.bind(join, &[]);
    em.aload(O);
    em.a.if_ref(op::IFNULL, is_null);
    em.aload(O);
    em.a.checkcast("[I");
    em.a.iconst(0);
    em.iaload();
    em.ld(Int, J);
    em.ib(op::IADD);
    em.a.goto(out);
    em.bind(is_null, &[]);
    em.ld(Int, J);
    em.a.iconst(1000);
    em.ib(op::ISUB);
    em.bind(out, &[Int]);
    em.i2l();
}

/// One stack-shuffle sequence; leaves a `long`.
fn one_shuffle(em: &mut Em, which: usize) {
    match which {
        0 => {
            // dup_x2 form 1: I I I -> I I I I
            em.ld(Int, A);
            em.a.iconst(7);
            em.ld(Long, B);
            em.l2i();
            em.a.dup_x2();
            em.ib(op::ISUB);
            em.ib(op::IMUL);
            em.ib(op::IXOR);
            em.i2l();
        }
        1 => {
            // dup_x2 form 2: J I -> I J I
            em.ld(Long, B);
            em.ld(Int, A);
            em.a.dup_x2();
            em.i2l();
            em.lb(op::LSUB);
            em.l2i();
            em.ib(op::ISUB);
            em.i2l();
        }
        2 => {
            // dup2_x1 form 1: I I I -> I I I I I
            em.ld(Int, A);
            em.a.iconst(3);
            em.a.iconst(-9);
            em.a.dup2_x1();
            em.ib(op::ISUB);
            em.ib(op::IXOR);
            em.ib(op::IMUL);
            em.ib(op::IADD);
            em.i2l();
        }
        3 => {
            // dup2_x1 form 2: I J -> J I J
            em.ld(Int, A);
            em.ld(Long, B);
            em.a.dup2_x1();
            em.l2i();
            em.ib(op::IADD);
            em.i2l();
            em.lb(op::LXOR);
        }
        4 => {
            // dup2_x2 form 1: I I I I -> I I I I I I
            em.ld(Int, A);
            em.a.iconst(5);
            em.ld(Long, B);
            em.l2i();
            em.a.iconst(-3);
            em.a.dup2_x2();
            em.ib(op::ISUB);
            em.ib(op::IXOR);
            em.ib(op::IMUL);
            em.ib(op::IADD);
            em.ib(op::ISUB);
            em.i2l();
        }
        5 => {
            // dup2_x2 form 2: I I J -> J I I J
            em.ld(Int, A);
            em.a.iconst(5);
            em.ld(Long, B);
            em.a.dup2_x2();
            em.l2i();
            em.ib(op::ISUB);
            em.ib(op::IMUL);
            em.i2l();
            em.lb(op::LADD);
        }
        6 => {
            // dup2_x2 form 3: J I I -> I I J I I
            em.ld(Long, B);
            em.ld(Int, A);
            em.a.iconst(11);
            em.a.dup2_x2();
            em.ib(op::ISUB);
            em.i2l();
            em.lb(op::LMUL);
            em.l2i();
            em.ib(op::IXOR);
            em.ib(op::IADD);
            em.i2l();
        }
        7 => {
            // dup2_x2 form 4: J J -> J J J
            em.ld(Long, B);
            em.ld(Int, A);
            em.i2l();
            em.a.dup2_x2();
            em.lb(op::LSUB);
            em.lb(op::LXOR);
        }
        8 => {
            em.ld(Int, A);
            em.ld(Long, B);
            em.l2i();
            em.a.swap();
            em.ib(op::ISUB);
            em.i2l();
        }
        9 => {
            // pop2 form 1
            em.ld(Int, A);
            em.a.iconst(1);
            em.a.iconst(2);
            em.a.pop2();
            em.i2l();
        }
        10 => {
            // pop2 form 2
            em.ld(Int, A);
            em.ld(Long, B);
            em.a.pop2();
            em.i2l();
            em.ld(Long, B);
            em.lb(op::LSUB);
        }
        _ => {
            // dup_x1, then dup2 on a long
            em.ld(Int, A);
            em.a.iconst(13);
            em.a.dup_x1();
            em.ib(op::IMUL);
            em.ib(op::ISUB);
            em.i2l();
            em.a.dup2();
            em.lb(op::LMUL);
        }
    }
}

fn stack_shuffles(em: &mut Em, r: &mut Rng) {
    let steps = 2 + r.below(3);
    for n in 0..steps {
        let which = r.below(12);
        one_shuffle(em, which);
        if n > 0 {
            em.lb(op::LXOR);
        }
    }
}

fn wide_locals(em: &mut Em, r: &mut Rng, wide_hi: bool) {
    if wide_hi {
        // A loop whose induction lives above slot 255, stepped by `wide iinc`.
        let head = em.label();
        let exit = em.label();
        em.ld(Int, A);
        em.st(Int, WIDE_HI); // index > 255 ⇒ `wide istore`
        let delta = pick(r, &[-30000, 3, 1000]);
        em.a.iinc(WIDE_HI, delta, false);
        em.a.iconst(0);
        em.st(Int, G);
        em.bind(head, &[]);
        em.ld(Int, G);
        em.a.iconst(8);
        em.a.if_icmp(op::IF_ICMPGE, exit);
        em.a.iinc(WIDE_HI, 3, true);
        em.a.iinc(G, 1, true);
        em.a.goto(head);
        em.bind(exit, &[]);
    }
    // `wide` on small indices: legal, and a separate decoder path.
    em.a.load(Int, A, true);
    em.a.store(Int, J, true);
    let delta = pick(r, &[1000, -30000, 7]);
    em.a.iinc(J, delta, true);
    em.a.load(Long, B, true);
    em.a.store(Long, Y, true);
    em.a.load(Long, Y, true);
    em.a.load(Int, J, true);
    em.i2l();
    em.lb(op::LXOR);
    em.a.load(Double, D, true);
    em.a.store(Double, Z, true);
    em.a.load(Double, Z, true);
    em.cv(op::D2L, Double, Long);
    em.lb(op::LADD);
    em.a.load(Float, F, true);
    em.a.store(Float, X, true);
    em.a.load(Float, X, true);
    em.cv(op::F2L, Float, Long);
    em.lb(op::LXOR);
    if wide_hi {
        em.ld(Int, WIDE_HI);
        em.i2l();
        em.lb(op::LADD);
    }
}

/// Push `t`'s four arguments for `inp`; `round_slot` is `main`'s loop counter.
fn push_args(em: &mut Em, inp: &Input, round_slot: usize) {
    em.a.iconst(inp.a);
    if inp.vary {
        em.ld(Int, round_slot);
        em.a.iconst(3);
        em.ib(op::IAND);
        em.ib(op::IXOR);
    }
    em.a.lconst(inp.b);
    em.a.fconst(inp.f);
    em.a.dconst(inp.d);
}

/// Assemble `main` for `p`.
pub fn build_main_method(cp: &mut ConstantPool, p: &JitProgram) -> Result<CodeBody, String> {
    const ARGS: usize = 0;
    const ROUND: usize = 1;
    const MACC: usize = 2;
    let locals = vec![VType::obj("[Ljava/lang/String;"), Int, Long, VType::Top];
    let mut em = Em {
        a: Asm::new_static(cp, "([Ljava/lang/String;)V", locals.len()),
        locals,
    };
    let _ = ARGS;
    em.a.iconst(0);
    em.st(Int, ROUND);
    em.a.lconst(0);
    em.st(Long, MACC);

    // One line per input, for triage.
    for inp in &p.inputs {
        em.a.getstatic("java/lang/System", "out", "Ljava/io/PrintStream;");
        push_args(&mut em, inp, ROUND);
        em.a.invokestatic(&p.name, "t", TEST_DESC);
        em.a.invokevirtual("java/io/PrintStream", "println", "(J)V");
    }

    let head = em.label();
    let end = em.label();
    em.bind(head, &[]);
    em.ld(Int, ROUND);
    em.a.iconst(p.rounds.min(i32::MAX as u32) as i32);
    em.a.if_icmp(op::IF_ICMPGE, end);
    for inp in &p.inputs {
        em.ld(Long, MACC);
        em.a.lconst(31);
        em.lb(op::LMUL);
        push_args(&mut em, inp, ROUND);
        em.a.invokestatic(&p.name, "t", TEST_DESC);
        em.lb(op::LADD);
        em.st(Long, MACC);
    }
    em.a.iinc(ROUND, 1, false);
    em.a.goto(head);
    em.bind(end, &[]);
    em.a.getstatic("java/lang/System", "out", "Ljava/io/PrintStream;");
    em.a.sconst(&format!("{} {CHECKSUM_NAME} ", crate::checksum::MARKER));
    em.a
        .invokevirtual("java/io/PrintStream", "print", "(Ljava/lang/String;)V");
    em.a.getstatic("java/lang/System", "out", "Ljava/io/PrintStream;");
    em.ld(Long, MACC);
    em.a.invokevirtual("java/io/PrintStream", "println", "(J)V");
    em.a.ret();
    em.a.finish()
}

/// Build the class file for `p`.
pub fn build_class(p: &JitProgram, version: ClassVersion) -> Result<Vec<u8>, String> {
    let mut cp = ConstantPool::new();
    let t = build_test_method(&mut cp, p).map_err(|e| format!("{}.t: {e}", p.name))?;
    let main = build_main_method(&mut cp, p).map_err(|e| format!("{}.main: {e}", p.name))?;
    Ok(classgen::write_class(
        cp,
        &p.name,
        vec![
            MethodDef {
                name: "t".into(),
                descriptor: TEST_DESC.into(),
                body: t,
            },
            MethodDef {
                name: "main".into(),
                descriptor: "([Ljava/lang/String;)V".into(),
                body: main,
            },
        ],
        version,
    ))
}

/// A human-readable description of a program: categories, inputs, and the
/// disassembly of `t`.
pub fn describe(p: &JitProgram) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "program {} (seed {}, rounds {})", p.name, p.seed, p.rounds);
    for (i, sn) in p.snippets.iter().enumerate() {
        let _ = writeln!(s, "  snippet {i}: {} (seed {:#018x})", sn.category.label(), sn.seed);
    }
    for (i, inp) in p.inputs.iter().enumerate() {
        let _ = writeln!(
            s,
            "  input {i}: a={} b={} f={:?} (bits {:#010x}) d={:?} (bits {:#018x}) vary={}",
            inp.a,
            inp.b,
            inp.f,
            inp.f.to_bits(),
            inp.d,
            inp.d.to_bits(),
            inp.vary
        );
    }
    match build_test_method(&mut ConstantPool::new(), p) {
        Ok(body) => {
            let _ = writeln!(s, "--- t{TEST_DESC} ---");
            s.push_str(&classgen::disassemble(&body.code));
        }
        Err(e) => {
            let _ = writeln!(s, "(t does not assemble: {e})");
        }
    }
    s
}

/// Shrink `p` while `still_fails` holds: drop snippets one at a time, then
/// inputs, then halve `rounds`. Greedy rather than ddmin because every probe
/// costs VM runs and programs have at most a handful of each.
pub fn minimize_program(p: &JitProgram, mut still_fails: impl FnMut(&JitProgram) -> bool) -> JitProgram {
    let mut cur = p.clone();
    let mut i = 0usize;
    while cur.snippets.len() > 1 && i < cur.snippets.len() {
        let mut cand = cur.clone();
        cand.snippets.remove(i);
        if still_fails(&cand) {
            cur = cand;
        } else {
            i += 1;
        }
    }
    i = 0;
    while cur.inputs.len() > 1 && i < cur.inputs.len() {
        let mut cand = cur.clone();
        cand.inputs.remove(i);
        if still_fails(&cand) {
            cur = cand;
        } else {
            i += 1;
        }
    }
    while cur.rounds > 1 {
        let mut cand = cur.clone();
        cand.rounds = cur.rounds / 2;
        if still_fails(&cand) {
            cur = cand;
        } else {
            break;
        }
    }
    cur
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

/// How to run the fuzzer.
#[derive(Debug, Clone)]
pub struct FuzzConfig {
    /// Modes to compare against `reference` (the reference itself is skipped).
    pub modes: Vec<Mode>,
    /// The oracle mode; `nojit` unless there is a reason otherwise.
    pub reference: Mode,
    pub timeout: Duration,
    pub rounds: u32,
    pub version: ClassVersion,
    /// Extra `KEY=VALUE` env layered on every **non-reference** mode.
    pub extra_env: Vec<(String, String)>,
    /// Shrink each failing program before writing it out.
    pub minimize: bool,
    /// Treat [`crate::pathgate::IR_VERIFY_REJECT_MARKER`] on a mode's stderr
    /// as a failure (needs `CRATONVM_DBG_IR_COMPILES=1` in the environment).
    pub fail_on_ir_verify_reject: bool,
    /// Also run HotSpot and compare it with the reference (validates the
    /// generator and the interpreter; needs `java`).
    pub hotspot: bool,
    pub jdk_home: Option<PathBuf>,
    /// Where minimized failing classes and reports go.
    pub out_dir: PathBuf,
    /// Scratch directory for class files under test.
    pub work_dir: PathBuf,
}

/// One mode disagreeing with the reference.
#[derive(Debug, Clone)]
pub struct ModeSplit {
    pub mode: Mode,
    pub diffs: Vec<ChannelDiff>,
    /// The IR verifier rejected a method during this run.
    pub ir_verify_reject: bool,
    /// The divergence did not reproduce on an immediate re-run.
    pub flaky: bool,
    pub observation: Observation,
}

/// Everything learned about one program.
#[derive(Debug, Clone)]
pub struct ProgramReport {
    pub program: JitProgram,
    /// The generator or assembler rejected the program (a generator bug).
    pub build_error: Option<String>,
    /// The reference run did not complete cleanly, so nothing can be judged.
    pub invalid_reference: Option<String>,
    pub reference: Option<Observation>,
    pub splits: Vec<ModeSplit>,
    /// Reference-vs-HotSpot differences, when `--hotspot` was requested.
    pub hotspot_diffs: Vec<ChannelDiff>,
}

impl ProgramReport {
    pub fn is_failure(&self) -> bool {
        self.build_error.is_some()
            || self.invalid_reference.is_some()
            || !self.splits.is_empty()
            || !self.hotspot_diffs.is_empty()
    }
}

fn io_err(e: std::io::Error) -> RunError {
    RunError::Io(e.to_string())
}

/// Why a reference run cannot serve as the oracle, if it cannot.
fn reference_problem(o: &Observation) -> Option<String> {
    if o.timed_out {
        return Some("reference timed out".into());
    }
    if o.exit_code != Some(0) {
        return Some(format!(
            "reference exited {:?}: {}",
            o.exit_code,
            o.stderr.lines().rev().take(5).collect::<Vec<_>>().join(" / ")
        ));
    }
    if let Some(e) = &o.exception {
        return Some(format!("reference threw {}: {}", e.fqcn, e.message));
    }
    if !oracle::checksums_of(o).contains_key(CHECKSUM_NAME) {
        return Some("reference printed no checksum line".into());
    }
    None
}

/// Write `p`'s class into its own scratch directory; returns the classpath.
fn stage(p: &JitProgram, cfg: &FuzzConfig) -> Result<Result<PathBuf, String>, RunError> {
    let bytes = match build_class(p, cfg.version) {
        Ok(b) => b,
        Err(e) => return Ok(Err(e)),
    };
    let dir = cfg.work_dir.join(&p.name);
    std::fs::create_dir_all(&dir).map_err(io_err)?;
    std::fs::write(dir.join(format!("{}.class", p.name)), bytes).map_err(io_err)?;
    Ok(Ok(dir))
}

fn env_for(mode: Mode, cfg: &FuzzConfig) -> &[(String, String)] {
    if mode == cfg.reference {
        &[]
    } else {
        &cfg.extra_env
    }
}

/// Run one program under the reference and every other mode.
pub fn run_program(bin: &Path, p: &JitProgram, cfg: &FuzzConfig) -> Result<ProgramReport, RunError> {
    let mut report = ProgramReport {
        program: p.clone(),
        build_error: None,
        invalid_reference: None,
        reference: None,
        splits: Vec::new(),
        hotspot_diffs: Vec::new(),
    };
    let dir = match stage(p, cfg)? {
        Ok(d) => d,
        Err(e) => {
            report.build_error = Some(e);
            return Ok(report);
        }
    };
    let reference = runner::run_cratonvm_with_env(bin, &dir, &p.name, cfg.reference, cfg.timeout, &[])?;
    if let Some(problem) = reference_problem(&reference) {
        report.invalid_reference = Some(problem);
        report.reference = Some(reference);
        return Ok(report);
    }
    if cfg.hotspot {
        let mut hs = runner::run_hotspot(&dir, &p.name, cfg.jdk_home.as_deref(), cfg.timeout)?;
        hs.exception = oracle::parse_exception(&hs.stderr);
        if let Verdict::Diverge(d) = oracle::compare(&reference, &hs, &Normalizer::strict()) {
            report.hotspot_diffs = d;
        }
    }
    for &mode in &cfg.modes {
        if mode == cfg.reference || !crate::crossmode::comparable(cfg.reference, mode) {
            continue;
        }
        let normalizer = Normalizer::for_mode(mode);
        let obs = runner::run_cratonvm_with_env(bin, &dir, &p.name, mode, cfg.timeout, env_for(mode, cfg))?;
        let reject = cfg.fail_on_ir_verify_reject
            && obs.stderr.contains(crate::pathgate::IR_VERIFY_REJECT_MARKER);
        match oracle::compare(&obs, &reference, &normalizer) {
            Verdict::Agree if !reject => {}
            Verdict::Agree => report.splits.push(ModeSplit {
                mode,
                diffs: Vec::new(),
                ir_verify_reject: true,
                flaky: false,
                observation: obs,
            }),
            Verdict::Diverge(diffs) => {
                let again =
                    runner::run_cratonvm_with_env(bin, &dir, &p.name, mode, cfg.timeout, env_for(mode, cfg))?;
                let flaky = oracle::compare(&again, &reference, &normalizer).agrees();
                report.splits.push(ModeSplit {
                    mode,
                    diffs,
                    ir_verify_reject: reject,
                    flaky,
                    observation: obs,
                });
            }
        }
    }
    report.reference = Some(reference);
    Ok(report)
}

/// Whether `cand` still makes any of `modes` disagree with the reference (or,
/// for a failure that was an invalid reference, still invalidates it).
fn still_fails(bin: &Path, cand: &JitProgram, modes: &[Mode], invalid_ref: bool, cfg: &FuzzConfig) -> bool {
    let narrowed = FuzzConfig {
        modes: modes.to_vec(),
        hotspot: false,
        ..cfg.clone()
    };
    match run_program(bin, cand, &narrowed) {
        Ok(r) if invalid_ref => r.invalid_reference.is_some(),
        // Only a reproducible split may steer the shrink, or a flake would
        // "minimize" the program down to something unrelated.
        Ok(r) => r.splits.iter().any(|s| !s.flaky),
        Err(_) => false,
    }
}

/// The command line that reproduces `mode` on a staged class.
pub fn repro_command(bin: &Path, dir: &Path, class: &str, mode: Mode, extra_env: &[(String, String)]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (k, v) in mode.env_overrides() {
        parts.push(format!("{k}={v}"));
    }
    for (k, v) in extra_env {
        parts.push(format!("{k}={v}"));
    }
    parts.push(bin.display().to_string());
    for a in mode.cli_args() {
        parts.push((*a).to_string());
    }
    parts.push("-cp".into());
    parts.push(dir.display().to_string());
    parts.push(class.to_string());
    parts.join(" ")
}

/// Render a failing program's report.
pub fn render_report(r: &ProgramReport, bin: &Path, class_dir: &Path, cfg: &FuzzConfig) -> String {
    let mut s = String::new();
    let p = &r.program;
    let cats: Vec<&str> = p.categories().iter().map(|c| c.label()).collect();
    let _ = writeln!(s, "{} — seed {} — categories {}", p.name, p.seed, cats.join(","));
    if let Some(e) = &r.build_error {
        let _ = writeln!(s, "  BUILD  generator/assembler rejected the program: {e}");
    }
    if let Some(e) = &r.invalid_reference {
        let _ = writeln!(
            s,
            "  INVALID  reference {} could not serve as the oracle: {e}",
            cfg.reference.label()
        );
        let _ = writeln!(
            s,
            "    (a VerifyError here is a generator bug or a CratonVM verifier bug — rerun with --hotspot to tell which)"
        );
    }
    for split in &r.splits {
        let chans: Vec<&str> = split.diffs.iter().map(|d| d.channel.label()).collect();
        let _ = writeln!(
            s,
            "  SPLIT  {}≠{}[{}]{}{}",
            cfg.reference.label(),
            split.mode.label(),
            chans.join(","),
            if split.flaky { " (did not reproduce on re-run)" } else { "" },
            if split.ir_verify_reject { " (IR verifier rejected a method)" } else { "" }
        );
        for d in &split.diffs {
            let _ = writeln!(s, "      {}: {} != {}", d.channel.label(), one_line(&d.cratonvm), one_line(&d.hotspot));
        }
        let _ = writeln!(
            s,
            "      repro: {}",
            repro_command(bin, class_dir, &p.name, split.mode, env_for(split.mode, cfg))
        );
    }
    for d in &r.hotspot_diffs {
        let _ = writeln!(
            s,
            "  HOTSPOT  {}≠java {}: {} != {}",
            cfg.reference.label(),
            d.channel.label(),
            one_line(&d.cratonvm),
            one_line(&d.hotspot)
        );
    }
    s
}

fn one_line(s: &str) -> String {
    let flat = s.replace('\n', " / ");
    if flat.chars().count() <= 160 {
        flat
    } else {
        format!("{}...", flat.chars().take(157).collect::<String>())
    }
}

/// The outcome of a fuzzing campaign.
#[derive(Debug, Default)]
pub struct FuzzSummary {
    pub ran: usize,
    pub failures: Vec<ProgramReport>,
    /// Failure directories written under `out_dir`.
    pub written: Vec<PathBuf>,
}

/// Generate, run, minimize and record programs for seeds
/// `start_seed .. start_seed + count`. Stops early after `max_failures`.
pub fn fuzz(
    bin: &Path,
    start_seed: u64,
    count: u64,
    max_failures: usize,
    cfg: &FuzzConfig,
    mut log: impl FnMut(&str),
) -> Result<FuzzSummary, RunError> {
    let mut summary = FuzzSummary::default();
    for seed in start_seed..start_seed.saturating_add(count) {
        let p = generate_program(seed, cfg.rounds);
        let report = run_program(bin, &p, cfg)?;
        summary.ran += 1;
        if !report.is_failure() {
            log(&format!("  OK    {} [{}]", p.name, labels(&p)));
            continue;
        }
        let minimized = if cfg.minimize && report.build_error.is_none() {
            let modes: Vec<Mode> = report.splits.iter().map(|s| s.mode).collect();
            let invalid = report.invalid_reference.is_some();
            if modes.is_empty() && !invalid {
                // Only a HotSpot difference: nothing CratonVM-internal to shrink against.
                p.clone()
            } else {
                minimize_program(&p, |cand| still_fails(bin, cand, &modes, invalid, cfg))
            }
        } else {
            p.clone()
        };
        let dir = write_failure(bin, &report, &minimized, cfg)?;
        log(&format!("  FAIL  {} [{}] -> {}", p.name, labels(&p), dir.display()));
        for line in render_report(&report, bin, &dir, cfg).lines() {
            log(&format!("        {line}"));
        }
        summary.written.push(dir);
        summary.failures.push(report);
        if summary.failures.len() >= max_failures {
            log(&format!("  (stopping after {max_failures} failing program(s))"));
            break;
        }
    }
    Ok(summary)
}

fn labels(p: &JitProgram) -> String {
    p.categories().iter().map(|c| c.label()).collect::<Vec<_>>().join(",")
}

/// Write `<out>/<name>/`: the minimized class (runnable as-is with
/// `-cp <out>/<name>`), `original/<name>.class`, `report.txt`, and
/// `program.txt` for both versions.
fn write_failure(bin: &Path, report: &ProgramReport, minimized: &JitProgram, cfg: &FuzzConfig) -> Result<PathBuf, RunError> {
    let name = &report.program.name;
    let dir = cfg.out_dir.join(name);
    let orig_dir = dir.join("original");
    std::fs::create_dir_all(&orig_dir).map_err(io_err)?;
    if let Ok(bytes) = build_class(minimized, cfg.version) {
        std::fs::write(dir.join(format!("{name}.class")), bytes).map_err(io_err)?;
    }
    if let Ok(bytes) = build_class(&report.program, cfg.version) {
        std::fs::write(orig_dir.join(format!("{name}.class")), bytes).map_err(io_err)?;
    }
    let mut text = render_report(report, bin, &dir, cfg);
    let _ = writeln!(
        text,
        "\nregenerate: cratonvm-difftest fuzz-jit --seed {} --count 1 --rounds {} --class-version {} --modes {},{}",
        report.program.seed,
        report.program.rounds,
        cfg.version.major(),
        cfg.reference.label(),
        report
            .splits
            .iter()
            .map(|s| s.mode.label())
            .collect::<Vec<_>>()
            .join(",")
    );
    if let Some(r) = &report.reference {
        let _ = writeln!(text, "\n--- reference stdout ---\n{}", r.stdout);
    }
    for split in &report.splits {
        let _ = writeln!(
            text,
            "\n--- {} stdout ---\n{}\n--- {} stderr (tail) ---\n{}",
            split.mode.label(),
            split.observation.stdout,
            split.mode.label(),
            split.observation.stderr.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
        );
    }
    std::fs::write(dir.join("report.txt"), text).map_err(io_err)?;
    let program_text = format!(
        "=== minimized ===\n{}\n=== original ===\n{}",
        describe(minimized),
        describe(&report.program)
    );
    std::fs::write(dir.join("program.txt"), program_text).map_err(io_err)?;
    Ok(dir)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn generation_is_byte_identical_per_seed() {
        for seed in 0..32u64 {
            let a = build_class(&generate_program(seed, DEFAULT_ROUNDS), ClassVersion::Java8).unwrap();
            let b = build_class(&generate_program(seed, DEFAULT_ROUNDS), ClassVersion::Java8).unwrap();
            assert_eq!(a, b, "seed {seed} is not deterministic");
        }
        let a = build_class(&generate_program(1, DEFAULT_ROUNDS), ClassVersion::Java8).unwrap();
        let b = build_class(&generate_program(2, DEFAULT_ROUNDS), ClassVersion::Java8).unwrap();
        assert_ne!(a, b, "different seeds should differ");
    }

    #[test]
    fn every_generated_class_assembles_and_passes_the_shape_checks() {
        for seed in 0..128u64 {
            let p = generate_program(seed, DEFAULT_ROUNDS);
            for version in [ClassVersion::Java8, ClassVersion::Java5] {
                let bytes = build_class(&p, version)
                    .unwrap_or_else(|e| panic!("seed {seed}: {e}\n{}", describe(&p)));
                classgen::check_class_shape(&bytes)
                    .unwrap_or_else(|e| panic!("seed {seed} v{}: {e}\n{}", version.major(), describe(&p)));
                let scan = crate::matrix::scan_class_bytes(&p.name, &bytes)
                    .unwrap_or_else(|| panic!("seed {seed}: matrix scanner rejected the class"));
                assert_eq!(scan.methods, 2);
            }
        }
    }

    #[test]
    fn every_category_assembles_on_its_own() {
        // A program made of one category, many sub-seeds: isolates a broken
        // emitter from the others.
        for c in Category::ALL {
            for sub in 0..40u64 {
                let p = JitProgram {
                    seed: sub,
                    name: "One".into(),
                    snippets: vec![Snippet { category: c, seed: sub }],
                    inputs: generate_program(sub, 1).inputs,
                    rounds: 1,
                };
                let bytes = build_class(&p, ClassVersion::Java8)
                    .unwrap_or_else(|e| panic!("{} sub-seed {sub}: {e}", c.label()));
                classgen::check_class_shape(&bytes).unwrap_or_else(|e| panic!("{}: {e}", c.label()));
            }
        }
    }

    #[test]
    fn sixteen_consecutive_seeds_cover_every_category() {
        let covered: BTreeSet<Category> = (100..116u64)
            .flat_map(|s| generate_program(s, DEFAULT_ROUNDS).categories())
            .collect();
        for c in Category::ALL {
            assert!(covered.contains(&c), "{} not covered", c.label());
        }
    }

    #[test]
    fn a_small_seed_range_reaches_every_targeted_opcode() {
        let mut seen: BTreeSet<u8> = BTreeSet::new();
        for seed in 0..256u64 {
            let p = generate_program(seed, DEFAULT_ROUNDS);
            let bytes = build_class(&p, ClassVersion::Java8).unwrap();
            let scan = crate::matrix::scan_class_bytes(&p.name, &bytes).unwrap();
            for key in scan.sites.keys() {
                if let Some(Ok(o)) = key.split(':').next().map(str::parse::<u8>) {
                    seen.insert(o);
                }
            }
        }
        let want = [
            op::IDIV, op::IREM, op::LDIV, op::LREM, op::ISHL, op::ISHR, op::IUSHR, op::LSHL,
            op::LSHR, op::LUSHR, op::FCMPL, op::FCMPG, op::DCMPL, op::DCMPG, op::F2I, op::F2L,
            op::D2I, op::D2L, op::FREM, op::DREM, op::I2B, op::I2C, op::I2S, op::TABLESWITCH,
            op::LOOKUPSWITCH, op::DUP_X2, op::DUP2_X1, op::DUP2_X2, op::SWAP, op::POP2,
            op::WIDE, op::IFNULL, op::CHECKCAST, op::IALOAD, op::IASTORE, op::DALOAD,
            op::DASTORE, op::IF_ICMPGT, op::IF_ICMPLT,
        ];
        for o in want {
            assert!(
                seen.contains(&o),
                "opcode {o:#04x} ({}) never generated",
                crate::matrix::opcode_name(o).unwrap_or("?")
            );
        }
    }

    #[test]
    fn edge_constants_reach_the_constant_pool() {
        // MIN_VALUE / -1 needs the MIN constant; NaN compares need a NaN float.
        let mut ints = false;
        let mut nan = false;
        for seed in 0..64u64 {
            let bytes = build_class(&generate_program(seed, DEFAULT_ROUNDS), ClassVersion::Java8).unwrap();
            ints |= bytes.windows(5).any(|w| w == [3, 0x80, 0, 0, 0]);
            nan |= bytes.windows(5).any(|w| w == [4, 0x7f, 0xc0, 0, 0]);
        }
        assert!(ints, "Integer.MIN_VALUE constant never emitted");
        assert!(nan, "Float.NaN constant never emitted");
    }

    #[test]
    fn minimization_shrinks_to_the_interesting_part() {
        let mut p = generate_program(7, 64);
        p.snippets.push(Snippet {
            category: Category::StackShuffles,
            seed: 1,
        });
        let min = minimize_program(&p, |c| {
            c.snippets.iter().any(|s| s.category == Category::StackShuffles)
        });
        assert_eq!(min.snippets.len(), 1);
        assert_eq!(min.snippets[0].category, Category::StackShuffles);
        assert_eq!(min.inputs.len(), 1);
        assert_eq!(min.rounds, 1);
        // The minimized program still builds.
        build_class(&min, ClassVersion::Java8).unwrap();
    }

    #[test]
    fn a_reference_without_a_checksum_cannot_be_the_oracle() {
        let mut o = Observation::empty();
        o.exit_code = Some(0);
        o.stdout = "1\n2".into();
        assert!(reference_problem(&o).is_some());
        o.stdout = format!("1\n{} {CHECKSUM_NAME} 42", crate::checksum::MARKER);
        assert!(reference_problem(&o).is_none());
        o.exit_code = Some(1);
        assert!(reference_problem(&o).is_some());
    }

    #[test]
    fn repro_command_names_the_mode_env_and_extra_env() {
        let cmd = repro_command(
            Path::new("cratonvm"),
            Path::new("out/JitFuzz_3"),
            "JitFuzz_3",
            Mode::IrJit,
            &[("CRATONVM_DBG_GC_STRESS".into(), "65536".into())],
        );
        assert!(cmd.contains("CRATONVM_JIT_FORCE_C2=1"), "{cmd}");
        assert!(cmd.contains("CRATONVM_DBG_GC_STRESS=65536"), "{cmd}");
        assert!(cmd.ends_with("-cp out/JitFuzz_3 JitFuzz_3") || cmd.contains("JitFuzz_3 JitFuzz_3"), "{cmd}");
    }
}
