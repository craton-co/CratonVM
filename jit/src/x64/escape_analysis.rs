// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Escape analysis and scalar replacement of non-escaping allocations.
//!
//! Moved verbatim out of `x64.rs`'s `Escape analysis`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.

use super::*;

/// Per-`invokespecial` shape needed for precise escape analysis.
///
/// `analyze_escapes` walks raw bytecode and cannot resolve constant-pool
/// `MethodRef` entries on its own, so the caller (which *does* have the
/// CP resolver) supplies this map keyed by the `invokespecial` PC.
#[derive(Clone, Copy)]
pub(super) struct InvokeSpecialShape {
    /// Total operand-stack slots the call consumes (receiver + params).
    pub(super) arg_slots: usize,
    /// `true` only for an `<init>()V` whose BODY the caller's resolver proved
    /// empty (`elidable_init_pcs` in `x64/driver.rs`). The `()V` descriptor
    /// alone is not that proof — `Foo() { this.x = 5; }` is also `()V`. A
    /// proven-empty constructor performs no field initialization beyond the
    /// implicit `Object.<init>` chain, so scalar replacement of the
    /// receiver is sound. Any other invokespecial (an arg-bearing
    /// `<init>`, a `super`/`private` call) writes the receiver's fields
    /// from a *separate, un-inlined* method body — the JIT cannot
    /// reproduce those writes in the frame, so the receiver must escape.
    pub(super) is_trivial_void_init: bool,
}

/// Identify `new` instructions (0xbb) whose produced objects are non-escaping.
///
/// A `new` at PC `p` is non-escaping when none of the following happen:
/// - The object is returned via `areturn`
/// - The object is stored as the VALUE of a `putfield` or `aastore`
/// - The object is passed as an argument to any invoke other than a
///   trivial `invokespecial <init>()V` on the object itself
///
/// The analysis is a single forward pass with an abstract operand stack tracking object
/// provenance (`Some(new_pc)` if the slot holds a reference produced by that `new`,
/// `None` otherwise). It is conservative: any ambiguity (e.g., complex dup variants,
/// branches that produce indeterminate stack shapes) causes all objects to be treated
/// as potentially escaping.
///
/// `invokespecial_shapes` carries the CP-resolved shape of every
/// `invokespecial` site. A missing entry is treated conservatively
/// (the call escapes every tracked operand).
///
/// Returns the set of `new` bytecode PCs that are confirmed non-escaping.
pub(super) fn analyze_escapes(
    code: &[u8],
    code_len: usize,
    invokespecial_shapes: &FxHashMap<usize, InvokeSpecialShape>,
) -> std::collections::HashSet<usize> {
    analyze_escapes_with_handlers(code, code_len, invokespecial_shapes, None)
}

/// The `goto`-over-handler straight-line map for one method, plus the targets
/// it exempts from the merge barrier.
///
/// `None` handlers means "the caller does not have this method's exception
/// table", and the answer is then a pair of all-`false` maps: every `goto`
/// stays a hard barrier, which is the pre-2026-09-22 behaviour. That is the
/// conservative direction and it is deliberate — see
/// `bytecode_analysis::straight_line_gotos` for why an empty slice must not be
/// used to mean "no handlers" here.
///
/// The second map is what makes the first usable. A straight-line `goto`'s
/// target is still a branch target, so the merge barrier at the TOP of both
/// walks would undo the see-through the instant the walk arrived. Exempting it
/// is sound for exactly the reason the `goto` qualified: the target has exactly
/// one ordinary predecessor, so the linear state on arrival is the only state
/// that reaches it, and there is nothing to merge.
fn straight_line_goto_maps(
    code: &[u8],
    code_len: usize,
    handler_pcs: Option<&[usize]>,
) -> (Vec<bool>, Vec<bool>) {
    let Some(handler_pcs) = handler_pcs else {
        // EMPTY, not `vec![false; code_len]`. Both maps are only ever read
        // through `.get(pc).copied().unwrap_or(false)`, which answers `false`
        // for an empty vec exactly as it does for an all-`false` one — and this
        // arm is the hot one: `x64/bytecode_compat`'s conservative pre-pass
        // runs on every method the scanner looks at and passes `None`.
        return (Vec::new(), Vec::new());
    };
    let straight = bytecode_analysis::straight_line_gotos(code, code_len, handler_pcs);
    let mut exempt = vec![false; code_len];
    for (pc, ok) in straight.iter().enumerate() {
        if *ok {
            if let Some(t) =
                bytecode_analysis::offset_branch_target(&code[..code_len.min(code.len())], pc)
            {
                if t < code_len {
                    exempt[t] = true;
                }
            }
        }
    }
    (straight, exempt)
}

/// [`analyze_escapes`] with the method's exception-handler entry pcs, which is
/// what licenses the `goto`-over-handler see-through (`EA-2` in
/// `docs/feature-designs/jit-ir-optimizations.md`).
///
/// `handler_pcs = None` is the caller saying it does not have the exception
/// table. Every `goto` then stays a hard barrier and this behaves exactly as it
/// did before the see-through existed.
pub(super) fn analyze_escapes_with_handlers(
    code: &[u8],
    code_len: usize,
    invokespecial_shapes: &FxHashMap<usize, InvokeSpecialShape>,
    handler_pcs: Option<&[usize]>,
) -> std::collections::HashSet<usize> {
    analyze_escapes_keeping_locks(code, code_len, invokespecial_shapes, handler_pcs, false)
}

/// [`analyze_escapes_with_handlers`], where `keep_locks` (t1 wave 2, lane M;
/// `BackendRequest::monitor_observer_at_compile`) makes `monitorenter` /
/// `monitorexit` ESCAPE their object instead of only popping it. A locked
/// object is then never scalar-replaced, so `op_object.rs`'s elision arm
/// (keyed on `sr_monitor_scalar_ops`, derived from the replaced set) removes
/// no lock, and a monitor observer (JVMTI `GetOwnedMonitorInfo`, JDWP
/// `ownedMonitors`, `getLockedMonitors`) sees the lock the program took, as
/// HotSpot's escape barrier shows it. `false` is byte for byte
/// [`analyze_escapes_with_handlers`].
pub(super) fn analyze_escapes_keeping_locks(
    code: &[u8],
    code_len: usize,
    invokespecial_shapes: &FxHashMap<usize, InvokeSpecialShape>,
    handler_pcs: Option<&[usize]>,
    keep_locks: bool,
) -> std::collections::HashSet<usize> {
    let (straight_gotos, barrier_exempt) = straight_line_goto_maps(code, code_len, handler_pcs);
    // Abstract stack: each entry is Some(new_pc) if the slot holds a new-created reference,
    // or None for non-tracked values.
    let mut abs_stack: Vec<Option<usize>> = Vec::with_capacity(16);
    // Per-local-variable provenance (up to 256 locals).
    let mut local_origin: [Option<usize>; 256] = [None; 256];
    let mut escaped: std::collections::HashSet<usize> = std::collections::HashSet::new();

    // EC-SCALAR-SOUNDNESS: branch-target PCs are hard barriers (merge
    // points where the linear abstract state is not guaranteed to match
    // the real verification-time state on every incoming edge).
    let branch_targets = bytecode_analysis::branch_target_map(code, code_len);

    // Helper: mark all tracked objects currently on the stack as escaped.
    macro_rules! escape_all {
        () => {
            for slot in abs_stack.iter() {
                if let Some(p) = slot {
                    escaped.insert(*p);
                }
            }
        };
    }

    let mut pc = 0usize;
    while pc < code_len {
        // EC-SCALAR-SOUNDNESS: entering a branch target (merge point) — any
        // object whose provenance is live here may have arrived on an edge
        // this single linear pass cannot model. Conservatively escape every
        // tracked object (stack + locals) and drop all provenance so the
        // object is never scalar-replaced. (pc 0 is also flagged as a target
        // but nothing is live there, so this is a no-op at entry.)
        // EA-2: a target whose ONLY ordinary predecessor is a straight-line
        // `goto` this walk is about to step through is not a merge. See
        // `straight_line_goto_maps`.
        if branch_targets.get(pc).copied().unwrap_or(false)
            && !barrier_exempt.get(pc).copied().unwrap_or(false)
        {
            escape_all!();
            for slot in local_origin.iter_mut() {
                if let Some(p) = slot.take() {
                    escaped.insert(p);
                }
            }
            abs_stack.clear();
        }
        let op = code[pc];
        match op {
            // new — push a tracked reference
            0xbb => {
                abs_stack.push(Some(pc));
                pc += 3;
            }
            // dup — duplicate top
            0x59 => {
                let top = abs_stack.last().copied().flatten();
                abs_stack.push(top);
                pc += 1;
            }
            // astore_0..3
            0x4b..=0x4e => {
                let idx = (op - 0x4b) as usize; // Widening: always safe
                let val = abs_stack.pop().flatten();
                // If local already had a tracked object, old value might re-escape on reassign
                if let Some(prev) = local_origin[idx] {
                    escaped.insert(prev);
                }
                local_origin[idx] = val;
                pc += 1;
            }
            // astore N
            0x3a => {
                if pc + 1 < code_len {
                    let idx = code[pc + 1] as usize; // Widening: always safe
                    let val = abs_stack.pop().flatten();
                    if let Some(prev) = local_origin[idx] {
                        escaped.insert(prev);
                    }
                    local_origin[idx] = val;
                }
                pc += 2;
            }
            // aload_0..3
            0x2a..=0x2d => {
                let idx = (op - 0x2a) as usize; // Widening: always safe
                abs_stack.push(local_origin[idx]);
                pc += 1;
            }
            // aload N
            0x19 => {
                if pc + 1 < code_len {
                    let idx = code[pc + 1] as usize; // Widening: always safe
                    abs_stack.push(local_origin[idx]);
                } else {
                    abs_stack.push(None);
                }
                pc += 2;
            }
            // areturn — top of stack returns, so it escapes
            0xb0 => {
                if let Some(p) = abs_stack.pop().flatten() {
                    escaped.insert(p);
                }
                // The path ENDS here: see the plain-return arm below for why
                // the locals are dropped rather than escaped.
                escape_all!();
                abs_stack.clear();
                for slot in local_origin.iter_mut() {
                    *slot = None;
                }
                pc += 1;
            }
            // Primitive stores (r9-ea). `istore`/`fstore` overwrite one slot and
            // `lstore`/`dstore` two. A slot that held a tracked reference no
            // longer does, so its provenance is DROPPED (not escaped: losing
            // the last frame reference to an object publishes nothing).
            //
            // This used to fall into the catch-all, which escaped every tracked
            // stack operand and never cleared the overwritten slot. The stale
            // slot was then escaped at the next branch target, so javac's
            // ordinary slot reuse — `{ Foo f = new Foo(); … } int x = …;` in
            // straight-line code followed by any branch — cost `f` its scalar
            // replacement, and `plan_scalar_replacement` kept publishing the
            // dead object as live in that slot to deopt snapshots.
            //
            // A tracked value consumed as the stored operand cannot happen in
            // verified code; it is escaped anyway, as every modelled consumer
            // here does.
            0x36..=0x39 => {
                if let Some(p) = abs_stack.pop().flatten() {
                    escaped.insert(p);
                }
                if pc + 1 < code_len {
                    let idx = code[pc + 1] as usize; // Widening: always safe
                    local_origin[idx] = None;
                    // lstore (0x37) / dstore (0x39) also overwrite idx + 1.
                    if matches!(op, 0x37 | 0x39) && idx + 1 < local_origin.len() {
                        local_origin[idx + 1] = None;
                    }
                }
                pc += 2;
            }
            // istore_<n> 0x3b..=0x3e, lstore_<n> 0x3f..=0x42,
            // fstore_<n> 0x43..=0x46, dstore_<n> 0x47..=0x4a.
            0x3b..=0x4a => {
                if let Some(p) = abs_stack.pop().flatten() {
                    escaped.insert(p);
                }
                let idx = ((op - 0x3b) % 4) as usize; // Widening: always safe
                local_origin[idx] = None;
                if matches!(op, 0x3f..=0x42 | 0x47..=0x4a) {
                    local_origin[idx + 1] = None;
                }
                pc += 1;
            }
            // Primitive arithmetic, conversions and compares (r9-ea). They read
            // and produce primitives only, so they cannot publish a reference,
            // and they are modelled with their exact stack effect — the same
            // effect `plan_scalar_replacement` gives them.
            //
            // They used to reach the catch-all, which escapes EVERY tracked
            // operand on the stack. `p.x = a + b` compiles to
            // `aload p; iload a; iload b; iadd; putfield x`, so the `iadd`
            // escaped `p` and single-pass scalar replacement only ever fired
            // for fields assigned a constant or a bare local.
            //
            // The integer divides (`idiv`/`irem`/`ldiv`/`lrem`) can throw, and a
            // throw can leave a tracked dummy reference below the operands on
            // the stack. That is harmless: the exception discards the operand
            // stack (JVMS §2.10), and a deopt that re-executes the divide throws
            // again before anything reads it.
            //
            // Binary ops (pop 2, push 1): iadd..drem, ishl..lxor, lcmp..dcmpg.
            0x60..=0x73 | 0x78..=0x83 | 0x94..=0x98 => {
                for _ in 0..2 {
                    if let Some(p) = abs_stack.pop().flatten() {
                        escaped.insert(p);
                    }
                }
                abs_stack.push(None);
                pc += 1;
            }
            // Unary ops and conversions (pop 1, push 1): ineg..dneg, i2l..i2s.
            0x74..=0x77 | 0x85..=0x93 => {
                if let Some(p) = abs_stack.pop().flatten() {
                    escaped.insert(p);
                }
                abs_stack.push(None);
                pc += 1;
            }
            // iinc — writes an int local in place; no stack effect, and an int
            // slot holds no tracked reference.
            0x84 => {
                pc += 3;
            }
            // `wide iinc` (r9-ea) — javac's `x += 1000`: an int local updated in
            // place, no stack effect, no reference touched. Modelled exactly (in
            // `plan_scalar_replacement` too) rather than as the `wide` barrier
            // below, which would cost every straight-line object across it its
            // scalar replacement.
            0xc4 if pc + 1 < code_len && code[pc + 1] == 0x84 => {
                pc += 6;
            }
            // wide (r9-ea) — a widened load/store/iinc on ANY local index.
            // `plan_scalar_replacement` does not decode it and drops every
            // provenance it holds at one, so the two analyses only stay in
            // agreement if this one treats it as the barrier it is there: a
            // `wide astore` can overwrite a slot whose provenance would
            // otherwise go stale, and a `wide aload` pushes a value this walk
            // cannot name. Rare (methods with > 255 locals, large `iinc`).
            0xc4 => {
                escape_all!();
                for slot in local_origin.iter_mut() {
                    if let Some(p) = slot.take() {
                        escaped.insert(p);
                    }
                }
                abs_stack.clear();
                pc += bytecode_analysis::step(code, pc);
            }
            // putfield (0xb5): pops value then objectref; value escapes if tracked
            0xb5 => {
                let val = abs_stack.pop().flatten();
                abs_stack.pop(); // objectref (we don't care about its origin here)
                if let Some(p) = val {
                    escaped.insert(p);
                }
                pc += 3;
            }
            // getfield (0xb4): pop objectref, push field value (not a tracked new)
            0xb4 => {
                abs_stack.pop();
                abs_stack.push(None);
                pc += 3;
            }
            // aastore (0x53): pops value, index, array; value escapes if tracked
            0x53 => {
                let val = abs_stack.pop().flatten();
                abs_stack.pop(); // index
                abs_stack.pop(); // array
                if let Some(p) = val {
                    escaped.insert(p);
                }
                pc += 1;
            }
            // pop — discard top (no escape needed; the object stays local)
            0x57 => {
                abs_stack.pop();
                pc += 1;
            }
            // monitorenter (0xC2) / monitorexit (0xC3).
            //
            // Byte for byte the `pop` arm above, and that is the entire stack
            // contract of both opcodes: each consumes exactly one reference and
            // pushes nothing. No descriptor, no constant-pool resolution,
            // nothing this single linear pass cannot know. It is the easiest
            // exact arm in this function.
            //
            // # This is a LOOSENING, and the note it replaces said so
            //
            // Until 2026-09-22 both opcodes fell into the catch-all below,
            // which does not pop: it escapes every tracked operand, overwrites
            // every slot with `None`, and advances. Because a `synchronized(f)`
            // block always has `f` on the operand stack at the
            // `monitorenter`/`monitorexit` (`aload f; monitorenter`), EVERY
            // object that was ever the target of a `synchronized` block was
            // unconditionally escaped, however local its lifetime.
            //
            // Giving an opcode an exact arm removes the catch-all's incidental
            // SHIELD (see the invariant written on it), so every exact arm here
            // is justified by a named defect rather than by neatness. This one's
            // justification is `docs/feature-designs/jit-ir-optimizations.md
            // r10-ea-single-pass-monitor-scalar-relock-is-unreachable-FIXED-20260922.md`:
            // the backend carries an entire dedicated feature for relocking a
            // scalar-replaced object's monitor on deopt ("Phase C", below, plus
            // `x64/deopt_stubs.rs` and `op_object.rs`'s elision arm), and this
            // catch-all was the reason it could never receive a candidate.
            //
            // # Why keeping the receiver non-escaping is sound
            //
            // Only because of a coupling the surrounding code already enforces:
            // scalar-replacing a monitored object is safe ONLY if the monitor op
            // is also elided. A scalar-replaced `new` pushes a dummy zero
            // reference, so a surviving `jit_monitor_enter` on that receiver
            // would be a call on a null. `op_object.rs`'s `0xC2`/`0xC3` arm keys
            // its elision on `sr_monitor_scalar_ops`, which is derived from
            // `final_objects` — the same set `objects` is pruned to — so an
            // object that survives as a scalar has ALL of its monitor ops
            // elided, and one that is pruned or poisoned keeps both its real
            // allocation and its real locks. The two halves cannot disagree.
            //
            // Given that, each hazard resolves:
            //
            //  * CONTENTION / INFLATION. Impossible for a non-escaping object:
            //    no other thread holds a reference, so none can contend, and
            //    nothing can inflate a lock nobody else can reach.
            //  * `Object.wait()` / `notify()` / `hashCode()` /
            //    `Thread.holdsLock(o)`. All are `invokevirtual`/`invokestatic`,
            //    which hit this function's `escape_all!` arms. An object any of
            //    them is applied to is escaped, is not scalar-replaced, and
            //    keeps its real lock. Existing behaviour, not a new argument.
            //  * A SAFEPOINT INSIDE THE MONITOR. A GC has nothing to scan for an
            //    object that was never allocated; the lock it does not hold
            //    needs no root.
            //  * DEOPT WHILE HOLDING THE MONITOR. This is what Phase C exists
            //    for: `monitor_at[bci]` becomes
            //    `MonitorInfo { object: VirtualObjectRef(new_pc), lock_depth,
            //    relock: true }`, and the resume re-acquires the lock on the
            //    re-materialized object `lock_depth` times.
            //  * `IllegalMonitorStateException` FROM UNBALANCED LOCKING.
            //    Eliding removes a possible IMSE. For an object whose whole
            //    `new -> monitorenter -> monitorexit` lifetime is inside one
            //    region this pass admits at all, the locking is balanced by
            //    construction.
            //
            // # The arm is necessary and was NOT sufficient
            //
            // On its own it changed nothing for javac output, because a second
            // gate closed on the same population first: javac's mandatory
            // monitor handler makes `local_handler_reads_unsafe_local` true,
            // hence `precise_exception_frames` true, hence `non_escaping_for_sr`
            // the empty set in `x64/driver.rs`; and independently the `goto`
            // javac emits over that handler was a hard barrier that escaped the
            // receiver out of its locals. Both were removed in the same change
            // as this arm — EA-2 above for the `goto`, and the
            // precise-exception-frame admission in `x64/driver.rs` — which is
            // why this landed as one piece of work and not three.
            0xC2 | 0xC3 => {
                // t1 wave 2, lane M: under a monitor observer the lock must
                // stay real, so its object may not be scalar-replaced.
                if let Some(p) = abs_stack.pop().flatten() {
                    if keep_locks {
                        escaped.insert(p);
                    }
                }
                pc += 1;
            }
            // invokespecial (0xb7).
            //
            // A trivial `<init>()V` consumes exactly one slot (the dup'd
            // `this`) and does not let the receiver escape — the canonical
            // `new; dup; invokespecial <init>()V` pattern that scalar
            // replacement targets.
            //
            // ANY other invokespecial — an arg-bearing constructor
            // (`<init>(I)V`, …), a `super.m()` or `private` call — must:
            //   1. pop the correct number of operand slots (receiver +
            //      params), or the abstract stack desyncs and every
            //      object below the call is mis-tracked; and
            //   2. escape every tracked operand it consumes, because the
            //      callee writes the receiver's fields from a separate,
            //      un-inlined method body that the JIT frame cannot
            //      reproduce. Scalar-replacing such a receiver would drop
            //      those field initializations (observed as boxed values
            //      coming back as 0 — the `Integer.valueOf` /
            //      `String.toLowerCase` allocate-then-putfield miscompile).
            0xb7 => {
                match invokespecial_shapes.get(&pc) {
                    Some(shape) if shape.is_trivial_void_init => {
                        // `<init>()V`: pop only `this`; receiver does not escape.
                        abs_stack.pop();
                    }
                    Some(shape) => {
                        // Arg-bearing invokespecial: pop receiver + params,
                        // escaping any tracked object among them.
                        for _ in 0..shape.arg_slots {
                            if let Some(p) = abs_stack.pop().flatten() {
                                escaped.insert(p);
                            }
                        }
                        // Round 12 wave 2 (x64opt): the shape does not say
                        // whether the call RETURNS a value (a `super.m()` or a
                        // pre-nestmate `private` call does), so the stack can be
                        // one entry short of the real one from here on, and a
                        // later consumer then pops a tracked object BELOW the
                        // result in the result's place — `use(new T(),
                        // super.foo().f)` let `getfield` swallow `T`, and the
                        // call that really receives `T` escaped nothing. Escape
                        // and drop what is left, as the invokevirtual arm does,
                        // so no tracked object survives beneath an unknown-depth
                        // point. `plan_scalar_replacement` models the return and
                        // stays exact.
                        escape_all!();
                        abs_stack.clear();
                        abs_stack.push(None);
                    }
                    None => {
                        // Descriptor unknown — conservatively escape the
                        // whole operand stack and clear it.
                        escape_all!();
                        abs_stack.clear();
                    }
                }
                // The trivial `<init>()V` arm pushes nothing (void). The two
                // other arms leave no tracked object on the stack, so a result
                // they do or do not push cannot misattribute one.
                pc += 3;
            }
            // invokevirtual/invokeinterface/invokestatic — all args on the stack escape
            0xb6 | 0xb8 | 0xb9 => {
                escape_all!();
                abs_stack.clear();
                abs_stack.push(None);
                pc += if op == 0xb9 { 5 } else { 3 };
            }
            // EC-SCALAR-SOUNDNESS (bc math-ec JIT miscompile fix):
            //
            // This whole analysis is a SINGLE LINEAR forward pass over the
            // bytecode — it never resets/merges the abstract operand stack
            // or `local_origin` at basic-block boundaries. That is only sound
            // for STRAIGHT-LINE code. The instant control flow can branch
            // (conditional/goto/switch), throw, `ret`, or split via `jsr`,
            // the linear `abs_stack`/`local_origin` state diverges from the
            // real verification-time stack at the branch target. A genuinely
            // escaping store reached on a path the linear walk mis-models is
            // then attributed to the wrong `new` (or to none), so an object
            // that DOES escape is reported non-escaping and later scalar-
            // replaced. The scalar-replaced `new` pushes a dummy zero ref
            // (see the `0xbb` handler) and its real escaping store writes a
            // bad heap pointer — which the GC later follows and SEGVs on
            // (observed: bc `org.bouncycastle.math.ec` AllTests, fault read
            // @ ~0x31 off a near-page-aligned garbage base inside the
            // young-gen scavenge copy loop).
            //
            // Correctness fix: treat EVERY control-transfer instruction as a
            // hard escape barrier. Escape every object currently tracked on
            // the operand stack AND every object held in a local (any of
            // which could be `aload`ed and made to escape on an edge this
            // pass cannot follow), then clear all provenance. This confines
            // scalar replacement to objects whose entire `new; dup;
            // <init>()V; (putfield|getfield)*` lifecycle is provably within a
            // single straight-line region — exactly where the single-pass
            // abstract interpretation is sound. Straight-line allocation
            // sites (the common case this optimization targets) are
            // unaffected; only objects whose liveness crosses a CFG edge are
            // conservatively de-optimized.
            //
            //   0x99..=0xa8  ifeq..jsr (conditional branches, goto, jsr)
            //   0xa9         ret
            //   0xaa,0xab    table/lookupswitch
            //   0xbf         athrow
            //   0xc6,0xc7    ifnull / ifnonnull
            //   0xc8,0xc9    goto_w / jsr_w
            //
            // Plain *return (0xac/0xad/0xae/0xaf/0xb1) is deliberately NOT in
            // this list — it has its own arm below. (areturn 0xb0 is matched
            // earlier and escapes the returned object.)
            0x99..=0xa9 | 0xaa | 0xab | 0xbf | 0xc6 | 0xc7 | 0xc8 | 0xc9 => {
                // EA-2 (2026-09-22) — a forward `goto` whose target has exactly
                // one ordinary predecessor, and which skips only handler-only
                // code, is a straight-line continuation rather than a transfer.
                // `bytecode_analysis::straight_line_gotos` proves all three
                // conditions; the caller proves the fourth by passing the
                // handler pcs at all.
                //
                // This is the shape javac emits around every `synchronized`
                // block and every `try`/`finally`: the normal path jumps over
                // the catch-all handler to the instruction after it. Nothing
                // merges at that instruction and nothing in between runs, so the
                // state there IS the state here — but the barrier could not see
                // it, and escaped every object held in a local for a transfer
                // that transfers nothing.
                //
                // The `goto` is followed rather than stepped over: `pc` becomes
                // the target, so the handler-only region in between is never
                // walked. That matters as much as keeping the state — the
                // handler's own `astore <e>` would otherwise overwrite a local's
                // provenance on a path that cannot reach it.
                if straight_gotos.get(pc).copied().unwrap_or(false) {
                    if let Some(t) = bytecode_analysis::offset_branch_target(code, pc) {
                        pc = t;
                        continue;
                    }
                }
                escape_all!();
                for slot in local_origin.iter_mut() {
                    if let Some(p) = slot.take() {
                        escaped.insert(p);
                    }
                }
                abs_stack.clear();
                pc += bytecode_analysis::step(code, pc);
            }
            // Plain (non-areturn) returns: ireturn/lreturn/freturn/dreturn/
            // return. A method-exit return ENDS the current path — it is NOT a
            // CFG-divergence point, so unlike the branch barrier above it must
            // NOT escape locals: an object held in a local here dies with the
            // frame, it does not escape. Escaping locals here too (as the prior
            // over-broad barrier that lumped returns in with branches did)
            // falsely de-optimises the straight-line `new X(); use; return
            // <primitive|void>` idiom — contradicting this pass's "straight-line
            // allocation sites are unaffected" contract. Operand-stack objects
            // (rare at a non-object return — the return value is a
            // primitive/void) are escaped defensively.
            //
            // r9-ea: the locals are now DROPPED here as well, not merely left
            // intact. A return has no linear successor: the next pc is reached
            // only through a branch (whose own instruction already escaped the
            // state it carries) or as an exception handler. Leaving the
            // provenance in place handed it to the branch-target barrier at the
            // next pc, which escaped it — so the common early-exit shape
            // `if (c) { Foo f = new Foo(); …; return f.x; } …` lost `f` to a
            // barrier on a path `f` never reaches.
            //
            // A handler entry after a return does not need the dropped state:
            // scalar replacement is only planned when the method has no
            // precise exception frames (`x64/driver.rs`), and without them a
            // handler never runs in this compiled frame, and a handler that
            // reads a non-parameter local is refused compilation outright
            // (`local_handler_reads_unsafe_local`). `plan_scalar_replacement`
            // drops the same state at the same instruction, so the two walks
            // still agree.
            0xac | 0xad | 0xae | 0xaf | 0xb1 => {
                escape_all!();
                abs_stack.clear();
                for slot in local_origin.iter_mut() {
                    *slot = None;
                }
                pc += bytecode_analysis::step(code, pc);
            }
            // nop — no stack effect; must NOT disturb tracked provenance.
            0x00 => {
                pc += 1;
            }
            // Primitive loads / constants / getstatic — each pushes exactly ONE
            // untracked operand (a primitive, a constant-pool constant, or a
            // static-field value; never a `new`-tracked object) and pops
            // nothing. Push a single `None` slot WITHOUT touching the
            // provenance of objects already on the operand stack.
            //
            // The catch-all `_` arm below forgets EVERY slot's provenance, which
            // silently de-tracked an object loaded just before a primitive arg —
            // the canonical `aload obj; iload prim; invoke(obj, prim)`. The
            // `escape_all!` at the call then missed `obj`, so an object that
            // truly escapes-to-callee was reported non-escaping and scalar-
            // replaced. kafka bug-25: `MessageUtil.toByteBuffer` does
            // `aload_2 cache; iload_1 version; invokeinterface Message.size`,
            // and the `iload_1` erased `cache`'s provenance → the
            // ObjectSerializationCache was scalar-replaced (never allocated) →
            // `size()` received a null cache → "Cannot invoke cacheSerializedValue
            // on null". (long/double are one 64-bit slot in this model — same as
            // `dup2`'s FORM-2 handling — so lload/dload/lconst/dconst/ldc2_w push
            // one slot too.) `aload`/`aload_n` and `getfield`/`new`/`dup` keep
            // their own arms above; this range deliberately excludes them.
            0x01..=0x18 | 0x1a..=0x29 | 0xb2 => {
                abs_stack.push(None);
                pc += bytecode_analysis::step(code, pc);
            }
            // For all other opcodes, use `bytecode_analysis::step` for PC advance.
            //
            // EC-SCALAR-SOUNDNESS (varargs-ctor receiver fix): an opcode this
            // single pass does not model precisely. We cannot know whether it
            // consumes, stores, or otherwise lets a tracked object escape — so
            // the only SOUND action is to ESCAPE every tracked object currently
            // on the operand stack, then forget provenance (keeping depth
            // approximately correct).
            //
            // Merely forgetting provenance (the previous behaviour) was UNSOUND:
            // an unmodeled opcode sitting ABOVE a tracked `new`-receiver on the
            // operand stack wiped that receiver's provenance, so a later
            // arg-bearing `invokespecial <init>` no longer saw it as tracked and
            // never escaped it. The classic trigger is a varargs constructor
            // call `new C(a, b)` which javac compiles to
            //   new C; dup; iconst_n; anewarray; (dup;…;aastore)*; invokespecial C.<init>([…])V
            // The `anewarray` (0xbd) — not modeled here — erased the dup'd
            // receiver's provenance; the `<init>` then escaped nothing; the
            // receiver was reported non-escaping, scalar-replaced to a dummy
            // null (see the 0xbb handler), and passed as `this` to the
            // un-inlined constructor → "Cannot assign field … because \"this\"
            // is null" (Spring `ResourceDatabasePopulator` varargs ctor, BUG-05).
            //
            // Escaping (rather than forgetting) is purely soundness-restoring:
            // it can only cause MORE objects to be heap-allocated normally,
            // never fewer — it never changes a correctly-allocated object into a
            // scalar-replaced one. Straight-line allocation sites built only
            // from modeled opcodes are unaffected.
            //
            // # The depth invariant this arm maintains (r10-earelock)
            //
            // This arm does NOT pop. An unmodeled opcode that really consumes
            // `k` slots leaves `abs_stack` exactly `k` entries deeper than the
            // real verification-time stack from here on. That is stated as an
            // invariant rather than a bug because it is load-bearing for the
            // soundness argument above, and because a future arm that "fixes"
            // the depth is a LOOSENING, not a tidy-up:
            //
            //   * every surplus entry is `None` (this arm overwrites the whole
            //     stack with `None` before leaving), and
            //   * `Vec::pop` on an exhausted stack yields `None` too,
            //
            // so a later consumer that reaches into the surplus reads the same
            // `None` it would have read from an exact model, and the surplus can
            // never turn a tracked object into an untracked one. The one thing
            // it CAN do is shield a tracked object from a consumer that would
            // not have escaped it (`pop`, `astore`) — and that direction ends in
            // MORE escapes, which is the safe one. Giving an opcode an exact arm
            // removes that shield, which is why every exact arm in this function
            // is justified by a named miscompile rather than by neatness.
            //
            // # `monitorenter` (0xC2) / `monitorexit` (0xC3) NO LONGER land here
            //
            // They had their own arm as of 2026-09-22, and it is worth knowing
            // why they were here for so long, because the answer was never
            // "the arm would be unsound". It was "the arm would change
            // nothing": two further gates closed on the same population first,
            // and removing one of three gates buys nothing measurable. All
            // three went in one change — see the arm's own comment, and
            // `docs/feature-designs/jit-ir-optimizations.md` for the page that had them counted.
            //
            // The lesson that outlives them: an opcode sitting in this
            // catch-all is not evidence that modelling it is hard, and a
            // catch-all entry is not a decision anybody made. Adding an exact
            // arm is a LOOSENING (it removes the shield described above), so it
            // needs a named defect behind it -- but the absence of an arm needs
            // a reason too, and "nobody noticed" is how a whole dedicated
            // feature shipped unreachable for a year.
            _ => {
                let len = bytecode_analysis::step(code, pc);
                for slot in abs_stack.iter() {
                    if let Some(p) = slot {
                        escaped.insert(*p);
                    }
                }
                for slot in abs_stack.iter_mut() {
                    *slot = None; // forget provenance, but keep stack depth correct
                }
                pc += len;
            }
        }
    }

    // Return the set of new_pcs that did NOT escape.
    let mut non_escaping = std::collections::HashSet::new();
    let mut pc = 0usize;
    while pc < code_len {
        if code[pc] == 0xbb && !escaped.contains(&pc) {
            non_escaping.insert(pc);
        }
        pc += bytecode_analysis::step(code, pc);
    }
    non_escaping
}

// ---------------------------------------------------------------------------
// Scalar replacement: eliminate heap allocations for non-escaping objects
// ---------------------------------------------------------------------------

/// Metadata for a scalar-replaced object whose fields live in the JIT frame.
#[derive(Clone, Debug)]
pub(super) struct ScalarReplacedObject {
    pub(super) num_fields: usize,
    /// Frame offset of field slot 0: `[RBP - field_base_offset]`.
    /// Field `i` starts at `[RBP - (field_base_offset + i * SLOT_SIZE)]`, matching
    /// heap object layout (`HEADER_SIZE + i * SLOT_SIZE` from `jit_getfield`).
    pub(super) field_base_offset: i32,
    /// Resolved class id of the eliminated `new`. Phase B (real-frame-deopt x64
    /// backport): required to emit a `FrameValue::VirtualObject` so a guard deopt
    /// can re-materialize the elided object. Available in `new_info` and captured
    /// here; otherwise dropped.
    pub(super) class_id: u32,
}

/// Result of scalar replacement planning: which bytecode PCs to rewrite.
pub(super) struct ScalarReplacementPlan {
    /// Non-escaping NEW PCs → their scalar-replaced object info.
    pub(super) objects: FxHashMap<usize, ScalarReplacedObject>,
    /// putfield/getfield PCs that operate on a scalar-replaced object → the NEW PC.
    pub(super) field_ops: FxHashMap<usize, usize>,
    /// invokespecial PCs whose `<init>()V` call should be skipped.
    pub(super) init_skips: std::collections::HashSet<usize>,
    /// Total 8-byte frame slots reserved for scalar-replaced fields
    /// (`num_fields * (SLOT_SIZE / 8)` per object).
    pub(super) total_slots: usize,
    /// Phase B (real-frame-deopt x64 backport): per-bytecode-PC snapshot of which
    /// LOCALS hold a live scalar-replaced object at that PC — `(local_index,
    /// new_pc)` pairs, recorded only for PCs where at least one local is a scalar
    /// object. Captured during the locals abs-interp below (same `local_prov`
    /// tracking, same branch-barrier reset). A deopt snapshot at `bci` consults
    /// `local_prov_at[bci]` to emit `VirtualObject`/`VirtualObjectRef` for those
    /// locals. Scalar objects never reach the operand stack (a call-arg escapes),
    /// so only per-local provenance is needed.
    pub(super) local_prov_at: FxHashMap<usize, Vec<(usize, usize)>>,
    /// Phase C (monitors): per-bytecode-PC snapshot of the scalar monitors held at
    /// that PC — `(new_pc, lock_depth)` pairs. A deopt snapshot at `bci` consults
    /// `monitor_at[bci]` to emit `MonitorInfo` so the resume re-acquires the elided
    /// lock on the re-materialized object.
    pub(super) monitor_at: FxHashMap<usize, Vec<(usize, u32)>>,
    /// Phase C: monitorenter/monitorexit PCs whose receiver is a scalar object
    /// (relockable on deopt). A monitor op NOT in this set operated on a
    /// non-scalar object, which WOULD keep the method off the resume path — but
    /// no such elision happens: `has_elided_monitor` is never set to `true`
    /// (`r10-earelock-has-elided-monitor-is-never-set-FIXED-20260922.md`).
    pub(super) monitor_scalar_ops: std::collections::HashSet<usize>,
    /// STR920-1 (JIT round 19 wave 11, lane hoist11): every object
    /// `analyze_escapes` admitted that this walk POISONED -- a drift between
    /// the pair's two models (see `poisoned`), with the pc and opcode that
    /// poisoned it. Collected only under `CRATONVM_DBG_SCALAR_DEOPT` (empty
    /// otherwise); diagnostic only, it changes no plan.
    pub(super) pair_drift: Vec<EaPairDrift>,
    /// STR920-4 (JIT round 19 wave 14): how many admitted objects this walk
    /// poisoned, counted on every plan (the census above names them only
    /// under its flag). Zero whenever the pair agrees; the driver
    /// `debug_assert!`s it for the set `analyze_escapes` produced.
    pub(super) poisoned_admitted: usize,
}

/// STR920-1: one object the single-pass plan walk poisoned although
/// `analyze_escapes` admitted it (`ScalarReplacementPlan::pair_drift`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct EaPairDrift {
    /// The object's `new` pc.
    pub(super) new_pc: usize,
    /// The pc the walk was at when it poisoned the object.
    pub(super) pc: usize,
    /// `code[pc]`.
    pub(super) opcode: u8,
    /// `opcode` (the instruction's own arm), `merge-barrier` (a branch target
    /// cleared the provenance before the instruction), or
    /// `monitor-without-local` (a held scalar monitor no local names).
    pub(super) reason: &'static str,
}

/// Analyze bytecode to plan scalar replacement for non-escaping objects.
///
/// Uses abstract interpretation to track object provenance through the stack
/// and locals, identifying which putfield/getfield/invokespecial PCs operate
/// on scalar-replaced objects.
///
/// Test fixture only (STR819-2): it stands in for a resolver that PROVED every
/// `<init>()V` site in `invoke_info` empty, which is what the plan tests built
/// on it model. The compiler calls [`plan_scalar_replacement_with_handlers`]
/// with the resolver's real `elidable_init_pcs`.
#[cfg(test)]
pub(super) fn plan_scalar_replacement(
    code: &[u8],
    code_len: usize,
    non_escaping_new: &std::collections::HashSet<usize>,
    new_info: &[(usize, u32, usize, bool, bool)],
    invoke_info: &[(usize, *const JitInvokeInfo)],
    scalar_base: usize,
) -> ScalarReplacementPlan {
    let proven: std::collections::HashSet<usize> = invoke_info
        .iter()
        .filter(|&&(_, ptr)| {
            // SAFETY: test callers pass leaked, live `JitInvokeInfo`s.
            let info = unsafe { &*ptr };
            info.method_name == "<init>" && info.descriptor == "()V"
        })
        .map(|&(pc, _)| pc)
        .collect();
    plan_scalar_replacement_with_handlers(
        code,
        code_len,
        non_escaping_new,
        new_info,
        invoke_info,
        scalar_base,
        None,
        Some(&proven),
    )
}

/// [`plan_scalar_replacement`] with the method's exception-handler entry pcs.
///
/// This walk and [`analyze_escapes`] are a matched pair over the same bytecode,
/// and their agreement is load-bearing: an object the first admits and the
/// second models differently is half-replaced, which is a real field access on
/// a dummy-zero receiver. So the `goto`-over-handler see-through EA-2 gave
/// `analyze_escapes` has to arrive here in the same change, with the same
/// conditions computed by the same function, or the pair drifts in exactly the
/// direction the `poisoned` comment below is four paragraphs about.
///
/// `handler_pcs = None` disables the see-through, as it does there. Callers
/// must pass the SAME value to both, which `x64/driver.rs` does.
///
/// `proven_empty_inits` is the same proof `analyze_escapes` was given
/// (`elidable_init_pcs` in `x64/driver.rs`, the source of every
/// [`InvokeSpecialShape::is_trivial_void_init`]); `None` proves nothing. Only a
/// `<init>()V` pc in it is ever recorded as an init skip (STR819-2). A tracked
/// receiver reaching any other `()V` constructor is poisoned instead -- the
/// descriptor alone is not evidence of an empty body, and `analyze_escapes`'
/// arg-bearing arm has already escaped that receiver, so with the pair in
/// agreement the poison never fires and the plan is the one the old
/// descriptor-only rule produced.
#[allow(clippy::too_many_arguments)]
pub(super) fn plan_scalar_replacement_with_handlers(
    code: &[u8],
    code_len: usize,
    non_escaping_new: &std::collections::HashSet<usize>,
    new_info: &[(usize, u32, usize, bool, bool)],
    invoke_info: &[(usize, *const JitInvokeInfo)],
    scalar_base: usize,
    handler_pcs: Option<&[usize]>,
    proven_empty_inits: Option<&std::collections::HashSet<usize>>,
) -> ScalarReplacementPlan {
    let empty = ScalarReplacementPlan {
        objects: FxHashMap::default(),
        field_ops: FxHashMap::default(),
        init_skips: std::collections::HashSet::new(),
        total_slots: 0,
        local_prov_at: FxHashMap::default(),
        monitor_at: FxHashMap::default(),
        monitor_scalar_ops: std::collections::HashSet::new(),
        pair_drift: Vec::new(),
        poisoned_admitted: 0,
    };
    if non_escaping_new.is_empty() {
        return empty;
    }
    // AFTER the early return, not before it. `x64/driver.rs` calls this on every
    // compile, and the overwhelming majority hand it an empty set -- paying for
    // a reachability walk and a predecessor pass to then return `empty` is a
    // per-compile cost for nothing.
    let (straight_gotos, barrier_exempt) = straight_line_goto_maps(code, code_len, handler_pcs);

    // Build objects map with deterministic frame offset assignment.
    let mut objects: FxHashMap<usize, ScalarReplacedObject> = FxHashMap::default();
    let mut total_slots = 0usize;
    let mut sorted_pcs: Vec<usize> = non_escaping_new.iter().copied().collect();
    sorted_pcs.sort();
    // PERF: index new_info by PC once (O(new_info_len)) instead of doing a
    // linear `new_info.iter().find(|(p,..)| *p == new_pc)` inside the loop
    // below — that was O(sorted_pcs.len() * new_info.len()). To preserve the
    // exact prior behavior, where `find` returns the FIRST matching entry, we
    // keep the first occurrence on duplicate PCs (`entry(..).or_insert(..)`).
    // Phase B: carry `class_id` alongside `num_fields` so the scalar-replaced
    // object descriptor can drive `VirtualObject` materialization on deopt.
    let mut new_info_by_pc: FxHashMap<usize, (u32, usize)> = FxHashMap::default();
    new_info_by_pc.reserve(new_info.len());
    for &(p, class_id, num_fields, _, _) in new_info {
        new_info_by_pc.entry(p).or_insert((class_id, num_fields));
    }
    for &new_pc in &sorted_pcs {
        if let Some(&(class_id, num_fields)) = new_info_by_pc.get(&new_pc) {
            if num_fields > 0 && num_fields <= 16 {
                let field_base_offset = ((scalar_base + total_slots) as i32 + 1) * 8; // Cast: x86-64 immediate encoding
                objects.insert(
                    new_pc,
                    ScalarReplacedObject {
                        num_fields,
                        field_base_offset,
                        class_id,
                    },
                );
                total_slots += num_fields * (SLOT_SIZE / 8);
            }
        }
    }
    if objects.is_empty() {
        return empty;
    }

    // Abstract interpretation: track object provenance through stack and locals.
    let mut abs_stack: Vec<Option<usize>> = Vec::with_capacity(16);
    let mut local_prov: [Option<usize>; 256] = [None; 256];
    let mut field_ops: FxHashMap<usize, usize> = FxHashMap::default();
    // `<init>()V` pc → the `new` pc whose receiver it initialises. Keyed by the
    // provenance the walk actually saw, not by the `init_pc - 4` arithmetic the
    // pruning below used to reverse it with: that only names the owner for the
    // canonical `new; dup; invokespecial` layout. A receiver reached any other
    // way (`new; astore; aload; invokespecial`) lost its skip in the pruning
    // while its `new` still became a dummy zero, so the proven-empty
    // constructor was CALLED on a null receiver.
    let mut init_skip_owner: FxHashMap<usize, usize> = FxHashMap::default();
    // Objects whose provenance this walk LOST while they were still live (on
    // the operand stack or in a local) — at a barrier, an unmodelled opcode or
    // a call. Such an object is never scalar-replaced.
    //
    // # Why this exists (r9-ea)
    //
    // This walk and `analyze_escapes` are two separate models of the same
    // bytecode, and they had drifted. `analyze_escapes` keeps local provenance
    // across `nop`, `dup2_x1`/`dup2_x2`, `invokedynamic`, `multianewarray` and
    // `wide`; this walk's catch-all forgot every local at each of them. An
    // object stored in a local before, say, a string-concatenation
    // `invokedynamic` was therefore proven non-escaping and given a dummy-zero
    // `new`, while every `getfield`/`putfield` on it AFTER the indy was left
    // unmapped — a real field access on a null receiver, i.e. a spurious NPE.
    //
    // The specific opcodes are now modelled below, but the general rule is
    // what makes the pair robust: a scalar object must have its provenance
    // for its whole life, so the moment this walk drops a live one, the object
    // is demoted to an ordinary allocation — which is always correct — rather
    // than half-replaced.
    let mut poisoned: FxHashSet<usize> = FxHashSet::default();
    // Every live provenance on the operand stack (`poison_stack!`) or in a
    // local (`poison_locals!`) is poisoned. Called immediately BEFORE the walk
    // clears either.
    macro_rules! poison_stack {
        () => {
            for p in abs_stack.iter().flatten() {
                poisoned.insert(*p);
            }
        };
    }
    macro_rules! poison_locals {
        () => {
            for p in local_prov.iter().flatten() {
                poisoned.insert(*p);
            }
        };
    }
    // Phase B: per-PC snapshot of locals holding a live scalar object.
    let mut local_prov_at: FxHashMap<usize, Vec<(usize, usize)>> = FxHashMap::default();
    // Phase C (monitors): running monitor recursion depth per scalar object
    // (`new_pc → depth`), the per-PC snapshot of held scalar monitors, and the set
    // of monitorenter/exit PCs whose receiver IS a scalar object. A monitor op over
    // a scalar object is relockable on deopt (recorded here); one over a
    // non-scalar (escaping) object is the pre-existing blanket-elision case,
    // which WOULD keep the method off the resume path via `has_elided_monitor`
    // in codegen — except that flag is never set, because that elision never
    // happens (`r10-earelock-has-elided-monitor-is-never-set-FIXED-20260922.md`).
    let mut mon_depth: FxHashMap<usize, u32> = FxHashMap::default();
    let mut monitor_at: FxHashMap<usize, Vec<(usize, u32)>> = FxHashMap::default();
    // `(monitor pc, new pc)` for every monitor op on a tracked object. Filtered
    // to the surviving objects at the end, exactly as `local_prov_at` and
    // `monitor_at` are: a monitor op whose object was pruned or poisoned
    // operates on a REAL allocation, and eliding its lock while no deopt
    // record relocks it would resume the interpreter outside a monitor its
    // bytecode is about to exit.
    let mut monitor_scalar_op_owner: Vec<(usize, usize)> = Vec::new();

    // EC-SCALAR-SOUNDNESS: same branch-target barrier as `analyze_escapes`.
    // Objects in `objects` are already guaranteed (by the stricter
    // `analyze_escapes`) to live within a single straight-line region, so
    // clearing all provenance at every merge point can never strip a *valid*
    // field-op mapping — it only prevents a stale `local_prov`/`abs_stack`
    // entry from binding a post-branch field op to the wrong scalar object.
    // Anything still live here anyway is poisoned rather than silently dropped.
    let branch_targets = bytecode_analysis::branch_target_map(code, code_len);

    // STR920-1 (JIT round 19 wave 11, lane hoist11): name every poisoning.
    // `poisoned` only ever holds `objects` keys, i.e. objects
    // `analyze_escapes` put in `non_escaping_new`, so each one is a place the
    // two models disagree. Attributed by set difference at three points per
    // instruction (cheap: the set is a handful of pcs, and the whole census is
    // off unless `CRATONVM_DBG_SCALAR_DEOPT` is on).
    let drift_census = false;
    let mut pair_drift: Vec<EaPairDrift> = Vec::new();
    macro_rules! attribute_drift {
        ($at:expr, $reason:expr) => {
            if drift_census && poisoned.len() != pair_drift.len() {
                let mut fresh: Vec<usize> = poisoned
                    .iter()
                    .copied()
                    .filter(|p| !pair_drift.iter().any(|d| d.new_pc == *p))
                    .collect();
                fresh.sort_unstable();
                let at: usize = $at;
                for new_pc in fresh {
                    pair_drift.push(EaPairDrift {
                        new_pc,
                        pc: at,
                        opcode: code.get(at).copied().unwrap_or(0),
                        reason: $reason,
                    });
                }
            }
        };
    }
    let mut last_pc = 0usize;

    let mut pc = 0usize;
    while pc < code_len {
        // STR920-1: whatever the previous instruction's arm poisoned.
        attribute_drift!(last_pc, "opcode");
        last_pc = pc;
        // EA-2: exempt exactly the targets `analyze_escapes` exempts. See
        // `straight_line_goto_maps`.
        if branch_targets.get(pc).copied().unwrap_or(false)
            && !barrier_exempt.get(pc).copied().unwrap_or(false)
        {
            poison_stack!();
            poison_locals!();
            abs_stack.clear();
            for prov in local_prov.iter_mut() {
                *prov = None;
            }
            // Phase C: a monitored scalar object cannot cross a CFG edge (it would
            // escape and not be scalar-replaced), so any depth still standing at a
            // merge point is stale tracking — clear it (mirrors the provenance
            // barrier above). A correctly-balanced `synchronized` block exits before
            // its back-edge, so this never drops a genuinely-held monitor.
            mon_depth.clear();
        }
        // Phase C: snapshot the scalar monitors held at the ENTRY of this PC
        // (after the barrier, before the opcode), recorded only when non-empty.
        //
        // r10-earelock: an entry recorded here becomes
        // `MonitorInfo { object: FrameValue::VirtualObjectRef(new_pc), .. }` in
        // `x64/deopt_stubs.rs`'s `build_frame_state_at`. A `VirtualObjectRef` is
        // an INTRA-FRAME id edge, not a value: it resolves only against the
        // `FrameValue::VirtualObject(new_pc)` that some LOCAL of the same frame
        // state defines, and the locals lane of that frame state is built from
        // `local_prov_at[bci]` alone. So a pc at which a scalar monitor is held
        // but NO local holds the object publishes a monitor entry pointing at a
        // shell nothing defines — `deopt::DeoptMetadataError::
        // UndefinedVirtualObjectRef` is exactly that defect.
        //
        // That defect is now caught on BOTH backends, which it was not when the
        // demotion below was written: round 10 wave 6 wired `DeoptVerifier` into
        // the single-pass finalize path too
        // (`x64/driver.rs`'s `verify_deopt_metadata` call, whose refusal records
        // the `deopt-metadata-violation` bail site), so the sentence that used
        // to sit here — "nothing in the single-pass backend would catch it:
        // `DeoptVerifier` is only ever run by `ir_lower.rs`" — is no longer
        // true. See
        // `docs/feature-designs/deopt-osr.md`.
        // The demotion stays: a producer that does not emit the malformed frame
        // is better than an install-time refusal of the whole artifact, and the
        // verifier is the backstop for the shapes this reasoning missed.
        //
        // The shape that produces it is `new; dup; <init>()V; dup; monitorenter`
        // — the receiver is locked straight off the operand stack and never
        // stored. `analyze_escapes` cannot currently hand such an object over
        // (see the `0xC2`/`0xC3` note on its catch-all), so this is a latent
        // defect rather than an observed one, and it is fixed by DEMOTION: the
        // object is poisoned, so the pruning below drops it from `final_objects`
        // and with it every monitor op, field op and init skip that named it.
        // The `new` is then allocated for real and the lock is really taken —
        // the same all-or-nothing coupling `final_monitor_scalar_ops` already
        // enforces in the other direction. Read, not executed.
        {
            let held: Vec<(usize, u32)> = mon_depth
                .iter()
                .filter(|(_, &d)| d > 0)
                .map(|(&np, &d)| (np, d))
                .collect();
            if !held.is_empty() {
                // STR920-1: the merge barrier above, attributed first.
                attribute_drift!(pc, "merge-barrier");
                for &(np, _) in &held {
                    if !local_prov.iter().any(|p| *p == Some(np)) {
                        poisoned.insert(np);
                    }
                }
                attribute_drift!(pc, "monitor-without-local");
                monitor_at.insert(pc, held);
            }
        }
        // STR920-1: a merge barrier at this pc (a no-op when the monitor
        // block above already attributed it).
        attribute_drift!(pc, "merge-barrier");
        // Phase B: snapshot which locals hold a live scalar object at the ENTRY
        // of this PC (after the branch barrier, before the opcode executes) — the
        // state a deopt snapshot taken at `bci == pc` must see. Recorded only when
        // non-empty so the map stays small. A `local_prov[i]` is only ever
        // `Some(new_pc)` for a `new_pc` in `objects` (set exclusively from the
        // `new`/aload/astore/dup provenance below), so each pair names a real
        // scalar-replaced object.
        {
            let live: Vec<(usize, usize)> = local_prov
                .iter()
                .enumerate()
                .filter_map(|(i, p)| p.map(|np| (i, np)))
                .collect();
            if !live.is_empty() {
                local_prov_at.insert(pc, live);
            }
        }
        let op = code[pc];
        match op {
            // new
            0xBB => {
                abs_stack.push(if objects.contains_key(&pc) {
                    Some(pc)
                } else {
                    None
                });
                pc += 3;
            }
            // dup
            0x59 => {
                let top = abs_stack.last().copied().flatten();
                abs_stack.push(top);
                pc += 1;
            }
            // aconst_null
            0x01 => {
                abs_stack.push(None);
                pc += 1;
            }
            // iconst_m1..iconst_5
            0x02..=0x08 => {
                abs_stack.push(None);
                pc += 1;
            }
            // lconst_0, lconst_1
            0x09 | 0x0A => {
                abs_stack.push(None);
                pc += 1;
            }
            // fconst_0..fconst_2
            0x0B..=0x0D => {
                abs_stack.push(None);
                pc += 1;
            }
            // dconst_0, dconst_1
            0x0E | 0x0F => {
                abs_stack.push(None);
                pc += 1;
            }
            // bipush
            0x10 => {
                abs_stack.push(None);
                pc += 2;
            }
            // sipush
            0x11 => {
                abs_stack.push(None);
                pc += 3;
            }
            // ldc
            0x12 => {
                abs_stack.push(None);
                pc += 2;
            }
            // ldc_w, ldc2_w
            0x13 | 0x14 => {
                abs_stack.push(None);
                pc += 3;
            }
            // iload, lload, fload, dload
            0x15..=0x18 => {
                abs_stack.push(None);
                pc += 2;
            }
            // aload
            //
            // r10-earelock: the operand byte is read through a bounds test, as
            // `analyze_escapes`' own `0x19` arm has always done. This walk read
            // `code[pc + 1]` bare, so a method whose LAST byte is a truncated
            // `aload` panicked the compiler instead of bailing it. That is a
            // permanent decline rather than one lost compile attempt
            // (`jit/tests/panic_free_compile_ratchet.rs` argues why the two are
            // not the same outcome), and this module's header declares a
            // no-panic gate. The guard cannot change the answer for any
            // well-formed method: `pc + 1 < code_len` fails only at a final
            // instruction the verifier would already have rejected. (The old
            // `idx < 256` test was dead — a `u8` index is always in range for a
            // 256-entry array.) Read, not executed.
            0x19 => {
                abs_stack.push(if pc + 1 < code_len {
                    local_prov[code[pc + 1] as usize] // Widening: always safe
                } else {
                    None
                });
                pc += 2;
            }
            // iload_0..3, lload_0..3, fload_0..3, dload_0..3
            0x1A..=0x29 => {
                abs_stack.push(None);
                pc += 1;
            }
            // aload_0..3
            0x2A..=0x2D => {
                let idx = (op - 0x2A) as usize; // Widening: always safe
                abs_stack.push(local_prov[idx]);
                pc += 1;
            }
            // Xaload (iaload..saload): pop index + arrayref, push value
            0x2E..=0x35 => {
                abs_stack.pop();
                abs_stack.pop();
                abs_stack.push(None);
                pc += 1;
            }
            // istore, lstore, fstore, dstore.
            //
            // r9-ea: the overwritten slot (two for lstore/dstore) no longer
            // holds the object. It used to keep its provenance, so every later
            // `local_prov_at` snapshot went on naming the dead object in a slot
            // that now holds a primitive — and a deopt there materialised a
            // `VirtualObject` into an int local. javac reuses a slot this way
            // for sibling block scopes in straight-line code.
            //
            // r10-earelock: bounds-guarded, as the twin arm in
            // `analyze_escapes` already is — see the `0x19` arm above for why a
            // truncated tail instruction must bail rather than panic.
            0x36..=0x39 => {
                if let Some(p) = abs_stack.pop().flatten() {
                    poisoned.insert(p);
                }
                if pc + 1 < code_len {
                    let idx = code[pc + 1] as usize; // Widening: always safe
                    local_prov[idx] = None;
                    if matches!(op, 0x37 | 0x39) && idx + 1 < local_prov.len() {
                        local_prov[idx + 1] = None;
                    }
                }
                pc += 2;
            }
            // astore (r10-earelock: bounds-guarded, as above; `analyze_escapes`
            // likewise pops only inside the guard, so the two walks still agree
            // instruction for instruction on a truncated tail).
            0x3A => {
                if pc + 1 < code_len {
                    let idx = code[pc + 1] as usize; // Widening: always safe
                    let val = abs_stack.pop().flatten();
                    local_prov[idx] = val;
                }
                pc += 2;
            }
            // istore_0..3 (0x3B..=0x3E), lstore_0..3 (0x3F..=0x42),
            // fstore_0..3 (0x43..=0x46), dstore_0..3 (0x47..=0x4A). Same slot
            // clearing as the indexed forms above.
            0x3B..=0x4A => {
                if let Some(p) = abs_stack.pop().flatten() {
                    poisoned.insert(p);
                }
                let idx = ((op - 0x3B) % 4) as usize; // Widening: always safe
                local_prov[idx] = None;
                if matches!(op, 0x3F..=0x42 | 0x47..=0x4A) {
                    local_prov[idx + 1] = None;
                }
                pc += 1;
            }
            // astore_0..3
            0x4B..=0x4E => {
                let idx = (op - 0x4B) as usize; // Widening: always safe
                let val = abs_stack.pop().flatten();
                local_prov[idx] = val;
                pc += 1;
            }
            // Xastore (iastore..sastore): pop value, index, arrayref. A tracked
            // value stored by `aastore` is published — `analyze_escapes`
            // escapes it, so one reaching here is a disagreement: poison it.
            0x4F..=0x56 => {
                if let Some(p) = abs_stack.pop().flatten() {
                    poisoned.insert(p);
                }
                abs_stack.pop();
                abs_stack.pop();
                pc += 1;
            }
            // `wide iinc` — no stack effect, no reference touched; modelled
            // exactly as `analyze_escapes` models it (r9-ea). Every other `wide`
            // form still reaches the catch-all, which poisons what is live —
            // and `analyze_escapes` escapes the same state there.
            0xC4 if pc + 1 < code_len && code[pc + 1] == 0x84 => {
                pc += 6;
            }
            // nop — no stack effect. Explicit (r9-ea) because the catch-all
            // drops every provenance, which `analyze_escapes` does not do at a
            // `nop`; see `poisoned`.
            0x00 => {
                pc += 1;
            }
            // pop
            0x57 => {
                abs_stack.pop();
                pc += 1;
            }
            // pop2
            0x58 => {
                abs_stack.pop();
                abs_stack.pop();
                pc += 1;
            }
            // dup_x1
            0x5A => {
                let v1 = abs_stack.pop().flatten();
                let v2 = abs_stack.pop().flatten();
                abs_stack.push(v1);
                abs_stack.push(v2);
                abs_stack.push(v1);
                pc += 1;
            }
            // dup_x2
            0x5B => {
                let v1 = abs_stack.pop().flatten();
                let v2 = abs_stack.pop().flatten();
                let v3 = abs_stack.pop().flatten();
                abs_stack.push(v1);
                abs_stack.push(v3);
                abs_stack.push(v2);
                abs_stack.push(v1);
                pc += 1;
            }
            // dup2
            0x5C => {
                let len = abs_stack.len();
                let v1 = if len >= 1 { abs_stack[len - 1] } else { None };
                let v2 = if len >= 2 { abs_stack[len - 2] } else { None };
                abs_stack.push(v2);
                abs_stack.push(v1);
                pc += 1;
            }
            // swap
            0x5F => {
                let len = abs_stack.len();
                if len >= 2 {
                    abs_stack.swap(len - 1, len - 2);
                }
                pc += 1;
            }
            // dup2_x1 / dup2_x2 (r9-ea). Their shape depends on operand
            // categories this walk does not track, so the STACK model is
            // dropped — but they touch no local, so local provenance survives,
            // exactly as it does in `analyze_escapes` (whose catch-all escapes
            // the stack operands of both). They used to reach this walk's
            // catch-all, which dropped the locals too: javac emits `dup2_x1` for
            // a `long`/`double` field compound assignment used as an expression.
            0x5D | 0x5E => {
                poison_stack!();
                abs_stack.clear();
                pc += 1;
            }
            // Binary arithmetic: iadd(0x60)..drem(0x73), ishl(0x78)..lxor(0x83)
            0x60..=0x73 | 0x78..=0x83 => {
                abs_stack.pop();
                if let Some(last) = abs_stack.last_mut() {
                    *last = None;
                }
                pc += 1;
            }
            // Unary: ineg..dneg
            0x74..=0x77 => {
                if let Some(last) = abs_stack.last_mut() {
                    *last = None;
                }
                pc += 1;
            }
            // Conversion: i2l..i2s
            0x85..=0x93 => {
                if let Some(last) = abs_stack.last_mut() {
                    *last = None;
                }
                pc += 1;
            }
            // Comparison: lcmp..dcmpg (pop 2, push 1)
            0x94..=0x98 => {
                abs_stack.pop();
                if let Some(last) = abs_stack.last_mut() {
                    *last = None;
                }
                pc += 1;
            }
            // Conditional branches.
            //
            // EC-SCALAR-SOUNDNESS (bc math-ec JIT miscompile fix): this is a
            // single LINEAR pass with no per-block reset/merge, so once
            // control flow can branch, neither `abs_stack` nor `local_prov`
            // reliably matches the real verification-time state at the branch
            // target. The previous code cleared only the STACK provenance and
            // kept `local_prov`, so a stale `local_prov[k] == Some(new_pc)`
            // could mis-attribute a later `getfield`/`putfield` (whose local
            // `k` was reused for a different value on the branch-reached path)
            // to a scalar-replaced object — rewriting it into a frame-slot
            // access against memory that does not hold that object, or
            // wrongly skipping its `<init>`. Combined with the (now stricter)
            // `analyze_escapes`, which marks any object whose provenance
            // crosses a CFG edge as escaping, scalar-replaced objects are
            // guaranteed to live entirely within one straight-line region.
            // Mirror that here: a control transfer is a hard barrier — drop
            // ALL provenance (stack + locals) so no field op past a branch is
            // ever bound to a scalar object via stale state.
            0x99..=0xA6 => {
                // Everything live crosses the edge; `analyze_escapes` escaped
                // it all at this branch, so anything tracked here is poisoned.
                poison_stack!();
                poison_locals!();
                abs_stack.clear();
                for prov in local_prov.iter_mut() {
                    *prov = None;
                }
                pc += 3;
            }
            // goto — control transfer; same hard barrier as above, EXCEPT for
            // the straight-line case EA-2 admits.
            0xA7 => {
                // The identical test `analyze_escapes` makes, from the identical
                // map. If these two ever disagree about a `goto`, one walk keeps
                // a provenance the other dropped, and a field op past the jump
                // binds to a scalar object the other pass did not admit.
                if straight_gotos.get(pc).copied().unwrap_or(false) {
                    if let Some(t) = bytecode_analysis::offset_branch_target(code, pc) {
                        pc = t;
                        continue;
                    }
                }
                poison_stack!();
                poison_locals!();
                abs_stack.clear();
                for prov in local_prov.iter_mut() {
                    *prov = None;
                }
                pc += 3;
            }
            // ireturn, lreturn, freturn, dreturn, areturn.
            //
            // The path ENDS: nothing here flows to the next pc, which is
            // reachable only through a branch target or as a handler. The
            // LOCALS are dropped WITHOUT poisoning — nothing live was lost — so
            // that a branch target right after the return does not poison an
            // object whose life ended here. `analyze_escapes` drops the same
            // locals at the same instruction. (They used to be kept, which also
            // left `local_prov_at` naming dead objects at every pc after the
            // return.) The operand STACK — including an `areturn`'s value — is
            // escaped by `analyze_escapes` at every return, so anything tracked
            // there is a disagreement and is poisoned.
            0xAC..=0xB1 => {
                poison_stack!();
                abs_stack.clear();
                for prov in local_prov.iter_mut() {
                    *prov = None;
                }
                pc += 1;
            }
            // getstatic
            0xB2 => {
                abs_stack.push(None);
                pc += 3;
            }
            // putstatic. A tracked VALUE is published to a static; like the
            // putfield/aastore arms, one reaching here is poisoned rather than
            // left to `analyze_escapes` alone (round 12 wave 2, x64opt).
            0xB3 => {
                if let Some(p) = abs_stack.pop().flatten() {
                    poisoned.insert(p);
                }
                pc += 3;
            }
            // getfield
            0xB4 => {
                let obj = abs_stack.pop().flatten();
                if let Some(new_pc) = obj {
                    if objects.contains_key(&new_pc) {
                        field_ops.insert(pc, new_pc);
                    }
                }
                abs_stack.push(None);
                pc += 3;
            }
            // putfield. A tracked VALUE is published into a field;
            // `analyze_escapes` escapes it, so one reaching here is poisoned.
            0xB5 => {
                if let Some(p) = abs_stack.pop().flatten() {
                    poisoned.insert(p);
                }
                let obj = abs_stack.pop().flatten();
                if let Some(new_pc) = obj {
                    if objects.contains_key(&new_pc) {
                        field_ops.insert(pc, new_pc);
                    }
                }
                pc += 3;
            }
            // invokespecial
            0xB7 => {
                let info_ptr = invoke_info
                    .iter()
                    .find(|&&(ipc, _)| ipc == pc)
                    .map(|&(_, ptr)| ptr);
                if let Some(ptr) = info_ptr {
                    // SAFETY: ptr comes from invoke_info, which holds pointers to JitInvokeInfo
                    // structs that are kept alive by the caller for the duration of compilation.
                    let info = unsafe { &*ptr };
                    let n = info.num_jit_args;
                    if info.method_name == "<init>" && info.descriptor == "()V" {
                        // Zero-arg init: pop only `this`. The skip is recorded
                        // only for a constructor the resolver PROVED empty
                        // (STR819-2): `Foo() { this.x = 5; }` is also `()V`,
                        // and skipping it would drop that write. An unproven
                        // one keeps its receiver a real allocation.
                        let receiver = abs_stack.pop().flatten();
                        if let Some(new_pc) = receiver {
                            if objects.contains_key(&new_pc) {
                                if proven_empty_inits.is_some_and(|s| s.contains(&pc)) {
                                    init_skip_owner.insert(pc, new_pc);
                                } else {
                                    poisoned.insert(new_pc);
                                }
                            }
                        }
                    } else {
                        // Pop all args (including this). Every one of them is
                        // escaped by `analyze_escapes`' arg-bearing arm.
                        for _ in 0..n {
                            if let Some(p) = abs_stack.pop().flatten() {
                                poisoned.insert(p);
                            }
                        }
                        if info.return_type != b'V' {
                            abs_stack.push(None);
                        }
                    }
                } else {
                    poison_stack!();
                    abs_stack.clear();
                    abs_stack.push(None);
                }
                pc += 3;
            }
            // invokevirtual, invokestatic
            0xB6 | 0xB8 => {
                poison_stack!();
                abs_stack.clear();
                abs_stack.push(None);
                pc += 3;
            }
            // invokeinterface
            0xB9 => {
                poison_stack!();
                abs_stack.clear();
                abs_stack.push(None);
                pc += 5;
            }
            // invokedynamic (r9-ea) — every operand is an argument, so the stack
            // model is dropped like any call's, but no local is touched.
            // `analyze_escapes` keeps local provenance across it (its catch-all
            // escapes only the stack), and this walk's catch-all used to drop
            // the locals — the string-concatenation `invokedynamic` between two
            // field accesses of a local scalar object is what that broke.
            0xBA => {
                poison_stack!();
                abs_stack.clear();
                abs_stack.push(None);
                pc += bytecode_analysis::step(code, pc);
            }
            // multianewarray — pops its dimensions, pushes the array, touches no
            // local. Same reasoning as `invokedynamic` above.
            0xC5 => {
                poison_stack!();
                abs_stack.clear();
                abs_stack.push(None);
                pc += bytecode_analysis::step(code, pc);
            }
            // checkcast: keeps object on stack with same provenance
            0xC0 => {
                pc += 3;
            }
            // instanceof: pop ref, push int
            0xC1 => {
                abs_stack.pop();
                abs_stack.push(None);
                pc += 3;
            }
            // ifnull, ifnonnull — control transfer; hard barrier (see the
            // EC-SCALAR-SOUNDNESS note on the conditional-branch arm above):
            // drop ALL provenance (stack + locals) so no field op past the
            // branch is bound to a scalar object via stale state.
            0xC6 | 0xC7 => {
                poison_stack!();
                poison_locals!();
                abs_stack.clear();
                for prov in local_prov.iter_mut() {
                    *prov = None;
                }
                pc += 3;
            }
            // athrow — control transfer (to handler or caller); same barrier.
            0xBF => {
                poison_stack!();
                poison_locals!();
                abs_stack.clear();
                for prov in local_prov.iter_mut() {
                    *prov = None;
                }
                pc += 1;
            }
            // arraylength
            0xBE => {
                abs_stack.pop();
                abs_stack.push(None);
                pc += 1;
            }
            // iinc
            0x84 => {
                pc += 3;
            }
            // newarray, anewarray
            0xBC => {
                abs_stack.pop();
                abs_stack.push(None);
                pc += 2;
            }
            0xBD => {
                abs_stack.pop();
                abs_stack.push(None);
                pc += 3;
            }
            // Phase C: monitorenter / monitorexit — pop the receiver and, when it
            // is a scalar-replaced object, track the recursion depth so a deopt can
            // record + relock the elided monitor. Handled explicitly (not via the
            // catch-all below) so the scalar object's LOCAL provenance survives the
            // op — the catch-all would clear it, hiding the object from later deopt
            // snapshots.
            0xC2 => {
                if let Some(np) = abs_stack.pop().flatten() {
                    *mon_depth.entry(np).or_insert(0) += 1;
                    monitor_scalar_op_owner.push((pc, np));
                }
                pc += 1;
            }
            0xC3 => {
                if let Some(np) = abs_stack.pop().flatten() {
                    if let Some(d) = mon_depth.get_mut(&np) {
                        *d = d.saturating_sub(1);                    }
                    monitor_scalar_op_owner.push((pc, np));
                }
                pc += 1;
            }
            // For anything else, conservatively clear all provenance — and
            // poison whatever was still live, so the object is allocated for
            // real instead of losing its field mappings past this point.
            _ => {
                poison_stack!();
                poison_locals!();
                abs_stack.clear();
                for prov in local_prov.iter_mut() {
                    *prov = None;
                }
                pc += bytecode_analysis::step(code, pc);            }        }    }

    // STR920-1: the last instruction's arm, then report.
    attribute_drift!(last_pc, "opcode");
    if drift_census {
        for d in &pair_drift {
            eprintln!(
                "[DBG_SCALAR_DEOPT] ea-pair-drift code_len={code_len} new_pc={} poisoned_at={} \
                 op=0x{:02x} reason={}",
                d.new_pc, d.pc, d.opcode, d.reason
            );
        }
    }

    // Only keep objects that have at least one field op or init skip, and
    // never one whose provenance was lost while it was live (`poisoned`).
    let used_objects: std::collections::HashSet<usize> = field_ops
        .values()
        .copied()
        .chain(init_skip_owner.values().copied())
        .filter(|new_pc| objects.contains_key(new_pc) && !poisoned.contains(new_pc))
        .collect();

    // Rebuild objects map with only used ones, reassign offsets
    let mut final_objects: FxHashMap<usize, ScalarReplacedObject> = FxHashMap::default();
    let mut final_total = 0usize;
    for &new_pc in &sorted_pcs {
        if used_objects.contains(&new_pc) {
            if let Some(obj) = objects.get(&new_pc) {
                let field_base_offset = ((scalar_base + final_total) as i32 + 1) * 8; // Cast: x86-64 immediate encoding
                final_objects.insert(
                    new_pc,
                    ScalarReplacedObject {
                        num_fields: obj.num_fields,
                        field_base_offset,
                        class_id: obj.class_id,
                    },
                );
                final_total += obj.num_fields * (SLOT_SIZE / 8);
            }
        }
    }

    // Remap field_ops to only reference final objects
    let final_field_ops: FxHashMap<usize, usize> = field_ops
        .into_iter()
        .filter(|(_, new_pc)| final_objects.contains_key(new_pc))
        .collect();
    let final_init_skips: std::collections::HashSet<usize> = init_skip_owner
        .iter()
        .filter(|(_, new_pc)| final_objects.contains_key(*new_pc))
        .map(|(&init_pc, _)| init_pc)
        .collect();
    // STR819-2: the walk's own invariant -- no constructor is skipped unless
    // it was proven empty.
    debug_assert!(
        final_init_skips
            .iter()
            .all(|pc| proven_empty_inits.is_some_and(|s| s.contains(pc))),
        "init skip recorded for a constructor not proven empty"
    );
    // Only monitor ops on a SURVIVING scalar object may be elided.
    let final_monitor_scalar_ops: std::collections::HashSet<usize> = monitor_scalar_op_owner
        .iter()
        .filter(|(_, np)| final_objects.contains_key(np))
        .map(|&(mpc, _)| mpc)
        .collect();

    // Phase B: drop any provenance entry referencing an object that did not
    // survive the used-objects pruning, and any PC whose locals all dropped out.
    let final_local_prov_at: FxHashMap<usize, Vec<(usize, usize)>> = local_prov_at
        .into_iter()
        .filter_map(|(pc, live)| {
            let kept: Vec<(usize, usize)> = live
                .into_iter()
                .filter(|(_, np)| final_objects.contains_key(np))
                .collect();
            if kept.is_empty() {
                None
            } else {
                Some((pc, kept))
            }
        })
        .collect();

    // Phase C: keep only monitor snapshots referencing surviving objects.
    let final_monitor_at: FxHashMap<usize, Vec<(usize, u32)>> = monitor_at
        .into_iter()
        .filter_map(|(pc, held)| {
            let kept: Vec<(usize, u32)> = held
                .into_iter()
                .filter(|(np, _)| final_objects.contains_key(np))
                .collect();
            if kept.is_empty() {
                None
            } else {
                Some((pc, kept))
            }
        })
        .collect();

    ScalarReplacementPlan {
        objects: final_objects,
        field_ops: final_field_ops,
        init_skips: final_init_skips,
        total_slots: final_total,
        local_prov_at: final_local_prov_at,
        monitor_at: final_monitor_at,
        monitor_scalar_ops: final_monitor_scalar_ops,
        pair_drift,
        poisoned_admitted: poisoned.len(),
    }
}

/// Loop headers whose LICM / speculative **pre-header would be bypassed** by at
/// least one control-flow edge that enters the loop from outside it.
///
/// Every pre-header this backend emits — aaload / integer / FP LICM hoists,
/// speculative-BCE range guards, SIMD batch preheaders — is emitted inline *at*
/// the loop-header PC, and `pc_to_native[header]` is then set to the position
/// **after** it so the in-loop back edge does not re-run it (see the
/// `osr_entry_native` field doc). That contract silently assumes the only way
/// into the loop is the linear fall-through, which runs the pre-header first.
///
/// The assumption breaks for a loop whose header (or any body PC) is also the
/// target of a *forward* branch from before the loop:
///
/// ```text
///   0: iload_1; ifeq 10
///   4: bipush 25; istore_2
///   7: goto 13          <-- enters the loop header directly, skipping the
///  10: bipush 30; istore_2     pre-header emitted just before it
///  13: iload_2; iload_0; iconst_5; imul; if_icmpge 27   <-- loop header
///  20: iload_2; iconst_2; imul; istore_2
///  24: goto 13
/// ```
///
/// That is exactly `org.xml.sax.helpers.AttributesImpl.ensureCapacity` (and
/// `LicmEntryProbe.shapeB`, the regression witness). Taking the `goto 13` edge
/// lands after the pre-header, so the arith-LICM slot caching `n * 5` is never
/// written and the loop compares `max` against whatever the previous call left
/// at that frame offset: a small leftover exits the loop immediately (the
/// probe's `max` stays 25 instead of growing to 400), a large positive leftover
/// doubles `max` up to ~1.6e9 and the following `new String[max]` dies with
/// `OutOfMemoryError: Java heap space (anewarray component 6 length
/// 1677721600)` — the HIB-LONGTAIL.2 signature.
///
/// This predicate refuses to speculate on such headers at all: the caller drops
/// every transform whose header is listed here, which can only ever *remove*
/// an optimisation. Loops with a single fall-through entry — the overwhelming
/// majority, including every javac `for`/`while`/`do` whose header is not also
/// a branch target from outside — keep hoisting unchanged. The pre-header
/// transforms emitted at the header itself (the LICM hoists, the BCE guards,
/// the batch pre-headers and, since round 11 wave 6, the native unroller) use
/// [`find_preheader_bypassing_loop_headers`] instead, leaving this one to the
/// bytecode loop rewriter (`loop_rewrite.rs`), which moves bytes and has no
/// landing: since round 11 wave 4 a forward branch ONTO the header (the
/// `goto 13` above) lands before the pre-header and runs it, so that edge is
/// not a bypass for them.
///
/// Fail-closed on opaque control flow: a method containing `jsr`/`ret` gets
/// every loop header marked, because `ret` returns to a value-carried address
/// this walk cannot resolve.
///
/// Exception-handler edges (`exception_ranges`, as
/// `(start_pc, end_pc, handler_pc)`) are not bytecode branches, so the edge
/// decoding below cannot see them: the real edge is "any throwing instruction
/// in `[start_pc, end_pc)` -> `handler_pc`", materialised by the runtime's
/// exception dispatch. A handler landing inside a loop body is an entry into
/// that loop and bypasses its pre-header exactly as a `goto` into the header
/// does — but only when it can be reached from OUTSIDE the loop, which is why
/// the predicate also requires the protected range to escape `[header,
/// loop_end)`. A range lying wholly inside the loop is sound and keeps its
/// hoist (the throw can only have happened after the header was entered, so
/// the pre-header already ran) — that is the shape javac emits for the common
/// `while (…) { try { … } catch { … } }`.
///
/// **This handler clause is live.** It was written pre-emptively, when a
/// method with a non-empty exception table was not admitted to this backend
/// at all; that gate has since been relaxed, and methods with `try`/`catch`
/// are compiled here routinely (the optimizing tier refuses them, so this is
/// their only compiled tier). Measured round 9 wave 8 on the w7b binary: a
/// `try { for (i < a.length) s += a[i]; } catch (RuntimeException e)` kernel
/// compiles single-pass at both tiers, with its `arraylength` hoist and its
/// vectorised reduction, and runs at the handler-free kernel's speed. The
/// driver passes the real table (`exception_ranges`, output coordinates).
/// The witness for the shape this clause exists for -- a handler inside the
/// loop whose protected range lies entirely before it, the normal path
/// falling *through* into the header so no branch edge exists for the rule
/// above to catch -- was an ASM generator under `docs/known-issues/repros/`,
/// since removed from the tree. Round 9 wave 8 rebuilt it with the JDK
/// class-file API (`C:\craton\jitr9-probes\review8a\src\GenHandlerEdge.java`,
/// `shapeA`, plus a `goto`-into-the-body `shapeB`, a post-tested `shapeC` and
/// a two-entry rotated `shapeD`); on the w7b binary all four match HotSpot
/// (`shapeA` is not compiled today: its OSR is denied for the `athrow`).
pub(super) fn find_bypassable_loop_headers(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
    exception_ranges: &[(usize, usize, usize)],
) -> FxHashSet<usize> {
    bypassable_loop_headers(code, code_len, loops, exception_ranges, false)
}

/// [`find_bypassable_loop_headers`] for a pre-header emitted AT the header by
/// the single-pass walk, which a forward branch onto the header now runs.
///
/// The walk records `preheader_native[header]`, the native position BEFORE
/// the header's pre-header (the position `osr_entry_native[header]` already
/// names), and `Compiler::branch_landing_native` resolves every `forward_patches`
/// entry EMITTED BEFORE that position to it: a jump from code laid out before
/// the loop is a loop entry and runs the pre-header like the fall-through
/// does. So an offset-branch edge `src < header -> header` is no longer a
/// bypass, and this predicate does not count it. The same holds for every
/// switch edge onto the header: `lookupswitch` edges and compare-chain
/// `tableswitch` edges are `forward_patches` branches (round 11 wave 6), and
/// since wave 7 a jump-table `tableswitch` slot resolves through the same
/// `branch_landing_native`, with the slot's own offset as the branch position
/// (`r11w6-x64-jump-table-landing-patch`). The table is emitted inline right
/// after the dispatch, so a switch laid out before the loop has every slot
/// below `preheader_native[header]`. So `switch (op) { case 0: while (..) .. }`
/// keeps its pre-header whatever the switch's form.
///
/// Everything else still counts: an edge into the body past the header, a
/// second back edge (per pair), and every exception-handler entry. Callers are the pre-header
/// transforms the walk emits at the header itself: the three LICM hoists, the
/// speculative-BCE guards and (since round 11 wave 5) the batch pre-headers
/// (matrix dot, SIMD sum and element-wise, bulk byte loops), which sit in
/// `[preheader_native, pc_to_native)` or, strip-mined, right after
/// `pc_to_native`, and so run on a forward entry too. `single_pass_only.rs`
/// asks the IR-tier veto with this predicate for the same reason. Since
/// round 11 wave 6 the native unroller takes it as well: it copies
/// `[pc_to_native[header], back_edge)`, so the redirected entry runs the
/// uncopied pre-header once and falls into copy 0
/// (`r11-x64loop-header-entered-from-outside-drops-every-preheader`).
pub(super) fn find_preheader_bypassing_loop_headers(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
    exception_ranges: &[(usize, usize, usize)],
) -> FxHashSet<usize> {
    bypassable_loop_headers(code, code_len, loops, exception_ranges, true)
}

/// [`find_preheader_bypassing_loop_headers`] for the driver's two loop lists
/// (every `(header, back_edge)` pair, and `loops_by_header`'s merged pairs)
/// from ONE decode of the method's branch edges instead of one per list
/// (round 11 wave 7, proposal W7-3). Same answers as two separate calls.
pub(super) fn find_preheader_bypassing_loop_headers_per_pair_and_merged(
    code: &[u8],
    code_len: usize,
    per_pair: &[(usize, usize)],
    merged: &[(usize, usize)],
    exception_ranges: &[(usize, usize, usize)],
) -> (FxHashSet<usize>, FxHashSet<usize>) {
    if per_pair.is_empty() && merged.is_empty() {
        return (FxHashSet::default(), FxHashSet::default());
    }
    let edges = loop_entry_branch_edges(code, code_len);
    (
        bypassable_from_edges(code, per_pair, edges.as_deref(), exception_ranges, true),
        bypassable_from_edges(code, merged, edges.as_deref(), exception_ranges, true),
    )
}

fn bypassable_loop_headers(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
    exception_ranges: &[(usize, usize, usize)],
    forward_header_entry_lands_on_preheader: bool,
) -> FxHashSet<usize> {
    if loops.is_empty() {
        return FxHashSet::default();
    }
    let edges = loop_entry_branch_edges(code, code_len);
    bypassable_from_edges(
        code,
        loops,
        edges.as_deref(),
        exception_ranges,
        forward_header_entry_lands_on_preheader,
    )
}

/// `(src_pc, target_pc)` for every explicit branch edge of the method, or
/// `None` when the method has control flow the scan cannot follow (see the
/// comment inside), which makes every loop bypassable.
fn loop_entry_branch_edges(code: &[u8], code_len: usize) -> Option<Vec<(usize, usize)>> {
    // (src_pc, target_pc) for every explicit branch edge, from the shared
    // decoder. Every one of them (offset branches, both switch forms, jump
    // table slots included) is resolved by `Compiler::branch_landing_native`,
    // which redirects an edge laid out before a pre-header onto it. Control
    // flow this scan cannot follow is OPAQUE, which marks every loop
    // bypassable: `ret`, `jsr`/`jsr_w` (their edge is still recorded), and any
    // switch `bytecode_analysis::switch_table` refuses.
    let mut edges: Vec<(usize, usize)> = Vec::new();
    let mut opaque = false;
    let mut pc = 0usize;
    while pc < code_len {
        let op = code[pc];
        if bytecode_analysis::is_subroutine_op(op) {
            opaque = true;
        }
        if bytecode_analysis::is_offset_branch(op) {
            if let Some(t) = bytecode_analysis::offset_branch_target(&code[..code_len], pc)
                .filter(|&t| t < code_len)
            {
                edges.push((pc, t));
            }
        } else if matches!(op, 0xaa | 0xab) {
            match bytecode_analysis::switch_table(code, code_len, pc) {
                Some(table) => edges.extend(table.targets().map(|t| (pc, t))),
                None => {
                    opaque = true;
                    break;
                }
            }
        }
        pc += bytecode_analysis::step(code, pc);
    }
    if opaque {
        None
    } else {
        Some(edges)
    }
}

/// The loop headers of `loops` some entry reaches without running the
/// pre-header, given the method's branch edges (`None`: opaque, every header).
fn bypassable_from_edges(
    code: &[u8],
    loops: &[(usize, usize)],
    edges: Option<&[(usize, usize)]>,
    exception_ranges: &[(usize, usize, usize)],
    forward_header_entry_lands_on_preheader: bool,
) -> FxHashSet<usize> {
    let mut bypassable: FxHashSet<usize> = FxHashSet::default();
    let Some(edges) = edges else {
        for &(header, _) in loops {
            bypassable.insert(header);
        }
        return bypassable;
    };

    for &(header, back_edge) in loops {
        let loop_end = back_edge + bytecode_analysis::step(code, back_edge);
        for &(src, target) in edges {
            if forward_header_entry_lands_on_preheader && src < header && target == header {
                continue;
            }
            let target_inside = target >= header && target < loop_end;
            let src_inside = src >= header && src < loop_end;
            if target_inside && !src_inside {
                bypassable.insert(header);
                break;
            }
        }
        // Exception-handler edges are NOT bytecode branches, so the decoding
        // above cannot see them: the real edge is "any throwing instruction in
        // `[start_pc, end_pc)` -> `handler_pc`", materialised by the runtime's
        // exception dispatch. A handler that lands inside this loop's body is
        // therefore an entry into the loop, and it bypasses the pre-header for
        // exactly the same reason a `goto` into the header does.
        //
        // The predicate is the same one used for branch edges — an entry whose
        // SOURCE is outside the loop — with the throwing site standing in for
        // the branch source. A protected range lying wholly inside the loop is
        // sound and keeps its hoist: the throw can only have happened after
        // the header was entered, so the pre-header had already run. That is
        // precisely the shape javac emits for the common
        // `while (…) { try { … } catch { … } }`, so this costs nothing there.
        // A range reaching before the header (or past the loop) can deliver
        // control into the body from code the pre-header never covered.
        for &(start_pc, end_pc, handler_pc) in exception_ranges {
            let handler_inside = handler_pc >= header && handler_pc < loop_end;
            let range_escapes = start_pc < header || end_pc > loop_end;
            if handler_inside && range_escapes {
                bypassable.insert(header);
                break;
            }
        }
    }
    bypassable
}

/// The single-pass `tableswitch` lowering's form (`op_control.rs`, the `0xaa`
/// arm, which decides with this function): a compare chain while the LIVE
/// cases (slots whose target is not the default) fit
/// `crate::ir::SWITCH_CHAIN_MAX_CASES`, else a jump table with one slot per
/// case, holes (slots naming the default) included.
///
/// The form no longer matters to the pre-header landing predicate: since
/// round 11 wave 7 a jump-table slot lands through
/// `Compiler::branch_landing_native` exactly like a compare-chain `JE`
/// (`r11w6-x64-jump-table-landing-patch`; the wave-6
/// `switch_edge_is_forward_patched` distinction is gone).
pub(super) fn tableswitch_emits_jump_table(table: &bytecode_analysis::SwitchTable) -> bool {
    table
        .cases
        .iter()
        .filter(|&&(_, target)| target != table.default)
        .count()
        > crate::ir::SWITCH_CHAIN_MAX_CASES
}

/// Rotated (`goto cond`) loops whose pre-header can be emitted at the rotation
/// entry instead of at the header: `header -> entry goto pc`.
///
/// ecj, kotlinc and scalac (and javac for some constructs) emit
///
/// ```text
///         goto COND          <- the only entry into the loop
/// BODY:   ...                <- header (back-edge target)
/// COND:   ...; if<cond> BODY
/// ```
///
/// [`find_bypassable_loop_headers`] marks every such header bypassable,
/// because the entry `goto` lands inside the loop without passing the
/// header's pre-header. But when that `goto` is the loop's ONLY entry, every
/// execution of the loop passes the `goto`, so a pre-header emitted
/// immediately BEFORE the `goto` runs exactly once per loop entry — the
/// placement the header-anchored pre-header has for a fall-through loop. The
/// machine state at the `goto` is the state at the first test: nothing
/// executes between them.
///
/// A header qualifies only when, for EVERY loop in `loops` with that header:
///
/// * the instruction immediately before the header is a `goto` (`0xa7`, three
///   bytes — so control cannot fall into the header) whose target lies in
///   `[header, loop_end)`;
/// * no other explicit edge enters `[header, loop_end)` from outside it;
/// * no explicit edge targets the `goto` itself, and no exception handler
///   starts at it: the relocated pre-header sits before `pc_to_native[goto]`,
///   so a branch to the `goto` would skip it;
/// * no exception handler lands in the loop from a protected range that
///   escapes it (the rule [`find_bypassable_loop_headers`] applies).
///
/// Opaque control flow (`jsr`/`ret`, an undecodable branch or switch) answers
/// the empty map. Consumers: the driver's pre-header filters (behind
/// `CRATONVM_JIT_ROTATED_PREHEADER`) and the walk, which emits the header's
/// relocatable transforms when it reaches the `goto`
/// (`rotated-loops-lose-every-preheader-transform-20260918.md`).
pub(super) fn find_rotation_preheader_entries(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
    exception_ranges: &[(usize, usize, usize)],
) -> FxHashMap<usize, usize> {
    let code_len = code_len.min(code.len());
    let mut out: FxHashMap<usize, usize> = FxHashMap::default();
    if loops.is_empty() || code_len == 0 {
        return out;
    }

    // Every explicit edge, from the shared decoder. Fail closed: anything this
    // scan cannot follow answers "no rotated header at all".
    let mut edges: Vec<(usize, usize)> = Vec::new();
    let mut pc = 0usize;
    while pc < code_len {
        let op = code[pc];
        if bytecode_analysis::is_subroutine_op(op) || bytecode_analysis::is_wide_ret(code, pc) {
            return FxHashMap::default();
        }
        if bytecode_analysis::is_offset_branch(op) {
            match bytecode_analysis::offset_branch_target(&code[..code_len], pc)
                .filter(|&t| t < code_len)
            {
                Some(t) => edges.push((pc, t)),
                None => return FxHashMap::default(),
            }
        } else if matches!(op, 0xaa | 0xab) {
            match bytecode_analysis::switch_table(code, code_len, pc) {
                Some(table) => edges.extend(table.targets().map(|t| (pc, t))),
                None => return FxHashMap::default(),
            }
        }
        let len = bytecode_analysis::step(code, pc);
        if len == 0 {
            return FxHashMap::default();
        }
        pc += len;
    }
    let starts = bytecode_analysis::instruction_starts(code, code_len);

    let qualifies = |header: usize, back_edge: usize| -> Option<usize> {
        let goto_pc = header.checked_sub(3)?;
        if !starts.get(goto_pc).copied().unwrap_or(false) || code[goto_pc] != 0xa7 {
            return None;
        }
        let loop_end = back_edge.checked_add(bytecode_analysis::step(code, back_edge))?;
        let entry = bytecode_analysis::offset_branch_target(&code[..code_len], goto_pc)?;
        if !(header..loop_end).contains(&entry) {
            return None;
        }
        let inside = |p: usize| (header..loop_end).contains(&p);
        for &(src, t) in &edges {
            if t == goto_pc {
                return None;
            }
            if inside(t) && !inside(src) && src != goto_pc {
                return None;
            }
        }
        for &(start_pc, end_pc, handler_pc) in exception_ranges {
            if handler_pc == goto_pc {
                return None;
            }
            if inside(handler_pc) && (start_pc < header || end_pc > loop_end) {
                return None;
            }
        }
        Some(goto_pc)
    };

    let mut refused: FxHashSet<usize> = FxHashSet::default();
    for &(header, back_edge) in loops {
        if refused.contains(&header) {
            continue;
        }
        match qualifies(header, back_edge) {
            Some(goto_pc) => {
                out.insert(header, goto_pc);
            }
            None => {
                // One loop at this header that does not qualify disqualifies
                // the header: its transforms are shared by every loop there.
                refused.insert(header);
                out.remove(&header);
            }
        }
    }
    out
}

/// Is the rotated-loop pre-header relocation armed? It moves speculative code
/// (the BCE entry guards, the `aaload` / arithmetic / `arraylength` hoists
/// and, since round 9 wave 4, the batch pre-headers via
/// [`detect_batch_loop`]) to a rotated loop's entry `goto`.
///
/// DEFAULT-ON since round 9 wave 5's integration; `CRATONVM_JIT_ROTATED_PREHEADER=0`
/// is the kill switch. Measured on the wave-4 binary with the ecj-compiled
/// LoopProbe (`NOTES-w5-spstack5.md`, request C), flag off -> on, ms: sum
/// 970 -> 270, sumlen 684 -> 118, elem 1130 -> 262, sieve 1794 -> 893,
/// matrix 481 -> 228 -- the ecj column now tracks the javac one (which the
/// flag leaves unchanged) -- and every checksum matched HotSpot in both arms.
/// Read per compile.
pub(super) fn rotated_preheader_enabled() -> bool {
    true
}

/// A rotated loop rewritten into the top-tested shape the batch pre-header
/// detectors match (round 9 wave 4, lane `x64core4`).
///
/// ```text
///   rotated (real code)                 unrotated (`code` below)
///   G:  goto C                          0:         COND
///   H:  BODY                            len(COND): if<!cc> EXIT
///   C:  COND                            body:      BODY'      (branches re-based)
///   B:  if<cc> H                        back_edge: goto 0
///       ...                             EXIT:      return + 2 padding bytes
/// ```
///
/// For every entry through the `goto` the two run the same instructions on
/// the same locals, so a detector's answer about the unrotated copy (which
/// locals are the array, the induction variable, the bound, the accumulator)
/// is an answer about the rotated loop entered at `G`. Only the pcs differ:
/// [`BatchLoopAnchor::anchor_at`] puts the real ones back.
pub(super) struct UnrotatedLoop {
    /// The synthetic method body (COND, the negated test, BODY', the back
    /// edge, a `return` and two padding bytes).
    pub(super) code: Vec<u8>,
    /// Synthetic header pc (COND's first instruction): always 0.
    pub(super) header: usize,
    /// Synthetic back edge pc (the `goto 0`).
    pub(super) back_edge: usize,
}

/// Unrotate the loop `(header, back_edge)` (a [`detect_loops`] pair) when it
/// has exactly the rotated shape: a 3-byte `goto` immediately before `header`
/// targeting COND in `(header, back_edge]`, COND straight-line code, and the
/// back edge a 3-byte conditional branch to `header`. `None` otherwise — in
/// particular for a BODY holding a switch (its padding depends on the pc),
/// `jsr`/`ret`, `goto_w`, or a branch that leaves the loop anywhere but the
/// fall-through exit.
///
/// Branches inside BODY are re-based: to BODY -> BODY', to COND -> the
/// synthetic header, to the loop exit -> the synthetic exit.
pub(super) fn unrotate_loop(
    code: &[u8],
    code_len: usize,
    header: usize,
    back_edge: usize,
) -> Option<UnrotatedLoop> {
    let code_len = code_len.min(code.len());
    let code = &code[..code_len];
    let goto_pc = header.checked_sub(3)?;
    if code.get(goto_pc).copied() != Some(0xa7) {
        return None;
    }
    let cond = bytecode_analysis::offset_branch_target(code, goto_pc)?;
    let back_op = *code.get(back_edge)?;
    let negated = match back_op {
        // ifeq/ifne, iflt/ifge, ifgt/ifle, if_icmpeq/ne, if_icmplt/ge,
        // if_icmpgt/le, if_acmpeq/ne: each pair sits at (even, odd) offsets
        // from 0x99.
        0x99..=0xa6 if (back_op - 0x99) % 2 == 0 => back_op + 1,
        0x99..=0xa6 => back_op - 1,
        0xc6 => 0xc7, // ifnull -> ifnonnull
        0xc7 => 0xc6, // ifnonnull -> ifnull
        _ => return None,
    };
    if bytecode_analysis::offset_branch_target(code, back_edge) != Some(header) {
        return None;
    }
    let loop_end = back_edge.checked_add(3)?;
    if !(header < cond && cond <= back_edge) || loop_end > code_len {
        return None;
    }

    // COND: straight-line, ending exactly at the back edge.
    let mut pc = cond;
    while pc < back_edge {
        let insn = bytecode_analysis::decode_at(code, code_len, pc)?;
        let op = insn.op;
        if bytecode_analysis::is_offset_branch(op)
            || matches!(op, 0xa9 | 0xaa | 0xab | 0xac..=0xb1 | 0xbf)
        {
            return None;
        }
        pc = insn.next_pc();
    }
    if pc != back_edge {
        return None;
    }

    // Layout of the synthetic body.
    let cond_len = back_edge - cond;
    let body_base = cond_len + 3;
    let body_len = cond - header;
    let synth_back_edge = body_base + body_len;
    let exit = synth_back_edge + 3;
    let new_pos = |real: usize| -> Option<usize> {
        if (header..cond).contains(&real) {
            Some(body_base + (real - header))
        } else if real == cond {
            Some(0)
        } else if real == loop_end {
            Some(exit)
        } else {
            None
        }
    };
    let rel16 = |from: usize, to: usize| -> Option<[u8; 2]> {
        let delta = (to as isize).checked_sub(from as isize)?;
        Some(i16::try_from(delta).ok()?.to_be_bytes())
    };

    let mut out: Vec<u8> = Vec::with_capacity(exit + 3);
    out.extend_from_slice(&code[cond..back_edge]);
    out.push(negated);
    out.extend_from_slice(&rel16(cond_len, exit)?);
    out.extend_from_slice(&code[header..cond]);
    // Re-base BODY's branches in the copy.
    let mut pc = header;
    while pc < cond {
        let insn = bytecode_analysis::decode_at(code, code_len, pc)?;
        let op = insn.op;
        if bytecode_analysis::is_subroutine_op(op) || matches!(op, 0xaa | 0xab | 0xc8) {
            return None;
        }
        if bytecode_analysis::is_offset_branch(op) {
            // The branch forms left (`if*`, `goto`, `ifnull`, `ifnonnull`)
            // are all 3 bytes with a 16-bit offset.
            let target = bytecode_analysis::offset_branch_target(code, pc)?;
            let from = new_pos(pc)?;
            let to = new_pos(target)?;
            let bytes = rel16(from, to)?;
            let slot = out.get_mut(from + 1..from + 3)?;
            slot.copy_from_slice(&bytes);
        }
        pc = insn.next_pc();
    }
    if pc != cond {
        return None;
    }
    out.push(0xa7);
    out.extend_from_slice(&rel16(synth_back_edge, 0)?);
    out.extend_from_slice(&[0xb1, 0x00, 0x00]); // EXIT: return, then padding
    Some(UnrotatedLoop {
        code: out,
        header: 0,
        back_edge: synth_back_edge,
    })
}

/// Unrotate, in place, every rotated loop nested inside an [`UnrotatedLoop`]'s
/// body (round 9 wave 6, lane `rotated6`).
///
/// ecj nests rotated loops: after [`unrotate_loop`] rewrites the outer loop,
/// an inner `goto C'; H': BODY'; C': COND'; if<cc> H'` is still rotated, and
/// a detector written against javac's nest (the byte sieve, whose inner
/// marking loop must be top-tested) does not match. A rotated loop and its
/// top-tested spelling have the SAME length (`3 + |BODY| + |COND| + 3` either
/// way), so the rewrite keeps every pc outside the loop, and the loop's own
/// entry and exit pcs, where they were; see [`unrotate_loop_in_place`].
/// Repeats until no nested rotated loop is left (bounded: each rewrite turns
/// one conditional back edge into a `goto`, which is never rewritten again).
pub(super) fn unrotate_nested_loops(u: &mut UnrotatedLoop) {
    for _ in 0..32 {
        let len = u.code.len();
        let rewritten = detect_loops(&u.code, len)
            .into_iter()
            .filter(|&(h, b)| h > u.header + 3 && b + 3 <= u.back_edge)
            .find_map(|(h, b)| unrotate_loop_in_place(&u.code, h, b));
        match rewritten {
            Some(code) => u.code = code,
            None => return,
        }
    }
}

/// The in-place form of [`unrotate_loop`], for a loop inside a larger body:
///
/// ```text
///   G:          goto C               G:              COND
///   H:          BODY                 G+|COND|:       if<!cc> E
///   C:          COND                 H+|COND|:       BODY'   (re-based)
///   B:          if<cc> H             B:              goto G
///   E:                               E:
/// ```
///
/// Returns the whole rewritten code (same length), or `None` unless:
/// * `G = header - 3` is an instruction boundary holding a 3-byte `goto`
///   into `(header, back_edge]`, and `back_edge` is a negatable 3-byte
///   conditional branch to `header`;
/// * COND is straight-line (no branch, switch, return or `athrow`);
/// * no switch, subroutine op or `goto_w` appears anywhere;
/// * no branch outside `[G, E)` targets inside it other than at `G` (the
///   entry, which becomes COND's first instruction: same behaviour);
/// * BODY's branches target BODY, COND's first instruction, `G`, `E` or
///   outside `[G, E]` — re-based respectively to BODY', `G`, `G`, `E` and
///   the unchanged pc. A branch into the middle of COND or to the back edge
///   refuses.
///
/// Every entry reaches the loop through `G` (by fall-through or a branch)
/// and every exit leaves at `E` or through a BODY branch that keeps its
/// target, so both spellings run the same instructions on the same state.
pub(super) fn unrotate_loop_in_place(
    code: &[u8],
    header: usize,
    back_edge: usize,
) -> Option<Vec<u8>> {
    let code_len = code.len();
    let goto_pc = header.checked_sub(3)?;
    if code.get(goto_pc).copied() != Some(0xa7) {
        return None;
    }
    let loop_end = back_edge.checked_add(3)?;
    if loop_end > code_len {
        return None;
    }
    let cond = bytecode_analysis::offset_branch_target(code, goto_pc)?;
    if !(header < cond && cond <= back_edge) {
        return None;
    }
    if bytecode_analysis::offset_branch_target(code, back_edge) != Some(header) {
        return None;
    }
    let back_op = code[back_edge];
    let negated = match back_op {
        // Same pairing as `unrotate_loop`.
        0x99..=0xa6 if (back_op - 0x99) % 2 == 0 => back_op + 1,
        0x99..=0xa6 => back_op - 1,
        0xc6 => 0xc7,
        0xc7 => 0xc6,
        _ => return None,
    };

    // One decode of the whole body: instruction boundaries, the refusals,
    // and the branch rules outside the loop and in COND.
    let mut starts: Vec<usize> = Vec::new();
    let mut pc = 0usize;
    while pc < code_len {
        let insn = bytecode_analysis::decode_at(code, code_len, pc)?;
        let op = insn.op;
        if bytecode_analysis::is_subroutine_op(op) || matches!(op, 0xaa | 0xab | 0xc8) {
            return None;
        }
        let in_loop = (goto_pc..loop_end).contains(&pc);
        if !in_loop && bytecode_analysis::is_offset_branch(op) {
            let target = bytecode_analysis::offset_branch_target(code, pc)?;
            if goto_pc < target && target < loop_end {
                return None;
            }
        }
        if (cond..back_edge).contains(&pc)
            && (bytecode_analysis::is_offset_branch(op) || matches!(op, 0xac..=0xb1 | 0xbf))
        {
            return None;
        }
        starts.push(pc);
        pc = insn.next_pc();
    }
    if pc != code_len
        || starts.binary_search(&goto_pc).is_err()
        || starts.binary_search(&header).is_err()
        || starts.binary_search(&cond).is_err()
        || starts.binary_search(&back_edge).is_err()
    {
        return None;
    }

    let cond_len = back_edge - cond;
    let rel16 = |from: usize, to: usize| -> Option<[u8; 2]> {
        let delta = (to as isize).checked_sub(from as isize)?;
        Some(i16::try_from(delta).ok()?.to_be_bytes())
    };
    let mut span: Vec<u8> = Vec::with_capacity(loop_end - goto_pc);
    span.extend_from_slice(&code[cond..back_edge]);
    span.push(negated);
    span.extend_from_slice(&rel16(goto_pc + cond_len, loop_end)?);
    span.extend_from_slice(&code[header..cond]);
    span.push(0xa7);
    span.extend_from_slice(&rel16(back_edge, goto_pc)?);
    if span.len() != loop_end - goto_pc {
        return None;
    }
    let mut out = code.to_vec();
    out[goto_pc..loop_end].copy_from_slice(&span);

    // Re-base BODY's branches (BODY moved up by `cond_len`).
    for &pc in starts.iter().filter(|&&pc| (header..cond).contains(&pc)) {
        if !bytecode_analysis::is_offset_branch(code[pc]) {
            continue;
        }
        let target = bytecode_analysis::offset_branch_target(code, pc)?;
        let to = if (header..cond).contains(&target) {
            target + cond_len
        } else if target == cond || target == goto_pc {
            goto_pc
        } else if target == loop_end || target < goto_pc || target > loop_end {
            target
        } else {
            return None;
        };
        let from = pc + cond_len;
        let bytes = rel16(from, to)?;
        out.get_mut(from + 1..from + 3)?.copy_from_slice(&bytes);
    }
    Some(out)
}

/// The loop pcs a batch pre-header descriptor carries, so a descriptor
/// detected on an [`UnrotatedLoop`] can be re-anchored at the real loop.
pub(super) trait BatchLoopAnchor {
    /// Replace the descriptor's header (and back edge, where it keeps one)
    /// with the real loop's.
    fn anchor_at(&mut self, header: usize, back_edge: usize);
}

impl BatchLoopAnchor for BulkZeroByteFillLoop {
    fn anchor_at(&mut self, header: usize, _back_edge: usize) {
        self.header_pc = header;
    }
}

impl BatchLoopAnchor for BulkSetByteStrideLoop {
    fn anchor_at(&mut self, header: usize, _back_edge: usize) {
        self.header_pc = header;
    }
}

impl BatchLoopAnchor for ByteSieveLoop {
    fn anchor_at(&mut self, header: usize, _back_edge: usize) {
        self.header_pc = header;
    }
}

/// Run a batch pre-header detector (SIMD sum / element-wise, matrix dot, the
/// bulk byte loops) on one loop, honouring the pre-header placement contract.
///
/// `detect` gets `(code, code_len, header, back_edge)`.
///
/// * A header no outside edge can bypass: the detector runs on the method's
///   own code, as it always did.
/// * A bypassable header that is NOT a rotation entry (`rotation`, empty
///   unless `CRATONVM_JIT_ROTATED_PREHEADER` is armed): `None`, because the
///   bypassing edge would skip the pre-header.
/// * A rotation entry: the walk emits the batch pre-header at the entry
///   `goto` (every entry passes it) and NOT at the header (which the `goto`
///   reaches only through COND, and an OSR entry only mid-iteration, after
///   COND's test passed). The detector runs on the [`unrotate_loop`] copy
///   (whose nested rotated loops [`unrotate_nested_loops`] unrotates too) and
///   the descriptor is re-anchored at the real header. A rotation entry that
///   does not unrotate (a `goto` straight to the header: a top-tested loop
///   behind a jump) is detected on the real code.
///
/// Why emitting the pre-header at the `goto` is exact: every batch pre-header
/// is self-contained. It reads the loop's locals and either finishes the
/// loop's work and publishes the final induction variable (and accumulator),
/// or changes nothing Java-visible; then it falls through. At the `goto` the
/// fall-through is the jump to COND, which tests the published state exactly
/// as the top-tested header would.
pub(super) fn detect_batch_loop<T: BatchLoopAnchor>(
    code: &[u8],
    code_len: usize,
    header: usize,
    back_edge: usize,
    bypassable: &FxHashSet<usize>,
    rotation: &FxHashMap<usize, usize>,
    detect: impl FnOnce(&[u8], usize, usize, usize) -> Option<T>,
) -> Option<T> {
    if !bypassable.contains(&header) {
        return detect(code, code_len, header, back_edge);
    }
    if !rotation.contains_key(&header) {
        return None;
    }
    match unrotate_loop(code, code_len, header, back_edge) {
        Some(mut u) => {
            // ecj's nests are rotated at every level: a nest detector (the
            // byte sieve) wants javac's top-tested inner loops too. A copy
            // with no nested rotated loop is left unchanged, and none of the
            // other detectors matches a body holding an inner loop.
            unrotate_nested_loops(&mut u);
            let mut found = detect(&u.code[..], u.code.len(), u.header, u.back_edge)?;
            found.anchor_at(header, back_edge);
            Some(found)
        }
        None => detect(code, code_len, header, back_edge),
    }
}

/// Detect natural loops by finding backward branches in bytecode.
/// Returns a list of `(header_pc, back_edge_pc)` pairs.
pub(super) fn detect_loops(code: &[u8], code_len: usize) -> Vec<(usize, usize)> {
    // The shared back-edge set, kept to the 16-bit `goto` / `if*` forms this
    // pass has always modelled: a backward switch arm, `jsr` or `goto_w`
    // does not describe a loop the transforms below can rewrite.
    bytecode_analysis::back_edges(code, code_len)
        .into_iter()
        .filter(|&(_, src)| matches!(code[src], 0x99..=0xa7 | 0xc6 | 0xc7) && code[src] != 0xa8)
        .collect()
}

/// A canonical javac byte/boolean-array zero-fill loop.
///
/// The emitter turns the initially-entered range into one `REP STOSB`, updates
/// `iv` to `bound + 1`, and then falls through to the original header. Any
/// null, negative, empty, or out-of-bounds range branches around the bulk path
/// and executes the original scalar bytecode, preserving Java's exact
/// partial-write and exception behavior.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BulkZeroByteFillLoop {
    pub(super) header_pc: usize,
    pub(super) array_local: usize,
    pub(super) iv_local: usize,
    pub(super) bound_local: usize,
}

/// A canonical javac byte/boolean-array unit store with a positive,
/// loop-invariant variable stride.
///
/// This is the shape used by the inner marking loop in CratonBench Sieve:
/// `for (j = i + i; j <= limit; j += i) composite[j] = true`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BulkSetByteStrideLoop {
    pub(super) header_pc: usize,
    pub(super) array_local: usize,
    pub(super) iv_local: usize,
    pub(super) bound_local: usize,
    pub(super) step_local: usize,
}

/// The canonical nested byte/boolean-array Sieve of Eratosthenes loop emitted
/// by javac. A guarded preheader executes the remaining counted loop nest in
/// registers and then falls through to the original exit test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ByteSieveLoop {
    pub(super) header_pc: usize,
    pub(super) array_local: usize,
    pub(super) outer_iv_local: usize,
    pub(super) bound_local: usize,
    pub(super) count_local: usize,
    pub(super) inner_iv_local: usize,
}

// Bulk preheaders have no back-edge safepoint. Cap the covered range so an
// enormous application loop retains the scalar path's cooperative polls.
pub(super) const MAX_BULK_BYTE_LOOP_SPAN: i32 = 1 << 20;

pub(super) fn decode_bulk_int_load(
    code: &[u8],
    code_len: usize,
    pc: usize,
) -> Option<(usize, usize)> {
    if pc >= code_len {
        return None;
    }
    match code[pc] {
        0x15 if pc + 1 < code_len => Some((code[pc + 1] as usize, pc + 2)),
        0x1a..=0x1d => Some(((code[pc] - 0x1a) as usize, pc + 1)),
        _ => None,
    }
}

pub(super) fn decode_ref_load(code: &[u8], code_len: usize, pc: usize) -> Option<(usize, usize)> {
    if pc >= code_len {
        return None;
    }
    match code[pc] {
        0x19 if pc + 1 < code_len => Some((code[pc + 1] as usize, pc + 2)),
        0x2a..=0x2d => Some(((code[pc] - 0x2a) as usize, pc + 1)),
        _ => None,
    }
}

pub(super) fn detect_bulk_zero_byte_fill_loop(
    code: &[u8],
    code_len: usize,
    header: usize,
    back_edge: usize,
) -> Option<BulkZeroByteFillLoop> {
    if header >= back_edge || back_edge + 2 >= code_len || code[back_edge] != 0xa7 {
        return None;
    }
    let back_offset = i16::from_be_bytes([code[back_edge + 1], code[back_edge + 2]]) as i32;
    if back_edge.checked_add_signed(back_offset as isize) != Some(header) {
        return None;
    }

    let (iv_local, mut pc) = decode_bulk_int_load(code, code_len, header)?;
    let (bound_local, next) = decode_bulk_int_load(code, code_len, pc)?;
    pc = next;
    // Inclusive loop: continue while iv <= bound.
    if pc + 2 >= code_len || code[pc] != 0xa3 {
        return None;
    }
    let exit_offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
    let exit_pc = pc.checked_add_signed(exit_offset as isize)?;
    if exit_pc <= back_edge || exit_pc > code_len {
        return None;
    }
    pc += 3;

    let (array_local, next) = decode_ref_load(code, code_len, pc)?;
    pc = next;
    let (store_iv, next) = decode_bulk_int_load(code, code_len, pc)?;
    if store_iv != iv_local {
        return None;
    }
    pc = next;
    if pc >= code_len || code[pc] != 0x03 {
        return None;
    }
    pc += 1;
    if pc >= code_len || code[pc] != 0x54 {
        return None;
    }
    pc += 1;
    if pc + 2 >= code_len
        || code[pc] != 0x84
        || code[pc + 1] as usize != iv_local
        || code[pc + 2] != 1
    {
        return None;
    }
    pc += 3;
    if pc != back_edge {
        return None;
    }

    Some(BulkZeroByteFillLoop {
        header_pc: header,
        array_local,
        iv_local,
        bound_local,
    })
}

pub(super) fn detect_bulk_set_byte_stride_loop(
    code: &[u8],
    code_len: usize,
    header: usize,
    back_edge: usize,
) -> Option<BulkSetByteStrideLoop> {
    if header >= back_edge || back_edge + 2 >= code_len || code[back_edge] != 0xa7 {
        return None;
    }
    let back_offset = i16::from_be_bytes([code[back_edge + 1], code[back_edge + 2]]) as i32;
    if back_edge.checked_add_signed(back_offset as isize) != Some(header) {
        return None;
    }

    let (iv_local, mut pc) = decode_bulk_int_load(code, code_len, header)?;
    let (bound_local, next) = decode_bulk_int_load(code, code_len, pc)?;
    if bound_local == iv_local {
        return None;
    }
    pc = next;
    if pc + 2 >= code_len || code[pc] != 0xa3 {
        return None;
    }
    let exit_offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
    let exit_pc = pc.checked_add_signed(exit_offset as isize)?;
    if exit_pc <= back_edge || exit_pc > code_len {
        return None;
    }
    pc += 3;

    let (array_local, next) = decode_ref_load(code, code_len, pc)?;
    pc = next;
    let (store_iv, next) = decode_bulk_int_load(code, code_len, pc)?;
    if store_iv != iv_local {
        return None;
    }
    pc = next;
    if pc >= code_len || code[pc] != 0x04 {
        return None;
    }
    pc += 1;
    if pc >= code_len || code[pc] != 0x54 {
        return None;
    }
    pc += 1;

    let (add_iv, next) = decode_bulk_int_load(code, code_len, pc)?;
    if add_iv != iv_local {
        return None;
    }
    pc = next;
    let (step_local, next) = decode_bulk_int_load(code, code_len, pc)?;
    if step_local == iv_local {
        return None;
    }
    pc = next;
    if pc >= code_len || code[pc] != 0x60 {
        return None;
    }
    pc += 1;
    let (store_local, next) = decode_int_store(code, pc, code_len)?;
    if store_local != iv_local || next != back_edge {
        return None;
    }

    Some(BulkSetByteStrideLoop {
        header_pc: header,
        array_local,
        iv_local,
        bound_local,
        step_local,
    })
}

pub(super) fn detect_byte_sieve_loop(
    code: &[u8],
    code_len: usize,
    header: usize,
    back_edge: usize,
) -> Option<ByteSieveLoop> {
    if header >= back_edge || back_edge + 2 >= code_len || code[back_edge] != 0xa7 {
        return None;
    }
    let outer_back = i16::from_be_bytes([code[back_edge + 1], code[back_edge + 2]]) as i32;
    if back_edge.checked_add_signed(outer_back as isize) != Some(header) {
        return None;
    }

    let (outer_iv_local, mut pc) = decode_bulk_int_load(code, code_len, header)?;
    let (bound_local, next) = decode_bulk_int_load(code, code_len, pc)?;
    pc = next;
    if pc + 2 >= code_len || code[pc] != 0xa3 {
        return None;
    }
    let exit_offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
    let exit_pc = pc.checked_add_signed(exit_offset as isize)?;
    if exit_pc <= back_edge || exit_pc > code_len {
        return None;
    }
    pc += 3;

    let (array_local, next) = decode_ref_load(code, code_len, pc)?;
    pc = next;
    let (load_outer, next) = decode_bulk_int_load(code, code_len, pc)?;
    if load_outer != outer_iv_local {
        return None;
    }
    pc = next;
    if pc >= code_len || code[pc] != 0x33 {
        return None;
    }
    pc += 1;
    if pc + 2 >= code_len || code[pc] != 0x9a {
        return None;
    }
    let tail_offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
    let outer_tail = pc.checked_add_signed(tail_offset as isize)?;
    pc += 3;

    if pc + 2 >= code_len || code[pc] != 0x84 || code[pc + 2] != 1 {
        return None;
    }
    let count_local = code[pc + 1] as usize;
    pc += 3;
    let (outer_a, next) = decode_bulk_int_load(code, code_len, pc)?;
    pc = next;
    let (outer_b, next) = decode_bulk_int_load(code, code_len, pc)?;
    if outer_a != outer_iv_local || outer_b != outer_iv_local {
        return None;
    }
    pc = next;
    if pc >= code_len || code[pc] != 0x60 {
        return None;
    }
    pc += 1;
    let (inner_iv_local, next) = decode_int_store(code, pc, code_len)?;
    pc = next;
    let inner_header = pc;

    let (inner_load, next) = decode_bulk_int_load(code, code_len, pc)?;
    if inner_load != inner_iv_local {
        return None;
    }
    pc = next;
    let (inner_bound, next) = decode_bulk_int_load(code, code_len, pc)?;
    if inner_bound != bound_local {
        return None;
    }
    pc = next;
    if pc + 2 >= code_len || code[pc] != 0xa3 {
        return None;
    }
    let inner_exit = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
    if pc.checked_add_signed(inner_exit as isize) != Some(outer_tail) {
        return None;
    }
    pc += 3;
    let (inner_array, next) = decode_ref_load(code, code_len, pc)?;
    if inner_array != array_local {
        return None;
    }
    pc = next;
    let (store_inner, next) = decode_bulk_int_load(code, code_len, pc)?;
    if store_inner != inner_iv_local {
        return None;
    }
    pc = next;
    if pc + 1 >= code_len || code[pc] != 0x04 || code[pc + 1] != 0x54 {
        return None;
    }
    pc += 2;
    let (add_inner, next) = decode_bulk_int_load(code, code_len, pc)?;
    if add_inner != inner_iv_local {
        return None;
    }
    pc = next;
    let (step_outer, next) = decode_bulk_int_load(code, code_len, pc)?;
    if step_outer != outer_iv_local {
        return None;
    }
    pc = next;
    if pc >= code_len || code[pc] != 0x60 {
        return None;
    }
    pc += 1;
    let (store_inner, next) = decode_int_store(code, pc, code_len)?;
    if store_inner != inner_iv_local {
        return None;
    }
    pc = next;
    if pc + 2 >= code_len || code[pc] != 0xa7 {
        return None;
    }
    let inner_back = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
    if pc.checked_add_signed(inner_back as isize) != Some(inner_header) {
        return None;
    }
    pc += 3;
    if pc != outer_tail
        || pc + 3 != back_edge
        || code[pc] != 0x84
        || code[pc + 1] as usize != outer_iv_local
        || code[pc + 2] != 1
    {
        return None;
    }

    let distinct = [
        array_local,
        outer_iv_local,
        bound_local,
        count_local,
        inner_iv_local,
    ];
    for i in 0..distinct.len() {
        if distinct[i] >= 64 || distinct[(i + 1)..].contains(&distinct[i]) {
            return None;
        }
    }

    Some(ByteSieveLoop {
        header_pc: header,
        array_local,
        outer_iv_local,
        bound_local,
        count_local,
        inner_iv_local,
    })
}

/// Every bulk-byte loop lowering the single-pass backend found in one method.
///
/// These three detectors are the whole reason `CratonBench`'s `sieve` phase
/// runs at HotSpot speed: they replace a scalar `boolean[]` element loop with
/// a vectorised pre-header. They exist only on this backend — the optimizing
/// (IR) tier lowers the same loops one element at a time.
pub(super) struct BulkByteLoops {
    pub(super) zero_fill: Vec<BulkZeroByteFillLoop>,
    pub(super) set_stride: Vec<BulkSetByteStrideLoop>,
    pub(super) sieve: Vec<ByteSieveLoop>,
}

impl BulkByteLoops {
    pub(super) fn is_empty(&self) -> bool {
        self.zero_fill.is_empty() && self.set_stride.is_empty() && self.sieve.is_empty()
    }
}

/// Run all three bulk-byte detectors over one method's loops.
///
/// Factored out of the driver so that "would the single-pass backend
/// vectorise a loop in this method?" has exactly ONE answer. It is asked in
/// two places now — here, to emit the pre-headers, and from the optimizing
/// tier's admission chain to decline a method this backend does better — and
/// two copies of one predicate in two files is how the frame reservation and
/// the stub spill drifted 32 registers apart.
///
/// `rotation` is the driver's rotated-header map (empty unless
/// `CRATONVM_JIT_ROTATED_PREHEADER` is armed); a loop at one of its headers
/// is detected on its unrotated copy ([`detect_batch_loop`]) and its
/// pre-header is emitted at the entry `goto`.
pub(super) fn detect_bulk_byte_loops(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
    bypassable_headers: &FxHashSet<usize>,
    rotation: &FxHashMap<usize, usize>,
) -> BulkByteLoops {
    if !bulk_byte_loops_enabled() {
        return BulkByteLoops {
            zero_fill: Vec::new(),
            set_stride: Vec::new(),
            sieve: Vec::new(),
        };
    }
    BulkByteLoops {
        zero_fill: loops
            .iter()
            .filter_map(|&(h, b)| {
                detect_batch_loop(
                    code,
                    code_len,
                    h,
                    b,
                    bypassable_headers,
                    rotation,
                    detect_bulk_zero_byte_fill_loop,
                )
            })
            .collect(),
        set_stride: loops
            .iter()
            .filter_map(|&(h, b)| {
                detect_batch_loop(
                    code,
                    code_len,
                    h,
                    b,
                    bypassable_headers,
                    rotation,
                    detect_bulk_set_byte_stride_loop,
                )
            })
            .collect(),
        sieve: loops
            .iter()
            .filter_map(|&(h, b)| {
                detect_batch_loop(
                    code,
                    code_len,
                    h,
                    b,
                    bypassable_headers,
                    rotation,
                    detect_byte_sieve_loop,
                )
            })
            .collect(),    }}

/// Read once per compile, no `static` (j27 LATCHES).
pub(crate) fn bulk_byte_loops_enabled() -> bool {
    true
}

/// Find which locals are modified (stored/incremented) within a bytecode range.
/// Returns a bitmask where bit N is set if local N is modified, for N < 63.
///
/// **Bit 63 is saturating**: it is set when ANY slot >= 63 is written. Callers
/// ask about slot `k` as bit `k.min(63)`, which stays sound — a clear bit 63
/// means no slot >= 63 is written, a set one refuses every high slot — at the
/// price of one high write disabling every high slot's hoist.
///
/// This is NOT the same contract as `loop_analysis::modified_locals_strict`,
/// which refuses the whole answer (`None`) for a slot it cannot represent. An
/// earlier comment called the two twins (round 11 wave 2 corrected it,
/// `r11-iropt-stale-comments-outside-the-lane-FIXED-20260924.md`); a caller that
/// switches between them must switch its high-slot handling too.
pub(super) fn find_modified_locals(code: &[u8], start: usize, end: usize) -> u64 {
    let mut modified: u64 = 0;
    let mut pc = start;
    while pc < end {
        match code[pc] {
            // istore_0..istore_3
            0x3b..=0x3e => {
                modified |= 1 << (code[pc] - 0x3b);
                pc += 1;
            }
            // lstore_0..lstore_3.
            //
            // A `long` or `double` occupies TWO local slots, and this mask
            // recorded only the lower one until 2026-09-05. The upper one is
            // the "dead high half" — dead to the reader, written by the store
            // all the same — so a consumer asking about slot k+1 was told the
            // loop does not write it. `loop_analysis::modified_locals_strict`
            // (the reviewed sibling of this function; it refuses high slots
            // where this one saturates them, see the doc above) has always
            // marked it, with the reason in its doc: an invariance check built
            // on a set that quietly forgot a write is not a check.
            //
            // It was not hypothetical. `find_fp_loop_hoists` admits a
            // `dload k` on the strength of bit k alone, and TWO tests in this
            // tree (`p87_fp_loop_hoist_detection`,
            // `p87_fp_hoist_double_and_float`) asserted that a `dload_1` is
            // hoistable out of a loop whose body contains `dstore_0` — with
            // the comment "this modifies 0, but doesn't affect 1". `dstore_0`
            // writes slots 0 AND 1. Both tests were written from what the
            // implementation did, and both pinned an unsound hoist.
            //
            // Reachability, stated honestly: javac cannot emit that shape.
            // After `dstore_0` slot 1 holds TOP, so a later `dload_1` (or
            // `iload_1`) fails verification unless something re-stores the
            // slot first — which marks it anyway. So this costs nothing in
            // practice and buys agreement between the two masks. Like every
            // other arm here it can only ADD bits, i.e. only ever refuse a
            // hoist.
            0x3f..=0x42 => {
                modified |= 1 << (code[pc] - 0x3f);
                modified |= 1 << (code[pc] - 0x3f + 1);
                pc += 1;
            }
            // fstore_0..fstore_3
            0x43..=0x46 => {
                modified |= 1 << (code[pc] - 0x43);
                pc += 1;
            }
            // dstore_0..dstore_3 — the high half too; see `lstore_0` above.
            0x47..=0x4a => {
                modified |= 1 << (code[pc] - 0x47);
                modified |= 1 << (code[pc] - 0x47 + 1);
                pc += 1;
            }
            // astore_0..astore_3
            0x4b..=0x4e => {
                modified |= 1 << (code[pc] - 0x4b);
                pc += 1;
            }
            // istore/lstore/fstore/dstore/astore (1-byte index operand)
            0x36..=0x3a => {
                // Local index is 0..255; clamp the shift like every other
                // shift-by-local site (a high local saturates to bit 63, which
                // is conservatively treated as "some local >= 63 modified").
                // Widening: u8 -> wider int (bytecode operand byte, value fits)
                let slot = code[pc + 1] as usize;
                modified |= 1u64 << slot.min(63);
                // `lstore`/`dstore` write the high half too; see `lstore_0`.
                if matches!(code[pc], 0x37 | 0x39) {
                    modified |= 1u64 << (slot + 1).min(63);
                }
                pc += 2;
            }
            // iinc
            0x84 => {
                // Widening: u8 -> wider int (bytecode operand byte, value fits)
                modified |= 1u64 << (code[pc + 1] as usize).min(63);
                pc += 3;
            }
            // wide — the prefix this scan had no arm for at all.
            //
            // AUDIT 2026-09-05, and it was WRONG CODE. Every arm above
            // names a narrow-form store; `wide` re-encodes the same
            // stores with a `u16` local index, and `wide iinc` with a
            // `u16` index AND an `i16` delta. Without an arm they fell to
            // the length-only default below: the walk stayed aligned, so
            // nothing looked broken, and the local was never marked
            // modified.
            //
            // javac reaches for `wide iinc` whenever the delta does not
            // fit in a signed byte. `for (i = 0; i < n; i += 1024)` is
            // exactly that, so its induction variable read as
            // LOOP-INVARIANT to every consumer of this bitmask --
            // including `find_arith_loop_hoists`, which then hoisted
            // `i + r` into the pre-header and left every iteration
            // replaying the first one's value. Stride 1 was correct and
            // stride 1024 was not, which is what made it look like an
            // addressing bug rather than an invariance one. See
            // `test_classes/jit/OsrStridedValueMin.java` and
            // the retired `osr-miscompiles-cachecoherence-20260904` write-up.
            //
            // Over-marking is the safe direction here: this bitmask only
            // ever DISABLES a hoist, so a wide form that turns out not to
            // write a local costs a missed optimisation, never a wrong
            // answer.
            0xc4 if pc + 3 < end && pc + 3 < code.len() => {
                let widened = code[pc + 1];
                // `wide` operand layout: [c4][op][idx:u16] and, for iinc
                // only, a further [const:i16].
                let idx = u16::from_be_bytes([code[pc + 2], code[pc + 3]]) as usize;
                // istore/lstore/fstore/dstore/astore, and iinc.
                if widened == 0x84 || (0x36..=0x3a).contains(&widened) {
                    modified |= 1u64 << idx.min(63);
                }
                // `wide lstore`/`wide dstore` write the high half too; see
                // the `lstore_0..lstore_3` arm above for why that matters.
                if matches!(widened, 0x37 | 0x39) {
                    modified |= 1u64 << (idx + 1).min(63);
                }
                pc += if widened == 0x84 { 6 } else { 4 };
            }
            // Other: advance by instruction length
            _ => pc += bytecode_analysis::step(code, pc),
        }
    }
    modified
}

/// Try to match an invariant `aload X; iload Y; aaload` sequence at `pc`.
/// Returns `(array_local, index_local, seq_end_pc)` if the pattern matches
/// and both locals are not in the `modified` bitmask.
pub(super) fn match_invariant_aaload(
    code: &[u8],
    pc: usize,
    modified: u64,
    code_len: usize,
) -> Option<(usize, usize, usize)> {
    // Match aload variant (loads the Object[] array reference)
    let (array_local, next_pc) = match code[pc] {
        0x2a => (0usize, pc + 1),
        0x2b => (1, pc + 1),
        0x2c => (2, pc + 1),
        0x2d => (3, pc + 1),
        0x19 if pc + 1 < code_len => (code[pc + 1] as usize, pc + 2), // Widening: always safe
        _ => return None,
    };

    // Check array_local is not modified in this loop.
    //
    // `find_modified_locals` SATURATES a local at or above 63 onto bit 63
    // ("some local >= 63 modified"). This used to test the bit only for a local
    // below 64, so `aload 70; iload 5; aaload` in a loop that re-assigns local
    // 70 read the mask as "unmodified" and hoisted the element load out of it
    // (r9-ea). Read the saturated bit for a high local, as every other consumer
    // of that mask does — it can only refuse a hoist.
    if modified & (1u64 << array_local.min(63)) != 0 {
        return None;
    }

    // Match iload variant (loads the array index)
    if next_pc >= code_len {
        return None;
    }
    let (index_local, next_pc2) = match code[next_pc] {
        0x1a => (0usize, next_pc + 1),
        0x1b => (1, next_pc + 1),
        0x1c => (2, next_pc + 1),
        0x1d => (3, next_pc + 1),
        0x15 if next_pc + 1 < code_len => (code[next_pc + 1] as usize, next_pc + 2), // Widening: always safe
        _ => return None,
    };

    // Check index_local is not modified in this loop (saturated bit for a
    // high local — see the array check above).
    if modified & (1u64 << index_local.min(63)) != 0 {
        return None;
    }

    // Match aaload (0x32) — Object[] element access
    if next_pc2 >= code_len || code[next_pc2] != 0x32 {
        return None;
    }

    Some((array_local, index_local, next_pc2 + 1))
}

/// Kill switch for the driver's class-initialization fence on these hoists
/// (`compile_with_param_slots`, after the volatile filter): default on;
/// `CRATONVM_JIT_AALOAD_LICM_CLINIT_FENCE=0` keeps a hoist whose loop can run a
/// `<clinit>` or a class loader through a static-field, deferred-`new`/
/// `anewarray` or class-`ldc` helper (the round-12-wave-2 behaviour).

/// Find loop-invariant aaload sequences that can be hoisted out of loops.
/// For nested loops, hoists to the outermost loop where the sequence is
/// invariant and that no OSR entry can land inside (round 9 wave 8: in
/// practice the innermost loop containing the sequence, when it has no inner
/// loop of its own).
pub(super) fn find_loop_hoists(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
) -> Vec<LoopHoist> {
    if loops.is_empty() {
        return Vec::new();
    }

    let mut hoists = Vec::new();
    let mut hoisted_pcs: FxHashSet<usize> = FxHashSet::default();
    // Round 11 (lane x64loop): the walk replaces `aload; iload; aaload` with
    // ONE slot load emitted at `seq_start`, and points every interior pc's
    // `pc_to_native` AFTER that load. A branch into the interior therefore
    // lands past the replacement with its own operands never consumed:
    // `(c ? a : b)[r]` compiles to `ifeq L1; aload a; goto L2; L1: aload b;
    // L2: iload r; aaload`, the matcher takes `aload b; iload r; aaload` at
    // L1, and the `c == true` path arrives at L2 holding `a` itself as the
    // "element". The arraylength and arith hoists refuse an entered interior
    // (`interior_entered`, `interior_safe`); this one never did.
    let targets = bytecode_analysis::branch_target_map(code, code_len);
    let interior_is_entered = |start: usize, end: usize| -> bool {
        let mut q = start + bytecode_analysis::step(code, start);
        while q < end {
            if targets.get(q).copied().unwrap_or(true) {
                return true;
            }
            q += bytecode_analysis::step(code, q);
        }
        false
    };

    // Sort loops by span size descending (outermost first for nested loop handling)
    let mut sorted_loops = loops.to_vec();
    sorted_loops.sort_by_key(|&(h, b)| std::cmp::Reverse(b.saturating_sub(h)));
    let osr_entries = osr_entry_pcs(code, code_len);

    for &(header, back_edge) in &sorted_loops {
        let loop_end = back_edge + bytecode_analysis::step(code, back_edge);
        if loop_end > code_len {
            continue;
        }
        // "Outermost" stops at the first loop no OSR entry can land inside
        // (`licm::span_holds_inner_osr_entry`, which carries the measured
        // cost): a hoist on an outer loop would make the walk publish the
        // inner header OSR-ineligible (`inside_aaload_hoisted`). Skipped here,
        // the sequence falls to the inner loop on its turn -- where the
        // matrix-dot pre-header, which looks its row up by the INNER header,
        // can use the slot too.
        if span_holds_inner_osr_entry(&osr_entries, header, loop_end) {
            continue;
        }

        // Conservative safety: the hoisted `m[r]` is read once in the
        // pre-header, so nothing inside the loop may be able to store into
        // `m`. An `aastore` (0x53) is the direct way; any invoke (0xb6-0xba)
        // is the indirect one — the callee can `m[r] = ...`, or reach
        // `System.arraycopy`, `Arrays.fill`, a VarHandle or `Unsafe` store —
        // and a `monitorenter`/`monitorexit` (0xc2/0xc3) marks a region
        // where another thread's store is expected to become visible. The
        // arithmetic LICM (`licm_int::find_arith_loop_hoists`) already
        // refuses call-bearing loops for the same reason.
        let mut element_may_change = false;
        let mut check_pc = header;
        while check_pc < loop_end {
            if matches!(code[check_pc], 0x53 | 0xb6..=0xba | 0xc2 | 0xc3) {
                element_may_change = true;
                break;
            }
            check_pc += bytecode_analysis::step(code, check_pc);
        }
        if element_may_change {
            continue;
        }

        let modified = find_modified_locals(code, header, loop_end);

        let mut pc = header;
        while pc < loop_end && pc < code_len {
            if hoisted_pcs.contains(&pc) {
                // Already hoisted by an outer loop
                pc += bytecode_analysis::step(code, pc);
                continue;
            }

            if let Some((array_local, index_local, seq_end)) =
                match_invariant_aaload(code, pc, modified, code_len)
            {
                if seq_end <= loop_end && !interior_is_entered(pc, seq_end) {
                    hoists.push(LoopHoist {
                        loop_header: header,
                        loop_end,
                        seq_start: pc,
                        seq_end,
                        array_local,
                        index_local,
                        field: None,
                    });
                    hoisted_pcs.insert(pc);
                }
                pc = seq_end;
            } else {
                pc += bytecode_analysis::step(code, pc);
            }
        }
    }

    hoists
}

/// X2 (`docs/feature-designs/jit-machine-level-and-instruction-selection.md`; the design page
/// `r12w3-vec-field-array-hoist-x2-not-landed-FIXED-20260928.md`): the loop-invariant
/// `aload R; getfield F` sequences of an ARRAY field F whose value the loop
/// cannot change, grouped per `(loop, R, F's compact offset, F's index)`.
///
/// What this function refuses is what the BYTECODE can show; the driver adds
/// the table-backed refusals (de-spec, pre-header bypass, volatile sites,
/// class-initializing static / `ldc` sites) through the same filters the
/// `aaload` hoist takes, plus its own `<clinit>` filter that does not depend
/// on the `aaload` fence switch.
///
/// Soundness of reading `R.F` ONCE, in the pre-header, instead of per site:
///
/// * **A store in the loop.** Any `putfield` refuses the loop (any field:
///   `other.F = x` may alias `R`). `R` itself is unmodified (no store to its
///   local), so every site names the same object.
/// * **A call that may store it.** Any invoke (`0xb6..=0xba`) refuses the
///   loop, and so does everything that can run Java code in this thread
///   without one: `new`/`newarray`/`anewarray`/`multianewarray` (allocation
///   can collect, and a collection is where a VM may run Java inline),
///   `checkcast`/`instanceof` (class resolution can reach a class loader).
///   Class-initializing `getstatic`/`putstatic` and class-`ldc` sites are the
///   driver's (they need the resolver's tables).
/// * **Another thread.** With no synchronization action in the loop (no
///   monitor op, no volatile access, no call), a plain field read may be
///   satisfied by the same write at every site (JLS 17.4: nothing orders
///   another thread's write before any particular in-loop read). HotSpot
///   hoists the same read.
/// * **A volatile field.** Refused: a volatile site of F is never a hoist
///   site, and the driver's volatile filter refuses a loop with ANY volatile
///   access (an acquire orders every later plain read).
/// * **A final field.** Admitted like any other: a final field is never
///   written outside a constructor, and a constructor's own loop over
///   `this.f` has the `putfield` that refuses it.
/// * **Deopt.** The pre-header's guards deopt (reason 2) at the header,
///   where the interpreter re-executes the real `getfield`s; a deopt inside
///   the loop resumes with the pushed copy of the slot on its operand stack,
///   marked as a reference, which is the value the skipped `getfield` would
///   have pushed.
pub(super) fn find_field_array_hoists(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
    field_info: &[(usize, usize, u8)],
    compact_field_info: &[(usize, u32, bool)],
    volatile_field_pcs: &FxHashSet<usize>,
) -> Vec<LoopHoist> {
    if loops.is_empty() || field_info.is_empty() || compact_field_info.is_empty() {
        return Vec::new();
    }
    let code_len = code_len.min(code.len());
    let array_field_at: FxHashMap<usize, (usize, u8)> = field_info
        .iter()
        .map(|&(pc, index, tag)| (pc, (index, tag)))
        .collect();
    let compact_at: FxHashMap<usize, (u32, bool)> = compact_field_info
        .iter()
        .map(|&(pc, off, is_ref)| (pc, (off, is_ref)))
        .collect();
    let targets = bytecode_analysis::branch_target_map(code, code_len);
    let osr_entries = osr_entry_pcs(code, code_len);

    let mut sorted_loops = loops.to_vec();
    sorted_loops.sort_by_key(|&(h, b)| std::cmp::Reverse(b.saturating_sub(h)));
    let mut claimed: FxHashSet<usize> = FxHashSet::default();
    let mut hoists: Vec<LoopHoist> = Vec::new();

    for &(header, back_edge) in &sorted_loops {
        let loop_end = back_edge + bytecode_analysis::step(code, back_edge);
        if loop_end > code_len || header >= loop_end {
            continue;
        }
        if span_holds_inner_osr_entry(&osr_entries, header, loop_end) {
            continue;
        }
        let mut refused = false;
        let mut pc = header;
        while pc < loop_end {
            let Some(&op) = code.get(pc) else {
                refused = true;
                break;
            };
            if matches!(
                op,
                0xb5 | 0xb6..=0xba | 0xbb | 0xbc | 0xbd | 0xc0 | 0xc1 | 0xc2 | 0xc3 | 0xc5
            ) {
                refused = true;
                break;
            }
            pc += bytecode_analysis::step(code, pc);
        }
        if refused {
            continue;
        }
        let modified = find_modified_locals(code, header, loop_end);

        // `(receiver local, compact offset, field index) -> index into
        // `hoists`` for this loop. Both placements are in the key: a site
        // shares a slot only with sites that read the same word in a compact
        // AND in a legacy receiver.
        let mut by_key: FxHashMap<(usize, u32, usize), usize> = FxHashMap::default();
        let mut pc = header;
        while pc < loop_end {
            let Some(&op) = code.get(pc) else {
                break;
            };
            let step = bytecode_analysis::step(code, pc);
            let recv = match op {
                0x2a..=0x2d => Some(((op - 0x2a) as usize, 1usize)), // Widening: aload_n index
                0x19 => code.get(pc + 1).map(|&l| (l as usize, 2usize)), // Widening: u8 local
                _ => None,
            };
            let Some((recv, recv_len)) = recv else {
                pc += step;
                continue;
            };
            let get_pc = pc + recv_len;
            let seq_end = get_pc + 3;
            let admitted = !claimed.contains(&pc)
                && recv < 63
                && modified & (1u64 << recv) == 0
                && seq_end <= loop_end
                && code.get(get_pc) == Some(&0xb4)
                && !targets.get(get_pc).copied().unwrap_or(true)
                && !volatile_field_pcs.contains(&get_pc);
            let placement = match (array_field_at.get(&get_pc), compact_at.get(&get_pc)) {
                (Some(&(index, b'[')), Some(&(off, true))) if admitted => Some((off, index)),
                _ => None,
            };
            let Some((off, index)) = placement else {
                pc += step;
                continue;
            };
            claimed.insert(pc);
            match by_key.get(&(recv, off, index)).copied() {
                Some(i) => {
                    if let Some(field) = hoists.get_mut(i).and_then(|h| h.field.as_mut()) {
                        field.sites.push((pc, seq_end));
                    }
                }
                None => {
                    by_key.insert((recv, off, index), hoists.len());
                    hoists.push(LoopHoist {
                        loop_header: header,
                        loop_end,
                        seq_start: pc,
                        seq_end,
                        array_local: recv,
                        index_local: FIELD_HOIST_NO_INDEX,
                        field: Some(FieldHoist {
                            field_off: off,
                            field_index: index,
                            sites: vec![(pc, seq_end)],
                        }),
                    });
                }
            }
            pc = seq_end;
        }
    }
    hoists
}

/// Round 13 wave 3 (lane loopcall): the `aload R; getfield F` sites of `int`
/// fields inside the loops X2 already hoists an array field of, grouped per
/// `(loop, R, F's compact offset, F's index)` exactly as
/// [`find_field_array_hoists`] groups its sites. `x2_spans` are the
/// `(loop_header, loop_end)` of the SURVIVING X2 hoists (after every driver
/// filter), so each loop here has already passed every refusal a field read
/// hoisted out of it needs -- the finder's bytecode refusals (any `putfield`,
/// invoke, allocation, typecheck or monitor) and the driver's table-backed
/// ones (volatile access, class initialization, de-spec, pre-header bypass).
/// What is left to check is per SITE: an unmodified receiver local, a
/// non-volatile compact `int` field (`field_info` tag `I`, compact cell not a
/// reference), and a `getfield` no branch lands on.
///
/// Each record is an [`ArrayLenHoist`] with [`ArrayLenHoist::int_field`] set:
/// an `int` slot in no oop map, replaced at every site by a slot load, and
/// OSR-vetoed inside its loop, like the length hoist it rides with. Nested X2
/// spans give a site to the OUTERMOST one (its slot is live over the inner
/// loop too), as X2 itself does.
pub(super) fn find_field_int_hoists(
    code: &[u8],
    code_len: usize,
    x2_spans: &[(usize, usize)],
    field_info: &[(usize, usize, u8)],
    compact_field_info: &[(usize, u32, bool)],
    volatile_field_pcs: &FxHashSet<usize>,
) -> Vec<ArrayLenHoist> {
    if x2_spans.is_empty() || field_info.is_empty() || compact_field_info.is_empty() {
        return Vec::new();
    }
    let code_len = code_len.min(code.len());
    let field_at: FxHashMap<usize, (usize, u8)> = field_info
        .iter()
        .map(|&(pc, index, tag)| (pc, (index, tag)))
        .collect();
    let compact_at: FxHashMap<usize, (u32, bool)> = compact_field_info
        .iter()
        .map(|&(pc, off, is_ref)| (pc, (off, is_ref)))
        .collect();
    let targets = bytecode_analysis::branch_target_map(code, code_len);

    let mut spans = x2_spans.to_vec();
    spans.sort_unstable();
    spans.dedup();
    spans.sort_by_key(|&(h, e)| std::cmp::Reverse(e.saturating_sub(h)));
    let mut claimed: FxHashSet<usize> = FxHashSet::default();
    let mut hoists: Vec<ArrayLenHoist> = Vec::new();

    for &(header, loop_end) in &spans {
        if loop_end > code_len || header >= loop_end {
            continue;
        }
        let modified = find_modified_locals(code, header, loop_end);
        let mut by_key: FxHashMap<(usize, u32, usize), usize> = FxHashMap::default();
        let mut pc = header;
        while pc < loop_end {
            let Some(&op) = code.get(pc) else {
                break;
            };
            let step = bytecode_analysis::step(code, pc);
            if step == 0 {
                break;
            }
            let recv = match op {
                0x2a..=0x2d => Some(((op - 0x2a) as usize, 1usize)), // Widening: aload_n index
                0x19 => code.get(pc + 1).map(|&l| (l as usize, 2usize)), // Widening: u8 local
                _ => None,
            };
            let Some((recv, recv_len)) = recv else {
                pc += step;
                continue;
            };
            let get_pc = pc + recv_len;
            let seq_end = get_pc + 3;
            let admitted = !claimed.contains(&pc)
                && recv < 63
                && modified & (1u64 << recv) == 0
                && seq_end <= loop_end
                && code.get(get_pc) == Some(&0xb4)
                && !targets.get(get_pc).copied().unwrap_or(true)
                && !volatile_field_pcs.contains(&get_pc);
            let placement = match (field_at.get(&get_pc), compact_at.get(&get_pc)) {
                (Some(&(index, b'I')), Some(&(off, false))) if admitted => Some((off, index)),
                _ => None,
            };
            let Some((off, index)) = placement else {
                pc += step;
                continue;
            };
            claimed.insert(pc);
            match by_key.get(&(recv, off, index)).copied() {
                Some(i) => {
                    if let Some(h) = hoists.get_mut(i) {
                        h.sites.push((pc, seq_end));
                    }
                }
                None => {
                    by_key.insert((recv, off, index), hoists.len());
                    hoists.push(ArrayLenHoist {
                        loop_header: header,
                        loop_end,
                        array_local: recv,
                        sites: vec![(pc, seq_end)],
                        trap_bci: get_pc,
                        int_field: Some(IntFieldHoist {
                            field_off: off,
                            field_index: index,
                        }),
                    });
                }
            }
            pc = seq_end;
        }
    }
    hoists
}

// ---------------------------------------------------------------------------
// r9-ea: agreement between `analyze_escapes` and `plan_scalar_replacement`
// ---------------------------------------------------------------------------

#[cfg(test)]
mod r9_ea_tests;

// ---------------------------------------------------------------------------
// r9w2-spcore: rotated-loop pre-header relocation
// ---------------------------------------------------------------------------

#[cfg(test)]
mod rotation_preheader_tests;

/// Round 9 wave 4 (lane `x64core4`): the batch pre-header detectors on
/// ROTATED loops, through [`unrotate_loop`] / [`detect_batch_loop`]. Every
/// fixture is the exact bytecode ecj 3.45 emits for the Java in its doc
/// (`rotated-loops-lose-every-preheader-transform-20260918.md`).
#[cfg(test)]
mod rotated_batch_loop_tests;

#[cfg(test)]
mod r11_x64loop_aaload_hoist_tests;

#[cfg(test)]
mod r12w2_x64opt_ea_tests;

#[cfg(test)]
mod str819_init_skip_proof_tests;
