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
    let branch_targets = compute_branch_targets(code, code_len);

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
                pc += 1;
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
                pc += bytecode_len_at(code, pc);
            }
            // Plain (non-areturn) returns: ireturn/lreturn/freturn/dreturn/
            // return. A method-exit return ENDS the current path — it is NOT a
            // CFG-divergence point, so unlike the branch barrier above it must
            // NOT escape locals: an object held in a local here dies with the
            // frame, it does not escape. Any object that IS live into reachable
            // post-return code arrives there via a branch, and the
            // branch-target barrier at the top of the loop escapes it at that
            // target. Escaping locals here too (as the prior over-broad barrier
            // that lumped returns in with branches did) falsely de-optimises
            // the straight-line `new X(); use; return <primitive|void>` idiom —
            // contradicting this pass's "straight-line allocation sites are
            // unaffected" contract. Operand-stack objects (rare at a non-object
            // return — the return value is a primitive/void) are escaped
            // defensively; locals are left intact.
            0xac | 0xad | 0xae | 0xaf | 0xb1 => {
                escape_all!();
                abs_stack.clear();
                pc += bytecode_len_at(code, pc);
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
                pc += bytecode_len_at(code, pc);
            }
            // For all other opcodes, use bytecode_len_at for PC advance.
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
                let len = bytecode_len_at(code, pc);
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
        pc += bytecode_len_at(code, pc);
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
    let mut init_skips = std::collections::HashSet::new();
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
    let mut monitor_scalar_ops: std::collections::HashSet<usize> = std::collections::HashSet::new();

    // EC-SCALAR-SOUNDNESS: same branch-target barrier as `analyze_escapes`.
    // Objects in `objects` are already guaranteed (by the stricter
    // `analyze_escapes`) to live within a single straight-line region, so
    // clearing all provenance at every merge point can never strip a *valid*
    // field-op mapping — it only prevents a stale `local_prov`/`abs_stack`
    // entry from binding a post-branch field op to the wrong scalar object.
    let branch_targets = compute_branch_targets(code, code_len);

    let mut pc = 0usize;
    while pc < code_len {
        if branch_targets.get(pc).copied().unwrap_or(false) {
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
            // istore, lstore, fstore, dstore
            0x36..=0x39 => {
                abs_stack.pop();
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
            // istore_0..3, lstore_0..3, fstore_0..3, dstore_0..3
            0x3B..=0x4A => {
                abs_stack.pop();
                pc += 1;
            }
            // astore_0..3
            0x4B..=0x4E => {
                let idx = (op - 0x4B) as usize; // Widening: always safe
                let val = abs_stack.pop().flatten();
                local_prov[idx] = val;
                pc += 1;
            }
            // Xastore (iastore..sastore): pop value, index, arrayref
            0x4F..=0x56 => {
                abs_stack.pop();
                abs_stack.pop();
                abs_stack.pop();
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
                // Pop comparison operands
                match op {
                    0x99..=0x9E | 0xC6 | 0xC7 => {
                        abs_stack.pop();
                    }
                    _ => {
                        abs_stack.pop();
                        abs_stack.pop();
                    }
                }
                abs_stack.clear();
                for prov in local_prov.iter_mut() {
                    *prov = None;
                }
                pc += 3;
            }
            // goto — control transfer; same hard barrier as above.
            0xA7 => {
                abs_stack.clear();
                for prov in local_prov.iter_mut() {
                    *prov = None;
                }
                pc += 3;
            }
            // ireturn, lreturn, freturn, dreturn, areturn
            0xAC..=0xB0 => {
                abs_stack.pop();
                pc += 1;
            }
            // return (void)
            0xB1 => {
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
            // putfield
            0xB5 => {
                let _value = abs_stack.pop();
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
                                init_skips.insert(pc);
                            }
                        }
                    } else {
                        // Pop all args (including this)
                        for _ in 0..n {
                            abs_stack.pop();
                        }
                        if info.return_type != b'V' {
                            abs_stack.push(None);
                        }
                    }
                } else {
                    abs_stack.clear();
                    abs_stack.push(None);
                }
                pc += 3;
            }
            // invokevirtual, invokestatic
            0xB6 | 0xB8 => {
                abs_stack.clear();
                abs_stack.push(None);
                pc += 3;
            }
            // invokeinterface
            0xB9 => {
                abs_stack.clear();
                abs_stack.push(None);
                pc += 5;
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
                abs_stack.pop();
                abs_stack.clear();
                for prov in local_prov.iter_mut() {
                    *prov = None;
                }
                pc += 3;
            }
            // athrow — control transfer (to handler or caller); same barrier.
            0xBF => {
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
                    monitor_scalar_ops.insert(pc);
                }
                pc += 1;
            }
            0xC3 => {
                if let Some(np) = abs_stack.pop().flatten() {
                    if let Some(d) = mon_depth.get_mut(&np) {
                        *d = d.saturating_sub(1);
                    }
                    monitor_scalar_ops.insert(pc);
                }
                pc += 1;
            }
            // For anything else, conservatively clear all provenance
            _ => {
                abs_stack.clear();
                for prov in local_prov.iter_mut() {
                    *prov = None;
                }
                pc += bytecode_len_at(code, pc);
            }
        }
    }

    // Only keep objects that have at least one field op or init skip
    let used_objects: std::collections::HashSet<usize> = field_ops
        .values()
        .copied()
        .chain(init_skips.iter().copied().filter_map(|init_pc| {
            // Map init_pc to the new_pc (init is always at new_pc + 4)
            let new_pc = init_pc.wrapping_sub(4);
            if objects.contains_key(&new_pc) {
                Some(new_pc)
            } else {
                None
            }
        }))
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
    let final_init_skips: std::collections::HashSet<usize> = init_skips
        .into_iter()
        .filter(|init_pc| {
            let new_pc = init_pc.wrapping_sub(4);
            final_objects.contains_key(&new_pc)
        })
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
        monitor_scalar_ops,
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
/// Exception-handler edges are invisible here (the JIT is not handed the
/// exception table), so a handler landing inside a hoisted loop body without
/// passing the header would have the same defect. That is a pre-existing
/// limitation of the pre-header placement contract, not one this predicate
/// widens.
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
/// **This handler clause is currently unreachable and is pre-emptive.** A
/// method with a non-empty exception table is not admitted to this backend at
/// all, so `exception_ranges` is empty in every production compile today and
/// the codegen is byte-identical. Measured on dev `d134f7104` with
/// `CRATONVM_DBG_DUMP_JIT=LIST`: `LicmEntryProbe.shapeA/shapeB` (no handlers)
/// are listed as compiled; `TryCatchHot.f` (try/catch in a hot loop, 200 000
/// calls) and `HandlerLoopProbe.shape` (400 000 calls) are not. It is written
/// now because that admission gate has already been relaxed once (RBC.6
/// admitted explicit `athrow`) and the hazard is silent when it goes live — a
/// wrong loop bound, or a speculative-BCE guard elided but never run.
/// `docs/known-issues/repros/jit-licm-handler-edge/` holds the dormant
/// witness: an ASM generator for a shape javac cannot express (handler inside
/// the loop, protected range entirely before it, normal path falling *through*
/// into the header so no branch edge exists for the rule above to catch), plus
/// a driver that alternates both entries. Run it first if that gate is ever
/// relaxed.
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

    // (src_pc, target_pc) for every explicit branch edge. Decoding mirrors
    // `compute_branch_targets` (same opcode set, same switch padding rule) but
    // keeps the source PC so an edge can be classified as internal/external.
    let mut edges: Vec<(usize, usize)> = Vec::new();
    let mut opaque = false;
    let mut pc = 0usize;
    while pc < code_len {
        let op = code[pc];
        let mut push_edge = |src: usize, t: isize, edges: &mut Vec<(usize, usize)>| {
            // Cast: non-negative index into the code array
            if t >= 0 && (t as usize) < code_len {
                edges.push((src, t as usize)); // Cast: non-negative index to usize
            }
        };
        match op {
            // ret — the return address came from a `jsr` and lives in a local;
            // its successors are not statically known here.
            0xA9 => opaque = true,
            // Conditional branches + goto + jsr: 2-byte signed offset from `pc`.
            0x99..=0xA8 | 0xC6 | 0xC7 => {
                if op == 0xA8 {
                    opaque = true;
                }
                if pc + 2 < code_len {
                    // Cast: signed branch displacement to isize
                    let off = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as isize;
                    push_edge(pc, pc as isize + off, &mut edges); // Cast: pc to isize
                }
            }
            // goto_w / jsr_w: 4-byte signed offset from `pc`.
            0xC8 | 0xC9 => {
                if op == 0xC9 {
                    opaque = true;
                }
                if pc + 4 < code_len {
                    let off = i32::from_be_bytes([
                        code[pc + 1],
                        code[pc + 2],
                        code[pc + 3],
                        code[pc + 4],
                        // Cast: signed branch displacement to isize
                    ]) as isize;
                    push_edge(pc, pc as isize + off, &mut edges); // Cast: pc to isize
                }
            }
            // tableswitch: default + (high-low+1) offsets, all relative to `pc`.
            0xAA => {
                let mut p = pc + 1;
                while p % 4 != 0 {
                    p += 1;
                }
                if p + 12 > code_len {
                    opaque = true;
                    break;
                }
                let read_off = |at: usize| -> isize {
                    // Cast: signed branch displacement to isize
                    i32::from_be_bytes([code[at], code[at + 1], code[at + 2], code[at + 3]])
                        as isize
                };
                push_edge(pc, pc as isize + read_off(p), &mut edges); // default
                                                                      // Cast: table bound to i32
                let low = read_off(p + 4) as i32;
                // Cast: table bound to i32
                let high = read_off(p + 8) as i32;
                let count = checked_tableswitch_count(low, high).unwrap_or(0);
                let mut jp = p + 12;
                for _ in 0..count {
                    if jp + 4 > code_len {
                        opaque = true;
                        break;
                    }
                    push_edge(pc, pc as isize + read_off(jp), &mut edges); // Cast: pc to isize
                    jp += 4;
                }
            }
            // lookupswitch: default + npairs (match, offset) pairs.
            0xAB => {
                let mut p = pc + 1;
                while p % 4 != 0 {
                    p += 1;
                }
                if p + 8 > code_len {
                    opaque = true;
                    break;
                }
                let read_off = |at: usize| -> isize {
                    // Cast: signed branch displacement to isize
                    i32::from_be_bytes([code[at], code[at + 1], code[at + 2], code[at + 3]])
                        as isize
                };
                push_edge(pc, pc as isize + read_off(p), &mut edges); // default
                let npairs =
                    i32::from_be_bytes([code[p + 4], code[p + 5], code[p + 6], code[p + 7]]).max(0)
                        as usize; // Cast: non-negative count to usize
                let mut jp = p + 8;
                for _ in 0..npairs {
                    if jp + 8 > code_len {
                        opaque = true;
                        break;
                    }
                    // pair is (match:i32, offset:i32); the offset is at jp+4.
                    push_edge(pc, pc as isize + read_off(jp + 4), &mut edges); // Cast: pc to isize
                    jp += 8;
                }
            }
            _ => {}
        }
        let len = bytecode_len_at(code, pc);
        if len == 0 {
            opaque = true;
            break;
        }
        pc += len;
    }

    if opaque {
        for &(header, _) in loops {
            bypassable.insert(header);
        }
        return bypassable;
    }

    for &(header, back_edge) in loops {
        let loop_end = back_edge + bytecode_len_at(code, back_edge);
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

/// Detect natural loops by finding backward branches in bytecode.
/// Returns a list of `(header_pc, back_edge_pc)` pairs.
pub(super) fn detect_loops(code: &[u8], code_len: usize) -> Vec<(usize, usize)> {
    let mut loops = Vec::new();
    let mut pc = 0;
    while pc < code_len {
        match code[pc] {
            // goto — check for backward target
            0xa7 => {
                if pc + 2 < code_len {
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32; // Widening: always safe
                    let target = match pc.checked_add_signed(offset as isize) {
                        // Cast: address arithmetic
                        Some(t) if t < code_len => t,
                        _ => {
                            pc += 3;
                            continue;
                        } // invalid target — skip
                    };
                    if target <= pc {
                        loops.push((target, pc));
                    }
                }
                pc += 3;
            }
            // Conditional branches — check for backward target (do-while loops)
            0x99..=0xa6 | 0xc6 | 0xc7 => {
                if pc + 2 < code_len {
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32; // Widening: always safe
                    let target = match pc.checked_add_signed(offset as isize) {
                        // Cast: address arithmetic
                        Some(t) if t < code_len => t,
                        _ => {
                            pc += 3;
                            continue;
                        } // invalid target — skip
                    };
                    if target <= pc {
                        loops.push((target, pc));
                    }
                }
                pc += 3;
            }
            // Other instructions: advance by instruction length. Must use the
            // canonical table — an ad-hoc copy here was missing ldc/ldc_w/
            // ldc2_w (and the invoke/field/switch ops), so the walk stepped
            // into operand bytes and could fabricate or miss backward branches
            // (the CM-FASTMATH length-table desync family).
            _ => pc += bytecode_len_at(code, pc),
        }
    }
    loops
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
pub(super) fn detect_bulk_byte_loops(
    code: &[u8],
    code_len: usize,
    loops: &[(usize, usize)],
    bypassable_headers: &FxHashSet<usize>,
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
            .filter_map(|&(h, b)| detect_bulk_zero_byte_fill_loop(code, code_len, h, b))
            .filter(|f| !bypassable_headers.contains(&f.header_pc))
            .collect(),
        set_stride: loops
            .iter()
            .filter_map(|&(h, b)| detect_bulk_set_byte_stride_loop(code, code_len, h, b))
            .filter(|f| !bypassable_headers.contains(&f.header_pc))
            .collect(),
        sieve: loops
            .iter()
            .filter_map(|&(h, b)| detect_byte_sieve_loop(code, code_len, h, b))
            .filter(|s| !bypassable_headers.contains(&s.header_pc))
            .collect(),
    }
}

pub(super) fn bulk_byte_loops_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_BULK_BYTE_LOOPS")
            .map(|v| {
                let v = v.trim();
                !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
            })
            .unwrap_or(true)
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
            // lstore_0..lstore_3
            0x3f..=0x42 => {
                modified |= 1 << (code[pc] - 0x3f);
                pc += 1;
            }
            // fstore_0..fstore_3
            0x43..=0x46 => {
                modified |= 1 << (code[pc] - 0x43);
                pc += 1;
            }
            // dstore_0..dstore_3
            0x47..=0x4a => {
                modified |= 1 << (code[pc] - 0x47);
                pc += 1;
            }
            // astore_0..astore_3
            0x4b..=0x4e => {
                modified |= 1 << (code[pc] - 0x4b);
                pc += 1;
            }
            // istore/lstore/fstore/dstore/astore (wide index)
            0x36..=0x3a => {
                // Local index is 0..255; clamp the shift like every other
                // shift-by-local site (a high local saturates to bit 63, which
                // is conservatively treated as "some local >= 63 modified").
                // Widening: u8 -> wider int (bytecode operand byte, value fits)
                modified |= 1u64 << (code[pc + 1] as usize).min(63);
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
            // `docs/known-issues/jit/osr-miscompiles-cachecoherence-20260904.md`.
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
                pc += if widened == 0x84 { 6 } else { 4 };
            }
            // Other: advance by instruction length
            _ => pc += bytecode_len_at(code, pc),
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

    // Check array_local is not modified in this loop
    if array_local < 64 && modified & (1u64 << array_local) != 0 {
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

    // Check index_local is not modified in this loop
    if index_local < 64 && modified & (1u64 << index_local) != 0 {
        return None;
    }

    // Match aaload (0x32) — Object[] element access
    if next_pc2 >= code_len || code[next_pc2] != 0x32 {
        return None;
    }

    Some((array_local, index_local, next_pc2 + 1))
}

/// Find loop-invariant aaload sequences that can be hoisted out of loops.
/// For nested loops, hoists to the outermost loop where the sequence is invariant.
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

    for &(header, back_edge) in &sorted_loops {
        let loop_end = back_edge + bytecode_len_at(code, back_edge);
        if loop_end > code_len {
            continue;
        }

        // Conservative safety: skip if loop contains aastore (0x53) which could
        // invalidate a hoisted Object[] element by modifying the array contents.
        let mut has_aastore = false;
        let mut check_pc = header;
        while check_pc < loop_end {
            if code[check_pc] == 0x53 {
                has_aastore = true;
                break;
            }
            check_pc += bytecode_len_at(code, check_pc);
        }
        if has_aastore {
            continue;
        }

        let modified = find_modified_locals(code, header, loop_end);

        let mut pc = header;
        while pc < loop_end && pc < code_len {
            if hoisted_pcs.contains(&pc) {
                // Already hoisted by an outer loop
                pc += bytecode_len_at(code, pc);
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
                pc += bytecode_len_at(code, pc);
            }
        }
    }

    hoists
}
