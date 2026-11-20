#[allow(unused_imports)]
use super::super::*;

/// The bytecode half of [`ir_sync_splice_site_refusal`]: every instruction
/// must lower to something that can neither deopt nor raise while the monitor
/// is held -- constants, locals, stack shuffles, arithmetic that cannot trap,
/// conversions, compares, FORWARD branches, one trailing return, and field
/// accesses through the receiver (instance callee) or of the method's own
/// class's statics (static callee). `ir::sync_splice_window_node_admitted`
/// is the same rule on the graph.
///
/// A small abstract interpretation over JVM slots tracks which slots hold the
/// receiver (`true`), so `this.a` is admitted and `this.a.b` -- a read through
/// a field value, which may be null -- is not. With forward branches only, one
/// pass in pc order has seen every predecessor of a join when it reaches it.
pub(crate) fn ir_sync_splice_body_scan(site: &InlineSite) -> Result<(), &'static str> {
    type SlotState = (Vec<bool>, Vec<bool>);
    fn pop(stack: &mut Vec<bool>, n: usize) -> Result<Vec<bool>, &'static str> {
        let at = stack
            .len()
            .checked_sub(n)
            .ok_or("ir-sync-splice-stack-underflow")?;
        Ok(stack.split_off(at))
    }
    fn join(a: SlotState, b: SlotState) -> Result<SlotState, &'static str> {
        if a.0.len() != b.0.len() || a.1.len() != b.1.len() {
            return Err("ir-sync-splice-join-shape");
        }
        let meet = |x: Vec<bool>, y: Vec<bool>| -> Vec<bool> {
            x.into_iter().zip(y).map(|(p, q)| p && q).collect()
        };
        Ok((meet(a.0, b.0), meet(a.1, b.1)))
    }
    fn set_local(locals: &mut [bool], idx: usize, v: bool) -> Result<(), &'static str> {
        *locals.get_mut(idx).ok_or("ir-sync-splice-local-index")? = v;
        Ok(())
    }
    // `(slots popped, slots pushed)` of the admitted arithmetic, conversions
    // and compares. The dividing forms are absent: an integer one traps on a
    // zero divisor, and a floating one is an `Op::Div`/`Op::Rem` the window
    // rule does not admit either.
    fn arith(op: u8) -> Option<(usize, usize)> {
        Some(match op {
            // iadd ladd fadd dadd, isub .. dsub, imul .. dmul
            0x60..=0x6b => {
                if (op - 0x60) % 2 == 0 {
                    (2, 1)
                } else {
                    (4, 2)
                }
            }
            0x74 | 0x76 => (1, 1),
            0x75 | 0x77 => (2, 2),
            0x78 | 0x7a | 0x7c => (2, 1),
            0x79 | 0x7b | 0x7d => (3, 2),
            0x7e | 0x80 | 0x82 => (2, 1),
            0x7f | 0x81 | 0x83 => (4, 2),
            0x85 | 0x87 | 0x8c | 0x8d => (1, 2),
            0x86 | 0x8b | 0x91..=0x93 => (1, 1),
            0x88 | 0x89 | 0x8e | 0x90 => (2, 1),
            0x8a | 0x8f => (2, 2),
            0x94 | 0x97 | 0x98 => (4, 1),
            0x95 | 0x96 => (2, 1),
            _ => return None,
        })
    }
    fn slots(tag: u8) -> usize {
        if matches!(tag, b'J' | b'D') {
            2
        } else {
            1
        }
    }

    // Round 14 wave 4 (lane sync4, SS-7): the one call admitted is a
    // `Thread.holdsLock` the builder folds (checked per instruction below).
    // Round 14 wave 6 (lane sync6, SS8-1 a): or a same-receiver synchronized
    // body spliced with no pair ([`ir_sync_splice_nested_held_site`], its
    // receiver and its own body checked at the `invokevirtual` below).
    if site
        .nested_sites
        .iter()
        .any(|n| ir_sync_splice_nested_held_site(site, n.callee_pc).is_none())
        || site.invoke_targets.iter().any(|&(pc, _)| {
            !ir_sync_splice_holds_lock_row(site, pc)
                && ir_sync_splice_nested_held_site(site, pc).is_none()
        })
        || !site.elided_invoke_pcs.is_empty()
    {
        return Err("ir-sync-splice-body-calls");
    }
    if !site.ir_new_info.is_empty() || !site.ir_typecheck_info.is_empty() {
        return Err("ir-sync-splice-body-allocates-or-casts");
    }
    let is_static = site.callee_is_static;
    if is_static && site.class_id == 0 {
        return Err("ir-sync-splice-static-class-unknown");
    }
    let code = &site.callee_code;
    let len = site.callee_code_len;
    if len == 0 || code.len() < len {
        return Err("ir-sync-splice-code-length");
    }
    let mut entry_locals = vec![false; site.callee_max_locals];
    if !is_static {
        *entry_locals
            .first_mut()
            .ok_or("ir-sync-splice-no-receiver-slot")? = true;
    }
    let mut cur: Option<SlotState> = Some((Vec::new(), entry_locals));
    let mut pending: HashMap<usize, SlotState> = HashMap::new();
    let mut returns = 0usize;
    // Round 14 wave 7 (lane sync7, SS8-5): the pc right after an admitted
    // self-class `ldc`, which only the `holdsLock` call there may consume.
    let mut self_class_ldc_before: Option<usize> = None;
    let mut pc = 0usize;
    while pc < len {
        let after_self_class_ldc = self_class_ldc_before.take() == Some(pc);
        if let Some(incoming) = pending.remove(&pc) {
            cur = Some(match cur.take() {
                None => incoming,
                Some(here) => join(here, incoming)?,
            });
        }
        let (mut stack, mut locals) = cur.take().ok_or("ir-sync-splice-unreachable-code")?;
        let op = *code.get(pc).ok_or("ir-sync-splice-code-length")?;
        let width = bytecode_analysis::step(code, pc).max(1);
        let operand = |k: usize| -> Result<usize, &'static str> {
            code.get(pc + k)
                .copied()
                .map(usize::from)
                .ok_or("ir-sync-splice-operand")
        };
        let mut falls_through = true;
        let mut branch_to: Option<usize> = None;
        match op {
            // nop
            0x00 => {}
            // aconst_null, iconst_*, fconst_*, bipush, sipush
            0x01..=0x08 | 0x0b..=0x0d | 0x10 | 0x11 => stack.push(false),
            // lconst_*, dconst_*
            0x09 | 0x0a | 0x0e | 0x0f => stack.extend([false, false]),
            // ldc / ldc_w of an Integer or Float (a String or Class constant
            // has no row: its materialisation is a helper call).
            // Round 14 wave 7 (lane sync7, SS8-5): or, in a static body, the
            // `ldc C` of `holdsLock(C.class)` with `C` the body's own class
            // (an `ir_ldc_class_info` row), which the builder folds with the
            // `invokestatic` after it (`ir::IrBuilder::
            // try_fold_self_class_holds_lock`). That call must be the very
            // next instruction and no branch target, so the value it pops is
            // this one.
            0x12 | 0x13 => {
                if site.ldc_info.iter().any(|&(p, _)| p == pc) {
                    stack.push(false);
                } else if is_static
                    && site
                        .ir_ldc_class_info
                        .iter()
                        .any(|&(p, class_id)| p == pc && class_id == site.class_id)
                    && ir::ir_sync_splice_self_class_holds_lock_enabled()
                    && ir_sync_splice_holds_lock_row(site, pc + width)
                    && !pending.contains_key(&(pc + width))
                {
                    self_class_ldc_before = Some(pc + width);
                    stack.push(false);
                } else {
                    return Err("ir-sync-splice-ldc");
                }
            }
            0x14 => {
                if !site.ldc2w_info.iter().any(|&(p, _)| p == pc) {
                    return Err("ir-sync-splice-ldc");
                }
                stack.extend([false, false]);
            }
            // iload, fload, and their `_n` forms
            0x15 | 0x17 | 0x1a..=0x1d | 0x22..=0x25 => stack.push(false),
            // lload, dload, and their `_n` forms
            0x16 | 0x18 | 0x1e..=0x21 | 0x26..=0x29 => stack.extend([false, false]),
            // aload, aload_n: the local's own kind
            0x19 | 0x2a..=0x2d => {
                let idx = if op == 0x19 {
                    operand(1)?
                } else {
                    usize::from(op - 0x2a)
                };
                let v = *locals.get(idx).ok_or("ir-sync-splice-local-index")?;
                stack.push(v);
            }
            // istore, fstore, istore_n, fstore_n
            0x36 | 0x38 | 0x3b..=0x3e | 0x43..=0x46 => {
                let idx = match op {
                    0x36 | 0x38 => operand(1)?,
                    0x3b..=0x3e => usize::from(op - 0x3b),
                    _ => usize::from(op - 0x43),
                };
                pop(&mut stack, 1)?;
                set_local(&mut locals, idx, false)?;
            }
            // lstore, dstore, lstore_n, dstore_n
            0x37 | 0x39 | 0x3f..=0x42 | 0x47..=0x4a => {
                let idx = match op {
                    0x37 | 0x39 => operand(1)?,
                    0x3f..=0x42 => usize::from(op - 0x3f),
                    _ => usize::from(op - 0x47),
                };
                pop(&mut stack, 2)?;
                set_local(&mut locals, idx, false)?;
                set_local(&mut locals, idx + 1, false)?;
            }
            // astore, astore_n: the local takes the value's kind
            0x3a | 0x4b..=0x4e => {
                let idx = if op == 0x3a {
                    operand(1)?
                } else {
                    usize::from(op - 0x4b)
                };
                let v = pop(&mut stack, 1)?;
                set_local(&mut locals, idx, matches!(v.as_slice(), [true]))?;
            }
            // pop, pop2
            0x57 => {
                pop(&mut stack, 1)?;
            }
            0x58 => {
                pop(&mut stack, 2)?;
            }
            // dup .. swap, on slots
            0x59 => match pop(&mut stack, 1)?.as_slice() {
                &[a] => stack.extend([a, a]),
                _ => return Err("ir-sync-splice-stack-underflow"),
            },
            0x5a => match pop(&mut stack, 2)?.as_slice() {
                &[b, a] => stack.extend([a, b, a]),
                _ => return Err("ir-sync-splice-stack-underflow"),
            },
            0x5b => match pop(&mut stack, 3)?.as_slice() {
                &[c, b, a] => stack.extend([a, c, b, a]),
                _ => return Err("ir-sync-splice-stack-underflow"),
            },
            0x5c => match pop(&mut stack, 2)?.as_slice() {
                &[b, a] => stack.extend([b, a, b, a]),
                _ => return Err("ir-sync-splice-stack-underflow"),
            },
            0x5d => match pop(&mut stack, 3)?.as_slice() {
                &[c, b, a] => stack.extend([b, a, c, b, a]),
                _ => return Err("ir-sync-splice-stack-underflow"),
            },
            0x5e => match pop(&mut stack, 4)?.as_slice() {
                &[d, c, b, a] => stack.extend([b, a, d, c, b, a]),
                _ => return Err("ir-sync-splice-stack-underflow"),
            },
            0x5f => match pop(&mut stack, 2)?.as_slice() {
                &[b, a] => stack.extend([a, b]),
                _ => return Err("ir-sync-splice-stack-underflow"),
            },
            // iinc
            0x84 => {}
            // if<cond>, ifnull, ifnonnull
            0x99..=0x9e | 0xc6 | 0xc7 => {
                pop(&mut stack, 1)?;
                branch_to = bytecode_analysis::offset_branch_target(code, pc);
            }
            // if_icmp<cond>, if_acmp<cond>
            0x9f..=0xa6 => {
                pop(&mut stack, 2)?;
                branch_to = bytecode_analysis::offset_branch_target(code, pc);
            }
            // goto, goto_w
            0xa7 | 0xc8 => {
                branch_to = bytecode_analysis::offset_branch_target(code, pc);
                falls_through = false;
            }
            // the one return, and it must be the last instruction -- or, under
            // SS8-1 b (round 14 wave 5, lane sync5), one of several whose
            // join the builder closes the window after
            // (`ir::ir_sync_splice_multi_return_enabled`); the last
            // instruction is then still a return (flow cannot fall off the
            // end: `cur` must be empty there, below).
            0xac..=0xb1 => {
                if pc + 1 != len && !ir::ir_sync_splice_multi_return_enabled() {
                    return Err("ir-sync-splice-return-not-last");
                }
                returns += 1;
                falls_through = false;
            }
            // Round 14 wave 6 (lane sync6, SS8-1 a): a `synchronized` instance
            // call on THIS body's receiver, spliced with no pair inside the
            // window (`ir::IrBuilder::begin_splice` proves the receiver node
            // again). Its receiver must be the receiver slot, and its own body
            // must pass this scan with one return and no nested body (one
            // level), so every node it builds is one the window admits.
            0xb6 => {
                // Without a held row it is the call it always was.
                let n = ir_sync_splice_nested_held_site(site, pc).ok_or("ir-sync-splice-opcode")?;
                if !n.site.nested_sites.is_empty()
                    || ir_splice_body_has_several_returns(
                        &n.site.callee_code,
                        n.site.callee_code_len,
                    )
                {
                    return Err("ir-sync-splice-nested-shape");
                }
                ir_sync_splice_body_scan(&n.site)?;
                let (_, span) = compute_param_jvm_slots(&n.site.descriptor, false);
                let (_, ret) =
                    static_call_shape(&n.site.descriptor).ok_or("ir-sync-splice-nested-shape")?;
                if pop(&mut stack, span)?.first() != Some(&true) {
                    return Err("ir-sync-splice-nested-not-receiver");
                }
                if ret != b'V' {
                    stack.extend(std::iter::repeat(false).take(slots(ret)));
                }
            }
            // Round 14 wave 4 (lane sync4, SS-7): `Thread.holdsLock(this)` of
            // an instance body, which the builder folds to `1` (the window
            // holds the receiver; `ir::IrBuilder::try_fold_holds_lock`).
            // Round 14 wave 7 (lane sync7, SS8-5): or `holdsLock(C.class)` of
            // a static body of `C`, its `ldc C` the instruction right before
            // (the window holds `C`'s mirror).
            0xb8 => {
                if !ir::ir_holds_lock_fold_enabled() || !ir_sync_splice_holds_lock_row(site, pc) {
                    return Err("ir-sync-splice-body-calls");
                }
                if is_static {
                    if !after_self_class_ldc {
                        return Err("ir-sync-splice-body-calls");
                    }
                    pop(&mut stack, 1)?;
                } else if !matches!(pop(&mut stack, 1)?.as_slice(), [true]) {
                    return Err("ir-sync-splice-holds-lock-not-receiver");
                }
                stack.push(false);
            }
            // getstatic / putstatic: the static callee's OWN class, a
            // non-volatile write
            0xb2 | 0xb3 => {
                if !is_static {
                    return Err("ir-sync-splice-instance-body-static-access");
                }
                let &(_, class_id, _, tag, is_volatile) = site
                    .static_field_info
                    .iter()
                    .find(|row| row.0 == pc)
                    .ok_or("ir-sync-splice-static-unresolved")?;
                if class_id != site.class_id || (op == 0xb3 && is_volatile) {
                    return Err("ir-sync-splice-foreign-static");
                }
                if op == 0xb2 {
                    stack.extend(std::iter::repeat(false).take(slots(tag)));
                } else {
                    pop(&mut stack, slots(tag))?;
                }
            }
            // getfield / putfield: through the receiver only
            0xb4 | 0xb5 => {
                if is_static {
                    return Err("ir-sync-splice-static-body-field-access");
                }
                let &(_, _, tag) = site
                    .field_info
                    .iter()
                    .find(|row| row.0 == pc)
                    .ok_or("ir-sync-splice-field-unresolved")?;
                if op == 0xb5 {
                    pop(&mut stack, slots(tag))?;
                }
                if !matches!(pop(&mut stack, 1)?.as_slice(), [true]) {
                    return Err("ir-sync-splice-field-not-through-receiver");
                }
                if op == 0xb4 {
                    stack.extend(std::iter::repeat(false).take(slots(tag)));
                }
            }
            // nio21 wave 12 (lane L4, W10L2-3): idiv / irem / ldiv / lrem by
            // a non-zero constant pushed right before it, on no branch's
            // target (`ir::splice_division_cannot_trap`): the builder plants
            // no guard and `ir::sync_splice_window_node_admitted` admits the
            // `Op::Div` / `Op::Rem` (`node_cannot_deopt`). A variable divisor
            // stays refused: nothing in the window may trap.
            0x6c | 0x6d | 0x70 | 0x71
                if ir::splice_division_cannot_trap(code, len, pc, |p| {
                    inline_site_ldc_integral(site, p)
                }) =>
            {
                let (popped, pushed) = if matches!(op, 0x6d | 0x71) {
                    (4, 2)
                } else {
                    (2, 1)
                };
                pop(&mut stack, popped)?;
                stack.extend(std::iter::repeat(false).take(pushed));
            }
            _ => match arith(op) {
                Some((popped, pushed)) => {
                    pop(&mut stack, popped)?;
                    stack.extend(std::iter::repeat(false).take(pushed));
                }
                // Calls, allocation, array access, casts, `athrow`, monitors,
                // switches, subroutines, `wide`, any other division: refused.
                None => return Err("ir-sync-splice-opcode"),
            },
        }
        if matches!(op, 0x99..=0xa7 | 0xc6..=0xc8) {
            let target = branch_to.ok_or("ir-sync-splice-branch-target")?;
            if target <= pc || target >= len {
                return Err("ir-sync-splice-backward-branch");
            }
            let state = (stack.clone(), locals.clone());
            let merged = match pending.remove(&target) {
                Some(prev) => join(prev, state)?,
                None => state,
            };
            pending.insert(target, merged);
        }
        if falls_through {
            cur = Some((stack, locals));
        }
        pc += width;
    }
    let returns_admitted =
        returns == 1 || (returns > 1 && ir::ir_sync_splice_multi_return_enabled());
    if !returns_admitted
        || cur.is_some()
        || !pending.is_empty()
        || !matches!(code.get(len - 1), Some(0xac..=0xb1))
    {
        return Err("ir-sync-splice-not-single-trailing-return");
    }
    Ok(())
}

/// Round 14 wave 4 (lane sync4, SS-7): is the call at callee pc `pc` of `site`
/// an `invokestatic java/lang/Thread.holdsLock(Ljava/lang/Object;)Z` that
/// stays a call row (not spliced)? Only the shape; whether its argument is the
/// receiver is [`ir_sync_splice_body_scan`]'s (and the builder's) question.
pub(crate) fn ir_sync_splice_holds_lock_row(site: &InlineSite, pc: usize) -> bool {
    site.callee_code.get(pc).copied() == Some(0xb8)
        && pc < site.callee_code_len
        && !site.nested_sites.iter().any(|n| n.callee_pc == pc)
        && site.invoke_targets.iter().any(|(p, t)| {
            *p == pc
                && t.invoke_kind == 3
                && t.class_name == "java/lang/Thread"
                && t.method_name == "holdsLock"
                && t.descriptor == "(Ljava/lang/Object;)Z"
        })
}

/// Round 14 wave 6 (lane sync6, SS8-1 a): the nested row at callee pc `pc` of
/// the synchronized instance body `site`, when it is a same-receiver
/// synchronized body the window may hold for it: an unguarded
/// `invokevirtual` whose resolved body is a `synchronized` INSTANCE method,
/// under [`ir::ir_sync_splice_nested_same_receiver_enabled`]. Only the shape:
/// whether its receiver is `site`'s receiver, and whether its own body is
/// admitted, is [`ir_sync_splice_body_scan`]'s question (and the receiver
/// node is proven again by `ir::IrBuilder::begin_splice`).
pub(crate) fn ir_sync_splice_nested_held_site(site: &InlineSite, pc: usize) -> Option<&NestedInlineSite> {
    if !site.callee_is_synchronized
        || site.callee_is_static
        || pc >= site.callee_code_len
        || site.callee_code.get(pc).copied() != Some(0xb6)
        || !ir::ir_sync_splice_nested_same_receiver_enabled()
    {
        return None;
    }
    site.nested_sites.iter().find(|n| {
        n.callee_pc == pc
            && n.guard_class_id == 0
            && n.site.callee_is_synchronized
            && !n.site.callee_is_static
    })
}

/// SS-7: does the builder fold every call `site` makes -- a synchronized body
/// [`ir_sync_splice_body_scan`] admits whose calls are `holdsLock(this)`
/// (or, since round 14 wave 6, SS8-1 a, same-receiver synchronized bodies
/// spliced in the window)? Such
/// a body leaves no call behind and traps nowhere, so the planner's replay
/// fence (`ir_splice_fence_refuses_site`) and its unbindable-call refusal
/// (`append_ir_inline_site`) have nothing to judge. `false` for a body with no
/// call at all (those rules already pass it) and for every other body.
pub(crate) fn ir_sync_splice_folds_every_call(site: &InlineSite) -> bool {
    // Round 14 wave 7 (lane sync7, SS8-5): a static body too, whose one
    // admitted call is `holdsLock(C.class)` of its own class (the scan admits
    // no other call in a static body).
    site.callee_is_synchronized
        && (!site.callee_is_static || ir::ir_sync_splice_self_class_holds_lock_enabled())
        && !site.invoke_targets.is_empty()
        && ir::ir_sync_splice_enabled()
        && ir_sync_splice_body_scan(site).is_ok()
}



/// Round 14 wave 3 (lane sync): the planner halves of SS-2 and SS-5.










/// Can an instruction that may trap run, inside this body, AFTER a call that
/// stays a call (round 11 wave 16, lane `replay`)?
///
/// Every trap inside a splice deopts to the OUTERMOST `invoke` with
/// REEXECUTE, which runs a call that already returned a second time
/// (`r11w15-ir-splice-replay-reruns-a-leaf-call`). The builder refuses the
/// whole METHOD when it meets that shape (`IrBuilder::splice_call_committed`).
/// This is the planner's cheaper answer for the shapes it can see: refuse the
/// SITE, so the method still compiles with that call left in place. It is a
/// heuristic in both directions -- the builder stays the soundness gate.
///
/// A "call" is an invoke that is neither spliced (a nested site; a self
/// copy's leaf is one too, and is replay-safe) nor elided, or a nested site
/// whose own subtree leaves such a call behind ([`ir_splice_tree_leaves_a_call`]).
/// After one, only [`ir_splice_opcode_cannot_trap`] opcodes may follow in pc
/// order. A loop that brings a trap round after the call needs a branch after
/// it, which that list refuses.
pub(crate) fn ir_splice_body_may_trap_after_a_call(
    site: &InlineSite,
    self_rec: Option<&ir::IrSelfRecursion>,
) -> bool {
    let code = &site.callee_code;
    let len = site.callee_code_len.min(code.len());
    let mut call_seen = false;
    let mut store_seen = false;
    let mut pc = 0usize;
    while pc < len {
        let Some(&op) = code.get(pc) else {
            return true;
        };
        // nio21 wave 11, lane L3: a constant-word lambda row neither traps
        // nor commits (`ir::IrBuilder::spliced_constant_indy_at`).
        let constant_indy = op == 0xba && site.ir_indy_rows.iter().any(|r| r.0 == pc && r.1 == 0);
        if call_seen && !ir_splice_opcode_cannot_trap(op) && !constant_indy {
            return true;
        }
        // j27 wave 4 (lane SPLICECAT): a concat row is a call (operand
        // `toString()`s), and may not follow a committed store.
        let Some(concat) =
            splice_concat_bridge::planner_concat_step(site, pc, op, &mut store_seen)
        else {
            return true;
        };
        call_seen |= concat;
        if matches!(op, 0xb6..=0xb9) {
            let nested = site.nested_sites.iter().find(|n| n.callee_pc == pc);
            call_seen |= match nested {
                Some(n) => ir_splice_nested_leaves_a_call(n, self_rec),
                None => !site.elided_invoke_pcs.contains(&pc),
            };
        }
        pc += bytecode_analysis::step(code, pc).max(1);
    }
    false
}

/// The opcodes `IrBuilder::admits_after_committed_splice_store` admits
/// whatever the operand stack holds: constants (the resolver admits only an
/// immediate `ldc` in a spliced body), local loads and stores, `pop`/`dup`,
/// non-dividing arithmetic, conversions, compares, and the returns.
pub(crate) fn ir_splice_opcode_cannot_trap(op: u8) -> bool {
    matches!(op, 0x00..=0x2d | 0x36..=0x4e | 0x57 | 0x59 | 0x60..=0x6b | 0x74..=0x98 | 0xac..=0xb1)
}

/// Does splicing nested site `n` (under the self-recursion plan `self_rec`)
/// leave a call behind anywhere in its subtree? A self copy's LEAF does not
/// count: its replay commits nothing observable (see
/// `IrBuilder::is_self_copy_leaf_call`).
pub(crate) fn ir_splice_nested_leaves_a_call(
    n: &NestedInlineSite,
    self_rec: Option<&ir::IrSelfRecursion>,
) -> bool {
    let deeper = match self_rec {
        None => None,
        Some(plan) => match plan.classify(
            &n.site.class_name,
            &n.site.method_name,
            &n.site.descriptor,
            n.site.class_id,
            n.site.callee_is_static,
        ) {
            ir::IrSelfSplice::NotSelf => Some(plan.clone()),
            ir::IrSelfSplice::Splice(deeper) => Some(deeper),
            ir::IrSelfSplice::Leaf(_) => return false,
        },
    };
    ir_splice_tree_leaves_a_call(&n.site, deeper.as_ref())
}

/// Does this body, or any body spliced into it, leave a call behind? See
/// [`ir_splice_nested_leaves_a_call`].
pub(crate) fn ir_splice_tree_leaves_a_call(site: &InlineSite, plan: Option<&ir::IrSelfRecursion>) -> bool {
    // j27 wave 4 (lane SPLICECAT): a concat row stays a bridge call.
    if splice_concat_bridge::site_has_concat_rows(site) {
        return true;
    }
    let code = &site.callee_code;
    let len = site.callee_code_len.min(code.len());
    let mut pc = 0usize;
    while pc < len {
        if matches!(code.get(pc), Some(0xb6..=0xb9)) {
            let leaves = match site.nested_sites.iter().find(|n| n.callee_pc == pc) {
                Some(n) => ir_splice_nested_leaves_a_call(n, plan),
                None => !site.elided_invoke_pcs.contains(&pc),
            };
            if leaves {
                return true;
            }
        }
        pc += bytecode_analysis::step(code, pc).max(1);
    }
    false
}

/// Does the replay fence refuse this site (round 12 wave 5, lane iropt4)?
///
/// [`ir_splice_body_may_trap_after_a_call`] first: a site it admits is
/// admitted, whatever `relax` says. A site it refuses is admitted after all
/// only when every relaxation it needs is on and
/// [`ir_splice_relaxed_fence_admits`] can show the builder will admit the
/// body. `relax` is `ir::SpliceFenceRelax::NONE` unless the OUTERMOST site's
/// whole tree passed [`ir_splice_tree_admits_fence_relaxation`], so with every
/// switch off, or for any tree that writes a field or has a loop, this is the
/// round-11 answer exactly.
pub(crate) fn ir_splice_fence_refuses_site(
    site: &InlineSite,
    self_rec: Option<&ir::IrSelfRecursion>,
    relax: ir::SpliceFenceRelax,
) -> bool {
    // Round 14 wave 4 (lane sync4, SS-7): its one call is folded, and nothing
    // in it can trap.
    if ir_sync_splice_folds_every_call(site) {
        return false;
    }
    if !ir_splice_body_may_trap_after_a_call(site, self_rec) {
        return false;
    }
    !(relax.any() && ir_splice_relaxed_fence_admits(site, self_rec, relax) == Some(true))
}

/// May the fence relaxations apply anywhere in this outermost site's tree
/// (round 12 wave 5, lane iropt4)? Only when no body in it writes a field and
/// none has a backward branch.
///
/// Both clauses keep the planner from admitting a body the builder then
/// refuses, which would cost the WHOLE method its compile. The builder applies
/// the relaxations only to a fence a call raised, never to one a committed
/// store raised, and the planner cannot tell a splice-local store from a
/// committed one. And the builder admits a forward branch only while every open
/// splice is walked in pc order, which a body with a rotated loop is not; this
/// planner also models only loop-free bodies.
pub(crate) fn ir_splice_tree_admits_fence_relaxation(site: &InlineSite) -> bool {
    let code = &site.callee_code;
    let len = site.callee_code_len.min(code.len());
    if !bytecode_analysis::back_edges(code, len).is_empty() {
        return false;
    }
    let mut pc = 0usize;
    while pc < len {
        if code.get(pc) == Some(&0xb5) {
            return false;
        }
        pc += bytecode_analysis::step(code, pc).max(1);
    }
    site.nested_sites
        .iter()
        .all(|n| ir_splice_tree_admits_fence_relaxation(&n.site))
}

/// One abstract frame of [`ir_splice_relaxed_fence_admits`]: an id per
/// operand-stack entry (one per VALUE, the way `IrBuilder` counts them) and
/// per local slot (0 = nothing known), and the ids a dominating null test
/// proved non-null in the current region.
#[derive(Clone)]
pub(crate) struct IrFenceWalkState {
    pub(crate) stack: Vec<u32>,
    pub(crate) locals: Vec<u32>,
    pub(crate) facts: Vec<u32>,
}

/// The planner half of the relaxed replay fence (round 12 wave 5, lane
/// iropt4; the builder half and the proofs are `ir::SpliceFenceRelax`).
///
/// An abstract walk of ONE body in pc order that tracks which values the
/// builder will be able to prove non-null, and answers whether every
/// instruction after the first surviving call is one the builder's
/// `admits_after_committed_splice_store` admits under `relax`:
/// `Some(true)` admitted, `Some(false)` refused, `None` when the walk cannot
/// model the body (a back edge, a switch, `pop2`, a monitor, a stack-depth
/// disagreement at a join, ...), which keeps the round-11 answer.
///
/// It must never admit what the builder refuses (that costs the method its
/// compile, not the site its splice), so every fact here is one the builder
/// also has:
///
/// * an id made by `new` is the builder's `Op::New` (`definitely_non_null`);
/// * the body's own receiver is the splice frame's receiver (relaxation 2);
/// * a null fact starts where the builder's does (the fall-through of
///   `ifnull v`, the target of `ifnonnull v` when that edge is the only way
///   in) and is dropped wherever the builder drops it (every branch target,
///   which is a builder merge, and every nested splice's entry and exit);
/// * a join keeps a slot's id only when every edge agrees, as
///   `activate_merge` keeps a node only when no phi is needed.
///
/// A fact the builder has and this walk does not (a caller argument that is an
/// allocation, an enclosing splice's receiver, a String constant) only makes
/// the planner stricter.
pub(crate) fn ir_splice_relaxed_fence_admits(
    site: &InlineSite,
    self_rec: Option<&ir::IrSelfRecursion>,
    relax: ir::SpliceFenceRelax,
) -> Option<bool> {
    fn fresh(next: &mut u32) -> u32 {
        let id = *next;
        *next = next.saturating_add(1);
        id
    }
    fn pop(s: &mut IrFenceWalkState, n: usize) -> Option<()> {
        let keep = s.stack.len().checked_sub(n)?;
        s.stack.truncate(keep);
        Some(())
    }
    fn set_local(s: &mut IrFenceWalkState, idx: usize, id: u32) -> Option<()> {
        *s.locals.get_mut(idx)? = id;
        Some(())
    }
    // A long/double store also ends whatever the slot above it held.
    fn store(s: &mut IrFenceWalkState, idx: usize, wide: bool) -> Option<()> {
        let v = s.stack.pop()?;
        set_local(s, idx, v)?;
        if wide {
            set_local(s, idx.checked_add(1)?, 0)?;
        }
        Some(())
    }
    // The state at a join: one edge in keeps its frame (and its own fact, if
    // it is an `ifnonnull` taken edge), several keep what they agree on.
    fn join(
        preds: Vec<(IrFenceWalkState, Option<u32>)>,
        next: &mut u32,
    ) -> Option<Option<IrFenceWalkState>> {
        let mut preds = preds.into_iter();
        let Some((mut s, fact)) = preds.next() else {
            return Some(None);
        };
        s.facts.clear();
        let rest: Vec<(IrFenceWalkState, Option<u32>)> = preds.collect();
        if rest.is_empty() {
            s.facts.extend(fact);
            return Some(Some(s));
        }
        for (o, _) in &rest {
            if o.stack.len() != s.stack.len() || o.locals.len() != s.locals.len() {
                return None;
            }
        }
        for i in 0..s.stack.len() {
            if rest.iter().any(|(o, _)| o.stack.get(i) != s.stack.get(i)) {
                *s.stack.get_mut(i)? = fresh(next);
            }
        }
        for i in 0..s.locals.len() {
            if rest.iter().any(|(o, _)| o.locals.get(i) != s.locals.get(i)) {
                *s.locals.get_mut(i)? = fresh(next);
            }
        }
        Some(Some(s))
    }

    let code = &site.callee_code;
    let len = site.callee_code_len.min(code.len());
    // Every branch target (a builder merge), all forward.
    let mut targets: FxHashSet<usize> = FxHashSet::default();
    let mut pc = 0usize;
    while pc < len {
        let op = *code.get(pc)?;
        if bytecode_analysis::is_subroutine_op(op)
            || bytecode_analysis::is_wide_ret(code, pc)
            || matches!(op, 0xaa | 0xab)
        {
            return None;
        }
        if bytecode_analysis::is_offset_branch(op) {
            let t = bytecode_analysis::offset_branch_target(code, pc)?;
            if t <= pc || t >= len {
                return None;
            }
            targets.insert(t);
        }
        pc += bytecode_analysis::step(code, pc).max(1);
    }

    let mut next: u32 = 1;
    let mut allocs: FxHashSet<u32> = FxHashSet::default();
    let mut entry = IrFenceWalkState {
        stack: Vec::new(),
        locals: vec![0; site.callee_max_locals],
        facts: Vec::new(),
    };
    let (arg_slots, _) = compute_param_jvm_slots(&site.descriptor, site.callee_is_static);
    for &slot in &arg_slots {
        let id = fresh(&mut next);
        set_local(&mut entry, slot, id)?;
    }
    let receiver: Option<u32> = if site.callee_is_static {
        None
    } else {
        entry.locals.first().copied().filter(|&id| id != 0)
    };
    let mut pending: HashMap<usize, Vec<(IrFenceWalkState, Option<u32>)>> = HashMap::new();
    let mut cur: Option<IrFenceWalkState> = Some(entry);
    let mut call_seen = false;
    let mut pc = 0usize;
    while pc < len {
        let op = *code.get(pc)?;
        if targets.contains(&pc) {
            let mut preds = pending.remove(&pc).unwrap_or_default();
            if let Some(s) = cur.take() {
                preds.push((s, None));
            }
            cur = join(preds, &mut next)?;
        }
        // Unreachable in pc order: the builder refuses such a body.
        let s = cur.as_mut()?;
        let proven = |s: &IrFenceWalkState, depth: usize| -> bool {
            s.stack
                .len()
                .checked_sub(depth + 1)
                .and_then(|i| s.stack.get(i))
                .is_some_and(|&id| {
                    id != 0
                        && (allocs.contains(&id)
                            || (relax.null_facts
                                && (receiver == Some(id) || s.facts.contains(&id))))
                })
        };
        let nested = site.nested_sites.iter().find(|n| n.callee_pc == pc);
        let target_row = site.invoke_targets.iter().find(|(tpc, _)| *tpc == pc);
        let elided = site.elided_invoke_pcs.contains(&pc);
        if call_seen && !ir_splice_opcode_cannot_trap(op) {
            let admitted = match op {
                // Every target was checked forward above.
                0x99..=0xa7 | 0xc6..=0xc8 => relax.forward_branch,
                0xb4 => proven(s, 0),
                0xb5 => proven(s, 1),
                0xb6 | 0xb7 => {
                    relax.guarded_call
                        && nested.is_none()
                        && !elided
                        && match target_row {
                            Some((_, t)) if matches!(t.invoke_kind, 0 | 1) => {
                                static_call_shape(&t.descriptor)
                                    .is_some_and(|(args, _)| proven(s, args))
                            }
                            _ => false,
                        }
                }
                _ => false,
            };
            if !admitted {
                return Some(false);
            }
        }
        match op {
            0x00 => {}
            // aconst_null, the constants, bipush, sipush, ldc
            0x01..=0x14 => s.stack.push(fresh(&mut next)),
            // iload .. aload <idx>
            0x15..=0x19 => {
                let idx = usize::from(*code.get(pc + 1)?);
                let v = *s.locals.get(idx)?;
                s.stack.push(v);
            }
            // iload_0 .. aload_3
            0x1a..=0x2d => {
                let v = *s.locals.get(usize::from((op - 0x1a) & 3))?;
                s.stack.push(v);
            }
            // array loads
            0x2e..=0x35 => {
                pop(s, 2)?;
                s.stack.push(fresh(&mut next));
            }
            // istore .. astore <idx>
            0x36..=0x3a => store(
                s,
                usize::from(*code.get(pc + 1)?),
                matches!(op, 0x37 | 0x39),
            )?,
            // istore_0 .. astore_3
            0x3b..=0x4e => store(
                s,
                usize::from((op - 0x3b) & 3),
                matches!(op, 0x3f..=0x42 | 0x47..=0x4a),
            )?,
            // array stores
            0x4f..=0x56 => pop(s, 3)?,
            // pop
            0x57 => pop(s, 1)?,
            // dup
            0x59 => {
                let v = *s.stack.last()?;
                s.stack.push(v);
            }
            // dup_x1
            0x5a => {
                let v1 = s.stack.pop()?;
                let v2 = s.stack.pop()?;
                s.stack.extend([v1, v2, v1]);
            }
            // binary arithmetic, shifts, logic, compares
            0x60..=0x73 | 0x78..=0x83 | 0x94..=0x98 => {
                pop(s, 2)?;
                s.stack.push(fresh(&mut next));
            }
            // negation, conversions
            0x74..=0x77 | 0x85..=0x93 => {
                pop(s, 1)?;
                s.stack.push(fresh(&mut next));
            }
            // iinc
            0x84 => {
                let id = fresh(&mut next);
                set_local(s, usize::from(*code.get(pc + 1)?), id)?;
            }
            // if<cond>, if_icmp<cond>, if_acmp<cond>
            0x99..=0xa6 => {
                pop(s, if op <= 0x9e { 1 } else { 2 })?;
                let t = bytecode_analysis::offset_branch_target(code, pc)?;
                pending.entry(t).or_default().push((s.clone(), None));
            }
            // ifnull: `v` is non-null on the fall-through
            0xc6 => {
                let v = s.stack.pop()?;
                let t = bytecode_analysis::offset_branch_target(code, pc)?;
                pending.entry(t).or_default().push((s.clone(), None));
                if relax.null_facts {
                    s.facts.push(v);
                }
            }
            // ifnonnull: `v` is non-null on the taken edge
            0xc7 => {
                let v = s.stack.pop()?;
                let t = bytecode_analysis::offset_branch_target(code, pc)?;
                let fact = relax.null_facts.then_some(v);
                pending.entry(t).or_default().push((s.clone(), fact));
            }
            // goto, goto_w
            0xa7 | 0xc8 => {
                let t = bytecode_analysis::offset_branch_target(code, pc)?;
                if let Some(s) = cur.take() {
                    pending.entry(t).or_default().push((s, None));
                }
            }
            // returns, athrow
            0xac..=0xb1 | 0xbf => {
                cur = None;
            }
            // getstatic
            0xb2 => s.stack.push(fresh(&mut next)),
            // putstatic
            0xb3 => pop(s, 1)?,
            // getfield
            0xb4 => {
                pop(s, 1)?;
                s.stack.push(fresh(&mut next));
            }
            // putfield
            0xb5 => pop(s, 2)?,
            // the invokes
            0xb6..=0xb9 => {
                let (desc, is_static) = if let Some(n) = nested {
                    (n.site.descriptor.as_str(), n.site.callee_is_static)
                } else if let Some((_, t)) = target_row {
                    (t.descriptor.as_str(), t.invoke_kind == 3)
                } else if op == 0xb7 && elided {
                    ("()V", false)
                } else {
                    return None;
                };
                let (args, ret) = static_call_shape(desc)?;
                pop(s, args + usize::from(!is_static))?;
                if ret != b'V' {
                    s.stack.push(fresh(&mut next));
                }
                // The builder drops every fact at a splice's entry and exit.
                if nested.is_some() {
                    s.facts.clear();
                }
                call_seen |= match nested {
                    Some(n) => ir_splice_nested_leaves_a_call(n, self_rec),
                    None => !elided,
                };
            }
            // new
            0xbb => {
                let id = fresh(&mut next);
                allocs.insert(id);
                s.stack.push(id);
            }
            // newarray, anewarray, arraylength, checkcast, instanceof
            0xbc | 0xbd | 0xbe | 0xc0 | 0xc1 => {
                pop(s, 1)?;
                s.stack.push(fresh(&mut next));
            }
            // wide load / store / iinc
            0xc4 => {
                let w = *code.get(pc + 1)?;
                let idx = usize::from(u16::from_be_bytes([*code.get(pc + 2)?, *code.get(pc + 3)?]));
                match w {
                    0x15..=0x19 => {
                        let v = *s.locals.get(idx)?;
                        s.stack.push(v);
                    }
                    0x36..=0x3a => store(s, idx, matches!(w, 0x37 | 0x39))?,
                    0x84 => {
                        let id = fresh(&mut next);
                        set_local(s, idx, id)?;
                    }
                    _ => return None,
                }
            }
            // pop2, the category-dependent dups, swap, monitors,
            // multianewarray, invokedynamic, ...
            _ => return None,
        }
        pc += bytecode_analysis::step(code, pc).max(1);
    }
    Some(true)
}

/// JIT round 21 (lane staticfwd, FG16-3): may `site`'s body run an
/// instruction twice -- a backward branch, a switch (its targets are not
/// checked), a subroutine, or bytes the walk cannot step? Conservative: `true`
/// for anything it cannot read.
pub(crate) fn ir_splice_body_may_loop(site: &InlineSite) -> bool {
    let code = &site.callee_code;
    let len = site.callee_code_len.min(code.len());
    let mut pc = 0usize;
    while pc < len {
        let Some(&op) = code.get(pc) else {
            return true;
        };
        if matches!(op, 0xaa | 0xab)
            || bytecode_analysis::is_subroutine_op(op)
            || bytecode_analysis::is_wide_ret(code, pc)
        {
            return true;
        }
        if bytecode_analysis::is_offset_branch(op)
            && bytecode_analysis::offset_branch_target(code, pc).is_none_or(|t| t <= pc)
        {
            return true;
        }
        let step = bytecode_analysis::step(code, pc);
        if step == 0 {
            return true;
        }
        pc += step;
    }
    false
}



/// Does the builder's COMMITTED-STORE fence certainly refuse this OUTERMOST
/// site's body (round 12 wave 6, lane iropt5, proposal W5-1; page
/// `r12w5-iropt4-planner-does-not-model-the-store-fence-FIXED-20260926.md`)?
///
/// The builder raises the replay fence for a `putfield` into an object the
/// splice did not allocate and that is not an open constructor receiver
/// (`IrBuilder::splice_committed_bases`), and from then to the end of the
/// outermost splice admits only the round-11 list: what
/// [`ir_splice_opcode_cannot_trap`] names, and a `getfield`/`putfield` through
/// a base it can prove non-null (a committed base or an allocation). Anything
/// else refuses the WHOLE method. The wave-5 planner modelled only the call
/// half ([`ir_splice_body_may_trap_after_a_call`]), so `n += k; other.n++;` on
/// an exact receiver was admitted as a site and then cost the method.
///
/// This answers `true` only when the walk shows the builder will refuse, so a
/// site it refuses is one the builder would have refused the method over. The
/// approximations all lean the other way (a missed refusal, which the builder
/// still catches and the W5-2 rebuild then turns into a lost splice):
///
/// * a store COMMITS only through a base that is certainly caller-visible: a
///   parameter of this body, or a value read (`getfield`, `aaload`,
///   `getstatic`) through one. The builder commits exactly those (a top-level
///   body's arguments are caller nodes, never `splice_local`), except the
///   receiver of an `<init>` body, which may be the caller's own `new`
///   (`splice_store_is_ctor_receiver`) and so is not counted. Anything else --
///   an allocation of this body, a value read through one, a call or nested
///   splice's result, a join of different values -- might be splice-local and
///   is not counted;
/// * after the commit, a base counts as PROVEN unless it is certainly
///   caller-visible, not yet written through, and not the body's receiver.
///   The builder proves an argument only when the caller passed an `Op::New`
///   (for the receiver, the exact-allocation receivers this matters most for),
///   so a caller-visible non-receiver argument is unproven for it too, except
///   when the caller allocated it in place (`t.add(1, new Tally())`), the one
///   case this can refuse that the builder admits;
/// * nested bodies are not walked: a store they commit is left to the builder.
///   An invoke of ANY kind after a commit refuses, as it does in the builder
///   (its fence check runs at the `invoke` before any splice opens).
///
/// A body the walk cannot model (a back edge, a switch, `pop2`, a category-2
/// `dup`, a monitor, `invokedynamic`, code unreachable in pc order) answers
/// `false`: the wave-5 answer.
///
/// Round 14 wave 3 (lane sync, SS-5): `false` for a synchronized body the
/// splice scan admits ([`ir_sync_splice_skips_store_fence`]); the builder
/// fences nothing inside a synchronized splice.
pub(crate) fn ir_splice_store_fence_refuses_site(site: &InlineSite, committed_store_admitted: bool) -> bool {
    if ir_sync_splice_skips_store_fence(site, committed_store_admitted) {
        return false;
    }
    ir_splice_store_fence_walk(site, committed_store_admitted) == Some(true)
}

/// The walk behind [`ir_splice_store_fence_refuses_site`]: `Some(true)` the
/// store fence refuses the body, `Some(false)` it admits it (or is never
/// raised), `None` the body cannot be modelled.
pub(crate) fn ir_splice_store_fence_walk(site: &InlineSite, committed_store_admitted: bool) -> Option<bool> {
    #[derive(Clone)]
    struct Frame {
        stack: Vec<u32>,
        locals: Vec<u32>,
    }
    fn fresh(next: &mut u32) -> u32 {
        let id = *next;
        *next = next.saturating_add(1);
        id
    }
    fn pop(s: &mut Frame, n: usize) -> Option<()> {
        let keep = s.stack.len().checked_sub(n)?;
        s.stack.truncate(keep);
        Some(())
    }
    fn at_depth(s: &Frame, depth: usize) -> Option<u32> {
        let i = s.stack.len().checked_sub(depth + 1)?;
        s.stack.get(i).copied()
    }
    fn set_local(s: &mut Frame, idx: usize, id: u32) -> Option<()> {
        *s.locals.get_mut(idx)? = id;
        Some(())
    }
    fn store(s: &mut Frame, idx: usize, wide: bool) -> Option<()> {
        let v = s.stack.pop()?;
        set_local(s, idx, v)?;
        if wide {
            set_local(s, idx.checked_add(1)?, 0)?;
        }
        Some(())
    }
    // Several edges into a join keep an entry only where they all agree; a
    // disagreement is a φ, which is never certainly caller-visible.
    fn join(preds: Vec<Frame>, next: &mut u32) -> Option<Option<Frame>> {
        let mut preds = preds.into_iter();
        let Some(mut s) = preds.next() else {
            return Some(None);
        };
        for o in preds {
            if o.stack.len() != s.stack.len() || o.locals.len() != s.locals.len() {
                return None;
            }
            for (mine, theirs) in s.stack.iter_mut().zip(o.stack.iter()) {
                if *mine != *theirs {
                    *mine = fresh(next);
                }
            }
            for (mine, theirs) in s.locals.iter_mut().zip(o.locals.iter()) {
                if *mine != *theirs {
                    *mine = fresh(next);
                }
            }
        }
        Some(Some(s))
    }

    let code = &site.callee_code;
    let len = site.callee_code_len.min(code.len());
    // Every branch target, all forward: with no back edge, pc order is the
    // builder's walk order (it walks a body with a rotated loop by blocks).
    let mut targets: FxHashSet<usize> = FxHashSet::default();
    let mut pc = 0usize;
    while pc < len {
        let op = *code.get(pc)?;
        if bytecode_analysis::is_subroutine_op(op)
            || bytecode_analysis::is_wide_ret(code, pc)
            || matches!(op, 0xaa | 0xab)
        {
            return None;
        }
        if bytecode_analysis::is_offset_branch(op) {
            let t = bytecode_analysis::offset_branch_target(code, pc)?;
            if t <= pc || t >= len {
                return None;
            }
            targets.insert(t);
        }
        pc += bytecode_analysis::step(code, pc).max(1);
    }

    let mut next: u32 = 1;
    // Certainly caller-visible values, and the committed bases among them.
    let mut caller: FxHashSet<u32> = FxHashSet::default();
    let mut committed: FxHashSet<u32> = FxHashSet::default();
    let mut entry = Frame {
        stack: Vec::new(),
        locals: vec![0; site.callee_max_locals],
    };
    let (arg_slots, _) = compute_param_jvm_slots(&site.descriptor, site.callee_is_static);
    for &slot in &arg_slots {
        let id = fresh(&mut next);
        caller.insert(id);
        set_local(&mut entry, slot, id)?;
    }
    let receiver: Option<u32> = if site.callee_is_static {
        None
    } else {
        entry.locals.first().copied().filter(|&id| id != 0)
    };
    let is_init = !site.callee_is_static && site.method_name == "<init>";
    // Round 16 (lane sync, SY16-2): the builder's own condition, read the
    // same way (relaxation 1 on, and the store-fence switch on).
    let store_fence_forward_branch = ir::SpliceFenceRelax::from_flags().forward_branch
        && ir::ir_splice_store_fence_forward_branch_enabled();
    let mut pending: HashMap<usize, Vec<Frame>> = HashMap::new();
    let mut cur: Option<Frame> = Some(entry);
    let mut store_committed = false;
    // JIT round 21 (lane staticfwd, FG16-3): the classes whose `putstatic`
    // this body committed (the builder's `splice_static_access_cannot_trap`).
    let mut committed_static: FxHashSet<u32> = FxHashSet::default();
    let static_class_at = |pc: usize| {
        site.static_field_info
            .iter()
            .find(|row| row.0 == pc)
            .map(|row| row.1)
            .filter(|&c| c != 0)
    };
    let mut pc = 0usize;
    while pc < len {
        let op = *code.get(pc)?;
        if targets.contains(&pc) {
            let mut preds = pending.remove(&pc).unwrap_or_default();
            if let Some(s) = cur.take() {
                preds.push(s);
            }
            cur = join(preds, &mut next)?;
        }
        let s = cur.as_mut()?;
        // Unproven for the fence: certainly caller-visible, not yet written
        // through, and not the receiver (see the doc comment).
        let unproven =
            |id: u32| caller.contains(&id) && !committed.contains(&id) && receiver != Some(id);
        if store_committed && !ir_splice_opcode_cannot_trap(op) {
            let admitted = match op {
                0xb4 => at_depth(s, 0).is_some_and(|b| !unproven(b)),
                0xb5 => at_depth(s, 1).is_some_and(|b| !unproven(b)),
                // Round 16 (lane sync, SY16-2): the builder's relaxation 1
                // after a committed store. Every branch of a body this walk
                // models is forward (a back edge answers `None` above) and
                // inside it, so the builder admits each one it reaches in pc
                // order (`IrBuilder::splice_branch_is_forward`).
                0x99..=0xa7 | 0xc6..=0xc8 => store_fence_forward_branch,
                // JIT round 21 (lane staticfwd, FG16-3): a static access of a
                // class whose write this body committed cannot trap. (The
                // builder also admits a class the compile's resolver reported
                // initialised, which this walk cannot see: a refusal here it
                // would have admitted costs a splice, never a method.)
                0xb2 | 0xb3 => {
                    ir::ir_splice_putstatic_enabled()
                        && static_class_at(pc).is_some_and(|c| committed_static.contains(&c))
                }
                _ => false,
            };
            if !admitted {
                return Some(true);
            }
        }
        match op {
            0x00 => {}
            // aconst_null, the constants, bipush, sipush, ldc
            0x01..=0x14 => s.stack.push(fresh(&mut next)),
            // iload .. aload <idx>
            0x15..=0x19 => {
                let v = *s.locals.get(usize::from(*code.get(pc + 1)?))?;
                s.stack.push(v);
            }
            // iload_0 .. aload_3
            0x1a..=0x2d => {
                let v = *s.locals.get(usize::from((op - 0x1a) & 3))?;
                s.stack.push(v);
            }
            // aaload: an element of a caller-visible array is caller-visible
            0x32 => {
                let array = at_depth(s, 1)?;
                pop(s, 2)?;
                let id = fresh(&mut next);
                if caller.contains(&array) {
                    caller.insert(id);
                }
                s.stack.push(id);
            }
            // the other array loads
            0x2e..=0x35 => {
                pop(s, 2)?;
                s.stack.push(fresh(&mut next));
            }
            // istore .. astore <idx>
            0x36..=0x3a => store(
                s,
                usize::from(*code.get(pc + 1)?),
                matches!(op, 0x37 | 0x39),
            )?,
            // istore_0 .. astore_3
            0x3b..=0x4e => store(
                s,
                usize::from((op - 0x3b) & 3),
                matches!(op, 0x3f..=0x42 | 0x47..=0x4a),
            )?,
            // array stores
            0x4f..=0x56 => pop(s, 3)?,
            // pop
            0x57 => pop(s, 1)?,
            // dup
            0x59 => {
                let v = *s.stack.last()?;
                s.stack.push(v);
            }
            // dup_x1
            0x5a => {
                let v1 = s.stack.pop()?;
                let v2 = s.stack.pop()?;
                s.stack.extend([v1, v2, v1]);
            }
            // swap
            0x5f => {
                let v1 = s.stack.pop()?;
                let v2 = s.stack.pop()?;
                s.stack.extend([v1, v2]);
            }
            // binary arithmetic (dividing forms included), shifts, logic, compares
            0x60..=0x73 | 0x78..=0x83 | 0x94..=0x98 => {
                pop(s, 2)?;
                s.stack.push(fresh(&mut next));
            }
            // negation, conversions
            0x74..=0x77 | 0x85..=0x93 => {
                pop(s, 1)?;
                s.stack.push(fresh(&mut next));
            }
            // iinc
            0x84 => {
                let id = fresh(&mut next);
                set_local(s, usize::from(*code.get(pc + 1)?), id)?;
            }
            // if<cond>, ifnull, ifnonnull; if_icmp<cond>, if_acmp<cond>
            0x99..=0xa6 | 0xc6 | 0xc7 => {
                pop(s, if matches!(op, 0x9f..=0xa6) { 2 } else { 1 })?;
                let t = bytecode_analysis::offset_branch_target(code, pc)?;
                pending.entry(t).or_default().push(s.clone());
            }
            // goto, goto_w
            0xa7 | 0xc8 => {
                let t = bytecode_analysis::offset_branch_target(code, pc)?;
                if let Some(s) = cur.take() {
                    pending.entry(t).or_default().push(s);
                }
            }
            // returns, athrow
            0xac..=0xb1 | 0xbf => cur = None,
            // getstatic: a caller-visible value
            0xb2 => {
                let id = fresh(&mut next);
                caller.insert(id);
                s.stack.push(id);
            }
            // putstatic. JIT round 21 (lane staticfwd, FG16-3): a static is
            // always caller-visible, so the write commits, as the builder's
            // `0xb3` arm does (a synchronized body never reaches this walk).
            0xb3 => {
                pop(s, 1)?;
                if ir::ir_splice_putstatic_enabled() {
                    if !committed_store_admitted {
                        // `CRATONVM_JIT_IR_SPLICE_COMMITTED_STORE=0`: the
                        // builder refuses the store itself.
                        return Some(true);
                    }
                    store_committed = true;
                    if let Some(c) = static_class_at(pc) {
                        committed_static.insert(c);
                    }
                }
            }
            // getfield: a field of a caller-visible object is caller-visible
            0xb4 => {
                let base = s.stack.pop()?;
                let id = fresh(&mut next);
                if caller.contains(&base) {
                    caller.insert(id);
                }
                s.stack.push(id);
            }
            // putfield: the value on top, the base under it
            0xb5 => {
                let base = at_depth(s, 1)?;
                pop(s, 2)?;
                if caller.contains(&base) && !(is_init && receiver == Some(base)) {
                    if !committed_store_admitted {
                        // `CRATONVM_JIT_IR_SPLICE_COMMITTED_STORE=0`: the
                        // builder refuses the store itself.
                        return Some(true);
                    }
                    store_committed = true;
                    committed.insert(base);
                }
            }
            // the invokes (after a commit they refused above)
            0xb6..=0xb9 => {
                let nested = site.nested_sites.iter().find(|n| n.callee_pc == pc);
                let target_row = site.invoke_targets.iter().find(|(tpc, _)| *tpc == pc);
                let (desc, is_static) = if let Some(n) = nested {
                    (n.site.descriptor.as_str(), n.site.callee_is_static)
                } else if let Some((_, t)) = target_row {
                    (t.descriptor.as_str(), t.invoke_kind == 3)
                } else if op == 0xb7 && site.elided_invoke_pcs.contains(&pc) {
                    ("()V", false)
                } else {
                    return None;
                };
                let (args, ret) = static_call_shape(desc)?;
                pop(s, args + usize::from(!is_static))?;
                if ret != b'V' {
                    s.stack.push(fresh(&mut next));
                }
            }
            // new, newarray, anewarray: this body's allocations
            0xbb => s.stack.push(fresh(&mut next)),
            0xbc | 0xbd | 0xbe | 0xc1 => {
                pop(s, 1)?;
                s.stack.push(fresh(&mut next));
            }
            // checkcast: the same object
            0xc0 => {}
            // wide load / store / iinc
            0xc4 => {
                let w = *code.get(pc + 1)?;
                let idx = usize::from(u16::from_be_bytes([*code.get(pc + 2)?, *code.get(pc + 3)?]));
                match w {
                    0x15..=0x19 => {
                        let v = *s.locals.get(idx)?;
                        s.stack.push(v);
                    }
                    0x36..=0x3a => store(s, idx, matches!(w, 0x37 | 0x39))?,
                    0x84 => {
                        let id = fresh(&mut next);
                        set_local(s, idx, id)?;
                    }
                    _ => return None,
                }
            }
            // pop2, the category-dependent dups, monitors, multianewarray,
            // invokedynamic, ...
            _ => return None,
        }
        pc += bytecode_analysis::step(code, pc).max(1);
    }
    Some(false)
}





/// How many extra builds one compile may spend dropping splices the builder
/// refused in. Each drops one more top-level site, so this bounds the
/// compile-time cost of a planner/builder disagreement at three builds.
pub(crate) const IR_SPLICE_REBUILD_MAX: usize = 2;

/// The top-level site (its caller pc) a refused build should be retried
/// without, from the pc the builder blamed (`IrBuilder::set_splice_refusal_sink`)
/// and the plan's `(start, end, invoke pc)` spliced ranges. A blamed pc below
/// `code_len` names a caller site directly; one at or past it is inside the
/// relocated body of the range that holds it (every nested body is appended
/// inside its top-level site's range). `None` when nothing is blamed or the pc
/// is in no range.
pub(crate) fn ir_splice_rebuild_site(
    blamed: usize,
    code_len: usize,
    ranges: &[(usize, usize, usize)],
) -> Option<usize> {
    if blamed == ir::SPLICE_REFUSAL_NONE {
        return None;
    }
    if blamed < code_len {
        return ranges
            .iter()
            .find(|&&(_, _, site)| site == blamed)
            .map(|&(_, _, site)| site);
    }
    ranges
        .iter()
        .find(|&&(start, end, _)| blamed >= start && blamed < end)
        .map(|&(_, _, site)| site)
}




/// May this callee body be spliced as a bounded self-recursive COPY (round 11
/// wave 11, lane `calls`)? Only when it can neither trap nor call anything but
/// itself: locals, constants, stack shuffles, non-dividing arithmetic,
/// conversions, compares, branches, returns, and `invokestatic` of the method
/// itself (every other invoke target refuses).
///
/// Why so narrow. A copy's leaf calls make the spliced region impure
/// (`bytecode_commits_side_effect`), and inside an impure region any guard --
/// an array bounds or null check, a cast -- raises `splice_guard_seen`, which
/// refuses the WHOLE method's optimizing compile (its deopt would re-run the
/// outer invoke, and with it the recursion that already ran). A store into a
/// caller-visible object followed by the leaf call is refused the same way
/// (`admits_after_committed_splice_store`). Admitting such a body would turn a
/// method that compiles today, with direct self-calls, into one that does not
/// compile at all; refusing it here keeps that method exactly as it was.
/// `fib`, `tri`, Ackermann-style integer recursion pass.
///
/// Round 11 wave 15 (lane `fib`): this is the FAST route, and it is unchanged.
/// A body it refuses may still be spliced through the second admission,
/// [`ir_self_copy_body_replays_safely`], which is what binary-trees' two
/// methods need.
pub(crate) fn ir_self_copy_body_is_trap_free(site: &InlineSite) -> bool {
    let code = &site.callee_code;
    let len = site.callee_code_len.min(code.len());
    let mut pc = 0usize;
    while pc < len {
        let Some(&op) = code.get(pc) else {
            return false;
        };
        if !ir_self_copy_trap_free_opcode(site, len, pc, op) {
            return false;
        }
        pc += bytecode_analysis::step(code, pc).max(1);
    }
    site.invoke_targets
        .iter()
        .all(|(_, t)| ir_self_copy_target_is_self(site, t))
}

/// The opcode half of [`ir_self_copy_body_is_trap_free`], shared with
/// [`ir_self_copy_body_replays_safely`] so the two admissions cannot come to
/// disagree about what "cannot trap" means. `0xb8` is admitted here as an
/// opcode only: each caller checks every invoke target itself.
///
/// nio21 wave 12 (lane L4, W10L2-3): an integer division by a non-zero
/// constant pushed right before it ([`ir::splice_division_cannot_trap`], an
/// `ldc` divisor read from the resolver's rows, [`inline_site_ldc_integral`])
/// is admitted: the builder plants no guard for it, so it can neither trap
/// nor raise `splice_guard_seen` in an impure copy. A variable divisor stays
/// refused -- its guard is exactly what this admission's contract excludes.
pub(crate) fn ir_self_copy_trap_free_opcode(site: &InlineSite, len: usize, pc: usize, op: u8) -> bool {
    let code = &site.callee_code;
    match op {
        // nop, constants, bipush/sipush, ldc (the resolver admits only an
        // immediate), local loads
        0x00..=0x2d => true,
        // local stores, stack shuffles
        0x36..=0x5f => !(0x4f..=0x56).contains(&op),
        // add, sub, mul (no divide or remainder: `idiv`/`irem` trap, and
        // the FP forms are helper calls on some paths)
        0x60..=0x6b => true,
        // idiv / ldiv / irem / lrem by a non-zero constant (wave 12, above)
        0x6c | 0x6d | 0x70 | 0x71 => {
            ir::splice_division_cannot_trap(code, len, pc, |p| inline_site_ldc_integral(site, p))
        }
        // neg, shifts, and/or/xor, iinc, conversions, compares, the
        // conditional and unconditional branches, returns
        0x74..=0xa7 | 0xac..=0xb1 => true,
        // ifnull / ifnonnull / goto_w
        0xc6..=0xc8 => true,
        // wide (a widened load/store/iinc)
        0xc4 => code
            .get(pc + 1)
            .is_some_and(|&w| matches!(w, 0x15..=0x19 | 0x36..=0x3a | 0x84)),
        // the self-call; every target is checked by the caller
        0xb8 => true,
        _ => false,
    }
}

/// The integral value the resolver resolved for the `ldc` / `ldc_w` /
/// `ldc2_w` at callee pc `pc` of `site` (`ldc_info` / `ldc2w_info`); `None`
/// for a pc it tagged floating (`ldc_fp_pcs`) or never resolved (a `String`
/// / `Class` constant has no row). nio21 wave 12, lane L4.
pub(crate) fn inline_site_ldc_integral(site: &InlineSite, pc: usize) -> Option<i64> {
    if site.ldc_fp_pcs.contains(&pc) {
        return None;
    }
    site.ldc_info
        .iter()
        .chain(site.ldc2w_info.iter())
        .find(|&&(p, _)| p == pc)
        .map(|&(_, v)| v)
}

/// Is `t` the `invokestatic` of the method `site` is a copy of?
///
/// JIT round 21 wave 16 (lane jit-selfcopy, w15 review item 4): since wave 14
/// `site.class_name` / `site.class_id` name the DECLARING class, while
/// `t.class_name` is the constant-pool qualifier as written, so a copy of
/// `C.f` whose own call is `invokestatic Sub.f` (`Sub extends C`, `f` not
/// hidden) failed the name compare and the copy was refused
/// (`ir-splice-self-copy-may-trap`). Under [`ir_self_copy_by_id_enabled`] the
/// RESOLVED declaring class id is compared when both ids are known; either id
/// `0` falls back to the name compare. With the switch off the name compare
/// is the only rule, exactly as before.
pub(crate) fn ir_self_copy_target_is_self(site: &InlineSite, t: &InlineInvokeTarget) -> bool {
    if t.invoke_kind != 3 || t.method_name != site.method_name || t.descriptor != site.descriptor {
        return false;
    }
    if t.resolved_class_id != 0 && site.class_id != 0 && ir_self_copy_by_id_enabled() {
        return t.resolved_class_id == site.class_id;
    }
    t.class_name == site.class_name
}

/// Opt-in switch for the id compare in [`ir_self_copy_target_is_self`] and for
/// the VM resolver filling [`InlineInvokeTarget::resolved_class_id`] (JIT
/// round 21 wave 16, lane jit-selfcopy). **Default OFF**: it admits self copies
/// the name rule refused, so it changes which compiles happen.
/// `CRATONVM_JIT_IR_SELF_COPY_BY_ID=1` turns it on. Read per call, like
/// [`ir_self_copy_replay_safe_enabled`]; the VM resolver reads it once per
/// resolution.
pub fn ir_self_copy_by_id_enabled() -> bool {
    false
}



/// Kill switch for the second self-copy admission,
/// [`ir_self_copy_body_replays_safely`] (round 11 wave 15, lane `fib`).
/// **Default ON**; `CRATONVM_JIT_IR_RECURSIVE_INLINE_REPLAY_SAFE=0` restores
/// the wave-11 rule exactly (only [`ir_self_copy_body_is_trap_free`] bodies
/// are copied). Read per call, like [`ir::ir_recursive_inline_enabled`].
pub(crate) fn ir_self_copy_replay_safe_enabled() -> bool {
    true
}

/// May this self copy be spliced although it CAN trap, because re-running the
/// enclosing top-level `invoke` from scratch is observably the same as running
/// it once (round 11 wave 15, lane `fib`)?
///
/// This is the argument dev's binary-trees work (`df003056f`, `1513759d9`)
/// relied on without writing down, made a checked predicate. Every trap inside
/// a splice -- the implicit null check of a `getfield`, a failed allocation --
/// deopts to the OUTERMOST `invoke` with REEXECUTE (`ir_lower`'s
/// `resume_bci`), so the interpreter re-runs the whole call, including every
/// leaf self-call that already returned. That is harmless exactly when neither
/// the copy nor any execution of the method can commit an effect a second run
/// would repeat observably. The body is admitted when:
///
/// * every instruction is one [`ir_self_copy_trap_free_opcode`] admits, or a
///   `getfield` (a READ: a replay reads again, and the first read's value was
///   never published), or a `new` (a replay allocates a fresh object; the
///   first is unreachable garbage -- the same argument
///   `IrBuilder::splice_store_is_local` makes for splice-local allocations);
/// * every `invokestatic` is the method itself, and every `invokespecial` is
///   either an elided no-op constructor or a constructor spliced IN (a nested
///   site, so never a surviving call) whose body writes only its own receiver
///   ([`ir_splice_ctor_writes_only_its_receiver`]). The copy is a STATIC
///   method (`ir::IrSelfRecursion` is static-only), so by JVMS 4.10.1.9 every
///   `<init>` receiver in it is the `uninitialized` result of a `new` executed
///   by this very activation: those writes land in an object a replay
///   re-creates. No `putfield`, `putstatic`, array store, monitor, `athrow`,
///   division or other call is admitted in the copy itself.
///
/// The leaf self-calls are covered by induction on the call depth: they run
/// this same body, which by the two clauses above commits nothing but fresh
/// allocations and writes into them. So a replay of the outer `invoke` is a
/// second run of a computation whose only lasting effects were garbage.
///
/// Why the builder then keeps the method: neither admitted opcode builds an
/// `Op::Guard` (a `getfield`'s null check and an allocation failure are
/// lowering-time deopts, not guard nodes), a spliced constructor's receiver is
/// an `Op::New` and so needs no receiver guard, and every store the copy makes
/// is a `splice_store_is_ctor_receiver` store rather than a committed one --
/// so neither `splice_guard_seen` nor `admits_after_committed_splice_store`
/// can refuse the graph because of the copy. This is `itemCheck`
/// (`getfield`s) and `bottomUpTree` (`new Node(...)`) of
/// `tools/bench/BinTreesClassic.java`.
pub(crate) fn ir_self_copy_body_replays_safely(site: &InlineSite) -> bool {
    // `IrSelfRecursion::classify` already requires this; restated because the
    // `<init>`-receiver argument above holds only for a static method.
    if !site.callee_is_static {
        return false;
    }
    let is_ctor_target = |pc: usize, t: &InlineInvokeTarget| {
        t.invoke_kind == 1
            && t.method_name == "<init>"
            && site.nested_sites.iter().any(|n| {
                n.callee_pc == pc
                    && n.guard_class_id == 0
                    && ir_splice_ctor_writes_only_its_receiver(&n.site)
            })
    };
    let code = &site.callee_code;
    let len = site.callee_code_len.min(code.len());
    // `new`s whose constructor call has not been reached yet, in pc order.
    let mut pending_new = 0usize;
    let mut pc = 0usize;
    while pc < len {
        let Some(&op) = code.get(pc) else {
            return false;
        };
        let admitted = match op {
            // getfield
            0xb4 => true,
            // new
            0xbb => {
                pending_new += 1;
                true
            }
            // invokespecial: an elided no-op constructor, or a spliced one
            // that writes only its receiver
            0xb7 => {
                pending_new = pending_new.saturating_sub(1);
                site.elided_invoke_pcs.contains(&pc)
                    || site
                        .invoke_targets
                        .iter()
                        .any(|(tpc, t)| *tpc == pc && is_ctor_target(pc, t))
            }
            // invokestatic: the method itself, and nothing else
            0xb8 => site
                .invoke_targets
                .iter()
                .any(|(tpc, t)| *tpc == pc && ir_self_copy_target_is_self(site, t)),
            // A branch between a `new` and its constructor (`new C(c ? a : b)`)
            // can hand the spliced constructor a merged receiver rather than
            // the `Op::New` itself; the builder then neither skips its receiver
            // guard nor counts its stores as constructor-receiver stores, and
            // either refuses the whole method. Not a soundness question -- a
            // compile this admission must not cost -- so refuse the copy.
            0x99..=0xa7 | 0xc6..=0xc8 if pending_new > 0 => false,
            _ => ir_self_copy_trap_free_opcode(site, len, pc, op),
        };
        if !admitted {
            return false;
        }
        pc += bytecode_analysis::step(code, pc).max(1);
    }
    site.invoke_targets
        .iter()
        .all(|(tpc, t)| ir_self_copy_target_is_self(site, t) || is_ctor_target(*tpc, t))
}

/// Does this constructor body write only fields of its own receiver, and do
/// nothing else a replay could observe (round 11 wave 15, lane `fib`)?
///
/// Straight-line only: loads of locals and constants, `putfield` whose object
/// operand is local 0 (`this`, never reassigned: no store opcode is admitted),
/// the resolver's elided no-op `super()`, and `return`. No field read (the
/// builder's `splice_ctor_receiver_read` rule would turn the next store into a
/// committed one, and the self copy's later calls would then refuse the whole
/// method), no call that survives, no allocation, no static access, no branch.
/// The operand stack is tracked one entry per VALUE, which is also how
/// `putfield` pops, so a category-2 argument needs no special case.
///
/// `Node(Node l, Node r) { this.left = l; this.right = r; }` passes.
pub(crate) fn ir_splice_ctor_writes_only_its_receiver(site: &InlineSite) -> bool {
    if site.callee_is_static
        || site.method_name != "<init>"
        || !site.invoke_targets.is_empty()
        || !site.nested_sites.is_empty()
        || !site.ir_new_info.is_empty()
        || !site.static_field_info.is_empty()
        || !site.ir_typecheck_info.is_empty()
    {
        return false;
    }
    let code = &site.callee_code;
    let len = site.callee_code_len.min(code.len());
    // `true` = this value is the receiver (local 0).
    let mut stack: Vec<bool> = Vec::new();
    let mut pc = 0usize;
    while pc < len {
        let Some(&op) = code.get(pc) else {
            return false;
        };
        match op {
            // aload_0
            0x2a => stack.push(true),
            // aload <idx>
            0x19 => stack.push(code.get(pc + 1).copied() == Some(0)),
            // aconst_null .. ldc2_w, iload/lload/fload/dload <idx>,
            // iload_0 .. dload_3, aload_1 .. aload_3
            0x01..=0x18 | 0x1a..=0x29 | 0x2b..=0x2d => stack.push(false),
            // putfield: the value on top, the object under it
            0xb5 => {
                let value = stack.pop();
                let object = stack.pop();
                if value.is_none() || object != Some(true) {
                    return false;
                }
            }
            // the resolver's elided `super()`: `<init>()V` on `this`
            0xb7 => {
                if !site.elided_invoke_pcs.contains(&pc) || stack.pop() != Some(true) {
                    return false;
                }
            }
            // return
            0xb1 => {}
            _ => return false,
        }
        pc += bytecode_analysis::step(code, pc).max(1);
    }
    true
}



/// Bytes of [`IR_INLINE_MAX_TOTAL_BYTES`] a NESTED body over
/// [`MAX_INLINE_SIZE_COLD`] may not spend, under
/// [`ir_splice_nested_cold_reserve_enabled`] (round 13 wave 6, lane chain2).
/// The budget is spent in caller pc order, and a nested body is appended
/// however cold its call site is: `putVal` nests `resize` (twice) and, once
/// `HashMap$TreeNode` is initialized, `putTreeVal` / `treeifyBin`, which left
/// `get`, planned after `put`, with no budget
/// (`r13w5-hashsplice-nested-append-refusals-and-cold-budget` item 2). A
/// quarter keeps room for a later top-level site's own tree without any
/// profile of the nested call sites (which the planner does not have).
pub(crate) const IR_INLINE_NESTED_COLD_RESERVE: usize = IR_INLINE_MAX_TOTAL_BYTES / 4;

/// `CRATONVM_JIT_IR_SPLICE_NESTED_COLD_RESERVE`, **default OFF** (round 13
/// wave 6, lane chain2): keep a nested body larger than
/// [`MAX_INLINE_SIZE_COLD`] as its call when splicing it would leave less than
/// [`IR_INLINE_NESTED_COLD_RESERVE`] of the budget. Only where the append may
/// degrade that body to its call anyway (the call survives and the fences
/// already counted it), so it never refuses the enclosing body. A
/// measurement switch: the rule has no evidence that the kept call is cold.
pub(crate) fn ir_splice_nested_cold_reserve_enabled() -> bool {
    false
}

/// May the planner admit a spliced body's call to a not-yet-loaded static
/// (or a type check of a not-loaded class) as a precise exit? Only under the
/// chain fences AND with site traps on: the builder plants that exit only
/// when `ir::ir_site_trap_enabled()` (its `invokestatic` arm,
/// `plant_unloaded_typecheck_exit`), and refused the WHOLE build at that pc
/// otherwise, where the planner had already exempted the call from the
/// "neither spliced nor bindable" refusal. JIT round 19 wave 11, lane tail11
/// (`r19-unloaded-static-call-trap-planner-and-builder-gate-differently`);
/// unconditional, a planning-consistency fix: with site traps on (the
/// default) the answer is `chain_fences`, exactly as before.
pub(crate) fn unloaded_static_call_trap_admitted(chain_fences: bool) -> bool {
    chain_fences && ir::ir_site_trap_enabled()
}

/// JIT r23 wave 9, lane L6 (`CRATONVM_JIT_IR_SPLICE_UNRESOLVED_LDC_EXIT`):
/// what [`append_ir_inline_site`] does with one `InlineSite::ir_ldc_ref_rows`
/// row before its ordinary checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnresolvedLdcRow {
    /// A resolved row (non-zero slot): the ordinary checks decide it.
    NotUnresolved,
    /// An unresolved String constant on a never-run arm: rebase the pc into
    /// `unloaded_static_call_pcs` for `IrBuilder::plant_unresolved_ldc_exit`.
    Exit,
    /// Refuse the body.
    Refuse(&'static str),
}

/// Pure half of the unresolved-`ldc` row rule. `opcode` is the relocated byte
/// at the row's pc, and `in_body` means the pc lies inside the callee's code.
/// Only the resolver's slot-`0` String row reaches the exit. It needs the
/// switch on and a chain snapshot to resume through
/// ([`unloaded_static_call_trap_admitted`]). Without them the body is refused
/// with the old `ldc-non-numeric-constant` reason. A slot-`0` Class row is
/// never sent, and is refused as a mismatch.
pub(crate) fn splice_unresolved_ldc_row_verdict(
    opcode: Option<u8>,
    in_body: bool,
    holder: u32,
    is_class: bool,
    slot_addr: usize,
    switch_on: bool,
    trap_admitted: bool,
) -> UnresolvedLdcRow {
    if slot_addr != 0 {
        return UnresolvedLdcRow::NotUnresolved;
    }
    let _ = is_class;
    if holder == 0 || !in_body || !matches!(opcode, Some(0x12 | 0x13)) {
        return UnresolvedLdcRow::Refuse("ir-splice-ref-ldc-row-mismatch");
    }
    if !switch_on || !trap_admitted {
        return UnresolvedLdcRow::Refuse("ldc-non-numeric-constant");
    }
    UnresolvedLdcRow::Exit
}


/// Would [`append_ir_inline_site`] refuse this NESTED body at its entry checks,
/// whatever the budget (round 13 wave 5, lane hashsplice)? The static half of
/// its refusals, asked with no self-recursion plan: a guarded nested site,
/// several returns with multi-return off, the call replay fence under `relax`
/// (`None` skips it; so do the chain fences, which leave it to the builder),
/// and a surviving statically bound call with no direct entry. A body whose
/// code length is inconsistent answers `false`: that is a resolver defect for
/// the append to report, not a reason to degrade.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn ir_splice_nested_entry_refused(
    n: &NestedInlineSite,
    relax: Option<ir::SpliceFenceRelax>,
    chain_fences: bool,
) -> bool {
    ir_splice_nested_entry_refused_in(None, n, relax, chain_fences)
}

/// [`ir_splice_nested_entry_refused`] with the enclosing body in hand, for the
/// forwarder cap of [`ir_splice_nested_exact_admitted_in`] (nio21 wave 5, L4).
pub(crate) fn is_reflection_get_caller_class_row(t: &InlineInvokeTarget) -> bool {
    t.invoke_kind == 3
        && t.class_name == "jdk/internal/reflect/Reflection"
        && t.method_name == "getCallerClass"
        && t.descriptor == "()Ljava/lang/Class;"
}

pub(crate) fn ir_splice_nested_entry_refused_in(
    parent: Option<&InlineSite>,
    n: &NestedInlineSite,
    relax: Option<ir::SpliceFenceRelax>,
    chain_fences: bool,
) -> bool {
    // Round 13 wave 10 (lane iropt8, HS-2): an admitted exact-receiver row is
    // judged like any other nested body below.
    if n.guard_class_id != 0 && !ir_splice_nested_exact_admitted_in(parent, n, chain_fences) {
        return true;
    }
    let site = &n.site;
    let code_len = site.callee_code_len;
    if code_len == 0 || site.callee_code.len() < code_len {
        return false;
    }
    if !ir::ir_splice_multi_return_enabled() && ir_splice_site_has_several_returns(site) {
        return true;
    }
    if let Some(relax) = relax.filter(|_| !chain_fences) {
        if ir_splice_fence_refuses_site(site, None, relax) {
            return true;
        }
    }
    let live_mask = ir_splice_live_pc_mask(site);
    ir_splice_refuse_unbindable_call_enabled()
        && site.invoke_targets.iter().any(|(pc, t)| {
            live_mask.as_ref().is_none_or(|m| m.get(*pc).copied().unwrap_or(true))
                && !site.nested_sites.iter().any(|m| m.callee_pc == *pc)
                && !matches!(t.invoke_kind, 0 | 2)
                && t.direct_entry.is_none()
                // JIT round 23 wave 4 (CF23-1): or one the bound-site cache
                // serves (`append_ir_inline_site` routes it to the pair).
                && !ir_splice_unbound_call_takes_ic(t)
                // JIT r25 wave 11 (W11-2): bound after the build.
                && !r25_w11_2_arraycopy_row::counts_bound(t)
                // JIT round 22 (lane memory): built as an `Op::KeepAlive`.
                && !r22_fence_row::counts_keepalive(t)
                // j27 wave 8 (lane W8SPL): lowered as `PAUSE`, no call.
                && !ir_lower::on_spin_wait::counts_as_barrier_row(t)
                && !is_reflection_get_caller_class_row(t)
                && !ir_lower::ir_spliced_prefix_counts_bound(t)
                && !(unloaded_static_call_trap_admitted(chain_fences)
                    && site.unloaded_static_call_pcs.contains(pc))
                // nio21 wave 1 L5 (L3W6-1): lowered as a scalar op, not a call.
                && ir_spliced_scalar_row(&t.class_name, &t.method_name, &t.descriptor, t.invoke_kind)
                    .is_none()
                && !is_ir_lowered_buffer_or_charset_intrinsic(t)
        })
}

/// Does this planned inline tree allocate in any body, including nested
/// guarded and bimorphic bodies? Bytecode is decoded before opcode inspection.
pub(crate) fn ir_splice_tree_has_allocation(site: &InlineSite) -> bool {
    let len = site.callee_code_len.min(site.callee_code.len());
    let own_allocation = cratonvm_reader::verified_code(&site.callee_code[..len])
        .ok()
        .is_some_and(|verified| {
            verified.instructions().iter().any(|decoded| {
                matches!(
                    decoded.instruction,
                    cratonvm_reader::instruction::Instruction::New(_)
                        | cratonvm_reader::instruction::Instruction::Newarray(_)
                        | cratonvm_reader::instruction::Instruction::Anewarray(_)
                        | cratonvm_reader::instruction::Instruction::Multianewarray { .. }
                )
            })
        });
    own_allocation
        || site.nested_sites.iter().any(|nested| {
            ir_splice_tree_has_allocation(&nested.site)
                || nested
                    .bimorphic_second
                    .as_deref()
                    .is_some_and(|second| ir_splice_tree_has_allocation(&second.site))
        })
}

/// Does a top-level inline site consume a reference array allocated in its
/// caller since the last control-flow or invoke boundary? Keep such a splice as
/// a call: its flat frame state otherwise roots the caller's pre-invoke operand
/// while a nested callee runs.
pub(crate) fn ir_splice_site_takes_fresh_array_argument(
    site: &InlineSite,
    caller_code: &[u8],
    caller_pc: usize,
) -> bool {
    if site.method_name == "<init>" {
        return false;
    }
    let Some((params, _)) = site.descriptor.split_once(')') else {
        return false;
    };
    if !params.contains('[') && !params.contains('L') {
        return false;
    }
    let Ok(verified) = cratonvm_reader::verified_code(caller_code) else {
        return false;
    };
    let mut fresh_array = false;
    for decoded in verified.instructions() {
        let pc = decoded.pc as usize;
        if pc >= caller_pc {
            break;
        }
        match decoded.instruction {
            cratonvm_reader::instruction::Instruction::Newarray(_)
            | cratonvm_reader::instruction::Instruction::Anewarray(_)
            | cratonvm_reader::instruction::Instruction::Multianewarray { .. } => {
                fresh_array = true;
            }
            cratonvm_reader::instruction::Instruction::Invokevirtual(_)
            | cratonvm_reader::instruction::Instruction::Invokespecial(_)
            | cratonvm_reader::instruction::Instruction::Invokestatic(_)
            | cratonvm_reader::instruction::Instruction::Invokeinterface { .. }
            | cratonvm_reader::instruction::Instruction::Invokedynamic(_) => {
                fresh_array = false;
            }
            _ if verified.successors(pc).as_slice() != [decoded.next_pc as usize] => {
                fresh_array = false;
            }
            _ => {}
        }
    }
    fresh_array
}

/// Can the call at callee pc `pc` of `site` stay a call if its nested splice is
/// dropped? The resolver keeps every nested pc's `invoke_targets` row
/// (nesting is additive), and [`append_ir_inline_site`] lowers a surviving
/// row as an inline-cached virtual call, or a raw `CALL` when it is bound; a
/// statically bound row with no entry is what its unbindable-call rule
/// refuses, so such a site cannot be degraded.
/// nio21 wave 6, lane L4: will the planner refuse `site` as a NESTED body
/// for a statically bound call it leaves without an entry -- the unbindable
/// clause of [`ir_splice_nested_entry_refused_in`] (and of
/// `append_ir_inline_site`'s refusal), asked by the VM's resolver before it
/// binds the enclosing body's calls? Answered as for a compile WITH chain
/// fences (an unloaded or never-run exit is no refusal), and `false` for a
/// synchronized body (whose `holdsLock` the builder folds) and for the thin
/// box calls the append binds, so it never claims a refusal the planner would
/// not make.
///
/// Why the VM asks: the resolver direct-binds every statically bound call of
/// a body EXCEPT a nested pc (a spliced body beats a call), so a nested body
/// the planner then refuses leaves its pc unbound -- and the ENCLOSING body is
/// refused in turn. `StringLatin1.compareToCI` (two `Character.toLowerCase`
/// natives, unbindable) took `CaseInsensitiveComparator.compare(String,String)`
/// down with it. The VM drops such a nested body and binds its call instead
/// (`CRATONVM_JIT_IR_SPLICE_UNBINDABLE_NESTED_TO_CALL`).
pub fn ir_splice_body_has_unbindable_call(site: &InlineSite) -> bool {
    if site.callee_is_synchronized || !ir_splice_refuse_unbindable_call_enabled() {
        return false;
    }
    let box_thin = ir_splice_box_thin_admits_body(&site.callee_code, site.callee_code_len);
    let live_mask = ir_splice_live_pc_mask(site);
    site.invoke_targets.iter().any(|(pc, t)| {
        if matches!(t.invoke_kind, 0 | 2)
            || t.direct_entry.is_some()
            || ir_splice_row_is_prune_dead(&live_mask, *pc)
        {
            return false;
        }
        if let Some(nested) = site.nested_sites.iter().find(|m| m.callee_pc == *pc) {
            // A planned child is not proof the edge can disappear: the planner
            // may refuse that child after its own budget/admission checks. Carry
            // an unbindable surviving call outward so the VM can keep this edge
            // as a call and bind it instead of refusing the entire splice chain.
            return ir_splice_body_has_unbindable_call(&nested.site);
        }
        !ir_splice_unbound_call_takes_ic(t) && !ir_splice_static_cell_candidate(t)
            // JIT r25 wave 11 (W11-2): bound after the build.
            && !r25_w11_2_arraycopy_row::counts_bound(t)
            // JIT round 22 (lane memory): built as an `Op::KeepAlive`.
            && !r22_fence_row::counts_keepalive(t)
            // j27 wave 8 (lane W8SPL): lowered as `PAUSE`, no call.
            && !ir_lower::on_spin_wait::counts_as_barrier_row(t)
            && !is_reflection_get_caller_class_row(t)
            && !ir_lower::ir_spliced_prefix_counts_bound(t)
            && !site.unloaded_static_call_pcs.contains(pc)
            && ir_spliced_scalar_row(&t.class_name, &t.method_name, &t.descriptor, t.invoke_kind)
                .is_none()
            && !(t.invoke_kind == 3
                && box_thin
                && matches!(
                    (
                        t.class_name.as_str(),
                        t.method_name.as_str(),
                        t.descriptor.as_str()
                    ),
                    ("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;")
                        | ("java/lang/Long", "valueOf", "(J)Ljava/lang/Long;")
                ))
    })
}

/// `CRATONVM_JIT_IR_LATIN1_CASE_NESTED_TO_CALL` (JIT round 21, lane ci,
/// page `jr21-ci-compare-to-ci-and-case-map-prefix-reach-20261014.md`):
/// default ON, `=0` off. Read per call (no process global).
pub fn ir_latin1_case_nested_to_call_enabled() -> bool {
    true
}

/// JIT round 21, lane ci: is the statically bound call `class.method
/// descriptor` (`invoke_kind`) one of the Latin-1 case prefix kinds
/// (`StringLatin1.compareToCI` / `toLowerCase` / `toUpperCase`) that the VM's
/// resolver should keep a BOUND CALL instead of a nested spliced body? Nested,
/// the pc is never direct-bound (a spliced body beats a call), so
/// [`ir_splice_strip_string_prefix_nested`] cannot degrade it
/// ([`ir_splice_nested_call_survives`] needs the entry) and the JDK loop the
/// prefix replaces is spliced instead: `String.CASE_INSENSITIVE_ORDER.compare`
/// spliced into a caller ran the JDK `compareToCI` loop whenever its
/// `Character.toLowerCase` calls happened to be bindable.
pub fn ir_splice_nested_case_prefix_stays_call(
    class: &str,
    method: &str,
    descriptor: &str,
    invoke_kind: u8,
) -> bool {
    if class != "java/lang/StringLatin1" || !ir_latin1_case_nested_to_call_enabled() {
        return false;
    }
    ir_string_search_prefix_handled_kind(class, method, descriptor).is_some_and(|k| {
        matches!(
            k,
            IrStringSearchKind::Latin1CompareToCI
                | IrStringSearchKind::Latin1ToLowerCase
                | IrStringSearchKind::Latin1ToUpperCase
                | IrStringSearchKind::Latin1RegionMatchesCI
                | IrStringSearchKind::Latin1ReplaceChar
        ) && k.admits_invoke_kind(invoke_kind)
            && ir_string_prefix_site_stays_a_call(k)
    })
}


pub(crate) fn ir_splice_nested_call_survives(site: &InlineSite, pc: usize) -> bool {
    site.invoke_targets.iter().any(|(tpc, t)| {
        *tpc == pc
            && (matches!(t.invoke_kind, 0 | 2)
                || t.direct_entry.is_some()
                || ir_splice_unbound_call_takes_ic(t)
                || ir_splice_static_cell_candidate(t)
                || !ir_splice_refuse_unbindable_call_enabled())
    })
}

/// An unresolved static call can remain in a covered nested body when the
/// normal late-static-cell path is available. This is a planning predicate;
/// the append path also asks the VM's cycle-cell admission callback before it
/// creates the cell, including the initialized-class requirement.
pub(crate) fn ir_splice_static_cell_candidate(target: &InlineInvokeTarget) -> bool {
    target.invoke_kind == 3
        && target.direct_entry.is_none()
        && target.declaring_class_id != 0
        && target.target_class_id != 0
        && target.resolved_class_id == target.target_class_id
        && late_static_cell_site(false, true)
        && jit_cycle_edge_cell_enabled()
        && jit_cycle_direct_bind_enabled()
        && direct_jit_callee_calls_enabled()
        && !crate::offload_hook::keeps_dispatch_helper(
            &target.class_name,
            &target.method_name,
            &target.descriptor,
        )
        && !crate::offload_hook::is_reflect_ctor_dispatch(
            &target.class_name,
            &target.method_name,
            &target.descriptor,
        )
}

/// `CRATONVM_JIT_IR_SPLICE_BOUND_SITE_IC` (JIT round 23 wave 4, lane L1,
/// CF23-1; perf, **default ON** since JIT r24, `=0` off; `CfCpu pending` min
/// 100 against 113 ns/op over 7 interleaved reps, `done` / `complete` equal):
/// a statically bound INSTANCE call
/// that survives in a spliced body with no `direct_entry` (its callee was not
/// compiled when the caller was) takes the bound-site inline-cache pair the
/// caller's own such sites take ([`ir_bound_site_ic_planned`],
/// `CRATONVM_JIT_IR_BOUND_SITE_IC`), instead of refusing the splice
/// (`ir-splice-unbindable-surviving-call`) or -- the whole tree -- as a
/// covered nested row inside a spliced `try`
/// (`ir-splice-callee-handler-covered-nested-body`). Its miss helper's
/// bound-site arm (`jit_invoke_virtual_mic`, `invoke_kind == 1`) calls what
/// the blind site would call and publishes the body it entered keyed by the
/// receiver class: after the first call it is a raw `CALL`, not a by-name
/// resolution. Inside a covered range the call keeps its call-site exception
/// edge like the virtual rows there (the cascade is the edge's call). Read per
/// row at compile time.
pub fn ir_splice_bound_site_ic_enabled() -> bool {
    true
}

/// Does the unbound statically bound row `t` of a spliced body take the
/// bound-site inline cache ([`ir_splice_bound_site_ic_enabled`])? Kind 1
/// (an instance call; a static one has no receiver to key a cache on), no
/// `direct_entry`, and the caller's own rule for such a site
/// ([`ir_bound_site_ic_planned`]: constructors only under
/// `CRATONVM_JIT_SP_BOUND_SITE_IC_CTOR`).
pub(crate) fn ir_splice_unbound_call_takes_ic(t: &InlineInvokeTarget) -> bool {
    t.invoke_kind == 1
        && t.direct_entry.is_none()
        && ir_splice_bound_site_ic_enabled()
        && ir_bound_site_ic_planned(true, &t.method_name, false)
}



/// The top-level splice planner's carve-out (`ir_tier_attempt`,
/// `ir-splice-refused string-search-prefix-site`): does the site `class.name
/// descriptor` at `opcode` stay a CALL because the String prefix answers it?
/// The kind must admit the opcode and be one [`ir_string_prefix_site_stays_a_call`]
/// keeps.
///
/// rx27 wave 3 review (page `rx27-R-wave3-review-residuals-FIXED-20261101.md` item
/// 1): [`IrStringSearchKind::SeqSubSequence`] answers only a String receiver
/// and declines every other one to the interface call, so its site stays a
/// call only when its receiver profile (`counts`) has no row or String
/// (`string_class_id`) holds at least half of it. A site the profile says is
/// a `StringBuilder` / `CharBuffer` / other `CharSequence` is left to the
/// guarded planner (its splice or devirtualisation), as the guarded
/// `CharSequence` accessors are; with no String class id (no layout, so no
/// prefix) it is left there too. The same profile rule keeps String-trained
/// `charAt` sites as calls: expanding the JDK body would remove the call the
/// String prefix and its loop hoist consume. Non-String receivers retain the
/// guarded bytecode splice.
pub(crate) fn top_level_string_prefix_site_stays_call(
    class: &str,
    name: &str,
    descriptor: &str,
    opcode: u8,
    counts: Option<&profile::ReceiverCounts>,
    string_class_id: Option<u32>,
) -> bool {
    let Some(kind) = ir_string_search_prefix_handled_kind(class, name, descriptor) else {
        return false;
    };
    let string_receiver_prefix = matches!(kind,
        IrStringSearchKind::SeqSubSequence | IrStringSearchKind::SeqCharAt);
    if !kind.admits_opcode(opcode)
        || (!string_receiver_prefix && !ir_string_prefix_site_stays_a_call(kind))
    {
        return false;
    }
    if !string_receiver_prefix {
        return true;
    }
    let Some(string_id) = string_class_id.filter(|&id| id != 0) else {
        return false;
    };
    let Some(counts) = counts.filter(|c| !c.is_empty()) else {
        return true;
    };
    let summary = profile::summarize_receivers(counts);
    let strings = counts.get(&string_id).copied().unwrap_or(0);
    summary.observations == 0 || summary.holds_at_least_pct(strings, 50)
}



/// nio20 wave 4, lane L5: the prefix kinds whose site the splice planner keeps
/// a CALL, so the inline prefix runs instead of the JDK body
/// (`String.equals` -> `StringLatin1.equals`, `indexOf(I)` -> `indexOf(I,I)`
/// -> ..., `AbstractStringBuilder.append(C)`'s capacity / coder dispatch).
/// `hashCode` (its spliced body is the same cached read), the guarded
/// `CharSequence` accessors (whose profiled non-String receivers splice behind
/// a guard) and `StringBuilder.length` / `charAt` (a field read and a checked
/// load) keep splicing. One list for the top-level carve-out and
/// [`ir_splice_strip_string_prefix_nested`].
///
/// nio20 wave 6 (L2): `String.getChars` too -- spliced, it is the JDK body
/// (two bounds helpers, then `StringLatin1.getChars` -> the `inflate` native).
pub(crate) fn ir_string_prefix_site_stays_a_call(kind: IrStringSearchKind) -> bool {
    // JIT r20 wave 18, lane xSb (STR20-1, `CRATONVM_JIT_IR_SB_CHARAT_STAYS_CALL`):
    // `StringBuilder.charAt` too -- spliced, it is `checkIndex` ->
    // `Preconditions.checkIndex(.., SIOOBE_FORMATTER)` plus the coder test and
    // `StringUTF16.charAt`, where the prefix is one bounds compare and a load.
    if kind == IrStringSearchKind::SbCharAt {
        return ir_sb_charat_stays_call_enabled();
    }
    matches!(
        kind,
        IrStringSearchKind::Equals
            | IrStringSearchKind::CompareTo
            | IrStringSearchKind::IndexOfChar
            // JIT j27 wave 1, lane STRRX: spliced, it is `Math.max` /
            // `isLatin1` / `Math.min` and the `StringLatin1` / `StringUTF16`
            // `indexOf` -> `indexOfChar` byte loop the prefix replaces.
            | IrStringSearchKind::IndexOfCharFrom
            // Round rx27, lane splitfast: spliced, these are the JDK chain
            // (`checkBoundsBeginEnd`, `isLatin1`, `StringLatin1.newString` ->
            // `Arrays.copyOfRange` -> `System.arraycopy`, `new String`) the
            // prefix's one bump and copy replace.
            | IrStringSearchKind::Substring
            | IrStringSearchKind::SeqSubSequence
            // Round rx27 wave 3: spliced, the JDK body (clone test, `new
            // byte[]`, `Math.min`, the arraycopy prefix) the prefix replaces.
            | IrStringSearchKind::ArraysCopyOfRangeBytes
            | IrStringSearchKind::IndexOfStr
            | IrStringSearchKind::SbAppendChar
            | IrStringSearchKind::SbAppendString
            // JIT r20 wave 18, lane xSb (STR20-2): spliced, these are the JDK
            // bodies (`DecimalDigits.stringSize` + `ensureCapacitySameCoder`
            // + `uncheckedGetCharsLatin1`; `new String(this)`).
            | IrStringSearchKind::SbAppendInt
            | IrStringSearchKind::SbToString
            | IrStringSearchKind::GetChars
            | IrStringSearchKind::Inflate
            // JIT r20 wave 12, lane qStr (W6-L2-2): spliced, these are the JDK
            // bodies (`isLatin1` + `StringLatin1.toChars`, the `compress`
            // chain) -- or, `--compatible`, a native that cannot splice.
            | IrStringSearchKind::ToCharArray
            | IrStringSearchKind::CharsValueOf
            | IrStringSearchKind::CharsInit
            // JIT r20 wave 18, lane xUtf8 (STR20-6): spliced, these are the
            // JDK's byte-per-iteration loops the prefix replaces.
            | IrStringSearchKind::CountPositives
            | IrStringSearchKind::EncodeAsciiArray
            // JIT r20 wave 19, lane zCp: spliced, these are the JDK bodies
            // (`AbstractStringBuilder.appendCodePoint` -> a virtual
            // `append(char)` / `Character.toChars` + `append(char[])`). The
            // `toChars` body's throw arm (a String `ldc` and `String.format`)
            // is NOT an obstacle to splicing it: it is a masked tail arm
            // (`ir::cold_throw_arms`; test
            // `zcp20_character_to_chars_invalid_arm_is_a_masked_tail_arm`). The
            // kind stays a call because the prefix beats the splice, and a
            // spliced `new char[]` would be visible to escape analysis only
            // with `CRATONVM_JIT_IR_CHARACTER_TO_CHARS_PREFIX=0` (ZCP-5).
            | IrStringSearchKind::SbAppendCodePoint
            | IrStringSearchKind::CharacterToChars
            // JIT r23 wave 2, lane L5: spliced, `checkIndex` + `isLatin1` +
            // `StringUTF16.codePointAtSB`.
            | IrStringSearchKind::SbCodePointAt
            // JIT r23 wave 2b, lane L5: spliced, the JDK body ends in a call
            // to `ArraysSupport.mismatch` the prefix replaces.
            | IrStringSearchKind::ArraysEquals
            // JIT r21 wave 1, lane strings (CP21-1): spliced, this is the JDK
            // body (`isLatin1`, `checkIndex` -> `Preconditions.checkIndex`,
            // `StringUTF16.codePointAt`) the w17s A/B measured slower than
            // the call; the prefix is two compares and one or two loads.
            | IrStringSearchKind::CodePointAt
            // JIT r23 wave 1, lane L5 (STRINGS21-4): spliced, these are the
            // JDK bodies (`checkIndex` / `Preconditions.checkFromToIndex`,
            // `isLatin1`, the `StringUTF16` loops).
            | IrStringSearchKind::CodePointBefore
            | IrStringSearchKind::CodePointCount
            // JIT r21 wave 1, lane strings (STRINGS21-2 / ZCP-3): spliced,
            // these are the JDK chains (`CASE_INSENSITIVE_ORDER.compare` ->
            // `compareToCI`; `isLatin1` + `StringLatin1.toLowerCase`). The
            // STRINGS21-1 accessors `coder()` / `isLatin1()` are NOT listed:
            // their spliced bodies are a static read and a field read.
            | IrStringSearchKind::CompareToIgnoreCase
            | IrStringSearchKind::LowerLocaleUnchanged
            | IrStringSearchKind::LowerUnchanged
            // JIT r21 wave 3: spliced, these are the JDK loops (per-byte
            // `CharacterDataLatin1` virtual calls, two allocations).
            | IrStringSearchKind::Latin1CompareToCI
            | IrStringSearchKind::Latin1ToLowerCase

            // JIT round 21, lane strings2 (STRINGS21-1): spliced, these are
            // the JDK's scan / `Locale.getLanguage()` / per-byte
            // `CharacterDataLatin1` map / `new String` bodies the prefix replaces.
            | IrStringSearchKind::Latin1ToUpperCase
            // JIT round 21, lane str21b (STRINGS2-21-4): spliced, it is the
            // JDK's per-byte `CharacterDataLatin1.equalsIgnoreCase` loop the
            // prefix replaces.
            | IrStringSearchKind::Latin1RegionMatchesCI
            | IrStringSearchKind::ReplaceChar
            | IrStringSearchKind::Latin1ReplaceChar
            | IrStringSearchKind::ReplaceCharSeq
            | IrStringSearchKind::StringConcat
    )
}

/// nio20 wave 4, lane L5: drop, bottom-up, every NESTED body of `site` whose
/// call [`ir_string_prefix_site_stays_a_call`] -- the nested twin of the
/// top-level carve-out in the IR splice planner, which saw only the compiling
/// method's own pcs, so a `String.equals` inside a spliced `HashMap.getNode`
/// (or any helper) still ran the JDK body. Only where the call survives
/// ([`ir_splice_nested_call_survives`]); the dropped pc keeps its resolver row
/// and lowers as the `Op::Call` the prefix sits on. Returns how many bodies
/// were dropped.
pub(crate) fn ir_splice_strip_string_prefix_nested(site: &mut InlineSite) -> usize {
    let mut dropped = 0usize;
    let nested = std::mem::take(&mut site.nested_sites);
    let mut kept = Vec::with_capacity(nested.len());
    for mut n in nested {
        dropped += ir_splice_strip_string_prefix_nested(&mut n.site);
        let carved = site.invoke_targets.iter().any(|(tpc, t)| {
            *tpc == n.callee_pc
                && ir_string_search_prefix_handled_kind(
                    &t.class_name,
                    &t.method_name,
                    &t.descriptor,
                )
                .is_some_and(|k| {
                    k.admits_invoke_kind(t.invoke_kind)
                        && ir_string_prefix_site_stays_a_call(k)
                        // rx27 wave 3 review (lane R): a `CharSequence` kind
                        // replaces only a String receiver's body; a body the
                        // profile guarded on another class keeps its splice
                        // (the prefix would decline it to the interface call).
                        && (k != IrStringSearchKind::SeqSubSequence
                            || &*n.site.class_name == "java/lang/String")
                })
        });
        if carved && ir_splice_nested_call_survives(site, n.callee_pc) {
            dropped += 1;
            continue;
        }
        kept.push(n);
    }
    site.nested_sites = kept.into();
    dropped
}

/// Round nio21 wave 4 (lane L2): does any body of this splice tree carry
/// admitted handler rows ([`InlineSite::ir_handler_rows`])?
pub(crate) fn ir_site_tree_has_handler_rows(site: &InlineSite) -> bool {
    !site.ir_handler_rows.is_empty()
        || site.nested_sites.iter().any(|n| {
            ir_site_tree_has_handler_rows(&n.site)
                // JIT round 20 (lane splice): a nested bimorphic row's second
                // body is spliced too; its rows were not seen here, so the
                // builder's fail-closed refusal cost a W5-2 rebuild.
                || n
                    .bimorphic_second
                    .as_ref()
                    .is_some_and(|second| ir_site_tree_has_handler_rows(&second.site))
        })
}

/// Does this tree contain a nested call covered by an enclosing spliced `try`?
/// Such a body needs protected budget in its compile: ordinary helper
/// splices before it must not consume the space needed by the hot call itself.
pub(crate) fn ir_site_tree_has_covered_nested_candidate(site: &InlineSite, inherited: bool) -> bool {
    site.nested_sites.iter().any(|n| {
        let covered = inherited
            || site
                .ir_handler_rows
                .iter()
                .any(|&(start, end, _, _, _)| start <= n.callee_pc && n.callee_pc < end);
        covered
            || ir_site_tree_has_covered_nested_candidate(&n.site, covered)
            || n.bimorphic_second.as_ref().is_some_and(|second| {
                ir_site_tree_has_covered_nested_candidate(&second.site, covered)
            })
    })
}

/// Round nio21 wave 4 (lane L2): keep as its CALL every nested row at an
/// `invoke` inside a covered `try` range of its enclosing body's admitted
/// handler rows ([`ir::ir_splice_callee_handlers_enabled`]), at every depth.
/// A body spliced there would leave through the outermost `invoke` on a throw
/// and skip the enclosing body's `catch`; a call there takes the call-site
/// edge. Returns how many rows were dropped, or `None` when such a row's call
/// would not survive ([`ir_splice_nested_call_survives`]) and the whole site
/// must stay a call (the builder would refuse it at the nested `invoke`).
pub(crate) fn ir_splice_strip_handler_covered_nested(site: &mut InlineSite) -> Option<usize> {
    strip_handler_covered_nested(site, false, true, 0)
}

pub(crate) fn strip_handler_covered_nested(
    site: &mut InlineSite,
    inherited: bool,
    inherited_non_error: bool,
    depth: usize,
) -> Option<usize> {
    let mut dropped = 0usize;
    let mut refused = false;
    let nested = std::mem::take(&mut site.nested_sites);
    let mut kept = Vec::with_capacity(nested.len());
    for mut n in nested {
        let mut covered = inherited;
        let mut non_error = inherited_non_error;
        for &(start, end, _, ref catch_name, _) in &site.ir_handler_rows {
            if start <= n.callee_pc && n.callee_pc < end {
                covered = true;
                non_error &= ir::catch_type_cannot_catch_error(catch_name);
            }
        }
        let inner_ok = strip_handler_covered_nested(
            &mut n.site,
            covered,
            non_error,
            depth.saturating_add(1),
        )
            .map(|d| {
                dropped += d;
            })
            .is_some();
        let admitted = !covered
            || (ir_splice_covered_nested_pure_enabled() && nested_body_cannot_raise(&n.site))
            || ir::covered_nested_row_admitted_with_policy(&n, depth, non_error);
        if !admitted || !inner_ok {
            if ir_splice_nested_call_survives(site, n.callee_pc) {
                dropped += 1;
                continue;
            }
            refused = true;
        }
        kept.push(n);
    }
    site.nested_sites = kept.into();
    (!refused).then_some(dropped)
}

/// `CRATONVM_JIT_IR_SPLICE_COVERED_NESTED_PURE` (JIT r24, CompletableFuture
/// page; perf, default ON, `=0` keeps every covered nested row as its call):
/// [`ir_splice_strip_handler_covered_nested`] keeps a nested row inside a
/// covered `try` range spliced when [`nested_body_cannot_raise`]. Acts only
/// under the opt-in callee-handler family. Read per covered nested row.
pub(crate) fn ir_splice_covered_nested_pure_enabled() -> bool {
    ir::ir_splice_covered_nested_pure_enabled()
}

/// JIT r24: can the spliced body `site` raise nothing at all? Straight-line
/// or forward-branching code over locals, constants, the stack, non-trapping
/// arithmetic, compares, `ifnull` / `ifnonnull` and returns, plus a
/// `getstatic` of the callee's OWN class from an instance method (the class
/// has an instance, so it is initialized: the read cannot run `<clinit>` or
/// raise). No call, field or array access, allocation, division, type check,
/// `ldc`, monitor, throw, switch or backward branch (no poll); not
/// synchronized; no nested rows. Such a body leaves only through its return.
pub(crate) fn nested_body_cannot_raise(site: &InlineSite) -> bool {
    if site.callee_is_synchronized || !site.nested_sites.is_empty() || site.class_id == 0 {
        return false;
    }
    let code = &site.callee_code;
    let len = site.callee_code_len.min(code.len());
    let mut pc = 0usize;
    while pc < len {
        let op = code[pc];
        let ok = match op {
            0x00..=0x11
            | 0x15..=0x2d
            | 0x36..=0x4e
            | 0x57..=0x5f
            | 0x60..=0x6b
            | 0x74..=0x98 => true,
            0x99..=0xa7 | 0xc6 | 0xc7 => {
                bytecode_analysis::offset_branch_target(code, pc).is_some_and(|t| t > pc)
            }
            0xac..=0xb1 => true,
            0xb2 => {
                !site.callee_is_static
                    && site
                        .static_field_info
                        .iter()
                        .any(|&(at, class_id, _, _, _)| at == pc && class_id == site.class_id)
            }
            _ => false,
        };
        if !ok {
            return false;
        }
        pc += bytecode_analysis::step(code, pc).max(1);
    }
    true
}





/// Drop, bottom-up, every nested site of the top-level `site` that
/// [`append_ir_inline_site`] would refuse at its entry checks
/// ([`ir_splice_nested_entry_refused`]) and whose call can survive in its place
/// ([`ir_splice_nested_call_survives`]), so that the site keeps the call
/// instead of losing its whole splice (round 13 wave 5, lane hashsplice).
/// `flags` are the compile's relaxation switches; returns how many bodies were
/// dropped.
///
/// The append propagates a nested refusal to the enclosing body, and the
/// enclosing body's own fence is judged with the nested body in its tree, so
/// before this one refused `HashMap.putVal` under
/// `CRATONVM_JIT_IR_SPLICE_MULTI_RETURN=1` (a call followed by traps) cost
/// `HashMap.put` its splice as well: `putVal`'s field stores and loop made
/// `ir_splice_tree_admits_fence_relaxation` refuse the tree, and without the
/// relaxation `put`'s call to `putVal` after `hash`'s `hashCode` refused `put`.
///
/// Two passes, so that no body is judged under a relaxation other than the
/// one the append will use. The first drops only what is refused whatever the
/// tree earns: the relaxation-independent refusals, and a body whose own
/// subtree denies the tree the relaxation (a field write or a loop) and which
/// the round-11 fence refuses -- kept, it would hold the whole tree at the
/// round-11 fence and be refused by it. The second judges what is left under
/// the relaxation the remaining tree earns, which is the one the planner then
/// hands the append (dropping a body never loses a relaxation).
///
/// Not a soundness question: the builder is the replay-fence gate, and a
/// dropped body is the ordinary call the site's resolver row already
/// describes, which every enclosing check then sees as a call (it walks
/// `invoke_targets` for a pc with no nested site). Called with no
/// self-recursion plan only.
pub(crate) fn ir_splice_prune_refused_nested(
    site: &mut InlineSite,
    flags: ir::SpliceFenceRelax,
    chain_fences: bool,
) -> usize {
    let mut dropped = ir_splice_prune_nested_pass(site, None, chain_fences);
    let relax = if ir_splice_tree_admits_fence_relaxation(site) {
        flags
    } else {
        ir::SpliceFenceRelax::NONE
    };
    dropped += ir_splice_prune_nested_pass(site, Some(relax), chain_fences);
    dropped
}

/// One bottom-up pass of [`ir_splice_prune_refused_nested`]: `relax = None` is
/// the first pass, `Some` the second.
pub(crate) fn ir_splice_prune_nested_pass(
    site: &mut InlineSite,
    relax: Option<ir::SpliceFenceRelax>,
    chain_fences: bool,
) -> usize {
    let mut dropped = 0usize;
    let nested = std::mem::take(&mut site.nested_sites);
    let mut kept = Vec::with_capacity(nested.len());
    for mut n in nested {
        if n.guard_class_id == 0
            || ir_splice_nested_exact_admitted_in(Some(&*site), &n, chain_fences)
        {
            dropped += ir_splice_prune_nested_pass(&mut n.site, relax, chain_fences);
        }
        // First pass: only a body that denies the tree the relaxation is
        // judged by the fence, and then by the round-11 one.
        let judged_under = relax.or_else(|| {
            (!ir_splice_tree_admits_fence_relaxation(&n.site)).then_some(ir::SpliceFenceRelax::NONE)
        });
        if ir_splice_nested_entry_refused_in(Some(&*site), &n, judged_under, chain_fences)
            && ir_splice_nested_call_survives(site, n.callee_pc)
        {
            dropped += 1;
            continue;
        }
        kept.push(n);
    }
    site.nested_sites = kept.into();
    dropped
}



/// `CRATONVM_JIT_IR_SPLICE_NESTED_EXACT_RECEIVER`, **default ON** (round 13
/// wave 10, lane iropt8, proposal HS-2 of `jit-r13-hashsplice-proposals-RETIRED-20260929.md`).
///
/// A virtual call INSIDE a spliced body on a receiver the compile proved to be
/// exactly one class -- in `CratonBench.hashMapPutGet`, `putVal`'s
/// `this.newNode(..)` / `this.afterNodeInsertion(..)` once `put` -> `putVal` is
/// spliced on the kernel's `HashMap map = new HashMap<>()` -- is spliced like
/// the top-level exact-receiver site it descends from. Three halves read this
/// one switch: the resolver's nested loop (`jit_bridge.rs`) resolves such a
/// call for its constant-pool class and marks the row with that class
/// (`NestedInlineSite::guard_class_id`); the planner appends the body and
/// registers its combined pc as an exact-receiver site
/// ([`ir_splice_nested_exact_admitted`]); the builder splices it only when it
/// proves the receiver NODE is exactly that class, and builds the ordinary
/// inline-cached call (whose row the planner keeps) otherwise. Only under chain
/// fences. `0` keeps every such call the call it was. Read per compile.
pub fn ir_splice_nested_exact_receiver_enabled() -> bool {
    true
}

/// Round 13 wave 10 (lane iropt8, HS-2): is nested row `n` an exact-receiver
/// candidate this compile appends? In an IR-mode tree a non-zero
/// `guard_class_id` means exactly that (the IR emits no guarded nested splice;
/// see `ir_splice_nested_exact_receiver_enabled`), and names the class the
/// body was resolved for.
///
/// Only under chain fences: there the builder owns every replay fence, so a
/// body it then declines to splice (receiver not proven) leaves the call the
/// planner kept at that pc without making the planner's fence model wrong.
/// Only an instance body no larger than [`MAX_INLINE_SIZE_COLD`] (HotSpot's
/// `MaxInlineSize` for a site with no frequency; `newNode` is 13 bytes, the
/// `afterNode*` hooks 1), which bounds the budget a declined body wastes.
#[allow(dead_code)]
pub(crate) fn ir_splice_nested_exact_admitted(n: &NestedInlineSite, chain_fences: bool) -> bool {
    ir_splice_nested_exact_admitted_in(None, n, chain_fences)
}

/// [`ir_splice_nested_exact_admitted`] asked with the ENCLOSING body in hand
/// (nio21 wave 5, lane L4): when `parent` is a receiver forwarder whose one
/// call is this row ([`ir_splice_nested_exact_size_cap`]), the row may be as
/// large as a hot top-level body. `None` is the plain rule.
pub(crate) fn ir_splice_nested_exact_admitted_in(
    parent: Option<&InlineSite>,
    n: &NestedInlineSite,
    chain_fences: bool,
) -> bool {
    // `CRATONVM_FROZEN_PAGES`: a row of a frozen object's class was planned
    // for a receiver the snapshot names, and is spliced only behind its
    // identity guard: the plain inline cap.
    // shortcut: keyed by the classes of the snapshot objects this compile
    // planned rows on, so an unproven receiver of such a class takes
    // the cap too, behind its class test; key by row if that shows up.
    let site_live_len = ir_splice_live_code_len(&n.site);
    let frozen_fits = frozen_facts::compile_has_class(n.guard_class_id)
        && site_live_len <= MAX_INLINE_BYTECODE_SIZE;
    let fits = frozen_fits || parent.map_or(site_live_len <= MAX_INLINE_SIZE_COLD, |p| {
        site_live_len
            <= ir_splice_nested_exact_size_cap(
                &p.callee_code,
                p.callee_code_len,
                n.callee_pc,
                n.site.callee_num_args,
            )
            // JIT round 20 (lane splice): or the nested hot cap the VM's
            // resolver admitted a profile-guarded row under
            // (`ir_nested_guarded_body_cap`, same row, same body count; wave
            // 4: a leaf body only, `ir_nested_hot_body_admitted`).
            || ir_nested_hot_body_admitted(
                &n.site,
                ir_nested_row_hot_cap(
                    p,
                    n.callee_pc,
                    1 + usize::from(n.bimorphic_second.is_some()),
                ),
            )
            // JIT round 20 wave 11 (lane pSplice, W7L4-5): or the hot cap of a
            // nest-exact row under a hot enclosing body, the same answer the
            // VM's nest-exact arm admitted it under.
            || ir_psplice_nested_exact_row_admitted(
                &n.site,
                p.callee_receiver_profile
                    .iter()
                    .find(|(pc, _)| *pc == n.callee_pc)
                    .map(|(_, counts)| counts),
                &p.callee_code,
                p.callee_code_len,
                n.callee_pc,
                n.site.callee_num_args,
            )
            // JIT r25 wave 7 (W7-B, `CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE`): or
            // a dispatch-chain link the chain rule admits, the answer the VM's
            // nested resolver admitted it under.

    });
    n.guard_class_id != 0
        && chain_fences
        && !n.site.callee_is_static
        && fits
        && ir_splice_nested_exact_receiver_enabled()
}

/// `CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY` (nio21 wave 5, lane L4;
/// `r20-jdk-http-client-throughput-is-ten-to-fifty-x-hotspot-CLOSED-20261012.md`) --
/// **default ON**, `=0` restores the `native-shadow` refusal for every row.
pub fn splice_intrinsic_jdk_body_enabled() -> bool {
    true
}

/// May a compiled caller splice the JDK's OWN bytecode of `(class, method,
/// descriptor)` although an `Intrinsic` native is registered over it? The VM's
/// inline resolver refuses every native-shadowed body (`native-shadow`), so a
/// leaf the VM registered for its interpreter became a call through the generic
/// native funnel from compiled code -- and a body that kept such a call could
/// not be spliced either (`ir-splice-unbindable-surviving-call`).
///
/// `StringLatin1.getChar([BI)C` was the measured case: `compareToCI` calls it
/// twice per differing byte, so every `String.CASE_INSENSITIVE_ORDER`
/// comparison (`TreeMap` behind `HttpHeaders`, `Nio20HttpHot tmap` / `hdr`)
/// paid two funnel crossings per character and could not be spliced into
/// `CaseInsensitiveComparator.compare`; the `hc` census counted ~220 crossings
/// per request. Since 2026-10-13 no native is registered over it (the
/// bytecode beat the native in the interpreter too;
/// `r21-stringlatin1-kc26-intrinsics-are-funnel-calls-with-non-hotspot-failures-FIXED-20261013.md`),
/// so it is plain bytecode, splices without a row here, and has none.
///
/// Each row is REVIEWED: the registered native has no VM state of its own and
/// the JDK body is a pure leaf (no call, no allocation, no static), so the
/// splice runs exactly what HotSpot runs. The caller asks this only for a
/// native whose kind is `Intrinsic`; a stub or a bridge over the same triple
/// keeps the refusal. Classes of `java.base`, which only the bootstrap loader
/// defines.
pub fn splice_intrinsic_jdk_body_admitted(class: &str, method: &str, descriptor: &str) -> bool {
    if !splice_intrinsic_jdk_body_enabled() {
        return false;
    }
    matches!(
        (class, method, descriptor),
        // aload_0 getfield value ireturn (final classes)
        ("java/lang/Boolean", "booleanValue", "()Z") | ("java/lang/Character", "charValue", "()C")
    ) || splice_intrinsic_jdk_body_with_calls_admitted(class, method, descriptor)
        || splice_intrinsic_jdk_body_exact_admitted(class, method, descriptor)
        || splice_intrinsic_jdk_body_floor_admitted(class, method, descriptor)
        || splice_intrinsic_jdk_body_multiply_exact_long_admitted(class, method, descriptor)
        || splice_intrinsic_jdk_body_code_point_admitted(class, method, descriptor)
}

/// `CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY_CODE_POINT` (JIT round 20 wave 17,
/// lane pStr3; handoff row `R20PIRCodePoint` cp-iterate, 8101 ns against
/// HotSpot's 52) -- opt-in since the w17s A/B (it lost: cp-iterate 12736 vs
/// 9680 ns, cp-direct 106 vs 62), `=1` admits the row below.
pub fn splice_intrinsic_jdk_body_code_point_enabled() -> bool {
    false
}

/// `String.codePointAt(I)I`, a reviewed row of the
/// [`splice_intrinsic_jdk_body_with_calls_admitted`] kind (JIT round 20 wave
/// 17, lane pStr3). It is registered `Intrinsic` (`lang_math.rs`,
/// `register_wrapper_natives`, a blanket category: HotSpot has no intrinsic
/// for it), so every compiled `codePointAt` was a generic native-funnel
/// crossing (~200 ns) and the code-point iteration loop
/// (`for (i = 0; i < n; i += Character.charCount(s.codePointAt(i)))`) paid one
/// per code point. The JDK body (JDK 17 / 21 / 25 alike) is `isLatin1()`, the
/// static `String.checkIndex` (`Preconditions.checkIndex` with the SIOOBE
/// formatter: the thin helper), an array load, or the static
/// `StringUTF16.codePointAt` (plain bytecode: `getChar`, the
/// `Character.isHighSurrogate` / `isLowSurrogate` scalar rows,
/// `Character.toCodePoint`). No allocation, no VM state in the native
/// (`lang_string::native_string_code_point_at` reads one or two code units of
/// the receiver), and the bytecode's failure answers ARE HotSpot's (the
/// native mirrors its SIOOBE). A body the planner then refuses keeps the
/// native call, today's behaviour.
pub fn splice_intrinsic_jdk_body_code_point_admitted(
    class: &str,
    method: &str,
    descriptor: &str,
) -> bool {
    class == "java/lang/String"
        && method == "codePointAt"
        && descriptor == "(I)I"
        && splice_intrinsic_jdk_body_enabled()
        && splice_intrinsic_jdk_body_code_point_enabled()
}

/// [`splice_intrinsic_jdk_body_admitted`] asked with the site's position
/// (nio21 wave 14, lane L2, W12L4-1): `nested_in_ir_splice` is true for a
/// call the optimizing tier's resolver meets INSIDE a body it is splicing
/// (`resolve_inline_site_from` at `nest_depth > 0` in IR mode). Such a site
/// may also take the nested-only rows
/// ([`splice_intrinsic_jdk_body_nested_only_admitted`]); a compiling method's
/// own call site never does, so its `Math.abs(long)` keeps the scalar `AbsL`.
pub fn splice_intrinsic_jdk_body_admitted_at(
    class: &str,
    method: &str,
    descriptor: &str,
    nested_in_ir_splice: bool,
) -> bool {
    splice_intrinsic_jdk_body_admitted(class, method, descriptor)
        || (nested_in_ir_splice
            && splice_intrinsic_jdk_body_nested_only_admitted(class, method, descriptor))
}

/// The rows admitted only INSIDE an optimizing-tier splice (nio21 wave 14,
/// lane L2, W12L4-1): `Math.abs(J)J`, the one callee `multiplyExact(JJ)J`
/// needs. At a compiling method's own site the IR lowers `Math.abs(long)` as
/// `ir::ScalarOp::AbsL`, and the builder would take a planned splice before
/// that scalar op; inside a splice there is no scalar lowering for it
/// (`ir_spliced_scalar_row` refuses a `J` descriptor), so the alternative
/// there is a refused splice, never the scalar op. Same switches as
/// [`splice_intrinsic_jdk_body_multiply_exact_long_admitted`]. The single-pass
/// tier never asks (the resolver passes `nested_in_ir_splice` only in IR mode).
pub fn splice_intrinsic_jdk_body_nested_only_admitted(
    class: &str,
    method: &str,
    descriptor: &str,
) -> bool {
    class == "java/lang/Math"
        && method == "abs"
        && descriptor == "(J)J"
        && splice_intrinsic_jdk_body_enabled()
        && splice_intrinsic_jdk_body_multiply_exact_long_enabled()
        && ir::ir_splice_div_guard_enabled()
}

/// `CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY_MULTIPLY_EXACT_LONG` (nio21 wave 12,
/// lane L4, proposal W10L2-2) -- **default ON since nio21 wave 14** (lane L2),
/// `=0` withdraws [`splice_intrinsic_jdk_body_multiply_exact_long_admitted`]
/// and [`splice_intrinsic_jdk_body_nested_only_admitted`]. It was off while
/// the `Math.abs(J)J` row could also take a compiling method's OWN
/// `Math.abs(long)` site away from the scalar `AbsL`; that row is nested-only
/// now ([`splice_intrinsic_jdk_body_admitted_at`]).
pub fn splice_intrinsic_jdk_body_multiply_exact_long_enabled() -> bool {
    true
}

/// The `Math.multiplyExact(JJ)J` row and the one callee it needs (nio21 wave
/// 12, lane L4, W10L2-2). JDK 25.0.3 (`javap -c`): `multiplyExact(JI)J` is
/// plain bytecode (`lload_0; iload_2; i2l; invokestatic multiplyExact(JJ)J;
/// lreturn`) and needs only this row; `multiplyExact(JJ)J` is `r = x * y`,
/// two `Math.abs(J)J` calls, `(ax | ay) >>> 31 != 0` guarding `y != 0 && r /
/// y != x` (an `ldiv` by a VARIABLE, so [`splice_division_by_variable_admitted`]
/// names it too) and `x == MIN && y == -1`, then one cold arm `new
/// ArithmeticException; dup; ldc "long overflow"; invokespecial; athrow`
/// entered by a fall-through and a side branch (`ir::cold_throw_shape`'s side
/// entry), and one return. It does NOT call `multiplyHigh` in 25.0.3. Every
/// callee must splice, so `Math.abs(J)J` (`lload_0; lconst_0; lcmp; ifge;
/// lload_0; lneg; goto; lload_0; lreturn`, a pure leaf) is a row as well --
/// a NESTED-ONLY one since wave 14 ([`splice_intrinsic_jdk_body_nested_only_admitted`]),
/// not this predicate's. Both natives are `Intrinsic`s with no VM state
/// (`lang_math.rs`). The division's guard rides
/// [`ir::ir_splice_div_guard_enabled`], which this row requires.
pub fn splice_intrinsic_jdk_body_multiply_exact_long_admitted(
    class: &str,
    method: &str,
    descriptor: &str,
) -> bool {
    class == "java/lang/Math"
        && method == "multiplyExact"
        && descriptor == "(JJ)J"
        && splice_intrinsic_jdk_body_enabled()
        && splice_intrinsic_jdk_body_multiply_exact_long_enabled()
        && ir::ir_splice_div_guard_enabled()
}

/// `CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY_FLOOR` (nio21 wave 10, lane L2,
/// proposal W9L2-3) -- **default ON**, `=0` withdraws the `Math.floorDiv` /
/// `floorMod` rows of [`splice_intrinsic_jdk_body_floor_admitted`] and keeps
/// every other row.
pub fn splice_intrinsic_jdk_body_floor_enabled() -> bool {
    true
}

/// The reviewed `Math.floorDiv` / `floorMod` rows (nio21 wave 10, lane L2,
/// proposal W9L2-3): the four `Intrinsic`s over `(II)I` and `(JJ)J`
/// (`lang_math.rs`, "Floor/ceil division"). The `(JI)` overloads are plain
/// JDK bytecode (`lload_0; iload_2; i2l; invokestatic floorDiv(JJ)J; lreturn`,
/// `floorMod` with an `l2i`), so they splice today and need only this row for
/// their one call.
///
/// The JDK 25 bodies (`javap -c`, 25.0.3) are pure leaves: `q = x / y` (or
/// `r = x % y`), a sign test and one fix-up, two returns -- no call, no field,
/// no allocation. That is exactly what HotSpot runs; C2 has no `floorDiv`
/// intrinsic either. Splicing them is the choice over a new `ScalarOp`
/// lowering because the two halves it needs already exist:
///
/// * a CONSTANT divisor at the call (`floorDiv(epochMilli, 1000)`, the
///   `java.time` shape) reaches the builder as an `Op::Const` argument, so the
///   division builds no guard and lowers to a multiply-high
///   (`ir::IrBuilder::add_div_zero_guard`, `ir_lower`'s `emit_div_by_const`);
/// * a VARIABLE divisor keeps its zero-divisor guard inside the splice
///   ([`ir::ir_splice_div_guard_enabled`]), which deopts into `floorDiv`'s
///   own frame (or re-runs the call) so the interpreter raises `/ by zero`
///   exactly as the bytecode does; `MIN_VALUE / -1` is the lowering's `NEG`.
///
/// The VM's inline resolver admits the body's variable division only for these
/// rows ([`splice_division_by_variable_admitted`]). A single-pass caller still
/// refuses them (two returns).
pub fn splice_intrinsic_jdk_body_floor_admitted(
    class: &str,
    method: &str,
    descriptor: &str,
) -> bool {
    class == "java/lang/Math"
        && matches!(
            (method, descriptor),
            ("floorDiv", "(II)I")
                | ("floorDiv", "(JJ)J")
                | ("floorMod", "(II)I")
                | ("floorMod", "(JJ)J")
        )
        && splice_intrinsic_jdk_body_enabled()
        && splice_intrinsic_jdk_body_floor_enabled()
        && ir::ir_splice_div_guard_enabled()
}

/// May the VM's IR inline resolver admit a division whose divisor is NOT a
/// constant pushed right before it, in the body of `(class, method,
/// descriptor)`? Only the reviewed floor rows
/// ([`splice_intrinsic_jdk_body_floor_admitted`], which also asks
/// [`ir::ir_splice_div_guard_enabled`]), and, under its default-OFF switch,
/// `multiplyExact(JJ)J` ([`splice_intrinsic_jdk_body_multiply_exact_long_admitted`],
/// nio21 wave 12 L4); every other body keeps the `ir-splice-division` refusal
/// (nio21 wave 10, lane L2) -- unless
/// [`splice_division_by_variable_any_body_admitted`] (JIT round 20 wave 11,
/// lane pSplice, W10L2-1) admits every body.
pub fn splice_division_by_variable_admitted(class: &str, method: &str, descriptor: &str) -> bool {
    splice_intrinsic_jdk_body_floor_admitted(class, method, descriptor)
        || (method == "multiplyExact"
            && splice_intrinsic_jdk_body_multiply_exact_long_admitted(class, method, descriptor))
        || splice_division_by_variable_any_body_admitted()
}

/// `CRATONVM_JIT_IR_PSPLICE_VARIABLE_DIVISOR` (JIT round 20 wave 11, lane
/// pSplice; proposal W10L2-1 of `jit-r21-nio-proposals-w10-L2.md`, the
/// direction of SPLICE20-6 in `docs/feature-designs/deopt-osr.md`)
/// -- **default ON**, `=0` restores the reviewed-rows-only rule of
/// [`splice_division_by_variable_admitted`]. Read once per spliced division
/// the VM's resolver meets; never on a hot path.
pub fn ir_psplice_variable_divisor_enabled() -> bool {
    true
}

/// May ANY body's division by a variable (a parameter, a field, a computed
/// value -- a divisor that may be 0, or -1 under `MIN_VALUE`) be spliced? The
/// guard's soundness never depended on which body it is in:
///
/// * the builder keeps the zero-divisor `Op::Guard` at the division's own
///   combined pc (`IrBuilder::add_div_zero_guard`), and lets it stand inside a
///   splice only where it can resume (`IrBuilder::splice_div_guard_resumes`):
///   through the chain snapshot into the CALLEE's frame at the division (the
///   interpreter re-executes it and throws `/ by zero` from the callee, with
///   the caller's frame below it), or, while nothing the splice ran is
///   committed, by re-running the whole call from the outermost `invoke`
///   (which throws from the same frames). Never in a synchronized window. A
///   guard that can do neither refuses the build, and the planner rebuilds
///   without that site (`ir_splice_rebuild_site`): a lost splice, not a
///   wrong answer;
/// * the lowering keys the division's own zero test off that guard
///   (`ir_lower::Lowerer::emit_div_zero_guard`'s anchored shape: a
///   placeholder, never a bare `IDIV` on a zero divisor), and the `IDIV` path
///   keeps `emit_div_overflow_guard`'s `MIN_VALUE / -1` fix-up for every
///   variable divisor (JVMS: `MIN_VALUE`, remainder 0);
/// * loop motion never moves a division's guard off its division
///   (`plant_length_predicates`, JIT round 20 wave 5, `R20FuzzLicm1/6`), so
///   a zero-trip loop or an untaken arm holding the division never throws.
///
/// The other byte screens (the self-copy admissions, synchronized splices,
/// `bytecode_body_cannot_throw`) keep refusing a variable divisor: their
/// contracts exclude a guard. Requires [`ir::ir_splice_div_guard_enabled`],
/// whose `=0` refuses every variable divisor again, this rule included.
pub fn splice_division_by_variable_any_body_admitted() -> bool {
    ir_psplice_variable_divisor_enabled() && ir::ir_splice_div_guard_enabled()
}

/// `CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY_EXACT` (nio21 wave 9, lane L2;
/// `r20-jdk-http-client-throughput-is-ten-to-fifty-x-hotspot-CLOSED-20261012.md`) --
/// **default ON**, `=0` withdraws the `Math.*Exact` rows of
/// [`splice_intrinsic_jdk_body_exact_admitted`] and keeps every other row.
pub fn splice_intrinsic_jdk_body_exact_enabled() -> bool {
    true
}

/// The reviewed `java.lang.Math` overflow-checked rows (nio21 wave 9, lane L2).
///
/// Every one is registered as an `Intrinsic` (`lang_math.rs`, the "Exact
/// arithmetic" block, left OFF the leaf list "until something measures
/// them"), so a compiled caller paid a generic native-funnel crossing per call:
/// the `--real-jdk` `HttpClient` census (`Nio20HttpHot hc`, 200 requests)
/// counted 15.6k `Math.addExact` crossings, about 78 per request (the
/// `Instant` / `Duration` arithmetic of the client's deadlines and timeouts,
/// `ResponseSubscribers` length sums). HotSpot never calls them: C2 emits the
/// arithmetic and an overflow-flag branch to an uncommon trap.
///
/// The JDK 25 bodies are all ONE shape (checked with `javap -c`, 25.0.3):
/// straight-line arithmetic, one conditional branch over a cold arm `new
/// ArithmeticException; dup; ldc "integer overflow" | "long overflow";
/// invokespecial <init>; athrow`, then the result. No other call, no field,
/// no static. The optimizing tier admits such a body only through
/// `CRATONVM_JIT_IR_INLINE_COLD_ATHROW` (`ir::mask_cold_throw_arms`): the arm
/// is masked out of the spliced copy and its branch becomes a guard that
/// deopts into the callee's own frame, where the interpreter runs the method's
/// unmasked bytecode and throws -- with `Math.addExact` on the stack, exactly
/// HotSpot's trap-then-throw. A single-pass caller still refuses the body
/// (`athrow`) and keeps the call. The natives hold no VM state.
///
/// Not admitted: `multiplyExact(JJ)J` (two `Math.abs` calls and an `ldiv`;
/// its own default-OFF row since nio21 wave 12,
/// [`splice_intrinsic_jdk_body_multiply_exact_long_admitted`])
/// and `multiplyExact(JI)J` (a call of it), the `*DivExact` / `ceil*` family
/// (a division that may raise its own `ArithmeticException`; `floorDiv` /
/// `floorMod` have their own rows since nio21 wave 10,
/// [`splice_intrinsic_jdk_body_floor_admitted`]), and every
/// `StrictMath` twin (each is a call of the `Math` row).
pub fn splice_intrinsic_jdk_body_exact_admitted(
    class: &str,
    method: &str,
    descriptor: &str,
) -> bool {
    class == "java/lang/Math"
        && matches!(
            (method, descriptor),
            ("addExact", "(II)I")
                | ("addExact", "(JJ)J")
                | ("subtractExact", "(II)I")
                | ("subtractExact", "(JJ)J")
                | ("multiplyExact", "(II)I")
                | ("incrementExact", "(I)I")
                | ("incrementExact", "(J)J")
                | ("decrementExact", "(I)I")
                | ("decrementExact", "(J)J")
                | ("negateExact", "(I)I")
                | ("negateExact", "(J)J")
                | ("toIntExact", "(J)I")
        )
        && splice_intrinsic_jdk_body_enabled()
        && splice_intrinsic_jdk_body_exact_enabled()
}

/// `CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY_CALLS` (nio21 wave 6, lane L4,
/// proposal W5L4-1) -- **default ON**, `=0` withdraws the rows below and keeps
/// the leaf rows of [`splice_intrinsic_jdk_body_admitted`].
pub fn splice_intrinsic_jdk_body_calls_enabled() -> bool {
    true
}

/// The reviewed `Intrinsic` rows whose JDK body is NOT a leaf (nio21 wave 6,
/// lane L4, W5L4-1). The review rule widened by one clause: no allocation, no
/// static field, no VM state in the native, and EVERY call the body makes is
/// either plain JDK bytecode (spliced, bound, or -- an arm that never ran -- a
/// precise exit under `CRATONVM_JIT_IR_SPLICE_NEVER_RUN_STATIC_EXIT`) or a
/// reviewed accessor the optimizing tier decodes inline.
///
/// * `String.regionMatches(ZILjava/lang/String;II)Z` (150 bytes, JDK 25): the
///   tail of every `equalsIgnoreCase` (`HttpHeaders` name compares,
///   `Http1HeaderParser`, the `noCookie` filters), a generic-funnel crossing
///   per call under `--real-jdk` (4 258 in the `hc` census). Its calls:
///   `String.length()` twice (an `Intrinsic`, decoded inline as a field read
///   by `IrBuilder`'s string accessors), `coder()` twice (a private-final
///   field read), `regionMatches(ILString;II)Z` on the `!ignoreCase` arm
///   (dead under `equalsIgnoreCase`'s constant `true`; otherwise the virtual
///   call it always was), and one static `regionMatchesCI*` per coder pair,
///   plain JDK bytecode. The native (`lang_string::region_matches_impl`)
///   already mirrors the bytecode's bounds terms and its `other.length()`
///   NPE, so the splice changes no answer; it runs what HotSpot runs.
pub fn splice_intrinsic_jdk_body_with_calls_admitted(
    class: &str,
    method: &str,
    descriptor: &str,
) -> bool {
    matches!(
        (class, method, descriptor),
        (
            "java/lang/String",
            "regionMatches",
            "(ZILjava/lang/String;II)Z"
        )
    ) && splice_intrinsic_jdk_body_enabled()
        && splice_intrinsic_jdk_body_calls_enabled()
}

/// `CRATONVM_JIT_IR_SPLICE_FORWARDER_EXACT_HOT` (nio21 wave 5, lane L4;
/// `r20-jdk-http-client-throughput-is-ten-to-fifty-x-hotspot-CLOSED-20261012.md`) --
/// **default ON**, `=0` restores the [`MAX_INLINE_SIZE_COLD`] cap for every
/// nested exact-receiver row.
///
/// A javac BRIDGE (`String$CaseInsensitiveComparator.compare(Object,Object)`:
/// `aload_0; aload_1; checkcast; aload_2; checkcast; invokevirtual
/// compare(String,String); ireturn`) does nothing but forward to its target on
/// `this`. The guarded splice of `Comparator.compare` at `TreeMap.put`'s
/// comparison loop takes the 13-byte bridge, and its one call -- whose
/// receiver the guard just made exact -- was a nested exact row held to the
/// cold 35-byte cap, so the 68-byte `compare(String,String)` stayed a virtual
/// call on every comparison. For a forwarder, the nested call IS the site the
/// profile measured hot, so it takes the hot tier's cap
/// ([`MAX_INLINE_BYTECODE_SIZE`], HotSpot's `FreqInlineSize`); every other
/// enclosing body keeps the cold cap. Budget only: the builder still splices
/// the row only for a receiver it proves exact, and keeps the call otherwise.
/// Read per question, never on a hot path.
pub fn ir_splice_forwarder_exact_hot_enabled() -> bool {
    true
}

/// Is `code[..code_len]` a RECEIVER FORWARDER whose one call is the
/// `invokevirtual` at `call_pc`, taking `callee_arg_values` operands
/// (receiver included, one per parameter whatever its category)? The shape is
/// exactly `aload_0`, then only local loads and `checkcast`s, then the
/// `invokevirtual`, then one return that ends the body -- so the call's
/// receiver is `this` (every pushed value is one of its operands) and nothing
/// else happens. A `wide` load, a constant, a field read or a second call
/// answers `false`.
///
/// JIT round 20 wave 11 (lane pSplice, W5L4-5): under
/// [`ir_psplice_iface_forwarder_enabled`] the call may also be an
/// `invokeinterface` (five bytes) on `this` -- a default method that forwards
/// (`Map.Entry.comparingByKey`'s comparator body, a `Comparator` default that
/// re-dispatches on `this`). A STATIC forwarder needs nothing here: `aload_0`
/// is then its first parameter and the shape test is the same, and a static
/// nested row already takes the resolver's full cap.
pub fn ir_splice_body_is_receiver_forwarder(
    code: &[u8],
    code_len: usize,
    call_pc: usize,
    callee_arg_values: usize,
) -> bool {
    let len = code_len.min(code.len());
    let call_len = match code.get(call_pc) {
        Some(0xb9) if ir_psplice_iface_forwarder_enabled() => 5usize,
        _ => 3usize,
    };
    if len < 5 || code[0] != 0x2a || call_pc.checked_add(call_len + 1) != Some(len) {
        return false;
    }
    let mut pc = 1usize;
    let mut pushed = 1usize;
    while pc < call_pc {
        match code[pc] {
            // iload_n .. aload_n
            0x1a..=0x2d => {
                pushed += 1;
                pc += 1;
            }
            // iload .. aload <index>
            0x15..=0x19 => {
                pushed += 1;
                pc += 2;
            }
            // checkcast: the value stays on the stack.
            0xc0 => pc += 3,
            _ => return false,
        }
    }
    pc == call_pc
        && (code[call_pc] == 0xb6 || (call_len == 5 && code[call_pc] == 0xb9))
        && matches!(code[call_pc + call_len], 0xac..=0xb1)
        && pushed == callee_arg_values
}

/// `CRATONVM_JIT_IR_PSPLICE_IFACE_FORWARDER` (JIT round 20 wave 11, lane
/// pSplice; W5L4-5 of `jit-r21-nio-proposals-w5-L4.md`) -- **default ON**,
/// `=0` restores the `invokevirtual`-only forwarder shape of
/// [`ir_splice_body_is_receiver_forwarder`] and drops the VM resolver's
/// `nest-iface-forwarder` rows. Read per question; never on a hot path.
pub fn ir_psplice_iface_forwarder_enabled() -> bool {
    true
}

/// The size cap of a nested exact-receiver row at `call_pc` of the enclosing
/// body `parent_code[..parent_code_len]`: [`MAX_INLINE_BYTECODE_SIZE`] for a
/// receiver forwarder ([`ir_splice_body_is_receiver_forwarder`]) under
/// [`ir_splice_forwarder_exact_hot_enabled`], [`MAX_INLINE_SIZE_COLD`]
/// otherwise. One answer for the VM's nested resolver and this planner, so a
/// row the resolver hands over is never dropped here for its size.
pub fn ir_splice_nested_exact_size_cap(
    parent_code: &[u8],
    parent_code_len: usize,
    call_pc: usize,
    callee_arg_values: usize,
) -> usize {
    if ir_splice_forwarder_exact_hot_enabled()
        && ir_splice_body_is_receiver_forwarder(
            parent_code,
            parent_code_len,
            call_pc,
            callee_arg_values,
        )
    {
        MAX_INLINE_BYTECODE_SIZE
    } else {
        MAX_INLINE_SIZE_COLD
    }
}





/// nio21 wave 12 (lane L4, W10L2-3 / W10L2-2): the byte screens admit a
/// division by a non-zero constant; the `multiplyExact(JJ)J` row.


/// nio21 wave 10 (lane L2, proposal W9L2-3): the `Math.floorDiv` /
/// `floorMod` JDK-body rows.


/// Round 13 wave 10 (lane iropt8, HS-2): drop, at every depth, each nested
/// exact-receiver row ([`ir_splice_nested_exact_admitted`]) this compile does
/// not admit -- a synchronized compiling method (no chain snapshots), the
/// switch off, a body over the size -- so the tree every later planner rule
/// judges is exactly the one the resolver built before those rows existed: the
/// pc keeps the virtual call row it always had. Returns how many were dropped.
pub(crate) fn ir_splice_strip_unadmitted_exact_nested(site: &mut InlineSite, chain_fences: bool) -> usize {
    let mut dropped = 0usize;
    let nested = std::mem::take(&mut site.nested_sites);
    let mut kept = Vec::with_capacity(nested.len());
    for mut n in nested {
        if n.guard_class_id != 0
            && !ir_splice_nested_exact_admitted_in(Some(&*site), &n, chain_fences)
        {
            dropped += 1;
            continue;
        }
        dropped += ir_splice_strip_unadmitted_exact_nested(&mut n.site, chain_fences);
        // JIT r25 wave 7 (W7-B): a chain link admitted for the rows under it
        // goes when the strip took one of them.
        if r25_w7b_chain_splice::withdrawn_after_strip(&*site, &n, chain_fences) {
            dropped += 1 + usize::from(n.bimorphic_second.is_some());
            continue;
        }
        // JIT round 19 wave 2 (lane splice2, SPLICE19-4): a second class's
        // body is kept only beside an admitted exact row of another class,
        // admitted itself by the same rule, under the switch; its own tree is
        // stripped the same way, and it carries no second body of its own.
        if let Some(mut second) = n.bimorphic_second.take() {
            if n.guard_class_id != 0
                && second.guard_class_id != 0
                && second.guard_class_id != n.guard_class_id
                && second.callee_pc == n.callee_pc
                && second.bimorphic_second.is_none()
                // Round rx27 wave 3 (lane chain): or a chain site's pair under
                // `CRATONVM_JIT_IR_CHAIN_BIMORPHIC_LINKS`.
                && r25_w7b_chain_splice::nested_pair_admitted_row(&*site, &n)
                && (ir_splice_nested_exact_admitted(&second, chain_fences)
                    // JIT round 20 (lane splice): the pair's share of the
                    // nested hot cap, as the first body's.
                    || (chain_fences
                        && !second.site.callee_is_static
                        && (ir_nested_hot_body_admitted(
                            &second.site,
                            ir_nested_row_hot_cap(&*site, second.callee_pc, 2),
                        )
                            // JIT r25 wave 7 (W7-B): or the pair's share of
                            // a chain link.
)
                        && ir_splice_nested_exact_receiver_enabled()))
            {
                dropped +=
                    ir_splice_strip_unadmitted_exact_nested(&mut second.site, chain_fences);
                n.bimorphic_second = Some(second);
            } else {
                dropped += 1;
            }
        }
        kept.push(n);
    }
    site.nested_sites = kept.into();
    dropped
}

/// Is callee pc `target` reached on every normal execution of `code` -- no
/// branch, switch, subroutine, return or `athrow` before it in the linear
/// decode? Bytecode falls through from pc 0, so everything before the first
/// control transfer runs, in order, on every invocation that does not throw.
pub(crate) fn ir_splice_pc_in_entry_prefix(code: &[u8], code_len: usize, target: usize) -> bool {
    let len = code_len.min(code.len());
    let mut pc = 0usize;
    while pc < len {
        if pc == target {
            return true;
        }
        // ifeq..jsr, ret, tableswitch, lookupswitch, the returns, athrow,
        // ifnull, ifnonnull, goto_w, jsr_w.
        if matches!(code[pc], 0x99..=0xb1 | 0xbf | 0xc6..=0xc9) {
            return false;
        }
        pc += bytecode_analysis::step(code, pc).max(1);
    }
    false
}

/// Keep as their calls, bottom-up, the NESTED bodies of `site` larger than
/// [`MAX_INLINE_SIZE_COLD`] whose call is not on their enclosing body's entry
/// prefix ([`ir_splice_pc_in_entry_prefix`]) and can survive as a call
/// ([`ir_splice_nested_call_survives`]). Returns how many bodies were dropped
/// (round 13 wave 8, lane iropt7; item 2 of
/// `r13w5-hashsplice-nested-append-refusals-and-cold-budget-FIXED-20260928.md`).
///
/// The planner has no profile for a pc inside a callee, so every nested body
/// the resolver admits used to be spliced however cold its site: in the chain
/// arm `HashMap.put` -> `putVal` also spliced `treeifyBin` (reached when a bin
/// reaches 8 entries) and, once `HashMap$TreeNode` is initialized,
/// `putTreeVal` / `getTreeNode` -> `find` -- walk budget spent in caller pc
/// order, so `get`, planned after `put`, found it gone. HotSpot splices a body
/// over `MaxInlineSize` only on call-site frequency; the static stand-in here
/// is "the enclosing body always reaches the call": a thin wrapper's call to
/// its worker (`put` -> `putVal`, `get` -> `getNode`) stays spliced, a call
/// behind a branch of a nested body does not. A top-level site (the compiling
/// method's own, which has a profile and the hot-loop tiers) is never judged.
///
/// Only under chain fences: there the builder owns the replay fences, so a
/// body turned back into its call cannot make the planner refuse the
/// enclosing site (the call fence the planner models is off). Elsewhere the
/// enclosing bodies of such calls are refused by that fence anyway. Not a
/// soundness question: a dropped body is the ordinary call its resolver row
/// describes, exactly as in [`ir_splice_prune_refused_nested`].
pub(crate) fn ir_splice_prune_cold_nested(site: &mut InlineSite) -> usize {
    ir_splice_prune_cold_nested_in(site, false)
}

/// [`ir_splice_prune_cold_nested`] below a body reached (`in_loop_above`) at a
/// site in a loop of some enclosing body. Round rx27 wave 4
/// (`CRATONVM_JIT_IR_NESTED_IN_LOOP_HOT`): such a body runs per iteration, so
/// a row at a site in a loop of its own enclosing body, or anywhere under one,
/// is not cold (`Matcher.group` in the `regex-groups` lambda's `find` loop).
pub(crate) fn ir_splice_prune_cold_nested_in(site: &mut InlineSite, in_loop_above: bool) -> usize {
    let mut dropped = 0usize;
    let nested = std::mem::take(&mut site.nested_sites);
    let mut kept = Vec::with_capacity(nested.len());
    for mut n in nested {
        let in_loop = rx27_nested_in_loop_hot::enabled()
            && (in_loop_above
                || bytecode_pc_in_loop(&site.callee_code, site.callee_code_len, n.callee_pc));
        if n.guard_class_id == 0
            && n.site.callee_code_len > MAX_INLINE_SIZE_COLD
            && !ir_splice_pc_in_entry_prefix(&site.callee_code, site.callee_code_len, n.callee_pc)
            // JIT round 23 wave 10 (lane L2, CF-W10-2): nor on the enclosing
            // body's profiled dominant path.
            && !site.callee_hot_path_pcs.contains(&n.callee_pc)
            && !in_loop
            && ir_splice_nested_call_survives(site, n.callee_pc)
        {
            dropped += 1;
            continue;
        }
        dropped += ir_splice_prune_cold_nested_in(&mut n.site, in_loop);
        kept.push(n);
    }
    site.nested_sites = kept.into();
    dropped
}


/// JIT round 23 wave 10 (lane L2): the spliced-prune floor (CF-W10-1) and the
/// dominant-path walk (CF-W10-2).






/// `CRATONVM_JIT_IR_FACTORY_RECEIVER_OVER_PROFILE` (the for-each residual
/// round, 2026-10-06; default ON, `=0` is the kill switch): the factory class
/// ([`ir_factory_receiver_enabled`]) is planned for a site that HAS a receiver
/// profile too, in place of the profile's answer.
///
/// A loop compiled after it ran (many calls on short lists: the common case)
/// has a profile at `it.hasNext()` / `it.next()`. The profile answer is a
/// GUARDED site whose miss edge is the site's own call taking the iterator as
/// its receiver, so the iterator escaped (`ArgEscape`); and where the profile's
/// body did not resolve as a guarded one (`next()`), the site stayed a plain
/// call. The factory answer registers the same class as an EXACT-receiver
/// site: the builder splices it with no test and no miss edge where it proves
/// the receiver is the factory's `new` (`IrBuilder::exact_receiver_proof`), and
/// otherwise falls back to the guarded splice the profile would have planned
/// (`builder.add_guarded_receiver_site`, under the same profile rule), so a
/// receiver the proof does not cover is never worse off. Read once per
/// compile.
/// `CRATONVM_JIT_IR_FACTORY_TRAP_MISS` (the for-each residual round,
/// 2026-10-06; default ON, `=0` is the kill switch): a guarded FACTORY site
/// (`ir_factory_new_class`: `list.iterator()` on a list read from a field,
/// whose receiver no proof covers) takes a deopt as its miss edge instead of
/// the call, when its receiver profile names no other class
/// ([`ir_factory_trap_profile_admits`]) and the trap was not withdrawn at its
/// bci.
///
/// With the call as the miss edge, the factory's result is a φ of the spliced
/// `new` and the call's return, which is `GlobalEscape`, so the allocation is
/// too, and every later site on the result local loses its exact-receiver
/// proof. With the trap, the result IS the `new` past the guard, and the
/// iterator is scalar-replaced like one over a local list. This is the
/// general `CRATONVM_JIT_IR_GUARDED_SPLICE_TRAP_MISS` (round 14 wave 2, lane
/// guard, CH5-5; default OFF) restricted to factory sites; its deopt and
/// de-spec round trip is the same (`(bci, UncommonTrap)`, consulted before
/// the site is registered). Read once per compile.
pub(crate) fn ir_factory_trap_miss_enabled() -> bool {
    ir::ir_guarded_receiver_splice_enabled()
}

/// The receiver-profile half of [`ir_factory_trap_miss_enabled`]: the site's
/// profile names `class_id` and nothing else, at any sample count and
/// fidelity, or names nothing at all. Looser than the general rule
/// (`ir::profile_admits_trap_miss`: 32 exact samples), because a factory site
/// is usually compiled early -- in the OSR body of its first long loop, a few
/// calls in -- and the general rule then never admits it. A wrong guess costs
/// a deopt that re-executes the `invoke`, and the de-spec registry withdraws
/// the trap at its bci after a few (`(bci, UncommonTrap)`, asked before the
/// site is registered), so the recompile takes the call edge. Another class
/// recorded at the site keeps the call.
pub(crate) fn ir_factory_trap_profile_admits(
    profile: Option<&profile::MethodProfile>,
    pc: usize,
    class_id: u32,
) -> bool {
    if class_id == 0 {
        return false;
    }
    match profile.and_then(|p| p.receivers.get(&pc)) {
        None => true,
        Some(counts) => {
            let evidence = profile::ReceiverEvidence::of(counts);
            let summary = evidence.summary();
            summary.types == 0
                || (summary.types == 1 && summary.top.map(|(c, _)| c) == Some(class_id))
        }
    }
}

pub(crate) fn ir_factory_receiver_over_profile_enabled() -> bool {
    true
}

/// The class a spliced body allocates and returns when it is a factory in
/// the strict shape [`ir_factory_receiver_enabled`] describes: `new` at pc 0
/// (its class from the resolver's `ir_new_info` row), `dup`, only local loads,
/// one `invokespecial`, then the single `areturn` the body ends with.
pub(crate) fn ir_factory_new_class(site: &InlineSite) -> Option<u32> {
    let len = site.callee_code_len.min(site.callee_code.len());
    let code = site.callee_code.get(..len)?;
    if len < 8 || code[0] != 0xbb || code[3] != 0x59 || code[len - 1] != 0xb0 {
        return None;
    }
    if code[len - 4] != 0xb7 {
        return None;
    }
    let mut pc = 4usize;
    while pc < len - 4 {
        // iload..aload (with index), iload_0..aload_3.
        if !matches!(code[pc], 0x15..=0x2d) {
            return None;
        }
        pc += bytecode_analysis::step(code, pc).max(1);
    }
    if pc != len - 4 {
        return None;
    }
    site.ir_new_info
        .iter()
        .find(|&&(npc, _, _, _)| npc == 0)
        .map(|&(_, class_id, _, _)| class_id)
        .filter(|&c| c != 0)
}

/// The local `astore` that directly follows the invoke at `pc` stores its
/// result into, when no instruction after that `astore` (in pc order) stores
/// into the slot again. Earlier stores are allowed: javac reuses a slot across
/// scopes. A hint only -- the builder proves the receiver on the path it walks
/// (`IrBuilder::exact_receiver_proof`).
pub(crate) fn ir_factory_result_local(code: &[u8], code_len: usize, pc: usize, opcode: u8) -> Option<usize> {
    let len = code_len.min(code.len());
    let next = pc + if opcode == 0xb9 { 5 } else { 3 };
    let slot = match *code.get(next)? {
        0x3a => usize::from(*code.get(next + 1)?),
        op @ 0x4b..=0x4e => usize::from(op - 0x4b),
        _ => return None,
    };
    let mut stores = 0usize;
    let mut p = 0usize;
    while p < len {
        let wide = code[p] == 0xc4;
        let (op, at) = if wide {
            (*code.get(p + 1)?, p + 1)
        } else {
            (code[p], p)
        };
        let k = match op {
            0x36..=0x3a | 0x84 => Some(if wide {
                usize::from(u16::from_be_bytes([*code.get(at + 1)?, *code.get(at + 2)?]))
            } else {
                usize::from(*code.get(at + 1)?)
            }),
            0x3b..=0x4e => Some(usize::from((op - 0x3b) % 4)),
            _ => None,
        };
        // `lstore`/`dstore k` also write `k + 1`.
        let wide_value = matches!(op, 0x37 | 0x39) || matches!(op, 0x3f..=0x42 | 0x47..=0x4a);
        if p > next && k.is_some_and(|k| k == slot || (wide_value && k + 1 == slot)) {
            stores += 1;
        }
        p += bytecode_analysis::step(code, p).max(1);
    }
    (stores == 0).then_some(slot)
}

/// The factory class planned for the receiver of the no-argument
/// virtual / interface site at `pc`: its receiver is pushed by the `aload k`
/// right before it, and `k` holds a factory call's result
/// ([`ir_factory_result_local`]).
pub(crate) fn ir_factory_receiver_class(
    code: &[u8],
    pc: usize,
    opcode: u8,
    desc: &str,
    factory_locals: &HashMap<usize, u32>,
) -> Option<u32> {
    if factory_locals.is_empty() || !matches!(opcode, 0xb6 | 0xb9) || !desc.starts_with("()") {
        return None;
    }
    // JIT r22 wave 1 (lane inline, `CRATONVM_JIT_IR_SPLICE_ORDER_INSN_START`,
    // default ON): the receiver's `aload` must be the INSTRUCTION right before
    // the invoke. The byte-only read took `aload_0; getfield #42; invoke..()`
    // (cp index low byte 0x2a) for `aload_0` and planned an exact site the
    // builder can never prove -- sound (the builder keeps the call), but a
    // planned body that spent the splice budget for nothing.
    let k = if ir_splice_order::ir_splice_order_insn_start_enabled() {
        ir_receiver_aload_local(code, pc)?
    } else {
        match (
            pc.checked_sub(2).and_then(|p| code.get(p)),
            pc.checked_sub(1).and_then(|p| code.get(p)),
        ) {
            (Some(0x19), Some(&k)) => usize::from(k),
            (_, Some(&op @ 0x2a..=0x2d)) => usize::from(op - 0x2a),
            _ => return None,
        }
    };
    factory_locals.get(&k).copied()
}

/// The local the instruction immediately before `pc` loads, when that
/// instruction is an `aload k` / `wide aload k` / `aload_<k>` (JIT r22,
/// lane inline). Decodes `code[..pc]` from pc 0, so an operand byte is never
/// read as an opcode; `None` when `pc` is not an instruction boundary of that
/// decode or the predecessor is anything else. Compile time only.
pub(crate) fn ir_receiver_aload_local(code: &[u8], pc: usize) -> Option<usize> {
    if pc == 0 || pc > code.len() {
        return None;
    }
    let insns = bytecode_analysis::decode_method(code, pc)?;
    let last = insns.last()?;
    if last.next_pc() != pc {
        return None;
    }
    match (last.wide, last.op) {
        (false, 0x19) => code.get(last.pc + 1).map(|&k| usize::from(k)),
        (true, 0x19) => Some(usize::from(u16::from_be_bytes([
            *code.get(last.pc + 2)?,
            *code.get(last.pc + 3)?,
        ]))),
        (false, op @ 0x2a..=0x2d) => Some(usize::from(op - 0x2a)),
        _ => None,
    }
}

/// `CRATONVM_JIT_IR_BOUND_SITE_IC` (nio21 wave 15, lane L1) -- **default ON**,
/// `=0` restores the wave-14 plan exactly (an unbound statically bound site is
/// a blind `jit_invoke_dispatch` call for the life of the body). Read per
/// planned site at compile time, never on a hot path.
pub fn ir_bound_site_ic_enabled() -> bool {
    true
}

/// Does the optimizing tier give a statically bound INSTANCE call site an
/// inline-cache pair (nio21 wave 15, lane L1;
/// `r21-unbound-static-site-is-blind-dispatch-for-the-body-life-FIXED-20261009.md`)?
///
/// The direct bind asks the VM for the callee's compiled entry ONCE, at the
/// caller's compile. A callee that is not compiled yet (a private helper that
/// tiers up a moment after its caller, `DirectByteBuffer.putInt(II)` ->
/// `putInt(JI)`) left the site on `jit_invoke_dispatch` for the life of the
/// caller's body: ~250 ns per call through the Rust funnel against ~5 for a
/// raw CALL (`Nio20BufferTyped directput`, 4 101 ns/op vs HotSpot's 4).
///
/// Such a site now gets the virtual sites' MIC/PIC pair and is lowered by the
/// same cascade (`ir_lower::emit_inline_cache_call`). Its miss helper is
/// `jit_invoke_virtual_mic`, whose bound-site arm (`invoke_kind == 1`) calls
/// exactly what the blind site called (`jit_invoke_dispatch`) and then
/// publishes the compiled body that call entered, keyed by the receiver's
/// class. Sound because the target of a statically bound site does not depend
/// on the receiver's class: the class guard is only a key, every receiver
/// class that reaches the site is published the same body, and a null receiver
/// fails the cascade's own null test and raises the NPE in the helper, as
/// before.
///
/// `statically_bound_instance`: `invoke_kind == 1` on a non-static site;
/// `name`: the method name (constructors keep their dispatch unless
/// `CRATONVM_JIT_SP_BOUND_SITE_IC_CTOR` is on, JIT r22 wave 2; an ELIDED
/// `<init>` -- `trivial_init_pcs` / `object_init_pcs` -- never reaches the call
/// lowering, so its slots stay unused);
/// `already_routed`: the site has a direct row, a caller-held synchronized
/// row, or a cycle-edge cell.
pub(crate) fn ir_bound_site_ic_planned(
    statically_bound_instance: bool,
    name: &str,
    already_routed: bool,
) -> bool {
    statically_bound_instance
        && !already_routed
        // JIT r22 wave 2, lane inline (w15-L1 P3; `CRATONVM_JIT_SP_BOUND_SITE_IC_CTOR`,
        // opt-in, the single-pass rule's switch): a constructor too. Its
        // target is statically bound and its receiver's class is initialized
        // by the `new` that made it; the caller excludes `Object.<init>`.
        && (name != "<init>" || sp_bound_site_ic_ctor_enabled())
        && ir_bound_site_ic_enabled()
}



/// `CRATONVM_JIT_SP_BOUND_SITE_IC` (nio21 wave 18, lane L4) -- **default ON**,
/// `=0` restores the earlier single-pass plan exactly (an unbound statically
/// bound site is a blind `jit_invoke_dispatch` call for the life of the body).
/// Read per planned site at compile time, never on a hot path.
pub fn sp_bound_site_ic_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SP_BOUND_SITE_IC_CTOR` (JIT round 22 wave 1, lane inline;
/// hand-off J2 of `docs/feature-designs/deopt-osr.md`)
/// -- perf, **opt-in**. A single-pass `invokespecial <init>` site the direct
/// bind left unbound (the constructor had no compiled entry when the caller,
/// typically an early OSR body, was built) gets the bound-site inline cache
/// ([`sp_bound_site_ic_planned`], and the OSR door's mirror in the VM) instead
/// of `jit_invoke_dispatch` for the life of the body.
///
/// Sound for the reason every statically bound site is: the target does not
/// depend on the receiver's class, the cache's ways are keyed by the exact
/// receiver class and hold only the body the site's own dispatch entered
/// (`publish_bound_site_entry`, after the redefinition-epoch and quiescence
/// gates), and the slot is a `SITE_BOUND` PIC that never reaches the
/// virtual-selection paths (`CRATONVM_JIT_BOUND_PIC_ANY_OWNER`, default ON;
/// with it off only a `final` owner is planned, as before). The receiver of
/// `<init>` was allocated by a `new` that already ran the class's
/// initialization, so the raw CALL can never skip a `<clinit>`. Redefinition,
/// retirement and unload withdraw the publication exactly as for the other
/// bound sites (MIC `redefine_epoch`, `withdraw_ic_publication_if_redefined`,
/// stale-body checks on the dispatch row). A JDK throwable constructor served by
/// the dispatch door's direct path (`try_throwable_init_direct_at_dispatch`)
/// fills no dispatch row, so it is never published and keeps that path.
/// Elided constructors are excluded by the caller. Read per planned site at
/// compile time.
pub fn sp_bound_site_ic_ctor_enabled() -> bool {
    false
}

/// Does the SINGLE-PASS planner give a statically bound instance site an
/// inline-cache pair (nio21 wave 18, lane L4;
/// `r21-treemap-comparator-rows-microseconds-per-op-FIXED-20261010.md`)?
///
/// The optimizing tier's twin is [`ir_bound_site_ic_planned`]
/// (`r21-unbound-static-site-is-blind-dispatch-for-the-body-life-FIXED-20261009.md`,
/// whose "Still open" names this tier). The shape that needs it: a javac
/// BRIDGE (`ByLength.compare(Object,Object)` -> `compare(String,String)`, an
/// `invokevirtual` of a method of a `final` class, so statically bound) whose
/// single-pass body was compiled before its target had a compiled body. Its
/// direct bind found no entry, the site became `jit_invoke_dispatch` for the
/// body's whole life, and every comparison a `TreeMap` made through the bridge
/// paid the Rust funnel: measured on the 2026-10-06 Windows binary, the slow
/// mode of `Nio21TreeMapRows tmapnat,tmapcmp` made 6.48 M `jit_invoke_dispatch`
/// calls (`kind_special`, served from the dispatch cache) for 240 000 ops, 27
/// per op = one per comparison, against 0.32 M in the fast mode, and ran
/// 8 700-18 000 ns/op against 1 600-2 200. Whether that mode is hit depends on
/// which of the bridge and its target the compile threads publish first.
///
/// The site gets the virtual sites' MIC/PIC pair, lowered by the unchanged
/// single-pass cascade; its miss helper `jit_invoke_virtual_mic` routes a
/// kind-1 info first to the bound-site arm, which makes exactly the call the
/// blind site made and publishes the compiled body that call entered.
///
/// Narrower than the optimizing tier's rule, on purpose: the owner class must
/// be `final` (`owner_is_final`, asked only when everything else holds). Then
/// exactly one receiver class reaches the site, so its PIC never overflows
/// into the megamorphic gate, the hashed ways, the shared table or the vtable
/// column, whose keys are VIRTUAL selection and which this backend's cascade
/// reaches on an overflow (a private target of a non-final owner could be
/// shadowed there by a subclass's same-named method). Super calls are excluded
/// with it (a superclass is never final), and so are constructors unless
/// `CRATONVM_JIT_SP_BOUND_SITE_IC_CTOR` is on (JIT r22, with the bound PIC
/// mode; elided constructors and `Object.<init>` never).
///
/// `opcode`: `0xb6` or `0xb7` (an `0xb9` private interface target keeps its
/// dispatch); `already_routed`: the site has a direct row or an inline plan.
pub(crate) fn sp_bound_site_ic_planned(
    opcode: u8,
    invoke_kind: u8,
    name: &str,
    already_routed: bool,
    owner_is_final: impl FnOnce() -> bool,
) -> bool {
    matches!(opcode, 0xb6 | 0xb7)
        && invoke_kind == 1
        && (name != "<init>" || (opcode == 0xb7 && sp_bound_site_ic_ctor_enabled()))
        && !already_routed
        && sp_bound_site_ic_enabled()
        && (bound_pic_any_owner_enabled() || owner_is_final())
}

/// `CRATONVM_JIT_BOUND_PIC_ANY_OWNER` (JIT round 20 wave 11, lane pLate;
/// W18L4-2 of `docs/feature-designs/jit-ir-optimizations.md`,
/// DISP20-1 of `docs/feature-designs/jit-ir-optimizations.md`) --
/// **default ON**, `=0` restores the final-owner-only rule exactly. Read per
/// planned site at compile time.
///
/// With it on, a single-pass bound site ([`sp_bound_site_ic_planned`], and
/// the OSR door's mirror in the VM) is planned whatever its owner's finality,
/// and its PIC is marked [`JitPICSlot::SITE_BOUND`]
/// ([`JitPICSlot::mark_bound_site`]). That is what makes a NON-final owner
/// sound: several receiver classes can reach such a site (a private target of
/// `DirectByteBuffer`, any `invokespecial` of a superclass method), and a
/// plain PIC would overflow into the megamorphic gate, the hashed ways, the
/// shared table and the vtable column, which are keyed by VIRTUAL selection.
/// A bound slot never sets its gate, never takes a hashed way, never binds a
/// selector or a column, so the cascade's inline ways are the only cache it
/// has; each way is guarded by the receiver's EXACT class and holds the one
/// target the bound-site helper published for that class (the body this
/// site's own dispatch entered, which does not depend on the class). A fifth
/// class misses to the helper, which makes the blind call the site made
/// before.
pub fn bound_pic_any_owner_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_MEMO_SKIP_IS_A_REFUSAL` (nio21 wave 18, lane L4;
/// `r21-duplicate-single-pass-compiles-of-a-c1-method-CLOSED-20261012.md` item 3) --
/// **default ON**, `=0` restores the earlier behaviour (a memo skip publishes
/// no verdict). Should an OPTIMIZING compile the acceptance gate's refusal
/// memo skipped ("`[ir] acceptance ...: SKIPPED (a previous compile carried
/// no evidence)`") publish the refusal verdict (`ir_evidence::publish_verdict(false)`)?
///
/// The memo exists because the gate's verdict is a property of the bytecode
/// and the passes, the same on every attempt. But a skipped compile published
/// NO verdict, so its door read "no opinion" (`take_last_verdict() == None`)
/// where a judged one reads "refused", and the door's abandon rule
/// (`try_jit_compile_callee_slow`'s `refused_supersede`: a refused optimizing
/// compile does not republish its single-pass fall-through over a published
/// baseline) did not fire. Every later C2 task of such a method compiled and
/// PUBLISHED one more single-pass body over the live one -- `TreeMap.put(K,V)`
/// and the `ByLength` bridge on `Nio21TreeMapRows` -- and since wave 16 each
/// such publication makes the displaced body not entrant, so every caller
/// that baked it pays the forwarding stub. The abandon rule's own exceptions
/// (`fall_through_resolves_more_calls`, `fall_through_sheds_the_door`) still
/// keep a richer fall-through.
///
/// C1 requests (`optimize == false`) never reach the gate and publish nothing.
pub(crate) fn memo_skip_publishes_refusal(optimize: bool) -> bool {
    optimize
}





/// `CRATONVM_JIT_IR_CHA_GUARDED_SPLICE` (JIT round 21 wave 3, lane splice;
/// perf, opt-in): the optimizing planner guards an unprofiled own virtual /
/// interface site on the class-hierarchy analysis's single implementor (see
/// the `ir_cha_guard_class` closure in `ir_tier_attempt`). Read once per IR
/// compile that reaches the planner.
pub(crate) fn ir_cha_guarded_splice_enabled() -> bool {
    false
}

/// Has the site at `pc` no receiver row the profile-guarded splice could read
/// -- no row, or fewer than [`MIN_GUARDED_RECEIVER_EVIDENCE`] observations?
/// Only such a site may take a class-hierarchy guard class
/// ([`ir_cha_guarded_splice_enabled`]): a row with evidence is the profile's
/// to answer, and one that names many classes contradicts a one-implementor
/// hierarchy answer.
pub(crate) fn ir_cha_guard_site_unprofiled(
    profile: Option<&profile::MethodProfile>,
    pc: usize,
) -> bool {
    match profile.and_then(|p| p.receivers.get(&pc)) {
        None => true,
        Some(counts) => {
            profile::ReceiverEvidence::of(counts).summary().observations
                < MIN_GUARDED_RECEIVER_EVIDENCE
        }
    }
}

pub(crate) fn ir_profile_guard_class(
    opcode: u8,
    own_site: bool,
    exact_class: Option<u32>,
    profile_class: Option<u32>,
) -> Option<u32> {
    let class_id = profile_class.filter(|&c| c != 0)?;
    if !matches!(opcode, 0xb6 | 0xb9) || !own_site || exact_class == Some(class_id) {
        return None;
    }
    ir::ir_profile_guarded_splice_enabled().then_some(class_id)
}

/// Round 13 wave 12 (lane guardsplice, W7-1): resolve the body a receiver of
/// EXACTLY `class_id` dispatches to at the site declared `(cp_class, name,
/// desc)`, for a guarded splice (`CompileRequest::ir_receiver_inline_resolver`,
/// the VM's `resolve_ir_profile_receiver_inline_site`: the JVMS selection walk
/// from `class_id` with every receiver-dispatch faithfulness check, under the
/// optimizing tier's admission set). `None` without a resolver, for a refused
/// body, for a static body (a virtual site never selects one; asked again here
/// because the builder's guard needs a receiver), and for a body the builder's
/// guarded plan would decline unwalked
/// ([`ir::GUARDED_SPLICE_MAX_BODY_BYTES`]): the planner then keeps whatever it
/// had, rather than appending a body that can only cost budget.
pub(crate) fn ir_profile_guarded_site(
    resolver: Option<&dyn Fn(u32, &str, &str, &str) -> Option<InlineSite>>,
    class_id: u32,
    cp_class: &str,
    name: &str,
    desc: &str,
) -> Option<(InlineSite, u32)> {
    ir_profile_guarded_site_capped(
        resolver,
        class_id,
        cp_class,
        name,
        desc,
        ir::GUARDED_SPLICE_MAX_BODY_BYTES,
    )
}

/// [`ir_profile_guarded_site`] under an explicit body cap (nio21 wave 6, lane
/// L4): [`ir_guarded_splice_site_cap`]'s answer for a one-class site, the
/// cold [`ir::GUARDED_SPLICE_MAX_BODY_BYTES`] for every other caller.
pub(crate) fn ir_profile_guarded_site_capped(
    resolver: Option<&dyn Fn(u32, &str, &str, &str) -> Option<InlineSite>>,
    class_id: u32,
    cp_class: &str,
    name: &str,
    desc: &str,
    cap: usize,
) -> Option<(InlineSite, u32)> {
    let resolve = resolver?;
    let Some(site) = resolve(class_id, cp_class, name, desc) else {
        if x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-profile-guard DECLINED {cp_class}.{name}{desc}: \
                 receiver class id {class_id} has no resolvable bytecode body"
            );
        }
        return None;
    };
    let bytes = ir_guarded_body_bytes(&site);
    if bytes > cap
        && ir::ir_splice_body_is_lambda_forwarder(site.class_id)
        && x64::dbg_jitc_enabled()
    {
        eprintln!(
            "[cratonvm-jitc] lambda-proxy-census site={cp_class}.{name}{desc} proxy={class_id} \
             tier=ir depth=0: refused by the planner (forwarder + implementation {bytes} bytes \
             over the site cap {cap})"
        );
    }
    if site.callee_is_static || bytes > cap {
        if x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-profile-guard DECLINED {cp_class}.{name}{desc}: \
                 receiver class id {class_id} resolved {} bytes, static={}, cap={cap}",
                bytes,
                site.callee_is_static
            );
        }
        return None;
    }
    Some((site, class_id))
}

/// Profile-only prediction of an optimizing standalone guarded splice. This
/// shares the receiver rule, body screens and size cap with the IR planner;
/// callers must not substitute a currently published C1/C2 artifact as evidence.
/// The initial budget prices a potential standalone splice, before other sites
/// compete for it. Keeping a call conservatively cannot change Java behavior.
pub fn standalone_profile_guarded_splice(
    target: &InlineInvokeTarget,
    pc: usize,
    profile: Option<&profile::MethodProfile>,
    in_loop: bool,
    resolver: &dyn Fn(u32, &str, &str, &str) -> Option<InlineSite>,
) -> Option<u32> {
    let opcode = match target.invoke_kind {
        0 => 0xb6,
        2 => 0xb9,
        _ => return None,
    };
    let class_id = ir_profile_guard_class(
        opcode,
        true,
        None,
        ir::profile_guarded_receiver_class(profile, pc),
    )?;
    let cap = ir_guarded_splice_site_cap(
        in_loop || ir_guarded_site_is_hot(None, profile, pc),
        ir_inline_total_budget(),
        0,
    );
    ir_profile_guarded_site_capped(
        Some(resolver),
        class_id,
        &target.class_name,
        &target.method_name,
        &target.descriptor,
        cap,
    )
    .map(|(_, id)| id)
}

/// The bytes a guarded body costs against its site cap
/// ([`ir_profile_guarded_site_capped`]): its own, and for a lambda SAM
/// forwarder (JIT round 23 wave 2, lane L2, L2W1-2;
/// [`ir::ir_splice_body_is_lambda_forwarder`]) also its implementation's, the
/// body the call really splices. The VM resolver caps the implementation only
/// at the hot cap (it cannot see the site's hotness); the planner's site cap
/// -- cold, or hot for a profile-hot site -- decides here.
pub(crate) fn ir_guarded_body_bytes(site: &InlineSite) -> usize {
    if ir::ir_splice_body_is_lambda_forwarder(site.class_id) {
        site.nested_sites
            .iter()
            .fold(site.callee_code_len, |n, s| n.saturating_add(s.site.callee_code_len))
    } else {
        site.callee_code_len
    }
}

/// Executions a guarded site's own profile must record
/// (`MethodProfile::call_site_count`) before a site OUTSIDE every loop counts
/// as hot for [`ir_guarded_splice_site_cap`]. The method around it already
/// reached the optimizing tier; this asks that the site itself ran on a real
/// share of those invocations rather than on a cold arm. The loop threshold of
/// the plan order (`HOT_LOOP_MIN_BACKEDGES`) uses the same number.
pub(crate) const GUARDED_SPLICE_HOT_MIN_CALLS: u32 = 256;

/// JIT round 23 wave 8 (lane L5,
/// `r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`): HotSpot's
/// `InlineFrequencyCount` (100 on x86-64), the count at which C2 treats a call
/// site as frequent and inlines up to `FreqInlineSize` instead of
/// `MaxInlineSize`. Under [`ir_guarded_hot_freq_count_enabled`] it replaces
/// [`GUARDED_SPLICE_HOT_MIN_CALLS`] in [`ir_guarded_site_is_hot`].
pub(crate) const GUARDED_SPLICE_HOT_FREQ_MIN_CALLS: u32 = 100;

/// `CRATONVM_JIT_IR_GUARDED_HOT_FREQ_COUNT` (JIT round 23 wave 8, lane L5;
/// perf, **default ON since j27 2026-10-28**; `=0` keeps the 256-call floor). A guarded site
/// outside every loop counts as hot ([`ir_guarded_site_is_hot`]) from
/// [`GUARDED_SPLICE_HOT_FREQ_MIN_CALLS`] recorded calls, HotSpot's
/// `InlineFrequencyCount`, instead of [`GUARDED_SPLICE_HOT_MIN_CALLS`].
///
/// Was opt-in after the n23-w8b A/B (P-cores, reps 2-3, RxLoop ns/op, `=1` vs
/// off): matches 3050/3188 vs 2926/2975, split-ws 9858/10398 vs 9748/9625,
/// split-comma 1306/1310 vs 1363/1418, replaceall 4781/4873 vs 4586/4379 --
/// no win, slightly worse.
///
/// The sites that measured it are the `Pattern$Node.match` chain of
/// `R23ArraysEqRegex regex-groups` (n23-w7 `CRATONVM_DBG_JITC=1`):
/// `GroupTail.match` pc 70 is bimorphic (`obs=342 types=2 top=50%
/// second=49%`, 152 calls) and was `first-body-refused`; `BmpCharProperty.match`
/// pc 36 (92 % one class, 185 calls) and `GroupHead.match` pc 28 (214 calls)
/// kept their profiled classes as calls. Every body of that chain is 44-110 bytecode bytes
/// (`LastNode` 45, `GroupHead` 47, `BmpCharProperty` 55,
/// `BmpCharPropertyGreedy` 89), over the cold 35-byte cap, and each site ran
/// under 256 times by the time its method reached the optimizing tier, so each
/// node stayed a full compiled call per node per position. HotSpot inlines the
/// same bimorphic / major-receiver sites at its frequent size.
///
/// Sound for the reason the hot cap is: only how large a guarded body may be
/// changes, never which body a class gets; the tests are exact and the miss
/// edge is the site's call. The per-compile bounds stay
/// ([`GUARDED_SPLICE_HOT_MAX_SITES`], half the budget left). Read per planned
/// site, no `static`.
pub(crate) fn ir_guarded_hot_freq_count_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// The call count at which a guarded site outside every loop is hot
/// ([`ir_guarded_site_is_hot`]): [`GUARDED_SPLICE_HOT_FREQ_MIN_CALLS`] under
/// [`ir_guarded_hot_freq_count_enabled`], else [`GUARDED_SPLICE_HOT_MIN_CALLS`].
pub(crate) fn ir_guarded_splice_hot_min_calls() -> u32 {
    if ir_guarded_hot_freq_count_enabled() {
        GUARDED_SPLICE_HOT_FREQ_MIN_CALLS
    } else {
        GUARDED_SPLICE_HOT_MIN_CALLS
    }
}

/// How many guarded bodies over the cold cap one compile may plan
/// ([`ir_guarded_splice_site_cap`]). Each is walked by builds that never
/// walked it, and a refusal inside costs one W5-2 rebuild (whose count is
/// capped), so the number bounds that exposure as the byte rule below bounds
/// the budget.
pub(crate) const GUARDED_SPLICE_HOT_MAX_SITES: usize = 4;

/// Is the compiling method's own guarded site at `pc` profile-hot (nio21 wave
/// 6, lane L4)? In a loop by the plan order's ranking (`InLoop`: reached, not
/// a tail exit, inside a textual loop), or recorded at least
/// [`ir_guarded_splice_hot_min_calls`] times (JIT r23 wave 8: 100 under
/// `CRATONVM_JIT_IR_GUARDED_HOT_FREQ_COUNT`, default ON since j27 2026-10-28;
/// [`GUARDED_SPLICE_HOT_MIN_CALLS`] at `=0`) by the site's own counter or
/// receiver samples. A site the branch profile shows unreached is never hot.
pub(crate) fn ir_guarded_site_is_hot(
    ranked: Option<&ir_splice_order::ProfilePlanOrder>,
    profile: Option<&profile::MethodProfile>,
    pc: usize,
) -> bool {
    match ranked.and_then(|r| r.class_of(pc)) {
        Some(ir_splice_order::SiteClass::InLoop) => true,
        Some(ir_splice_order::SiteClass::Cold) => false,
        _ => profile.is_some_and(|p| {
            p.call_site_count(pc).count_or_zero() >= ir_guarded_splice_hot_min_calls()
        }),
    }
}

/// The body cap of a one-class profile-guarded site (nio21 wave 6, lane L4;
/// `r21-profile-guarded-splice-caps-hot-sites-at-the-cold-size-FIXED-20261012.md`).
/// [`ir::GUARDED_SPLICE_MAX_BODY_BYTES`] (HotSpot's `MaxInlineSize`) unless
/// all of these hold, then up to [`ir::GUARDED_SPLICE_HOT_MAX_BODY_BYTES`]
/// (HotSpot's `FreqInlineSize`):
///
/// * `CRATONVM_JIT_IR_GUARDED_SPLICE_HOT_CAP` is on;
/// * the site is hot ([`ir_guarded_site_is_hot`]);
/// * fewer than [`GUARDED_SPLICE_HOT_MAX_SITES`] hot bodies are planned so far;
/// * the body may take at most HALF of the byte budget still left, so one hot
///   guarded body never starves the sites after it. Never below the cold cap:
///   a body the old rule took is still taken.
pub(crate) fn ir_guarded_splice_site_cap(hot: bool, budget_left: usize, hot_sites_planned: usize) -> usize {
    let cold = ir::GUARDED_SPLICE_MAX_BODY_BYTES;
    if !hot
        || hot_sites_planned >= GUARDED_SPLICE_HOT_MAX_SITES
        || !ir::ir_guarded_splice_hot_cap_enabled()
    {
        return cold;
    }
    (budget_left / 2)
        .min(ir::GUARDED_SPLICE_HOT_MAX_BODY_BYTES)
        .max(cold)
}


/// A resolved guarded body and the receiver class it was resolved for.
pub(crate) type IrGuardedBody = (InlineSite, u32);

// Round 14 wave 2 (lane guard, GS-1 of `jit-r13-guardsplice-proposals-RETIRED-20260929.md`):
// the two bodies of a bimorphic guarded splice at the caller's site, first
// class first, or the reason there are none (a `[cratonvm-jitc]
// ir-bimorphic-census` token). `classes` is
// [`ir::profile_bimorphic_receiver_classes`]' answer for the site. Same site
// rule as the one-class profile guard ([`ir_profile_guard_class`]: an
// `invokevirtual` / `invokeinterface` of the compiling method's own code), and
// each body is resolved and filtered by [`ir_profile_guarded_site`], so
// neither is one the builder's guarded plan would decline unwalked.
// nio21 wave 7 (lane L4): the planner asks `ir_profile_bimorphic_sites_capped`;
// this cold-cap form is the tests'.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn ir_profile_bimorphic_sites(
    opcode: u8,
    own_site: bool,
    classes: Option<(u32, u32)>,
    resolver: Option<&dyn Fn(u32, &str, &str, &str) -> Option<InlineSite>>,
    cp_class: &str,
    name: &str,
    desc: &str,
) -> Result<(IrGuardedBody, IrGuardedBody), &'static str> {
    if !matches!(opcode, 0xb6 | 0xb9) || !own_site {
        return Err("not-an-own-virtual-site");
    }
    if !ir::ir_bimorphic_guarded_splice_enabled() {
        return Err("switched-off");
    }
    ir_profile_bimorphic_sites_capped(
        opcode,
        own_site,
        classes,
        resolver,
        cp_class,
        name,
        desc,
        ir::GUARDED_SPLICE_MAX_BODY_BYTES,
    )
}

/// [`ir_profile_bimorphic_sites`] with the site's byte cap passed in (nio21
/// wave 7, lane L4): [`ir_guarded_splice_site_cap`]'s answer for the site, of
/// which each of the two bodies may take [`ir_guarded_multi_body_cap`]'s
/// share.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ir_profile_bimorphic_sites_capped(
    opcode: u8,
    own_site: bool,
    classes: Option<(u32, u32)>,
    resolver: Option<&dyn Fn(u32, &str, &str, &str) -> Option<InlineSite>>,
    cp_class: &str,
    name: &str,
    desc: &str,
    site_cap: usize,
) -> Result<(IrGuardedBody, IrGuardedBody), &'static str> {
    if !matches!(opcode, 0xb6 | 0xb9) || !own_site {
        return Err("not-an-own-virtual-site");
    }
    if !ir::ir_bimorphic_guarded_splice_enabled() {
        return Err("switched-off");
    }
    let (first, second) = classes.ok_or("profile-not-bimorphic")?;
    let cap = ir_guarded_multi_body_cap(site_cap, 2);
    let first = ir_profile_guarded_site_capped(resolver, first, cp_class, name, desc, cap)
        .ok_or("first-body-refused")?;
    let second = ir_profile_guarded_site_capped(resolver, second, cp_class, name, desc, cap)
        .ok_or("second-body-refused")?;
    Ok((first, second))
}

/// `CRATONVM_JIT_IR_GUARDED_SPLICE_HOT_CAP_MULTI` (nio21 wave 7, lane L4;
/// `r21-profile-guarded-splice-caps-hot-sites-at-the-cold-size-FIXED-20261012.md`,
/// proposal W6L4-2) -- **default ON**, `=0` keeps every bimorphic and
/// polymorphic body at the cold [`ir::GUARDED_SPLICE_MAX_BODY_BYTES`] (the
/// one-class hot tier is `CRATONVM_JIT_IR_GUARDED_SPLICE_HOT_CAP`'s, and this
/// switch is inert without it). Read per site; no static.
pub fn ir_guarded_splice_hot_cap_multi_enabled() -> bool {
    true
}

/// The cap of EACH body of a guarded site with `bodies` bodies whose site cap
/// is `site_cap` ([`ir_guarded_splice_site_cap`]): an equal share, never below
/// the cold cap, so the bodies of one hot site together take about what one
/// hot body may (each extra body is code a miss leaves dead), and a cold site
/// (`site_cap` = the cold cap) keeps exactly the old rule. One body is the
/// whole cap; under the multi switch off, every share is the cold cap.
pub(crate) fn ir_guarded_multi_body_cap(site_cap: usize, bodies: usize) -> usize {
    let cold = ir::GUARDED_SPLICE_MAX_BODY_BYTES;
    if bodies <= 1 {
        return site_cap.max(cold);
    }
    if !ir_guarded_splice_hot_cap_multi_enabled() {
        return cold;
    }
    (site_cap / bodies).max(cold)
}

/// `CRATONVM_JIT_IR_NESTED_GUARDED_HOT_CAP` (JIT round 20, lane splice;
/// `r21-profile-guarded-splice-caps-hot-sites-at-the-cold-size-FIXED-20261012.md`,
/// "Round 20") -- **default ON**, `=0` keeps every nested profile-guarded body
/// at [`MAX_INLINE_SIZE_COLD`] in the VM's nested resolver and the planner's
/// registration alike. Read per nested site; no static.
pub fn ir_nested_guarded_hot_cap_enabled() -> bool {
    true
}

/// The body cap of EACH of the `bodies` bodies of a NESTED profile-guarded
/// row (the VM resolver's `nest-profiled` / `nest-profiled-iface`) at
/// `callee_pc` of the enclosing body `code[..code_len]`, whose own receiver
/// row there is `row` (JIT round 20, lane splice). The top-level guarded
/// site's hot tier ([`ir_guarded_splice_site_cap`]) is the compiling method's
/// OWN site only, so a guarded site one level down -- `static int site(Shape
/// s, int k) { return s.score(A, k); }` spliced into the loop that calls it --
/// held every body to 35 bytes however hot, and the bodies the site's own
/// compile took under the hot cap never ran (that compile's body is not called
/// once it is spliced).
///
/// Hot here is HotSpot's frequency rule one level down: the site runs on every
/// execution of its enclosing body (it is on the body's entry prefix,
/// [`ir_splice_pc_in_entry_prefix`]), or the row has at least
/// [`GUARDED_SPLICE_HOT_MIN_CALLS`] samples. Wave 2 (host run w1c): samples
/// alone never engaged -- the enclosing body is a callee the interpreter calls
/// only until its own early compile, so its row is thin -- and whether the
/// enclosing body is itself hot is the planner's question, asked at the
/// top-level site ([`ir_splice_strip_cold_nested_hot_rows`]). Each body takes
/// an equal share of HotSpot's `FreqInlineSize` ([`ir_guarded_multi_body_cap`]).
/// The VM's resolver and the planner's admission ([`ir_nested_row_hot_cap`])
/// ask this of the same body and row, so they agree.
pub fn ir_nested_guarded_body_cap(
    row: Option<&profile::ReceiverCounts>,
    code: &[u8],
    code_len: usize,
    callee_pc: usize,
    bodies: usize,
) -> usize {
    ir_nested_guarded_body_cap_in(row, 0, code, code_len, callee_pc, bodies)
}

/// `CRATONVM_JIT_IR_NESTED_HOT_CAP_BY_FREQUENCY` -- determinism, **opt-in**
/// `=1` (rx27 wave 3, lane bimodal; not a measured win, so not default; page
/// `docs/feature-designs/jit-ir-optimizations.md`). The nested hot
/// cap's sample bar ([`GUARDED_SPLICE_HOT_MIN_CALLS`]) counted a row filled
/// only while the enclosing body ran interpreted, a window that ends when the
/// body's own C1 code publishes -- so compile-thread timing set the count.
/// `BmpCharPropertyGreedy.match` pc 70 (`next.match` into `LastNode`) read
/// 216-255 samples in some processes and over 256 in others, and `matchonly`
/// ran 27.1k or 29.0k instructions per op by which. With the enclosing body's
/// invocation count of the same window known, the bar is the row's frequency
/// instead: [`r25_w7b_chain_splice::CHAIN_MIN_FREQUENCY_PERMILLE`] (C2's
/// `InlineFrequencyRatio`), the same verdict the nested chain rule and the
/// keep-loop rare-row rule ask of that site. A ratio of one window does not
/// move with the window's length. No invocation count (`0`): the sample bar,
/// as before. Read per nested row; no static.
pub fn ir_nested_hot_cap_by_frequency_enabled() -> bool {
    false
}

/// Is a nested row of `samples` receivers, in an enclosing body of
/// `invocations` interpreted entries, hot by its count
/// ([`ir_nested_guarded_body_cap_in`] without the entry-prefix half)?
pub(crate) fn ir_nested_row_count_hot(samples: u32, invocations: u32) -> bool {
    if invocations > 0 && ir_nested_hot_cap_by_frequency_enabled() {
        return r25_w7b_chain_splice::chain_frequency_permille(samples, invocations)
            >= r25_w7b_chain_splice::CHAIN_MIN_FREQUENCY_PERMILLE;
    }
    samples >= GUARDED_SPLICE_HOT_MIN_CALLS
}

/// [`ir_nested_guarded_body_cap`] with the enclosing body's interpreted
/// invocation count (`InlineSite::callee_invocations`; `0` = unknown), which
/// [`ir_nested_hot_cap_by_frequency_enabled`] turns the sample bar into a
/// frequency with. The VM's resolver and [`ir_nested_row_hot_cap`] pass the
/// same count, so they agree.
pub fn ir_nested_guarded_body_cap_in(
    row: Option<&profile::ReceiverCounts>,
    invocations: u32,
    code: &[u8],
    code_len: usize,
    callee_pc: usize,
    bodies: usize,
) -> usize {
    let hot = ir::ir_nested_guarded_splice_enabled()
        && ir::ir_guarded_splice_hot_cap_enabled()
        && ir_nested_guarded_hot_cap_enabled()
        && (ir_nested_row_count_hot(ir_nested_row_samples(row), invocations)
            || ir_splice_pc_in_entry_prefix(code, code_len, callee_pc)
            // Round rx27 wave 4 (`CRATONVM_JIT_IR_NESTED_IN_LOOP_HOT`): or a
            // site in a loop of its enclosing body.
            || rx27_nested_in_loop_hot::site_in_loop(code, code_len, callee_pc));
    if !hot {
        return MAX_INLINE_SIZE_COLD;
    }
    ir_guarded_multi_body_cap(ir::GUARDED_SPLICE_HOT_MAX_BODY_BYTES, bodies.max(1))
}

/// `CRATONVM_JIT_IR_NESTED_GUARDED_HOT_CAP_LEAF` (JIT round 20 wave 4, lane
/// splice) -- **default ON**, `=0` admits any body under the nested hot cap.
/// Host w3f: the hot cap cost `R16AllocPremium hashmap-cached` ~10 % (11.4 ->
/// 12.7 ns), won back by `CRATONVM_JIT_IR_NESTED_GUARDED_HOT_CAP=0`: inside
/// `HashMap.hash` / `getNode` / `putVal` it spliced the profiled
/// `key.hashCode()` / `equals` bodies (`String.hashCode` and kin: a cached
/// field read in front of calls and a loop), whose code and class test cost
/// more than the call they replaced. A body over the cold cap now takes the
/// nested hot cap only when it is a LEAF ([`ir_nested_hot_body_is_leaf`]):
/// the shape where a frame is most of what a call costs. Read per nested row.
pub fn ir_nested_guarded_hot_cap_leaf_enabled() -> bool {
    true
}

/// A body with no call (no `invoke_targets` row, so no nested body either)
/// and no loop anywhere in its tree.
pub fn ir_nested_hot_body_is_leaf(site: &InlineSite) -> bool {
    let live_mask = ir_splice_live_pc_mask(site);
    site.invoke_targets
        .iter()
        .all(|(pc, _)| ir_splice_row_is_prune_dead(&live_mask, *pc))
        && site.nested_sites.is_empty()
        && !ir_splice_tree_has_loop(site)
}

/// May the nested guarded body `site` (its enclosing row's hot cap `hot_cap`,
/// [`ir_nested_guarded_body_cap`]) be admitted for its size? Within the cold
/// cap always; over it, within `hot_cap` and a leaf (under the leaf switch).
/// One answer for the VM's nested resolver and the planner's admission.
pub fn ir_nested_hot_body_admitted(site: &InlineSite, hot_cap: usize) -> bool {
    let live_len = ir_splice_live_code_len(&site);
    live_len <= MAX_INLINE_SIZE_COLD
        || (live_len <= hot_cap
            && (!ir_nested_guarded_hot_cap_leaf_enabled()
                || ir_nested_hot_body_is_leaf(site)
                || (!ir_splice_tree_has_loop(site)
                    && site.class_name.starts_with("jdk/internal/reflect/"))))
}

/// `CRATONVM_JIT_IR_PSPLICE_NESTED_EXACT_HOT` (JIT round 20 wave 11, lane
/// pSplice; W7L4-5 of `jit-r21-nio-proposals-w7-L4.md`) -- **default ON**,
/// `=0` keeps every nest-exact row at [`ir_splice_nested_exact_size_cap`]
/// (35 bytes unless its enclosing body is a forwarder) in the VM's nest-exact
/// arm and the planner's admission alike. Read per nested row.
pub fn ir_psplice_nested_exact_hot_enabled() -> bool {
    true
}

/// The size cap the VM's nest-exact arm resolves a row at `callee_pc` of the
/// enclosing body `code[..code_len]` under (JIT round 20 wave 11, lane
/// pSplice, W7L4-5): the larger of [`ir_splice_nested_exact_size_cap`] and,
/// under [`ir_psplice_nested_exact_hot_enabled`], the nested hot cap of that
/// pc ([`ir_nested_guarded_body_cap`]: on the enclosing body's entry prefix,
/// or a receiver row there with [`GUARDED_SPLICE_HOT_MIN_CALLS`] samples).
/// [`ir_psplice_nested_exact_row_admitted`] then judges the body it resolved.
pub fn ir_psplice_nested_exact_row_cap(
    row: Option<&profile::ReceiverCounts>,
    code: &[u8],
    code_len: usize,
    callee_pc: usize,
    callee_arg_values: usize,
) -> usize {
    let base = ir_splice_nested_exact_size_cap(code, code_len, callee_pc, callee_arg_values);
    if !ir_psplice_nested_exact_hot_enabled() {
        return base;
    }
    base.max(ir_nested_guarded_body_cap(
        row, code, code_len, callee_pc, 1,
    ))
}

/// May the nest-exact body `site` (a call whose receiver the builder must
/// prove exactly the row's class -- `this.helper()` inside a hot guarded
/// body, `HashMap.putVal`'s `newNode`) be admitted for its size at
/// `callee_pc` of its enclosing body? Within
/// [`ir_splice_nested_exact_size_cap`] always (the rule before round 20 wave
/// 11). Over it, under [`ir_psplice_nested_exact_hot_enabled`], within the
/// nested hot cap of that pc when the body holds NO LOOP anywhere in its tree
/// ([`ir_splice_tree_has_loop`]); calls are admitted (W7L4-5: "the body is
/// real work"). Why not the profiled rows' leaf rule
/// ([`ir_nested_hot_body_admitted`]): that rule exists for a PROFILED row,
/// whose splice adds a class test and a miss call in front of the body (the
/// `R16AllocPremium hashmap-cached` loss was `key.hashCode()` / `equals`
/// bodies behind such tests); a nest-exact row splices only for a receiver
/// proven exact, so no test is added and the call it replaces is all the
/// cost. A loop is still left to its call, as the hashCode loss's loop was.
/// Profiled rows keep the leaf rule unchanged. One answer for the VM's
/// nest-exact arm and the planner's admission
/// ([`ir_splice_nested_exact_admitted_in`]).
pub fn ir_psplice_nested_exact_row_admitted(
    site: &InlineSite,
    row: Option<&profile::ReceiverCounts>,
    code: &[u8],
    code_len: usize,
    callee_pc: usize,
    callee_arg_values: usize,
) -> bool {
    let live_len = ir_splice_live_code_len(&site);
    if live_len
        <= ir_splice_nested_exact_size_cap(code, code_len, callee_pc, callee_arg_values)
    {
        return true;
    }
    ir_psplice_nested_exact_hot_enabled()
        && live_len <= ir_nested_guarded_body_cap(row, code, code_len, callee_pc, 1)
        // Round rx27 wave 4 (`CRATONVM_JIT_IR_NESTED_IN_LOOP_HOT`): a loop in
        // the tree is admitted at a site in a loop of its enclosing body.
        && (!ir_splice_tree_has_loop(site)
            || rx27_nested_in_loop_hot::site_in_loop(code, code_len, callee_pc))
}

/// JIT round 20 wave 11 (lane pSplice): W5L4-5 (interface forwarders) and
/// W7L4-5 (nest-exact rows under a hot enclosing body).


/// The receiver samples of a profile row (0 without one), for the DBG lines
/// and [`ir_nested_guarded_body_cap`].
pub fn ir_nested_row_samples(row: Option<&profile::ReceiverCounts>) -> u32 {
    row.map_or(0, |counts| {
        counts.values().copied().fold(0u32, u32::saturating_add)
    })
}

/// [`ir_nested_guarded_body_cap`] for the nested row at `callee_pc` of
/// `parent`, read from the parent body's code and the receiver rows the VM
/// carried on it.
pub(crate) fn ir_nested_row_hot_cap(parent: &InlineSite, callee_pc: usize, bodies: usize) -> usize {
    let row = parent
        .callee_receiver_profile
        .iter()
        .find(|(pc, _)| *pc == callee_pc)
        .map(|(_, counts)| counts);
    ir_nested_guarded_body_cap_in(
        row,
        parent.callee_invocations,
        &parent.callee_code,
        parent.callee_code_len,
        callee_pc,
        bodies,
    )
}

/// JIT round 20 wave 2 (lane splice): at a top-level site the plan order does
/// NOT call hot ([`ir_guarded_site_is_hot`]), drop at every depth each nested
/// guarded row (and each bimorphic second body) admitted only under the nested
/// hot cap -- over the cap the row had before round 20
/// ([`ir_splice_nested_exact_size_cap`] of its parent, or 35 for a second
/// body). The site keeps the call rows it always had there. Returns how many
/// bodies were dropped.
pub(crate) fn ir_splice_strip_cold_nested_hot_rows(site: &mut InlineSite) -> usize {
    if !ir_nested_guarded_hot_cap_enabled() {
        return 0;
    }
    let mut dropped = 0usize;
    let nested = std::mem::take(&mut site.nested_sites);
    let mut kept = Vec::with_capacity(nested.len());
    for mut n in nested {
        if n.guard_class_id != 0 {
            let old_cap = ir_splice_nested_exact_size_cap(
                &site.callee_code,
                site.callee_code_len,
                n.callee_pc,
                n.site.callee_num_args,
            );
            if n.site.callee_code_len > old_cap {
                dropped += 1 + usize::from(n.bimorphic_second.is_some());
                continue;
            }
            if n.bimorphic_second
                .as_ref()
                .is_some_and(|second| second.site.callee_code_len > MAX_INLINE_SIZE_COLD)
            {
                n.bimorphic_second = None;
                dropped += 1;
            }
        }
        dropped += ir_splice_strip_cold_nested_hot_rows(&mut n.site);
        kept.push(n);
    }
    site.nested_sites = kept.into();
    dropped
}

