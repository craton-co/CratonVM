// SPDX-License-Identifier: Apache-2.0
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
    /// `true` only for a `<init>` whose descriptor is exactly `()V`.
    /// Such a constructor performs no field initialization beyond the
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
        if branch_targets.get(pc).copied().unwrap_or(false) {
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
                    }
                    None => {
                        // Descriptor unknown — conservatively escape the
                        // whole operand stack and clear it.
                        escape_all!();
                        abs_stack.clear();
                    }
                }
                // No push: invokespecial return type is void for <init>;
                // non-void private/super calls are rare and the cleared/
                // conservative state above already covers them.
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
    /// (relockable on deopt). A monitor op NOT in this set operated on a non-scalar
    /// object and keeps the method off the resume path.
    pub(super) monitor_scalar_ops: std::collections::HashSet<usize>,
}

/// Analyze bytecode to plan scalar replacement for non-escaping objects.
///
/// Uses abstract interpretation to track object provenance through the stack
/// and locals, identifying which putfield/getfield/invokespecial PCs operate
/// on scalar-replaced objects.
pub(super) fn plan_scalar_replacement(
    code: &[u8],
    code_len: usize,
    non_escaping_new: &std::collections::HashSet<usize>,
    new_info: &[(usize, u32, usize, bool, bool)],
    invoke_info: &[(usize, *const JitInvokeInfo)],
    scalar_base: usize,
) -> ScalarReplacementPlan {
    let empty = ScalarReplacementPlan {
        objects: FxHashMap::default(),
        field_ops: FxHashMap::default(),
        init_skips: std::collections::HashSet::new(),
        total_slots: 0,
        local_prov_at: FxHashMap::default(),
        monitor_at: FxHashMap::default(),
        monitor_scalar_ops: std::collections::HashSet::new(),
    };
    if non_escaping_new.is_empty() {
        return empty;
    }

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
    // a scalar object is relockable on deopt (recorded here); one over a non-scalar
    // (escaping) object is the pre-existing blanket-elision case and keeps the
    // method off the resume path (`has_elided_monitor` in codegen).
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

    let mut pc = 0usize;
    while pc < code_len {
        if branch_targets.get(pc).copied().unwrap_or(false) {
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
        {
            let held: Vec<(usize, u32)> = mon_depth
                .iter()
                .filter(|(_, &d)| d > 0)
                .map(|(&np, &d)| (np, d))
                .collect();
            if !held.is_empty() {
                monitor_at.insert(pc, held);
            }
        }
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
            0x19 => {
                let idx = code[pc + 1] as usize; // Widening: always safe
                abs_stack.push(if idx < 256 { local_prov[idx] } else { None });
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
            0x36..=0x39 => {
                if let Some(p) = abs_stack.pop().flatten() {
                    poisoned.insert(p);
                }
                let idx = code[pc + 1] as usize; // Widening: always safe
                local_prov[idx] = None;
                if matches!(op, 0x37 | 0x39) && idx + 1 < local_prov.len() {
                    local_prov[idx + 1] = None;
                }
                pc += 2;
            }
            // astore
            0x3A => {
                let idx = code[pc + 1] as usize; // Widening: always safe
                let val = abs_stack.pop().flatten();
                if idx < 256 {
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
            // goto — control transfer; same hard barrier as above.
            0xA7 => {
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
            // putstatic
            0xB3 => {
                abs_stack.pop();
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
                        // Zero-arg init: pop only `this`
                        let receiver = abs_stack.pop().flatten();
                        if let Some(new_pc) = receiver {
                            if objects.contains_key(&new_pc) {
                                init_skip_owner.insert(pc, new_pc);
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
                        *d = d.saturating_sub(1);
                    }
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
                pc += bytecode_analysis::step(code, pc);
            }
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
/// Routing external entries to a second entry point would mean carrying the
/// source PC through all 21 `forward_patches` sites plus every immediate
/// backward-branch resolution. Instead this predicate refuses to speculate on
/// such headers at all: the caller drops every hoist/guard whose header is
/// listed here, which can only ever *remove* an optimisation. Loops with a
/// single fall-through entry — the overwhelming majority, including every
/// javac `for`/`while`/`do` whose header is not also a branch target from
/// outside — keep hoisting unchanged.
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
    let mut bypassable: FxHashSet<usize> = FxHashSet::default();
    if loops.is_empty() {
        return bypassable;
    }

    // (src_pc, target_pc) for every explicit branch edge, from the shared
    // decoder. Control flow this scan cannot follow is OPAQUE, which marks
    // every loop bypassable: `ret`, `jsr`/`jsr_w` (their edge is still
    // recorded), and any switch `bytecode_analysis::switch_table` refuses.
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
        for &(header, _) in loops {
            bypassable.insert(header);
        }
        return bypassable;
    }

    for &(header, back_edge) in loops {
        let loop_end = back_edge + bytecode_analysis::step(code, back_edge);
        for &(src, target) in &edges {
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_ROTATED_PREHEADER")
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
            .collect(),
    }
}

pub(super) fn bulk_byte_loops_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_BULK_BYTE_LOOPS")
    })
}

/// Find which locals are modified (stored/incremented) within a bytecode range.
/// Returns a bitmask where bit N is set if local N is modified.
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
            // loop does not write it. `loop_analysis::modified_locals_strict`,
            // the reviewed twin of this function, has always marked it, with
            // the reason in its doc: an invariance check built on a set that
            // quietly forgot a write is not a check.
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
    let mut hoisted_pcs: Vec<usize> = Vec::new();

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
                if seq_end <= loop_end {
                    hoists.push(LoopHoist {
                        loop_header: header,
                        loop_end,
                        seq_start: pc,
                        seq_end,
                        array_local,
                        index_local,
                    });
                    hoisted_pcs.push(pc);
                }
                pc = seq_end;
            } else {
                pc += bytecode_analysis::step(code, pc);
            }
        }
    }

    hoists
}

// ---------------------------------------------------------------------------
// r9-ea: agreement between `analyze_escapes` and `plan_scalar_replacement`
// ---------------------------------------------------------------------------

#[cfg(test)]
mod r9_ea_tests {
    use super::{
        analyze_escapes, find_modified_locals, match_invariant_aaload, plan_scalar_replacement,
        InvokeSpecialShape,
    };
    use crate::JitInvokeInfo;
    use rustc_hash::FxHashMap;

    /// A leaked `<init>()V` info, as the existing plan tests build one.
    fn init_void() -> *const JitInvokeInfo {
        // LEAK(intentional): test-only; plan_scalar_replacement reads it through a raw pointer.
        Box::leak(Box::new(JitInvokeInfo {
            class_name: "Foo",
            method_name: "<init>",
            descriptor: "()V",
            num_jit_args: 1,
            return_type: b'V',
            invoke_kind: 1,
            declaring_class_id: 0,
        })) as *const JitInvokeInfo
    }

    fn trivial_init_at(pc: usize) -> FxHashMap<usize, InvokeSpecialShape> {
        let mut shapes = FxHashMap::default();
        shapes.insert(
            pc,
            InvokeSpecialShape {
                arg_slots: 1,
                is_trivial_void_init: true,
            },
        );
        shapes
    }

    fn non_escaping(pc: usize) -> std::collections::HashSet<usize> {
        let mut s = std::collections::HashSet::new();
        s.insert(pc);
        s
    }

    /// `Foo f = new Foo(); f.x = 1; String s = "" + n; return f.x;` — an
    /// `invokedynamic` between two field accesses of a local scalar object.
    /// The plan's catch-all used to drop the local, leaving the `getfield`
    /// after the indy unmapped: a real field read on the dummy-null `new`.
    #[test]
    fn an_invokedynamic_does_not_unmap_a_local_scalar_objects_later_field_ops() {
        let code: Vec<u8> = vec![
            0xbb, 0x00, 0x01, // 0: new #1
            0x59, // 3: dup
            0xb7, 0x00, 0x02, // 4: invokespecial <init>()V
            0x4c, // 7: astore_1
            0x2b, // 8: aload_1
            0x04, // 9: iconst_1
            0xb5, 0x00, 0x03, // 10: putfield #3
            0xba, 0x00, 0x04, 0x00, 0x00, // 13: invokedynamic #4
            0x57, // 18: pop
            0x2b, // 19: aload_1
            0xb4, 0x00, 0x03, // 20: getfield #3
            0xac, // 23: ireturn
        ];
        let len = code.len();
        assert!(analyze_escapes(&code, len, &trivial_init_at(4)).contains(&0));
        let new_info = vec![(0usize, 1u32, 1usize, true, true)];
        let invoke_info = vec![(4usize, init_void())];
        let plan =
            plan_scalar_replacement(&code, len, &non_escaping(0), &new_info, &invoke_info, 0);
        assert!(plan.objects.contains_key(&0), "Foo is scalar-replaced");
        assert!(
            plan.field_ops.contains_key(&10),
            "putfield before the indy is mapped"
        );
        assert!(
            plan.field_ops.contains_key(&20),
            "getfield AFTER the indy must be mapped too, or it reads the dummy null"
        );
    }

    /// `p.x = a + 1` keeps `p` on the operand stack across the `iadd`. The
    /// arithmetic used to hit `analyze_escapes`' catch-all and escape `p`.
    #[test]
    fn arithmetic_under_a_tracked_receiver_does_not_escape_it() {
        let code: Vec<u8> = vec![
            0xbb, 0x00, 0x01, // 0: new #1
            0x59, // 3: dup
            0xb7, 0x00, 0x02, // 4: invokespecial <init>()V
            0x4c, // 7: astore_1
            0x2b, // 8: aload_1
            0x1a, // 9: iload_0
            0x04, // 10: iconst_1
            0x60, // 11: iadd
            0xb5, 0x00, 0x03, // 12: putfield #3
            0x2b, // 15: aload_1
            0xb4, 0x00, 0x03, // 16: getfield #3
            0xac, // 19: ireturn
        ];
        let len = code.len();
        assert!(
            analyze_escapes(&code, len, &trivial_init_at(4)).contains(&0),
            "an iadd reads and writes primitives only"
        );
        let new_info = vec![(0usize, 1u32, 1usize, true, true)];
        let invoke_info = vec![(4usize, init_void())];
        let plan =
            plan_scalar_replacement(&code, len, &non_escaping(0), &new_info, &invoke_info, 0);
        assert!(plan.field_ops.contains_key(&12));
        assert!(plan.field_ops.contains_key(&16));
    }

    /// `{ Foo f = new Foo(); f.x = 1; } int y = 5; return y;` — javac reuses
    /// slot 1. The dead object must stop being published in that slot.
    #[test]
    fn a_primitive_store_ends_the_objects_provenance_in_that_slot() {
        let code: Vec<u8> = vec![
            0xbb, 0x00, 0x01, // 0: new #1
            0x59, // 3: dup
            0xb7, 0x00, 0x02, // 4: invokespecial <init>()V
            0x4c, // 7: astore_1
            0x2b, // 8: aload_1
            0x04, // 9: iconst_1
            0xb5, 0x00, 0x03, // 10: putfield #3
            0x08, // 13: iconst_5
            0x3c, // 14: istore_1
            0x1b, // 15: iload_1
            0xac, // 16: ireturn
        ];
        let len = code.len();
        assert!(analyze_escapes(&code, len, &trivial_init_at(4)).contains(&0));
        let new_info = vec![(0usize, 1u32, 1usize, true, true)];
        let invoke_info = vec![(4usize, init_void())];
        let plan =
            plan_scalar_replacement(&code, len, &non_escaping(0), &new_info, &invoke_info, 0);
        assert!(plan.objects.contains_key(&0));
        assert_eq!(
            plan.local_prov_at.get(&14).map(|v| v.as_slice()),
            Some([(1usize, 0usize)].as_slice()),
            "slot 1 still holds the object when istore_1 starts"
        );
        assert!(
            !plan.local_prov_at.contains_key(&15),
            "after istore_1 slot 1 is an int, not a VirtualObject"
        );
    }

    /// `if (c) { Foo f = new Foo(); return f.x; } return 0;` — the `return`
    /// ends `f`'s path, so the branch target after it must not escape `f`.
    #[test]
    fn an_early_return_does_not_hand_its_locals_to_the_next_branch_target() {
        let code: Vec<u8> = vec![
            0x1a, // 0: iload_0
            0x99, 0x00, 0x10, // 1: ifeq +16 -> 17
            0xbb, 0x00, 0x01, // 4: new #1
            0x59, // 7: dup
            0xb7, 0x00, 0x02, // 8: invokespecial <init>()V
            0x4c, // 11: astore_1
            0x2b, // 12: aload_1
            0xb4, 0x00, 0x03, // 13: getfield #3
            0xac, // 16: ireturn
            0x03, // 17: iconst_0   (branch target)
            0xac, // 18: ireturn
        ];
        let len = code.len();
        assert!(
            analyze_escapes(&code, len, &trivial_init_at(8)).contains(&4),
            "the object's whole life ends at the ireturn at 16"
        );
        let new_info = vec![(4usize, 1u32, 1usize, true, true)];
        let invoke_info = vec![(8usize, init_void())];
        let plan =
            plan_scalar_replacement(&code, len, &non_escaping(4), &new_info, &invoke_info, 0);
        assert!(plan.objects.contains_key(&4));
        assert!(plan.init_skips.contains(&8));
        assert!(plan.field_ops.contains_key(&13));
        assert!(!plan.local_prov_at.contains_key(&17));
    }

    /// `new; astore_1; aload_1; invokespecial <init>()V` — the init skip is
    /// owned by the object the walk saw, not by `init_pc - 4`.
    #[test]
    fn a_non_canonical_init_keeps_its_skip() {
        let code: Vec<u8> = vec![
            0xbb, 0x00, 0x01, // 0: new #1
            0x4c, // 3: astore_1
            0x2b, // 4: aload_1
            0xb7, 0x00, 0x02, // 5: invokespecial <init>()V
            0x2b, // 8: aload_1
            0x04, // 9: iconst_1
            0xb5, 0x00, 0x03, // 10: putfield #3
            0xb1, // 13: return
        ];
        let len = code.len();
        let new_info = vec![(0usize, 1u32, 1usize, true, true)];
        let invoke_info = vec![(5usize, init_void())];
        let plan =
            plan_scalar_replacement(&code, len, &non_escaping(0), &new_info, &invoke_info, 0);
        assert!(plan.objects.contains_key(&0));
        assert!(
            plan.init_skips.contains(&5),
            "the <init> of a scalar object must be skipped, or it is called on the dummy null"
        );
    }

    /// Provenance lost while live (here: a `wide` the plan does not decode)
    /// demotes the object to a real allocation — and takes its monitor
    /// elision with it.
    #[test]
    fn lost_provenance_demotes_the_object_and_its_monitor_ops() {
        let code: Vec<u8> = vec![
            0xbb, 0x00, 0x01, // 0: new #1
            0x59, // 3: dup
            0xb7, 0x00, 0x02, // 4: invokespecial <init>()V
            0x4c, // 7: astore_1
            0x2b, // 8: aload_1
            0xc2, // 9: monitorenter
            0xc4, 0x15, 0x00, 0x02, // 10: wide iload 2
            0x57, // 14: pop
            0x2b, // 15: aload_1
            0xc3, // 16: monitorexit
            0x2b, // 17: aload_1
            0xb4, 0x00, 0x03, // 18: getfield #3
            0xac, // 21: ireturn
        ];
        let len = code.len();
        assert!(
            !analyze_escapes(&code, len, &trivial_init_at(4)).contains(&0),
            "a `wide` is a barrier for the escape walk too"
        );
        // Handed the object anyway (the two walks disagreeing is exactly the
        // case this guards), the plan must refuse it whole.
        let new_info = vec![(0usize, 1u32, 1usize, true, true)];
        let invoke_info = vec![(4usize, init_void())];
        let plan =
            plan_scalar_replacement(&code, len, &non_escaping(0), &new_info, &invoke_info, 0);
        assert!(
            plan.objects.is_empty(),
            "a poisoned object is allocated for real"
        );
        assert!(plan.field_ops.is_empty());
        assert!(plan.init_skips.is_empty());
        assert!(
            !plan.monitor_scalar_ops.contains(&9),
            "the lock on a real allocation must not be elided without a relock record"
        );
    }

    /// `aload 70; iload_1; aaload` in a loop that re-assigns local 70. The
    /// modified-locals mask saturates every local >= 63 onto bit 63, and the
    /// hoist matcher used to ignore that bit for a local >= 64.
    #[test]
    fn a_high_local_the_loop_reassigns_is_not_hoisted() {
        // The loop body's store: `astore 70`.
        let body: Vec<u8> = vec![0x3a, 70];
        let modified = find_modified_locals(&body, 0, body.len());
        assert_ne!(modified & (1u64 << 63), 0, "local 70 saturates onto bit 63");
        let seq: Vec<u8> = vec![0x19, 70, 0x1b, 0x32]; // aload 70; iload_1; aaload
        assert!(
            match_invariant_aaload(&seq, 0, modified, seq.len()).is_none(),
            "a re-assigned array local must not be read once in the pre-header"
        );
        assert!(
            match_invariant_aaload(&seq, 0, 0, seq.len()).is_some(),
            "an unmodified one still is"
        );
    }

    /// `x += 1000` compiles to `wide iinc`. It used to reach both walks'
    /// catch-alls — which disagreed (the escape walk kept the locals, the plan
    /// dropped them), leaving `f.x` after the `iinc` a real read of the dummy
    /// null. It is now modelled exactly in both.
    #[test]
    fn a_wide_iinc_neither_escapes_nor_unmaps_a_local_scalar_object() {
        let code: Vec<u8> = vec![
            0xbb, 0x00, 0x01, // 0: new #1
            0x59, // 3: dup
            0xb7, 0x00, 0x02, // 4: invokespecial <init>()V
            0x4c, // 7: astore_1
            0x2b, // 8: aload_1
            0x04, // 9: iconst_1
            0xb5, 0x00, 0x03, // 10: putfield #3
            0xc4, 0x84, 0x00, 0x02, 0x03, 0xe8, // 13: wide iinc 2, 1000
            0x2b, // 19: aload_1
            0xb4, 0x00, 0x03, // 20: getfield #3
            0xac, // 23: ireturn
        ];
        let len = code.len();
        assert!(analyze_escapes(&code, len, &trivial_init_at(4)).contains(&0));
        let new_info = vec![(0usize, 1u32, 1usize, true, true)];
        let invoke_info = vec![(4usize, init_void())];
        let plan =
            plan_scalar_replacement(&code, len, &non_escaping(0), &new_info, &invoke_info, 0);
        assert!(plan.objects.contains_key(&0));
        assert!(plan.field_ops.contains_key(&10));
        assert!(
            plan.field_ops.contains_key(&20),
            "the getfield after `wide iinc` must stay mapped to the frame slot"
        );
    }
}

// ---------------------------------------------------------------------------
// r9w2-spcore: rotated-loop pre-header relocation
// ---------------------------------------------------------------------------

#[cfg(test)]
mod rotation_preheader_tests {
    use super::{detect_loops, find_bypassable_loop_headers, find_rotation_preheader_entries};

    /// `s = 0; i = 0; goto COND; BODY: s += x*3 + 11; i++; COND: if (i < n)
    /// goto BODY; return s` — locals 0=x, 1=n, 2=s, 3=i.
    fn rotated() -> Vec<u8> {
        vec![
            0x03, // 0: iconst_0
            0x3d, // 1: istore_2
            0x03, // 2: iconst_0
            0x3e, // 3: istore_3
            0xa7, 0x00, 0x0f, // 4: goto +15 -> 19
            0x1c, // 7: iload_2             ; header
            0x1a, // 8: iload_0
            0x06, // 9: iconst_3
            0x68, // 10: imul
            0x10, 0x0b, // 11: bipush 11
            0x60, // 13: iadd
            0x60, // 14: iadd
            0x3d, // 15: istore_2
            0x84, 0x03, 0x01, // 16: iinc 3, 1
            0x1d, // 19: iload_3            ; COND
            0x1b, // 20: iload_1
            0xa1, 0xff, 0xf2, // 21: if_icmplt -14 -> 7
            0x1c, // 24: iload_2
            0xac, // 25: ireturn
        ]
    }

    #[test]
    fn the_rotation_goto_is_the_preheader_site_of_a_bypassable_header() {
        let code = rotated();
        let len = code.len();
        let loops = detect_loops(&code, len);
        assert_eq!(loops, vec![(7, 21)]);
        assert!(
            find_bypassable_loop_headers(&code, len, &loops, &[]).contains(&7),
            "the rotated header is bypassable for the header-anchored pre-header"
        );
        let entries = find_rotation_preheader_entries(&code, len, &loops, &[]);
        assert_eq!(entries.get(&7), Some(&4));
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn a_second_entry_into_the_loop_disqualifies_the_rotation() {
        // Prefix `iload_0; ifne +N` jumping straight into BODY past the goto.
        let mut code = vec![0x1a, 0x9a, 0x00, 0x00]; // 0: iload_0; 1: ifne -> patched
        let base = code.len();
        code.extend(rotated());
        // BODY is at base + 7; the `ifne` at pc 1 targets it.
        let off = (base + 7 - 1) as i16;
        code[2..4].copy_from_slice(&off.to_be_bytes());
        let len = code.len();
        let loops = detect_loops(&code, len);
        assert_eq!(loops, vec![(base + 7, base + 21)]);
        assert!(find_rotation_preheader_entries(&code, len, &loops, &[]).is_empty());
    }

    #[test]
    fn a_branch_to_the_goto_itself_disqualifies_the_rotation() {
        // `iload_0; ifne +3` lands on the goto; the relocated pre-header would
        // sit before `pc_to_native[goto]` and that edge would skip it.
        let mut code = vec![0x1a, 0x9a, 0x00, 0x00];
        let base = code.len();
        code.extend(rotated());
        let goto_pc = base + 4;
        let off = (goto_pc - 1) as i16;
        code[2..4].copy_from_slice(&off.to_be_bytes());
        let len = code.len();
        let loops = detect_loops(&code, len);
        assert!(find_rotation_preheader_entries(&code, len, &loops, &[]).is_empty());
    }

    #[test]
    fn a_handler_at_the_goto_or_into_the_loop_disqualifies_the_rotation() {
        let code = rotated();
        let len = code.len();
        let loops = detect_loops(&code, len);
        // A handler starting at the goto.
        assert!(find_rotation_preheader_entries(&code, len, &loops, &[(0, 4, 4)]).is_empty());
        // A handler inside the loop whose protected range starts before it.
        assert!(find_rotation_preheader_entries(&code, len, &loops, &[(0, 10, 19)]).is_empty());
        // A range wholly inside the loop is fine.
        assert_eq!(
            find_rotation_preheader_entries(&code, len, &loops, &[(7, 16, 19)]).get(&7),
            Some(&4)
        );
    }

    #[test]
    fn a_top_tested_loop_is_not_a_rotation() {
        // javac: `i = 0; H: if (i >= n) exit; i++; goto H` — the header falls
        // through from the code before it; no rotation entry.
        let code = vec![
            0x03, // 0: iconst_0
            0x3c, // 1: istore_1
            0x1b, // 2: iload_1            ; header
            0x1a, // 3: iload_0
            0xa2, 0x00, 0x09, // 4: if_icmpge +9 -> 13
            0x84, 0x01, 0x01, // 7: iinc 1, 1
            0xa7, 0xff, 0xf8, // 10: goto -8 -> 2
            0xb1, // 13: return
        ];
        let len = code.len();
        let loops = detect_loops(&code, len);
        assert!(find_rotation_preheader_entries(&code, len, &loops, &[]).is_empty());
    }
}

/// Round 9 wave 4 (lane `x64core4`): the batch pre-header detectors on
/// ROTATED loops, through [`unrotate_loop`] / [`detect_batch_loop`]. Every
/// fixture is the exact bytecode ecj 3.45 emits for the Java in its doc
/// (`rotated-loops-lose-every-preheader-transform-20260918.md`).
#[cfg(test)]
mod rotated_batch_loop_tests {
    use super::{
        detect_batch_loop, detect_bulk_set_byte_stride_loop, detect_bulk_zero_byte_fill_loop,
        detect_byte_sieve_loop, detect_loops, find_bypassable_loop_headers,
        find_rotation_preheader_entries, unrotate_loop, unrotate_loop_in_place,
        unrotate_nested_loops,
    };
    use crate::x64::{
        bytecode_analysis, detect_int_array_element_wise_forms, detect_int_array_sum_forms,
        detect_matrix_dot_loop, find_induction_variable, SimdLoopBound,
    };
    use rustc_hash::{FxHashMap, FxHashSet};

    /// `static void z(boolean[] a, int limit) { for (int i = 0; i <= limit; i++) a[i] = false; }`
    fn ecj_zero_fill() -> Vec<u8> {
        vec![
            0x03, 0x3d, // 0: i = 0
            0xa7, 0x00, 0x0a, // 2: goto 12
            0x2a, 0x1c, 0x03, 0x54, // 5: a[i] = 0        ; header
            0x84, 0x02, 0x01, // 9: i++
            0x1c, 0x1b, // 12: iload i; iload limit     ; COND
            0xa4, 0xff, 0xf7, // 14: if_icmple 5
            0xb1, // 17: return
        ]
    }

    /// `static int s(int[] a) { int s = 0; int n = a.length; for (int i = 0; i < n; i++) s += a[i]; return s; }`
    fn ecj_int_sum() -> Vec<u8> {
        vec![
            0x03, 0x3c, // 0: s = 0
            0x2a, 0xbe, 0x3d, // 2: n = a.length
            0x03, 0x3e, // 5: i = 0
            0xa7, 0x00, 0x0c, // 7: goto 19
            0x1b, 0x2a, 0x1d, 0x2e, 0x60, 0x3c, // 10: s += a[i]  ; header
            0x84, 0x03, 0x01, // 16: i++
            0x1d, 0x1c, // 19: iload i; iload n          ; COND
            0xa1, 0xff, 0xf5, // 21: if_icmplt 10
            0x1b, 0xac, // 24: return s
        ]
    }

    /// `static void add(int[] c, int[] a, int[] b) { for (int i = 0; i < c.length; i++) c[i] = a[i] + b[i]; }`
    fn ecj_add_over_length() -> Vec<u8> {
        vec![
            0x03, 0x3e, // 0: i = 0
            0xa7, 0x00, 0x10, // 2: goto 18
            0x2a, 0x1d, 0x2b, 0x1d, 0x2e, 0x2c, 0x1d, 0x2e, 0x60,
            0x4f, // 5: c[i] = a[i] + b[i]
            0x84, 0x03, 0x01, // 15: i++
            0x1d, 0x2a, 0xbe, // 18: iload i; aload c; arraylength   ; COND
            0xa1, 0xff, 0xf0, // 21: if_icmplt 5
            0xb1, // 24: return
        ]
    }

    /// `CratonBench.matmul(int[][] a, int[][] b, int n)` as ecj compiles it.
    fn ecj_matmul() -> Vec<u8> {
        vec![
            0x1c, 0x1c, 0xc5, 0x00, 0x34, 0x02, // 0: new int[n][n]
            0x4e, // 6: astore_3 (c)
            0x03, 0x36, 0x04, // 7: i = 0
            0xa7, 0x00, 0x44, // 10: goto 78
            0x03, 0x36, 0x05, // 13: j = 0
            0xa7, 0x00, 0x35, // 16: goto 69
            0x03, 0x36, 0x06, // 19: sum = 0
            0x03, 0x36, 0x07, // 22: k = 0
            0xa7, 0x00, 0x1a, // 25: goto 51
            0x15, 0x06, 0x2a, 0x15, 0x04, 0x32, 0x15, 0x07, 0x2e, // 28: sum, a[i][k]
            0x2b, 0x15, 0x07, 0x32, 0x15, 0x05, 0x2e, // 37: b[k][j]
            0x68, 0x60, 0x36, 0x06, // 44: sum += a[i][k] * b[k][j]
            0x84, 0x07, 0x01, // 48: k++
            0x15, 0x07, 0x1c, 0xa1, 0xff, 0xe6, // 51: if (k < n) goto 28
            0x2d, 0x15, 0x04, 0x32, 0x15, 0x05, 0x15, 0x06, 0x4f, // 57: c[i][j] = sum
            0x84, 0x05, 0x01, // 66: j++
            0x15, 0x05, 0x1c, 0xa1, 0xff, 0xcb, // 69: if (j < n) goto 19
            0x84, 0x04, 0x01, // 75: i++
            0x15, 0x04, 0x1c, 0xa1, 0xff, 0xbc, // 78: if (i < n) goto 13
            0x2d, 0xb0, // 84: return c
        ]
    }

    /// `CratonBench.sieve(boolean[] a, int limit)` as ecj compiles it.
    fn ecj_sieve() -> Vec<u8> {
        vec![
            0x03, 0x3d, 0xa7, 0x00, 0x0a, // 0: i = 0; goto 12
            0x2a, 0x1c, 0x03, 0x54, 0x84, 0x02, 0x01, // 5: a[i] = false; i++
            0x1c, 0x1b, 0xa4, 0xff, 0xf7, // 12: if (i <= limit) goto 5
            0x03, 0x3d, 0x05, 0x3e, // 17: count = 0; i = 2
            0xa7, 0x00, 0x28, // 21: goto 61
            0x2a, 0x1d, 0x33, 0x9a, 0x00, 0x1f, // 24: if (a[i]) goto 58
            0x84, 0x02, 0x01, // 30: count++
            0x1d, 0x1d, 0x60, 0x36, 0x04, // 33: j = i + i
            0xa7, 0x00, 0x0e, // 38: goto 52
            0x2a, 0x15, 0x04, 0x04, 0x54, // 41: a[j] = true
            0x15, 0x04, 0x1d, 0x60, 0x36, 0x04, // 46: j += i
            0x15, 0x04, 0x1b, 0xa4, 0xff, 0xf2, // 52: if (j <= limit) goto 41
            0x84, 0x03, 0x01, // 58: i++
            0x1d, 0x1b, 0xa4, 0xff, 0xd9, // 61: if (i <= limit) goto 24
            0x1c, 0xac, // 66: return count
        ]
    }

    /// The driver's inputs for `code`: loops, bypassable headers, and the
    /// rotation map as `CRATONVM_JIT_ROTATED_PREHEADER=1` computes it.
    fn analyse(
        code: &[u8],
    ) -> (
        Vec<(usize, usize)>,
        FxHashSet<usize>,
        FxHashMap<usize, usize>,
    ) {
        let len = code.len();
        let loops = detect_loops(code, len);
        let bypassable = find_bypassable_loop_headers(code, len, &loops, &[]);
        let rotation = find_rotation_preheader_entries(code, len, &loops, &[]);
        (loops, bypassable, rotation)
    }

    #[test]
    fn a_rotated_zero_fill_unrotates_to_the_javac_shape() {
        let code = ecj_zero_fill();
        let (loops, bypassable, rotation) = analyse(&code);
        assert_eq!(loops, vec![(5, 14)]);
        assert!(bypassable.contains(&5));
        assert_eq!(rotation.get(&5), Some(&2));
        let u = unrotate_loop(&code, code.len(), 5, 14).expect("the ecj loop unrotates");
        assert_eq!(u.header, 0);
        assert_eq!(u.back_edge, 12);
        assert_eq!(
            u.code,
            vec![
                0x1c, 0x1b, 0xa3, 0x00, 0x0d, // COND; if_icmpgt EXIT(15)
                0x2a, 0x1c, 0x03, 0x54, 0x84, 0x02, 0x01, // BODY
                0xa7, 0xff, 0xf4, // goto 0
                0xb1, 0x00, 0x00, // EXIT
            ],
            "exactly javac's top-tested spelling of the same loop"
        );
        // Through the driver's rule: detected, re-anchored at the real header.
        let fill = detect_batch_loop(
            &code,
            code.len(),
            5,
            14,
            &bypassable,
            &rotation,
            detect_bulk_zero_byte_fill_loop,
        )
        .expect("a relocated rotated zero fill is detected");
        assert_eq!(fill.header_pc, 5);
        assert_eq!(
            (fill.array_local, fill.iv_local, fill.bound_local),
            (0, 2, 1)
        );
        // Unarmed (no rotation map): the bypassable header keeps its veto.
        assert!(detect_batch_loop(
            &code,
            code.len(),
            5,
            14,
            &bypassable,
            &FxHashMap::default(),
            detect_bulk_zero_byte_fill_loop,
        )
        .is_none());
    }

    #[test]
    fn a_rotated_int_sum_is_detected_with_its_real_header() {
        let code = ecj_int_sum();
        let (loops, bypassable, rotation) = analyse(&code);
        assert_eq!(loops, vec![(10, 21)]);
        assert_eq!(rotation.get(&10), Some(&7));
        let sum = detect_batch_loop(
            &code,
            code.len(),
            10,
            21,
            &bypassable,
            &rotation,
            |c, _, h, b| {
                let iv = find_induction_variable(c, h, b + bytecode_analysis::step(c, b))?;
                detect_int_array_sum_forms(c, h, b, iv, true)
            },
        )
        .expect("the rotated `s += a[i]` loop is a SIMD sum");
        assert_eq!((sum.header_pc, sum.back_edge_pc), (10, 21));
        assert_eq!((sum.iv_local, sum.acc_local, sum.array_local), (3, 1, 0));
        assert_eq!(sum.bound, SimdLoopBound::Local(2));
        assert!(!sum.acc_is_long);
    }

    #[test]
    fn a_rotated_element_wise_loop_over_a_length_is_detected() {
        let code = ecj_add_over_length();
        let (_, bypassable, rotation) = analyse(&code);
        assert_eq!(rotation.get(&5), Some(&2));
        let e = detect_batch_loop(
            &code,
            code.len(),
            5,
            21,
            &bypassable,
            &rotation,
            |c, _, h, b| {
                let iv = find_induction_variable(c, h, b + bytecode_analysis::step(c, b))?;
                detect_int_array_element_wise_forms(c, h, b, iv, true)
            },
        )
        .expect("the rotated `c[i] = a[i] + b[i]` loop is element-wise");
        assert_eq!(e.header_pc, 5);
        assert_eq!((e.out_local, e.a_local, e.b_local), (0, 1, 2));
        assert_eq!(e.bound, SimdLoopBound::ArrayLength(0));
    }

    #[test]
    fn the_rotated_matrix_dot_inner_loop_is_detected() {
        let code = ecj_matmul();
        let (loops, bypassable, rotation) = analyse(&code);
        assert!(loops.contains(&(28, 54)), "{loops:?}");
        assert_eq!(rotation.get(&28), Some(&25));
        let dot = detect_batch_loop(
            &code,
            code.len(),
            28,
            54,
            &bypassable,
            &rotation,
            |c, _, h, b| {
                let iv = find_induction_variable(c, h, b + bytecode_analysis::step(c, b))?;
                detect_matrix_dot_loop(c, h, b, iv)
            },
        )
        .expect("the rotated k-loop is the matrix dot");
        assert_eq!((dot.header_pc, dot.back_edge_pc), (28, 54));
        assert_eq!(
            (dot.iv_local, dot.bound_local, dot.acc_local),
            (7, 2, 6),
            "k, n, sum"
        );
        assert_eq!(
            (
                dot.a_outer_local,
                dot.a_row_local,
                dot.b_outer_local,
                dot.b_column_local
            ),
            (0, 4, 1, 5)
        );
    }

    #[test]
    fn the_rotated_sieve_stride_loop_and_the_nest_are_detected() {
        let code = ecj_sieve();
        let (loops, bypassable, rotation) = analyse(&code);
        for l in [(5, 14), (41, 55), (24, 63)] {
            assert!(loops.contains(&l), "{l:?} in {loops:?}");
        }
        assert_eq!(rotation.get(&41), Some(&38));
        assert_eq!(rotation.get(&24), Some(&21));
        let stride = detect_batch_loop(
            &code,
            code.len(),
            41,
            55,
            &bypassable,
            &rotation,
            detect_bulk_set_byte_stride_loop,
        )
        .expect("the rotated `j += i` marking loop is a stride store");
        assert_eq!(stride.header_pc, 41);
        assert_eq!(
            (
                stride.array_local,
                stride.iv_local,
                stride.bound_local,
                stride.step_local
            ),
            (0, 4, 1, 3)
        );
        // The outer loop unrotates (its body's `ifne` and the inner loop's
        // `goto` / back edge are re-based). The inner loop is still rotated
        // in that copy; `unrotate_nested_loops` (wave 6) rewrites it in place
        // so the javac-shaped nest detector matches.
        let outer = unrotate_loop(&code, code.len(), 24, 63).expect("the outer loop unrotates");
        // COND (2 bytes) + negated test (3) = body base 5; the `ifne` at real
        // 27 (body offset 3) sits at synthetic 8 and targets real 58 (body
        // offset 34), synthetic 39: +31.
        assert_eq!(&outer.code[8..11], &[0x9a, 0x00, 0x1f]);
        // The inner `goto 52` at real 38 (offset 14 -> synthetic 19) targets
        // real 52 (offset 28 -> synthetic 33): +14, unchanged.
        assert_eq!(&outer.code[19..22], &[0xa7, 0x00, 0x0e]);
        // On the outer-only copy the nest detector does not match...
        assert!(
            detect_byte_sieve_loop(&outer.code, outer.code.len(), 0, outer.back_edge).is_none()
        );
        // ...and through the driver's rule (nested loops unrotated) it does,
        // anchored at the real outer header.
        let sieve = detect_batch_loop(
            &code,
            code.len(),
            24,
            63,
            &bypassable,
            &rotation,
            detect_byte_sieve_loop,
        )
        .expect("the rotated ecj sieve nest is matched");
        assert_eq!(sieve.header_pc, 24);
        assert_eq!(
            (
                sieve.array_local,
                sieve.outer_iv_local,
                sieve.bound_local,
                sieve.count_local,
                sieve.inner_iv_local
            ),
            (0, 3, 1, 2, 4),
            "a, i, limit, count, j"
        );
        // Unarmed (no rotation map): still refused.
        assert!(detect_batch_loop(
            &code,
            code.len(),
            24,
            63,
            &bypassable,
            &FxHashMap::default(),
            detect_byte_sieve_loop,
        )
        .is_none());
    }

    /// Round 9 wave 6 (lane `rotated6`): the nested rewrite of the ecj sieve
    /// produces exactly javac's nest spelling, pc for pc.
    #[test]
    fn the_nested_rotated_sieve_loop_unrotates_in_place() {
        let code = ecj_sieve();
        let mut outer = unrotate_loop(&code, code.len(), 24, 63).expect("the outer loop unrotates");
        let before_len = outer.code.len();
        unrotate_nested_loops(&mut outer);
        assert_eq!(
            outer.code.len(),
            before_len,
            "an in-place rewrite keeps the length"
        );
        assert_eq!((outer.header, outer.back_edge), (0, 42));
        assert_eq!(
            outer.code,
            vec![
                0x1d, 0x1b, 0xa3, 0x00, 0x2b, // 0: if (i > limit) goto 45
                0x2a, 0x1d, 0x33, 0x9a, 0x00, 0x1f, // 5: if (a[i]) goto 39
                0x84, 0x02, 0x01, // 11: count++
                0x1d, 0x1d, 0x60, 0x36, 0x04, // 14: j = i + i
                0x15, 0x04, 0x1b, 0xa3, 0x00, 0x11, // 19: if (j > limit) goto 39
                0x2a, 0x15, 0x04, 0x04, 0x54, // 25: a[j] = true
                0x15, 0x04, 0x1d, 0x60, 0x36, 0x04, // 30: j += i
                0xa7, 0xff, 0xef, // 36: goto 19
                0x84, 0x03, 0x01, // 39: i++
                0xa7, 0xff, 0xd6, // 42: goto 0
                0xb1, 0x00, 0x00, // 45: EXIT
            ]
        );
        // Idempotent: nothing rotated is left.
        let again = outer.code.clone();
        unrotate_nested_loops(&mut outer);
        assert_eq!(outer.code, again);
    }

    /// The in-place rewrite refuses a nested loop with a second entry (a
    /// branch from outside into its body) and a body branch into COND's
    /// middle.
    #[test]
    fn a_nested_loop_with_a_second_entry_is_not_unrotated() {
        let code = ecj_sieve();
        let outer = unrotate_loop(&code, code.len(), 24, 63).expect("the outer loop unrotates");
        // Inner loop in the copy: goto at 19, header 22, COND 33, back edge 36.
        assert!(unrotate_loop_in_place(&outer.code, 22, 36).is_some());
        // Retarget the outer `ifne` (at 8) from 39 to the inner body (22):
        // a second entry into the inner loop.
        let mut second_entry = outer.code.clone();
        second_entry[9..11].copy_from_slice(&(22i16 - 8).to_be_bytes());
        assert!(unrotate_loop_in_place(&second_entry, 22, 36).is_none());
        // Retarget it at the inner COND's second instruction (35): refused
        // too (the middle of COND moves).
        let mut into_cond = outer.code.clone();
        into_cond[9..11].copy_from_slice(&(35i16 - 8).to_be_bytes());
        assert!(unrotate_loop_in_place(&into_cond, 22, 36).is_none());
        // Retarget it at the inner entry `goto` (19): allowed, it becomes
        // COND's first instruction.
        let mut to_entry = outer.code.clone();
        to_entry[9..11].copy_from_slice(&(19i16 - 8).to_be_bytes());
        assert!(unrotate_loop_in_place(&to_entry, 22, 36).is_some());
        // Not a rotated loop: the header 19 is not preceded by a `goto`.
        assert!(unrotate_loop_in_place(&outer.code, 19, 36).is_none());
    }

    #[test]
    fn shapes_that_do_not_unrotate_are_refused() {
        // Top-tested loop (javac): no `goto` before the header.
        let javac_fill = [
            0x03, 0x3d, // 0: i = 0
            0x1c, 0x1b, 0xa3, 0x00, 0x0d, // 2: if (i > limit) goto 17
            0x2a, 0x1c, 0x03, 0x54, // 7: a[i] = 0
            0x84, 0x02, 0x01, 0xa7, 0xff, 0xf4, // 11: i++; goto 2
            0xb1, // 17
        ];
        assert!(unrotate_loop(&javac_fill, javac_fill.len(), 2, 14).is_none());

        // A body branch that leaves the loop anywhere but its fall-through
        // exit: `a[i] = 0` (4 bytes at 5) becomes `goto 18; nop`, and 17/18
        // are `nop; return`.
        let mut escaping = ecj_zero_fill();
        escaping.truncate(17);
        escaping.push(0x00); // 17: nop (the loop's fall-through exit)
        escaping.push(0xb1); // 18: return
        escaping[5..9].copy_from_slice(&[0xa7, 0x00, 0x0d, 0x00]);
        assert!(unrotate_loop(&escaping, escaping.len(), 5, 14).is_none());

        // A back edge that is not a conditional branch.
        let mut goto_back = ecj_zero_fill();
        goto_back[14] = 0xa7;
        assert!(unrotate_loop(&goto_back, goto_back.len(), 5, 14).is_none());
    }
}
