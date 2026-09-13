// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The bytecode walk — `Compiler::compile_bytecode`.
//!
//! One method, ten thousand lines: the per-opcode dispatch that drives every
//! emitter in this backend. It was a quarter of `x64.rs` on its own, which is
//! why it is the first piece of the compiler proper to move (SEAM-01).
//!
//! Nothing here changed in the move. The body keeps its original indentation
//! because it lands inside an `impl Compiler` block at the same depth, so the
//! diff is a pure relocation and the emitted bytes are identical.

use super::*;

/// Which family module (`op_*.rs`) lowers an opcode. `compile_bytecode`'s
/// dispatch names one for every opcode it lowers.
#[derive(Clone, Copy)]
enum WalkFamily {
    LocalStack,
    Array,
    Arith,
    Control,
    Field,
    Invoke,
    Object,
}

/// What a family's `walk_*` method tells the walk loop.
pub(super) enum WalkStep {
    /// Continue the walk at this pc.
    Next(usize),
    /// Stop the walk: `compile_bytecode` returns this value.
    Return(bool),
}

/// The walk's source text: this file and the family modules its dispatch
/// routes to, for the tests that pin a lowering by its source.
#[cfg(test)]
pub(super) const WALK_SOURCES: &str = concat!(
    include_str!("bytecode_walk.rs"),
    include_str!("op_local_stack.rs"),
    include_str!("op_array.rs"),
    include_str!("op_arith.rs"),
    include_str!("op_control.rs"),
    include_str!("op_field.rs"),
    include_str!("op_invoke.rs"),
    include_str!("op_object.rs"),
);

/// How many compiles the single-pass backend refused because a field site had
/// no resolved layout. Read by the `jit-method-stats` report.
pub static UNRESOLVED_FIELD_SITE_BAILS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Refuse the compile: a `getfield`/`putfield`/`getstatic`/`putstatic` site has
/// no entry in the resolved field tables, so this backend has no slot index and
/// no type tag for it. Returns `false`, the walker's "stay interpreted" answer.
///
/// `CRATONVM_DBG_JIT_FIELD_SITES=<method-key substring>` — print every field
/// site this backend emits for a matching method: the pc, the slot index it
/// resolved to, and the type tag it will lower with.
///
/// A wrong-slot store leaves no trace in the generated code, which is why the
/// punned `SQLChar.rawData` cell could be caught at the helper door
/// (`jit_putfield_int(this, 1, Int(1))` on a slot declared `[C`) without the
/// SITE that emitted it ever being nameable. The runtime report cannot name it
/// — a raw JIT-to-JIT direct call sets no callee identity, and there is no
/// compiled return address for the stack walk to resolve. This says what the
/// COMPILER decided, per method and per pc, which is the same question asked
/// where the answer is still attributable.
///
/// Compare the output against `javap -c` on the same method: a line whose
/// `slot=` disagrees with the bytecode's field is the defect, and one whose
/// `tag=` disagrees is the punning half of it.
pub(super) fn dbg_field_sites() -> Option<&'static str> {
    static F: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    F.get_or_init(|| cratonvm_types::flags::runtime_var("CRATONVM_DBG_JIT_FIELD_SITES").ok())
        .as_deref()
}

/// Emit one `[jit-field-site]` line when the method matches the filter.
/// Emit one `[jit-emit-direct]` line per raw direct CALL this backend bakes.
///
/// The bind-time tracer (`CRATONVM_DBG_JIT_DIRECT_BINDS`) sees only the one
/// bind path it was added to. This sees the EMITTER, which is the thing that
/// actually puts an address in the code -- and the set it prints, compared
/// against `_direct_callee_entries`, is what says whether that address is
/// kept alive and invalidated with its callee.
pub(super) fn note_emit_direct(method_key: &str, pc: usize, entry: usize) {
    let Some(filter) = dbg_field_sites() else {
        return;
    };
    if !method_key.contains(filter) {
        return;
    }
    eprintln!("[jit-emit-direct] caller={method_key} pc={pc} entry={entry:#x}");
}

pub(super) fn note_field_site(method_key: &str, what: &str, pc: usize, slot: usize, tag: u8) {
    let Some(filter) = dbg_field_sites() else {
        return;
    };
    if !method_key.contains(filter) {
        return;
    }
    eprintln!(
        "[jit-field-site] {what} method={method_key} pc={pc} slot={slot} tag={}",
        tag as char
    );
}

/// # Why a refusal and not a default
///
/// These four sites each used to substitute `(pc, 0, b'I')` — **slot 0, tagged
/// `int`** — and carry on emitting. That is not a conservative default; it is a
/// write to a DIFFERENT field of the receiver, under a type the field does not
/// have, with nothing in the generated code or in any counter recording that a
/// substitution happened.
///
/// It has already been caught doing exactly that once. When the OSR compile
/// path shipped without populating `field_info`, every instance-field write in
/// an OSR-compiled method took this default: `HashtableOfInt.rehash()` (Eclipse
/// JDT BatchCompiler boot) stored its new `int[]` into slot 0 as
/// `Value::Int(low32_of_ptr)`, and the next `put()` read that back and died at
/// `arraylength` with "expected object reference, got int(N)". The fix then was
/// to populate the table for that one path — the default that turned a missing
/// entry into a corrupt heap cell was left in place for every other path.
///
/// It is the same shape as the punned `SQLChar.rawData` cell — a `[C` slot
/// holding `Int(1)`, dereferenced by a compiled `arraylength` as the pointer
/// `1` — and the reason that investigation could not name a writer is that a
/// substituted slot leaves no trace of having been substituted
/// (`known-issues/tomcat/punned-sqlchar-rawdata-cell-writer-localized-…`).
///
/// A missing entry means the VM-side resolver declined the site (the field's
/// class is not loadable at compile time, or the constant-pool entry is
/// malformed). Declining the METHOD costs one interpreted method and is always
/// correct; guessing a slot is never correct. The counter says how often it
/// happens, so "this refusal is expensive" stays a measurement rather than a
/// worry.
/// `CRATONVM_JIT_UNRESOLVED_FIELD_SUBSTITUTE=1` — restore the pre-fix
/// behaviour: substitute slot 0 tagged `int` for an unresolved field site and
/// carry on emitting, instead of refusing the compile.
///
/// # Why a switch for a behaviour nobody wants
///
/// The refusal is a correctness fix, and a correctness fix that UNBLOCKS a
/// workload has no A/B: the old binary cannot run the shape that the new one
/// fixed, so "it passes now" and "it passes today" are indistinguishable on a
/// workload whose base rate nobody measured. Comparing two BINARIES does not
/// close that — a cross-binary A/B varies everything that landed between them.
///
/// One binary and one variable does close it. Arming this restores exactly the
/// substitution and nothing else, so a workload that fails with it and passes
/// without it has been attributed, not merely observed to have stopped failing.
/// It is not a supported configuration and must never be set outside an arm.
pub(super) fn substitute_unresolved_field_sites() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_UNRESOLVED_FIELD_SUBSTITUTE")
    })
}

/// Snapshot of [`UNRESOLVED_FIELD_SITE_BAILS`], for the end-of-run report.
pub fn unresolved_field_site_bails() -> u64 {
    UNRESOLVED_FIELD_SITE_BAILS.load(std::sync::atomic::Ordering::Relaxed)
}

pub(super) fn unresolved_field_site(pc: usize, opcode: u8) -> bool {
    UNRESOLVED_FIELD_SITE_BAILS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    crate::note_jit_bail_site_at("unresolved-field-site", pc, opcode);
    false
}

/// How many `aastore` (0x53) sites this backend WALKED, whether it lowered them
/// inline or routed them to the `jit_aastore` helper.
///
/// This is the DENOMINATOR of the ZGC-barrier gate census below, and it exists
/// for one reason: the gate it counts is expected to fire **zero** times for the
/// life of every shipping process (nothing arms the barrier -- see
/// [`no_aastore_barrier_gate`]), and a zero on the gate counter alone cannot be
/// told apart from a gate wired somewhere it can never be reached. This tree has
/// already paid for that confusion once, with an instrument armed in a place no
/// value could arrive at, printing the same silence as a correctly-inert one.
///
/// Read the two together:
///   * `walked > 0, fallbacks == 0` -- the gate was consulted N times and
///     correctly declined every time. This is the expected steady state.
///   * `walked == 0` -- this instrument never ran. Either the workload compiled
///     no method containing an `aastore`, or the wiring is broken; a workload
///     that stores into an `Object[]` in compiled code and reports zero is the
///     second, and the counter has to be fixed before its zero means anything.
pub static AASTORE_SITES_WALKED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// How many `aastore` sites were routed to the `jit_aastore` helper because the
/// ZGC load barrier was ARMED at emission time.
///
/// Expected to be zero today; see [`AASTORE_SITES_WALKED`] for why that zero is
/// only readable next to its denominator.
pub static AASTORE_ZGC_GATE_FALLBACKS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// How many `aastore` sites were emitted INLINE even though the barrier was
/// armed, because `CRATONVM_JIT_NO_AASTORE_BARRIER_GATE` was set.
///
/// This is what makes the kill switch a real A/B rather than a claim: an arm run
/// with the switch set whose `suppressed` is zero never actually exercised the
/// pre-fix behaviour, so a pass on that arm attributes nothing.
pub static AASTORE_ZGC_GATE_SUPPRESSED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Snapshot of the `aastore` ZGC-barrier gate census, for the end-of-run report:
/// `(sites walked, helper fallbacks, kill-switch suppressions)`.
///
/// Not yet called by the report -- wiring it into `jit-method-stats` is a `vm/`
/// edit and is recorded in `.agent-requests/B6-wiring.txt`. Until then the same
/// three numbers are readable in-process with
/// `CRATONVM_DBG_AASTORE_BARRIER_GATE=1`, which prints one line per site.
pub fn aastore_barrier_gate_census() -> (u64, u64, u64) {
    let relaxed = std::sync::atomic::Ordering::Relaxed;
    (
        AASTORE_SITES_WALKED.load(relaxed),
        AASTORE_ZGC_GATE_FALLBACKS.load(relaxed),
        AASTORE_ZGC_GATE_SUPPRESSED.load(relaxed),
    )
}

/// `CRATONVM_JIT_NO_AASTORE_BARRIER_GATE=1` -- emit the `aastore` reference
/// load/store INLINE even while the ZGC read barrier is armed, i.e. restore the
/// behaviour this file had before the gate at the `0x53` arm was added.
///
/// # Why a switch for a behaviour nobody wants
///
/// Same argument as [`substitute_unresolved_field_sites`] above. The gate is a
/// coverage fix on a path nothing can reach today, so there is no workload on
/// which "it passes with the gate" and "it passes without it" differ, and a
/// cross-binary comparison would vary everything else that landed beside it.
/// One binary and one variable is the only honest A/B: arming this restores
/// exactly the inline emission and nothing else, and
/// [`AASTORE_ZGC_GATE_SUPPRESSED`] says whether the restoration actually
/// engaged. It is not a supported configuration once a cycle can arm the
/// barrier -- at that point setting it reinstates the hole described at the
/// `0x53` arm.
pub(super) fn no_aastore_barrier_gate() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_AASTORE_BARRIER_GATE")
    })
}

/// `CRATONVM_DBG_AASTORE_BARRIER_GATE=1` -- print one `[aastore-gate]` line per
/// `aastore` site this backend lowers, naming the pc, whether the ZGC read
/// barrier was armed at that moment, and which of the three verdicts the site
/// took.
///
/// The counters answer "how many"; this answers "which sites, and why", which is
/// the question a zero cannot be interrogated with. It is deliberately per-SITE
/// and not per-execution: the gate is an emission-time decision, so an execution
/// count would be measuring the workload rather than the compiler.
pub(super) fn dbg_aastore_barrier_gate() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_AASTORE_BARRIER_GATE"))
}

/// The int constant pushed by the instruction IMMEDIATELY before `pc`, if that
/// instruction is a constant push.
///
/// E27-1 N2b. The guarded inline `indexOf(I)` scan is only admissible for a
/// needle in `0..=0xFFFF`, because outside that range the JDK's rule is
/// `Character.isValidCodePoint` plus a surrogate-PAIR match and the inline
/// single-code-unit scan is simply a different function. The obvious way to
/// enforce that is a runtime screen with a deopt on the miss — and it is a
/// trap: the miss path invalidates the whole compiled method
/// (`DeoptimizationController::deoptimize`) and `ReceiverTypeChanged` is given
/// `RecompileAndReinterpret` on EVERY occurrence, then `MakeNotCompilable`. A
/// supplementary needle is a property of the DATA, so it recurs, and the
/// method would be recompiled per call and then barred from compilation.
///
/// So the screen is here, at COMPILE time, and its miss costs nothing: the
/// site simply is not intrinsified and takes the dispatch it takes today.
///
/// `ldc`/`ldc_w` are deliberately NOT decoded — they need the constant pool,
/// which this layer does not have. A `char` literal above `0x7FFF` therefore
/// falls back to dispatch. That is a missed optimisation, never a wrong
/// answer, and it is the rare shape: `indexOf(',')`, `indexOf('/')`,
/// `indexOf('=')` are `bipush`/`sipush`.
///
/// Scans from 0 rather than keeping a running "previous instruction" — the
/// callers are `invokevirtual` sites that already resolved to one specific
/// intrinsic, of which a method has a handful, and a linear scan of a
/// bytecode array is nothing next to the compile it is part of.
pub(super) fn prev_insn_int_const(code: &[u8], code_len: usize, pc: usize) -> Option<i32> {
    if pc == 0 || pc > code_len {
        return None;
    }
    let mut cur = 0usize;
    let mut prev: Option<usize> = None;
    while cur < pc {
        let next = cur + crate::bytecode_analysis::step(code, cur);
        if next > pc {
            // `pc` is not an instruction boundary on this linear decode
            // (a jump target inside a wide/tableswitch pad, say). Decline.
            return None;
        }
        prev = Some(cur);
        cur = next;
    }
    let at = prev?;
    match code.get(at)? {
        // iconst_m1 .. iconst_5
        op @ 0x02..=0x08 => Some(*op as i32 - 0x03),
        // bipush <i8>
        0x10 => code.get(at + 1).map(|b| *b as i8 as i32),
        // sipush <i16>
        0x11 => {
            let hi = *code.get(at + 1)? as i16;
            let lo = *code.get(at + 2)? as i16;
            Some((((hi << 8) | lo) as i16) as i32)
        }
        _ => None,
    }
}

impl Compiler {
    // -----------------------------------------------------------------------
    // Bytecode compilation
    // -----------------------------------------------------------------------

    // The intrinsic-dispatch ladders below compare `callee_entry` against the
    // deprecated `crate::MATH_*_INTRINSIC` aliases; new code should compare
    // against `JitIntrinsic::Foo.as_entry()` instead.
    #[allow(deprecated)]
    pub(super) fn compile_bytecode(&mut self, code: &[u8], code_len: usize) -> bool {
        // Pre-allocate pc_to_native mapping
        self.pc_to_native.resize(code_len + 1, -1);
        self.osr_entry_native.resize(code_len + 1, -1);

        // DCE: compute branch targets so we know which PCs are reachable
        let mut branch_targets = vec![false; code_len + 1];
        branch_targets[0] = true; // entry point
        {
            let mut p = 0;
            while p < code_len {
                match code[p] {
                    // Conditional and unconditional branches
                    0x99..=0xa6 | 0xa7 | 0xc6 | 0xc7 => {
                        if p + 2 < code_len {
                            let off = i16::from_be_bytes([code[p + 1], code[p + 2]]) as i32; // Widening: always safe
                            if let Some(target) = p.checked_add_signed(off as isize) {
                                // Cast: address arithmetic
                                if target < code_len {
                                    branch_targets[target] = true;
                                }
                            }
                        }
                    }
                    // tableswitch
                    0xaa => {
                        let base = p;
                        let mut q = p + 1;
                        while q % 4 != 0 {
                            q += 1;
                        }
                        if q + 12 <= code_len {
                            let def = i32::from_be_bytes([
                                code[q],
                                code[q + 1],
                                code[q + 2],
                                code[q + 3],
                            ]);
                            // Cast: signed offset to isize for pointer/index arithmetic
                            if let Some(t) = base.checked_add_signed(def as isize) {
                                // Cast: address arithmetic
                                if t < code_len {
                                    branch_targets[t] = true;
                                }
                            }
                            let low = i32::from_be_bytes([
                                code[q + 4],
                                code[q + 5],
                                code[q + 6],
                                code[q + 7],
                            ]);
                            let high = i32::from_be_bytes([
                                code[q + 8],
                                code[q + 9],
                                code[q + 10],
                                code[q + 11],
                            ]);
                            // Checked `high - low + 1`: raw i32 arithmetic
                            // overflows on attacker-controlled bounds. Such
                            // methods are rejected upstream by `jit_scan`; treat
                            // an unparseable count as zero offsets here.
                            let cnt = checked_tableswitch_count(low, high).unwrap_or(0);
                            q += 12;
                            for _ in 0..cnt {
                                if q + 4 <= code_len {
                                    let off = i32::from_be_bytes([
                                        code[q],
                                        code[q + 1],
                                        code[q + 2],
                                        code[q + 3],
                                    ]);
                                    // Cast: signed offset to isize for pointer/index arithmetic
                                    if let Some(t) = base.checked_add_signed(off as isize) {
                                        // Cast: address arithmetic
                                        if t < code_len {
                                            branch_targets[t] = true;
                                        }
                                    }
                                }
                                q += 4;
                            }
                        }
                    }
                    // lookupswitch
                    0xab => {
                        let base = p;
                        let mut q = p + 1;
                        while q % 4 != 0 {
                            q += 1;
                        }
                        if q + 8 <= code_len {
                            let def = i32::from_be_bytes([
                                code[q],
                                code[q + 1],
                                code[q + 2],
                                code[q + 3],
                            ]);
                            // Cast: signed offset to isize for pointer/index arithmetic
                            if let Some(t) = base.checked_add_signed(def as isize) {
                                // Cast: address arithmetic
                                if t < code_len {
                                    branch_targets[t] = true;
                                }
                            }
                            // A negative `npairs` cast straight to `usize` becomes
                            // ~1.8e19 and the loop below spins effectively forever
                            // (a hang/DoS on crafted bytecode). Clamp negatives to
                            // 0 and cap the iteration count to the pairs that
                            // actually fit before `code_len`, so a bogus huge
                            // `npairs` cannot drive an unbounded loop. The method
                            // is rejected later by the main lookupswitch handler.
                            let npairs_raw = i32::from_be_bytes([
                                code[q + 4],
                                code[q + 5],
                                code[q + 6],
                                code[q + 7],
                            ]);
                            // Cast: non-negative index/count to usize
                            let npairs = npairs_raw.max(0) as usize;
                            q += 8;
                            let max_pairs = (code_len - q) / 8;
                            for _ in 0..npairs.min(max_pairs) {
                                let off = i32::from_be_bytes([
                                    code[q + 4],
                                    code[q + 5],
                                    code[q + 6],
                                    code[q + 7],
                                ]);
                                // Cast: signed offset to isize for pointer/index arithmetic
                                if let Some(t) = base.checked_add_signed(off as isize) {
                                    // Cast: address arithmetic
                                    if t < code_len {
                                        branch_targets[t] = true;
                                    }
                                }
                                q += 8;
                            }
                        }
                    }
                    _ => {}
                }
                p += bytecode_analysis::step(code, p);
            }
        }
        // (getstatic cache removed — every getstatic calls the helper at
        // runtime for JMM thread-safety.)

        // The map above answers "does SOME instruction branch here", which is
        // not the same question as "can control reach here". The walk below
        // revives dead code at every branch target, so a target whose only
        // predecessors are themselves dead came back to life with no recorded
        // state — `expected_depth` fell back to 0 and the operand stack was
        // rebuilt empty.
        //
        // The population that hits it is EXCEPTION HANDLER BODIES. The backend
        // has no in-method handler dispatch, so a handler body is dead code in
        // the emitted image; but a branch INSIDE one (a ternary, an `if`, a
        // loop) still marks its own targets, and the walk cannot tell those
        // apart from a live join. `Rbc6FieldProbe.getfieldRefHandlerLocal` —
        // `catch (NPE e) { return scratch + (seen == null ? 0 : 1); }` —
        // revived at the ternary's merge with an empty stack, so the `iadd`
        // there underflowed and refused the whole method. The revived block was
        // also published as an OSR entry point, described by a stack model that
        // never applied to it.
        //
        // Compute real reachability and revive only there. Nothing live is
        // lost: the first PC of every reachable run is either PC 0 or a branch
        // target, which is exactly where a revival happens, and a fall-through
        // successor of a reachable instruction is reachable by construction. A
        // `None` result means opaque control flow (`jsr`/`ret`, malformed
        // encodings) — keep the historical behaviour there rather than guess.
        // ── Handler blocks become LIVE code when local handlers are armed ──
        //
        // The paragraph above is the pre-2026-08-20 world, in which "the
        // backend has no in-method handler dispatch, so a handler body is dead
        // code in the emitted image". With `local_handler_table` non-empty it
        // does have one, so each `handler_pc` is:
        //
        //   * a reachability ROOT — the exception edge is a real predecessor
        //     the bytecode's own branch decoding cannot see, and without it the
        //     block stays dead, `pc_to_native[handler_pc]` stays `-1`, and
        //     `patch_branches` would reject the whole method rather than
        //     silently mis-jump;
        //   * a branch TARGET whose incoming operand stack is the JVMS
        //     `[exception]` — depth one, and a reference. Seeded HERE, before
        //     the walk, so it wins the `or_insert` in
        //     `record_branch_target_depth` against anything a later branch to
        //     the same pc records.
        let local_handler_pcs: Vec<usize> = self
            .local_handler_table
            .iter()
            .map(|(_, _, handler_pc, _)| *handler_pc)
            .filter(|pc| *pc < code_len)
            .collect();
        for &handler_pc in &local_handler_pcs {
            branch_targets[handler_pc] = true;
            self.branch_target_stack_depth
                .entry(handler_pc)
                .or_insert(1);
            self.branch_target_stack_oop_marks
                .entry(handler_pc)
                .or_insert_with(|| vec![true]);
        }
        let reachable = bytecode_analysis::reachable_pcs(code, code_len, &local_handler_pcs);
        // Which pcs became live ONLY because of a handler root.
        //
        // Those must not be published as OSR entry points. An OSR entry is
        // taken at a back edge with the interpreter's frame seeded into the
        // compiled one, and the trampoline seeds LOCALS: a pc inside a `catch`
        // block can be standing on an operand stack the entry contract has no
        // way to describe. Before this feature such a pc was dead and got no
        // entry, so suppressing them keeps the published OSR entry set exactly
        // what it was — the feature buys handler THROUGHPUT and changes nothing
        // about which loops can be entered.
        //
        // Empty on every compile that arms no local handlers, and computed only
        // then: the second reachability pass is not worth paying for otherwise.
        let handler_only_pcs: Vec<bool> = if local_handler_pcs.is_empty() {
            Vec::new()
        } else {
            let normal = bytecode_analysis::reachable_pcs(code, code_len, &[]);
            match (&reachable, &normal) {
                (Some(all), Some(norm)) => all
                    .iter()
                    .zip(norm.iter())
                    .map(|(a, n)| *a && !*n)
                    .collect(),
                // Opaque control flow: neither map is trustworthy, so treat
                // every pc as handler-only and publish no OSR entries at all.
                // Strictly more conservative than before, and unreachable in
                // practice — `jsr`/`ret` never reaches this backend.
                _ => vec![true; code_len + 1],
            }
        };

        // Back-edge targets — the only bcis that get an OSR-exit map (see the
        // Step-7 emission site below for why "every pc" was wrong).
        //
        // Computed from `code` HERE rather than taken from the driver's own
        // `detect_loops` result on purpose: when the bytecode loop rewriter is
        // armed, `pc` in this walk is an OUTPUT pc and the driver's headers are
        // INPUT bcis. Deriving the set from the same slice the walk iterates
        // makes the coordinate spaces agree by construction instead of by
        // review. `detect_loops` is a linear scan, so this costs one extra pass
        // over the bytecode and only when OSR-exit metadata is being built at
        // all.
        let osr_exit_map_headers: FxHashSet<usize> = if crate::deopt_real_enabled() {
            super::escape_analysis::detect_loops(code, code_len)
                .into_iter()
                .map(|(header, _back_edge)| header)
                .collect()
        } else {
            FxHashSet::default()
        };

        let mut dead = false; // true after unconditional control transfer

        // Read once for the whole method: see `osr_empty_stack_entry_enabled`
        // for why this is not a `OnceLock` and not read per pc.
        let osr_empty_stack_rule = super::osr::osr_empty_stack_entry_enabled();

        let mut pc = 0;
        while pc < code_len {
            // Stage 2 (precise oop maps) — track the bytecode PC being emitted
            // so `emit_oop_map_for_safepoint` can look up the live oop-local
            // mask for this instruction without threading `pc` through every
            // safepoint call site.
            self.cur_bc_pc = pc;
            // Reload-elision mirror (see `slot_mirror`): a branch-target PC is
            // a control-flow join — a path jumping here did NOT execute the
            // instruction the mirror describes, so the register/slot pairing
            // must not survive across it. (This is the only zero-emitted-bytes
            // join the buffer-position rule cannot catch.)
            if branch_targets[pc] {
                self.slot_mirror = None;
            }
            // DCE: if we're in dead code and this PC isn't reachable, skip it
            if dead {
                let revive = match &reachable {
                    Some(r) => r[pc],
                    None => branch_targets[pc],
                };
                if revive {
                    // Reachable via a branch. At merge points after
                    // unconditional branches, reconstruct the simulated stack
                    // using canonical frame offsets: the predecessor path
                    // called `canonicalize_stack` before the goto, so values
                    // live at `base_spill + i*8`.
                    dead = false;
                    // A revived PC is reachable, so an emitted branch named
                    // it — and every branch-emitting arm records the operand
                    // stack live at its target through
                    // `record_branch_target_depth`. The one shape that could
                    // still land here unrecorded is a PC reached ONLY by a
                    // LATER (backward) branch, which has not been emitted yet;
                    // the historical `unwrap_or(0)` rebuilt an empty stack for
                    // it and compiled on, which is a silent wrong-code path,
                    // not a conservative one. Refuse instead — and say so.
                    let Some(&expected_depth) = self.branch_target_stack_depth.get(&pc) else {
                        self.fail("singlepass-codegen/revived-merge-depth-unrecorded");
                        return false;
                    };
                    if !self.set_spill_depth(expected_depth) {
                        return false;
                    }
                    self.stack.clear();
                    let base = self.base_spill_offset;
                    for i in 0..expected_depth {
                        let canonical_off = base + (i as i32) * 8; // Cast: x86-64 immediate encoding
                        self.stack.push(StackSlot::Frame(canonical_off));
                    }
                    // SECURITY FIX (V15): rebuild the parallel oop-mark
                    // vector in lock-step with the reconstructed stack.
                    // Previously only `self.stack` was rebuilt here, leaving
                    // `stack_oop_marks` holding stale type bits from the DEAD
                    // predecessor path. At the next safepoint,
                    // `emit_oop_map_for_safepoint` would index those stale
                    // bits against the freshly canonicalised frame slots and
                    // could emit an oop map that mislabels a slot (a stale
                    // `true` pins a non-reference word; a stale `false` would
                    // omit a real oop, which the conservative frame sweep
                    // still catches, but we must not rely on that here).
                    //
                    // FIX (testoutputbuffer-writespeed-content-length-mismatch
                    // follow-up): resetting every reconstructed slot to
                    // `false` was UNSOUND, not just conservative — the "the
                    // merge target's own bytecode will re-tag any slot that
                    // genuinely holds an oop as it re-executes the producing
                    // instruction" argument only holds for slots PRODUCED
                    // between the branch and this merge point. A slot pushed
                    // well BEFORE the branch (e.g. `getstatic` of a reference
                    // field, sitting under an `if`/`else` that only computes
                    // a LATER argument) is simply carried across untouched:
                    // nothing re-executes its producing instruction, so a
                    // blanket `false` here permanently mis-marked it as
                    // non-oop for the rest of the compiled method — silently
                    // defeating the OSR-exit/invokedynamic-uncommon-trap
                    // deopt snapshot's operand-stack decoding at any later
                    // safepoint that read the slot (confirmed via
                    // `getstatic System.out` immediately followed by an
                    // `if`/`else`-computed `makeConcatWithConstants` arg —
                    // see
                    // `testoutputbuffer-writespeed-content-length-mismatch-FIXED.md`).
                    // `record_branch_target_depth` now captures the REAL
                    // marks live at this target the first time it's seen
                    // (mirroring how `expected_depth` itself is captured);
                    // use them when present. Falls back to the historical
                    // all-`false` reconstruction only if no marks were ever
                    // recorded for this pc (shouldn't happen — every
                    // depth-recording call site records marks alongside —
                    // but degrades to the pre-existing, already-reviewed-safe
                    // behavior rather than panicking or guessing).
                    //
                    // ...AND SAY SO. The line below used to read
                    // `self.stack_oop_marks_exact = expected_depth == 0`, which
                    // is the answer for the all-`false` FALLBACK the paragraph
                    // above replaced: a reconstruction that guessed could not
                    // claim exactness at any nonzero depth. When the recorded
                    // marks are present AND their length matches, nothing was
                    // guessed -- they are the marks a predecessor actually had
                    // here -- and the flag was still reporting otherwise.
                    //
                    // It is not a cosmetic disagreement. `record_oop_map` seeds
                    // `map_incomplete` from this flag
                    // (`map_incomplete_cause::MARKS_INEXACT`), so every
                    // safepoint in the revived block loses its relocation claim
                    // and, through `fully_shadow_covered`, so does the whole
                    // method. MEASURED: on `RTreeRangeGc` and `RPriorityQueueGc`
                    // this was the ONLY remaining cause -- `checkMap` 6,
                    // `checkSet` 9, `singleThreaded` 3 -- and every failing pc
                    // is the arm or the merge of one `?:` feeding a string
                    // concat, the shape javac emits constantly.
                    //
                    // Sound for the same reason the recorded marks are usable at
                    // all: a merge point's predecessors must agree about which
                    // stack slots hold references, because JVMS 4.10.1 admits no
                    // merge of a reference with a primitive. Whichever
                    // predecessor `record_branch_target_depth` captured first
                    // therefore speaks for all of them. The fallback still fails
                    // closed -- it genuinely did guess.
                    let recorded_marks = self
                        .branch_target_stack_oop_marks
                        .get(&pc)
                        .filter(|marks| marks.len() == expected_depth)
                        .cloned();
                    // The kill switch gates only the CLAIM. The marks
                    // themselves are the earlier fix and are used either way,
                    // so `CRATONVM_JIT_MERGE_MARKS_EXACT=0` restores exactly
                    // the previous flag without reintroducing the mis-marking
                    // that fix was for.
                    self.stack_oop_marks_exact = expected_depth == 0
                        || (recorded_marks.is_some() && merge_marks_exact_enabled());
                    self.stack_oop_marks =
                        recorded_marks.unwrap_or_else(|| vec![false; expected_depth]);
                } else {
                    self.pc_to_native[pc] = -1;
                    pc += bytecode_analysis::step(code, pc);
                    continue;
                }
            }
            // At merge points (branch targets reachable from multiple paths),
            // canonicalize the current stack so all paths agree on frame layout.
            // The predecessor that did a `goto` already canonicalized; now the
            // fall-through path must match.
            if !dead && branch_targets[pc] {
                // A handler entry reached ALIVE by ordinary control flow would
                // have to agree with the exception edge about what is on the
                // stack, and the exception edge always says exactly one value:
                // the throwable. javac never emits such a block — a handler is
                // preceded by the `goto`/`return`/`athrow` that ends the
                // protected code, so the walk arrives dead and takes the
                // revival above. Refuse rather than canonicalise two
                // disagreeing pictures onto the same slots: the local-handler
                // stub would then store the throwable over a live value.
                if !local_handler_pcs.is_empty()
                    && local_handler_pcs.contains(&pc)
                    && self.stack.len() != 1
                {
                    self.fail("singlepass-codegen/local-handler-entry-live-fallthrough");
                    return false;
                }
                if let Some(&expected_depth) = self.branch_target_stack_depth.get(&pc) {
                    if expected_depth > 0 && self.stack.len() == expected_depth {
                        self.canonicalize_stack();
                    }
                }
            }
            // Reclaim spill slots at every instruction boundary. Each pre-call
            // flush (`flush_scratch_registers`) spills live operand-stack values
            // to fresh frame slots via `next_spill_offset += 8` and never rolls
            // that cursor back once the operands are consumed. Individual
            // stack-consuming handlers (invoke/switch/athrow/…) call
            // `reset_spills()` themselves, but a straight-line basic block full
            // of flush-bearing ops that DON'T (e.g. a long `putfield` run such
            // as `Token.copyTo`'s ~25 field stores, each emitting a
            // `jit_putfield_object` write-barrier CALL → flush) leaks one slot
            // per op. With `spill_size == max_stack*8` that cursor eventually
            // marches past the reserved spill region and into the callee-saved
            // GPR save area (`callee_saved_base`), overwriting the CALLER's
            // saved R12/R13. The epilogue then restores garbage into the
            // caller's callee-saved registers — e.g. HSQLDB's `Token.duplicate`
            // (which holds the freshly-`new`ed Token in a callee-saved local
            // across the `copyTo` call) returned null, breaking every embedded
            // in-memory database open after warm-up. `reset_spills` only lowers
            // the allocation cursor to just past the highest LIVE frame slot, so
            // it never disturbs a live value — it just recycles the dead scratch
            // slots the previous instruction left behind.
            if !dead {
                self.reset_spills();
            }
            // OSR soundness: record the pre-hoist native position as the OSR
            // entry for this PC. The LICM preheaders emitted just below
            // initialise hoist spill slots that the rewritten in-loop loads
            // depend on; a normal back-edge skips them (slots already warm)
            // but an OSR entry has cold slots, so it MUST run the preheader.
            // `pc_to_native[pc]` (set after the preheader) stays the back-edge
            // target; `osr_entry_native[pc]` points here, before the preheader.
            //
            // LICM-OSR soundness for nested loops: if this PC lives strictly
            // inside the body of a *hoisted* loop (header `H` with hoists, and
            // `H < pc < loop_end(H)`), an OSR entry that lands here would skip
            // `H`'s preheader and read uninitialised hoist spill slots — which
            // for an `aaload`-hoisted row pointer means a garbage pointer fed
            // straight into the in-loop array access (SIGSEGV) or, at best, a
            // silently wrong result. Mark such PCs as OSR-ineligible (-1) so
            // the runtime rejects the entry and the interpreter keeps running
            // until it next reaches `H` (or any PC outside every hoisted loop),
            // at which point OSR fires correctly with warm slots.
            //
            // We only test header PCs of hoists, not every loop — uncontained
            // loops (no LICM hoisting) are unaffected. The check is O(num_hoists),
            // and `num_hoists` is bounded by the static analysis upstream.
            //
            // Bytecode-loop-transform contract: `osr_entry_native` is published
            // as `CompiledMethod::osr_pc_to_native` and INDEXED BY INTERPRETER
            // BCI by the runtime's OSR entry check, but `pc` here is whatever
            // the emitter is walking — an interpreter bci on an ordinary
            // compile, an OUTPUT pc when the bytecode rewriter is armed. Keep
            // writing `self.buf.pos()` at the emitted pc: the coordinate change
            // is a single `LoopXform::rebuild_pc_to_native` at the end of
            // `compile_with_param_slots`, NOT a translation here. It has to be
            // there because the reverse of `bci_of` is one-to-many inside a
            // transformed region and this loop only ever sees one image at a
            // time; picking the wrong one is a wrong-code bug, not a missed
            // optimisation (entering a PEELED copy re-runs the peeled
            // iterations, so the loop executes `k` times too many).
            // `rebuild_pc_to_native` applies `osr_entry_pc`'s steady-state
            // choice pointwise. See `docs/jit/loop-rewriter-wiring.md`.
            if !dead && pc < self.osr_entry_native.len() {
                let inside_aaload_hoisted = self
                    .hoist_info
                    .iter()
                    .any(|h| h.loop_header < pc && pc < h.loop_end);
                let inside_arith_hoisted = self
                    .arith_hoist_info
                    .iter()
                    .any(|h| h.loop_header < pc && pc < h.loop_end);
                // The array-length hoist caches an int, not a pointer, so a
                // cold slot cannot be dereferenced -- but it can be BELIEVED.
                // The hoisted length is what a `bounds_safe_pcs` access was
                // proved safe against, so entering with a garbage length is an
                // unchecked out-of-bounds access, not merely a wrong answer.
                let inside_len_hoisted = self
                    .array_len_hoist_info
                    .iter()
                    .any(|h| h.loop_header < pc && pc < h.loop_end);
                // A versioned rewrite's pre-header guard is SYNTHETIC: its
                // bytes are an image of no original instruction, so there is no
                // bci for an entry there to be published under, and part-way
                // through the guard the abstract operand stack is not the
                // header's. That is not a new claim — `LoopXform::osr_entry_pc`
                // already answers the header's OSR entry with the fallback copy
                // for exactly this reason, after a versioned artifact entered at
                // the guard ran its loop with a null receiver
                // (`probes/LoopVersionOsrProbe.java`). What is new is that
                // eligibility ALSO gates the OSR-EXIT snapshot below, and since
                // the `deopt_real` refusal was retired that snapshot is live: an
                // exit map recorded on the guard would publish the header's bci
                // with the guard's frame under it.
                let inside_synthetic_guard = self
                    .synthetic_guard_span
                    .is_some_and(|(from, to)| pc >= from && pc < to);
                // See `handler_only_pcs`: a pc that is live only because a
                // `catch` block is now emitted keeps the OSR-entry answer it
                // had when that block was dead code.
                let handler_only = handler_only_pcs.get(pc).copied().unwrap_or(false);
                // The abstract operand stack must be EMPTY here.
                //
                // AUDIT 2026-09-04, and this one was wrong code, not a
                // missed optimisation. Entering part-way through an
                // expression means the entry prologue has to materialise
                // the pending operands -- and it does, correctly, for the
                // entering iteration. What it cannot do is make the LOOP
                // recompute them: the back edge targets the header, the
                // operand pushes live ABOVE the entry point, and every
                // later iteration replays the slots the prologue filled
                // once.
                //
                // `test_classes/jit/OsrStridedValue.java`, entering at the
                // `iastore` of `a[i] = i + r` with `[a, i, i+r]` pending:
                //
                //     --nojit   11 1035 2059 3083 4107 5131
                //     jit       11   11   11   11   11   11
                //
                // The addresses advance because the index is a local in a
                // register; the VALUE is frozen at the entering
                // iteration's `i` because it lives in an operand slot
                // written before the loop. Same shape, same reason, as the
                // synthetic-guard case just above -- "part-way through,
                // the abstract operand stack is not the header's" -- which
                // is why that one already refuses.
                //
                // Costs nothing in practice: javac gives every loop header
                // an empty expression stack, so the pcs this newly refuses
                // are mid-expression ones the interpreter reaches again a
                // few bytecodes later at the header.
                // `CRATONVM_JIT_NO_OSR_EMPTY_STACK_ENTRY=1` gives this rule
                // an off switch; see `osr::osr_empty_stack_entry_enabled` for
                // why a soundness rule gets one.
                let operand_stack_live = !self.stack.is_empty() && osr_empty_stack_rule;
                if operand_stack_live {
                    // The switch above changes no ANSWER — that is the page's
                    // own finding — so its engagement is invisible in output.
                    // Count it, and name it in the `[cratonvm-jitc]` stream the
                    // other OSR refusals already use, so "the rule fired" /
                    // "the switch turned it off" is one grep instead of an
                    // inference from a diff that is empty either way. See
                    // `super::osr::OSR_EMPTY_STACK_REFUSALS`.
                    let n = super::osr::OSR_EMPTY_STACK_REFUSALS
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        + 1;
                    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JITC") {
                        eprintln!(
                            "[cratonvm-jitc] osr-refuse (operand-stack-live) pc={pc} depth={} #{n}",
                            self.stack.len()
                        );
                    }
                }
                if inside_aaload_hoisted
                    || inside_arith_hoisted
                    || inside_len_hoisted
                    || inside_synthetic_guard
                    || handler_only
                    || operand_stack_live
                {
                    self.osr_entry_native[pc] = -1; // OSR rejected — fall back to interpreter
                } else {
                    self.osr_entry_native[pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                                                                       // Shadow-stack note: this position is reached by BOTH the OSR
                                                                       // entry (which bypasses the prologue) AND the initial fall-through
                                                                       // (slots already set by the prologue). So we can't (re)initialise
                                                                       // the slots here without corrupting the normal path. The OSR
                                                                       // trampoline initializes the cached thread/watermark slots before
                                                                       // jumping here.

                    // deopt-osr Step 7: emit an OSR-exit map capturing the
                    // loop-body interpreter state at this loop header.
                    // Emit-and-discard: Step 8 will resume the loop body at this
                    // bci on a mid-loop bail. Gated on `deopt_real_enabled()` so
                    // a deopt-off build carries no OSR-exit metadata and stays
                    // byte-identical.
                    //
                    // *** `osr_exit_map_headers` is the whole reason OSR works
                    // at all. *** This arm's own doc has always said "a
                    // loop-boundary bci", but the condition it sat under is
                    // `pc < osr_entry_native.len()` minus the LICM-hoisted
                    // interiors — i.e. essentially EVERY pc. So a map was
                    // recorded at every instruction boundary, including the ones
                    // with a non-empty operand stack mid-expression.
                    //
                    // That is fatal, because `CompiledMethod::osr_exit_policy`
                    // is an artifact-wide veto: ONE unresumable deopt point
                    // refuses OSR entry at EVERY pc of the method. And a
                    // mid-expression map is unresumable almost by construction —
                    // the operand stack has no per-entry width source, so in any
                    // method that touches a `long`/`float`/`double`
                    // (`uses_long_float_double`) every non-oop stack entry is
                    // recorded `FrameValue::Unsupported` rather than risk a
                    // truncated long on resume (see `build_and_record_deopt_point`).
                    //
                    // Net effect before this gate: every counted loop in every
                    // method with a `long` accumulator was refused
                    // `osr-entry-unresumable-exit`, naming the first bci with a
                    // non-empty stack — usually bci 1. A once-invoked method
                    // whose loop is hot then never left the interpreter: ~90
                    // ns/op against ~1.6 compiled, and identical under
                    // `--nojit`. `probes/StaticFieldProbe.java` and
                    // `probes/VirtOnlyProbe.java` were both measuring the
                    // interpreter because of it.
                    //
                    // A loop header is the only bci this metadata is ever
                    // consulted at: OSR entry happens at back-edge targets
                    // (`osr_entry_frame_state(entry_pc)`), and the sole reason-7
                    // stub emitter is the Step-8 test trigger, which picks
                    // `loops.iter().map(header).min()`. If Step 8 ever routes
                    // real bails at arbitrary bcis, this set has to grow to
                    // cover those sites — and the operand-stack width gap above
                    // has to be closed first, or the new points will veto the
                    // artifact exactly as these did.
                    if crate::deopt_real_enabled() && osr_exit_map_headers.contains(&pc) {
                        self.emit_osr_exit_map_at(pc);
                    }
                }
            }
            // === Speculative BCE: Emit range guards at loop headers ===
            // MUST run FIRST in the preheader — before the LICM hoists and the
            // SIMD batch preheaders below — so a failing guard deopts before
            // any speculative code (in particular an AVX2 element-wise batch
            // whose per-element checks were elided on the strength of these
            // guards) touches the heap. Emitted per header:
            //   load iv -> EAX; TEST EAX,EAX; JS deopt        (iv >= 0, once)
            // then for each guarded array:
            //   load array ref -> RAX
            //   MOV R10D, [RAX + ARRAY_LENGTH_OFFSET]  (array length)
            //   load loop bound -> ECX
            //   CMP R10D, ECX  (array.length vs loop_bound)
            //   JB deopt_stub  (if array.length < loop_bound, deopt)
            {
                // O(1) lookup of this header's guards (indexed once in `compile`)
                // instead of re-scanning the whole guard vector per loop header.
                let guards: Vec<SpeculativeBCEGuard> = self
                    .speculative_bce_guards_by_header
                    .get(&pc)
                    .cloned()
                    .unwrap_or_default();
                let had_guards = !guards.is_empty();
                if let Some(first) = guards.first() {
                    // iv >= 0 at entry: combined with the step guards below
                    // (unit +1, or a proven `0 <= step <= MAX - bound`
                    // variable stride) this bounds every elided index from
                    // below. All guards at one header share the loop's IV,
                    // bound and step, so test them once.
                    if let Some(reg) = self.reg_for_local(first.iv_local) {
                        self.emit_mov_reg_reg(RAX, reg);
                    } else {
                        self.emit_load_local(RAX, self.local_offset(first.iv_local));
                    }
                    // TEST EAX, EAX (85 C0); JS rel32 (0F 88) — negative iv deopts
                    self.buf.emit(&[0x85, 0xC0]);
                    self.buf.emit(&[0x0F, 0x88]);
                    let patch_offset = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    self.deopt_stubs.push((patch_offset, pc, 2)); // 2 = DEOPT_REASON_BOUNDS_CHECK
                    if first.inclusive || first.step_local.is_some() {
                        // Load the loop bound into ECX for the entry guards.
                        if let Some(reg) = self.reg_for_local(first.bound_local) {
                            self.emit_mov_reg_reg(RCX, reg);
                        } else {
                            self.emit_load_local(RCX, self.local_offset(first.bound_local));
                        }
                    }
                    if first.inclusive {
                        // Inclusive wrap hazard: at `iv == bound ==
                        // Integer.MAX_VALUE` the post-body increment wraps
                        // negative while `iv <= bound` keeps passing; the
                        // interpreter then throws AIOOBE on the wrapped index,
                        // so elided code must deopt up front.
                        // CMP ECX, imm32 (81 F9 id); JE rel32 (0F 84).
                        self.buf.emit(&[0x81, 0xF9]);
                        self.buf.emit(&0x7FFF_FFFFi32.to_le_bytes());
                        self.buf.emit(&[0x0F, 0x84]);
                        let p = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                        self.deopt_stubs.push((p, pc, 2)); // 2 = DEOPT_REASON_BOUNDS_CHECK
                    }
                    if let Some(step_local) = first.step_local {
                        // Variable-stride guards: `step >= 0` (a negative
                        // step walks the elided index below zero) and
                        // `step <= Integer.MAX_VALUE - bound` (no int wrap
                        // past the exit test: every reached index satisfies
                        // `iv <= bound` pre-step, so `iv + step` stays
                        // representable and the NEXT exit test is honest).
                        if let Some(reg) = self.reg_for_local(step_local) {
                            self.emit_mov_reg_reg(RDX, reg);
                        } else {
                            self.emit_load_local(RDX, self.local_offset(step_local));
                        }
                        // TEST EDX, EDX (85 D2); JS rel32 (0F 88).
                        self.buf.emit(&[0x85, 0xD2]);
                        self.buf.emit(&[0x0F, 0x88]);
                        let p = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                        self.deopt_stubs.push((p, pc, 2)); // 2 = DEOPT_REASON_BOUNDS_CHECK
                                                           // MOV R11D, INT_MAX (41 BB id); SUB R11D, ECX (41 29 CB);
                                                           // CMP EDX, R11D (44 39 DA); JG rel32 (0F 8F).
                        self.buf.emit(&[0x41, 0xBB]);
                        self.buf.emit(&0x7FFF_FFFFi32.to_le_bytes());
                        self.buf.emit(&[0x41, 0x29, 0xCB]);
                        self.buf.emit(&[0x44, 0x39, 0xDA]);
                        self.buf.emit(&[0x0F, 0x8F]);
                        let p = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                        self.deopt_stubs.push((p, pc, 2)); // 2 = DEOPT_REASON_BOUNDS_CHECK
                    }
                }
                for guard in guards {
                    // Load array reference into RAX
                    if let Some(reg) = self.reg_for_local(guard.array_local) {
                        self.emit_mov_reg_reg(RAX, reg);
                    } else {
                        self.emit_load_local(RAX, self.local_offset(guard.array_local));
                    }
                    // Null guard: TEST RAX, RAX (48 85 C0); JZ deopt (0F 84) — the header
                    // guard runs UNCONDITIONALLY, even when the loop is zero-trip
                    // (`bound == 0`), where the original bytecode never dereferences
                    // the array at all. A null array with bound 0 is a perfectly
                    // legal program state (freemarker's
                    // `TemplateElement.setChildren` receives `buffer == null,
                    // count == 0` for childless elements and SIGSEGV'd here on the
                    // raw length load — reactor `boundedElastic` render thread,
                    // FreeMarkerMacroTests/FreeMarkerViewTests ABEND). Route null
                    // to the same reason-2 deopt stub: the interpreter re-runs the
                    // loop with real per-access semantics (returning normally for
                    // zero-trip, throwing NPE only if an access is actually
                    // reached). Mirrors the LICM hoist null guard below.
                    self.buf.emit(&[0x48, 0x85, 0xC0]);
                    self.buf.emit(&[0x0F, 0x84]);
                    let null_patch = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    self.deopt_stubs.push((null_patch, pc, 2)); // 2 = DEOPT_REASON_BOUNDS_CHECK
                                                                // MOV R10D, DWORD [RAX + ARRAY_LENGTH_OFFSET] — array length
                    self.buf
                        .emit(&[0x44, 0x8B, 0x50, ARRAY_LENGTH_OFFSET as u8]); // Cast: x86-64 register encoding
                                                                               // Load loop bound into ECX
                    if let Some(reg) = self.reg_for_local(guard.bound_local) {
                        self.emit_mov_reg_reg(RCX, reg);
                    } else {
                        self.emit_load_local(RCX, self.local_offset(guard.bound_local));
                    }
                    // CMP R10D, ECX — compare array.length vs loop_bound
                    // Encoding: 44 3B D1 (REX.R + CMP r32, r/m32 + ModRM(11, R10, ECX))
                    self.buf.emit(&[0x44, 0x3B, 0xD1]);
                    // Exclusive: JB — deopt if length < bound. Inclusive: JBE —
                    // the loop reaches `iv == bound`, so the guard must prove
                    // length > bound (SECURITY FIX V17, sound-guard form). A
                    // runtime-negative bound reads as huge unsigned and deopts
                    // conservatively (zero-trip loops re-run interpreted).
                    self.buf.emit(if guard.inclusive {
                        &[0x0F, 0x86] // JBE rel32
                    } else {
                        &[0x0F, 0x82] // JB rel32
                    });
                    let patch_offset = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    // Route to deopt stub (calls jit_uncommon_trap) instead of AIOOBE
                    self.deopt_stubs.push((patch_offset, pc, 2)); // 2 = DEOPT_REASON_BOUNDS_CHECK
                }
                // deopt-osr Step 1: record a precise deopt snapshot for this
                // BCE loop-header guard (emit-and-discard; nothing reads it yet,
                // so the i64::MIN re-run via deopt_stubs above is unchanged).
                if had_guards {
                    self.emit_deopt_snapshot_at_guard(pc);
                }
            }

            // === LICM: Emit hoisted aaload code at loop headers ===
            // Hoisted code runs BEFORE pc_to_native is set, so back-edges
            // (which use pc_to_native[header]) skip the hoisted computation.
            // On initial loop entry (fall-through from preheader), the hoisted
            // code executes and caches the invariant value in a spill slot.
            //
            // SOUNDNESS: the preheader executes UNCONDITIONALLY — even when
            // the loop is zero-trip, and even when the in-body access the
            // sequence was hoisted from is behind a conditional the program
            // would skip (`for(..){ if (m != null) use(m[j][i]); }` with
            // m == null). The original program may therefore never perform
            // this load at all, so it must not fault and must not throw
            // here. Each hoisted load is preceded by a null + unsigned
            // bounds guard routed to the reason-2 deopt stub: on failure the
            // interpreter re-runs the loop with real per-access semantics
            // (throwing NPE/AIOOBE only if the access is actually reached).
            // Repeated guard failures at this header cross the per-bci
            // de-spec threshold and the recompile drops the hoist entirely
            // (see the `DespecRegistry::contains` filter on `hoist_info` in
            // `compile_with_param_slots`). Before these guards the hoist was
            // a raw MOV — a null/OOB row index crashed the VM or fed a
            // garbage row pointer to the loop body (test_classes/
            // LicmHoistRepro.java).
            {
                // Collect hoist data to avoid borrow conflicts with self
                let loop_hoists: Vec<(usize, usize, i32)> = self
                    .hoist_info
                    .iter()
                    .enumerate()
                    .filter(|(_, h)| h.loop_header == pc)
                    .map(|(idx, h)| (h.array_local, h.index_local, self.hoist_offsets[idx]))
                    .collect();
                let had_hoists = !loop_hoists.is_empty();

                for (array_local, index_local, hoist_offset) in loop_hoists {
                    // Load array reference into RAX
                    if let Some(reg) = self.reg_for_local(array_local) {
                        self.emit_mov_reg_reg(RAX, reg);
                    } else {
                        self.emit_load_local(RAX, self.local_offset(array_local));
                    }
                    // Load index into RCX
                    if let Some(reg) = self.reg_for_local(index_local) {
                        self.emit_mov_reg_reg(RCX, reg);
                    } else {
                        self.emit_load_local(RCX, self.local_offset(index_local));
                    }
                    // Null guard: TEST RAX, RAX (48 85 C0); JZ deopt (0F 84).
                    self.buf.emit(&[0x48, 0x85, 0xC0]);
                    self.buf.emit(&[0x0F, 0x84]);
                    let null_patch = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    self.deopt_stubs.push((null_patch, pc, 2)); // 2 = DEOPT_REASON_BOUNDS_CHECK
                                                                // Bounds guard: MOV R10D, [RAX + ARRAY_LENGTH_OFFSET];
                                                                // CMP ECX, R10D; JAE deopt — unsigned, so a negative
                                                                // index is caught as huge (same as emit_bounds_check).
                    self.buf
                        .emit(&[0x44, 0x8B, 0x50, ARRAY_LENGTH_OFFSET as u8]); // Cast: x86-64 register encoding
                    self.buf.emit(&[0x41, 0x3B, 0xCA]);
                    self.buf.emit(&[0x0F, 0x83]);
                    let bounds_patch = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    self.deopt_stubs.push((bounds_patch, pc, 2)); // 2 = DEOPT_REASON_BOUNDS_CHECK
                                                                  // Inline aaload: MOV RAX, [RAX + RCX*8 + HEADER_SIZE]
                    self.emit_ref_aload_regs();
                    // Store hoisted value in dedicated spill slot
                    self.emit_store_local(hoist_offset, RAX);
                }
                // Deopt snapshot for the hoist guards at this header (skip if
                // the speculative-BCE guard block above already recorded one).
                if had_hoists && !self.deopt_box_ptr_by_bci.contains_key(&pc) {
                    self.emit_deopt_snapshot_at_guard(pc);
                }
            }

            // === LICM: Emit hoisted integer-arithmetic at loop headers ===
            // Same placement contract as the aaload hoist: emitted BEFORE
            // pc_to_native[pc] is set, so the back-edge skips it. The pure,
            // non-faulting expression is computed once and its result cached
            // in a dedicated frame slot; in-loop occurrences become a load.
            {
                let arith_hoists: Vec<(Vec<ArithStep>, i32)> = self
                    .arith_hoist_info
                    .iter()
                    .enumerate()
                    .filter(|(_, h)| h.loop_header == pc)
                    .map(|(idx, h)| (h.steps.clone(), self.arith_hoist_offsets[idx]))
                    .collect();

                for (steps, result_offset) in arith_hoists {
                    self.emit_arith_hoist_into_rax(&steps);
                    self.emit_store_local(result_offset, RAX);
                }
            }

            // === LICM: Emit hoisted `arraylength` at loop headers ===
            // Same placement contract as the two hoists above: emitted BEFORE
            // `pc_to_native[pc]` is set, so the back edge skips it, while
            // `osr_entry_native[pc]` (set above) points here, so a cold OSR
            // entry at the header runs it.
            //
            // SOUNDNESS: the pre-header executes UNCONDITIONALLY, including
            // when the loop is zero-trip. `find_array_len_hoists` therefore
            // only offers sites in the HEADER'S STRAIGHT-LINE PREFIX -- reached
            // through nothing but local/constant pushes -- so the original
            // program was always going to evaluate this `arraylength` at this
            // exact moment anyway. That is why the null case here THROWS
            // (`npe_action::ARRAY_LENGTH`, the same JEP-358 action and the same
            // shared stub the in-loop `arraylength` would have used) rather
            // than deopting the way the aaload hoist's unconstrained sites must.
            // It also keeps the emitted bytes address-independent: a deopt
            // snapshot bakes a `Box` pointer, and two compiles of one method
            // would stop producing identical code.
            //
            // The cached value is an int, so unlike the aaload hoist's row
            // pointer it is not a GC root, needs no oop map, and is not
            // invalidated by relocation: an array's length is immutable and
            // no bytecode can write it.
            {
                // The third element is the bci this hoisted check reports when
                // it traps. The check no longer stands where the programmer
                // wrote it, so it has to name a site explicitly: the FIRST
                // `arraylength` of the hoist (`seq_end - 1`, one before the
                // one-past-the-end recorded by `ArrayLenHoist::sites`), which
                // is the site that would have trapped first had nothing moved.
                // A hoist with no sites cannot happen -- LICM builds the record
                // from them -- but falling back to the loop header keeps a bci
                // in this method rather than none at all.
                let len_hoists: Vec<(usize, i32, usize)> = self
                    .array_len_hoist_info
                    .iter()
                    .enumerate()
                    .filter(|(_, h)| h.loop_header == pc)
                    .map(|(idx, h)| {
                        (
                            h.array_local,
                            self.array_len_hoist_offsets[idx],
                            h.sites
                                .first()
                                .map_or(h.loop_header, |&(_, seq_end)| seq_end - 1),
                        )
                    })
                    .collect();
                for (array_local, hoist_offset, trap_bci) in len_hoists {
                    // Array reference into RAX.
                    if let Some(reg) = self.reg_for_local(array_local) {
                        self.emit_mov_reg_reg(RAX, reg);
                    } else {
                        self.emit_load_local(RAX, self.local_offset(array_local));
                    }
                    // The in-loop null check, moved here whole: same TEST/JZ,
                    // same shared stub, same "Cannot read the array length"
                    // action. It is elided outright when the dataflow already
                    // proves the receiver non-null at the header.
                    if !self.is_local_nonnull(pc, array_local) {
                        let key = crate::x64::inlining::record_npe_trap_site(trap_bci);
                        self.emit_null_check_array_load(npe_action::ARRAY_LENGTH, key);
                    }
                    // MOV EAX, [RAX + array length offset] -- zero-extends into
                    // RAX, and a length is non-negative, so the 64-bit slot
                    // below holds the same number either way.
                    self.emit_arraylength_regs();
                    self.emit_store_local(hoist_offset, RAX);
                }
            }

            // === SIMD: Emit vectorized preheader for int-array-sum loops ===
            // Runs once on initial loop entry; back-edges skip to scalar loop.
            {
                let simd_match = self.simd_loops.iter().find(|s| s.header_pc == pc).map(|s| {
                    (
                        s.iv_local,
                        s.acc_local,
                        s.array_local,
                        s.bound_local,
                        s.acc_is_long,
                    )
                });

                if let Some((iv_local, acc_local, array_local, bound_local, acc_is_long)) =
                    simd_match
                {
                    let acc_offset = self.local_offset(acc_local);

                    // Sync accumulator register → frame slot (SIMD code operates on frame)
                    if let Some(acc_reg) = self.reg_for_local(acc_local) {
                        self.emit_store_local(acc_offset, acc_reg);
                    }

                    // Load array reference into RCX
                    if let Some(reg) = self.reg_for_local(array_local) {
                        self.emit_mov_reg_reg(RCX, reg);
                    } else {
                        self.emit_load_local(RCX, self.local_offset(array_local));
                    }
                    // Load induction variable into R10D
                    if let Some(reg) = self.reg_for_local(iv_local) {
                        self.emit_mov_reg_reg(R10, reg);
                    } else {
                        self.emit_load_local(R10, self.local_offset(iv_local));
                    }
                    // Load bound into R11D
                    if let Some(reg) = self.reg_for_local(bound_local) {
                        self.emit_mov_reg_reg(R11, reg);
                    } else {
                        self.emit_load_local(R11, self.local_offset(bound_local));
                    }

                    // Emit SIMD int-array sum (operates on frame slot for accumulator)
                    self.emit_simd_int_array_sum(acc_offset, acc_is_long);

                    // Sync accumulator frame slot → register
                    if let Some(acc_reg) = self.reg_for_local(acc_local) {
                        self.emit_load_local(acc_reg, acc_offset);
                    }

                    // Update induction variable from R10D
                    if let Some(reg) = self.reg_for_local(iv_local) {
                        self.emit_mov_reg_reg(reg, R10);
                    } else {
                        self.emit_store_local(self.local_offset(iv_local), R10);
                    }
                }
            }

            if let Some(sieve) = self
                .byte_sieve_loops
                .iter()
                .find(|sieve| sieve.header_pc == pc)
                .cloned()
            {
                self.emit_byte_sieve_preheader(&sieve);
            }

            if let Some(fill) = self
                .bulk_zero_byte_fill_loops
                .iter()
                .find(|fill| fill.header_pc == pc)
                .cloned()
            {
                self.emit_bulk_zero_byte_fill_preheader(&fill);
            }

            if let Some(fill) = self
                .bulk_set_byte_stride_loops
                .iter()
                .find(|fill| fill.header_pc == pc)
                .cloned()
            {
                self.emit_bulk_set_byte_stride_preheader(&fill);
            }

            // === T17.Β.2 — SIMD element-wise preheader ====================
            // Emit an AVX2 batch loop + scalar tail for loops matching
            // `OUT[i] = A[i] OP B[i]`. Gated on AVX2 availability; when
            // absent, the original scalar loop body fires as-is (no
            // emission, no crash).
            if has_avx2() {
                let ewise_match = self
                    .simd_element_wise_loops
                    .iter()
                    .find(|e| e.header_pc == pc)
                    .map(|e| {
                        (
                            e.iv_local,
                            e.out_local,
                            e.a_local,
                            e.b_local,
                            e.bound_local,
                            e.op,
                        )
                    });

                if let Some((iv_local, out_local, a_local, b_local, bound_local, op)) = ewise_match
                {
                    // Load A base → RAX
                    if let Some(reg) = self.reg_for_local(a_local) {
                        self.emit_mov_reg_reg(RAX, reg);
                    } else {
                        self.emit_load_local(RAX, self.local_offset(a_local));
                    }
                    // Load B base → RCX
                    if let Some(reg) = self.reg_for_local(b_local) {
                        self.emit_mov_reg_reg(RCX, reg);
                    } else {
                        self.emit_load_local(RCX, self.local_offset(b_local));
                    }
                    // Load OUT base → RDX
                    if let Some(reg) = self.reg_for_local(out_local) {
                        self.emit_mov_reg_reg(RDX, reg);
                    } else {
                        self.emit_load_local(RDX, self.local_offset(out_local));
                    }
                    // Load i → R10D
                    if let Some(reg) = self.reg_for_local(iv_local) {
                        self.emit_mov_reg_reg(R10, reg);
                    } else {
                        self.emit_load_local(R10, self.local_offset(iv_local));
                    }
                    // Load n → R11D
                    if let Some(reg) = self.reg_for_local(bound_local) {
                        self.emit_mov_reg_reg(R11, reg);
                    } else {
                        self.emit_load_local(R11, self.local_offset(bound_local));
                    }

                    self.emit_simd_int_array_element_wise(op);

                    // Update induction variable from R10D.
                    if let Some(reg) = self.reg_for_local(iv_local) {
                        self.emit_mov_reg_reg(reg, R10);
                    } else {
                        self.emit_store_local(self.local_offset(iv_local), R10);
                    }
                }
            }

            // Guarded `int[][]` dot-product replacement.  Like the SIMD
            // preheaders above, normal back-edges target `pc_to_native[pc]`
            // below and therefore skip this fall-through-only fast path.
            if let Some(dot) = self
                .matrix_dot_loops
                .iter()
                .find(|dot| dot.header_pc == pc)
                .cloned()
            {
                self.emit_matrix_dot_preheader(&dot);
            }

            // (The speculative-BCE range guards are emitted at the TOP of this
            // preheader — before the LICM hoists and SIMD batch preheaders —
            // so a failing guard deopts before any speculative code runs.)

            // Record mapping from bytecode PC to native offset
            // (AFTER speculative-BCE/hoisted/SIMD code, so back-edges skip the preheader)
            self.pc_to_native[pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding

            // Reload-elision mirror (see the `slot_mirror` field doc) — the
            // REAL join point. `pc_to_native[pc]` is exactly where every
            // branch to this PC lands, so anything emitted for this PC BEFORE
            // this line is fall-through-only code: the merge-point
            // `canonicalize_stack()` above, the LICM/SIMD preheaders, the
            // speculative-BCE guards. A mirror recorded by that code is valid
            // only on the fall-through edge, so it must not survive the join.
            //
            // BUG-JOIN-MIRROR-20260726: clearing the mirror at the TOP of the
            // iteration (a few hundred lines above) was not enough precisely
            // because `canonicalize_stack()` runs after it and records a fresh
            // one. `String getProperty(String key, String def) { String s =
            // getProperty(key); return s == null ? def : s; }` compiled to a
            // fall-through arm that canonicalized `s` from its callee-saved
            // home via `MOV RAX,R12 ; MOV [rbp-0x40],RAX` — leaving the mirror
            // `[rbp-0x40] == RAX` live — and an `areturn` whose reload was then
            // elided down to nothing. The `goto` arm had stored `def` straight
            // from ITS home (`MOV [rbp-0x40],R13`, no RAX), so taking that edge
            // returned the stale RAX: the null `s` instead of the default.
            // H2 opened every database with `ACCESS_MODE_DATA` null.
            if branch_targets[pc] {
                self.slot_mirror = None;
            }

            // deopt-osr Step 8 (test trigger): at the chosen loop header, emit a
            // synthetic UNCONDITIONAL branch to the OSR-exit frame-deopt stub
            // (reason 7) on the NORMAL loop path (right at `pc_to_native[pc]`, the
            // back-edge/fall-through target — NOT the separate OSR-entry landing
            // pad), so normal JIT execution bails to the interpreter at this loop
            // bci on the first reach. `x64_deopt_entry` reconstructs the frame at
            // this bci → the `execute_jit_call` sink resumes the loop body here
            // instead of re-running from entry. Only when `osr_exit_test_trigger_bci`
            // is set (CRATONVM_OSR_EXIT_TEST + DEOPT_REAL); absent in production ⇒
            // no JMP ⇒ byte-identical. The OSR-exit map at this bci (emitted above)
            // is the box the stub bakes via `osr_exit_box_ptr_by_bci`.
            if self.osr_exit_test_trigger_bci == Some(pc) {
                if let Some(n) = self.osr_exit_after_count {
                    // P4: counter-gated bail — bail only on the N-th reach, so the
                    // JIT advances ~N iterations (and commits their side effects)
                    // before the exit. Carries genuinely JIT-advanced state to the
                    // interpreter's true OSR-exit transfer.
                    self.emit_osr_exit_after_trigger(pc, n);
                } else {
                    // Step 8: unconditional bail on the first reach (iteration 0).
                    self.buf.emit_byte(0xE9); // JMP rel32
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    self.deopt_stubs.push((patch_off, pc, 7)); // 7 = OSR-exit
                }
            }

            // deopt-osr: the through-JIT deopt-EXIT differential trigger. At the
            // chosen loop header, record a reason-2 snapshot (if the speculative-BCE
            // path above didn't already) and emit an UNCONDITIONAL branch to the
            // deopt stub on the NORMAL loop path, so the JIT'd loop deopts at this
            // loop bci on the first reach — reconstructing the loop-header frame
            // (incl. long/double/float locals) and resuming in the interpreter via
            // `resume_real_ir_deopt` (the deopt-EXIT sink). Independent of the
            // speculative-BCE guard. Only under CRATONVM_DEOPT_EAGER + DEOPT_REAL ⇒
            // no JMP in production ⇒ byte-identical.
            if self.deopt_eager_bci == Some(pc) {
                if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_SCALAR_DEOPT") {
                    eprintln!("[DBG_SCALAR_DEOPT] x64 eager deopt-EXIT JMP emitted at bci={pc}");
                }
                if !self.deopt_box_ptr_by_bci.contains_key(&pc) {
                    self.emit_deopt_snapshot_at_guard(pc);
                }
                self.buf.emit_byte(0xE9); // JMP rel32
                let patch_off = self.buf.pos();
                self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                self.deopt_stubs.push((patch_off, pc, 2)); // 2 = deopt-EXIT resume
            }

            // === LICM: Replace hoisted sequences with spill slot loads ===
            {
                let hoist_replace = self
                    .hoist_info
                    .iter()
                    .enumerate()
                    .find(|(_, h)| h.seq_start == pc)
                    .map(|(idx, h)| (h.seq_end, self.hoist_offsets[idx]));

                if let Some((seq_end, hoist_offset)) = hoist_replace {
                    // Load cached value from hoisted spill slot
                    self.emit_load_local(RAX, hoist_offset);
                    self.push_from_rax();
                    // Mark intermediate PCs in the skipped sequence
                    let native_pos = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                    let mut skip_pc = pc + bytecode_analysis::step(code, pc);
                    while skip_pc < seq_end {
                        self.pc_to_native[skip_pc] = native_pos;
                        skip_pc += bytecode_analysis::step(code, skip_pc);
                    }
                    pc = seq_end;
                    continue;
                }
            }

            // === LICM: Replace a hoisted `arraylength` with a slot load ===
            // `aload A ; arraylength` becomes one `MOV RAX, [rbp-slot]`,
            // deleting the receiver move, the null check's TEST/JZ and the
            // header dereference from every iteration. The pushed value is an
            // INT: `push_from_rax` leaves the slot unmarked, which is what the
            // oop maps must see -- a length is never a reference.
            {
                let len_replace =
                    self.array_len_hoist_info
                        .iter()
                        .enumerate()
                        .find_map(|(idx, h)| {
                            h.sites
                                .iter()
                                .find(|&&(start, _)| start == pc)
                                .map(|&(_, seq_end)| (seq_end, self.array_len_hoist_offsets[idx]))
                        });

                if let Some((seq_end, hoist_offset)) = len_replace {
                    self.emit_load_local(RAX, hoist_offset);
                    self.push_from_rax();
                    // Mark the skipped `arraylength`. `find_array_len_hoists`
                    // rejects a sequence whose interior is a branch target, so
                    // nothing jumps here -- but `pc_to_native` is also read by
                    // the deopt and OSR machinery, and a `-1` hole there is a
                    // different claim than "the same native point".
                    let native_pos = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                    let mut skip_pc = pc + bytecode_analysis::step(code, pc);
                    while skip_pc < seq_end {
                        self.pc_to_native[skip_pc] = native_pos;
                        skip_pc += bytecode_analysis::step(code, skip_pc);
                    }
                    pc = seq_end;
                    continue;
                }
            }

            // === LICM: Replace hoisted integer-arithmetic runs with a load ===
            // At the start of an invariant run, replace the whole expression
            // with a single load of its cached result. The matched run is
            // straight-line (only constant/iload pushes + ALU ops), so no
            // bytecode inside it is a branch target — but guard anyway: if
            // any interior PC is a branch target, fall through to the normal
            // per-opcode path (the arithmetic is still correct, just not
            // hoisted).
            {
                let arith_replace = self
                    .arith_hoist_info
                    .iter()
                    .enumerate()
                    .find(|(_, h)| h.seq_start == pc)
                    .map(|(idx, h)| (h.seq_end, self.arith_hoist_offsets[idx]));

                if let Some((seq_end, result_offset)) = arith_replace {
                    // Safety: no interior PC may be a branch target.
                    let mut interior_safe = true;
                    let mut q = pc + bytecode_analysis::step(code, pc);
                    while q < seq_end {
                        if branch_targets[q] {
                            interior_safe = false;
                            break;
                        }
                        q += bytecode_analysis::step(code, q);
                    }
                    if interior_safe {
                        self.emit_load_local(RAX, result_offset);
                        self.push_from_rax();
                        let native_pos = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                        let mut skip_pc = pc + bytecode_analysis::step(code, pc);
                        while skip_pc < seq_end {
                            self.pc_to_native[skip_pc] = native_pos;
                            skip_pc += bytecode_analysis::step(code, skip_pc);
                        }
                        pc = seq_end;
                        continue;
                    }
                }
            }

            let op = code[pc];
            self.dbg_last_pc = pc;
            self.dbg_last_op = op;
            let family = match op {
                0x00 => WalkFamily::LocalStack,
                0x01 => WalkFamily::LocalStack,
                0x02..=0x08 => WalkFamily::LocalStack,
                0x09 => WalkFamily::LocalStack,
                0x0a => WalkFamily::LocalStack,
                0x0b => WalkFamily::LocalStack,
                0x0c => WalkFamily::LocalStack,
                0x0d => WalkFamily::LocalStack,
                0x0e => WalkFamily::LocalStack,
                0x0f => WalkFamily::LocalStack,
                0x10 => WalkFamily::LocalStack,
                0x11 => WalkFamily::LocalStack,
                0x12 => WalkFamily::LocalStack,
                0x13 => WalkFamily::LocalStack,
                0x14 => WalkFamily::LocalStack,
                0x15..=0x19 => WalkFamily::LocalStack,
                0x1a..=0x1d => WalkFamily::LocalStack,
                0x1e..=0x21 => WalkFamily::LocalStack,
                0x22..=0x25 => WalkFamily::LocalStack,
                0x26..=0x29 => WalkFamily::LocalStack,
                0x2a..=0x2d => WalkFamily::LocalStack,
                0x2e => WalkFamily::Array,
                0x32 => WalkFamily::Array,
                0x2f => WalkFamily::Array,
                0x30 => WalkFamily::Array,
                0x31 => WalkFamily::Array,
                0x33 => WalkFamily::Array,
                0x34 => WalkFamily::Array,
                0x35 => WalkFamily::Array,
                0x36..=0x3a => WalkFamily::LocalStack,
                0x3b..=0x3e => WalkFamily::LocalStack,
                0x3f..=0x42 => WalkFamily::LocalStack,
                0x43..=0x46 => WalkFamily::LocalStack,
                0x47..=0x4a => WalkFamily::LocalStack,
                0x4b..=0x4e => WalkFamily::LocalStack,
                0x4f => WalkFamily::Array,
                0x53 => WalkFamily::Array,
                0x50 => WalkFamily::Array,
                0x51 => WalkFamily::Array,
                0x52 => WalkFamily::Array,
                0x54 => WalkFamily::Array,
                0x55 => WalkFamily::Array,
                0x56 => WalkFamily::Array,
                0x57 => WalkFamily::LocalStack,
                0x58 => WalkFamily::LocalStack,
                0x59 => WalkFamily::LocalStack,
                0x5a => WalkFamily::LocalStack,
                0x5b => WalkFamily::LocalStack,
                0x5c => WalkFamily::LocalStack,
                0x5d => WalkFamily::LocalStack,
                0x5e => WalkFamily::LocalStack,
                0x5f => WalkFamily::LocalStack,
                0x60 => WalkFamily::Arith,
                0x61 => WalkFamily::Arith,
                0x62 => WalkFamily::Arith,
                0x63 => WalkFamily::Arith,
                0x64 => WalkFamily::Arith,
                0x65 => WalkFamily::Arith,
                0x66 => WalkFamily::Arith,
                0x67 => WalkFamily::Arith,
                0x68 => WalkFamily::Arith,
                0x69 => WalkFamily::Arith,
                0x6a => WalkFamily::Arith,
                0x6b => WalkFamily::Arith,
                0x6c => WalkFamily::Arith,
                0x6d => WalkFamily::Arith,
                0x6e => WalkFamily::Arith,
                0x6f => WalkFamily::Arith,
                0x70 => WalkFamily::Arith,
                0x71 => WalkFamily::Arith,
                0x72 | 0x73 => WalkFamily::Arith,
                0x74 => WalkFamily::Arith,
                0x75 => WalkFamily::Arith,
                0x76 => WalkFamily::Arith,
                0x77 => WalkFamily::Arith,
                0x78 => WalkFamily::Arith,
                0x79 => WalkFamily::Arith,
                0x7a => WalkFamily::Arith,
                0x7b => WalkFamily::Arith,
                0x7c => WalkFamily::Arith,
                0x7d => WalkFamily::Arith,
                0x7e => WalkFamily::Arith,
                0x7f => WalkFamily::Arith,
                0x80 => WalkFamily::Arith,
                0x81 => WalkFamily::Arith,
                0x82 => WalkFamily::Arith,
                0x83 => WalkFamily::Arith,
                0x84 => WalkFamily::LocalStack,
                0xc4 => WalkFamily::LocalStack,
                0x85 => WalkFamily::Arith,
                0x86 => WalkFamily::Arith,
                0x87 => WalkFamily::Arith,
                0x88 => WalkFamily::Arith,
                0x89 => WalkFamily::Arith,
                0x8a => WalkFamily::Arith,
                0x8b => WalkFamily::Arith,
                0x8c => WalkFamily::Arith,
                0x8d => WalkFamily::Arith,
                0x8e => WalkFamily::Arith,
                0x8f => WalkFamily::Arith,
                0x90 => WalkFamily::Arith,
                0x91 => WalkFamily::Arith,
                0x92 => WalkFamily::Arith,
                0x93 => WalkFamily::Arith,
                0x94 => WalkFamily::Arith,
                0x95 => WalkFamily::Arith,
                0x96 => WalkFamily::Arith,
                0x97 => WalkFamily::Arith,
                0x98 => WalkFamily::Arith,
                0x99..=0x9e => WalkFamily::Control,
                0x9f..=0xa4 => WalkFamily::Control,
                0xa7 => WalkFamily::Control,
                0xaa => WalkFamily::Control,
                0xab => WalkFamily::Control,
                0xac..=0xb0 => WalkFamily::Control,
                0xb1 => WalkFamily::Control,
                0xbf => WalkFamily::Control,
                0xb2 => WalkFamily::Field,
                0xb3 => WalkFamily::Field,
                0xb4 => WalkFamily::Field,
                0xb5 => WalkFamily::Field,
                0xb8 => WalkFamily::Invoke,
                0xb6 | 0xb7 | 0xb9 => WalkFamily::Invoke,
                0xba => WalkFamily::Invoke,
                0xbc => WalkFamily::Object,
                0xbb => WalkFamily::Object,
                0xbd => WalkFamily::Object,
                0xbe => WalkFamily::Array,
                0xa5 => WalkFamily::Control,
                0xa6 => WalkFamily::Control,
                0xc0 => WalkFamily::Object,
                0xc1 => WalkFamily::Object,
                0xc5 => WalkFamily::Object,
                0xc6 => WalkFamily::Control,
                0xc7 => WalkFamily::Control,
                0xC2 | 0xC3 => WalkFamily::Object,

                _ => {
                    // A scan-admitted opcode with no arm here. This is NOT
                    // unreachable — `jit_scan` and this dispatch loop are two
                    // hand-maintained opcode tables and nothing forced them to
                    // agree, so an opcode the scanner advances past but this
                    // loop does not lower lands here and loses the method's
                    // compilation for the life of the process, attributed to an
                    // arm that names nothing. `dup2_x2` (0x5E) sat in exactly
                    // that gap until 2026-08-18; `pop2` (0x58) and `dup2_x1`
                    // (0x5D) did before it. The two tables are now compared by
                    // `x64::tests::scan_admitted_opcodes_are_lowered_or_declared`,
                    // which fails when a new one appears. Name the opcode here
                    // so a stray one is at least legible in the bail record.
                    self.fail("singlepass-codegen/opcode-scan-admitted-but-unlowered");
                    return false;
                }
            };
            let step = match family {
                WalkFamily::LocalStack => {
                    self.walk_local_stack(code, code_len, op, pc, &mut dead, &branch_targets)
                }
                WalkFamily::Array => {
                    self.walk_array(code, code_len, op, pc, &mut dead, &branch_targets)
                }
                WalkFamily::Arith => {
                    self.walk_arith(code, code_len, op, pc, &mut dead, &branch_targets)
                }
                WalkFamily::Control => {
                    self.walk_control(code, code_len, op, pc, &mut dead, &branch_targets)
                }
                WalkFamily::Field => {
                    self.walk_field(code, code_len, op, pc, &mut dead, &branch_targets)
                }
                WalkFamily::Invoke => {
                    self.walk_invoke(code, code_len, op, pc, &mut dead, &branch_targets)
                }
                WalkFamily::Object => {
                    self.walk_object(code, code_len, op, pc, &mut dead, &branch_targets)
                }
            };
            match step {
                WalkStep::Next(next) => pc = next,
                WalkStep::Return(done) => return done,
            }
        }

        // Record final PC mapping
        if pc <= self.pc_to_native.len() {
            // Safety: pc might equal code_len
        }

        // Bail out if an internal error (e.g. stack underflow) was detected.
        if self.failed {
            return false;
        }

        // Emit out-of-line bounds check failure stubs (after all bytecode)
        self.emit_bounds_check_stubs();
        // Round-8 CRIT fix: emit shared null-check-failure stub for inline
        // array-store opcodes (iastore / bastore / aastore / lastore /
        // fastore / dastore / castore / sastore). Without this, the inline
        // bounds-check would deref NULL on a null array and the signal
        // handler would re-raise instead of throwing NPE.
        self.emit_null_check_store_stubs();
        // Shared out-of-line stub for post-invoke pending-exception guards.
        // Without this, a JIT-dispatched callee that throws would leave the
        // exception stashed in TLS while the JIT kept running with a bogus
        // `0` return value (Jetty `Main.main` "getClasspath on null").
        // Local-handler dispatch stubs FIRST: each one's "not ours" edge is
        // recorded as an ordinary entry in one of the two lists below, so both
        // must still be unemitted when this runs.
        self.emit_local_handler_stubs();
        self.emit_exception_check_stub();
        self.emit_deopt_stubs();
        true
    }

    /// `iinc local[idx] += inc`, for both the narrow (`0x84`) and the `wide`
    /// (`0xC4 0x84`) encodings.
    ///
    /// One emitter, two callers, because the two encodings differ ONLY in how
    /// the index and the constant are decoded — the machine code is identical,
    /// and it already handled an `imm32` addend before `wide` could reach it.
    /// A second copy would be a second place to forget the `MOVSXD` that
    /// re-canonicalises the 32-bit result into the 64-bit local slot.
    pub(super) fn emit_iinc_local(&mut self, idx: usize, inc: i32) {
        if let Some(local_reg) = self.reg_for_local(idx) {
            // Invalidate any CalleeSaved refs before modifying the register
            self.invalidate_callee_saved(local_reg);
            // ADD r32, imm directly on the callee-saved register
            if local_reg >= 8 {
                self.buf.emit_byte(0x41); // REX.B
            }
            if (-128..=127).contains(&inc) {
                self.buf.emit_byte(0x83); // ADD r/m32, imm8
                self.buf.emit_byte(0xC0 | (local_reg & 7));
                self.buf.emit_byte(inc as u8); // Cast: x86-64 immediate encoding
            } else {
                self.buf.emit_byte(0x81); // ADD r/m32, imm32
                self.buf.emit_byte(0xC0 | (local_reg & 7));
                self.buf.emit(&inc.to_le_bytes());
            }
            // Sign-extend r32 to r64
            self.rex_w_rb(local_reg, local_reg);
            self.buf.emit_byte(0x63); // MOVSXD r64, r/m32
            self.modrm_reg(local_reg, local_reg);
        } else {
            let off = self.local_offset(idx);
            self.emit_load_local(RAX, off);
            if (-128..=127).contains(&inc) {
                self.buf.emit(&[0x83, 0xC0]); // ADD eax, imm8
                self.buf.emit_byte(inc as u8); // Cast: x86-64 immediate encoding
            } else {
                self.buf.emit_byte(0x05); // ADD eax, imm32
                self.buf.emit(&inc.to_le_bytes());
            }
            // Sign-extend back
            self.rex_w();
            self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
            self.emit_store_local(off, RAX);
        }
    }
}
