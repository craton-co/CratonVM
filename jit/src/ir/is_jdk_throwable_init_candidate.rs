#[allow(unused_imports)]
use super::*;

impl IrBuilder {
    /// Keep the compiled local-throw edge disabled while a JVMTI exception
    /// listener is armed. The event bytes are per VM and may change after
    /// compilation, so a compiled handler is entered only when every byte is
    /// still zero; otherwise the exact throw bci is deoptimized and the
    /// interpreter performs exception dispatch and event delivery.
    pub(in crate::ir) fn guard_local_throw_events(&mut self, pc: usize, exc: NodeId) {
        let Some(flags) = self.exception_event_guard else {
            return;
        };
        let Some(ctrl) = self.ctrl_opt() else {
            return;
        };
        // Emit direct Guard(ClassInitTest) checks when this pc has a frame
        // state (normal bytecode athrow) so pure loops stay single-block and
        // can have the check hoisted to method/OSR entry.
        if self.has_frame_state_at(pc) {
            for state_addr in flags {
                let flag_clear = self.graph.add(
                    Op::ClassInitTest {
                        state_addr,
                        initialized: 0,
                    },
                    IrType::Int,
                    vec![ctrl],
                    Some(pc),
                );
                self.graph.add(
                    Op::Guard { bci: pc },
                    IrType::Void,
                    vec![ctrl, flag_clear],
                    Some(pc),
                );
            }
            return;
        }
        let mut quiet = None;
        for state_addr in flags {
            let flag_clear = self.graph.add(
                Op::ClassInitTest {
                    state_addr,
                    initialized: 0,
                },
                IrType::Int,
                vec![ctrl],
                Some(pc),
            );
            quiet = Some(match quiet {
                Some(previous) => {
                    self.add_data(Op::And, IrType::Int, vec![previous, flag_clear], pc)
                }
                None => flag_clear,
            });
        }
        let Some(quiet) = quiet else {
            return;
        };
        let branch = self
            .graph
            .add(Op::If, IrType::Control, vec![ctrl, quiet], Some(pc));
        let compiled = self
            .graph
            .add(Op::Proj(0), IrType::Control, vec![branch], Some(pc));
        let event_armed = self
            .graph
            .add(Op::Proj(1), IrType::Control, vec![branch], Some(pc));

        // The event path resumes at `athrow` with its operand restored. A
        // constant-false guard requests that precise interpreter replay; the
        // throw node after it is the graph's unreachable terminator, matching
        // the existing spliced-throw event guard.
        let saved_stack = std::mem::replace(&mut self.stack, vec![exc]);
        let zero = self.iconst(0);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![event_armed, zero],
            Some(pc),
        );
        let throw = self.graph.add(
            Op::Throw,
            IrType::Void,
            vec![event_armed, self.mem, exc],
            Some(pc),
        );
        self.graph.exit = throw;
        self.stack = saved_stack;
        self.ctrl = compiled;
    }

    /// JIT round 20 wave 11 (lane pIR, L2-3 / L2W2-4): the row the handler
    /// search picks for an exception of EXACTLY `class_id` thrown at `pc`, in
    /// table order (JVMS 2.10), when that is decided at compile time; `None`
    /// when it is not.
    ///
    /// Each covering row in order: a catch-all, the thrown class itself, or a
    /// catch class the VM answered `true` for is the row; a catch class the VM
    /// answered `false` for certainly does not match, and the search goes on
    /// to the next row; an `Unknown` row or an unanswered pair STOPS the
    /// decision (the search could stop there at run time, so no later row may
    /// be chosen). No covering row: `None` (the exception leaves the frame,
    /// the shared exit). The caller re-checks the row's class match.
    pub(in crate::ir) fn local_throw_superclass_row(&self, pc: usize, class_id: u32) -> Option<&IrLocalThrowRow> {
        for row in self
            .local_throw_rows
            .iter()
            .filter(|r| r.start_pc <= pc && pc < r.end_pc)
        {
            match row.catch {
                IrCatchClass::Any => return Some(row),
                IrCatchClass::Class(c) if c == class_id => return Some(row),
                IrCatchClass::Class(c) => match self.local_throw_subclass.get(&(class_id, c)) {
                    Some(true) => return Some(row),
                    Some(false) => continue,
                    None => return None,
                },
                IrCatchClass::Unknown => return None,
            }
        }
        None
    }

    /// How many branches this build replaced with a guarded jump.
    pub fn branches_pruned(&self) -> usize {
        self.branches_pruned
    }

    /// nio21 wave 17 (lane L4): branch pcs whose TAKEN edge the profile never
    /// saw. See [`Self::prune_never_taken_branch`]. Never calling it (or an
    /// empty set) builds exactly what every earlier build built.
    /// JIT round 23 wave 4 (lane L1, CF23-2; `CRATONVM_JIT_IR_SPLICE_NEVER_TAKEN`):
    /// the spliced branches (combined pcs) [`Self::prune_never_taken_branch`]
    /// may prune inside their splice, and the combined pcs only their taken
    /// edges reach. Never calling it builds what every earlier build built.
    pub fn set_splice_never_taken(
        &mut self,
        branches: std::collections::HashSet<usize>,
        dead: std::collections::HashSet<usize>,
    ) {
        self.splice_never_taken = branches;
        self.splice_never_taken_dead = dead;
    }

    pub fn set_splice_always_taken(
        &mut self,
        branches: std::collections::HashSet<usize>,
        dead: std::collections::HashSet<usize>,
    ) {
        self.splice_always_taken = branches;
        self.splice_always_taken_dead = dead;
    }

    pub fn set_splice_pruned_switch_edges(
        &mut self,
        edges: Vec<(usize, usize)>,
        dead: std::collections::HashSet<usize>,
    ) {
        self.splice_pruned_switch_edges.clear();
        for (sw_pc, target_pc) in edges {
            self.splice_pruned_switch_edges
                .entry(sw_pc)
                .or_default()
                .insert(target_pc);
        }
        self.splice_never_taken_dead.extend(dead);
    }

    fn prune_spliced_always_taken_branch(
        &mut self,
        pc: usize,
        cmp: NodeId,
        target_pc: usize,
    ) -> bool {
        if !self.splice_always_taken.contains(&pc)
            || target_pc <= pc + 3
            || self.block_walk
            || self.sync_splice_open()
        {
            return false;
        }
        let Some(frame) = self.splice.last() else {
            return false;
        };
        if !frame.ranges.is_empty() || pc < frame.base || pc >= frame.end || target_pc >= frame.end
        {
            return false;
        }
        if !self.chain_state_at(pc) {
            return false;
        }
        let Some(ctrl) = self.ctrl_opt() else {
            return false;
        };
        let needs_chain = !self.spliced_bodies_pure
            || !self.splice_committed_bases.is_empty()
            || self.splice_call_committed;
        if needs_chain && !self.relax_fence_by_chain(pc) {
            return false;
        }
        let guard = self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, cmp],
            Some(pc),
        );
        if !self.graph.unstable_if_trap_pcs.contains(&pc) {
            self.graph.unstable_if_trap_pcs.push(pc);
        }
        self.graph
            .chain_predicate_guards
            .push((guard, crate::deopt::PredicateKind::UnstableIf as u8));
        self.add_merge_predecessor(target_pc);
        self.ctrl = NO_NODE;
        self.branches_pruned += 1;
        note_branch_pruned();
        true
    }

    pub fn set_never_taken_branches(&mut self, pcs: std::collections::HashSet<usize>) {
        self.pruned_never_taken = pcs;
    }

    /// How many never-taken branches this build pruned.
    pub fn never_taken_branches_pruned(&self) -> usize {
        self.never_taken_pruned
    }

    /// nio21 wave 17 (lane L4): the mirror of
    /// [`Self::prune_always_taken_branch`] -- a conditional branch whose TAKEN
    /// edge the profile never observed (`lib.rs::ir_never_taken_branches`)
    /// becomes `Guard(!cmp)` and the walk continues on the fall-through; the
    /// target does not get this predecessor, so a taken arm no other edge
    /// reaches is never built. HotSpot C2's `unstable_if` uncommon trap.
    ///
    /// # Why the deopt is exact
    ///
    /// The guard fails exactly when the branch would be taken. It deopts at
    /// `pc` itself (the branch not yet executed), `DeoptAction::Reinterpret`
    /// with `DeoptReason::UncommonTrap`, through the compiling method's own
    /// snapshot at the top of `pc` (`has_frame_state_at`): the interpreter
    /// re-executes the branch, takes it, and runs the cold arm itself. Every
    /// exception, stack trace and side effect of that arm is therefore the
    /// interpreter's, i.e. HotSpot's. Nothing before `pc` is re-run.
    ///
    /// # When it is withdrawn
    ///
    /// The trap is charged at `(UncommonTrap, pc)`; after
    /// `PER_BCI_DESPEC_LIMIT` charges the VM records `pc` in its de-spec
    /// registry (`despec_site_after_trap_limit`) and the feed offers the pc no
    /// more, so the next compile keeps the `If` (the HotSpot "recompile after
    /// the trap" behaviour, with HotSpot's `PerBytecodeTrapLimit` grain).
    /// Since JIT r20 wave 11 (lane pProf, W17L4-5) the guard is listed in
    /// `Graph::unstable_if_trap_pcs` and traps as `DeoptReason::UnstableIf`:
    /// recompile on the first trap, withdrawn after ONE charge (HotSpot's
    /// `Reason_unstable_if`); `CRATONVM_JIT_PROF_UNSTABLE_IF_REASON=0` keeps
    /// the `UncommonTrap` schedule above.
    ///
    /// # Refusals (the ordinary `Op::If` is built)
    ///
    /// * the pc not offered, a backward or self target (a loop's back edge is
    ///   never "never taken" evidence);
    /// * inside a splice (its pcs are not the profile's pcs);
    /// * the block walk (a rotated loop): the walk visits ranges in reverse
    ///   post-order, and a range entered only through the pruned edge would
    ///   be reached with dead control the walk cannot tell from a bug;
    /// * no frame state at `pc`, or dead control;
    /// * `cmp` not an integer/reference `Op::Cmp` this arm built.
    /// JIT round 23 wave 4 (lane L1, CF23-2; `CRATONVM_JIT_IR_SPLICE_NEVER_TAKEN`):
    /// [`Self::prune_never_taken_branch`] inside a SPLICED body, for a branch
    /// listed by [`Self::set_splice_never_taken`] (the callee's own profile
    /// never saw its taken edge). A guard that the branch falls through, at
    /// `pc`, and the walk continues on the fall-through; the code only the
    /// taken edge reached is stepped over with dead control
    /// (`splice_never_taken_dead`). Exact as the cold-throw side entry is: the
    /// guard deopts at the branch, not yet executed, through the CHAIN
    /// snapshot the walk pushed at the top of `pc` (the callee at its own
    /// branch, every caller parked at its invoke), and the interpreter takes
    /// the edge in the callee's own frame. The guard is listed in
    /// `Graph::chain_predicate_guards` as [`crate::deopt::PredicateKind::UnstableIf`]
    /// so its point carries the chain id the VM charges at the call site.
    ///
    /// Refused (the ordinary `Op::If` is built, the taken edge's code is then
    /// reached with live control): not listed; a branch to its own
    /// fall-through or backward; out of the innermost body; inside a
    /// synchronized splice's window, a handler-covered body (`ranges`) or the
    /// block walk; no chain snapshot at `pc`; dead control; `cmp` not a
    /// two-operand `Op::Cmp`.
    pub(in crate::ir) fn prune_spliced_never_taken_branch(&mut self, pc: usize, cmp: NodeId, target_pc: usize) -> bool {
        if !self.splice_never_taken.contains(&pc)
            || target_pc <= pc + 3
            || self.block_walk
            || self.sync_splice_open()
        {
            return false;
        }
        let Some(frame) = self.splice.last() else {
            return false;
        };
        if !frame.ranges.is_empty() || pc < frame.base || pc >= frame.end || target_pc >= frame.end
        {
            return false;
        }
        if !self.chain_state_at(pc) {
            return false;
        }
        let (cc, a, b) = match self.graph.node_opt(cmp) {
            Some(n) => match (&n.op, n.inputs.as_slice()) {
                (Op::Cmp(cc), &[a, b]) => (*cc, a, b),
                _ => return false,
            },
            None => return false,
        };
        let Some(ctrl) = self.ctrl_opt() else {
            return false;
        };
        // As the cold-throw side entry: a body that committed something
        // relaxes the replay fence through the chain.
        let needs_chain = !self.spliced_bodies_pure
            || !self.splice_committed_bases.is_empty()
            || self.splice_call_committed;
        if needs_chain && !self.relax_fence_by_chain(pc) {
            return false;
        }
        let not_taken = self.add_data(Op::Cmp(cc.negate()), IrType::Int, vec![a, b], pc);
        let guard = self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, not_taken],
            Some(pc),
        );
        self.graph
            .chain_predicate_guards
            .push((guard, crate::deopt::PredicateKind::UnstableIf as u8));
        self.never_taken_pruned += 1;
        note_branch_pruned();
        if ir_bail_reporting() {
            eprintln!(
                "[ir] spliced never-taken branch PRUNED at combined pc {pc} (target {target_pc}) in {} \
                 -- a taken edge deopts through the chain at the branch",
                self.method_label.as_deref().unwrap_or("<unknown>"),
            );
        }
        true
    }

    pub(in crate::ir) fn prune_never_taken_branch(&mut self, pc: usize, cmp: NodeId, target_pc: usize) -> bool {
        // JIT r20 wave 11, lane pProf (W17L4-5 of
        // `jit-r21-nio-proposals-w17-L4.md`): `CRATONVM_JIT_PROF_UNSTABLE_IF_REASON`,
        // default ON; `=0` leaves the prune guard an `UncommonTrap`. Read
        // once per pruned branch.

        // JIT round 23 wave 4 (lane L1, CF23-2): inside a splice, a branch the
        // CALLEE's profile never saw taken, as a chain-framed guard.
        if !self.splice.is_empty() {
            return self.prune_spliced_never_taken_branch(pc, cmp, target_pc);
        }
        if !self.pruned_never_taken.contains(&pc) {
            return false;
        }
        // `target == pc + 3` is a branch to its own fall-through: the edge is
        // the fall-through edge too, so there is nothing to prune.
        if target_pc <= pc + 3 || self.block_walk {
            return false;
        }
        let Some(ctrl) = self.ctrl_opt() else {
            return false;
        };
        if !self.has_frame_state_at(pc) {
            return false;
        }
        let Some((cc, a, b)) = self.graph.node_opt(cmp).and_then(|n| match n.op {
            Op::Cmp(cc) if n.inputs.len() == 2 => Some((cc, n.inputs[0], n.inputs[1])),
            _ => None,
        }) else {
            return false;
        };
        // `!cmp`: 1 exactly when the branch FALLS THROUGH. Integer and
        // reference compares only (the `fcmp*` / `lcmp` results are ints by
        // then), so the negation is exact -- no NaN ordering to respect.
        let not_taken = self.add_data(Op::Cmp(cc.negate()), IrType::Int, vec![a, b], pc);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, not_taken],
            Some(pc),
        );
        // W17L4-5: the guard's trap is an unstable-if, not a generic
        // uncommon trap (one deopt + one recompile on the first taken edge).
        if !self.graph.unstable_if_trap_pcs.contains(&pc) {
            self.graph.unstable_if_trap_pcs.push(pc);
        }
        self.never_taken_dead_from = self.never_taken_dead_from.min(target_pc);
        self.never_taken_edges.push((pc, target_pc));
        self.never_taken_pruned += 1;
        note_branch_pruned();
        if ir_bail_reporting() {
            eprintln!(
                "[ir] never-taken branch PRUNED at bytecode pc {pc} (target {target_pc}) in {} -- \
                 a taken edge deopts to the interpreter at the branch",
                self.method_label.as_deref().unwrap_or("<unknown>"),
            );
        }
        true
    }

    /// nio21 wave 19 (lane L3, `r21-threadlocal-get-has-no-ir-intrinsic-FIXED-20261010.md`;
    /// `CRATONVM_JIT_IR_THREAD_LOCAL_GET`, default ON, `=0` keeps the call):
    /// `invokevirtual ThreadLocal.get()` as `ThreadLocalMap.getEntry`'s hit
    /// path, inline, with the original call as the one miss edge:
    ///
    /// ```text
    /// exact(recv, ThreadLocal | SuppliedThreadLocal)  else miss
    /// t   = CurrentThreadMirror                        null -> miss
    /// map = t.threadLocals                             null or not exactly ThreadLocalMap -> miss
    /// tab = map.table                                  null -> miss
    /// len = tab.length                                 len <= 0 -> miss
    /// e   = tab[recv.threadLocalHashCode & (len - 1)]  null or not exactly Entry -> miss
    /// e.referent == recv                               else miss
    /// v   = e.value
    /// join: phi(v, call)
    /// ```
    ///
    /// # Why it is exact
    ///
    /// This is `ThreadLocal.get()` -> `get(Thread.currentThread())` ->
    /// `getMap(t)` -> `map.getEntry(this)`'s first probe, for a receiver
    /// whose class is exactly `ThreadLocal` or `SuppliedThreadLocal` (neither
    /// declares `get` nor `getMap`; `InheritableThreadLocal`, which reads
    /// another map, and every other subclass take the call). The hit returns
    /// `e.value` exactly as `get(Thread)` does; everything else -- no map,
    /// `NOT_SUPPORTED`, an empty slot, a stale or other key, the
    /// `getEntryAfterMiss` probe chain, `setInitialValue` -- is the call, which
    /// runs exactly what it ran before. (JIT round 21, lane tlocal: the first
    /// [`TL_PROBE_WALK_EXTRA`] steps of the probe chain over LIVE keys are
    /// inline too, `CRATONVM_JIT_IR_THREAD_LOCAL_PROBE_WALK`; a stale key on
    /// the chain is still the call.) The hit path writes nothing, takes no
    /// lock, and reads plain fields the JDK reads plainly. The referent is only
    /// compared (`refersTo`).
    ///
    /// The thread word is the one the thin helper reads (`java_thread_obj`).
    /// The class ids and field indices are the VM's own
    /// (`DirectHelperTable::thread_local_ir_layout`), published only where the
    /// VM's helper serves (`--real-jdk` with the real-field natives), so the
    /// two cannot disagree about the table. Every load is an ordinary
    /// `Op::Load` / `Op::ArrayLoad` (the lowering's layout dispatch, barriers
    /// and narrow oops); every base is null- and class-tested before its load,
    /// and the element index is `h & (len - 1)` with `len > 0`, so no load can
    /// fail. Each node carries the invoke's pc, whose frame is the call's own
    /// (nothing has been committed), so even a deopt would re-execute the
    /// invoke. The pc is listed in `Graph::synthetic_field_access_pcs`.
    ///
    /// Refused (the call is built as before): no layout, a site that is not
    /// `java/lang/ThreadLocal.get()Ljava/lang/Object;` (or, JIT round 21 lane tl2, an
    /// `InheritableThreadLocal` site without its layout words), a splice site
    /// under `CRATONVM_JIT_IR_THREAD_LOCAL_IN_SPLICE=0`, a
    /// caller-held synchronized row, an armed ZGC read barrier, no TLS
    /// mirror, no frame at `pc`, a non-reference receiver.
    pub(in crate::ir) fn try_thread_local_get_intrinsic(
        &mut self,
        pc: usize,
        info_ptr: usize,
        num_args: usize,
        ret_type: u8,
        code_len: usize,
    ) -> bool {
        let Some(l) = self.thread_local_layout else {
            // TLGET21-7: no layout YET -- trap a never-run site.
            self.tl_pending_layout_trap(pc, info_ptr, code_len);
            return false;
        };
        if info_ptr == 0 {
            return false;
        }
        // SAFETY: as `try_string_access_intrinsic`'s read of the same row.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        // JIT round 23 wave 2f (lane L4, W19L3-1,
        // `CRATONVM_JIT_IR_INHERITABLE_THREAD_LOCAL`, read VM-side when it
        // fills `inheritable_class`): an `InheritableThreadLocal` site takes
        // the same probe for a receiver of EXACTLY that class, whose `getMap`
        // reads `Thread.inheritableThreadLocals`; every other receiver misses
        // to the call. (JIT round 21's lane tl2 built the same arm; this one
        // is kept.)
        let inheritable = info.class_name == "java/lang/InheritableThreadLocal";
        if !(info.class_name == "java/lang/ThreadLocal" || (inheritable && l.inheritable_class != 0))
        {
            return false;
        }
        // nio21 wave 20 (lane L3, W19L3-1): `set(Object)` takes the same
        // probe, and on a hit stores the value into the found entry -- the
        // found-key arm of `ThreadLocalMap.set` -- instead of reading it.
        let is_get = info.method_name == "get"
            && info.descriptor == "()Ljava/lang/Object;"
            && num_args == 1
            && ret_type == b'L';
        let is_set = info.method_name == "set"
            && info.descriptor == "(Ljava/lang/Object;)V"
            && num_args == 2
            && ret_type == b'V'
            && l.set_admitted
            && ir_thread_local_set_enabled();
        if !(is_get || is_set) {
            return false;
        }
        // JIT round 21 (lane tl2, `CRATONVM_JIT_IR_THREAD_LOCAL_IN_SPLICE`): a
        // site inside a spliced body. Nothing the expansion builds can trap or
        // deopt (every load's base is null- and class-tested first, the index
        // is masked into bounds, the branches only choose between the hit and
        // the call), so it needs no frame state of its own: the one node that
        // can leave compiled code is the miss call, which is built, noted
        // (`note_splice_call`: the replay fence, the taints, the post-call
        // snapshot) and given its handler edges exactly as the plain `Op::Call`
        // a spliced site gets. The caller-pc tests below are the top level's.
        let spliced = !self.splice.is_empty();
        if spliced {
            let in_innermost = self
                .splice
                .last()
                .is_some_and(|f| pc >= f.base && pc < f.end);
            if !ir_thread_local_in_splice_enabled() || !in_innermost {
                return false;
            }
        }
        if !ir_thread_local_get_enabled()
            || (!spliced
                && (pc >= code_len
                    || self.sync_direct_calls.contains_key(&pc)
                    || !self.has_frame_state_at(pc)))
            || crate::x64::zgc_read_barrier_blocks_inline_fields()
            || crate::x64::jit_thread_tls_disp() == 0
        {
            return false;
        }
        let Some(ctrl0) = self.ctrl_opt() else {
            return false;
        };
        let Some(at) = self.stack.len().checked_sub(num_args) else {
            return false;
        };
        let is_ref = |b: &Self, v: Option<&NodeId>| {
            v.and_then(|&v| b.graph.node_opt(v)).map(|n| n.ty) == Some(IrType::Ref)
        };
        if !is_ref(self, self.stack.get(at)) || (is_set && !is_ref(self, self.stack.get(at + 1))) {
            return false;
        }
        // JIT round 21 (lane tlsub): the site's profiled receiver class --
        // a subclass ORed into the call form's class test (TL321-4), and the
        // one class the trap form tests alone (TL321-1, every miss a deopt).
        let (sub_class, trap_class) = self.tl_profiled_receiver(pc, inheritable, is_set, &l);
        // tl21 (TLOCAL21-4b): a site declared on `ThreadLocal` whose profile saw
        // ONE class alone that reads `inheritableThreadLocals` (an
        // `InheritableThreadLocal` or a subclass, e.g. Spring's
        // `NamedInheritableThreadLocal` held in a `ThreadLocal`-typed field) is
        // the trap form on that map. The site's own map is the plain one, so
        // this is never a call-form class OR.
        let inheritable_receiver = if !inheritable && sub_class.is_none() && trap_class.is_none() {
            self.tl_profiled_inheritable_receiver(pc, is_set, &l)
        } else {
            None
        };
        let trap_class = trap_class
            .or(inheritable_receiver)
            .filter(|_| self.tl_trap_site_admits(pc, spliced));
        // Which `Thread` map the expansion reads: the site's, or -- only in the
        // trap form just admitted -- the inheritable one.
        let inheritable_map =
            inheritable || (inheritable_receiver.is_some() && trap_class.is_some());
        let trap = trap_class.is_some();
        // JIT round 21 (lane tlget, TLGET21-1,
        // `CRATONVM_JIT_IR_THREAD_LOCAL_UNPROFILED_TRAP`): a site with no
        // settled profile -- never run, or too few samples to name a class --
        // tests the layout's classes as a GUARD at the invoke; every other
        // test keeps the call form. See [`Self::tl_unprofiled_site`].
        let class_trap = !trap
            && sub_class.is_none()
            && self.tl_unprofiled_site(pc)
            && self.tl_trap_site_admits(pc, spliced);
        // `CRATONVM_JIT_IR_THREAD_LOCAL_UNPROFILED_FULL_TRAP` (TLGET21-6): such
        // a site -- never run, as C2 treats unreached code -- is the whole
        // trap form, its class test the layout's classes; see
        // [`ir_thread_local_unprofiled_full_trap_enabled`].
        let unprofiled_trap = class_trap && ir_thread_local_unprofiled_full_trap_enabled();
        let trap = trap || unprofiled_trap;
        let class_trap = class_trap && !unprofiled_trap;
        // ── Past this point the site is consumed. ──
        let new_value = if is_set { Some(self.pop()) } else { None };
        let recv = self.pop();
        let mem0 = self.mem;
        // TLGET21-3: the first node of this expansion (`TlChainRecord`).
        let first = self.graph.nodes.len();
        let null = self.aconst_null();
        let mut misses: Vec<NodeId> = Vec::new();
        self.tl_license = NO_NODE;

        // 1. The receiver's exact class.
        let class_ok = if let Some(class_id) = trap_class {
            self.add_data(Op::ExactClassIs { class_id }, IrType::Int, vec![recv], pc)
        } else {
            let is_tl = self.add_data(
                Op::ExactClassIs {
                    class_id: if inheritable {
                        l.inheritable_class
                    } else {
                        l.thread_local_class
                    },
                },
                IrType::Int,
                vec![recv],
                pc,
            );
            let class_ok = if l.supplied_class != 0 && !inheritable {
                let is_supplied = self.add_data(
                    Op::ExactClassIs {
                        class_id: l.supplied_class,
                    },
                    IrType::Int,
                    vec![recv],
                    pc,
                );
                self.add_data(Op::Or, IrType::Int, vec![is_tl, is_supplied], pc)
            } else {
                is_tl
            };
            match sub_class {
                Some(class_id) => {
                    let is_sub =
                        self.add_data(Op::ExactClassIs { class_id }, IrType::Int, vec![recv], pc);
                    self.add_data(Op::Or, IrType::Int, vec![class_ok, is_sub], pc)
                }
                None => class_ok,
            }
        };
        let mut ctrl = self.tl_test(trap || class_trap, ctrl0, class_ok, pc, &mut misses);
        // 2. The current thread.
        let t = self.graph.add(
            Op::CurrentThreadMirror {
                offset: l.thread_obj_offset,
            },
            IrType::Ref,
            vec![ctrl],
            Some(pc),
        );
        let t_ok = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![t, null], pc);
        ctrl = self.tl_test(trap, ctrl, t_ok, pc, &mut misses);
        // 3. Its map, exactly a `ThreadLocalMap` (`inheritableThreadLocals`
        // for an `InheritableThreadLocal`, JIT round 23 wave 2f).
        let (map_index, map_row) = if inheritable_map {
            (l.inheritable_thread_locals_index, l.inheritable_thread_locals_row)
        } else {
            (l.thread_locals_index, l.thread_locals_row)
        };
        let mut mem = mem0;
        let map = self.tl_load(ctrl, &mut mem, t, map_index, MemKind::Ref, IrType::Ref, pc);
        self.tl_row(map, map_row, true);
        self.tl_trust(map);
        let map_ok = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![map, null], pc);
        ctrl = self.tl_test(trap, ctrl, map_ok, pc, &mut misses);
        let map_class = self.add_data(
            Op::ExactClassIs {
                class_id: l.map_class,
            },
            IrType::Int,
            vec![map],
            pc,
        );
        ctrl = self.tl_test(trap, ctrl, map_class, pc, &mut misses);
        // 4. The table and its length.
        let tab = self.tl_load(
            ctrl,
            &mut mem,
            map,
            l.table_index,
            MemKind::Ref,
            IrType::Ref,
            pc,
        );
        self.tl_row(tab, l.table_row, true);
        self.tl_trust(tab);
        let tab_ok = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![tab, null], pc);
        ctrl = self.tl_test(trap, ctrl, tab_ok, pc, &mut misses);
        let len = self.graph.add(
            Op::ArrayLength,
            IrType::Int,
            with_licensing_guard(vec![ctrl, mem, tab], self.tl_license),
            Some(pc),
        );
        let zero = self.iconst(0);
        let len_ok = self.add_data(Op::Cmp(CmpOp::Gt), IrType::Int, vec![len, zero], pc);
        ctrl = self.tl_test(trap, ctrl, len_ok, pc, &mut misses);
        // 5. The first probe: `h & (len - 1)`, in `[0, len)` for `len > 0`.
        let h = self.tl_load(
            ctrl,
            &mut mem,
            recv,
            l.hash_index,
            MemKind::Int,
            IrType::Int,
            pc,
        );
        self.tl_row(h, l.hash_row, false);
        let one = self.iconst(1);
        let mask = self.add_data(Op::Sub, IrType::Int, vec![len, one], pc);
        let mut i = self.add_data(Op::And, IrType::Int, vec![h, mask], pc);
        // JIT round 21 (lane tlocal, `CRATONVM_JIT_IR_THREAD_LOCAL_PROBE_WALK`,
        // default ON): `getEntryAfterMiss`'s walk, bounded. A key displaced by
        // linear probing (its home slot holds another LIVE key) used to miss
        // on every call and pay the helper; now up to `tl_probe_walk_extra()`
        // (four, or two under `..._PROBE_WALK_DEEP=0`) further slots
        // `(i + 1) & (len - 1)` are tried
        // (`nextIndex` for the power-of-two tables `ThreadLocalMap` keeps). A
        // null slot ends the walk as the JDK's `while (e != null)` does, and a
        // CLEARED key (`refersTo(null)`, where the JDK would run
        // `expungeStaleEntry`) is a miss: the call then does the expunge. So
        // the hit path still writes nothing, and since a key occurs at most
        // once in a table, the entry it finds is the one the JDK finds.
        // `CRATONVM_JIT_IR_THREAD_LOCAL_PROBE_WALK_DEEP`: four slots.
        let extra = tl_probe_walk_extra();
        // The guard current before the walk dominates the hit join; the arms' own do not.
        let pre_walk_license = self.tl_license;
        // `(ctrl, entry, mem)` of each probe that found the key.
        let mut hits: Vec<(NodeId, NodeId, NodeId)> = Vec::new();
        for k in 0..=extra {
            let e = self.graph.add(
                Op::ArrayLoad(MemKind::Ref),
                IrType::Ref,
                with_licensing_guard(vec![ctrl, mem, tab, i], self.tl_license),
                Some(pc),
            );
            mem = e;
            let e_ok = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![e, null], pc);
            ctrl = self.tl_test(trap, ctrl, e_ok, pc, &mut misses);
            let e_class = self.add_data(
                Op::ExactClassIs {
                    class_id: l.entry_class,
                },
                IrType::Int,
                vec![e],
                pc,
            );
            ctrl = self.tl_test(trap, ctrl, e_class, pc, &mut misses);
            // 6. `refersTo(this)`: compared, never handed out.
            let referent = self.tl_load(
                ctrl,
                &mut mem,
                e,
                l.referent_index,
                MemKind::Ref,
                IrType::Ref,
                pc,
            );
            self.tl_trust(referent);
            // Only compared with `recv` (non-null, live on this frame), never
            // handed out or tested against `null` for the result, so the ZGC
            // referent gate cannot affect `same` and need not force `jit_getfield`.
            // TL321-5 (`CRATONVM_JIT_IR_THREAD_LOCAL_REFERENT_ROW`): a compact
            // `Entry` reads its referent inline too. Only compared with
            // `recv`, which this frame holds: a slot holding `recv` holds a
            // live object, so neither the collector's referent gate (a dead
            // referent reads as null) nor its keep-alive can change `same`;
            // the `live` test below only chooses between walking on and the
            // call (which expunges), never the answer.
            if ir_thread_local_referent_row_enabled() {
                self.tl_row(referent, l.referent_row, true);
            }
            let same = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![referent, recv], pc);
            if k == extra {
                ctrl = self.tl_test(trap, ctrl, same, pc, &mut misses);
                hits.push((ctrl, e, mem));
                break;
            }
            let iff = self
                .graph
                .add(Op::If, IrType::Control, vec![ctrl, same], Some(pc));
            let found = self
                .graph
                .add(Op::Proj(0), IrType::Control, vec![iff], Some(pc));
            hits.push((found, e, mem));
            ctrl = self
                .graph
                .add(Op::Proj(1), IrType::Control, vec![iff], Some(pc));
            // Another key: a cleared one is the JDK's expunge (the call).
            let live = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![referent, null], pc);
            ctrl = self.tl_test(trap, ctrl, live, pc, &mut misses);
            let next = self.add_data(Op::Add, IrType::Int, vec![i, one], pc);
            i = self.add_data(Op::And, IrType::Int, vec![next, mask], pc);
        }
        let e = if hits.len() == 1 {
            let (c, e, m) = hits[0];
            ctrl = c;
            mem = m;
            e
        } else {
            let hit_join = self.graph.add(
                Op::Merge,
                IrType::Control,
                hits.iter().map(|h| h.0).collect(),
                Some(pc),
            );
            let mut e_in = vec![hit_join];
            e_in.extend(hits.iter().map(|h| h.1));
            let mut m_in = vec![hit_join];
            m_in.extend(hits.iter().map(|h| h.2));
            ctrl = hit_join;
            // The trap form's last guard sits on the final probe's arm only and does
            // not dominate the join: license the reads below it with the pre-walk one.
            self.tl_license = pre_walk_license;
            mem = self.graph.add(Op::Phi, IrType::Memory, m_in, Some(pc));
            self.graph.add(Op::Phi, IrType::Ref, e_in, Some(pc))
        };
        // 7. The hit.
        let value = match new_value {
            None => {
                let v = self.tl_load(
                    ctrl,
                    &mut mem,
                    e,
                    l.value_index,
                    MemKind::Ref,
                    IrType::Ref,
                    pc,
                );
                self.tl_row(v, l.value_row, true);
                self.tl_trust(v);
                Some(v)
            }
            // `e.value = value`: an ordinary reference `putfield` (the
            // lowering's `jit_putfield_object`, with the SATB pre-barrier on
            // the old value and the card mark).
            Some(nv) => {
                let idx = self.iconst(i64::from(l.value_index));
                let st = self.graph.add(
                    Op::Store(MemKind::Ref),
                    IrType::Memory,
                    vec![ctrl, mem, e, idx, nv],
                    Some(pc),
                );
                // `CRATONVM_JIT_IR_THREAD_LOCAL_SET_ROW`: the store's own
                // row (`Graph::field_rows`, read as the lowering's
                // `synthetic_store_row`), so a compact `Entry` takes the
                // collector's gated inline reference store instead of
                // `jit_putfield_object` on every `set`.
                if ir_thread_local_set_row_enabled() {
                    self.tl_row(st, l.value_row, true);
                }
                mem = st;
                None
            }
        };
        let (hit_ctrl, hit_mem) = (ctrl, mem);

        // JIT round 21 (lane tlsub, TL321-1): the trap form has no miss
        // edge -- every test above was a `Guard` deopting at this invoke --
        // so the hit IS the site: no call, no join, no memory merge.
        if trap {
            debug_assert!(misses.is_empty());
            self.ctrl = hit_ctrl;
            self.mem = hit_mem;
            if let Some(value) = value {
                self.push(value);
            }
            // TLGET21-3: the whole get (or set), for the loop predicate.
            if let (Ok(first), Ok(end)) = (
                NodeId::try_from(first),
                NodeId::try_from(self.graph.nodes.len()),
            ) {
                self.graph.tl_chains.push(TlChainRecord {
                    recv,
                    entry_ctrl: ctrl0,
                    entry_mem: mem0,
                    hit_ctrl,
                    value: value.unwrap_or(NO_NODE),
                    store: if value.is_none() { hit_mem } else { NO_NODE },
                    first,
                    end,
                    pc,
                });
            }
            self.graph.synthetic_field_access_pcs.insert(pc);
            if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[ir] ThreadLocal.{} expanded at bytecode pc {pc} in {}{}{} -- the hit path \
                     inline, every miss an uncommon trap (receiver class {}){}",
                    if is_set { "set" } else { "get" },
                    self.method_label.as_deref().unwrap_or("<unknown>"),
                    if spliced { " (spliced body)" } else { "" },
                    if inheritable_map { " (inheritable)" } else { "" },
                    trap_class.unwrap_or(0),
                    // TLGET21-6.
                    if unprofiled_trap {
                        " (unprofiled: the layout's classes)"
                    } else {
                        ""
                    },
                );
            }
            return true;
        }

        // The miss edge: the call this site always made, from the memory
        // state before the expansion (the hit path wrote nothing).
        let miss_ctrl = if misses.len() == 1 {
            misses[0]
        } else {
            self.graph.add(Op::Merge, IrType::Control, misses, Some(pc))
        };
        self.ctrl = miss_ctrl;
        self.mem = mem0;
        let mut call_inputs = vec![miss_ctrl, mem0, recv];
        call_inputs.extend(new_value);
        let call_ty = if is_set { IrType::Void } else { IrType::Ref };
        let call = self
            .graph
            .add(Op::Call { info_ptr }, call_ty, call_inputs, Some(pc));
        self.note_splice_call(call);
        self.mem = call;
        self.plant_call_handler_entry(pc, call, code_len);
        let miss_end = self.ctrl;
        let miss_mem = self.mem;

        // The join.
        let join = self.graph.add(
            Op::Merge,
            IrType::Control,
            vec![hit_ctrl, miss_end],
            Some(pc),
        );
        self.ctrl = join;
        self.mem = self.graph.add(
            Op::Phi,
            IrType::Memory,
            vec![join, hit_mem, miss_mem],
            Some(pc),
        );
        let mut result_phi = None;
        if let Some(value) = value {
            let result = self
                .graph
                .add(Op::Phi, IrType::Ref, vec![join, value, call], Some(pc));
            self.push(result);
            result_phi = Some(result);
        }
        // JIT round 21 (lane tl2): inside a splice the miss call asked for a
        // post-call chain snapshot at this invoke's successor
        // (`note_splice_call`), where the walk's stack holds the join's φ, not
        // the call. Name the call's result there, as SPLICE2-5 does for a
        // nested guarded splice's miss call at its join; with that switch off,
        // the call gets no post-call snapshot. A `set` pushes nothing.
        if spliced && self.pending_post_call.is_some_and(|(c, _)| c == call) {
            if let Some(phi) = result_phi {
                if ir_nested_miss_post_call_exit_enabled() {
                    self.post_call_join_value = Some((phi, call));
                } else {
                    self.pending_post_call = None;
                }
            }
        }
        self.graph.synthetic_field_access_pcs.insert(pc);
        if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] ThreadLocal.{} expanded at bytecode pc {pc} in {}{}{}{}{} -- the hit path \
                 inline, the call on a miss",
                if is_set { "set" } else { "get" },
                self.method_label.as_deref().unwrap_or("<unknown>"),
                if spliced { " (spliced body)" } else { "" },
                if inheritable { " (inheritable)" } else { "" },
                // JIT round 21 (lane tlsub, TL321-4).
                sub_class.map_or(String::new(), |c| format!(" (profiled subclass {c})")),
                // JIT round 21 (lane tlget, TLGET21-1).
                if class_trap {
                    " (unprofiled: another receiver class traps)"
                } else {
                    ""
                },
            );
        }
        true
    }

    /// JIT round 23 wave 9 (lane W9-L2, CF-W9-3,
    /// [`ir_vh_const_fresh_field_enabled`]): a constant-`VarHandle` `get*` /
    /// `set*` whose handle operand is a `getstatic` of a `static final` the VM
    /// filed a field plan for (`ir_lower::vh_const_plan`), and whose receiver
    /// is a FRESH allocation of exactly the plan's class not yet published, is
    /// built as the field access itself: an `Op::Load` / `Op::Store` at the
    /// plan's slot, with no `Op::Call` in the graph. Before this the site was
    /// always a call whose lowering carried an inline prefix; escape analysis
    /// saw the call and the receiver escaped (`CompletableFuture(Object)`'s
    /// `RESULT.setRelease(this, r)`, so neither future of a `thenApply` stage
    /// was ever scalar-replaced).
    ///
    /// Why it is exact:
    /// * the handle: a `static final` is written once, by `<clinit>`, before
    ///   the VM filed it (the same proof the prefix uses);
    /// * the receiver: an `Op::New` of exactly the plan's class is non-null and
    ///   of that class, so no NPE, `ClassCastException` or subclass layout is
    ///   possible -- the call could not have thrown;
    /// * ordering: every mode but plain `get` / `set` is marked a volatile
    ///   access (`Graph::set_volatile_access`), so no pass merges, hoists or
    ///   sinks it and the lowering fences a store (stronger than release /
    ///   opaque need, never weaker). The object is not yet visible to another
    ///   thread anyway (`vh_fresh_receiver_publication`).
    ///
    /// Read-modify-write modes (`compareAndSet`, `getAndAdd`, ...) keep their
    /// call: the IR has no atomic field op. `false` leaves the stack untouched
    /// and the site builds as its call.
    pub(in crate::ir) fn try_vh_const_fresh_field_access(
        &mut self,
        pc: usize,
        info_ptr: usize,
        num_args: usize,
    ) -> bool {
        let Some(helpers) = self.vh_fresh_helpers else {
            return false;
        };
        if info_ptr == 0 || !ir_vh_const_fresh_field_enabled() {
            return false;
        }
        // SAFETY: as `try_thread_local_get_intrinsic`'s read of the same row.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if info.class_name != "java/lang/invoke/VarHandle" || !matches!(info.invoke_kind, 0 | 1) {
            return false;
        }
        let Some(shape) = crate::ir_lower::vh_const_site_shape(info.method_name, info.descriptor)
        else {
            return self.vh_fresh_refused(pc, info, "not a field access-mode shape");
        };
        let is_set = match shape.op {
            crate::ir_lower::VhConstOp::Get => false,
            crate::ir_lower::VhConstOp::Set { .. } => true,
            crate::ir_lower::VhConstOp::Rmw(_) => {
                return self.vh_fresh_refused(pc, info, "a read-modify-write mode")
            }
        };
        // [handle, receiver, (value)]
        if num_args != 2 + usize::from(is_set) {
            return self.vh_fresh_refused(pc, info, "argument count");
        }
        let Some(at) = self.stack.len().checked_sub(num_args) else {
            return false;
        };
        let (handle, recv) = (self.stack[at], self.stack[at + 1]);
        let Some(Op::LoadStatic {
            class_id,
            field_index,
            ..
        }) = self.graph.node_opt(handle).map(|n| &n.op)
        else {
            return self.vh_fresh_refused(pc, info, "the handle is not a getstatic");
        };
        let (static_class, static_field) = (*class_id, *field_index as usize); // Cast: u32 index
        let Some(plan) = crate::ir_lower::vh_const_plan(&helpers, static_class, static_field) else {
            return self.vh_fresh_refused(pc, info, "no constant plan filed");
        };
        if plan.kind != shape.kind || (is_set && plan.read_only) {
            return self.vh_fresh_refused(pc, info, "kind mismatch or a read-only handle");
        }
        // `int` and reference fields: the two kinds every lowering path
        // serves without a further gate (a `long` field's store needs the
        // long-field helpers the `putfield` arm gates on; it keeps its call).
        let (mem_kind, ty) = match plan.kind {
            b'I' => (MemKind::Int, IrType::Int),
            b'L' => (MemKind::Ref, IrType::Ref),
            _ => return self.vh_fresh_refused(pc, info, "a long or narrow field"),
        };
        // A `get`'s result tag must be the field's (an `int` field read as
        // `Object` is a boxing call site, not this shape).
        let ret_ok = if is_set {
            shape.ret == b'V'
        } else {
            match plan.kind {
                b'L' => matches!(shape.ret, b'L' | b'['),
                k => shape.ret == k,
            }
        };
        if !ret_ok {
            return self.vh_fresh_refused(pc, info, "return tag");
        }
        let fresh_exact = matches!(
            self.graph.node_opt(recv).map(|n| &n.op),
            Some(Op::New { class_id, .. }) if *class_id == plan.receiver_class_id
        );
        if !fresh_exact {
            return self.vh_fresh_refused(pc, info, "the receiver is not a fresh new of the plan class");
        }
        if let Some(why) = vh_fresh_receiver_publication(&self.graph, self.mem, recv) {
            return self.vh_fresh_refused(pc, info, &why);
        }
        let Some(disp) = crate::ir_lower::vh_const_field_disp(plan) else {
            return self.vh_fresh_refused(pc, info, "no compact displacement");
        };
        let Ok(disp) = u32::try_from(disp) else {
            return false;
        };
        let plain = matches!(info.method_name, "get" | "set");
        let ordered = !plain;
        // The lowering's own volatile switches, not the builder's admission
        // flags: those turn on only when this method (or a splice) has a
        // volatile field pc, so on the n23-w9c build every ordered site was
        // refused here (wave 9d).
        let volatile_loads_ok = ir_volatile_fields_enabled();
        let volatile_stores_ok = volatile_loads_ok && IR_LOWER_FENCES_VOLATILE_STORES;
        if ordered && ((is_set && !volatile_stores_ok) || (!is_set && !volatile_loads_ok)) {
            return self.vh_fresh_refused(pc, info, "volatile accesses not admitted");
        }
        // Inside a splice a store must be into an object the splice made
        // (the `putfield` arm's replay rule); a fresh `new` of the splice is.
        if is_set
            && !self.splice.is_empty()
            && !(self.splice_store_is_local(recv) || self.splice_store_is_ctor_receiver(recv))
        {
            return self.vh_fresh_refused(pc, info, "a splice store into an object the splice did not make");
        }
        // ── Past this point the site is consumed. ──
        let value = if is_set { Some(self.pop()) } else { None };
        let _recv = self.pop();
        let _handle = self.pop();
        let offset = self.iconst(i64::from(plan.slot));
        let node = match value {
            Some(value) => {
                if !self.splice.is_empty() {
                    self.note_splice_store_value(recv, value);
                }
                self.graph.add(
                    Op::Store(mem_kind),
                    IrType::Memory,
                    vec![self.ctrl, self.mem, recv, offset, value],
                    Some(pc),
                )
            }
            None => self.graph.add(
                Op::Load(mem_kind),
                ty,
                vec![self.ctrl, self.mem, recv, offset],
                Some(pc),
            ),
        };
        if ordered {
            self.graph.set_volatile_access(node);
            // `setRelease` / `setOpaque`: ordered by every pass, no StoreLoad
            // fence (`Graph::release_stores`). `setVolatile` keeps its fence.
            if is_set && matches!(info.method_name, "setRelease" | "setOpaque") {
                self.graph.release_stores.insert(node);
            }
        }
        // The field-index arms at this pc, with this node's own row.
        self.graph.synthetic_field_access_pcs.insert(pc);
        self.graph
            .field_rows
            .insert(node, (disp, plan.kind == b'L', plan.kind));
        self.mem = node;
        if !is_set {
            self.note_splice_local_load(node, recv);
            self.push(node);
        }
        if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] vh-const fresh field {}{} at bytecode pc {pc} in {}: {} of class {} slot {} \
                 disp {disp}{} -- no call",
                info.method_name,
                info.descriptor,
                self.method_label.as_deref().unwrap_or("<unknown>"),
                if is_set { "store" } else { "load" },
                plan.receiver_class_id,
                plan.slot,
                if ordered { " (ordered)" } else { "" },
            );
        }
        true
    }

    /// JIT round 23 wave 10 (lane L2): name why a constant-`VarHandle` site
    /// keeps its call (`CRATONVM_DBG_JITC` / `CRATONVM_DBG_IR_COMPILES`), and
    /// answer `false`. The n23-w9c and n23-w10a proofs saw no
    /// `vh-const fresh field` line and nothing said which rule refused.
    pub(in crate::ir) fn vh_fresh_refused(&self, pc: usize, info: &JitInvokeInfo, why: &str) -> bool {
        if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] vh-const fresh field REFUSED {}{} at bytecode pc {pc} in {}: {why}",
                info.method_name,
                info.descriptor,
                self.method_label.as_deref().unwrap_or("<unknown>"),
            );
        }
        false
    }

    /// TLGET21-7 (`CRATONVM_JIT_IR_THREAD_LOCAL_PENDING_TRAP`): a
    /// `ThreadLocal.get` / `set` site compiled while the expansion's layout is
    /// PENDING -- the VM serves it, but `ThreadLocalMap` / its `Entry` are not
    /// loaded yet, as in a driver compiled before the program's first
    /// `ThreadLocal.set` (`R21TlsubRows`, `R21Tl3Rows`) -- gets an
    /// unconditional `Guard` at the invoke when the site has never run, and
    /// then builds its call as before. The interpreter runs the first
    /// execution (which loads the classes), and the recompile expands it; the
    /// call form used to stay for the life of the code, ~150 ns a get. C2's
    /// unloaded-class traps do the same for code that was never reached.
    ///
    /// Exact as every trap form's guard is: at the invoke's own pc, before
    /// anything is committed ([`Self::tl_trap_site_admits`]: the frame, the
    /// chain snapshot, the de-spec registry, which withdraws a site that keeps
    /// trapping). A site that has run, or any site once the layout is there,
    /// is untouched.
    pub(in crate::ir) fn tl_pending_layout_trap(&mut self, pc: usize, info_ptr: usize, code_len: usize) {
        if info_ptr == 0
            || !self
                .tl_site_plan
                .as_deref()
                .is_some_and(|p| p.layout_pending && p.unprofiled(pc))
        {
            return;
        }
        // SAFETY: as `try_thread_local_get_intrinsic`'s read of the same row.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        let site = matches!(
            info.class_name,
            "java/lang/ThreadLocal" | "java/lang/InheritableThreadLocal"
        ) && matches!(
            (info.method_name, info.descriptor),
            ("get", "()Ljava/lang/Object;") | ("set", "(Ljava/lang/Object;)V")
        );
        if !site {
            return;
        }
        let spliced = !self.splice.is_empty();
        if spliced {
            if !self
                .splice
                .last()
                .is_some_and(|f| pc >= f.base && pc < f.end)
            {
                return;
            }
        } else if pc >= code_len || self.sync_direct_calls.contains_key(&pc) {
            return;
        }
        if !self.tl_trap_site_admits(pc, spliced) {
            return;
        }
        let Some(ctrl) = self.ctrl_opt() else {
            return;
        };
        let zero = self.iconst(0);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, zero],
            Some(pc),
        );
        if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] ThreadLocal.{} at bytecode pc {pc} in {}{} -- layout pending, never run: \
                 an uncommon trap",
                info.method_name,
                self.method_label.as_deref().unwrap_or("<unknown>"),
                if spliced { " (spliced body)" } else { "" },
            );
        }
    }

    /// JIT round 21 (lane tlget, `CRATONVM_JIT_IR_THREAD_LOCAL_TRUSTED_LOADS`,
    /// default ON, `=0` keeps the full containment): list the expansion's
    /// reference load `load` in [`Graph::tl_trusted_loads`]. Its base is a
    /// `Ref` node, the receiver proof the trusted-receiver form rests on
    /// (`Lowerer::emit_read_bounds_live_check`), and the one question the
    /// containment also answered for a reference load -- "do reference slots
    /// hold plain pointers right now" -- is still asked by that check.
    pub(in crate::ir) fn tl_trust(&mut self, load: NodeId) {
        if ir_thread_local_trusted_loads_enabled() {
            self.graph.tl_trusted_loads.insert(load);
        }
    }

    /// JIT round 21 (lane tlget, TLGET21-1,
    /// `CRATONVM_JIT_IR_THREAD_LOCAL_UNPROFILED_TRAP`, default ON, `=0` kill):
    /// does the site at `pc` have NO settled receiver profile -- no row in
    /// the plan, or one under [`crate::profile::DOMINANT_RECEIVER_MIN_OBSERVATIONS`]
    /// samples ([`ThreadLocalSitePlan::unprofiled`])?
    ///
    /// Such a site's class test becomes a `Guard` at the invoke (the trap
    /// form's, [`Self::tl_trap_site_admits`] still asked) instead of a branch
    /// to the call; every other test keeps the call form. A receiver of the
    /// layout's classes runs exactly as before. Any other receiver deopts
    /// once, the interpreter runs the JDK body and profiles the site, and the
    /// recompile reads that profile: the `new ThreadLocal<>() { initialValue }`
    /// subclass is then admitted (TL321-4, call form or, alone over the trap
    /// floor, TL321-1's trap form) instead of paying the call on every get
    /// for the life of the code. That was the whole of
    /// `R21Tl2Rows tl-subclass-get`: `run` is compiled once, while row 0
    /// warms, with every kernel spliced in and `subclassGet`'s site never run,
    /// so no compile ever saw its profile. A site that keeps trapping is
    /// withdrawn by the de-spec registry as the trap form's is, and the next
    /// compile keeps the call form. Off with
    /// `CRATONVM_JIT_IR_THREAD_LOCAL_PROFILED_SUBCLASS=0` too (a recompile
    /// could then admit nothing new).
    pub(in crate::ir) fn tl_unprofiled_site(&self, pc: usize) -> bool {
        ir_thread_local_unprofiled_trap_enabled()
            && ir_thread_local_profiled_subclass_enabled()
            && self
                .tl_site_plan
                .as_deref()
                .is_some_and(|p| p.unprofiled(pc))
    }

    /// nio21 wave 20 (lane L3): give the expansion's load `load` its own
    /// compact row (`Graph::field_rows`), from the VM's `1 + displacement`
    /// word `row` (`0` = none: the load keeps the field-index arms). A
    /// reference row is `b'L'`, the hash `b'I'`. Off under
    /// `CRATONVM_JIT_IR_THREAD_LOCAL_GET_ROWS=0` or without compact reference
    /// fields (the compile door's own rule for its rows).
    pub(in crate::ir) fn tl_row(&mut self, load: NodeId, row: u32, is_ref: bool) {
        if row == 0
            || !ir_thread_local_get_rows_enabled()
            || !cratonvm_types::compact_ref_fields_enabled()
        {
            return;
        }
        let tag = if is_ref { b'L' } else { b'I' };
        self.graph.field_rows.insert(load, (row - 1, is_ref, tag));
    }

    /// `If(cond)` at `ctrl`: the true edge is returned, the false edge joins
    /// `misses` (nio21 wave 19, `try_thread_local_get_intrinsic`).
    pub(in crate::ir) fn tl_branch(
        &mut self,
        ctrl: NodeId,
        cond: NodeId,
        pc: usize,
        misses: &mut Vec<NodeId>,
    ) -> NodeId {
        let iff = self
            .graph
            .add(Op::If, IrType::Control, vec![ctrl, cond], Some(pc));
        let yes = self
            .graph
            .add(Op::Proj(0), IrType::Control, vec![iff], Some(pc));
        let no = self
            .graph
            .add(Op::Proj(1), IrType::Control, vec![iff], Some(pc));
        misses.push(no);
        yes
    }

    /// JIT round 21 (lane tlsub, TL321-1; re-landed t1 wave 8):
    /// [`Self::tl_branch`], or -- in the trap form -- `Guard(cond)` at `ctrl`
    /// deopting at the invoke `pc` (whose frame is the call's own: nothing of
    /// the expansion has committed anything), after which the walk continues
    /// at the same `ctrl`. The guard becomes the licensing guard of the reads
    /// that follow ([`Self::tl_license`]).
    pub(in crate::ir) fn tl_test(
        &mut self,
        trap: bool,
        ctrl: NodeId,
        cond: NodeId,
        pc: usize,
        misses: &mut Vec<NodeId>,
    ) -> NodeId {
        if !trap {
            return self.tl_branch(ctrl, cond, pc, misses);
        }
        let guard = self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, cond],
            Some(pc),
        );
        self.tl_license = guard;
        ctrl
    }

    /// t1 wave 10 (lane REFS3, `CRATONVM_JIT_IR_REFERS_TO_NO_KEEPALIVE`): list
    /// the compare-only referent read `load` in
    /// [`Graph::no_keepalive_referent_loads`].
    pub(in crate::ir) fn note_no_keepalive_referent_load(&mut self, load: NodeId) {
        if ir_refers_to_no_keepalive_enabled() {
            self.graph.no_keepalive_referent_loads.insert(load);
        }
    }

    /// JIT round 21 (lane tlsub): the `ThreadLocal` site's profiled receiver
    /// class as `(call-form subclass, trap-form class)`.
    ///
    /// The profile ([`ThreadLocalSitePlan`]) names a dominant class `C`. `C`
    /// is admitted when it is one of the layout's own classes (`ThreadLocal`,
    /// an admitted `SuppliedThreadLocal`; `InheritableThreadLocal` at an
    /// `InheritableThreadLocal`-typed site), or -- TL321-4,
    /// [`ir_thread_local_profiled_subclass_enabled`] -- a class the VM says
    /// `ThreadLocal.get` reads the same map for ([`Self::tl_subclass_admitted`]).
    /// An admitted subclass is the first half (ORed into the call form's
    /// class test); `C` is the second half when the profile saw it ALONE over
    /// [`crate::MIN_GUARDED_RECEIVER_EVIDENCE`] exact samples and
    /// [`ir_thread_local_miss_trap_enabled`] (the caller still asks
    /// [`Self::tl_trap_site_admits`]).
    pub(in crate::ir) fn tl_profiled_receiver(
        &mut self,
        pc: usize,
        inheritable: bool,
        is_set: bool,
        l: &ThreadLocalIrLayout,
    ) -> (Option<u32>, Option<u32>) {
        let Some((dominant, only)) = self
            .tl_site_plan
            .as_deref()
            .and_then(|p| p.receivers.get(&pc).copied())
        else {
            return (None, None);
        };
        let layout_class = if inheritable {
            dominant == l.inheritable_class
        } else {
            dominant == l.thread_local_class
                || (l.supplied_class != 0 && dominant == l.supplied_class)
        };
        if dominant == 0
            || (!layout_class && !self.tl_subclass_admitted(dominant, inheritable, is_set))
        {
            return (None, None);
        }
        let sub = (!layout_class).then_some(dominant);
        let trap =
            (only == Some(dominant) && ir_thread_local_miss_trap_enabled()).then_some(dominant);
        (sub, trap)
    }

    /// tl21 (TLOCAL21-4b): the one class a `ThreadLocal`-declared site's
    /// profile saw ALONE, when the VM says `ThreadLocal.get` / `set` read
    /// `Thread.inheritableThreadLocals` for it -- `InheritableThreadLocal`
    /// itself or a subclass that declares no `get()` (and, for `set`, no
    /// `set(Object)`), e.g. Spring's `NamedInheritableThreadLocal` kept in a
    /// field declared `ThreadLocal<..>`. TLOCAL21-4 admitted only sites
    /// declared on `InheritableThreadLocal`, which that field never is, so
    /// these sites paid the thin helper on every call.
    ///
    /// Trap form only (the caller still asks [`Self::tl_trap_site_admits`]):
    /// the class test is that one exact class, and every other test is a
    /// guard at the invoke, so the expansion needs no second map and no call
    /// edge. The profile must see the class alone over the trap floor
    /// ([`ThreadLocalSitePlan::note_receivers`]'s `only`), and the layout must
    /// carry the inheritable rows (`inheritable_class != 0`, which the VM
    /// withholds under `CRATONVM_JIT_IR_INHERITABLE_THREAD_LOCAL=0`). The
    /// verdict is [`Self::tl_subclass_admitted`]'s, asked for the inheritable
    /// kind; it also records the class as copied, so an unload withdraws the
    /// code. `None` when any of that fails: the site keeps its call form.
    pub(in crate::ir) fn tl_profiled_inheritable_receiver(
        &mut self,
        pc: usize,
        is_set: bool,
        l: &ThreadLocalIrLayout,
    ) -> Option<u32> {
        if l.inheritable_class == 0
            || !ir_thread_local_miss_trap_enabled()
            || !ir_thread_local_profiled_subclass_enabled()
        {
            return None;
        }
        let (dominant, only) = self
            .tl_site_plan
            .as_deref()
            .and_then(|p| p.receivers.get(&pc).copied())?;
        if dominant == 0 || only != Some(dominant) {
            return None;
        }
        self.tl_subclass_admitted(dominant, true, is_set)
            .then_some(dominant)
    }

    /// JIT round 21 (lane tlsub, TL321-4): may the expansion test for
    /// `class_id`, a class other than the layout's own?
    ///
    /// Only where the VM answers, for that class, the map kind the site's
    /// expansion reads (`ThreadLocal.threadLocals`, or
    /// `inheritableThreadLocals` at an `InheritableThreadLocal` site) --
    /// `tl_get_receiver_kind`, the verdict the thin helper serves that class
    /// by: no class on its chain declares `get()`, and it is not a
    /// `TerminatingThreadLocal` -- and, for `set`, that no class below
    /// `ThreadLocal` declares `set(Object)`. `getMap` is package-private in
    /// `java.lang`, which no loader but the boot loader defines, and
    /// `InheritableThreadLocal` is the one boot class that overrides it (the
    /// inheritable kind). So for an object of exactly that class the hit path
    /// reads what `get(Thread)` / `set(Thread, T)` read, and everything else
    /// is the miss (the call, or the trap), which runs the class's own
    /// `initialValue` and the rest of the JDK body.
    ///
    /// The id is baked into this code like a profile-chosen guarded splice's
    /// class, and recorded the same way: as a class this compile copied
    /// (`note_copy_in_open_compile`), which an unload of the class withdraws
    /// the code by (`CRATONVM_JIT_UNLOAD_WITHDRAWS_COPIED`). With no compile
    /// scope open to record it in, the class is not admitted.
    pub(in crate::ir) fn tl_subclass_admitted(
        &mut self,
        class_id: u32,
        inheritable: bool,
        is_set: bool,
    ) -> bool {
        if class_id == 0 || !ir_thread_local_profiled_subclass_enabled() {
            return false;
        }
        let Some(plan) = self.tl_site_plan.as_deref_mut() else {
            return false;
        };
        let Some((kind, set_ok)) = plan.receiver_kind(class_id) else {
            return false;
        };
        let want = if inheritable {
            crate::direct_helpers::TL_RECEIVER_KIND_INHERITABLE
        } else {
            crate::direct_helpers::TL_RECEIVER_KIND_PLAIN
        };
        if kind != want || (is_set && !set_ok) {
            return false;
        }
        crate::note_copy_in_open_compile(class_id)
    }

    /// JIT round 21 (lane tlsub, TL321-1): may this site's misses be
    /// uncommon traps instead of the call?
    ///
    /// # Why the deopt is exact
    ///
    /// Every trap-form test is a `Guard` at the invoke's own pc, before
    /// anything of the expansion has been committed (a `set` stores only on
    /// the hit, after the last guard), so a failing one resumes the
    /// interpreter at the `invokevirtual` with the receiver (and value) still
    /// on its stack: the interpreter dispatches the call itself, and its
    /// `initialValue`, `getEntryAfterMiss`, expunge, `NullPointerException`
    /// and every other effect are the JDK body's. At the top level the frame
    /// is the compiling method's snapshot at `pc` (`has_frame_state_at`,
    /// asked by the caller); inside a splice it is the chain snapshot at `pc`
    /// ([`Self::relax_fence_by_chain`], as the unloaded type check's exit),
    /// never inside a synchronized splice window.
    ///
    /// # Why it never loops
    ///
    /// The guard traps as `DeoptReason::UncommonTrap`. At the top level the
    /// VM charges `(UncommonTrap, pc)` and, past `PER_BCI_DESPEC_LIMIT`,
    /// records the bci in this VM's de-spec registry; any withdrawal recorded
    /// at `pc` keeps the call form here. Inside a splice the chain sink
    /// charges the OUTERMOST call site under the named nested-trap id of this
    /// scope and bci (`deopt_resume.rs` `nested_trap_charge_id`: an
    /// `invokevirtual` bci), which this asks for exactly
    /// ([`crate::nested_trap_speculation_id`]); its wildcard backstop (or the
    /// wildcard `CRATONVM_JIT_IR_NESTED_TRAP_OWN_CHARGE=0` records) withdraws
    /// the whole splice in the planner, and `contains_speculation` answers
    /// `true` for it too. Without a registry there is nothing to withdraw by,
    /// so nothing is admitted.
    pub(in crate::ir) fn tl_trap_site_admits(&mut self, pc: usize, spliced: bool) -> bool {
        if !ir_site_trap_enabled() {
            return false;
        }
        let Some(plan) = self.tl_site_plan.as_deref() else {
            return false;
        };
        let Some(despec) = plan.despec.as_ref() else {
            return false;
        };
        if plan.method_key.is_empty() {
            return false;
        }
        if !spliced {
            let Ok(bci) = u32::try_from(pc) else {
                return false;
            };
            if despec.contains(&plan.method_key, bci) || !self.has_frame_state_at(pc) {
                return false;
            }
            return true;
        }
        if self.sync_splice_open() {
            return false;
        }
        let (Some(outer), Some(inner)) = (self.splice.first(), self.splice.last()) else {
            return false;
        };
        if inner.method_key.is_empty() || pc < inner.base {
            return false;
        }
        let (Ok(outer_bci), Ok(inner_bci)) = (
            u32::try_from(outer.invoke_pc),
            u32::try_from(pc - inner.base),
        ) else {
            return false;
        };
        let id = crate::nested_trap_speculation_id(&inner.method_key, inner_bci);
        if despec.contains_speculation(&plan.method_key, outer_bci, id) {
            return false;
        }
        self.relax_fence_by_chain(pc)
    }

    /// A field read `base.#index` pinned at `ctrl`, threaded on `mem`
    /// (nio21 wave 19, `try_thread_local_get_intrinsic`). No declaring class
    /// is recorded: alias analysis treats it by index alone, the conservative
    /// answer.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir) fn tl_load(
        &mut self,
        ctrl: NodeId,
        mem: &mut NodeId,
        base: NodeId,
        index: u32,
        kind: MemKind,
        ty: IrType,
        pc: usize,
    ) -> NodeId {
        let idx = self.iconst(i64::from(index));
        // t1 wave 8: the trap form's licensing guard, when one was planted
        // ([`Self::tl_license`]; `NO_NODE` adds nothing).
        let inputs = with_licensing_guard(vec![ctrl, *mem, base, idx], self.tl_license);
        let load = self.graph.add(Op::Load(kind), ty, inputs, Some(pc));
        *mem = load;
        load
    }

    #[allow(clippy::too_many_arguments)]
    fn nio_load_field(
        &mut self,
        ctrl: NodeId,
        mem: &mut NodeId,
        base: NodeId,
        index: u32,
        row: u32,
        kind: MemKind,
        ty: IrType,
        tag: u8,
        pc: usize,
    ) -> NodeId {
        let idx = self.iconst(i64::from(index));
        let ld = self.graph.add(
            Op::Load(kind),
            ty,
            with_licensing_guard(vec![ctrl, *mem, base, idx], self.tl_license),
            Some(pc),
        );
        if row != 0 && cratonvm_types::compact_ref_fields_enabled() {
            self.graph
                .field_rows
                .insert(ld, (row - 1, kind == MemKind::Ref, tag));
        }
        *mem = ld;
        ld
    }

    #[allow(clippy::too_many_arguments)]
    fn nio_store_field(
        &mut self,
        ctrl: NodeId,
        mem: &mut NodeId,
        base: NodeId,
        index: u32,
        row: u32,
        kind: MemKind,
        tag: u8,
        value: NodeId,
        pc: usize,
    ) {
        let idx = self.iconst(i64::from(index));
        let st = self.graph.add(
            Op::Store(kind),
            IrType::Memory,
            vec![ctrl, *mem, base, idx, value],
            Some(pc),
        );
        if row != 0 && cratonvm_types::compact_ref_fields_enabled() {
            self.graph
                .field_rows
                .insert(st, (row - 1, kind == MemKind::Ref, tag));
        }
        *mem = st;
    }

    fn finish_nio_intrinsic_with_miss_call(
        &mut self,
        pc: usize,
        info_ptr: usize,
        code_len: usize,
        spliced: bool,
        hit_ctrl: NodeId,
        hit_mem: NodeId,
        hit_val: NodeId,
        misses: Vec<NodeId>,
        mem0: NodeId,
        call_args: Vec<NodeId>,
    ) {
        let miss_ctrl = if misses.len() == 1 {
            misses[0]
        } else {
            self.graph.add(Op::Merge, IrType::Control, misses, Some(pc))
        };
        self.ctrl = miss_ctrl;
        self.mem = mem0;
        let mut call_inputs = Vec::with_capacity(2 + call_args.len());
        call_inputs.push(miss_ctrl);
        call_inputs.push(mem0);
        call_inputs.extend(call_args);
        let call = self
            .graph
            .add(Op::Call { info_ptr }, IrType::Ref, call_inputs, Some(pc));
        self.note_splice_call(call);
        self.mem = call;
        self.plant_call_handler_entry(pc, call, code_len);
        let miss_end = self.ctrl;
        let miss_mem = self.mem;

        let join = self.graph.add(
            Op::Merge,
            IrType::Control,
            vec![hit_ctrl, miss_end],
            Some(pc),
        );
        self.ctrl = join;
        self.mem = self.graph.add(
            Op::Phi,
            IrType::Memory,
            vec![join, hit_mem, miss_mem],
            Some(pc),
        );
        let result = self
            .graph
            .add(Op::Phi, IrType::Ref, vec![join, hit_val, call], Some(pc));
        self.push(result);
        if spliced && self.pending_post_call.is_some_and(|(c, _)| c == call) {
            if ir_nested_miss_post_call_exit_enabled() {
                self.post_call_join_value = Some((result, call));
            } else {
                self.pending_post_call = None;
            }
        }
        self.graph.synthetic_field_access_pcs.insert(pc);
    }

    fn invoke_matches_at(&self, code: &[u8], p: usize, code_len: usize, expect_op: u8, cls: &str, name: &str, desc: &str) -> bool {
        if p + 2 >= code_len || code[p] != expect_op {
            return false;
        }
        if let Some(&(info_ptr, _, _)) = self.invoke_info.get(&p) {
            if info_ptr != 0 {
                // SAFETY: `info_ptr` points to a valid `JitInvokeInfo` recorded for `p`.
                let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
                return info.class_name == cls && info.method_name == name && info.descriptor == desc;
            }
        }
        let Some(site) = self.inline_sites.get(&p) else {
            return false;
        };
        site.method_key
            .strip_prefix(cls)
            .and_then(|rest| rest.strip_prefix('.'))
            .and_then(|rest| rest.strip_prefix(name))
            .and_then(|rest| rest.strip_prefix(':'))
            == Some(desc)
    }

    fn no_live_merges_in_range(&self, start: usize, end_incl: usize) -> bool {
        (start..=end_incl).all(|q| {
            !self.loop_headers.contains(&q)
                && self.merges.get(&q).is_none_or(|m| m.ctrl_inputs.is_empty())
        })
    }

    fn guard_non_null_exact_class(
        &mut self,
        ctrl: NodeId,
        val: NodeId,
        class_id: u32,
        pc: usize,
        trap: bool,
        misses: &mut Vec<NodeId>,
    ) -> NodeId {
        let null = self.aconst_null();
        let non_null = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![val, null], pc);
        let ctrl = self.tl_test(trap, ctrl, non_null, pc, misses);
        let exact = self.add_data(Op::ExactClassIs { class_id }, IrType::Int, vec![val], pc);
        self.tl_test(trap, ctrl, exact, pc, misses)
    }

    fn emit_latin1_byte_to_char_loop(
        &mut self,
        ctrl: NodeId,
        mem: NodeId,
        src_hb: NodeId,
        src_start: NodeId,
        dst_hb: NodeId,
        dst_start: NodeId,
        len: NodeId,
        pc: usize,
    ) -> (NodeId, NodeId) {
        let zero = self.iconst(0);
        let one = self.iconst(1);
        let mask_ff = self.iconst(0xff);
        let loop_head = self.graph.add(Op::Merge, IrType::Control, vec![ctrl], Some(pc));
        let i_phi = self.graph.add(Op::Phi, IrType::Int, vec![loop_head, zero], Some(pc));
        let mem_phi = self.graph.add(Op::Phi, IrType::Memory, vec![loop_head, mem], Some(pc));
        let cond = self.add_data(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i_phi, len], pc);
        let iff = self.graph.add(Op::If, IrType::Control, vec![loop_head, cond], Some(pc));
        let body_ctrl = self.graph.add(Op::Proj(0), IrType::Control, vec![iff], Some(pc));
        let exit_ctrl = self.graph.add(Op::Proj(1), IrType::Control, vec![iff], Some(pc));

        let src_idx = self.add_data(Op::Add, IrType::Int, vec![src_start, i_phi], pc);
        let b = self.graph.add(
            Op::ArrayLoad(MemKind::Byte),
            IrType::Int,
            vec![body_ctrl, mem_phi, src_hb, src_idx],
            Some(pc),
        );
        let ch = self.add_data(Op::And, IrType::Int, vec![b, mask_ff], pc);
        let dst_idx = self.add_data(Op::Add, IrType::Int, vec![dst_start, i_phi], pc);
        let mem_st = self.graph.add(
            Op::ArrayStore(MemKind::Char),
            IrType::Memory,
            vec![body_ctrl, mem_phi, dst_hb, dst_idx, ch],
            Some(pc),
        );
        let i_next = self.add_data(Op::Add, IrType::Int, vec![i_phi, one], pc);

        self.graph.nodes[loop_head as usize].inputs.push(body_ctrl);
        self.graph.nodes[i_phi as usize].inputs.push(i_next);
        self.graph.nodes[mem_phi as usize].inputs.push(mem_st);
        (exit_ctrl, mem_phi)
    }

    fn nio_allow_splice_store(&mut self, base: NodeId, pc: usize) -> bool {
        if !self.splice.is_empty() && !self.splice_store_is_local(base) {
            if !self.relax_fence_by_chain(pc) {
                return false;
            }
            self.splice_committed_bases.insert(base);
        }
        true
    }

    /// Matches `Charset.decode(ByteBuffer.wrap(buff, off, len))` -> `astore slot`
    /// -> optional forward `goto target` -> `return new String(cb.array(), cb.arrayOffset(), cb.length())`.
    /// Returns `(resume_pc, areturn_pc)` so a `goto` skipping an `else` block
    /// resumes at `after_store + 3` (`BlockWalk` range end), while a straight-line
    /// tail resumes at `areturn_pc + 1`.
    fn match_wrap_decode_to_string_return(
        &self,
        code: &[u8],
        pc: usize,
        code_len: usize,
    ) -> Option<(usize, usize)> {
        let mut p = pc + 3;
        if !self.invoke_matches_at(
            code,
            p,
            code_len,
            0xb6,
            "java/nio/charset/Charset",
            "decode",
            "(Ljava/nio/ByteBuffer;)Ljava/nio/CharBuffer;",
        ) {
            return None;
        }
        p += 3;
        let (slot, after_store) = match *code.get(p)? {
            op @ 0x4b..=0x4e => (op - 0x4b, p + 1),
            0x3a => (*code.get(p + 1)?, p + 2),
            _ => return None,
        };
        p = after_store;
        let mut goto_target = None;
        if code.get(p) == Some(&0xa7) && p + 2 < code_len {
            let off = i16::from_be_bytes([code[p + 1], code[p + 2]]) as isize;
            let target = usize::try_from(p as isize + off).ok()?;
            if target < after_store + 3 || target >= code_len {
                return None;
            }
            goto_target = Some(target);
            p = target;
        }
        if code.get(p) != Some(&0xbb) || p + 2 >= code_len {
            return None;
        }
        p += 3;
        if code.get(p) != Some(&0x59) {
            return None;
        }
        p += 1;
        for (cls, name, desc) in [
            ("java/nio/CharBuffer", "array", "()[C"),
            ("java/nio/CharBuffer", "arrayOffset", "()I"),
            ("java/nio/CharBuffer", "length", "()I"),
        ] {
            p = match *code.get(p)? {
                op @ 0x2a..=0x2d if op - 0x2a == slot => p + 1,
                0x19 if code.get(p + 1) == Some(&slot) => p + 2,
                _ => return None,
            };
            if !self.invoke_matches_at(code, p, code_len, 0xb6, cls, name, desc) {
                return None;
            }
            p += 3;
        }
        if !self.invoke_matches_at(code, p, code_len, 0xb7, "java/lang/String", "<init>", "([CII)V") {
            return None;
        }
        p += 3;
        if code.get(p) != Some(&0xb0) {
            return None;
        }
        if let Some(target) = goto_target {
            if !self.no_live_merges_in_range(pc + 1, after_store + 2)
                || !self.no_live_merges_in_range(target + 1, p)
            {
                return None;
            }
            Some((after_store + 3, p))
        } else {
            if !self.no_live_merges_in_range(pc + 1, p) {
                return None;
            }
            Some((p + 1, p))
        }
    }

    /// Fuses `charset.decode(ByteBuffer.wrap(buff, off, len))` followed by
    /// `return new String(cb.array(), cb.arrayOffset(), cb.length());` into
    /// a direct LATIN1 `String` allocation and byte copy when `charset` is
    /// `ISO_8859_1`, bounds are valid, and compact strings are enabled.
    pub(in crate::ir) fn try_nio_charset_decode_to_string_return(
        &mut self,
        code: &[u8],
        pc: usize,
        info_ptr: usize,
        num_args: usize,
        ret_type: u8,
        code_len: usize,
    ) -> Option<usize> {
        let l = self.nio_charset_layout?;
        let spliced = !self.splice.is_empty();
        if info_ptr == 0
            || !ir_nio_charset_enabled()
            || self.splice_handler_mark.is_some()
            || l.iso_class == 0
            || l.str_class == 0
            || l.compact_strings == 0
            || num_args != 3
            || ret_type != b'L'
            || self.sync_direct_calls.contains_key(&pc)
        {
            return None;
        }
        let bound = if spliced {
            let frame = self.splice.last()?;
            if pc < frame.base || pc >= frame.end {
                return None;
            }
            frame.end
        } else {
            if pc >= code_len || !self.has_frame_state_at(pc) || !self.catch_arm_rows.is_empty() {
                return None;
            }
            code_len
        };
        // SAFETY: `info_ptr` points to a valid `JitInvokeInfo` for this site.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if info.class_name != "java/nio/ByteBuffer"
            || info.method_name != "wrap"
            || info.descriptor != "([BII)Ljava/nio/ByteBuffer;"
        {
            return None;
        }
        if !self.tl_trap_site_admits(pc, spliced) {
            return None;
        }
        let (resume_pc, areturn_pc) = self.match_wrap_decode_to_string_return(code, pc, bound)?;
        if spliced {
            let frame = self.splice.last()?;
            if !frame.returns_value {
                return None;
            }
        }
        if self.stack.len() < 4 {
            return None;
        }
        self.tl_license = NO_NODE;
        let len = self.pop();
        let off = self.pop();
        let buff = self.pop();
        let charset = self.pop();
        let mut ctrl = self.ctrl;
        let mut mem = self.mem;
        let null = self.aconst_null();
        let zero = self.iconst(0);
        let one = self.iconst(1);
        let mut misses = Vec::new();

        let cs_nn = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![charset, null], pc);
        ctrl = self.tl_test(true, ctrl, cs_nn, pc, &mut misses);
        let cs_iso = self.add_data(Op::ExactClassIs { class_id: l.iso_class }, IrType::Int, vec![charset], pc);
        ctrl = self.tl_test(true, ctrl, cs_iso, pc, &mut misses);
        let buff_nn = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![buff, null], pc);
        ctrl = self.tl_test(true, ctrl, buff_nn, pc, &mut misses);

        let buff_len = self.graph.add(
            Op::ArrayLength,
            IrType::Int,
            with_licensing_guard(vec![ctrl, mem, buff], self.tl_license),
            Some(pc),
        );
        let end = self.add_data(Op::Add, IrType::Int, vec![off, len], pc);
        let avail = self.add_data(Op::Sub, IrType::Int, vec![buff_len, end], pc);
        let o1 = self.add_data(Op::Or, IrType::Int, vec![off, len], pc);
        let o2 = self.add_data(Op::Or, IrType::Int, vec![end, avail], pc);
        let any_neg = self.add_data(Op::Or, IrType::Int, vec![o1, o2], pc);
        let bounds_ok = self.add_data(Op::Cmp(CmpOp::Ge), IrType::Int, vec![any_neg, zero], pc);
        ctrl = self.tl_test(true, ctrl, bounds_ok, pc, &mut misses);
        self.tl_license = NO_NODE;

        let bytes = self.graph.add(
            Op::NewArray {
                element_type: 8, // T_BYTE
                component_class_id: 0,
            },
            IrType::Ref,
            vec![ctrl, mem, len],
            Some(pc),
        );
        let str_obj = self.graph.add(
            Op::New {
                class_id: l.str_class,
                num_fields: l.str_num_fields as usize,
            },
            IrType::Ref,
            vec![ctrl, bytes],
            Some(pc),
        );
        mem = str_obj;
        self.nio_store_field(
            ctrl,
            &mut mem,
            str_obj,
            l.str_val_index,
            l.str_val_row,
            MemKind::Ref,
            b'L',
            bytes,
            pc,
        );
        self.nio_store_field(
            ctrl,
            &mut mem,
            str_obj,
            l.str_coder_index,
            l.str_coder_row,
            MemKind::Byte,
            b'B',
            zero,
            pc,
        );

        let mem_copy = self.graph.add(
            Op::ByteArrayCopy,
            IrType::Memory,
            vec![ctrl, mem, buff, off, bytes, zero, len],
            Some(pc),
        );
        let exit_ctrl = ctrl;
        let mem_phi = mem_copy;
        let _ = one;

        self.graph.synthetic_field_access_pcs.insert(pc);
        if spliced {
            if let Some(frame) = self.splice.last_mut() {
                frame.multi_return = true;
                frame.exits.push((exit_ctrl, mem_phi, str_obj));
                frame.exit_bci = areturn_pc;
            }
        } else {
            let ret = self.graph.add(
                Op::Return,
                IrType::Void,
                vec![exit_ctrl, str_obj],
                Some(areturn_pc),
            );
            self.graph.exit = ret;
        }
        self.ctrl = NO_NODE;
        self.mem = mem_phi;
        Some(resume_pc)
    }

    pub(in crate::ir) fn try_nio_charset_ir_intrinsic(
        &mut self,
        pc: usize,
        op: u8,
        info_ptr: usize,
        num_args: usize,
        ret_type: u8,
        code_len: usize,
    ) -> bool {
        let Some(l) = self.nio_charset_layout else {
            return false;
        };
        if info_ptr == 0
            || !ir_nio_charset_enabled()
            || self.splice_handler_mark.is_some()
        {
            return false;
        }
        let zgc_loads_ok = !crate::x64::zgc_read_barrier_blocks_inline_fields()
            || crate::x64::zgc_colored_inline_field_load_base().is_some();
        let spliced = !self.splice.is_empty();
        if spliced {
            let in_innermost = self
                .splice
                .last()
                .is_some_and(|f| pc >= f.base && pc < f.end);
            if !in_innermost {
                return false;
            }
        } else if pc >= code_len
            || self.sync_direct_calls.contains_key(&pc)
            || !self.has_frame_state_at(pc)
        {
            return false;
        }
        // SAFETY: `info_ptr` points to a valid `JitInvokeInfo` for this site.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        let trap = !spliced && self.tl_trap_site_admits(pc, false);
        self.tl_license = NO_NODE;

        if op == 0xb8
            && num_args == 1
            && ret_type == b'L'
            && info.method_name == "allocate"
            && ((info.class_name == "java/nio/ByteBuffer"
                && info.descriptor == "(I)Ljava/nio/ByteBuffer;")
                || (l.hcb_class != 0
                    && info.class_name == "java/nio/CharBuffer"
                    && info.descriptor == "(I)Ljava/nio/CharBuffer;"))
        {
            let is_char = info.class_name == "java/nio/CharBuffer";
            let cap = self.pop();
            let ctrl0 = self.ctrl;
            let mem0 = self.mem;
            let zero = self.iconst(0);
            let neg1 = self.iconst(-1);
            let one = self.iconst(1);
            let mut misses = Vec::new();
            let cap_ok = self.add_data(Op::Cmp(CmpOp::Ge), IrType::Int, vec![cap, zero], pc);
            let ctrl = self.tl_test(trap, ctrl0, cap_ok, pc, &mut misses);
            self.tl_license = NO_NODE;

            let (elem_type, buf_class, buf_num_fields, hb_index, hb_row) = if is_char {
                (5, l.hcb_class, l.hcb_num_fields, l.cb_hb_index, l.cb_hb_row)
            } else {
                (8, l.hbb_class, l.hbb_num_fields, l.bb_hb_index, l.bb_hb_row)
            };
            let backing = self.graph.add(
                Op::NewArray {
                    element_type: elem_type,
                    component_class_id: 0,
                },
                IrType::Ref,
                vec![ctrl, mem0, cap],
                Some(pc),
            );
            self.note_splice_local_alloc(backing);
            let buf = self.graph.add(
                Op::New {
                    class_id: buf_class,
                    num_fields: buf_num_fields as usize,
                },
                IrType::Ref,
                vec![ctrl, backing],
                Some(pc),
            );
            self.note_splice_local_alloc(buf);
            let mut mem = buf;
            self.nio_store_field(ctrl, &mut mem, buf, l.buf_mark_index, l.buf_mark_row, MemKind::Int, b'I', neg1, pc);
            self.nio_store_field(ctrl, &mut mem, buf, l.buf_lim_index, l.buf_lim_row, MemKind::Int, b'I', cap, pc);
            self.nio_store_field(ctrl, &mut mem, buf, l.buf_cap_index, l.buf_cap_row, MemKind::Int, b'I', cap, pc);
            let base_off = self.lconst(cratonvm_types::HEADER_SIZE as i64);
            self.nio_store_field(ctrl, &mut mem, buf, l.buf_addr_index, l.buf_addr_row, MemKind::Long, b'J', base_off, pc);
            self.nio_store_field(ctrl, &mut mem, buf, hb_index, hb_row, MemKind::Ref, b'L', backing, pc);
            if !is_char {
                self.nio_store_field(ctrl, &mut mem, buf, l.bb_be_index, l.bb_be_row, MemKind::Int, b'Z', one, pc);
            }

            if trap {
                self.ctrl = ctrl;
                self.mem = mem;
                self.push(buf);
                self.graph.synthetic_field_access_pcs.insert(pc);
            } else {
                self.finish_nio_intrinsic_with_miss_call(
                    pc,
                    info_ptr,
                    code_len,
                    spliced,
                    ctrl,
                    mem,
                    buf,
                    misses,
                    mem0,
                    vec![cap],
                );
            }
            return true;
        }

        if op == 0xb8
            && num_args == 3
            && ret_type == b'L'
            && info.class_name == "java/nio/ByteBuffer"
            && info.method_name == "wrap"
            && info.descriptor == "([BII)Ljava/nio/ByteBuffer;"
        {
            let length = self.pop();
            let offset = self.pop();
            let array = self.pop();
            let ctrl0 = self.ctrl;
            let mem0 = self.mem;
            let null = self.aconst_null();
            let zero = self.iconst(0);
            let neg1 = self.iconst(-1);
            let one = self.iconst(1);
            let mut misses = Vec::new();

            let arr_non_null =
                self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![array, null], pc);
            let mut ctrl = self.tl_test(trap, ctrl0, arr_non_null, pc, &mut misses);

            let arr_len = self.graph.add(
                Op::ArrayLength,
                IrType::Int,
                with_licensing_guard(vec![ctrl, mem0, array], self.tl_license),
                Some(pc),
            );
            let end = self.add_data(Op::Add, IrType::Int, vec![offset, length], pc);
            let rem = self.add_data(Op::Sub, IrType::Int, vec![arr_len, end], pc);
            let o1 = self.add_data(Op::Or, IrType::Int, vec![offset, length], pc);
            let o2 = self.add_data(Op::Or, IrType::Int, vec![o1, end], pc);
            let any_neg = self.add_data(Op::Or, IrType::Int, vec![o2, rem], pc);
            let bounds_ok =
                self.add_data(Op::Cmp(CmpOp::Ge), IrType::Int, vec![any_neg, zero], pc);
            ctrl = self.tl_test(trap, ctrl, bounds_ok, pc, &mut misses);
            self.tl_license = NO_NODE;

            let bb = self.graph.add(
                Op::New {
                    class_id: l.hbb_class,
                    num_fields: l.hbb_num_fields as usize,
                },
                IrType::Ref,
                vec![ctrl, mem0],
                Some(pc),
            );
            self.note_splice_local_alloc(bb);
            let mut mem = bb;
            self.nio_store_field(ctrl, &mut mem, bb, l.buf_mark_index, l.buf_mark_row, MemKind::Int, b'I', neg1, pc);
            self.nio_store_field(ctrl, &mut mem, bb, l.buf_pos_index, l.buf_pos_row, MemKind::Int, b'I', offset, pc);
            self.nio_store_field(ctrl, &mut mem, bb, l.buf_lim_index, l.buf_lim_row, MemKind::Int, b'I', end, pc);
            self.nio_store_field(ctrl, &mut mem, bb, l.buf_cap_index, l.buf_cap_row, MemKind::Int, b'I', arr_len, pc);
            let base_off = self.lconst(cratonvm_types::HEADER_SIZE as i64);
            self.nio_store_field(ctrl, &mut mem, bb, l.buf_addr_index, l.buf_addr_row, MemKind::Long, b'J', base_off, pc);
            self.nio_store_field(ctrl, &mut mem, bb, l.bb_hb_index, l.bb_hb_row, MemKind::Ref, b'L', array, pc);
            self.nio_store_field(ctrl, &mut mem, bb, l.bb_be_index, l.bb_be_row, MemKind::Int, b'Z', one, pc);

            if trap {
                self.ctrl = ctrl;
                self.mem = mem;
                self.push(bb);
                self.graph.synthetic_field_access_pcs.insert(pc);
            } else {
                self.finish_nio_intrinsic_with_miss_call(
                    pc,
                    info_ptr,
                    code_len,
                    spliced,
                    ctrl,
                    mem,
                    bb,
                    misses,
                    mem0,
                    vec![array, offset, length],
                );
            }
            return true;
        }

        if zgc_loads_ok
            && op == 0xb6
            && num_args == 2
            && ret_type == b'L'
            && l.hcb_class != 0
            && l.iso_class != 0
            && (info.class_name == "java/nio/charset/Charset"
                || info.class_name == "sun/nio/cs/ISO_8859_1")
            && info.method_name == "decode"
            && info.descriptor == "(Ljava/nio/ByteBuffer;)Ljava/nio/CharBuffer;"
        {
            let bb = self.stack[self.stack.len() - 1];
            if !self.nio_allow_splice_store(bb, pc) {
                return false;
            }
            let bb = self.pop();
            let recv = self.pop();
            let ctrl0 = self.ctrl;
            let mem0 = self.mem;
            let null = self.aconst_null();
            let zero = self.iconst(0);
            let neg1 = self.iconst(-1);
            let mut misses = Vec::new();

            let mut ctrl = self.guard_non_null_exact_class(ctrl0, recv, l.iso_class, pc, trap, &mut misses);
            ctrl = self.guard_non_null_exact_class(ctrl, bb, l.hbb_class, pc, trap, &mut misses);

            let mut mem = mem0;
            let hb = self.nio_load_field(ctrl, &mut mem, bb, l.bb_hb_index, l.bb_hb_row, MemKind::Ref, IrType::Ref, b'L', pc);
            let hb_non_null =
                self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![hb, null], pc);
            ctrl = self.tl_test(trap, ctrl, hb_non_null, pc, &mut misses);

            let pos = self.nio_load_field(ctrl, &mut mem, bb, l.buf_pos_index, l.buf_pos_row, MemKind::Int, IrType::Int, b'I', pc);
            let lim = self.nio_load_field(ctrl, &mut mem, bb, l.buf_lim_index, l.buf_lim_row, MemKind::Int, IrType::Int, b'I', pc);
            let off = self.nio_load_field(ctrl, &mut mem, bb, l.bb_off_index, l.bb_off_row, MemKind::Int, IrType::Int, b'I', pc);
            let hb_len = self.graph.add(
                Op::ArrayLength,
                IrType::Int,
                with_licensing_guard(vec![ctrl, mem, hb], self.tl_license),
                Some(pc),
            );

            let rem = self.add_data(Op::Sub, IrType::Int, vec![lim, pos], pc);
            let start = self.add_data(Op::Add, IrType::Int, vec![off, pos], pc);
            let end = self.add_data(Op::Add, IrType::Int, vec![off, lim], pc);
            let avail = self.add_data(Op::Sub, IrType::Int, vec![hb_len, end], pc);

            let o1 = self.add_data(Op::Or, IrType::Int, vec![pos, lim], pc);
            let o2 = self.add_data(Op::Or, IrType::Int, vec![off, rem], pc);
            let o3 = self.add_data(Op::Or, IrType::Int, vec![start, end], pc);
            let o4 = self.add_data(Op::Or, IrType::Int, vec![o1, o2], pc);
            let o5 = self.add_data(Op::Or, IrType::Int, vec![o3, avail], pc);
            let any_neg = self.add_data(Op::Or, IrType::Int, vec![o4, o5], pc);
            let bounds_ok =
                self.add_data(Op::Cmp(CmpOp::Ge), IrType::Int, vec![any_neg, zero], pc);
            ctrl = self.tl_test(trap, ctrl, bounds_ok, pc, &mut misses);
            self.tl_license = NO_NODE;

            // Hit path: allocate char[rem] and HeapCharBuffer, advance bb.position = lim, copy bytes.
            let chars = self.graph.add(
                Op::NewArray {
                    element_type: 5, // T_CHAR
                    component_class_id: 0,
                },
                IrType::Ref,
                vec![ctrl, mem, rem],
                Some(pc),
            );
            self.note_splice_local_alloc(chars);
            let cb = self.graph.add(
                Op::New {
                    class_id: l.hcb_class,
                    num_fields: l.hcb_num_fields as usize,
                },
                IrType::Ref,
                vec![ctrl, chars],
                Some(pc),
            );
            self.note_splice_local_alloc(cb);
            mem = cb;

            self.nio_store_field(ctrl, &mut mem, bb, l.buf_pos_index, l.buf_pos_row, MemKind::Int, b'I', lim, pc);
            self.nio_store_field(ctrl, &mut mem, cb, l.buf_mark_index, l.buf_mark_row, MemKind::Int, b'I', neg1, pc);
            self.nio_store_field(ctrl, &mut mem, cb, l.buf_lim_index, l.buf_lim_row, MemKind::Int, b'I', rem, pc);
            self.nio_store_field(ctrl, &mut mem, cb, l.buf_cap_index, l.buf_cap_row, MemKind::Int, b'I', rem, pc);
            let base_off = self.lconst(cratonvm_types::HEADER_SIZE as i64);
            self.nio_store_field(ctrl, &mut mem, cb, l.buf_addr_index, l.buf_addr_row, MemKind::Long, b'J', base_off, pc);
            self.nio_store_field(ctrl, &mut mem, cb, l.cb_hb_index, l.cb_hb_row, MemKind::Ref, b'L', chars, pc);

            let (exit_ctrl, mem_phi) =
                self.emit_latin1_byte_to_char_loop(ctrl, mem, hb, start, chars, zero, rem, pc);

            if trap {
                self.ctrl = exit_ctrl;
                self.mem = mem_phi;
                self.push(cb);
                self.graph.synthetic_field_access_pcs.insert(pc);
            } else {
                self.finish_nio_intrinsic_with_miss_call(
                    pc,
                    info_ptr,
                    code_len,
                    spliced,
                    exit_ctrl,
                    mem_phi,
                    cb,
                    misses,
                    mem0,
                    vec![recv, bb],
                );
            }
            return true;
        }

        if zgc_loads_ok
            && op == 0xb6
            && num_args == 4
            && ret_type == b'L'
            && l.hcb_class != 0
            && l.iso_dec_class != 0
            && l.dec_state_row != 0
            && l.coder_result_class != 0
            && (info.class_name == "java/nio/charset/CharsetDecoder"
                || info.class_name == "sun/nio/cs/ISO_8859_1$Decoder")
            && info.method_name == "decode"
            && info.descriptor == "(Ljava/nio/ByteBuffer;Ljava/nio/CharBuffer;Z)Ljava/nio/charset/CoderResult;"
        {
            let recv_slot = self.stack[self.stack.len() - 4];
            let bb_slot = self.stack[self.stack.len() - 3];
            let cb_slot = self.stack[self.stack.len() - 2];
            if !self.nio_allow_splice_store(recv_slot, pc)
                || !self.nio_allow_splice_store(bb_slot, pc)
                || !self.nio_allow_splice_store(cb_slot, pc)
            {
                return false;
            }
            let eoi = self.pop();
            let cb = self.pop();
            let bb = self.pop();
            let recv = self.pop();
            let ctrl0 = self.ctrl;
            let mem0 = self.mem;
            let null = self.aconst_null();
            let zero = self.iconst(0);
            let one = self.iconst(1);
            let mut misses = Vec::new();

            let mut ctrl = self.guard_non_null_exact_class(ctrl0, recv, l.iso_dec_class, pc, trap, &mut misses);
            ctrl = self.guard_non_null_exact_class(ctrl, bb, l.hbb_class, pc, trap, &mut misses);
            ctrl = self.guard_non_null_exact_class(ctrl, cb, l.hcb_class, pc, trap, &mut misses);

            let mut mem = mem0;
            let state = self.nio_load_field(ctrl, &mut mem, recv, l.dec_state_index, l.dec_state_row, MemKind::Int, IrType::Int, b'I', pc);
            let eoi_bit = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![eoi, zero], pc);
            let new_state = self.add_data(Op::Add, IrType::Int, vec![eoi_bit, one], pc);
            let state_rem = self.add_data(Op::Sub, IrType::Int, vec![new_state, state], pc);
            let state_range = self.add_data(Op::Or, IrType::Int, vec![state, state_rem], pc);
            let state_ok = self.add_data(Op::Cmp(CmpOp::Ge), IrType::Int, vec![state_range, zero], pc);
            ctrl = self.tl_test(trap, ctrl, state_ok, pc, &mut misses);

            let bb_hb = self.nio_load_field(ctrl, &mut mem, bb, l.bb_hb_index, l.bb_hb_row, MemKind::Ref, IrType::Ref, b'L', pc);
            let bb_hb_nn = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![bb_hb, null], pc);
            ctrl = self.tl_test(trap, ctrl, bb_hb_nn, pc, &mut misses);
            let cb_hb = self.nio_load_field(ctrl, &mut mem, cb, l.cb_hb_index, l.cb_hb_row, MemKind::Ref, IrType::Ref, b'L', pc);
            let cb_hb_nn = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![cb_hb, null], pc);
            ctrl = self.tl_test(trap, ctrl, cb_hb_nn, pc, &mut misses);
            if spliced && !self.splice_store_is_local(cb) {
                self.splice_committed_bases.insert(cb_hb);
            }

            let bb_pos = self.nio_load_field(ctrl, &mut mem, bb, l.buf_pos_index, l.buf_pos_row, MemKind::Int, IrType::Int, b'I', pc);
            let bb_lim = self.nio_load_field(ctrl, &mut mem, bb, l.buf_lim_index, l.buf_lim_row, MemKind::Int, IrType::Int, b'I', pc);
            let bb_off = self.nio_load_field(ctrl, &mut mem, bb, l.bb_off_index, l.bb_off_row, MemKind::Int, IrType::Int, b'I', pc);
            let cb_pos = self.nio_load_field(ctrl, &mut mem, cb, l.buf_pos_index, l.buf_pos_row, MemKind::Int, IrType::Int, b'I', pc);
            let cb_lim = self.nio_load_field(ctrl, &mut mem, cb, l.buf_lim_index, l.buf_lim_row, MemKind::Int, IrType::Int, b'I', pc);
            let cb_off = self.nio_load_field(ctrl, &mut mem, cb, l.cb_off_index, l.cb_off_row, MemKind::Int, IrType::Int, b'I', pc);

            let bb_hb_len = self.graph.add(
                Op::ArrayLength,
                IrType::Int,
                with_licensing_guard(vec![ctrl, mem, bb_hb], self.tl_license),
                Some(pc),
            );
            let cb_hb_len = self.graph.add(
                Op::ArrayLength,
                IrType::Int,
                with_licensing_guard(vec![ctrl, mem, cb_hb], self.tl_license),
                Some(pc),
            );

            let bb_rem = self.add_data(Op::Sub, IrType::Int, vec![bb_lim, bb_pos], pc);
            let cb_rem = self.add_data(Op::Sub, IrType::Int, vec![cb_lim, cb_pos], pc);
            let is_underflow = self.add_data(Op::Cmp(CmpOp::Le), IrType::Int, vec![bb_rem, cb_rem], pc);
            let res_iff = self.graph.add(Op::If, IrType::Control, vec![ctrl, is_underflow], Some(pc));
            let t_ctrl = self.graph.add(Op::Proj(0), IrType::Control, vec![res_iff], Some(pc));
            let f_ctrl = self.graph.add(Op::Proj(1), IrType::Control, vec![res_iff], Some(pc));
            let res_u = self.graph.add(
                Op::LoadStatic {
                    class_id: l.coder_result_class,
                    field_index: l.cr_underflow_static_index,
                    type_tag: b'L',
                    is_volatile: false,
                },
                IrType::Ref,
                vec![t_ctrl, mem],
                Some(pc),
            );
            let res_o = self.graph.add(
                Op::LoadStatic {
                    class_id: l.coder_result_class,
                    field_index: l.cr_overflow_static_index,
                    type_tag: b'L',
                    is_volatile: false,
                },
                IrType::Ref,
                vec![f_ctrl, mem],
                Some(pc),
            );
            ctrl = self.graph.add(Op::Merge, IrType::Control, vec![t_ctrl, f_ctrl], Some(pc));
            mem = self.graph.add(Op::Phi, IrType::Memory, vec![ctrl, res_u, res_o], Some(pc));
            let copy_len = self.graph.add(Op::Phi, IrType::Int, vec![ctrl, bb_rem, cb_rem], Some(pc));
            let coder_res = self.graph.add(Op::Phi, IrType::Ref, vec![ctrl, res_u, res_o], Some(pc));

            let src_start = self.add_data(Op::Add, IrType::Int, vec![bb_pos, bb_off], pc);
            let dst_start = self.add_data(Op::Add, IrType::Int, vec![cb_pos, cb_off], pc);
            let src_end = self.add_data(Op::Add, IrType::Int, vec![src_start, copy_len], pc);
            let dst_end = self.add_data(Op::Add, IrType::Int, vec![dst_start, copy_len], pc);
            let src_avail = self.add_data(Op::Sub, IrType::Int, vec![bb_hb_len, src_end], pc);
            let dst_avail = self.add_data(Op::Sub, IrType::Int, vec![cb_hb_len, dst_end], pc);

            let o1 = self.add_data(Op::Or, IrType::Int, vec![bb_pos, bb_lim], pc);
            let o2 = self.add_data(Op::Or, IrType::Int, vec![bb_off, bb_rem], pc);
            let o3 = self.add_data(Op::Or, IrType::Int, vec![src_start, src_end], pc);
            let o4 = self.add_data(Op::Or, IrType::Int, vec![src_avail, cb_pos], pc);
            let o5 = self.add_data(Op::Or, IrType::Int, vec![cb_lim, cb_off], pc);
            let o6 = self.add_data(Op::Or, IrType::Int, vec![cb_rem, dst_start], pc);
            let o7 = self.add_data(Op::Or, IrType::Int, vec![dst_end, dst_avail], pc);
            let u1 = self.add_data(Op::Or, IrType::Int, vec![o1, o2], pc);
            let u2 = self.add_data(Op::Or, IrType::Int, vec![o3, o4], pc);
            let u3 = self.add_data(Op::Or, IrType::Int, vec![o5, o6], pc);
            let u4 = self.add_data(Op::Or, IrType::Int, vec![u1, u2], pc);
            let u5 = self.add_data(Op::Or, IrType::Int, vec![u3, o7], pc);
            let any_neg = self.add_data(Op::Or, IrType::Int, vec![u4, u5], pc);
            let bounds_ok = self.add_data(Op::Cmp(CmpOp::Ge), IrType::Int, vec![any_neg, zero], pc);
            ctrl = self.tl_test(trap, ctrl, bounds_ok, pc, &mut misses);
            self.tl_license = NO_NODE;

            // Hit path: update decoder.state, copy bytes->chars, advance bb/cb positions.
            self.nio_store_field(ctrl, &mut mem, recv, l.dec_state_index, l.dec_state_row, MemKind::Int, b'I', new_state, pc);
            let (exit_ctrl, mut mem_out) = self.emit_latin1_byte_to_char_loop(
                ctrl, mem, bb_hb, src_start, cb_hb, dst_start, copy_len, pc,
            );
            let new_bb_pos = self.add_data(Op::Add, IrType::Int, vec![bb_pos, copy_len], pc);
            let new_cb_pos = self.add_data(Op::Add, IrType::Int, vec![cb_pos, copy_len], pc);
            self.nio_store_field(exit_ctrl, &mut mem_out, bb, l.buf_pos_index, l.buf_pos_row, MemKind::Int, b'I', new_bb_pos, pc);
            self.nio_store_field(exit_ctrl, &mut mem_out, cb, l.buf_pos_index, l.buf_pos_row, MemKind::Int, b'I', new_cb_pos, pc);

            if trap {
                self.ctrl = exit_ctrl;
                self.mem = mem_out;
                self.push(coder_res);
                self.graph.synthetic_field_access_pcs.insert(pc);
            } else {
                self.finish_nio_intrinsic_with_miss_call(
                    pc,
                    info_ptr,
                    code_len,
                    spliced,
                    exit_ctrl,
                    mem_out,
                    coder_res,
                    misses,
                    mem0,
                    vec![recv, bb, cb, eoi],
                );
            }
            return true;
        }

        false
    }

    /// JIT round 21 (lane tl3, `CRATONVM_JIT_IR_REFERS_TO_INLINE`, default ON,
    /// `=0` keeps the call; page
    /// `docs/feature-designs/generative-gc-architecture.md`):
    /// `ref.refersTo(obj)` -- and, inside a splice of it, `refersToImpl(obj)` /
    /// `refersTo0(obj)` -- as `ref.referent == obj`, inline:
    ///
    /// ```text
    /// ref != null                     else the call (its NPE, JEP 358 message and all)
    /// r = ref.referent                (volatile-ordered: re-read every time)
    /// result = (r == obj)
    /// ```
    ///
    /// # Why it is exact
    ///
    /// `Reference.refersTo` is `final` and is `return refersToImpl(obj)`;
    /// `refersToImpl` is package-private in `java.lang.ref`, so only
    /// `PhantomReference` overrides it, and both bodies are `return
    /// refersTo0(obj)`, a native that both registrations answer with the same
    /// function (`native_reference_refers_to`: slot 0 compared by identity,
    /// no keep-alive), as do the `Reference.refersTo` bridge
    /// (`native_ref_refers_to`) and the thin helper
    /// (`jit_refers_to0_body`). So for every receiver the answer is the
    /// identity compare of the referent slot, which is HotSpot's
    /// `Reference.refersTo0` intrinsic too (an `AS_NO_KEEPALIVE` load and a
    /// compare). Admitted only for constant-pool classes of the boot
    /// `java.*` packages that ARE `Reference` subclasses
    /// ([`refers_to_inline_site`]): no other loader can define such a name,
    /// and the verifier makes the receiver one of them.
    ///
    /// The read is compared, never handed out, so no SATB keep-alive is owed
    /// (the same argument as the `ThreadLocal` expansion's `refersTo(key)` and
    /// `tl_find_entry`). It is volatile-ordered so a `while
    /// (!ref.refersTo(null))` loop re-reads the slot the collector clears, and
    /// its home is a back-edge dead-home candidate like any `Ref` load. The
    /// referent's field index is the VM's (`ThreadLocalIrLayout::referent_index`,
    /// `REF_FIELD_REFERENT`), published only under `--real-jdk`; `--compatible`
    /// keeps the call, whose registered natives answer the same.
    ///
    /// t1 wave 10 (lane REFS3): the read skips the checked helper's SATB
    /// keep-alive (`CRATONVM_JIT_IR_REFERS_TO_NO_KEEPALIVE`,
    /// [`Self::note_no_keepalive_referent_load`]). t1 wave 12 (lane REFS4): the
    /// VM answers the referent-only query under `--compatible` too
    /// (`CRATONVM_JIT_IR_REFERS_TO_COMPATIBLE`, real class bytes only), and the
    /// referent read carries `Reference.referent`'s compact row
    /// (`CRATONVM_JIT_IR_REFERS_TO_COMPACT_ROW`, [`Self::note_referent_row`]):
    /// without it every compact receiver's read was a `jit_getfield` call.
    ///
    /// Refused (the call is built as before): no layout, another site, an
    /// armed ZGC barrier without a colored decoder and compare-only row,
    /// a top-level pc with no frame or a caller-held
    /// synchronized row, a splice pc outside the innermost body, non-reference
    /// operands.
    pub(in crate::ir) fn try_reference_refers_to_intrinsic(
        &mut self,
        pc: usize,
        info_ptr: usize,
        num_args: usize,
        ret_type: u8,
        code_len: usize,
    ) -> bool {
        // JIT round 21 (lane refs2): the `ThreadLocal` layout's referent
        // index, else the referent-only answer (which does not wait for the
        // `ThreadLocal` classes; `CRATONVM_JIT_IR_REFERS_TO_OWN_LAYOUT`).
        let Some(referent_index) = self
            .thread_local_layout
            .map(|l| l.referent_index)
            .or(self.refers_to_referent_index)
        else {
            return false;
        };
        if info_ptr == 0 || num_args != 2 || ret_type != b'Z' || !ir_refers_to_inline_enabled() {
            return false;
        }
        // SAFETY: as `try_thread_local_get_intrinsic`'s read of the same row.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if !refers_to_inline_site(info.class_name, info.method_name, info.descriptor) {
            return false;
        }
        let spliced = !self.splice.is_empty();
        if spliced {
            if !self
                .splice
                .last()
                .is_some_and(|f| pc >= f.base && pc < f.end)
            {
                return false;
            }
        } else if pc >= code_len
            || self.sync_direct_calls.contains_key(&pc)
            || !self.has_frame_state_at(pc)
        {
            return false;
        }
        // The VM names a compare-only row only when the resurrection gate cannot arm.
        let colored_base = crate::x64::zgc_colored_inline_field_load_base();
        if (crate::x64::zgc_read_barrier_blocks_inline_fields() || colored_base.is_some())
            && (colored_base.is_none()
                || self.refers_to_referent_row == 0
                || !ir_refers_to_compact_row_enabled()
                || !cratonvm_types::compact_ref_fields_enabled()
                || !ir_refers_to_no_keepalive_enabled())
        {
            return false;
        }
        let Some(ctrl0) = self.ctrl_opt() else {
            return false;
        };
        let Some(at) = self.stack.len().checked_sub(2) else {
            return false;
        };
        let is_ref = |b: &Self, v: Option<&NodeId>| {
            v.and_then(|&v| b.graph.node_opt(v)).map(|n| n.ty) == Some(IrType::Ref)
        };
        if !is_ref(self, self.stack.get(at)) || !is_ref(self, self.stack.get(at + 1)) {
            return false;
        }
        // ── Past this point the site is consumed. ──
        let other = self.pop();
        let recv = self.pop();
        let mem0 = self.mem;
        let mut misses: Vec<NodeId> = Vec::new();
        self.tl_license = NO_NODE;
        let ctrl = if self.receiver_cannot_be_null(recv) {
            ctrl0
        } else {
            let null = self.aconst_null();
            let ok = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![recv, null], pc);
            self.tl_branch(ctrl0, ok, pc, &mut misses)
        };
        let mut mem = mem0;
        let referent = self.tl_load(
            ctrl,
            &mut mem,
            recv,
            referent_index,
            MemKind::Ref,
            IrType::Ref,
            pc,
        );
        self.graph.set_volatile_access(referent);
        self.note_no_keepalive_referent_load(referent);
        // t1 wave 12 (lane REFS4, `CRATONVM_JIT_IR_REFERS_TO_COMPACT_ROW`):
        // the referent's compact row, so a compact receiver reads inline
        // rather than through the checked helper.
        self.note_referent_row(referent);
        let same = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![referent, other], pc);
        self.graph.synthetic_field_access_pcs.insert(pc);
        let report = |b: &Self| {
            if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[ir] Reference.{} inline at bytecode pc {pc} in {}{}",
                    info.method_name,
                    b.method_label.as_deref().unwrap_or("<unknown>"),
                    if spliced { " (spliced body)" } else { "" },
                );
            }
        };
        if misses.is_empty() {
            // A receiver that cannot be null: no call at all.
            self.ctrl = ctrl;
            self.mem = mem;
            self.push(same);
            report(&*self);
            return true;
        }
        let (hit_ctrl, hit_mem) = (ctrl, mem);
        // The miss edge (a null receiver): the call this site always made,
        // from the memory state before the expansion.
        let miss_ctrl = misses[0];
        self.ctrl = miss_ctrl;
        self.mem = mem0;
        let call = self.graph.add(
            Op::Call { info_ptr },
            IrType::Int,
            vec![miss_ctrl, mem0, recv, other],
            Some(pc),
        );
        self.note_splice_call(call);
        self.mem = call;
        self.plant_call_handler_entry(pc, call, code_len);
        let miss_end = self.ctrl;
        let miss_mem = self.mem;
        let join = self.graph.add(
            Op::Merge,
            IrType::Control,
            vec![hit_ctrl, miss_end],
            Some(pc),
        );
        self.ctrl = join;
        self.mem = self.graph.add(
            Op::Phi,
            IrType::Memory,
            vec![join, hit_mem, miss_mem],
            Some(pc),
        );
        let result = self
            .graph
            .add(Op::Phi, IrType::Int, vec![join, same, call], Some(pc));
        self.push(result);
        // As the `ThreadLocal` expansion's miss call (JIT round 21, lane tl2):
        // inside a splice the post-call chain snapshot names the call's result
        // in place of the join's φ, or the call gets none.
        if spliced && self.pending_post_call.is_some_and(|(c, _)| c == call) {
            if ir_nested_miss_post_call_exit_enabled() {
                self.post_call_join_value = Some((result, call));
            } else {
                self.pending_post_call = None;
            }
        }
        report(&*self);
        true
    }

    /// JIT round 21 (lane g1vhsoft, `CRATONVM_JIT_IR_REFERENCE_GET_INLINE`,
    /// default ON, `=0` keeps the call; page
    /// `docs/feature-designs/jit-barrier-lowering.md`):
    /// `ref.get()` at a `WeakReference`- or `SoftReference`-typed
    /// `invokevirtual`, for a receiver of EXACTLY that boot class, as the JDK
    /// body inline (and, JIT tier2 wave 4, lane g1vh21, G1VHSOFT21-2, at a
    /// `Reference`-typed one, for a receiver of exactly `WeakReference`):
    ///
    /// ```text
    /// ExactClassIs(ref, K)            else the call (a null, a subclass, an array)
    /// r = ref.referent                (volatile-ordered; the SATB keep-alive gate)
    /// Soft only:
    ///   ts = ref.timestamp ; ck = SoftReference.clock
    ///   r == null || ts == ck          else the call (which writes the timestamp)
    /// result = r
    /// ```
    ///
    /// # Why it is exact
    ///
    /// `WeakReference` declares no `get()`, so an exactly-`WeakReference`
    /// receiver runs `Reference.get()`: `return this.referent`.
    /// `SoftReference.get()` is `o = super.get(); if (o != null && timestamp
    /// != clock) timestamp = clock; return o;` -- the hit path is the arm that
    /// writes nothing (a null referent, or the timestamp already current) and
    /// returns `o`; the arm that writes is the call, which re-runs the whole
    /// body from the memory state before the expansion (the hit path wrote
    /// nothing). The VM answers a class only where the JDK body is what runs
    /// (no registered `get` native would win: `registered_native_will_run`
    /// on the three triples) and resolves every index from the boot classes
    /// themselves; a JDK whose `SoftReference` lacks `timestamp` / `clock`
    /// answers no `SoftReference`.
    ///
    /// The referent read is the spliced `Reference.get` read of JIT r20
    /// wave 18 / round 21 TL221-1: its pc joins [`Graph::referent_load_pcs`],
    /// so the lowering reads it inline only while the collector's SATB pre-gate
    /// byte is clear (tested after the read) and otherwise takes the checked
    /// `jit_getfield` helper, which logs the referent through the keep-alive
    /// (G1, generational SATB, ZGC's `pre_active`); its home is a back-edge
    /// dead-home candidate (refspin). A racing clear therefore yields `null`
    /// or a referent this thread holds as a root before any later mark can
    /// start: HotSpot's `Reference.get` intrinsic contract. A compact
    /// receiver reads at the row the VM answered (`Graph::field_rows`).
    /// `clock` is a plain `getstatic` of an initialised class, so it is read
    /// inline (no helper, no `<clinit>`): [`ReferenceGetIrLayout::from_table`]
    /// drops the `SoftReference` half otherwise.
    ///
    /// Refused (the call or the splice is built as before): no layout, another
    /// site, an armed ZGC read barrier, a top-level pc with no frame or a
    /// caller-held synchronized row, a splice pc outside the innermost body,
    /// a non-reference receiver.
    pub(in crate::ir) fn try_reference_get_intrinsic(
        &mut self,
        pc: usize,
        info_ptr: usize,
        num_args: usize,
        ret_type: u8,
        code_len: usize,
    ) -> bool {
        let Some(l) = self.reference_get_layout else {
            return false;
        };
        if info_ptr == 0 || num_args != 1 || ret_type != b'L' {
            return false;
        }
        // SAFETY: as `try_thread_local_get_intrinsic`'s read of the same row.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if info.invoke_kind != 0
            || info.method_name != "get"
            || info.descriptor != "()Ljava/lang/Object;"
        {
            return false;
        }
        let (class_id, referent_row, soft) = match info.class_name {
            "java/lang/ref/WeakReference" if l.weak_class != 0 => {
                (l.weak_class, l.weak_referent_row, false)
            }
            "java/lang/ref/SoftReference" if l.soft_class != 0 => {
                (l.soft_class, l.soft_referent_row, true)
            }
            // JIT tier2 wave 4, lane g1vh21 (G1VHSOFT21-2): a `Reference<T>`-typed
            // site. Its receiver is any `Reference`; the exact-class test below
            // admits a `WeakReference` (which declares no `get()`, so it runs
            // `Reference.get()`: `return this.referent`) and sends every other
            // receiver -- a `SoftReference` (its own `get()` writes the
            // timestamp), a `PhantomReference` (`get()` is `null`), a subclass, a
            // null -- to the call this site always made. The `WeakReference`
            // body is the only one a static `Reference` type can expand without
            // a profile; the soft body stays with the `SoftReference`-typed
            // sites. Same kill switch as the other two (the layout is `None`).
            "java/lang/ref/Reference" if l.weak_class != 0 => {
                (l.weak_class, l.weak_referent_row, false)
            }
            _ => return false,
        };
        // A `WeakReference` site the resolver planned a splice for keeps it:
        // that splice (TL221-1's gated read) already serves the referent
        // inline, and its guard admits the subclasses (`WeakHashMap$Entry`,
        // `ThreadLocalMap$Entry`) this exact-class test sends to the call.
        // `SoftReference.get()`'s body (a nested `super.get()`, the `clock`
        // read and the `timestamp` write) measured as a call per use
        // (`R21Refs221Rows soft-get` 441 ns per 16 against `weakref-get` ~70),
        // so its expansion is asked first.
        // When `referent_row != 0`, this collector reads `Reference.referent`
        // inline; in JDK 25 `Reference.get()` delegates to native `get0()`, so
        // deferring to a planned splice turns an inline load into a native call.
        if !soft && referent_row == 0 && self.inline_sites.contains_key(&pc) {
            return false;
        }
        let spliced = !self.splice.is_empty();
        if spliced {
            if !self
                .splice
                .last()
                .is_some_and(|f| pc >= f.base && pc < f.end)
            {
                return false;
            }
        } else if pc >= code_len
            || self.sync_direct_calls.contains_key(&pc)
            || !self.has_frame_state_at(pc)
        {
            return false;
        }
        if crate::x64::zgc_read_barrier_blocks_inline_fields() {
            return false;
        }
        let Some(ctrl0) = self.ctrl_opt() else {
            return false;
        };
        let recv_is_ref = self
            .stack
            .last()
            .and_then(|&v| self.graph.node_opt(v))
            .map(|n| n.ty)
            == Some(IrType::Ref);
        if !recv_is_ref {
            return false;
        }
        // ── Past this point the site is consumed. ──
        let recv = self.pop();
        let mem0 = self.mem;
        let mut misses: Vec<NodeId> = Vec::new();
        // 1. Exactly the JDK class (`0` for a null or an array: the call).
        let class_ok = self.add_data(Op::ExactClassIs { class_id }, IrType::Int, vec![recv], pc);
        let mut ctrl = self.tl_branch(ctrl0, class_ok, pc, &mut misses);
        // 2. The referent, as the spliced `Reference.get` reads it.
        let mut mem = mem0;
        let referent = self.tl_load(
            ctrl,
            &mut mem,
            recv,
            l.referent_index,
            MemKind::Ref,
            IrType::Ref,
            pc,
        );
        self.graph.set_volatile_access(referent);
        self.graph.referent_load_pcs.insert(pc);
        self.reference_get_row(referent, referent_row, b'L');
        self.tl_trust(referent);
        // 3. `SoftReference`: the arm of `get()` that writes nothing.
        if soft {
            let ts = self.tl_load(
                ctrl,
                &mut mem,
                recv,
                l.timestamp_index,
                MemKind::Long,
                IrType::Long,
                pc,
            );
            self.reference_get_row(ts, l.timestamp_row, b'J');
            let clock = self.graph.add(
                Op::LoadStatic {
                    class_id: l.soft_class,
                    field_index: l.clock_index,
                    type_tag: b'J',
                    is_volatile: l.clock_volatile,
                },
                IrType::Long,
                vec![ctrl, mem],
                Some(pc),
            );
            mem = clock;
            let order = self.add_data(Op::LCmp, IrType::Int, vec![ts, clock], pc);
            let zero = self.iconst(0);
            let current = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![order, zero], pc);
            let null = self.aconst_null();
            let cleared = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![referent, null], pc);
            let quiet = self.add_data(Op::Or, IrType::Int, vec![current, cleared], pc);
            ctrl = self.tl_branch(ctrl, quiet, pc, &mut misses);
        }
        self.graph.synthetic_field_access_pcs.insert(pc);
        let (hit_ctrl, hit_mem) = (ctrl, mem);
        // The miss edge: the call this site always made, from the memory
        // state before the expansion (the hit path wrote nothing).
        let miss_ctrl = if misses.len() == 1 {
            misses[0]
        } else {
            self.graph.add(Op::Merge, IrType::Control, misses, Some(pc))
        };
        self.ctrl = miss_ctrl;
        self.mem = mem0;
        let call = self.graph.add(
            Op::Call { info_ptr },
            IrType::Ref,
            vec![miss_ctrl, mem0, recv],
            Some(pc),
        );
        self.note_splice_call(call);
        self.mem = call;
        self.plant_call_handler_entry(pc, call, code_len);
        let miss_end = self.ctrl;
        let miss_mem = self.mem;
        let join = self.graph.add(
            Op::Merge,
            IrType::Control,
            vec![hit_ctrl, miss_end],
            Some(pc),
        );
        self.ctrl = join;
        self.mem = self.graph.add(
            Op::Phi,
            IrType::Memory,
            vec![join, hit_mem, miss_mem],
            Some(pc),
        );
        let result = self
            .graph
            .add(Op::Phi, IrType::Ref, vec![join, referent, call], Some(pc));
        self.push(result);
        // As the `refersTo` / `ThreadLocal` expansions' miss call: inside a
        // splice the post-call chain snapshot names the call's result in
        // place of the join's φ, or the call gets none.
        if spliced && self.pending_post_call.is_some_and(|(c, _)| c == call) {
            if ir_nested_miss_post_call_exit_enabled() {
                self.post_call_join_value = Some((result, call));
            } else {
                self.pending_post_call = None;
            }
        }
        if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] {}.get inline at bytecode pc {pc} in {}{} (receiver class {class_id}, \
                 referent row {referent_row})",
                info.class_name,
                self.method_label.as_deref().unwrap_or("<unknown>"),
                if spliced { " (spliced body)" } else { "" },
            );
        }
        true
    }

    /// JIT round 21 (lane g1vhsoft): the `Reference.get` expansion's own
    /// compact row for `load` (`Graph::field_rows`), from the VM's `1 +
    /// displacement` word `row` (`0` = none: the field-index arms, i.e. the
    /// legacy-cell read or the checked helper). Off without compact reference
    /// fields (the compile door's own rule for its rows).
    pub(in crate::ir) fn reference_get_row(&mut self, load: NodeId, row: u32, tag: u8) {
        if row == 0 || !cratonvm_types::compact_ref_fields_enabled() {
            return;
        }
        self.graph
            .field_rows
            .insert(load, (row - 1, tag == b'L', tag));
    }

    /// nio21 wave 18 (lane L3, `r21-chm-putval-has-no-ir-body-CLOSED-20261012.md`;
    /// `CRATONVM_JIT_IR_ATHROW_IN_MONITOR_TRAP`, default ON, `=0` restores the
    /// refusal of the whole method): an `athrow` reached while a
    /// `monitorenter` of this body is held becomes an unconditional
    /// `Guard { bci: pc }` -- `DeoptReason::UncommonTrap`,
    /// `DeoptAction::Reinterpret`, the compiling method's own snapshot at the
    /// top of `pc`, whose operand stack holds the throwable and whose monitor
    /// stack names the held locks (the shape
    /// [`Self::prune_never_taken_branch`] plants, with a constant condition).
    ///
    /// # Why it is exact
    ///
    /// The interpreter resumes at the `athrow` itself with the lock still
    /// held (a body with a monitor in a snapshot publishes precise resume,
    /// `ir_lower`'s monitor-body vouch, and the frame names the lock
    /// `relock == false`), throws, and its own handler lookup finds the
    /// block's catch-any release handler: the exception, its stack trace
    /// (filled when it was constructed, before this point) and the lock
    /// release are the interpreter's. The `Op::Throw` this tier would build
    /// instead leaves the frame without releasing the lock, which is why the
    /// arm refused.
    ///
    /// # Cost
    ///
    /// Every execution of such an `athrow` deopts. The arms that reach one
    /// (`ConcurrentHashMap.putVal`'s "Recursive update", a
    /// `synchronized` block's own validation throws) are exceptional; the
    /// alternative was no optimizing body for the whole method.
    ///
    /// Refused (the build bails as before): the switch off, inside a splice
    /// (a combined pc has no flat frame), a `synchronized` method, dead
    /// control, no frame state at `pc`.
    pub(in crate::ir) fn trap_athrow_in_monitor(&mut self, pc: usize) -> bool {
        if !ir_athrow_in_monitor_trap_enabled() || !self.splice.is_empty() {
            return false;
        }
        // A `synchronized` method's own lock (held by the door around the
        // body, or by the body itself) is not this arm's: such a body keeps
        // its trap-free shape (the caller-held direct sites bind only a body
        // with no trap point), and the build bails as before.
        if self.graph.method_self_lock.is_some()
            || self.graph.method_monitor_param.is_some()
            || self.graph.method_monitor_class.is_some()
        {
            return false;
        }
        let Some(ctrl) = self.ctrl_opt() else {
            return false;
        };
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
        if ir_bail_reporting() {
            eprintln!(
                "[ir] athrow inside a held monitor at bytecode pc {pc} in {} -- deopts to the \
                 interpreter at the athrow, which releases the lock in the block's handler",
                self.method_label.as_deref().unwrap_or("<unknown>"),
            );
        }
        true
    }

    /// nio21 wave 17 (lane L4): is `pc`, reached by the pc-order walk with dead
    /// control, code that only a pruned never-taken edge reached? Asked only
    /// at or past [`Self::never_taken_dead_from`]. Exact rather than "anything
    /// past the target": `pc` must be unreachable from the entry along normal
    /// flow once every taken edge pruned SO FAR is removed. An edge pruned
    /// later (a higher pc) is not yet in the list, which can only make the
    /// answer "reachable" -- the walk then refuses the build exactly as it did
    /// before this pass existed. So a bookkeeping slip elsewhere still bails;
    /// it is never stepped over as "cold".
    pub(in crate::ir) fn never_taken_pc_is_dead(
        &mut self,
        verified: &cratonvm_reader::VerifiedCode,
        code_len: usize,
        pc: usize,
    ) -> bool {
        if self.never_taken_edges.is_empty() || pc < self.never_taken_dead_from {
            return false;
        }
        let edges = self.never_taken_edges.len();
        if self.never_taken_live.as_ref().map(|(n, _)| *n) != Some(edges) {
            let live = reachable_pcs_without_edges(verified, code_len, &self.never_taken_edges);
            self.never_taken_live = Some((edges, live));
        }
        self.never_taken_live
            .as_ref()
            .is_some_and(|(_, live)| !live.contains(&pc))
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
    /// # Three refusals
    ///
    /// * **Inside a splice.** `splice_guard_seen` refuses a graph that built a
    ///   guard inside a relocated body, because such a guard would resolve its
    ///   frame state from the caller's snapshot at the `invoke`. Rather than
    ///   set that flag and lose the whole method, simply do not speculate
    ///   there.
    /// * **No frame state at this bci.** A guard whose bci has no
    ///   `SafepointSnapshot` cannot be resumed, so there would be nothing to
    ///   deopt *to*. `plant_uncommon_trap` makes the same check for the same
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
    pub(in crate::ir) fn prune_always_taken_branch(&mut self, pc: usize, cmp: NodeId, target_pc: usize) -> bool {
        if !self.splice.is_empty() {
            return self.prune_spliced_always_taken_branch(pc, cmp, target_pc);
        }
        if !self.pruned_always_taken.contains(&pc) {
            return false;
        }
        if target_pc <= pc {
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
        if !self.has_frame_state_at(pc) {
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
        if !self.graph.unstable_if_trap_pcs.contains(&pc) {
            self.graph.unstable_if_trap_pcs.push(pc);
        }
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

    /// Round 16 wave 7 (lane arraylist, AL16-1): replace the branch at `pc`
    /// inside a spliced body, whose fall-through is one of the body's cold
    /// throw arms ([`cold_throw_arms`]), with a GUARD plus an unconditional
    /// jump to `target_pc`: the shape [`Self::prune_always_taken_branch`]
    /// builds from a profile, built here from the arm's shape instead.
    ///
    /// # Why the deopt state is exact
    ///
    /// The guard fails exactly when the branch would fall into the arm. It
    /// deopts at `pc` itself -- the branch, not yet executed -- and the
    /// interpreter re-executes the branch, falls into the arm and runs it:
    /// allocates the exception, runs its constructor and throws. The stack
    /// trace is therefore filled in by interpreter frames, the spliced
    /// callee's included, at the bcis the bytecode gives them. Two frames can
    /// describe `pc`:
    ///
    /// * the CHAIN snapshot the walk pushed at the top of `pc` (the callee at
    ///   its own bci over every caller parked mid-`invoke`), which the
    ///   lowerer's `Op::Guard` arm publishes when it can; or
    /// * the flat frame at the outermost `invoke`, which re-executes the whole
    ///   call in the interpreter. Sound only while nothing the splice already
    ///   ran is committed -- so when something is (a committed store or call,
    ///   or an impure spliced body), the guard is planted only with the chain
    ///   REQUIRED ([`Self::relax_fence_by_chain`], the receiver null guard's
    ///   rule), and otherwise not at all.
    ///
    /// # Refusals (the branch is built as an ordinary `Op::If`, and the walk
    /// then refuses at the masked arm's `athrow`)
    ///
    /// * the switch off, or no splice open, or `pc` outside the innermost body;
    /// * a body walked by ranges (a rotated loop): the arm is skipped by pc;
    /// * a synchronized splice window, which may not trap anywhere;
    /// * a branch that is not an arm's, or whose target is not the
    ///   instruction after the arm's `athrow`;
    /// * dead control (the branch is unreachable: nothing to do).
    pub(in crate::ir) fn prune_cold_throw_arm(
        &mut self,
        code: &[u8],
        pc: usize,
        cmp: NodeId,
        target_pc: usize,
    ) -> bool {
        if !self.inline_cold_athrow || self.sync_splice_open() {
            return false;
        }
        let Some(frame) = self.splice.last() else {
            return false;
        };
        if !frame.ranges.is_empty() {
            return false;
        }
        let (base, end) = (frame.base, frame.end);
        if pc < base || pc >= end || target_pc <= pc || target_pc >= end {
            return false;
        }
        let rel = pc - base;
        let Some(arm) = self
            .cold_throw_shape_of(code, base, end)
            .0
            .iter()
            .find(|a| a.branch_pc == rel)
            .copied()
        else {
            return false;
        };
        if base + arm.athrow_pc + 1 != target_pc || base + arm.start != pc + 3 {
            return false;
        }
        // `CRATONVM_JIT_IR_SPLICED_THROW_EDGES`: an arm the resolver left
        // unmasked (it throws a value it does not construct) is walked when
        // its `athrow` may take a spliced throw edge.
        if code.get(base + arm.start).is_some_and(|&op| op != 0x01)
            && self.spliced_throw_target().is_some()
        {
            return false;
        }
        // JIT round 23 wave 10 (lane L1, CompletableFuture page;
        // `CRATONVM_JIT_IR_SPLICE_COLD_BRANCH_BOTH_ARMS`): the branch's TAKEN
        // edge enters a masked TAIL arm of the same body
        // (`reportJoin`'s `ifeq 53` at 45: `throw (CompletionException) x` on
        // the fall-through, `throw wrapInCompletionException(x)` on the jump).
        // Both ways out of the branch throw, so neither may become the fast
        // path: the guard below would send live control into the masked tail
        // arm, whose `athrow` carries `null` and refuses the build (the
        // `getNow` splice was dropped there). See
        // `both_arms_cold_branch_tail_end`.
        let tail_end = self.both_arms_cold_branch_tail_end(code, base, end, target_pc);
        let Some(ctrl) = self.ctrl_opt() else {
            return false;
        };
        let needs_chain = !self.spliced_bodies_pure
            || !self.splice_committed_bases.is_empty()
            || self.splice_call_committed;
        if needs_chain && !self.relax_fence_by_chain(pc) {
            return false;
        }
        if let Some(tail_end) = tail_end {
            // An always-failing guard: the interpreter re-executes the branch
            // and runs whichever arm it takes. The `Op::Throw` behind it is
            // the block's terminator, unreachable at run time (as behind every
            // always-failing athrow trap).
            let zero = self.iconst(0);
            self.graph.add(
                Op::Guard { bci: pc },
                IrType::Void,
                vec![ctrl, zero],
                Some(pc),
            );
            let null = self.aconst_null();
            let throw = self.graph.add(
                Op::Throw,
                IrType::Void,
                vec![ctrl, self.mem, null],
                Some(pc),
            );
            self.graph.exit = throw;
            self.ctrl = NO_NODE;
            self.cold_throw_dead = (base + arm.start, tail_end);
            if ir_bail_reporting() {
                eprintln!(
                    "[ir] cold throw branch TRAPPED at combined pc {pc} (callee bci {rel}): both \
                     arms throw ({}..={} and the tail arm up to {}) in {}",
                    arm.start,
                    arm.athrow_pc,
                    tail_end - base - 1,
                    self.method_label.as_deref().unwrap_or("<unknown>"),
                );
            }
            return true;
        }
        // `cmp` is 1 exactly when the branch is TAKEN (around the arm): a
        // non-zero `cmp` continues, a zero one deopts into the arm.
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, cmp],
            Some(pc),
        );
        self.add_merge_predecessor(target_pc);
        self.ctrl = NO_NODE;
        self.cold_throw_dead = (base + arm.start, target_pc);
        if ir_bail_reporting() {
            eprintln!(
                "[ir] cold throw arm TRAPPED at combined pc {pc} (callee bci {rel}, arm {}..={}) in {} -- \
                 the spliced body keeps its fast path, the throw resumes in the interpreter",
                arm.start,
                arm.athrow_pc,
                self.method_label.as_deref().unwrap_or("<unknown>"),
            );
        }
        true
    }

    /// JIT round 23 wave 10 (`CRATONVM_JIT_IR_SPLICE_COLD_BRANCH_BOTH_ARMS`):
    /// when the branch whose fall-through is a cold throw arm jumps to
    /// `target_pc`, the first instruction of a MASKED [`COLD_THROW_TAIL_ARM`]
    /// of the same body (`aconst_null; nop...; athrow`), the end (exclusive,
    /// combined pc) of that tail arm; `None` otherwise or with the switch off.
    /// [`Self::prune_cold_throw_arm`] then traps the branch outright.
    pub(in crate::ir) fn both_arms_cold_branch_tail_end(
        &mut self,
        code: &[u8],
        base: usize,
        end: usize,
        target_pc: usize,
    ) -> Option<usize> {
        // Lives inside the opt-in athrow-exit path, so the default build keeps
        // its old answer (the site is dropped) until that switch is flipped.
        if !ir_splice_athrow_exit_enabled()
            || !ir_splice_cold_branch_both_arms_enabled()
            || !ir_cold_throw_tail_arms_enabled()
        {
            return None;
        }
        if code.get(target_pc) != Some(&0x01) {
            return None; // not masked: its athrow throws a real value
        }
        self.cold_throw_shape_of(code, base, end)
            .0
            .iter()
            .find(|a| a.branch_pc == COLD_THROW_TAIL_ARM && base + a.start == target_pc)
            .map(|a| base + a.athrow_pc + 1)
    }

    /// The cold throw arms and side entries of the spliced body
    /// `code[base..end]`, computed once per body.
    pub(in crate::ir) fn cold_throw_shape_of(
        &mut self,
        code: &[u8],
        base: usize,
        end: usize,
    ) -> &(Vec<ColdThrowArm>, Vec<ColdThrowSideEntry>) {
        self.cold_throw_arms_by_base.entry(base).or_insert_with(|| {
            code.get(base..end)
                .and_then(|body| {
                    cold_throw_shape(body, body.len(), ir_cold_throw_side_entries_enabled())
                })
                .unwrap_or_default()
        })
    }

    /// al16 (2026-10-05): the branch at `pc` inside a spliced body jumps
    /// straight to one of the body's cold throw arms ([`ColdThrowSideEntry`],
    /// the first test of `if (a || b) throw ...`). Replace it with a GUARD that
    /// the branch is NOT taken, and continue on the fall-through. The arm's own
    /// branch becomes a guard through [`Self::prune_cold_throw_arm`], so the
    /// masked arm has no live way in.
    ///
    /// Exact for the reason [`Self::prune_cold_throw_arm`] gives: the guard
    /// fails exactly when the branch would jump into the arm, it deopts at
    /// `pc` (the branch, not yet executed), and the interpreter re-executes
    /// the branch, enters the arm and throws from the callee's own frame. The
    /// same refusals, plus a `cmp` that is not a two-operand `Op::Cmp` (the
    /// guard needs its exact negation). A refusal builds the ordinary `Op::If`;
    /// its taken edge then reaches the masked arm with live control and the
    /// build refuses at the arm's `athrow`, keeping the call.
    pub(in crate::ir) fn prune_cold_throw_side_entry(
        &mut self,
        code: &[u8],
        pc: usize,
        cmp: NodeId,
        target_pc: usize,
    ) -> bool {
        if !self.inline_cold_athrow || self.sync_splice_open() {
            return false;
        }
        let Some(frame) = self.splice.last() else {
            return false;
        };
        if !frame.ranges.is_empty() {
            return false;
        }
        let (base, end) = (frame.base, frame.end);
        if pc < base || pc >= end || target_pc <= pc || target_pc >= end {
            return false;
        }
        let rel = pc - base;
        let found = self
            .cold_throw_shape_of(code, base, end)
            .1
            .iter()
            .any(|s| s.side_pc == rel && base + s.arm_start == target_pc);
        if !found {
            return false;
        }
        // As in `prune_cold_throw_arm`: an unmasked arm a spliced throw edge
        // can take is entered by its branch.
        if code.get(target_pc).is_some_and(|&op| op != 0x01)
            && self.spliced_throw_target().is_some()
        {
            return false;
        }
        let (cc, a, b) = match self.graph.node_opt(cmp) {
            Some(n) => match (&n.op, n.inputs.as_slice()) {
                (Op::Cmp(cc), &[a, b]) => (*cc, a, b),
                _ => return false,
            },
            None => return false,
        };
        let Some(ctrl) = self.ctrl_opt() else {
            return false;
        };
        let needs_chain = !self.spliced_bodies_pure
            || !self.splice_committed_bases.is_empty()
            || self.splice_call_committed;
        if needs_chain && !self.relax_fence_by_chain(pc) {
            return false;
        }
        // `not_taken` is 1 exactly when the branch falls through: a non-zero
        // value continues, a zero one deopts into the arm.
        let not_taken = self.add_data(Op::Cmp(cc.negate()), IrType::Int, vec![a, b], pc);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, not_taken],
            Some(pc),
        );
        if ir_bail_reporting() {
            eprintln!(
                "[ir] cold throw side entry TRAPPED at combined pc {pc} (callee bci {rel}, into the arm at {}) in {}",
                target_pc - base,
                self.method_label.as_deref().unwrap_or("<unknown>"),
            );
        }
        true
    }

    /// JIT round 23 wave 8e (`CRATONVM_JIT_IR_COLD_THROW_TAIL_ARM_DEAD_SKIP`):
    /// is `pc` inside a [`COLD_THROW_TAIL_ARM`] (from its first instruction to
    /// its `athrow`) of the innermost spliced body, which the walk may step
    /// over when it arrives there with dead control? `false` with the switch
    /// off, for a body walked by ranges (a rotated loop), and outside a splice.
    pub(in crate::ir) fn pc_in_dead_tail_cold_throw_arm(&mut self, code: &[u8], pc: usize) -> bool {
        if !ir_cold_throw_tail_arm_dead_skip_enabled() || !ir_cold_throw_tail_arms_enabled() {
            return false;
        }
        let Some(frame) = self.splice.last() else {
            return false;
        };
        if !frame.ranges.is_empty() {
            return false;
        }
        let (base, end) = (frame.base, frame.end);
        if pc < base || pc >= end {
            return false;
        }
        let rel = pc - base;
        self.cold_throw_shape_of(code, base, end)
            .0
            .iter()
            .any(|a| a.branch_pc == COLD_THROW_TAIL_ARM && a.start <= rel && rel <= a.athrow_pc)
    }

    /// Dispatch `key` over the strictly ascending live cases `live` with a
    /// balanced tree of signed `key < pivot` tests whose leaves are short
    /// equality chains; every leaf that matches nothing takes the default edge.
    /// See [`switch_search_tree_cases`] for when the switch arm asks for it.
    ///
    /// Every edge carries the frame the switch left (it consumed only `key`),
    /// so each leaf records its predecessors exactly as the chain does. An
    /// explicit worklist rather than recursion: the depth is logarithmic, but a
    /// background compiler thread is no place to find out otherwise. Leaves
    /// ctrl as the last leaf left it; the caller clears it.
    pub(in crate::ir) fn build_switch_search_tree(
        &mut self,
        key: NodeId,
        live: &[(i32, usize)],
        default_target: usize,
        pc: usize,
    ) {
        let mut work: Vec<(usize, usize, NodeId)> = vec![(0, live.len(), self.ctrl)];
        while let Some((lo, hi, ctrl)) = work.pop() {
            self.ctrl = ctrl;
            let Some(range) = live.get(lo..hi) else {
                self.refuse(line!(), pc);
                return;
            };
            if range.len() <= SWITCH_SEARCH_TREE_LEAF_CASES {
                for &(match_val, target) in range {
                    let cval = self.iconst(i64::from(match_val));
                    let cmp = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![key, cval], pc);
                    let if_node =
                        self.graph
                            .add(Op::If, IrType::Control, vec![self.ctrl, cmp], Some(pc));
                    let hit = self
                        .graph
                        .add(Op::Proj(0), IrType::Control, vec![if_node], Some(pc));
                    let miss =
                        self.graph
                            .add(Op::Proj(1), IrType::Control, vec![if_node], Some(pc));
                    self.ctrl = hit;
                    self.add_merge_predecessor(target);
                    self.ctrl = miss;
                }
                self.add_merge_predecessor(default_target);
                continue;
            }
            // `range.len() > LEAF_CASES >= 1`, so both halves are non-empty.
            let mid = lo + range.len() / 2;
            let Some(&(pivot, _)) = live.get(mid) else {
                self.refuse(line!(), pc);
                return;
            };
            let cval = self.iconst(i64::from(pivot));
            let below = self.add_data(Op::Cmp(CmpOp::Lt), IrType::Int, vec![key, cval], pc);
            let if_node = self
                .graph
                .add(Op::If, IrType::Control, vec![self.ctrl, below], Some(pc));
            let lower = self
                .graph
                .add(Op::Proj(0), IrType::Control, vec![if_node], Some(pc));
            let upper = self
                .graph
                .add(Op::Proj(1), IrType::Control, vec![if_node], Some(pc));
            work.push((mid, hi, upper));
            work.push((lo, mid, lower));
        }
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
    ///
    /// Round nio21 wave 10 (lane L2): a divisor that is an `Op::Const` other
    /// than 0 at the division's width builds NO guard (returns [`NO_NODE`]),
    /// under [`ir_splice_const_division_enabled`]. Such a division cannot
    /// throw, and the lowerer already emits neither the zero test nor a deopt
    /// point for it (`ir_lower::Lowerer::emit_div_zero_guard`); -1 and `MIN`
    /// included: `emit_div_by_const` answers `x / -1` with `NEG` (`NEG MIN ==
    /// MIN`) and `x % -1` with 0, `CRATONVM_JIT_IR_CONST_DIV=0` keeps the
    /// `CMP RCX, -1` overflow fix-up for a constant -1, and `IDIV` by `MIN`
    /// cannot raise `#DE`. Outside a splice the optimizer folded this guard
    /// away anyway; inside one it raised `splice_guard_seen` and refused the
    /// build, which is what kept every `x / 1000` callee a call
    /// (`r21-ir-splice-refuses-a-division-by-a-non-zero-constant-CLOSED-20261012.md`).
    pub(in crate::ir) fn add_div_zero_guard(&mut self, divisor: NodeId, ty: IrType, pc: usize) -> NodeId {
        if const_divisor_is_nonzero(&self.graph, ty, divisor) && ir_splice_const_division_enabled()
        {
            return NO_NODE;
        }
        let Some(ctrl) = self.ctrl_opt() else {
            return NO_NODE;
        };
        let zero = if ty == IrType::Long {
            self.lconst(0)
        } else {
            self.iconst(0)
        };
        let cond = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![divisor, zero], pc);
        if !self.splice.is_empty() && !self.splice_div_guard_resumes(pc) {
            self.splice_guard_seen = true;
            self.splice_guard_pc.get_or_insert(pc);
        }
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, cond],
            Some(pc),
        )
    }

    /// May the zero-divisor guard of a division at combined pc `pc`, inside an
    /// open splice, stand -- rather than raise `splice_guard_seen` and refuse
    /// the build? Round nio21 wave 10 (lane L2,
    /// [`ir_splice_div_guard_enabled`]): the rule of
    /// [`Self::prune_cold_throw_arm`]'s guard, which deopts from a spliced
    /// body today. The guard resumes either through the chain snapshot at
    /// `pc` (the callee's own frame at the division, so the interpreter
    /// re-executes it and throws `ArithmeticException` from the callee) or
    /// through the flat frame at the outermost `invoke`, which re-runs the
    /// whole call -- sound only while nothing the splice ran is committed, so
    /// when something is, only with the chain REQUIRED
    /// ([`Self::relax_fence_by_chain`]). Never inside a synchronized splice
    /// window, which may not trap. The lowerer keys the division's own
    /// zero test off this guard (`ir_lower::Lowerer::emit_div_zero_guard`'s
    /// anchored shape: a placeholder, never a bare `IDIV`).
    pub(in crate::ir) fn splice_div_guard_resumes(&mut self, pc: usize) -> bool {
        if !ir_splice_div_guard_enabled() || self.sync_splice_open() {
            return false;
        }
        let needs_chain = !self.spliced_bodies_pure
            || !self.splice_committed_bases.is_empty()
            || self.splice_call_committed;
        !needs_chain || self.relax_fence_by_chain(pc)
    }

    /// FP value tier (inc 30): a `float` constant, stored as its raw 32-bit
    /// IEEE-754 bit pattern (zero-extended into the `u64` payload). Typed
    /// `IrType::Float` so the lowerer marshals it through XMM. No dedup — GVN
    /// collapses duplicates. (`const_value`/`is_const_val` in `ir_optimize`
    /// match only `Op::Const`, never `Op::ConstF`, so the integer fold/identity
    /// passes never touch a float constant — FP arithmetic is never mis-folded.)
    pub(in crate::ir) fn fconst(&mut self, val: f32) -> NodeId {
        self.graph.add(
            Op::ConstF(val.to_bits() as u64),
            IrType::Float,
            vec![],
            None,
        )
    }

    /// FP value tier (inc 30): a `double` constant, stored as its raw 64-bit
    /// IEEE-754 bit pattern. Typed `IrType::Double`.
    pub(in crate::ir) fn dconst(&mut self, val: f64) -> NodeId {
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
    pub(in crate::ir) fn ensure_merge(&mut self, target_pc: usize) -> &mut MergeState {
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
    ///
    /// nio21 wave 19 (lane L3, from lane L4's `ConcurrentHashMap.putVal`
    /// finding): each INPUT is stripped too, recursively and bounded
    /// ([`STRIP_TRIVIAL_PHI_INPUT_DEPTH`]), before it is compared. A join after
    /// an inner loop reads `phi(loopphi(f, itself), f)`: the inner loop's φ is
    /// trivial, but its id is not `f`'s, so the join counted as two values and
    /// the `monitorexit` of the lock held across the loop refused the method.
    /// Sound for the same reason the one-level strip is: an input that strips
    /// to `x` IS `x` on its edge, so a φ all of whose inputs strip to `x` (or
    /// to itself) is `x`. `CRATONVM_JIT_IR_STRIP_TRIVIAL_PHI_INPUTS=0` compares
    /// raw input ids again. [`Self::strip_settled_trivial_phis`] (the
    /// `holdsLock` fold) is unchanged.
    pub(in crate::ir) fn strip_trivial_phis(&self, id: NodeId) -> NodeId {
        let depth = if ir_strip_trivial_phi_inputs_enabled() {
            STRIP_TRIVIAL_PHI_INPUT_DEPTH
        } else {
            0
        };
        self.strip_trivial_phis_at(id, depth)
    }

    /// [`Self::strip_trivial_phis`] with `depth` levels of input stripping left.
    pub(in crate::ir) fn strip_trivial_phis_at(&self, id: NodeId, depth: u32) -> NodeId {
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
                let v = if depth > 0 && v != cur {
                    self.strip_trivial_phis_at(v, depth - 1)
                } else {
                    v
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
    pub(in crate::ir) fn same_monitor_object(&self, a: NodeId, b: NodeId) -> bool {
        a == b || self.strip_trivial_phis(a) == self.strip_trivial_phis(b)
    }

    /// Round 14 wave 5 (lane sync5): [`Self::same_monitor_object`] for a
    /// `Thread.holdsLock` fold, which bakes its answer for EVERY execution of
    /// the site. During the walk a loop-header φ has only its entry inputs
    /// until the back edges are patched, so `φ(held, <pending>)` looks trivial
    /// and would fold `holdsLock(x)` in `x = held; loop { holdsLock(x); x =
    /// other; }` to `1` on every iteration. The monitor matching can afford
    /// that reading (a `monitorexit` still unlocks the node it names); a fold
    /// cannot. So this strips only φs no open loop header owns, except a
    /// local-0 φ of the method's own loops when that local is provably the
    /// held receiver throughout ([`Self::method_monitor_slot0_loops_below`]).
    pub(in crate::ir) fn holds_lock_same_object(&self, held: NodeId, arg: NodeId) -> bool {
        held == arg || self.strip_settled_trivial_phis(held) == self.strip_settled_trivial_phis(arg)
    }

    /// [`Self::strip_trivial_phis`], stopping at a loop-header φ whose inputs
    /// may still grow (see [`Self::holds_lock_same_object`]).
    pub(in crate::ir) fn strip_settled_trivial_phis(&self, id: NodeId) -> NodeId {
        let mut cur = id;
        for _ in 0..32 {
            let Some(node) = self.graph.node_opt(cur) else {
                return cur;
            };
            if node.op != Op::Phi || node.ty == IrType::Memory {
                return cur;
            }
            let loop_owned = self.loop_phis.iter().any(|(&header, lp)| {
                let slot0_settled = self
                    .method_monitor_slot0_loops_below
                    .is_some_and(|below| header < below)
                    && lp.local_phis.first() == Some(&cur);
                !slot0_settled && (lp.local_phis.contains(&cur) || lp.stack_phis.contains(&cur))
            });
            if loop_owned {
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

    /// Do two abstract monitor stacks describe the same held-lock sequence?
    pub(in crate::ir) fn monitor_stacks_agree(&self, a: &[NodeId], b: &[NodeId]) -> bool {
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
    pub(in crate::ir) fn add_merge_predecessor(&mut self, target_pc: usize) {
        if self.ensure_merge(target_pc).visited {
            self.patch_loop_backedge(target_pc);
            return;
        }
        // JIT round 23 wave 2e (W4L1-2): the memo this edge carries into a
        // forward join of the compiling method's own code.
        if self.field_memo_precise_joins
            && self.splice.is_empty()
            && !self.block_walk
            && !self.loop_headers.contains(&target_pc)
        {
            self.field_load_memo_scan();
            let memo = self.field_load_memo.clone();
            self.field_memo_join_snapshots.entry(target_pc).or_default().push(memo);
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
    pub(in crate::ir) fn join_monitor_stacks(&mut self, snapshots: &[Vec<NodeId>]) {
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
    pub(in crate::ir) fn activate_loop_header(&mut self, target_pc: usize) {
        let visited = match self.merges.get(&target_pc) {
            Some(s) if !s.ctrl_inputs.is_empty() => s.visited,
            _ => return,
        };
        if visited {
            self.refuse(line!(), target_pc);
            return;
        }
        // Every predecessor of a join carries the same abstract frame in
        // verified bytecode. Where they disagree there is no one frame to
        // record, and the φ construction below would silently pad the short
        // one with `NO_NODE` / truncate the long one — see
        // [`Self::predecessor_frames_agree`].
        let agree = self
            .merges
            .get(&target_pc)
            .is_some_and(|s| self.predecessor_frames_agree(s));
        if !agree {
            self.refuse(line!(), target_pc);
            return;
        }
        // Taken, not cloned: see `MergeState::take_for_activation`. The first
        // predecessor's frame stays behind for `patch_loop_backedge`.
        let Some(mut state) = self
            .merges
            .get_mut(&target_pc)
            .map(|s| s.take_for_activation(true))
        else {
            return;
        };
        let region = state.merge_id;
        // Round 11 wave 14 (lane osrmerge): an OSR-open header is entered
        // through the opaque merge in front of it, which becomes its ONE
        // forward predecessor. Everything below then builds the header's φs
        // over that merge's φs exactly as it would over a single entry edge.
        // The monitor snapshots stay the entries' (all empty, or no merge).
        if let Some((pre, pre_mem, pre_locals)) = self.build_opaque_osr_merge(target_pc, &state) {
            // al16: the arm brings the interpreter's value into every slot it
            // merges, which no flow fact describes (`exact_receiver_flow`).
            if let Some(entry) = state.local_snapshots.first() {
                self.exact_receiver_flow
                    .retain(|&k, _| entry.get(k).is_some() && entry.get(k) == pre_locals.get(k));
            }
            // OSR round (2026-10-06): nor does a single-store slot's fact
            // (`exact_receiver_single_store_slots`). Its one store wrote a `new`
            // on the paths this walk built; the interpreter frame the arm reads
            // ran the same store after its OWN call, which may have dispatched
            // anywhere. `R16ArraylistSplit.iterForEach`'s OSR body, entered at
            // the for-each header, spliced `ArrayList$Itr.hasNext`/`next` as
            // exact on the arm's iterator: a call over an `ArrayList` subclass
            // whose `iterator()` returns another class (`FerHdrBugA`) read that
            // object's fields as an `Itr`'s and threw
            // `ConcurrentModificationException`. The arm supplies an
            // `Op::OsrLocal` to exactly the slots it changes.
            // `osr_imported_exact` keeps a fact the arm re-establishes with a
            // class test of its own.
            if let Some(osr_locals) = self.osr_opaque_locals.get(&target_pc) {
                let imported: Vec<usize> = osr_locals.iter().map(|&(k, _)| k).collect();
                for k in imported {
                    if self.osr_imported_exact.contains_key(&k) {
                        continue;
                    }
                    self.exact_receiver_single_store_slots.remove(&k);
                    self.exact_receiver_locals.remove(&k);
                    self.exact_receiver_flow.remove(&k);
                }
            }
            state.ctrl_inputs = vec![pre];
            state.mem_inputs = vec![pre_mem];
            state.local_snapshots = vec![pre_locals];
            state.stack_snapshots = vec![Vec::new()];
        }
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
            //
            // ...and CLEAR them (round 11). The walk activates a header whose
            // fall-through is dead (the code before it ended in a `return` or
            // `goto`) without recording that fall-through as a predecessor, so
            // `self.locals` still holds the dead arm's values here. Skipping
            // left them in place, and every snapshot in the loop then named a
            // node that does not dominate it — a deopt read its never-written
            // home into an interpreter local (for a `Ref`, a word the GC then
            // scans). `activate_merge` has no
            // such hole because it overwrites every slot; with a live
            // fall-through this is a no-op, because that edge's snapshot is
            // one of the ones found empty here.
            if state
                .local_snapshots
                .iter()
                .all(|s| s.get(i).copied().and_then(node_id_opt).is_none())
            {
                if let Some(slot) = self.locals.get_mut(i) {
                    *slot = NO_NODE;
                }
                continue;
            }
            let mut inputs = vec![region];
            for snap in &state.local_snapshots {
                inputs.push(snap.get(i).copied().unwrap_or(NO_NODE));
            }
            let phi_ty = self.phi_data_type(&inputs);
            // Wave 107 (lane L2): a loop nested in the requested header's
            // loop: its entry values are that header's proven φ (the back
            // edges are checked by `patch_loop_backedge`).
            let profiled = self.osr_profiled_phi_class(&inputs);
            let phi = self.graph.add(Op::Phi, phi_ty, inputs, Some(target_pc));
            if let Some(class_id) = profiled {
                self.osr_profiled_exact_nodes.insert(phi, class_id);
            }
            local_phis[i] = phi;
            self.locals[i] = phi;
        }
        // JIT round 21 wave 2 (lane keepset): two FORWARD entries that split a
        // `long` / `double` pair leave its slot TOP at the header too. Only the
        // frame is cleared: the φ stays in `local_phis` for the back edge's
        // input and dies unread.
        self.kill_cat2_values_split_at_a_join();
        // Interpreter round i5 wave 107 (lane L2,
        // `CRATONVM_JIT_IR_OSR_PROFILED_SLOT_CHECK`): an admitted profiled
        // slot's class is a fact of THIS header's φ, which only code the
        // header dominates reads. Taken: one requested header per build.
        if self.osr_requested_header == Some(target_pc) && !self.osr_profiled_admitted.is_empty()
        {
            for (k, class_id) in std::mem::take(&mut self.osr_profiled_admitted) {
                if let Some(phi) = local_phis.get(k).copied().and_then(node_id_opt) {
                    self.osr_profiled_exact_nodes.insert(phi, class_id);
                }
            }
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
    /// disagree: a future edit to
    /// the header set, or (before its own fix) the pruned-branch path that
    /// jumped the walk backwards onto an activated merge. "I cannot record this
    /// edge" must never be spelled the same way as "nothing to do", so it
    /// latches [`Self::build_refused`] instead.
    pub(in crate::ir) fn patch_loop_backedge(&mut self, target_pc: usize) {
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
                // Wave 107 (lane L2): a profiled header φ's node fact holds
                // only while every back edge brings the φ back (its slot has
                // one store, before the header). Anything else: refuse.
                if let Some(&class_id) = self.osr_profiled_exact_nodes.get(&phi) {
                    if v != phi
                        && v != NO_NODE
                        && self.osr_profiled_exact_nodes.get(&v) != Some(&class_id)
                    {
                        self.refuse(line!(), target_pc);
                        return;
                    }
                }
                self.graph.push_input(phi, v);
                self.retype_phi(phi, target_pc);
            }
        }
        // Round 11 wave 14 (lane osrmerge): an OSR arm reads a local only when
        // it is LIVE at the header, where every incoming path agrees on its
        // type; the completed header φ must agree too. A disagreement means
        // the arm would read the interpreter's word as the wrong kind (a
        // `long` as a reference is a GC root), so the build is refused.
        if let Some(osr_locals) = self.osr_opaque_locals.get(&target_pc).cloned() {
            for (i, osr) in osr_locals {
                let header_ty = lp
                    .local_phis
                    .get(i)
                    .copied()
                    .and_then(node_id_opt)
                    .and_then(|phi| self.graph.node_opt(phi))
                    .map(|n| n.ty);
                let osr_ty = self.graph.node_opt(osr).map(|n| n.ty);
                if header_ty.is_none() || header_ty != osr_ty {
                    self.refuse(line!(), target_pc);
                    return;
                }
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
    pub(in crate::ir) fn predecessor_frames_agree(&self, state: &MergeState) -> bool {
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

    /// Interpreter round i5 wave 107 (lane L2): the class every data input of
    /// a φ under construction (`inputs[0]` is its merge) is proven by
    /// [`Self::osr_profiled_exact_nodes`], or `None`. Empty map: `None`, at
    /// no cost.
    pub(in crate::ir) fn osr_profiled_phi_class(&self, inputs: &[NodeId]) -> Option<u32> {
        if self.osr_profiled_exact_nodes.is_empty() {
            return None;
        }
        let mut class: Option<u32> = None;
        for v in inputs.iter().skip(1) {
            let c = *self.osr_profiled_exact_nodes.get(v)?;
            if class.is_some_and(|prev| prev != c) {
                return None;
            }
            class = Some(c);
        }
        class
    }

    /// Activate the merge at `target_pc`: set builder state from merged predecessors.
    pub(in crate::ir) fn activate_merge(&mut self, target_pc: usize) {
        let (empty, visited) = match self.merges.get(&target_pc) {
            Some(s) => (s.ctrl_inputs.is_empty(), s.visited),
            None => return,
        };

        if empty {
            return;
        }

        // A second activation rebuilds every φ at this join and resets the
        // merge's control inputs, orphaning the first generation — the same
        // corruption [`Self::activate_loop_header`] documents at length. Any
        // walk that reaches a merge pc twice (a malformed range list, a future
        // block-walk edit) is a bug in the walk, so refuse rather than
        // duplicate.
        if visited {
            self.refuse(line!(), target_pc);
            return;
        }
        let agree = self
            .merges
            .get(&target_pc)
            .is_some_and(|s| self.predecessor_frames_agree(s));
        if !agree {
            self.refuse(line!(), target_pc);
            return;
        }
        // Taken, not cloned: a forward join's predecessor frames are read
        // here and never again (`MergeState::take_for_activation`).
        let Some(state) = self
            .merges
            .get_mut(&target_pc)
            .map(|s| s.take_for_activation(false))
        else {
            return;
        };

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
                    // Wave 107 (lane L2): a join of proven profiled values.
                    let profiled = self.osr_profiled_phi_class(&phi_inputs);
                    let phi = self.graph.add(Op::Phi, phi_ty, phi_inputs, Some(target_pc));
                    if let Some(class_id) = profiled {
                        self.osr_profiled_exact_nodes.insert(phi, class_id);
                    }
                    self.locals[local_idx] = phi;
                }
            }
            // JIT round 21 wave 2 (lane keepset): a `long` / `double` whose high
            // half another predecessor wrote is TOP here.
            self.kill_cat2_values_split_at_a_join();
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
    pub(in crate::ir) fn audit_unactivated_merges(&mut self) -> Option<usize> {
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
    /// # A φ with an UNDEFINED input is retired too (round 11 wave 7)
    ///
    /// The same TOP argument covers a φ one of whose edges carries [`NO_NODE`]:
    /// the verifier's merge of a value with an unset slot is TOP as well.
    /// `activate_merge` builds such a φ — `HashMap.getNode`'s `k` is assigned
    /// on one arm of the first-node test only, so the join at bci 71 got
    /// `φ(undef, first.key, first.key)` — and `ir_optimize`'s trivial-φ rule,
    /// which skips placeholder inputs, then folds it to `first.key`, a load
    /// that does not dominate the join. The loop-header φ at bci 99 inherited
    /// that load on its entry edge, and the post-schedule check refused
    /// `getNode`/`putVal`/`removeNode` and `ConcurrentHashMap.addCount` in
    /// every program (`r11w6-orch-post-schedule-phi-check-refuses-hashmap`).
    /// The check was right: the φ's copy on that edge reads the load's home on
    /// a path that never wrote it, and the φ is a `Ref` that a frame state
    /// names. Retiring the φ makes those frame slots say "undefined", which is
    /// what the interpreter's own frame holds there.
    ///
    /// Retiring a φ turns its uses in other φs into [`NO_NODE`], so a φ that
    /// merges a retired one is TOP as well and is retired in turn (a worklist
    /// over φ→φ edges, so one pass however long the chain). A φ that any
    /// other node reads, or that a snapshot names as a held monitor, is never
    /// retired, by the rule above: it keeps its inputs as they are.
    pub(in crate::ir) fn retire_untypeable_phis(&mut self) -> usize {
        fn is_data_phi(n: &Node) -> bool {
            n.op == Op::Phi && !matches!(n.ty, IrType::Memory | IrType::Control)
        }
        let seeds: Vec<usize> = self
            .graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| {
                is_data_phi(n)
                    && (n.phi_value_inputs().any(|v| v.is_none())
                        || phi_inputs_conflict(&self.graph, &n.inputs))
            })
            .map(|(id, _)| id)
            .collect();
        if seeds.is_empty() {
            return 0;
        }
        let n = self.graph.nodes.len();
        // A φ that anything but another φ reads is not a dead slot.
        let mut read = vec![false; n];
        // Data φ → the data φs that merge it (self-edges left out).
        let mut phi_users: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for (idx, node) in self.graph.nodes.iter().enumerate() {
            if node.op == Op::Dead {
                continue;
            }
            if node.op == Op::Phi {
                if is_data_phi(node) {
                    for &inp in node.inputs.iter().skip(1) {
                        if inp != idx as NodeId && self.graph.node_opt(inp).is_some_and(is_data_phi)
                        {
                            phi_users.entry(inp).or_default().push(idx as NodeId);
                        }
                    }
                }
                continue;
            }
            for &inp in node.inputs.iter() {
                if let Some(r) = read.get_mut(inp as usize) {
                    *r = true;
                }
            }
        }
        for sp in &self.graph.safepoints {
            for &m in &sp.monitors {
                if let Some(r) = read.get_mut(m as usize) {
                    *r = true;
                }
            }
        }
        let mut doomed = vec![false; n];
        let mut work: Vec<NodeId> = Vec::new();
        for id in seeds {
            if !read.get(id).copied().unwrap_or(true) {
                if let Some(d) = doomed.get_mut(id) {
                    *d = true;
                    work.push(id as NodeId);
                }
            }
        }
        while let Some(d) = work.pop() {
            let Some(users) = phi_users.get(&d) else {
                continue;
            };
            for &u in users {
                let ui = u as usize;
                if read.get(ui).copied().unwrap_or(true) {
                    continue;
                }
                if let Some(slot) = doomed.get_mut(ui) {
                    if !*slot {
                        *slot = true;
                        work.push(u);
                    }
                }
            }
        }
        let mut retired = 0usize;
        for (idx, &gone) in doomed.iter().enumerate() {
            if !gone {
                continue;
            }
            let id = idx as NodeId;
            self.graph.replace_all_uses(id, NO_NODE);
            self.graph.kill(id);
            retired += 1;
        }
        retired
    }

    /// Hoist local-throw JVMTI event guards to method entry (and the OSR arm,
    /// for OSR compiles) when the graph contains no calls, allocations, stores,
    /// monitors, or throws that could observe or toggle listeners mid-body.
    pub(in crate::ir) fn finish_local_throw_event_guards(&mut self) {
        let Some(flags) = self.exception_event_guard else {
            return;
        };
        if self.local_throw_edges == 0
            || self.spliced_throw_edges > 0
            || !self.has_frame_state_at(0)
        {
            return;
        }
        let entry_ctrl_ok = self.graph.node_opt(0).is_some_and(|n| n.op == Op::Start)
            && self.graph.node_opt(1).is_some_and(|n| {
                n.op == Op::Proj(0) && n.ty == IrType::Control && n.input_opt(0) == Some(0)
            });
        if !entry_ctrl_ok {
            return;
        }
        let has_effects = self.graph.nodes.iter().any(|n| {
            matches!(
                n.op,
                Op::Call { .. }
                    | Op::IndyBridge { .. }
                    | Op::New { .. }
                    | Op::NewArray { .. }
                    | Op::MultiNewArray { .. }
                    | Op::NewCp { .. }
                    | Op::MonitorEnter
                    | Op::MonitorExit
                    | Op::Store(_)
                    | Op::ArrayStore(_)
                    | Op::StoreStatic { .. }
                    | Op::Throw
            )
        });
        if has_effects {
            return;
        }

        let osr_arm = if self.graph.osr_compile {
            let Some(target_pc) = self.osr_requested_header else {
                return;
            };
            let arm_ctrl = self.graph.nodes.iter().enumerate().find_map(|(id, n)| {
                let Op::Proj(0) = n.op else {
                    return None;
                };
                if n.ty != IrType::Control {
                    return None;
                }
                let if_id = n.input_opt(0)?;
                let if_node = self.graph.node_opt(if_id)?;
                if if_node.op != Op::If {
                    return None;
                }
                let flag_id = if_node.input_opt(1)?;
                let flag_node = self.graph.node_opt(flag_id)?;
                match flag_node.op {
                    Op::OsrEntryFlag { header_bci } if header_bci == target_pc => {
                        Some(id as NodeId)
                    }
                    _ => None,
                }
            });
            let Some(arm) = arm_ctrl else {
                return;
            };
            let Some(osr_locals) = self.osr_opaque_locals.get(&target_pc) else {
                return;
            };
            let mut locals = vec![NO_NODE; self.locals.len()];
            for &(slot, osr) in osr_locals {
                if let Some(entry) = locals.get_mut(slot) {
                    *entry = osr;
                }
            }
            Some((target_pc, arm, locals))
        } else {
            None
        };

        let mut victims: Vec<(NodeId, NodeId)> = Vec::new();
        for (gid, g) in self.graph.nodes.iter().enumerate() {
            let Op::Guard { .. } = g.op else {
                continue;
            };
            let Some(tid) = g.input_opt(1) else {
                continue;
            };
            let Some(tn) = self.graph.node_opt(tid) else {
                continue;
            };
            if let Op::ClassInitTest {
                state_addr,
                initialized: 0,
            } = tn.op
            {
                if state_addr == flags[0] || state_addr == flags[1] {
                    victims.push((gid as NodeId, tid));
                }
            }
        }
        if victims.is_empty() {
            return;
        }
        for &(gid, _) in &victims {
            self.graph.kill(gid);
        }
        for &(_, tid) in &victims {
            if self.graph.users_of(tid).is_empty() {
                self.graph.kill(tid);
            }
        }

        for state_addr in flags {
            let flag_clear = self.graph.add(
                Op::ClassInitTest {
                    state_addr,
                    initialized: 0,
                },
                IrType::Int,
                vec![1],
                Some(0),
            );
            self.graph.add(
                Op::Guard { bci: 0 },
                IrType::Void,
                vec![1, flag_clear],
                Some(0),
            );
        }

        if let Some((target_pc, arm, arm_locals)) = osr_arm {
            let snapshot = self.graph.safepoints.len() as u32;
            self.graph.push_safepoint(SafepointSnapshot::new(
                target_pc,
                arm_locals,
                Vec::new(),
                Vec::new(),
            ));
            for state_addr in flags {
                let flag_clear = self.graph.add(
                    Op::ClassInitTest {
                        state_addr,
                        initialized: 0,
                    },
                    IrType::Int,
                    vec![arm],
                    None,
                );
                let guard = self.graph.add(
                    Op::Guard { bci: target_pc },
                    IrType::Void,
                    vec![arm, flag_clear],
                    Some(target_pc),
                );
                let _ = self.graph.set_node_frame_snapshot(guard, snapshot);
            }
        }
    }
}

/// Should `(cn, mn, desc)` bypass `ir_direct_calls` / splicing so `jit_invoke_dispatch`
/// can route it through `try_throwable_init_direct_at_dispatch` (`jit_new_throwable_direct`)?
pub(crate) fn is_jdk_throwable_init_candidate(cn: &str, mn: &str, desc: &str) -> bool {
    mn == "<init>"
        && matches!(desc, "()V" | "(Ljava/lang/String;)V")
        && (cn == "java/lang/Throwable"
            || ((cn.starts_with("java/")
                || cn.starts_with("javax/")
                || cn.starts_with("jdk/")
                || cn.starts_with("sun/"))
                && (cn.ends_with("Exception") || cn.ends_with("Error"))))
}
