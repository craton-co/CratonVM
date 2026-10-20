// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The optimizing tier's element-wise vector loop (round 16 matrix residual,
//! `docs/internal/performance/r16-matrix-2d-inner-loop-rereads-rows-FIXED-20261006.md`,
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
//! JIT round 21 (lane vector, book `docs/internal/proposals/jit/jit-r21-vector-proposals.md`)
//! widened the shape under three opt-in switches, each read once per compile
//! ([`VecOpts::from_flags`]):
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

use crate::ir::{CmpOp, Graph, IrType, MemKind, NodeId, Op, NO_NODE};
use crate::ir_schedule::Schedule;

/// `CRATONVM_JIT_IR_VECTOR_LOOP` -- the element-wise vector pre-loop. Default
/// ON; `=0` turns it off (the scalar loop then runs every element, as before).
/// Read per compile: no static, so a test's thread override reaches it.
pub(crate) fn ir_vector_loop_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_VECTOR_LOOP")
}

/// `CRATONVM_JIT_IR_VECTOR_BITOPS` (r21 vector, VEC21-1) -- `& | ^` and
/// constant-count shifts over `int`/`long` lanes. Opt-in (perf).
pub(crate) fn ir_vector_bitops_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_VECTOR_BITOPS")
}

/// `CRATONVM_JIT_IR_VECTOR_SUBWORD` (r21 vector, VEC21-3) -- `byte[]` and
/// `short[]`/`char[]` lanes. Opt-in (perf).
pub(crate) fn ir_vector_subword_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_VECTOR_SUBWORD")
}

/// `CRATONVM_JIT_IR_VECTOR_REDUCE` (r21 vector, VEC21-4) -- `int`/`long` add
/// reductions. Opt-in (perf).
pub(crate) fn ir_vector_reduce_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_VECTOR_REDUCE")
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
}

impl VecOpts {
    pub(crate) fn from_flags() -> Self {
        Self {
            bitops: ir_vector_bitops_enabled(),
            subword: ir_vector_subword_enabled(),
            reduce: ir_vector_reduce_enabled(),
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
}

/// A reduction loop's accumulator (VEC21-4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VecReduce {
    /// The accumulator phi `s`; its back value is `s + value` (or `s - value`
    /// when `sub`).
    pub(crate) acc: NodeId,
    pub(crate) sub: bool,
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
    /// The distinct scalar operands (constants included), all defined outside
    /// the loop.
    pub(crate) invariants: Vec<NodeId>,
    /// `bases[store_base][j] = value`. Unused (0) for a reduction.
    pub(crate) store_base: usize,
    pub(crate) value: VExpr,
    /// `Some` for a reduction loop (VEC21-4), which has no store: each step
    /// adds `value`'s lanes into the accumulator register instead.
    pub(crate) reduce: Option<VecReduce>,
}

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
    if !hb.successors.contains(&latch)
        || lb.successors.as_slice() != [h]
        || lb.predecessors.as_slice() != [h]
        || lb.terminator.is_some()
    {
        return Err("body is not one straight-line latch block");
    }
    let eb = &schedule.blocks[entry_pred];
    if eb.successors.as_slice() != [h] || eb.terminator.is_some() {
        return Err("entry edge is not a goto");
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
    // Header phis: one Int induction (plus, for a reduction, one Int/Long
    // accumulator), any number of memory phis.
    let max_value_phis = 1 + usize::from(opts.reduce);
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
        if !matches!(an.ty, IrType::Int | IrType::Long) {
            return Err("a reduction over a non-integer accumulator");
        }
        let acc_next = an.inputs[back_slot];
        let nx = node(acc_next).ok_or("dangling accumulator")?;
        if !in_loop(acc_next) || nx.ty != an.ty || nx.inputs.len() != 2 {
            return Err("the accumulator is not updated by one add in the loop");
        }
        let (e, sub) = match nx.op {
            Op::Add if nx.inputs[0] == acc && nx.inputs[1] != acc => (nx.inputs[1], false),
            Op::Add if nx.inputs[1] == acc && nx.inputs[0] != acc => (nx.inputs[0], false),
            Op::Sub if nx.inputs[0] == acc && nx.inputs[1] != acc => (nx.inputs[1], true),
            _ => return Err("the accumulator is not updated by one add in the loop"),
        };
        if users[acc_next as usize].iter().any(|&u| u != acc) {
            return Err("the accumulated value is read besides the back edge");
        }
        if users[acc as usize].iter().any(|&u| u != acc_next && in_loop(u)) {
            return Err("the accumulator is read in the loop besides its add");
        }
        reduce = Some((VecReduce { acc, sub }, acc_next, e));
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
        _ => return Err("test is not `iv < bound`"),
    };
    let bound_length_of = match node(bound) {
        Some(n) if n.op == Op::ArrayLength && in_loop(bound) => {
            let arr = match n.inputs.as_slice() {
                [a] => *a,
                [_, _, a, ..] => *a,
                _ => return Err("array length arity"),
            };
            if !invariant(arr) || users[bound as usize].iter().any(|&u| u != cond) {
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
            Op::Add if id == iv_next => {}
            Op::Add | Op::Sub if id == acc_next => {}
            Op::Const(_) | Op::ConstF(_) => {}
            Op::ArrayLoad(k) => {
                if n.inputs.len() != 4 || n.inputs[3] != iv || !invariant(n.inputs[2]) {
                    return Err("an element read not at [invariant][iv]");
                }
                set_kind(*k)?;
                members.push(id);
            }
            Op::ArrayStore(k) => {
                if n.inputs.len() != 5 || n.inputs[3] != iv || !invariant(n.inputs[2]) {
                    return Err("an element store not at [invariant][iv]");
                }
                if store != NO_NODE {
                    return Err("more than one store");
                }
                set_kind(*k)?;
                store = id;
            }
            Op::Add | Op::Sub | Op::Mul | Op::Div => members.push(id),
            // VEC21-1 / VEC21-3: judged per kind in `build_expr`.
            Op::And | Op::Or | Op::Xor | Op::Shl | Op::Shr | Op::UShr
                if opts.bitops || opts.subword =>
            {
                members.push(id)
            }
            other => {
                if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
                    eprintln!("[ir-vec]   n{id} {other:?} has no vector form");
                }
                return Err("an op with no vector form in the loop");
            }
        }
    }
    let kind = kind.ok_or("no array access")?;
    match reduce {
        Some((r, _, _)) => {
            if store != NO_NODE {
                return Err("a reduction loop that also stores");
            }
            if !matches!(kind, VecKind::Int | VecKind::Long)
                || graph.nodes[r.acc as usize].ty != kind.ir_type()
            {
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
    if users[iv_next as usize].iter().any(|&u| u != iv) {
        return Err("the stepped induction is read besides the back edge");
    }
    let mut bases: Vec<NodeId> = Vec::new();
    let mut invariants: Vec<NodeId> = Vec::new();
    let mut seen: Vec<NodeId> = Vec::new();
    let (store_base, root) = match reduce {
        Some((_, _, e)) => (0, e),
        None => {
            let st = &graph.nodes[store as usize];
            (base_index(&mut bases, st.inputs[2])?, st.inputs[4])
        }
    };
    let ctx = TreeCtx {
        graph,
        kind,
        opts,
        in_loop: &in_loop,
        invariant: &invariant,
    };
    let value = build_expr(&ctx, root, &mut bases, &mut invariants, &mut seen, 0)?;
    // Every member was reached from the store (nothing computed and dropped,
    // no load whose only purpose was a fault).
    if members.iter().any(|m| !seen.contains(m)) {
        return Err("a body value the store does not read");
    }
    Ok(VecLoopPlan {
        header: h,
        entry_pred,
        header_bci: region.bytecode_pc,
        iv,
        bound,
        bound_length_of,
        kind,
        bases,
        invariants,
        store_base,
        value,
        reduce: reduce.map(|(r, _, _)| r),
    })
}

fn is_const(graph: &Graph, id: NodeId, v: i64) -> bool {
    graph.nodes.get(id as usize).map(|n| &n.op) == Some(&Op::Const(v))
}

fn base_index(bases: &mut Vec<NodeId>, b: NodeId) -> Result<usize, &'static str> {
    if let Some(i) = bases.iter().position(|&x| x == b) {
        return Ok(i);
    }
    if bases.len() == MAX_BASES {
        return Err("more distinct arrays than scratch registers");
    }
    bases.push(b);
    Ok(bases.len() - 1)
}

/// What [`build_expr`] reads but never changes.
struct TreeCtx<'a> {
    graph: &'a Graph,
    kind: VecKind,
    opts: VecOpts,
    in_loop: &'a dyn Fn(NodeId) -> bool,
    invariant: &'a dyn Fn(NodeId) -> bool,
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
    bases: &mut Vec<NodeId>,
    invariants: &mut Vec<NodeId>,
    seen: &mut Vec<NodeId>,
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
    if !(cx.in_loop)(id) {
        if !(cx.invariant)(id) {
            return Err("an operand neither in the loop nor invariant");
        }
        return Ok(VExpr::Inv(inv_index(invariants, id)));
    }
    if matches!(n.op, Op::Const(_) | Op::ConstF(_)) {
        return Ok(VExpr::Inv(inv_index(invariants, id)));
    }
    if seen.contains(&id) {
        // A shared subtree (`t = b[j]; c[j] = t * t`): rebuilt per use, which
        // recomputes the same lane values.
    } else {
        seen.push(id);
    }
    match &n.op {
        Op::ArrayLoad(_) => Ok(VExpr::Load(base_index(bases, n.inputs[2])?)),
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
                        return build_expr(cx, keep, bases, invariants, seen, depth + 1);
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
                (VecKind::Int, VOp::Div) | (VecKind::Long, VOp::Div | VOp::Mul) => {
                    return Err("integer op with no AVX2 form")
                }
                (VecKind::Byte, VOp::Mul | VOp::Div) | (VecKind::Short, VOp::Div) => {
                    return Err("sub-word op with no AVX2 form")
                }
                _ => {}
            }
            let l = build_expr(cx, n.inputs[0], bases, invariants, seen, depth + 1)?;
            let r = build_expr(cx, n.inputs[1], bases, invariants, seen, depth + 1)?;
            Ok(VExpr::Bin(op, Box::new(l), Box::new(r)))
        }
        Op::Shl | Op::Shr | Op::UShr => {
            if n.inputs.len() != 2 {
                return Err("a shift with a guard input");
            }
            let count = const_of(cx.graph, n.inputs[1]).ok_or("a shift by a variable count")?;
            if kind.is_subword() {
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
                if !seen.contains(&inner_id) {
                    seen.push(inner_id);
                }
                return build_expr(cx, inner.inputs[0], bases, invariants, seen, depth + 1);
            }
            if !cx.opts.bitops {
                return Err("a shift with no vector form here");
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
            // Java masks the count; x86 would give zero for a count >= width.
            // Cast: masked to 0..=63, fits a byte.
            let imm = (count & mask) as u8;
            let e = build_expr(cx, n.inputs[0], bases, invariants, seen, depth + 1)?;
            Ok(VExpr::Shift(sh, imm, Box::new(e)))
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

fn emit_binop(out: &mut Vec<u8>, kind: VecKind, op: VOp, dst: u8, src1: u8, src2: Src) {
    let (mm, pp, opc) = op_code(kind, op);
    match src2 {
        Src::Reg(r) => {
            vex3(out, dst, 0, r, mm, false, src1, true, pp, opc);
            out.push(0xC0 | ((dst & 7) << 3) | (r & 7));
        }
        Src::Mem { base, disp } => {
            vex3(out, dst, IV_REG, base, mm, false, src1, true, pp, opc);
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
    let (opc, ext) = match (kind, s) {
        (VecKind::Int, VShift::Shl) => (0x72, 6),
        (VecKind::Int, VShift::Sar) => (0x72, 4),
        (VecKind::Int, VShift::Shr) => (0x72, 2),
        (VecKind::Long, VShift::Shl) => (0x73, 6),
        (VecKind::Long, VShift::Shr) => (0x73, 2),
        _ => return None,
    };
    vex3(out, ext, 0, src, 1, false, dst, true, 1, opc);
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
    /// `inv_regs[i]` holds `invariants[i]`, broadcast.
    pub(crate) inv_regs: &'a [u8],
    /// A reduction's accumulator register (VEC21-4); `None` otherwise.
    pub(crate) acc: Option<u8>,
}

/// The machine code of ONE vector step at byte displacement `disp` (the
/// element offset `HEADER_SIZE + step * 32` folded in), or `None` when the
/// tree needs more temporaries than `regs.temps` has (or a reduction has no
/// accumulator register).
pub(crate) fn step_code(plan: &VecLoopPlan, regs: &StepRegs<'_>, disp: i8) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut free: Vec<u8> = regs.temps.iter().rev().copied().collect();
    if let Some(r) = plan.reduce {
        let acc = regs.acc?;
        let op = if r.sub { VOp::Sub } else { VOp::Add };
        match leaf_src(plan, regs, &plan.value, disp) {
            Some(src) => emit_binop(&mut out, plan.kind, op, acc, acc, src),
            None => {
                let dst = free.pop()?;
                gen(plan, regs, &plan.value, dst, &mut free, disp, &mut out)?;
                emit_binop(&mut out, plan.kind, op, acc, acc, Src::Reg(dst));
            }
        }
        return Some(out);
    }
    let dst = free.pop()?;
    gen(plan, regs, &plan.value, dst, &mut free, disp, &mut out)?;
    emit_store(&mut out, plan.kind, dst, BASE_REGS[plan.store_base], disp);
    Some(out)
}

fn leaf_src(plan: &VecLoopPlan, regs: &StepRegs<'_>, e: &VExpr, disp: i8) -> Option<Src> {
    match e {
        VExpr::Load(b) => Some(Src::Mem {
            base: BASE_REGS[*b],
            disp,
        }),
        VExpr::Inv(i) => regs.inv_regs.get(*i).map(|&r| Src::Reg(r)),
        VExpr::Bin(..) | VExpr::Shift(..) => {
            let _ = plan;
            None
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
        VExpr::Load(b) => emit_load(out, plan.kind, dst, BASE_REGS[*b], disp),
        VExpr::Inv(i) => emit_move(out, dst, *regs.inv_regs.get(*i)?),
        VExpr::Shift(s, imm, inner) => {
            gen(plan, regs, inner, dst, free, disp, out)?;
            emit_shift(out, plan.kind, *s, dst, dst, *imm)?;
        }
        VExpr::Bin(op, l, r) => {
            // A leaf on the left and a tree on the right: compute the tree
            // first when the op commutes, so the leaf is a memory/register
            // operand and no temporary is needed.
            let (l, r) = if op.commutative()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_of(kind: VecKind, value: VExpr, bases: usize, invs: usize) -> VecLoopPlan {
        VecLoopPlan {
            header: 0,
            entry_pred: 0,
            header_bci: None,
            iv: 0,
            bound: 0,
            bound_length_of: None,
            kind,
            bases: (10..10 + bases as NodeId).collect(),
            invariants: (20..20 + invs as NodeId).collect(),
            store_base: 0,
            value,
            reduce: None,
        }
    }

    #[test]
    fn the_encodings_match_the_sdm() {
        let mut o = Vec::new();
        // vmovupd ymm0, [rax + rdx*8 + 0x10]
        emit_load(&mut o, VecKind::Double, 0, 0, 0x10);
        assert_eq!(o, [0xC4, 0xE1, 0x7D, 0x10, 0x44, 0xD0, 0x10]);
        o.clear();
        // vmovupd [rcx + rdx*8 + 0x30], ymm9
        emit_store(&mut o, VecKind::Double, 9, 1, 0x30);
        assert_eq!(o, [0xC4, 0x61, 0x7D, 0x11, 0x4C, 0xD1, 0x30]);
        o.clear();
        // vmulpd ymm0, ymm0, [r8 + rdx*8 + 0x10]
        emit_binop(
            &mut o,
            VecKind::Double,
            VOp::Mul,
            0,
            0,
            Src::Mem {
                base: 8,
                disp: 0x10,
            },
        );
        assert_eq!(o, [0xC4, 0xC1, 0x7D, 0x59, 0x44, 0xD0, 0x10]);
        o.clear();
        // vaddpd ymm1, ymm1, ymm15
        emit_binop(&mut o, VecKind::Double, VOp::Add, 1, 1, Src::Reg(15));
        assert_eq!(o, [0xC4, 0xC1, 0x75, 0x58, 0xCF]);
        o.clear();
        // vbroadcastsd ymm15, xmm0
        emit_broadcast(&mut o, VecKind::Double, 15, 0);
        assert_eq!(o, [0xC4, 0x62, 0x7D, 0x19, 0xF8]);
        o.clear();
        // vpmulld ymm0, ymm0, [rax + rdx*4 + 0x10]
        emit_binop(
            &mut o,
            VecKind::Int,
            VOp::Mul,
            0,
            0,
            Src::Mem {
                base: 0,
                disp: 0x10,
            },
        );
        assert_eq!(o, [0xC4, 0xE2, 0x7D, 0x40, 0x44, 0x90, 0x10]);
        o.clear();
        // vmovdqu ymm0, [rax + rdx*4 + 0x10]
        emit_load(&mut o, VecKind::Int, 0, 0, 0x10);
        assert_eq!(o, [0xC4, 0xE1, 0x7E, 0x6F, 0x44, 0x90, 0x10]);
        o.clear();
        // vmovaps ymm0, ymm15
        emit_move(&mut o, 0, 15);
        assert_eq!(o, [0xC4, 0xC1, 0x7C, 0x28, 0xC7]);
    }

    /// The round-21 encodings (VEC21-1/-3/-4), each against the SDM form.
    #[test]
    fn the_r21_encodings_match_the_sdm() {
        let mut o = Vec::new();
        // vpxor ymm0, ymm0, [rcx + rdx*4 + 0x10]   VEX.256.66.0F EF /r
        emit_binop(
            &mut o,
            VecKind::Int,
            VOp::Xor,
            0,
            0,
            Src::Mem {
                base: 1,
                disp: 0x10,
            },
        );
        assert_eq!(o, [0xC4, 0xE1, 0x7D, 0xEF, 0x44, 0x91, 0x10]);
        o.clear();
        // vpand ymm1, ymm1, [rax + rdx*8 + 0x30]   (long lanes, scale 8)
        emit_binop(
            &mut o,
            VecKind::Long,
            VOp::And,
            1,
            1,
            Src::Mem {
                base: 0,
                disp: 0x30,
            },
        );
        assert_eq!(o, [0xC4, 0xE1, 0x75, 0xDB, 0x4C, 0xD0, 0x30]);
        o.clear();
        // vpor ymm0, ymm0, ymm9
        emit_binop(&mut o, VecKind::Int, VOp::Or, 0, 0, Src::Reg(9));
        assert_eq!(o, [0xC4, 0xC1, 0x7D, 0xEB, 0xC1]);
        o.clear();
        // vpslld ymm0, ymm0, 3   VEX.256.66.0F 72 /6 ib
        emit_shift(&mut o, VecKind::Int, VShift::Shl, 0, 0, 3).unwrap();
        assert_eq!(o, [0xC4, 0xE1, 0x7D, 0x72, 0xF0, 0x03]);
        o.clear();
        // vpsrad ymm1, ymm1, 31   72 /4
        emit_shift(&mut o, VecKind::Int, VShift::Sar, 1, 1, 31).unwrap();
        assert_eq!(o, [0xC4, 0xE1, 0x75, 0x72, 0xE1, 0x1F]);
        o.clear();
        // vpsrlq ymm9, ymm9, 7   73 /2 ib, vvvv = ymm9, rm = ymm9 (VEX.B)
        emit_shift(&mut o, VecKind::Long, VShift::Shr, 9, 9, 7).unwrap();
        assert_eq!(o, [0xC4, 0xC1, 0x35, 0x73, 0xD1, 0x07]);
        o.clear();
        // No VPSRAQ in AVX2.
        assert!(emit_shift(&mut o, VecKind::Long, VShift::Sar, 0, 0, 1).is_none());
        // vpaddb ymm0, ymm0, [rax + rdx*1 + 0x10]   66 0F FC, scale 1
        emit_binop(
            &mut o,
            VecKind::Byte,
            VOp::Add,
            0,
            0,
            Src::Mem {
                base: 0,
                disp: 0x10,
            },
        );
        assert_eq!(o, [0xC4, 0xE1, 0x7D, 0xFC, 0x44, 0x10, 0x10]);
        o.clear();
        // vpmullw ymm0, ymm0, [rcx + rdx*2 + 0x10]   66 0F D5, scale 2
        emit_binop(
            &mut o,
            VecKind::Short,
            VOp::Mul,
            0,
            0,
            Src::Mem {
                base: 1,
                disp: 0x10,
            },
        );
        assert_eq!(o, [0xC4, 0xE1, 0x7D, 0xD5, 0x44, 0x51, 0x10]);
        o.clear();
        // vmovdqu ymm0, [rax + rdx*1 + 0x10]
        emit_load(&mut o, VecKind::Byte, 0, 0, 0x10);
        assert_eq!(o, [0xC4, 0xE1, 0x7E, 0x6F, 0x44, 0x10, 0x10]);
        o.clear();
        // vpbroadcastb ymm1, xmm0   VEX.256.66.0F38.W0 78 /r
        emit_broadcast(&mut o, VecKind::Byte, 1, 0);
        assert_eq!(o, [0xC4, 0xE2, 0x7D, 0x78, 0xC8]);
        o.clear();
        // vpbroadcastw ymm1, xmm0   79 /r
        emit_broadcast(&mut o, VecKind::Short, 1, 0);
        assert_eq!(o, [0xC4, 0xE2, 0x7D, 0x79, 0xC8]);
        o.clear();
        // vpxor ymm1, ymm1, ymm1
        emit_zero(&mut o, 1);
        assert_eq!(o, [0xC4, 0xE1, 0x75, 0xEF, 0xC9]);
        o.clear();
        // The int fold of ymm1 through xmm0.
        let f = fold_code(VecKind::Int, 1, 0).unwrap();
        assert_eq!(
            f,
            [
                0xC4, 0xE3, 0x7D, 0x39, 0xC8, 0x01, // vextracti128 xmm0, ymm1, 1
                0xC4, 0xE1, 0x71, 0xFE, 0xC8, // vpaddd xmm1, xmm1, xmm0
                0xC4, 0xE1, 0x79, 0x70, 0xC1, 0x4E, // vpshufd xmm0, xmm1, 0x4e
                0xC4, 0xE1, 0x71, 0xFE, 0xC8, // vpaddd xmm1, xmm1, xmm0
                0xC4, 0xE1, 0x79, 0x70, 0xC1, 0xB1, // vpshufd xmm0, xmm1, 0xb1
                0xC4, 0xE1, 0x71, 0xFE, 0xC8, // vpaddd xmm1, xmm1, xmm0
                0xC4, 0xE1, 0x79, 0x7E, 0xC8, // vmovd eax, xmm1
            ]
        );
        // The long fold ends in VMOVQ (W1) after one shuffle.
        let f = fold_code(VecKind::Long, 1, 0).unwrap();
        assert_eq!(&f[f.len() - 5..], [0xC4, 0xE1, 0xF9, 0x7E, 0xC8]);
        assert!(fold_code(VecKind::Double, 1, 0).is_none());
        assert!(fold_code(VecKind::Int, 1, 1).is_none());
    }

    #[test]
    fn a_step_with_two_registers_computes_c_plus_x_times_b() {
        // c[j] = c[j] + x * b[j], bases [c, b], invariants [x].
        let plan = plan_of(
            VecKind::Double,
            VExpr::Bin(
                VOp::Add,
                Box::new(VExpr::Load(0)),
                Box::new(VExpr::Bin(
                    VOp::Mul,
                    Box::new(VExpr::Inv(0)),
                    Box::new(VExpr::Load(1)),
                )),
            ),
            2,
            1,
        );
        let regs = StepRegs {
            temps: &[0],
            inv_regs: &[1],
            acc: None,
        };
        let code = step_code(&plan, &regs, 0x10).expect("one temporary suffices");
        assert_eq!(
            code,
            [
                0xC4, 0xE1, 0x7C, 0x28, 0xC1, // vmovaps ymm0, ymm1
                0xC4, 0xE1, 0x7D, 0x59, 0x44, 0xD1, 0x10, // vmulpd ymm0, ymm0, [rcx+rdx*8+16]
                0xC4, 0xE1, 0x7D, 0x58, 0x44, 0xD0, 0x10, // vaddpd ymm0, ymm0, [rax+rdx*8+16]
                0xC4, 0xE1, 0x7D, 0x11, 0x44, 0xD0, 0x10, // vmovupd [rax+rdx*8+16], ymm0
            ]
        );
        // `c - x * b` does not commute: it needs a second temporary.
        let mut sub = plan.clone();
        if let VExpr::Bin(op, ..) = &mut sub.value {
            *op = VOp::Sub;
        }
        assert!(step_code(&sub, &regs, 0x10).is_none());
        assert!(step_code(
            &sub,
            &StepRegs {
                temps: &[0, 8],
                inv_regs: &[1],
                acc: None,
            },
            0x10
        )
        .is_some());
    }

    /// A reduction step adds into the accumulator and stores nothing; a leaf
    /// addend needs no temporary at all, and a missing accumulator refuses.
    #[test]
    fn a_reduction_step_adds_into_the_accumulator() {
        let mut plan = plan_of(VecKind::Int, VExpr::Load(0), 1, 0);
        plan.reduce = Some(VecReduce { acc: 30, sub: false });
        let regs = StepRegs {
            temps: &[],
            inv_regs: &[],
            acc: Some(1),
        };
        let code = step_code(&plan, &regs, 0x10).expect("a leaf needs no temporary");
        // vpaddd ymm1, ymm1, [rax + rdx*4 + 0x10]
        assert_eq!(code, [0xC4, 0xE1, 0x75, 0xFE, 0x4C, 0x90, 0x10]);
        let none = StepRegs {
            temps: &[0],
            inv_regs: &[],
            acc: None,
        };
        assert!(step_code(&plan, &none, 0x10).is_none());
        // `s -= a[j] * b[j]`: one temporary, then VPSUBD into the accumulator.
        plan.value = VExpr::Bin(VOp::Mul, Box::new(VExpr::Load(0)), Box::new(VExpr::Load(1)));
        plan.bases.push(11);
        plan.reduce = Some(VecReduce { acc: 30, sub: true });
        let regs = StepRegs {
            temps: &[0],
            inv_regs: &[],
            acc: Some(1),
        };
        let code = step_code(&plan, &regs, 0x10).expect("one temporary");
        assert_eq!(
            code,
            [
                0xC4, 0xE1, 0x7E, 0x6F, 0x44, 0x90, 0x10, // vmovdqu ymm0, [rax+rdx*4+16]
                0xC4, 0xE2, 0x7D, 0x40, 0x44, 0x91, 0x10, // vpmulld ymm0, ymm0, [rcx+rdx*4+16]
                0xC4, 0xE1, 0x75, 0xFA, 0xC8, // vpsubd ymm1, ymm1, ymm0
            ]
        );
    }

    // ── Executed: the step bytes on the host CPU (AVX2 only) ─────────────

    /// `CALL_ARG_REGS[0]`: where the harness's one pointer argument arrives.
    #[cfg(windows)]
    const ARG0: u8 = 1; // RCX
    #[cfg(not(windows))]
    const ARG0: u8 = 7; // RDI

    /// A fake array: `len` in the VM's length word, `data` from
    /// `HEADER_SIZE`, and 64 guard bytes past the end that no step may touch.
    fn fake_array(elem: usize, data: &[u8]) -> Vec<u8> {
        let h = cratonvm_types::HEADER_SIZE;
        let mut v = vec![0xA5u8; h + data.len() + 64];
        let len = (data.len() / elem) as u32;
        let lo = cratonvm_types::ARRAY_LENGTH_OFFSET;
        v[lo..lo + 4].copy_from_slice(&len.to_le_bytes());
        v[h..h + data.len()].copy_from_slice(data);
        v
    }

    /// Run `body` as `extern "C" fn(*mut [u64; 8])` with RAX/RCX/R8/R9 =
    /// `args[0..4]`, RDX = `args[4]` (the element index `j`) and, on return,
    /// `args[5]` = RAX. `body` may touch only those GPRs, R10/R11 and
    /// XMM0-XMM3 (volatile on both ABIs). `None` without AVX2.
    fn run_on_host(body: &[u8], args: &mut [u64; 8]) -> Option<()> {
        if !crate::x64::has_avx2() {
            return None;
        }
        let mut buf = crate::ExecutableBuffer::new(4096).expect("executable buffer");
        // MOV R11, ARG0
        buf.emit(&[0x49, 0x89, 0xC3 | ((ARG0 & 7) << 3)]);
        // MOV RAX,[R11]; MOV RCX,[R11+8]; MOV R8,[R11+16]; MOV R9,[R11+24];
        // MOV RDX,[R11+32].
        buf.emit(&[0x49, 0x8B, 0x03]);
        buf.emit(&[0x49, 0x8B, 0x4B, 0x08]);
        buf.emit(&[0x4D, 0x8B, 0x43, 0x10]);
        buf.emit(&[0x4D, 0x8B, 0x4B, 0x18]);
        buf.emit(&[0x49, 0x8B, 0x53, 0x20]);
        buf.emit(body);
        buf.emit(&[0x49, 0x89, 0x43, 0x28]); // MOV [R11+40], RAX
        buf.emit(&[0xC5, 0xF8, 0x77]); // VZEROUPPER
        buf.emit(&[0xC3]); // RET
        assert!(!buf.overflowed());
        buf.finalize();
        // SAFETY: the buffer is a complete `extern "C" fn(*mut u64)` that
        // reads its argument block, touches only volatile registers and the
        // fake arrays the block points at, and returns.
        let f = unsafe { std::mem::transmute::<*const u8, extern "C" fn(*mut u64)>(buf.as_ptr()) };
        f(args.as_mut_ptr());
        Some(())
    }

    fn disp0() -> i8 {
        i8::try_from(cratonvm_types::HEADER_SIZE).expect("header fits a disp8")
    }

    /// `c[j] = ((a[j] << 3) ^ (a[j] >>> 29)) & k` over eight int lanes,
    /// executed, against the scalar Java answer (the count masking included:
    /// a planner-masked `<< 35` is `<< 3`).
    #[test]
    fn an_int_bitops_step_executes_like_java() {
        let a: Vec<i32> = (0..8i32)
            .map(|i| i.wrapping_mul(0x1357_9BDF).wrapping_sub(i.wrapping_shl(28)))
            .collect();
        let k = 0x7FF0_0FF7_i32;
        let mut c = fake_array(4, &[0u8; 32]);
        let a_arr = fake_array(4, &a.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>());
        let kbuf: Vec<u8> = (0..8).flat_map(|_| k.to_le_bytes()).collect();
        let k_arr = fake_array(4, &kbuf);
        // bases [c, a, kk]: k as a third "array" keeps the test register-free.
        let plan = plan_of(
            VecKind::Int,
            VExpr::Bin(
                VOp::And,
                Box::new(VExpr::Bin(
                    VOp::Xor,
                    Box::new(VExpr::Shift(VShift::Shl, 35 & 31, Box::new(VExpr::Load(1)))),
                    Box::new(VExpr::Shift(VShift::Shr, 29, Box::new(VExpr::Load(1)))),
                )),
                Box::new(VExpr::Load(2)),
            ),
            3,
            0,
        );
        let regs = StepRegs {
            temps: &[0, 1, 2],
            inv_regs: &[],
            acc: None,
        };
        let code = step_code(&plan, &regs, disp0()).expect("three temporaries");
        let mut args = [
            c.as_mut_ptr() as u64,
            a_arr.as_ptr() as u64,
            k_arr.as_ptr() as u64,
            0,
            0,
            0,
            0,
            0,
        ];
        if run_on_host(&code, &mut args).is_none() {
            return;
        }
        let h = cratonvm_types::HEADER_SIZE;
        for (i, &x) in a.iter().enumerate() {
            // Java's `x << 35` is `x << (35 & 31)`.
            let want = (x.wrapping_shl(35) ^ ((x as u32) >> 29) as i32) & k;
            let got = i32::from_le_bytes(c[h + 4 * i..h + 4 * i + 4].try_into().unwrap());
            assert_eq!(got, want, "lane {i}");
        }
        assert!(c[h + 32..].iter().all(|&b| b == 0xA5), "a store past the step");
    }

    /// `c[j] = (byte)((a[j] ^ b[j]) + a[j])` over 32 byte lanes, executed,
    /// against Java's int arithmetic truncated by `bastore`.
    #[test]
    fn a_byte_lane_step_executes_like_java() {
        let a: Vec<u8> = (0..32u32).map(|i| (i * 37 + 200) as u8).collect();
        let b: Vec<u8> = (0..32u32).map(|i| (i * 91 + 7) as u8).collect();
        let mut c = fake_array(1, &[0u8; 32]);
        let a_arr = fake_array(1, &a);
        let b_arr = fake_array(1, &b);
        let plan = plan_of(
            VecKind::Byte,
            VExpr::Bin(
                VOp::Add,
                Box::new(VExpr::Bin(
                    VOp::Xor,
                    Box::new(VExpr::Load(1)),
                    Box::new(VExpr::Load(2)),
                )),
                Box::new(VExpr::Load(1)),
            ),
            3,
            0,
        );
        let regs = StepRegs {
            temps: &[0, 1],
            inv_regs: &[],
            acc: None,
        };
        let code = step_code(&plan, &regs, disp0()).expect("two temporaries");
        let mut args = [
            c.as_mut_ptr() as u64,
            a_arr.as_ptr() as u64,
            b_arr.as_ptr() as u64,
            0,
            0,
            0,
            0,
            0,
        ];
        if run_on_host(&code, &mut args).is_none() {
            return;
        }
        let h = cratonvm_types::HEADER_SIZE;
        for i in 0..32 {
            // Java: baload sign-extends, the int ops, bastore keeps the low byte.
            let (x, y) = (i32::from(a[i] as i8), i32::from(b[i] as i8));
            let want = ((x ^ y) + x) as u8;
            assert_eq!(c[h + i], want, "lane {i}");
        }
        assert!(c[h + 32..].iter().all(|&b| b == 0xA5), "a store past the step");
    }

    /// `s += a[j] * b[j]` (int, wrapping), two steps at `j = 0` and `j = 8`
    /// into one accumulator, folded: the scalar wrapped dot product exactly.
    #[test]
    fn an_int_reduction_folds_to_the_wrapped_scalar_sum() {
        let a: Vec<i32> = (0..16).map(|i| i32::MAX - i * 977).collect();
        let b: Vec<i32> = (0..16).map(|i| 3 + i * 65_537).collect();
        let a_arr = fake_array(4, &a.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>());
        let b_arr = fake_array(4, &b.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>());
        let mut plan = plan_of(
            VecKind::Int,
            VExpr::Bin(VOp::Mul, Box::new(VExpr::Load(0)), Box::new(VExpr::Load(1))),
            2,
            0,
        );
        plan.reduce = Some(VecReduce { acc: 30, sub: false });
        let regs = StepRegs {
            temps: &[0],
            inv_regs: &[],
            acc: Some(1),
        };
        let d0 = disp0();
        let mut body = Vec::new();
        emit_zero(&mut body, 1);
        body.extend(step_code(&plan, &regs, d0).unwrap());
        // The second step at the next 32 bytes (the main loop's unrolled form).
        body.extend(step_code(&plan, &regs, d0 + 32).unwrap());
        body.extend(fold_code(VecKind::Int, 1, 0).unwrap());
        let mut args = [a_arr.as_ptr() as u64, b_arr.as_ptr() as u64, 0, 0, 0, 0, 0, 0];
        if run_on_host(&body, &mut args).is_none() {
            return;
        }
        let want = a
            .iter()
            .zip(&b)
            .fold(0i32, |s, (x, y)| s.wrapping_add(x.wrapping_mul(*y)));
        assert_eq!(args[5] as u32 as i32, want);
    }

    /// The long fold: four qword lanes of `a[j]` near `Long.MAX_VALUE` wrap.
    #[test]
    fn a_long_reduction_folds_to_the_wrapped_scalar_sum() {
        let a: Vec<i64> = (0..4).map(|i| i64::MAX - i * 3).collect();
        let a_arr = fake_array(8, &a.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>());
        let mut plan = plan_of(VecKind::Long, VExpr::Load(0), 1, 0);
        plan.reduce = Some(VecReduce { acc: 30, sub: true });
        let regs = StepRegs {
            temps: &[0],
            inv_regs: &[],
            acc: Some(1),
        };
        let mut body = Vec::new();
        emit_zero(&mut body, 1);
        body.extend(step_code(&plan, &regs, disp0()).unwrap());
        body.extend(fold_code(VecKind::Long, 1, 0).unwrap());
        let mut args = [a_arr.as_ptr() as u64, 0, 0, 0, 0, 0, 0, 0];
        if run_on_host(&body, &mut args).is_none() {
            return;
        }
        let want = a.iter().fold(0i64, |s, x| s.wrapping_sub(*x));
        assert_eq!(args[5] as i64, want);
    }
}
