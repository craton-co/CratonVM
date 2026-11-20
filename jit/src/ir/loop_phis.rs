#[allow(unused_imports)]
use super::*;

impl Graph {
    /// The bci a frame parked in node `id` (lowered at bytecode pc `pc`)
    /// reports to a stack walk, a debugger and the throw-site stamp: `pc`,
    /// except for an `Op::Call` listed in [`Graph::call_reported_bcis`]. The
    /// node's by-pc rows (direct entry, inline cache, exception exit, deopt
    /// anchor, post-call exit) stay keyed by `pc`; only the reported bci moves.
    pub fn reported_call_bci(&self, id: NodeId, pc: usize) -> usize {
        let is_call = self
            .node_opt(id)
            .is_some_and(|n| matches!(n.op, Op::Call { .. }));
        if !is_call {
            return pc;
        }
        self.call_reported_bcis
            .iter()
            .find(|&&(row_pc, _)| row_pc == pc)
            .map_or(pc, |&(_, bci)| bci)
    }

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
        // JIT round 19 wave 2 (lane irops2, IROPS19-1): a direct static
        // access is a plain read / write of exactly one static cell.
        if self.static_alias_class {
            if let Some((cell, is_write)) = self.direct_static_access(node) {
                return if is_write {
                    MemEffect::write(cell)
                } else {
                    MemEffect::read(cell)
                };
            }
            // JIT round 19 wave 4 (lane irops3, IROPS2-5): the reference
            // store's helper call is still a call (a safepoint map, and the
            // SATB pre-barrier reads the old value), but of an initialised
            // class it runs no `<clinit>` and writes only its own cell.
            if let Some(cell) = self.direct_static_ref_store(node) {
                return MemEffect {
                    reads: cell,
                    writes: cell,
                    order: MemOrder::Plain,
                    safepoint: true,
                    allocates: false,
                };
            }
        }
        // Round nio20 wave 4 (lane L1, W3L4-1): the scoped fold's raw class.
        if self.scoped_raw_alias {
            if let Some(eff) = scoped_raw_effect(node) {
                return eff;
            }
        }
        // Round nio20 wave 6 (lane L1): a direct-route plain `getstatic`.
        if !self.plain_static_reads.is_empty() {
            if let Op::LoadStatic {
                class_id,
                field_index,
                ..
            } = node.op
            {
                let plain = self.plain_static_reads.contains(&(class_id, field_index));
                if let Some(eff) = plain_static_read_effect(node, plain) {
                    return eff;
                }
            }
        }
        let mut eff = effect_of_node(node);
        eff.reads = self.fold_offsets(eff.reads);
        eff.writes = self.fold_offsets(eff.writes);
        // Round 15 wave 3 (lane chain2, CHN15-2): the resolved field's
        // declaring class, for a builder `getfield` / `putfield` node only.
        if let Some(decl) = self.field_identity_of(id) {
            eff.reads = with_field_decl(eff.reads, decl);
            eff.writes = with_field_decl(eff.writes, decl);
        }
        eff
    }

    /// JIT round 19 wave 2 (lane irops2, IROPS19-1): the static cell a
    /// DIRECT static access of this compile touches, and whether it writes.
    ///
    /// * a non-volatile `Op::LoadStatic` in the full `[ctrl, mem]` form whose
    ///   field is in [`Self::direct_static_fields`] with the inline route on:
    ///   `ir_lower::emit_inline_getstatic`'s two `MOV`s through the class's
    ///   baked statics-base cell. No call, no `<clinit>` (the class was
    ///   initialised at compile time), no safepoint map, no trap.
    /// * a non-volatile PRIMITIVE `Op::StoreStatic` in the full
    ///   `[ctrl, mem, value]` form on the same list with
    ///   `CRATONVM_JIT_INLINE_PUTSTATIC` on: `emit_inline_putstatic`'s direct
    ///   store into the one 16-byte cell. A reference `putstatic` keeps the
    ///   helper (SATB pre-barrier, a call and a map) and stays opaque.
    ///
    /// Everything else answers `None` (the node keeps its table effect). A
    /// class's static storage is not any object's instance or element
    /// storage (the premise [`Graph::may_alias`] records), so a cell found
    /// here commutes with every field, array and length access, and with a
    /// direct access of any OTHER cell.
    ///
    /// Asked by [`Graph::memory_effect`] only under
    /// [`Self::static_alias_class`]; `ir_optimize`'s direct-static LICM asks
    /// it under the same switch.
    pub(crate) fn direct_static_access(&self, node: &Node) -> Option<(AliasClass, bool)> {
        if self.direct_static_fields.is_empty() {
            return None;
        }
        let (class_id, field_index, is_write) = match node.op {
            Op::LoadStatic {
                class_id,
                field_index,
                is_volatile: false,
                ..
            } if node.inputs.len() >= 2 => (class_id, field_index, false),
            Op::StoreStatic {
                class_id,
                field_index,
                type_tag: b'I' | b'Z' | b'B' | b'C' | b'S' | b'J' | b'F' | b'D',
                is_volatile: false,
            } if node.inputs.len() >= 3 => (class_id, field_index, true),
            _ => return None,
        };
        if !self.direct_static_fields.contains(&(class_id, field_index))
            || !crate::x64::inline_getstatic_enabled()
            || (is_write && !crate::ir_lower::inline_putstatic_enabled())
        {
            return None;
        }
        Some((
            AliasClass::Static {
                class_id,
                field: field_index,
            },
            is_write,
        ))
    }

    /// JIT round 19 wave 4 (lane irops3, IROPS2-5): the static cell a
    /// non-volatile REFERENCE `putstatic` (`type_tag` `L` or `[`, the full
    /// `[ctrl, mem, value]` form) writes, when its field is in
    /// [`Self::direct_static_fields`] and [`Self::static_ref_store_effect`] is
    /// on.
    ///
    /// The store keeps its helper (`jit_putstatic_object`: a call with a
    /// safepoint map). What is exact about it is the memory it touches: the
    /// field's class was initialised at compile time
    /// (`DirectHelperTable::resolve_static_base`), so the helper's class-init
    /// guard answers from its memo and runs no Java code, and
    /// `set_static_shared` writes exactly this one cell (logging the old
    /// reference for SATB, which reads the cell). It allocates nothing. So the
    /// effect is `read + write(Static{class, field})` with `safepoint`: it
    /// orders against every write (the safepoint rule) but commutes with
    /// field, array and length READS and with direct accesses of other
    /// static cells -- which is what lets a loop that publishes a reference
    /// (`INSTANCE = ...`, a lazily built cache) keep its load hoists.
    pub(crate) fn direct_static_ref_store(&self, node: &Node) -> Option<AliasClass> {
        if !self.static_ref_store_effect || self.direct_static_fields.is_empty() {
            return None;
        }
        let (class_id, field_index) = match node.op {
            Op::StoreStatic {
                class_id,
                field_index,
                type_tag: b'L' | b'[',
                is_volatile: false,
            } if node.inputs.len() >= 3 => (class_id, field_index),
            _ => return None,
        };
        if !self.direct_static_fields.contains(&(class_id, field_index)) {
            return None;
        }
        Some(AliasClass::Static {
            class_id,
            field: field_index,
        })
    }

    /// Round nio20 wave 4 (lane L1, W3L4-1): is `id`'s safepoint (as
    /// [`Self::memory_effect`] reports it) only a DEOPT -- a transfer out of
    /// the compiled body that never comes back -- rather than a call, an
    /// allocation or a poll at which the collector may run and compiled code
    /// continues? True only for the scoped fold under
    /// [`Self::scoped_raw_alias`] ([`scoped_raw_effect`]).
    ///
    /// The difference matters to a pass that hands a value computed BEFORE
    /// the node to a use AFTER it (load CSE): across a deopt-only safepoint
    /// either the deopt is taken, and no later compiled code runs, or it is
    /// not, and nothing happened that could move an object or write memory
    /// the alias class does not name.
    pub fn safepoint_is_deopt_only(&self, id: NodeId) -> bool {
        self.scoped_raw_alias
            && self
                .node_opt(id)
                .is_some_and(|n| scoped_raw_effect(n).is_some())
    }

    /// Round 15 wave 3 (lane chain2, CHN15-2): the declaring class id
    /// [`Graph::memory_effect`] puts into `id`'s [`AliasClass::Field`]:
    /// [`Self::field_decl_class`]'s row for a live `Op::Load` / `Op::Store`,
    /// and only while [`Self::field_identity_alias`] is on.
    pub(in crate::ir) fn field_identity_of(&self, id: NodeId) -> Option<u32> {
        if !self.field_identity_alias || self.field_decl_class.is_empty() {
            return None;
        }
        let node = self.node_opt(id)?;
        if !matches!(node.op, Op::Load(_) | Op::Store(_)) {
            return None;
        }
        self.field_decl_class.get(&id).copied()
    }

    /// Round 15 wave 3 (lane chain2, CHN15-2): do `a` and `b` both have a
    /// declaring-class row ([`Self::field_decl_class`]) and name DIFFERENT
    /// classes? Asked by the G14-1 census whatever
    /// [`Self::field_identity_alias`] says.
    pub(crate) fn field_decls_differ(&self, a: NodeId, b: NodeId) -> bool {
        match (self.field_decl_class.get(&a), self.field_decl_class.get(&b)) {
            (Some(x), Some(y)) => x != y,
            _ => false,
        }
    }

    /// [`Self::may_alias`] with the field identities of both classes dropped:
    /// the answer the index-only model gives (the G14-1 census's baseline).
    pub(crate) fn may_alias_by_index(&self, a: AliasClass, b: AliasClass) -> bool {
        self.may_alias(without_field_decl(a), without_field_decl(b))
    }

    /// Replace a [`AccessOffset::Dynamic`] offset with [`AccessOffset::Const`]
    /// when the node it names is an integer constant.
    pub(in crate::ir) fn fold_offsets(&self, class: AliasClass) -> AliasClass {
        let fold = |off: AccessOffset| match off {
            AccessOffset::Dynamic(id) => match self.node_opt(id).map(|n| &n.op) {
                Some(Op::Const(v)) => AccessOffset::Const(*v),
                _ => off,
            },
            other => other,
        };
        match class {
            AliasClass::Field { base, offset, decl } => AliasClass::Field {
                base,
                offset: fold(offset),
                decl,
            },
            AliasClass::ArrayElem { array, index, elem } => AliasClass::ArrayElem {
                array,
                index: fold(index),
                elem,
            },
            other => other,
        }
    }

    /// What reference `id` may point to, following `Op::Phi` merges.
    ///
    /// Leaves: `Op::New` / `Op::NewArray` are exactly themselves (the node *is*
    /// the reference — see the builder's `new` arm); `Op::Param` is a
    /// pre-existing reference; anything else is unknown provenance. A φ joins
    /// its value inputs, skipping the control anchor at its head.
    pub fn ref_origin(&self, id: NodeId) -> RefOrigin {
        // `HashSet::new()` does not allocate until its first insert, and only a
        // φ ever inserts, so the overwhelmingly common case (a `New`, a
        // `Param`, an unknown leaf) still costs nothing.
        let mut visited: HashSet<NodeId> = HashSet::new();
        self.ref_origin_at(id, 0, &mut visited)
    }

    /// The recursive half of [`Graph::ref_origin`].
    ///
    /// # `visited` is not (only) a cycle guard — it is what makes this linear
    ///
    /// [`REF_ORIGIN_MAX_DEPTH`] bounds the *depth*, not the number of paths,
    /// and a φ web is a DAG: `φ_k = φ(φ_{k-1}, φ_{k-2})` makes the path count
    /// grow like Fibonacci, so thirty-two φ nodes — ordinary Java
    /// (`x3 = c ? x2 : x1; x4 = c ? x3 : x2; …`) — cost about 2.2 million node
    /// visits per query. `refs_may_alias` runs two of these, and
    /// `ir_optimize` asks it per pair of memory operations, so the blow-up is
    /// in compile time on a graph nothing else refuses.
    ///
    /// Skipping a φ this query already processed is sound because the join is a
    /// monotone union over the LEAVES reachable from the root: a node's
    /// contribution is absorbed the first time it is reached, and absorbing it
    /// again into a sibling's partial join cannot add anything the root does
    /// not already have. A repeat visit therefore answers the identity element
    /// of [`RefOrigin::absorb`] — an empty allocation set, `pre_existing =
    /// false` — and **not** [`RefOrigin::unknown`], which would poison the
    /// root with a fact the graph does not support.
    ///
    /// The mark is taken AFTER the depth test, deliberately: a node whose first
    /// visit was cut off by the depth bound is left unmarked, so a later,
    /// shallower path still gets to derive it properly.
    pub(in crate::ir) fn ref_origin_at(&self, id: NodeId, depth: u32, visited: &mut HashSet<NodeId>) -> RefOrigin {
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
                if !visited.insert(id) {
                    // Already absorbed into this query's root. The identity of
                    // `absorb`, not `unknown()`.
                    return joined;
                }
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
                    joined.absorb(self.ref_origin_at(inp, depth + 1, visited));
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
    ///   [`AliasClass::Any`], not as one of the structured classes. The one
    ///   raw access that is NOT `Any` is the `ScopedMemoryAccess` fold, whose
    ///   lowering admits only a primitive array's payload or off-heap memory:
    ///   [`AliasClass::Raw`], which overlaps every raw access and every
    ///   primitive element and nothing else (round nio20 wave 4).
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
    /// * **Two fields of different DECLARING classes never overlap** (round 15
    ///   wave 3, lane chain2, CHN15-2; only when both classes carry one, which
    ///   [`Graph::memory_effect`] fills under [`Graph::field_identity_alias`]).
    ///   A field is `(declaring class, name, descriptor)` (JVMS 5.4.3.2 names
    ///   the class the lookup found it in, and the resolver records exactly
    ///   that class), so two different declaring classes are two different
    ///   Java variables. In the VM's layout they are two different slots of
    ///   any one object as well: a class's own instance fields sit at
    ///   `first_field_index = superclass.num_total_fields` and up, so a slot
    ///   index belongs to exactly one class of an object's superclass chain,
    ///   and an interface declares no instance field. So two accesses at the
    ///   same constant index with different declaring classes cannot be on one
    ///   object, whatever the bases' provenance. The premise a wrong answer
    ///   would break is the resolver's declaring class (read off the same
    ///   resolution the index comes from, never from a name) and a
    ///   well-typed receiver (a verified `getfield`'s receiver is an instance
    ///   of the field's class). As for the element kinds, the `x != y` guard
    ///   keeps one base node read as two classes' field -- only on a dead or
    ///   ill-typed path -- a must-alias.
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
            (
                C::Field {
                    base: x,
                    offset: i,
                    decl: d,
                },
                C::Field {
                    base: y,
                    offset: j,
                    decl: e,
                },
            ) => {
                let decls_disjoint = x != y && matches!((d, e), (Some(d), Some(e)) if d != e);
                !decls_disjoint && !i.provably_distinct(j) && self.refs_may_alias(x, y)
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
            // Round nio20 wave 4 (lane L1, W3L4-1): raw memory is some
            // primitive array's payload or off-heap memory, at an address the
            // model cannot place -- so every other raw access and every
            // primitive element, whatever the array, and nothing else (see
            // `AliasClass::Raw`).
            (C::Raw, C::Raw) => true,
            (C::Raw, C::ArrayElem { elem, .. }) | (C::ArrayElem { elem, .. }, C::Raw) => {
                elem != MemKind::Ref
            }
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
pub(in crate::ir) struct LoopPhis {
    /// Phi for each local variable slot (`NO_NODE` = none).
    pub(in crate::ir) local_phis: Vec<NodeId>,
    /// Phi for each operand-stack slot at the header (usually empty).
    pub(in crate::ir) stack_phis: Vec<NodeId>,
    /// Phi for the memory token (`NO_NODE` = none).
    pub(in crate::ir) mem_phi: NodeId,
}

/// State at a bytecode merge point (branch target / loop header).
#[derive(Clone)]
pub(in crate::ir) struct MergeState {
    /// The Merge / Region node for this target.
    pub(in crate::ir) merge_id: NodeId,
    /// Control inputs collected so far.
    pub(in crate::ir) ctrl_inputs: Vec<NodeId>,
    /// Locals state from each predecessor (one Vec<NodeId> per predecessor).
    pub(in crate::ir) local_snapshots: Vec<Vec<NodeId>>,
    /// Stack state from each predecessor.
    pub(in crate::ir) stack_snapshots: Vec<Vec<NodeId>>,
    /// Memory token from each predecessor.
    pub(in crate::ir) mem_inputs: Vec<NodeId>,
    /// Abstract monitor stack from each FORWARD predecessor. Structured locking
    /// (JVMS §2.11.10) makes these identical on every edge into a join, and the
    /// builder refuses the method when they are not — see
    /// [`IrBuilder::unstructured_monitors`].
    pub(in crate::ir) monitor_snapshots: Vec<Vec<NodeId>>,
    /// Whether this merge has been visited (target reached during forward walk).
    pub(in crate::ir) visited: bool,
}

impl MergeState {
    /// Move the recorded predecessor frames out for activation (round 11
    /// wave 2).
    ///
    /// Both activation paths used to `clone()` the whole state — every
    /// predecessor's locals, stack and monitors — and leave the original in
    /// `IrBuilder::merges` until the build ended, so each join cost its
    /// predecessor frames twice over and kept them alive for the rest of the
    /// walk. Nothing reads them after activation except
    /// `IrBuilder::patch_loop_backedge`, which reads the FIRST predecessor's
    /// stack depth, locals length and monitor stack of a loop header;
    /// `keep_first` leaves exactly those behind. `ctrl_inputs` stays (a back
    /// edge appends to it, and it is a handful of ids).
    pub(in crate::ir) fn take_for_activation(&mut self, keep_first: bool) -> MergeState {
        let taken = MergeState {
            merge_id: self.merge_id,
            ctrl_inputs: self.ctrl_inputs.clone(),
            local_snapshots: std::mem::take(&mut self.local_snapshots),
            stack_snapshots: std::mem::take(&mut self.stack_snapshots),
            mem_inputs: std::mem::take(&mut self.mem_inputs),
            monitor_snapshots: std::mem::take(&mut self.monitor_snapshots),
            visited: self.visited,
        };
        if keep_first {
            if let Some(first) = taken.local_snapshots.first() {
                self.local_snapshots.push(first.clone());
            }
            if let Some(first) = taken.stack_snapshots.first() {
                self.stack_snapshots.push(first.clone());
            }
            if let Some(&first) = taken.mem_inputs.first() {
                self.mem_inputs.push(first);
            }
            if let Some(first) = taken.monitor_snapshots.first() {
                self.monitor_snapshots.push(first.clone());
            }
        }
        taken
    }
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
// `_ =>` arms and would have been fine.) It is the same rule that forced the
// retired `InlineScopeTable` to be a side table rather than a field on
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
// nio20 wave 3, lane L2 (proposal NIO20-L3-4): the refused guarded site is
// now answered one level down, as an inline PREFIX of its `Op::Call`
// (`ir_lower::Lowerer::try_emit_ir_string_search_prefix`): kind bits, then
// `[recv+0] == string_class_id` in machine code, then the same decode; a miss
// declines to the interface call, so no guard node and no deopt is needed.
// The same prefix answers `hashCode` / `equals` / `compareTo` / both
// `indexOf` (`SI_NOT_ACCESSOR` here), which is what lets `lib.rs`'s planning
// gate and pin admit methods containing them
// (`ir_string_search_prefix_handles`).
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

/// The first `java/lang/String` field layout any compile in this process
/// resolved — kept ONLY as the latch that makes [`publish_string_layout`]'s
/// generation bump fire once.
///
/// Until round 11 wave 3 this was also the layout the String expander decoded
/// against: [`IrBuilder::new`] seeded every builder from it, on the argument
/// that the four fields the expander reads (`value_field_index`,
/// `coder_field_index`, `has_coder`, `string_class_id`) are "fixed for the life
/// of the process". They are fixed per VM, not per process: two VMs in one
/// process can load different class libraries, and this `OnceLock` keeps the
/// FIRST one's indices, so VM B's compiled `charAt` would read VM A's slots
/// (`docs/feature-designs/jit-ir-optimizations.md`).
/// A builder now starts with NO layout and the compile driver hands it its own
/// compile's through [`IrBuilder::set_string_layout`]; a builder nobody hands
/// one to keeps every String call site dispatching and counts it under
/// `no_layout` in [`ir_string_intrinsic_census`] — slower, never wrong.
pub(in crate::ir) static PUBLISHED_STRING_LAYOUT: std::sync::OnceLock<crate::StringFieldLayout> =
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
    pub(in crate::ir) static STRING_ACCESS_SITE_PCS: std::cell::RefCell<Vec<usize>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Clear the per-build String-accessor site list.
pub(in crate::ir) fn reset_string_access_sites() {
    STRING_ACCESS_SITE_PCS.with(|v| v.borrow_mut().clear());
}

/// Record one expanded String-accessor site.
pub(in crate::ir) fn note_string_access_site(pc: usize) {
    STRING_ACCESS_SITE_PCS.with(|v| v.borrow_mut().push(pc));
}

/// The pcs at which this thread's most recent [`IrBuilder::build`] expanded a
/// String accessor. Empty when the emitter did not fire.
pub fn string_access_site_pcs() -> Vec<usize> {
    STRING_ACCESS_SITE_PCS.with(|v| v.borrow().clone())
}

/// Record that a `java/lang/String` layout has been resolved in this process.
///
/// The first call bumps the acceptance gate's inputs generation and later
/// calls do nothing. The layout itself is NOT what a builder decodes against
/// any more (round 11 wave 3): see [`PUBLISHED_STRING_LAYOUT`] and
/// [`IrBuilder::set_string_layout`].
pub fn publish_string_layout(layout: crate::StringFieldLayout) {
    // The first resolvable layout changes what the String expander can do for
    // every method, so a refusal the acceptance gate recorded before it judged
    // inputs that no longer hold (round 9 wave 2,
    // `ir-refusal-memo-outlives-the-inputs-it-judged`).
    if PUBLISHED_STRING_LAYOUT.set(layout).is_ok() {
        crate::ir_evidence::bump_inputs_generation();
    }
}

/// Outcome names for the String-intrinsic census, in variant order — the
/// `SI_*` indices below index both this table and
/// [`STRING_INTRINSIC_COUNTS`], so the two cannot drift apart.
pub(in crate::ir) const STRING_INTRINSIC_OUTCOMES: [&str; 11] = [
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
    // JIT round 21 (lane strintr2, STRINTR21-1): a site whose guard the VM's
    // de-spec registry withdrew, built as its call
    // (`CRATONVM_JIT_IR_STRING_ACCESS_TRAP_WITHDRAW`).
    "trap_withdrawn",
];

pub(in crate::ir) const SI_SITES_SEEN: usize = 0;
pub(in crate::ir) const SI_FLAG_OFF: usize = 1;
pub(in crate::ir) const SI_NO_LAYOUT: usize = 2;
pub(in crate::ir) const SI_NOT_ACCESSOR: usize = 3;
pub(in crate::ir) const SI_GUARDED_REFUSED: usize = 4;
pub(in crate::ir) const SI_IN_SPLICE: usize = 5;
pub(in crate::ir) const SI_SHAPE_REFUSED: usize = 6;
pub(in crate::ir) const SI_EMITTED_LENGTH: usize = 7;
pub(in crate::ir) const SI_EMITTED_IS_EMPTY: usize = 8;
pub(in crate::ir) const SI_EMITTED_CHAR_AT: usize = 9;
pub(in crate::ir) const SI_TRAP_WITHDRAWN: usize = 10;

/// One relaxed counter per [`STRING_INTRINSIC_OUTCOMES`] entry.
pub(in crate::ir) static STRING_INTRINSIC_COUNTS: [AtomicU64; STRING_INTRINSIC_OUTCOMES.len()] = [
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
pub(in crate::ir) fn note_string_intrinsic(idx: usize) {
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
pub(crate) fn ir_string_intrinsic_census() -> Vec<(&'static str, u64)> {
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
///
/// `pub(crate)` since nio20 wave 1 (lane L3, NIO19-8): the String-intrinsic
/// pin in `lib.rs` (`string_intrinsic_pin_verdict`) asks it too, so a method
/// whose every String intrinsic site this expander emits is no longer pinned
/// to the single-pass backend -- and is pinned again the moment the expander
/// is switched off.
///
/// Read per String site at build time, no `static` (j27 LATCHES). The body is
/// the hand-rolled one on purpose: an EMPTY value is off here, and
/// `runtime_flag_default_on` would turn it on.
pub(crate) fn ir_string_intrinsics_enabled() -> bool {
    match None::<std::ffi::OsString> {
        Some(value) => !matches!(
            value.to_string_lossy().trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no" | ""
        ),
        None => true,
    }
}

/// `CRATONVM_JIT_IR_STRING_CHAR_DECODE` (JIT round 21, lane charat,
/// STRINTR21-5, page `jr21-strintr2-charat-loop-per-char-node-cost-FIXED-20261101.md`)
/// -- **default ON**, `=0` restores the branchless two-`ArrayLoad(Byte)`
/// combine byte for byte. On, the `charAt` expansion reads the char through
/// ONE [`Op::StrCharDecode`] behind its bounds guard (`TEST coder; MOVZX` byte
/// or word) instead of a shift, two checked byte loads and a six-node combine.
/// Read live, once per expanded `charAt` site (no `static`).
pub(crate) fn ir_string_char_decode_enabled() -> bool {
    true
}

/// `CRATONVM_DBG_IR_STRING` — print the running census on every expander
/// decision. Cached: an uncached flag read on a per-call-site path is how
/// `CRATONVM_DBG_COMPACT_INLINE` came to be 99.1% of all flag reads on a
/// `BigDecimal` run.
pub(in crate::ir) fn string_intrinsic_reporting() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| false)
}

/// JIT round 20 wave 11 (lane pIndy, SPLICE19-1;
/// `CRATONVM_JIT_INDY_MUTABLE_GUARD`): what the guarded plain call of one
/// `MutableCallSite` / `VolatileCallSite` `invokedynamic` compares and calls
/// through. Every address is the VM's and lives as long as the VM (the two
/// slots are never-released JNI global references, the bridge site is
/// process-lived). See [`IrBuilder::try_indy_mutable_guarded_call`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndyMutableGuard {
    /// The instruction's bridge site ([`Op::IndyBridge`]): the miss edge.
    pub bridge_site: usize,
    /// The root word holding the instruction's `CallSite`.
    pub call_site_slot: usize,
    /// The root word holding the target handle the call was planned against.
    pub target_slot: usize,
    /// `CallSite.target`'s field slot (the `getfield` index).
    pub field_index: u32,
    /// The planned call is an unbound VIRTUAL one (receiver first): a null
    /// receiver takes the bridge, whose handle raises the NPE.
    pub virtual_target: bool,
    /// JIT round 21 wave 1 (lane indy, INDY21-1,
    /// `CRATONVM_JIT_INDY_MUTABLE_BIMORPHIC`): the root word holding a SECOND
    /// target handle the site flips to, or `0` for a one-target guard. Its
    /// leaf has the planned call's invoke kind, so `virtual_target` serves
    /// both arms.
    pub second_target_slot: usize,
    /// The second arm's bridge site (a VM cell of kind `GUARDED_LEAF`, which
    /// the bridge helper dispatches to the second target's method), or `0`.
    pub second_site: usize,
    /// mcs21 (MCS21-2, `CRATONVM_JIT_INDY_MUTABLE_GUARD_ROW`, opt-in): the
    /// VM's `1 + compact displacement` of `CallSite.target` in the exact
    /// class of the instruction's call site, or `0` (none: the guard's load
    /// keeps the field-index arms, which read a compact `CallSite` through the
    /// checked `jit_getfield` helper). The call site is the one object the
    /// root word [`Self::call_site_slot`] names, so its class (and with it the
    /// layout this row was read from) is fixed for the guard's life. See
    /// `IrBuilder::note_indy_guard_row`.
    pub field_row: u32,
}

/// JIT round 20 wave 12 (lane qLamNew, W10L3-2 / W11L3-2 second half;
/// `CRATONVM_JIT_IR_INDY_CAPTURE_NEW`): what the optimizing tier allocates for
/// one linked CAPTURING lambda site -- the VM-spun proxy class and the type of
/// each capture, from the VM's `(proxy class, capture count)` fact and the
/// site's own descriptor. See [`IrBuilder::try_indy_capture_new`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndyCaptureNew {
    /// The proxy class id (`SharedVm::alloc_lambda_proxy_id`'s range): no
    /// `<clinit>`, no compact layout, capture `i` in field `i`.
    pub proxy_class_id: u32,
    /// One tag per capture, in field order: `I` (the whole int category),
    /// `J`, `F`, `D` or `L` (`indy_arg_type_tags`).
    pub capture_tags: Vec<u8>,
}

/// JIT round 21 wave 17 (lane records, IN16-6;
/// `CRATONVM_JIT_IR_INDY_RECORD_FOLD`): a javac-generated record `hashCode` /
/// `equals` body's `invokedynamic ObjectMethods.bootstrap`, as the field-wise
/// fold the bootstrap's handle chain computes. The VM answers it only for the
/// record class's OWN generated body (`final` class, javac's canonical getter
/// list, every getter a `REF_getField` of the class). See
/// [`IrBuilder::try_indy_record_fold`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndyRecordFold {
    /// `true` for `equals (R, Object)Z`, `false` for `hashCode (R)I`.
    pub equals: bool,
    /// The (final) record class: `equals`' `isInstance` is exactly its class
    /// id.
    pub record_class_id: u32,
    /// `(field index, descriptor tag)` of each getter, in the site's getter
    /// order: the `getfield` index the `0xb4` arm would read, and the field's
    /// first descriptor byte (`I` `S` `B` `C` `Z` `J` `F` `D` `L` `[`).
    pub components: Vec<(u32, u8)>,
    /// j27 wave 5 (lane RECORDS, RECORDS21-3): per component, is its field
    /// typed `Ljava/lang/String;`? Empty when the VM marked none.
    pub string_components: Vec<bool>,
    /// `String.hash`'s field slot, when a `String` component's hash may read
    /// the cached value first (`lib.rs`, `record_fold_admit_string_hash`,
    /// `CRATONVM_JIT_IR_INDY_RECORD_FOLD_STRING_HASH`). `None` from the VM.
    pub string_hash_slot: Option<u32>,
}

/// JIT round 21 wave 20 (lane mhfilter; MHBIND2-1,
/// `CRATONVM_JIT_MH_CONSTANT_COMPILE_FILTER` in the VM): a constant handle
/// site whose `static final` handle is a REAL filter species, built as two
/// plain calls ([`IrBuilder::set_mh_constant_filter_sites`]). The MAIN call is
/// the site's own `invoke_info` row (built by the invoke arm at the
/// `invokeExact` pc); the PRE call is the `invoke_info` row the planner keyed
/// by the site's `getstatic` pc ([`Self::pre_pc`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrMhFilterSite {
    /// The site's `getstatic` pc: the PRE call's row and bytecode pc.
    pub pre_pc: usize,
    /// `Some(p)`: `filterArguments` at site argument `p` (PRE is the filter,
    /// MAIN the target); `None`: `filterReturnValue` (PRE is the target, MAIN
    /// the filter).
    pub position: Option<u16>,
    /// The MAIN call is an instance (`invokevirtual`) row.
    pub main_virtual: bool,
    /// The PRE call is an instance (`invokevirtual`) row.
    pub pre_virtual: bool,
    /// The species' `argL0` slot (the target's direct handle), `0` unknown.
    pub target_slot: u32,
}

/// A converting REAL `asType` handle (`invoke_kind 7`) site's guards,
/// unboxes, and return boxing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IrMhConvertSite {
    pub pre_pc: usize,
    pub main_kind: u8,
    pub arity: u8,
    pub cast_class_ids: [u32; 8],
    pub cast_class_names: [String; 8],
    pub unbox_op: [u8; 8],
    pub unbox_class_ids: [u32; 8],
    pub box_return: u8,
}

/// Builds an IR `Graph` from JVM bytecode by abstract-interpreting
/// the operand stack and local variable state.
pub struct IrBuilder {
    /// The graph under construction.
    pub graph: Graph,
    /// Abstract operand stack: each entry is a `NodeId`.
    pub(in crate::ir) stack: Vec<NodeId>,
    /// Current value of each local variable (SSA: changes on each store).
    pub(in crate::ir) locals: Vec<NodeId>,
    /// Current control token.
    pub(in crate::ir) ctrl: NodeId,
    /// Current memory token.
    pub(in crate::ir) mem: NodeId,
    /// Abstract monitor stack, outermost first: the reference each executed
    /// `monitorenter` locked, popped by its `monitorexit`. Copied into every
    /// [`SafepointSnapshot::monitors`], which is how a lowered deopt frame
    /// learns which locks the interpreter must hold when it resumes.
    pub(in crate::ir) monitors: Vec<NodeId>,
    /// Set when the monitor stacks on two edges into one join disagree. Such a
    /// method has no single monitor state at the join to describe, so the build
    /// is refused (the single-pass backend still compiles it). Checked at the
    /// top of every instruction and once more at the end of [`Self::build`].
    pub(in crate::ir) unstructured_monitors: bool,
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
    pub(in crate::ir) build_refused: Option<(u32, usize)>,
    /// Number of JVM locals (including params).
    pub(in crate::ir) _num_locals: usize,
    /// Merge-point state, keyed by bytecode PC.
    pub(in crate::ir) merges: HashMap<usize, MergeState>,
    /// Bytecode PCs that are loop headers (targets of a backward branch).
    /// A loop header is activated with eager loop-carried phis whose back-edge
    /// input is back-patched when the backward branch is processed.
    pub(in crate::ir) loop_headers: HashSet<usize>,
    /// Loop-carried phis per loop-header PC, recorded at activation so the
    /// backward branch can fill each phi's back-edge input.
    pub(in crate::ir) loop_phis: HashMap<usize, LoopPhis>,
    /// Resolved instance-field layout, keyed by bytecode pc:
    /// `pc → (field_index, type_tag)`. Populated by the caller
    /// ([`Self::set_field_info`]) from the constant-pool field resolver before
    /// [`Self::build`]; empty for hand-built / test graphs. A `getfield` whose
    /// pc is absent — or whose tag is not int-category — makes `build` bail
    /// (`None` → single-pass), the existing safety net.
    pub(in crate::ir) field_info: HashMap<usize, (usize, u8)>,
    /// `getfield` / `putfield` pcs whose field is declared `volatile`. Set by
    /// [`Self::set_volatile_field_pcs`]; `build` refuses a field access at any
    /// of them unless the matching admission bit below is set, in which case
    /// it builds the access and marks it [`Node::volatile_access`].
    pub(in crate::ir) volatile_field_pcs: HashSet<usize>,
    /// nio21 wave 2 (lane L3): `getfield` pcs whose resolved field is declared
    /// `final` (`ACC_FINAL`) -- the method's own pcs and, rebased, the spliced
    /// bodies' ([`Self::add_final_field_pcs`]). A read at one of them is
    /// forwarded by the G14-1 memo under
    /// `CRATONVM_JIT_IR_FIELD_FINAL_FORWARD` (default ON) even when the
    /// general read switch is off. See [`Self::field_load_memo_forward`].
    pub(in crate::ir) final_field_pcs: HashSet<usize>,
    /// nio21 wave 7 (lane L4, W5L4-3): `getfield` pcs whose value is a
    /// `byte[]` or null ([`Self::add_byte_array_field_pcs`]); each load (or
    /// forwarded value) built at one goes into [`Graph::byte_array_nodes`].
    pub(in crate::ir) byte_array_field_pcs: HashSet<usize>,
    /// nio21 wave 2 (lane L3): `CRATONVM_JIT_IR_FIELD_FINAL_FORWARD`, read
    /// once per builder ([`ir_field_final_forward_enabled`]).
    pub(in crate::ir) field_final_forward_on: bool,
    /// Build a volatile `getfield` as an ordered `Op::Load` instead of
    /// refusing (round 9 wave 6). Set by [`Self::set_volatile_field_pcs`] from
    /// [`ir_volatile_fields_enabled`].
    pub(in crate::ir) volatile_loads_admitted: bool,
    /// Build a volatile `putfield` as an ordered `Op::Store` instead of
    /// refusing. Additionally needs [`IR_LOWER_FENCES_VOLATILE_STORES`].
    pub(in crate::ir) volatile_stores_admitted: bool,
    /// Resolved allocation layout for `new`, keyed by bytecode pc:
    /// `pc → (class_id, num_fields)`. Set by [`Self::set_new_info`]; only the
    /// allocations the caller admits (non-escaping-eligible: no primitive field
    /// initialisers, no finalizer) are present. A `new` whose pc is absent
    /// makes `build` bail (`None` → single-pass).
    pub(in crate::ir) new_info: HashMap<usize, (u32, usize)>,
    /// cov-06: resolved allocation layout for `anewarray`, keyed by bytecode
    /// pc: `pc → component_class_id`. Set by [`Self::set_anewarray_info`];
    /// only an already-loaded component class is present — a `Deferred` site
    /// (target class not loaded yet) is omitted, exactly like an unresolved
    /// `new`. An `anewarray` whose pc is absent makes `build` bail (`None` →
    /// single-pass).
    pub(in crate::ir) anewarray_info: HashMap<usize, u32>,
    /// Round 16 (lane matrix): resolved `multianewarray` sites, keyed by
    /// bytecode pc: `pc → packed (holder_class_id, cp_idx)` site word
    /// ([`Op::MultiNewArray`]). Set by [`Self::set_multianewarray_sites`];
    /// empty unless `CRATONVM_JIT_IR_MULTIANEWARRAY` is on and the compile
    /// door wired the helper. A `multianewarray` whose pc is absent makes
    /// `build` bail (`None` → single-pass).
    pub(in crate::ir) multianewarray_sites: HashMap<usize, i64>,
    /// nio21 wave 15 (lane L3): the `new` sites whose class was not loaded at
    /// compile time (`JitNewSite::Deferred`), keyed by bytecode pc:
    /// `pc -> (holder_class_id, cp_idx)` ([`Op::NewCp`]). Set by
    /// [`Self::set_new_deferred_sites`]; empty unless
    /// [`ir_deferred_new_enabled`] is on and the door wired
    /// `helpers.new_object_cp`. Consulted only for a pc absent from
    /// `new_info`, and only outside a splice.
    pub(in crate::ir) new_deferred_sites: HashMap<usize, (u32, u16)>,
    /// Bytecode pcs of `invokespecial` calls to a trivial no-arg void
    /// constructor (`<init>()V`) that may be **elided** (the receiver is a
    /// fresh, non-escaping object whose fields are zero-initialised and set by
    /// the visible `putfield`s). Set by [`Self::set_trivial_init_pcs`]. An
    /// `invokespecial` whose pc is NOT here makes `build` bail.
    pub(in crate::ir) trivial_init_pcs: HashSet<usize>,
    /// JIT round 19 wave 2, lane eager2 (EAGER19-1,
    /// `CRATONVM_JIT_UNINIT_NEW_TRAP`): the pcs of `new`s with no `new_info`
    /// row (`JitNewSite::Deferred`) whose class is LOADED but owes its
    /// initialization, and which the VM admits to an uncommon trap
    /// ([`TrapCause::UninitNew`]) instead of the bail to single-pass. Set by
    /// [`Self::set_uninit_new_trap_pcs`]; empty unless the door wired the
    /// resolver.
    pub(in crate::ir) uninit_new_trap_pcs: HashSet<usize>,
    /// Interpreter round i4 wave 94, lane L2 (`CRATONVM_JIT_IR_FIELD_SITE_TRAP`,
    /// default OFF): `pc -> descriptor tag` of this method's own field sites
    /// (`getstatic` / `putstatic` / `getfield` / `putfield`) with no field row
    /// whose owner is not loaded, which the VM admits to an uncommon trap
    /// ([`TrapCause::UnresolvedField`]) instead of the bail to single-pass.
    /// Set by [`Self::set_field_trap_sites`]; empty unless the door wired the
    /// resolver.
    pub(in crate::ir) field_trap_sites: HashMap<usize, u8>,
    /// JIT round 19 wave 3, lane eager3 (EAGER2-3,
    /// `CRATONVM_JIT_UNINIT_NEW_GUARD`): the pcs of `new`s whose class is
    /// LOADED but owes its initialization and which the VM admits to a
    /// class-initialized GUARD, valued by the class's init-word address and
    /// the byte it reads once initialized. Such a pc HAS a `new_info` row; the
    /// 0xbb arm plants [`Self::plant_class_init_guard`] in front of the
    /// `Op::New`, and when the plant is refused it takes the row-less path
    /// (the EAGER19-1 trap or the bail) instead. Set by
    /// [`Self::set_uninit_new_guard_sites`]; empty unless the door wired it.
    pub(in crate::ir) uninit_new_guard_sites: HashMap<usize, (usize, u8)>,
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
    pub(in crate::ir) object_init_pcs: HashSet<usize>,
    /// Interpreter round i1 wave 50, lane L1: this method's call sites the
    /// compile door found unresolved, `pc -> crate::JitFirstCallCell`
    /// ([`Self::set_first_call_sites`]). Such a site is always built as an
    /// `Op::Call` -- no elision, splice, fold or intrinsic -- because the
    /// lowerer's first-execution guard rides on the call
    /// ([`Graph::first_call_cells`]).
    pub(in crate::ir) first_call_sites: HashMap<usize, usize>,
    /// Gap B: resolved `invokestatic` call sites the builder lowers into an
    /// `Op::Call`. `pc → (info_ptr, num_args, ret_type)` where `info_ptr` is the
    /// address of a leaked `JitInvokeInfo` (the dispatch helper's 2nd argument),
    /// `num_args` the JVM arg-slot count, and `ret_type` the JVM return-type byte
    /// (`V` → no value; `L`/`[` → a reference result, typed `IrType::Ref`; any
    /// int-category byte → `IrType::Int`). Set by [`Self::set_invoke_info`]. The
    /// caller (`lib.rs`) restricts which methods get this (inc 22: oops may be
    /// live across the call — found by the conservative GC scan of the spilled
    /// frame, sound because GC is non-moving while a JIT frame is active).
    pub(in crate::ir) invoke_info: HashMap<usize, (usize, usize, u8)>,
    pub(in crate::ir) unloaded_static_call_pcs: HashSet<usize>,
    /// Round 11 wave 10 (lane `synccall`): the `invokestatic` pcs whose callee
    /// is a `static synchronized` method CALLed directly while THIS method
    /// holds the class monitor, `pc -> (holder class id, class cp index,
    /// mirror slot)`. Set by [`Self::set_sync_direct_calls`]; see the
    /// `0xb6 | 0xb8` arm for the shape and why no frame state names the lock.
    pub(in crate::ir) sync_direct_calls: HashMap<usize, (u32, u16, usize)>,
    /// Round 14 wave 2 (lane syncsplice, SR-1): caller pcs whose planned
    /// splice is a `synchronized` body, `pc -> class id` of a `static` callee
    /// (`0` for an instance one). Such a body is spliced only as a
    /// synchronized splice ([`Self::sync_splice_plan`]); when that cannot be
    /// built the site is NOT spliced. Set by [`Self::add_sync_splice_site`].
    pub(in crate::ir) sync_splice_sites: HashMap<usize, u32>,
    /// Round 14 wave 3 (lane sync, SS-2): the class mirror of a `static
    /// synchronized` splice whose site has no caller-held CALL row, in that
    /// row's shape `pc -> (holder class id, class cp index, mirror slot)`.
    /// Read only by [`Self::sync_splice_plan`]; set by
    /// [`Self::add_sync_splice_mirror`].
    pub(in crate::ir) sync_splice_mirrors: HashMap<usize, (u32, u16, usize)>,
    /// Round 14 wave 6 (lane sync6, SS8-1 a): combined pcs whose planned
    /// splice is a synchronized instance body on the enclosing window's own
    /// receiver ([`IrInlineTables::sync_held_sites`]). Spliced with no pair,
    /// and only under the proof in [`Self::begin_splice`].
    pub(in crate::ir) sync_splice_held_sites: HashSet<usize>,
    /// Round 14 wave 7 (lane sync7, SS8-5): combined pcs of a spliced `ldc`
    /// naming its `static synchronized` body's own class and followed by
    /// `Thread.holdsLock` -> that class id
    /// ([`IrInlineTables::ldc_self_class_holds_lock`]). Read only by
    /// [`Self::try_fold_self_class_holds_lock`].
    pub(in crate::ir) ldc_self_class_holds_lock: HashMap<usize, u32>,
    /// JIT round 20 wave 12 (lane qMisc, NIO20-L3-1): the compiling method's
    /// `invokevirtual Object.getClass()` pcs followed by `ldc C` and
    /// `if_acmpeq`/`if_acmpne` whose `C` the VM states `getClass()` reports
    /// exactly for its own instances -> `C`'s class id (`lib.rs`
    /// `ir_getclass_exact_compares`). Read only by
    /// [`Self::try_fold_getclass_exact_compare`].
    pub(in crate::ir) getclass_exact_compares: HashMap<usize, u32>,
    /// JIT r20 wave 19 (lane zLdc, THROW20-4 `date`, [`ir_splice_ref_ldc_enabled`]):
    /// combined pcs of a SPLICED body's `invokevirtual Object.getClass()`
    /// followed by `ldc C` and `if_acmpeq`/`if_acmpne`, `C` a class the VM
    /// states `getClass()` reports exactly for its own instances -> `C`'s id
    /// ([`IrInlineTables::splice_getclass_compares`]). Read only by
    /// [`Self::try_fold_spliced_getclass_of_fresh`].
    pub(in crate::ir) splice_getclass_compares: HashMap<usize, u32>,
    /// Round 14 wave 7 (lane sync7, RV6-1): `(holder class id, cp index)` of
    /// the compiling method's own `CONSTANT_Class` entries the VM resolved to
    /// [`Graph::method_monitor_class`] -- an `Op::ConstClass` with one of these
    /// is that class's mirror whenever it produces a value. Set by
    /// [`Self::set_method_monitor_class_cps`]; read only by
    /// [`Self::try_fold_holds_lock`], and only while the graph claims the class.
    pub(in crate::ir) method_monitor_class_cps: HashSet<(u32, u16)>,
    /// Round 14 wave 5 (lane sync5, SS8-3): `Some(code_len)` when the graph
    /// claims a method monitor (`Graph::method_monitor_param`) and the
    /// method's own code writes local 0 nowhere, so every loop-header φ of
    /// local 0 at a header below `code_len` (the method's own loops, not a
    /// spliced body's) can only merge the receiver with itself -- even while
    /// its back edges are still missing. Set by [`Self::build`]; read only by
    /// [`Self::holds_lock_same_object`].
    pub(in crate::ir) method_monitor_slot0_loops_below: Option<usize>,
    /// Round 15 wave 1 (lane sync, SS8-2): build a SELF-LOCKING body
    /// ([`MethodSelfLock`]). Set by [`Self::set_method_self_lock`].
    pub(in crate::ir) method_self_lock_requested: bool,
    /// Round 15 wave 3 (lane sync2, S15-3): the `static synchronized` twin's
    /// monitor -- `(declaring class id, the VM's GC-maintained mirror slot)`
    /// -- or `None` (an instance self-lock, which locks `this`). Set by
    /// [`Self::set_method_self_lock_mirror`].
    pub(in crate::ir) method_self_lock_mirror: Option<(u32, usize)>,
    pub(in crate::ir) root_caller_mirror_slot: usize,
    pub(in crate::ir) root_security_walk_ignored: bool,
    pub(in crate::ir) splice_caller_frames: HashMap<usize, (usize, bool)>,
    /// Call sites the invoke planner recognised as a scalar-intrinsic family
    /// and deliberately left OUT of `invoke_info`, keyed by caller pc.
    ///
    /// A pc in here is not a missing plan -- it is a plan to emit arithmetic.
    /// The distinction matters because every invoke arm's `None` from
    /// `invoke_info` means "refuse the method", and without this map a site the
    /// planner handled on purpose would be indistinguishable from one it could
    /// not handle at all.
    pub(in crate::ir) scalar_intrinsics: HashMap<usize, ScalarOp>,
    /// j27 wave 2 (lane INTRINS): call sites expanded into plain IR
    /// arithmetic, same contract as [`Self::scalar_intrinsics`]
    /// (`pure_expand`).
    pub(in crate::ir) pure_expansions: HashMap<usize, PureExpansion>,
    pub(in crate::ir) unbox_intrinsics: HashMap<usize, (UnboxOp, u32)>,
    /// nio21 wave 1 L3: the compile knows the JIT-readable `Unsafe` arena
    /// table (`ir_lower::scoped_arena_jit_table`), so a `ScopedMemoryAccess`
    /// site with a LITERAL-null base (a direct buffer, a native segment) may
    /// fold: its lowering resolves the arena handle inline. `false` (the
    /// default, and every door that does not ask) keeps the nio20 refusal.
    /// See [`Self::set_scoped_offheap_fold`].
    pub(in crate::ir) scoped_offheap_fold: bool,
    /// The `static final VarHandle` fields (keyed `(declaring class id,
    /// static field index)`, the pair an [`Op::LoadStatic`] carries) the VM
    /// registered as constant handles over an instance field. A
    /// [`UnboxOp::FieldInt`] / [`UnboxOp::FieldLong`] candidate row in
    /// [`Self::unbox_intrinsics`] resolves against this table at the invoke:
    /// the row only says "this `VarHandle.get*` site may fold", the handle
    /// (and so the slot and the receiver class) is whatever the site's first
    /// operand turns out to be.
    pub(in crate::ir) vh_field_plans: HashMap<(u32, u32), crate::StaticVarHandleField>,
    /// JIT r23 wave 4 (lane L8, `CRATONVM_JIT_IR_FFM_ELEMENT_INLINE`): the
    /// `MemorySegment` element sites the planner asks this builder to expand
    /// inline, keyed by caller pc ([`Self::try_emit_ffm_element_inline`]).
    pub(in crate::ir) ffm_inline_sites: HashMap<usize, crate::ffm_element_inline::FfmElementInlineSite>,
    /// JIT round 24: [`Self::set_ffm_bimorphic_seconds`].
    pub(in crate::ir) ffm_bimorphic_seconds: HashMap<usize, crate::ffm_element_inline::FfmSegmentShape>,
    /// JIT r23 wave 11 (lane L4, `CRATONVM_JIT_IR_FFM_UNREACHED_SITE_TRAP`):
    /// element sites no execution has reached, keyed by caller pc, each built
    /// as an always-failing `UnstableIf` guard ([`Self::try_emit_ffm_unreached_trap`]).
    pub(in crate::ir) ffm_unreached_sites: HashMap<usize, crate::ffm_element_inline::FfmElementAccessor>,
    /// JIT r23 wave 12 (lane L7, `CRATONVM_JIT_IR_UNREACHED_CALL_TRAP`): the
    /// generic twin of [`Self::ffm_unreached_sites`] -- any call-site
    /// intrinsic the planner would have refused the method for, at a site no
    /// execution has reached, keyed by caller pc: `(operand-stack entries the
    /// call consumes, return type tag)`, one entry per argument as in
    /// [`Self::indy_trap_sites`]. Built by the same
    /// [`Self::try_emit_ffm_unreached_trap`].
    pub(in crate::ir) unreached_call_sites: HashMap<usize, (usize, u8)>,
    /// The layout facts of this method's own `getstatic`s of a filed
    /// `static final` `ValueLayout`, keyed like [`Self::vh_field_plans`].
    pub(in crate::ir) ffm_layout_facts: HashMap<(u32, u32), crate::ffm_element_inline::FfmLayoutFacts>,
    /// `invokedynamic` sites this tier replaces with an uncommon trap, keyed by
    /// caller pc: `(operand-stack entries the call consumes, return type tag)`.
    ///
    /// The stack effect is the ONLY thing needed, because the site emits no
    /// call -- but it is needed exactly, or every bytecode after it reads the
    /// wrong entries. The counts come from `count_param_slots`, which counts
    /// one entry per ARGUMENT (a `long` is one), matching this builder's
    /// one-entry-per-value abstract stack; `compute_param_jvm_slots` counts
    /// two for a `long` and is the wrong function here.
    pub(in crate::ir) indy_trap_sites: HashMap<usize, (usize, u8)>,
    /// Round 16 (lane indy, SY4-1): the `invokedynamic` sites this tier
    /// CALLS through the VM's bridge instead of trapping at, keyed by caller
    /// pc: the bridge-site pointer ([`Op::IndyBridge`]). A subset of
    /// `indy_trap_sites`' keys (that map still supplies the stack effect);
    /// empty unless `CRATONVM_JIT_IR_INDY_BRIDGE` is on and the compile door
    /// found a bridged bootstrap and a wired helper. See
    /// [`Self::set_indy_bridge_sites`].
    pub(in crate::ir) indy_bridge_sites: HashMap<usize, usize>,
    /// JIT round 22 (lane throws, churn-loop J1;
    /// [`ir_indy_bridge_on_trap_refusal_enabled`]): `indy_bridge_sites` was
    /// filled ONLY so that a site whose uncommon trap would be refused (or
    /// that the default door gave no shape, so the build would bail) is
    /// bridged instead of bailing the whole method. A site whose trap the
    /// default arm would plant keeps the trap exactly as before. `false`
    /// (the default) means `indy_bridge_sites`, when non-empty, is the
    /// unconditional `CRATONVM_JIT_IR_INDY_BRIDGE` plan.
    pub(in crate::ir) indy_bridge_refusal_only: bool,
    /// With [`Self::indy_bridge_refusal_only`]: the default door (the
    /// invoke-plan block, or the lambda-word / capture-new plans) had given
    /// this build its indy shapes, i.e. the default arm would have reached
    /// `plant_uncommon_trap` rather than bailing at a shapeless site.
    pub(in crate::ir) indy_default_trap_shapes: bool,
    /// t1 wave 8, lane LAMBDATRAP: the pcs [`Self::add_indy_bridge_sites`]
    /// recorded -- the method's own published-word lambda sites
    /// (`CRATONVM_JIT_IR_INDY_PUBLISHED_WORD_BRIDGE`). Their bridge is a probe
    /// of a known singleton, not a bare bootstrap call, so the refusal-only
    /// plan does not demote them back to a trap
    /// ([`ir_indy_published_word_bridge_over_refusal_only_enabled`]).
    pub(in crate::ir) indy_published_word_pcs: HashSet<usize>,
    /// nio21 wave 11, lane L3 (W10L3-1, [`ir_splice_indy_enabled`]): the
    /// compile door wired `DirectHelperTable::indy_bridge`, so a SPLICED
    /// constant-word lambda site (a row of [`IrInlineTables::indy_bridge_sites`])
    /// may lower to its `Op::IndyBridge`. See [`Self::set_spliced_indy_wired`].
    pub(in crate::ir) spliced_indy_wired: bool,
    /// JIT round 16 wave 10 (lane indyir, IN16-8): the `invokedynamic` sites
    /// the compile door planned as a PLAIN CALL of their constant call site's
    /// target, keyed by caller pc: the invoke opcode whose arm builds the site
    /// (`0xb8` a static target, `0xb6` an unbound virtual one). The plan's
    /// rows for the pc (`invoke_info`, inline sites, inline caches) are the
    /// ordinary ones for that call. See [`Self::indy_direct_opcode`].
    pub(in crate::ir) indy_direct_sites: HashMap<usize, u8>,
    /// JIT round 20 wave 11 (lane pIndy, SPLICE19-1): the `invokedynamic`
    /// sites of a NON-constant call site the door planned as a GUARDED plain
    /// call, keyed by caller pc. Never in `indy_direct_sites`. See
    /// [`Self::try_indy_mutable_guarded_call`].
    pub(in crate::ir) indy_mutable_guards: HashMap<usize, IndyMutableGuard>,
    /// JIT round 20 wave 11 (lane pIndy, W10L3-3 / W11L3-3): the method's own
    /// non-capturing lambda sites with a published singleton slot, `pc ->
    /// (bridge site, slot address)`. See [`Self::try_indy_lambda_word`].
    pub(in crate::ir) indy_lambda_words: HashMap<usize, (usize, usize)>,
    /// JIT round 20 wave 12 (lane qLamNew, W10L3-2 / W11L3-2): the method's own
    /// linked capturing lambda sites, `pc -> what to allocate`. See
    /// [`Self::try_indy_capture_new`].
    pub(in crate::ir) indy_capture_news: HashMap<usize, IndyCaptureNew>,
    /// JIT round 21 wave 17 (lane records, IN16-6): the method's record
    /// `ObjectMethods` sites the door planned as a field-wise fold, `pc ->
    /// fold`. Each pc's `invoke_info` row is the `Objects.hashCode` /
    /// `Objects.equals` call the fold makes per reference component. See
    /// [`Self::try_indy_record_fold`].
    pub(in crate::ir) indy_record_folds: HashMap<usize, IndyRecordFold>,
    /// JIT round 21 wave 20 (lane sbfuse, STRINTR21-4): the method's
    /// `StringBuilder` chains the door planned as one concat, `new pc ->
    /// fusion`. See [`Self::sb_chain_fuse_step`].
    pub(in crate::ir) sb_chain_fusions: HashMap<usize, crate::sb_chain_fuse::SbChainFusion>,
    /// The fused chain the walk is inside, if any.
    pub(in crate::ir) sb_chain_active: Option<crate::sb_chain_fuse::SbChainActive>,
    /// JIT round 21 (lane lambda2, TLOCAL21-1 / LAMBDATRAP21-3;
    /// `CRATONVM_JIT_IR_LAMBDA_IMPL_CALL`): the `invokeinterface` SAM calls
    /// the compile door planned as an `invokestatic` of the lambda's impl,
    /// `invoke pc -> (invokedynamic pc, capture count, impl parameter
    /// count)`. See [`Self::lambda_impl_opcode`].
    pub(in crate::ir) lambda_impl_sites: HashMap<usize, (usize, usize, usize)>,
    /// JIT round 21 (lane lambda2): what [`Self::try_indy_capture_new`] built
    /// at each of the method's own `invokedynamic` pcs, `pc -> (the proxy's
    /// `New`, the capture values in field order)`, for
    /// [`Self::lambda_impl_opcode`].
    pub(in crate::ir) indy_capture_new_values: HashMap<usize, (NodeId, Vec<NodeId>)>,
    /// JIT round 21 wave 19 (lane lambda3fix, LAMBDA321-4,
    /// `CRATONVM_JIT_IR_LAMBDA_IMPL_LOOP_SLOT`): `(SAM call pc, receiver,
    /// proxy New)` of every planned capturing site whose receiver was admitted
    /// as loop-header phis resolving to the proxy
    /// ([`Self::lambda_receiver_resolves_to`]) while the loop's back edges were
    /// not yet patched. Re-asked at the end of the build, where a phi that a
    /// back edge feeds anything else refuses the build.
    pub(in crate::ir) lambda_impl_phi_checks: Vec<(usize, NodeId, NodeId)>,
    /// JIT round 21 (lane lambda3; `CRATONVM_JIT_IR_LAMBDA_IMPL_SPLICED_CAPTURES`):
    /// a CAPTURING site of [`Self::lambda_impl_sites`] is built only as a
    /// splice of the impl; any other route refuses the build (the door then
    /// rebuilds with the interface call). See [`Self::lambda_impl_opcode`].
    pub(in crate::ir) lambda_impl_captures_need_splice: bool,
    /// JIT round 16 wave 17 (lane mh7; MH16-8 at compile time): the
    /// `invokevirtual MethodHandle.invokeExact` / `invoke` sites the compile
    /// door planned as an `invokestatic` of a constant handle's member, keyed
    /// by caller pc: the pc of the `getstatic` whose `static final` field holds
    /// the handle. The plan's row for the pc is the member's. See
    /// [`Self::mh_constant_opcode`].
    pub(in crate::ir) mh_constant_sites: HashMap<usize, usize>,
    /// JIT round 19 wave 1 (lane mhiface; MHRESID18-4): the invoke pcs among
    /// [`Self::mh_constant_sites`] whose handle is an unbound INTERFACE
    /// method's (`invoke_kind 2`); their receiver is guarded with an
    /// `instanceof` of the interface. See [`Self::mh_constant_opcode`].
    pub(in crate::ir) mh_constant_interface_sites: HashSet<usize>,
    /// JIT round 20 (MHIFACE19-1): for each pc of
    /// [`Self::mh_constant_interface_sites`], the address of the artifact-owned
    /// two-way cell its receiver guard's `instanceof` remembers the first two
    /// implementing classes in (`Op::InstanceOf::seen_cell`). A pc without one
    /// plants a plain `instanceof`.
    pub(in crate::ir) mh_constant_interface_seen_cells: HashMap<usize, usize>,
    /// JIT round 21 wave 19 (lane mhbind2; MHBIND21-1): the invoke pcs among
    /// [`Self::mh_constant_sites`] whose handle is a REAL `bindTo` result of a
    /// virtual member, each with the slot of the handle's `argL1` field: the
    /// member's receiver, read off the handle in place of it. See
    /// [`Self::mh_constant_opcode`].
    pub(in crate::ir) mh_constant_bound_sites: HashMap<usize, u32>,
    /// j27 wave 6 (lane T1LAM2): the invoke pcs among
    /// [`Self::mh_constant_bound_sites`] whose bound handle is a STATIC
    /// member's (`invoke_kind 6`, the bound value its leading argument), built
    /// by the `invokestatic` arm with no receiver guard. Must agree with the
    /// row's own kind ([`Self::mh_constant_bound_opcode`]).
    pub(in crate::ir) mh_constant_bound_static_sites: HashSet<usize>,
    /// JIT round 21 wave 20 (lane mhfilter; MHBIND2-1): the invoke pcs among
    /// [`Self::mh_constant_sites`] whose handle is a REAL filter species. See
    /// [`Self::mh_constant_filter_opcode`].
    pub(in crate::ir) mh_constant_filter_sites: HashMap<usize, IrMhFilterSite>,
    pub(in crate::ir) mh_constant_convert_sites: HashMap<usize, IrMhConvertSite>,
    /// IR-tier inlining: callee bodies to splice, keyed by the CALLER pc of the
    /// `invoke` each replaces. Empty on every compile that inlines nothing,
    /// which is every compile with the gate off. See [`IrInlineSite`].
    pub(in crate::ir) inline_sites: HashMap<usize, IrInlineSite>,
    /// Round 12 wave 4 (lane iropt3, proposal W3-1): the `invokevirtual` sites
    /// among [`Self::inline_sites`] whose body was resolved for a receiver of
    /// EXACTLY this class id, not proven monomorphic by the language. Such a
    /// site is spliced only when [`Self::exact_receiver_proof`] shows the
    /// receiver is that class; otherwise it stays a call. See
    /// [`Self::add_exact_receiver_site`].
    pub(in crate::ir) exact_receiver_sites: HashMap<usize, u32>,
    /// Caller local slots written by exactly one instruction of the whole
    /// method, that instruction an `astore`, and not a parameter slot. Filled
    /// at the start of [`Self::build`] only when [`Self::exact_receiver_sites`]
    /// is non-empty. See [`single_astore_local_slots`].
    pub(in crate::ir) exact_receiver_single_store_slots: HashSet<usize>,
    /// Slot -> class id of the `Op::New` its one store wrote, recorded by
    /// [`Self::store_local`] outside every splice.
    pub(in crate::ir) exact_receiver_locals: HashMap<usize, u32>,
    /// al16 (2026-10-05): slot -> class id of the `Op::New` the LAST store
    /// into that caller slot wrote, on the path the walk is building
    /// (`CRATONVM_JIT_IR_EXACT_RECEIVER_FLOW`). Unlike
    /// [`Self::exact_receiver_locals`] the slot may be written elsewhere too
    /// (javac reuses a slot across scopes: the for-each iterator shares one
    /// with the loop counter before it). Any other store into the slot drops
    /// it, a forward join keeps only the slots holding one node on every
    /// predecessor, and a loop header keeps one
    /// only when the walk falls into it as its sole forward predecessor and no
    /// bytecode of the loop writes the slot ([`Self::loop_written_slots`]).
    pub(in crate::ir) exact_receiver_flow: HashMap<usize, u32>,
    /// Loop header pc -> every caller slot an instruction between it and its
    /// last back edge writes. Filled at the start of [`Self::build`] when
    /// [`Self::exact_receiver_sites`] is non-empty and the flow proof is on.
    pub(in crate::ir) loop_written_slots: HashMap<usize, HashSet<usize>>,
    /// Slots an admitted exact-receiver splice relied on. A later store that
    /// contradicts the record refuses the build rather than leave a splice
    /// whose proof no longer holds.
    pub(in crate::ir) exact_receiver_relied: HashSet<usize>,
    /// Round 13 wave 10 (lane iropt8, HS-2): receiver NODES an admitted
    /// exact-receiver splice proved to be exactly this class id. An object
    /// never changes class, so the fact holds wherever the node flows,
    /// including into the splice frames the site opens. Read only under
    /// `crate::ir_splice_nested_exact_receiver_enabled()`.
    pub(in crate::ir) exact_receiver_nodes: HashMap<NodeId, u32>,
    /// Round 13 wave 10 (lane iropt8, HS-2): [`IrInlineSite::base`] of every
    /// spliced instance body that never writes its local 0, so the value in
    /// that slot is the body's receiver at every pc of it. Filled at the start
    /// of [`Self::build`] only when an exact-receiver site exists and the
    /// switch is on. See [`Self::exact_receiver_node_proof`].
    pub(in crate::ir) splice_bases_keeping_this: HashSet<usize>,
    /// Round 12 wave 7 (lane iropt6, proposal W6-1): the exact-receiver sites
    /// (a subset of [`Self::exact_receiver_sites`]) whose receiver, when
    /// [`Self::exact_receiver_proof`] cannot show it is exactly the class, may
    /// still be spliced behind a run-time class test with the ordinary call as
    /// the miss arm. See [`Self::add_guarded_receiver_site`].
    pub(in crate::ir) guarded_receiver_sites: HashSet<usize>,
    /// JIT round 23 wave 6 (`CRATONVM_JIT_IR_CHA_MONOMORPHIC_SPLICE`): guarded
    /// receiver sites whose test is a class-id WATERMARK instead of the exact
    /// class (caller pc -> watermark). See
    /// [`Self::add_cha_watermark_site`].
    pub(in crate::ir) cha_watermark_sites: HashMap<usize, u32>,
    /// j27 wave 5 (lane PROXY, `CRATONVM_JIT_IR_CHA_INTERFACE_CLASS_SET_SPLICE`):
    /// guarded receiver sites whose test is an exact class SET (caller pc ->
    /// class ids, the profile's first). See [`Self::add_cha_class_set_site`].
    pub(in crate::ir) cha_class_set_sites: HashMap<usize, Vec<u32>>,
    /// nio21 wave 6, lane L4: the guarded sites (a subset of
    /// [`Self::guarded_receiver_sites`]) the planner found profile-hot, whose
    /// first body may be as large as [`GUARDED_SPLICE_HOT_MAX_BODY_BYTES`]
    /// instead of [`GUARDED_SPLICE_MAX_BODY_BYTES`]. See
    /// [`Self::add_guarded_hot_cap_site`].
    pub(in crate::ir) guarded_hot_cap_sites: HashSet<usize>,
    /// JIT round 19 (lane splice19, CH6W5-1): NESTED exact-receiver sites
    /// (combined pcs inside a spliced body) that may take a guarded splice on
    /// their exact class when the receiver is not proven
    /// ([`Self::add_nested_guarded_receiver_site`]).
    pub(in crate::ir) nested_guarded_sites: HashSet<usize>,
    /// `(caller pc, receiver)` of the guarded splice being entered, set by
    /// [`Self::begin_guarded_receiver_splice`] for the one [`Self::begin_splice`]
    /// call it makes and taken there. It tells `begin_splice` that the receiver
    /// is proven exact (and so non-null) on the edge the body is built on.
    pub(in crate::ir) guarded_splice_pending: Option<(usize, NodeId)>,
    /// Round 14 wave 2 (lane guard, GS-1): caller pc -> the SECOND class of a
    /// bimorphic guarded site (the first is the site's
    /// [`Self::exact_receiver_sites`] row). Its body is in
    /// [`Self::inline_sites`] under [`bimorphic_second_site_key`]. See
    /// [`Self::add_bimorphic_receiver_site`].
    pub(in crate::ir) bimorphic_sites: HashMap<usize, u32>,
    /// Round 16 (lane mega): the THIRD and later classes of a polymorphic
    /// guarded site, by caller pc, best first; their bodies are in
    /// [`Self::inline_sites`] under [`polymorphic_site_key`]. See
    /// [`Self::add_polymorphic_receiver_sites`].
    pub(in crate::ir) polymorphic_sites: HashMap<usize, Vec<u32>>,
    /// Round rx27 wave 4: [`ir_max_splice_depth`], read once at construction.
    pub(in crate::ir) max_splice_depth: usize,
    /// Round 14 wave 2 (lane guard, CH5-5): guarded sites whose miss edge is a
    /// deopt at the `invoke` rather than the call. See
    /// [`Self::add_guarded_trap_site`].
    pub(in crate::ir) guarded_trap_sites: HashSet<usize>,
    /// Round 14 wave 2 (lane guard, CH5-5): receiver nodes a miss-edge TRAP
    /// proved to be exactly this class. Unlike [`Self::exact_receiver_nodes`]
    /// the fact is scoped by dominance: it holds from the trap to the next
    /// join (or jump) of the compiling method's own code, and is cleared
    /// there. See [`Self::begin_trap_guarded_receiver_splice`].
    pub(in crate::ir) trap_exact_facts: HashMap<NodeId, u32>,
    /// Round 15 wave 1 (lane chain, G14-1): `(base node, field index)` ->
    /// `(the value the cell holds, splices_done when it was recorded, the
    /// memory node that put it there)` for the plain `getfield`s of the
    /// compiling method's own code. For a read the value and the memory node
    /// are the same `Op::Load`; round 16 (lane fieldguard, CHN15-5 (a)) also
    /// records a plain primitive `putfield`, whose value is the NARROWED stored
    /// value and whose memory node is the `Op::Store`. See
    /// [`Self::field_load_memo_forward`] for the invalidation rules.
    ///
    /// Round nio20 wave 4 (lane L1, W3L4-2): the fourth member is the splice
    /// DEPTH (`self.splice.len()`) the entry was recorded at -- 0 for the
    /// compiling method's own code, the only depth recorded at unless
    /// [`Self::field_memo_in_splices`]. See [`Self::field_load_memo_clear_at_splice_join`].
    pub(in crate::ir) field_load_memo: HashMap<(NodeId, usize), (NodeId, usize, NodeId, usize)>,
    /// Round nio20 wave 4 (lane L1, W3L4-2):
    /// `CRATONVM_JIT_IR_FIELD_MEMO_IN_SPLICES` (default ON), read once per
    /// builder. Lets the memo record and answer inside spliced bodies, with
    /// the depth-scoped clears [`Self::field_load_memo_clear_at_splice_join`]
    /// describes.
    pub(in crate::ir) field_memo_in_splices: bool,
    /// First node id [`Self::field_load_memo_scan`] has not classified yet.
    pub(in crate::ir) field_load_memo_scan_from: usize,
    /// JIT round 23 wave 2e, lane L2 (W4L1-1 of `jit-r20-nio-proposals-w4-L1.md`,
    /// `CRATONVM_JIT_IR_FIELD_MEMO_DEFER_MISS_CALL`): the miss-edge `Op::Call`s
    /// of the guarded splice diamonds whose hit body is being walked. The memo
    /// scan skips them (no hit-body node is on a path through the call) and
    /// [`Self::finish_multi_return_splice`] applies them at the join (a call
    /// writes everything: the memo clears).
    pub(in crate::ir) field_memo_deferred_calls: Vec<NodeId>,
    /// JIT round 23 wave 2e, lane L2 (W4L1-2, `CRATONVM_JIT_IR_FIELD_MEMO_PRECISE_JOINS`):
    /// per forward-join target pc of the compiling method's own code, the memo
    /// each predecessor edge carried when [`Self::add_merge_predecessor`]
    /// registered it. At the join the memo keeps exactly the entries every
    /// incoming edge carries unchanged ([`Self::field_load_memo_join`]).
    pub(in crate::ir) field_memo_join_snapshots: HashMap<usize, Vec<FieldMemoMap>>,
    /// Both switches above, read once per builder.
    pub(in crate::ir) field_memo_defer_miss_call: bool,
    pub(in crate::ir) field_memo_precise_joins: bool,
    /// Off, census-only, or forwarding. See [`FieldLoadMemoMode`].
    pub(in crate::ir) field_load_memo_mode: FieldLoadMemoMode,
    /// Round 16 (lane fieldguard, CHN15-5 (a)):
    /// `CRATONVM_JIT_IR_FIELD_STORE_FORWARD`, read once per builder. Lets a
    /// store-recorded memo entry answer a later `getfield`.
    pub(in crate::ir) field_store_forward_on: bool,
    /// The G14-1 census line's numbers (`CRATONVM_DBG_IR_COMPILES`).
    pub(in crate::ir) field_load_memo_census: FieldLoadMemoCensus,
    /// Round 16 wave 14 (lane osrir, MON3-2): `(class id, field index)` -> the
    /// plain `Op::LoadStatic` that last read that static in the compiling
    /// method's own code, for [`Self::static_load_memo_forward`]. Scoped like
    /// [`Self::field_load_memo`]: cleared at every join and by anything that
    /// may write a static or order memory. The flag marks an entry a plain
    /// `putstatic` recorded (the stored value: t1 wave 7 lane NBC, the port
    /// of JIT round 21 lane staticfwd's OI-4) rather than an `Op::LoadStatic`.
    /// The third member is the splice DEPTH the entry was recorded at (0
    /// unless [`Self::static_memo_in_splices`], STATICFWD21-2).
    pub(in crate::ir) static_load_memo: HashMap<(u32, u32), (NodeId, bool, usize)>,
    /// t1 wave 7 (lane NBC, STATICFWD21-2):
    /// `CRATONVM_JIT_IR_STATIC_MEMO_IN_SPLICES` (default OFF), read once per
    /// builder: the static memo records and answers plain `getstatic`s inside
    /// spliced bodies too, with the field memo's depth-scoped clears
    /// ([`Self::static_load_memo_clear_from_depth`]).
    /// First node id [`Self::static_load_memo_scan`] has not classified yet.
    pub(in crate::ir) static_load_memo_scan_from: usize,
    /// Off, census-only, or forwarding ([`StaticLoadMemoMode`]).
    pub(in crate::ir) static_load_memo_mode: StaticLoadMemoMode,
    /// Classes whose statics the compile's resolver answered a direct base for
    /// (`DirectHelperTable::resolve_static_base`): initialised at compile time,
    /// so a `getstatic` of theirs runs no `<clinit>`. Set by
    /// [`Self::set_static_initialized_classes`]; empty = no class is known.
    pub(in crate::ir) static_initialized_classes: HashSet<u32>,
    /// t1misc (page `t1-staticfwd-proposal-spliced-static-classes-initialized`):
    /// the classes the spliced bodies' static rows name that the resolver
    /// answered a direct base for. Read ONLY by [`Self::static_load_memo_scan`]
    /// (a spliced `getstatic` / `putstatic` of such a class runs no `<clinit>`
    /// and so keeps the memo); deliberately NOT merged into
    /// [`Self::static_initialized_classes`], which also feeds the FG16-3 replay
    /// fence ([`Self::splice_static_access_cannot_trap`]). Set by
    /// [`Self::add_spliced_static_initialized_classes`].
    /// The MON3-2 census line's numbers.
    pub(in crate::ir) static_load_memo_census: StaticLoadMemoCensus,
    /// Round 15 wave 3 (lane chain2, CHN15-2): `pc -> declaring class id` of
    /// the resolved field each `getfield` / `putfield` names (combined-buffer
    /// pcs for spliced bodies). Set by [`Self::set_field_decl_classes`]; a pc
    /// without a row builds a node with no identity. Copied onto the graph
    /// per node ([`Graph::field_decl_class`]).
    pub(in crate::ir) field_decl_class_by_pc: HashMap<usize, u32>,
    /// Parameter slot count handed to [`Self::new`]: those slots are written
    /// on entry, so none of them is a single-store local.
    pub(in crate::ir) param_slot_count: usize,
    /// The splices currently open, innermost last. Non-empty exactly while the
    /// walk is inside a relocated callee body, which is what suppresses merge
    /// activation, the reachability skip and safepoint recording.
    pub(in crate::ir) splice: Vec<SpliceFrame>,
    /// `IrInlineSite::base` of every admitted body with more than one reachable
    /// `return`. Filled by the pre-scan in [`Self::build`] and read once, by
    /// [`Self::begin_splice`]. Empty unless `CRATONVM_JIT_IR_SPLICE_MULTI_RETURN=1`
    /// — and without it the scanner has admitted no such body either.
    pub(in crate::ir) splice_multi_return: HashSet<usize>,
    /// `IrInlineSite::base` → the block-walk ranges, rebased to combined-buffer
    /// pcs, of every admitted body with a rotated loop. Filled by the pre-scan
    /// in [`Self::build`] and read by [`Self::begin_splice`].
    pub(in crate::ir) splice_block_walks: HashMap<usize, Vec<(usize, usize)>>,
    /// Diagnostic-only: how many splices this build performed, for the
    /// `[ir] spliced` line. Never read by lowering.
    pub(in crate::ir) splices_done: usize,
    /// References the OUTERMOST open splice allocated itself, plus everything
    /// reachable from one by a field or element read.
    ///
    /// A store inside a spliced body has to target one of these — see
    /// [`Self::splice_store_is_local`] for why, and for why a `Load` from a
    /// local base is itself local.
    pub(in crate::ir) splice_local: HashSet<NodeId>,
    /// Splice-local objects that have had a NON-local reference stored into
    /// them. Reading a field of one gives back something the caller can still
    /// see, so locality must not propagate through it — see
    /// [`Self::splice_store_is_local`].
    pub(in crate::ir) splice_tainted: HashSet<NodeId>,
    /// Bases of the stores the OUTERMOST open splice has committed into
    /// objects it did NOT allocate. Non-empty means everything the walk builds
    /// from here to the end of that splice must be unable to trap: see
    /// [`Self::admits_after_committed_splice_store`]. Cleared when the walk is
    /// back in the caller.
    pub(in crate::ir) splice_committed_bases: HashSet<NodeId>,
    /// Receivers of a spliced CONSTRUCTOR that this method allocated itself
    /// outside every splice: `new C; dup; ...; invokespecial C.<init>`, the
    /// shape javac emits for every `new C(args)`. See
    /// [`Self::splice_store_is_ctor_receiver`]. An entry lives exactly as long
    /// as the constructor splice that added it ([`SpliceFrame::ctor_receiver`]).
    pub(in crate::ir) splice_ctor_receivers: HashSet<NodeId>,
    /// Constructor receivers whose fields a spliced body has READ, directly or
    /// possibly through an alias ([`Self::note_splice_field_read_alias`]). A
    /// store to one after that is refused -- see
    /// [`Self::splice_store_is_ctor_receiver`].
    pub(in crate::ir) splice_ctor_receiver_read: HashSet<NodeId>,
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
    pub(in crate::ir) splice_guard_seen: bool,
    /// Combined-buffer pc of the guard that first raised
    /// [`Self::splice_guard_seen`], so the end-of-build refusal can name the
    /// splice it belongs to (round 12 wave 6, lane iropt5; see
    /// [`Self::set_splice_refusal_sink`]). Diagnostic and retry input only.
    pub(in crate::ir) splice_guard_pc: Option<usize>,
    /// Round 12 wave 6 (lane iropt5, proposal W5-2): where a refused build
    /// reports the splice it refused in. See [`Self::set_splice_refusal_sink`].
    pub(in crate::ir) splice_refusal_sink: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    /// Set when the OUTERMOST open splice has built an `Op::Call` for a call
    /// the resolver did not splice (round 11 wave 16, lane `replay`): the call
    /// has run, and whatever it wrote is committed. Every trap inside a splice
    /// resumes at the CALLER's `invoke` with REEXECUTE, so a trap after the
    /// call would run it a second time. While this is set the walk admits only
    /// what [`Self::admits_after_committed_splice_store`] admits, exactly as
    /// after a committed store. Cleared when the walk is back in the caller.
    /// The kind-4 leaf self-call of an admitted self copy does not set it; see
    /// [`Self::is_self_copy_leaf_call`].
    pub(in crate::ir) splice_call_committed: bool,
    /// Round 12 wave 5 (lane iropt4): which relaxations of the call fence this
    /// build applies. Read once, at the start of [`Self::build`], when the
    /// build splices anything; see [`SpliceFenceRelax`].
    pub(in crate::ir) splice_fence_relax: SpliceFenceRelax,
    /// Round 16 (lane sync, SY16-2): relaxation 1 of [`SpliceFenceRelax`]
    /// (forward branches) applies to a fence a committed STORE raised too
    /// ([`ir_splice_store_fence_forward_branch_enabled`], and only with
    /// `splice_fence_relax.forward_branch`). Read with `splice_fence_relax`.
    pub(in crate::ir) splice_store_fence_forward_branch: bool,
    /// Round 13 wave 3 (lane framestate, W5-3 stage 1): record a snapshot with
    /// a [`SpliceScopeChain`] at every instruction of a spliced body
    /// ([`ir_splice_frame_states_enabled`]). Read once, at the start of
    /// [`Self::build`], when the build splices anything; `false` otherwise,
    /// which records exactly what every earlier build recorded.
    pub(in crate::ir) splice_frame_states: bool,
    /// JIT round 19 (lane splice19, CH16-4): with [`Self::splice_frame_states`],
    /// record a chain snapshot only at a spliced instruction a guard can be
    /// lowered at ([`Self::splice_pc_may_read_chain`];
    /// [`ir_chain_snapshots_lazy_enabled`]). Read once with it.
    pub(in crate::ir) chain_snapshots_lazy: bool,
    /// Interpreter round i1 wave 47, lane L3: record a post-call chain snapshot
    /// at the successor of every real call built inside a spliced body
    /// ([`ir_post_call_splice_exits_enabled`]). Read once, at the start of
    /// [`Self::build`], when the build splices anything; `false` otherwise.
    pub(in crate::ir) splice_call_states: bool,
    /// The spliced call whose successor the walk reaches next, and the pc of
    /// its `invoke` ([`Self::note_splice_call`]); taken at the top of the next
    /// instruction, which records the snapshot when it IS that successor.
    pub(in crate::ir) pending_post_call: Option<(NodeId, usize)>,
    /// JIT round 19 wave 3 (lane splice3, SPLICE2-5): `(join value φ, miss
    /// call)` when [`Self::pending_post_call`] is a nested guarded splice's
    /// miss call raised at its join ([`ir_nested_miss_post_call_exit_enabled`]):
    /// the post-call snapshot names the call's result where the walk's stack
    /// holds the φ. Taken by [`Self::push_post_call_splice_state`].
    pub(in crate::ir) post_call_join_value: Option<(NodeId, NodeId)>,
    /// Slots the chain snapshots of this build hold so far, against the same
    /// budget the caller's snapshots are bounded by
    /// ([`IR_MAX_FRAME_SNAPSHOT_WORDS`]). Past it the builder stops recording
    /// them, which costs precision (a trap there resumes at the outermost
    /// `invoke`, as with the switch off), never correctness.
    pub(in crate::ir) splice_state_words: usize,
    /// Round 13 wave 3 (lane framestate, W5-3 stage 3): relax the splice
    /// replay fences at a pc whose chain snapshot this build recorded
    /// ([`ir_splice_chain_fences_enabled`]; [`Self::chain_state_at`]). Read
    /// with [`Self::splice_frame_states`], and only ever `true` with it.
    pub(in crate::ir) splice_chain_fences: bool,
    /// Values proven non-null in the region the walk is in, by a dominating
    /// `ifnull`/`ifnonnull` edge (relaxation 2 of [`SpliceFenceRelax`]).
    /// Dropped at every merge activation, splice entry and splice exit.
    pub(in crate::ir) splice_null_facts: HashSet<NodeId>,
    /// JIT r24: combined pcs of planned splices whose bodies cannot raise
    /// ([`Self::plan_pure_splice_sites`]); admitted inside a covered `try`.
    pub(in crate::ir) pure_splice_sites: HashSet<usize>,
    /// `getstatic` pcs of box-returning converting handle sites whose
    /// member body, planned there, cannot trap
    /// ([`Self::plan_convert_member_splices`]): spliced, not called.
    pub(in crate::ir) convert_member_pure: HashSet<usize>,
    /// A spliced convert member's box, keyed by its splice's `invoke_pc`
    /// (the `getstatic` pc): `(box info_ptr, invokeExact pc)`, built when
    /// the body returns ([`Self::box_convert_member_result`]).
    pub(in crate::ir) convert_box_after: HashMap<usize, (usize, usize)>,
    /// Where the walk continues after an opcode hook opened a splice in
    /// place of the instruction (the convert member splice).
    pub(in crate::ir) pending_splice_jump: Option<usize>,
    /// The compiling method's own code length (`build`'s `code_len`).
    pub(in crate::ir) root_code_len: usize,
    /// `ifnonnull v` taken-edge projection -> `v`. Read when that edge's
    /// target merge activates with the edge as its only input
    /// ([`Self::note_splice_merge_null_facts`]).
    pub(in crate::ir) splice_edge_null_facts: HashMap<NodeId, NodeId>,
    /// Conditional-branch pcs whose NOT-TAKEN edge the profile never observed,
    /// and which [`Self::prune_always_taken_branch`] may therefore replace with
    /// a guarded unconditional jump.
    ///
    /// Populated by `lib.rs` from the method's branch profile, and empty
    /// whenever there is no profile or the feature is off — in which case every
    /// branch builds the ordinary `Op::If` and codegen is byte-identical.
    pub(in crate::ir) pruned_always_taken: std::collections::HashSet<usize>,
    /// How many branches this build actually pruned. Diagnostic; read through
    /// [`Self::branches_pruned`].
    pub(in crate::ir) branches_pruned: usize,
    /// The highest target of any branch [`Self::prune_always_taken_branch`]
    /// pruned (0 when none was). Below it, the main walk steps over an
    /// instruction reached with dead control instead of refusing: that is the
    /// pruned COLD ARM, walked as dead code so that a merge inside it which
    /// some OTHER branch targets is still activated (round 9 wave 2,
    /// `ir-branch-prune-skips-the-cold-arm-by-pc`).
    pub(in crate::ir) pruned_dead_until: usize,
    /// nio21 wave 17 (lane L4): conditional-branch pcs whose TAKEN edge the
    /// profile never observed, which [`Self::prune_never_taken_branch`] may
    /// replace with a guard on the fall-through (HotSpot's `unstable_if`).
    /// Fed by `lib.rs` (`ir_never_taken_branches`); empty without a profile or
    /// under `CRATONVM_JIT_IR_PRUNE_NEVER_TAKEN=0`, which builds exactly what
    /// every earlier build built.
    pub(in crate::ir) pruned_never_taken: std::collections::HashSet<usize>,
    /// JIT round 23 wave 4 (lane L1, CF23-2): combined pcs of SPLICED branches
    /// whose taken edge the callee's own profile never saw
    /// ([`Self::set_splice_never_taken`]). Empty: no prune inside a splice.
    pub(in crate::ir) splice_never_taken: std::collections::HashSet<usize>,
    /// CF23-2: combined pcs only those branches' taken edges reach, stepped
    /// over when the walk reaches them with dead control.
    pub(in crate::ir) splice_never_taken_dead: std::collections::HashSet<usize>,
    /// The lowest TARGET of any branch [`Self::prune_never_taken_branch`]
    /// pruned (`usize::MAX` when none was). At or past it, outside a splice
    /// and in the pc-order walk, an instruction reached with dead control is
    /// the pruned cold arm (or code only it reached): the walk steps over it,
    /// as it does below [`Self::pruned_dead_until`]. A merge in it with a live
    /// predecessor is activated before the test; a lost edge is refused by
    /// [`Self::audit_unactivated_merges`].
    pub(in crate::ir) never_taken_dead_from: usize,
    /// How many never-taken branches this build pruned (diagnostic).
    pub(in crate::ir) never_taken_pruned: usize,
    /// The `(branch pc, target)` taken edges [`Self::prune_never_taken_branch`]
    /// removed, in prune order.
    pub(in crate::ir) never_taken_edges: Vec<(usize, usize)>,
    /// The method's normally reachable pcs with [`Self::never_taken_edges`]
    /// removed, memoised against that list's length
    /// ([`Self::never_taken_pc_is_dead`]).
    pub(in crate::ir) never_taken_live: Option<(usize, HashSet<usize>)>,
    /// Round 16 wave 7 (lane arraylist, AL16-1):
    /// [`ir_inline_cold_athrow_enabled`], read once per build that splices.
    /// `false` everywhere else, which builds exactly what every earlier build
    /// built.
    pub(in crate::ir) inline_cold_athrow: bool,
    /// The cold throw arms ([`cold_throw_arms`]) of each spliced body this
    /// build has branched in, keyed by the body's `SpliceFrame::base`; pcs
    /// body-relative. Filled lazily by [`Self::prune_cold_throw_arm`] and
    /// [`Self::prune_cold_throw_side_entry`], with each body's side entries
    /// ([`cold_throw_shape`]).
    pub(in crate::ir) cold_throw_arms_by_base: HashMap<usize, (Vec<ColdThrowArm>, Vec<ColdThrowSideEntry>)>,
    /// `[from, until)` in combined-buffer pcs: the cold throw arm the last
    /// [`Self::prune_cold_throw_arm`] turned into a trap. The main walk steps
    /// over it with dead control, as it does over a pruned branch's arm
    /// ([`Self::pruned_dead_until`]); `until` is the branch target, a merge.
    pub(in crate::ir) cold_throw_dead: (usize, usize),
    /// Round nio21 wave 1 (lane L2): this method's exception table as the
    /// compiled local throw edges read it ([`IrLocalThrowRow`], table order),
    /// set by `lib.rs` only under [`ir_local_throw_edges_enabled`] for a
    /// method-entry compile. Empty everywhere else, which builds exactly what
    /// every earlier build built.
    pub(in crate::ir) local_throw_rows: Vec<IrLocalThrowRow>,
    /// JIT round 20 wave 11 (lane pIR, L2-3 / L2W2-4): `(thrown, catch) ->
    /// is the thrown class a subclass of the catch class`, as the VM's
    /// handler search answers it, for every pair of a resolved `new` class of
    /// this method and a resolved catch class of [`Self::local_throw_rows`]
    /// (`CompileRequest::class_is_subclass_resolver`). A pair not in the map
    /// is undecided. Set by `lib.rs` only under
    /// [`ir_local_throw_superclass_catch_enabled`]; empty (the default)
    /// decides only an exact class or a catch-all, as every earlier build.
    pub(in crate::ir) local_throw_subclass: HashMap<(u32, u32), bool>,
    /// The handler pcs an `athrow` of this method may branch to
    /// ([`Self::plan_local_throw_reachability`]). Only a handler in this set
    /// has its body in the walk's reachable set.
    pub(in crate::ir) local_throw_handlers: HashSet<usize>,
    /// The pcs the local throw edges added to the reachable set: handler code
    /// that normal control flow never reaches. The walk steps over such a pc
    /// with dead control (its handler took no edge in this build), as it does
    /// over a pruned branch's cold arm.
    pub(in crate::ir) local_throw_optional: HashSet<usize>,
    /// How many `athrow`s this build turned into a local edge. Diagnostic.
    pub(in crate::ir) local_throw_edges: usize,
    /// `CRATONVM_JIT_IR_SPLICED_THROW_EDGES`: the interned type-check target
    /// (`intern_typecheck_target`) of each row of [`Self::local_throw_rows`]
    /// whose catch class resolved (same order; `None` for a catch-all or an
    /// unresolved row). Local and spliced `athrow` edges test thrown values
    /// against the matching row target.
    pub(in crate::ir) local_throw_catch_targets: Vec<Option<(usize, usize)>>,
    /// How many spliced `athrow`s this build turned into a guarded edge into
    /// a handler of the compiling method. Diagnostic.
    pub(in crate::ir) spliced_throw_edges: usize,
    /// The VM's JVMTI `Exception` / `ExceptionCatch` union flags, when an
    /// exception event may be armed at run time
    /// ([`Self::set_exception_event_guard`]). `Some`: the method's own local
    /// throw edges stay off, and a spliced throw edge tests both bytes.
    pub(in crate::ir) exception_event_guard: Option<[usize; 2]>,
    /// nio21 wave 3, lane L2: the catch-type name of each row of
    /// [`Self::local_throw_rows`] (same order, `""` = catch-all), set by
    /// `lib.rs` only under [`ir_call_site_exception_edges_enabled`]. Empty
    /// (the default) plans no call-site exception edge.
    pub(in crate::ir) call_throw_names: Vec<&'static str>,
    /// The compiling method's declaring class, for the catch-type resolution
    /// `jit_local_handler_lookup` makes through its loader.
    pub(in crate::ir) call_throw_declaring_class: u32,
    /// `pc -> handler pcs` (table order) of every invoke the planner gave
    /// call-site exception edges ([`Self::plan_local_throw_reachability`]).
    pub(in crate::ir) call_throw_planned: HashMap<usize, Vec<usize>>,
    /// How many call sites this build gave exception edges. Diagnostic.
    pub(in crate::ir) call_throw_edges: usize,
    /// nio21 wave 5, lane L2 ([`ir_implicit_exception_edges_enabled`]):
    /// `pc -> handler pcs` (table order) of every array access and integer
    /// division the planner gave IMPLICIT exception edges
    /// ([`implicit_site_throw_plan`]).
    pub(in crate::ir) implicit_throw_planned: HashMap<usize, Vec<usize>>,
    /// How many implicit sites this build gave exception edges. Diagnostic.
    pub(in crate::ir) implicit_throw_edges: usize,
    /// t1 wave 10 (lane LCAP2): plan no implicit edges in this build even
    /// under their switch ([`Self::set_implicit_throw_edges_withheld`]).
    pub(in crate::ir) implicit_throw_edges_withheld: bool,
    /// nio21 wave 4, lane L2 ([`ir_splice_callee_handlers_enabled`]): the
    /// admitted exception rows of each spliced body that has them, keyed by
    /// the body's base, in combined-buffer pcs ([`IrInlineTables::splice_handler_rows`]).
    pub(in crate::ir) splice_handler_rows: HashMap<usize, Vec<IrSpliceHandlerRow>>,
    /// May a body with [`Self::splice_handler_rows`] be spliced in this
    /// compile? Set by `lib.rs` under the same gate it plans such a site
    /// under; off, `begin_splice` refuses the body (fail closed).
    pub(in crate::ir) splice_handler_edges: bool,
    /// Combined pcs of the handler entries of those bodies.
    pub(in crate::ir) splice_handler_entries: HashSet<usize>,
    /// Combined pcs of those bodies that normal flow never reaches (their
    /// masked handler runs): stepped over with dead control.
    pub(in crate::ir) splice_handler_dead: HashSet<usize>,
    /// The spliced calls [`Self::plant_call_throw_edges`] gave edges.
    pub(in crate::ir) splice_handler_planted_calls: HashSet<NodeId>,
    /// `(pc, first node id)` of the instruction just built inside a covered
    /// `try` range, checked at the next instruction boundary
    /// ([`Self::splice_handler_nodes_armed`]).
    pub(in crate::ir) splice_handler_mark: Option<(usize, usize)>,
    /// Diagnostic-only: `pc → "0xNN cn.mn desc"` for every invoke site in the
    /// method. Populated by `lib.rs` **only** when [`ir_bail_reporting`] is on,
    /// and read **only** by [`Self::bail_invoke`]. Never consulted by lowering,
    /// so an absent or stale entry cannot change a compile.
    ///
    /// It exists because the invoke bails below report a line number and a
    /// pc, and the question every one of them raises — *which callee* — was
    /// otherwise a `javap` away for each event. `cov-04`'s whole first
    /// increment was "group the 53 by callee"; this is what makes that a grep.
    pub(in crate::ir) invoke_labels: HashMap<usize, Box<str>>,
    /// Diagnostic-only companion to [`Self::invoke_labels`]: the compiling
    /// method's own `cn.mn desc`. Without it the caller has to be recovered by
    /// pairing a bail line with the nearest preceding `[ir] invoke-plan` line,
    /// which is an *assumption about log interleaving* rather than a fact —
    /// exactly the kind of inferred join this directory's rule 6 says not to
    /// trust. Same lifetime and same flag as `invoke_labels`.
    pub(in crate::ir) method_label: Option<Box<str>>,
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
    pub(in crate::ir) spliced_bodies_pure: bool,
    /// JIT round 23 wave 2b, lane L1 (churn page J1;
    /// `CRATONVM_JIT_IR_OSR_DOOR_TRAPS_IN_PLACE`): this is the optimizing OSR
    /// DOOR's build (`CompileRequest::osr_mode_exit_polls`), whose body is
    /// never published and is entered only through its OSR stubs. Its site
    /// traps leave through `try_osr`'s planless guard-exit transfer, which
    /// writes the trap's frame into the live interpreter frame and resumes AT
    /// the trap -- no replay from method entry -- and the door refuses the
    /// whole body (method-wide) unless the lowerer admitted every guard exit
    /// for that transfer (`osr_optimizing_guard_exits_admissible`). So the
    /// replay guard, whose question is "may the method be replayed from
    /// entry", is not this build's question for a trap OFF every loop (see
    /// [`Self::trap_resumable_here`]). Set by `lib.rs` with
    /// [`Self::set_osr_door_traps_in_place`]; `false` for every other build.
    pub(in crate::ir) osr_door_traps_in_place: bool,
    /// JIT round 23 wave 4 (proxy-define page;
    /// `CRATONVM_JIT_IR_UNRESOLVED_TYPECHECK_IN_PLACE`, opt-in): an own-code
    /// `checkcast` / `instanceof` of a class that is not loaded is planted as a
    /// site trap even when it is unresumable by whole-method replay, because the
    /// trap's own frame state resumes it in place. Set by `lib.rs` only for a
    /// compile whose sinks resume such a trap precisely; see
    /// [`Self::set_typecheck_traps_resume_in_place`]. `false` for every other
    /// build.
    pub(in crate::ir) typecheck_traps_resume_in_place: bool,
    /// JIT round 23 wave 8b (lane W8-L1,
    /// `CRATONVM_JIT_IR_IN_PLACE_TRAP_DEAD_PLACEHOLDER`): did the most recent
    /// successful [`Self::plant_uncommon_trap`] plant an in-place trap? Read by
    /// [`Self::trap_placeholder_ref`] right after the plant.
    pub(in crate::ir) last_trap_in_place: bool,
    /// The fresh (never interned) `Ref` zero nodes [`Self::trap_placeholder_ref`]
    /// pushed after an in-place trap. Each is defined only after an
    /// always-failing guard, so a frame holding one directly is unreachable at
    /// run time, and [`Self::frame_resumes_in_place_cleanly`] does not count
    /// it as the null constant it rejects.
    pub(in crate::ir) in_place_trap_placeholders: HashSet<NodeId>,
    /// `Op::New` nodes whose `<init>` has not yet completed (`0xbb` inserts,
    /// `0xb7` `<init>` elision/call/splice-return removes). An initialized
    /// `Op::New` is safe for in-place trap resumption; an uninitialized one
    /// is not.
    pub(in crate::ir) uninitialized_news: HashSet<NodeId>,
    /// JVMS §6.5 `ireturn` narrowing for THIS method's own return type
    /// ([`crate::narrowed_int_return_tag`]), set by
    /// [`Self::set_return_descriptor`]. `None` — narrow nothing — for `I`, for
    /// every non-int return, and for a hand-built/test builder that never said.
    /// A spliced callee's `ireturn` uses its own frame's tag instead.
    pub(in crate::ir) return_narrow: Option<u8>,
    /// Set by `lib.rs` when every adapter run in the method is proven by its
    /// constant pool (`crate::lambda_int_to_double_runs_proven`; interpreter
    /// round i1 wave 10 — it used to name the TDigest `Dist.quantile` / `.cdf`
    /// kernels, the shape's origin): lets the `invokestatic` arm fold the
    /// `Integer.valueOf` / `Function.apply` / `checkcast Double` /
    /// `doubleValue` adapter run into one [`Op::LambdaIntToDouble`] — only
    /// when [`Self::tdigest_adapter_run_matches`] confirms the run's members,
    /// not just its opcode bytes.
    pub tdigest_scalar_kernel: bool,
    /// inc 26/35: resolved `ldc2_w` (0x14) constant values (`pc → (bits, is_double)`).
    /// Set by [`Self::set_ldc2w_info`]; an `ldc2_w` pc not present bails to
    /// single-pass. `is_double` selects the lowering: a `long` constant becomes
    /// `Op::Const(Long)` (`lconst`), a `double` constant `Op::ConstF` (`dconst`).
    pub(in crate::ir) ldc2w_info: HashMap<usize, (i64, bool)>,
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
    pub(in crate::ir) wide_field_long: bool,
    /// COV-03: may a `getfield`/`putfield` of an `F`/`D` field build? Same
    /// reasoning as [`Self::wide_field_long`], for `ir_emit_fp`.
    pub(in crate::ir) wide_field_fp: bool,
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
    pub(in crate::ir) ldc_info: HashMap<usize, (i64, bool)>,
    /// cov-01 increment 2: resolved `ldc <String>` sites (`pc → (bytes, len)`),
    /// the ADDRESS and length of the literal's UTF-8, not a reference. Set by
    /// [`Self::set_ldc_string_info`]. The caller owns the bytes for the
    /// artifact's lifetime; see [`Op::ConstString`] for why the object itself
    /// is re-fetched on every execution instead.
    pub(in crate::ir) ldc_string_info: HashMap<usize, (u32, u16)>,
    /// cov-01 increment 3: resolved `ldc <Class>` sites
    /// (`pc → (holder_class_id, cp_idx)`). Set by
    /// [`Self::set_ldc_class_info`]; a site whose helper is unwired is simply
    /// absent, so the arm bails. See [`Op::ConstClass`].
    pub(in crate::ir) ldc_class_info: HashMap<usize, (u32, u16)>,
    /// Round 9 wave 8 (irl8): `(holder_class_id, cp_idx) -> slot address` for
    /// `ldc <String>` / `ldc <Class>` sites whose resolved constant the VM
    /// keeps in a GC-maintained word (`JitLdcConstant::StringSlot` /
    /// `ClassMirrorSlot`). Set by [`Self::set_ldc_slot_info`]; copied into
    /// the node's `slot_addr`. Keyed by SITE, like the single-pass
    /// `BackendRequest::ldc_slots`. Empty = every site calls its helper.
    pub(in crate::ir) ldc_slot_info: HashMap<(u32, u16), usize>,
    /// JIT r23 wave 4b, lane L2 (`CRATONVM_JIT_IR_LDC_CONDY_SLOT`): `pc -> slot
    /// address` of the compiling method's `ldc` sites of a resolved reference
    /// `CONSTANT_Dynamic` (`JitLdcConstant::DynamicSlot`), built as an
    /// [`Op::IndyRootRef`] of the slot. Never consulted inside a splice (the
    /// pcs are the compiling method's own). Set by
    /// [`Self::set_ldc_condy_slot_info`].
    pub(in crate::ir) ldc_condy_slot_info: HashMap<usize, usize>,
    /// cov-01 increment 4: resolved `getstatic` (0xb2) sites
    /// (`pc → (class_id, field_index, type_tag, is_volatile)`) — the same tuple
    /// the single-pass backend's `static_field_info` carries. Set by
    /// [`Self::set_static_field_info`]; an unresolvable site is absent and the
    /// arm bails the method. See [`Op::LoadStatic`].
    pub(in crate::ir) static_field_info: HashMap<usize, (u32, usize, u8, bool)>,
    /// `(declaring class id, static field index) -> class id` of the object a
    /// trusted `static final` reference field holds, for this build's static
    /// reads only. See [`Self::set_static_final_exact_classes`].
    pub(in crate::ir) static_final_exact_classes: HashMap<(u32, u32), u32>,
    /// `CRATONVM_FROZEN_PAGES`: the VM's snapshot of objects on frozen pages.
    /// See [`Self::set_frozen_facts`].
    pub(in crate::ir) frozen_facts: Option<std::sync::Arc<crate::frozen_facts::FrozenFacts>>,
    /// Reference loads proven to be an object of [`Self::frozen_facts`], by
    /// its id. A node enters only with a generation guard planted right after
    /// it, so the fact holds wherever the node can be used.
    pub(in crate::ir) frozen_nodes: HashMap<NodeId, u32>,
    /// Stage 2: class name, or call site `"class.name:descriptor@bci"`, ->
    /// the object of [`Self::frozen_facts`] a receiver of that class, or of a
    /// body spliced at that site, is expected to be
    /// ([`Self::set_frozen_receivers`]). Believed only behind an identity guard.
    pub(in crate::ir) frozen_expected: HashMap<String, u32>,
    /// `(node, object, splice depth)`: receivers an identity guard on the
    /// path to the walk point proved to be that object. Dropped where the
    /// field-load memo's entries of that depth are (a join), so the guard
    /// dominates every use.
    pub(in crate::ir) frozen_known: Vec<(NodeId, u32, usize)>,
    /// The bodies that never write local 0, by their first pc in the
    /// combined buffer (`0`: the compiling method). There local 0 is the
    /// receiver at every point, whatever node carries it (a loop's phi).
    pub(in crate::ir) frozen_keeps_this: HashSet<usize>,
    /// The registry an identity guard that keeps failing is withdrawn
    /// through, and the compiling method's key in it. Without one no
    /// identity guard is planted.
    pub(in crate::ir) frozen_despec: Option<(std::sync::Arc<crate::deopt::DespecRegistry>, String)>,
    /// The same for the static-handle identity guard
    /// ([`Self::plant_static_guess_guard`]); without one none is planted.
    pub(in crate::ir) static_guess_despec: Option<(std::sync::Arc<crate::deopt::DespecRegistry>, String)>,
    pub(in crate::ir) object_class_id: Option<u32>,
    pub(in crate::ir) const_folds: ConstFoldTables,
    pub(in crate::ir) const_ref_slots: HashMap<NodeId, usize>,
    /// A constant root's exact class (`ConstFoldTables`'s rows name it): the
    /// object in a trusted `static final` / `@Stable` / trusted-final slot is
    /// fixed, and so is its class. Read by [`Self::static_final_exact_class_of`].
    pub(in crate::ir) const_ref_exact_classes: HashMap<NodeId, u32>,
    pub(in crate::ir) const_folded_dead_skip: bool,
    pub(in crate::ir) const_folded_dead_targets: HashSet<usize>,
    /// cov-05 increment 1: resolved `instanceof` (0xc1) sites whose target
    /// class is already loaded (`pc → (name_ptr, name_len)`), the same
    /// (address, length) shape [`Self::ldc_string_info`] carries. Set by
    /// [`Self::set_instanceof_info`]; a pc not present — target not yet
    /// loaded, or no resolver supplied — bails that `instanceof` to
    /// single-pass. See [`Op::InstanceOf`].
    pub(in crate::ir) instanceof_info: HashMap<usize, (usize, usize)>,
    /// cov-05: resolved `checkcast` (0xc0) sites whose target class is
    /// already loaded, same `(name_ptr, name_len)` shape as
    /// [`Self::instanceof_info`]. Set by [`Self::set_checkcast_info`]; a pc
    /// not present bails that `checkcast` to single-pass. See
    /// [`Op::CheckCast`].
    pub(in crate::ir) checkcast_info: HashMap<usize, (usize, usize)>,
    /// The resolved `java/lang/String` field layout the String access
    /// expander decodes against, or `None` — in which case every String
    /// call site keeps the dispatch it has today. `None` at construction and
    /// set per compile by [`Self::set_string_layout`] (round 11 wave 3: it
    /// used to be seeded from a process-wide `OnceLock`, which can hold
    /// another VM's field indices — see [`PUBLISHED_STRING_LAYOUT`]).
    pub(in crate::ir) string_layout: Option<crate::StringFieldLayout>,
    /// JIT round 21 (lane strintr2, STRINTR21-1): this method's own
    /// `invokevirtual` pcs whose String-accessor guard the VM has withdrawn
    /// ([`Self::set_string_access_withdrawn_pcs`]); such a site is built as
    /// its `Op::Call`, never expanded. Empty unless set.
    pub(in crate::ir) string_access_withdrawn_pcs: HashSet<usize>,
    /// nio21 wave 19 (lane L3): this compile's `ThreadLocal.get()` expansion
    /// layout ([`Self::set_thread_local_layout`]), or `None`: every
    /// `ThreadLocal.get()` site keeps its call.
    pub(in crate::ir) thread_local_layout: Option<ThreadLocalIrLayout>,
    pub(crate) vthread_guard: Option<(u32, u32)>,
    /// JIT round 21 (lane refs2; re-landed t1 wave 9, lane REFS2):
    /// `Reference.referent`'s index for the `refersTo` expansion when
    /// [`Self::thread_local_layout`] is `None`
    /// ([`Self::set_refers_to_referent_index`]).
    pub(in crate::ir) refers_to_referent_index: Option<u32>,
    /// t1 wave 12 (lane REFS4, `CRATONVM_JIT_IR_REFERS_TO_COMPACT_ROW`):
    /// `1 + compact displacement` of `Reference.referent`, `0` = none
    /// ([`Self::set_refers_to_referent_row`]).
    pub(in crate::ir) refers_to_referent_row: u32,
    /// j27 wave 1 (lane TYPECHK, `CRATONVM_JIT_IR_REFERENT_SPLICE_COMPACT_ROW`):
    /// `1 + compact displacement` of `Reference.referent` for the spliced
    /// `Reference.get` reads (`Graph::referent_load_pcs`), `0` = none
    /// ([`Self::set_spliced_referent_row`]).
    pub(in crate::ir) spliced_referent_row: u32,
    /// JIT round 21 (lane tlsub, TL321-4 / TL321-1; re-landed t1 wave 8): the
    /// receiver profiles, the VM's subclass query and the de-spec registry
    /// the `ThreadLocal` expansion reads ([`Self::set_thread_local_site_plan`]),
    /// or `None`.
    pub(in crate::ir) tl_site_plan: Option<Box<ThreadLocalSitePlan>>,
    /// The newest `Guard` the `ThreadLocal` expansion's trap form planted
    /// (`tl_test`), or [`NO_NODE`]: the licensing guard its later reads name
    /// (`with_licensing_guard`), so the scheduler cannot float a read of a
    /// not-yet-class-tested base above its guard (t1 wave 8 review of the
    /// r21 trap form, which ordered them only positionally; the LOOPS21-4
    /// shape). Reset per expansion.
    pub(in crate::ir) tl_license: NodeId,
    /// JIT round 23 wave 9 (lane W9-L2, CF-W9-3): this compile's helper
    /// table, for the constant-`VarHandle` plans
    /// [`Self::try_vh_const_fresh_field_access`] asks; `None` builds every
    /// `VarHandle` site as its call.
    pub(in crate::ir) vh_fresh_helpers: Option<crate::DirectHelperTable>,
    /// JIT round 21 (lane g1vhsoft): the `WeakReference.get()` /
    /// `SoftReference.get()` expansion's layout
    /// ([`Self::set_reference_get_layout`]), or `None`: every such site keeps
    /// its call (or its splice).
    pub(in crate::ir) reference_get_layout: Option<ReferenceGetIrLayout>,
    /// `(value, type)` → the lowest-id `Op::Const` node of that value and
    /// type, for every node below [`Self::const_intern_upto`]. What
    /// [`Self::iconst`] and [`Self::aconst_null`] dedupe against, in place of
    /// the arena scan each call used to make. See [`Self::interned_const`] for
    /// how it stays true of a graph that changes under it.
    pub(in crate::ir) const_intern: HashMap<(i64, IrType), NodeId>,
    /// How far into `graph.nodes` [`Self::const_intern`] has indexed.
    pub(in crate::ir) const_intern_upto: usize,
    /// True while [`Self::build`] walks the caller's bytecode block by block,
    /// in reverse post-order, instead of in pc order. That happens only for a
    /// method with a rotated loop; see [`BlockWalk`]. Read by
    /// [`Self::prune_always_taken_branch`]: its `pc = target` jump is a
    /// pc-order move that the block walk's ranges cannot follow.
    pub(in crate::ir) block_walk: bool,
    /// Round 11 wave 14 (lane osrmerge): the method's per-pc live-locals
    /// bitsets, handler edges included (`regalloc::live_locals_per_pc_all`:
    /// `(rows, covered, words)`), set by [`Self::set_osr_opaque_liveness`]
    /// only under `CRATONVM_JIT_OSR_OPAQUE_ENTRY`. `Some` makes every loop
    /// header the walk activates an OSR-open one when it can
    /// ([`Self::build_opaque_osr_merge`]); `None` is the pre-wave-14 builder.
    pub(in crate::ir) osr_opaque_liveness: Option<(Vec<u64>, Vec<bool>, usize)>,
    /// al16 (2026-10-05): the header the OSR route was asked to enter
    /// (`CompileRequest::osr_requested_bci`); see
    /// [`Self::set_osr_requested_header`].
    pub(in crate::ir) osr_requested_header: Option<usize>,
    /// Round 11 wave 14: per opaque header pc, the `(local, Op::OsrLocal)`
    /// pairs its arm reads — what [`Self::patch_loop_backedge`] checks the
    /// completed header φ types against.
    pub(in crate::ir) osr_opaque_locals: HashMap<usize, Vec<(usize, NodeId)>>,
    /// JIT round 19 wave 5 (lane handler5, SPLICE4-1): the method's exception
    /// table as [`Self::set_catch_arm_table`] supplied it (empty: no compiled
    /// handler entries), and the declaring class its catch types resolve in.
    pub(in crate::ir) catch_arm_rows: Vec<CatchArmRow>,
    pub(in crate::ir) catch_arm_class_id: u32,
    /// SPLICE4-1: `invoke pc -> row indices` for every call the plan
    /// ([`Self::plan_catch_arms`]) admits an arm for: the first covering row
    /// alone, or under `CRATONVM_JIT_IR_CATCH_ARM_LADDER` (HANDLER19-3) the
    /// longest table-order prefix of covering rows whose handlers all qualify.
    pub(in crate::ir) catch_arm_calls: HashMap<usize, Vec<usize>>,
    /// SPLICE4-1: per admitted `handler_pc`, its region (the pcs normal flow
    /// reaches from it that are not normally reachable from entry), the
    /// region's merge targets, and whether an arm opened it yet.
    pub(in crate::ir) catch_arm_handlers: HashMap<usize, CatchHandlerRegion>,
    /// SPLICE4-1: the pcs of every OPENED region -- walked although
    /// `normally_reachable_pcs` leaves them out.
    pub(in crate::ir) catch_arm_walkable: HashSet<usize>,
    /// OSR round (2026-10-06): `local -> class` for the slots an opaque arm
    /// imports with a class test of its own, so the slot's exact-receiver
    /// fact survives the arm (see the arm's withdrawal in
    /// [`Self::activate_loop_header`]). Empty until such a test is built.
    pub(in crate::ir) osr_imported_exact: HashMap<usize, u32>,
    /// `CRATONVM_JIT_IR_OSR_PROFILED_SLOT_CHECK`: write-once caller locals
    /// (bytecode fact) an in-loop site's one-class receiver profile names,
    /// `slot -> class` ([`Self::set_osr_profiled_slot_classes`]).
    pub(in crate::ir) osr_profiled_slot_classes: HashMap<usize, u32>,
    /// Interpreter round i5 wave 107 (lane L2): the profiled slots
    /// [`Self::guard_osr_arm_exact_locals`] admitted at the requested header,
    /// `slot -> class`. Taken by [`Self::activate_loop_header`], which turns
    /// each into a NODE fact ([`Self::osr_profiled_exact_nodes`]). Until
    /// wave 107 the admission wrote the slot-wide facts
    /// (`exact_receiver_single_store_slots` / `exact_receiver_locals`), which
    /// every later site of the walk read -- including a site the header's
    /// checks do not dominate (the `else` arm of an outer-loop body laid out
    /// after the inner loop), whose receiver is the slot's one store's value,
    /// never tested: `L2W107OsrProfiledSlotElseArm`.
    pub(in crate::ir) osr_profiled_admitted: HashMap<usize, u32>,
    /// Interpreter round i5 wave 107 (lane L2): the requested header's local
    /// φ of each admitted profiled slot, `node -> class`. Every entry into the
    /// header tests the class (the walked edge and the arm), the slot has one
    /// store, which runs before the header, and so the φ is exactly that
    /// class on every path it flows along ([`Self::patch_loop_backedge`]
    /// refuses the build if a back edge brings anything else).
    pub(in crate::ir) osr_profiled_exact_nodes: HashMap<NodeId, u32>,
    /// OSR round (2026-10-06): may [`Self::guard_osr_arm_exact_locals`] test
    /// an imported local on the arm? `false` keeps every imported slot's fact
    /// withdrawn; see [`Self::set_osr_arm_exact_checks`].
    pub(in crate::ir) osr_arm_exact_checks: bool,
    /// The tests [`Self::guard_osr_arm_exact_locals`] decided on, built after
    /// the walk by [`Self::finish_osr_arm_checks`]: `(header pc, arm control,
    /// the arm's locals, (imported value, class))`.
    pub(in crate::ir) pending_osr_arm_checks: Vec<(usize, NodeId, Vec<NodeId>, Vec<(NodeId, u32)>)>,
    pub(in crate::ir) splice_bases_written_locals: HashMap<usize, u64>,
    pub(in crate::ir) splice_always_taken: std::collections::HashSet<usize>,
    pub(in crate::ir) splice_always_taken_dead: std::collections::HashSet<usize>,
    pub(in crate::ir) splice_pruned_switch_edges: HashMap<usize, HashSet<usize>>,
    pub(in crate::ir) nio_charset_layout: Option<NioCharsetIrLayout>,
}

thread_local! {
    /// IR site traps planted by the build currently running on this thread.
    ///
    /// Read back by `lib.rs` the moment `build()` returns, the same idiom
    /// `reset_string_access_sites` uses -- `build(mut self, ..)` consumes the
    /// builder, so a getter on it is not available afterwards.
    ///
    /// JIT round 19 proposal wave 7 (lane interp7, EAGER3-19-1 / OSRENTRY6-1):
    /// the planted bytecode pcs, one entry per planted trap, not only their
    /// count, so the compile door can register each trap by `(method, bci)`
    /// ([`crate::JitVerdictRegistry::register_site_trap_at`]). The count is the
    /// list's length. The same thread-local as before (no new static).
    pub(in crate::ir) static SITE_TRAPS_THIS_BUILD: std::cell::RefCell<Vec<u32>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

pub(in crate::ir) fn reset_site_traps_this_build() {
    SITE_TRAPS_THIS_BUILD.with(|c| c.borrow_mut().clear());
}

/// Record one site trap planted at bytecode `pc` by the build running on this
/// thread.
pub(in crate::ir) fn note_site_trap_planted_this_build(pc: usize) {
    let pc = u32::try_from(pc).unwrap_or(u32::MAX);
    SITE_TRAPS_THIS_BUILD.with(|c| c.borrow_mut().push(pc));
}

/// How many IR site traps the build that just returned planted.
pub fn site_traps_planted_this_build() -> usize {
    SITE_TRAPS_THIS_BUILD.with(|c| c.borrow().len())
}

/// The bytecode pcs of the IR site traps the build that just returned planted
/// (EAGER3-19-1 / OSRENTRY6-1), sorted and without duplicates.
pub fn site_trap_pcs_planted_this_build() -> Vec<u32> {
    SITE_TRAPS_THIS_BUILD.with(|c| {
        let mut pcs = c.borrow().clone();
        pcs.sort_unstable();
        pcs.dedup();
        pcs
    })
}

/// The end of the bytecode range an abandoned compiled attempt may already
/// have EXECUTED when it stopped at `bci`: every instruction below the answer
/// may have run, none at or above it can have (round 12 wave 2, lane irbuild).
///
/// The range the VM's replay rule needs for an arbitrary (possibly
/// CONDITIONAL) deopt: `r12w2-irbuild-deopt-resume-replay-extent-patch` makes
/// `deopt_resume::replay_from_entry_is_observably_equivalent` scan
/// `code[..replay_committed_extent(..)]`. A trap that fires on its FIRST
/// arrival needs only the tighter `commits_before_first_arrival` walk.
///
/// # Why `bci` itself is not the answer
///
/// The replay-safety rule (`IrBuilder::trap_replay_is_safe` here, and
/// `deopt_resume::replay_from_entry_is_observably_equivalent` in the VM) used
/// to ask "does `code[..bci]` commit a side effect?", on the argument that the
/// bytecode at `bci` had not completed and "the bytecodes after it never
/// ran". That holds for straight-line code only. Inside a loop, earlier
/// iterations ran the WHOLE body, including every instruction textually after
/// `bci`:
///
/// ```text
/// for (i = 0; i < n; i++) {
///     if (i > 0) { <trap at bci> }
///     counter++;                   // putstatic, pc > bci
/// }
/// ```
///
/// Iteration 0 commits `counter++`, iteration 1 traps, and a replay from entry
/// commits it a second time — while the prefix `code[..bci]` is pure.
///
/// # The rule
///
/// A backward edge `s -> t` with `t` inside the current extent means the
/// instructions up to `s` can run before control returns to `bci`, so the
/// extent grows to the end of `s`; repeated to a fixpoint. An instruction past
/// the fixpoint cannot reach `bci` at all: every path from it back into the
/// extent would need a backward edge from beyond the extent into it, which
/// the fixpoint has already taken. `jsr`/`ret` (whose return edges are not
/// statically known) answer the whole method, conservatively. Exception
/// edges are not followed here: this form is for a frame that never takes
/// one (the optimizing tier's, whose handlers are not in the graph). A frame
/// that enters its own `catch` block ([`replay_committed_extent_with_handlers`])
/// must be asked with its exception table.
///
/// `code_len` bounds the walk (the buffer may be a combined one); the answer
/// is at most `code_len`.
pub fn replay_committed_extent(code: &[u8], code_len: usize, bci: usize) -> usize {
    replay_committed_extent_with_handlers(code, code_len, bci, &[])
}

/// [`replay_committed_extent`] for a frame that may take EXCEPTION edges
/// inside itself: the single-pass tier's local handlers
/// (`JitLocalHandlerSite`, `CRATONVM_JIT_LOCAL_HANDLERS`, default ON since
/// 2026-08-20) enter the method's own `catch` block without leaving compiled
/// code. Round 13 wave 10, lane exc4
/// (`r13w9-replay5-replay-extent-ignores-exception-edges`).
///
/// `handlers` is the exception table as `(start_pc, end_pc, handler_pc)`,
/// `end_pc` exclusive. A row whose handler lies below its range's end is a
/// backward edge `(handler, end)`: every protected pc up to `end` can run and
/// then hand control back to `handler`, at or before the trap. A row whose
/// handler lies at or past `end` is a forward edge and changes nothing: the
/// handler is then reached only by passing the protected pcs, and a way back
/// from it into the extent is an explicit backward branch the fixpoint already
/// takes. javac places every handler after its range except `synchronized`'s
/// self-covering `monitorexit` row (`handler == start`), which this also
/// follows (the stricter direction).
pub(crate) fn collect_replay_backward_edges(
    code: &[u8],
    code_len: usize,
    handlers: &[(usize, usize, usize)],
) -> Result<Vec<(usize, usize)>, usize> {
    let len = code_len.min(code.len());
    // `(target, end of source instruction)` for every backward explicit edge.
    let mut back: Vec<(usize, usize)> = Vec::new();
    let mut pc = 0usize;
    while pc < len {
        let Some(&op) = code.get(pc) else {
            return Err(len);
        };
        if crate::bytecode_analysis::is_subroutine_op(op)
            || crate::bytecode_analysis::is_wide_ret(code, pc)
        {
            return Err(len);
        }
        let end = pc.saturating_add(crate::bytecode_analysis::step(code, pc));
        if matches!(op, 0xaa | 0xab) {
            let clamped_end = end.min(len);
            crate::bytecode_analysis::for_each_switch_target_lenient(code, len, pc, |target| {
                if target <= pc {
                    back.push((target, clamped_end));
                }
            });
        } else if crate::bytecode_analysis::is_offset_branch(op) {
            if let Some(target) = crate::bytecode_analysis::offset_branch_target(&code[..len], pc)
                .filter(|&t| t < len && t <= pc)
            {
                back.push((target, end.min(len)));
            }
        }
        pc = end;
    }
    for &(start, end, handler) in handlers {
        if handler < end && start < end && start < len {
            back.push((handler, end.min(len)));
        }
    }
    Ok(back)
}

pub(crate) fn replay_committed_extent_from_back_edges(
    len: usize,
    bci: usize,
    back: &[(usize, usize)],
) -> usize {
    let mut extent = bci.min(len);
    // Each pass either grows `extent` or stops, and it can grow at most once
    // per edge, so this terminates after at most `back.len() + 1` passes.
    loop {
        let mut grown = false;
        for &(target, end) in back {
            if target <= extent && end > extent {
                extent = end;
                grown = true;
            }
        }
        if !grown {
            return extent;
        }
    }
}

pub fn replay_committed_extent_with_handlers(
    code: &[u8],
    code_len: usize,
    bci: usize,
    handlers: &[(usize, usize, usize)],
) -> usize {
    let len = code_len.min(code.len());
    match collect_replay_backward_edges(code, len, handlers) {
        Ok(back) => replay_committed_extent_from_back_edges(len, bci, &back),
        Err(full_len) => full_len,
    }
}

/// Can an instruction that commits a side effect
/// ([`crate::opcode_commits_side_effect`]) run before control FIRST arrives
/// at `bci`? The question an UNCONDITIONAL trap planted at `bci` has to ask
/// (round 12 wave 2, lane irbuild).
///
/// Such a trap fires on its first arrival, so what the abandoned attempt ran
/// is exactly what is reachable from the entry without passing `bci`: a walk
/// that never expands `bci`. Tighter than [`replay_committed_extent`], which
/// must also cover a CONDITIONAL deopt taken on a later arrival, and still
/// sound inside a loop: `for (…) { if (i > 0) <trap>; counter++; }` reaches
/// the `putstatic` through the untaken arm, while `for (…) { <trap>;
/// counter++; }` cannot reach it without passing the trap.
///
/// `jsr`/`ret` (return edges not statically known) and a walk that loses
/// instruction sync answer `true`, conservatively.
///
/// Exception edges are not walked, deliberately (round 13 wave 10, lane
/// exc4): the only producer of a first-arrival point is an IR site trap, and
/// an optimizing frame never enters a handler (the builder skips handler
/// bodies, a throw leaves the frame). Walking them at the VM's end alone would
/// make it stricter than `IrBuilder::trap_replay_is_safe`, which is the
/// direction that turns a planted trap into a refused replay.
///
/// JIT round 22 wave 11 (lane review): "an optimizing frame never enters a
/// handler" stopped being true with the local throw edges and compiled handler
/// entries. The planter now ALSO asks
/// [`commits_before_first_arrival_with_handlers`] over the body's in-frame
/// rows (`CRATONVM_JIT_IR_TRAP_REPLAY_FOLLOWS_HANDLERS`), so it is the
/// stricter end; the VM sink still asks this handler-blind walk, which is
/// sound only because a trap the planter refuses is never planted.
///
/// Public since round 12 wave 3 (lane replay): the VM's replay rule
/// (`deopt::replay_from_entry_commits_nothing`) asks it for a point that
/// carries `ResumeSemantics::first_arrival`, with `code_len` narrowed to the
/// loop-closed extent. Successors at or past `code_len` are not walked.
pub fn commits_before_first_arrival(code: &[u8], code_len: usize, bci: usize) -> bool {
    let len = code_len.min(code.len());
    let mut seen = vec![false; len];
    let mut work: Vec<usize> = vec![0];
    while let Some(pc) = work.pop() {
        if pc >= len || pc == bci {
            continue;
        }
        let Some(slot) = seen.get_mut(pc) else {
            return true;
        };
        if *slot {
            continue;
        }
        *slot = true;
        let Some(&op) = code.get(pc) else {
            return true;
        };
        if crate::opcode_commits_side_effect(op)
            || crate::bytecode_analysis::is_subroutine_op(op)
            || crate::bytecode_analysis::is_wide_ret(code, pc)
        {
            return true;
        }
        crate::bytecode_analysis::for_each_lenient_successor(code, len, pc, |s| work.push(s));
    }
    false
}

/// JIT round 22 wave 10 (lane throws, `CRATONVM_JIT_IR_TRAP_REPLAY_FOLLOWS_HANDLERS`,
/// correctness, **default ON**, `=0` restores the handler-blind walk):
/// [`commits_before_first_arrival`] that ALSO follows exception edges: every
/// row `(start, end, handler)` makes `handler` a successor of every pc in
/// `[start, end)` the walk reaches.
///
/// Why: the first-arrival walk's premise ("no producer of a first-arrival
/// point takes an exception edge", `deopt::replay_from_entry_commits_nothing_with_handlers`)
/// stopped holding when optimizing bodies started entering their OWN handlers
/// in compiled code (`CRATONVM_JIT_IR_LOCAL_THROW_EDGES`, default ON; the
/// implicit / call-site edges, opt-in; compiled handler entries). Then a
/// handler can run a side effect placed textually AFTER a site trap and
/// reach the trap later, e.g. `for (i..2) { try { if (i == 0) throw ex; <trap>
/// } finally { CNT++; } }`-style shapes and `try { .. } catch (..) { for (..)
/// { if (late) <trap>; CNT++; } }`: the handler-blind walk saw nothing
/// committed, the textual prefix neither, the trap was planted, and a
/// whole-method replay (the only fallback of, e.g., a `synchronized` method)
/// committed `CNT++` twice.
pub fn commits_before_first_arrival_with_handlers(
    code: &[u8],
    code_len: usize,
    bci: usize,
    rows: &[(usize, usize, usize)],
) -> bool {
    let len = code_len.min(code.len());
    let mut seen = vec![false; len];
    let mut work: Vec<usize> = vec![0];
    while let Some(pc) = work.pop() {
        if pc >= len || pc == bci {
            continue;
        }
        let Some(slot) = seen.get_mut(pc) else {
            return true;
        };
        if *slot {
            continue;
        }
        *slot = true;
        let Some(&op) = code.get(pc) else {
            return true;
        };
        if crate::opcode_commits_side_effect(op)
            || crate::bytecode_analysis::is_subroutine_op(op)
            || crate::bytecode_analysis::is_wide_ret(code, pc)
        {
            return true;
        }
        crate::bytecode_analysis::for_each_lenient_successor(code, len, pc, |s| work.push(s));
        for &(start, end, handler) in rows {
            if start <= pc && pc < end {
                work.push(handler);
            }
        }
    }
    false
}





/// Kill switch for [`commits_before_first_arrival`] in
/// [`IrBuilder::trap_replay_is_safe`]. **Default ON**;
/// `CRATONVM_JIT_IR_TRAP_REPLAY_LOOP_EXTENT=0` restores the textual-prefix
/// rule alone. Read once per planted trap, never on a hot path.
pub(in crate::ir) fn ir_trap_replay_first_arrival_enabled() -> bool {
    true
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
            .map(|(&key, s)| (s.base, s.code_len, inline_site_caller_pc(key)))
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
    pub(in crate::ir) static SPLICE_RESUME_THIS_BUILD: std::cell::RefCell<Option<IrSpliceResumeMap>> =
        const { std::cell::RefCell::new(None) };
}

pub(in crate::ir) fn set_splice_resume_map_this_build(map: Option<IrSpliceResumeMap>) {
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
pub struct IrBuildResultsScope(pub(in crate::ir) ());

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

    /// The bytecode pcs of the site traps the build inside this scope planted
    /// (EAGER3-19-1 / OSRENTRY6-1), sorted, without duplicates.
    pub fn site_trap_pcs_planted(&self) -> Vec<u32> {
        site_trap_pcs_planted_this_build()
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

/// One VM's site-trap memo, owned by its [`crate::JitVerdictRegistry`] and
/// reached through the registry's `register_site_trap_method`,
/// `method_has_site_trap`, `any_site_trap_registered` and
/// `claim_site_trap_decision` (below).
///
/// Two sets and a flag:
///
/// * `trapped` — methods whose optimizing body carries at least one planted
///   site trap. The runtime consults it when a trapped callee resumes, to tell
///   an IR SITE TRAP apart from genuinely unreachable code. The two want
///   opposite actions: unreachable code should blacklist the method, whereas a
///   site trap means only that the OPTIMIZING tier could not lower one call
///   site -- the single-pass backend lowers `invokedynamic` and unresolved
///   typechecks perfectly well, and blacklisting takes that body away too.
/// * `decided` — methods whose site-trap policy has been applied; see
///   [`crate::JitVerdictRegistry::claim_site_trap_decision`].
/// * `any_registered` — set once any method is registered, so the common case
///   (a VM that has planted no site trap at all) costs one relaxed load
///   instead of a hash and a lock acquisition. `try_resume_trapped_callee`
///   runs on EVERY deopt resume, not only trapped ones.
///
/// Interpreter round i1 wave 23, lane L5
/// (`i22-L5-ir-site-trap-decisions-are-process-wide-by-method-name`): until
/// then these were three PROCESS statics keyed by the method's name hash
/// alone, so a second VM's, a second loader's or a redefined copy of a method
/// found the decision already taken and its trapping body was never evicted.
/// The memo is now per VM, both keys carry the declaring class's identity
/// (unique within one VM), and the decision key also the VM's redefine epoch.
#[derive(Default)]
pub(crate) struct SiteTrapMemo {
    pub(in crate::ir) any_registered: std::sync::atomic::AtomicBool,
    pub(in crate::ir) trapped: parking_lot::RwLock<rustc_hash::FxHashSet<u64>>,
    pub(in crate::ir) decided: parking_lot::RwLock<rustc_hash::FxHashSet<u64>>,
    /// JIT round 19 proposal wave 7 (lane interp7, EAGER3-19-1 / OSRENTRY6-1):
    /// `(method, class, bci)` keys ([`site_trap_bci_key`]) of every site trap
    /// an optimizing body of the method planted. Never unregistered: the
    /// method-entry body and every OSR body of a method share the per-method
    /// key, and an older trapping body may still be running, so a per-bci key
    /// is what lets a recompiled trap-free body stop inheriting the policy
    /// for an unrelated bci without withdrawing a live body's genuine trap.
    /// Read by the trap sink only under `CRATONVM_JIT_SITE_TRAP_PER_BCI`.
    pub(in crate::ir) trapped_at: parking_lot::RwLock<rustc_hash::FxHashSet<u64>>,
    /// t1 wave 7, lane NBB (LAMBDATRAP21-2): repeats per decision key (the
    /// key [`crate::JitVerdictRegistry::claim_site_trap_decision`] claims),
    /// for the `CRATONVM_DBG_DEOPT` census line only
    /// ([`crate::JitVerdictRegistry::note_site_trap_repeat_of`]). Bounded by
    /// [`MAX_TRAPPED_METHOD_MEMOS`].
    pub(in crate::ir) repeats: parking_lot::Mutex<rustc_hash::FxHashMap<u64, u32>>,
}

/// Cap of [`SiteTrapMemo::trapped_at`] (a few traps per trapped method).
pub(in crate::ir) const MAX_TRAPPED_SITE_MEMOS: usize = 4 * MAX_TRAPPED_METHOD_MEMOS;

/// The [`SiteTrapMemo::trapped_at`] key: [`site_trap_method_key`] mixed with
/// the planted trap's bytecode pc.
pub(in crate::ir) fn site_trap_bci_key(ir_method_hash: u64, class_id: cratonvm_types::ClassId, bci: u32) -> u64 {
    use std::hash::Hasher;
    let mut h = rustc_hash::FxHasher::default();
    h.write_u64(site_trap_method_key(ir_method_hash, class_id));
    h.write_u32(bci);
    h.finish()
}

/// Cap, for the same reason the refusal memo has one: a pathological program
/// must not turn a diagnostic into unbounded retained memory.
pub(in crate::ir) const MAX_TRAPPED_METHOD_MEMOS: usize = 8192;

/// The `trapped` key of a method: its [`crate::ir_method_memo_hash`] mixed
/// with its declaring class's identity. No redefine epoch: the runtime asks
/// about frames of code that is already installed, which may have been
/// compiled before the VM's last redefinition. A redefined version whose own
/// body planted no trap therefore still reads "trapped"; that costs the new
/// version its optimizing tier (the site-trap policy), never a wrong answer,
/// and is the direction that keeps an old body's trap from blacklisting the
/// method on every tier.
pub(in crate::ir) fn site_trap_method_key(ir_method_hash: u64, class_id: cratonvm_types::ClassId) -> u64 {
    use std::hash::Hasher;
    let mut h = rustc_hash::FxHasher::default();
    h.write_u64(ir_method_hash);
    h.write_u32(class_id.as_u32());
    h.finish()
}

impl crate::JitVerdictRegistry {
    /// Has any method of this registry's VM been registered as carrying a
    /// site trap?
    ///
    /// A `false` here is authoritative: the flag is set BEFORE the set insert,
    /// and only ever goes false -> true, so a reader that sees `false` cannot
    /// be racing a registration whose method it is about to be asked about --
    /// the registering compile has not published its artifact yet.
    pub fn any_site_trap_registered(&self) -> bool {
        self.site_traps
            .any_registered
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Record that the optimizing body of the method `ir_method_hash`
    /// ([`crate::ir_method_memo_hash`]) of the loaded class `class_id` carries
    /// a planted site trap. Called by the compile door.
    pub(crate) fn register_site_trap_method(
        &self,
        ir_method_hash: u64,
        class_id: cratonvm_types::ClassId,
    ) {
        self.site_traps
            .any_registered
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut set = self.site_traps.trapped.write();
        if set.len() < MAX_TRAPPED_METHOD_MEMOS {
            set.insert(site_trap_method_key(ir_method_hash, class_id));
        }
    }

    /// Does an optimizing body of this method of the loaded class `class_id`
    /// carry a planted site trap?
    pub fn method_has_site_trap(
        &self,
        ir_method_hash: u64,
        class_id: cratonvm_types::ClassId,
    ) -> bool {
        self.site_traps
            .trapped
            .read()
            .contains(&site_trap_method_key(ir_method_hash, class_id))
    }

    /// EAGER3-19-1 / OSRENTRY6-1 (JIT round 19 proposal wave 7, lane
    /// interp7): record that an optimizing body of the method planted a site
    /// trap at each of `bcis`. Called by the compile door beside
    /// [`Self::register_site_trap_method`], with the build's planted pcs
    /// ([`site_trap_pcs_planted_this_build`]). Data only: the trap sink reads
    /// it under `CRATONVM_JIT_SITE_TRAP_PER_BCI` (and for its diagnostic).
    pub(crate) fn register_site_trap_at(
        &self,
        ir_method_hash: u64,
        class_id: cratonvm_types::ClassId,
        bcis: &[u32],
    ) {
        if bcis.is_empty() {
            return;
        }
        let mut set = self.site_traps.trapped_at.write();
        for &bci in bcis {
            if set.len() >= MAX_TRAPPED_SITE_MEMOS {
                break;
            }
            set.insert(site_trap_bci_key(ir_method_hash, class_id, bci));
        }
    }

    /// Did an optimizing body of this method of the loaded class `class_id`
    /// plant a site trap at bytecode `bci` ([`Self::register_site_trap_at`])?
    pub fn method_has_site_trap_at(
        &self,
        ir_method_hash: u64,
        class_id: cratonvm_types::ClassId,
        bci: u32,
    ) -> bool {
        self.site_traps
            .trapped_at
            .read()
            .contains(&site_trap_bci_key(ir_method_hash, class_id, bci))
    }
}

/// Site traps actually TAKEN at runtime.
///
/// `ir_trap_census` counts what was planted; this is the other half the
/// planting doc promised and did not have. A cause whose taken count is not
/// ~0 has had its coldness argument refuted -- which is exactly what happened
/// to the unresolved-class cause, and the number that says so.
pub(in crate::ir) static SITE_TRAPS_TAKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Count one site trap taken at runtime.
pub fn note_site_trap_taken() {
    SITE_TRAPS_TAKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Methods whose site-trap policy has been applied (`SiteTrapMemo::decided`).
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
///
/// Keyed by `(method, declaring class id, redefine epoch)` in the VM's own
/// registry, so another VM's copy, another loader's copy and a redefined
/// version of the method each get their own decision (HotSpot keeps trap
/// history in each `Method*`'s `MethodData`, which is per loader and per
/// class version).
impl crate::JitVerdictRegistry {
    /// Claim the site-trap policy decision for the method `ir_method_hash`
    /// ([`crate::ir_method_memo_hash`]) of the loaded class `class_id`, under
    /// this registry's current redefine epoch.
    ///
    /// `true` exactly once per method and class version: the caller that gets
    /// it must apply the policy, and every later trap on that method is a
    /// repeat. Insert-and-test under one write lock so two threads trapping the
    /// same method concurrently cannot both decide.
    pub fn claim_site_trap_decision(
        &self,
        ir_method_hash: u64,
        class_id: cratonvm_types::ClassId,
    ) -> bool {
        let key = crate::ir_refusal_memo_key(ir_method_hash, class_id, self.redefine_epoch.load());
        let mut set = self.site_traps.decided.write();
        if set.len() >= MAX_TRAPPED_METHOD_MEMOS {
            // Cap reached: decide every time rather than never. Re-deciding is
            // a recompile request; NOT deciding leaves a trapping artifact
            // installed.
            return true;
        }
        set.insert(key)
    }

    /// t1 wave 7, lane NBB (LAMBDATRAP21-2): count one repeat of an
    /// already-claimed site-trap decision for this method (same key as
    /// [`Self::claim_site_trap_decision`]) and return its ordinal (1-based),
    /// or `None` once the map is at its bound and the method is new to it.
    /// Diagnostics only: the caller prints a `CRATONVM_DBG_DEOPT` line on the
    /// 1st, 16th, 256th, ... repeat ([`site_trap_repeat_is_reported`]).
    pub fn note_site_trap_repeat_of(
        &self,
        ir_method_hash: u64,
        class_id: cratonvm_types::ClassId,
    ) -> Option<u32> {
        let key = crate::ir_refusal_memo_key(ir_method_hash, class_id, self.redefine_epoch.load());
        let mut map = self.site_traps.repeats.lock();
        if let Some(n) = map.get_mut(&key) {
            *n = n.saturating_add(1);
            return Some(*n);
        }
        if map.len() >= MAX_TRAPPED_METHOD_MEMOS {
            return None;
        }
        map.insert(key, 1);
        Some(1)
    }
}

/// t1 wave 7, lane NBB (LAMBDATRAP21-2): is repeat number `n` of one method's
/// site-trap decision one the census reports -- the 1st and every power of 16
/// (16, 256, 4096, ...)?
pub fn site_trap_repeat_is_reported(n: u32) -> bool {
    n == 1 || (n.is_power_of_two() && n.trailing_zeros() % 4 == 0)
}

/// Site traps taken AFTER the policy for that method was already decided.
///
/// These cost an interpreter resume each and buy nothing: the method is already
/// IR-banned and recompiled. A large number means a trapped site sits inside a
/// long-running caller whose in-flight frame still holds a baked CALL to the
/// old body -- see `try_resume_trapped_callee`.
pub(in crate::ir) static SITE_TRAP_REPEATS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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

