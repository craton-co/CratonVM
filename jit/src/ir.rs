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

use std::collections::HashMap;

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
    /// Produces (ctrl, mem, retval) via Proj nodes.
    Call,

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

// ── Graph ────────────────────────────────────────────────────────────

/// The IR graph — a flat arena of nodes with an entry and exit.
pub struct Graph {
    /// All nodes, indexed by `NodeId`.
    pub nodes: Vec<Node>,
    /// The `Start` node.
    pub entry: NodeId,
    /// The `Return` node (may be a Merge if multiple returns).
    pub exit: NodeId,
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
    pub fn replace_all_uses(&mut self, old: NodeId, new_id: NodeId) {
        for node in &mut self.nodes {
            for inp in &mut node.inputs {
                if *inp == old {
                    *inp = new_id;
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
}

impl IrBuilder {
    /// Create a new builder for a method with `num_params` parameter slots
    /// and `num_locals` total local variable slots.
    pub fn new(num_params: usize, num_locals: usize) -> Self {
        let mut graph = Graph {
            nodes: Vec::with_capacity(256),
            entry: 0,
            exit: NO_NODE,
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
        }
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

    // ── Merge / phi handling ─────────────────────────────────────────

    /// Register a branch target that may need a merge node.
    fn ensure_merge(&mut self, target_pc: usize) {
        if !self.merges.contains_key(&target_pc) {
            let merge_id = self.graph.add(Op::Merge, IrType::Control, vec![], Some(target_pc));
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
    fn add_merge_predecessor(&mut self, target_pc: usize) {
        self.ensure_merge(target_pc);
        let state = self.merges.get_mut(&target_pc).unwrap();
        state.ctrl_inputs.push(self.ctrl);
        state.local_snapshots.push(self.locals.clone());
        state.stack_snapshots.push(self.stack.clone());
        state.mem_inputs.push(self.mem);
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
            self.mem = self.graph.add(Op::Phi, IrType::Memory, phi_inputs, Some(target_pc));
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
                    let phi = self.graph.add(Op::Phi, IrType::Int, phi_inputs, Some(target_pc));
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
                        let phi =
                            self.graph.add(Op::Phi, IrType::Int, phi_inputs, Some(target_pc));
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
        // First pass: identify branch targets so we know where merges go
        let targets = find_branch_targets(code, code_len);
        for &target in &targets {
            self.ensure_merge(target);
        }

        let mut pc = 0;
        while pc < code_len {
            // If this PC is a merge target, activate the merge
            if self.merges.contains_key(&pc) {
                // Add current state as predecessor (fall-through)
                if self.ctrl != NO_NODE {
                    self.add_merge_predecessor(pc);
                }
                self.activate_merge(pc);
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
                // irem
                0x70 => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.add_data(Op::Rem, IrType::Int, vec![a, b], pc);
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
                    let if_node = self.graph.add(Op::If, IrType::Control, vec![self.ctrl, cmp], Some(pc));
                    let true_ctrl = self.graph.add(Op::Proj(0), IrType::Control, vec![if_node], Some(pc));
                    let false_ctrl = self.graph.add(Op::Proj(1), IrType::Control, vec![if_node], Some(pc));

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
                    let if_node = self.graph.add(Op::If, IrType::Control, vec![self.ctrl, cmp], Some(pc));
                    let true_ctrl = self.graph.add(Op::Proj(0), IrType::Control, vec![if_node], Some(pc));
                    let false_ctrl = self.graph.add(Op::Proj(1), IrType::Control, vec![if_node], Some(pc));

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
                0xac => {
                    let val = self.pop();
                    let ret = self.graph.add(Op::Return, IrType::Void, vec![self.ctrl, val], Some(pc));
                    self.graph.exit = ret;
                    self.ctrl = NO_NODE;
                    pc += 1;
                }

                // lreturn
                0xad => {
                    let val = self.pop();
                    let ret = self.graph.add(Op::Return, IrType::Void, vec![self.ctrl, val], Some(pc));
                    self.graph.exit = ret;
                    self.ctrl = NO_NODE;
                    pc += 1;
                }

                // return (void)
                0xb1 => {
                    let ret = self.graph.add(Op::Return, IrType::Void, vec![self.ctrl], Some(pc));
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

                // Unsupported opcode — bail out
                _ => return None,
            }
        }

        Some(self.graph)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

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
            // 1-byte opcodes
            0x02..=0x0a | 0x1a..=0x21 | 0x3b..=0x42 | 0x57 | 0x59
            | 0x60 | 0x61 | 0x64 | 0x65 | 0x68 | 0x69 | 0x6c | 0x70
            | 0x74 | 0x75 | 0x78 | 0x7a | 0x7c | 0x7e | 0x80 | 0x82
            | 0x85 | 0x88 | 0xac | 0xad | 0xb1 => {
                pc += 1;
            }
            // 2-byte opcodes
            0x10 | 0x15 | 0x36 => {
                pc += 2;
            }
            // 3-byte opcodes
            0x11 | 0x84 => {
                pc += 3;
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
/// Invokedynamic is filtered earlier in `jit_scan` (it returns `None`), and
/// there is no exception-table-aware IR codegen yet (STUB-S8) — the IR builder
/// has no path that constructs exception edges, so methods with try/catch are
/// naturally rejected either there or at the bytecode-parse level.
///
/// TODO(IR widening): increase caps once differential tests validate.
pub fn ir_compatible(scan: &super::x64::JitScanResult) -> bool {
    // Caps below are deliberately conservative. A method that exceeds a cap is
    // still compilable via the x64 single-pass backend; we just decline to
    // route it through the IR pipeline until we've gained more confidence.

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
        assert!(matches!(graph.nodes[add.inputs[0] as usize].op, Op::Const(3)));
        assert!(matches!(graph.nodes[add.inputs[1] as usize].op, Op::Const(4)));
    }

    #[test]
    fn test_ir_branch_creates_merge() {
        // int f(int x) { if (x == 0) return 1; return 0; }
        // iload_0; ifeq +5; iconst_0; ireturn; iconst_1; ireturn
        //   0       1        4         5        6         7
        let code = [
            0x1a,             // 0: iload_0
            0x99, 0x00, 0x05, // 1: ifeq → 6
            0x03,             // 4: iconst_0
            0xac,             // 5: ireturn
            0x04,             // 6: iconst_1
            0xac,             // 7: ireturn
            0, 0,             // padding
        ];
        let graph = build_ir(&code, 8, 1, 1);
        // Should have Merge, If, Cmp nodes
        let has_if = graph.nodes.iter().any(|n| n.op == Op::If);
        assert!(has_if, "Should contain an If node");
        let has_cmp = graph.nodes.iter().any(|n| matches!(n.op, Op::Cmp(_)));
        assert!(has_cmp, "Should contain a Cmp node");
    }

    #[test]
    fn test_ir_iinc() {
        // void f(int x) { x++; return x; }  [simplified]
        // iload_0; istore_1; iinc 1,1; iload_1; ireturn
        let code = [
            0x1a,       // 0: iload_0
            0x3c,       // 1: istore_1
            0x84, 1, 1, // 2: iinc 1, 1
            0x1b,       // 5: iload_1
            0xac,       // 6: ireturn
            0, 0,       // padding
        ];
        let graph = build_ir(&code, 7, 1, 2);
        // The iinc should create an Add node
        let has_add = graph.nodes.iter().any(|n| n.op == Op::Add);
        assert!(has_add, "iinc should produce an Add node");
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
            ldc_ops: vec![],
        };
        assert!(ir_compatible_sized(&scan, 200));
        assert!(!ir_compatible_sized(&scan, 201));
    }
}
