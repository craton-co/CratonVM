#[allow(unused_imports)]
use super::*;


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
pub(in crate::ir) fn is_int_family(t: IrType) -> bool {
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

/// JIT r21 wave 1 (lane typecheck, TYPECHECK21-6): is a `checkcast` of `obj`
/// against the site `name_ptr`, built while the builder's control token is
/// `ctrl`, proven by the `instanceof` that guards it?
///
/// `true` exactly when `ctrl` IS the projection of an `If` on which the same
/// SSA value `obj` is an instance of the same interned site target:
///
/// * `Proj(1)` (the fall-through) of `If(Cmp(Eq, InstanceOf(obj), 0))` --
///   javac's `instanceof T ; ifeq else`, the shape of every `if (o
///   instanceof T)` and of every pattern match;

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
pub(in crate::ir) fn phi_inputs_conflict(graph: &Graph, inputs: &[NodeId]) -> bool {
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
// Until round 12 the requirements below were enforced in release builds only
// when an operator explicitly asked (`CRATONVM_JIT_VERIFY_TYPES=1`); see
// `ir_verify::operand_type_lane_enabled`. Since round 11 wave 18, debug builds
// also run them at post-optimize (`ir_verify::operand_type_lane_staged_at`;
// `CRATONVM_JIT_VERIFY_TYPES=0` turns that off), and since round 12 wave 1 so
// do release builds unless `CRATONVM_JIT_VERIFY_OPERAND_TYPES_RELEASE=0`.
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
        // `[ctrl, mem, src, src_pos, dst, dst_pos, len]`.
        Op::ByteArrayCopy => match idx {
            0 => TypeReq::Control,
            1 => TypeReq::Memory,
            2 | 4 => TypeReq::Exact(IrType::Ref),
            3 | 5 | 6 => TypeReq::Exact(IrType::Int),
            _ => TypeReq::Any,
        },
        // `[ctrl, mem, array]`
        Op::ArrayLength => match idx {
            0 => TypeReq::Control,
            1 => TypeReq::Memory,
            2 => TypeReq::Exact(IrType::Ref),
            _ => TypeReq::Any,
        },
        // `[ctrl, mem, value, index, coder]` (STRINTR21-5).
        Op::StrCharDecode => match idx {
            0 => TypeReq::Control,
            1 => TypeReq::Memory,
            2 => TypeReq::Exact(IrType::Ref),
            3 | 4 => TypeReq::Exact(IrType::Int),
            _ => TypeReq::Any,
        },
        // `[ctrl, mem]`
        Op::New { .. }
        | Op::NewCp { .. }
        | Op::ConstString { .. }
        | Op::ConstClass { .. }
        | Op::LoadStatic { .. } => {
            match idx {
                0 => TypeReq::Control,
                1 => TypeReq::Memory,
                _ => TypeReq::Any,
            }
        }
        // `[ctrl, mem, value]` — the value's type is the field descriptor's
        // (a `Z/B/C/S` value is an already-narrowed `Int`).
        Op::StoreStatic { type_tag, .. } => match idx {
            0 => TypeReq::Control,
            1 => TypeReq::Memory,
            2 => match *type_tag {
                b'L' | b'[' => TypeReq::Exact(IrType::Ref),
                b'J' => TypeReq::Exact(IrType::Long),
                b'F' => TypeReq::Exact(IrType::Float),
                b'D' => TypeReq::Exact(IrType::Double),
                _ => TypeReq::Exact(IrType::Int),
            },
            _ => TypeReq::Any,
        },
        // `[ctrl, mem, length]`
        Op::NewArray { .. } => match idx {
            0 => TypeReq::Control,
            1 => TypeReq::Memory,
            2 => TypeReq::Exact(IrType::Int),
            _ => TypeReq::Any,
        },
        // JIT round 24 B2: the pre-header arena cache reads no object.
        // `ArenaCacheEntry` is `[ctrl, mem, min, span, session]`,
        // `ArenaCacheBase` `[ctrl, mem, entry, min]` (the general `Unbox` row
        // below would demand a reference of `min` / `entry`, and the verifier
        // then sent every cached method to the single-pass tier).
        Op::Unbox {
            op: UnboxOp::ArenaCacheEntry,
            ..
        } => match idx {
            0 => TypeReq::Control,
            1 => TypeReq::Memory,
            2 | 3 => TypeReq::Exact(IrType::Long),
            4 => TypeReq::Exact(IrType::Ref),
            _ => TypeReq::Any,
        },
        Op::Unbox {
            op: UnboxOp::ArenaCacheBase,
            ..
        } => match idx {
            0 => TypeReq::Control,
            1 => TypeReq::Memory,
            2 | 3 => TypeReq::Exact(IrType::Long),
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
        // descriptor's, and nothing here carries it. A bridged
        // `invokedynamic` has the same shape (round 16, lane indy).
        Op::Call { .. } | Op::IndyBridge { .. } => match idx {
            0 => TypeReq::Control,
            1 => TypeReq::Memory,
            _ => TypeReq::Any,
        },
        // `[ctrl, mem, dims…]`: every dimension is an `int` (round 16, lane
        // matrix).
        Op::MultiNewArray { .. } => match idx {
            0 => TypeReq::Control,
            1 => TypeReq::Memory,
            _ => TypeReq::Exact(IrType::Int),
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
        // `[obj]` — the reference whose header class id is tested.
        Op::ExactClassIs { .. } | Op::ClassWordOf => {
            if idx == 0 {
                TypeReq::Exact(IrType::Ref)
            } else {
                TypeReq::Any
            }
        }
        // `[ctrl]` — the opaque OSR merge's three ops (round 11 wave 14), and
        // the class-init test (round 19 wave 3, lane eager3).
        Op::OsrEntryFlag { .. }
        | Op::OsrLocal(_)
        | Op::OsrMemory
        | Op::ClassInitTest { .. }
        | Op::CurrentThreadMirror { .. }
        | Op::VthreadBoundaryTest { .. }
        // JIT round 20 wave 11 (lane pIndy): a root word read, like the mirror.
        | Op::IndyRootRef { .. }
        // JIT round 19 wave 5 (lane handler5, SPLICE4-1): the compiled
        // handler entry's flag and caught throwable, `[ctrl]` like the OSR arm.
        | Op::CatchEntryFlag { .. }
        | Op::CaughtException => {
            if idx == 0 {
                TypeReq::Control
            } else {
                TypeReq::Any
            }
        }
        // `[ctrl, ref]` — the fence's referent (nio20 L2).
        Op::KeepAlive => match idx {
            0 => TypeReq::Control,
            1 => TypeReq::Exact(IrType::Ref),
            _ => TypeReq::Any,
        },
        // `[ctrl, call]` — the call-site exception edge's flag (nio21 wave 3
        // L2; its `Op::CaughtException` is the `[ctrl]` arm above). The call
        // is an ordering edge of any type (a `void` call included), never a
        // value the op reads.
        Op::CallThrew { .. } => {
            if idx == 0 {
                TypeReq::Control
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
    /// The write is [`Op::StoreStatic`], which always takes the helper.
    LoadStatic {
        class_id: u32,
        field_index: u32,
        type_tag: u8,
        is_volatile: bool,
    },

    /// `putstatic` — write `value` to the static field at `field_index` of
    /// `class_id`. Inputs `[ctrl, mem, value]`; the node is the new memory
    /// token (type `Memory`), like `Op::Store`.
    ///
    /// Always lowered as a call to the tag's `helpers.putstatic_*`, the helper
    /// the single-pass `0xb3` arm calls: `set_static_shared` owns the SATB
    /// pre-barrier on the overwritten reference and the block's creation on
    /// first touch, and the helper runs `<clinit>` (reporting a failure as the
    /// `i64::MIN` sentinel with a pending exception). `value` is already
    /// narrowed for `Z/B/C/S`. A volatile write is followed by the StoreLoad
    /// fence in the lowering; the node's memory shape is `Opaque` either way.
    StoreStatic {
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
    ///
    /// `seen_cell` (JIT round 20, MHIFACE19-1; `0` everywhere else): the
    /// address of a `[AtomicU32; 2]` the artifact owns
    /// (`CompiledMethod::_jit_typecheck_seen_cells`): two write-once ways, each
    /// the target's own id until the helper answers `1` for a plain object
    /// class, which then fills the first way still empty. The lowering answers
    /// `1` inline for a receiver of exactly a way's class. Planted only by the
    /// interface constant handle's receiver guard
    /// (`IrBuilder::mh_constant_interface_receiver_guard`).
    InstanceOf {
        name_ptr: usize,
        name_len: usize,
        seen_cell: usize,
        /// JIT r21 wave 2 (lane typecheck, TYPECHECK21-1 part A,
        /// `CRATONVM_JIT_IR_INSTANCEOF_HEADER_READ`): the node is a READ of
        /// the operand's immutable header, not an opaque helper call, in the
        /// memory model ([`MemAccess::HeaderRead`]). `false` everywhere else,
        /// which keeps the `Opaque` classification byte for byte.
        header_only: bool,
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
    ///
    /// Round nio21 wave 1 (lane L2): under `CRATONVM_JIT_IR_LOCAL_THROW_EDGES`
    /// an `athrow` whose catch this method decides at compile time builds no
    /// `Throw` at all: it is an ordinary edge into the handler's `Merge`, like
    /// a `goto` (`IrBuilder::local_throw_target`). Every `Throw` that IS built
    /// still leaves the frame, so the statement above stands.
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

    /// Bulk non-overlapping `byte[]` copy. Inputs: `[ctrl, mem, src, src_pos, dst, dst_pos, len]`.
    ByteArrayCopy,

    /// JIT round 21, lane charat (STRINTR21-5,
    /// `CRATONVM_JIT_IR_STRING_CHAR_DECODE`): the compact-String char read of
    /// the `String.charAt` expansion. Inputs `[ctrl, mem, value, index,
    /// coder]`, or with the guard token `[.., guard]` (see
    /// [`Op::guard_shape`]); result `Int`, the char zero-extended.
    ///
    /// `coder == 0` reads the byte `value[index]`, anything else the
    /// little-endian UTF16 unit at byte `2 * index`: one `MOVZX` either way
    /// behind a `TEST coder`. It checks NOTHING. The builder emits it only
    /// after its receiver, `value` and `0 <= index < value.length >>> coder`
    /// guards, which prove both reads in bounds, and it is pinned to its
    /// control (`ir_verify::control_input_indices`) and impure, so it is never
    /// scheduled above them. It reads memory no Java store reaches (a String's
    /// `value` array is never written after construction) and does not advance
    /// the memory token, like [`Op::ArrayLength`].
    StrCharDecode,

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
    /// A BRIDGED `invokedynamic` (round 16, lane indy, SY4-1;
    /// `CRATONVM_JIT_IR_INDY_BRIDGE`). Inputs: `[ctrl, mem, args…]`, one input
    /// per descriptor argument; the result is typed from the descriptor's
    /// return (`void` sites are never bridged).
    ///
    /// `site` is the process-lived bridge-site pointer the VM's
    /// `cp_invokedynamic_descriptor_resolver` hands out
    /// (`JitStringConcatSite` / `JitIndyGenericSite`, self-describing through
    /// their `kind` tag), the same word the single-pass backend bakes. The
    /// lowering calls `DirectHelperTable::indy_bridge` -- the entry the
    /// single-pass `0xba` arm calls -- with the `jit_invoke_dispatch` ABI
    /// `(vm, site, args_ptr, n)`, so one helper serves both tiers and the
    /// two cannot disagree about what a site means.
    ///
    /// Shaped like `Op::Call` everywhere it matters: an `Opaque` memory
    /// effect and the new memory token, a GC point, a caller-saved clobber,
    /// and a fallible result (the bridge returns the `i64::MIN` sentinel with
    /// the throwable stashed when a bootstrap or an operand `toString()`
    /// throws), so it takes the shared call-exception edge at its bci.
    IndyBridge {
        site: usize,
    },
    /// `multianewarray` (round 16, lane matrix;
    /// `CRATONVM_JIT_IR_MULTIANEWARRAY`). Inputs `[ctrl, mem, dim0, …]`, one
    /// `int` per dimension the opcode names, OUTERMOST first (source order);
    /// the result is the new array, a `Ref`.
    ///
    /// `site` is the packed `(holder_class_id, cp_idx)` word
    /// (`crate::pack_multianewarray_site`) the single-pass `0xc5` arm bakes,
    /// and the lowering calls the same `multianewarray_n(vm, site, ndims,
    /// dims_ptr)` helper (`jit_multianewarray_n`), with the staged dimension
    /// words as the buffer, so both tiers resolve the same per-level classes
    /// through `interpreter::multianewarray_alloc`.
    ///
    /// Shaped like `Op::IndyBridge`: an `Opaque` memory effect and the new
    /// memory token (resolution can run a user class loader), a GC point, a
    /// caller-saved clobber. The helper answers `0` with the throwable
    /// stashed (`NegativeArraySizeException`, `OutOfMemoryError`, a failed
    /// resolution), which takes the shared call-exception edge at its bci.
    MultiNewArray {
        site: i64,
    },
    /// nio21 wave 15 (lane L3; `CRATONVM_JIT_IR_DEFERRED_NEW`, default ON): a
    /// `new` whose class was NOT loaded when this method compiled
    /// (`JitNewSite::Deferred`). Inputs `[ctrl, mem]`; the result is the new
    /// object, a `Ref`, and the node is the new memory token.
    ///
    /// Lowered as a call to `helpers.new_object_cp(vm, holder, cp_idx)` --
    /// the helper, ABI and shared emitter (`emit_new_object_cp_stub`) the
    /// single-pass `0xbb` arm uses for the same site, which resolves,
    /// initialises (`<clinit>`, arbitrary Java) and allocates exactly as the
    /// interpreter's `new` does, on the same thread at the same program point.
    /// It answers `0` with the throwable stashed (a failed resolution, a
    /// `<clinit>` failure, OOM), which takes the shared call-exception edge at
    /// its bci, `Op::MultiNewArray`'s convention.
    ///
    /// Shaped like `Op::MultiNewArray`: `Opaque`, a GC point, a caller-saved
    /// clobber, a DCE root (resolution can throw), never scalar-replaced (no
    /// layout is known at compile time). Never built inside a splice: the
    /// `(holder, cp_idx)` pair names the compiling method's own pool.
    ///
    /// Before it, the IR builder's `0xbb` arm bailed the WHOLE method on such
    /// a site, so `UTF_8$Encoder.encodeBufferLoop` -- whose only `new` is the
    /// cold `new Surrogate.Parser()` -- never left the single-pass tier while
    /// no surrogate had been encoded.
    NewCp {
        holder_class_id: u32,
        cp_idx: u16,
    },

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
    /// identity observation that blocks scalar replacement. Three passes
    /// delete monitors, each only as a balanced unit: `escape_analysis`'s
    /// lock elision (a whole object's plan at once — see
    /// `docs/jit/lock-elimination.md` §4 and §6.1), the EA coarsening
    /// consumer in `ea_ir_bridge` (`CRATONVM_JIT_LOCK_COARSEN=1`), and
    /// `ir_optimize::coarsen_adjacent_monitors` (an adjacent exit/enter
    /// pair on one reference node).
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

    /// Is `obj` an instance of EXACTLY the class `class_id`? Inputs `[obj]`,
    /// result `Int`: `1` when `obj` is non-null, a plain object (not an
    /// array) and its header class id is `class_id`; `0` otherwise.
    ///
    /// Round 11 wave 13 (lane `cha`, page
    /// `r11w12-calls-cha-sync-target-needs-a-class-guard-before-the-enter`):
    /// the condition of the `Op::Guard` a class-hierarchy-bound caller-held
    /// synchronized direct CALL takes BEFORE its `MonitorEnter`
    /// (`IrBuilder::enter_receiver_monitor`). A test rather than a guard of its
    /// own, so the failing edge is the ordinary `Op::Guard` deopt (REEXECUTE at
    /// the invoke), with every frame-state, OSR-exit and de-spec rule that
    /// already applies to it, instead of a second deopt producer.
    ///
    /// PURE and TOTAL: a null answers `0` without a load, an array answers `0`
    /// (its header carries its COMPONENT's class id, so the kind byte is
    /// tested first), and a class id never changes for an object's lifetime.
    /// So it may float, be GVN'd and be hoisted anywhere its input is defined,
    /// above a null check included. It emits no call and no deopt.
    ExactClassIs {
        class_id: u32,
    },
    /// JIT round 19 proposal wave (lane osr19, OSRPOLY19-2 / OSRFOLLOW18-2):
    /// the header class id of `obj`, or `0`. Inputs `[obj]`, result `Int`:
    /// the (sign-extended) 32-bit class word when `obj` is non-null and a
    /// plain object (not an array); `0` for a null and for an array (whose
    /// header names its COMPONENT's class). Every class id a test compares it
    /// with is non-zero, so `Cmp(Eq, ClassWordOf(r), Const(id))` answers
    /// exactly what `ExactClassIs { class_id: id }` answers for the same `r`.
    ///
    /// Emitted once at the head of a polymorphic guarded splice's test chain
    /// (`IrBuilder::begin_bimorphic_receiver_splice`) under
    /// `CRATONVM_JIT_IR_CLASS_WORD_CHAIN` (opt-in), so the chain loads the
    /// receiver's class word once instead of once per test. PURE and TOTAL
    /// for the same reasons as `ExactClassIs`: a null answers `0` without a
    /// load, and the class id and kind byte are fixed at allocation. It may
    /// float, be GVN'd and be hoisted anywhere its input is defined. It emits
    /// no call and no deopt.
    ClassWordOf,
    /// JIT round 19 wave 3 (lane eager3, EAGER2-3 of
    /// `jit-r19-eager2-proposals.md`; `CRATONVM_JIT_UNINIT_NEW_GUARD`, opt-in):
    /// is the class whose initialization word lives at `state_addr` (the
    /// `Class::init_state` byte the VM hands over, kept alive by the artifact,
    /// `CompiledMethod::_jit_class_init_cells`) initialized? Inputs `[ctrl]`,
    /// result `Int`: `1` when the byte reads `initialized`, else `0`.
    ///
    /// The condition of the `Op::Guard` an uninitialized `new` takes
    /// (`IrBuilder::plant_class_init_guard`): its failing edge is the ordinary
    /// guard deopt (REEXECUTE at the `new`, whose interpreter run initializes
    /// the class), its passing edge today's resolved `Op::New`. A body compiled
    /// before the class initialized therefore runs the allocation once the
    /// class is initialized by anyone, without a trap or a recompile.
    ///
    /// Pinned to its control input and NOT pure (never GVN'd), like
    /// `OsrLocal`: the word changes from "not initialized" to "initialized"
    /// while the body runs. A single byte load, no call, no deopt, total.
    ClassInitTest {
        state_addr: usize,
        initialized: u8,
    },
    /// nio21 wave 19 (lane L3, `r21-threadlocal-get-has-no-ir-intrinsic-FIXED-20261010.md`):
    /// the current thread's `java.lang.Thread` mirror -- the word
    /// `JvmThread::java_thread_obj` at byte `offset` from the `JvmThread*` the
    /// `JIT_THREAD` TLS mirror holds, exactly what the thin helper
    /// `jit_thread_current_thread_direct` returns on its fast arm. Inputs
    /// `[ctrl]`, result `Ref`; `null` outside a JIT thread context or before
    /// the thread has a mirror. Built only by the `ThreadLocal.get()`
    /// expansion (`IrBuilder::try_thread_local_get_intrinsic`), which tests it
    /// for null and sends a null to its call.
    ///
    /// Pinned to its control input and NOT pure, like `ClassInitTest`: the
    /// word is per thread, so it must not float across a point where the
    /// compiled code could change threads (it cannot today, but a pin costs
    /// nothing). No call, no deopt, total.
    CurrentThreadMirror {
        offset: u32,
    },
    /// True unless the current compiled thread is a JDK continuation.
    VthreadBoundaryTest { offset: u32, mask: u32 },
    /// JIT round 20 wave 11 (lane pIndy, SPLICE19-1;
    /// `CRATONVM_JIT_INDY_MUTABLE_GUARD`): the object a VM-maintained root
    /// word holds -- `[slot_addr]`, where `slot_addr` is the untagged box of a
    /// JNI global reference of this VM that is never released (the
    /// `compiled_ldc_slot_for` slot kind). Inputs `[ctrl]`, result `Ref`.
    /// Built only by the guarded `MutableCallSite` call
    /// (`IrBuilder::try_indy_mutable_guarded_call`), for the call site and
    /// the target handle it was planned against.
    ///
    /// The word is a GC root the collector rewrites in place while every
    /// mutator is parked, so the read is never stale; a reference held in a
    /// frame slot after it is published in the safepoint maps like any other
    /// `Ref` value. Pinned to its control input and NOT pure, like
    /// `CurrentThreadMirror`. One load, no call, no deopt, total.
    IndyRootRef {
        slot_addr: usize,
    },

    // ── The opaque OSR entry merge (round 11 wave 14, lane osrmerge) ──
    //
    // `r11w11-irexc-osr-entry-as-a-real-predecessor`, its `## Wave 13` design.
    // Behind `CRATONVM_JIT_OSR_OPAQUE_ENTRY` (default OFF,
    // `osr_exit::osr_opaque_entry_enabled`); no graph contains one of these
    // three with the switch off. For an OSR-open loop header the builder
    // (`IrBuilder::build_opaque_osr_merge`) hangs `If(OsrEntryFlag)` off the
    // header's forward entry and merges its `Proj(0)` arm with that entry in
    // a pre-header `Merge`, whose φs take `OsrLocal(i)` (or `NO_NODE` for a
    // local dead at the header) and whose memory φ takes `OsrMemory` on the
    // arm's edge. The optimizing OSR entry stub jumps to the arm instead of
    // seeding the header, so every fact at the header — value, range, type,
    // null-ness and memory — already includes "whatever the interpreter
    // brought", and LICM's pre-header (the `Merge`) runs on both entries.
    /// The condition of that `If`. Inputs `[ctrl]` (the header's forward entry
    /// control), result `Int`, always `0` when executed: the method-entry path
    /// never takes the arm, and the OSR stub jumps past the `If`. OPAQUE: not
    /// pure (never GVN'd), no range, never folded. `header_bci` names the loop
    /// header the arm enters, which is how `ir_lower::emit_osr_entry_stubs`
    /// finds the arm; it carries no bytecode pc of its own.
    OsrEntryFlag {
        header_bci: usize,
    },
    /// JVM local `i` as the interpreter holds it on OSR entry. Inputs `[ctrl]`
    /// (the arm's `Proj(0)`), typed from the header φ. A pinned, control-
    /// dependent read of the interpreter's locals array through the pointer
    /// the entry stub parks in the phi-copy scratch word: never hoisted, CSE'd
    /// or rematerialised, never a `Param` (the prologue homes, the receiver
    /// facts and the GC root set of a `Param` are all keyed on the op). Built
    /// only for a local LIVE at the header, so its type is the verifier's.
    OsrLocal(u16),
    /// The memory state the interpreter brings on OSR entry. Inputs `[ctrl]`
    /// (the arm's `Proj(0)`), type `Memory`, effect `MemEffect::OPAQUE`: no
    /// load is forwarded from, and no store is found dead across, the merge.
    /// Emits nothing.
    OsrMemory,

    // ── The compiled handler entry (JIT round 19 wave 5, lane handler5) ──
    //
    // SPLICE4-1, `CRATONVM_JIT_IR_COMPILED_HANDLER_ENTRY` (opt-in, default
    // OFF; no graph contains one of these two with the switch off). The
    // opaque-OSR-arm idea applied to an exception edge: right after a plain
    // `Op::Call` of the compiling method's own code whose `invoke` the
    // method's FIRST covering exception-table row catches, the builder
    // (`IrBuilder::note_catch_arm`) hangs `If(CatchEntryFlag)` off the call's
    // control, and the `If`'s `Proj(0)` arm enters the handler's merge with
    // the locals at the call and `[CaughtException]` as its operand stack.
    // Normal flow never takes the arm; the call's exceptional exit asks the
    // VM's local-handler lookup and, on a hit, JUMPS to the arm's block
    // (`ir_lower::Lowerer::emit_catch_probes`). A miss, or any exit the
    // lowerer could not prove enterable, takes the exit the call always took.
    /// The condition of that `If`. Inputs `[ctrl]` (the call's control),
    /// result `Int`, always `0` when executed. OPAQUE: not pure, never folded
    /// by the optimizer (the lowerer reads it as a constant false edge).
    /// `call_bci` is the `invoke` pc (the call node's own `bytecode_pc`), which
    /// is how the lowerer pairs the `If` with the call scheduled in the same
    /// block; `site` indexes [`Graph::catch_arm_sites`].
    CatchEntryFlag {
        call_bci: u32,
        site: u16,
    },
    /// The throwable the arm's handler catches. Inputs `[ctrl]` (the arm's
    /// `Proj(0)`), type `Ref`. A pinned read of the frame word the lookup
    /// parked the taken exception in (the phi-copy scratch word), which it
    /// clears; the lowerer enters the arm only when this is the arm block's
    /// first node.
    ///
    /// Shared with nio21 wave 3 L2's call-site exception edges
    /// ([`Op::CallThrew`], `CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES`): there
    /// the `ctrl` is the `Proj(0)` of an `If(CallThrew)` and the frame word is
    /// the one `jit_local_handler_lookup` stored the taken throwable into; the
    /// lowerer arms that edge only when nothing before this node in its block
    /// can call, trap or reach a safepoint. One op, one lowering: the two
    /// features merged independently with the same read-and-clear semantics.
    CaughtException,

    // ── Keep-alive (nio20 wave 3, lane L2; NIO19-9) ─────────────────
    /// `Reference.reachabilityFence(ref)`: keeps `ref` LIVE up to this point
    /// and does nothing else. Inputs `[ctrl, ref]`, type `Void`, no memory
    /// token in or out.
    ///
    /// HotSpot emits no code for the fence; its whole contract is that the
    /// referent stays strongly reachable (in every GC map) until here. This
    /// node is that contract and nothing more:
    ///
    /// * a USE of `ref` -- liveness is computed from the graph, so `ref`'s
    ///   register or home stays allocated and in every safepoint map up to
    ///   this node;
    /// * pinned to its control (`ir_verify::control_input_indices` `0..1`)
    ///   and IMPURE (not `is_pure`), so the scheduler keeps it in the
    ///   block's side-effect chain at its program position -- after every
    ///   call or safepoint that precedes the fence -- and never floats or
    ///   GVNs it;
    /// * a DCE root (`ir_optimize::is_dce_root`), so nothing removes it;
    /// * NO memory effect (`memory_shape_of` = `None`), not a safepoint, no
    ///   frame state, cannot raise: it is no barrier to LICM, load reuse or
    ///   store motion, which is what the `Op::Call` it replaces was;
    /// * lowered to no code.
    ///
    /// Built by the IR builder in place of the call to
    /// `java/lang/ref/Reference.reachabilityFence(Ljava/lang/Object;)V`
    /// (`CRATONVM_JIT_IR_REACHABILITY_FENCE_KEEPALIVE`, default on).
    KeepAlive,

    // ── Call-site exception edges (nio21 wave 3, lane L2) ───────────
    /// "The call before me threw, and candidate `index` of exception site
    /// `site` of this method takes it." Inputs `[ctrl, call]`, result `Int`.
    ///
    /// Built only under `CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES`
    /// ([`ir_call_site_exception_edges_enabled`], default OFF) by
    /// `IrBuilder::plant_call_throw_edges`, right after an `Op::Call` inside a
    /// `try` whose handlers lie after it: one `CallThrew` + `Op::If` per
    /// candidate row of [`Graph::call_throw_sites`]`[site]`, chained on the
    /// false edges. The true edge of `If(CallThrew { index: k })` carries an
    /// [`Op::CaughtException`] into handler `k`'s merge; the last false edge is
    /// the call's normal continuation.
    ///
    /// The value is `0` wherever it executes. The true edge is never taken by
    /// a branch: `ir_lower` lowers the `If` as a plain transfer to its false
    /// edge, and the call's out-of-line exception pad
    /// (`Lowerer::emit_call_throw_pads`) asks `jit_local_handler_lookup` and
    /// JUMPS into the true edge's block of the candidate it answered. The pad
    /// is entered only from the `i64::MIN` sentinel edge after the J/D/F
    /// `dispatch_threw` peek and the callee-deopt service, and the helper
    /// itself answers `-1` for a deopt, a stashed IR-deopt frame or nothing
    /// pending, so the sentinel's three meanings are told apart on the cold
    /// side only; `-1` takes today's exit unchanged.
    ///
    /// The `call` input is an ORDERING edge (it keeps the flag after its call
    /// in the call's block), never a value read. Opaque:
    /// not pure, no memory shape, cannot raise, never folded; kept by its
    /// `If` (not a DCE root).
    CallThrew {
        site: u32,
        index: u16,
    },
    // The throwable a call-site exception edge carries into its handler is
    // the shared [`Op::CaughtException`] (declared with `CatchEntryFlag`
    // above; inputs `[ctrl]`, the `Proj(0)` of an `If(CallThrew)`).

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
                // Total and a function of its one input: the header class id
                // is immutable (see the variant).
                | Op::ExactClassIs { .. }
                // JIT round 19 (lane osr19): the same header read, answered as
                // the class word itself.
                | Op::ClassWordOf
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
    // `span` is at most `SWITCH_MAX_SPAN` here (2047 — this comment said "255"
    // until 2026-09-20, from the days when the budget was `u8::MAX + 1`), so
    // neither the widening nor the multiplication can overflow. The
    // `saturating_mul` guards `cases`, which is a decoded payload count and is
    // not bounded by anything on this side.
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
    // widening cannot overflow, and `span` is already known to be at most
    // `SWITCH_MAX_SPAN` (2047 — this comment said "255" until 2026-09-20, from
    // the days when the budget was `u8::MAX + 1`) by the test above. The
    // doubling is `saturating_mul` because `cases` is not bounded here.
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
    pub(in crate::ir) static EDGE_EPOCH: Cell<u64> = Cell::new(1);

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
    pub(in crate::ir) static THREAD_TOKEN: u64 = NEXT_THREAD_TOKEN.fetch_add(1, Ordering::Relaxed);
}

/// Source of the per-thread tokens. Never 0, so `UseLists::new()` (owner 0)
/// can never be mistaken for a reconciled state.
pub(in crate::ir) static NEXT_THREAD_TOKEN: AtomicU64 = AtomicU64::new(1);

/// Note an untracked edge write. See [`EDGE_EPOCH`].
#[inline]
pub(in crate::ir) fn bump_edge_epoch() {
    EDGE_EPOCH.with(|e| e.set(e.get().wrapping_add(1)));
}

/// The current edge epoch. See [`EDGE_EPOCH`].
#[inline]
pub(in crate::ir) fn edge_epoch() -> u64 {
    EDGE_EPOCH.with(|e| e.get())
}

/// This thread's identity. See [`THREAD_TOKEN`].
#[inline]
pub(in crate::ir) fn thread_token() -> u64 {
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
pub struct Inputs(pub(in crate::ir) InputsRepr);

#[derive(Clone)]
pub(in crate::ir) enum InputsRepr {
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
    pub(in crate::ir) fn slots_mut(&mut self) -> &mut [NodeId] {
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

    pub(in crate::ir) fn push_untracked(&mut self, id: NodeId) {
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

    pub(in crate::ir) fn pop_untracked(&mut self) -> Option<NodeId> {
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

    pub(in crate::ir) fn clear_untracked(&mut self) {
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
    pub(in crate::ir) fn swap_remove_first_untracked(&mut self, val: NodeId) -> bool {
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
/// `docs/feature-designs/jit-ir-optimizations.md`): `ir_lower` emits
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
/// `docs/feature-designs/jit-ir-optimizations.md`: flag row, this reader,
/// and the `lib.rs` routing test, together) landed in wave 7b (`wire7b`).
pub fn ir_volatile_fields_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE` (JIT round 21 wave 13, lane cf;
/// perf, **default ON** since JIT r24 with the CompletableFuture handler
/// family, `=0` off): a SPLICED body may keep a volatile
/// `putfield`. The VM resolver refused every such body in both tiers
/// (`splice-volatile-field-store`) on the note "the optimizing tier has no
/// fenced spliced store", which stopped being true when the builder's
/// `putfield` arm started building any pc in `volatile_field_pcs` as an
/// ordered `Op::Store` and `ir_lower` started fencing every
/// `is_volatile_store()` node: a spliced pc arrives through
/// [`IrBuilder::add_spliced_volatile_field_pcs`], which sets the store
/// admission bit exactly as [`IrBuilder::set_volatile_field_pcs`] does.
/// Optimizing tier only, and only with [`ir_volatile_fields_enabled`] and
/// [`IR_LOWER_FENCES_VOLATILE_STORES`]; the single-pass inliner (plain store,
/// no fence) keeps the refusal. A store into an object the splice did not
/// allocate is a committed store (`CRATONVM_JIT_IR_SPLICE_COMMITTED_STORE`),
/// so no deopt re-executes it. Row: `CompletableFuture.uniApplyNow`'s
/// `d.result = d.encodeValue(f.apply(t))`. Read per splice decision.
pub fn ir_splice_volatile_store_enabled() -> bool {
    IR_LOWER_FENCES_VOLATILE_STORES
        && ir_volatile_fields_enabled()
}

/// `CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE_FRESH` (JIT round 23 wave 11b,
/// lane L5, CompletableFuture page; perf, **default ON** since JIT r24, `=0`
/// off): the narrow form
/// of [`ir_splice_volatile_store_enabled`]. A spliced body may keep a
/// volatile `putfield` only into an object that is provably still private
/// to the thread when the store runs:
/// - the receiver is an `Op::New` of this compile;
/// - L2's backward walk from the store to that allocation's memory state
///   finds nothing that publishes it ([`vh_fresh_receiver_publication`]).
///
/// Anything else bails the build at that pc, and the planner rebuilds
/// without the site. Such a store is invisible to every other thread until
/// the object is published, so its volatile ordering is vacuous; it is built
/// as the ordered `Op::Store`, and `ir_lower` may drop its fence
/// (`CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED`).
///
/// The row is `CompletableFuture.uniApplyNow`'s `d.result = ..`, where `d`
/// is `newIncompleteFuture()`'s `new CompletableFuture<>()` once that call
/// splices. When the general switch is on, it decides and this one adds
/// nothing. Same admission prerequisites as the general switch. Read per
/// splice decision and per spliced volatile store.
pub fn ir_splice_volatile_store_fresh_enabled() -> bool {
    IR_LOWER_FENCES_VOLATILE_STORES
        && ir_volatile_fields_enabled()
}

/// May a spliced body keep a volatile `putfield` at all? Under either switch
/// ([`ir_splice_volatile_store_enabled`] or
/// [`ir_splice_volatile_store_fresh_enabled`]). The resolver's and the
/// planner's admission.
pub fn ir_splice_volatile_store_admitted() -> bool {
    ir_splice_volatile_store_enabled() || ir_splice_volatile_store_fresh_enabled()
}

/// Must the builder prove a spliced volatile `putfield`'s receiver fresh?
/// Exactly when the fresh switch admitted the store and the general one did
/// not.
pub fn ir_splice_volatile_store_needs_fresh_receiver() -> bool {
    !ir_splice_volatile_store_enabled() && ir_splice_volatile_store_fresh_enabled()
}

/// The builder's rule for one spliced volatile `putfield` under
/// [`ir_splice_volatile_store_needs_fresh_receiver`]: `None` (admitted) when
/// `base` is an `Op::New` and the walk from `mem` back to its allocation
/// finds no publication; otherwise the reason it is refused.
pub(crate) fn spliced_volatile_store_fresh_refusal(
    graph: &Graph,
    mem: NodeId,
    base: NodeId,
) -> Option<String> {
    // JIT r23 wave 13 (lane L5): a base that is a `checkcast` of the
    // allocation (or of such a cast) is the allocation, re-typed
    // ([`ir_fresh_walk_typecheck_steps_enabled`]).
    let mut alloc = base;
    if ir_fresh_walk_typecheck_steps_enabled() {
        let mut hops = 0;
        while let Some(n) = graph.node_opt(alloc) {
            if !matches!(n.op, Op::CheckCast { .. }) || hops >= 8 {
                break;
            }
            match n.inputs.get(2) {
                Some(&obj) if obj != NO_NODE => alloc = obj,
                _ => break,
            }
            hops += 1;
        }
    }
    if !matches!(graph.node_opt(alloc).map(|n| &n.op), Some(Op::New { .. })) {
        return Some("the receiver is not an allocation of this compile".to_string());
    }
    vh_fresh_receiver_publication(graph, mem, alloc)
}

/// `CRATONVM_JIT_IR_FRESH_WALK_TYPECHECK_STEPS` (JIT round 23 wave 13, lane
/// L5, CompletableFuture page; perf, **default ON**, `=0` restores the wave-11
/// walk). It widens the fresh-receiver publication walk
/// ([`vh_fresh_receiver_publication`]) that the opt-in consumers ask:
/// `CRATONVM_JIT_IR_VH_CONST_FRESH_FIELD` and
/// `CRATONVM_JIT_IR_SPLICE_VOLATILE_STORE_FRESH`. It changes nothing else.
///
/// - A type test on the memory chain (`Op::CheckCast` / `Op::InstanceOf`,
///   `[ctrl, mem, obj]`, [`fresh_walk_type_test`]) is a memory step. Its
///   helper reads the tested object's header and class and stores nothing:
///   no heap field, no static, no thread-visible location. It does not
///   publish the object it tests, even when that object is the allocation or
///   an alias of it.
/// - A load or store whose BASE is an alias of the allocation (its
///   `checkcast`) is an access to the allocation's own field, as one based on
///   the allocation itself is.
/// - [`spliced_volatile_store_fresh_refusal`] peels `checkcast`s off a store
///   base to find the `Op::New`.
///
/// It is sound because the walk only answers "has any thread but this one been
/// handed the object yet?". A cast that throws `ClassCastException` (the
/// helper stashes it and returns the sentinel, which leaves through the
/// exception epilogue) happens BEFORE the store in program order, and the
/// walk moves nothing. The consumer's store keeps its place on the memory
/// chain after the cast, so a throwing cast still means the store never ran,
/// and the object is still private when the frame unwinds. No reordering is
/// introduced: only the store's ordering fence is dropped
/// (`CRATONVM_JIT_IR_FRESH_VOLATILE_STORE_UNFENCED`), and the fence orders a
/// store against OTHER threads, which cannot see this object yet. The helper may reach a safepoint
/// (class loading, the exception's allocation); a collector seeing the
/// object through this frame's roots is not publication to Java code. Read
/// per walk; no `static`.
pub fn ir_fresh_walk_typecheck_steps_enabled() -> bool {
    true
}

/// The type-test nodes the fresh-receiver walk passes through
/// ([`ir_fresh_walk_typecheck_steps_enabled`]): `checkcast` and `instanceof`.
pub(crate) fn fresh_walk_type_test(op: &Op) -> bool {
    matches!(op, Op::CheckCast { .. } | Op::InstanceOf { .. })
}

/// What `append_ir_inline_site` makes of one spliced volatile row: the opcode
/// at the callee pc the resolver recorded. A `getfield` is always admitted
/// (round 9 wave 10); a `putfield` only under
/// [`ir_splice_volatile_store_enabled`] (passed in, so the rule is testable
/// without flag state); anything else names a row the resolver never makes.
pub fn spliced_volatile_row_admitted(
    op: Option<u8>,
    stores_admitted: bool,
) -> Result<(), &'static str> {
    match op {
        Some(0xb4) => Ok(()),
        Some(0xb5) if stores_admitted => Ok(()),
        Some(0xb5) => Err("ir-splice-volatile-row-is-a-putfield"),
        _ => Err("ir-splice-volatile-row-not-a-getfield"),
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
    /// The builder's abstract MONITOR stack at this bci, outermost first: one
    /// entry per `monitorenter` that has executed and whose `monitorexit` has
    /// not, naming the locked reference. A re-entrant lock on one object is two
    /// entries; `ir_lower` coalesces them into one `deopt::MonitorInfo` whose
    /// `lock_depth` is the count.
    ///
    /// A slot here is a node reference exactly like a local: it follows a value
    /// through [`Graph::replace_all_uses`] ([`SafepointSlotKind::Monitor`]), it
    /// is a DCE root, and it pins its value's home. Whether escape analysis
    /// ELIDED an object's locks is not recorded here -- that elision is
    /// all-or-nothing per object, and the lowerer recovers it from whether any
    /// live monitor op still names the object. The one per-ENTRY elision,
    /// nested-lock elision, is recorded in [`Self::monitor_elided_mask`].
    ///
    /// Before 2026-09-12 the builder kept no monitor stack, so every lowered
    /// `FrameState` claimed "holds no lock" by construction
    /// (`ir-frame-states-carry-no-monitor-stack-FIXED-20260912.md`).
    pub monitors: Vec<NodeId>,
    /// Bit `k` set: the region [`Self::monitors`]`[k]` names was ELIDED by
    /// `ir_optimize::elide_nested_monitors` (round 11 wave 14, lane fib) --
    /// it is a re-entrant region on an object an OUTER entry of this same
    /// snapshot already holds, and its `monitorenter`/`monitorexit` were
    /// deleted, so the compiled code holds that level zero times. `ir_lower`'s
    /// `resolve_monitors` then describes the object as two entries: the held
    /// levels (`relock == false`) and the elided ones (`relock == true`).
    ///
    /// Positional, so it survives [`Graph::replace_all_uses`] rewriting the
    /// slots, and a snapshot is only ever moved or cloned whole. `0` from
    /// [`Self::new`], which is "every entry is a lock the compiled code took"
    /// -- the only claim a producer that never heard of the pass can make.
    /// Entries past 63 cannot be marked; the pass refuses such an object.
    pub monitor_elided_mask: u64,
    /// Round 13 wave 3 (lane framestate, W5-3 stage 1): `Some` exactly for a
    /// snapshot the builder recorded INSIDE a spliced callee body
    /// ([`ir_splice_frame_states_enabled`]). Its `bci` is then the
    /// combined-buffer pc, its `locals` the callee's, and its `stack` the
    /// callee's operand stack followed by every enclosing scope's frame -- see
    /// [`SpliceScopeChain`] for the layout. `None` from [`Self::new`], and on
    /// every snapshot of a build with the switch off.
    ///
    /// A snapshot carrying it is NOT a frame of the compiling method, so every
    /// consumer that resolves a frame BY BCI skips it
    /// ([`Self::is_splice_state`]); only `ir_lower`'s chain resolution reads
    /// it, and a node that names one ([`Node::frame_snapshot`], an unrolled
    /// copy) resolves through that path too.
    pub splice_scope: Option<std::sync::Arc<SpliceScopeChain>>,
    /// Interpreter round i1 wave 47, lane L3
    /// (`i39-L3-proposal-post-call-exits-inside-a-splice`): the real `Op::Call`
    /// made INSIDE a spliced body whose return this chain snapshot describes --
    /// the builder recorded it at the call's successor, the call's result on
    /// the innermost scope's stack ([`ir_post_call_splice_exits_enabled`]).
    /// `ir_lower::post_call_exit_state` turns it into the chain the post-call
    /// exit of that call leaves through. [`NO_NODE`] on every other snapshot.
    pub post_call_of: NodeId,
    /// With [`Self::post_call_of`]: the snapshot exists for the post-call exit
    /// ALONE, so every consumer that looks a chain snapshot up by its pc (a
    /// guard's chain, the fence relaxations, the duplicate-bci check, LICM)
    /// skips it and answers exactly as it did without it. `false` otherwise.
    pub post_call_only: bool,
    /// JIT r23 w2c close: a frame an unroll APPENDED for one copy of a loop
    /// body (`ir_optimize::install_copy_frames`). Once no live node names it
    /// (a later pass removed that copy's nodes) it describes nothing, and
    /// `ir_optimize::drop_unnamed_copy_frames` drops it. `false` otherwise.
    pub copy_frame: bool,
    /// A loop-entry predicate moved this frame before its original header.
    /// Allocations directly named by its slots must remain concrete: virtual
    /// recipes are currently described against a BCI, not this moved control.
    /// Escape analysis must retain their allocation, stores and object identity.
    /// False for builder snapshots; set only after loop-entry slot validation.
    pub preserve_allocations: bool,
    /// Virtual `java/lang/StringBuilder` instances accumulated across fused
    /// `append` steps when a call operand may deopt mid-chain.
    pub sb_virtual_builders: Vec<SbVirtualBuilder>,
}

/// One segment of a virtual `StringBuilder` captured for deoptimization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SbVirtualPart {
    Literal {
        text: Vec<u16>,
    },
    Value {
        kind: crate::sb_chain_fuse::SbAppendKind,
        val: NodeId,
    },
}

/// Virtual-object state for an elided `StringBuilder` chain at a safepoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SbVirtualBuilder {
    pub id: u32,
    pub local_slot: Option<usize>,
    pub stack_slot: Option<usize>,
    pub parts: Vec<SbVirtualPart>,
}

impl SafepointSnapshot {
    /// A frame state at `bci` with the given slot vectors.
    ///
    /// Round 11 wave 6, step 1 of making the snapshot slots epoch-tracked
    /// (`r11-irback-use-list-invariant-enforced-by-convention-only`): once
    /// every site builds a snapshot through this rather than a struct literal,
    /// the slot fields' type can change in this file alone.
    pub fn new(bci: usize, locals: Vec<NodeId>, stack: Vec<NodeId>, monitors: Vec<NodeId>) -> Self {
        SafepointSnapshot {
            bci,
            locals,
            stack,
            monitors,
            monitor_elided_mask: 0,
            splice_scope: None,
            post_call_of: NO_NODE,
            post_call_only: false,
            copy_frame: false,
            preserve_allocations: false,
            sb_virtual_builders: Vec::new(),
        }
    }

    pub fn sb_virtual_nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.sb_virtual_builders
            .iter()
            .flat_map(|b| b.parts.iter())
            .filter_map(|p| match p {
                SbVirtualPart::Value { val, .. } => Some(*val),
                SbVirtualPart::Literal { .. } => None,
            })
    }

    pub(crate) fn sb_virtual_slot(&self, mut slot: usize) -> Option<NodeId> {
        self.sb_virtual_nodes().nth(slot)
    }

    pub(crate) fn sb_virtual_slot_mut(&mut self, mut slot: usize) -> Option<&mut NodeId> {
        for b in &mut self.sb_virtual_builders {
            for p in &mut b.parts {
                if let SbVirtualPart::Value { val, .. } = p {
                    if slot == 0 {
                        return Some(val);
                    }
                    slot -= 1;
                }
            }
        }
        None
    }

    /// Is this a frame state recorded inside a spliced callee body (it carries
    /// a [`SpliceScopeChain`]) rather than a frame of the compiling method?
    #[inline]
    pub fn is_splice_state(&self) -> bool {
        self.splice_scope.is_some()
    }

    /// [`Self::is_splice_state`], but not a snapshot recorded for a post-call
    /// exit alone ([`Self::post_call_only`]): the chain snapshots a guard, a
    /// fence relaxation or a by-pc check may consult. Interpreter round i1
    /// wave 47, lane L3.
    #[inline]
    pub fn is_guard_splice_state(&self) -> bool {
        self.splice_scope.is_some() && !self.post_call_only
    }

    /// Was the region of monitor entry `k` elided as a nested lock? See
    /// [`Self::monitor_elided_mask`]. `false` for an index past the mask.
    #[inline]
    pub fn monitor_elided(&self, k: usize) -> bool {
        k < 64 && (self.monitor_elided_mask >> k) & 1 == 1
    }

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

/// One interpreter frame of a [`SpliceScopeChain`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpliceScopeFrame {
    /// `ClassId` of the class [`Self::method_key`] names, or `0` for the compiling method.
    pub class_id: u32,
    /// `"class/Name.method:descriptor"` of the method this scope runs
    /// ([`IrInlineSite::method_key`]). EMPTY for the outermost scope, the
    /// compiling method, whose key the lowerer takes from
    /// [`Graph::scope_method_key`].
    pub method_key: String,
    /// Bytecode index in THIS scope's own code. Scope 0: the instruction the
    /// snapshot was taken at. Every other scope: the `invoke` it is parked in,
    /// which a deopt resumes AFTER (`deopt::ResumeSemantics::for_caller_scope`).
    pub bci: u32,
    /// This scope's `Code.code_length`, for the install verifier's per-scope
    /// limits. `0` for the compiling method (unknown to the builder; the
    /// lowerer registers that scope saturated).
    pub code_len: u32,
    /// This scope's `max_locals`, same use; `0` for the compiling method.
    pub max_locals: u32,
    /// How many entries of [`SafepointSnapshot::stack`] hold this scope's
    /// locals. `0` for scope 0, whose locals are [`SafepointSnapshot::locals`].
    pub locals_len: u32,
    /// How many entries of [`SafepointSnapshot::stack`] hold this scope's
    /// operand stack.
    pub stack_len: u32,
}

/// Round 13 wave 3 (lane framestate, W5-3 stage 1): the chain of interpreter
/// frames a snapshot taken INSIDE a spliced body describes, innermost first.
///
/// `scopes[0]` is the spliced callee the snapshot was taken in, `scopes[1]`
/// the method whose `invoke` spliced it (another spliced body, or the compiling
/// method), and so on out to the compiling method, which is always the last.
///
/// # Layout
///
/// Scope 0's locals are the snapshot's `locals`. Everything else is laid out,
/// in scope order, in the snapshot's `stack`: scope 0's operand stack
/// (`stack_len` entries), then for each further scope its locals (`locals_len`)
/// followed by its operand stack (`stack_len`, the call's arguments already
/// popped). The lengths must add up to `stack.len()` exactly;
/// [`Self::split`] refuses anything else.
///
/// The caller frames ride in `stack` on purpose. Every pass that renames, roots,
/// prunes or keeps alive a snapshot slot (`Graph::replace_all_uses`, DCE,
/// liveness and slot pinning, unrolling's per-copy frames, EA's rewrites)
/// already walks `stack`, so the caller values stay described and kept alive
/// with no pass taught anything new. Only a consumer that reads a snapshot as
/// ONE interpreter frame has to know, and those skip a snapshot that carries
/// this chain ([`SafepointSnapshot::is_splice_state`]).
///
/// Monitors: a spliced body never holds one (the resolver admits no
/// `monitorenter` and no synchronized callee, and the builder refuses one), so
/// the snapshot's `monitors` are all the COMPILING method's, and belong to the
/// last scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpliceScopeChain {
    /// Innermost first; never empty in a chain the builder records (at least
    /// the callee and the compiling method).
    pub scopes: Vec<SpliceScopeFrame>,
}

impl SpliceScopeChain {
    /// Split a snapshot's resolved (or raw) `locals` / `stack` into one
    /// `(locals, stack)` pair per scope, innermost first, per the layout on
    /// [`SpliceScopeChain`]. `None` when the lengths do not add up, which is a
    /// malformed chain: a consumer must then fall back or refuse, never guess.
    pub fn split<'a, T>(&self, locals: &'a [T], stack: &'a [T]) -> Option<Vec<(&'a [T], &'a [T])>> {
        let first = self.scopes.first()?;
        if first.locals_len != 0 {
            return None;
        }
        let mut out: Vec<(&'a [T], &'a [T])> = Vec::with_capacity(self.scopes.len());
        let mut at = 0usize;
        for (i, scope) in self.scopes.iter().enumerate() {
            let locals_len = usize::try_from(scope.locals_len).ok()?;
            let stack_len = usize::try_from(scope.stack_len).ok()?;
            let scope_locals: &'a [T] = if i == 0 {
                locals
            } else {
                let end = at.checked_add(locals_len)?;
                let s = stack.get(at..end)?;
                at = end;
                s
            };
            let end = at.checked_add(stack_len)?;
            let scope_stack = stack.get(at..end)?;
            at = end;
            out.push((scope_locals, scope_stack));
        }
        (at == stack.len()).then_some(out)
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
// `InlineScopeTable` was that missing upstream record, as a side table
// alongside the graph. It was DELETED in round 10 wave 9 — see the retirement
// note above [`MAX_INLINE_SCOPE_DEPTH`] for the argument — because nothing ever
// populated it: `ir_lower` read it on the production path and produced
// `caller: None` on every compile, which is what `resolve_frame_state` now says
// outright instead.
//
// The situation it describes is unchanged, and this note stays for the reader
// who arrives at a spliced body wondering where the callee's frame identity
// went. `IrBuilder::build` walks ONE method's bytecode; when it splices
// ([`IrInlineSite`]) the spliced region carries the CALLER's `invoke` bci with
// re-execute semantics rather than a scope chain — deliberately, not by
// omission, because re-execution needs no frame identity while a chain needs
// the innermost frame's, which `ir_lower::resolve_frame_state` leaves for the
// VM to fill from the running `CompiledMethod` (i.e. it would name the
// caller). The reasoning is written out at [`IrInlineSite`].
//
// So this tier's deopt frames are flat, by construction and by intent. A
// producer that wants chains has to decide the identity question first; a side
// table is the easy half and was not the missing one.
//
// Round 13 wave 3 (lane framestate) decided it, behind
// `CRATONVM_JIT_IR_SPLICE_FRAME_STATES` (default off): each spliced scope's
// identity is its `IrInlineSite::method_key`, the outermost scope's is
// `Graph::scope_method_key` (set by the planner), and the builder records a
// [`SpliceScopeChain`] snapshot at every spliced instruction. With the switch
// off, everything below still holds word for word.

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
// No flat safepoint snapshot is pushed inside a splice, so a combined-buffer pc
// never reaches `build_deopt_points`. (Under `CRATONVM_JIT_IR_SPLICE_FRAME_STATES`
// a CHAIN snapshot is, and `build_deopt_points` skips it; a guard publishes it
// as a `FrameState::caller` chain instead of the flat re-execute frame. See
// [`SpliceScopeChain`].)
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

/// The builder's splice-depth cap for this compile: [`MAX_IR_SPLICE_DEPTH`],
/// or [`crate::rx27_deep_spine::SPLICE_DEPTH`] (12) under
/// `CRATONVM_JIT_IR_CHAIN_DEEP_SPINE` (round rx27 wave 4; default ON since 2026-10-29), whose
/// resume budget ([`crate::deopt::max_osr_inline_resume_depth`]) rises with
/// it. The resolver plans that deep only inside a dispatch-chain splice tree
/// (`r25_w7b_chain_splice::default_nest_depth_in`) unless an explicit
/// `CRATONVM_JIT_IR_INLINE_MAX_NEST` asks for it. Read once per builder.
pub fn ir_max_splice_depth() -> usize {
    if crate::rx27_deep_spine::enabled() {
        crate::rx27_deep_spine::SPLICE_DEPTH
    } else {
        MAX_IR_SPLICE_DEPTH
    }
}

// ── Synchronized splices (round 14 wave 2, lane syncsplice, SR-1) ────────
//
// A `synchronized` callee is spliced as
//
// ```text
// [Guard(recv != null)]            // instance: `begin_splice`'s JVMS 6.5 guard
// m = recv | ConstClass(mirror)    // at the invoke's pc
// MonitorEnter m                   // at the invoke's pc
// <the callee body>
// MonitorExit m                    // at the invoke's pc
// ```
//
// and `m` is NOT pushed on the builder's monitor stack: no frame state names
// it. That is sound only because nothing between the enter and the exit can
// transfer to the interpreter or raise -- a deopt there would re-execute the
// `invoke` (which locks again) with the compiled frame's lock leaked, and an
// exception would leave the frame holding it. Exactly the caller-held
// synchronized CALL's contract (`enter_receiver_monitor`), with the callee's
// body in place of the CALL. Three gates enforce it, each fail-closed:
//
// 1. the planner admits only a trap-free body (`lib.rs`
//    `ir_sync_splice_site_refusal`: no call (but a `Thread.holdsLock(this)`
//    the builder folds to `1`, SS-7, and -- round 14 wave 6, SS8-1 a -- a
//    same-receiver synchronized body spliced with no pair of its own, held
//    to these same rules), allocation, array access, cast,
//    division, back edge, `athrow` or monitor; field accesses only through the
//    receiver, statics only of the method's own class), resolved statically
//    for the compiling method's own site; a static callee's site must also
//    carry the caller-held route's `sync_direct_calls` row, whose class-mirror
//    materialisation the splice reuses;
// 2. the builder checks every node the body built
//    ([`sync_splice_window_node_admitted`]) before it emits the exit, and
//    refuses the SITE (a rebuild without it) otherwise;
// 3. `ir_lower` checks the FINAL schedule between the two
//    (`Lowerer::sync_splice_window_refusal`) and refuses the compile when
//    anything that can trap was scheduled into the window.
//
// The enter's own exits (a contended park, the NPE/OOM sentinel) happen with
// the monitor NOT yet held and resume or raise at the whole `invoke`, as the
// caller-held route's do. A field access through the receiver keeps a
// null-receiver exit in its lowering that cannot fire: the receiver was just
// locked. JMM: the pair is the same acquire/release a `monitorenter` /
// `monitorexit` builds, with the roach-motel walls every monitor op has.

/// `CRATONVM_JIT_IR_SYNC_SPLICE` -- default ON; `0|false|off|no` restores the
/// resolver's `synchronized` refusal (no synchronized body is spliced, the
/// site keeps its caller-held CALL or dispatch). Read per compile.
pub fn ir_sync_splice_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SYNC_SPLICE_IN_REGION` -- default ON (round 14 wave 3,
/// lane sync, proposal SS-3): a synchronized splice may sit inside a region
/// the compiling method itself holds (`synchronized (o) { o.leaf(); }`), and
/// a method with monitor bytecode is planned for them. Sound since
/// `ir_optimize::elide_nested_monitors` skips the window's frameless pair by
/// node and `coarsenable_monitor_pair` refuses to merge a window half with a
/// bytecode monitor op. `0|false|off|no` restores wave 2 (such methods keep
/// every synchronized call). Read per compile.
pub fn ir_sync_splice_in_region_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_HOLDSLOCK_FOLD` -- default ON (round 14 wave 4, lane
/// sync4, proposal SS-7): `Thread.holdsLock(o)` is built as the constant `1`
/// where the builder can prove this thread holds `o` -- `o` is the monitor
/// node of an open synchronized splice, or an entry of the compiling
/// method's own monitor stack ([`IrBuilder::try_fold_holds_lock`]). The
/// planner then admits a synchronized body whose only call is
/// `holdsLock(this)` (`lib.rs` `ir_sync_splice_body_scan`). `0|false|off|no`
/// keeps the native call everywhere, and such a body is refused again.
/// Read per matching site, at compile time.
pub fn ir_holds_lock_fold_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SYNC_METHOD_MONITOR_FACTS` -- default ON (round 14 wave 5,
/// lane sync5, proposal SS8-3): the optimizing compile of an
/// `ACC_SYNCHRONIZED` instance method records that its receiver's monitor is
/// held for the whole body ([`Graph::method_monitor_param`]), so
/// `Thread.holdsLock(this)` folds to `1` there and a synchronized splice
/// locking `this` inside it loses its recursive pair (under the SY3-2
/// switches). `0|false|off|no` records nothing: both stay as in wave 4. Read
/// once per compile, by `lib.rs`.
pub fn ir_sync_method_monitor_facts_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SYNC_STATIC_METHOD_MONITOR_FACTS` -- default ON (round 14
/// wave 7, lane sync7, proposal RV6-1 of `jit-r14-review6-proposals-RETIRED-20260930.md`): the
/// `static synchronized` twin of [`ir_sync_method_monitor_facts_enabled`]
/// (and off with it). The optimizing compile of an `ACC_SYNCHRONIZED |
/// ACC_STATIC` method of class `C` records that `C`'s mirror is held for the
/// whole body ([`Graph::method_monitor_class`]): `Thread.holdsLock(C.class)`
/// of the method's own `ldc C` folds to `1`
/// ([`IrBuilder::set_method_monitor_class_cps`]) and a `static synchronized`
/// splice of another `C` method loses its recursive pair
/// (`ir_optimize::elide_region_nested_sync_windows`). `0|false|off|no`
/// records nothing. Read once per compile, by `lib.rs`.
pub fn ir_sync_static_method_monitor_facts_enabled() -> bool {
    ir_sync_method_monitor_facts_enabled()
}

/// `CRATONVM_JIT_IR_SYNC_SPLICE_SELF_CLASS_HOLDSLOCK` -- default ON (round 14
/// wave 7, lane sync7, SS8-5 through proposal S6-1's rows): inside a `static
/// synchronized` splice of class `C`, `ldc C; invokestatic
/// Thread.holdsLock(Object)Z` is `true` (the window holds `C`'s mirror) and is
/// built as the constant `1` with no `ConstClass` node and no call
/// ([`IrBuilder::try_fold_self_class_holds_lock`]). Three halves read it: the
/// VM resolver answers the body's `ldc` of a `CONSTANT_Class` naming the
/// callee's own class with an [`crate::InlineSite::ir_ldc_class_info`] row
/// (depth 0, IR mode, `static synchronized` callee only; without the switch
/// such a body is refused as before), the planner's scan admits the pair
/// (`lib.rs` `ir_sync_splice_body_scan`), and the builder folds it. Also off
/// under [`ir_holds_lock_fold_enabled`] or [`ir_sync_splice_enabled`] off.
/// `0|false|off|no` restores wave 6: the resolver refuses such a body
/// (`ldc-non-numeric-constant`) and the site keeps its call. Read per site.
pub fn ir_sync_splice_self_class_holds_lock_enabled() -> bool {
    ir_sync_splice_enabled()
        && ir_holds_lock_fold_enabled()
        && true
}

/// `CRATONVM_JIT_IR_SPLICE_REF_LDC` -- default ON (JIT r20 wave 19, lane
/// zLdc, THROW20-4 `date` of
/// `docs/feature-designs/jit-ir-optimizations.md`): an OPTIMIZING
/// splice may carry an `ldc`/`ldc_w` of a `CONSTANT_String` or
/// `CONSTANT_Class` entry of the callee's pool, built as the same
/// `Op::ConstString` / `Op::ConstClass` a top-level `ldc` builds, with the
/// CALLEE's `(holder, cp index)`. The VM admits one only when that entry is
/// already resolved and has a GC-maintained slot (`compiled_ldc_slot_for`
/// non-zero: resolution, its loading, access check and interning already ran,
/// so splicing it moves no side effect and the slot holds the interpreter's
/// own object). And inside such a splice, `x.getClass() == C.class` /
/// `!=` on an `x` every definition of which is a `new X` is answered at build
/// time (`X == C`, for a `C` whose `getClass()` reports only itself;
/// [`IrBuilder::try_fold_spliced_getclass_of_fresh`]), so the call does not
/// make the allocation escape. Three halves read it: the VM resolver
/// (`resolve_inline_site_from`'s `ldc` scan), `lib.rs`
/// `append_ir_inline_site` (refuses a body carrying rows with it off) and the
/// builder's fold. `0|false|off|no`: every such body is refused
/// (`ldc-non-numeric-constant`) and keeps its call, as before. The
/// single-pass inliner never admits one. Read per site.
pub fn ir_splice_ref_ldc_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SYNC_SPLICE_MULTI_RETURN` -- default ON (round 14 wave 5,
/// lane sync5, proposal SS8-1 b): a synchronized body with several `return`s
/// (`if (c) return a; return b;`), walked in pc order, may be spliced; its
/// `MonitorExit` goes after the returns' join
/// (`IrBuilder::finish_multi_return_splice`), and the window rule still
/// judges every node in between. The planner's scan
/// (`lib.rs` `ir_sync_splice_body_scan`) admits such a body under it. The VM
/// resolver answers a multi-return body only when it admits one
/// (`CRATONVM_JIT_IR_SPLICE_MULTI_RETURN`, or the patch
/// `r14w5-sync5-resolver-sync-splice-multi-return-patch-FIXED-20260929.md`).
/// `0|false|off|no` restores the single-return rule. Read per site. Off with
/// [`ir_splice_branch_enabled`] off too: only its pre-scan tells the builder a
/// body has several returns (`splice_multi_return`).
pub fn ir_sync_splice_multi_return_enabled() -> bool {
    ir_splice_branch_enabled()
        && true
}

/// `CRATONVM_JIT_IR_SYNC_SPLICE_NESTED_SAME_RECEIVER` -- default ON (round 14
/// wave 6, lane sync6, SS8-1 a): a `synchronized` instance call INSIDE an
/// instance synchronized splice, on the splice's own receiver
/// (`synchronized int stepNested() { .. return step(); }`), is spliced with NO
/// monitor pair: the enclosing window holds that monitor for the whole nested
/// body, so the nested acquire/release is a recursive pair that synchronizes
/// with nobody (the SY3-2 argument). Four halves read it: the VM resolver
/// answers such a body at nesting depth 1 under a synchronized depth-0 body,
/// the planner's scan admits it (`lib.rs` `ir_sync_splice_body_scan`), the
/// append registers its combined pc ([`IrInlineTables::sync_held_sites`]), and
/// [`IrBuilder::begin_splice`] splices it only when the receiver node is
/// PROVEN the open window's monitor (else the build is refused). The window
/// rule then judges the nested body's nodes like the outer body's.
/// `0|false|off|no` restores wave 5 (the outer body is refused,
/// `ir-sync-splice-body-calls`). Read per site, at compile time.
pub fn ir_sync_splice_nested_same_receiver_enabled() -> bool {
    ir_sync_splice_enabled()
        && true
}

/// `CRATONVM_JIT_IR_HOLDSLOCK_METHOD_FOLD` -- default ON (round 14 wave 6, lane
/// sync6, proposal S5-2 of `jit-r14-sync5-proposals-RETIRED-20260930.md`): after the build, a
/// `Thread.holdsLock(o)` CALL the builder kept whose `o` is, through trivial
/// φs of the FINISHED graph, the compiling synchronized method's receiver
/// ([`Graph::method_monitor_param`]) is replaced by the constant `1`
/// (`ir_optimize::fold_method_monitor_holds_lock`). The builder's own fold
/// ([`IrBuilder::try_fold_holds_lock`]) refuses a loop-header φ whose back edges
/// are still pending; on the finished graph every φ is complete, so
/// `x = this; while (..) { holdsLock(x) .. }` folds too. Also off under
/// [`ir_holds_lock_fold_enabled`] off. `0|false|off|no` keeps those calls.
/// Read once per compile.
pub fn ir_holds_lock_method_fold_enabled() -> bool {
    ir_holds_lock_fold_enabled()
        && true
}

/// Is `info_ptr` the row of `invokestatic java/lang/Thread.holdsLock
/// (Ljava/lang/Object;)Z`? `false` for a zero pointer. Shared by the builder's
/// fold ([`IrBuilder::try_fold_holds_lock`]) and the finished-graph fold
/// (`ir_optimize::fold_method_monitor_holds_lock`).
pub(crate) fn is_holds_lock_info_row(info_ptr: usize) -> bool {
    if info_ptr == 0 {
        return false;
    }
    // SAFETY: a non-zero `info_ptr` on a call row or an `Op::Call` addresses a
    // `JitInvokeInfo` the planner boxed and keeps alive on the artifact for the
    // life of the compiled method (tests leak theirs); the call lowering bakes
    // the same pointer.
    let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
    info.invoke_kind == 3
        && info.class_name == "java/lang/Thread"
        && info.method_name == "holdsLock"
        && info.descriptor == "(Ljava/lang/Object;)Z"
}

/// nio20 wave 3, lane L2 (NIO19-9): `CRATONVM_JIT_IR_REACHABILITY_FENCE_KEEPALIVE`
/// (default ON) -- build `Reference.reachabilityFence(ref)` as an
/// [`Op::KeepAlive`] instead of an `Op::Call`. `=0` keeps the call (a real
/// call to the empty body or the thin helper). Read per site.
pub(crate) fn ir_reachability_fence_keepalive_enabled() -> bool {
    true
}

/// Is the call row `info_ptr` the static
/// `java/lang/ref/Reference.reachabilityFence(Ljava/lang/Object;)V`?
pub(crate) fn is_reachability_fence_info_row(info_ptr: usize) -> bool {
    if info_ptr == 0 {
        return false;
    }
    // SAFETY: as `is_holds_lock_info_row`.
    let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
    info.invoke_kind == 3
        && crate::is_reachability_fence_site(info.class_name, info.method_name, info.descriptor)
}

pub(crate) fn is_reflection_get_caller_class_info_row(info_ptr: usize) -> bool {
    if info_ptr == 0 {
        return false;
    }
    // SAFETY: as `is_holds_lock_info_row`.
    let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
    info.invoke_kind == 3
        && info.class_name == "jdk/internal/reflect/Reflection"
        && info.method_name == "getCallerClass"
        && info.descriptor == "()Ljava/lang/Class;"
}

pub(crate) fn is_reflection_get_class_access_flags_info_row(info_ptr: usize) -> bool {
    if info_ptr == 0 {
        return false;
    }
    // SAFETY: as `is_holds_lock_info_row`, the planner owns this call row.
    let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
    info.invoke_kind == 3
        && info.class_name == "jdk/internal/reflect/Reflection"
        && info.method_name == "getClassAccessFlags"
        && info.descriptor == "(Ljava/lang/Class;)I"
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConstFieldFold {
    Ref { slot_addr: usize, exact_class_id: u32 },
    I32(i32),
    I64(i64),
}

#[derive(Clone, Debug, Default)]
pub struct ConstMhFold {
    pub info_ptr: usize,
    pub ret_type: u8,
    pub convert: Option<crate::compile_request::MhConstantConvertRow>,
    pub box_info_ptr: usize,
}

#[derive(Clone, Debug, Default)]
pub struct ConstFoldTables {
    pub static_final_const_refs: HashMap<(u32, u32), (usize, u32)>,
    pub static_final_prim_arrays: HashMap<(u32, u32), (u8, i32)>,
    pub const_field_folds: HashMap<(usize, u32), ConstFieldFold>,
    pub const_class_access_flags: HashMap<usize, i32>,
    /// A constant `java.lang.Class` mirror's slot -> the plain (non-array,
    /// non-primitive) class it mirrors, `(class id, internal name)`.
    pub const_mirror_classes: HashMap<usize, (u32, String)>,
    pub const_mh_invoke_infos: HashMap<(usize, String), ConstMhFold>,
    pub static_final_prims: HashMap<(u32, u32), (u8, i64)>,
    /// A NON-final static `MethodHandle` field `(holder class id, field
    /// index)` -> the GC-maintained slot holding the handle it held when a
    /// mutator last offered a method reading it, and that handle's class. The
    /// slot's folds are its `const_mh_invoke_infos` rows; a site using them
    /// guards `field value == slot value` (`IrBuilder::plant_static_guess_guard`).
    pub static_guess_const_refs: HashMap<(u32, u32), (usize, u32)>,
}

// Round 14 wave 4 (lane sync4, proposal SY3-5): what the synchronized splice
// and its two follow-ups did across the process. Rows of
// `IR_SYNC_SPLICE_CENSUS`, in `SYNC_SPLICE_CENSUS_ROWS` order. A window is
// counted when the builder closes it (`end_splice`), so a graph a later stage
// refuses is still counted; `ir-sync-splice-window REFUSED` lines name those.
pub(crate) const SYNC_CENSUS_INSTANCE: usize = 0;
pub(crate) const SYNC_CENSUS_STATIC_ROW: usize = 1;
pub(crate) const SYNC_CENSUS_STATIC_MIRROR: usize = 2;
pub(crate) const SYNC_CENSUS_IN_REGION: usize = 3;
pub(crate) const SYNC_CENSUS_HOLDSLOCK_IN_SPLICE: usize = 4;
pub(crate) const SYNC_CENSUS_HOLDSLOCK_IN_REGION: usize = 5;
pub(crate) const SYNC_CENSUS_WINDOW_ELIDED: usize = 6;
// Round 14 wave 5 (lane sync5, SS8-3): the same two under the compiling
// synchronized method's own monitor (`Graph::method_monitor_param`).
pub(crate) const SYNC_CENSUS_HOLDSLOCK_IN_METHOD: usize = 7;
pub(crate) const SYNC_CENSUS_WINDOW_ELIDED_IN_METHOD: usize = 8;
// SS8-1 (b): a synchronized splice closed at a multi-return join.
pub(crate) const SYNC_CENSUS_MULTI_RETURN: usize = 9;
// Round 14 wave 6 (lane sync6): SS8-1 (a), a same-receiver synchronized body
// spliced inside a window with no pair of its own; S5-2, a `holdsLock` call
// folded on the finished graph of a synchronized method.
pub(crate) const SYNC_CENSUS_NESTED_SAME_RECEIVER: usize = 10;
pub(crate) const SYNC_CENSUS_HOLDSLOCK_FINISHED_GRAPH: usize = 11;
// Round 14 wave 7 (lane sync7): SS8-5, `holdsLock(C.class)` folded inside a
// `static synchronized` splice of `C`; RV6-1, the same under a `static
// synchronized` method's own monitor and a static window of its class deleted
// there.
pub(crate) const SYNC_CENSUS_HOLDSLOCK_SELF_CLASS_IN_SPLICE: usize = 12;
pub(crate) const SYNC_CENSUS_HOLDSLOCK_IN_STATIC_METHOD: usize = 13;
pub(crate) const SYNC_CENSUS_WINDOW_ELIDED_IN_STATIC_METHOD: usize = 14;
// Round 15 wave 1 (lane sync): S6-2's census -- a caller-held synchronized
// instance CALL whose receiver is the compiling synchronized method's own
// monitor (a recursive pair the raw callee call would save) -- and SS8-2's, a
// self-locking optimizing body built (`MethodSelfLock`; a later stage may
// still refuse it, `ir self-lock REFUSED` names why).
pub(crate) const SYNC_CENSUS_CALLER_HELD_ON_METHOD_MONITOR: usize = 15;
pub(crate) const SYNC_CENSUS_IR_SELF_LOCK_BUILT: usize = 16;
// Round 15 wave 3 (lane sync2): S15-3's static self-locking bodies built (a
// subset of row 16), and -- S15-2's first half -- how many self-locking
// bodies passed `ir_lower`'s closed-exits check and how many it refused (a
// passed body can still be declined later: the verifier, the acceptance gate).
pub(crate) const SYNC_CENSUS_IR_SELF_LOCK_STATIC_BUILT: usize = 17;
pub(crate) const SYNC_CENSUS_IR_SELF_LOCK_CLOSED: usize = 18;
pub(crate) const SYNC_CENSUS_IR_SELF_LOCK_REFUSED: usize = 19;
// Round 15 wave 4 (lane sync3, SY2-6 of `jit-r15w3-sync2-proposals-RETIRED-20261003.md`): row
// 19 split by cause -- ten rows from here, in `ir_lower::SelfLockRefusal`
// order (`SelfLockRefusal::census_row`) -- and what became of the self-locking
// bodies the check passed: declined by the acceptance gate (a subset of the
// candidates that fell through), accepted (handed to the VM to publish), and
// every candidate whose optimizing attempt published nothing, whatever
// declined it (builder refusal, closed-exits check, a post-lowering backstop,
// the gate) -- the population the single-pass self-locking body then serves.
pub(crate) const SYNC_CENSUS_IR_SELF_LOCK_REFUSED_CAUSE_FIRST: usize = 20;
pub(crate) const SYNC_CENSUS_IR_SELF_LOCK_GATE_DECLINED: usize = 30;
pub(crate) const SYNC_CENSUS_IR_SELF_LOCK_ACCEPTED: usize = 31;
pub(crate) const SYNC_CENSUS_IR_SELF_LOCK_FELL_THROUGH: usize = 32;
pub(crate) const SYNC_SPLICE_CENSUS_ROWS: [&str; 33] = [
    "instance",
    "static-row",
    "static-mirror",
    "in-region",
    "holdslock-folded-in-splice",
    "holdslock-folded-in-region",
    "window-elided-in-region",
    "holdslock-folded-in-method",
    "window-elided-in-method",
    "multi-return",
    "nested-same-receiver",
    "holdslock-folded-on-finished-graph",
    "holdslock-self-class-folded-in-static-splice",
    "holdslock-folded-in-static-method",
    "window-elided-in-static-method",
    "caller-held-call-on-method-monitor",
    "ir-self-locking-body",
    "ir-self-locking-static-body",
    "ir-self-locking-exits-closed",
    "ir-self-locking-refused",
    "ir-self-locking-refused-monitor-home",
    "ir-self-locking-refused-protected-range",
    "ir-self-locking-refused-enter",
    "ir-self-locking-refused-transfer-before-enter",
    "ir-self-locking-refused-exit-shape",
    "ir-self-locking-refused-transfer-after-exit",
    "ir-self-locking-refused-return-not-released",
    "ir-self-locking-refused-handover-off",
    "ir-self-locking-refused-frame-chain",
    "ir-self-locking-refused-frame-shape",
    "ir-self-locking-gate-declined",
    "ir-self-locking-accepted",
    "ir-self-locking-candidate-fell-through",
];
static IR_SYNC_SPLICE_CENSUS: [AtomicU64; SYNC_SPLICE_CENSUS_ROWS.len()] =
    [const { AtomicU64::new(0) }; SYNC_SPLICE_CENSUS_ROWS.len()];

/// Count one event of row `row` (a `SYNC_CENSUS_*` index). An unknown row is
/// ignored.
pub(crate) fn note_sync_splice_census(row: usize) {
    if let Some(c) = IR_SYNC_SPLICE_CENSUS.get(row) {
        c.fetch_add(1, Ordering::Relaxed);
    }
}

/// The synchronized-splice census as one line, `name=count` for every row,
/// zeroes included (a zero separates "never built" from "not counted").
/// Printed at exit under `CRATONVM_DBG_JITC` (`vm` `interp_census.rs`).
pub fn ir_sync_splice_census_line() -> String {
    SYNC_SPLICE_CENSUS_ROWS
        .iter()
        .zip(IR_SYNC_SPLICE_CENSUS.iter())
        .map(|(name, c)| format!("{name}={}", c.load(Ordering::Relaxed)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// One synchronized splice's monitor window, recorded by the builder on
/// [`Graph::sync_splice_windows`] for `ir_lower`'s schedule check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncSpliceWindow {
    /// The `Op::MonitorEnter` before the body.
    pub enter: NodeId,
    /// The `Op::MonitorExit` after it ([`NO_NODE`] while the body is open).
    pub exit: NodeId,
    /// The locked object: the receiver, or the class mirror's `Op::ConstClass`.
    pub monitor: NodeId,
    /// The class a `static synchronized` callee declares (its statics are the
    /// only ones the window may touch), or `0` for an instance callee.
    pub static_class_id: u32,
}

/// May node `n` sit between a synchronized splice's `MonitorEnter` and its
/// `MonitorExit`? Only when its lowering cannot transfer to the interpreter
/// or raise while the monitor is held:
///
/// * anything `ir_lower::node_cannot_deopt` admits;
/// * a field read or write through the locked RECEIVER (constant offset): its
///   lowering's null-receiver exit cannot fire, the object was just locked;
/// * a static read or write of the locked class's OWN statics: the class is
///   initialised (the resolver admits no other static callee), so neither
///   route runs `<clinit>` and neither helper answers the sentinel.
///
/// One predicate for the builder's check and the lowerer's, so the two cannot
/// disagree.
pub(crate) fn sync_splice_window_node_admitted(
    graph: &Graph,
    n: &Node,
    window: &SyncSpliceWindow,
) -> bool {
    if n.op == Op::Dead || crate::ir_lower::node_cannot_deopt(graph, n) {
        return true;
    }
    match n.op {
        Op::Load(_) | Op::Store(_) => {
            window.static_class_id == 0
                && n.input_opt(2) == Some(window.monitor)
                && n.input_opt(3)
                    .and_then(|o| graph.node_opt(o))
                    .is_some_and(|o| matches!(o.op, Op::Const(_)))
        }
        Op::LoadStatic { class_id, .. } | Op::StoreStatic { class_id, .. } => {
            window.static_class_id != 0 && class_id == window.static_class_id
        }
        _ => false,
    }
}

/// Round 15 wave 1 (lane sync, proposal SS8-2 of `jit-r14-sync4-proposals-RETIRED-20260930.md`;
/// item 3 of `r13w4-sync2-self-locking-bodies-with-deopt-exits-FIXED-20260930.md`):
/// the method monitor of a SELF-LOCKING optimizing body, recorded by the
/// builder on [`Graph::method_self_lock`] when `lib.rs` asks for one
/// ([`IrBuilder::set_method_self_lock`]).
///
/// The builder emits `enter = MonitorEnter(monitor)` on the entry control
/// before the first bytecode's snapshot and pushes `monitor` on its monitor
/// stack as an entry no bytecode pops, so every frame state of the method
/// names the hold `{this, 1, relock: false}` -- the hand-over shape
/// `deopt::handed_method_monitor_index` recognises -- and a `MonitorExit` of
/// it right before every `Op::Return` (`exits`). `ir_lower` releases the
/// monitor on every exceptional exit, refuses the body unless every other
/// exit hands the hold over with its frame, and publishes it
/// `CompiledMethod::self_locks_monitor`. The optimizer leaves these ops
/// alone (`ir_optimize::elide_nested_monitors`, `coarsen_adjacent_monitors`,
/// escape analysis' lock elision); `ir_lower` checks they survived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodSelfLock {
    /// The `Op::MonitorEnter` at the method's entry.
    pub enter: NodeId,
    /// The locked object: the receiver's `Op::Param(0)`.
    pub monitor: NodeId,
    /// One `Op::MonitorExit` per `Op::Return`, in build order.
    pub exits: Vec<NodeId>,
}

impl MethodSelfLock {
    /// Is `id` the method monitor's enter or one of its exits?
    pub fn names_op(&self, id: NodeId) -> bool {
        self.enter == id || self.exits.contains(&id)
    }
}

/// Round 15 wave 3 (lane sync2, proposal S15-3): the `cp_idx` of the one
/// `Op::ConstClass` a `static synchronized` self-locking body locks
/// ([`IrBuilder::set_method_self_lock_mirror`]) -- `u16::MAX`, the index the
/// VM files a class's own mirror slot under (`vm::jit::helpers`
/// `CLASS_MIRROR_SLOT_CP_IDX`). No `ldc` names it (`constant_pool_count` is a
/// `u16`, so 65534 is the last valid index), which is how `ir_lower` tells
/// that node from every `ldc`: it lowers it as a plain read of its slot and
/// never calls the resolution helper with this index.
pub const SELF_LOCK_MIRROR_CP_IDX: u16 = u16::MAX;

/// How a synchronized splice takes its monitor ([`IrBuilder::sync_splice_plan`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir) enum SyncMonitorPlan {
    /// The receiver, argument 0.
    Instance,
    /// The class mirror, materialised as the caller-held static route does.
    Static {
        holder_class_id: u32,
        cp_idx: u16,
        slot_addr: usize,
        class_id: u32,
    },
}

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

/// Round 13 wave 6 (lane chain2; `r13w5-resume-chain-scope-class-ids-patch`):
/// `(method_key, class_id)` of every body in `sites`, sorted by key with one
/// row per key -- the table an optimizing artifact keeps
/// (`CompiledMethod::splice_scope_class_ids`) so the VM resolves an inlined
/// deopt chain's inner scope by the class the compile spliced it from
/// (`deopt::splice_scope_class_hint`), not by name from the enclosing class's
/// loader. A site with no class id (`0`) adds nothing; a key two sites
/// resolved to DIFFERENT classes gets [`crate::deopt::SPLICE_SCOPE_CLASS_AMBIGUOUS`].
pub fn splice_scope_class_ids(sites: &HashMap<usize, IrInlineSite>) -> Vec<(String, u32)> {
    let mut rows: Vec<(String, u32)> = sites
        .values()
        .filter(|s| s.class_id != 0 && !s.method_key.is_empty())
        .map(|s| (s.method_key.clone(), s.class_id))
        .collect();
    rows.sort_unstable();
    rows.dedup();
    let mut out: Vec<(String, u32)> = Vec::with_capacity(rows.len());
    for (key, id) in rows {
        if let Some(last) = out.last_mut() {
            if last.0 == key {
                if last.1 != id {
                    last.1 = crate::deopt::SPLICE_SCOPE_CLASS_AMBIGUOUS;
                }
                continue;
            }
        }
        out.push((key, id));
    }
    out
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
    pub(in crate::ir) bodies: Vec<(usize, usize, usize, String, u32)>,
    /// Round 13 wave 13 (lane jitfix): per body `base`, the callee's argument
    /// local slots ([`IrInlineSite::arg_local_slots`]) and `max_locals` -- what
    /// a forward "definitely assigned" analysis of that body needs, which the
    /// tuple above does not carry. Read by [`Self::body_for_key`].
    pub(in crate::ir) entry_shapes: HashMap<usize, (Vec<usize>, usize)>,
    pub(in crate::ir) original_bodies: HashMap<usize, Vec<u8>>,
    /// JIT round 21 wave 3 (lane keepset): per body `base`, the admitted
    /// exception rows of a spliced callee with real handlers
    /// ([`IrInlineTables::splice_handler_rows`]) as BODY-relative
    /// `(start, end, handler)`, so the chain-snapshot liveness models the
    /// handler edges (`ea_ir_bridge::ir_prune_dead_snapshot_locals_scoped`).
    pub(in crate::ir) handler_rows: HashMap<usize, Vec<(usize, usize, usize)>>,
}

impl IrInlineFrameSites {
    /// JIT round 21 wave 3 (lane keepset): record each spliced body's admitted
    /// handler rows ([`IrSpliceHandlerRow`], combined pcs) rebased to the
    /// body. A row that does not lie inside a recorded body's extent leaves
    /// an empty-but-present entry for that body, which the prune reads as
    /// "cannot model the handlers: prune nothing in it".
    pub fn with_handler_rows(mut self, rows: &HashMap<usize, Vec<IrSpliceHandlerRow>>) -> Self {
        for &(base, len, _, _, _) in &self.bodies {
            let Some(list) = rows.get(&base) else {
                continue;
            };
            let rebased: Option<Vec<(usize, usize, usize)>> = list
                .iter()
                .map(|r| {
                    let start = r.start_pc.checked_sub(base)?;
                    let end = r.end_pc.checked_sub(base)?;
                    let handler = r.handler_pc.checked_sub(base)?;
                    (start < end && end <= len && handler < len).then_some((start, end, handler))
                })
                .collect();
            self.handler_rows.insert(base, rebased.unwrap_or_default());
        }
        self
    }

    pub fn with_original_handler_rows(
        mut self,
        orig: &HashMap<usize, Vec<(usize, usize, usize, u16)>>,
    ) -> Self {
        for &(base, len, _, _, _) in &self.bodies {
            let Some(cur) = self.handler_rows.get(&base) else {
                continue;
            };
            if cur.is_empty() {
                continue;
            }
            let Some(list) = orig.get(&base) else {
                continue;
            };
            let valid: Option<Vec<(usize, usize, usize)>> = list
                .iter()
                .map(|&(start, end, handler, _)| {
                    (start < end && end <= len && handler < len).then_some((start, end, handler))
                })
                .collect();
            self.handler_rows.insert(base, valid.unwrap_or_default());
        }
        self
    }

    /// JIT round 21 wave 3 (lane keepset): the body-relative handler rows of
    /// the body at `base`: `None` when it has none, `Some(&[])` when it has
    /// rows this table could not rebase (model nothing: prune nothing).
    pub(crate) fn handler_rows(&self, base: usize) -> Option<&[(usize, usize, usize)]> {
        self.handler_rows.get(&base).map(Vec::as_slice)
    }

    /// Keep the interpreter source only for a body whose recorded extent fits.
    /// Missing or malformed source leaves the conservative copied-body path.
    pub fn with_original_bodies(mut self, source: &HashMap<usize, Vec<u8>>) -> Self {
        for &(base, len, _, _, _) in &self.bodies {
            if let Some(original) = source.get(&base).and_then(|code| code.get(..len)) {
                self.original_bodies.insert(base, original.to_vec());
            }
        }
        self
    }

    pub(crate) fn original_body(&self, base: usize) -> Option<&[u8]> {
        self.original_bodies.get(&base).map(Vec::as_slice)
    }
    /// Round 13 wave 13 (lane jitfix;
    /// `r13w12-snapliv-debugger-discard-check-ignores-chain-inner-scopes`):
    /// `(base, code_len, argument local slots, max_locals)` of a body this
    /// build spliced for the method `key` (`"class/Name.method:descriptor"`),
    /// or `None` when none was. Several splices of one method are copies of
    /// the same bytecode, so any of them answers a question about the method's
    /// own bcis; the lowest `base` is taken.
    pub fn body_for_key(&self, key: &str) -> Option<(usize, usize, &[usize], usize)> {
        let &(base, code_len, _, _, _) = self.bodies.iter().find(|b| b.3 == key)?;
        let (slots, max_locals) = self.entry_shapes.get(&base)?;
        Some((base, code_len, slots.as_slice(), *max_locals))
    }

    /// Build from the builder's site table. `sites` is keyed by CALLER pc, in
    /// combined coordinates, exactly as [`IrInlineTables::sites`] holds it.
    pub fn from_sites(sites: &HashMap<usize, IrInlineSite>) -> Self {
        let entry_shapes: HashMap<usize, (Vec<usize>, usize)> = sites
            .values()
            .map(|s| {
                // Cast: a JVM local index (u16 range) widened.
                let slots: Vec<usize> = s.arg_local_slots.iter().map(|&k| k as usize).collect();
                (s.base, (slots, s.max_locals))
            })
            .collect();
        let mut bodies: Vec<(usize, usize, usize, String, u32)> = sites
            .iter()
            .map(|(&key, s)| {
                (
                    s.base,
                    s.code_len,
                    // Round 14 wave 2 (lane guard, GS-1): a bimorphic site's
                    // second body is keyed with a tag; its caller is the pc.
                    inline_site_caller_pc(key),
                    s.method_key.clone(),
                    s.class_id,
                )
            })
            .collect();
        bodies.sort_unstable_by_key(|&(base, _, _, _, _)| base);
        Self {
            bodies,
            entry_shapes,
            original_bodies: HashMap::new(),
            handler_rows: HashMap::new(),
        }
    }

    /// Interpreter round i2 wave 58 (lane L2): the `base` of the ONE body this
    /// build spliced for the call at caller pc `pc` (combined coordinates), or
    /// `None` when none was or more than one was (a bimorphic site's two
    /// bodies share their caller pc; naming either would guess).
    /// `chain_at(base)` is then that callee at bci 0 over its callers.
    pub fn sole_body_base_for_caller_pc(&self, pc: usize) -> Option<usize> {
        let mut found = None;
        for &(base, _, caller_pc, _, _) in &self.bodies {
            if caller_pc == pc {
                if found.is_some() {
                    return None;
                }
                found = Some(base);
            }
        }
        found
    }

    /// `true` when nothing was spliced — the state of every compile that
    /// inlines nothing, and the fast path every caller takes first.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bodies.is_empty()
    }

    /// JIT round 23 wave 1, lane L2 (INLINE22-4): is `pc` inside a lambda SAM
    /// forwarder's own body ([`ir_splice_body_is_lambda_forwarder`])? Such a
    /// body is spliced at depth 0 only and commits nothing before its one
    /// call, so a guard in it may resume at the outermost `invoke` even where
    /// the graph relies on chains elsewhere (`ir_lower`'s
    /// `splice_chain_for_site`).
    pub fn pc_in_lambda_forwarder(&self, pc: usize) -> bool {
        // JIT round 23 wave 2d: a TOP-LEVEL forwarder only (its caller pc is
        // the compiling method's own); a nested one resumes through its
        // re-execute chain (`IrBuilder::forwarder_reexecute_chain`), and a
        // flat resume there would re-run what the enclosing bodies committed.
        self.body_at(pc).is_some_and(|i| {
            ir_splice_body_is_lambda_forwarder(self.bodies[i].4)
                && self.body_at(self.bodies[i].2).is_none()
        })
    }

    /// JIT round 23 wave 2, lane L2 (INLINE22-4 stage 2): may a guard at `pc`
    /// resume at the outermost `invoke` although the graph relies on chains
    /// elsewhere? Inside a forwarder's own body ([`Self::pc_in_lambda_forwarder`]),
    /// and anywhere under an OPAQUE forwarder (one making more than one call:
    /// `SpliceFrame::opaque`, its `base` in `opaque_bases` =
    /// [`Graph::opaque_forwarder_bases`]), whose subtree records no chain, so no
    /// replay fence in it was relaxed and the flat resume re-runs nothing
    /// committed (a forwarder is spliced at depth 0 only).
    pub fn pc_resumes_flat_under_forwarder(&self, pc: usize, opaque_bases: &[usize]) -> bool {
        if self.pc_in_lambda_forwarder(pc) {
            return true;
        }
        let mut at = pc;
        let mut steps = 0usize;
        while let Some(i) = self.body_at(at) {
            steps += 1;
            if steps > MAX_INLINE_SCOPE_DEPTH + 1 {
                return false;
            }
            if ir_splice_body_is_lambda_forwarder(self.bodies[i].4) {
                return opaque_bases.contains(&self.bodies[i].0);
            }
            at = self.bodies[i].2;
        }
        false
    }

    /// Index of the body whose range contains `pc`, or `None`.
    pub(in crate::ir) fn body_at(&self, pc: usize) -> Option<usize> {
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
        let mut steps = 0usize;
        while let Some(i) = self.body_at(at) {
            let (base, _, caller_pc, ref key, class_id) = self.bodies[i];
            steps += 1;
            if steps > MAX_INLINE_SCOPE_DEPTH + 1 {
                // The cyclic-table guard below, for a walk that skips levels.
                return Vec::new();
            }
            // JIT round 23 wave 1, lane L2 (INLINE22-4): a lambda SAM
            // forwarder contributes no level. HotSpot's spun proxy frame is
            // hidden, and the interpreter's lambda dispatch pushes none, so a
            // trace through the splice names the implementation directly
            // over the caller's `invoke`, as the uncompiled call does.
            if ir_splice_body_is_lambda_forwarder(class_id) {
                at = caller_pc;
                continue;
            }
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

    /// Round 13 wave 12 (lane snapliv, CH5-1): `(base, code_len, caller_pc,
    /// method_key)` of every spliced body enclosing `pc`, innermost first --
    /// the walk [`Self::chain_at`] makes, with the ranges it reads. The chain
    /// snapshot prune (`ea_ir_bridge::ir_prune_dead_snapshot_locals_scoped`)
    /// checks a snapshot's [`SpliceScopeChain`] against it and runs each
    /// scope's liveness over `combined[base .. base + code_len]`. Empty for a
    /// pc in no body, and for a walk deeper than [`MAX_INLINE_SCOPE_DEPTH`]
    /// (a malformed table), exactly as `chain_at` answers.
    pub fn bodies_enclosing(&self, pc: usize) -> Vec<(usize, usize, usize, &str)> {
        let mut out: Vec<(usize, usize, usize, &str)> = Vec::new();
        let mut at = pc;
        let mut steps = 0usize;
        while let Some(i) = self.body_at(at) {
            let Some((base, code_len, caller_pc, key, class_id)) = self.bodies.get(i) else {
                return Vec::new();
            };
            steps += 1;
            if steps > MAX_INLINE_SCOPE_DEPTH + 1 {
                return Vec::new();
            }
            // JIT round 23 wave 1, lane L2 (INLINE22-4): a lambda SAM
            // forwarder is no scope of a chain (`IrBuilder::splice_scope_chain`
            // skips it), so the body it called is parked at the forwarder's
            // own caller pc: the `invoke` it replaced.
            if ir_splice_body_is_lambda_forwarder(*class_id) {
                if let Some(last) = out.last_mut() {
                    last.2 = *caller_pc;
                }
                at = *caller_pc;
                continue;
            }
            out.push((*base, *code_len, *caller_pc, key.as_str()));
            if out.len() > MAX_INLINE_SCOPE_DEPTH {
                return Vec::new();
            }
            at = *caller_pc;
        }
        out
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
    /// Unmasked interpreter bodies keyed by their combined-buffer base.
    pub original_body_codes: HashMap<usize, Vec<u8>>,
    /// Round rx27 (lane matcher): the ORIGINAL exception table of each body
    /// whose own code has one, keyed by base, rows body-relative
    /// (`InlineSite::callee_original_handler_rows`). Such a body keeps no
    /// `SpliceScopeSource` row carrying these rows (with the class file's
    /// `catch_type` last), and its chain-snapshot liveness reads its
    /// original code with them (`lib.rs` `spliced_body_codes`).
    pub original_body_handler_rows: HashMap<usize, Vec<(usize, usize, usize, u16)>>,
    /// `pc → (field_index, type_tag)`, merged into the builder's `field_info`.
    pub field_info: HashMap<usize, (usize, u8)>,
    /// `pc → (info_ptr, num_args, ret_type)` for the calls a spliced body makes
    /// that are NOT themselves spliced.
    pub invoke_info: HashMap<usize, (usize, usize, u8)>,
    /// Spliced static calls whose owner has not loaded: precise cold exits.
    /// Round nio20 wave 1, lane L1 (NIO19-3): also the spliced `checkcast` /
    /// `instanceof` pcs whose class has not loaded, on an arm the callee's
    /// profile never ran (the opcode at the pc tells the two apart; the
    /// builder's type-check arms plant the same exit,
    /// `IrBuilder::plant_unloaded_typecheck_exit`).
    pub unloaded_static_call_pcs: HashSet<usize>,
    /// `pc → (class_id, num_fields)` for the allocations a spliced body makes.
    /// This is the row whose absence made `loadFromArray` bail the whole IR
    /// tier — see `docs/known-issues/jit/per-voxel-allocation-*`.
    pub new_info: HashMap<usize, (u32, usize)>,
    /// JIT round 25 wave 10 (W10-6, `CRATONVM_JIT_IR_SPLICE_ANEWARRAY`):
    /// `pc -> component class id` for the `anewarray`s a spliced body makes,
    /// merged into the builder's `anewarray_info` (the map its `0xbd` arm
    /// reads; `IrBuilder::set_anewarray_info` installed the caller's own rows
    /// first, under the caller's pcs). Empty with the switch off.
    pub anewarray_info: HashMap<usize, u32>,
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
    /// `putstatic` (0xb3) stays refused inside a splice: the builder's `0xb3`
    /// arm (`Op::StoreStatic`, the `jit_putstatic_*` helper call) bails when a
    /// splice is open, and the resolver refuses such a callee separately so
    /// the two sides cannot disagree.
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
    /// Round 14 wave 6 (lane sync6, SS8-1 a): combined-buffer pcs of the
    /// `invokevirtual`s inside a synchronized body whose spliced callee is a
    /// `synchronized` instance method on the SAME receiver. Such a body is
    /// spliced with no monitor pair of its own, and only where
    /// [`IrBuilder::begin_splice`] proves its receiver is the open window's
    /// monitor ([`ir_sync_splice_nested_same_receiver_enabled`]). A nested
    /// synchronized body NOT listed here is never appended (`lib.rs` refuses
    /// it), so none is ever spliced without a hold.
    pub sync_held_sites: HashSet<usize>,
    /// Round 14 wave 7 (lane sync7, SS8-5 through S6-1's rows): combined pc
    /// of an `ldc`/`ldc_w` inside a `static synchronized` body that names the
    /// body's own class, immediately followed by `invokestatic
    /// Thread.holdsLock(Object)Z` -> that class's id (the VM resolved the
    /// constant to it; `crate::InlineSite::ir_ldc_class_info`). The builder
    /// folds the pair to `1` inside the window of that class
    /// ([`IrBuilder::try_fold_self_class_holds_lock`]) and otherwise refuses
    /// the build at the pc (no other `ldc` row names it).
    pub ldc_self_class_holds_lock: HashMap<usize, u32>,
    /// Round 15 wave 3 (lane chain3, CHN15-2 producer): combined pc of a
    /// spliced `getfield` / `putfield` -> the DECLARING class id of its
    /// resolved field (`crate::InlineSite::field_decl_info`, kept only beside
    /// the matching [`Self::field_info`] row). Merged into the builder's rows
    /// ([`IrBuilder::set_field_decl_classes`]); a pc with no row has no
    /// identity.
    pub field_decl_class: HashMap<usize, u32>,
    /// nio21 wave 2 (lane L3): combined pcs of the spliced `getfield`s whose
    /// resolved field is `final` (`crate::InlineSite::final_field_pcs`,
    /// rebased). Merged into [`IrBuilder::add_final_field_pcs`]; a pc absent
    /// here is simply not forwarded under the final switch.
    pub final_field_pcs: HashSet<usize>,
    /// nio21 wave 7 (lane L4, W5L4-3): combined pcs of the spliced `getfield`s
    /// whose value is a `byte[]` or null (`crate::InlineSite::byte_array_field_pcs`,
    /// rebased under [`ir_scoped_byte_array_base_enabled`]). Merged into
    /// [`IrBuilder::add_byte_array_field_pcs`]; the load node a listed pc
    /// produces (or forwards) goes into [`Graph::byte_array_nodes`]. A pc
    /// absent here claims nothing.
    pub byte_array_field_pcs: HashSet<usize>,
    /// JIT r20 wave 18 (lane xLib, THROW20-4): combined pcs of the spliced
    /// `Reference.referent` reads (`crate::InlineSite::referent_load_pcs`,
    /// rebased). Merged into [`Graph::referent_load_pcs`], where the lowering
    /// sends each to the checked `jit_getfield` helper (the SATB keep-alive).
    pub referent_load_pcs: HashSet<usize>,
    /// JIT round 16 wave 7 (lane callbind): combined pc of a SURVIVING
    /// `Integer.intValue()I` / `Long.longValue()J` call inside a spliced body
    /// -> the unboxing accessor and its receiver guard class, exactly the row
    /// the compiling method's own sites get in
    /// [`IrBuilder::set_unbox_intrinsics`]. Unlike an own-site row, the pc
    /// KEEPS its [`Self::invoke_info`] row: [`IrBuilder::try_emit_unbox_intrinsic`]
    /// declines inside a splice wherever a trap could not resume (a committed
    /// store or call, no chain snapshot), and the call is the fallback.
    /// Filled by `lib.rs` (`ir_plan_spliced_unbox_intrinsics`) only under
    /// `CRATONVM_JIT_IR_SPLICE_UNBOX_INTRINSIC`.
    pub unbox_intrinsics: HashMap<usize, (UnboxOp, u32)>,
    /// nio21 wave 4 (lane L2, [`ir_splice_callee_handlers_enabled`]): the
    /// admitted exception rows of a spliced body whose table has real
    /// handlers, keyed by the body's combined-buffer base, every pc rebased.
    /// The builder covers their `try` ranges with call-site edges and refuses
    /// the body when it cannot.
    pub splice_handler_rows: HashMap<usize, Vec<IrSpliceHandlerRow>>,
    /// nio21 wave 11, lane L3 (W10L3-1, [`ir_splice_indy_enabled`]): combined
    /// pc of a spliced `invokedynamic` -> its stack effect `(argument entries,
    /// return tag)`, merged into the builder's `indy_trap_sites`. Only the
    /// constant-word lambda shape `(0, b'L')` is ever produced
    /// (`crate::InlineSite::ir_indy_rows`).
    pub indy_trap_sites: HashMap<usize, (usize, u8)>,
    /// The same pcs -> the VM's bridge-site pointer, merged into the
    /// builder's `indy_bridge_sites`. A site here has a PUBLISHED constant
    /// word (the resolver asked the VM), so its `Op::IndyBridge` probe answers
    /// the singleton and the bridge call is cold.
    pub indy_bridge_sites: HashMap<usize, usize>,
    /// JIT round 20 wave 19 (lane ySplice, SPLICE20-1,
    /// [`ir_splice_indy_capture_enabled`]): combined pc of a spliced linked
    /// CAPTURING lambda site -> what its evaluation allocates
    /// (`crate::InlineSite::ir_indy_capture_rows`), merged into the builder's
    /// `indy_capture_news`; its stack effect `(captures, b'L')` is in
    /// [`Self::indy_trap_sites`]. Built by [`IrBuilder::try_indy_capture_new`].
    pub indy_capture_news: HashMap<usize, IndyCaptureNew>,
    /// JIT round 21 wave 15 (lane lambda3, LAMBDACAP21-2): combined pc of a
    /// spliced body's SAM `invokeinterface` whose lambda's static impl is
    /// spliced at that same pc -> `(combined invokedynamic pc, capture count,
    /// impl parameter count)` (`crate::InlineSite::ir_lambda_impl_rows`),
    /// merged into the builder's `lambda_impl_sites`. See
    /// [`IrBuilder::lambda_impl_opcode`].
    pub lambda_impl_sites: HashMap<usize, (usize, usize, usize)>,
    /// JIT round 19 proposal wave 4 (lane megaosr4, MEGAOSR19-2): combined pc
    /// of a surviving virtual / interface call inside a spliced body -> the
    /// receiver counts the CALLEE's own profile holds at that call's
    /// callee-relative bci (`crate::InlineSite::callee_receiver_profile`).
    /// Planner-only: `try_compile_inner` seeds the site's inline cache from it
    /// (`crate::ir_splice_callee_profile_seed_enabled`); the builder ignores
    /// it. Empty with the switch off.
    pub spliced_receiver_seeds: HashMap<usize, crate::profile::ReceiverCounts>,
    /// JIT round 23 wave 4 (lane L1, CF23-2; `CRATONVM_JIT_IR_SPLICE_NEVER_TAKEN`):
    /// `(combined branch pc, callee method key, callee branch pc)` of each
    /// spliced conditional branch whose taken edge the callee's profile never
    /// saw (`crate::InlineSite::callee_never_taken_pcs`). Planner-only:
    /// `try_compile_inner` drops the rows withdrawn at their outermost call
    /// site and hands the rest to [`IrBuilder::set_splice_never_taken`].
    pub spliced_never_taken: Vec<(usize, String, u32)>,
    /// CF23-2: the combined pcs only those rows' taken edges reach
    /// (`crate::splice_never_taken_dead_pcs`); stepped over with dead control.
    pub spliced_never_taken_dead: Vec<usize>,
    /// Combined pcs of spliced conditional branches whose fall-through edge
    /// the callee's profile never saw (`crate::InlineSite::callee_always_taken_pcs`).
    pub spliced_always_taken: Vec<(usize, String, u32)>,
    /// Combined pcs only those rows' fall-through edges reach.
    pub spliced_always_taken_dead: Vec<usize>,
    /// Combined `(switch_pc, target_pc, callee_key, callee_bci)` for cold
    /// `tableswitch`/`lookupswitch` arms inside spliced bodies.
    pub spliced_pruned_switch_edges: Vec<(usize, usize, String, u32)>,
    /// Combined pcs only those pruned switch arms reach.
    pub spliced_pruned_switch_dead: Vec<usize>,
    /// JIT round 23 wave 8 (lane W8-L2, CF-W8-3): `(start, end, permille)`
    /// combined-pc ranges only a rarely taken spliced arm reaches
    /// (`crate::InlineSite::callee_cold_arm_ranges`, rebased). Planner-only:
    /// handed to `ir_evidence::set_splice_cold_ranges` to price an in-splice
    /// blind dispatch; never read by the builder.
    pub spliced_cold_ranges: Vec<(usize, usize, u16)>,
    /// JIT round 20 wave 12 (lane hFin): the classes of the spliced bodies'
    /// `new` rows the resolver answered `has_finalizer` for
    /// (`crate::InlineSite::ir_new_info`'s fourth field). Merged into
    /// [`Graph::finalizable_new_classes`], so escape analysis never
    /// scalar-replaces such an allocation, whatever
    /// `CRATONVM_JIT_IR_SPLICE_REFUSES_FINALIZABLE_NEW` let the resolver admit.
    pub finalizable_new_classes: HashSet<u32>,
    /// JIT r20 wave 19 (lane zLdc, THROW20-4 `date`, [`ir_splice_ref_ldc_enabled`]):
    /// combined pc of a spliced `ldc`/`ldc_w` of a `CONSTANT_String` ->
    /// `(holder class id, cp index)` of the CALLEE's pool
    /// (`crate::InlineSite::ir_ldc_ref_rows`, rebased). Merged into the
    /// builder's own `ldc_string_info`, so the spliced site builds the same
    /// `Op::ConstString` a top-level `ldc` does. Only a site whose constant is
    /// already resolved (a non-zero [`Self::ldc_ref_slots`] row) is ever here.
    pub ldc_ref_string: HashMap<usize, (u32, u16)>,
    /// The same for a `CONSTANT_Class` entry (`Op::ConstClass`), merged into
    /// the builder's `ldc_class_info`.
    pub ldc_ref_class: HashMap<usize, (u32, u16)>,
    /// `(holder class id, cp index)` -> the VM's GC-maintained slot holding
    /// the resolved constant (`JitLdcConstant::StringSlot` /
    /// `ClassMirrorSlot`), merged into the builder's `ldc_slot_info`. Every
    /// row of the two maps above has one, non-zero.
    pub ldc_ref_slots: HashMap<(u32, u16), usize>,
    /// Combined pc of a spliced `invokevirtual Object.getClass()` that is
    /// followed by `ldc C` (one of [`Self::ldc_ref_class`]'s rows) and an
    /// `if_acmpeq`/`if_acmpne` -> `C`'s class id, for a `C` the VM states
    /// `getClass()` reports exactly (`crate::InlineSite::ir_getclass_compares`).
    /// Merged into [`IrBuilder::try_fold_spliced_getclass_of_fresh`]'s rows.
    pub splice_getclass_compares: HashMap<usize, u32>,
    /// Combined-buffer base pc -> `(caller_mirror_slot, security_walk_ignored)`
    /// for folding `Reflection.getCallerClass()` inside spliced `@CallerSensitive` frames.
    pub splice_caller_frames: HashMap<usize, (usize, bool)>,
}

/// The builder's state for one splice in progress.
pub(in crate::ir) struct SpliceFrame {
    /// `ClassId` of the spliced callee (`IrInlineSite::class_id`).
    pub(in crate::ir) class_id: u32,
    /// Caller pc to resume at — the instruction after the spliced `invoke`.
    pub(in crate::ir) return_pc: usize,
    /// One past the callee's last bytecode, in combined-buffer coordinates.
    pub(in crate::ir) end: usize,
    pub(in crate::ir) saved_locals: Vec<NodeId>,
    pub(in crate::ir) saved_stack: Vec<NodeId>,
    pub(in crate::ir) returns_value: bool,
    /// The callee's JVMS §6.5 `ireturn` narrowing
    /// ([`crate::narrowed_int_return_tag`] of its `method_key`), applied to the
    /// value each of its `ireturn`s hands back. `None` for `I` and for a
    /// fixture with no key.
    pub(in crate::ir) return_narrow: Option<u8>,
    /// This body has more than one reachable `return`, so a `return` is NOT a
    /// splice exit: it is an edge into the continuation built at [`Self::end`].
    /// Decided once, by the pre-scan in [`IrBuilder::build`], from the same
    /// verified decode the merge targets come from — never from the walk,
    /// which meets the returns one at a time and cannot know it is at the last.
    pub(in crate::ir) multi_return: bool,
    /// Remaining receiver arms for this frame. Keeping the continuation with
    /// its body allows that body to contain another guarded receiver chain.
    pub(in crate::ir) bimorphic_reopen: Option<BimorphicReopen>,
    /// One `(ctrl, mem, value)` per `return` the walk has reached, in walk
    /// order. `value` is [`NO_NODE`] for a `void` callee.
    ///
    /// The caller's locals and operand stack need no entry here: they are
    /// saved above and are the same on every path out of the callee, because
    /// the callee cannot reach them.
    pub(in crate::ir) exits: Vec<(NodeId, NodeId, NodeId)>,
    /// Combined-buffer pc of the last `return` recorded in [`Self::exits`].
    ///
    /// The continuation's `Merge` and `Phi`s are stamped with this rather than
    /// with `end`: `end` is one PAST the body, so `spliced_ranges` would
    /// resolve it to the caller and `resume_bci` would stop rewriting these
    /// nodes to the enclosing `invoke`. See [`IrInlineSite`] on the two
    /// meanings of `bytecode_pc`.
    pub(in crate::ir) exit_bci: usize,
    /// `[start, end)` ranges, in combined-buffer pcs, in which the walk visits
    /// a body with a rotated loop (see [`BlockWalk`]). Empty for a body walked
    /// in pc order. A walked body's `return`s are exit edges, as for
    /// [`Self::multi_return`], because reverse post-order need not visit the
    /// trailing `return` last; the body closes when its ranges run out.
    pub(in crate::ir) ranges: Vec<(usize, usize)>,
    /// Index into [`Self::ranges`] of the next range to visit.
    pub(in crate::ir) next_range: usize,
    /// End of the range being walked. Unused when `ranges` is empty.
    pub(in crate::ir) range_end: usize,
    /// The constructor receiver THIS frame added to
    /// `IrBuilder::splice_ctor_receivers`, removed again when the frame closes
    /// (round 11 wave 15, lane `ir`). `None` for a non-constructor body and for
    /// a super-constructor spliced inside a constructor already open on the
    /// same receiver, which leaves the entry to the outer frame.
    ///
    /// The receiver rule is sound only while the object is still being
    /// constructed. Once its `<init>` returns, the caller can publish it
    /// (`holder.c = c`), and a later splice that reads its field back through
    /// that alias (`holder.c.n`) is a read the rule never sees -- it tracks
    /// node identity -- so a trap after the store re-ran a store that the
    /// replay then computed from its own first result.
    pub(in crate::ir) ctor_receiver: Option<NodeId>,
    /// The body's receiver (argument 0 of an instance callee), or [`NO_NODE`]
    /// for a static one. Non-null for the whole body: `begin_splice` proved it
    /// or guarded it at the `invoke` (round 12 wave 5, lane iropt4; see
    /// [`SpliceFenceRelax`], relaxation 2).
    pub(in crate::ir) receiver: NodeId,
    /// The value bound to the body's local 0 at the `invoke` (argument 0,
    /// the receiver of an instance callee), or [`NO_NODE`] for a callee with
    /// no arguments. Read only by [`IrBuilder::exact_receiver_node_proof`]
    /// (`CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`), for a static body that
    /// never writes local 0: there local 0 is that value at every point.
    pub(in crate::ir) arg0: NodeId,
    /// Round 13 wave 10 (lane iropt8, I8-2): the class a GUARDED splice's
    /// `ExactClassIs` proved [`Self::receiver`] to be on the edge this body is
    /// built on (`0` for every other splice). Scoped to this frame: never
    /// recorded as a node fact, because the receiver is that class on the hit
    /// edge only.
    pub(in crate::ir) receiver_exact_class: u32,
    /// Round 13 wave 3 (lane framestate): the combined-buffer pc of the
    /// `invoke` this splice replaced -- in the ENCLOSING scope's code (the
    /// compiling method's for an outermost splice, the enclosing body's for a
    /// nested one). The parked caller frame's bci is this minus that scope's
    /// [`Self::base`] (0 for the compiling method).
    pub(in crate::ir) invoke_pc: usize,
    /// The callee body's first pc in the combined buffer
    /// ([`IrInlineSite::base`]); a pc inside it is `base + bci`.
    pub(in crate::ir) base: usize,
    /// The callee's [`IrInlineSite::method_key`], its `code_len` and its
    /// `max_locals`: the identity and limits of this scope in a
    /// [`SpliceScopeChain`].
    pub(in crate::ir) method_key: String,
    pub(in crate::ir) code_len: usize,
    pub(in crate::ir) max_locals: usize,
    /// Round 14 wave 2 (lane syncsplice): the monitor window of a
    /// synchronized splice (its `exit` still [`NO_NODE`]), or `None`. See
    /// [`SyncSpliceWindow`].
    pub(in crate::ir) sync_monitor: Option<SyncSpliceWindow>,
    /// JIT round 19 (lane splice19, CH6W5-1): this is the hit edge of a
    /// guarded splice opened INSIDE another splice, whose miss edge is the
    /// real call named here. That call has committed whatever it did once the
    /// join runs, so [`IrBuilder::finish_multi_return_splice`] raises the
    /// enclosing body's [`IrBuilder::splice_call_committed`] there (and not
    /// before: the hit body never runs after the call).
    ///
    /// JIT round 19 wave 2 (lane splice2, SPLICE19-6): the call's other
    /// effects ([`IrBuilder::note_splice_call`]'s taints of splice-local
    /// arguments and its read of the escaped constructor receivers) are
    /// recorded at the join too, not when the call is built: the hit body is
    /// a path the call never ran on, so it keeps the locality the eager record
    /// took from it. `None` for every other frame. For a nested BIMORPHIC
    /// splice (SPLICE19-4) the innermost body's frame names the call, and its
    /// join hands it on to the next body's frame
    /// ([`IrBuilder::reopen_bimorphic_first_body`]), so it is recorded once,
    /// at the outermost join.
    pub(in crate::ir) miss_call: Option<NodeId>,
    /// JIT round 23 wave 1, lane L2 (INLINE22-4,
    /// `CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE`): this body is a lambda proxy's
    /// synthesised SAM forwarder ([`ir_splice_body_is_lambda_forwarder`]),
    /// which has no interpreter frame to rebuild. A snapshot inside it records
    /// no chain (a trap there resumes at the outermost `invoke`, which nothing
    /// before its one call has committed past), and a chain through it skips
    /// its scope: the implementation's frame is parked directly under the
    /// `invoke` the forwarder replaced, as the interpreter's own lambda
    /// dispatch runs it ([`IrBuilder::splice_scope_chain`]).
    pub(in crate::ir) frameless: bool,
    /// JIT round 23 wave 2, lane L2 (INLINE22-4 stage 2): a [`Self::frameless`]
    /// forwarder that makes more than one call (LambdaMetafactory's boxing /
    /// unboxing around the implementation): its scope cannot be skipped (a
    /// resumed implementation would return around the conversion), so no
    /// chain is recorded anywhere under it and every trap in its subtree
    /// resumes at the outermost `invoke`, under the replay fences no chain
    /// relaxed ([`IrInlineFrameSites::pc_resumes_flat_under_forwarder`]).
    pub(in crate::ir) opaque: bool,
    /// JIT round 23 wave 2d, lane L2 (L2W1-3): a [`Self::frameless`]
    /// forwarder nested inside another splice keeps the operands its `invoke`
    /// popped (receiver first), so a trap inside it can resume the ENCLOSING
    /// body at that `invoke`, re-executed ([`IrBuilder::forwarder_reexecute_chain`]).
    /// Empty everywhere else.
    pub(in crate::ir) reexec_args: Vec<NodeId>,
    pub(in crate::ir) caller_mirror_slot: usize,
    pub(in crate::ir) security_walk_ignored: bool,
    pub(in crate::ir) param_nodes: Vec<NodeId>,
}

/// Receiver bodies waiting for the current splice frame to finish. Its join
/// becomes the first exit edge of the next body.
pub(in crate::ir) struct BimorphicReopen {
    /// The `invoke` both bodies replace, and its length.
    pub(in crate::ir) pc: usize,
    pub(in crate::ir) instr_len: usize,
    /// The bodies still to walk, outermost test LAST: each is its class
    /// test's hit edge, its class, and its row's key in the site table (`pc`
    /// itself for the site's own first body). Round 16 (lane mega): a
    /// bimorphic site has one entry (the first body); a polymorphic site has
    /// one per body but the innermost, which is walked first. Popped from the
    /// back, so the bodies are walked innermost test first.
    pub(in crate::ir) pending: Vec<(NodeId, u32, usize)>,
    /// The memory state before the `invoke` (every edge starts from it).
    pub(in crate::ir) mem: NodeId,
    /// The `invoke`'s operands, receiver first, pushed back for the first
    /// body's `begin_splice`.
    pub(in crate::ir) args: Vec<NodeId>,
    pub(in crate::ir) receiver: NodeId,
    /// SPLICE19-4: the enclosing body's replay-fence state at the `invoke`
    /// ([`IrBuilder::splice_committed_bases`],
    /// [`IrBuilder::splice_call_committed`]), which every body starts from.
    /// An outermost site clears them instead, as it always did (they are
    /// empty there: the walk clears them back in the caller).
    pub(in crate::ir) saved_committed_bases: HashSet<NodeId>,
    pub(in crate::ir) saved_call_committed: bool,
}

/// Round 14 wave 2 (lane guard, GS-1): a bimorphic site's SECOND body is kept
/// in the one-body-per-caller-pc site table ([`IrInlineTables::sites`]) under
/// this key: the caller pc with a tag bit no bytecode pc can carry, so no
/// instruction lookup ever finds it. Every consumer that reads a caller pc
/// back out of a key goes through [`inline_site_caller_pc`].
pub(in crate::ir) const BIMORPHIC_SECOND_SITE_TAG: usize = 1 << (usize::BITS - 2);

/// The site-table key of the second body of the bimorphic site at `caller_pc`.
pub(crate) fn bimorphic_second_site_key(caller_pc: usize) -> usize {
    caller_pc | BIMORPHIC_SECOND_SITE_TAG
}

/// Round 16 (lane mega): a polymorphic site's third and later bodies carry
/// their body index (minus one) in the four bits under the tag, so the
/// second body's key is unchanged ([`polymorphic_site_key`]).
pub(in crate::ir) const POLYMORPHIC_BODY_SHIFT: u32 = usize::BITS - 6;
pub(in crate::ir) const POLYMORPHIC_BODY_MASK: usize = 0xF << POLYMORPHIC_BODY_SHIFT;

/// The site-table key of body `body` of the guarded site at `caller_pc`:
/// `caller_pc` itself for the first body (the site's own row), the bimorphic
/// key for the second, and the same key with the body index under the tag for
/// the third and later ones (round 16, lane mega; at most 16 bodies).
pub(crate) fn polymorphic_site_key(caller_pc: usize, body: usize) -> usize {
    match body {
        0 => caller_pc,
        b => {
            bimorphic_second_site_key(caller_pc)
                | (((b - 1) << POLYMORPHIC_BODY_SHIFT) & POLYMORPHIC_BODY_MASK)
        }
    }
}

/// The caller pc a site-table key names ([`bimorphic_second_site_key`]'s tag
/// and [`polymorphic_site_key`]'s body index removed; every other key is its
/// own caller pc, which never reaches those bits).
pub(crate) fn inline_site_caller_pc(key: usize) -> usize {
    key & !(BIMORPHIC_SECOND_SITE_TAG | POLYMORPHIC_BODY_MASK)
}

/// Hard cap on inline-scope chain length.
///
/// Bounds the walks in [`IrInlineFrameSites`], which recovers a stack trace's
/// inlined levels by following each spliced body's caller pc outward: a
/// malformed table (a body whose caller pc lands back inside itself) must not
/// turn into an unbounded loop. HotSpot's own inline depth limit is 9
/// (`MaxInlineLevel` + `MaxRecursiveInlineLevel`), so 64 is far above anything
/// a real inliner produces.
///
/// It was introduced for `InlineScopeTable`, which round 10 wave 9 retired (see
/// the note below); the frame-site walks were always the other user and are the
/// reason it survives.
pub const MAX_INLINE_SCOPE_DEPTH: usize = 64;

// `InlineScope`, `InlineScopeId` and `InlineScopeTable` were DELETED in round
// 10 wave 9, together with `ir_lower::Lowerer::caller_chain_for`.
//
// They were the IR tier's half of the inlined-deopt-scope design
// (`docs/jit/deopt-inline-scopes.md`): a side table binding each
// `Graph::safepoints` entry to the chain of caller frames parked mid-`invoke`
// above it, which `ir_lower` read to fill `deopt::FrameState::caller`.
//
// The READ side was wired to production. The WRITE side never was: `push_scope`,
// `bind_snapshot` and `bind_snapshot_range` had no caller outside `#[cfg(test)]`
// anywhere in the workspace, so `is_empty()` was true on every compile and every
// artifact this tier installed carried `caller: None`. That is the same
// "staged, never reached" shape that got the interned frame-state
// representation deleted in the same round — and it was WORSE than the
// interner's, because the interner had no production reference at all (a `rg`
// told the truth about it) while a `rg` for `InlineScopeTable` found production
// hits and told a reader this tier described inlined scope chains.
//
// Nothing is lost that a future producer needs:
//
//   * the metadata shape, including the fail-closed `Unsupported` caller frame
//     and the cross-scope `emitted`-set gap the deleted lowering had, is in
//     `docs/jit/deopt-inline-scopes.md`;
//   * `deopt::FrameState::caller`, `deopt::MAX_SCOPE_CHAIN` and
//     `frame_state_is_resumable`'s caller walk all stay — the SINGLE-PASS
//     backend can build a chain (`Compiler::inline_caller_chain`), so the
//     consumer side is live code, not staging;
//   * the VM can already resume one:
//     `interpreter::deopt_resume::materialise_inlined_chain` walks a chain and
//     `scope_resume_pc` parks each scope by its own `ResumeSemantics`;
//   * the decision and its argument are
//     `docs/feature-designs/deopt-osr.md`
//     and `docs/feature-designs/jit-ir-optimizations.md` §2.
//
// Re-landing this is a PRODUCER change first: `IrBuilder::build` does not
// inline, and `IrInlineSite` splicing deliberately carries the caller's
// `invoke` bci with re-execute semantics instead of a scope chain (see the note
// above `IrInlineFrameSites`). A table with no populator is not a head start.

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
    /// `SafepointSnapshot::sb_virtual_builders` value slots
    SbVirtual,
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
///   *superset*: [`Graph::clear_safepoint_slots_where`] (the dead-slot
///   normalisation in `ir_optimize`, the liveness prune in `ea_ir_bridge`)
///   clears slots without notifying the lists. Clearing only ever *removes* a
///   reference, and every rewrite re-checks the slot's current value before
///   touching it, so a stale entry is inert. A write that *installs* a new
///   node id goes through [`Graph::set_safepoint_slot`]; any other write
///   through the public `Graph::safepoints` field bumps the edge epoch (see
///   [`Safepoints`], round 11 wave 18), so it demotes the lists instead of
///   being missed by them.
///
/// Both invariants are checked by [`Graph::verify_use_lists`].
///
/// Edge writes that bypass the mutators (through the public `Node::inputs` or
/// `Graph::safepoints` fields) bump [`EDGE_EPOCH`] and demote the next
/// mutation to a full scan, so bypassing costs performance, never correctness.
#[derive(Clone)]
pub struct UseLists {
    /// `users[i]` = one entry per input slot naming node `i`.
    pub(in crate::ir) users: Vec<Inputs>,
    /// `sp_users[i]` = snapshot slots naming node `i` (superset — see above).
    pub(in crate::ir) sp_users: Vec<Vec<SafepointSlot>>,
    /// Master switch. Off means "never maintain, never consult".
    pub(in crate::ir) tracking: bool,
    /// Whether `users` / `sp_users` currently describe the graph.
    pub(in crate::ir) valid: bool,
    /// Value of [`EDGE_EPOCH`] when the lists were last reconciled.
    pub(in crate::ir) epoch: u64,
    /// [`THREAD_TOKEN`] of the thread that reconciled them. 0 = never.
    pub(in crate::ir) owner: u64,
    /// `Graph::safepoints.len()` when the lists were last reconciled.
    pub(in crate::ir) sp_len: usize,
    /// An edge names an id past the end of the arena. Such an edge is invisible
    /// to `use_counts` today, but a later `add` could make the id real, so the
    /// lists are re-derived rather than extended while this is set.
    pub(in crate::ir) dangling: bool,
    /// Instrumentation: input slots examined by `replace_all_uses`.
    pub(in crate::ir) examined: u64,
    /// Instrumentation: full rebuilds performed.
    pub(in crate::ir) rebuilds: u64,
}

