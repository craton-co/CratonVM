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
use std::sync::atomic::{AtomicU64, Ordering};

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
/// The same goes for an `Op::Const(0)` typed `Long` — `lconst_0` is a zero, not
/// a null — and it is doubly excluded: `join_data_type` has no `Long ⊔ Ref`, so
/// even a caller that mistook one would not be able to merge it with a
/// reference. Only the `Ref`-typed zero [`IrBuilder::aconst_null`] interns is a
/// null.
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

/// Do two TYPED value inputs of this φ input list (`[merge, val_0, …]`) fail
/// to join under [`join_data_type`]?
///
/// This is the *conflict* half of [`Graph::phi_data_type_checked`]'s `Err`
/// only. The other half — no input carries a type at all — is not a conflict
/// and answers `false`, exactly as `ir_verify::check_types` tolerates it.
/// Undefined (`NO_NODE`), out-of-range, removed and `Void` inputs contribute
/// nothing, by the same skip rule both of those use.
///
/// # Why a conflicting merge is a DEAD slot, and why that is sound to act on
///
/// For verified bytecode a category conflict between two values reaching one
/// join is legal only for a slot nothing reads afterwards: the JVM verifier's
/// merge of `int` and `reference` (or `int` and `long`, …) is TOP, and no
/// instruction accepts TOP. The builder may therefore describe such a slot as
/// undefined ([`NO_NODE`]) — which is also what a deopt frame should say for
/// it — rather than build a φ that can only take [`PHI_TYPE_FALLBACK`].
///
/// A loop-carried φ is typed from partial information (its entry edges) when
/// a forward join inside the body asks this question, but the lattice makes
/// that harmless: a conflict seen with a SUBSET of a φ's inputs is still a
/// conflict once the rest arrive, so the answer never flips from "conflict"
/// to "joins".
fn phi_inputs_conflict(graph: &Graph, inputs: &[NodeId]) -> bool {
    let mut joined: Option<IrType> = None;
    for &n in inputs.iter().skip(1) {
        let ty = match graph.node_opt(n) {
            Some(node) if node.op != Op::Dead && node.ty != IrType::Void => node.ty,
            _ => continue,
        };
        joined = match joined {
            None => Some(ty),
            Some(prev) => match join_data_type(prev, ty) {
                Some(j) => Some(j),
                None => return true,
            },
        };
    }
    false
}

// ── Expected operand types ───────────────────────────────────────────
//
// **One table, three readers.** `ir_verify::check_operand_types` reports a
// violation of it, `IrBuilder::add_data` asserts it in debug builds at the
// moment the offending node is created (where the bytecode pc is still in
// hand), and `ir_lower` is meant to read it to pick operand width instead of
// re-deriving the width from node types at each site.
//
// That last reader is the point of the exercise rather than a bonus. Today
// `ir_lower` selects a 64-bit signed `CMP` when either operand of an
// `Op::Cmp` is `Ref` or `Long`, which means one wrongly-`Long`-typed operand
// inverts an `if_icmplt` — ints are stored zero-extended, so a negative int
// reads as a large positive 64-bit value — and one wrongly-`Int`-typed `Ref`
// operand turns `if_acmpeq` into a 32-bit compare, where two distinct objects
// 4 GiB apart compare equal. A verifier and a lowerer reading the same table
// cannot disagree about which of those a node is.
//
// # Staging
//
// The requirements below are enforced only when an operator explicitly asks
// (`CRATONVM_JIT_VERIFY_TYPES=1`); see `ir_verify::operand_type_lane_enabled`.
// A broad new type requirement landing on by default in the same change that
// introduces it risks converting a *modelling* gap — a shape the front end
// emits that this table does not describe — into a fleet-wide silent fallback
// to the single-pass tier, which is a throughput regression with no failing
// test. Read the difftest lane first, then promote.

/// What one input slot of a node is required to hold.
///
/// A requirement, not a description: `Any` means this table makes no claim,
/// never that anything goes in the machine sense.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TypeReq {
    /// No requirement. A call argument (its type lives in the descriptor,
    /// which an `&Op` does not carry), a φ input (the join lattice in
    /// [`join_data_type`] is that requirement), or a slot past the end of the
    /// documented form.
    Any,
    /// A control token.
    Control,
    /// A memory token — deliberately **not** `Exact(IrType::Memory)`. The
    /// builder reuses a value node as a token (`self.mem = load`, where the
    /// load is typed `Int`), so a token slot's declared type is whatever last
    /// wrote memory, and requiring `Memory` here would report every graph the
    /// front end has ever built. The *structural* memory lane is what checks
    /// these slots; this one stays out of its way.
    Memory,
    /// Exactly this type.
    Exact(IrType),
    /// Whatever slot `.0` of this same node holds.
    ///
    /// The homogeneity requirement a fixed type cannot express: `Op::Cmp` is
    /// the IR's `if_icmp*` **and** its `if_acmp*`, so its operands have no one
    /// type — they only have to be the *same* type, which is exactly the fact
    /// `ir_lower` needs and does not check.
    SameAs(usize),
    /// The node's own result type — `Op::Add` and friends, where the result is
    /// the operand type by construction.
    SameAsResult,
}

impl MemKind {
    /// The [`IrType`] a value of this storage kind has in the IR.
    ///
    /// The sub-int kinds collapse to [`IrType::Int`], which is not a rounding
    /// error: this IR has no `Byte`/`Char`/`Short` value type (see
    /// [`join_data_type`]), the narrowing lives in the access, and a `Store`
    /// of `MemKind::Byte` therefore takes an `Int` operand.
    pub fn value_type(self) -> IrType {
        match self {
            MemKind::Int | MemKind::Byte | MemKind::Char | MemKind::Short => IrType::Int,
            MemKind::Long => IrType::Long,
            MemKind::Float => IrType::Float,
            MemKind::Double => IrType::Double,
            MemKind::Ref => IrType::Ref,
        }
    }
}

/// The requirement on input slot `idx` of an `op` node with `arity` inputs.
///
/// `arity` is a parameter rather than a lookup because the memory ops have
/// more than one layout. `ir_optimize::store_operands` and `load_base` accept
/// a *compact* `[base, value]` / `[base]` form built by hand and by the
/// EA bridge, whose slot 1 is a value where the documented form's is a token.
/// [`Op::memory_shape`]'s `min_full_arity` is what tells the two apart, and it
/// is the same number the token-slot model reads, so the two cannot drift.
/// Below that arity this table declines to describe the node at all.
pub fn expected_input_type(op: &Op, idx: usize, arity: usize) -> TypeReq {
    // A compact memory form: not the layout below. See the doc comment.
    if let Some(shape) = op.memory_shape() {
        if arity < shape.min_full_arity {
            return TypeReq::Any;
        }
    }
    // The trailing guard token (see [`guard_token_slot_for`]). `Op::Guard` is
    // `IrType::Void`, so no type requirement can be stated for the slot at
    // all; ARITY is what identifies it, and `ir_verify::check_guard_tokens` is
    // what checks it really names a guard.
    //
    // This arm is load-bearing for `Op::Div`/`Op::Rem` and cosmetic for the
    // three memory ops. `Load`/`ArrayLoad`/`ArrayLength` already answer
    // `TypeReq::Any` past their documented slots; `Div`/`Rem` sit in the
    // homogeneous arithmetic group below, which would demand the guard operand
    // be the division's own result type. Without this line a guarded `idiv`
    // fails the operand-type lane on every graph that has one — i.e. only on
    // real bytecode, never in a unit test that builds a `Load` by hand.
    if guard_token_slot_for(op, arity) == Some(idx) {
        return TypeReq::Any;
    }
    match op {
        // ── Control ───────────────────────────────────────────────────────
        //
        // `Op::Proj`'s input is the multi-output node it projects from, whose
        // declared type is `Control` for an `Op::If` and has been seen as both
        // `Control` and `Void` for `Op::Start` across the graph builders in
        // this crate. Unconstrained rather than wrong.
        Op::Start | Op::Proj(_) => TypeReq::Any,
        Op::Merge | Op::Region => TypeReq::Control,
        // `[ctrl, value]` — the returned value's type is the method's, which
        // is not in the op.
        Op::Return => {
            if idx == 0 {
                TypeReq::Control
            } else {
                TypeReq::Any
            }
        }
        // `[ctrl, cond]`. The condition is the `Int` 0/1 an `Op::Cmp`
        // produces, for both the branch and the guard.
        //
        // `[ctrl, key]` for the multi-way branch, and the requirement is the
        // same shape for a different reason: the key is the `int` the JVMS
        // switch opcodes pop, and `ir_lower` bounds-checks it with a 32-bit
        // `SUB`/`CMP` pair. A `Long`-typed key reaching that pair would have
        // its top half silently discarded, and a `Ref`-typed one would index
        // the jump table with an address.
        Op::If | Op::Switch { .. } | Op::Guard { .. } => {
            if idx == 0 {
                TypeReq::Control
            } else {
                TypeReq::Exact(IrType::Int)
            }
        }
        // `[region, v_per_pred…]`. The value slots are governed by the join
        // lattice, which `ir_verify::check_types` folds directly.
        Op::Phi => TypeReq::Any,

        // ── Leaves ────────────────────────────────────────────────────────
        Op::Const(_) | Op::ConstF(_) | Op::Param(_) => TypeReq::Any,

        // ── Homogeneous arithmetic ────────────────────────────────────────
        Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Rem | Op::Neg | Op::And | Op::Or | Op::Xor => {
            TypeReq::SameAsResult
        }
        // A shift's distance is an `Int` at both widths: `lshl` shifts a
        // `Long` by an `Int`, which is a genuine non-join in the lattice and
        // the reason this is not simply homogeneous.
        Op::Shl | Op::Shr | Op::UShr => {
            if idx == 0 {
                TypeReq::SameAsResult
            } else {
                TypeReq::Exact(IrType::Int)
            }
        }

        // ── Comparisons ───────────────────────────────────────────────────
        //
        // Homogeneous in the operands, unrelated to the result (`Int`). See
        // [`TypeReq::SameAs`] for why this is the requirement that matters.
        Op::Cmp(_) => {
            if idx == 0 {
                TypeReq::Any
            } else {
                TypeReq::SameAs(0)
            }
        }
        Op::LCmp => TypeReq::Exact(IrType::Long),
        Op::FCmp { double, .. } => TypeReq::Exact(if *double {
            IrType::Double
        } else {
            IrType::Float
        }),

        // ── Conversions ───────────────────────────────────────────────────
        //
        // Crossing types is what these are for, so the *source* type is the
        // whole requirement and it is exactly the op's name.
        Op::I2L | Op::I2F | Op::I2D | Op::I2B | Op::I2C | Op::I2S => TypeReq::Exact(IrType::Int),
        Op::L2I | Op::L2F | Op::L2D => TypeReq::Exact(IrType::Long),
        Op::F2I | Op::F2L | Op::F2D => TypeReq::Exact(IrType::Float),
        Op::D2I | Op::D2L | Op::D2F => TypeReq::Exact(IrType::Double),

        // ── Memory ────────────────────────────────────────────────────────
        //
        // `[ctrl, mem, base, offset]` and `[ctrl, mem, base, offset, value]`.
        // The offset is the field index the builder interns as an `Op::Const`.
        Op::Load(_) | Op::Store(_) => match idx {
            0 => TypeReq::Control,
            1 => TypeReq::Memory,
            2 => TypeReq::Exact(IrType::Ref),
            3 => TypeReq::Exact(IrType::Int),
            4 => match op {
                Op::Store(kind) => TypeReq::Exact(kind.value_type()),
                _ => TypeReq::Any,
            },
            _ => TypeReq::Any,
        },
        // `[ctrl, mem, array, index]` and `[ctrl, mem, array, index, value]`.
        Op::ArrayLoad(_) | Op::ArrayStore(_) => match idx {
            0 => TypeReq::Control,
            1 => TypeReq::Memory,
            2 => TypeReq::Exact(IrType::Ref),
            3 => TypeReq::Exact(IrType::Int),
            4 => match op {
                Op::ArrayStore(kind) => TypeReq::Exact(kind.value_type()),
                _ => TypeReq::Any,
            },
            _ => TypeReq::Any,
        },
        // `[ctrl, mem, array]`
        Op::ArrayLength => match idx {
            0 => TypeReq::Control,
            1 => TypeReq::Memory,
            2 => TypeReq::Exact(IrType::Ref),
            _ => TypeReq::Any,
        },
        // `[ctrl, mem]`
        Op::New { .. } | Op::ConstString { .. } | Op::ConstClass { .. } | Op::LoadStatic { .. } => {
            match idx {
                0 => TypeReq::Control,
                1 => TypeReq::Memory,
                _ => TypeReq::Any,
            }
        }
        // `[ctrl, mem, length]`
        Op::NewArray { .. } => match idx {
            0 => TypeReq::Control,
            1 => TypeReq::Memory,
            2 => TypeReq::Exact(IrType::Int),
            _ => TypeReq::Any,
        },
        // `[ctrl, mem, obj]` — the object a monitor locks, the reference a
        // cast or `instanceof` inspects, the exception a `Throw` carries, the
        // receiver an unboxing accessor reads. Each one was in no lane at all
        // before `data_input_indices` grew the three-operand arms, so a
        // control token could reach `jit_throw_exception` as an oop; this is
        // the specific-type half of the same fix.
        Op::InstanceOf { .. }
        | Op::CheckCast { .. }
        | Op::Throw
        | Op::MonitorEnter
        | Op::MonitorExit
        | Op::Unbox { .. } => match idx {
            0 => TypeReq::Control,
            1 => TypeReq::Memory,
            2 => TypeReq::Exact(IrType::Ref),
            _ => TypeReq::Any,
        },
        // `[ctrl, mem, lambda, index]`
        Op::LambdaIntToDouble => match idx {
            0 => TypeReq::Control,
            1 => TypeReq::Memory,
            2 => TypeReq::Exact(IrType::Ref),
            3 => TypeReq::Exact(IrType::Int),
            _ => TypeReq::Any,
        },
        // `[ctrl, mem, args…]`. The argument types are the callee
        // descriptor's, and nothing here carries it.
        Op::Call { .. } => match idx {
            0 => TypeReq::Control,
            1 => TypeReq::Memory,
            _ => TypeReq::Any,
        },

        // ── Intrinsics ────────────────────────────────────────────────────
        //
        // `ScalarOp::input_ty` is documented as "the single source of truth"
        // for each family's operand types — `Long.rotateLeft` takes a `long`
        // value and an `int` distance — and until this table existed nothing
        // consulted it, so `ScalarOp::MinL` fed two `Int` nodes verified clean
        // and lowered to a 64-bit `CMP`/`CMOV`.
        Op::ScalarIntrinsic(s) => {
            if idx < s.arity() {
                TypeReq::Exact(s.input_ty(idx))
            } else {
                TypeReq::Any
            }
        }

        // A removed node makes no claims.
        Op::Dead => TypeReq::Any,
    }
}

/// Resolve [`expected_input_type`] into the concrete **value** type slot `idx`
/// must have, or `None` when this table makes no claim about it.
///
/// `ty_of` reads the declared type of another input, which is what
/// [`TypeReq::SameAs`] needs; it answers `None` for a slot whose node cannot
/// be typed, and an unresolvable requirement constrains nothing rather than
/// failing.
///
/// # Why a token slot resolves to `None`
///
/// [`TypeReq::Control`] and [`TypeReq::Memory`] both answer `None` here, and
/// that is not the same statement for the two of them:
///
/// * a memory slot genuinely has no required type (the builder reuses a value
///   node as a token — see [`TypeReq::Memory`]);
/// * a control slot does have one, and `ir_verify`'s **structural** lane
///   already owns it. `control_input_indices` / `produces_control` check every
///   control edge unconditionally, in every build profile, and have done so
///   since before this table existed. Resolving `Control` to `IrType::Control`
///   here would add a second opinion about the same edge, enforced under a
///   different flag, which is precisely the duplication the table exists to
///   remove. The layout information is still recorded — `ir_lower` reads
///   [`expected_input_type`] to know *which slot is the control edge* — it is
///   only the type assertion that stays in one place.
pub fn required_input_type(
    op: &Op,
    result_ty: IrType,
    inputs: &[NodeId],
    idx: usize,
    ty_of: &dyn Fn(NodeId) -> Option<IrType>,
) -> Option<IrType> {
    match expected_input_type(op, idx, inputs.len()) {
        TypeReq::Any | TypeReq::Memory | TypeReq::Control => None,
        TypeReq::Exact(t) => Some(t),
        TypeReq::SameAsResult => Some(result_ty),
        TypeReq::SameAs(other) => ty_of(*inputs.get(other)?),
    }
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

    /// Mirror the comparison about its operands: the `op` for which
    /// `a op b` and `b self.swapped() a` state the same fact.
    ///
    /// Distinct from [`CmpOp::negate`], which states the OPPOSITE fact about
    /// the same operand order. Confusing the two is a sign error that reads as
    /// a working optimisation on symmetric test data (`Eq`/`Ne` are fixed
    /// points of both), so they are named for what they do rather than for the
    /// direction the arrow ends up pointing.
    pub fn swapped(self) -> Self {
        match self {
            CmpOp::Eq => CmpOp::Eq,
            CmpOp::Ne => CmpOp::Ne,
            CmpOp::Lt => CmpOp::Gt,
            CmpOp::Le => CmpOp::Ge,
            CmpOp::Gt => CmpOp::Lt,
            CmpOp::Ge => CmpOp::Le,
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

    /// A multi-way branch on an `Int` key — `tableswitch` / `lookupswitch`.
    /// Inputs `[ctrl, key]`.
    ///
    /// `low` and `high` are the INCLUSIVE key bounds of a DENSE index space:
    /// output `Proj(k)` is the edge for key `low + k`, for every
    /// `k in 0 ..= high - low`, and `Proj(high - low + 1)` is the default
    /// edge. A key outside `[low, high]` takes the default edge, and so does
    /// a key inside it whose own projection was wired to the default target —
    /// which is how a `lookupswitch` (and a `tableswitch` with holes) is
    /// normalised into this form: every unlisted key in the span gets a
    /// projection pointing at the default merge target, so the JVMS edge set
    /// is preserved EXACTLY, including two cases that name the same target.
    ///
    /// # Why the dense form, and why the builder may refuse it
    ///
    /// The dense form is what makes a jump table expressible without carrying
    /// a key vector on every `Op` in the graph: the lowerer recovers key `k`'s
    /// edge by index, not by search. It costs one projection per span SLOT
    /// rather than per case, so a sparse key set would cost more IR nodes than
    /// the `Op::Cmp`/`Op::If` chain it replaces. Two predicates decide, both
    /// next to this op and both reading the same `i64` span arithmetic a
    /// full-`int`-range `tableswitch` needs: [`switch_is_representable`] is
    /// what the builder asks before building one at all, and
    /// [`switch_is_dense`] is what `ir_lower` asks before putting one in a
    /// jump table.
    ///
    /// A `Switch` has at most [`SWITCH_MAX_PROJECTIONS`] outputs -- a budget
    /// inside `Op::Proj`'s `u16` index, not the width of it; both predicates
    /// enforce it, and a switch over a wider span keeps the compare chain.
    ///
    /// Produces control, touches no memory, and cannot raise: the default
    /// edge covers every key, so there is no "no arm matched" outcome to
    /// throw.
    Switch {
        low: i32,
        high: i32,
    },

    /// Control-flow merge of multiple predecessors (forward join).
    /// Inputs: `[ctrl_0, ctrl_1, …]`.
    Merge,

    /// Loop header merge — like Merge but marks a loop back-edge.
    /// Inputs: `[ctrl_entry, ctrl_backedge]`.
    Region,

    // ── Projection ───────────────────────────────────────────────────
    /// Extract the Nth output from a multi-output node.
    /// Input: `[multi_node]`.
    Proj(u16),

    // ── Constants ────────────────────────────────────────────────────
    /// Integer / long constant (no inputs).
    Const(i64),

    /// Float / double constant stored as raw bits (no inputs).
    ConstF(u64),

    // ── Constant-pool constants that are NOT immediates (cov-01) ─────
    //
    // These three are constants in the bytecode's sense and runtime calls in
    // the machine's. Each names a constant-pool SITE and materialises its value
    // on every execution through the same helper the single-pass backend uses,
    // and each therefore carries `[ctrl, mem]` and the `MemAccess::Opaque`
    // memory shape `Op::Call` has: a safepoint, an allocation, and a barrier
    // nothing reorders across.
    //
    // Re-materialising rather than baking is not a missed optimisation — it is
    // the correctness requirement. An `ObjectRef` baked at compile time is
    // stale the moment a relocating collector moves it, and `<clinit>` is a
    // side effect the constant owes on first touch. See each variant.
    /// `ldc <String>` — the interned literal named at `cp_idx` in
    /// `holder_class_id`'s constant pool. Inputs `[ctrl, mem]`, result `Ref`.
    ///
    /// Lowered as a call to `helpers.ldc_string_cp`. CP-indexed rather than
    /// bytes-indexed because that is the key JVMS §5.4.3's recorded resolution
    /// is filed under; the predecessor shape baked the literal's UTF-8 and so
    /// had to re-derive the object through the string pool on every execution.
    /// The object is still fetched at run time — a baked `ObjectRef` is stale
    /// the moment a relocating collector moves it — but now from a record
    /// keyed by the site rather than by the literal's content.
    ///
    /// `slot_addr` (round 9 wave 8, irl8; request G of
    /// `perf-compiled-ldc-string-is-a-helper-call-per-execution`): the address
    /// of the VM's GC-maintained word for this site
    /// (`JitLdcConstant::StringSlot`, `vm::jit::helpers::compiled_ldc_slot_for`),
    /// or `0`. With one, the lowering reads the word first and calls the
    /// helper only when it is still empty -- the single-pass
    /// `emit_ldc_cp_site` probe. The word is a root the collector rewrites in
    /// place while mutators are parked, so the read is never stale.
    ConstString {
        holder_class_id: u32,
        cp_idx: u16,
        slot_addr: usize,
    },

    /// `ldc <Class>` — the mirror of the class named at `cp_idx` in
    /// `holder_class_id`'s constant pool. Inputs `[ctrl, mem]`, result `Ref`.
    ///
    /// Lowered as a call to `helpers.ldc_class_cp`. CP-indexed rather than
    /// class-id-indexed for the two reasons `JitLdcConstant::ClassMirror`
    /// records: the target may not be loaded yet, and loading it is a user
    /// `ClassLoader.loadClass` that must not run inside the compiler. A `0`
    /// return is a published pending exception, not a value.
    ///
    /// `slot_addr`: as [`Op::ConstString`]'s (`JitLdcConstant::ClassMirrorSlot`).
    ConstClass {
        holder_class_id: u32,
        cp_idx: u16,
        slot_addr: usize,
    },

    /// `getstatic` — read the static field at `field_index` of `class_id`.
    /// Inputs `[ctrl, mem]`; result typed from `type_tag`.
    ///
    /// Two lowerings, chosen exactly as the single-pass backend's `0xb2` arm
    /// chooses: a direct load through the class's baked base-POINTER cell when
    /// `DirectHelperTable::resolve_static_base` accepts the site (which it does only for a
    /// class already initialised at compile time), and `helpers.getstatic`
    /// otherwise — that helper runs `<clinit>` on first touch and reports a
    /// failure as the `i64::MIN` deopt sentinel with a pending Java exception.
    ///
    /// `putstatic` has no counterpart here on purpose: a static reference WRITE
    /// owes an SATB pre-barrier that no collector `set_field` barrier covers,
    /// which is why the single-pass backend keeps writes on their helpers.
    LoadStatic {
        class_id: u32,
        field_index: u32,
        type_tag: u8,
        is_volatile: bool,
    },

    // ── cov-05 ────────────────────────────────────────────────────
    /// `instanceof` (0xc1) against a class already resolved and loaded at
    /// compile time. Inputs `[ctrl, mem, obj]`, result `Int` (0 or 1).
    ///
    /// Lowered as a call to `helpers.instanceof_check` — the SAME helper the
    /// single-pass backend's 0xc1 arm calls, with the identical
    /// `(vm_ptr, obj_ptr, name_ptr, name_len)` ABI, so this node reuses the
    /// single-pass backend's subtype answer (including the strict-vs-lenient
    /// array rule and every recorded typecheck defect fix) rather than
    /// re-deriving one. `Opaque` because the helper's fast path can still
    /// allocate a `java/lang/Class` mirror on first touch (a GC-triggering
    /// safepoint) even though the *target* class is already loaded — see
    /// `Op::ConstString` for why re-materialising through the shared helper,
    /// not baking a value, is the correctness requirement here too.
    ///
    /// The name is resolved and its target confirmed loaded once, at compile
    /// time (`(name_ptr, name_len)` baked from the artifact's own owned
    /// string table, kept alive on `CompiledMethod::_jit_strings`) — an
    /// `instanceof` whose target is not yet loaded is refused per-site by the
    /// caller (`lib.rs`'s `instanceof_info` construction), never reaching
    /// this node, because the not-yet-loaded resolution path can run a user
    /// classloader. Never throws and has no control-flow consequence, unlike
    /// its sibling `checkcast` (0xc0) below. See
    /// `cov-05-checkcast-and-instanceof-RETIRED-*.md`.
    InstanceOf {
        name_ptr: usize,
        name_len: usize,
    },

    /// `checkcast` (0xc0) against a class already resolved and loaded at
    /// compile time — `instanceof`'s throwing sibling. Inputs
    /// `[ctrl, mem, obj]`, result `Ref` (the same reference, or `null`).
    ///
    /// Lowered as a call to `helpers.checkcast` — same helper, same ABI, same
    /// loaded-only admission gate as `Op::InstanceOf` above, and the same
    /// reason (`lib.rs`'s `checkcast_info` construction: the not-yet-loaded
    /// resolution path can run a user classloader).
    ///
    /// Unlike `instanceof`, a definitive refusal THROWS
    /// `ClassCastException` — the helper stashes it and returns the
    /// `i64::MIN` sentinel, exactly the protocol `helpers.ldc_class_cp`
    /// (`Op::ConstClass`) already uses for a resolution failure. This is
    /// **not** the same machinery `athrow` (0xbf) needs: it is the generic
    /// "helper failed, drain the pending exception through the shared
    /// epilogue" protocol every fallible `Opaque` call in this tier already
    /// has (`Lowerer::emit_call_return_check`), not a jump into a LOCAL
    /// exception-table handler inside this compiled frame — the whole-method
    /// `precise_exception_frames` admission term already excludes the one
    /// case (a handler reading a non-parameter local) where that distinction
    /// would matter, independently of which opcode throws. `checkcast` and
    /// `athrow` are independent lanes; see the cov-05 retirement doc for the
    /// full reasoning.
    CheckCast {
        name_ptr: usize,
        name_len: usize,
    },

    // ── cov-07 ────────────────────────────────────────────────────
    /// `athrow` (0xbf). Inputs `[ctrl, mem, exc]`, produces no value — it is a
    /// terminator, like [`Op::Return`], never a data node.
    ///
    /// Lowered as a call to `helpers.throw_exception` (`jit_throw_exception`),
    /// the SAME helper and the SAME `(exc_ptr, bci)` ABI the single-pass
    /// backend's `0xbf` arm already uses — this is not a second, divergent
    /// answer to where an exception goes. That helper always stashes the
    /// exception (or, on a null reference, the JVMS-mandated NPE) and returns
    /// the `i64::MIN` deopt sentinel; it never branches to an in-method
    /// handler itself. `route_jit_exception_through_method`
    /// (`vm/src/runtime/interpreter/exception_dispatch.rs`) does that
    /// resolution afterwards, in the INTERPRETER, from the bci baked into this
    /// call — exactly the mechanism `Op::CheckCast`'s helper-thrown
    /// `ClassCastException` already reaches via the shared
    /// `emit_call_return_check` sentinel-drain protocol. See
    /// `cov-07-athrow-RETIRED-*.md` for the full reasoning,
    /// including why `escape_analysis::program_order_proves_dominance` does
    /// NOT need a `may_throw` term for this: a throw always LEAVES the frame,
    /// it never creates a control edge back into normal flow.
    Throw,

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
    /// Division. Inputs: `[a, b]`, or `[a, b, guard]` — see
    /// [`Op::guard_shape`]. The optional trailing edge names the `Op::Guard`
    /// `IrBuilder::add_div_zero_guard` anchors beside it, which is the node
    /// that owns this division's `ArithmeticException`.
    Div,
    /// Remainder. Inputs: `[a, b]`, or `[a, b, guard]` — see [`Op::Div`].
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
    /// Array / field load.  Inputs: `[ctrl, mem, base, index/offset]`, or
    /// `[ctrl, mem, base, index/offset, guard]` — see [`Op::guard_shape`].
    Load(MemKind),

    /// Array / field store.  Inputs: `[ctrl, mem, base, index/offset, value]`.
    Store(MemKind),

    /// Array length.  Inputs: `[ctrl, mem, array_ref]`, or
    /// `[ctrl, mem, array_ref, guard]` — see [`Op::guard_shape`]. Hand-built
    /// optimizer fixtures also use the compact `[array_ref]`.
    ArrayLength,

    /// Array ELEMENT load. Inputs: `[ctrl, mem, array, index]`, or
    /// `[ctrl, mem, array, index, guard]` — see [`Op::guard_shape`]. Unlike
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
    ///
    /// Two shapes, distinguished by `element_type`:
    ///   * `newarray` (0xbc) — a PRIMITIVE array. `element_type` is the JVM
    ///     atype tag (4-11: `T_BOOLEAN`..`T_LONG`); `component_class_id` is
    ///     unused (`0`). No class resolution, no deferred path — the atype
    ///     names the element kind directly.
    ///   * `anewarray` (0xbd) — a REFERENCE array of an already-loaded
    ///     component class. `element_type` is `0` (not a valid atype — JVM
    ///     atypes start at 4), and `component_class_id` names the loaded
    ///     class. A component class not yet loaded at compile time (the
    ///     deferred case) is refused by the builder, exactly like an
    ///     unresolved `new`.
    NewArray {
        element_type: u8,
        component_class_id: u32,
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

    // ── Synchronization ──────────────────────────────────────────────
    /// `monitorenter` on an object. Inputs: `[ctrl, mem, obj]`.
    ///
    /// Like `Op::Store` the node **is** the new memory token: it consumes the
    /// prior one at slot 1 and every later access chains off it. Its effect is
    /// [`MemEffect::monitor_enter`] — a read *and* a write of
    /// [`AliasClass::Monitor`], a safepoint, and a JMM **acquire**, so nothing
    /// after it may move above it.
    ///
    /// Impure and **not** removable by a liveness sweep. A monitor is
    /// observable in three ways no value consumer witnesses: an unbalanced
    /// pair throws `IllegalMonitorStateException`, the lock is a
    /// happens-before edge for other threads, and a live monitor is an
    /// identity observation that blocks scalar replacement. The only pass
    /// licensed to delete one is `escape_analysis`'s lock elision, and only by
    /// applying a whole balanced plan at once — see
    /// `docs/jit/lock-elimination.md` §4 and §6.1.
    MonitorEnter,

    /// `monitorexit` on an object. Inputs: `[ctrl, mem, obj]`.
    ///
    /// The release half of [`Op::MonitorEnter`]: nothing before it may move
    /// below it. Everything that variant says about token threading and about
    /// deletion applies here unchanged — the two are only ever removed as a
    /// balanced pair.
    MonitorExit,

    // ── Speculation guard (real-frame-deopt) ─────────────────────────
    /// Speculative guard. Inputs: `[ctrl, cond]`. If `cond` is zero at
    /// runtime the guard fails and control transfers to the deopt
    /// trampoline, reconstructing the interpreter frame for `bci`. A
    /// non-zero `cond` falls through with no effect. Produces no value and
    /// is impure (must not be DCE'd or reordered past side effects), so it
    /// is neither `is_pure` nor `is_control`.
    ///
    /// A guard is also the PRODUCER of the guard token edge: the nodes it
    /// licenses name it as a trailing input, which is how "the guard ran
    /// first" stops being a fact about the order inside one block. See
    /// [`Op::guard_shape`] for which ops carry the edge and
    /// [`guard_token_edges_enabled`] for the gate that emits it.
    Guard {
        /// Bytecode index whose frame state to reconstruct on failure.
        bci: usize,
    },

    /// A call-site intrinsic this tier lowers to arithmetic instead of a call.
    ///
    /// # Why this exists, and why it is ONE op with a sub-enum
    ///
    /// `try_compile_inner`'s invoke-planning loop refuses the WHOLE METHOD when
    /// any call site is a `JitIntrinsic` the optimizing tier cannot emit. On
    /// the H2 JDBC workload that was **68 methods, 100% of every invoke-plan
    /// discard** -- `Math.max`, `Integer.compare`, `Long.numberOfLeadingZeros`
    /// and friends, each of them costing a whole method's optimization.
    ///
    /// The obvious repair -- let the site take an ordinary `Op::Call` -- is a
    /// DOWNGRADE, and a large one: a dispatch is a ~300 ns floor where the
    /// single-pass backend emits two instructions. So the families whose
    /// intrinsic is a handful of straight-line instructions get lowered here as
    /// arithmetic, which is strictly better than either alternative (a node the
    /// optimizer can fold, GVN and schedule). The families whose intrinsic
    /// replaces a LOOP -- `System.arraycopy`, `Arrays.fill`/`equals`/`sort`,
    /// `String.equals`/`indexOf`/`hashCode`, CRC32 -- keep the method-level
    /// refusal, because there the intrinsic really is worth more than the rest
    /// of the method's optimization. See [`try_ir_scalar_intrinsic`].
    ///
    /// One op with a sub-enum rather than eight ops, because every new `Op`
    /// arm has to be added to `ir_verify`'s arity table, `regalloc`'s
    /// `ir_op_defines_value` and `ir_lower`'s `op_defines_result_slot` -- and
    /// the last two are the pair `the_two_value_defining_enumerations_agree`
    /// exists to keep in lockstep after a drift that silently disabled register
    /// residency on every array-touching method. Eight chances to get that
    /// wrong is eight too many.
    ///
    /// Inputs are `[a]` for unary and `[a, b]` for binary; [`ScalarOp::arity`]
    /// is the single source of truth and `ir_verify` reads it.
    ScalarIntrinsic(ScalarOp),
    /// An unboxing accessor lowered inline: null check, exact receiver class
    /// guard, per-object compact/legacy layout branch, then the payload load.
    /// Inputs `[ctrl, mem, obj]`; result `Long` or `Int`.
    ///
    /// Carries only the guard class id -- the byte offsets are re-derived at
    /// lowering from `unbox_offsets`, the same resolver the single-pass backend
    /// asks, so the two cannot disagree about where the field lives.
    ///
    /// A null receiver or a class mismatch DEOPTS rather than throwing here:
    /// the interpreter re-runs the call and raises the NPE, or dispatches to
    /// the override that made the guard fail. `AtomicLong` and friends are not
    /// final, which is why the guard is not optional.
    Unbox {
        op: UnboxOp,
        class_id: u32,
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
            Op::Start
                | Op::Return
                | Op::Throw
                | Op::If
                | Op::Switch { .. }
                | Op::Merge
                | Op::Region
                | Op::Proj(_)
        )
    }

    /// True if this node is pure (no side effects, safe to GVN / eliminate).
    ///
    /// # Why the three-way comparisons are here
    ///
    /// [`Op::LCmp`] and [`Op::FCmp`] were absent until 2026-09-17, and their
    /// absence cost far more than a missed GVN. `is_pure` was the predicate
    /// `ir_optimize` asked when it wanted to know whether a node can touch
    /// memory — `loop_has_hard_barrier`, `preheader_may_trap`,
    /// `loop_writes_memory` and `is_memory_barrier` were all spelled
    /// `!op.is_pure() && !op.is_control() && !matches!(op, Load|Store|Phi|Dead)`
    /// — so a node missing from this list is classified as "may read or write
    /// arbitrary memory". That made **every loop containing an `lcmp`,
    /// `fcmpl`, `fcmpg`, `dcmpl` or `dcmpg` lose all LICM**: `while (i < n)`
    /// over `long` counters, and every FP comparison in a kernel loop.
    ///
    /// Both are total, side-effect-free arithmetic on exactly two value inputs
    /// with no control and no memory edge (`ir_verify::expected_arity` gives
    /// them `(2, 2)`), and both are deterministic functions of their operands
    /// — `FCmp`'s NaN polarity rides in the op itself (`nan_greater`), so two
    /// `FCmp`s GVN together only when they are the same comparison. Listing
    /// them is therefore sound for every consumer at once: GVN may now collapse
    /// duplicate comparisons, DCE may drop a dead one, and the unroller may
    /// clone one.
    ///
    /// `Op::Div`/`Op::Rem` stay excluded because they trap, and `Op::Phi`'s
    /// exclusion is deliberate (a φ is not a value computation).
    ///
    /// The deeper point — that four call sites were asking a *memory-shape*
    /// question with a *purity* predicate, and would misclassify the next
    /// pure-but-unlisted op the same way — is closed: [`memory_shape_of`] and
    /// [`may_raise`] are those two questions, both exhaustive over `Op` so a
    /// new variant cannot be forgotten by either, and `ir_optimize`'s four
    /// sites read them. **This list is once again only about GVN and DCE**;
    /// adding an op to it no longer silently changes what LICM will hoist.
    pub fn is_pure(&self) -> bool {
        matches!(
            self,
            Op::Const(_)
                | Op::ConstF(_)
                | Op::Param(_)
                | Op::ScalarIntrinsic(_)
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
                | Op::LCmp
                | Op::FCmp { .. }
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

// ── `Op::Switch` admission: the density rules, in ONE place ──────────
//
// `IrBuilder`'s `tableswitch`/`lookupswitch` arm asks `switch_is_representable`
// before it builds an `Op::Switch` at all; `ir_lower`'s `Op::Switch` arm asks
// `switch_is_dense` to pick between a jump table and a binary search. Two
// questions, one span calculation, and both of them here: a second copy of
// this arithmetic somewhere else is how a builder comes to emit a node the
// lowerer handles badly.

/// The most outputs an [`Op::Switch`] may have, cases and default together.
///
/// # A BUDGET, deliberately chosen -- and no longer the width of `Op::Proj`
///
/// Until 2026-09-17 this WAS the width: `Op::Proj` carried a `u8`, so it was
/// `u8::MAX + 1` and "256" was not a choice but a ceiling. Every switch over
/// 255 dense cases kept the `Cmp`/`If` compare chain -- five IR nodes and a
/// branch per case -- where a jump table would do. `NOTES-opts8.md` section 2
/// recorded widening it as an optimisation its lane could not compile.
///
/// `Op::Proj` is now a `u16`, and this is a budget inside that width:
///
/// * **2048** covers the 256..~2000-case range the note identified as the one
///   that benefits, and caps a worst-case jump table at 2048 eight-byte slots.
/// * Raising it is NOT free the way raising a width is. A dense switch costs
///   one `Op::Proj` per span slot and a merge with that many predecessors, and
///   [`SWITCH_MAX_WASTE_FACTOR`] lets up to eight slots stand for one live
///   case, so the node count can exceed the chain it replaces by 8/5 on a
///   sparse switch. For a DENSE one it is ~1 node per case against ~5, so this
///   budget is a code-size bound, not a compile-time one.
/// * It must never exceed what `Op::Proj` can name, or the two `as u16` casts
///   in the switch builder become a live truncation -- projection 65536
///   aliasing projection 0, a jump to the wrong arm. The `const` assertion
///   below makes that a compile error rather than a test.
pub const SWITCH_MAX_PROJECTIONS: usize = 2048;

// The budget must fit the width. A `const` assertion rather than a test: a test
// runs after the build, and by then a too-large budget has already produced a
// binary whose switch builder can truncate a projection index.
const _: () = assert!(
    SWITCH_MAX_PROJECTIONS <= u16::MAX as usize + 1,
    "SWITCH_MAX_PROJECTIONS exceeds what Op::Proj's u16 index can name"
);

/// The largest span (`high - low + 1`) an [`Op::Switch`] may cover: one output
/// per span slot, plus the default edge, must fit in
/// [`SWITCH_MAX_PROJECTIONS`].
pub const SWITCH_MAX_SPAN: i64 = SWITCH_MAX_PROJECTIONS as i64 - 1;

/// How much of the dense projection range the builder will let a switch waste
/// on default-target slots before it gives up and keeps the compare chain.
///
/// The dense form costs one `Op::Proj` per span SLOT and the chain costs five
/// nodes per LISTED CASE, so the two break even at a waste factor of about
/// five and the dense form is a loss above it. Eight rather than five because
/// the projections are also what makes a binary search possible at all, and a
/// search over `n` keys is worth strictly more at run time than a chain over
/// the same `n` — so a little past break-even on nodes is still ahead on the
/// thing the item is about.
///
/// A switch past this factor is genuinely sparse — `case 1: case 1000:` — and
/// for a genuinely sparse switch with few cases the chain is the right answer
/// anyway.
pub const SWITCH_MAX_WASTE_FACTOR: i64 = 8;

/// Below this many live case edges a jump table loses to a compare chain, so
/// `ir_lower` emits the chain.
///
/// An indirect branch through a table is one the branch predictor gets wrong
/// on anything but a repeating key, and a mispredict costs more than the four
/// `CMP`/`JE` pairs it replaced. The single-pass backend's `0xaa` arm has used
/// the same threshold since it was written; the two tiers agreeing about where
/// the crossover is, is worth more than either being individually optimal.
pub const SWITCH_CHAIN_MAX_CASES: usize = 4;

/// May a switch over the inclusive key range `[low, high]` with `cases` live
/// case edges be BUILT as an [`Op::Switch`] at all?
///
/// This is the builder's question. [`switch_is_dense`] is the lowerer's, it is
/// strictly stronger, and the ordering between them is the whole point of
/// having two:
///
/// * if the builder admitted only what the lowerer would put in a table, the
///   binary-search shape would be unreachable — every node the lowerer ever
///   saw would already be table-dense, and the `lookupswitch`-shaped case the
///   item is really about would keep the chain forever;
/// * if the builder admitted anything at all, a `tableswitch` whose payload is
///   250 default slots around two real cases would cost 251 projections and a
///   251-predecessor merge to replace ten nodes of chain.
///
/// So: admit up to [`SWITCH_MAX_WASTE_FACTOR`] slots per live case, and let
/// `ir_lower` decide between a table (dense), a search (sparse) and a chain
/// (small) inside that. `switch_is_dense(l, h, c)` implies
/// `switch_is_representable(l, h, c)` for every input — asserted in this
/// file's tests, because a lowerer shape the builder cannot produce is a dead
/// arm and a builder shape the lowerer has no rule for is a wrong answer.
pub fn switch_is_representable(low: i32, high: i32, cases: usize) -> bool {
    if high < low || cases == 0 {
        return false;
    }
    let span = i64::from(high) - i64::from(low) + 1;
    if span > SWITCH_MAX_SPAN {
        return false;
    }
    // `span` is at most 255 here, so neither the widening nor the
    // multiplication can overflow.
    span <= (cases as i64).saturating_mul(SWITCH_MAX_WASTE_FACTOR)
}

/// Is a switch over the inclusive key range `[low, high]` with `cases` live
/// case edges dense enough to be worth a JUMP TABLE?
///
/// # The `i64` is the point
///
/// `high - low + 1` **overflows `i32`** for the switch JVMS permits and javac
/// will not emit but an adversarial class file can: `tableswitch low=i32::MIN
/// high=i32::MAX` is a legal instruction whose span is 2^32. Computed in `i32`
/// the span comes out as `0`, which reads as "perfectly dense" and asks the
/// lowerer for a jump table with 4 294 967 296 entries. Computed in `i64` it
/// comes out as 2^32 and fails [`SWITCH_MAX_SPAN`] on the first test below.
///
/// `x64::switch_validation::checked_tableswitch_count` already refuses the
/// *decode* of a table with more than `1 << 24` entries, so the worst span
/// that reaches here from a real `tableswitch` payload is bounded — but a
/// `lookupswitch` reaches here with a span bounded by NOTHING (two pairs,
/// `i32::MIN` and `i32::MAX`, is 16 bytes of payload and a 2^32 span), which
/// is why the bound is stated here rather than inherited.
///
/// # The two tests
///
/// * the span fits the projection budget ([`SWITCH_MAX_SPAN`]);
/// * the span wastes at most one table slot per case (`span <= 2 * cases`),
///   which is the classic jump-table density rule. Below it a table is four
///   bytes per slot and one indirect branch; above it a binary search over the
///   live keys is smaller and its compares predict better.
///
/// `cases` is the number of LIVE case edges — the keys that go somewhere other
/// than the default target. A `tableswitch`'s payload lists one entry per slot
/// and javac fills its holes with the default offset, so counting payload
/// entries instead would call every `tableswitch` perfectly dense however many
/// of its slots do nothing.
pub fn switch_is_dense(low: i32, high: i32, cases: usize) -> bool {
    if high < low || cases == 0 {
        return false;
    }
    let span = i64::from(high) - i64::from(low) + 1;
    if span > SWITCH_MAX_SPAN {
        return false;
    }
    // `cases` is a `usize` from a decoded payload capped at `1 << 24`; the
    // widening cannot overflow and the doubling cannot either, because `span`
    // is already known to be at most 255 by the test above.
    let cases = cases as i64;
    span <= cases.saturating_mul(2)
}

/// The number of projections an [`Op::Switch`] over `[low, high]` has: one per
/// span slot, plus the default edge.
///
/// `i64` for the reason [`switch_is_dense`] spells out, and saturating into
/// `usize` rather than wrapping, so a node that somehow escaped the density
/// rule fails `ir_verify`'s projection lane loudly instead of computing a
/// plausible small arity from an overflowed subtraction.
pub fn switch_projection_count(low: i32, high: i32) -> usize {
    let span = i64::from(high) - i64::from(low) + 1;
    span.saturating_add(1).clamp(0, i64::from(u32::MAX)) as usize
}

// ── Compact edge lists ───────────────────────────────────────────────

/// Number of edges an [`Inputs`] list stores inline before spilling to the heap.
///
/// Picked from the arity of the node constructors in this file — every
/// non-test `Graph::add` call site, counted mechanically:
///
/// | inputs | call sites | constructors |
/// |--------|-----------:|--------------|
/// | 0      | 6          | `Const`, `ConstF`, `Param`, `Start` |
/// | 1      | 10         | `Proj`, `Neg`, the `I2L`/`L2I`/… conversions |
/// | 2      | 7          | `Add`/`Sub`/`Mul`/`Cmp`, `Return [ctrl, val]`, `If [ctrl, cmp]` |
/// | 3      | 0          | — |
/// | 4      | 3          | `Load [ctrl, mem, base, offset]` |
/// | 5      | 3          | `Store [ctrl, mem, base, offset, value]` |
///
/// 29 fixed-arity sites, none wider than 5, plus 11 variable-arity ones: `Phi`
/// (`1 + preds`, 3 sites), `Merge`/`Region` (`preds`) and `Op::Call`
/// (`[ctrl, mem, args…]`, i.e. `2 + args`). A capacity of **5** therefore
/// covers every fixed-arity constructor in the IR — including the widest one,
/// `Op::Store` — a φ over up to four predecessors, and a call with up to three
/// arguments. Only genuinely wide merges and many-argument calls spill. (The
/// *dynamic* distribution is even more concentrated: `Op::Const` and
/// `Op::Param` nodes dominate a real graph and carry no edges at all.)
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

    /// Identity of this thread, so a graph cannot mistake *another* thread's
    /// epoch for its own.
    ///
    /// The epoch is per-thread (a process-wide counter would let one compiler
    /// thread's edge write invalidate every other thread's use lists, which
    /// costs a rebuild per pass and gives back the whole win). A graph is
    /// `Send`, though, so it could in principle be stamped on one thread, made
    /// stale there, and then consulted on a thread whose counter happens to
    /// hold the same value. Pairing the epoch with a thread token makes that
    /// coincidence impossible: a graph reconciled on another thread simply
    /// reads as stale.
    static THREAD_TOKEN: u64 = NEXT_THREAD_TOKEN.fetch_add(1, Ordering::Relaxed);
}

/// Source of the per-thread tokens. Never 0, so `UseLists::new()` (owner 0)
/// can never be mistaken for a reconciled state.
static NEXT_THREAD_TOKEN: AtomicU64 = AtomicU64::new(1);

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

/// This thread's identity. See [`THREAD_TOKEN`].
#[inline]
fn thread_token() -> u64 {
    THREAD_TOKEN.with(|t| *t)
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
    /// Index into [`Graph::safepoints`] of the snapshot describing the
    /// interpreter frame at THIS node's program point, when that cannot be
    /// found by searching for [`Self::bytecode_pc`].
    ///
    /// # Why a bci is not enough
    ///
    /// `ir_lower` resolves a deopt frame by scanning `graph.safepoints` for the
    /// first snapshot whose `bci` matches the trapping node's `bytecode_pc`.
    /// That is exact while a bci appears once in the graph — which it does,
    /// until a pass COPIES a region. A cloned node keeps the original's
    /// `bytecode_pc` (it must: the pc is also the site key for the compact
    /// field offset, the direct-call entry and the inline-cache pair, and
    /// rewriting it makes a copy read another site's metadata). So after an
    /// unroll there are `trip` nodes at one bci, the by-bci scan finds copy 0's
    /// snapshot for every one of them, and copies `1..trip` deopt into a frame
    /// describing values they do not hold.
    ///
    /// That is the shape of the defect
    /// `internal/fixed-bugs/` records against the deopt metadata — a frame slot
    /// read from a word nothing had written, surfacing as an H2 `GROUP BY` that
    /// returned 3 rows of 5. It does not announce itself; it returns a wrong
    /// answer.
    ///
    /// `None` on every node the builder makes and on every clone a pass makes
    /// without opting in, which is every compile that does not unroll — the
    /// by-bci scan is then the only path and its behaviour is unchanged. A
    /// producer that clones a region sets this on each copy's nodes through
    /// [`Graph::set_node_frame_snapshot`]; see `ir_optimize::unroll`.
    ///
    /// # It is a program point, not a value
    ///
    /// Two nodes that compute the same value at different program points have
    /// different snapshots, so this participates in GVN's identity: merging
    /// across it would hand one copy's code another copy's frame. See
    /// `ir_optimize::gvn`.
    pub frame_snapshot: Option<u32>,
    /// This `Op::Load` / `Op::Store` accesses a field declared `volatile`
    /// (round 9 wave 6, `perf-ir-refuses-volatile-instance-field-methods`).
    ///
    /// A flag on the node rather than a new `Op` variant or a new `MemKind`:
    /// every lowering and every pass that matches `Op::Load(kind)` stays
    /// correct for the ACCESS itself (a volatile read is one aligned `MOV` on
    /// x86-64, exactly like a plain one). What differs is ORDERING, and that
    /// is answered by [`effect_of_node`] ([`MemEffect::volatile_read`] /
    /// [`MemEffect::volatile_write`]) and by the passes that move, merge or
    /// delete memory operations, each of which asks
    /// [`Node::is_volatile_access`]. `ir_lower` owes the StoreLoad fence after
    /// a volatile store ([`Node::is_volatile_store`]); see
    /// [`IR_LOWER_FENCES_VOLATILE_STORES`].
    ///
    /// `false` on every node [`Graph::add`] makes; only the builder's
    /// `getfield` / `putfield` arms set it, through
    /// [`Graph::set_volatile_access`]. A pass that CLONES a node must carry it
    /// (`ir_optimize::unroll` does); a pass that rewrites `op` in place keeps
    /// it, which is harmless because [`Node::is_volatile_access`] also checks
    /// the op.
    pub volatile_access: bool,
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

    /// Is this a live `Op::Load` / `Op::Store` of a VOLATILE field? See
    /// [`Node::volatile_access`]. Every pass that may move, merge or delete a
    /// memory operation must ask this before treating the node as a plain
    /// access.
    #[inline]
    pub fn is_volatile_access(&self) -> bool {
        self.volatile_access && matches!(self.op, Op::Load(_) | Op::Store(_))
    }

    /// Is this a VOLATILE `Op::Store` — the node after which `ir_lower` owes
    /// the JMM's StoreLoad fence (`MFENCE`)?
    #[inline]
    pub fn is_volatile_store(&self) -> bool {
        self.volatile_access && matches!(self.op, Op::Store(_))
    }
}

/// Does `ir_lower` emit the StoreLoad fence (`MFENCE`) after a node for which
/// [`Node::is_volatile_store`] holds?
///
/// The two-sided contract of round 9 wave 6
/// (`perf-ir-refuses-volatile-instance-field-methods-FIXED-20260918.md`): the IR
/// side (this file, `ir_optimize`) models volatile ordering; the fence is
/// emitted by `ir_lower`, which is another lane's file. Before that edit
/// landed this was `false`, and the builder kept REFUSING volatile `putfield`
/// whatever `CRATONVM_JIT_IR_VOLATILE_FIELDS` says, because an unfenced
/// volatile store breaks Dekker-style handshakes (`r9w5_volatile5_fields`'s
/// `a_dekker_handshake_on_compiled_volatile_stores_never_reads_both_zero`).
/// Volatile `getfield` needs nothing from the lowerer (x86-64 loads are
/// acquires in hardware; the compiler-side ordering is the IR's), so reads are
/// admitted under the flag alone.
///
/// The fence landed in round 9 wave 6 (cross-lane request R1 in
/// `docs/internal/jit-review-r9/NOTES-w6-irsnap6.md`): `ir_lower` emits
/// `MFENCE` after every `is_volatile_store()` node in its block loop, so this
/// is `true`. Setting it back to `false` re-refuses volatile `putfield` in the
/// builder without touching anything else.
pub const IR_LOWER_FENCES_VOLATILE_STORES: bool = true;

/// `CRATONVM_JIT_IR_VOLATILE_FIELDS=1` — let the optimizing tier build
/// `getfield` / `putfield` of a VOLATILE instance field as an ordered
/// `Op::Load` / `Op::Store` ([`Node::volatile_access`]) instead of refusing the
/// method. **Default ON** (round 9 wave 7); `CRATONVM_JIT_IR_VOLATILE_FIELDS=0`
/// restores the refusal (and with it the single-pass route). Stores
/// additionally need [`IR_LOWER_FENCES_VOLATILE_STORES`].
///
/// Round 9 wave 7 (`irsnap7`) measured it ON against the w6 binary — spin loop
/// terminates, 0 forbidden outcomes in a 200k-round Dekker litmus with the
/// handshake bodies published as IR bodies, checksum parity everywhere — and
/// recommended default-on. The flip (cross-lane request V1 in
/// `docs/internal/jit-review-r9/NOTES-w7-irsnap7.md`: flag row, this reader,
/// and the `lib.rs` routing test, together) landed in wave 7b (`wire7b`).
pub fn ir_volatile_fields_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_VOLATILE_FIELDS")
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
    /// The builder's abstract MONITOR stack at this bci, outermost first: one
    /// entry per `monitorenter` that has executed and whose `monitorexit` has
    /// not, naming the locked reference. A re-entrant lock on one object is two
    /// entries; `ir_lower` coalesces them into one `deopt::MonitorInfo` whose
    /// `lock_depth` is the count.
    ///
    /// A slot here is a node reference exactly like a local: it follows a value
    /// through [`Graph::replace_all_uses`] ([`SafepointSlotKind::Monitor`]), it
    /// is a DCE root, and it pins its value's home. Whether the lock was ELIDED
    /// is not recorded here — escape analysis decides that after the build — and
    /// the lowerer recovers it from whether any live monitor op still names the
    /// object.
    ///
    /// Before 2026-09-12 the builder kept no monitor stack, so every lowered
    /// `FrameState` claimed "holds no lock" by construction
    /// (`ir-frame-states-carry-no-monitor-stack-FIXED-20260912.md`).
    pub monitors: Vec<NodeId>,
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

// ── Inlined scopes (deopt caller chains) ─────────────────────────────
//
// A [`SafepointSnapshot`] describes ONE interpreter frame: the locals and
// operand stack of the method whose bci it names. That is the whole story
// while the method being compiled is the only method in the artifact — and it
// is a silent lie the moment a callee is inlined into it, because the deopt
// then has to rebuild *two* interpreter frames (the callee's, and the caller's
// parked mid-`invoke`). `deopt::FrameState::caller` is the field that models
// it, and `ir_lower` hard-coded it `None` because nothing upstream recorded
// which inlined scope a snapshot belonged to.
//
// [`InlineScopeTable`] is that missing upstream record. It is deliberately a
// **side table** rather than a field on [`SafepointSnapshot`] or [`Graph`]:
// both of those are constructed by struct literal in files this crate spreads
// across (`ir_optimize.rs`, `ir_verify.rs`, `ir_schedule.rs`, `lib.rs`,
// `escape_analysis.rs`), and a new field breaks every one of them. A table
// threaded alongside the graph costs one parameter at the lowering entry point
// and nothing at all to the ~30 hand-built graphs in the test suites.
//
// The table is EMPTY on every compile today: `IrBuilder::build` does not
// inline (the single-pass backend does, via `lib.rs`'s `inline_sites`), so
// nothing registers a scope and `ir_lower` produces exactly the flat, caller-
// less frame states it always did.
//
// STILL empty after `IrBuilder` learned to splice ([`IrInlineSite`]) — and
// deliberately so, not by omission. A spliced region carries the CALLER's
// `invoke` bci with re-execute semantics rather than a scope chain, because
// re-execution needs no frame identity while a chain needs the innermost
// frame's, which `ir_lower::resolve_frame_state` leaves for the VM to fill from
// the running `CompiledMethod` (i.e. it would name the caller). The reasoning
// is written out at [`IrInlineSite`]; this is the note for a reader who arrives
// here first.

// ── IR-tier inlining (bytecode splicing) ─────────────────────────────
//
// `IrBuilder::build` walks ONE method's bytecode. An accessor that returns a
// freshly-allocated wrapper therefore always shows escape analysis an object
// that leaves through `areturn`, and `scalar-replaced 0/N` is the only answer
// per-method EA can give however good the tier is. HotSpot deletes the same
// object by inlining the accessor into its consuming loop first and running EA
// on the merged graph; this is that missing first half.
//
// The splice is done in the BYTECODE domain, not by re-entering the builder:
// `lib.rs` hands `build` a combined buffer — the caller's code, then each
// admitted callee's body appended after it — plus, for every callee pc, the
// same pc-keyed side-table rows (`field_info`, `invoke_info`, `new_info`, …)
// the builder already consumes. `build` walks the caller as before and, at an
// admitted `invoke`, jumps `pc` into the callee's region with the callee's
// locals installed, returning to `pc + instr_len` at the callee's return. The
// giant opcode `match` is reused verbatim, which is the point: a second walker
// would be a second model of every opcode, and a duplicated model is what this
// tree keeps paying for elsewhere.
//
// Relocation is free because JVM branch offsets are relative to the branching
// instruction — a whole body moves without rewriting. `tableswitch` /
// `lookupswitch` are the exception (their padding is measured from method
// start), and the resolver refuses them.
//
// ## Two bcis, and why a node keeps the COMBINED one
//
// A node's `bytecode_pc` is read for two unrelated purposes, and a spliced
// node needs a different answer for each:
//
//  * as a **site key** — `ir_lower` looks up the compact field offset, the
//    direct-call entry and the MIC/PIC pair by it. Here the answer must be
//    unique per site, so a spliced node keeps its COMBINED-buffer pc and
//    `lib.rs` rebases the callee's rows to match. Giving every node in a
//    region the caller's `invoke` pc instead — which an earlier draft did —
//    makes the whole region share the metadata of the call the splice
//    REPLACED. That is a wrong-code hazard (an inline-cache hit on a matching
//    receiver class jumps to the wrong method), and it is also a silent
//    ~200x slowdown: with no compact-offset row every spliced field read falls
//    back to the checked `jit_getfield` helper, and with no cache every
//    surviving call falls back to a blind name resolution. Measured on the
//    `VoxelAlloc2` probe: `ShortArray.get` spliced into its caller took the
//    control arm from 47 ns to 11 000 ns per element.
//
//  * as an **interpreter program point** — the throw-site bci the exception
//    table's `[start_pc, end_pc)` check uses, and the bci a null/bounds guard's
//    deopt point resumes at. Here a combined-buffer pc names no instruction in
//    this method at all, so `ir_lower` translates it back to the enclosing
//    `invoke` through `spliced_ranges` (see `Lowerer::resume_bci`). "The
//    `invoke` at this bci has not taken effect; re-execute it" is true of the
//    whole region and needs no frame identity — which is why the region gets
//    that rather than a real inlined-scope chain, whose innermost frame would
//    need its own method key (`ir_lower::resolve_frame_state` leaves that for
//    the VM to fill from the running `CompiledMethod`, i.e. it would name the
//    caller).
//
// No safepoint snapshot is pushed inside a splice, so a combined-buffer pc
// never reaches `build_deopt_points`.
//
// The one thing re-execution cannot survive is a side effect the spliced body
// has ALREADY committed, so the resolver admits only bodies that cannot commit
// one before a trap — no `athrow`, no `monitorenter`, no `putstatic`, and no
// division (`Op::Guard` is emitted for div-zero and nothing else, so with
// division refused a spliced region carries no guard of its own at all) — and
// the builder proves every store targets an object the splice itself allocated
// ([`IrBuilder::splice_store_is_local`]).

/// Hard cap on how many splices may enclose one another.
///
/// The accessor chains this exists for are 3-4 deep (`get` → `loadFromArray` →
/// `<init>()` → `<init>([S)`). Eight leaves room for one deeper shape without
/// letting a resolver defect turn into an unbounded bytecode expansion — the
/// combined buffer `lib.rs` builds grows by the callee's whole body per level.
pub const MAX_IR_SPLICE_DEPTH: usize = 8;

/// One callee body relocated into the combined bytecode buffer `build` walks.
///
/// Keyed by the CALLER pc of the `invoke` it replaces. Everything here is
/// resolved by `lib.rs` before the build starts; the builder does no descriptor
/// parsing and no class lookup of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IrInlineSite {
    /// Offset of the callee's first bytecode in the combined buffer.
    pub base: usize,
    /// Length of the callee's bytecode. `base + code_len` is one past its last
    /// instruction, and the walk bails if it ever reaches there without having
    /// seen a return.
    pub code_len: usize,
    /// Values the call consumes from the caller's operand stack, receiver
    /// included. Counted the way this builder counts them — one slot per value,
    /// a `long`/`double` included (see `crate::static_call_shape`).
    pub num_args: usize,
    /// The callee's `max_locals`, i.e. how many local slots to install.
    pub max_locals: usize,
    /// JVM local index each argument lands in, in source order. A `long` or
    /// `double` argument advances the next index by two even though it occupies
    /// one `NodeId` slot here, so this cannot be derived from `num_args`.
    pub arg_local_slots: Vec<u32>,
    /// Whether the callee returns a value to push on the caller's stack.
    pub returns_value: bool,
    /// Is argument 0 a RECEIVER — i.e. is the callee an instance method?
    ///
    /// JVMS §6.5 makes a null `objectref` an NPE AT THE INVOKE, before the
    /// callee's first instruction. Splicing deletes that invoke, so unless the
    /// builder puts a check back a null receiver simply flows into callee local
    /// 0 and the body runs with `this == null` — measured on
    /// `NullReceiverCachedProbe`: `warm-invokespecial=NO-THROW(3)`, the private
    /// method's body returning its value for a receiver that was null. See
    /// [`IrBuilder::begin_splice`], which is where the guard goes.
    ///
    /// Not derivable from `num_args` or `arg_local_slots`: a static callee's
    /// argument 0 also lands in local 0.
    pub receiver_is_arg0: bool,
    /// The callee's `"class/Name.method:descriptor"` label — the same shape
    /// [`crate::x64::InlineFrameLevel::label`] carries and the same one
    /// `CompiledMethod::method_label` uses, so a consumer parses it with the
    /// splitter `stackwalker::compiled_frame_entry` already has.
    ///
    /// Carried for ONE reason: a spliced body's frames are invisible to a stack
    /// trace without it. Everything else on this struct describes how to RUN
    /// the callee's bytecode; this pair describes whose bytecode it is, which
    /// is what a trace needs and what nothing here recorded until 2026-09-08.
    /// See [`IrInlineFrameSites`].
    pub method_key: String,
    /// `ClassId` of the class [`Self::method_key`] names, or `0` when the
    /// resolver supplied none. Same contract as
    /// [`crate::x64::InlineFrameLevel::class_id`]: a consumer that must answer
    /// in `ClassId` expands the level without resolving a JIT label by name.
    pub class_id: u32,
}

/// One level of an inline chain, in the arch-neutral form this module can
/// speak. `ir_lower` converts these to [`crate::x64::InlineFrameLevel`] at the
/// point it records a row; the split keeps `ir.rs` free of an x64 dependency
/// while both sides agree on the field meanings exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IrInlineFrameLevel {
    /// `"class/Name.method:descriptor"` of the spliced callee.
    pub method_key: String,
    /// Bytecode index inside that callee's OWN code — never a combined-buffer
    /// pc, which is the whole point of the translation below.
    pub bci: u32,
    /// `ClassId` of the class `method_key` names, or `0`.
    pub class_id: u32,
}

/// Resolves a COMBINED-BUFFER pc to the chain of spliced callees enclosing it,
/// innermost first.
///
/// # Why this exists, and why the combined pc is the right key
///
/// The IR tier splices in the bytecode domain: `lib.rs` appends each admitted
/// callee's body after the caller's code and the builder walks the result, so
/// a program point inside a spliced body carries a pc PAST the caller's own
/// `code_len` (see [`IrInlineSite`]). Every node the builder creates there is
/// stamped with that combined pc, and it survives untouched into `ir_lower`.
///
/// That makes the combined pc an EXACT key, and exactness is what this needed.
/// The obvious alternative — the bci a frame reports — cannot work: a spliced
/// region is covered by the caller's snapshot at the `invoke` pc, so every
/// level of a nested splice reports the SAME bci, and
/// `x64::InlineFrameMap::from_rows` (correctly) poisons rows that disagree
/// under one bci. Combined-pc ranges, by contrast, are disjoint by
/// construction: `lib.rs` appends each body once, at its own `base`, so a pc
/// names exactly one body and the nesting is recovered by following each
/// site's CALLER pc outward.
///
/// # What it deliberately does not do
///
/// It answers `None` — not a guess — for a pc in no spliced range (the
/// compiling method's own code, which is the physical frame and not an inlined
/// level), for a malformed site table, and for a chain that would exceed
/// [`MAX_INLINE_SCOPE_DEPTH`]. A missing chain costs a trace some frames; a
/// wrong one names a method that never ran, which is the outcome this whole
/// area refuses.
#[derive(Clone, Debug, Default)]
pub struct IrInlineFrameSites {
    /// `(base, code_len, caller_pc, method_key, class_id)` per spliced body,
    /// ascending by `base`. Ranges are disjoint, so a binary search on `base`
    /// finds the only candidate.
    bodies: Vec<(usize, usize, usize, String, u32)>,
}

impl IrInlineFrameSites {
    /// Build from the builder's site table. `sites` is keyed by CALLER pc, in
    /// combined coordinates, exactly as [`IrInlineTables::sites`] holds it.
    pub fn from_sites(sites: &HashMap<usize, IrInlineSite>) -> Self {
        let mut bodies: Vec<(usize, usize, usize, String, u32)> = sites
            .iter()
            .map(|(&caller_pc, s)| {
                (
                    s.base,
                    s.code_len,
                    caller_pc,
                    s.method_key.clone(),
                    s.class_id,
                )
            })
            .collect();
        bodies.sort_unstable_by_key(|&(base, _, _, _, _)| base);
        Self { bodies }
    }

    /// `true` when nothing was spliced — the state of every compile that
    /// inlines nothing, and the fast path every caller takes first.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bodies.is_empty()
    }

    /// Index of the body whose range contains `pc`, or `None`.
    fn body_at(&self, pc: usize) -> Option<usize> {
        // The last body whose `base <= pc`; ranges are disjoint and ascending,
        // so no earlier one can contain `pc`.
        let i = match self.bodies.binary_search_by_key(&pc, |&(b, _, _, _, _)| b) {
            Ok(i) => i,
            Err(0) => return None,
            Err(i) => i - 1,
        };
        let (base, code_len, ..) = self.bodies[i];
        // `base + code_len` cannot overflow: both came from a buffer this
        // process built, but saturate rather than assume it.
        if pc < base.saturating_add(code_len) {
            Some(i)
        } else {
            None
        }
    }

    /// The chain of spliced callees enclosing `pc`, innermost first, or an
    /// empty vector when `pc` is not inside any spliced body.
    ///
    /// Each level's `bci` is an index into THAT level's own code: for the
    /// innermost it is `pc - base`, and for every level above it the offset of
    /// the nested call inside its parent's body.
    pub fn chain_at(&self, pc: usize) -> Vec<IrInlineFrameLevel> {
        let mut out = Vec::new();
        let mut at = pc;
        while let Some(i) = self.body_at(at) {
            let (base, _, caller_pc, ref key, class_id) = self.bodies[i];
            // Cast: a bci inside a body this compile appended, so it is bounded
            // by the combined buffer's length.
            let Ok(bci) = u32::try_from(at - base) else {
                return Vec::new();
            };
            out.push(IrInlineFrameLevel {
                method_key: key.clone(),
                bci,
                class_id,
            });
            if out.len() > MAX_INLINE_SCOPE_DEPTH {
                // A malformed table (a body whose caller pc lands back inside
                // itself) would otherwise walk forever. Give up the chain
                // whole rather than return a truncated one that reads as
                // complete.
                return Vec::new();
            }
            // Step out: the call that produced this level lives at the site's
            // CALLER pc, which is either inside the enclosing spliced body or
            // in the compiling method's own code — where the walk ends.
            at = caller_pc;
        }
        out
    }

    /// The COMPILING method's own bci covering `pc`: the caller pc of the
    /// outermost splice enclosing it, which is by construction an index into
    /// that method's real bytecode.
    ///
    /// `None` when `pc` is in no spliced body — the caller already has the
    /// method's own bci in that case and must not take one from here.
    pub fn enclosing_bci_at(&self, pc: usize) -> Option<u32> {
        let mut at = pc;
        let mut guard = 0usize;
        let mut found = None;
        while let Some(i) = self.body_at(at) {
            at = self.bodies[i].2;
            found = Some(at);
            guard += 1;
            if guard > MAX_INLINE_SCOPE_DEPTH {
                return None;
            }
        }
        found.and_then(|b| u32::try_from(b).ok())
    }
}

/// The pc-keyed rows a set of [`IrInlineSite`]s adds to the builder's existing
/// side tables, in COMBINED-buffer coordinates.
///
/// Splicing a body means the builder meets that body's `getfield`s, `invoke`s
/// and `new`s at pcs the caller's own resolution never visited, so every table
/// those arms consult needs the callee's rows too. They arrive as one bundle
/// rather than eight more setters because they are produced together, from one
/// resolution pass, and applying half of them would leave the walk bailing in
/// the middle of a body it had already committed to.
///
/// What is NOT here is what the resolver still refuses in a spliced body:
/// `putstatic` and `anewarray`. A callee using either is refused whole at
/// resolution. `ldc` / `ldc2_w`, `getstatic` and `checkcast` / `instanceof`
/// were all on that list and came off it, in every case because the refusal
/// was PLUMBING rather than modelling -- the builder had the arm and nothing
/// rebased the rows it reads. `putstatic` is the one that is genuinely
/// modelling: the builder has no arm for it at all, and a static reference
/// write owes an SATB pre-barrier the single-pass `jit_putstatic_*` path
/// carries.
#[derive(Clone, Debug, Default)]
pub struct IrInlineTables {
    /// The bodies themselves, keyed by CALLER pc.
    pub sites: HashMap<usize, IrInlineSite>,
    /// `pc → (field_index, type_tag)`, merged into the builder's `field_info`.
    pub field_info: HashMap<usize, (usize, u8)>,
    /// `pc → (info_ptr, num_args, ret_type)` for the calls a spliced body makes
    /// that are NOT themselves spliced.
    pub invoke_info: HashMap<usize, (usize, usize, u8)>,
    /// `pc → (class_id, num_fields)` for the allocations a spliced body makes.
    /// This is the row whose absence made `loadFromArray` bail the whole IR
    /// tier — see `docs/known-issues/jit/per-voxel-allocation-*`.
    pub new_info: HashMap<usize, (u32, usize)>,
    /// `invokespecial` pcs the resolver PROVED are no-ops (`Object.<init>()V`,
    /// or a super constructor whose body is exactly one). Merged into
    /// `object_init_pcs` — the set that elides on any receiver — because that
    /// is what "proven no-op body" means, receiver notwithstanding.
    pub object_init_pcs: HashSet<usize>,
    /// `pc → (bits, is_float)` for the `ldc` / `ldc_w` sites inside spliced
    /// bodies, rebased into combined-buffer coordinates and merged into the
    /// builder's own `ldc_info`.
    ///
    /// Without this row a spliced `ldc` reaches the builder's `0x12 | 0x13` arm
    /// with nothing in any of its three tables and bails the whole METHOD —
    /// which is why the splice scanner used to refuse such a callee up front
    /// rather than discover it here. See [`crate::InlineSite::ldc_fp_pcs`].
    pub ldc_info: HashMap<usize, (i64, bool)>,
    /// `pc → (bits, is_double)` for the `ldc2_w` sites inside spliced bodies.
    /// Same shape and same reason as [`Self::ldc_info`].
    pub ldc2w_info: HashMap<usize, (i64, bool)>,
    /// `pc -> (class_id, field_index, type_tag, is_volatile)` for the
    /// `getstatic` sites inside spliced bodies, rebased into combined-buffer
    /// coordinates and merged into the builder's own `static_field_info`.
    ///
    /// Same shape and same reason as [`Self::ldc_info`], and the same class of
    /// cause: the rows were resolvable all along -- `InlineSite` has carried
    /// `static_field_info` for the single-pass inliner since it existed -- and
    /// the optimizing tier refused every callee containing a `getstatic`
    /// (`ir-splice-static-field`) because nothing rebased them. `getstatic` is
    /// the single largest opcode in the ir-coverage survey (92 of 273 events),
    /// so the refusal fell on the callee shape framework code is mostly made
    /// of: a static-table read behind an accessor.
    ///
    /// `putstatic` (0xb3) stays refused, and not for a plumbing reason: the
    /// builder has no arm for it at all, and a static reference WRITE owes an
    /// SATB pre-barrier that lives on the single-pass `jit_putstatic_*` path.
    /// The resolver refuses it separately so the two sides cannot disagree.
    pub static_field_info: HashMap<usize, (u32, usize, u8, bool)>,
    /// `pc -> (name_ptr, name_len)` for the `checkcast` (0xc0) sites inside
    /// spliced bodies, rebased and merged into the builder's own
    /// `checkcast_info`. [`Self::instanceof_info`] is the 0xc1 twin.
    ///
    /// Same rebase and the same fail-closed shape as
    /// [`Self::static_field_info`]: a missing row bails the METHOD, so the
    /// resolver refuses a CALLEE whose targets it could not resolve rather
    /// than admitting the body and leaving rows out.
    ///
    /// The `0xc0`/`0xc1` arms read as though a missing row were graceful --
    /// they call `plant_uncommon_trap` and compile the rest. By default they
    /// are not: `TrapCause::UnresolvedTypeCheck` is gated behind
    /// `ir_unresolved_class_trap_enabled`, which is off, so the plant refuses
    /// and the arm bails. With that switch ON the trap does fire, and inside a
    /// splice it deopts to "re-execute the invoke" -- which is the second
    /// reason the resolver refuses rather than relying on the arm: one setting
    /// of that flag costs the caller its compile, the other costs it a trap on
    /// every call.
    ///
    /// The `(ptr, len)` pairs are interned process-wide by
    /// [`crate::intern_typecheck_target`] and are NOT owned by the compile, so
    /// unlike the invoke tables these rows need no keep-alive on the artifact.
    pub checkcast_info: HashMap<usize, (usize, usize)>,
    /// `pc -> (name_ptr, name_len)` for the `instanceof` (0xc1) sites inside
    /// spliced bodies. See [`Self::checkcast_info`].
    ///
    /// Kept as a separate map for the same reason the builder keeps two: the
    /// two opcodes take different lowerings, and a `checkcast` additionally
    /// obliges the artifact to carry `has_dispatch` — its ClassCastException
    /// path publishes through the `JIT_THREAD` TLS that the no-dispatch fast
    /// entry never sets. `instanceof` answers a boolean and owes nothing.
    pub instanceof_info: HashMap<usize, (usize, usize)>,
}

/// The builder's state for one splice in progress.
struct SpliceFrame {
    /// Caller pc to resume at — the instruction after the spliced `invoke`.
    return_pc: usize,
    /// One past the callee's last bytecode, in combined-buffer coordinates.
    end: usize,
    saved_locals: Vec<NodeId>,
    saved_stack: Vec<NodeId>,
    returns_value: bool,
    /// The callee's JVMS §6.5 `ireturn` narrowing
    /// ([`crate::narrowed_int_return_tag`] of its `method_key`), applied to the
    /// value each of its `ireturn`s hands back. `None` for `I` and for a
    /// fixture with no key.
    return_narrow: Option<u8>,
    /// This body has more than one reachable `return`, so a `return` is NOT a
    /// splice exit: it is an edge into the continuation built at [`Self::end`].
    /// Decided once, by the pre-scan in [`IrBuilder::build`], from the same
    /// verified decode the merge targets come from — never from the walk,
    /// which meets the returns one at a time and cannot know it is at the last.
    multi_return: bool,
    /// One `(ctrl, mem, value)` per `return` the walk has reached, in walk
    /// order. `value` is [`NO_NODE`] for a `void` callee.
    ///
    /// The caller's locals and operand stack need no entry here: they are
    /// saved above and are the same on every path out of the callee, because
    /// the callee cannot reach them.
    exits: Vec<(NodeId, NodeId, NodeId)>,
    /// Combined-buffer pc of the last `return` recorded in [`Self::exits`].
    ///
    /// The continuation's `Merge` and `Phi`s are stamped with this rather than
    /// with `end`: `end` is one PAST the body, so `spliced_ranges` would
    /// resolve it to the caller and `resume_bci` would stop rewriting these
    /// nodes to the enclosing `invoke`. See [`IrInlineSite`] on the two
    /// meanings of `bytecode_pc`.
    exit_bci: usize,
    /// `[start, end)` ranges, in combined-buffer pcs, in which the walk visits
    /// a body with a rotated loop (see [`BlockWalk`]). Empty for a body walked
    /// in pc order. A walked body's `return`s are exit edges, as for
    /// [`Self::multi_return`], because reverse post-order need not visit the
    /// trailing `return` last; the body closes when its ranges run out.
    ranges: Vec<(usize, usize)>,
    /// Index into [`Self::ranges`] of the next range to visit.
    next_range: usize,
    /// End of the range being walked. Unused when `ranges` is empty.
    range_end: usize,
}

/// Hard cap on inline-scope chain length.
///
/// Chains are acyclic by construction — [`InlineScopeTable::push_scope`] only
/// accepts a parent that was pushed *before* the child, so parent ids are
/// strictly smaller — but every walk is bounded anyway: a metadata defect must
/// not turn into an unbounded loop on a deopt path. HotSpot's own inline depth
/// limit is 9 (`MaxInlineLevel` + `MaxRecursiveInlineLevel`), so 64 is far
/// above anything a real inliner produces.
pub const MAX_INLINE_SCOPE_DEPTH: usize = 64;

/// Handle to one scope in an [`InlineScopeTable`].
///
/// Only meaningful in the table that issued it. Handles are dense and
/// monotonically increasing, and a scope's `parent` is always strictly smaller
/// than the scope itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InlineScopeId(u32);

impl InlineScopeId {
    /// Raw index, for diagnostics and stable ordering only.
    #[inline]
    pub fn index(self) -> u32 {
        self.0
    }
}

/// One **caller** frame of an inlined call chain.
///
/// Read it as "the frame that is parked mid-`invoke` while the scope below it
/// runs": `method_key` is the *caller's* `"<class>.<method>:<descriptor>"`,
/// `caller_bci` is the bci of the `invoke` inside it, and `parent` is that
/// caller's own caller (`None` at the outermost, i.e. the method actually being
/// compiled). A [`SafepointSnapshot`] bound to scope `s` is the innermost
/// frame; `s`, `s.parent`, … are the frames stacked above it, innermost-first.
///
/// `caller_snapshot` is what makes the caller frame *describable*. A caller
/// frame is not empty — its locals and operand stack are live and the resume
/// has to rebuild them — so the scope names the [`SafepointSnapshot`] (by index
/// into `Graph::safepoints`) that records the caller's state at `caller_bci`.
/// `None` means "this scope exists but nobody recorded the caller's values",
/// which `ir_lower` renders as an explicitly **unresumable** frame rather than
/// as an empty one: an empty caller frame reconstructs as a method whose
/// locals are all `0`, which is the same class of silent wrong-reconstruction
/// [`crate::deopt::FrameValue::MaterializationRequired`] exists to prevent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineScope {
    /// The caller's fully-qualified method key.
    pub method_key: String,
    /// Bci of the `invoke` in the caller. The call is already in progress, so
    /// this bci must NOT be re-executed on resume — see
    /// [`crate::deopt::ResumeSemantics::for_caller_scope`].
    pub caller_bci: u32,
    /// Index into `Graph::safepoints` of the snapshot describing the caller's
    /// locals/stack at `caller_bci`, or `None` when it was not recorded.
    pub caller_snapshot: Option<u32>,
    /// The caller's own caller, or `None` at the outermost frame.
    pub parent: Option<InlineScopeId>,
}

/// Per-graph table of inlined caller scopes, plus the snapshot → scope binding.
///
/// Two maps in one, because they have the same lifetime and the same producer:
///
/// * the scope arena — `(method_key, caller_bci, caller_snapshot, parent)`
///   tuples, parent-linked so a depth-`n` chain costs `n` entries in total
///   rather than `n` per safepoint;
/// * `by_snapshot` — which scope (if any) each `Graph::safepoints` entry
///   belongs to, keyed by the snapshot's **index**.
///
/// Keying by index is sound because nothing in the pipeline removes or reorders
/// `Graph::safepoints`: `IrBuilder` pushes, `ir_optimize` and `lib.rs` rewrite
/// slots in place, and the lowerer only reads. An index past the end simply
/// reads back `None`, so a table built against a shorter graph degrades to "no
/// inlining info" rather than to a wrong scope.
#[derive(Clone, Debug, Default)]
pub struct InlineScopeTable {
    scopes: Vec<InlineScope>,
    by_snapshot: Vec<Option<InlineScopeId>>,
}

impl InlineScopeTable {
    /// An empty table — the state of every compile that inlines nothing.
    pub fn new() -> Self {
        Self {
            scopes: Vec::new(),
            by_snapshot: Vec::new(),
        }
    }

    /// `true` when no scope has been registered. The lowerer's fast path: an
    /// empty table produces exactly the caller-less frame states it always did.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.scopes.is_empty()
    }

    /// Number of distinct scopes held.
    #[inline]
    pub fn len(&self) -> usize {
        self.scopes.len()
    }

    /// Register a caller scope, returning its handle.
    ///
    /// `None` — the scope is **refused**, not silently accepted — when:
    ///
    /// * `parent` names a scope this table never issued. A forward or foreign
    ///   parent is the one way a chain could become cyclic, so it is rejected
    ///   at the door rather than defended against at every walk.
    /// * the resulting chain would exceed [`MAX_INLINE_SCOPE_DEPTH`].
    /// * the arena is full (`u32::MAX` scopes).
    ///
    /// A refused scope means the producer must leave the snapshot unbound, and
    /// the deopt point stays flat — the same conservative outcome as not
    /// inlining at all.
    pub fn push_scope(
        &mut self,
        method_key: &str,
        caller_bci: u32,
        caller_snapshot: Option<u32>,
        parent: Option<InlineScopeId>,
    ) -> Option<InlineScopeId> {
        if let Some(p) = parent {
            // Strictly smaller than the id about to be issued: acyclicity.
            if p.0 as usize >= self.scopes.len() {
                return None;
            }
            if self.depth(p) >= MAX_INLINE_SCOPE_DEPTH {
                return None;
            }
        }
        let id = u32::try_from(self.scopes.len()).ok()?;
        if id == u32::MAX {
            return None;
        }
        self.scopes.push(InlineScope {
            method_key: method_key.to_string(),
            caller_bci,
            caller_snapshot,
            parent,
        });
        Some(InlineScopeId(id))
    }

    /// The scope `id` names, or `None` for a handle this table never issued.
    #[inline]
    pub fn scope(&self, id: InlineScopeId) -> Option<&InlineScope> {
        self.scopes.get(id.0 as usize)
    }

    /// The caller-of-the-caller, or `None` at the outermost scope (and for an
    /// unknown handle).
    #[inline]
    pub fn parent(&self, id: InlineScopeId) -> Option<InlineScopeId> {
        self.scope(id).and_then(|s| s.parent)
    }

    /// How many scopes `id`'s chain holds, `id` included. `0` for an unknown
    /// handle; capped at [`MAX_INLINE_SCOPE_DEPTH`].
    pub fn depth(&self, id: InlineScopeId) -> usize {
        let mut n = 0;
        let mut cursor = Some(id);
        while let Some(handle) = cursor {
            if self.scope(handle).is_none() {
                break;
            }
            n += 1;
            if n >= MAX_INLINE_SCOPE_DEPTH {
                break;
            }
            cursor = self.parent(handle);
        }
        n
    }

    /// `id`'s whole chain, innermost (i.e. `id` itself) first. Empty for an
    /// unknown handle; capped at [`MAX_INLINE_SCOPE_DEPTH`].
    pub fn chain(&self, id: InlineScopeId) -> Vec<InlineScopeId> {
        let mut out = Vec::new();
        let mut cursor = Some(id);
        while let Some(handle) = cursor {
            if self.scope(handle).is_none() {
                break;
            }
            out.push(handle);
            if out.len() >= MAX_INLINE_SCOPE_DEPTH {
                break;
            }
            cursor = self.parent(handle);
        }
        out
    }

    /// Bind the snapshot at `Graph::safepoints[snapshot]` to `scope`.
    ///
    /// `false` (and nothing is recorded) for a handle this table never issued —
    /// an unbound snapshot lowers exactly as it does today, so refusing is the
    /// conservative direction.
    pub fn bind_snapshot(&mut self, snapshot: usize, scope: InlineScopeId) -> bool {
        if self.scope(scope).is_none() {
            return false;
        }
        if self.by_snapshot.len() <= snapshot {
            self.by_snapshot.resize(snapshot + 1, None);
        }
        self.by_snapshot[snapshot] = Some(scope);
        true
    }

    /// [`Self::bind_snapshot`] over a half-open range of snapshot indices — the
    /// shape a producer has after splicing an inlined callee, which appends a
    /// contiguous run of snapshots to `Graph::safepoints`.
    pub fn bind_snapshot_range(
        &mut self,
        range: std::ops::Range<usize>,
        scope: InlineScopeId,
    ) -> bool {
        if self.scope(scope).is_none() {
            return false;
        }
        for i in range {
            self.bind_snapshot(i, scope);
        }
        true
    }

    /// The scope the snapshot at `Graph::safepoints[snapshot]` belongs to, or
    /// `None` when it is not inside any inlined callee.
    #[inline]
    pub fn snapshot_scope(&self, snapshot: usize) -> Option<InlineScopeId> {
        self.by_snapshot.get(snapshot).copied().flatten()
    }
}

// ── Def-use edges ────────────────────────────────────────────────────

/// Which half of a [`SafepointSnapshot`] a slot lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SafepointSlotKind {
    /// `SafepointSnapshot::locals`
    Local,
    /// `SafepointSnapshot::stack`
    Stack,
    /// `SafepointSnapshot::monitors`
    Monitor,
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
    pub kind: SafepointSlotKind,
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
    /// [`THREAD_TOKEN`] of the thread that reconciled them. 0 = never.
    owner: u64,
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
            owner: 0,
            sp_len: 0,
            dangling: false,
            examined: 0,
            rebuilds: 0,
        }
    }

    /// Record that the lists now describe the graph, as of this thread and
    /// this edge epoch.
    #[inline]
    fn stamp(&mut self) {
        self.epoch = edge_epoch();
        self.owner = thread_token();
    }

    /// True when nothing has written an edge outside the mutators since the
    /// last [`UseLists::stamp`], on this thread.
    #[inline]
    fn stamped(&self) -> bool {
        self.owner == thread_token() && self.epoch == edge_epoch()
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
    /// Which `Op::Param` holds the receiver, when this graph is an INSTANCE
    /// method's. `None` means "static, or nobody said" — never "no receiver",
    /// because a hand-built graph leaves it `None` and must not thereby claim
    /// a fact about parameter 0.
    ///
    /// # Why the graph carries this and not the lowerer
    ///
    /// It is a property of the method, and the lowerer already holds `&Graph`.
    /// The alternative was a fifteenth positional argument to `Lowerer::new`,
    /// threaded through `lower` and `lower_inner`, to carry one bit that the
    /// graph builder already had in hand.
    ///
    /// What it licenses is one fact, stated once: `this` is non-null. The JVM
    /// enters an instance method only through a call site that has already
    /// null-checked its receiver, `<init>` included — its receiver is
    /// uninitialized but never null.
    pub receiver_param: Option<u16>,
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
            // A new node belongs to no COPY until a producer says so: the
            // by-bci scan is the whole story for everything the builder makes.
            frame_snapshot: None,
            // Plain until the builder's `getfield` / `putfield` arm says
            // otherwise (`set_volatile_access`).
            volatile_access: false,
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
                self.uses.stamp();
            }
        }
        id
    }

    /// Mark `id` as an access of a VOLATILE field ([`Node::volatile_access`]).
    ///
    /// Only an `Op::Load` / `Op::Store` can carry the mark; any other node (or
    /// an out-of-range id) is left untouched, so a caller cannot make a node
    /// "volatile" that no pass would ask about.
    pub fn set_volatile_access(&mut self, id: NodeId) {
        if let Some(n) = self.nodes.get_mut(id as usize) {
            if matches!(n.op, Op::Load(_) | Op::Store(_)) {
                n.volatile_access = true;
            }
        }
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
        // Self-replacement is a graph no-op, but it is NOT a use-list no-op on
        // the tracked path: rewriting a slot to `old` leaves it matching `old`,
        // so every visit of a multi-edge user counts the same hits again and
        // re-pushes them. A user with k edges then accrues k² entries. Nothing
        // in the graph changes, so return before either path runs.
        if old == new_id {
            return;
        }
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
                SafepointSlotKind::Local => sp.locals.get_mut(r.slot as usize),
                SafepointSlotKind::Stack => sp.stack.get_mut(r.slot as usize),
                SafepointSlotKind::Monitor => sp.monitors.get_mut(r.slot as usize),
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
        self.uses.stamp();
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
                            kind: SafepointSlotKind::Local,
                            slot: k as u32,
                        }
                    } else {
                        SafepointSlot {
                            snapshot: si as u32,
                            kind: SafepointSlotKind::Stack,
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
            for (k, v) in sp.monitors.iter_mut().enumerate() {
                if *v == old {
                    *v = new_id;
                }
                if track {
                    let def = *v;
                    if def == NO_NODE {
                        continue;
                    }
                    let slot = SafepointSlot {
                        snapshot: si as u32,
                        kind: SafepointSlotKind::Monitor,
                        slot: k as u32,
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
            self.uses.stamp();
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
            self.uses.stamp();
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
    ///
    /// Reads the maintained [`UseLists`] when they are current, and otherwise
    /// falls back to the historical scan. Both produce the same vector: an
    /// entry per referencing input *slot*, snapshot slots excluded, edges to
    /// [`NO_NODE`] or to an id past the end of the arena ignored.
    pub fn use_counts(&self) -> Vec<u32> {
        if self.use_lists_current() {
            return self.uses.users.iter().map(|u| u.len() as u32).collect();
        }
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

    // ── Incremental def-use edges ────────────────────────────────────

    /// True when the maintained [`UseLists`] describe this graph and may be
    /// consulted.
    ///
    /// Cheap enough to call on every mutation: a flag, a thread-local read and
    /// two length comparisons. `sp_len` catches snapshots pushed through the
    /// public `safepoints` field; the epoch catches edges written through the
    /// public `Node::inputs` field.
    #[inline]
    pub fn use_lists_valid(&self) -> bool {
        self.use_lists_current()
    }

    #[inline]
    fn use_lists_current(&self) -> bool {
        self.uses.tracking
            && self.uses.valid
            && self.uses.stamped()
            && self.uses.users.len() == self.nodes.len()
            && self.uses.sp_len == self.safepoints.len()
    }

    /// Turn def-use maintenance on or off. On by default.
    ///
    /// Turning it off drops the derived state and sends every mutation down the
    /// historical full-scan path — the escape hatch if a caller ever needs the
    /// pre-incremental behaviour bit for bit.
    pub fn set_use_tracking(&mut self, on: bool) {
        self.uses.tracking = on;
        if !on {
            self.uses.discard();
        }
    }

    /// Derive the def-use edges from a full scan of the graph.
    ///
    /// `O(nodes + edges + snapshot slots)`. Called automatically whenever a
    /// mutation finds the lists stale; public so a pass that is about to do
    /// many rewrites can pay for it up front. An explicit rebuild also turns
    /// tracking back on if [`Graph::set_use_tracking`] had turned it off.
    pub fn rebuild_use_lists(&mut self) {
        let num_nodes = self.nodes.len();
        self.uses.tracking = true;
        self.uses.users = vec![Inputs::new(); num_nodes];
        self.uses.sp_users = vec![Vec::new(); num_nodes];
        self.uses.dangling = false;
        {
            let (nodes, uses) = (&self.nodes, &mut self.uses);
            for (uid, node) in nodes.iter().enumerate() {
                for &inp in node.inputs.as_slice() {
                    uses.record_input(inp, uid as NodeId, num_nodes);
                }
            }
        }
        {
            let (safepoints, uses) = (&self.safepoints, &mut self.uses);
            for (si, sp) in safepoints.iter().enumerate() {
                for (k, &v) in sp.locals.iter().enumerate() {
                    uses.record_slot(
                        v,
                        SafepointSlot {
                            snapshot: si as u32,
                            kind: SafepointSlotKind::Local,
                            slot: k as u32,
                        },
                        num_nodes,
                    );
                }
                for (k, &v) in sp.stack.iter().enumerate() {
                    uses.record_slot(
                        v,
                        SafepointSlot {
                            snapshot: si as u32,
                            kind: SafepointSlotKind::Stack,
                            slot: k as u32,
                        },
                        num_nodes,
                    );
                }
                for (k, &v) in sp.monitors.iter().enumerate() {
                    uses.record_slot(
                        v,
                        SafepointSlot {
                            snapshot: si as u32,
                            kind: SafepointSlotKind::Monitor,
                            slot: k as u32,
                        },
                        num_nodes,
                    );
                }
            }
        }
        self.uses.sp_len = self.safepoints.len();
        self.uses.valid = true;
        self.uses.rebuilds += 1;
        self.uses.stamp();
    }

    /// Make the def-use edges current, deriving them if they are not.
    pub fn ensure_use_lists(&mut self) {
        if self.uses.tracking && !self.use_lists_current() {
            self.rebuild_use_lists();
        }
    }

    /// Number of input slots naming `id`, in O(1) when the lists are current.
    ///
    /// The single-node form of [`Graph::use_counts`]; same definition, same
    /// exclusions (snapshot slots do not count).
    pub fn use_count(&self, id: NodeId) -> u32 {
        if self.use_lists_current() {
            return self
                .uses
                .users
                .get(id as usize)
                .map_or(0, |u| u.len() as u32);
        }
        if id == NO_NODE {
            return 0;
        }
        let mut n = 0u32;
        for node in &self.nodes {
            for &inp in &node.inputs {
                if inp == id {
                    n += 1;
                }
            }
        }
        n
    }

    /// The distinct nodes that reference `id` as an input, in no particular
    /// order.
    pub fn users_of(&self, id: NodeId) -> Vec<NodeId> {
        let mut out: Vec<NodeId> = Vec::new();
        if self.use_lists_current() {
            if let Some(list) = self.uses.users.get(id as usize) {
                for &u in list.as_slice() {
                    if !out.contains(&u) {
                        out.push(u);
                    }
                }
            }
            return out;
        }
        if id == NO_NODE {
            return out;
        }
        for (uid, node) in self.nodes.iter().enumerate() {
            if node.inputs.as_slice().contains(&id) {
                out.push(uid as NodeId);
            }
        }
        out
    }

    /// The snapshot slots that reference `id`.
    pub fn safepoint_users_of(&self, id: NodeId) -> Vec<SafepointSlot> {
        let mut out: Vec<SafepointSlot> = Vec::new();
        if id == NO_NODE {
            return out;
        }
        if self.use_lists_current() {
            if let Some(list) = self.uses.sp_users.get(id as usize) {
                // Superset: re-check each slot's current value.
                for &r in list {
                    let live = self.safepoints.get(r.snapshot as usize).and_then(|sp| {
                        match r.kind {
                            SafepointSlotKind::Local => sp.locals.get(r.slot as usize),
                            SafepointSlotKind::Stack => sp.stack.get(r.slot as usize),
                            SafepointSlotKind::Monitor => sp.monitors.get(r.slot as usize),
                        }
                        .copied()
                    });
                    if live == Some(id) && !out.contains(&r) {
                        out.push(r);
                    }
                }
            }
            return out;
        }
        for (si, sp) in self.safepoints.iter().enumerate() {
            for (k, &v) in sp.locals.iter().enumerate() {
                if v == id {
                    out.push(SafepointSlot {
                        snapshot: si as u32,
                        kind: SafepointSlotKind::Local,
                        slot: k as u32,
                    });
                }
            }
            for (k, &v) in sp.stack.iter().enumerate() {
                if v == id {
                    out.push(SafepointSlot {
                        snapshot: si as u32,
                        kind: SafepointSlotKind::Stack,
                        slot: k as u32,
                    });
                }
            }
            for (k, &v) in sp.monitors.iter().enumerate() {
                if v == id {
                    out.push(SafepointSlot {
                        snapshot: si as u32,
                        kind: SafepointSlotKind::Monitor,
                        slot: k as u32,
                    });
                }
            }
        }
        out
    }

    // ── Tracked edge mutation ────────────────────────────────────────
    //
    // The write half of the def-use invariant. Every one of these keeps the
    // lists current; the equivalent direct write through `Node::inputs` or
    // `Graph::safepoints` is still legal but costs a rebuild.

    /// Overwrite input slot `idx` of `node`. `false` when the node or the slot
    /// does not exist.
    pub fn set_input(&mut self, node: NodeId, idx: usize, new: NodeId) -> bool {
        if !self.is_valid_id(node) || idx >= self.nodes[node as usize].inputs.len() {
            return false;
        }
        let current = self.use_lists_current();
        let num_nodes = self.nodes.len();
        let old = {
            let slots = self.nodes[node as usize].inputs.slots_mut();
            let old = slots[idx];
            slots[idx] = new;
            old
        };
        if current {
            if old != NO_NODE && (old as usize) < num_nodes {
                self.uses.users[old as usize].swap_remove_first_untracked(node);
            }
            self.uses.record_input(new, node, num_nodes);
            self.uses.stamp();
        } else {
            bump_edge_epoch();
        }
        true
    }

    /// Append an input edge to `node`. `false` when the node does not exist.
    pub fn push_input(&mut self, node: NodeId, new: NodeId) -> bool {
        if !self.is_valid_id(node) {
            return false;
        }
        let current = self.use_lists_current();
        let num_nodes = self.nodes.len();
        self.nodes[node as usize].inputs.push_untracked(new);
        if current {
            self.uses.record_input(new, node, num_nodes);
            self.uses.stamp();
        } else {
            bump_edge_epoch();
        }
        true
    }

    /// Replace the whole edge list of `node`. `false` when it does not exist.
    pub fn set_inputs(&mut self, node: NodeId, inputs: impl Into<Inputs>) -> bool {
        if !self.is_valid_id(node) {
            return false;
        }
        let current = self.use_lists_current();
        let num_nodes = self.nodes.len();
        let new_inputs = inputs.into();
        let old = std::mem::replace(&mut self.nodes[node as usize].inputs, new_inputs);
        if current {
            for &inp in old.as_slice() {
                if inp != NO_NODE && (inp as usize) < num_nodes {
                    self.uses.users[inp as usize].swap_remove_first_untracked(node);
                }
            }
            let (nodes, uses) = (&self.nodes, &mut self.uses);
            for &inp in nodes[node as usize].inputs.as_slice() {
                uses.record_input(inp, node, num_nodes);
            }
            self.uses.stamp();
        } else {
            bump_edge_epoch();
        }
        true
    }

    /// Drop every input edge of `node` (without marking it dead). `false` when
    /// it does not exist.
    pub fn clear_inputs(&mut self, node: NodeId) -> bool {
        self.set_inputs(node, Inputs::new())
    }

    /// Append a safepoint snapshot, recording the node references it carries.
    pub fn push_safepoint(&mut self, sp: SafepointSnapshot) {
        let current = self.use_lists_current();
        let si = self.safepoints.len() as u32;
        self.safepoints.push(sp);
        if current {
            let num_nodes = self.nodes.len();
            let (safepoints, uses) = (&self.safepoints, &mut self.uses);
            let snap = &safepoints[si as usize];
            for (k, &v) in snap.locals.iter().enumerate() {
                uses.record_slot(
                    v,
                    SafepointSlot {
                        snapshot: si,
                        kind: SafepointSlotKind::Local,
                        slot: k as u32,
                    },
                    num_nodes,
                );
            }
            for (k, &v) in snap.stack.iter().enumerate() {
                uses.record_slot(
                    v,
                    SafepointSlot {
                        snapshot: si,
                        kind: SafepointSlotKind::Stack,
                        slot: k as u32,
                    },
                    num_nodes,
                );
            }
            for (k, &v) in snap.monitors.iter().enumerate() {
                uses.record_slot(
                    v,
                    SafepointSlot {
                        snapshot: si,
                        kind: SafepointSlotKind::Monitor,
                        slot: k as u32,
                    },
                    num_nodes,
                );
            }
            self.uses.sp_len = self.safepoints.len();
            self.uses.stamp();
        }
    }

    /// Drop the snapshots whose `keep` entry is `false`, preserving the order of
    /// the rest. Returns how many were dropped; `0` (and no change) when `keep`
    /// is not one entry per snapshot.
    ///
    /// Every [`Node::frame_snapshot`] is renumbered to its snapshot's new index,
    /// and cleared when that snapshot was dropped — a caller that drops a
    /// snapshot a live node claims has decided nothing reads it. The def-use
    /// edges are rebuilt, since every slot position after the first dropped
    /// snapshot moved.
    ///
    /// Must run before anything records snapshot indices elsewhere (an
    /// `InlineScopeTable`); nothing in the pipeline does before lowering.
    pub fn retain_safepoints(&mut self, keep: &[bool]) -> usize {
        if keep.len() != self.safepoints.len() || keep.iter().all(|&k| k) {
            return 0;
        }
        let mut remap: Vec<Option<u32>> = Vec::with_capacity(keep.len());
        let mut next = 0u32;
        for &k in keep {
            if k {
                remap.push(Some(next));
                next += 1;
            } else {
                remap.push(None);
            }
        }
        let before = self.safepoints.len();
        let old = std::mem::take(&mut self.safepoints);
        self.safepoints = old
            .into_iter()
            .zip(keep.iter().copied())
            .filter_map(|(sp, k)| k.then_some(sp))
            .collect();
        for node in self.nodes.iter_mut() {
            if let Some(si) = node.frame_snapshot {
                node.frame_snapshot = remap.get(si as usize).copied().flatten();
            }
        }
        if self.uses.tracking {
            self.rebuild_use_lists();
        }
        before - self.safepoints.len()
    }

    /// Bind `id` to the snapshot at `graph.safepoints[snapshot]` as the frame
    /// describing ITS program point, overriding `ir_lower`'s by-bci scan.
    ///
    /// `false` when `id` is not a node of this graph, or when `snapshot` names
    /// no entry in [`Self::safepoints`] — both are producer bugs, and both are
    /// refused here rather than installed and discovered at a deopt.
    ///
    /// # Fail closed on a bci disagreement
    ///
    /// The snapshot's `bci` must equal the node's `bytecode_pc`. A node whose
    /// frame resumes at a different bytecode index than the node itself sits at
    /// is not a copy identity — it is a mis-substitution, and the interpreter
    /// would resume in the wrong instruction with a frame that looks plausible.
    /// Refused. A node carrying no `bytecode_pc` has no program point to agree
    /// with and is refused too.
    ///
    /// See [`Node::frame_snapshot`] for what this is for.
    pub fn set_node_frame_snapshot(&mut self, id: NodeId, snapshot: u32) -> bool {
        let Some(sp_bci) = self.safepoints.get(snapshot as usize).map(|s| s.bci) else {
            return false;
        };
        let Some(node) = self.nodes.get_mut(id as usize) else {
            return false;
        };
        if node.bytecode_pc != Some(sp_bci) {
            return false;
        }
        node.frame_snapshot = Some(snapshot);
        true
    }

    /// Overwrite one snapshot slot. `false` when the snapshot or slot does not
    /// exist.
    ///
    /// Use this rather than `graph.safepoints[i].locals[k] = v` whenever the
    /// new value is a real node id: a direct write that *installs* a reference
    /// is the one edit the def-use edges cannot notice.
    pub fn set_safepoint_slot(
        &mut self,
        snapshot: usize,
        kind: SafepointSlotKind,
        slot: usize,
        new: NodeId,
    ) -> bool {
        let current = self.use_lists_current();
        let num_nodes = self.nodes.len();
        let sp = match self.safepoints.get_mut(snapshot) {
            Some(sp) => sp,
            None => return false,
        };
        let cell = match kind {
            SafepointSlotKind::Local => sp.locals.get_mut(slot),
            SafepointSlotKind::Stack => sp.stack.get_mut(slot),
            SafepointSlotKind::Monitor => sp.monitors.get_mut(slot),
        };
        let cell = match cell {
            Some(c) => c,
            None => return false,
        };
        *cell = new;
        if current {
            self.uses.record_slot(
                new,
                SafepointSlot {
                    snapshot: snapshot as u32,
                    kind,
                    slot: slot as u32,
                },
                num_nodes,
            );
            self.uses.stamp();
        }
        // The stale entry left on the previous value is harmless: every rewrite
        // re-checks the slot before touching it (see `UseLists`).
        true
    }

    // ── Verification / instrumentation ───────────────────────────────

    /// Re-derive the def-use edges by a full scan and compare them with the
    /// maintained ones.
    ///
    /// `Ok(())` when they agree — or when the lists are not current, in which
    /// case they make no claim and the next mutation re-derives them anyway.
    /// Cheap to call from a test or from `ir_verify`; it allocates two
    /// scratch vectors and touches every edge once.
    ///
    /// Checked:
    /// - the derived vectors are sized to the arena;
    /// - `users[i]` is exactly the multiset of input slots naming `i`
    ///   (so `users[i].len()` is the `use_counts` entry for `i`);
    /// - `sp_users[i]` *contains* every snapshot slot naming `i`. Extra entries
    ///   are tolerated by design — see [`UseLists`] — because a slot cleared
    ///   through the public `safepoints` field cannot notify the list, and a
    ///   stale entry only ever costs a re-check.
    pub fn verify_use_lists(&self) -> Result<(), String> {
        if !self.use_lists_current() {
            return Ok(());
        }
        let num_nodes = self.nodes.len();
        if self.uses.users.len() != num_nodes || self.uses.sp_users.len() != num_nodes {
            return Err(format!(
                "use lists are sized {} / {} for a {}-node graph",
                self.uses.users.len(),
                self.uses.sp_users.len(),
                num_nodes
            ));
        }

        let mut expect_users: Vec<Vec<NodeId>> = vec![Vec::new(); num_nodes];
        for (uid, node) in self.nodes.iter().enumerate() {
            for &inp in node.inputs.as_slice() {
                if inp != NO_NODE && (inp as usize) < num_nodes {
                    expect_users[inp as usize].push(uid as NodeId);
                }
            }
        }
        for (id, expect) in expect_users.iter_mut().enumerate() {
            let mut got: Vec<NodeId> = self.uses.users[id].as_slice().to_vec();
            got.sort_unstable();
            expect.sort_unstable();
            if got != *expect {
                return Err(format!(
                    "use list for node {id} is {got:?}, the graph says {expect:?}"
                ));
            }
        }

        for (si, sp) in self.safepoints.iter().enumerate() {
            let halves = [
                (SafepointSlotKind::Local, &sp.locals),
                (SafepointSlotKind::Stack, &sp.stack),
                (SafepointSlotKind::Monitor, &sp.monitors),
            ];
            for (kind, values) in halves {
                for (k, &v) in values.iter().enumerate() {
                    if v == NO_NODE || (v as usize) >= num_nodes {
                        continue;
                    }
                    let want = SafepointSlot {
                        snapshot: si as u32,
                        kind,
                        slot: k as u32,
                    };
                    if !self.uses.sp_users[v as usize].contains(&want) {
                        return Err(format!(
                            "safepoint slot {want:?} names node {v}, but is missing \
                             from its use list"
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Input slots examined by [`Graph::replace_all_uses`] since the counter
    /// was last reset. The measurement hook — see the def-use tests.
    pub fn use_list_slots_examined(&self) -> u64 {
        self.uses.examined
    }

    /// Full rebuilds of the def-use edges since the counter was last reset.
    pub fn use_list_rebuilds(&self) -> u64 {
        self.uses.rebuilds
    }

    /// Zero the instrumentation counters.
    pub fn reset_use_list_stats(&mut self) {
        self.uses.examined = 0;
        self.uses.rebuilds = 0;
    }
}

// ── Memory-effect model ──────────────────────────────────────────────
//
// The IR's only *encoded* ordering relation between two memory operations is
// the memory-token chain: every memory-effecting node takes the previous
// writer as an input edge and hands itself on as the token for the next one.
// That chain is a total order. It is sound — the scheduler's topological sort
// cannot reorder across an input edge — and it is far stronger than the Java
// Memory Model requires: it serialises a load of `a.x` behind a store to
// `b.y`, two reads behind each other, and an `ArrayLength` behind everything.
//
// Every pass that wanted to beat that order had to re-derive, privately, what
// the chain means. `ir_optimize` grew `RefPointsTo` / `resolve_ref_points_to`
// / `loop_store_clobber` for LICM and a second, differently-shaped
// `StoreLoc` / `is_memory_barrier` pair for dead-store elimination; `lib.rs`
// grew a third copy of the token predicate for the escape-analysis bridge;
// `ir_verify` a fourth for its ordering lane. Four private notions of "may
// these two touch the same memory", none of them able to answer the question
// an optimizer actually asks.
//
// This section is that answer, in one place:
//
//   * [`AliasClass`] — *where* a node touches memory: a named field cell, an
//     array element, an array's length word, a static field, an object's
//     monitor, nothing, or anything.
//   * [`MemOrder`] — *how strongly* it is ordered: the JMM acquire/release
//     lattice, so a monitor-enter and a volatile read pin later accesses
//     below them and a monitor-exit and a volatile write pin earlier
//     accesses above them.
//   * [`MemEffect`] — the pair of alias classes a node reads and writes, its
//     [`MemOrder`], and whether it is a safepoint / an allocation.
//   * [`Graph::may_alias`], [`Graph::may_reorder`],
//     [`Graph::may_reorder_effects`] — the query API. The answer is a
//     [`Reorder`], which carries the *reason*: a [`ReorderProof`] naming the
//     fact that licenses the move, or a [`ReorderBlock`] naming the fact that
//     forbids it. A pass records the proof rather than re-deriving it.
//
// **The token chain becomes a consumer of this model, not a rival truth.**
// [`memory_token_slot`] and [`is_memory_token_slot`] — the canonical
// spellings of the predicate that exists in three private copies today — are
// *derived* from the same [`Op::memory_shape`] table the effect
// classification reads, so the two can no longer drift apart. The chain stays
// as the graph's conservative default ordering; [`Graph::may_reorder`] is how
// a pass proves it may deviate from it.
//
// ## What this model deliberately does NOT answer
//
// `may_reorder` is a **memory-side** question. Two other orderings constrain
// node placement and are unaffected by any answer here:
//
//   * **Control dependence.** A node's `ctrl` input pins it to a block. The
//     query refuses outright ([`ReorderBlock::Control`]) when either node is
//     a control node, and it never licenses moving a node across its own
//     control edge.
//   * **Implicit exception order.** `Op::Load` / `Op::Store` /
//     `Op::ArrayLoad` / `Op::ArrayStore` fault on a null base or an
//     out-of-range index and deopt. Whether `a.f = 1; b.g = 2` may be swapped
//     when `b` is null is a question about the *exception*, not about the
//     memory: the two stores provably touch disjoint cells, and it is the
//     control/exception edge that keeps them in order. That is why those four
//     ops are **not** flagged [`MemEffect::safepoint`] — flagging them would
//     make the model answer "no" to every question and it would be answering
//     the wrong one. A scheduler must combine this answer with control
//     dependence; it must not use it alone.
//
// The query is also **pairwise**: `may_reorder(a, b)` says nothing about a
// third node between them. Moving `b` above `a` past an intervening `c`
// requires the query to hold for `(c, b)` too.

/// The offset operand of a memory access — the second half of an
/// [`AliasClass`]'s address, after the base reference.
///
/// Two accesses to the same base are disjoint exactly when their offsets are
/// two *different compile-time constants*. Every other combination is
/// "possibly the same cell": two runtime values may be equal, and a runtime
/// value may equal any constant. Note that [`AccessOffset::Dynamic`] naming
/// the *same* node twice is a must-alias, not a disjointness — the same node
/// computes the same value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AccessOffset {
    /// A compile-time constant: a field index (`Op::Load` / `Op::Store`) or a
    /// constant-folded array index.
    Const(i64),
    /// A runtime value produced by this node.
    Dynamic(NodeId),
    /// The layout carries no offset operand at all — the compact
    /// `[base, value]` `Store` and `[base]` `Load` forms the EA bridge and the
    /// hand-built test graphs use. Treated as "the object's storage", so it is
    /// never disjoint from another access to the same base.
    Absent,
}

impl AccessOffset {
    /// True when these two offsets provably name different cells.
    ///
    /// The whole precision of the model lives in this one line, so it is
    /// stated positively: disjointness must be *proved*, and only a pair of
    /// unequal constants proves it.
    pub fn provably_distinct(self, other: AccessOffset) -> bool {
        match (self, other) {
            (AccessOffset::Const(a), AccessOffset::Const(b)) => a != b,
            _ => false,
        }
    }
}

/// Where in memory a node reads or writes.
///
/// The classes are *storage kinds*, and different kinds never overlap: an
/// instance field cell is not an array element, an array's length word is not
/// a field cell, a class's static storage is not any object's instance
/// storage, and an object's monitor is not any of them. Those cross-kind
/// disjointness claims are the model's premises, and each is recorded at its
/// arm in [`Graph::may_alias`] so a future op that violates one is caught at
/// the place that would silently start returning the wrong answer.
///
/// [`AliasClass::Any`] is the top of the lattice ("could be anywhere") and
/// [`AliasClass::None`] the bottom ("touches no memory"). An unreadable node
/// layout always degrades to `Any`, never to `None`: the failure direction has
/// to be the one that refuses optimizations, not the one that licenses them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AliasClass {
    /// No memory at all. The bottom of the lattice — aliases nothing, not even
    /// itself.
    None,
    /// The instance field cell `base.#offset`. `Op::Load` / `Op::Store`.
    Field {
        /// The object reference.
        base: NodeId,
        /// The field index (constant in the builder-emitted form).
        offset: AccessOffset,
    },
    /// The array element `array[index]`. `Op::ArrayLoad` / `Op::ArrayStore`.
    ArrayElem {
        /// The array reference.
        array: NodeId,
        /// The element index — usually [`AccessOffset::Dynamic`].
        index: AccessOffset,
        /// The element **type**, read off the accessing op itself
        /// (`Op::ArrayLoad` / `Op::ArrayStore` each carry their [`MemKind`])
        /// by [`access_location`]. A layout that does not yield one degrades
        /// the whole class to [`AliasClass::Any`].
        ///
        /// It lives in the class rather than alongside it because a consumer
        /// that carries its own copy is trusting its *producer* for a fact the
        /// memory model already knows. `x64::simd_analysis`'s `vector_gate`
        /// refuses a vector access to [`MemKind::Ref`] elements, because a
        /// vector store of oops bypasses the GC write barrier; while that
        /// refusal read a caller-supplied `MemKind`, the barrier elision was
        /// only as sound as whoever filled the field in. Reading it here makes
        /// the refusal unforgeable.
        ///
        /// It also buys precision: a Java array's component type is fixed at
        /// allocation, so two accesses at *different* element kinds cannot be
        /// the same object — see [`Graph::may_alias`].
        elem: MemKind,
    },
    /// An array's length word. `Op::ArrayLength`.
    ///
    /// Its own class because array length is **immutable**: nothing in the JVM
    /// writes it after the allocation that produced it, so no store — not even
    /// an opaque call — can clobber it. That is what lets the model prove an
    /// `ArrayLength` commutes with a store, which the token chain (where it
    /// sits between two writers) cannot.
    ArrayLength {
        /// The array reference.
        array: NodeId,
    },
    /// A class's static field cell.
    ///
    /// Keyed by `(class_id, field)` rather than by a base node, because static
    /// storage has no reference operand to resolve — which makes static
    /// accesses the one class the model can disambiguate *exactly*. No op
    /// produces this yet (`getstatic` / `putstatic` have no IR lowering); the
    /// class exists so the lowering lands with a classification instead of
    /// widening everything to [`AliasClass::Any`].
    Static {
        /// Declaring class.
        class_id: u32,
        /// Field index within the class's static storage.
        field: u32,
    },
    /// An object's monitor (lock word). [`Op::MonitorEnter`] /
    /// [`Op::MonitorExit`].
    ///
    /// Aliasing is only half the story for a monitor: its *ordering* comes
    /// from [`MemOrder`], not from this class. Two monitor operations on
    /// provably distinct objects do not alias, and they still do not commute
    /// with the accesses between them, because monitor-enter is an acquire and
    /// monitor-exit a release.
    Monitor {
        /// The locked object.
        obj: NodeId,
    },
    /// Anywhere. The top of the lattice: aliases everything except
    /// [`AliasClass::None`].
    Any,
}

impl AliasClass {
    /// True when this names no memory at all.
    pub fn is_none(self) -> bool {
        matches!(self, AliasClass::None)
    }

    /// True when this names memory (anything but [`AliasClass::None`]).
    pub fn is_some(self) -> bool {
        !self.is_none()
    }

    /// The base reference operand, when the class has one.
    ///
    /// [`AliasClass::Static`] has no base by construction, and `None` / `Any`
    /// are not addressed by a reference.
    pub fn base(self) -> Option<NodeId> {
        match self {
            AliasClass::Field { base, .. } => Some(base),
            AliasClass::ArrayElem { array, .. } => Some(array),
            AliasClass::ArrayLength { array } => Some(array),
            AliasClass::Monitor { obj } => Some(obj),
            AliasClass::None | AliasClass::Static { .. } | AliasClass::Any => Option::None,
        }
    }
}

/// The Java Memory Model ordering strength of a node.
///
/// The lattice is the two independent fence halves:
///
/// ```text
///            SeqCst          (both — a volatile write)
///           /      \
///      Acquire    Release    (monitor-enter / volatile read,
///           \      /          monitor-exit / volatile write)
///            Plain           (an ordinary load or store)
/// ```
///
/// Read the two halves as motion bans, which is what the query needs:
///
/// * **Acquire** on the *earlier* node — nothing after it may move above it.
/// * **Release** on the *later* node — nothing before it may move below it.
///
/// That asymmetry is the JMM's "roach motel": accesses may move *into* a
/// synchronized region from either end, never *out* of it. It is what makes
/// `monitorenter … monitorexit` bound the operations between them without
/// pinning the operations outside them.
///
/// A volatile **read** is `Acquire`. A volatile **write** is
/// [`MemOrder::SeqCst`] rather than a bare `Release`: the JMM requires
/// sequential consistency across volatile accesses, which HotSpot implements
/// with a `StoreLoad` fence after the write, so it is a barrier in both
/// directions. Being stronger than a bare release here is the conservative
/// direction and it is what the acceptance test pins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemOrder {
    /// No ordering of its own — the ordinary case.
    Plain,
    /// Acquire: no later access may move above this node.
    Acquire,
    /// Release: no earlier access may move below this node.
    Release,
    /// A full fence: both halves.
    SeqCst,
}

impl MemOrder {
    /// True when nothing after this node may move above it.
    pub fn has_acquire(self) -> bool {
        matches!(self, MemOrder::Acquire | MemOrder::SeqCst)
    }

    /// True when nothing before this node may move below it.
    pub fn has_release(self) -> bool {
        matches!(self, MemOrder::Release | MemOrder::SeqCst)
    }

    /// True for a full (bidirectional) fence.
    pub fn is_fence(self) -> bool {
        matches!(self, MemOrder::SeqCst)
    }

    /// True when this imposes no ordering of its own.
    pub fn is_plain(self) -> bool {
        matches!(self, MemOrder::Plain)
    }

    /// Least upper bound: the strength of a node that does both.
    pub fn join(self, other: MemOrder) -> MemOrder {
        match (
            self.has_acquire() || other.has_acquire(),
            self.has_release() || other.has_release(),
        ) {
            (true, true) => MemOrder::SeqCst,
            (true, false) => MemOrder::Acquire,
            (false, true) => MemOrder::Release,
            (false, false) => MemOrder::Plain,
        }
    }
}

/// The complete memory effect of one node: what it reads, what it writes, how
/// strongly it is ordered, and whether it is a safepoint or an allocation.
///
/// Reads and writes are kept as two separate [`AliasClass`]es rather than one
/// class plus a read/write flag, because the interesting nodes do both to
/// *different* places — and because a read-read pair commutes regardless of
/// aliasing, which a merged representation cannot express.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MemEffect {
    /// Locations this node may read.
    pub reads: AliasClass,
    /// Locations this node may write.
    pub writes: AliasClass,
    /// JMM ordering strength — see [`MemOrder`].
    pub order: MemOrder,
    /// This node is a safepoint: control may leave for the runtime here and
    /// an interpreter frame may be rebuilt from it (`Op::Call`, `Op::New`,
    /// `Op::NewArray`, `Op::Guard`).
    ///
    /// The constraint that follows is specifically about **writes**: every
    /// store the method has performed must have happened before the frame is
    /// rebuilt, and no store it has not yet performed may have. Loads may
    /// cross a safepoint freely — a load has no visible effect, and re-running
    /// one after a deopt reproduces it. Two safepoints may cross each other
    /// unless one of them allocates.
    ///
    /// See the section header for why the implicit null/bounds faults of
    /// `Op::Load` and friends are *not* modelled here.
    pub safepoint: bool,
    /// This node may allocate (`Op::New`, `Op::NewArray`, and any call, whose
    /// callee may). Two allocating nodes do not commute: which one runs first
    /// is observable through `OutOfMemoryError` and through the addresses and
    /// identity hashes the collector hands out.
    pub allocates: bool,
}

impl MemEffect {
    /// The effect of a node that touches no memory and imposes no ordering.
    pub const NONE: MemEffect = MemEffect {
        reads: AliasClass::None,
        writes: AliasClass::None,
        order: MemOrder::Plain,
        safepoint: false,
        allocates: false,
    };

    /// Reads and writes anything, is a safepoint, and may allocate — the
    /// effect of a call whose callee is unknown, and the fallback for any node
    /// whose layout could not be read. The top of the effect lattice.
    pub const OPAQUE: MemEffect = MemEffect {
        reads: AliasClass::Any,
        writes: AliasClass::Any,
        order: MemOrder::Plain,
        safepoint: true,
        allocates: true,
    };

    /// A plain read of one location.
    pub fn read(class: AliasClass) -> MemEffect {
        MemEffect {
            reads: class,
            ..MemEffect::NONE
        }
    }

    /// A plain write of one location.
    pub fn write(class: AliasClass) -> MemEffect {
        MemEffect {
            writes: class,
            ..MemEffect::NONE
        }
    }

    /// A fresh allocation: writes only storage nothing else can yet name, but
    /// is a safepoint and orders against other allocations.
    pub fn allocation() -> MemEffect {
        MemEffect {
            safepoint: true,
            allocates: true,
            ..MemEffect::NONE
        }
    }

    /// `monitorenter` on `obj` — an **acquire**.
    ///
    /// [`Op::MonitorEnter`] is the producing op, and [`effect_of_node`]
    /// reaches this through [`MemEffect::monitor_enter_of`] after reading the
    /// locked reference out of the node's `[ctrl, mem, obj]` edge list. This
    /// entry point stays because a caller holding a reference and no node —
    /// a test, or a pass reasoning about an effect it has not built yet — is
    /// the case the ordering discipline was first stated for.
    ///
    /// Both halves of the effect are deliberate. It **reads and writes** the
    /// monitor, so two enters of the same object never commute. It is a
    /// **safepoint**, because the runtime may block here and an interpreter
    /// frame may be rebuilt from it — which is also why a coarsened lock
    /// region moves deopt points (`docs/jit/lock-elimination.md` §3).
    pub fn monitor_enter(obj: NodeId) -> MemEffect {
        MemEffect::monitor_enter_of(AliasClass::Monitor { obj })
    }

    /// `monitorexit` on `obj` — a **release**. See [`MemEffect::monitor_enter`].
    pub fn monitor_exit(obj: NodeId) -> MemEffect {
        MemEffect::monitor_exit_of(AliasClass::Monitor { obj })
    }

    /// [`MemEffect::monitor_enter`] over an already-built class.
    ///
    /// [`effect_of_node`] resolves the locked reference out of the edge list
    /// and hands the class straight over, so an unreadable layout arrives here
    /// as [`AliasClass::Any`] — the refusing direction — instead of being
    /// silently rebuilt around a wrong node id.
    pub fn monitor_enter_of(class: AliasClass) -> MemEffect {
        MemEffect {
            reads: class,
            writes: class,
            order: MemOrder::Acquire,
            safepoint: true,
            allocates: false,
        }
    }

    /// [`MemEffect::monitor_exit`] over an already-built class. See
    /// [`MemEffect::monitor_enter_of`].
    pub fn monitor_exit_of(class: AliasClass) -> MemEffect {
        MemEffect {
            reads: class,
            writes: class,
            order: MemOrder::Release,
            safepoint: true,
            allocates: false,
        }
    }

    /// A volatile read of `class` — an **acquire**. Produced by
    /// [`effect_of_node`] for an `Op::Load` marked [`Node::volatile_access`]
    /// (round 9 wave 6); still callable directly for a caller holding an
    /// effect no node produces.
    pub fn volatile_read(class: AliasClass) -> MemEffect {
        MemEffect {
            reads: class,
            writes: AliasClass::None,
            order: MemOrder::Acquire,
            safepoint: false,
            allocates: false,
        }
    }

    /// A volatile write of `class` — a **full fence**, so a barrier in both
    /// directions. See [`MemOrder`] for why this is `SeqCst` and not a bare
    /// release.
    pub fn volatile_write(class: AliasClass) -> MemEffect {
        MemEffect {
            reads: AliasClass::None,
            writes: class,
            order: MemOrder::SeqCst,
            safepoint: false,
            allocates: false,
        }
    }

    /// True when this node reads or writes memory.
    pub fn touches_memory(self) -> bool {
        self.reads.is_some() || self.writes.is_some()
    }

    /// True when this node writes memory.
    pub fn is_write(self) -> bool {
        self.writes.is_some()
    }

    /// True when this node constrains motion in *no* way — no memory, no
    /// fence, no safepoint, no allocation. Such a node commutes with anything.
    pub fn is_inert(self) -> bool {
        !self.touches_memory() && self.order.is_plain() && !self.safepoint && !self.allocates
    }
}

/// What kind of memory access an op performs — the `access` half of
/// [`OpMemoryShape`], and the discriminator [`access_location`] keys off when
/// it reads the base/offset operands out of a node's edge list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemAccess {
    /// Reads one instance field cell.
    FieldRead,
    /// Writes one instance field cell.
    FieldWrite,
    /// Reads one array element.
    ArrayRead,
    /// Writes one array element.
    ArrayWrite,
    /// Reads an array's (immutable) length word.
    LengthRead,
    /// Allocates fresh storage.
    Allocate,
    /// Enters an object's monitor — an acquire, and a safepoint.
    MonitorEnter,
    /// Exits an object's monitor — a release, and a safepoint.
    MonitorExit,
    /// Reads and writes anything.
    Opaque,
}

/// Static description of a memory-effecting op: where its incoming memory
/// token sits, what distinguishes the documented full layout from the compact
/// ones, and what it does to memory.
///
/// **This is the single table.** [`memory_token_slot`] reads it to answer
/// "which slot is the token", and [`effect_of_node`] reads it to answer "what
/// does this node touch". Before this existed the two questions had separate
/// tables in separate files (three copies of the first, four private notions
/// of the second), which is how they drifted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpMemoryShape {
    /// Input slot carrying the incoming memory token in the documented
    /// `[ctrl, mem, …]` form. Always 1 today; named rather than assumed.
    pub token_slot: usize,
    /// Minimum input count of the documented full form.
    ///
    /// The arity guard is what distinguishes that form from the compact
    /// hand-built / EA-bridge layouts `ir_optimize::store_operands` and
    /// `ir_optimize::load_base` also accept — a compact `Store` is
    /// `[base, value]`, whose slot 1 is a *value*, not a token. Below this
    /// arity the node has no memory token at all.
    pub min_full_arity: usize,
    /// What the op does to memory.
    pub access: MemAccess,
}

impl Op {
    /// The memory shape of this op, or `None` when it consumes no memory token
    /// in the documented `[ctrl, mem, …]` layout.
    ///
    /// `None` here is **not** "touches no memory": a memory-typed `Op::Phi`
    /// consumes a token from every predecessor rather than at one fixed slot,
    /// and `Op::Return` ends the method. Both are classified directly by
    /// [`effect_of_node`] / [`is_memory_token_slot`]. This table answers the
    /// narrower question "does this op have a single token slot, and where".
    ///
    /// The arities are the ones `ir_optimize::memory_token_slot`,
    /// `ir_verify::is_memory_token_input` and `lib.rs::ea_memory_token_slot`
    /// each carry a private copy of; the anti-drift test in this file asserts
    /// all of them still agree, op by op and arity by arity.
    pub fn memory_shape(&self) -> Option<OpMemoryShape> {
        let (min_full_arity, access) = match self {
            Op::Load(_) => (3, MemAccess::FieldRead), // [ctrl, mem, base]
            Op::Store(_) => (4, MemAccess::FieldWrite), // [ctrl, mem, base, value]
            Op::ArrayLoad(_) => (4, MemAccess::ArrayRead), // [ctrl, mem, array, index]
            Op::ArrayStore(_) => (5, MemAccess::ArrayWrite), // [ctrl, mem, array, index, value]
            Op::ArrayLength => (3, MemAccess::LengthRead), // [ctrl, mem, array_ref]
            Op::New { .. } => (2, MemAccess::Allocate), // [ctrl, mem]
            Op::NewArray { .. } => (3, MemAccess::Allocate), // [ctrl, mem, length]
            Op::Call { .. } => (2, MemAccess::Opaque), // [ctrl, mem, args…]
            // cov-01. Each is a call to a runtime helper that can intern, load
            // a class, or run `<clinit>` — i.e. arbitrary Java. `Opaque` is the
            // top of the effect lattice (reads Any, writes Any, safepoint,
            // allocates), which is the same classification `Op::Call` gets and
            // the only defensible one for a node whose callee is a class
            // initialiser.
            Op::ConstString { .. } | Op::ConstClass { .. } => (2, MemAccess::Opaque), // [ctrl, mem]
            Op::LoadStatic { .. } => (2, MemAccess::Opaque),                          // [ctrl, mem]
            // cov-05: same Opaque classification as the three above, for the
            // same reason — the helper it calls can allocate a Class mirror.
            Op::InstanceOf { .. } => (3, MemAccess::Opaque), // [ctrl, mem, obj]
            // cov-05: same reasoning, plus the helper can stash a pending
            // ClassCastException and return the deopt/exception sentinel.
            Op::CheckCast { .. } => (3, MemAccess::Opaque), // [ctrl, mem, obj]
            // cov-07: same Opaque classification — `jit_throw_exception` can
            // allocate an NPE (a null athrow), touches thread-local pending-
            // exception state, and is a safepoint.
            Op::Throw => (3, MemAccess::Opaque), // [ctrl, mem, exc]
            Op::LambdaIntToDouble => (4, MemAccess::Opaque), // [ctrl, mem, lambda, index]
            Op::MonitorEnter => (3, MemAccess::MonitorEnter), // [ctrl, mem, obj]
            // The guarded slot-0 accessors. `Opaque` -- the top of the lattice --
            // rather than the `FieldRead`/`FieldWrite` pair the shape suggests:
            // `access_location` reads a `(base, offset)` out of input slots 2
            // and 3, and this node's slot 3 is a DELTA, not an offset (the
            // offsets it uses are resolved from `class_id` at lowering and are
            // carried by no edge at all). Three of the six are a `LOCK XADD` on
            // a VOLATILE field, whose ordering is a full fence anyway, which is
            // what `Opaque` already means here.
            //
            // Added 2026-09-08 with the `Atomic*` families. Before them the op
            // had no entry at all, which was survivable for a pure load and is
            // not for a read-modify-write.
            Op::Unbox { .. } => (3, MemAccess::Opaque), // [ctrl, mem, receiver]
            Op::MonitorExit => (3, MemAccess::MonitorExit), // [ctrl, mem, obj]
            _ => return None,
        };
        Some(OpMemoryShape {
            token_slot: 1,
            min_full_arity,
            access,
        })
    }
}

/// The input slot carrying `node`'s incoming memory token, or `None` when this
/// op does not consume one.
///
/// **The canonical spelling.** Three private copies of this predicate exist —
/// `ir_optimize::memory_token_slot`, `ir_verify::is_memory_token_input` and
/// `lib.rs::ea_memory_token_slot` — reconciled by hand and kept numerically
/// identical by a comment in each. They should all become calls to this
/// function; see the anti-drift test, which fails the moment any of the
/// reachable ones diverges.
///
/// Derived from [`Op::memory_shape`], so the token convention and the effect
/// classification cannot disagree about which ops are memory operations.
pub fn memory_token_slot(node: &Node) -> Option<usize> {
    let shape = node.op.memory_shape()?;
    if node.inputs.len() >= shape.min_full_arity {
        Some(shape.token_slot)
    } else {
        None
    }
}

/// True when input `idx` of `node` is a memory *token* — an ordering edge
/// naming the previous writer — rather than a value the node reads.
///
/// A memory-typed φ is the one op whose token edges are not a single slot:
/// *every* value input (slots 1.., past the control anchor) is a token from
/// one predecessor.
pub fn is_memory_token_slot(node: &Node, idx: usize) -> bool {
    if matches!(node.op, Op::Phi) {
        return node.ty == IrType::Memory && idx >= 1;
    }
    memory_token_slot(node) == Some(idx)
}

// ── The guard token edge ─────────────────────────────────────────────
//
// `Op::Guard` carries `[ctrl, cond]` and shares its control input with the
// nodes it licenses, so "the guard ran before this load" has always been a
// fact about the ORDER INSIDE ONE BLOCK that no edge expressed. Every
// predicate in `ir_optimize` that reasons about a guard —
// `control_token_has_guard`, `licm_hoist_barrier`'s guard arm, `LoopGuards`'
// cone attribution — exists because of that gap.
//
// The token edge closes it: a node the guard licenses names the guard as a
// trailing input, exactly the way a memory operation names its previous
// writer. `Op::Guard` is `IrType::Void` and defines no value, so the slot is
// an ORDERING edge and nothing else — `regalloc::build_live_model` skips it
// (the guard is absent from `ir_op_defines_value`, so `wants_loc` is false)
// and `ir_lower::verify_data_locations` skips it (`is_value_ty(Void)` is
// false). Neither needs a guard-shaped special case; both were checked
// against the code rather than assumed.
//
// **One table, like `Op::memory_shape`.** The slot is identified by ARITY,
// which is what lets the three consumers that must know about it —
// `expected_input_type`, `ir_verify::expected_arity` and
// `ir_verify::data_input_indices` — read this instead of holding three
// opinions. The third is the one a hand-written version of this change
// forgets: `data_input_indices` classifies slots `2..` of a `Load` and ALL
// slots of a `Div` as data, and `check_types` reports a `Void` node in a data
// slot in every build profile, so an unfiltered guard slot would make
// `ir_verify_reject` true for every method containing a guard.

/// Static description of an op that may carry a trailing `Op::Guard` token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpGuardShape {
    /// Input count of the documented form WITHOUT the guard token. The token,
    /// when present, sits at exactly this index and is the last input.
    pub unguarded_arity: usize,
}

impl Op {
    /// The guard-token shape of this op, or `None` when nothing ever licenses
    /// one of these with a guard.
    ///
    /// The members are the guarded families the builder actually emits:
    ///
    /// * `Op::Load` / `Op::ArrayLoad` / `Op::ArrayLength` — the IR `String`
    ///   expansion's receiver-null, `value`-null and bounds guards;
    /// * `Op::Div` / `Op::Rem` — `IrBuilder::add_div_zero_guard`.
    ///
    /// `Op::Store` / `Op::ArrayStore` are deliberately absent: nothing licenses
    /// a store with a guard today, and giving them a shape would put an
    /// optional slot immediately after the one `access_location` and
    /// `ir_optimize::store_operands` read the stored VALUE from — the one
    /// place where an extra trailing edge really would be ambiguous.
    ///
    /// # Why the arities below cannot collide with an existing form
    ///
    /// Each is one past the widest arity the op already has: `Op::Load`'s
    /// documented form is `[ctrl, mem, base, offset]` (4) and its compact
    /// forms are 1..=3; `Op::ArrayLoad`'s is `[ctrl, mem, array, index]` (4);
    /// `Op::ArrayLength`'s is `[ctrl, mem, array]` (3), with the hand-built
    /// `[array]` at 1; `Op::Div`/`Op::Rem` are `[a, b]` (2) and have no other
    /// form at all. So "arity == unguarded_arity + 1" names the guarded shape
    /// and nothing else.
    pub fn guard_shape(&self) -> Option<OpGuardShape> {
        let unguarded_arity = match self {
            // [ctrl, mem, base, offset]
            Op::Load(_) => 4,
            // [ctrl, mem, array, index]
            Op::ArrayLoad(_) => 4,
            // [ctrl, mem, array]
            Op::ArrayLength => 3,
            // [a, b] — a floating data node with no control edge of its own.
            Op::Div | Op::Rem => 2,
            _ => return None,
        };
        Some(OpGuardShape { unguarded_arity })
    }
}

/// The input slot carrying `op`'s trailing `Op::Guard` token at this `arity`,
/// or `None` when this shape carries none.
///
/// The arity-keyed core of [`guard_token_slot`], split out because
/// [`expected_input_type`] is handed `(op, idx, arity)` and never sees a
/// [`Node`].
pub fn guard_token_slot_for(op: &Op, arity: usize) -> Option<usize> {
    let shape = op.guard_shape()?;
    (arity == shape.unguarded_arity + 1).then_some(shape.unguarded_arity)
}

/// The input slot carrying `node`'s trailing `Op::Guard` token, or `None`.
///
/// The companion of [`memory_token_slot`], and read the same way: it answers
/// *where the token is*, not *whether the edge is well-formed*.
/// `ir_verify::check_guard_tokens` is what checks the slot really names a live
/// `Op::Guard`, so a producer that puts a value there is a reported violation
/// rather than a silently-ignored operand.
pub fn guard_token_slot(node: &Node) -> Option<usize> {
    guard_token_slot_for(&node.op, node.inputs.len())
}

/// The `Op::Guard` `node` names as its licensing guard, or [`NO_NODE`].
pub fn guard_token_of(node: &Node) -> NodeId {
    match guard_token_slot(node) {
        Some(slot) => node.inputs.get(slot).copied().unwrap_or(NO_NODE),
        None => NO_NODE,
    }
}

/// `CRATONVM_JIT_IR_GUARD_TOKEN=1` — emit the guard token edge from the
/// builder.
///
/// **Default OFF.** The consumer side (the table above, the verifier's three
/// lanes, `ir_optimize::LoopGuards::permits_hoist`'s use of the token) is
/// complete and correct at either setting; this gate decides only whether
/// [`IrBuilder`] actually appends the edge. It is off because the failure mode
/// of a modelling gap here is neither a crash nor a failing test: a lane that
/// rejects the new shape makes `ir_verify_reject` answer `true`, the method
/// falls back to the single-pass backend, and the optimizing tier goes quiet
/// on every method containing a guard. Turn it on, soak, then flip the
/// default — see `NOTES-opts8.md` §1.
///
/// Latched on first read: the `String` expansion and every `idiv` consult it.
pub fn guard_token_edges_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_GUARD_TOKEN"))
}

/// Append `guard` to a node's edge list when the token edge is enabled.
///
/// The one spelling of the producer side: a site that forgets the gate is a
/// site that does not compile.
pub fn with_guard_token(mut inputs: Vec<NodeId>, guard: NodeId) -> Vec<NodeId> {
    if guard != NO_NODE && guard_token_edges_enabled() {
        inputs.push(guard);
    }
    inputs
}

/// Read the `(base, offset)` a memory access addresses, out of whichever edge
/// layout the node is in.
///
/// Mirrors `ir_optimize::store_operands` / `ir_optimize::load_base` exactly,
/// including their tolerance for the compact hand-built and EA-bridge forms.
/// An unreadable layout returns [`AliasClass::Any`] — the direction that
/// refuses optimizations.
///
/// Offsets come back as [`AccessOffset::Dynamic`]; [`Graph::memory_effect`]
/// constant-folds them, which is where the model gets the precision that lets
/// two field stores commute.
fn access_location(node: &Node, access: MemAccess) -> AliasClass {
    let inputs = node.inputs.as_slice();
    let dyn_at = |i: usize| match inputs.get(i).copied().and_then(node_id_opt) {
        Some(id) => AccessOffset::Dynamic(id),
        None => AccessOffset::Absent,
    };
    match access {
        // `load_base`: compact `[base]` / `[base, index]`, full
        // `[ctrl, mem, base, index?]`.
        MemAccess::FieldRead => match inputs.len() {
            1 | 2 => AliasClass::Field {
                base: inputs[0],
                offset: dyn_at(1),
            },
            n if n >= 3 => AliasClass::Field {
                base: inputs[2],
                offset: dyn_at(3),
            },
            _ => AliasClass::Any,
        },
        // `store_operands`: compact `[base, value]`, full-without-offset
        // `[ctrl, mem, base, value]`, full `[ctrl, mem, base, offset, value]`.
        // Note the arity-4 form's slot 3 is the *value*, so the offset is
        // Absent there — reading it as an offset would invent a disjointness.
        MemAccess::FieldWrite => match inputs.len() {
            2 => AliasClass::Field {
                base: inputs[0],
                offset: AccessOffset::Absent,
            },
            4 => AliasClass::Field {
                base: inputs[2],
                offset: AccessOffset::Absent,
            },
            n if n >= 5 => AliasClass::Field {
                base: inputs[2],
                offset: dyn_at(3),
            },
            _ => AliasClass::Any,
        },
        // `[ctrl, mem, array, index]` — the only documented form. The element
        // kind comes from the op, never from the layout: `array_elem_kind` is
        // what makes the class unforgeable by a consumer (see
        // `AliasClass::ArrayElem::elem`). An op that reached here without one
        // is a shape-table entry nobody taught the kind reader about, and it
        // degrades to `Any` rather than guessing an element type.
        MemAccess::ArrayRead | MemAccess::ArrayWrite => {
            match (inputs.len() >= 4, array_elem_kind(&node.op)) {
                (true, Some(elem)) => AliasClass::ArrayElem {
                    array: inputs[2],
                    index: dyn_at(3),
                    elem,
                },
                _ => AliasClass::Any,
            }
        }
        // `[ctrl, mem, array_ref]`, or the hand-built `[array]`. The arity
        // range is deliberately 1..=3 (see `ir_verify`'s arity lane).
        MemAccess::LengthRead => match inputs.len() {
            1 | 2 => AliasClass::ArrayLength { array: inputs[0] },
            n if n >= 3 => AliasClass::ArrayLength { array: inputs[2] },
            _ => AliasClass::Any,
        },
        MemAccess::Allocate => AliasClass::None,
        // `[ctrl, mem, obj]` — the only form. A monitor is addressed by the
        // locked reference and by nothing else: there is no offset operand, so
        // two monitors on the same object are always a must-alias.
        MemAccess::MonitorEnter | MemAccess::MonitorExit => {
            if inputs.len() >= 3 {
                AliasClass::Monitor { obj: inputs[2] }
            } else {
                AliasClass::Any
            }
        }
        MemAccess::Opaque => AliasClass::Any,
    }
}

/// The element type an array-element access reads or writes, taken from the op
/// itself.
///
/// The one authority for `AliasClass::ArrayElem::elem`. `Op::ArrayLoad` and
/// `Op::ArrayStore` each carry their [`MemKind`] as a type tag, so the element
/// type of an access is a property of the *node*, not of whoever is asking —
/// which is the whole point of moving it into the alias class.
fn array_elem_kind(op: &Op) -> Option<MemKind> {
    match op {
        Op::ArrayLoad(kind) | Op::ArrayStore(kind) => Some(*kind),
        _ => None,
    }
}

/// The memory effect of a node, read from its op and edge layout alone.
///
/// Offsets stay [`AccessOffset::Dynamic`] because folding one needs the graph;
/// use [`Graph::memory_effect`] to get the folded form. Everything else — the
/// alias classes, the ordering, the safepoint and allocation flags — is final
/// here.
///
/// Ops with no memory shape are classified explicitly:
///
/// * a **memory-typed φ** is [`MemEffect::OPAQUE`]. It performs no access of
///   its own, but it is the merge of two token chains, and letting an access
///   float across it is a control-flow question this model does not answer.
/// * a **non-memory φ** is inert: it computes a value, it reads no memory.
/// * `Op::Return` is [`MemEffect::OPAQUE`] — the method's writes must all have
///   happened.
/// * every other **control** op is inert. `Op::Start`'s `Proj(1)` is the
///   initial memory token, but producing a token is not an access; a control
///   node's placement is governed by control edges, and
///   [`Graph::may_reorder`] refuses control nodes outright.
/// * `Op::Dead` is [`MemEffect::OPAQUE`]. A killed node should never be
///   queried; if one is, it must not answer "commutes with everything".
/// * everything else (arithmetic, comparisons, conversions, constants,
///   parameters, `Op::Phi` on data) is inert. `Op::Div` / `Op::Rem` throw on a
///   zero divisor, which is an exception-ordering question, not a memory one —
///   see the section header.
///
/// `Op::MonitorEnter` / `Op::MonitorExit` do have a shape, so they come out of
/// the table like any other memory op — but they are the one family whose
/// effect is neither a read nor a write alone: each reads *and* writes
/// [`AliasClass::Monitor`], is a safepoint, and carries half a JMM fence.
pub fn effect_of_node(node: &Node) -> MemEffect {
    match &node.op {
        Op::Phi => {
            if node.ty == IrType::Memory {
                MemEffect::OPAQUE
            } else {
                MemEffect::NONE
            }
        }
        Op::Return | Op::Dead => MemEffect::OPAQUE,
        Op::Guard { .. } => MemEffect {
            safepoint: true,
            ..MemEffect::NONE
        },
        op => match op.memory_shape() {
            None => MemEffect::NONE,
            Some(shape) => {
                let class = access_location(node, shape.access);
                match shape.access {
                    // A VOLATILE field access (round 9 wave 6): the same cell,
                    // with JMM ordering — an acquire for a read, a full fence
                    // for a write. See `Node::volatile_access`.
                    MemAccess::FieldRead if node.volatile_access => MemEffect::volatile_read(class),
                    MemAccess::FieldWrite if node.volatile_access => {
                        MemEffect::volatile_write(class)
                    }
                    MemAccess::FieldRead | MemAccess::ArrayRead | MemAccess::LengthRead => {
                        MemEffect::read(class)
                    }
                    MemAccess::FieldWrite | MemAccess::ArrayWrite => MemEffect::write(class),
                    MemAccess::Allocate => MemEffect::allocation(),
                    MemAccess::MonitorEnter => MemEffect::monitor_enter_of(class),
                    MemAccess::MonitorExit => MemEffect::monitor_exit_of(class),
                    MemAccess::Opaque => MemEffect::OPAQUE,
                }
            }
        },
    }
}

/// What an op may do to memory — **independent of whether it is GVN-able**.
///
/// # Why this exists
///
/// Four predicates in `ir_optimize` — `preheader_may_trap`,
/// `loop_has_hard_barrier`, `loop_writes_memory` and `is_memory_barrier` —
/// spelled "can this node touch memory?" as
///
/// ```text
/// !op.is_pure() && !op.is_control() && !matches!(op, Load|Store|Phi|Dead)
/// ```
///
/// [`Op::is_pure`] is a *GVN/DCE* predicate — "may I fold, hash and delete
/// this?" — and its allow-list is maintained for that question. Any
/// pure-but-unlisted op was therefore silently reclassified as "may read or
/// write arbitrary memory". That is not a hypothetical: `Op::LCmp` and
/// `Op::FCmp` were missing from it, and the consequence was that **every loop
/// containing an `lcmp`, `fcmpl`, `fcmpg`, `dcmpl` or `dcmpg` was declared to
/// contain a hard memory barrier and lost all of its LICM** — `while (i < n)`
/// over `long` counters, and every FP comparison in a kernel loop. Adding the
/// two ops to `is_pure` fixed that instance and left the mechanism in place
/// for the next op somebody adds to the IR, silently, with no failing test.
///
/// # Why the match below has no `_` arm
///
/// That is the actual fix. A new `Op` variant now fails to compile until
/// someone states what it does to memory, which is the one question this
/// table exists to answer and the one nobody remembered to ask. Deriving the
/// answer for the memory-token ops from [`Op::memory_shape`] rather than
/// re-listing them keeps a single table: this function is the coarse
/// projection of that one, not a second opinion about it.
///
/// # Trapping is a SEPARATE question
///
/// `Op::Div` and `Op::Rem` answer [`MemoryShape::None`] here and that is
/// correct — neither touches memory — but neither is freely movable, because
/// a zero divisor raises `ArithmeticException`. [`may_raise`] is that
/// question, and the call sites ask whichever one they mean. Nothing here
/// makes a trapping node hoistable; all it does is stop one from CLOBBERING
/// an unrelated load somebody else wants hoisted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemoryShape {
    /// Touches no memory at all: arithmetic, constants, comparisons, control,
    /// and a `Guard` (which transfers control to a deopt and writes nothing).
    None,
    /// May read a named location; never writes one.
    Reads,
    /// Writes a named location (and so also constrains reads of it).
    Writes,
    /// May read and write arbitrary memory — a call, an allocation, a class
    /// initialiser, a monitor. The top of the lattice.
    Opaque,
}

impl MemoryShape {
    /// True when this shape touches memory in any way at all.
    pub fn touches_memory(self) -> bool {
        !matches!(self, MemoryShape::None)
    }

    /// True when this shape may WRITE memory — the narrower question
    /// `ir_optimize::loop_writes_memory` asks, where a read cannot clobber the
    /// location a hoisted load reads and so cannot make a hoist unsafe.
    pub fn may_write(self) -> bool {
        matches!(self, MemoryShape::Writes | MemoryShape::Opaque)
    }
}

/// The memory shape of `op`. See [`MemoryShape`] for why this is not
/// [`Op::is_pure`], and for why there is no `_` arm.
pub fn memory_shape_of(op: &Op) -> MemoryShape {
    match op {
        // ── The ops with a `[ctrl, mem, …]` token: ONE table answers ───────
        //
        // `Op::memory_shape` is the table `memory_token_slot` and
        // `effect_of_node` already read; this arm is its coarse projection.
        Op::Load(_)
        | Op::Store(_)
        | Op::ArrayLoad(_)
        | Op::ArrayStore(_)
        | Op::ArrayLength
        | Op::New { .. }
        | Op::NewArray { .. }
        | Op::Call { .. }
        | Op::ConstString { .. }
        | Op::ConstClass { .. }
        | Op::LoadStatic { .. }
        | Op::InstanceOf { .. }
        | Op::CheckCast { .. }
        | Op::Throw
        | Op::LambdaIntToDouble
        | Op::MonitorEnter
        | Op::MonitorExit
        | Op::Unbox { .. } => match op.memory_shape() {
            Some(shape) => match shape.access {
                MemAccess::FieldRead | MemAccess::ArrayRead | MemAccess::LengthRead => {
                    MemoryShape::Reads
                }
                MemAccess::FieldWrite | MemAccess::ArrayWrite => MemoryShape::Writes,
                // An allocation runs a GC-visible store into fresh storage and
                // is a safepoint; a monitor is a full fence on
                // `AliasClass::Monitor`. Both are the top of the lattice for
                // the same reason `effect_of_node` gives them one.
                MemAccess::Allocate
                | MemAccess::MonitorEnter
                | MemAccess::MonitorExit
                | MemAccess::Opaque => MemoryShape::Opaque,
            },
            // Unreachable: every op listed above has a table entry, and the
            // anti-drift test in this file asserts it. `Opaque` rather than a
            // panic, because the safe answer to "I do not know" is the top of
            // the lattice and a compiler does not get to abort a compile over
            // a table it can fall back from.
            None => MemoryShape::Opaque,
        },

        // ── No op-level answer: the conservative one ───────────────────────
        //
        // * A **φ** has no single shape. A `Memory`-typed one carries a token
        //   and `effect_of_node` gives it `OPAQUE`; a value φ touches nothing.
        //   An `&Op` cannot tell them apart, so the op-level answer is the top
        //   of the lattice, and every call site that means the value case
        //   allows `Op::Phi` explicitly (all four of them already do).
        // * **`Op::Dead`** is a node whose inputs have been cleared. Its
        //   effects are not something to reason about — `effect_of_node` says
        //   `OPAQUE` for the same reason — and the call sites that know a dead
        //   node emits nothing exempt it by name.
        // * **`Op::Return`** ends the method; everything written before it is
        //   observable after it. `effect_of_node` says `OPAQUE`.
        Op::Phi | Op::Dead | Op::Return => MemoryShape::Opaque,

        // ── Touches no memory ─────────────────────────────────────────────
        //
        // `Op::Guard` belongs here on the merits: it carries `[ctrl, cond]`,
        // no memory edge at all, and transfers control to a deopt. It is
        // still a barrier to *motion* — see `ir_optimize::licm_hoist_barrier`
        // and `control_token_has_guard` — because a node hoisted above it runs
        // on an input the guard was going to reject. That is a control fact,
        // not a memory one, and conflating the two is what this enum is for.
        //
        // `Op::Div` / `Op::Rem` likewise: no memory, but [`may_raise`].
        Op::Start
        | Op::If
        // A multi-way branch reads its key out of a register and writes
        // nothing. The jump TABLE `ir_lower` may emit for it is read-only
        // constant data in the code buffer, not a location any Java program
        // can name, so it is outside this lattice entirely.
        | Op::Switch { .. }
        | Op::Merge
        | Op::Region
        | Op::Proj(_)
        | Op::Const(_)
        | Op::ConstF(_)
        | Op::Param(_)
        | Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::Rem
        | Op::Neg
        | Op::And
        | Op::Or
        | Op::Xor
        | Op::Shl
        | Op::Shr
        | Op::UShr
        | Op::Cmp(_)
        | Op::LCmp
        | Op::FCmp { .. }
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
        | Op::Guard { .. }
        | Op::ScalarIntrinsic(_) => MemoryShape::None,
    }
}

/// May executing this op raise, deopt, or fault — i.e. transfer control
/// somewhere other than its own successor?
///
/// The companion question to [`memory_shape_of`], and the reason that one can
/// answer [`MemoryShape::None`] for `Op::Div` without making a division
/// hoistable. Separating them is the whole point: the memory question decides
/// whether a node can CLOBBER what somebody else wants to move, and this one
/// decides whether the node ITSELF can move.
///
/// Also with no `_` arm, and for the same reason.
///
/// Note what this does *not* say: a caller that exempts some trapping op
/// because of a property of its own analysis — `ir_optimize::preheader_may_trap`
/// exempts `Load`/`Store`, because the nodes it is clearing a path for are
/// themselves guarded loads at the same control token — states that exemption
/// at the call site, where the justification lives. It is not encoded here,
/// because here it would be false.
pub fn may_raise(op: &Op) -> bool {
    match op {
        // Arithmetic that traps on a zero divisor. The only pure-looking
        // members of this list, and the reason it exists.
        Op::Div | Op::Rem => true,
        // A null base faults; an out-of-range index raises; a store into an
        // array of references can raise `ArrayStoreException`.
        Op::Load(_) | Op::Store(_) | Op::ArrayLoad(_) | Op::ArrayStore(_) | Op::ArrayLength => true,
        // Allocation raises `OutOfMemoryError` / `NegativeArraySizeException`;
        // anything that may run `<clinit>`, a callee, or a runtime helper can
        // raise whatever that code raises.
        Op::New { .. }
        | Op::NewArray { .. }
        | Op::Call { .. }
        | Op::ConstString { .. }
        | Op::ConstClass { .. }
        | Op::LoadStatic { .. }
        | Op::InstanceOf { .. }
        | Op::CheckCast { .. }
        | Op::Throw
        | Op::LambdaIntToDouble
        | Op::MonitorEnter
        | Op::MonitorExit
        | Op::Unbox { .. } => true,
        // A guard's whole purpose is to leave the compiled body.
        Op::Guard { .. } => true,
        // A removed node: conservative, like its memory shape.
        Op::Dead => true,
        // Everything else computes a value or joins control and cannot fail.
        // `Op::ScalarIntrinsic` is on this side deliberately: every member of
        // `ScalarOp` is branchless arithmetic with a defined answer for every
        // input, zero included (see the bit-scan note on that enum).
        Op::Start
        | Op::Return
        | Op::If
        // Every key has an edge: the ones in `[low, high]` by projection and
        // the rest by the default edge, which JVMS makes mandatory. There is
        // no "nothing matched" outcome for a switch to raise.
        | Op::Switch { .. }
        | Op::Merge
        | Op::Region
        | Op::Proj(_)
        | Op::Const(_)
        | Op::ConstF(_)
        | Op::Param(_)
        | Op::Phi
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
        | Op::LCmp
        | Op::FCmp { .. }
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
        | Op::ScalarIntrinsic(_) => false,
    }
}

/// Largest allocation set [`Graph::ref_origin`] will carry before giving up
/// and reporting unknown provenance.
///
/// A reference that may be one of nine different allocations is not a
/// reference any pass profits from disambiguating, and the bound keeps the
/// join over a deep φ web linear.
pub const MAX_ALIAS_ALLOC_SET: usize = 8;

/// Recursion bound for [`Graph::ref_origin`]. A loop-carried φ cycle resolves
/// to unknown rather than hanging.
const REF_ORIGIN_MAX_DEPTH: u32 = 32;

/// What a reference node may point to — the provenance half of
/// [`Graph::may_alias`].
///
/// Same shape as `ir_optimize`'s private `RefPointsTo`, which it is meant to
/// replace: `allocs` is the set of in-method allocations the reference may
/// name (`None` = unknown provenance, may be anything), and `pre_existing`
/// records whether it may be an object that existed before the method ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefOrigin {
    /// The fresh in-method allocations this reference may name, or `None` when
    /// its provenance is unknown (a loaded reference, a call result, an
    /// over-wide φ join).
    pub allocs: Option<Vec<NodeId>>,
    /// True when it may be a *pre-existing* reference — a parameter, `this`.
    /// Never an in-method allocation.
    pub pre_existing: bool,
}

impl RefOrigin {
    /// Unknown provenance: may be any object at all.
    pub fn unknown() -> RefOrigin {
        RefOrigin {
            allocs: None,
            pre_existing: false,
        }
    }

    /// True when nothing is known about this reference.
    pub fn is_unknown(&self) -> bool {
        self.allocs.is_none()
    }

    /// Exactly one fresh allocation.
    fn alloc(id: NodeId) -> RefOrigin {
        RefOrigin {
            allocs: Some(vec![id]),
            pre_existing: false,
        }
    }

    /// A pre-existing reference and nothing else.
    fn pre_existing_only() -> RefOrigin {
        RefOrigin {
            allocs: Some(Vec::new()),
            pre_existing: true,
        }
    }

    /// Join another origin into this one (the φ merge). Any unknown input
    /// poisons the result, and overflowing [`MAX_ALIAS_ALLOC_SET`] degrades it
    /// to unknown.
    fn absorb(&mut self, other: RefOrigin) {
        self.pre_existing |= other.pre_existing;
        match (self.allocs.as_mut(), other.allocs) {
            (Some(mine), Some(theirs)) => {
                for id in theirs {
                    if !mine.contains(&id) {
                        mine.push(id);
                    }
                }
                if mine.len() > MAX_ALIAS_ALLOC_SET {
                    self.allocs = None;
                }
            }
            _ => self.allocs = None,
        }
    }

    /// True when these two references provably name **different** objects.
    ///
    /// Requires both sides to have known provenance, their allocation sets to
    /// be disjoint, and at most one of them to be possibly-pre-existing —
    /// because two parameters can be the same object (`foo(x, x)`), while a
    /// fresh allocation can never be a value that existed before the method
    /// ran.
    pub fn provably_distinct(&self, other: &RefOrigin) -> bool {
        let (mine, theirs) = match (&self.allocs, &other.allocs) {
            (Some(a), Some(b)) => (a, b),
            _ => return false,
        };
        if self.pre_existing && other.pre_existing {
            return false;
        }
        !mine.iter().any(|id| theirs.contains(id))
    }
}

/// A `may_reorder` answer, with the fact that justifies it.
///
/// The point of returning a reason rather than a `bool` is the acceptance
/// criterion: *"optimizations cannot reorder through side effects without a
/// proof encoded in the graph."* A pass that moves a node records the
/// [`ReorderProof`] it moved on; a pass that declines records the
/// [`ReorderBlock`] it hit. Both are checkable after the fact, and a `Blocked`
/// reason is the diagnostic a missed optimization report needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reorder {
    /// The two nodes may be swapped, on the strength of this fact.
    Allowed(ReorderProof),
    /// They may not, because of this fact.
    Blocked(ReorderBlock),
}

impl Reorder {
    /// True when the swap is licensed.
    pub fn is_allowed(self) -> bool {
        matches!(self, Reorder::Allowed(_))
    }

    /// True when the swap is refused.
    pub fn is_blocked(self) -> bool {
        matches!(self, Reorder::Blocked(_))
    }
}

/// Why a reorder is licensed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReorderProof {
    /// At least one of the two constrains motion in no way at all — no memory,
    /// no fence, no safepoint, no allocation.
    EffectFree,
    /// Neither node writes memory, so no read can observe a difference.
    ReadOnly,
    /// Both touch memory, but the locations are provably disjoint.
    DisjointLocations,
}

/// Why a reorder is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReorderBlock {
    /// One of the ids does not name a node.
    UnknownNode,
    /// One of them is a control node. Control placement is governed by control
    /// edges, which this model does not reason about.
    Control,
    /// The later node consumes the earlier one as a **value** (or control)
    /// input. Note this deliberately does *not* fire for a memory-token edge:
    /// breaking an over-strict token edge is exactly what the model is for.
    DataDependence,
    /// One of them is a full fence — a volatile access.
    Fence,
    /// The earlier node is an acquire (monitor-enter, volatile read), so
    /// nothing after it may move above it.
    Acquire,
    /// The later node is a release (monitor-exit, volatile write), so nothing
    /// before it may move below it.
    Release,
    /// Both allocate, and allocation order is observable.
    Allocation,
    /// One is a safepoint and the other writes memory.
    Safepoint,
    /// Their locations may be the same, and at least one of them writes.
    MayAlias,
}

impl Graph {
    /// The memory effect of `id`, with constant offsets folded.
    ///
    /// The graph-aware form of [`effect_of_node`]: an offset operand that
    /// resolves to an `Op::Const` becomes an [`AccessOffset::Const`], which is
    /// the *only* thing that ever proves two accesses to the same base
    /// disjoint. A node id that names nothing answers [`MemEffect::OPAQUE`].
    pub fn memory_effect(&self, id: NodeId) -> MemEffect {
        let node = match self.node_opt(id) {
            Some(n) => n,
            None => return MemEffect::OPAQUE,
        };
        let mut eff = effect_of_node(node);
        eff.reads = self.fold_offsets(eff.reads);
        eff.writes = self.fold_offsets(eff.writes);
        eff
    }

    /// Replace a [`AccessOffset::Dynamic`] offset with [`AccessOffset::Const`]
    /// when the node it names is an integer constant.
    fn fold_offsets(&self, class: AliasClass) -> AliasClass {
        let fold = |off: AccessOffset| match off {
            AccessOffset::Dynamic(id) => match self.node_opt(id).map(|n| &n.op) {
                Some(Op::Const(v)) => AccessOffset::Const(*v),
                _ => off,
            },
            other => other,
        };
        match class {
            AliasClass::Field { base, offset } => AliasClass::Field {
                base,
                offset: fold(offset),
            },
            AliasClass::ArrayElem { array, index, elem } => AliasClass::ArrayElem {
                array,
                index: fold(index),
                elem,
            },
            other => other,
        }
    }

    /// The element type node `id` accesses, when it is an array-element access
    /// (`Op::ArrayLoad` / `Op::ArrayStore`); `None` for every other node, and
    /// for an id that names nothing.
    ///
    /// The node-level spelling of `AliasClass::ArrayElem::elem`, for a caller
    /// that holds a `NodeId` and not a class — `x64::simd_analysis`'s
    /// `vector_gate` builds its body descriptions from node ids before it ever
    /// reaches an alias class. Both read `array_elem_kind`, so a consumer
    /// cannot get a different answer here than the memory model uses.
    pub fn element_kind(&self, id: NodeId) -> Option<MemKind> {
        array_elem_kind(&self.node_opt(id)?.op)
    }

    /// What reference `id` may point to, following `Op::Phi` merges.
    ///
    /// Leaves: `Op::New` / `Op::NewArray` are exactly themselves (the node *is*
    /// the reference — see the builder's `new` arm); `Op::Param` is a
    /// pre-existing reference; anything else is unknown provenance. A φ joins
    /// its value inputs, skipping the control anchor at its head.
    pub fn ref_origin(&self, id: NodeId) -> RefOrigin {
        self.ref_origin_at(id, 0)
    }

    fn ref_origin_at(&self, id: NodeId, depth: u32) -> RefOrigin {
        if depth > REF_ORIGIN_MAX_DEPTH {
            return RefOrigin::unknown();
        }
        let node = match self.node_opt(id) {
            Some(n) => n,
            None => return RefOrigin::unknown(),
        };
        match &node.op {
            Op::New { .. } | Op::NewArray { .. } => RefOrigin::alloc(id),
            Op::Param(_) => RefOrigin::pre_existing_only(),
            Op::Phi => {
                let mut joined = RefOrigin {
                    allocs: Some(Vec::new()),
                    pre_existing: false,
                };
                let mut saw_value = false;
                for &inp in node.inputs.iter() {
                    let pred = match self.node_opt(inp) {
                        Some(p) => p,
                        None => continue,
                    };
                    if pred.op.is_control() {
                        continue; // the φ's region/merge anchor, not a value
                    }
                    saw_value = true;
                    joined.absorb(self.ref_origin_at(inp, depth + 1));
                }
                if saw_value {
                    joined
                } else {
                    RefOrigin::unknown()
                }
            }
            _ => RefOrigin::unknown(),
        }
    }

    /// True when two reference nodes may name the same object.
    ///
    /// The same id is always a must-alias. Otherwise the answer is the
    /// negation of [`RefOrigin::provably_distinct`].
    pub fn refs_may_alias(&self, a: NodeId, b: NodeId) -> bool {
        if a == b {
            return true;
        }
        !self.ref_origin(a).provably_distinct(&self.ref_origin(b))
    }

    /// True when two alias classes may name overlapping memory.
    ///
    /// This is the model's core disjointness judgement, and every "false" it
    /// returns is a claim that has to be true of the VM's object layout:
    ///
    /// * **Different storage kinds never overlap.** A field cell is not an
    ///   array element (a Java object is either an array or a class instance,
    ///   never both, and verified bytecode never reaches `getfield` on an
    ///   array or `aaload` on a non-array); an array's length word is not a
    ///   field cell; a class's static storage is a per-class area no object
    ///   reference addresses; an object's monitor is not any of them. If an op
    ///   is ever added that violates one of these — a raw `Unsafe`-style
    ///   access with a computed offset, say — it must classify as
    ///   [`AliasClass::Any`], not as one of the structured classes.
    /// * **`ArrayLength` is read-only storage.** Nothing writes an array's
    ///   length, so [`AliasClass::ArrayLength`] never conflicts with a write —
    ///   not even an opaque one, since [`AliasClass::Any`] is handled by the
    ///   earlier arm only because an opaque node may *read* it too. (An
    ///   `Any` write does conservatively conflict with a length read; the
    ///   precision available here is the cross-kind one, which is what lets an
    ///   `ArrayLength` commute with a field or element store.)
    /// * **Same kind, same base:** disjoint only when the offsets are two
    ///   different compile-time constants ([`AccessOffset::provably_distinct`]).
    /// * **Same kind, different base:** disjoint only when the bases are
    ///   provably different objects ([`Graph::refs_may_alias`]).
    /// * **Two array elements at different element types never overlap.** A
    ///   Java array's component type is fixed at allocation and
    ///   `AliasClass::ArrayElem::elem` is read off the accessing op, so an
    ///   `int[]` element and a reference element are two different objects
    ///   even when neither reference's provenance is known. This is the one
    ///   premise here that a *wrong* element tag could break, which is exactly
    ///   why the tag is not caller-supplied.
    ///
    /// Aliasing alone does not decide reordering — see
    /// [`Graph::may_reorder_effects`], which also applies the JMM fences and
    /// the safepoint rule.
    pub fn may_alias(&self, a: AliasClass, b: AliasClass) -> bool {
        use AliasClass as C;
        match (a, b) {
            // Bottom: no memory, nothing to conflict with.
            (C::None, _) | (_, C::None) => false,
            // Top: could be anywhere.
            (C::Any, _) | (_, C::Any) => true,
            (C::Field { base: x, offset: i }, C::Field { base: y, offset: j }) => {
                !i.provably_distinct(j) && self.refs_may_alias(x, y)
            }
            (
                C::ArrayElem {
                    array: x,
                    index: i,
                    elem: ke,
                },
                C::ArrayElem {
                    array: y,
                    index: j,
                    elem: kf,
                },
            ) => {
                // A Java array's component type is fixed at allocation, so two
                // accesses at different element kinds cannot be reading the
                // same object — an `int[]` cell is never an `Object[]` cell.
                // The `x != y` guard keeps that from becoming an *invented*
                // disjointness on an ill-typed graph where one array node is
                // accessed at two kinds: there the references must-alias, and
                // the model has to keep saying so rather than trusting the two
                // type tags to be consistent.
                let kinds_disjoint = x != y && ke != kf;
                !kinds_disjoint && !i.provably_distinct(j) && self.refs_may_alias(x, y)
            }
            (C::ArrayLength { array: x }, C::ArrayLength { array: y }) => self.refs_may_alias(x, y),
            (
                C::Static {
                    class_id: c,
                    field: i,
                },
                C::Static {
                    class_id: d,
                    field: j,
                },
            ) => c == d && i == j,
            (C::Monitor { obj: x }, C::Monitor { obj: y }) => self.refs_may_alias(x, y),
            // Different storage kinds — see the premises in the doc above.
            _ => false,
        }
    }

    /// May the node `earlier` and the node `later` — given in **program
    /// order** — be swapped?
    ///
    /// This is the question a pass asks as *"may I move this load above that
    /// store?"*: `may_reorder(store, load)`, with the store first because that
    /// is the order they are in today.
    ///
    /// On top of [`Graph::may_reorder_effects`] it adds the two graph-level
    /// refusals:
    ///
    /// * either node being a **control** node ([`ReorderBlock::Control`]);
    /// * `later` consuming `earlier` through a **non-token** input edge
    ///   ([`ReorderBlock::DataDependence`]). A memory-token edge is
    ///   deliberately *not* a refusal: the token chain is this model's
    ///   conservative default, and overriding it with a proof is the entire
    ///   purpose of the query. A pass that acts on an `Allowed` answer must
    ///   then repair the chain (`ir_optimize::kill_store_splicing_memory_chain`
    ///   is the existing splice) so the graph still encodes the new order.
    ///
    /// See the section header for what this does *not* cover: control
    /// dependence, implicit-exception order, and any third node between the
    /// two.
    pub fn may_reorder(&self, earlier: NodeId, later: NodeId) -> Reorder {
        let (a, b) = match (self.node_opt(earlier), self.node_opt(later)) {
            (Some(a), Some(b)) => (a, b),
            _ => return Reorder::Blocked(ReorderBlock::UnknownNode),
        };
        if a.op.is_control() || b.op.is_control() {
            return Reorder::Blocked(ReorderBlock::Control);
        }
        for (i, &inp) in b.inputs.iter().enumerate() {
            if inp == earlier && !is_memory_token_slot(b, i) {
                return Reorder::Blocked(ReorderBlock::DataDependence);
            }
        }
        self.may_reorder_effects(self.memory_effect(earlier), self.memory_effect(later))
    }

    /// [`Graph::may_reorder`] over two effects directly, for a caller holding
    /// an effect that no node produces yet.
    ///
    /// That is not a hypothetical: volatile field access still has no `Op`
    /// variant, so [`MemEffect::volatile_read`] / [`MemEffect::volatile_write`]
    /// are the only way to state its ordering — and the only way to test it
    /// before the op lands. The monitors were in that position too until
    /// [`Op::MonitorEnter`] / [`Op::MonitorExit`] landed; they now reach these
    /// rules through [`Graph::may_reorder`] like any other node, and
    /// [`MemEffect::monitor_enter`] is kept as the node-free entry point.
    ///
    /// The rules, in the order they are applied:
    ///
    /// 1. Either side inert → allowed ([`ReorderProof::EffectFree`]).
    /// 2. Either side a full fence → [`ReorderBlock::Fence`].
    /// 3. `earlier` is an acquire → [`ReorderBlock::Acquire`]: nothing after a
    ///    monitor-enter or a volatile read may move above it.
    /// 4. `later` is a release → [`ReorderBlock::Release`]: nothing before a
    ///    monitor-exit or a volatile write may move below it.
    ///    Rules 3 and 4 are one-sided on purpose — that asymmetry is the JMM's
    ///    roach motel, and it is why a synchronized region *bounds* the
    ///    accesses inside it without pinning the ones outside.
    /// 5. Both allocate → [`ReorderBlock::Allocation`].
    /// 6. Their locations may conflict (write/read, read/write, write/write) →
    ///    [`ReorderBlock::MayAlias`].
    /// 7. One is a safepoint and the other writes → [`ReorderBlock::Safepoint`].
    /// 8. Neither writes → allowed ([`ReorderProof::ReadOnly`]).
    /// 9. Otherwise allowed ([`ReorderProof::DisjointLocations`]).
    ///
    /// The alias test precedes the safepoint test only so that the *reason* a
    /// call blocks a store is the sharper `MayAlias` rather than `Safepoint`;
    /// both refuse.
    pub fn may_reorder_effects(&self, earlier: MemEffect, later: MemEffect) -> Reorder {
        if earlier.is_inert() || later.is_inert() {
            return Reorder::Allowed(ReorderProof::EffectFree);
        }
        if earlier.order.is_fence() || later.order.is_fence() {
            return Reorder::Blocked(ReorderBlock::Fence);
        }
        if earlier.order.has_acquire() {
            return Reorder::Blocked(ReorderBlock::Acquire);
        }
        if later.order.has_release() {
            return Reorder::Blocked(ReorderBlock::Release);
        }
        if earlier.allocates && later.allocates {
            return Reorder::Blocked(ReorderBlock::Allocation);
        }
        let conflicts = self.may_alias(earlier.writes, later.reads)
            || self.may_alias(earlier.reads, later.writes)
            || self.may_alias(earlier.writes, later.writes);
        if conflicts {
            return Reorder::Blocked(ReorderBlock::MayAlias);
        }
        if (earlier.safepoint && later.is_write()) || (later.safepoint && earlier.is_write()) {
            return Reorder::Blocked(ReorderBlock::Safepoint);
        }
        if !earlier.is_write() && !later.is_write() {
            return Reorder::Allowed(ReorderProof::ReadOnly);
        }
        Reorder::Allowed(ReorderProof::DisjointLocations)
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
    /// Abstract monitor stack from each FORWARD predecessor. Structured locking
    /// (JVMS §2.11.10) makes these identical on every edge into a join, and the
    /// builder refuses the method when they are not — see
    /// [`IrBuilder::unstructured_monitors`].
    monitor_snapshots: Vec<Vec<NodeId>>,
    /// Whether this merge has been visited (target reached during forward walk).
    visited: bool,
}

// ── String access intrinsics in the optimizing tier ───────────────────
//
// `String.length()`, `String.isEmpty()` and `String.charAt(int)` lowered BY
// this tier, instead of dispatched out of it.
//
// ## The defect this closes
//
// string-charat-loop-cost-and-the-unsteerable-intrinsic-20260901
// measured `s.charAt(i)` in a counted loop at a FLAT 186-196 ns/char from
// 200 000 to 100 000 000 characters, against 5.2 ns/char for the identical
// loop over a `char[]` in the same run and 0.5 on HotSpot. The cause is
// structural rather than a tuning problem, and the page's own grep is the
// statement of it: neither `ir.rs` nor `ir_lower.rs` mentioned a String
// intrinsic or a string layout even once.
//
// Every String access intrinsic lives in the single-pass backend
// (`x64/bytecode_walk.rs`, region `STRING_ACCESS`), so a method ADMITTED to
// the optimizing pipeline cannot keep them and falls back to the whole
// `charAt -> isLatin1 -> StringLatin1.charAt -> String.checkIndex ->
// Preconditions.checkIndex` chain, whose tail is a registered NATIVE. The
// in-tree price of that loss is 504 ns/call against 135 (3.7x), recorded on
// `string_intrinsic_pin_enabled` in `lib.rs`.
//
// ## Why this expands into ordinary nodes rather than adding an `Op`
//
// Two shapes were possible. A new `Op::StringCharAt` that `ir_lower` expands
// during lowering is the tidier IR, but it is BLOCKED: `ir_verify`'s
// `expected_arity` is an exhaustive `match` over `Op` that ends at
// `Op::Dead => (0, 0)` with no wildcard arm, so a new variant is a compile
// error in a file this change may not touch. (`ir_optimize`, `ir_schedule`,
// `range_analysis`, `escape_analysis`, `regalloc` and `ir_lower` all carry
// `_ =>` arms and would have been fine.) It is the same rule that already
// forced [`InlineScopeTable`] to be a side table rather than a field on
// [`Graph`] or [`SafepointSnapshot`].
//
// Expanding here is also the better answer on its own merits, for a reason
// the exhaustiveness accident does not supply: the `value` and `coder` reads
// become ordinary `Op::Load`s that the existing optimizer can SEE. Neither
// field is ever written after construction, so `ir_optimize`'s LICM
// (`loop_store_clobber` / `load_safe_past_clobber`) may hoist both out of a
// counted loop and leave only the element read in the body — which is the
// shape HotSpot's 0.5 ns/char comes from, and which is unreachable for any
// node the tier treats as one opaque unit.
//
// MEASURED 2026-09-02, and for the first ten weeks of this emitter's life
// the answer was NO. `ir_optimize::loop_has_hard_barrier` refused any loop
// holding a node that is not pure, not control and not `Load`/`Store`/`Phi`
// — and this expansion emits four `Op::Guard`s, two `Op::ArrayLoad`s and an
// `Op::ArrayLength` into the very loop it wants hoisting out of, so **it
// disqualified its own LICM**. `CRATONVM_DBG=licm` reported
// `hard_barrier=true` with 0 hoisted on every candidate header, and the arm
// ran at ~78 ns/char rather than ~3.
//
// `ir_optimize::loop_writes_memory` and the restricted read-hoist arm beside
// the `Op::ArrayLength` one fixed that: all four loads leave the loop and
// the arm reads **3.2-3.5 ns/char**, a tie with the single-pass body it was
// 23x behind. `CRATONVM_JIT_NO_LICM_READ_HOIST=1` is the B arm.
//
// Separately: both loads sit at an INVOKE pc, and `ir_lower`'s inline
// compact-`getfield` path is keyed by GETFIELD pc, so until the same day
// each one was a checked `jit_getfield` helper CALL — 917,203,334 of them in
// one `probes/CharAtCostCurve.java` run. `try_compile_inner` now installs
// the two rows they need (`CRATONVM_JIT_NO_STRING_ACCESS_INLINE_ROWS=1`
// restores the fallback); it is worth ~4.9x on its own and is mostly
// subsumed by the hoist for a loop shape, which is why both are kept.
//
// ## What is emitted
//
// Compact-String representation, exactly as the single-pass region assumes:
// `value` is a `byte[]`, `coder` is 0 (LATIN1, 1 byte/char) or 1 (UTF16, 2
// little-endian bytes/char), and `length() == value.length >> coder`.
//
//     coder  = Op::Load(Int)   [ctrl, mem, recv, Const(coder_field_index)]
//     value  = Op::Load(Ref)   [ctrl, mem, recv, Const(value_field_index)]
//     len    = Op::ArrayLength [ctrl, mem, value]
//     count  = Op::UShr(len, coder)
//     charAt: off = Op::Shl(index, coder)
//             lo  = Op::ArrayLoad(Byte)(value, off)         & 0xFF
//             hi  = Op::ArrayLoad(Byte)(value, off + coder) & 0xFF
//             ch  = lo | ((hi << 8) & (0 - coder))
//
// The decode is BRANCHLESS where the single-pass region uses a
// `TEST coder; JNZ utf16` diamond, because a diamond built here would have to
// open a control-flow region in the middle of one bytecode's expansion — new
// `Op::If`/`Op::Merge` nodes at a pc that is not a branch target — which the
// merge bookkeeping (`merges`, `loop_headers`, and the one safepoint snapshot
// per bci) is not built to see. The second byte's address is `off + coder`:
// the SAME byte as `lo` when `coder == 0`, and the high half of the UTF-16
// unit when it is 1. Both are in bounds whenever `index` is — a valid UTF16
// `value.length` is even, so `2*index + 1 <= value.length - 1` — and the
// `0 - coder` mask discards the second byte on the LATIN1 path. `baload`
// sign-extends, so each half is masked to `0xFF` before it is combined.
//
// ## The narrow-oop `value` load
//
// This expansion NEVER emits a hand-rolled displacement for `String.value`,
// and that is the point rather than an omission. `Op::Load`'s only two
// lowerings are `ir_lower::emit_inline_compact_getfield`, which refuses the
// site outright under narrow oops (`x64::narrow_oops_block_inline_fields`),
// and the compact- and narrow-aware `jit_getfield` helper. Under compressed
// oops a compact `String.value` is a 4-byte narrow oop, and an unconditional
// 8-byte load reads half a pointer plus the adjacent coder/hash field and
// dereferences the result — the defect `emit_load_string_value_ptr` already
// had to be fixed for once in the single-pass backend
// (`StringFieldLayout::value_compact_is_narrow`). Of the layout, this file
// reads only `value_field_index`, `coder_field_index`, `has_coder` and
// `string_class_id`: the four facts that are slot indices and identities
// rather than byte offsets, and which therefore cannot encode a width at all.
//
// `coder` is loaded BEFORE `value` deliberately. The `Op::Load` helper arm is
// a `CALL`, and loading the `Ref` last is the shortest window in which a live
// oop sits across one.
//
// ## Every uncertain case deopts; none of them throws
//
// A null receiver, a null `value` array and an out-of-range index all reach
// `Op::Guard { bci }` (plus the null + bounds guards `Op::ArrayLength` and
// `Op::ArrayLoad` already emit), all at the `invoke`'s own bci. The
// interpreter then RE-EXECUTES the `invokevirtual`, and the native
// implementation reproduces the exact `NullPointerException` /
// `StringIndexOutOfBoundsException`, in-method handlers included. No `CALL`
// and no fabricated exception is emitted on the inline path — the same
// discipline the single-pass region keeps with its shared reason-6 trap.
//
// The resume bci is the `invoke`'s, and it is right WITHOUT new bookkeeping:
// `IrBuilder::build` pushes a `SafepointSnapshot { bci: pc, .. }` at the top
// of every bytecode, BEFORE the opcode runs, so the snapshot at the `invoke`
// pc still holds the receiver (and the index) on the operand stack — the
// same state `snapshot_pre_intrinsic_call` records in the single-pass region.
// Getting a resume bci wrong is a known and expensive failure here: a spliced
// body recorded a bci the method does not have and turned an
// `IndexOutOfBoundsException` into an `InternalError`
// (`ir-inline-turns-an-index-out-of-bounds-into-an-internalerror-FIXED-20260828`,
// cited on `ir_inline_enabled`). That is also why this expansion REFUSES
// inside an open splice: a spliced region records no snapshot at all, so a
// guard there would deopt into an empty frame.
//
// ## The receiver guard
//
// The policy is `try_resolve_string_intrinsic`'s, CALLED rather than
// re-derived: a `java/lang/String` site is final and monomorphic and needs no
// guard (`guard_class_id == 0`); a `java/lang/CharSequence` site may see any
// CharSequence and needs a `[recv+0] == string_class_id` compare; a
// CharSequence site with no resolved String class id is refused outright.
//
// This tier then refuses the GUARDED case too, and counts it
// (`guarded_site_refused`). There is no IR node that reads an `ObjectHeader`
// class id — `Op::Load` addresses a field, not the header — and the only
// existing substitute, `Op::InstanceOf`, is an opaque helper `CALL` that
// would sit in the loop body and defeat the exercise. Emitting the decode
// without the compare would admit a `StringBuilder` receiver to a
// String-layout decode, which is a wrong VALUE, not a slow one. A refused
// site simply dispatches exactly as it does today, so the refusal costs
// nothing that is not already being paid.
//
// ## Both switches, the THIRD door, and why the default is ON
//
// `CRATONVM_JIT_IR_STRING_INTRINSICS=0` disables this emitter; it defaults
// ON. That default is not a claim that the emitter is proven — it is forced
// by the OTHER switch. `string_intrinsic_pin_enabled` (default ON) pins a
// method containing a String-intrinsic site to the single-pass backend, so
// with the pin in place nothing here is ever reached and a default-OFF
// emitter would be dead twice over. The A/B that means anything is therefore
//
//     CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1                        (emitter on)
//   vs
//     CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1 CRATONVM_JIT_IR_STRING_INTRINSICS=0
//
// with the default (pinned) arm as the third reading.
//
// That is only true as of 2026-09-01, and it was NOT true when this emitter
// landed. The pin is one of THREE independent refusals of the same method,
// and lifting one of three is indistinguishable from lifting none — the
// failure `jit/src/compile_gate.rs` exists for. The second refusal is
// `is_intrinsic_site` in `try_compile_inner`'s invoke-planning loop
// (`jit/src/lib.rs`), which carried a bare unconditional
// `|| cn == "java/lang/String"`: any method with any `java/lang/String`
// invoke set `all_emittable = false`, the `else` arm left `invoke_info`
// unset for the WHOLE method, and `IrBuilder::build`'s `0xb6` arm returns
// `bail_invoke` on the missing entry BEFORE the line that offers the site to
// `try_string_access_intrinsic`. With that gate in place the first arm above
// could not enter this file by any input; reaching it additionally required
// `CRATONVM_JIT_IR_OVER_INTRINSIC=1`, which disables the gate for every
// intrinsic family at once and so prices a different program.
//
// `lib.rs::ir_string_access_expander_handles` now carves exactly the three
// ACCESSORS on an UNGUARDED (`java/lang/String`) receiver out of that gate —
// asking `try_resolve_string_intrinsic`, the same function this file calls,
// rather than matching a class name — so the two-arm A/B above is reachable
// as written. Everything else declared on `java/lang/String` still bails,
// which is correct: `hashCode`, `equals`, `compareTo` and both `indexOf`
// forms are emitted by the single-pass region and refused here
// (`SI_NOT_ACCESSOR`), so a method containing one must keep the tier that
// can emit it. `CRATONVM_JIT_NO_IR_STRING_ACCESS_ADMIT=1` restores the
// blanket refusal, i.e. reproduces the unreachable state deliberately, and
// is the fourth reading.
//
// Read `ir_string_intrinsic_census_line()` on every arm: a zero `emitted_*`
// is what tells "the emitter did not help" apart from "the emitter never
// ran", and this tier has been burned by an instrument armed where it cannot
// fire often enough that the distinction is worth a counter of its own
// (`no_layout`). `sites_seen` is the one to read FIRST — a zero there on a
// pin-off arm means the method never reached this builder at all, which is
// exactly the third-door signature above and is not a fact about this
// emitter. Once this is measured, the pin, `CRATONVM_JIT_IR_OVER_INTRINSIC`
// and `ir_string_access_expander_handles` are all retirable together — the
// flags' own doc comments stage exactly that.

/// The `java/lang/String` field layout this tier expands String accessors
/// against, published once per process.
///
/// The layout is produced per compile by `lib.rs`'s `string_layout_resolver`
/// and handed to the single-pass backend as `Compiler::string_layout`; the IR
/// builder is constructed in the same function and has never been given it.
/// Rather than thread a new argument through every construction site (the
/// method-entry door, the OSR door, the eager first-call door and ~30 test
/// builders), the resolved layout is published ONCE and every later
/// [`IrBuilder::new`] seeds itself from it. [`IrBuilder::set_string_layout`]
/// overrides it for a caller that has one in hand.
///
/// Publishing once is sound for the four fields this tier reads and for no
/// others. `value_field_index`, `coder_field_index`, `has_coder` and
/// `string_class_id` are properties of the LOADED `java/lang/String` class,
/// fixed for the life of the process once String is loaded. The byte offsets
/// beside them are not: `StringFieldLayout::new`'s compact arm falls back to
/// the LEGACY address when no `CompactLayout` has been registered for
/// `string_class_id` yet, so a layout published early can carry offsets a
/// later one would not. Nothing here reads an offset — see the section header
/// for why that is also the narrow-oop answer.
static PUBLISHED_STRING_LAYOUT: std::sync::OnceLock<crate::StringFieldLayout> =
    std::sync::OnceLock::new();

/// The `invokevirtual` pcs at which [`IrBuilder::try_string_access_intrinsic`]
/// expanded a String accessor during the CURRENT build, on this thread.
///
/// # Why a thread-local
///
/// `IrBuilder::build` takes `self` BY VALUE, so a caller cannot read a field
/// back off the builder once it has run, and [`Graph`] is built by struct
/// literal in a dozen tests, so a new field there is a wide mechanical change
/// for a side table exactly one caller wants. `reset` happens at the top of
/// `build` and the read happens in `lib.rs` immediately after `build` returns,
/// on the same thread — the only window in which the list means anything.
///
/// # What the reader does with it
///
/// The two `Op::Load`s this expansion emits sit at an INVOKE pc, and
/// `ir_lower`'s inline-`getfield` fast path is keyed by
/// `(getfield pc, is_reference)`. With no row there both loads fall back to the
/// checked `jit_getfield` helper — one CALL per character. Measured on
/// `probes/CharAtCostCurve.java`, arm B, 2026-09-02: **917,203,334** helper
/// calls in one run, with `IR-tier inline-getfield refusals:
/// no-compact-slot-for-pc=8` naming the reason. These pcs are what lets
/// `lib.rs` install the two rows.
thread_local! {
    static STRING_ACCESS_SITE_PCS: std::cell::RefCell<Vec<usize>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Clear the per-build String-accessor site list.
fn reset_string_access_sites() {
    STRING_ACCESS_SITE_PCS.with(|v| v.borrow_mut().clear());
}

/// Record one expanded String-accessor site.
fn note_string_access_site(pc: usize) {
    STRING_ACCESS_SITE_PCS.with(|v| v.borrow_mut().push(pc));
}

/// The pcs at which this thread's most recent [`IrBuilder::build`] expanded a
/// String accessor. Empty when the emitter did not fire.
pub fn string_access_site_pcs() -> Vec<usize> {
    STRING_ACCESS_SITE_PCS.with(|v| v.borrow().clone())
}

/// Publish the resolved `java/lang/String` layout for the optimizing tier.
///
/// The first call wins and later calls are ignored; see
/// [`PUBLISHED_STRING_LAYOUT`] for why that is sound for the fields this tier
/// actually reads. Until something calls this, every String call site in the
/// optimizing tier keeps its dispatch and `no_layout` in
/// [`ir_string_intrinsic_census`] counts each one — which is what makes "the
/// emitter is unwired" readable rather than indistinguishable from "the
/// emitter found nothing".
pub fn publish_string_layout(layout: crate::StringFieldLayout) {
    // The first publish changes what the String expander can do for every
    // method, so a refusal the acceptance gate recorded before it judged
    // inputs that no longer hold (round 9 wave 2,
    // `ir-refusal-memo-outlives-the-inputs-it-judged`).
    if PUBLISHED_STRING_LAYOUT.set(layout).is_ok() {
        crate::ir_evidence::bump_inputs_generation();
    }
}

/// The layout published by [`publish_string_layout`], if any.
pub fn published_string_layout() -> Option<crate::StringFieldLayout> {
    PUBLISHED_STRING_LAYOUT.get().copied()
}

/// Outcome names for the String-intrinsic census, in variant order — the
/// `SI_*` indices below index both this table and
/// [`STRING_INTRINSIC_COUNTS`], so the two cannot drift apart.
const STRING_INTRINSIC_OUTCOMES: [&str; 10] = [
    // A `java/lang/String` or `java/lang/CharSequence` invoke reached the
    // expander at all. Every other row is a partition of this one.
    "sites_seen",
    "flag_off",
    // Nobody called `publish_string_layout` — the emitter is armed where it
    // cannot fire. This row exists so that reading is not a silent zero.
    "no_layout",
    "not_an_accessor",
    // A CharSequence-declared site: correct only behind a header class-id
    // compare this tier has no node for. See the section header.
    "guarded_site_refused",
    "in_splice_refused",
    "shape_refused",
    "emitted_length",
    "emitted_isEmpty",
    "emitted_charAt",
];

const SI_SITES_SEEN: usize = 0;
const SI_FLAG_OFF: usize = 1;
const SI_NO_LAYOUT: usize = 2;
const SI_NOT_ACCESSOR: usize = 3;
const SI_GUARDED_REFUSED: usize = 4;
const SI_IN_SPLICE: usize = 5;
const SI_SHAPE_REFUSED: usize = 6;
const SI_EMITTED_LENGTH: usize = 7;
const SI_EMITTED_IS_EMPTY: usize = 8;
const SI_EMITTED_CHAR_AT: usize = 9;

/// One relaxed counter per [`STRING_INTRINSIC_OUTCOMES`] entry.
static STRING_INTRINSIC_COUNTS: [AtomicU64; STRING_INTRINSIC_OUTCOMES.len()] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Count one expander outcome, and — under `CRATONVM_DBG_IR_STRING` — print
/// the running census.
///
/// Printed per DECISION rather than once at exit because nothing in this crate
/// is called on the way out: the exit censuses this VM has
/// (`report_lambda_census_at_exit` and its siblings) are all invoked from
/// `vm-cli/src/main.rs`. The line is cumulative, so `tail -1` of the run's
/// `[ir-string]` lines IS the exit census, and the flag is cached so a
/// default run pays one relaxed increment and nothing else. Wiring
/// [`ir_string_intrinsic_census_line`] into that exit block is a one-line
/// follow-up in a file this change may not touch.
fn note_string_intrinsic(idx: usize) {
    STRING_INTRINSIC_COUNTS[idx].fetch_add(1, Ordering::Relaxed);
    if string_intrinsic_reporting() {
        eprintln!("[ir-string] {}", ir_string_intrinsic_census_line());
    }
}

/// `(outcome name, count)` for every row of the String-intrinsic census.
///
/// "The emitter exists" and "the emitter ran" are different facts and this is
/// what separates them. `emitted_charAt` is the ENGAGEMENT number for the
/// whole lane; `no_layout`, `guarded_site_refused` and `flag_off` each name a
/// distinct way for it to be zero while the code is present and correct.
pub fn ir_string_intrinsic_census() -> Vec<(&'static str, u64)> {
    STRING_INTRINSIC_OUTCOMES
        .iter()
        .zip(STRING_INTRINSIC_COUNTS.iter())
        .map(|(name, count)| (*name, count.load(Ordering::Relaxed)))
        .collect()
}

/// [`ir_string_intrinsic_census`] as one line, for a log or an exit report.
pub fn ir_string_intrinsic_census_line() -> String {
    let mut out = String::from("ir string intrinsics:");
    for (name, count) in ir_string_intrinsic_census() {
        out.push_str(&format!(" {name}={count}"));
    }
    out
}

/// Kill switch for the String access expander. Default ON — see the section
/// header for why a default-OFF switch would be dead by construction while
/// `string_intrinsic_pin_enabled` is also on, and for the A/B that needs
/// both.
fn ir_string_intrinsics_enabled() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        match cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_IR_STRING_INTRINSICS") {
            Some(value) => !matches!(
                value.to_string_lossy().trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no" | ""
            ),
            None => true,
        }
    })
}

/// `CRATONVM_DBG_IR_STRING` — print the running census on every expander
/// decision. Cached: an uncached flag read on a per-call-site path is how
/// `CRATONVM_DBG_COMPACT_INLINE` came to be 99.1% of all flag reads on a
/// `BigDecimal` run.
fn string_intrinsic_reporting() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_IR_STRING"))
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
    /// Abstract monitor stack, outermost first: the reference each executed
    /// `monitorenter` locked, popped by its `monitorexit`. Copied into every
    /// [`SafepointSnapshot::monitors`], which is how a lowered deopt frame
    /// learns which locks the interpreter must hold when it resumes.
    monitors: Vec<NodeId>,
    /// Set when the monitor stacks on two edges into one join disagree. Such a
    /// method has no single monitor state at the join to describe, so the build
    /// is refused (the single-pass backend still compiles it). Checked at the
    /// top of every instruction and once more at the end of [`Self::build`].
    unstructured_monitors: bool,
    /// A refusal latched by a helper that has no way to return one, as
    /// `(ir.rs line, bytecode pc)` — the same pair [`ir_build_bail`] takes, so
    /// the refusal lands in the per-compilation bail census like every other
    /// one instead of being invisible.
    ///
    /// Every arm of [`Self::build`] refuses with `return ir_build_bail(line!(),
    /// pc)`, but the merge-bookkeeping helpers ([`Self::activate_merge`],
    /// [`Self::activate_loop_header`], [`Self::patch_loop_backedge`],
    /// [`Self::retype_phi`]) are called for effect and return `()`. Before
    /// 2026-09-17 each of them spelled "I cannot record this" as a bare
    /// `return` — indistinguishable from "nothing to do" — which is how a
    /// dropped control edge (see `patch_loop_backedge`) could produce a merge
    /// with fewer predecessors than the CFG has and still verify clean.
    ///
    /// Checked at the top of every walk iteration, next to
    /// [`Self::unstructured_monitors`], and once more before [`Self::build`]
    /// hands the graph back — the second check is what catches a refusal
    /// latched on a path that then stops walking (a `goto` into dead control,
    /// or the very last instruction). First refusal wins; see [`Self::refuse`].
    build_refused: Option<(u32, usize)>,
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
    /// `getfield` / `putfield` pcs whose field is declared `volatile`. Set by
    /// [`Self::set_volatile_field_pcs`]; `build` refuses a field access at any
    /// of them unless the matching admission bit below is set, in which case
    /// it builds the access and marks it [`Node::volatile_access`].
    volatile_field_pcs: HashSet<usize>,
    /// Build a volatile `getfield` as an ordered `Op::Load` instead of
    /// refusing (round 9 wave 6). Set by [`Self::set_volatile_field_pcs`] from
    /// [`ir_volatile_fields_enabled`].
    volatile_loads_admitted: bool,
    /// Build a volatile `putfield` as an ordered `Op::Store` instead of
    /// refusing. Additionally needs [`IR_LOWER_FENCES_VOLATILE_STORES`].
    volatile_stores_admitted: bool,
    /// Resolved allocation layout for `new`, keyed by bytecode pc:
    /// `pc → (class_id, num_fields)`. Set by [`Self::set_new_info`]; only the
    /// allocations the caller admits (non-escaping-eligible: no primitive field
    /// initialisers, no finalizer) are present. A `new` whose pc is absent
    /// makes `build` bail (`None` → single-pass).
    new_info: HashMap<usize, (u32, usize)>,
    /// cov-06: resolved allocation layout for `anewarray`, keyed by bytecode
    /// pc: `pc → component_class_id`. Set by [`Self::set_anewarray_info`];
    /// only an already-loaded component class is present — a `Deferred` site
    /// (target class not loaded yet) is omitted, exactly like an unresolved
    /// `new`. An `anewarray` whose pc is absent makes `build` bail (`None` →
    /// single-pass).
    anewarray_info: HashMap<usize, u32>,
    /// Bytecode pcs of `invokespecial` calls to a trivial no-arg void
    /// constructor (`<init>()V`) that may be **elided** (the receiver is a
    /// fresh, non-escaping object whose fields are zero-initialised and set by
    /// the visible `putfield`s). Set by [`Self::set_trivial_init_pcs`]. An
    /// `invokespecial` whose pc is NOT here makes `build` bail.
    trivial_init_pcs: HashSet<usize>,
    /// Bytecode pcs of `invokespecial java/lang/Object.<init>()V` — the
    /// terminal of every constructor chain — which may be elided on **any**
    /// receiver, unlike [`Self::trivial_init_pcs`].
    ///
    /// The single-pass backend has always done this (`x64::bytecode_walk`'s
    /// `0xb7` arm) on the grounds that the body is a bare `return` and the
    /// VM-side registration is `native_noop_with_this`, so the call has no
    /// observable effect at all. The IR builder had no equivalent, and it
    /// cannot borrow `trivial_init_pcs` for the job for two independent
    /// reasons:
    ///
    ///  * `is_elidable_construction` REFUSES `java/lang/Object` — a registered
    ///    native shadows its bytecode, which is the exact hazard that check
    ///    exists for (`HashMap.<init>()V`) — so the pc never enters that set;
    ///  * `trivial_init_pcs` is only supplied when the method contains a
    ///    `new`, and the site that matters here is the `super()` call inside a
    ///    **constructor**, which contains none.
    ///
    /// Measured: every `new F(i)` in an optimizing-tier compile paid TWO
    /// `jit_invoke_dispatch` round trips — one for `F.<init>` and one for the
    /// `Object.<init>` inside it — at ~784 cycles each.
    object_init_pcs: HashSet<usize>,
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
    /// Call sites the invoke planner recognised as a scalar-intrinsic family
    /// and deliberately left OUT of `invoke_info`, keyed by caller pc.
    ///
    /// A pc in here is not a missing plan -- it is a plan to emit arithmetic.
    /// The distinction matters because every invoke arm's `None` from
    /// `invoke_info` means "refuse the method", and without this map a site the
    /// planner handled on purpose would be indistinguishable from one it could
    /// not handle at all.
    scalar_intrinsics: HashMap<usize, ScalarOp>,
    unbox_intrinsics: HashMap<usize, (UnboxOp, u32)>,
    /// `invokedynamic` sites this tier replaces with an uncommon trap, keyed by
    /// caller pc: `(operand-stack entries the call consumes, return type tag)`.
    ///
    /// The stack effect is the ONLY thing needed, because the site emits no
    /// call -- but it is needed exactly, or every bytecode after it reads the
    /// wrong entries. The counts come from `count_param_slots`, which counts
    /// one entry per ARGUMENT (a `long` is one), matching this builder's
    /// one-entry-per-value abstract stack; `compute_param_jvm_slots` counts
    /// two for a `long` and is the wrong function here.
    indy_trap_sites: HashMap<usize, (usize, u8)>,
    /// IR-tier inlining: callee bodies to splice, keyed by the CALLER pc of the
    /// `invoke` each replaces. Empty on every compile that inlines nothing,
    /// which is every compile with the gate off. See [`IrInlineSite`].
    inline_sites: HashMap<usize, IrInlineSite>,
    /// The splices currently open, innermost last. Non-empty exactly while the
    /// walk is inside a relocated callee body, which is what suppresses merge
    /// activation, the reachability skip and safepoint recording.
    splice: Vec<SpliceFrame>,
    /// `IrInlineSite::base` of every admitted body with more than one reachable
    /// `return`. Filled by the pre-scan in [`Self::build`] and read once, by
    /// [`Self::begin_splice`]. Empty unless `CRATONVM_JIT_IR_SPLICE_MULTI_RETURN=1`
    /// — and without it the scanner has admitted no such body either.
    splice_multi_return: HashSet<usize>,
    /// `IrInlineSite::base` → the block-walk ranges, rebased to combined-buffer
    /// pcs, of every admitted body with a rotated loop. Filled by the pre-scan
    /// in [`Self::build`] and read by [`Self::begin_splice`].
    splice_block_walks: HashMap<usize, Vec<(usize, usize)>>,
    /// Diagnostic-only: how many splices this build performed, for the
    /// `[ir] spliced` line. Never read by lowering.
    splices_done: usize,
    /// References the OUTERMOST open splice allocated itself, plus everything
    /// reachable from one by a field or element read.
    ///
    /// A store inside a spliced body has to target one of these — see
    /// [`Self::splice_store_is_local`] for why, and for why a `Load` from a
    /// local base is itself local.
    splice_local: HashSet<NodeId>,
    /// Splice-local objects that have had a NON-local reference stored into
    /// them. Reading a field of one gives back something the caller can still
    /// see, so locality must not propagate through it — see
    /// [`Self::splice_store_is_local`].
    splice_tainted: HashSet<NodeId>,
    /// Set if an `Op::Guard` was ever built inside a splice. Checked at the end
    /// of [`Self::build`], which refuses the graph.
    ///
    /// Belt and braces over the resolver's `ir-splice-division` refusal: the
    /// div-zero guard is the ONLY guard this builder emits, and a guard inside a
    /// spliced region would resolve its frame state from the caller's snapshot
    /// at the `invoke` — a correct re-execution point today, but one that stops
    /// being correct the moment a spliced body is admitted that can commit a
    /// side effect first. A structural check here cannot drift from the
    /// resolver's opcode list the way a comment can.
    splice_guard_seen: bool,
    /// Conditional-branch pcs whose NOT-TAKEN edge the profile never observed,
    /// and which [`Self::prune_always_taken_branch`] may therefore replace with
    /// a guarded unconditional jump.
    ///
    /// Populated by `lib.rs` from the method's branch profile, and empty
    /// whenever there is no profile or the feature is off — in which case every
    /// branch builds the ordinary `Op::If` and codegen is byte-identical.
    pruned_always_taken: std::collections::HashSet<usize>,
    /// How many branches this build actually pruned. Diagnostic; read through
    /// [`Self::branches_pruned`].
    branches_pruned: usize,
    /// The highest target of any branch [`Self::prune_always_taken_branch`]
    /// pruned (0 when none was). Below it, the main walk steps over an
    /// instruction reached with dead control instead of refusing: that is the
    /// pruned COLD ARM, walked as dead code so that a merge inside it which
    /// some OTHER branch targets is still activated (round 9 wave 2,
    /// `ir-branch-prune-skips-the-cold-arm-by-pc`).
    pruned_dead_until: usize,
    /// Diagnostic-only: `pc → "0xNN cn.mn desc"` for every invoke site in the
    /// method. Populated by `lib.rs` **only** when [`ir_bail_reporting`] is on,
    /// and read **only** by [`Self::bail_invoke`]. Never consulted by lowering,
    /// so an absent or stale entry cannot change a compile.
    ///
    /// It exists because the invoke bails below report a line number and a
    /// pc, and the question every one of them raises — *which callee* — was
    /// otherwise a `javap` away for each event. `cov-04`'s whole first
    /// increment was "group the 53 by callee"; this is what makes that a grep.
    invoke_labels: HashMap<usize, Box<str>>,
    /// Diagnostic-only companion to [`Self::invoke_labels`]: the compiling
    /// method's own `cn.mn desc`. Without it the caller has to be recovered by
    /// pairing a bail line with the nearest preceding `[ir] invoke-plan` line,
    /// which is an *assumption about log interleaving* rather than a fact —
    /// exactly the kind of inferred join this directory's rule 6 says not to
    /// trust. Same lifetime and same flag as `invoke_labels`.
    method_label: Option<Box<str>>,
    /// Is every body this compile SPLICED IN free of side effects?
    ///
    /// The second half of the interpreter's replay question (see
    /// `replay_from_entry_is_observably_equivalent`): a spliced artifact's
    /// abandoned attempt also ran part of a RELOCATED callee body, which the
    /// caller's own bytecode does not describe, so the prefix test alone
    /// cannot clear it. `lib.rs` computes this while planning the splice, and
    /// hands it over with [`Self::set_spliced_bodies_pure`]. Vacuously `true`
    /// for a build that splices nothing — the default, and the value every
    /// hand-built/test graph keeps.
    spliced_bodies_pure: bool,
    /// JVMS §6.5 `ireturn` narrowing for THIS method's own return type
    /// ([`crate::narrowed_int_return_tag`]), set by
    /// [`Self::set_return_descriptor`]. `None` — narrow nothing — for `I`, for
    /// every non-int return, and for a hand-built/test builder that never said.
    /// A spliced callee's `ireturn` uses its own frame's tag instead.
    return_narrow: Option<u8>,
    pub tdigest_scalar_kernel: bool,
    /// inc 26/35: resolved `ldc2_w` (0x14) constant values (`pc → (bits, is_double)`).
    /// Set by [`Self::set_ldc2w_info`]; an `ldc2_w` pc not present bails to
    /// single-pass. `is_double` selects the lowering: a `long` constant becomes
    /// `Op::Const(Long)` (`lconst`), a `double` constant `Op::ConstF` (`dconst`).
    ldc2w_info: HashMap<usize, (i64, bool)>,
    /// COV-03: may a `getfield`/`putfield` of a `J` (long) field build?
    ///
    /// A wide field access is the one way a category-2 VALUE can enter the
    /// graph without a category-2 OPCODE appearing in the body — `getfield J;
    /// invokestatic (J)V` contains neither. The admission gate in `lib.rs`
    /// keys on opcodes and the descriptor, so it would admit that method with
    /// the long tier OFF and the builder would then produce `IrType::Long`
    /// nodes the tier does not support. Gate the arm on the same flag the
    /// admission chain uses (`ir_emit_long`) instead of relying on a premise
    /// the arm itself breaks. Default `false` ⇒ a `J` field bails, exactly as
    /// before this lane.
    wide_field_long: bool,
    /// COV-03: may a `getfield`/`putfield` of an `F`/`D` field build? Same
    /// reasoning as [`Self::wide_field_long`], for `ir_emit_fp`.
    wide_field_fp: bool,
    /// cov-01: resolved `ldc` / `ldc_w` (0x12 / 0x13) IMMEDIATE constants
    /// (`pc → (bits, is_float)`). Set by [`Self::set_ldc_info`]; a pc not
    /// present bails to single-pass, which is what every String / Class /
    /// `MethodHandle` / condy site does — those are not immediates and the
    /// builder must not invent one for them.
    ///
    /// `is_float` selects the node exactly as `ldc2w_info`'s `is_double` does:
    /// an `int` constant becomes `Op::Const`/`Int` (`iconst`), a `float`
    /// constant `Op::ConstF`/`Float` (`fconst`). The caller only inserts a
    /// float entry when the FP tier is on.
    ldc_info: HashMap<usize, (i64, bool)>,
    /// cov-01 increment 2: resolved `ldc <String>` sites (`pc → (bytes, len)`),
    /// the ADDRESS and length of the literal's UTF-8, not a reference. Set by
    /// [`Self::set_ldc_string_info`]. The caller owns the bytes for the
    /// artifact's lifetime; see [`Op::ConstString`] for why the object itself
    /// is re-fetched on every execution instead.
    ldc_string_info: HashMap<usize, (u32, u16)>,
    /// cov-01 increment 3: resolved `ldc <Class>` sites
    /// (`pc → (holder_class_id, cp_idx)`). Set by
    /// [`Self::set_ldc_class_info`]; a site whose helper is unwired is simply
    /// absent, so the arm bails. See [`Op::ConstClass`].
    ldc_class_info: HashMap<usize, (u32, u16)>,
    /// Round 9 wave 8 (irl8): `(holder_class_id, cp_idx) -> slot address` for
    /// `ldc <String>` / `ldc <Class>` sites whose resolved constant the VM
    /// keeps in a GC-maintained word (`JitLdcConstant::StringSlot` /
    /// `ClassMirrorSlot`). Set by [`Self::set_ldc_slot_info`]; copied into
    /// the node's `slot_addr`. Keyed by SITE, like the single-pass
    /// `BackendRequest::ldc_slots`. Empty = every site calls its helper.
    ldc_slot_info: HashMap<(u32, u16), usize>,
    /// cov-01 increment 4: resolved `getstatic` (0xb2) sites
    /// (`pc → (class_id, field_index, type_tag, is_volatile)`) — the same tuple
    /// the single-pass backend's `static_field_info` carries. Set by
    /// [`Self::set_static_field_info`]; an unresolvable site is absent and the
    /// arm bails the method. See [`Op::LoadStatic`].
    static_field_info: HashMap<usize, (u32, usize, u8, bool)>,
    /// cov-05 increment 1: resolved `instanceof` (0xc1) sites whose target
    /// class is already loaded (`pc → (name_ptr, name_len)`), the same
    /// (address, length) shape [`Self::ldc_string_info`] carries. Set by
    /// [`Self::set_instanceof_info`]; a pc not present — target not yet
    /// loaded, or no resolver supplied — bails that `instanceof` to
    /// single-pass. See [`Op::InstanceOf`].
    instanceof_info: HashMap<usize, (usize, usize)>,
    /// cov-05: resolved `checkcast` (0xc0) sites whose target class is
    /// already loaded, same `(name_ptr, name_len)` shape as
    /// [`Self::instanceof_info`]. Set by [`Self::set_checkcast_info`]; a pc
    /// not present bails that `checkcast` to single-pass. See
    /// [`Op::CheckCast`].
    checkcast_info: HashMap<usize, (usize, usize)>,
    /// The resolved `java/lang/String` field layout the String access
    /// expander decodes against, or `None` — in which case every String
    /// call site keeps the dispatch it has today. Seeded from
    /// [`published_string_layout`] at construction and overridable with
    /// [`Self::set_string_layout`]; see the section above this struct for
    /// which of its fields are read and why publishing once is sound.
    string_layout: Option<crate::StringFieldLayout>,
    /// `(value, type)` → the lowest-id `Op::Const` node of that value and
    /// type, for every node below [`Self::const_intern_upto`]. What
    /// [`Self::iconst`] and [`Self::aconst_null`] dedupe against, in place of
    /// the arena scan each call used to make. See [`Self::interned_const`] for
    /// how it stays true of a graph that changes under it.
    const_intern: HashMap<(i64, IrType), NodeId>,
    /// How far into `graph.nodes` [`Self::const_intern`] has indexed.
    const_intern_upto: usize,
    /// True while [`Self::build`] walks the caller's bytecode block by block,
    /// in reverse post-order, instead of in pc order. That happens only for a
    /// method with a rotated loop; see [`BlockWalk`]. Read by
    /// [`Self::prune_always_taken_branch`]: its `pc = target` jump is a
    /// pc-order move that the block walk's ranges cannot follow.
    block_walk: bool,
}

thread_local! {
    /// IR site traps planted by the build currently running on this thread.
    ///
    /// Read back by `lib.rs` the moment `build()` returns, the same idiom
    /// `reset_string_access_sites` uses -- `build(mut self, ..)` consumes the
    /// builder, so a getter on it is not available afterwards.
    static SITE_TRAPS_THIS_BUILD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn reset_site_traps_this_build() {
    SITE_TRAPS_THIS_BUILD.with(|c| c.set(0));
}

/// How many IR site traps the build that just returned planted.
pub fn site_traps_planted_this_build() -> usize {
    SITE_TRAPS_THIS_BUILD.with(|c| c.get())
}

/// Where a deopt at a combined-buffer pc of a graph with SPLICED bodies
/// resumes: the pc itself for the compiling method's own code, the OUTERMOST
/// enclosing `invoke` for a pc inside a relocated body.
///
/// That is the rule `ir_lower`'s `resume_bci` applies with `lib.rs`'s spliced
/// ranges (one `(start, end, invoke)` per top-level site, covering its nested
/// bodies). This recovers the same answer from the builder's own
/// [`IrInlineSite`] table by walking each body's caller pc outwards until it
/// lands in the caller's code, so it needs no knowledge of how `lib.rs` laid the
/// bodies out. Published per build ([`splice_resume_map_this_build`]) for
/// `ir_optimize`'s snapshot liveness, which cannot otherwise attribute a node
/// of a spliced body to a snapshot and so gave up on every graph with a splice.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IrSpliceResumeMap {
    /// The compiling method's own code length. Every pc below it is the
    /// caller's; every pc at or past it is relocated callee code.
    pub code_len: usize,
    /// `(base, code_len, caller_pc)` per spliced body: the body occupies
    /// `[base, base + code_len)` and replaced the `invoke` at `caller_pc`,
    /// which is itself a combined-buffer pc (inside the enclosing body, for a
    /// nested splice).
    pub bodies: Vec<(usize, usize, usize)>,
}

impl IrSpliceResumeMap {
    /// The map for a build over `inline_sites` (keyed by caller pc) whose
    /// compiling method is `code_len` bytes long. Bodies are sorted, so two
    /// builds of the same plan publish equal maps.
    pub fn from_sites(code_len: usize, inline_sites: &HashMap<usize, IrInlineSite>) -> Self {
        let mut bodies: Vec<(usize, usize, usize)> = inline_sites
            .iter()
            .map(|(&caller, s)| (s.base, s.code_len, caller))
            .collect();
        bodies.sort_unstable();
        IrSpliceResumeMap { code_len, bodies }
    }

    /// The bci a deopt at combined pc `pc` resumes at. `None` when a pc at or
    /// past [`Self::code_len`] lies in no recorded body, or the caller chain
    /// does not reach the caller's code within `bodies.len()` steps (a
    /// malformed table): the caller must then treat the pc as unattributable.
    pub fn resume_bci(&self, pc: usize) -> Option<usize> {
        let mut at = pc;
        for _ in 0..=self.bodies.len() {
            if at < self.code_len {
                return Some(at);
            }
            let &(_, _, caller) = self
                .bodies
                .iter()
                .find(|&&(base, len, _)| at >= base && at < base.saturating_add(len))?;
            at = caller;
        }
        None
    }
}

thread_local! {
    /// The [`IrSpliceResumeMap`] of the build that last ran on this thread,
    /// `None` when it spliced nothing. Reset at the start of every build and by
    /// [`IrBuildResultsScope`], like the other per-build channels.
    static SPLICE_RESUME_THIS_BUILD: std::cell::RefCell<Option<IrSpliceResumeMap>> =
        const { std::cell::RefCell::new(None) };
}

fn set_splice_resume_map_this_build(map: Option<IrSpliceResumeMap>) {
    SPLICE_RESUME_THIS_BUILD.with(|c| *c.borrow_mut() = map);
}

/// The splice resume map of the build that last ran on this thread, if it
/// spliced anything. Read by `ir_optimize::optimize` at its entry, which in the
/// compile pipeline runs on the graph that build just produced.
pub fn splice_resume_map_this_build() -> Option<IrSpliceResumeMap> {
    SPLICE_RESUME_THIS_BUILD.with(|c| c.borrow().clone())
}

/// Clears the IR builder's per-build result channels -- the planted
/// site-trap count, the String-access site list and the splice resume map --
/// when it is created and again when it is dropped. The compile driver holds
/// one across a build and reads the results through it, so a build that
/// bails, returns early or unwinds cannot leave its results for the next build
/// on this thread.
pub struct IrBuildResultsScope(());

impl IrBuildResultsScope {
    /// Open the scope, clearing every channel.
    pub fn enter() -> Self {
        reset_string_access_sites();
        reset_site_traps_this_build();
        set_splice_resume_map_this_build(None);
        IrBuildResultsScope(())
    }

    /// How many site traps the build inside this scope planted.
    pub fn site_traps_planted(&self) -> usize {
        site_traps_planted_this_build()
    }

    /// The pcs at which the build inside this scope expanded a String accessor.
    pub fn string_access_site_pcs(&self) -> Vec<usize> {
        string_access_site_pcs()
    }
}

impl Drop for IrBuildResultsScope {
    fn drop(&mut self) {
        reset_string_access_sites();
        reset_site_traps_this_build();
        set_splice_resume_map_this_build(None);
    }
}

/// Methods whose IR body carries at least one planted site trap, by
/// `compute_jit_key_hash(class, method, desc, ClassId::new(0))`.
///
/// The runtime consults this when a trapped callee resumes, to tell an IR SITE
/// TRAP apart from genuinely unreachable code. The two want opposite actions:
/// unreachable code should blacklist the method, whereas a site trap means only
/// that the OPTIMIZING tier could not lower one call site -- the single-pass
/// backend lowers `invokedynamic` and unresolved typechecks perfectly well, and
/// blacklisting takes that body away too.
fn trapped_methods() -> &'static std::sync::RwLock<rustc_hash::FxHashSet<u64>> {
    static M: std::sync::OnceLock<std::sync::RwLock<rustc_hash::FxHashSet<u64>>> =
        std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::RwLock::new(rustc_hash::FxHashSet::default()))
}

/// Cap, for the same reason the refusal memo has one: a pathological program
/// must not turn a diagnostic into unbounded retained memory.
const MAX_TRAPPED_METHOD_MEMOS: usize = 8192;

/// Set once any method is registered, so the common case -- a VM that has
/// planted no site trap at all -- costs one relaxed load instead of a hash and
/// a lock acquisition.
///
/// `try_resume_trapped_callee` runs on EVERY deopt resume, not only trapped
/// ones, so the lookup it performs is on a path that has nothing to do with
/// site traps for most workloads.
static ANY_SITE_TRAP_REGISTERED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Has any method in this process been registered as carrying a site trap?
///
/// A `false` here is authoritative: the flag is set BEFORE the set insert, and
/// only ever goes false -> true, so a reader that sees `false` cannot be racing
/// a registration whose method it is about to be asked about -- the registering
/// compile has not published its artifact yet.
pub fn any_site_trap_registered() -> bool {
    ANY_SITE_TRAP_REGISTERED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Record that `hash`'s IR body carries a planted site trap.
pub fn register_site_trap_method(hash: u64) {
    ANY_SITE_TRAP_REGISTERED.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut set = trapped_methods().write().unwrap();
    if set.len() < MAX_TRAPPED_METHOD_MEMOS {
        set.insert(hash);
    }
}

/// Does this method's compiled IR body carry a planted site trap?
pub fn method_has_site_trap(hash: u64) -> bool {
    trapped_methods().read().unwrap().contains(&hash)
}

/// Site traps actually TAKEN at runtime.
///
/// `ir_trap_census` counts what was planted; this is the other half the
/// planting doc promised and did not have. A cause whose taken count is not
/// ~0 has had its coldness argument refuted -- which is exactly what happened
/// to the unresolved-class cause, and the number that says so.
static SITE_TRAPS_TAKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Count one site trap taken at runtime.
pub fn note_site_trap_taken() {
    SITE_TRAPS_TAKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Methods whose site-trap policy has been applied, by memo hash.
///
/// A DEDICATED set, not the IR refusal memo. The first version of this reused
/// `ir_evidence::method_already_refused` as the "already decided" flag, which
/// is wrong because that memo has a SECOND writer: the acceptance gate marks a
/// method refused whenever it discards an optimizing body. A method that got a
/// trapping IR body, was later recompiled, and had THAT recompile refused by
/// the gate would then look "already decided" the first time its old artifact
/// trapped -- so the policy would never be applied, the artifact never evicted,
/// and the trap would fire forever with nothing recorded.
///
/// Two writers, two meanings, two sets.
fn site_trap_decided_methods() -> &'static std::sync::RwLock<rustc_hash::FxHashSet<u64>> {
    static M: std::sync::OnceLock<std::sync::RwLock<rustc_hash::FxHashSet<u64>>> =
        std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::RwLock::new(rustc_hash::FxHashSet::default()))
}

/// Claim the site-trap policy decision for `hash`.
///
/// `true` exactly once per method: the caller that gets it must apply the
/// policy, and every later trap on that method is a repeat. Insert-and-test
/// under one write lock so two threads trapping the same method concurrently
/// cannot both decide.
pub fn claim_site_trap_decision(hash: u64) -> bool {
    let mut set = site_trap_decided_methods().write().unwrap();
    if set.len() >= MAX_TRAPPED_METHOD_MEMOS {
        // Cap reached: decide every time rather than never. Re-deciding is a
        // recompile request; NOT deciding leaves a trapping artifact installed.
        return true;
    }
    set.insert(hash)
}

/// Site traps taken AFTER the policy for that method was already decided.
///
/// These cost an interpreter resume each and buy nothing: the method is already
/// IR-banned and recompiled. A large number means a trapped site sits inside a
/// long-running caller whose in-flight frame still holds a baked CALL to the
/// old body -- see `try_resume_trapped_callee`.
static SITE_TRAP_REPEATS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Count one site trap taken after the decision was already made.
pub fn note_site_trap_repeat() {
    SITE_TRAP_REPEATS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Site traps that re-fired after their method's policy was already applied.
pub fn site_trap_repeats() -> u64 {
    SITE_TRAP_REPEATS.load(std::sync::atomic::Ordering::Relaxed)
}

/// How many planted site traps have actually fired.
pub fn site_traps_taken() -> u64 {
    SITE_TRAPS_TAKEN.load(std::sync::atomic::Ordering::Relaxed)
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
            uses: UseLists::new(),
            receiver_param: None,
        };
        // The builder appends: every edge it writes goes through the tracked
        // mutators, so the def-use edges are maintained from an empty graph
        // rather than derived by a scan once optimization starts.
        graph.rebuild_use_lists();

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
            monitors: Vec::new(),
            unstructured_monitors: false,
            build_refused: None,
            _num_locals: num_locals,
            merges: HashMap::new(),
            loop_headers: HashSet::new(),
            loop_phis: HashMap::new(),
            field_info: HashMap::new(),
            volatile_field_pcs: HashSet::new(),
            volatile_loads_admitted: false,
            volatile_stores_admitted: false,
            new_info: HashMap::new(),
            anewarray_info: HashMap::new(),
            trivial_init_pcs: HashSet::new(),
            object_init_pcs: HashSet::new(),
            invoke_info: HashMap::new(),
            scalar_intrinsics: HashMap::new(),
            unbox_intrinsics: HashMap::new(),
            indy_trap_sites: HashMap::new(),
            inline_sites: HashMap::new(),
            splice: Vec::new(),
            splice_multi_return: HashSet::new(),
            splice_block_walks: HashMap::new(),
            splices_done: 0,
            splice_local: HashSet::new(),
            splice_tainted: HashSet::new(),
            splice_guard_seen: false,
            pruned_always_taken: std::collections::HashSet::new(),
            branches_pruned: 0,
            pruned_dead_until: 0,
            invoke_labels: HashMap::new(),
            method_label: None,
            spliced_bodies_pure: true,
            return_narrow: None,
            tdigest_scalar_kernel: false,
            ldc2w_info: HashMap::new(),
            wide_field_long: false,
            wide_field_fp: false,
            ldc_info: HashMap::new(),
            ldc_string_info: HashMap::new(),
            ldc_class_info: HashMap::new(),
            ldc_slot_info: HashMap::new(),
            static_field_info: HashMap::new(),
            instanceof_info: HashMap::new(),
            checkcast_info: HashMap::new(),
            string_layout: published_string_layout(),
            const_intern: HashMap::new(),
            const_intern_upto: 0,
            block_walk: false,
        }
    }

    /// inc 26: supply resolved `ldc2_w` long-constant values (`pc → i64`). Must
    /// be called before [`Self::build`]; an `ldc2_w` pc not present bails.
    pub fn set_ldc2w_info(&mut self, info: HashMap<usize, (i64, bool)>) {
        self.ldc2w_info = info;
    }

    /// JVMS §6.5: narrow this method's `ireturn`s to its declared int-category
    /// return type (`Z` as if by `& 1`, `B`/`C`/`S` by truncation and
    /// extension). `descriptor` is the method's own descriptor; a method key
    /// works too (see [`crate::narrowed_int_return_tag`]). Must be called
    /// before [`Self::build`]. A builder that never calls it narrows nothing.
    pub fn set_return_descriptor(&mut self, descriptor: &str) {
        self.return_narrow = crate::narrowed_int_return_tag(descriptor);
    }

    /// COV-03: admit `J` / `F`+`D` instance-field accesses, from the same two
    /// flags the `lib.rs` admission chain evaluates (`ir_emit_long`,
    /// `ir_emit_fp`). Must be called before [`Self::build`]; both default off,
    /// which is the pre-lane behaviour (every wide field bails to single-pass).
    pub fn set_wide_field_gates(&mut self, long: bool, fp: bool) {
        self.wide_field_long = long;
        self.wide_field_fp = fp;
    }

    /// The `IrType` a field of descriptor tag `type_tag` loads/stores as, or
    /// `None` when this builder must refuse the access.
    ///
    /// One place decides it for both the `getfield` (0xb4) and `putfield`
    /// (0xb5) arms. COV-03 exists because those two arms disagreed: `getfield`
    /// learned about reference fields and `putfield`, twenty lines below it,
    /// did not — a fix applied to one of two sites. Sharing the classifier is
    /// what stops that recurring.
    fn field_access_type(&self, type_tag: u8) -> Option<IrType> {
        match type_tag {
            b'I' | b'Z' | b'B' | b'C' | b'S' => Some(IrType::Int),
            b'L' | b'[' => Some(IrType::Ref),
            b'J' if self.wide_field_long => Some(IrType::Long),
            b'F' if self.wide_field_fp => Some(IrType::Float),
            b'D' if self.wide_field_fp => Some(IrType::Double),
            _ => None,
        }
    }

    /// `value` narrowed the way `putfield` narrows it into a field of
    /// descriptor `type_tag`: `B` sign-extends the low 8 bits, `S` the low 16,
    /// `C` zero-extends the low 16, `Z` keeps bit 0 (JVMS §6.5; the
    /// interpreter's `narrow_int_to_field_type`). Every other tag, and a value
    /// that is not an `Int` node, is returned unchanged.
    ///
    /// Built in exactly the shapes the `i2b`/`i2s`/`i2c` arms build —
    /// `(x << 24) >> 24`, `(x << 16) >> 16`, `x & 0xFFFF` — plus `x & 1` for
    /// `Z`, because `Op::I2B`/`I2S`/`I2C` have no lowering (`ir_lower` lists
    /// them `Unlowerable`).
    ///
    /// No node is added when the value is already narrow: the shape javac's
    /// own `i2b`/`i2s`/`i2c` produced, an array element of that width, an
    /// in-range constant, or — for `Z` — a comparison or `instanceof`, whose
    /// result is 0 or 1. So javac-compiled code builds the graph it built
    /// before; only bytecode that stores an unnarrowed int pays the
    /// conversion.
    fn narrow_to_field_type(&mut self, value: NodeId, type_tag: u8, pc: usize) -> NodeId {
        if !matches!(type_tag, b'B' | b'S' | b'C' | b'Z') {
            return value;
        }
        let Some(node) = self.graph.node_opt(value) else {
            return value;
        };
        if node.ty != IrType::Int {
            return value;
        }
        let konst = match node.op {
            Op::Const(c) => Some(c as i32),
            _ => None,
        };
        // `Some(bits)` when `value` is `(x << (32 - bits)) >> (32 - bits)`,
        // i.e. already sign-extended from `bits` bits.
        let sext_from = self.sign_extended_from_bits(value);
        // The constant mask of `value = x & mask`, if it is one.
        let and_mask = match node.op {
            Op::And => {
                node.inputs
                    .iter()
                    .find_map(|&i| match self.graph.node_opt(i).map(|n| &n.op) {
                        Some(Op::Const(m)) => Some(*m as i32),
                        _ => None,
                    })
            }
            _ => None,
        };
        let already_narrow = match type_tag {
            b'B' => {
                sext_from == Some(8)
                    || matches!(node.op, Op::ArrayLoad(MemKind::Byte))
                    || konst.is_some_and(|c| c == i32::from(c as i8))
            }
            b'S' => {
                matches!(sext_from, Some(8) | Some(16))
                    || matches!(
                        node.op,
                        Op::ArrayLoad(MemKind::Short) | Op::ArrayLoad(MemKind::Byte)
                    )
                    || konst.is_some_and(|c| c == i32::from(c as i16))
            }
            b'C' => {
                and_mask.is_some_and(|m| m & !0xFFFF == 0)
                    || matches!(node.op, Op::ArrayLoad(MemKind::Char))
                    || konst.is_some_and(|c| c == i32::from(c as u16))
            }
            _ => {
                and_mask.is_some_and(|m| m & !1 == 0)
                    || matches!(node.op, Op::Cmp(_) | Op::InstanceOf { .. })
                    || konst.is_some_and(|c| c == 0 || c == 1)
            }
        };
        if already_narrow {
            return value;
        }
        match type_tag {
            b'B' | b'S' => {
                let shift = self.iconst(if type_tag == b'B' { 24 } else { 16 });
                let shl = self.add_data(Op::Shl, IrType::Int, vec![value, shift], pc);
                self.add_data(Op::Shr, IrType::Int, vec![shl, shift], pc)
            }
            b'C' => {
                let mask = self.iconst(0xFFFF);
                self.add_data(Op::And, IrType::Int, vec![value, mask], pc)
            }
            _ => {
                let one = self.iconst(1);
                self.add_data(Op::And, IrType::Int, vec![value, one], pc)
            }
        }
    }

    /// `Some(bits)` when `id` is `Shr(Shl(x, k), k)` with the same constant
    /// `k` in `1..32` on both — the shape the `i2b`/`i2s` arms build — i.e.
    /// `x` sign-extended from its low `32 - k` bits.
    fn sign_extended_from_bits(&self, id: NodeId) -> Option<u32> {
        let const_of = |n: NodeId| match self.graph.node_opt(n).map(|n| &n.op) {
            Some(Op::Const(c)) => Some(*c),
            _ => None,
        };
        let shr = self.graph.node_opt(id)?;
        if shr.op != Op::Shr || shr.inputs.len() != 2 {
            return None;
        }
        let shl = self.graph.node_opt(shr.inputs[0])?;
        if shl.op != Op::Shl || shl.inputs.len() != 2 {
            return None;
        }
        let k = const_of(shr.inputs[1])?;
        if const_of(shl.inputs[1])? != k || !(1..32).contains(&k) {
            return None;
        }
        Some(32 - k as u32)
    }

    /// cov-01: supply resolved `ldc` / `ldc_w` IMMEDIATE constants
    /// (`pc → (bits, is_float)`). Must be called before [`Self::build`]; a pc
    /// not present makes that `ldc` bail the method to single-pass.
    pub fn set_ldc_info(&mut self, info: HashMap<usize, (i64, bool)>) {
        self.ldc_info = info;
    }

    /// cov-01 increment 2: supply resolved `ldc <String>` sites
    /// (`pc → (bytes, len)`). The caller must keep the bytes alive for the
    /// artifact's lifetime — the lowered body bakes their address.
    pub fn set_ldc_string_info(&mut self, info: HashMap<usize, (u32, u16)>) {
        self.ldc_string_info = info;
    }

    /// cov-01 increment 3: supply resolved `ldc <Class>` sites
    /// (`pc → (holder_class_id, cp_idx)`). Only sites whose
    /// `helpers.ldc_class_cp` is wired may be supplied.
    pub fn set_ldc_class_info(&mut self, info: HashMap<usize, (u32, u16)>) {
        self.ldc_class_info = info;
    }

    /// Round 9 wave 8 (irl8): supply the GC-maintained constant slots of
    /// resolved `ldc <String>` / `ldc <Class>` sites,
    /// `(holder_class_id, cp_idx) -> slot address` (the `slot_addr` of
    /// `JitLdcConstant::StringSlot` / `ClassMirrorSlot`). A site with no entry,
    /// or an entry of `0`, keeps the plain helper call. Only a site that is
    /// also in [`Self::set_ldc_string_info`] / [`Self::set_ldc_class_info`] is
    /// built at all.
    pub fn set_ldc_slot_info(&mut self, info: HashMap<(u32, u16), usize>) {
        self.ldc_slot_info = info;
    }

    /// cov-01 increment 4: supply resolved `getstatic` sites
    /// (`pc → (class_id, field_index, type_tag, is_volatile)`). Must be called
    /// before [`Self::build`]; an absent pc bails that `getstatic` to
    /// single-pass.
    pub fn set_static_field_info(&mut self, info: HashMap<usize, (u32, usize, u8, bool)>) {
        self.static_field_info = info;
    }

    /// cov-05 increment 1: supply resolved `instanceof` sites whose target
    /// class is already loaded (`pc → (name_ptr, name_len)`). Must be called
    /// before [`Self::build`]; an absent pc bails that `instanceof` to
    /// single-pass — including every not-yet-loaded target, which the caller
    /// must not insert here (see [`Op::InstanceOf`]).
    pub fn set_instanceof_info(&mut self, info: HashMap<usize, (usize, usize)>) {
        self.instanceof_info = info;
    }

    /// cov-05: supply resolved `checkcast` sites whose target class is
    /// already loaded (`pc → (name_ptr, name_len)`). Must be called before
    /// [`Self::build`]; an absent pc bails that `checkcast` to single-pass —
    /// including every not-yet-loaded target, which the caller must not
    /// insert here (see [`Op::CheckCast`]).
    pub fn set_checkcast_info(&mut self, info: HashMap<usize, (usize, usize)>) {
        self.checkcast_info = info;
    }

    /// Supply the resolved `java/lang/String` field layout for the String
    /// access expander. Must be called before [`Self::build`]. `None` — the
    /// state of a builder constructed before anything published a layout —
    /// leaves every String call site dispatching exactly as it does today,
    /// and counts each one under `no_layout` in
    /// [`ir_string_intrinsic_census`] so the absence is readable.
    pub fn set_string_layout(&mut self, layout: Option<crate::StringFieldLayout>) {
        self.string_layout = layout;
    }

    /// Expand a `String.length()` / `isEmpty()` / `charAt(int)` call site into
    /// ordinary IR nodes, or leave the site alone.
    ///
    /// `true` means the site was CONSUMED — operands popped, result pushed —
    /// and the caller must not build an `Op::Call` for it. `false` leaves the
    /// abstract stack exactly as it found it, so the caller's dispatch path is
    /// unaffected; every refusal below returns before the first `pop`.
    ///
    /// The representation, the deopt discipline, the receiver-guard rule and
    /// both switches are all documented in the section above [`IrBuilder`].
    fn try_string_access_intrinsic(&mut self, pc: usize, info_ptr: usize, num_args: usize) -> bool {
        // SAFETY: `info_ptr` addresses a `JitInvokeInfo` the caller (`lib.rs`'s
        // invoke-planning loop) boxed and keeps alive on the artifact's
        // `_jit_invoke_infos` for the life of the compiled method. It is the
        // same pointer this builder is about to bake into an `Op::Call`, and
        // the same read that loop already performs on its own boxes to
        // recognise `java/lang/Object.<init>()V`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        // Screen on the declared class before touching any counter: every
        // other invoke in the program would otherwise show up as a "site".
        if info.class_name != "java/lang/String" && info.class_name != "java/lang/CharSequence" {
            return false;
        }
        note_string_intrinsic(SI_SITES_SEEN);
        if !ir_string_intrinsics_enabled() {
            note_string_intrinsic(SI_FLAG_OFF);
            return false;
        }
        let Some(layout) = self.string_layout else {
            note_string_intrinsic(SI_NO_LAYOUT);
            return false;
        };
        // Which sites are accessors, which need a receiver guard, and which
        // are refused for want of a `coder` field (a legacy `char[]` String)
        // or of a String class id, is `try_resolve_string_intrinsic`'s
        // decision — called, not re-derived, so this tier cannot drift from
        // the one the single-pass backend registers against.
        let Some((entry, num_params, _ret, guard_class_id)) = crate::try_resolve_string_intrinsic(
            info.class_name,
            info.method_name,
            info.descriptor,
            Some(layout),
        ) else {
            note_string_intrinsic(SI_NOT_ACCESSOR);
            return false;
        };
        // Compare against the sentinels rather than `JitIntrinsic::from_entry`:
        // that recovery function only classifies the CRC32 family, and the
        // single-pass region identifies these three the same way.
        let kind = if entry == crate::JitIntrinsic::StringLength.as_entry() {
            SI_EMITTED_LENGTH
        } else if entry == crate::JitIntrinsic::StringIsEmpty.as_entry() {
            SI_EMITTED_IS_EMPTY
        } else if entry == crate::JitIntrinsic::StringCharAt.as_entry() {
            SI_EMITTED_CHAR_AT
        } else {
            // `hashCode`, `equals`, `compareTo` and the two `indexOf` forms
            // resolve through the same function and are deliberately NOT
            // expanded here: each is a loop or a lazy-cache protocol rather
            // than a field decode, so each needs control flow this expansion
            // cannot open (see the section header). They keep their dispatch.
            note_string_intrinsic(SI_NOT_ACCESSOR);
            return false;
        };
        // A guarded (CharSequence-declared) site needs a header class-id
        // compare, and there is no IR node that reads an `ObjectHeader`. See
        // the section header for why `Op::InstanceOf` is not a substitute and
        // why emitting the decode unguarded is a wrong-value bug.
        if guard_class_id != 0 {
            note_string_intrinsic(SI_GUARDED_REFUSED);
            return false;
        }
        // A spliced region records no safepoint snapshot, so a guard built
        // inside one would resolve an EMPTY deopt frame at a combined-buffer
        // pc. Refuse; the site keeps its dispatch inside the splice.
        if !self.splice.is_empty() {
            note_string_intrinsic(SI_IN_SPLICE);
            return false;
        }
        let Some(ctrl) = self.ctrl_opt() else {
            note_string_intrinsic(SI_SHAPE_REFUSED);
            return false;
        };
        // The receiver is one slot and every argument of these three
        // signatures is one slot, so the caller's `num_args` must be exactly
        // `num_params + 1`. A disagreement means the resolved descriptor and
        // the planned site are not describing the same call; refuse rather
        // than pop a depth nothing agreed on.
        if num_args != num_params + 1 || self.stack.len() < num_args {
            note_string_intrinsic(SI_SHAPE_REFUSED);
            return false;
        }

        // ── Past this point the site is consumed. ──
        // Recorded BEFORE the loads are built, so the site is on the list
        // whatever the expansion does after this point — there is no
        // refusal past here, and a list that could disagree with what was
        // emitted would be worse than no list.
        note_string_access_site(pc);
        let index = if kind == SI_EMITTED_CHAR_AT {
            Some(self.pop())
        } else {
            None
        };
        let recv = self.pop();

        // 1. Null receiver -> deopt at this bci. The interpreter re-executes
        //    the `invoke` and the native implementation raises the NPE, with
        //    this method's own handler semantics. `Op::Cmp` widens to a 64-bit
        //    compare because one operand is `Ref`-typed, so a pointer whose
        //    low word happens to be zero is not mistaken for null.
        let null = self.aconst_null();
        let recv_nonnull = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![recv, null], pc);
        let recv_guard = self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, recv_nonnull],
            Some(pc),
        );

        // 2. `coder` first, then `value` — the `Op::Load` helper arm is a
        //    `CALL`, and loading the reference last is the shortest window in
        //    which a live oop sits across one.
        //
        //    Both name `recv_guard` as their trailing guard token (see
        //    [`with_guard_token`]): these two reads are legal only because the
        //    guard above ran, and until the token edge existed that fact lived
        //    in nothing but their position in this function.
        let coder_index = self.iconst(layout.coder_field_index as i64);
        let coder = self.graph.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            with_guard_token(vec![ctrl, self.mem, recv, coder_index], recv_guard),
            Some(pc),
        );
        self.mem = coder;
        let value_index = self.iconst(layout.value_field_index as i64);
        let value = self.graph.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            with_guard_token(vec![ctrl, self.mem, recv, value_index], recv_guard),
            Some(pc),
        );
        self.mem = value;

        // 3. A real String's `value` is never null, but this expansion may not
        //    ASSUME it — the whole point of the deopt discipline is that an
        //    uncertain case leaves for the interpreter rather than reading a
        //    fabricated address. `Op::ArrayLength` and `Op::ArrayLoad` each
        //    emit their own null check as well; the explicit guard states the
        //    intent and keeps the `length` and `charAt` paths identical.
        let value_nonnull = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![value, null], pc);
        let value_guard = self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, value_nonnull],
            Some(pc),
        );

        // 4. char_count = value.length >> coder. `Op::ArrayLength` does not
        //    advance the memory token (nothing writes an array's length), so
        //    it takes the current one as an input only. It DOES name
        //    `value_guard`: the length word it reads is at a fixed offset from
        //    a reference the guard above is what proves non-null.
        let byte_len = self.graph.add(
            Op::ArrayLength,
            IrType::Int,
            with_guard_token(vec![ctrl, self.mem, value], value_guard),
            Some(pc),
        );
        let char_count = self.add_data(Op::UShr, IrType::Int, vec![byte_len, coder], pc);

        let result = if let Some(index) = index {
            // charAt(I)C.
            //
            // Bounds: `0 <= index < char_count`, as two SIGNED compares rather
            // than one unsigned one because `Op::Cmp` has no unsigned form.
            // `char_count` is an `arraylength` shifted right and so is never
            // negative, which makes the pair exactly the unsigned test the
            // single-pass region spells `CMP; JAE`.
            let zero = self.iconst(0);
            let non_negative =
                self.add_data(Op::Cmp(CmpOp::Ge), IrType::Int, vec![index, zero], pc);
            self.graph.add(
                Op::Guard { bci: pc },
                IrType::Void,
                vec![ctrl, non_negative],
                Some(pc),
            );
            let in_bounds =
                self.add_data(Op::Cmp(CmpOp::Lt), IrType::Int, vec![index, char_count], pc);
            let bounds_guard = self.graph.add(
                Op::Guard { bci: pc },
                IrType::Void,
                vec![ctrl, in_bounds],
                Some(pc),
            );

            // Branchless LATIN1/UTF16 decode. See the section header for why
            // the second byte's address is `off + coder` and why both reads
            // are in bounds for every index this guard admits.
            //
            // Both element reads name `bounds_guard` — the LAST of the two
            // bounds guards — as their token. The slot holds one guard, so the
            // model it expresses is "this node may not precede that guard";
            // the `index >= 0` guard in front of it keeps its place through
            // the rule that already ordered every guard, namely that
            // `ir_schedule::priority_sort_block` leaves impure nodes in their
            // relative order. Naming either one is enough for
            // `ir_optimize::LoopGuards`, which intersects against the UNION of
            // every covering guard's cone.
            let off = self.add_data(Op::Shl, IrType::Int, vec![index, coder], pc);
            let lo_raw = self.graph.add(
                Op::ArrayLoad(MemKind::Byte),
                IrType::Int,
                with_guard_token(vec![ctrl, self.mem, value, off], bounds_guard),
                Some(pc),
            );
            self.mem = lo_raw;
            let hi_off = self.add_data(Op::Add, IrType::Int, vec![off, coder], pc);
            let hi_raw = self.graph.add(
                Op::ArrayLoad(MemKind::Byte),
                IrType::Int,
                with_guard_token(vec![ctrl, self.mem, value, hi_off], bounds_guard),
                Some(pc),
            );
            self.mem = hi_raw;
            // `baload` SIGN-extends (JVMS), and a compact String byte is an
            // unsigned half of a code unit — mask both halves before combining
            // or every byte >= 0x80 decodes as a negative char.
            let byte_mask = self.iconst(0xFF);
            let lo = self.add_data(Op::And, IrType::Int, vec![lo_raw, byte_mask], pc);
            let hi = self.add_data(Op::And, IrType::Int, vec![hi_raw, byte_mask], pc);
            let eight = self.iconst(8);
            let hi_shifted = self.add_data(Op::Shl, IrType::Int, vec![hi, eight], pc);
            // `0 - coder` is all-ones for UTF16 and zero for LATIN1, which is
            // what selects between the two decodes without a branch.
            let coder_mask = self.add_data(Op::Sub, IrType::Int, vec![zero, coder], pc);
            let hi_selected = self.add_data(Op::And, IrType::Int, vec![hi_shifted, coder_mask], pc);
            self.add_data(Op::Or, IrType::Int, vec![lo, hi_selected], pc)
        } else if kind == SI_EMITTED_IS_EMPTY {
            let zero = self.iconst(0);
            self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![char_count, zero], pc)
        } else {
            char_count
        };
        self.push(result);
        note_string_intrinsic(kind);
        true
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
    /// Memory/control φs are bookkeeping tokens and are left alone.
    ///
    /// When the widened input list no longer joins, the φ takes
    /// [`PHI_TYPE_FALLBACK`], exactly as a conflict at creation does, and so
    /// does every φ that merges it. It used to KEEP the entry-edge type, which
    /// was not the conservative choice when that type was `Ref`: javac reuses
    /// a slot for a dead `Object` before the loop and an `int`/`long`/`double`
    /// inside it, the header φ stayed `Ref`, and the lowering then published
    /// the slot as a GC root at every safepoint and tagged it `StackSlotRef` in
    /// deopt frames — a raw long or double handed to the collector as an oop.
    /// A conflicting merge is legal only for a slot that is dead at the header
    /// (verified bytecode cannot read it), so the fallback costs nothing.
    ///
    /// # Propagation runs on BOTH outcomes (2026-09-17)
    ///
    /// Until 2026-09-17 the dependant re-typing above lived only in the `Err`
    /// arm: a φ whose type *succeeded* in changing — the exact case this doc
    /// opens with, `Int` → `Ref` once the back edge arrived — updated itself and
    /// stopped. Any φ built between header activation and the back-edge patch
    /// that consumed the loop φ had been typed from the **old** `Int`, was
    /// never revisited, and therefore carried an oop under `IrType::Int`:
    /// excluded from `ir_lower::zero_ref_phi_slots`, excluded from
    /// `emit_safepoint_map`'s `Ref`-typed scan (so the collector neither scans
    /// nor relocates the slot), and compared at 32 bits by `ir_lower`'s
    /// `Op::Cmp` width selection. The successful join is propagated now, which
    /// is what the first paragraph always claimed happened.
    ///
    /// # Worklist, not recursion
    ///
    /// The `Err` arm used to recurse once per φ link, allocating a `Vec` of
    /// dependants at every level. A deep loop nest with many live locals makes
    /// that a stack-depth risk on a background compiler thread, where a
    /// stack overflow is a VM crash rather than a failed compile. The explicit
    /// worklist below has the same reachability and a flat frame.
    ///
    /// Only a node whose type actually **changed** enqueues its dependants, so
    /// a fixpoint is reached as soon as the join stops moving. The step budget
    /// is a backstop for a lattice that someone later makes non-monotone: it
    /// latches [`Self::build_refused`] rather than returning a half-propagated
    /// graph, because a φ whose type does not describe its inputs is precisely
    /// the GC-unsound shape this function exists to prevent.
    fn retype_phi(&mut self, phi: NodeId, pc: usize) {
        // Generous: one visit per φ per type change, and the lattice admits at
        // most a handful of changes per node. Anything past this is a bug in
        // the lattice, not a large method.
        let mut budget = self.graph.nodes.len().saturating_mul(4).saturating_add(64);
        let mut work = vec![phi];
        while let Some(cur) = work.pop() {
            if budget == 0 {
                self.refuse(line!(), pc);
                return;
            }
            budget -= 1;

            let current = match self.graph.node_opt(cur) {
                Some(node) => node.ty,
                None => continue,
            };
            // Memory/control φs are bookkeeping tokens and are left alone.
            if matches!(current, IrType::Memory | IrType::Control) {
                continue;
            }
            let inputs = self.graph.nodes[cur as usize].inputs.clone();
            let joined = match self.graph.phi_data_type_checked(&inputs) {
                Ok(ty) => ty,
                Err(why) => {
                    report_phi_type_fallback(&why);
                    PHI_TYPE_FALLBACK
                }
            };
            if joined == current {
                continue;
            }
            self.graph.nodes[cur as usize].ty = joined;
            // Every φ that merges this one was typed from the OLD type, on
            // either outcome. Re-derive theirs too.
            //
            // Through the def-use lists rather than an arena scan: the scan
            // was O(nodes) per CHANGED φ, and `patch_loop_backedge` retypes
            // every local φ of a loop, so a method with many loops and many
            // live locals paid nodes × φs at build time. `users_of` falls back
            // to exactly that scan when the lists are not current, so the
            // answer is the same either way. A φ's slot 0 is its merge, never
            // another φ, so "any input" and "any value input" agree here.
            for user in self.graph.users_of(cur) {
                if user != cur && self.graph.node_opt(user).is_some_and(|n| n.op == Op::Phi) {
                    work.push(user);
                }
            }
        }
    }

    /// Latch a refusal from a helper that cannot `return ir_build_bail(..)`.
    ///
    /// The FIRST refusal wins: it is the one that describes the state the
    /// builder was still consistent in, and every later one is a consequence of
    /// continuing to walk after it. See [`Self::build_refused`].
    fn refuse(&mut self, site: u32, pc: usize) {
        if self.build_refused.is_none() {
            self.build_refused = Some((site, pc));
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

    /// Supply the `getfield` / `putfield` pcs whose field is `volatile`. Must
    /// be called before [`Self::build`]; a field access at any of them makes
    /// `build` bail, so the method compiles single-pass.
    ///
    /// Refused rather than modelled. A volatile access has no `Op` of its own:
    /// the arms below would emit an ordinary `Op::Load(MemKind)` /
    /// `Op::Store(MemKind)`, whose memory effect is a plain read / write, so
    /// GVN may merge two reads of a volatile flag and LICM may hoist one out of
    /// a loop whose body only reads — `while (!this.stop) {}` then never sees
    /// another thread's write and spins forever — and the lowerer emits no
    /// StoreLoad fence after the store. [`MemEffect::volatile_read`] /
    /// [`MemEffect::volatile_write`] already describe the right ordering, but
    /// `MemKind` is matched in every IR pass and both backends' lowerers, so
    /// carrying the flag through them is a change to many files for a tier
    /// the single-pass backend already covers correctly: it fences every
    /// volatile `putfield` and has no field-load hoist
    /// (`volatile-instance-fields-are-plain-accesses-in-compiled-code`).
    ///
    /// The method's own pcs. A spliced callee's volatile pcs arrive through
    /// [`Self::add_spliced_volatile_field_pcs`] (round 9 wave 10); until the
    /// VM's inline resolver feeds it, that resolver keeps refusing such a
    /// callee in IR mode (`ir-splice-volatile-field`).
    ///
    /// # Modelled, behind a flag (round 9 wave 6)
    ///
    /// `CRATONVM_JIT_IR_VOLATILE_FIELDS` ([`ir_volatile_fields_enabled`],
    /// default ON since round 9 wave 7; `=0` refuses) builds the access instead, as the ordinary `Op::Load` /
    /// `Op::Store` marked [`Node::volatile_access`]. The mark gives the node
    /// [`MemEffect::volatile_read`] / [`MemEffect::volatile_write`], and every
    /// `ir_optimize` pass that moves, merges or deletes memory operations
    /// refuses to do so across or to it (redundant-load elimination, both
    /// dead-store phases, both LICM load arms); the builder's memory-token
    /// chain, which every read and write already advances, keeps the rest in
    /// program order through the scheduler. A volatile `putfield` is admitted
    /// only when [`IR_LOWER_FENCES_VOLATILE_STORES`] also says the lowerer
    /// fences it.
    pub fn set_volatile_field_pcs(&mut self, pcs: HashSet<usize>) {
        // Merged, not replaced (round 9 wave 10, irl10): a spliced callee's
        // volatile pcs may be installed first by
        // [`Self::add_spliced_volatile_field_pcs`], and the two key spaces are
        // disjoint (a spliced pc is a combined-buffer pc at or past
        // `code_len`). A builder is fresh per compile, so for the method's own
        // pcs this is the same as the assignment it replaces.
        self.volatile_field_pcs.extend(pcs);
        let admit = ir_volatile_fields_enabled();
        self.volatile_loads_admitted = admit;
        self.volatile_stores_admitted = admit && IR_LOWER_FENCES_VOLATILE_STORES;
    }

    /// Supply the volatile `getfield` / `putfield` pcs of SPLICED callees, in
    /// combined-buffer coordinates (callee pc + the site's `base`, exactly as
    /// `append_ir_inline_site` rebases the `field_info` rows). Round 9 wave 10
    /// (irl10), `perf-ir-refuses-volatile-instance-field-methods`: until now
    /// the VM's inline resolver refused every callee with a volatile instance
    /// access in IR mode (`ir-splice-volatile-field`), because nothing could
    /// tell the builder which spliced access was volatile -- it would have
    /// built an unordered `Op::Load` that LICM may hoist out of a spin loop.
    /// With these pcs the spliced access takes the same arm as the method's
    /// own: an ordered `Op::Load` / `Op::Store` marked
    /// [`Node::volatile_access`], or a bail when admission is off.
    ///
    /// Additive and order-free with respect to [`Self::set_volatile_field_pcs`]
    /// and [`Self::apply_inline_tables`]. Sets the admission bits from
    /// [`ir_volatile_fields_enabled`] the same way
    /// [`Self::set_volatile_field_pcs`] does, so a compile with no caller-side
    /// volatile resolver still models (or, with the flag off, refuses) the
    /// spliced access instead of bailing on a pc whose admission was never set.
    /// An empty iterator changes nothing.
    pub fn add_spliced_volatile_field_pcs<I: IntoIterator<Item = usize>>(&mut self, pcs: I) {
        let before = self.volatile_field_pcs.len();
        self.volatile_field_pcs.extend(pcs);
        if self.volatile_field_pcs.len() == before {
            return;
        }
        let admit = ir_volatile_fields_enabled();
        self.volatile_loads_admitted = admit;
        self.volatile_stores_admitted = admit && IR_LOWER_FENCES_VOLATILE_STORES;
    }

    /// Test hook: set the two admission bits directly, bypassing the flag and
    /// the lowering contract, so a unit test can build and inspect the graph
    /// either way. Call AFTER [`Self::set_volatile_field_pcs`]. Not
    /// test-gated on purpose: the panic-free ratchet stops scanning a file
    /// at its first test-gate attribute line, and this item sits mid-file.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn set_volatile_field_admission(&mut self, loads: bool, stores: bool) {
        self.volatile_loads_admitted = loads;
        self.volatile_stores_admitted = stores;
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

    /// Supply the `invokespecial java/lang/Object.<init>()V` pcs — see
    /// [`Self::object_init_pcs`]. Independent of [`Self::set_new_info`]: the
    /// site that matters is the `super()` call inside a constructor, and a
    /// constructor contains no `new`.
    pub fn set_object_init_pcs(&mut self, pcs: HashSet<usize>) {
        self.object_init_pcs = pcs;
    }

    /// cov-06: supply the resolved allocation layout (`pc → component_class_id`)
    /// for `anewarray` sites the builder lowers into `Op::NewArray`. Must be
    /// called before [`Self::build`]; an absent `anewarray` pc bails to
    /// single-pass — see [`Self::anewarray_info`].
    pub fn set_anewarray_info(&mut self, info: HashMap<usize, u32>) {
        self.anewarray_info = info;
    }

    /// Record the call sites the planner will lower as an inline unboxing
    /// accessor. Keyed by caller pc, exactly like [`Self::set_invoke_info`],
    /// and deliberately a SEPARATE map: a site here has no `JitInvokeInfo` box
    /// and never will.
    pub fn set_unbox_intrinsics(&mut self, sites: HashMap<usize, (UnboxOp, u32)>) {
        self.unbox_intrinsics = sites;
    }

    /// Record the call sites the planner will lower as arithmetic. Same
    /// contract as [`Self::set_unbox_intrinsics`]; see
    /// [`Self::scalar_intrinsics`].
    pub fn set_scalar_intrinsics(&mut self, sites: HashMap<usize, ScalarOp>) {
        self.scalar_intrinsics = sites;
    }

    /// Record the `invokedynamic` sites this tier may replace with a trap.
    /// See [`IrBuilder::indy_trap_sites`].
    pub fn set_indy_trap_sites(&mut self, sites: HashMap<usize, (usize, u8)>) {
        self.indy_trap_sites = sites;
    }

    /// Emit an unboxing accessor inline. One input (the receiver), and the
    /// node needs `ctrl` and `mem` because it can DEOPT and it reads the heap.
    fn try_emit_unbox_intrinsic(&mut self, pc: usize) -> bool {
        let Some(&(uop, class_id)) = self.unbox_intrinsics.get(&pc) else {
            return false;
        };
        if self.ctrl_opt().is_none() {
            return false;
        }
        // A guard needs a snapshot to resume from, exactly as `plant_uncommon_trap`
        // does: without a safepoint at this bci the deopt would rebuild a frame
        // from nothing. Refusing here leaves the site to the fallback path.
        if !self.graph.safepoints.iter().rev().any(|sp| sp.bci == pc) {
            return false;
        }
        // The receiver is one abstract-stack entry and so is the one argument
        // any of these takes (`getAndAdd(J)J` -- a category-2 value is ONE entry
        // in this builder, see `static_call_shape`). A stack shallower than that
        // means the planned site and the abstract state disagree; refuse rather
        // than pop a depth nothing agreed on. Every refusal above and here
        // returns before the first `pop`, so the caller's dispatch path finds
        // the stack exactly as it left it.
        if self.stack.len() < uop.arity() + 1 {
            return false;
        }
        // Operands come off deepest-last: the delta is shallower, the receiver
        // is deepest, so the delta pops first.
        let delta = if uop.arity() == 1 {
            Some(self.pop())
        } else {
            None
        };
        let obj = self.pop();
        let mut inputs = vec![self.ctrl, self.mem, obj];
        if let Some(d) = delta {
            inputs.push(d);
        }
        let node = self.graph.add(
            Op::Unbox { op: uop, class_id },
            uop.result_type(),
            inputs,
            Some(pc),
        );
        // The node IS the new memory token as well as the result -- the
        // convention `Op::Load` and the String expander use. Three of the six
        // families WRITE the field with a `LOCK XADD`, so dropping this would
        // let a later read of the same object float above the write. It is set
        // for the pure loads too: the cost is ordering a load this tier could
        // otherwise move, and the alternative is a rule with an exception in it.
        self.mem = node;
        self.push(node);
        note_unbox_lowered();
        true
    }

    /// Emit the arithmetic for a scalar-intrinsic call site, if this pc is one.
    ///
    /// Returns `true` when it consumed the site. The operands come off the
    /// abstract stack in reverse order, which is the same convention every
    /// binary arm in this builder uses; the receiver is never popped because
    /// every family in [`ScalarOp`] is declared `static`.
    fn try_emit_scalar_intrinsic(&mut self, pc: usize) -> bool {
        let Some(&sop) = self.scalar_intrinsics.get(&pc) else {
            return false;
        };
        let inputs = if sop.arity() == 2 {
            let b = self.pop();
            let a = self.pop();
            vec![a, b]
        } else {
            vec![self.pop()]
        };
        let node = self.add_data(Op::ScalarIntrinsic(sop), sop.result_type(), inputs, pc);
        self.push(node);
        note_scalar_intrinsic_lowered();
        true
    }

    /// Gap B: supply the resolved call sites (`pc → (info_ptr, num_args,
    /// ret_type)`) the builder lowers into `Op::Call`. Must be called before
    /// [`Self::build`]; an invoke pc not present bails the build to
    /// single-pass. `ret_type` is the JVM return-type byte (see the field doc).
    pub fn set_invoke_info(&mut self, info: HashMap<usize, (usize, usize, u8)>) {
        self.invoke_info = info;
    }

    /// Supply the callee bodies to splice, keyed by the CALLER pc of the
    /// `invoke` each replaces. Must be called before [`Self::build`], and the
    /// `code` handed to `build` must be the COMBINED buffer these sites' `base`
    /// offsets index — a site whose region is not there walks into whatever
    /// follows and bails on the bounds check in the loop.
    ///
    /// A pc present here takes the splice; a pc absent takes whatever lowering
    /// it took before, so an empty map is byte-identical to no inlining.
    pub fn set_inline_sites(&mut self, sites: HashMap<usize, IrInlineSite>) {
        self.inline_sites = sites;
    }

    /// Install a whole inline plan: the callee bodies plus every side-table row
    /// they add. Must be called AFTER the per-table `set_*` setters (which
    /// replace wholesale) and before [`Self::build`].
    ///
    /// Rows are merged, not replaced, and a caller pc can never collide with a
    /// callee pc: callee regions live past the caller's `code_len` by
    /// construction.
    pub fn apply_inline_tables(&mut self, tables: IrInlineTables) {
        let IrInlineTables {
            sites,
            field_info,
            invoke_info,
            new_info,
            object_init_pcs,
            ldc_info,
            ldc2w_info,
            static_field_info,
            checkcast_info,
            instanceof_info,
        } = tables;
        self.inline_sites.extend(sites);
        self.field_info.extend(field_info);
        self.invoke_info.extend(invoke_info);
        self.new_info.extend(new_info);
        self.object_init_pcs.extend(object_init_pcs);
        // Merged, not replaced: the caller's own `ldc` sites are already in
        // these maps (`set_ldc_info` runs before the tables are applied) and a
        // spliced body's pcs are in combined-buffer coordinates, so the two key
        // spaces are disjoint by construction.
        self.ldc_info.extend(ldc_info);
        self.ldc2w_info.extend(ldc2w_info);
        // Same merge and same disjointness argument as the `ldc` rows above:
        // `set_static_field_info` has already installed the caller's own
        // `getstatic` sites, keyed by its own pcs, and a spliced body's pcs are
        // combined-buffer pcs at or past `code_len`.
        self.static_field_info.extend(static_field_info);
        // Merged, not replaced, on the same disjointness argument: the caller's
        // own typecheck sites are already installed under its own pcs, and a
        // spliced body's pcs are combined-buffer pcs at or past `code_len`.
        self.checkcast_info.extend(checkcast_info);
        self.instanceof_info.extend(instanceof_info);
    }

    /// The caller's own `invoke_info` row for a DIRECT self-recursive call
    /// (`JitInvokeInfo::invoke_kind == 4`), if this method has one — the row a
    /// bounded self-recursive splice binds its leaf calls to. See
    /// [`IrSelfRecursion`].
    ///
    /// Only caller pcs (`< code_len`) are looked at, and the lowest such pc is
    /// answered so the choice is deterministic. Every kind-4 row of one method
    /// describes the same call (this method, static, the same descriptor), so
    /// which one is answered does not change the code. Must be asked after
    /// [`Self::set_invoke_info`] and before [`Self::apply_inline_tables`],
    /// i.e. while `invoke_info` holds the caller's rows only.
    pub fn self_recursive_call_row(&self, code_len: usize) -> Option<(usize, usize, u8)> {
        let mut best: Option<(usize, (usize, usize, u8))> = None;
        for (&pc, &row) in &self.invoke_info {
            if pc >= code_len || row.0 == 0 {
                continue;
            }
            // SAFETY: every `invoke_info` row's first field addresses a
            // `JitInvokeInfo` the caller (`lib.rs`'s invoke-planning loop)
            // boxed into `ir_call_infos`, which outlives the compile — the
            // same read `try_string_access_intrinsic` makes on the same rows.
            let info: &JitInvokeInfo = unsafe { &*(row.0 as *const JitInvokeInfo) };
            if info.invoke_kind != 4 {
                continue;
            }
            if best.is_none_or(|(b, _)| pc < b) {
                best = Some((pc, row));
            }
        }
        best.map(|(_, row)| row)
    }

    /// How many splices [`Self::build`] performed. Diagnostic only.
    pub fn splices_done(&self) -> usize {
        self.splices_done
    }

    /// Enter the callee spliced at caller pc `pc`, returning the combined-buffer
    /// pc to continue the walk at.
    ///
    /// `instr_len` is the length of the `invoke` being replaced (3, or 5 for
    /// `invokeinterface`) — the caller resumes at `pc + instr_len`.
    ///
    /// `None` means "refuse this compile". Every refusal here is a disagreement
    /// between the resolved site and the state the walk actually reached (too
    /// deep, not enough operands, an argument slot past the callee's frame), so
    /// there is nothing to fall back to at this point: the operands have been
    /// counted against a body that does not match them.
    fn begin_splice(&mut self, pc: usize, instr_len: usize) -> Option<usize> {
        let site = self.inline_sites.get(&pc)?;
        let base = site.base;
        let end = base.checked_add(site.code_len)?;
        let num_args = site.num_args;
        let max_locals = site.max_locals;
        let returns_value = site.returns_value;
        let receiver_is_arg0 = site.receiver_is_arg0;
        let arg_local_slots = site.arg_local_slots.clone();
        // The CALLEE's return type, not the caller's: an inlined `()Z` body
        // returning 2 must hand its caller 0, exactly as its own compiled
        // body would have.
        let return_narrow = crate::narrowed_int_return_tag(&site.method_key);

        if self.splice.len() >= MAX_IR_SPLICE_DEPTH {
            return None;
        }
        if arg_local_slots.len() != num_args || self.stack.len() < num_args {
            return None;
        }

        // Deepest-first on the abstract stack, so `split_off` already leaves
        // them in source order — argument 0 (the receiver, for an instance
        // callee) first.
        let args = self.stack.split_off(self.stack.len() - num_args);
        let mut callee_locals = vec![NO_NODE; max_locals];
        for (arg, &slot) in args.iter().zip(arg_local_slots.iter()) {
            let slot = slot as usize;
            if slot >= callee_locals.len() {
                return None;
            }
            callee_locals[slot] = *arg;
        }

        // JVMS §6.5, put back where the deleted invoke used to enforce it.
        //
        // The splice replaces `invokespecial`/`invokevirtual` with the callee's
        // own bytecode, and nothing in that bytecode is obliged to touch `this`
        // — `private int small() { return 3; }` does not. So a null receiver ran
        // the body and returned its value, with no exception anywhere:
        // `warm-invokespecial=NO-THROW(3)` on `NullReceiverCachedProbe`, against
        // HotSpot's NPE, and `NO-THROW(440)` for a callee too large for the
        // single-pass inliner but not for this one. The single-pass backend's
        // own direct-call arm has carried this check since `cd451facc`; this
        // tier had it nowhere, so the fix that landed there was invisible on
        // every method the optimizing tier claimed. See
        // `docs/internal/retired/warm-invokespecial-on-a-null-receiver-runs-the-callee-again-FIXED-20260908.md`.
        //
        // An `Op::Guard` rather than a thrown NPE, which is HotSpot's answer
        // too: the deopt reconstructs the frame for THIS bci and the
        // interpreter re-executes the invoke, where the null-receiver guard in
        // `execute_invokevirtual_cached` sends it to the slow path and the
        // canonical NPE (message, JEP 358 action and all) is raised by the code
        // that owns it. Re-executing is sound because the guard fires before a
        // single byte of the callee has run.
        //
        // The snapshot the deopt resolves is the one the main walk pushed at
        // the top of this bci, with the receiver and the arguments still on the
        // operand stack — recorded before this function popped them. Absent it,
        // `resolve_frame_state_for_bci` answers with an EMPTY frame rather than
        // an error, which would park the interpreter at `pc` with no operands;
        // so refuse the compile instead, exactly as `plant_uncommon_trap` does
        // for the same reason.
        // ...UNLESS THE RECEIVER CANNOT BE NULL, and this is not only a saved
        // `TEST`/`JNZ`.
        //
        // `Op::Guard`'s reference inputs are `GlobalEscape` to escape analysis
        // (`ir_op_to_ea_op` funnels `Op::Cmp` into `EaOp::Other`, whose arm
        // republishes every reference operand), so a guard on the receiver of
        // `new Vec3(…).add(…)` would take scalar replacement away from exactly
        // the shape the IR inliner exists to enable -- the `per-voxel-allocation`
        // case, where splicing the accessor chain is what lets the object die
        // where it is used.
        //
        // `definitely_non_null` is `ir_check_elim`'s own predicate rather than a
        // second copy of the list: a successful `Op::New`/`Op::NewArray` returns
        // a non-null reference and a failed one returns the deopt sentinel and
        // never reaches a use. Nothing is given up by skipping the guard for
        // one -- the check could not fire.
        let receiver_needs_null_check = receiver_is_arg0
            && args.first().is_some_and(|&r| {
                self.graph
                    .nodes
                    .get(r as usize)
                    .is_none_or(|n| !crate::ir_check_elim::definitely_non_null(&n.op))
            });
        if receiver_needs_null_check {
            let receiver = *args.first()?;
            let ctrl = self.ctrl_opt()?;
            if self.splice.is_empty() && !self.graph.safepoints.iter().rev().any(|sp| sp.bci == pc)
            {
                return None;
            }
            // A guard inside an OPEN splice resolves its frame state through
            // `resume_bci` to the OUTERMOST invoke, so taking it re-executes
            // that whole call — including any spliced prefix that already ran.
            // `spliced_bodies_pure` is the property that makes that harmless
            // and it is the same clause `trap_replay_is_safe` asks; when it does
            // not hold, raise the fence that refuses the graph, exactly as
            // `add_div_zero_guard` and `plant_uncommon_trap` do.
            if !self.splice.is_empty() && !self.spliced_bodies_pure {
                self.splice_guard_seen = true;
            }
            // `aconst_null`, not `iconst(0)`: `ir_lower`'s `Op::Cmp` selects a
            // 64-bit CMP only when an operand is `Ref`-typed, and a 32-bit one
            // would read a pointer whose low word is zero as null. The
            // `ifnull` arm makes the same choice for the same reason.
            let null = self.aconst_null();
            let cond = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![receiver, null], pc);
            self.graph.add(
                Op::Guard { bci: pc },
                IrType::Void,
                vec![ctrl, cond],
                Some(pc),
            );
        }

        let saved_locals = std::mem::replace(&mut self.locals, callee_locals);
        let saved_stack = std::mem::take(&mut self.stack);
        let multi_return = self.splice_multi_return.contains(&base);
        let ranges = self
            .splice_block_walks
            .get(&base)
            .cloned()
            .unwrap_or_default();
        // `BlockWalk::reverse_postorder` puts the body's pc 0 (`base`) first.
        let range_end = ranges.first().map_or(end, |&(_, e)| e);
        self.splice.push(SpliceFrame {
            return_pc: pc + instr_len,
            end,
            saved_locals,
            saved_stack,
            returns_value,
            return_narrow,
            multi_return: multi_return || !ranges.is_empty(),
            exits: Vec::new(),
            exit_bci: pc,
            ranges,
            next_range: 1,
            range_end,
        });
        self.splices_done += 1;
        // A callee body in the graph is the transform that makes every other
        // pass here worth running -- see `ir_evidence`.
        crate::ir_evidence::note(crate::ir_evidence::Transform::Inlined);
        Some(base)
    }

    /// May a store inside the currently open splice write to `base`?
    ///
    /// A guard failure inside compiled code returns the `i64::MIN` sentinel and
    /// the method is re-run in the interpreter — and on the OSR-exit path it is
    /// resumed precisely, at the bci every node in a spliced region carries: the
    /// CALLER's `invoke`. Resuming there re-executes the whole call, so any
    /// write the spliced prefix already committed is committed a second time.
    ///
    /// The write is harmless when its target is an object the splice itself
    /// allocated: re-execution allocates a fresh one and writes that instead,
    /// and the first is unreachable garbage. So the rule is "stores only to
    /// objects this splice made", and the proof is a reachability set rather
    /// than an alias analysis:
    ///
    ///  * an `Op::New` / `Op::NewArray` built inside the splice is local;
    ///  * a `Load` whose BASE is local is local — a field of an object this
    ///    splice allocated holds either `null` or something this splice stored
    ///    there — UNLESS what the splice stored there was itself non-local, in
    ///    which case the base is TAINTED and locality stops at it. Without that
    ///    second half the rule is wrong in an obvious way: `new W(); w.f =
    ///    callerObject; w.f.x = 1` would read `w.f` back as "local" and admit a
    ///    write straight into the caller's object.
    ///
    /// That second clause is what admits `Short2.setX` → `Short2.set` →
    /// `storage[0] = v`: `storage` is read from a `Short2` the splice allocated
    /// three levels up. It also correctly REFUSES `VolumeShort2.set`, whose
    /// `storage` is read from the receiver the caller passed in.
    ///
    /// `Op::Phi` is not a case here, and that is now a deliberate
    /// conservatism rather than a fact about the input: since the splice
    /// pre-scan registers a body's own merges (`ir_splice_branch_enabled`) and
    /// multi-return bodies join their exits, a spliced region CAN contain a φ.
    /// A φ is never in `splice_local`, so a store through one is refused even
    /// when every input is local — a missed splice, never a wrong one.
    ///
    /// What this does NOT cover is a CALL the spliced body makes: re-executing
    /// the `invoke` re-runs it, and the builder cannot see whether it writes.
    /// See `ir::IrInlineSite`.
    fn splice_store_is_local(&self, base: NodeId) -> bool {
        self.splice_local.contains(&base) && !self.splice_tainted.contains(&base)
    }

    /// Record `id` as splice-local when it is one: called on every allocation
    /// and every field/element read the walk builds, and a no-op outside a
    /// splice.
    fn note_splice_local_alloc(&mut self, id: NodeId) {
        if !self.splice.is_empty() {
            self.splice_local.insert(id);
        }
    }

    /// Record a `Load`-shaped node as local when its base is.
    fn note_splice_local_load(&mut self, id: NodeId, base: NodeId) {
        if !self.splice.is_empty() && self.splice_store_is_local(base) {
            self.splice_local.insert(id);
        }
    }

    /// Record that a non-local value was stored into splice-local `base`, so
    /// locality stops propagating through its reads.
    fn note_splice_store_value(&mut self, base: NodeId, value: NodeId) {
        if !self.splice.is_empty() && !self.splice_local.contains(&value) {
            self.splice_tainted.insert(base);
        }
    }

    /// Leave the innermost splice, restoring the caller's frame and pushing the
    /// callee's result. Returns the caller pc to continue at.
    ///
    /// Nodes built inside the splice keep their COMBINED-buffer pcs; `ir_lower`
    /// translates those to the enclosing `invoke` wherever a bci has to name an
    /// instruction in this method. See the two-bci discussion at
    /// [`IrInlineSite`].
    fn end_splice(&mut self, value: Option<NodeId>) -> Option<usize> {
        let frame = self.splice.pop()?;
        self.locals = frame.saved_locals;
        self.stack = frame.saved_stack;
        if frame.returns_value {
            self.push(value?);
        }
        if self.splice.is_empty() {
            // The sets are scoped to one outermost splice: node ids from a
            // closed region can never be a later region's store target, and
            // keeping them would only grow.
            self.splice_local.clear();
            self.splice_tainted.clear();
        }
        Some(frame.return_pc)
    }

    /// A `return` inside an open splice.
    ///
    /// For a body with one `return` this IS the splice exit and the v1 fast
    /// path stands: restore the caller's frame, push the value, resume after
    /// the `invoke`.
    ///
    /// For a body with several, a `return` is an *edge* rather than an exit —
    /// the walk has to keep going, because the other returns and the code that
    /// reaches them are still ahead of it in the relocated buffer. The edge's
    /// `(ctrl, mem, value)` is parked on the frame, `ctrl` goes dead, and the
    /// walk steps to the next instruction exactly as it does after any
    /// unconditional transfer. Whatever follows is either a branch target —
    /// whose merge the branch itself created, and whose activation restores
    /// `ctrl` — or genuinely unreachable, and the walk's own "reachable code
    /// must have a control token" net refuses the method.
    ///
    /// Returns the pc to continue at, or `None` to refuse the method.
    fn splice_return(&mut self, value: Option<NodeId>, pc: usize) -> Option<usize> {
        if !self.splice.last()?.multi_return {
            return self.end_splice(value);
        }
        let ctrl = self.ctrl_opt()?;
        let mem = self.mem;
        // `NO_NODE` for a `void` callee: `returns_value` decides whether the
        // continuation reads this column at all, so a placeholder here is
        // never a value anything can name.
        let val = if self.splice.last()?.returns_value {
            value?
        } else {
            NO_NODE
        };
        let frame = self.splice.last_mut()?;
        frame.exits.push((ctrl, mem, val));
        frame.exit_bci = pc;
        self.ctrl = NO_NODE;
        Some(pc + 1)
    }

    /// Close a multi-return splice: join its recorded exit edges and hand the
    /// result back to the caller.
    ///
    /// Called when the walk reaches `end` — one past the callee's last
    /// bytecode, which is a `return` by the scanner's own admission rule, so
    /// every edge is already in.
    ///
    /// The join is built by hand rather than through [`Self::ensure_merge`] /
    /// [`Self::activate_merge`] for two reasons. The `merges` map is keyed by
    /// pc, and `end` is the `base` of whatever body `lib.rs` appended next —
    /// so a continuation keyed there would collide with that body's own
    /// merge at its pc 0 (a `while` starting at the top of a method makes one).
    /// And the merge machinery phis the locals and operand stack, which here
    /// are the *callee's*, about to be thrown away: every phi it built would be
    /// dead on arrival.
    ///
    /// What does need a phi is the memory token and the returned value, and
    /// only where the edges disagree.
    fn finish_multi_return_splice(&mut self) -> Option<usize> {
        let frame = self.splice.pop()?;
        if frame.exits.is_empty() {
            return None;
        }
        let bci = frame.exit_bci;

        let ctrls: Vec<NodeId> = frame.exits.iter().map(|&(c, _, _)| c).collect();
        let ctrl = if ctrls.len() == 1 {
            ctrls[0]
        } else {
            // This used to disqualify the artifact from the optimizing OSR
            // door, alongside every other spliced merge (`SPLICED_MERGE_SEEN`).
            // The containment came off on 2026-09-09: the wrong answer was
            // never the merge but `emit_osr_entry_stubs` jumping past the block
            // that writes a constant arm's home word, and the stub seeds those
            // constants now. See `ir_lower`'s `const_seeds` loop.
            MULTI_RETURN_SPLICES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            MULTI_RETURN_EDGES.fetch_add(ctrls.len() as u64, std::sync::atomic::Ordering::Relaxed);
            self.graph.add(Op::Merge, IrType::Control, ctrls, Some(bci))
        };
        self.ctrl = ctrl;

        let mems: Vec<NodeId> = frame.exits.iter().map(|&(_, m, _)| m).collect();
        if mems.iter().any(|&m| m != mems[0]) {
            let mut inputs = vec![ctrl];
            inputs.extend_from_slice(&mems);
            self.mem = self.graph.add(Op::Phi, IrType::Memory, inputs, Some(bci));
        } else {
            self.mem = mems[0];
        }

        self.locals = frame.saved_locals;
        self.stack = frame.saved_stack;
        if frame.returns_value {
            let vals: Vec<NodeId> = frame.exits.iter().map(|&(_, _, v)| v).collect();
            if vals.iter().any(|&v| v == NO_NODE) {
                return None;
            }
            let joined = if vals.iter().any(|&v| v != vals[0]) {
                let mut inputs = vec![ctrl];
                inputs.extend_from_slice(&vals);
                let ty = self.phi_data_type(&inputs);
                self.graph.add(Op::Phi, ty, inputs, Some(bci))
            } else {
                vals[0]
            };
            self.push(joined);
        }

        if self.splice.is_empty() {
            self.splice_local.clear();
            self.splice_tainted.clear();
        }
        Some(frame.return_pc)
    }

    /// Diagnostic-only: supply `pc → "0xNN cn.mn desc"` for the invoke sites, so
    /// the three invoke bails can name the callee they refused. Never read by
    /// lowering; `lib.rs` only calls this when `CRATONVM_DBG=ir-compiles` (or
    /// `jitc`) is on, so the default path never builds the strings.
    pub fn set_invoke_labels(&mut self, labels: HashMap<usize, Box<str>>, method: Box<str>) {
        self.invoke_labels = labels;
        self.method_label = Some(method);
    }

    /// [`ir_build_bail`] for the invoke arms, which — given the diagnostic map
    /// above — know the one thing the bare pc does not: *which callee*.
    ///
    /// Falls back to the plain report when the map is empty (no flag, a
    /// hand-built graph, or a resolver that declined the CP index), so the
    /// `ir.rs:NNNN` line the survey greps for is emitted either way.
    #[cold]
    #[inline(never)]
    fn bail_invoke<T>(&self, site: u32, pc: usize) -> Option<T> {
        if ir_bail_reporting() {
            if let Some(label) = self.invoke_labels.get(&pc) {
                eprintln!(
                    "[ir] IrBuilder::build refused at ir.rs:{site} (bytecode pc {pc}) \
                     in {} callee {label}",
                    self.method_label.as_deref().unwrap_or("<unknown>"),
                );
                return None;
            }
        }
        ir_build_bail(site, pc)
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

    // ── Local-variable slots ─────────────────────────────────────────

    /// Is the value `id` names a JVM **category 2** value (`long`/`double`),
    /// i.e. one that occupies two local slots?
    ///
    /// Answered from the node's own type, which is the only model the IR has:
    /// the abstract `locals` array holds one `NodeId` per JVM slot and a
    /// category-2 value lives entirely in the LOW slot, so the width of a value
    /// is a property of the node, never of where it sits.
    fn is_cat2_value(&self, id: NodeId) -> bool {
        matches!(
            self.graph.node_opt(id).map(|n| n.ty),
            Some(IrType::Long | IrType::Double)
        )
    }

    /// Write `val` into local slot `idx`, invalidating the neighbouring slot
    /// the JVM's two-slot rule makes dead.
    ///
    /// # Why every store has to go through this (2026-09-17)
    ///
    /// Each store arm used to be a bare `self.locals[idx] = val`, and every
    /// category-2 arm carried a comment saying the high half `idx + 1` was
    /// "untouched". Untouched is not the same as unused, and two things follow
    /// from it:
    ///
    /// * **Stale oop retention, in every shape.** `locals[idx + 1]` keeps
    ///   whatever node was there before the `lstore`/`dstore`.
    ///   [`SafepointSnapshot::locals`] is pushed verbatim at every bci, and
    ///   `ir_optimize::eliminate_dead_nodes` roots every snapshot slot, so that
    ///   node stays alive — and, if it is `Ref`-typed, is published as a live
    ///   GC root at every later safepoint in the method. The JVM's own model
    ///   says the value is dead the moment the wide store lands; keeping it
    ///   pins an object for the rest of the frame.
    /// * **Deopt local misalignment.** `deopt_resume::ir_deopt_locals` walks a
    ///   snapshot's locals and does `i += if Long|Double { 2 } else { 1 }`.
    ///   That realigns correctly when the category-2 node at `i` is *current*.
    ///   It does not when slot `i` holds a **stale** `long`/`double` and slot
    ///   `i + 1` holds a live category-1 value: the category-1 value is skipped
    ///   and every later local in the frame shifts by one slot. javac's
    ///   lowest-free-index allocator makes that hard to produce, but ECJ,
    ///   Kotlin, Scala and any bytecode rewriter are free to, and nothing
    ///   upstream refuses it.
    ///
    /// Both directions of JVMS §2.6.1 are enforced here: a category-2 store
    /// clears the reserved high half, and ANY store into `idx` clears a
    /// category-2 value at `idx - 1` whose high half it has just overwritten.
    /// The second is what keeps the `i += 2` skip above honest.
    ///
    /// # "Any" store, not only a category-1 one (2026-09-18)
    ///
    /// The two clauses used to be an `if wide { … } else if … { … }`, so a
    /// category-2 store at `idx` never looked at `idx - 1`. `lstore_0;
    /// lstore_1` (ECJ and bytecode rewriters reuse slots like this; javac's
    /// allocator does not) left the first `long` at slot 0 AND the second at
    /// slot 1: the deopt walk sees a category-2 value at 0, skips slot 1 — the
    /// live `long` — and misaligns every later local by one. The two clauses
    /// are independent facts about two different slots.
    fn store_local(&mut self, idx: usize, val: NodeId) {
        if idx >= self.locals.len() {
            // Verified bytecode cannot name a slot past `max_locals`, so this
            // is unreachable — but the arms used to reach it as
            // `self.locals[idx] = val`, i.e. as a PANIC, on a background
            // compiler thread where a panic is a VM crash rather than a failed
            // compile. Refuse the method instead, and do not drop the store
            // silently: a lost store is a wrong-value miscompile.
            self.refuse(line!(), idx);
            return;
        }
        let wide = self.is_cat2_value(val);
        self.locals[idx] = val;
        if wide {
            // The high half of a category-2 value is reserved, and JVMS says
            // nothing may read it. Say "uninitialised" rather than leaving the
            // previous occupant to be rooted and published.
            if let Some(slot) = self.locals.get_mut(idx + 1) {
                *slot = NO_NODE;
            }
        }
        if idx > 0 && self.is_cat2_value(self.locals[idx - 1]) {
            // Storing into one slot of a two-slot value destroys the whole
            // value, whatever the width of what was stored.
            self.locals[idx - 1] = NO_NODE;
        }
    }

    /// Push the value of local slot `idx`, or answer `false` for a slot past
    /// the frame.
    ///
    /// The load half of [`Self::store_local`]'s bounds rule. Every load arm
    /// used to be `self.push(self.locals[idx])` — a PANIC on a background
    /// compiler thread for an index past `max_locals` — while the store arms
    /// refused the same index. The caller turns `false` into an attributed
    /// `ir_build_bail`.
    fn push_local(&mut self, idx: usize) -> bool {
        match self.locals.get(idx).copied() {
            Some(v) => {
                self.push(v);
                true
            }
            None => false,
        }
    }

    // ── Node creation helpers ────────────────────────────────────────

    fn add_data(&mut self, op: Op, ty: IrType, inputs: Vec<NodeId>, pc: usize) -> NodeId {
        #[cfg(debug_assertions)]
        self.assert_operand_types(&op, ty, &inputs, pc);
        self.graph.add(op, ty, inputs, Some(pc))
    }

    /// The builder's half of the shared operand-type table
    /// ([`expected_input_type`]): check the node *as it is created*, where the
    /// bytecode pc and the arm that built it are still on the stack.
    ///
    /// This is the reader that turns a verifier finding into a bug report. The
    /// verifier sees `n412:Cmp input[1] = n77:Const is long where the operand
    /// table requires int` after the whole method is built and several passes
    /// have run; this panics inside the `if_icmplt` arm that built it.
    ///
    /// # Why it is gated twice
    ///
    /// `cfg(debug_assertions)` compiles it out of a release build entirely,
    /// and [`crate::ir_verify::operand_type_lane_enabled`] keeps it silent in
    /// a debug build unless an operator set `CRATONVM_JIT_VERIFY_TYPES=1`.
    /// The second gate is the staging `NOTES-irfront.md` §4 asks for: this
    /// table has never been read against the whole corpus, and a *modelling*
    /// gap in it — a shape the front end legitimately emits that the table
    /// does not describe — must not be able to abort a debug build before
    /// anyone has looked at the findings. Turn it on, read them, then decide.
    ///
    /// It panics rather than reporting because that is the entire difference
    /// between this reader and the verifier's: an opt-in debug flag whose
    /// value is the stack trace.
    #[cfg(debug_assertions)]
    fn assert_operand_types(&self, op: &Op, ty: IrType, inputs: &[NodeId], pc: usize) {
        if !crate::ir_verify::operand_type_lane_enabled() {
            return;
        }
        let ty_of = |id: NodeId| {
            self.graph
                .node_opt(id)
                .filter(|n| n.op != Op::Dead && n.ty != IrType::Void)
                .map(|n| n.ty)
        };
        for slot in 0..inputs.len() {
            let Some(want) = required_input_type(op, ty, inputs, slot, &ty_of) else {
                continue;
            };
            let Some(have) = ty_of(inputs[slot]) else {
                continue;
            };
            assert_eq!(
                have, want,
                "pc {pc}: {op:?} input[{slot}] (node {}) is {have:?} where the operand \
                 table requires {want:?} — see ir::expected_input_type",
                inputs[slot]
            );
        }
    }

    /// JVMS §6.5 `ireturn` narrowing of `val` to `tag`
    /// ([`crate::narrowed_int_return_tag`]), built from exactly the nodes the
    /// `iand` (`Z`, `x & 1`), `i2b`, `i2c` and `i2s` arms build — no new op, no
    /// new lowering, and the fold passes see a shape they already know.
    fn narrow_int_return(&mut self, val: NodeId, tag: Option<u8>, pc: usize) -> NodeId {
        match tag {
            Some(b'Z') => {
                let one = self.iconst(1);
                self.add_data(Op::And, IrType::Int, vec![val, one], pc)
            }
            Some(b'B') => {
                let c = self.iconst(24);
                let shl = self.add_data(Op::Shl, IrType::Int, vec![val, c], pc);
                self.add_data(Op::Shr, IrType::Int, vec![shl, c], pc)
            }
            Some(b'C') => {
                let mask = self.iconst(0xFFFF);
                self.add_data(Op::And, IrType::Int, vec![val, mask], pc)
            }
            Some(b'S') => {
                let c = self.iconst(16);
                let shl = self.add_data(Op::Shl, IrType::Int, vec![val, c], pc);
                self.add_data(Op::Shr, IrType::Int, vec![shl, c], pc)
            }
            _ => val,
        }
    }

    fn iconst(&mut self, val: i64) -> NodeId {
        // Check if we already have this constant (simple dedup).
        //
        // The type is part of the identity: `aconst_null` also materialises
        // `Op::Const(0)`, typed `Ref` so the safepoint maps publish its slot.
        // Deduping on the opcode alone would hand that reference-typed node
        // back for every later `iconst(0)` — an integer comparison operand
        // would then be published as a live oop, and `Op::Cmp` would widen to
        // a 64-bit compare on it.
        if let Some(id) = self.interned_const(val, IrType::Int) {
            return id;
        }
        self.graph.add(Op::Const(val), IrType::Int, vec![], None)
    }

    /// The lowest-id `Op::Const(val)` node typed `ty`, or `None` — the answer
    /// the arena scan [`Self::iconst`] and [`Self::aconst_null`] used to make
    /// on every call, from [`Self::const_intern`] instead.
    ///
    /// # Why the map cannot go stale
    ///
    /// * **New nodes** — including a constant added through `self.graph`
    ///   directly rather than through a helper — are indexed on the next call:
    ///   everything from [`Self::const_intern_upto`] to the end of the arena is
    ///   scanned once, and `or_insert` keeps the LOWEST id per key, which is the
    ///   one the scan's first match returned.
    /// * **A killed node** (`Graph::kill` rewrites the op to `Op::Dead`; it is
    ///   the only op rewrite in this file) fails the re-check on a hit, and the
    ///   full scan decides instead and repairs the entry. Nothing ever turns a
    ///   node INTO a constant, so a node below a valid entry that was not a
    ///   match when indexed is not one now.
    /// * **A shorter arena** than has been indexed means the arena was replaced
    ///   or truncated, which the builder does not do today; the index is
    ///   rebuilt from scratch rather than trusted.
    fn interned_const(&mut self, val: i64, ty: IrType) -> Option<NodeId> {
        if self.graph.nodes.len() < self.const_intern_upto {
            self.const_intern.clear();
            self.const_intern_upto = 0;
        }
        for id in self.const_intern_upto..self.graph.nodes.len() {
            let node = &self.graph.nodes[id];
            if let Op::Const(v) = node.op {
                self.const_intern
                    .entry((v, node.ty))
                    .or_insert(id as NodeId);
            }
        }
        self.const_intern_upto = self.graph.nodes.len();

        let id = *self.const_intern.get(&(val, ty))?;
        if matches!(self.graph.nodes.get(id as usize), Some(n) if n.op == Op::Const(val) && n.ty == ty)
        {
            return Some(id);
        }
        match self
            .graph
            .nodes
            .iter()
            .position(|n| n.op == Op::Const(val) && n.ty == ty)
        {
            Some(found) => {
                self.const_intern.insert((val, ty), found as NodeId);
                Some(found as NodeId)
            }
            None => {
                self.const_intern.remove(&(val, ty));
                None
            }
        }
    }

    /// `aconst_null` — the null reference, as a `Ref`-typed `Op::Const(0)`.
    ///
    /// Typed `Ref` rather than `Int` for two reasons: `emit_safepoint_map`
    /// publishes exactly the `Ref`-typed defined nodes (a published null is
    /// harmless and keeps the frame's oop set complete), and `Op::Cmp` selects
    /// its operand width from the operand types — a reference comparison must
    /// be 64-bit. Deduped separately from the integer constants; see
    /// [`Self::iconst`].
    fn aconst_null(&mut self) -> NodeId {
        if let Some(id) = self.interned_const(0, IrType::Ref) {
            return id;
        }
        self.graph.add(Op::Const(0), IrType::Ref, vec![], None)
    }

    /// A `long` constant, interned exactly the way [`Self::iconst`] and
    /// [`Self::aconst_null`] are.
    ///
    /// [`Self::interned_const`] already keys on `(value, TYPE)` — that is the
    /// whole reason an `Int` `Const(0)` and the `Ref` `Const(0)` that
    /// `aconst_null` materialises are distinct nodes — so the `Long` case
    /// needed nothing but the call. Without it every `lconst_0`/`lconst_1`,
    /// every `ldc2_w` long and, worst, the zero every `ldiv`/`lrem` zero-guard
    /// materialises ([`Self::add_div_zero_guard`]) added a fresh node that GVN
    /// then had to collapse — compile time and graph budget
    /// ([`IR_MAX_GRAPH_NODES`]) spent to produce a node the very next pass
    /// removes.
    fn lconst(&mut self, val: i64) -> NodeId {
        if let Some(id) = self.interned_const(val, IrType::Long) {
            return id;
        }
        self.graph.add(Op::Const(val), IrType::Long, vec![], None)
    }

    /// Tell the builder whether the bodies it is about to splice commit any
    /// side effect. See [`Self::spliced_bodies_pure`]; called by `lib.rs`
    /// alongside the combined buffer it hands to [`Self::build`].
    pub fn set_spliced_bodies_pure(&mut self, pure: bool) {
        self.spliced_bodies_pure = pure;
    }

    /// Would the interpreter accept a whole-method replay for a trap at `pc`?
    ///
    /// This is [`replay_from_entry_is_observably_equivalent`]'s rule, asked at
    /// the producing end. It is deliberately the SAME three clauses in the
    /// same order rather than a paraphrase, because the whole point is that a
    /// trap this predicate admits is a trap the consumer will not refuse:
    ///
    ///  1. a body that commits nothing anywhere replays trivially;
    ///  2. otherwise every spliced body must also commit nothing, since the
    ///     caller's bytecode does not describe what a relocated body did;
    ///  3. and then only `code[..pc]` can have committed anything, because
    ///     every deopt point this VM emits is `REEXECUTE` — the bytecode AT
    ///     the trap had not completed and nothing after it ran.
    ///
    /// `code` may be the COMBINED buffer (method body, then the spliced
    /// bodies appended after it), so `code_len` — the original method's own
    /// length — is what bounds clause 1. A trap inside a spliced region never
    /// reaches this point: `plant_uncommon_trap` raises `splice_guard_seen`
    /// for one, which refuses the graph outright.
    fn trap_replay_is_safe(&self, code: &[u8], code_len: usize, pc: usize) -> bool {
        let body_len = code_len.min(code.len());
        if !crate::bytecode_commits_side_effect(code, body_len) {
            return true;
        }
        if !self.spliced_bodies_pure {
            return false;
        }
        if pc > body_len {
            return false;
        }
        !crate::bytecode_commits_side_effect(code, pc)
    }

    /// Plant an UNCONDITIONAL uncommon trap at `pc` and keep building.
    ///
    /// This is the answer to the question this front end used to answer by
    /// discarding the whole method: what to do with a construct at ONE bytecode
    /// index that this tier cannot lower. HotSpot's answer is an uncommon trap,
    /// and CratonVM's own single-pass backend already gives that answer for
    /// `invokedynamic` -- the site becomes a transfer to the interpreter at
    /// that bci and the rest of the method still compiles.
    ///
    /// Mechanically it is `Op::Guard { bci: pc }` with a condition that is the
    /// constant zero, which is exactly the shape [`Self::add_div_zero_guard`]
    /// builds when a divisor is provably zero. `Op::Guard` is a DCE root
    /// (`ir_optimize::eliminate_dead_nodes` lists it), nothing rewrites a guard
    /// on a constant condition, and its lowering already emits
    /// `DeoptReason::UncommonTrap` with `DeoptAction::Reinterpret` -- so the
    /// interpreter re-runs the bytecode at `pc`, which is precisely the
    /// semantics a site this tier declined to compile needs.
    ///
    /// # Why the code after it is still built
    ///
    /// The guard always fires, so everything the builder appends after it is
    /// unreachable at run time. It is built anyway because the alternative --
    /// truncating the walk -- leaves the abstract stack and the block structure
    /// in a state the rest of the builder is not written to accept, and an
    /// unreachable tail costs code size at compile time and nothing at run
    /// time. The caller pushes a zero of the site's result type so the stack
    /// depth stays what the verifier proved.
    ///
    /// # The coldness argument, and why it is different per caller
    ///
    /// A trap on a HOT path is worse than not compiling the method: every
    /// execution re-enters the interpreter. Each caller therefore owes an
    /// argument that its site is cold, and two of the three have a real one:
    ///
    /// * an unresolved `new` / `checkcast` / `instanceof` names a class that
    ///   **has never been loaded**, and a class that has never been loaded
    ///   cannot have been touched by any path that has executed. That is a
    ///   proof, not a heuristic.
    /// * `invokedynamic` has no such proof, but the single-pass backend has
    ///   made exactly this trade by default since it stopped bailing on indy,
    ///   so matching it here is consistent with a decision already soaked.
    ///
    /// [`ir_trap_census`] counts what was PLANTED by cause; the runtime side
    /// counts what is TAKEN. A cause whose taken count is not ~0 has had its
    /// coldness argument refuted, which is the falsifiable form of the claim.
    fn plant_uncommon_trap(
        &mut self,
        code: &[u8],
        code_len: usize,
        pc: usize,
        cause: TrapCause,
    ) -> bool {
        if !ir_site_trap_enabled() {
            return false;
        }
        // Counted at the END, on the success path only -- see the increment
        // just before `true` is returned. A refused plant must not make the
        // runtime think this method carries a trap.
        // The unresolved-class causes are opt-in and off by default; see
        // `ir_unresolved_class_trap_enabled` for the argument that was refuted.
        if matches!(
            cause,
            TrapCause::UnresolvedTypeCheck | TrapCause::UnresolvedNew
        ) && !ir_unresolved_class_trap_enabled()
        {
            return false;
        }

        // A trap this tier CANNOT BE RESUMED FROM is not a slow path, it is a
        // guaranteed `InternalError`. `x64::driver` sets
        // `can_deopt_resume = !deopt_points.is_empty() && !has_elided_monitor`
        // for the single-pass backend, but this backend only ever sets it on
        // the scalar-replacement path (`sr_map.is_some()`, i.e.
        // `CRATONVM_SCALAR_DEOPT` + `CRATONVM_DEOPT_REAL`) -- so on a
        // production artifact an optimizing-tier deopt has exactly ONE
        // fallback: the interpreter's whole-method replay from entry. That
        // replay is refused, fatally, whenever the bytecode before the trap
        // already committed something a re-run would duplicate.
        //
        // The doc comment above claims parity with the single-pass backend's
        // indy trade. That backend takes the same trade AND carries the safety
        // valve this one was missing: `bytecode_walk`'s 0xba arm bails the
        // whole compile (`mark_codegen_unencodable("unresumable-indy-trap")`)
        // when the snapshot it just built is not resumable. Copying the trade
        // without the valve is what made `com/sun/tools/javac/code/
        // Scope$ScopeImpl.remove` -- `Assert.check(...)` at bci 12, an
        // `invokedynamic` at bci 21 -- die on its FIRST compiled call with
        // `precise deoptimization unavailable ... refusing side-effecting
        // replay`, which javac reports as a compile that failed with zero
        // diagnostics. That is the whole 19-class Spring `TestCompiler`/AOT
        // cluster and the `--nojit`-clears-it finding above it.
        //
        // Asked with the CONSUMER's own predicate
        // (`replay_from_entry_is_observably_equivalent` in
        // vm/src/runtime/interpreter/deopt_resume.rs) so the two ends cannot
        // drift: whole body pure, else the prefix before the trap pure and
        // every spliced body pure.
        // AFTER the default-off unresolved-class gate, deliberately. Both
        // arms return `false` and the compile bails identically either way,
        // so the order is not a behaviour question -- it is a COUNTING one.
        // Above the gate, every `checkcast`/`instanceof`/`new` site was
        // charged to this refusal even though `ir_unresolved_class_trap_enabled`
        // was going to decline it regardless: on one javac workload that was
        // 77 of 126 rows, i.e. the census read the guard as three times more
        // expensive than it is. A refusal counted here now means exactly
        // "this site would have been a trap but for the resumability rule",
        // which is the only reading that makes the census a price tag.
        if ir_trap_replay_guard_enabled() && !self.trap_replay_is_safe(code, code_len, pc) {
            if ir_bail_reporting() {
                eprintln!(
                    "[ir] site TRAP REFUSED at bytecode pc {pc} ({}) in {} -- \
                     the code before it commits a side effect and this tier \
                     publishes no resumable deopt, so the trap would be a hard \
                     InternalError; declining the optimizing tier instead",
                    cause.as_str(),
                    self.method_label.as_deref().unwrap_or("<unknown>"),
                );
            }
            note_trap_refused(cause);
            return false;
        }
        let Some(ctrl) = self.ctrl_opt() else {
            return false;
        };
        // A trap inside a spliced callee body would record a bci the OUTER
        // method does not have -- the shape that produced the
        // `ir-inline-turns-an-index-out-of-bounds-into-an-internalerror`
        // regression. `Op::Guard`'s lowering maps through `resume_bci`, and
        // `splice_guard_seen` is the fence that refuses such a graph outright;
        // raise it here for the same reason `add_div_zero_guard` does.
        if !self.splice.is_empty() {
            self.splice_guard_seen = true;
        }
        // FAIL CLOSED on a missing frame state, and this is the hazard worth
        // spelling out: `ir_lower`'s `resolve_frame_state_for_bci` answers a
        // bci it has no snapshot for with an EMPTY `FrameState` -- no locals,
        // no stack -- rather than an error. A trap resolved that way would park
        // the interpreter at `pc` with a blank frame, which is a plausible
        // wrong answer rather than a crash.
        //
        // The main walk pushes a snapshot at the top of every bci it visits, so
        // this holds by construction today and the check has never fired. It is
        // here because the property it depends on lives two thousand lines away
        // and nothing else would notice it changing.
        //
        // Searched newest-first, here and at the three other "is there a
        // snapshot at this bci" sites: the walk pushes the snapshot for `pc`
        // immediately before the opcode runs, so it is the LAST one and the
        // search is O(1) where a front-to-back scan was O(snapshots) per trap
        // — quadratic over a method with many trap or guard sites.
        if !self.graph.safepoints.iter().rev().any(|sp| sp.bci == pc) {
            return false;
        }
        let zero = self.iconst(0);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, zero],
            Some(pc),
        );
        note_trap_planted(cause);
        SITE_TRAPS_THIS_BUILD.with(|c| c.set(c.get() + 1));
        if ir_bail_reporting() {
            eprintln!(
                "[ir] site TRAP planted at bytecode pc {pc} ({}) in {} -- the rest of the method still compiles",
                cause.as_str(),
                self.method_label.as_deref().unwrap_or("<unknown>"),
            );
        }
        true
    }

    /// Branch pcs the profile says are ALWAYS taken. See
    /// [`Self::prune_always_taken_branch`].
    pub fn set_pruned_branches(&mut self, pcs: std::collections::HashSet<usize>) {
        self.pruned_always_taken = pcs;
    }

    /// How many branches this build replaced with a guarded jump.
    pub fn branches_pruned(&self) -> usize {
        self.branches_pruned
    }

    /// Replace an always-taken conditional branch with a GUARD plus an
    /// unconditional jump, deleting its cold arm from the graph entirely.
    ///
    /// # What this is
    ///
    /// The first actual speculation this tier performs. Every deopt point it
    /// emitted before this carried the reason `TransferToInterpreter` — plain
    /// resume points, planted so that a trap *could* be described, never so
    /// that a transform could be justified. Uncommon traps existed only to
    /// stand in for opcodes the tier cannot lower. Nothing anywhere used the
    /// profile to remove work.
    ///
    /// This does. When the profile has seen a branch's not-taken edge exactly
    /// ZERO times over a meaningful sample, the branch becomes:
    ///
    /// ```text
    ///     Guard(cmp)        ; cmp == 0 -> deopt at this bci
    ///     goto target
    /// ```
    ///
    /// and the whole fall-through arm is never built. That is the shape javac
    /// gives `if (rare) { ... }` — `ifeq skip; <cold body>; skip:` — so the arm
    /// deleted is exactly the cold body, together with everything downstream
    /// that only it kept alive.
    ///
    /// # Why it is fail-SAFE rather than fail-closed
    ///
    /// A wrong speculation cannot produce a wrong answer. The guard's failure
    /// edge is the ordinary deopt trampoline with `DeoptAction::Reinterpret`:
    /// the interpreter re-executes this very branch and takes the cold arm
    /// itself. The cost of being wrong is a deopt, and repeated deopts demote
    /// the method to C1 through the existing `c2_bailout` path — the same
    /// machinery a div-by-zero guard already relies on. That is what makes this
    /// a different risk class from a speculative devirtualization, where a
    /// wrong guard runs the wrong body.
    ///
    /// # Two refusals
    ///
    /// * **Inside a splice.** `splice_guard_seen` refuses a graph that built a
    ///   guard inside a relocated body, because such a guard would resolve its
    ///   frame state from the caller's snapshot at the `invoke`. Rather than
    ///   set that flag and lose the whole method, simply do not speculate
    ///   there.
    /// * **No frame state at this bci.** A guard whose bci has no
    ///   `SafepointSnapshot` cannot be resumed, so there would be nothing to
    ///   deopt *to*. `plant_site_trap` makes the same check for the same
    ///   reason.
    /// * **A BACKWARD branch.** See below.
    ///
    /// # Why a back edge is never a candidate (2026-09-17)
    ///
    /// The profile feed in `lib.rs` selects every branch pc with `not_taken ==
    /// 0 && taken >= MIN_OBSERVATIONS_TO_PRUNE`. It now also requires a
    /// forward target (`lib.rs::speculatable_forward_branch`), so a back edge
    /// no longer reaches here at all — but the refusal below stays, because
    /// this is where the consequence of pruning one is, and a builder that
    /// depends on its caller's filter for termination is a builder one
    /// refactor away from looping forever again. Before either existed, a
    /// *bottom-tested* loop (`do { … } while (cond)`) is not rotated —
    /// `has_rotated_loop_header` excludes a header that has a reachable
    /// lower-pc predecessor — so it is walked in pc order with `block_walk ==
    /// false` and reached this function. Its back edge is always-taken by
    /// definition whenever the profile was gathered while the loop was still
    /// running (an OSR trigger, a retry/spin loop): the cold arm is the loop
    /// *exit*, which has been taken zero times precisely because the loop has
    /// not finished yet.
    ///
    /// Pruning it did this: record the header as a predecessor (the header is
    /// already `visited`, so `patch_loop_backedge` appends the back edge),
    /// then `pc = target_pc` and `continue` — landing the walk back ON the
    /// header, which re-ran `activate_loop_header`, built a second generation
    /// of φs with one value against the region's two predecessors, orphaned the
    /// first generation, re-walked the whole body, pruned the same branch again
    /// and **never terminated**, allocating nodes on every pass. Nothing
    /// bounded it: `IR_MAX_GRAPH_NODES` is only checked after `build` returns.
    ///
    /// So the refusal below is not merely a safety net for that hang (the
    /// `visited` guard in `activate_loop_header` and the step budget in `build`
    /// are), it is the semantically right answer: a loop's exit edge being
    /// unobserved is evidence about *when the profile was taken*, not evidence
    /// that the loop never exits, and speculating on it would plant a guard
    /// that deopts on the final iteration of every single loop it fires on.
    fn prune_always_taken_branch(&mut self, pc: usize, cmp: NodeId, target_pc: usize) -> bool {
        if !self.pruned_always_taken.contains(&pc) {
            return false;
        }
        if target_pc <= pc {
            return false;
        }
        if !self.splice.is_empty() {
            return false;
        }
        // A pruned branch continues the walk at `target_pc`, skipping the
        // untaken arm by pc order. The block walk visits ranges in reverse
        // post-order and would lose its place, and the fall-through block it
        // still visits would have no live control. Keep the branch instead.
        if self.block_walk {
            return false;
        }
        let Some(ctrl) = self.ctrl_opt() else {
            return false;
        };
        if !self.graph.safepoints.iter().rev().any(|sp| sp.bci == pc) {
            return false;
        }
        // `cmp` is 1 exactly when the branch is TAKEN, so a non-zero `cmp`
        // continues and a zero one deopts — the identical polarity
        // `add_div_zero_guard` uses for its `Cmp(Ne)` against zero.
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, cmp],
            Some(pc),
        );
        // From here it is exactly the `goto` arm (0xa7): the target gains this
        // predecessor and control is dead until the next merge.
        //
        // The caller then continues the walk at `pc + 3` — INTO the cold arm,
        // as dead code — rather than jumping to `target_pc`. Until round 9
        // wave 2 it jumped, and the cold arm `[pc + 3, target_pc)` was never
        // walked; a merge inside it that some OTHER branch targets recorded a
        // predecessor and was never activated, leaving that branch's
        // projection with no successor (`audit_unactivated_merges` refused
        // the build). Walking it as dead code activates exactly the merges
        // that have a live predecessor; `pruned_dead_until` tells the main
        // walk that dead control below `target_pc` is the cold arm, not a
        // bookkeeping bug.
        self.add_merge_predecessor(target_pc);
        self.ctrl = NO_NODE;
        self.pruned_dead_until = self.pruned_dead_until.max(target_pc);
        self.branches_pruned += 1;
        note_branch_pruned();
        true
    }

    /// Anchor an integer `idiv`/`ldiv`/`irem`/`lrem`'s zero-divisor trap to the
    /// control token in effect at `pc`, and return the guard so the caller can
    /// name it as the division's token (see [`with_guard_token`]).
    ///
    /// Returns [`NO_NODE`] when no guard was built — unreachable code, where
    /// there is no live control token, no safepoint snapshot is recorded, and
    /// the lowerer emits no guard either.
    ///
    /// # Where this doc used to live
    ///
    /// Immediately above `plant_uncommon_trap`, whose own doc it ran into
    /// without a blank line, so rustdoc attached all of it to that function
    /// and this one was undocumented. Moved here on 2026-09-17 with the
    /// signature change; nothing below is new except the paragraph above.
    ///
    /// `Op::Div` / `Op::Rem` are **floating** data nodes: `add_data` gives them
    /// two value inputs and no control edge, so `ir_schedule::find_best_block`
    /// places them in the deepest block their operands are available in. That
    /// block can *dominate* the branch the bytecode guards the division with —
    /// which is fine for a pure node (the result is simply dead on the paths
    /// Java would not have computed it) but not for the trap: the lowerer's
    /// own `emit_div_zero_guard` then deopts, at this bci, on a path the
    /// interpreter never reaches, and the interpreter faithfully re-executes
    /// the division it is parked on and throws.
    ///
    /// Measured shape (`org.h2.util.MemoryEstimator.estimateMemory`, H2's
    /// `TestMultiThread`): `sum = (sum * counter + delta + (counter >> 1)) /
    /// counter` sits under `if (initialized == 0)`, where `counter` has just
    /// been incremented back to a positive value; on the *other* arm `counter`
    /// is `-1`, so the hoisted `ldiv` divides by zero and the method dies with
    /// `ArithmeticException: / by zero` (or, before the deopt frames carried an
    /// identity, with `InternalError: … refusing side-effecting replay` blamed
    /// on an unrelated caller).
    ///
    /// `Op::Guard` carries `[ctrl, cond]`, so the scheduler pins it to the
    /// block the division really belongs to, and it deopts at exactly this bci
    /// when `cond` is false. With the guard in place the lowerer stops trapping
    /// on the speculative copy and materialises a placeholder result instead —
    /// see `ir_lower::emit_div_zero_guard`, which keys off the presence of this
    /// node so a graph built without one keeps its old behaviour. The token
    /// edge makes that "presence" an edge rather than a by-bci scan; the
    /// lowerer still scans, and is unchanged.
    fn add_div_zero_guard(&mut self, divisor: NodeId, ty: IrType, pc: usize) -> NodeId {
        let Some(ctrl) = self.ctrl_opt() else {
            return NO_NODE;
        };
        let zero = if ty == IrType::Long {
            self.lconst(0)
        } else {
            self.iconst(0)
        };
        let cond = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![divisor, zero], pc);
        if !self.splice.is_empty() {
            self.splice_guard_seen = true;
        }
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, cond],
            Some(pc),
        )
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
    /// Returns a mutable handle to the state for `target_pc`, creating it on
    /// first touch.
    ///
    /// Returning the handle rather than `()` is what removes
    /// `add_merge_predecessor`'s `self.merges.get_mut(&target_pc).unwrap()`.
    /// That unwrap was sound — this function had just inserted the entry — but
    /// it was sound by a two-statement argument that nothing enforced, on a
    /// path that runs on a background compiler thread where a panic is a VM
    /// crash rather than a failed compile. The borrow checker enforces it now,
    /// and there is no `unwrap`/`expect`/`unreachable!` left on the path: the
    /// `Op::Merge` node is allocated only when the entry is vacant, and
    /// `or_insert_with` then consumes it without a fallible lookup.
    fn ensure_merge(&mut self, target_pc: usize) -> &mut MergeState {
        // Allocate the control node BEFORE taking the map entry: `self.graph`
        // and `self.merges` are disjoint fields, but the borrow checker sees
        // one `&mut self`, so the node cannot be created inside the closure.
        // Allocating only on the vacant path keeps this free for the common
        // "merge already exists" case.
        let fresh_merge_id = if self.merges.contains_key(&target_pc) {
            None
        } else {
            Some(
                self.graph
                    .add(Op::Merge, IrType::Control, vec![], Some(target_pc)),
            )
        };
        self.merges.entry(target_pc).or_insert_with(|| MergeState {
            // Reached only when the entry is vacant, which is exactly when
            // `fresh_merge_id` is `Some` — the two tests are the same test.
            merge_id: fresh_merge_id.unwrap_or(NO_NODE),
            ctrl_inputs: Vec::new(),
            local_snapshots: Vec::new(),
            stack_snapshots: Vec::new(),
            mem_inputs: Vec::new(),
            monitor_snapshots: Vec::new(),
            visited: false,
        })
    }

    /// Follow `id` through φs that merge a single value (every value input is
    /// either that value or the φ itself) to the value they stand for.
    ///
    /// A loop header gives EVERY entry-initialised local an eager φ, so the
    /// `aload` that feeds a `monitorexit` after a loop reads `phi(x, x)` while
    /// the monitor stack recorded `x` at the `monitorenter`. Both name the same
    /// reference; this is what lets [`Self::same_monitor_object`] say so.
    /// Bounded, and stops at anything that is not a single-value φ.
    fn strip_trivial_phis(&self, id: NodeId) -> NodeId {
        let mut cur = id;
        for _ in 0..32 {
            let Some(node) = self.graph.node_opt(cur) else {
                return cur;
            };
            if node.op != Op::Phi || node.ty == IrType::Memory {
                return cur;
            }
            let mut only: Option<NodeId> = None;
            for v in node.phi_value_inputs() {
                let Some(v) = v else {
                    return cur;
                };
                if v == cur {
                    continue;
                }
                match only {
                    None => only = Some(v),
                    Some(o) if o == v => {}
                    Some(_) => return cur,
                }
            }
            match only {
                Some(next) => cur = next,
                None => return cur,
            }
        }
        cur
    }

    /// Do `a` and `b` provably name the same locked reference?
    ///
    /// Identity, or identity after [`Self::strip_trivial_phis`]. Anything else
    /// is answered `false`, which makes the caller refuse the method: a
    /// `monitorexit` whose operand cannot be matched to the innermost held
    /// monitor is either unstructured locking or a shape this builder cannot
    /// describe in a frame state.
    fn same_monitor_object(&self, a: NodeId, b: NodeId) -> bool {
        a == b || self.strip_trivial_phis(a) == self.strip_trivial_phis(b)
    }

    /// Do two abstract monitor stacks describe the same held-lock sequence?
    fn monitor_stacks_agree(&self, a: &[NodeId], b: &[NodeId]) -> bool {
        a.len() == b.len()
            && a.iter()
                .zip(b.iter())
                .all(|(&x, &y)| self.same_monitor_object(x, y))
    }

    /// Record the current state as a predecessor of the merge at `target_pc`.
    ///
    /// If the merge has already been activated (`visited`), this predecessor is
    /// a **loop back-edge** (in reducible bytecode a forward merge is activated
    /// only after all its forward predecessors are in, so a later predecessor
    /// can only come from a backward branch). Back-patch the loop-header phis
    /// instead of recording a snapshot that activation already consumed.
    fn add_merge_predecessor(&mut self, target_pc: usize) {
        if self.ensure_merge(target_pc).visited {
            self.patch_loop_backedge(target_pc);
            return;
        }
        // Cloned before the borrow so the single `ensure_merge` handle below
        // is the only one live — this is what lets the `unwrap` go.
        let (ctrl, mem) = (self.ctrl, self.mem);
        let monitors = self.monitors.clone();
        let locals = self.locals.clone();
        let stack = self.stack.clone();
        let state = self.ensure_merge(target_pc);
        state.ctrl_inputs.push(ctrl);
        state.local_snapshots.push(locals);
        state.stack_snapshots.push(stack);
        state.mem_inputs.push(mem);
        state.monitor_snapshots.push(monitors);
    }

    /// Adopt the monitor stack every forward predecessor of a join carries, or
    /// latch [`Self::unstructured_monitors`] when two of them disagree.
    fn join_monitor_stacks(&mut self, snapshots: &[Vec<NodeId>]) {
        let Some(first) = snapshots.first() else {
            return;
        };
        if snapshots[1..]
            .iter()
            .any(|other| !self.monitor_stacks_agree(first, other))
        {
            self.unstructured_monitors = true;
        }
        self.monitors = first.clone();
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
    ///
    /// # Activating twice is always a bug
    ///
    /// The `visited` refusal below is not defensive programming; it is the
    /// direct fix for a hang. A profiled always-taken BACKWARD branch used to
    /// reach [`Self::prune_always_taken_branch`], which records the header as a
    /// predecessor and jumps the walk to it — so the walk arrived at an
    /// already-activated header, re-ran this function, called `set_inputs` over
    /// a now-two-entry `ctrl_inputs`, built a **fresh** generation of φs with
    /// one value per predecessor against the region's two, orphaned the first
    /// generation in `loop_phis`, re-walked the body, pruned the same branch
    /// again and looped forever — allocating nodes on every pass, with
    /// [`IR_MAX_GRAPH_NODES`] only checked *after* `build` returns.
    /// `prune_always_taken_branch` now declines a back edge outright, and this
    /// refusal is the independent guard: whatever future edit reaches a merge
    /// pc twice gets a bounded fall-back to the single-pass tier instead of a
    /// compiler thread that never returns.
    fn activate_loop_header(&mut self, target_pc: usize) {
        let state = match self.merges.get(&target_pc) {
            Some(s) if !s.ctrl_inputs.is_empty() => s.clone(),
            _ => return,
        };
        if state.visited {
            self.refuse(line!(), target_pc);
            return;
        }
        // Every predecessor of a join carries the same abstract frame in
        // verified bytecode. Where they disagree there is no one frame to
        // record, and the φ construction below would silently pad the short
        // one with `NO_NODE` / truncate the long one — see
        // [`Self::predecessor_frames_agree`].
        if !self.predecessor_frames_agree(&state) {
            self.refuse(line!(), target_pc);
            return;
        }
        let region = state.merge_id;
        // The region's control inputs are the forward-entry ctrls so far; the
        // back-edge ctrl is appended in patch_loop_backedge.
        self.graph.set_inputs(region, state.ctrl_inputs.clone());
        self.ctrl = region;
        // The monitor stack is NOT φ'd: a held lock is the same reference on
        // every edge, and `patch_loop_backedge` checks the back edge agrees.
        self.join_monitor_stacks(&state.monitor_snapshots);

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
    ///
    /// # "No loop phis here" is a REFUSAL, not a no-op (2026-09-17)
    ///
    /// [`Self::add_merge_predecessor`] routes *every* predecessor of an
    /// already-`visited` merge here, so this function is the only thing that
    /// records such an edge. Until 2026-09-17 a `target_pc` that was not a
    /// registered loop header answered `None => return`: the predecessor's
    /// control edge and all of its values were dropped on the floor, silently.
    ///
    /// That is invisible to the verifier by construction. The merge simply has
    /// fewer predecessors than the CFG does, and the φs anchored at it have a
    /// value count that still *matches* that reduced predecessor count, so
    /// `ir_verify::check_phis` sees a perfectly consistent graph and
    /// `check_control` only asks that some terminator is reachable. The result
    /// is a compiled method in which one whole path's values never reach the
    /// join — a wrong-value miscompile with no bailout and no diagnostic.
    ///
    /// It is reachable whenever `self.loop_headers` and the walk order
    /// disagree: an irreducible CFG that `BlockWalk` admitted, a future edit to
    /// the header set, or (before its own fix) the pruned-branch path that
    /// jumped the walk backwards onto an activated merge. "I cannot record this
    /// edge" must never be spelled the same way as "nothing to do", so it
    /// latches [`Self::build_refused`] instead.
    fn patch_loop_backedge(&mut self, target_pc: usize) {
        let lp = match self.loop_phis.get(&target_pc) {
            Some(lp) => lp.clone(),
            None => {
                self.refuse(line!(), target_pc);
                return;
            }
        };
        // Copied out rather than held as a borrow: `self.refuse` below needs
        // `&mut self`, and the map entry is three `usize`s.
        //
        // `add_merge_predecessor` created the entry before calling us, so the
        // `None` arm cannot happen — but it is a map lookup on a compiler
        // thread where a panic is a VM crash, so it refuses rather than
        // indexes, which is what `self.merges[&target_pc]` used to do.
        let entry = self.merges.get(&target_pc).map(|m| {
            (
                m.merge_id,
                m.stack_snapshots.first().map(|s| s.len()),
                m.local_snapshots.first().map(|s| s.len()),
            )
        });
        let Some((region, entry_depth, entry_locals)) = entry else {
            self.refuse(line!(), target_pc);
            return;
        };
        let back_ctrl = self.ctrl;
        // The back edge must arrive with the same abstract frame shape the
        // header was entered with. `self.stack.get(i)` below silently ignores
        // any extra entries the back edge carries and pads a short one with
        // `NO_NODE`; refuse instead, for the reason
        // [`Self::predecessor_frames_agree`] gives.
        if entry_depth.is_some_and(|d| d != self.stack.len())
            || entry_locals.is_some_and(|n| n != self.locals.len())
        {
            self.refuse(line!(), target_pc);
            return;
        }
        self.graph.push_input(region, back_ctrl);
        // Structured locking: the back edge must arrive holding exactly the
        // monitors the header was entered with.
        let back_edge_monitors_disagree = self
            .merges
            .get(&target_pc)
            .and_then(|s| s.monitor_snapshots.first())
            .is_some_and(|entry| !self.monitor_stacks_agree(entry, &self.monitors));
        if back_edge_monitors_disagree {
            self.unstructured_monitors = true;
        }

        // Appending the back-edge value completes the φ's input list, so its
        // type is re-derived from the *whole* merge (`retype_phi`): the type
        // recorded at header activation saw only the entry edge(s).
        for (i, &phi) in lp.local_phis.iter().enumerate() {
            if let Some(phi) = node_id_opt(phi) {
                let v = self.locals.get(i).copied().unwrap_or(NO_NODE);
                self.graph.push_input(phi, v);
                self.retype_phi(phi, target_pc);
            }
        }
        for (i, &phi) in lp.stack_phis.iter().enumerate() {
            if let Some(phi) = node_id_opt(phi) {
                let v = self.stack.get(i).copied().unwrap_or(NO_NODE);
                self.graph.push_input(phi, v);
                self.retype_phi(phi, target_pc);
            }
        }
        if let Some(mem_phi) = node_id_opt(lp.mem_phi) {
            self.graph.push_input(mem_phi, self.mem);
        }
        // Record this back-edge as a predecessor for completeness (so any later
        // consumer that scans MergeState sees the right pred count).
        if let Some(s) = self.merges.get_mut(&target_pc) {
            s.ctrl_inputs.push(back_ctrl);
        }
    }

    /// Do every recorded predecessor of a join describe the same abstract
    /// frame *shape* — the same operand-stack depth and the same locals length?
    ///
    /// Verified bytecode guarantees they do, so a `false` here is never a
    /// property of the Java program: it is this builder's own walk order or
    /// splice bookkeeping having gone wrong. That is exactly why it must be a
    /// refusal rather than a repair. Both activation paths read
    /// `snap.get(i).copied().unwrap_or(NO_NODE)` and size themselves from the
    /// FIRST predecessor, so a disagreement is absorbed silently: a shorter
    /// predecessor contributes `NO_NODE` (an undefined value reaching a real
    /// use) and a longer one has its extra slots dropped. Neither is visible to
    /// `ir_verify` afterwards, because the φs and the merge stay *consistent
    /// with each other* — they are just consistent about the wrong frame.
    ///
    /// This is the same posture the `dup2` family arms take: where two models
    /// of the operand stack could disagree, refuse instead of guessing.
    fn predecessor_frames_agree(&self, state: &MergeState) -> bool {
        let stacks_agree = match state.stack_snapshots.first() {
            Some(first) => state.stack_snapshots.iter().all(|s| s.len() == first.len()),
            None => true,
        };
        let locals_agree = match state.local_snapshots.first() {
            Some(first) => state.local_snapshots.iter().all(|s| s.len() == first.len()),
            None => true,
        };
        stacks_agree && locals_agree
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

        // A second activation rebuilds every φ at this join and resets the
        // merge's control inputs, orphaning the first generation — the same
        // corruption [`Self::activate_loop_header`] documents at length. Any
        // walk that reaches a merge pc twice (a malformed range list, a future
        // block-walk edit) is a bug in the walk, so refuse rather than
        // duplicate.
        if state.visited {
            self.refuse(line!(), target_pc);
            return;
        }
        if !self.predecessor_frames_agree(&state) {
            self.refuse(line!(), target_pc);
            return;
        }

        // Update merge node inputs
        let merge_id = state.merge_id;
        self.graph.set_inputs(merge_id, state.ctrl_inputs.clone());
        self.ctrl = merge_id;
        self.join_monitor_stacks(&state.monitor_snapshots);

        // Create phi for memory — only where the edges disagree, exactly as the
        // locals below and `finish_multi_return_splice` do. An `if` whose arms
        // touch no memory reaches its join with one token on every edge, and
        // `phi(t, t, …)` is `t`: building it anyway cost a node per such join
        // for GVN to fold back.
        if state.mem_inputs.len() > 1 && state.mem_inputs.iter().any(|&m| m != state.mem_inputs[0])
        {
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
                    // A slot whose predecessors hold values of categories that
                    // do not join is DEAD here — the JVM verifier's merge makes
                    // it TOP, which nothing may read. javac produces this all
                    // the time: `if (c) { int x … } else { Object y … }` reuses
                    // one slot for both. Say "uninitialised" instead of
                    // building the φ: that φ could only take
                    // `PHI_TYPE_FALLBACK`, every later snapshot would root it,
                    // and `ir_verify::check_types` reports exactly that φ at
                    // `"post-optimize"` — so every such method used to fall back
                    // to the single-pass tier in any build that verifies.
                    // See [`phi_inputs_conflict`] for why this is sound.
                    if phi_inputs_conflict(&self.graph, &phi_inputs) {
                        self.locals[local_idx] = NO_NODE;
                        continue;
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
                        let phi = self.graph.add(Op::Phi, phi_ty, phi_inputs, Some(target_pc));
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

    /// End-of-build check over every merge the pre-scan registered: return
    /// the lowest pc of a merge that RECORDED a predecessor but was never
    /// activated (the caller refuses the build), and kill every merge that was
    /// neither reached nor recorded (an orphan).
    ///
    /// # The lost edge (refused)
    ///
    /// A predecessor recorded into a merge the walk never activates is a
    /// control edge with nowhere to go: its `Op::Proj` is named only by the
    /// builder's `MergeState`, never by a graph node, so the branch arm it
    /// stands for has NO successor in the graph. Nothing downstream can see
    /// that — `eliminate_dead_nodes` removes the input-less merge, the
    /// verifier checks projection indices but not that each one leads
    /// somewhere, and the lowerer has no block to send the arm to.
    ///
    /// The walk visits every reachable pc, so this cannot happen today except
    /// through [`Self::prune_always_taken_branch`] (default off): it jumps the
    /// walk over the whole cold arm, and a merge inside that arm which ANOTHER
    /// branch also targets keeps that branch's edge and is never activated.
    /// Refusing is the correct answer there and the backstop for any later
    /// edit that skips code.
    ///
    /// # The orphan (killed)
    ///
    /// A merge with no predecessor at all is code the walk legitimately never
    /// entered — most often a spliced callee body that was never spliced
    /// because its `invokespecial` took the trivial-`<init>` elision first,
    /// whose own merge targets the pre-scan registered anyway. The node was
    /// created with no inputs and nothing names it (φs are made only at
    /// activation), so it is dead; but `ir_verify`'s structural lane requires
    /// every `Op::Merge` to have a predecessor, so leaving it made the
    /// post-build verification refuse an otherwise correct graph.
    fn audit_unactivated_merges(&mut self) -> Option<usize> {
        let mut lost_at: Option<usize> = None;
        let mut orphans: Vec<NodeId> = Vec::new();
        for (&target, state) in &self.merges {
            if state.visited {
                continue;
            }
            if state.ctrl_inputs.is_empty() {
                orphans.push(state.merge_id);
            } else {
                lost_at = Some(lost_at.map_or(target, |p| p.min(target)));
            }
        }
        if lost_at.is_some() {
            return lost_at;
        }
        for id in orphans {
            let unused_and_inputless = self
                .graph
                .node_opt(id)
                .is_some_and(|n| n.op == Op::Merge && n.inputs.is_empty());
            if unused_and_inputless && self.graph.use_count(id) == 0 {
                self.graph.kill(id);
            }
        }
        None
    }

    /// Retire every data φ whose value inputs do not join (see
    /// [`phi_inputs_conflict`]): its uses in other φs and in safepoint
    /// snapshots become [`NO_NODE`] ("undefined here") and the φ is killed.
    /// Returns how many were retired.
    ///
    /// # Why this is needed after `activate_merge` already declines them
    ///
    /// A forward join knows all its predecessors when it is activated, so it
    /// simply does not build such a φ. A LOOP header cannot: its φs are built
    /// from the entry edges before the back-edge value exists, and a slot
    /// javac reuses — a dead `Object` before the loop, an `int` declared in
    /// the body — only turns out to conflict when `patch_loop_backedge`
    /// appends the back-edge value, by which time the φ is named by every
    /// snapshot in the loop body. `retype_phi` then gives it
    /// [`PHI_TYPE_FALLBACK`], and `ir_verify::check_types` reports that φ at
    /// `"post-optimize"` — so, in every build that verifies, a method with an
    /// ordinary reused slot around a loop fell back to the single-pass tier.
    ///
    /// # Why it is sound
    ///
    /// A conflicting merge is TOP in the JVM verifier's model, so in verified
    /// bytecode no instruction reads the slot: the only consumers are other φs
    /// (themselves merging a dead slot) and snapshots (which describe it as
    /// undefined from now on — the right answer for a deopt frame, too). A φ
    /// that some OTHER node does read, or that a snapshot names as a HELD
    /// MONITOR, contradicts that premise; it is left exactly as it was, so the
    /// verifier still reports it and nothing is silently changed.
    ///
    /// All conflicting φs are collected BEFORE any is rewritten. Rewriting one
    /// can only remove a typed input from another φ, which never creates a
    /// conflict and never changes the join of a φ that is not itself retired
    /// (a survivor's remaining typed inputs already agreed).
    fn retire_untypeable_phis(&mut self) -> usize {
        let doomed: Vec<NodeId> = self
            .graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| {
                n.op == Op::Phi
                    && !matches!(n.ty, IrType::Memory | IrType::Control)
                    && phi_inputs_conflict(&self.graph, &n.inputs)
            })
            .map(|(id, _)| id as NodeId)
            .collect();
        if doomed.is_empty() {
            return 0;
        }
        let doomed_set: HashSet<NodeId> = doomed.iter().copied().collect();
        // A doomed φ that anything but another φ reads is not a dead slot.
        let mut read: HashSet<NodeId> = HashSet::new();
        for node in &self.graph.nodes {
            if node.op == Op::Phi || node.op == Op::Dead {
                continue;
            }
            for inp in node.inputs.iter() {
                if doomed_set.contains(inp) {
                    read.insert(*inp);
                }
            }
        }
        for sp in &self.graph.safepoints {
            for m in &sp.monitors {
                if doomed_set.contains(m) {
                    read.insert(*m);
                }
            }
        }
        let mut retired = 0usize;
        for id in doomed {
            if read.contains(&id) {
                continue;
            }
            self.graph.replace_all_uses(id, NO_NODE);
            self.graph.kill(id);
            retired += 1;
        }
        retired
    }

    // ── Main bytecode→IR conversion ──────────────────────────────────

    /// Convert bytecode to IR graph.  Returns `None` if an unsupported
    /// opcode is encountered.
    pub fn build(mut self, code: &[u8], code_len: usize) -> Option<Graph> {
        // Per-build, per-thread: `lib.rs` reads it back the moment this
        // returns, to install the compact-field rows the String-access
        // expansion's two loads need. See `string_access_site_pcs`.
        reset_string_access_sites();
        reset_site_traps_this_build();
        // Published now rather than at the end: it depends only on the plan,
        // and `ir_optimize` reads it only for a graph this build returned.
        set_splice_resume_map_this_build(if self.inline_sites.is_empty() {
            None
        } else {
            Some(IrSpliceResumeMap::from_sites(code_len, &self.inline_sites))
        });
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

        // ── Bottom-tested (rotated) loops ────────────────────────────
        //
        // ECJ, Kotlin, Scala and several bytecode generators rotate a loop:
        // `goto test; body: …; test: if<cond> body`. In pc order the body, a
        // loop header by the verifier's "target of a backward branch"
        // definition, comes before every one of its predecessors. The pc walk
        // below reaches it with no control token, `activate_loop_header` has
        // no entry edge to seed its phis from, and the safety net refused the
        // whole method.
        //
        // Such a method is walked in reverse post-order of its block CFG
        // instead (see `BlockWalk`). A block is then visited after every
        // forward predecessor, the test block before the body it dominates,
        // and the loop headers are the targets of the edges that are
        // retreating in THAT order. Every method the pc walk already handles
        // keeps the pc walk, node for node.
        let block_walk: Option<BlockWalk> =
            if has_rotated_loop_header(&verified, &reachable, &self.loop_headers, code_len) {
                match BlockWalk::reverse_postorder(&verified, &reachable, code_len) {
                    Some(walk) => Some(walk),
                    None => return ir_build_bail(line!(), 0),
                }
            } else {
                None
            };

        // ── The same pre-scan, for every SPLICED body ────────────────
        //
        // `verified_code` above analysed `code[..code_len]` — the caller alone.
        // A relocated callee body lives past `code_len` in the combined buffer
        // and has its own branches, its own merge targets and its own loop
        // headers, none of which that analysis can see. That gap, and only that
        // gap, is why the splice scanner refused every callee containing an
        // `if` or a `goto` and called itself "straight-line only, v1".
        //
        // Closing it needs no new analysis, because a callee body IS a valid
        // method body: run the SAME verifier over it and rebase what comes out
        // by the body's `base`. Branch offsets need no rebasing at all — they
        // are relative, the body is copied contiguously, and a branch inside it
        // therefore lands inside it in combined coordinates by construction.
        //
        // Reachability needs no merging either: the walk's skip is already
        // scoped to `self.splice.is_empty()` (see the `!reachable.contains`
        // test below) precisely because a relocated body is unreachable from
        // pc 0. What a body's own reachability is used for here is the same
        // thing it is used for above — refusing to create an `Op::Merge` for a
        // target only handler code can reach, which would leave an input-less
        // control node in the graph.
        //
        // A `return` anywhere but the last instruction is still refused by the
        // scanner (`ir-splice-not-single-trailing-return`); several returns
        // that all funnel to a trailing one are admitted under
        // `ir_splice_multi_return_enabled`, and the loop below is where the
        // walk learns which bodies those are.
        if ir_splice_branch_enabled() {
            let bodies: Vec<(usize, usize)> = self
                .inline_sites
                .values()
                .map(|s| (s.base, s.code_len))
                .collect();
            for (base, body_len) in bodies {
                let Some(body) = code.get(base..base.saturating_add(body_len)) else {
                    return ir_build_bail(line!(), base);
                };
                let Ok(body_verified) = cratonvm_reader::verified_code(body) else {
                    // The body did not verify on its own. Refuse the METHOD
                    // rather than walk a region whose control flow nothing has
                    // analysed — the failure mode that produces is orphan nodes
                    // referencing `NO_NODE`, which is what STUB-S8 was.
                    return ir_build_bail(line!(), base);
                };
                let body_reachable = normally_reachable_pcs(&body_verified, body_len);
                for &target in body_verified.merge_targets() {
                    let target = target as usize;
                    if body_reachable.contains(&target) {
                        self.ensure_merge(base + target);
                    }
                }
                let body_headers: HashSet<usize> = body_verified
                    .loop_headers()
                    .iter()
                    .map(|&h| h as usize)
                    .filter(|h| body_reachable.contains(h))
                    .collect();
                // A body with a rotated loop gets its own block walk, for the
                // same reason the caller does: walked in pc order, its loop
                // body has no forward entry when the walk reaches it, the
                // safety net fires, and the CALLER's whole build is refused.
                // Its loop headers are then the walk's (the test blocks).
                if has_rotated_loop_header(&body_verified, &body_reachable, &body_headers, body_len)
                {
                    let Some(walk) =
                        BlockWalk::reverse_postorder(&body_verified, &body_reachable, body_len)
                    else {
                        return ir_build_bail(line!(), base);
                    };
                    self.loop_headers
                        .extend(walk.headers.iter().map(|&h| base + h));
                    self.splice_block_walks.insert(
                        base,
                        walk.ranges
                            .iter()
                            .map(|&(s, e)| (base + s, base + e))
                            .collect(),
                    );
                } else {
                    self.loop_headers
                        .extend(body_headers.iter().map(|&h| base + h));
                }

                // A body with several `return`s: the walk must NOT leave the
                // splice at the first one. Decide it here, from the same
                // verified decode, because the walk meets the returns one at a
                // time and cannot tell the first from the last.
                //
                // Only reachable returns count. An unreachable one is code the
                // walk never enters, and counting it would put the body on the
                // continuation path with one exit edge that never arrives —
                // which `finish_multi_return_splice` would refuse, losing a
                // body the straight-through path handles fine.
                if ir_splice_multi_return_enabled() {
                    let mut reachable_returns = 0usize;
                    let mut p = 0usize;
                    while p < body_len {
                        let Some(decoded) = body_verified.instruction_at(p) else {
                            return ir_build_bail(line!(), base + p);
                        };
                        if body_reachable.contains(&p) && matches!(body.get(p), Some(0xac..=0xb1)) {
                            reachable_returns += 1;
                        }
                        p = decoded.next_pc as usize;
                    }
                    if reachable_returns > 1 {
                        self.splice_multi_return.insert(base);
                    }
                }
            }
        }

        // A block walk's loop headers replace the caller's pc-order ones: a
        // rotated body is an ordinary forward merge in reverse post-order, and
        // its test block is the header. A spliced body's headers (at or past
        // `code_len`) are kept, because a relocated body is still walked in pc
        // order.
        let mut range_end = code_len;
        let mut next_range = 0usize;
        if let Some(walk) = block_walk.as_ref() {
            self.loop_headers.retain(|&h| h >= code_len);
            self.loop_headers.extend(walk.headers.iter().copied());
            self.block_walk = true;
            // `BlockWalk::reverse_postorder` returns at least one range, and
            // the first starts at pc 0.
            range_end = walk.ranges[0].1;
            next_range = 1;
        }

        let mut pc = 0;
        // Where the walk stops, and where it goes next.
        //
        // The pc walk stops at `code_len` with no splice open. That is the
        // loop condition this used to be, `pc < code_len ||
        // !self.splice.is_empty()`: inside a splice `pc` addresses a relocated
        // callee body appended AFTER `code_len`, and the walk must keep going
        // until that body returns.
        //
        // The block walk stops when its ranges run out. At the end of each
        // range, `pc` is the instruction after the range's last one. If control
        // is still live, that instruction falls through into `pc`. The walk
        // visits `pc` somewhere else in its order, so the edge is recorded
        // exactly as the merge activation below records a fall-through: as a
        // predecessor, or as a back edge when `pc` was already visited.
        // Control and the abstract frame are then dropped before the jump, the
        // same as after a `goto`. The next range starts at a merge, whose
        // activation restores them from its predecessors.
        //
        // ── The step budget ──────────────────────────────────────────
        //
        // The walk visits each instruction of the caller and of every spliced
        // body once — `pc` advances monotonically within a range, ranges are
        // disjoint, and each splice frame is entered once — so the number of
        // iterations is bounded by the combined buffer's length. It had no
        // *enforced* bound until 2026-09-17, and the profiled always-taken
        // back-edge prune (see `prune_always_taken_branch`) turned that into a
        // compiler thread that never returned, allocating graph nodes on every
        // pass: `IR_MAX_GRAPH_NODES` is checked only after `build` returns, so
        // nothing downstream could stop it either.
        //
        // The budget below costs one increment and one compare per opcode and
        // converts every future control-flow bookkeeping bug of that shape from
        // "the VM hangs" into "this method took the single-pass backend". The
        // factor is deliberately loose — this is a backstop, not a policy. The
        // walk performs at most one iteration per INSTRUCTION and the shortest
        // instruction is one byte, so `4 * len` is four times the loosest legal
        // walk and no real method can approach it.
        let step_budget = code
            .len()
            .max(code_len)
            .saturating_mul(IR_BUILD_STEPS_PER_CODE_BYTE)
            .saturating_add(64);
        let mut steps = 0usize;
        loop {
            steps += 1;
            if steps > step_budget {
                return ir_build_bail(line!(), pc);
            }
            // A helper latched a refusal it had no way to return (a dropped
            // control edge, a second activation of a merge, a φ re-typing that
            // did not converge). Stop at the next instruction boundary, which
            // is the last point the graph is still describable.
            if let Some((site, at)) = self.build_refused {
                return ir_build_bail(site, at);
            }
            if self.splice.is_empty() {
                match block_walk.as_ref() {
                    None => {
                        if pc >= code_len {
                            break;
                        }
                    }
                    Some(walk) => {
                        if pc >= range_end {
                            // An arm moved `pc` past the range's end rather than
                            // onto it. The ranges and the walk disagree about
                            // where instructions end; refuse.
                            if pc != range_end {
                                return ir_build_bail(line!(), pc);
                            }
                            if self.ctrl_opt().is_some() {
                                // Only a merge target can be reached by a
                                // fall-through that the order does not follow:
                                // every other block is placed right after the
                                // block that falls into it.
                                if !self.merges.contains_key(&pc) {
                                    return ir_build_bail(line!(), pc);
                                }
                                self.add_merge_predecessor(pc);
                            }
                            let Some(&(start, end)) = walk.ranges.get(next_range) else {
                                break;
                            };
                            next_range += 1;
                            self.ctrl = NO_NODE;
                            // A local no entry edge initialises gets no loop
                            // phi, so `activate_loop_header` leaves its slot
                            // as it finds it. After a jump that would be a
                            // value from whichever block the order visited
                            // last, which need not dominate this one. Say
                            // "uninitialised" instead.
                            self.locals.fill(NO_NODE);
                            self.stack.clear();
                            pc = start;
                            range_end = end;
                        }
                    }
                }
            }
            // Bounds on the relocated region. A callee body that falls off its
            // own end never executed a return, which means the resolver admitted
            // a shape the walk does not agree with — refuse rather than walk
            // into whatever `lib.rs` appended next.
            // A spliced body with a rotated loop is walked range by range, the
            // same as the caller's block walk above. At a range's end a live
            // fall-through is a merge predecessor. When the ranges run out,
            // every `return` has recorded its exit edge, so the body closes the
            // way a multi-return body does.
            if let Some(frame) = self.splice.last() {
                if !frame.ranges.is_empty() && pc >= frame.range_end {
                    let (range_end, end) = (frame.range_end, frame.end);
                    if pc != range_end {
                        return ir_build_bail(line!(), pc);
                    }
                    if self.ctrl_opt().is_some() {
                        // Live control at the body's end never executed a
                        // `return`; anywhere else it must enter a merge.
                        if pc >= end || !self.merges.contains_key(&pc) {
                            return ir_build_bail(line!(), pc);
                        }
                        self.add_merge_predecessor(pc);
                    }
                    let Some(frame) = self.splice.last_mut() else {
                        return ir_build_bail(line!(), pc);
                    };
                    if let Some((start, next_end)) = frame.ranges.get(frame.next_range).copied() {
                        frame.next_range += 1;
                        frame.range_end = next_end;
                        // As at a caller range jump: the next range starts at a
                        // merge whose activation restores the callee's frame.
                        self.ctrl = NO_NODE;
                        self.locals.fill(NO_NODE);
                        self.stack.clear();
                        pc = start;
                        continue;
                    }
                    if frame.exits.is_empty() {
                        return ir_build_bail(line!(), pc);
                    }
                    match self.finish_multi_return_splice() {
                        Some(next) => {
                            pc = next;
                            continue;
                        }
                        None => return ir_build_bail(line!(), pc),
                    }
                }
            }
            if let Some(frame) = self.splice.last() {
                if pc >= frame.end {
                    // A multi-return body ENDS here rather than falling off:
                    // its last instruction is a `return` (the scanner admits no
                    // other shape) and that return recorded an edge instead of
                    // exiting, so `pc` lands exactly on `end` with every edge
                    // in and `ctrl` dead. Join them and resume the caller.
                    if frame.multi_return && pc == frame.end && !frame.exits.is_empty() {
                        match self.finish_multi_return_splice() {
                            Some(next) => {
                                pc = next;
                                continue;
                            }
                            None => return ir_build_bail(line!(), pc),
                        }
                    }
                    return ir_build_bail(line!(), pc);
                }
            }
            // A relocated body is unreachable from pc 0 by construction, so the
            // REACHABILITY skip — computed over the caller's code alone — must
            // stay scoped to code outside a splice. Merge bookkeeping is a
            // different matter: since 2026-09-09 the pre-scan above registers
            // each spliced body's own merge targets and loop headers, rebased,
            // so a merge inside a splice both exists and must be activated. The
            // gate below is deliberately NOT `self.splice.is_empty()` any more.
            if self.splice.is_empty() && !reachable.contains(&pc) {
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

            // If this PC is a merge target, activate the merge.
            //
            // Inside a splice too. Every predecessor of a merge that the
            // spliced-body pre-scan registered is itself inside that same body,
            // so the locals and operand stack this snapshots are the callee's
            // throughout — the same invariant the caller's own merges rely on.
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
            //
            // One exception: below `pruned_dead_until` dead control is the cold
            // arm of a pruned branch (see `prune_always_taken_branch`), walked
            // as dead code. Step over the instruction exactly as the
            // unreachable-code skip above does. A merge in that arm with a live
            // predecessor was activated just above and does not get here; one
            // whose only predecessors are in the cold arm stays unvisited and
            // input-less, and `audit_unactivated_merges` retires it. A back
            // edge into a skipped header from live code records a predecessor
            // on a merge the walk has passed, which the same audit refuses.
            if self.ctrl_opt().is_none() && self.splice.is_empty() && pc < self.pruned_dead_until {
                pc = match verified.instruction_at(pc) {
                    Some(decoded) => decoded.next_pc as usize,
                    None => return ir_build_bail(line!(), pc),
                };
                continue;
            }
            if self.ctrl_opt().is_none() {
                return ir_build_bail(line!(), pc);
            }
            // Two edges into a join held different monitors: there is no one
            // monitor state to record for this bci. See `unstructured_monitors`.
            if self.unstructured_monitors {
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
            //
            // Never inside a splice: a snapshot there would name a
            // combined-buffer pc, and `build_deopt_points` turns EVERY snapshot
            // into a `DeoptimizationPoint` whose `bci` the VM parks the
            // interpreter at. The spliced region is covered instead by the
            // caller's snapshot at the `invoke` pc, which every node in the
            // region is re-stamped with — see [`IrInlineSite`].
            if self.splice.is_empty() && self.ctrl_opt().is_some() {
                self.graph.push_safepoint(SafepointSnapshot {
                    bci: pc,
                    locals: self.locals.clone(),
                    stack: self.stack.clone(),
                    monitors: self.monitors.clone(),
                });
            }

            let op = code[pc];
            // A method-exit `return` while this frame still holds a monitor is
            // unstructured locking (JVMS §2.11.10): the interpreter would throw
            // `IllegalMonitorStateException` there, and no frame state this
            // builder records could describe the lock being dropped. Refuse.
            // Inside a splice a return hands a value back to the caller, whose
            // monitors are legitimately still held.
            if self.splice.is_empty() && matches!(op, 0xac..=0xb1) && !self.monitors.is_empty() {
                return ir_build_bail(line!(), pc);
            }
            match op {
                // nop — do nothing, advance one byte.
                //
                // Free coverage that was missing until 2026-09-17, and a real
                // divergence: `jit_scan` accepts `nop` (`x64/bytecode_compat.rs`
                // admits 0x00), the single-pass tier implements it
                // (`x64/op_local_stack.rs`), and `ir_compatible` says nothing
                // about it — so a `nop`-bearing method was admitted to this
                // tier, built all the way to the `nop`, and then refused at the
                // `_ =>` catch-all after a full partial build. That is exactly
                // the pattern `ir_compatible`'s own comment condemns: "A method
                // the builder will refuse must be refused HERE, cheaply and
                // with a reason, not after a full graph build."
                //
                // It is not a rare opcode in practice: ASM-based rewriters emit
                // `nop` as padding when they shorten an instruction, and
                // several agent/instrumentation frameworks and obfuscators
                // leave them behind. This arm ADMITS those methods to the
                // optimizing tier for the first time.
                0x00 => {
                    pc += 1;
                }
                // aconst_null — push the null reference.
                //
                // Absent until 2026-07-31, and it cost more than its size
                // suggests: `BinTreesClassic.bottomUpTree` was refused at its
                // first `aconst_null`, and a null literal is unavoidable in any
                // method that builds or terminates a reference structure. See
                // `Self::aconst_null` for why the node is `Ref`-typed.
                0x01 => {
                    let c = self.aconst_null();
                    self.push(c);
                    pc += 1;
                }
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
                    if !self.push_local(idx) {
                        return ir_build_bail(line!(), pc);
                    }
                    pc += 2;
                }
                // iload_0..3
                0x1a..=0x1d => {
                    let idx = (op - 0x1a) as usize;
                    if !self.push_local(idx) {
                        return ir_build_bail(line!(), pc);
                    }
                    pc += 1;
                }
                // lload_0..3
                0x1e..=0x21 => {
                    let idx = (op - 0x1e) as usize;
                    if !self.push_local(idx) {
                        return ir_build_bail(line!(), pc);
                    }
                    pc += 1;
                }
                // lload (wide index) — inc 26. A long is one NodeId slot; the
                // value lives at `locals[idx]` (the high half slot `idx+1` is
                // never read by valid bytecode). Long-only, so inert for the int
                // path.
                0x16 => {
                    let idx = code[pc + 1] as usize;
                    if !self.push_local(idx) {
                        return ir_build_bail(line!(), pc);
                    }
                    pc += 2;
                }
                // aload — push an object reference local (same node-graph
                // mechanics as iload: a reference is just a NodeId on the
                // abstract stack; its only consumer in the IR slice we lower is
                // a `getfield` base).
                0x19 => {
                    let idx = code[pc + 1] as usize;
                    if !self.push_local(idx) {
                        return ir_build_bail(line!(), pc);
                    }
                    pc += 2;
                }
                // aload_0..3
                0x2a..=0x2d => {
                    let idx = (op - 0x2a) as usize;
                    if !self.push_local(idx) {
                        return ir_build_bail(line!(), pc);
                    }
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
                    self.store_local(idx, val);
                    pc += 2;
                }
                // astore_0..3
                0x4b..=0x4e => {
                    let idx = (op - 0x4b) as usize;
                    let val = self.pop();
                    self.store_local(idx, val);
                    pc += 1;
                }
                // istore
                0x36 => {
                    let idx = code[pc + 1] as usize;
                    let val = self.pop();
                    self.store_local(idx, val);
                    pc += 2;
                }
                // istore_0..3
                0x3b..=0x3e => {
                    let idx = (op - 0x3b) as usize;
                    let val = self.pop();
                    self.store_local(idx, val);
                    pc += 1;
                }
                // lstore_0..3
                0x3f..=0x42 => {
                    let idx = (op - 0x3f) as usize;
                    let val = self.pop();
                    self.store_local(idx, val);
                    pc += 1;
                }
                // lstore (wide index) — inc 26. Stores the long NodeId at
                // `locals[idx]`; `store_local` marks the high half slot `idx+1`
                // uninitialised (JVMS §2.6.1). Long-only.
                0x37 => {
                    let idx = code[pc + 1] as usize;
                    let val = self.pop();
                    self.store_local(idx, val);
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
                    let g = self.add_div_zero_guard(b, IrType::Int, pc);
                    let r =
                        self.add_data(Op::Div, IrType::Int, with_guard_token(vec![a, b], g), pc);
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
                    let g = self.add_div_zero_guard(b, IrType::Long, pc);
                    let r =
                        self.add_data(Op::Div, IrType::Long, with_guard_token(vec![a, b], g), pc);
                    self.push(r);
                    pc += 1;
                }
                // irem
                0x70 => {
                    let b = self.pop();
                    let a = self.pop();
                    let g = self.add_div_zero_guard(b, IrType::Int, pc);
                    let r =
                        self.add_data(Op::Rem, IrType::Int, with_guard_token(vec![a, b], g), pc);
                    self.push(r);
                    pc += 1;
                }
                // lrem — see `ldiv`: the lowerer handles the 64-bit `Long` path.
                0x71 => {
                    let b = self.pop();
                    let a = self.pop();
                    let g = self.add_div_zero_guard(b, IrType::Long, pc);
                    let r =
                        self.add_data(Op::Rem, IrType::Long, with_guard_token(vec![a, b], g), pc);
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
                // wide — the 16-bit-index forms of the local loads, local
                // stores and `iinc`. Same node-graph mechanics as the narrow
                // arms (a load pushes the local's node, a store replaces it,
                // `iinc` adds an `Int` constant); only the operand width
                // differs. `jit_scan` accepted `wide`, so `ir_compatible`
                // admitted its methods and the build refused them at the
                // catch-all. `wide ret` stays refused, like `ret`.
                0xc4 => {
                    if pc + 3 >= code.len() {
                        return ir_build_bail(line!(), pc);
                    }
                    let sub = code[pc + 1];
                    let idx = u16::from_be_bytes([code[pc + 2], code[pc + 3]]) as usize;
                    if idx >= self.locals.len() {
                        return ir_build_bail(line!(), pc);
                    }
                    match sub {
                        // iload lload fload dload aload
                        0x15..=0x19 => {
                            if !self.push_local(idx) {
                                return ir_build_bail(line!(), pc);
                            }
                            pc += 4;
                        }
                        // istore lstore fstore dstore astore
                        0x36..=0x3a => {
                            let val = self.pop();
                            self.store_local(idx, val);
                            pc += 4;
                        }
                        // iinc with a 16-bit signed increment
                        0x84 => {
                            if pc + 5 >= code.len() {
                                return ir_build_bail(line!(), pc);
                            }
                            let inc = i64::from(i16::from_be_bytes([code[pc + 4], code[pc + 5]]));
                            let old = self.locals[idx];
                            let c = self.iconst(inc);
                            let r = self.add_data(Op::Add, IrType::Int, vec![old, c], pc);
                            self.store_local(idx, r);
                            pc += 6;
                        }
                        _ => return ir_build_bail(line!(), pc),
                    }
                }
                // iinc
                0x84 => {
                    let idx = code[pc + 1] as usize;
                    let inc = code[pc + 2] as i8 as i64;
                    let Some(old) = self.locals.get(idx).copied() else {
                        return ir_build_bail(line!(), pc);
                    };
                    let c = self.iconst(inc);
                    let r = self.add_data(Op::Add, IrType::Int, vec![old, c], pc);
                    self.store_local(idx, r);
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
                // (cat-2, exactly like `long` — `store_local` marks the high-half
                // slot uninitialised).
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
                // `locals[idx]` (a double's high-half slot `idx+1` is never read).
                // fload (wide index)
                0x17 => {
                    let idx = code[pc + 1] as usize;
                    if !self.push_local(idx) {
                        return ir_build_bail(line!(), pc);
                    }
                    pc += 2;
                }
                // dload (wide index)
                0x18 => {
                    let idx = code[pc + 1] as usize;
                    if !self.push_local(idx) {
                        return ir_build_bail(line!(), pc);
                    }
                    pc += 2;
                }
                // fload_0..3
                0x22..=0x25 => {
                    let idx = (op - 0x22) as usize;
                    if !self.push_local(idx) {
                        return ir_build_bail(line!(), pc);
                    }
                    pc += 1;
                }
                // dload_0..3
                0x26..=0x29 => {
                    let idx = (op - 0x26) as usize;
                    if !self.push_local(idx) {
                        return ir_build_bail(line!(), pc);
                    }
                    pc += 1;
                }
                // fstore / dstore — write a FP local (same mechanics as
                // `istore`/`lstore`; `store_local` marks a double's high-half
                // slot `idx+1` uninitialised).
                // fstore (wide index)
                0x38 => {
                    let idx = code[pc + 1] as usize;
                    let val = self.pop();
                    self.store_local(idx, val);
                    pc += 2;
                }
                // dstore (wide index)
                0x39 => {
                    let idx = code[pc + 1] as usize;
                    let val = self.pop();
                    self.store_local(idx, val);
                    pc += 2;
                }
                // fstore_0..3
                0x43..=0x46 => {
                    let idx = (op - 0x43) as usize;
                    let val = self.pop();
                    self.store_local(idx, val);
                    pc += 1;
                }
                // dstore_0..3
                0x47..=0x4a => {
                    let idx = (op - 0x47) as usize;
                    let val = self.pop();
                    self.store_local(idx, val);
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
                //
                // Inside a splice both are held to the rule every other store
                // arm keeps — `splice_store_is_local` — and until 2026-09-18
                // these two were the exception. The splice scanner in
                // `jit_bridge.rs` admits `0x4f..=0x56` in IR mode, fastore and
                // dastore included, so a spliced `a[i] = x` into a CALLER'S
                // `float[]`/`double[]` was built, and a deopt later in the same
                // spliced region re-executed the whole call and committed the
                // write twice (`a[i] += 1.0f` would add two).
                0x51 => {
                    let value = self.pop();
                    let index = self.pop();
                    let array = self.pop();
                    if !self.splice.is_empty() && !self.splice_store_is_local(array) {
                        return ir_build_bail(line!(), pc);
                    }
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
                    if !self.splice.is_empty() && !self.splice_store_is_local(array) {
                        return ir_build_bail(line!(), pc);
                    }
                    let store = self.graph.add(
                        Op::ArrayStore(MemKind::Double),
                        IrType::Memory,
                        vec![self.ctrl, self.mem, array, index, value],
                        Some(pc),
                    );
                    self.mem = store;
                    pc += 1;
                }

                // ── COV-02: the integral and reference array element access ──
                //
                // `faload`/`daload`/`fastore`/`dastore` above are the arms an FP
                // benchmark kernel needed. Nobody decided that a `float[]`
                // element should be lowerable and an `int[]` element should
                // not; the survey in `docs/known-issues/c2/` measured 77 events
                // on the missing arms, second-largest opcode bucket, and the
                // *shape* is the argument rather than the number. These arms
                // are the same node with a different [`MemKind`]: one element
                // width, one sign/zero-extension rule, the same JVMS null +
                // bounds guards, the same memory token.
                //
                // iaload / laload / aaload / baload / caload / saload.
                // (`baload` covers `boolean[]` too — one byte either way.)
                //
                // `aaload` is a REFERENCE load: the node is typed
                // `IrType::Ref`, so `ir_lower::emit_safepoint_map`'s scan
                // publishes its spill slot as a rewritable root at every later
                // safepoint. Typing it `Int` would compile — and lose the
                // element across the first relocating collection.
                //
                // `aastore` is deliberately NOT here (see the store arm below).
                0x2e | 0x2f | 0x32 | 0x33 | 0x34 | 0x35 => {
                    let (kind, ty) = match op {
                        0x2e => (MemKind::Int, IrType::Int),
                        0x2f => (MemKind::Long, IrType::Long),
                        0x32 => (MemKind::Ref, IrType::Ref),
                        0x33 => (MemKind::Byte, IrType::Int),
                        0x34 => (MemKind::Char, IrType::Int),
                        // 0x35 saload
                        _ => (MemKind::Short, IrType::Int),
                    };
                    let index = self.pop();
                    let array = self.pop();
                    let load = self.graph.add(
                        Op::ArrayLoad(kind),
                        ty,
                        vec![self.ctrl, self.mem, array, index],
                        Some(pc),
                    );
                    self.note_splice_local_load(load, array);
                    self.mem = load;
                    self.push(load);
                    pc += 1;
                }
                // iastore / lastore / bastore / castore / sastore.
                //
                // `aastore` (0x53) is IN scope as of 2026-09-06, and the
                // paragraph that used to stand here is worth keeping because
                // its reasoning is still correct -- it just argued for the
                // wrong conclusion. It said a reference element store needs the
                // SATB pre-write barrier and the card mark the single-pass
                // backend emits around `emit_ref_astore_regs`, that a missing
                // barrier is invisible until a concurrent collection drops the
                // only path to an overwritten-but-live target, and that
                // refusing the method is the cheap answer.
                //
                // All true. What it missed is that emitting the store INLINE is
                // not the only alternative to refusing: `jit_aastore` owns the
                // covariance check and both barriers, and the single-pass
                // backend already calls it as its own escape hatch under the
                // ZGC barrier gate. So this arm builds the node and
                // `ir_lower` calls that helper -- no barrier contract moves
                // into this tier, which is the property the old paragraph was
                // really protecting.
                //
                // Measured: **35 methods refused on the H2 JDBC workload**, the
                // second-largest build refusal there after the invoke plan.
                0x4f | 0x50 | 0x53 | 0x54 | 0x55 | 0x56 => {
                    let kind = match op {
                        0x4f => MemKind::Int,
                        0x50 => MemKind::Long,
                        0x53 => MemKind::Ref,
                        0x54 => MemKind::Byte,
                        0x55 => MemKind::Char,
                        // 0x56 sastore
                        _ => MemKind::Short,
                    };
                    let value = self.pop();
                    let index = self.pop();
                    let array = self.pop();
                    // See `splice_store_is_local`: a write inside a spliced body
                    // may be re-executed, so it must target an array the splice
                    // itself made.
                    if !self.splice.is_empty() && !self.splice_store_is_local(array) {
                        return ir_build_bail(line!(), pc);
                    }
                    let store = self.graph.add(
                        Op::ArrayStore(kind),
                        IrType::Memory,
                        vec![self.ctrl, self.mem, array, index, value],
                        Some(pc),
                    );
                    self.mem = store;
                    pc += 1;
                }
                // arraylength — the array's immutable length word.
                //
                // The cheapest thing in this lane: one 32-bit load at a fixed
                // header offset behind a null check. No element type, no bounds
                // check, no barrier.
                //
                // It does NOT advance the memory token. Nothing in the JVM
                // writes an array's length, so `AliasClass::ArrayLength` can
                // never conflict with a write (`Graph::may_alias`), and putting
                // a pure read in the token chain would serialise every store
                // around it for nothing. The token is still an *input*, which is
                // what pins the node behind the writes that produced the array.
                0xbe => {
                    let array = self.pop();
                    let len = self.graph.add(
                        Op::ArrayLength,
                        IrType::Int,
                        vec![self.ctrl, self.mem, array],
                        Some(pc),
                    );
                    self.push(len);
                    pc += 1;
                }
                // athrow — cov-07. Pop the exception ref and terminate the
                // block: this never falls through and produces no value,
                // exactly like `ireturn`/`areturn` above — a later bytecode pc
                // is reachable only via a real branch target (goto/if), never
                // by falling out of this arm.
                //
                // Lowered as a call to `helpers.throw_exception` with THIS
                // instruction's own bci baked in as an immediate — the SAME
                // `(exc_ptr, bci)` call the single-pass backend's `0xbf` arm
                // makes (`x64/bytecode_walk.rs`), so handler resolution reuses
                // the exact machinery a callee-thrown or `checkcast`-thrown
                // exception already goes through:
                // `route_jit_exception_through_method`
                // (`vm/src/runtime/interpreter/exception_dispatch.rs`), run in
                // the INTERPRETER after the compiled frame exits via the
                // shared `i64::MIN` sentinel. See `Op::Throw`'s doc comment
                // for why this is not a second, divergent answer to where an
                // exception goes.
                0xbf => {
                    // Symmetry with the `return` refusal at the top of this
                    // loop, which `athrow` lacked until 2026-09-17.
                    //
                    // `Op::Throw` lowers to `helpers.throw_exception` plus the
                    // shared `i64::MIN` sentinel: the compiled frame *returns*,
                    // and the interpreter resolves the handler afterwards.
                    // Nothing on that path releases a monitor this frame took,
                    // so a `synchronized` region exited by an exception would
                    // leak the lock. Unlike `return`, the splice case is not an
                    // exception: a `return` inside a spliced callee hands a
                    // value back to the caller, whose monitors are legitimately
                    // still held, whereas an `athrow` leaves the WHOLE compiled
                    // frame no matter how deeply spliced it is — so this checks
                    // `self.monitors` unconditionally.
                    //
                    // The method was already excluded upstream, but only by
                    // accident: `precise_exception_frames` refuses it because a
                    // `synchronized` block always carries a synthetic
                    // `astore exc; aload mon; monitorexit; aload exc; athrow`
                    // handler that reads non-parameter locals. That is an
                    // accidental protection, not a stated one, and it
                    // evaporates the moment that gate widens. Stating it costs
                    // one comparison.
                    if !self.monitors.is_empty() {
                        return ir_build_bail(line!(), pc);
                    }
                    let exc = self.pop();
                    let throw = self.graph.add(
                        Op::Throw,
                        IrType::Void,
                        vec![self.ctrl, self.mem, exc],
                        Some(pc),
                    );
                    self.graph.exit = throw;
                    self.ctrl = NO_NODE;
                    pc += 1;
                }
                // checkcast / instanceof — cov-05. Both admitted ONLY for a
                // site whose target class is already resolved and loaded at
                // compile time: `checkcast_info`/`instanceof_info` are
                // populated in `lib.rs`'s `try_compile_inner` (reusing
                // `cp_new_resolver`, the same CONSTANT_Class resolver
                // `new`/`anewarray` already use, to answer "is it loaded" —
                // see that call site). A pc absent from the map — not yet
                // loaded, an unresolvable CP entry, or no resolver supplied at
                // all — bails this site (and so this whole method, same as
                // every other per-pc bail in this builder) to single-pass,
                // which resolves lazily via `jit_typecheck_resolve`'s
                // not-yet-loaded slow path — a path that can run a user
                // classloader's `loadClass`/`findClass`, arbitrary Java this
                // tier does not host inside a helper call.
                0xc0 => {
                    let (name_ptr, name_len) = match self.checkcast_info.get(&pc) {
                        Some(&info) => info,
                        // The named class is not loaded, so no path that has
                        // ever executed reached this `checkcast` -- trap here
                        // and compile the rest. `plant_uncommon_trap` carries
                        // the argument; the operand is replaced by a null so
                        // the abstract stack keeps the depth the verifier
                        // proved, and the code that reads it is unreachable.
                        None => {
                            if !self.plant_uncommon_trap(
                                code,
                                code_len,
                                pc,
                                TrapCause::UnresolvedTypeCheck,
                            ) {
                                return ir_build_bail(line!(), pc);
                            }
                            let _obj = self.pop();
                            // A `Ref`-typed null, not `iconst(0)`: the value
                            // this arm replaces is a reference, and a
                            // mistyped stack entry would be joined against a
                            // real reference at the next merge.
                            let null = self.aconst_null();
                            self.push(null);
                            pc += 3;
                            continue;
                        }
                    };
                    let obj = self.pop();
                    let result = self.graph.add(
                        Op::CheckCast { name_ptr, name_len },
                        IrType::Ref,
                        vec![self.ctrl, self.mem, obj],
                        Some(pc),
                    );
                    // Opaque: same reasoning as `instanceof` below, plus a
                    // definitive refusal stashes a pending ClassCastException
                    // and returns the deopt sentinel — see `Op::CheckCast`.
                    self.mem = result;
                    self.push(result);
                    pc += 3;
                }
                0xc1 => {
                    let (name_ptr, name_len) = match self.instanceof_info.get(&pc) {
                        Some(&info) => info,
                        // Same argument as the `checkcast` arm above: an
                        // unloaded class has been reached by nothing.
                        None => {
                            if !self.plant_uncommon_trap(
                                code,
                                code_len,
                                pc,
                                TrapCause::UnresolvedTypeCheck,
                            ) {
                                return ir_build_bail(line!(), pc);
                            }
                            let _obj = self.pop();
                            let zero = self.iconst(0);
                            self.push(zero);
                            pc += 3;
                            continue;
                        }
                    };
                    let obj = self.pop();
                    let result = self.graph.add(
                        Op::InstanceOf { name_ptr, name_len },
                        IrType::Int,
                        vec![self.ctrl, self.mem, obj],
                        Some(pc),
                    );
                    // Opaque: the helper can still allocate a `java/lang/Class`
                    // mirror on first touch even though the target class is
                    // already loaded — see `Op::InstanceOf`'s doc comment.
                    self.mem = result;
                    self.push(result);
                    pc += 3;
                }
                // dup_x1 — insert a copy of the top value below the second.
                //
                // A stack shuffle rather than an access; it is in this lane only
                // because it appears in the same method bodies (six events).
                // The abstract stack holds one entry per VALUE, so the shuffle
                // is three pushes — but only if BOTH operands are category 1,
                // which is what JVMS §dup_x1 requires. A category-2 operand
                // would mean this builder's one-entry-per-value stack and the
                // verifier's two-slot stack disagree about what "the value
                // below" names, so refuse rather than shuffle the wrong entry.
                0x5a => {
                    let (Some(v1), Some(v2)) = (self.pop_opt(), self.pop_opt()) else {
                        return ir_build_bail(line!(), pc);
                    };
                    let is_cat2 = |ty: IrType| matches!(ty, IrType::Long | IrType::Double);
                    if is_cat2(self.graph.nodes[v1 as usize].ty)
                        || is_cat2(self.graph.nodes[v2 as usize].ty)
                    {
                        return ir_build_bail(line!(), pc);
                    }
                    self.push(v1);
                    self.push(v2);
                    self.push(v1);
                    pc += 1;
                }
                // getfield — read an instance field as an `Op::Load`.
                //
                // Slice 1 (read-only) of the field/call IR frontier. An
                // int-category field (`I`/`Z`/`B`/`C`/`S`) reads the 32-bit
                // `Value::Int` payload sign-extended — the exact ABI the
                // single-pass backend's inline getfield emits (`MOVSXD` from
                // `HEADER_SIZE + field_index*SLOT_SIZE +
                // FIELD_CELL_PAYLOAD32_OFFSET`). A reference field reads the raw
                // pointer, and (COV-03) a `J`/`F`/`D` field the long payload or
                // the FP bit pattern, both through the checked helper. The
                // cell-offset operand is a `Const(field_index)`; the lowerer
                // derives the byte displacement. A pc without resolved layout
                // bails (`None` → single-pass).
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
                    // (jit-ir-relocation-map-contract.md).
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
                    //
                    // A VOLATILE field is refused unless admitted: a hoisted or
                    // merged read of a volatile flag is a spin loop that never
                    // ends. Admitted, the load is marked and every pass orders
                    // it. See `set_volatile_field_pcs`.
                    let is_volatile = self.volatile_field_pcs.contains(&pc);
                    if is_volatile && !self.volatile_loads_admitted {
                        return ir_build_bail(line!(), pc);
                    }
                    let (field_index, type_tag) = match self.field_info.get(&pc) {
                        Some(&fi) => fi,
                        None => return ir_build_bail(line!(), pc),
                    };
                    // Reference fields (`L…;` / `[…`) read through the SAME
                    // `jit_getfield` helper: it already returns the raw pointer
                    // for a compact reference slot (0 for null, and an
                    // implausible word degraded to null), and `i64::MIN` stays
                    // unambiguous because no plausible heap pointer can equal
                    // it. Only the node type differs — `Ref`, so every
                    // safepoint map publishes the result slot as a rewritable
                    // root. `ir_lower::lower_inner` refuses the graph when the
                    // helper is absent (the unit-test stub table), because the
                    // inline displacement fallback decodes an int cell.
                    //
                    // This was the single largest builder refusal measured on a
                    // real workload (66 of 141 on a Hibernate class): a
                    // reference field read is most of what object-oriented Java
                    // does.
                    // COV-03 (wide fields): a `J`/`F`/`D` field reads through
                    // the SAME helper — it already returns the long payload or
                    // the float/double BITS — so the only thing that differs is
                    // the node type and, for `J`/`D`, the `i64::MIN`-sentinel
                    // collision the lowerer must disambiguate through
                    // `jit_dispatch_threw` (the identical problem, and the
                    // identical answer, as a `J`/`D` `Op::Call` return). The
                    // inline compact fast path refuses these tags on its own, so
                    // a wide field simply takes the helper arm.
                    let Some(ty) = self.field_access_type(type_tag) else {
                        return ir_build_bail(line!(), pc);
                    };
                    let base = self.pop();
                    let offset = self.iconst(field_index as i64);
                    let mem_kind = match ty {
                        IrType::Ref => MemKind::Ref,
                        IrType::Long => MemKind::Long,
                        IrType::Float => MemKind::Float,
                        IrType::Double => MemKind::Double,
                        _ => MemKind::Int,
                    };
                    let load = self.graph.add(
                        Op::Load(mem_kind),
                        ty,
                        vec![self.ctrl, self.mem, base, offset],
                        Some(pc),
                    );
                    if is_volatile {
                        self.graph.set_volatile_access(load);
                    }
                    self.note_splice_local_load(load, base);
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
                // Slice 2 (read/write) of the field/call IR frontier. The store
                // consumes the current memory token and produces a new one (the
                // store node itself), so the scheduler serialises it after every
                // prior memory op and before every later one (RAW/WAR/WAW all
                // preserved by the input-edge topological sort). Any pc without
                // resolved layout bails (`None` → single-pass).
                0xb5 => {
                    // See the getfield (0xb4) note. This arm accepts exactly the
                    // tags that arm accepts — one classifier, two call sites —
                    // and the DIFFERENCE between a read and a write is carried
                    // entirely by `MemKind`, which selects the lowering:
                    //
                    //   * `Int`  → the inline store, or `jit_putfield_int` when
                    //     compact layout is on (an int cell has no barrier).
                    //   * `Ref`  → ALWAYS `jit_putfield_object`, which is the
                    //     single-pass backend's own full-barrier route: SATB
                    //     pre-barrier on the OLD reference, then the collector's
                    //     post-write barrier (G1's remembered-set edge included).
                    //     This — not the compact layout — is the reason the
                    //     reference case was missing here while `getfield` had
                    //     it: a reference LOAD needs no barrier, so the fix that
                    //     added it there had nothing to say about this arm.
                    //     `jit_putfield_object` is compact-aware in its own
                    //     right, so there is no separate compact refusal to make.
                    //   * `Long`/`Float`/`Double` → the matching
                    //     `jit_putfield_{long,float,double}` helper, gated on the
                    //     same long/FP flags the read arm uses.
                    //
                    // `ir_lower::lower_inner` refuses the graph when the helper a
                    // given kind needs is absent (the unit-test stub table).
                    //
                    // A VOLATILE field is refused unless admitted, which needs
                    // the lowerer's StoreLoad fence as well as the flag
                    // (`IR_LOWER_FENCES_VOLATILE_STORES`). See
                    // `set_volatile_field_pcs`.
                    let is_volatile = self.volatile_field_pcs.contains(&pc);
                    if is_volatile && !self.volatile_stores_admitted {
                        return ir_build_bail(line!(), pc);
                    }
                    let (field_index, type_tag) = match self.field_info.get(&pc) {
                        Some(&fi) => fi,
                        None => return ir_build_bail(line!(), pc),
                    };
                    let Some(ty) = self.field_access_type(type_tag) else {
                        return ir_build_bail(line!(), pc);
                    };
                    let mem_kind = match ty {
                        IrType::Ref => MemKind::Ref,
                        IrType::Long => MemKind::Long,
                        IrType::Float => MemKind::Float,
                        IrType::Double => MemKind::Double,
                        _ => MemKind::Int,
                    };
                    let value = self.pop();
                    let base = self.pop();
                    // A `byte`/`short`/`char`/`boolean` field holds the value
                    // NARROWED to its type (JVMS §6.5 putfield; the
                    // interpreter's `narrow_int_to_field_type`). Every such
                    // field is `MemKind::Int` here, so without this the store
                    // wrote the whole int and IR scalar replacement forwarded
                    // it to the next `getfield` unchanged: `sipush 300;
                    // putfield B; getfield B` read 300 compiled and 44
                    // interpreted (round 9 wave 2,
                    // `sub-int-field-stores-are-not-narrowed-by-compiled-code`).
                    // Narrowing the VALUE covers the store, the forwarded load
                    // and any snapshot that names it, with no new `MemKind`.
                    let value = self.narrow_to_field_type(value, type_tag, pc);
                    // See `splice_store_is_local`.
                    if !self.splice.is_empty() {
                        if !self.splice_store_is_local(base) {
                            return ir_build_bail(line!(), pc);
                        }
                        self.note_splice_store_value(base, value);
                    }
                    let offset = self.iconst(field_index as i64);
                    let store = self.graph.add(
                        Op::Store(mem_kind),
                        IrType::Memory,
                        vec![self.ctrl, self.mem, base, offset, value],
                        Some(pc),
                    );
                    if is_volatile {
                        self.graph.set_volatile_access(store);
                    }
                    self.mem = store;
                    pc += 3;
                }
                // monitorenter / monitorexit — emitted so escape analysis can
                // see the lock and offer an elision or coarsening plan. Both
                // advance the memory token: they are ordering barriers with JMM
                // acquire/release semantics and both are safepoints, so nothing
                // may be reordered across them.
                //
                // `ir_lower` refuses the graph when the helper table carries no
                // monitor entry, so a backend that cannot lower these declines
                // the method rather than silently emitting nothing.
                //
                // Both also maintain the abstract monitor stack every snapshot
                // copies. `monitorenter` pushes the locked reference;
                // `monitorexit` must release the innermost one, and the build is
                // refused when it names anything else (unstructured locking, or
                // a shape the frame state could not describe). A splice never
                // takes a monitor: the resolver admits no callee body with
                // `monitorenter`, and a spliced region's frame is the caller's
                // snapshot at the `invoke`, which could not describe one.
                0xc2 | 0xc3 => {
                    let obj = self.pop();
                    if obj == NO_NODE || !self.splice.is_empty() {
                        return ir_build_bail(line!(), pc);
                    }
                    if code[pc] == 0xc2 {
                        self.monitors.push(obj);
                    } else {
                        let matches_innermost = self
                            .monitors
                            .last()
                            .is_some_and(|&held| self.same_monitor_object(held, obj));
                        if !matches_innermost {
                            return ir_build_bail(line!(), pc);
                        }
                        self.monitors.pop();
                    }
                    let op = if code[pc] == 0xc2 {
                        Op::MonitorEnter
                    } else {
                        Op::MonitorExit
                    };
                    let mon = self.graph.add(
                        op,
                        IrType::Memory,
                        vec![self.ctrl, self.mem, obj],
                        Some(pc),
                    );
                    self.mem = mon;
                    pc += 1;
                }
                // new — allocate an object as an `Op::New`. Emitted so escape
                // analysis can scalar-replace it when it does not escape (no
                // heap allocation, fields become SSA values). An escaping
                // allocation survives optimization and is emitted through the
                // shared compact-layout/TLAB-aware runtime lowering stub.
                0xbb => {
                    let (class_id, num_fields) = match self.new_info.get(&pc) {
                        Some(&ci) => ci,
                        // `JitNewSite::Deferred`: the class is not loaded, so
                        // nothing has ever run this `new`. Trap and keep the
                        // method. Note the `<init>` that follows names the same
                        // unloaded class and so has no `invoke_info` either --
                        // it takes the unplanned-invoke trap for the same
                        // reason, and the two together are what actually saves
                        // the method. Item 7's look budget is the other half of
                        // this story: it stops the retry sweep re-resolving a
                        // site that traps perfectly well.
                        None => {
                            if !self.plant_uncommon_trap(
                                code,
                                code_len,
                                pc,
                                TrapCause::UnresolvedNew,
                            ) {
                                return ir_build_bail(line!(), pc);
                            }
                            let null = self.aconst_null();
                            self.push(null);
                            pc += 3;
                            continue;
                        }
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
                    self.note_splice_local_alloc(newobj);
                    self.push(newobj);
                    pc += 3;
                }
                // newarray — allocate a PRIMITIVE array (cov-06, first
                // increment). No class resolution, no deferred path: the
                // atype immediate names the element kind directly, so this
                // proves the `Op::NewArray` shape with nothing else attached.
                // Escape analysis already refuses to scalar-replace it (only
                // `Op::New` is eligible — see `escape_analysis.rs`), so a
                // `newarray` always survives to `ir_lower`'s shared
                // allocation stub, which calls the same `jit_newarray` helper
                // the single-pass backend's 0xbc arm does.
                0xbc => {
                    let atype = code[pc + 1];
                    let len = self.pop();
                    let arr = self.graph.add(
                        Op::NewArray {
                            element_type: atype,
                            component_class_id: 0,
                        },
                        IrType::Ref,
                        vec![self.ctrl, self.mem, len],
                        Some(pc),
                    );
                    self.note_splice_local_alloc(arr);
                    self.push(arr);
                    pc += 2;
                }
                // anewarray — allocate a REFERENCE array of an
                // already-loaded component class (cov-06, second increment).
                // `element_type: 0` is not a valid JVM atype (they start at
                // 4), so it doubles as the "this is a reference array"
                // discriminant `ir_lower` and `jit_anewarray_object` share.
                //
                // A `Deferred` site (component class not loaded at compile
                // time) has no entry in `anewarray_info` — `lib.rs` never
                // inserts one — so it bails this method to single-pass here,
                // exactly like an unresolved `new`. `ir_compatible` no longer
                // refuses every `anewarray`-bearing method outright; only the
                // per-site resolution gates it, the same model `new` already
                // uses.
                0xbd => {
                    let component_class_id = match self.anewarray_info.get(&pc) {
                        Some(&id) => id,
                        None => return ir_build_bail(line!(), pc),
                    };
                    let len = self.pop();
                    let arr = self.graph.add(
                        Op::NewArray {
                            element_type: 0,
                            component_class_id,
                        },
                        IrType::Ref,
                        vec![self.ctrl, self.mem, len],
                        Some(pc),
                    );
                    self.note_splice_local_alloc(arr);
                    self.push(arr);
                    pc += 3;
                }
                // invokespecial — two lowerings, tried in this order:
                //
                //  1. ELISION of a trivial `<init>()V` on a fresh object: pop
                //     the `dup`'d receiver and emit nothing. Sound only because
                //     the caller admits the pc to `trivial_init_pcs` exclusively
                //     when the object is a `new` whose class has no primitive
                //     field initialisers (its fields are zero-initialised and
                //     set by the visible `putfield`s — the constructor adds
                //     nothing the scalar-replaced slots don't already model),
                //     AND because the receiver is a fresh `Op::New` this pass
                //     emitted. Preferred when it applies: it is the whole point
                //     of scalar replacement, and a call here would pin the
                //     allocation (`Op::Call` arg-escapes its reference inputs).
                //
                //  2. an ordinary statically-bound CALL — a `super.m()`, a
                //     private method, or (cov-04) a real constructor: a
                //     `super(...)`/`this(...)` chain call, or the `<init>` of a
                //     `new` the elision analysis declined.
                //
                // cov-04 measured this arm on the three Spring Boot workloads
                // that carry 1,947 of the survey's 1,954 compile requests, at
                // `48fba3a31` — **every** refusal here was an `<init>`, and
                // **none** was a non-`<init>` `invokespecial` the arm had no
                // path for. 29 of 52 were a `super(...)`/`this(...)` chain call
                // in a compiled constructor (receiver `this`, no `new` in the
                // method at all) and 23 were a `new X(args)` site. So the
                // ordering above is not a preference between two common cases —
                // case 1 is rare (`is_elidable_construction` admits only a
                // 5-byte `aload_0; invokespecial Object.<init>()V; return`
                // body) and case 2 is the arm's real work.
                //
                // The COUNTS are baseline-relative and should not be quoted
                // without their tree: the same measurement re-run after `cov-01`
                // and `cov-02` landed reads 106, not 52, because a method
                // blocked on `ldc` never reached its `invokespecial`. The
                // *shape* — all of them `<init>` — held at every baseline.
                //
                // Calling an `<init>` runs every side effect the elision path
                // was allowed to skip, which is the safe direction: the hazard
                // named at the elision site is the other one.
                0xb7 => {
                    // Defence in depth on the elision: only elide when the
                    // receiver (top of stack for a no-arg `<init>`) is a fresh
                    // `Op::New` we emitted. Eliding a `<init>` whose receiver is
                    // `this` or a parameter would skip a real superclass
                    // constructor (and hide any escape it performs) — and that
                    // is exactly what a `super()` chain call looks like, which
                    // the census found 29 of. Such a site now falls through to
                    // the call path below instead of bailing the method.
                    let elide = (self.trivial_init_pcs.contains(&pc)
                        && matches!(
                            self.peek_opt().and_then(|r| self.graph.node_opt(r)),
                            Some(Node {
                                op: Op::New { .. },
                                ..
                            })
                        ))
                        // `java/lang/Object.<init>()V` needs no receiver check:
                        // its body is a bare `return` and the native that
                        // shadows it is `native_noop_with_this`, so eliding it
                        // is sound on `this` as much as on a fresh `Op::New`.
                        // That is the ONLY shape exempt from the check above,
                        // and it is the terminal of every constructor chain —
                        // see `object_init_pcs` for what it was costing.
                        || self.object_init_pcs.contains(&pc);
                    if elide {
                        self.pop();
                        pc += 3;
                    }
                    // inc 24 (Gap B) + cov-04: a resolved `invokespecial`
                    // lowered to `Op::Call` — identical to the `invokestatic`
                    // arm except the receiver is arg0 (the caller's
                    // `invoke_info` entry already counts it in `num_args`, and
                    // the leaked `JitInvokeInfo` carries `invoke_kind == 1` so
                    // `invoke_dispatch` does the non-virtual dispatch to the
                    // statically-resolved target — the same route the
                    // single-pass backend gives every non-elided `<init>`).
                    // GC-safe by the same conservative IR-frame scan that roots
                    // reference args (inc 22). Only populated when the
                    // special-call gate is on; otherwise `invoke_info` has no
                    // entry for this pc and the method bails to single-pass.
                    // IR-tier inlining: splice the callee's body in place of the
                    // call. Checked after the elision (eliding costs nothing and
                    // a site that qualifies for it has no body worth splicing)
                    // and before the dispatch lowering.
                    else if self.inline_sites.contains_key(&pc) {
                        match self.begin_splice(pc, 3) {
                            Some(next) => pc = next,
                            None => return ir_build_bail(line!(), pc),
                        }
                    } else if let Some(&(info_ptr, num_args, ret_type)) = self.invoke_info.get(&pc)
                    {
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
                        // Neither lowering applies: no `invoke_info` entry for
                        // this pc (the special-call gate is off, or one
                        // non-emittable invoke elsewhere in the method
                        // discarded the whole map) and the site is not an
                        // elidable `<init>` on a fresh `Op::New`. Refuse the
                        // method — never guess a callee identity.
                        return self.bail_invoke(line!(), pc);
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
                    // IR-tier inlining: splice the callee's body in place of the
                    // call. `invokevirtual` reaches here only for a site the
                    // resolver bound to ONE body (a `final`/`private`/
                    // effectively-monomorphic target); a polymorphic site never
                    // gets an entry.
                    if self.inline_sites.contains_key(&pc) {
                        match self.begin_splice(pc, 3) {
                            Some(next) => {
                                pc = next;
                                continue;
                            }
                            None => return ir_build_bail(line!(), pc),
                        }
                    }
                    // A scalar-intrinsic family: emit the arithmetic and skip
                    // the call entirely. Checked BEFORE `invoke_info`, because
                    // the planner deliberately builds no row for such a site --
                    // see `IrBuilder::scalar_intrinsics`.
                    if self.try_emit_scalar_intrinsic(pc) {
                        pc += 3;
                        continue;
                    }
                    // An unboxing accessor: emit the guarded field load inline
                    // and skip the call. Same contract as the scalar families
                    // above -- the planner builds no `invoke_info` row for a
                    // site it registered here.
                    if self.try_emit_unbox_intrinsic(pc) {
                        pc += 3;
                        continue;
                    }
                    let (info_ptr, num_args, ret_type) = match self.invoke_info.get(&pc) {
                        Some(&t) => t,
                        None => return self.bail_invoke(line!(), pc),
                    };
                    // String access intrinsics in this tier — see the section
                    // above `IrBuilder`. Offered only to `invokevirtual`:
                    // `invokestatic` has no receiver, so no String accessor
                    // can reach this arm through it. A refusal leaves the
                    // abstract stack untouched and falls through to the
                    // `Op::Call` below, which is what the site does today.
                    if op == 0xb6 && self.try_string_access_intrinsic(pc, info_ptr, num_args) {
                        pc += 3;
                        continue;
                    }
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
                // invokedynamic — replaced by an uncommon trap, not compiled.
                //
                // This tier has no lowering for a bootstrap-method call site
                // and is not going to grow one soon. Before this arm existed,
                // `ir_compatible` refused any method containing one: **53
                // methods on the H2 JDBC workload**, most of them lambda or
                // string-concatenation sites on paths that run once.
                //
                // The single-pass backend made exactly this trade first -- an
                // indy site there lowers to an uncommon-trap deopt stub and the
                // rest of the method still compiles -- so this is matching a
                // decision already taken and soaked, not making a new one.
                //
                // The stack effect must still be exact. A pc with no recorded
                // shape means the descriptor resolver declined, and there the
                // method IS refused: guessing how many entries an indy consumes
                // would silently corrupt every bytecode after it.
                0xba => {
                    let Some(&(arg_entries, ret_tag)) = self.indy_trap_sites.get(&pc) else {
                        return ir_build_bail(line!(), pc);
                    };
                    if !self.plant_uncommon_trap(code, code_len, pc, TrapCause::Indy) {
                        return ir_build_bail(line!(), pc);
                    }
                    for _ in 0..arg_entries {
                        self.pop();
                    }
                    if ret_tag != b'V' {
                        let placeholder = match ret_tag {
                            b'J' => self.lconst(0),
                            b'F' => self.fconst(0.0),
                            b'D' => self.dconst(0.0),
                            b'L' | b'[' => self.aconst_null(),
                            _ => self.iconst(0),
                        };
                        self.push(placeholder);
                    }
                    // invokedynamic is five bytes: opcode, two CP index bytes,
                    // and two zero bytes JVMS §invokedynamic reserves.
                    pc += 5;
                }
                0xb9 => {
                    // IR-tier inlining — five bytes, not three.
                    if self.inline_sites.contains_key(&pc) {
                        match self.begin_splice(pc, 5) {
                            Some(next) => {
                                pc = next;
                                continue;
                            }
                            None => return ir_build_bail(line!(), pc),
                        }
                    }
                    let (info_ptr, num_args, ret_type) = match self.invoke_info.get(&pc) {
                        Some(&t) => t,
                        None => return self.bail_invoke(line!(), pc),
                    };
                    // A `java/lang/CharSequence.charAt`/`length`/`isEmpty`
                    // site arrives here rather than at 0xb6. The expander
                    // refuses every guarded site today, so this call exists
                    // to COUNT that population (`guarded_site_refused`)
                    // rather than to emit anything — a number this lane will
                    // need before anyone decides whether the header class-id
                    // compare is worth building. Five bytes, not three.
                    if self.try_string_access_intrinsic(pc, info_ptr, num_args) {
                        pc += 5;
                        continue;
                    }
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

                // dup2 — JVMS has two forms, and this builder's abstract stack
                // makes the second one trivial.
                //
                // Form 2 is a single CATEGORY-2 value duplicated. The stack here
                // holds one entry per VALUE rather than per slot, so a `long` or
                // `double` is one entry and duplicating it is exactly `dup`.
                //
                // Form 1 is the top TWO category-1 values duplicated as a pair:
                // `.., v2, v1` becomes `.., v2, v1, v2, v1`.
                //
                // A category-1 top over a category-2 second is not a legal
                // `dup2` shape in either form — the verifier does not produce
                // it — so it is refused rather than shuffled, the same call
                // `dup_x1` above makes for the same reason: guessing which
                // entry "the value below" names is how the builder's stack and
                // the verifier's stack quietly disagree.
                //
                // Measured on `org.h2.test.db.TestAlter`: one of the six
                // `unsupported_opcode` build refusals.
                0x5c => {
                    let Some(v1) = self.pop_opt() else {
                        return ir_build_bail(line!(), pc);
                    };
                    let v1_cat2 = matches!(
                        self.graph.nodes[v1 as usize].ty,
                        IrType::Long | IrType::Double
                    );
                    if v1_cat2 {
                        self.push(v1);
                        self.push(v1);
                    } else {
                        let Some(v2) = self.pop_opt() else {
                            return ir_build_bail(line!(), pc);
                        };
                        if matches!(
                            self.graph.nodes[v2 as usize].ty,
                            IrType::Long | IrType::Double
                        ) {
                            return ir_build_bail(line!(), pc);
                        }
                        self.push(v2);
                        self.push(v1);
                        self.push(v2);
                        self.push(v1);
                    }
                    pc += 1;
                }

                // pop2, swap, dup_x2, dup2_x1, dup2_x2 — the rest of the JVMS
                // stack shuffles, on the same one-entry-per-VALUE model as
                // `dup_x1` and `dup2` above: a category-2 value is one entry,
                // its category is read from the node's type, and a shape the
                // verifier cannot produce (a category-2 value where the form
                // needs category 1) is refused, never guessed.
                //
                // `jit_scan` accepted all five, so `ir_compatible` admitted
                // their methods and the build then refused them at the
                // catch-all after doing the partial work.
                0x58 | 0x5f | 0x5b | 0x5d | 0x5e => {
                    macro_rules! is_cat2 {
                        ($v:expr) => {
                            matches!(
                                self.graph.nodes[$v as usize].ty,
                                IrType::Long | IrType::Double
                            )
                        };
                    }
                    macro_rules! pop_value {
                        () => {
                            match self.pop_opt() {
                                Some(v) => v,
                                None => return ir_build_bail(line!(), pc),
                            }
                        };
                    }
                    macro_rules! pop_cat1 {
                        () => {{
                            let v = pop_value!();
                            if is_cat2!(v) {
                                return ir_build_bail(line!(), pc);
                            }
                            v
                        }};
                    }
                    match op {
                        // pop2: one category-2 value, or two category-1 values.
                        0x58 => {
                            let v1 = pop_value!();
                            if !is_cat2!(v1) {
                                let _ = pop_cat1!();
                            }
                        }
                        // swap: `.., v2, v1` -> `.., v1, v2`, both category 1.
                        0x5f => {
                            let v1 = pop_cat1!();
                            let v2 = pop_cat1!();
                            self.push(v1);
                            self.push(v2);
                        }
                        // dup_x2: v1 is category 1.
                        //   form 1: `.., v3, v2, v1` -> `.., v1, v3, v2, v1`
                        //   form 2: `.., v2(cat2), v1` -> `.., v1, v2, v1`
                        0x5b => {
                            let v1 = pop_cat1!();
                            let v2 = pop_value!();
                            if is_cat2!(v2) {
                                self.push(v1);
                                self.push(v2);
                                self.push(v1);
                            } else {
                                let v3 = pop_cat1!();
                                self.push(v1);
                                self.push(v3);
                                self.push(v2);
                                self.push(v1);
                            }
                        }
                        // dup2_x1:
                        //   form 1: `.., v3, v2, v1` -> `.., v2, v1, v3, v2, v1`
                        //   form 2: `.., v2, v1(cat2)` -> `.., v1, v2, v1`
                        0x5d => {
                            let v1 = pop_value!();
                            if is_cat2!(v1) {
                                let v2 = pop_cat1!();
                                self.push(v1);
                                self.push(v2);
                                self.push(v1);
                            } else {
                                let v2 = pop_cat1!();
                                let v3 = pop_cat1!();
                                self.push(v2);
                                self.push(v1);
                                self.push(v3);
                                self.push(v2);
                                self.push(v1);
                            }
                        }
                        // dup2_x2:
                        //   form 1: `.., v4, v3, v2, v1` -> `.., v2, v1, v4, v3, v2, v1`
                        //   form 2: `.., v3, v2, v1(cat2)` -> `.., v1, v3, v2, v1`
                        //   form 3: `.., v3(cat2), v2, v1` -> `.., v2, v1, v3, v2, v1`
                        //   form 4: `.., v2(cat2), v1(cat2)` -> `.., v1, v2, v1`
                        _ => {
                            let v1 = pop_value!();
                            if is_cat2!(v1) {
                                let v2 = pop_value!();
                                if is_cat2!(v2) {
                                    self.push(v1);
                                    self.push(v2);
                                    self.push(v1);
                                } else {
                                    let v3 = pop_cat1!();
                                    self.push(v1);
                                    self.push(v3);
                                    self.push(v2);
                                    self.push(v1);
                                }
                            } else {
                                let v2 = pop_cat1!();
                                let v3 = pop_value!();
                                if is_cat2!(v3) {
                                    self.push(v2);
                                    self.push(v1);
                                    self.push(v3);
                                    self.push(v2);
                                    self.push(v1);
                                } else {
                                    let v4 = pop_cat1!();
                                    self.push(v2);
                                    self.push(v1);
                                    self.push(v4);
                                    self.push(v3);
                                    self.push(v2);
                                    self.push(v1);
                                }
                            }
                        }
                    }
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
                    if self.prune_always_taken_branch(pc, cmp, target_pc) {
                        pc += 3; // into the cold arm, as dead code
                        continue;
                    }
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
                    if self.prune_always_taken_branch(pc, cmp, target_pc) {
                        pc += 3; // into the cold arm, as dead code
                        continue;
                    }
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

                // ifnull (0xc6) / ifnonnull (0xc7) — compare a reference
                // against null; if_acmpeq (0xa5) / if_acmpne (0xa6) — compare
                // two references. Structurally identical to the int forms
                // above; the operands are `Ref`-typed, and `ir_lower`'s
                // `Op::Cmp` selects a 64-bit CMP when either operand is, so a
                // pointer whose low 32 bits happen to be zero is not mistaken
                // for null.
                0xa5 | 0xa6 | 0xc6 | 0xc7 => {
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
                    let target_pc = (pc as i32 + offset) as usize;
                    let (a, b) = if op == 0xc6 || op == 0xc7 {
                        let val = self.pop();
                        (val, self.aconst_null())
                    } else {
                        let b = self.pop();
                        let a = self.pop();
                        (a, b)
                    };
                    let cc = if op == 0xc6 || op == 0xa5 {
                        CmpOp::Eq
                    } else {
                        CmpOp::Ne
                    };
                    let cmp = self.add_data(Op::Cmp(cc), IrType::Int, vec![a, b], pc);
                    if self.prune_always_taken_branch(pc, cmp, target_pc) {
                        pc += 3; // into the cold arm, as dead code
                        continue;
                    }
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

                    // True edge → target
                    self.ctrl = true_ctrl;
                    self.add_merge_predecessor(target_pc);

                    // False edge → fall-through
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

                // goto_w — `goto` with a four-byte signed offset. The same arm;
                // only the operand width differs. javac emits it only past a
                // 32 KiB method (beyond `IR_MAX_BYTECODE_SIZE`), but ASM-based
                // rewriters and instrumentation agents widen jumps freely, and
                // the verifier, `bytecode_analysis` and the merge pre-scan all
                // already treat it as a branch. It was the one control-flow
                // opcode with no arm, so such a method built all the way to it
                // and then died at the `_ =>` catch-all.
                0xc8 => {
                    if pc + 4 >= code.len() {
                        return ir_build_bail(line!(), pc);
                    }
                    let offset = i32::from_be_bytes([
                        code[pc + 1],
                        code[pc + 2],
                        code[pc + 3],
                        code[pc + 4],
                    ]);
                    let Some(target_pc) = pc.checked_add_signed(offset as isize) else {
                        return ir_build_bail(line!(), pc);
                    };
                    self.add_merge_predecessor(target_pc);
                    self.ctrl = NO_NODE; // dead after unconditional jump
                    pc += 5;
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
                // areturn (0xb0) shares this arm: `Op::Return` loads the value's
                // frame slot into RAX and tears the frame down, which is the
                // same ABI for a reference as for an int — the single-pass
                // backend returns a raw pointer in RAX too. It was simply never
                // wired, and it is the second-largest builder refusal on a real
                // workload (25 of 141 on a Hibernate class): any method that
                // returns an object was refused at its return.
                0xac | 0xb0 => {
                    let val = self.pop();
                    // JVMS §6.5 `ireturn` narrowing for a `Z`/`B`/`C`/`S`
                    // return — never `areturn`. Inside a splice the tag is the
                    // spliced callee's own, so it applies at every return edge
                    // of that body, single- or multi-return alike.
                    let val = if code[pc] == 0xac {
                        let tag = match self.splice.last() {
                            Some(frame) => frame.return_narrow,
                            None => self.return_narrow,
                        };
                        self.narrow_int_return(val, tag, pc)
                    } else {
                        val
                    };
                    // Inside a splice this is not a method exit: it hands the
                    // value back to the caller's operand stack and the walk
                    // resumes after the `invoke`. No `Op::Return`, and `ctrl`
                    // stays live.
                    if !self.splice.is_empty() {
                        match self.splice_return(Some(val), pc) {
                            Some(next) => {
                                pc = next;
                                continue;
                            }
                            None => return ir_build_bail(line!(), pc),
                        }
                    }
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
                    if !self.splice.is_empty() {
                        match self.splice_return(Some(val), pc) {
                            Some(next) => {
                                pc = next;
                                continue;
                            }
                            None => return ir_build_bail(line!(), pc),
                        }
                    }
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
                    if !self.splice.is_empty() {
                        match self.splice_return(Some(val), pc) {
                            Some(next) => {
                                pc = next;
                                continue;
                            }
                            None => return ir_build_bail(line!(), pc),
                        }
                    }
                    let ret =
                        self.graph
                            .add(Op::Return, IrType::Void, vec![self.ctrl, val], Some(pc));
                    self.graph.exit = ret;
                    self.ctrl = NO_NODE;
                    pc += 1;
                }

                // return (void)
                0xb1 => {
                    if !self.splice.is_empty() {
                        match self.splice_return(None, pc) {
                            Some(next) => {
                                pc = next;
                                continue;
                            }
                            None => return ir_build_bail(line!(), pc),
                        }
                    }
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
                // ldc (0x12, 1-byte CP index) / ldc_w (0x13, 2-byte CP index) —
                // cov-01 increment 1, the IMMEDIATE case.
                //
                // `ldc` + `ldc_w` + `getstatic` was 189 of the 273 opcode-gap
                // events measured on 2026-08-03 — 69% of every opcode
                // `IrBuilder::build` had no arm for at all
                // (`ir-coverage-survey-20260803.md`).
                //
                // A resolved `int` / `float` constant is a constant node and
                // nothing else — no memory edge, no safepoint, no GC
                // interaction — so it is typed and pushed exactly the way the
                // `ldc2_w` arm above types its `long` / `double`. `is_float`
                // comes from the resolver rather than from the opcode because
                // `ldc` is polymorphic and `is_float_opcode` does NOT list it:
                // a method whose only FP is `ldc 1.5f` is admitted through the
                // int clause, so the builder cannot infer the width from
                // admission either. See `JitLdcConstant::Immediate`.
                //
                // Increments 2 and 3 add the two REFERENCE constants — a String
                // literal and a Class mirror. Neither is an immediate: each
                // names a constant-pool site whose value is materialised on
                // every execution by the same runtime helper the single-pass
                // backend calls, because an `ObjectRef` baked at compile time is
                // stale the moment a relocating collector moves it. Both are
                // therefore `[ctrl, mem]` safepoint nodes, not constants — see
                // `Op::ConstString` / `Op::ConstClass`.
                //
                // Everything still absent from all three tables — `MethodHandle`,
                // `MethodType`, condy, a site whose helper is unwired, a float
                // with the FP gate off — bails the method to single-pass, which
                // DOES compile all of them. That is the fail-closed half of this
                // arm and it is the point: a constant materialised without its
                // resolution side effects (interning, class loading, `<clinit>`)
                // is a wrong-code bug, not a missing optimisation.
                0x12 | 0x13 => {
                    let width = if op == 0x12 { 2 } else { 3 };
                    if let Some(&(bits, is_float)) = self.ldc_info.get(&pc) {
                        let c = if is_float {
                            self.fconst(f32::from_bits(bits as u32))
                        } else {
                            self.iconst(bits)
                        };
                        self.push(c);
                        pc += width;
                    } else if let Some(&(holder_class_id, cp_idx)) = self.ldc_string_info.get(&pc) {
                        let slot_addr = self
                            .ldc_slot_info
                            .get(&(holder_class_id, cp_idx))
                            .copied()
                            .unwrap_or(0);
                        let s = self.graph.add(
                            Op::ConstString {
                                holder_class_id,
                                cp_idx,
                                slot_addr,
                            },
                            IrType::Ref,
                            vec![self.ctrl, self.mem],
                            Some(pc),
                        );
                        // The node IS the new memory token, exactly as
                        // `Op::Call` is: interning can allocate, and a
                        // collection may run inside the helper, so nothing may
                        // be reordered across it.
                        self.mem = s;
                        self.push(s);
                        pc += width;
                    } else if let Some(&(holder_class_id, cp_idx)) = self.ldc_class_info.get(&pc) {
                        let slot_addr = self
                            .ldc_slot_info
                            .get(&(holder_class_id, cp_idx))
                            .copied()
                            .unwrap_or(0);
                        let c = self.graph.add(
                            Op::ConstClass {
                                holder_class_id,
                                cp_idx,
                                slot_addr,
                            },
                            IrType::Ref,
                            vec![self.ctrl, self.mem],
                            Some(pc),
                        );
                        self.mem = c;
                        self.push(c);
                        pc += width;
                    } else {
                        return ir_build_bail(line!(), pc);
                    }
                }
                // getstatic — cov-01 increment 4, and the single largest opcode
                // in the survey (92 of 273 events).
                //
                // A load from a statics base plus the ref/non-ref split, exactly
                // as the lane brief frames it. `static_field_info` names the
                // offset and the type tag; this arm's job is to match the
                // single-pass `0xb2` arm's behaviour, not to invent one, so the
                // class-initialisation question is answered where that arm
                // answers it — in the lowering, which picks between the direct
                // load (only for a class the resolver reports already
                // initialised) and `helpers.getstatic` (which runs `<clinit>`
                // and can throw).
                //
                // Every JVM field type lowers. `J`/`D`/`F` were refused when this
                // arm landed, on the grounds that admitting one would put a
                // `Long`/`Double`/`Float` node in a graph whose admission clause
                // may have been the int one. That is a real hazard and the
                // refusal was in the wrong PLACE: `getstatic` is polymorphic and
                // is listed by neither `is_category2_opcode` nor
                // `is_float_opcode`, so the builder cannot see the width — but
                // the CALLER can, because it resolved the type tag in order to
                // build this table at all. So the gate moved to the feed in
                // `try_compile`, which admits a `J` entry only under
                // `ir_emit_long` and a `D`/`F` entry only under `ir_emit_fp`,
                // exactly as it already does for a float `ldc` and as the
                // `ldc2_w` feed does for its `(bits, is_double)`.
                //
                // A tag absent from this map is therefore either unresolvable or
                // gated off, and both bail — the same fail-closed answer, now
                // decided by the one party that has the information.
                0xb2 => {
                    let (class_id, field_index, type_tag, is_volatile) =
                        match self.static_field_info.get(&pc) {
                            Some(&si) => si,
                            None => return ir_build_bail(line!(), pc),
                        };
                    let ty = match type_tag {
                        b'I' | b'Z' | b'B' | b'C' | b'S' => IrType::Int,
                        b'L' | b'[' => IrType::Ref,
                        b'J' => IrType::Long,
                        b'D' => IrType::Double,
                        b'F' => IrType::Float,
                        _ => return ir_build_bail(line!(), pc),
                    };
                    // A `field_index` past `u32` cannot be encoded in the baked
                    // displacement; refuse rather than truncate.
                    let Ok(field_index) = u32::try_from(field_index) else {
                        return ir_build_bail(line!(), pc);
                    };
                    let load = self.graph.add(
                        Op::LoadStatic {
                            class_id,
                            field_index,
                            type_tag,
                            is_volatile,
                        },
                        ty,
                        vec![self.ctrl, self.mem],
                        Some(pc),
                    );
                    // Both lowerings can reach the runtime (the helper one runs
                    // `<clinit>`), and a volatile read is a JMM acquire, so the
                    // node advances the memory token like every other
                    // `MemAccess::Opaque` node.
                    self.mem = load;
                    self.push(load);
                    pc += 3;
                }

                // tableswitch / lookupswitch — an `Op::Switch` multi-way
                // terminator when `ir::switch_is_dense` admits the key range,
                // and the historical CMP-equality chain otherwise.
                //
                // # The two shapes, and why both exist
                //
                // The chain is one `Op::Const`, one `Op::Cmp`, one `Op::If` and
                // two `Op::Proj` **per case** — five nodes each — and up to one
                // sequential compare per case at run time. It is correct, it is
                // what this arm has always emitted, and for two or three cases
                // it is also the right answer: a jump table costs an indirect
                // branch the predictor mispredicts, and four compares do not.
                // `ir_lower` keeps a chain lowering for exactly that reason, so
                // the floor is never lost.
                //
                // `Op::Switch` is one node plus one `Op::Proj` per span slot,
                // and `ir_lower` turns it into a jump table or a binary search.
                // The span, not the case count, is what it costs — which is
                // why `switch_is_representable` is asked HERE rather than left
                // for the lowerer to regret, and why the span it is asked
                // about is trimmed to the keys that actually go somewhere.
                //
                // # What the edge set has to preserve
                //
                // JVMS §6.5 gives every switch a default edge and one edge per
                // listed key, and says nothing about them being distinct
                // targets. Both shapes below preserve all of them: the dense
                // form gives every slot in `[low, high]` its own projection,
                // pointing at that key's target if the payload listed one and
                // at the default target if it did not, so two cases naming the
                // same pc stay two edges into the same merge, and a hole in a
                // `tableswitch` stays an edge to default rather than becoming
                // an absent edge.
                0xaa | 0xab => {
                    // `switch_table` is a STRICT decode over `code[..code_len]`
                    // — the CALLER's code alone. Inside a splice `pc` addresses
                    // a relocated callee body past `code_len`, so it answers
                    // `None` and this arm refuses the whole method.
                    //
                    // That refusal is correct and must stay: `switch_operand_base`
                    // computes the JVMS 4-byte operand padding relative to the
                    // start of the BUFFER, while the padding bytes in a
                    // relocated body were emitted relative to that callee's own
                    // code array, so widening the bound to `code.len()` would
                    // decode the wrong operand unless the splice base happened
                    // to be 4-aligned. See `bytecode_analysis::switch_operand_base`.
                    //
                    // What was wrong was the spelling: a bare `?` returns
                    // `None` with NO attribution, so this was one of the
                    // refusals the per-method bail census could not see — the
                    // exact invisibility `ir_build_bail`'s own doc was written
                    // about. It is an attributed refusal now.
                    let Some((len, default_target, cases)) =
                        crate::bytecode_analysis::switch_table(code, code_len, pc)
                            .map(|t| (t.len, t.default, t.cases))
                    else {
                        return ir_build_bail(line!(), pc);
                    };
                    let key = self.pop();
                    if let Some((low, high)) = dense_switch_bounds(&cases, default_target) {
                        // ── The multi-way terminator ───────────────────────
                        //
                        // One `Op::Switch` and one `Op::Proj` per span slot,
                        // plus the default's. `dense_switch_bounds` has already
                        // established that the keys are strictly ascending and
                        // that [`switch_is_representable`] admits the span, so
                        // the projection count below is bounded by
                        // `SWITCH_MAX_PROJECTIONS` and the `as u8` casts cannot
                        // truncate.
                        //
                        // `switch_is_representable`, NOT `switch_is_dense`,
                        // which this comment used to name. The two are not
                        // interchangeable: `switch_is_dense` is the LOWERER's
                        // question (jump table, or binary search) and is
                        // strictly stronger, so naming it here overstated what
                        // the builder had actually checked. The weaker
                        // predicate is the correct gate, and it is the one
                        // that bounds the span at `SWITCH_MAX_SPAN` — which is
                        // the clause the casts below rest on.
                        let span = switch_projection_count(low, high) - 1;
                        // Key → target for the slots the payload actually
                        // listed. Built once; the loop below is a lookup per
                        // slot rather than a scan per slot, so a 255-slot
                        // switch stays linear.
                        let listed: HashMap<i32, usize> = cases.iter().copied().collect();
                        let switch_node = self.graph.add(
                            Op::Switch { low, high },
                            IrType::Control,
                            vec![self.ctrl, key],
                            Some(pc),
                        );
                        for k in 0..span {
                            // `low + k` cannot overflow: `k < span` and
                            // `low + span - 1 == high`, both checked in `i64`
                            // by `switch_is_dense`.
                            let match_val = low.wrapping_add(k as i32);
                            let target = listed.get(&match_val).copied().unwrap_or(default_target);
                            let edge = self.graph.add(
                                Op::Proj(k as u16),
                                IrType::Control,
                                vec![switch_node],
                                Some(pc),
                            );
                            // The abstract state is invariant across the edges
                            // (the switch consumed only `key`), so every
                            // predecessor snapshot taken here is correct — the
                            // same argument the chain below rests on.
                            self.ctrl = edge;
                            self.add_merge_predecessor(target);
                        }
                        // The default edge is the LAST projection, and it is
                        // mandatory: `Proj(span)` is the index
                        // `ir_verify::projection_arity` reserves for it and the
                        // one `ir_lower` emits the out-of-range branch to.
                        let default_edge = self.graph.add(
                            Op::Proj(span as u16),
                            IrType::Control,
                            vec![switch_node],
                            Some(pc),
                        );
                        self.ctrl = default_edge;
                        self.add_merge_predecessor(default_target);
                    } else {
                        for (match_val, target) in &cases {
                            let cval = self.iconst(*match_val as i64);
                            let cmp =
                                self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![key, cval], pc);
                            let if_node = self.graph.add(
                                Op::If,
                                IrType::Control,
                                vec![self.ctrl, cmp],
                                Some(pc),
                            );
                            let true_ctrl = self.graph.add(
                                Op::Proj(0),
                                IrType::Control,
                                vec![if_node],
                                Some(pc),
                            );
                            let false_ctrl = self.graph.add(
                                Op::Proj(1),
                                IrType::Control,
                                vec![if_node],
                                Some(pc),
                            );
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
                    }
                    self.ctrl = NO_NODE;
                    pc += len;
                }

                // Unsupported opcode — bail out
                _ => return ir_build_bail_opcode(op, pc),
            }
        }

        // A splice still open here means the walk left a relocated body without
        // passing its return — only reachable if the loop condition and the
        // splice bookkeeping disagree. The graph would be missing the caller
        // frame it saved, so refuse rather than emit it.
        if !self.splice.is_empty() {
            return ir_build_bail(line!(), code_len);
        }
        // See `splice_guard_seen`.
        if self.splice_guard_seen {
            return ir_build_bail(line!(), code_len);
        }
        // A join the walk activated at the very end of the method (no
        // instruction after it to trip the per-instruction check).
        if self.unstructured_monitors {
            return ir_build_bail(line!(), code_len);
        }
        // Same reason, for the refusals a bookkeeping helper latches: the last
        // `patch_loop_backedge` of a method is very often performed by the very
        // last instruction (the back edge of the outermost loop), so the
        // per-instruction check above never runs again. The site and pc are the
        // helper's own, not this line's, so the census names the real refusal.
        if let Some((site, at)) = self.build_refused {
            return ir_build_bail(site, at);
        }
        // Every merge the pre-scan registered was either activated by the walk
        // or never reached; see `audit_unactivated_merges`.
        if let Some(lost_at) = self.audit_unactivated_merges() {
            return ir_build_bail(line!(), lost_at);
        }
        self.retire_untypeable_phis();
        if self.splices_done > 0 && ir_bail_reporting() {
            eprintln!(
                "[ir] spliced {} callee bod{} into {}",
                self.splices_done,
                if self.splices_done == 1 { "y" } else { "ies" },
                self.method_label.as_deref().unwrap_or("<method>"),
            );
        }

        Some(self.graph)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

/// The inclusive key bounds of an [`Op::Switch`] for this decoded switch
/// payload, or `None` when the payload must keep the `Op::Cmp`/`Op::If` chain.
///
/// `cases` is `bytecode_analysis::SwitchTable::cases` — `(match value, absolute
/// target)` in TABLE order, which for a `tableswitch` is one entry per slot in
/// `low ..= high` and for a `lookupswitch` is the listed pairs.
///
/// Three refusals, each of them a correctness requirement rather than a
/// heuristic:
///
/// * **Empty.** A `tableswitch` with `high < low` or a `lookupswitch` with
///   `npairs == 0` is legal bytecode that means "always take the default".
///   It has no key range at all, and the chain handles it by falling straight
///   through to the default edge.
/// * **Not strictly ascending.** JVMS §6.5 *requires* a `lookupswitch`'s pairs
///   to be sorted and distinct, and the verifier is entitled to assume it — but
///   nothing on this path checks it, and a duplicate key would want two
///   projections for one slot of the dense form (`ir_verify::check_projections`
///   rejects the second, and before that the later one would silently win).
///   An unsorted or duplicated payload therefore keeps the chain, which
///   compares against every listed key in table order and so preserves the
///   JVMS "first match wins" reading whatever the payload contains.
/// * **Too sparse, or too wide.** [`switch_is_representable`] — see there.
///
/// # The bounds are the LIVE bounds
///
/// `low`/`high` come from the first and last entry whose target is not the
/// default, not from the first and last entry. A `tableswitch` pads its payload
/// with the default offset — `switch (x) { case 3: case 4: }` over a method
/// whose other branches made javac pick `low = 0` lists slots 0, 1 and 2 as
/// default — and a range that included those slots would spend projections on
/// keys the default edge already covers. Trimming them costs nothing: a key
/// outside `[low, high]` takes the default edge, which is exactly where those
/// slots were going.
///
/// The ascending check comes FIRST, because the bounds are read off the ends of
/// the live entries and only a sorted list has its extremes there.
fn dense_switch_bounds(cases: &[(i32, usize)], default_target: usize) -> Option<(i32, i32)> {
    if !cases.windows(2).all(|w| w[0].0 < w[1].0) {
        return None;
    }
    let mut low: Option<i32> = None;
    let mut high = 0i32;
    let mut live_count = 0usize;
    for &(key, target) in cases {
        if target == default_target {
            continue;
        }
        if low.is_none() {
            low = Some(key);
        }
        high = key;
        live_count += 1;
    }
    // Every listed key pointed at the default: there is nothing for a switch
    // node to dispatch on, and the chain's trailing jump to the default is
    // already the whole instruction.
    let low = low?;
    if !switch_is_representable(low, high, live_count) {
        return None;
    }
    Some((low, high))
}

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

/// Does the caller's code have a loop header that the pc-order walk in
/// [`IrBuilder::build`] cannot build?
///
/// That is a reachable header other than pc 0 with no reachable predecessor at
/// a LOWER pc. At pc 0 the method entry itself is the forward entry. The pc
/// walk activates a header when it arrives, and needs an entry edge recorded
/// by then. A rotated loop's body, `goto test; body: …; test: if<cond> body`,
/// has no such edge. Its only predecessors are the back edge from the test and,
/// through the test, the `goto`, and both sit at higher pcs or target the test
/// instead.
///
/// `loop_headers` is the caller's own set: the verifier's "target of a
/// backward branch" headers, filtered to reachable pcs.
fn has_rotated_loop_header(
    verified: &cratonvm_reader::VerifiedCode,
    reachable: &HashSet<usize>,
    loop_headers: &HashSet<usize>,
    code_len: usize,
) -> bool {
    // A method with no loop, which is most methods, pays nothing more.
    if !loop_headers.iter().any(|&h| h != 0 && h < code_len) {
        return false;
    }
    let mut entered_forward: HashSet<usize> = HashSet::new();
    for insn in verified.instructions() {
        let pc = insn.pc as usize;
        if pc >= code_len || !reachable.contains(&pc) {
            continue;
        }
        for succ in verified.successors(pc) {
            if succ > pc && loop_headers.contains(&succ) {
                entered_forward.insert(succ);
            }
        }
    }
    loop_headers
        .iter()
        .any(|&h| h != 0 && h < code_len && !entered_forward.contains(&h))
}

/// The order in which [`IrBuilder::build`] walks a method that has a rotated
/// loop (see [`has_rotated_loop_header`]): the reachable caller bytecode cut
/// into basic blocks, in reverse post-order of the block CFG.
///
/// # Why reverse post-order
///
/// In RPO every edge goes forward except the retreating ones, and in a
/// reducible CFG those are exactly the back edges, whose targets dominate their
/// sources. So when the walk reaches a block, every forward predecessor has
/// already recorded its snapshot, which is the invariant `activate_merge` and
/// `activate_loop_header` both rest on. Every later predecessor is a back edge,
/// which `add_merge_predecessor` back-patches into the header's phis. The pc
/// walk has the same invariant for javac's top-tested loops only because pc
/// order happens to be such an order for them.
///
/// The loop headers are therefore the targets of the edges that are retreating
/// in THIS order, not the verifier's backward-branch targets. In
/// `goto test; body: …; test: if<cond> body` the header is the test, which
/// dominates the body. The body becomes an ordinary one-predecessor merge.
///
/// # What the walk relies on, and checks
///
/// * **A block reached only by fall-through is placed right after the block
///   that falls into it.** Such a block is not a merge target, so only the
///   live walk state carries its entry. The DFS visits a block's fall-through
///   successor LAST (`VerifiedCode::successors` lists it last), and that puts
///   the successor immediately after its only predecessor in reverse
///   post-order.
/// * **Every loop header is a merge target**, so there is an `Op::Merge` to
///   hang its phis on.
///
/// A method that breaks either is refused (`None`), never walked.
struct BlockWalk {
    /// `[start, end)` bytecode ranges in walk order. Blocks adjacent both in
    /// the order and in the bytecode are coalesced into one range, so a range
    /// ends exactly where the order leaves the bytecode's own sequence. The
    /// first range starts at pc 0.
    ranges: Vec<(usize, usize)>,
    /// Start pcs of the blocks targeted by an edge that is retreating in this
    /// order.
    headers: HashSet<usize>,
}

impl BlockWalk {
    /// Cut the reachable instructions below `code_len` into blocks and order
    /// them. `None` for a CFG whose shape breaks one of the checks on
    /// [`BlockWalk`].
    fn reverse_postorder(
        verified: &cratonvm_reader::VerifiedCode,
        reachable: &HashSet<usize>,
        code_len: usize,
    ) -> Option<BlockWalk> {
        // Reachable instructions in pc order, as `(pc, next_pc)`.
        let insns: Vec<(usize, usize)> = verified
            .instructions()
            .iter()
            .map(|insn| (insn.pc as usize, insn.next_pc as usize))
            .filter(|&(pc, _)| pc < code_len && reachable.contains(&pc))
            .collect();
        if insns.first().map(|&(pc, _)| pc) != Some(0) {
            return None;
        }
        let merge_targets: HashSet<usize> = verified
            .merge_targets()
            .iter()
            .map(|&t| t as usize)
            .collect();

        // Block leaders are: the entry; every merge target; every successor of
        // an instruction that does not simply fall through, and the
        // instruction after it; and any instruction whose textual predecessor
        // is unreachable.
        let mut leaders: HashSet<usize> = HashSet::new();
        leaders.insert(0);
        for (k, &(pc, next)) in insns.iter().enumerate() {
            if merge_targets.contains(&pc) || (k > 0 && insns[k - 1].1 != pc) {
                leaders.insert(pc);
            }
            let succs = verified.successors(pc);
            if !(succs.len() == 1 && succs[0] == next) {
                leaders.extend(succs.iter().copied());
                leaders.insert(next);
            }
        }

        // Blocks, numbered in pc order: start, end (the `next_pc` of the last
        // instruction), and the last instruction's pc. Block 0 starts at pc 0.
        let mut starts: Vec<usize> = Vec::new();
        let mut ends: Vec<usize> = Vec::new();
        let mut lasts: Vec<usize> = Vec::new();
        for &(pc, next) in &insns {
            if leaders.contains(&pc) || starts.is_empty() {
                starts.push(pc);
                ends.push(next);
                lasts.push(pc);
            } else {
                let b = ends.len() - 1;
                ends[b] = next;
                lasts[b] = pc;
            }
        }
        let nb = starts.len();
        let block_at: HashMap<usize, usize> =
            starts.iter().enumerate().map(|(b, &s)| (s, b)).collect();

        // Successor blocks, branch targets first and the fall-through last.
        let mut succ: Vec<Vec<usize>> = Vec::with_capacity(nb);
        for &last in &lasts {
            let mut out = Vec::new();
            for s in verified.successors(last) {
                if s >= code_len {
                    continue;
                }
                // Every successor of a block's last instruction is a leader
                // by construction; a miss means the cut above is wrong.
                out.push(*block_at.get(&s)?);
            }
            succ.push(out);
        }

        // Iterative DFS from the entry: a long chain of blocks must not recurse.
        let mut seen = vec![false; nb];
        let mut postorder: Vec<usize> = Vec::with_capacity(nb);
        let mut dfs: Vec<(usize, usize)> = vec![(0, 0)];
        seen[0] = true;
        while let Some(&(b, i)) = dfs.last() {
            if i < succ[b].len() {
                let top = dfs.len() - 1;
                dfs[top].1 = i + 1;
                let s = succ[b][i];
                if !seen[s] {
                    seen[s] = true;
                    dfs.push((s, 0));
                }
            } else {
                dfs.pop();
                postorder.push(b);
            }
        }
        // Every block holds reachable code, so the DFS must reach all of them.
        if postorder.len() != nb {
            return None;
        }
        let order: Vec<usize> = postorder.into_iter().rev().collect();
        let mut pos = vec![0usize; nb];
        for (k, &b) in order.iter().enumerate() {
            pos[b] = k;
        }

        let mut headers: HashSet<usize> = HashSet::new();
        for b in 0..nb {
            for &s in &succ[b] {
                let target = starts[s];
                if pos[s] <= pos[b] {
                    if !merge_targets.contains(&target) {
                        return None;
                    }
                    headers.insert(target);
                }
                if target == ends[b] && !merge_targets.contains(&target) && pos[s] != pos[b] + 1 {
                    return None;
                }
            }
        }

        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for &b in &order {
            match ranges.last_mut() {
                Some(last) if last.1 == starts[b] => last.1 = ends[b],
                _ => ranges.push((starts[b], ends[b])),
            }
        }
        Some(BlockWalk { ranges, headers })
    }
}

/// A call-site intrinsic family the optimizing tier lowers as arithmetic.
///
/// Every member is BRANCHLESS and uses only baseline x86-64 encodings -- `CMP`,
/// `CMOVcc`, `SETcc`, `SAR`, `XOR`, `SUB`. Nothing here needs a CPU feature bit,
/// which is deliberate: `POPCNT`/`LZCNT`/`TZCNT` would each need a runtime
/// probe and a fallback, and a family that is *sometimes* an intrinsic is a
/// family whose A/B measures the host rather than the change. The bit-scan
/// families (`bitCount`, `numberOfLeadingZeros`, `numberOfTrailingZeros`,
/// `reverseBytes`, `highestOneBit`, `lowestOneBit`) are the obvious next
/// entries and are deliberately NOT here yet -- see the census, which counts
/// what each family would have been worth before anyone builds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScalarOp {
    /// `Math.min(int,int)` / `StrictMath.min`.
    MinI,
    /// `Math.max(int,int)`.
    MaxI,
    /// `Math.min(long,long)`.
    MinL,
    /// `Math.max(long,long)`.
    MaxL,
    /// `Math.abs(int)`. NOTE `Math.abs(Integer.MIN_VALUE) == Integer.MIN_VALUE`
    /// by JLS 15.15.4, which the branchless sequence reproduces exactly (the
    /// two's-complement negation of the minimum is itself); a `CMOV` form
    /// written from the obvious `x < 0 ? -x : x` would too, but only because
    /// the negation wraps -- worth knowing before anyone "simplifies" it.
    AbsI,
    /// `Math.abs(long)`, same edge at `Long.MIN_VALUE`.
    AbsL,
    /// `Integer.compare(int,int)` -> `-1`/`0`/`1`, SIGNED.
    CompareI,
    /// `Long.compare(long,long)` -> `-1`/`0`/`1`, SIGNED.
    CompareL,

    // -- Bit-scan families, 2026-09-06 -------------------------------
    //
    // `refused_method=59` on H2 was the work list these come off. Every one
    // uses BSR / BSF / BSWAP / ROL / ROR, which are BASELINE x86-64 -- no
    // POPCNT, LZCNT or TZCNT, and so no CPU feature probe and no fallback.
    // `Integer.bitCount` is deliberately still absent for exactly that reason:
    // it wants POPCNT or a twelve-instruction SWAR, and a family that is
    // *sometimes* an intrinsic makes its own A/B a measurement of the host.
    //
    // The ZERO input is the edge every one of these has, and each answers it
    // BRANCHLESSLY. BSR and BSF leave the destination UNDEFINED on a zero
    // source and set ZF -- so the sequences read ZF with CMOVZ or SETZ before
    // anything clobbers the flags, which is why the MOV sitting between the
    // scan and the conditional is load-bearing rather than sloppy.
    /// `Integer.numberOfLeadingZeros(int)` -> `int`; 32 for zero.
    NlzI,
    /// `Long.numberOfLeadingZeros(long)` -> `int`; 64 for zero.
    NlzL,
    /// `Integer.numberOfTrailingZeros(int)` -> `int`; 32 for zero.
    NtzI,
    /// `Long.numberOfTrailingZeros(long)` -> `int`; 64 for zero.
    NtzL,
    /// `Integer.reverseBytes(int)`.
    ReverseBytesI,
    /// `Long.reverseBytes(long)`.
    ReverseBytesL,
    /// `Integer.lowestOneBit(int)`, the `x AND -x` identity; 0 for zero, and
    /// the only member of this group with no undefined-register edge at all.
    LowestOneBitI,
    /// `Long.lowestOneBit(long)`.
    LowestOneBitL,
    /// `Integer.highestOneBit(int)`; 0 for zero.
    HighestOneBitI,
    /// `Long.highestOneBit(long)`; 0 for zero.
    HighestOneBitL,
    /// `Integer.rotateLeft(int, int)`. x86 masks the count to 5 bits, which is
    /// exactly the `distance AND 31` the JLS specifies -- including for a
    /// NEGATIVE distance, where `rotateLeft(i, -1) == rotateRight(i, 1)`.
    RotateLeftI,
    /// `Long.rotateLeft(long, int)`; masked to 6 bits.
    RotateLeftL,
    /// `Integer.rotateRight(int, int)`.
    RotateRightI,
    /// `Long.rotateRight(long, int)`.
    RotateRightL,

    // -- Floating-point sign mask, 2026-09-07 ------------------------
    //
    // `Math.abs(float)` / `Math.abs(double)` clear the sign bit, which is one
    // AND against a mask -- no branch, no memory, and no NaN special case:
    // clearing the sign of a NaN yields a NaN, which is what the JLS requires.
    // It also gets `abs(-0.0) == +0.0` right for free, where a comparison-based
    // sequence (`x < 0 ? -x : x`) returns -0.0 and is WRONG.
    //
    // These are the first FP members of this enum. They do NOT go through
    // `gp_load_value`/`store_rax` like every variant above; see
    // `ScalarOp::is_fp` and the guard at the top of the lowering arm.
    /// `Math.abs(float)`.
    AbsF,
    /// `Math.abs(double)`.
    AbsD,
}

impl ScalarOp {
    /// How many data inputs the node takes. `ir_verify` reads this rather than
    /// carrying a second copy of the table.
    pub fn arity(self) -> usize {
        match self {
            ScalarOp::AbsI
            | ScalarOp::AbsL
            | ScalarOp::NlzI
            | ScalarOp::NlzL
            | ScalarOp::NtzI
            | ScalarOp::NtzL
            | ScalarOp::ReverseBytesI
            | ScalarOp::ReverseBytesL
            | ScalarOp::LowestOneBitI
            | ScalarOp::LowestOneBitL
            | ScalarOp::HighestOneBitI
            | ScalarOp::HighestOneBitL
            | ScalarOp::AbsF
            | ScalarOp::AbsD => 1,
            _ => 2,
        }
    }

    /// Does this family operate in XMM registers rather than general-purpose
    /// ones?
    ///
    /// The lowering arm loads `inputs[0]` into RAX before it dispatches, which
    /// is wrong for an FP family; this is the predicate that guards it. Kept as
    /// its own question rather than derived from `result_type`, because
    /// `Long.numberOfLeadingZeros` already proves operand width and result
    /// width are independent here.
    pub fn is_fp(self) -> bool {
        matches!(self, ScalarOp::AbsF | ScalarOp::AbsD)
    }

    /// The type of data input `idx`.
    ///
    /// A per-input answer rather than a per-op one, because the ROTATES are
    /// mixed: `Long.rotateLeft` takes a `long` VALUE and an `int` DISTANCE.
    /// Reading [`Self::operands_are_long`] for both would type the distance as
    /// a `long`, and a mistyped stack entry is joined against a real one at the
    /// next merge.
    pub fn input_ty(self, idx: usize) -> IrType {
        match self {
            ScalarOp::RotateLeftI
            | ScalarOp::RotateLeftL
            | ScalarOp::RotateRightI
            | ScalarOp::RotateRightL
                if idx == 1 =>
            {
                IrType::Int
            }
            ScalarOp::AbsF => IrType::Float,
            ScalarOp::AbsD => IrType::Double,
            _ if self.operands_are_long() => IrType::Long,
            _ => IrType::Int,
        }
    }

    /// The node's result type.
    pub fn result_type(self) -> IrType {
        match self {
            ScalarOp::MinL
            | ScalarOp::MaxL
            | ScalarOp::AbsL
            | ScalarOp::ReverseBytesL
            | ScalarOp::LowestOneBitL
            | ScalarOp::HighestOneBitL
            | ScalarOp::RotateLeftL
            | ScalarOp::RotateRightL => IrType::Long,
            ScalarOp::AbsF => IrType::Float,
            ScalarOp::AbsD => IrType::Double,
            // `Integer.compare`, `Long.compare` and BOTH widths of
            // `numberOfLeading/TrailingZeros` return `int` --
            // `Long.numberOfLeadingZeros` takes a `long` and answers an `int`,
            // which is why this table and `operands_are_long` are two tables
            // and not one.
            _ => IrType::Int,
        }
    }

    /// True when the operands are 64-bit. Distinct from [`Self::result_type`]
    /// precisely because `Long.compare` takes `long`s and returns an `int` --
    /// reading the result type to size the `CMP` is the bug this pair exists to
    /// make impossible.
    pub fn operands_are_long(self) -> bool {
        matches!(
            self,
            ScalarOp::MinL
                | ScalarOp::MaxL
                | ScalarOp::AbsL
                | ScalarOp::CompareL
                | ScalarOp::NlzL
                | ScalarOp::NtzL
                | ScalarOp::ReverseBytesL
                | ScalarOp::LowestOneBitL
                | ScalarOp::HighestOneBitL
                | ScalarOp::RotateLeftL
                | ScalarOp::RotateRightL
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ScalarOp::MinI => "Math.min(II)",
            ScalarOp::MaxI => "Math.max(II)",
            ScalarOp::MinL => "Math.min(JJ)",
            ScalarOp::MaxL => "Math.max(JJ)",
            ScalarOp::AbsI => "Math.abs(I)",
            ScalarOp::AbsL => "Math.abs(J)",
            ScalarOp::CompareI => "Integer.compare(II)",
            ScalarOp::CompareL => "Long.compare(JJ)",
            ScalarOp::NlzI => "Integer.numberOfLeadingZeros(I)",
            ScalarOp::NlzL => "Long.numberOfLeadingZeros(J)",
            ScalarOp::NtzI => "Integer.numberOfTrailingZeros(I)",
            ScalarOp::NtzL => "Long.numberOfTrailingZeros(J)",
            ScalarOp::ReverseBytesI => "Integer.reverseBytes(I)",
            ScalarOp::ReverseBytesL => "Long.reverseBytes(J)",
            ScalarOp::LowestOneBitI => "Integer.lowestOneBit(I)",
            ScalarOp::LowestOneBitL => "Long.lowestOneBit(J)",
            ScalarOp::HighestOneBitI => "Integer.highestOneBit(I)",
            ScalarOp::HighestOneBitL => "Long.highestOneBit(J)",
            ScalarOp::RotateLeftI => "Integer.rotateLeft(II)",
            ScalarOp::RotateLeftL => "Long.rotateLeft(JI)",
            ScalarOp::RotateRightI => "Integer.rotateRight(II)",
            ScalarOp::RotateRightL => "Long.rotateRight(JI)",
            ScalarOp::AbsF => "Math.abs(F)",
            ScalarOp::AbsD => "Math.abs(D)",
        }
    }
}

/// Does this call site name a family the optimizing tier lowers as arithmetic?
///
/// The single source of truth, consulted from BOTH sides: `try_compile_inner`
/// asks it to decide whether a `JitIntrinsic` site still refuses the method,
/// and `IrBuilder` asks it again to decide what to emit. Two enumerations of
/// one question is the failure this file has recorded more than once, so there
/// is one.
///
/// `StrictMath` is accepted alongside `Math` for `min`/`max`/`abs` only: those
/// three are specified identically in both classes (JLS 15.20, `StrictMath`'s
/// own javadoc delegates), unlike the transcendentals, which are not.
/// The unboxing accessors, as a GUARDED FIELD LOAD rather than a refusal.
///
/// `ScalarOp` covers call-site intrinsics that are pure register arithmetic.
/// These are not: they read a field, and the byte offset of that field depends
/// on how the INSTANCE was allocated -- a compact instance stores the payload
/// at a registered body offset, a legacy one inside its 16-byte `Value` cell,
/// and both shapes exist at once because different allocators build different
/// cells. That per-object branch is why this family sat on the refusal list
/// while the arithmetic ones were lowered.
///
/// The node carries only the guard class id. The offsets are re-derived at
/// LOWERING time from that id, through the very same `AtomicLongFieldLayout` /
/// `AtomicIntFieldLayout` the single-pass emitter uses, so the two backends
/// cannot drift into disagreeing about where the field is -- which, for a raw
/// load, is the difference between a value and a wild read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnboxOp {
    /// `java/lang/Long.longValue()J` -- 8-byte payload.
    LongValue,
    /// `java/lang/Integer.intValue()I` -- 4-byte payload.
    IntValue,

    // -- The `Atomic*` accessors, 2026-09-08 ----------------------------
    //
    // Same node, same three guards, same per-object layout branch: these are
    // the SAME shape as the two above -- a guarded field access at slot 0 of a
    // receiver whose class id the planner resolved -- differing only in the
    // instruction at the bottom and, for one of them, an argument.
    //
    // They joined `UnboxOp` rather than arriving as a second op with a second
    // recognizer and a second planner clause, which is what a first cut of this
    // work did (`claude/unboxing-atomic-ir-nodes-d7aff4`). Two recognizers both
    // claiming `Long.longValue` is one recognizer plus dead code that still
    // compiles and still has passing tests -- the failure this file has a
    // standing rule about.
    //
    // The name is now a little narrow for its contents. It is kept because
    // every reference to it is, and because "the guarded slot-0 accessor
    // families" has no shorter spelling.
    /// `AtomicLong.get()J` -- a VOLATILE 8-byte load. An aligned 8-byte `MOV`
    /// is atomic on x86-64 and loads are not reordered with older loads under
    /// TSO, so the acquire is owed nothing: the same plain `MOV`
    /// [`Self::LongValue`] emits is the correct encoding.
    AtomicLongGet,
    /// `AtomicInteger.incrementAndGet()I` -- `LOCK XADD` of `+1`, returning the
    /// POST-add value.
    AtomicIntIncrementAndGet,
    /// `AtomicInteger.decrementAndGet()I` -- `LOCK XADD` of `-1`, POST-add.
    AtomicIntDecrementAndGet,
    /// `AtomicLong.getAndAdd(J)J` -- `LOCK XADD` of a runtime delta, returning
    /// the PRE-add value, which is what `XADD` leaves in its source register
    /// with no fixup at all.
    AtomicLongGetAndAdd,
}

impl UnboxOp {
    pub fn result_type(self) -> IrType {
        if self.is_wide() {
            IrType::Long
        } else {
            IrType::Int
        }
    }

    /// True when the field access is 64-bit (`REX.W`) rather than 32-bit.
    pub fn is_wide(self) -> bool {
        matches!(
            self,
            UnboxOp::LongValue | UnboxOp::AtomicLongGet | UnboxOp::AtomicLongGetAndAdd
        )
    }

    /// True for the families that only READ the field -- a `MOV`, with no
    /// `LOCK` prefix and no delta register.
    pub fn is_load(self) -> bool {
        matches!(
            self,
            UnboxOp::LongValue | UnboxOp::IntValue | UnboxOp::AtomicLongGet
        )
    }

    /// How many DATA inputs the node takes past `[ctrl, mem, receiver]`.
    /// `ir_verify` reads this rather than carrying a second copy of the table.
    pub fn arity(self) -> usize {
        match self {
            UnboxOp::AtomicLongGetAndAdd => 1,
            _ => 0,
        }
    }

    /// The `LOCK XADD` delta when it is a compile-time immediate, or `None`
    /// when the family has no delta (the loads) or takes it as an argument.
    ///
    /// `+1`/`-1` are the only immediates here.
    pub fn delta_imm(self) -> Option<i32> {
        match self {
            UnboxOp::AtomicIntIncrementAndGet => Some(1),
            UnboxOp::AtomicIntDecrementAndGet => Some(-1),
            _ => None,
        }
    }

    /// True when the result is the POST-add value, so the delta must be added
    /// back after the `XADD` (which leaves the PRE-add value in its source).
    pub fn returns_post_add(self) -> bool {
        matches!(
            self,
            UnboxOp::AtomicIntIncrementAndGet | UnboxOp::AtomicIntDecrementAndGet
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            UnboxOp::LongValue => "java/lang/Long.longValue",
            UnboxOp::IntValue => "java/lang/Integer.intValue",
            UnboxOp::AtomicLongGet => "java/util/concurrent/atomic/AtomicLong.get",
            UnboxOp::AtomicIntIncrementAndGet => {
                "java/util/concurrent/atomic/AtomicInteger.incrementAndGet"
            }
            UnboxOp::AtomicIntDecrementAndGet => {
                "java/util/concurrent/atomic/AtomicInteger.decrementAndGet"
            }
            UnboxOp::AtomicLongGetAndAdd => "java/util/concurrent/atomic/AtomicLong.getAndAdd",
        }
    }
}

/// Recognise an unboxing call site the optimizing tier can lower.
///
/// `class_id` is the receiver guard, resolved from the constant pool by the
/// caller. Id 0 means "unresolved", and the layout resolvers refuse it -- a
/// site with no class id must decline, because the emitter has nothing to
/// derive an offset from.
pub fn try_ir_unbox_intrinsic(
    class: &str,
    method: &str,
    descriptor: &str,
    class_id: u32,
) -> Option<UnboxOp> {
    if !ir_scalar_intrinsics_enabled() || class_id == 0 {
        return None;
    }
    let op = match (class, method, descriptor) {
        ("java/lang/Long", "longValue", "()J") => UnboxOp::LongValue,
        ("java/lang/Integer", "intValue", "()I") => UnboxOp::IntValue,
        ("java/util/concurrent/atomic/AtomicLong", "get", "()J") => UnboxOp::AtomicLongGet,
        ("java/util/concurrent/atomic/AtomicInteger", "incrementAndGet", "()I") => {
            UnboxOp::AtomicIntIncrementAndGet
        }
        ("java/util/concurrent/atomic/AtomicInteger", "decrementAndGet", "()I") => {
            UnboxOp::AtomicIntDecrementAndGet
        }
        ("java/util/concurrent/atomic/AtomicLong", "getAndAdd", "(J)J") => {
            UnboxOp::AtomicLongGetAndAdd
        }
        _ => return None,
    };
    // Ask the layout NOW as well as at lowering: a class whose `value` field is
    // not an 8/4-byte scalar at a resolvable offset must be refused at the
    // planner, not discovered by the emitter after the method was admitted.
    unbox_offsets(op, class_id)?;
    Some(op)
}

/// `(compact, legacy)` byte offsets of the payload, from the SAME resolver the
/// single-pass backend uses. The single source of truth for both backends.
pub fn unbox_offsets(op: UnboxOp, class_id: u32) -> Option<(i32, i32)> {
    // The WIDTH decides which resolver applies, and it is read off the family
    // rather than off the class name: `AtomicLong` and `AtomicInteger` differ by
    // exactly this, and pointing the 64-bit form at a 4-byte payload is a wrong
    // VALUE that no test of the 32-bit form would catch. Both constructors
    // carry the width check too -- they refuse unless slot 0's compact storage
    // is exactly 8 / 4 bytes -- so a layout the access could not address never
    // reaches codegen.
    if op.is_wide() {
        crate::AtomicLongFieldLayout::new(0, class_id)
            .map(|l| (l.value_compact_offset, l.value_legacy_offset))
    } else {
        crate::AtomicIntFieldLayout::new(0, class_id)
            .map(|l| (l.value_compact_offset, l.value_legacy_offset))
    }
}

/// Unbox sites the optimizing tier LOWERED, for the engagement census.
static UNBOX_LOWERED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn note_unbox_lowered() {
    UNBOX_LOWERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

pub fn unbox_lowered() -> u64 {
    UNBOX_LOWERED.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn try_ir_scalar_intrinsic(class: &str, method: &str, descriptor: &str) -> Option<ScalarOp> {
    if !ir_scalar_intrinsics_enabled() {
        return None;
    }
    match (class, method, descriptor) {
        ("java/lang/Math" | "java/lang/StrictMath", "min", "(II)I") => Some(ScalarOp::MinI),
        ("java/lang/Math" | "java/lang/StrictMath", "max", "(II)I") => Some(ScalarOp::MaxI),
        ("java/lang/Math" | "java/lang/StrictMath", "min", "(JJ)J") => Some(ScalarOp::MinL),
        ("java/lang/Math" | "java/lang/StrictMath", "max", "(JJ)J") => Some(ScalarOp::MaxL),
        ("java/lang/Math" | "java/lang/StrictMath", "abs", "(I)I") => Some(ScalarOp::AbsI),
        ("java/lang/Math" | "java/lang/StrictMath", "abs", "(J)J") => Some(ScalarOp::AbsL),
        ("java/lang/Integer", "compare", "(II)I") => Some(ScalarOp::CompareI),
        ("java/lang/Long", "compare", "(JJ)I") => Some(ScalarOp::CompareL),
        // `Integer.min`/`max` and `Long.min`/`max` are one-line delegations to
        // the `Math` methods directly above — `Integer.min(a, b)` IS
        // `Math.min(a, b)` in the JDK source — so they lower to the identical
        // sequence and need no new `ScalarOp`.
        //
        // They are here because they are separately native-shadowed in this VM,
        // and a shadowed leaf is an inlining barrier as well as an
        // un-compilable method: the caller cannot splice it (there is no
        // bytecode to splice) and, without a row here, cannot lower it as
        // arithmetic either, so a two-instruction operation costs a call
        // through Rust. `Math.min` was already covered; these three-character
        // spellings of it were not, and `Integer.max(a, b)` is not a rare way
        // to write it.
        ("java/lang/Integer", "min", "(II)I") => Some(ScalarOp::MinI),
        ("java/lang/Integer", "max", "(II)I") => Some(ScalarOp::MaxI),
        ("java/lang/Long", "min", "(JJ)J") => Some(ScalarOp::MinL),
        ("java/lang/Long", "max", "(JJ)J") => Some(ScalarOp::MaxL),
        ("java/lang/Integer", "numberOfLeadingZeros", "(I)I") => Some(ScalarOp::NlzI),
        ("java/lang/Long", "numberOfLeadingZeros", "(J)I") => Some(ScalarOp::NlzL),
        ("java/lang/Integer", "numberOfTrailingZeros", "(I)I") => Some(ScalarOp::NtzI),
        ("java/lang/Long", "numberOfTrailingZeros", "(J)I") => Some(ScalarOp::NtzL),
        ("java/lang/Integer", "reverseBytes", "(I)I") => Some(ScalarOp::ReverseBytesI),
        ("java/lang/Long", "reverseBytes", "(J)J") => Some(ScalarOp::ReverseBytesL),
        ("java/lang/Integer", "lowestOneBit", "(I)I") => Some(ScalarOp::LowestOneBitI),
        ("java/lang/Long", "lowestOneBit", "(J)J") => Some(ScalarOp::LowestOneBitL),
        ("java/lang/Integer", "highestOneBit", "(I)I") => Some(ScalarOp::HighestOneBitI),
        ("java/lang/Long", "highestOneBit", "(J)J") => Some(ScalarOp::HighestOneBitL),
        ("java/lang/Integer", "rotateLeft", "(II)I") => Some(ScalarOp::RotateLeftI),
        ("java/lang/Long", "rotateLeft", "(JI)J") => Some(ScalarOp::RotateLeftL),
        ("java/lang/Integer", "rotateRight", "(II)I") => Some(ScalarOp::RotateRightI),
        ("java/lang/Long", "rotateRight", "(JI)J") => Some(ScalarOp::RotateRightL),
        ("java/lang/Math", "abs", "(F)F") => Some(ScalarOp::AbsF),
        ("java/lang/Math", "abs", "(D)D") => Some(ScalarOp::AbsD),
        _ => None,
    }
}

#[cfg(test)]
mod scalar_intrinsic_recognizer_tests {
    use super::*;

    /// The site-trap registry is what lets the runtime tell an IR SITE TRAP
    /// apart from genuinely unreachable code, and the two want OPPOSITE
    /// actions: unreachable code should blacklist the method, a site trap must
    /// not -- `MakeNotCompilable` is consulted by `compile_gate` itself, so it
    /// takes away the single-pass body too, and single-pass lowers the very
    /// opcodes the trap was planted for.
    ///
    /// Measured before the split existed: `MVStore.getMapId` on H2 trapped
    /// once and lost its body on every tier.
    #[test]
    fn a_registered_site_trap_method_is_distinguishable_from_unreachable_code() {
        // The registry is PROCESS-GLOBAL and this binary runs thousands of
        // tests, some of which compile real fixtures and register real methods;
        // the memo key is a hash, so an unlucky collision is possible too. So
        // this asserts the DISCRIMINATION -- registering one method changes
        // that method's answer and nothing else's -- rather than asserting the
        // registry starts empty, which is not this test's to control. A first
        // draft asserted the precondition and failed in whichever of the two
        // feature configurations happened to run a colliding fixture first.
        let h =
            crate::ir_method_memo_hash("cratonvm/test/SiteTrapRegistryProbe", "trapping", "()V");
        let other =
            crate::ir_method_memo_hash("cratonvm/test/SiteTrapRegistryProbe", "untrapped", "()V");
        assert_ne!(h, other, "distinct methods must not share a memo key");
        let other_before = method_has_site_trap(other);
        register_site_trap_method(h);
        assert!(
            method_has_site_trap(h),
            "a registered method must be recognisable -- this is what stops the              runtime blacklisting it on every tier",
        );
        assert_eq!(
            method_has_site_trap(other),
            other_before,
            "registering one method must not implicate another: the runtime              uses this answer to choose between recompiling and blacklisting",
        );
    }

    /// An uncommon trap is only a slow path if the interpreter can get back
    /// to the bytecode. This tier publishes no resumable deopt on a production
    /// artifact, so the ONLY fallback is the whole-method replay from entry —
    /// and the interpreter refuses that, fatally, once the prefix before the
    /// trap has committed something the replay would duplicate.
    ///
    /// This is the `com/sun/tools/javac/code/Scope$ScopeImpl.remove` shape,
    /// spelled in bytecode: `Assert.check(...)` (an `invokestatic`) at bci 5,
    /// then the `invokedynamic` the tier cannot lower at bci 8. Planting there
    /// is what made every in-process javac compile under Spring's
    /// `TestCompiler` die with `InternalError: precise deoptimization
    /// unavailable ... refusing side-effecting replay` — a compile that failed
    /// with zero diagnostics.
    #[test]
    fn a_trap_after_a_side_effect_is_refused() {
        let builder = IrBuilder::new(1, 1);
        // 0: aload_0, 1: aload_0, 2..4: invokestatic #1, 5..7: getstatic #2,
        // 8..12: invokedynamic #3
        let code = [
            0x2a, 0x2a, 0xb8, 0x00, 0x01, 0xb2, 0x00, 0x02, 0xba, 0x00, 0x03, 0x00, 0x00,
        ];
        assert!(
            !builder.trap_replay_is_safe(&code, code.len(), 8),
            "an invokestatic at bci 2 is a committed side effect, so a replay             from entry would duplicate it and the interpreter refuses — the trap             at bci 8 must not be planted",
        );
    }

    /// The other half of the same rule, and the reason it is a PREFIX test and
    /// not "does this body commit anything anywhere". A trap reached before
    /// the method has done anything replays harmlessly: the locals are rebuilt
    /// from the same arguments and nothing outside the frame was written. The
    /// side effect AFTER the trap never ran — every deopt point this VM emits
    /// is `REEXECUTE`, so the bytecode at the trap had not completed.
    ///
    /// Refusing this shape too would cost the optimizing tier every method
    /// that opens with a lambda, for a hazard it does not have.
    #[test]
    fn a_trap_before_any_side_effect_is_admitted() {
        let builder = IrBuilder::new(1, 1);
        // 0: aload_0, 1..5: invokedynamic #1, 6..8: invokestatic #2 (AFTER)
        let code = [0x2a, 0xba, 0x00, 0x01, 0x00, 0x00, 0xb8, 0x00, 0x02];
        assert!(
            builder.trap_replay_is_safe(&code, code.len(), 1),
            "nothing before bci 1 commits anything, and the invokestatic at bci             6 never ran — this replay is observably the abandoned attempt",
        );
    }

    /// Clause 2 of the consumer's rule, which the prefix test cannot see: a
    /// spliced artifact's abandoned attempt also ran part of a RELOCATED
    /// callee body, and the caller's own bytecode does not describe it. The
    /// interpreter reads the same one number off the artifact
    /// (`spliced_bodies_side_effect_free`), so the producer has to ask it too.
    #[test]
    fn an_impure_splice_refuses_a_trap_a_pure_prefix_would_admit() {
        let code = [0x2a, 0xba, 0x00, 0x01, 0x00, 0x00, 0xb8, 0x00, 0x02];
        let mut builder = IrBuilder::new(1, 1);
        builder.set_spliced_bodies_pure(false);
        assert!(
            !builder.trap_replay_is_safe(&code, code.len(), 1),
            "the prefix is pure but a spliced body is not, and the replay             would re-run that body's side effects",
        );
        // Same bytes, same bci, only the splice answer differs — so this pair
        // isolates the clause rather than merely exercising it.
        let mut pure = IrBuilder::new(1, 1);
        pure.set_spliced_bodies_pure(true);
        assert!(pure.trap_replay_is_safe(&code, code.len(), 1));
    }

    /// A body that commits nothing ANYWHERE needs no reasoning about where the
    /// attempt stopped — clause 1, and the one clause that answers `true` even
    /// for a trap late in the method. It also has to hold with an impure
    /// splice recorded, because the consumer asks it first, before it looks at
    /// the splice answer at all; a producer that ordered the clauses the other
    /// way would refuse traps the interpreter would have accepted.
    ///
    /// The body here traps on an unresolved `checkcast`, not an
    /// `invokedynamic`, and that is the point — see
    /// [`clause_one_can_never_fire_for_an_invokedynamic_body`].
    #[test]
    fn a_body_that_commits_nothing_admits_a_trap_anywhere() {
        let mut builder = IrBuilder::new(1, 1);
        builder.set_spliced_bodies_pure(false);
        // 0: aload_0, 1..3: checkcast #1, 4: areturn
        let code = [0x2a, 0xc0, 0x00, 0x01, 0xb0];
        assert!(
            builder.trap_replay_is_safe(&code, code.len(), 1),
            "no store, no call, no monitor action anywhere in this body",
        );
    }

    /// `invokedynamic` is `0xba`, and `opcode_commits_side_effect` commits the
    /// whole `0xb6..=0xba` invoke range — so a body containing one can never
    /// satisfy clause 1, no matter how pure the rest of it is. Every indy trap
    /// is therefore decided by the PREFIX clause alone.
    ///
    /// This is worth a test of its own because it is the difference between
    /// the rule as stated and the rule as it runs, and the first version of
    /// the clause-1 test above got it wrong: it used an indy body, asserted
    /// clause 1, and failed. Left as an assertion so that widening or
    /// narrowing `opcode_commits_side_effect` says so here instead of quietly
    /// changing which methods the optimizing tier will trap in.
    #[test]
    fn clause_one_can_never_fire_for_an_invokedynamic_body() {
        // 0: aload_0, 1: pop, 2..6: invokedynamic #1, 7: return — nothing but
        // the indy itself could commit anything.
        let code = [0x2a, 0x57, 0xba, 0x00, 0x01, 0x00, 0x00, 0xb1];
        assert!(
            crate::bytecode_commits_side_effect(&code, code.len()),
            "the indy opcode is itself in the side-effect set",
        );
        let mut impure = IrBuilder::new(1, 1);
        impure.set_spliced_bodies_pure(false);
        assert!(
            !impure.trap_replay_is_safe(&code, code.len(), 2),
            "clause 1 cannot rescue it, so the impure splice decides",
        );
        let mut pure = IrBuilder::new(1, 1);
        pure.set_spliced_bodies_pure(true);
        assert!(
            pure.trap_replay_is_safe(&code, code.len(), 2),
            "with the splice clause satisfied the PREFIX is what admits it",
        );
    }

    /// `code` may be the COMBINED buffer — the method body, then the spliced
    /// bodies appended after it — while `code_len` is the method's own length.
    /// Clause 1 has to be bounded by `code_len`, or a side effect in a
    /// relocated callee would make the whole-body test read "impure" for a
    /// method that is pure, and the trap would be refused for the wrong
    /// reason. The prefix clause then still answers correctly.
    #[test]
    fn the_whole_body_clause_stops_at_the_methods_own_length() {
        let builder = IrBuilder::new(1, 1);
        // Method body is the first 8 bytes (pure); bytes 8.. are a relocated
        // callee that stores to a static.
        let code = [
            0x2a, 0x57, 0xba, 0x00, 0x01, 0x00, 0x00, 0xb1, // method (len 8)
            0x2a, 0xb3, 0x00, 0x04, 0xb1, // spliced callee: putstatic
        ];
        assert!(
            builder.trap_replay_is_safe(&code, 8, 2),
            "the relocated tail is not this method's body and must not decide             clause 1",
        );
    }

    /// The site-trap decision must be claimable exactly ONCE, and must not be
    /// confused with the IR refusal memo.
    ///
    /// The first version read `ir_evidence::method_already_refused` as the
    /// "already decided" flag. That memo has a second writer -- the acceptance
    /// gate marks a method refused whenever it discards an optimizing body --
    /// so a method that got a trapping IR body, was recompiled, and had THAT
    /// recompile refused would look already-decided on its FIRST trap. The
    /// policy would never be applied and the trapping artifact never evicted:
    /// a trap firing forever with nothing recorded. Two writers, two meanings,
    /// two sets.
    #[test]
    fn the_site_trap_decision_is_claimable_exactly_once_and_is_not_the_refusal_memo() {
        let h = crate::ir_method_memo_hash("cratonvm/test/SiteTrapDecisionProbe", "claimed", "()V");
        assert!(claim_site_trap_decision(h), "the first claim must succeed");
        assert!(
            !claim_site_trap_decision(h),
            "a second claim must fail -- otherwise every trap re-applies the              policy and the per-method deopt count escalates to blacklisting",
        );

        // The refusal memo must NOT be able to pre-empt a decision.
        let g = crate::ir_method_memo_hash(
            "cratonvm/test/SiteTrapDecisionProbe",
            "gate_refused_first",
            "()V",
        );
        crate::ir_evidence::note_method_refused(g);
        assert!(
            crate::ir_evidence::method_already_refused(g),
            "precondition: the gate has marked this method IR-refused",
        );
        assert!(
            claim_site_trap_decision(g),
            "a gate refusal must not consume the site-trap decision: the first              trap still has to evict the trapping artifact",
        );
    }

    /// The unbox recognizer must decline an UNRESOLVED receiver class.
    ///
    /// `class_id == 0` means the constant pool did not resolve the receiver.
    /// The lowering derives its byte offsets from that id, so a site admitted
    /// without one would have the emitter loading from an offset nobody
    /// vouched for -- which for a raw field load is a wild read, not a wrong
    /// answer. This is the same refusal `box_unbox_intrinsic_shape` makes for
    /// the single-pass backend, and the two must agree.
    #[test]
    fn the_unbox_recognizer_declines_an_unresolved_receiver_class() {
        assert!(
            try_ir_unbox_intrinsic("java/lang/Long", "longValue", "()J", 0).is_none(),
            "class_id 0 is 'unresolved' and must never be admitted",
        );
        assert!(try_ir_unbox_intrinsic("java/lang/Integer", "intValue", "()I", 0).is_none(),);
    }

    /// Only the two triples, and only with their exact descriptors. A
    /// near-miss must fall through to the ordinary path rather than be lowered
    /// as a field load of something else.
    #[test]
    fn the_unbox_recognizer_matches_only_its_declared_triples() {
        for (c, n, d) in [
            ("java/lang/Long", "intValue", "()I"),
            ("java/lang/Integer", "longValue", "()J"),
            ("java/lang/Long", "longValue", "()I"),
            ("java/lang/Double", "doubleValue", "()D"),
            ("java/lang/Short", "shortValue", "()S"),
            // The BOXING half: `valueOf` allocates or reads a cache, and has no
            // receiver whose slot 0 could be read.
            ("java/lang/Long", "valueOf", "(J)Ljava/lang/Long;"),
            ("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;"),
            // Right name, wrong owner: `Number.longValue` is abstract and its
            // receiver may be any subclass, so slot 0 means nothing.
            ("java/lang/Number", "longValue", "()J"),
            // Real `Atomic*` methods this family deliberately does NOT claim.
            // `compareAndSet` is a `LOCK CMPXCHG` whose operand placement
            // differs in a way that compiles fine while comparing the object
            // pointer against the field; the `getAnd*`/`addAndGet` spellings are
            // absent because the H2 census does not name them, and a family
            // added without a site to exercise it is one whose first real
            // receiver is a user's.
            (
                "java/util/concurrent/atomic/AtomicLong",
                "compareAndSet",
                "(JJ)Z",
            ),
            (
                "java/util/concurrent/atomic/AtomicLong",
                "getAndIncrement",
                "()J",
            ),
            (
                "java/util/concurrent/atomic/AtomicLong",
                "addAndGet",
                "(J)J",
            ),
            ("java/util/concurrent/atomic/AtomicInteger", "get", "()I"),
            (
                "java/util/concurrent/atomic/AtomicInteger",
                "getAndAdd",
                "(I)I",
            ),
            // Right owner and name, wrong descriptor.
            (
                "java/util/concurrent/atomic/AtomicLong",
                "getAndAdd",
                "(I)I",
            ),
        ] {
            assert!(
                try_ir_unbox_intrinsic(c, n, d, 7).is_none(),
                "{c}.{n}{d} must not be recognised as an unbox site",
            );
        }
    }

    /// The triple each declared family must be recognised from, as an
    /// EXHAUSTIVE match: a new `UnboxOp` variant is a COMPILE ERROR here until
    /// its signature is declared.
    ///
    /// A `match` rather than a hand-written table, because the failure this
    /// catches is a family added to the enum and its lowering but forgotten in
    /// the recognizer -- which reads, from the outside, exactly like a workload
    /// that has no such call site, and which a table only catches if someone
    /// remembers to add a row.
    #[cfg(test)]
    fn unbox_triple_for(op: UnboxOp) -> (&'static str, &'static str, &'static str) {
        match op {
            UnboxOp::LongValue => ("java/lang/Long", "longValue", "()J"),
            UnboxOp::IntValue => ("java/lang/Integer", "intValue", "()I"),
            UnboxOp::AtomicLongGet => ("java/util/concurrent/atomic/AtomicLong", "get", "()J"),
            UnboxOp::AtomicIntIncrementAndGet => (
                "java/util/concurrent/atomic/AtomicInteger",
                "incrementAndGet",
                "()I",
            ),
            UnboxOp::AtomicIntDecrementAndGet => (
                "java/util/concurrent/atomic/AtomicInteger",
                "decrementAndGet",
                "()I",
            ),
            UnboxOp::AtomicLongGetAndAdd => (
                "java/util/concurrent/atomic/AtomicLong",
                "getAndAdd",
                "(J)J",
            ),
        }
    }

    #[cfg(test)]
    const ALL_UNBOX_OPS: [UnboxOp; 6] = [
        UnboxOp::LongValue,
        UnboxOp::IntValue,
        UnboxOp::AtomicLongGet,
        UnboxOp::AtomicIntIncrementAndGet,
        UnboxOp::AtomicIntDecrementAndGet,
        UnboxOp::AtomicLongGetAndAdd,
    ];

    #[test]
    fn every_declared_unbox_family_is_recognised() {
        for op in ALL_UNBOX_OPS {
            let (c, m, d) = unbox_triple_for(op);
            assert_eq!(
                try_ir_unbox_intrinsic(c, m, d, 7),
                Some(op),
                "{c}.{m}{d} is declared as an unbox family and was not recognised",
            );
        }
    }

    /// The width and delta plans, checked against each family's DESCRIPTOR
    /// rather than against the tables under test.
    ///
    /// A wrong answer in either is a wrong VALUE, not a slow one: an `IntValue`
    /// mistyped wide reads four bytes of the next field into the answer, and a
    /// `getAndAdd` that claimed `returns_post_add` would add the delta twice.
    #[test]
    fn unbox_width_and_delta_plans_match_each_descriptor() {
        for op in ALL_UNBOX_OPS {
            let (_, m, d) = unbox_triple_for(op);
            let wide = d.ends_with(")J");
            assert_eq!(op.is_wide(), wide, "{}", op.as_str());
            assert_eq!(
                op.result_type(),
                if wide { IrType::Long } else { IrType::Int },
                "{}",
                op.as_str(),
            );
            assert_eq!(
                op.arity(),
                usize::from(!d.starts_with("()")),
                "{} disagrees with its descriptor about the argument count",
                op.as_str(),
            );
            // Exactly one delta source: an immediate, an argument, or neither.
            assert!(
                !(op.arity() == 1 && op.delta_imm().is_some()),
                "{} claims both an immediate and an argument delta",
                op.as_str(),
            );
            assert_eq!(
                op.is_load(),
                op.arity() == 0 && op.delta_imm().is_none(),
                "{} disagrees about whether it has a delta at all",
                op.as_str(),
            );
            // `XADD` leaves the PRE-add value in its source, so only the
            // `*AndGet` spellings owe the fixup -- and `getAndAdd`, whose name
            // also ends in "AndGet" if read carelessly, must not.
            assert_eq!(
                op.returns_post_add(),
                m.ends_with("AndGet") && !m.starts_with("getAnd"),
                "{}",
                op.as_str(),
            );
        }
    }

    /// `unbox_offsets` is the SINGLE source of truth for both backends. If this
    /// ever stopped agreeing with `AtomicLongFieldLayout`/`AtomicIntFieldLayout`
    /// the optimizing tier and the single-pass tier would read different halves
    /// of the same object.
    #[test]
    fn unbox_offsets_come_from_the_same_resolver_the_single_pass_backend_uses() {
        const CID: u32 = 42;
        // Every family, driven off the same list the recognizer test uses, so a
        // new one cannot be added with its width silently pointed at the wrong
        // resolver.
        for op in ALL_UNBOX_OPS {
            let want = if op.is_wide() {
                crate::AtomicLongFieldLayout::new(0, CID)
                    .map(|l| (l.value_compact_offset, l.value_legacy_offset))
            } else {
                crate::AtomicIntFieldLayout::new(0, CID)
                    .map(|l| (l.value_compact_offset, l.value_legacy_offset))
            };
            assert_eq!(unbox_offsets(op, CID), want, "{}", op.as_str());
        }
    }

    /// The per-build counter is what carries "this build planted a trap" back
    /// to `lib.rs`, because `build(mut self, ..)` consumes the builder. It must
    /// start at zero for every build or one trapping method would register
    /// every method compiled after it on the same thread.
    #[test]
    fn the_per_build_trap_count_starts_at_zero() {
        reset_site_traps_this_build();
        assert_eq!(site_traps_planted_this_build(), 0);
        SITE_TRAPS_THIS_BUILD.with(|c| c.set(3));
        assert_eq!(site_traps_planted_this_build(), 3);
        reset_site_traps_this_build();
        assert_eq!(
            site_traps_planted_this_build(),
            0,
            "a stale count would register a method that planted nothing",
        );
    }

    #[test]
    fn the_build_results_scope_clears_both_channels_on_entry_and_on_drop() {
        SITE_TRAPS_THIS_BUILD.with(|c| c.set(2));
        note_string_access_site(7);
        {
            let scope = IrBuildResultsScope::enter();
            assert_eq!(
                scope.site_traps_planted(),
                0,
                "a stale count from an earlier build"
            );
            assert!(scope.string_access_site_pcs().is_empty());
            SITE_TRAPS_THIS_BUILD.with(|c| c.set(1));
            note_string_access_site(9);
            assert_eq!(scope.site_traps_planted(), 1);
            assert_eq!(scope.string_access_site_pcs(), vec![9]);
        }
        assert_eq!(
            site_traps_planted_this_build(),
            0,
            "a bailed build left its count"
        );
        assert!(string_access_site_pcs().is_empty());
    }

    // RESTORED 2026-09-07. The site-trap tests above were inserted between
    // this test's doc comment and its `fn`, which orphaned the comment onto
    // the first of them and left this function with NO `#[test]` attribute —
    // so the one check that a `ScalarOp` family cannot be added without its
    // recognizer signature had silently stopped running, and
    // `cargo clippy --workspace --all-targets -- -D warnings` was red on
    // `duplicated attribute` for everyone.
    /// Every family the recognizer claims must actually be recognised.
    ///
    /// A table rather than a spot check, because the failure this catches is a
    /// family added to `ScalarOp` and its lowering but forgotten in the match —
    /// which reads, from the outside, exactly like a workload that has no such
    /// call site.
    #[test]
    fn every_declared_family_is_recognised() {
        // Driven off an EXHAUSTIVE match rather than a hand-kept list. The
        // previous version was a list of tuples, which could not catch the one
        // failure this test exists for: a family added to the enum and to the
        // lowering but missing from the recognizer reads, from outside,
        // exactly like a workload with no such call site -- the census simply
        // does not count it. With the match below, adding a variant without
        // declaring its signature is a COMPILE error.
        fn declared_signature(sop: ScalarOp) -> (&'static str, &'static str, &'static str) {
            match sop {
                ScalarOp::MinI => ("java/lang/Math", "min", "(II)I"),
                ScalarOp::MaxI => ("java/lang/Math", "max", "(II)I"),
                ScalarOp::MinL => ("java/lang/Math", "min", "(JJ)J"),
                ScalarOp::MaxL => ("java/lang/Math", "max", "(JJ)J"),
                ScalarOp::AbsI => ("java/lang/Math", "abs", "(I)I"),
                ScalarOp::AbsL => ("java/lang/Math", "abs", "(J)J"),
                ScalarOp::CompareI => ("java/lang/Integer", "compare", "(II)I"),
                ScalarOp::CompareL => ("java/lang/Long", "compare", "(JJ)I"),
                ScalarOp::NlzI => ("java/lang/Integer", "numberOfLeadingZeros", "(I)I"),
                ScalarOp::NlzL => ("java/lang/Long", "numberOfLeadingZeros", "(J)I"),
                ScalarOp::NtzI => ("java/lang/Integer", "numberOfTrailingZeros", "(I)I"),
                ScalarOp::NtzL => ("java/lang/Long", "numberOfTrailingZeros", "(J)I"),
                ScalarOp::ReverseBytesI => ("java/lang/Integer", "reverseBytes", "(I)I"),
                ScalarOp::ReverseBytesL => ("java/lang/Long", "reverseBytes", "(J)J"),
                ScalarOp::LowestOneBitI => ("java/lang/Integer", "lowestOneBit", "(I)I"),
                ScalarOp::LowestOneBitL => ("java/lang/Long", "lowestOneBit", "(J)J"),
                ScalarOp::HighestOneBitI => ("java/lang/Integer", "highestOneBit", "(I)I"),
                ScalarOp::HighestOneBitL => ("java/lang/Long", "highestOneBit", "(J)J"),
                ScalarOp::RotateLeftI => ("java/lang/Integer", "rotateLeft", "(II)I"),
                ScalarOp::RotateLeftL => ("java/lang/Long", "rotateLeft", "(JI)J"),
                ScalarOp::RotateRightI => ("java/lang/Integer", "rotateRight", "(II)I"),
                ScalarOp::RotateRightL => ("java/lang/Long", "rotateRight", "(JI)J"),
                ScalarOp::AbsF => ("java/lang/Math", "abs", "(F)F"),
                ScalarOp::AbsD => ("java/lang/Math", "abs", "(D)D"),
            }
        }
        const ALL: &[ScalarOp] = &[
            ScalarOp::MinI,
            ScalarOp::MaxI,
            ScalarOp::MinL,
            ScalarOp::MaxL,
            ScalarOp::AbsI,
            ScalarOp::AbsL,
            ScalarOp::CompareI,
            ScalarOp::CompareL,
            ScalarOp::NlzI,
            ScalarOp::NlzL,
            ScalarOp::NtzI,
            ScalarOp::NtzL,
            ScalarOp::ReverseBytesI,
            ScalarOp::ReverseBytesL,
            ScalarOp::LowestOneBitI,
            ScalarOp::LowestOneBitL,
            ScalarOp::HighestOneBitI,
            ScalarOp::HighestOneBitL,
            ScalarOp::RotateLeftI,
            ScalarOp::RotateLeftL,
            ScalarOp::RotateRightI,
            ScalarOp::RotateRightL,
            ScalarOp::AbsF,
            ScalarOp::AbsD,
        ];
        for &sop in ALL {
            let (c, m, d) = declared_signature(sop);
            assert_eq!(
                try_ir_scalar_intrinsic(c, m, d),
                Some(sop),
                "{c}.{m}{d} is declared as a scalar-intrinsic family and was not recognised",
            );
            // And the shapes the lowering relies on must agree with the
            // descriptor: a mistyped input is joined against a real stack entry
            // at the next merge, which is a wrong-code bug rather than a missed
            // optimization.
            assert!(sop.arity() >= 1 && sop.arity() <= 2, "{c}.{m}{d} arity");
            assert_eq!(
                sop.is_fp(),
                matches!(sop, ScalarOp::AbsF | ScalarOp::AbsD),
                "{c}.{m}{d}: is_fp must name exactly the XMM families, because                  the lowering arm loads inputs[0] into RAX unless it says so",
            );
        }
    }
}

/// May the optimizing tier lower the scalar intrinsic families as arithmetic?
/// **Default ON**; `CRATONVM_JIT_IR_SCALAR_INTRINSICS=0` restores the
/// method-level refusal for these families too, which is the A arm of the only
/// A/B that means anything here.
/// May a spliced callee body contain its own branches? **Default ON**;
/// `CRATONVM_JIT_IR_SPLICE_BRANCH=0` restores the straight-line-only shape.
///
/// Read by BOTH halves of the feature — the splice scanner in `jit_bridge`,
/// which decides whether to admit such a callee, and [`IrBuilder::build`],
/// which pre-scans the admitted body's control flow. They must agree: a body
/// admitted without its pre-scan is exactly the orphan-node failure STUB-S8
/// was, so the scanner asks the VM-side mirror of this and the builder asks
/// this, and neither may be flipped alone.
pub fn ir_splice_branch_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_SPLICE_BRANCH").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// May a spliced callee body have more than one `return`? **Default OFF**;
/// `CRATONVM_JIT_IR_SPLICE_MULTI_RETURN=1` lifts the
/// `ir-splice-not-single-trailing-return` refusal.
///
/// # Why it is off, given that it works
///
/// It works, and it is soak-clean: 39 deterministic workloads under three
/// collectors with 0 divergence, checksum parity against Temurin JDK 25 on 14
/// workloads, and a probe built for it (`MultiRet`) where the census reads
/// `bodies=3 return_edges=8` on and `bodies=0` off.
///
/// What it does not do is go faster. Interleaved, order-flipped, 14 rounds on
/// that probe: **+0.0 % median, faster in 5 of 14 paired rounds.** Splicing
/// removes a call and adds a merge and a phi, and on this shape those cancel.
///
/// It used to cost something on the other side as well: a merge built inside a
/// spliced body set `SPLICED_MERGE_SEEN`, which withheld the artifact's
/// `ir_osr_entries` wholesale. That containment came off on 2026-09-09 — the
/// wrong answer was `emit_osr_entry_stubs` jumping past the block that writes a
/// constant arm's home word, not the splice — so the OSR door is no longer
/// forfeited by turning this on.
///
/// What is left is the measurement, and the measurement is neutral. It stays off
/// until a shape is found where splicing a multi-return body pays; the argument
/// that would have flipped it (coverage against nothing) is gone with the
/// containment, because there is no longer anything to trade coverage against.
///
/// Implies [`ir_splice_branch_enabled`] in practice — a second `return` is
/// only reachable through a branch — and, like it, is read by BOTH halves:
/// the splice scanner in `jit_bridge`, which decides whether to admit such a
/// callee, and [`IrBuilder::splice_return`] / [`IrBuilder::finish_multi_return_splice`],
/// which build the continuation. Admitting a body one half does not understand
/// is the orphan-node failure STUB-S8 was, so neither may be flipped alone.
///
/// # Implied by `CRATONVM_JIT_IR_RECURSIVE_INLINE=1` (round 9 wave 3)
///
/// The shape "a multi-return splice that pays" this section was waiting for is
/// self-recursion: every recursive method has a base case, so almost every one
/// has two `return`s (`if (n <= 1) return n; return f(n-1) + f(n-2);`), and a
/// recursive splice removes a whole frame per copy, not one call's worth of
/// overhead against a merge. [`ir_recursive_inline_enabled`] would be inert on
/// exactly its target shape without this, so turning it on turns this on too.
/// Both halves still read this one function, so they still cannot disagree.
pub fn ir_splice_multi_return_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        ir_splice_branch_enabled()
            && (matches!(
                cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_SPLICE_MULTI_RETURN")
                    .as_deref(),
                Ok("1") | Ok("true") | Ok("on") | Ok("yes")
            ) || ir_recursive_inline_enabled())
    })
}

/// Bounded self-recursive inlining in the optimizing tier. **Default OFF**;
/// `CRATONVM_JIT_IR_RECURSIVE_INLINE=1` turns it on (and implies
/// [`ir_splice_multi_return_enabled`]).
///
/// # What it changes
///
/// Without it a self-recursive static method (`fib`) is never inlined into
/// itself by this tier, and not by a decision anyone made:
///
/// * the splice scanner refuses the body outright
///   (`ir-splice-not-single-trailing-return`: the base case is a second
///   `return`);
/// * with multi-return splicing on, the resolver nests the method into itself
///   [`crate::MAX_IR_INLINE_NEST_DEPTH`] deep — 2 + 4 + … + 64 copies — and
///   the plan is then refused WHOLE: the leaves' self-calls have no
///   `direct_entry` on the first compile (the callee compiler declines the
///   cycle), so `append_ir_inline_site`'s unbindable-call rule refuses the
///   site, and on the C2 recompile the tree overruns the 1 300-byte splice
///   budget by one body (63 x 21 bytes for `fib`). Even had it fitted, its
///   leaves would have been bound to the PREVIOUS artifact, so the recursion
///   would have run in the old body.
///
/// With it, `lib.rs` splices at most [`IR_RECURSIVE_INLINE_MAX_COPIES`] copies
/// of the compiling method along any inline chain and turns the self-call at
/// the cut into this method's own DIRECT self-call (`invoke_kind == 4`, the
/// row [`IrBuilder::self_recursive_call_row`] finds), so the leaves recurse
/// into the NEW body. `fib` becomes one frame per seven nodes of its call tree
/// with eight direct leaf calls: the shape of HotSpot's recursive inlining,
/// one level deeper (see [`IR_RECURSIVE_INLINE_MAX_COPIES`] for why).
///
/// # Why that is sound
///
/// Nothing new is lowered. A spliced self copy is an ordinary IR splice (the
/// same scanner admitted it, the same builder walks it), and a leaf is an
/// ordinary `Op::Call` whose `info_ptr` is the caller's own kind-4
/// `JitInvokeInfo` — the route `ir_lower::emit_self_recursive_call` already
/// takes for the method's own recursive call sites, which records its own
/// inline-frame row and exception/stack checks and is position-independent.
/// The leaf's deopt/exception bci is the enclosing top-level invoke's, exactly
/// as for any call left behind in a spliced body today.
///
/// Read per call rather than cached, like [`crate::ir_inline_enabled`]: a
/// compile is not a hot path, and a test can then arrange it per thread.
pub fn ir_recursive_inline_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_RECURSIVE_INLINE")
}

/// How many copies of the compiling method a bounded self-recursive splice
/// ([`ir_recursive_inline_enabled`]) may stack along one inline chain, the
/// compiling method's own body not counted. HotSpot's `MaxRecursiveInlineLevel`
/// is 1.
///
/// Two, on a measurement rather than by analogy: `fib(40)` hand-inlined at the
/// source level on the wave-2 binary (`irmid3/src/FibRecInl.java`, interleaved,
/// median of three) runs 1 220 ms plain, ~910 ms with one level (1.34x) and
/// ~460 ms with two (2.6x); HotSpot runs the plain form in ~330 ms. Each level
/// multiplies the spliced bytecode by the method's self-call fan-out (two for
/// `fib`: 2 + 4 = 6 bodies, 126 bytes), and the per-compile splice budget still
/// bounds a wider fan-out — a site that does not fit is rolled back whole and
/// stays a direct self-call.
pub const IR_RECURSIVE_INLINE_MAX_COPIES: usize = 2;

/// The bounded self-recursive splice plan for one inline chain. See
/// [`ir_recursive_inline_enabled`].
///
/// Built once per compile by `lib.rs` from the compiling method's identity and
/// [`IrBuilder::self_recursive_call_row`], and then threaded down the chain:
/// every nested site is [`classified`](Self::classify) against it, and a self
/// site either takes one more copy ([`IrSelfSplice::Splice`], carrying the
/// deeper plan) or becomes a leaf call ([`IrSelfSplice::Leaf`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IrSelfRecursion {
    class_name: String,
    method_name: String,
    descriptor: String,
    class_id: u32,
    leaf_row: (usize, usize, u8),
    copies: usize,
    max_copies: usize,
}

/// What a site is to an [`IrSelfRecursion`] plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IrSelfSplice {
    /// Not the compiling method: splice (or not) exactly as without the plan,
    /// passing the SAME plan down to its nested sites.
    NotSelf,
    /// The compiling method, with a copy left: splice it and pass this deeper
    /// plan (one more copy counted) to its nested sites.
    Splice(IrSelfRecursion),
    /// The compiling method, with no copy left: do NOT splice. Install this
    /// `invoke_info` row (`(info_ptr, num_args, ret)`, the caller's own
    /// kind-4 row) at the site's combined-buffer pc, so it lowers as this
    /// method's direct self-call.
    Leaf((usize, usize, u8)),
}

impl IrSelfRecursion {
    /// The plan for the compiling method `class_name.method_name descriptor`
    /// declared in class `class_id`, with `leaf_row` its own kind-4
    /// `invoke_info` row, allowing [`IR_RECURSIVE_INLINE_MAX_COPIES`] copies.
    pub fn new(
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        class_id: u32,
        leaf_row: (usize, usize, u8),
    ) -> IrSelfRecursion {
        IrSelfRecursion {
            class_name: class_name.to_owned(),
            method_name: method_name.to_owned(),
            descriptor: descriptor.to_owned(),
            class_id,
            leaf_row,
            copies: 0,
            max_copies: IR_RECURSIVE_INLINE_MAX_COPIES,
        }
    }

    /// The same plan with a different copy budget. For tests and measurement.
    pub fn with_max_copies(mut self, max_copies: usize) -> IrSelfRecursion {
        self.max_copies = max_copies;
        self
    }

    /// Copies of the compiling method already spliced on this chain.
    pub fn copies(&self) -> usize {
        self.copies
    }

    /// Classify a site whose resolved callee is `class_name.method_name
    /// descriptor` in class `class_id`, static or not.
    ///
    /// A site is "self" only when it is STATIC (the kind-4 route is static-only)
    /// and names the same class id — non-zero, so an unknown id never matches —
    /// and the same name and descriptor. Anything else is [`IrSelfSplice::NotSelf`],
    /// which leaves today's behaviour untouched.
    pub fn classify(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        class_id: u32,
        is_static: bool,
    ) -> IrSelfSplice {
        let is_self = is_static
            && class_id != 0
            && class_id == self.class_id
            && class_name == self.class_name
            && method_name == self.method_name
            && descriptor == self.descriptor;
        if !is_self {
            return IrSelfSplice::NotSelf;
        }
        if self.copies >= self.max_copies {
            return IrSelfSplice::Leaf(self.leaf_row);
        }
        let mut deeper = self.clone();
        deeper.copies += 1;
        IrSelfSplice::Splice(deeper)
    }
}

/// Spliced callee bodies whose several `return`s were funnelled into one
/// continuation merge, and how many return edges that took in total. Both are
/// process-wide and diagnostic only; `interp_census` prints them so "no
/// multi-return body was ever admitted" is distinguishable from "the feature
/// is not wired".
static MULTI_RETURN_SPLICES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static MULTI_RETURN_EDGES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `(bodies, return edges)` — see [`MULTI_RETURN_SPLICES`].
pub fn multi_return_splice_census() -> (u64, u64) {
    (
        MULTI_RETURN_SPLICES.load(std::sync::atomic::Ordering::Relaxed),
        MULTI_RETURN_EDGES.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// May a spliced callee body contain a `getstatic`? **Default ON**;
/// `CRATONVM_JIT_IR_SPLICE_GETSTATIC=0` restores the `ir-splice-static-field`
/// refusal.
///
/// Read by BOTH halves, for the same reason and with the same hazard as
/// [`ir_splice_branch_enabled`]: the splice scanner in `jit_bridge` decides
/// whether to admit such a callee, and [`IrInlineTables::static_field_info`] is
/// what carries the rows the builder's `0xb2` arm then looks up. A body
/// admitted without its rows does not fall back -- the builder bails the whole
/// METHOD at the first spliced `getstatic`, which is how the `ldc` rows
/// behaved for their first hour. Neither half may be flipped alone.
///
/// `putstatic` is NOT covered by this switch in either direction. It stays
/// refused unconditionally; see [`IrInlineTables::static_field_info`].
pub fn ir_splice_getstatic_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_SPLICE_GETSTATIC").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// May a spliced callee body contain a `checkcast` or an `instanceof`?
/// **Default ON**; `CRATONVM_JIT_IR_SPLICE_TYPECHECK=0` restores the refusal.
///
/// Read by BOTH halves, with the same hazard as [`ir_splice_getstatic_enabled`]
/// and one extra step: the splice scanner in `jit_bridge` decides whether to
/// admit such a callee AND resolves the target class against the CALLEE's
/// constant pool, since `InlineSite` carried no typecheck rows before this and
/// only that pool can name the target. [`IrInlineTables::checkcast_info`]
/// carries what comes out. A body admitted without its rows bails the whole
/// METHOD, so neither half may be flipped alone.
///
/// Every typed read out of an untyped container is a `checkcast`, which is why
/// the survey that motivated the caller-side arms counted 306 events on this
/// pair -- "the largest single whole-method refusal, more than every opcode gap
/// combined". The splice scanner had been refusing the same shape for the same
/// non-reason.
pub fn ir_splice_typecheck_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_SPLICE_TYPECHECK").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

pub fn ir_scalar_intrinsics_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_SCALAR_INTRINSICS").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// Scalar-intrinsic sites lowered as arithmetic, this process.
static SCALAR_INTRINSICS_LOWERED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Conditional branches replaced by a guard plus an unconditional jump, this
/// process. See [`IrBuilder::prune_always_taken_branch`].
static BRANCHES_PRUNED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn note_branch_pruned() {
    BRANCHES_PRUNED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// How many cold branch arms this tier speculated away. A zero with the feature
/// ON means the profile never showed a branch to be one-sided over the sample —
/// which is a fact about the workload, not about the pass, and is exactly the
/// distinction a bare "it did nothing" cannot make.
pub fn branch_prune_census() -> u64 {
    BRANCHES_PRUNED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Speculative pruning of a branch arm the profile has never seen taken —
/// **default OFF**, opt in with `CRATONVM_JIT_IR_SPECULATE=1`.
///
/// Needs a branch profile to do anything, so it is only meaningful together
/// with `CRATONVM_TIER_PGO` / `CRATONVM_TIER_PGO_ALWAYS` or inside the C2
/// nomination window.
pub fn ir_speculate_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_SPECULATE").as_deref(),
            Ok("1") | Ok("true") | Ok("on") | Ok("yes")
        )
    })
}

/// Minimum observations of a branch before its unseen edge may be speculated
/// away.
///
/// A branch executed three times, all one way, says nothing. HotSpot's own
/// uncommon-trap policy wants a comparable sample before it prunes, and the
/// cost of being wrong here — a deopt, then a re-speculation on the next
/// compile — is paid per mistake, so the floor is what keeps a cold method from
/// paying it repeatedly.
pub const MIN_OBSERVATIONS_TO_PRUNE: u32 = 2_000;

/// Call sites REFUSED because their intrinsic family is loop-shaped and a
/// generic dispatch would be a downgrade.
///
/// Counted beside the lowered ones so the split is readable rather than
/// asserted: a large number here is the work list for whoever extends
/// [`try_ir_scalar_intrinsic`], and it is the only thing that says how much of
/// the original 68-method refusal is still on the table.
static SCALAR_INTRINSICS_REFUSED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub fn note_scalar_intrinsic_lowered() {
    SCALAR_INTRINSICS_LOWERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
}

pub fn note_scalar_intrinsic_refused() {
    SCALAR_INTRINSICS_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// `(lowered, refused)` call-site intrinsic decisions in the optimizing tier.
pub fn scalar_intrinsic_census() -> (u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        SCALAR_INTRINSICS_LOWERED.load(Relaxed),
        SCALAR_INTRINSICS_REFUSED.load(Relaxed),
    )
}

/// Why a site was replaced by an uncommon trap instead of refusing the method.
///
/// One variant per CALLER, not per opcode, because the census exists to test
/// each caller's coldness argument separately -- `checkcast` and `instanceof`
/// share a cause because they share an argument (the named class is unloaded),
/// while `invokedynamic` is its own because its argument is different and
/// weaker. See [`IrBuilder::plant_uncommon_trap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrapCause {
    /// `invokedynamic`. No coldness proof; matches the single-pass backend.
    Indy,
    /// `checkcast` / `instanceof` naming a class that is not loaded.
    UnresolvedTypeCheck,
    /// `new` of a class that is not loaded (`JitNewSite::Deferred`).
    UnresolvedNew,
}

impl TrapCause {
    pub fn as_str(self) -> &'static str {
        match self {
            TrapCause::Indy => "invokedynamic",
            TrapCause::UnresolvedTypeCheck => "unresolved-typecheck",
            TrapCause::UnresolvedNew => "unresolved-new",
        }
    }
    const COUNT: usize = 3;
    fn index(self) -> usize {
        match self {
            TrapCause::Indy => 0,
            TrapCause::UnresolvedTypeCheck => 1,
            TrapCause::UnresolvedNew => 2,
        }
    }
}

/// Traps planted, by cause. Process-wide; read by `ir_trap_census`.
static TRAPS_PLANTED: [std::sync::atomic::AtomicU64; TrapCause::COUNT] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Traps REFUSED because the trap would not have been resumable, by cause.
///
/// A planted count on its own cannot tell "this workload has no such site"
/// from "every such site was declined": both read as a low number of hard
/// failures, and only one of them means the guard is doing anything. This row
/// is what makes the refusal falsifiable — and it is the price tag of
/// `ir_trap_replay_guard_enabled`, since every refusal here is one optimizing
/// body given back to the single-pass tier.
static TRAPS_REFUSED_UNRESUMABLE: [std::sync::atomic::AtomicU64; TrapCause::COUNT] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

fn note_trap_planted(cause: TrapCause) {
    TRAPS_PLANTED[cause.index()].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

fn note_trap_refused(cause: TrapCause) {
    TRAPS_REFUSED_UNRESUMABLE[cause.index()].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// `(cause, refused)` for every trap cause — the companion row to
/// [`ir_trap_census`]. See [`TRAPS_REFUSED_UNRESUMABLE`].
pub fn ir_trap_refusal_census() -> [(&'static str, u64); TrapCause::COUNT] {
    use std::sync::atomic::Ordering::Relaxed;
    [
        (
            TrapCause::Indy.as_str(),
            TRAPS_REFUSED_UNRESUMABLE[0].load(Relaxed),
        ),
        (
            TrapCause::UnresolvedTypeCheck.as_str(),
            TRAPS_REFUSED_UNRESUMABLE[1].load(Relaxed),
        ),
        (
            TrapCause::UnresolvedNew.as_str(),
            TRAPS_REFUSED_UNRESUMABLE[2].load(Relaxed),
        ),
    ]
}

/// Must an uncommon trap be resumable before it may be planted? **Default
/// ON**; `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0` is the kill switch and restores
/// the pre-2026-09-07 behaviour (plant regardless, and let the interpreter's
/// deopt sink raise `InternalError` when it cannot replay).
///
/// The guard exists because this backend publishes no resumable deopt on a
/// production artifact — see [`IrBuilder::plant_uncommon_trap`] for the full
/// argument and the javac shape that proved it. The kill switch is here so the
/// cost of the refusal (optimizing bodies handed back to the single-pass tier)
/// can be measured as an A/B in one binary rather than argued about.
pub fn ir_trap_replay_guard_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_TRAP_REPLAY_GUARD").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// `(cause, planted)` for every trap cause, always all three rows.
///
/// All three even when zero, because a zero on one row cannot be told from
/// "this workload has no such site" unless the other rows are beside it -- the
/// distinction this file has been burned by often enough to make a rule of.
pub fn ir_trap_census() -> [(&'static str, u64); TrapCause::COUNT] {
    use std::sync::atomic::Ordering::Relaxed;
    [
        (TrapCause::Indy.as_str(), TRAPS_PLANTED[0].load(Relaxed)),
        (
            TrapCause::UnresolvedTypeCheck.as_str(),
            TRAPS_PLANTED[1].load(Relaxed),
        ),
        (
            TrapCause::UnresolvedNew.as_str(),
            TRAPS_PLANTED[2].load(Relaxed),
        ),
    ]
}

/// May a site this tier cannot lower become an uncommon trap, instead of
/// refusing the whole method? **Default ON**; `CRATONVM_JIT_IR_SITE_TRAP=0` is
/// the kill switch and restores the method-level refusal at every caller.
///
/// Measured on the H2 JDBC workload before this existed: 53 methods refused for
/// an `invokedynamic`, 34 for a `checkcast`/`instanceof` on an unloaded class,
/// and 13 for a `new` of one -- 100 methods, none of which had anything wrong
/// with the code the optimizing tier would actually have run.
///
/// THIS WAS BRIEFLY FLIPPED OFF ON 2026-09-07 AND THE FLIP WAS WRONG. Thirty
/// hibernate-reactive classes then read `ok=182 failed=7` with traps on against
/// `ok=241 failed=0` with them off, and 36 `InternalError: ... refusing
/// side-effecting replay` -- so the trap looked like the defect. It was only the
/// TRIGGER. The cause was a deopt SINK that aborted on a trapped frame its
/// sibling sink resumed, fixed the same day by another session
/// (`CRATONVM_JIT_DEOPT_SINK_RESUME`, default ON). Re-measured on a binary
/// carrying that fix:
///
/// ```text
///   site traps ON  + sink fix    ok=239  failed=0  InternalError=0
///   site traps OFF + sink fix    ok=241  failed=0  InternalError=0
/// ```
///
/// Zero errors either way, so there is nothing here to switch off. The lesson
/// kept is about the measurement, not the flag: an A/B with ONE lever proves
/// that lever is on the path to the failure, and says nothing about whether it
/// is the defect. A trigger and a cause both answer to a kill switch.
pub fn ir_site_trap_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_SITE_TRAP").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// May an UNRESOLVED-CLASS site (`checkcast`, `instanceof`, `new`) become a
/// trap? **Default OFF**; `CRATONVM_JIT_IR_UNRESOLVED_CLASS_TRAP=1` opts in.
///
/// The reason it is off CHANGED on 2026-09-07 and the old reason is retracted.
/// It read "200,000 deopts on `UnresolvedTrapProbe`, every call, permanently
/// not-compilable", and every one of those numbers was a broken deopt sink
/// measured through this switch (see `ir_site_trap_enabled`). On a binary with
/// `CRATONVM_JIT_DEOPT_SINK_RESUME` in it, 30 hibernate-reactive classes with
/// this switch ON read `ok=241 failed=0` with **112 traps taken and zero**
/// `refusing side-effecting replay` -- identical to the default arm.
///
/// It stays off because turning it on was always a THROUGHPUT argument (+13
/// accepted bodies, +11 lowered on H2) and that has never been measured. Not
/// because it is harmful.
///
/// # Why this is separate from the indy trap, and why it ships off
///
/// The coldness argument for these three was: "the named class has never been
/// loaded, and a class that has never been loaded cannot have been touched by
/// any path that has executed." The first clause is true and the conclusion
/// does not follow from it, because the observation is made at COMPILE time.
/// A class not loaded when the method compiles can load a moment later, and the
/// path can then run — at which point the trap fires on a LIVE path, the body
/// returns the deopt sentinel, and it does so on every execution, forever.
///
/// That is not a theory. `ir_vs_singlepass_checkcast_not_yet_loaded_refuses_ir`
/// executes exactly that path: with the trap planted, the compiled body
/// returned `i64::MIN` instead of the object. The fixture was written years
/// before this trap existed and it refuted the argument on the first run.
///
/// The transient case already has a mechanism, and it is the opposite of this
/// one: `note_deferred_new_bail` / `take_deferred_new_retry` REFUSE the method
/// and re-offer it once the class loads. Compiling around the site instead
/// bypasses that machinery — the builder no longer bails, so nothing arms the
/// memo and nothing re-offers the method — which trades a delayed optimizing
/// body for a permanently deopting one.
///
/// `invokedynamic` is different and stays on: there is no "later" for a
/// bootstrap this tier will never lower, and the single-pass backend has made
/// exactly that trade by default since it stopped bailing on indy.
///
/// What would make this safe is `DeoptAction::RecompileAndReinterpret` at these
/// sites instead of `Reinterpret`, so the first trap triggers the recompile
/// that resolves the class. `Op::Guard`'s lowering hard-codes the action today;
/// parameterising it is the work this switch is waiting on.
pub fn ir_unresolved_class_trap_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_IR_UNRESOLVED_CLASS_TRAP").as_deref(),
            Ok("1") | Ok("true") | Ok("on") | Ok("yes")
        )
    })
}

/// Report the exact [`IrBuilder::build`] site that refused a method.
///
/// The builder answers `Option<Graph>`, so every refusal is indistinguishable
/// from "the optimizing tier is switched off" — both present as zero IR bodies,
/// and this investigation drew the second conclusion from the first twice
/// (`jit-ir-relocation-map-contract.md`). Fifteen distinct
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
    // `bailout.rs`'s module doc names this as one of the three incompatible
    // ways the compiler says "I cannot compile this", and the one that "carries
    // no reason at all, so the per-method compiler report cannot be produced".
    // It could not: over an H2 test class, 51 of 620 compilations fell through
    // to single-pass and only 6 carried an attributed reason — the other 45
    // refused here, invisibly.
    //
    // The site/pc pair stays in the debug line above; what the report needs is
    // the method and the category, which is what this adds.
    // The SITE, not just the fact. On a real workload (H2 `TestAlter`) 38 of 50
    // fall-throughs land here, and with one shared category they were 38
    // identical rows — "the builder refused", 38 times, naming nothing to fix.
    // `ir.rs:<line>` is what separates them into the distinct refusals they
    // actually are, and the line is already the argument this function takes.
    attribute_build_bail_at(
        crate::bailout::BailoutReason::UnsupportedShape("IrBuilder::build"),
        site,
        pc,
    );
    None
}

/// Attach a builder refusal to the compilation in flight.
///
/// Split out so both bail helpers share one policy: the process-wide category
/// counter AND the per-compilation report, the same pair `verify_or_bail` uses.
fn attribute_build_bail(reason: crate::bailout::BailoutReason) {
    let bailout = crate::bailout::Bailout::new(reason);
    crate::bailout::record_bailout(&bailout);
    crate::metrics::note_current_bailout(&bailout, "build");
}

/// [`attribute_build_bail`] carrying the refusing site.
///
/// The category stays one value so the process-wide counters keep their stable
/// row set; the site rides in the bailout's context, which is what the
/// per-compilation report prints. That is the difference between "the builder
/// refused 38 times" and a ranked list of which refusals to go and fix.
fn attribute_build_bail_at(reason: crate::bailout::BailoutReason, site: u32, pc: usize) {
    let bailout =
        crate::bailout::Bailout::with_context(reason, format!("ir.rs:{site} (bytecode pc {pc})"));
    crate::bailout::record_bailout(&bailout);
    crate::metrics::note_current_bailout(&bailout, "build");
}

/// [`ir_build_bail`] for the opcode catch-all, which knows something more
/// useful than its own line number: *which* bytecode has no IR lowering.
#[cold]
#[inline(never)]
pub fn ir_build_bail_opcode<T>(op: u8, pc: usize) -> Option<T> {
    if ir_bail_reporting() {
        eprintln!("[ir] IrBuilder::build has no lowering for opcode {op:#04x} at bytecode pc {pc}");
    }
    // The one refusal that already knows exactly what it lacks, so it reports
    // the opcode rather than the generic shape.
    attribute_build_bail(crate::bailout::BailoutReason::UnsupportedOpcode { opcode: op });
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
    cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC")
        || cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_IR_COMPILES")
}

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
/// `invokedynamic` (0xba) is admitted only when the builder may replace it
/// with an uncommon trap (`ir_site_trap_enabled`, see the `indy_ops` check
/// below and the builder's `0xba` arm). There is still no exception-edge IR:
/// the builder SKIPS handler bodies (STUB-S8, `normally_reachable_pcs`)
/// because a compiled frame hands every exception back to the interpreter,
/// and which exception tables are admissible at all is decided by
/// `precise_exception_frames` below. (This paragraph used to say an indy
/// method always falls back and a try/catch method is rejected at parse
/// time; both stopped being true when those arms landed.)
///
/// TODO(IR widening): increase caps once differential tests validate.
pub fn ir_compatible(scan: &super::x64::JitScanResult) -> bool {
    // Caps below are deliberately conservative. A method that exceeds a cap is
    // still compilable via the x64 single-pass backend; we just decline to
    // route it through the IR pipeline until we've gained more confidence.

    // cov-07: `athrow` (0xbf) is NOT refused here any more. `IrBuilder::build`
    // now lowers it (`Op::Throw`), reusing the exact `jit_throw_exception`
    // sentinel-drain protocol `Op::CheckCast`'s helper-thrown
    // `ClassCastException` already uses (cov-05) — see `Op::Throw`'s doc
    // comment. `scan.has_athrow` itself is unchanged and still recorded;
    // nothing in this function reads it any more, but the scan field stays
    // for the debug trace and for any future caller that needs "does this
    // method contain an athrow" without re-decoding.
    //
    // What is NOT relaxed by this: a method whose handler reads a
    // non-parameter local still needs a precise resume frame, and the IR
    // tier has no way to publish one (that machinery is x64-only, gated
    // behind `precise_exception_frame_sites_supported`). That case is
    // excluded by `precise_exception_frames` (RBC.6, checked by this
    // function's caller) independently of which opcode throws — it gates on
    // ANY non-empty `exception_table`, not on `has_athrow` — so an
    // athrow-bearing method that needs precise locals was already refused
    // there before it ever reached this conjunct. See
    // `cov-07-athrow-RETIRED-*.md`.

    // invokedynamic-uncommon-trap fix: the IR builder has no lowering for
    // 0xba (it bails cleanly via the main loop's `_ => return None` if one
    // slips through), but decline it explicitly here rather than relying
    // solely on that later bail — this keeps the scope boundary self-
    // documenting and avoids running the (now 0xba-aware, but still only
    // IR-builder-adjacent) `bytecode_analysis::branch_target_map`/`bytecode_analysis::back_edges`
    // pre-scans on a method the IR path was never going to lower anyway.
    // Methods containing invokedynamic always fall back to the x64
    // single-pass backend, which lowers it directly.
    //
    // 2026-09-06: no longer unconditional. `IrBuilder`'s `0xba` arm now
    // replaces such a site with an uncommon trap -- the SAME trade the
    // single-pass backend has made by default since it stopped bailing on indy
    // -- so the method is admitted and compiles around the site. Measured
    // before this: **53 methods refused on the H2 JDBC workload**, none of them
    // for anything wrong with the code the optimizing tier would have run.
    //
    // The refusal survives with the kill switch, and it also survives ANYWHERE
    // the descriptor resolver cannot supply the site's stack effect: the `0xba`
    // arm bails the method on a pc it has no shape for, because guessing how
    // many operand-stack entries an indy consumes corrupts every bytecode after
    // it. The gate is the *admission*; the shape is the *proof*.
    if !scan.indy_ops.is_empty() && !ir_site_trap_enabled() {
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
    // `putstatic` has no IR lowering, and deliberately: a static reference
    // write owes the SATB pre-barrier the single-pass `jit_putstatic_*` path
    // carries. The builder refused it at its catch-all AFTER the partial graph
    // build (and `c2_upgrade_would_engage` still answered yes); refuse it
    // here instead.
    if scan.has_putstatic {
        return ir_reject("scan.has_putstatic");
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
    // cov-06: `anewarray` sites. Same budget shape and same reasoning as the
    // `new` cap above — it bounds how many reference-array allocations are
    // admitted, each one lowered through the shared `emit_new_array_stub`.
    // A REFERENCE array is still never scalar-replaced (`escape_analysis.rs`
    // refuses `anewarray` outright: a never-stored slot would have to be
    // answered with `null` and the applier's zero default is numeric), so this
    // cap bounds stub-call cost and nothing else. A small constant-length
    // PRIMITIVE array is a different story — see `MAX_SCALAR_ARRAY_LEN`.
    // `newarray` (0xbc) has no
    // analogous cap: it carries no constant-pool site to resolve, so there is
    // nothing here to bound admission on — an unbounded number of primitive
    // array allocations costs the builder nothing `IR_MAX_GRAPH_NODES`
    // doesn't already bound.
    if scan.anewarray_ops.len() > IR_MAX_ARRAY_ALLOCATIONS {
        return ir_reject("scan.anewarray_ops.len() > IR_MAX_ARRAY_ALLOCATIONS");
    }

    // ── Surviving exclusions — real missing lowerings, not budgets ───
    //
    //  * ONE-DIMENSIONAL array allocation is NOT excluded, and this bullet
    //    used to say the opposite. Until 2026-09-16 it read "`newarray`
    //    (0xbc) and `anewarray` (0xbd) have no arm in `IrBuilder::build`'s
    //    opcode match and `Op::NewArray` is constructed nowhere", and it went
    //    on to claim that the `anewarray_ops` BUDGET had been replaced by a
    //    refusal. All three halves were false: cov-06 added the builder's
    //    0xbc/0xbd arms (`IrBuilder::build`), `Op::NewArray` is constructed
    //    by both of them and lowered by `ir_lower`, and the budget is still
    //    the `scan.anewarray_ops.len() > IR_MAX_ARRAY_ALLOCATIONS` check
    //    twenty lines above this comment. The `multianewarray` bullet
    //    directly below contradicted it in the same block.
    //
    //    The lesson the false text was trying to teach is still the right
    //    one, so it is restated here as a rule rather than as a claim about
    //    two opcodes: **a method the builder will refuse must be refused
    //    HERE**, cheaply and with a reason, not after a full graph build.
    //    Admission and the builder disagreeing is what made the optimizing
    //    tier produce zero bodies on every probe written for the
    //    relocation-map contract while every gate upstream reported "open"
    //    (jit-ir-relocation-map-contract.md). `ir_admission_and_builder_agree`
    //    (below) now pins the surviving direction of that rule as a test, so
    //    the next edit to either side fails CI instead of rotting into a
    //    comment that says the reverse of the code beside it.
    //
    //  * `multianewarray` (0xc5) — no arm in `IrBuilder::build`'s opcode
    //    match and `Op::NewArray` cannot represent a multi-dimensional
    //    allocation. Unlike `newarray`/`anewarray` (cov-06's first two
    //    increments — see `Op::NewArray`, the builder's 0xbc/0xbd arms and
    //    `ir_lower`'s lowering arm), this one is a genuinely different
    //    lowering shape: the single-pass backend's 0xc5 arm is a helper call
    //    sequence (`jit_multianewarray_2d`) the IR pipeline has no equivalent
    //    op for, and the survey found only 2 events against `anewarray`'s
    //    138 — not worth a new `Op` variant and a second resolver-fed info
    //    map for. Left refused deliberately; a method containing one still
    //    compiles fine on the single-pass backend.
    //  * `checkcast` / `instanceof` / `athrow` — NONE of the three is refused
    //    here (as of cov-05 for the first two, cov-07 for the third).
    //    `instanceof` produces an `int`, cannot throw, and has no
    //    control-flow consequence. `checkcast` (0xc0) and `athrow` (0xbf) CAN
    //    throw, but both reach the SAME generic "helper stashed a pending
    //    exception, drain it through the shared epilogue" protocol every
    //    other fallible `Opaque` call in this tier already has
    //    (`Op::ConstClass`'s resolution failure is the existing precedent) —
    //    neither is a jump into a LOCAL exception-table handler inside this
    //    compiled frame, which is what `precise_exception_frames` (checked by
    //    this function's caller) guards against, independently of which
    //    opcode throws. See
    //    `cov-05-checkcast-and-instanceof-RETIRED-*.md` and
    //    `cov-07-athrow-RETIRED-*.md`.
    //
    //    `IrBuilder::build`'s 0xc0/0xc1 arms additionally require the target
    //    class to be resolved-and-loaded at compile time (via
    //    `checkcast_info`/`instanceof_info`, populated in `lib.rs`'s
    //    `try_compile_inner`); a site whose target is not yet loaded is
    //    simply absent from that map, so *that* method bails at the builder
    //    instead of here — the not-yet-loaded slow path in
    //    `jit_typecheck_resolve` can run a user classloader's
    //    `loadClass`/`findClass`, arbitrary Java this tier does not host
    //    inside a helper call.
    if !scan.multianewarray_ops.is_empty() {
        return ir_reject("!scan.multianewarray_ops.is_empty()");
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

/// Maximum `new` sites. Was 3, then 16 while a surviving allocation lowered
/// through the shared `emit_new_object_stub` (a CALL where the single-pass
/// backend bumps a TLAB inline). Since 2026-09-02 the optimizing tier has its
/// own inline bump (`emit_inline_tlab_new_ir`) and the cap sits with every
/// neighbouring cap at 64. It never applied to arrays, which are refused
/// outright.
pub const IR_MAX_ALLOCATIONS: usize = 64;

/// cov-06: maximum `anewarray` sites. Same budget posture as
/// [`IR_MAX_ALLOCATIONS`] — every admitted REFERENCE-array site is always
/// lowered through the shared `emit_new_array_stub` (a reference array is never
/// scalar-replaced), so this bounds the stub-call cost, not an optimization
/// headroom. It does not cover `newarray` (0xbc), which is where a
/// scalar-replaceable primitive array comes from.
pub const IR_MAX_ARRAY_ALLOCATIONS: usize = 16;

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

/// Walk iterations [`IrBuilder::build`] is allowed per byte of the combined
/// bytecode buffer before it refuses the method.
///
/// The *structural* bound is one iteration per instruction: `pc` advances
/// monotonically inside a range, the block walk's ranges are disjoint, and each
/// spliced body is entered once. Four visits per BYTE is therefore four times
/// looser than the loosest possible legal walk (an all-one-byte-opcode method),
/// which is the point — this is a backstop against a bookkeeping bug, not a
/// complexity policy, and it must never fire on a real method.
///
/// It exists because the walk had no enforced bound at all until 2026-09-17 and
/// a single mis-selected profiled branch could make it run forever; see
/// [`IrBuilder::prune_always_taken_branch`]. [`IR_MAX_GRAPH_NODES`] is not a
/// substitute: it is checked only once `build` has returned.
pub const IR_BUILD_STEPS_PER_CODE_BYTE: usize = 4;

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

    // ── Bottom-tested (rotated) loops ────────────────────────────────────

    /// `int f(int n) { int s = 0; for (int i = 0; i < n; i++) s += i; return s; }`
    /// as ECJ emits it: jump forward to the test, branch back to the body.
    const ROTATED_FOR: [u8; 23] = [
        0x03, 0x3c, // 0: iconst_0; 1: istore_1        s = 0
        0x03, 0x3d, // 2: iconst_0; 3: istore_2        i = 0
        0xa7, 0x00, 0x0a, // 4: goto 14
        0x1b, 0x1c, 0x60, 0x3c, // 7: iload_1; iload_2; iadd; istore_1   body
        0x84, 0x02, 0x01, // 11: iinc 2, 1
        0x1c, 0x1a, // 14: iload_2; 15: iload_0        test
        0xa1, 0xff, 0xf7, // 16: if_icmplt 7
        0x1b, 0xac, // 19: iload_1; 20: ireturn
        0, 0,
    ];

    /// `int f(int n) { do { n = n - 3; } while (n > 0); return n; }`. The loop
    /// header is pc 0, where the method entry is the forward edge.
    const DO_WHILE_AT_ENTRY: [u8; 12] = [
        0x1a, 0x06, 0x64, 0x3b, // 0: iload_0; iconst_3; isub; istore_0
        0x1a, // 4: iload_0
        0x9d, 0xff, 0xfb, // 5: ifgt 0
        0x1a, 0xac, // 8: iload_0; 9: ireturn
        0, 0,
    ];

    /// A javac-shaped outer loop around an ECJ-shaped inner one:
    /// `int f(int n) { int c = 0; for (int i = 0; i < n; i++) for (int j = 0; j < i; j++) c += j; return c; }`.
    const ROTATED_INSIDE_JAVAC_LOOP: [u8; 36] = [
        0x03, 0x3c, // 0: c = 0
        0x03, 0x3d, // 2: i = 0
        0x1c, 0x1a, 0xa2, 0x00, 0x1a, // 4: iload_2; iload_0; if_icmpge 32   outer header
        0x03, 0x3e, // 9: j = 0
        0xa7, 0x00, 0x0a, // 11: goto 21
        0x1b, 0x1d, 0x60, 0x3c, // 14: iload_1; iload_3; iadd; istore_1   inner body
        0x84, 0x03, 0x01, // 18: iinc 3, 1
        0x1d, 0x1c, 0xa1, 0xff, 0xf7, // 21: iload_3; iload_2; if_icmplt 14   inner test
        0x84, 0x02, 0x01, // 26: iinc 2, 1
        0xa7, 0xff, 0xe7, // 29: goto 4
        0x1b, 0xac, // 32: iload_1; 33: ireturn
        0, 0,
    ];

    /// A rotated loop whose body joins an if/else:
    /// `int f(int n) { int s = 0; for (int i = 0; i < n; i++) { if ((i & 1) == 0) s += i; else s -= 1; } return s; }`.
    const ROTATED_WITH_IF_ELSE: [u8; 35] = [
        0x03, 0x3c, // 0: s = 0
        0x03, 0x3d, // 2: i = 0
        0xa7, 0x00, 0x16, // 4: goto 26
        0x1c, 0x04, 0x7e, // 7: iload_2; iconst_1; iand        body
        0x9a, 0x00, 0x0a, // 10: ifne 20
        0x1b, 0x1c, 0x60, 0x3c, // 13: s += i
        0xa7, 0x00, 0x06, // 17: goto 23
        0x84, 0x01, 0xff, // 20: iinc 1, -1
        0x84, 0x02, 0x01, // 23: iinc 2, 1                   join
        0x1c, 0x1a, // 26: iload_2; 27: iload_0              test
        0xa1, 0xff, 0xeb, // 28: if_icmplt 7
        0x1b, 0xac, // 31: iload_1; 32: ireturn
        0, 0,
    ];

    /// The id of the `Op::Merge` the builder made for `pc`.
    fn merge_at(graph: &Graph, pc: usize) -> NodeId {
        graph
            .nodes
            .iter()
            .position(|n| matches!(n.op, Op::Merge) && n.bytecode_pc == Some(pc))
            .unwrap_or_else(|| panic!("no merge at pc {pc}")) as NodeId
    }

    /// The merge at `header` is a loop header with one entry and one back
    /// edge, and at least one phi carries a value around it.
    fn assert_loop_header(graph: &Graph, header: usize) {
        let merge = merge_at(graph, header);
        assert_eq!(
            graph.nodes[merge as usize].inputs.len(),
            2,
            "pc {header}: one entry edge and one back edge"
        );
        assert!(
            graph.nodes.iter().any(|n| matches!(n.op, Op::Phi)
                && n.input_opt(0) == Some(merge)
                && n.inputs.len() == 3),
            "pc {header}: a loop-carried phi with its back-edge input patched in"
        );
    }

    /// `static int f(int n) { return sumTo(n) + 1; }`, with `sumTo` the ECJ
    /// loop of [`ROTATED_FOR`] spliced in at the `invokestatic` (pc 1).
    /// Returns `(combined, caller_len, sites, base)`.
    fn rotated_callee_splice_fixture() -> (Vec<u8>, usize, HashMap<usize, IrInlineSite>, usize) {
        let caller = [
            0x1a, // 0: iload_0
            0xb8, 0x00, 0x01, // 1: invokestatic #1  sumTo(I)I
            0x04, 0x60, // 4: iconst_1; 5: iadd
            0xac, // 6: ireturn
        ];
        let mut combined = caller.to_vec();
        let base = combined.len();
        combined.extend_from_slice(&ROTATED_FOR[..21]);
        combined.extend_from_slice(&[0, 0]);
        let mut sites = HashMap::new();
        sites.insert(
            1,
            IrInlineSite {
                base,
                code_len: 21,
                num_args: 1,
                max_locals: 3,
                arg_local_slots: vec![0],
                returns_value: true,
                receiver_is_arg0: false,
                method_key: "P.sumTo:(I)I".to_string(),
                class_id: 0,
            },
        );
        (combined, caller.len(), sites, base)
    }

    #[test]
    fn a_spliced_callee_with_a_rotated_loop_builds_instead_of_refusing_the_caller() {
        let (combined, caller_len, sites, base) = rotated_callee_splice_fixture();
        let mut builder = IrBuilder::new(1, 1);
        builder.set_inline_sites(sites);
        let graph = builder
            .build(&combined, caller_len)
            .expect("a callee body with a rotated loop must splice, not refuse the caller");
        // The callee's test block (its pc 14) heads the loop, in combined pcs.
        assert_loop_header(&graph, base + 14);
        assert!(
            !graph.nodes.iter().any(|n| matches!(n.op, Op::Call { .. })),
            "the spliced call must be gone, not emitted alongside the body",
        );
    }

    #[test]
    fn a_rotated_for_loop_builds_with_its_test_block_as_the_header() {
        let graph = build_ir(&ROTATED_FOR, 21, 1, 3);
        assert_loop_header(&graph, 14);
        assert_eq!(
            graph.nodes[merge_at(&graph, 7) as usize].inputs.len(),
            1,
            "the body is an ordinary merge reached only by the test's branch"
        );
    }

    #[test]
    fn a_do_while_whose_header_is_the_method_entry_builds() {
        let graph = build_ir(&DO_WHILE_AT_ENTRY, 10, 1, 1);
        assert_loop_header(&graph, 0);
    }

    #[test]
    fn a_rotated_loop_nested_in_a_javac_loop_builds() {
        let graph = build_ir(&ROTATED_INSIDE_JAVAC_LOOP, 34, 1, 4);
        assert_loop_header(&graph, 4);
        assert_loop_header(&graph, 21);
    }

    #[test]
    fn a_rotated_loop_with_an_if_else_join_in_its_body_builds() {
        let graph = build_ir(&ROTATED_WITH_IF_ELSE, 33, 1, 3);
        assert_loop_header(&graph, 26);
        assert_eq!(
            graph.nodes[merge_at(&graph, 23) as usize].inputs.len(),
            2,
            "the if/else join inside the body is a forward merge of both arms"
        );
    }

    /// The order and headers `BlockWalk` computes for the ECJ `for`, block by
    /// block. A pc-order method never gets a block walk at all.
    #[test]
    fn the_block_walk_visits_the_test_before_the_body_it_dominates() {
        fn walk_for(code: &[u8], len: usize) -> (bool, Option<BlockWalk>) {
            let Ok(verified) = cratonvm_reader::verified_code(&code[..len]) else {
                panic!("fixture does not verify");
            };
            let reachable = normally_reachable_pcs(&verified, len);
            let headers: HashSet<usize> = verified
                .loop_headers()
                .iter()
                .map(|&h| h as usize)
                .filter(|h| reachable.contains(h))
                .collect();
            let rotated = has_rotated_loop_header(&verified, &reachable, &headers, len);
            (
                rotated,
                BlockWalk::reverse_postorder(&verified, &reachable, len),
            )
        }

        let (rotated, walk) = walk_for(&ROTATED_FOR, 21);
        assert!(rotated, "the ECJ body has no forward entry");
        let walk = walk.expect("an ECJ loop has a walk");
        // Entry block, then test + exit (adjacent in the bytecode), then body.
        assert_eq!(walk.ranges, vec![(0, 7), (14, 21), (7, 14)]);
        assert_eq!(walk.headers, HashSet::from([14]));

        let (rotated, _) = walk_for(&ROTATED_INSIDE_JAVAC_LOOP, 34);
        assert!(rotated);
        let (rotated, _) = walk_for(&DO_WHILE_AT_ENTRY, 10);
        assert!(!rotated, "pc 0 is entered by the method entry");
        // javac's nested counted loops: both headers are entered by fall-through.
        let javac_nested = [
            0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x19, 0x03, 0x3e, 0x1d, 0x1a, 0xa2,
            0x00, 0x0c, 0x84, 0x01, 0x01, 0x84, 0x03, 0x01, 0xa7, 0xff, 0xf5, 0x84, 0x02, 0x01,
            0xa7, 0xff, 0xe8, 0x1b, 0xac, 0, 0,
        ];
        let (rotated, _) = walk_for(&javac_nested, 33);
        assert!(!rotated, "a javac loop keeps the pc walk");
    }

    /// `int f(int x) { return x == 0 ? 2 : 1; }`, whose `ifeq` at pc 1 the
    /// profile says is always taken.
    ///
    /// Pruned, the branch becomes a guard plus a jump: the `Op::If` is gone,
    /// the cold arm (`iconst_1`, pcs 4-5) is never built, and an `Op::Guard`
    /// anchored at pc 1 carries the transfer back to the interpreter for the
    /// case the profile never saw.
    ///
    /// Asserted against the UNPRUNED build of the same bytecode rather than
    /// against absolute node counts, so the test says "pruning changed this"
    /// rather than restating today's node numbering.
    #[test]
    fn an_always_taken_branch_becomes_a_guard_and_a_jump() {
        let code = [
            0x1a, // 0: iload_0
            0x99, 0x00, 0x07, // 1: ifeq +7 -> 8
            0x04, // 4: iconst_1     <- the cold arm
            0xa7, 0x00, 0x04, // 5: goto +4 -> 9
            0x05, // 8: iconst_2
            0xac, // 9: ireturn
            0, 0, 0,
        ];

        let plain = build_ir(&code, 10, 1, 1);
        assert!(
            plain.nodes.iter().any(|n| matches!(n.op, Op::If)),
            "the control case must build an Op::If",
        );
        assert!(
            !plain.nodes.iter().any(|n| matches!(n.op, Op::Guard { .. })),
            "nothing unpruned may plant a guard here",
        );

        let mut builder = IrBuilder::new(1, 1);
        builder.set_pruned_branches(std::iter::once(1usize).collect());
        let pruned = builder.build(&code, 10).expect("pruned build");

        assert!(
            pruned
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::Guard { bci: 1 })),
            "the pruned branch must plant a guard at its own bci",
        );
        assert!(
            !pruned.nodes.iter().any(|n| matches!(n.op, Op::If)),
            "the pruned branch must not also build an Op::If",
        );
        // The cold arm produced `iconst_1`; the surviving arm produces
        // `iconst_2`. Only the second may be in the graph.
        let consts: Vec<i64> = pruned
            .nodes
            .iter()
            .filter_map(|n| match n.op {
                Op::Const(v) => Some(v),
                _ => None,
            })
            .collect();
        assert!(
            consts.contains(&2) && !consts.contains(&1),
            "the cold arm must be gone; constants were {consts:?}",
        );
    }

    /// The cold arm of a pruned branch is walked as DEAD code, not jumped over,
    /// so a merge inside it that another branch targets is still activated
    /// (round 9 wave 2, `ir-branch-prune-skips-the-cold-arm-by-pc`).
    ///
    /// `pc 5` is pruned (always taken → 16). Its cold arm `[8, 16)` contains
    /// pc 12, which the branch at pc 1 also targets. Jumping to 16 left that
    /// merge with a recorded predecessor and no activation, and the build was
    /// refused by `audit_unactivated_merges`.
    #[test]
    fn a_merge_inside_a_pruned_cold_arm_is_still_activated() {
        let code = [
            0x1a, // 0: iload_0
            0x9a, 0x00, 0x0b, // 1: ifne +11 -> 12
            0x1b, // 4: iload_1
            0x99, 0x00, 0x0b, // 5: ifeq +11 -> 16   (PRUNED)
            0x06, // 8: iconst_3      <- the cold arm
            0x3c, // 9: istore_1
            0x00, // 10: nop
            0x00, // 11: nop
            0x04, // 12: iconst_1     <- merge: pred from pc 1 (and cold 11)
            0xa7, 0x00, 0x04, // 13: goto +4 -> 17
            0x05, // 16: iconst_2     <- the pruned branch's target
            0xac, // 17: ireturn
            0, 0, 0,
        ];
        let mut b = IrBuilder::new(2, 2);
        b.set_pruned_branches(std::iter::once(5usize).collect());
        let graph = b
            .build(&code, 18)
            .expect("the cold arm is walked as dead code; the build must succeed");

        assert!(
            graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::Guard { bci: 5 })),
            "the pruned branch plants its guard",
        );
        let ifs: Vec<NodeId> = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.op == Op::If)
            .map(|(i, _)| i as NodeId)
            .collect();
        assert_eq!(
            ifs.len(),
            1,
            "only the unpruned branch at pc 1 builds an If"
        );
        let m12 = merge_at(&graph, 12);
        let preds = graph.nodes[m12 as usize].inputs.to_vec();
        assert_eq!(
            preds.len(),
            1,
            "pc 12's only LIVE predecessor is the taken edge of pc 1: {preds:?}"
        );
        let p = &graph.nodes[preds[0] as usize];
        assert!(
            matches!(p.op, Op::Proj(_)) && p.inputs.first() == Some(&ifs[0]),
            "the predecessor is a projection of the If at pc 1",
        );
        assert_eq!(
            graph.nodes[merge_at(&graph, 17) as usize].inputs.len(),
            2,
            "pc 17 joins the goto at 13 and the fall-through from 16",
        );
        assert!(
            !graph.nodes.iter().any(|n| n.op == Op::Const(3)),
            "the cold arm's own code is not built",
        );
    }

    /// A guard has nothing to resume to inside a relocated body, so the tier
    /// must decline to speculate there rather than set `splice_guard_seen` and
    /// lose the whole method.
    #[test]
    fn pruning_is_declined_inside_a_splice() {
        let mut b = IrBuilder::new(1, 1);
        b.set_pruned_branches(std::iter::once(1usize).collect());
        // No splice is open in this unit context, so the refusal under test is
        // the OTHER one: a pc with no recorded frame state. Drive it by asking
        // about a pc the walk has not reached.
        assert!(
            !b.prune_always_taken_branch(1, NO_NODE, 8),
            "a bci with no safepoint snapshot must not be speculated on",
        );
    }

    /// A bottom-tested loop's back edge is always-taken while the loop is still
    /// running, and pruning it used to make `build` never return.
    ///
    /// `do { i++; } while (i < 10); return i;` is not rotated — its header at
    /// pc 2 has a reachable lower-pc predecessor, so `has_rotated_loop_header`
    /// leaves it on the pc walk — and its `if_icmplt` back edge is exactly what
    /// a profile taken at an OSR trigger reports as `not_taken == 0`. Pruning
    /// it recorded the header as a predecessor and then set `pc = 2`, landing
    /// the walk back on an already-activated loop header: a second generation
    /// of φs, an orphaned first generation, and then the same branch pruned
    /// again, forever.
    ///
    /// This test HANGS against the pre-2026-09-17 builder, which is the point:
    /// it is a regression test for a non-termination, so it asserts on the
    /// result of a build that used to have none.
    #[test]
    fn a_profiled_always_taken_backward_branch_is_never_pruned() {
        let code = [
            0x03, // 0: iconst_0
            0x3b, // 1: istore_0
            0x1a, // 2: iload_0        <- loop header
            0x04, // 3: iconst_1
            0x60, // 4: iadd
            0x3b, // 5: istore_0
            0x1a, // 6: iload_0
            0x10, 0x0a, // 7: bipush 10
            0xa1, 0xff, 0xf9, // 9: if_icmplt -7 -> 2   (the back edge)
            0x1a, // 12: iload_0
            0xac, // 13: ireturn
            0, 0, 0,
        ];

        let mut b = IrBuilder::new(0, 1);
        b.set_pruned_branches(std::iter::once(9usize).collect());
        let graph = b
            .build(&code, 14)
            .expect("the do/while must still build, unpruned");

        assert!(
            !graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::Guard { bci: 9 })),
            "a back edge must not be speculated on — its cold arm is the loop \
             EXIT, unobserved because the profile was taken mid-loop",
        );
        assert!(
            graph.nodes.iter().any(|n| matches!(n.op, Op::If)),
            "the back edge must keep its ordinary Op::If",
        );
    }

    /// Re-activating a join rebuilds every φ on it and resets its control
    /// inputs, orphaning the first generation. It is always a bug in the walk,
    /// so it is a refusal rather than a duplication.
    #[test]
    fn activating_a_merge_twice_refuses_the_build() {
        let mut b = IrBuilder::new(0, 1);
        b.ensure_merge(4);
        b.add_merge_predecessor(4);
        b.activate_merge(4);
        assert!(
            b.build_refused.is_none(),
            "one activation is the normal path",
        );

        b.activate_merge(4);
        assert!(
            b.build_refused.is_some(),
            "a second activation of the same merge must refuse the method",
        );
    }

    /// The same guard on the loop-header path: `activate_loop_header` builds a
    /// fresh φ per live local every time it runs, and a second run would leave
    /// one value per predecessor against a region that now has two.
    #[test]
    fn activating_a_loop_header_twice_refuses_the_build() {
        let mut b = IrBuilder::new(0, 1);
        b.ensure_merge(4);
        b.add_merge_predecessor(4);
        b.activate_loop_header(4);
        assert!(b.build_refused.is_none());

        b.activate_loop_header(4);
        assert!(
            b.build_refused.is_some(),
            "a second activation of the same loop header must refuse the method",
        );
    }

    /// A predecessor arriving at an already-activated merge that is NOT a
    /// registered loop header used to be dropped on the floor: no edge, no
    /// values, no bailout, and a graph the verifier cannot tell from a correct
    /// one because the φs agree with the *reduced* predecessor count.
    #[test]
    fn a_back_edge_into_a_merge_with_no_loop_phis_refuses_instead_of_dropping_it() {
        let mut b = IrBuilder::new(0, 1);
        b.ensure_merge(4);
        b.add_merge_predecessor(4);
        // A FORWARD join: `activate_merge` registers no entry in `loop_phis`.
        b.activate_merge(4);
        let merge_id = b.merges[&4].merge_id;
        let preds_before = b.graph.nodes[merge_id as usize].inputs.len();
        assert!(b.build_refused.is_none());

        // A second predecessor, after activation. There are no loop φs to
        // back-patch, so the edge cannot be recorded.
        b.add_merge_predecessor(4);
        assert_eq!(
            b.graph.nodes[merge_id as usize].inputs.len(),
            preds_before,
            "this is the dropped edge — the point is that it is now REPORTED, \
             not that it is recorded",
        );
        assert!(
            b.build_refused.is_some(),
            "\"I cannot record this edge\" must not be spelled the same way as \
             \"nothing to do\"",
        );
    }

    /// A φ whose type *successfully* changes must re-type the φs that merge it.
    ///
    /// The `Int` → `Ref` retype is the case `retype_phi`'s own doc opens with,
    /// and until 2026-09-17 it was the one case that did not propagate: a φ
    /// built between header activation and the back-edge patch kept the
    /// provisional `Int`, so it carried an oop the oop map never reported and
    /// `zero_ref_phi_slots` never zeroed.
    #[test]
    fn a_successful_retype_propagates_to_the_phis_that_merge_it() {
        let mut b = IrBuilder::new(0, 1);
        let region = b.graph.add(Op::Region, IrType::Control, vec![], None);
        let merge = b.graph.add(Op::Merge, IrType::Control, vec![], None);
        // The loop-carried φ, created with only its (undefined) entry value, so
        // it takes the conservative fallback.
        let loop_phi = b
            .graph
            .add(Op::Phi, PHI_TYPE_FALLBACK, vec![region, NO_NODE], None);
        // A φ created LATER in the body that consumes it, typed from the
        // provisional type.
        let downstream = b
            .graph
            .add(Op::Phi, PHI_TYPE_FALLBACK, vec![merge, loop_phi], None);

        let back = b.graph.add(
            Op::New {
                class_id: 3,
                num_fields: 0,
            },
            IrType::Ref,
            vec![],
            None,
        );
        b.graph.nodes[loop_phi as usize].inputs.push(back);
        b.retype_phi(loop_phi, 0);

        assert_eq!(
            b.graph.nodes[loop_phi as usize].ty,
            IrType::Ref,
            "the back-edge reference retypes the loop φ",
        );
        assert_eq!(
            b.graph.nodes[downstream as usize].ty,
            IrType::Ref,
            "and every φ that merges it — otherwise this one holds an oop \
             typed Int, invisible to the oop map and to zero_ref_phi_slots",
        );
        assert!(
            b.build_refused.is_none(),
            "a converging propagation is not a refusal",
        );
    }

    /// JVMS §2.6.1 in both directions, at the one place the abstract locals
    /// array can express it.
    #[test]
    fn a_category_2_store_clears_the_reserved_slot_and_a_category_1_store_kills_the_pair() {
        let mut b = IrBuilder::new(0, 3);
        let obj = b.graph.add(
            Op::New {
                class_id: 1,
                num_fields: 0,
            },
            IrType::Ref,
            vec![],
            None,
        );
        b.store_local(1, obj);
        assert_eq!(b.locals[1], obj);

        // A `long` at slot 0 reserves slot 1. The reference that was there is
        // dead in the JVM's model; leaving it would keep it a DCE root through
        // every later `SafepointSnapshot` and publish it as a live GC root.
        let wide = b.graph.add(Op::Const(7), IrType::Long, vec![], None);
        b.store_local(0, wide);
        assert_eq!(b.locals[0], wide);
        assert_eq!(
            b.locals[1], NO_NODE,
            "a category-2 store must not leave a stale value in its reserved \
             high half",
        );

        // …and overwriting that reserved half destroys the whole `long`.
        // Without this, `deopt_resume::ir_deopt_locals` sees a category-2 entry
        // at slot 0, skips slot 1, and shifts every later local by one.
        let narrow = b.graph.add(Op::Const(5), IrType::Int, vec![], None);
        b.store_local(1, narrow);
        assert_eq!(b.locals[1], narrow);
        assert_eq!(
            b.locals[0], NO_NODE,
            "storing into a long's high half destroys the long",
        );
    }

    /// The same rule, observed where it matters: in the deopt frame states the
    /// builder records.
    #[test]
    fn a_long_store_does_not_retain_the_high_half_slot_in_later_frame_states() {
        let code = [
            0x09, // 0: lconst_0
            0x3f, // 1: lstore_0    (occupies slots 0 and 1)
            0x04, // 2: iconst_1
            0x3c, // 3: istore_1    (overwrites the long's reserved half)
            0x1b, // 4: iload_1
            0xac, // 5: ireturn
            0, 0, 0,
        ];
        let graph = IrBuilder::new(0, 3)
            .build(&code, 6)
            .expect("the long store must build");
        let last = graph
            .safepoints
            .iter()
            .find(|sp| sp.bci == 5)
            .expect("a frame state at the ireturn");
        assert_eq!(
            last.locals.first().copied(),
            Some(NO_NODE),
            "the long was destroyed by the istore into its high half, so its \
             slot must read as uninitialised rather than as a live category-2 \
             value the deopt walk would skip a slot for",
        );
    }

    /// `lstore_0; lstore_1`: the second `long` overwrites the first one's
    /// high half, so the first is destroyed. Until 2026-09-18 only a
    /// category-1 store looked at `idx - 1`, and the deopt walk then skipped
    /// the live `long` at slot 1 as if it were the first one's high half.
    #[test]
    fn a_category_2_store_into_a_long_s_high_half_kills_the_long() {
        let mut b = IrBuilder::new(0, 4);
        let first = b.graph.add(Op::Const(1), IrType::Long, vec![], None);
        b.store_local(0, first);
        let second = b.graph.add(Op::Const(2), IrType::Long, vec![], None);
        b.store_local(1, second);
        assert_eq!(b.locals[1], second);
        assert_eq!(
            b.locals[0], NO_NODE,
            "the first long's high half was overwritten, so the first long is gone",
        );
        assert_eq!(b.locals[2], NO_NODE, "the second long's own high half");
        assert!(b.build_refused.is_none());
    }

    /// A local index past the frame is refused like a store past it, not a
    /// panic on the compiler thread.
    #[test]
    fn a_load_or_iinc_of_a_slot_past_the_frame_refuses_instead_of_panicking() {
        // iload 5; ireturn — one local.
        let load = [0x15, 0x05, 0xac];
        assert!(IrBuilder::new(0, 1).build(&load, 3).is_none());
        // iinc 5, 1; iconst_0; ireturn
        let inc = [0x84, 0x05, 0x01, 0x03, 0xac];
        assert!(IrBuilder::new(0, 1).build(&inc, 5).is_none());
    }

    /// `int f(int c) { if (c != 0) { int x = 5; } else { Object y = null; }
    /// return 0; }` — javac gives `x` and `y` the same slot, so the join at
    /// pc 11 merges an `int` with a reference. The slot is dead there (TOP to
    /// the JVM verifier) and must be described as undefined, not as a φ that
    /// could only take `PHI_TYPE_FALLBACK` and that `check_types` rejects.
    #[test]
    fn a_slot_reused_for_two_types_is_dead_at_the_join_not_a_conflicting_phi() {
        let code = [
            0x1a, // 0: iload_0
            0x99, 0x00, 0x08, // 1: ifeq -> 9
            0x08, // 4: iconst_5
            0x3c, // 5: istore_1          int in slot 1
            0xa7, 0x00, 0x05, // 6: goto -> 11
            0x01, // 9: aconst_null
            0x4c, // 10: astore_1         reference in slot 1
            0x03, // 11: iconst_0         the join
            0xac, // 12: ireturn
        ];
        let graph = IrBuilder::new(1, 2)
            .build(&code, 13)
            .expect("a reused slot must not refuse the method");
        assert!(
            !graph
                .nodes
                .iter()
                .any(|n| n.op == Op::Phi && n.ty != IrType::Memory),
            "no value φ may be built for a slot whose predecessors do not join",
        );
        let at_join = graph
            .safepoints
            .iter()
            .find(|sp| sp.bci == 11)
            .expect("a frame state at the join");
        assert_eq!(at_join.locals.get(1).copied(), Some(NO_NODE));
        let verdict = crate::ir_verify::verify_graph(
            &graph,
            "test",
            crate::ir_verify::VerifyOptions::default(),
        );
        assert!(verdict.is_ok(), "{verdict:?}");

        // The control: the same shape with an `int` on both arms keeps its φ.
        let mut same = code;
        same[9] = 0x05; // iconst_2
        same[10] = 0x3c; // istore_1
        let graph = IrBuilder::new(1, 2).build(&same, 13).expect("builds");
        assert!(
            graph
                .nodes
                .iter()
                .any(|n| n.op == Op::Phi && n.ty == IrType::Int),
            "a slot whose predecessors DO join still gets its φ",
        );
    }

    /// The loop-header form of the same thing: a dead `Object` in slot 1
    /// before the loop, an `int` stored there inside it. The header φ is built
    /// from the entry edge (`Ref`) before the back edge exists, so the
    /// conflict appears only at `patch_loop_backedge` — and the φ is retired
    /// at the end of the build rather than left for `check_types` to reject.
    #[test]
    fn a_loop_phi_that_only_conflicts_on_its_back_edge_is_retired() {
        let code = [
            0x01, // 0: aconst_null
            0x4c, // 1: astore_1           dead reference before the loop
            0x1a, // 2: iload_0            loop header
            0x9e, 0x00, 0x0b, // 3: ifle -> 14
            0x04, // 6: iconst_1
            0x3c, // 7: istore_1           int in the same slot
            0x84, 0x00, 0xff, // 8: iinc 0, -1
            0xa7, 0xff, 0xf7, // 11: goto -> 2
            0x03, // 14: iconst_0
            0xac, // 15: ireturn
        ];
        let graph = IrBuilder::new(1, 2)
            .build(&code, 16)
            .expect("a reused slot around a loop must not refuse the method");
        for (id, n) in graph.nodes.iter().enumerate() {
            if n.op == Op::Phi && n.ty != IrType::Memory {
                assert!(
                    !phi_inputs_conflict(&graph, &n.inputs),
                    "n{id} is a live φ whose inputs do not join",
                );
            }
        }
        let in_body = graph
            .safepoints
            .iter()
            .find(|sp| sp.bci == 3)
            .expect("a frame state inside the loop");
        assert_eq!(
            in_body.locals.get(1).copied(),
            Some(NO_NODE),
            "the retired φ's snapshot slots read as undefined",
        );
        let counter = in_body.locals[0];
        assert!(
            matches!(graph.node_opt(counter), Some(n) if n.op == Op::Phi && n.ty == IrType::Int),
            "the loop counter's φ is untouched",
        );
        let verdict = crate::ir_verify::verify_graph(
            &graph,
            "test",
            crate::ir_verify::VerifyOptions::default(),
        );
        assert!(verdict.is_ok(), "{verdict:?}");
    }

    /// A predecessor recorded into a merge the walk never activates is an
    /// edge with nowhere to go, and refuses the build; a merge nothing ever
    /// reached is an orphan, and is removed.
    #[test]
    fn an_unactivated_merge_is_killed_when_orphaned_and_refused_when_it_lost_an_edge() {
        let mut b = IrBuilder::new(0, 1);
        let orphan = b.ensure_merge(20).merge_id;
        assert_eq!(b.audit_unactivated_merges(), None);
        assert_eq!(b.graph.nodes[orphan as usize].op, Op::Dead);

        b.ensure_merge(30);
        b.add_merge_predecessor(30);
        assert_eq!(b.audit_unactivated_merges(), Some(30));
    }

    /// `goto_w` is `goto` with a wide offset.
    #[test]
    fn goto_w_builds_like_goto() {
        let code = [
            0x04, // 0: iconst_1
            0xc8, 0x00, 0x00, 0x00, 0x05, // 1: goto_w -> 6
            0xac, // 6: ireturn
        ];
        let graph = IrBuilder::new(0, 0)
            .build(&code, 7)
            .expect("goto_w must build");
        assert!(graph.nodes.iter().any(|n| n.op == Op::Return));
        assert_eq!(graph.nodes[merge_at(&graph, 6) as usize].inputs.len(), 1);
    }

    /// A spliced `fastore`/`dastore` must target an array the splice itself
    /// allocated, exactly like every other store arm: a deopt inside the
    /// spliced region re-executes the whole call, and a write into the
    /// CALLER'S array would be committed twice.
    #[test]
    fn a_spliced_fp_array_store_into_a_callers_array_refuses_the_splice() {
        fn site(base: usize, code_len: usize, num_args: usize) -> IrInlineSite {
            IrInlineSite {
                base,
                code_len,
                num_args,
                max_locals: num_args,
                arg_local_slots: (0..num_args as u32).collect(),
                returns_value: false,
                receiver_is_arg0: false,
                method_key: "P.f:([F)V".to_string(),
                class_id: 0,
            }
        }

        // Caller: aload_0; invokestatic f([F)V; return.
        // Callee: aload_0; iconst_0; fconst_1; fastore; return.
        let mut combined = vec![0x2a, 0xb8, 0x00, 0x01, 0xb1];
        let base = combined.len();
        combined.extend_from_slice(&[0x2a, 0x03, 0x0c, 0x51, 0xb1]);
        let mut b = IrBuilder::new(1, 1);
        b.set_inline_sites(HashMap::from([(1usize, site(base, 5, 1))]));
        assert!(
            b.build(&combined, base).is_none(),
            "a spliced float store into the caller's array must refuse",
        );

        // Caller: invokestatic g()V; return.
        // Callee: iconst_1; newarray float; iconst_0; fconst_1; fastore; return.
        let mut combined = vec![0xb8, 0x00, 0x01, 0xb1];
        let base = combined.len();
        combined.extend_from_slice(&[0x04, 0xbc, 0x06, 0x03, 0x0c, 0x51, 0xb1]);
        let mut b = IrBuilder::new(0, 0);
        b.set_inline_sites(HashMap::from([(0usize, site(base, 7, 0))]));
        let graph = b
            .build(&combined, base)
            .expect("a store into an array the splice made is local and builds");
        assert!(graph
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::ArrayStore(MemKind::Float))));
    }

    /// `nop` is admitted by the scanner and lowered by the single-pass tier, so
    /// a `nop`-bearing method reached the builder and died at its `_ =>`
    /// catch-all after a full partial build. It builds now.
    #[test]
    fn a_method_containing_nop_builds() {
        let code = [
            0x00, // 0: nop
            0x1a, // 1: iload_0
            0x00, // 2: nop
            0xac, // 3: ireturn
            0, 0,
        ];
        assert!(
            IrBuilder::new(1, 1).build(&code, 4).is_some(),
            "`nop` must be lowered here, not refused after a full graph build",
        );
    }

    /// `athrow` leaves the compiled frame through the `i64::MIN` sentinel and
    /// nothing on that path releases a monitor this frame took — the same
    /// reason `return` is refused while one is held.
    #[test]
    fn an_athrow_while_a_monitor_is_held_is_refused_like_a_return() {
        // aload_0; athrow — no monitor, so the arm builds.
        let plain = [0x2a, 0xbf, 0, 0];
        assert!(
            IrBuilder::new(1, 1).build(&plain, 2).is_some(),
            "an ordinary athrow must still build",
        );

        // aload_0; monitorenter; aload_0; athrow — the lock is never released.
        let locked = [0x2a, 0xc2, 0x2a, 0xbf, 0, 0];
        assert!(
            IrBuilder::new(1, 1).build(&locked, 4).is_none(),
            "throwing out of a compiled frame while holding a monitor leaks the \
             lock; the invariant must be stated, not left to the exception-table \
             gate upstream",
        );
    }

    /// `lcmp`/`fcmpl`/`fcmpg`/`dcmpl`/`dcmpg` are total, side-effect-free
    /// arithmetic. `ir_optimize` asks `is_pure` when it wants to know whether a
    /// node can touch memory, so leaving them out of it declared every loop
    /// containing one to hold a hard memory barrier and cost it all of its LICM.
    #[test]
    fn the_three_way_comparisons_are_pure_so_they_are_not_memory_barriers() {
        for op in [
            Op::LCmp,
            Op::FCmp {
                double: false,
                nan_greater: true,
            },
            Op::FCmp {
                double: true,
                nan_greater: false,
            },
        ] {
            assert!(op.is_pure(), "{op:?} is pure arithmetic");
            assert!(!op.is_control(), "{op:?}");
            // Verbatim the classification `ir_optimize::loop_has_hard_barrier`
            // and `preheader_may_trap` apply.
            let hard_barrier = !op.is_pure()
                && !op.is_control()
                && !matches!(op, Op::Load(_) | Op::Store(_) | Op::Phi | Op::Dead);
            assert!(
                !hard_barrier,
                "{op:?} must not disqualify its loop from every load hoist",
            );
        }
        // The exclusions that are correct stay correct: these two trap.
        assert!(!Op::Div.is_pure());
        assert!(!Op::Rem.is_pure());
    }

    /// `interned_const` keys on `(value, type)`, so the `Long` case needed only
    /// the call — and without it every `lconst_0`/`lconst_1`, every `ldc2_w`
    /// long and every `ldiv`/`lrem` zero guard added a node for GVN to collapse.
    #[test]
    fn a_repeated_long_constant_is_interned_the_way_an_int_one_is() {
        let mut b = IrBuilder::new(0, 1);
        let first = b.lconst(7);
        let second = b.lconst(7);
        assert_eq!(first, second, "a long constant must dedupe");

        // The TYPE is part of the identity, exactly as it is for the `Ref`
        // zero `aconst_null` materialises: an `Int` 7 is a different node.
        let narrow = b.iconst(7);
        assert_ne!(first, narrow);
        assert_eq!(b.graph.nodes[first as usize].ty, IrType::Long);
        assert_eq!(b.graph.nodes[narrow as usize].ty, IrType::Int);
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

    /// Round 9 wave 10 (irl10), `perf-ir-refuses-volatile-instance-field-methods`:
    /// a SPLICED callee's volatile `getfield`, named by its combined-buffer pc
    /// through `add_spliced_volatile_field_pcs`, builds as the ordered
    /// `Op::Load` (Acquire) the method's own volatile read builds as; the same
    /// body without the pc builds a plain load (the control), and with
    /// admission off the build bails rather than emitting an unordered read.
    #[test]
    fn r9w10_irl10_a_spliced_volatile_getfield_builds_an_ordered_load() {
        // Caller: `static int f(O o) { return g(o); }`
        //   pc 0  aload_0
        //   pc 1  invokestatic #1   (spliced)
        //   pc 4  ireturn
        // Callee `static int g(O o) { return o.v; }`, relocated to combined pc 6:
        //   pc 6  aload_0
        //   pc 7  getfield #2
        //   pc 10 ireturn
        let code = [
            0x2a, 0xb8, 0x00, 0x01, 0xac, 0x00, // caller, code_len 5 (byte 5 is padding)
            0x2a, 0xb4, 0x00, 0x02, 0xac, // relocated callee at [6, 11)
            0, 0,
        ];
        let site = IrInlineSite {
            base: 6,
            code_len: 5,
            num_args: 1,
            max_locals: 1,
            arg_local_slots: vec![0],
            returns_value: true,
            receiver_is_arg0: false,
            method_key: "P.g:(LO;)I".to_string(),
            class_id: 7,
        };
        // `admit`: `None` = the pc is not supplied at all (the control).
        let build = |admit: Option<(bool, bool)>| {
            let mut b = IrBuilder::new(1, 1);
            let mut tables = IrInlineTables::default();
            tables.sites.insert(1, site.clone());
            // The row `append_ir_inline_site` rebases: CALLEE pc 1 + base 6.
            tables.field_info.insert(7, (0usize, b'I'));
            b.apply_inline_tables(tables);
            if let Some((loads, stores)) = admit {
                b.add_spliced_volatile_field_pcs([7usize]);
                // Pinned, so the test does not depend on
                // `CRATONVM_JIT_IR_VOLATILE_FIELDS` in the environment.
                b.set_volatile_field_admission(loads, stores);
            }
            b.build(&code, 5)
        };
        let the_load = |g: &Graph| -> NodeId {
            let loads: Vec<NodeId> = (0..g.nodes.len() as NodeId)
                .filter(|&i| matches!(g.nodes[i as usize].op, Op::Load(_)))
                .collect();
            assert_eq!(loads.len(), 1, "one spliced field read");
            loads[0]
        };

        let plain = build(None).expect("the plain spliced read builds");
        let pl = the_load(&plain);
        assert!(!plain.nodes[pl as usize].is_volatile_access());

        let ordered = build(Some((true, false))).expect("an admitted spliced volatile read builds");
        let vl = the_load(&ordered);
        assert!(
            ordered.nodes[vl as usize].is_volatile_access(),
            "the spliced read is marked volatile"
        );
        assert_eq!(ordered.memory_effect(vl).order, MemOrder::Acquire);

        assert!(
            build(Some((false, false))).is_none(),
            "with admission off a spliced volatile read bails the build"
        );

        // `set_volatile_field_pcs` after the splice pcs MERGES: the caller's
        // own (empty) set does not wipe the spliced pc.
        let mut b = IrBuilder::new(1, 1);
        let mut tables = IrInlineTables::default();
        tables.sites.insert(1, site.clone());
        tables.field_info.insert(7, (0usize, b'I'));
        b.apply_inline_tables(tables);
        b.add_spliced_volatile_field_pcs([7usize]);
        b.set_volatile_field_pcs(HashSet::new());
        b.set_volatile_field_admission(true, false);
        let merged = b.build(&code, 5).expect("merged build");
        let ml = the_load(&merged);
        assert!(merged.nodes[ml as usize].is_volatile_access());
    }

    /// A spliced `getstatic` resolves through `IrInlineTables::static_field_info`,
    /// and a MISSING row bails the whole method.
    ///
    /// Both halves matter, and the second is the one with no other witness.
    /// `ldc` spent its first hour in exactly this state -- the resolver
    /// admitting a callee whose rows nothing rebased, and the builder refusing
    /// the METHOD 65 bytes later, which from outside is indistinguishable from
    /// a workload that has no such callee. Asserting only the positive would
    /// leave the same hole: a future edit that drops the row from
    /// `apply_inline_tables` would still pass.
    #[test]
    fn a_spliced_getstatic_resolves_through_the_rebased_rows() {
        // Caller: `static int f() { return g(); }`
        //   pc 0  invokestatic #1   (spliced)
        //   pc 3  ireturn
        // Callee body, relocated to combined pc 5:
        //   pc 5  getstatic #2
        //   pc 8  ireturn
        let code = [
            0xb8, 0x00, 0x01, 0xac, 0x00, // caller, code_len 4 (byte 4 is padding)
            0xb2, 0x00, 0x02, 0xac, // relocated callee at [5, 9)
            0, 0,
        ];
        let site = IrInlineSite {
            base: 5,
            code_len: 4,
            num_args: 0,
            max_locals: 0,
            arg_local_slots: Vec::new(),
            returns_value: true,
            receiver_is_arg0: false,
            method_key: "P.g:()I".to_string(),
            class_id: 7,
        };

        let mut with_rows = IrBuilder::new(0, 1);
        let mut tables = IrInlineTables::default();
        tables.sites.insert(0, site.clone());
        // The row `append_ir_inline_site` rebases: CALLEE pc 0 + base 5.
        tables
            .static_field_info
            .insert(5, (7u32, 0usize, b'I', false));
        with_rows.apply_inline_tables(tables);
        let graph = with_rows
            .build(&code, 4)
            .expect("a spliced getstatic with its row must build");
        assert!(
            graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::LoadStatic { class_id: 7, .. })),
            "the spliced `getstatic` must lower to an Op::LoadStatic naming the              resolved class, not to a call left behind by a skipped splice",
        );

        // Same site, same bytes, no row.
        let mut without_rows = IrBuilder::new(0, 1);
        let mut bare = IrInlineTables::default();
        bare.sites.insert(0, site);
        without_rows.apply_inline_tables(bare);
        assert!(
            without_rows.build(&code, 4).is_none(),
            "with no row the builder must bail the METHOD -- that is the failure              mode the resolver's admission has to stay in step with",
        );
    }

    /// A spliced `instanceof` resolves through `IrInlineTables::instanceof_info`,
    /// and a MISSING row bails the whole method.
    ///
    /// The `0xc1` arm READS as though a missing row were graceful -- it calls
    /// `plant_uncommon_trap` and compiles the rest. It is not, by default:
    /// `TrapCause::UnresolvedTypeCheck` is gated behind
    /// `ir_unresolved_class_trap_enabled`, which is OFF (the argument for it
    /// having been refuted), so the plant refuses and the arm falls through to
    /// `ir_build_bail`. That is the same fail-closed answer a missing
    /// `getstatic` row gives, and it is why the resolver must refuse a callee
    /// whose targets it could not resolve rather than admit the body and leave
    /// rows out: doing so costs the CALLER its whole optimizing compile.
    ///
    /// Asserting the negative is the point. Reading the arm alone gives the
    /// wrong answer, and a future edit that flips the trap default would change
    /// this behaviour without touching either half of the splice feature.
    #[test]
    fn a_spliced_instanceof_resolves_through_the_rebased_rows() {
        // Caller: `static boolean f(Object o) { return g(o); }`
        //   pc 0  aload_0
        //   pc 1  invokestatic #1   (spliced)
        //   pc 4  ireturn
        // Callee body, relocated to combined pc 6:
        //   pc 6  aload_0
        //   pc 7  instanceof #2
        //   pc 10 ireturn
        let code = [
            0x2a, 0xb8, 0x00, 0x01, 0xac, 0x00, // caller, code_len 5
            0x2a, 0xc1, 0x00, 0x02, 0xac, // relocated callee at [6, 11)
            0, 0,
        ];
        let name = "java/lang/String";
        let (ptr, len) = crate::intern_typecheck_target(name, Some(11));
        let site = IrInlineSite {
            base: 6,
            code_len: 5,
            num_args: 1,
            max_locals: 1,
            arg_local_slots: vec![0],
            returns_value: true,
            receiver_is_arg0: false,
            method_key: "P.g:(Ljava/lang/Object;)Z".to_string(),
            class_id: 7,
        };

        let mut with_rows = IrBuilder::new(1, 1);
        let mut tables = IrInlineTables::default();
        tables.sites.insert(1, site.clone());
        // The row `append_ir_inline_site` rebases: CALLEE pc 1 + base 6.
        tables.instanceof_info.insert(7, (ptr as usize, len));
        with_rows.apply_inline_tables(tables);
        let graph = with_rows
            .build(&code, 5)
            .expect("a spliced instanceof with its row must build");
        assert!(
            graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::InstanceOf { .. })),
            "the spliced `instanceof` must lower to an Op::InstanceOf, not to a              call left behind by a skipped splice",
        );

        // Same site, same bytes, no row.
        let mut without_rows = IrBuilder::new(1, 1);
        let mut bare = IrInlineTables::default();
        bare.sites.insert(1, site);
        without_rows.apply_inline_tables(bare);
        assert!(
            without_rows.build(&code, 5).is_none(),
            "with no row the builder must bail the METHOD -- the `0xc1` arm's              uncommon-trap path is gated off by default, so the resolver's              admission has to stay in step with the rows it produces",
        );
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

    /// Round 9 wave 5 (`volatile5`),
    /// `volatile-instance-fields-are-plain-accesses-in-compiled-code`: a
    /// `getfield` or `putfield` of a VOLATILE field bails the build, so the
    /// method goes to single-pass instead of becoming an unordered
    /// `Op::Load` / `Op::Store` that GVN may merge and LICM may hoist. Each
    /// half is paired with the identical non-volatile build, which must
    /// succeed, so the refusal cannot pass vacuously.
    #[test]
    fn a_volatile_field_access_bails_the_ir_build() {
        // static int get(O o) { return o.v; } — aload_0; getfield #2; ireturn
        let get = [0x2a, 0xb4, 0x00, 0x02, 0xac, 0, 0];
        // static void put(O o, int x) { o.v = x; } — aload_0; iload_1; putfield #2; return
        let put = [0x2a, 0x1b, 0xb5, 0x00, 0x02, 0xb1, 0, 0];
        let cases: [(&[u8], usize, usize, usize); 2] = [(&get[..], 5, 1, 1), (&put[..], 6, 2, 2)];
        for (code, len, params, field_pc) in cases {
            let build = |volatile: bool| {
                let mut b = IrBuilder::new(params, params);
                b.set_field_info(HashMap::from([(field_pc, (0usize, b'I'))]));
                if volatile {
                    b.set_volatile_field_pcs(HashSet::from([field_pc]));
                    // Pin the REFUSAL arm explicitly (round 9 wave 7, irsnap7):
                    // this test is about what the builder does when the IR is
                    // NOT allowed to model the ordering, and must keep saying
                    // so whatever `CRATONVM_JIT_IR_VOLATILE_FIELDS` defaults to
                    // or the environment sets.
                    b.set_volatile_field_admission(false, false);
                }
                b.build(code, len)
            };
            assert!(
                build(false).is_some(),
                "the plain-field control must build (opcode {:#04x})",
                code[field_pc]
            );
            assert!(
                build(true).is_none(),
                "a volatile field access must bail the IR build (opcode {:#04x})",
                code[field_pc]
            );
        }
        // A volatile pc that is NOT a field access refuses nothing.
        let mut b = IrBuilder::new(1, 1);
        b.set_field_info(HashMap::from([(1usize, (0usize, b'I'))]));
        b.set_volatile_field_pcs(HashSet::from([0usize]));
        assert!(
            b.build(&get, 5).is_some(),
            "only the named field pcs refuse"
        );
    }

    /// Round 9 wave 6 (`irsnap6`),
    /// `perf-ir-refuses-volatile-instance-field-methods`: ADMITTED, a volatile
    /// `getfield` / `putfield` builds as the ordinary node marked
    /// `volatile_access`, whose memory effect carries the JMM ordering. A
    /// volatile `putfield` needs the store admission bit as well — the
    /// lowerer's fence — and without it still bails even with loads admitted.
    #[test]
    fn an_admitted_volatile_field_access_builds_a_marked_ordered_node() {
        let get = [0x2a, 0xb4, 0x00, 0x02, 0xac, 0, 0];
        let put = [0x2a, 0x1b, 0xb5, 0x00, 0x02, 0xb1, 0, 0];
        let build =
            |code: &[u8], len: usize, params: usize, field_pc: usize, admit: (bool, bool)| {
                let mut b = IrBuilder::new(params, params);
                b.set_field_info(HashMap::from([(field_pc, (0usize, b'I'))]));
                b.set_volatile_field_pcs(HashSet::from([field_pc]));
                b.set_volatile_field_admission(admit.0, admit.1);
                b.build(code, len)
            };

        let g = build(&get[..], 5, 1, 1, (true, false)).expect("an admitted volatile read builds");
        let loads: Vec<NodeId> = (0..g.nodes.len() as NodeId)
            .filter(|&i| matches!(g.nodes[i as usize].op, Op::Load(_)))
            .collect();
        assert_eq!(loads.len(), 1);
        let load = loads[0];
        assert!(
            g.nodes[load as usize].is_volatile_access(),
            "the read is marked"
        );
        assert!(!g.nodes[load as usize].is_volatile_store());
        assert_eq!(
            g.memory_effect(load).order,
            MemOrder::Acquire,
            "a volatile read is an acquire"
        );

        assert!(
            build(&put[..], 6, 2, 2, (true, false)).is_none(),
            "a volatile store without the lowerer's fence still bails"
        );
        let g = build(&put[..], 6, 2, 2, (true, true)).expect("an admitted volatile store builds");
        let store = (0..g.nodes.len() as NodeId)
            .find(|&i| matches!(g.nodes[i as usize].op, Op::Store(_)))
            .expect("the store");
        assert!(
            g.nodes[store as usize].is_volatile_store(),
            "the store is marked"
        );
        assert_eq!(
            g.memory_effect(store).order,
            MemOrder::SeqCst,
            "a volatile write is a full fence"
        );

        // The mark only lands on a field access, and a plain build is plain.
        let mut g = build(&get[..], 5, 1, 1, (true, false)).expect("builds");
        let not_a_load = (0..g.nodes.len() as NodeId)
            .find(|&i| matches!(g.nodes[i as usize].op, Op::Param(_)))
            .expect("the parameter");
        g.set_volatile_access(not_a_load);
        assert!(!g.nodes[not_a_load as usize].volatile_access);
        let mut plain = IrBuilder::new(1, 1);
        plain.set_field_info(HashMap::from([(1usize, (0usize, b'I'))]));
        let g = plain.build(&get, 5).expect("the plain read builds");
        assert!(
            g.nodes.iter().all(|n| !n.volatile_access),
            "no plain build marks anything"
        );
        let plain_load = (0..g.nodes.len() as NodeId)
            .find(|&i| matches!(g.nodes[i as usize].op, Op::Load(_)))
            .expect("the load");
        assert_eq!(g.memory_effect(plain_load).order, MemOrder::Plain);
    }

    /// Round 9 wave 2, `sub-int-field-stores-are-not-narrowed-by-compiled-code`:
    /// a `putfield` into a `byte`/`short`/`char`/`boolean` field stores the
    /// value narrowed to the field's type, as the interpreter does — and
    /// javac's already-narrowed values get no second conversion.
    #[test]
    fn a_sub_int_putfield_stores_the_narrowed_value() {
        // static int f(O o, int v) { o.b = <value>; return o.b; }
        //   aload_0; <push value>; putfield #1; aload_0; getfield #1; ireturn
        let stored = |push: &[u8], tag: u8| -> (Graph, NodeId) {
            let mut code = vec![0x2a];
            code.extend_from_slice(push);
            let put = code.len();
            code.extend_from_slice(&[0xb5, 0x00, 0x01, 0x2a]);
            let get = code.len();
            code.extend_from_slice(&[0xb4, 0x00, 0x01, 0xac]);
            let len = code.len();
            code.extend_from_slice(&[0, 0, 0]);
            let mut b = IrBuilder::new(2, 2);
            b.set_field_info(HashMap::from([(put, (0usize, tag)), (get, (0usize, tag))]));
            let g = b.build(&code, len).expect("IR build");
            let store = g
                .nodes
                .iter()
                .find(|n| matches!(n.op, Op::Store(_)))
                .expect("a Store");
            let value = store.inputs[4];
            (g, value)
        };
        let sipush_300 = [0x11, 0x01, 0x2c];

        // Is `v` the `(x << k) >> k` shape the `i2b`/`i2s` arms build, over `x`?
        let sext = |g: &Graph, v: NodeId, k: i64, x: &Op| {
            let shr = &g.nodes[v as usize];
            let shl = &g.nodes[shr.inputs[0] as usize];
            shr.op == Op::Shr
                && shl.op == Op::Shl
                && g.nodes[shr.inputs[1] as usize].op == Op::Const(k)
                && g.nodes[shl.inputs[1] as usize].op == Op::Const(k)
                && g.nodes[shl.inputs[0] as usize].op == *x
        };
        // Is `v` `x & mask`?
        let masked = |g: &Graph, v: NodeId, mask: i64| {
            let and = &g.nodes[v as usize];
            and.op == Op::And && g.nodes[and.inputs[1] as usize].op == Op::Const(mask)
        };

        // `sipush 300; putfield B` -> (300 << 24) >> 24: 300 reads back as 44.
        let (g, v) = stored(&sipush_300, b'B');
        assert!(
            sext(&g, v, 24, &Op::Const(300)),
            "{:?}",
            g.nodes[v as usize].op
        );
        // ...S keeps 300 (in range), C keeps 300, Z takes bit 0.
        let (g, v) = stored(&sipush_300, b'S');
        assert_eq!(g.nodes[v as usize].op, Op::Const(300), "300 fits a short");
        let (g, v) = stored(&sipush_300, b'C');
        assert_eq!(g.nodes[v as usize].op, Op::Const(300), "300 fits a char");
        let (g, v) = stored(&sipush_300, b'Z');
        assert!(masked(&g, v, 1));

        // An unknown int (`iload_1`) is narrowed for every sub-int tag...
        let p1 = Op::Param(1);
        let (g, v) = stored(&[0x1b], b'B');
        assert!(sext(&g, v, 24, &p1));
        let (g, v) = stored(&[0x1b], b'S');
        assert!(sext(&g, v, 16, &p1));
        let (g, v) = stored(&[0x1b], b'C');
        assert!(masked(&g, v, 0xFFFF));
        let (g, v) = stored(&[0x1b], b'Z');
        assert!(masked(&g, v, 1));
        // ...and not for `int`.
        let (g, v) = stored(&[0x1b], b'I');
        assert_eq!(g.nodes[v as usize].op, Op::Param(1));

        // javac's shape, `iload_1; i2b; putfield B`, is not narrowed twice:
        // the stored value is the `i2b` arm's own node, over the parameter.
        let (g, v) = stored(&[0x1b, 0x91], b'B');
        assert!(sext(&g, v, 24, &p1));
        // `iload_1; i2c; putfield C` likewise.
        let (g, v) = stored(&[0x1b, 0x92], b'C');
        assert!(masked(&g, v, 0xFFFF));
        assert_eq!(g.nodes[g.nodes[v as usize].inputs[0] as usize].op, p1);
    }

    /// cov-06, first increment: `newarray` (0xbc) needs no resolver
    /// plumbing at all — `iconst_1; newarray int; areturn` must build
    /// straight away, with the atype immediate carried verbatim and the
    /// runtime length wired as the third input.
    #[test]
    fn test_ir_newarray_emits_new_array_node() {
        let code = [0x04, 0xbc, 0x0a, 0xb0, 0, 0]; // iconst_1; newarray T_INT; areturn
        let builder = IrBuilder::new(0, 0);
        let graph = builder.build(&code, 4).expect("IR build failed");
        let arr = graph
            .nodes
            .iter()
            .find(|n| matches!(n.op, Op::NewArray { .. }))
            .expect("newarray should emit an Op::NewArray node");
        assert_eq!(
            arr.op,
            Op::NewArray {
                element_type: 10,
                component_class_id: 0,
            }
        );
        assert_eq!(arr.inputs.len(), 3, "NewArray inputs = [ctrl, mem, length]");
        assert_eq!(graph.nodes[arr.inputs[2] as usize].op, Op::Const(1));
    }

    /// cov-06, second increment: `anewarray` (0xbd) against an
    /// already-loaded component class lowers the same way, keyed by pc
    /// through `set_anewarray_info` exactly like `set_new_info` resolves
    /// `new`.
    #[test]
    fn test_ir_anewarray_resolved_emits_new_array_node() {
        let code = [0x04, 0xbd, 0x00, 0x01, 0xb0, 0, 0]; // iconst_1; anewarray #1; areturn
        let mut builder = IrBuilder::new(0, 0);
        let mut info = HashMap::new();
        info.insert(1usize, 42u32); // anewarray opcode is at pc 1
        builder.set_anewarray_info(info);
        let graph = builder.build(&code, 5).expect("IR build failed");
        let arr = graph
            .nodes
            .iter()
            .find(|n| matches!(n.op, Op::NewArray { .. }))
            .expect("anewarray should emit an Op::NewArray node");
        assert_eq!(
            arr.op,
            Op::NewArray {
                element_type: 0,
                component_class_id: 42,
            }
        );
    }

    /// cov-06: an `anewarray` whose component class is NOT in
    /// `anewarray_info` (the deferred / not-yet-loaded case) must bail the
    /// build, not guess a class id — `ir_compatible` no longer refuses
    /// every `anewarray`-bearing method outright, so this per-site gate is
    /// the only thing standing between a deferred site and wrong codegen.
    #[test]
    fn test_ir_anewarray_unresolved_bails() {
        let code = [0x04, 0xbd, 0x00, 0x01, 0xb0, 0, 0];
        let builder = IrBuilder::new(0, 0);
        assert!(
            builder.build(&code, 5).is_none(),
            "an anewarray pc absent from anewarray_info must bail to single-pass"
        );
    }

    /// `static int f(int flag, int n, int d) { if (flag != 0) return 0; return n / d; }`
    ///
    /// The `idiv` is reachable only on the not-taken arm, so its
    /// ArithmeticException must be anchored there — `Op::Guard`'s control input
    /// has to be the `If`'s projection, not the method entry. `Op::Div` itself
    /// is a floating node the scheduler is free to hoist into any block its
    /// operands are available in, which is exactly why the trap may not ride
    /// along with it.
    #[test]
    fn idiv_zero_divisor_trap_is_anchored_to_the_branch_that_reaches_it() {
        // 0: iload_0  1: ifne +7 (→8)  4: iload_1  5: iload_2  6: idiv
        // 7: ireturn  8: iconst_0  9: ireturn
        let code = [
            0x1a, 0x9a, 0x00, 0x07, 0x1b, 0x1c, 0x6c, 0xac, 0x03, 0xac, 0, 0,
        ];
        let graph = IrBuilder::new(3, 3)
            .build(&code, 10)
            .expect("IR build failed");

        let guards: Vec<&Node> = graph
            .nodes
            .iter()
            .filter(|n| matches!(n.op, Op::Guard { bci: 6 }))
            .collect();
        assert_eq!(
            guards.len(),
            1,
            "exactly one zero-divisor guard for the single idiv"
        );
        let guard = guards[0];
        assert_eq!(guard.ty, IrType::Void);
        assert_eq!(guard.inputs.len(), 2, "Op::Guard is [ctrl, cond]");

        // Anchored inside the conditional: the control edge is the token the
        // not-taken arm runs under (the builder gives the fall-through target
        // its own `Merge`), never the method's entry control — which is the
        // whole point, since the entry dominates both arms and a trap parked
        // there fires on the path that returns 0 without dividing.
        let ctrl_id = guard.inputs[0];
        let ctrl = &graph.nodes[ctrl_id as usize];
        assert_eq!(ctrl.ty, IrType::Control, "guard input 0 is a control edge");
        let entry_ctrl = graph
            .nodes
            .iter()
            .position(|n| {
                n.op == Op::Proj(0)
                    && n.ty == IrType::Control
                    && n.input_opt(0) == Some(graph.entry)
            })
            .expect("the graph has an entry control projection");
        assert_ne!(
            ctrl_id as usize, entry_ctrl,
            "the trap must not be anchored at the method entry"
        );
        assert!(
            graph.nodes.iter().any(|n| n.op == Op::If),
            "the fixture really does branch"
        );

        // The condition is `divisor != 0`, over the same node the division
        // divides by.
        let cond = &graph.nodes[guard.inputs[1] as usize];
        assert_eq!(cond.op, Op::Cmp(CmpOp::Ne));
        let div = graph
            .nodes
            .iter()
            .find(|n| n.op == Op::Div)
            .expect("the idiv builds an Op::Div");
        assert_eq!(cond.inputs[0], div.inputs[1], "guards the actual divisor");
        assert_eq!(graph.nodes[cond.inputs[1] as usize].op, Op::Const(0));
    }

    /// The same anchoring for `ldiv`, `irem` and `lrem`, and the `Long` form
    /// compares against a `Long`-typed zero (a 32-bit compare would read only
    /// half of a 64-bit divisor).
    #[test]
    fn every_integer_division_opcode_anchors_its_zero_divisor_trap() {
        for (opcode, div_pc, is_long, op) in [
            // 0: lload_0  1: lload_2  2: ldiv  3: lreturn
            (0x6du8, 2usize, true, Op::Div),
            // irem / lrem, same shape
            (0x70u8, 2usize, false, Op::Rem),
            (0x71u8, 2usize, true, Op::Rem),
        ] {
            let code = if is_long {
                [0x1e, 0x20, opcode, 0xad, 0, 0]
            } else {
                [0x1a, 0x1b, opcode, 0xac, 0, 0]
            };
            // Typed parameters: `IrBuilder::new` alone types every parameter
            // `Int`, so `lload` would feed an Int into `ldiv`, and the operand
            // type lane (`CRATONVM_JIT_VERIFY_TYPES=1`) rightly rejects that.
            let mut builder = IrBuilder::new(2, 4);
            let param = if is_long { IrType::Long } else { IrType::Int };
            builder.set_param_types(&[param, param]);
            let Some(graph) = builder.build(&code, 4) else {
                continue; // a shape this front end declines is not this test's business
            };
            let guard = graph
                .nodes
                .iter()
                .find(|n| matches!(n.op, Op::Guard { bci } if bci == div_pc))
                .unwrap_or_else(|| panic!("no zero-divisor guard for opcode {opcode:#x}"));
            let cond = &graph.nodes[guard.inputs[1] as usize];
            assert_eq!(cond.op, Op::Cmp(CmpOp::Ne), "opcode {opcode:#x}");
            let zero = &graph.nodes[cond.inputs[1] as usize];
            assert_eq!(zero.op, Op::Const(0), "opcode {opcode:#x}");
            assert_eq!(
                zero.ty,
                if is_long { IrType::Long } else { IrType::Int },
                "opcode {opcode:#x}: the zero must be as wide as the divisor"
            );
            assert!(
                graph.nodes.iter().any(|n| n.op == op),
                "opcode {opcode:#x} still builds its arithmetic node"
            );
        }
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
    fn test_ir_getfield_builds_a_ref_typed_load_for_a_reference_field() {
        // A reference field (`L…;`) reads through the same checked helper and
        // yields a `Ref`-typed `Op::Load(MemKind::Ref)`. The TYPE is the part
        // that matters beyond "it builds": `emit_safepoint_map` publishes
        // exactly the `Ref`-typed nodes, so an `Int`-typed load of a reference
        // would leave a live oop out of every map.
        //
        // This used to assert the opposite — that the builder bails — which
        // was the single largest exclusion in the optimizing tier (66 of 141
        // builder refusals on a Hibernate class).
        let code = [0x2a, 0xb4, 0x00, 0x02, 0xb0, 0, 0]; // aload_0; getfield; areturn
        let mut builder = IrBuilder::new(1, 1);
        let mut fi = HashMap::new();
        fi.insert(1usize, (0usize, b'L'));
        builder.set_field_info(fi);
        let graph = builder
            .build(&code, 5)
            .expect("a reference-field getfield must build");
        let load = graph
            .nodes
            .iter()
            .find(|n| matches!(n.op, Op::Load(MemKind::Ref)))
            .expect("the reference field must lower to Op::Load(MemKind::Ref)");
        assert_eq!(load.ty, IrType::Ref);
    }

    #[test]
    fn test_ir_getfield_still_bails_on_a_wide_field_with_the_gates_off() {
        // `J`/`D`/`F` fields build only when the long / FP tier is on — the
        // gate `lib.rs` passes down from the same `ir_emit_long` /
        // `ir_emit_fp` its admission chain evaluates. With both off (the
        // builder default, and what a hand-built graph gets) they are still
        // refused HERE rather than deeper in the pipeline.
        for tag in *b"JDF" {
            let code = [0x2a, 0xb4, 0x00, 0x02, 0xac, 0, 0];
            let mut builder = IrBuilder::new(1, 1);
            let mut fi = HashMap::new();
            fi.insert(1usize, (0usize, tag));
            builder.set_field_info(fi);
            assert!(
                builder.build(&code, 5).is_none(),
                "field tag {} must not build with the wide-field gates off",
                tag as char
            );
        }
    }

    /// COV-03. `getfield` learned about reference fields; `putfield` twenty
    /// lines below it did not — 37 refusals on `ir.rs:5085`, the second-largest
    /// structural refusal in the coverage survey, for a fix that had simply
    /// been applied to one of two sites.
    ///
    /// The node TYPE is the load-bearing assertion, exactly as it is for the
    /// read: `MemKind::Ref` is what selects `jit_putfield_object` in
    /// `ir_lower`, and that helper is what carries the SATB pre-barrier and the
    /// collector's post-write barrier. An `Int`-kinded reference store would
    /// lower to a raw 16-byte int-cell write with no barrier at all.
    #[test]
    fn test_ir_putfield_builds_a_ref_kinded_store_for_a_reference_field() {
        for tag in *b"L[" {
            // aload_0; aload_1; putfield #2; return
            let code = [0x2a, 0x2b, 0xb5, 0x00, 0x02, 0xb1, 0, 0];
            let mut builder = IrBuilder::new(2, 2);
            let mut fi = HashMap::new();
            fi.insert(2usize, (0usize, tag));
            builder.set_field_info(fi);
            let graph = builder
                .build(&code, 6)
                .unwrap_or_else(|| panic!("a {} field putfield must build", tag as char));
            assert!(
                graph
                    .nodes
                    .iter()
                    .any(|n| matches!(n.op, Op::Store(MemKind::Ref))),
                "a {} field must lower to Op::Store(MemKind::Ref), not Int",
                tag as char
            );
        }
    }

    /// The two arms must accept the SAME tag set. This is the regression the
    /// lane exists for: a tag one arm admits and the other refuses is how the
    /// asymmetry arose, and it is invisible in any test that exercises one arm.
    #[test]
    fn test_ir_getfield_and_putfield_admit_the_same_field_tags() {
        for &(long_gate, fp_gate) in &[(false, false), (true, false), (false, true), (true, true)] {
            for tag in *b"IZBCSL[JFDV" {
                // aload_0; getfield #2; return — the read side only needs to
                // build; leaving the value on the abstract stack at the return
                // is fine for a straight-line translation.
                let get = [0x2a, 0xb4, 0x00, 0x02, 0xb1, 0, 0];
                let mut gb = IrBuilder::new(1, 1);
                let mut gfi = HashMap::new();
                gfi.insert(1usize, (0usize, tag));
                gb.set_field_info(gfi);
                gb.set_wide_field_gates(long_gate, fp_gate);
                let get_builds = gb.build(&get, 5).is_some();

                // aload_0; aload_1; putfield #2; return
                let put = [0x2a, 0x2b, 0xb5, 0x00, 0x02, 0xb1, 0, 0];
                let mut pb = IrBuilder::new(2, 2);
                let mut pfi = HashMap::new();
                pfi.insert(2usize, (0usize, tag));
                pb.set_field_info(pfi);
                pb.set_wide_field_gates(long_gate, fp_gate);
                let put_builds = pb.build(&put, 6).is_some();

                assert_eq!(
                    get_builds, put_builds,
                    "tag {} (long_gate={long_gate} fp_gate={fp_gate}): getfield builds={get_builds} \
                     but putfield builds={put_builds} — the two arms have drifted apart again",
                    tag as char
                );
            }
        }
    }

    /// With the matching gate on, a wide field builds a wide-KINDED access on
    /// both arms. The kind is what picks `jit_putfield_{long,float,double}` and
    /// what tells the lowerer a `J`/`D` read needs the `dispatch_threw`
    /// disambiguation (`Long.MIN_VALUE` and `-0.0` are bit-identical to the
    /// helper's deopt sentinel).
    #[test]
    fn test_ir_wide_fields_build_wide_kinded_accesses_when_gated() {
        for (tag, kind, ty, load_op, ret) in [
            (b'J', MemKind::Long, IrType::Long, 0x16u8, 0xadu8),
            (b'F', MemKind::Float, IrType::Float, 0x17, 0xae),
            (b'D', MemKind::Double, IrType::Double, 0x18, 0xaf),
        ] {
            // aload_0; getfield #2; <x>return
            let get = [0x2a, 0xb4, 0x00, 0x02, ret, 0, 0];
            let mut gb = IrBuilder::new(1, 1);
            let mut gfi = HashMap::new();
            gfi.insert(1usize, (0usize, tag));
            gb.set_field_info(gfi);
            gb.set_wide_field_gates(true, true);
            let graph = gb
                .build(&get, 5)
                .unwrap_or_else(|| panic!("a gated {} field getfield must build", tag as char));
            let load = graph
                .nodes
                .iter()
                .find(|n| matches!(n.op, Op::Load(k) if k == kind))
                .unwrap_or_else(|| panic!("{} must lower to Op::Load({kind:?})", tag as char));
            assert_eq!(load.ty, ty, "{} load node type", tag as char);

            // aload_0; <x>load_1; putfield #2; return — `this` at slot 0, the
            // wide value at slot 1 (occupying slots 1-2 for `J`/`D`).
            let put = [0x2a, load_op, 0x01, 0xb5, 0x00, 0x02, 0xb1, 0, 0];
            let mut pb = IrBuilder::new(2, 4);
            pb.set_param_types(&[IrType::Ref, ty]);
            let mut pfi = HashMap::new();
            pfi.insert(3usize, (0usize, tag));
            pb.set_field_info(pfi);
            pb.set_wide_field_gates(true, true);
            let graph = pb
                .build(&put, 7)
                .unwrap_or_else(|| panic!("a gated {} field putfield must build", tag as char));
            let store = graph
                .nodes
                .iter()
                .find(|n| matches!(n.op, Op::Store(k) if k == kind))
                .unwrap_or_else(|| panic!("{} must lower to Op::Store({kind:?})", tag as char));
            // The VALUE operand must carry the wide type too — the lowerer
            // reads its frame slot as raw bits and hands them to the width's
            // helper, and an `Int`-typed producer would mean the slot was never
            // written as one.
            assert_eq!(
                graph.nodes[store.inputs[4] as usize].ty, ty,
                "{} store value operand type",
                tag as char
            );
        }
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

    /// The snapshots inside a `synchronized` block carry the locked reference
    /// on their monitor stack, and the ones outside it carry none.
    ///
    /// Before 2026-09-12 the builder kept no monitor stack, so every lowered
    /// frame state said "holds no lock".
    #[test]
    fn a_synchronized_blocks_snapshots_carry_its_monitor() {
        // javac shape for `synchronized (o) { }` with `o` in local 0:
        //   0: aload_0  1: dup  2: astore_1  3: monitorenter
        //   4: aload_1  5: monitorexit  6: return
        let code = [0x2a, 0x59, 0x4c, 0xc2, 0x2b, 0xc3, 0xb1, 0, 0];
        let graph = build_ir(&code, 7, 1, 2);
        let at = |bci: usize| graph.safepoints.iter().find(|s| s.bci == bci).unwrap();
        let obj = at(0).locals[0];
        assert_ne!(obj, NO_NODE);

        // Up to and including the monitorenter itself (resuming there
        // re-executes the enter), nothing is held.
        for bci in [0, 1, 2, 3] {
            assert!(at(bci).monitors.is_empty(), "bci {bci} holds nothing yet");
        }
        // Between the enter and the exit — the exit included, since resuming
        // there re-executes it — the lock on `o` is held.
        for bci in [4, 5] {
            assert_eq!(at(bci).monitors, vec![obj], "bci {bci} holds the lock on o");
        }
        // After the exit, released.
        assert!(at(6).monitors.is_empty());
    }

    /// A loop inside the block gives local 1 an eager header φ, so the
    /// `monitorexit`'s operand is `phi(o, o)` while the monitor stack recorded
    /// `o`. Those name the same reference and the build must accept it; the
    /// header snapshot carries the monitor too.
    #[test]
    fn a_monitor_held_across_a_loop_is_matched_through_the_header_phi() {
        //  0: aload_0  1: dup  2: astore_1  3: monitorenter
        //  4: iload_2 (loop header)  5: ifeq +6 -> 11
        //  8: goto -4 -> 4
        // 11: aload_1  12: monitorexit  13: return
        let code = [
            0x2a, 0x59, 0x4c, 0xc2, 0x1c, 0x99, 0x00, 0x06, 0xa7, 0xff, 0xfc, 0x2b, 0xc3, 0xb1, 0,
            0,
        ];
        let graph = IrBuilder::new(3, 3)
            .build(&code, 14)
            .expect("a structured lock around a loop must build");
        let at = |bci: usize| graph.safepoints.iter().find(|s| s.bci == bci).unwrap();
        let obj = at(0).locals[0];
        assert_eq!(at(4).monitors, vec![obj], "the loop header holds the lock");
        assert_eq!(at(12).monitors, vec![obj], "the monitorexit holds the lock");
        assert!(at(13).monitors.is_empty());
    }

    /// Unstructured locking is refused rather than described wrongly: a
    /// `monitorexit` on a reference other than the innermost held one, a
    /// `monitorexit` with nothing held, and a `return` that leaves a monitor
    /// held. The single-pass backend still compiles such a method.
    #[test]
    fn unstructured_locking_refuses_the_ir_build() {
        // aload_0; monitorenter; aload_1; monitorexit; return — exits `b`, holds `a`.
        let wrong_object = [0x2a, 0xc2, 0x2b, 0xc3, 0xb1, 0, 0];
        assert!(IrBuilder::new(2, 2).build(&wrong_object, 5).is_none());
        // aload_0; monitorexit; return — nothing held.
        let nothing_held = [0x2a, 0xc3, 0xb1, 0, 0];
        assert!(IrBuilder::new(1, 1).build(&nothing_held, 3).is_none());
        // aload_0; monitorenter; return — still held at the method exit.
        let held_at_return = [0x2a, 0xc2, 0xb1, 0, 0];
        assert!(IrBuilder::new(1, 1).build(&held_at_return, 3).is_none());
        // The structured control still builds.
        let structured = [0x2a, 0xc2, 0x2a, 0xc3, 0xb1, 0, 0];
        assert!(IrBuilder::new(1, 1).build(&structured, 5).is_some());
    }

    /// Two edges into one join that hold different monitors have no single
    /// monitor state to record at the join: refused.
    #[test]
    fn a_join_whose_edges_hold_different_monitors_refuses_the_build() {
        //  0: iload_1  1: ifeq +8 -> 9
        //  4: aload_0  5: monitorenter  6: goto +3 -> 9     (holds `a` on this edge)
        //  9: return                                        (join: held vs not held)
        let code = [
            0x1b, 0x99, 0x00, 0x08, 0x2a, 0xc2, 0xa7, 0x00, 0x03, 0xb1, 0, 0,
        ];
        assert!(IrBuilder::new(2, 2).build(&code, 10).is_none());
        // The control: the same join with the lock released on that edge first
        // (`aload_0; monitorexit` before the goto) builds, which is what shows the
        // refusal above is the monitor disagreement and not the bytecode shape.
        //  0: iload_1  1: ifeq +10 -> 11
        //  4: aload_0  5: monitorenter  6: aload_0  7: monitorexit
        //  8: goto +3 -> 11   11: return
        let balanced = [
            0x1b, 0x99, 0x00, 0x0a, 0x2a, 0xc2, 0x2a, 0xc3, 0xa7, 0x00, 0x03, 0xb1, 0, 0,
        ];
        assert!(IrBuilder::new(2, 2).build(&balanced, 12).is_some());
    }

    #[test]
    fn test_replace_all_uses_rewrites_safepoints() {
        let mut graph = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: UseLists::new(),
            receiver_param: None,
        };
        let a = graph.add(Op::Const(1), IrType::Int, vec![], None);
        let b = graph.add(Op::Const(2), IrType::Int, vec![], None);
        graph.safepoints.push(SafepointSnapshot {
            bci: 0,
            locals: vec![a],
            stack: vec![a, b],
            monitors: Vec::new(),
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
            checkcast_ops: vec![],
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
            has_putstatic: false,
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
            checkcast_ops: vec![],
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
            has_putstatic: false,
            ldc_ops: vec![],
        };
        assert!(ir_compatible(&scan));
    }

    /// STUB-S7: confirm caps still reject over-budget methods so we don't
    /// route huge heap-heavy methods through an under-tested IR pipeline.
    /// The admission gate and the builder agree about array allocation.
    ///
    /// # What this pins, and the comment it replaces
    ///
    /// `ir_compatible`'s "surviving exclusions" block asserted until
    /// 2026-09-16 that `newarray` (0xbc) and `anewarray` (0xbd) "have no arm
    /// in `IrBuilder::build`'s opcode match and `Op::NewArray` is constructed
    /// nowhere". cov-06 had added both arms; the comment had simply not been
    /// updated, and the `multianewarray` bullet immediately below it said so
    /// in passing. A reader deciding whether to widen the gate would have read
    /// the false half first.
    ///
    /// The rule that comment existed to teach is real, so it is a test now:
    /// **a shape the gate ADMITS must be one the builder can BUILD**, because
    /// the failure mode of disagreeing is silent — the method is admitted,
    /// the graph build bails, and the optimizing tier produces no body while
    /// every gate upstream reports "open".
    ///
    /// Behavioural rather than source-scanning on purpose: the question is
    /// "does a method of this shape survive the pipeline", and an opcode-match
    /// parser answers a proxy for it that can itself go stale.
    #[test]
    fn ir_admission_and_builder_agree_about_array_allocation() {
        use super::super::x64::jit_scan;

        // `static int[] f(int n) { return new int[n]; }` — the primitive
        // array shape, which needs no side map at all.
        let newarray: [u8; 4] = [
            0x1a, // iload_0        n
            0xbc, 0x0a, // newarray T_INT
            0xb0, // areturn
        ];
        let scan = jit_scan(&newarray, newarray.len(), "(I)[I")
            .expect("jit_scan accepts a primitive array allocation");
        assert!(
            scan.has_newarray,
            "the scan must see the newarray, or this test proves nothing"
        );
        assert!(
            ir_compatible(&scan),
            "`newarray` is NOT a surviving exclusion — the gate admits it"
        );
        let built = IrBuilder::new(1, 1).build(&newarray, newarray.len());
        assert!(
            built.is_some(),
            "the gate admitted a `newarray` method the builder refused —              admission and the builder have drifted apart, which produces a              tier that compiles nothing while every gate reports open"
        );
        let graph = built.expect("checked immediately above");
        assert!(
            graph.nodes.iter().any(|n| matches!(
                n.op,
                Op::NewArray {
                    element_type: 0x0a,
                    ..
                }
            )),
            "`Op::NewArray` IS constructed — the old comment said it was not"
        );

        // `multianewarray` is the exclusion that really does survive: no arm
        // in the builder, so the gate must refuse it BEFORE a graph build.
        let multi: [u8; 7] = [
            0x04, // iconst_1
            0x04, // iconst_1
            0xc5, 0x00, 0x01, 0x02, // multianewarray #1, dims=2
            0xb0, // areturn
        ];
        if let Some(scan) = jit_scan(&multi, multi.len(), "()[[I") {
            assert!(
                !scan.multianewarray_ops.is_empty(),
                "the scan must see the multianewarray, or this test proves nothing"
            );
            assert!(
                !ir_compatible(&scan),
                "`multianewarray` has no builder arm, so the gate must refuse                  it here rather than after a full graph build"
            );
        }
    }

    /// The `anewarray` budget is a BUDGET, not a refusal.
    ///
    /// The other half of the correction above: the comment claimed the
    /// `anewarray_ops` budget "used to be" one and had been replaced by an
    /// outright refusal. It had not; the check is still
    /// `> IR_MAX_ARRAY_ALLOCATIONS`, and a method under it is admitted.
    #[test]
    fn anewarray_is_bounded_by_a_budget_and_not_refused_outright() {
        let mut scan = super::super::x64::JitScanResult {
            needs_heap: true,
            multianewarray_ops: vec![],
            field_ops: vec![],
            typecheck_ops: vec![],
            checkcast_ops: vec![],
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
            has_putstatic: false,
            ldc_ops: vec![],
        };
        scan.anewarray_ops = (0..IR_MAX_ARRAY_ALLOCATIONS)
            .map(|i| (i, i as u16))
            .collect();
        assert!(
            ir_compatible(&scan),
            "exactly IR_MAX_ARRAY_ALLOCATIONS reference-array sites are admitted"
        );
        scan.anewarray_ops = (0..IR_MAX_ARRAY_ALLOCATIONS + 1)
            .map(|i| (i, i as u16))
            .collect();
        assert!(
            !ir_compatible(&scan),
            "one site past the budget is refused — it is a budget with an edge,              not a blanket exclusion"
        );
    }

    #[test]
    fn test_ir_compatible_rejects_over_cap() {
        let mut scan = super::super::x64::JitScanResult {
            needs_heap: true,
            multianewarray_ops: vec![],
            field_ops: vec![],
            typecheck_ops: vec![],
            checkcast_ops: vec![],
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
            has_putstatic: false,
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
        assert!(
            !ir_compatible(&scan),
            "one invoke past the budget is rejected"
        );
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
        scan.has_putstatic = true;
        assert!(
            !ir_compatible(&scan),
            "putstatic has no IR lowering; refuse it up front"
        );
        scan.has_putstatic = false;

        // Object allocations stay the most conservative budget: a surviving
        // `Op::New` lowers through the shared stub, so the cap bounds real
        // codegen and not only what escape analysis may try to eliminate.
        scan.new_ops = (0..IR_MAX_ALLOCATIONS).map(|i| (i, i as u16)).collect();
        assert!(ir_compatible(&scan));
        scan.new_ops = (0..IR_MAX_ALLOCATIONS + 1).map(|i| (i, i as u16)).collect();
        assert!(!ir_compatible(&scan));
        scan.new_ops.clear();
        // cov-06: `anewarray` is a budget now, same shape as `new_ops` above
        // — `IrBuilder::build` has a real 0xbd arm, so admission tracks
        // `IR_MAX_ARRAY_ALLOCATIONS` rather than refusing outright.
        scan.anewarray_ops = (0..IR_MAX_ARRAY_ALLOCATIONS)
            .map(|i| (i, i as u16))
            .collect();
        assert!(ir_compatible(&scan));
        scan.anewarray_ops = (0..IR_MAX_ARRAY_ALLOCATIONS + 1)
            .map(|i| (i, i as u16))
            .collect();
        assert!(!ir_compatible(&scan));
        scan.anewarray_ops.clear();

        // ── Surviving exclusions (NOT budgets) ──────────────────────
        // multianewarray still rejected outright — see the comment at its
        // `ir_compatible` check: cov-06 deliberately left it refused.
        scan.multianewarray_ops = vec![(0, 1, 2)];
        assert!(!ir_compatible(&scan));
        scan.multianewarray_ops.clear();
        // cov-05: neither `instanceof` (0xc1) nor `checkcast` (0xc0) refuses
        // the whole method here any more — both are admitted structurally,
        // with the loaded-target-only gate applied per-site by
        // `IrBuilder::build`'s 0xc0/0xc1 arms (fed from `lib.rs`), not by
        // this whole-method predicate.
        scan.typecheck_ops = vec![(0, 1)];
        assert!(
            ir_compatible(&scan),
            "instanceof-only (no checkcast) must be admitted"
        );
        scan.typecheck_ops.clear();
        scan.typecheck_ops = vec![(0, 1)];
        scan.checkcast_ops = vec![(0, 1)];
        assert!(
            ir_compatible(&scan),
            "cov-05: checkcast no longer refuses the whole method — its \
             throw routes through the same generic sentinel-drain protocol \
             Op::ConstClass already uses, independent of athrow's gap"
        );
        scan.typecheck_ops.clear();
        scan.checkcast_ops.clear();
        // cov-07: athrow no longer refuses the whole method — `Op::Throw`
        // gives the IR builder a real lowering, reusing the same generic
        // sentinel-drain protocol `checkcast` above already relies on.
        scan.has_athrow = true;
        assert!(
            ir_compatible(&scan),
            "cov-07: athrow no longer refuses the whole method"
        );
        scan.has_athrow = false;
        // invokedynamic: admitted since 2026-09-06 -- the builder's `0xba` arm
        // traps at the site and compiles the rest. The kill switch restores
        // the refusal, which is what this asserts against rather than a
        // constant.
        scan.indy_ops = vec![(0, 1)];
        assert_eq!(ir_compatible(&scan), ir_site_trap_enabled());
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
            checkcast_ops: vec![],
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
            has_putstatic: false,
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
            checkcast_ops: vec![],
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
            has_putstatic: false,
            ldc_ops: vec![],
        };
        // 2026-09-06: this assertion was inverted, deliberately. The IR
        // builder's `0xba` arm now replaces the site with an uncommon trap and
        // compiles the rest of the method -- the same trade the single-pass
        // backend already makes -- so the ADMISSION gate must let it through.
        //
        // What did not change is that a site with no resolved stack effect
        // still refuses: the gate is the admission, `set_indy_trap_sites` is
        // the proof, and that half is tested next.
        assert_eq!(
            ir_compatible(&scan),
            ir_site_trap_enabled(),
            "with the site-trap gate ON an invokedynamic no longer refuses the              method at the admission gate; with it OFF the old refusal must be              restored exactly"
        );
    }

    /// The admission gate is not the proof. A method admitted with an
    /// `invokedynamic` whose descriptor the resolver could not supply has no
    /// recorded stack effect, and the builder must refuse it rather than guess
    /// how many operand-stack entries the site consumes -- a wrong count
    /// silently corrupts every bytecode after it.
    #[test]
    fn an_indy_with_no_resolved_shape_still_refuses_the_method() {
        // invokedynamic #1, 0, 0 ; return
        let code = [0xba, 0x00, 0x01, 0x00, 0x00, 0xb1, 0, 0];
        let builder = IrBuilder::new(0, 1);
        assert!(
            builder.build(&code, 6).is_none(),
            "no `set_indy_trap_sites` call means no shape for pc 0, and the              builder must bail rather than guess the site's stack effect",
        );
    }

    /// invokedynamic-uncommon-trap fix — regression test for a PC-desync bug
    /// caught during review: `bytecode_analysis::branch_target_map`/`bytecode_analysis::back_edges` (the
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
    fn the_shared_branch_target_map_steps_over_invokedynamic() {
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
        let map = crate::bytecode_analysis::branch_target_map(&code, code.len());
        let targets: Vec<usize> = (0..code.len()).filter(|&p| map[p]).collect();
        assert_eq!(
            targets,
            vec![8],
            "goto's target must resolve to pc=8 (the real ireturn); a PC \
             desync from mis-stepping invokedynamic's operand bytes would \
             either fabricate a bogus target inside the invokedynamic's own \
             operand bytes or miss/mis-locate this one entirely"
        );
    }

    /// Sibling of the above for `bytecode_analysis::back_edges`: a BACKWARD branch to
    /// pc=0 (a `goto` after the invokedynamic, jumping back to it) must be
    /// recognized as a loop header at exactly pc=0 — which only happens if
    /// invokedynamic's 5-byte length is stepped correctly so the `goto` at
    /// pc=5 is decoded from the right bytes.
    #[test]
    fn the_shared_back_edges_step_over_invokedynamic() {
        // pc0: invokedynamic #1 (5 bytes: 0xba 0x00 0x01 0x11 0x00). The 4th
        // operand byte is 0x11 (sipush, a 3-byte op in this walker's table)
        // so a mis-step that treats invokedynamic as 1 byte overshoots past
        // the real pc=5 `goto` instead of coincidentally realigning on it
        // (see the sibling `bytecode_analysis::branch_target_map` test above for the same
        // reasoning) — a genuine regression guard, not a lucky bytestring.
        // pc5: goto -5 (3 bytes: 0xa7 0xff 0xfb) -> target pc0 (backward)
        let code: Vec<u8> = vec![
            0xba, 0x00, 0x01, 0x11, 0x00, // 0: invokedynamic #1
            0xa7, 0xff, 0xfb, // 5: goto -5 -> pc 0
        ];
        let headers: Vec<usize> = crate::bytecode_analysis::back_edges(&code, code.len())
            .into_iter()
            .map(|(header, _)| header)
            .collect();
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

    /// Before STUB-S8, an unsupported opcode reachable only through an
    /// exception edge (here `athrow`, 0xbf, inside a handler body — the
    /// rethrow every `finally` ends with) disqualified the WHOLE method: the
    /// linear walk fell into the handler and hit `_ => return None`. This
    /// test pins the builder's own behaviour, independent of whether 0xbf has
    /// a lowering arm: STUB-S8 skips handler bytecode outright, so the
    /// builder never visits this athrow at all, whether or not cov-07 gave
    /// 0xbf a real arm for the REACHABLE case. (As of cov-07, `athrow` no
    /// longer disqualifies a method one level up in `ir_compatible` either —
    /// but that is a different gate than the one this test exercises.)
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
            uses: UseLists::new(),
            receiver_param: None,
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
        b.retype_phi(phi, 0);
        assert_eq!(
            b.graph.nodes[phi as usize].ty,
            IrType::Ref,
            "the late back-edge input must retype the loop-carried φ"
        );

        // A late input that does NOT join drops the φ to the fallback. Keeping
        // `Ref` published a slot holding an `int` (or a long/double) as a GC
        // root; a conflicting merge is only legal for a dead slot, where the
        // fallback is free.
        let stray = b.graph.add(Op::Const(3), IrType::Int, vec![], None);
        b.graph.nodes[phi as usize].inputs.push(stray);
        b.retype_phi(phi, 0);
        assert_eq!(b.graph.nodes[phi as usize].ty, PHI_TYPE_FALLBACK);
        assert_ne!(b.graph.nodes[phi as usize].ty, IrType::Ref);

        // Memory φs are bookkeeping tokens and are never retyped as values.
        let mem_phi = b
            .graph
            .add(Op::Phi, IrType::Memory, vec![region, back], None);
        b.retype_phi(mem_phi, 0);
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

    // ── Constant interning ───────────────────────────────────────────

    /// `iconst` / `aconst_null` hand back one node per `(value, type)`, and
    /// the index they read gives the answer the arena scan it replaced gave —
    /// the LOWEST-id live match — through every way the arena changes under
    /// it during a build.
    #[test]
    fn constant_interning_returns_one_node_per_constant() {
        let mut b = IrBuilder::new(1, 2);

        let five = b.iconst(5);
        assert_eq!(b.iconst(5), five, "the same constant is one node");
        assert_eq!(b.graph.nodes[five as usize].op, Op::Const(5));
        assert_eq!(b.graph.nodes[five as usize].ty, IrType::Int);

        // The type is part of the identity, both ways round.
        let null = b.aconst_null();
        let zero = b.iconst(0);
        assert_ne!(null, zero, "null is not the int zero");
        assert_eq!(b.aconst_null(), null);
        assert_eq!(b.iconst(0), zero);
        let long_five = b.lconst(5);
        assert_ne!(long_five, five);
        assert_eq!(b.iconst(5), five, "a long 5 is not an int 5");

        // A constant added behind the helpers' back is found, as the scan
        // found it — and of two, the lower id wins, as the scan's first match
        // did.
        let seven_a = b.graph.add(Op::Const(7), IrType::Int, vec![], None);
        let seven_b = b.graph.add(Op::Const(7), IrType::Int, vec![], None);
        assert_eq!(b.iconst(7), seven_a);

        // Killing the interned node: the next call must not return the dead
        // one. With a live duplicate, the duplicate; with none, a new node.
        b.graph.kill(seven_a);
        assert_eq!(
            b.iconst(7),
            seven_b,
            "falls back to the surviving duplicate"
        );
        assert_eq!(b.iconst(7), seven_b, "and the repaired entry sticks");
        b.graph.kill(five);
        let five_again = b.iconst(5);
        assert_ne!(five_again, five, "a dead node is never handed back");
        assert_eq!(b.graph.nodes[five_again as usize].op, Op::Const(5));
        assert_eq!(b.iconst(5), five_again);

        // Every answer agrees with the scan the index replaced.
        let scan = |g: &Graph, v: i64, ty: IrType| {
            g.nodes
                .iter()
                .position(|n| n.op == Op::Const(v) && n.ty == ty)
                .map(|i| i as NodeId)
        };
        for (v, ty) in [
            (5, IrType::Int),
            (0, IrType::Int),
            (7, IrType::Int),
            (0, IrType::Ref),
        ] {
            assert_eq!(
                b.interned_const(v, ty),
                scan(&b.graph, v, ty),
                "({v}, {ty:?})"
            );
        }
        assert_eq!(b.interned_const(123, IrType::Int), None);
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
            monitors: Vec::new(),
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

    // ── Compact edge lists (`Inputs`) ────────────────────────────────

    /// `Inputs` must behave like the `Vec<NodeId>` it replaced, including the
    /// spill from the inline buffer to the heap and back down again.
    #[test]
    fn test_inputs_matches_vec_semantics_including_spill() {
        let mut v: Vec<NodeId> = Vec::new();
        let mut i = Inputs::new();
        assert!(i.is_empty());
        assert_eq!(i.len(), 0);
        assert!(!i.is_spilled());

        // Push past the inline bound; the two stay identical throughout.
        for k in 0..(INLINE_INPUTS as NodeId * 3) {
            v.push(k);
            i.push(k);
            assert_eq!(i.as_slice(), v.as_slice(), "after push {k}");
            assert_eq!(i.len(), v.len());
        }
        assert!(
            i.is_spilled(),
            "{} entries must have spilled past the inline bound of {}",
            i.len(),
            INLINE_INPUTS
        );

        // Slice access, indexing, iteration, `contains`, equality with a Vec.
        assert_eq!(i[2], v[2]);
        assert_eq!(i.first().copied(), Some(0));
        assert_eq!(&i[1..3], &v[1..3]);
        assert!(i.contains(&(INLINE_INPUTS as NodeId)));
        assert_eq!(i.iter().sum::<NodeId>(), v.iter().sum::<NodeId>());
        assert_eq!(i, v);
        assert_eq!(i.to_vec(), v);

        // Mutation through the slice.
        i.as_mut_slice()[0] = 77;
        v[0] = 77;
        assert_eq!(i.as_slice(), v.as_slice());

        while let Some(x) = v.pop() {
            assert_eq!(i.pop(), Some(x));
            assert_eq!(i.as_slice(), v.as_slice());
        }
        assert_eq!(i.pop(), None);

        // The inline boundary itself.
        let mut j = Inputs::new();
        for k in 0..INLINE_INPUTS as NodeId {
            j.push(k);
            assert!(!j.is_spilled(), "{} entries still fit inline", j.len());
        }
        j.push(99);
        assert!(j.is_spilled());
        assert_eq!(j.len(), INLINE_INPUTS + 1);
        j.clear();
        assert!(j.is_empty());

        // Conversions.
        let from_vec = Inputs::from(vec![1u32, 2, 3]);
        assert_eq!(from_vec, vec![1u32, 2, 3]);
        assert_eq!(from_vec.clone().into_vec(), vec![1u32, 2, 3]);
        let collected: Inputs = (0..8u32).collect();
        assert_eq!(collected.len(), 8);
        assert!(collected.is_spilled());
        assert_eq!(format!("{:?}", from_vec), "[1, 2, 3]");
    }

    // ── Incremental def-use edges ────────────────────────────────────

    /// A deterministic LCG — the randomized graphs must be reproducible so a
    /// failure can be replayed.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }

        fn below(&mut self, n: u64) -> u64 {
            if n == 0 {
                0
            } else {
                self.next() % n
            }
        }
    }

    /// A graph of `n` nodes with random edges into earlier nodes, some
    /// `NO_NODE` holes, and a handful of safepoint snapshots.
    fn random_graph(seed: u64, n: usize) -> Graph {
        let mut g = empty_graph();
        let mut rng = Lcg(seed);
        g.add(Op::Start, IrType::Control, vec![], None);
        for i in 1..n {
            let arity = rng.below(7) as usize;
            let mut inputs: Vec<NodeId> = Vec::with_capacity(arity);
            for _ in 0..arity {
                // One slot in eight is an undefined placeholder.
                if rng.below(8) == 0 {
                    inputs.push(NO_NODE);
                } else {
                    inputs.push(rng.below(i as u64) as NodeId);
                }
            }
            let op = if arity == 0 {
                Op::Const(rng.below(64) as i64)
            } else {
                Op::Add
            };
            g.add(op, IrType::Int, inputs, None);
        }
        for s in 0..6usize {
            let mut locals: Vec<NodeId> = Vec::new();
            let mut stack: Vec<NodeId> = Vec::new();
            for _ in 0..4 {
                locals.push(if rng.below(4) == 0 {
                    NO_NODE
                } else {
                    rng.below(n as u64) as NodeId
                });
            }
            for _ in 0..2 {
                stack.push(rng.below(n as u64) as NodeId);
            }
            g.safepoints.push(SafepointSnapshot {
                bci: s,
                locals,
                stack,
                monitors: Vec::new(),
            });
        }
        g
    }

    /// Every node and every snapshot slot, compared field by field.
    fn assert_same_graph(a: &Graph, b: &Graph, what: &str) {
        assert_eq!(a.nodes.len(), b.nodes.len(), "{what}: node count");
        for (id, (x, y)) in a.nodes.iter().zip(b.nodes.iter()).enumerate() {
            assert_eq!(x.op, y.op, "{what}: node {id} op");
            assert_eq!(x.ty, y.ty, "{what}: node {id} type");
            assert_eq!(
                x.inputs.as_slice(),
                y.inputs.as_slice(),
                "{what}: node {id} inputs"
            );
            assert_eq!(x.bytecode_pc, y.bytecode_pc, "{what}: node {id} pc");
        }
        assert_eq!(
            a.safepoints.len(),
            b.safepoints.len(),
            "{what}: safepoint count"
        );
        for (si, (x, y)) in a.safepoints.iter().zip(b.safepoints.iter()).enumerate() {
            assert_eq!(x.bci, y.bci, "{what}: safepoint {si} bci");
            assert_eq!(x.locals, y.locals, "{what}: safepoint {si} locals");
            assert_eq!(x.stack, y.stack, "{what}: safepoint {si} stack");
        }
    }

    /// A chain: node `i` is `Add(i-1, i-2)`, so every node has exactly two
    /// inputs and (in the interior) exactly two users. The shape makes the
    /// operation counts below exact rather than approximate.
    fn chain_graph(n: usize) -> Graph {
        assert!(n >= 4);
        let mut g = empty_graph();
        g.add(Op::Start, IrType::Control, vec![], None);
        g.add(Op::Const(1), IrType::Int, vec![], None);
        g.add(Op::Const(2), IrType::Int, vec![], None);
        for i in 3..n {
            g.add(
                Op::Add,
                IrType::Int,
                vec![(i - 1) as NodeId, (i - 2) as NodeId],
                None,
            );
        }
        g.exit = (n - 1) as NodeId;
        g
    }

    /// Use lists follow `add`, `replace_all_uses` and `kill`, and the counts
    /// they report are the counts the historical scan reported.
    #[test]
    fn test_use_lists_maintained_across_add_replace_kill() {
        let mut g = empty_graph();
        let a = g.add(Op::Const(1), IrType::Int, vec![], None);
        let b = g.add(Op::Const(2), IrType::Int, vec![], None);
        g.rebuild_use_lists();
        assert!(g.use_lists_valid());

        // `add` records the new node's edges.
        let s = g.add(Op::Add, IrType::Int, vec![a, b], None);
        let t = g.add(Op::Add, IrType::Int, vec![a, a], None);
        assert!(g.use_lists_valid(), "add keeps the lists current");
        assert_eq!(g.verify_use_lists(), Ok(()));
        assert_eq!(g.use_count(a), 3, "one edge from s, two from t");
        assert_eq!(g.use_count(b), 1);
        assert_eq!(g.use_counts(), vec![3, 1, 0, 0]);
        let mut users = g.users_of(a);
        users.sort_unstable();
        assert_eq!(users, vec![s, t]);

        // `replace_all_uses` moves every edge, including the doubled one.
        g.replace_all_uses(a, b);
        assert!(g.use_lists_valid());
        assert_eq!(g.verify_use_lists(), Ok(()));
        assert_eq!(g.use_count(a), 0);
        assert_eq!(g.use_count(b), 4);
        assert!(g.users_of(a).is_empty());

        // `kill` drops the dead node's outgoing edges.
        g.kill(t);
        assert!(g.use_lists_valid());
        assert_eq!(g.verify_use_lists(), Ok(()));
        assert_eq!(g.use_count(b), 2);
        assert_eq!(g.users_of(b), vec![s]);
        assert_eq!(g.use_counts(), vec![0, 2, 0, 0]);
    }

    /// Snapshot slots are use edges too: they are recorded, rewritten and
    /// verified alongside input edges.
    #[test]
    fn test_use_lists_track_safepoint_slots() {
        let mut g = empty_graph();
        let a = g.add(Op::Const(1), IrType::Int, vec![], None);
        let b = g.add(Op::Const(2), IrType::Int, vec![], None);
        g.rebuild_use_lists();

        g.push_safepoint(SafepointSnapshot {
            bci: 0,
            locals: vec![a, NO_NODE],
            stack: vec![a, b],
            monitors: Vec::new(),
        });
        assert!(
            g.use_lists_valid(),
            "push_safepoint keeps the lists current"
        );
        assert_eq!(g.verify_use_lists(), Ok(()));
        assert_eq!(g.safepoint_users_of(a).len(), 2);
        assert_eq!(
            g.use_count(a),
            0,
            "snapshot slots are not input edges and never were"
        );

        g.replace_all_uses(a, b);
        assert_eq!(g.safepoints[0].locals[0], b);
        assert_eq!(g.safepoints[0].locals[1], NO_NODE, "a hole stays a hole");
        assert_eq!(g.safepoints[0].stack[0], b);
        assert_eq!(g.safepoints[0].stack[1], b);
        assert_eq!(g.verify_use_lists(), Ok(()));
        assert!(g.safepoint_users_of(a).is_empty());
        assert_eq!(g.safepoint_users_of(b).len(), 3);

        // The tracked slot writer keeps the lists current …
        assert!(g.set_safepoint_slot(0, SafepointSlotKind::Local, 1, a));
        assert!(g.use_lists_valid());
        assert_eq!(g.verify_use_lists(), Ok(()));
        g.replace_all_uses(a, b);
        assert_eq!(g.safepoints[0].locals[1], b);

        // … while a snapshot pushed through the public field demotes the next
        // mutation to the scan, which is exactly the point of the guard.
        g.safepoints.push(SafepointSnapshot {
            bci: 1,
            locals: vec![b],
            stack: vec![],
            monitors: Vec::new(),
        });
        assert!(!g.use_lists_valid());
        g.replace_all_uses(b, a);
        assert_eq!(g.safepoints[1].locals[0], a, "the scan still rewrote it");
        assert!(g.use_lists_valid(), "the scan re-derived the lists");
        assert_eq!(g.verify_use_lists(), Ok(()));
    }

    /// `verify_use_lists` is the safety net for the invariant, so it has to
    /// actually fail on a broken list — in both directions.
    #[test]
    fn test_verify_use_lists_catches_a_corrupted_list() {
        let mut g = chain_graph(12);
        g.rebuild_use_lists();
        assert_eq!(g.verify_use_lists(), Ok(()));

        // A use that the graph does not have.
        g.uses.users[4].push_untracked(11);
        assert!(
            g.verify_use_lists().is_err(),
            "an invented use must be reported"
        );
        g.rebuild_use_lists();
        assert_eq!(g.verify_use_lists(), Ok(()));

        // A use the graph has but the list lost.
        g.uses.users[4].clear_untracked();
        let err = g.verify_use_lists().expect_err("a dropped use is a bug");
        assert!(err.contains("node 4"), "unexpected message: {err}");

        // A snapshot slot the list never learned about.
        g.rebuild_use_lists();
        g.push_safepoint(SafepointSnapshot {
            bci: 0,
            locals: vec![5],
            stack: vec![],
            monitors: Vec::new(),
        });
        assert_eq!(g.verify_use_lists(), Ok(()));
        g.uses.sp_users[5].clear();
        assert!(g.verify_use_lists().is_err(), "a dropped slot is a bug");
    }

    /// An edge written through the public `Node::inputs` field cannot notify
    /// the use lists, so it must demote the next mutation to the full scan
    /// rather than be silently missed.
    #[test]
    fn test_direct_field_writes_demote_to_the_scan() {
        let mut g = chain_graph(20);
        g.rebuild_use_lists();
        assert!(g.use_lists_valid());

        // A brand new edge to node 3, invisible to the lists.
        g.nodes[5].inputs.push(3);
        assert!(!g.use_lists_valid(), "the epoch guard must notice");

        g.replace_all_uses(3, 4);
        assert!(
            !g.nodes.iter().any(|n| n.inputs.contains(&3)),
            "the untracked edge must have been rewritten too"
        );
        assert!(g.use_lists_valid(), "the scan re-derived the lists");
        assert_eq!(g.verify_use_lists(), Ok(()));
        assert_eq!(g.use_counts(), scanned_use_counts(&g));
    }

    /// `use_counts` as it was written before the use lists existed.
    fn scanned_use_counts(g: &Graph) -> Vec<u32> {
        let mut counts = vec![0u32; g.nodes.len()];
        for node in &g.nodes {
            for &inp in node.inputs.as_slice() {
                if (inp as usize) < counts.len() {
                    counts[inp as usize] += 1;
                }
            }
        }
        counts
    }

    /// The incremental rewrite must produce the *same graph* as the historical
    /// full scan on graphs it did not choose, for replacements it did not
    /// choose. `set_use_tracking(false)` is the reference implementation: it is
    /// the pre-existing scan, byte for byte.
    #[test]
    fn test_replace_all_uses_matches_the_full_scan_on_random_graphs() {
        for seed in 0..24u64 {
            let n = 24 + (seed as usize % 40);
            let mut reference = random_graph(seed, n);
            let mut incremental = random_graph(seed, n);
            assert_same_graph(&reference, &incremental, "construction");

            reference.set_use_tracking(false);
            incremental.rebuild_use_lists();

            let mut rng = Lcg(seed ^ 0xa5a5_5a5a);
            for round in 0..16 {
                let old = rng.below(n as u64) as NodeId;
                let new = rng.below(n as u64) as NodeId;

                // The tracked graph goes first, from a known-current state:
                // mutating the *reference* graph bumps the shared edge epoch
                // (its `kill` clears inputs through the public path), which
                // would otherwise demote every incremental step to a scan and
                // quietly stop testing the thing under test.
                incremental.ensure_use_lists();
                assert!(incremental.use_lists_valid());
                incremental.replace_all_uses(old, new);
                assert_eq!(
                    incremental.verify_use_lists(),
                    Ok(()),
                    "seed {seed} round {round}"
                );
                reference.replace_all_uses(old, new);
                assert_same_graph(
                    &reference,
                    &incremental,
                    &format!("seed {seed} round {round} ({old} → {new})"),
                );
                assert_eq!(
                    incremental.use_counts(),
                    reference.use_counts(),
                    "seed {seed} round {round}"
                );

                // Interleave the other mutators so the lists are exercised
                // against a moving graph, not a frozen one.
                if round % 4 == 3 {
                    let victim = rng.below(n as u64) as NodeId;
                    let holder = rng.below(n as u64) as NodeId;
                    let val = rng.below(n as u64) as NodeId;
                    incremental.ensure_use_lists();
                    incremental.kill(victim);
                    incremental.push_input(holder, val);
                    let fresh_i = incremental.add(Op::Add, IrType::Int, vec![old, new], None);
                    assert_eq!(
                        incremental.verify_use_lists(),
                        Ok(()),
                        "seed {seed} round {round} after mutation"
                    );
                    reference.kill(victim);
                    reference.push_input(holder, val);
                    let fresh_r = reference.add(Op::Add, IrType::Int, vec![old, new], None);
                    assert_eq!(fresh_r, fresh_i);
                    assert_same_graph(
                        &reference,
                        &incremental,
                        &format!("seed {seed} round {round} after mutation"),
                    );
                }
            }
        }
    }

    /// The measurement the review asked for, made by construction rather than
    /// by timing: how many node-input slots does one `replace_all_uses` touch?
    ///
    /// The chain graph has `n - 3` two-input nodes, so a full scan examines
    /// `E = 2·(n-3)` slots *per call*, whatever the replacement is. The
    /// incremental walk visits only the users of the replaced node — exactly
    /// two of them, of two inputs each — so it examines 4 slots per call
    /// regardless of `n`. With `R = 8` replacements:
    ///
    /// | nodes  | E      | scan `R·E` | incremental `R·4` | ratio  |
    /// |--------|--------|-----------:|------------------:|-------:|
    /// | 100    | 194    | 1 552      | 32                | 48.5×  |
    /// | 1 000  | 1 994  | 15 952     | 32                | 498×   |
    /// | 5 000  | 9 994  | 79 952     | 32                | 2 498× |
    /// | 20 000 | 39 994 | 319 952    | 32                | 9 998× |
    ///
    /// The incremental cost is independent of graph size; the scan is linear
    /// in it. (Real passes also pay one rebuild per untracked edit — asserted
    /// to be zero here — so the end-to-end win on a mutation-heavy pass is
    /// bounded by how often the pass writes edges through the public field.)
    #[test]
    fn test_replace_all_uses_slot_counts_by_graph_size() {
        // Eight replacement targets, 8 apart, so no target is a user of
        // another and each rewrite is independent of the ones before it.
        const ROUNDS: usize = 8;
        let targets: Vec<NodeId> = (0..ROUNDS).map(|k| (10 + 8 * k) as NodeId).collect();

        for &n in &[100usize, 1_000, 5_000, 20_000] {
            let edges = 2 * (n - 3) as u64;

            // Reference: the historical full scan.
            let mut scan = chain_graph(n);
            scan.set_use_tracking(false);
            scan.reset_use_list_stats();
            for &t in &targets {
                scan.replace_all_uses(t, 1);
            }
            assert_eq!(
                scan.use_list_slots_examined(),
                ROUNDS as u64 * edges,
                "the scan examines every input slot on every call (n = {n})"
            );

            // Incremental: only the users of the replaced node.
            let mut inc = chain_graph(n);
            inc.rebuild_use_lists();
            inc.reset_use_list_stats();
            for &t in &targets {
                inc.replace_all_uses(t, 1);
            }
            assert_eq!(
                inc.use_list_slots_examined(),
                ROUNDS as u64 * 4,
                "two users of two inputs each, per call (n = {n})"
            );
            assert_eq!(
                inc.use_list_rebuilds(),
                0,
                "no rebuild is needed once the lists are current (n = {n})"
            );

            // Same work, same graph.
            assert_same_graph(&scan, &inc, &format!("n = {n}"));
            assert_eq!(inc.verify_use_lists(), Ok(()));
            assert_eq!(inc.use_counts(), scan.use_counts());
        }
    }

    /// A builder-produced graph arrives with its def-use edges already
    /// current: every edge the builder writes goes through a tracked mutator,
    /// so the first optimization rewrite is incremental without a warm-up
    /// scan.
    #[test]
    fn test_builder_graphs_arrive_with_current_use_lists() {
        // iload_0; iload_1; iadd; ireturn
        let code = [0x1a, 0x1b, 0x60, 0xac, 0, 0];
        let graph = build_ir(&code, 4, 2, 2);
        assert!(
            graph.use_lists_valid(),
            "the builder must not leave the lists stale"
        );
        assert_eq!(graph.verify_use_lists(), Ok(()));
        assert_eq!(graph.use_counts(), scanned_use_counts(&graph));
    }

    // ── IR-tier inline FRAME sites (stack traces) ────────────────────
    //
    // The probe these are modelled on is `probes/StackTraceAfterOsr.java`:
    // `probe()` calls `outer(1)`, which calls `mid`, which calls `leaf`. The
    // IR tier splices `outer` and then `mid` into `probe`'s combined buffer,
    // and before 2026-09-08 both contributed no stack frame at all.

    /// `probe` has real code `[0, code_len)`; `outer` is spliced at caller pc
    /// 42 and `mid` is spliced at a caller pc INSIDE `outer`'s body.
    fn probe_shaped_sites() -> HashMap<usize, IrInlineSite> {
        fn site(base: usize, code_len: usize, key: &str, class_id: u32) -> IrInlineSite {
            IrInlineSite {
                base,
                code_len,
                num_args: 1,
                max_locals: 1,
                arg_local_slots: vec![0],
                returns_value: true,
                receiver_is_arg0: false,
                method_key: key.to_string(),
                class_id,
            }
        }
        let mut m = HashMap::new();
        // `outer` spliced at probe's pc 42, body at [100, 107).
        m.insert(42, site(100, 7, "P.outer:(I)I", 7));
        // `mid` spliced at combined pc 101 — one byte into `outer`'s body —
        // with its own body at [200, 207).
        m.insert(101, site(200, 7, "P.mid:(I)I", 7));
        m
    }

    /// Nothing spliced ⇒ no chain anywhere, and the fast path reports empty.
    #[test]
    fn inline_frame_sites_empty_answers_nothing() {
        let sites = IrInlineFrameSites::default();
        assert!(sites.is_empty());
        assert!(sites.chain_at(0).is_empty());
        assert!(sites.chain_at(9999).is_empty());
        assert_eq!(sites.enclosing_bci_at(42), None);
    }

    /// A pc in the COMPILING method's own code is not an inlined level. It is
    /// the physical frame, which reports itself; answering a chain for it would
    /// name a callee that is not on the stack.
    #[test]
    fn inline_frame_sites_refuse_the_compiling_methods_own_code() {
        let sites = IrInlineFrameSites::from_sites(&probe_shaped_sites());
        assert!(!sites.is_empty());
        assert!(sites.chain_at(0).is_empty());
        assert!(sites.chain_at(42).is_empty());
        assert_eq!(sites.enclosing_bci_at(42), None);
    }

    /// One level deep: a pc inside `outer`'s body names `outer` alone, at
    /// `outer`'s OWN bci — never the combined-buffer pc.
    #[test]
    fn inline_frame_sites_one_level() {
        let sites = IrInlineFrameSites::from_sites(&probe_shaped_sites());
        let chain = sites.chain_at(103);
        assert_eq!(chain.len(), 1, "one spliced body encloses pc 103");
        assert_eq!(chain[0].method_key, "P.outer:(I)I");
        assert_eq!(chain[0].bci, 3, "103 - base 100");
        assert_eq!(chain[0].class_id, 7);
        // The compiling method's own bci covering it is the splice's caller pc.
        assert_eq!(sites.enclosing_bci_at(103), Some(42));
    }

    /// Two levels: a pc inside `mid` reports `mid` THEN `outer`, innermost
    /// first, each with a bci in its own code. This is the exact shape that
    /// printed `len=3` instead of `len=5` on `StackTraceAfterOsr`.
    #[test]
    fn inline_frame_sites_nested_chain_is_innermost_first() {
        let sites = IrInlineFrameSites::from_sites(&probe_shaped_sites());
        let chain = sites.chain_at(204);
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].method_key, "P.mid:(I)I");
        assert_eq!(chain[0].bci, 4, "204 - base 200, inside mid");
        assert_eq!(chain[1].method_key, "P.outer:(I)I");
        assert_eq!(
            chain[1].bci, 1,
            "mid's caller pc 101 - outer's base 100: where outer calls mid"
        );
        // Still the OUTERMOST splice's caller pc — an index into probe's own
        // bytecode, which is the only bci a row may legally carry.
        assert_eq!(sites.enclosing_bci_at(204), Some(42));
    }

    /// A pc in the gap between two bodies belongs to neither. Ranges are
    /// half-open, so `base + code_len` is already outside.
    #[test]
    fn inline_frame_sites_ranges_are_half_open() {
        let sites = IrInlineFrameSites::from_sites(&probe_shaped_sites());
        assert!(!sites.chain_at(106).is_empty(), "last byte of outer's body");
        assert!(sites.chain_at(107).is_empty(), "one past it");
        assert!(sites.chain_at(150).is_empty(), "the gap before mid's body");
    }

    /// A malformed table whose site sits inside its OWN body would walk
    /// forever. The chain is given up WHOLE rather than truncated: a truncated
    /// chain reads as complete and names a caller that is not there.
    #[test]
    fn inline_frame_sites_refuse_a_cyclic_table() {
        let mut m = HashMap::new();
        m.insert(
            105, // caller pc inside the body this very site installs
            IrInlineSite {
                base: 100,
                code_len: 20,
                num_args: 0,
                max_locals: 1,
                arg_local_slots: vec![],
                returns_value: false,
                receiver_is_arg0: false,
                method_key: "P.loop:()V".to_string(),
                class_id: 1,
            },
        );
        let sites = IrInlineFrameSites::from_sites(&m);
        assert!(
            sites.chain_at(110).is_empty(),
            "a cycle must yield NO chain, not a truncated one"
        );
        assert_eq!(sites.enclosing_bci_at(110), None);
    }

    // ── Inline scopes ────────────────────────────────────────────────

    /// The empty table — what every compile has today — answers "no scope"
    /// for every snapshot index, including ones past the end.
    #[test]
    fn inline_scope_table_is_empty_by_default() {
        let t = InlineScopeTable::new();
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
        assert_eq!(t.snapshot_scope(0), None);
        assert_eq!(t.snapshot_scope(9_999), None);
        assert_eq!(InlineScopeTable::default().len(), 0);
    }

    /// A depth-3 chain: each scope names the one pushed before it, `chain`
    /// reports it innermost-first, and `depth` counts the scope itself.
    #[test]
    fn inline_scope_chain_is_parent_linked_innermost_first() {
        let mut t = InlineScopeTable::new();
        let outer = t.push_scope("A.a:()V", 10, Some(0), None).unwrap();
        let mid = t.push_scope("B.b:()V", 20, Some(1), Some(outer)).unwrap();
        let inner = t.push_scope("C.c:()V", 30, Some(2), Some(mid)).unwrap();

        assert_eq!(t.len(), 3);
        assert_eq!(t.depth(outer), 1);
        assert_eq!(t.depth(mid), 2);
        assert_eq!(t.depth(inner), 3);
        assert_eq!(t.chain(inner), vec![inner, mid, outer]);
        assert_eq!(t.chain(outer), vec![outer]);
        assert_eq!(t.parent(inner), Some(mid));
        assert_eq!(t.parent(outer), None);

        let s = t.scope(mid).expect("issued handle resolves");
        assert_eq!(s.method_key, "B.b:()V");
        assert_eq!(s.caller_bci, 20);
        assert_eq!(s.caller_snapshot, Some(1));
    }

    /// A parent this table never issued is refused outright — that is the one
    /// way a chain could be made cyclic, and it is rejected at the door.
    #[test]
    fn inline_scope_rejects_a_foreign_parent() {
        let mut t = InlineScopeTable::new();
        assert_eq!(
            t.push_scope("A.a:()V", 1, None, Some(InlineScopeId(7))),
            None
        );
        assert!(t.is_empty());
        // …and an unknown handle reads back as nothing, never as scope 0.
        assert_eq!(t.scope(InlineScopeId(0)), None);
        assert_eq!(t.depth(InlineScopeId(3)), 0);
        assert!(t.chain(InlineScopeId(3)).is_empty());
    }

    /// The depth cap is enforced at push time, so no walk can be long.
    #[test]
    fn inline_scope_depth_is_capped() {
        let mut t = InlineScopeTable::new();
        let mut last = t.push_scope("A.a:()V", 0, None, None).unwrap();
        for i in 1..MAX_INLINE_SCOPE_DEPTH {
            last = t
                .push_scope("A.a:()V", i as u32, None, Some(last))
                .unwrap_or_else(|| panic!("depth {i} is within the cap"));
        }
        assert_eq!(t.depth(last), MAX_INLINE_SCOPE_DEPTH);
        assert_eq!(
            t.push_scope("A.a:()V", 999, None, Some(last)),
            None,
            "one past the cap is refused"
        );
    }

    /// Snapshot bindings are by index, grow on demand, and refuse a handle the
    /// table never issued.
    #[test]
    fn inline_scope_snapshot_binding_is_by_index() {
        let mut t = InlineScopeTable::new();
        let s = t.push_scope("A.a:()V", 4, Some(0), None).unwrap();
        assert!(!t.bind_snapshot(0, InlineScopeId(9)), "foreign handle");
        assert_eq!(t.snapshot_scope(0), None);

        assert!(t.bind_snapshot(3, s));
        assert_eq!(t.snapshot_scope(3), Some(s));
        assert_eq!(t.snapshot_scope(2), None, "holes stay holes");
        assert_eq!(t.snapshot_scope(4), None, "past the end reads as absent");

        assert!(t.bind_snapshot_range(5..8, s));
        for i in 5..8 {
            assert_eq!(t.snapshot_scope(i), Some(s));
        }
        assert_eq!(t.snapshot_scope(8), None);
    }

    // ── Memory model: monitors, and the element type in the alias class ──

    /// The `(ctrl, mem)` projections off a `Start`, the preamble every
    /// memory-op fixture below needs.
    fn start_preamble(g: &mut Graph) -> (NodeId, NodeId) {
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        (ctrl, mem)
    }

    /// A reference of *unknown* provenance. Two of them may be the same object
    /// (`f(x, x)`), so nothing separates two accesses through them except the
    /// facts the alias classes carry.
    fn ref_param(g: &mut Graph, i: u16) -> NodeId {
        g.add(Op::Param(i), IrType::Ref, vec![], None)
    }

    /// A runtime subscript. Two accesses at the *same* node are a must-alias
    /// on the offset, so every pair below reaches the interesting test.
    fn subscript(g: &mut Graph) -> NodeId {
        g.add(Op::Add, IrType::Int, vec![], None)
    }

    /// Both monitor ops are in the single memory-shape table, at the same
    /// token slot as every other memory op, and their third input is the
    /// locked object rather than a token.
    #[test]
    fn monitor_ops_take_their_memory_token_at_the_documented_slot() {
        let mut g = empty_graph();
        let (ctrl, mem) = start_preamble(&mut g);
        let obj = ref_param(&mut g, 0);

        for (op, access) in [
            (Op::MonitorEnter, MemAccess::MonitorEnter),
            (Op::MonitorExit, MemAccess::MonitorExit),
        ] {
            assert_eq!(
                op.memory_shape(),
                Some(OpMemoryShape {
                    token_slot: 1,
                    min_full_arity: 3,
                    access,
                }),
                "{op:?} must be registered in the single memory-shape table",
            );

            let id = g.add(op.clone(), IrType::Memory, vec![ctrl, mem, obj], None);
            let node = g.node_opt(id).expect("just added");
            assert_eq!(memory_token_slot(node), Some(1), "{op:?}");
            assert!(!is_memory_token_slot(node, 0), "{op:?}: slot 0 is control");
            assert!(is_memory_token_slot(node, 1), "{op:?}: slot 1 is the token");
            assert!(
                !is_memory_token_slot(node, 2),
                "{op:?}: slot 2 is the locked object, a value the node reads",
            );

            // Below the documented arity there is no token at all — the same
            // rule the compact hand-built `Store`/`Load` forms live under.
            let short = g.add(op.clone(), IrType::Memory, vec![ctrl, mem], None);
            assert_eq!(
                memory_token_slot(g.node_opt(short).expect("just added")),
                None,
                "{op:?} below its full arity has no token slot",
            );
        }
    }

    /// The ops produce exactly the effect the constructors state: a read *and*
    /// a write of the monitor, a safepoint, and half a JMM fence each.
    #[test]
    fn monitor_ops_carry_the_documented_memory_effect() {
        let mut g = empty_graph();
        let (ctrl, mem) = start_preamble(&mut g);
        let obj = ref_param(&mut g, 0);
        let enter = g.add(Op::MonitorEnter, IrType::Memory, vec![ctrl, mem, obj], None);
        let exit = g.add(
            Op::MonitorExit,
            IrType::Memory,
            vec![ctrl, enter, obj],
            None,
        );

        assert_eq!(g.memory_effect(enter), MemEffect::monitor_enter(obj));
        assert_eq!(g.memory_effect(exit), MemEffect::monitor_exit(obj));

        let e = g.memory_effect(enter);
        assert_eq!(e.reads, AliasClass::Monitor { obj });
        assert_eq!(e.writes, AliasClass::Monitor { obj });
        assert_eq!(e.order, MemOrder::Acquire);
        assert!(e.safepoint, "a monitor is a deopt point");
        assert!(!e.allocates);

        let x = g.memory_effect(exit);
        assert_eq!(x.order, MemOrder::Release);
        assert!(x.safepoint);

        // Neither is control and neither is pure, so no GVN or liveness sweep
        // may treat one as a value it can drop.
        for op in [Op::MonitorEnter, Op::MonitorExit] {
            assert!(!op.is_control(), "{op:?}");
            assert!(!op.is_pure(), "{op:?}");
        }
    }

    /// The roach motel, both walls of it: nothing after a `MonitorEnter` may
    /// move above it, nothing before a `MonitorExit` may move below it — and
    /// an access *outside* the region may still sink into it.
    #[test]
    fn a_monitor_pair_is_a_barrier_in_both_directions() {
        let mut g = empty_graph();
        let (ctrl, mem) = start_preamble(&mut g);
        let obj = ref_param(&mut g, 0);
        let base = ref_param(&mut g, 1);
        let off = g.add(Op::Const(0), IrType::Int, vec![], None);
        let val = g.add(Op::Const(7), IrType::Int, vec![], None);

        let pre = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, base, off],
            None,
        );
        let enter = g.add(Op::MonitorEnter, IrType::Memory, vec![ctrl, mem, obj], None);
        let inner_load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, enter, base, off],
            None,
        );
        let inner_store = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![ctrl, enter, base, off, val],
            None,
        );
        let exit = g.add(
            Op::MonitorExit,
            IrType::Memory,
            vec![ctrl, inner_store, obj],
            None,
        );

        // Wall 1 — the acquire. Note the *reason*: the token edge from `enter`
        // to the two inner accesses is deliberately not a `DataDependence`, so
        // an `Acquire` answer proves the fence is what refused, not the chain.
        assert_eq!(
            g.may_reorder(enter, inner_load),
            Reorder::Blocked(ReorderBlock::Acquire),
        );
        assert_eq!(
            g.may_reorder(enter, inner_store),
            Reorder::Blocked(ReorderBlock::Acquire),
        );

        // Wall 2 — the release.
        assert_eq!(
            g.may_reorder(inner_load, exit),
            Reorder::Blocked(ReorderBlock::Release),
        );
        assert_eq!(
            g.may_reorder(inner_store, exit),
            Reorder::Blocked(ReorderBlock::Release),
        );

        // …and the asymmetry that makes it a roach motel rather than two full
        // fences: a read from before the region may move *into* it. A monitor
        // is not the same storage kind as a field cell, so nothing aliases —
        // and the proof is `DisjointLocations` rather than `ReadOnly` because
        // the enter itself writes the lock word.
        assert_eq!(
            g.may_reorder(pre, enter),
            Reorder::Allowed(ReorderProof::DisjointLocations),
        );
    }

    /// Two monitors on provably distinct objects do not alias — and still do
    /// not commute. This is the claim `AliasClass::Monitor`'s doc makes, and
    /// it is the one a purely alias-based model would get wrong.
    #[test]
    fn monitors_on_distinct_objects_do_not_alias_but_still_do_not_commute() {
        let mut g = empty_graph();
        let (ctrl, mem) = start_preamble(&mut g);
        // `value(_, Ref)` is an `Op::New`, so the two are fresh in-method
        // allocations and provably different objects.
        let a = value(&mut g, IrType::Ref);
        let b = value(&mut g, IrType::Ref);
        assert!(
            !g.refs_may_alias(a, b),
            "two fresh allocations are distinct"
        );
        assert!(!g.may_alias(
            AliasClass::Monitor { obj: a },
            AliasClass::Monitor { obj: b }
        ));

        let enter_a = g.add(Op::MonitorEnter, IrType::Memory, vec![ctrl, mem, a], None);
        let enter_b = g.add(
            Op::MonitorEnter,
            IrType::Memory,
            vec![ctrl, enter_a, b],
            None,
        );
        assert_eq!(
            g.may_reorder(enter_a, enter_b),
            Reorder::Blocked(ReorderBlock::Acquire),
            "disjoint locks still order against each other",
        );
    }

    /// The element type is read off the op and lives in the alias class, so
    /// the model — not a producer — decides that a reference element and a
    /// primitive one are different memory.
    #[test]
    fn may_alias_distinguishes_a_reference_array_element_from_a_primitive_one() {
        let mut g = empty_graph();
        let (ctrl, mem) = start_preamble(&mut g);
        let a = ref_param(&mut g, 0);
        let b = ref_param(&mut g, 1);
        let idx = subscript(&mut g);
        let val = g.add(Op::Const(1), IrType::Int, vec![], None);
        assert!(
            g.refs_may_alias(a, b),
            "two parameters may be the same array — provenance separates nothing here",
        );

        let ref_load = g.add(
            Op::ArrayLoad(MemKind::Ref),
            IrType::Ref,
            vec![ctrl, mem, a, idx],
            None,
        );
        let int_store = g.add(
            Op::ArrayStore(MemKind::Int),
            IrType::Memory,
            vec![ctrl, mem, b, idx, val],
            None,
        );
        let ref_store = g.add(
            Op::ArrayStore(MemKind::Ref),
            IrType::Memory,
            vec![ctrl, mem, b, idx, val],
            None,
        );

        // Unforgeable: the kind comes from the node, and a non-access has none.
        assert_eq!(g.element_kind(ref_load), Some(MemKind::Ref));
        assert_eq!(g.element_kind(int_store), Some(MemKind::Int));
        assert_eq!(g.element_kind(idx), None);

        let ref_read = g.memory_effect(ref_load).reads;
        let int_write = g.memory_effect(int_store).writes;
        let ref_write = g.memory_effect(ref_store).writes;
        assert!(
            matches!(
                ref_read,
                AliasClass::ArrayElem {
                    elem: MemKind::Ref,
                    ..
                }
            ),
            "the class carries the element type: {ref_read:?}",
        );

        assert!(
            !g.may_alias(ref_read, int_write),
            "an int[] cell is never an Object[] cell",
        );
        assert!(
            g.may_alias(ref_read, ref_write),
            "two reference arrays may be the same array",
        );

        // The whole query agrees, which is what the vectorization gate reads.
        assert!(g.may_reorder(int_store, ref_load).is_allowed());
        assert_eq!(
            g.may_reorder(ref_store, ref_load),
            Reorder::Blocked(ReorderBlock::MayAlias),
        );
    }

    /// One array node accessed at two element kinds is an ill-typed graph, and
    /// the model must keep saying "may alias" rather than reading the two type
    /// tags as a proof of disjointness.
    #[test]
    fn one_array_at_two_element_kinds_is_still_a_must_alias() {
        let mut g = empty_graph();
        let (ctrl, mem) = start_preamble(&mut g);
        let a = ref_param(&mut g, 0);
        let idx = subscript(&mut g);
        let load_int = g.add(
            Op::ArrayLoad(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, a, idx],
            None,
        );
        let load_ref = g.add(
            Op::ArrayLoad(MemKind::Ref),
            IrType::Ref,
            vec![ctrl, mem, a, idx],
            None,
        );
        assert!(g.may_alias(
            g.memory_effect(load_int).reads,
            g.memory_effect(load_ref).reads,
        ));
    }

    /// The shape table for every op that had one before the monitors landed is
    /// byte-for-byte what it was, and every op that had none still has none.
    /// The memory-chain lane in `ir_verify` and the three private copies of
    /// `memory_token_slot` all read these numbers.
    #[test]
    fn the_memory_shape_table_is_unchanged_for_every_pre_existing_op() {
        let with_shape: &[(Op, usize, MemAccess)] = &[
            (Op::Load(MemKind::Int), 3, MemAccess::FieldRead),
            (Op::Store(MemKind::Int), 4, MemAccess::FieldWrite),
            (Op::ArrayLoad(MemKind::Int), 4, MemAccess::ArrayRead),
            (Op::ArrayStore(MemKind::Int), 5, MemAccess::ArrayWrite),
            (Op::ArrayLength, 3, MemAccess::LengthRead),
            (
                Op::New {
                    class_id: 1,
                    num_fields: 0,
                },
                2,
                MemAccess::Allocate,
            ),
            (
                Op::NewArray {
                    element_type: 10,
                    component_class_id: 0,
                },
                3,
                MemAccess::Allocate,
            ),
            (Op::Call { info_ptr: 0 }, 2, MemAccess::Opaque),
            // cov-01 — the same classification `Op::Call` has, for the same
            // reason: the helper each lowers to can run arbitrary Java.
            (
                Op::ConstString {
                    holder_class_id: 0,
                    cp_idx: 0,
                    slot_addr: 0,
                },
                2,
                MemAccess::Opaque,
            ),
            (
                Op::ConstClass {
                    holder_class_id: 0,
                    cp_idx: 0,
                    slot_addr: 0,
                },
                2,
                MemAccess::Opaque,
            ),
            (
                Op::LoadStatic {
                    class_id: 0,
                    field_index: 0,
                    type_tag: b'I',
                    is_volatile: false,
                },
                2,
                MemAccess::Opaque,
            ),
            (Op::LambdaIntToDouble, 4, MemAccess::Opaque),
            (Op::MonitorEnter, 3, MemAccess::MonitorEnter),
            (Op::MonitorExit, 3, MemAccess::MonitorExit),
        ];
        for (op, min_full_arity, access) in with_shape {
            assert_eq!(
                op.memory_shape(),
                Some(OpMemoryShape {
                    token_slot: 1,
                    min_full_arity: *min_full_arity,
                    access: *access,
                }),
                "{op:?}",
            );
        }

        for op in [
            Op::Start,
            Op::Return,
            Op::If,
            Op::Merge,
            Op::Region,
            Op::Proj(0),
            Op::Const(0),
            Op::ConstF(0),
            Op::Param(0),
            Op::Phi,
            Op::Add,
            Op::Guard { bci: 0 },
            Op::Dead,
        ] {
            assert_eq!(op.memory_shape(), None, "{op:?}");
        }
    }

    // ── The guard token edge ─────────────────────────────────────────────

    /// The guarded family and its arities, pinned. What this catches: an op
    /// added to (or removed from) `Op::guard_shape` without the matching arity
    /// widening in `ir_verify::expected_arity`, which would either reject the
    /// guarded shape (a silent fall-back to the single-pass tier) or admit a
    /// trailing operand with no token slot to explain it.
    #[test]
    fn the_guard_shape_table_names_exactly_the_guarded_family() {
        let guarded: &[(Op, usize)] = &[
            (Op::Load(MemKind::Int), 4),
            (Op::Load(MemKind::Ref), 4),
            (Op::ArrayLoad(MemKind::Byte), 4),
            (Op::ArrayLength, 3),
            (Op::Div, 2),
            (Op::Rem, 2),
        ];
        for (op, unguarded_arity) in guarded {
            assert_eq!(
                op.guard_shape(),
                Some(OpGuardShape {
                    unguarded_arity: *unguarded_arity
                }),
                "{op:?}",
            );
            // The slot is the unguarded arity, and only at exactly one more
            // input than that: a compact or documented form must not be read
            // as carrying a token.
            assert_eq!(
                guard_token_slot_for(op, *unguarded_arity + 1),
                Some(*unguarded_arity),
                "{op:?}",
            );
            for arity in 0..=*unguarded_arity {
                assert_eq!(guard_token_slot_for(op, arity), None, "{op:?} @ {arity}");
            }
            assert_eq!(
                guard_token_slot_for(op, *unguarded_arity + 2),
                None,
                "{op:?}",
            );
        }

        // The store family is deliberately excluded — see `Op::guard_shape`.
        for op in [
            Op::Store(MemKind::Int),
            Op::ArrayStore(MemKind::Int),
            Op::Add,
            Op::Mul,
            Op::Guard { bci: 0 },
            Op::Phi,
            Op::Dead,
        ] {
            assert_eq!(op.guard_shape(), None, "{op:?}");
        }
    }

    /// The `Div`/`Rem` split, which is the arm that shows up on no hand-built
    /// fixture. Both sit in the homogeneous arithmetic group of
    /// `expected_input_type`, which demands every operand carry the node's own
    /// result type; the guard is `IrType::Void`. Delete the guard-slot arm in
    /// `expected_input_type` and this fails — while every `Load`-shaped test
    /// keeps passing, because the memory ops already answer `Any` past their
    /// documented slots.
    #[test]
    fn the_guard_token_slot_carries_no_type_requirement() {
        for op in [Op::Div, Op::Rem] {
            assert_eq!(expected_input_type(&op, 0, 3), TypeReq::SameAsResult);
            assert_eq!(expected_input_type(&op, 1, 3), TypeReq::SameAsResult);
            assert_eq!(expected_input_type(&op, 2, 3), TypeReq::Any);
            // Unguarded, slot 2 does not exist and the group is unchanged.
            assert_eq!(expected_input_type(&op, 0, 2), TypeReq::SameAsResult);
            assert_eq!(expected_input_type(&op, 1, 2), TypeReq::SameAsResult);
        }
        assert_eq!(
            expected_input_type(&Op::Load(MemKind::Int), 4, 5),
            TypeReq::Any
        );
        assert_eq!(
            expected_input_type(&Op::ArrayLoad(MemKind::Byte), 4, 5),
            TypeReq::Any
        );
        assert_eq!(expected_input_type(&Op::ArrayLength, 3, 4), TypeReq::Any);
        // A `Store`'s slot 4 is the stored VALUE and keeps its exact
        // requirement — the store family carries no token, and reading one
        // there would drop the value.
        assert_eq!(
            expected_input_type(&Op::Store(MemKind::Byte), 4, 5),
            TypeReq::Exact(IrType::Int)
        );
    }

    /// The guarded shape does not disturb the readers that recover a memory
    /// access's operands by slot. What this catches: a token appended anywhere
    /// but last, or a shape whose guarded arity collides with a layout
    /// `access_location` disambiguates by input count (the `Store` arity-4
    /// "full-without-offset" form is exactly such a case, which is why the
    /// store family has no token).
    #[test]
    fn a_guard_token_does_not_move_a_memory_access_s_operands() {
        let mut g = empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let base = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let off = g.add(Op::Const(3), IrType::Int, vec![], None);
        let cond = g.add(Op::Const(1), IrType::Int, vec![], None);
        let guard = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![ctrl, cond], None);

        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, base, off, guard],
            None,
        );
        assert_eq!(
            access_location(&g.nodes[load as usize], MemAccess::FieldRead),
            AliasClass::Field {
                base,
                offset: AccessOffset::Dynamic(off),
            }
        );
        assert_eq!(memory_token_slot(&g.nodes[load as usize]), Some(1));
        assert_eq!(guard_token_slot(&g.nodes[load as usize]), Some(4));
        assert_eq!(guard_token_of(&g.nodes[load as usize]), guard);

        let al = g.add(
            Op::ArrayLoad(MemKind::Byte),
            IrType::Int,
            vec![ctrl, mem, base, off, guard],
            None,
        );
        assert_eq!(
            access_location(&g.nodes[al as usize], MemAccess::ArrayRead),
            AliasClass::ArrayElem {
                array: base,
                index: AccessOffset::Dynamic(off),
                elem: MemKind::Byte,
            }
        );
        assert_eq!(guard_token_of(&g.nodes[al as usize]), guard);

        let len = g.add(
            Op::ArrayLength,
            IrType::Int,
            vec![ctrl, mem, base, guard],
            None,
        );
        assert_eq!(
            access_location(&g.nodes[len as usize], MemAccess::LengthRead),
            AliasClass::ArrayLength { array: base }
        );
        assert_eq!(guard_token_of(&g.nodes[len as usize]), guard);

        // An unguarded node answers `NO_NODE`, not "the last input".
        let plain = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, base, off],
            None,
        );
        assert_eq!(guard_token_slot(&g.nodes[plain as usize]), None);
        assert_eq!(guard_token_of(&g.nodes[plain as usize]), NO_NODE);
    }

    /// The producer gate is OFF by default. What this catches: a flip of
    /// `guard_token_edges_enabled`'s default made without the soak
    /// `NOTES-opts8.md` §1 asks for — the flip is a one-word change and the
    /// failure mode of getting the consumer side wrong is silent.
    #[test]
    fn the_guard_token_producer_gate_is_off_by_default() {
        // Read the source rather than the function: the flag is latched in a
        // `OnceLock`, so a test that called it would depend on whether some
        // other test in this binary set the variable first.
        let src = include_str!("ir.rs");
        let body = src
            .split("pub fn guard_token_edges_enabled() -> bool {")
            .nth(1)
            .expect("guard_token_edges_enabled is in this file")
            .split("\n}")
            .next()
            .expect("a function body");
        assert!(
            body.contains("runtime_flag_on(\"CRATONVM_JIT_IR_GUARD_TOKEN\")")
                && !body.contains("runtime_flag_default_on"),
            "the gate must be opt-in (`runtime_flag_on`), not default-on: {body}"
        );
    }

    // ── The memory-shape / trap classifiers ──────────────────────────────

    /// The regression the classifier exists for: the four ops that are
    /// arithmetic on their operands and nothing else answer "touches no
    /// memory", whether or not `is_pure` happens to list them. `Op::Div` and
    /// `Op::Rem` are the interesting pair — they are NOT pure, because they
    /// trap, and that has nothing to do with memory.
    #[test]
    fn arithmetic_that_traps_still_touches_no_memory() {
        for op in [Op::Div, Op::Rem, Op::LCmp] {
            assert_eq!(memory_shape_of(&op), MemoryShape::None, "{op:?}");
            assert!(!memory_shape_of(&op).touches_memory(), "{op:?}");
            assert!(!memory_shape_of(&op).may_write(), "{op:?}");
        }
        let fcmp = Op::FCmp {
            double: true,
            nan_greater: false,
        };
        assert_eq!(memory_shape_of(&fcmp), MemoryShape::None);
        // …and the trap question, asked separately, keeps them apart.
        assert!(may_raise(&Op::Div));
        assert!(may_raise(&Op::Rem));
        assert!(!may_raise(&Op::LCmp));
        assert!(!may_raise(&fcmp));
    }

    /// A read is a read and a write is a write. `loop_writes_memory` rests on
    /// exactly this distinction: a read cannot clobber the location some other
    /// node's hoisted load reads.
    #[test]
    fn loads_read_stores_write_and_calls_are_opaque() {
        assert_eq!(memory_shape_of(&Op::Load(MemKind::Int)), MemoryShape::Reads);
        assert_eq!(
            memory_shape_of(&Op::ArrayLoad(MemKind::Int)),
            MemoryShape::Reads
        );
        assert_eq!(memory_shape_of(&Op::ArrayLength), MemoryShape::Reads);
        assert!(!memory_shape_of(&Op::ArrayLength).may_write());

        assert_eq!(
            memory_shape_of(&Op::Store(MemKind::Int)),
            MemoryShape::Writes
        );
        assert_eq!(
            memory_shape_of(&Op::ArrayStore(MemKind::Ref)),
            MemoryShape::Writes
        );
        assert!(memory_shape_of(&Op::Store(MemKind::Int)).may_write());

        // A callee, a class initialiser, an allocation and a monitor are all
        // the top of the lattice, which is the answer `effect_of_node` gives
        // them too.
        for op in [
            Op::Call { info_ptr: 0 },
            Op::LoadStatic {
                class_id: 1,
                field_index: 0,
                type_tag: b'I',
                is_volatile: false,
            },
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            Op::MonitorEnter,
            Op::MonitorExit,
        ] {
            assert_eq!(memory_shape_of(&op), MemoryShape::Opaque, "{op:?}");
            assert!(memory_shape_of(&op).may_write(), "{op:?}");
        }
    }

    /// A guard touches no memory — it carries `[ctrl, cond]` and no memory
    /// edge — and it can still leave the compiled body. Both halves matter:
    /// `ir_optimize::loop_writes_memory` reads the first and
    /// `ir_optimize::licm_hoist_barrier` reads the second.
    #[test]
    fn a_guard_writes_nothing_but_can_leave_the_body() {
        let guard = Op::Guard { bci: 7 };
        assert_eq!(memory_shape_of(&guard), MemoryShape::None);
        assert!(may_raise(&guard));
    }

    /// The op-level answer for the two nodes an `&Op` cannot classify is the
    /// top of the lattice, matching `effect_of_node`. Call sites that know
    /// better say so themselves.
    #[test]
    fn a_phi_and_a_dead_node_get_the_conservative_op_level_answer() {
        assert_eq!(memory_shape_of(&Op::Phi), MemoryShape::Opaque);
        assert_eq!(memory_shape_of(&Op::Dead), MemoryShape::Opaque);
        assert_eq!(memory_shape_of(&Op::Return), MemoryShape::Opaque);
    }

    /// Every op the memory-token table describes has a non-`None` shape, and
    /// every op it does not describe has one of the three hand-classified
    /// answers. This is the anti-drift pairing: `memory_shape_of` is the
    /// coarse projection of `Op::memory_shape`, not a second opinion.
    #[test]
    fn the_shape_classifier_agrees_with_the_memory_token_table() {
        for op in [
            Op::Load(MemKind::Int),
            Op::Store(MemKind::Int),
            Op::ArrayLoad(MemKind::Long),
            Op::ArrayStore(MemKind::Long),
            Op::ArrayLength,
            Op::New {
                class_id: 1,
                num_fields: 0,
            },
            Op::NewArray {
                element_type: 10,
                component_class_id: 0,
            },
            Op::Call { info_ptr: 0 },
            Op::MonitorEnter,
            Op::MonitorExit,
            Op::Throw,
        ] {
            assert!(op.memory_shape().is_some(), "{op:?}");
            assert!(memory_shape_of(&op).touches_memory(), "{op:?}");
            // Anything with a memory token can also transfer control
            // elsewhere — it is a call, an access that faults, or a throw.
            assert!(may_raise(&op), "{op:?}");
        }
        for op in [Op::Add, Op::Cmp(CmpOp::Lt), Op::I2L, Op::Const(3)] {
            assert!(op.memory_shape().is_none(), "{op:?}");
            assert_eq!(memory_shape_of(&op), MemoryShape::None, "{op:?}");
            assert!(!may_raise(&op), "{op:?}");
        }
    }

    // ── The shared expected-operand-type table ───────────────────────────

    /// `Op::Cmp` is the IR's `if_icmp*` and its `if_acmp*` at once, so its
    /// operands have no fixed type — only the requirement that they agree,
    /// which is exactly what `ir_lower` needs when it picks between a 32- and
    /// a 64-bit `CMP` and exactly what no table stated before this one.
    #[test]
    fn a_comparison_requires_its_operands_to_agree_rather_than_a_fixed_type() {
        assert_eq!(expected_input_type(&Op::Cmp(CmpOp::Lt), 0, 2), TypeReq::Any);
        assert_eq!(
            expected_input_type(&Op::Cmp(CmpOp::Lt), 1, 2),
            TypeReq::SameAs(0)
        );
        // Resolved against a node whose slot 0 is a `Ref`, the requirement on
        // slot 1 IS `Ref` — the `if_acmpeq` case, where a 32-bit compare would
        // make two objects 4 GiB apart equal.
        let inputs = [10 as NodeId, 11];
        let ty_of = |id: NodeId| if id == 10 { Some(IrType::Ref) } else { None };
        assert_eq!(
            required_input_type(&Op::Cmp(CmpOp::Eq), IrType::Int, &inputs, 1, &ty_of),
            Some(IrType::Ref)
        );
        // An unresolvable `SameAs` constrains nothing rather than failing.
        let blind = |_: NodeId| None;
        assert_eq!(
            required_input_type(&Op::Cmp(CmpOp::Eq), IrType::Int, &inputs, 1, &blind),
            None
        );
    }

    /// A shift is homogeneous in its value and `Int` in its distance: `lshl`
    /// shifts a `Long` by an `Int`, which is a genuine non-join in the lattice.
    #[test]
    fn a_shift_is_homogeneous_in_the_value_and_int_in_the_distance() {
        assert_eq!(expected_input_type(&Op::Shl, 0, 2), TypeReq::SameAsResult);
        assert_eq!(
            expected_input_type(&Op::Shl, 1, 2),
            TypeReq::Exact(IrType::Int)
        );
        let inputs = [1 as NodeId, 2];
        let blind = |_: NodeId| None;
        assert_eq!(
            required_input_type(&Op::Shl, IrType::Long, &inputs, 0, &blind),
            Some(IrType::Long)
        );
        assert_eq!(
            required_input_type(&Op::Shl, IrType::Long, &inputs, 1, &blind),
            Some(IrType::Int)
        );
    }

    /// The stored value's type comes from the `MemKind`, and the sub-int kinds
    /// collapse to `Int` because this IR has no `Byte` value type — the
    /// narrowing lives in the access.
    #[test]
    fn a_stores_value_type_comes_from_its_memory_kind() {
        let full = 5; // [ctrl, mem, base, offset, value]
        assert_eq!(
            expected_input_type(&Op::Store(MemKind::Byte), 4, full),
            TypeReq::Exact(IrType::Int)
        );
        assert_eq!(
            expected_input_type(&Op::Store(MemKind::Ref), 4, full),
            TypeReq::Exact(IrType::Ref)
        );
        assert_eq!(
            expected_input_type(&Op::Store(MemKind::Double), 4, full),
            TypeReq::Exact(IrType::Double)
        );
        // The base is a reference and the offset an int, at every kind.
        assert_eq!(
            expected_input_type(&Op::Store(MemKind::Int), 2, full),
            TypeReq::Exact(IrType::Ref)
        );
        assert_eq!(
            expected_input_type(&Op::Store(MemKind::Int), 3, full),
            TypeReq::Exact(IrType::Int)
        );
    }

    /// The compact `[base, value]` / `[base]` layouts that `ir_optimize` and
    /// the EA bridge build are outside this table: their slot 1 is a value
    /// where the documented form's is a token, and describing both with one
    /// slot list would describe neither. `Op::memory_shape`'s `min_full_arity`
    /// — the same number the token-slot model reads — is what tells them apart.
    #[test]
    fn a_compact_memory_form_is_not_described_by_the_operand_table() {
        for slot in 0..2 {
            assert_eq!(
                expected_input_type(&Op::Store(MemKind::Int), slot, 2),
                TypeReq::Any,
                "compact store slot {slot}"
            );
            assert_eq!(
                expected_input_type(&Op::Load(MemKind::Int), slot, 1),
                TypeReq::Any,
                "compact load slot {slot}"
            );
        }
    }

    /// `ScalarOp::input_ty` is the single source of truth for an intrinsic
    /// family's operands, and the table defers to it rather than restating it.
    /// `Long.rotateLeft` is the asymmetric case: a `long` value, an `int`
    /// distance.
    #[test]
    fn the_intrinsic_families_take_their_operand_types_from_scalar_op() {
        let rot = Op::ScalarIntrinsic(ScalarOp::RotateLeftL);
        assert_eq!(
            expected_input_type(&rot, 0, 2),
            TypeReq::Exact(IrType::Long)
        );
        assert_eq!(expected_input_type(&rot, 1, 2), TypeReq::Exact(IrType::Int));
        let min = Op::ScalarIntrinsic(ScalarOp::MinL);
        assert_eq!(
            expected_input_type(&min, 0, 2),
            TypeReq::Exact(IrType::Long)
        );
        assert_eq!(
            expected_input_type(&min, 1, 2),
            TypeReq::Exact(IrType::Long)
        );
        let abs = Op::ScalarIntrinsic(ScalarOp::AbsD);
        assert_eq!(
            expected_input_type(&abs, 0, 1),
            TypeReq::Exact(IrType::Double)
        );
        // Past the family's arity the table makes no claim.
        assert_eq!(expected_input_type(&abs, 1, 2), TypeReq::Any);
    }

    /// The two shapes the table knowingly cannot describe answer `Any`, and a
    /// token slot resolves to no requirement at all — the control lane in
    /// `ir_verify` owns control edges, and a memory token's declared type is
    /// whatever last wrote memory.
    #[test]
    fn the_table_declines_the_shapes_it_cannot_describe() {
        let call = Op::Call { info_ptr: 0 };
        assert_eq!(expected_input_type(&call, 2, 4), TypeReq::Any);
        assert_eq!(expected_input_type(&call, 3, 4), TypeReq::Any);
        assert_eq!(expected_input_type(&Op::Phi, 1, 3), TypeReq::Any);
        assert_eq!(expected_input_type(&call, 0, 4), TypeReq::Control);
        assert_eq!(expected_input_type(&call, 1, 4), TypeReq::Memory);
        let inputs = [1 as NodeId, 2, 3, 4];
        let blind = |_: NodeId| None;
        for slot in 0..4 {
            assert_eq!(
                required_input_type(&call, IrType::Int, &inputs, slot, &blind),
                None,
                "slot {slot}"
            );
        }
    }
}

/// The JVMS stack shuffles and `wide` forms the builder used to refuse at its
/// catch-all after `ir_compatible` had admitted the method.
#[cfg(test)]
mod stack_shuffle_and_wide_tests {
    use super::*;

    fn builds(params: usize, locals: usize, code: &[u8], len: usize) -> bool {
        IrBuilder::new(params, locals).build(code, len).is_some()
    }

    #[test]
    fn every_category1_and_category2_shuffle_form_builds() {
        // iload_0 iload_1 swap isub ireturn
        assert!(
            builds(2, 2, &[0x1a, 0x1b, 0x5f, 0x64, 0xac, 0, 0], 5),
            "swap"
        );
        // iconst_1 iconst_2 pop2 iload_0 ireturn
        assert!(
            builds(1, 1, &[0x04, 0x05, 0x58, 0x1a, 0xac, 0, 0], 5),
            "pop2 of two cat-1"
        );
        // lconst_1 pop2 iload_0 ireturn
        assert!(
            builds(1, 1, &[0x0a, 0x58, 0x1a, 0xac, 0, 0], 4),
            "pop2 of one cat-2"
        );
        // iconst_1 iconst_2 iconst_3 dup_x2 pop pop pop ireturn
        assert!(
            builds(
                0,
                0,
                &[0x04, 0x05, 0x06, 0x5b, 0x57, 0x57, 0x57, 0xac, 0, 0],
                8
            ),
            "dup_x2 form 1"
        );
        // lconst_1 iconst_2 dup_x2 pop pop2 ireturn
        assert!(
            builds(0, 0, &[0x0a, 0x05, 0x5b, 0x57, 0x58, 0xac, 0, 0], 6),
            "dup_x2 form 2"
        );
        // iconst_1 iconst_2 iconst_3 dup2_x1 pop2 pop pop2 iconst_0 ireturn
        assert!(
            builds(
                0,
                0,
                &[0x04, 0x05, 0x06, 0x5d, 0x58, 0x57, 0x58, 0x03, 0xac, 0, 0],
                9
            ),
            "dup2_x1 form 1"
        );
        // iconst_1 lconst_1 dup2_x1 pop2 pop pop2 iconst_0 ireturn
        assert!(
            builds(
                0,
                0,
                &[0x04, 0x0a, 0x5d, 0x58, 0x57, 0x58, 0x03, 0xac, 0, 0],
                8
            ),
            "dup2_x1 form 2"
        );
        // lconst_0 lconst_1 dup2_x2 pop2 pop2 pop2 iconst_0 ireturn
        assert!(
            builds(
                0,
                0,
                &[0x09, 0x0a, 0x5e, 0x58, 0x58, 0x58, 0x03, 0xac, 0, 0],
                8
            ),
            "dup2_x2 form 4"
        );
        // iconst_1 iconst_2 lconst_1 dup2_x2 pop2 pop2 pop pop iconst_0 ireturn
        assert!(
            builds(
                0,
                0,
                &[0x04, 0x05, 0x0a, 0x5e, 0x58, 0x57, 0x57, 0x58, 0x03, 0xac, 0, 0],
                10
            ),
            "dup2_x2 form 2"
        );
    }

    /// A category-2 value where the form needs category 1 is not a shape the
    /// verifier produces; the builder refuses rather than shuffling a guess.
    #[test]
    fn an_illegal_shuffle_shape_is_refused() {
        // iconst_1 lconst_1 swap ...
        assert!(!builds(
            0,
            0,
            &[0x04, 0x0a, 0x5f, 0x57, 0x57, 0x03, 0xac, 0, 0],
            7
        ));
    }

    #[test]
    fn wide_loads_stores_and_iinc_build() {
        // wide iinc 0, +256 ; iload_0 ; ireturn
        assert!(builds(
            1,
            1,
            &[0xc4, 0x84, 0x00, 0x00, 0x01, 0x00, 0x1a, 0xac, 0, 0],
            8
        ));
        // wide iload 0 ; wide istore 1 ; iload_1 ; ireturn
        assert!(builds(
            1,
            2,
            &[0xc4, 0x15, 0x00, 0x00, 0xc4, 0x36, 0x00, 0x01, 0x1b, 0xac, 0, 0],
            10
        ));
        // wide iload 7 with only one local: out of range, refused.
        assert!(!builds(1, 1, &[0xc4, 0x15, 0x00, 0x07, 0xac, 0, 0], 5));
    }
}

/// `Op::Switch` — the density rules, the dense normalisation, and the edge set
/// the JVMS requires the builder to preserve.
#[cfg(test)]
mod switch_tests {
    use super::*;

    /// `int f(int x) { switch (x) { … } }` as a `tableswitch` over the slots
    /// `low ..= low + arms.len() - 1`.
    ///
    /// `arms[i]` is `Some(v)` for a slot with its own arm returning `v`, and
    /// `None` for a slot wired to the DEFAULT target — which is how javac pads
    /// a `tableswitch` whose cases have holes, and the shape the trimming and
    /// the live-case count are about. The default arm returns 99.
    ///
    /// Layout: `iload_0` at 0, `tableswitch` at 1, JVMS padding to the next
    /// multiple of 4 (so the operands start at 4), then `default`, `low`,
    /// `high` and one offset per slot, then the arms as `bipush v; ireturn`.
    fn tableswitch_body(low: i32, arms: &[Option<i8>]) -> Vec<u8> {
        let n = arms.len();
        let arm_base = 16 + 4 * n;
        let live: Vec<usize> = (0..n).filter(|&i| arms[i].is_some()).collect();
        let default_at = arm_base + 3 * live.len();
        let mut code = vec![0x1a, 0xaa, 0x00, 0x00];
        code.extend_from_slice(&((default_at as i32) - 1).to_be_bytes());
        code.extend_from_slice(&low.to_be_bytes());
        code.extend_from_slice(&(low + n as i32 - 1).to_be_bytes());
        for i in 0..n {
            let at = match live.iter().position(|&l| l == i) {
                Some(j) => arm_base + 3 * j,
                None => default_at,
            };
            code.extend_from_slice(&((at as i32) - 1).to_be_bytes());
        }
        for &i in &live {
            let v = arms[i].unwrap_or(0);
            code.extend_from_slice(&[0x10, v as u8, 0xac]);
        }
        code.extend_from_slice(&[0x10, 99, 0xac]);
        code
    }

    /// The same, as a `lookupswitch` over `keys` in the order given — which is
    /// NOT necessarily ascending, because one of the things under test is what
    /// happens when it is not.
    fn lookupswitch_body(keys: &[i32]) -> Vec<u8> {
        let n = keys.len();
        let arm_base = 12 + 8 * n;
        let default_at = arm_base + 3 * n;
        let mut code = vec![0x1a, 0xab, 0x00, 0x00];
        code.extend_from_slice(&((default_at as i32) - 1).to_be_bytes());
        code.extend_from_slice(&(n as i32).to_be_bytes());
        for (i, &key) in keys.iter().enumerate() {
            code.extend_from_slice(&key.to_be_bytes());
            code.extend_from_slice(&(((arm_base + 3 * i) as i32) - 1).to_be_bytes());
        }
        for i in 0..n {
            code.extend_from_slice(&[0x10, 10 + i as u8, 0xac]);
        }
        code.extend_from_slice(&[0x10, 99, 0xac]);
        code
    }

    fn build_graph(code: &[u8]) -> Graph {
        let len = code.len();
        // Four trailing zero bytes: the strict decoders read fixed-width
        // operands, and a fixture that ended exactly at the last instruction
        // would be refused for running off the buffer rather than for the
        // reason the test is about.
        let mut padded = code.to_vec();
        padded.extend_from_slice(&[0, 0, 0, 0]);
        match IrBuilder::new(1, 1).build(&padded, len) {
            Some(g) => g,
            None => panic!("the fixture did not build"),
        }
    }

    fn switch_bounds(g: &Graph) -> Vec<(i32, i32)> {
        g.nodes
            .iter()
            .filter_map(|n| match n.op {
                Op::Switch { low, high } => Some((low, high)),
                _ => None,
            })
            .collect()
    }

    fn projections_of(g: &Graph, producer: NodeId) -> Vec<usize> {
        let mut out: Vec<usize> = g
            .nodes
            .iter()
            .filter(|n| n.inputs.first().copied() == Some(producer))
            .filter_map(|n| match n.op {
                Op::Proj(k) => Some(k as usize),
                _ => None,
            })
            .collect();
        out.sort_unstable();
        out
    }

    fn the_switch(g: &Graph) -> NodeId {
        match g
            .nodes
            .iter()
            .position(|n| matches!(n.op, Op::Switch { .. }))
        {
            Some(i) => i as NodeId,
            None => panic!("no Op::Switch in this graph"),
        }
    }

    // ── The density rules ────────────────────────────────────────────

    /// The reason both predicates compute the span in `i64`, as a test rather
    /// than only as a comment.
    ///
    /// `tableswitch low=i32::MIN high=i32::MAX` is legal bytecode. In `i32`,
    /// `high - low + 1` wraps to **0** — which reads as "span zero, perfectly
    /// dense" and would ask `ir_lower` for a jump table with 2^32 entries.
    #[test]
    fn a_full_int_range_switch_does_not_look_dense_through_an_overflowed_span() {
        assert_eq!(
            i32::MAX.wrapping_sub(i32::MIN).wrapping_add(1),
            0,
            "the i32 computation both predicates are avoiding"
        );
        assert!(!switch_is_dense(i32::MIN, i32::MAX, 2));
        assert!(!switch_is_representable(i32::MIN, i32::MAX, 2));
        // Claiming every key is a live case does not buy it a table either:
        // the projection budget is tested first.
        assert!(!switch_is_dense(i32::MIN, i32::MAX, usize::MAX));
        assert!(!switch_is_representable(i32::MIN, i32::MAX, usize::MAX));
        // And the arity the verifier computes is enormous rather than small,
        // so a `Proj` of such a node fails the projection lane instead of
        // passing a plausible bound.
        assert_eq!(
            switch_projection_count(i32::MIN, i32::MAX),
            u32::MAX as usize
        );
    }

    #[test]
    fn a_contiguous_case_set_is_dense_and_a_scattered_one_is_only_representable() {
        // Eight contiguous keys: no waste at all.
        assert!(switch_is_dense(0, 7, 8));
        // Every third key live — three slots per case. Past the table rule,
        // inside the builder's admission: the band the binary search exists
        // for.
        assert!(!switch_is_dense(0, 20, 7));
        assert!(switch_is_representable(0, 20, 7));
        // Two keys a thousand apart: neither.
        assert!(!switch_is_dense(1, 1000, 2));
        assert!(!switch_is_representable(1, 1000, 2));
    }

    /// A shape the lowerer would put in a table but the builder would never
    /// build is a dead arm; a shape the builder builds and the lowerer has no
    /// rule for is a wrong answer. Neither is possible while this holds.
    #[test]
    fn every_table_dense_switch_is_one_the_builder_may_build() {
        for low in [-70i32, -1, 0, 1, 1000] {
            for span in 1i64..=300 {
                let Some(high) = i64::from(low).checked_add(span - 1) else {
                    continue;
                };
                let Ok(high) = i32::try_from(high) else {
                    continue;
                };
                for cases in [1usize, 2, 3, 8, 64, 255] {
                    if switch_is_dense(low, high, cases) {
                        assert!(
                            switch_is_representable(low, high, cases),
                            "dense but not representable: [{low}, {high}] with {cases} cases"
                        );
                    }
                }
            }
        }
    }

    /// The projection budget fits `Op::Proj`'s index, and the span and the
    /// budget are the same fact expressed twice.
    ///
    /// Before 2026-09-17 this was `the_projection_budget_is_the_width_of_a_proj_
    /// index` and asserted EQUALITY with `u8::MAX + 1`: the budget was the width.
    /// It is now a budget inside a `u16`, so what is pinned is that it fits --
    /// the `const` assertion beside the constant already makes an overflow a
    /// compile error; this states the arithmetic around it.
    #[test]
    fn the_projection_budget_fits_the_width_of_a_proj_index() {
        assert!(SWITCH_MAX_PROJECTIONS <= u16::MAX as usize + 1);
        assert_eq!(SWITCH_MAX_SPAN, SWITCH_MAX_PROJECTIONS as i64 - 1);
        let span = SWITCH_MAX_SPAN as i32;
        let slots = SWITCH_MAX_SPAN as usize;
        // The widest admissible switch spends its last index on the default.
        assert!(switch_is_dense(0, span - 1, slots));
        assert_eq!(switch_projection_count(0, span - 1), SWITCH_MAX_PROJECTIONS);
        assert!(!switch_is_dense(0, span, slots + 1));
        assert!(!switch_is_representable(0, span, slots + 1));
    }

    /// The projection cap REFUSES; it does not truncate. End to end, through
    /// the builder, on real `tableswitch` bytecode.
    ///
    /// # Why this test and not only the predicate asserts above
    ///
    /// The only thing standing between a switch one slot past the budget and
    /// `Op::Proj(k as u16)` naming a projection the switch does not have is
    /// `dense_switch_bounds`'s call to [`switch_is_representable`]. Covered only
    /// by asserts on the predicate itself, a regression in `dense_switch_bounds`
    /// (dropping the call, or moving it after the bounds are returned) would be
    /// invisible to the builder tests.
    ///
    /// Both arms matter and they differ by exactly one slot, derived from
    /// [`SWITCH_MAX_PROJECTIONS`] rather than written as numbers -- this test
    /// used to hard-code 255 and 256, which were the `u8` width, and a budget
    /// change would have left it testing the old boundary:
    ///
    /// * `SWITCH_MAX_SPAN` slots, all live -- the widest admissible switch.
    /// * one more -- must keep the `Cmp`/`If` chain, whose projections are only
    ///   ever `Proj(0)` and `Proj(1)`. The method still compiles; the refusal is
    ///   a choice of lowering, not a bail.
    #[test]
    fn a_switch_one_slot_past_the_projection_budget_keeps_the_compare_chain() {
        let span = SWITCH_MAX_SPAN as usize;
        // The widest switch the builder may build.
        let widest: Vec<Option<i8>> = (0..span).map(|i| Some((i % 100) as i8)).collect();
        let g = build_graph(&tableswitch_body(0, &widest));
        assert_eq!(switch_bounds(&g), vec![(0, span as i32 - 1)]);
        let projs = projections_of(&g, the_switch(&g));
        assert_eq!(
            projs.len(),
            SWITCH_MAX_PROJECTIONS,
            "every case edge plus the default"
        );
        assert_eq!(
            projs.last().copied(),
            Some(SWITCH_MAX_PROJECTIONS - 1),
            "the default edge is the last projection the budget allows"
        );

        // One past. No `Op::Switch` at all.
        let one_past: Vec<Option<i8>> = (0..=span).map(|i| Some((i % 100) as i8)).collect();
        let g = build_graph(&tableswitch_body(0, &one_past));
        assert!(
            switch_bounds(&g).is_empty(),
            "a switch one slot past the budget must be REFUSED the dense form"
        );
        assert!(
            g.nodes.iter().any(|n| matches!(n.op, Op::Cmp(_))),
            "…and refused into the compare chain, which is what still compiles it"
        );
        // The decisive assertion: no projection index above 1 exists anywhere,
        // so nothing was produced by an `as u16` past the budget.
        let widest_proj = g
            .nodes
            .iter()
            .filter_map(|n| match n.op {
                Op::Proj(k) => Some(k),
                _ => None,
            })
            .max();
        assert!(
            matches!(widest_proj, Some(0) | Some(1)),
            "the chain's projections are an If's two edges, got {widest_proj:?}"
        );
    }

    /// **The optimisation, proven.** A switch of 256 dense cases -- one past the
    /// old `u8` ceiling -- now builds an `Op::Switch`, where until 2026-09-17 it
    /// was refused into a 256-case compare chain.
    ///
    /// Without this, widening `Op::Proj` could be a pure no-op (the budget left
    /// at 256) and every other test here would still pass. This is the one that
    /// fails if the widening did not actually buy anything.
    #[test]
    fn a_switch_past_the_old_u8_ceiling_now_gets_a_switch_node() {
        assert!(
            SWITCH_MAX_PROJECTIONS > u8::MAX as usize + 1,
            "the budget must exceed the old u8 width, or this item bought nothing"
        );
        let old_one_past: Vec<Option<i8>> = (0..256).map(|i| Some((i % 100) as i8)).collect();
        let g = build_graph(&tableswitch_body(0, &old_one_past));
        assert_eq!(
            switch_bounds(&g),
            vec![(0, 255)],
            "256 dense cases are within the budget now, and get a switch node"
        );
        let projs = projections_of(&g, the_switch(&g));
        assert_eq!(projs.len(), 257, "256 case edges plus the default");
        assert_eq!(
            projs.last().copied(),
            Some(256),
            "projection 256 is namable now -- it was the first index a u8 could not hold"
        );
    }

    // ── The builder ──────────────────────────────────────────────────

    #[test]
    fn a_dense_tableswitch_builds_one_switch_node_with_a_projection_per_slot() {
        let g = build_graph(&tableswitch_body(0, &[Some(10), Some(20), Some(30)]));
        assert_eq!(switch_bounds(&g), vec![(0, 2)]);
        // Three cases and the default: `Proj(0..=2)` and `Proj(3)`.
        assert_eq!(projections_of(&g, the_switch(&g)), vec![0, 1, 2, 3]);
        // And the five-nodes-per-case chain is gone rather than joined: there
        // is no equality compare against the key left in the graph.
        assert!(
            !g.nodes.iter().any(|n| matches!(n.op, Op::Cmp(_))),
            "the dense form replaces the compare chain"
        );
    }

    /// A `lookupswitch` whose keys are negative normalises like any other: the
    /// key range is `[-3, -1]` and nothing about it is a special case.
    #[test]
    fn a_lookupswitch_over_negative_keys_normalises_into_the_dense_form() {
        let g = build_graph(&lookupswitch_body(&[-3, -2, -1]));
        assert_eq!(switch_bounds(&g), vec![(-3, -1)]);
        assert_eq!(projections_of(&g, the_switch(&g)), vec![0, 1, 2, 3]);
    }

    /// The trimming: javac's leading padding slots end up OUTSIDE the range
    /// rather than as projections inside it, because a key outside
    /// `[low, high]` already goes where those slots were going.
    #[test]
    fn a_tableswitch_padded_with_default_slots_is_trimmed_to_its_live_range() {
        // Slots 0, 1 and 2 are default padding; 3 and 4 are the real cases.
        let g = build_graph(&tableswitch_body(
            0,
            &[None, None, None, Some(30), Some(40)],
        ));
        assert_eq!(switch_bounds(&g), vec![(3, 4)]);
        assert_eq!(projections_of(&g, the_switch(&g)), vec![0, 1, 2]);
    }

    /// A hole INSIDE the live range is not trimmed — it is a projection wired
    /// to the default target, which is what keeps the dense index space total.
    #[test]
    fn a_hole_inside_the_live_range_keeps_its_own_projection() {
        let g = build_graph(&tableswitch_body(1, &[Some(10), None, Some(30)]));
        assert_eq!(switch_bounds(&g), vec![(1, 3)]);
        assert_eq!(projections_of(&g, the_switch(&g)), vec![0, 1, 2, 3]);
    }

    /// Too sparse for the dense form to be worth its projections: the chain
    /// stays, and it stays CORRECT — which is the whole reason the floor is
    /// kept reachable rather than deleted.
    #[test]
    fn a_sparse_lookupswitch_keeps_the_compare_chain() {
        let g = build_graph(&lookupswitch_body(&[1, 1000]));
        assert!(switch_bounds(&g).is_empty());
        assert_eq!(
            g.nodes
                .iter()
                .filter(|n| matches!(n.op, Op::Cmp(CmpOp::Eq)))
                .count(),
            2,
            "one equality compare per listed key"
        );
    }

    /// JVMS §6.5 requires a `lookupswitch`'s pairs to be sorted and distinct.
    /// Nothing on this path verifies it, and a duplicate key would want two
    /// projections for one slot of the dense form — so an unsorted payload
    /// keeps the chain, which compares against every listed key in table order
    /// whatever the payload contains.
    #[test]
    fn an_unsorted_or_duplicated_lookupswitch_keeps_the_compare_chain() {
        assert!(switch_bounds(&build_graph(&lookupswitch_body(&[5, 1, 3]))).is_empty());
        assert!(switch_bounds(&build_graph(&lookupswitch_body(&[2, 2, 3]))).is_empty());
    }

    /// JVMS gives a switch one edge per listed key and one default edge, and
    /// says nothing about them being distinct targets. Three cases that all
    /// return the same value still have three edges.
    #[test]
    fn every_case_edge_and_the_default_edge_survive_the_dense_form() {
        let g = build_graph(&tableswitch_body(0, &[Some(10), Some(10), Some(10)]));
        let switch_node = the_switch(&g);
        assert_eq!(projections_of(&g, switch_node), vec![0, 1, 2, 3]);
        let edges = g
            .nodes
            .iter()
            .filter(|n| n.inputs.first().copied() == Some(switch_node))
            .count();
        assert_eq!(
            edges, 4,
            "three cases and the default, every one of them its own edge"
        );
    }

    /// Every slot pointing at the default is an instruction that always takes
    /// the default: there is nothing for a multi-way node to dispatch on, and
    /// the chain's trailing jump is already the whole instruction.
    #[test]
    fn a_tableswitch_whose_every_slot_is_the_default_keeps_the_chain() {
        let g = build_graph(&tableswitch_body(0, &[None, None, None]));
        assert!(switch_bounds(&g).is_empty());
    }
}

// ── Round 9 wave 3 (lane irmid3): bounded self-recursive splicing and the
// splice resume map ─────────────────────────────────────────────────────────
#[cfg(test)]
mod r9w3_irmid3_tests {
    use super::*;

    /// A live `JitInvokeInfo` for `P.f(I)I` of the given kind. Leaked: an
    /// `Op::Call` bakes the address and `self_recursive_call_row` reads it.
    fn info(invoke_kind: u8) -> usize {
        let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
            class_name: "P",
            method_name: "f",
            descriptor: "(I)I",
            num_jit_args: 1,
            return_type: b'I',
            invoke_kind,
            declaring_class_id: 7,
        }));
        info as *const JitInvokeInfo as usize
    }

    fn self_site(base: usize, code_len: usize) -> IrInlineSite {
        IrInlineSite {
            base,
            code_len,
            num_args: 1,
            max_locals: 1,
            arg_local_slots: vec![0],
            returns_value: true,
            receiver_is_arg0: false,
            method_key: "P.f:(I)I".to_string(),
            class_id: 7,
        }
    }

    /// A self site takes a copy while the budget lasts and becomes a leaf
    /// bound to the caller's own kind-4 row after that; anything that is not
    /// exactly the compiling method (another method, an instance method, an
    /// unknown class id, a same-named class with another id) is `NotSelf`.
    #[test]
    fn a_self_recursion_plan_splices_up_to_its_budget_then_leaves() {
        let row = (0x1000usize, 1usize, b'I');
        let plan = IrSelfRecursion::new("P", "f", "(I)I", 7, row);
        assert_eq!(plan.copies(), 0);
        let IrSelfSplice::Splice(one) = plan.classify("P", "f", "(I)I", 7, true) else {
            panic!("the first self copy splices");
        };
        assert_eq!(one.copies(), 1);
        let IrSelfSplice::Splice(two) = one.classify("P", "f", "(I)I", 7, true) else {
            panic!("the second self copy splices (IR_RECURSIVE_INLINE_MAX_COPIES = 2)");
        };
        assert_eq!(two.copies(), IR_RECURSIVE_INLINE_MAX_COPIES);
        assert_eq!(
            two.classify("P", "f", "(I)I", 7, true),
            IrSelfSplice::Leaf(row),
            "past the budget the site is a direct self-call",
        );
        // A non-self site passes the plan through unchanged.
        for (c, m, d, id, st) in [
            ("P", "g", "(I)I", 7u32, true),
            ("P", "f", "(J)I", 7, true),
            ("Q", "f", "(I)I", 7, true),
            ("P", "f", "(I)I", 8, true),
            ("P", "f", "(I)I", 0, true),
            ("P", "f", "(I)I", 7, false),
        ] {
            assert_eq!(
                two.classify(c, m, d, id, st),
                IrSelfSplice::NotSelf,
                "{c}.{m}{d} id={id} static={st}"
            );
        }
        // A zero budget makes every self site a leaf straight away.
        let none = IrSelfRecursion::new("P", "f", "(I)I", 7, row).with_max_copies(0);
        assert_eq!(
            none.classify("P", "f", "(I)I", 7, true),
            IrSelfSplice::Leaf(row)
        );
        // An unknown compiling class id matches nothing.
        let unknown = IrSelfRecursion::new("P", "f", "(I)I", 0, row);
        assert_eq!(
            unknown.classify("P", "f", "(I)I", 0, true),
            IrSelfSplice::NotSelf
        );
    }

    /// The row is the caller's own kind-4 row at the lowest caller pc; a
    /// kind-3 row and a row past `code_len` (a spliced body's) are ignored.
    #[test]
    fn the_self_recursive_row_is_the_callers_kind_4_row() {
        let k4_low = info(4);
        let k4_high = info(4);
        let k3 = info(3);
        let spliced = info(4);
        let mut b = IrBuilder::new(1, 1);
        b.set_invoke_info(HashMap::from([
            (1usize, (k3, 1usize, b'I')),
            (3usize, (k4_low, 1usize, b'I')),
            (6usize, (k4_high, 1usize, b'I')),
            (12usize, (spliced, 1usize, b'I')),
        ]));
        assert_eq!(b.self_recursive_call_row(9), Some((k4_low, 1, b'I')));
        let mut none = IrBuilder::new(1, 1);
        none.set_invoke_info(HashMap::from([(1usize, (k3, 1usize, b'I'))]));
        assert_eq!(none.self_recursive_call_row(9), None);
    }

    /// `static int f(int n) { return f(n - 1) + 1; }` with ONE self copy
    /// spliced at its call and the copy's own self-call bound to the caller's
    /// kind-4 row: the graph has exactly one `Op::Call`, at the copy's pc, and
    /// it carries that row -- i.e. it lowers as this method's direct
    /// self-call. This is the plan `lib.rs` builds for a leaf.
    #[test]
    fn a_leaf_row_inside_a_self_copy_builds_the_direct_self_call() {
        let body: [u8; 9] = [
            0x1a, // 0: iload_0
            0x04, // 1: iconst_1
            0x64, // 2: isub
            0xb8, 0x00, 0x02, // 3: invokestatic #2 (f)
            0x04, // 6: iconst_1
            0x60, // 7: iadd
            0xac, // 8: ireturn
        ];
        let code_len = body.len();
        let mut combined = body.to_vec();
        let base = combined.len();
        combined.extend_from_slice(&body);
        combined.extend_from_slice(&[0, 0]);

        let k4 = info(4);
        let row = (k4, 1usize, b'I');
        let mut b = IrBuilder::new(1, 1);
        b.set_invoke_info(HashMap::from([(3usize, row)]));
        assert_eq!(b.self_recursive_call_row(code_len), Some(row));
        let tables = IrInlineTables {
            sites: HashMap::from([(3usize, self_site(base, code_len))]),
            invoke_info: HashMap::from([(base + 3, row)]),
            ..IrInlineTables::default()
        };
        b.apply_inline_tables(tables);
        let graph = b
            .build(&combined, code_len)
            .expect("a self copy with a leaf self-call builds");
        let calls: Vec<(Option<usize>, usize)> = graph
            .nodes
            .iter()
            .filter_map(|n| match n.op {
                Op::Call { info_ptr } => Some((n.bytecode_pc, info_ptr)),
                _ => None,
            })
            .collect();
        assert_eq!(
            calls,
            vec![(Some(base + 3), k4)],
            "the spliced pc 3 emits no call; the copy's pc does, with the kind-4 row",
        );
        // The build published where the copy's pcs resume.
        let map = splice_resume_map_this_build().expect("a build with a splice publishes its map");
        assert_eq!(map.resume_bci(base + 3), Some(3));
        assert_eq!(map.resume_bci(2), Some(2));
    }

    /// The resume map sends a nested body's pc to the OUTERMOST invoke, and
    /// answers `None` for a pc in no body or a caller chain that never reaches
    /// the caller's code.
    #[test]
    fn the_splice_resume_map_walks_to_the_outermost_invoke() {
        // Caller 10 bytes: invoke at 2 -> body [10, 20); inside it an invoke
        // at 13 -> body [20, 26); invoke at 5 -> body [26, 30).
        let sites = HashMap::from([
            (2usize, self_site(10, 10)),
            (13usize, self_site(20, 6)),
            (5usize, self_site(26, 4)),
        ]);
        let map = IrSpliceResumeMap::from_sites(10, &sites);
        assert_eq!(map.bodies, vec![(10, 10, 2), (20, 6, 13), (26, 4, 5)]);
        assert_eq!(map.resume_bci(4), Some(4), "a caller pc resumes at itself");
        assert_eq!(map.resume_bci(10), Some(2));
        assert_eq!(map.resume_bci(19), Some(2));
        assert_eq!(map.resume_bci(21), Some(2), "nested: the outermost invoke");
        assert_eq!(map.resume_bci(29), Some(5));
        assert_eq!(map.resume_bci(30), None, "past every body");
        // A malformed table whose chain loops never reaches caller code.
        let looped = IrSpliceResumeMap {
            code_len: 10,
            bodies: vec![(10, 10, 15)],
        };
        assert_eq!(looped.resume_bci(12), None);
    }

    /// A build with no splice publishes no map, and the results scope clears
    /// one a previous build left.
    #[test]
    fn a_build_without_a_splice_publishes_no_resume_map() {
        set_splice_resume_map_this_build(Some(IrSpliceResumeMap::default()));
        let code = [0x04, 0xac, 0, 0]; // iconst_1; ireturn
        let _ = IrBuilder::new(0, 0).build(&code, 2).expect("build");
        assert_eq!(splice_resume_map_this_build(), None);
        set_splice_resume_map_this_build(Some(IrSpliceResumeMap::default()));
        drop(IrBuildResultsScope::enter());
        assert_eq!(splice_resume_map_this_build(), None);
    }
}
