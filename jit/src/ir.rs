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
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::{Deref, DerefMut};

// ── Node identity ────────────────────────────────────────────────────

/// Lightweight handle into the IR graph.
pub type NodeId = u32;

/// Sentinel value — "no node".
///
/// **Deprecated for new code.** This is a *value-domain* sentinel: it lives in
/// the same `u32` space as a real [`NodeId`], so every read of an id table has
/// to remember to test for it, and a forgotten test silently indexes
/// `nodes[u32::MAX]` (panic) or, worse, treats "undefined" as node 4294967295.
/// New code should use the `Option<NodeId>` accessors instead —
/// [`node_id_opt`], [`Node::input_opt`], [`Graph::node_opt`],
/// [`Graph::is_valid_id`], [`SafepointSnapshot::local_opt`] /
/// [`SafepointSnapshot::stack_opt`] — and only convert back to the sentinel at
/// the boundary of code that still stores it (`ir_lower`, `ir_optimize`,
/// `ir_schedule`, `lib.rs`).
///
/// The constant stays exported because those files still reference it. It can
/// never collide with a real id: [`Graph::add`] hands out ids starting at 0 and
/// increasing by one, so a collision would need `u32::MAX` live nodes — orders
/// of magnitude past every IR size cap. `Graph::add` carries a `debug_assert!`
/// that pins the invariant rather than leaving it to arithmetic folklore.
pub const NO_NODE: NodeId = u32::MAX;

/// Lift a sentinel-carrying id into an `Option`: `None` for [`NO_NODE`].
///
/// The one-line bridge between the old value-domain sentinel and the checked
/// accessors. Note this only rejects the sentinel — it does not check the id
/// against a graph; use [`Graph::node_opt`] when the graph is in hand.
#[inline]
pub fn node_id_opt(id: NodeId) -> Option<NodeId> {
    if id == NO_NODE {
        None
    } else {
        Some(id)
    }
}

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

// ── Type lattice ─────────────────────────────────────────────────────

/// True for the JVM category-1 *integer family*.
///
/// [`IrType::Int`] is already the widest member: `boolean`, `byte`, `char`,
/// `short` and `int` all decode to it (see the enum doc), and the narrowing
/// conversions `I2B`/`I2C`/`I2S` also produce it. The predicate exists so the
/// sub-width join rule has one named home — if a narrower variant is ever
/// added to [`IrType`], it belongs here and the join keeps widening to `Int`
/// instead of falling into the reject arm.
fn is_int_family(t: IrType) -> bool {
    matches!(t, IrType::Int)
}

/// True when `node` is the null literal.
///
/// This IR has **no distinct null type** — there is no `IrType::Null`, and
/// adding one would break every exhaustive `match` on [`IrType`] in the other
/// pipeline files. A null literal is therefore represented as
/// `Op::Const(0)` *typed* [`IrType::Ref`], which makes the JVMS rule "null
/// joins with any reference type to that reference type" fall straight out of
/// `Ref ⊔ Ref = Ref` in [`join_data_type`].
///
/// The corollary matters more than the predicate: an `Op::Const(0)` typed
/// `Int` is **not** a null. Accepting it as one would make every genuine
/// `int`/reference merge join silently to `Ref`, which is exactly the
/// unsound direction (the GC would then scan an arbitrary integer as an oop).
pub fn is_null_const(node: &Node) -> bool {
    matches!(node.op, Op::Const(0)) && node.ty == IrType::Ref
}

/// Join (least upper bound) of two IR value types.
///
/// This is the lattice a φ node's type is folded over
/// ([`Graph::phi_data_type_checked`]). `None` means the two types have no
/// common value representation — the merge is *untypeable*, which for verified
/// bytecode can only happen on a slot that is dead at the merge point.
///
/// Rules:
/// - identical types join to themselves (`Ref ⊔ Ref = Ref` is the conservative
///   "some common reference" — this IR models every oop with one `Ref`, so no
///   class-hierarchy least-upper-bound is computed or needed);
/// - the integer sub-widths (`boolean`/`byte`/`char`/`short`/`int`) join to the
///   widest integer, [`IrType::Int`] (see `is_int_family`);
/// - `null ⊔ Ref = Ref` — subsumed by the identity rule, because a null literal
///   is typed `Ref` (see [`is_null_const`]);
/// - `Float` joins only with `Float`, `Double` only with `Double`, `Long` only
///   with `Long`, `Ref` only with `Ref`;
/// - [`IrType::Void`] is the absence of a value, so it never joins — not even
///   with itself;
/// - everything else (any category conflict: `Ref`/`Int`, `Float`/`Int`,
///   `Int`/`Long`, `Float`/`Double`, …) returns `None`.
///
/// The operation is commutative and associative, so folding it left-to-right
/// over a φ's inputs is order-independent.
pub fn join_data_type(a: IrType, b: IrType) -> Option<IrType> {
    // `Void` is "no value at all", not a value type: it has no join partner.
    if a == IrType::Void || b == IrType::Void {
        return None;
    }
    if a == b {
        return Some(a);
    }
    if is_int_family(a) && is_int_family(b) {
        return Some(IrType::Int);
    }
    // Category conflict — `Ref`/`Int`, `Float`/`Int`, `Int`/`Long`,
    // `Float`/`Double`, `Control`/anything, `Memory`/anything.
    None
}

/// The φ type used when [`Graph::phi_data_type_checked`] cannot prove one.
///
/// `Int` is the conservative answer here, and specifically *not* `Ref`: a φ
/// typed `Ref` becomes a GC root (`ir_lower::zero_ref_phi_slots`, the
/// oop-map/deopt `StackSlotRef` classification), so guessing `Ref` for a slot
/// we could not type would hand the collector an arbitrary machine word to
/// dereference. `Int` keeps the slot out of the oop map, which is the
/// historical behaviour and the failure mode that degrades rather than
/// corrupts. Reaching it is always reported (see `report_phi_type_fallback`).
pub const PHI_TYPE_FALLBACK: IrType = IrType::Int;

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

// ── Compact edge lists ───────────────────────────────────────────────

/// Number of edges an [`Inputs`] list stores inline before spilling to the heap.
///
/// Picked from the arity of the node constructors in this file (every
/// `Graph::add` call site, counted mechanically):
///
/// | inputs | constructors | examples |
/// |--------|--------------|----------|
/// | 0      | 28           | `Const`, `ConstF`, `Param`, `Start` |
/// | 1      | 10           | `Proj`, `Neg`, the `I2L`/`L2I`/… conversions |
/// | 2      | 9            | `Add`/`Sub`/`Mul`/`Cmp`, `Return [ctrl, val]`, `If [ctrl, cmp]` |
/// | 4      | 3            | `Load [ctrl, mem, base, offset]` |
/// | 5      | 3            | `Store [ctrl, mem, base, offset, value]` |
///
/// plus three variable-arity families: `Phi` (`1 + preds`), `Merge`/`Region`
/// (`preds`) and `Call` (`2 + args`). A capacity of **5** therefore covers
/// every fixed-arity constructor in the IR — including the widest one,
/// `Op::Store` — a φ over up to four predecessors, and a call with up to three
/// arguments. Only genuinely wide merges and many-argument calls spill.
///
/// The capacity is free in memory terms: the spilled variant carries a `Vec`
/// (24 bytes on 64-bit), so any inline buffer up to that size rounds to the
/// same enum footprint. Dropping to 2 would save nothing and spill the whole
/// `Load`/`Store` family.
pub const INLINE_INPUTS: usize = 5;

thread_local! {
    /// Monotonic counter bumped by every edge write that does **not** go
    /// through a [`Graph`] mutator.
    ///
    /// `Node::inputs` is a public field, and several files in this crate still
    /// rewrite edges through it directly (`graph.nodes[i].inputs[0] = x`,
    /// `.push(..)`, `.iter_mut()`). Those writes cannot maintain a use list,
    /// and a *missed* one would make [`Graph::replace_all_uses`] silently skip
    /// a real reference. Every mutating entry point on [`Inputs`] therefore
    /// bumps this counter; a [`Graph`] records the value it last reconciled
    /// with in its [`UseLists`], and any mismatch demotes the next mutation to
    /// the full scan (which also rebuilds the lists). The guard is
    /// conservative in the safe direction: an unrelated graph's edit only
    /// costs one rebuild, never correctness.
    static EDGE_EPOCH: Cell<u64> = Cell::new(1);
}

/// Note an untracked edge write. See [`EDGE_EPOCH`].
#[inline]
fn bump_edge_epoch() {
    EDGE_EPOCH.with(|e| e.set(e.get().wrapping_add(1)));
}

/// The current edge epoch. See [`EDGE_EPOCH`].
#[inline]
fn edge_epoch() -> u64 {
    EDGE_EPOCH.with(|e| e.get())
}

/// A node's edge list: up to [`INLINE_INPUTS`] ids stored inline, spilling to
/// a heap `Vec` past that.
///
/// Externally this behaves like the `Vec<NodeId>` it replaces — it derefs to
/// `[NodeId]`, so indexing, slicing, `len()`, `iter()`, `first()`, `get()`,
/// `contains()`, `swap()` and `&node.inputs` → `&[NodeId]` coercion all work
/// unchanged — plus inherent `push`/`pop`/`clear` and `PartialEq` against
/// `Vec<NodeId>`. Only struct-literal construction differs: write
/// `Inputs::new()` or `vec![…].into()` instead of a bare `vec![…]`.
///
/// It is also the storage for the maintained use lists in [`UseLists`], where
/// the same "a handful of entries, no allocation" shape applies.
#[derive(Clone, Default)]
pub struct Inputs(InputsRepr);

#[derive(Clone)]
enum InputsRepr {
    Inline {
        len: u8,
        buf: [NodeId; INLINE_INPUTS],
    },
    Spilled(Vec<NodeId>),
}

impl Default for InputsRepr {
    fn default() -> Self {
        InputsRepr::Inline {
            len: 0,
            buf: [0; INLINE_INPUTS],
        }
    }
}

impl Inputs {
    /// An empty edge list (no allocation).
    pub fn new() -> Self {
        Inputs(InputsRepr::default())
    }

    /// An empty edge list sized for `n` edges: still inline when `n` fits.
    pub fn with_capacity(n: usize) -> Self {
        if n <= INLINE_INPUTS {
            Self::new()
        } else {
            Inputs(InputsRepr::Spilled(Vec::with_capacity(n)))
        }
    }

    /// The edges as a slice.
    #[inline]
    pub fn as_slice(&self) -> &[NodeId] {
        match &self.0 {
            InputsRepr::Inline { len, buf } => &buf[..*len as usize],
            InputsRepr::Spilled(v) => v.as_slice(),
        }
    }

    /// Mutable slice **without** bumping the edge epoch.
    ///
    /// Private on purpose: the only callers are the [`Graph`] mutators, which
    /// update the use lists themselves and then re-stamp the epoch. Every
    /// public mutable path goes through [`Inputs::as_mut_slice`].
    #[inline]
    fn slots_mut(&mut self) -> &mut [NodeId] {
        match &mut self.0 {
            InputsRepr::Inline { len, buf } => &mut buf[..*len as usize],
            InputsRepr::Spilled(v) => v.as_mut_slice(),
        }
    }

    /// The edges as a mutable slice. Marks the graph's use lists stale.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [NodeId] {
        bump_edge_epoch();
        self.slots_mut()
    }

    /// Append an edge. Marks the graph's use lists stale.
    pub fn push(&mut self, id: NodeId) {
        bump_edge_epoch();
        self.push_untracked(id);
    }

    fn push_untracked(&mut self, id: NodeId) {
        match &mut self.0 {
            InputsRepr::Inline { len, buf } => {
                if (*len as usize) < INLINE_INPUTS {
                    buf[*len as usize] = id;
                    *len += 1;
                    return;
                }
            }
            InputsRepr::Spilled(v) => {
                v.push(id);
                return;
            }
        }
        // Inline buffer full: spill.
        let mut v = Vec::with_capacity(INLINE_INPUTS * 2);
        v.extend_from_slice(self.as_slice());
        v.push(id);
        self.0 = InputsRepr::Spilled(v);
    }

    /// Remove and return the last edge. Marks the graph's use lists stale.
    pub fn pop(&mut self) -> Option<NodeId> {
        bump_edge_epoch();
        self.pop_untracked()
    }

    fn pop_untracked(&mut self) -> Option<NodeId> {
        match &mut self.0 {
            InputsRepr::Inline { len, buf } => {
                if *len == 0 {
                    None
                } else {
                    *len -= 1;
                    Some(buf[*len as usize])
                }
            }
            InputsRepr::Spilled(v) => v.pop(),
        }
    }

    /// Drop every edge. Marks the graph's use lists stale.
    pub fn clear(&mut self) {
        bump_edge_epoch();
        self.clear_untracked();
    }

    fn clear_untracked(&mut self) {
        match &mut self.0 {
            InputsRepr::Inline { len, .. } => *len = 0,
            InputsRepr::Spilled(v) => v.clear(),
        }
    }

    /// Remove the first occurrence of `val`, `true` when one was found.
    ///
    /// Order is not preserved (the last entry fills the hole). Used for use
    /// lists, where the entries are an unordered multiset — the *set* of
    /// rewritten edges, and therefore the resulting graph, does not depend on
    /// the order they are visited in.
    fn swap_remove_first_untracked(&mut self, val: NodeId) -> bool {
        let idx = match self.as_slice().iter().position(|&x| x == val) {
            Some(i) => i,
            None => return false,
        };
        let slots = self.slots_mut();
        let last = slots.len() - 1;
        slots.swap(idx, last);
        self.pop_untracked();
        true
    }

    /// True when the edges live on the heap (more than [`INLINE_INPUTS`] of
    /// them were pushed at some point). Diagnostic / test hook.
    pub fn is_spilled(&self) -> bool {
        matches!(self.0, InputsRepr::Spilled(_))
    }

    /// The inline capacity, as a function for callers that cannot name the
    /// constant.
    pub const fn inline_capacity() -> usize {
        INLINE_INPUTS
    }

    /// Consume the list into a `Vec`.
    pub fn into_vec(self) -> Vec<NodeId> {
        match self.0 {
            InputsRepr::Inline { len, buf } => buf[..len as usize].to_vec(),
            InputsRepr::Spilled(v) => v,
        }
    }
}

impl Deref for Inputs {
    type Target = [NodeId];
    #[inline]
    fn deref(&self) -> &[NodeId] {
        self.as_slice()
    }
}

impl DerefMut for Inputs {
    #[inline]
    fn deref_mut(&mut self) -> &mut [NodeId] {
        bump_edge_epoch();
        self.slots_mut()
    }
}

impl<'a> IntoIterator for &'a Inputs {
    type Item = &'a NodeId;
    type IntoIter = std::slice::Iter<'a, NodeId>;
    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter()
    }
}

impl<'a> IntoIterator for &'a mut Inputs {
    type Item = &'a mut NodeId;
    type IntoIter = std::slice::IterMut<'a, NodeId>;
    fn into_iter(self) -> Self::IntoIter {
        bump_edge_epoch();
        self.slots_mut().iter_mut()
    }
}

impl IntoIterator for Inputs {
    type Item = NodeId;
    type IntoIter = std::vec::IntoIter<NodeId>;
    fn into_iter(self) -> Self::IntoIter {
        self.into_vec().into_iter()
    }
}

impl FromIterator<NodeId> for Inputs {
    fn from_iter<T: IntoIterator<Item = NodeId>>(iter: T) -> Self {
        let mut out = Inputs::new();
        for id in iter {
            out.push_untracked(id);
        }
        out
    }
}

impl Extend<NodeId> for Inputs {
    fn extend<T: IntoIterator<Item = NodeId>>(&mut self, iter: T) {
        bump_edge_epoch();
        for id in iter {
            self.push_untracked(id);
        }
    }
}

impl From<Vec<NodeId>> for Inputs {
    fn from(v: Vec<NodeId>) -> Self {
        if v.len() <= INLINE_INPUTS {
            let mut buf = [0; INLINE_INPUTS];
            buf[..v.len()].copy_from_slice(&v);
            Inputs(InputsRepr::Inline {
                len: v.len() as u8,
                buf,
            })
        } else {
            Inputs(InputsRepr::Spilled(v))
        }
    }
}

impl From<&[NodeId]> for Inputs {
    fn from(s: &[NodeId]) -> Self {
        let mut out = Inputs::with_capacity(s.len());
        for &id in s {
            out.push_untracked(id);
        }
        out
    }
}

impl From<Inputs> for Vec<NodeId> {
    fn from(i: Inputs) -> Vec<NodeId> {
        i.into_vec()
    }
}

impl PartialEq for Inputs {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for Inputs {}

impl PartialEq<Vec<NodeId>> for Inputs {
    fn eq(&self, other: &Vec<NodeId>) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl PartialEq<Inputs> for Vec<NodeId> {
    fn eq(&self, other: &Inputs) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl PartialEq<[NodeId]> for Inputs {
    fn eq(&self, other: &[NodeId]) -> bool {
        self.as_slice() == other
    }
}

impl std::hash::Hash for Inputs {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

impl fmt::Debug for Inputs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Print exactly like the `Vec<NodeId>` this replaced, so existing
        // `{:?}` diagnostics (e.g. the `[irslot] UNALLOCATED` line in
        // `ir_lower`) keep their output.
        fmt::Debug::fmt(self.as_slice(), f)
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
    pub inputs: Inputs,
    /// Bytecode PC that produced this node (for OSR / debug).
    pub bytecode_pc: Option<usize>,
}

impl Node {
    /// Checked input read: the edge at `idx`, or `None` when the index is out
    /// of range **or** the edge is the [`NO_NODE`] placeholder.
    ///
    /// Prefer this over `node.inputs[idx]` / `node.inputs.get(idx)`: both of
    /// those hand back the sentinel as if it were an id, which is how a
    /// `nodes[u32::MAX]` panic gets written.
    #[inline]
    pub fn input_opt(&self, idx: usize) -> Option<NodeId> {
        self.inputs.get(idx).copied().and_then(node_id_opt)
    }

    /// The φ *value* inputs (i.e. `inputs[1..]`, skipping the merge/region
    /// control edge), each lifted through [`node_id_opt`] so a placeholder
    /// reads as `None` rather than as node `u32::MAX`.
    ///
    /// Meaningful only for `Op::Phi`; the caller is expected to know that.
    pub fn phi_value_inputs(&self) -> impl Iterator<Item = Option<NodeId>> + '_ {
        self.inputs.iter().skip(1).map(|&id| node_id_opt(id))
    }
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

impl SafepointSnapshot {
    /// Checked read of local slot `idx`: `None` when the slot is out of range
    /// or undefined ([`NO_NODE`]) at this bci.
    #[inline]
    pub fn local_opt(&self, idx: usize) -> Option<NodeId> {
        self.locals.get(idx).copied().and_then(node_id_opt)
    }

    /// Checked read of operand-stack slot `idx` (0 = bottom of stack).
    #[inline]
    pub fn stack_opt(&self, idx: usize) -> Option<NodeId> {
        self.stack.get(idx).copied().and_then(node_id_opt)
    }
}

// ── Def-use edges ────────────────────────────────────────────────────

/// Which half of a [`SafepointSnapshot`] a slot lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SlotKind {
    /// `SafepointSnapshot::locals`
    Local,
    /// `SafepointSnapshot::stack`
    Stack,
}

/// One safepoint snapshot slot: `graph.safepoints[snapshot].locals[slot]` (or
/// `.stack[slot]`).
///
/// A snapshot slot is a *reference to a node* exactly like an input edge — it
/// has to follow a value through [`Graph::replace_all_uses`] or a deopt frame
/// state is left naming a node that no longer defines the value. It is not,
/// however, counted by [`Graph::use_counts`] (which reports input edges only);
/// that asymmetry is pre-existing and preserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SafepointSlot {
    /// Index into `Graph::safepoints`.
    pub snapshot: u32,
    /// Which half of the snapshot.
    pub kind: SlotKind,
    /// Index within that half.
    pub slot: u32,
}

/// Incremental def-use edges: for every node, who references it.
///
/// Maintained by the [`Graph`] mutators so [`Graph::replace_all_uses`] can walk
/// the affected uses instead of every node and every snapshot slot, and so
/// [`Graph::use_counts`] can read maintained counts instead of rescanning.
///
/// ## Invariant
///
/// While the lists are *current* (see [`Graph::use_lists_valid`]):
///
/// - `users[i]` holds one entry per input **slot** that names `i`: a node with
///   two edges to `i` appears twice, so `users[i].len()` is exactly the count
///   [`Graph::use_counts`] reports for `i`;
/// - `sp_users[i]` holds every snapshot slot that names `i`. This one is a
///   *superset*: `ir_optimize`'s dead-slot normalisation clears snapshot slots
///   through the public `Graph::safepoints` field, which cannot notify the
///   lists. Clearing only ever *removes* a reference, and every rewrite
///   re-checks the slot's current value before touching it, so a stale entry is
///   inert. A direct write that *installs* a new node id into a slot would not
///   be — that is what [`Graph::set_safepoint_slot`] is for.
///
/// Both invariants are checked by [`Graph::verify_use_lists`].
///
/// Edge writes that bypass the mutators (through the public `Node::inputs`
/// field) bump [`EDGE_EPOCH`] and demote the next mutation to a full scan, so
/// bypassing costs performance, never correctness.
#[derive(Clone)]
pub struct UseLists {
    /// `users[i]` = one entry per input slot naming node `i`.
    users: Vec<Inputs>,
    /// `sp_users[i]` = snapshot slots naming node `i` (superset — see above).
    sp_users: Vec<Vec<SafepointSlot>>,
    /// Master switch. Off means "never maintain, never consult".
    tracking: bool,
    /// Whether `users` / `sp_users` currently describe the graph.
    valid: bool,
    /// Value of [`EDGE_EPOCH`] when the lists were last reconciled.
    epoch: u64,
    /// `Graph::safepoints.len()` when the lists were last reconciled.
    sp_len: usize,
    /// An edge names an id past the end of the arena. Such an edge is invisible
    /// to `use_counts` today, but a later `add` could make the id real, so the
    /// lists are re-derived rather than extended while this is set.
    dangling: bool,
    /// Instrumentation: input slots examined by `replace_all_uses`.
    examined: u64,
    /// Instrumentation: full rebuilds performed.
    rebuilds: u64,
}

impl UseLists {
    /// Empty, tracking enabled, nothing derived yet.
    pub fn new() -> Self {
        UseLists {
            users: Vec::new(),
            sp_users: Vec::new(),
            tracking: true,
            valid: false,
            epoch: 0,
            sp_len: 0,
            dangling: false,
            examined: 0,
            rebuilds: 0,
        }
    }

    /// Drop the derived state (keeps the counters and the master switch).
    fn discard(&mut self) {
        self.users = Vec::new();
        self.sp_users = Vec::new();
        self.valid = false;
        self.dangling = false;
    }

    /// Record that input slot of `user` names `def`.
    #[inline]
    fn record_input(&mut self, def: NodeId, user: NodeId, num_nodes: usize) {
        if def == NO_NODE {
            return;
        }
        if (def as usize) < num_nodes {
            self.users[def as usize].push_untracked(user);
        } else {
            self.dangling = true;
        }
    }

    /// Record that a snapshot slot names `def`.
    #[inline]
    fn record_slot(&mut self, def: NodeId, slot: SafepointSlot, num_nodes: usize) {
        if def == NO_NODE {
            return;
        }
        if (def as usize) < num_nodes {
            self.sp_users[def as usize].push(slot);
        } else {
            self.dangling = true;
        }
    }
}

impl Default for UseLists {
    fn default() -> Self {
        UseLists::new()
    }
}

impl fmt::Debug for UseLists {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UseLists")
            .field("tracking", &self.tracking)
            .field("valid", &self.valid)
            .field("nodes", &self.users.len())
            .finish()
    }
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
    /// Incremental def-use edges. A hand-built graph can leave this
    /// `UseLists::default()`: the lists are derived on first demand.
    pub uses: UseLists,
}

impl Graph {
    /// Allocate a new node, returning its `NodeId`.
    pub fn add(&mut self, op: Op, ty: IrType, inputs: Vec<NodeId>, pc: Option<usize>) -> NodeId {
        self.add_inputs(op, ty, Inputs::from(inputs), pc)
    }

    /// [`Graph::add`] taking an already-compact edge list.
    ///
    /// The `Vec` form stays the primary entry point because every existing
    /// call site spells its edges `vec![…]`; this one exists for code that
    /// already holds an [`Inputs`] (a cloned edge list, a `collect()`).
    pub fn add_inputs(&mut self, op: Op, ty: IrType, inputs: Inputs, pc: Option<usize>) -> NodeId {
        let id = self.nodes.len() as NodeId;
        // Ids are dense from 0, so this can only fire if a graph ever reaches
        // `u32::MAX` nodes — at which point a real id would be indistinguishable
        // from the `NO_NODE` placeholder and every "is this slot defined?" test
        // in the pipeline would invert. Pin the invariant rather than assume it.
        debug_assert!(
            id != NO_NODE,
            "node id collided with the NO_NODE sentinel ({} nodes)",
            self.nodes.len()
        );
        let current = self.use_lists_current();
        self.nodes.push(Node {
            op,
            ty,
            inputs,
            bytecode_pc: pc,
        });
        if current {
            if self.uses.dangling {
                // Some edge already names an id past the old end of the arena,
                // and `id` may be that id. Re-derive rather than guess.
                self.uses.valid = false;
            } else {
                self.uses.users.push(Inputs::new());
                self.uses.sp_users.push(Vec::new());
                let num_nodes = self.nodes.len();
                let (nodes, uses) = (&self.nodes, &mut self.uses);
                for &inp in nodes[id as usize].inputs.as_slice() {
                    uses.record_input(inp, id, num_nodes);
                }
                self.uses.epoch = edge_epoch();
            }
        }
        id
    }

    /// Replace all uses of `old` with `new_id` in the entire graph.
    ///
    /// Also rewrites any `old` references held by safepoint snapshots so a
    /// deopt frame state survives optimization rewrites (GVN, const-fold,
    /// materialization) intact — a safepoint slot pointing at `old` must
    /// follow the value to `new_id`, exactly like a real input edge.
    ///
    /// With current [`UseLists`] this visits only the recorded users of `old`
    /// (and only the snapshot slots that name it); otherwise it falls back to
    /// the historical full scan, which re-derives the lists in the same pass so
    /// the fallback is paid once rather than per call. The resulting graph is
    /// the same either way: both paths rewrite exactly the slots whose current
    /// value is `old`, and nodes are neither created, removed nor reordered.
    pub fn replace_all_uses(&mut self, old: NodeId, new_id: NodeId) {
        // `old == NO_NODE` (or an out-of-range id) has no use list to walk: the
        // scan would rewrite every *undefined* slot, which is a different
        // operation. Preserve it exactly by scanning.
        if self.use_lists_current() && old != NO_NODE && (old as usize) < self.nodes.len() {
            self.replace_all_uses_tracked(old, new_id);
        } else {
            self.replace_all_uses_scanning(old, new_id);
        }
    }

    /// Incremental rewrite: walk the recorded users of `old`.
    fn replace_all_uses_tracked(&mut self, old: NodeId, new_id: NodeId) {
        let num_nodes = self.nodes.len();
        let new_is_node = new_id != NO_NODE && (new_id as usize) < num_nodes;
        let new_is_dangling = new_id != NO_NODE && !new_is_node;
        let mut examined = 0u64;

        // Take the list: after the rewrite nothing names `old` any more, so the
        // entries either move to `new_id` or disappear.
        let candidates = std::mem::take(&mut self.uses.users[old as usize]);
        for &user in candidates.as_slice() {
            if (user as usize) >= num_nodes {
                continue;
            }
            let hits = {
                let slots = self.nodes[user as usize].inputs.slots_mut();
                examined += slots.len() as u64;
                let mut hits = 0usize;
                for slot in slots.iter_mut() {
                    if *slot == old {
                        *slot = new_id;
                        hits += 1;
                    }
                }
                hits
            };
            // A user with two edges to `old` appears twice in the list; the
            // second visit finds nothing left to rewrite, which is why the
            // move-over is driven by `hits` and not by the entry count.
            if hits > 0 && new_is_node {
                for _ in 0..hits {
                    self.uses.users[new_id as usize].push_untracked(user);
                }
            }
        }
        if new_is_dangling {
            self.uses.dangling = true;
        }

        // Snapshot slots. The list is a superset (see `UseLists`), so each slot
        // is re-checked before it is rewritten.
        let sp_candidates = std::mem::take(&mut self.uses.sp_users[old as usize]);
        for r in sp_candidates {
            let sp = match self.safepoints.get_mut(r.snapshot as usize) {
                Some(sp) => sp,
                None => continue,
            };
            let cell = match r.kind {
                SlotKind::Local => sp.locals.get_mut(r.slot as usize),
                SlotKind::Stack => sp.stack.get_mut(r.slot as usize),
            };
            match cell {
                Some(v) if *v == old => {
                    *v = new_id;
                    if new_is_node {
                        self.uses.sp_users[new_id as usize].push(r);
                    }
                }
                _ => {}
            }
        }

        self.uses.examined += examined;
        // Nothing here bumped the epoch (`slots_mut` is the untracked path), but
        // re-read it rather than assume: an unrelated graph may have moved it.
        self.uses.epoch = edge_epoch();
    }

    /// The historical full scan, plus (when tracking is on) a rebuild of the
    /// use lists in the same pass — the scan already touches every edge, so
    /// deriving the lists from it costs only the pushes.
    fn replace_all_uses_scanning(&mut self, old: NodeId, new_id: NodeId) {
        let num_nodes = self.nodes.len();
        let track = self.uses.tracking;
        let mut users: Vec<Inputs> = if track {
            vec![Inputs::new(); num_nodes]
        } else {
            Vec::new()
        };
        let mut sp_users: Vec<Vec<SafepointSlot>> = if track {
            vec![Vec::new(); num_nodes]
        } else {
            Vec::new()
        };
        let mut dangling = false;
        let mut examined = 0u64;

        for (uid, node) in self.nodes.iter_mut().enumerate() {
            let slots = node.inputs.slots_mut();
            examined += slots.len() as u64;
            for inp in slots.iter_mut() {
                if *inp == old {
                    *inp = new_id;
                }
                if track {
                    let def = *inp;
                    if def == NO_NODE {
                        continue;
                    }
                    if (def as usize) < num_nodes {
                        users[def as usize].push_untracked(uid as NodeId);
                    } else {
                        dangling = true;
                    }
                }
            }
        }
        for (si, sp) in self.safepoints.iter_mut().enumerate() {
            let locals = sp.locals.len();
            for (k, v) in sp.locals.iter_mut().chain(sp.stack.iter_mut()).enumerate() {
                if *v == old {
                    *v = new_id;
                }
                if track {
                    let def = *v;
                    if def == NO_NODE {
                        continue;
                    }
                    let slot = if k < locals {
                        SafepointSlot {
                            snapshot: si as u32,
                            kind: SlotKind::Local,
                            slot: k as u32,
                        }
                    } else {
                        SafepointSlot {
                            snapshot: si as u32,
                            kind: SlotKind::Stack,
                            slot: (k - locals) as u32,
                        }
                    };
                    if (def as usize) < num_nodes {
                        sp_users[def as usize].push(slot);
                    } else {
                        dangling = true;
                    }
                }
            }
        }

        self.uses.examined += examined;
        if track {
            self.uses.users = users;
            self.uses.sp_users = sp_users;
            self.uses.dangling = dangling;
            self.uses.sp_len = self.safepoints.len();
            self.uses.valid = true;
            self.uses.rebuilds += 1;
            self.uses.epoch = edge_epoch();
        }
    }

    /// Mark a node as dead (clears inputs and op).
    pub fn kill(&mut self, id: NodeId) {
        let current = self.use_lists_current();
        let num_nodes = self.nodes.len();
        let removed = {
            // Index (rather than `get_mut`) so an out-of-range id still panics,
            // as it always has.
            let n = &mut self.nodes[id as usize];
            n.op = Op::Dead;
            let removed = if current {
                let edges = n.inputs.clone();
                n.inputs.clear_untracked();
                Some(edges)
            } else {
                n.inputs.clear();
                None
            };
            n.ty = IrType::Void;
            removed
        };
        if let Some(edges) = removed {
            for &inp in edges.as_slice() {
                if inp == NO_NODE || (inp as usize) >= num_nodes {
                    continue;
                }
                self.uses.users[inp as usize].swap_remove_first_untracked(id);
            }
            self.uses.epoch = edge_epoch();
        }
    }

    /// Number of live (non-dead) nodes.
    pub fn live_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.op != Op::Dead).count()
    }

    // ── Checked id accessors ─────────────────────────────────────────
    //
    // The sentinel-free replacements for `id != NO_NODE && (id as usize) <
    // nodes.len()`, which is spelled out (or half spelled out) at dozens of
    // sites across the pipeline. See the [`NO_NODE`] doc.

    /// True when `id` names a real node in this graph — i.e. it is neither the
    /// [`NO_NODE`] placeholder nor out of range.
    ///
    /// Note this says nothing about liveness: a killed node (`Op::Dead`) still
    /// has a valid id.
    #[inline]
    pub fn is_valid_id(&self, id: NodeId) -> bool {
        id != NO_NODE && (id as usize) < self.nodes.len()
    }

    /// Checked node lookup: `None` for [`NO_NODE`] or an out-of-range id.
    #[inline]
    pub fn node_opt(&self, id: NodeId) -> Option<&Node> {
        if id == NO_NODE {
            return None;
        }
        self.nodes.get(id as usize)
    }

    /// Checked mutable node lookup — [`Graph::node_opt`]'s `&mut` twin.
    #[inline]
    pub fn node_opt_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        if id == NO_NODE {
            return None;
        }
        self.nodes.get_mut(id as usize)
    }

    /// The value type of `id`, or `None` if `id` does not name a node.
    #[inline]
    pub fn type_of(&self, id: NodeId) -> Option<IrType> {
        self.node_opt(id).map(|n| n.ty)
    }

    /// The data type of a `Op::Phi` with these `inputs`, or a description of
    /// why one could not be proven.
    ///
    /// `inputs` is the φ's full input list — `[merge_or_region, val_0, val_1,
    /// …]` — so `inputs[0]` (the control edge) is skipped and the remaining
    /// value edges are folded with [`join_data_type`].
    ///
    /// Semantics:
    /// - a value input that is [`NO_NODE`], out of range, or typed
    ///   [`IrType::Void`] contributes nothing and is skipped. This is *normal*,
    ///   not an error: `activate_loop_header` creates a loop-carried φ before
    ///   the back-edge value exists, and a local can be uninitialised on one
    ///   entry edge. `IrBuilder::retype_phi` re-runs the join once the
    ///   back-edge input has been appended;
    /// - `Err` when two inputs have no join (a category conflict — an untypeable
    ///   merge), or when *no* input carries a usable type at all;
    /// - `Ok` otherwise, with the joined type.
    ///
    /// The error is a `String` rather than a bail: the caller decides whether an
    /// untypeable φ is fatal. The builder's infallible wrapper
    /// (`IrBuilder::phi_data_type`) reports it and falls back to
    /// [`PHI_TYPE_FALLBACK`].
    pub fn phi_data_type_checked(&self, inputs: &[NodeId]) -> Result<IrType, String> {
        let mut joined: Option<IrType> = None;
        let mut unknown = 0usize;
        for (k, &n) in inputs.iter().enumerate().skip(1) {
            let ty = match self.node_opt(n) {
                Some(node) if node.ty != IrType::Void => node.ty,
                // Placeholder, out of range, or a void-producing node: no
                // information, and not (on its own) a conflict.
                _ => {
                    unknown += 1;
                    continue;
                }
            };
            joined = Some(match joined {
                None => ty,
                Some(prev) => join_data_type(prev, ty).ok_or_else(|| {
                    format!(
                        "phi value input #{k} (node {n}) is {ty:?}, which does not \
                         join with the {prev:?} of the preceding input(s)"
                    )
                })?,
            });
        }
        joined.ok_or_else(|| {
            format!(
                "phi has no typed value input ({} value edge(s), {unknown} \
                 undefined/void)",
                inputs.len().saturating_sub(1)
            )
        })
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

    /// The data type for a merge / loop-carried `Op::Phi`, derived from its
    /// value inputs (`inputs[0]` is the region/merge control) by folding the
    /// [`join_data_type`] lattice — see [`Graph::phi_data_type_checked`].
    ///
    /// inc 27 typed a φ `Long`/`Double` when it saw one of those and defaulted
    /// to `Int` for everything else, "to avoid perturbing `Ref`/`Float` phi
    /// handling". That default is the bug this replaces: a *reference* merge
    /// typed `Int` is invisible to `ir_lower::zero_ref_phi_slots` and to the
    /// oop-map/`StackSlotRef` classification in `frame_value_for`, so the
    /// merged oop is neither zero-initialised nor reported as a GC root, and a
    /// deopt frame rebuilt from it carries an `Int` where the interpreter
    /// expects a reference. A `Float` merge typed `Int` misresolves the same
    /// way (`StackSlot` instead of `StackSlotFloat`).
    ///
    /// Machine codegen is unaffected by the widening: φ edge copies are
    /// unconditionally 64-bit `MOV`s through RAX (`ir_lower::emit_phi_copies`
    /// switches on nothing but `Memory`/`Control`/`Void`), and every consumer
    /// picks its width from its own `node.ty`.
    ///
    /// Infallible by construction so no existing caller has to change: an
    /// untypeable merge is *reported* and falls back to [`PHI_TYPE_FALLBACK`]
    /// rather than silently answering `Int`. Callers that want the diagnosis
    /// use [`Self::phi_data_type_checked`].
    fn phi_data_type(&self, inputs: &[NodeId]) -> IrType {
        match self.graph.phi_data_type_checked(inputs) {
            Ok(ty) => ty,
            Err(why) => {
                report_phi_type_fallback(&why);
                PHI_TYPE_FALLBACK
            }
        }
    }

    /// [`Graph::phi_data_type_checked`] for the graph under construction: the
    /// φ type these `inputs` prove, or a description of why they prove none.
    pub fn phi_data_type_checked(&self, inputs: &[NodeId]) -> Result<IrType, String> {
        self.graph.phi_data_type_checked(inputs)
    }

    /// Re-derive a φ's type after an input was appended *after* creation.
    ///
    /// A loop-carried φ is created at the header with only its entry value(s)
    /// (the back-edge value does not exist yet — see
    /// [`Self::activate_loop_header`]), so its type is a judgement made on
    /// partial information. [`Self::patch_loop_backedge`] appends the back-edge
    /// value and then calls this so the recorded type describes the *whole*
    /// merge — e.g. a slot whose entry value was undefined (`NO_NODE`, no type
    /// information, so the φ fell back to `Int`) but whose back-edge value is a
    /// reference is retyped `Ref` and becomes a GC root.
    ///
    /// Never downgrades: memory/control φs are bookkeeping tokens and are left
    /// alone, and if the widened input list no longer joins, the previously
    /// derived type is kept (and the conflict reported) rather than replaced by
    /// the fallback.
    fn retype_phi(&mut self, phi: NodeId) {
        let current = match self.graph.node_opt(phi) {
            Some(node) => node.ty,
            None => return,
        };
        if matches!(current, IrType::Memory | IrType::Control) {
            return;
        }
        let inputs = self.graph.nodes[phi as usize].inputs.clone();
        match self.graph.phi_data_type_checked(&inputs) {
            Ok(ty) => self.graph.nodes[phi as usize].ty = ty,
            Err(why) => report_phi_type_fallback(&why),
        }
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

    /// Pop the top abstract-stack entry, or `None` on an empty stack (or a
    /// `NO_NODE` placeholder entry). The checked form of [`Self::pop`].
    fn pop_opt(&mut self) -> Option<NodeId> {
        self.stack.pop().and_then(node_id_opt)
    }

    /// Top of the abstract stack, or `None` when empty/undefined.
    fn peek_opt(&self) -> Option<NodeId> {
        self.stack.last().copied().and_then(node_id_opt)
    }

    /// Sentinel-returning [`Self::pop_opt`], kept for the opcode lowerings that
    /// still thread `NO_NODE` through to the graph.
    fn pop(&mut self) -> NodeId {
        self.pop_opt().unwrap_or(NO_NODE)
    }

    fn peek(&self) -> NodeId {
        self.peek_opt().unwrap_or(NO_NODE)
    }

    /// The current control token, or `None` in dead code (after an
    /// unconditional transfer, where `ctrl` is cleared to [`NO_NODE`]).
    fn ctrl_opt(&self) -> Option<NodeId> {
        node_id_opt(self.ctrl)
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
                .all(|s| s.get(i).copied().and_then(node_id_opt).is_none())
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

        // Appending the back-edge value completes the φ's input list, so its
        // type is re-derived from the *whole* merge (`retype_phi`): the type
        // recorded at header activation saw only the entry edge(s).
        for (i, &phi) in lp.local_phis.iter().enumerate() {
            if let Some(phi) = node_id_opt(phi) {
                let v = self.locals.get(i).copied().unwrap_or(NO_NODE);
                self.graph.nodes[phi as usize].inputs.push(v);
                self.retype_phi(phi);
            }
        }
        for (i, &phi) in lp.stack_phis.iter().enumerate() {
            if let Some(phi) = node_id_opt(phi) {
                let v = self.stack.get(i).copied().unwrap_or(NO_NODE);
                self.graph.nodes[phi as usize].inputs.push(v);
                self.retype_phi(phi);
            }
        }
        if let Some(mem_phi) = node_id_opt(lp.mem_phi) {
            self.graph.nodes[mem_phi as usize].inputs.push(self.mem);
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
                        // Same lattice join as the local phis above. This site
                        // was hardcoded `IrType::Int`, which mistyped every
                        // operand-stack merge of a reference or FP value (the
                        // `a ? x : y` shape merges on the stack, not in a
                        // local) — see `phi_data_type`.
                        let phi_ty = self.phi_data_type(&phi_inputs);
                        let phi = self
                            .graph
                            .add(Op::Phi, phi_ty, phi_inputs, Some(target_pc));
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
        // Consume the verifier's canonical decode/CFG contract instead of
        // maintaining a second opcode-length scanner in the compiler.
        let verified = cratonvm_reader::verified_code(code.get(..code_len)?).ok()?;

        // STUB-S8 fix: the bytecode of a `catch`/`finally` handler is reachable
        // ONLY through an exception edge, and a JIT frame never takes one — an
        // exception makes the compiled body return the `i64::MIN` sentinel and
        // the runtime re-runs (or precisely resumes) the method in the
        // interpreter, which is the only thing that consults the exception
        // table (see `route_implicit_exception_through_callee` /
        // `callee_has_exception_table` in vm/src/jit/helpers.rs). So handler
        // bodies are dead code in the compiled body and must be SKIPPED, not
        // walked.
        //
        // Walking them was the whole reason the optimizing tier refused every
        // method with an exception table: the linear pc walk fell into the
        // handler carrying whatever `ctrl`/`locals`/`stack` the *textually*
        // preceding code left behind, and a handler's leading `astore` of the
        // (never-modelled) exception object popped an empty stack, so the
        // builder emitted orphan nodes referencing `NO_NODE` that the
        // scheduler/lowerer still visited — panicking in `ir_lower::slot_of`
        // on a `u32::MAX` slot index.
        //
        // Skipping is safe for the code AFTER a handler too: any instruction
        // that follows unreachable code and is itself reachable can only be
        // entered by a branch (fall-through from unreachable code is not a real
        // predecessor), so it is necessarily a merge target and its state is
        // restored by `activate_merge` rather than inherited. The `ctrl ==
        // NO_NODE` bail inside the loop enforces exactly that invariant.
        let reachable = normally_reachable_pcs(&verified, code_len);

        // Only reachable targets get a merge node. A `merge_targets` entry that
        // lies inside a skipped handler (or that only handler code branches to)
        // would otherwise leave an `Op::Merge` with zero control inputs in the
        // graph — never activated, because the walk never arrives there — and
        // an input-less control node is exactly the kind of orphan the
        // scheduler/lowerer is not prepared for.
        for &target in verified.merge_targets() {
            let target = target as usize;
            if reachable.contains(&target) {
                self.ensure_merge(target);
            }
        }
        self.loop_headers = verified
            .loop_headers()
            .iter()
            .map(|target| *target as usize)
            .filter(|target| reachable.contains(target))
            .collect();

        let mut pc = 0;
        while pc < code_len {
            if !reachable.contains(&pc) {
                // Handler-only (or otherwise unreachable) bytecode: emit no IR
                // for it at all. `next_pc` comes from the verifier's canonical
                // decode, so this steps over multi-byte operands correctly
                // (a hand-rolled `pc += 1` would resync onto operand bytes).
                pc = match verified.instruction_at(pc) {
                    Some(decoded) => decoded.next_pc as usize,
                    // Not an instruction boundary — the verifier guarantees the
                    // walk only ever lands on one, so this is unreachable; bail
                    // to single-pass rather than guess a length.
                    None => return ir_build_bail(line!(), pc),
                };
                continue;
            }

            // If this PC is a merge target, activate the merge
            if self.merges.contains_key(&pc) {
                // Add current state as predecessor (fall-through). On a loop
                // header this is the forward-entry predecessor; the back-edge
                // arrives later and is back-patched (see add_merge_predecessor).
                if self.ctrl_opt().is_some() {
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

            // Safety net for the skip above: reachable code must always have a
            // live control token — either inherited from the fall-through or
            // restored by the merge activation. If it does not, the reachable
            // set and the merge bookkeeping disagree, and continuing would
            // build the very orphan nodes the skip exists to prevent. Bail to
            // the single-pass backend instead of emitting them.
            if self.ctrl_opt().is_none() {
                return ir_build_bail(line!(), pc);
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
            if self.ctrl_opt().is_some() {
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
                    // Compact reference-field layout packs field offsets (refs
                    // to 8 bytes), so the `HEADER_SIZE + field_index*SLOT_SIZE`
                    // displacement is wrong for a compact object. That is a
                    // property of ONE of `Op::Load`'s two lowerings, not of the
                    // opcode, and this used to be a blanket bail here.
                    //
                    // The blanket bail was the single largest exclusion in the
                    // optimizing tier. `compact_ref_fields_enabled()` defaults
                    // to TRUE, so it refused every method containing a
                    // `getfield` — which is most object-oriented Java — and it
                    // did so at stage one of the pipeline, invisibly. That is
                    // why the relocation-map contract could never be exercised:
                    // every probe written for it read a field
                    // (docs/internal/jit-ir-relocation-map-contract.md).
                    //
                    // The constraint belongs where the displacement is emitted.
                    // `ir_lower` lowers `Op::Load` through the checked
                    // `jit_getfield` helper whenever the helper address is
                    // present, and that helper IS compact-aware
                    // (`jit_compact_field_slot`); only the inline
                    // displacement fallback, taken when the helper is absent
                    // (the JIT unit tests' stub table), is layout-naive. So
                    // `ir_lower::lower_inner` refuses the graph in exactly that
                    // combination and the builder no longer refuses the opcode.
                    let (field_index, type_tag) = match self.field_info.get(&pc) {
                        Some(&fi) => fi,
                        None => return ir_build_bail(line!(), pc),
                    };
                    if !matches!(type_tag, b'I' | b'Z' | b'B' | b'C' | b'S') {
                        return ir_build_bail(line!(), pc);
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
                    // See the getfield (0xb4) note. `Op::Store` likewise has a
                    // layout-naive inline lowering and a compact-aware helper
                    // lowering (`jit_putfield_int`); `ir_lower::lower_inner`
                    // picks between them and refuses only when compact layout is
                    // on and no helper address is available.
                    let (field_index, type_tag) = match self.field_info.get(&pc) {
                        Some(&fi) => fi,
                        None => return ir_build_bail(line!(), pc),
                    };
                    if !matches!(type_tag, b'I' | b'Z' | b'B' | b'C' | b'S') {
                        return ir_build_bail(line!(), pc);
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
                // heap allocation, fields become SSA values). An escaping
                // allocation survives optimization and is emitted through the
                // shared compact-layout/TLAB-aware runtime lowering stub.
                0xbb => {
                    let (class_id, num_fields) = match self.new_info.get(&pc) {
                        Some(&ci) => ci,
                        None => return ir_build_bail(line!(), pc),
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
                            return ir_build_bail(line!(), pc);
                        }
                        // Defence in depth: only elide when the receiver (top of
                        // stack for a no-arg `<init>`) is a fresh `Op::New` we
                        // emitted. Eliding a `<init>` whose receiver is `this` or
                        // a parameter would skip a real superclass constructor
                        // (and hide any escape it performs).
                        let recv_is_new = matches!(
                            self.peek_opt().and_then(|r| self.graph.node_opt(r)),
                            Some(Node {
                                op: Op::New { .. },
                                ..
                            })
                        );
                        if !recv_is_new {
                            return ir_build_bail(line!(), pc);
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
                        None => return ir_build_bail(line!(), pc),
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
                        None => return ir_build_bail(line!(), pc),
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
                        None => return ir_build_bail(line!(), pc),
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
                _ => return ir_build_bail_opcode(op, pc),
            }
        }

        Some(self.graph)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

/// The set of bytecode pcs reachable from method entry following only
/// **normal** control flow — branches, switches and fall-through, never an
/// exception edge.
///
/// Everything outside this set is code the compiled body can never execute:
/// in practice the bodies of `catch` / `finally` handlers, which are entered
/// only by the interpreter after the JIT frame has already returned its
/// `i64::MIN` sentinel. [`IrBuilder::build`] skips those pcs entirely rather
/// than abstract-interpreting them with stale state (see the STUB-S8 comment
/// there for what that used to produce).
///
/// Edges come from [`cratonvm_reader::VerifiedCode::successors`], the same
/// canonical CFG contract the verifier and `merge_targets` are derived from,
/// so this cannot disagree with the merge bookkeeping about where an
/// instruction ends or which targets it has.
fn normally_reachable_pcs(
    verified: &cratonvm_reader::VerifiedCode,
    code_len: usize,
) -> HashSet<usize> {
    let mut reachable = HashSet::with_capacity(verified.instructions().len());
    if code_len == 0 {
        return reachable;
    }
    let mut work = vec![0usize];
    reachable.insert(0usize);
    while let Some(pc) = work.pop() {
        for succ in verified.successors(pc) {
            if succ < code_len && reachable.insert(succ) {
                work.push(succ);
            }
        }
    }
    reachable
}

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
#[cfg(test)]
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
#[cfg(test)]
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

/// Report the exact [`IrBuilder::build`] site that refused a method.
///
/// The builder answers `Option<Graph>`, so every refusal is indistinguishable
/// from "the optimizing tier is switched off" — both present as zero IR bodies,
/// and this investigation drew the second conclusion from the first twice
/// (`docs/internal/jit-ir-relocation-map-contract.md`). Fifteen distinct
/// `return None` sites share one observable outcome; only the site tells them
/// apart, and nothing reported it.
///
/// `site` is `line!()` at the refusal rather than a parallel reason enum: it
/// names the site exactly and cannot drift out of step with the code.
#[cold]
#[inline(never)]
pub fn ir_build_bail<T>(site: u32, pc: usize) -> Option<T> {
    if ir_bail_reporting() {
        eprintln!("[ir] IrBuilder::build refused at ir.rs:{site} (bytecode pc {pc})");
    }
    None
}

/// [`ir_build_bail`] for the opcode catch-all, which knows something more
/// useful than its own line number: *which* bytecode has no IR lowering.
#[cold]
#[inline(never)]
pub fn ir_build_bail_opcode<T>(op: u8, pc: usize) -> Option<T> {
    if ir_bail_reporting() {
        eprintln!("[ir] IrBuilder::build has no lowering for opcode {op:#04x} at bytecode pc {pc}");
    }
    None
}

/// Report a φ whose type could not be proven by the [`join_data_type`] lattice.
///
/// Observability for the one place the type system gives up. Silence here is
/// what let "every reference merge is an `Int`" survive: the wrong type is not
/// a crash, it is a missing GC root and a mistyped deopt frame value, both of
/// which surface much later and somewhere else. Uses the same
/// `CRATONVM_DBG_JITC` / `CRATONVM_DBG_IR_COMPILES` switch as the build bails
/// ([`ir_build_bail`]) so one flag shows the whole IR-refusal picture, and is
/// deliberately a report rather than a `debug_assert!`: an untypeable merge is
/// a *dead* slot in verified bytecode, so it must degrade, not panic.
///
/// (If/when `crate::bailout` lands, this is the natural place to raise a
/// `BailoutReason::IrVerification` instead of falling back — the fallback is
/// what the current infallible signature forces.)
#[cold]
#[inline(never)]
fn report_phi_type_fallback(why: &str) {
    if ir_bail_reporting() {
        eprintln!(
            "[ir] phi type join failed: {why}; using the conservative {:?} \
             (not a GC root)",
            PHI_TYPE_FALLBACK
        );
    }
}

fn ir_bail_reporting() -> bool {
    cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JITC").is_some()
        || cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_IR_COMPILES").is_some()
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
/// Report which conjunct of [`ir_compatible`] refused a method.
///
/// Same problem as [`ir_build_bail`], one stage earlier: the predicate answers
/// a bare `bool`, so "this method is not IR-eligible" and "the optimizing tier
/// never ran" are the same observation. The refused conjunct is the only thing
/// that distinguishes a deliberate exclusion from a bug.
#[cold]
#[inline(never)]
fn ir_reject(why: &str) -> bool {
    if ir_bail_reporting() {
        eprintln!("[ir] ir_compatible refused: {why}");
    }
    false
}

pub fn ir_compatible(scan: &super::x64::JitScanResult) -> bool {
    // Caps below are deliberately conservative. A method that exceeds a cap is
    // still compilable via the x64 single-pass backend; we just decline to
    // route it through the IR pipeline until we've gained more confidence.

    // RBC.6 — the IR pipeline has no athrow lowering; only the x64
    // single-pass backend emits the stash-pending-exception sequence.
    if scan.has_athrow {
        return ir_reject("scan.has_athrow");
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
        return ir_reject("!scan.indy_ops.is_empty()");
    }

    // Cap simple invokes (invokestatic / invokevirtual / invokespecial /
    // invokeinterface). Invokedynamic is rejected upstream by `jit_scan`.
    //
    // Raised 5 -> 32 by the IR direct-call slice. The old cap of 5 was not a
    // codegen limit at all: the IR path lowered EVERY invoke through the generic
    // `jit_invoke_dispatch` helper (no direct calls, no inline caches), so a
    // call-heavy method's "optimizing" IR recompile paid a full helper round trip
    // per call and could come out SLOWER than the single-pass body, which binds a
    // statically-resolved callee with a raw `CALL`. The cap was a blunt way of
    // saying "don't route call-heavy methods here". `ir_lower`'s
    // `emit_direct_cross_call` now closes that gap for the statically-bound kinds
    // (`invokestatic` and non-`<init>` `invokespecial` — the only kinds admitted
    // in a default configuration, since `CRATONVM_JIT_IR_CALL_VIRTUAL` is
    // default-OFF), so the original reason no longer applies to them.
    //
    // 2026-07-26 (jit-inlining-and-ir-calls): raised again, 32 -> 64, and the
    // "only the statically-bound kinds are covered" caveat above is now
    // obsolete. `ir_lower::emit_inline_cache_call` gives `invokevirtual` /
    // `invokeinterface` the same MIC + 3-way-PIC cascade the single-pass
    // backend emits, so a virtual site no longer pays a helper round trip with
    // a dynamic lookup either, and `CRATONVM_JIT_IR_CALL_VIRTUAL` inverted from
    // opt-in to opt-out. EVERY invoke kind now lowers at least as well as
    // single-pass does.
    //
    // What the cap still buys, unchanged: (1) each `Op::Call` widens the
    // argument staging region and forces `needs_context`, and `lower()` bails
    // when `1 + num_params` exceeds the ABI register file, so a very call-dense
    // method risks a lowering bail after all the graph work; (2) an
    // unresolvable callee still falls back to helper dispatch, so a method
    // whose invokes are mostly unresolvable gains nothing and only pays compile
    // time; (3) code size — an inline-cache site emits ~250 bytes, so 64 sites
    // is ~16 KiB of dispatch code, the same order the single-pass backend
    // produces for the same method. It is a budget, not a lowering gap.
    if scan.invoke_ops.len() > IR_MAX_INVOKES {
        return ir_reject("scan.invoke_ops.len() > IR_MAX_INVOKES");
    }
    // getfield/putfield. Raised 5 -> 64. Instance field access lowers to
    // `Op::Load`/`Op::Store` (optionally via the checked `jit_getfield`
    // helper); nothing about it scales worse than the single-pass backend, so
    // 5 was never a lowering limit — it was part of the same blanket "keep real
    // methods off the IR path" posture as the invoke cap.
    if scan.field_ops.len() > IR_MAX_FIELD_OPS {
        return ir_reject("scan.field_ops.len() > IR_MAX_FIELD_OPS");
    }
    // getstatic/putstatic (same shape as instance field ops for IR).
    if scan.static_field_ops.len() > IR_MAX_STATIC_FIELD_OPS {
        return ir_reject("scan.static_field_ops.len() > IR_MAX_STATIC_FIELD_OPS");
    }
    // `new` sites. Raised only 3 -> 16, deliberately the most conservative of
    // the budgets.
    //
    // DO NOT treat this like the others. It governs how many allocations escape
    // analysis may scalar-replace, and each one that SURVIVES is lowered through
    // the shared `emit_new_object_stub` — the same runtime-lowering stub the
    // baseline tier uses, not the inline TLAB bump-pointer sequence. Losing that
    // inline bump is one of the two independent causes of the July 2026 Binary
    // Trees 4x regression documented in BENCHMARK.md, so raising this cap
    // increases the number of sites that can pay the stub, not just the number
    // that can be optimized away.
    if scan.new_ops.len() > IR_MAX_ALLOCATIONS {
        return ir_reject("scan.new_ops.len() > IR_MAX_ALLOCATIONS");
    }

    // ── Surviving exclusions — real missing lowerings, not budgets ───
    //  * ARRAY allocation of every arity. `newarray` (0xbc) and `anewarray`
    //    (0xbd) have no arm in `IrBuilder::build`'s opcode match and `Op::New
    //    Array` is constructed nowhere, so a method containing either bails at
    //    the builder's `_ =>` catch-all; `multianewarray` additionally needs a
    //    resolver-shaped helper sequence the lowerer cannot synthesize.
    //
    //    This used to be a `> IR_MAX_ALLOCATIONS` budget on `anewarray_ops`,
    //    which ADMITTED up to 16 array allocations into a pipeline that cannot
    //    lower one. Admission and the builder disagreeing is not merely untidy:
    //    it is what made the optimizing tier produce zero bodies on every probe
    //    written for the relocation-map contract while every gate upstream
    //    reported "open" (docs/internal/jit-ir-relocation-map-contract.md). A
    //    method the builder will refuse must be refused HERE, cheaply and with
    //    a reason, not after a full graph build.
    //  * `checkcast` / `instanceof` — the runtime type check needs its own
    //    guard shape (class-id compare plus a subtype-check helper fallback)
    //    which the IR lowerer does not emit. The inline caches added for
    //    virtual/interface DISPATCH do not help: they cache a call target, not
    //    a subtype answer.
    if !scan.anewarray_ops.is_empty() {
        return ir_reject("!scan.anewarray_ops.is_empty() (no IR lowering for 0xbd)");
    }
    if !scan.multianewarray_ops.is_empty() {
        return ir_reject("!scan.multianewarray_ops.is_empty()");
    }
    if !scan.typecheck_ops.is_empty() {
        return ir_reject("!scan.typecheck_ops.is_empty()");
    }

    true
}

/// Maximum invoke sites (`invokestatic`/`virtual`/`special`/`interface`) in a
/// method routed through the IR pipeline. Was 5, then 32; see the rationale at
/// the check in [`ir_compatible`].
pub const IR_MAX_INVOKES: usize = 64;

/// Maximum `getfield`/`putfield` sites. Was 5.
pub const IR_MAX_FIELD_OPS: usize = 64;

/// Maximum `getstatic`/`putstatic` sites. Was 5.
pub const IR_MAX_STATIC_FIELD_OPS: usize = 64;

/// Maximum `new` sites. Was 3. See the warning at the allocation check in
/// [`ir_compatible`] — this one bounds both what escape analysis may attempt to
/// eliminate AND how many surviving allocations may be lowered through the
/// shared `emit_new_object_stub` (which costs the baseline tier's inline TLAB
/// bump). It never applied to arrays, which are refused outright.
pub const IR_MAX_ALLOCATIONS: usize = 16;

/// Maximum bytecode length for the IR pipeline.
///
/// Was 200 — small enough that essentially no real application method
/// qualified, which made every other cap moot. 8000 is HotSpot's
/// `HugeMethodLimit`, the point at which HotSpot itself declines to compile a
/// method at all (`DontCompileHugeMethods`), so this stops being an
/// IR-specific restriction and becomes the same pathological-method backstop
/// every JVM has. Tier-4 compile time is separately bounded by
/// [`IR_MAX_GRAPH_NODES`], which reflects actual complexity rather than a byte
/// count.
pub const IR_MAX_BYTECODE_SIZE: usize = 8000;

/// Maximum node count of a built IR graph before the optimizing pipeline is
/// abandoned in favour of the single-pass backend.
///
/// This is the real compile-time guard for tier 4. `ir_optimize` runs passes
/// that are super-linear in node count (GVN's value numbering, the
/// escape-analysis connection graph, the scheduler's block placement), so a
/// bytecode-length cap alone does not bound compile time: an 8000-byte method
/// of straight-line arithmetic builds a far larger graph than an 8000-byte
/// call-heavy one. Checked in `try_compile_inner` right after
/// `IrBuilder::build` and before any optimization runs, so an over-large graph
/// costs one linear build and nothing else. Its runtime companion is
/// `tiered::MAX_C2_COMPILE_TIME_MS`, which catches whatever slips past this.
pub const IR_MAX_GRAPH_NODES: usize = 20_000;

/// Variant of [`ir_compatible`] that also enforces the bytecode-length budget
/// ([`IR_MAX_BYTECODE_SIZE`]). Kept as a separate entry point so callers that
/// do not know the method size stay source-compatible.
pub fn ir_compatible_sized(scan: &super::x64::JitScanResult, code_len: usize) -> bool {
    if code_len > IR_MAX_BYTECODE_SIZE {
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
        // The builder used to refuse getfield/putfield outright whenever
        // compact layout was on (the default), which made this test
        // vacuous in every default run. The layout constraint now lives in
        // `ir_lower::lower_inner`, where the layout-naive displacement is
        // actually emitted, so the graph shape below is asserted for real.
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
        // The builder used to refuse getfield/putfield outright whenever
        // compact layout was on (the default), which made this test
        // vacuous in every default run. The layout constraint now lives in
        // `ir_lower::lower_inner`, where the layout-naive displacement is
        // actually emitted, so the graph shape below is asserted for real.
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
        // The builder used to refuse getfield/putfield outright whenever
        // compact layout was on (the default), which made this test
        // vacuous in every default run. The layout constraint now lives in
        // `ir_lower::lower_inner`, where the layout-naive displacement is
        // actually emitted, so the graph shape below is asserted for real.
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
            local_slot_ops: vec![],
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
            local_slot_ops: vec![],
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
            local_slot_ops: vec![],
            has_newarray: false,
            ldc_ops: vec![],
        };
        // Invokes: 5 -> 32 (direct-call slice) -> IR_MAX_INVOKES once inline
        // caches covered the virtual/interface kinds too. 6 invokes — the shape
        // the ORIGINAL cap rejected — must be admitted; the budget boundary
        // must be exact.
        scan.invoke_ops = (0..6).map(|i| (i, i as u16, 0xb8)).collect();
        assert!(
            ir_compatible(&scan),
            "6 invokes are within the raised cap and must be admitted"
        );
        scan.invoke_ops = (0..IR_MAX_INVOKES).map(|i| (i, i as u16, 0xb8)).collect();
        assert!(
            ir_compatible(&scan),
            "exactly IR_MAX_INVOKES invokes must be admitted"
        );
        scan.invoke_ops = (0..IR_MAX_INVOKES + 1)
            .map(|i| (i, i as u16, 0xb8))
            .collect();
        assert!(!ir_compatible(&scan), "one invoke past the budget is rejected");
        scan.invoke_ops.clear();
        // A virtual site is admitted on the same budget as a static one — the
        // inline-cache lowering removed the reason to treat it differently.
        scan.invoke_ops = (0..IR_MAX_INVOKES).map(|i| (i, i as u16, 0xb6)).collect();
        assert!(ir_compatible(&scan));
        scan.invoke_ops.clear();

        // Field / static-field budgets: the old cap of 5 must no longer bind.
        scan.field_ops = (0..6).map(|i| (i, i as u16)).collect();
        assert!(ir_compatible(&scan), "the old 5-field cap must not bind");
        scan.field_ops = (0..IR_MAX_FIELD_OPS + 1).map(|i| (i, i as u16)).collect();
        assert!(!ir_compatible(&scan));
        scan.field_ops.clear();
        scan.static_field_ops = (0..6).map(|i| (i, i as u16)).collect();
        assert!(ir_compatible(&scan));
        scan.static_field_ops = (0..IR_MAX_STATIC_FIELD_OPS + 1)
            .map(|i| (i, i as u16))
            .collect();
        assert!(!ir_compatible(&scan));
        scan.static_field_ops.clear();

        // Object allocations stay the most conservative budget: a surviving
        // `Op::New` lowers through the shared stub, so the cap bounds real
        // codegen and not only what escape analysis may try to eliminate.
        scan.new_ops = (0..IR_MAX_ALLOCATIONS).map(|i| (i, i as u16)).collect();
        assert!(ir_compatible(&scan));
        scan.new_ops = (0..IR_MAX_ALLOCATIONS + 1).map(|i| (i, i as u16)).collect();
        assert!(!ir_compatible(&scan));
        scan.new_ops.clear();
        // ARRAY allocation is not a budget at all — `IrBuilder::build` has no
        // arm for 0xbd, so ONE site must be refused here rather than admitted
        // into a pipeline that bails on it after a full graph build.
        scan.anewarray_ops = vec![(0, 1)];
        assert!(!ir_compatible(&scan));
        scan.anewarray_ops.clear();

        // ── Surviving exclusions (NOT budgets) ──────────────────────
        // checkcast/instanceof still rejected outright: the inline caches added
        // for virtual DISPATCH cache a call target, not a subtype answer.
        scan.typecheck_ops = vec![(0, 1)];
        assert!(!ir_compatible(&scan));
        scan.typecheck_ops.clear();
        // athrow: no IR lowering exists.
        scan.has_athrow = true;
        assert!(!ir_compatible(&scan));
        scan.has_athrow = false;
        // invokedynamic: no IR builder arm.
        scan.indy_ops = vec![(0, 1)];
        assert!(!ir_compatible(&scan));
        scan.indy_ops.clear();
        // multianewarray: needs a helper sequence the lowerer cannot synthesize.
        scan.multianewarray_ops = vec![(0, 1, 2)];
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
            local_slot_ops: vec![],
            has_newarray: false,
            ldc_ops: vec![],
        };
        // jit-inlining-and-ir-calls: the bytecode budget rose from 200 — which
        // excluded essentially every real application method, making the other
        // caps moot — to HotSpot's `HugeMethodLimit`. The 201-byte method that
        // used to sit just past the old boundary must now be admitted.
        assert!(ir_compatible_sized(&scan, 200));
        assert!(ir_compatible_sized(&scan, 201));
        assert!(ir_compatible_sized(&scan, IR_MAX_BYTECODE_SIZE));
        assert!(!ir_compatible_sized(&scan, IR_MAX_BYTECODE_SIZE + 1));
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
            local_slot_ops: vec![],
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

    /// The canonical `try`/`catch` layout javac emits:
    ///
    /// ```text
    ///  0: iload_0
    ///  1: iconst_1
    ///  2: iadd
    ///  3: istore_1      ← end of the protected range
    ///  4: goto +7 → 11
    ///  7: astore_2      ← HANDLER entry (exception object on an empty stack)
    ///  8: iconst_0
    ///  9: istore_1
    /// 10: nop
    /// 11: iload_1       ← join, reachable from both arms
    /// 12: ireturn
    /// ```
    ///
    /// pcs 7..=10 are reachable only through an exception edge. The builder
    /// must emit NO nodes and NO safepoint snapshots for them: walking them
    /// is what produced the `NO_NODE`-referencing orphans that panicked
    /// `ir_lower::slot_of` (the handler's leading `astore` pops an empty
    /// abstract stack, and `pop()` yields `NO_NODE` silently).
    #[test]
    fn test_handler_body_is_not_walked() {
        let code: Vec<u8> = vec![
            0x1a, // 0: iload_0
            0x04, // 1: iconst_1
            0x60, // 2: iadd
            0x3c, // 3: istore_1
            0xa7, 0x00, 0x07, // 4: goto +7 -> 11
            0x4d, // 7: astore_2   (handler)
            0x03, // 8: iconst_0
            0x3c, // 9: istore_1
            0x00, // 10: nop
            0x1b, // 11: iload_1
            0xac, // 12: ireturn
        ];
        let graph = build_ir(&code, code.len(), 1, 3);

        let handler_pcs = 7..=10;
        for (id, node) in graph.nodes.iter().enumerate() {
            if let Some(pc) = node.bytecode_pc {
                assert!(
                    !handler_pcs.contains(&pc),
                    "node {id} ({:?}) was built for handler-only pc {pc}; \
                     handler bodies are unreachable in a compiled frame and \
                     must be skipped, not abstract-interpreted",
                    node.op
                );
            }
        }
        for snap in &graph.safepoints {
            assert!(
                !handler_pcs.contains(&snap.bci),
                "safepoint snapshot recorded for handler-only bci {}; the \
                 compiled body can never resume there",
                snap.bci
            );
        }
        // The reachable code either side of the skipped region must still be
        // built — the skip must not swallow the join or the tail.
        assert!(
            graph.nodes.iter().any(|n| n.bytecode_pc == Some(2)),
            "the try body (pc 2, iadd) must still be compiled"
        );
        assert!(
            graph.safepoints.iter().any(|s| s.bci == 11),
            "the join after the handler (pc 11) must still be reached; a skip \
             that failed to resync would drop it"
        );
    }

    /// An opcode the builder cannot lower (`athrow`, 0xbf — the rethrow every
    /// `finally` ends with) must no longer disqualify the WHOLE method when it
    /// appears only inside a handler body. Before the skip, the linear walk
    /// fell into the handler and hit `_ => return None`.
    ///
    /// (The `lib.rs` gate additionally consults `scan.has_athrow`, so this
    /// particular shape is still refused one level up; the test pins the
    /// builder's own behaviour, which is what the skip changes.)
    #[test]
    fn test_unsupported_opcode_in_handler_does_not_bail_the_method() {
        let code: Vec<u8> = vec![
            0x1a, // 0: iload_0
            0x04, // 1: iconst_1
            0x60, // 2: iadd
            0x3c, // 3: istore_1
            0xa7, 0x00, 0x05, // 4: goto +5 -> 9
            0x4d, // 7: astore_2  (handler)
            0xbf, // 8: athrow    — unsupported by the builder
            0x1b, // 9: iload_1
            0xac, // 10: ireturn
        ];
        let builder = IrBuilder::new(1, 3);
        assert!(
            builder.build(&code, code.len()).is_some(),
            "an unsupported opcode reachable only through an exception edge \
             must not bail the method to single-pass"
        );
    }

    /// Reachability follows normal edges only, and in particular does NOT
    /// treat a handler entry as a root.
    #[test]
    fn test_normally_reachable_excludes_handler_only_code() {
        let code: Vec<u8> = vec![
            0x1a, // 0: iload_0
            0xa7, 0x00, 0x04, // 1: goto +4 -> 5
            0x00, // 4: nop        (handler-only)
            0xac, // 5: ireturn
        ];
        let verified = cratonvm_reader::verified_code(&code).unwrap();
        let reachable = normally_reachable_pcs(&verified, code.len());
        assert!(reachable.contains(&0));
        assert!(reachable.contains(&1));
        assert!(reachable.contains(&5), "the goto target is reachable");
        assert!(
            !reachable.contains(&4),
            "pc 4 is only reachable by falling through a goto — i.e. not at \
             all — so it must be excluded"
        );
    }

    // ── Type lattice / φ typing ──────────────────────────────────────

    /// An empty arena for hand-built graphs (no Start / Proj preamble).
    fn empty_graph() -> Graph {
        Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
        }
    }

    /// A value node of the given type, with no inputs.
    fn value(graph: &mut Graph, ty: IrType) -> NodeId {
        let op = match ty {
            IrType::Float | IrType::Double => Op::ConstF(0),
            IrType::Ref => Op::New {
                class_id: 1,
                num_fields: 0,
            },
            _ => Op::Const(1),
        };
        graph.add(op, ty, vec![], None)
    }

    #[test]
    fn test_join_identical_types() {
        for ty in [
            IrType::Int,
            IrType::Long,
            IrType::Float,
            IrType::Double,
            IrType::Ref,
        ] {
            assert_eq!(
                join_data_type(ty, ty),
                Some(ty),
                "{ty:?} must join with itself"
            );
        }
        // The integer family widens to the widest integer, which in this IR is
        // already `Int` (boolean/byte/char/short/int all decode to it).
        assert_eq!(join_data_type(IrType::Int, IrType::Int), Some(IrType::Int));
    }

    #[test]
    fn test_join_rejects_category_conflicts() {
        // Every cross-category pair is untypeable, in both directions.
        let conflicts = [
            (IrType::Int, IrType::Long),
            (IrType::Int, IrType::Float),
            (IrType::Int, IrType::Double),
            (IrType::Int, IrType::Ref),
            (IrType::Float, IrType::Double),
            (IrType::Float, IrType::Ref),
            (IrType::Long, IrType::Double),
            (IrType::Long, IrType::Ref),
        ];
        for (a, b) in conflicts {
            assert_eq!(join_data_type(a, b), None, "{a:?} ⊔ {b:?} must not join");
            assert_eq!(join_data_type(b, a), None, "{b:?} ⊔ {a:?} must not join");
            // Specifically: a conflict must never answer `Int`. That silent
            // answer is the bug this lattice exists to remove.
            assert_ne!(join_data_type(a, b), Some(IrType::Int));
        }
        // `Void` is the absence of a value — it never joins, not even with
        // itself.
        assert_eq!(join_data_type(IrType::Void, IrType::Void), None);
        assert_eq!(join_data_type(IrType::Void, IrType::Int), None);
        assert_eq!(join_data_type(IrType::Ref, IrType::Void), None);
    }

    #[test]
    fn test_phi_type_int_int() {
        let mut g = empty_graph();
        let m = g.add(Op::Merge, IrType::Control, vec![], None);
        let a = value(&mut g, IrType::Int);
        let b = value(&mut g, IrType::Int);
        assert_eq!(g.phi_data_type_checked(&[m, a, b]), Ok(IrType::Int));
    }

    #[test]
    fn test_phi_type_int_long_is_rejected() {
        let mut g = empty_graph();
        let m = g.add(Op::Merge, IrType::Control, vec![], None);
        let a = value(&mut g, IrType::Int);
        let b = value(&mut g, IrType::Long);
        assert!(
            g.phi_data_type_checked(&[m, a, b]).is_err(),
            "an int/long merge has no common representation"
        );
    }

    #[test]
    fn test_phi_type_float_float() {
        let mut g = empty_graph();
        let m = g.add(Op::Merge, IrType::Control, vec![], None);
        let a = value(&mut g, IrType::Float);
        let b = value(&mut g, IrType::Float);
        assert_eq!(g.phi_data_type_checked(&[m, a, b]), Ok(IrType::Float));
    }

    #[test]
    fn test_phi_type_double_double() {
        let mut g = empty_graph();
        let m = g.add(Op::Merge, IrType::Control, vec![], None);
        let a = value(&mut g, IrType::Double);
        let b = value(&mut g, IrType::Double);
        assert_eq!(g.phi_data_type_checked(&[m, a, b]), Ok(IrType::Double));
    }

    /// The headline regression: a float input must never produce an integer φ.
    #[test]
    fn test_phi_type_float_int_is_rejected_never_int() {
        let mut g = empty_graph();
        let m = g.add(Op::Merge, IrType::Control, vec![], None);
        let f = value(&mut g, IrType::Float);
        let i = value(&mut g, IrType::Int);
        let checked = g.phi_data_type_checked(&[m, f, i]);
        assert!(
            checked.is_err(),
            "float/int is untypeable; it must be reported, not answered `Int`: \
             {checked:?}"
        );
        assert_ne!(checked, Ok(IrType::Int));
    }

    #[test]
    fn test_phi_type_ref_ref_is_ref_not_int() {
        let mut g = empty_graph();
        let m = g.add(Op::Merge, IrType::Control, vec![], None);
        let a = value(&mut g, IrType::Ref);
        let b = value(&mut g, IrType::Ref);
        // Historically this answered `Int`, which hid the merged oop from
        // `zero_ref_phi_slots` and from the deopt `StackSlotRef` classification.
        assert_eq!(g.phi_data_type_checked(&[m, a, b]), Ok(IrType::Ref));
    }

    #[test]
    fn test_phi_type_ref_null_is_ref() {
        let mut g = empty_graph();
        let m = g.add(Op::Merge, IrType::Control, vec![], None);
        let r = value(&mut g, IrType::Ref);
        // A null literal: `Op::Const(0)` typed `Ref` (this IR has no distinct
        // null type — see `is_null_const`).
        let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
        assert!(is_null_const(&g.nodes[null as usize]));
        assert_eq!(g.phi_data_type_checked(&[m, r, null]), Ok(IrType::Ref));
        assert_eq!(g.phi_data_type_checked(&[m, null, r]), Ok(IrType::Ref));
        // An integer zero is NOT a null, and must not launder an int/ref merge
        // into `Ref` (that direction hands the GC a non-oop to dereference).
        let int_zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        assert!(!is_null_const(&g.nodes[int_zero as usize]));
        assert!(g.phi_data_type_checked(&[m, r, int_zero]).is_err());
    }

    #[test]
    fn test_phi_type_ref_int_is_rejected() {
        let mut g = empty_graph();
        let m = g.add(Op::Merge, IrType::Control, vec![], None);
        let r = value(&mut g, IrType::Ref);
        let i = value(&mut g, IrType::Int);
        assert!(g.phi_data_type_checked(&[m, r, i]).is_err());
        assert!(g.phi_data_type_checked(&[m, i, r]).is_err());
    }

    #[test]
    fn test_phi_type_skips_undefined_inputs() {
        let mut g = empty_graph();
        let m = g.add(Op::Merge, IrType::Control, vec![], None);
        let r = value(&mut g, IrType::Ref);
        // An entry edge on which the slot is uninitialised contributes no type
        // information — it must not veto the ref typing.
        assert_eq!(g.phi_data_type_checked(&[m, NO_NODE, r]), Ok(IrType::Ref));
        // …and a φ with nothing but placeholders is an error, not `Int`.
        assert!(g.phi_data_type_checked(&[m, NO_NODE, NO_NODE]).is_err());
        // An out-of-range id is treated exactly like the placeholder.
        let bogus = g.nodes.len() as NodeId + 7;
        assert_eq!(g.phi_data_type_checked(&[m, bogus, r]), Ok(IrType::Ref));
    }

    /// A loop-carried φ is created before its back-edge value exists; appending
    /// that value must re-derive the type (`patch_loop_backedge` → `retype_phi`).
    #[test]
    fn test_loop_carried_phi_retyped_when_second_input_arrives() {
        let mut b = IrBuilder::new(0, 1);
        let region = b.graph.add(Op::Region, IrType::Control, vec![], None);
        // Entry edge: slot uninitialised — no type information, so the φ takes
        // the conservative fallback.
        let entry_inputs = vec![region, NO_NODE];
        let ty0 = b.phi_data_type(&entry_inputs);
        assert_eq!(ty0, PHI_TYPE_FALLBACK);
        let phi = b.graph.add(Op::Phi, ty0, entry_inputs, None);

        // Back-edge value: a reference. After the patch the φ is a GC root.
        let back = b.graph.add(
            Op::New {
                class_id: 7,
                num_fields: 0,
            },
            IrType::Ref,
            vec![],
            None,
        );
        b.graph.nodes[phi as usize].inputs.push(back);
        b.retype_phi(phi);
        assert_eq!(
            b.graph.nodes[phi as usize].ty,
            IrType::Ref,
            "the late back-edge input must retype the loop-carried φ"
        );

        // A late input that does NOT join keeps the already-derived type rather
        // than downgrading it (and is reported, not silently applied).
        let stray = b.graph.add(Op::Const(3), IrType::Int, vec![], None);
        b.graph.nodes[phi as usize].inputs.push(stray);
        b.retype_phi(phi);
        assert_eq!(b.graph.nodes[phi as usize].ty, IrType::Ref);

        // Memory φs are bookkeeping tokens and are never retyped as values.
        let mem_phi = b.graph.add(Op::Phi, IrType::Memory, vec![region, back], None);
        b.retype_phi(mem_phi);
        assert_eq!(b.graph.nodes[mem_phi as usize].ty, IrType::Memory);
    }

    /// The infallible wrapper keeps its old signature and never panics: an
    /// untypeable merge degrades to [`PHI_TYPE_FALLBACK`] (and is reported).
    #[test]
    fn test_phi_data_type_falls_back_on_conflict() {
        let mut b = IrBuilder::new(0, 1);
        let m = b.graph.add(Op::Merge, IrType::Control, vec![], None);
        let r = b.graph.add(
            Op::New {
                class_id: 1,
                num_fields: 0,
            },
            IrType::Ref,
            vec![],
            None,
        );
        let i = b.graph.add(Op::Const(1), IrType::Int, vec![], None);
        assert!(b.phi_data_type_checked(&[m, r, i]).is_err());
        assert_eq!(b.phi_data_type(&[m, r, i]), PHI_TYPE_FALLBACK);
        // The fallback is deliberately NOT `Ref`: an unprovable slot must not
        // be handed to the collector as an oop.
        assert_ne!(PHI_TYPE_FALLBACK, IrType::Ref);
    }

    // ── Option-based id accessors ────────────────────────────────────

    #[test]
    fn test_checked_id_accessors() {
        let mut g = empty_graph();
        let a = g.add(Op::Const(1), IrType::Int, vec![], None);
        let b = g.add(Op::Const(2), IrType::Long, vec![a, NO_NODE], None);

        // node_id_opt / is_valid_id
        assert_eq!(node_id_opt(a), Some(a));
        assert_eq!(node_id_opt(NO_NODE), None);
        assert!(g.is_valid_id(a));
        assert!(g.is_valid_id(b));
        assert!(!g.is_valid_id(NO_NODE));
        assert!(!g.is_valid_id(g.nodes.len() as NodeId));

        // node_opt / type_of
        assert!(g.node_opt(NO_NODE).is_none());
        assert!(g.node_opt(g.nodes.len() as NodeId).is_none());
        assert_eq!(g.type_of(a), Some(IrType::Int));
        assert_eq!(g.type_of(b), Some(IrType::Long));
        assert_eq!(g.type_of(NO_NODE), None);
        g.node_opt_mut(a).unwrap().ty = IrType::Ref;
        assert_eq!(g.type_of(a), Some(IrType::Ref));

        // Node::input_opt — placeholder and out-of-range both read as `None`.
        let n = &g.nodes[b as usize];
        assert_eq!(n.input_opt(0), Some(a));
        assert_eq!(n.input_opt(1), None, "a NO_NODE edge is not an id");
        assert_eq!(n.input_opt(2), None, "out of range");
        let vals: Vec<Option<NodeId>> = n.phi_value_inputs().collect();
        assert_eq!(vals, vec![None]);

        // SafepointSnapshot accessors.
        let snap = SafepointSnapshot {
            bci: 0,
            locals: vec![a, NO_NODE],
            stack: vec![NO_NODE, b],
        };
        assert_eq!(snap.local_opt(0), Some(a));
        assert_eq!(snap.local_opt(1), None);
        assert_eq!(snap.local_opt(9), None);
        assert_eq!(snap.stack_opt(0), None);
        assert_eq!(snap.stack_opt(1), Some(b));
        assert_eq!(snap.stack_opt(9), None);
    }

    /// Real ids are dense from 0 and can never alias the sentinel.
    #[test]
    fn test_allocated_ids_never_alias_the_sentinel() {
        let mut g = empty_graph();
        for i in 0..8 {
            let id = g.add(Op::Const(i), IrType::Int, vec![], None);
            assert_eq!(id, i as NodeId);
            assert_ne!(id, NO_NODE);
            assert!(g.is_valid_id(id));
        }
    }
}
