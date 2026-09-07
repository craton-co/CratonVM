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
    ConstString {
        holder_class_id: u32,
        cp_idx: u16,
    },

    /// `ldc <Class>` — the mirror of the class named at `cp_idx` in
    /// `holder_class_id`'s constant pool. Inputs `[ctrl, mem]`, result `Ref`.
    ///
    /// Lowered as a call to `helpers.ldc_class_cp`. CP-indexed rather than
    /// class-id-indexed for the two reasons `JitLdcConstant::ClassMirror`
    /// records: the target may not be loaded yet, and loading it is a user
    /// `ClassLoader.loadClass` that must not run inside the compiler. A `0`
    /// return is a published pending exception, not a value.
    ConstClass {
        holder_class_id: u32,
        cp_idx: u16,
    },

    /// `getstatic` — read the static field at `field_index` of `class_id`.
    /// Inputs `[ctrl, mem]`; result typed from `type_tag`.
    ///
    /// Two lowerings, chosen exactly as the single-pass backend's `0xb2` arm
    /// chooses: a direct load through the class's baked base-POINTER cell when
    /// `x64::resolve_static_base` accepts the site (which it does only for a
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

    // ── Dead / removed ───────────────────────────────────────────────
    /// Placeholder for a removed node (inputs cleared, not referenced).
    Dead,
}

impl Op {
    /// True if this node produces a control token.
    pub fn is_control(&self) -> bool {
        matches!(
            self,
            Op::Start | Op::Return | Op::Throw | Op::If | Op::Merge | Op::Region | Op::Proj(_)
        )
    }

    /// True if this node is pure (no side effects, safe to GVN / eliminate).
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
/// What is NOT here is what v1 refuses in a spliced body: `ldc` / `ldc2_w`
/// (the resolver records a raw `i64` where the builder wants the value plus its
/// float/double discriminator, and inventing that bit is how a `long` constant
/// becomes a `double`), `getstatic` / `putstatic`, `anewarray`, `checkcast` and
/// `instanceof`. A callee using any of them is refused whole at resolution.
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
            self.uses.sp_len = self.safepoints.len();
            self.uses.stamp();
        }
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

    /// A volatile read of `class` — an **acquire**. Volatile field access
    /// still has no producing op (unlike the monitors, which now have
    /// [`Op::MonitorEnter`] / [`Op::MonitorExit`]), so this constructor is the
    /// only way to state its ordering.
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

/// Publish the resolved `java/lang/String` layout for the optimizing tier.
///
/// The first call wins and later calls are ignored; see
/// [`PUBLISHED_STRING_LAYOUT`] for why that is sound for the fields this tier
/// actually reads. Until something calls this, every String call site in the
/// optimizing tier keeps its dispatch and `no_layout` in
/// [`ir_string_intrinsic_census`] counts each one — which is what makes "the
/// emitter is unwired" readable rather than indistinguishable from "the
/// emitter found nothing".
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

pub fn publish_string_layout(layout: crate::StringFieldLayout) {
    let _ = PUBLISHED_STRING_LAYOUT.set(layout);
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
    *CACHE
        .get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_IR_STRING").is_some())
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
            _num_locals: num_locals,
            merges: HashMap::new(),
            loop_headers: HashSet::new(),
            loop_phis: HashMap::new(),
            field_info: HashMap::new(),
            new_info: HashMap::new(),
            anewarray_info: HashMap::new(),
            trivial_init_pcs: HashSet::new(),
            object_init_pcs: HashSet::new(),
            invoke_info: HashMap::new(),
            scalar_intrinsics: HashMap::new(),
            indy_trap_sites: HashMap::new(),
            inline_sites: HashMap::new(),
            splice: Vec::new(),
            splices_done: 0,
            splice_local: HashSet::new(),
            splice_tainted: HashSet::new(),
            splice_guard_seen: false,
            invoke_labels: HashMap::new(),
            method_label: None,
            tdigest_scalar_kernel: false,
            ldc2w_info: HashMap::new(),
            wide_field_long: false,
            wide_field_fp: false,
            ldc_info: HashMap::new(),
            ldc_string_info: HashMap::new(),
            ldc_class_info: HashMap::new(),
            static_field_info: HashMap::new(),
            instanceof_info: HashMap::new(),
            checkcast_info: HashMap::new(),
            string_layout: published_string_layout(),
        }
    }

    /// inc 26: supply resolved `ldc2_w` long-constant values (`pc → i64`). Must
    /// be called before [`Self::build`]; an `ldc2_w` pc not present bails.
    pub fn set_ldc2w_info(&mut self, info: HashMap<usize, (i64, bool)>) {
        self.ldc2w_info = info;
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
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, recv_nonnull],
            Some(pc),
        );

        // 2. `coder` first, then `value` — the `Op::Load` helper arm is a
        //    `CALL`, and loading the reference last is the shortest window in
        //    which a live oop sits across one.
        let coder_index = self.iconst(layout.coder_field_index as i64);
        let coder = self.graph.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, self.mem, recv, coder_index],
            Some(pc),
        );
        self.mem = coder;
        let value_index = self.iconst(layout.value_field_index as i64);
        let value = self.graph.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            vec![ctrl, self.mem, recv, value_index],
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
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, value_nonnull],
            Some(pc),
        );

        // 4. char_count = value.length >> coder. `Op::ArrayLength` does not
        //    advance the memory token (nothing writes an array's length), so
        //    it takes the current one as an input only.
        let byte_len = self.graph.add(
            Op::ArrayLength,
            IrType::Int,
            vec![ctrl, self.mem, value],
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
            let non_negative = self.add_data(Op::Cmp(CmpOp::Ge), IrType::Int, vec![index, zero], pc);
            self.graph.add(
                Op::Guard { bci: pc },
                IrType::Void,
                vec![ctrl, non_negative],
                Some(pc),
            );
            let in_bounds =
                self.add_data(Op::Cmp(CmpOp::Lt), IrType::Int, vec![index, char_count], pc);
            self.graph.add(
                Op::Guard { bci: pc },
                IrType::Void,
                vec![ctrl, in_bounds],
                Some(pc),
            );

            // Branchless LATIN1/UTF16 decode. See the section header for why
            // the second byte's address is `off + coder` and why both reads
            // are in bounds for every index this guard admits.
            let off = self.add_data(Op::Shl, IrType::Int, vec![index, coder], pc);
            let lo_raw = self.graph.add(
                Op::ArrayLoad(MemKind::Byte),
                IrType::Int,
                vec![ctrl, self.mem, value, off],
                Some(pc),
            );
            self.mem = lo_raw;
            let hi_off = self.add_data(Op::Add, IrType::Int, vec![off, coder], pc);
            let hi_raw = self.graph.add(
                Op::ArrayLoad(MemKind::Byte),
                IrType::Int,
                vec![ctrl, self.mem, value, hi_off],
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

    /// Gap B: supply the resolved `invokestatic` call sites (`pc → (info_ptr,
    /// num_args, ret_type)`) the builder lowers into `Op::Call`. Must be called
    /// before [`Self::build`]; an `invokestatic` pc not present bails the build
    /// to single-pass. `ret_type` is the JVM return-type byte (see the field doc).
    /// Record the call sites the planner will lower as arithmetic. Keyed by
    /// caller pc, exactly like [`Self::set_invoke_info`], and deliberately a
    /// SEPARATE map: a site here has no `JitInvokeInfo` box and never will.
    pub fn set_scalar_intrinsics(&mut self, sites: HashMap<usize, ScalarOp>) {
        self.scalar_intrinsics = sites;
    }

    /// Record the `invokedynamic` sites this tier may replace with a trap.
    /// See [`IrBuilder::indy_trap_sites`].
    pub fn set_indy_trap_sites(&mut self, sites: HashMap<usize, (usize, u8)>) {
        self.indy_trap_sites = sites;
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
        let node = self.add_data(
            Op::ScalarIntrinsic(sop),
            sop.result_type(),
            inputs,
            pc,
        );
        self.push(node);
        note_scalar_intrinsic_lowered();
        true
    }

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
        } = tables;
        self.inline_sites.extend(sites);
        self.field_info.extend(field_info);
        self.invoke_info.extend(invoke_info);
        self.new_info.extend(new_info);
        self.object_init_pcs.extend(object_init_pcs);
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
        let arg_local_slots = site.arg_local_slots.clone();

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

        let saved_locals = std::mem::replace(&mut self.locals, callee_locals);
        let saved_stack = std::mem::take(&mut self.stack);
        self.splice.push(SpliceFrame {
            return_pc: pc + instr_len,
            end,
            saved_locals,
            saved_stack,
            returns_value,
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
    /// `Op::Phi` is not a case here: the resolver admits branch-free bodies
    /// only, so a spliced region contains no φ.
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

    // ── Node creation helpers ────────────────────────────────────────

    fn add_data(&mut self, op: Op, ty: IrType, inputs: Vec<NodeId>, pc: usize) -> NodeId {
        self.graph.add(op, ty, inputs, Some(pc))
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
        for (i, node) in self.graph.nodes.iter().enumerate() {
            if node.op == Op::Const(val) && node.ty == IrType::Int {
                return i as NodeId;
            }
        }
        self.graph.add(Op::Const(val), IrType::Int, vec![], None)
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
        for (i, node) in self.graph.nodes.iter().enumerate() {
            if node.op == Op::Const(0) && node.ty == IrType::Ref {
                return i as NodeId;
            }
        }
        self.graph.add(Op::Const(0), IrType::Ref, vec![], None)
    }

    fn lconst(&mut self, val: i64) -> NodeId {
        self.graph.add(Op::Const(val), IrType::Long, vec![], None)
    }

    /// Anchor an integer `idiv`/`ldiv`/`irem`/`lrem`'s zero-divisor trap to the
    /// control token in effect at `pc`.
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
    /// node so a graph built without one keeps its old behaviour.
    ///
    /// No-op in unreachable code (no live control token), which is also where
    /// no safepoint snapshot is recorded and where the lowerer therefore emits
    /// no guard either.
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
    fn plant_uncommon_trap(&mut self, pc: usize, cause: TrapCause) -> bool {
        if !ir_site_trap_enabled() {
            return false;
        }
        // The unresolved-class causes are opt-in and off by default; see
        // `ir_unresolved_class_trap_enabled` for the argument that was refuted.
        if matches!(
            cause,
            TrapCause::UnresolvedTypeCheck | TrapCause::UnresolvedNew
        ) && !ir_unresolved_class_trap_enabled()
        {
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
        if !self.graph.safepoints.iter().any(|sp| sp.bci == pc) {
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
        if ir_bail_reporting() {
            eprintln!(
                "[ir] site TRAP planted at bytecode pc {pc} ({}) in {} -- the rest of the method still compiles",
                cause.as_str(),
                self.method_label.as_deref().unwrap_or("<unknown>"),
            );
        }
        true
    }

    fn add_div_zero_guard(&mut self, divisor: NodeId, ty: IrType, pc: usize) {
        let Some(ctrl) = self.ctrl_opt() else {
            return;
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
        );
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
        self.graph.set_inputs(region, state.ctrl_inputs.clone());
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
        self.graph.push_input(region, back_ctrl);

        // Appending the back-edge value completes the φ's input list, so its
        // type is re-derived from the *whole* merge (`retype_phi`): the type
        // recorded at header activation saw only the entry edge(s).
        for (i, &phi) in lp.local_phis.iter().enumerate() {
            if let Some(phi) = node_id_opt(phi) {
                let v = self.locals.get(i).copied().unwrap_or(NO_NODE);
                self.graph.push_input(phi, v);
                self.retype_phi(phi);
            }
        }
        for (i, &phi) in lp.stack_phis.iter().enumerate() {
            if let Some(phi) = node_id_opt(phi) {
                let v = self.stack.get(i).copied().unwrap_or(NO_NODE);
                self.graph.push_input(phi, v);
                self.retype_phi(phi);
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
        self.graph.set_inputs(merge_id, state.ctrl_inputs.clone());
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

    // ── Main bytecode→IR conversion ──────────────────────────────────

    /// Convert bytecode to IR graph.  Returns `None` if an unsupported
    /// opcode is encountered.
    pub fn build(mut self, code: &[u8], code_len: usize) -> Option<Graph> {
        // Per-build, per-thread: `lib.rs` reads it back the moment this
        // returns, to install the compact-field rows the String-access
        // expansion's two loads need. See `string_access_site_pcs`.
        reset_string_access_sites();
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
        // The `|| !self.splice.is_empty()` half is IR-tier inlining: inside a
        // splice `pc` addresses a relocated callee body appended AFTER
        // `code_len`, and the walk must keep going until that body returns.
        // With no splices open this is exactly `pc < code_len`.
        while pc < code_len || !self.splice.is_empty() {
            // Bounds on the relocated region. A callee body that falls off its
            // own end never executed a return, which means the resolver admitted
            // a shape the walk does not agree with — refuse rather than walk
            // into whatever `lib.rs` appended next.
            if let Some(frame) = self.splice.last() {
                if pc >= frame.end {
                    return ir_build_bail(line!(), pc);
                }
            }
            // A relocated body is unreachable from pc 0 by construction, so the
            // reachability skip and the merge bookkeeping — both of which are
            // computed over the CALLER's code alone — apply only outside a
            // splice. The resolver admits branch-free bodies only, so there is
            // no merge inside one to activate.
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

            // If this PC is a merge target, activate the merge
            if self.splice.is_empty() && self.merges.contains_key(&pc) {
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
                });
            }

            let op = code[pc];
            match op {
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
                    self.add_div_zero_guard(b, IrType::Int, pc);
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
                    self.add_div_zero_guard(b, IrType::Long, pc);
                    let r = self.add_data(Op::Div, IrType::Long, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // irem
                0x70 => {
                    let b = self.pop();
                    let a = self.pop();
                    self.add_div_zero_guard(b, IrType::Int, pc);
                    let r = self.add_data(Op::Rem, IrType::Int, vec![a, b], pc);
                    self.push(r);
                    pc += 1;
                }
                // lrem — see `ldiv`: the lowerer handles the 64-bit `Long` path.
                0x71 => {
                    let b = self.pop();
                    let a = self.pop();
                    self.add_div_zero_guard(b, IrType::Long, pc);
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
                            if !self.plant_uncommon_trap(pc, TrapCause::UnresolvedTypeCheck) {
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
                            if !self.plant_uncommon_trap(pc, TrapCause::UnresolvedTypeCheck) {
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
                0xc2 | 0xc3 => {
                    let obj = self.pop();
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
                            if !self.plant_uncommon_trap(pc, TrapCause::UnresolvedNew) {
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
                    if !self.plant_uncommon_trap(pc, TrapCause::Indy) {
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
                    // Inside a splice this is not a method exit: it hands the
                    // value back to the caller's operand stack and the walk
                    // resumes after the `invoke`. No `Op::Return`, and `ctrl`
                    // stays live.
                    if !self.splice.is_empty() {
                        match self.end_splice(Some(val)) {
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
                        match self.end_splice(Some(val)) {
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
                        match self.end_splice(Some(val)) {
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
                        match self.end_splice(None) {
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
                        let s = self.graph.add(
                            Op::ConstString {
                                holder_class_id,
                                cp_idx,
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
                        let c = self.graph.add(
                            Op::ConstClass {
                                holder_class_id,
                                cp_idx,
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
            // wide (0xc4) — JVMS §6.5. `wide iinc` is SIX bytes (prefix,
            // opcode, 2-byte index, 2-byte signed constant); every other
            // widened form is four. Without this arm the prefix fell to the
            // 1-byte catch-all below and the walk desynced by two bytes,
            // reading operand bytes as phantom opcodes — the same failure the
            // `0xb9`/`0xba` arms above are written for.
            //
            // The tables in `x64/licm.rs` and `regalloc.rs` that DO carry this
            // arm described it for months as "currently latent — `jit_scan`
            // rejects `wide`". That stopped being true when the widened forms
            // were implemented: `x64/bytecode_compat.rs::jit_scan` accepts
            // `wide` load/store and `wide iinc` today, so compiled methods DO
            // contain the prefix and every PC-stepping consumer is
            // load-bearing. This walker is reached only for a method the
            // builder then refuses — its own main loop has no `0xc4` arm, so
            // `wide` still bails to single-pass — which is why this is
            // insurance rather than a fix for anything measured.
            0xc4 => {
                if pc + 1 < code_len && code[pc + 1] == 0x84 {
                    pc += 6;
                } else {
                    pc += 4;
                }
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
            // wide (0xc4) — see the twin in `find_branch_targets` above for
            // why this arm exists and why the "currently latent" wording on
            // the other length tables was stale.
            0xc4 => {
                if pc + 1 < code_len && code[pc + 1] == 0x84 {
                    pc += 6
                } else {
                    pc += 4
                }
            }
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
/// (`jit-ir-relocation-map-contract.md`). Fifteen distinct
/// `return None` sites share one observable outcome; only the site tells them
/// apart, and nothing reported it.
///
/// `site` is `line!()` at the refusal rather than a parallel reason enum: it
/// names the site exactly and cannot drift out of step with the code.
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

fn note_trap_planted(cause: TrapCause) {
    TRAPS_PLANTED[cause.index()].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
    // IR-builder-adjacent) `find_branch_targets`/`find_loop_headers`
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
    //    reported "open" (jit-ir-relocation-map-contract.md). A
    //    method the builder will refuse must be refused HERE, cheaply and with
    //    a reason, not after a full graph build.
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
            let Some(graph) = IrBuilder::new(4, 4).build(&code, 4) else {
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
        let mem_phi = b
            .graph
            .add(Op::Phi, IrType::Memory, vec![region, back], None);
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
                },
                2,
                MemAccess::Opaque,
            ),
            (
                Op::ConstClass {
                    holder_class_id: 0,
                    cp_idx: 0,
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
}
