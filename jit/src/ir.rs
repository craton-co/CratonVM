// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Sea-of-Nodes Intermediate Representation for the JIT compiler.
//!
//! The IR sits between bytecode and x86-64 machine code, decoupling
//! optimization from instruction selection.  Nodes live in a flat
//! arena (`Graph`); edges are `NodeId` indices.
//!
//! ## Pipeline
//!
//! ```text
//! bytecode → IrBuilder → Graph → optimize → schedule → lower → x64
//! ```
//!
//! The IR is built incrementally: methods that pass `ir_compatible()`
//! are compiled through this pipeline; others fall back to the
//! single-pass `x64::compile`.

use crate::JitInvokeInfo;
use std::collections::{HashMap, HashSet};

// ── Node identity ────────────────────────────────────────────────────

/// Lightweight handle into the IR graph.
pub type NodeId = u32;

/// Sentinel value — "no node".
pub const NO_NODE: NodeId = u32::MAX;

// ── Types ────────────────────────────────────────────────────────────

/// IR value types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IrType {
    /// No value (void return, control edges).
    Void,
    /// 32-bit integer (JVM `int`, `boolean`, `byte`, `char`, `short`).
    Int,
    /// 64-bit integer (JVM `long`).
    Long,
    /// 32-bit float.
    Float,
    /// 64-bit double.
    Double,
    /// Object / array reference.
    Ref,
    /// Control-flow token (not a data value).
    Control,
    /// Memory-ordering token (enforces load/store sequencing).
    Memory,
}

// ── Comparison operators ─────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    /// Invert the comparison (for branch-inversion optimizations).
    pub fn negate(self) -> Self {
        match self {
            CmpOp::Eq => CmpOp::Ne,
            CmpOp::Ne => CmpOp::Eq,
            CmpOp::Lt => CmpOp::Ge,
            CmpOp::Le => CmpOp::Gt,
            CmpOp::Gt => CmpOp::Le,
            CmpOp::Ge => CmpOp::Lt,
        }
    }

    /// x86-64 condition code byte for a near-Jcc (0x0F 0x8x).
    pub fn x64_cc(self) -> u8 {
        match self {
            CmpOp::Eq => 0x84,
            CmpOp::Ne => 0x85,
            CmpOp::Lt => 0x8C,
            CmpOp::Le => 0x8E,
            CmpOp::Gt => 0x8F,
            CmpOp::Ge => 0x8D,
        }
    }
}

// ── Memory access kinds ──────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemKind {
    Int,
    Long,
    Float,
    Double,
    Ref,
    Byte,
    Char,
    Short,
}

// ── Node operations ──────────────────────────────────────────────────

/// The operation a node performs.
///
/// Nodes are partitioned into three families:
/// - **Control**: govern control flow (Start, Return, If, Merge, Region).
/// - **Data**: compute values (Const, Add, Cmp, Phi, Param, …).
/// - **Side-effect**: interact with memory (Load, Store, Call, New).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    // ── Control ──────────────────────────────────────────────────────
    /// Method entry.  Produces (ctrl, mem) via `Proj(0)`, `Proj(1)`.
    Start,

    /// Method exit.  Inputs: `[ctrl, (value)?]`.
    Return,

    /// Two-way branch.  Inputs: `[ctrl, cond]`.
    /// Outputs via `Proj(0)` (true edge) and `Proj(1)` (false edge).
    If,

    /// Control-flow merge of multiple predecessors (forward join).
    /// Inputs: `[ctrl_0, ctrl_1, …]`.
    Merge,

    /// Loop header merge — like Merge but marks a loop back-edge.
    /// Inputs: `[ctrl_entry, ctrl_backedge]`.
    Region,

    // ── Projection ───────────────────────────────────────────────────
    /// Extract the Nth output from a multi-output node.
    /// Input: `[multi_node]`.
    Proj(u8),

    // ── Constants ────────────────────────────────────────────────────
    /// Integer / long constant (no inputs).
    Const(i64),

    /// Float / double constant stored as raw bits (no inputs).
    ConstF(u64),

    // ── Parameters ───────────────────────────────────────────────────
    /// Method parameter at index `i` (no inputs — defined at Start).
    Param(u16),

    // ── SSA ──────────────────────────────────────────────────────────
    /// φ function at a Merge / Region.
    /// Inputs: `[merge_node, val_0, val_1, …]` (one val per predecessor).
    Phi,

    // ── Integer arithmetic ───────────────────────────────────────────
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Neg,

    // ── Bitwise / shift ──────────────────────────────────────────────
    And,
    Or,
    Xor,
    Shl,
    Shr,
    UShr,

    // ── Comparison ───────────────────────────────────────────────────
    /// Integer compare.  Inputs: `[left, right]`.  Result: `Int` (0/1).
    Cmp(CmpOp),
    /// Long 3-way compare (`lcmp`). Inputs: `[left, right]` (both `Long`).
    /// Result: `Int` ∈ {-1, 0, 1} = sign(left − right), signed. Typically feeds
    /// an `if<cond>` against zero (`lcmp; iflt` ⇒ `left < right`).
    LCmp,
    /// FP 3-way compare (`fcmpl`/`fcmpg`/`dcmpl`/`dcmpg`). Inputs:
    /// `[left, right]` (both `Float` or both `Double`). Result: `Int` ∈
    /// {-1, 0, 1} feeding an `if<cond>` against zero, exactly like [`Op::LCmp`].
    /// `double` selects `ucomisd` vs `ucomiss`. `nan_greater` is the JVMS
    /// unordered rule: a NaN operand yields `+1` for the `g` variants
    /// (`fcmpg`/`dcmpg`) and `-1` for the `l` variants (`fcmpl`/`dcmpl`).
    FCmp {
        double: bool,
        nan_greater: bool,
    },

    // ── Type conversion ──────────────────────────────────────────────
    I2L,
    L2I,
    I2F,
    I2D,
    L2F,
    L2D,
    F2I,
    F2L,
    F2D,
    D2I,
    D2L,
    D2F,
    I2B,
    I2C,
    I2S,

    // ── Memory ───────────────────────────────────────────────────────
    /// Array / field load.  Inputs: `[ctrl, mem, base, index/offset]`.
    Load(MemKind),

    /// Array / field store.  Inputs: `[ctrl, mem, base, index/offset, value]`.
    Store(MemKind),

    /// Array length.  Inputs: `[ctrl, mem, array_ref]`.
    ArrayLength,

    /// Array ELEMENT load. Inputs: `[ctrl, mem, array, index]`. Unlike
    /// [`Op::Load`] (a field read at a compile-time offset), the index is a
    /// runtime value and the lowerer emits the JVMS null + bounds checks
    /// (deopt-on-fault) before the `[array + HEADER_SIZE + index*elem_size]`
    /// access. `MemKind` carries the element type (Slice B wires the FP kinds —
    /// `faload`/`daload`). Impure (may throw NPE/AIOOBE) — DCE-rooted, not GVN'd.
    ArrayLoad(MemKind),

    /// Array ELEMENT store. Inputs: `[ctrl, mem, array, index, value]`. The
    /// store analogue of [`Op::ArrayLoad`] (`fastore`/`dastore`); produces a new
    /// memory token. Impure (NPE/AIOOBE + the write) — DCE-rooted.
    ArrayStore(MemKind),

    // ── Allocation ───────────────────────────────────────────────────
    /// Object allocation.  Inputs: `[ctrl, mem]`.
    New {
        class_id: u32,
        num_fields: usize,
    },

    /// Array allocation.  Inputs: `[ctrl, mem, length]`.
    NewArray {
        element_type: u8,
    },

    // ── Method calls ─────────────────────────────────────────────────
    /// Method call.  Inputs: `[ctrl, mem, args…]`.
    ///
    /// The node is BOTH the new memory token (a call is a hard memory barrier
    /// — it consumes the prior token and produces a new one) AND, for a
    /// non-void call, the return value (in `node_slot`, like `Op::Load`).
    /// `info_ptr` is the address of a leaked `JitInvokeInfo` (kept alive by the
    /// `CompiledMethod`'s `_jit_invoke_infos`) baked into the dispatch call as
    /// the helper's `info_ptr` argument. Only emitted for `invokestatic`
    /// (Gap B slice) in an oop-free method — see `ir_lower`'s `Op::Call` arm.
    Call {
        info_ptr: usize,
    },
    /// Scalar TDigest lambda adapter. Inputs: [ctrl, mem, lambda, index].
    LambdaIntToDouble,

    // ── Speculation guard (real-frame-deopt) ─────────────────────────
    /// Speculative guard. Inputs: `[ctrl, cond]`. If `cond` is zero at
    /// runtime the guard fails and control transfers to the deopt
    /// trampoline, reconstructing the interpreter frame for `bci`. A
    /// non-zero `cond` falls through with no effect. Produces no value and
    /// is impure (must not be DCE'd or reordered past side effects), so it
    /// is neither `is_pure` nor `is_control`.
    Guard {
        /// Bytecode index whose frame state to reconstruct on failure.
        bci: usize,
    },

    // ── Dead / removed ───────────────────────────────────────────────
    /// Placeholder for a removed node (inputs cleared, not referenced).
    Dead,
}

impl Op {
    /// True if this node produces a control token.
    pub fn is_control(&self) -> bool {
        matches!(
            self,
            Op::Start | Op::Return | Op::If | Op::Merge | Op::Region | Op::Proj(_)
        )
    }

    /// True if this node is pure (no side effects, safe to GVN / eliminate).
    pub fn is_pure(&self) -> bool {
        matches!(
            self,
            Op::Const(_)
                | Op::ConstF(_)
                | Op::Param(_)
                | Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Neg
                | Op::And
                | Op::Or
                | Op::Xor
                | Op::Shl
                | Op::Shr
                | Op::UShr
                | Op::Cmp(_)
                | Op::I2L
                | Op::L2I
                | Op::I2F
                | Op::I2D
                | Op::L2F
                | Op::L2D
                | Op::F2I
                | Op::F2L
                | Op::F2D
                | Op::D2I
                | Op::D2L
                | Op::D2F
                | Op::I2B
                | Op::I2C
                | Op::I2S
        )
    }
}

// ── Node ─────────────────────────────────────────────────────────────

/// A single node in the IR graph.
#[derive(Clone, Debug)]
pub struct Node {
    /// What this node computes.
    pub op: Op,
    /// Value type produced by this node.
    pub ty: IrType,
    /// Input edges (data + control + memory).
    pub inputs: Vec<NodeId>,
    /// Bytecode PC that produced this node (for OSR / debug).
    pub bytecode_pc: Option<usize>,
}

// ── Safepoint snapshots (deopt frame provenance) ─────────────────────

/// A snapshot of the abstract interpreter state at a bytecode boundary,
/// used to build a deopt `FrameState`.
///
/// Records, for the given `bci`, the `NodeId` currently providing each
/// local variable and each operand-stack slot (`NO_NODE` = undefined /
/// uninitialised). The lowerer (`ir_lower.rs`) resolves each `NodeId` to a
/// concrete `FrameValue` (frame spill slot or constant) once physical
/// locations are known, producing a `DeoptimizationPoint` keyed by the
/// native code offset of the safepoint.
///
/// This is the IR-path realisation of step 1 of `real-frame-deopt.md`:
/// carry the interpreter locals[]/stack[] shadow alongside the SSA graph
/// so a precise interpreter frame can be rebuilt at the trapping bci. The
/// snapshots are emit-and-discard until the lowerer consumes them — they
/// never change codegen on their own.
#[derive(Clone, Debug)]
pub struct SafepointSnapshot {
    /// Bytecode index this frame state resumes at.
    pub bci: usize,
    /// `NodeId` for each local variable slot (index = local index).
    pub locals: Vec<NodeId>,
    /// `NodeId` for each operand-stack slot (index 0 = bottom of stack).
    pub stack: Vec<NodeId>,
}

// ── Graph ────────────────────────────────────────────────────────────

/// The IR graph — a flat arena of nodes with an entry and exit.
pub struct Graph {
    /// All nodes, indexed by `NodeId`.
    pub nodes: Vec<Node>,
    /// The `Start` node.
    pub entry: NodeId,
    /// The `Return` node (may be a Merge if multiple returns).
    pub exit: NodeId,
    /// Deopt safepoint snapshots, recorded by the builder at bytecode
    /// boundaries and resolved into `DeoptimizationPoint`s by the lowerer.
    /// Empty when the builder did not record any (e.g. hand-built graphs).
    pub safepoints: Vec<SafepointSnapshot>,
}

impl Graph {
    /// Allocate a new node, returning its `NodeId`.
    pub fn add(&mut self, op: Op, ty: IrType, inputs: Vec<NodeId>, pc: Option<usize>) -> NodeId {
        let id = self.nodes.len() as NodeId;
        self.nodes.push(Node {
            op,
            ty,
            inputs,
            bytecode_pc: pc,
        });
        id
    }

    /// Replace all uses of `old` with `new_id` in the entire graph.
    ///
    /// Also rewrites any `old` references held by safepoint snapshots so a
    /// deopt frame state survives optimization rewrites (GVN, const-fold,
    /// materialization) intact — a safepoint slot pointing at `old` must
    /// follow the value to `new_id`, exactly like a real input edge.
    pub fn replace_all_uses(&mut self, old: NodeId, new_id: NodeId) {
        for node in &mut self.nodes {
            for inp in &mut node.inputs {
                if *inp == old {
                    *inp = new_id;
                }
            }
        }
        for sp in &mut self.safepoints {
            for v in sp.locals.iter_mut().chain(sp.stack.iter_mut()) {
                if *v == old {
                    *v = new_id;
                }
            }
        }
    }

    /// Mark a node as dead (clears inputs and op).
    pub fn kill(&mut self, id: NodeId) {
        let n = &mut self.nodes[id as usize];
        n.op = Op::Dead;
        n.inputs.clear();
        n.ty = IrType::Void;
    }

    /// Number of live (non-dead) nodes.
    pub fn live_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.op != Op::Dead).count()
    }

    /// Build a use-count vector: `uses[id]` = number of nodes that reference `id` as an input.
    pub fn use_counts(&self) -> Vec<u32> {
        let mut counts = vec![0u32; self.nodes.len()];
        for node in &self.nodes {
            for &inp in &node.inputs {
                if (inp as usize) < counts.len() {
                    counts[inp as usize] += 1;
                }
            }
        }
        counts
    }
}

// ── IR Builder ───────────────────────────────────────────────────────

/// Loop-carried phis created eagerly at a loop header, so a backward branch
/// can back-patch each phi's back-edge input. Indexed parallel to the
/// builder's `locals`/`stack`; `NO_NODE` means no phi was created for that
/// slot (uninitialised on entry → not loop-carried).
#[derive(Clone)]
struct LoopPhis {
    /// Phi for each local variable slot (`NO_NODE` = none).
    local_phis: Vec<NodeId>,
    /// Phi for each operand-stack slot at the header (usually empty).
    stack_phis: Vec<NodeId>,
    /// Phi for the memory token (`NO_NODE` = none).
    mem_phi: NodeId,
}

/// State at a bytecode merge point (branch target / loop header).
#[derive(Clone)]
struct MergeState {
    /// The Merge / Region node for this target.
    merge_id: NodeId,
    /// Control inputs collected so far.
    ctrl_inputs: Vec<NodeId>,
    /// Locals state from each predecessor (one Vec<NodeId> per predecessor).
    local_snapshots: Vec<Vec<NodeId>>,
    /// Stack state from each predecessor.
    stack_snapshots: Vec<Vec<NodeId>>,
    /// Memory token from each predecessor.
    mem_inputs: Vec<NodeId>,
    /// Whether this merge has been visited (target reached during forward walk).
    visited: bool,
}

/// Builds an IR `Graph` from JVM bytecode by abstract-interpreting
/// the operand stack and local variable state.
pub struct IrBuilder {
    /// The graph under construction.
    pub graph: Graph,
    /// Abstract operand stack: each entry is a `NodeId`.
    stack: Vec<NodeId>,
    /// Current value of each local variable (SSA: changes on each store).
    locals: Vec<NodeId>,
    /// Current control token.
    ctrl: NodeId,
    /// Current memory token.
    mem: NodeId,
    /// Number of JVM locals (including params).
    _num_locals: usize,
    /// Merge-point state, keyed by bytecode PC.
    merges: HashMap<usize, MergeState>,
    /// Bytecode PCs that are loop headers (targets of a backward branch).
    /// A loop header is activated with eager loop-carried phis whose back-edge
    /// input is back-patched when the backward branch is processed.
    loop_headers: HashSet<usize>,
    /// Loop-carried phis per loop-header PC, recorded at activation so the
    /// backward branch can fill each phi's back-edge input.
    loop_phis: HashMap<usize, LoopPhis>,
    /// Resolved instance-field layout, keyed by bytecode pc:
    /// `pc → (field_index, type_tag)`. Populated by the caller
    /// ([`Self::set_field_info`]) from the constant-pool field resolver before
    /// [`Self::build`]; empty for hand-built / test graphs. A `getfield` whose
    /// pc is absent — or whose tag is not int-category — makes `build` bail
    /// (`None` → single-pass), the existing safety net.
    field_info: HashMap<usize, (usize, u8)>,
    /// Resolved allocation layout for `new`, keyed by bytecode pc:
    /// `pc → (class_id, num_fields)`. Set by [`Self::set_new_info`]; only the
    /// allocations the caller admits (non-escaping-eligible: no primitive field
    /// initialisers, no finalizer) are present. A `new` whose pc is absent
    /// makes `build` bail (`None` → single-pass).
    new_info: HashMap<usize, (u32, usize)>,
    /// Bytecode pcs of `invokespecial` calls to a trivial no-arg void
    /// constructor (`<init>()V`) that may be **elided** (the receiver is a
    /// fresh, non-escaping object whose fields are zero-initialised and set by
    /// the visible `putfield`s). Set by [`Self::set_trivial_init_pcs`]. An
    /// `invokespecial` whose pc is NOT here makes `build` bail.
    trivial_init_pcs: HashSet<usize>,
    /// Gap B: resolved `invokestatic` call sites the builder lowers into an
    /// `Op::Call`. `pc → (info_ptr, num_args, ret_type)` where `info_ptr` is the
    /// address of a leaked `JitInvokeInfo` (the dispatch helper's 2nd argument),
    /// `num_args` the JVM arg-slot count, and `ret_type` the JVM return-type byte
    /// (`V` → no value; `L`/`[` → a reference result, typed `IrType::Ref`; any
    /// int-category byte → `IrType::Int`). Set by [`Self::set_invoke_info`]. The
    /// caller (`lib.rs`) restricts which methods get this (inc 22: oops may be
    /// live across the call — found by the conservative GC scan of the spilled
    /// frame, sound because GC is non-moving while a JIT frame is active).
    invoke_info: HashMap<usize, (usize, usize, u8)>,
    pub tdigest_scalar_kernel: bool,
    /// inc 26/35: resolved `ldc2_w` (0x14) constant values (`pc → (bits, is_double)`).
    /// Set by [`Self::set_ldc2w_info`]; an `ldc2_w` pc not present bails to
    /// single-pass. `is_double` selects the lowering: a `long` constant becomes
    /// `Op::Const(Long)` (`lconst`), a `double` constant `Op::ConstF` (`dconst`).
    ldc2w_info: HashMap<usize, (i64, bool)>,
}

impl IrBuilder {
    /// Create a new builder for a method with `num_params` parameter slots
    /// and `num_locals` total local variable slots.
    pub fn new(num_params: usize, num_locals: usize) -> Self {
        let mut graph = Graph {
            nodes: Vec::with_capacity(256),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
        };

        // Node 0: Start
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        // Node 1: Proj(0) = initial control
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        // Node 2: Proj(1) = initial memory
        let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);

        // Create Param nodes for each parameter
        let mut locals = vec![NO_NODE; num_locals];
        for i in 0..num_params {
            let param = graph.add(Op::Param(i as u16), IrType::Int, vec![start], None);
            locals[i] = param;
        }

        IrBuilder {
            graph,
            stack: Vec::with_capacity(16),
            locals,
            ctrl,
            mem,
            _num_locals: num_locals,
            merges: HashMap::new(),
            loop_headers: HashSet::new(),
            loop_phis: HashMap::new(),
            field_info: HashMap::new(),
            new_info: HashMap::new(),
            trivial_init_pcs: HashSet::new(),
            invoke_info: HashMap::new(),
            tdigest_scalar_kernel: false,
            ldc2w_info: HashMap::new(),
        }
    }

    /// inc 26: supply resolved `ldc2_w` long-constant values (`pc → i64`). Must
    /// be called before [`Self::build`]; an `ldc2_w` pc not present bails.
    pub fn set_ldc2w_info(&mut self, info: HashMap<usize, (i64, bool)>) {
        self.ldc2w_info = info;
    }

    /// inc 27: the data type for a merge / loop-carried `Op::Phi`, derived from
    /// its value inputs (`inputs[0]` is the region/merge control). A `long`
    /// (or `double`) value makes the phi that type, so `frame_value_for`
    /// resolves the correct 64-bit width on a deopt-frame resume — the phi was
    /// historically hardcoded `Int`, which would truncate a long on resume.
    /// Codegen is unaffected (phi copies are unconditionally 64-bit `MOV`s and
    /// every consumer picks width from its own `node.ty`); only the (currently
    /// unwired) deopt-resume path reads the phi's own type. Scoped to category-2
    /// (long/double) to avoid perturbing `Ref`/`Float` phi handling; defaults to
    /// `Int` otherwise (the prior behaviour).
    fn phi_data_type(&self, inputs: &[NodeId]) -> IrType {
        for &n in inputs.iter().skip(1) {
            if n != NO_NODE {
                if let Some(node) = self.graph.nodes.get(n as usize) {
                    if matches!(node.ty, IrType::Long | IrType::Double) {
                        return node.ty;
                    }
                }
            }
        }
        IrType::Int
    }

    /// Re-lay-out the parameter locals with the JVM category-2 two-slot
    /// convention and type each `Param` node. inc 25: [`Self::new`] packs every
    /// parameter one-per-slot typed `Int`, which is only correct for an
    /// all-category-1 signature. A `long`/`double` parameter occupies **two**
    /// JVM local slots, so a following parameter's value node sits two slots
    /// higher — e.g. `(long a, long b)` puts `a` at slot 0 and `b` at slot 2,
    /// matching `lload_2` for `b`. The `Param` node *index* stays the JIT-arg
    /// index (the lowerer reads param `i` from prologue slot `(i+1)*8`, one
    /// register per parameter); only the `locals` placement and the node type
    /// change. `param_types` is in JIT-arg order (one entry per parameter).
    /// Must be called before [`Self::build`].
    pub fn set_param_types(&mut self, param_types: &[IrType]) {
        let cat2 = |t: &IrType| matches!(t, IrType::Long | IrType::Double);
        // Clear the parameter region so a stale one-per-slot placement (from
        // `new`) cannot shadow a now-two-slot-wide layout.
        let total: usize = param_types
            .iter()
            .map(|t| if cat2(t) { 2 } else { 1 })
            .sum();
        for s in 0..total.min(self.locals.len()) {
            self.locals[s] = NO_NODE;
        }
        let mut jvm_slot = 0usize;
        for (i, &ty) in param_types.iter().enumerate() {
            if let Some(pid) = self
                .graph
                .nodes
                .iter()
                .position(|n| matches!(n.op, Op::Param(idx) if idx as usize == i))
            {
                self.graph.nodes[pid].ty = ty;
                if jvm_slot < self.locals.len() {
                    self.locals[jvm_slot] = pid as NodeId;
                }
            }
            jvm_slot += if cat2(&ty) { 2 } else { 1 };
        }
    }

    /// Supply the resolved instance-field layout (`pc → (field_index,
    /// type_tag)`) the builder uses to lower `getfield` into an `Op::Load`.
    /// Must be called before [`Self::build`]; absent / non-int-category
    /// entries make the corresponding `getfield` bail to single-pass.
    pub fn set_field_info(&mut self, info: HashMap<usize, (usize, u8)>) {
        self.field_info = info;
    }

    /// Supply the allocation layout (`pc → (class_id, num_fields)`) and the set
    /// of elidable trivial-`<init>` pcs the builder uses to lower `new` into an
    /// `Op::New` (for escape-analysis scalar replacement). Must be called before
    /// [`Self::build`]; an absent `new`/`invokespecial` pc bails to single-pass.
    pub fn set_new_info(
        &mut self,
        new_info: HashMap<usize, (u32, usize)>,
        trivial_init_pcs: HashSet<usize>,
    ) {
        self.new_info = new_info;
        self.trivial_init_pcs = trivial_init_pcs;
    }

    /// Gap B: supply the resolved `invokestatic` call sites (`pc → (info_ptr,
    /// num_args, ret_type)`) the builder lowers into `Op::Call`. Must be called
    /// before [`Self::build`]; an `invokestatic` pc not present bails the build
    /// to single-pass. `ret_type` is the JVM return-type byte (see the field doc).
    pub fn set_invoke_info(&mut self, info: HashMap<usize, (usize, usize, u8)>) {
        self.invoke_info = info;
    }

    // ── Stack operations ─────────────────────────────────────────────

    fn push(&mut self, id: NodeId) {
        self.stack.push(id);
    }

    fn pop(&mut self) -> NodeId {
        self.stack.pop().unwrap_or(NO_NODE)
    }

    fn peek(&self) -> NodeId {
        self.stack.last().copied().unwrap_or(NO_NODE)
    }

    // ── Node creation helpers ────────────────────────────────────────

    fn add_data(&mut self, op: Op, ty: IrType, inputs: Vec<NodeId>, pc: usize) -> NodeId {
        self.graph.add(op, ty, inputs, Some(pc))
    }

    fn iconst(&mut self, val: i64) -> NodeId {
        // Check if we already have this constant (simple dedup)
        for (i, node) in self.graph.nodes.iter().enumerate() {
            if node.op == Op::Const(val) {
                return i as NodeId;
            }
        }
        self.graph.add(Op::Const(val), IrType::Int, vec![], None)
    }

    fn lconst(&mut self, val: i64) -> NodeId {
        self.graph.add(Op::Const(val), IrType::Long, vec![], None)
    }

    /// FP value tier (inc 30): a `float` constant, stored as its raw 32-bit
    /// IEEE-754 bit pattern (zero-extended into the `u64` payload). Typed
    /// `IrType::Float` so the lowerer marshals it through XMM. No dedup — GVN
    /// collapses duplicates. (`const_value`/`is_const_val` in `ir_optimize`
    /// match only `Op::Const`, never `Op::ConstF`, so the integer fold/identity
    /// passes never touch a float constant — FP arithmetic is never mis-folded.)
    fn fconst(&mut self, val: f32) -> NodeId {
        self.graph.add(
            Op::ConstF(val.to_bits() as u64),
            IrType::Float,
            vec![],
            None,
        )
    }

    /// FP value tier (inc 30): a `double` constant, stored as its raw 64-bit
    /// IEEE-754 bit pattern. Typed `IrType::Double`.
    fn dconst(&mut self, val: f64) -> NodeId {
        self.graph
            .add(Op::ConstF(val.to_bits()), IrType::Double, vec![], None)
    }

    // ── Merge / phi handling ─────────────────────────────────────────

    /// Register a branch target that may need a merge node.
    fn ensure_merge(&mut self, target_pc: usize) {
        if !self.merges.contains_key(&target_pc) {
            let merge_id = self
                .graph
                .add(Op::Merge, IrType::Control, vec![], Some(target_pc));
            self.merges.insert(
                target_pc,
                MergeState {
                    merge_id,
                    ctrl_inputs: Vec::new(),
                    local_snapshots: Vec::new(),
                    stack_snapshots: Vec::new(),
                    mem_inputs: Vec::new(),
                    visited: false,
                },
            );
        }
    }

    /// Record the current state as a predecessor of the merge at `target_pc`.
    ///
    /// If the merge has already been activated (`visited`), this predecessor is
    /// a **loop back-edge** (in reducible bytecode a forward merge is activated
    /// only after all its forward predecessors are in, so a later predecessor
    /// can only come from a backward branch). Back-patch the loop-header phis
    /// instead of recording a snapshot that activation already consumed.
    fn add_merge_predecessor(&mut self, target_pc: usize) {
        self.ensure_merge(target_pc);
        if self.merges.get(&target_pc).map_or(false, |s| s.visited) {
            self.patch_loop_backedge(target_pc);
            return;
        }
        let state = self.merges.get_mut(&target_pc).unwrap();
        state.ctrl_inputs.push(self.ctrl);
        state.local_snapshots.push(self.locals.clone());
        state.stack_snapshots.push(self.stack.clone());
        state.mem_inputs.push(self.mem);
    }

    /// Activate a loop header: create a loop-carried phi for every live local
    /// (and operand-stack slot) up front, seeded with the forward-entry
    /// value(s), leaving the back-edge input to be appended by
    /// [`Self::patch_loop_backedge`] when the backward branch is processed.
    ///
    /// Unlike [`Self::activate_merge`] (a forward join, where every predecessor
    /// is known before activation), a loop header is reached forward with only
    /// its entry predecessor(s); the back-edge value does not exist yet. We
    /// therefore cannot decide "phi only where predecessors differ" — any local
    /// the loop body writes needs a phi to carry the updated value around the
    /// back-edge. We conservatively create a phi for every entry-initialised
    /// local; `phi(region, x, x)` for loop-invariant locals collapses under
    /// GVN / is a self-copy in the lowerer.
    fn activate_loop_header(&mut self, target_pc: usize) {
        let state = match self.merges.get(&target_pc) {
            Some(s) if !s.ctrl_inputs.is_empty() => s.clone(),
            _ => return,
        };
        let region = state.merge_id;
        // The region's control inputs are the forward-entry ctrls so far; the
        // back-edge ctrl is appended in patch_loop_backedge.
        self.graph.nodes[region as usize].inputs = state.ctrl_inputs.clone();
        self.ctrl = region;

        // Memory phi: [region, entry_mem_0, …]; back-edge mem appended later.
        let mem_phi = {
            let mut inputs = vec![region];
            inputs.extend_from_slice(&state.mem_inputs);
            self.graph
                .add(Op::Phi, IrType::Memory, inputs, Some(target_pc))
        };
        self.mem = mem_phi;

        // Local phis: one per entry-initialised local.
        let num_locals = state.local_snapshots.first().map_or(0, |s| s.len());
        let mut local_phis = vec![NO_NODE; num_locals];
        for i in 0..num_locals {
            // Skip locals uninitialised on every entry edge (not loop-carried).
            if state
                .local_snapshots
                .iter()
                .all(|s| s.get(i).copied().unwrap_or(NO_NODE) == NO_NODE)
            {
                continue;
            }
            let mut inputs = vec![region];
            for snap in &state.local_snapshots {
                inputs.push(snap.get(i).copied().unwrap_or(NO_NODE));
            }
            let phi_ty = self.phi_data_type(&inputs);
            let phi = self.graph.add(Op::Phi, phi_ty, inputs, Some(target_pc));
            local_phis[i] = phi;
            self.locals[i] = phi;
        }

        // Operand-stack phis: one per stack slot at the header (usually none).
        let stack_depth = state.stack_snapshots.first().map_or(0, |s| s.len());
        let mut stack_phis = vec![NO_NODE; stack_depth];
        self.stack = state.stack_snapshots.first().cloned().unwrap_or_default();
        for i in 0..stack_depth {
            let mut inputs = vec![region];
            for snap in &state.stack_snapshots {
                inputs.push(snap.get(i).copied().unwrap_or(NO_NODE));
            }
            let phi_ty = self.phi_data_type(&inputs);
            let phi = self.graph.add(Op::Phi, phi_ty, inputs, Some(target_pc));
            stack_phis[i] = phi;
            self.stack[i] = phi;
        }

        self.loop_phis.insert(
            target_pc,
            LoopPhis {
                local_phis,
                stack_phis,
                mem_phi,
            },
        );
        if let Some(s) = self.merges.get_mut(&target_pc) {
            s.visited = true;
        }
    }

    /// Back-patch a loop header's phis with the back-edge values: append the
    /// current control to the region and the current local/stack/mem values to
    /// each phi (matching the appended region input by position).
    fn patch_loop_backedge(&mut self, target_pc: usize) {
        let lp = match self.loop_phis.get(&target_pc) {
            Some(lp) => lp.clone(),
            None => return,
        };
        let region = self.merges[&target_pc].merge_id;
        let back_ctrl = self.ctrl;
        self.graph.nodes[region as usize].inputs.push(back_ctrl);

        for (i, &phi) in lp.local_phis.iter().enumerate() {
            if phi != NO_NODE {
                let v = self.locals.get(i).copied().unwrap_or(NO_NODE);
                self.graph.nodes[phi as usize].inputs.push(v);
            }
        }
        for (i, &phi) in lp.stack_phis.iter().enumerate() {
            if phi != NO_NODE {
                let v = self.stack.get(i).copied().unwrap_or(NO_NODE);
                self.graph.nodes[phi as usize].inputs.push(v);
            }
        }
        if lp.mem_phi != NO_NODE {
            self.graph.nodes[lp.mem_phi as usize].inputs.push(self.mem);
        }
        // Record this back-edge as a predecessor for completeness (so any later
        // consumer that scans MergeState sees the right pred count).
        if let Some(s) = self.merges.get_mut(&target_pc) {
            s.ctrl_inputs.push(back_ctrl);
        }
    }

    /// Activate the merge at `target_pc`: set builder state from merged predecessors.
    fn activate_merge(&mut self, target_pc: usize) {
        let state = match self.merges.get(&target_pc) {
            Some(s) => s.clone(),
            None => return,
        };

        if state.ctrl_inputs.is_empty() {
            return;
        }

        // Update merge node inputs
        let merge_id = state.merge_id;
        self.graph.nodes[merge_id as usize].inputs = state.ctrl_inputs.clone();
        self.ctrl = merge_id;

        // Create phi for memory
        if state.mem_inputs.len() > 1 {
            let mut phi_inputs = vec![merge_id];
            phi_inputs.extend_from_slice(&state.mem_inputs);
            self.mem = self
                .graph
                .add(Op::Phi, IrType::Memory, phi_inputs, Some(target_pc));
        } else if let Some(&m) = state.mem_inputs.first() {
            self.mem = m;
        }

        // Create phis for locals that differ across predecessors
        let num_preds = state.local_snapshots.len();
        if num_preds > 0 {
            let num_locals = state.local_snapshots[0].len();
            for local_idx in 0..num_locals {
                let first_val = state.local_snapshots[0][local_idx];
                let all_same = state
                    .local_snapshots
                    .iter()
                    .all(|s| s.get(local_idx).copied().unwrap_or(NO_NODE) == first_val);
                if all_same {
                    self.locals[local_idx] = first_val;
                } else {
                    let mut phi_inputs = vec![merge_id];
                    for snap in &state.local_snapshots {
                        phi_inputs.push(snap.get(local_idx).copied().unwrap_or(NO_NODE));
                    }
                    let phi_ty = self.phi_data_type(&phi_inputs);
                    let phi = self.graph.add(Op::Phi, phi_ty, phi_inputs, Some(target_pc));
                    self.locals[local_idx] = phi;
                }
            }
        }

        // Restore stack (use first predecessor's stack, with phis where they differ)
        if let Some(first_stack) = state.stack_snapshots.first() {
            self.stack = first_stack.clone();
            if state.stack_snapshots.len() > 1 {
                for slot_idx in 0..self.stack.len() {
                    let first_val = self.stack[slot_idx];
                    let all_same = state
                        .stack_snapshots
                        .iter()
                        .all(|s| s.get(slot_idx).copied().unwrap_or(NO_NODE) == first_val);
                    if !all_same {
                        let mut phi_inputs = vec![merge_id];
                        for snap in &state.stack_snapshots {
                            phi_inputs.push(snap.get(slot_idx).copied().unwrap_or(NO_NODE));
                        }
                        let phi = self
                            .graph
                            .add(Op::Phi, IrType::Int, phi_inputs, Some(target_pc));
                        self.stack[slot_idx] = phi;
                    }
                }
            }
        }

        // Mark as visited
        if let Some(state) = self.merges.get_mut(&target_pc) {
            state.visited = true;
        }
    }

    // ── Main bytecode→IR conversion ──────────────────────────────────

    /// Convert bytecode to IR graph.  Returns `None` if an unsupported
    /// opcode is encountered.
    pub fn build(mut self, code: &[u8], code_len: usize) -> Option<Graph> {
        // First pass: identify branch targets so we know where merges go, and
        // which of them are loop headers (targets of a backward branch).
        let targets = find_branch_targets(code, code_len);
        for &target in &targets {
            self.ensure_merge(target);
        }
        self.loop_headers = find_loop_headers(code, code_len);

        let mut pc = 0;
        while pc < code_len {
            // If this PC is a merge target, activate the merge
            if self.merges.contains_key(&pc) {
                // Add current state as predecessor (fall-through). On a loop
                // header this is the forward-entry predecessor; the back-edge
                // arrives later and is back-patched (see add_merge_predecessor).
                if self.ctrl != NO_NODE {
                    self.add_merge_predecessor(pc);
                }
                // Loop headers get eager loop-carried phis; forward joins use
                // the difference-based merge (all predecessors already known).
                if self.loop_headers.contains(&pc) {
                    self.activate_loop_header(pc);
                } else {
                    self.activate_merge(pc);
                }
            }

            // Step 1 of real-frame-deopt: record the abstract interpreter
            // state at this bytecode boundary so a precise deopt frame can be
            // rebuilt if execution must resume here. We snapshot AFTER any
            // merge activation above, so the recorded locals/stack reflect the
            // merged (phi-resolved) state the interpreter would see on entry to
            // this bci. Dead code after an unconditional transfer (ctrl ==
            // NO_NODE, not itself a merge target) has no reachable frame state,
            // so it is skipped. These snapshots are emit-and-discard until the
            // lowerer resolves them — they do not affect codegen on their own.
            if self.ctrl != NO_NODE {
                self.graph.safepoints.push(SafepointSnapshot {
                    bci: pc,
                    locals: self.locals.clone(),
                    stack: self.stack.clone(),
                });
            }

            let op = code[pc];
            match op {
                // iconst_m1..iconst_5
                0x02..=0x08 => {
                    let val = (op as i64) - 3;
                    let c = self.iconst(val);
                    self.push(c);
                    pc += 1;
                }
                // bipush
                0x10 => {
                    let val = code[pc + 1] as i8 as i64;
                    let c = self.iconst(val);
                    self.push(c);
                    pc += 2;
                }
                // sipush
                0x11 => {
                    let val = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i64;
                    let c = self.iconst(val);
                    self.push(c);
                    pc += 3;
                }
                // iload
                0x15 => {
                    let idx = code[pc + 1] as usize;
                    self.push(self.locals[idx]);
                    pc += 2;
                }
                // iload_0..3
                0x1a..=0x1d => {
                    let idx = (op - 0x1a) as usize;
                    self.push(self.locals[idx]);
                    pc += 1;
                }
                // lload_0..3
                0x1e..=0x21 => {
                    let idx = (op - 0x1e) as usize;
                    self.push(self.locals[idx]);
                    pc += 1;
                }
                // lload (wide index) — inc 26. A long is one NodeId slot; the
                // value lives at `locals[idx]` (the high half slot `idx+1` is
                // never read by valid bytecode). Long-only, so inert for the int
                // path.
                0x16 => {
                    let idx = code[pc + 1] as usize;
                    self.push(self.locals[idx]);
                    pc += 2;
                }
                // aload — push an object reference local (same node-graph
                // mechanics as iload: a reference is just a NodeId on the
                // abstract stack; its only consumer in the IR slice we lower is
                // a `getfield` base).
                0x19 => {
                    let idx = code[pc + 1] as usize;
                    self.push(self.locals[idx]);
                    pc += 2;
                }
                // aload_0..3
                0x2a..=0x2d => {
                    let idx = (op - 0x2a) as usize;
                    self.push(self.locals[idx]);
                    pc += 1;
                }
                // astore — store an object reference into a local. Same
                // node-graph mechanics as istore: a reference is just a NodeId
                // slot in the abstract locals array (mirrors the `aload`
                // comment above). Real javac stores a `new` object into a local
                // (`new; dup; invokespecial; astore_N`), so without this the IR
                // builder bails on every allocation method and scalar
                // replacement never fires on production bytecode.
                0x3a => {
                    let idx = code[pc + 1] as usize;
                    let val = self.pop();
                    self.locals[idx] = val;
                    pc += 2;
                }
                // astore_0..3
                0x4b..=0x4e => {
                    let idx = (op - 0x4b) as usize;
                    let val = self.pop();
                    self.locals[idx] = val;
                    pc += 1;
                }
                // istore
                0x36 => {
                    let idx = code[pc + 1] as usize;
                    let val = self.pop();
                    self.locals[idx] = val;
                    pc += 2;
                }
                // istore_0..3
                0x3b..=0x3e => {
                    let idx = (op - 0x3b) as usize;
                    let val = self.pop();
                    self.locals[idx] = val;
                    pc += 1;
                }
                // lstore_0..3
                0x3f..=0x42 => {
                    let idx = (op - 0x3f) as usize;
                    let val = self.pop();
                    self.locals[idx] = val;
                    pc += 1;
                }
                // lstore (wide index) — inc 26. Stores the long NodeId at
                // `locals[idx]` (high half slot `idx+1` untouched). Long-only.
                0x37 => {
                    let idx = code[pc + 1] as usize;
                    let val = self.pop();
                    self.locals[idx] = val;
                    pc += 2;
                }
                // iadd
                0x60 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Add, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // ladd
                0x61 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Add, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // isub
                0x64 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Sub, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // lsub
                0x65 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Sub, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // imul
                0x68 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Mul, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // lmul
                0x69 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Mul, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // idiv
                0x6c => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Div, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // ldiv — the lowerer already emits CQO + 64-bit `IDIV RCX` for a
                // `Long` Op::Div (incl. the div-by-zero + MIN/-1 overflow guards),
                // so the builder just needs to produce the typed node. The long
                // div-by-zero guard is the cat-2 deopt trigger (real-frame-deopt).
                0x6d => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Div, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // irem
                0x70 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Rem, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // lrem — see `ldiv`: the lowerer handles the 64-bit `Long` path.
                0x71 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Rem, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // ineg
                0x74 => {
                    let a = self.pop();
                    let r = self.add_data(Op::Neg, IrType::Int, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                // lneg
                0x75 => {
                    let a = self.pop();
                    let r = self.add_data(Op::Neg, IrType::Long, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                // ishl
                0x78 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Shl, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // ishr
                0x7a => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Shr, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // iushr
                0x7c => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::UShr, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // iand
                0x7e => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::And, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // ior
                0x80 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Or, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // ixor
                0x82 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Xor, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // inc 27: long shifts + bitwise. The shift count is an `int` (one
                // slot) on top of the `long` value; the lowerer's `Op::Shl/Shr/
                // UShr` are width-aware (64-bit shift masks the count to 6 bits,
                // matching `lshl`'s `count & 0x3f`), and `Op::And/Or/Xor` are
                // already 64-bit (correct for both Int and Long). These are
                // 1-byte opcodes (the length walkers' default arm sizes them
                // correctly). Long-only → inert for the int path.
                // lshl
                0x79 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Shl, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // lshr
                0x7b => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Shr, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // lushr
                0x7d => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::UShr, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // land
                0x7f => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::And, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // lor
                0x81 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Or, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // lxor
                0x83 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Xor, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // lcmp — inc 27. 3-way signed compare of two longs → int
                // {-1,0,1}; usually feeds an `if<cond>` against 0 (the existing
                // 0x99..0x9e arm), so `lcmp; iflt` becomes `left < right`.
                0x94 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::LCmp, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // fcmpl / fcmpg / dcmpl / dcmpg — FP 3-way compare → int
                // {-1,0,1}, like `lcmp` but with the JVMS NaN-unordered rule:
                // a NaN operand yields -1 for the `l` variants (fcmpl/dcmpl) and
                // +1 for the `g` variants (fcmpg/dcmpg). The result feeds the
                // same `if<cond>`-against-0 arm as `lcmp`. Only reachable under
                // the FP gate (`method_uses_fp` ⇒ `ir_emit_fp`); the lowerer
                // emits `ucomiss`/`ucomisd` (see `Op::FCmp`).
                0x95 | 0x96 | 0x97 | 0x98 => {
                    let b = self.pop();
                    let a = self.pop();
                    let double = op == 0x97 || op == 0x98; // dcmpl / dcmpg
                    let nan_greater = op == 0x96 || op == 0x98; // fcmpg / dcmpg
                    let r = self.add_data(
                        Op::FCmp {
                            double,
                            nan_greater,
                        },
                        IrType::Int,
                        vec![a, b],
                        pc,
                    );
                    self.push(r);
                    pc += 1;
                }
                // iinc
                0x84 => {
                    let idx = code[pc + 1] as usize;
                    let inc = code[pc + 2] as i8 as i64;
                    let old = self.locals[idx];
                    let c = self.iconst(inc);
                    let r = self.add_data(Op::Add, IrType::Int, vec![old, c], pc);
                    self.locals[idx] = r;
                    pc += 3;
                }
                // i2l
                0x85 => {
                    let a = self.pop();
                    let r = self.add_data(Op::I2L, IrType::Long, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                // l2i
                0x88 => {
                    let a = self.pop();
                    let r = self.add_data(Op::L2I, IrType::Int, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                // i2b — sign-extend the low byte. Decomposed to `(x << 24) >> 24`
                // (32-bit `SHL`/`SAR EAX`, so the arithmetic shift sign-extends
                // the byte) rather than a dedicated truncation node — no new Op
                // or lowering, and the existing GVN/fold passes handle it.
                0x91 => {
                    let a = self.pop();
                    let c = self.iconst(24);
                    let shl = self.add_data(Op::Shl, IrType::Int, vec![a, c], pc);
                    let r = self.add_data(Op::Shr, IrType::Int, vec![shl, c], pc);
                    self.push(r);
                    pc += 1;
                }
                // i2c — zero-extend the low 16 bits: `x & 0xFFFF` (char is an
                // unsigned 16-bit value).
                0x92 => {
                    let a = self.pop();
                    let mask = self.iconst(0xFFFF);
                    let r = self.add_data(Op::And, IrType::Int, vec![a, mask], pc);
                    self.push(r);
                    pc += 1;
                }
                // i2s — sign-extend the low 16 bits: `(x << 16) >> 16`.
                0x93 => {
                    let a = self.pop();
                    let c = self.iconst(16);
                    let shl = self.add_data(Op::Shl, IrType::Int, vec![a, c], pc);
                    let r = self.add_data(Op::Shr, IrType::Int, vec![shl, c], pc);
                    self.push(r);
                    pc += 1;
                }

                // ── FP value tier (inc 30) ───────────────────────────────────
                //
                // The whole block below is reached ONLY when the method was
                // admitted via the `ir_emit_fp` gate clause in `lib.rs`
                // (`method_uses_fp` ⇒ FP-gate-only). With the gate off, no
                // FP-opcode method enters the builder, so these arms are inert
                // and gate-off codegen is byte-identical. A float is one NodeId
                // (cat-1); a double is one NodeId occupying two JVM local slots
                // (cat-2, exactly like `long` — the high-half slot is untouched).
                //
                // The lowerer (`ir_lower.rs`) marshals every `IrType::Float`/
                // `Double` value through XMM. The optimizer passes are FP-safe by
                // construction: `const_value`/`is_const_val` match only
                // `Op::Const`, never `Op::ConstF`, and `int_width` returns `None`
                // for FP, so fold/identity/reassociation never rewrite FP nodes.

                // fconst_0 / fconst_1 / fconst_2
                0x0b => {
                    let c = self.fconst(0.0);
                    self.push(c);
                    pc += 1;
                }
                0x0c => {
                    let c = self.fconst(1.0);
                    self.push(c);
                    pc += 1;
                }
                0x0d => {
                    let c = self.fconst(2.0);
                    self.push(c);
                    pc += 1;
                }
                // dconst_0 / dconst_1
                0x0e => {
                    let c = self.dconst(0.0);
                    self.push(c);
                    pc += 1;
                }
                0x0f => {
                    let c = self.dconst(1.0);
                    self.push(c);
                    pc += 1;
                }

                // fload / dload — read a FP local. Identical node-graph mechanics
                // to `iload`/`lload`: the value is a single NodeId held at
                // `locals[idx]` (a double's high-half slot `idx+1` is untouched).
                // fload (wide index)
                0x17 => {
                    let idx = code[pc + 1] as usize;
                    self.push(self.locals[idx]);
                    pc += 2;
                }
                // dload (wide index)
                0x18 => {
                    let idx = code[pc + 1] as usize;
                    self.push(self.locals[idx]);
                    pc += 2;
                }
                // fload_0..3
                0x22..=0x25 => {
                    let idx = (op - 0x22) as usize;
                    self.push(self.locals[idx]);
                    pc += 1;
                }
                // dload_0..3
                0x26..=0x29 => {
                    let idx = (op - 0x26) as usize;
                    self.push(self.locals[idx]);
                    pc += 1;
                }
                // fstore / dstore — write a FP local (same mechanics as
                // `istore`/`lstore`; a double leaves the high-half slot `idx+1`
                // untouched).
                // fstore (wide index)
                0x38 => {
                    let idx = code[pc + 1] as usize;
                    let val = self.pop();
                    self.locals[idx] = val;
                    pc += 2;
                }
                // dstore (wide index)
                0x39 => {
                    let idx = code[pc + 1] as usize;
                    let val = self.pop();
                    self.locals[idx] = val;
                    pc += 2;
                }
                // fstore_0..3
                0x43..=0x46 => {
                    let idx = (op - 0x43) as usize;
                    let val = self.pop();
                    self.locals[idx] = val;
                    pc += 1;
                }
                // dstore_0..3
                0x47..=0x4a => {
                    let idx = (op - 0x47) as usize;
                    let val = self.pop();
                    self.locals[idx] = val;
                    pc += 1;
                }

                // FP arithmetic — `Op::Add/Sub/Mul/Div/Neg` typed `Float`/
                // `Double`. The lowerer dispatches on the node type to the XMM
                // scalar form (`addss`/`addsd`/…). FP `Div` has NO div-zero /
                // overflow guard (IEEE division by zero is `±inf`/`NaN`, never an
                // exception), so unlike int `Div` it never deopts. `frem`/`drem`
                // (0x72/0x73) — JVM FP remainder is `fmod`-style with no single
                // SSE instruction, so the lowerer emits a `CALL` to the
                // `jit_frem`/`jit_drem` runtime helper (Slice A); the builder
                // just produces `Op::Rem` typed `Float`/`Double`.
                // fadd / dadd
                0x62 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Add, IrType::Float, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                0x63 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Add, IrType::Double, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // fsub / dsub
                0x66 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Sub, IrType::Float, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                0x67 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Sub, IrType::Double, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // fmul / dmul
                0x6a => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Mul, IrType::Float, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                0x6b => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Mul, IrType::Double, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // fdiv / ddiv
                0x6e => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Div, IrType::Float, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                0x6f => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Div, IrType::Double, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // frem / drem — `fmod`-style remainder lowered via the
                // jit_frem/jit_drem helper call (no single SSE instruction).
                0x72 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Rem, IrType::Float, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                0x73 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Rem, IrType::Double, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // fneg / dneg
                0x76 => {
                    let a = self.pop();
                    let r = self.add_data(Op::Neg, IrType::Float, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                0x77 => {
                    let a = self.pop();
                    let r = self.add_data(Op::Neg, IrType::Double, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }

                // FP ⇄ integer / FP ⇄ FP conversions. All are pure single-input
                // `Op` variants the lowerer maps to a `cvt*` XMM instruction.
                // i2f / i2d
                0x86 => {
                    let a = self.pop();
                    let r = self.add_data(Op::I2F, IrType::Float, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                0x87 => {
                    let a = self.pop();
                    let r = self.add_data(Op::I2D, IrType::Double, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                // l2f / l2d
                0x89 => {
                    let a = self.pop();
                    let r = self.add_data(Op::L2F, IrType::Float, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                0x8a => {
                    let a = self.pop();
                    let r = self.add_data(Op::L2D, IrType::Double, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                // f2i / f2l / f2d
                0x8b => {
                    let a = self.pop();
                    let r = self.add_data(Op::F2I, IrType::Int, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                0x8c => {
                    let a = self.pop();
                    let r = self.add_data(Op::F2L, IrType::Long, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                0x8d => {
                    let a = self.pop();
                    let r = self.add_data(Op::F2D, IrType::Double, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                // d2i / d2l / d2f
                0x8e => {
                    let a = self.pop();
                    let r = self.add_data(Op::D2I, IrType::Int, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                0x8f => {
                    let a = self.pop();
                    let r = self.add_data(Op::D2L, IrType::Long, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }
                0x90 => {
                    let a = self.pop();
                    let r = self.add_data(Op::D2F, IrType::Float, vec![a], pc);
                    self.push(r);
                    pc += 1;
                }

                // faload / daload — FP array element load (Slice B). The index
                // is a runtime value, so the lowerer emits the JVMS null +
                // bounds checks (deopt-on-fault) before the element access. The
                // load advances the memory token (a later store to a possibly-
                // aliasing element must be ordered after this read).
                0x30 => {
                    let index = self.pop();
                    let array = self.pop();
                    let load = self.graph.add(
                        Op::ArrayLoad(MemKind::Float),
                        IrType::Float,
                        vec![self.ctrl, self.mem, array, index],
                        Some(pc),
                    );
                    self.mem = load;
                    self.push(load);
                    pc += 1;
                }
                0x31 => {
                    let index = self.pop();
                    let array = self.pop();
                    let load = self.graph.add(
                        Op::ArrayLoad(MemKind::Double),
                        IrType::Double,
                        vec![self.ctrl, self.mem, array, index],
                        Some(pc),
                    );
                    self.mem = load;
                    self.push(load);
                    pc += 1;
                }
                // fastore / dastore — FP array element store (Slice B). Consumes
                // the memory token and produces a new one, so the scheduler
                // serialises it against neighbouring memory ops.
                0x51 => {
                    let value = self.pop();
                    let index = self.pop();
                    let array = self.pop();
                    let store = self.graph.add(
                        Op::ArrayStore(MemKind::Float),
                        IrType::Memory,
                        vec![self.ctrl, self.mem, array, index, value],
                        Some(pc),
                    );
                    self.mem = store;
                    pc += 1;
                }
                0x52 => {
                    let value = self.pop();
                    let index = self.pop();
                    let array = self.pop();
                    let store = self.graph.add(
                        Op::ArrayStore(MemKind::Double),
                        IrType::Memory,
                        vec![self.ctrl, self.mem, array, index, value],
                        Some(pc),
                    );
                    self.mem = store;
                    pc += 1;
                }
                // getfield — read an instance field as an `Op::Load`.
                //
                // Slice 1 (read-only) of the field/call IR frontier: only
                // int-category fields (`I`/`Z`/`B`/`C`/`S`) are lowered. They
                // all read the 32-bit `Value::Int` payload sign-extended — the
                // exact ABI the single-pass backend's inline getfield emits
                // (`MOVSXD` from `HEADER_SIZE + field_index*SLOT_SIZE +
                // FIELD_CELL_PAYLOAD32_OFFSET`). The cell-offset operand is a
                // `Const(field_index)`; the lowerer derives the byte
                // displacement. Float/long/double/reference fields and any pc
                // without resolved layout bail (`None` → single-pass), as does
                // every `putfield` (no IR `Op::Store` lowering yet — writes
                // need scheduler memory ordering, a separate slice).
                0xb4 => {
                    // Compact reference-field layout packs field offsets (refs to
                    // 8 bytes), so the `HEADER_SIZE + field_index*SLOT_SIZE`
                    // displacement this lowerer derives is wrong. Bail to the
                    // compact-aware single-pass helper path.
                    if cratonvm_types::compact_ref_fields_enabled() {
                        return None;
                    }
                    let (field_index, type_tag) = match self.field_info.get(&pc) {
                        Some(&fi) => fi,
                        None => return None,
                    };
                    if !matches!(type_tag, b'I' | b'Z' | b'B' | b'C' | b'S') {
                        return None;
                    }
                    let base = self.pop();
                    let offset = self.iconst(field_index as i64);
                    let load = self.graph.add(
                        Op::Load(MemKind::Int),
                        IrType::Int,
                        vec![self.ctrl, self.mem, base, offset],
                        Some(pc),
                    );
                    // The load advances the memory token: a subsequent store to
                    // a possibly-aliasing location must be ordered AFTER this
                    // read (WAR), and the scheduler enforces ordering only via
                    // the input-edge dependency this creates. (For a getfield-
                    // only method this merely serialises reads — harmless.)
                    self.mem = load;
                    self.push(load);
                    pc += 3;
                }
                // putfield — write an instance field as an `Op::Store`.
                //
                // Slice 2 (read/write) of the field/call IR frontier: only
                // int-category fields lower. The store consumes the current
                // memory token and produces a new one (the store node itself),
                // so the scheduler serialises it after every prior memory op and
                // before every later one (RAW/WAR/WAW all preserved by the
                // input-edge topological sort). Non-int fields and any pc without
                // resolved layout bail (`None` → single-pass).
                0xb5 => {
                    // See the getfield (0xb4) note: compact layout packs offsets,
                    // so bail to single-pass.
                    if cratonvm_types::compact_ref_fields_enabled() {
                        return None;
                    }
                    let (field_index, type_tag) = match self.field_info.get(&pc) {
                        Some(&fi) => fi,
                        None => return None,
                    };
                    if !matches!(type_tag, b'I' | b'Z' | b'B' | b'C' | b'S') {
                        return None;
                    }
                    let value = self.pop();
                    let base = self.pop();
                    let offset = self.iconst(field_index as i64);
                    let store = self.graph.add(
                        Op::Store(MemKind::Int),
                        IrType::Memory,
                        vec![self.ctrl, self.mem, base, offset, value],
                        Some(pc),
                    );
                    self.mem = store;
                    pc += 3;
                }
                // new — allocate an object as an `Op::New`. Emitted so escape
                // analysis can scalar-replace it when it does not escape (no
                // heap allocation, fields become SSA values). The lowerer has
                // no allocation path, so an `Op::New` that SURVIVES escape
                // analysis (escaping) makes the whole compile bail to
                // single-pass — enforced by the caller after `optimize`.
                0xbb => {
                    // Compact layout sizes objects from the packed body, not
                    // `num_fields * SLOT_SIZE`; bail to the compact-aware
                    // single-pass `jit_new_object` helper path.
                    if cratonvm_types::compact_ref_fields_enabled() {
                        return None;
                    }
                    let (class_id, num_fields) = match self.new_info.get(&pc) {
                        Some(&ci) => ci,
                        None => return None,
                    };
                    let newobj = self.graph.add(
                        Op::New {
                            class_id,
                            num_fields,
                        },
                        IrType::Ref,
                        vec![self.ctrl, self.mem],
                        Some(pc),
                    );
                    self.push(newobj);
                    pc += 3;
                }
                // invokespecial — only a trivial `<init>()V` on a fresh object
                // is handled, by ELISION: pop the receiver (the `dup`'d new
                // object) and emit nothing. Sound only because the caller
                // admits the pc to `trivial_init_pcs` exclusively when the
                // object is a non-escaping `new` whose class has no primitive
                // field initialisers (its fields are zero-initialised and set
                // by the visible `putfield`s — the constructor adds nothing the
                // scalar-replaced slots don't already model). Any other
                // `invokespecial` bails to single-pass.
                0xb7 => {
                    // inc 24 (Gap B): a resolved non-`<init>` `invokespecial`
                    // lowered to `Op::Call` — identical to the `invokestatic`
                    // arm except the receiver is arg0 (the caller's
                    // `invoke_info` entry already counts it in `num_args`, and
                    // the leaked `JitInvokeInfo` carries `invoke_kind == 1` so
                    // `invoke_dispatch` does the non-virtual dispatch to the
                    // statically-resolved target). GC-safe by the same
                    // conservative IR-frame scan that roots reference args
                    // (inc 22). Only populated when the special-call gate is on;
                    // otherwise `invoke_info` has no entry for this pc and we
                    // fall through to the elidable-`<init>` path below.
                    if let Some(&(info_ptr, num_args, ret_type)) = self.invoke_info.get(&pc) {
                        let mut args = Vec::with_capacity(num_args);
                        for _ in 0..num_args {
                            args.push(self.pop());
                        }
                        args.reverse();
                        let mut inputs = Vec::with_capacity(2 + num_args);
                        inputs.push(self.ctrl);
                        inputs.push(self.mem);
                        inputs.extend(args);
                        let returns_value = ret_type != b'V';
                        let ty = match ret_type {
                            b'V' => IrType::Void,
                            b'L' | b'[' => IrType::Ref,
                            // A `J` (long) return is a 64-bit value node so
                            // downstream category-2 ops (lstore/lreturn/ladd/…)
                            // type-check; `static_call_shape` only admits `J`
                            // returns once the i64::MIN-sentinel collision is
                            // disambiguated at the call site (see `Op::Call`
                            // lowering). `D`/`F` returns stay rejected by the
                            // shape gate until the XMM value tier exists.
                            b'J' => IrType::Long,
                            // A `D` (double) return is a 64-bit value node (inc
                            // 32); admitted by `static_call_shape`, the call-site
                            // sentinel check already disambiguates it.
                            b'D' => IrType::Double,
                            // A `F` (float) return is a 32-bit value node (inc 33).
                            b'F' => IrType::Float,
                            _ => IrType::Int,
                        };
                        let call = self.graph.add(Op::Call { info_ptr }, ty, inputs, Some(pc));
                        self.mem = call;
                        if returns_value {
                            self.push(call);
                        }
                        pc += 3;
                    } else {
                        // Elidable-`<init>` path (scalar-new): elide a trivial
                        // `<init>()V` on a fresh object.
                        if !self.trivial_init_pcs.contains(&pc) {
                            return None;
                        }
                        // Defence in depth: only elide when the receiver (top of
                        // stack for a no-arg `<init>`) is a fresh `Op::New` we
                        // emitted. Eliding a `<init>` whose receiver is `this` or
                        // a parameter would skip a real superclass constructor
                        // (and hide any escape it performs).
                        let recv = self.peek();
                        let recv_is_new = recv != NO_NODE
                            && matches!(
                                self.graph.nodes.get(recv as usize).map(|n| &n.op),
                                Some(Op::New { .. })
                            );
                        if !recv_is_new {
                            return None;
                        }
                        self.pop();
                        pc += 3;
                    }
                }
                // invokestatic (0xb8) and invokevirtual (0xb6) — lower a resolved
                // call to `Op::Call` (Gap B). `invokestatic` has no receiver;
                // `invokevirtual` marshals the receiver as arg0 (the caller's
                // `invoke_info` entry already counts it in `num_args`, and the
                // leaked `JitInvokeInfo` carries `invoke_kind == 0` so
                // `invoke_dispatch` resolves the actual target on the receiver's
                // runtime class — full virtual dispatch through the generic helper,
                // no inline cache in the emitted code). GC-safe by the same
                // conservative IR-frame scan that roots reference args (inc 22).
                // Both are 3-byte instructions. A pc not in `invoke_info` (gate
                // off, or a non-emittable invoke present) bails to single-pass.
                0xb6 | 0xb8 => {
                    // Scalarize the fixed erased adapter in Dist's private
                    // numeric kernels before it creates Integer/Double boxes.
                    if op == 0xb8
                        && self.tdigest_scalar_kernel
                        && pc + 14 <= code.len()
                        && code[pc + 3] == 0xb9
                        && code[pc + 8] == 0xc0
                        && code[pc + 11] == 0xb6
                    {
                        if self.stack.len() >= 2 {
                            let index = self.pop();
                            let lambda = self.pop();
                            let inputs = vec![self.ctrl, self.mem, lambda, index];
                            let call = self.graph.add(
                                Op::LambdaIntToDouble,
                                IrType::Double,
                                inputs,
                                Some(pc),
                            );
                            self.mem = call;
                            self.push(call);
                            pc += 14;
                            continue;
                        }
                    }
                    let (info_ptr, num_args, ret_type) = match self.invoke_info.get(&pc) {
                        Some(&t) => t,
                        None => return None,
                    };
                    // Pop args (deepest-first on the abstract stack) and restore
                    // source order so inputs are [ctrl, mem, arg0, arg1, …].
                    let mut args = Vec::with_capacity(num_args);
                    for _ in 0..num_args {
                        args.push(self.pop());
                    }
                    args.reverse();
                    let mut inputs = Vec::with_capacity(2 + num_args);
                    inputs.push(self.ctrl);
                    inputs.push(self.mem);
                    inputs.extend(args);
                    // A call is a hard memory barrier: it consumes the current
                    // memory token and BECOMES the new one (serialising every
                    // prior memory op before it and every later one after). The
                    // same node also carries the return value (like `Op::Load`).
                    // Result type from the descriptor: `V` → no value; `L`/`[` →
                    // a reference result (`IrType::Ref`); else int-category.
                    let returns_value = ret_type != b'V';
                    let ty = match ret_type {
                        b'V' => IrType::Void,
                        b'L' | b'[' => IrType::Ref,
                        // A `J` (long) return is a 64-bit value node so
                        // downstream category-2 ops type-check; `static_call_shape`
                        // only admits `J` returns once the i64::MIN-sentinel
                        // collision is disambiguated at the call site.
                        b'J' => IrType::Long,
                        // A `D` (double) return is a 64-bit value node (inc 32).
                        b'D' => IrType::Double,
                        // A `F` (float) return is a 32-bit value node (inc 33).
                        b'F' => IrType::Float,
                        _ => IrType::Int,
                    };
                    let call = self.graph.add(Op::Call { info_ptr }, ty, inputs, Some(pc));
                    self.mem = call;
                    if returns_value {
                        self.push(call);
                    }
                    pc += 3;
                }
                // invokeinterface (0xb9) — identical to the invokevirtual path
                // (receiver arg0, `invoke_kind == 2`, dynamic dispatch via the
                // helper) except it is a FIVE-byte instruction (opcode, cp_hi,
                // cp_lo, count, 0). The trailing count/0 bytes are not consumed by
                // the builder (the descriptor was resolved from the cp index by
                // the caller); only the pc advance differs.
                0xb9 => {
                    let (info_ptr, num_args, ret_type) = match self.invoke_info.get(&pc) {
                        Some(&t) => t,
                        None => return None,
                    };
                    let mut args = Vec::with_capacity(num_args);
                    for _ in 0..num_args {
                        args.push(self.pop());
                    }
                    args.reverse();
                    let mut inputs = Vec::with_capacity(2 + num_args);
                    inputs.push(self.ctrl);
                    inputs.push(self.mem);
                    inputs.extend(args);
                    let returns_value = ret_type != b'V';
                    let ty = match ret_type {
                        b'V' => IrType::Void,
                        b'L' | b'[' => IrType::Ref,
                        // A `J` (long) return is a 64-bit value node so
                        // downstream category-2 ops type-check; `static_call_shape`
                        // only admits `J` returns once the i64::MIN-sentinel
                        // collision is disambiguated at the call site.
                        b'J' => IrType::Long,
                        // A `D` (double) return is a 64-bit value node (inc 32).
                        b'D' => IrType::Double,
                        // A `F` (float) return is a 32-bit value node (inc 33).
                        b'F' => IrType::Float,
                        _ => IrType::Int,
                    };
                    let call = self.graph.add(Op::Call { info_ptr }, ty, inputs, Some(pc));
                    self.mem = call;
                    if returns_value {
                        self.push(call);
                    }
                    pc += 5;
                }
                // dup
                0x59 => {
                    let top = self.peek();
                    self.push(top);
                    pc += 1;
                }
                // pop
                0x57 => {
                    self.pop();
                    pc += 1;
                }

                // ifeq..ifle (0x99..0x9e) — compare int against zero
                0x99..=0x9e => {
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
                    let target_pc = (pc as i32 + offset) as usize;
                    let val = self.pop();
                    let zero = self.iconst(0);
                    let cc = match op {
                        0x99 => CmpOp::Eq,
                        0x9a => CmpOp::Ne,
                        0x9b => CmpOp::Lt,
                        0x9c => CmpOp::Ge,
                        0x9d => CmpOp::Gt,
                        0x9e => CmpOp::Le,
                        _ => unreachable!(),
                    };
                    let cmp = self.add_data(Op::Cmp(cc), IrType::Int, vec![val, zero], pc);
                    let if_node =
                        self.graph
                            .add(Op::If, IrType::Control, vec![self.ctrl, cmp], Some(pc));
                    let true_ctrl =
                        self.graph
                            .add(Op::Proj(0), IrType::Control, vec![if_node], Some(pc));
                    let false_ctrl =
                        self.graph
                            .add(Op::Proj(1), IrType::Control, vec![if_node], Some(pc));

                    // True edge → target
                    let _saved_ctrl = self.ctrl;
                    let saved_locals = self.locals.clone();
                    let saved_stack = self.stack.clone();
                    let saved_mem = self.mem;

                    self.ctrl = true_ctrl;
                    self.add_merge_predecessor(target_pc);

                    // False edge → fall-through
                    self.ctrl = false_ctrl;
                    self.locals = saved_locals;
                    self.stack = saved_stack;
                    self.mem = saved_mem;

                    pc += 3;
                }

                // if_icmpeq..if_icmple (0x9f..0xa4)
                0x9f..=0xa4 => {
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
                    let target_pc = (pc as i32 + offset) as usize;
                    let b = self.pop();
                    let a = self.pop();
                    let cc = match op {
                        0x9f => CmpOp::Eq,
                        0xa0 => CmpOp::Ne,
                        0xa1 => CmpOp::Lt,
                        0xa2 => CmpOp::Ge,
                        0xa3 => CmpOp::Gt,
                        0xa4 => CmpOp::Le,
                        _ => unreachable!(),
                    };
                    let cmp = self.add_data(Op::Cmp(cc), IrType::Int, vec![a, b], pc);
                    let if_node =
                        self.graph
                            .add(Op::If, IrType::Control, vec![self.ctrl, cmp], Some(pc));
                    let true_ctrl =
                        self.graph
                            .add(Op::Proj(0), IrType::Control, vec![if_node], Some(pc));
                    let false_ctrl =
                        self.graph
                            .add(Op::Proj(1), IrType::Control, vec![if_node], Some(pc));

                    let saved_locals = self.locals.clone();
                    let saved_stack = self.stack.clone();
                    let saved_mem = self.mem;

                    self.ctrl = true_ctrl;
                    self.add_merge_predecessor(target_pc);

                    self.ctrl = false_ctrl;
                    self.locals = saved_locals;
                    self.stack = saved_stack;
                    self.mem = saved_mem;

                    pc += 3;
                }

                // goto
                0xa7 => {
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
                    let target_pc = (pc as i32 + offset) as usize;
                    self.add_merge_predecessor(target_pc);
                    self.ctrl = NO_NODE; // dead after unconditional jump
                    pc += 3;
                }

                // ireturn
                //
                // A method may have several `return` statements, so this builds
                // one `Op::Return` terminator per `ireturn`. `graph.exit` records
                // only the LAST one (each `ireturn` overwrites it) — it is an
                // "an exit" marker, NOT the sole exit. Consumers that need every
                // exit must enumerate all `Op::Return` nodes: the scheduler does
                // (it blocks every control node), and `eliminate_dead_nodes`
                // roots from ALL returns (rooting only from `graph.exit` would
                // delete the other return paths — see its comment).
                0xac => {
                    let val = self.pop();
                    let ret =
                        self.graph
                            .add(Op::Return, IrType::Void, vec![self.ctrl, val], Some(pc));
                    self.graph.exit = ret;
                    self.ctrl = NO_NODE;
                    pc += 1;
                }

                // lreturn
                0xad => {
                    let val = self.pop();
                    let ret =
                        self.graph
                            .add(Op::Return, IrType::Void, vec![self.ctrl, val], Some(pc));
                    self.graph.exit = ret;
                    self.ctrl = NO_NODE;
                    pc += 1;
                }

                // freturn (inc 33) / dreturn (inc 32) — an FP return rides RAX as
                // bits (the JIT i64 return ABI): the interpreter's post-JIT path
                // reads `result as u64`→`f64::from_bits` (`D`) or `result as u32`
                // →`f32::from_bits` (`F`), and `lower_terminator`'s `Op::Return`
                // does `load_to_rax` from the value's slot, so no XMM return
                // marshalling is needed. Identical to `lreturn`/`ireturn`.
                //
                // `float` (inc 33): the slot holds 32 bits; `load_to_rax` reads 64,
                // so RAX's UPPER 32 are stale — harmless because every consumer
                // reads only the low 32 (`result as u32`; downstream `MOVSS`). The
                // one hazard is a `+0.0f` whose stale upper bits make RAX ==
                // `i64::MIN`: on a `F` CALL result the call-site `dispatch_threw`
                // peek (extended to `IrType::Float`) disambiguates it, and the
                // restore (RAX=`i64::MIN`, low-32=0) preserves `+0.0f` — `i64::MIN`
                // has low-63 = 0, so only `+0.0f` can ever collide.
                //
                // Reachable only under the FP gate; the `-0.0`/`Long.MIN_VALUE` ↔
                // `i64::MIN` collision on a `D`/`F` call result is handled by the
                // item-1 `dispatch_threw` peek.
                0xae | 0xaf => {
                    let val = self.pop();
                    let ret =
                        self.graph
                            .add(Op::Return, IrType::Void, vec![self.ctrl, val], Some(pc));
                    self.graph.exit = ret;
                    self.ctrl = NO_NODE;
                    pc += 1;
                }

                // return (void)
                0xb1 => {
                    let ret = self
                        .graph
                        .add(Op::Return, IrType::Void, vec![self.ctrl], Some(pc));
                    self.graph.exit = ret;
                    self.ctrl = NO_NODE;
                    pc += 1;
                }

                // lconst_0, lconst_1
                0x09 => {
                    let c = self.lconst(0);
                    self.push(c);
                    pc += 1;
                }
                0x0a => {
                    let c = self.lconst(1);
                    self.push(c);
                    pc += 1;
                }
                // ldc2_w (long OR double constant from the constant pool) — inc 26
                // (long) / inc 35 (double). The resolved `(bits, is_double)` comes
                // from `set_ldc2w_info`; an absent pc bails to single-pass. A
                // `double` constant lowers to `dconst` (`Op::ConstF`/`Double`), a
                // `long` to `lconst` (`Op::Const`/`Long`). (`ldc2_w` is 3 bytes:
                // opcode + 2-byte CP index.)
                0x14 => {
                    let (val, is_double) = match self.ldc2w_info.get(&pc) {
                        Some(&v) => v,
                        None => return None,
                    };
                    let c = if is_double {
                        self.dconst(f64::from_bits(val as u64))
                    } else {
                        self.lconst(val)
                    };
                    self.push(c);
                    pc += 3;
                }

                // tableswitch / lookupswitch — lower as a CMP-equality chain
                // (the same shape the single-pass backend emits): each case is
                // `if (key == match) goto target`, the unmatched edge falls
                // through to the next comparison, and the final unmatched edge
                // goes to the default target. Reuses the existing If/Cmp/merge
                // machinery — no dedicated multi-way node.
                0xaa | 0xab => {
                    let (len, default_target, cases) = parse_switch(code, code_len, pc)?;
                    let key = self.pop();
                    for (match_val, target) in &cases {
                        let cval = self.iconst(*match_val as i64);
                        let cmp =
                            self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![key, cval], pc);
                        let if_node =
                            self.graph
                                .add(Op::If, IrType::Control, vec![self.ctrl, cmp], Some(pc));
                        let true_ctrl =
                            self.graph
                                .add(Op::Proj(0), IrType::Control, vec![if_node], Some(pc));
                        let false_ctrl =
                            self.graph
                                .add(Op::Proj(1), IrType::Control, vec![if_node], Some(pc));
                        // Matched edge → the case target. The abstract state is
                        // invariant across the chain (the switch consumed only
                        // `key`), so each predecessor snapshot is correct.
                        self.ctrl = true_ctrl;
                        self.add_merge_predecessor(*target);
                        // Unmatched edge → next comparison.
                        self.ctrl = false_ctrl;
                    }
                    // All comparisons failed → default target.
                    self.add_merge_predecessor(default_target);
                    self.ctrl = NO_NODE;
                    pc += len;
                }

                // Unsupported opcode — bail out
                _ => return None,
            }
        }

        Some(self.graph)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Parse a `tableswitch` (0xaa) / `lookupswitch` (0xab) at opcode offset
/// `op_pc`. Returns `(instruction_len, default_target, cases)` where each case
/// is `(match_value, target_pc)`; all targets are absolute bytecode offsets
/// (JVMS switch branch offsets are relative to the opcode pc). Returns `None`
/// if the table is malformed, exceeds the dense/sparse caps shared with the
/// single-pass scanner, or any target is out of range — callers treat that as
/// "not IR-compilable" (the single-pass backend still handles it).
fn parse_switch(
    code: &[u8],
    code_len: usize,
    op_pc: usize,
) -> Option<(usize, usize, Vec<(i32, usize)>)> {
    let op = *code.get(op_pc)?;
    // 0-3 bytes of padding so the table starts 4-byte aligned from method start.
    let mut p = op_pc + 1;
    while p % 4 != 0 {
        p += 1;
    }
    let read_i32 =
        |at: usize| i32::from_be_bytes([code[at], code[at + 1], code[at + 2], code[at + 3]]);
    let target_of = |off: i32| -> Option<usize> {
        let t = op_pc as i64 + off as i64;
        (t >= 0 && (t as usize) < code_len).then_some(t as usize)
    };
    match op {
        0xaa => {
            if p + 12 > code_len {
                return None;
            }
            let default = read_i32(p);
            let low = read_i32(p + 4);
            let high = read_i32(p + 8);
            let num = crate::x64::checked_tableswitch_count(low, high)?;
            let end = p + 12 + num * 4;
            if end > code_len {
                return None;
            }
            let mut cases = Vec::with_capacity(num);
            for i in 0..num {
                let off = read_i32(p + 12 + i * 4);
                cases.push((low + i as i32, target_of(off)?));
            }
            Some((end - op_pc, target_of(default)?, cases))
        }
        0xab => {
            if p + 8 > code_len {
                return None;
            }
            let default = read_i32(p);
            let npairs = crate::x64::checked_lookupswitch_npairs(read_i32(p + 4))?;
            let end = p + 8 + npairs * 8;
            if end > code_len {
                return None;
            }
            let mut cases = Vec::with_capacity(npairs);
            for i in 0..npairs {
                let pair = p + 8 + i * 8;
                cases.push((read_i32(pair), target_of(read_i32(pair + 4))?));
            }
            Some((end - op_pc, target_of(default)?, cases))
        }
        _ => None,
    }
}

/// Scan bytecode for branch targets (PCs that are jumped to).
fn find_branch_targets(code: &[u8], code_len: usize) -> Vec<usize> {
    let mut targets = Vec::new();
    let mut pc = 0;
    while pc < code_len {
        let op = code[pc];
        match op {
            // ifeq..ifle, if_icmpeq..if_icmple
            0x99..=0xa4 => {
                if pc + 2 < code_len {
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
                    let target_i32 = pc as i32 + offset;
                    // Validate: target must be non-negative and within code bounds.
                    if target_i32 >= 0 && (target_i32 as usize) < code_len {
                        targets.push(target_i32 as usize);
                    }
                    // Also the fall-through PC after the branch
                    if pc + 3 < code_len {
                        targets.push(pc + 3);
                    }
                }
                pc += 3;
            }
            // goto
            0xa7 => {
                if pc + 2 < code_len {
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
                    let target_i32 = pc as i32 + offset;
                    if target_i32 >= 0 && (target_i32 as usize) < code_len {
                        targets.push(target_i32 as usize);
                    }
                }
                pc += 3;
            }
            // tableswitch / lookupswitch — register the default + every case
            // target (each case body is a merge target). A malformed switch is
            // left for the builder to bail on; just step past the opcode.
            0xaa | 0xab => {
                if let Some((len, default_target, cases)) = parse_switch(code, code_len, pc) {
                    targets.push(default_target);
                    for (_, target) in cases {
                        targets.push(target);
                    }
                    pc += len;
                } else {
                    pc += 1;
                }
            }
            // 1-byte opcodes
            0x02..=0x0a
            | 0x1a..=0x21
            | 0x3b..=0x42
            | 0x57
            | 0x59
            | 0x60
            | 0x61
            | 0x64
            | 0x65
            | 0x68
            | 0x69
            | 0x6c
            | 0x70
            | 0x74
            | 0x75
            | 0x78
            | 0x7a
            | 0x7c
            | 0x7e
            | 0x80
            | 0x82
            | 0x85
            | 0x88
            | 0x91..=0x93
            | 0x2a..=0x2d
            | 0x4b..=0x4e
            | 0xac
            | 0xad
            | 0xb1 => {
                pc += 1;
            }
            // 2-byte opcodes (inc 26: + 0x16 lload, 0x37 lstore; inc 30: + the
            // wide FP load/store forms 0x17 fload, 0x18 dload, 0x38 fstore,
            // 0x39 dstore — each is opcode + 1-byte local index).
            0x10 | 0x15 | 0x16 | 0x17 | 0x18 | 0x19 | 0x36 | 0x37 | 0x38 | 0x39 | 0x3a => {
                pc += 2;
            }
            // 3-byte opcodes (the 3-byte method invokes: invokevirtual 0xb6,
            // invokespecial 0xb7, invokestatic 0xb8; inc 26: + 0x14 ldc2_w)
            0x11 | 0x14 | 0x84 | 0xb4 | 0xb5 | 0xb6 | 0xb7 | 0xb8 | 0xbb => {
                pc += 3;
            }
            // 5-byte opcodes: invokeinterface (0xb9) — opcode, cp_hi, cp_lo,
            // count, 0. invokedynamic (0xba) — opcode, cp_hi, cp_lo, 0, 0. The
            // trailing operand bytes MUST be skipped or a later branch target
            // would be mis-located (the builder lowers 0xb9; it bails cleanly
            // on 0xba via the main loop's `_ => return None`, but this PRE-scan
            // must still step over 0xba's operand bytes correctly — otherwise
            // it misreads them as up to 4 phantom opcodes, which can fabricate
            // or miss real branch targets before the main loop ever reaches the
            // real 0xba byte and bails. `jit_scan` no longer rejects 0xba
            // upstream, so this function must handle it explicitly rather than
            // falling into the "unknown opcode" catch-all below.
            0xb9 | 0xba => {
                pc += 5;
            }
            _ => {
                // Unknown opcode — skip (builder will also bail)
                pc += 1;
            }
        }
    }
    targets.sort_unstable();
    targets.dedup();
    targets
}

/// Identify loop headers: bytecode PCs reached by a *backward* branch
/// (`target <= source`). These must be activated with eager loop-carried phis
/// (see `activate_loop_header`) so the back-edge value can be back-patched.
/// Mirrors `find_branch_targets`' opcode-length walk exactly.
fn find_loop_headers(code: &[u8], code_len: usize) -> HashSet<usize> {
    let mut headers = HashSet::new();
    let mut pc = 0;
    while pc < code_len {
        let op = code[pc];
        match op {
            // ifeq..ifle, if_icmpeq..if_icmple, goto — all 2-byte signed offset.
            0x99..=0xa4 | 0xa7 => {
                if pc + 2 < code_len {
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
                    let target = pc as i32 + offset;
                    if target >= 0 && (target as usize) <= pc && (target as usize) < code_len {
                        headers.insert(target as usize);
                    }
                }
                pc += 3;
            }
            // tableswitch / lookupswitch — a backward case/default target is a
            // loop header (same rule as a backward branch).
            0xaa | 0xab => {
                if let Some((len, default_target, cases)) = parse_switch(code, code_len, pc) {
                    if default_target <= pc {
                        headers.insert(default_target);
                    }
                    for (_, target) in cases {
                        if target <= pc {
                            headers.insert(target);
                        }
                    }
                    pc += len;
                } else {
                    pc += 1;
                }
            }
            // 1-byte opcodes
            0x02..=0x0a
            | 0x1a..=0x21
            | 0x3b..=0x42
            | 0x57
            | 0x59
            | 0x60
            | 0x61
            | 0x64
            | 0x65
            | 0x68
            | 0x69
            | 0x6c
            | 0x70
            | 0x74
            | 0x75
            | 0x78
            | 0x7a
            | 0x7c
            | 0x7e
            | 0x80
            | 0x82
            | 0x85
            | 0x88
            | 0x91..=0x93
            | 0x2a..=0x2d
            | 0x4b..=0x4e
            | 0xac
            | 0xad
            | 0xb1 => pc += 1,
            // 2-byte opcodes (inc 26: + 0x16 lload, 0x37 lstore; inc 30: + the
            // wide FP load/store forms 0x17 fload, 0x18 dload, 0x38 fstore,
            // 0x39 dstore).
            0x10 | 0x15 | 0x16 | 0x17 | 0x18 | 0x19 | 0x36 | 0x37 | 0x38 | 0x39 | 0x3a => pc += 2,
            // 3-byte opcodes (invokevirtual 0xb6 / invokespecial 0xb7 /
            // invokestatic 0xb8 — the 3-byte method invokes; inc 26: + 0x14 ldc2_w)
            0x11 | 0x14 | 0x84 | 0xb4 | 0xb5 | 0xb6 | 0xb7 | 0xb8 | 0xbb => pc += 3,
            // 5-byte: invokeinterface (0xb9) — skip its count/0 trailer so a
            // backward branch target after it is located correctly.
            // invokedynamic (0xba) — same 5-byte shape (opcode, cp_hi, cp_lo,
            // 0, 0); must be stepped explicitly for the same reason as
            // `find_branch_targets` above — `jit_scan` no longer rejects it
            // upstream, so falling into the 1-byte catch-all would desync
            // every subsequent PC in this pre-scan.
            0xb9 | 0xba => pc += 5,
            _ => pc += 1,
        }
    }
    headers
}

/// Check if a method (from its JitScanResult) is suitable for IR compilation.
///
/// STUB-S7 widening: previously this rejected every method with any heap op,
/// invoke, or allocation — which excluded virtually all real-world methods and
/// left the IR-resident optimization passes (escape analysis, GVN, fold,
/// schedule, lower) effectively unreachable. We now admit methods with a
/// bounded number of common heap-touching opcodes; the IR builder itself
/// returns `None` when it encounters an opcode it doesn't yet lower, and the
/// caller in `lib.rs` (`try_compile`) falls through to the x64 single-pass
/// backend on either `build()` or `lower()` returning `None`. That fallback is
/// the safety net that lets us widen gradually without crashing.
///
/// `jit_scan` no longer rejects invokedynamic (0xba) upstream — it is
/// explicitly excluded here instead (see the `indy_ops` check below), since
/// the IR builder has no lowering for it (the main opcode loop bails via
/// `_ => return None`, same as any other unsupported opcode) and a method
/// containing one always falls back to the x64 single-pass backend, which DOES
/// lower it (unconditional deopt to `DeoptReason::UnreachedCode`). There is no
/// exception-table-aware IR codegen yet (STUB-S8) — the IR builder has no path
/// that constructs exception edges, so methods with try/catch are naturally
/// rejected either there or at the bytecode-parse level.
///
/// TODO(IR widening): increase caps once differential tests validate.
pub fn ir_compatible(scan: &super::x64::JitScanResult) -> bool {
    // Caps below are deliberately conservative. A method that exceeds a cap is
    // still compilable via the x64 single-pass backend; we just decline to
    // route it through the IR pipeline until we've gained more confidence.

    // RBC.6 — the IR pipeline has no athrow lowering; only the x64
    // single-pass backend emits the stash-pending-exception sequence.
    if scan.has_athrow {
        return false;
    }

    // invokedynamic-uncommon-trap fix: the IR builder has no lowering for
    // 0xba (it bails cleanly via the main loop's `_ => return None` if one
    // slips through), but decline it explicitly here rather than relying
    // solely on that later bail — this keeps the scope boundary self-
    // documenting and avoids running the (now 0xba-aware, but still only
    // IR-builder-adjacent) `find_branch_targets`/`find_loop_headers`
    // pre-scans on a method the IR path was never going to lower anyway.
    // Methods containing invokedynamic always fall back to the x64
    // single-pass backend, which lowers it directly.
    if !scan.indy_ops.is_empty() {
        return false;
    }

    // Cap simple invokes (invokestatic / invokevirtual / invokespecial /
    // invokeinterface). Invokedynamic is rejected upstream by `jit_scan`.
    if scan.invoke_ops.len() > 5 {
        return false;
    }
    // Cap getfield/putfield.
    if scan.field_ops.len() > 5 {
        return false;
    }
    // Cap getstatic/putstatic (same shape as instance field ops for IR).
    if scan.static_field_ops.len() > 5 {
        return false;
    }
    // Cap `new` allocations.
    if scan.new_ops.len() > 3 {
        return false;
    }
    // Cap `anewarray` allocations.
    if scan.anewarray_ops.len() > 3 {
        return false;
    }

    // Still rejected outright — IR has no lowering for these yet:
    //  * `multianewarray` — multi-dim allocation needs a resolver-shaped helper
    //    call sequence that the IR lowerer does not synthesize.
    //  * `checkcast` / `instanceof` — runtime type checks need polymorphic
    //    inline caches that the IR pipeline does not yet emit.
    if !scan.multianewarray_ops.is_empty() {
        return false;
    }
    if !scan.typecheck_ops.is_empty() {
        return false;
    }

    true
}

/// Variant of [`ir_compatible`] that also enforces a hard cap on bytecode
/// length. Kept as a separate entry point so the existing call site and tests
/// stay source-compatible while callers that know the method size can opt into
/// the extra guard.
///
/// TODO(IR widening): fold into `ir_compatible` once `try_compile` plumbs
/// `code_len` through and once we've validated larger methods.
pub fn ir_compatible_sized(scan: &super::x64::JitScanResult, code_len: usize) -> bool {
    if code_len > 200 {
        return false;
    }
    ir_compatible(scan)
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build IR from bytecode, panicking on failure.
    fn build_ir(code: &[u8], code_len: usize, num_params: usize, num_locals: usize) -> Graph {
        let builder = IrBuilder::new(num_params, num_locals);
        builder.build(code, code_len).expect("IR build failed")
    }

    #[test]
    fn test_ir_identity_function() {
        // int f(int x) { return x; }
        // iload_0; ireturn
        let code = [0x1a, 0xac, 0, 0];
        let graph = build_ir(&code, 2, 1, 1);
        // Should have: Start, Proj(0), Proj(1), Param(0), Return
        assert!(graph.exit != NO_NODE);
        assert_eq!(graph.nodes[graph.exit as usize].op, Op::Return);
        // Return's data input should be Param(0)
        let ret = &graph.nodes[graph.exit as usize];
        assert_eq!(ret.inputs.len(), 2); // [ctrl, value]
        let val_id = ret.inputs[1];
        assert_eq!(graph.nodes[val_id as usize].op, Op::Param(0));
    }

    #[test]
    fn test_ir_add_two_params() {
        // int f(int a, int b) { return a + b; }
        // iload_0; iload_1; iadd; ireturn
        let code = [0x1a, 0x1b, 0x60, 0xac, 0, 0];
        let graph = build_ir(&code, 4, 2, 2);
        let ret = &graph.nodes[graph.exit as usize];
        let add_id = ret.inputs[1];
        assert_eq!(graph.nodes[add_id as usize].op, Op::Add);
        let add = &graph.nodes[add_id as usize];
        assert_eq!(graph.nodes[add.inputs[0] as usize].op, Op::Param(0));
        assert_eq!(graph.nodes[add.inputs[1] as usize].op, Op::Param(1));
    }

    #[test]
    fn test_ir_constant_folding_candidates() {
        // int f() { return 3 + 4; }
        // iconst_3; iconst_4; iadd; ireturn
        let code = [0x06, 0x07, 0x60, 0xac, 0, 0];
        let graph = build_ir(&code, 4, 0, 0);
        let ret = &graph.nodes[graph.exit as usize];
        let add_id = ret.inputs[1];
        let add = &graph.nodes[add_id as usize];
        // Both inputs should be Const nodes
        assert!(matches!(
            graph.nodes[add.inputs[0] as usize].op,
            Op::Const(3)
        ));
        assert!(matches!(
            graph.nodes[add.inputs[1] as usize].op,
            Op::Const(4)
        ));
    }

    #[test]
    fn test_ir_branch_creates_merge() {
        // int f(int x) { if (x == 0) return 1; return 0; }
        // iload_0; ifeq +5; iconst_0; ireturn; iconst_1; ireturn
        //   0       1        4         5        6         7
        let code = [
            0x1a, // 0: iload_0
            0x99, 0x00, 0x05, // 1: ifeq → 6
            0x03, // 4: iconst_0
            0xac, // 5: ireturn
            0x04, // 6: iconst_1
            0xac, // 7: ireturn
            0, 0, // padding
        ];
        let graph = build_ir(&code, 8, 1, 1);
        // Should have Merge, If, Cmp nodes
        let has_if = graph.nodes.iter().any(|n| n.op == Op::If);
        assert!(has_if, "Should contain an If node");
        let has_cmp = graph.nodes.iter().any(|n| matches!(n.op, Op::Cmp(_)));
        assert!(has_cmp, "Should contain a Cmp node");
    }

    #[test]
    fn test_ir_getfield_emits_load() {
        // static int get(Corpus o) { return o.x; }
        // aload_0; getfield #2; ireturn
        let code = [0x2a, 0xb4, 0x00, 0x02, 0xac, 0, 0];
        let mut builder = IrBuilder::new(1, 1);
        let mut fi = HashMap::new();
        fi.insert(1usize, (0usize, b'I'));
        builder.set_field_info(fi);
        if cratonvm_types::compact_ref_fields_enabled() {
            assert!(
                builder.build(&code, 5).is_none(),
                "compact field layout must bail to the checked single-pass path"
            );
            return;
        }
        let graph = builder.build(&code, 5).expect("IR build failed");
        let has_load = graph.nodes.iter().any(|n| matches!(n.op, Op::Load(_)));
        assert!(has_load, "getfield should emit an Op::Load node");
        // The Load's base must be the Param(0) receiver and its offset operand a
        // Const(0) (field index 0).
        let load = graph
            .nodes
            .iter()
            .find(|n| matches!(n.op, Op::Load(_)))
            .unwrap();
        assert_eq!(
            load.inputs.len(),
            4,
            "Load inputs = [ctrl, mem, base, offset]"
        );
        assert_eq!(graph.nodes[load.inputs[2] as usize].op, Op::Param(0));
        assert_eq!(graph.nodes[load.inputs[3] as usize].op, Op::Const(0));
    }

    #[test]
    fn test_ir_putfield_emits_store_and_threads_memory() {
        // static void set(Corpus o, int v) { o.x = v; }
        // aload_0; iload_1; putfield #2; return
        let code = [0x2a, 0x1b, 0xb5, 0x00, 0x02, 0xb1, 0, 0];
        let mut builder = IrBuilder::new(2, 2);
        let mut fi = HashMap::new();
        fi.insert(2usize, (0usize, b'I'));
        builder.set_field_info(fi);
        if cratonvm_types::compact_ref_fields_enabled() {
            assert!(
                builder.build(&code, 6).is_none(),
                "compact field layout must bail to the checked single-pass path"
            );
            return;
        }
        let graph = builder.build(&code, 6).expect("IR build failed");
        let store = graph
            .nodes
            .iter()
            .find(|n| matches!(n.op, Op::Store(_)))
            .expect("putfield should emit an Op::Store");
        // inputs = [ctrl, mem, base, offset, value]
        assert_eq!(store.inputs.len(), 5);
        assert_eq!(graph.nodes[store.inputs[2] as usize].op, Op::Param(0)); // base = o
        assert_eq!(graph.nodes[store.inputs[3] as usize].op, Op::Const(0)); // field index
        assert_eq!(graph.nodes[store.inputs[4] as usize].op, Op::Param(1)); // value = v
        assert_eq!(store.ty, IrType::Memory);
    }

    #[test]
    fn test_ir_putfield_store_after_load_in_memory_chain() {
        // static int swap(Corpus o, int v) { int t = o.x; o.x = v; return t; }
        // The store's memory input must be the load (so the scheduler orders the
        // store AFTER the read — WAR), not the initial Start memory token.
        // aload_0; getfield #2; istore_2; aload_0; iload_1; putfield #2; iload_2; ireturn
        let code = [
            0x2a, 0xb4, 0x00, 0x02, 0x3d, 0x2a, 0x1b, 0xb5, 0x00, 0x02, 0x1c, 0xac, 0, 0,
        ];
        let mut builder = IrBuilder::new(2, 3);
        let mut fi = HashMap::new();
        fi.insert(1usize, (0usize, b'I')); // getfield pc 1
        fi.insert(7usize, (0usize, b'I')); // putfield pc 7
        builder.set_field_info(fi);
        if cratonvm_types::compact_ref_fields_enabled() {
            assert!(
                builder.build(&code, 12).is_none(),
                "compact field layout must bail to the checked single-pass path"
            );
            return;
        }
        let graph = builder.build(&code, 12).expect("IR build failed");
        let store = graph
            .nodes
            .iter()
            .find(|n| matches!(n.op, Op::Store(_)))
            .expect("putfield emits a Store");
        // store.inputs[1] (memory) must be the Load node, not the Start Proj(1).
        let mem_in = store.inputs[1];
        assert!(
            matches!(graph.nodes[mem_in as usize].op, Op::Load(_)),
            "store's memory input must be the prior load (WAR ordering), got {:?}",
            graph.nodes[mem_in as usize].op
        );
    }

    #[test]
    fn test_ir_getfield_bails_without_field_info() {
        // Same method but no field layout supplied → build must bail (None),
        // the single-pass safety net.
        let code = [0x2a, 0xb4, 0x00, 0x02, 0xac, 0, 0];
        let builder = IrBuilder::new(1, 1);
        assert!(builder.build(&code, 5).is_none());
    }

    #[test]
    fn test_ir_getfield_bails_on_non_int_field() {
        // A reference field (`L…;`) is not int-category → build bails.
        let code = [0x2a, 0xb4, 0x00, 0x02, 0xac, 0, 0];
        let mut builder = IrBuilder::new(1, 1);
        let mut fi = HashMap::new();
        fi.insert(1usize, (0usize, b'L'));
        builder.set_field_info(fi);
        assert!(builder.build(&code, 5).is_none());
    }

    #[test]
    fn test_ir_iinc() {
        // void f(int x) { x++; return x; }  [simplified]
        // iload_0; istore_1; iinc 1,1; iload_1; ireturn
        let code = [
            0x1a, // 0: iload_0
            0x3c, // 1: istore_1
            0x84, 1, 1,    // 2: iinc 1, 1
            0x1b, // 5: iload_1
            0xac, // 6: ireturn
            0, 0, // padding
        ];
        let graph = build_ir(&code, 7, 1, 2);
        // The iinc should create an Add node
        let has_add = graph.nodes.iter().any(|n| n.op == Op::Add);
        assert!(has_add, "iinc should produce an Add node");
    }

    // ── real-frame-deopt step 1: safepoint snapshots ────────────────────

    #[test]
    fn test_safepoints_recorded_per_instruction() {
        // int f(int a, int b) { return a + b; }
        // iload_0; iload_1; iadd; ireturn   (pc 0,1,2,3)
        let code = [0x1a, 0x1b, 0x60, 0xac, 0, 0];
        let graph = build_ir(&code, 4, 2, 2);

        // One snapshot per reachable bytecode boundary (pc 0..=3); the
        // post-ireturn pc is past code_len and never recorded.
        let bcis: Vec<usize> = graph.safepoints.iter().map(|s| s.bci).collect();
        assert_eq!(bcis, vec![0, 1, 2, 3], "a snapshot at every reachable bci");

        let at = |bci: usize| graph.safepoints.iter().find(|s| s.bci == bci).unwrap();

        // Entry to the method: two params in locals, empty operand stack.
        assert_eq!(at(0).locals.len(), 2);
        assert!(at(0).stack.is_empty());
        // Param nodes are live in locals throughout.
        assert_ne!(at(0).locals[0], NO_NODE);
        assert_ne!(at(0).locals[1], NO_NODE);

        // Before iload_1: one operand pushed (a).
        assert_eq!(at(1).stack.len(), 1);
        // Before iadd: both operands on the stack.
        assert_eq!(at(2).stack.len(), 2);
        // Before ireturn: the Add result is the sole stack slot.
        assert_eq!(at(3).stack.len(), 1);
        let add_id = at(3).stack[0];
        assert_eq!(graph.nodes[add_id as usize].op, Op::Add);
    }

    #[test]
    fn test_safepoint_local_tracks_store() {
        // void-ish: iload_0; istore_1; iinc 1,1; iload_1; ireturn
        // Mirrors test_ir_iinc; check the snapshot for local 1 changes after
        // the store + increment rather than staying NO_NODE.
        let code = [0x1a, 0x3c, 0x84, 1, 1, 0x1b, 0xac, 0, 0];
        let graph = build_ir(&code, 7, 1, 2);

        let at = |bci: usize| graph.safepoints.iter().find(|s| s.bci == bci).unwrap();
        // Before istore_1 (pc 1): local 1 is still uninitialised.
        assert_eq!(at(1).locals[1], NO_NODE);
        // Before iload_1 (pc 5), after the store + iinc: local 1 is an Add.
        let l1 = at(5).locals[1];
        assert_ne!(l1, NO_NODE);
        assert_eq!(graph.nodes[l1 as usize].op, Op::Add);
    }

    #[test]
    fn test_replace_all_uses_rewrites_safepoints() {
        let mut graph = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
        };
        let a = graph.add(Op::Const(1), IrType::Int, vec![], None);
        let b = graph.add(Op::Const(2), IrType::Int, vec![], None);
        graph.safepoints.push(SafepointSnapshot {
            bci: 0,
            locals: vec![a],
            stack: vec![a, b],
        });
        graph.replace_all_uses(a, b);
        // Every `a` reference in the snapshot must now point at `b`.
        assert_eq!(graph.safepoints[0].locals[0], b);
        assert_eq!(graph.safepoints[0].stack[0], b);
        assert_eq!(graph.safepoints[0].stack[1], b);
    }

    #[test]
    fn test_ir_compatible_basic() {
        let scan = super::super::x64::JitScanResult {
            needs_heap: false,
            multianewarray_ops: vec![],
            field_ops: vec![],
            typecheck_ops: vec![],
            static_field_ops: vec![],
            invoke_ops: vec![],
            new_ops: vec![],
            anewarray_ops: vec![],
            non_escaping_new: std::collections::HashSet::new(),
            ldc2w_ops: vec![],
            indy_ops: vec![],
            has_athrow: false,
            has_newarray: false,
            ldc_ops: vec![],
        };
        assert!(ir_compatible(&scan));
    }

    /// STUB-S7: confirm the widened filter admits a method with bounded
    /// field/invoke/new ops, where the previous filter would have rejected.
    #[test]
    fn test_ir_compatible_widened_admits_simple_heap_ops() {
        let scan = super::super::x64::JitScanResult {
            needs_heap: true,
            multianewarray_ops: vec![],
            field_ops: vec![(0, 1), (3, 2)],
            typecheck_ops: vec![],
            static_field_ops: vec![(6, 3)],
            invoke_ops: vec![(9, 4, 0xb8), (12, 5, 0xb6)],
            new_ops: vec![(15, 6)],
            anewarray_ops: vec![],
            non_escaping_new: std::collections::HashSet::new(),
            ldc2w_ops: vec![],
            indy_ops: vec![],
            has_athrow: false,
            has_newarray: false,
            ldc_ops: vec![],
        };
        assert!(ir_compatible(&scan));
    }

    /// STUB-S7: confirm caps still reject over-budget methods so we don't
    /// route huge heap-heavy methods through an under-tested IR pipeline.
    #[test]
    fn test_ir_compatible_rejects_over_cap() {
        let mut scan = super::super::x64::JitScanResult {
            needs_heap: true,
            multianewarray_ops: vec![],
            field_ops: vec![],
            typecheck_ops: vec![],
            static_field_ops: vec![],
            invoke_ops: vec![],
            new_ops: vec![],
            anewarray_ops: vec![],
            non_escaping_new: std::collections::HashSet::new(),
            ldc2w_ops: vec![],
            indy_ops: vec![],
            has_athrow: false,
            has_newarray: false,
            ldc_ops: vec![],
        };
        // Too many invokes.
        scan.invoke_ops = (0..6).map(|i| (i, i as u16, 0xb8)).collect();
        assert!(!ir_compatible(&scan));
        scan.invoke_ops.clear();
        // checkcast still rejected outright.
        scan.typecheck_ops = vec![(0, 1)];
        assert!(!ir_compatible(&scan));
    }

    /// STUB-S7: size-aware variant rejects oversized methods.
    #[test]
    fn test_ir_compatible_sized_rejects_large_methods() {
        let scan = super::super::x64::JitScanResult {
            needs_heap: false,
            multianewarray_ops: vec![],
            field_ops: vec![],
            typecheck_ops: vec![],
            static_field_ops: vec![],
            invoke_ops: vec![],
            new_ops: vec![],
            anewarray_ops: vec![],
            non_escaping_new: std::collections::HashSet::new(),
            ldc2w_ops: vec![],
            indy_ops: vec![],
            has_athrow: false,
            has_newarray: false,
            ldc_ops: vec![],
        };
        assert!(ir_compatible_sized(&scan, 200));
        assert!(!ir_compatible_sized(&scan, 201));
    }

    /// invokedynamic-uncommon-trap fix — regression test: `ir_compatible`
    /// must decline ANY method containing an invokedynamic (0xba) site,
    /// even one that satisfies every other cap. The IR builder has no
    /// lowering for 0xba (it bails cleanly via `_ => return None`), so a
    /// method with one must always fall back to the x64 single-pass
    /// backend, which lowers it directly (unconditional deopt to
    /// `DeoptReason::UnreachedCode`).
    #[test]
    fn test_ir_compatible_rejects_invokedynamic() {
        let scan = super::super::x64::JitScanResult {
            needs_heap: false,
            multianewarray_ops: vec![],
            field_ops: vec![],
            typecheck_ops: vec![],
            static_field_ops: vec![],
            invoke_ops: vec![],
            new_ops: vec![],
            anewarray_ops: vec![],
            non_escaping_new: std::collections::HashSet::new(),
            ldc2w_ops: vec![],
            indy_ops: vec![(0, 1)],
            has_athrow: false,
            has_newarray: false,
            ldc_ops: vec![],
        };
        assert!(
            !ir_compatible(&scan),
            "a method containing invokedynamic must never be routed through \
             the IR pipeline — it must fall back to the x64 single-pass \
             backend, which is the only backend that lowers 0xba"
        );
    }

    /// invokedynamic-uncommon-trap fix — regression test for a PC-desync bug
    /// caught during review: `find_branch_targets`/`find_loop_headers` (the
    /// two bytecode pre-scans `IrBuilder::build()` runs before its main
    /// opcode loop) must step over invokedynamic's full 5-byte encoding
    /// (opcode, cp_hi, cp_lo, 0, 0), exactly like `invokeinterface` (0xb9).
    /// Before this fix both functions fell through to the generic
    /// "unknown opcode, skip 1 byte" catch-all for 0xba, which misreads the
    /// instruction's 4 operand bytes as up to 4 phantom opcodes — corrupting
    /// branch-target / loop-header detection for the rest of the method.
    /// (`ir_compatible` now excludes every indy-bearing method from the IR
    /// path entirely — see `test_ir_compatible_rejects_invokedynamic` — so
    /// this is defense-in-depth: these two pre-scan functions must stay
    /// correct on their own terms regardless of that caller-side gate.)
    #[test]
    fn test_find_branch_targets_steps_over_invokedynamic() {
        // pc0: invokedynamic #1 (5 bytes: 0xba 0x00 0x01 0x11 0x00). The 4th
        // operand byte is 0x11 (sipush, a 3-byte op in this walker's table)
        // so that a mis-step which treats invokedynamic as 1 byte overshoots
        // PAST the real pc=5 `goto` and decodes garbage from inside it,
        // instead of coincidentally realigning: 1 (0xba) + 1 (0x00) + 1
        // (0x01) + 3 (0x11 sipush consumes pc4,pc5,pc6) lands at pc=7, two
        // bytes INSIDE the real goto's 3-byte encoding — guaranteed
        // desync, not a lucky realignment.
        // pc5: goto +3 (3 bytes: 0xa7 0x00 0x03) -> target pc8
        // pc8: ireturn (1 byte)
        let code: Vec<u8> = vec![
            0xba, 0x00, 0x01, 0x11, 0x00, // 0: invokedynamic #1
            0xa7, 0x00, 0x03, // 5: goto +3 -> pc 8
            0xac, // 8: ireturn
        ];
        let targets = find_branch_targets(&code, code.len());
        assert_eq!(
            targets,
            vec![8],
            "goto's target must resolve to pc=8 (the real ireturn); a PC \
             desync from mis-stepping invokedynamic's operand bytes would \
             either fabricate a bogus target inside the invokedynamic's own \
             operand bytes or miss/mis-locate this one entirely"
        );
    }

    /// Sibling of the above for `find_loop_headers`: a BACKWARD branch to
    /// pc=0 (a `goto` after the invokedynamic, jumping back to it) must be
    /// recognized as a loop header at exactly pc=0 — which only happens if
    /// invokedynamic's 5-byte length is stepped correctly so the `goto` at
    /// pc=5 is decoded from the right bytes.
    #[test]
    fn test_find_loop_headers_steps_over_invokedynamic() {
        // pc0: invokedynamic #1 (5 bytes: 0xba 0x00 0x01 0x11 0x00). The 4th
        // operand byte is 0x11 (sipush, a 3-byte op in this walker's table)
        // so a mis-step that treats invokedynamic as 1 byte overshoots past
        // the real pc=5 `goto` instead of coincidentally realigning on it
        // (see the sibling `find_branch_targets` test above for the same
        // reasoning) — a genuine regression guard, not a lucky bytestring.
        // pc5: goto -5 (3 bytes: 0xa7 0xff 0xfb) -> target pc0 (backward)
        let code: Vec<u8> = vec![
            0xba, 0x00, 0x01, 0x11, 0x00, // 0: invokedynamic #1
            0xa7, 0xff, 0xfb, // 5: goto -5 -> pc 0
        ];
        let headers = find_loop_headers(&code, code.len());
        assert!(
            headers.contains(&0),
            "the backward goto's target (pc=0) must be recognized as a loop \
             header; a PC desync from mis-stepping invokedynamic would \
             decode the goto's offset bytes from the wrong position and \
             either miss this header or fabricate a wrong one"
        );
    }
}
