#[allow(unused_imports)]
use super::*;

impl IrBuilder {
    /// JVMS §6.5 `ireturn` narrowing of `val` to `tag`
    /// ([`crate::narrowed_int_return_tag`]), built from exactly the nodes the
    /// `iand` (`Z`, `x & 1`), `i2b`, `i2c` and `i2s` arms build — no new op, no
    /// new lowering, and the fold passes see a shape they already know.
    pub(in crate::ir) fn narrow_int_return(&mut self, val: NodeId, tag: Option<u8>, pc: usize) -> NodeId {
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

    pub(in crate::ir) fn iconst(&mut self, val: i64) -> NodeId {
        // Check if we already have this constant (simple dedup).
        //
        // The type is part of the identity: `aconst_null` also materialises
        // `Op::Const(0)`, typed `Ref` so the safepoint maps publish its slot.
        // Deduping on the opcode alone would hand that reference-typed node
        // back for every later `iconst(0)` — an integer comparison operand
        // would then be published as a live oop, and `Op::Cmp` would widen to
        // a 64-bit compare on it.
        //
        // t1 wave 2 (lane Y): keyed on the sign-extended value `Graph::add`
        // stores, so an unsigned spelling of an interned `int` finds it.
        // Cast: an `int` constant is its low 32 bits.
        let val = i64::from(val as i32);
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
    pub(in crate::ir) fn interned_const(&mut self, val: i64, ty: IrType) -> Option<NodeId> {
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
    pub(in crate::ir) fn aconst_null(&mut self) -> NodeId {
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
    pub(in crate::ir) fn lconst(&mut self, val: i64) -> NodeId {
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

    /// Mark this build as the optimizing OSR door's (JIT round 23 wave 2b,
    /// lane L1). See [`Self::osr_door_traps_in_place`].
    pub fn set_osr_door_traps_in_place(&mut self, on: bool) {
        self.osr_door_traps_in_place = on;
    }

    /// Admit unresolved own-code type-check traps that only an in-place resume
    /// serves (JIT round 23 wave 4; see [`Self::typecheck_traps_resume_in_place`]
    /// and [`ir_unresolved_typecheck_in_place_enabled`]). The caller vouches
    /// that the compile's deopt sinks resume such a trap from its frame state:
    /// `lib.rs` sets it only for a method that is not `synchronized`, holds no
    /// monitor, is no lambda implementation body (whose helper sink can only
    /// replay), with `CRATONVM_JIT_DEOPT_SINK_RESUME` on.
    pub fn set_typecheck_traps_resume_in_place(&mut self, on: bool) {
        self.typecheck_traps_resume_in_place = on;
    }

    /// Is the trap at `pc` (cause `cause`) one that
    /// [`Self::set_typecheck_traps_resume_in_place`] admits without the
    /// replay rule and without the default-off unresolved-class gate? Only a
    /// `checkcast` / `instanceof` of a class that is not loaded, in the
    /// compiling method's own code (no splice open, `pc` inside its own body).
    ///
    /// Why the unresolved-class gate's objection does not apply
    /// ([`ir_unresolved_class_trap_enabled`]: "the trap fires on every
    /// execution, forever" once the class loads): such a trap is a SITE trap
    /// (listed in `Graph::site_trap_pcs`), and the VM's site-trap policy
    /// (`helpers::despeculate_trapped_method`) evicts the body, makes it not
    /// entrant, de-specs that bci and recompiles on the first firing. By then
    /// the interpreter has loaded the class.
    pub(in crate::ir) fn typecheck_trap_resumes_in_place(
        &self,
        code: &[u8],
        code_len: usize,
        pc: usize,
        cause: TrapCause,
    ) -> bool {
        self.typecheck_traps_resume_in_place
            // JIT round 23 wave 8 (lane W8-L1,
            // `CRATONVM_JIT_IR_UNRESOLVED_FIELD_IN_PLACE`): an admitted
            // unresolved-field site trap (`plant_field_site_trap`) is resumed
            // in place by the same sinks, under the same frame rules.
            && match cause {
                TrapCause::UnresolvedTypeCheck => ir_unresolved_typecheck_in_place_enabled(),
                TrapCause::UnresolvedField => ir_unresolved_field_in_place_enabled(),
                _ => false,
            }
            && self.splice.is_empty()
            && pc < code_len.min(code.len())
            && self.frame_resumes_in_place_cleanly()
    }

    /// Wave 5 fix (page `r23-w5-typecheck-in-place-trap-resumes-a-null-as-int`):
    /// may the frame at the current pc be rebuilt in the interpreter from its
    /// snapshot without a value the rebuild mistypes? Refused when any operand
    /// stack or local entry is the `Ref` null constant (the snapshot carries it
    /// as an untyped zero, and the interpreter received `Int(0)` where
    /// `checkcast` needs a reference -- `ClassLoader.<init>` pc 26 on the
    /// proxy row) or an `Op::New` (an object whose constructor may not have run
    /// yet, which this tier may also have deferred). Either keeps the default
    /// refusal, and so the single-pass body.
    pub(in crate::ir) fn frame_resumes_in_place_cleanly(&self) -> bool {
        let check_null = !crate::ir_lower::deopt_null_const_as_object();
        let suspect = |&id: &NodeId| {
            id != NO_NODE
                // Wave 8b: a dead in-place-trap placeholder (see
                // `in_place_trap_placeholders`) puts no reachable frame here.
                && !self.in_place_trap_placeholders.contains(&id)
                && self
                    .graph
                    .nodes
                    .get(id as usize)
                    .is_some_and(|n| {
                        (check_null && is_null_const(n))
                            || (matches!(n.op, Op::New { .. })
                                && self.uninitialized_news.contains(&id))
                    })
        };
        !self.stack.iter().any(suspect) && !self.locals.iter().any(suspect)
    }

    /// Wave 5 (`CRATONVM_JIT_IR_NULL_TYPECHECK_NO_RESOLVE`): is the operand of
    /// the `checkcast` / `instanceof` at the top of the stack the `Ref` null
    /// constant? Then JVMS 6.5 answers without resolving the class: `checkcast`
    /// leaves the null, `instanceof` pushes 0 ("If objectref is null ...
    /// Otherwise, the named class is resolved"), so an unloaded class needs no
    /// trap at all.
    pub(in crate::ir) fn top_is_null_const_operand(&self) -> bool {
        ir_null_typecheck_no_resolve_enabled()
            && self
                .peek_opt()
                .and_then(|id| self.graph.nodes.get(id as usize))
                .is_some_and(is_null_const)
    }

    /// May a site trap / class-init guard be planted at `pc` as far as its
    /// RESUMPTION is concerned? Either the method may be replayed from entry
    /// ([`Self::trap_replay_is_safe`], every build's rule), or (JIT round 23
    /// wave 2b, lane L1; `CRATONVM_JIT_IR_OSR_DOOR_TRAPS_IN_PLACE`) this is
    /// the optimizing OSR door's build ([`Self::osr_door_traps_in_place`]),
    /// the trap is in this method's own code with no splice open, and `pc`
    /// lies on no loop: outside every textual back-edge range
    /// ([`osr_door_trap_pc_off_every_loop`]). The door resumes such a trap in
    /// the live frame at the trap's own bci, and it fires at most once per
    /// activation, so it costs one exit, where its refusal cost the loop its
    /// optimizing body for the activation's life (the churn page's J1: the
    /// concat after the churn loop). A trap ON a loop keeps the replay rule:
    /// it would fire every iteration, each a door exit and an interpreted
    /// iteration, where the single-pass body it bails to runs the site
    /// compiled.
    pub(in crate::ir) fn trap_resumable_here(&self, code: &[u8], code_len: usize, pc: usize) -> bool {
        if self.osr_door_traps_in_place
            && self.splice.is_empty()
            && pc < code_len.min(code.len())
            && osr_door_trap_pc_off_every_loop(code, code_len, pc)
        {
            return true;
        }
        self.trap_replay_is_safe(code, code_len, pc)
    }

    /// Would the interpreter accept a whole-method replay for a trap at `pc`?
    ///
    /// This is [`replay_from_entry_is_observably_equivalent`]'s rule, asked at
    /// the producing end. (JIT round 22 wave 2: the VM side has since become
    /// `deopt::replay_from_entry_commits_nothing_with_handlers` -- a
    /// loop-closed extent and a first-arrival walk -- so the clauses below are
    /// a stricter paraphrase, and the shared function itself is now asked too;
    /// see the end of the body.) The whole point is that a trap this predicate
    /// admits is a trap the consumer will not refuse:
    ///
    ///  1. a body that commits nothing anywhere replays trivially;
    ///  2. otherwise every spliced body must also commit nothing, since the
    ///     caller's bytecode does not describe what a relocated body did;
    ///  3. and then only `code[..pc]` can have committed anything, because
    ///     every deopt point this VM emits is `REEXECUTE` — the bytecode AT
    ///     the trap had not completed and nothing after it ran. That last
    ///     clause is false inside a loop, so since round 12 wave 2 the trap
    ///     must ALSO pass [`commits_before_first_arrival`]: an earlier
    ///     iteration may have run code textually after the trap
    ///     (`r12w2-irbuild-replay-prefix-ignores-loops`). This end is then
    ///     stricter than the VM's predicate, which is the safe direction: a
    ///     trap it declines is a method the single-pass tier compiles instead.
    ///
    /// `code` may be the COMBINED buffer (method body, then the spliced
    /// bodies appended after it), so `code_len` — the original method's own
    /// length — is what bounds clause 1. A trap inside a spliced region never
    /// reaches this point: `plant_uncommon_trap` raises `splice_guard_seen`
    /// for one, which refuses the graph outright.
    pub(in crate::ir) fn trap_replay_is_safe(&self, code: &[u8], code_len: usize, pc: usize) -> bool {
        let body_len = code_len.min(code.len());
        if !crate::bytecode_commits_side_effect(code, body_len) {
            return true;
        }
        // JIT r22 wave 8 (lane inline,
        // `CRATONVM_JIT_TRAP_REPLAY_OWN_SITE_IGNORES_SPLICES`, default ON): an
        // impure spliced body matters only if one can have run before the
        // trap. A trap planted in this method's OWN code (no splice open) at a
        // non-invoke pc is a first-arrival point whose first-arrival walk,
        // asked below, counts every invoke -- every splice site -- as a
        // commit, so a trap it admits ran no spliced body. The VM sink asks
        // the same predicate (`deopt::replay_own_site_ignores_splice_purity`).
        // Wave 8 review: and the pc itself is no splice site -- not an
        // invoke row with an inline plan, not an `invokedynamic` planned as a
        // plain (and so spliceable) call of its constant target
        // (`indy_direct_sites`). The trap this admits is the site's whole
        // translation, so no body is in progress at `pc`.
        if !self.spliced_bodies_pure
            && !(self.splice.is_empty()
                && !self.inline_sites.contains_key(&pc)
                && !self.indy_direct_sites.contains_key(&pc)
                && crate::deopt::replay_own_site_ignores_splice_purity(
                    code,
                    body_len,
                    u32::try_from(pc).unwrap_or(u32::MAX),
                ))
        {
            return false;
        }
        if pc > body_len {
            return false;
        }
        // Clause 3 read literally ("only `code[..pc]` can have committed
        // anything") is false inside a loop: earlier iterations ran the body
        // PAST the trap too. So BOTH questions, deliberately. The textual
        // prefix is what the VM's
        // `replay_from_entry_is_observably_equivalent` still asks, and a trap
        // it would refuse to replay must not be planted; the first-arrival
        // walk is what makes the answer sound inside a loop.
        //
        // JIT round 22 wave 2 (lane throws, THROWS22-3;
        // `CRATONVM_JIT_IR_TRAP_REPLAY_ASKS_SHARED_RULE`, correctness, default
        // ON): AND the consumer's own function, asked exactly as the VM's frame
        // sink asks it for a site trap (`first_arrival = true`, no local-handler
        // edges: an optimizing body has none). The VM no longer asks the
        // textual prefix; it asks `deopt::replay_from_entry_commits_nothing_with_handlers`,
        // which reads its OWN kill switches. With the defaults that answer is
        // implied by the two clauses above (its first-arrival walk is bounded by
        // the loop-closed extent, this one by the body), so nothing changes.
        // Under `CRATONVM_JIT_DEOPT_REPLAY_FIRST_ARRIVAL=0`, though, the VM
        // judges every point by the loop-closed extent while this end still
        // admitted a trap by its first-arrival walk: a trap at the top of a loop
        // whose body commits after it was planted and then refused at the sink,
        // a hard `InternalError: precise deoptimization unavailable` -- a kill
        // switch documented as "stricter" that crashed instead.
        !crate::bytecode_commits_side_effect(code, pc)
            && !(ir_trap_replay_first_arrival_enabled()
                && commits_before_first_arrival(code, body_len, pc))
            && (crate::deopt::replay_from_entry_commits_nothing_with_handlers(
                    code,
                    body_len,
                    self.spliced_bodies_pure,
                    u32::try_from(pc).unwrap_or(u32::MAX),
                    true,
                    &[],
                ))
            // JIT round 22 wave 10 (lane throws,
            // `CRATONVM_JIT_IR_TRAP_REPLAY_FOLLOWS_HANDLERS`, default ON): the
            // walks above do not follow exception edges, but this body may take
            // them in-frame (the local throw edges, the implicit / call-site
            // edges and compiled handler entries read exactly these rows). Walk
            // again through them; see `commits_before_first_arrival_with_handlers`.
            && !(ir_trap_replay_first_arrival_enabled()
                && (!self.local_throw_rows.is_empty() || !self.catch_arm_rows.is_empty())
                && commits_before_first_arrival_with_handlers(
                    code,
                    body_len,
                    pc,
                    &self.in_frame_exception_rows(),
                ))
    }

    /// JIT round 22 wave 10 (lane throws): the exception-table rows this body
    /// may follow INSIDE the frame, as `(start, end, handler)` method pcs:
    /// [`Self::local_throw_rows`] (local throw, implicit and call-site edges)
    /// and [`Self::catch_arm_rows`] (compiled handler entries).
    pub(in crate::ir) fn in_frame_exception_rows(&self) -> Vec<(usize, usize, usize)> {
        self.local_throw_rows
            .iter()
            .map(|r| (r.start_pc, r.end_pc, r.handler_pc))
            .chain(
                self.catch_arm_rows
                    .iter()
                    .map(|r| (r.start_pc, r.end_pc, r.handler_pc)),
            )
            .collect()
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
    pub(in crate::ir) fn plant_uncommon_trap(
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
        // JIT round 23 wave 4: an own-code unresolved type check this build
        // resumes in place skips both the default-off gate below and the
        // replay guard further down (see `typecheck_trap_resumes_in_place`).
        let in_place = self.typecheck_trap_resumes_in_place(code, code_len, pc, cause);
        if matches!(
            cause,
            TrapCause::UnresolvedTypeCheck | TrapCause::UnresolvedNew
        ) && !ir_unresolved_class_trap_enabled()
            && !in_place
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
        if ir_trap_replay_guard_enabled()
            && !in_place
            && !self.trap_resumable_here(code, code_len, pc)
        {
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
            self.splice_guard_pc.get_or_insert(pc);
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
        if !self.has_frame_state_at(pc) {
            return false;
        }
        let zero = self.iconst(0);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, zero],
            Some(pc),
        );
        // Round 12 wave 3 (lane replay): the lowerer stamps this guard's point
        // `first_arrival`, which is what lets the VM's replay rule be exact
        // for it (`deopt::replay_from_entry_commits_nothing`). Only a pc of
        // the compiling method's own code: a trap inside a splice has already
        // raised `splice_guard_seen`, which refuses the graph.
        if pc < code_len && !self.graph.site_trap_pcs.contains(&pc) {
            self.graph.site_trap_pcs.push(pc);
        }
        // JIT round 19 wave 3, lane eager3 (EAGER2-4): the cause, by the same
        // pc, for the lowerer to record beside the point.
        if pc < code_len {
            self.graph.note_trap_cause(pc, cause);
        }
        note_trap_planted(cause);
        note_site_trap_planted_this_build(pc);
        // Wave 8b: read by `trap_placeholder_ref` for this site's placeholder.
        self.last_trap_in_place = in_place;
        if ir_bail_reporting() {
            eprintln!(
                "[ir] site TRAP planted at bytecode pc {pc} ({}) in {} -- the rest of the method still compiles",
                cause.as_str(),
                self.method_label.as_deref().unwrap_or("<unknown>"),
            );
        }
        true
    }

    /// The `Ref` placeholder a trap arm pushes for the value its site would
    /// have produced (JIT round 23 wave 8b, lane W8-L1;
    /// `CRATONVM_JIT_IR_IN_PLACE_TRAP_DEAD_PLACEHOLDER`, default ON, `=0`
    /// kill). Called right after a successful [`Self::plant_uncommon_trap`].
    ///
    /// After an IN-PLACE trap it is a fresh, never interned `Ref` zero,
    /// recorded in [`Self::in_place_trap_placeholders`]: it is defined only on
    /// the path after an always-failing guard, so a later frame that holds it
    /// directly (not through a phi with a live value) is unreachable, and a
    /// second in-place trap there need not refuse it as the null constant
    /// `frame_resumes_in_place_cleanly` rejects. `Pattern.compile()V` is the
    /// shape: the pc-486 `checkcast Pattern$Loop` trap's placeholder is still
    /// on the stack at the pc-500 `putfield Pattern$Loop.posIndex`, whose
    /// unresolved-field trap was refused for it. Every other trap keeps the
    /// interned null ([`Self::aconst_null`]), as before.
    pub(in crate::ir) fn trap_placeholder_ref(&mut self) -> NodeId {
        if self.last_trap_in_place && ir_in_place_trap_dead_placeholder_enabled() {
            let id = self.graph.add(Op::Const(0), IrType::Ref, vec![], None);
            self.in_place_trap_placeholders.insert(id);
            id
        } else {
            self.aconst_null()
        }
    }

    /// JIT round 19 wave 3, lane eager3 (EAGER2-3 of
    /// `jit-r19-eager2-proposals.md`, `CRATONVM_JIT_UNINIT_NEW_GUARD`): plant a
    /// CONDITIONAL guard at the `new` at `pc` whose class is loaded but not
    /// initialized: `Op::Guard { bci: pc }` on [`Op::ClassInitTest`] of the
    /// class's init word. The failing edge is the ordinary guard deopt
    /// (`UncommonTrap` / `Reinterpret`, REEXECUTE at the `new`), whose
    /// interpreter run initializes the class; the VM's deopt service treats it
    /// as the EAGER19-1 trap (one-shot site budget, late-bind recompile). The
    /// passing edge is the caller's resolved `Op::New`, so a body compiled
    /// before the class initialized keeps running once anyone initializes it,
    /// where EAGER19-1's unconditional trap evicts it at the first execution.
    ///
    /// Held to `plant_uncommon_trap`'s rules, so this guard is never admitted
    /// where the trap would not have been: a frame state at `pc`, an open
    /// control, and (under `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD`, default ON) a
    /// replayable trap. Not listed in `Graph::site_trap_pcs` (it is not a site
    /// trap: it does not fire on every arrival, so neither the site-trap policy
    /// nor `first_arrival` applies); its cause is recorded in
    /// `Graph::trap_causes`. Returns `false`, having added nothing, when
    /// refused.
    pub(in crate::ir) fn plant_class_init_guard(
        &mut self,
        code: &[u8],
        code_len: usize,
        pc: usize,
        state_addr: usize,
        initialized: u8,
    ) -> bool {
        if state_addr == 0 || pc >= code_len {
            return false;
        }
        if ir_trap_replay_guard_enabled() && !self.trap_resumable_here(code, code_len, pc) {
            if ir_bail_reporting() {
                eprintln!(
                    "[ir] class-init GUARD REFUSED at bytecode pc {pc} in {} -- the code before it commits a side effect",
                    self.method_label.as_deref().unwrap_or("<unknown>"),
                );
            }
            return false;
        }
        let Some(ctrl) = self.ctrl_opt() else {
            return false;
        };
        if !self.has_frame_state_at(pc) {
            return false;
        }
        let test = self.graph.add(
            Op::ClassInitTest {
                state_addr,
                initialized,
            },
            IrType::Int,
            vec![ctrl],
            Some(pc),
        );
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, test],
            Some(pc),
        );
        self.graph.note_trap_cause(pc, TrapCause::UninitNew);
        if ir_bail_reporting() {
            eprintln!(
                "[ir] class-init GUARD planted at bytecode pc {pc} in {} -- the `new` runs once its class is initialized",
                self.method_label.as_deref().unwrap_or("<unknown>"),
            );
        }
        true
    }

    /// JIT round 16 wave 10 (lane indyir, IN16-8;
    /// `CRATONVM_JIT_INDY_CONSTANT_INVOKE`): the opcode whose arm builds the
    /// `invokedynamic` at `pc` -- the planned invoke (`0xb8` / `0xb6`) for a
    /// constant call site the door resolved to a plain call of its target,
    /// `0xba` (the bridge or the trap, exactly as before) for every other.
    ///
    /// The invoke arm then builds the site as the call it is for the rest of
    /// the caller's life: a direct bind, an inline cache, or a splice of the
    /// target's body. The arm advances by the instruction's own length (five),
    /// and every by-bci consumer of the call already knows an
    /// `invokedynamic` is a five-byte invoke (`caller_resume_pc`, the
    /// post-call exit's `invoke_len`).
    ///
    /// Terms, each failing closed to `0xba`:
    ///
    /// * a planned site, not inside a splice (the plan is keyed by the
    ///   compiling method's own pcs);
    /// * an `invoke_info` row for it (the plan survived: without one the
    ///   invoke arm would refuse the METHOD where the bridge serves the site);
    /// * for a virtual target, a receiver this build proves non-null or a
    ///   null guard planted at `pc` ([`Self::indy_direct_receiver_guard`]): a
    ///   null receiver must reach the interpreter's `invokedynamic`, whose
    ///   `MethodHandle` raises the NPE, not a plain call's.
    pub(in crate::ir) fn indy_direct_opcode(&mut self, pc: usize) -> u8 {
        const INVOKEDYNAMIC: u8 = 0xba;
        let Some(&op) = self.indy_direct_sites.get(&pc) else {
            return INVOKEDYNAMIC;
        };
        if !self.splice.is_empty() || !matches!(op, 0xb6 | 0xb8) {
            return INVOKEDYNAMIC;
        }
        let Some(&(_, num_args, _)) = self.invoke_info.get(&pc) else {
            return INVOKEDYNAMIC;
        };
        if op == 0xb6 && !self.indy_direct_receiver_guard(pc, num_args) {
            return INVOKEDYNAMIC;
        }
        // JIT round 21 (lane indylate): the bridge call the single-pass body
        // keeps, replaced by the target's plain call -- acceptance evidence,
        // as the `0xba` arm's three expansions are (lane accept). Without it a
        // body whose target is not spliced is discarded as "no evidence" and
        // the site stays on the bridge.
        crate::ir_evidence::note_builder_expansion();
        if ir_bail_reporting() {
            eprintln!(
                "[ir] indy CONSTANT at bytecode pc {pc} in {} -- built as opcode {op:#04x}, \
                 a plain call of the call site's target",
                self.method_label.as_deref().unwrap_or("<unknown>"),
            );
        }
        op
    }

    /// JIT round 16 wave 17 (lane mh7; MH16-8 at compile time,
    /// `CRATONVM_JIT_MH_CONSTANT_COMPILE`): the `invokevirtual
    /// MethodHandle.invokeExact` / `invoke` planned at `pc` as an
    /// `invokestatic` of its constant handle's member is built by the
    /// `invokestatic` arm (`0xb8`), with the handle dropped from the abstract
    /// stack first. `None` -- the method is refused -- when the plan does not
    /// hold here: the row the arm needs (the plan replaced the site's own) or
    /// the receiver is not the planned field's value.
    ///
    /// The receiver must be an `Op::LoadStatic` of the very field whose
    /// `getstatic` the door found feeding the call (its own node, or an
    /// earlier read of the same field `static_load_memo_forward` handed on):
    /// the field is a `static final` of an initialized class, so its value is
    /// the handle the door admitted.
    ///
    /// Frame states stay the interpreter's: the snapshot at `pc` was pushed at
    /// the top of the instruction, with the handle below the arguments, so a
    /// deopt there re-executes the `invokevirtual` (through the handle's
    /// door), and every later snapshot is taken after the call, with the
    /// handle and the arguments consumed and the result pushed, as the
    /// `invokevirtual` leaves them. The post-call exit, which derives its
    /// state from the row, refuses such a site (`ir_lower`'s
    /// `post_call_exit_state`, through [`Graph::mh_constant_call_pcs`]). Never
    /// inside a splice: the plan names the compiling method's own pcs.
    ///
    /// JIT round 17 wave 3 (lane mh4; MHCONST17-2): a row the planner baked
    /// as an instance call (`invoke_kind` `0`, `1` after a CHA re-mark, or
    /// `2`) is an unbound VIRTUAL handle's member, built by the
    /// `invokevirtual` arm (`0xb6`; the instruction is the three-byte
    /// `invokevirtual` either way) with the receiver the site's first
    /// argument. Its receiver is proven non-null or guarded here with a deopt
    /// at `pc` ([`Self::indy_direct_receiver_guard`]), whose snapshot holds
    /// the handle: a `null` re-executes the `invokeExact`, and the handle's
    /// own door raises its `NullPointerException`, not a plain call's.
    /// Is `node` this method's read of the static field `(class_id,
    /// field_index)`: its `LoadStatic`, or the constant root the static-final
    /// fold built in its place (`ConstFoldTables::static_final_const_refs`)?
    /// The constant handle arms below ask it of the handle on the stack.
    pub(in crate::ir) fn is_static_field_read(&self, node: NodeId, class_id: u32, field_index: u32) -> bool {
        match self.graph.nodes.get(node as usize).map(|n| &n.op) {
            Some(Op::LoadStatic { class_id: c, field_index: f, .. }) => *c == class_id && *f == field_index,
            Some(Op::IndyRootRef { slot_addr }) => self
                .const_folds
                .static_final_const_refs
                .get(&(class_id, field_index))
                .is_some_and(|&(slot, _)| slot != 0 && slot == *slot_addr),
            _ => false,
        }
    }

    pub(in crate::ir) fn mh_constant_opcode(&mut self, pc: usize) -> Option<u8> {
        if !self.splice.is_empty() {
            return None;
        }
        let &getstatic_pc = self.mh_constant_sites.get(&pc)?;
        let &(info_ptr, num_args, _) = self.invoke_info.get(&pc)?;
        if info_ptr == 0 {
            return None;
        }
        // SAFETY: `info_ptr` is the address of the `JitInvokeInfo` the planner
        // boxed for this pc and keeps alive on the artifact (the `Op::Call`
        // this arm builds bakes the same address).
        let invoke_kind = unsafe { (*(info_ptr as *const JitInvokeInfo)).invoke_kind };
        let opcode: u8 = match invoke_kind {
            3 => 0xb8,
            0..=2 => 0xb6,
            _ => return None,
        };
        // JIT round 21 wave 19 (lane mhbind2; MHBIND21-1): a bound handle's
        // site, whose member's receiver is the handle's bound value (a
        // `static final` handle; a condy one is never planned bound).
        if let Some(&slot) = self.mh_constant_bound_sites.get(&pc) {
            let &(class_id, field_index, _, _) = self.static_field_info.get(&getstatic_pc)?;
            let field_index = u32::try_from(field_index).ok()?;
            return self.mh_constant_bound_opcode(
                pc,
                num_args,
                invoke_kind,
                slot,
                class_id,
                field_index,
            );
        }
        // JIT round 21 wave 20 (lane mhfilter; MHBIND2-1): a filter species'
        // site, two plain calls.
        if let Some(&site) = self.mh_constant_filter_sites.get(&pc) {
            let &(class_id, field_index, _, _) = self.static_field_info.get(&getstatic_pc)?;
            let field_index = u32::try_from(field_index).ok()?;
            return self.mh_constant_filter_opcode(
                pc,
                getstatic_pc,
                num_args,
                opcode,
                site,
                (class_id, field_index),
            );
        }
        if let Some(site) = self.mh_constant_convert_sites.get(&pc).cloned() {
            let &(class_id, field_index, _, _) = self.static_field_info.get(&getstatic_pc)?;
            let field_index = u32::try_from(field_index).ok()?;
            return self.mh_constant_convert_opcode(
                pc,
                getstatic_pc,
                num_args,
                opcode,
                site,
                (class_id, field_index),
            );
        }
        let at = self.stack.len().checked_sub(num_args + 1)?;
        let receiver = node_id_opt(self.stack[at])?;
        // JIT r23 wave 4b, lane L2 (L2W4B-1, `CRATONVM_JIT_MH_CONSTANT_CONDY`):
        // the receiver `ldc` of a resolved condy must have been built as the
        // read of that very condy's slot (`ldc_condy_slot_info`); the handle
        // it holds was resolved once and never changes (JVMS 5.4.3.6).
        let is_constant = if let Some(&slot) = self.ldc_condy_slot_info.get(&getstatic_pc) {
            self.graph.nodes.get(receiver as usize).is_some_and(
                |n| matches!(n.op, Op::IndyRootRef { slot_addr } if slot_addr == slot),
            )
        } else {
            let &(class_id, field_index, _, _) = self.static_field_info.get(&getstatic_pc)?;
            let field_index = u32::try_from(field_index).ok()?;
            self.is_static_field_read(receiver, class_id, field_index)
        };
        if !is_constant {
            return None;
        }
        // JIT round 19 wave 1 (lane mhiface; MHRESID18-4): an interface
        // handle's site must have been planned as one (`invoke_kind 2`, the
        // `invokeinterface` row) and the other way round; a mismatch refuses.
        let interface = self.mh_constant_interface_sites.contains(&pc);
        if interface != (invoke_kind == 2) {
            return None;
        }
        self.stack.remove(at);
        if opcode == 0xb6 {
            let guarded = if interface {
                self.mh_constant_interface_receiver_guard(pc, num_args, info_ptr)
            } else {
                self.indy_direct_receiver_guard(pc, num_args)
            };
            if !guarded {
                return None;
            }
        }
        self.graph.mh_constant_call_pcs.push(pc);
        if ir_bail_reporting() {
            eprintln!(
                "[ir] mh CONSTANT at bytecode pc {pc} in {} -- built as opcode {opcode:#04x}, \
                 a plain call of the handle's member",
                self.method_label.as_deref().unwrap_or("<unknown>"),
            );
        }
        Some(opcode)
    }

    /// JIT round 21 (lane lambda2, TLOCAL21-1 / LAMBDATRAP21-3 of
    /// `jit-r21-tlocal-proposals.md` / `jit-proposals/jit-r21-lambdatrap-proposals-RETIRED-20261030.md`;
    /// `CRATONVM_JIT_IR_LAMBDA_IMPL_CALL`, default ON): the `invokeinterface`
    /// SAM call at `pc` whose receiver the door proved (straight-line code,
    /// `ir_lambda_impl_plan` in `lib.rs`) is the value of this method's own
    /// lambda `invokedynamic` is built by the `invokestatic` arm (`0xb8`) as a
    /// call of the lambda's STATIC impl -- what the proxy's SAM method does,
    /// with no adapter (the VM admits only an identity coercion: no
    /// `checkcast`, no box / unbox) -- with the receiver replaced on the
    /// abstract stack by the captured values, in impl-parameter order:
    ///
    /// ```text
    /// before:  .. proxy, a1 .. ak      (the invokeinterface's operands)
    /// after:   .. c1 .. cn, a1 .. ak   (impl(c1 .. cn, a1 .. ak))
    /// ```
    ///
    /// * A CAPTURING site: the receiver must be the very `New` the `0xba` arm
    ///   built at the planned `invokedynamic` ([`Self::try_indy_capture_new`]),
    ///   whose capture values are the ones it stored; the `New` then has no
    ///   use but its stores and the frame states, so escape analysis may drop
    ///   it.
    /// * A NON-CAPTURING site: the proxy is the published singleton (or, if
    ///   the site trapped, this code is dead), so the receiver is dropped.
    ///
    /// Frame states stay the interpreter's, as for a constant handle
    /// ([`Self::mh_constant_opcode`], whose post-call-exit refusal this site
    /// shares through [`Graph::mh_constant_call_pcs`]): the snapshot at `pc`
    /// holds the proxy below the SAM arguments and re-executes the
    /// `invokeinterface`; a spliced impl's caller scope holds the stack below
    /// the proxy, which is what the interpreter's `invokeinterface` leaves.
    ///
    /// `None` -- the method is refused -- when the plan does not hold here:
    /// inside a splice with no impl body appended at `pc`, a row that is not a
    /// static one of the planned arity, or a receiver that is not the planned
    /// site's value.
    /// JIT round 21 wave 19 (lane lambda3fix, LAMBDA321-4): whether `node` is
    /// `target`, or a loop header's LOCAL phi ([`Self::loop_phis`]) every one
    /// of whose value inputs (the region input excluded) is `target` or,
    /// recursively, such a phi -- a value no edge into those headers changes.
    /// Asked at the SAM call (a header's back-edge inputs are appended only
    /// when its back edge is walked, so they are not seen yet) and again at
    /// the end of the build, when every input is there. Any other node (a
    /// forward-join phi, an OSR entry arm's merge, an `Op::OsrLocal`) answers
    /// `false`. Bounded: more than 64 phis answers `false`.
    pub(in crate::ir) fn lambda_receiver_resolves_to(&self, node: NodeId, target: NodeId) -> bool {
        let mut seen: HashSet<NodeId> = HashSet::new();
        let mut work: Vec<NodeId> = vec![node];
        while let Some(n) = work.pop() {
            if n == target || !seen.insert(n) {
                continue;
            }
            if seen.len() > 64 {
                return false;
            }
            if !self.loop_phis.values().any(|lp| lp.local_phis.contains(&n)) {
                return false;
            }
            let Some(phi) = self.graph.nodes.get(n as usize) else {
                return false;
            };
            if !matches!(phi.op, Op::Phi) {
                return false;
            }
            for &input in phi.inputs.as_slice().iter().skip(1) {
                match node_id_opt(input) {
                    Some(v) => work.push(v),
                    None => return false,
                }
            }
        }
        true
    }

    pub(in crate::ir) fn lambda_impl_opcode(&mut self, pc: usize) -> Option<u8> {
        // JIT round 21 wave 15 (lane lambda3, LAMBDACAP21-2): inside a splice
        // only a combined-pc row the planner registered with the impl's body
        // appended at this very pc (`IrInlineTables::lambda_impl_sites`), so
        // the `0xb8` arm splices it: an unspliced impl call has no invoke row
        // here, and would be no better than the inline cache it replaces.
        if !self.splice.is_empty() && !self.inline_sites.contains_key(&pc) {
            return None;
        }
        let &(indy_pc, captures, impl_params) = self.lambda_impl_sites.get(&pc)?;
        // JIT round 21 (lane lambda3): an unspliced impl call on a capturing
        // site measured 3.5x SLOWER than the interface call it replaces
        // (`jr21-tlocal-capture-new-kernel-FIXED-20261014.md`, "Round 21 lane
        // lambda3"): it keeps the proxy's allocation and adds a call the
        // inline cache did not pay. Refused here; the door rebuilds the
        // method with the plan withheld (`ir_tier`).
        if captures > 0
            && self.lambda_impl_captures_need_splice
            && !self.inline_sites.contains_key(&pc)
        {
            if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[ir] lambda IMPL CALL refused at bytecode pc {pc} in {} -- capturing site, \
                     impl not spliced (CRATONVM_JIT_IR_LAMBDA_IMPL_SPLICED_CAPTURES)",
                    self.method_label.as_deref().unwrap_or("<unknown>"),
                );
            }
            return None;
        }
        // The planner builds no row for a site it lowers as arithmetic (a
        // scalar intrinsic), which the `0xb8` arm serves before its row.
        if let Some(&(info_ptr, num_args, _)) = self.invoke_info.get(&pc) {
            if info_ptr == 0 || num_args != impl_params {
                return None;
            }
            // SAFETY: as in `mh_constant_opcode` -- the address of the
            // `JitInvokeInfo` the planner boxed for this pc, kept alive on the
            // artifact.
            if unsafe { (*(info_ptr as *const JitInvokeInfo)).invoke_kind } != 3 {
                return None;
            }
        }
        let sam_args = impl_params.checked_sub(captures)?;
        let at = self.stack.len().checked_sub(sam_args + 1)?;
        let receiver = node_id_opt(self.stack[at])?;
        if self.graph.nodes.get(receiver as usize).map(|n| n.ty) != Some(IrType::Ref) {
            return None;
        }
        let mut built = self
            .indy_capture_new_values
            .get(&indy_pc)
            .filter(|(proxy, values)| *proxy == receiver && values.len() == captures)
            .map(|(_, values)| values.clone());
        // JIT round 21 wave 19 (lane lambda3fix, LAMBDA321-4,
        // `CRATONVM_JIT_IR_LAMBDA_IMPL_LOOP_SLOT`): a lambda made before the
        // loop that applies it reaches the call as the loop header's phi of
        // its local (the door proved the local is written once, outside every
        // loop). Admitted when every phi on the way resolves to the very
        // `New`; the back edges are not patched yet, so the end of the build
        // asks again (`lambda_impl_phi_checks`).
        if built.is_none() && captures > 0 && crate::ir_lambda_impl_loop_slot_enabled() {
            if let Some((proxy, values)) = self.indy_capture_new_values.get(&indy_pc) {
                let proxy = *proxy;
                if values.len() == captures
                    && receiver != proxy
                    && self.lambda_receiver_resolves_to(receiver, proxy)
                {
                    built = Some(values.clone());
                    self.lambda_impl_phi_checks.push((pc, receiver, proxy));
                }
            }
        }
        let values: Vec<NodeId> = if captures == 0 {
            Vec::new()
        } else if let Some(values) = built {
            values
        } else if self.graph.site_trap_pcs.contains(&indy_pc) {
            // The planned `invokedynamic` is an uncommon trap: nothing after
            // it runs, so typed zeros stand in for the captures.
            // JIT round 21 (lane lambda3): not under the spliced-captures
            // switch. The dead impl call would still count as a builder
            // expansion (acceptance evidence) and publish a body that traps
            // at the `invokedynamic` on every call; refused, the door
            // rebuilds without the plan.
            // JIT round 21 wave 15 (LAMBDACAP21-2): nor inside a splice, for
            // the same reason (the door's retry withholds the spliced plan
            // too).
            if self.lambda_impl_captures_need_splice || !self.splice.is_empty() {
                return None;
            }
            let tags = self.indy_capture_news.get(&indy_pc)?.capture_tags.clone();
            if tags.len() != captures {
                return None;
            }
            tags.iter()
                .map(|&tag| match tag {
                    b'J' => self.lconst(0),
                    b'F' => self.fconst(0.0),
                    b'D' => self.dconst(0.0),
                    b'L' => self.aconst_null(),
                    _ => self.iconst(0),
                })
                .collect()
        } else {
            return None;
        };
        self.stack.remove(at);
        for (k, value) in values.into_iter().enumerate() {
            self.stack.insert(at + k, value);
        }
        self.graph.mh_constant_call_pcs.push(pc);
        // JIT round 21 wave 15 (LAMBDACAP21-2): the door's census.
        if !self.splice.is_empty() {
            self.graph
                .lambda_impl_splice_rows
                .push((pc, indy_pc, captures));
        }
        // The interface call (inline cache, lambda thunk) this replaces is the
        // single-pass body's: acceptance evidence.
        crate::ir_evidence::note_builder_expansion();
        if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] lambda IMPL CALL at bytecode pc {pc} in {} -- the SAM call on the \
                 lambda of pc {indy_pc}, {captures} capture(s), built as the impl's invokestatic",
                self.method_label.as_deref().unwrap_or("<unknown>"),
            );
        }
        Some(0xb8)
    }

    /// JIT round 21 wave 19 (lane mhbind2; MHBIND21-1 of
    /// `jit-r21-mhbind-proposals.md`, `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND`
    /// in the VM): [`Self::mh_constant_opcode`] for a site whose `static final`
    /// handle is a REAL `bindTo` result of a virtual member
    /// (`BoundMethodHandle$Species_LL`, `argL0` the direct handle, `argL1` the
    /// bound value). Built by the `invokevirtual` arm (`0xb6`) of the member,
    /// whose descriptor is the site's, with the handle REPLACED on the abstract
    /// stack by a load of its `argL1` field (slot `slot`):
    ///
    /// ```text
    /// before:  .. handle, a1 .. ak   (the invokeExact's operands)
    /// after:   .. handle.argL1, a1 .. ak   (member(a1 .. ak) on argL1)
    /// ```
    ///
    /// What the handle does: its form is `invokeBasic(argL0, argL1, a*)`, and
    /// `invokeBasic` of a plain virtual direct handle is `linkToVirtual(argL1,
    /// a*, member)` -- the member SELECTED from the bound value's class, which
    /// is what an `invokevirtual` does. The VM's admission (the dispatch
    /// helper's fill, `lang_invoke::mh_real_bound_compiled_bind`) found the
    /// handle's type to be the site's, the bound value a non-null non-array
    /// instance of the member's class, and the form the `bindTo` one; every
    /// field read is `final`, and the field is a `static final` of an
    /// initialized class, so the load answers that same value on every call.
    /// The receiver is still null-guarded at `pc` ([`Self::indy_direct_receiver_guard`]):
    /// a deopt there re-executes the `invokeExact` through the handle's door
    /// (the snapshot at `pc` holds the handle). Frame states are the plain
    /// constant site's (see [`Self::mh_constant_opcode`]); the load is listed
    /// as a synthetic field access, so no per-pc compact row describes it.
    pub(in crate::ir) fn mh_constant_bound_opcode(
        &mut self,
        pc: usize,
        num_args: usize,
        invoke_kind: u8,
        slot: u32,
        class_id: u32,
        field_index: u32,
    ) -> Option<u8> {
        // j27 wave 6 (lane T1LAM2): or a STATIC member's (`invoke_kind 6` in
        // the plan), an unguarded `invokestatic` with the bound value first.
        let (opcode, guarded) = crate::ir_mh_bound_static::bound_site_build(
            invoke_kind,
            self.mh_constant_bound_static_sites.contains(&pc),
            num_args,
            self.mh_constant_interface_sites.contains(&pc),
        )?;
        // The handle stands where the member's receiver (a static member's
        // leading argument) goes: the row counts it among its arguments, the
        // site has the handle there.
        let at = self.stack.len().checked_sub(num_args)?;
        let handle = node_id_opt(self.stack[at])?;
        let is_field = self.is_static_field_read(handle, class_id, field_index);
        if !is_field || !self.has_frame_state_at(pc) {
            return None;
        }
        let offset = self.iconst(i64::from(slot));
        let receiver = self.graph.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            vec![self.ctrl, self.mem, handle, offset],
            Some(pc),
        );
        // As the `getfield` arm: the read joins the memory chain.
        self.mem = receiver;
        // Not this pc's own field instruction: the field-index arms serve it.
        self.graph.synthetic_field_access_pcs.insert(pc);
        self.stack[at] = receiver;
        if guarded && !self.indy_direct_receiver_guard(pc, num_args) {
            return None;
        }
        self.graph.mh_constant_call_pcs.push(pc);
        // The dispatch helper's bound route the single-pass body calls,
        // replaced by the member's plain call: acceptance evidence.
        crate::ir_evidence::note_builder_expansion();
        if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] mh CONSTANT BOUND at bytecode pc {pc} in {} -- built as opcode {opcode:#04x}, \
                 the member's plain call on the handle's argL1 (slot {slot})",
                self.method_label.as_deref().unwrap_or("<unknown>"),
            );
        }
        Some(opcode)
    }

    /// JIT round 21 wave 20 (lane mhfilter; MHBIND2-1 of
    /// `jit-r21-mhbind2-proposals.md`, `CRATONVM_JIT_MH_CONSTANT_COMPILE_FILTER`
    /// in the VM): [`Self::mh_constant_opcode`] for a site whose `static final`
    /// handle is a REAL filter species ([`IrMhFilterSite`]). The PRE call is
    /// built here, as a plain `Op::Call` of the row keyed by the site's
    /// `getstatic` pc (its bytecode pc too, so every by-pc table of the
    /// lowering -- direct entry, inline cache, exception exit -- reads the PRE
    /// member's own rows; no exception handler covers that window and no local
    /// is written in it, `lib.rs` `mh_filter_window_admits`). The MAIN call is
    /// then built by the invoke arm of the returned opcode from the site's own
    /// row:
    ///
    /// ```text
    /// filterArguments(target, p, filter):
    ///   before:  .. handle, a0 .. ak
    ///   PRE:     f = filter(a_p)              (invokestatic, at the getstatic pc)
    ///   after:   .. a0 .. f .. ak             (MAIN: target(a0 .. f .. ak))
    /// filterReturnValue(target, filter):
    ///   before:  .. handle, a0 .. ak
    ///   PRE:     r = target(a0 .. ak)         (at the getstatic pc)
    ///   after:   .. r                         (MAIN: filter(r))
    /// ```
    ///
    /// What the handle does (JDK 25 `LambdaFormEditor.filterArgumentForm` /
    /// `filterReturnForm` over the target's reinvoker form): the filter is
    /// `invokeBasic(argL1, a_p)`, evaluated FIRST, then `invokeBasic(argL0,
    /// ..)` of the direct target; or the target first and the filter on its
    /// result. An exception of either call propagates from the `invokeExact`
    /// (no handler covers the site), with only hidden LambdaForm frames
    /// between, as HotSpot reports it.
    ///
    /// Deopts: nothing that resumes in the interpreter may follow the PRE
    /// call, which re-executing the `invokeExact` would run again. So a
    /// virtual MAIN call's receiver that is a site argument (and a virtual
    /// PRE call's receiver) is null-guarded BEFORE it, with the plain
    /// snapshot at `pc`; a FILTERED receiver (`p == 0` of a virtual target)
    /// is null-guarded after it with a frame of its own
    /// ([`Self::mh_constant_filter_receiver_guard`]) that re-executes the
    /// `invokeExact` on the TARGET handle with the filtered value -- the
    /// remaining half of the species' work. Neither call is spliced (the
    /// planner skips both pcs), the MAIN call gets no post-call exit
    /// ([`Graph::mh_constant_call_pcs`]) and the PRE call none either (its pc
    /// is no invoke's). A site this does not fit refuses the method. A
    /// box-returning converting site's member is the exception: a body that
    /// cannot raise, planned at the `getstatic` pc, is spliced and its result
    /// boxed ([`Self::convert_member_splice_admitted`]).
    pub(in crate::ir) fn mh_constant_convert_opcode(
        &mut self,
        pc: usize,
        getstatic_pc: usize,
        num_args: usize,
        opcode: u8,
        site: IrMhConvertSite,
        (class_id, field_index): (u32, u32),
    ) -> Option<u8> {
        if self.mh_constant_interface_sites.contains(&pc)
            || self.mh_constant_bound_sites.contains_key(&pc)
            || self.unloaded_static_call_pcs.contains(&pc)
            || self.first_call_cell_at(pc).is_some()
            || self.inline_sites.contains_key(&pc)
        {
            return None;
        }
        let arity = usize::from(site.arity);
        if arity > 8 {
            return None;
        }
        let member_info = if site.box_return != 0 {
            if site.pre_pc != getstatic_pc
                || opcode != 0xb8
                || num_args != 1
                || self.unloaded_static_call_pcs.contains(&getstatic_pc)
                || self.first_call_cell_at(getstatic_pc).is_some()
            {
                return None;
            }
            let &(pre_info, pre_args, pre_ret) = self.invoke_info.get(&getstatic_pc)?;
            if pre_info == 0 || pre_args != arity || pre_ret == b'V' {
                return None;
            }
            pre_info
        } else {
            if num_args != arity {
                return None;
            }
            let &(info_ptr, _, _) = self.invoke_info.get(&pc)?;
            if info_ptr == 0 {
                return None;
            }
            info_ptr
        };
        let at = self.stack.len().checked_sub(arity + 1)?;
        let handle = node_id_opt(self.stack[at])?;
        let is_field = self.is_static_field_read(handle, class_id, field_index);
        if !is_field || !self.has_frame_state_at(pc) || self.ctrl_opt().is_none() {
            return None;
        }
        for i in 0..arity {
            let arg = node_id_opt(self.stack[at + 1 + i])?;
            if site.cast_class_ids[i] != 0 {
                let target_id = site.cast_class_ids[i];
                let target_name = &site.cast_class_names[i];
                if target_name.is_empty() {
                    return None;
                }
                let ctrl = self.ctrl_opt()?;
                let (name_ptr, name_len) =
                    crate::intern_typecheck_target(target_name, Some(target_id));
                let is_inst = self.graph.add(
                    Op::InstanceOf {
                        name_ptr: name_ptr as usize,
                        name_len,
                        seen_cell: 0,
                        header_only: false,
                    },
                    IrType::Int,
                    vec![ctrl, self.mem, arg],
                    Some(pc),
                );
                self.mem = is_inst;
                let null = self.aconst_null();
                let is_null = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![arg, null], pc);
                let ok = self.add_data(Op::Or, IrType::Int, vec![is_null, is_inst], pc);
                let ctrl = self.ctrl_opt()?;
                self.graph.add(
                    Op::Guard { bci: pc },
                    IrType::Void,
                    vec![ctrl, ok],
                    Some(pc),
                );
            }
            if site.unbox_op[i] != 0 {
                let wrap_id = site.unbox_class_ids[i];
                if wrap_id == 0 {
                    return None;
                }
                let (op, ty) = match site.unbox_op[i] {
                    1 => (UnboxOp::IntValue, IrType::Int),
                    2 => (UnboxOp::LongValue, IrType::Long),
                    _ => return None,
                };
                let ctrl = self.ctrl_opt()?;
                let unboxed = self.graph.add(
                    Op::Unbox {
                        op,
                        class_id: wrap_id,
                    },
                    ty,
                    vec![ctrl, self.mem, arg],
                    Some(pc),
                );
                self.mem = unboxed;
                self.stack[at + 1 + i] = unboxed;
            }
        }
        // Receiver non-null / interface guard before any call runs.
        match site.main_kind {
            0 => {
                if !self.indy_direct_receiver_guard(pc, arity) {
                    return None;
                }
            }
            2 => {
                if !self.mh_constant_interface_receiver_guard(pc, arity, member_info) {
                    return None;
                }
            }
            3 => {}
            _ => return None,
        }
        if site.box_return != 0 {
            let args = self.stack[at + 1..]
                .iter()
                .map(|&v| node_id_opt(v))
                .collect::<Option<Vec<NodeId>>>()?;
            if let Some(box_info) = self.convert_member_splice_admitted(pc, getstatic_pc, site.main_kind) {
                let next = self.splice_convert_member(pc, getstatic_pc, at, box_info)?;
                self.pending_splice_jump = Some(next);
                self.graph.mh_constant_call_pcs.push(pc);
                crate::ir_evidence::note_builder_expansion();
                return Some(0xb8);
            }
            let member_ret = if site.box_return == 2 { b'J' } else { b'I' };
            let result =
                self.mh_constant_filter_pre_call(getstatic_pc, member_info, member_ret, &args);
            self.stack.truncate(at);
            self.push(result);
            self.graph.mh_constant_call_pcs.push(pc);
            if !self.graph.call_reported_bcis.contains(&(getstatic_pc, pc)) {
                self.graph.call_reported_bcis.push((getstatic_pc, pc));
            }
            crate::ir_evidence::note_builder_expansion();
            Some(0xb8)
        } else {
            self.stack.remove(at);
            self.graph.mh_constant_call_pcs.push(pc);
            crate::ir_evidence::note_builder_expansion();
            Some(if site.main_kind == 3 { 0xb8 } else { 0xb6 })
        }
    }

    /// The box row (`Integer.valueOf` / `Long.valueOf` at `pc`) when the
    /// member planned at `getstatic_pc` may be spliced: its body cannot raise
    /// ([`Self::plan_convert_member_splices`]) and an instance member's
    /// receiver is proven exactly the planned class. `None` keeps the call.
    pub(in crate::ir) fn convert_member_splice_admitted(&self, pc: usize, getstatic_pc: usize, main_kind: u8) -> Option<usize> {
        if !self.convert_member_pure.contains(&getstatic_pc) {
            return None;
        }
        let &(box_info, box_args, box_ret) = self.invoke_info.get(&pc)?;
        if box_info == 0 || box_args != 1 || box_ret != b'L' {
            return None;
        }
        if main_kind != 3 {
            let &class_id = self.exact_receiver_sites.get(&getstatic_pc)?;
            self.exact_receiver_proof(getstatic_pc, class_id)?;
        }
        Some(box_info)
    }

    /// Splice the member body planned at `getstatic_pc` in place of a
    /// box-returning converting handle's member call
    /// ([`Self::convert_member_splice_admitted`]). The handle at stack index
    /// `at` goes; the converted arguments are the body's. The walk resumes
    /// after the `invokeExact` at `pc`, where the returned value is boxed
    /// ([`Self::box_convert_member_result`]). `None` refuses the method.
    pub(in crate::ir) fn splice_convert_member(
        &mut self,
        pc: usize,
        getstatic_pc: usize,
        at: usize,
        box_info: usize,
    ) -> Option<usize> {
        self.stack.remove(at);
        self.convert_box_after.insert(getstatic_pc, (box_info, pc));
        // `invokeExact` is the three-byte `invokevirtual`.
        self.begin_splice(getstatic_pc, pc + 3 - getstatic_pc)
    }

    pub(in crate::ir) fn mh_constant_filter_opcode(
        &mut self,
        pc: usize,
        getstatic_pc: usize,
        num_args: usize,
        opcode: u8,
        site: IrMhFilterSite,
        (class_id, field_index): (u32, u32),
    ) -> Option<u8> {
        if site.pre_pc != getstatic_pc
            || self.mh_constant_interface_sites.contains(&pc)
            || self.mh_constant_bound_sites.contains_key(&pc)
            || (opcode == 0xb6) != site.main_virtual
            || [pc, getstatic_pc].iter().any(|at| {
                self.unloaded_static_call_pcs.contains(at)
                    || self.first_call_cell_at(*at).is_some()
                    || self.inline_sites.contains_key(at)
            })
        {
            return None;
        }
        let &(pre_info, pre_args, pre_ret) = self.invoke_info.get(&getstatic_pc)?;
        if pre_info == 0 || pre_ret == b'V' {
            return None;
        }
        // SAFETY: `pre_info` is the address of the `JitInvokeInfo` the planner
        // boxed for the PRE row and keeps alive on the artifact (the
        // `Op::Call` built below bakes the same address).
        let pre_kind = unsafe { (*(pre_info as *const JitInvokeInfo)).invoke_kind };
        let pre_virtual = match pre_kind {
            3 => false,
            0 | 1 => true,
            _ => return None,
        };
        if pre_virtual != site.pre_virtual {
            return None;
        }
        // The site's own operands: the MAIN row's for an argument filter
        // (the target takes them), the PRE row's for a return filter.
        let site_args = match site.position {
            Some(_) => num_args,
            None => pre_args,
        };
        let at = self.stack.len().checked_sub(site_args + 1)?;
        let handle = node_id_opt(self.stack[at])?;
        let is_field = self.is_static_field_read(handle, class_id, field_index);
        if !is_field || !self.has_frame_state_at(pc) || self.ctrl_opt().is_none() {
            return None;
        }
        match site.position {
            Some(p) => {
                let p = usize::from(p);
                if pre_args != 1 || pre_virtual || p >= num_args {
                    return None;
                }
                // A virtual target's receiver that is a site argument: guarded
                // before the filter runs (a deopt re-executes everything).
                if site.main_virtual && p != 0 && !self.indy_direct_receiver_guard(pc, num_args) {
                    return None;
                }
                let arg = node_id_opt(self.stack[at + 1 + p])?;
                let filtered =
                    self.mh_constant_filter_pre_call(getstatic_pc, pre_info, pre_ret, &[arg]);
                self.stack[at + 1 + p] = filtered;
                if site.main_virtual
                    && p == 0
                    && !self.mh_constant_filter_receiver_guard(
                        pc,
                        at,
                        handle,
                        filtered,
                        site.target_slot,
                    )
                {
                    return None;
                }
                self.stack.remove(at);
            }
            None => {
                if num_args != 1 || site.main_virtual {
                    return None;
                }
                if pre_virtual && !self.indy_direct_receiver_guard(pc, pre_args) {
                    return None;
                }
                let args = self.stack[at + 1..]
                    .iter()
                    .map(|&v| node_id_opt(v))
                    .collect::<Option<Vec<NodeId>>>()?;
                if args.len() != pre_args {
                    return None;
                }
                let result =
                    self.mh_constant_filter_pre_call(getstatic_pc, pre_info, pre_ret, &args);
                self.stack.truncate(at);
                self.push(result);
            }
        }
        self.graph.mh_constant_call_pcs.push(pc);
        // j27 wave 2 (lane GCMAP): the PRE call's frame is the `invokeExact`'s
        // to every observer (HotSpot hides the LambdaForm frames between).
        if !self.graph.call_reported_bcis.contains(&(getstatic_pc, pc)) {
            self.graph.call_reported_bcis.push((getstatic_pc, pc));
        }
        // The signature-polymorphic link chain the single-pass body walks,
        // replaced by two plain calls: acceptance evidence.
        crate::ir_evidence::note_builder_expansion();
        if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] mh CONSTANT FILTER at bytecode pc {pc} in {} -- {}, the PRE call at pc \
                 {getstatic_pc}, then opcode {opcode:#04x}",
                self.method_label.as_deref().unwrap_or("<unknown>"),
                match site.position {
                    Some(p) => format!("filterArguments at argument {p}"),
                    None => "filterReturnValue".to_string(),
                },
            );
        }
        Some(opcode)
    }

    /// The PRE call of a filter species site ([`Self::mh_constant_filter_opcode`]):
    /// a plain `Op::Call` of `info_ptr` on `args` at bytecode pc `pre_pc` (the
    /// site's `getstatic`), from the current control and memory, as the invoke
    /// arm builds one; the new memory token and the call's value.
    pub(in crate::ir) fn mh_constant_filter_pre_call(
        &mut self,
        pre_pc: usize,
        info_ptr: usize,
        ret_type: u8,
        args: &[NodeId],
    ) -> NodeId {
        let ty = match ret_type {
            b'L' | b'[' => IrType::Ref,
            b'J' => IrType::Long,
            b'D' => IrType::Double,
            b'F' => IrType::Float,
            _ => IrType::Int,
        };
        let mut inputs = Vec::with_capacity(2 + args.len());
        inputs.push(self.ctrl);
        inputs.push(self.mem);
        inputs.extend_from_slice(args);
        let call = self
            .graph
            .add(Op::Call { info_ptr }, ty, inputs, Some(pre_pc));
        self.mem = call;
        call
    }

    /// The null guard of a FILTERED receiver (`filterArguments(target, 0,
    /// filter)` of a virtual target; [`Self::mh_constant_filter_opcode`]):
    /// the filter has run, so the plain snapshot at `pc` (which re-executes
    /// the whole `invokeExact`) cannot serve. The guard names a frame of its
    /// own at `pc`: the snapshot's, with the handle (stack index `at`)
    /// replaced by the species' `argL0` -- the TARGET's direct handle, read
    /// from the `final` field at `target_slot` -- and the receiver by the
    /// filter's value. The interpreter then re-executes the `invokeExact` on
    /// the target handle, whose type is the site's (the filter takes and
    /// returns the receiver's class, `compile_request::mh_filter_row_fits`),
    /// and its door raises the `NullPointerException` the species would.
    /// Appended after the plain snapshot at `pc`, which every by-bci reader
    /// keeps finding first (the OSR arm checks' rule). `false`: refused.
    pub(in crate::ir) fn mh_constant_filter_receiver_guard(
        &mut self,
        pc: usize,
        at: usize,
        handle: NodeId,
        filtered: NodeId,
        target_slot: u32,
    ) -> bool {
        if target_slot == 0 {
            return false;
        }
        let Some(ctrl) = self.ctrl_opt() else {
            return false;
        };
        let Some(sp) = self
            .graph
            .safepoints
            .iter()
            .rev()
            .find(|sp| sp.bci == pc && !sp.is_splice_state())
        else {
            return false;
        };
        if sp.stack.len() <= at + 1 || sp.stack[at] != handle {
            return false;
        }
        let (locals, mut stack, monitors, mask) = (
            sp.locals.clone(),
            sp.stack.clone(),
            sp.monitors.clone(),
            sp.monitor_elided_mask,
        );
        let Ok(snapshot) = u32::try_from(self.graph.safepoints.len()) else {
            return false;
        };
        let offset = self.iconst(i64::from(target_slot));
        let target = self.graph.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            vec![ctrl, self.mem, handle, offset],
            Some(pc),
        );
        // As the `getfield` arm: the read joins the memory chain.
        self.mem = target;
        // Not this pc's own field instruction: the field-index arms serve it.
        self.graph.synthetic_field_access_pcs.insert(pc);
        stack[at] = target;
        stack[at + 1] = filtered;
        let mut frame = SafepointSnapshot::new(pc, locals, stack, monitors);
        frame.monitor_elided_mask = mask;
        self.graph.push_safepoint(frame);
        let null = self.aconst_null();
        let cond = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![filtered, null], pc);
        let guard = self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, cond],
            Some(pc),
        );
        self.graph.set_node_frame_snapshot(guard, snapshot)
    }

    /// JIT round 19 wave 1 (lane mhiface; MHRESID18-4,
    /// `CRATONVM_JIT_MH_CONSTANT_INTERFACE` in the VM): the receiver of the
    /// interface handle's call planned at `pc` (`num_args` values on the
    /// stack, receiver deepest) is an instance of the member's interface past
    /// this point, guarded here with an `Op::InstanceOf` of the interface (the
    /// row's class, interned under the row's class id, which the planner set
    /// to the member's class: `declaring_class_id`) and a deopt at `pc` that
    /// re-executes the `invokeExact` with the handle on the stack. The
    /// verifier does not check interface assignability, so a receiver need not
    /// implement it; such a receiver, and a `null` (`instanceof` answers `0`),
    /// meet the handle's own door and its HotSpot exception. Never proven
    /// away. `false` (nothing planted) without a frame state at `pc` or a live
    /// control token, or with fewer values than the call takes.
    pub(in crate::ir) fn mh_constant_interface_receiver_guard(
        &mut self,
        pc: usize,
        num_args: usize,
        info_ptr: usize,
    ) -> bool {
        if num_args == 0 || self.stack.len() < num_args || info_ptr == 0 {
            return false;
        }
        let Some(receiver) = node_id_opt(self.stack[self.stack.len() - num_args]) else {
            return false;
        };
        if !self.has_frame_state_at(pc) {
            return false;
        }
        let Some(ctrl) = self.ctrl_opt() else {
            return false;
        };
        let _ = ctrl;
        // SAFETY: `info_ptr` is the address of the `JitInvokeInfo` the planner
        // boxed for this pc and keeps alive on the artifact.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        let seen_cell = self
            .mh_constant_interface_seen_cells
            .get(&pc)
            .copied()
            .unwrap_or(0);
        self.plant_member_class_guard(pc, receiver, info, seen_cell)
    }

    /// Guard at `pc` that `value` is an instance of the member class `info`
    /// names (interned under `info.declaring_class_id`), deopting at `pc`
    /// otherwise; `null` fails it too. The receiver check of a handle's
    /// interface member: the verifier does not check interface
    /// assignability, so neither a `null` nor a non-implementing receiver
    /// reaches the call, and the interpreter raises the handle's own error.
    /// `false` (nothing planted) without a live control token or a class.
    pub(in crate::ir) fn plant_member_class_guard(
        &mut self,
        pc: usize,
        value: NodeId,
        info: &JitInvokeInfo,
        seen_cell: usize,
    ) -> bool {
        if info.declaring_class_id == 0 || info.class_name.is_empty() {
            return false;
        }
        let Some(ctrl) = self.ctrl_opt() else {
            return false;
        };
        let (name_ptr, name_len) =
            crate::intern_typecheck_target(info.class_name, Some(info.declaring_class_id));
        let is_instance = self.graph.add(
            Op::InstanceOf {
                name_ptr: name_ptr as usize,
                name_len,
                seen_cell,
                header_only: false,
            },
            IrType::Int,
            vec![ctrl, self.mem, value],
            Some(pc),
        );
        self.mem = is_instance;
        let zero = self.iconst(0);
        let cond = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![is_instance, zero], pc);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, cond],
            Some(pc),
        );
        true
    }

    /// JIT round 16 wave 10 (lane indyir, IN16-8): the receiver of the
    /// virtual call planned at `pc` (`num_args` values on the stack, receiver
    /// deepest) is non-null past this point: proven by the graph, or guarded
    /// here with a deopt that re-executes the `invokedynamic` in the
    /// interpreter. `false` (nothing planted) without a frame state at `pc` or
    /// a live control token, or with fewer values than the call takes.
    pub(in crate::ir) fn indy_direct_receiver_guard(&mut self, pc: usize, num_args: usize) -> bool {
        if num_args == 0 || self.stack.len() < num_args {
            return false;
        }
        let Some(receiver) = node_id_opt(self.stack[self.stack.len() - num_args]) else {
            return false;
        };
        let proven = self
            .graph
            .nodes
            .get(receiver as usize)
            .is_some_and(|n| crate::ir_check_elim::definitely_non_null(&n.op));
        if proven {
            return true;
        }
        if !self.has_frame_state_at(pc) {
            return false;
        }
        let Some(ctrl) = self.ctrl_opt() else {
            return false;
        };
        let null = self.aconst_null();
        let cond = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![receiver, null], pc);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, cond],
            Some(pc),
        );
        true
    }

    /// Round 16 (lane indy, SY4-1): the bridge-site pointer the `invokedynamic`
    /// at `pc` is CALLED through, or `None` to keep the uncommon trap.
    ///
    /// # Why this exists
    ///
    /// The `0xba` arm's trap was justified as "matching the single-pass
    /// backend", but that backend stopped trapping on bridged bootstraps long
    /// ago (`x64/op_invoke.rs` `indy_site_is_bridged`). A method whose concat
    /// or lambda creation is LIVE therefore got an optimizing body that
    /// transferred to the interpreter at that site on every execution:
    /// `R16IndyThrower` (`throw new ISE("up " + (i & 1))` in a compiled
    /// thrower) took 157 971 `TransferToInterpreter` deopts at the indy bci in
    /// 180 000 calls, with nothing ever retiring the body (page
    /// `r16-indy-optimizing-tier-traps-at-every-live-invokedynamic-20261001`).
    ///
    /// # The terms
    ///
    /// The single-pass three (`indy_site_is_bridged`: a resolved bootstrap, a
    /// wired helper -- the compile door checks that one before it records the
    /// site -- and a non-`void` descriptor), plus the IR builder's own:
    ///
    /// * not inside a splice: the site table is keyed by the compiling
    ///   method's own pcs, and a relocated body's pc names no row of it;
    /// * a snapshot at `pc`, the same fail-closed rule as
    ///   [`Self::plant_uncommon_trap`]: the call's exception edge resolves its
    ///   frame state by this bci, and a protected range's rethrow pad would
    ///   otherwise park the interpreter on a blank frame;
    /// * a live control token (an unreachable tail keeps the trap shape).
    pub(in crate::ir) fn indy_bridge_site_at(&self, pc: usize, ret_tag: u8) -> Option<usize> {
        let site = *self.indy_bridge_sites.get(&pc)?;
        if site == 0 || ret_tag == b'V' {
            return None;
        }
        if !self.splice.is_empty() {
            // nio21 wave 11, lane L3 (W10L3-1): a spliced row exists only for
            // a constant-word lambda site (`IrInlineTables::indy_bridge_sites`),
            // whose probe answers the instruction's singleton: no bootstrap,
            // nothing committed, nothing to resume. Built like a spliced
            // `Op::Call` (the exception edge of the cold bridge resolves by
            // the enclosing invoke), never as a trap inside the splice.
            // j27 wave 4 (lane SPLICECAT): or a concat bridge row, built the
            // same way and recorded as a call the splice made
            // (`push_indy_bridge_call`).
            if !self.spliced_constant_indy_at(pc) && !self.spliced_concat_indy_at(pc) {
                return None;
            }
            self.ctrl_opt()?;
            return Some(site);
        }
        if !self.has_frame_state_at(pc) {
            return None;
        }
        self.ctrl_opt()?;
        Some(site)
    }

    /// nio21 wave 11, lane L3 (W10L3-1): is `pc` a SPLICED constant-word
    /// lambda site this build may lower to its `Op::IndyBridge`? A rebased row
    /// of the `(0, L)` shape, the bridge wired, the switch on. Pure: the probe
    /// answers a published singleton, so the site neither traps nor commits,
    /// which is why the committed-splice fence admits it too
    /// ([`Self::admits_after_committed_splice_store`]).
    pub(in crate::ir) fn spliced_constant_indy_at(&self, pc: usize) -> bool {
        !self.splice.is_empty()
            && self.spliced_indy_wired
            && ir_splice_indy_enabled()
            && self
                .indy_bridge_sites
                .get(&pc)
                .is_some_and(|&site| site != 0)
            && self.indy_trap_sites.get(&pc) == Some(&(0, b'L'))
    }

    /// j27 wave 4, lane SPLICECAT (`CRATONVM_JIT_SPLICE_CONCAT_BRIDGE`): is
    /// `pc` a SPLICED concat bridge row -- a rebased `(n > 0, L)` row
    /// (`crate::splice_concat_bridge`), the bridge wired, both switches on?
    /// NOT pure, unlike [`Self::spliced_constant_indy_at`]: an operand
    /// `toString()` is Java, so the node is a call the splice made
    /// ([`Self::note_splice_call`]) and the committed-splice fence does not
    /// admit it.
    pub(in crate::ir) fn spliced_concat_indy_at(&self, pc: usize) -> bool {
        !self.splice.is_empty()
            && self.spliced_indy_wired
            && ir_splice_indy_enabled()
            && crate::splice_concat_bridge::splice_concat_bridge_enabled()
            && self
                .indy_bridge_sites
                .get(&pc)
                .is_some_and(|&site| site != 0)
            && self
                .indy_trap_sites
                .get(&pc)
                .is_some_and(|&(args, ret)| args > 0 && ret == b'L')
    }

    /// Round 16 (lane indy, SY4-1): build the [`Op::IndyBridge`] call for the
    /// `invokedynamic` at `pc` and push its result.
    ///
    /// Exactly the `Op::Call` shape the invoke arms build: the arguments are
    /// popped deepest-first and put back in source order, so the node's
    /// inputs are `[ctrl, mem, arg0, arg1, …]` -- the order the lowering
    /// stages them in and the bridge decodes them in (`arg_types[i]` types
    /// `args_ptr[i]`). The node is the new memory token: a bootstrap or an
    /// operand `toString()` is arbitrary Java, so nothing may move across it.
    pub(in crate::ir) fn push_indy_bridge_call(&mut self, pc: usize, site: usize, arg_entries: usize, ret_tag: u8) {
        let mut args = Vec::with_capacity(arg_entries);
        for _ in 0..arg_entries {
            args.push(self.pop());
        }
        args.reverse();
        let ty = match ret_tag {
            b'L' | b'[' => IrType::Ref,
            b'J' => IrType::Long,
            b'D' => IrType::Double,
            b'F' => IrType::Float,
            _ => IrType::Int,
        };
        let mut inputs = Vec::with_capacity(2 + args.len());
        inputs.push(self.ctrl);
        inputs.push(self.mem);
        inputs.extend(args);
        let call = self
            .graph
            .add(Op::IndyBridge { site }, ty, inputs, Some(pc));
        // j27 wave 4 (lane SPLICECAT): a spliced concat runs its operands'
        // `toString()`s -- a call the splice made (replay fence, post-call
        // chain snapshot at its successor).
        if self.spliced_concat_indy_at(pc) {
            self.note_splice_call(call);
        }
        self.mem = call;
        self.push(call);
        if ir_bail_reporting() {
            eprintln!(
                "[ir] indy BRIDGED at bytecode pc {pc} ({arg_entries} arg(s), returns {}) in {} -- \
                 a call through the single-pass tier's bridge, not a trap",
                ret_tag as char,
                self.method_label.as_deref().unwrap_or("<unknown>"),
            );
        }
    }

    /// JIT round 21 wave 20 (lane sbfuse, STRINTR21-4;
    /// `CRATONVM_JIT_IR_SB_CHAIN_FUSE`, default ON): walk a fused
    /// `new StringBuilder().append(..)...toString()` chain
    /// ([`crate::sb_chain_fuse`]) as ONE [`Op::IndyBridge`] of the concat site
    /// the VM made of its recipe:
    ///
    /// ```text
    /// new / dup / <init>   -> a placeholder in the builder's stack slot
    /// constant operand     -> skipped with its append (literal text)
    /// run-time operand     -> built by the ordinary arms
    /// append(X)            -> the operand taken off the stack
    /// toString()           -> IndyBridge(site, operands) at the new's bci
    /// ```
    ///
    /// The node carries the `new`'s pc, whose snapshot (pushed at the top of
    /// the `new`) is the frame before the chain. Operand code has no calls,
    /// stores or branches. Field and array reads may trap; their guards and
    /// trapping IR nodes are re-stamped with this pre-`new` snapshot so
    /// deoptimization replays the original bytecode chain, including its
    /// handlers. Other nodes need no snapshot. The bridge's
    /// `OutOfMemoryError` also leaves from this frame with the chain's own
    /// handlers. The lowering is the inline concat with the bridge as its
    /// decline edge (`ir_lower::lower_indy_bridge_concat_inline`, admitted
    /// through [`Graph::sb_chain_fused_sites`]).
    ///
    /// Answers `None` at a pc that is not one of the chain's own
    /// instructions (a run-time operand's code, and every pc outside a
    /// chain) and at a planned `new` this walk does not fuse (inside a
    /// splice, dead control, a class owing initialization: the ordinary arm
    /// builds the chain as before); `Some(Some(next))` after consuming one;
    /// `Some(None)` when the walk disagrees with the plan, which refuses the
    /// build rather than guess (the recognizer read the same bytecode).
    pub(in crate::ir) fn sb_chain_fuse_step(&mut self, pc: usize) -> Option<Option<usize>> {
        use crate::sb_chain_fuse::{SbChainActive, SbChainPart};
        let Some(mut active) = self.sb_chain_active.take() else {
            let fusion = self.sb_chain_fusions.get(&pc)?;
            if !self.splice.is_empty()
                || self.ctrl_opt().is_none()
                || fusion.site == 0
                || !self.new_info.contains_key(&pc)
                || self.uninit_new_guard_sites.contains_key(&pc)
                || self.uninit_new_trap_pcs.contains(&pc)
            {
                return None;
            }
            let Some(resume_snapshot) = self
                .graph
                .safepoints
                .iter()
                .rposition(|snapshot| snapshot.bci == pc)
                .and_then(|index| u32::try_from(index).ok())
            else {
                return None;
            };
            let next = fusion.chain.first_operand_pc();
            let has_call = fusion.chain.has_call;
            let placeholder = self.aconst_null();
            self.push(placeholder);
            self.sb_chain_active = Some(SbChainActive {
                new_pc: pc,
                placeholder,
                base_len: self.stack.len(),
                args: Vec::new(),
                next_part: 0,
                resume_snapshot,
                node_cursor: self.graph.nodes.len(),
                has_call,
            });
            return Some(Some(next));
        };
        // A field or array read in an operand can trap before the builder's
        // append. Re-entering the operand with its partial builder on the
        // stack would expose the fusion placeholder to the interpreter. Give
        // only those trap nodes the pre-`new` snapshot so deoptimization
        // replays the original bytecode sequence from a real frame.
        if !active.has_call {
            for id in active.node_cursor..self.graph.nodes.len() {
                let Some(node) = self.graph.nodes.get_mut(id) else {
                    return Some(None);
                };
                let trap = match node.op {
                    Op::Guard { .. } => {
                        node.op = Op::Guard { bci: active.new_pc };
                        node.bytecode_pc = Some(active.new_pc);
                        true
                    }
                    Op::ArrayLoad(_) | Op::ArrayLength => {
                        node.bytecode_pc = Some(active.new_pc);
                        true
                    }
                    _ => false,
                };
                if trap && !self.graph.set_node_frame_snapshot(id as NodeId, active.resume_snapshot) {
                    return Some(None);
                }
            }
        }
        active.node_cursor = self.graph.nodes.len();
        let (part, to_string_pc, site, value_count, parts, local) =
            match self.sb_chain_fusions.get(&active.new_pc) {
                Some(f) => (
                    f.chain.parts.get(active.next_part).cloned(),
                    f.chain.to_string_pc,
                    f.site,
                    f.chain.value_count(),
                    f.chain.parts.len(),
                    f.chain.local.clone(),
                ),
                None => return Some(None),
            };
        // The builder's slot is on top, with `extra` operand values above it.
        let base_len = active.base_len;
        let placeholder = active.placeholder;
        let builder_on_top = |stack: &[NodeId], extra: usize| {
            stack.len() == base_len + extra
                && base_len
                    .checked_sub(1)
                    .and_then(|i| stack.get(i))
                    .is_some_and(|&slot| slot == placeholder)
        };
        let builder_present = |stack: &[NodeId]| {
            stack.len() >= base_len
                && base_len
                    .checked_sub(1)
                    .and_then(|i| stack.get(i))
                    .is_some_and(|&slot| slot == placeholder)
        };
        if let Some((start, end)) = local
            .as_ref()
            .and_then(|local| local.receiver_loads.get(active.next_part))
            .copied()
            .flatten()
        {
            if pc == start {
                let receiver_ready = if active.next_part == parts {
                    builder_present(self.stack.as_slice())
                } else {
                    builder_on_top(self.stack.as_slice(), 0)
                };
                if !receiver_ready {
                    return Some(None);
                }
                self.sb_chain_active = Some(active);
                return Some(Some(end));
            }
        }
        if local
            .as_ref()
            .and_then(|local| local.to_string_prefix)
            .is_some_and(|(start, end)| start <= pc && pc < end)
        {
            self.sb_chain_active = Some(active);
            return None;
        }
        let skip_append_pop = local
            .as_ref()
            .and_then(|local| local.append_result_pops.get(active.next_part))
            .copied()
            .unwrap_or(false);
        match part {
            Some(SbChainPart::Literal {
                operand_pc,
                append_pc,
                ..
            }) if pc == operand_pc => {
                if !builder_on_top(self.stack.as_slice(), 0) {
                    return Some(None);
                }
                active.next_part += 1;
                self.sb_chain_active = Some(active);
                Some(Some(append_pc + if skip_append_pop { 4 } else { 3 }))
            }
            Some(SbChainPart::Value { append_pc, .. }) if pc == append_pc => {
                if !builder_on_top(self.stack.as_slice(), 1) {
                    return Some(None);
                }
                let value = self.pop();
                active.args.push(value);
                active.next_part += 1;
                self.sb_chain_active = Some(active);
                Some(Some(pc + if skip_append_pop { 4 } else { 3 }))
            }
            Some(SbChainPart::Value {
                operand_pc,
                append_pc,
                ..
            }) if pc >= operand_pc && pc < append_pc => {
                self.sb_chain_active = Some(active);
                None
            }
            None if pc == to_string_pc => {
                if !builder_present(self.stack.as_slice()) || active.args.len() != value_count {
                    return Some(None);
                }
                self.stack.remove(base_len - 1);
                let mut inputs = Vec::with_capacity(2 + active.args.len());
                inputs.push(self.ctrl);
                inputs.push(self.mem);
                inputs.extend(active.args.iter().copied());
                let call_bci = if active.has_call { to_string_pc } else { active.new_pc };
                let call = self.graph.add(
                    Op::IndyBridge { site },
                    IrType::Ref,
                    inputs,
                    Some(call_bci),
                );
                if !active.has_call {
                    let _ = self.graph.set_node_frame_snapshot(call, active.resume_snapshot);
                }
                self.mem = call;
                self.push(call);
                self.graph.sb_chain_fused_sites.push(site);
                // Acceptance evidence: the chain's calls are gone.
                crate::ir_evidence::note_builder_expansion();
                if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[ir] sb-chain FUSED at bytecode pc {}..{} in {}: {parts} append(s), \
                         {value_count} run-time operand(s), one concat (site {site:#x})",
                        active.new_pc,
                        pc + 3,
                        self.method_label.as_deref().unwrap_or("<unknown>"),
                    );
                }
                Some(Some(pc + 3))
            }
            _ => Some(None),
        }
    }

    /// JIT round 20 wave 11 (lane pIndy, SPLICE19-1 of
    /// `jit-r19-splice19-proposals.md`; `CRATONVM_JIT_INDY_MUTABLE_GUARD`,
    /// default ON in the VM): the `invokedynamic` at `pc`, whose call site is
    /// a `MutableCallSite` / `VolatileCallSite`, as a plain call of the target
    /// the door planned, behind a guard on the call site's CURRENT target:
    ///
    /// ```text
    /// cs   = IndyRootRef(call_site_slot)          the instruction's CallSite
    /// cur  = Load(Ref) cs.#field_index            CallSite.target, read now
    /// want = IndyRootRef(target_slot)             the planned handle
    /// hit  = cur == want  (&& receiver != null for a virtual target)
    /// hit:  Call { the plan's row }               the target method itself
    /// miss: IndyBridge { bridge_site }            exactly today's call
    /// join: phi(call, bridge)
    /// ```
    ///
    /// # Why it is exact
    ///
    /// The VM planned the call from the bound leaf of the very handle the
    /// target slot holds (`invokedynamic::jit_indy_mutable_guard_target`): a
    /// direct handle of exactly the site's type, whose call IS that method's
    /// call -- the bridge's own `jit_indy_linked_direct_leaf` dispatches the
    /// same leaf once it has checked the same identity. Handle identity is the
    /// whole test, as for a `ConstantCallSite`'s planned target: a handle's
    /// behaviour is fixed at construction. Both words are GC roots, so the
    /// compare is of current addresses after any move, and the planned handle
    /// stays alive (no other object can come to sit at its address). The
    /// field read is an ordinary `Op::Load`, marked volatile so no pass merges
    /// or hoists it: every execution reads the target `setTarget` last wrote
    /// (`MethodHandleNatives.setCallSiteTarget{Normal,Volatile}` store the
    /// same field), which is as prompt as HotSpot's (a `MutableCallSite`
    /// needs `syncAll` only for a guarantee HotSpot may also meet early; a
    /// `VolatileCallSite` read is a plain load on x86 after the native's
    /// fenced store). Any other target, and a null receiver of a virtual
    /// target, takes the bridge -- the call this site made before, with the
    /// same arguments, from the same frame -- never a deopt: a relinking
    /// call site (Groovy, JRuby) must not charge the method.
    ///
    /// Every node carries `pc`, whose snapshot is the interpreter's frame at
    /// the top of the `invokedynamic` (nothing is committed before either
    /// call), and the call's resume and post-call rows know an
    /// `invokedynamic` is five bytes (the constant-site plan's convention).
    /// Neither call plants handler edges: the bridge never did, and both
    /// leave through the exit the bridge always took.
    ///
    /// Refused (the arm continues as before): no guard planned at `pc`, inside
    /// a splice (the plan names the compiling method's own pcs), a `void` site
    /// (the bridge returns a value), a zero word, an armed ZGC read barrier,
    /// no snapshot at `pc`, dead control, no `invoke_info` row or one whose
    /// argument count or return disagrees with the site, a virtual target
    /// without a reference receiver.
    pub(in crate::ir) fn try_indy_mutable_guarded_call(
        &mut self,
        pc: usize,
        arg_entries: usize,
        ret_tag: u8,
    ) -> bool {
        if self.indy_mutable_guards.is_empty() {
            return false;
        }
        let Some(&g) = self.indy_mutable_guards.get(&pc) else {
            return false;
        };
        // JIT round 21 lane mcs: every refusal below is named under
        // `CRATONVM_DBG_JITC` (`[ir] indy MUTABLE guard refused`).
        if let Some(why) = self.indy_mutable_guard_refusal(pc, &g, arg_entries, ret_tag) {
            self.note_indy_mutable_guard_refused(pc, why);
            return false;
        }
        let Some(&(info_ptr, _, _)) = self.invoke_info.get(&pc) else {
            return false;
        };
        let Some(ctrl0) = self.ctrl_opt() else {
            return false;
        };
        // ── Past this point the site is consumed. ──
        let mut args = Vec::with_capacity(arg_entries);
        for _ in 0..arg_entries {
            args.push(self.pop());
        }
        args.reverse();
        let ty = match ret_tag {
            b'L' | b'[' => IrType::Ref,
            b'J' => IrType::Long,
            b'D' => IrType::Double,
            b'F' => IrType::Float,
            _ => IrType::Int,
        };
        let mem0 = self.mem;
        // The guard: the call site's current target against the planned one.
        let call_site = self.graph.add(
            Op::IndyRootRef {
                slot_addr: g.call_site_slot,
            },
            IrType::Ref,
            vec![ctrl0],
            Some(pc),
        );
        let field = self.iconst(i64::from(g.field_index));
        let current = self.graph.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            vec![ctrl0, mem0, call_site, field],
            Some(pc),
        );
        self.graph.set_volatile_access(current);
        self.note_indy_guard_row(current, g.field_row);
        let planned = self.graph.add(
            Op::IndyRootRef {
                slot_addr: g.target_slot,
            },
            IrType::Ref,
            vec![ctrl0],
            Some(pc),
        );
        let mut hit_cond =
            self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![current, planned], pc);
        let mut non_null = None;
        if g.virtual_target {
            let null = self.aconst_null();
            let nn = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![args[0], null], pc);
            hit_cond = self.add_data(Op::And, IrType::Int, vec![hit_cond, nn], pc);
            non_null = Some(nn);
        }
        let iff = self
            .graph
            .add(Op::If, IrType::Control, vec![ctrl0, hit_cond], Some(pc));
        let hit = self
            .graph
            .add(Op::Proj(0), IrType::Control, vec![iff], Some(pc));
        let miss = self
            .graph
            .add(Op::Proj(1), IrType::Control, vec![iff], Some(pc));
        // The hit: the planned target's own call (its row's direct bind or
        // inline cache), from the memory state after the read.
        let mut call_inputs = Vec::with_capacity(2 + args.len());
        call_inputs.push(hit);
        call_inputs.push(current);
        call_inputs.extend(args.iter().copied());
        let call = self
            .graph
            .add(Op::Call { info_ptr }, ty, call_inputs, Some(pc));
        // JIT round 21 wave 1 (lane indy, INDY21-1): a bimorphic guard tests
        // the second target on the miss edge. Its arm is an ordinary bridge
        // call of the second target's VM cell (a `GUARDED_LEAF` bridge site
        // the helper dispatches straight to that target's method), so every
        // pass treats it exactly as the bridge: no second `Op::Call` at this
        // pc for the pc-keyed call tables (direct binds, inline caches, the
        // splice plan) to mistake for the planned target.
        let bimorphic = g.second_site != 0 && g.second_target_slot != 0;
        let (miss_ctrl, second) = if bimorphic {
            let planned2 = self.graph.add(
                Op::IndyRootRef {
                    slot_addr: g.second_target_slot,
                },
                IrType::Ref,
                vec![ctrl0],
                Some(pc),
            );
            let mut hit2_cond =
                self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![current, planned2], pc);
            if let Some(nn) = non_null {
                hit2_cond = self.add_data(Op::And, IrType::Int, vec![hit2_cond, nn], pc);
            }
            let iff2 = self
                .graph
                .add(Op::If, IrType::Control, vec![miss, hit2_cond], Some(pc));
            let hit2 = self
                .graph
                .add(Op::Proj(0), IrType::Control, vec![iff2], Some(pc));
            let miss2 = self
                .graph
                .add(Op::Proj(1), IrType::Control, vec![iff2], Some(pc));
            let mut second_inputs = Vec::with_capacity(2 + args.len());
            second_inputs.push(hit2);
            second_inputs.push(current);
            second_inputs.extend(args.iter().copied());
            let second_call = self.graph.add(
                Op::IndyBridge {
                    site: g.second_site,
                },
                ty,
                second_inputs,
                Some(pc),
            );
            (miss2, Some((hit2, second_call)))
        } else {
            (miss, None)
        };
        // The miss: the bridge call the site always made.
        let mut bridge_inputs = Vec::with_capacity(2 + args.len());
        bridge_inputs.push(miss_ctrl);
        bridge_inputs.push(current);
        bridge_inputs.extend(args.iter().copied());
        let bridge = self.graph.add(
            Op::IndyBridge {
                site: g.bridge_site,
            },
            ty,
            bridge_inputs,
            Some(pc),
        );
        // The miss side's join (bimorphic: second arm and bridge), then the
        // join with the planned call.
        let (miss_join, miss_mem, miss_value) = match second {
            Some((hit2, second_call)) => {
                let join2 =
                    self.graph
                        .add(Op::Merge, IrType::Control, vec![hit2, miss_ctrl], Some(pc));
                let mem2 = self.graph.add(
                    Op::Phi,
                    IrType::Memory,
                    vec![join2, second_call, bridge],
                    Some(pc),
                );
                let value2 = self
                    .graph
                    .add(Op::Phi, ty, vec![join2, second_call, bridge], Some(pc));
                (join2, mem2, value2)
            }
            None => (miss, bridge, bridge),
        };
        let join = self
            .graph
            .add(Op::Merge, IrType::Control, vec![hit, miss_join], Some(pc));
        self.ctrl = join;
        self.mem = self
            .graph
            .add(Op::Phi, IrType::Memory, vec![join, call, miss_mem], Some(pc));
        let result = self
            .graph
            .add(Op::Phi, ty, vec![join, call, miss_value], Some(pc));
        self.push(result);
        // The read is not this pc's own field instruction: no per-pc compact
        // row may describe it (the field-index arms serve it).
        self.graph.synthetic_field_access_pcs.insert(pc);
        if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] indy MUTABLE guarded call at bytecode pc {pc} in {} -- the planned \
                 target on a guard hit, the bridge on a miss{}",
                self.method_label.as_deref().unwrap_or("<unknown>"),
                if bimorphic {
                    " (bimorphic: the second target's leaf bridge first)"
                } else {
                    ""
                },
            );
        }
        true
    }

    /// JIT round 21 lane mcs (page
    /// `jr21-mcs-guarded-indy-call-lost-its-direct-row-FIXED-20261014.md`): why the
    /// guarded call planned at `pc` cannot be built here
    /// ([`Self::try_indy_mutable_guarded_call`],
    /// [`Self::try_indy_mutable_guarded_splice`]), or `None` when it can. Pure.
    pub(in crate::ir) fn indy_mutable_guard_refusal(
        &self,
        pc: usize,
        g: &IndyMutableGuard,
        arg_entries: usize,
        ret_tag: u8,
    ) -> Option<&'static str> {
        if !self.splice.is_empty() {
            return Some("inside a splice");
        }
        if ret_tag == b'V' {
            return Some("a void site");
        }
        if g.bridge_site == 0 || g.call_site_slot == 0 || g.target_slot == 0 {
            return Some("a zero bridge, call-site or target word");
        }
        if crate::x64::zgc_read_barrier_blocks_inline_fields() {
            return Some("the ZGC read barrier is armed");
        }
        if !self.has_frame_state_at(pc) {
            return Some("no frame state at the pc");
        }
        let Some(&(info_ptr, num_args, ret_type)) = self.invoke_info.get(&pc) else {
            return Some("no invoke_info row (the invoke plan dropped the site)");
        };
        let is_ref_tag = |t: u8| matches!(t, b'L' | b'[');
        if info_ptr == 0
            || num_args != arg_entries
            || !(ret_type == ret_tag || (is_ref_tag(ret_type) && is_ref_tag(ret_tag)))
        {
            return Some("the invoke_info row disagrees with the site's shape");
        }
        if self.ctrl_opt().is_none() {
            return Some("dead control");
        }
        let Some(at) = self.stack.len().checked_sub(arg_entries) else {
            return Some("too few operands");
        };
        if g.virtual_target {
            let receiver_is_ref = arg_entries > 0
                && self
                    .stack
                    .get(at)
                    .and_then(|&v| self.graph.node_opt(v))
                    .map(|n| n.ty)
                    == Some(IrType::Ref);
            if !receiver_is_ref {
                return Some("a virtual target without a reference receiver");
            }
        }
        None
    }

    /// mcs21 (MCS21-2, `CRATONVM_JIT_INDY_MUTABLE_GUARD_ROW`, opt-in): give a
    /// guard's `CallSite.target` read `load` the compact row the VM named
    /// ([`IndyMutableGuard::field_row`]), so a compact `CallSite` instance is
    /// read inline instead of through the checked `jit_getfield` helper (the
    /// load sits at a [`Graph::synthetic_field_access_pcs`] pc, which the
    /// per-pc rows never describe).
    ///
    /// Sound on the terms of every other [`Graph::field_rows`] entry: the
    /// lowering believes the row for this node alone, checks the reference-ness
    /// against the node's type, reads a legacy receiver through the legacy arm
    /// and keeps the layout-epoch guard's helper for a replaced layout. The
    /// receiver is the one `CallSite` object of the cell, so the exact-class
    /// layout the VM read the row from is the layout of the object read.
    /// Nothing is recorded for a zero row, with the switch off, or without
    /// compact reference fields (the compile door's own rule for its rows).
    pub(in crate::ir) fn note_indy_guard_row(&mut self, load: NodeId, row: u32) {
        if row == 0
            || !ir_indy_mutable_guard_row_enabled()
            || !cratonvm_types::compact_ref_fields_enabled()
        {
            return;
        }
        self.graph.field_rows.insert(load, (row - 1, true, b'L'));
        if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] indy MUTABLE guard in {}: the CallSite.target read carries compact \
                 displacement {}",
                self.method_label.as_deref().unwrap_or("<unknown>"),
                row - 1,
            );
        }
    }

    /// JIT round 21 lane mcs: the `CRATONVM_DBG_JITC` line naming why the
    /// guarded call planned at `pc` was not built (the site keeps its bridge).
    pub(in crate::ir) fn note_indy_mutable_guard_refused(&self, pc: usize, why: &str) {
        if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] indy MUTABLE guard refused at bytecode pc {pc} in {}: {why} -- the \
                 bridge is kept",
                self.method_label.as_deref().unwrap_or("<unknown>"),
            );
        }
    }

    /// JIT round 21 lane mcs (`CRATONVM_JIT_INDY_MUTABLE_GUARD_SPLICE`, perf,
    /// **default ON**, `=0` keeps the guarded plain call; page
    /// `jr21-mcs-guarded-indy-call-lost-its-direct-row-FIXED-20261014.md`): the guarded
    /// `MutableCallSite` / `VolatileCallSite` site at `pc` with the planned
    /// STATIC target's BODY spliced on the hit edge, where
    /// [`Self::try_indy_mutable_guarded_call`] builds a plain call of it:
    ///
    /// ```text
    /// cur  = Load(Ref) IndyRootRef(call_site_slot).#field_index   (volatile)
    /// If(cur == IndyRootRef(target_slot))
    ///   Proj(1): r1 = IndyBridge(site, args)    the call this site always made
    ///   Proj(0): the target's body, walked as a multi-return splice
    /// Merge(miss, returns..); Phi(memory); Phi(result)
    /// ```
    ///
    /// The splice-shaped twin of the guarded call, and the shape the constant
    /// plan already gives a `ConstantCallSite` (whose `invokedynamic` the
    /// `invokestatic` arm splices at the same pc, `begin_splice(pc, 5)`).
    /// HotSpot inlines a `MutableCallSite`'s current target too, under a
    /// dependency that `setTarget` invalidates; the guard is that dependency,
    /// checked per execution, so a retarget is observed by the very next call
    /// exactly as with the call form. The body is the one the planner resolved
    /// for the target in its class context (the planned call's own row), and a
    /// trap inside it resumes at this `invokedynamic` with REEXECUTE, as the
    /// constant splice's does. The miss edge is built before the body is
    /// walked (`begin_guarded_receiver_splice`'s rule), so no fence of the body
    /// sees it.
    ///
    /// `None`: not taken (the guarded call is tried next, unchanged) -- the
    /// switch off, a virtual target, a bimorphic guard (wave 1 lane indy's
    /// second arm keeps the call form), any refusal of the call form, no planned
    /// body at `pc`, or a body this route does not splice (first-call cell,
    /// exact-receiver, synchronized, inside a covered `try`). `Some(None)`:
    /// the body disagreed with the walk after the guard was built -- refuse the
    /// build, as every other guarded splice does.
    pub(in crate::ir) fn try_indy_mutable_guarded_splice(
        &mut self,
        pc: usize,
        arg_entries: usize,
        ret_tag: u8,
    ) -> Option<Option<usize>> {
        if self.indy_mutable_guards.is_empty() {
            return None;
        }
        let &g = self.indy_mutable_guards.get(&pc)?;
        if g.virtual_target
            || g.second_site != 0
            || g.second_target_slot != 0
            || !ir_indy_mutable_guard_splice_enabled()
            || self
                .indy_mutable_guard_refusal(pc, &g, arg_entries, ret_tag)
                .is_some()
        {
            return None;
        }
        let site = self.inline_sites.get(&pc)?;
        if site.num_args != arg_entries || !site.returns_value || site.receiver_is_arg0 {
            return None;
        }
        if self.first_call_cell_at(pc).is_some()
            || self.exact_receiver_sites.contains_key(&pc)
            || self.sync_splice_sites.contains_key(&pc)
            || self.pc_in_splice_handler_cover(pc)
        {
            return None;
        }
        let ctrl0 = self.ctrl_opt()?;
        let at = self.stack.len().checked_sub(arg_entries)?;
        let args: Vec<NodeId> = self.stack.get(at..)?.to_vec();
        if args.contains(&NO_NODE) {
            return None;
        }
        // ── Past this point the site is consumed. ──
        let ty = match ret_tag {
            b'L' | b'[' => IrType::Ref,
            b'J' => IrType::Long,
            b'D' => IrType::Double,
            b'F' => IrType::Float,
            _ => IrType::Int,
        };
        let mem0 = self.mem;
        // The guard, exactly as the call form builds it.
        let call_site = self.graph.add(
            Op::IndyRootRef {
                slot_addr: g.call_site_slot,
            },
            IrType::Ref,
            vec![ctrl0],
            Some(pc),
        );
        let field = self.iconst(i64::from(g.field_index));
        let current = self.graph.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            vec![ctrl0, mem0, call_site, field],
            Some(pc),
        );
        self.graph.set_volatile_access(current);
        self.note_indy_guard_row(current, g.field_row);
        let planned = self.graph.add(
            Op::IndyRootRef {
                slot_addr: g.target_slot,
            },
            IrType::Ref,
            vec![ctrl0],
            Some(pc),
        );
        let hit_cond = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![current, planned], pc);
        let iff = self
            .graph
            .add(Op::If, IrType::Control, vec![ctrl0, hit_cond], Some(pc));
        let hit = self
            .graph
            .add(Op::Proj(0), IrType::Control, vec![iff], Some(pc));
        let miss = self
            .graph
            .add(Op::Proj(1), IrType::Control, vec![iff], Some(pc));
        // The miss edge: the bridge call the site always made.
        let mut bridge_inputs = Vec::with_capacity(2 + args.len());
        bridge_inputs.push(miss);
        bridge_inputs.push(current);
        bridge_inputs.extend(args.iter().copied());
        let bridge = self.graph.add(
            Op::IndyBridge {
                site: g.bridge_site,
            },
            ty,
            bridge_inputs,
            Some(pc),
        );
        // JIT round 23 wave 2e (W4L1-1): the miss call is applied at the join
        // (`finish_multi_return_splice`), as `begin_guarded_receiver_splice`'s.
        if self.field_memo_defer_miss_call {
            self.field_memo_deferred_calls.push(bridge);
        }
        // The read is not this pc's own field instruction (as the call form).
        self.graph.synthetic_field_access_pcs.insert(pc);
        // The hit edge: the target's body, from the memory state after the
        // read. `begin_splice` takes the arguments off the abstract stack.
        self.ctrl = hit;
        self.mem = current;
        let Some(next) = self.begin_splice(pc, 5) else {
            return Some(None);
        };
        let Some(frame) = self.splice.last_mut() else {
            return Some(None);
        };
        frame.multi_return = true;
        frame.exits.push((miss, bridge, bridge));
        if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] indy MUTABLE guarded splice at bytecode pc {pc} in {} -- the planned \
                 target's body on a guard hit, the bridge on a miss",
                self.method_label.as_deref().unwrap_or("<unknown>"),
            );
        }
        Some(Some(next))
    }

    /// JIT round 20 wave 11 (lane pIndy, W10L3-3 / W11L3-3 of
    /// `jit-r21-nio-proposals-w10-L3.md` / `-w11-L3.md`;
    /// `CRATONVM_JIT_IR_INDY_LAMBDA_WORD`, default ON): the method's own
    /// non-capturing lambda site at `pc`, whose singleton slot the VM
    /// published, as
    ///
    /// ```text
    /// w = IndyRootRef(slot)            the instruction's singleton, or null
    /// w != null ? w : IndyBridge(site)
    /// ```
    ///
    /// instead of the trap (or, under `CRATONVM_JIT_IR_INDY_BRIDGE`, the
    /// bridge call whose probe reads the same word). The hot path is one load
    /// with NO memory effect -- an `Op::IndyBridge` is an opaque effect even
    /// when its probe hits, which fences every load and store around a lambda
    /// creation -- and the memory state only joins the bridge's on the cold
    /// edge.
    ///
    /// # Why it is exact
    ///
    /// The slot is the instruction's `LAMBDA_SINGLETON_CACHE` row, published
    /// once (`invokedynamic::jit_indy_constant_word_slot`): a non-zero word in
    /// it is the very instance the interpreter hands out for the instruction,
    /// which is what the bridge's probe returns; a retired row reads `0` and
    /// takes the bridge. The slot is a GC root rewritten in place while
    /// mutators are parked, so the read is never stale. The bridge edge is the
    /// call the site makes today, from the frame at the top of the
    /// `invokedynamic` (`has_frame_state_at`), and leaves through the same
    /// exits.
    ///
    /// Refused (the arm continues as before): no row, inside a splice (rows
    /// name the compiling method's own pcs; a spliced site keeps its
    /// `spliced_constant_indy_at` bridge), a site with arguments or a non-
    /// reference result, a zero word, no snapshot at `pc`, dead control.
    ///
    /// Not built (the rest of W10L3-3): GVN of two evaluations (the read is
    /// pinned) and the proxy's exact class as a type fact.
    pub(in crate::ir) fn try_indy_lambda_word(&mut self, pc: usize, arg_entries: usize, ret_tag: u8) -> bool {
        if self.indy_lambda_words.is_empty() {
            return false;
        }
        let Some(&(site, slot)) = self.indy_lambda_words.get(&pc) else {
            return false;
        };
        if !self.splice.is_empty()
            || arg_entries != 0
            || ret_tag != b'L'
            || site == 0
            || slot == 0
            || !self.has_frame_state_at(pc)
        {
            return false;
        }
        let Some(ctrl0) = self.ctrl_opt() else {
            return false;
        };
        let mem0 = self.mem;
        let word = self.graph.add(
            Op::IndyRootRef { slot_addr: slot },
            IrType::Ref,
            vec![ctrl0],
            Some(pc),
        );
        let null = self.aconst_null();
        let published = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![word, null], pc);
        let iff = self
            .graph
            .add(Op::If, IrType::Control, vec![ctrl0, published], Some(pc));
        let hit = self
            .graph
            .add(Op::Proj(0), IrType::Control, vec![iff], Some(pc));
        let miss = self
            .graph
            .add(Op::Proj(1), IrType::Control, vec![iff], Some(pc));
        let bridge = self.graph.add(
            Op::IndyBridge { site },
            IrType::Ref,
            vec![miss, mem0],
            Some(pc),
        );
        let join = self
            .graph
            .add(Op::Merge, IrType::Control, vec![hit, miss], Some(pc));
        self.ctrl = join;
        self.mem = self
            .graph
            .add(Op::Phi, IrType::Memory, vec![join, mem0, bridge], Some(pc));
        let result = self
            .graph
            .add(Op::Phi, IrType::Ref, vec![join, word, bridge], Some(pc));
        self.push(result);
        if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] indy LAMBDA WORD at bytecode pc {pc} in {} -- the published singleton, \
                 the bridge when it is retired",
                self.method_label.as_deref().unwrap_or("<unknown>"),
            );
        }
        true
    }

    /// JIT round 20 wave 12 (lane qLamNew, W10L3-2 / W11L3-2 second half /
    /// W9L2-1 part 2 of `jit-r21-nio-proposals-w10-L3.md` / `-w11-L3.md` /
    /// `-w9-L2.md`; `CRATONVM_JIT_IR_INDY_CAPTURE_NEW`, default ON): the
    /// method's own linked CAPTURING lambda site at `pc`, built as what the
    /// bridge's warm path does at run time (`allocate_lambda_proxy_tlab_fast`):
    ///
    /// ```text
    /// p = New(proxy class, n)          a fresh object per evaluation
    /// p.field[i] = capture i           one Store per capture, field order
    /// ```
    ///
    /// instead of the trap (or the bridge call). Escape analysis sees an
    /// ordinary allocation with constant-index stores, so a lambda that does
    /// not escape is scalar-replaced, and a deopt that names it rematerialises
    /// an object of the proxy class with its captures (`deopt_materialize`
    /// allocates by class id and field count and stores through `set_field`,
    /// which is class-agnostic).
    ///
    /// # Why it is exact
    ///
    /// * **The class.** The row's proxy class is the VM's answer for the
    ///   instruction (`JitIndyGenericSite::capture_memo`, learnt from the
    ///   resolution cache), and a linked row is permanent
    ///   (`CRATONVM_RESOLUTION_LINKED_PERMANENT`; the VM refuses the fact with
    ///   it off), so every evaluation the bridge would serve allocates exactly
    ///   this class.
    /// * **The layout.** A proxy has no registered compact layout, so it is a
    ///   legacy object of `n` 16-byte cells (`plan_object_alloc` and both
    ///   inline bumps ask the same predicate), capture `i` in cell `i` with the
    ///   tag its `Value` carries -- what the stores below write, through the
    ///   ordinary field-store lowering (barriers included; the pc is listed
    ///   synthetic, so no per-pc compact row can steer them).
    /// * **Identity.** A capturing evaluation is a new object on HotSpot too,
    ///   so `==` between two evaluations stays `false`.
    /// * **Initialisation.** A proxy has no `<clinit>` and needs no post-init
    ///   work ([`Graph::lambda_proxy_alloc_classes`]); every field is written
    ///   right after the allocation, before any safepoint can observe it.
    ///
    /// Refused (the arm continues as before): no row, inside a splice, a site
    /// without arguments or with a non-reference result, a capture whose type
    /// the tier cannot store (`field_access_type`) or whose operand disagrees
    /// with its tag, no snapshot at `pc`, dead control.
    pub(in crate::ir) fn try_indy_capture_new(&mut self, pc: usize, arg_entries: usize, ret_tag: u8) -> bool {
        if self.indy_capture_news.is_empty() {
            return false;
        }
        let Some(row) = self.indy_capture_news.get(&pc).cloned() else {
            return false;
        };
        let n = row.capture_tags.len();
        // JIT round 20 wave 19 (lane ySplice, SPLICE20-1): a SPLICED row (a
        // combined pc only `apply_inline_tables` installs, from
        // `IrInlineTables::indy_capture_news`) is admitted under its switch.
        // Inside a splice the walk records the chain snapshot at `pc` rather
        // than a flat one, which is what a deopt naming the proxy resumes
        // through (the captures are still on its operand stack); the
        // allocation is a splice-local object, as a spliced `new`'s is. The
        // committed-splice fence (`admits_after_committed_splice_store`) does
        // not admit this pc after a committed store, and the planner's
        // trap-after-call rule counts it (it is no constant-word row).
        let in_splice = !self.splice.is_empty();
        if (in_splice && !ir_splice_indy_capture_enabled())
            || ret_tag != b'L'
            || arg_entries == 0
            || n != arg_entries
            || row.proxy_class_id == 0
            || self.stack.len() < n
            || (!in_splice && !self.has_frame_state_at(pc))
            || self.ctrl_opt().is_none()
        {
            return false;
        }
        // Every capture's store kind, checked against its operand BEFORE
        // anything is popped or built, so a refusal leaves the stack as it was.
        let first = self.stack.len() - n;
        let mut kinds: Vec<MemKind> = Vec::with_capacity(n);
        for (i, &tag) in row.capture_tags.iter().enumerate() {
            let Some(ty) = self.field_access_type(tag) else {
                return false;
            };
            let Some(value) = node_id_opt(self.stack[first + i]) else {
                return false;
            };
            if self.graph.nodes.get(value as usize).map(|v| v.ty) != Some(ty) {
                return false;
            }
            kinds.push(match ty {
                IrType::Ref => MemKind::Ref,
                IrType::Long => MemKind::Long,
                IrType::Float => MemKind::Float,
                IrType::Double => MemKind::Double,
                _ => MemKind::Int,
            });
        }
        let captures: Vec<NodeId> = self.stack.split_off(first);
        let proxy = self.graph.add(
            Op::New {
                class_id: row.proxy_class_id,
                num_fields: n,
            },
            IrType::Ref,
            vec![self.ctrl, self.mem],
            Some(pc),
        );
        // SPLICE20-1: splice-local, as the `0xbb` arm's allocation (a no-op
        // outside a splice).
        self.note_splice_local_alloc(proxy);
        for (i, (&value, &kind)) in captures.iter().zip(kinds.iter()).enumerate() {
            let offset = self.iconst(i as i64);
            let store = self.graph.add(
                Op::Store(kind),
                IrType::Memory,
                vec![self.ctrl, self.mem, proxy, offset, value],
                Some(pc),
            );
            self.mem = store;
        }
        // The stores are not this pc's own field instruction: no per-pc
        // compact row may describe them (the field-index arms serve them).
        self.graph.synthetic_field_access_pcs.insert(pc);
        self.graph
            .lambda_proxy_alloc_classes
            .insert(row.proxy_class_id);
        // JIT round 21 (lane lambda2): the planned SAM call on this proxy
        // takes these values instead (`lambda_impl_opcode`). JIT round 21
        // wave 15 (lane lambda3, LAMBDACAP21-2): a spliced body's combined
        // pcs too (`IrInlineTables::lambda_impl_sites`), disjoint from the
        // method's own.
        if self.lambda_impl_sites.values().any(|&(at, _, _)| at == pc) {
            self.indy_capture_new_values
                .insert(pc, (proxy, captures.clone()));
        }
        self.push(proxy);
        if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] indy CAPTURE NEW at bytecode pc {pc} in {} -- proxy class {:#x}, {n} \
                 capture(s), allocated inline",
                self.method_label.as_deref().unwrap_or("<unknown>"),
                row.proxy_class_id,
            );
        }
        true
    }

    /// JIT round 21 wave 17 (lane records, IN16-6 of
    /// `jr21-r18-backlog-retriage-FIXED-20261029.md` section 2.2;
    /// `CRATONVM_JIT_IR_INDY_RECORD_FOLD`, perf, **default ON**, `=0` keeps the
    /// bridge): a record's javac-generated `hashCode` / `equals` body's
    /// `invokedynamic ObjectMethods.bootstrap` at `pc`, built as the fold the
    /// bootstrap's handle chain computes, instead of the bridge call (the
    /// bridge's record arm walks the record natively per call: 768 ns for
    /// `R16IndyRecord` `hashP` against HotSpot's 2).
    ///
    /// ```text
    /// hashCode (R)I:   h = 0; for each getter g, in order: h = 31 * h + hash(g(this))
    /// equals (R,O)Z:   this == o                    -> true
    ///                  !ExactClassIs(o, R)          -> false   (null, an array, another class)
    ///                  for each getter g, LAST first: !eq(g(this), g(o)) -> false
    ///                  -> true
    /// ```
    ///
    /// # Why it is exact (JDK 25 `java.lang.runtime.ObjectMethods`)
    ///
    /// * `makeHashCode` folds the getters in order with `hashCombiner(x, y)
    ///   = x * 31 + y`, the accumulator (every earlier getter) evaluated before
    ///   the component (`filterArguments` runs its filters left to right).
    ///   `hasher(T)`: `Integer.hashCode` for `int` (and the identity of
    ///   `Short` / `Byte` / `Character.hashCode` for `short` / `byte` / `char`,
    ///   whose field values are already the narrowed ints), `Boolean.hashCode`
    ///   (`1231` / `1237`), `Long.hashCode` (`(int)(v ^ v >>> 32)`),
    ///   `Float.hashCode` (`floatToIntBits`: one canonical NaN),
    ///   `Double.hashCode` (`doubleToLongBits`, then `Long.hashCode`), and
    ///   `Objects.hashCode` for a reference or an array -- CALLED here, the
    ///   same static method the handle reaches, so a component's `hashCode`
    ///   runs (or throws) in the same order with the same frames below it.
    /// * `makeEquals` is `guardWithTest(isSameObject, true,
    ///   guardWithTest(isInstance, accumulator, false))`, the accumulator a
    ///   `guardWithTest` per getter, each new one outermost: the LAST getter
    ///   is compared first and the first difference ends it. `isInstance` of
    ///   a `final` class (the VM answers only those) is the exact class test
    ///   (`ExactClassIs`: `0` for null and for an array). `equalator(T)`:
    ///   `==` for the int category and `long`, `Float.compare(a, b) == 0` /
    ///   `Double.compare(a, b) == 0` -- equal exactly when the CANONICAL bits
    ///   are (so `NaN` equals `NaN` and `0.0` does not equal `-0.0`) -- and a
    ///   call of `Objects.equals(a, b)` for a reference or an array.
    /// * The getters are the record's own instance fields (`REF_getField` of
    ///   the class, resolved by the VM as the `getfield` site resolves), read
    ///   with the `0xb4` arm's `Op::Load` by field index; the pc is listed
    ///   synthetic, so no per-pc compact row may describe them.
    /// * Each `Objects` call is an `Op::Call` of the pc's `invoke_info` row
    ///   (the plan's `invokestatic`), from the frame at the top of the
    ///   `invokedynamic`: an exception leaves through the call's exit, as one
    ///   from the handle chain leaves the bridge. None of them is the
    ///   instruction's result, so the pc is listed in
    ///   [`Graph::record_fold_call_pcs`] and `ir_lower` plants no post-call
    ///   exit after them (one would resume after the instruction with a
    ///   component's hash as its value).
    ///
    /// Refused (the arm continues as before: the bridge): see
    /// [`Self::indy_record_fold_refusal`].
    pub(in crate::ir) fn try_indy_record_fold(&mut self, pc: usize, arg_entries: usize, ret_tag: u8) -> bool {
        if self.indy_record_folds.is_empty() {
            return false;
        }
        let Some(fold) = self.indy_record_folds.get(&pc).cloned() else {
            return false;
        };
        if let Some(why) = self.indy_record_fold_refusal(pc, &fold, arg_entries, ret_tag) {
            if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[ir] indy RECORD FOLD refused at bytecode pc {pc} in {}: {why} -- the \
                     bridge is kept",
                    self.method_label.as_deref().unwrap_or("<unknown>"),
                );
            }
            return false;
        }
        // `0` when the record has no reference component (no call is built).
        let info_ptr = self
            .invoke_info
            .get(&pc)
            .map(|&(info_ptr, _, _)| info_ptr)
            .unwrap_or(0);
        let result = if fold.equals {
            let other = self.pop();
            let this = self.pop();
            self.record_fold_equals(pc, &fold, info_ptr, this, other)
        } else {
            let this = self.pop();
            self.record_fold_hash(pc, &fold, info_ptr, this)
        };
        // The loads are not this pc's own field instruction: no per-pc compact
        // row may describe them (the field-index arms serve them).
        self.graph.synthetic_field_access_pcs.insert(pc);
        self.graph.record_fold_call_pcs.push(pc);
        self.push(result);
        if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] indy RECORD FOLD at bytecode pc {pc} in {} -- {} over {} component(s), \
                 field-wise (string-hash slot {:?}, {} String component(s))",
                self.method_label.as_deref().unwrap_or("<unknown>"),
                if fold.equals { "equals" } else { "hashCode" },
                fold.components.len(),
                fold.string_hash_slot,
                fold.string_components.iter().filter(|&&s| s).count(),
            );
        }
        true
    }

    /// JIT round 21 wave 17 (lane records, IN16-6): why the record fold
    /// planned at `pc` cannot be built here
    /// ([`Self::try_indy_record_fold`]), or `None` when it can. Pure.
    pub(in crate::ir) fn indy_record_fold_refusal(
        &self,
        pc: usize,
        fold: &IndyRecordFold,
        arg_entries: usize,
        ret_tag: u8,
    ) -> Option<&'static str> {
        /// More components than this keep the bridge (a record that wide is
        /// rare, and each reference component is a call).
        const MAX_COMPONENTS: usize = 32;
        if !self.splice.is_empty() {
            return Some("inside a splice");
        }
        let (want_args, want_ret) = if fold.equals { (2, b'Z') } else { (1, b'I') };
        if arg_entries != want_args || ret_tag != want_ret {
            return Some("the site's shape is not the fold's");
        }
        if fold.record_class_id == 0 {
            return Some("no record class id");
        }
        if fold.components.len() > MAX_COMPONENTS {
            return Some("too many components");
        }
        let mut has_reference = false;
        for &(_, tag) in &fold.components {
            if self.field_access_type(tag).is_none() {
                return Some("a component type this tier cannot load");
            }
            match tag {
                b'L' | b'[' => has_reference = true,
                b'F' | b'D' if !ir_fp_raw_bits_enabled() => {
                    return Some("a float component without the raw-bits moves");
                }
                b'I' | b'S' | b'B' | b'C' | b'Z' | b'J' | b'F' | b'D' => {}
                _ => return Some("an unknown component tag"),
            }
        }
        if has_reference {
            let Some(&(info_ptr, num_args, ret_type)) = self.invoke_info.get(&pc) else {
                return Some("no invoke_info row (the invoke plan dropped the site)");
            };
            if info_ptr == 0 || num_args != want_args || ret_type != want_ret {
                return Some("the invoke_info row is not the Objects call");
            }
        }
        if !self.has_frame_state_at(pc) {
            return Some("no frame state at the pc");
        }
        if self.ctrl_opt().is_none() {
            return Some("dead control");
        }
        let Some(at) = self.stack.len().checked_sub(arg_entries) else {
            return Some("too few operands");
        };
        let all_refs = self.stack[at..].iter().all(|&v| {
            node_id_opt(v)
                .and_then(|v| self.graph.node_opt(v))
                .map(|n| n.ty)
                == Some(IrType::Ref)
        });
        if !all_refs {
            return Some("an operand is not a reference");
        }
        None
    }

    /// The record fold's read of component field `index` (tag `tag`) of
    /// `base`, as the `0xb4` arm reads a field: control-pinned, the new
    /// memory token.
    pub(in crate::ir) fn record_fold_load(&mut self, pc: usize, base: NodeId, index: u32, tag: u8) -> NodeId {
        let (kind, ty) = match tag {
            b'L' | b'[' => (MemKind::Ref, IrType::Ref),
            b'J' => (MemKind::Long, IrType::Long),
            b'F' => (MemKind::Float, IrType::Float),
            b'D' => (MemKind::Double, IrType::Double),
            _ => (MemKind::Int, IrType::Int),
        };
        let offset = self.iconst(i64::from(index));
        let load = self.graph.add(
            Op::Load(kind),
            ty,
            vec![self.ctrl, self.mem, base, offset],
            Some(pc),
        );
        self.mem = load;
        load
    }

    /// `Float.floatToIntBits(v)`: the raw bits, with every NaN replaced by
    /// the canonical `0x7fc00000`, branch-free:
    /// `nan = (raw & 0x7fffffff) > 0x7f800000; mask = -nan;
    ///  (raw & ~mask) | (0x7fc00000 & mask)`.
    pub(in crate::ir) fn record_fold_float_bits(&mut self, pc: usize, v: NodeId) -> NodeId {
        let sop = ScalarOp::FloatToRawIntBits;
        let raw = self.add_data(Op::ScalarIntrinsic(sop), sop.result_type(), vec![v], pc);
        let abs_mask = self.iconst(0x7fff_ffff);
        let mag = self.add_data(Op::And, IrType::Int, vec![raw, abs_mask], pc);
        let inf = self.iconst(0x7f80_0000);
        let nan = self.add_data(Op::Cmp(CmpOp::Gt), IrType::Int, vec![mag, inf], pc);
        let mask = self.add_data(Op::Neg, IrType::Int, vec![nan], pc);
        let all = self.iconst(-1);
        let not_mask = self.add_data(Op::Xor, IrType::Int, vec![mask, all], pc);
        let keep = self.add_data(Op::And, IrType::Int, vec![raw, not_mask], pc);
        let canonical = self.iconst(0x7fc0_0000);
        let canon = self.add_data(Op::And, IrType::Int, vec![canonical, mask], pc);
        self.add_data(Op::Or, IrType::Int, vec![keep, canon], pc)
    }

    /// `Double.doubleToLongBits(v)`: [`Self::record_fold_float_bits`] at 64
    /// bits (canonical NaN `0x7ff8000000000000`).
    pub(in crate::ir) fn record_fold_double_bits(&mut self, pc: usize, v: NodeId) -> NodeId {
        let sop = ScalarOp::DoubleToRawLongBits;
        let raw = self.add_data(Op::ScalarIntrinsic(sop), sop.result_type(), vec![v], pc);
        let abs_mask = self.lconst(i64::MAX);
        let mag = self.add_data(Op::And, IrType::Long, vec![raw, abs_mask], pc);
        let inf = self.lconst(0x7ff0_0000_0000_0000);
        let order = self.add_data(Op::LCmp, IrType::Int, vec![mag, inf], pc);
        let zero = self.iconst(0);
        let nan = self.add_data(Op::Cmp(CmpOp::Gt), IrType::Int, vec![order, zero], pc);
        let wide = self.add_data(Op::I2L, IrType::Long, vec![nan], pc);
        let mask = self.add_data(Op::Neg, IrType::Long, vec![wide], pc);
        let all = self.lconst(-1);
        let not_mask = self.add_data(Op::Xor, IrType::Long, vec![mask, all], pc);
        let keep = self.add_data(Op::And, IrType::Long, vec![raw, not_mask], pc);
        let canonical = self.lconst(0x7ff8_0000_0000_0000);
        let canon = self.add_data(Op::And, IrType::Long, vec![canonical, mask], pc);
        self.add_data(Op::Or, IrType::Long, vec![keep, canon], pc)
    }

    /// `Long.hashCode(v)`: `(int)(v ^ (v >>> 32))`.
    pub(in crate::ir) fn record_fold_long_hash(&mut self, pc: usize, v: NodeId) -> NodeId {
        let thirty_two = self.iconst(32);
        let high = self.add_data(Op::UShr, IrType::Long, vec![v, thirty_two], pc);
        let mixed = self.add_data(Op::Xor, IrType::Long, vec![v, high], pc);
        self.add_data(Op::L2I, IrType::Int, vec![mixed], pc)
    }

    /// The `Objects` call of the fold's row (`Objects.hashCode(a)` or
    /// `Objects.equals(a, b)`), from the current control and memory, as the
    /// invoke arm builds an `Op::Call`; the new memory token.
    pub(in crate::ir) fn record_fold_call(&mut self, pc: usize, info_ptr: usize, args: &[NodeId]) -> NodeId {
        let mut inputs = Vec::with_capacity(2 + args.len());
        inputs.push(self.ctrl);
        inputs.push(self.mem);
        inputs.extend_from_slice(args);
        let call = self
            .graph
            .add(Op::Call { info_ptr }, IrType::Int, inputs, Some(pc));
        self.mem = call;
        call
    }

    /// The record fold's `hashCode`: `h = 31 * h + hash(component)` over the
    /// getters in order. See [`Self::try_indy_record_fold`].
    pub(in crate::ir) fn record_fold_hash(
        &mut self,
        pc: usize,
        fold: &IndyRecordFold,
        info_ptr: usize,
        this: NodeId,
    ) -> NodeId {
        let mut h = self.iconst(0);
        for (position, &(index, tag)) in fold.components.iter().enumerate() {
            let v = self.record_fold_load(pc, this, index, tag);
            let hashed = match tag {
                b'Z' => {
                    // `Boolean.hashCode`: 1231 for true, 1237 for false.
                    let zero = self.iconst(0);
                    let set = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![v, zero], pc);
                    let six = self.iconst(6);
                    let off = self.add_data(Op::Mul, IrType::Int, vec![set, six], pc);
                    let base = self.iconst(1237);
                    self.add_data(Op::Sub, IrType::Int, vec![base, off], pc)
                }
                b'J' => self.record_fold_long_hash(pc, v),
                b'F' => self.record_fold_float_bits(pc, v),
                b'D' => {
                    let bits = self.record_fold_double_bits(pc, v);
                    self.record_fold_long_hash(pc, bits)
                }
                // j27 wave 5 (lane RECORDS): `record_fold_shortcuts.rs`.
                b'L' | b'[' => self.record_fold_ref_hash(pc, fold, position, info_ptr, v),
                // `I` / `S` / `B` / `C`: the (narrowed) int itself.
                _ => v,
            };
            let thirty_one = self.iconst(31);
            let scaled = self.add_data(Op::Mul, IrType::Int, vec![h, thirty_one], pc);
            h = self.add_data(Op::Add, IrType::Int, vec![scaled, hashed], pc);
        }
        h
    }

    /// The record fold's `equals`. See [`Self::try_indy_record_fold`].
    pub(in crate::ir) fn record_fold_equals(
        &mut self,
        pc: usize,
        fold: &IndyRecordFold,
        info_ptr: usize,
        this: NodeId,
        other: NodeId,
    ) -> NodeId {
        // `(control, memory, answer)` of every edge that has its answer.
        let mut exits: Vec<(NodeId, NodeId, NodeId)> = Vec::new();
        let one = self.iconst(1);
        let zero = self.iconst(0);
        // isSameObject.
        let same = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![this, other], pc);
        let ctrl = self.ctrl;
        let (is_same, not_same) = self.record_fold_branch(pc, ctrl, same);
        exits.push((is_same, self.mem, one));
        // isInstance of the final record class.
        let instance = self.add_data(
            Op::ExactClassIs {
                class_id: fold.record_class_id,
            },
            IrType::Int,
            vec![other],
            pc,
        );
        let (is, not) = self.record_fold_branch(pc, not_same, instance);
        exits.push((not, self.mem, zero));
        self.ctrl = is;
        // The getters, LAST first; the first difference answers `false`.
        for &(index, tag) in fold.components.iter().rev() {
            let a = self.record_fold_load(pc, this, index, tag);
            let b = self.record_fold_load(pc, other, index, tag);
            let equal = match tag {
                b'J' => {
                    let order = self.add_data(Op::LCmp, IrType::Int, vec![a, b], pc);
                    self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![order, zero], pc)
                }
                b'F' => {
                    let a_bits = self.record_fold_float_bits(pc, a);
                    let b_bits = self.record_fold_float_bits(pc, b);
                    self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![a_bits, b_bits], pc)
                }
                b'D' => {
                    let a_bits = self.record_fold_double_bits(pc, a);
                    let b_bits = self.record_fold_double_bits(pc, b);
                    let order = self.add_data(Op::LCmp, IrType::Int, vec![a_bits, b_bits], pc);
                    self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![order, zero], pc)
                }
                // j27 wave 5 (lane RECORDS): `record_fold_shortcuts.rs`.
                b'L' | b'[' => self.record_fold_ref_equal(pc, info_ptr, a, b),
                // The int category, `boolean` included: `a == b`.
                _ => self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![a, b], pc),
            };
            let ctrl = self.ctrl;
            let (same_value, differ) = self.record_fold_branch(pc, ctrl, equal);
            exits.push((differ, self.mem, zero));
            self.ctrl = same_value;
        }
        exits.push((self.ctrl, self.mem, one));
        self.record_fold_join(pc, &exits)
    }

    /// `If(cond)` from `ctrl`: `(taken when non-zero, taken when zero)`.
    pub(in crate::ir) fn record_fold_branch(&mut self, pc: usize, ctrl: NodeId, cond: NodeId) -> (NodeId, NodeId) {
        let iff = self
            .graph
            .add(Op::If, IrType::Control, vec![ctrl, cond], Some(pc));
        let yes = self
            .graph
            .add(Op::Proj(0), IrType::Control, vec![iff], Some(pc));
        let no = self
            .graph
            .add(Op::Proj(1), IrType::Control, vec![iff], Some(pc));
        (yes, no)
    }

    /// Branch pcs the profile says are ALWAYS taken. See
    /// [`Self::prune_always_taken_branch`].
    pub fn set_pruned_branches(&mut self, pcs: std::collections::HashSet<usize>) {
        self.pruned_always_taken = pcs;
    }

    /// Round nio21 wave 1 (lane L2): this method's exception table, in table
    /// order, for the compiled local throw edges (see [`IrLocalThrowRow`] and
    /// [`ir_local_throw_edges_enabled`]). Must be called before
    /// [`Self::build`]; never calling it builds exactly what every earlier
    /// build built.
    pub fn set_local_throw_rows(&mut self, rows: Vec<IrLocalThrowRow>) {
        self.local_throw_rows = rows;
    }

    /// t1 wave 10 (lane LCAP2, `CRATONVM_JIT_IR_CONTEXT_ABI_OF_INSTALLED`):
    /// `true` plans this build as if `CRATONVM_JIT_IR_IMPLICIT_EXCEPTION_EDGES`
    /// were off -- the call-site and `athrow` edges are planned exactly as
    /// then. Set by `lib.rs` when the method's published body is context-free
    /// (`r25_w10_2_steady_c1::installed_body_withholds_implicit_edges`): the
    /// edges' pads take the VM context, and a C2 body that does can never take
    /// over that body's baked callers. Never calling it changes nothing.
    pub fn set_implicit_throw_edges_withheld(&mut self, withheld: bool) {
        self.implicit_throw_edges_withheld = withheld;
    }

    /// The interned type-check target of each row of
    /// [`Self::set_local_throw_rows`] (same order; `None` where the row is a
    /// catch-all or did not resolve). The local and spliced throw edges use
    /// these names for runtime subtype tests. Never calling it builds no
    /// runtime-tested throw edge to a typed row.
    pub fn set_local_throw_catch_targets(&mut self, targets: Vec<Option<(usize, usize)>>) {
        self.local_throw_catch_targets = targets;
    }

    /// The addresses of the VM's JVMTI `Exception` and `ExceptionCatch` union
    /// flags when an exception event may be armed while this body runs
    /// (`CompileRequest::exception_events_armed_at_compile`; a listener armed
    /// later does not withdraw it). With `Some`, the method's own local throw
    /// edges are not built (they post nothing), and every spliced throw edge
    /// branches only while both bytes read `0`, deopting at the `athrow`
    /// otherwise: the interpreter's throw posts both events.
    pub fn set_exception_event_guard(&mut self, flags: Option<[usize; 2]>) {
        self.exception_event_guard = flags;
        self.graph.exception_event_guard = flags;
    }

    /// `CRATONVM_JIT_IR_SPLICED_THROW_EDGES`: the first covering row (index and
    /// handler) of the OUTERMOST open splice's `invoke` when an `athrow`
    /// inside the splice may branch to that handler: no open body has a
    /// handler of its own (it would come first), no synchronized splice window
    /// or held monitor (a branch would skip a release), the walk is the pc
    /// walk, the row's catch class is decided or testable, and the handler is
    /// a planned candidate after the `invoke` whose merge is still open.
    pub(in crate::ir) fn spliced_throw_target(&self) -> Option<(usize, usize)> {
        match self.spliced_throw_target_why() {
            Ok(target) => Some(target),
            Err(why) => {
                if ir_bail_reporting() && !self.splice.is_empty() {
                    eprintln!(
                        "[ir] spliced throw edge declined in {}: {why}",
                        self.method_label.as_deref().unwrap_or("<unknown>")
                    );
                }
                None
            }
        }
    }

    /// [`Self::spliced_throw_target`], naming the clause that refuses.
    pub(in crate::ir) fn spliced_throw_target_why(&self) -> Result<(usize, usize), &'static str> {
        if !ir_spliced_throw_edges_enabled() {
            return Err("switch off");
        }
        if self.local_throw_handlers.is_empty() {
            return Err("no planned handler");
        }
        if self.block_walk {
            return Err("block walk");
        }
        if self.sync_splice_open() || !self.monitors.is_empty() {
            return Err("a monitor is held");
        }
        let root = self.splice.first().ok_or("no splice")?;
        if self.splice.iter().any(|f| {
            !f.ranges.is_empty()
                || self
                    .splice_handler_rows
                    .get(&f.base)
                    .is_some_and(|rows| !rows.is_empty())
        }) {
            return Err("a body is walked by ranges or has handlers");
        }
        let p = root.invoke_pc;
        let (idx, row) = self
            .local_throw_rows
            .iter()
            .enumerate()
            .find(|(_, r)| r.start_pc <= p && p < r.end_pc)
            .ok_or("no row covers the outermost invoke")?;
        let testable = match row.catch {
            IrCatchClass::Any => true,
            IrCatchClass::Class(_) => self
                .local_throw_catch_targets
                .get(idx)
                .copied()
                .flatten()
                .is_some(),
            IrCatchClass::Unknown => false,
        };
        if !testable {
            return Err("the row's class is not testable");
        }
        if self.merges.get(&row.handler_pc).is_some_and(|m| m.visited) {
            return Err("the handler's merge was already activated");
        }
        if row.handler_pc <= p || !self.local_throw_handlers.contains(&row.handler_pc) {
            return Err("the handler is not a planned candidate after the invoke");
        }
        Ok((idx, row.handler_pc))
    }

    /// `CRATONVM_JIT_IR_SPLICED_THROW_EDGES`: an `athrow` of `exc` at combined
    /// pc `pc`, inside a spliced body, whose exception the compiling method's
    /// own handler catches when the thrown object passes the first covering
    /// row of the outermost `invoke` ([`Self::spliced_throw_target`]). Builds
    ///
    /// ```text
    /// If(InstanceOf(exc, catch))      // Cmp(exc != null) for a catch-all
    ///   true:  the handler's merge, with the compiling method's locals at
    ///          the outermost invoke and the stack [exc]
    ///   false: Guard(0) at `pc`: a deopt through the chain snapshot at the
    ///          `athrow`, which the interpreter re-executes in the callee's
    ///          own frame (a null, a class no row here takes)
    /// ```
    ///
    /// Exact for the reasons the local throw edge is (JVMS 2.10 takes the
    /// first covering row whose class the exception is an instance of; the
    /// spliced callees have no handler of their own, so the search reaches the
    /// compiling method's table at the outermost `invoke`, the spliced frames
    /// discarded; `athrow` touches no stack trace), with the class decided at
    /// run time by the same type check `instanceof` makes. `false` -- nothing
    /// built -- when there is no target, no chain to deopt through, or `exc`
    /// is a constant null (a masked cold arm, which keeps its own path).
    pub(in crate::ir) fn spliced_throw_edge(&mut self, pc: usize, exc: NodeId) -> Option<usize> {
        // The caller handler is in the root method. Unwinding nested frames
        // needs each parked frame and replay fence restored, so keep this
        // optimization at the one-splice boundary described by this path.
        if self.splice.len() != 1 || exc == NO_NODE {
            return None;
        }
        if self.graph.node_opt(exc).is_some_and(is_null_const) {
            return None;
        }
        let Some((idx, handler_pc)) = self.spliced_throw_target() else {
            return None;
        };
        let Some(ctrl) = self.ctrl_opt() else {
            return None;
        };
        if !ir_site_trap_enabled() || !self.relax_fence_by_chain(pc) {
            if ir_bail_reporting() {
                eprintln!(
                    "[ir] spliced throw edge declined at combined pc {pc}: no chain snapshot"
                );
            }
            return None;
        }
        let Some(root_locals) = self.splice.first().map(|f| f.saved_locals.clone()) else {
            return None;
        };
        let row_catch = self.local_throw_rows[idx].catch;
        // The thrown object's exact class, when a node fact names it: an
        // allocation, or a trusted `static final` (the netty validators'
        // preallocated `VALIDATION_EXCEPTION`). Caught for certain when it is
        // the row's class or the VM answered it a subclass.
        let exact = match self.graph.node_opt(exc).map(|n| &n.op) {
            Some(Op::New { class_id, .. }) => Some(*class_id),
            _ => self.static_final_exact_class_of(exc),
        };
        let test = match row_catch {
            IrCatchClass::Class(c)
                if exact.is_some_and(|e| {
                    e == c || self.local_throw_subclass.get(&(e, c)) == Some(&true)
                }) =>
            {
                None
            }
            IrCatchClass::Class(_) => {
                let Some((name_ptr, name_len)) = self.local_throw_catch_targets[idx] else {
                    return None;
                };
                let test = self.graph.add(
                    Op::InstanceOf {
                        name_ptr,
                        name_len,
                        seen_cell: 0,
                        header_only: false,
                    },
                    IrType::Int,
                    vec![ctrl, self.mem, exc],
                    Some(pc),
                );
                self.mem = test;
                Some(test)
            }
            IrCatchClass::Any => {
                let null = self.aconst_null();
                Some(self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![exc, null], pc))
            }
            IrCatchClass::Unknown => return None,
        };
        // No event may be wanted: both JVMTI union bytes read `0`.
        let test = match self.exception_event_guard {
            Some(flags) => {
                let mut all = test;
                for state_addr in flags {
                    let quiet = self.graph.add(
                        Op::ClassInitTest {
                            state_addr,
                            initialized: 0,
                        },
                        IrType::Int,
                        vec![ctrl],
                        Some(pc),
                    );
                    all = Some(match all {
                        Some(t) => self.add_data(Op::And, IrType::Int, vec![t, quiet], pc),
                        None => quiet,
                    });
                }
                all
            }
            None => test,
        };
        let (caught, missed) = match test {
            Some(test) => {
                let iff = self
                    .graph
                    .add(Op::If, IrType::Control, vec![ctrl, test], Some(pc));
                let caught = self
                    .graph
                    .add(Op::Proj(0), IrType::Control, vec![iff], Some(pc));
                let missed = self
                    .graph
                    .add(Op::Proj(1), IrType::Control, vec![iff], Some(pc));
                (caught, Some(missed))
            }
            None => (ctrl, None),
        };
        self.ctrl = caught;
        let saved_locals = std::mem::replace(&mut self.locals, root_locals);
        let saved_stack = std::mem::replace(&mut self.stack, vec![exc]);
        self.add_merge_predecessor(handler_pc);
        self.locals = saved_locals;
        self.stack = saved_stack;
        if let Some(missed) = missed {
            let zero = self.iconst(0);
            self.graph.add(
                Op::Guard { bci: pc },
                IrType::Void,
                vec![missed, zero],
                Some(pc),
            );
            // The terminator every path needs, as behind
            // `trap_athrow_in_monitor`'s guard: unreachable at run time.
            let throw = self.graph.add(
                Op::Throw,
                IrType::Void,
                vec![missed, self.mem, exc],
                Some(pc),
            );
            self.graph.exit = throw;
        }
        self.ctrl = NO_NODE;
        self.spliced_throw_edges += 1;
        if ir_bail_reporting() {
            eprintln!(
                "[ir] spliced throw edge at combined pc {pc} in {} -> handler {handler_pc} (row {idx})",
                self.method_label.as_deref().unwrap_or("<unknown>")
            );
        }
        Some(handler_pc)
    }

    /// Close a root-level single splice whose only compiled exit is a
    /// `spliced_throw_edge`, then continue the bytecode walk at its caller
    /// handler. The edge already contributed root locals and the exception to
    /// the handler merge; restoring the parked caller frame lets the ordinary
    /// merge activation build the handler exactly once.
    pub(in crate::ir) fn finish_spliced_throw(&mut self, handler_pc: usize) -> Option<usize> {
        let frame = self.splice.pop()?;
        if frame.sync_monitor.is_some() || !self.splice.is_empty() {
            return None;
        }
        self.release_ctor_receiver(frame.ctor_receiver);
        if self.field_memo_in_splices {
            self.field_load_memo_clear_from_depth(1);
        }
        self.locals = frame.saved_locals;
        self.stack = frame.saved_stack;
        self.splice_null_facts.clear();
        self.splice_local.clear();
        self.splice_tainted.clear();
        self.splice_edge_null_facts.clear();
        self.splice_committed_bases.clear();
        self.splice_call_committed = false;
        Some(handler_pc)
    }

    /// JIT round 20 wave 11 (lane pIR, L2-3 / L2W2-4): the subclass answers
    /// the local throw edges may use to decide a typed catch row whose class
    /// is not the thrown class itself (see [`Self::local_throw_target`]).
    /// Each entry is `(thrown class id, catch class id) -> caught`, answered
    /// by the VM from the loaded hierarchy exactly as its handler search
    /// answers it. Never calling it (or an empty map) builds exactly what
    /// every earlier build built.
    pub fn set_local_throw_subclass_facts(&mut self, facts: HashMap<(u32, u32), bool>) {
        self.local_throw_subclass = facts;
    }

    /// Round nio21 wave 3 (lane L2): the catch-type NAME of each row handed to
    /// [`Self::set_local_throw_rows`] (same order; `""` for a catch-all) and
    /// the compiling method's declaring class, which arm the call-site
    /// exception edges ([`Op::CallThrew`]). `lib.rs` calls it only under
    /// [`ir_call_site_exception_edges_enabled`] and only when every row's name
    /// resolved; a list whose length differs from the rows' plans no call
    /// edge. Never calling it builds exactly what wave 2 built.
    pub fn set_call_throw_catch_names(
        &mut self,
        names: Vec<&'static str>,
        declaring_class_id: u32,
    ) {
        self.call_throw_names = names;
        self.call_throw_declaring_class = declaring_class_id;
    }

    /// nio21 wave 4 (lane L2): may this build splice a callee body that
    /// carries admitted handler rows ([`IrInlineTables::splice_handler_rows`])?
    /// `lib.rs` sets it under the gate it plans such a site under
    /// ([`ir_splice_callee_handlers_enabled`] and the call-site edges' own
    /// conditions). Never calling it refuses every such body at its `invoke`.
    pub fn set_splice_handler_edges(&mut self, enabled: bool) {
        self.splice_handler_edges = enabled;
    }

    /// Plant the call-site exception edges of the `Op::Call` just built at
    /// `pc` (round nio21 wave 3, lane L2; see [`Op::CallThrew`]). Called by
    /// the three plain invoke arms after `self.mem = call` and BEFORE the
    /// result is pushed, with no caller-held synchronized monitor pair.
    ///
    /// For candidate `k` (the rows covering `pc`, table order) it builds
    /// `t = CallThrew { site, k }` `[ctrl, call]`, `If(t)`, and on the true
    /// edge the JVMS 2.10 handler entry -- the operand stack cleared to
    /// `[CaughtException]`, the locals and memory as they stand after the call
    /// -- as a predecessor of handler `k`'s merge; the false edge carries on
    /// to candidate `k + 1`, and the last one is the call's normal
    /// continuation. Nothing is built (the call keeps today's exit) unless the
    /// planner chose this pc, the code is the method's own (no splice open,
    /// `pc < code_len`), the frame holds no monitor, control is live and
    /// every candidate's merge is still open.
    pub(in crate::ir) fn plant_call_throw_edges(&mut self, pc: usize, call: NodeId, code_len: usize) {
        if self.call_throw_planned.is_empty() || !self.monitors.is_empty() || self.ctrl == NO_NODE {
            return;
        }
        let spliced = !self.splice.is_empty();
        // A nested call inherits the table at the invoke which opened its body.
        // The saved locals in the next frame belong to that table's scope.
        // Covered nested bodies have no tables of their own; decline any chain
        // with multiple covering scopes rather than skip an intervening catch.
        let mut scope = None;
        for depth in (0..self.splice.len()).rev() {
            let frame = &self.splice[depth];
            let at = self
                .splice
                .get(depth + 1)
                .map_or(pc, |child| child.invoke_pc);
            if at < frame.base || at >= frame.end {
                return;
            }
            if let Some(rows) = self.splice_handler_rows.get(&frame.base) {
                let covering: Vec<_> = rows
                    .iter()
                    .filter(|r| r.start_pc <= at && at < r.end_pc)
                    .collect();
                if !covering.is_empty() {
                    if scope.is_some() {
                        return;
                    }
                    scope = Some((depth, at));
                    if covering
                        .iter()
                        .any(|r| r.catch_name.is_empty() || r.catch_name == "java/lang/Throwable")
                    {
                        break;
                    }
                }
            }
        }
        let (candidates, declaring_class_id, planned_pc, handler_locals): (
            Vec<(&'static str, u32)>,
            u32,
            usize,
            Vec<NodeId>,
        ) = if spliced {
            let Some((depth, at)) = scope else {
                return;
            };
            if depth + 1 < self.splice.len() && !ir_splice_covered_nested_guarded_enabled() {
                return;
            }
            let rows = &self.splice_handler_rows[&self.splice[depth].base];
            let covering: Vec<_> = rows
                .iter()
                .filter(|r| r.start_pc <= at && at < r.end_pc)
                .collect();
            let declaring = covering[0].declaring_class_id;
            if covering.iter().any(|r| r.declaring_class_id != declaring) {
                return;
            }
            (
                covering
                    .iter()
                    .map(|r| (r.catch_name, r.handler_pc as u32))
                    .collect(),
                declaring,
                at,
                self.splice
                    .get(depth + 1)
                    .map_or_else(|| self.locals.clone(), |child| child.saved_locals.clone()),
            )
        } else {
            if pc >= code_len {
                return;
            }
            (
                self.local_throw_rows
                    .iter()
                    .zip(self.call_throw_names.iter())
                    .filter(|(r, _)| r.start_pc <= pc && pc < r.end_pc)
                    .map(|(r, &name)| (name, r.handler_pc as u32))
                    .collect(),
                self.call_throw_declaring_class,
                pc,
                self.locals.clone(),
            )
        };
        let Some(handlers) = self.call_throw_planned.get(&planned_pc).cloned() else {
            return;
        };
        if handlers.is_empty()
            || handlers
                .iter()
                .any(|h| self.merges.get(h).is_some_and(|m| m.visited))
        {
            return;
        }
        if candidates.len() != handlers.len()
            || candidates
                .iter()
                .zip(handlers.iter())
                .any(|(&(_, c), &h)| c as usize != h)
        {
            return;
        }
        let Ok(site) = u32::try_from(self.graph.call_throw_sites.len()) else {
            return;
        };
        self.graph.call_throw_sites.push(IrCallThrowSite {
            // Cast: a bci (or a combined-buffer pc, bounded by
            // `IR_MAX_BYTECODE_SIZE`) fits u32.
            bci: pc as u32,
            declaring_class_id,
            candidates,
            spliced,
        });
        let saved_stack = self.stack.clone();
        let saved_locals = self.locals.clone();
        let mut normal = self.ctrl;
        for (k, &h) in handlers.iter().enumerate() {
            // Cast: `k < IR_CALL_THROW_MAX_CANDIDATES`.
            let threw = self.graph.add(
                Op::CallThrew {
                    site,
                    index: k as u16,
                },
                IrType::Int,
                vec![normal, call],
                Some(pc),
            );
            let iff = self
                .graph
                .add(Op::If, IrType::Control, vec![normal, threw], Some(pc));
            let caught = self
                .graph
                .add(Op::Proj(0), IrType::Control, vec![iff], Some(pc));
            normal = self
                .graph
                .add(Op::Proj(1), IrType::Control, vec![iff], Some(pc));
            // No bytecode pc: the node anchors no bci (`note_bci_anchor`), and
            // it cannot deopt or reach a safepoint.
            let exc = self
                .graph
                .add(Op::CaughtException, IrType::Ref, vec![caught], None);
            self.ctrl = caught;
            self.stack.clear();
            self.push(exc);
            self.locals = handler_locals.clone();
            self.add_merge_predecessor(h);
            self.locals = saved_locals.clone();
            self.stack = saved_stack.clone();
        }
        self.ctrl = normal;
        self.call_throw_edges += 1;
        if spliced {
            self.splice_handler_planted_calls.insert(call);
        }
    }

    /// Plant the IMPLICIT exception edges of the trapping node `anchor` just
    /// built at `pc` (round nio21 wave 5, lane L2;
    /// [`ir_implicit_exception_edges_enabled`]): an `ArrayLoad` / `ArrayStore`
    /// (null and bounds checks) or the zero-divisor `Op::Guard` of an integer
    /// division. Called BEFORE the node's own effect is recorded (`self.mem`
    /// still the token before an access, the result not pushed), with
    /// `mem_before` the memory state the instruction found.
    ///
    /// The graph is the call-site edges' ([`Op::CallThrew`], one site of
    /// [`Graph::call_throw_sites`], one flag + `If` per covering row), with
    /// `anchor` in place of the call: `t = CallThrew { site, k }` `[ctrl,
    /// anchor]`. The true edge carries the JVMS 2.10 handler entry -- stack
    /// `[CaughtException]`, the locals as they stand (none of these
    /// instructions writes one) and `mem_before`, because the trapping
    /// instruction has NOT executed: a handler must not see an array store
    /// that never happened, nor have a load of the same element forwarded from
    /// it. The lowerer arms the site only when the node's own guard emitter
    /// reaches it (`ir_lower::Lowerer::plan_implicit_throw_site`); an unarmed
    /// site's `If` reads the flag's zero and the guard deopts as before.
    ///
    /// Nothing is built outside the method's own code (a splice open or
    /// `pc >= code_len`), with a monitor held, with dead control, or unless
    /// the planner chose `pc` and every candidate's merge is still open.
    pub(in crate::ir) fn plant_implicit_throw_edges(
        &mut self,
        pc: usize,
        anchor: NodeId,
        mem_before: NodeId,
        code_len: usize,
    ) {
        if self.implicit_throw_planned.is_empty()
            || anchor == NO_NODE
            || !self.splice.is_empty()
            || pc >= code_len
            || !self.monitors.is_empty()
            || self.ctrl == NO_NODE
        {
            return;
        }
        // A division by a non-zero constant cannot trap, and the optimizer
        // folds its guard away: an edge hung on it would only dangle.
        if let Some(cond) = self
            .graph
            .node_opt(anchor)
            .filter(|n| matches!(n.op, Op::Guard { .. }))
            .and_then(|n| n.inputs.get(1).copied())
        {
            let nonzero_const = self.graph.node_opt(cond).is_some_and(|c| {
                c.inputs.iter().any(|&i| {
                    matches!(self.graph.node_opt(i).map(|x| &x.op), Some(Op::Const(v)) if *v != 0)
                })
            });
            if nonzero_const {
                return;
            }
        }
        let Some(handlers) = self.implicit_throw_planned.get(&pc).cloned() else {
            return;
        };
        if handlers.is_empty()
            || handlers
                .iter()
                .any(|h| self.merges.get(h).is_some_and(|m| m.visited))
        {
            return;
        }
        let candidates: Vec<(&'static str, u32)> = self
            .local_throw_rows
            .iter()
            .zip(self.call_throw_names.iter())
            .filter(|(r, _)| r.start_pc <= pc && pc < r.end_pc)
            // Cast: a handler pc is a bci, which fits u32 by classfile limits.
            .map(|(r, &name)| (name, r.handler_pc as u32))
            .collect();
        if candidates.len() != handlers.len()
            || candidates
                .iter()
                .zip(handlers.iter())
                .any(|(&(_, c), &h)| c as usize != h)
        {
            return;
        }
        let Ok(site) = u32::try_from(self.graph.call_throw_sites.len()) else {
            return;
        };
        self.graph.call_throw_sites.push(IrCallThrowSite {
            // Cast: a bci fits u32 (`IR_MAX_BYTECODE_SIZE`).
            bci: pc as u32,
            declaring_class_id: self.call_throw_declaring_class,
            candidates,
            spliced: false,
        });
        let saved_stack = self.stack.clone();
        let saved_mem = self.mem;
        let mut normal = self.ctrl;
        for (k, &h) in handlers.iter().enumerate() {
            // Cast: `k < IR_CALL_THROW_MAX_CANDIDATES`.
            let threw = self.graph.add(
                Op::CallThrew {
                    site,
                    index: k as u16,
                },
                IrType::Int,
                vec![normal, anchor],
                Some(pc),
            );
            let iff = self
                .graph
                .add(Op::If, IrType::Control, vec![normal, threw], Some(pc));
            let caught = self
                .graph
                .add(Op::Proj(0), IrType::Control, vec![iff], Some(pc));
            normal = self
                .graph
                .add(Op::Proj(1), IrType::Control, vec![iff], Some(pc));
            let exc = self
                .graph
                .add(Op::CaughtException, IrType::Ref, vec![caught], None);
            self.ctrl = caught;
            self.mem = mem_before;
            self.stack.clear();
            self.push(exc);
            self.add_merge_predecessor(h);
            self.stack = saved_stack.clone();
        }
        self.ctrl = normal;
        self.mem = saved_mem;
        self.implicit_throw_edges += 1;
    }

    /// nio21 wave 6 L2: can the field-access receiver `base` never be null --
    /// an allocation or constant-pool object
    /// ([`crate::ir_check_elim::definitely_non_null`]) or the receiver
    /// parameter? Such a `getfield` / `putfield` plants no implicit edge: its
    /// null check is never emitted (`ir_lower::store_receiver_cannot_be_null`)
    /// or never fails, so the edge could only split the block.
    pub(in crate::ir) fn receiver_cannot_be_null(&self, base: NodeId) -> bool {
        self.graph.node_opt(base).is_some_and(|b| {
            crate::ir_check_elim::definitely_non_null(&b.op)
                || matches!(b.op, Op::Param(p) if Some(p) == self.graph.receiver_param)
        })
    }

    /// nio21 wave 4 L2: is `pc` inside a `try` range of the innermost open
    /// splice's admitted handler rows ([`ir_splice_callee_handlers_enabled`])?
    pub(in crate::ir) fn pc_in_splice_handler_cover(&self, pc: usize) -> bool {
        self.splice
            .last()
            .and_then(|f| self.splice_handler_rows.get(&f.base))
            .is_some_and(|rows| rows.iter().any(|r| r.start_pc <= pc && pc < r.end_pc))
    }

    /// nio21 wave 4 L2: did the instruction whose nodes start at `mark` (one
    /// inside a spliced callee's `try` range) build nothing that leaves the
    /// frame except a call with planted edges, or a guard (which deopts into
    /// the callee's own frame, whose interpreter runs the real table)? Any
    /// other exit -- an unplanted call, a throw, a field or array access, an
    /// allocation -- would leave through the CALLER's `invoke` and skip the
    /// callee's `catch`, so the build refuses the splice.
    ///
    /// `at` is the instruction's combined pc (j27 wave 7, lane T1L1: an
    /// allocation there under rows that cannot catch an `Error`,
    /// [`Self::splice_handler_new_admitted`]).
    pub(in crate::ir) fn splice_handler_nodes_armed(&self, mark: usize, at: usize, code: &[u8]) -> bool {
        let new_admitted = self.splice_handler_new_admitted(at, code);
        let unarmed = self
            .graph
            .nodes
            .iter()
            .enumerate()
            .skip(mark)
            .find(|&(id, n)| match &n.op {
                // Cast: node ids index `graph.nodes`, bounded by the builder.
                Op::Call { .. } => !self.splice_handler_planted_calls.contains(&(id as NodeId)),
                Op::Guard { .. } | Op::Dead => false,
                // JIT r24 (CompletableFuture page, `uniApplyNow`'s `try`): a
                // FIELD access whose base is proven non-null cannot raise --
                // its only exception is the base's NPE -- so it has no exit
                // to arm. `d.result = ..` on the fresh `d` is one.
                Op::Load(_) | Op::Store(_) if ir_splice_handler_non_null_field_access_enabled() => {
                    !n.input_opt(2)
                        .is_some_and(|b| self.splice_handler_base_non_null(b))
                }
                Op::Throw => !self.graph.splice_handler_proven_exits.contains(&(id as NodeId)),
                // nio21 wave 4 L2: array reads in a covered callee are safe
                // only with the opt-in lowerer proof. Their null/bounds
                // failures deopt at the original bytecode, letting that
                // callee's interpreter run the applicable exception table.
                // The lowerer records these exact nodes before it waives the
                // covered-handler refusal; stores remain unarmed.
                Op::InstanceOf { .. } => false,
                Op::Unbox {
                    op: UnboxOp::IntValue | UnboxOp::LongValue,
                    ..
                } if ir_splice_handler_lower_proven_enabled() => false,
                Op::ArrayLoad(_) if ir_splice_handler_lower_proven_enabled() => false,
                Op::ArrayLength => {
                    let guard = guard_token_of(n);
                    !ir_splice_handler_lower_proven_enabled()
                        && !self.graph.node_opt(guard)
                            .is_some_and(|g| matches!(g.op, Op::Guard { bci } if bci == at))
                }
                // Reading a resolved static of an active splice's declaring
                // class cannot start initialization. Instance/static entry (or
                // the allocation before a constructor) already crossed it.
                Op::LoadStatic {
                    class_id,
                    field_index,
                    is_volatile: false,
                    ..
                } => !(self.static_final_exact_classes.contains_key(&(*class_id, *field_index))
                    || self.splice.iter().any(|frame| {
                        self.inline_sites.get(&frame.invoke_pc)
                            .is_some_and(|site| site.class_id == *class_id)
                    })),
                // j27 wave 7 (lane T1L1, `CRATONVM_JIT_IR_SPLICE_COVERED_NEW_NON_ERROR_CATCH`):
                // an allocation raises only `Error`s, which no covering row catches.
                Op::New { .. } | Op::ConstString { .. } if new_admitted => false,
                Op::NewArray { .. } if new_admitted => {
                    self.splice_handler_newarray_non_negative(n).is_none()
                }
                Op::ArrayStore(kind) if self.splice_handler_array_store_safe(n, *kind) => false,
                op => may_raise(op),
            });
        if let Some((id, n)) = unarmed {
            if ir_bail_reporting() {
                eprintln!(
                    "[ir] spliced try range: n{id} {:?} at combined pc {:?} leaves the frame unarmed in {}",
                    n.op,
                    n.bytecode_pc,
                    self.method_label.as_deref().unwrap_or("<unknown>")
                );
            }
            return false;
        }
        true
    }

    /// JIT r24: is `base` proven non-null where
    /// [`Self::splice_handler_nodes_armed`] asks? An allocation or the
    /// method's own receiver ([`Self::receiver_cannot_be_null`]), the
    /// receiver of an open splice (null-checked at its call), a base an
    /// earlier committed store wrote through, or a value a dominating null
    /// test proved ([`Self::splice_null_facts`]).
    pub(in crate::ir) fn splice_handler_newarray_non_negative(&self, n: &Node) -> Option<i32> {
        let len_id = n.inputs.get(2).copied().unwrap_or(NO_NODE);
        let Some(Op::Const(len)) = self.graph.node_opt(len_id).map(|m| &m.op) else {
            return None;
        };
        i32::try_from(*len).ok().filter(|&v| v >= 0)
    }

    pub(in crate::ir) fn splice_handler_array_store_safe(&self, n: &Node, kind: MemKind) -> bool {
        let arr_id = n.inputs.get(2).copied().unwrap_or(NO_NODE);
        let idx_id = n.inputs.get(3).copied().unwrap_or(NO_NODE);
        let Some(arr_node) = self.graph.node_opt(arr_id) else {
            return false;
        };
        let Op::NewArray {
            element_type,
            component_class_id,
        } = arr_node.op
        else {
            return false;
        };
        let Some(len) = self.splice_handler_newarray_non_negative(arr_node) else {
            return false;
        };
        let Some(Op::Const(idx)) = self.graph.node_opt(idx_id).map(|m| &m.op) else {
            return false;
        };
        let Ok(idx) = i32::try_from(*idx) else { return false; }; if idx < 0 || idx >= len {
            return false;
        }
        if kind == MemKind::Ref {
            element_type == 0
                && (component_class_id == 0 || Some(component_class_id) == self.object_class_id)
        } else {
            element_type != 0
        }
    }

    pub(in crate::ir) fn splice_handler_base_non_null(&self, base: NodeId) -> bool {
        base != NO_NODE
            && (self.receiver_cannot_be_null(base)
                || self.splice.iter().any(|f| f.receiver == base)
                || self.splice_committed_bases.contains(&base)
                || self.splice_null_facts.contains(&base))
    }

    /// nio21 wave 4 L2: the first instruction of a spliced callee's handler,
    /// reached with live control only through a call-site edge
    /// ([`Self::plant_call_throw_edges`]) and the stack `[throwable]`. The
    /// handler itself is not compiled (its copy is `nop`s,
    /// [`splice_callee_handler_plan`]): an always-failing guard resumes the
    /// interpreter HERE through the chain snapshot the walk just pushed --
    /// the callee at the handler's bci with the throwable on its stack, every
    /// caller parked in its `invoke` -- and the interpreter runs the handler
    /// with the method's own code. Nothing before it is replayed. `false`
    /// (the build refuses, which the planner charges to this splice) without
    /// the chain, inside a synchronized splice window, or with site traps off.
    pub(in crate::ir) fn plant_splice_handler_trap(&mut self, pc: usize) -> bool {
        if !ir_site_trap_enabled() || self.splice.is_empty() || self.sync_splice_open() {
            return false;
        }
        let Some(ctrl) = self.ctrl_opt() else {
            return false;
        };
        if !self.relax_fence_by_chain(pc) {
            return false;
        }
        let zero = self.iconst(0);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, zero],
            Some(pc),
        );
        // JIT round 23 wave 10 (lane L1, CompletableFuture page;
        // `CRATONVM_JIT_IR_SPLICE_HANDLER_TRAP_THROW`, correctness, default
        // ON, `=0` restores the bare guard): the caught path's block needs a
        // terminator. The guard always fires, but it is not a control node,
        // so with nothing after it the block reached through the call-site
        // edge (`Proj(0)` of the `CallThrew` test) had no terminator and no
        // successor, and `ir_lower` refused the whole method ("a reachable
        // block with no terminator and no successor":
        // `CompletableFuture.reportJoin` / `uniApplyNow` under the
        // callee-handler family). An `Op::Throw` of the caught throwable is
        // built behind the guard, unreachable at run time -- the same
        // terminator the always-failing athrow traps build
        // (`plant_spliced_athrow_exit`, `trap_athrow_in_monitor`).
        if ir_splice_handler_trap_throw_enabled() {
            if let Some(&exc) = self.stack.last() {
                let throw = self.graph.add(
                    Op::Throw,
                    IrType::Void,
                    vec![ctrl, self.mem, exc],
                    Some(pc),
                );
                if ir_splice_handler_lower_proven_enabled() {
                    self.graph.splice_handler_proven_exits.push(throw);
                    self.graph.splice_handler_proven_pcs.push(pc);
                }
                self.graph.exit = throw;
            }
        }
        self.ctrl = NO_NODE;
        self.stack.clear();
        if ir_bail_reporting() {
            eprintln!(
                "[ir] spliced handler entered at combined pc {pc} in {} -- resumes in the interpreter",
                self.method_label.as_deref().unwrap_or("<unknown>")
            );
        }
        true
    }

    /// The reachable set the walk uses, widened by the handler bodies the
    /// local throw edges may enter (round nio21 wave 1, lane L2).
    ///
    /// `base` is [`normally_reachable_pcs`]. A handler is a CANDIDATE when
    /// some normally reachable `athrow` of the caller's own code has it as the
    /// handler of its FIRST covering row (JVMS 2.10 order), the row's catch
    /// class is decidable at compile time ([`IrCatchClass::Unknown`] is not),
    /// and the handler lies AFTER the `athrow` (the pc walk activates a forward
    /// merge only once every forward predecessor is in; a handler at or before
    /// its `athrow` would be a back edge onto a block the walk has passed). The
    /// class match itself is decided per site during the walk
    /// ([`Self::local_throw_target`]); a candidate no `athrow` enters is
    /// stepped over (`local_throw_optional`).
    ///
    /// The widening is DROPPED (this returns `base`, and the build is exactly
    /// the pre-nio21 one) when any of these holds, each a shape the walk's
    /// invariants were not written for:
    ///
    /// * the method needs the block walk, with or without the handlers (a
    ///   rotated loop: the reverse post-order is computed over normal edges);
    /// * the handler code contains a loop header (an OSR entry or a header φ
    ///   inside code that may never be entered);
    /// * a handler is itself normally reachable (a merge the walk may already
    ///   have passed through a fall-through);
    /// * this is an OSR route build ([`Self::osr_requested_header`]).
    pub(in crate::ir) fn plan_local_throw_reachability(
        &mut self,
        verified: &cratonvm_reader::VerifiedCode,
        base: HashSet<usize>,
        code: &[u8],
        code_len: usize,
    ) -> HashSet<usize> {
        self.local_throw_handlers.clear();
        self.local_throw_optional.clear();
        self.local_throw_edges = 0;
        self.spliced_throw_edges = 0;
        self.call_throw_planned.clear();
        self.call_throw_edges = 0;
        self.implicit_throw_planned.clear();
        self.implicit_throw_edges = 0;
        if self.local_throw_rows.is_empty()
            || (self.osr_requested_header.is_some() && !ir_spliced_throw_edges_enabled())
        {
            return base;
        }
        let athrow =
            local_throw_candidate_handlers(verified, &base, code, code_len, &self.local_throw_rows);
        // `CRATONVM_JIT_IR_SPLICED_THROW_EDGES`: the handler of the first
        // covering row of every planned splice site, planned WITH the athrow
        // candidates first; when that union is refused, everything below runs
        // exactly as before, so a spliced candidate can never cost a method
        // its own edges.
        if ir_spliced_throw_edges_enabled() {
            let spliced = spliced_throw_candidate_handlers(
                verified,
                &base,
                code_len,
                &self.local_throw_rows,
                &self.inline_sites,
            );
            if spliced.difference(&athrow).next().is_some() {
                let mut with = athrow.clone();
                with.extend(spliced.iter().copied());
                let widened = widen_for_local_handlers(verified, &base, &with, code_len);
                if ir_bail_reporting() {
                    eprintln!(
                        "[ir] spliced throw candidates in {}: handlers {:?} {}",
                        self.method_label.as_deref().unwrap_or("<method>"),
                        spliced,
                        if widened.is_some() { "planned" } else { "refused by the widening" },
                    );
                }
                if let Some((extended, optional)) = widened {
                    self.local_throw_handlers = with;
                    self.local_throw_optional = optional;
                    return extended;
                }
            } else if ir_bail_reporting() {
                eprintln!(
                    "[ir] spliced throw candidates in {}: none ({} row(s), {} site(s))",
                    self.method_label.as_deref().unwrap_or("<method>"),
                    self.local_throw_rows.len(),
                    self.inline_sites.len(),
                );
            }
        }
        // JIT round 20 wave 11 (lane pIR, L2-3 / L2W2-4): with subclass
        // answers in hand, a typed row the VM says certainly does not match is
        // skipped, so a LATER covering row's handler may be the target too.
        // Those handlers are tried on top of every widening below; a widening
        // they make refuse is retried without them, exactly as before, so they
        // can never cost a method an edge it had.
        let extra: HashSet<usize> = if self.local_throw_subclass.is_empty() {
            HashSet::new()
        } else {
            local_throw_later_row_handlers(verified, &base, code, code_len, &self.local_throw_rows)
                .difference(&athrow)
                .copied()
                .collect()
        };
        let widen = |set: &HashSet<usize>| {
            if !extra.is_empty() {
                let mut with = set.clone();
                with.extend(extra.iter().copied());
                if let Some((extended, optional)) =
                    widen_for_local_handlers(verified, &base, &with, code_len)
                {
                    return Some((with, extended, optional));
                }
            }
            widen_for_local_handlers(verified, &base, set, code_len)
                .map(|(extended, optional)| (set.clone(), extended, optional))
        };
        // nio21 wave 3 L2: the invokes whose every covering row's handler lies
        // after them (`call_site_throw_plan`), under the same widening rules.
        // Planned WITH the athrow candidates first; when the union is refused,
        // the athrow edges alone are planned exactly as before, so a call-site
        // candidate can never cost a method its wave-1 edges.
        let calls = if self.call_throw_names.len() == self.local_throw_rows.len() {
            call_site_throw_plan(verified, &base, code, code_len, &self.local_throw_rows)
        } else {
            HashMap::new()
        };
        // nio21 wave 5 L2: the implicit sites (array accesses, integer
        // divisions) under `CRATONVM_JIT_IR_IMPLICIT_EXCEPTION_EDGES`, planned
        // WITH everything above first; when that union is refused, the plan
        // below runs exactly as wave 4's did, so an implicit candidate can never
        // cost a method its call-site or athrow edges.
        let implicit = if ir_implicit_exception_edges_enabled()
            && !self.implicit_throw_edges_withheld
            && self.call_throw_names.len() == self.local_throw_rows.len()
        {
            implicit_site_throw_plan(
                verified,
                &base,
                code,
                code_len,
                &self.local_throw_rows,
                &self.call_throw_names,
            )
        } else {
            HashMap::new()
        };
        if !implicit.is_empty() {
            let mut union = athrow.clone();
            for hs in calls.values().chain(implicit.values()) {
                union.extend(hs.iter().copied());
            }
            if let Some((union, extended, optional)) = widen(&union) {
                self.local_throw_handlers = union;
                self.local_throw_optional = optional;
                self.call_throw_planned = calls;
                self.implicit_throw_planned = implicit;
                return extended;
            }
        }
        if !calls.is_empty() {
            let mut union = athrow.clone();
            for hs in calls.values() {
                union.extend(hs.iter().copied());
            }
            if let Some((union, extended, optional)) = widen(&union) {
                self.local_throw_handlers = union;
                self.local_throw_optional = optional;
                self.call_throw_planned = calls;
                return extended;
            }
        }
        match widen(&athrow) {
            Some((athrow, extended, optional)) => {
                self.local_throw_handlers = athrow;
                self.local_throw_optional = optional;
                extended
            }
            None => base,
        }
    }

    /// The handler an `athrow` at `pc` of `exc` branches to, or `None` for
    /// the shared exception exit (`Op::Throw`) every earlier build took.
    ///
    /// The soundness argument is on the page
    /// `docs/feature-designs/jit-ir-optimizations.md`
    /// (nio21 wave 1 L2). In short, the edge is taken only when the answer the
    /// interpreter's handler search would give is decided at compile time:
    ///
    /// * the thrown value is an `Op::New` of this method, or a trusted
    ///   non-null `static final` reference with an exact-class fact (an
    ///   object's class never changes);
    /// * the FIRST row covering `pc` in table order is a catch-all, or names
    ///   the very same class id (both resolved through this method's own
    ///   constant pool and loader, so the same class, not just the same name);
    ///   any other first row -- a different or unresolved class -- is not
    ///   decided here, and the throw leaves as before (JIT round 20 wave 11,
    ///   lane pIR: unless the VM's subclass answers decide it, see
    ///   [`Self::local_throw_superclass_row`]);
    /// * the code is the caller's own (`pc < code_len`, no splice open: a
    ///   spliced body's exception table is its callee's);
    /// * the handler is a planned candidate, lies after `pc`, and its merge
    ///   has not been activated yet.
    pub(in crate::ir) fn local_throw_target(&self, pc: usize, code_len: usize, exc: NodeId) -> Option<usize> {
        if self.local_throw_handlers.is_empty()
            || !self.splice.is_empty()
            || pc >= code_len
        {
            return None;
        }
        let class_id = match self.graph.node_opt(exc).map(|n| &n.op) {
            Some(Op::New { class_id, .. }) => *class_id,
            _ => self.static_final_exact_class_of(exc)?,
        };
        let row = if self.local_throw_subclass.is_empty() {
            self.local_throw_rows
                .iter()
                .find(|r| r.start_pc <= pc && pc < r.end_pc)?
        } else {
            self.local_throw_superclass_row(pc, class_id)?
        };
        let caught = match row.catch {
            IrCatchClass::Any => true,
            IrCatchClass::Class(c) => {
                c == class_id || self.local_throw_subclass.get(&(class_id, c)) == Some(&true)
            }
            IrCatchClass::Unknown => false,
        };
        let open = !self.merges.get(&row.handler_pc).is_some_and(|m| m.visited);
        let decided = caught
            && open
            && row.handler_pc > pc
            && self.local_throw_handlers.contains(&row.handler_pc);
        // JIT round 20 wave 11 (lane pIR): the positive control of the
        // superclass decision (`CRATONVM_DBG_JITC=1`).
        if decided && ir_bail_reporting() {
            if let IrCatchClass::Class(c) = row.catch {
                if c != class_id {
                    eprintln!(
                        "[ir] local throw superclass catch in {}: athrow at pc {} of class id {} \
                         branches to handler {} (catch class id {})",
                        self.method_label.as_deref().unwrap_or("<method>"),
                        pc,
                        class_id,
                        row.handler_pc,
                        c,
                    );
                }
            }
        }
        decided.then_some(row.handler_pc)
    }

    /// Build a local handler edge when the exception value's exact class is
    /// not known during compilation. Restrict this first runtime path to one
    /// covering, walkable row; its miss replays the original `athrow` in the
    /// interpreter, which preserves null-throw and exception-table behavior.
    pub(in crate::ir) fn local_throw_dynamic_edge(
        &mut self,
        pc: usize,
        code_len: usize,
        exc: NodeId,
    ) -> Option<usize> {
        if self.local_throw_handlers.is_empty()
            || !self.splice.is_empty()
            || pc >= code_len
            || self.graph.node_opt(exc).is_some_and(is_null_const)
        {
            return None;
        }
        let mut covering = self
            .local_throw_rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.start_pc <= pc && pc < row.end_pc);
        let (idx, row) = covering.next()?;
        let handler_pc = row.handler_pc;
        let catch = row.catch;
        if covering.next().is_some()
            || handler_pc <= pc
            || handler_pc >= code_len
            || !self.local_throw_handlers.contains(&handler_pc)
            || self.merges.get(&handler_pc).is_some_and(|m| m.visited)
        {
            return None;
        }
        drop(covering);
        let Some(ctrl) = self.ctrl_opt() else {
            return None;
        };
        let mut test = match catch {
            IrCatchClass::Class(_) => {
                let (name_ptr, name_len) = self
                    .local_throw_catch_targets
                    .get(idx)
                    .copied()
                    .flatten()?;
                let test = self.graph.add(
                    Op::InstanceOf {
                        name_ptr,
                        name_len,
                        seen_cell: 0,
                        header_only: false,
                    },
                    IrType::Int,
                    vec![ctrl, self.mem, exc],
                    Some(pc),
                );
                self.mem = test;
                test
            }
            IrCatchClass::Any => {
                let null = self.aconst_null();
                self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![exc, null], pc)
            }
            IrCatchClass::Unknown => return None,
        };
        // Keep JVMTI's exception and exception-catch notifications on the
        // interpreter path. Folding the live event bytes into the match
        // condition makes either an armed event or a type miss replay the
        // original `athrow` without creating a second graph exit.
        if let Some(flags) = self.exception_event_guard {
            for state_addr in flags {
                let quiet = self.graph.add(
                    Op::ClassInitTest {
                        state_addr,
                        initialized: 0,
                    },
                    IrType::Int,
                    vec![ctrl],
                    Some(pc),
                );
                test = self.add_data(Op::And, IrType::Int, vec![test, quiet], pc);
            }
        }
        let iff = self
            .graph
            .add(Op::If, IrType::Control, vec![ctrl, test], Some(pc));
        let caught = self
            .graph
            .add(Op::Proj(0), IrType::Control, vec![iff], Some(pc));
        let missed = self
            .graph
            .add(Op::Proj(1), IrType::Control, vec![iff], Some(pc));

        self.ctrl = caught;
        self.stack.clear();
        self.push(exc);
        self.add_merge_predecessor(handler_pc);

        // A non-match (including `athrow null`, whose `instanceof` is false)
        // must retain the original throwing semantics. Re-execute that bci
        // rather than propagating through an incompletely modeled IR edge.
        let zero = self.iconst(0);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![missed, zero],
            Some(pc),
        );
        let throw = self.graph.add(
            Op::Throw,
            IrType::Void,
            vec![missed, self.mem, exc],
            Some(pc),
        );
        self.graph.exit = throw;
        self.ctrl = NO_NODE;
        self.local_throw_edges += 1;
        Some(handler_pc)
    }

}
