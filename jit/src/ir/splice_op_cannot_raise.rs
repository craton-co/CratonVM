#[allow(unused_imports)]
use super::*;

/// An instruction a spliced body may hold and still raise nothing: locals,
/// constants, stack ops, non-trapping arithmetic, compares, a forward branch
/// that stays before `end`, a return. A `getstatic` or a call is the
/// caller's to judge.
pub(crate) fn splice_op_cannot_raise(code: &[u8], at: usize, end: usize) -> bool {
    match code[at] {
        0x00..=0x11 | 0x15..=0x2d | 0x36..=0x4e | 0x57..=0x5f | 0x60..=0x6b | 0x74..=0x98 | 0xac..=0xb1 => true,
        0x99..=0xa7 | 0xc6 | 0xc7 => {
            crate::bytecode_analysis::offset_branch_target(code, at).is_some_and(|t| t > at && t < end)
        }
        _ => false,
    }
}

impl IrBuilder {
    /// May the invoke arm splice the body planned at `pc`? `true` for every
    /// body that is not `synchronized`; for one that is, only when
    /// [`Self::sync_splice_plan`] can build its monitor pair. A `false` sends
    /// the arm on to its ordinary call lowering.
    /// JIT r24 (CompletableFuture page; `lib.rs` `nested_body_cannot_raise`,
    /// `CRATONVM_JIT_IR_SPLICE_COVERED_NESTED_PURE`): may the body planned at
    /// combined pc `pc`, inside a covered `try` range, be spliced anyway? Only
    /// when it can raise nothing, so it leaves only through its return and
    /// cannot skip the enclosing `catch`: the same opcode rule as the
    /// planner's -- locals, constants, stack ops, non-trapping arithmetic,
    /// compares, forward branches, returns, and a `getstatic` of the callee's
    /// OWN class from an instance callee -- over its combined-buffer copy, not
    /// synchronized, with no splice planned inside it.
    pub(in crate::ir) fn covered_splice_cannot_raise(&self, pc: usize) -> bool {
        self.pure_splice_sites.contains(&pc)
    }

    /// Fill [`Self::pure_splice_sites`] from the combined buffer (see
    /// [`Self::covered_splice_cannot_raise`]), once per build, before the walk.
    pub(in crate::ir) fn plan_pure_splice_sites(&mut self, code: &[u8]) {
        self.pure_splice_sites.clear();
        if !ir_splice_covered_nested_pure_enabled() || self.splice_handler_rows.is_empty() {
            return;
        }
        let pure = self
            .inline_sites
            .keys()
            .copied()
            .filter(|&pc| self.splice_body_cannot_raise(code, pc, 0))
            .collect();
        self.pure_splice_sites = pure;
    }

    /// Can the body planned at combined pc `pc` raise nothing and leave only
    /// through its return? Every instruction is one of
    /// [`Self::covered_splice_cannot_raise`]'s, or, while `nest` is above
    /// zero, an `invokestatic` whose own planned body passes with one less.
    pub(in crate::ir) fn splice_body_cannot_raise(&self, code: &[u8], pc: usize, nest: u8) -> bool {
        let Some(site) = self.inline_sites.get(&pc) else {
            return false;
        };
        if site.class_id == 0 || self.sync_splice_sites.contains_key(&pc) {
            return false;
        }
        let (start, end) = (site.base, (site.base + site.code_len).min(code.len()));
        let mut at = start;
        let mut ok = start < end;
        while ok && at < end {
            ok = if self.inline_sites.contains_key(&at) {
                code[at] == 0xb8 && nest > 0 && self.splice_body_cannot_raise(code, at, nest - 1)
            } else if code[at] == 0xb2 {
                site.receiver_is_arg0
                    && self
                        .static_field_info
                        .get(&at)
                        .is_some_and(|&(class_id, _, _, _)| class_id == site.class_id)
            } else {
                splice_op_cannot_raise(code, at, end)
            };
            at += crate::bytecode_analysis::step(code, at).max(1);
        }
        ok
    }

    /// Fill [`Self::convert_member_pure`]: the box-returning converting
    /// handle sites whose member body, planned at the `getstatic` pc, cannot
    /// raise (a forwarder and the one static body it calls included). A trap
    /// inside that body would resume at the `getstatic` with the handle's
    /// arguments already converted, so only such a body is spliced there.
    pub(in crate::ir) fn plan_convert_member_splices(&mut self, code: &[u8]) {
        let pure = self
            .mh_constant_convert_sites
            .values()
            .filter(|site| site.box_return != 0)
            .map(|site| site.pre_pc)
            .filter(|&pre| self.splice_body_cannot_raise(code, pre, 1))
            .collect();
        self.convert_member_pure = pure;
    }

    /// Is the call at `pc` a signature-polymorphic `MethodHandle.invokeExact`
    /// / `invoke`? A body planned there is its constant handle's MEMBER
    /// (`folded_handle_member_row` in the VM resolver), whose arguments are
    /// not the call's: only the fold splices it
    /// ([`Self::folded_member_splice_admitted`]), never the invoke arms.
    pub(in crate::ir) fn signature_polymorphic_site(&self, pc: usize) -> bool {
        self.invoke_info.get(&pc).is_some_and(|&(info_ptr, _, _)| {
            info_ptr != 0 && {
                // SAFETY: a non-zero row is an interned `JitInvokeInfo` the artifact keeps alive.
                let info = unsafe { &*(info_ptr as *const JitInvokeInfo) };
                info.class_name == "java/lang/invoke/MethodHandle"
                    && matches!(info.method_name, "invokeExact" | "invoke")
            }
        })
    }

    /// The receiver class under which the member body planned at the folded
    /// handle call `pc` may be spliced: the body is that member's
    /// (`member_info`'s name and descriptor), it cannot raise
    /// ([`Self::splice_body_cannot_raise`]: a trap would re-execute the
    /// `invokeExact` with its arguments already converted), and the receiver
    /// on the stack is proven exactly the class it was resolved for.
    /// `None` keeps the member call.
    pub(in crate::ir) fn folded_member_splice_admitted(&self, code: &[u8], pc: usize, member_info: usize) -> Option<u32> {
        let site = self.inline_sites.get(&pc)?;
        if member_info == 0 || !site.receiver_is_arg0 || !self.signature_polymorphic_site(pc) {
            return None;
        }
        // SAFETY: the fold row's `info_ptr`, an interned `JitInvokeInfo`.
        let info = unsafe { &*(member_info as *const JitInvokeInfo) };
        if !site.method_key.ends_with(&format!(".{}:{}", info.method_name, info.descriptor)) {
            return None;
        }
        let &class_id = self.exact_receiver_sites.get(&pc)?;
        if !self.splice_body_cannot_raise(code, pc, 1) {
            return None;
        }
        self.exact_receiver_proof(pc, class_id)?;
        Some(class_id)
    }

    /// The box of a spliced convert member's result
    /// ([`Self::convert_box_after`]), when the splice opened at `invoke_pc`
    /// has just returned it onto the stack.
    pub(in crate::ir) fn box_convert_member_result(&mut self, invoke_pc: usize) -> Option<()> {
        let Some((box_info, pc)) = self.convert_box_after.remove(&invoke_pc) else {
            return Some(());
        };
        let value = node_id_opt(*self.stack.last()?)?;
        self.pop();
        let boxed = self.mh_constant_filter_pre_call(pc, box_info, b'L', &[value]);
        self.plant_call_throw_edges(pc, boxed, self.root_code_len);
        self.push(boxed);
        Some(())
    }

    pub(in crate::ir) fn sync_splice_site_admits(&self, pc: usize) -> bool {
        !self.sync_splice_sites.contains_key(&pc) || self.sync_splice_plan(pc).is_some()
    }

    /// Is the walk inside a synchronized splice's window? Its body is checked
    /// whole for traps ([`sync_splice_window_node_admitted`], at its exit and
    /// again by `ir_lower`), so the replay fences, whose only job is to keep a
    /// trap from re-running committed work, have nothing to guard there.
    ///
    /// Any open frame, not only the innermost (round 14 wave 6, lane sync6,
    /// SS8-1 a): a same-receiver synchronized body nested in the window has no
    /// window of its own, and every node it builds is judged by the enclosing
    /// window's exit check. Before that nesting existed a synchronized splice
    /// held no nested splice, so the two readings agree on every older graph.
    pub(in crate::ir) fn sync_splice_open(&self) -> bool {
        self.splice.iter().any(|f| f.sync_monitor.is_some())
    }

    /// Round 14 wave 6 (lane sync6, SS8-1 a): may the same-receiver
    /// synchronized body planned at combined pc `pc` be spliced with no pair,
    /// its receiver being `receiver`? Only when the INNERMOST open splice is an
    /// instance synchronized splice whose window monitor is provably that same
    /// object ([`Self::holds_lock_same_object`], the fold's stricter identity):
    /// the window's enter ran before this body's first instruction and its
    /// exit runs after the enclosing body's last, so the body runs with the
    /// monitor held, and its own acquire/release would be recursive.
    pub(in crate::ir) fn sync_splice_held_site_proven(&self, pc: usize, receiver: NodeId) -> bool {
        self.sync_splice_held_sites.contains(&pc)
            && receiver != NO_NODE
            && ir_sync_splice_nested_same_receiver_enabled()
            && self.splice.last().is_some_and(|f| {
                f.sync_monitor.is_some_and(|w| {
                    w.static_class_id == 0 && self.holds_lock_same_object(w.monitor, receiver)
                })
            })
    }

    /// May the open splice write the static field of `class_id`? Only the
    /// OWN class of an outermost `static synchronized` splice, non-volatile
    /// (the resolver admits no other `putstatic` into an optimizing splice).
    pub(in crate::ir) fn sync_splice_owns_static(&self, class_id: u32, is_volatile: bool) -> bool {
        self.splice.len() == 1
            && !is_volatile
            && class_id != 0
            && self
                .splice
                .last()
                .and_then(|f| f.sync_monitor)
                .is_some_and(|w| w.static_class_id == class_id)
    }

    /// Round 14 wave 4 (lane sync4, proposal SS-7): is the `invokestatic`
    /// whose row is `info_ptr` a `Thread.holdsLock(o)` this thread provably
    /// holds `o` for? Then its answer is `true` and no call is built: the
    /// argument is popped and the constant `1` pushed. Proven when `o` (the
    /// operand-stack top, compared through trivial φs) is
    ///
    /// * the receiver an open INSTANCE synchronized splice locked -- the
    ///   window's enter ran before the body's first instruction and its exit
    ///   runs after the last, and the body can neither call (`wait` gives a
    ///   monitor back only from inside a call) nor leave early; or
    /// * an entry of this frame's monitor stack: a `monitorenter` of that
    ///   node whose `monitorexit` has not been reached on this path
    ///   (structured locking; a nested-lock elision keeps the outermost hold
    ///   and EA elides only a lock no other thread can see, and never one
    ///   whose object reaches a call).
    ///
    /// `Thread.holdsLock` is not a synchronization action, so no ordering is
    /// lost. A static window is not matched (`holdsLock(C.class)` pushes a
    /// different `ConstClass` node). `false` -- the call is built as before --
    /// for any other site, a zero or foreign row, or under
    /// [`ir_holds_lock_fold_enabled`] off. Inside a synchronized splice that
    /// call is a node the window refuses, so a body the planner admitted for
    /// this fold and the builder did not fold is refused as a site, never
    /// built with a call under its monitor.
    pub(in crate::ir) fn try_fold_holds_lock(&mut self, info_ptr: usize, num_args: usize) -> bool {
        if num_args != 1 || !is_holds_lock_info_row(info_ptr) {
            return false;
        }
        let Some(arg) = self.peek_opt() else {
            return false;
        };
        let in_splice = self.splice.iter().any(|f| {
            f.sync_monitor.is_some_and(|w| {
                w.static_class_id == 0 && self.holds_lock_same_object(w.monitor, arg)
            })
        });
        let in_region = !in_splice
            && self
                .monitors
                .iter()
                .any(|&m| self.holds_lock_same_object(m, arg));
        // Round 14 wave 5 (lane sync5, SS8-3): the receiver of the
        // synchronized instance method being compiled, held for the whole body
        // (`Graph::method_monitor_param`).
        let in_method = !(in_splice || in_region)
            && self
                .graph
                .method_monitor_node()
                .is_some_and(|m| self.holds_lock_same_object(m, arg));
        // Round 14 wave 7 (lane sync7, RV6-1): `holdsLock(C.class)` in a
        // `static synchronized` method of `C`, the `ldc C` being one of the
        // method's own entries the VM resolved to `C`
        // (`set_method_monitor_class_cps`).
        let in_static_method = !(in_splice || in_region || in_method)
            && self.graph.method_monitor_class.is_some()
            && self.graph.node_opt(arg).is_some_and(|n| match n.op {
                Op::ConstClass {
                    holder_class_id,
                    cp_idx,
                    ..
                } => self
                    .method_monitor_class_cps
                    .contains(&(holder_class_id, cp_idx)),
                _ => false,
            });
        if !(in_splice || in_region || in_method || in_static_method)
            || !ir_holds_lock_fold_enabled()
        {
            return false;
        }
        self.pop();
        let one = self.iconst(1);
        self.push(one);
        note_sync_splice_census(if in_splice {
            SYNC_CENSUS_HOLDSLOCK_IN_SPLICE
        } else if in_region {
            SYNC_CENSUS_HOLDSLOCK_IN_REGION
        } else if in_method {
            SYNC_CENSUS_HOLDSLOCK_IN_METHOD
        } else {
            SYNC_CENSUS_HOLDSLOCK_IN_STATIC_METHOD
        });
        true
    }

    /// JIT round 20 wave 12 (lane qMisc, NIO20-L3-1 of
    /// `docs/feature-designs/jit-ir-optimizations.md`, the sound
    /// route; [`crate::ir_getclass_exact_fold_enabled`]): at the
    /// `invokevirtual Object.getClass()` at `pc`, the three instructions
    ///
    /// ```text
    /// invokevirtual getClass ; ldc C ; if_acmpeq/if_acmpne T
    /// ```
    ///
    /// built as
    ///
    /// ```text
    /// Guard(x != null)                  // deopt at the invoke, REEXECUTE
    /// If(Cmp(ExactClassIs(x, C), 0))    // at the if_acmp's pc
    /// ```
    ///
    /// returning the pc after the `if_acmp` (`None` builds nothing and the
    /// call is built as before).
    ///
    /// # Why it is sound
    ///
    /// * The VM stated (`lib.rs` `ir_getclass_exact_compares`, from
    ///   native-builtins' `getclass_reports_only_itself`) that no remap of
    ///   `getClass()` reports `C` for an object whose header names another
    ///   class and that `C`'s own instances report `C`; an array's header
    ///   names its component and `ExactClassIs` answers `0` for an array, as
    ///   `getClass()` of an array is never a non-array `C`. So for a non-null
    ///   `x`, `x.getClass() == C.class` is `ExactClassIs(x, C)`, both mirrors
    ///   being the one mirror of class `C` (the `ldc` resolves the method's own
    ///   entry to the class the VM resolved, which is loaded, so it runs no
    ///   loading, no initialization and cannot throw).
    /// * A null `x` deopts at the invoke, from the snapshot the walk took at
    ///   the top of this bci with `x` on the stack, and the interpreter raises
    ///   the NPE (message and handler included), as the begin-splice null
    ///   guard does. A `definitely_non_null` `x` needs none.
    /// * Neither skipped instruction is a join or a loop header, so no edge
    ///   enters between them, and nothing between them can deopt (the
    ///   compare is built here, with no prune): no frame state of those two
    ///   pcs is ever read. The method's own code only (no open splice).
    pub(in crate::ir) fn try_fold_getclass_exact_compare(
        &mut self,
        code: &[u8],
        pc: usize,
    ) -> Option<usize> {
        let &class_id = self.getclass_exact_compares.get(&pc)?;
        if class_id == 0 || !self.splice.is_empty() || !crate::ir_getclass_exact_fold_enabled() {
            return None;
        }
        let ldc_pc = pc + 3;
        let width = match code.get(ldc_pc).copied()? {
            0x12 => 2,
            0x13 => 3,
            _ => return None,
        };
        let acmp_pc = ldc_pc + width;
        let acmp = code.get(acmp_pc).copied()?;
        if !matches!(acmp, 0xa5 | 0xa6) || acmp_pc + 2 >= code.len() {
            return None;
        }
        if self.merges.contains_key(&ldc_pc)
            || self.merges.contains_key(&acmp_pc)
            || self.loop_headers.contains(&ldc_pc)
            || self.loop_headers.contains(&acmp_pc)
        {
            return None;
        }
        let ctrl = self.ctrl_opt()?;
        let receiver = self.peek_opt()?;
        let needs_null_guard = self
            .graph
            .nodes
            .get(receiver as usize)
            .is_none_or(|n| !crate::ir_check_elim::definitely_non_null(&n.op));
        if needs_null_guard && !self.has_frame_state_at(pc) {
            return None;
        }
        let offset = i16::from_be_bytes([code[acmp_pc + 1], code[acmp_pc + 2]]) as i64;
        let target_pc = usize::try_from(acmp_pc as i64 + offset).ok()?;
        self.pop();
        if needs_null_guard {
            // `aconst_null`, not `iconst(0)`: a 64-bit compare (see the
            // begin-splice null guard).
            let null = self.aconst_null();
            let cond = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![receiver, null], pc);
            self.graph.add(
                Op::Guard { bci: pc },
                IrType::Void,
                vec![ctrl, cond],
                Some(pc),
            );
        }
        let exact = self.add_data(
            Op::ExactClassIs { class_id },
            IrType::Int,
            vec![receiver],
            pc,
        );
        let zero = self.iconst(0);
        // `if_acmpeq` jumps when the classes are equal: the test is non-zero.
        let cc = if acmp == 0xa5 { CmpOp::Ne } else { CmpOp::Eq };
        let cmp = self.add_data(Op::Cmp(cc), IrType::Int, vec![exact, zero], acmp_pc);
        let if_node = self
            .graph
            .add(Op::If, IrType::Control, vec![ctrl, cmp], Some(acmp_pc));
        let true_ctrl = self
            .graph
            .add(Op::Proj(0), IrType::Control, vec![if_node], Some(acmp_pc));
        let false_ctrl = self
            .graph
            .add(Op::Proj(1), IrType::Control, vec![if_node], Some(acmp_pc));
        self.ctrl = true_ctrl;
        self.add_merge_predecessor(target_pc);
        self.ctrl = false_ctrl;
        // A native call (and its `ldc`) the single-pass body keeps, replaced
        // by straight-line code: acceptance evidence, as a scalar intrinsic
        // and the String inflate prefix note it. Without it a kernel whose
        // only gain is this fold reads as "nothing the baseline lacks", and
        // the C1->C2 gate (`ir_evidence::is_worth_publishing`) refuses and
        // memoises the body: the single-pass body, with its full
        // `Object.getClass` native call per element, then runs -- the
        // suspected cause of the w12a A/B's ~4x loss (the `=0` arm's body kept
        // the call with its prefix, which the gate judged on other evidence).
        crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] getclass-exact-fold (ir) pc={pc} class_id={class_id} op={}",
                if acmp == 0xa5 { "eq" } else { "ne" }
            );
        }
        Some(acmp_pc + 3)
    }

    /// JIT r20 wave 19 (lane zLdc, THROW20-4 `date`, [`ir_splice_ref_ldc_enabled`]):
    /// inside an open splice, at the `invokevirtual Object.getClass()` at
    /// combined pc `pc`, the three instructions
    ///
    /// ```text
    /// invokevirtual getClass ; ldc C ; if_acmpeq/if_acmpne T
    /// ```
    ///
    /// on a receiver EVERY definition of which is an `Op::New` of one class
    /// `X` ([`Self::fresh_new_class_of`]) are built as `If(Cmp(Const(X == C),
    /// 0))` -- no call, no `ldc` node -- which the constant-`If` prune then
    /// folds to the one live arm. `java.util.Date.getMillisOf(Date)` opens with
    /// exactly this test, and the call was what made both `Date`s of a spliced
    /// `compareTo` escape. Returns the pc after the `if_acmp`; `None` builds
    /// nothing (the call is built as before).
    ///
    /// # Why it is sound
    ///
    /// * The row exists only for a `C` the VM states `getClass()` reports
    ///   exactly for an object whose header names `C`, and for no object whose
    ///   header names another class (`getclass_reports_only_itself`, the
    ///   predicate [`Self::try_fold_getclass_exact_compare`] rests on), and
    ///   only where the invoke resolves to the method `java/lang/Object`
    ///   declares. A `new X` is a non-null plain object whose header names
    ///   exactly `X` for its whole life, so `x.getClass() == C.class` is
    ///   `X == C` -- both mirrors being the one mirror of the class.
    /// * The `ldc C` is a resolved constant of the callee's pool (its row came
    ///   with a non-zero slot), so not building it moves no resolution side
    ///   effect; `getClass()` has none, and the receiver cannot be null (no
    ///   NPE to raise).
    /// * Neither skipped instruction is a join or a loop header, so no edge
    ///   enters between them, and nothing between them can deopt: no frame
    ///   state of those two pcs is ever read.
    /// JIT r20 wave 19 (lane zLdc): is the `ldc` at `pc` a String / Class
    /// site with a non-zero GC-maintained slot (`ldc_slot_info`)?
    pub(in crate::ir) fn ldc_ref_has_slot(&self, pc: usize) -> bool {
        self.ldc_string_info
            .get(&pc)
            .or_else(|| self.ldc_class_info.get(&pc))
            .and_then(|key| self.ldc_slot_info.get(key))
            .is_some_and(|&slot| slot != 0)
    }

    pub(in crate::ir) fn try_fold_spliced_getclass_of_fresh(
        &mut self,
        code: &[u8],
        pc: usize,
    ) -> Option<usize> {
        let &class_id = self.splice_getclass_compares.get(&pc)?;
        if class_id == 0 || self.splice.is_empty() || !ir_splice_ref_ldc_enabled() {
            return None;
        }
        let ldc_pc = pc + 3;
        let width = match code.get(ldc_pc).copied()? {
            0x12 => 2,
            0x13 => 3,
            _ => return None,
        };
        let acmp_pc = ldc_pc + width;
        let acmp = code.get(acmp_pc).copied()?;
        if !matches!(acmp, 0xa5 | 0xa6) || acmp_pc + 2 >= code.len() {
            return None;
        }
        if self.merges.contains_key(&ldc_pc)
            || self.merges.contains_key(&acmp_pc)
            || self.loop_headers.contains(&ldc_pc)
            || self.loop_headers.contains(&acmp_pc)
        {
            return None;
        }
        let ctrl = self.ctrl_opt()?;
        let receiver = self.peek_opt()?;
        let allocated = self.fresh_new_class_of(receiver)?;
        let offset = i16::from_be_bytes([code[acmp_pc + 1], code[acmp_pc + 2]]) as i64;
        let target_pc = usize::try_from(acmp_pc as i64 + offset).ok()?;
        self.pop();
        let same = self.iconst(i64::from(allocated == class_id));
        let zero = self.iconst(0);
        // `if_acmpeq` jumps when the classes are equal: the test is non-zero.
        let cc = if acmp == 0xa5 { CmpOp::Ne } else { CmpOp::Eq };
        let cmp = self.add_data(Op::Cmp(cc), IrType::Int, vec![same, zero], acmp_pc);
        let if_node = self
            .graph
            .add(Op::If, IrType::Control, vec![ctrl, cmp], Some(acmp_pc));
        let true_ctrl = self
            .graph
            .add(Op::Proj(0), IrType::Control, vec![if_node], Some(acmp_pc));
        let false_ctrl = self
            .graph
            .add(Op::Proj(1), IrType::Control, vec![if_node], Some(acmp_pc));
        self.ctrl = true_ctrl;
        self.add_merge_predecessor(target_pc);
        self.ctrl = false_ctrl;
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] splice-getclass-fresh-fold (ir) pc={pc} new_class={allocated} class_id={class_id} op={}",
                if acmp == 0xa5 { "eq" } else { "ne" }
            );
        }
        Some(acmp_pc + 3)
    }

    /// Round 14 wave 7 (lane sync7, SS8-5 through proposal S6-1's rows): is the
    /// `ldc`/`ldc_w` at combined pc `pc` (`width` bytes) the first half of
    /// `holdsLock(C.class)` inside an open `static synchronized` splice of `C`?
    /// Then the pair is built as the constant `1` -- no `ConstClass` node (the
    /// window admits none) and no call -- and `true` is answered; the caller
    /// resumes after the `invokestatic` (`width + 3` bytes on).
    ///
    /// Sound because the window's enter ran before the body's first
    /// instruction and its exit runs after its last, on `C`'s mirror (the
    /// window's `static_class_id` is the class declaring the spliced
    /// `static synchronized` method, JVMS 2.11.10), and the `ldc` names `C`
    /// (the VM resolved the body's own constant to that id, which is what the
    /// row carries). The `ldc` cannot throw: `C` is loaded and initialised
    /// (the resolver splices no static callee of an uninitialised class). The
    /// planner's scan guarantees the `invokestatic` is the next instruction,
    /// is the `holdsLock` row, and is no branch target.
    ///
    /// `false` (the arm continues with its ordinary tables, which carry no row
    /// for this pc, so the build is refused and the W5-2 rebuild drops the
    /// site) for any other shape: no row, no open static window of that class
    /// innermost, a next instruction that is not the `holdsLock` row, or
    /// [`ir_sync_splice_self_class_holds_lock_enabled`] off.
    pub(in crate::ir) fn try_fold_self_class_holds_lock(
        &mut self,
        code: &[u8],
        pc: usize,
        width: usize,
    ) -> bool {
        let Some(&class_id) = self.ldc_self_class_holds_lock.get(&pc) else {
            return false;
        };
        let next = pc + width;
        let window_class = self
            .splice
            .last()
            .and_then(|f| f.sync_monitor)
            .map(|w| w.static_class_id);
        if class_id == 0
            || window_class != Some(class_id)
            || code.get(next).copied() != Some(0xb8)
            || !ir_sync_splice_self_class_holds_lock_enabled()
        {
            return false;
        }
        let holds_lock_row = self
            .invoke_info
            .get(&next)
            .is_some_and(|&(info_ptr, num_args, _)| {
                num_args == 1 && is_holds_lock_info_row(info_ptr)
            });
        if !holds_lock_row {
            return false;
        }
        let one = self.iconst(1);
        self.push(one);
        note_sync_splice_census(SYNC_CENSUS_HOLDSLOCK_SELF_CLASS_IN_SPLICE);
        true
    }

    /// Count a synchronized splice the builder just closed (SY3-5), and name
    /// it under `CRATONVM_DBG_JITC`.
    pub(in crate::ir) fn note_sync_splice_built(
        &self,
        window: &SyncSpliceWindow,
        invoke_pc: usize,
    ) {
        let kind = if window.static_class_id == 0 {
            SYNC_CENSUS_INSTANCE
        } else if self.sync_splice_row_admits(invoke_pc, true) {
            SYNC_CENSUS_STATIC_ROW
        } else {
            SYNC_CENSUS_STATIC_MIRROR
        };
        note_sync_splice_census(kind);
        let in_region = !self.monitors.is_empty();
        if in_region {
            note_sync_splice_census(SYNC_CENSUS_IN_REGION);
        }
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-sync-splice built {} at pc={invoke_pc}{} (census: {})",
                SYNC_SPLICE_CENSUS_ROWS.get(kind).copied().unwrap_or("?"),
                if in_region { " in-region" } else { "" },
                ir_sync_splice_census_line(),
            );
        }
    }

    /// Round 11 wave 11 (lane `calls`, page
    /// `r11w4-sync-compiled-caller-cannot-call-a-synchronized-callee`): the
    /// enter half of a caller-held INSTANCE synchronized direct CALL at invoke
    /// `pc` -- the receiver-monitor twin of the `static` shape in the
    /// `0xb6 | 0xb8` arm. A row in `sync_direct_calls` at an `invokevirtual`
    /// or `invokespecial` pc means `lib.rs` bound a statically selected
    /// `synchronized` instance method, whose monitor is its receiver (JVMS
    /// 2.11.10). Emits
    ///
    /// ```text
    /// Guard(receiver != null)     // at `pc`, before any monitor action
    /// MonitorEnter(receiver)      // NOT pushed on `self.monitors`
    /// ```
    ///
    /// and answers the monitor (the receiver itself) for the caller's
    /// `MonitorExit` after the `Op::Call`, or `None` to refuse the build.
    ///
    /// ORDER. JVMS 6.5 raises the NPE of a null receiver at the invoke, before
    /// the callee -- and so before its method monitor -- exists. The guard is
    /// the same one a spliced `invokespecial` gets (`begin_splice`): it deopts
    /// with the snapshot taken at the top of this bci, the interpreter
    /// re-executes the invoke and raises the canonical NPE (message and JEP 358
    /// action included), and no monitor was touched. A null reaching the
    /// `MonitorEnter` instead would raise the monitor helper's NPE, whose
    /// message names a `synchronized` block, not the call. A receiver that
    /// cannot be null needs no guard, exactly as in `begin_splice`.
    ///
    /// No frame state may name the monitor, for the static shape's reason:
    /// nothing between the enter and the exit can deopt (`ir_lower` checks the
    /// final schedule), and an interpreter frame that named it would release
    /// it a second time.
    ///
    /// Round 11 wave 13 (lane `cha`): a non-zero `exact_class_id` is a
    /// GENUINELY virtual site `lib.rs` bound by class-hierarchy analysis. The
    /// callee is right only for a receiver of exactly that class, so a second
    /// guard follows the null check, still before the enter:
    ///
    /// ```text
    /// Guard(receiver != null)
    /// Guard(ExactClassIs(receiver, exact_class_id))
    /// MonitorEnter(receiver)
    /// ```
    ///
    /// The hierarchy answer is a prediction about the classes loaded at
    /// compile time. A frame already running this body when a subclass is
    /// defined keeps running it (retirement does not stop it), and that is
    /// exactly what the guard catches: it deopts with the same top-of-bci
    /// snapshot, the interpreter re-executes the `invokevirtual`, dispatch
    /// selects the override, and no monitor was touched. Placed after the
    /// enter it would have to release the monitor through a helper CALL on
    /// its failure edge, which clobbers the registers the deopt then reads.
    /// `0` (every statically bound site) emits no class guard.
    pub(in crate::ir) fn enter_receiver_monitor(
        &mut self,
        pc: usize,
        receiver: NodeId,
        exact_class_id: u32,
    ) -> Option<NodeId> {
        if receiver == NO_NODE || !self.splice.is_empty() {
            return None;
        }
        let receiver_needs_null_check = self
            .graph
            .nodes
            .get(receiver as usize)
            .is_none_or(|n| !crate::ir_check_elim::definitely_non_null(&n.op));
        // Either guard deopts with the snapshot the walk pushed at the top of
        // this bci; without one `resolve_frame_state_for_bci` would park the
        // interpreter at `pc` with an empty operand stack.
        if (receiver_needs_null_check || exact_class_id != 0) && !self.has_frame_state_at(pc) {
            return None;
        }
        if receiver_needs_null_check {
            let ctrl = self.ctrl_opt()?;
            let null = self.aconst_null();
            let cond = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![receiver, null], pc);
            self.graph.add(
                Op::Guard { bci: pc },
                IrType::Void,
                vec![ctrl, cond],
                Some(pc),
            );
        }
        if exact_class_id != 0 {
            let ctrl = self.ctrl_opt()?;
            let exact = self.add_data(
                Op::ExactClassIs {
                    class_id: exact_class_id,
                },
                IrType::Int,
                vec![receiver],
                pc,
            );
            self.graph.add(
                Op::Guard { bci: pc },
                IrType::Void,
                vec![ctrl, exact],
                Some(pc),
            );
        }
        // Round 15 wave 1 (lane sync, S6-2 first step): a census of the
        // recursive pairs a raw callee call would save -- the receiver is the
        // compiling synchronized method's own monitor, held around this CALL.
        let on_method_monitor = self
            .graph
            .method_monitor_node()
            .or(self
                .graph
                .method_self_lock
                .as_ref()
                .map(|lock| lock.monitor))
            .is_some_and(|m| self.holds_lock_same_object(m, receiver));
        if on_method_monitor {
            note_sync_splice_census(SYNC_CENSUS_CALLER_HELD_ON_METHOD_MONITOR);
        }
        let enter = self.graph.add(
            Op::MonitorEnter,
            IrType::Memory,
            vec![self.ctrl, self.mem, receiver],
            Some(pc),
        );
        self.mem = enter;
        Some(receiver)
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
            original_body_codes: _,
            original_body_handler_rows: _,
            unloaded_static_call_pcs,
            sites,
            field_info,
            invoke_info,
            new_info,
            anewarray_info,
            object_init_pcs,
            ldc_info,
            ldc2w_info,
            static_field_info,
            checkcast_info,
            instanceof_info,
            sync_held_sites,
            ldc_self_class_holds_lock,
            field_decl_class,
            final_field_pcs,
            byte_array_field_pcs,
            referent_load_pcs,
            unbox_intrinsics,
            splice_handler_rows,
            indy_trap_sites,
            indy_bridge_sites,
            indy_capture_news,
            lambda_impl_sites,
            // MEGAOSR19-2: planner rows (inline-cache seeds); JIT round 25
            // wave 7 (lane W7-D) keeps them on the graph too, for the
            // `charAt` predicate's profile gate (`Graph::seq_charat_receiver_rows`);
            // JIT round 21 (lane tlsub) also the spliced `ThreadLocal` sites' profiles.
            spliced_receiver_seeds,
            // CF23-2: planner-only too (`IrBuilder::set_splice_never_taken`).
            spliced_never_taken: _,
            spliced_never_taken_dead: _,
            spliced_always_taken: _,
            spliced_always_taken_dead: _,
            spliced_pruned_switch_edges: _,
            spliced_pruned_switch_dead: _,
            // CF-W8-3: planner-only (`ir_evidence::set_splice_cold_ranges`).
            spliced_cold_ranges: _,
            finalizable_new_classes,
            ldc_ref_string,
            ldc_ref_class,
            ldc_ref_slots,
            splice_getclass_compares,
            splice_caller_frames,
        } = tables;
        self.splice_caller_frames.extend(splice_caller_frames);
        // JIT round 21 (lane tlsub): combined-buffer pcs, disjoint from the
        // caller's own bcis the plan was built with.
        if let Some(plan) = self.tl_site_plan.as_deref_mut() {
            for (pc, counts) in &spliced_receiver_seeds {
                plan.note_receivers(*pc, counts);
            }
        }
        // JIT r20 wave 19 (lane zLdc, THROW20-4 `date`): the spliced bodies'
        // resolved String / Class constants, combined-buffer pcs disjoint from
        // the caller's own (`set_ldc_*` ran first); merged into the very maps
        // the `0x12`/`0x13` arm reads, so a spliced `ldc` builds the node a
        // top-level one does, with the CALLEE's `(holder, cp index)` and slot.
        self.graph
            .seq_charat_receiver_rows
            .extend(spliced_receiver_seeds);
        self.ldc_string_info.extend(ldc_ref_string);
        self.ldc_class_info.extend(ldc_ref_class);
        self.ldc_slot_info.extend(ldc_ref_slots);
        self.splice_getclass_compares
            .extend(splice_getclass_compares);
        // JIT r20 wave 18 (lane xLib): combined-buffer pcs; merged. Read by
        // the lowering only (`Lowerer::emit_inline_compact_getfield`).
        self.graph.referent_load_pcs.extend(referent_load_pcs);
        // JIT round 20 wave 19 (lane ySplice, SPLICE20-1): combined-buffer
        // pcs, disjoint from the door's own rows (`set_indy_capture_news`
        // runs first); merged, not replaced.
        self.indy_capture_news.extend(indy_capture_news);
        // JIT round 21 wave 15 (lane lambda3, LAMBDACAP21-2): combined-buffer
        // pcs, disjoint from the door's own plan (`set_lambda_impl_sites`
        // runs first); merged, not replaced.
        self.lambda_impl_sites.extend(lambda_impl_sites);
        // JIT round 20 wave 12 (lane hFin): class ids; merged into the
        // caller's own rows (`note_finalizable_new_classes`).
        self.graph
            .finalizable_new_classes
            .extend(finalizable_new_classes);
        // nio21 wave 11 (lane L3): combined-buffer pcs, disjoint from the
        // caller's own indy rows; merged, not replaced (the door's setters run
        // first).
        self.indy_trap_sites.extend(indy_trap_sites);
        self.indy_bridge_sites.extend(indy_bridge_sites);
        // nio21 wave 7 (lane L4): combined-buffer pcs; merged.
        self.add_byte_array_field_pcs(byte_array_field_pcs);
        // nio21 wave 4 (lane L2): keyed by body base; merged. The covered
        // ranges also go on the graph, where the lowering refuses any exit in
        // them that is not an armed call-site edge.
        for rows in splice_handler_rows.values() {
            self.graph
                .splice_handler_covered
                .extend(rows.iter().map(|r| (r.start_pc, r.end_pc)));
        }
        self.splice_handler_rows.extend(splice_handler_rows);
        // nio21 wave 2 (lane L3): combined-buffer pcs; merged.
        self.add_final_field_pcs(final_field_pcs);
        // JIT round 16 wave 7 (lane callbind): combined-buffer pcs, disjoint
        // from the caller's own `set_unbox_intrinsics` rows; merged, not
        // replaced. See `IrInlineTables::unbox_intrinsics`.
        self.unbox_intrinsics.extend(unbox_intrinsics);
        // Round 15 wave 3 (lane chain3, CHN15-2 producer): combined-buffer
        // pcs, disjoint from the caller's own rows; merged, not replaced.
        if !field_decl_class.is_empty() {
            self.set_field_decl_classes(field_decl_class);
        }
        // Round 14 wave 6 (lane sync6, SS8-1 a): see `begin_splice`.
        self.sync_splice_held_sites.extend(sync_held_sites);
        // Round 14 wave 7 (lane sync7, SS8-5): see
        // `try_fold_self_class_holds_lock`.
        self.ldc_self_class_holds_lock
            .extend(ldc_self_class_holds_lock);
        self.inline_sites.extend(sites);
        self.field_info.extend(field_info);
        self.invoke_info.extend(invoke_info);
        self.unloaded_static_call_pcs
            .extend(unloaded_static_call_pcs);
        self.new_info.extend(new_info);
        // W10-6: combined-buffer pcs, disjoint from the caller's own
        // `anewarray` rows (`set_anewarray_info` ran first); merged, not
        // replaced.
        self.anewarray_info.extend(anewarray_info);
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
            // JIT round 23 wave 3d: a receiver-carrying (instance) kind-4
            // row is not one: the bounded splice is static-only
            // (`IrSelfRecursion::classify`).
            if info.invoke_kind != 4 || crate::self_call_info_has_receiver(info) {
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

    /// Round 12 wave 4 (lane iropt3, proposal W3-1): mark the spliced
    /// `invokevirtual` at CALLER pc `caller_pc` as resolved for a receiver of
    /// exactly `receiver_class_id` -- a body the resolver would refuse as "not
    /// provably monomorphic" for an unknown receiver, and selects correctly
    /// for this one class only.
    ///
    /// The planner (`lib.rs`) asks this of a site the resolver admitted only
    /// under the exact-receiver contract, and it must keep the site's own
    /// `invoke_info` row: when [`Self::exact_receiver_proof`] cannot show the
    /// receiver is that class, the walk declines the splice and builds the
    /// ordinary call instead. Must be called before [`Self::build`], after
    /// [`Self::apply_inline_tables`] has installed the site. A class id of 0
    /// (the "unknown" sentinel) is never provable and so never spliced.
    pub fn add_exact_receiver_site(&mut self, caller_pc: usize, receiver_class_id: u32) {
        self.exact_receiver_sites
            .insert(caller_pc, receiver_class_id);
    }

    /// Round 12 wave 7 (lane iropt6, proposal W6-1): let the exact-receiver
    /// site at `caller_pc` (already registered by
    /// [`Self::add_exact_receiver_site`]) take a GUARDED splice when its
    /// receiver is not proven exact:
    ///
    /// ```text
    /// If(ExactClassIs(receiver, C))
    ///   true:  the body resolved for C, spliced
    ///   false: the ordinary call (its own `invoke_info` row and inline cache)
    /// Merge; Phi(memory); Phi(result)
    /// ```
    ///
    /// `HashMap.putVal`'s `this.newNode(..)` and `this.afterNodeInsertion(..)`
    /// are the shape: `LinkedHashMap` overrides both, so no rule makes them
    /// monomorphic, and the receiver is `putVal`'s `this`, so no exact proof
    /// exists either. See [`Self::begin_guarded_receiver_splice`] for the
    /// argument. A no-op for a pc without an exact-receiver row, and with
    /// `CRATONVM_JIT_IR_GUARDED_RECEIVER_SPLICE=0`.
    pub fn add_guarded_receiver_site(&mut self, caller_pc: usize) {
        self.guarded_receiver_sites.insert(caller_pc);
    }

    /// JIT round 23 wave 6 (`CRATONVM_JIT_IR_CHA_MONOMORPHIC_SPLICE`): the
    /// guarded site at `caller_pc` (registered with
    /// [`Self::add_exact_receiver_site`] and
    /// [`Self::add_guarded_receiver_site`]) tests `0 < class word <
    /// watermark` instead of its exact class
    /// ([`Self::cha_watermark_receiver_test`]), and its body is built with no
    /// exact-class fact about the receiver. A no-op for `watermark == 0`.
    pub fn add_cha_watermark_site(&mut self, caller_pc: usize, watermark: u32) {
        if watermark != 0 {
            self.cha_watermark_sites.insert(caller_pc, watermark);
        }
    }

    /// The hit test of a CHA watermark site: `(word > 0) & (word <
    /// watermark)` on the receiver's class word ([`Op::ClassWordOf`]).
    ///
    /// The class word is `0` for a null and for an array (its header names its
    /// component), and sign-extended, so a lambda proxy's id from the reserved
    /// high range reads negative: all three miss. Every class whose id is below
    /// the watermark was in the store when the VM showed that all of the site's
    /// loaded subtypes select this body, and a class outside that subtree
    /// cannot reach the site (the verifier). A class defined afterwards has a
    /// larger id and misses.
    pub(in crate::ir) fn cha_watermark_receiver_test(
        &mut self,
        receiver: NodeId,
        watermark: u32,
        pc: usize,
    ) -> NodeId {
        let word = self.add_data(Op::ClassWordOf, IrType::Int, vec![receiver], pc);
        let zero = self.iconst(0);
        let above_zero = self.add_data(Op::Cmp(CmpOp::Gt), IrType::Int, vec![word, zero], pc);
        // Cast: a watermark is a class count, far below `i32::MAX`; clamped so
        // an absurd one could only make the test stricter.
        let bound = self.iconst(i64::from(watermark.min(i32::MAX as u32)));
        let below = self.add_data(Op::Cmp(CmpOp::Lt), IrType::Int, vec![word, bound], pc);
        self.add_data(Op::And, IrType::Int, vec![above_zero, below], pc)
    }

    /// nio21 wave 6, lane L4: the guarded site at `caller_pc` is profile-hot,
    /// so its first body may be as large as [`GUARDED_SPLICE_HOT_MAX_BODY_BYTES`]
    /// ([`Self::guarded_splice_body_cap`]). Budget only: the class test, the
    /// miss edge and every other check of [`Self::guarded_receiver_splice_plan`]
    /// are unchanged. A no-op for a site that is not guarded, and inert under
    /// `CRATONVM_JIT_IR_GUARDED_SPLICE_HOT_CAP=0`.
    pub fn add_guarded_hot_cap_site(&mut self, caller_pc: usize) {
        self.guarded_hot_cap_sites.insert(caller_pc);
    }

    /// The size cap of the first guarded body at `pc`: the hot cap for a site
    /// the planner registered with [`Self::add_guarded_hot_cap_site`] (switch
    /// on), [`GUARDED_SPLICE_MAX_BODY_BYTES`] otherwise.
    pub(in crate::ir) fn guarded_splice_body_cap(&self, pc: usize) -> usize {
        if self.guarded_hot_cap_sites.contains(&pc) && ir_guarded_splice_hot_cap_enabled() {
            GUARDED_SPLICE_HOT_MAX_BODY_BYTES
        } else {
            GUARDED_SPLICE_MAX_BODY_BYTES
        }
    }

    /// nio21 wave 7, lane L4 (W6L4-2): the size cap of the SECOND and later
    /// guarded bodies at `pc` (bimorphic / polymorphic,
    /// [`Self::guarded_body_row_admitted`]): the site's cap
    /// ([`Self::guarded_splice_body_cap`]) under
    /// `CRATONVM_JIT_IR_GUARDED_SPLICE_HOT_CAP_MULTI`, the cold cap otherwise.
    /// The planner divided that cap between the bodies
    /// (`ir_guarded_multi_body_cap`); this is the builder's bound on each, so a
    /// body the planner appended is never declined here for its size.
    pub(in crate::ir) fn guarded_extra_body_cap(&self, pc: usize) -> usize {
        if crate::ir_guarded_splice_hot_cap_multi_enabled() {
            self.guarded_splice_body_cap(pc)
        } else {
            GUARDED_SPLICE_MAX_BODY_BYTES
        }
    }

    /// JIT round 19 (lane splice19, CH6W5-1 of `jit-r15w5-chain6-proposals`):
    /// let the NESTED exact-receiver site at combined pc `pc` (registered by
    /// [`Self::add_exact_receiver_site`], inside a spliced body) take the W6-1
    /// guarded splice when its receiver is not proven exact: an
    /// `ExactClassIs` test on the site's exact class, the body on the hit
    /// edge, the call row the planner kept on the miss edge. Honoured only
    /// under `CRATONVM_JIT_IR_NESTED_GUARDED_SPLICE`
    /// ([`ir_nested_guarded_splice_enabled`]). See
    /// [`Self::guarded_receiver_splice_plan`].
    pub fn add_nested_guarded_receiver_site(&mut self, pc: usize) {
        self.nested_guarded_sites.insert(pc);
    }

    /// Round 14 wave 2 (lane guard, GS-1 of `jit-r13-guardsplice-proposals-RETIRED-20260929.md`):
    /// let the guarded site at `caller_pc` (already registered by
    /// [`Self::add_exact_receiver_site`] for its first class and
    /// [`Self::add_guarded_receiver_site`]) test a SECOND class on the first
    /// test's miss edge, with the body resolved for `second_class_id` in the
    /// site table under [`bimorphic_second_site_key`]`(caller_pc)`:
    ///
    /// ```text
    /// If(ExactClassIs(r, A))
    ///   true:  body A
    ///   false: If(ExactClassIs(r, B))
    ///            true:  body B
    ///            false: the ordinary call
    /// ```
    ///
    /// See [`Self::begin_bimorphic_receiver_splice`]. A no-op for a site the
    /// guarded plan declines, and with
    /// `CRATONVM_JIT_IR_BIMORPHIC_GUARDED_SPLICE=0`.
    pub fn add_bimorphic_receiver_site(&mut self, caller_pc: usize, second_class_id: u32) {
        self.bimorphic_sites.insert(caller_pc, second_class_id);
    }

    /// Round 16 (lane mega): let the bimorphic site at `caller_pc` (already
    /// registered by [`Self::add_bimorphic_receiver_site`]) test further
    /// classes, `extra_class_ids` in order, each on the previous test's miss
    /// edge, with body `k + 2` in the site table under
    /// [`polymorphic_site_key`]`(caller_pc, k + 2)`; the call stays the last
    /// miss edge. A no-op unless `CRATONVM_JIT_IR_POLYMORPHIC_GUARDED_SPLICE`
    /// is on ([`ir_polymorphic_guarded_splice_enabled`]).
    pub fn add_polymorphic_receiver_sites(&mut self, caller_pc: usize, extra_class_ids: Vec<u32>) {
        self.polymorphic_sites.insert(caller_pc, extra_class_ids);
    }

    /// Round 14 wave 2 (lane guard, CH5-5 of `jit-r13-chain5-proposals-RETIRED-20260929.md`):
    /// the guarded site at `caller_pc` takes a DEOPT at its `invoke` as the
    /// miss edge instead of the call, so its receiver is exactly the site's
    /// class on every path past the test (until the next join). The planner
    /// registers only a site whose receiver profile names that class alone and
    /// whose trap the de-spec registry has not withdrawn. See
    /// [`Self::begin_trap_guarded_receiver_splice`]. The planner registers one
    /// only under `CRATONVM_JIT_IR_GUARDED_SPLICE_TRAP_MISS` (default off), or,
    /// for a factory site, `CRATONVM_JIT_IR_FACTORY_TRAP_MISS` (default on since
    /// the for-each residual round), or, for a `java.nio` byte-buffer site,
    /// `CRATONVM_JIT_IR_BUFFER_TRAP_MISS` (default on since nio20 wave 6); the
    /// builder honours what it is handed.
    pub fn add_guarded_trap_site(&mut self, caller_pc: usize) {
        self.guarded_trap_sites.insert(caller_pc);
    }

    /// Round 12 wave 6 (lane iropt5, proposal W5-2): report, through `sink`,
    /// WHICH splice a refused [`Self::build`] refused in, so the planner can
    /// build once more without that site instead of losing the method.
    ///
    /// After `build` returns `None`, `sink` holds a combined-buffer pc: one at
    /// or past the compiling method's `code_len` (inside a relocated body), or
    /// a caller pc of a planned site (the refusal came while entering it). It
    /// holds [`SPLICE_REFUSAL_NONE`] when the refusal was not inside or at a
    /// splice. The value after a successful build is meaningless.
    ///
    /// Attribution is a heuristic in one direction only: it may blame a splice
    /// for a refusal it did not cause (the retry then refuses again, at the
    /// cost of one build), and it never makes a build succeed that should not,
    /// because dropping a splice leaves the ordinary call, which the planner
    /// already does for every site a splice rule refuses. The walk records the
    /// pc of every instruction it is about to build, so any refusal inside an
    /// arm names that instruction; the end-of-walk refusals name the pc they
    /// already carry ([`Self::splice_guard_pc`], a latched refusal's pc, a
    /// lost merge's pc) or nothing.
    pub fn set_splice_refusal_sink(
        &mut self,
        sink: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        sink.store(SPLICE_REFUSAL_NONE, std::sync::atomic::Ordering::Relaxed);
        self.splice_refusal_sink = Some(sink);
    }

    /// Record `pc` (or nothing) as the place a refusal from here on belongs
    /// to; see [`Self::set_splice_refusal_sink`]. A no-op without a sink.
    pub(in crate::ir) fn note_splice_refusal_pc(&self, pc: Option<usize>, code_len: usize) {
        let Some(sink) = self.splice_refusal_sink.as_ref() else {
            return;
        };
        let blamed = pc
            .filter(|&p| p >= code_len || self.inline_sites.contains_key(&p))
            .unwrap_or(SPLICE_REFUSAL_NONE);
        sink.store(blamed, std::sync::atomic::Ordering::Relaxed);
    }

    /// Is the receiver of the spliced call at `pc` -- argument 0 of the site's
    /// operands, still on the abstract stack -- an object whose class is
    /// EXACTLY `class_id` (or `null`, which the splice's own receiver guard
    /// sends to the interpreter)?
    ///
    /// `None`: not proven, and the site must not be spliced. `Some(None)`: the
    /// receiver IS the `Op::New` of that class. `Some(Some(k))`: the receiver
    /// is the current value of caller local `k`, a slot the method writes
    /// exactly once, with that `Op::New`.
    ///
    /// # Why this is enough for an UNGUARDED splice
    ///
    /// * **Exactness.** `new C` creates an object whose class is `C`, and an
    ///   object never changes class. A slot the whole method writes once
    ///   (parameters count as a write, so no parameter slot qualifies; every
    ///   `xstore`, the wide forms, a `long`/`double` store into the slot below
    ///   and `iinc` count too) holds, at any point it is read, either nothing
    ///   (the JVMS verifier rejects a read before the store) or the value that
    ///   one store wrote. The builder saw that store write the `Op::New` of
    ///   `C`, as an SSA value rather than a phi, so every execution of it
    ///   stores a fresh `C`. That covers loop phis and the OSR entry value of
    ///   the slot alike: an interpreter frame's slot `k` was also written by
    ///   that one instruction (JVMTI `SetLocalObject` is a side table in this
    ///   VM, `jvmti.rs`, and never writes a live frame). A contradicting store
    ///   met later in the walk refuses the build if a splice relied on the slot
    ///   ([`Self::store_local`]).
    /// * **Selection.** The resolver selected the body by the JVMS walk from
    ///   `C` itself (`resolve_inline_site_from` with `receiver_class_id =
    ///   Some(C)`, which applies the receiver-dispatch faithfulness checks), so
    ///   it is the body `invokevirtual` dispatches to for a `C` receiver. A
    ///   subclass instance (a `LinkedHashMap` held in a `HashMap` local) is an
    ///   `Op::New` of another class id and is declined, not spliced.
    /// * **Deopt state.** Unchanged from any other splice: the receiver guard
    ///   and every trap inside the body resume at this `invoke` with
    ///   REEXECUTE, under the same replay fences
    ///   ([`Self::admits_after_committed_splice_store`]). The proof adds no
    ///   node and no speculation, so it has no deopt of its own.
    /// * **Exceptions.** The same as any splice: the body's implicit
    ///   exceptions resume at this `invoke`, whose handler range the caller's
    ///   own table answers.
    /// * **Redefinition and class loading.** No class-hierarchy assumption is
    ///   made, so no class load can invalidate it; that is what makes an
    ///   unguarded splice possible without a dependency. A redefinition cannot
    ///   add or remove methods of `C` (JVMTI `UNSUPPORTED_REDEFINITION_METHOD_*`),
    ///   and a new body for the declaring class takes the copied-bytecode
    ///   flush every splice relies on (`JitCache::copied_bytecode_classes`).
    ///
    /// Round 13 wave 10 (lane iropt8, HS-2): inside a splice the answer comes
    /// from node facts alone ([`Self::exact_receiver_node_proof`]), and is
    /// `Some(None)` (no caller slot is relied on).
    pub(in crate::ir) fn exact_receiver_proof(
        &self,
        pc: usize,
        class_id: u32,
    ) -> Option<Option<usize>> {
        if class_id == 0 {
            return None;
        }
        let receiver = self.exact_receiver_operand(pc)?;
        if crate::x64::dbg_jitc_enabled() {
            if let Some(class) = self.frozen_class_of(receiver).filter(|&c| c != class_id) {
                eprintln!(
                    "[ir] frozen receiver of class {class} at combined pc {pc} in {}: the planner's body is for class {class_id}",
                    self.method_label.as_deref().unwrap_or("<unknown>")
                );
            }
        }
        if !self.splice.is_empty() {
            if !crate::ir_splice_nested_exact_receiver_enabled() {
                return None;
            }
            return self
                .exact_receiver_node_proof(receiver, class_id)
                .then_some(None);
        }
        if matches!(
            self.graph.nodes.get(receiver as usize).map(|n| &n.op),
            Some(&Op::New { class_id: c, .. }) if c == class_id
        ) || self.static_final_exact_class_of(receiver) == Some(class_id)
            || self.frozen_class_of(receiver) == Some(class_id)
        {
            return Some(None);
        }
        // Round 14 wave 2 (lane guard, CH5-5): a miss-edge trap on this node
        // dominates the site (the fact is cleared at every join).
        if self.trap_exact_facts.get(&receiver) == Some(&class_id) {
            return Some(None);
        }
        // Interpreter round i5 wave 107 (lane L2): the requested OSR header's
        // tested φ of a profiled slot (`CRATONVM_JIT_IR_OSR_PROFILED_SLOT_CHECK`).
        if self.osr_profiled_exact_nodes.get(&receiver) == Some(&class_id) {
            return Some(None);
        }
        // al16: the slot holding the receiver was last written, on every path
        // the walk built to here, with a `new` of exactly this class.
        if self
            .locals
            .iter()
            .enumerate()
            .any(|(k, &v)| v == receiver && self.exact_receiver_flow.get(&k) == Some(&class_id))
        {
            return Some(None);
        }
        self.locals
            .iter()
            .enumerate()
            .find(|&(k, &v)| {
                v == receiver
                    && self.exact_receiver_single_store_slots.contains(&k)
                    && self.exact_receiver_locals.get(&k) == Some(&class_id)
            })
            .map(|(k, _)| Some(k))
    }

    /// al16: the class of `node` when every definition reaching it is an
    /// `Op::New` of that one class -- the node itself, or a φ (a spliced
    /// factory's return merge, say) whose every data input is. `None` for
    /// anything else, including a φ still missing an input and any loop
    /// header's φ.
    pub(in crate::ir) fn fresh_new_class_of(&self, node: NodeId) -> Option<u32> {
        let mut seen: HashSet<NodeId> = HashSet::new();
        let mut work = vec![node];
        let mut class: Option<u32> = None;
        while let Some(n) = work.pop() {
            if !seen.insert(n) || seen.len() > 16 {
                if seen.len() > 16 {
                    return None;
                }
                continue;
            }
            let entry = self.graph.nodes.get(n as usize)?;
            match entry.op {
                Op::New { class_id, .. } if class_id != 0 => {
                    if class.is_some_and(|c| c != class_id) {
                        return None;
                    }
                    class = Some(class_id);
                }
                // Never a loop header's: its back-edge input is patched in
                // after this question, and may be anything.
                Op::Phi
                    if entry.ty != IrType::Memory
                        && entry.inputs.len() > 1
                        && entry
                            .bytecode_pc
                            .is_some_and(|pc| !self.loop_headers.contains(&pc)) =>
                {
                    for &input in &entry.inputs[1..] {
                        if input == NO_NODE {
                            return None;
                        }
                        work.push(input);
                    }
                }
                _ => return None,
            }
        }
        class
    }

    /// al16: is a definition reaching `node`, through any φ (a loop
    /// header's included), an `Op::New` of exactly `class_id`? A hint for
    /// [`Self::guarded_receiver_splice_plan`]'s size cap, never a proof.
    pub(in crate::ir) fn reaches_fresh_new_of(&self, node: NodeId, class_id: u32) -> bool {
        let mut seen: HashSet<NodeId> = HashSet::new();
        let mut work = vec![node];
        while let Some(n) = work.pop() {
            if n == NO_NODE || !seen.insert(n) || seen.len() > 32 {
                continue;
            }
            match self.graph.nodes.get(n as usize) {
                Some(entry) => match entry.op {
                    Op::New { class_id: c, .. } if c == class_id => return true,
                    Op::Phi if entry.ty != IrType::Memory => {
                        work.extend(entry.inputs.iter().skip(1).copied());
                    }
                    _ => {}
                },
                None => {}
            }
        }
        false
    }

    /// The receiver operand of the spliced call at `pc` -- argument 0 of the
    /// site's operands, still on the abstract stack -- when the site has one.
    pub(in crate::ir) fn exact_receiver_operand(&self, pc: usize) -> Option<NodeId> {
        let site = self.inline_sites.get(&pc)?;
        if !site.receiver_is_arg0 || site.num_args == 0 {
            return None;
        }
        let at = self.stack.len().checked_sub(site.num_args)?;
        let receiver = *self.stack.get(at)?;
        (receiver != NO_NODE).then_some(receiver)
    }

    /// Round 13 wave 10 (lane iropt8, HS-2): is `node` provably an object of
    /// EXACTLY `class_id`, by facts about nodes rather than caller slots?
    ///
    /// * an `Op::New` of that class;
    /// * an `Op::LoadStatic` of a trusted `static final` whose object the VM
    ///   filed as that class ([`Self::set_static_final_exact_classes`]);
    /// * a node an earlier admitted exact-receiver proof named
    ///   ([`Self::exact_receiver_nodes`]): every definition reaching it is a
    ///   `new` of the class, and an object never changes class (a slot-based
    ///   proof a later store contradicts refuses the whole build,
    ///   [`Self::note_exact_receiver_store`]);
    /// * the current value of local 0 of an open splice whose body never
    ///   writes local 0 ([`Self::splice_bases_keeping_this`]): at run time that
    ///   slot holds the body's receiver at every pc, whatever φ the walk built
    ///   for it, so it is exactly the class when that receiver is -- asked of
    ///   the enclosing frame the same way. The innermost frame's locals are
    ///   `self.locals`; an outer frame's are the next inner frame's
    ///   `saved_locals` (its frame at the `invoke` it spliced). A guarded
    ///   splice's receiver is exact inside its own body only
    ///   ([`SpliceFrame::receiver_exact_class`], never a node fact).
    pub(in crate::ir) fn exact_receiver_node_proof(&self, node: NodeId, class_id: u32) -> bool {
        self.exact_receiver_node_proof_scoped(node, class_id, true)
    }

    /// [`Self::exact_receiver_node_proof`], with the guarded frames' edge-scoped
    /// facts ([`SpliceFrame::receiver_exact_class`]) consulted only when
    /// `use_guarded_frames`. Round 13 wave 11 (lane chain5): `false` answers
    /// whether the fact holds on EVERY path the node flows along, which is the
    /// only kind `Self::exact_receiver_nodes` may record.
    pub(in crate::ir) fn exact_receiver_node_proof_scoped(
        &self,
        node: NodeId,
        class_id: u32,
        use_guarded_frames: bool,
    ) -> bool {
        let named = |n: NodeId| {
            self.exact_receiver_nodes.get(&n) == Some(&class_id)
                // Wave 107 (lane L2): an every-path fact, as the line above.
                || self.osr_profiled_exact_nodes.get(&n) == Some(&class_id)
                // Round 14 wave 2 (lane guard, CH5-5): a dominating miss-edge
                // trap. Scoped to its region, so never an every-path fact.
                || (use_guarded_frames && self.trap_exact_facts.get(&n) == Some(&class_id))
                || matches!(
                    self.graph.nodes.get(n as usize).map(|x| &x.op),
                    Some(&Op::New { class_id: c, .. }) if c == class_id
                )
                || self.static_final_exact_class_of(n) == Some(class_id)
                // `CRATONVM_FROZEN_PAGES`: definition-time nodes (`frozen_def_class_of`)
                // hold on every path; flow-scoped identity guards (`frozen_class_of`)
                // hold only while `use_guarded_frames` admits guarded facts.
                || (if use_guarded_frames {
                    self.frozen_class_of(n)
                } else {
                    self.frozen_def_class_of(n)
                }) == Some(class_id)
        };
        if named(node) {
            return true;
        }
        // `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: the current value of a
        // compiling-method local the root rule proves exact -- written once,
        // by a `new` of the class ([`Self::exact_receiver_proof`]'s last arm):
        // at run time that slot holds that object wherever the walk is, so an
        // argument a splice bound to it is exactly the class.
        let root_slot_exact = |n: NodeId| {
            ir_static_final_exact_class_enabled()
                && self.splice.first().is_some_and(|root| {
                    root.saved_locals.iter().enumerate().any(|(k, &v)| {
                        v == n
                            && self.exact_receiver_single_store_slots.contains(&k)
                            && self.exact_receiver_locals.get(&k) == Some(&class_id)
                    })
                })
        };
        if root_slot_exact(node) {
            return true;
        }
        let mut cur = node;
        for depth in (0..self.splice.len()).rev() {
            let Some(frame) = self.splice.get(depth) else {
                return false;
            };
            let locals = match self.splice.get(depth + 1) {
                Some(inner) => &inner.saved_locals,
                None => &self.locals,
            };
            let written = self
                .splice_bases_written_locals
                .get(&frame.base)
                .copied()
                .unwrap_or(u64::MAX);
            // Match any unwritten parameter slot `k`: `k == 0` is `this` (or
            // static `arg0` under `ir_static_final_exact_class_enabled`), and
            // `k > 0` is an unwritten argument slot (LAMBDA321-5).
            let mut next_bound: Option<(usize, NodeId)> = None;
            for (k, &param_node) in frame.param_nodes.iter().enumerate() {
                if k >= 64 || param_node == NO_NODE || (written & (1u64 << k)) != 0 {
                    continue;
                }
                if k == 0 && frame.receiver == NO_NODE && !ir_static_final_exact_class_enabled() {
                    continue;
                }
                if locals.get(k) == Some(&cur) {
                    next_bound = Some((k, param_node));
                    break;
                }
            }
            let Some((slot, bound)) = next_bound else {
                return false;
            };
            cur = bound;
            if (slot == 0
                && use_guarded_frames
                && class_id != 0
                && frame.receiver != NO_NODE
                && frame.receiver_exact_class == class_id)
                || named(cur)
                || (depth == 0 && root_slot_exact(cur))
            {
                return true;
            }
        }
        false
    }

    /// May the walk splice the site at `pc`? Always for an ordinary site; for
    /// an exact-receiver one ([`Self::exact_receiver_sites`]) only with the
    /// switch on and the receiver proven. A `false` leaves the site to the
    /// ordinary call lowering, whose `invoke_info` row the planner kept.
    pub(in crate::ir) fn exact_receiver_splice_admits(&mut self, pc: usize) -> bool {
        let Some(&class_id) = self.exact_receiver_sites.get(&pc) else {
            return true;
        };
        if !ir_exact_receiver_splice_enabled() {
            return false;
        }
        self.frozen_prove_site_receiver(pc, class_id);
        match self.exact_receiver_proof(pc, class_id) {
            None => {
                // al16: why an exact site stays a call.
                if crate::x64::dbg_jitc_enabled() {
                    let receiver = self.exact_receiver_operand(pc);
                    let op = receiver
                        .and_then(|r| self.graph.nodes.get(r as usize))
                        .map(|n| &n.op);
                    let slots: Vec<usize> = self
                        .locals
                        .iter()
                        .enumerate()
                        .filter(|&(_, &v)| Some(v) == receiver)
                        .map(|(k, _)| k)
                        .collect();
                    eprintln!(
                        "[ir] exact-receiver UNPROVEN pc={pc} class={class_id} splice_depth={} receiver={receiver:?} op={op:?} slots={slots:?} flow={:?}",
                        self.splice.len(),
                        self.exact_receiver_flow,
                    );
                }
                false
            }
            Some(slot) => {
                if let Some(k) = slot {
                    self.exact_receiver_relied.insert(k);
                }
                // Round 13 wave 10 (lane iropt8, HS-2): the node, for the
                // exact sites nested in the bodies this splice opens.
                //
                // Round 13 wave 11 (lane chain5): unless the proof leaned on a
                // GUARDED frame (I8-2). There the receiver is exact on the hit
                // edge only, but the node itself (a field load, a parameter)
                // flows on past the guarded join: recorded here, a later
                // splice handed the same node -- `helper(m)` after `m.put(..)`,
                // `helper`'s nested `m.size()` -- spliced `HashMap.size` with
                // no test for a receiver the miss edge had let through.
                if let Some(receiver) = self.exact_receiver_operand(pc) {
                    // Round 14 wave 2 (lane guard, CH5-5): nor a proof that
                    // leaned on a miss-edge trap, which holds only up to the
                    // next join (`trap_exact_facts`).
                    let flow_scoped = self.trap_exact_facts.get(&receiver) == Some(&class_id)
                        || self.frozen_known.iter().any(|&(n, _, _)| n == receiver);
                    let record = if flow_scoped {
                        self.exact_receiver_node_proof_scoped(receiver, class_id, false)
                    } else {
                        self.splice.is_empty()
                            || self.exact_receiver_node_proof_scoped(receiver, class_id, false)
                    };
                    if record {
                        self.exact_receiver_nodes.insert(receiver, class_id);
                    }
                }
                true
            }
        }
    }

    /// [`Self::store_local`]'s half of [`Self::exact_receiver_proof`]: record
    /// what a single-store slot's one store wrote, and withdraw the slot when
    /// a second store (or a non-allocation) reaches it after all.
    pub(in crate::ir) fn note_exact_receiver_store(&mut self, idx: usize, val: NodeId) {
        if !self.splice.is_empty() || !self.exact_receiver_single_store_slots.contains(&idx) {
            return;
        }
        let stored = match self.graph.nodes.get(val as usize).map(|n| &n.op) {
            Some(&Op::New { class_id, .. }) if class_id != 0 => Some(class_id),
            _ => None,
        };
        match (stored, self.exact_receiver_locals.get(&idx).copied()) {
            (Some(c), None) => {
                self.exact_receiver_locals.insert(idx, c);
            }
            (Some(c), Some(prev)) if c == prev => {}
            _ => {
                self.exact_receiver_single_store_slots.remove(&idx);
                self.exact_receiver_locals.remove(&idx);
                if self.exact_receiver_relied.contains(&idx) {
                    self.refuse(line!(), idx);
                }
            }
        }
    }

    /// Round 12 wave 7 (lane iropt6, W6-1): may the `invokevirtual` at `pc`
    /// (or, since round 13 wave 12, the `invokeinterface` of a profile-selected
    /// class: lane guardsplice, W7-1),
    /// whose `invoke_info` row is `(num_args, ret_type)`, take a guarded splice
    /// ([`Self::add_guarded_receiver_site`])? `Some((receiver, class id))` when
    /// it may. Pure: a `None` leaves the ordinary call to be built exactly as
    /// before, and every check [`Self::begin_splice`] could still fail on is
    /// made here first, so the graph is only touched once the splice will open.
    ///
    /// Outermost only: the caller's own sites are the only exact-receiver
    /// sites the planner registers, and a deopt inside the body must resolve
    /// the frame state the walk pushed at the top of THIS bci (a snapshot
    /// exists only outside every splice).
    pub(in crate::ir) fn guarded_receiver_splice_plan(
        &self,
        pc: usize,
        num_args: usize,
        ret_type: u8,
    ) -> Option<(NodeId, u32)> {
        // JIT round 19 (lane splice19, CH6W5-1): a nested exact-receiver site
        // the planner registered may be guarded inside a splice. Its miss
        // edge is the same call the unproven exact site lowers today; the
        // body's traps resume exactly as an exact nested splice's do (the
        // chain snapshot at their own pc, or the outermost `invoke`'s frame).
        let nested = !self.splice.is_empty();
        let registered = if nested {
            self.nested_guarded_sites.contains(&pc)
                && ir_nested_guarded_splice_enabled()
                && !self.sync_splice_open()
        } else {
            self.guarded_receiver_sites.contains(&pc)
        };
        if !registered
            || self.signature_polymorphic_site(pc)
            || !ir_guarded_receiver_splice_enabled()
            || self.sync_direct_calls.contains_key(&pc)
        {
            return None;
        }
        let class_id = *self.exact_receiver_sites.get(&pc)?;
        let site = self.inline_sites.get(&pc)?;
        // `java/lang/Object`'s own bodies (`equals`, `toString`) are resolved
        // for a receiver of exactly `Object`, which is almost never the
        // receiver: the class test would miss on every call. The call stays.
        let at = self.stack.len().checked_sub(num_args)?;
        // al16: no size cap when a definition reaching the receiver is a
        // fresh `new` of exactly this class (a spliced factory's hit edge,
        // `ArrayList.iterator()`'s `new Itr`): the test is the expected hit,
        // not a profile's guess (`CRATONVM_JIT_IR_GUARDED_FRESH_NEW_UNCAPPED`).
        let fresh_hit = self
            .stack
            .get(at)
            .is_some_and(|&r| r != NO_NODE && self.reaches_fresh_new_of(r, class_id));
        // nio21 wave 6, lane L4: a profile-hot site's cap is the hot one
        // (`guarded_splice_body_cap`).
        if class_id == 0
            || (site.code_len > self.guarded_splice_body_cap(pc) && !(fresh_hit))
            || !site.receiver_is_arg0
            || site.num_args == 0
            || site.num_args != num_args
            || site.returns_value != (ret_type != b'V')
            || site.arg_local_slots.len() != num_args
            || site
                .arg_local_slots
                .iter()
                .any(|&s| s as usize >= site.max_locals)
            || site.method_key.starts_with("java/lang/Object.")
        {
            return None;
        }
        let args = self.stack.get(at..)?;
        let receiver = *args.first()?;
        if args.contains(&NO_NODE) {
            return None;
        }
        // An allocation of another class can never pass the test.
        if matches!(
            self.graph.nodes.get(receiver as usize).map(|n| &n.op),
            Some(&Op::New { class_id: c, .. }) if c != class_id
        ) {
            return None;
        }
        self.ctrl_opt()?;
        // A nested site has no flat snapshot at its combined pc by design;
        // see above.
        if !nested && !self.has_frame_state_at(pc) {
            return None;
        }
        Some((receiver, class_id))
    }

    /// Round nio20 wave 2, lane L3 (`utfencode`;
    /// [`ir_splice_nested_profile_trap_enabled`]): may the `invokeinterface`
    /// at combined pc `pc`, INSIDE an open splice, take a trap-guarded splice
    /// ([`Self::begin_trap_guarded_receiver_splice`])? Such a site is an
    /// exact-receiver row the planner registered for a nested body
    /// ([`Self::add_exact_receiver_site`]); at an `invokeinterface` that row
    /// can only be the VM resolver's one-class profile row (the nest-exact
    /// rows are `invokevirtual` only). `Some((receiver, class id))` when:
    ///
    /// * a splice is open and no synchronized window is (a guard there has no
    ///   chain of its own to resume through) and the pc is no synchronized
    ///   site (`begin_splice` refuses a guarded hit edge there);
    /// * the body row matches the call's shape, exactly as
    ///   [`Self::guarded_receiver_splice_plan`] asks of a top-level one;
    /// * the chain snapshot the walk pushed at the top of `pc` exists
    ///   ([`Self::relax_fence_by_chain`]): the guard resumes the callee's own
    ///   frame AT the `invokeinterface`, which the interpreter re-runs, so
    ///   nothing in the enclosing splices is replayed.
    ///
    /// The miss edge is a deopt (charged like any chain trap), the hit edge
    /// the body resolved for the class, so it is sound for every receiver; a
    /// wrong profile costs traps, never a wrong target.
    ///
    /// Round nio20 wave 3, lane L4 (L3W2-3): `op` is the invoke opcode. An
    /// `invokevirtual` (`0xb6`) row is a nest-exact one (resolved for its
    /// constant-pool class), admitted only under
    /// [`ir_splice_nested_exact_trap_enabled`]; the VM keeps such a row under
    /// that switch only where no evidence names another receiver class. The
    /// argument above is the same for both: the guard is exact, the body is
    /// the one that class dispatches to, and a null or any other class deopts
    /// to the interpreter at this `invoke`. The guard records no dominance
    /// fact (see [`Self::begin_trap_guarded_receiver_splice`]).
    pub(in crate::ir) fn nested_trap_guard_plan(
        &mut self,
        pc: usize,
        op: u8,
        num_args: usize,
        ret_type: u8,
    ) -> Option<(NodeId, u32)> {
        let switched_on = match op {
            0xb9 => ir_splice_nested_profile_trap_enabled(),
            0xb6 => ir_splice_nested_exact_trap_enabled(),
            _ => false,
        };
        if self.splice.is_empty()
            || !switched_on
            || self.signature_polymorphic_site(pc)
            || self.sync_splice_open()
            || self.sync_splice_sites.contains_key(&pc)
            || self.sync_splice_held_sites.contains(&pc)
        {
            return None;
        }
        let class_id = *self.exact_receiver_sites.get(&pc)?;
        let site = self.inline_sites.get(&pc)?;
        if class_id == 0
            || !site.receiver_is_arg0
            || site.num_args == 0
            || site.num_args != num_args
            || site.returns_value != (ret_type != b'V')
            || site.arg_local_slots.len() != num_args
            || site
                .arg_local_slots
                .iter()
                .any(|&s| s as usize >= site.max_locals)
            || site.method_key.starts_with("java/lang/Object.")
        {
            return None;
        }
        let at = self.stack.len().checked_sub(num_args)?;
        let args = self.stack.get(at..)?;
        let receiver = *args.first()?;
        if args.contains(&NO_NODE) {
            return None;
        }
        // An allocation of another class can never pass the test, nor a node
        // already proven exactly another class (the guard would trap on every
        // arrival).
        if matches!(
            self.graph.nodes.get(receiver as usize).map(|n| &n.op),
            Some(&Op::New { class_id: c, .. }) if c != class_id
        ) || self
            .exact_receiver_nodes
            .get(&receiver)
            .is_some_and(|&c| c != class_id)
        {
            return None;
        }
        self.ctrl_opt()?;
        if !self.relax_fence_by_chain(pc) {
            return None;
        }
        if ir_bail_reporting() {
            eprintln!(
                "[ir] nested profile trap guard at combined pc {pc} (opcode {op:#04x}) on class {class_id} in {}",
                self.method_label.as_deref().unwrap_or("<unknown>")
            );
        }
        Some((receiver, class_id))
    }

    /// Round 12 wave 7 (lane iropt6, proposal W6-1): build the guarded splice
    /// planned by [`Self::guarded_receiver_splice_plan`] and enter its body,
    /// returning the combined-buffer pc to continue the walk at (`None`
    /// refuses the build; the planner's W5-2 rebuild then keeps the call).
    ///
    /// ```text
    /// t    = ExactClassIs(receiver, C)        // total: null / array / other -> 0
    /// If(t)
    ///   Proj(1): r1 = Call(info_ptr, args)    // the ordinary call, built FIRST
    ///   Proj(0): the body resolved for C, walked as a multi-return splice
    /// Merge(miss, returns..); Phi(memory); Phi(result)
    /// ```
    ///
    /// # Why it is sound
    ///
    /// * **Selection.** The body is the one the resolver selected by the JVMS
    ///   walk from `C` (`resolve_inline_site_from` with `receiver_class_id =
    ///   Some(C)`), which is what `invokevirtual` dispatches to for a receiver
    ///   whose class is exactly `C`, and the hit edge is taken only for such a
    ///   receiver. A class id never changes for an object's lifetime, and a
    ///   class that defines or loads later is another id, so no class-hierarchy
    ///   assumption is made and no dependency is needed.
    /// * **Where `C` came from does not matter** (round 13 wave 12, lane
    ///   guardsplice, W7-1): the constant-pool class, or the class the site's
    ///   receiver profile says dominates it. Only the test's exactness and the
    ///   body's selection from `C` carry the argument. A profile-selected `C`
    ///   may be defined by a loader that dies before the caller's; the VM's
    ///   resolver records it (and the body's declaring class) as copied into
    ///   this compile, which is what a redefinition withdraws the body by, and
    ///   an unload too (`CRATONVM_JIT_UNLOAD_WITHDRAWS_COPIED`). With that
    ///   switch off, an unloaded `C` leaves a hit edge no receiver can take
    ///   (class ids are never reused).
    /// * **Null.** `ExactClassIs` answers `0` for a null, so a null receiver
    ///   takes the call, which raises the NPE exactly where it did. The body's
    ///   receiver is non-null on the hit edge, so [`Self::begin_splice`] adds no
    ///   null guard for it, and [`SpliceFrame::receiver`]'s non-null premise
    ///   holds.
    /// * **Deopt and replay.** The hit edge is an exact-receiver splice in
    ///   every respect: every trap in the body resumes at this `invoke` with
    ///   REEXECUTE under the same fences, and the interpreter's dispatch on the
    ///   same receiver selects the same body. The miss edge is the call the
    ///   site built before, at this bci, outside every splice, so it commits no
    ///   splice fence. It is built before the body is walked, which keeps the
    ///   body's fences blind to it: the two edges never both run.
    /// * **Exceptions.** The call's exceptions are raised at this bci and
    ///   dispatched by the caller's own handler table, as before; the body's
    ///   implicit exceptions deopt, as in any splice.
    /// * **The join.** The call is recorded as the first exit edge of the
    ///   splice frame, and the frame is marked multi-return, so every `return`
    ///   the body reaches is another edge and
    ///   [`Self::finish_multi_return_splice`] merges control, memory and the
    ///   result. The caller's locals and stack are the saved ones on every edge.
    pub(in crate::ir) fn begin_guarded_receiver_splice(
        &mut self,
        pc: usize,
        // The `invoke`'s length: 3 for `invokevirtual`, 5 for
        // `invokeinterface` (round 13 wave 12, lane guardsplice, W7-1: a
        // profile-selected class guards interface sites too).
        instr_len: usize,
        info_ptr: usize,
        num_args: usize,
        ret_type: u8,
        receiver: NodeId,
        class_id: u32,
    ) -> Option<usize> {
        // JIT round 23 wave 6 (`CRATONVM_JIT_IR_CHA_MONOMORPHIC_SPLICE`): a CHA
        // watermark site (outermost only) takes the one-test form below with
        // the watermark test; no second class, no deopt miss edge.
        let cha_watermark = if self.splice.is_empty() {
            self.cha_watermark_sites.get(&pc).copied()
        } else {
            None
        };
        // j27 wave 5 (lane PROXY): or an exact class set, the same one-test
        // form (`cha_class_set::cha_class_set_receiver_test`).
        let cha_class_set = self.cha_class_set_at(pc);
        let cha_guarded = cha_watermark.is_some() || cha_class_set.is_some();
        // Round 14 wave 2 (lane guard, GS-1): a second class on the miss edge.
        let covered =
            self.pc_in_splice_handler_cover(pc) || self.enter_covered_nested_instruction();
        if !cha_guarded && !covered {
            if let Some(second) = self.bimorphic_second_class(pc, num_args, ret_type, class_id) {
                return self.begin_bimorphic_receiver_splice(
                    pc, instr_len, info_ptr, num_args, ret_type, receiver, class_id, second,
                );
            }
        }
        // Round 14 wave 2 (lane guard, CH5-5): a deopt as the miss edge. The
        // planner registers a site only under a switch that allows it (the
        // general one, or the factory sites' since the for-each residual
        // round: `lib.rs` `ir_factory_trap_miss_enabled`).
        if !cha_guarded && self.guarded_trap_sites.contains(&pc) {
            return self.begin_trap_guarded_receiver_splice(pc, instr_len, receiver, class_id);
        }
        let ctrl = self.ctrl_opt()?;
        let at = self.stack.len().checked_sub(num_args)?;
        let args: Vec<NodeId> = self.stack.get(at..)?.to_vec();
        let mem_before = self.mem;
        let exact = match (cha_watermark, cha_class_set) {
            (Some(watermark), _) => self.cha_watermark_receiver_test(receiver, watermark, pc),
            (None, Some(set)) => self.cha_class_set_receiver_test(receiver, &set, pc),
            (None, None) => self.one_class_receiver_test(receiver, class_id, pc),
        };
        let iff = self
            .graph
            .add(Op::If, IrType::Control, vec![ctrl, exact], Some(pc));
        let hit = self
            .graph
            .add(Op::Proj(0), IrType::Control, vec![iff], Some(pc));
        let miss = self
            .graph
            .add(Op::Proj(1), IrType::Control, vec![iff], Some(pc));
        // The miss edge: the `Op::Call` the `invokevirtual` arm builds for this
        // site, typed the same way.
        let ty = match ret_type {
            b'V' => IrType::Void,
            b'L' | b'[' => IrType::Ref,
            b'J' => IrType::Long,
            b'D' => IrType::Double,
            b'F' => IrType::Float,
            _ => IrType::Int,
        };
        let mut inputs = Vec::with_capacity(2 + num_args);
        inputs.push(miss);
        inputs.push(mem_before);
        inputs.extend(args);
        let call = self.graph.add(Op::Call { info_ptr }, ty, inputs, Some(pc));
        let miss = if covered {
            self.ctrl = miss;
            self.mem = call;
            self.plant_call_throw_edges(pc, call, 0);
            if !self.splice_handler_planted_calls.contains(&call) {
                return None;
            }
            self.ctrl
        } else {
            miss
        };

        // JIT round 23 wave 2e (W4L1-1): the hit body is walked on the edge the
        // call is not on; the memo applies the call at the join instead
        // (`finish_multi_return_splice`, which this frame closes through).
        if self.field_memo_defer_miss_call {
            self.field_memo_deferred_calls.push(call);
        }
        let miss_value = if ret_type == b'V' { NO_NODE } else { call };
        // JIT round 19 (lane splice19, CH6W5-1): a miss call INSIDE a splice
        // is a surviving call of the enclosing body, with every effect
        // `note_splice_call` records (the replay commit, escaped constructor
        // receivers, tainted splice-local arguments). None of them is on the
        // hit edge, so all of them are recorded at the join
        // (`SpliceFrame::miss_call`; wave 2, lane splice2, SPLICE19-6: wave 1
        // recorded the taints here, before the hit body was walked). The
        // call gets no post-call chain snapshot: its successor is the join.
        let nested = !self.splice.is_empty();
        // The hit edge: the body, from the memory state before the call.
        self.ctrl = hit;
        self.mem = mem_before;
        self.guarded_splice_pending = Some((pc, receiver));
        let next = self.begin_splice(pc, instr_len);
        self.guarded_splice_pending = None;
        let next = next?;
        let frame = self.splice.last_mut()?;
        frame.multi_return = true;
        frame.miss_call = nested.then_some(call);
        frame.exits.push((miss, call, miss_value));
        if nested && crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-nested-guarded-splice at combined pc={pc} depth={} \
                 class id {class_id}",
                self.splice.len() - 1,
            );
        }
        Some(next)
    }

    /// Round 14 wave 2 (lane guard, GS-1): the second class of the bimorphic
    /// site at `pc`, when its guarded plan (already made for `first_class`,
    /// [`Self::guarded_receiver_splice_plan`]) may test one: the switch is on,
    /// the planner registered a second class other than the first
    /// ([`Self::add_bimorphic_receiver_site`]), and its body under
    /// [`bimorphic_second_site_key`] passes every check the first body's plan
    /// made. Pure: a `None` builds the one-class guarded splice.
    pub(in crate::ir) fn bimorphic_second_class(
        &self,
        pc: usize,
        num_args: usize,
        ret_type: u8,
        first_class: u32,
    ) -> Option<u32> {
        let &second = self.bimorphic_sites.get(&pc)?;
        if second == 0 || second == first_class || !ir_bimorphic_guarded_splice_enabled() {
            return None;
        }
        // Nested receiver chains use the same admission switch as the planner.
        // Each open body owns its remaining arms in its SpliceFrame.
        if !self.splice.is_empty() && !crate::r25_w7b_chain_splice::nested_pair_switch_on() {
            return None;
        }
        // nio21 wave 7 (lane L4): the row check takes the site pc for its cap.
        self.guarded_body_row_admitted(pc, bimorphic_second_site_key(pc), num_args, ret_type)
            .then_some(second)
    }

    /// Whether the guarded body in the site table under `key` passes the
    /// checks the first body's guarded plan made (the bimorphic second body's
    /// rule, shared with the polymorphic bodies since round 16). nio21 wave 7,
    /// lane L4: its size against the site `pc`'s
    /// [`Self::guarded_extra_body_cap`] (was always the cold cap).
    pub(in crate::ir) fn guarded_body_row_admitted(
        &self,
        pc: usize,
        key: usize,
        num_args: usize,
        ret_type: u8,
    ) -> bool {
        let cap = self.guarded_extra_body_cap(pc);
        self.inline_sites.get(&key).is_some_and(|site| {
            site.code_len <= cap
                && site.receiver_is_arg0
                && site.num_args != 0
                && site.num_args == num_args
                && site.returns_value == (ret_type != b'V')
                && site.arg_local_slots.len() == num_args
                && site
                    .arg_local_slots
                    .iter()
                    .all(|&s| (s as usize) < site.max_locals)
                && !site.method_key.starts_with("java/lang/Object.")
        })
    }

    /// Round 16 (lane mega): the third and later classes of the polymorphic
    /// site at `pc`, in test order, as far as each is a new class whose body
    /// row is admitted ([`Self::guarded_body_row_admitted`]); the first that
    /// is not ends the list. Empty with
    /// `CRATONVM_JIT_IR_POLYMORPHIC_GUARDED_SPLICE` off. Pure.
    pub(in crate::ir) fn polymorphic_extra_classes(
        &self,
        pc: usize,
        num_args: usize,
        ret_type: u8,
        first_class: u32,
        second_class: u32,
    ) -> Vec<u32> {
        let mut extras: Vec<u32> = Vec::new();
        // JIT round 19 wave 1 (lane osrpoly19): or the OSR-door-scoped switch;
        // the planner registers sites under it only for a door compile.
        if !ir_polymorphic_guarded_splice_enabled() && !osr_door_ir_polymorphic_splice_enabled() {
            return extras;
        }
        let Some(listed) = self.polymorphic_sites.get(&pc) else {
            return extras;
        };
        for (k, &class_id) in listed.iter().enumerate() {
            let body = k + 2;
            if class_id == 0
                || class_id == first_class
                || class_id == second_class
                || extras.contains(&class_id)
                || body >= 16
                || !self.guarded_body_row_admitted(
                    pc,
                    polymorphic_site_key(pc, body),
                    num_args,
                    ret_type,
                )
            {
                break;
            }
            extras.push(class_id);
        }
        extras
    }

    /// Round 14 wave 2 (lane guard, GS-1 of `jit-r13-guardsplice-proposals-RETIRED-20260929.md`):
    /// the guarded splice of a BIMORPHIC site, two class tests and then the
    /// call, returning the combined-buffer pc to continue the walk at (`None`
    /// refuses the build; the planner's W5-2 rebuild then keeps the call).
    ///
    /// ```text
    /// If(ExactClassIs(r, A))
    ///   Proj(1): If(ExactClassIs(r, B))
    ///              Proj(1): c = Call(info_ptr, args)   // built FIRST
    ///              Proj(0): body B                     // walked first
    ///            join_B = Merge(c, returns of B); Phi(mem); Phi(value)
    ///   Proj(0): body A                                // walked when B closes
    /// Merge(join_B, returns of A); Phi(mem); Phi(value)
    /// ```
    ///
    /// The walk visits bodies one at a time, so the form is built inside out:
    /// body B is a guarded multi-return frame whose first exit edge is the
    /// call (exactly [`Self::begin_guarded_receiver_splice`]'s frame, on the
    /// second test's hit edge), and when that frame closes the builder does
    /// not resume the caller: its join becomes the first exit edge of body A's
    /// frame, opened on the first test's hit edge
    /// ([`Self::reopen_bimorphic_first_body`], called by
    /// [`Self::finish_multi_return_splice`]). Body B is found in the one-body-
    /// per-pc site table under [`bimorphic_second_site_key`] and swapped in at
    /// `pc` only for the one [`Self::begin_splice`] that reads it.
    ///
    /// # Why it is sound
    ///
    /// Every argument of the one-class form holds edge by edge: each body is
    /// the one the resolver selected for exactly its class
    /// (`resolve_inline_site_from` with that receiver class), each is entered
    /// only on its own exact test's hit edge (so its receiver is non-null and
    /// exactly that class inside it, `SpliceFrame::receiver_exact_class`),
    /// and a null or any third class reaches the call. The three edges are
    /// disjoint, so nothing one body commits is on another's path: that is why
    /// the replay fences body B raised are dropped before body A is walked.
    /// Every trap in either body resumes at this `invoke` with REEXECUTE from
    /// the snapshot taken at the top of this bci, before any of the three
    /// edges ran, and the interpreter's dispatch selects the same body for the
    /// same receiver.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir) fn begin_bimorphic_receiver_splice(
        &mut self,
        pc: usize,
        instr_len: usize,
        info_ptr: usize,
        num_args: usize,
        ret_type: u8,
        receiver: NodeId,
        first_class: u32,
        second_class: u32,
    ) -> Option<usize> {
        // Round 16 (lane mega): a polymorphic site tests more classes, each on
        // the previous test's miss edge; the call stays the last miss edge.
        let extras =
            self.polymorphic_extra_classes(pc, num_args, ret_type, first_class, second_class);
        let mut classes: Vec<u32> = Vec::with_capacity(2 + extras.len());
        classes.push(first_class);
        classes.push(second_class);
        classes.extend(extras);
        let ctrl = self.ctrl_opt()?;
        let at = self.stack.len().checked_sub(num_args)?;
        let args: Vec<NodeId> = self.stack.get(at..)?.to_vec();
        let mem_before = self.mem;
        // The test chain: `hits[i]` is class `i`'s hit edge; `miss` ends as
        // the last test's miss edge.
        let trap_miss =
            self.splice.is_empty() && self.guarded_trap_sites.contains(&pc) && classes.len() == 2;
        let mut hits: Vec<NodeId> = Vec::with_capacity(classes.len());
        let mut miss = ctrl;
        // JIT round 19 proposal wave (lane osr19, OSRPOLY19-2): one class-word
        // load at the chain head, then one compare per class
        // (`ir_class_word_chain_enabled`). `None` keeps the per-test
        // `ExactClassIs` form.
        let class_word = (ir_class_word_chain_enabled() && classes.iter().all(|&c| c != 0))
            .then(|| self.add_data(Op::ClassWordOf, IrType::Int, vec![receiver], pc));
        let last_class_idx = classes.len() - 1;
        for (idx, &class_id) in classes.iter().enumerate() {
            let exact = match class_word {
                Some(word) => {
                    // The class word is a sign-extended `int`
                    // (`ir_lower::class_word_of_bytes`): compare with the id's
                    // own `int` reading.
                    // Cast: the id's bits, read as the `int` the word is.
                    let id = self.iconst(i64::from(class_id as i32));
                    self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![word, id], pc)
                }
                None => self.add_data(
                    Op::ExactClassIs { class_id },
                    IrType::Int,
                    vec![receiver],
                    pc,
                ),
            };
            if trap_miss && idx == last_class_idx {
                self.graph.add(
                    Op::Guard { bci: pc },
                    IrType::Void,
                    vec![miss, exact],
                    Some(pc),
                );
                hits.push(miss);
            } else {
                let iff = self
                    .graph
                    .add(Op::If, IrType::Control, vec![miss, exact], Some(pc));
                hits.push(
                    self.graph
                        .add(Op::Proj(0), IrType::Control, vec![iff], Some(pc)),
                );
                miss = self
                    .graph
                    .add(Op::Proj(1), IrType::Control, vec![iff], Some(pc));
            }
        }
        // The miss edge: either an uncommon trap (already planted on `miss`) or
        // the site's own call, typed as the `invoke` arm types it.
        let depth = self.splice.len();
        let nested = depth > 0;
        let (call, miss_value) = if trap_miss {
            (NO_NODE, NO_NODE)
        } else {
            let ty = match ret_type {
                b'V' => IrType::Void,
                b'L' | b'[' => IrType::Ref,
                b'J' => IrType::Long,
                b'D' => IrType::Double,
                b'F' => IrType::Float,
                _ => IrType::Int,
            };
            let mut inputs = Vec::with_capacity(2 + num_args);
            inputs.push(miss);
            inputs.push(mem_before);
            inputs.extend(args.iter().copied());
            let call = self.graph.add(Op::Call { info_ptr }, ty, inputs, Some(pc));
            let miss_value = if ret_type == b'V' { NO_NODE } else { call };
            (call, miss_value)
        };
        // JIT round 19 wave 2 (lane splice2, SPLICE19-4): inside a splice the
        // call's effects are recorded at the outermost join, as the one-class
        // nested splice records them (`SpliceFrame::miss_call`), and every
        // body starts from the enclosing body's fence state at this `invoke`.
        let saved_committed_bases = if nested {
            self.splice_committed_bases.clone()
        } else {
            HashSet::new()
        };
        let saved_call_committed = nested && self.splice_call_committed;
        // The innermost test's body first (body B of a bimorphic site), on
        // its hit edge. Its row is swapped in at `pc` for the one
        // `begin_splice` that reads it, and the first body's row put back
        // whatever that call answers.
        let last = classes.len() - 1;
        let last_key = polymorphic_site_key(pc, last);
        let last_site = self.inline_sites.get(&last_key)?.clone();
        let first_site = self.inline_sites.insert(pc, last_site);
        self.ctrl = *hits.get(last)?;
        self.mem = mem_before;
        self.guarded_splice_pending = Some((pc, receiver));
        let next = self.begin_splice(pc, instr_len);
        self.guarded_splice_pending = None;
        match first_site {
            Some(site) => {
                self.inline_sites.insert(pc, site);
            }
            None => {
                self.inline_sites.remove(&pc);
            }
        }
        let next = next?;
        let frame = self.splice.last_mut()?;
        frame.multi_return = true;
        // `begin_splice` named the first class (the site's exact row); on
        // this edge the receiver is exactly the innermost test's class.
        frame.receiver_exact_class = *classes.get(last)?;
        if !trap_miss {
            frame.miss_call = nested.then_some(call);
            frame.exits.push((miss, call, miss_value));
        }
        if nested && crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-nested-bimorphic-splice at combined pc={pc} depth={depth} \
                 class ids {}",
                classes
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join("+"),
            );
        }
        // Every other body, walked innermost test first as each one's join
        // closes (`pending` pops from the back).
        let pending: Vec<(NodeId, u32, usize)> = (0..last)
            .map(|i| (hits[i], classes[i], polymorphic_site_key(pc, i)))
            .collect();
        frame.bimorphic_reopen = Some(BimorphicReopen {
            pc,
            instr_len,
            pending,
            mem: mem_before,
            args,
            receiver,
            saved_committed_bases,
            saved_call_committed,
        });
        Some(next)
    }

    /// Round 14 wave 2 (lane guard, GS-1): body B of a bimorphic splice has
    /// just closed ([`Self::finish_multi_return_splice`] built its join and
    /// pushed its value): open body A on the first test's hit edge, with that
    /// join as its first exit edge, instead of resuming the caller. Returns
    /// the pc to continue the walk at (body A's first), `None` to refuse.
    pub(in crate::ir) fn reopen_bimorphic_first_body(
        &mut self,
        mut reopen: BimorphicReopen,
        returns_value: bool,
        // JIT round 19 wave 2 (lane splice2, SPLICE19-4): the closed frame's
        // nested miss call (`SpliceFrame::miss_call`), handed on to the
        // reopened frame so the outermost join records it.
        miss_call: Option<NodeId>,
    ) -> Option<usize> {
        // Round 16 (lane mega): the next body out, innermost test first; the
        // site's own first body is the last one popped.
        let (hit, class_id, key) = reopen.pending.pop()?;
        let join_ctrl = self.ctrl_opt()?;
        let join_mem = self.mem;
        let join_value = if returns_value {
            self.pop_opt()?
        } else {
            NO_NODE
        };
        // The closed body's replay fences describe a path this body is never
        // on (the hit edges are disjoint), and no caller instruction is walked
        // between the two bodies to clear them the way the walk does after a
        // splice.
        //
        // JIT round 19 wave 2 (lane splice2, SPLICE19-4): inside a splice the
        // enclosing body's fences raised BEFORE the `invoke` are on this
        // body's path too, so they are restored rather than cleared.
        if self.splice.is_empty() {
            self.splice_committed_bases.clear();
            self.splice_call_committed = false;
        } else {
            self.splice_committed_bases = reopen.saved_committed_bases.clone();
            self.splice_call_committed = reopen.saved_call_committed;
        }
        self.stack.extend(reopen.args.iter().copied());
        self.ctrl = hit;
        self.mem = reopen.mem;
        // A body other than the site's own is swapped in at `pc` for the one
        // `begin_splice` that reads it, as the innermost body was.
        let own_site = if key != reopen.pc {
            let site = self.inline_sites.get(&key)?.clone();
            Some(self.inline_sites.insert(reopen.pc, site))
        } else {
            None
        };
        self.guarded_splice_pending = Some((reopen.pc, reopen.receiver));
        let next = self.begin_splice(reopen.pc, reopen.instr_len);
        self.guarded_splice_pending = None;
        if let Some(own) = own_site {
            match own {
                Some(site) => {
                    self.inline_sites.insert(reopen.pc, site);
                }
                None => {
                    self.inline_sites.remove(&reopen.pc);
                }
            }
        }
        let next = next?;
        let frame = self.splice.last_mut()?;
        frame.multi_return = true;
        frame.receiver_exact_class = class_id;
        frame.miss_call = miss_call;
        frame.exits.push((join_ctrl, join_mem, join_value));
        if !reopen.pending.is_empty() {
            // Another body waits for this one's join.
            frame.bimorphic_reopen = Some(reopen);
        }
        Some(next)
    }

    /// The class test of a ONE-class guarded splice (the call-miss form,
    /// [`Self::begin_guarded_receiver_splice`], and the trap-miss form,
    /// [`Self::begin_trap_guarded_receiver_splice`]): `ExactClassIs(r, C)`,
    /// or, under `CRATONVM_JIT_IR_CLASS_WORD_SINGLE_TEST`
    /// ([`ir_class_word_single_test_enabled`], JIT round 19 wave 2, lane
    /// osr2, OSR19-2), `Cmp(Eq, ClassWordOf(r), Const(C as i32))`, the
    /// class-word chain's per-class compare. Same answer for every `r`: `C` is
    /// non-zero (checked here; a zero id keeps `ExactClassIs`), and
    /// `ClassWordOf` answers `0` for a null or an array, so the compare is `1`
    /// exactly when `ExactClassIs` is. What it buys is GVN: every site testing
    /// one receiver (two guarded sites on `p`, a one-class site next to a
    /// bimorphic chain on the same `p` under `CRATONVM_JIT_IR_CLASS_WORD_CHAIN`)
    /// shares one header read, and each compare fuses into its `If` / `Guard`
    /// by the ordinary compare fusion. Nothing downstream keys on the op:
    /// `trap_exact_facts` is keyed by the receiver NODE, `begin_splice`'s
    /// null-guard skip reads the guarded receiver, not the test, and the
    /// lowering's `op_guard_is_exact` only admits `Cmp(Ne, x, 0)`.
    pub(in crate::ir) fn one_class_receiver_test(
        &mut self,
        receiver: NodeId,
        class_id: u32,
        pc: usize,
    ) -> NodeId {
        // `CRATONVM_FROZEN_PAGES`: a frozen object of exactly this class
        // passes without a test.
        if class_id != 0 && self.frozen_class_of(receiver) == Some(class_id) {
            return self.iconst(1);
        }
        if class_id != 0 && ir_class_word_single_test_enabled() {
            let word = self.add_data(Op::ClassWordOf, IrType::Int, vec![receiver], pc);
            // Cast: the id's bits, read as the `int` the class word is.
            let id = self.iconst(i64::from(class_id as i32));
            return self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![word, id], pc);
        }
        self.add_data(
            Op::ExactClassIs { class_id },
            IrType::Int,
            vec![receiver],
            pc,
        )
    }

    /// Round 14 wave 2 (lane guard, CH5-5 of `jit-r13-chain5-proposals-RETIRED-20260929.md`):
    /// the guarded splice with a DEOPT as its miss edge, returning the
    /// combined-buffer pc to continue the walk at (`None` refuses the build).
    ///
    /// ```text
    /// Guard(ExactClassIs(receiver, C))      // deopt at this `invoke`, REEXECUTE
    /// the body resolved for C, spliced
    /// ```
    ///
    /// There is no join: the receiver is exactly `C` on every path past the
    /// guard, so the node is recorded in [`Self::trap_exact_facts`] and every
    /// later exact site on it (the `m.get(k)` after `m.put(k, v)` on a
    /// field-held map) is spliced with no test, until the next join of the
    /// compiling method's own code clears the fact (the proof is dominance,
    /// never node identity alone: the same node reaches other paths too).
    ///
    /// # Why it is sound
    ///
    /// * The guard is the ordinary `Op::Guard` deopt at this bci, as
    ///   `enter_receiver_monitor`'s class guard and `begin_splice`'s null guard
    ///   are: it fires before any of the body has run, resumes the interpreter
    ///   at this `invoke` from the snapshot taken at the top of this bci
    ///   (checked by [`Self::guarded_receiver_splice_plan`]), and the
    ///   interpreter dispatches the call itself, so a null raises its NPE there
    ///   and a third class runs its own method. The plan is outermost-only, so
    ///   no replay fence is involved.
    /// * A trap that keeps firing is charged at `(bci, UncommonTrap)`, and the
    ///   planner consults the de-spec registry at that key before registering
    ///   the site ([`Self::add_guarded_trap_site`]), so the recompile takes the
    ///   call edge.
    pub(in crate::ir) fn begin_trap_guarded_receiver_splice(
        &mut self,
        pc: usize,
        instr_len: usize,
        receiver: NodeId,
        class_id: u32,
    ) -> Option<usize> {
        let ctrl = self.ctrl_opt()?;
        // Round nio20 wave 3 (lane L4): the fact below is cleared only at the
        // joins of the compiling method's OWN code (`build`'s walk; a spliced
        // body's merges and `finish_multi_return_splice` never clear it). That
        // is the dominance argument for an OUTERMOST trap, whose guard precedes
        // every merge of the body it opens. A NESTED trap (`nested_trap_guard_plan`,
        // inside an open splice) may sit on one arm of the enclosing body: its
        // fact would outlive that arm's merge, and a later exact site on the
        // same node (a parameter or local held across the merge) would splice
        // unguarded, and with no null check (`begin_splice`), for a receiver
        // the guard never saw. The nested hit edge still proves its own body's
        // receiver through the splice frame (`guarded_splice_pending`).
        if self.sb_chain_active.is_some() {
            return None;
        }
        let records_fact = self.splice.is_empty();
        let exact = self.one_class_receiver_test(receiver, class_id, pc);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, exact],
            Some(pc),
        );
        self.guarded_splice_pending = Some((pc, receiver));
        let next = self.begin_splice(pc, instr_len);
        self.guarded_splice_pending = None;
        let next = next?;
        if records_fact {
            self.trap_exact_facts.insert(receiver, class_id);
        }
        Some(next)
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
    pub(in crate::ir) fn begin_splice(&mut self, pc: usize, instr_len: usize) -> Option<usize> {
        if self.sb_chain_active.is_some() {
            return None;
        }
        // nio21 wave 4 (lane L2): no body is spliced at an `invoke` inside a
        // covered `try` range of the enclosing body: its throws would leave
        // through the outermost `invoke` and skip that body's `catch`
        // (`lib.rs` keeps such nested rows as calls, so this is a backstop).
        // j27 wave 7 (lane T1L1, CF23-1,
        // `CRATONVM_JIT_IR_SPLICE_COVERED_NESTED_GUARDED`): or any planned body,
        // whose every instruction is then checked as a covered one
        // (`enter_covered_nested_instruction`).
        if self.pc_in_splice_handler_cover(pc)
            && !self.covered_splice_cannot_raise(pc)
            && !ir_splice_covered_nested_guarded_enabled()
        {
            return None;
        }
        // Round 12 wave 7 (lane iropt6): the hit edge of a guarded splice
        // (`begin_guarded_receiver_splice`), whose `ExactClassIs` test is the
        // proof the check below asks for. Taken, so it serves this call only.
        let guarded_receiver = self
            .guarded_splice_pending
            .take()
            .filter(|&(p, _)| p == pc)
            .map(|(_, r)| r);
        // An exact-receiver site reaches here only through
        // `exact_receiver_splice_admits`. Asked again so that no other arm can
        // splice one unproven: refusing the build is the safe answer there.
        if let (Some(&class_id), None) = (self.exact_receiver_sites.get(&pc), guarded_receiver) {
            if !ir_exact_receiver_splice_enabled()
                || self.exact_receiver_proof(pc, class_id).is_none()
            {
                return None;
            }
        }
        // Round 14 wave 2 (lane syncsplice, SR-1): a synchronized body is
        // spliced only WITH its monitor pair. The invoke arms ask
        // `sync_splice_site_admits` first and keep the call otherwise, so a
        // refusal here is a route that did not ask (a guarded hit edge never
        // carries a synchronized body): refuse the build rather than splice
        // one without its lock.
        let sync_plan = if self.sync_splice_sites.contains_key(&pc) {
            if guarded_receiver.is_some() {
                return None;
            }
            Some(self.sync_splice_plan(pc)?)
        } else {
            None
        };
        // Round 14 wave 6 (lane sync6, SS8-1 a): a same-receiver synchronized
        // body nested in a window, spliced with no pair only when its receiver
        // is proven the window's monitor (checked once the arguments are
        // popped, below). Any other route to such a pc refuses the build.
        let sync_held = self.sync_splice_held_sites.contains(&pc);
        if sync_held && (guarded_receiver.is_some() || sync_plan.is_some()) {
            return None;
        }
        let site = self.inline_sites.get(&pc)?;
        let base = site.base;
        // nio21 wave 4 (lane L2): a body with admitted handler rows only
        // where this compile may cover them (`set_splice_handler_edges`).
        if !self.splice_handler_edges && self.splice_handler_rows.contains_key(&base) {
            return None;
        }
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
        let ctor_callee = site.method_key.contains(".<init>:");
        // Round 13 wave 3 (lane framestate): this scope's identity in a
        // `SpliceScopeChain`. Cloned only when the chain snapshots (or, since
        // wave 47, the post-call chain snapshots) are on.
        let scope_key = if self.splice_frame_states || self.splice_call_states {
            site.method_key.clone()
        } else {
            String::new()
        };
        let site_code_len = site.code_len;
        let site_class_id = site.class_id;
        // JIT round 23 wave 1, lane L2 (INLINE22-4): a lambda SAM forwarder is
        // spliced only as the compiling method's own guarded site (the VM
        // resolver answers one only for the outermost profile route): its
        // frameless scope resumes a trap at the outermost `invoke`, which is
        // the forwarder's own only at depth 0. Refusing the build here costs
        // the site (W5-2 rebuild), never soundness.
        let frameless = ir_splice_body_is_lambda_forwarder(site.class_id);
        // JIT round 23 wave 2 (stage 2): a forwarder making more than one call
        // -- spliced or kept, its conversions -- is opaque; see
        // `SpliceFrame::opaque`.
        // Wave 3: and a constructor-reference forwarder (`new C; dup; ..;
        // invokespecial <init>`), whose result is the allocation, not the
        // call's: its frame cannot be skipped either.
        let opaque = frameless
            && (self
                .inline_sites
                .keys()
                .map(|&k| inline_site_caller_pc(k))
                .chain(self.invoke_info.keys().copied())
                .filter(|pc| (base..end).contains(pc))
                .count()
                > 1
                || self.new_info.keys().any(|pc| (base..end).contains(pc)));
        // JIT round 23 wave 2d (L2W1-3): the receiver must be proven the proxy
        // -- the guarded hit edge, or an exact-receiver row whose proof passed
        // above -- and a forwarder nested inside a splice must be transparent
        // (a trap in it resumes at the ENCLOSING body's `invoke`,
        // `IrBuilder::forwarder_reexecute_chain`, which an opaque forwarder's
        // conversions would skip) and not directly inside another forwarder.
        // JIT round 23 wave 6: a CHA watermark site never takes a frameless
        // forwarder (its proof is about a proxy class, which the test misses).
        // j27 wave 5 (lane PROXY): nor a class-set site (`cha_guarded_site`).
        if frameless && self.cha_guarded_site(pc) {
            return None;
        }
        if frameless
            && (!(guarded_receiver.is_some() || self.exact_receiver_sites.contains_key(&pc))
                || (!self.splice.is_empty()
                    && (opaque || self.splice.last().is_some_and(|f| f.frameless))))
        {
            return None;
        }
        if opaque && !self.graph.opaque_forwarder_bases.contains(&base) {
            self.graph.opaque_forwarder_bases.push(base);
        }

        let sun_unsafe_frames = usize::from(
            site.method_key.starts_with("sun/misc/Unsafe.")
                || self.splice.iter().any(|f| {
                    self.inline_sites
                        .get(&f.invoke_pc)
                        .is_some_and(|s| s.method_key.starts_with("sun/misc/Unsafe."))
                }),
        ) * 2;
        if self.splice.len() >= self.max_splice_depth + sun_unsafe_frames {
            return None;
        }
        // Round rx27 wave 4 (`CRATONVM_JIT_IR_CHAIN_DEEP_SPINE`): a frame past
        // the default cap, which only the deep spine opens.
        if self.splice.len() >= MAX_IR_SPLICE_DEPTH && crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] chain-deep-spine splice at pc={pc} depth={} (past the \
                 {MAX_IR_SPLICE_DEPTH}-frame default cap, under {})",
                self.splice.len() + 1,
                self.max_splice_depth,
            );
        }
        if arg_local_slots.len() != num_args || self.stack.len() < num_args {
            return None;
        }

        // Deepest-first on the abstract stack, so `split_off` already leaves
        // them in source order — argument 0 (the receiver, for an instance
        // callee) first.
        let args = self.stack.split_off(self.stack.len() - num_args);
        // JIT round 23 wave 2d: a nested forwarder's operands, for the
        // re-execute chain of a trap inside it.
        let reexec_args = if frameless && !self.splice.is_empty() {
            args.clone()
        } else {
            Vec::new()
        };
        let mut callee_locals = vec![NO_NODE; max_locals];
        for (arg, &slot) in args.iter().zip(arg_local_slots.iter()) {
            let slot = slot as usize;
            if slot >= callee_locals.len() {
                return None;
            }
            callee_locals[slot] = *arg;
        }
        // SS8-1 a (round 14 wave 6): the proof. Refusing here refuses the
        // build, whose rebuild drops the ENCLOSING site (its call stays).
        if sync_held
            && !(receiver_is_arg0
                && self.sync_splice_held_site_proven(pc, args.first().copied().unwrap_or(NO_NODE)))
        {
            return None;
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
        // `docs/feature-designs/jit-ir-optimizations.md`.
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
        // A guarded hit edge (round 12 wave 7): `ExactClassIs` answered `1` for
        // exactly this receiver, so it is not null and needs no guard.
        if let Some(g) = guarded_receiver {
            if !receiver_is_arg0 || args.first() != Some(&g) {
                return None;
            }
        }
        // Round 14 wave 2 (lane guard, CH5-5): nor one a dominating miss-edge
        // trap's `ExactClassIs` passed (`trap_exact_facts`), which is 0 for null.
        // Nor a same-receiver synchronized body (SS8-1 a, proven above): its
        // receiver is the object the open window locked, so it is not null --
        // and a guard would be a trap inside the window, which it refuses.
        let receiver_needs_null_check = guarded_receiver.is_none()
            && !sync_held
            && receiver_is_arg0
            && args.first().is_some_and(|&r| {
                !self.trap_exact_facts.contains_key(&r)
                    && self
                        .graph
                        .nodes
                        .get(r as usize)
                        .is_none_or(|n| !crate::ir_check_elim::definitely_non_null(&n.op))
            });
        if receiver_needs_null_check {
            let receiver = *args.first()?;
            let ctrl = self.ctrl_opt()?;
            if self.splice.is_empty() && !self.has_frame_state_at(pc) {
                return None;
            }
            // A guard inside an OPEN splice resolves its frame state through
            // `resume_bci` to the OUTERMOST invoke, so taking it re-executes
            // that whole call — including any spliced prefix that already ran.
            // `spliced_bodies_pure` is the property that makes that harmless
            // and it is the same clause `trap_replay_is_safe` asks; when it does
            // not hold, raise the fence that refuses the graph, exactly as
            // `add_div_zero_guard` and `plant_uncommon_trap` do.
            // Round 13 wave 3 (W5-3 stage 3): unless the guard resumes through
            // the caller chain recorded at this `invoke`, which re-runs nothing.
            if !self.splice.is_empty()
                && !self.spliced_bodies_pure
                && !self.relax_fence_by_chain(pc)
            {
                self.splice_guard_seen = true;
                self.splice_guard_pc.get_or_insert(pc);
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

        // Round 14 wave 2 (lane syncsplice, SR-1): take the callee's monitor
        // AFTER the receiver's null guard (JVMS 6.5 raises that NPE at the
        // invoke, before any monitor exists) and before the body's first
        // instruction. Both nodes carry the invoke's pc, so the enter's own
        // exits resume or raise at the whole `invoke` with nothing held. Not
        // pushed on `self.monitors`: no frame state names it (see
        // `SyncSpliceWindow`). The exit is `end_splice`'s.
        let sync_monitor = match sync_plan {
            None => None,
            Some(plan) => {
                let ctrl = self.ctrl_opt()?;
                let (monitor, static_class_id) = match plan {
                    SyncMonitorPlan::Instance => (*args.first()?, 0),
                    SyncMonitorPlan::Static {
                        holder_class_id,
                        cp_idx,
                        slot_addr,
                        class_id,
                    } => {
                        let m = self.graph.add(
                            Op::ConstClass {
                                holder_class_id,
                                cp_idx,
                                slot_addr,
                            },
                            IrType::Ref,
                            vec![ctrl, self.mem],
                            Some(pc),
                        );
                        self.mem = m;
                        (m, class_id)
                    }
                };
                if monitor == NO_NODE || (static_class_id == 0 && !receiver_is_arg0) {
                    return None;
                }
                let enter = self.graph.add(
                    Op::MonitorEnter,
                    IrType::Memory,
                    vec![ctrl, self.mem, monitor],
                    Some(pc),
                );
                self.mem = enter;
                Some(SyncSpliceWindow {
                    enter,
                    exit: NO_NODE,
                    monitor,
                    static_class_id,
                })
            }
        };

        // A constructor spliced onto an object this method allocated outside
        // every splice -- see `splice_store_is_ctor_receiver`. A super-
        // constructor spliced inside it passes the same receiver on.
        //
        // Any `Op::New` receiver qualifies, inside a splice or not. One a
        // spliced body allocated is splice-local already, but storing a
        // non-local value into it (the subtree a leaf call returned) TAINTS it,
        // and the taint -- which exists for reads THROUGH the object -- then
        // refused the constructor's next store into the object itself. A replay
        // of such a splice re-runs the enclosing call, which allocates a fresh
        // object, so a store to this one is harmless for the same reason.
        //
        // Scoped to the constructor's own frame (round 11 wave 15, lane `ir`):
        // the frame that first adds a receiver removes it when it closes, so a
        // later splice on the finished -- possibly published -- object is
        // judged by the ordinary rules. See `SpliceFrame::ctor_receiver`.
        let mut ctor_receiver = None;
        if receiver_is_arg0 && ctor_callee {
            if let Some(&recv) = args.first() {
                let is_new = self
                    .graph
                    .nodes
                    .get(recv as usize)
                    .is_some_and(|n| matches!(n.op, Op::New { .. }));
                if is_new && self.splice_ctor_receivers.insert(recv) {
                    ctor_receiver = Some(recv);
                }
            }
        }

        let receiver = if receiver_is_arg0 {
            args.first().copied().unwrap_or(NO_NODE)
        } else {
            NO_NODE
        };
        // A null fact of the enclosing body is not carried into this one (nor
        // back out of it): `lib.rs`'s planner judges each body from an empty
        // set, and it must never assume a fact this builder does not hold.
        self.splice_null_facts.clear();
        let param_end = arg_local_slots
            .iter()
            .map(|&s| s as usize + 1)
            .max()
            .unwrap_or(0)
            .min(64)
            .min(callee_locals.len());
        let param_nodes: Vec<NodeId> = callee_locals[..param_end].to_vec();
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
        // Round 13 wave 10 (lane iropt8, I8-2): a guarded hit edge's receiver
        // is exactly the site's class inside this body (and nowhere else).
        // JIT round 23 wave 6: a CHA watermark site's hit edge proves only
        // "some class the analysis saw", never one exact class.
        let receiver_exact_class = match guarded_receiver {
            // j27 wave 5 (lane PROXY): nor a class-set site's, one of several.
            Some(_) if self.cha_guarded_site(pc) => 0,
            Some(_) => self.exact_receiver_sites.get(&pc).copied().unwrap_or(0),
            None => 0,
        };
        self.splice.push(SpliceFrame {
            class_id: site_class_id,
            return_pc: pc + instr_len,
            end,
            saved_locals,
            saved_stack,
            returns_value,
            return_narrow,
            multi_return: multi_return || !ranges.is_empty(),
            bimorphic_reopen: None,
            exits: Vec::new(),
            exit_bci: pc,
            ranges,
            next_range: 1,
            range_end,
            ctor_receiver,
            receiver,
            arg0: args.first().copied().unwrap_or(NO_NODE),
            param_nodes,
            receiver_exact_class,
            invoke_pc: pc,
            base,
            method_key: scope_key,
            code_len: site_code_len,
            max_locals,
            sync_monitor,
            miss_call: None,
            frameless,
            opaque,
            reexec_args,
            caller_mirror_slot: self
                .splice_caller_frames
                .get(&base)
                .map_or(0, |&(slot, _)| slot),
            security_walk_ignored: self
                .splice_caller_frames
                .get(&base)
                .is_some_and(|&(_, ignored)| ignored),
        });
        // A synchronized body's exit goes on its ONE return edge
        // (`end_splice`), or, for a pc-order body with several returns, after
        // their join (`finish_multi_return_splice`, SS8-1 b);
        // `sync_splice_plan` admitted no other shape.
        if sync_monitor.is_some()
            && self.splice.last().is_some_and(|f| {
                f.multi_return && (!f.ranges.is_empty() || !ir_sync_splice_multi_return_enabled())
            })
        {
            return None;
        }
        if sync_held {
            // SS8-1 a: counted when opened; the enclosing window's exit check
            // still judges every node this body builds.
            note_sync_splice_census(SYNC_CENSUS_NESTED_SAME_RECEIVER);
            if crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] ir-sync-splice nested-same-receiver at pc={pc} (no pair; \
                     the enclosing window holds the monitor)"
                );
            }
        }
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
    ///    which case the base is TAINTED and locality stops (since round 11
    ///    wave 15: for every later read of the splice, because a second node
    ///    can name the same object -- see [`Self::note_splice_local_load`]).
    ///    Without that
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
    /// That is fenced separately and positionally, like a committed store
    /// (round 11 wave 16, lane `replay`): see [`Self::splice_call_committed`].
    ///
    /// A store that is NOT local is still admitted when nothing after it in
    /// the outermost splice can trap (round 11 wave 11, lane `hashmap`): then
    /// no re-execution can follow it at all. See
    /// [`Self::admits_after_committed_splice_store`]. That is what a
    /// constructor spliced onto the caller's own `new` needs:
    /// `new HashMap(); invokespecial <init>` splices `this.loadFactor = 0.75f;
    /// return`, and `this` is the caller's allocation, not the splice's. The
    /// local-only rule refused the whole method (`CratonBench.hashMapPutGet`,
    /// `IrBuilder::build refused at ir.rs:11491 (bytecode pc 85)`).
    pub(in crate::ir) fn splice_store_is_local(&self, base: NodeId) -> bool {
        self.splice_local.contains(&base) && !self.splice_tainted.contains(&base)
    }

    /// May the walk build the instruction at `pc` after the outermost open
    /// splice has committed a store into an object it did not allocate, or
    /// has made a call the resolver did not splice
    /// ([`Self::splice_call_committed`], round 11 wave 16)?
    ///
    /// Every trap inside a splice (a null check, a deopt, a site trap, a
    /// callee's exception) resumes at the CALLER's `invoke` and re-runs the
    /// whole call, so a store committed before it would be committed twice and
    /// the second run would read what the first one wrote. The rule is
    /// therefore positional: once such a store is committed, only instructions
    /// that cannot trap may follow until the walk is back in the caller.
    ///
    /// * constants, local loads and stores, `pop`/`dup`, and arithmetic other
    ///   than the dividing forms (whose zero test is a trap);
    /// * an `ldc`/`ldc2_w` only when it resolved to an immediate (a String or
    ///   Class constant is a helper call);
    /// * a `getfield`/`putfield` only on a base that cannot be null: one an
    ///   earlier committed store already wrote through (its null check ran
    ///   BEFORE its write), or an allocation (`Op::New`/`Op::NewArray`, which
    ///   includes a constructor receiver) -- never a value merely READ from
    ///   one, which may be `null`;
    /// * the returns, which end the splice.
    ///
    /// Everything else (an invoke, an allocation, a branch, an array access, a
    /// cast, a monitor) refuses the method, exactly as the store alone used to.
    /// A branch is refused on purpose: in walk order a loop body can precede
    /// the store while executing after it, and every loop needs a branch that
    /// the walk reaches after the store.
    ///
    /// Round 12 wave 5 (lane iropt4): after a committed CALL (and no committed
    /// store), three relaxations admit a forward branch, an access through a
    /// base proven non-null by a dominating null test or by being an open
    /// splice's receiver, and a plain call on such a receiver. Each has its own
    /// switch and its proof on [`SpliceFenceRelax`].
    ///
    /// Round 16 (lane sync, SY16-2): under
    /// [`ir_splice_store_fence_forward_branch_enabled`] a fence a committed
    /// STORE raised admits relaxation 1 (a forward branch) too, and nothing
    /// else of the three. Relaxation 1's proof never used what raised the
    /// fence: walk order and pc order agree inside pc-walked splices, so every
    /// instruction that can execute after the store is walked after it and
    /// fenced, and the branch itself builds no trap (no profile pruning inside
    /// a splice, no poll on a forward edge). A join only re-reads the frames
    /// its edges carried, and a base that differs across them becomes a φ,
    /// which is no committed base, so an access through it still refuses.
    pub(in crate::ir) fn admits_after_committed_splice_store(
        &self,
        code: &[u8],
        pc: usize,
    ) -> bool {
        let Some(&op) = code.get(pc) else {
            return false;
        };
        // A fence a committed store raised keeps the round-11 rule, save
        // relaxation 1 under SY16-2.
        let relax = if self.splice_committed_bases.is_empty() {
            self.splice_fence_relax
        } else if self.splice_store_fence_forward_branch {
            SpliceFenceRelax {
                forward_branch: true,
                ..SpliceFenceRelax::NONE
            }
        } else {
            SpliceFenceRelax::NONE
        };
        // NOT `splice_store_is_local` (round 11 wave 15, lane `ir`): that set
        // also holds every field/element READ through a splice-local base, and
        // a field of an object the splice allocated is `null` until something
        // stores into it. `w = new W(); caller.f = 1; w.next.x` then took the
        // null check on `w.next` AFTER the committed store, and the replay
        // committed it twice. Only an allocation itself is non-null, and
        // `definitely_non_null` is the predicate that says so (a constructor
        // receiver is an `Op::New`, so it is covered too).
        let proven_base = |depth: usize| self.fence_proven_non_null(depth, relax);
        match op {
            // nop, aconst_null, the iconst/lconst/fconst/dconst family, bipush, sipush
            0x00..=0x11 => true,
            // JIT r20 wave 19 (lane zLdc): or a String / Class constant whose
            // GC-maintained slot holds the resolved object (a never-cleared
            // root, so the helper miss arm never runs and nothing can trap) --
            // the planner's `ir_splice_opcode_cannot_trap` already admits it.
            0x12 | 0x13 => {
                self.ldc_info.contains_key(&pc)
                    || (ir_splice_ref_ldc_enabled() && self.ldc_ref_has_slot(pc))
            }
            0x14 => self.ldc2w_info.contains_key(&pc),
            // iload .. aload_3
            0x15..=0x2d => true,
            // istore .. astore_3: a local store cannot trap.
            0x36..=0x4e => true,
            // pop, dup
            0x57 | 0x59 => true,
            // add/sub/mul, then neg/shifts/and/or/xor/iinc/conversions/compares.
            // 0x6c..=0x73 (div/rem) are the gap.
            0x60..=0x6b | 0x74..=0x98 => true,
            // getfield: the base is on top of the stack.
            0xb4 => proven_base(0),
            // putfield: the value is on top, the base under it.
            0xb5 => proven_base(1),
            // JIT round 21 (lane staticfwd, FG16-3): a static access that
            // cannot trap (`splice_static_access_cannot_trap`).
            0xb2 | 0xb3 => self.splice_static_access_cannot_trap(pc),
            // ireturn .. return
            0xac..=0xb1 => true,
            // nio21 wave 11, lane L3: a spliced constant-word lambda site
            // reads a published singleton -- it cannot trap, and re-running it
            // in a replay answers the same instance.
            0xba => self.spliced_constant_indy_at(pc),
            // Round 12 wave 5, relaxation 1: if*, if_icmp*, if_acmp*, goto,
            // ifnull, ifnonnull, goto_w -- forward only.
            0x99..=0xa7 | 0xc6..=0xc8 => {
                relax.forward_branch && self.splice_branch_is_forward(code, pc)
            }
            // Relaxation 3: a plain call on a receiver proven non-null.
            0xb6 | 0xb7 => {
                relax.guarded_call
                    && !self.inline_sites.contains_key(&pc)
                    && !self.exact_receiver_sites.contains_key(&pc)
                    && !self.sync_direct_calls.contains_key(&pc)
                    && !self.scalar_intrinsics.contains_key(&pc)
                    // JIT round 16 wave 7 (lane callbind): a SPLICED unbox row
                    // keeps its call row, and under a raised fence with no
                    // chain snapshot at `pc` `try_emit_unbox_intrinsic`
                    // declines, so the site builds as the plain call admitted
                    // here. An own-site unbox row has no `invoke_info` row.
                    && (!self.unbox_intrinsics.contains_key(&pc)
                        || self.invoke_info.contains_key(&pc))
                    && self.invoke_info.get(&pc).is_some_and(|&(_, num_args, _)| {
                        num_args >= 1 && proven_base(num_args - 1)
                    })
            }
            _ => false,
        }
    }

    /// JIT round 21 (lane staticfwd, FG16-3): can the `getstatic` /
    /// `putstatic` at `pc` not trap, so the replay fence may admit it after a
    /// commit? Only for a non-zero class that is either one the compile's
    /// resolver reported initialised ([`Self::static_initialized_classes`])
    /// or one whose `putstatic` this splice has already committed: the
    /// resolver admitted that write only for a class initialised at resolve
    /// time (monotone), so neither access route runs `<clinit>`, and an
    /// initialised class's helper never answers the sentinel. A volatile
    /// `putstatic` still bails in its own arm. Off with
    /// [`ir_splice_putstatic_enabled`].
    pub(in crate::ir) fn splice_static_access_cannot_trap(&self, pc: usize) -> bool {
        if !ir_splice_putstatic_enabled() {
            return false;
        }
        let Some(&(class_id, _, _, _)) = self.static_field_info.get(&pc) else {
            return false;
        };
        class_id != 0
            && (self.static_initialized_classes.contains(&class_id)
                || self.splice_committed_bases.iter().any(|&b| {
                    self.graph.nodes.get(b as usize).is_some_and(
                        |n| matches!(n.op, Op::StoreStatic { class_id: c, .. } if c == class_id),
                    )
                }))
    }

    /// j27 wave 7 (lane T1STATICFWD, STATICFWD21-1,
    /// `CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_REPLAY`): may the spliced
    /// `putstatic` the walk is at skip the commit, because a replay of the
    /// OUTERMOST open body re-runs it harmlessly
    /// ([`crate::ir_splice_static_replay::body_replays_idempotently`], the
    /// planner's question over the same bytes and rows)? Only directly in a
    /// depth-1 body that holds no monitor window and is no lambda forwarder.
    pub(in crate::ir) fn splice_static_store_replays_idempotently(&self, code: &[u8]) -> bool {
        let [frame] = self.splice.as_slice() else {
            return false;
        };
        if frame.sync_monitor.is_some()
            || frame.frameless
            || !crate::ir_splice_static_replay::enabled()
        {
            return false;
        }
        let base = frame.base;
        let Some(body) = code.get(base..base.saturating_add(frame.code_len)) else {
            return false;
        };
        crate::ir_splice_static_replay::body_replays_idempotently(
            body,
            |p| self.static_field_info.get(&(base + p)).copied(),
            |p| self.volatile_field_pcs.contains(&(base + p)),
        )
    }

    /// Is the value `depth` entries below the top of the operand stack proven
    /// non-null for the committed-splice fence
    /// ([`Self::admits_after_committed_splice_store`])?
    ///
    /// Always: a base an earlier committed store wrote through (its null check
    /// ran before its write), or an allocation (`definitely_non_null`). With
    /// `relax.null_facts` (round 12 wave 5, lane iropt4) also: the receiver of
    /// an open splice, and a value a dominating null test proved
    /// ([`Self::splice_null_facts`]). See [`SpliceFenceRelax`] for why each
    /// holds.
    pub(in crate::ir) fn fence_proven_non_null(
        &self,
        depth: usize,
        relax: SpliceFenceRelax,
    ) -> bool {
        let Some(&b) = self
            .stack
            .len()
            .checked_sub(depth + 1)
            .and_then(|i| self.stack.get(i))
        else {
            return false;
        };
        if b == NO_NODE {
            return false;
        }
        self.splice_committed_bases.contains(&b)
            || self
                .graph
                .nodes
                .get(b as usize)
                .is_some_and(|n| crate::ir_check_elim::definitely_non_null(&n.op))
            || (relax.null_facts
                && (self.splice_null_facts.contains(&b)
                    || self.splice.iter().any(|f| f.receiver == b)))
    }

    /// Does the branch at `pc` jump FORWARD, inside the innermost open splice,
    /// with every open splice walked in pc order? Relaxation 1 of
    /// [`SpliceFenceRelax`]: under those two conditions walk order and pc
    /// order agree, and only a backward branch can bring execution back to
    /// code walked before the fence went up.
    pub(in crate::ir) fn splice_branch_is_forward(&self, code: &[u8], pc: usize) -> bool {
        let Some(frame) = self.splice.last() else {
            return false;
        };
        if self.splice.iter().any(|f| !f.ranges.is_empty()) {
            return false;
        }
        crate::bytecode_analysis::offset_branch_target(code, pc)
            .is_some_and(|target| target > pc && target < frame.end)
    }

    /// A merge at `pc` was just activated: drop every null fact, then, inside
    /// a splice, start the one an `ifnonnull` taken edge carries when that edge
    /// is the merge's ONLY input (the region from here to the next merge is
    /// then dominated by it). A loop header never starts one: its back edge is
    /// not an input yet. Relaxation 2 of [`SpliceFenceRelax`].
    pub(in crate::ir) fn note_splice_merge_null_facts(&mut self, pc: usize) {
        self.splice_null_facts.clear();
        if self.splice.is_empty()
            || self.splice_edge_null_facts.is_empty()
            || self.loop_headers.contains(&pc)
        {
            return;
        }
        let Some(merge_id) = self.merges.get(&pc).map(|s| s.merge_id) else {
            return;
        };
        if self.ctrl != merge_id {
            return;
        }
        let inputs = self
            .graph
            .nodes
            .get(merge_id as usize)
            .map(|n| n.inputs.as_slice());
        let only_input = match inputs {
            Some(&[edge]) => edge,
            _ => return,
        };
        if let Some(&v) = self.splice_edge_null_facts.get(&only_input) {
            self.splice_null_facts.insert(v);
        }
    }

    /// May a store inside the open splice write to `base` because `base` is the
    /// receiver of a spliced constructor this method allocated itself?
    ///
    /// `new C(args)` compiles to `new C; dup; <args>; invokespecial C.<init>`,
    /// and when this method is the one doing the `new`, the object is created
    /// OUTSIDE every splice, so [`Self::splice_store_is_local`] does not know
    /// it and refused every field store of every inlined constructor. That
    /// kept any method that builds an object with arguments out of this tier
    /// (binary-trees' `bottomUpTree`).
    ///
    /// The re-execution argument [`Self::splice_store_is_local`] makes is what
    /// has to hold, and it does for this receiver, with one exception:
    ///
    /// * nothing but this frame can see the object. It is the `uninitialized`
    ///   result of a `new` (JVMS 4.10.1.9), not stored anywhere yet, so a
    ///   re-run of the constructor from the enclosing `invokespecial` finds the
    ///   same object, reachable from nothing else;
    /// * the constructor re-writes every field it had written, with the same
    ///   argument values, so the object ends up as a single run leaves it;
    /// * UNLESS the constructor read a field of `this` before writing it
    ///   (`this.n = this.n + 1`): the re-run would read the first run's value.
    ///   So a store to a receiver whose fields this splice has read is refused.
    pub(in crate::ir) fn splice_store_is_ctor_receiver(&self, base: NodeId) -> bool {
        self.splice_ctor_receivers.contains(&base)
            && !self.splice_ctor_receiver_read.contains(&base)
    }

    /// Record `id` as splice-local when it is one: called on every allocation
    /// and every field/element read the walk builds, and a no-op outside a
    /// splice.
    pub(in crate::ir) fn note_splice_local_alloc(&mut self, id: NodeId) {
        if !self.splice.is_empty() {
            self.splice_local.insert(id);
        }
    }

    /// Record a `Load`-shaped node as local when its base is.
    ///
    /// Only while NO splice-local object has been tainted (round 11 wave 15,
    /// lane `ir`). Taint is recorded per NODE, but it is a property of the
    /// OBJECT, and two nodes can name one object: `v = new V(); w.f = v;
    /// w.f.g = callerObj` taints the node `w.f`, not `v`, so `v.g` read back as
    /// local and `v.g.x = 1` was admitted as a store into a splice-made object
    /// -- it is the caller's. Loads the walk built before the first taint keep
    /// their locality: each read its value before any caller reference was
    /// stored anywhere local. (A loop cannot carry a local base round a back
    /// edge to a later iteration: the header φs every entry-live local, and a
    /// φ is never local.)
    pub(in crate::ir) fn note_splice_local_load(&mut self, id: NodeId, base: NodeId) {
        if !self.splice.is_empty()
            && self.splice_tainted.is_empty()
            && self.splice_store_is_local(base)
        {
            self.splice_local.insert(id);
        }
        if !self.splice.is_empty() && self.splice_ctor_receivers.contains(&base) {
            self.splice_ctor_receiver_read.insert(base);
        }
    }

    /// A FIELD read inside a splice through `base`, which is not itself an
    /// open constructor receiver but may be an ALIAS of one: a constructor
    /// that stores `this` somewhere and reads its own field back through that
    /// place (`h.c = this; this.n = h.c.n + 1`), or through a φ that merges
    /// `this` with something else. [`Self::note_splice_local_load`] marks a
    /// receiver read only by node identity, so the store after such a read
    /// was admitted as a receiver store, and a trap after it replayed the
    /// constructor over the first run's value (round 11 wave 15, lane `ir`).
    ///
    /// A base that provably names a DIFFERENT object marks nothing: another
    /// allocation, one of the compiled method's own parameters (they predate
    /// every allocation in it), `null` or another constant, a String/Class
    /// constant, or a splice-local
    /// read taken before any caller reference was stored into a local object
    /// (it holds `null` or a splice-made object, and a receiver this method
    /// allocated outside every splice is neither). Anything else -- a field
    /// read, a φ, a call result -- marks every open receiver that has ESCAPED
    /// ([`Self::ctor_receiver_escaped`]): one nothing has been handed can be
    /// named by no such value.
    pub(in crate::ir) fn note_splice_field_read_alias(&mut self, base: NodeId) {
        if self.splice.is_empty()
            || self.splice_ctor_receivers.is_empty()
            || self.splice_ctor_receivers.contains(&base)
            || self.splice_local.contains(&base)
        {
            return;
        }
        let distinct = self.graph.nodes.get(base as usize).is_some_and(|n| {
            matches!(
                n.op,
                Op::New { .. }
                    | Op::NewArray { .. }
                    | Op::Param(_)
                    | Op::Const(_)
                    | Op::ConstString { .. }
                    | Op::ConstClass { .. }
            )
        });
        if !distinct {
            self.mark_escaped_ctor_receivers_read();
        }
    }

    /// Has the open constructor receiver `recv` been handed to anything --
    /// used by any node other than as the BASE of a field read or write
    /// (a stored value, a call argument, a φ input, a compare, a cast)?
    ///
    /// Until it has, no field read, φ or call result can name it: before
    /// `<init>` the object is `uninitialized` and reachable from this frame
    /// alone (JVMS 4.10.1.9), and every node that could hand it on is built
    /// after the `Op::New`. The walk visits a splice in reverse post-order,
    /// so the escape that makes an alias read possible was built before the
    /// read on every path that executes both; a loop back edge cannot change
    /// that, because a read after the first write of a field reads the
    /// replay's own write. Linear in the nodes built since the allocation, and
    /// asked only while a constructor splice is open.
    pub(in crate::ir) fn ctor_receiver_escaped(&self, recv: NodeId) -> bool {
        self.graph
            .nodes
            .iter()
            .skip((recv as usize).saturating_add(1))
            .any(|n| {
                n.inputs.iter().enumerate().any(|(k, &input)| {
                    input == recv && !(k == 2 && matches!(n.op, Op::Load(_) | Op::Store(_)))
                })
            })
    }

    /// Mark every open constructor receiver that has escaped as READ -- see
    /// [`Self::note_splice_field_read_alias`] and [`Self::note_splice_call`].
    pub(in crate::ir) fn mark_escaped_ctor_receivers_read(&mut self) {
        let escaped: Vec<NodeId> = self
            .splice_ctor_receivers
            .iter()
            .copied()
            .filter(|&r| self.ctor_receiver_escaped(r))
            .collect();
        self.splice_ctor_receiver_read.extend(escaped);
    }

    /// An `Op::Call` built inside a splice (a callee the resolver did not
    /// splice): the builder cannot see what it reads or writes (round 11
    /// wave 15, lane `ir`).
    ///
    /// * It may store a caller reference into a splice-local object it is
    ///   handed (`helper(w, p)` doing `w.f = p`), which no `putfield` arm
    ///   here sees, so a splice-local argument taints -- and with it ends the
    ///   splice's load locality (see [`Self::note_splice_local_load`]).
    /// * It may read a field of an open constructor receiver it can reach
    ///   (`this.n = get(this) + 1`, or `register(this)` earlier), which a
    ///   replay of the constructor would read back changed. So every open
    ///   receiver that has escaped -- this call's own argument included, since
    ///   the call node is already built -- counts as read. One the callee
    ///   cannot reach does not (`this.v = Objects.requireNonNull(v)`).
    /// * It has RUN when anything after it executes, and a trap after it in
    ///   the same outermost splice re-executes the caller's `invoke`, which
    ///   runs it again (round 11 wave 16, lane `replay`,
    ///   `r11w15-ir-splice-replay-reruns-a-leaf-call`: `inc(); return a[i];`
    ///   counted twice on an out-of-bounds `i`). So it raises
    ///   [`Self::splice_call_committed`], and the walk-top fence then admits
    ///   only what cannot trap until the walk is back in the caller -- unless
    ///   it is the leaf self-call of an admitted self copy, whose replay
    ///   commits nothing observable ([`Self::is_self_copy_leaf_call`]).
    pub(in crate::ir) fn note_splice_call(&mut self, call: NodeId) {
        if self.splice.is_empty() {
            return;
        }
        let leaf = match self.graph.nodes.get(call as usize).map(|n| &n.op) {
            Some(&Op::Call { info_ptr }) => self.is_self_copy_leaf_call(info_ptr),
            _ => false,
        };
        if !leaf {
            self.splice_call_committed = true;
            // Interpreter round i1 wave 47, lane L3: the call's successor gets
            // a post-call chain snapshot ([`Self::push_post_call_splice_state`]).
            if self.splice_call_states {
                self.pending_post_call = self
                    .graph
                    .nodes
                    .get(call as usize)
                    .and_then(|n| n.bytecode_pc)
                    .map(|pc| (call, pc));
            }
        }
        if !self.splice_ctor_receivers.is_empty() {
            self.mark_escaped_ctor_receivers_read();
        }
        let local_args: Vec<NodeId> = self
            .graph
            .nodes
            .get(call as usize)
            .map(|n| {
                n.inputs
                    .iter()
                    .skip(2)
                    .copied()
                    .filter(|&a| {
                        self.splice_local.contains(&a)
                            && self
                                .graph
                                .nodes
                                .get(a as usize)
                                .is_some_and(|m| m.ty == IrType::Ref)
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.splice_tainted.extend(local_args);
    }

    /// Is a call built inside a splice with the `invoke_info` row whose first
    /// field is `info_ptr` the LEAF self-call of an admitted bounded self copy
    /// (round 11 wave 16, lane `replay`)? Such a call is exempt from
    /// [`Self::splice_call_committed`].
    ///
    /// The test is identity with one of the COMPILING method's own rows (a row
    /// at a pc inside no spliced body), and it is exact because of how
    /// `lib.rs`'s `append_ir_inline_site` fills a spliced pc's row:
    ///
    /// * every surviving call gets a `JitInvokeInfo` boxed fresh by
    ///   `intern_inline_invoke_targets`, whose address is no other row's;
    /// * the one exception is an `IrSelfSplice::Leaf`, which installs the
    ///   compiling method's own kind-4 row ([`Self::self_recursive_call_row`])
    ///   -- and only inside a body that is itself an admitted self copy (the
    ///   planner refuses a leaf anywhere else).
    ///
    /// Why the exemption is sound: `ir_self_copy_body_is_trap_free` or
    /// `ir_self_copy_body_replays_safely` admitted that copy, i.e. THIS method's
    /// bytecode, and the leaf runs this method. So a replay re-runs a
    /// computation whose only lasting effects are fresh allocations and writes
    /// into them -- the induction those predicates' docs spell out. Without the
    /// exemption `itemCheck`'s `getfield` after every leaf would refuse its
    /// whole optimizing compile and binary-trees would lose its recursive
    /// inline again
    /// (`r11w15-orch-recursive-inline-admission-drops-the-binary-trees-gain`).
    ///
    /// No `JitInvokeInfo` is dereferenced: builder fixtures bake fake
    /// addresses, and identity is all the argument needs.
    pub(in crate::ir) fn is_self_copy_leaf_call(&self, info_ptr: usize) -> bool {
        if info_ptr == 0 {
            return false;
        }
        let in_a_spliced_body = |pc: usize| {
            self.inline_sites
                .values()
                .any(|s| pc >= s.base && pc < s.base.saturating_add(s.code_len))
        };
        self.invoke_info
            .iter()
            .any(|(&pc, &(ptr, _, _))| ptr == info_ptr && !in_a_spliced_body(pc))
    }

    /// Close the constructor-receiver scope a splice frame opened -- see
    /// [`SpliceFrame::ctor_receiver`].
    pub(in crate::ir) fn release_ctor_receiver(&mut self, recv: Option<NodeId>) {
        if let Some(recv) = recv {
            self.splice_ctor_receivers.remove(&recv);
            self.splice_ctor_receiver_read.remove(&recv);
        }
    }

    /// Record that a caller-visible REFERENCE was stored into splice-local
    /// `base`, so locality stops propagating through reads (see
    /// [`Self::note_splice_local_load`]).
    ///
    /// A primitive, and the `null` constant, name no object and taint nothing
    /// (round 11 wave 15, lane `ir`): with taint now ending locality for the
    /// whole splice, an `int` field store would otherwise have cost every
    /// later read its locality for no reason.
    pub(in crate::ir) fn note_splice_store_value(&mut self, base: NodeId, value: NodeId) {
        if self.splice.is_empty() || self.splice_local.contains(&value) {
            return;
        }
        let names_no_object = self
            .graph
            .nodes
            .get(value as usize)
            .is_some_and(|n| n.ty != IrType::Ref || matches!(n.op, Op::Const(0)));
        if !names_no_object {
            self.splice_tainted.insert(base);
        }
    }

    /// Is there a frame state of the COMPILING method at `pc` -- a snapshot the
    /// walk pushed at the top of `pc` outside every splice?
    ///
    /// The question the trap- and guard-planting arms ask before they plant
    /// anything a deopt would resume from. A chain snapshot
    /// ([`SafepointSnapshot::is_splice_state`]) does not count: those arms
    /// resolve their frame at `resume_bci`, and before round 13 wave 3 no
    /// snapshot existed inside a splice at all, so this keeps every one of them
    /// answering exactly as it did. Searched newest-first: the walk pushes the
    /// snapshot for `pc` immediately before the opcode runs.
    pub(in crate::ir) fn has_frame_state_at(&self, pc: usize) -> bool {
        self.graph
            .safepoints
            .iter()
            .rev()
            .any(|sp| sp.bci == pc && !sp.is_splice_state())
    }

    /// May a replay fence be relaxed at `pc` because a trap there resumes
    /// through a caller chain (round 13 wave 3, W5-3 stage 3;
    /// [`ir_splice_chain_fences_enabled`])? Only inside a splice, only with the
    /// switch on, and only when the walk recorded the chain snapshot for `pc`
    /// itself -- it is the LAST snapshot, pushed at the top of `pc` just before
    /// the opcode (and the fence) ran. A pc past the snapshot budget has none,
    /// and keeps the fence.
    pub(in crate::ir) fn chain_snapshot_is_last_at(&self, pc: usize) -> bool {
        // JIT round 24 B3: [`Self::chain_state_at`]'s question without the
        // fence switch -- did the walk record the chain snapshot for the top of
        // `pc` (the last guard-usable snapshot)?
        !self.splice.is_empty()
            && self
                .graph
                .safepoints
                .iter()
                .rev()
                .find(|sp| !sp.post_call_only)
                .is_some_and(|sp| sp.bci == pc && sp.is_splice_state())
    }

    pub(in crate::ir) fn chain_state_at(&self, pc: usize) -> bool {
        self.splice_chain_fences
            && !self.splice.is_empty()
            && self
                .graph
                .safepoints
                .iter()
                .rev()
                // A post-call snapshot (wave 47) is pushed after it at the
                // same top of `pc`, and is no guard's.
                .find(|sp| !sp.post_call_only)
                .is_some_and(|sp| sp.bci == pc && sp.is_splice_state())
    }

    /// Round nio20 wave 1, lane L1 (NIO19-3): plant the precise exit of a
    /// spliced `checkcast` / `instanceof` whose class is not loaded
    /// ([`ir_splice_cold_unloaded_typecheck_enabled`]; the resolver admitted
    /// it only on an arm the callee's profile never ran). Exactly the
    /// unloaded-static-call exit of the invoke arm: an `Op::Guard` that always
    /// fails, resumed through the chain snapshot the walk pushed at the top of
    /// `pc` -- the callee's own frame AT the type check, its operand still on
    /// the stack -- so the interpreter resolves, loads and checks. Nothing in
    /// the splice is replayed. `false` (the caller refuses the build at `pc`,
    /// which the planner's retry charges to this splice) inside a synchronized
    /// splice window, with site traps off, outside a splice, or without the
    /// chain snapshot.
    /// JIT round 23 wave 5 (lane L1, `CRATONVM_JIT_IR_SPLICE_ATHROW_EXIT`,
    /// opt-in): the precise exit of a SPLICED `athrow` the walk reaches with
    /// live control and no edge to take (no spliced throw edge, no masked cold
    /// arm the branch trap replaced). Before this the whole build refused at
    /// such a pc (`R21CfChain.thenApplyDone`: `getNow` -> `reportJoin`'s
    /// `throw (CancellationException) x`, the tree then rebuilt without the
    /// `getNow` splice, a call per op).
    ///
    /// An always-failing `Op::Guard` resumed through the chain snapshot the
    /// walk pushed at the top of `pc`: the callee at its own `athrow` with the
    /// exception on its operand stack, every caller parked mid-`invoke`. The
    /// interpreter then executes the `athrow` itself -- the callee's and each
    /// caller's handlers in the interpreter's order, the exception's own stack
    /// trace untouched (it was built before the throw). Nothing before `pc` is
    /// replayed.
    ///
    /// Refused (`false`, the caller keeps its old refusal) with the switch or
    /// site traps off, outside a splice, in a synchronized splice window,
    /// without the chain snapshot at `pc`, with dead control, and when the
    /// thrown value is a `null` constant: a cold arm the resolver MASKED to
    /// `aconst_null; nop...; athrow` carries `null` where the original arm
    /// built its exception, and the interpreter resuming at the original
    /// `athrow` with that `null` would raise `NullPointerException` instead
    /// (`prune_cold_throw_arm` traps such an arm at its branch).
    pub(in crate::ir) fn plant_spliced_athrow_exit(&mut self, pc: usize) -> bool {
        if !ir_splice_athrow_exit_enabled()
            || !ir_site_trap_enabled()
            || self.splice.is_empty()
            || self.sync_splice_open()
        {
            return false;
        }
        let Some(&exc) = self.stack.last() else {
            return false;
        };
        if self
            .graph
            .node_opt(exc)
            .is_some_and(|n| matches!(n.op, Op::Const(0)) && n.ty == IrType::Ref)
        {
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
        if ir_bail_reporting() {
            eprintln!(
                "[ir] spliced athrow EXIT at combined pc {pc} in {} with precise scope chain",
                self.method_label.as_deref().unwrap_or("<unknown>")
            );
        }
        true
    }

    pub(in crate::ir) fn plant_unloaded_typecheck_exit(&mut self, pc: usize) -> bool {
        if !ir_site_trap_enabled()
            || self.splice.is_empty()
            || self.sync_splice_open()
            || self.stack.is_empty()
        {
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
        // The same OSR consequence as the static-call exit: an entry past
        // such an exit must import whatever state the interpreter left.
        self.graph.unloaded_static_call_traps = true;
        self.note_class_loading_trap(pc);
        if ir_bail_reporting() {
            eprintln!(
                "[ir] unloaded-typecheck trap at combined pc {pc} in {} with precise scope chain",
                self.method_label.as_deref().unwrap_or("<unknown>")
            );
        }
        true
    }

    /// JIT round 20 wave 19 (lane ySplice, SPLICE20-2,
    /// [`ir_splice_unresolved_new_exit_enabled`]): plant the precise exit of a
    /// spliced `new` whose class is not loaded (the resolver's class-id-0 row,
    /// admitted only on an arm the callee's profile never ran). The type
    /// check's exit ([`Self::plant_unloaded_typecheck_exit`]) without its
    /// operand requirement (`new` pops nothing): an always-failing
    /// `Op::Guard` resumed through the chain snapshot at the top of `pc` --
    /// the callee's own frame BEFORE the `new`, every caller parked mid-
    /// `invoke` -- so the interpreter resolves, loads and initializes the
    /// class there (each `<clinit>` once, in the interpreter's order, its
    /// `NoClassDefFoundError` / `ExceptionInInitializerError` raised in the
    /// callee's frame), then allocates. Nothing of the splice before `pc` is
    /// replayed. Stamped `DeoptReason::ClassLoading`
    /// ([`Self::note_class_loading_trap`]): its first firing asks the
    /// recompile that finds the class loaded. `false` (the caller refuses the
    /// build at `pc`) inside a synchronized splice window, with site traps or
    /// the switch off, outside a splice, or without the chain snapshot.
    pub(in crate::ir) fn plant_unloaded_new_exit(&mut self, pc: usize) -> bool {
        if !ir_site_trap_enabled()
            || !ir_splice_unresolved_new_exit_enabled()
            || self.splice.is_empty()
            || self.sync_splice_open()
        {
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
        // As the type check's exit: an OSR entry past it imports the
        // interpreter's state.
        self.graph.unloaded_static_call_traps = true;
        self.note_class_loading_trap(pc);
        if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] unloaded-new exit at combined pc {pc} in {} with precise scope chain",
                self.method_label.as_deref().unwrap_or("<unknown>")
            );
        }
        true
    }

    /// JIT round 23 wave 9, lane L6 ([`ir_splice_unresolved_ldc_exit_enabled`]):
    /// plant the precise exit of a spliced `ldc` / `ldc_w` whose
    /// `CONSTANT_String` entry is not resolved yet. The resolver sends such a
    /// row with slot `0`, and only on an arm the callee's profile never ran;
    /// `append_ir_inline_site` rebases it into `unloaded_static_call_pcs`. The
    /// exit is [`Self::plant_unloaded_new_exit`]'s: an always-failing guard
    /// resumed through the chain snapshot at the top of `pc`, so the
    /// interpreter resolves the constant there. The caller pushes a `Ref`
    /// null to keep the stack depth the verifier proved; the code after it
    /// cannot be reached. `false` (the caller refuses the build at `pc`)
    /// inside a synchronized splice window, with site traps or the switch off,
    /// outside a splice, or without the chain snapshot.
    pub(in crate::ir) fn plant_unresolved_ldc_exit(&mut self, pc: usize) -> bool {
        if !ir_site_trap_enabled()
            || !ir_splice_unresolved_ldc_exit_enabled()
            || self.splice.is_empty()
            || self.sync_splice_open()
        {
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
        self.graph.unloaded_static_call_traps = true;
        self.note_class_loading_trap(pc);
        if ir_bail_reporting() || crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] unresolved-ldc exit at combined pc {pc} in {} with precise scope chain",
                self.method_label.as_deref().unwrap_or("<unknown>")
            );
        }
        true
    }

    /// Round nio20 wave 2, lane L3 (proposal L1-2): list `pc` in
    /// [`Graph::class_loading_trap_pcs`] so the lowering stamps
    /// `DeoptReason::ClassLoading` on the precise exit just planted there
    /// (`CRATONVM_JIT_IR_CLASS_LOADING_SPLICE_EXITS`, default ON).
    pub(in crate::ir) fn note_class_loading_trap(&mut self, pc: usize) {
        if ir_class_loading_splice_exits_enabled()
            && !self.graph.class_loading_trap_pcs.contains(&pc)
        {
            self.graph.class_loading_trap_pcs.push(pc);
        }
    }

    /// [`Self::chain_state_at`], recording that the graph now RELIES on it:
    /// every guard in a spliced body must publish its chain
    /// ([`Graph::splice_chains_required`]).
    pub(in crate::ir) fn relax_fence_by_chain(&mut self, pc: usize) -> bool {
        if !self.chain_state_at(pc) {
            return false;
        }
        self.graph.splice_chains_required = true;
        true
    }

    /// The [`SpliceScopeChain`] of the walk's state at combined pc `pc`
    /// inside the innermost open splice, and the snapshot `stack` it lays out
    /// (the callee's operand stack, then each enclosing scope's locals and
    /// operand stack, innermost caller first).
    ///
    /// `None` -- record nothing -- when no splice is open, a scope has no
    /// method key (a fixture's site), a pc lies outside the body that owns it,
    /// or a bci does not fit `u32`: a chain that could name the wrong frame is
    /// worse than none, since without one a trap resumes at the outermost
    /// `invoke` exactly as it always has.
    pub(in crate::ir) fn splice_scope_chain(
        &self,
        pc: usize,
    ) -> Option<(Vec<NodeId>, SpliceScopeChain)> {
        let innermost = self.splice.last()?;
        if pc < innermost.base || pc >= innermost.end || innermost.method_key.is_empty() {
            return None;
        }
        // JIT round 23 wave 1, lane L2 (INLINE22-4): a lambda SAM forwarder
        // has no frame to name; a trap in it resumes at the outermost `invoke`
        // at depth 0 (nothing before its one call commits), and at the
        // enclosing body's `invoke` at depth > 0 (wave 2d:
        // [`Self::forwarder_reexecute_chain`], asked by `push_splice_state`).
        // See `SpliceFrame::frameless`.
        if innermost.frameless {
            return None;
        }
        let to_u32 = |v: usize| u32::try_from(v).ok();
        let mut stack = self.stack.clone();
        let mut scopes: Vec<SpliceScopeFrame> = Vec::with_capacity(self.splice.len() + 1);
        scopes.push(SpliceScopeFrame {
            class_id: innermost.class_id,
            method_key: innermost.method_key.clone(),
            bci: to_u32(pc - innermost.base)?,
            code_len: to_u32(innermost.code_len)?,
            max_locals: to_u32(innermost.max_locals)?,
            locals_len: 0,
            stack_len: to_u32(self.stack.len())?,
        });
        // Outward. Frame `i`'s saved state is the frame of the scope that
        // CONTAINS its `invoke`: the enclosing splice `i - 1`, or the compiling
        // method for `i == 0`.
        for i in (0..self.splice.len()).rev() {
            let frame = &self.splice[i];
            // JIT round 23 wave 1, lane L2 (INLINE22-4): the body under a
            // lambda SAM forwarder is parked directly in the `invoke` the
            // forwarder replaced -- the forwarder's scope is skipped, as the
            // interpreter's lambda dispatch pushes no proxy frame. Sound only
            // because the forwarder holds nothing on its operand stack under
            // its call and runs nothing after it but its `return` of the
            // call's value (the VM resolver admits no other shape:
            // `jit_bridge::resolve_ir_lambda_sam_site`); its locals (the
            // receiver and the arguments) are dead past the call.
            if let Some(outer) = i.checked_sub(1).filter(|&o| self.splice[o].frameless) {
                // JIT round 23 wave 2 (stage 2): never through an opaque
                // forwarder (its conversions would be skipped).
                if self.splice[outer].opaque || !frame.saved_stack.is_empty() {
                    return None;
                }
                continue;
            }
            let (class_id, key, base, code_len, max_locals) = match i.checked_sub(1) {
                Some(outer) => {
                    let enclosing = &self.splice[outer];
                    if enclosing.method_key.is_empty() {
                        return None;
                    }
                    (
                        enclosing.class_id,
                        enclosing.method_key.clone(),
                        enclosing.base,
                        enclosing.code_len,
                        enclosing.max_locals,
                    )
                }
                None => (0, String::new(), 0, 0, 0),
            };
            let bci = frame.invoke_pc.checked_sub(base)?;
            if i > 0 && bci >= code_len {
                return None;
            }
            stack.extend_from_slice(&frame.saved_locals);
            stack.extend_from_slice(&frame.saved_stack);
            scopes.push(SpliceScopeFrame {
                class_id,
                method_key: key,
                bci: to_u32(bci)?,
                code_len: to_u32(code_len)?,
                max_locals: to_u32(max_locals)?,
                locals_len: to_u32(frame.saved_locals.len())?,
                stack_len: to_u32(frame.saved_stack.len())?,
            });
        }
        Some((stack, SpliceScopeChain { scopes }))
    }

    /// Push the chain snapshot for the top of `pc` inside a splice (round 13
    /// wave 3, lane framestate). Bounded by the build's snapshot budget; past
    /// it, or for a state [`Self::splice_scope_chain`] refuses, nothing is
    /// recorded and a trap at `pc` resumes at the outermost `invoke`.
    pub(in crate::ir) fn push_splice_state(&mut self, pc: usize) {
        // JIT round 23 wave 2d: inside a nested lambda SAM forwarder the
        // snapshot is the ENCLOSING body's frame at its `invoke`, re-executed.
        let (locals, stack, chain) = match self.forwarder_reexecute_chain(pc) {
            Some(found) => found,
            None => {
                let Some((stack, chain)) = self.splice_scope_chain(pc) else {
                    return;
                };
                (self.locals.clone(), stack, chain)
            }
        };
        let words = locals
            .len()
            .saturating_add(stack.len())
            .saturating_add(self.monitors.len());
        let total = self.splice_state_words.saturating_add(words);
        if total > IR_MAX_FRAME_SNAPSHOT_WORDS {
            return;
        }
        self.splice_state_words = total;
        self.graph.chain_snapshot_census.recorded += 1;
        self.graph.chain_snapshot_census.recorded_words += words;
        let mut sp = SafepointSnapshot::new(pc, locals, stack, self.monitors.clone());
        sp.splice_scope = Some(std::sync::Arc::new(chain));
        self.graph.push_safepoint(sp);
    }

    /// JIT round 23 wave 2d, lane L2 (L2W1-3): the chain a trap at `pc`
    /// inside a lambda SAM forwarder NESTED in another splice resumes through,
    /// as `(snapshot locals, snapshot stack, chain)`; `None` anywhere else (not
    /// in a forwarder, a forwarder at depth 0, whose trap resumes at the
    /// outermost `invoke`, or a state this cannot describe).
    ///
    /// The forwarder has no frame, and nothing before its one call commits
    /// (its own instructions only read captures, load and cast arguments), so
    /// a trap in it re-executes the `invoke` it replaced, in the ENCLOSING
    /// body: scope 0 is that body at that `invoke`'s bci (a re-execute point,
    /// as every scope 0 is), with its locals and its operand stack as they were
    /// before the call (the saved stack plus the call's operands,
    /// `SpliceFrame::reexec_args`). Every scope outward is the ordinary chain
    /// from the enclosing body's own `invoke` ([`Self::splice_scope_chain`]'s
    /// loop). The interpreter then dispatches the call to the proxy as it
    /// always does.
    pub(in crate::ir) fn forwarder_reexecute_chain(
        &self,
        pc: usize,
    ) -> Option<(Vec<NodeId>, Vec<NodeId>, SpliceScopeChain)> {
        let k = self.splice.len().checked_sub(1)?;
        let fwd = &self.splice[k];
        if !fwd.frameless || pc < fwd.base || pc >= fwd.end {
            return None;
        }
        let e = k.checked_sub(1)?;
        let enclosing = &self.splice[e];
        if enclosing.frameless || enclosing.method_key.is_empty() {
            return None;
        }
        let to_u32 = |v: usize| u32::try_from(v).ok();
        let bci = fwd.invoke_pc.checked_sub(enclosing.base)?;
        if bci >= enclosing.code_len {
            return None;
        }
        let locals = fwd.saved_locals.clone();
        let mut stack = fwd.saved_stack.clone();
        stack.extend_from_slice(&fwd.reexec_args);
        let mut scopes: Vec<SpliceScopeFrame> = Vec::with_capacity(e + 2);
        scopes.push(SpliceScopeFrame {
            class_id: enclosing.class_id,
            method_key: enclosing.method_key.clone(),
            bci: to_u32(bci)?,
            code_len: to_u32(enclosing.code_len)?,
            max_locals: to_u32(enclosing.max_locals)?,
            locals_len: 0,
            stack_len: to_u32(stack.len())?,
        });
        for i in (0..=e).rev() {
            let frame = &self.splice[i];
            if let Some(outer) = i.checked_sub(1).filter(|&o| self.splice[o].frameless) {
                if self.splice[outer].opaque || !frame.saved_stack.is_empty() {
                    return None;
                }
                continue;
            }
            let (class_id, key, base, code_len, max_locals) = match i.checked_sub(1) {
                Some(outer) => {
                    let body = &self.splice[outer];
                    if body.method_key.is_empty() {
                        return None;
                    }
                    (
                        body.class_id,
                        body.method_key.clone(),
                        body.base,
                        body.code_len,
                        body.max_locals,
                    )
                }
                None => (0, String::new(), 0, 0, 0),
            };
            let at = frame.invoke_pc.checked_sub(base)?;
            if i > 0 && at >= code_len {
                return None;
            }
            stack.extend_from_slice(&frame.saved_locals);
            stack.extend_from_slice(&frame.saved_stack);
            scopes.push(SpliceScopeFrame {
                class_id,
                method_key: key,
                bci: to_u32(at)?,
                code_len: to_u32(code_len)?,
                max_locals: to_u32(max_locals)?,
                locals_len: to_u32(frame.saved_locals.len())?,
                stack_len: to_u32(frame.saved_stack.len())?,
            });
        }
        Some((locals, stack, SpliceScopeChain { scopes }))
    }

    /// JIT round 19 (lane splice19, CH16-4): can anything consult a chain
    /// snapshot recorded at the top of spliced instruction `pc`?
    ///
    /// The readers of a chain snapshot are: a guard or trapping node lowered at
    /// its raw pc (`ir_lower`'s `splice_chain_snapshot_for_site`), a builder
    /// fence the chain relaxes ([`Self::relax_fence_by_chain`], asked only at
    /// an instruction that can trap: `admits_after_committed_splice_store`
    /// admits every opcode below without asking), a spliced loop header's
    /// poll exit and an optimizing OSR entry at a join, and an unrolled copy's
    /// per-iteration frame (cloned by the pc of a node that can deopt).
    /// Instructions that only move values between locals and the operand
    /// stack, or compute without a trap (no division, no array, field or
    /// call), produce none of those, so `false` for them -- unless `pc` is a
    /// join, a loop header or the body's first instruction, which are kept
    /// whatever their opcode. Conservative in the other direction: anything
    /// unlisted (including `ldc`, `wide` and every branch, which a pruned
    /// profile can turn into a trap) answers `true`.
    ///
    /// A guard that finds no chain where one was skipped falls back to the
    /// flat frame at the outermost `invoke` (correct whenever the chain was
    /// optional), or, if the graph relies on its chains, refuses the compile
    /// (`ir_lower: a spliced guard after a chain-relaxed fence has no caller
    /// chain`) -- never a wrong resume.
    pub(in crate::ir) fn splice_pc_may_read_chain(&self, code: &[u8], pc: usize) -> bool {
        if self.merges.contains_key(&pc) || self.loop_headers.contains(&pc) {
            return true;
        }
        if self.splice.last().is_some_and(|f| f.base == pc) {
            return true;
        }
        let Some(&op) = code.get(pc) else {
            return true;
        };
        !splice_opcode_cannot_trap(op)
    }

    /// Interpreter round i1 wave 47, lane L3 (the first stage of
    /// `i39-L3-proposal-post-call-exits-inside-a-splice`): at the top of `pc`,
    /// record the chain snapshot of the frames the spliced call `call` returns
    /// into, when `pc` is that call's successor (`invoke_pc`'s next
    /// instruction) in the same open splice: the callee at `pc` with the
    /// call's result pushed, every caller parked in its `invoke`. Marked
    /// [`SafepointSnapshot::post_call_only`], so no guard, fence or by-pc
    /// reader consults it. Bounded by the chain snapshots' budget; nothing is
    /// recorded past it or for a state [`Self::splice_scope_chain`] refuses,
    /// and the call then gets no post-call exit, as before.
    pub(in crate::ir) fn push_post_call_splice_state(
        &mut self,
        pc: usize,
        call: NodeId,
        invoke_pc: usize,
        next_pc: Option<usize>,
    ) {
        // SPLICE2-5: taken first, so it never outlives this call's successor.
        let join_value = self.post_call_join_value.take().filter(|&(_, c)| c == call);
        if next_pc != Some(pc) || self.splice.is_empty() || self.ctrl_opt().is_none() {
            return;
        }
        let Some((stack, chain)) = self.splice_scope_chain(pc) else {
            return;
        };
        // JIT round 19 wave 3 (lane splice3, SPLICE2-5): a nested guarded
        // splice's miss call returns into the join's successor with ITS result
        // where the walk's stack holds the join's value φ (the φ's input on
        // the miss edge). Every other entry is the saved caller frame, the
        // same on both edges.
        let stack = match join_value {
            Some((phi, c)) => stack
                .into_iter()
                .map(|v| if v == phi { c } else { v })
                .collect(),
            None => stack,
        };
        // The call and its successor in ONE body: the innermost scope's.
        let Some(innermost) = self.splice.last() else {
            return;
        };
        if invoke_pc < innermost.base || invoke_pc >= innermost.end {
            return;
        }
        let words = self
            .locals
            .len()
            .saturating_add(stack.len())
            .saturating_add(self.monitors.len());
        let total = self.splice_state_words.saturating_add(words);
        if total > IR_MAX_FRAME_SNAPSHOT_WORDS {
            return;
        }
        self.splice_state_words = total;
        let mut sp = SafepointSnapshot::new(pc, self.locals.clone(), stack, self.monitors.clone());
        sp.splice_scope = Some(std::sync::Arc::new(chain));
        sp.post_call_of = call;
        sp.post_call_only = true;
        self.graph.push_safepoint(sp);
    }

    /// Leave the innermost splice, restoring the caller's frame and pushing the
    /// callee's result. Returns the caller pc to continue at.
    ///
    /// Nodes built inside the splice keep their COMBINED-buffer pcs; `ir_lower`
    /// translates those to the enclosing `invoke` wherever a bci has to name an
    /// instruction in this method. See the two-bci discussion at
    /// [`IrInlineSite`].
    pub(in crate::ir) fn end_splice(&mut self, value: Option<NodeId>) -> Option<usize> {
        let frame = self.splice.pop()?;
        // Round 14 wave 2 (lane syncsplice, SR-1): release a synchronized
        // body's monitor on its one return edge -- after every node the body
        // built, each of which must be one that cannot trap while the monitor
        // is held (`sync_splice_window_node_admitted`). A body that built
        // anything else refuses the build here, at a spliced pc, so the
        // planner's rebuild drops this site and the method keeps the call.
        if let Some(mut window) = frame.sync_monitor {
            let first = (window.enter as usize).checked_add(1)?;
            let body = self.graph.nodes.get(first..).unwrap_or(&[]);
            if !body
                .iter()
                .all(|n| sync_splice_window_node_admitted(&self.graph, n, &window))
            {
                return None;
            }
            let ctrl = self.ctrl_opt()?;
            let exit = self.graph.add(
                Op::MonitorExit,
                IrType::Memory,
                vec![ctrl, self.mem, window.monitor],
                Some(frame.invoke_pc),
            );
            self.mem = exit;
            window.exit = exit;
            self.graph.sync_splice_windows.push(window);
            self.note_sync_splice_built(&window, frame.invoke_pc);
        }
        if let Some(recv) = frame.ctor_receiver {
            self.uninitialized_news.remove(&recv);
        }
        self.release_ctor_receiver(frame.ctor_receiver);
        self.locals = frame.saved_locals;
        self.stack = frame.saved_stack;
        if frame.returns_value {
            self.push(value?);
        }
        // See `begin_splice`: facts do not cross a splice boundary.
        self.splice_null_facts.clear();
        if self.splice.is_empty() {
            // The sets are scoped to one outermost splice: node ids from a
            // closed region can never be a later region's store target, and
            // keeping them would only grow.
            self.splice_local.clear();
            self.splice_tainted.clear();
            self.splice_edge_null_facts.clear();
        }
        self.box_convert_member_result(frame.invoke_pc)?;
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
    pub(in crate::ir) fn splice_return(
        &mut self,
        value: Option<NodeId>,
        pc: usize,
    ) -> Option<usize> {
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
    pub(in crate::ir) fn finish_multi_return_splice(&mut self) -> Option<usize> {
        let mut frame = self.splice.pop()?;
        let reopen = frame.bimorphic_reopen.take();
        let reopen_here = reopen.is_some();
        // Round nio20 wave 4 (lane L1, W3L4-2): the join below merges the
        // closed body's return edges (and a guarded site's miss edge), none of
        // which an entry recorded inside that body need dominate.
        if self.field_memo_in_splices {
            self.field_load_memo_clear_from_depth(self.splice.len() + 1);
        }
        // JIT round 23 wave 2e (W4L1-1): a deferred miss call of this frame
        // joins here; it writes everything, so the memo clears.
        if !self.field_memo_deferred_calls.is_empty() {
            let before = self.field_memo_deferred_calls.len();
            self.field_memo_deferred_calls
                .retain(|c| !frame.exits.iter().any(|&(_, mem, _)| mem == *c));
            if self.field_memo_deferred_calls.len() != before && !self.field_load_memo.is_empty() {
                self.field_load_memo.clear();
                self.field_load_memo_census.cleared_by_opaque += 1;
            }
        }
        // Round 14 wave 5 (lane sync5, SS8-1 b): a synchronized splice reaches
        // here only for a pc-order body (`begin_splice`); its exit goes after
        // the join below.
        if frame.sync_monitor.is_some()
            && (!frame.ranges.is_empty() || !ir_sync_splice_multi_return_enabled())
        {
            return None;
        }
        if let Some(recv) = frame.ctor_receiver {
            self.uninitialized_news.remove(&recv);
        }
        self.release_ctor_receiver(frame.ctor_receiver);
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

        // SS8-1 b: the synchronized body's one exit, after the join -- the
        // same check and release `end_splice` makes on a single return edge.
        // Every return edge arrives here (the scan admits no other way out),
        // and the join's own nodes are control and φs.
        if let Some(mut window) = frame.sync_monitor {
            let first = (window.enter as usize).checked_add(1)?;
            let body = self.graph.nodes.get(first..).unwrap_or(&[]);
            if !body
                .iter()
                .all(|n| sync_splice_window_node_admitted(&self.graph, n, &window))
            {
                return None;
            }
            let exit = self.graph.add(
                Op::MonitorExit,
                IrType::Memory,
                vec![ctrl, self.mem, window.monitor],
                Some(frame.invoke_pc),
            );
            self.mem = exit;
            window.exit = exit;
            self.graph.sync_splice_windows.push(window);
            self.note_sync_splice_built(&window, frame.invoke_pc);
            note_sync_splice_census(SYNC_CENSUS_MULTI_RETURN);
        }

        // JIT round 19 (lane splice19, CH6W5-1; wave 2, lane splice2,
        // SPLICE19-6): past this join the nested guarded splice's miss call may
        // have run -- its replay commit and its taints hold from here on, in
        // the enclosing body (the hit body was a path it never ran on). A
        // bimorphic second body's join hands the call on to the next body
        // instead (`reopen_here`). The call gets no post-call chain snapshot.
        if let Some(call) = frame.miss_call {
            if !self.splice.is_empty() && !reopen_here {
                let pending = self.pending_post_call.take();
                self.note_splice_call(call);
                // JIT round 19 wave 3 (lane splice3, SPLICE2-5,
                // `CRATONVM_JIT_IR_NESTED_MISS_POST_CALL_EXIT`): keep the
                // post-call snapshot `note_splice_call` just asked for (the
                // successor of the nested `invoke` is this join's
                // `return_pc`, walked next), naming the call's result in place
                // of the joined value. Off: no snapshot, as before.
                if self.pending_post_call.is_some() && ir_nested_miss_post_call_exit_enabled() {
                    self.post_call_join_value = if frame.returns_value {
                        self.stack
                            .last()
                            .copied()
                            .filter(|&v| v != call)
                            .map(|phi| (phi, call))
                    } else {
                        None
                    };
                } else {
                    self.pending_post_call = pending;
                }
            }
        }
        // A join: see `end_splice`.
        self.splice_null_facts.clear();
        if self.splice.is_empty() {
            self.splice_local.clear();
            self.splice_tainted.clear();
            self.splice_edge_null_facts.clear();
        }
        // Round 14 wave 2 (lane guard, GS-1): the second body of a bimorphic
        // splice closed; its join is the first exit of the first body, which
        // opens now instead of the caller resuming.
        if let Some(reopen) = reopen {
            return self.reopen_bimorphic_first_body(reopen, frame.returns_value, frame.miss_call);
        }
        self.box_convert_member_result(frame.invoke_pc)?;
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

    /// Diagnostic-only: name the method for census lines when no invoke
    /// labels were supplied (interpreter round i4 wave 95, lane L2: the
    /// unresolved-field trap census of a method with no invoke). Never read
    /// by lowering.
    pub fn set_method_label(&mut self, method: Box<str>) {
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
    pub(in crate::ir) fn bail_invoke<T>(&self, site: u32, pc: usize) -> Option<T> {
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

    pub(in crate::ir) fn push(&mut self, id: NodeId) {
        self.stack.push(id);
    }

    /// Pop the top abstract-stack entry, or `None` on an empty stack (or a
    /// `NO_NODE` placeholder entry). The checked form of [`Self::pop`].
    pub(in crate::ir) fn pop_opt(&mut self) -> Option<NodeId> {
        self.stack.pop().and_then(node_id_opt)
    }

    /// Top of the abstract stack, or `None` when empty/undefined.
    pub(in crate::ir) fn peek_opt(&self) -> Option<NodeId> {
        self.stack.last().copied().and_then(node_id_opt)
    }

    /// Sentinel-returning [`Self::pop_opt`], kept for the opcode lowerings that
    /// still thread `NO_NODE` through to the graph.
    pub(in crate::ir) fn pop(&mut self) -> NodeId {
        self.pop_opt().unwrap_or(NO_NODE)
    }

    pub(in crate::ir) fn peek(&self) -> NodeId {
        self.peek_opt().unwrap_or(NO_NODE)
    }

    /// The current control token, or `None` in dead code (after an
    /// unconditional transfer, where `ctrl` is cleared to [`NO_NODE`]).
    pub(in crate::ir) fn ctrl_opt(&self) -> Option<NodeId> {
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
    pub(in crate::ir) fn is_cat2_value(&self, id: NodeId) -> bool {
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
    pub(in crate::ir) fn store_local(&mut self, idx: usize, val: NodeId) {
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
        if !self.exact_receiver_single_store_slots.is_empty() {
            self.note_exact_receiver_store(idx, val);
        }
        if self.splice.is_empty() && !self.loop_written_slots.is_empty() {
            match self.fresh_new_class_of(val) {
                Some(class_id) => {
                    self.exact_receiver_flow.insert(idx, class_id);
                }
                None => {
                    self.exact_receiver_flow.remove(&idx);
                }
            }
            if wide {
                self.exact_receiver_flow.remove(&(idx + 1));
            }
        }
    }

    /// JIT round 21 wave 2 (lane keepset): restore [`Self::store_local`]'s
    /// two-slot invariant on a frame a JOIN just built.
    ///
    /// A merge builds each slot independently. One predecessor holding a
    /// `long` at `idx` (its high half `idx + 1` `NO_NODE`) and another that
    /// wrote `idx + 1` (`store_local` cleared its `idx`) join to a `Long` φ at
    /// `idx` -- `phi_inputs_conflict` lets `NO_NODE` contribute nothing -- and
    /// a category-1 φ at `idx + 1`. Every frame state after the join then names
    /// a category-2 value whose high half holds another live value, and
    /// `deopt_resume::ir_deopt_locals` (`i += 2` after a `Long`/`Double`) skips
    /// that value and shifts every later local of the rebuilt frame by one slot.
    /// The JVM verifier's merge makes `idx` TOP there (`long` joined with TOP),
    /// so nothing can read it: describe it as uninitialised. A non-`NO_NODE`
    /// `idx + 1` after the join proves some predecessor wrote it, which is what
    /// makes `idx` TOP. javac's lowest-free-slot allocator does not emit this
    /// (ECJ, Kotlin and bytecode rewriters may), so the scan is a no-op on its
    /// output; the switch is read only when the shape is present.
    /// `CRATONVM_JIT_IR_MERGE_CAT2_PAIR_KILL=0` keeps the φ, as before.
    ///
    /// Wave 3 correction: the finished graph was already safe. The split
    /// `long`'s φ always has an undefined input (the predecessor that wrote the
    /// high half cleared the low slot), and `retire_untypeable_phis`, which
    /// runs unconditionally at the end of `build`, retires every such φ and
    /// rewrites the snapshots naming it to undefined. This rule makes the
    /// builder's own frame at the join consistent from the start (defense in
    /// depth for anything that reads frames before that pass); the VM-side
    /// backstop (`deopt_resume::cat2_pair_split_in_locals`) covers other
    /// producers.
    pub(in crate::ir) fn kill_cat2_values_split_at_a_join(&mut self) {
        let split: Vec<usize> = (0..self.locals.len().saturating_sub(1))
            .filter(|&idx| self.locals[idx + 1] != NO_NODE && self.is_cat2_value(self.locals[idx]))
            .collect();
        if split.is_empty() {
            return;
        }
        for idx in split {
            self.locals[idx] = NO_NODE;
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
    pub(in crate::ir) fn push_local(&mut self, idx: usize) -> bool {
        match self.locals.get(idx).copied() {
            Some(v) => {
                self.push(v);
                true
            }
            None => false,
        }
    }

    // ── Node creation helpers ────────────────────────────────────────

    pub(in crate::ir) fn add_data(
        &mut self,
        op: Op,
        ty: IrType,
        inputs: Vec<NodeId>,
        pc: usize,
    ) -> NodeId {
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
    pub(in crate::ir) fn assert_operand_types(
        &self,
        op: &Op,
        ty: IrType,
        inputs: &[NodeId],
        pc: usize,
    ) {
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
}
