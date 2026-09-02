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
fn substitute_unresolved_field_sites() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_UNRESOLVED_FIELD_SUBSTITUTE").is_some()
    })
}

/// Snapshot of [`UNRESOLVED_FIELD_SITE_BAILS`], for the end-of-run report.
pub fn unresolved_field_site_bails() -> u64 {
    UNRESOLVED_FIELD_SITE_BAILS.load(std::sync::atomic::Ordering::Relaxed)
}

fn unresolved_field_site(pc: usize, opcode: u8) -> bool {
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
fn no_aastore_barrier_gate() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_AASTORE_BARRIER_GATE").is_some()
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
fn dbg_aastore_barrier_gate() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_AASTORE_BARRIER_GATE").is_some()
    })
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
        let next = cur + crate::scev::bytecode_len(code, cur, code_len);
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
                p += bytecode_len_at(code, p);
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
        let reachable = compute_reachable_pcs_with_roots(code, code_len, &local_handler_pcs);
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
            let normal = compute_reachable_pcs(code, code_len);
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
                    // `fixed-suite-bugs/testoutputbuffer-writespeed-content-length-mismatch-FIXED.md`).
                    // `record_branch_target_depth` now captures the REAL
                    // marks live at this target the first time it's seen
                    // (mirroring how `expected_depth` itself is captured);
                    // use them when present. Falls back to the historical
                    // all-`false` reconstruction only if no marks were ever
                    // recorded for this pc (shouldn't happen — every
                    // depth-recording call site records marks alongside —
                    // but degrades to the pre-existing, already-reviewed-safe
                    // behavior rather than panicking or guessing).
                    self.stack_oop_marks = self
                        .branch_target_stack_oop_marks
                        .get(&pc)
                        .filter(|marks| marks.len() == expected_depth)
                        .cloned()
                        .unwrap_or_else(|| vec![false; expected_depth]);
                    self.stack_oop_marks_exact = expected_depth == 0;
                } else {
                    self.pc_to_native[pc] = -1;
                    pc += bytecode_len_at(code, pc);
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
                if inside_aaload_hoisted
                    || inside_arith_hoisted
                    || inside_synthetic_guard
                    || handler_only
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
            // (see the `despec_contains` filter on `hoist_info` in
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

            // === SIMD FP: Emit vectorized preheader for double-array-sum loops ===
            {
                let simd_fp_match = self
                    .simd_fp_loops
                    .iter()
                    .find(|s| s.header_pc == pc)
                    .map(|s| (s.iv_local, s.acc_local, s.array_local, s.bound_local));

                if let Some((iv_local, acc_local, array_local, bound_local)) = simd_fp_match {
                    let acc_offset = self.local_offset(acc_local);

                    // Sync accumulator XMM/register → frame slot
                    if let Some(xmm) = self.xmm_for_local(acc_local) {
                        self.emit_movq_mem_rbp_from_xmm(acc_offset, xmm);
                    } else if let Some(acc_reg) = self.reg_for_local(acc_local) {
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

                    // Emit SIMD FP array sum
                    self.emit_simd_fp_array_sum(acc_offset);

                    // Sync accumulator frame slot → XMM/register
                    if let Some(xmm) = self.xmm_for_local(acc_local) {
                        self.emit_load_local(RAX, acc_offset);
                        self.emit_movq_xmm_from_rax(xmm);
                    } else if let Some(acc_reg) = self.reg_for_local(acc_local) {
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

            // === T17.Β.3 — Loop unswitch pre-header evaluation ===========
            //
            // The detector has already proved that `invariant_local`
            // is never written inside the loop body and that the
            // body size is ≤ MAX_UNSWITCH_BYTECODES. We emit a
            // single evaluation of the invariant predicate at the
            // preheader. The body's per-iteration branch still
            // executes as before (so semantics are bit-identical to
            // the scalar loop), but the early evaluation:
            //
            // 1. Warms the CPU branch predictor for the branch's
            //    single outcome — because the predicate is
            //    invariant, the per-iteration branch is always
            //    taken the same way.
            // 2. Serves as a hook for future body-duplication: the
            //    pre-evaluation slot can be consumed by a specialized
            //    code-gen variant without changing the invariant.
            //
            // # Correctness
            //
            // The emitted sequence is *additive* — it reads
            // `invariant_local` and sets flags but never writes back
            // to any local. Because detection rejects loops that
            // write `invariant_local`, the value observed at the
            // preheader matches the value observed on every
            // iteration. Removing the emission yields identical
            // final state, which is exactly the "bytecode-equivalent
            // semantics" the scope requires.
            self.emit_loop_unswitch_preheader(pc);

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
                if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SCALAR_DEOPT").is_some() {
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
                    let mut skip_pc = pc + bytecode_len_at(code, pc);
                    while skip_pc < seq_end {
                        self.pc_to_native[skip_pc] = native_pos;
                        skip_pc += bytecode_len_at(code, skip_pc);
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
                    let mut q = pc + bytecode_len_at(code, pc);
                    while q < seq_end {
                        if branch_targets[q] {
                            interior_safe = false;
                            break;
                        }
                        q += bytecode_len_at(code, q);
                    }
                    if interior_safe {
                        self.emit_load_local(RAX, result_offset);
                        self.push_from_rax();
                        let native_pos = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                        let mut skip_pc = pc + bytecode_len_at(code, pc);
                        while skip_pc < seq_end {
                            self.pc_to_native[skip_pc] = native_pos;
                            skip_pc += bytecode_len_at(code, skip_pc);
                        }
                        pc = seq_end;
                        continue;
                    }
                }
            }

            let op = code[pc];
            self.dbg_last_pc = pc;
            self.dbg_last_op = op;
            match op {
                // nop
                0x00 => {
                    pc += 1;
                }

                // aconst_null — push 0 (null reference)
                0x01 => {
                    self.emit_xor_reg_self(RAX);
                    self.push_from_rax();
                    // T1.1.a — null is a valid object reference per JVMS.
                    self.mark_top_as_oop();
                    pc += 1;
                }

                // iconst_m1..iconst_5
                0x02..=0x08 => {
                    let val = op as i32 - 3; // Widening: always safe
                    if self.try_const_arith_peephole(val, pc + 1, code, code_len, &branch_targets) {
                        pc += 2;
                    } else if let Some(next_pc) = self.try_const_compare_peephole(
                        val,
                        pc + 1,
                        code,
                        code_len,
                        &branch_targets,
                    ) {
                        pc = next_pc;
                    } else {
                        self.emit_mov_imm32_sx(RAX, val);
                        self.push_from_rax();
                        pc += 1;
                    }
                }

                // lconst_0
                0x09 => {
                    self.emit_xor_reg_self(RAX);
                    self.push_from_rax();
                    pc += 1;
                }

                // lconst_1
                0x0a => {
                    self.emit_mov_imm32_sx(RAX, 1);
                    self.push_from_rax();
                    pc += 1;
                }

                // fconst_0
                0x0b => {
                    // 0.0f32 → bits = 0x00000000
                    self.emit_xor_reg_self(RAX);
                    self.push_from_rax();
                    pc += 1;
                }

                // fconst_1
                0x0c => {
                    // 1.0f32 → bits = 0x3F800000 = 1065353216
                    self.emit_mov_imm32_sx(RAX, 0x3F80_0000u32 as i32); // Cast: x86-64 immediate encoding
                    self.push_from_rax();
                    pc += 1;
                }

                // fconst_2
                0x0d => {
                    // 2.0f32 → bits = 0x40000000 = 1073741824
                    self.emit_mov_imm32_sx(RAX, 0x4000_0000u32 as i32); // Cast: x86-64 immediate encoding
                    self.push_from_rax();
                    pc += 1;
                }

                // dconst_0
                0x0e => {
                    // 0.0f64 → bits = 0x0000000000000000
                    self.emit_xor_reg_self(RAX);
                    self.push_from_rax();
                    pc += 1;
                }

                // dconst_1
                0x0f => {
                    // 1.0f64 → bits = 0x3FF0000000000000
                    self.emit_mov_imm64(RAX, 0x3FF0_0000_0000_0000u64 as i64); // Cast: JIT ABI convention
                    self.push_from_rax();
                    pc += 1;
                }

                // bipush
                0x10 => {
                    let val = code[pc + 1] as i8 as i32; // Widening: always safe
                    if self.try_const_arith_peephole(val, pc + 2, code, code_len, &branch_targets) {
                        pc += 3;
                    } else if let Some(next_pc) = self.try_const_compare_peephole(
                        val,
                        pc + 2,
                        code,
                        code_len,
                        &branch_targets,
                    ) {
                        pc = next_pc;
                    } else {
                        self.emit_mov_imm32_sx(RAX, val);
                        self.push_from_rax();
                        pc += 2;
                    }
                }

                // sipush
                0x11 => {
                    let val = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32; // Widening: always safe
                    if self.try_const_arith_peephole(val, pc + 3, code, code_len, &branch_targets) {
                        pc += 4;
                    } else if let Some(next_pc) = self.try_const_compare_peephole(
                        val,
                        pc + 3,
                        code,
                        code_len,
                        &branch_targets,
                    ) {
                        pc = next_pc;
                    } else {
                        self.emit_mov_imm32_sx(RAX, val);
                        self.push_from_rax();
                        pc += 3;
                    }
                }

                // ldc — load int/float/string/class constant from CP (1-byte index)
                0x12 => {
                    if self.emit_ldc_class(pc) {
                        pc += 2;
                        continue;
                    }
                    if self.ldc_class_info_idx.contains_key(&pc) {
                        return false;
                    }
                    if self.emit_ldc_string(pc) {
                        pc += 2;
                        continue;
                    }
                    if self.ldc_string_info_idx.contains_key(&pc) {
                        // Recognised as a string `ldc` and NOT emittable —
                        // `ldc_string_cp` unwired, or no context slot. Bail the
                        // site rather than fall through to `ldc_info`, which
                        // does not hold this pc and would push a null.
                        return false;
                    }
                    let val = self.ldc_info_idx.get(&pc).map(|&i| self.ldc_info[i].1);
                    match val {
                        Some(v) => {
                            self.emit_mov_imm64(RAX, v);
                            self.push_from_rax();
                            pc += 2;
                        }
                        None => return false,
                    }
                }

                // ldc_w — load int/float/string/class constant from CP (2-byte index)
                0x13 => {
                    if self.emit_ldc_class(pc) {
                        pc += 3;
                        continue;
                    }
                    if self.ldc_class_info_idx.contains_key(&pc) {
                        return false;
                    }
                    if self.emit_ldc_string(pc) {
                        pc += 3;
                        continue;
                    }
                    if self.ldc_string_info_idx.contains_key(&pc) {
                        // Recognised as a string `ldc` and NOT emittable —
                        // `ldc_string_cp` unwired, or no context slot. Bail the
                        // site rather than fall through to `ldc_info`, which
                        // does not hold this pc and would push a null.
                        return false;
                    }
                    let val = self.ldc_info_idx.get(&pc).map(|&i| self.ldc_info[i].1);
                    match val {
                        Some(v) => {
                            self.emit_mov_imm64(RAX, v);
                            self.push_from_rax();
                            pc += 3;
                        }
                        None => return false,
                    }
                }

                // ldc2_w — load long/double constant from CP (resolved to i64)
                0x14 => {
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let val = self.ldc2w_info_idx.get(&pc).map(|&i| self.ldc2w_info[i].1);
                    match val {
                        Some(v) => {
                            // Long const-arith fusion (perf/halfgap residuals,
                            // 2026-07-18): `ldc2_w K; l{mul,div,rem,add,sub}` is
                            // the dominant shape of long arithmetic kernels
                            // (`i * 3`, `i / 2`, `i % 7`). A long op as the next
                            // opcode implies the constant is a long, not a
                            // double (the verifier rejects the mix).
                            if self.try_const_arith_peephole_long(
                                v,
                                pc + 3,
                                code,
                                code_len,
                                &branch_targets,
                            ) {
                                pc += 4;
                                continue;
                            }
                            self.emit_mov_imm64(RAX, v);
                            self.push_from_rax();
                            pc += 3;
                        }
                        None => {
                            // Not resolved — bail out; method will stay interpreted
                            return false;
                        }
                    }
                }

                // iload / lload / fload / dload / aload
                // iload/lload/fload/dload/aload (wide index: opcode 0x15-0x19, then idx byte)
                0x15..=0x19 => {
                    // Affine self-recurrence strength reduction (CRATONVM_JIT_REASSOC).
                    if op == 0x15 && crate::ir_optimize::reassoc_enabled() {
                        if let Some((local, k, c, end)) =
                            match_affine_chain(code, pc, code_len, &branch_targets)
                        {
                            self.emit_affine_fold(local, k, c);
                            pc = end;
                            continue;
                        }
                    }
                    let idx = code[pc + 1] as usize; // Widening: always safe
                                                     // fload (0x17) and dload (0x18) may have XMM-allocated locals
                    if matches!(op, 0x17 | 0x18) {
                        if let Some(xmm) = self.xmm_for_local(idx) {
                            // FP value — never an oop.
                            self.stack_push(StackSlot::Xmm(xmm), false);
                            pc += 2;
                            continue;
                        }
                    }
                    let is_aload = op == 0x19;
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        // T1.1.a — only aload pushes oops; iload/lload/fload/dload
                        // push primitives. Stage 1 — keep marks in lockstep.
                        self.stack_push(StackSlot::CalleeSaved(local_reg), is_aload);
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_load_local(RAX, off);
                        self.push_from_rax();
                        if is_aload {
                            self.mark_top_as_oop();
                        }
                    }
                    pc += 2;
                }

                // iload_0..iload_3
                0x1a..=0x1d => {
                    // Affine self-recurrence strength reduction (CRATONVM_JIT_REASSOC):
                    // fold a run of `x = x*c1 + c2` steps into one `x = x*K + C`.
                    if crate::ir_optimize::reassoc_enabled() {
                        if let Some((local, k, c, end)) =
                            match_affine_chain(code, pc, code_len, &branch_targets)
                        {
                            self.emit_affine_fold(local, k, c);
                            pc = end;
                            continue;
                        }
                    }
                    let idx = (op - 0x1a) as usize; // Widening: always safe
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        // Zero-cost: just record register reference on simulated
                        // stack. iload pushes a primitive — never an oop.
                        self.stack_push(StackSlot::CalleeSaved(local_reg), false);
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_load_local(RAX, off);
                        self.push_from_rax();
                    }
                    pc += 1;
                }

                // lload_0..lload_3
                0x1e..=0x21 => {
                    let idx = (op - 0x1e) as usize; // Widening: always safe
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        self.stack_push(StackSlot::CalleeSaved(local_reg), false);
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_load_local(RAX, off);
                        self.push_from_rax();
                    }
                    pc += 1;
                }

                // fload_0..fload_3 (float load)
                0x22..=0x25 => {
                    let idx = (op - 0x22) as usize; // Widening: always safe
                    if let Some(xmm) = self.xmm_for_local(idx) {
                        self.stack_push(StackSlot::Xmm(xmm), false);
                    } else if let Some(local_reg) = self.reg_for_local(idx) {
                        self.stack_push(StackSlot::CalleeSaved(local_reg), false);
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_load_local(RAX, off);
                        self.push_from_rax();
                    }
                    pc += 1;
                }

                // dload_0..dload_3 (double load)
                0x26..=0x29 => {
                    let idx = (op - 0x26) as usize; // Widening: always safe
                    if let Some(xmm) = self.xmm_for_local(idx) {
                        // Zero-cost push: just reference the XMM register.
                        // No code emitted until the value is consumed.
                        self.stack_push(StackSlot::Xmm(xmm), false);
                    } else if let Some(local_reg) = self.reg_for_local(idx) {
                        self.stack_push(StackSlot::CalleeSaved(local_reg), false);
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_load_local(RAX, off);
                        self.push_from_rax();
                    }
                    pc += 1;
                }

                // aload_0..aload_3 (reference load — identical to iload for JIT)
                0x2a..=0x2d => {
                    let idx = (op - 0x2a) as usize; // Widening: always safe
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        // aload* always pushes an object ref. Stage 1 — keep
                        // marks in lockstep and tag the entry. CalleeSaved slots
                        // don't have a frame offset, so the oop-map walker skips
                        // them (preserved by the ABI across calls and cached by
                        // the JIT's frame save/restore prologue).
                        self.stack_push(StackSlot::CalleeSaved(local_reg), true);
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_load_local(RAX, off);
                        self.push_from_rax();
                        // T1.1.a — aload* always pushes an object ref.
                        self.mark_top_as_oop();
                    }
                    pc += 1;
                }

                // iaload — load int from int[] array (inline)
                0x2e => {
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-9 HIGH fix: NPE on null array (JVMS §iaload).
                    self.emit_null_check_array_load_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.emit_int_aload_regs();
                    self.push_from_rax();
                    pc += 1;
                }

                // aaload — load reference from Object[] array (inline, 8 bytes/element)
                0x32 => {
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-9 HIGH fix: NPE on null array (JVMS §aaload).
                    self.emit_null_check_array_load_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.emit_ref_aload_regs();
                    self.push_from_rax();
                    // T1.1.a — aaload reads a reference from an Object[].
                    self.mark_top_as_oop();
                    pc += 1;
                }

                // laload — load long from long[] array (inline, 8 bytes/element)
                0x2f => {
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-9 HIGH fix: NPE on null array (JVMS §laload).
                    self.emit_null_check_array_load_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.emit_long_aload_regs();
                    self.push_from_rax();
                    pc += 1;
                }

                // faload — load float from float[] array (inline, 4 bytes/element)
                0x30 => {
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-9 HIGH fix: NPE on null array (JVMS §faload).
                    self.emit_null_check_array_load_at(code, pc);
                    self.emit_bounds_check(pc);
                    // Float is 4 bytes, same as int; value is stored as bit pattern
                    self.emit_int_aload_regs();
                    self.push_from_rax_as_xmm0();
                    pc += 1;
                }

                // daload — load double from double[] array (inline, 8 bytes/element)
                0x31 => {
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-9 HIGH fix: NPE on null array (JVMS §daload).
                    self.emit_null_check_array_load_at(code, pc);
                    self.emit_bounds_check(pc);
                    // Double is 8 bytes, same as long; value is stored as bit pattern
                    self.emit_long_aload_regs();
                    self.push_from_rax_as_xmm0();
                    pc += 1;
                }

                // baload — load byte/boolean from byte[]/boolean[] array (inline)
                0x33 => {
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-9 HIGH fix: NPE on null array (JVMS §baload).
                    self.emit_null_check_array_load_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.emit_byte_aload_regs();
                    self.push_from_rax();
                    pc += 1;
                }

                // caload — load char from char[] array (inline, 2 bytes/element, zero-extend)
                0x34 => {
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-9 HIGH fix: NPE on null array (JVMS §caload).
                    self.emit_null_check_array_load_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.emit_char_aload_regs();
                    self.push_from_rax();
                    pc += 1;
                }

                // saload — load short from short[] array (inline, 2 bytes/element, sign-extend)
                0x35 => {
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-9 HIGH fix: NPE on null array (JVMS §saload).
                    self.emit_null_check_array_load_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.emit_short_aload_regs();
                    self.push_from_rax();
                    pc += 1;
                }

                // istore / lstore / fstore / dstore / astore
                // istore/lstore/fstore/dstore/astore (wide index)
                0x36..=0x3a => {
                    let idx = code[pc + 1] as usize; // Widening: always safe
                                                     // fstore (0x38) and dstore (0x39) may have XMM-allocated locals
                    if matches!(op, 0x38 | 0x39) {
                        if let Some(dst_xmm) = self.xmm_for_local(idx) {
                            let slot = self.pop_stack();
                            match slot {
                                StackSlot::Xmm(src) if src == dst_xmm => {}
                                StackSlot::Xmm(src) => {
                                    if op == 0x39 {
                                        self.emit_movsd_xmm_xmm(dst_xmm, src);
                                    } else {
                                        self.emit_movss_xmm_xmm(dst_xmm, src);
                                    }
                                }
                                _ => {
                                    self.load_slot_to_reg(RAX, slot);
                                    self.emit_movq_xmm_from_rax(dst_xmm);
                                }
                            }
                            pc += 2;
                            continue;
                        }
                    }
                    self.pop_to_rax();
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        self.invalidate_callee_saved(local_reg);
                        self.emit_mov_reg_reg(local_reg, RAX);
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_store_local(off, RAX);
                    }
                    pc += 2;
                }

                // istore_0..istore_3
                0x3b..=0x3e => {
                    let idx = (op - 0x3b) as usize; // Widening: always safe
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        self.invalidate_callee_saved(local_reg);
                    }
                    self.pop_to_rax();
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        self.emit_mov_reg_reg(local_reg, RAX);
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_store_local(off, RAX);
                    }
                    pc += 1;
                }

                // lstore_0..lstore_3
                0x3f..=0x42 => {
                    let idx = (op - 0x3f) as usize; // Widening: always safe
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        self.invalidate_callee_saved(local_reg);
                    }
                    self.pop_to_rax();
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        self.emit_mov_reg_reg(local_reg, RAX);
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_store_local(off, RAX);
                    }
                    pc += 1;
                }

                // fstore_0..fstore_3 (float store)
                0x43..=0x46 => {
                    let idx = (op - 0x43) as usize; // Widening: always safe
                    if let Some(dst_xmm) = self.xmm_for_local(idx) {
                        let slot = self.pop_stack();
                        match slot {
                            StackSlot::Xmm(src) if src == dst_xmm => {}
                            StackSlot::Xmm(src) => {
                                self.emit_movss_xmm_xmm(dst_xmm, src);
                            }
                            _ => {
                                self.load_slot_to_reg(RAX, slot);
                                self.emit_movq_xmm_from_rax(dst_xmm);
                            }
                        }
                    } else {
                        self.pop_to_rax();
                        if let Some(local_reg) = self.reg_for_local(idx) {
                            self.invalidate_callee_saved(local_reg);
                            self.emit_mov_reg_reg(local_reg, RAX);
                        } else {
                            let off = self.local_offset(idx);
                            self.emit_store_local(off, RAX);
                        }
                    }
                    pc += 1;
                }

                // dstore_0..dstore_3 (double store)
                0x47..=0x4a => {
                    let idx = (op - 0x47) as usize; // Widening: always safe
                                                    // Optimize: if top-of-stack is Xmm and target is XMM local,
                                                    // move directly XMM→XMM without going through RAX.
                    if let Some(dst_xmm) = self.xmm_for_local(idx) {
                        let slot = self.pop_stack();
                        match slot {
                            StackSlot::Xmm(src) if src == dst_xmm => {
                                // Already in the right register — no-op
                            }
                            StackSlot::Xmm(src) => {
                                self.emit_movsd_xmm_xmm(dst_xmm, src);
                            }
                            _ => {
                                self.load_slot_to_reg(RAX, slot);
                                self.emit_movq_xmm_from_rax(dst_xmm);
                            }
                        }
                    } else {
                        self.pop_to_rax();
                        if let Some(local_reg) = self.reg_for_local(idx) {
                            self.invalidate_callee_saved(local_reg);
                            self.emit_mov_reg_reg(local_reg, RAX);
                        } else {
                            let off = self.local_offset(idx);
                            self.emit_store_local(off, RAX);
                        }
                    }
                    pc += 1;
                }

                // astore_0..astore_3 (reference store — identical to istore for JIT)
                0x4b..=0x4e => {
                    let idx = (op - 0x4b) as usize; // Widening: always safe
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        self.invalidate_callee_saved(local_reg);
                    }
                    self.pop_to_rax();
                    if let Some(local_reg) = self.reg_for_local(idx) {
                        self.emit_mov_reg_reg(local_reg, RAX);
                    } else {
                        let off = self.local_offset(idx);
                        self.emit_store_local(off, RAX);
                    }
                    pc += 1;
                }

                // iastore — store int to int[] array (inline)
                0x4f => {
                    let val_slot = self.pop_stack();
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-8 CRIT fix: NPE on null array (JVMS §iastore).
                    self.emit_null_check_array_store_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.load_slot_to_reg(RDX, val_slot);
                    self.emit_int_astore_regs();
                    pc += 1;
                }

                // aastore — store a reference into a reference array.
                //
                // Lowered INLINE (null check, bounds check, covariance check,
                // SATB pre-write barrier, store, card mark). The single
                // call-out is `jit_aastore_type_check` (vm/src/jit/helpers.rs),
                // because answering the JVMS §6.5 covariance question needs the
                // class manager. The complete-opcode helper `jit_aastore` is
                // NOT reached from here.
                //
                // ## Why the covariance check is emitted at all
                //
                // This note used to read "the current `jit_aastore` helper does
                // NOT enforce the ASE check (the interpreter does it via
                // `set_array_element`). This inline path matches the helper's
                // behavior exactly — no regression." That premise was TRUE when
                // R20 / HIGH-5 (docs/PRESENTATION.md) replaced the helper call
                // with the inline store, and was FALSIFIED later — silently —
                // when the JVMS §aastore covariance check landed inside
                // `jit_aastore`, because this path had already stopped calling
                // that helper and a premise stated in a comment is not a
                // compile-time link. From that moment the compiled tier
                // performed every reference array store unconditionally while
                // the interpreter refused the illegal ones, and nothing failed
                // to build.
                //
                // The consequence is not a wrong answer, it is heap type
                // confusion: `Object[] a = new String[1]; a[0] = anInteger;`
                // leaves an `Integer` inside a `String[]`, so a later
                // `aaload`-and-use reads a `String`-typed reference to an
                // `Integer` with no cast to catch it. Under a precise GC that
                // is a memory-safety-relevant corruption, not an etiquette
                // problem — and it is TIER-DEPENDENT: correct for the first
                // ~500 executions and wrong once the method tiers up.
                // `RExceptions` read `cold=[java.lang.Integer] hot=[no-throw]`
                // at i≈500, i.e. the tier-parity assertion caught it the moment
                // the method tiered up. See
                // docs/known-issues/jdk-only/W7-38-jit-aastore-never-called-its-own-check.md.
                //
                // The claim that an inline ASE check "needs type-narrowing
                // infrastructure" is also not so: type narrowing is what would
                // let a check be ELIDED, not what makes one correct. And the
                // rule now lives in exactly ONE body — `aastore_store_is_refused`
                // in vm/src/jit/helpers.rs, shared by `jit_aastore_type_check`
                // and `jit_aastore` — so the inline lowering and the full helper
                // can never again enforce different rules.
                //
                // ## Ordering — JVMS §6.5 is NPE → AIOOBE → ASE
                //
                // Not stylistic. Putting the covariance check ahead of the
                // bounds check reproduces the exact divergence
                // `RArrayStoreTiers` s15 caught in the interpreter fast path:
                // ASE reported for a past-the-end index.
                //
                // `flush_scratch_registers` first: it rewrites every
                // register-resident (`Scratch`/`Xmm`) stack slot to a frame
                // slot, so every `load_slot_to_reg` below reads from memory and
                // cannot clobber another's source register regardless of ABI
                // (`ARG_REGS` is RCX/RDX/R8/R9 on Windows, RDI/RSI/RDX/RCX on
                // SysV).
                //
                // A helper call clobbers the caller-saved registers, so
                // `array_slot` / `index_slot` / `val_slot` are RE-LOADED after
                // the check and again after the SATB barrier. Hoisting those
                // loads above either call is silently wrong.
                //
                // 0x53 is a one-byte opcode, so
                // `emit_post_invoke_exception_check` keeps THIS pc as the throw
                // pc, which is what the handler `[start_pc, end_pc)` range test
                // needs (see the note at that function).
                //
                // ## Why this arm must force the dispatch-aware entry
                //
                // `jit_aastore_type_check` builds its `ArrayStoreException`
                // through `jit_thread_mut()`, which only the dispatch-aware
                // entry sets (`vm/src/runtime/interpreter/jit_bridge.rs` — the
                // `!compiled.has_dispatch` arm skips `set_jit_thread`). Without
                // it the check fails open (its documented last resort) and the
                // illegal store proceeds. `emitted_aastore_throw` below is what
                // forces that entry; `x64/driver.rs` reads it alongside
                // `emitted_checkcast_throw`, for the same reason.
                0x53 => {
                    // The gate census. `AASTORE_SITES_WALKED` is the
                    // denominator that makes the expected ZERO on
                    // `AASTORE_ZGC_GATE_FALLBACKS` readable as "consulted and
                    // correctly declined" rather than "never reached"; see the
                    // doc on those statics.
                    AASTORE_SITES_WALKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let barrier_armed = zgc_read_barrier_blocks_inline_fields();
                    let gate_taken = barrier_armed && !no_aastore_barrier_gate();
                    if barrier_armed && !gate_taken {
                        AASTORE_ZGC_GATE_SUPPRESSED
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    if dbg_aastore_barrier_gate() {
                        let verdict = if gate_taken {
                            "helper-fallback"
                        } else if barrier_armed {
                            "inline/suppressed-by-kill-switch"
                        } else {
                            "inline/barrier-not-armed"
                        };
                        eprintln!("[aastore-gate] pc={pc} armed={barrier_armed} verdict={verdict}");
                    }
                    self.flush_scratch_registers();
                    let val_slot = self.pop_stack();
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-8 CRIT fix: NPE on null array (JVMS §aastore).
                    // Records a null-check stub.
                    self.emit_null_check_array_store_at(code, pc);
                    // AIOOBE. Records a bounds-check stub.
                    self.emit_bounds_check(pc);

                    // ## The ZGC read-barrier coverage gate
                    //
                    // Everything below this point touches the element SLOT as
                    // raw machine code: `emit_ref_aload_regs` reads the old
                    // reference for the SATB snapshot and `emit_ref_astore_regs`
                    // writes the new one. Both emitters understand compressed
                    // oops themselves (`emit_narrow_ref_aload_regs` /
                    // `emit_narrow_ref_astore_regs` encode and decode the 4-byte
                    // `(addr - base) >> 3` form), which is exactly why the
                    // missing gate here stayed invisible: the narrow half of
                    // `narrow_oops_block_inline_fields()` is genuinely handled,
                    // so nothing ever went wrong under narrow oops and nobody
                    // looked at the other half.
                    //
                    // The other half is `zgc_read_barrier_blocks_inline_fields()`
                    // and it is NOT handled. Under an armed ZGC cycle a
                    // reference slot holds `Z_COLORED_TAG (bit 63) | colour
                    // (bits 42..=46) | 42-bit offset`, which is not a machine
                    // pointer at all:
                    //   * the inline LOAD would read that word with no colour
                    //     test and hand it to `jit_satb_pre_write_barrier` as if
                    //     it were a pointer -- a Category-A read-barrier hole,
                    //     and one that is not even among the nine emission
                    //     points of `zgc-jit-load-barrier.md` 2.3;
                    //   * the inline STORE would write a PLAIN pointer into a
                    //     slot the barrier next classifies with
                    //     `classify_bad_masked`, which reads an uncoloured word
                    //     as `Good` and truncates it to 42 bits -- silently, per
                    //     that function's own doc -- and can clobber a heal that
                    //     was concurrently in flight.
                    //
                    // Be precise about what this is NOT. It is not the
                    // `write_ref_slot` data race of
                    // `.agent-requests/A16-vm-stores.txt` section 1: an aligned
                    // qword `mov` emitted by the JIT is not a Rust memory access
                    // at all, and on x86-64 it is architecturally atomic against
                    // a `lock cmpxchg`. Nothing here can tear, and no Rust UB is
                    // in play. What is wrong is COVERAGE -- the rule
                    // `classify_bad_masked` states for itself, that a slot is
                    // either fully barriered on BOTH the read and the write side
                    // or is never handed to it at all.
                    //
                    // # Why the ZGC disjunct only, and not the whole
                    // # `narrow_oops_block_inline_fields()` predicate
                    //
                    // Because the narrow half is already covered here, and
                    // refusing it would be a pure throughput regression on about
                    // the hottest reference-store opcode there is: every
                    // `Object[]` store in every narrow-oops run would take a
                    // helper call to buy nothing. The two compact-field arms
                    // (`getfield` above, reference `putfield` below) use the
                    // combined predicate because THEIR inline emitters bake an
                    // 8-byte access at a fixed offset and would read the wrong
                    // width under narrow oops; that hazard does not exist at
                    // this site. ZGC and compressed oops are mutually refused at
                    // VM init in any case (A9's P1), so the two disjuncts can
                    // never both be true and splitting them loses nothing.
                    //
                    // # Why this changes nothing on a default run
                    //
                    // `cratonvm_types::zgc_read_barrier_armed()` is a
                    // process-global flag whose only writer is
                    // `ZgcRealHeap::set_barrier_color`, which has no non-test
                    // caller; `RELOCATION_REQUESTED` is a pinned `false` in
                    // `vm/src/vm/vm_init.rs`, and `barrier_good_mask` never
                    // leaves `Z_REMAPPED`. So `gate_taken` is false in every
                    // shipping configuration and the bytes this arm emits are
                    // identical to what it emitted before. The cost is one
                    // relaxed load of that flag per compiled `aastore` SITE --
                    // not per execution.
                    //
                    // What it does close is a claim that was already being made
                    // elsewhere: `zgc_codegen_honours_read_barrier()` returns a
                    // constant `true`, and its mechanism 2 says "every inline
                    // compact-field site is gated on
                    // `narrow_oops_block_inline_fields`". This site was not, so
                    // that justification was false here; `zgc_relocation_permitted`
                    // reads it, which is how an unclosed hole would have become
                    // a relocating cycle handing JIT code stale pointers.
                    //
                    // # The residual this cannot close
                    //
                    // An emission-time gate cannot reach code that is ALREADY
                    // compiled, so arming must still happen where no Java thread
                    // is inside compiled code, i.e. at a safepoint. That
                    // obligation is recorded on `ZgcRealHeap::set_barrier_color`
                    // and is unchanged by this gate.
                    if gate_taken {
                        // The fallback is `jit_aastore`, which performs the
                        // whole opcode -- NPE, AIOOBE, the SAME
                        // `aastore_store_is_refused` covariance rule the inline
                        // path calls, the SATB pre-read, the store and the post
                        // barrier -- through `read_ref_slot` / `write_ref_slot`,
                        // the single chokepoint the barrier is being plumbed
                        // into. The design doc calls the helper-CALL arms "the
                        // barrier's cheap escape hatch": correct today, with
                        // inline emission a throughput optimisation on top
                        // rather than a correctness prerequisite.
                        //
                        // The null and bounds checks above are left in place and
                        // are therefore emitted twice on this path. That is
                        // deliberate: they are the well-exercised JIT stubs, the
                        // duplicate is a compare on a path that cannot execute
                        // today, and removing them would make the two arms
                        // differ in more than the one variable being changed.
                        if self.helpers.aastore == 0 {
                            // An unwired helper table (only the unit-test
                            // sentinel tables leave this zero). Refuse the
                            // method rather than emit a call to address 0 --
                            // and, more to the point, rather than fall through
                            // to the inline sequence, which with the barrier
                            // armed is precisely the defect this gate exists to
                            // avoid. Failing closed is the only correct answer.
                            crate::note_jit_bail_site_at(
                                "aastore-zgc-barrier-no-helper",
                                pc,
                                0x53,
                            );
                            return false;
                        }
                        AASTORE_ZGC_GATE_FALLBACKS
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // Args: (vm_ptr, array_ptr, index, val). Loaded in
                        // ARG_REGS order after `flush_scratch_registers`, so
                        // every source is a frame slot (or a callee-saved
                        // register, which is never an `ARG_REGS` member) and no
                        // load can clobber a later one's source on either ABI.
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.load_slot_to_reg(ARG_REGS[1], array_slot);
                        self.load_slot_to_reg(ARG_REGS[2], index_slot);
                        self.load_slot_to_reg(ARG_REGS[3], val_slot);
                        // `jit_aastore` builds an ArrayStoreException on its
                        // refusal path, so it allocates: spill and publish an
                        // oop map exactly as the inline path does around
                        // `aastore_type_check`.
                        self.emit_pre_safepoint_spill();
                        self.emit_call_absolute(self.helpers.aastore);
                        self.emit_oop_map_for_safepoint();
                        // Deliberately NO `emit_post_invoke_exception_check`.
                        // `jit_aastore` returns `()`, so RAX is UNDEFINED on
                        // return and the `CMP RAX, i64::MIN; JE bail` that check
                        // emits would fire on whatever the helper happened to
                        // leave there. Its exceptions travel by the
                        // pending-signal channel instead
                        // (`set_jit_pending_npe_action`, `JIT_SIGNALS.aioobe`,
                        // `set_jit_pending_exception`), which the interpreter's
                        // post-JIT path drains on every return -- the documented
                        // contract of the void helper, and the reason the inline
                        // arm's `b'V'` note is careful to say that ITS RAX is a
                        // defined value.
                        //
                        // `emitted_aastore_throw` is still set: the helper
                        // reaches the ArrayStoreException through
                        // `jit_thread_mut()` exactly as `jit_aastore_type_check`
                        // does, so it needs the dispatch-aware entry for the
                        // same reason, and without it the check fails open and
                        // the illegal store proceeds.
                        self.emitted_aastore_throw = true;
                        pc += 1;
                        continue;
                    }
                    // JVMS §aastore covariance check, BEFORE anything mutates:
                    // on a refusal no element may be written and no barrier may
                    // run. `jit_aastore_type_check` answers 0 (legal) or the
                    // i64::MIN sentinel, having stashed the
                    // ArrayStoreException; `emit_post_invoke_exception_check`
                    // routes the sentinel through the same drain
                    // `jit_checkcast`'s ClassCastException uses.
                    //
                    // A null value is legal for every reference array, so it
                    // branches over the call entirely — `arr[i] = null` keeps
                    // costing a test and a not-taken jump. Everything else pays
                    // one call, which is the price of the JVMS rule; the arm
                    // already makes one (SATB) to two (card mark) helper calls.
                    self.load_slot_to_reg(RDX, val_slot);
                    self.buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
                    self.buf.emit(&[0x0F, 0x84]); // JZ rel32 -> past the call
                    let ase_skip_patch = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    // Args: (vm_ptr, array_ptr, value_ptr).
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                    self.load_slot_to_reg(ARG_REGS[1], array_slot);
                    self.load_slot_to_reg(ARG_REGS[2], val_slot);
                    // The refusal path allocates (it builds the throwable), so
                    // spill and publish an oop map exactly as `checkcast` does.
                    self.emit_pre_safepoint_spill();
                    self.emit_call_absolute(self.helpers.aastore_type_check);
                    self.emit_oop_map_for_safepoint();
                    // `b'V'` is the OPCODE's return descriptor, which is what
                    // this parameter documents; it selects the plain
                    // `CMP RAX, i64::MIN; JE bail`, correct because the helper
                    // returns only `0` or the sentinel — RAX here is a defined
                    // value, not the undefined RAX a `-> ()` helper leaves.
                    self.emit_post_invoke_exception_check(b'V');
                    self.emitted_aastore_throw = true;
                    {
                        let here = self.buf.pos() as i32;
                        let rel = here - (ase_skip_patch as i32 + 4);
                        self.buf.try_patch_i32(ase_skip_patch, rel).ok();
                    }
                    // The call clobbers the scratch registers; re-establish
                    // RAX=array / RCX=index for the SATB load below.
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-7 fix (CRIT, UAF in JIT): SATB pre-write barrier.
                    // Inline-load the OLD reference at the slot and pipe it
                    // through `jit_satb_pre_write_barrier(vm_ptr, old_ref)`
                    // BEFORE the inline store overwrites it. The helper
                    // short-circuits via a single Acquire load when no
                    // concurrent mark cycle is in flight (`SatbQueue::
                    // is_active() == false`), so the steady-state cost is
                    // just an inline load + a not-taken-branch call. Without
                    // this, a still-live reference overwritten by JIT code
                    // during concurrent marking would be silently dropped by
                    // the marker → use-after-free on the next mixed
                    // evacuation (audit: history/round7-gc.md §1).
                    //
                    // Save RAX (array) / RCX (index) into argument registers
                    // first since `emit_ref_aload_regs` clobbers RAX with
                    // the loaded value.
                    self.emit_ref_aload_regs(); // RAX = OLD ref value
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                    self.emit_mov_reg_reg(ARG_REGS[1], RAX);
                    self.emit_call_absolute(self.helpers.satb_pre_write_barrier);
                    // Reload array / index / new value (helper call may have
                    // clobbered scratch registers including RAX, RCX, RDX).
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    self.load_slot_to_reg(RDX, val_slot);
                    // Inline store: MOV QWORD [RAX + RCX*8 + HEADER_SIZE], RDX
                    self.emit_ref_astore_regs();
                    // Post-store publication. Generational GC exposes a stable
                    // atomic card map, so RAX=array/RDX=value can mark it
                    // inline without a helper transition. G1/ZGC retain their
                    // collector-specific helper.
                    if self.inline_card_mark_available() {
                        self.emit_inline_card_mark_regs(RAX, RDX);
                    } else {
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.load_slot_to_reg(ARG_REGS[1], array_slot);
                        self.load_slot_to_reg(ARG_REGS[2], val_slot);
                        self.emit_call_absolute(self.helpers.write_barrier);
                    }
                    pc += 1;
                }

                // lastore — store long to long[] array (inline, 8 bytes/element)
                0x50 => {
                    let val_slot = self.pop_stack();
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-8 CRIT fix: NPE on null array (JVMS §lastore).
                    self.emit_null_check_array_store_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.load_slot_to_reg(RDX, val_slot);
                    self.emit_long_astore_regs();
                    pc += 1;
                }

                // fastore — store float to float[] array (inline, 4 bytes/element)
                0x51 => {
                    let val_slot = self.pop_stack();
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-8 CRIT fix: NPE on null array (JVMS §fastore).
                    self.emit_null_check_array_store_at(code, pc);
                    self.emit_bounds_check(pc);
                    match val_slot {
                        StackSlot::Xmm(xmm) => {
                            // MOVSS [RAX + RCX*4 + HEADER_SIZE], XMMn
                            // F3 [REX] 0F 11 ModRM SIB disp8
                            let rex_r = if xmm >= 8 { 0x04u8 } else { 0 };
                            self.buf.emit_byte(0xF3);
                            if rex_r != 0 {
                                self.buf.emit_byte(0x40 | rex_r);
                            }
                            self.buf.emit_byte(0x0F);
                            self.buf.emit_byte(0x11);
                            self.buf.emit_byte(0x44 | ((xmm & 7) << 3)); // ModRM: mod=01, reg=xmm, r/m=SIB
                            self.buf.emit_byte(0x88); // SIB: scale=2(*4), index=RCX, base=RAX
                            self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
                        }
                        _ => {
                            self.load_slot_to_reg(RDX, val_slot);
                            self.emit_int_astore_regs();
                        }
                    }
                    pc += 1;
                }

                // dastore — store double to double[] array (inline, 8 bytes/element)
                0x52 => {
                    let val_slot = self.pop_stack();
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-8 CRIT fix: NPE on null array (JVMS §dastore).
                    self.emit_null_check_array_store_at(code, pc);
                    self.emit_bounds_check(pc);
                    // Optimize: if value is in XMM, use MOVSD to store directly to memory
                    match val_slot {
                        StackSlot::Xmm(xmm) => {
                            // MOVSD [RAX + RCX*8 + HEADER_SIZE], XMMn
                            // F2 [REX] 0F 11 ModRM SIB disp8
                            let rex_r = if xmm >= 8 { 0x04u8 } else { 0 };
                            self.buf.emit_byte(0xF2);
                            if rex_r != 0 {
                                self.buf.emit_byte(0x40 | rex_r);
                            }
                            self.buf.emit_byte(0x0F);
                            self.buf.emit_byte(0x11); // MOVSD store direction
                            self.buf.emit_byte(0x44 | ((xmm & 7) << 3)); // ModRM: mod=01, reg=xmm, r/m=SIB
                            self.buf.emit_byte(0xC8); // SIB: scale=3(*8), index=RCX, base=RAX
                            self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
                        }
                        _ => {
                            self.load_slot_to_reg(RDX, val_slot);
                            self.emit_long_astore_regs();
                        }
                    }
                    pc += 1;
                }

                // bastore — store byte/boolean to byte[]/boolean[] array (inline)
                0x54 => {
                    let val_slot = self.pop_stack();
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-8 CRIT fix: NPE on null array (JVMS §bastore).
                    self.emit_null_check_array_store_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.load_slot_to_reg(RDX, val_slot);
                    self.emit_byte_astore_regs();
                    pc += 1;
                }

                // castore — store char to char[] array (inline, 2 bytes/element)
                0x55 => {
                    let val_slot = self.pop_stack();
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-8 CRIT fix: NPE on null array (JVMS §castore).
                    self.emit_null_check_array_store_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.load_slot_to_reg(RDX, val_slot);
                    self.emit_short_astore_regs();
                    pc += 1;
                }

                // sastore — store short to short[] array (inline, 2 bytes/element)
                0x56 => {
                    let val_slot = self.pop_stack();
                    let index_slot = self.pop_stack();
                    let array_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, array_slot);
                    self.load_slot_to_reg(RCX, index_slot);
                    // Round-8 CRIT fix: NPE on null array (JVMS §sastore).
                    self.emit_null_check_array_store_at(code, pc);
                    self.emit_bounds_check(pc);
                    self.load_slot_to_reg(RDX, val_slot);
                    self.emit_short_astore_regs();
                    pc += 1;
                }

                // pop
                0x57 => {
                    let _ = self.pop_stack();
                    pc += 1;
                }

                // pop2
                //
                // Absent until 2026-08-17, and not a rare shape: javac emits
                // `pop2` whenever a call returning `long`/`double` is used as a
                // STATEMENT. commons-math's `PSquarePercentile$Markers` has one
                // in `adjustHeightsOfMarkers` (discarding `estimate(int)`) and
                // one in `findCellAndUpdateMinMax` (discarding a synthetic
                // `access$502` setter), and those two methods are called once
                // per `increment()` — so the whole P-square hot loop refused to
                // compile over a missing stack-height adjustment. See
                // bug-commonsmath-accuratemathtest-psquarepercentiletest-interpreter-throughput-cliff-20260816.
                //
                // The operand model holds ONE entry per VALUE, not per JVM slot,
                // so form 2 (a single category-2 value) pops once and form 1
                // (two category-1 values) pops twice. `dup2_top_cat2` answers
                // the same width question `dup2` asks, from the producing
                // instruction.
                0x58 => {
                    match self.dup2_top_cat2(code, pc) {
                        // FORM-2: one category-2 value occupies one entry.
                        Some(true) => {
                            let _ = self.pop_stack();
                        }
                        // FORM-1: two category-1 values.
                        Some(false) if self.stack.len() >= 2 => {
                            let _ = self.pop_stack();
                            let _ = self.pop_stack();
                        }
                        _ => {
                            // Unprovable top width (or a malformed FORM-1 with
                            // height < 2). Mirror `dup2`: keep the modelled
                            // height plausible for the rest of the dispatch loop
                            // so a later handler does not raise a second,
                            // misleading failure before the post-loop `failed`
                            // check discards this compilation.
                            self.fail("singlepass-codegen/pop2-unprovable-top-width");
                            for _ in 0..2 {
                                if self.stack.is_empty() {
                                    break;
                                }
                                let _ = self.pop_stack();
                            }
                        }
                    }
                    pc += 1;
                }

                // dup
                0x59 => {
                    let top = self.peek_stack();
                    // T1.1.a / EC oop-map fix: capture the source slot's oop
                    // mark so the duplicate carries it. The `self.stack.push`
                    // fast-paths below would otherwise push to `self.stack`
                    // WITHOUT a paired `stack_oop_marks` push (desyncing the
                    // two vectors), and the `push_from_rax`/`push_stack` paths
                    // push a hard-coded `false` — both leave a duplicated
                    // object reference UNMARKED in the precise oop map. A
                    // duplicated oop live across a safepoint must stay precisely
                    // mapped so a moving GC remaps it; otherwise it can decay to
                    // a stale/garbage base.
                    let top_is_oop = self.stack_oop_marks.last().copied().unwrap_or(false);
                    match top {
                        StackSlot::Frame(off) => {
                            self.emit_load_local(RAX, off);
                            self.push_from_rax();
                        }
                        StackSlot::CalleeSaved(_) => {
                            // Zero-cost: just duplicate the register reference
                            self.stack.push(top);
                            self.stack_oop_marks.push(top_is_oop);
                        }
                        StackSlot::Xmm(_) => {
                            // Zero-cost: just duplicate the XMM register reference
                            self.stack.push(top);
                            self.stack_oop_marks.push(top_is_oop);
                        }
                        StackSlot::Scratch(reg, ..) => {
                            // Scratch register holds the value — try to dup into
                            // another scratch register, else spill original to frame
                            // and push another frame copy.
                            //
                            // The duplicate needs its OWN home: the two entries
                            // are separate stack positions and a shared home
                            // would have one flush overwrite the other.
                            let avail = SCRATCH_REGS.iter().copied().find(|&sr| {
                                sr != reg
                                    && !self
                                        .stack
                                        .iter()
                                        .any(|s| matches!(s, StackSlot::Scratch(r, ..) if *r == sr))
                            });
                            let dup_home = avail.and_then(|_| self.reserve_spill_slots(1));
                            if let (Some(sr), Some(home)) = (avail, dup_home) {
                                self.emit_mov_reg_reg(sr, reg);
                                self.stack.push(StackSlot::Scratch(sr, home));
                                self.stack_oop_marks.push(top_is_oop);
                            } else {
                                // No scratch available — load to RAX and push via frame
                                self.emit_mov_reg_reg(RAX, reg);
                                if let Some(StackSlot::Frame(off)) = self.push_stack() {
                                    self.emit_store_local(off, RAX);
                                }
                            }
                        }
                    }
                    // Propagate the oop mark onto the freshly-pushed duplicate
                    // (the `push_from_rax`/`push_stack` paths pushed `false`).
                    if top_is_oop {
                        if let Some(m) = self.stack_oop_marks.last_mut() {
                            *m = true;
                        }
                    }
                    pc += 1;
                }

                // dup_x1 — `[…, b, a] → […, a, b, a]`. JVMS §6.5 guarantees both
                // operands are category-1, so unlike dup2/dup_x2 no width proof
                // is needed. javac emits this for a field post-increment used as
                // a value (`xBuf[xBufOff++] = in` in BC's GeneralDigest.update —
                // the per-byte digest hot path that kept every BC digest
                // interpreter-bound while this opcode bailed).
                //
                // Implementation: materialize ONE copy of the top value into a
                // fresh frame slot (fresh offsets only grow, so no aliasing with
                // the live `b`/`a` slots), then rotate the top three MODEL
                // entries so the copy sits below the original pair. Only the
                // copy costs instructions. The rotated entries' frame offsets
                // are momentarily non-canonical, which is fine:
                // `canonicalize_stack` resolves arbitrary offset permutations as
                // a parallel-move problem at the next branch/call boundary.
                0x5a => {
                    if dupx_codegen_disabled() || dup_x1_codegen_disabled() || self.stack.len() < 2
                    {
                        self.fail("singlepass-codegen/dup_x1-unsupported-shape");
                        let _ = self.push_stack();
                    } else {
                        let a_slot = self.peek_stack();
                        let a_oop = self.stack_oop_marks.last().copied().unwrap_or(false);
                        let before = self.stack.len();
                        self.load_slot_to_reg(RAX, a_slot);
                        self.push_from_rax(); // […, b, a, aC]
                                              // `push_from_rax` is silent when `push_stack` cannot
                                              // reserve a spill slot: it emits nothing and does NOT
                                              // grow the model. The rotate below indexes `n - 3`, so
                                              // a missed push rotates the WRONG three entries and
                                              // leaves the operand stack one short — silent wrong
                                              // code rather than a bail.
                        if self.stack.len() != before + 1 {
                            self.fail("singlepass-codegen/dup_x1-copy-not-pushed");
                            pc += 1;
                            continue;
                        }
                        if a_oop {
                            self.mark_top_as_oop();
                        }
                        let n = self.stack.len();
                        if dupx_trace() {
                            eprintln!(
                                "[DUPX1-TRACE] {} pc={} before={:?} marks={:?}",
                                self.method_key,
                                pc,
                                &self.stack[n - 3..],
                                &self.stack_oop_marks[n - 3..]
                            );
                        }
                        self.stack[n - 3..].rotate_right(1); // […, aC, b, a]
                        self.stack_oop_marks[n - 3..].rotate_right(1);
                        if dupx_trace() {
                            eprintln!(
                                "[DUPX1-TRACE] {} pc={} after ={:?} marks={:?}",
                                self.method_key,
                                pc,
                                &self.stack[n - 3..],
                                &self.stack_oop_marks[n - 3..]
                            );
                        }
                        if dupx_eager_canon() {
                            self.canonicalize_stack();
                        }
                    }
                    pc += 1;
                }

                // dup_x2 — FORM-1 `[…, c, b, a] → […, a, c, b, a]` (all three
                // category-1) vs FORM-2 `[…, w, a] → […, a, w, a]` (w is a
                // category-2 long/double = ONE slot in this model). The top is
                // category-1 in BOTH forms; what decides the shape is the width
                // of the entry BELOW it — two entries deep for FORM-2, three for
                // FORM-1 — and that width is exactly what this backend's compact
                // operand model does not carry.
                //
                // Two independent witnesses answer it, and either alone suffices:
                //
                //   * `stack_entry_categories` — the `x64::stack_kinds` forward
                //     analysis, admitted only when its depth and per-entry
                //     ref-ness agree with the emitter's own model and, for the
                //     top entry, with `dup2_top_cat2`'s wholly independent
                //     peephole. This is the same second-entry oracle `dup2_x2`
                //     (0x5e) already uses, and it is what lifts the restriction
                //     this arm used to carry.
                //   * the NEXT opcode is a category-1 array store — then the
                //     verifier guarantees the top three slots are
                //     `[arrayref, index, cat1-value]`, i.e. FORM-1. Kept as the
                //     fallback for methods whose kind analysis poisons: it is
                //     javac's `++z[i]` / `--z[i]` value-producing pattern, which
                //     is BC's `Nat.inc`/`Nat.dec` DRBG block-counter helpers
                //     re-running the whole compile pipeline 35 923× in one
                //     crypto-prng suite run.
                //
                // What the analysis adds over the peephole is the POST-increment
                // idiom `z[i]++` / `arr[n[0]++] = v`, where the `dup_x2` is
                // followed by `iconst_1; iadd; iastore` rather than by the store
                // itself. That is the shape that kept
                // `HibfixComposeProbe2.chain` `ineligible-by-policy` at pc=15 —
                // see
                // `performance/completablefuture-composition-force-interpreted-by-a-stale-forkjointask-blocklist-FIXED-20260827.md`.
                0x5b => {
                    let cats = self.stack_entry_categories(pc);
                    let peephole_top = self.dup2_top_cat2(code, pc);
                    // Insertion depth in MODEL entries, or `None` for a form
                    // this compile cannot prove.
                    let depth = cats
                        .as_ref()
                        .and_then(|cats| {
                            let n = cats.len();
                            let top = (*cats.get(n.checked_sub(1)?)?)?;
                            // Third opinion: when the peephole answers for the
                            // top it must agree. A disagreement means one of two
                            // independent analyses is wrong; use neither.
                            if matches!(peephole_top, Some(p) if p != top) {
                                return None;
                            }
                            if top {
                                // No legal `dup_x2` form has a category-2 top.
                                return None;
                            }
                            let second = (*cats.get(n.checked_sub(2)?)?)?;
                            if second {
                                Some(2usize) // FORM-2
                            } else {
                                // FORM-1 additionally requires v3 category-1.
                                let third = (*cats.get(n.checked_sub(3)?)?)?;
                                if third {
                                    None
                                } else {
                                    Some(3usize)
                                }
                            }
                        })
                        .or_else(|| {
                            let next_is_cat1_astore = pc + 1 < code_len
                                && matches!(code[pc + 1], 0x4f | 0x51 | 0x53 | 0x54 | 0x55 | 0x56);
                            next_is_cat1_astore.then_some(3usize)
                        });
                    let disabled = dupx_codegen_disabled() || dup_x2_codegen_disabled();
                    match depth {
                        Some(depth) if !disabled && self.stack.len() >= depth => {
                            let a_slot = self.peek_stack();
                            let a_oop = self.stack_oop_marks.last().copied().unwrap_or(false);
                            let before = self.stack.len();
                            self.load_slot_to_reg(RAX, a_slot);
                            self.push_from_rax(); // […, c, b, a, aC]
                                                  // `push_from_rax` is SILENT when it cannot reserve a
                                                  // spill slot: it emits nothing and does NOT grow the
                                                  // model, and the rotate below would then reorder the
                                                  // WRONG entries and leave the operand stack one
                                                  // short — silent wrong code rather than a bail. The
                                                  // same guard has been in `dup_x1`/`dup2_x1`/
                                                  // `dup2_x2` since they were written; this arm was
                                                  // the one missing it.
                            if self.stack.len() != before + 1 {
                                self.fail("singlepass-codegen/dup_x2-copy-not-pushed");
                                pc += 1;
                                continue;
                            }
                            if a_oop {
                                self.mark_top_as_oop();
                            }
                            let n = self.stack.len();
                            let window = depth + 1;
                            self.stack[n - window..].rotate_right(1); // […, aC, c, b, a]
                            self.stack_oop_marks[n - window..].rotate_right(1);
                            if dupx_eager_canon() {
                                self.canonicalize_stack();
                            }
                        }
                        _ => {
                            // Unprovable form (or the kill switch) — stay
                            // interpreted; placeholder keeps the model height
                            // plausible until the post-loop `failed` check.
                            self.fail("singlepass-codegen/dup_x2-unprovable-form");
                            let _ = self.push_stack();
                        }
                    }
                    pc += 1;
                }

                // dup2 — FORM-1 (`[…, a, b] → […, a, b, a, b]`, two category-1
                // values) or FORM-2 (`[…, w] → […, w, w]`, one category-2
                // long/double = a single slot in this model). The form is
                // decided by the top operand's width = the result width of the
                // instruction producing it (`dup2_top_cat2`, which reads the
                // immediately-preceding op + resolved field/invoke metadata).
                // FORM-2 is exactly `dup` of the one slot. If the width can't be
                // proven locally, bail to the interpreter rather than risk a
                // category miscompile (the historic FORM-1-on-cat-2 hard abort).
                // This is the codegen-side replacement for the
                // `dup2_category_safe` reject gate, which over-rejected FORM-1
                // methods (regressing bintrees18) because the CP-less scan could
                // not resolve a `<getfield/invoke>; dup2` top to category-1.
                0x5c => {
                    match self.dup2_top_cat2(code, pc) {
                        Some(true) => {
                            // FORM-2: duplicate the single category-2 top slot.
                            self.emit_dup_top_slot();
                        }
                        Some(false) if self.stack.len() >= 2 => {
                            // FORM-1: duplicate the top two (category-1) slots.
                            let len = self.stack.len();
                            let a = self.stack[len - 2]; // deeper
                            let b = self.stack[len - 1]; // top
                                                         // EC oop-map fix: carry the two source oop marks onto
                                                         // the two duplicated entries (push_from_rax pushes `false`).
                            let ml = self.stack_oop_marks.len();
                            let a_oop = self
                                .stack_oop_marks
                                .get(ml.wrapping_sub(2))
                                .copied()
                                .unwrap_or(false);
                            let b_oop = self
                                .stack_oop_marks
                                .get(ml.wrapping_sub(1))
                                .copied()
                                .unwrap_or(false);
                            self.load_slot_to_reg(RAX, a);
                            self.push_from_rax();
                            if a_oop {
                                if let Some(m) = self.stack_oop_marks.last_mut() {
                                    *m = true;
                                }
                            }
                            self.load_slot_to_reg(RAX, b);
                            self.push_from_rax();
                            if b_oop {
                                if let Some(m) = self.stack_oop_marks.last_mut() {
                                    *m = true;
                                }
                            }
                        }
                        _ => {
                            // Unprovable top width (or a malformed FORM-1 with
                            // height < 2) — stay interpreted. Push two
                            // placeholders so downstream opcode handlers keep a
                            // plausible stack height until the post-loop `failed`
                            // check discards this compilation.
                            self.fail("singlepass-codegen/dup2-unprovable-top-width");
                            let _ = self.push_stack();
                            let _ = self.push_stack();
                        }
                    }
                    pc += 1;
                }

                // dup2_x1 — FORM-2 `[…, b, w] → […, w, b, w]` (w category-2 =
                // ONE entry in this value model, b category-1) vs FORM-1
                // `[…, c, b, a] → […, b, a, c, b, a]` (all three category-1).
                //
                // Both are lowered now, and each has its own witness:
                //
                //   * FORM-2 needs only the TOP's width. A VERIFIED `dup2_x1`
                //     whose top is category-2 cannot be FORM-1 (that form is all
                //     category-1), and JVMS requires its value2 to be
                //     category-1 — a category-2 second operand would have had to
                //     be `dup2_x2`. So `dup2_top_cat2` answering `true` settles
                //     the shape on its own, and that peephole is kept as the
                //     fallback for methods whose kind analysis poisons.
                //   * FORM-1 duplicates TWO entries and so needs the widths of
                //     the three entries under the dup — `stack_entry_categories`
                //     (the `x64::stack_kinds` forward analysis, cross-checked
                //     against the emitter's depth, its per-entry oop marks and
                //     the peephole's opinion of the top). Same oracle, same
                //     admission rules, as `dup_x2` and `dup2_x2`.
                //
                // What FORM-2 unsticks: javac emits `dup2_x1` for
                // `return this.field = value;` on a long/double field, which is
                // every synthetic outer-class setter of a `double` field.
                // commons-math's `PSquarePercentile$Marker.access$502` is one,
                // reached from the P-square min/max update path. FORM-1 is the
                // same statement over a category-1 field, and the `map[k] = v`
                // shapes that leave `[map, key, value]` on the stack.
                0x5d => {
                    let cats = self.stack_entry_categories(pc);
                    let peephole_top = self.dup2_top_cat2(code, pc);
                    // (entries duplicated, insertion depth in entries).
                    let shape = cats
                        .as_ref()
                        .and_then(|cats| {
                            let n = cats.len();
                            let top = (*cats.get(n.checked_sub(1)?)?)?;
                            // Third opinion, as in `dup_x2`/`dup2_x2`.
                            if matches!(peephole_top, Some(p) if p != top) {
                                return None;
                            }
                            let second = (*cats.get(n.checked_sub(2)?)?)?;
                            if top {
                                // FORM-2 — JVMS requires value2 category-1.
                                if second {
                                    None
                                } else {
                                    Some((1usize, 2usize))
                                }
                            } else {
                                // FORM-1 — all three category-1.
                                if second {
                                    return None;
                                }
                                let third = (*cats.get(n.checked_sub(3)?)?)?;
                                if third {
                                    None
                                } else {
                                    Some((2usize, 3usize))
                                }
                            }
                        })
                        .or_else(|| {
                            (peephole_top == Some(true) && self.stack.len() >= 2)
                                .then_some((1usize, 2usize))
                        });
                    let disabled = dupx_codegen_disabled() || dup_x1_codegen_disabled();
                    match shape {
                        Some((dup_entries, depth)) if !disabled && self.stack.len() >= depth => {
                            // Same emit as `dup2_x2`: materialize the copies into
                            // fresh frame slots deepest-first so the pushed group
                            // ends up in operand order, then rotate the top
                            // `depth + dup_entries` MODEL entries right by
                            // `dup_entries` to slide the copies underneath.
                            let n0 = self.stack.len();
                            let mut ok = true;
                            for k in (0..dup_entries).rev() {
                                let src = self.stack[n0 - 1 - k];
                                let src_oop = self.stack_oop_marks[n0 - 1 - k];
                                let before = self.stack.len();
                                self.load_slot_to_reg(RAX, src);
                                self.push_from_rax();
                                // `push_from_rax` is SILENT when it cannot
                                // reserve a spill slot — it emits nothing and
                                // does not grow the model, and the rotate below
                                // would then reorder the wrong entries.
                                if self.stack.len() != before + 1 {
                                    self.fail("singlepass-codegen/dup2_x1-copy-not-pushed");
                                    ok = false;
                                    break;
                                }
                                if src_oop {
                                    self.mark_top_as_oop();
                                }
                            }
                            if ok {
                                let n = self.stack.len();
                                let window = depth + dup_entries;
                                self.stack[n - window..].rotate_right(dup_entries);
                                self.stack_oop_marks[n - window..].rotate_right(dup_entries);
                                if dupx_eager_canon() {
                                    self.canonicalize_stack();
                                }
                            }
                        }
                        _ => {
                            self.fail("singlepass-codegen/dup2_x1-unprovable-form");
                            let _ = self.push_stack();
                        }
                    }
                    pc += 1;
                }

                // dup2_x2 — the last category-dependent stack shuffle x64
                // did not lower. `jit_scan` has always ADMITTED it (it just
                // advances `pc`), so before this arm existed the method reached
                // the dispatch loop's `_ =>` catch-all and lost its compilation
                // for the life of the process, with the refusal attributed to
                // an arm that names nothing. See
                // fixed-suite-bugs/jit/dup2_x2-is-scan-admitted-but-lowered-by-neither-x64-backend-20260817-FIXED.md.
                //
                // Four JVMS forms. In this backend's operand model — one entry
                // per VALUE, so a category-2 long/double is ONE entry — they
                // are four different shuffles over two, three or four entries:
                //
                //   FORM 4  v1,v2 cat-2   [v2, v1]         -> [v1, v2, v1]
                //   FORM 2  v1 cat-2      [v3, v2, v1]     -> [v1, v3, v2, v1]
                //   FORM 3  v3 cat-2      [v3, v2, v1]     -> [v2, v1, v3, v2, v1]
                //   FORM 1  all cat-1     [v4, v3, v2, v1] -> [v2, v1, v4, v3, v2, v1]
                //
                // So the TOP entry's category decides how many entries are
                // duplicated (one for a cat-2 top, two for a cat-1 pair) and the
                // entry BELOW the duplicated group decides how deep the copy is
                // inserted. `dup2_top_cat2` answers only the first question —
                // which is why this opcode waited for a second-entry oracle.
                // `stack_entry_categories` is it: the widths come from the
                // `x64::stack_kinds` forward analysis, admitted only when its
                // depth and per-entry ref-ness agree with the emitter's own
                // model AND, for the top entry, with `dup2_top_cat2`'s wholly
                // independent peephole answer.
                //
                // aarch64's arm is NOT the template: it pops four operands
                // unconditionally, which is FORM 1 only.
                0x5e => {
                    let cats = self.stack_entry_categories(pc);
                    let peephole_top = self.dup2_top_cat2(code, pc);
                    // Resolve (entries duplicated, insertion depth in entries).
                    let shape = cats.as_ref().and_then(|cats| {
                        let n = cats.len();
                        let top = (*cats.get(n.checked_sub(1)?)?)?;
                        // Third opinion: when the peephole answers for the top,
                        // it must agree. A disagreement means one of two
                        // independent analyses is wrong; use neither.
                        if matches!(peephole_top, Some(p) if p != top) {
                            return None;
                        }
                        let second = (*cats.get(n.checked_sub(2)?)?)?;
                        if top {
                            // FORM 4 (second cat-2, two entries) or FORM 2
                            // (second cat-1, three entries).
                            if second {
                                Some((1usize, 2usize))
                            } else {
                                // FORM 2 additionally requires v3 category-1;
                                // verified bytecode guarantees it, and checking
                                // costs one lookup.
                                let third = (*cats.get(n.checked_sub(3)?)?)?;
                                if third {
                                    None
                                } else {
                                    Some((1, 3))
                                }
                            }
                        } else {
                            // Two cat-1 entries duplicated. v2 is cat-1 in both
                            // remaining forms.
                            if second {
                                return None;
                            }
                            let third = (*cats.get(n.checked_sub(3)?)?)?;
                            if third {
                                Some((2, 3)) // FORM 3
                            } else {
                                // FORM 1 additionally requires v4 category-1.
                                let fourth = (*cats.get(n.checked_sub(4)?)?)?;
                                if fourth {
                                    None
                                } else {
                                    Some((2, 4))
                                }
                            }
                        }
                    });
                    let disabled = dupx_codegen_disabled() || dup2_x2_codegen_disabled();
                    match shape {
                        Some((dup_entries, depth)) if !disabled => {
                            // Materialize the copies into fresh frame slots
                            // (fresh offsets only grow, so no aliasing with the
                            // live originals), deepest-first so the pushed pair
                            // ends up in operand order, then rotate the top
                            // `depth + dup_entries` MODEL entries right by
                            // `dup_entries` to slide the copies underneath. Only
                            // the copies cost instructions; the rotate is
                            // bookkeeping that `canonicalize_stack` resolves as a
                            // parallel move at the next branch/call boundary.
                            let n0 = self.stack.len();
                            let mut ok = true;
                            for k in (0..dup_entries).rev() {
                                let src = self.stack[n0 - 1 - k];
                                let src_oop = self.stack_oop_marks[n0 - 1 - k];
                                let before = self.stack.len();
                                self.load_slot_to_reg(RAX, src);
                                self.push_from_rax();
                                // `push_from_rax` is SILENT when it cannot
                                // reserve a spill slot: it emits nothing and does
                                // not grow the model, and the rotate below would
                                // then reorder the wrong entries. Same guard as
                                // `dup_x1`/`dup2_x1`.
                                if self.stack.len() != before + 1 {
                                    self.fail("singlepass-codegen/dup2_x2-copy-not-pushed");
                                    ok = false;
                                    break;
                                }
                                if src_oop {
                                    self.mark_top_as_oop();
                                }
                            }
                            if ok {
                                let n = self.stack.len();
                                let window = depth + dup_entries;
                                self.stack[n - window..].rotate_right(dup_entries);
                                self.stack_oop_marks[n - window..].rotate_right(dup_entries);
                                if dupx_eager_canon() {
                                    self.canonicalize_stack();
                                }
                            }
                        }
                        _ => {
                            // No provable form (or the kill switch) — stay
                            // interpreted. Push two placeholders so downstream
                            // handlers keep a plausible height until the
                            // post-loop `failed` check discards this
                            // compilation, matching `dup2`.
                            self.fail("singlepass-codegen/dup2_x2-unprovable-form");
                            let _ = self.push_stack();
                            let _ = self.push_stack();
                        }
                    }
                    pc += 1;
                }

                // swap
                0x5f => {
                    // EC oop-map fix (round 2): the previous round paired the
                    // pushes with `stack_oop_marks` (fixing a desync) but pushed
                    // hard-coded `false` on the conservative-frame-sweep argument.
                    // That justification only covers `StackSlot::Frame` — the
                    // conservative scan walks frame qwords, not registers — so a
                    // register-resident oop (`StackSlot::Scratch`/`CalleeSaved`)
                    // swapped here lost its precise mark and would not be remapped
                    // by a moving GC at the next safepoint; the next deref would
                    // read stale from-space. Mirror the existing `dup`/`dup2`
                    // mark-propagation: snapshot each operand's mark BEFORE the
                    // pops (`pop_stack` discards them) and carry it onto the
                    // swapped position so the precise oop map matches the values
                    // the slots actually hold.
                    let ml = self.stack_oop_marks.len();
                    let a_oop = self
                        .stack_oop_marks
                        .get(ml.wrapping_sub(1))
                        .copied()
                        .unwrap_or(false); // top before swap
                    let b_oop = self
                        .stack_oop_marks
                        .get(ml.wrapping_sub(2))
                        .copied()
                        .unwrap_or(false); // below-top before swap
                    let a = self.pop_stack();
                    let b = self.pop_stack();
                    match (a, b) {
                        (StackSlot::Frame(off_a), StackSlot::Frame(off_b)) => {
                            // Physically exchange the two frame slots and keep
                            // each slot ENTRY at its original position, so the
                            // positions keep their original (ascending) frame
                            // offsets: below-top stays Frame(off_b) and now
                            // reads value1, top stays Frame(off_a) and reads
                            // value2. The previous code exchanged the memory
                            // but pushed the entries in (a, b) order, which
                            // re-paired each entry with its original value —
                            // the exchange and the reorder cancelled out and
                            // swap was a NO-OP for two frame-resident values.
                            self.emit_load_local(RAX, off_a);
                            self.emit_load_local(RCX, off_b);
                            self.emit_store_local(off_a, RCX);
                            self.emit_store_local(off_b, RAX);
                            self.stack.push(b);
                            self.stack_oop_marks.push(a_oop); // off_b now holds value1
                            self.stack.push(a);
                            self.stack_oop_marks.push(b_oop); // off_a now holds value2
                        }
                        _ => {
                            // Mixed or register-resident — no memory traffic;
                            // reorder the slot entries, each carrying its own
                            // value and oop mark to its new position.
                            self.stack.push(a);
                            self.stack_oop_marks.push(a_oop);
                            self.stack.push(b);
                            self.stack_oop_marks.push(b_oop);
                        }
                    }
                    // The two pops above may have reclaimed the operands' spill
                    // slots (next_spill_offset rewound below the still-live
                    // frame slots just pushed back); a later push would then be
                    // handed a live slot and clobber it. Recompute the cursor
                    // from the live stack.
                    self.reset_spills();
                    pc += 1;
                }

                // iadd
                0x60 => {
                    self.pop_to_rcx(); // b
                    self.pop_to_rax(); // a
                                       // ADD eax, ecx (32-bit, wrapping)
                    self.buf.emit(&[0x01, 0xC8]); // add eax, ecx
                                                  // Sign-extend eax to rax for consistency
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                    self.push_from_rax();
                    pc += 1;
                }

                // ladd
                0x61 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    // ADD rax, rcx (64-bit)
                    self.rex_w();
                    self.buf.emit(&[0x01, 0xC8]); // add rax, rcx
                    self.push_from_rax();
                    pc += 1;
                }

                // fadd
                0x62 => {
                    self.emit_float_binop(0x58); // ADDSS
                    pc += 1;
                }

                // dadd
                0x63 => {
                    self.emit_double_binop(0x58); // ADDSD
                    pc += 1;
                }

                // isub
                0x64 => {
                    self.pop_to_rcx(); // b
                    self.pop_to_rax(); // a
                                       // SUB eax, ecx
                    self.buf.emit(&[0x29, 0xC8]); // sub eax, ecx
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                    self.push_from_rax();
                    pc += 1;
                }

                // lsub
                0x65 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.rex_w();
                    self.buf.emit(&[0x29, 0xC8]); // sub rax, rcx
                    self.push_from_rax();
                    pc += 1;
                }

                // fsub
                0x66 => {
                    self.emit_float_binop(0x5C); // SUBSS
                    pc += 1;
                }

                // dsub
                0x67 => {
                    self.emit_double_binop(0x5C); // SUBSD
                    pc += 1;
                }

                // imul
                0x68 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    // IMUL eax, ecx
                    self.buf.emit(&[0x0F, 0xAF, 0xC1]); // imul eax, ecx
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                    self.push_from_rax();
                    pc += 1;
                }

                // lmul
                0x69 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    // IMUL rax, rcx
                    self.rex_w();
                    self.buf.emit(&[0x0F, 0xAF, 0xC1]); // imul rax, rcx
                    self.push_from_rax();
                    pc += 1;
                }

                // fmul
                0x6a => {
                    self.emit_float_binop(0x59); // MULSS
                    pc += 1;
                }

                // dmul — with strength reduction: dmul by 2.0 → dadd self
                0x6b => {
                    if self.fp_strength_reduction_pcs.contains(&pc) {
                        // Pattern: <value>, ldc2_w 2.0, dmul
                        // Stack: [value, 2.0] → pop 2.0, emit ADDSD value, value
                        let _two = self.pop_stack(); // discard the 2.0 constant
                        let val = self.pop_stack();
                        self.flush_xmm0_slots();
                        // Load value into XMM0
                        match val {
                            StackSlot::Xmm(xmm) if xmm != 0 => {
                                self.emit_movsd_xmm_xmm(0, xmm);
                            }
                            StackSlot::Xmm(0) => {} // already there
                            _ => {
                                self.load_slot_to_reg(RAX, val);
                                self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]); // MOVQ XMM0, RAX
                            }
                        }
                        // ADDSD XMM0, XMM0 — doubles the value
                        self.buf.emit(&[0xF2, 0x0F, 0x58, 0xC0]);
                        self.stack_push(StackSlot::Xmm(0), false);
                    } else {
                        self.emit_double_binop(0x59); // MULSD
                    }
                    pc += 1;
                }

                // idiv — JVMS-compliant: guards divide-by-zero (→ deopt to
                // throw ArithmeticException) and INT_MIN / -1 (→ INT_MIN).
                // See `emit_safe_idiv` for the guard sequence.
                0x6c => {
                    self.pop_to_rcx(); // divisor
                    self.pop_to_rax(); // dividend
                    self.emit_safe_idiv(pc, /*is_64bit*/ false, /*is_rem*/ false);
                    self.push_from_rax();
                    pc += 1;
                }

                // ldiv — JVMS-compliant guards; LONG_MIN / -1 returns LONG_MIN.
                0x6d => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.emit_safe_idiv(pc, /*is_64bit*/ true, /*is_rem*/ false);
                    self.push_from_rax();
                    pc += 1;
                }

                // fdiv
                0x6e => {
                    self.emit_float_binop(0x5E); // DIVSS
                    pc += 1;
                }

                // ddiv
                0x6f => {
                    self.emit_double_binop(0x5E); // DIVSD
                    pc += 1;
                }

                // irem — JVMS-compliant guards; INT_MIN % -1 returns 0.
                0x70 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.emit_safe_idiv(pc, /*is_64bit*/ false, /*is_rem*/ true);
                    self.push_from_rax();
                    pc += 1;
                }

                // lrem — JVMS-compliant guards; LONG_MIN % -1 returns 0.
                0x71 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.emit_safe_idiv(pc, /*is_64bit*/ true, /*is_rem*/ true);
                    self.push_from_rax();
                    pc += 1;
                }

                // ineg
                0x74 => {
                    self.pop_to_rax();
                    // NEG eax
                    self.buf.emit(&[0xF7, 0xD8]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                    self.push_from_rax();
                    pc += 1;
                }

                // lneg
                0x75 => {
                    self.pop_to_rax();
                    self.rex_w();
                    self.buf.emit(&[0xF7, 0xD8]); // NEG rax
                    self.push_from_rax();
                    pc += 1;
                }

                // fneg — flip sign bit of float (bit 31)
                0x76 => {
                    self.pop_to_rax();
                    // XOR EAX, 0x80000000 (flip sign bit, zeroes upper 32 bits)
                    self.buf.emit_byte(0x35); // XOR EAX, imm32
                    self.buf.emit(&0x8000_0000u32.to_le_bytes());
                    self.push_from_rax();
                    pc += 1;
                }

                // dneg — flip sign bit of double (bit 63)
                0x77 => {
                    self.pop_to_rax();
                    // BTC RAX, 63 — complement bit 63
                    self.rex_w();
                    self.buf.emit(&[0x0F, 0xBA, 0xF8, 63]); // BTC r/m64, imm8
                    self.push_from_rax();
                    pc += 1;
                }

                // ishl
                0x78 => {
                    self.pop_to_rcx(); // shift count (low 5 bits)
                    self.pop_to_rax();
                    // SHL eax, cl
                    self.buf.emit(&[0xD3, 0xE0]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]); // movsxd
                    self.push_from_rax();
                    pc += 1;
                }

                // lshl
                0x79 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.rex_w();
                    self.buf.emit(&[0xD3, 0xE0]); // SHL rax, cl
                    self.push_from_rax();
                    pc += 1;
                }

                // ishr
                0x7a => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    // SAR eax, cl
                    self.buf.emit(&[0xD3, 0xF8]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    pc += 1;
                }

                // lshr
                0x7b => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.rex_w();
                    self.buf.emit(&[0xD3, 0xF8]); // SAR rax, cl
                    self.push_from_rax();
                    pc += 1;
                }

                // iushr
                0x7c => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    // SHR eax, cl
                    self.buf.emit(&[0xD3, 0xE8]);
                    // Zero-extend eax to rax (automatic with 32-bit ops on x64)
                    self.push_from_rax();
                    pc += 1;
                }

                // lushr
                0x7d => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.rex_w();
                    self.buf.emit(&[0xD3, 0xE8]); // SHR rax, cl
                    self.push_from_rax();
                    pc += 1;
                }

                // iand
                0x7e => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.buf.emit(&[0x21, 0xC8]); // AND eax, ecx
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]); // movsxd
                    self.push_from_rax();
                    pc += 1;
                }

                // land
                0x7f => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.rex_w();
                    self.buf.emit(&[0x21, 0xC8]); // AND rax, rcx
                    self.push_from_rax();
                    pc += 1;
                }

                // ior
                0x80 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.buf.emit(&[0x09, 0xC8]); // OR eax, ecx
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    pc += 1;
                }

                // lor
                0x81 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.rex_w();
                    self.buf.emit(&[0x09, 0xC8]); // OR rax, rcx
                    self.push_from_rax();
                    pc += 1;
                }

                // ixor
                0x82 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.buf.emit(&[0x31, 0xC8]); // XOR eax, ecx
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    pc += 1;
                }

                // lxor
                0x83 => {
                    self.pop_to_rcx();
                    self.pop_to_rax();
                    self.rex_w();
                    self.buf.emit(&[0x31, 0xC8]); // XOR rax, rcx
                    self.push_from_rax();
                    pc += 1;
                }

                // iinc
                0x84 => {
                    let idx = code[pc + 1] as usize; // Widening: always safe
                    let inc = code[pc + 2] as i8 as i32; // Widening: always safe
                    self.emit_iinc_local(idx, inc);
                    pc += 3;
                }

                // wide (JVMS §6.5) — the two-byte-index prefix. See `jit_scan`'s
                // own `0xC4` arm for what one un-emittable `iinc_w` cost, and
                // why the scan and this walk are changed together: a form the
                // scan admits and this arm cannot emit is a whole-method
                // compile bail, and a form this arm emits that the scan refuses
                // is unreachable.
                //
                // The load and store bodies mirror the narrow arms above
                // one-for-one — only the operand decode differs — and `iinc`
                // shares its emitter outright rather than keeping a second copy
                // of the ADD/MOVSXD sequence.
                0xc4 => {
                    let wop = code[pc + 1];
                    // Cast: a JVM local index; `max_locals` bounds it.
                    let idx = u16::from_be_bytes([code[pc + 2], code[pc + 3]]) as usize;
                    match wop {
                        // wide iload / lload / fload / dload / aload
                        0x15..=0x19 => {
                            let is_aload = wop == 0x19;
                            // fload/dload may have an XMM-allocated local.
                            if matches!(wop, 0x17 | 0x18) {
                                if let Some(xmm) = self.xmm_for_local(idx) {
                                    // FP value — never an oop.
                                    self.stack_push(StackSlot::Xmm(xmm), false);
                                    pc += 4;
                                    continue;
                                }
                            }
                            if let Some(local_reg) = self.reg_for_local(idx) {
                                self.stack_push(StackSlot::CalleeSaved(local_reg), is_aload);
                            } else {
                                let off = self.local_offset(idx);
                                self.emit_load_local(RAX, off);
                                self.push_from_rax();
                                if is_aload {
                                    self.mark_top_as_oop();
                                }
                            }
                            pc += 4;
                        }
                        // wide istore / lstore / fstore / dstore / astore
                        0x36..=0x3a => {
                            if matches!(wop, 0x38 | 0x39) {
                                if let Some(dst_xmm) = self.xmm_for_local(idx) {
                                    let slot = self.pop_stack();
                                    match slot {
                                        StackSlot::Xmm(src) if src == dst_xmm => {}
                                        StackSlot::Xmm(src) => {
                                            if wop == 0x39 {
                                                self.emit_movsd_xmm_xmm(dst_xmm, src);
                                            } else {
                                                self.emit_movss_xmm_xmm(dst_xmm, src);
                                            }
                                        }
                                        _ => {
                                            self.load_slot_to_reg(RAX, slot);
                                            self.emit_movq_xmm_from_rax(dst_xmm);
                                        }
                                    }
                                    pc += 4;
                                    continue;
                                }
                            }
                            self.pop_to_rax();
                            if let Some(local_reg) = self.reg_for_local(idx) {
                                self.invalidate_callee_saved(local_reg);
                                self.emit_mov_reg_reg(local_reg, RAX);
                            } else {
                                let off = self.local_offset(idx);
                                self.emit_store_local(off, RAX);
                            }
                            pc += 4;
                        }
                        // wide iinc — the SIGNED 16-bit constant is the whole
                        // reason javac emits this prefix in netty's codecs
                        // (`iinc_w 18, -255`).
                        0x84 => {
                            // Widening: i16 -> i32, sign preserved.
                            let inc = i16::from_be_bytes([code[pc + 4], code[pc + 5]]) as i32;
                            self.emit_iinc_local(idx, inc);
                            pc += 6;
                        }
                        // `wide ret` and anything else — `jit_scan` refuses the
                        // same set, so this is unreachable; bail rather than
                        // emit for a form neither walk models.
                        _ => {
                            self.fail("singlepass-codegen/wide-unsupported-opcode");
                            return false;
                        }
                    }
                }

                // i2l — sign-extend int to long
                0x85 => {
                    self.pop_to_rax();
                    // movsxd rax, eax (sign-extend 32→64)
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    pc += 1;
                }

                // i2f — int to float
                0x86 => {
                    self.flush_xmm0_slots();
                    self.pop_to_rax();
                    // CVTSI2SS XMM0, EAX: F3 0F 2A C0
                    self.buf.emit(&[0xF3, 0x0F, 0x2A, 0xC0]);
                    self.stack_push(StackSlot::Xmm(0), false);
                    pc += 1;
                }

                // i2d — int to double
                0x87 => {
                    self.flush_xmm0_slots();
                    self.pop_to_rax();
                    // CVTSI2SD XMM0, EAX: F2 0F 2A C0
                    self.buf.emit(&[0xF2, 0x0F, 0x2A, 0xC0]);
                    self.stack_push(StackSlot::Xmm(0), false);
                    pc += 1;
                }

                // l2i — truncate long to int
                0x88 => {
                    self.pop_to_rax();
                    // Just keep lower 32 bits, sign-extend
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                    self.push_from_rax();
                    pc += 1;
                }

                // l2f — long to float
                0x89 => {
                    self.flush_xmm0_slots();
                    self.pop_to_rax();
                    // CVTSI2SS XMM0, RAX: F3 48 0F 2A C0
                    self.buf.emit(&[0xF3, 0x48, 0x0F, 0x2A, 0xC0]);
                    self.stack_push(StackSlot::Xmm(0), false);
                    pc += 1;
                }

                // l2d — long to double
                0x8a => {
                    self.flush_xmm0_slots();
                    self.pop_to_rax();
                    // CVTSI2SD XMM0, RAX: F2 48 0F 2A C0
                    self.buf.emit(&[0xF2, 0x48, 0x0F, 0x2A, 0xC0]);
                    self.stack_push(StackSlot::Xmm(0), false);
                    pc += 1;
                }

                // f2i — float to int (truncate toward zero, NaN→0, overflow→MAX/MIN)
                0x8b => {
                    self.pop_to_rax();
                    // MOVD XMM0, EAX: 66 0F 6E C0
                    self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]);
                    // CVTTSS2SI EAX, XMM0: F3 0F 2C C0
                    self.buf.emit(&[0xF3, 0x0F, 0x2C, 0xC0]);
                    self.emit_fp_to_int_nan_fixup(false, false);
                    // Sign-extend EAX to RAX
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                    self.push_from_rax();
                    pc += 1;
                }

                // f2l — float to long (truncate toward zero, NaN→0, overflow→MAX/MIN)
                0x8c => {
                    self.pop_to_rax();
                    // MOVD XMM0, EAX: 66 0F 6E C0
                    self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]);
                    // CVTTSS2SI RAX, XMM0: F3 48 0F 2C C0
                    self.buf.emit(&[0xF3, 0x48, 0x0F, 0x2C, 0xC0]);
                    self.emit_fp_to_int_nan_fixup(false, true);
                    self.push_from_rax();
                    pc += 1;
                }

                // f2d — float to double
                0x8d => {
                    self.flush_xmm0_slots();
                    self.pop_to_rax();
                    // MOVD XMM0, EAX: 66 0F 6E C0
                    self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]);
                    // CVTSS2SD XMM0, XMM0: F3 0F 5A C0
                    self.buf.emit(&[0xF3, 0x0F, 0x5A, 0xC0]);
                    self.stack_push(StackSlot::Xmm(0), false);
                    pc += 1;
                }

                // d2i — double to int (truncate toward zero, NaN→0, overflow→MAX/MIN)
                0x8e => {
                    self.pop_to_rax();
                    // MOVQ XMM0, RAX: 66 48 0F 6E C0
                    self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]);
                    // CVTTSD2SI EAX, XMM0: F2 0F 2C C0
                    self.buf.emit(&[0xF2, 0x0F, 0x2C, 0xC0]);
                    self.emit_fp_to_int_nan_fixup(true, false);
                    // Sign-extend EAX to RAX
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                    self.push_from_rax();
                    pc += 1;
                }

                // d2l — double to long (truncate toward zero, NaN→0, overflow→MAX/MIN)
                0x8f => {
                    self.pop_to_rax();
                    // MOVQ XMM0, RAX: 66 48 0F 6E C0
                    self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]);
                    // CVTTSD2SI RAX, XMM0: F2 48 0F 2C C0
                    self.buf.emit(&[0xF2, 0x48, 0x0F, 0x2C, 0xC0]);
                    self.emit_fp_to_int_nan_fixup(true, true);
                    self.push_from_rax();
                    pc += 1;
                }

                // d2f — double to float
                0x90 => {
                    self.flush_xmm0_slots();
                    self.pop_to_rax();
                    // MOVQ XMM0, RAX: 66 48 0F 6E C0
                    self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]);
                    // CVTSD2SS XMM0, XMM0: F2 0F 5A C0
                    self.buf.emit(&[0xF2, 0x0F, 0x5A, 0xC0]);
                    self.stack_push(StackSlot::Xmm(0), false);
                    pc += 1;
                }

                // i2b — truncate int to byte (sign-extend)
                0x91 => {
                    self.pop_to_rax();
                    // MOVSX EAX, AL — sign-extend byte to 32-bit
                    self.buf.emit(&[0x0F, 0xBE, 0xC0]);
                    // MOVSXD RAX, EAX — sign-extend to 64-bit
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    pc += 1;
                }

                // i2c — truncate int to char (zero-extend unsigned 16-bit)
                0x92 => {
                    self.pop_to_rax();
                    // MOVZX EAX, AX — zero-extend 16-bit to 32-bit
                    self.buf.emit(&[0x0F, 0xB7, 0xC0]);
                    // Upper 32 bits of RAX auto-zeroed by 32-bit op
                    self.push_from_rax();
                    pc += 1;
                }

                // i2s — truncate int to short (sign-extend)
                0x93 => {
                    self.pop_to_rax();
                    // MOVSX EAX, AX — sign-extend 16-bit to 32-bit
                    self.buf.emit(&[0x0F, 0xBF, 0xC0]);
                    // MOVSXD RAX, EAX — sign-extend to 64-bit
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    pc += 1;
                }

                // lcmp
                0x94 => {
                    self.pop_to_rcx(); // value2
                    self.pop_to_rax(); // value1
                                       // CMP rax, rcx
                    self.rex_w();
                    self.buf.emit(&[0x39, 0xC8]); // cmp rax, rcx
                                                  // Produce -1, 0, or 1 using SETG/SETL (avoids RBX).
                                                  // lcmp is a SIGNED comparison: use SETG (0F 9F),
                                                  // not SETA (0F 97, unsigned-above). With SETA, a
                                                  // negative operand reads as a huge unsigned value,
                                                  // so e.g. `ts == -6L` JIT-compiled as `lcmp; ifne`
                                                  // returned "equal" for every ts >= 0 (sign-bit
                                                  // clear) — Kafka ListOffsetsHandler computed
                                                  // request version 11 instead of 1 for normal
                                                  // timestamps. The other two lcmp sites already use
                                                  // SETG; this one was the lone unsigned outlier.
                    self.buf.emit(&[0x0F, 0x9F, 0xC0]); // SETG AL (signed)
                    self.buf.emit(&[0x0F, 0x9C, 0xC1]); // SETL CL (signed)
                    self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                    self.buf.emit(&[0x0F, 0xB6, 0xC9]); // MOVZX ECX, CL
                    self.buf.emit(&[0x29, 0xC8]); // SUB EAX, ECX
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
                    self.push_from_rax();
                    pc += 1;
                }

                // fcmpl — float compare, NaN → -1
                0x95 => {
                    self.emit_fcmp(false, false); // float, NaN→-1
                    pc += 1;
                }

                // fcmpg — float compare, NaN → 1
                0x96 => {
                    self.emit_fcmp(false, true); // float, NaN→1
                    pc += 1;
                }

                // dcmpl — double compare, NaN → -1
                0x97 => {
                    self.emit_fcmp(true, false); // double, NaN→-1
                    pc += 1;
                }

                // dcmpg — double compare, NaN → 1
                0x98 => {
                    self.emit_fcmp(true, true); // double, NaN→1
                    pc += 1;
                }

                // ifeq..ifle (0x99..0x9e) — compare int against zero
                0x99..=0x9e => {
                    self.flush_scratch_registers();
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32; // Widening: always safe
                    let target_pc = match pc.checked_add_signed(offset as isize) {
                        // Cast: address arithmetic
                        Some(t) => t,
                        None => return false, // invalid branch target
                    };
                    if target_pc <= pc {
                        self.emit_safepoint_poll();
                    }

                    // Canonicalize for forward merge points BEFORE popping the
                    // operand, so it participates in the relocation. Popping
                    // first left the operand's frame slot invisible to
                    // canonicalize_stack(), which could store a remaining
                    // register-resident slot to that same offset (register
                    // slots shift the offsets of Frame slots above them down)
                    // and clobber the operand before the TEST below read it.
                    if target_pc > pc && self.stack.len() > 1 {
                        self.canonicalize_stack();
                    }
                    let slot = self.pop_stack();
                    // TEST r32, r32 — sets ZF/SF for comparison against zero
                    let reg = self.slot_to_gpr(slot, RCX);
                    self.emit_test_r32_r32(reg);

                    let cc = match op {
                        0x99 => 0x84, // JE
                        0x9a => 0x85, // JNE
                        0x9b => 0x8C, // JL
                        0x9c => 0x8D, // JGE
                        0x9d => 0x8F, // JG
                        0x9e => 0x8E, // JLE
                        _ => unreachable!(),
                    };

                    // PGO branch prediction hint prefix (Intel Architecture Manual 2.4.4).
                    // 0x3E = DS prefix = "branch taken" hint.
                    // 0x2E = CS prefix = "branch not taken" hint.
                    if let Some(&is_taken) = self.branch_hints.get(&pc) {
                        self.buf.emit_byte(if is_taken { 0x3E } else { 0x2E });
                    }
                    self.buf.emit_byte(0x0F);
                    self.buf.emit_byte(cc);
                    let patch_offset = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                    self.forward_patches.push((patch_offset, target_pc));
                    self.record_branch_target_depth(target_pc);
                    self.reset_spills();
                    pc += 3;
                }

                // if_icmpeq..if_icmple (0x9f..0xa4)
                0x9f..=0xa4 => {
                    self.flush_scratch_registers();
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32; // Widening: always safe
                    let target_pc = match pc.checked_add_signed(offset as isize) {
                        // Cast: address arithmetic
                        Some(t) => t,
                        None => return false, // invalid branch target
                    };
                    if target_pc <= pc {
                        self.emit_safepoint_poll();
                    }

                    // Canonicalize for forward merge points BEFORE popping the
                    // operands (see ifeq..ifle above): with both operands still
                    // on the simulated stack they are relocated above every
                    // store target, so a remaining register-resident slot can
                    // no longer be stored over a popped operand's frame slot.
                    // Done before the cmov consult too — the peephole then sees
                    // frame-canonical operands in this rare deep-stack shape,
                    // which costs a reload but stays correct.
                    if target_pc > pc && self.stack.len() > 2 {
                        self.canonicalize_stack();
                    }
                    let val2 = self.pop_stack(); // value2
                    let val1 = self.pop_stack(); // value1

                    // peephole-cmov (Round-11 HIGH-3): user-written
                    // min/max pattern → CMOV. The peephole consumes
                    // the if_icmp, the fall-through iload, the goto,
                    // and the taken-side iload all at once; on hit
                    // we resume at the merge PC L2.
                    if let Some(new_pc) = self.try_cmov_minmax_peephole(code, pc, op, val1, val2) {
                        // Map the original if_icmp PC to the start of
                        // the CMOV sequence so downstream branch
                        // resolution keeps working.
                        pc = new_pc;
                        self.reset_spills();
                        continue;
                    }

                    // Emit CMP with direct reg-reg when possible
                    let r1 = self.slot_to_gpr(val1, RAX);
                    let r2 = self.slot_to_gpr(val2, RCX);
                    self.emit_cmp_r32_r32(r1, r2);

                    let cc = match op {
                        0x9f => 0x84, // JE
                        0xa0 => 0x85, // JNE
                        0xa1 => 0x8C, // JL
                        0xa2 => 0x8D, // JGE
                        0xa3 => 0x8F, // JG
                        0xa4 => 0x8E, // JLE
                        _ => unreachable!(),
                    };

                    // PGO branch prediction hint (same encoding as ifeq..ifle above).
                    if let Some(&is_taken) = self.branch_hints.get(&pc) {
                        self.buf.emit_byte(if is_taken { 0x3E } else { 0x2E });
                    }
                    self.buf.emit_byte(0x0F);
                    self.buf.emit_byte(cc);
                    let patch_offset = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                    self.forward_patches.push((patch_offset, target_pc));
                    self.record_branch_target_depth(target_pc);
                    self.reset_spills();
                    pc += 3;
                }

                // goto
                0xa7 => {
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32; // Widening: always safe
                    let target_pc = match pc.checked_add_signed(offset as isize) {
                        // Cast: address arithmetic
                        Some(t) => t,
                        None => return false, // invalid branch target
                    };
                    // Flush scratch registers before any branch -- they are
                    // caller-saved and not valid across basic block boundaries.
                    self.flush_scratch_registers();
                    // For forward branches with non-empty stack, canonicalize the
                    // stack layout so all paths to the target use the same offsets.
                    if target_pc > pc && !self.stack.is_empty() {
                        self.canonicalize_stack();
                    }

                    // Loop unrolling: if this is a back-edge of an unrollable loop,
                    // copy the native code for the loop body as extra iterations
                    let unroll_copies = if target_pc <= pc {
                        self.unroll_loops
                            .iter()
                            .find(|&&(_, be, _)| be == pc)
                            .map(|&(_, _, copies)| copies)
                    } else {
                        None
                    };

                    if let Some(extra_copies) = unroll_copies {
                        // Copy the native code from header to here for extra iterations
                        let header_native = self.pc_to_native[target_pc];
                        if header_native >= 0 {
                            let body_start = header_native as usize; // Cast: address arithmetic
                            let body_end = self.buf.pos();
                            let body_len = body_end - body_start;

                            if body_len > 0 && body_len < 4096 {
                                // Snapshot the original body bytes once
                                let body_bytes: Vec<u8> =
                                    self.buf.as_slice()[body_start..body_end].to_vec();

                                // Snapshot every patch vector entry whose
                                // native offset falls inside the original
                                // body span. Each duplicated copy needs a
                                // shifted twin for every snapshot entry so
                                // late-stage stub emission / branch
                                // resolution covers the copies too.
                                //
                                // Task #60: this is the deep follow-up that
                                // removes the previous allow-list — earlier
                                // unrollers shifted only `bounds_check_stubs`
                                // and gated bodies containing getfield /
                                // invokevirtual / new / athrow / checkcast
                                // behind a safety check. With every patch
                                // vector now shifted AND helper rel32s
                                // re-resolved AND IC slots per-clone, the
                                // duplicator handles arbitrary bytecodes.
                                let orig_patches: Vec<(usize, usize)> = self
                                    .forward_patches
                                    .iter()
                                    .filter(|&&(po, _)| po >= body_start && po < body_end)
                                    .copied()
                                    .collect();
                                let orig_bounds_stubs: Vec<(usize, usize)> = self
                                    .bounds_check_stubs
                                    .iter()
                                    .filter(|&&(po, _)| po >= body_start && po < body_end)
                                    .copied()
                                    .collect();
                                let orig_excn_stubs: Vec<(usize, usize)> = self
                                    .exception_check_stubs
                                    .iter()
                                    .filter(|&&(po, _)| po >= body_start && po < body_end)
                                    .copied()
                                    .collect();
                                // JEP 358: each entry is (action, patch_offset);
                                // filter by the offset, carry the action through.
                                let orig_nullstore_stubs: Vec<(u8, usize)> = self
                                    .null_check_store_stubs
                                    .iter()
                                    .filter(|&&(_, po)| po >= body_start && po < body_end)
                                    .copied()
                                    .collect();
                                let orig_self_calls: Vec<usize> = self
                                    .self_call_patches
                                    .iter()
                                    .filter(|&&po| po >= body_start && po < body_end)
                                    .copied()
                                    .collect();
                                let orig_deopt_stubs: Vec<(usize, usize, i64)> = self
                                    .deopt_stubs
                                    .iter()
                                    .filter(|&&(po, _, _)| po >= body_start && po < body_end)
                                    .copied()
                                    .collect();
                                let orig_jump_table_patches: Vec<(usize, usize, usize)> = self
                                    .jump_table_patches
                                    .iter()
                                    .filter(|&&(eo, tb, _)| {
                                        // Both the entry slot AND the table base
                                        // must live inside the body for the
                                        // RIP-relative arithmetic to remain
                                        // consistent under a uniform shift. In
                                        // practice tableswitch tables are emitted
                                        // immediately after the dispatch code,
                                        // so this is the common case; any entry
                                        // that straddles the boundary is left for
                                        // the late patcher (which still resolves
                                        // the *original* copy correctly).
                                        eo >= body_start
                                            && eo < body_end
                                            && tb >= body_start
                                            && tb < body_end
                                    })
                                    .copied()
                                    .collect();
                                let orig_oop_maps: Vec<crate::OopMapEntry> = self
                                    .oop_maps
                                    .iter()
                                    .filter(|e| {
                                        let off = e.native_pc_offset as usize; // Widening: u32 → usize
                                        off >= body_start && off < body_end
                                    })
                                    .cloned()
                                    .collect();
                                let orig_helper_calls: Vec<usize> = self
                                    .helper_call_patches
                                    .iter()
                                    .filter(|&&po| po >= body_start && po < body_end)
                                    .copied()
                                    .collect();
                                let orig_ic_patches: Vec<(usize, u8, usize)> = self
                                    .ic_patches
                                    .iter()
                                    .filter(|&&(po, _, _)| po >= body_start && po < body_end)
                                    .copied()
                                    .collect();

                                // Snapshot the buffer's base pointer ONCE here.
                                // `JitBuf::reserve` does not relocate after
                                // `as_ptr()` is observed (see the safety note
                                // on `emit_call_absolute`), so this base is the
                                // same address every copy will resolve against.
                                // Cast: non-negative index/count to usize
                                let buf_base = self.buf.as_ptr() as usize;

                                for _ in 0..extra_copies {
                                    let copy_start = self.buf.pos();
                                    let shift = copy_start as i32 - body_start as i32; // Cast: x86-64 immediate encoding
                                    let shift_us = shift as usize; // Cast: address arithmetic

                                    // Copy the raw bytes verbatim.
                                    self.buf.emit(&body_bytes);

                                    // Handle forward patches: internal ones (target
                                    // within the loop body) are resolved immediately
                                    // using shifted addresses; external ones are
                                    // deferred normally.
                                    for &(po, tp) in &orig_patches {
                                        let shifted_po = po + shift_us;
                                        if tp >= target_pc && tp <= pc {
                                            // Internal: resolve now using shifted target
                                            let orig_target = self.pc_to_native[tp];
                                            if orig_target >= 0 {
                                                let shifted_target = orig_target + shift;
                                                let rel = shifted_target - (shifted_po as i32 + 4); // Cast: x86-64 immediate encoding
                                                self.buf.try_patch_i32(shifted_po, rel).ok();
                                                // on Err try_patch_i32 set buf.overflowed; compile bails
                                            }
                                        } else {
                                            // External: defer to normal resolution
                                            self.forward_patches.push((shifted_po, tp));
                                        }
                                    }

                                    // Re-resolve every helper rel32 in this
                                    // copy. The duplicated bytes carry the
                                    // *original* rel32 — which, after the
                                    // shift, would land at `helper + shift`
                                    // (the N-Body Body.x SIGSEGV pattern from
                                    // CHANGELOG). Reconstruct the helper
                                    // address from the original site and
                                    // re-encode the rel32 against the copy's
                                    // call PC.
                                    for &po in &orig_helper_calls {
                                        // `po` is the offset of the 4-byte
                                        // rel32 within the buffer; the byte
                                        // after the rel32 is `po + 4`, which
                                        // is the reference point for both
                                        // the original and copied rel32
                                        // displacements. The next-PC's
                                        // *runtime* absolute address is
                                        // `buf_base + po + 4`.
                                        // Read the 4-byte rel32 immediate.
                                        // `from_le_bytes` wants an owned
                                        // `[u8; 4]`; copy out of the buffer
                                        // slice explicitly so the immutable
                                        // borrow ends before the upcoming
                                        // `patch_i32` mutable call.
                                        let mut rel_bytes = [0u8; 4];
                                        rel_bytes.copy_from_slice(&self.buf.as_slice()[po..po + 4]);
                                        let orig_rel32 = i32::from_le_bytes(rel_bytes);
                                        let orig_next_pc =
                                            buf_base.wrapping_add(po).wrapping_add(4);
                                        let helper_addr =
                                            // Widening: usize address & i32 rel32 -> i64 (no truncation; rel math)
                                            (orig_next_pc as i64).wrapping_add(orig_rel32 as i64);
                                        let copy_po = po + shift_us;
                                        let copy_next_pc =
                                            buf_base.wrapping_add(copy_po).wrapping_add(4);
                                        let delta: i128 =
                                            // Widening: i64/usize -> i128 (no truncation, for range check)
                                            (helper_addr as i128) - (copy_next_pc as i128);
                                        // Helpers reachable in ±2GB at the
                                        // original site stay reachable at the
                                        // shifted copy (the shift is at most
                                        // body_len < 4096 bytes). Truncating
                                        // to i32 is safe in practice; debug-
                                        // assert to catch any pathological
                                        // future code-cache layout.
                                        debug_assert!(
                                            // Widening: i64/usize -> i128 (no truncation, for range check)
                                            delta >= i32::MIN as i128 && delta <= i32::MAX as i128,
                                            "unrolled helper rel32 out of range",
                                        );
                                        self.buf
                                            .try_patch_i32(copy_po, delta as i32) // Cast: rel32 displacement
                                            .ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                                    }

                                    // Per-clone MIC/PIC slots. For each IC
                                    // site in the original body, mint a fresh
                                    // Box<JitMICSlot> / Box<JitPICSlot>, stash
                                    // it on the compiler so it outlives the
                                    // compiled method, and rewrite the imm64
                                    // baked into the duplicated `MOV R10,
                                    // imm64` to point at the new slot.
                                    //
                                    // Without this, every copy shares the
                                    // original slot — per-iteration cache
                                    // hits collide and miss across copies for
                                    // any receiver-type-varying loop.
                                    let mut cloned_ic_slots: HashMap<(u8, usize), i64> =
                                        HashMap::new();
                                    for &(po, kind, original_ptr) in &orig_ic_patches {
                                        let copy_po = po + shift_us;
                                        let fresh_ptr = *cloned_ic_slots
                                            .entry((kind, original_ptr))
                                            .or_insert_with(|| match kind {
                                                0 => {
                                                    let mic = Box::new(crate::JitMICSlot::new());
                                                    let p: *const crate::JitMICSlot = &*mic;
                                                    self.cloned_mic_slots.push(mic);
                                                    p as i64
                                                }
                                                1 => {
                                                    let pic = Box::new(crate::JitPICSlot::new());
                                                    let p: *const crate::JitPICSlot = &*pic;
                                                    self.cloned_pic_slots.push(pic);
                                                    p as i64
                                                }
                                                _ => {
                                                    unreachable!("unknown ic_patches kind {}", kind)
                                                }
                                            });
                                        // Overwrite the 8-byte little-endian
                                        // imm64 baked into the duplicated
                                        // `MOV R10, imm64` (10-byte form).
                                        let bytes = fresh_ptr.to_le_bytes();
                                        for i in 0..8 {
                                            self.buf.try_patch_byte(copy_po + i, bytes[i]).ok();
                                            // on Err try_patch_byte set buf.overflowed; compile bails
                                        }
                                    }

                                    // Shift bounds-check, exception-check,
                                    // null-check-store, and self-call patch
                                    // sites so the late stub emitters see
                                    // every duplicated branch.
                                    self.bounds_check_stubs.extend(
                                        orig_bounds_stubs
                                            .iter()
                                            .map(|&(po, bci)| (po + shift_us, bci)),
                                    );
                                    self.exception_check_stubs.extend(
                                        orig_excn_stubs
                                            .iter()
                                            .map(|&(po, bci)| (po + shift_us, bci)),
                                    );
                                    self.null_check_store_stubs.extend(
                                        orig_nullstore_stubs
                                            .iter()
                                            .map(|&(action, po)| (action, po + shift_us)),
                                    );
                                    self.self_call_patches
                                        .extend(orig_self_calls.iter().map(|&po| po + shift_us));
                                    // Deopt stub patches: (patch_offset, bci,
                                    // reason). bci and reason are the same
                                    // across copies (it's the same logical
                                    // safepoint, identified by JVM bci); only
                                    // the patch offset shifts. Sharing the
                                    // (bci, reason) key lets emit_deopt_stubs
                                    // coalesce the duplicated guards onto a
                                    // single shared stub.
                                    self.deopt_stubs.extend(
                                        orig_deopt_stubs
                                            .iter()
                                            .map(|&(po, bci, reason)| (po + shift_us, bci, reason)),
                                    );
                                    // Jump-table patches use RIP-relative
                                    // offsets stored as i32 from
                                    // table_base_native_offset to the target.
                                    // When the entry slot AND the table base
                                    // are both inside the body span, the
                                    // offset is shift-invariant — both move
                                    // by the same amount, so the i32 already
                                    // emitted in the duplicated bytes is
                                    // still correct. Just shift the
                                    // (entry_offset, table_base, target_pc)
                                    // tuple itself so the late patcher
                                    // re-resolves the copy.
                                    self.jump_table_patches.extend(
                                        orig_jump_table_patches.iter().map(|&(eo, tb, tpc)| {
                                            (eo + shift_us, tb + shift_us, tpc)
                                        }),
                                    );
                                    // Oop maps: each entry stashes the
                                    // native_pc_offset of the instruction
                                    // AFTER a safepoint. The GC root walker
                                    // looks up the map by PC, so duplicated
                                    // safepoints need their own shifted
                                    // entries — same frame slots, new PC.
                                    self.oop_maps.extend(orig_oop_maps.iter().map(|e| {
                                        let mut copy = e.clone();
                                        // native_pc_offset is u32; shift
                                        // is i32 but always positive
                                        // (copy_start > body_start), so
                                        // saturate-add via usize for safe
                                        // arithmetic.
                                        // Cast: non-negative index/count to usize
                                        copy.native_pc_offset = (e.native_pc_offset as usize)
                                            .wrapping_add(shift_us)
                                            as u32; // Cast: native_pc_offset width
                                        copy
                                    }));
                                    // Helper-call patches: track the
                                    // duplicated rel32 site so any future
                                    // pass that walks helper_call_patches
                                    // (e.g. a nested unroll) sees the copy.
                                    self.helper_call_patches
                                        .extend(orig_helper_calls.iter().map(|&po| po + shift_us));
                                    // IC patches: same idea — record the
                                    // shifted imm64 location with its kind
                                    // so any later pass can find it.
                                    let mut cloned_patches =
                                        Vec::with_capacity(orig_ic_patches.len());
                                    for &(po, kind, original_ptr) in &orig_ic_patches {
                                        let Some(&cloned_ptr) =
                                            cloned_ic_slots.get(&(kind, original_ptr))
                                        else {
                                            // An incomplete IC clone would leave generated
                                            // code pointing at the wrong call-site state.
                                            // Reject this compilation and fall back to the
                                            // interpreter instead of publishing unsafe code.
                                            return false;
                                        };
                                        cloned_patches.push((
                                            po + shift_us,
                                            kind,
                                            cloned_ptr as usize,
                                        ));
                                    }
                                    self.ic_patches.extend(cloned_patches);
                                }
                            }
                        }
                    }

                    // Cooperative JIT safepoint poll (CRATONVM_JIT_SAFEPOINT_POLLS)
                    // -- loop back-edge. `target_pc <= pc` is this codebase's own
                    // definition of a `goto`-shaped back edge (mirrors the check
                    // just above that drives `unroll_copies`, and
                    // `detect_natural_loops`, which finds loop headers the same
                    // way). No-op unless the env flag is set AND this is a
                    // context method AND the helper table wired the flag
                    // address (see `emit_safepoint_poll`'s doc for the current
                    // coverage gap: a loop whose only backward branch is a
                    // conditional `ifXX`/`if_icmpXX`/`if_acmpXX` is not polled
                    // by this first cut).
                    if target_pc <= pc {
                        self.emit_safepoint_poll();
                    }

                    // JMP rel32 to header
                    self.buf.emit_byte(0xE9);
                    let patch_offset = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                    self.forward_patches.push((patch_offset, target_pc));
                    self.record_branch_target_depth(target_pc);
                    self.reset_spills();
                    dead = true;
                    pc += 3;
                }

                // tableswitch — jump table for dense tables, CMP chain for small
                0xaa => {
                    self.flush_scratch_registers();
                    let base_pc = pc;
                    pc += 1;
                    while pc % 4 != 0 {
                        pc += 1;
                    }
                    // Validate the fixed 12-byte header (default/low/high) is
                    // fully in-bounds before reading it. Crafted/unverified
                    // bytecode can place a tableswitch near the end of `code`;
                    // indexing past `code_len` would panic. Bail instead.
                    if pc + 12 > code_len {
                        return false;
                    }
                    let default_offset =
                        i32::from_be_bytes([code[pc], code[pc + 1], code[pc + 2], code[pc + 3]]);
                    let low = i32::from_be_bytes([
                        code[pc + 4],
                        code[pc + 5],
                        code[pc + 6],
                        code[pc + 7],
                    ]);
                    let high = i32::from_be_bytes([
                        code[pc + 8],
                        code[pc + 9],
                        code[pc + 10],
                        code[pc + 11],
                    ]);
                    // Compute the entry count in i64 so `high - low + 1` cannot
                    // overflow (release builds have overflow-checks off, so the
                    // old `(high - low + 1).max(0)` could wrap to a bogus
                    // positive value and drive a ~16 GB allocation / OOB read).
                    if low > high {
                        return false;
                    }
                    // Widening: i32 -> i64 (sign-extended so high-low+1 cannot overflow)
                    let count_i64 = (high as i64) - (low as i64) + 1;
                    pc += 12;
                    // The jump table is `count` i32 entries immediately after the
                    // header. Reject any count that does not fit the remaining
                    // bytes before allocating or reading it.
                    let remaining_entries = (code_len - pc) / 4;
                    // Cast: non-negative value to u64
                    if count_i64 < 0 || count_i64 as u64 > remaining_entries as u64 {
                        return false;
                    }
                    // Cast: non-negative index/count to usize
                    let count = count_i64 as usize;

                    // Collect all targets from the bytecode
                    let mut targets = Vec::with_capacity(count);
                    for _ in 0..count {
                        let off = i32::from_be_bytes([
                            code[pc],
                            code[pc + 1],
                            code[pc + 2],
                            code[pc + 3],
                        ]);
                        targets.push((base_pc as i32 + off) as usize); // Cast: x86-64 immediate encoding
                        pc += 4;
                    }
                    let def_target = (base_pc as i32 + default_offset) as usize; // Cast: x86-64 immediate encoding
                    let any_backward =
                        def_target <= base_pc || targets.iter().any(|&target| target <= base_pc);
                    // Canonicalize the operands that OUTLIVE this switch, exactly
                    // as the `ifeq`/`if_icmp`/`goto` arms do — and before popping
                    // the key, so the key participates in the relocation and
                    // cannot be clobbered by another slot's move (the same
                    // ordering rule those arms state).
                    //
                    // Every arm of a switch is a branch target, and a target
                    // revived from dead code rebuilds the operand stack at the
                    // canonical `base_spill_offset + i*8` — a layout NOTHING was
                    // establishing here, so any operand this basic block left at
                    // a non-canonical offset was read from the wrong slot by
                    // every arm. That is the second half of ECJ's
                    // `OperandStack.pop(OperandCategory)` miscompile: the inlined
                    // `TypeIds.getCategory` result sat above its semantic depth
                    // (fixed in `x64/inlining.rs`) and this `tableswitch` was the
                    // one branch shape in the walk that did not repair it, so the
                    // `if_icmpeq` at the merge compared the raw `TypeBinding.id`
                    // (tomcat/ecj-operandstack-*.md).
                    //
                    // Forward-only, mirroring those arms: a backward target's
                    // layout was fixed when the walk emitted it, and relocating
                    // to suit a forward merge would disagree with it.
                    if !any_backward && self.stack.len() > 1 {
                        self.canonicalize_stack();
                    }
                    let key_slot = self.pop_stack();
                    if any_backward {
                        self.emit_safepoint_poll();
                    }
                    self.load_slot_to_reg(RAX, key_slot);
                    // Every arm of a switch is a branch target, and needs the
                    // operand stack live at it recorded exactly the way the
                    // `if`/`goto` arms record theirs — the dead-code merge
                    // reconstruction reads both the depth and the oop marks
                    // from this map. Nothing recorded them before: an arm
                    // revived from dead code fell back to depth 0 with
                    // all-`false` marks, which is a guess in the depth and the
                    // very unsoundness `branch_target_stack_oop_marks`
                    // documents in the marks. The key is already popped here,
                    // so `self.stack` is exactly what every arm sees.
                    self.record_branch_target_depth(def_target);
                    for &target in &targets {
                        self.record_branch_target_depth(target);
                    }

                    if count <= 4 {
                        // Small table: CMP chain (compact code, few comparisons)
                        // Normalize key: SUB EAX, low
                        if low != 0 {
                            self.buf.emit(&[0x2D]); // SUB EAX, imm32
                            self.buf.emit(&low.to_le_bytes());
                        }
                        for (i, &target) in targets.iter().enumerate() {
                            self.buf.emit(&[0x3D]); // CMP EAX, imm32
                            self.buf.emit(&(i as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
                            self.buf.emit(&[0x0F, 0x84]); // JE rel32
                            let patch = self.buf.pos();
                            self.buf.emit(&[0; 4]);
                            self.forward_patches.push((patch, target));
                        }
                        // Default: JMP
                        self.buf.emit_byte(0xE9);
                        let dp = self.buf.pos();
                        self.buf.emit(&[0; 4]);
                        self.forward_patches.push((dp, def_target));
                    } else {
                        // Large table: O(1) jump table dispatch
                        //   SUB EAX, low        ; normalize index
                        //   CMP EAX, count      ; bounds check
                        //   JAE default          ; out of range → default
                        //   MOVSXD RCX, [RDX + RAX*4]  ; load relative offset from table
                        //   ADD RCX, RDX        ; compute absolute address
                        //   JMP RCX             ; indirect jump
                        //   <jump table: count * 4 bytes of i32 offsets>

                        // Normalize key: SUB EAX, low
                        if low != 0 {
                            self.buf.emit(&[0x2D]); // SUB EAX, imm32
                            self.buf.emit(&low.to_le_bytes());
                        }
                        // Bounds check: CMP EAX, count; JAE default
                        self.buf.emit(&[0x3D]); // CMP EAX, imm32
                        self.buf.emit(&(count as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
                        self.buf.emit(&[0x0F, 0x83]); // JAE rel32
                        let bounds_patch = self.buf.pos();
                        self.buf.emit(&[0; 4]);
                        self.forward_patches.push((bounds_patch, def_target));

                        // LEA RDX, [RIP + 0]  → points to jump table
                        // We'll emit: LEA RDX, [RIP + disp32] where disp32 will be
                        // patched to point to the table start.
                        self.buf.emit(&[0x48, 0x8D, 0x15]); // LEA RDX, [RIP + disp32]
                        let lea_patch = self.buf.pos();
                        self.buf.emit(&[0; 4]); // placeholder disp32

                        // MOVSXD RCX, [RDX + RAX*4]  ; load table[index]
                        // Encoding: REX.W 0x63 /r with SIB [RDX + RAX*4]
                        self.buf.emit(&[0x48, 0x63, 0x0C, 0x82]); // MOVSXD RCX, [RDX + RAX*4]

                        // ADD RCX, RDX  ; absolute = table_base + offset
                        self.buf.emit(&[0x48, 0x01, 0xD1]); // ADD RCX, RDX

                        // JMP RCX  ; indirect jump
                        self.buf.emit(&[0xFF, 0xE1]); // JMP RCX

                        // Patch LEA: disp32 = table_start - (lea_patch + 4)
                        let table_start = self.buf.pos();
                        let lea_rel = table_start as i32 - (lea_patch as i32 + 4); // Cast: x86-64 rel32 displacement
                        self.buf.try_patch_i32(lea_patch, lea_rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails

                        // Emit jump table: count entries, each i32 offset from table_start
                        for &target in &targets {
                            let entry_offset = self.buf.pos();
                            self.buf.emit(&[0; 4]); // placeholder
                            self.jump_table_patches
                                .push((entry_offset, table_start, target));
                        }
                    }
                    self.reset_spills();
                    dead = true;
                }

                // lookupswitch — CMP chain for small, binary search for large
                0xab => {
                    self.flush_scratch_registers();
                    let base_pc = pc;
                    pc += 1;
                    while pc % 4 != 0 {
                        pc += 1;
                    }
                    // Validate the fixed 8-byte header (default/npairs) is fully
                    // in-bounds before reading it. Bail on crafted bytecode that
                    // places the header past `code_len`.
                    if pc + 8 > code_len {
                        return false;
                    }
                    let default_offset =
                        i32::from_be_bytes([code[pc], code[pc + 1], code[pc + 2], code[pc + 3]]);
                    // `npairs` is a signed i32 in the classfile; a negative value
                    // would become a huge usize and drive an OOM allocation /
                    // OOB read. Reject it, and validate the pair table (8 bytes
                    // each) fits the remaining bytes before allocating.
                    let npairs_i32 = i32::from_be_bytes([
                        code[pc + 4],
                        code[pc + 5],
                        code[pc + 6],
                        code[pc + 7],
                    ]);
                    pc += 8;
                    if npairs_i32 < 0 {
                        return false;
                    }
                    // Cast: non-negative index/count to usize
                    let npairs = npairs_i32 as usize;
                    let remaining_pairs = (code_len - pc) / 8;
                    if npairs > remaining_pairs {
                        return false;
                    }

                    // Collect all (key, target) pairs
                    let mut pairs = Vec::with_capacity(npairs);
                    for _ in 0..npairs {
                        let key = i32::from_be_bytes([
                            code[pc],
                            code[pc + 1],
                            code[pc + 2],
                            code[pc + 3],
                        ]);
                        let off = i32::from_be_bytes([
                            code[pc + 4],
                            code[pc + 5],
                            code[pc + 6],
                            code[pc + 7],
                        ]);
                        let target = (base_pc as i32 + off) as usize; // Cast: x86-64 immediate encoding
                        pc += 8;
                        pairs.push((key, target));
                    }
                    let def_target = (base_pc as i32 + default_offset) as usize; // Cast: x86-64 immediate encoding
                    let any_backward =
                        def_target <= base_pc || pairs.iter().any(|&(_, target)| target <= base_pc);
                    // Same canonicalization the `tableswitch` arm above performs,
                    // and for the same reason — see the note there. The two
                    // switch arms are the only branch shapes in this walk that
                    // were not establishing the canonical layout their own
                    // targets are revived with.
                    if !any_backward && self.stack.len() > 1 {
                        self.canonicalize_stack();
                    }
                    let key_slot = self.pop_stack();
                    if any_backward {
                        self.emit_safepoint_poll();
                    }
                    self.load_slot_to_reg(RAX, key_slot);
                    // Every arm of a switch is a branch target, and needs the
                    // operand stack live at it recorded exactly the way the
                    // `if`/`goto` arms record theirs — the dead-code merge
                    // reconstruction reads both the depth and the oop marks
                    // from this map. Nothing recorded them before: an arm
                    // revived from dead code fell back to depth 0 with
                    // all-`false` marks, which is a guess in the depth and the
                    // very unsoundness `branch_target_stack_oop_marks`
                    // documents in the marks. The key is already popped here,
                    // so `self.stack` is exactly what every arm sees.
                    self.record_branch_target_depth(def_target);
                    for &(_, target) in &pairs {
                        self.record_branch_target_depth(target);
                    }

                    if npairs <= 6 {
                        // Small: linear CMP chain (fast for few entries)
                        for &(key, target) in &pairs {
                            self.buf.emit(&[0x3D]); // CMP EAX, imm32
                            self.buf.emit(&key.to_le_bytes());
                            self.buf.emit(&[0x0F, 0x84]); // JE rel32
                            let patch = self.buf.pos();
                            self.buf.emit(&[0; 4]);
                            self.forward_patches.push((patch, target));
                        }
                        // Default: JMP
                        self.buf.emit_byte(0xE9);
                        let dp = self.buf.pos();
                        self.buf.emit(&[0; 4]);
                        self.forward_patches.push((dp, def_target));
                    } else {
                        // Large: binary search tree emitted as nested CMP/JL/JG/JE
                        // The keys in lookupswitch are sorted per JVM spec.
                        // We emit a balanced binary search: O(log n) comparisons.
                        //
                        // Value in EAX. We use a recursive emission strategy:
                        //   pick middle key, CMP EAX, mid_key
                        //   JE target
                        //   JL left_subtree
                        //   (fall through to right subtree)
                        // At leaves, fall through to default.
                        self.emit_binary_search_lookup(&pairs, def_target);
                    }
                    self.reset_spills();
                    dead = true;
                }

                // ireturn / lreturn / freturn / dreturn / areturn
                0xac..=0xb0 => {
                    self.flush_scratch_registers();
                    self.pop_to_rax();
                    self.emit_epilogue();
                    self.reset_spills();
                    dead = true;
                    pc += 1;
                }

                // return (void)
                0xb1 => {
                    // No return value needed, just emit epilogue.
                    self.flush_scratch_registers();
                    // Zero RAX on the normal void-return path so callers can
                    // reliably distinguish a clean return (RAX == 0) from the
                    // `i64::MIN` deopt sentinel. Without this RAX is whatever
                    // the last op left, which could spuriously equal i64::MIN
                    // and trip the caller's post-invoke exception guard / the
                    // interpreter's post-JIT deopt check.
                    // XOR EAX, EAX  (31 C0) — zero-extends to RAX.
                    self.buf.emit(&[0x31, 0xC0]);
                    self.emit_epilogue();
                    self.reset_spills();
                    dead = true;
                    pc += 1;
                }

                // athrow (RBC.6) — lower to "stash the exception object as
                // the pending JIT exception, then return the i64::MIN deopt
                // sentinel". The helper (`jit_throw_exception`) handles the
                // JVMS athrow-on-null case by setting the pending-NPE flag
                // instead. The interpreter's JIT-return drains route the
                // exception to the caller; compilation is gated upstream to
                // methods with NO local exception handlers (this lowering
                // cannot branch to an in-method handler) and the OSR
                // trigger declines athrow methods entirely (its bail path
                // resumes at the back-edge and could re-run side effects).
                // Mirrors the shared bounds-check stub, which calls
                // `helpers.throw_aioobe` and epilogues with the sentinel.
                0xbf => {
                    self.flush_scratch_registers();
                    // Exception ref → first argument register.
                    let exc_slot = self.pop_stack();
                    self.load_slot_to_reg(ARG_REGS[0], exc_slot);
                    // RBC.6 correctness fix — pass this athrow's own bytecode
                    // pc as the second argument (a compile-time immediate) so
                    // `jit_throw_exception` can stash it alongside the
                    // exception. `execute_jit_call` then gives
                    // `route_jit_exception_through_method` a real `throw_pc`
                    // instead of `usize::MAX`, which — with 2+ exception-table
                    // entries whose catch types are in a subtype relationship —
                    // can match the wrong entry regardless of which
                    // try-region actually threw. `pc` here is this
                    // instruction's own bci (loaded after ARG_REGS[0] so it
                    // does not disturb the exception-ref load above).
                    //
                    // `route_jit_exception_through_method` range-tests this
                    // against the INTERPRETER exception table, so it must be an
                    // interpreter bci — hence `orig_bci`, which is the identity
                    // unless this compile is emitting rewritten bytecode. This
                    // is the fourth and last site in the backend that bakes a
                    // bci as an immediate; see `Compiler::bci_provenance`.
                    let throw_bci = self.orig_bci(pc);
                    self.emit_mov_imm32_sx(ARG_REGS[1], throw_bci as i32); // Cast: bci fits i32
                    self.emit_call_absolute(self.helpers.throw_exception);
                    // Helper returned the i64::MIN sentinel in RAX -
                    // propagate it as the method's return value.
                    //
                    // RBC.6 `athrow` admission: inside a protected range the
                    // sentinel alone is not enough. `jit_throw_exception` has
                    // stashed the exception and this bci, but nothing has
                    // recorded where this frame's non-parameter locals live, so
                    // a handler that reads one would resume it as 0/null. Route
                    // through the reason-9 stub instead of returning directly:
                    // it spills the trapping registers, materializes the precise
                    // exceptional frame from the snapshot recorded here, and
                    // then runs exactly the epilogue this arm would have run.
                    // The unconditional `JMP rel32` is patched by
                    // `emit_deopt_stubs` the same way a `Jcc rel32` guard is -
                    // both end in the same four displacement bytes.
                    //
                    // `flush_scratch_registers` above ran before the call, so
                    // any local the snapshot places in a caller-saved register
                    // has already been spilled to its frame slot; this is the
                    // same ordering `emit_post_invoke_exception_check` relies on.
                    //
                    // Keyed on the EMITTER pc, never on `throw_bci`: every
                    // `*_box_ptr_by_bci` map, `build_and_record_deopt_point`'s
                    // analysis lookups and `emit_deopt_stubs`' stub sharing are
                    // all in emitter coordinates, and each applies `orig_bci`
                    // itself for the value it hands the runtime. Handing an
                    // already-translated bci in would double-apply it under a
                    // bytecode loop rewrite (identity, and byte-identical, on an
                    // ordinary compile).
                    let precise_athrow_stub =
                        self.precise_exception_frames && self.pc_is_protected(pc);
                    if precise_athrow_stub {
                        if !self.exc_frame_box_ptr_by_bci.contains_key(&pc) {
                            let box_ptr = self.build_and_record_deopt_point(
                                pc,
                                crate::deopt::DeoptReason::PendingException,
                            );
                            self.exc_frame_box_ptr_by_bci.insert(pc, box_ptr);
                        }
                        // JMP rel32 (E9) - patched to the reason-9 stub, or to
                        // this bci's local-handler stub when this method's own
                        // exception table can catch here and compiled local
                        // handlers are armed. A `throw` caught by the very
                        // method that raised it is the shape javac emits for
                        // every `try { ... throw ... } catch` and for a
                        // rethrowing `finally`, and it is as enterable in
                        // compiled code as a callee's throw: the helper takes
                        // the throwable `jit_throw_exception` just stashed.
                        // The stub's own miss edge is this same reason-9 stub,
                        // so a propagating throw is unchanged.
                        self.buf.emit_byte(0xE9);
                        let patch_offset = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                        self.record_exception_check_edge(patch_offset, pc, true);
                    } else {
                        self.emit_epilogue();
                    }
                    self.reset_spills();
                    self.emitted_athrow = true;
                    dead = true;
                    pc += 1;
                }

                // getstatic (0xb2) — a direct load, or the helper
                //
                // HotSpot emits a plain load for a `getstatic`, because both
                // the class and the slot address are known at compile time.
                // CratonVM called `jit_getstatic` for every static read
                // instead, and that CALL — not the read — was the whole cost:
                // ~35 ns against HotSpot's ~1. See
                // `jit-getstatic-costs-a-helper-call-FIXED-20260803.md`.
                //
                // MED-2 (round-2 JIT review) named three blockers for emitting
                // the load. All three are gone:
                //
                //   1. "Slot storage is a `Vec<Value>` inside an `RwLock`ed
                //      `HashMap`, so the address is not stable." It is now a
                //      `StaticsBlock` — one leaked, never-freed allocation per
                //      class — mirrored by the lock-free `StaticsIndex`.
                //   2. "The `Value` enum is a tagged union, not a machine
                //      word." Its layout is pinned by
                //      `types::heap_types::field_cell_layout_matches_value_enum`,
                //      and the inline `getfield` arms have been reading field
                //      cells through `FIELD_CELL_PAYLOAD*_OFFSET` since July.
                //      A static cell is the same 16 bytes.
                //   3. "The `jit` crate has no `vm` dependency, so it cannot
                //      resolve a slot address at compile time." It does not
                //      need one. The VM registers a resolver function pointer
                //      plus its own `SharedVm` pointer through a process-global
                //      setter (`set_static_base_resolver`), exactly as it
                //      registers the savebase watch helpers — no
                //      `JitRuntimeHelpers` field, no golden offset, no ABI
                //      revision bump, because generated code never calls it.
                //      Only this backend does, while emitting.
                //
                // What is baked is the address of the class's base-POINTER
                // cell, not of the block: see `try_emit_inline_getstatic` for
                // the emitted shape and `StaticsIndex::base_cell_addr` for why
                // that one extra dependent load buys immunity to every
                // republication path.
                //
                // The helper below still owns every site the resolver declines
                // — a class not yet initialized at compile time (an inline load
                // runs no `<clinit>`), `java/lang/System` (the `out`/`err`/`in`
                // bootstrap intercept), anything not yet published, a second VM
                // in this process, and everything when
                // `CRATONVM_JIT=getstatic-helper` is set.
                //
                // `putstatic` (0xb3) deliberately stays on its helpers; see the
                // note there.
                0xb2 => {
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let (_, class_id_raw, field_index, type_tag, is_volatile) = match self
                        .static_field_info_idx
                        .get(&pc)
                        .map(|&i| self.static_field_info[i])
                    {
                        Some(v) => v,
                        None => {
                            if !substitute_unresolved_field_sites() {
                                return unresolved_field_site(pc, 0xb2);
                            }
                            (pc, 0, 0, b'I', false)
                        }
                    };

                    // Direct load, no helper CALL — the structural fix this
                    // opcode's long bail comment above describes. It emits the
                    // value push, the oop mark and the volatile fence itself,
                    // so the whole helper sequence below is skipped.
                    if self.try_emit_inline_getstatic(
                        class_id_raw,
                        field_index,
                        type_tag,
                        is_volatile,
                    ) {
                        pc += 3;
                        continue;
                    }

                    self.flush_scratch_registers();
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                    self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32); // Cast: x86-64 immediate encoding
                    self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                    self.emit_call_absolute(self.helpers.getstatic);
                    // jit-linewrapper-flushtype-npe fix (2026-07-17): the
                    // helper now runs `<clinit>` on first touch and, on
                    // failure, stashes the Java exception and returns the
                    // `i64::MIN` deopt sentinel instead of a field value.
                    // Route that through the shared exception-check stub
                    // (mirrors every other fallible JIT helper call) rather
                    // than pushing the sentinel bits as if they were a
                    // legitimate result.
                    self.emit_post_invoke_exception_check(type_tag);
                    // Volatile static: emit MFENCE after read (SeqCst acquire)
                    if is_volatile {
                        self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                    }
                    self.push_from_rax();
                    // T1.1.a (fix, 2026-07-07 — jit-invokedynamic-groovy
                    // regression follow-up): a reference-typed static field's
                    // loaded value is a live oop, but `push_from_rax`'s fast
                    // path always pushes a hard-coded `false` oop-mark — the
                    // same gap already fixed for `getfield`'s inline arms
                    // (see the `c_is_ref`/`type_tag == b'L' || b'['` markers
                    // above), just never applied here. Left unmarked, this
                    // stack slot decodes as a plain non-oop value in any
                    // precise GC/deopt oop map built while it's live (e.g. the
                    // invokedynamic uncommon-trap snapshot machinery, which
                    // records the operand stack directly beneath a live
                    // `invokedynamic` call site — `getstatic
                    // System.out` immediately followed by a
                    // `makeConcatWithConstants` invokedynamic is exactly this
                    // shape). A NullPointerException on the following
                    // `println` was the concrete symptom (`LicmRepro`/
                    // `LicmRepro2`/`ArrRepro`) traced to this exact gap.
                    if type_tag == b'L' || type_tag == b'[' {
                        self.mark_top_as_oop();
                    }
                    pc += 3;
                }

                // putstatic (0xb3) — write static field via type-specific helper
                //
                // The three MED-2 blockers listed on the 0xb2 arm above are
                // gone, and the same baked base-pointer cell would address a
                // write just as well. The write side is NOT symmetric, though,
                // and deliberately stays on the helper: `set_static_shared`
                // fires the SATB pre-barrier for an overwritten reference —
                // statics live in this Rust-side table, not the heap, so no
                // collector `set_field` barrier covers them and a missed one is
                // a hidden-pointer SATB hole (final remark misses the old
                // referent, cleanup frees a live region) — and it is also what
                // creates or grows a class's block on first touch. Inlining
                // reads costs that machinery nothing; inlining writes would
                // have to reproduce all of it. Reads were the measured problem
                // (see the 0xb2 arm's doc); a primitive-only inline `putstatic`
                // is the tractable next step if static WRITES ever show up on a
                // hot path.
                0xb3 => {
                    self.flush_scratch_registers();
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let (_, class_id_raw, field_index, type_tag, is_volatile) = match self
                        .static_field_info_idx
                        .get(&pc)
                        .map(|&i| self.static_field_info[i])
                    {
                        Some(v) => v,
                        None => {
                            if !substitute_unresolved_field_sites() {
                                return unresolved_field_site(pc, 0xb3);
                            }
                            (pc, 0, 0, b'I', false)
                        }
                    };

                    let val_slot = self.pop_stack();

                    // Select the appropriate helper based on type_tag
                    let helper_fn: usize = match type_tag {
                        b'J' => self.helpers.putstatic_long,
                        b'F' => self.helpers.putstatic_float,
                        b'D' => self.helpers.putstatic_double,
                        b'L' | b'[' => self.helpers.putstatic_object,
                        _ => self.helpers.putstatic_int,
                    };

                    // Volatile static: emit MFENCE before write (SeqCst release)
                    if is_volatile {
                        self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                    }
                    // Call jit_putstatic_xxx(vm_ptr, class_id_raw, field_index, val)
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset); // vm_ptr
                    self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32); // class_id // Cast: x86-64 immediate encoding
                    self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // field_index // Cast: x86-64 immediate encoding
                    self.load_slot_to_reg(ARG_REGS[3], val_slot); // value
                    self.emit_call_absolute(helper_fn);
                    // jit-putstatic-clinit-gap fix (2026-07-17): see the
                    // matching comment at the inlined-callee 0xb3 arm above —
                    // same helper, same new fallible-`<clinit>` sentinel.
                    self.emit_post_invoke_exception_check(b'V');
                    // Volatile static: emit MFENCE after write (SeqCst store-load barrier)
                    if is_volatile {
                        self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                    }
                    pc += 3;
                }

                // getfield — read object field via helper (or frame slot for scalar-replaced)
                0xb4 => {
                    if let Some(&new_pc) = self.scalar_field_ops.get(&pc) {
                        // Scalar-replaced getfield: load directly from frame slot
                        // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                        let (_, field_index, type_tag) =
                            match self.field_info_idx.get(&pc).map(|&i| self.field_info[i]) {
                                Some(v) => v,
                                None => {
                                    if !substitute_unresolved_field_sites() {
                                        return unresolved_field_site(pc, 0xb4);
                                    }
                                    (pc, 0, b'I')
                                }
                            };
                        let _obj_slot = self.pop_stack(); // dummy objectref
                        let sr_obj = &self.scalar_replaced[&new_pc];
                        let field_off =
                            sr_obj.field_base_offset + (field_index as i32) * (SLOT_SIZE as i32); // Cast: x86-64 immediate encoding
                        self.emit_load_local(RAX, field_off);
                        self.push_from_rax();
                        // A REFERENCE field of a scalar-replaced object is an
                        // ordinary live oop once loaded — the object being
                        // exploded into frame slots changes where the field
                        // lives, not what its value is. Same obligation as every
                        // other `getfield` arm.
                        if type_tag == b'L' || type_tag == b'[' {
                            self.mark_top_as_oop();
                        }
                        pc += 3;
                    } else if let Some(&(c_off, c_is_ref)) =
                        self.compact_field_off.get(&pc).filter(|_| {
                            !narrow_oops_block_inline_fields()
                                && (inline_getfield_enabled()
                                    || (guarded_inline_getfield_enabled()
                                        && self.helpers.read_bounds_addr != 0))
                        })
                    {
                        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_COMPACT_INLINE")
                            .is_some()
                        {
                            eprintln!(
                                "[compact-inline] getfield pc={pc} off={c_off} ref={c_is_ref}"
                            );
                        }
                        // Compact reference-field layout inline getfield. The
                        // packed byte offset + ref-ness were resolved at compile
                        // time, so emit a raw MOV (no helper call, no runtime
                        // layout lookup). A reference field is the bare 8-byte
                        // pointer AT the field; primitives are their tagless
                        // descriptor width (1/2/4/8 bytes).
                        //
                        // CRITICAL: a class with a registered compact layout may
                        // still have LEGACY-laid-out (16-byte-cell) instances —
                        // any allocation whose `num_fields` disagrees with the
                        // layout's field count falls back to the uniform layout
                        // (`plan_object_alloc`), e.g. native/synthetic-stub
                        // allocations of `java/lang/reflect/Method`,
                        // `ConcurrentHashMap`, `ArrayList`, … whose stub field
                        // count exceeds the real declared count the layout was
                        // built from. The compact offset is only valid for a
                        // genuinely-compact object, so we MUST key on the
                        // per-object `GC_FLAG_COMPACT` header bit (byte 21) the
                        // same way every heap/helper access path does — reading a
                        // legacy object at the compact offset returns a mangled
                        // {tag,partial-pointer} word that SIGSEGVs when later
                        // dereferenced/called. For a legacy receiver we take the
                        // uniform `index * SLOT_SIZE` 16-byte-cell path inline.
                        let (_, field_index, type_tag) =
                            match self.field_info_idx.get(&pc).map(|&i| self.field_info[i]) {
                                Some(v) => v,
                                None => {
                                    if !substitute_unresolved_field_sites() {
                                        return unresolved_field_site(pc, 0xb4);
                                    }
                                    (pc, 0, b'I')
                                }
                            };
                        let cell_off = (HEADER_SIZE + c_off as usize) as i32; // Cast: x86-64 disp32
                        let legacy_cell_off = (HEADER_SIZE + field_index * SLOT_SIZE) as i32; // Cast: disp32
                                                                                              // GUARDED (default) vs RAW (CRATONVM_JIT_INLINE_GETFIELD):
                                                                                              // raw keeps the historical null→0 inline semantics; guarded
                                                                                              // routes null/implausible receivers to the checked helper
                                                                                              // (NPE + i64::MIN sentinel) and flushes the scratch cache
                                                                                              // up-front because its slow path CALLs out.
                        let raw_mode = inline_getfield_enabled();
                        if !raw_mode {
                            self.flush_scratch_registers();
                        }
                        let trusted_have_key = !self.method_key.is_empty();
                        let trusted_marks_exact = self.stack_oop_marks_exact;
                        let trusted_top_is_oop =
                            self.stack_oop_marks.last().copied().unwrap_or(false);
                        let receiver_is_trusted_oop =
                            trusted_have_key && trusted_marks_exact && trusted_top_is_oop;
                        // WHICH of the three clauses refused, at EMISSION time.
                        //
                        // The trusted-oop shortcut is the difference between a
                        // bare null test and the six-compare containment check
                        // that no non-publishing collector can ever pass — i.e.
                        // between an inline load and a helper call, on the
                        // hottest path in the VM. The getfield page's open item
                        // 1 is "why is `stack_oop_marks_exact` false at these
                        // sites", and until now the only way to ask was to read
                        // the disassembly and infer. A bare "not trusted" would
                        // repeat this page's own founding mistake: it names a
                        // verdict, not a cause, and the three clauses want
                        // completely different fixes.
                        if !receiver_is_trusted_oop
                            && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_COMPACT_INLINE")
                                .is_some()
                        {
                            eprintln!(
                                "[compact-inline] getfield pc={pc} NOT-trusted-oop                                  have_key={trusted_have_key} marks_exact={trusted_marks_exact}                                  top_is_oop={trusted_top_is_oop} depth={} method={}",
                                self.stack.len(),
                                self.method_key,
                            );
                        }
                        let obj_slot = self.pop_stack();
                        self.load_slot_to_reg(RAX, obj_slot);
                        let (mut slow_patches, null_patch) = if raw_mode {
                            // Null check: TEST RAX,RAX; JZ <null> (result 0).
                            self.emit_test_r64_r64(RAX);
                            (Vec::new(), Some(self.emit_jcc_rel32_patch(0x84))) // JE
                        } else if receiver_is_trusted_oop {
                            (self.emit_trusted_oop_receiver_check(), None)
                        } else {
                            (
                                self.emit_guarded_getfield_receiver_check(
                                    self.helpers.read_bounds_addr,
                                ),
                                None,
                            )
                        };
                        // Per-object compactness: gc_flags byte @21 & GC_FLAG_COMPACT.
                        // Zero ⇒ legacy 16-byte-cell object → uniform-layout read.
                        self.emit_mov_r32_mem_disp32(
                            RCX,
                            RAX,
                            cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                        );
                        self.emit_and_r64_imm8(RCX, cratonvm_types::GC_FLAG_COMPACT as i8);
                        let legacy_patch = self.emit_jcc_rel32_patch(0x84); // JZ → legacy
                                                                            // --- compact path (8-byte ref / packed primitive cell) ---
                        if c_is_ref {
                            // 8-byte raw pointer at the cell base.
                            self.emit_mov_r64_mem_disp32(RAX, RAX, cell_off);
                        } else {
                            match type_tag {
                                b'J' | b'D' => {
                                    self.emit_mov_r64_mem_disp32(RAX, RAX, cell_off);
                                }
                                b'L' | b'[' => {
                                    // Defense-in-depth: contradictory metadata
                                    // (`c_is_ref == false` for a reference
                                    // descriptor). A non-ref-classified slot is
                                    // written as a full 16-byte `Value` cell, so
                                    // read the 8-byte pointer payload — never the
                                    // 32-bit MOVSXD below, which sign-extends half
                                    // a pointer into a bogus non-null receiver.
                                    self.emit_mov_r64_mem_disp32(
                                        RAX,
                                        RAX,
                                        cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32,
                                    );
                                }
                                b'F' => {
                                    self.emit_mov_r32_mem_disp32(RAX, RAX, cell_off);
                                }
                                b'Z' => self.emit_movx_r64_mem_disp32(RAX, RAX, cell_off, 8, false),
                                b'B' => self.emit_movx_r64_mem_disp32(RAX, RAX, cell_off, 8, true),
                                b'C' => {
                                    self.emit_movx_r64_mem_disp32(RAX, RAX, cell_off, 16, false)
                                }
                                b'S' => self.emit_movx_r64_mem_disp32(RAX, RAX, cell_off, 16, true),
                                _ => {
                                    self.emit_movsxd_r64_mem_disp32(RAX, RAX, cell_off);
                                }
                            }
                        }
                        let done_compact_patch = self.emit_jmp_rel32_patch();
                        // --- legacy path (uniform 16-byte Value cell) ---
                        // Mirrors the non-compact inline getfield arm: a reference
                        // (or long/double) field is the 8-byte payload at
                        // `legacy_cell + PAYLOAD64`; float is 4 bytes at PAYLOAD32;
                        // int-category is a sign-extended 4-byte load at PAYLOAD32.
                        self.patch_rel32_to_here(legacy_patch);
                        // Before either reference arm below reads the cell's
                        // 8-byte payload as a POINTER, check that the cell says
                        // it holds one. The IR backend's twin of this sequence
                        // did not, and a `[C` field whose cell held
                        // `Value::Int(1)` was loaded as the pointer `1` and
                        // dereferenced by the `arraylength` three instructions
                        // later -- SIGSEGV at `addr=0x5`, deterministically
                        // (Tomcat/Derby, 2026-08-23). The checked helper has
                        // always looked at the variant; this arm exists to skip
                        // the helper, so it has to ask the same question.
                        //
                        // RAW mode is excluded because it has no slow path to
                        // defer to -- its null arm yields 0 by design. It is an
                        // explicit opt-in whose own comment calls itself
                        // "historical semantics"; the guarded path is the
                        // default and is what the crash was on.
                        if !raw_mode && (c_is_ref || matches!(type_tag, b'L' | b'[')) {
                            self.buf.emit(&[0x83, 0xB8]); // CMP DWORD [RAX+disp32], imm8
                            self.buf.emit(
                                &(legacy_cell_off + FIELD_CELL_TAG_OFFSET as i32).to_le_bytes(),
                            );
                            self.buf
                                .emit_byte(cratonvm_types::FIELD_CELL_TAG_OBJECT as u8);
                            slow_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNE → helper
                        }
                        if c_is_ref {
                            self.emit_mov_r64_mem_disp32(
                                RAX,
                                RAX,
                                legacy_cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32,
                            );
                        } else {
                            match type_tag {
                                b'J' | b'D' => {
                                    self.emit_mov_r64_mem_disp32(
                                        RAX,
                                        RAX,
                                        legacy_cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32,
                                    );
                                }
                                b'L' | b'[' => {
                                    // Defense-in-depth (see the compact branch):
                                    // a reference descriptor always reads the
                                    // legacy cell's 64-bit pointer payload, even
                                    // when `c_is_ref` wrongly says non-ref.
                                    self.emit_mov_r64_mem_disp32(
                                        RAX,
                                        RAX,
                                        legacy_cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32,
                                    );
                                }
                                b'F' => {
                                    self.emit_mov_r32_mem_disp32(
                                        RAX,
                                        RAX,
                                        legacy_cell_off + FIELD_CELL_PAYLOAD32_OFFSET as i32,
                                    );
                                }
                                _ => {
                                    self.emit_movsxd_r64_mem_disp32(
                                        RAX,
                                        RAX,
                                        legacy_cell_off + FIELD_CELL_PAYLOAD32_OFFSET as i32,
                                    );
                                }
                            }
                        }
                        let done_legacy_patch = self.emit_jmp_rel32_patch();
                        if let Some(null_patch) = null_patch {
                            // RAW mode null path: RAX := 0 (historical semantics).
                            self.patch_rel32_to_here(null_patch);
                            self.emit_xor_reg_self(RAX);
                        } else {
                            // GUARDED slow path: null / unaligned / out-of-heap
                            // receiver → the checked helper, whose NPE +
                            // i64::MIN-sentinel semantics match the helper-only
                            // arm below exactly.
                            for p in slow_patches {
                                self.patch_rel32_to_here(p);
                            }
                            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                            self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                            self.emit_getfield_index_arg(ARG_REGS[2], field_index, type_tag);
                            crate::metrics::note_getfield_arm(1);
                            self.emit_call_absolute(self.helpers.getfield);
                            self.emit_post_invoke_exception_check(type_tag);
                        }
                        // join
                        self.patch_rel32_to_here(done_compact_patch);
                        self.patch_rel32_to_here(done_legacy_patch);
                        self.push_from_rax();
                        // T1.1.a — a reference field's loaded value is a live
                        // oop; the `push_from_rax` fast path always pushes a
                        // hard-coded `false` mark, which left this getfield
                        // result (and everything computed from it) unrooted
                        // for the GC / precise-deopt oop map. `c_is_ref` is
                        // authoritative here regardless of which sub-path
                        // (compact or legacy cell) actually loaded it.
                        if c_is_ref {
                            self.mark_top_as_oop();
                        }
                        pc += 3;
                    } else if let Some(&info_idx) = self
                        .field_info_idx
                        .get(&pc)
                        // RAW inline (opt-in) only when the compact layout is
                        // globally off: a compact receiver's reference field is an
                        // 8-byte pointer and a primitive field sits at a packed
                        // offset, so the baked `HEADER + index*SLOT_SIZE`
                        // 16-byte-cell load is wrong for it. The GUARDED default
                        // handles compact-on by testing the per-object
                        // GC_FLAG_COMPACT bit and routing compact receivers to the
                        // compact-aware `jit_getfield` helper.
                        .filter(|_| {
                            (inline_getfield_enabled()
                                && !cratonvm_types::compact_ref_fields_enabled())
                                || (guarded_inline_getfield_enabled()
                                    && self.helpers.read_bounds_addr != 0)
                        })
                    {
                        // Inline field load — the field index and type tag are
                        // statically resolved (`field_info` was built from
                        // `resolve_field_ref` in lib.rs), so we can emit a raw
                        // MOV against the object's field cell instead of a
                        // `CALL jit_getfield`. Bit-identical to the helper:
                        //
                        //   * null receiver  → result 0  (the helper's
                        //     `if obj_ptr == 0 { return 0 }` guard);
                        //   * int-category   → MOVSXD (sign-extend, matches
                        //     `Value::Int(i) => i as i64`);
                        //   * float          → 32-bit MOV (zero-extend, matches
                        //     `Value::Float(f) => f.to_bits() as i64`);
                        //   * long/double/ref → 64-bit MOV of the payload word
                        //     (`Long`/`Double` bits, or the raw object pointer
                        //     which is 0 for `Object(None)`).
                        //
                        // The field cell is the 16-byte `Value` enum; payload
                        // offsets within the cell come from the
                        // `FIELD_CELL_PAYLOAD*_OFFSET` constants in `types`
                        // (layout pinned by `field_cell_layout_matches_value_enum`).
                        let (_, field_index, type_tag) = self.field_info[info_idx];
                        let cell_off = (HEADER_SIZE + field_index * SLOT_SIZE) as i32; // Cast: x86-64 disp32
                                                                                       // GUARDED (default) vs RAW (opt-in, compact-off only) —
                                                                                       // see the compact arm above for the mode contract.
                        let raw_mode = inline_getfield_enabled()
                            && !cratonvm_types::compact_ref_fields_enabled();
                        if !raw_mode {
                            self.flush_scratch_registers();
                        }
                        let receiver_is_trusted_oop = !self.method_key.is_empty()
                            && self.stack_oop_marks_exact
                            && self.stack_oop_marks.last().copied().unwrap_or(false);
                        let obj_slot = self.pop_stack();
                        // Receiver → RAX.
                        self.load_slot_to_reg(RAX, obj_slot);
                        let (mut slow_patches, null_patch) = if raw_mode {
                            // Null check: TEST RAX,RAX; JZ <null-path>. On null we
                            // skip the load entirely and leave RAX = 0, matching
                            // `jit_getfield`'s early `return 0`.
                            self.emit_test_r64_r64(RAX);
                            (Vec::new(), Some(self.emit_jcc_rel32_patch(0x84))) // JE
                        } else if receiver_is_trusted_oop {
                            (self.emit_trusted_oop_receiver_check(), None)
                        } else {
                            (
                                self.emit_guarded_getfield_receiver_check(
                                    self.helpers.read_bounds_addr,
                                ),
                                None,
                            )
                        };
                        if !raw_mode && cratonvm_types::compact_ref_fields_enabled() {
                            // Per-object compact receiver → helper: this pc has no
                            // registered compact offset (or the class layout didn't
                            // match), so the uniform 16-byte-cell load below is only
                            // valid for a legacy-laid-out object. gc_flags byte @21.
                            self.emit_mov_r32_mem_disp32(
                                RCX,
                                RAX,
                                cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                            );
                            self.emit_and_r64_imm8(RCX, cratonvm_types::GC_FLAG_COMPACT as i8);
                            slow_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNZ
                        }
                        // A REFERENCE read must check the cell's discriminant
                        // before treating its payload as a pointer. `J`, `D`,
                        // `L` and `[` all share the 8-byte payload offset, so
                        // the DESCRIPTOR does not tell you what the cell
                        // actually holds -- only the tag does. Reading it
                        // without asking is the Tomcat/Derby SIGSEGV of
                        // 2026-08-23; see FIELD_CELL_TAG_OBJECT and the twin
                        // check in the compact arm above.
                        //
                        // RAW mode is excluded because it has no slow path to
                        // defer to; it is an explicit opt-in that documents
                        // itself as historical semantics.
                        if !raw_mode && matches!(type_tag, b'L' | b'[') {
                            self.buf.emit(&[0x83, 0xB8]); // CMP DWORD [RAX+disp32], imm8
                            self.buf
                                .emit(&(cell_off + FIELD_CELL_TAG_OFFSET as i32).to_le_bytes());
                            self.buf
                                .emit_byte(cratonvm_types::FIELD_CELL_TAG_OBJECT as u8);
                            slow_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNE → helper
                        }
                        match type_tag {
                            b'J' | b'D' | b'L' | b'[' => {
                                // 8-byte payload: MOV RAX, [RAX + cell + 8].
                                self.emit_mov_r64_mem_disp32(
                                    RAX,
                                    RAX,
                                    // Cast: fixed struct/layout offset to i32 instruction displacement
                                    cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32,
                                );
                            }
                            b'F' => {
                                // 4-byte float bits: MOV EAX (zero-extends).
                                self.emit_mov_r32_mem_disp32(
                                    RAX,
                                    RAX,
                                    // Cast: fixed struct/layout offset to i32 instruction displacement
                                    cell_off + FIELD_CELL_PAYLOAD32_OFFSET as i32,
                                );
                            }
                            _ => {
                                // int / boolean / byte / char / short: MOVSXD
                                // (sign-extend) — `Value::Int` payload.
                                self.emit_movsxd_r64_mem_disp32(
                                    RAX,
                                    RAX,
                                    // Cast: fixed struct/layout offset to i32 instruction displacement
                                    cell_off + FIELD_CELL_PAYLOAD32_OFFSET as i32,
                                );
                            }
                        }
                        let done_patch = self.emit_jmp_rel32_patch();
                        if let Some(null_patch) = null_patch {
                            // RAW mode null path: RAX := 0.
                            self.patch_rel32_to_here(null_patch);
                            self.emit_xor_reg_self(RAX);
                        } else {
                            // GUARDED slow path: null / unaligned / out-of-heap /
                            // compact-flagged receiver → the checked helper (same
                            // NPE + sentinel semantics as the helper-only arm).
                            for p in slow_patches {
                                self.patch_rel32_to_here(p);
                            }
                            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                            self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                            self.emit_getfield_index_arg(ARG_REGS[2], field_index, type_tag);
                            crate::metrics::note_getfield_arm(2);
                            self.emit_call_absolute(self.helpers.getfield);
                            self.emit_post_invoke_exception_check(type_tag);
                        }
                        // Join: result in RAX.
                        self.patch_rel32_to_here(done_patch);
                        self.push_from_rax();
                        // T1.1.a — see the compact-getfield arm above: a
                        // reference-typed field's loaded value is a live oop
                        // and must not be left with the default `false` mark
                        // `push_from_rax` assigns, or it decodes as a plain
                        // `Int` (not `Object`) in the precise GC/deopt oop
                        // map — the root cause of the JDT `Parser`
                        // stack-corruption bug (fixed-suite-bugs/
                        // jasper-jdt-parser-arrayindexoutofbounds.md): a
                        // `char[][]` field read this way, then used live
                        // across an always-deopting `System.arraycopy`
                        // reference-array call, resumed in the interpreter as
                        // a raw integer instead of an object reference.
                        if type_tag == b'L' || type_tag == b'[' {
                            self.mark_top_as_oop();
                        }
                        pc += 3;
                    } else if let Some(&info_idx) = self.field_info_idx.get(&pc) {
                        // Statically resolved field, but the raw inline path
                        // is disabled. Route through the checked helper while
                        // preserving the resolved slot index and oop marking.
                        let (_, field_index, type_tag) = self.field_info[info_idx];
                        self.flush_scratch_registers();
                        let obj_slot = self.pop_stack();
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                        self.emit_getfield_index_arg(ARG_REGS[2], field_index, type_tag);
                        crate::metrics::note_getfield_arm(3);
                        self.emit_call_absolute(self.helpers.getfield);
                        // See the inlined-callee getfield site above: the checked
                        // helper's `i64::MIN` sentinel must be caught here, before
                        // it's pushed/tagged as a live value, or a bad receiver
                        // silently corrupts execution instead of throwing.
                        self.emit_post_invoke_exception_check(type_tag);
                        self.push_from_rax();
                        if type_tag == b'L' || type_tag == b'[' {
                            self.mark_top_as_oop();
                        }
                        pc += 3;
                    } else {
                        // No statically-resolved field metadata for this
                        // getfield PC — fall back to the runtime helper. The
                        // field's real type is unknown here, so pass `b'J'` to
                        // force the ambiguity-safe (peek-dispatch_threw) sentinel
                        // check unconditionally — safe for every type, since a
                        // non-J/D/F field can never legitimately produce
                        // `i64::MIN` anyway.
                        self.flush_scratch_registers();
                        let obj_slot = self.pop_stack();
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                        // NOT routed through `emit_getfield_index_arg`: that
                        // encoder needs a type tag, and this arm is the one
                        // that has none. So `GETFIELD_EXPECT_REFERENCE` cannot
                        // be set here and the helper keeps its pre-2026-08-23
                        // behaviour of returning a punned slot's payload. That
                        // is survivable only because the same missing metadata
                        // means this arm does not know the value is a reference
                        // either, so nothing downstream dereferences it on the
                        // strength of a static type -- but it is a residual,
                        // and the place to close it is the resolution that
                        // failed, not here.
                        self.emit_mov_imm32_sx(ARG_REGS[2], 0); // Cast: x86-64 immediate encoding
                        crate::metrics::note_getfield_arm(4);
                        self.emit_call_absolute(self.helpers.getfield);
                        self.emit_post_invoke_exception_check(b'J');
                        self.push_from_rax();
                        pc += 3;
                    }
                }

                // putfield — write object field (frame slot for scalar-replaced, helper otherwise)
                0xb5 => {
                    if let Some(&new_pc) = self.scalar_field_ops.get(&pc) {
                        // Scalar-replaced putfield: store value directly to frame slot
                        // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                        let (_, field_index, _type_tag) =
                            match self.field_info_idx.get(&pc).map(|&i| self.field_info[i]) {
                                Some(v) => v,
                                None => {
                                    if !substitute_unresolved_field_sites() {
                                        return unresolved_field_site(pc, 0xb5);
                                    }
                                    (pc, 0, b'I')
                                }
                            };
                        note_field_site(&self.method_key, "putfield/scalar", pc, field_index, b'?');
                        let val_slot = self.pop_stack();
                        let _obj_slot = self.pop_stack(); // dummy objectref
                        let sr_obj = &self.scalar_replaced[&new_pc];
                        let field_off =
                            sr_obj.field_base_offset + (field_index as i32) * (SLOT_SIZE as i32); // Cast: x86-64 immediate encoding
                        self.load_slot_to_reg(RAX, val_slot);
                        self.emit_store_local(field_off, RAX);
                        // Each scalar field reserves SLOT_SIZE bytes (see `new` zero-init). Always
                        // clear the high qword so category-1 values and refs never leave garbage in
                        // the second word — mismatches here showed up as Windows AVs under Spring
                        // with JIT on (SportMe / insurance) while interpreter-only runs continued.
                        self.emit_xor_reg_self(RAX);
                        self.emit_store_local(field_off + 8, RAX);
                        pc += 3;
                    } else {
                        self.flush_scratch_registers();
                        // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                        let (_, field_index, type_tag) =
                            match self.field_info_idx.get(&pc).map(|&i| self.field_info[i]) {
                                Some(v) => v,
                                None => {
                                    if !substitute_unresolved_field_sites() {
                                        return unresolved_field_site(pc, 0xb5);
                                    }
                                    (pc, 0, b'I')
                                }
                            };
                        note_field_site(&self.method_key, "putfield", pc, field_index, type_tag);
                        let receiver_mark_index = self.stack_oop_marks.len().checked_sub(2);
                        let receiver_is_trusted_oop = !self.method_key.is_empty()
                            && self.stack_oop_marks_exact
                            && receiver_mark_index
                                .and_then(|i| self.stack_oop_marks.get(i))
                                .copied()
                                .unwrap_or(false);
                        let val_slot = self.pop_stack();
                        let obj_slot = self.pop_stack();
                        // A null receiver must become a real Java NPE here, before
                        // either the inline store or the `jit_putfield_*` helper can
                        // turn it into a silent no-op. The helper's guard
                        // (`!plausible_heap_pointer(obj_ptr) { return; }`) exists to
                        // avoid dereferencing garbage, but it returns WITHOUT raising,
                        // so a `putfield` on null silently dropped the store and
                        // execution continued -- see probes/NullPutfieldProbe.java.
                        //
                        // This is the same call the inlined-callee `0xb5` arm already
                        // makes; only the top-level arm was missed when
                        // `emit_precise_null_check_field_store` landed. Inside a
                        // protected range under `precise_exception_frames` it records a
                        // reason-10 precise NPE frame; otherwise it falls back to the
                        // ordinary null-check stub. NOT emitted on the
                        // scalar-replaced branch above, whose "objectref" is a dummy
                        // with no real receiver behind it.
                        self.load_slot_to_reg(RAX, obj_slot);
                        self.emit_precise_null_check_field_store();
                        // GATED reference store — tried before every arm below,
                        // and it supersedes them on the counts that matter: it
                        // reads the collector's published barrier gates instead
                        // of inferring them from a region table G1 and ZGC
                        // leave empty (so the compact arm below is UNREACHABLE
                        // under the default collector — it emits six
                        // containment compares that cannot pass and then calls
                        // the helper), and it does not require the field's old
                        // value to be null, so an ordinary re-assignment stays
                        // inline instead of taking the helper.
                        //
                        // `false` here means "not admitted", and every arm
                        // below then runs exactly as it does today. Declining
                        // is the safe direction and the only one a missing
                        // barrier plan can produce.
                        let gated_ref_store = (type_tag == b'L' || type_tag == b'[')
                            && gated_ref_store_enabled()
                            && inline_putfield_enabled()
                            && !narrow_oops_block_inline_fields()
                            && cratonvm_types::compact_ref_fields_enabled()
                            && match self.compact_field_off.get(&pc) {
                                Some(&(c_off, _)) => {
                                    // Cast: a compact field offset plus the
                                    // header is bounded by the object size.
                                    let cell_off = (HEADER_SIZE + c_off as usize) as i32;
                                    self.emit_gated_compact_ref_putfield(
                                        obj_slot,
                                        val_slot,
                                        field_index,
                                        cell_off,
                                        receiver_is_trusted_oop,
                                    )
                                }
                                None => false,
                            };
                        if gated_ref_store {
                            // The sequence above is complete: store, both
                            // barrier gates, the helper fallback and the
                            // out-of-bounds drop all converge here.
                        } else if type_tag == b'L' || type_tag == b'[' {
                            // HIGH-5 / R20: inline the reference-field store on the
                            // barrier-free fast path (CRATONVM_JIT_INLINE_PUTFIELD).
                            // The field cell is the 16-byte `Value` enum: tag dword
                            // (`Value::Object` == 4) at FIELD_CELL_TAG_OFFSET, raw
                            // pointer payload at FIELD_CELL_PAYLOAD64_OFFSET. We emit
                            // the store directly ONLY when no GC barrier is required:
                            //   * receiver non-null;
                            //   * receiver YOUNG (`gc_flags & GC_FLAG_OLD_GEN == 0`,
                            //     header byte @21) → no generational card needed;
                            //   * field's OLD value null (payload @cell+8 == 0) → no
                            //     SATB snapshot to preserve, regardless of marking;
                            //   * index in bounds (`num_slots`, header u32 @16).
                            // Every other case bails to `jit_putfield_object`, which
                            // performs the full SATB pre-barrier + card write-barrier.
                            // This is the fresh-init pattern (`n.left = newChild`) that
                            // dominates allocation-heavy code. Off ⇒ helper as before.
                            // Compact layout: reference fields are 8-byte
                            // pointers, so this inline 16-byte `Value` store is
                            // wrong — bail to the compact-aware
                            // `jit_putfield_object` helper.
                            if let Some(&(c_off, _)) =
                                self.compact_field_off.get(&pc).filter(|_| {
                                    // Keep compact reference stores behind the
                                    // same opt-in as legacy inline putfield.
                                    // A stale/misclassified compact receiver
                                    // otherwise lets this bare 8-byte store
                                    // scribble a Value cell during Tomcat's
                                    // repeated webapp start/stop cycles.
                                    inline_putfield_enabled()
                                        && !narrow_oops_block_inline_fields()
                                        && cratonvm_types::compact_ref_fields_enabled()
                                        && self.helpers.region_bounds_addr != 0
                                })
                            {
                                if cratonvm_types::flags::runtime_var_os(
                                    "CRATONVM_DBG_COMPACT_INLINE",
                                )
                                .is_some()
                                {
                                    eprintln!("[compact-inline] putfield-ref pc={pc} off={c_off}");
                                }
                                // Reached only when the gated sequence declined
                                // (no published plan), so this arm is the
                                // pre-existing behaviour, unchanged.
                                note_ungated_ref_store();
                                // COMPACT inline reference putfield: store the
                                // bare 8-byte pointer on the barrier-free fast
                                // path (non-null YOUNG receiver, NULL old value,
                                // index in bounds), else bail to the compact-aware
                                // jit_putfield_object helper (full SATB + card).
                                // Same barrier-free reasoning as the legacy
                                // 16-byte fast path below — only the store width
                                // (8 bytes, no tag dword) and the old-value offset
                                // (cell base, not +8) differ.
                                let cell_off = (HEADER_SIZE + c_off as usize) as i32; // Cast: disp32
                                let mut bail: Vec<usize> = Vec::new();
                                self.load_slot_to_reg(RAX, obj_slot);
                                // INT-6: null + alignment + published-region
                                // containment (subsumes the old bare null check).
                                // The YOUNG test below reads GC_FLAG_OLD_GEN,
                                // which ONLY the Generational backend maintains —
                                // under G1/ZGC every object reads as "young" and
                                // the fast path would skip G1's RSet post-barrier
                                // (edge lost, referent freed live at the next
                                // pause: Old→young after G1-1, and young→young
                                // into a JNI-pinned, CSet-excluded region even
                                // after it). G1/ZGC publish no region bounds
                                // (table all zeros), so this guard routes EVERY
                                // receiver to the full-barrier helper there;
                                // under Generational it adds the same three
                                // containment compares the guarded getfield
                                // already pays.
                                //
                                // G1-2: the trusted-oop substitution below drops
                                // exactly the containment compares that make the
                                // above true, so it is legal ONLY when the
                                // backend really has bounds published — which is
                                // the table's CONTENT, not `region_bounds_addr
                                // != 0` (the address of a process-global static,
                                // always non-zero). See `region_bounds_are_live`.
                                bail.extend(
                                    if receiver_is_trusted_oop
                                        && region_bounds_are_live(self.helpers.region_bounds_addr)
                                    {
                                        self.emit_trusted_oop_receiver_check()
                                    } else {
                                        self.emit_guarded_getfield_receiver_check(
                                            self.helpers.region_bounds_addr,
                                        )
                                    },
                                );
                                // LEGACY receiver (no GC_FLAG_COMPACT) → helper: the
                                // compact 8-byte cell offset is only valid for a
                                // genuinely-compact object. A class with a registered
                                // compact layout can still have uniform 16-byte-cell
                                // instances (any allocation whose `num_fields`
                                // disagrees with the layout field count — e.g.
                                // native/synthetic-stub `Method`/`ArrayList`/… whose
                                // padded stub count exceeds the real declared count).
                                // `jit_putfield_object` keys on the per-object flag
                                // and does the correct uniform-layout store. Without
                                // this the compact-offset old-value read + store would
                                // scribble a pointer into the wrong bytes of a legacy
                                // object → heap corruption / SIGSEGV.
                                self.emit_mov_r32_mem_disp32(
                                    RCX,
                                    RAX,
                                    cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                );
                                self.emit_and_r64_imm8(RCX, cratonvm_types::GC_FLAG_COMPACT as i8);
                                bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ not-compact → helper
                                                                            // old-gen receiver → helper (card). gc_flags @21 bit0.
                                if !self.inline_card_mark_available() {
                                    self.emit_mov_r32_mem_disp32(
                                        RCX,
                                        RAX,
                                        cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                    );
                                    self.emit_and_r64_imm8(RCX, 1);
                                    bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ old-gen
                                }
                                // non-null OLD value → helper (SATB). The old ref
                                // is the 8-byte pointer AT the cell base.
                                self.emit_mov_r64_mem_disp32(RCX, RAX, cell_off);
                                self.emit_test_r64_r64(RCX);
                                bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ non-null old
                                                                            // bounds: field_index < num_slots (header u32 @12).
                                self.emit_mov_r32_mem_disp32(
                                    RCX,
                                    RAX,
                                    cratonvm_types::NUM_SLOTS_OFFSET as i32,
                                );
                                self.emit_mov_imm64(RDX, field_index as i64);
                                self.emit_cmp_r32_r32(RDX, RCX);
                                let oob = self.emit_jcc_rel32_patch(0x83); // JAE → drop
                                                                           // FAST STORE: bare 8-byte pointer at the cell base.
                                self.load_slot_to_reg(RDX, val_slot);
                                self.emit_mov_mem_disp32_r64(RAX, RDX, cell_off);
                                if self.inline_card_mark_available() {
                                    self.emit_inline_card_mark_regs(RAX, RDX);
                                }
                                let done = self.emit_jmp_rel32_patch();
                                // --- helper fallback (full barriers) ---
                                for b in bail {
                                    self.patch_rel32_to_here(b);
                                }
                                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                                self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast
                                self.load_slot_to_reg(ARG_REGS[3], val_slot);
                                self.emit_call_absolute(self.helpers.putfield_object);
                                self.patch_rel32_to_here(oob);
                                self.patch_rel32_to_here(done);
                            } else if inline_putfield_enabled()
                                && !cratonvm_types::compact_ref_fields_enabled()
                                && self.helpers.region_bounds_addr != 0
                            {
                                let cell_off = (HEADER_SIZE + field_index * SLOT_SIZE) as i32; // Cast: x86-64 disp32
                                let mut bail: Vec<usize> = Vec::new();
                                // obj → RAX
                                self.load_slot_to_reg(RAX, obj_slot);
                                // INT-6: null + alignment + published-region
                                // containment (subsumes the old bare null check;
                                // null still reaches the helper, matching its
                                // no-op semantics). The YOUNG test below reads
                                // GC_FLAG_OLD_GEN, which ONLY the Generational
                                // backend maintains — under G1/ZGC every object
                                // reads as "young" and this fast path would elide
                                // G1's RSet post-barrier (Old→young before G1-1;
                                // young→young into a JNI-pinned, CSet-excluded
                                // region after it). G1/ZGC publish no region
                                // bounds (table all zeros), so every receiver
                                // bails to the full-barrier helper there.
                                //
                                // G1-2: same reasoning as the compact arm above —
                                // the trusted-oop substitution removes the
                                // containment compares, so it is conditional on
                                // the bounds table actually holding live bounds.
                                bail.extend(
                                    if receiver_is_trusted_oop
                                        && region_bounds_are_live(self.helpers.region_bounds_addr)
                                    {
                                        self.emit_trusted_oop_receiver_check()
                                    } else {
                                        self.emit_guarded_getfield_receiver_check(
                                            self.helpers.region_bounds_addr,
                                        )
                                    },
                                );
                                // old-gen receiver → helper (card barrier). gc_flags is
                                // the exported gc_flags byte; GC_FLAG_OLD_GEN == bit 0.
                                if !self.inline_card_mark_available() {
                                    self.emit_mov_r32_mem_disp32(
                                        RCX,
                                        RAX,
                                        cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                    );
                                    self.emit_and_r64_imm8(RCX, 1);
                                    bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ old-gen
                                }
                                // non-null OLD value → helper (SATB). Read the cell's
                                // 8-byte payload; a null old value never needs SATB.
                                self.emit_mov_r64_mem_disp32(
                                    RCX,
                                    RAX,
                                    cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32, // Cast: layout offset → disp32
                                );
                                self.emit_test_r64_r64(RCX);
                                bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ non-null old
                                                                            // bounds: field_index < num_slots (header u32 @12).
                                                                            // 32-bit compare — an 8-byte read would fold in the
                                                                            // adjacent gc_age/gc_flags bytes.
                                self.emit_mov_r32_mem_disp32(
                                    RCX,
                                    RAX,
                                    cratonvm_types::NUM_SLOTS_OFFSET as i32,
                                );
                                self.emit_mov_imm64(RDX, field_index as i64); // RDX = field_index
                                self.emit_cmp_r32_r32(RDX, RCX); // cmp field_index, num_slots
                                let oob = self.emit_jcc_rel32_patch(0x83); // JAE → out of bounds, drop
                                                                           // FAST STORE. tag dword = 4 (+ zeroed pad dword) via a
                                                                           // sign-extended imm32 qword store; payload = value.
                                self.emit_mov_imm64(RCX, 4);
                                self.emit_mov_mem_disp32_r64(
                                    RAX,
                                    RCX,
                                    cell_off + FIELD_CELL_TAG_OFFSET as i32, // Cast: layout offset → disp32
                                );
                                self.load_slot_to_reg(RDX, val_slot);
                                self.emit_mov_mem_disp32_r64(
                                    RAX,
                                    RDX,
                                    cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32, // Cast: layout offset → disp32
                                );
                                if self.inline_card_mark_available() {
                                    self.emit_inline_card_mark_regs(RAX, RDX);
                                }
                                let done = self.emit_jmp_rel32_patch();
                                // --- helper fallback (full barriers) ---
                                for b in bail {
                                    self.patch_rel32_to_here(b);
                                }
                                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                                self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                                self.load_slot_to_reg(ARG_REGS[3], val_slot);
                                self.emit_call_absolute(self.helpers.putfield_object);
                                // join: the out-of-bounds skip and the post-store jump
                                // both land here (after the helper).
                                self.patch_rel32_to_here(oob);
                                self.patch_rel32_to_here(done);
                            } else {
                                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                                self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                                self.load_slot_to_reg(ARG_REGS[3], val_slot);
                                self.emit_call_absolute(self.helpers.putfield_object);
                            }
                        } else {
                            self.load_slot_to_reg(ARG_REGS[0], obj_slot);
                            self.emit_mov_imm32_sx(ARG_REGS[1], field_index as i32); // Cast: x86-64 immediate encoding
                            self.load_slot_to_reg(ARG_REGS[2], val_slot);
                            let helper = match type_tag {
                                b'J' => self.helpers.putfield_long,
                                b'F' => self.helpers.putfield_float,
                                b'D' => self.helpers.putfield_double,
                                _ => self.helpers.putfield_int,
                            };
                            self.emit_call_absolute(helper);
                        }
                        pc += 3;
                    }
                }

                // invokestatic — self-call, direct call, inline, or dispatch helper
                0xb8 => {
                    self.flush_scratch_registers();

                    // TDigest's private quantile/cdf kernels use exactly:
                    // Integer.valueOf(i) -> Function.apply(Object) ->
                    // checkcast Double -> Double.doubleValue().  Scalarize that
                    // erased adapter only in those private kernels, retaining the
                    // generic invoke lowering for every other call site.
                    let tdigest_numeric_kernel = self
                        .method_label
                        .starts_with("org/elasticsearch/tdigest/Dist.quantile")
                        || self
                            .method_label
                            .starts_with("org/elasticsearch/tdigest/Dist.cdf");
                    if tdigest_numeric_kernel
                        && pc + 14 <= code.len()
                        && code[pc + 3] == 0xb9
                        && code[pc + 8] == 0xc0
                        && code[pc + 11] == 0xb6
                    {
                        if self.stack.len() >= 2 {
                            // Keep the lambda receiver on the simulated stack
                            // through the safepoint so the oop map roots it.
                            let Some(&index_slot) = self.stack.last() else {
                                // The specialized pattern was recognized but
                                // its simulated stack no longer matches. Bail
                                // out of JIT compilation; the interpreter can
                                // execute the ordinary invoke path safely.
                                self.fail("singlepass-codegen/lambda-int-to-double-stack-shape");
                                return false;
                            };
                            let lambda_slot = self.stack[self.stack.len() - 2];
                            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                            self.load_slot_to_reg(ARG_REGS[1], lambda_slot);
                            self.load_slot_to_reg(ARG_REGS[2], index_slot);
                            self.emit_pre_safepoint_spill();
                            self.emit_call_absolute(self.helpers.lambda_int_to_double);
                            self.emit_oop_map_for_safepoint();
                            let _ = self.pop_stack();
                            let _ = self.pop_stack();
                            self.emit_post_invoke_exception_check(b'D');
                            self.push_from_rax_as_xmm0();
                            pc += 14;
                            continue;
                        }
                    }

                    // Check for inline site first (most profitable)
                    if self.inline_sites.contains_key(&pc) {
                        if self.try_emit_inline(pc) {
                            pc += 3;
                            continue;
                        }
                    }

                    // Check for direct call target
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let direct = self.direct_calls_idx.get(&pc).map(|&i| {
                        let dc = &self.direct_calls[i].1;
                        note_emit_direct(&self.method_key, pc, dc.entry);
                        (dc.entry, dc.needs_context, dc.num_params, dc.return_type)
                    });

                    // Check for invoke_info (fallback to jit_invoke_dispatch)
                    let info_ptr = self
                        .invoke_info_idx
                        .get(&pc)
                        .map(|&i| self.invoke_info[i].1);

                    let direct = direct.filter(|(entry, _, _, _)| {
                        if *entry != crate::JitIntrinsic::ArraycopyPrimitive.as_entry() {
                            return true;
                        }
                        !crate::deopt::despec_contains(&self.method_key, pc as u32)
                    });
                    // E27-1 N2b: `indexOf(I)` is intrinsified ONLY where the
                    // needle is a compile-time constant in `0..=0xFFFF`, which
                    // is the range on which the inline single-code-unit scan
                    // and `code_point_needle` are the same function. Filtered
                    // HERE, before the intrinsic ladder, so a declined site
                    // takes the ordinary dispatch it takes today — the same
                    // shape as the `ArraycopyPrimitive` despec filter above.
                    // Deliberately NOT a runtime screen: see
                    // `prev_insn_int_const` for why that would be a cliff.
                    let direct = direct.filter(|(entry, _, _, _)| {
                        if *entry != crate::JitIntrinsic::StringIndexOfChar.as_entry() {
                            return true;
                        }
                        matches!(prev_insn_int_const(code, code_len, pc), Some(0..=0xFFFF))
                    });

                    if let Some((callee_entry, callee_needs_ctx, callee_params, ret_type)) = direct
                    {
                        if callee_entry == crate::MATH_SQRT_INTRINSIC {
                            // Math.sqrt(double) intrinsic: inline SQRTSD — no call overhead
                            let arg_slot = self.pop_stack();
                            // Flush any OTHER Xmm(0) slots that would be clobbered by SQRTSD
                            // (the arg itself is fine — it's consumed)
                            self.flush_xmm0_slots();
                            match arg_slot {
                                StackSlot::Xmm(xmm) => {
                                    if xmm != 0 {
                                        // MOVSD XMM0, XMMn
                                        let modrm = 0xC0 | (xmm & 7);
                                        if xmm >= 8 {
                                            self.buf.emit(&[0xF2, 0x41, 0x0F, 0x10, modrm]);
                                        } else {
                                            self.buf.emit(&[0xF2, 0x0F, 0x10, modrm]);
                                        }
                                    }
                                    // else: already in XMM0
                                }
                                _ => {
                                    self.load_slot_to_reg(RAX, arg_slot);
                                    self.emit_movq_xmm_from_rax(0);
                                }
                            }
                            self.emit_sqrtsd_xmm0();
                            self.stack_push(StackSlot::Xmm(0), false);
                        } else if callee_entry == crate::MATH_FLOOR_INTRINSIC
                            || callee_entry == crate::MATH_CEIL_INTRINSIC
                            || callee_entry == crate::MATH_RINT_INTRINSIC
                        {
                            // Math.floor/ceil/rint intrinsic: ROUNDSD XMM0, XMM0, imm8
                            let arg_slot = self.pop_stack();
                            self.flush_xmm0_slots();
                            match arg_slot {
                                StackSlot::Xmm(xmm) => {
                                    if xmm != 0 {
                                        let modrm = 0xC0 | (xmm & 7);
                                        if xmm >= 8 {
                                            self.buf.emit(&[0xF2, 0x41, 0x0F, 0x10, modrm]);
                                        } else {
                                            self.buf.emit(&[0xF2, 0x0F, 0x10, modrm]);
                                        }
                                    }
                                }
                                _ => {
                                    self.load_slot_to_reg(RAX, arg_slot);
                                    self.emit_movq_xmm_from_rax(0);
                                }
                            }
                            // ROUNDSD XMM0, XMM0, imm8
                            // Encoding: 66 0F 3A 0B C0 imm8
                            let imm8 = if callee_entry == crate::MATH_FLOOR_INTRINSIC {
                                0x09u8 // round toward -inf, inexact suppress
                            } else if callee_entry == crate::MATH_CEIL_INTRINSIC {
                                0x0Au8 // round toward +inf, inexact suppress
                            } else {
                                0x08u8 // round to nearest even, inexact suppress
                            };
                            self.buf.emit(&[0x66, 0x0F, 0x3A, 0x0B, 0xC0, imm8]);
                            self.stack_push(StackSlot::Xmm(0), false);
                        } else if callee_entry == crate::MATH_ABS_DOUBLE_INTRINSIC {
                            // Math.abs(double): clear sign bit (bit 63)
                            let arg_slot = self.pop_stack();
                            self.flush_xmm0_slots();
                            match arg_slot {
                                StackSlot::Xmm(xmm) => {
                                    if xmm != 0 {
                                        let modrm = 0xC0 | (xmm & 7);
                                        if xmm >= 8 {
                                            self.buf.emit(&[0xF2, 0x41, 0x0F, 0x10, modrm]);
                                        } else {
                                            self.buf.emit(&[0xF2, 0x0F, 0x10, modrm]);
                                        }
                                    }
                                }
                                _ => {
                                    self.load_slot_to_reg(RAX, arg_slot);
                                    self.emit_movq_xmm_from_rax(0);
                                }
                            }
                            // Load sign mask 0x7FFFFFFFFFFFFFFF into RCX, then MOVQ XMM1, RCX, ANDPD XMM0, XMM1
                            // MOV RCX, imm64
                            self.buf.emit_byte(0x48); // REX.W
                            self.buf.emit_byte(0xB9); // MOV RCX, imm64
                            self.buf.emit(&0x7FFFFFFFFFFFFFFFu64.to_le_bytes());
                            // MOVQ XMM1, RCX
                            self.emit_movq_xmm_from_gpr(1, RCX);
                            // ANDPD XMM0, XMM1: 66 0F 54 C1
                            self.buf.emit(&[0x66, 0x0F, 0x54, 0xC1]);
                            self.stack_push(StackSlot::Xmm(0), false);
                        } else if callee_entry == crate::MATH_ABS_FLOAT_INTRINSIC {
                            // Math.abs(float): clear sign bit (bit 31)
                            let arg_slot = self.pop_stack();
                            self.flush_xmm0_slots();
                            match arg_slot {
                                StackSlot::Xmm(xmm) => {
                                    if xmm != 0 {
                                        let modrm = 0xC0 | (xmm & 7);
                                        if xmm >= 8 {
                                            self.buf.emit(&[0xF3, 0x41, 0x0F, 0x10, modrm]);
                                        } else {
                                            self.buf.emit(&[0xF3, 0x0F, 0x10, modrm]);
                                        }
                                    }
                                }
                                _ => {
                                    self.load_slot_to_reg(RAX, arg_slot);
                                    // MOVD XMM0, EAX: 66 0F 6E C0
                                    self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]);
                                }
                            }
                            // Load sign mask 0x7FFFFFFF into ECX, MOVD XMM1, ECX, ANDPS XMM0, XMM1
                            // MOV ECX, imm32
                            self.buf.emit_byte(0xB9);
                            self.buf.emit(&0x7FFFFFFFu32.to_le_bytes());
                            // MOVD XMM1, ECX: 66 0F 6E C9
                            self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC9]);
                            // ANDPS XMM0, XMM1: 0F 54 C1
                            self.buf.emit(&[0x0F, 0x54, 0xC1]);
                            self.stack_push(StackSlot::Xmm(0), false);
                        } else if callee_entry == crate::MATH_ABS_INT_INTRINSIC {
                            // Math.abs(int): branchless absolute value
                            let arg_slot = self.pop_stack();
                            self.load_slot_to_reg(RAX, arg_slot);
                            // MOV ECX, EAX: 89 C1
                            self.buf.emit(&[0x89, 0xC1]);
                            // SAR EAX, 31 (sign-extend to all bits): C1 F8 1F
                            self.buf.emit(&[0xC1, 0xF8, 0x1F]);
                            // XOR ECX, EAX: 31 C1
                            self.buf.emit(&[0x31, 0xC1]);
                            // SUB ECX, EAX: 29 C1
                            self.buf.emit(&[0x29, 0xC1]);
                            // MOV EAX, ECX: 89 C8
                            self.buf.emit(&[0x89, 0xC8]);
                            self.push_from_rax();
                        } else if callee_entry == crate::MATH_ABS_LONG_INTRINSIC {
                            // Math.abs(long): branchless 64-bit absolute value
                            let arg_slot = self.pop_stack();
                            self.load_slot_to_reg(RAX, arg_slot);
                            // MOV RCX, RAX: 48 89 C1
                            self.buf.emit(&[0x48, 0x89, 0xC1]);
                            // SAR RAX, 63: 48 C1 F8 3F
                            self.buf.emit(&[0x48, 0xC1, 0xF8, 0x3F]);
                            // XOR RCX, RAX: 48 31 C1
                            self.buf.emit(&[0x48, 0x31, 0xC1]);
                            // SUB RCX, RAX: 48 29 C1
                            self.buf.emit(&[0x48, 0x29, 0xC1]);
                            // MOV RAX, RCX: 48 89 C8
                            self.buf.emit(&[0x48, 0x89, 0xC8]);
                            self.push_from_rax();
                        } else if callee_entry == crate::MATH_FMA_DOUBLE_INTRINSIC {
                            // T1.1.28 — Math.fma(double, double, double).
                            //
                            // Per JLS, `Math.fma(a, b, c)` computes `a*b + c`
                            // as if with unlimited intermediate precision and
                            // then rounded once. We lower to a direct call
                            // into `jit_math_fma_double`, which delegates to
                            // Rust's `f64::mul_add` — that maps to
                            // `VFMADD231SD` on FMA3-capable hosts and to a
                            // correctly-rounded software fused operation on
                            // everything else. Either path satisfies the JLS
                            // single-rounding requirement.
                            //
                            // extern "C" fn(f64, f64, f64) -> f64 — arguments
                            // pass in XMM0, XMM1, XMM2 on both SysV and Win64.
                            self.flush_scratch_registers();
                            let c_slot = self.pop_stack();
                            let b_slot = self.pop_stack();
                            let a_slot = self.pop_stack();
                            // Load a into RAX then → XMM0.
                            self.load_slot_to_reg(RAX, a_slot);
                            self.emit_movq_xmm_from_rax(0);
                            self.load_slot_to_reg(RAX, b_slot);
                            self.emit_movq_xmm_from_rax(1);
                            self.load_slot_to_reg(RAX, c_slot);
                            self.emit_movq_xmm_from_rax(2);
                            self.emit_call_absolute(self.helpers.math_fma_double);
                            // Result in XMM0 → move to RAX and push as FP.
                            self.emit_movq_rax_from_xmm(0);
                            self.push_from_rax_as_xmm0();
                        } else if callee_entry == crate::MATH_FMA_FLOAT_INTRINSIC {
                            // T1.1.28 — Math.fma(float, float, float) — same
                            // plan via `jit_math_fma_float` / `f32::mul_add`.
                            self.flush_scratch_registers();
                            let c_slot = self.pop_stack();
                            let b_slot = self.pop_stack();
                            let a_slot = self.pop_stack();
                            self.load_slot_to_reg(RAX, a_slot);
                            self.emit_movq_xmm_from_rax(0);
                            self.load_slot_to_reg(RAX, b_slot);
                            self.emit_movq_xmm_from_rax(1);
                            self.load_slot_to_reg(RAX, c_slot);
                            self.emit_movq_xmm_from_rax(2);
                            self.emit_call_absolute(self.helpers.math_fma_float);
                            self.emit_movq_rax_from_xmm(0);
                            self.push_from_rax_as_xmm0();
                        } else if callee_entry == crate::MATH_MIN_INT_INTRINSIC
                            || callee_entry == crate::MATH_MAX_INT_INTRINSIC
                        {
                            // Round-8 Bug 8 — branchless Math.min(int,int) /
                            // Math.max(int,int) via CMOV. Pop b then a (a is
                            // the deeper operand, the leftmost arg in the JLS
                            // signature). After `CMP EAX, ECX` (a vs b):
                            //   * CMOVL EAX, ECX fires when `a < b` and
                            //     overwrites EAX (=a) with ECX (=b) — i.e.
                            //     keeps the LARGER value in EAX. This is MAX.
                            //   * CMOVG EAX, ECX fires when `a > b` and
                            //     overwrites EAX (=a) with ECX (=b) — i.e.
                            //     keeps the SMALLER value in EAX. This is MIN.
                            // Round-9 CRIT fix: the previous version had these
                            // two swapped, so `Math.min(3, 5)` returned 5 and
                            // `Math.max(3, 5)` returned 3.
                            let b_slot = self.pop_stack();
                            let a_slot = self.pop_stack();
                            self.load_slot_to_reg(RAX, a_slot);
                            self.load_slot_to_reg(RCX, b_slot);
                            // CMP EAX, ECX — sets flags for signed compare.
                            self.emit_cmp_r32_r32(RAX, RCX);
                            let cc = if callee_entry == crate::MATH_MIN_INT_INTRINSIC {
                                0x4Fu8 // CMOVG — if a > b, replace a with b (keep smaller)
                            } else {
                                0x4Cu8 // CMOVL — if a < b, replace a with b (keep larger)
                            };
                            // CMOVcc EAX, ECX (32-bit, no REX.W): 0F 4c C1
                            self.buf.emit(&[0x0F, cc, 0xC1]);
                            // The 32-bit CMOV zero-extends the selected value
                            // into the upper 32 bits of RAX. The JIT's value
                            // ABI keeps `int`s sign-extended to 64 bits (see
                            // i2b/i2s/i2l, which all MOVSXD to 64-bit), so a
                            // negative result such as `Math.min(-7, 4)` must
                            // be re-extended or it surfaces as a large
                            // positive (`-7` → `0xFFFFFFF9`). MOVSXD RAX, EAX.
                            self.buf.emit(&[0x48, 0x63, 0xC0]);
                            self.push_from_rax();
                        } else if callee_entry == crate::MATH_MIN_LONG_INTRINSIC
                            || callee_entry == crate::MATH_MAX_LONG_INTRINSIC
                        {
                            // Round-8 Bug 8 — 64-bit Math.min(long,long) /
                            // Math.max(long,long) via REX.W CMP + CMOV. Same
                            // semantics as the int variants but 64-bit.
                            // Round-9 CRIT fix: opcodes were swapped (see int
                            // variant above for the full rationale).
                            let b_slot = self.pop_stack();
                            let a_slot = self.pop_stack();
                            self.load_slot_to_reg(RAX, a_slot);
                            self.load_slot_to_reg(RCX, b_slot);
                            // CMP RAX, RCX (REX.W): 48 39 C8
                            self.buf.emit(&[0x48, 0x39, 0xC8]);
                            let cc = if callee_entry == crate::MATH_MIN_LONG_INTRINSIC {
                                0x4Fu8 // CMOVG — if a > b, replace a with b (keep smaller)
                            } else {
                                0x4Cu8 // CMOVL — if a < b, replace a with b (keep larger)
                            };
                            // CMOVcc RAX, RCX (REX.W): 48 0F 4c C1
                            self.buf.emit(&[0x48, 0x0F, cc, 0xC1]);
                            self.push_from_rax();
                        } else if callee_entry == crate::MATH_MIN_FLOAT_INTRINSIC
                            || callee_entry == crate::MATH_MAX_FLOAT_INTRINSIC
                            || callee_entry == crate::MATH_MIN_DOUBLE_INTRINSIC
                            || callee_entry == crate::MATH_MAX_DOUBLE_INTRINSIC
                        {
                            // `Math.min`/`Math.max` for float and double.
                            //
                            // SSE's MINSS/MAXSS are NOT Math.min/Math.max.
                            // Per the SDM, `MINSS dst, src` returns `src`
                            // whenever both operands are zero or either is
                            // NaN. Java's javadoc requires the opposite in
                            // both cases:
                            //
                            //   * "If either value is NaN, then the result is
                            //     NaN" -- and the JDK body returns the NaN
                            //     ARGUMENT, whose payload bits are observable
                            //     through `Float.floatToRawIntBits`.
                            //   * "this method considers negative zero to be
                            //     strictly smaller than positive zero", so
                            //     min(+0.0f, -0.0f) is -0.0f whichever way
                            //     round the arguments come, and max is +0.0f.
                            //
                            // The sequence below gets both right. For `min`:
                            //
                            //     t1 = MINSS(a, b)      ; a<b ? a : b
                            //     t2 = MINSS(b, a)      ; b<a ? b : a
                            //     r  = t1 OR t2
                            //
                            // For ordered, unequal inputs t1 == t2 == the
                            // smaller value, so the OR is the identity. For
                            // +-0.0 the two MINSSs return the two DIFFERENT
                            // zeros, and OR-ing their bit patterns sets the
                            // sign bit iff either was -0.0 -- exactly "the
                            // result is negative zero whenever one of them
                            // is". `max` is the mirror image: MAXSS and AND,
                            // so the sign survives only when BOTH were -0.0.
                            //
                            // NaN is then patched with two never-taken
                            // branches rather than folded into the bitwise
                            // trick, because OR-ing a NaN with the other
                            // operand's bits yields *a* NaN but not *the* NaN
                            // Java returns. `Math.min(a, NaN)` returns the
                            // second argument (`a <= b ? a : b` in the JDK
                            // body is false when unordered) and
                            // `Math.min(NaN, b)` returns the first.
                            let is_double = callee_entry == crate::MATH_MIN_DOUBLE_INTRINSIC
                                || callee_entry == crate::MATH_MAX_DOUBLE_INTRINSIC;
                            let is_min = callee_entry == crate::MATH_MIN_FLOAT_INTRINSIC
                                || callee_entry == crate::MATH_MIN_DOUBLE_INTRINSIC;
                            self.flush_xmm0_slots();
                            let b_slot = self.pop_stack();
                            let a_slot = self.pop_stack();
                            self.load_slot_to_reg(RAX, a_slot);
                            self.emit_movq_xmm_from_rax(0); // XMM0 = a
                            self.load_slot_to_reg(RAX, b_slot);
                            self.emit_movq_xmm_from_rax(1); // XMM1 = b

                            // MOVAPS/MOVAPD XMM2 <- XMM0 (save `a`) and
                            // XMM3 <- XMM1 (save `b`) for the NaN fixups.
                            let movap: &[u8] = if is_double {
                                &[0x66, 0x0F, 0x28]
                            } else {
                                &[0x0F, 0x28]
                            };
                            self.buf.emit(movap);
                            self.buf.emit_byte(0xD0); // XMM2 <- XMM0
                            self.buf.emit(movap);
                            self.buf.emit_byte(0xD9); // XMM3 <- XMM1

                            // MIN/MAX SS/SD: XMM0 op= XMM1, then XMM1 op= XMM2.
                            let prefix = if is_double { 0xF2u8 } else { 0xF3u8 };
                            let op = if is_min { 0x5Du8 } else { 0x5Fu8 };
                            self.buf.emit(&[prefix, 0x0F, op, 0xC1]); // XMM0, XMM1
                            self.buf.emit(&[prefix, 0x0F, op, 0xCA]); // XMM1, XMM2

                            // ORPS/ORPD for min, ANDPS/ANDPD for max.
                            let bitop = if is_min { 0x56u8 } else { 0x54u8 };
                            if is_double {
                                self.buf.emit(&[0x66, 0x0F, bitop, 0xC1]);
                            } else {
                                self.buf.emit(&[0x0F, bitop, 0xC1]);
                            }

                            // NaN fixups. UCOMISS/UCOMISD sets PF when its
                            // operands are unordered, so comparing a register
                            // with itself tests "is this NaN".
                            let ucomis: &[u8] = if is_double {
                                &[0x66, 0x0F, 0x2E]
                            } else {
                                &[0x0F, 0x2E]
                            };
                            // UCOMIS XMM2, XMM2 -- is `a` NaN?
                            self.buf.emit(ucomis);
                            self.buf.emit_byte(0xD2);
                            // JNP .check_b (a is not NaN)
                            self.buf.emit(&[0x7B, 0x00]);
                            let jnp_check_b = self.buf.pos() - 1;
                            // MOVAP XMM0 <- XMM2: return `a` with its exact bits.
                            self.buf.emit(movap);
                            self.buf.emit_byte(0xC2);
                            // JMP .done
                            self.buf.emit(&[0xEB, 0x00]);
                            let jmp_done = self.buf.pos() - 1;

                            let check_b = self.buf.pos();
                            // UCOMIS XMM3, XMM3 -- is `b` NaN?
                            self.buf.emit(ucomis);
                            self.buf.emit_byte(0xDB);
                            // JNP .done (neither is NaN: keep the bitwise result)
                            self.buf.emit(&[0x7B, 0x00]);
                            let jnp_done = self.buf.pos() - 1;
                            // MOVAP XMM0 <- XMM3: return `b` with its exact bits.
                            self.buf.emit(movap);
                            self.buf.emit_byte(0xC3);

                            let done = self.buf.pos();
                            for (patch, target) in
                                [(jnp_check_b, check_b), (jmp_done, done), (jnp_done, done)]
                            {
                                // Cast: usize offsets to i64 for the rel8 patch math.
                                let rel = target as i64 - (patch as i64 + 1);
                                debug_assert!(
                                    (-128..=127).contains(&rel),
                                    "Math.min/max fp intrinsic rel8 out of range: {rel}"
                                );
                                Self::patch_rel8_or_bail(&mut self.buf, patch, rel);
                            }

                            self.stack_push(StackSlot::Xmm(0), is_double);
                        } else if callee_entry == crate::MATH_MULTIPLY_HIGH_INTRINSIC
                            || callee_entry == crate::MATH_UNSIGNED_MULTIPLY_HIGH_INTRINSIC
                        {
                            // Math.multiplyHigh(JJ)J / unsignedMultiplyHigh(JJ)J —
                            // high 64 bits of the 128-bit product. The hottest leaf
                            // in the SunEC P-256 Montgomery field multiply, called
                            // once per limb pair. One-operand `IMUL r64` (signed) /
                            // `MUL r64` (unsigned) compute RDX:RAX = RAX * r64; the
                            // high half lands in RDX. Multiplication is commutative,
                            // so operand order is irrelevant to the result.
                            //
                            // flush_scratch_registers() above already spilled every
                            // value-stack slot out of RAX/RCX/RDX (locals live only
                            // in callee-saved R12-R15/RBX/RSI/RDI, deferred-spill
                            // slots only in R8/R9), so clobbering RAX/RCX/RDX here is
                            // safe — same contract the Math.min/max long path relies
                            // on, extended to RDX.
                            let b_slot = self.pop_stack();
                            let a_slot = self.pop_stack();
                            self.load_slot_to_reg(RAX, a_slot);
                            self.load_slot_to_reg(RCX, b_slot);
                            // IMUL RCX (48 F7 E9) signed / MUL RCX (48 F7 E1) unsigned.
                            let modrm = if callee_entry == crate::MATH_MULTIPLY_HIGH_INTRINSIC {
                                0xE9u8 // /5 IMUL
                            } else {
                                0xE1u8 // /4 MUL
                            };
                            self.buf.emit(&[0x48, 0xF7, modrm]);
                            // MOV RAX, RDX (48 89 D0) — high half is the result.
                            self.buf.emit(&[0x48, 0x89, 0xD0]);
                            self.push_from_rax();
                        }
                        // --- invokestatic intrinsic family regions ---
                        // A follow-up agent for family <TAG> appends its
                        // codegen as `else if callee_entry ==
                        // crate::JitIntrinsic::Foo.as_entry() { ... }`
                        // strictly between that family's BEGIN/END markers.
                        // An empty region contributes nothing, so the
                        // `if let Some(...) = direct` chain stays valid.
                        //
                        // ===== INTRINSIC REGION BEGIN: INT_BITS =====
                        // java.lang.Integer bit-manipulation intrinsics
                        // (Phase 1a). Each pops `num_params` ints off the
                        // operand stack, computes into EAX and pushes the
                        // result. All emitted code is bit-identical to the
                        // JDK semantics (verified by intrinsic_int_bits.rs).
                        // --- FP_BITS: Double bit reinterpretation ---
                        //
                        // One `MOVQ` each. These replaced 361 million checked
                        // native-bridge crossings in one run of
                        // `PSquarePercentileTest`; see the FP_BITS region in
                        // `jit/src/lib.rs` for the census.
                        //
                        // RAW semantics come free: `MOVQ` moves all 64 bits,
                        // NaN payload included, which is exactly what
                        // `doubleToRawLongBits` is specified to return. The
                        // canonicalising `doubleToLongBits` is not matched by
                        // the resolver and so cannot reach here.
                        else if callee_entry
                            == crate::JitIntrinsic::DoubleToRawLongBits.as_entry()
                        {
                            // Double.doubleToRawLongBits(d): the argument's
                            // 64 bits, unchanged, as a long.
                            let arg = self.pop_stack();
                            match arg {
                                StackSlot::Xmm(xmm) => self.emit_movq_rax_from_xmm(xmm),
                                // A frame slot or GPR already holds the raw
                                // 64-bit pattern -- the JIT stores a double
                                // as its bits -- so this is a plain load and
                                // no XMM round trip is needed.
                                _ => self.load_slot_to_reg(RAX, arg),
                            }
                            self.push_from_rax();
                        } else if callee_entry == crate::JitIntrinsic::LongBitsToDouble.as_entry() {
                            // Double.longBitsToDouble(bits): the mirror.
                            let arg = self.pop_stack();
                            self.flush_xmm0_slots();
                            self.load_slot_to_reg(RAX, arg);
                            self.emit_movq_xmm_from_rax(0);
                            self.stack_push(StackSlot::Xmm(0), false);
                        } else if callee_entry == crate::JitIntrinsic::IntBitCount.as_entry() {
                            // Integer.bitCount(i): POPCNT EAX, EAX. The
                            // matcher only registers this when has_popcnt()
                            // is true, so the instruction is always valid.
                            let arg = self.pop_stack();
                            self.load_slot_to_reg(RAX, arg);
                            // POPCNT EAX, EAX: F3 0F B8 C0
                            self.buf.emit(&[0xF3, 0x0F, 0xB8, 0xC0]);
                            self.push_from_rax();
                        } else if callee_entry
                            == crate::JitIntrinsic::IntNumberOfLeadingZeros.as_entry()
                        {
                            // Integer.numberOfLeadingZeros(i): result is 32
                            // for input 0, else 31 - floor(log2(i)).
                            let arg = self.pop_stack();
                            self.load_slot_to_reg(RAX, arg);
                            if crate::x64::has_lzcnt() {
                                // LZCNT EAX, EAX: F3 0F BD C0 — defined to
                                // return 32 for a zero input, matching JDK.
                                self.buf.emit(&[0xF3, 0x0F, 0xBD, 0xC0]);
                            } else {
                                // Fallback: BSR ECX, EAX gives the MSB index
                                // and sets ZF iff the input is zero. We pick
                                // ECX = -1 on a zero input so the subsequent
                                // `31 - ECX` formula yields 32.
                                //   MOV EDX, -1
                                self.buf.emit(&[0xBA]);
                                self.buf.emit(&(-1i32).to_le_bytes());
                                // BSR ECX, EAX: 0F BD C8 (ZF=1 if EAX==0)
                                self.buf.emit(&[0x0F, 0xBD, 0xC8]);
                                // CMOVZ ECX, EDX: 0F 44 CA (consumes BSR's ZF)
                                self.buf.emit(&[0x0F, 0x44, 0xCA]);
                                // MOV EAX, 31: B8 1F 00 00 00
                                self.buf.emit(&[0xB8]);
                                self.buf.emit(&31i32.to_le_bytes());
                                // SUB EAX, ECX: 29 C8  → EAX = 31 - index
                                self.buf.emit(&[0x29, 0xC8]);
                            }
                            self.push_from_rax();
                        } else if callee_entry
                            == crate::JitIntrinsic::IntNumberOfTrailingZeros.as_entry()
                        {
                            // Integer.numberOfTrailingZeros(i): result is 32
                            // for input 0, else the LSB index.
                            let arg = self.pop_stack();
                            self.load_slot_to_reg(RAX, arg);
                            if crate::x64::has_bmi1() {
                                // TZCNT EAX, EAX: F3 0F BC C0 — defined to
                                // return 32 for a zero input, matching JDK.
                                self.buf.emit(&[0xF3, 0x0F, 0xBC, 0xC0]);
                            } else {
                                // Fallback: BSF ECX, EAX gives the LSB index
                                // and sets ZF iff the input is zero; pick 32
                                // for the zero case.
                                //   MOV EDX, 32
                                self.buf.emit(&[0xBA]);
                                self.buf.emit(&32i32.to_le_bytes());
                                // BSF ECX, EAX: 0F BC C8 (ZF=1 if EAX==0)
                                self.buf.emit(&[0x0F, 0xBC, 0xC8]);
                                // CMOVZ ECX, EDX: 0F 44 CA
                                self.buf.emit(&[0x0F, 0x44, 0xCA]);
                                // MOV EAX, ECX: 89 C8
                                self.buf.emit(&[0x89, 0xC8]);
                            }
                            self.push_from_rax();
                        } else if callee_entry == crate::JitIntrinsic::IntReverseBytes.as_entry() {
                            // Integer.reverseBytes(i): BSWAP EAX (0F C8).
                            let arg = self.pop_stack();
                            self.load_slot_to_reg(RAX, arg);
                            self.buf.emit(&[0x0F, 0xC8]);
                            self.push_from_rax();
                        } else if callee_entry == crate::JitIntrinsic::IntHighestOneBit.as_entry() {
                            // Integer.highestOneBit(i): the value with only
                            // the highest set bit of `i`, or 0 when i == 0.
                            let arg = self.pop_stack();
                            self.load_slot_to_reg(RAX, arg);
                            // MOV EDX, EAX: 89 C2 — preserve the original.
                            self.buf.emit(&[0x89, 0xC2]);
                            // BSR ECX, EAX: 0F BD C8 — ECX = MSB index
                            // (undefined for input 0, handled below).
                            self.buf.emit(&[0x0F, 0xBD, 0xC8]);
                            // MOV EAX, 1: B8 01 00 00 00
                            self.buf.emit(&[0xB8]);
                            self.buf.emit(&1i32.to_le_bytes());
                            // SHL EAX, CL: D3 E0 — EAX = 1 << index.
                            self.buf.emit(&[0xD3, 0xE0]);
                            // TEST EDX, EDX: 85 D2 — set ZF iff input was 0.
                            self.buf.emit(&[0x85, 0xD2]);
                            // CMOVZ EAX, EDX: 0F 44 C2 — input 0 → result 0.
                            self.buf.emit(&[0x0F, 0x44, 0xC2]);
                            self.push_from_rax();
                        } else if callee_entry == crate::JitIntrinsic::IntLowestOneBit.as_entry() {
                            // Integer.lowestOneBit(i): i & -i. Naturally
                            // yields 0 for input 0, matching the JDK.
                            let arg = self.pop_stack();
                            self.load_slot_to_reg(RAX, arg);
                            // MOV ECX, EAX: 89 C1
                            self.buf.emit(&[0x89, 0xC1]);
                            // NEG EAX: F7 D8  → EAX = -i
                            self.buf.emit(&[0xF7, 0xD8]);
                            // AND EAX, ECX: 21 C8  → EAX = -i & i
                            self.buf.emit(&[0x21, 0xC8]);
                            self.push_from_rax();
                        } else if callee_entry == crate::JitIntrinsic::IntReverse.as_entry() {
                            // Integer.reverse(i): reverse the bit order via
                            // the standard SWAR sequence. The JDK does five
                            // stages; the final two (swap byte pairs, then
                            // swap halves) are exactly BSWAP, so we emit
                            // three SWAR stages followed by BSWAP.
                            let arg = self.pop_stack();
                            self.load_slot_to_reg(RAX, arg);
                            // One SWAR stage for (mask, shift):
                            //   MOV ECX, EAX
                            //   SHR EAX, shift
                            //   AND EAX, mask
                            //   AND ECX, mask
                            //   SHL ECX, shift
                            //   OR  EAX, ECX
                            for &(mask, shift) in
                                &[(0x5555_5555u32, 1u8), (0x3333_3333, 2), (0x0F0F_0F0F, 4)]
                            {
                                // MOV ECX, EAX: 89 C1
                                self.buf.emit(&[0x89, 0xC1]);
                                // SHR EAX, imm8: C1 E8 ib
                                self.buf.emit(&[0xC1, 0xE8, shift]);
                                // AND EAX, imm32: 25 id
                                self.buf.emit(&[0x25]);
                                self.buf.emit(&mask.to_le_bytes());
                                // AND ECX, imm32: 81 E1 id
                                self.buf.emit(&[0x81, 0xE1]);
                                self.buf.emit(&mask.to_le_bytes());
                                // SHL ECX, imm8: C1 E1 ib
                                self.buf.emit(&[0xC1, 0xE1, shift]);
                                // OR EAX, ECX: 09 C8
                                self.buf.emit(&[0x09, 0xC8]);
                            }
                            // BSWAP EAX: 0F C8 — swaps the four bytes, which
                            // completes the 8- and 16-bit reversal stages.
                            self.buf.emit(&[0x0F, 0xC8]);
                            self.push_from_rax();
                        } else if callee_entry == crate::JitIntrinsic::IntCompare.as_entry() {
                            // Integer.compare(x, y): branchless (x>y)-(x<y).
                            // The stack holds x (deeper) then y.
                            let y = self.pop_stack();
                            let x = self.pop_stack();
                            self.load_slot_to_reg(RAX, x);
                            self.load_slot_to_reg(RCX, y);
                            // CMP EAX, ECX: 39 C8 — signed compare x vs y.
                            self.buf.emit(&[0x39, 0xC8]);
                            // SETG AL:  0F 9F C0 — AL = 1 if x > y.
                            self.buf.emit(&[0x0F, 0x9F, 0xC0]);
                            // SETL DL:  0F 9C C2 — DL = 1 if x < y.
                            self.buf.emit(&[0x0F, 0x9C, 0xC2]);
                            // MOVZX EAX, AL: 0F B6 C0
                            self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                            // MOVZX EDX, DL: 0F B6 D2
                            self.buf.emit(&[0x0F, 0xB6, 0xD2]);
                            // SUB EAX, EDX: 29 D0 → -1, 0 or 1.
                            self.buf.emit(&[0x29, 0xD0]);
                            self.push_from_rax();
                        } else if callee_entry == crate::JitIntrinsic::IntRotateLeft.as_entry()
                            || callee_entry == crate::JitIntrinsic::IntRotateRight.as_entry()
                        {
                            // Integer.rotateLeft(i, distance) / rotateRight:
                            // ROL/ROR EAX, CL. x86 masks CL & 0x1f for a 32-bit
                            // rotate, byte-identical to the JDK (rotation mod 32),
                            // so no explicit distance masking is needed. Stack:
                            // i (deeper), distance (top).
                            let dist = self.pop_stack();
                            let val = self.pop_stack();
                            self.load_slot_to_reg(RAX, val);
                            self.load_slot_to_reg(RCX, dist);
                            // ROL EAX, CL: D3 /0 = D3 C0 ; ROR EAX, CL: D3 /1 = D3 C8
                            let modrm =
                                if callee_entry == crate::JitIntrinsic::IntRotateLeft.as_entry() {
                                    0xC0u8
                                } else {
                                    0xC8u8
                                };
                            self.buf.emit(&[0xD3, modrm]);
                            // MOVSXD RAX, EAX (48 63 C0): a 32-bit rotate may set
                            // the high bit (negative int); re-extend to the
                            // canonical sign-extended 64-bit int form the value
                            // ABI expects (mirrors the Math.min int path).
                            self.buf.emit(&[0x48, 0x63, 0xC0]);
                            self.push_from_rax();
                        }
                        // ===== INTRINSIC REGION END: INT_BITS =====

                        // ===== INTRINSIC REGION BEGIN: LONG_BITS =====
                        // java.lang.Long bit ops (Phase 1b). Each long operand
                        // occupies one 64-bit JIT stack slot; load_slot_to_reg
                        // loads the full 64 bits. All instructions below are
                        // REX.W-prefixed (0x48) so they operate on the whole
                        // 64-bit value. bitCount/numberOfLeadingZeros/
                        // numberOfTrailingZeros return an int (0..=64) which
                        // is left in EAX with the upper 32 bits cleared.
                        else if callee_entry == crate::JitIntrinsic::LongBitCount.as_entry() {
                            // Long.bitCount(j): 64-bit POPCNT. Matcher gates
                            // this on has_popcnt(), so the instruction is
                            // always valid here. Result 0..=64 fits in EAX.
                            let arg = self.pop_stack();
                            self.load_slot_to_reg(RAX, arg);
                            // POPCNT RAX, RAX: F3 48 0F B8 C0
                            self.buf.emit(&[0xF3, 0x48, 0x0F, 0xB8, 0xC0]);
                            self.push_from_rax();
                        } else if callee_entry
                            == crate::JitIntrinsic::LongNumberOfLeadingZeros.as_entry()
                        {
                            // Long.numberOfLeadingZeros(j).
                            let arg = self.pop_stack();
                            self.load_slot_to_reg(RAX, arg);
                            if crate::x64::has_lzcnt() {
                                // LZCNT RAX, RAX: F3 48 0F BD C0 — defined to
                                // yield 64 for a zero input, matching the JDK.
                                self.buf.emit(&[0xF3, 0x48, 0x0F, 0xBD, 0xC0]);
                            } else {
                                // BSR fallback. BSR RCX, RAX sets ZF iff the
                                // source is zero and otherwise leaves the
                                // highest set-bit index (0..=63) in RCX.
                                //   nlz = 63 - index   (for a non-zero input)
                                //   nlz = 64           (for a zero input)
                                // 63 - index == index ^ 63 for index in 0..=63,
                                // computed with XOR so RAX is left untouched
                                // for the TEST that re-derives the zero case.
                                // BSR RCX, RAX: 48 0F BD C8
                                self.buf.emit(&[0x48, 0x0F, 0xBD, 0xC8]);
                                // XOR ECX, 63: 83 F1 3F — ECX = 63 - index
                                // (garbage if input was 0; fixed up below).
                                self.buf.emit(&[0x83, 0xF1, 0x3F]);
                                // MOV EDX, 64: BA 40 00 00 00
                                self.buf.emit(&[0xBA]);
                                self.buf.emit(&64i32.to_le_bytes());
                                // TEST RAX, RAX: 48 85 C0 — ZF iff input == 0.
                                self.buf.emit(&[0x48, 0x85, 0xC0]);
                                // CMOVZ RCX, RDX: 48 0F 44 CA — input 0 → 64.
                                self.buf.emit(&[0x48, 0x0F, 0x44, 0xCA]);
                                // MOV EAX, ECX: 89 C8
                                self.buf.emit(&[0x89, 0xC8]);
                            }
                            self.push_from_rax();
                        } else if callee_entry
                            == crate::JitIntrinsic::LongNumberOfTrailingZeros.as_entry()
                        {
                            // Long.numberOfTrailingZeros(j).
                            let arg = self.pop_stack();
                            self.load_slot_to_reg(RAX, arg);
                            if crate::x64::has_bmi1() {
                                // TZCNT RAX, RAX: F3 48 0F BC C0 — defined to
                                // yield 64 for a zero input, matching the JDK.
                                self.buf.emit(&[0xF3, 0x48, 0x0F, 0xBC, 0xC0]);
                            } else {
                                // BSF fallback. BSF RCX, RAX sets ZF iff the
                                // source is zero and otherwise leaves the
                                // lowest set-bit index (0..=63) in RCX, which
                                // is exactly ntz for a non-zero input. For a
                                // zero input the result must be 64.
                                // BSF RCX, RAX: 48 0F BC C8
                                self.buf.emit(&[0x48, 0x0F, 0xBC, 0xC8]);
                                // MOV EDX, 64: BA 40 00 00 00
                                self.buf.emit(&[0xBA]);
                                self.buf.emit(&64i32.to_le_bytes());
                                // TEST RAX, RAX: 48 85 C0 — ZF iff input == 0.
                                self.buf.emit(&[0x48, 0x85, 0xC0]);
                                // CMOVZ RCX, RDX: 48 0F 44 CA — input 0 → 64.
                                self.buf.emit(&[0x48, 0x0F, 0x44, 0xCA]);
                                // MOV EAX, ECX: 89 C8
                                self.buf.emit(&[0x89, 0xC8]);
                            }
                            self.push_from_rax();
                        } else if callee_entry == crate::JitIntrinsic::LongReverseBytes.as_entry() {
                            // Long.reverseBytes(j): 64-bit BSWAP.
                            let arg = self.pop_stack();
                            self.load_slot_to_reg(RAX, arg);
                            // BSWAP RAX: 48 0F C8
                            self.buf.emit(&[0x48, 0x0F, 0xC8]);
                            self.push_from_rax();
                        } else if callee_entry == crate::JitIntrinsic::LongHighestOneBit.as_entry()
                        {
                            // Long.highestOneBit(j): 1L << bitIndex of the MSB,
                            // or 0 for a zero input. BSR leaves the index in
                            // RCX; SHL forms the mask; a CMOVZ keyed on the
                            // original input restores 0 for the zero case.
                            let arg = self.pop_stack();
                            self.load_slot_to_reg(RAX, arg);
                            // BSR RCX, RAX: 48 0F BD C8 — RCX = MSB index.
                            self.buf.emit(&[0x48, 0x0F, 0xBD, 0xC8]);
                            // MOV EDX, 1: BA 01 00 00 00 (RDX = 1, upper bits 0).
                            self.buf.emit(&[0xBA]);
                            self.buf.emit(&1i32.to_le_bytes());
                            // SHL RDX, CL: 48 D3 E2 — RDX = 1 << index
                            // (garbage if input was 0; fixed up below).
                            self.buf.emit(&[0x48, 0xD3, 0xE2]);
                            // XOR ECX, ECX: 31 C9 — RCX = 0 (zero-input result).
                            self.buf.emit(&[0x31, 0xC9]);
                            // TEST RAX, RAX: 48 85 C0 — ZF iff input == 0.
                            self.buf.emit(&[0x48, 0x85, 0xC0]);
                            // CMOVZ RDX, RCX: 48 0F 44 D1 — input 0 → 0.
                            self.buf.emit(&[0x48, 0x0F, 0x44, 0xD1]);
                            // MOV RAX, RDX: 48 89 D0
                            self.buf.emit(&[0x48, 0x89, 0xD0]);
                            self.push_from_rax();
                        } else if callee_entry == crate::JitIntrinsic::LongLowestOneBit.as_entry() {
                            // Long.lowestOneBit(j): j & -j. Naturally yields 0
                            // for a zero input, matching the JDK.
                            let arg = self.pop_stack();
                            self.load_slot_to_reg(RAX, arg);
                            // MOV RCX, RAX: 48 89 C1
                            self.buf.emit(&[0x48, 0x89, 0xC1]);
                            // NEG RCX: 48 F7 D9 — RCX = -j
                            self.buf.emit(&[0x48, 0xF7, 0xD9]);
                            // AND RAX, RCX: 48 21 C8 — RAX = j & -j
                            self.buf.emit(&[0x48, 0x21, 0xC8]);
                            self.push_from_rax();
                        } else if callee_entry == crate::JitIntrinsic::LongCompare.as_entry() {
                            // Long.compare(x, y): branchless signed
                            // (x > y) - (x < y). The stack holds x (deeper)
                            // then y. SETcc reads the flags from CMP without
                            // disturbing them; the int result lands in EAX.
                            let y = self.pop_stack();
                            let x = self.pop_stack();
                            self.load_slot_to_reg(RAX, x);
                            self.load_slot_to_reg(RCX, y);
                            // CMP RAX, RCX: 48 39 C8 — signed 64-bit compare.
                            self.buf.emit(&[0x48, 0x39, 0xC8]);
                            // SETG AL:  0F 9F C0 — AL = 1 if x > y.
                            self.buf.emit(&[0x0F, 0x9F, 0xC0]);
                            // SETL DL:  0F 9C C2 — DL = 1 if x < y.
                            self.buf.emit(&[0x0F, 0x9C, 0xC2]);
                            // MOVZX EAX, AL: 0F B6 C0
                            self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                            // MOVZX EDX, DL: 0F B6 D2
                            self.buf.emit(&[0x0F, 0xB6, 0xD2]);
                            // SUB EAX, EDX: 29 D0 → -1, 0 or 1.
                            self.buf.emit(&[0x29, 0xD0]);
                            self.push_from_rax();
                        } else if callee_entry == crate::JitIntrinsic::LongRotateLeft.as_entry()
                            || callee_entry == crate::JitIntrinsic::LongRotateRight.as_entry()
                        {
                            // Long.rotateLeft(i, distance) / rotateRight:
                            // ROL/ROR RAX, CL. x86 masks CL & 0x3f for a 64-bit
                            // rotate, byte-identical to the JDK (rotation mod 64).
                            // Descriptor is (JI)J: the long value (deeper) then
                            // the int distance (top), each one JIT stack slot.
                            let dist = self.pop_stack();
                            let val = self.pop_stack();
                            self.load_slot_to_reg(RAX, val);
                            self.load_slot_to_reg(RCX, dist);
                            // ROL RAX, CL: 48 D3 C0 ; ROR RAX, CL: 48 D3 C8.
                            let modrm =
                                if callee_entry == crate::JitIntrinsic::LongRotateLeft.as_entry() {
                                    0xC0u8
                                } else {
                                    0xC8u8
                                };
                            self.buf.emit(&[0x48, 0xD3, modrm]);
                            self.push_from_rax();
                        }
                        // ===== INTRINSIC REGION END: LONG_BITS =====

                        // ===== INTRINSIC REGION BEGIN: ARRAYCOPY =====
                        //
                        // De-spec guard: a call site whose src/dst are ALWAYS
                        // a reference-element array (e.g. `char[][]`) fails
                        // Guard 4 below on every single invocation, so this
                        // speculative fast path deopts every call. Each deopt
                        // evicts the artifact and triggers a background
                        // recompile that re-emits the SAME unconditional
                        // guard, so the method deopts forever — creating a
                        // continuous compile/evict race window. If the
                        // resume's epoch-staleness check ever loses that race
                        // (the live epoch was bumped by an in-flight
                        // recompile), the deopt falls back to the documented
                        // whole-method re-run, which DOUBLE-EXECUTES every
                        // side effect the method already performed before
                        // reaching this call (e.g. a `stack[ptr--]` decrement
                        // already committed to the heap) — the mechanism
                        // behind the JDT `Parser` stack-corruption bug
                        // (fixed-suite-bugs/jasper-jdt-parser-arrayindexoutofbounds.md).
                        // `real_frame_deopt_resume_and_despeculate` already
                        // records this bci in the de-spec registry after
                        // `PER_BCI_DESPEC_LIMIT` deopts, exactly like the
                        // loop-header speculative-BCE guards (see
                        // `despec_contains` above); this intrinsic just needs
                        // to honor it on recompile — bail to the generic
                        // (non-speculative) call dispatch below instead of
                        // re-emitting a guard proven to always fail.
                        else if callee_entry == crate::JitIntrinsic::ArraycopyPrimitive.as_entry()
                            && !crate::deopt::despec_contains(&self.method_key, pc as u32)
                        {
                            // Phase 2 — System.arraycopy(src, srcPos, dst,
                            // dstPos, len). The descriptor is type-erased;
                            // the element kind is only known at runtime.
                            //
                            // Strategy (roadmap §3.3/§3.4): inline a
                            // primitive fast path. The emitted code performs
                            // a runtime dispatch — null checks, an
                            // is-array + same-primitive-element-kind check,
                            // and the five fused bounds checks of
                            // `native_system_arraycopy`. Whenever ANY guard
                            // is unsatisfied — null receiver, non-array,
                            // reference array, mismatched/incompatible
                            // element kinds, or an out-of-bounds position —
                            // control branches to the uncommon-trap deopt
                            // stub (DEOPT_REASON_BOUNDS_CHECK = 2). That
                            // re-runs the whole method in the interpreter,
                            // which dispatches `System.arraycopy` through
                            // the native registry. The native impl throws
                            // NullPointerException / ArrayStoreException /
                            // ArrayIndexOutOfBoundsException and applies the
                            // GC store barrier exactly — so correctness for
                            // every bailed case is delegated verbatim and
                            // reference arrays are never inlined.
                            //
                            // The proven-safe primitive case copies via
                            // `REP MOVSB` with memmove semantics: when src
                            // and dst are the SAME array and dstPos > srcPos
                            // the regions may overlap forward, so the copy
                            // runs backward (STD) in that case and forward
                            // (CLD) otherwise. Distinct arrays are distinct
                            // allocations and never overlap.
                            //
                            // Operand stack (deepest first): src, srcPos,
                            // dst, dstPos, len.
                            self.flush_scratch_registers();
                            // Step 6: snapshot the pre-pop JVM operand stack
                            // (src, srcPos, dst, dstPos, len) so a bail from
                            // any null/bounds guard resumes precisely at this
                            // invokestatic bci rather than re-running from entry.
                            if crate::deopt_real_enabled() {
                                self.snapshot_pre_intrinsic_call(
                                    pc,
                                    crate::deopt::DeoptReason::BoundsCheck,
                                );
                            }
                            let len_slot = self.pop_stack();
                            let dst_pos_slot = self.pop_stack();
                            let dst_slot = self.pop_stack();
                            let src_pos_slot = self.pop_stack();
                            let src_slot = self.pop_stack();

                            // Pin all five operands into owned frame scratch
                            // slots. After the five pops `next_spill_offset`
                            // sits below the slots the operands occupied, so
                            // these offsets are guaranteed in-frame (the
                            // call site had >=5 operand-stack entries, hence
                            // max_stack >= 5). They are scratch-only:
                            // arraycopy pushes nothing, so the next bytecode
                            // re-allocates spill slots from the same base.
                            //
                            // The base is pushed BELOW every operand's own
                            // frame home, so the five stores below cannot land
                            // on a slot one of them still lives in.
                            //
                            // The overlap is real and this is where it comes
                            // from: `pop_stack` reclaims a Frame slot that sits
                            // at the top of the spill region, so the five pops
                            // just above rewound `next_spill_offset` back OVER
                            // the very homes `flush_scratch_registers` spilled
                            // the oop operands into. Taking the base from
                            // `next_spill_offset` therefore aliases them by
                            // construction.
                            //
                            // The emitter used to work around that for its OWN
                            // reads only, by loading all five into distinct
                            // GPRs before storing any (kept below — it costs
                            // nothing and defends the ordering directly). But
                            // the aliasing has a SECOND consumer that the
                            // workaround does not reach: the deopt snapshot
                            // taken above names each operand's ORIGINAL frame
                            // home, and reads it when the bail actually fires —
                            // long after `s_src_pos`'s store has overwritten
                            // `dst`'s home with srcPos. A reference-array
                            // `arraycopy` (which always bails here, by design)
                            // then resumed in the interpreter with `Object(1)`
                            // — the literal srcPos — where `dst` belonged: not
                            // a plausible heap pointer, degraded to null by
                            // `CompactValue::to_value`, and `System.arraycopy`
                            // threw NullPointerException. `RMethodSiteCache`'s
                            // `mixedRefAndPrimitive` is the witness; the bogus
                            // payload tracks srcPos exactly (`srcPos=2` gives
                            // `Object(2)`).
                            //
                            // Removing the overlap fixes both consumers at
                            // once, which is why it is done here rather than by
                            // teaching the snapshot about the scratch homes.
                            let mut scratch_base = self.next_spill_offset;
                            for slot in [src_slot, src_pos_slot, dst_slot, dst_pos_slot, len_slot] {
                                if let crate::x64::StackSlot::Frame(off) = slot {
                                    scratch_base = scratch_base.max(off.saturating_add(8));
                                }
                            }
                            if !self.spill_range_fits(scratch_base, 5) {
                                return false;
                            }
                            let s_src = scratch_base;
                            let s_src_pos = scratch_base + 8;
                            let s_dst = scratch_base + 16;
                            let s_dst_pos = scratch_base + 24;
                            let s_len = scratch_base + 32;
                            // ALIASING HAZARD: these scratch homes can overlap
                            // the operands' OWN frame homes. `flush_scratch_
                            // registers()` above spills any CalleeSaved *oop*
                            // operand (here src and dst) to frame slots taken
                            // from `next_spill_offset`; the five pops then rewind
                            // `next_spill_offset` back over those very slots, so
                            // e.g. `src_slot`/`dst_slot` may be `Frame(s_src)` /
                            // `Frame(s_src_pos)`. Writing the scratch homes in
                            // operand order would corrupt a not-yet-read operand:
                            // storing s_src_pos (=srcPos) overwrites dst's spilled
                            // home BEFORE s_dst reads it, leaving s_dst = srcPos
                            // (a small int) — guard-2 then bails to native when
                            // srcPos==0, or dereferences the bogus pointer and
                            // SIGSEGVs when srcPos!=0. Defeat the aliasing by
                            // loading ALL five operands into distinct scratch
                            // GPRs FIRST, then storing. RAX/RCX/RDX/R10/R11 are
                            // never local-mapped (LOCAL_REGS is RBX/R12..R15
                            // [+RSI/RDI on SysV]) and hold no deferred-Scratch
                            // value after the flush, so no load can clobber an
                            // operand still pending a read.
                            self.load_slot_to_reg(RAX, src_slot);
                            self.load_slot_to_reg(RCX, src_pos_slot);
                            self.load_slot_to_reg(RDX, dst_slot);
                            self.load_slot_to_reg(R10, dst_pos_slot);
                            self.load_slot_to_reg(R11, len_slot);
                            self.emit_store_local(s_src, RAX);
                            self.emit_store_local(s_src_pos, RCX);
                            self.emit_store_local(s_dst, RDX);
                            self.emit_store_local(s_dst_pos, R10);
                            self.emit_store_local(s_len, R11);

                            // Collect every "bail to native" branch patch
                            // here; they are all wired to one shared deopt
                            // stub (reason 2) after the inline body.
                            let mut bail_patches: Vec<usize> = Vec::new();

                            // --- Guard 1: src != null ---
                            self.emit_load_local(RAX, s_src);
                            self.emit_test_r64_r64(RAX);
                            bail_patches.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                            // --- Guard 2: dst != null ---
                            self.emit_load_local(RCX, s_dst);
                            self.emit_test_r64_r64(RCX);
                            bail_patches.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                            // --- Guards 3 and 4: both are arrays, of the SAME
                            // PRIMITIVE element kind ---
                            //
                            // `kind` and `element_type` are BOTH in the one
                            // byte at `KIND_TAGS_BYTE_OFFSET` — `kind` in bits
                            // 0..2 (`KIND_TAG_BYTE_MASK`), `element_type` in
                            // bits 2..6. They used to be separate bytes at
                            // offsets 4 and 5, and this code still read those
                            // two literals after the header shrank 24 -> 16 on
                            // 2026-08-07 and the quartet moved into the mark
                            // word.
                            //
                            // Offset 4 is now `shape` — an ARRAY'S LENGTH. The
                            // same function reads the length from that very
                            // offset (via `ARRAY_LENGTH_OFFSET`) forty lines
                            // below, so the "is this an array" guard was
                            // testing the length's low byte and the "element
                            // type" was the next length byte. That does not
                            // fail safe: for a length whose low byte is 1 and
                            // whose second byte is >= 4 — 1025 = 0x0401 — both
                            // tests PASS and the element width comes out as
                            // `1 << ((4 - 4) & 3)` = one byte. `arraycopy` on a
                            // `long[1025]` moved 1025 bytes instead of 8200 and
                            // returned normally: no exception, no crash, a
                            // silently truncated copy.
                            // `probes/ArraycopyHeaderOffsetProbe.java` is the
                            // repro — mismatch at index 128 for `long[1025]`
                            // and 256 for `int[1025]`, while 1024 / 300 / 257
                            // pass, which is why this hid.
                            //
                            // Read the constants, as every other header access
                            // in this file already does.
                            const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8;

                            // MOVZX EDX, BYTE [RAX + KIND_TAGS]  (src tags)
                            self.buf.emit(&[0x0F, 0xB6, 0x50, KIND_TAGS]);
                            // MOV R10D, EDX — keep the whole byte; EDX is about
                            // to be masked down to the kind bits.
                            self.buf.emit(&[0x41, 0x89, 0xD2]);
                            // AND EDX, KIND_TAG_BYTE_MASK
                            self.buf
                                .emit(&[0x83, 0xE2, cratonvm_types::KIND_TAG_BYTE_MASK]);
                            // CMP EDX, ObjectKind::Array
                            self.buf
                                .emit(&[0x83, 0xFA, cratonvm_types::ObjectKind::Array as u8]);
                            bail_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                            // MOVZX EDX, BYTE [RCX + KIND_TAGS]  (dst tags)
                            self.buf.emit(&[0x0F, 0xB6, 0x51, KIND_TAGS]);
                            // MOV R11D, EDX
                            self.buf.emit(&[0x41, 0x89, 0xD3]);
                            self.buf
                                .emit(&[0x83, 0xE2, cratonvm_types::KIND_TAG_BYTE_MASK]);
                            self.buf
                                .emit(&[0x83, 0xFA, cratonvm_types::ObjectKind::Array as u8]);
                            bail_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                            // element_type = (tags >> 2) & 0xF. ArrayElementType:
                            // Reference=0, Boolean=4, Char=5, Float=6, Double=7,
                            // Byte=8, Short=9, Int=10, Long=11 — four bits.
                            // SHR R10D, 2 ; AND R10D, 0xF   (src)
                            self.buf.emit(&[0x41, 0xC1, 0xEA, 0x02]);
                            self.buf.emit(&[0x41, 0x83, 0xE2, 0x0F]);
                            // SHR R11D, 2 ; AND R11D, 0xF   (dst)
                            self.buf.emit(&[0x41, 0xC1, 0xEB, 0x02]);
                            self.buf.emit(&[0x41, 0x83, 0xE3, 0x0F]);
                            // CMP R10D, R11D → element kinds must be equal
                            self.buf.emit(&[0x45, 0x39, 0xDA]);
                            bail_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNE
                                                                                // CMP R10D, 4 → primitive kinds are 4..=11; a
                                                                                // value < 4 means Reference (0) — bail (the GC
                                                                                // store barrier / ArrayStoreException make
                                                                                // reference copies unsafe to inline).
                            self.buf.emit(&[0x41, 0x83, 0xFA, 0x04]);
                            bail_patches.push(self.emit_jcc_rel32_patch(0x82)); // JB (unsigned <)
                                                                                // MOV EDX, R10D — the shift math below operates
                                                                                // on EDX, as it did when EDX held the element
                                                                                // type directly.
                            self.buf.emit(&[0x44, 0x89, 0xD2]);

                            // shift = (element_type - 4) & 3, where
                            //   width == 1 << shift  for every primitive
                            //   kind (verified: 4→0,5→1,6→2,7→3,8→0,9→1,
                            //   10→2,11→3). Stash the shift in R11 (scratch,
                            //   untouched until the copy below).
                            // SUB EDX, 4
                            self.buf.emit(&[0x83, 0xEA, 0x04]);
                            // AND EDX, 3
                            self.buf.emit(&[0x83, 0xE2, 0x03]);
                            // MOV R11D, EDX
                            self.buf.emit(&[0x41, 0x89, 0xD3]);

                            // --- Guard 5: bounds checks ---
                            // Load srcPos, dstPos, len sign-extended to
                            // 64-bit so the pos+len additions cannot
                            // overflow.
                            // srcPos → RAX
                            self.emit_load_local(RAX, s_src_pos);
                            self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
                                                                // dstPos → RDX
                            self.emit_load_local(RDX, s_dst_pos);
                            self.buf.emit(&[0x48, 0x63, 0xD2]); // MOVSXD RDX, EDX
                                                                // len → RCX
                            self.emit_load_local(RCX, s_len);
                            self.buf.emit(&[0x48, 0x63, 0xC9]); // MOVSXD RCX, ECX

                            // srcPos < 0 ?  TEST RAX,RAX; JS bail
                            self.emit_test_r64_r64(RAX);
                            bail_patches.push(self.emit_jcc_rel32_patch(0x88)); // JS
                                                                                // dstPos < 0 ?  TEST RDX,RDX; JS bail
                            self.emit_test_r64_r64(RDX);
                            bail_patches.push(self.emit_jcc_rel32_patch(0x88)); // JS
                                                                                // len < 0 ?  TEST RCX,RCX; JS bail
                            self.emit_test_r64_r64(RCX);
                            bail_patches.push(self.emit_jcc_rel32_patch(0x88)); // JS

                            // srcPos + len > src.length ?
                            // R10 = srcPos + len
                            self.buf.emit(&[0x49, 0x89, 0xC2]); // MOV R10, RAX
                            self.buf.emit(&[0x49, 0x01, 0xCA]); // ADD R10, RCX
                                                                // RAX = src ptr; EAX = src.length (zero-extended,
                                                                // so the 64-bit value is non-negative).
                            self.emit_load_local(RAX, s_src);
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            self.buf.emit(&[0x8B, 0x40, ARRAY_LENGTH_OFFSET as u8]); // MOV EAX,[RAX+12]
                                                                                     // CMP R10, RAX  (srcPos+len vs src.length)
                            self.buf.emit(&[0x49, 0x39, 0xC2]);
                            bail_patches.push(self.emit_jcc_rel32_patch(0x8F)); // JG (signed >)

                            // dstPos + len > dst.length ?
                            self.buf.emit(&[0x49, 0x89, 0xD2]); // MOV R10, RDX
                            self.buf.emit(&[0x49, 0x01, 0xCA]); // ADD R10, RCX
                            self.emit_load_local(RAX, s_dst);
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            self.buf.emit(&[0x8B, 0x40, ARRAY_LENGTH_OFFSET as u8]); // MOV EAX,[RAX+12]
                            self.buf.emit(&[0x49, 0x39, 0xC2]); // CMP R10, RAX
                            bail_patches.push(self.emit_jcc_rel32_patch(0x8F)); // JG

                            // --- len == 0 fast exit ---
                            // All bounds are validated; an empty copy is a
                            // no-op. RCX still holds the sign-extended len.
                            self.emit_test_r64_r64(RCX);
                            let zero_len_skip = self.emit_jcc_rel32_patch(0x84); // JZ → done

                            // --- compute byte addresses & count ---
                            // Preserve RSI / RDI: on Windows they are
                            // callee-saved AND used by the local register
                            // allocator (LOCAL_REGS), so they may hold live
                            // locals. PUSH/POP brackets the REP MOVSB; no
                            // CALL occurs in between, so RSP stays balanced.
                            // PUSH RSI ; PUSH RDI
                            self.buf.emit(&[0x56, 0x57]);

                            // shift → CL
                            // MOV ECX, R11D
                            self.buf.emit(&[0x44, 0x89, 0xD9]);

                            // srcAddr = src + HEADER_SIZE + (srcPos << shift)
                            self.emit_load_local(RSI, s_src_pos);
                            self.buf.emit(&[0x48, 0x63, 0xF6]); // MOVSXD RSI, ESI
                            self.buf.emit(&[0x48, 0xD3, 0xE6]); // SHL RSI, CL
                            self.emit_load_local(RAX, s_src);
                            self.buf.emit(&[0x48, 0x01, 0xC6]); // ADD RSI, RAX
                                                                // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            self.buf.emit(&[0x48, 0x83, 0xC6, HEADER_SIZE as u8]); // ADD RSI, HEADER_SIZE

                            // dstAddr = dst + HEADER_SIZE + (dstPos << shift)
                            self.emit_load_local(RDI, s_dst_pos);
                            self.buf.emit(&[0x48, 0x63, 0xFF]); // MOVSXD RDI, EDI
                            self.buf.emit(&[0x48, 0xD3, 0xE7]); // SHL RDI, CL
                            self.emit_load_local(RAX, s_dst);
                            self.buf.emit(&[0x48, 0x01, 0xC7]); // ADD RDI, RAX
                                                                // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            self.buf.emit(&[0x48, 0x83, 0xC7, HEADER_SIZE as u8]); // ADD RDI, HEADER_SIZE

                            // byteCount = len << shift  → RDX (kept for the
                            // backward-copy adjust, then copied into RCX
                            // for REP).
                            self.emit_load_local(RDX, s_len);
                            self.buf.emit(&[0x48, 0x63, 0xD2]); // MOVSXD RDX, EDX
                            self.buf.emit(&[0x48, 0xD3, 0xE2]); // SHL RDX, CL

                            // --- direction select ---
                            // Overlap is possible only within the SAME
                            // array; distinct arrays are distinct heap
                            // allocations. Copy backward iff
                            //   src_ptr == dst_ptr  &&  dstPos > srcPos.
                            // Otherwise forward is always memmove-correct.
                            self.emit_load_local(RAX, s_src);
                            self.emit_load_local(RCX, s_dst);
                            // CMP RAX, RCX
                            self.buf.emit(&[0x48, 0x39, 0xC8]);
                            let fwd_if_diff = self.emit_jcc_rel32_patch(0x85); // JNE → forward
                                                                               // same array: compare dstPos vs srcPos (signed
                                                                               // 32-bit; both already validated >= 0).
                            self.emit_load_local(RAX, s_dst_pos);
                            self.emit_load_local(RCX, s_src_pos);
                            // CMP EAX, ECX  (dstPos vs srcPos)
                            self.buf.emit(&[0x39, 0xC8]);
                            let fwd_if_le = self.emit_jcc_rel32_patch(0x8E); // JLE → forward

                            // --- backward copy (STD) ---
                            // Point RSI/RDI at the LAST byte of each region:
                            //   addr += byteCount - 1.
                            // LEA RSI, [RSI + RDX - 1]
                            self.buf.emit(&[0x48, 0x8D, 0x74, 0x16, 0xFF]);
                            // LEA RDI, [RDI + RDX - 1]
                            self.buf.emit(&[0x48, 0x8D, 0x7C, 0x17, 0xFF]);
                            // MOV RCX, RDX  (byte count)
                            self.buf.emit(&[0x48, 0x89, 0xD1]);
                            // STD ; REP MOVSB ; CLD
                            self.buf.emit(&[0xFD, 0xF3, 0xA4, 0xFC]);
                            let backward_done = self.emit_jmp_rel32_patch();

                            // --- forward copy (CLD) ---
                            self.patch_rel32_to_here(fwd_if_diff);
                            self.patch_rel32_to_here(fwd_if_le);
                            // MOV RCX, RDX  (byte count)
                            self.buf.emit(&[0x48, 0x89, 0xD1]);
                            // CLD ; REP MOVSB
                            self.buf.emit(&[0xFC, 0xF3, 0xA4]);

                            // Both copy paths converge here.
                            self.patch_rel32_to_here(backward_done);
                            // POP RDI ; POP RSI
                            self.buf.emit(&[0x5F, 0x5E]);

                            // --- done ---
                            self.patch_rel32_to_here(zero_len_skip);

                            // Every bail branch (null, non-array,
                            // reference-element array, mismatched kind, or
                            // out-of-bounds) is a NORMAL, valid outcome for
                            // `System.arraycopy` — either a real exception or
                            // a successful reference-array copy — not an
                            // "uncommon" condition that should re-run the
                            // method. Route them through the SAME safe
                            // native-dispatch CALL an ordinary
                            // non-intrinsic `invokestatic` site uses
                            // (registered for this exact pc alongside the
                            // intrinsic — see the `ArraycopyPrimitive` match
                            // arm in `jit_scan`'s caller), so a guard failure
                            // throws/succeeds via normal call semantics
                            // instead of a deopt trap whose only resume
                            // strategy (whole-method re-run) can
                            // DOUBLE-EXECUTE a side effect the method already
                            // performed before reaching this call (e.g. a
                            // `stack[ptr--]` decrement already committed to
                            // the heap) — the mechanism behind the JDT
                            // `Parser` stack-corruption bug (fixed-suite-bugs/
                            // jasper-jdt-parser-arrayindexoutofbounds.md).
                            // Falls back to the historical deopt trap only if
                            // the dispatch info wasn't registered (defensive;
                            // should not happen for this intrinsic).
                            let dispatch_info = self
                                .invoke_info_idx
                                .get(&pc)
                                .map(|&i| self.invoke_info[i].1);
                            match dispatch_info.map(|info| (info, self.reserve_spill_slots(5))) {
                                Some((info, Some(args_base))) => {
                                    let skip_dispatch = self.emit_jmp_rel32_patch();
                                    for &patch in &bail_patches {
                                        self.patch_rel32_to_here(patch);
                                    }
                                    // Reload the five original operands from
                                    // their pinned scratch homes (untouched
                                    // by the guard sequence above) into the
                                    // freshly reserved, contiguous args
                                    // buffer in the layout `jit_invoke_
                                    // dispatch` expects: arg[0] at the
                                    // highest offset (lowest address),
                                    // arg[n-1] at the lowest offset —
                                    // mirrors the generic invokestatic
                                    // dispatch site's buffer construction.
                                    // ALIASING HAZARD: `args_base` reuses the
                                    // SAME frame region as `s_src..s_len`
                                    // (arraycopy's scratch homes never
                                    // advanced `next_spill_offset` — see the
                                    // "arraycopy pushes nothing" comment
                                    // above), and the target buffer order is
                                    // the REVERSE of the scratch-home order,
                                    // so a naive per-index load-then-store
                                    // would overwrite a not-yet-read home
                                    // (e.g. writing arg[0] to `s_len`'s
                                    // address before arg[4] has read `s_len`).
                                    // Defeat it exactly like the fast-path
                                    // guard setup above: load ALL five
                                    // operands into distinct registers FIRST,
                                    // then store.
                                    self.emit_load_local(RAX, s_src);
                                    self.emit_load_local(RCX, s_src_pos);
                                    self.emit_load_local(RDX, s_dst);
                                    self.emit_load_local(R10, s_dst_pos);
                                    self.emit_load_local(R11, s_len);
                                    self.emit_store_local(args_base + 4 * 8, RAX); // Cast: x86-64 immediate encoding
                                    self.emit_store_local(args_base + 3 * 8, RCX); // Cast: x86-64 immediate encoding
                                    self.emit_store_local(args_base + 2 * 8, RDX); // Cast: x86-64 immediate encoding
                                    self.emit_store_local(args_base + 1 * 8, R10); // Cast: x86-64 immediate encoding
                                    self.emit_store_local(args_base, R11);
                                    // SAFETY: info comes from self.invoke_info, which holds
                                    // pointers to JitInvokeInfo structs kept alive by the
                                    // caller for the duration of compilation.
                                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                    self.emit_mov_imm64(ARG_REGS[1], info as *const _ as i64); // Cast: function pointer for JIT call target
                                    let buf_start = args_base + 4 * 8; // Cast: x86-64 immediate encoding
                                    self.emit_lea_frame_slot(ARG_REGS[2], buf_start);
                                    self.emit_mov_imm32_sx(ARG_REGS[3], 5);
                                    self.emit_pre_safepoint_spill();
                                    self.emit_call_absolute(self.helpers.invoke_dispatch);
                                    self.emit_oop_map_for_safepoint();
                                    self.emit_post_invoke_exception_check(b'V');
                                    // arraycopy returns void: nothing pushed;
                                    // the args buffer is dead, reclaim it.
                                    self.next_spill_offset = args_base;
                                    self.patch_rel32_to_here(skip_dispatch);
                                }
                                Some((_, None)) => {
                                    // Spill region exhausted — bail the whole
                                    // compile (always safe: the method falls
                                    // back to the interpreter).
                                    self.fail("singlepass-codegen/arraycopy-args-spill-exhausted");
                                    return false;
                                }
                                None => {
                                    // Wire every bail branch to a shared deopt stub
                                    // (reason 2 = DEOPT_REASON_BOUNDS_CHECK). The
                                    // emit_deopt_stubs pass coalesces equal
                                    // (bci, reason) pairs into one stub, so all the
                                    // bail edges share a single trap.
                                    for patch in bail_patches {
                                        self.deopt_stubs.push((patch, pc, 2));
                                    }
                                }
                            }
                        }
                        // ===== INTRINSIC REGION END: ARRAYCOPY =====

                        // ===== INTRINSIC REGION BEGIN: ARRAYS_OPS =====
                        // java.util.Arrays.fill / Arrays.equals — Phase 4a.
                        //
                        // Array layout (cratonvm_types): a 40-byte object
                        // header with the i32 element count at offset 12
                        // (ARRAY_LENGTH_OFFSET); element data begins at
                        // HEADER_SIZE (40). Primitive elements are packed at
                        // their natural width (byte=1, char/short=2, int=4,
                        // long=8).
                        //
                        // Register discipline: `flush_scratch_registers()`
                        // ran at the top of the 0xb8 handler, so no Java
                        // local lives in a scratch GPR — RAX/RCX/RDX/R8/R9/
                        // R10/R11 are all free to clobber. RDI and RSI ARE
                        // in `LOCAL_REGS` (Windows callee-saved), so the
                        // REP STOS/CMPS sequences PUSH/POP them to keep the
                        // owning locals intact. There is no CALL and no
                        // safepoint between the PUSH and the POP, so the
                        // transient RSP adjustment is invisible to the GC.
                        else if callee_entry == crate::JitIntrinsic::ArraysFill1.as_entry()
                            || callee_entry == crate::JitIntrinsic::ArraysFill2.as_entry()
                            || callee_entry == crate::JitIntrinsic::ArraysFill4.as_entry()
                            || callee_entry == crate::JitIntrinsic::ArraysFill8.as_entry()
                        {
                            // Arrays.fill(array, value) : void
                            //
                            // Operand stack (deepest first): [array, value].
                            // Pop value, then array.
                            let value_slot = self.pop_stack();
                            let array_slot = self.pop_stack();

                            // Array pointer → RAX for the null check.
                            self.load_slot_to_reg(RAX, array_slot);
                            // Null array → JDK throws NullPointerException.
                            // Reuse the shared null-check stub (sets
                            // JIT_PENDING_NPE, deopts out): TEST RAX,RAX +
                            // JZ stub. This is an intrinsic, not a plain
                            // array opcode, so no precise JEP-358 array action
                            // applies — record NONE (unmessaged NPE), matching
                            // the prior behaviour.
                            self.emit_null_check_array_load(npe_action::NONE);

                            // Save array base in R8 (RAX is needed as the
                            // STOS source register).
                            // MOV R8, RAX  (49 89 C0)
                            self.buf.emit(&[0x49, 0x89, 0xC0]);
                            // Element count → ECX (zero-extends to RCX, the
                            // REP counter). MOV ECX, [RAX+12]  (8B 48 0C)
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            self.buf.emit(&[0x8B, 0x48, ARRAY_LENGTH_OFFSET as u8]);

                            // Fill value → RAX. STOSB/W/D/Q use AL/AX/EAX/
                            // RAX, all sub-registers of RAX, so a single
                            // 64-bit load serves every width.
                            self.load_slot_to_reg(RAX, value_slot);

                            // Save RDI (a Java local may live there), point
                            // it at the element data, run REP STOS, restore.
                            // PUSH RDI  (57)
                            self.buf.emit_byte(0x57);
                            // LEA RDI, [R8 + HEADER_SIZE]  (49 8D 78 28)
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            self.buf.emit(&[0x49, 0x8D, 0x78, HEADER_SIZE as u8]);
                            if callee_entry == crate::JitIntrinsic::ArraysFill1.as_entry() {
                                // REP STOSB  (F3 AA)
                                self.buf.emit(&[0xF3, 0xAA]);
                            } else if callee_entry == crate::JitIntrinsic::ArraysFill2.as_entry() {
                                // REP STOSW  (66 F3 AB)
                                self.buf.emit(&[0x66, 0xF3, 0xAB]);
                            } else if callee_entry == crate::JitIntrinsic::ArraysFill4.as_entry() {
                                // REP STOSD  (F3 AB)
                                self.buf.emit(&[0xF3, 0xAB]);
                            } else {
                                // REP STOSQ  (F3 48 AB)
                                self.buf.emit(&[0xF3, 0x48, 0xAB]);
                            }
                            // POP RDI  (5F)
                            self.buf.emit_byte(0x5F);
                            // void return — nothing pushed onto the operand
                            // stack.
                        } else if callee_entry == crate::JitIntrinsic::ArraysEquals1.as_entry()
                            || callee_entry == crate::JitIntrinsic::ArraysEquals2.as_entry()
                            || callee_entry == crate::JitIntrinsic::ArraysEquals4.as_entry()
                            || callee_entry == crate::JitIntrinsic::ArraysEquals8.as_entry()
                        {
                            // Arrays.equals(a, b) : boolean
                            //
                            // JDK semantics (java.util.Arrays):
                            //   a == b               -> true   (both null, or same ref)
                            //   a == null || b==null -> false
                            //   a.length != b.length -> false
                            //   else element-wise equality.
                            // `equals` never throws — it is a pure compare,
                            // so no null-check stub is needed.
                            //
                            // The element-wise compare is a raw byte compare
                            // of `length * elem_size` bytes via REP CMPSB.
                            // boolean[] stores 0/1 per byte, so a byte
                            // compare is exact for `equals([Z[Z)Z`.
                            let b_slot = self.pop_stack();
                            let a_slot = self.pop_stack();
                            // a → R8, b → R9.
                            self.load_slot_to_reg(R8, a_slot);
                            self.load_slot_to_reg(R9, b_slot);

                            let shift: u8 = if callee_entry
                                == crate::JitIntrinsic::ArraysEquals1.as_entry()
                            {
                                0
                            } else if callee_entry == crate::JitIntrinsic::ArraysEquals2.as_entry()
                            {
                                1
                            } else if callee_entry == crate::JitIntrinsic::ArraysEquals4.as_entry()
                            {
                                2
                            } else {
                                3
                            };

                            // CMP R8, R9  (4D 39 C8) — same reference?
                            self.buf.emit(&[0x4D, 0x39, 0xC8]);
                            // JE -> true  (74 rel8) — covers both-null and
                            // identical-reference.
                            self.buf.emit_byte(0x74);
                            let je_true_1 = self.buf.pos();
                            self.buf.emit_byte(0x00);

                            // TEST R8, R8  (4D 85 C0) — a null (b not)?
                            self.buf.emit(&[0x4D, 0x85, 0xC0]);
                            // JZ -> false  (74 rel8)
                            self.buf.emit_byte(0x74);
                            let jz_false_1 = self.buf.pos();
                            self.buf.emit_byte(0x00);

                            // TEST R9, R9  (4D 85 C9) — b null (a not)?
                            self.buf.emit(&[0x4D, 0x85, 0xC9]);
                            // JZ -> false  (74 rel8)
                            self.buf.emit_byte(0x74);
                            let jz_false_2 = self.buf.pos();
                            self.buf.emit_byte(0x00);

                            // Lengths: EAX = a.length, EDX = b.length.
                            // MOV EAX, [R8+12]  (41 8B 40 0C)
                            self.buf
                                // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                .emit(&[0x41, 0x8B, 0x40, ARRAY_LENGTH_OFFSET as u8]);
                            // MOV EDX, [R9+12]  (41 8B 51 0C)
                            self.buf
                                // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                .emit(&[0x41, 0x8B, 0x51, ARRAY_LENGTH_OFFSET as u8]);
                            // CMP EAX, EDX  (39 D0)
                            self.buf.emit(&[0x39, 0xD0]);
                            // JNE -> false  (75 rel8)
                            self.buf.emit_byte(0x75);
                            let jne_false_1 = self.buf.pos();
                            self.buf.emit_byte(0x00);

                            // Byte count = length << shift  → RCX (REP
                            // counter). MOVZX is unnecessary: array lengths
                            // are non-negative i32, so a 32-bit MOV
                            // zero-extends cleanly into RCX.
                            // MOV ECX, EAX  (89 C1)
                            self.buf.emit(&[0x89, 0xC1]);
                            if shift != 0 {
                                // SHL RCX, shift  (48 C1 E1 ib)
                                self.buf.emit(&[0x48, 0xC1, 0xE1, shift]);
                            }
                            // Empty arrays (count == 0): equal. Also avoids
                            // running REP CMPSB with RCX==0, whose ZF would
                            // otherwise carry over from the SHL above.
                            // TEST RCX, RCX  (48 85 C9)
                            self.buf.emit(&[0x48, 0x85, 0xC9]);
                            // JZ -> true  (74 rel8)
                            self.buf.emit_byte(0x74);
                            let jz_true_2 = self.buf.pos();
                            self.buf.emit_byte(0x00);

                            // Save RSI/RDI (Java locals may live there),
                            // point them at the two element-data regions,
                            // run REP CMPSB, restore. POP does not touch
                            // flags, so ZF from CMPSB survives the restore.
                            // PUSH RSI (56) ; PUSH RDI (57)
                            self.buf.emit(&[0x56, 0x57]);
                            // LEA RSI, [R8 + HEADER_SIZE]  (49 8D 70 28)
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            self.buf.emit(&[0x49, 0x8D, 0x70, HEADER_SIZE as u8]);
                            // LEA RDI, [R9 + HEADER_SIZE]  (49 8D 79 28)
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            self.buf.emit(&[0x49, 0x8D, 0x79, HEADER_SIZE as u8]);
                            // REP CMPSB  (F3 A6) — compares RCX bytes;
                            // stops early on the first mismatch. ZF=1 iff
                            // every byte matched.
                            self.buf.emit(&[0xF3, 0xA6]);
                            // POP RDI (5F) ; POP RSI (5E)
                            self.buf.emit(&[0x5F, 0x5E]);
                            // SETE AL  (0F 94 C0) — AL = ZF.
                            self.buf.emit(&[0x0F, 0x94, 0xC0]);
                            // MOVZX EAX, AL  (0F B6 C0) — boolean result.
                            self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                            // JMP -> done  (EB rel8)
                            self.buf.emit_byte(0xEB);
                            let jmp_done_1 = self.buf.pos();
                            self.buf.emit_byte(0x00);

                            // --- true label ---
                            let true_label = self.buf.pos();
                            // MOV EAX, 1  (B8 01 00 00 00)
                            self.buf.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
                            // JMP -> done  (EB rel8)
                            self.buf.emit_byte(0xEB);
                            let jmp_done_2 = self.buf.pos();
                            self.buf.emit_byte(0x00);

                            // --- false label ---
                            let false_label = self.buf.pos();
                            // XOR EAX, EAX  (31 C0)
                            self.buf.emit(&[0x31, 0xC0]);

                            // --- done label ---
                            let done_label = self.buf.pos();

                            // Patch all rel8 displacements. Every span here
                            // is a few dozen bytes — comfortably inside the
                            // signed-rel8 range; debug_assert guards it.
                            for (patch, target) in [
                                (je_true_1, true_label),
                                (jz_true_2, true_label),
                                (jz_false_1, false_label),
                                (jz_false_2, false_label),
                                (jne_false_1, false_label),
                                (jmp_done_1, done_label),
                                (jmp_done_2, done_label),
                            ] {
                                // Cast: signed offset to isize for pointer/index arithmetic
                                let rel = target as isize - (patch as isize + 1);
                                debug_assert!(
                                    (-128..=127).contains(&rel),
                                    "Arrays.equals intrinsic rel8 out of range: {rel}"
                                );
                                // Widening: isize -> i64 (no truncation)
                                Self::patch_rel8_or_bail(&mut self.buf, patch, rel as i64);
                            }

                            // boolean result in RAX → operand stack.
                            self.push_from_rax();
                        }
                        // ===== INTRINSIC REGION END: ARRAYS_OPS =====

                        // ===== INTRINSIC REGION BEGIN: ARRAYS_SORT =====
                        else if callee_entry == crate::JitIntrinsic::ArraysSortInt.as_entry()
                            || callee_entry == crate::JitIntrinsic::ArraysSortLong.as_entry()
                            || callee_entry == crate::JitIntrinsic::ArraysSortChar.as_entry()
                            || callee_entry == crate::JitIntrinsic::ArraysSortShort.as_entry()
                            || callee_entry == crate::JitIntrinsic::ArraysSortByte.as_entry()
                        {
                            // Phase 4b — java.util.Arrays.sort(prim[]) inline
                            // insertion sort. Void return: nothing is pushed.
                            //
                            // The matcher (try_resolve_intrinsic ARRAYS_SORT
                            // region) registers only the five integral
                            // single-arg overloads. Insertion sort is O(n^2)
                            // but provably correct for EVERY length — empty,
                            // single, sorted, reverse, duplicates, negatives.
                            // There is deliberately no runtime "bail to native
                            // for large arrays": once the call site resolves
                            // to this intrinsic there is no native call left
                            // to fall through to, so bailing would silently
                            // leave a long array unsorted. Correctness wins
                            // over the constant factor (roadmap §3.4).
                            //
                            // All work uses caller-saved scratch only
                            // (RAX/RCX/RDX/R8/R9/R10/R11) — `flush_scratch_
                            // registers()` already spilled them and Java
                            // locals live in callee-saved registers, so the
                            // loop never clobbers live state.
                            //
                            // Register file for the emitted routine:
                            //   R8  = array base pointer
                            //   R9  = n (element count)
                            //   R10 = i (outer index)
                            //   R11 = j (inner index)
                            //   RAX = key (= a[i])
                            //   RCX = a[j] scratch
                            //   RDX = j+1 (store index)
                            //
                            // Every element is loaded sign-/zero-extended to
                            // a full 64-bit register, so a single signed
                            // 64-bit CMP orders all five element kinds
                            // correctly (char is unsigned 0..=65535, which is
                            // non-negative, so signed compare still works).

                            // SIB scale bits + load/store encodings per width.
                            let is_int =
                                callee_entry == crate::JitIntrinsic::ArraysSortInt.as_entry();
                            let is_long =
                                callee_entry == crate::JitIntrinsic::ArraysSortLong.as_entry();
                            let is_char =
                                callee_entry == crate::JitIntrinsic::ArraysSortChar.as_entry();
                            let is_short =
                                callee_entry == crate::JitIntrinsic::ArraysSortShort.as_entry();
                            // is_byte is the remaining case.
                            let scale_ss: u8 = if is_long {
                                0b11 // *8
                            } else if is_int {
                                0b10 // *4
                            } else if is_char || is_short {
                                0b01 // *2
                            } else {
                                0b00 // *1 (byte)
                            };
                            // HEADER_SIZE / ARRAY_LENGTH_OFFSET both fit in a
                            // signed disp8 (asserted in cratonvm_types).
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            let hdr = HEADER_SIZE as u8;
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            let len_off = ARRAY_LENGTH_OFFSET as u8;

                            // Pop the array reference, null-check it (reusing
                            // the shared NPE stub), then park it in R8.
                            let arr_slot = self.pop_stack();
                            self.load_slot_to_reg(RAX, arr_slot);
                            // TEST RAX,RAX / JZ -> shared null-check stub.
                            // Intrinsic array access — no precise array opcode,
                            // so record NONE (unmessaged NPE).
                            self.emit_null_check_array_load(npe_action::NONE);
                            // MOV R8, RAX  (49 89 C0)
                            self.buf.emit(&[0x49, 0x89, 0xC0]);
                            // MOV R9D, [R8 + ARRAY_LENGTH_OFFSET]  (45 8B 48 dd)
                            // (32-bit load zero-extends n into R9.)
                            self.buf.emit(&[0x45, 0x8B, 0x48, len_off]);
                            // MOV R10D, 1   (41 BA 01 00 00 00) — i = 1
                            self.buf.emit(&[0x41, 0xBA, 0x01, 0x00, 0x00, 0x00]);

                            // --- inline emitters for width-specific access ---
                            // Access form: [R8 + idx*scale + hdr] with a
                            // ModRM.reg operand `r` (the GPR load dest /
                            // store source). REX bits MUST be derived per
                            // register: REX.R from `r`, REX.X from `idx`,
                            // REX.B is always 1 (base is R8). REX.W comes
                            // from the caller (`w`). A prior version hard-
                            // coded REX.X=1, which silently re-routed a
                            // store through a non-extended index register
                            // (RDX -> R10) and left the array unsorted.
                            let rex = |w: u8, r: u8, idx: u8| -> u8 {
                                0x40 | (w << 3)
                                    // Truncation: wider int -> u8 (low 8 bits, intentional)
                                    | (((r >= 8) as u8) << 2)
                                    // Truncation: wider int -> u8 (low 8 bits, intentional)
                                    | (((idx >= 8) as u8) << 1)
                                    | 1 // REX.B: base = R8
                            };
                            // load r <- [R8 + idx*scale + hdr]
                            let emit_load = |buf: &mut crate::ExecutableBuffer, r: u8, idx: u8| {
                                let sib = (scale_ss << 6) | ((idx & 7) << 3);
                                let modrm = 0x40 | ((r & 7) << 3) | 0x04;
                                if is_long {
                                    // MOV r64,[..]  REX.W, 8B
                                    buf.emit(&[rex(1, r, idx), 0x8B, modrm, sib, hdr]);
                                } else if is_int {
                                    // MOVSXD r64,[..]  REX.W, 63
                                    buf.emit(&[rex(1, r, idx), 0x63, modrm, sib, hdr]);
                                } else if is_char {
                                    // MOVZX r32,m16  0F B7. char is
                                    // unsigned 0..=65535; zero-extending
                                    // to 32 bits also clears RAX[63:32],
                                    // so the 64-bit signed CMP is correct.
                                    buf.emit(&[rex(0, r, idx), 0x0F, 0xB7, modrm, sib, hdr]);
                                } else if is_short {
                                    // MOVSX r64,m16  REX.W 0F BF — MUST
                                    // sign-extend to the FULL 64-bit
                                    // register: the inner-loop CMP is
                                    // 64-bit, and a 32-bit MOVSX would
                                    // leave a negative short looking like
                                    // a large positive (0x0000_0000_FFFF…).
                                    buf.emit(&[rex(1, r, idx), 0x0F, 0xBF, modrm, sib, hdr]);
                                } else {
                                    // MOVSX r64,m8  REX.W 0F BE — sign-
                                    // extend a signed byte to 64 bits
                                    // (same rationale as short above).
                                    buf.emit(&[rex(1, r, idx), 0x0F, 0xBE, modrm, sib, hdr]);
                                }
                            };
                            // store [R8 + idx*scale + hdr] <- r
                            let emit_store = |buf: &mut crate::ExecutableBuffer, r: u8, idx: u8| {
                                let sib = (scale_ss << 6) | ((idx & 7) << 3);
                                let modrm = 0x40 | ((r & 7) << 3) | 0x04;
                                if is_long {
                                    // MOV [..],r64  REX.W, 89
                                    buf.emit(&[rex(1, r, idx), 0x89, modrm, sib, hdr]);
                                } else if is_int {
                                    // MOV [..],r32  89
                                    buf.emit(&[rex(0, r, idx), 0x89, modrm, sib, hdr]);
                                } else if is_char || is_short {
                                    // MOV [..],r16  66 prefix, 89
                                    buf.emit(&[0x66, rex(0, r, idx), 0x89, modrm, sib, hdr]);
                                } else {
                                    // MOV [..],r8   88
                                    buf.emit(&[rex(0, r, idx), 0x88, modrm, sib, hdr]);
                                }
                            };

                            // .outer:
                            let outer_label = self.buf.pos();
                            // CMP R10, R9   (4D 39 CA) — i vs n
                            self.buf.emit(&[0x4D, 0x39, 0xCA]);
                            // JGE .done  (0F 8D rel32) — signed: i >= n
                            let done_patch = self.emit_jcc_rel32_patch(0x8D);
                            // key = a[i]
                            emit_load(&mut self.buf, RAX, R10);
                            // MOV R11, R10  (4D 89 D3) — j = i
                            self.buf.emit(&[0x4D, 0x89, 0xD3]);
                            // DEC R11       (49 FF CB) — j = i - 1
                            self.buf.emit(&[0x49, 0xFF, 0xCB]);

                            // .inner:
                            let inner_label = self.buf.pos();
                            // TEST R11,R11  (4D 85 DB)
                            self.buf.emit(&[0x4D, 0x85, 0xDB]);
                            // JS .insert    (0F 88 rel32) — j < 0 -> stop
                            let insert_patch = self.emit_jcc_rel32_patch(0x88);
                            // RCX = a[j]
                            emit_load(&mut self.buf, RCX, R11);
                            // CMP RCX, RAX  (48 39 C1) — a[j] vs key, signed64
                            self.buf.emit(&[0x48, 0x39, 0xC1]);
                            // JLE .insert   (0F 8E rel32) — a[j] <= key -> stop
                            //   (stable: equal keys are never shifted past.)
                            let insert_patch2 = self.emit_jcc_rel32_patch(0x8E);
                            // a[j+1] = a[j]
                            // LEA RDX, [R11 + 1]  (49 8D 53 01)
                            self.buf.emit(&[0x49, 0x8D, 0x53, 0x01]);
                            emit_store(&mut self.buf, RCX, RDX);
                            // DEC R11  (49 FF CB)  — j--
                            self.buf.emit(&[0x49, 0xFF, 0xCB]);
                            // JMP .inner  (E9 rel32) — backward branch.
                            self.buf.emit_byte(0xE9);
                            {
                                let here = self.buf.pos();
                                // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                                let rel = (inner_label as i64) - (here as i64 + 4);
                                // Truncation: i64 -> i32 (rel32 branch displacement, range-checked)
                                self.buf.emit(&(rel as i32).to_le_bytes());
                            }

                            // .insert: both JS and JLE land here.
                            self.patch_rel32_to_here(insert_patch);
                            self.patch_rel32_to_here(insert_patch2);
                            // a[j+1] = key
                            // LEA RDX, [R11 + 1]  (49 8D 53 01)
                            self.buf.emit(&[0x49, 0x8D, 0x53, 0x01]);
                            emit_store(&mut self.buf, RAX, RDX);
                            // INC R10  (49 FF C2)  — i++
                            self.buf.emit(&[0x49, 0xFF, 0xC2]);
                            // JMP .outer  (E9 rel32) — backward branch.
                            self.buf.emit_byte(0xE9);
                            {
                                let here = self.buf.pos();
                                // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                                let rel = (outer_label as i64) - (here as i64 + 4);
                                // Truncation: i64 -> i32 (rel32 branch displacement, range-checked)
                                self.buf.emit(&(rel as i32).to_le_bytes());
                            }

                            // .done: the top-of-loop JGE lands here.
                            self.patch_rel32_to_here(done_patch);
                            // Void method — nothing pushed; `ret_type` is 'V'.
                            let _ = ret_type;
                        }
                        // ===== INTRINSIC REGION END: ARRAYS_SORT =====
                        else {
                            // value-stack-usize-underflow-nio-worker-panic fix:
                            // snapshot the pre-pop operand stack (see the
                            // matching invokevirtual/interface fix below) so a
                            // post-invoke exception/deopt guard's `Reinterpret`
                            // resume at this bci has the args this invokestatic
                            // needs, instead of underflowing on an empty stack.
                            if crate::deopt_real_enabled() {
                                self.snapshot_pre_intrinsic_call(
                                    pc,
                                    crate::deopt::DeoptReason::ReceiverTypeChanged,
                                );
                            }
                            // Direct call to a JIT-compiled callee
                            let n = callee_params;
                            // `pop_stack` rewinds `next_spill_offset` when it pops a
                            // top-of-stack `Frame` slot, but it still HANDS THE SLOT
                            // BACK, and every `arg_slots` entry stays live until
                            // `emit_stack_arg_setup` marshals it into the entry ABI far
                            // below. Anything that reserves spill space in between is
                            // therefore handed the argument slots themselves. Remember
                            // the pre-pop top so such a reservation can be placed above
                            // them. See
                            // fixed-suite-bugs/jit-direct-call-arg1-clobbered-by-arg0-FIXED.md.
                            let args_frame_top = self.next_spill_offset;
                            let (arg_slots, arg_oops) = self.pop_invoke_args(n);
                            // A reference staged where no oop map can name it fails the
                            // safepoint closed. DEFERRED to just after the service-range
                            // reservation below, because whether that is true here is
                            // exactly what the reservation decides: when it succeeds it
                            // copies every argument into a contiguous frame range, and a
                            // frame range IS nameable. See `direct_call_arg_maps_enabled`.
                            // Spill cursor as the bytecode's operand stack sees it
                            // now that this invoke's arguments are popped. The
                            // return value belongs HERE, not wherever the
                            // service-argument reservation below leaves the cursor.
                            let post_pop_spill = self.next_spill_offset;

                            // T5.2.16 — Sibling tail-call optimization.
                            //
                            // When the caller's immediate next bytecode
                            // is an `xreturn` of the same type the
                            // callee produces, we can tear down our
                            // frame and `JMP` into the callee so the
                            // callee returns straight to OUR caller.
                            // Gate on:
                            //   1. pc+3 is an xreturn whose type tag
                            //      matches ret_type, or the callee is
                            //      void AND pc+3 is `return` (0xB1).
                            //   2. callee_needs_ctx == self.needs_heap
                            //      (we have a VM context iff the
                            //      callee wants one) — otherwise the
                            //      ABI shift wouldn't match.
                            //   3. RET intrinsics (MATH_*_INTRINSIC)
                            //      are NOT targeted (already branched
                            //      above), so the callee is a normal
                            //      JIT-compiled method.
                            //   4. `pc + 3` is NOT a branch target. The
                            //      tail form CONSUMES the `xreturn` — it emits
                            //      no code for that PC and leaves
                            //      `pc_to_native[pc + 3]` unset — so any other
                            //      edge into it becomes unresolvable and
                            //      `patch_branches` rejects the whole method
                            //      with `branch-target-not-an-instruction-
                            //      boundary`, a reason whose message blames
                            //      malformed bytecode. It is the ordinary
                            //      shape `return (x != null ? x : missing())`:
                            //      the `else` arm's call sits immediately
                            //      before the shared `areturn`, and the `then`
                            //      arm's `goto` lands on it. Fusing would also
                            //      be wrong on its own terms — the other edge
                            //      arrives with its own value on the operand
                            //      stack and expects a plain return, not "load
                            //      args and JMP to the callee". This is the
                            //      same precondition the const-arith peepholes
                            //      state: never fuse across a merge point.
                            let tail_op_matches = pc + 3 < code_len
                                && !branch_targets[pc + 3]
                                && match (ret_type, code[pc + 3]) {
                                    (b'I' | b'Z' | b'B' | b'S' | b'C', 0xAC) => true,
                                    (b'J', 0xAD) => true,
                                    (b'F', 0xAE) => true,
                                    (b'D', 0xAF) => true,
                                    (b'L' | b'[', 0xB0) => true,
                                    (b'V', 0xB1) => true,
                                    _ => false,
                                };
                            // A tail-call inside a try region would tear this
                            // frame down before the callee runs, so anything it
                            // throws escapes the handler that covers this pc
                            // (see `pc_is_protected`). Demote to a normal CALL.
                            let is_sibling_tail = tail_op_matches
                                && callee_needs_ctx == self.needs_heap
                                && !self.pc_is_protected(pc);

                            // Round-8 wave-3: sibling-tail demotion.
                            // Tail-calling with stack args is non-trivial
                            // — args would have to be re-materialized
                            // *after* the epilogue restores RSP, which
                            // requires an additional shuffle buffer.
                            // Simpler and still correct: demote to a
                            // non-tail CALL when the arg count would
                            // require stack passing. The fall-through
                            // below handles that case with the proper
                            // stack-arg setup helper.
                            let sibling_reg_limit = if callee_needs_ctx {
                                ARG_REGS.len() - 1
                            } else {
                                ARG_REGS.len()
                            };
                            // 5. The callee cannot stash a deopt frame.
                            //
                            // A sibling tail call REPLACES this frame, so the
                            // callee returns straight to OUR caller — and if it
                            // traps, the `i64::MIN` sentinel and the frame it
                            // stashed under the CALLEE's key arrive at a call
                            // site that invoked US. That site's identity gate
                            // (`try_resume_trapped_callee`) correctly refuses a
                            // stash naming a method it did not call, and the
                            // frame becomes an orphan nobody can attribute.
                            // A real CALL keeps this frame alive long enough
                            // for `emit_inline_callee_deopt_check` below to
                            // service the trap at the site that made it.
                            //
                            // `info_ptr.is_some()` IS the "can stash" test:
                            // a `JitInvokeInfo` is registered for exactly the
                            // sites whose callee is a compiled Java artifact
                            // (plus `ArraycopyPrimitive`, the one intrinsic
                            // that deopts). Inline-machine-code intrinsics and
                            // the thin native helpers have no info and no way
                            // to stash, so they keep the tail form.
                            let sibling_tail_ok = is_sibling_tail
                                && arg_slots.len() <= sibling_reg_limit
                                && info_ptr.is_none()
                                && sp_tailcall_enabled();
                            if sibling_tail_ok {
                                // Load args into ABI registers, tear
                                // down our frame, then JMP.
                                if callee_needs_ctx {
                                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                    for (i, slot) in arg_slots.iter().enumerate() {
                                        if i + 1 < ARG_REGS.len() {
                                            self.load_slot_to_reg(ARG_REGS[i + 1], *slot);
                                        }
                                    }
                                } else {
                                    for (i, slot) in arg_slots.iter().enumerate() {
                                        if i < ARG_REGS.len() {
                                            self.load_slot_to_reg(ARG_REGS[i], *slot);
                                        }
                                    }
                                }
                                self.emit_epilogue_without_ret();
                                self.emit_jmp_absolute(callee_entry);
                                // Consume the invokestatic (3) and the
                                // xreturn (1) — no fall-through.
                                pc += 3; // invokestatic
                                pc += 1; // xreturn
                                self.reset_spills();
                                continue;
                            }

                            // Preserve Java arguments in a contiguous frame range for
                            // the cold callee-sentinel service. A baked direct call has no
                            // dispatch helper frame to recover them from when its own
                            // exception table must run.
                            let service_args_base = info_ptr.and_then(|_| {
                                // See `reserve_direct_call_service_slots`: this range MUST
                                // sit above the argument slots `pop_stack` just handed
                                // back, or the copy below reverses the arguments into
                                // themselves and the callee gets arg0 in every slot.
                                let base = self.reserve_direct_call_service_slots(
                                    args_frame_top,
                                    &arg_slots,
                                )?;
                                for (i, slot) in arg_slots.iter().enumerate() {
                                    self.load_slot_to_reg(R11, *slot);
                                    let off = base + ((arg_slots.len() - 1 - i) as i32) * 8;
                                    self.emit_store_local(off, R11);
                                    // THE SAME CHANNEL THE DISPATCH SITE USES. Its args
                                    // buffer pushes each oop among the arguments to
                                    // `pending_staged_arg_oops`, so the safepoint map
                                    // NAMES it and `collect_live_oop_homes` publishes it
                                    // on the shadow stack. This copy is the same shape --
                                    // a contiguous frame range, written before the CALL,
                                    // still live after it (`emit_inline_callee_deopt_check`
                                    // reads it) -- and it named nothing.
                                    if direct_call_arg_maps_enabled() && arg_oops[i] {
                                        self.pending_staged_arg_oops.push(off);
                                    }
                                }
                                Some(base)
                            });
                            // Only an argument oop with NO named home fails the
                            // safepoint closed now. With the service range reserved
                            // every one of them has one.
                            if arg_oops.iter().any(|&o| o)
                                && (!direct_call_arg_maps_enabled() || service_args_base.is_none())
                            {
                                self.pending_staged_args_unmapped = true;
                            }
                            // Round-8 wave-3 HIGH fix: stack-arg setup
                            // for direct calls whose total arg count
                            // exceeds ARG_REGS. Uses platform ABI
                            // (Win64 32-byte shadow + stack; SysV pure
                            // stack), 16-byte aligned at the CALL site.
                            let total_sub = self.emit_stack_arg_setup(&arg_slots, callee_needs_ctx);
                            // Round-8 wave-3: defensive callee-saved spill
                            // before any GC-triggering CALL -- unless the frame
                            // is provably oop-clean here, in which case the
                            // 14-store blind copy publishes nothing and only the
                            // safepoint id is needed. See
                            // `can_elide_direct_call_register_spill`.
                            // `args_frame_resident` is what lets mode 2 admit a
                            // reference argument: the service range makes it
                            // frame-resident for the CONSERVATIVE walk. That is no
                            // longer the whole obligation -- naming the argument in
                            // the map means a moving cycle will rewrite it, and for
                            // that it must also be PUBLISHED, which only the real
                            // spill path emits. So a call that names its argument
                            // oops declines the elision and pays the spill again;
                            // `CRATONVM_JIT_DIRECT_CALL_ARG_MAPS=0` restores the
                            // cheaper, unrelocatable arrangement.
                            let names_arg_oops = direct_call_arg_maps_enabled()
                                && service_args_base.is_some()
                                && arg_oops.iter().any(|&o| o);
                            if self.can_elide_direct_call_register_spill(
                                &arg_oops,
                                service_args_base.is_some() && !names_arg_oops,
                                1,
                            ) {
                                self.emit_safepoint_metadata_only();
                            } else {
                                // ARG_REGS only, and only if the service slots
                                // were actually reserved: the copy above stages
                                // through R11, so RAX is untouched here and its
                                // contents are unpublished.
                                self.emit_pre_safepoint_spill_args_published(
                                    service_args_base.is_some(),
                                    false,
                                );
                            }
                            // Emit direct CALL to callee entry point
                            self.emit_call_absolute(callee_entry);
                            self.emit_post_call_rbp_republish();
                            // T1.1.2 — direct call to a JIT-compiled
                            // callee is still a safepoint: the callee
                            // may allocate and trigger GC transitively.
                            self.emit_oop_map_for_safepoint();
                            self.emit_stack_arg_cleanup(total_sub);
                            if let (Some(info), Some(args_base)) = (info_ptr, service_args_base) {
                                self.emit_inline_callee_deopt_check(
                                    info as *const crate::JitInvokeInfo,
                                    arg_slots.len(),
                                    args_base,
                                );
                            } else {
                                self.dbg_unserviced_direct_call(
                                    "invokestatic",
                                    pc,
                                    info_ptr.is_some(),
                                    service_args_base.is_some(),
                                );
                                self.fail_unserviced_java_direct_call(info_ptr, service_args_base);
                            }

                            // A directly-called compiled callee that throws
                            // (or deopts) returns the `i64::MIN` sentinel.
                            // Without this guard the JIT would push the
                            // sentinel as the return value — for an L/[
                            // return it would then be tagged as an oop
                            // (`mark_top_as_oop` below) and the next deref
                            // of that `0x8000_0000_0000_0000` wild pointer
                            // segfaults. Deopt out so the interpreter routes
                            // the stashed exception through the exception
                            // table instead.
                            self.emit_post_invoke_exception_check(ret_type);

                            // Reclaim the spill cursor to the popped-args depth
                            // before the result is pushed, exactly as the
                            // dispatch-helper arm below does with its own
                            // `post_pop_spill`.
                            //
                            // `reserve_direct_call_service_slots` parks the cold
                            // deopt-service copy of the arguments ABOVE the argument
                            // slots (it has to: the slots `pop_stack` handed back are
                            // still live sources for `emit_stack_arg_setup`), which
                            // leaves `next_spill_offset` n slots past the pre-pop top.
                            // Pushing the return value from there parks it above its
                            // semantic operand-stack depth, and every later push in
                            // this basic block inherits the shift. The linear walk
                            // stays self-consistent, so nothing looks wrong -- until
                            // the first branch target after the call, whose depth is
                            // re-established from the bytecode. Writer and reader then
                            // address different slots and the method computes with a
                            // stale one. Measured on ECJ's
                            // `OperandStack.pop(OperandCategory)`, whose `if_icmpeq`
                            // (a tableswitch merge point) compared `TypeBinding.id`
                            // against the expected category instead of
                            // `TypeIds.getCategory(id)`: every JSP compiled after that
                            // method tiered up threw `AssertionError: Unexpected
                            // operand at stack top` (tomcat/ecj-operandstack-*.md).
                            //
                            // Safe to hand the reserved range back: its only consumer
                            // is `emit_inline_callee_deopt_check`, emitted just above.
                            self.next_spill_offset = post_pop_spill;

                            if ret_type != b'V' {
                                if matches!(ret_type, b'D' | b'F') {
                                    self.push_from_rax_as_xmm0();
                                } else {
                                    self.push_from_rax();
                                }
                                if matches!(ret_type, b'L' | b'[') {
                                    self.mark_top_as_oop();
                                }
                            }
                        }
                    } else if let Some(info) = info_ptr {
                        // Non-self invokestatic without a direct target — use dispatch helper
                        // SAFETY: info comes from self.invoke_info, which holds pointers to
                        // JitInvokeInfo structs kept alive by the caller for the duration of compilation.
                        let info_ref = unsafe { &*info };
                        let n = info_ref.num_jit_args;

                        // value-stack-usize-underflow-nio-worker-panic fix:
                        // snapshot the pre-pop operand stack (see the matching
                        // invokevirtual/interface fix below) so a post-invoke
                        // exception/deopt guard's `Reinterpret` resume at this
                        // bci has the args this invokestatic needs, instead of
                        // underflowing on an empty stack.
                        if crate::deopt_real_enabled() {
                            self.snapshot_pre_intrinsic_call(
                                pc,
                                crate::deopt::DeoptReason::ReceiverTypeChanged,
                            );
                        }

                        // Capture spill offset BEFORE popping to prevent
                        // the args buffer from overlapping source Frame slots.
                        let pre_pop_spill = self.next_spill_offset;
                        let (arg_slots, arg_oops) = self.pop_invoke_args(n);
                        // Cursor at the popped-args depth — the reclaim after
                        // the call restores THIS level (not `pre_pop_spill`).
                        // Restoring to pre_pop left the return value parked
                        // n slots above its semantic depth, permanently
                        // inflating the cursor by n per non-void dispatch
                        // site; across a long method the creep walked the
                        // args buffer past `sub rsp, frame_size` into the
                        // callee's stack (Bug 4: testAdHocData's FFT receiver
                        // zeroed by the next helper call's frame).
                        let post_pop_spill = self.next_spill_offset;

                        let args_base_offset = pre_pop_spill;
                        if n > 0 {
                            let Some(args_end) = self.checked_spill_range_end(args_base_offset, n)
                            else {
                                return false;
                            };
                            self.next_spill_offset = args_end;
                            // Store args in reverse offset order so they form
                            // a contiguous ascending-address buffer:
                            //   arg[0] at [rbp - highest_offset] (lowest addr)
                            //   arg[n-1] at [rbp - args_base_offset] (highest addr)
                            // This is necessary because modrm_rbp_disp negates
                            // the offset, so higher offsets map to lower addresses.
                            for (i, slot) in arg_slots.iter().enumerate() {
                                let buf_offset = args_base_offset + ((n - 1 - i) as i32) * 8; // Cast: x86-64 immediate encoding
                                self.load_slot_to_reg(RAX, *slot);
                                self.emit_store_local(buf_offset, RAX);
                                // This argument leaves the simulated operand
                                // stack here; if it is a reference, the
                                // safepoint map below is the only thing that
                                // can still name it.
                                if arg_oops[i] {
                                    self.pending_staged_arg_oops.push(buf_offset);
                                }
                            }
                        }
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.emit_mov_imm64(ARG_REGS[1], info as *const _ as i64); // Cast: function pointer for JIT call target
                        if n > 0 {
                            // LEA to the highest offset = lowest address = start of buffer
                            let buf_start = args_base_offset + ((n as i32) - 1) * 8; // Cast: x86-64 immediate encoding
                            self.emit_lea_frame_slot(ARG_REGS[2], buf_start);
                        } else {
                            self.emit_xor_reg_self(ARG_REGS[2]);
                        }
                        self.emit_mov_imm32_sx(ARG_REGS[3], n as i32); // Cast: x86-64 immediate encoding
                                                                       // Round-8 wave-3: defensive callee-saved spill before any
                                                                       // GC-triggering CALL -- unless the caller frame is
                                                                       // provably oop-clean here. Every argument of this site,
                                                                       // reference or not, was just stored into the helper's
                                                                       // args buffer at `args_base_offset`, and each oop among
                                                                       // them was pushed to `pending_staged_arg_oops` so the map
                                                                       // below NAMES it: `args_frame_resident` is
                                                                       // unconditionally true here, which is a stronger
                                                                       // guarantee than the direct sites' service slots.
                        if self.can_elide_direct_call_register_spill(&arg_oops, true, 2) {
                            self.emit_safepoint_metadata_only();
                        } else {
                            // Every argument is in the helper's args buffer and
                            // the oops among them are named in the map below;
                            // ARG_REGS carry the helper ABI (heap/info/buf/count),
                            // never a Java oop. RAX is published only when the
                            // staging loop -- which writes through RAX -- ran.
                            self.emit_pre_safepoint_spill_args_published(true, n > 0);
                        }
                        self.emit_call_absolute(self.helpers.invoke_dispatch);
                        // T1.1.2 — invoke dispatch is a full safepoint:
                        // the callee may allocate, trigger GC, or throw.
                        // Record an oop map for the operand stack state
                        // that survives the call (args are already popped,
                        // return value not yet pushed).
                        self.emit_oop_map_for_safepoint();

                        // After the dispatch returns, check whether the
                        // static callee threw a Java exception. `jit_invoke_
                        // dispatch` returns `i64::MIN` (and stashes the
                        // exception in `JIT_PENDING_EXCEPTION`) when the
                        // callee throws. Without this guard — which the
                        // invokevirtual/invokespecial paths already have —
                        // the JIT pushes the `i64::MIN` sentinel as the
                        // return value; for an L/[ static method it is then
                        // tagged as an oop (`mark_top_as_oop` below) and the
                        // next deref of that wild `0x8000_0000_0000_0000`
                        // pointer segfaults (the Tomcat boot regression).
                        // Deopt out so the interpreter routes the stashed
                        // exception through the method's exception table.
                        self.emit_post_invoke_exception_check(info_ref.return_type);

                        // Reclaim spill slots used for invoke args AND the
                        // popped arg values — see `post_pop_spill` above.
                        self.next_spill_offset = post_pop_spill;

                        if info_ref.return_type != b'V' {
                            if matches!(info_ref.return_type, b'D' | b'F') {
                                self.push_from_rax_as_xmm0();
                            } else {
                                self.push_from_rax();
                            }
                            // T1.1.2 — the return value is an object
                            // reference iff the descriptor ends in `L`
                            // or `[`. Tag it so the next safepoint
                            // records it as a live oop.
                            if matches!(info_ref.return_type, b'L' | b'[') {
                                self.mark_top_as_oop();
                            }
                        }
                    } else {
                        // Self-recursive call (no invoke_info, no direct_call)
                        let n = self.num_params;

                        // Check for tail call: invokestatic self at PC, xreturn at PC+3.
                        // Never inside a try region — the tail form tears this
                        // frame down, so a throw from the self-recursive callee
                        // would bypass the handler covering this pc
                        // (see `pc_is_protected`).
                        // Never when the `xreturn` is a branch target: the
                        // tail form consumes that PC without emitting it, so
                        // another edge into it has no native offset to be
                        // patched to (see the sibling-tail arm above for the
                        // full argument).
                        let is_tail_call = pc + 3 < code_len
                            && matches!(code[pc + 3], 0xac..=0xb0) // ireturn..areturn
                            && !branch_targets[pc + 3]
                            && !self.pc_is_protected(pc)
                            // `-self-tailcall` demotes this to the raw
                            // self-recursive CALL below, restoring one native
                            // frame per activation. Off is the HotSpot-faithful
                            // answer; on is the default.
                            && self_tailcall_enabled();

                        // jit-invokedynamic-groovy-regression fix: a method
                        // containing a live invokedynamic site (compiled as an
                        // unconditional reason-8 trap) must NEVER machine-CALL
                        // its own entry: a trap in the INNER recursive
                        // invocation stashes a frame whose method identity
                        // equals this method's, so the dispatch-helper resume
                        // above this frame could not distinguish the inner
                        // invocation's frame from the outer's and would resume
                        // the wrong one (dropping the outer continuation). The
                        // tail-JMP form below is exempt (it reuses the SAME
                        // frame, so the stash genuinely describes the one live
                        // invocation). For the non-tail raw CALL, bail the
                        // whole compile — correctness first; a self-recursive
                        // method that also contains an invokedynamic is rare
                        // enough that staying interpreted is acceptable.
                        if !self.indy_info.is_empty()
                            && !(is_tail_call && self.body_entry_offset > 0)
                        {
                            return false;
                        }

                        // value-stack-usize-underflow-nio-worker-panic fix:
                        // snapshot the pre-pop operand stack for the non-tail
                        // path below (see the matching invokevirtual/interface
                        // fix elsewhere in this match arm) — the tail-call form
                        // JMPs and never reaches `emit_post_invoke_exception_check`,
                        // so this is a no-op for it beyond the idempotent
                        // `deopt_box_ptr_by_bci` insert.
                        if crate::deopt_real_enabled() {
                            self.snapshot_pre_intrinsic_call(
                                pc,
                                crate::deopt::DeoptReason::ReceiverTypeChanged,
                            );
                        }
                        let (arg_slots, arg_oops) = self.pop_invoke_args(n);
                        // A reference staged into an area no oop map can name (the
                        // native-ABI outgoing-argument area, the direct-call service
                        // slots, or an inlined callee's parameter locals). The
                        // conservative scan covers those and the precise map cannot,
                        // so this method must not claim precise coverage here.
                        if arg_oops.iter().any(|&o| o) {
                            self.pending_staged_args_unmapped = true;
                        }

                        if is_tail_call && self.body_entry_offset > 0 {
                            // Tail-call optimization: load args into parameter locals
                            // and JMP back to body entry (skip prologue)
                            let local_assignments = self.local_assignments.clone();
                            for (i, slot) in arg_slots.iter().enumerate() {
                                if i < n {
                                    if let Some(reg) = local_assignments.get(i).copied().flatten() {
                                        self.load_slot_to_reg(reg, *slot);
                                    } else {
                                        self.load_slot_to_reg(RAX, *slot);
                                        self.emit_store_local(self.local_offset(i), RAX);
                                    }
                                }
                            }
                            // JMP rel32 back to body entry
                            self.buf.emit_byte(0xE9); // JMP rel32
                            let jmp_offset = self.buf.pos();
                            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                            // Patch: target = body_entry_offset
                            let rel = self.body_entry_offset as i32 - (jmp_offset as i32 + 4); // Cast: x86-64 rel32 displacement
                            let pos = self.buf.pos();
                            self.buf.try_patch_i32(jmp_offset, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                            let _ = pos;

                            // Skip the following xreturn — we already jumped
                            pc += 3; // invokestatic
                            pc += 1; // xreturn
                            self.reset_spills();
                            continue;
                        }

                        // BUG-1 companion — native-stack headroom guard for the
                        // direct self-recursive CALL below. Historically every
                        // NON-tail self-recursive site was routed through
                        // `jit_invoke_dispatch` purely so the dispatch depth
                        // guard could convert runaway compiled recursion into a
                        // catchable StackOverflowError; that made each recursive
                        // call pay the full dispatch-helper round trip (the
                        // dominant cost of fib/binarytrees-style recursion).
                        // With the dedicated guard helper wired, the site stays
                        // a direct CALL and pays one cheap leaf helper call:
                        //   MOV  ARG0, [RBP - heap_local]   ; vm_ptr
                        //   CALL self_call_stack_guard      ; 0 = ok
                        //   TEST RAX, RAX
                        //   JNZ  merge                      ; RAX = i64::MIN →
                        //                                   ; post-invoke check
                        //                                   ; routes the stashed
                        //                                   ; StackOverflowError
                        // The guard's overflow arm allocates (SOE construction),
                        // so it is bracketed like a call safepoint: defensive
                        // callee-saved spill before, and — under precise/shadow
                        // modes — an oop map (whose shadow RELOAD pairs with the
                        // spill's PUSH) at its return PC. The JNZ target sits
                        // AFTER `emit_stack_arg_cleanup`, so the overflow path
                        // skips arg setup + CALL + cleanup as one balanced unit
                        // (no RSP adjustment happens on that path). `arg_slots`
                        // are call-safe across the guard CALL: the
                        // `flush_scratch_registers()` at the 0xb8 arm entry
                        // moved scratch GPR/XMM stack slots into frame slots.
                        // Requires `needs_heap` (the routing in `try_compile`
                        // sets it for every raw-routed self-call site) — the
                        // vm_ptr frame slot is what the guard is called with.
                        // Unwired helper (tests, historical callers): emits
                        // nothing, byte-identical legacy code.
                        let guard_skip_patch =
                            if self.helpers.self_call_stack_guard != 0 && self.needs_heap {
                                // perf/throughput-20260710 -- INLINE floor fast
                                // path: the prologue cached this thread's
                                // native-stack floor in a frame slot, so the
                                // common case is:
                                //   CMP RSP, [rbp - floor_slot]
                                //   JA  <skip helper guard>   (headroom ok)
                                // OSR-entered frames have the slot initialised
                                // to usize::MAX by the trampoline (`RSP > MAX`
                                // is unsatisfiable), so they always take the
                                // helper. The skipped block is the guard CALL
                                // plus its safepoint spill and shadow
                                // push/reload bracketing (skipped TOGETHER, so
                                // the shadow stack stays balanced); the
                                // recursive CALL below still emits its own
                                // spill + oop map, so GC coverage of the
                                // actual recursion is unchanged. This constant
                                // was the dominant per-level cost of
                                // fib/binarytrees-style recursion.
                                let fast_skip = if self.stack_floor_slot_off != 0 {
                                    self.emit_cmp_r64_rbp_local(RSP, self.stack_floor_slot_off);
                                    Some(self.emit_jcc_rel32_patch(0x87)) // JA
                                } else {
                                    None
                                };
                                if self.gc_inert_selfrec {
                                    self.emit_pre_safepoint_spill_without_shadow();
                                } else {
                                    self.emit_pre_safepoint_spill();
                                }
                                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                self.emit_call_absolute(self.helpers.self_call_stack_guard);
                                if self.precise_maps || self.shadow_enabled {
                                    self.emit_oop_map_for_safepoint();
                                }
                                self.emit_test_r64_r64(RAX);
                                let jne_merge = Some(self.emit_jcc_rel32_patch(0x85)); // JNE merge
                                if let Some(skip) = fast_skip {
                                    self.patch_rel32_to_here(skip);
                                }
                                jne_merge
                            } else {
                                None
                            };
                        // Round-8 wave-3 HIGH fix: stack-arg setup for
                        // self-recursive direct calls past ARG_REGS.
                        let total_sub = self.emit_stack_arg_setup(&arg_slots, self.needs_heap);
                        // Round-8 wave-3: defensive callee-saved spill
                        // before the recursive CALL (which transitively
                        // can allocate and reach a GC safepoint).
                        if self.gc_inert_selfrec {
                            // The callee is this same poll-free,
                            // allocation-free method. The direct edge is not a
                            // safepoint, so publishing roots here would be pure
                            // overhead. The overflow helper above retains its
                            // cold safepoint protocol.
                        } else if self.can_elide_self_call_register_spill() {
                            self.emit_safepoint_metadata_only();
                        } else {
                            self.emit_pre_safepoint_spill();
                        }
                        // Normal self-call via CALL (rel32, patched
                        // post-emission).
                        self.buf.emit_byte(0xE8);
                        let call_patch = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                        self.self_call_patches.push(call_patch);
                        self.emit_post_call_rbp_republish();
                        // Stage A (precise oop maps, B-K fix) — a self-recursive
                        // compiled call IS a GC-capable safepoint (the callee
                        // allocates: this is exactly bintrees18's recursive
                        // `make()`). The neighbour direct/dispatch invoke sites
                        // already pair `emit_pre_safepoint_spill` with an oop map
                        // at the return PC; this site historically spilled but
                        // recorded NO map, so the precise relocation path could
                        // not remap this frame (`remap_one_jit_frame` found no
                        // entry for the active sp-id) — the documented gap #1.
                        // Gated behind `precise_maps`: emits zero bytes and no
                        // metadata on the default path, so it is byte-identical
                        // gate-OFF; gate-ON it records the map AND the paired
                        // post-safepoint register reload (Stage 4 / G5).
                        // Also required under `shadow_enabled`: the shadow-stack
                        // PUSH happened in `emit_pre_safepoint_spill`, so its
                        // paired RELOAD (inside `emit_oop_map_for_safepoint`) must
                        // run here too, else the shadow stack grows unbalanced
                        // through this recursive call → unbounded pinning → OOM.
                        if !self.gc_inert_selfrec && (self.precise_maps || self.shadow_enabled) {
                            self.emit_oop_map_for_safepoint();
                        }
                        self.emit_stack_arg_cleanup(total_sub);
                        // Stack-guard merge point: the overflow JNE lands here
                        // with RAX = i64::MIN, flowing straight into the
                        // sentinel check below (exactly as if the callee threw).
                        if let Some(p) = guard_skip_patch {
                            self.patch_rel32_to_here(p);
                        }
                        // A self-recursive compiled call that throws (or
                        // deopts) returns the `i64::MIN` sentinel — same
                        // hazard as the direct/dispatch invokestatic paths
                        // above. Guard it so the sentinel is never pushed
                        // (and never tagged as an oop) as a return value.
                        //
                        // The callee here IS this method, so its return type is
                        // the method's own — but single-pass does not thread the
                        // method descriptor into the Compiler, so we pass `b'I'`
                        // (the plain `CMP; JE` check, byte-identical to before).
                        // Consequence: a self-recursive `long`/`double` method
                        // that *legitimately* returns `Long.MIN_VALUE` retains
                        // the pre-existing `i64::MIN` collision at THIS site
                        // only. That is a rare corner (cross-method J/D calls —
                        // the real unblock — go through the dispatch/direct sites
                        // above, which ARE disambiguated). Left as a documented
                        // follow-up rather than threading a new descriptor param
                        // through every `x64::compile` caller.
                        self.emit_post_invoke_exception_check(b'I');
                        // VOID self-recursive fix (ES SortingDigestTests -Jit
                        // on): this push was unconditional, so a `void`
                        // self-recursive callee (DualPivotQuicksort.sort) left
                        // a PHANTOM entry on the simulated operand stack after
                        // every non-tail self-call. The extra entry shifts the
                        // canonical spill-slot layout for everything downstream
                        // of the next merge point, so later loads read
                        // neighbouring slots (an array ref read as a double →
                        // heap addresses stored into double[] elements,
                        // deterministic mis-sorts and garbage AIOOBE indices —
                        // no GC involved). The method's own return type IS
                        // available via `method_key` ("Class.name:descriptor"),
                        // so only push a return value when there is one. When
                        // `method_key` is empty (unit-test compiles) keep the
                        // historical push — those callers never compile void
                        // self-recursive methods.
                        let self_ret_ty = self
                            .method_key
                            .rfind(')')
                            .and_then(|i| self.method_key.as_bytes().get(i + 1))
                            .copied();
                        if self_ret_ty != Some(b'V') {
                            self.push_from_rax();
                            // The callee IS this method, so its return type is
                            // the method's own. A reference result MUST be
                            // tagged: `collect_live_oop_homes` publishes only
                            // marked operand entries, so an untagged reference
                            // left on the operand stack across a later
                            // GC-capable call is invisible to the shadow stack
                            // — while `moving_young_safepoint_coverage_complete`
                            // still certifies the frame, because it only checks
                            // that MARKED entries have frame/register homes.
                            //
                            // That combination is the measured heap corruption
                            // in `fixed-suite-bugs/app-jvm-bugs/
                            // moving-young-gen-drops-jit-held-oops-FIXED.md`:
                            // `BinTreesClassic.bottomUpTree` keeps the result of
                            // its first recursive call — an entire subtree — on
                            // the operand stack across its second, and a moving
                            // young cycle neither marked nor rewrote it.
                            match self_ret_ty {
                                Some(b'L') | Some(b'[') => self.mark_top_as_oop(),
                                // No descriptor (the legacy `compile` test
                                // wrapper passes an empty `method_key`): we
                                // cannot tell whether this is a reference, so
                                // the mark vector is no longer exact and this
                                // frame must not certify moving-young coverage.
                                // Fail-closed costs a non-moving cycle; guessing
                                // costs the heap.
                                None => self.stack_oop_marks_exact = false,
                                _ => {}
                            }
                        }
                    }
                    pc += 3;
                }

                // invokevirtual / invokespecial / invokeinterface — direct call or dispatch helper
                0xb6 | 0xb7 | 0xb9 => {
                    // Scalar replacement: skip <init>()V on scalar-replaced objects
                    if op == 0xb7 && self.scalar_init_skips.contains(&pc) {
                        let _ = self.pop_stack(); // discard dup'd receiver
                        pc += 3;
                        continue;
                    }
                    // Elide `invokespecial java/lang/Object.<init>()V` — the
                    // terminal of every constructor chain. The method body is
                    // a bare `return` and the VM-side registration is
                    // `native_noop_with_this`, so the call has no observable
                    // effect. The site can never become a direct call or an
                    // inline site (`<init>` + native-shadow compile gates), so
                    // without this it falls to `jit_invoke_dispatch`'s
                    // interpreter slow path once per object allocation —
                    // dominant on allocation-heavy code (bintrees18: ~69M
                    // dispatches of an empty method).
                    if op == 0xb7 {
                        if let Some(&idx) = self.invoke_info_idx.get(&pc) {
                            // SAFETY: invoke_info pointers are owned by the
                            // enclosing try_compile scope and outlive codegen.
                            let info = unsafe { &*self.invoke_info[idx].1 };
                            if info.invoke_kind == 1
                                && info.descriptor == "()V"
                                && info.method_name == "<init>"
                                && info.class_name == "java/lang/Object"
                            {
                                let _ = self.pop_stack(); // discard receiver
                                pc += 3;
                                continue;
                            }
                        }
                    }
                    self.flush_scratch_registers();

                    // Check for inline site (invokespecial only — virtual/interface not eligible)
                    if op == 0xb7 && self.inline_sites.contains_key(&pc) {
                        if self.try_emit_inline(pc) {
                            pc += 3;
                            continue;
                        }
                    }

                    // PGO-02 (docs/feature-designs/profile-guided-inlining.md):
                    // guarded MONOMORPHIC virtual/interface inline. `inline_sites`
                    // + `inline_guard_variants` are populated TOGETHER, only for
                    // an admitted `InlineVerdict::Monomorphic` plan, only when
                    // `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE` is on (see
                    // `InlineBackendCaps` in jit/src/lib.rs) — with the flag off
                    // `inline_guard_variants` is always empty and this whole
                    // block costs one HashMap probe. Splices the callee body via
                    // the SAME `try_emit_inline` the invokespecial check above
                    // already uses, behind a receiver class-id guard; the miss
                    // edge falls through UNCHANGED to the normal dispatch code
                    // below (MIC/PIC/`jit_invoke_dispatch`) — never a deopt (see
                    // the design doc's §3 deopt-safety argument: no caller
                    // scopes are populated, so this relies on — and does not
                    // change — the existing guarantee that nothing inside an
                    // inlined body publishes a deopt point).
                    let mut guarded_virtual_done_patches: Vec<usize> = Vec::new();
                    if op != 0xb7 {
                        if let Some(variants) = self.inline_guard_variants.get(&pc).cloned() {
                            // Every variant is the SAME call site, so every
                            // body pops the same operand shape. A disagreement
                            // means the two were resolved from different
                            // descriptors, which this lowering has no model
                            // for — refuse the whole site rather than emit two
                            // guards over two different stack effects.
                            let recv_depth = variants
                                .first()
                                .map(|(_, site)| site.callee_num_args)
                                .unwrap_or(0);
                            let uniform = !variants.is_empty()
                                && variants
                                    .iter()
                                    .all(|(_, site)| site.callee_num_args == recv_depth);
                            if uniform && recv_depth >= 1 && self.stack.len() >= recv_depth {
                                let recv_slot = self.stack[self.stack.len() - recv_depth];

                                // Full state snapshot from BEFORE any guard byte
                                // is emitted — mirrors try_emit_inline's own
                                // checkpoint set exactly (see its comment on the
                                // groovyjarjarasm-asm-handler-getexceptiontablesize
                                // fix for why every one of these fields matters),
                                // so a rewind on either failure path below is
                                // indistinguishable from never having attempted
                                // this guard at all.
                                let buf_checkpoint = self.buf.pos();
                                let stack_checkpoint = self.stack.clone();
                                let oop_marks_checkpoint = self.stack_oop_marks.clone();
                                let spill_checkpoint = self.next_spill_offset;
                                let exception_check_stubs_checkpoint =
                                    self.exception_check_stubs.len();
                                let deopt_stubs_checkpoint = self.deopt_stubs.len();
                                let forward_patches_checkpoint = self.forward_patches.len();
                                let jump_table_patches_checkpoint = self.jump_table_patches.len();
                                let self_call_patches_checkpoint = self.self_call_patches.len();
                                let bounds_check_stubs_checkpoint = self.bounds_check_stubs.len();
                                let null_check_store_stubs_checkpoint =
                                    self.null_check_store_stubs.len();

                                // The receiver is loaded and null-checked ONCE,
                                // ahead of the guard chain: `null` fails every
                                // guard, and re-testing it per variant would be
                                // pure code size. Peeked, not popped —
                                // try_emit_inline_site does its own popping on
                                // each hit path, and the miss tail needs the
                                // receiver+args untouched for the normal-dispatch
                                // code that runs next.
                                self.load_slot_to_reg(RAX, recv_slot);
                                self.emit_test_r64_r64(RAX);
                                let null_miss_patch = self.emit_jcc_rel32_patch(0x84); // JZ

                                // The guard chain. Variant k's mismatch edge
                                // lands at variant k+1's `CMP`; the last one's
                                // lands at the normal-dispatch code below. A hit
                                // jumps PAST that code entirely.
                                let mut pending_miss: Option<usize> = None;
                                let mut spliced_any = false;
                                for (guard_class_id, site) in &variants {
                                    // Per-variant checkpoint: if THIS body cannot
                                    // be spliced, only this variant's bytes are
                                    // rewound — the ones already emitted for
                                    // earlier variants stay.
                                    let variant_buf_checkpoint = self.buf.pos();
                                    let variant_exception_stubs = self.exception_check_stubs.len();
                                    let variant_deopt_stubs = self.deopt_stubs.len();
                                    let variant_forward_patches = self.forward_patches.len();
                                    let variant_jump_table_patches = self.jump_table_patches.len();
                                    let variant_self_call_patches = self.self_call_patches.len();
                                    let variant_bounds_stubs = self.bounds_check_stubs.len();
                                    let variant_null_store_stubs =
                                        self.null_check_store_stubs.len();

                                    // Land the previous variant's mismatch edge
                                    // exactly here. If this variant then fails and
                                    // rewinds, the same offset becomes the start of
                                    // whatever is emitted next — the following
                                    // variant's `CMP`, or the normal-dispatch code
                                    // — which is the correct landing spot either
                                    // way.
                                    if let Some(prev) = pending_miss.take() {
                                        self.patch_rel32_to_here(prev);
                                    }

                                    // CMP DWORD [RAX+0], guard_class_id —
                                    // identical encoding to the String/CRC32
                                    // intrinsic guard above (81 /7 id, ModRM 0x78
                                    // = mod00 /7 rm=RAX).
                                    self.buf.emit(&[0x81, 0x78, 0x00]);
                                    self.buf.emit(&guard_class_id.to_le_bytes());
                                    let this_miss = self.emit_jcc_rel32_patch(0x85); // JNE

                                    if self.try_emit_inline_site(pc, site) {
                                        // Hit: skip every later guard AND the
                                        // normal-dispatch bytes entirely.
                                        guarded_virtual_done_patches
                                            .push(self.emit_jmp_rel32_patch());
                                        spliced_any = true;
                                        pending_miss = Some(this_miss);
                                        // The inline body consumed the
                                        // receiver+args and pushed its result via
                                        // the same push_from_rax /
                                        // push_from_rax_as_xmm0 convention the
                                        // normal-dispatch code below also uses.
                                        // Restore the compiler's SYMBOLIC state
                                        // (not the already-emitted bytes) to
                                        // exactly what it was before the guard
                                        // chain, so the NEXT variant sees the same
                                        // operand stack this one did, and so the
                                        // dispatch code — the only Rust-level
                                        // continuation from here, run
                                        // unconditionally — pops the SAME
                                        // receiver+args positions and pushes a
                                        // canonically-shaped result regardless of
                                        // which machine-code path a given
                                        // execution actually takes at runtime.
                                        self.stack = stack_checkpoint.clone();
                                        self.stack_oop_marks = oop_marks_checkpoint.clone();
                                        self.next_spill_offset = spill_checkpoint;
                                    } else {
                                        // try_emit_inline_site already rolled back
                                        // its OWN side effects; rewind this
                                        // variant's guard bytes too, so the site is
                                        // byte-identical to never having offered
                                        // this variant.
                                        self.buf.rewind_to(variant_buf_checkpoint);
                                        self.stack = stack_checkpoint.clone();
                                        self.stack_oop_marks = oop_marks_checkpoint.clone();
                                        self.next_spill_offset = spill_checkpoint;
                                        self.exception_check_stubs
                                            .truncate(variant_exception_stubs);
                                        self.deopt_stubs.truncate(variant_deopt_stubs);
                                        self.forward_patches.truncate(variant_forward_patches);
                                        self.jump_table_patches
                                            .truncate(variant_jump_table_patches);
                                        self.self_call_patches.truncate(variant_self_call_patches);
                                        self.bounds_check_stubs.truncate(variant_bounds_stubs);
                                        self.null_check_store_stubs
                                            .truncate(variant_null_store_stubs);
                                    }
                                }

                                if spliced_any {
                                    // Every remaining miss edge — the null check
                                    // and the last guard — lands at the exact
                                    // start of the UNCHANGED normal-dispatch code
                                    // that runs next.
                                    self.patch_rel32_to_here(null_miss_patch);
                                    if let Some(last) = pending_miss {
                                        self.patch_rel32_to_here(last);
                                    }
                                } else {
                                    // No variant could be spliced: rewind the
                                    // shared receiver load and null check too, so
                                    // the fall-through is byte-identical to never
                                    // having attempted a guard.
                                    self.buf.rewind_to(buf_checkpoint);
                                    self.stack = stack_checkpoint;
                                    self.stack_oop_marks = oop_marks_checkpoint;
                                    self.next_spill_offset = spill_checkpoint;
                                    self.exception_check_stubs
                                        .truncate(exception_check_stubs_checkpoint);
                                    self.deopt_stubs.truncate(deopt_stubs_checkpoint);
                                    self.forward_patches.truncate(forward_patches_checkpoint);
                                    self.jump_table_patches
                                        .truncate(jump_table_patches_checkpoint);
                                    self.self_call_patches
                                        .truncate(self_call_patches_checkpoint);
                                    self.bounds_check_stubs
                                        .truncate(bounds_check_stubs_checkpoint);
                                    self.null_check_store_stubs
                                        .truncate(null_check_store_stubs_checkpoint);
                                }
                            }
                        }
                    }

                    // The direct exceptional-return service needs the same
                    // invoke metadata as the normal dispatch fallback.
                    let info_ptr = self
                        .invoke_info_idx
                        .get(&pc)
                        .map(|&i| self.invoke_info[i].1);
                    // Check for direct call target (invokespecial with compiled callee)
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let direct = self.direct_calls_idx.get(&pc).map(|&i| {
                        let dc = &self.direct_calls[i].1;
                        note_emit_direct(&self.method_key, pc, dc.entry);
                        (
                            dc.entry,
                            dc.needs_context,
                            dc.num_params,
                            dc.return_type,
                            dc.guard_class_id,
                        )
                    });

                    // Receiver-type speculation consults the per-bci de-spec
                    // registry. A call-site intrinsic with `guard_class_id != 0`
                    // (the `java/lang/CharSequence` String family, the CRC32
                    // family, the atomic-field family) is a SPECULATION: the
                    // inline body it emits is valid for exactly that one
                    // receiver class and every other receiver takes the
                    // `ReceiverTypeChanged` deopt edge. On a site whose receiver
                    // is NEVER that class the guard fails on every single call,
                    // so the method deopts per invocation, is recompiled with
                    // the identical guard, and is finally barred from
                    // compilation altogether by `recommend_action`'s
                    // `MakeNotCompilable` escalation -- for a speculation the
                    // compiler chose, about a program that never satisfied it.
                    // netty's `HttpHeaderValidationUtil.validateValidHeaderValue`
                    // is that shape: its `CharSequence.length()` at bci 1 is
                    // reached only with `AsciiString` and an anonymous
                    // `CharSequence`, and the class measured 1.67 deopts per
                    // loop iteration (docs/known-issues/netty/httpheader-
                    // validationutiltest-exhaustive-loop-timeout-20260816.md).
                    //
                    // `real_frame_deopt_resume_and_despeculate` already records
                    // such a bci in the de-spec registry after
                    // `PER_BCI_DESPEC_LIMIT` deopts and prints "speculation
                    // suppressed on next compile" -- but until now nothing on
                    // the receiver-guard path READ that registry (only the two
                    // loop-hoist gates in `driver.rs` and the
                    // `ArraycopyPrimitive` intrinsic below did), so the claim
                    // was false and the next compile emitted the same guard.
                    // The consult belongs at the RESOLVER, not here, and this
                    // block counts rather than declines. Measured 2026-08-24,
                    // and it cost most of a session: declining a registered
                    // intrinsic in THIS filter does not send the site to the
                    // `else` arm's MIC/PIC dispatch, because that arm needs
                    // `invoke_info` at this pc and there is none. A site the
                    // resolver registered as an intrinsic took
                    // `direct_calls.push(..); continue;` in `try_compile_inner`
                    // BEFORE the `invoke_info.push` below it, so the dispatch
                    // metadata was never built. With both `direct` and
                    // `info_ptr` `None` the emitter falls through to the
                    // unconditional `UnreachedCode` trap at the bottom of this
                    // arm -- so the "declined" site deopts on EVERY execution
                    // instead of dispatching. The de-spec consult read as inert
                    // (deopts 3502 -> 3055 on `HeaderValidationLoopRate`) while
                    // its own `sites-declined` counter said it had fired 51
                    // times; only correlating the decline trace against the
                    // deopt stream separated the two -- 33 declines at
                    // `oldHeaderValueValidationAlgorithm pc=6` and 1466 deopts
                    // at that same bci AFTERWARDS.
                    //
                    // The pre-existing `ArraycopyPrimitive` and
                    // `StringIndexOfChar` filters in the invokestatic ladder
                    // above have the identical shape and therefore the identical
                    // defect; they are left alone here because each needs its
                    // own A/B, and are named on the known-issue page.
                    let direct = direct.filter(|&(_, _, _, _, guard_class_id)| {
                        if guard_class_id == 0 {
                            crate::metrics::note_receiver_despec(
                                crate::metrics::RECEIVER_DESPEC_UNGUARDED,
                            );
                            return true;
                        }
                        crate::metrics::note_receiver_despec(
                            crate::metrics::RECEIVER_DESPEC_GUARD_EMITTED,
                        );
                        true
                    });

                    if let Some((
                        callee_entry,
                        callee_needs_ctx,
                        callee_params,
                        ret_type,
                        guard_class_id,
                    )) = direct
                    {
                        // --- invokevirtual/special/interface intrinsic ladder ---
                        // Instance-method call-site intrinsics (String / CRC32)
                        // are dispatched here BEFORE the plain direct-call
                        // handling below. Unlike the invokestatic ladder,
                        // instance intrinsics treat the deepest stack operand
                        // as the receiver (`this`): the JLS argument count is
                        // `callee_params`, and total operands popped is
                        // `callee_params + 1`.
                        //
                        // A follow-up agent for family <TAG> fills exactly one
                        // region with `if callee_entry ==
                        // crate::JitIntrinsic::Foo.as_entry() {
                        //     <emit inline code>; <handled = true>; }`.
                        // When every region is empty `intrinsic_handled`
                        // stays false and control falls through to the
                        // unchanged plain direct-call path.
                        #[allow(unused_mut)]
                        let mut intrinsic_handled = false;

                        // ===== INTRINSIC REGION BEGIN: FFM_SEGMENT =====
                        // `MemorySegment.getAtIndex`/`setAtIndex`, lowered to a
                        // CALL of the FFM element fast-path helper with a
                        // DECLINE edge that runs the site's ordinary native
                        // dispatch.
                        //
                        // Through that ordinary dispatch these measure ~1158
                        // ns/element against ~0.8 ns for a `short[]` element,
                        // and they are per-ELEMENT: any segment-backed array
                        // drives one per element.
                        //
                        // Not inline machine code, deliberately. The carrier's
                        // liveness model spans two synthetic classes owned by
                        // two different files, and a second copy of one of its
                        // slot indices has already made that check silently
                        // DEAD once (the W7-89 note on `PE_ARENA_CLASS`). The
                        // helper asks the NATIVE for a verdict instead of
                        // re-deriving one — see
                        // `cratonvm_native_builtins::ffm_fast`.
                        //
                        // The decline edge is what makes this safe to be wrong
                        // about: helper returns 0 and control falls into the
                        // same `invoke_dispatch` the site would have used
                        // anyway, so a heap-backed carrier, a closed scope, an
                        // out-of-bounds index or an unrecognised shape all keep
                        // today's behaviour AND today's exceptions. Nothing
                        // here has to reproduce an exception.
                        if !intrinsic_handled
                            && (callee_entry
                                == crate::JitIntrinsic::FfmSegmentGetAtIndex.as_entry()
                                || callee_entry
                                    == crate::JitIntrinsic::FfmSegmentSetAtIndex.as_entry())
                        {
                            let is_get = callee_entry
                                == crate::JitIntrinsic::FfmSegmentGetAtIndex.as_entry();
                            let helper = if is_get {
                                self.helpers.ffm_segment_get
                            } else {
                                self.helpers.ffm_segment_set
                            };
                            // The site's own dispatch info: the decline edge's
                            // call target, and the source of the element kind
                            // (the descriptor NAMES the `ValueLayout` subtype,
                            // so the width is a compile-time constant).
                            let info_ptr = self
                                .invoke_info_idx
                                .get(&pc)
                                .map(|&i| self.invoke_info[i].1);
                            // SAFETY: the pointee is owned by this compile's
                            // `_jit_invoke_infos` arena and outlives the code.
                            let kind = info_ptr.and_then(|p| {
                                if p.is_null() {
                                    None
                                } else {
                                    crate::ffm_kind_for_descriptor(unsafe { (*p).descriptor })
                                }
                            });
                            match (info_ptr, kind) {
                                (Some(info), Some(kind)) if helper != 0 && !info.is_null() => {
                                    // SAFETY: the pointee is owned by this
                                    // compile's `_jit_invoke_infos` arena and
                                    // outlives the code being emitted.
                                    let ret_tag = unsafe { (*info).return_type };
                                    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_FFM")
                                        .is_some()
                                    {
                                        eprintln!(
                                            "[ffm] EMITTED pc={pc} kind={kind} is_get={is_get}"
                                        );
                                    }
                                    self.flush_scratch_registers();
                                    // Operands, deepest first: receiver, layout,
                                    // index, and (set only) the value.
                                    let value_slot =
                                        if is_get { None } else { Some(self.pop_stack()) };
                                    let index_slot = self.pop_stack();
                                    let layout_slot = self.pop_stack();
                                    let recv_slot = self.pop_stack();

                                    // One slot for the helper's out-parameter
                                    // (get only). Reserved BEFORE the decline
                                    // edge's argument buffer so reclaiming that
                                    // buffer cannot free this.
                                    let out_base = if is_get {
                                        match self.reserve_spill_slots(1) {
                                            Some(b) => Some(b),
                                            None => {
                                                self.fail(
                                                    "singlepass-codegen/ffm-out-spill-exhausted",
                                                );
                                                return false;
                                            }
                                        }
                                    } else {
                                        None
                                    };

                                    // ---- fast path -------------------------
                                    // No safepoint spill and no oop map: the
                                    // helper neither allocates nor blocks, so no
                                    // GC can run inside it and no oop it is
                                    // handed can move.
                                    self.load_slot_to_reg(ARG_REGS[0], recv_slot);
                                    self.load_slot_to_reg(ARG_REGS[1], index_slot);
                                    self.emit_mov_imm32_sx(ARG_REGS[2], kind as i32);
                                    // arg3 is the out-pointer for a get and the
                                    // value for a set. `is_get` already pinned
                                    // which of the two is `Some`, but this file
                                    // routes every recoverable case through a
                                    // bail rather than a panic (see the
                                    // `deny(...)` header) — so a shape that
                                    // cannot arise fails the COMPILE, which
                                    // drops the method to the interpreter.
                                    match (out_base, value_slot) {
                                        (Some(out), _) => {
                                            self.emit_lea_frame_slot(ARG_REGS[3], out)
                                        }
                                        (None, Some(v)) => self.load_slot_to_reg(ARG_REGS[3], v),
                                        (None, None) => {
                                            self.fail(
                                                "singlepass-codegen/ffm-missing-arg3-operand",
                                            );
                                            return false;
                                        }
                                    }
                                    self.emit_call_absolute(helper);
                                    self.emit_test_r64_r64(RAX);
                                    // RAX == 0 -> declined.
                                    let declined = self.emit_jcc_rel32_patch(0x84);
                                    if let Some(out) = out_base {
                                        self.emit_load_local(RAX, out);
                                    }
                                    let done = self.emit_jmp_rel32_patch();

                                    // ---- decline edge: the unchanged dispatch
                                    self.patch_rel32_to_here(declined);
                                    let nargs = if is_get { 3 } else { 4 };
                                    let args_base = match self.reserve_spill_slots(nargs) {
                                        Some(b) => b,
                                        None => {
                                            self.fail(
                                                "singlepass-codegen/ffm-args-spill-exhausted",
                                            );
                                            return false;
                                        }
                                    };
                                    // `jit_invoke_dispatch`'s buffer runs
                                    // arg[0] at the HIGHEST offset down to
                                    // arg[n-1] at the lowest — the same layout
                                    // the generic dispatch site builds. Load
                                    // every operand into a distinct register
                                    // before storing any of them: the buffer can
                                    // overlap the operand homes, and a
                                    // load-then-store per index would overwrite
                                    // a home not yet read.
                                    self.load_slot_to_reg(RAX, recv_slot);
                                    self.load_slot_to_reg(RCX, layout_slot);
                                    self.load_slot_to_reg(RDX, index_slot);
                                    if let Some(v) = value_slot {
                                        self.load_slot_to_reg(R10, v);
                                    }
                                    let top = args_base + (nargs as i32 - 1) * 8;
                                    self.emit_store_local(top, RAX);
                                    self.emit_store_local(top - 8, RCX);
                                    self.emit_store_local(top - 16, RDX);
                                    if value_slot.is_some() {
                                        self.emit_store_local(top - 24, R10);
                                    }
                                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                    self.emit_mov_imm64(ARG_REGS[1], info as *const _ as i64);
                                    self.emit_lea_frame_slot(ARG_REGS[2], top);
                                    self.emit_mov_imm32_sx(ARG_REGS[3], nargs as i32);
                                    self.emit_pre_safepoint_spill();
                                    self.emit_call_absolute(self.helpers.invoke_dispatch);
                                    self.emit_oop_map_for_safepoint();
                                    // The site's REAL return tag, not `b'I'`.
                                    // `emit_post_invoke_exception_check` has a
                                    // separate arm for `J`/`D`/`F` because the
                                    // pending-exception sentinel is `i64::MIN`,
                                    // which is also a LEGITIMATE value for those
                                    // widths — `-0.0` as a double is exactly
                                    // that word. Passing `b'I'` took the plain
                                    // `CMP RAX, i64::MIN; JE bail` arm and would
                                    // have mistaken such a value for a throw.
                                    self.emit_post_invoke_exception_check(if is_get {
                                        ret_tag
                                    } else {
                                        b'V'
                                    });

                                    // ---- join ------------------------------
                                    self.patch_rel32_to_here(done);
                                    // Reclaim both the argument buffer and the
                                    // out slot; RAX already carries whichever
                                    // path ran.
                                    self.next_spill_offset =
                                        out_base.unwrap_or(args_base).min(args_base);
                                    if is_get {
                                        // Both arms converge with the value's
                                        // RAW BITS in RAX — the helper returns
                                        // them that way and `invoke_dispatch`
                                        // already did — so one push serves the
                                        // fast path and the decline edge. A
                                        // float/double has to reach an XMM
                                        // stack slot, which is the same
                                        // `MOVQ XMM0, RAX` the generic dispatch
                                        // emits for those return types.
                                        if matches!(ret_tag, b'F' | b'D') {
                                            self.push_from_rax_as_xmm0();
                                        } else {
                                            self.push_from_rax();
                                        }
                                    }
                                    intrinsic_handled = true;
                                }
                                // Falling through here is NOT safe: the site
                                // carries an intrinsic SENTINEL as its
                                // `JitDirectCall::entry`, and the ordinary
                                // direct-call path would CALL that sentinel
                                // (measured: `EXCEPTION_ACCESS_VIOLATION at
                                // pc=0xFFFFFFFFFFFFFFB1`). Registration and
                                // emission are gated on the same
                                // `ffm_kind_for_descriptor`, so reaching this
                                // arm means they disagreed — bail the whole
                                // compile, which drops the method to the
                                // interpreter and is always safe.
                                _ => {
                                    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_FFM")
                                        .is_some()
                                    {
                                        eprintln!(
                                            "[ffm] UNHANDLED pc={pc} info={} kind={:?} helper={}",
                                            info_ptr.is_some(),
                                            kind,
                                            helper != 0
                                        );
                                    }
                                    self.fail("singlepass-codegen/ffm-unhandled-sentinel");
                                    return false;
                                }
                            }
                        }
                        // ===== INTRINSIC REGION END: FFM_SEGMENT =====

                        // ===== INTRINSIC REGION BEGIN: ATOMIC_INT =====
                        // `AtomicInteger` RMW family, emitted as ONE
                        // `LOCK XADD [value], ECX`.
                        //
                        // `XADD` atomically adds the source register to the
                        // destination and leaves the PRE-add value in the
                        // source, which is exactly `getAndAdd` semantics; the
                        // `*AndGet` forms add the delta back afterwards. That
                        // replaces a full native dispatch (~250 ns/op measured)
                        // with a single locked instruction.
                        //
                        // Soundness rests on three things:
                        //   * the registered native keeps its state in the SAME
                        //     memory (`get_field_volatile(this, 0)` /
                        //     `compare_and_swap_field(this, 0, ..)`), so an
                        //     interpreted caller and a compiled caller still
                        //     agree on one location;
                        //   * the receiver class-id guard below — AtomicInteger
                        //     is not final, so a subclass override must NOT take
                        //     this path;
                        //   * the per-object COMPACT/LEGACY branch, the same one
                        //     `emit_load_string_i32_field` uses, because a class
                        //     with a registered `CompactLayout` may still have
                        //     legacy-laid-out instances.
                        // Every uncertain case (null receiver, class mismatch)
                        // goes to the shared uncommon-trap stub and re-runs in
                        // the interpreter, which reproduces the NPE exactly.
                        if !intrinsic_handled {
                            // (delta_imm, return_post_add, delta_is_arg)
                            //
                            // `AtomicIntGet` is the one arm with no delta at
                            // all: it reads the same slot the RMW forms address
                            // and returns it. Everything before the final
                            // instruction -- null check, class guard, the
                            // per-object COMPACT/LEGACY branch, the deopt stub
                            // -- is shared, which is the whole reason it belongs
                            // in this block rather than beside it.
                            let is_load =
                                callee_entry == crate::JitIntrinsic::AtomicIntGet.as_entry();
                            let plan: Option<(i32, bool, bool)> = if is_load {
                                Some((0, false, false))
                            } else if callee_entry
                                == crate::JitIntrinsic::AtomicIntGetAndIncrement.as_entry()
                            {
                                Some((1, false, false))
                            } else if callee_entry
                                == crate::JitIntrinsic::AtomicIntGetAndDecrement.as_entry()
                            {
                                Some((-1, false, false))
                            } else if callee_entry
                                == crate::JitIntrinsic::AtomicIntIncrementAndGet.as_entry()
                            {
                                Some((1, true, false))
                            } else if callee_entry
                                == crate::JitIntrinsic::AtomicIntDecrementAndGet.as_entry()
                            {
                                Some((-1, true, false))
                            } else if callee_entry
                                == crate::JitIntrinsic::AtomicIntGetAndAdd.as_entry()
                            {
                                Some((0, false, true))
                            } else if callee_entry
                                == crate::JitIntrinsic::AtomicIntAddAndGet.as_entry()
                            {
                                Some((0, true, true))
                            } else {
                                None
                            };
                            if let Some((delta_imm, return_post_add, delta_is_arg)) = plan {
                                // Recomputed from the same two inputs the
                                // matcher used; `None` here cannot happen for a
                                // registered site, and bailing keeps the plain
                                // direct-call path rather than emitting a CALL
                                // to an intrinsic sentinel.
                                if let Some(layout) =
                                    crate::AtomicIntFieldLayout::new(0, guard_class_id)
                                {
                                    self.flush_scratch_registers();
                                    if crate::deopt_real_enabled() {
                                        self.snapshot_pre_intrinsic_call(
                                            pc,
                                            crate::deopt::DeoptReason::ReceiverTypeChanged,
                                        );
                                    }
                                    let mut bail: Vec<usize> = Vec::new();
                                    // Operands: delta (if any) is shallower,
                                    // the receiver is deepest.
                                    let delta_slot = if delta_is_arg {
                                        Some(self.pop_stack())
                                    } else {
                                        None
                                    };
                                    let recv_slot = self.pop_stack();

                                    // RAX = receiver; null → deopt.
                                    self.load_slot_to_reg(RAX, recv_slot);
                                    self.emit_test_r64_r64(RAX);
                                    bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                                    // Exact receiver class guard:
                                    // CMP DWORD [RAX + 0], guard_class_id ; JNE
                                    self.buf.emit(&[0x81, 0x78, 0x00]);
                                    self.buf.emit(&guard_class_id.to_le_bytes());
                                    bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                                    // EDX = delta (kept for the *AndGet fixup,
                                    // since XADD overwrites its source with the
                                    // pre-add value).
                                    if !is_load {
                                        match delta_slot {
                                            Some(slot) => self.load_slot_to_reg(RDX, slot),
                                            None => {
                                                self.buf.emit(&[0xBA]); // MOV EDX, imm32
                                                self.buf.emit(&delta_imm.to_le_bytes());
                                            }
                                        }
                                        self.buf.emit(&[0x89, 0xD1]); // MOV ECX, EDX
                                    }

                                    // Per-object layout branch.
                                    self.emit_test_mem8_imm8(
                                        RAX,
                                        cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                        cratonvm_types::GC_FLAG_COMPACT,
                                    );
                                    let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                                    if is_load {
                                        // MOV ECX, [RAX + compact]. A plain load
                                        // is the correct volatile/acquire read on
                                        // x86-64: loads are not reordered with
                                        // older loads, so nothing is owed here.
                                        self.buf.emit(&[0x8B, 0x88]);
                                        self.buf.emit(&layout.value_compact_offset.to_le_bytes());
                                    } else {
                                        // LOCK XADD [RAX + compact], ECX
                                        self.buf.emit(&[0xF0, 0x0F, 0xC1, 0x88]);
                                        self.buf.emit(&layout.value_compact_offset.to_le_bytes());
                                    }
                                    let done = self.emit_jmp_rel32_patch();
                                    self.patch_rel32_to_here(legacy);
                                    if is_load {
                                        // MOV ECX, [RAX + legacy]
                                        self.buf.emit(&[0x8B, 0x88]);
                                        self.buf.emit(&layout.value_legacy_offset.to_le_bytes());
                                    } else {
                                        // LOCK XADD [RAX + legacy], ECX
                                        self.buf.emit(&[0xF0, 0x0F, 0xC1, 0x88]);
                                        self.buf.emit(&layout.value_legacy_offset.to_le_bytes());
                                    }
                                    self.patch_rel32_to_here(done);

                                    // ECX now holds the PRE-add value.
                                    if return_post_add {
                                        self.buf.emit(&[0x01, 0xD1]); // ADD ECX, EDX
                                    }
                                    self.buf.emit(&[0x48, 0x63, 0xC1]); // MOVSXD RAX, ECX
                                    self.push_from_rax();

                                    for p in bail {
                                        self.deopt_stubs.push((p, pc, 6));
                                    }
                                    intrinsic_handled = true;
                                }
                            }
                        }
                        // ===== INTRINSIC REGION END: ATOMIC_INT =====

                        // ===== INTRINSIC REGION BEGIN: ATOMIC_LONG =====
                        // `AtomicLong`, emitted as ONE REX.W `LOCK XADD
                        // [value], RCX`. Structurally identical to the 32-bit
                        // region above -- same null check, same exact class-id
                        // guard, same per-object COMPACT/LEGACY branch, same
                        // deopt stub for every uncertain case -- and different
                        // only in operand width.
                        //
                        // The width differences are the whole risk surface, so
                        // they are spelled out:
                        //   * every access carries REX.W (0x48), so it reads
                        //     and writes 8 bytes;
                        //   * the layout's LEGACY offset is the 64-bit payload
                        //     offset inside the `Value` cell, not the 32-bit
                        //     one, and a compact storage width other than 8 is
                        //     refused by `AtomicLongFieldLayout::new`;
                        //   * the delta immediate is `MOV RDX, imm32`
                        //     sign-extended, which is exact for the only
                        //     immediates this region uses, +1 and -1;
                        //   * no `MOVSXD` at the end -- the value is already
                        //     64-bit in RCX, and sign-extending it would be
                        //     both wrong and unnecessary.
                        //
                        // An aligned 8-byte `MOV` is atomic on x86-64 and is a
                        // correct volatile/acquire load, so the `get` arm owes
                        // no `LOCK` and no fence.
                        if !intrinsic_handled {
                            let is_load =
                                callee_entry == crate::JitIntrinsic::AtomicLongGet.as_entry();
                            // (delta_imm, return_post_add, delta_is_arg)
                            let plan: Option<(i32, bool, bool)> = if is_load {
                                Some((0, false, false))
                            } else if callee_entry
                                == crate::JitIntrinsic::AtomicLongGetAndIncrement.as_entry()
                            {
                                Some((1, false, false))
                            } else if callee_entry
                                == crate::JitIntrinsic::AtomicLongGetAndDecrement.as_entry()
                            {
                                Some((-1, false, false))
                            } else if callee_entry
                                == crate::JitIntrinsic::AtomicLongIncrementAndGet.as_entry()
                            {
                                Some((1, true, false))
                            } else if callee_entry
                                == crate::JitIntrinsic::AtomicLongDecrementAndGet.as_entry()
                            {
                                Some((-1, true, false))
                            } else if callee_entry
                                == crate::JitIntrinsic::AtomicLongGetAndAdd.as_entry()
                            {
                                Some((0, false, true))
                            } else if callee_entry
                                == crate::JitIntrinsic::AtomicLongAddAndGet.as_entry()
                            {
                                Some((0, true, true))
                            } else {
                                None
                            };
                            if let Some((delta_imm, return_post_add, delta_is_arg)) = plan {
                                // Recomputed from the same two inputs the
                                // matcher used; `None` cannot happen for a
                                // registered site, and bailing keeps the plain
                                // direct-call path rather than emitting a CALL
                                // to an intrinsic sentinel.
                                if let Some(layout) =
                                    crate::AtomicLongFieldLayout::new(0, guard_class_id)
                                {
                                    self.flush_scratch_registers();
                                    if crate::deopt_real_enabled() {
                                        self.snapshot_pre_intrinsic_call(
                                            pc,
                                            crate::deopt::DeoptReason::ReceiverTypeChanged,
                                        );
                                    }
                                    let mut bail: Vec<usize> = Vec::new();
                                    // Operands: delta (if any) is shallower,
                                    // the receiver is deepest.
                                    let delta_slot = if delta_is_arg {
                                        Some(self.pop_stack())
                                    } else {
                                        None
                                    };
                                    let recv_slot = self.pop_stack();

                                    // RAX = receiver; null -> deopt.
                                    self.load_slot_to_reg(RAX, recv_slot);
                                    self.emit_test_r64_r64(RAX);
                                    bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                                    // Exact receiver class guard:
                                    // CMP DWORD [RAX + 0], guard_class_id ; JNE
                                    self.buf.emit(&[0x81, 0x78, 0x00]);
                                    self.buf.emit(&guard_class_id.to_le_bytes());
                                    bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                                    // RDX = delta (kept for the *AndGet fixup,
                                    // since XADD overwrites its source with the
                                    // pre-add value).
                                    if !is_load {
                                        match delta_slot {
                                            Some(slot) => self.load_slot_to_reg(RDX, slot),
                                            None => {
                                                // MOV RDX, imm32 (sign-extended)
                                                self.buf.emit(&[0x48, 0xC7, 0xC2]);
                                                self.buf.emit(&delta_imm.to_le_bytes());
                                            }
                                        }
                                        self.buf.emit(&[0x48, 0x89, 0xD1]); // MOV RCX, RDX
                                    }

                                    // Per-object layout branch.
                                    self.emit_test_mem8_imm8(
                                        RAX,
                                        cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                        cratonvm_types::GC_FLAG_COMPACT,
                                    );
                                    let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                                    if is_load {
                                        // MOV RCX, [RAX + compact]
                                        self.buf.emit(&[0x48, 0x8B, 0x88]);
                                        self.buf.emit(&layout.value_compact_offset.to_le_bytes());
                                    } else {
                                        // LOCK XADD [RAX + compact], RCX
                                        self.buf.emit(&[0xF0, 0x48, 0x0F, 0xC1, 0x88]);
                                        self.buf.emit(&layout.value_compact_offset.to_le_bytes());
                                    }
                                    let done = self.emit_jmp_rel32_patch();
                                    self.patch_rel32_to_here(legacy);
                                    if is_load {
                                        // MOV RCX, [RAX + legacy]
                                        self.buf.emit(&[0x48, 0x8B, 0x88]);
                                        self.buf.emit(&layout.value_legacy_offset.to_le_bytes());
                                    } else {
                                        // LOCK XADD [RAX + legacy], RCX
                                        self.buf.emit(&[0xF0, 0x48, 0x0F, 0xC1, 0x88]);
                                        self.buf.emit(&layout.value_legacy_offset.to_le_bytes());
                                    }
                                    self.patch_rel32_to_here(done);

                                    // RCX now holds the PRE-add value.
                                    if return_post_add {
                                        self.buf.emit(&[0x48, 0x01, 0xD1]); // ADD RCX, RDX
                                    }
                                    self.buf.emit(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
                                    self.push_from_rax();

                                    for p in bail {
                                        self.deopt_stubs.push((p, pc, 6));
                                    }
                                    intrinsic_handled = true;
                                }
                            }
                        }
                        // ===== INTRINSIC REGION END: ATOMIC_LONG =====

                        // ===== INTRINSIC REGION BEGIN: BOX_UNBOX =====
                        // `Long.longValue()J` and `Integer.intValue()I` — the
                        // UNBOX half of autoboxing, emitted as the same aligned
                        // `MOV` the two `*Get` arms above emit. `Long.value` /
                        // `Integer.value` are `private final` at field slot 0:
                        // a plain load, no `LOCK`, no fence, nothing to order.
                        //
                        // Structurally this is the `is_load` path of the two
                        // regions above with a different class guard, and it is
                        // written out rather than shared with them because the
                        // two differ in exactly one thing an abstraction would
                        // have to re-introduce anyway — the operand width, and
                        // with it the final sign-extension (`MOVSXD` for `I`,
                        // none for `J`).
                        //
                        // Null receiver deopts to the interpreter (bail reason
                        // 6), which re-runs the unbox and raises the same NPE
                        // `Long.longValue` on `null` raises today. Nothing is
                        // CALLed on the inline path.
                        if !intrinsic_handled {
                            let is_long =
                                callee_entry == crate::JitIntrinsic::LongLongValue.as_entry();
                            let is_int =
                                callee_entry == crate::JitIntrinsic::IntegerIntValue.as_entry();
                            if is_long || is_int {
                                // Recomputed from the same two inputs the
                                // matcher used. `None` cannot happen for a
                                // registered site; bailing keeps the plain
                                // direct-call path rather than emitting a CALL
                                // to an intrinsic sentinel.
                                let offsets = if is_long {
                                    crate::AtomicLongFieldLayout::new(0, guard_class_id)
                                        .map(|l| (l.value_compact_offset, l.value_legacy_offset))
                                } else {
                                    crate::AtomicIntFieldLayout::new(0, guard_class_id)
                                        .map(|l| (l.value_compact_offset, l.value_legacy_offset))
                                };
                                if let Some((compact_off, legacy_off)) = offsets {
                                    self.flush_scratch_registers();
                                    if crate::deopt_real_enabled() {
                                        self.snapshot_pre_intrinsic_call(
                                            pc,
                                            crate::deopt::DeoptReason::ReceiverTypeChanged,
                                        );
                                    }
                                    let mut bail: Vec<usize> = Vec::new();
                                    let recv_slot = self.pop_stack();

                                    // RAX = receiver; null -> deopt.
                                    self.load_slot_to_reg(RAX, recv_slot);
                                    self.emit_test_r64_r64(RAX);
                                    bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                                    // Exact receiver class guard:
                                    // CMP DWORD [RAX + 0], guard_class_id ; JNE
                                    self.buf.emit(&[0x81, 0x78, 0x00]);
                                    self.buf.emit(&guard_class_id.to_le_bytes());
                                    bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                                    // Per-object layout branch, exactly as the
                                    // `Atomic*` arms do it: a COMPACT instance
                                    // stores the payload at the registered body
                                    // offset, a LEGACY one inside its 16-byte
                                    // `Value` cell.
                                    self.emit_test_mem8_imm8(
                                        RAX,
                                        cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32,
                                        cratonvm_types::GC_FLAG_COMPACT,
                                    );
                                    let legacy = self.emit_jcc_rel32_patch(0x84); // JZ
                                    if is_long {
                                        // MOV RCX, [RAX + compact]
                                        self.buf.emit(&[0x48, 0x8B, 0x88]);
                                    } else {
                                        // MOV ECX, [RAX + compact]
                                        self.buf.emit(&[0x8B, 0x88]);
                                    }
                                    self.buf.emit(&compact_off.to_le_bytes());
                                    let done = self.emit_jmp_rel32_patch();
                                    self.patch_rel32_to_here(legacy);
                                    if is_long {
                                        // MOV RCX, [RAX + legacy]
                                        self.buf.emit(&[0x48, 0x8B, 0x88]);
                                    } else {
                                        // MOV ECX, [RAX + legacy]
                                        self.buf.emit(&[0x8B, 0x88]);
                                    }
                                    self.buf.emit(&legacy_off.to_le_bytes());
                                    self.patch_rel32_to_here(done);

                                    if is_long {
                                        self.buf.emit(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
                                    } else {
                                        // MOVSXD RAX, ECX — an `int` is kept
                                        // sign-extended in the 64-bit operand
                                        // slot, the same way the `AtomicInt`
                                        // arm above ends.
                                        self.buf.emit(&[0x48, 0x63, 0xC1]);
                                    }
                                    self.push_from_rax();

                                    for p in bail {
                                        self.deopt_stubs.push((p, pc, 6));
                                    }
                                    intrinsic_handled = true;
                                }
                            }
                        }
                        // ===== INTRINSIC REGION END: BOX_UNBOX =====

                        // ===== INTRINSIC REGION BEGIN: STRING_ACCESS =====
                        // java.lang.String access intrinsics (Phase 3a):
                        // length()I, isEmpty()Z, charAt(I)C, hashCode()I.
                        //
                        // These are registered by `try_resolve_string_intrinsic`
                        // ONLY when a `StringFieldLayout` with a `coder` field
                        // resolved for this compilation; that same layout is in
                        // `self.string_layout`. The defensive `if let Some` below
                        // therefore always matches when a String sentinel is
                        // seen — but if it somehow does not (layout unexpectedly
                        // absent), `intrinsic_handled` stays false and control
                        // falls through to the normal direct-call path, so the
                        // intrinsic sentinel is never mis-`CALL`ed.
                        //
                        // String representation (compact): `value` is a `byte[]`,
                        // `coder` is 0 (LATIN1, 1 byte/char) or 1 (UTF16, 2 LE
                        // bytes/char). `length() == value.length >> coder`.
                        //
                        // Every uncertain case — null receiver, null `value`
                        // array, charAt index out of bounds — branches to a
                        // shared uncommon-trap deopt stub (reason 6) which
                        // re-runs the whole method in the interpreter; the
                        // native `lang_string.rs` impl then reproduces the
                        // exact NPE / StringIndexOutOfBoundsException / value
                        // semantics. No `CALL` is emitted on the inline path.
                        if let Some(layout) = self.string_layout {
                            let acc = if callee_entry
                                == crate::JitIntrinsic::StringLength.as_entry()
                            {
                                Some(0u8)
                            } else if callee_entry == crate::JitIntrinsic::StringIsEmpty.as_entry()
                            {
                                Some(1)
                            } else if callee_entry == crate::JitIntrinsic::StringCharAt.as_entry() {
                                Some(2)
                            } else if callee_entry == crate::JitIntrinsic::StringHashCode.as_entry()
                            {
                                Some(3)
                            } else {
                                None
                            };
                            if let Some(kind) = acc {
                                self.flush_scratch_registers();
                                // Step 6: snapshot before arg pops so a null-
                                // receiver / class-id guard bail resumes at the
                                // invokevirtual bci (receiver [+ index] on stack).
                                if crate::deopt_real_enabled() {
                                    self.snapshot_pre_intrinsic_call(
                                        pc,
                                        crate::deopt::DeoptReason::ReceiverTypeChanged,
                                    );
                                }
                                let mut bail: Vec<usize> = Vec::new();

                                // Pop operands. charAt has an index arg
                                // (shallower); the receiver is always deepest.
                                let index_slot = if kind == 2 {
                                    Some(self.pop_stack())
                                } else {
                                    None
                                };
                                let recv_slot = self.pop_stack();

                                // RAX = receiver. Null receiver → deopt.
                                self.load_slot_to_reg(RAX, recv_slot);
                                self.emit_test_r64_r64(RAX);
                                bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                                // Receiver class-id guard. For a
                                // `java/lang/String` call site (final →
                                // monomorphic) `guard_class_id == 0` and no
                                // guard is emitted. For a `java/lang/CharSequence`
                                // site the receiver may be any CharSequence, so
                                // this inline String-layout decode is valid only
                                // when the receiver is actually a String: compare
                                // the ObjectHeader class id at [recv+0] against
                                // the String class id and deopt (→ native
                                // dispatch) on a mismatch (e.g. a StringBuilder /
                                // StringBuffer receiver). Same guard the CRC32
                                // family uses; see its STRING_SEARCH-adjacent
                                // region below.
                                if guard_class_id != 0 {
                                    // CMP DWORD [RAX + 0], guard_class_id
                                    //   81 /7 id, ModRM 0x78 = mod00 /7 rm=RAX.
                                    self.buf.emit(&[0x81, 0x78, 0x00]);
                                    self.buf.emit(&guard_class_id.to_le_bytes());
                                    bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE
                                }

                                // RCX = value (byte[]) ref. Null → deopt.
                                self.emit_load_string_value_ptr(
                                    RCX,
                                    RAX,
                                    layout.value_compact_offset,
                                    layout.value_legacy_offset,
                                );
                                self.emit_test_r64_r64(RCX);
                                bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ

                                // R10D = coder (0 LATIN1 / 1 UTF16).
                                self.emit_load_string_i32_field(
                                    R10,
                                    RAX,
                                    layout.coder_compact_offset,
                                    layout.coder_compact_is_byte,
                                    layout.coder_legacy_offset,
                                );

                                if kind == 3 {
                                    // hashCode(): first read the cached `hash`
                                    // int. A non-zero cache is the result —
                                    // matches the native lazy cache. (A zero
                                    // cache, or an empty string, recomputes;
                                    // the inline path does NOT write the cache
                                    // back — the returned value is identical
                                    // either way, the cache is a
                                    // non-observable optimisation.)
                                    self.emit_load_string_i32_field(
                                        RAX,
                                        RAX,
                                        layout.hash_compact_offset,
                                        false,
                                        layout.hash_legacy_offset,
                                    );
                                    // TEST EAX,EAX ; JNZ cached_done
                                    self.buf.emit(&[0x85, 0xC0]);
                                    let cached_done = self.emit_jcc_rel32_patch(0x85); // JNZ

                                    // Recompute: char_count = value.len >> coder.
                                    // R11D = value.length (zero-extended).
                                    self.buf
                                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                        .emit(&[0x44, 0x8B, 0x59, ARRAY_LENGTH_OFFSET as u8]); // MOV R11D,[RCX+12]
                                                                                               // MOV ECX, R10D ; SHR R11D, CL
                                    self.buf.emit(&[0x44, 0x89, 0xD1]);
                                    self.buf.emit(&[0x41, 0xD3, 0xEB]);
                                    // Reload value ptr into RDX (RCX is now CL
                                    // scratch). value cell is still in [RAX..]
                                    // — but RAX now holds h(=0 region); reload
                                    // from the receiver. Receiver was clobbered:
                                    // re-pop is not possible. Instead keep value
                                    // ptr safe: recompute from recv_slot.
                                    self.load_slot_to_reg(RDX, recv_slot);
                                    self.emit_load_string_value_ptr(
                                        RDX,
                                        RDX,
                                        layout.value_compact_offset,
                                        layout.value_legacy_offset,
                                    );
                                    // h = 0 (EAX) ; i = 0 (R8D).
                                    self.emit_xor_reg_self(RAX);
                                    self.buf.emit(&[0x45, 0x31, 0xC0]); // XOR R8D,R8D
                                                                        // loop: CMP R8D,R11D ; JGE done
                                    let loop_top = self.buf.pos();
                                    self.buf.emit(&[0x45, 0x39, 0xD8]); // CMP R8D,R11D
                                    let loop_done = self.emit_jcc_rel32_patch(0x8D); // JGE
                                                                                     // decode char into ECX: coder branch.
                                                                                     // TEST R10D,R10D ; JNZ utf16
                                    self.buf.emit(&[0x45, 0x85, 0xD2]);
                                    let utf16 = self.emit_jcc_rel32_patch(0x85);
                                    // LATIN1: MOVZX ECX, BYTE [RDX+R8*1+40]
                                    self.buf.emit(&[
                                        0x42,
                                        0x0F,
                                        0xB6,
                                        0x4C,
                                        0x02,
                                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                        HEADER_SIZE as u8,
                                    ]);
                                    let dec_done = self.emit_jmp_rel32_patch();
                                    // UTF16: MOVZX ECX, WORD [RDX+R8*2+40]
                                    self.patch_rel32_to_here(utf16);
                                    self.buf.emit(&[
                                        0x42,
                                        0x0F,
                                        0xB7,
                                        0x4C,
                                        0x42,
                                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                        HEADER_SIZE as u8,
                                    ]);
                                    self.patch_rel32_to_here(dec_done);
                                    // h = h*31 + c  ==  (h<<5) - h + c.
                                    // MOV R9D,EAX ; SHL EAX,5 ; SUB EAX,R9D ;
                                    // ADD EAX,ECX
                                    self.buf.emit(&[0x41, 0x89, 0xC1]); // MOV R9D,EAX
                                    self.buf.emit(&[0xC1, 0xE0, 0x05]); // SHL EAX,5
                                    self.buf.emit(&[0x44, 0x29, 0xC8]); // SUB EAX,R9D
                                    self.buf.emit(&[0x01, 0xC8]); // ADD EAX,ECX
                                                                  // INC R8D ; JMP loop
                                    self.buf.emit(&[0x41, 0xFF, 0xC0]);
                                    let back = self.emit_jmp_rel32_patch();
                                    // Cast: value to i32 (encoding immediate/displacement)
                                    let rel = loop_top as i32 - (back as i32 + 4);
                                    self.buf.try_patch_i32(back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                                    self.patch_rel32_to_here(loop_done);
                                    self.patch_rel32_to_here(cached_done);
                                    // Result (EAX) is sign-extended on push.
                                    self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
                                    self.push_from_rax();
                                } else if kind == 2 {
                                    // charAt(I)C. Register plan:
                                    //   R8  = value ptr   (RCX freed for CL)
                                    //   R9  = index       (survives SHR)
                                    //   R11 = char_count
                                    //   R10 = coder
                                    // R9 = index.
                                    self.load_slot_to_reg(R9, index_slot.unwrap());
                                    // R11D = value.length (zero-extended) —
                                    // read BEFORE freeing RCX.
                                    self.buf
                                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                        .emit(&[0x44, 0x8B, 0x59, ARRAY_LENGTH_OFFSET as u8]); // MOV R11D,[RCX+12]
                                                                                               // R8 = value ptr (so CL can use RCX).
                                    self.buf.emit(&[0x49, 0x89, 0xC8]); // MOV R8,RCX
                                                                        // char_count = value.length >> coder.
                                                                        // MOV ECX,R10D ; SHR R11D,CL
                                    self.buf.emit(&[0x44, 0x89, 0xD1]);
                                    self.buf.emit(&[0x41, 0xD3, 0xEB]);
                                    // Bounds: (unsigned) index >= char_count
                                    // → deopt (also catches negative index).
                                    // CMP R9D, R11D ; JAE deopt
                                    self.buf.emit(&[0x45, 0x39, 0xD9]);
                                    bail.push(self.emit_jcc_rel32_patch(0x83));
                                    // Decode: coder branch. R10D = coder,
                                    // R9 = index, R8 = value ptr.
                                    self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D,R10D
                                    let utf16 = self.emit_jcc_rel32_patch(0x85);
                                    // LATIN1: MOVZX EAX, BYTE [R8+R9*1+40]
                                    self.buf.emit(&[
                                        0x43,
                                        0x0F,
                                        0xB6,
                                        0x44,
                                        0x08,
                                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                        HEADER_SIZE as u8,
                                    ]);
                                    let dec_done = self.emit_jmp_rel32_patch();
                                    // UTF16: MOVZX EAX, WORD [R8+R9*2+40]
                                    self.patch_rel32_to_here(utf16);
                                    self.buf.emit(&[
                                        0x43,
                                        0x0F,
                                        0xB7,
                                        0x44,
                                        0x48,
                                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                        HEADER_SIZE as u8,
                                    ]);
                                    self.patch_rel32_to_here(dec_done);
                                    // char result already zero-extended in EAX.
                                    self.push_from_rax();
                                } else {
                                    // length()I (kind 0) / isEmpty()Z (kind 1).
                                    // EAX = value.length (zero-extended).
                                    // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                    self.buf.emit(&[0x8B, 0x41, ARRAY_LENGTH_OFFSET as u8]); // MOV EAX,[RCX+12]
                                                                                             // MOV ECX,R10D ; SHR EAX,CL  → char count.
                                    self.buf.emit(&[0x44, 0x89, 0xD1]);
                                    self.buf.emit(&[0xD3, 0xE8]);
                                    if kind == 1 {
                                        // isEmpty: EAX = (char_count == 0).
                                        // TEST EAX,EAX ; SETE AL ; MOVZX EAX,AL
                                        self.buf.emit(&[0x85, 0xC0]);
                                        self.buf.emit(&[0x0F, 0x94, 0xC0]);
                                        self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                                    } else {
                                        // length: sign-extend the int result.
                                        self.buf.emit(&[0x48, 0x63, 0xC0]);
                                    }
                                    self.push_from_rax();
                                }

                                // Wire every bail edge to one shared
                                // uncommon-trap stub (reason 6).
                                for p in bail {
                                    self.deopt_stubs.push((p, pc, 6));
                                }
                                intrinsic_handled = true;
                            }
                        }
                        // ===== INTRINSIC REGION END: STRING_ACCESS =====

                        // ===== INTRINSIC REGION BEGIN: STRING_SEARCH =====
                        // java.lang.String search intrinsics (Phase 3b):
                        // equals(Ljava/lang/Object;)Z, compareTo(String)I,
                        // indexOf(I)I and indexOf(String)I.
                        //
                        // `equals` strategy. The receiver is a `java/lang/String`
                        // (monomorphic — String is final). The emitted code:
                        //   * other == null            → result 0 (false)
                        //   * this.ptr == other.ptr    → result 1 (true)
                        //   * other's ObjectHeader class id != this's
                        //                              → deopt (non-String
                        //                                argument: native
                        //                                equals returns false)
                        //   * this.value/other.value null   → deopt
                        //   * this.coder != other.coder     → deopt (rare;
                        //                                native compares the
                        //                                decoded char slices)
                        //   * value-array lengths differ    → result 0
                        //   * else REP CMPSB over the bytes → 1 iff identical
                        // Same coder + identical backing bytes ⇒ identical
                        // decoded strings, so the raw byte compare is exact.
                        // No `CALL` on the inline path; every deopt edge re-runs
                        // the method in the interpreter (native `equals`).
                        if self.string_layout.is_some()
                            && callee_entry == crate::JitIntrinsic::StringEquals.as_entry()
                        {
                            let layout = self.string_layout.unwrap();
                            self.flush_scratch_registers();
                            // Step 6: snapshot (this, other) before pops.
                            if crate::deopt_real_enabled() {
                                self.snapshot_pre_intrinsic_call(
                                    pc,
                                    crate::deopt::DeoptReason::ReceiverTypeChanged,
                                );
                            }
                            let mut bail: Vec<usize> = Vec::new();

                            // Operand stack (deepest first): this, other.
                            let other_slot = self.pop_stack();
                            let this_slot = self.pop_stack();

                            // RAX = this, RDX = other.
                            self.load_slot_to_reg(RAX, this_slot);
                            self.load_slot_to_reg(RDX, other_slot);

                            // other == null → result 0.
                            self.emit_test_r64_r64(RDX);
                            let other_null = self.emit_jcc_rel32_patch(0x84); // JZ

                            // this.ptr == other.ptr → result 1.
                            // CMP RAX,RDX
                            self.buf.emit(&[0x48, 0x39, 0xD0]);
                            let same_ref = self.emit_jcc_rel32_patch(0x84); // JZ

                            // Class-id check: ObjectHeader.class_id is the i32
                            // at offset 0. `this` is a String, so [RAX] is
                            // String's class id; a differing [RDX] means a
                            // non-String argument → deopt.
                            // MOV ECX,[RAX] ; CMP ECX,[RDX]
                            self.buf.emit(&[0x8B, 0x08]);
                            self.buf.emit(&[0x3B, 0x0A]);
                            bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                            // R8 = this.value, R9 = other.value (byte[] refs).
                            self.emit_load_string_value_ptr(
                                R8,
                                RAX,
                                layout.value_compact_offset,
                                layout.value_legacy_offset,
                            );
                            self.emit_load_string_value_ptr(
                                R9,
                                RDX,
                                layout.value_compact_offset,
                                layout.value_legacy_offset,
                            );
                            // Null value array on either side → deopt.
                            self.buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8,R8
                            bail.push(self.emit_jcc_rel32_patch(0x84));
                            self.buf.emit(&[0x4D, 0x85, 0xC9]); // TEST R9,R9
                            bail.push(self.emit_jcc_rel32_patch(0x84));

                            // coder mismatch → deopt.
                            // MOV ECX,[RAX+coder] ; CMP ECX,[RDX+coder]
                            self.emit_load_string_i32_field(
                                RCX,
                                RAX,
                                layout.coder_compact_offset,
                                layout.coder_compact_is_byte,
                                layout.coder_legacy_offset,
                            );
                            // other.coder may come from a legacy-laid-out `other`
                            // independently of `this` -- load it through the
                            // same compact/legacy-aware helper (R11 is free
                            // here) instead of a raw CMP-with-memory-operand,
                            // then compare register-to-register.
                            self.emit_load_string_i32_field(
                                R11,
                                RDX,
                                layout.coder_compact_offset,
                                layout.coder_compact_is_byte,
                                layout.coder_legacy_offset,
                            );
                            self.emit_alu_r32_r32(0x39, RCX, R11); // CMP ECX,R11D
                            bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                            // value-array length mismatch → result 0.
                            // MOV ECX,[R8+12] ; CMP ECX,[R9+12]
                            self.buf
                                // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                .emit(&[0x41, 0x8B, 0x48, ARRAY_LENGTH_OFFSET as u8]);
                            self.buf
                                // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                .emit(&[0x41, 0x3B, 0x49, ARRAY_LENGTH_OFFSET as u8]);
                            let len_diff = self.emit_jcc_rel32_patch(0x85); // JNE

                            // Byte compare of ECX bytes from
                            // [R8+HEADER] vs [R9+HEADER] via REP CMPSB.
                            // RSI/RDI are callee-saved + may hold locals —
                            // bracket with PUSH/POP (no CALL in between).
                            // PUSH RSI ; PUSH RDI
                            self.buf.emit(&[0x56, 0x57]);
                            // RSI = R8 + HEADER ; RDI = R9 + HEADER
                            // LEA RSI,[R8+40]
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            self.buf.emit(&[0x49, 0x8D, 0x70, HEADER_SIZE as u8]);
                            // LEA RDI,[R9+40]
                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                            self.buf.emit(&[0x49, 0x8D, 0x79, HEADER_SIZE as u8]);
                            // RCX = length (ECX already holds it,
                            // zero-extended into RCX).
                            // REPE CMPSB  (F3 A6)
                            self.buf.emit(&[0xF3, 0xA6]);
                            // POP RDI ; POP RSI
                            self.buf.emit(&[0x5F, 0x5E]);
                            // SETE AL ; MOVZX EAX,AL → 1 iff all bytes equal
                            // (REPE stops on the first mismatch with ZF clear).
                            self.buf.emit(&[0x0F, 0x94, 0xC0]);
                            self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                            let eq_done = self.emit_jmp_rel32_patch();

                            // result 0 path (other null / length mismatch).
                            self.patch_rel32_to_here(other_null);
                            self.patch_rel32_to_here(len_diff);
                            self.emit_xor_reg_self(RAX);
                            let false_done = self.emit_jmp_rel32_patch();

                            // result 1 path (same reference).
                            self.patch_rel32_to_here(same_ref);
                            // MOV EAX,1
                            self.buf.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00]);

                            // join.
                            self.patch_rel32_to_here(eq_done);
                            self.patch_rel32_to_here(false_done);
                            self.push_from_rax();

                            for p in bail {
                                self.deopt_stubs.push((p, pc, 6));
                            }
                            intrinsic_handled = true;
                        }

                        // --- compareTo(Ljava/lang/String;)I ---------------
                        // Lexicographic decoded-char compare. Each side is
                        // decoded through ITS OWN `coder` byte, so every
                        // LATIN1/UTF16 combination (including mixed) is
                        // handled inline — no coder-mismatch deopt. The deopt
                        // stub is reached only for a null receiver, a null
                        // String argument (native throws NPE on the re-run),
                        // or a null backing `value` array. After those checks
                        // the result is fully determined: the unsigned-char
                        // difference at the first mismatch, else len1-len2.
                        // Identical to `native_string_compare_to`.
                        if self.string_layout.is_some()
                            && callee_entry == crate::JitIntrinsic::StringCompareTo.as_entry()
                        {
                            let layout = self.string_layout.unwrap();
                            self.flush_scratch_registers();
                            // Step 6: snapshot (this, other) before pops.
                            if crate::deopt_real_enabled() {
                                self.snapshot_pre_intrinsic_call(
                                    pc,
                                    crate::deopt::DeoptReason::ReceiverTypeChanged,
                                );
                            }
                            let mut bail: Vec<usize> = Vec::new();

                            // Operand stack (deepest first): this, other.
                            let other_slot = self.pop_stack();
                            let this_slot = self.pop_stack();

                            // --- deopt checks + field reads happen BEFORE
                            // any PUSH, into CALLER-saved registers only.
                            // Two reasons: (1) the deopt stub's epilogue
                            // assumes RSP is at the post-prologue value, so
                            // the stack must stay balanced on every path that
                            // can reach a bail; (2) a `this`/`other` operand
                            // slot may itself be a callee-saved register that
                            // the loop is about to overwrite — reading it
                            // before the loop's registers are clobbered (and
                            // before the PUSH, while it is still the live
                            // local) is the only sound order.
                            //
                            // RAX = this; null receiver → deopt.
                            self.load_slot_to_reg(RAX, this_slot);
                            self.emit_test_r64_r64(RAX);
                            bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                                        // RDX = other; null argument → deopt.
                            self.load_slot_to_reg(RDX, other_slot);
                            self.emit_test_r64_r64(RDX);
                            bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                                        // R8 = this.value, R9 = other.value; null → deopt.
                            self.emit_load_string_value_ptr(
                                R8,
                                RAX,
                                layout.value_compact_offset,
                                layout.value_legacy_offset,
                            );
                            self.buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8,R8
                            bail.push(self.emit_jcc_rel32_patch(0x84));
                            self.emit_load_string_value_ptr(
                                R9,
                                RDX,
                                layout.value_compact_offset,
                                layout.value_legacy_offset,
                            );
                            self.buf.emit(&[0x4D, 0x85, 0xC9]); // TEST R9,R9
                            bail.push(self.emit_jcc_rel32_patch(0x84));
                            // R10 = this.coder, R11 = other.coder.
                            self.emit_load_string_i32_field(
                                R10,
                                RAX,
                                layout.coder_compact_offset,
                                layout.coder_compact_is_byte,
                                layout.coder_legacy_offset,
                            );
                            self.emit_load_string_i32_field(
                                R11,
                                RDX,
                                layout.coder_compact_offset,
                                layout.coder_compact_is_byte,
                                layout.coder_legacy_offset,
                            );

                            // --- past every deopt edge: save the callee-
                            // saved registers the loop uses, then park the
                            // pre-computed caller-saved values into them.
                            // PUSH RBX,RSI,RDI,R12,R13,R14,R15.
                            self.buf.emit(&[0x53, 0x56, 0x57]);
                            self.buf.emit(&[0x41, 0x54, 0x41, 0x55]);
                            self.buf.emit(&[0x41, 0x56, 0x41, 0x57]);
                            // RSI=this.value, RDI=other.value, R12=coder1,
                            // R13=coder2 (moves out of the caller-saved regs;
                            // the original callee-saved values are safely on
                            // the machine stack).
                            self.emit_mov_r64_r64(RSI, R8);
                            self.emit_mov_r64_r64(RDI, R9);
                            self.emit_mov_r64_r64(R12, R10);
                            self.emit_mov_r64_r64(R13, R11);
                            // len1 = this.value.length >> coder1  → R14D.
                            // Cast: fixed struct/layout offset to i32 instruction displacement
                            self.emit_mov_r32_mem_disp32(RAX, RSI, ARRAY_LENGTH_OFFSET as i32);
                            self.emit_alu_r32_r32(0x89, RCX, R12); // MOV ECX,R12D
                            self.buf.emit(&[0xD3, 0xE8]); // SHR EAX,CL
                            self.emit_alu_r32_r32(0x89, R14, RAX); // MOV R14D,EAX
                                                                   // len2 = other.value.length >> coder2 → R15D.
                                                                   // Cast: fixed struct/layout offset to i32 instruction displacement
                            self.emit_mov_r32_mem_disp32(RAX, RDI, ARRAY_LENGTH_OFFSET as i32);
                            self.emit_alu_r32_r32(0x89, RCX, R13); // MOV ECX,R13D
                            self.buf.emit(&[0xD3, 0xE8]); // SHR EAX,CL
                            self.emit_alu_r32_r32(0x89, R15, RAX); // MOV R15D,EAX
                                                                   // min_len = min(len1,len2) → EBX.
                            self.emit_alu_r32_r32(0x89, RBX, R14); // MOV EBX,R14D
                            self.emit_alu_r32_r32(0x39, RBX, R15); // CMP EBX,R15D
                                                                   // CMOVG EBX,R15D (EBX > R15D ⇒ keep R15D as min).
                            self.buf.emit(&[0x41, 0x0F, 0x4F, 0xDF]);

                            // i = 0 (R8D).
                            self.buf.emit(&[0x45, 0x31, 0xC0]); // XOR R8D,R8D
                            let loop_top = self.buf.pos();
                            // CMP R8D,EBX ; JGE loop_done (i >= min_len).
                            self.emit_alu_r32_r32(0x39, R8, RBX); // CMP R8D,EBX
                            let loop_done = self.emit_jcc_rel32_patch(0x8D);
                            // char_a = this[i]  → R9D ; char_b = other[i] → R10D.
                            self.emit_string_decode_char(R9, RSI, R8, R12);
                            self.emit_string_decode_char(R10, RDI, R8, R13);
                            // diff = char_a - char_b ; JNZ mismatch.
                            self.emit_alu_r32_r32(0x29, R9, R10); // SUB R9D,R10D
                            let mismatch = self.emit_jcc_rel32_patch(0x85);
                            // INC R8D ; JMP loop_top.
                            self.buf.emit(&[0x41, 0xFF, 0xC0]);
                            let back = self.emit_jmp_rel32_patch();
                            // Cast: value to i32 (encoding immediate/displacement)
                            let rel = loop_top as i32 - (back as i32 + 4);
                            self.buf.try_patch_i32(back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                                                                    // loop_done: result = len1 - len2.
                            self.patch_rel32_to_here(loop_done);
                            self.emit_alu_r32_r32(0x89, RAX, R14); // MOV EAX,R14D
                            self.emit_alu_r32_r32(0x29, RAX, R15); // SUB EAX,R15D
                            let cmp_join = self.emit_jmp_rel32_patch();
                            // mismatch: result = diff (R9D).
                            self.patch_rel32_to_here(mismatch);
                            self.emit_alu_r32_r32(0x89, RAX, R9); // MOV EAX,R9D
                            self.patch_rel32_to_here(cmp_join);
                            // Sign-extend the int result for the push ABI.
                            self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
                                                                // POP R15,R14,R13,R12,RDI,RSI,RBX.
                            self.buf.emit(&[0x41, 0x5F, 0x41, 0x5E]);
                            self.buf.emit(&[0x41, 0x5D, 0x41, 0x5C]);
                            self.buf.emit(&[0x5F, 0x5E, 0x5B]);
                            self.push_from_rax();

                            for p in bail {
                                self.deopt_stubs.push((p, pc, 6));
                            }
                            intrinsic_handled = true;
                        }

                        // --- indexOf(I)I ----------------------------------
                        // LIVE again as of E27-1 N2b (2026-08-18), for constant
                        // BMP needles only. The `direct.filter` far above is
                        // what enforces that; by the time control reaches here
                        // the needle is known to be a compile-time constant in
                        // `0..=0xFFFF`.
                        //
                        // Scan the receiver for the first code unit equal to
                        // `(ch & 0xFFFF)`, from index 0. This comment used to
                        // call that "bit-identical to `native_string_index_of`,
                        // which likewise masks the argument". BOTH HALVES WERE
                        // FALSE — the JDK gates on `Character.isValidCodePoint`
                        // BEFORE any narrowing and matches a supplementary `ch`
                        // as a surrogate PAIR, and the native side stopped
                        // masking at E18-1, which put the rule in one place
                        // (`lang_string.rs`'s `code_point_needle`). It is
                        // spelled out rather than deleted because E27-1's
                        // finding is that code reading as a working
                        // implementation is how four copies of this rule
                        // survived.
                        //
                        // What makes the scan correct now is the RANGE, not the
                        // mask: on `0..=0xFFFF` the JDK scans for exactly one
                        // code unit, lone surrogates included, so the mask is
                        // the identity and this loop is `code_point_needle`'s
                        // answer. It is left in place rather than folded into a
                        // baked immediate to keep this change a gate change and
                        // nothing else — the emitted bytes here are unchanged.
                        //
                        // The deopt stub is reached only for a null receiver or
                        // a null backing `value` array, both genuinely
                        // once-per-program. It is NOT reached for an
                        // out-of-range needle: such a site is never
                        // intrinsified in the first place. That distinction is
                        // the whole of N2b — see `prev_insn_int_const`.
                        //
                        // Uses only caller-saved registers, so no PUSH/POP is
                        // needed.
                        if self.string_layout.is_some()
                            && callee_entry == crate::JitIntrinsic::StringIndexOfChar.as_entry()
                        {
                            let layout = self.string_layout.unwrap();
                            self.flush_scratch_registers();
                            // Step 6: snapshot (this, ch) before pops.
                            if crate::deopt_real_enabled() {
                                self.snapshot_pre_intrinsic_call(
                                    pc,
                                    crate::deopt::DeoptReason::ReceiverTypeChanged,
                                );
                            }
                            let mut bail: Vec<usize> = Vec::new();

                            // Operand stack (deepest first): this, ch.
                            let ch_slot = self.pop_stack();
                            let this_slot = self.pop_stack();

                            // RAX = this; null receiver → deopt.
                            self.load_slot_to_reg(RAX, this_slot);
                            self.emit_test_r64_r64(RAX);
                            bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                                        // R8 = this.value; null → deopt.
                            self.emit_load_string_value_ptr(
                                R8,
                                RAX,
                                layout.value_compact_offset,
                                layout.value_legacy_offset,
                            );
                            self.buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8,R8
                            bail.push(self.emit_jcc_rel32_patch(0x84));
                            // R10 = this.coder.
                            self.emit_load_string_i32_field(
                                R10,
                                RAX,
                                layout.coder_compact_offset,
                                layout.coder_compact_is_byte,
                                layout.coder_legacy_offset,
                            );
                            // R9D = needle = ch & 0xFFFF.
                            self.load_slot_to_reg(R9, ch_slot);
                            // AND R9D, 0xFFFF  (REX.B + 81 /4 id).
                            self.buf.emit(&[0x41, 0x81, 0xE1]);
                            self.buf.emit(&0xFFFFu32.to_le_bytes());
                            // len = this.value.length >> coder → R11D.
                            // Cast: fixed struct/layout offset to i32 instruction displacement
                            self.emit_mov_r32_mem_disp32(RAX, R8, ARRAY_LENGTH_OFFSET as i32);
                            self.emit_alu_r32_r32(0x89, RCX, R10); // MOV ECX,R10D
                            self.buf.emit(&[0xD3, 0xE8]); // SHR EAX,CL
                            self.emit_alu_r32_r32(0x89, R11, RAX); // MOV R11D,EAX
                                                                   // i = 0 (EDX).
                            self.emit_xor_reg_self(RDX);
                            let loop_top = self.buf.pos();
                            // CMP EDX,R11D ; JGE not_found.
                            self.emit_alu_r32_r32(0x39, RDX, R11);
                            let not_found = self.emit_jcc_rel32_patch(0x8D);
                            // c = this[i] → ECX ; CMP ECX,R9D ; JE found.
                            self.emit_string_decode_char(RCX, R8, RDX, R10);
                            self.emit_alu_r32_r32(0x39, RCX, R9); // CMP ECX,R9D
                            let found = self.emit_jcc_rel32_patch(0x84);
                            // INC EDX ; JMP loop_top.
                            self.buf.emit(&[0xFF, 0xC2]);
                            let back = self.emit_jmp_rel32_patch();
                            // Cast: value to i32 (encoding immediate/displacement)
                            let rel = loop_top as i32 - (back as i32 + 4);
                            self.buf.try_patch_i32(back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                                                                    // found: result = i.
                            self.patch_rel32_to_here(found);
                            self.emit_alu_r32_r32(0x89, RAX, RDX); // MOV EAX,EDX
                            let join = self.emit_jmp_rel32_patch();
                            // not_found: result = -1.
                            self.patch_rel32_to_here(not_found);
                            self.buf.emit(&[0xB8]);
                            self.buf.emit(&(-1i32).to_le_bytes());
                            self.patch_rel32_to_here(join);
                            self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
                            self.push_from_rax();

                            for p in bail {
                                self.deopt_stubs.push((p, pc, 6));
                            }
                            intrinsic_handled = true;
                        }

                        // --- indexOf(Ljava/lang/String;)I -----------------
                        // Naive O(n*m) substring search from index 0; an
                        // empty needle returns 0. Each haystack/needle char is
                        // decoded through its own `coder`, so all coder combos
                        // are handled inline. Bit-identical to
                        // `native_string_index_of_str`. The deopt stub is
                        // reached only for a null receiver or a null backing
                        // `value` array on either side; a null String argument
                        // also deopts — the native re-run then returns -1,
                        // which is the same answer.
                        if self.string_layout.is_some()
                            && callee_entry == crate::JitIntrinsic::StringIndexOfStr.as_entry()
                        {
                            let layout = self.string_layout.unwrap();
                            self.flush_scratch_registers();
                            // Step 6: snapshot (this, needle) before pops.
                            if crate::deopt_real_enabled() {
                                self.snapshot_pre_intrinsic_call(
                                    pc,
                                    crate::deopt::DeoptReason::ReceiverTypeChanged,
                                );
                            }
                            let mut bail: Vec<usize> = Vec::new();

                            // Operand stack (deepest first): this, needle.
                            let needle_slot = self.pop_stack();
                            let this_slot = self.pop_stack();

                            // --- deopt checks + field reads BEFORE any PUSH,
                            // into CALLER-saved registers only (see the
                            // compareTo block for the rationale: balanced
                            // stack on deopt edges, and an operand slot may
                            // itself be a callee-saved register the loop is
                            // about to clobber).
                            self.load_slot_to_reg(RAX, this_slot);
                            self.emit_test_r64_r64(RAX);
                            bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                            self.load_slot_to_reg(RDX, needle_slot);
                            self.emit_test_r64_r64(RDX);
                            bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                            self.emit_load_string_value_ptr(
                                R8,
                                RAX,
                                layout.value_compact_offset,
                                layout.value_legacy_offset,
                            );
                            self.buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8,R8
                            bail.push(self.emit_jcc_rel32_patch(0x84));
                            self.emit_load_string_value_ptr(
                                R9,
                                RDX,
                                layout.value_compact_offset,
                                layout.value_legacy_offset,
                            );
                            self.buf.emit(&[0x4D, 0x85, 0xC9]); // TEST R9,R9
                            bail.push(self.emit_jcc_rel32_patch(0x84));
                            // R10 = haystack coder, R11 = needle coder.
                            self.emit_load_string_i32_field(
                                R10,
                                RAX,
                                layout.coder_compact_offset,
                                layout.coder_compact_is_byte,
                                layout.coder_legacy_offset,
                            );
                            self.emit_load_string_i32_field(
                                R11,
                                RDX,
                                layout.coder_compact_offset,
                                layout.coder_compact_is_byte,
                                layout.coder_legacy_offset,
                            );

                            // PUSH RBX,RSI,RDI,R12,R13,R14,R15.
                            self.buf.emit(&[0x53, 0x56, 0x57]);
                            self.buf.emit(&[0x41, 0x54, 0x41, 0x55]);
                            self.buf.emit(&[0x41, 0x56, 0x41, 0x57]);
                            // RSI=haystack value, RDI=needle value,
                            // R12=haystack coder, R13=needle coder.
                            self.emit_mov_r64_r64(RSI, R8);
                            self.emit_mov_r64_r64(RDI, R9);
                            self.emit_mov_r64_r64(R12, R10);
                            self.emit_mov_r64_r64(R13, R11);
                            // hlen → R14D, nlen → R15D.
                            // Cast: fixed struct/layout offset to i32 instruction displacement
                            self.emit_mov_r32_mem_disp32(RAX, RSI, ARRAY_LENGTH_OFFSET as i32);
                            self.emit_alu_r32_r32(0x89, RCX, R12); // MOV ECX,R12D
                            self.buf.emit(&[0xD3, 0xE8]); // SHR EAX,CL
                            self.emit_alu_r32_r32(0x89, R14, RAX); // MOV R14D,EAX
                                                                   // Cast: fixed struct/layout offset to i32 instruction displacement
                            self.emit_mov_r32_mem_disp32(RAX, RDI, ARRAY_LENGTH_OFFSET as i32);
                            self.emit_alu_r32_r32(0x89, RCX, R13); // MOV ECX,R13D
                            self.buf.emit(&[0xD3, 0xE8]); // SHR EAX,CL
                            self.emit_alu_r32_r32(0x89, R15, RAX); // MOV R15D,EAX

                            // empty needle (nlen == 0) → result 0.
                            self.buf.emit(&[0x45, 0x85, 0xFF]); // TEST R15D,R15D
                            let needle_empty = self.emit_jcc_rel32_patch(0x84); // JZ
                                                                                // nlen > hlen → not_found.
                            self.emit_alu_r32_r32(0x39, R15, R14); // CMP R15D,R14D
                            let too_long = self.emit_jcc_rel32_patch(0x8F); // JG
                                                                            // max_start = hlen - nlen → EBX (inclusive bound).
                            self.emit_alu_r32_r32(0x89, RBX, R14); // MOV EBX,R14D
                            self.emit_alu_r32_r32(0x29, RBX, R15); // SUB EBX,R15D

                            // outer: i = 0 (R8D).
                            self.buf.emit(&[0x45, 0x31, 0xC0]); // XOR R8D,R8D
                            let outer_top = self.buf.pos();
                            // CMP R8D,EBX ; JG not_found (i > max_start).
                            self.emit_alu_r32_r32(0x39, R8, RBX);
                            let outer_done = self.emit_jcc_rel32_patch(0x8F);
                            // inner: j = 0 (R9D).
                            self.buf.emit(&[0x45, 0x31, 0xC9]); // XOR R9D,R9D
                            let inner_top = self.buf.pos();
                            // CMP R9D,R15D ; JGE match_found (j >= nlen).
                            self.emit_alu_r32_r32(0x39, R9, R15);
                            let match_found = self.emit_jcc_rel32_patch(0x8D);
                            // h = haystack[i+j]: R10D = i+j, decode → R11D.
                            self.emit_alu_r32_r32(0x89, R10, R8); // MOV R10D,R8D
                            self.emit_alu_r32_r32(0x01, R10, R9); // ADD R10D,R9D
                            self.emit_string_decode_char(R11, RSI, R10, R12);
                            // n = needle[j]: decode → R10D.
                            self.emit_string_decode_char(R10, RDI, R9, R13);
                            // CMP R11D,R10D ; JNE inner_break.
                            self.emit_alu_r32_r32(0x39, R11, R10);
                            let inner_break = self.emit_jcc_rel32_patch(0x85);
                            // INC R9D ; JMP inner_top.
                            self.buf.emit(&[0x41, 0xFF, 0xC1]);
                            let inner_back = self.emit_jmp_rel32_patch();
                            // Cast: value to i32 (encoding immediate/displacement)
                            let rel = inner_top as i32 - (inner_back as i32 + 4);
                            self.buf.try_patch_i32(inner_back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                                                                          // inner_break: INC R8D ; JMP outer_top.
                            self.patch_rel32_to_here(inner_break);
                            self.buf.emit(&[0x41, 0xFF, 0xC0]); // INC R8D
                            let outer_back = self.emit_jmp_rel32_patch();
                            // Cast: value to i32 (encoding immediate/displacement)
                            let rel = outer_top as i32 - (outer_back as i32 + 4);
                            self.buf.try_patch_i32(outer_back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                                                                          // match_found: result = i (R8D).
                            self.patch_rel32_to_here(match_found);
                            self.emit_alu_r32_r32(0x89, RAX, R8); // MOV EAX,R8D
                            let join = self.emit_jmp_rel32_patch();
                            // not_found (nlen>hlen or outer exhausted): -1.
                            self.patch_rel32_to_here(too_long);
                            self.patch_rel32_to_here(outer_done);
                            self.buf.emit(&[0xB8]);
                            self.buf.emit(&(-1i32).to_le_bytes());
                            let join2 = self.emit_jmp_rel32_patch();
                            // needle_empty: result = 0.
                            self.patch_rel32_to_here(needle_empty);
                            self.emit_xor_reg_self(RAX);
                            // join.
                            self.patch_rel32_to_here(join);
                            self.patch_rel32_to_here(join2);
                            self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
                                                                // POP R15,R14,R13,R12,RDI,RSI,RBX.
                            self.buf.emit(&[0x41, 0x5F, 0x41, 0x5E]);
                            self.buf.emit(&[0x41, 0x5D, 0x41, 0x5C]);
                            self.buf.emit(&[0x5F, 0x5E, 0x5B]);
                            self.push_from_rax();

                            for p in bail {
                                self.deopt_stubs.push((p, pc, 6));
                            }
                            intrinsic_handled = true;
                        }
                        // ===== INTRINSIC REGION END: STRING_SEARCH =====

                        // ===== INTRINSIC REGION BEGIN: CRC32 =====
                        // java.util.zip.CRC32 / CRC32C `update` call-site
                        // intrinsics (Phase 4c). Both classes hold a single
                        // `private int crc` at instance field slot 0 — the
                        // running (uncomplemented) CRC state — see
                        // gaps/crc_layout_contract.md. The four
                        // sentinels handled here:
                        //
                        //   Crc32cUpdateByte  : CRC32C.update(I)V
                        //   Crc32cUpdateBytes : CRC32C.update([BII)V
                        //   Crc32UpdateByte   : CRC32.update(I)V
                        //   Crc32UpdateBytes  : CRC32.update([BII)V
                        //
                        // Every variant:
                        //   1. pops the operand stack (receiver is the
                        //      deepest operand; `callee_params + 1` total),
                        //   2. emits a receiver class-id guard — `CMP
                        //      DWORD [recv+0], guard_class_id` — and deopts
                        //      to normal dispatch on a null receiver or a
                        //      class-id mismatch (a subclass could override
                        //      `update`),
                        //   3. loads the running crc from field cell slot 0
                        //      (`HEADER_SIZE + FIELD_CELL_PAYLOAD32_OFFSET`),
                        //   4. folds the input byte(s) into it,
                        //   5. writes the result back to the same cell
                        //      (Int tag word + payload word).
                        //
                        // CRC32C folds with the hardware `CRC32` instruction
                        // (it computes exactly the Castagnoli polynomial);
                        // CRC32 folds with an inline reflected-CRC bit loop
                        // (IEEE poly 0xEDB88320 — the hardware instruction is
                        // the wrong polynomial). Neither path emits a `CALL`.
                        //
                        // `update([BII)V` preserves null-array NPE and
                        // out-of-bounds AIOOBE by deopting on a null array or
                        // a range outside `[0, array.length]` — the
                        // interpreter then re-runs `update` via the native
                        // override, which raises the exact exception.
                        {
                            let crc32c_byte = crate::JitIntrinsic::Crc32cUpdateByte.as_entry();
                            let crc32c_bytes = crate::JitIntrinsic::Crc32cUpdateBytes.as_entry();
                            let crc32_byte = crate::JitIntrinsic::Crc32UpdateByte.as_entry();
                            let crc32_bytes = crate::JitIntrinsic::Crc32UpdateBytes.as_entry();
                            let is_crc32c =
                                callee_entry == crc32c_byte || callee_entry == crc32c_bytes;
                            let is_crc32_ieee =
                                callee_entry == crc32_byte || callee_entry == crc32_bytes;
                            let is_byte_form =
                                callee_entry == crc32c_byte || callee_entry == crc32_byte;
                            let is_bytes_form =
                                callee_entry == crc32c_bytes || callee_entry == crc32_bytes;

                            if is_crc32c || is_crc32_ieee {
                                // The matcher only registers a CRC32 family
                                // intrinsic with a resolved class id (it
                                // skips registration when guard_class_id
                                // would be 0), so this is always non-zero
                                // here; assert the invariant defensively.
                                debug_assert!(
                                    guard_class_id != 0,
                                    "CRC32 intrinsic reached codegen without a guard class id",
                                );

                                // Reflected polynomial for the IEEE bit loop.
                                // Castagnoli uses the hardware instruction,
                                // so this constant is only consumed when
                                // `is_crc32_ieee`.
                                const CRC32_IEEE_REVERSED_POLY: u32 = 0xEDB8_8320;
                                // Instance field cell for the `int crc` at
                                // slot 0: HEADER_SIZE + 0*SLOT_SIZE, then the
                                // tag word at +0 and the 32-bit payload at
                                // +FIELD_CELL_PAYLOAD32_OFFSET (see the
                                // inline-getfield codegen for opcode 0xb4).
                                // Cast: fixed struct/layout offset to i32 instruction displacement
                                let cell_off = HEADER_SIZE as i32;
                                // Cast: fixed struct/layout offset to i32 instruction displacement
                                let tag_off = cell_off + FIELD_CELL_TAG_OFFSET as i32;
                                // Cast: fixed struct/layout offset to i32 instruction displacement
                                let pay_off = cell_off + FIELD_CELL_PAYLOAD32_OFFSET as i32;

                                self.flush_scratch_registers();
                                // Step 6: snapshot (receiver [, arr, off, len])
                                // before pops so null/bounds guard bails resume
                                // at the invokevirtual CRC32.update bci.
                                if crate::deopt_real_enabled() {
                                    self.snapshot_pre_intrinsic_call(
                                        pc,
                                        crate::deopt::DeoptReason::BoundsCheck,
                                    );
                                }

                                // --- pop operands (deepest = receiver) ---
                                // update(I)V    : [receiver, b]
                                // update([BII)V : [receiver, arr, off, len]
                                let (b_or_len_slot, off_slot, arr_slot, recv_slot);
                                if is_byte_form {
                                    let b = self.pop_stack();
                                    let r = self.pop_stack();
                                    b_or_len_slot = b;
                                    off_slot = b; // unused
                                    arr_slot = b; // unused
                                    recv_slot = r;
                                } else {
                                    let len = self.pop_stack();
                                    let off = self.pop_stack();
                                    let arr = self.pop_stack();
                                    let r = self.pop_stack();
                                    b_or_len_slot = len;
                                    off_slot = off;
                                    arr_slot = arr;
                                    recv_slot = r;
                                }

                                // Pin operands into owned frame scratch slots
                                // below `next_spill_offset`. The call site
                                // had >= (callee_params+1) operand-stack
                                // entries, so these offsets are in-frame. The
                                // intrinsic pushes nothing (void return), so
                                // the next bytecode re-allocates spill slots
                                // from the same base.
                                let scratch_slots = if is_byte_form { 2 } else { 4 };
                                if !self.spill_range_fits(self.next_spill_offset, scratch_slots) {
                                    return false;
                                }
                                let s_recv = self.next_spill_offset;
                                let s_a = self.next_spill_offset + 8;
                                let s_b = self.next_spill_offset + 16;
                                let s_c = self.next_spill_offset + 24;
                                self.load_slot_to_reg(RAX, recv_slot);
                                self.emit_store_local(s_recv, RAX);
                                if is_byte_form {
                                    self.load_slot_to_reg(RAX, b_or_len_slot);
                                    self.emit_store_local(s_a, RAX);
                                } else {
                                    self.load_slot_to_reg(RAX, arr_slot);
                                    self.emit_store_local(s_a, RAX);
                                    self.load_slot_to_reg(RAX, off_slot);
                                    self.emit_store_local(s_b, RAX);
                                    self.load_slot_to_reg(RAX, b_or_len_slot);
                                    self.emit_store_local(s_c, RAX);
                                }

                                // Every "bail to interpreter" edge is wired
                                // to a shared uncommon-trap deopt stub
                                // (reason 2). The interpreter re-runs the
                                // method and dispatches `update` normally —
                                // preserving NPE / AIOOBE and any overriding
                                // subclass `update` exactly.
                                let mut bail_patches: Vec<usize> = Vec::new();

                                // --- receiver class-id guard ---
                                // RAX = receiver. A null receiver bails
                                // (the interpreter NPEs on the virtual
                                // dispatch). Then CMP the class id at
                                // ObjectHeader+0 against the declared class.
                                self.emit_load_local(RAX, s_recv);
                                self.emit_test_r64_r64(RAX);
                                bail_patches.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                                                    // CMP DWORD [RAX + 0], guard_class_id
                                                                                    //   81 /7 ib? — use the imm32 form: 81 /7.
                                                                                    //   ModRM 0x78 = mod00 reg=7(/7=CMP) rm=RAX.
                                self.buf.emit(&[0x81, 0x78, 0x00]);
                                self.buf.emit(&guard_class_id.to_le_bytes());
                                bail_patches.push(self.emit_jcc_rel32_patch(0x85)); // JNE

                                // --- load CRC state → ECX ---
                                // MOV ECX, DWORD [RAX + pay_off]. The slot
                                // holds a `Value::Int`. CRC32C stores the
                                // running (complemented) state, while real
                                // JDK CRC32 stores the public value. The IEEE
                                // folding helper consumes the former, so the
                                // CRC32 path complements on either side.
                                self.emit_mov_r32_mem_disp32(RCX, RAX, pay_off);
                                if is_crc32_ieee {
                                    // NOT ECX — public CRC32 value -> running
                                    // reflected-CRC state before the fold.
                                    self.buf.emit(&[0xF7, 0xD1]);
                                }

                                if is_byte_form {
                                    // --- update(I)V: fold one byte ---
                                    // EDX = arg byte & 0xFF.
                                    self.emit_load_local(RDX, s_a);
                                    // MOVZX EDX, DL  (0F B6 D2) — low 8 bits.
                                    self.buf.emit(&[0x0F, 0xB6, 0xD2]);
                                    if is_crc32c {
                                        // CRC32 ECX, DL — hardware Castagnoli
                                        // fold of one byte. F2 0F 38 F0 /r,
                                        // ModRM 0xCA = reg=ECX rm=EDX(=DL).
                                        self.buf.emit(&[0xF2, 0x0F, 0x38, 0xF0, 0xCA]);
                                    } else {
                                        self.emit_crc32_ieee_fold_byte(CRC32_IEEE_REVERSED_POLY);
                                    }
                                } else {
                                    // --- update([BII)V: fold a range ---
                                    // Guards (all bail to the deopt stub,
                                    // matching the native override's NPE /
                                    // AIOOBE semantics):
                                    //   arr != null
                                    //   off >= 0, len >= 0
                                    //   off + len <= arr.length
                                    //
                                    // Register file held live across the
                                    // guards into the fold loop:
                                    //   R8  = array base pointer
                                    //   R9  = current index (starts at off)
                                    //   R11 = end index = off + len
                                    //   RCX = running crc (already loaded)
                                    // RDX/RAX are loop-body scratch (the
                                    // IEEE helper consumes EDX and clobbers
                                    // EAX), so they must NOT carry the index.
                                    //
                                    // R8 = array ptr; null-array → bail.
                                    self.emit_load_local(R8, s_a);
                                    self.buf.emit(&[0x4D, 0x85, 0xC0]); // TEST R8,R8
                                    bail_patches.push(self.emit_jcc_rel32_patch(0x84)); // JZ → null array
                                                                                        // RDX = off, sign-extended to 64-bit so
                                                                                        // the range arithmetic cannot overflow.
                                    self.emit_load_local(RDX, s_b);
                                    self.buf.emit(&[0x48, 0x63, 0xD2]); // MOVSXD RDX,EDX
                                                                        // off < 0 ? TEST RDX,RDX; JS bail.
                                    self.emit_test_r64_r64(RDX);
                                    bail_patches.push(self.emit_jcc_rel32_patch(0x88)); // JS
                                                                                        // R11 = len, sign-extended.
                                    self.emit_load_local(R11, s_c);
                                    self.buf.emit(&[0x4D, 0x63, 0xDB]); // MOVSXD R11,R11D
                                                                        // len < 0 ? TEST R11,R11; JS bail.
                                    self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11,R11
                                    bail_patches.push(self.emit_jcc_rel32_patch(0x88)); // JS
                                                                                        // R11 = off + len  (the end index).
                                    self.buf.emit(&[0x49, 0x01, 0xD3]); // ADD R11,RDX
                                                                        // RAX = arr.length (zero-extended 32-bit
                                                                        // load → non-negative 64-bit value).
                                    self.buf
                                        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                        .emit(&[0x41, 0x8B, 0x40, ARRAY_LENGTH_OFFSET as u8]); // MOV EAX,[R8+ARRAY_LENGTH_OFFSET]
                                                                                               // off + len > arr.length ? CMP R11,RAX;
                                                                                               // JG bail (signed >).
                                    self.buf.emit(&[0x49, 0x39, 0xC3]); // CMP R11,RAX
                                    bail_patches.push(self.emit_jcc_rel32_patch(0x8F)); // JG
                                                                                        // R9 = current index = off (RDX).
                                    self.buf.emit(&[0x49, 0x89, 0xD1]); // MOV R9,RDX

                                    // --- fold loop ---
                                    // .loop: CMP R9,R11 ; JGE .done
                                    let loop_label = self.buf.pos();
                                    self.buf.emit(&[0x4D, 0x39, 0xD9]); // CMP R9,R11
                                    let done_patch = self.emit_jcc_rel32_patch(0x8D); // JGE
                                    if is_crc32c {
                                        // EAX = byte = arr[R9].
                                        // MOVZX EAX, BYTE [R8 + R9 + HDR]
                                        //   43 0F B6 44 08 dd
                                        //   (REX.X for R9 index, REX.B for
                                        //    R8 base → 0x43; SIB scale=1).
                                        self.buf.emit(&[
                                            0x43,
                                            0x0F,
                                            0xB6,
                                            0x44,
                                            0x08,
                                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                            HEADER_SIZE as u8,
                                        ]);
                                        // CRC32 ECX, AL — hardware Castagnoli
                                        // fold. F2 0F 38 F0 /r, ModRM 0xC8 =
                                        // reg=ECX rm=EAX(=AL).
                                        self.buf.emit(&[0xF2, 0x0F, 0x38, 0xF0, 0xC8]);
                                    } else {
                                        // EDX = byte = arr[R9] — the IEEE
                                        // bit loop consumes the byte in EDX.
                                        // MOVZX EDX, BYTE [R8 + R9 + HDR]
                                        //   43 0F B6 54 08 dd
                                        self.buf.emit(&[
                                            0x43,
                                            0x0F,
                                            0xB6,
                                            0x54,
                                            0x08,
                                            // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
                                            HEADER_SIZE as u8,
                                        ]);
                                        self.emit_crc32_ieee_fold_byte(CRC32_IEEE_REVERSED_POLY);
                                    }
                                    // INC R9 ; JMP .loop
                                    self.buf.emit(&[0x49, 0xFF, 0xC1]); // INC R9
                                    self.buf.emit_byte(0xE9); // JMP rel32
                                    {
                                        let here = self.buf.pos();
                                        // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                                        let rel = (loop_label as i64) - (here as i64 + 4);
                                        // Truncation: i64 -> i32 (rel32 branch displacement, range-checked)
                                        self.buf.emit(&(rel as i32).to_le_bytes());
                                    }
                                    // .done:
                                    self.patch_rel32_to_here(done_patch);
                                }

                                if is_crc32_ieee {
                                    // NOT ECX — running reflected-CRC state
                                    // back to real JDK CRC32's public value.
                                    self.buf.emit(&[0xF7, 0xD1]);
                                }

                                // --- write CRC state back to slot 0 ---
                                // RAX = receiver again (reload — RAX was
                                // clobbered by the array-length load / loop).
                                self.emit_load_local(RAX, s_recv);
                                // Tag word := 0  (Value::Int discriminant —
                                // pinned by `field_cell_layout_matches_
                                // value_enum` in cratonvm-types). Keeps the
                                // cell a well-formed Int even if a prior
                                // write left a stale tag.
                                self.emit_mov_dword_mem_disp32_imm32(RAX, tag_off, 0);
                                // Payload word := ECX (the class-specific
                                // state representation described above).
                                // MOV DWORD [RAX + pay_off], ECX  (89 88 dd).
                                self.buf.emit_byte(0x89);
                                self.buf.emit_byte(0x88);
                                self.buf.emit(&pay_off.to_le_bytes());

                                // Wire every bail edge to the shared
                                // uncommon-trap stub (reason 2). Equal
                                // (bci, reason) pairs are coalesced by
                                // `emit_deopt_stubs`.
                                for patch in bail_patches {
                                    self.deopt_stubs.push((patch, pc, 2));
                                }
                                // `update` is void — nothing is pushed.
                                let _ = ret_type;
                                intrinsic_handled = true;
                            }
                        }
                        // ===== INTRINSIC REGION END: CRC32 =====

                        if !intrinsic_handled {
                            // value-stack-usize-underflow-nio-worker-panic fix:
                            // snapshot the pre-pop operand stack here too (see
                            // the matching fix + comment on the MIC/PIC helper
                            // dispatch path below) — a direct call's callee can
                            // still throw/deopt, and `emit_post_invoke_exception_check`
                            // would otherwise be the first (and only) snapshot
                            // for this bci, taken AFTER the receiver/args are
                            // popped, which underflows on a `Reinterpret` resume.
                            if crate::deopt_real_enabled() {
                                self.snapshot_pre_intrinsic_call(
                                    pc,
                                    crate::deopt::DeoptReason::ReceiverTypeChanged,
                                );
                            }
                            // Direct call: pop receiver + params, call compiled entry
                            // invokespecial has a receiver, so total args = callee_params + 1
                            let n = callee_params + 1; // receiver + params
                                                       // `pop_stack` rewinds `next_spill_offset` when it pops a
                                                       // top-of-stack `Frame` slot, but it still HANDS THE SLOT
                                                       // BACK, and every `arg_slots` entry stays live until
                                                       // `emit_stack_arg_setup` marshals it into the entry ABI far
                                                       // below. Anything that reserves spill space in between is
                                                       // therefore handed the argument slots themselves. Remember
                                                       // the pre-pop top so such a reservation can be placed above
                                                       // them. See
                                                       // fixed-suite-bugs/jit-direct-call-arg1-clobbered-by-arg0-FIXED.md.
                            let args_frame_top = self.next_spill_offset;
                            let (arg_slots, arg_oops) = self.pop_invoke_args(n);
                            // JVMS 6.5: a null `objectref` raises NPE AT THE INVOKE,
                            // before the callee's first instruction. A baked direct
                            // call jumps straight into the compiled callee, so the
                            // only thing that ever raised it here was the callee
                            // body faulting on its own — which it does only if it
                            // dereferences `this`. MEASURED on three private callees
                            // behind one 50 000-call warming loop (HotSpot raises NPE
                            // for all three):
                            //
                            //     return 3;          NPE interpreted, NO-THROW(3) jit
                            //     return this.x;     NPE both
                            //     return helper();   NPE interpreted, NO-THROW(5) jit
                            //
                            // `invokevirtual`/`invokeinterface` are correct only
                            // incidentally: their inline cache tests the receiver's
                            // class, and a null fails every guard. `invokespecial`
                            // is statically bound, has no guard, and reaches here.
                            // This is the mechanism behind the bogus `Cannot read
                            // field "interfaces" because "rd" is null` at
                            // Class.java:1217 that `vm/tests/
                            // null_receiver_cached_invoke.rs` was written for.
                            //
                            // The receiver is argument 0 of every invoke that
                            // reaches this arm — 0xb6/0xb7/0xb9 all have one, which
                            // is why `n` above is `callee_params + 1`.
                            if let Some(receiver) = arg_slots.first() {
                                self.load_slot_to_reg(RAX, *receiver);
                                self.emit_precise_null_check_field_store();
                            }
                            // A reference staged where no oop map can name it fails the
                            // safepoint closed. DEFERRED to just after the service-range
                            // reservation below, because whether that is true here is
                            // exactly what the reservation decides: when it succeeds it
                            // copies every argument into a contiguous frame range, and a
                            // frame range IS nameable. See `direct_call_arg_maps_enabled`.
                            // Spill cursor as the bytecode's operand stack sees it
                            // now that this invoke's arguments are popped. The
                            // return value belongs HERE, not wherever the
                            // service-argument reservation below leaves the cursor.
                            let post_pop_spill = self.next_spill_offset;

                            // Preserve Java arguments for the cold direct-callee
                            // exception-table service before call marshalling.
                            let service_args_base = info_ptr.and_then(|_| {
                                // See `reserve_direct_call_service_slots`: this range MUST
                                // sit above the argument slots `pop_stack` just handed
                                // back, or the copy below reverses the arguments into
                                // themselves and the callee gets arg0 in every slot.
                                let base = self.reserve_direct_call_service_slots(
                                    args_frame_top,
                                    &arg_slots,
                                )?;
                                for (i, slot) in arg_slots.iter().enumerate() {
                                    self.load_slot_to_reg(R11, *slot);
                                    let off = base + ((arg_slots.len() - 1 - i) as i32) * 8;
                                    self.emit_store_local(off, R11);
                                    // THE SAME CHANNEL THE DISPATCH SITE USES. Its args
                                    // buffer pushes each oop among the arguments to
                                    // `pending_staged_arg_oops`, so the safepoint map
                                    // NAMES it and `collect_live_oop_homes` publishes it
                                    // on the shadow stack. This copy is the same shape --
                                    // a contiguous frame range, written before the CALL,
                                    // still live after it (`emit_inline_callee_deopt_check`
                                    // reads it) -- and it named nothing.
                                    if direct_call_arg_maps_enabled() && arg_oops[i] {
                                        self.pending_staged_arg_oops.push(off);
                                    }
                                }
                                Some(base)
                            });
                            // Only an argument oop with NO named home fails the
                            // safepoint closed now. With the service range reserved
                            // every one of them has one.
                            if arg_oops.iter().any(|&o| o)
                                && (!direct_call_arg_maps_enabled() || service_args_base.is_none())
                            {
                                self.pending_staged_args_unmapped = true;
                            }
                            // Round-8 wave-3 HIGH fix: stack-arg setup for
                            // invokespecial/virtual direct calls whose
                            // receiver+params exceed ARG_REGS.
                            let total_sub = self.emit_stack_arg_setup(&arg_slots, callee_needs_ctx);
                            // Round-8 wave-3: defensive callee-saved spill
                            // before any GC-triggering CALL -- see the
                            // invokestatic site above for why an oop-clean frame
                            // can publish the safepoint id alone.
                            // `args_frame_resident` is what lets mode 2 admit a
                            // reference argument: the service range makes it
                            // frame-resident for the CONSERVATIVE walk. That is no
                            // longer the whole obligation -- naming the argument in
                            // the map means a moving cycle will rewrite it, and for
                            // that it must also be PUBLISHED, which only the real
                            // spill path emits. So a call that names its argument
                            // oops declines the elision and pays the spill again;
                            // `CRATONVM_JIT_DIRECT_CALL_ARG_MAPS=0` restores the
                            // cheaper, unrelocatable arrangement.
                            let names_arg_oops = direct_call_arg_maps_enabled()
                                && service_args_base.is_some()
                                && arg_oops.iter().any(|&o| o);
                            if self.can_elide_direct_call_register_spill(
                                &arg_oops,
                                service_args_base.is_some() && !names_arg_oops,
                                1,
                            ) {
                                self.emit_safepoint_metadata_only();
                            } else {
                                // See the invokestatic twin: R11 stages, so only
                                // ARG_REGS are published, and only with slots.
                                self.emit_pre_safepoint_spill_args_published(
                                    service_args_base.is_some(),
                                    false,
                                );
                            }
                            self.emit_call_absolute(callee_entry);
                            self.emit_post_call_rbp_republish();
                            // Stage A (precise oop maps, B-K fix) — a direct
                            // invokespecial/virtual call to a compiled callee is a
                            // GC-capable safepoint (the callee may allocate). Like
                            // the self-recursive site above, it historically spilled
                            // but recorded NO oop map (gap #1), so the precise path
                            // could not remap this frame. Gated behind `precise_maps`
                            // → byte-identical gate-OFF; gate-ON records the map and
                            // the paired post-safepoint register reload.
                            // Also under `shadow_enabled` (balance the shadow push/
                            // reload across this direct call — see the self-recursive
                            // site above).
                            if self.precise_maps || self.shadow_enabled {
                                self.emit_oop_map_for_safepoint();
                            }
                            self.emit_stack_arg_cleanup(total_sub);
                            if let (Some(info), Some(args_base)) = (info_ptr, service_args_base) {
                                self.emit_inline_callee_deopt_check(
                                    info as *const crate::JitInvokeInfo,
                                    arg_slots.len(),
                                    args_base,
                                );
                            } else {
                                self.dbg_unserviced_direct_call(
                                    "invokespecial/virtual",
                                    pc,
                                    info_ptr.is_some(),
                                    service_args_base.is_some(),
                                );
                                self.fail_unserviced_java_direct_call(info_ptr, service_args_base);
                            }

                            // A directly-called compiled callee that throws (or
                            // deopts) returns the `i64::MIN` sentinel. Propagate
                            // the deopt instead of running on with a bogus value.
                            self.emit_post_invoke_exception_check(ret_type);

                            // Reclaim the spill cursor to the popped-args depth
                            // before the result is pushed, exactly as the
                            // dispatch-helper arm below does with its own
                            // `post_pop_spill`.
                            //
                            // `reserve_direct_call_service_slots` parks the cold
                            // deopt-service copy of the arguments ABOVE the argument
                            // slots (it has to: the slots `pop_stack` handed back are
                            // still live sources for `emit_stack_arg_setup`), which
                            // leaves `next_spill_offset` n slots past the pre-pop top.
                            // Pushing the return value from there parks it above its
                            // semantic operand-stack depth, and every later push in
                            // this basic block inherits the shift. The linear walk
                            // stays self-consistent, so nothing looks wrong -- until
                            // the first branch target after the call, whose depth is
                            // re-established from the bytecode. Writer and reader then
                            // address different slots and the method computes with a
                            // stale one. Measured on ECJ's
                            // `OperandStack.pop(OperandCategory)`, whose `if_icmpeq`
                            // (a tableswitch merge point) compared `TypeBinding.id`
                            // against the expected category instead of
                            // `TypeIds.getCategory(id)`: every JSP compiled after that
                            // method tiered up threw `AssertionError: Unexpected
                            // operand at stack top` (tomcat/ecj-operandstack-*.md).
                            //
                            // Safe to hand the reserved range back: its only consumer
                            // is `emit_inline_callee_deopt_check`, emitted just above.
                            self.next_spill_offset = post_pop_spill;

                            if ret_type != b'V' {
                                if matches!(ret_type, b'D' | b'F') {
                                    self.push_from_rax_as_xmm0();
                                } else {
                                    self.push_from_rax();
                                }
                                // Same obligation as the direct INVOKESTATIC arm
                                // (which has always tagged it) and the dispatch
                                // arm below: a reference return is a live oop and
                                // must not keep `push_from_rax`'s default `false`
                                // mark. Missing here, the reference is published
                                // to neither the precise oop map nor the shadow
                                // stack — see the self-recursive site above.
                                if matches!(ret_type, b'L' | b'[') {
                                    self.mark_top_as_oop();
                                }
                            }
                        } // end `if !intrinsic_handled` (plain direct call)
                    } else {
                        // Dispatch via helper (MIC-optimized for virtual/interface, plain for others)
                        // MED-4 / Fix 3 — O(1) pc-indexed lookups for invoke/MIC/PIC.
                        let info_ptr = self
                            .invoke_info_idx
                            .get(&pc)
                            .map(|&i| self.invoke_info[i].1);
                        if let Some(info) = info_ptr {
                            // SAFETY: info comes from self.invoke_info, which holds pointers to
                            // JitInvokeInfo structs kept alive by the caller for the duration of compilation.
                            let info_ref = unsafe { &*info };
                            let n = info_ref.num_jit_args;

                            // value-stack-usize-underflow-nio-worker-panic fix:
                            // snapshot the operand stack BEFORE popping this
                            // invoke's receiver/args, mirroring
                            // `snapshot_pre_intrinsic_call`'s "Step 6" pattern
                            // used by the String-intrinsic ladder above. Without
                            // this, the ONLY deopt point available at this bci is
                            // the one `emit_post_invoke_exception_check` builds
                            // AFTER the args are already popped (it only builds
                            // one when `deopt_box_ptr_by_bci` has no entry yet) —
                            // that snapshot is fine for "resume after the call
                            // with an exception pending", but every such point is
                            // tagged `DeoptAction::Reinterpret` at THIS bci, which
                            // means "re-execute this same invoke bytecode from
                            // scratch" and therefore needs the receiver (+ args)
                            // still live on the operand stack. An empty
                            // post-pop snapshot underflows the moment the
                            // resumed interpreter re-fetches the receiver —
                            // reproduced as a `value_stack.rs` panic on a
                            // background NIO worker thread resuming
                            // `LinkedBlockingQueue.take()`'s `Condition.await()`
                            // interface dispatch after an inline-cache miss.
                            if crate::deopt_real_enabled() {
                                self.snapshot_pre_intrinsic_call(
                                    pc,
                                    crate::deopt::DeoptReason::ReceiverTypeChanged,
                                );
                            }

                            // Check for MIC slot at this PC
                            let mic_ptr = self.mic_slots_idx.get(&pc).map(|&i| self.mic_slots[i].1);
                            // Check for PIC slot at this PC. When both PIC
                            // and MIC are present (the adaptive recompiler
                            // promotes MIC → PIC and leaves the old MIC
                            // slot live as a fallback), PIC takes
                            // precedence: it caches a 4-entry superset.
                            let pic_ptr = self.pic_slots_idx.get(&pc).map(|&i| self.pic_slots[i].1);
                            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JIT_GEN")
                                .is_some()
                            {
                                eprintln!(
                                    "[JIT_GEN_INVOKE_VS] pc={} op=0x{:02x} info_kind={} mic_present={} pic_present={} {}.{}{}",
                                    pc, op, info_ref.invoke_kind, mic_ptr.is_some(), pic_ptr.is_some(),
                                    info_ref.class_name, info_ref.method_name, info_ref.descriptor,
                                );
                            }

                            // Capture spill offset BEFORE popping to prevent
                            // the args buffer from overlapping source Frame slots.
                            let pre_pop_spill = self.next_spill_offset;
                            let (arg_slots, arg_oops) = self.pop_invoke_args(n);
                            // Post-pop cursor — the restore point after the
                            // dispatch (see the invokestatic twin above for
                            // the Bug-4 frame-creep rationale).
                            let post_pop_spill = self.next_spill_offset;

                            let args_base_offset = pre_pop_spill;
                            if n > 0 {
                                let Some(args_end) =
                                    self.checked_spill_range_end(args_base_offset, n)
                                else {
                                    return false;
                                };
                                self.next_spill_offset = args_end;
                                // Store args in reverse offset order (same fix
                                // as invokestatic): higher offsets → lower addresses,
                                // so arg[0] at highest offset = lowest address.
                                for (i, slot) in arg_slots.iter().enumerate() {
                                    let buf_offset = args_base_offset + ((n - 1 - i) as i32) * 8; // Cast: x86-64 immediate encoding
                                    self.load_slot_to_reg(RAX, *slot);
                                    self.emit_store_local(buf_offset, RAX);
                                    // See the invokestatic twin: the map below
                                    // is the only remaining namer of this oop.
                                    if arg_oops[i] {
                                        self.pending_staged_arg_oops.push(buf_offset);
                                    }
                                }
                            }

                            // PRECISE-MAPS FIX (bug-03 layer C / B-K): the inline
                            // MIC/PIC cascade below calls the resolved compiled
                            // callee directly (`call r11`) on a class-id hit and
                            // then `jmp`s to the shared `.done` site, where
                            // `emit_oop_map_for_safepoint` emits the precise
                            // post-safepoint RELOAD of oop register-locals from
                            // their canonical frame slots. But the inline-hit path
                            // never reaches the slow-path `emit_pre_safepoint_spill`
                            // below — so under `precise_maps` the reload would load
                            // an UN-spilled (stale) slot back into a live oop
                            // register (e.g. the receiver `this`), which then
                            // faults on the next `getfield` (observed: compiled
                            // `String.codePointAt` → `this.isLatin1()` inline hit →
                            // reload corrupts `this` → SIGSEGV). Spill HERE, before
                            // the cascade, so the spill dominates BOTH the
                            // inline-hit and the slow/miss paths and pairs with the
                            // single shared reload. Gated on `precise_maps`, and the
                            // slow-path spill below is made `!precise_maps`, so the
                            // gate-OFF default path is byte-identical (exactly one
                            // conservative spill, on the slow path, as before).
                            //
                            // SB-CRASH-04: `safepoint_reg_spill` joins this hoist for
                            // the SAME reason — the inline-hit virtual dispatch is a
                            // GC-capable safepoint (the callee allocates), so the
                            // caller's register-only oops must be spilled BEFORE the
                            // cascade to be visible to the conservative scan. Hoisting
                            // here (vs the .miss slow path) also keeps the inline-hit
                            // `jmp .done` rel8 span from being widened by the spill.
                            if self.precise_maps || self.safepoint_reg_spill {
                                // The hoisted spill dominates the inline-hit and
                                // the miss path and pairs with ONE shared reload
                                // at `.done`. Both halves of that pairing survive
                                // the elision: the predicate requires
                                // `precise_maps` and refuses any register-homed
                                // reference local, so the shared
                                // `emit_post_safepoint_reload` -- which walks
                                // `local_oop_masks[pc]`, oops only -- has nothing
                                // to reload and emits nothing. Every argument,
                                // receiver included, is already in the args
                                // buffer with its oops named in the map above.
                                if self.can_elide_direct_call_register_spill(&arg_oops, true, 3) {
                                    self.emit_safepoint_metadata_only();
                                } else {
                                    // Args are in the buffer and their oops are
                                    // named in the map at `.done`; RAX only when
                                    // the staging loop ran. See the dispatch twin.
                                    self.emit_pre_safepoint_spill_args_published(true, n > 0);
                                }
                            }

                            // CRIT-8 — Inline MIC fast-path guard.
                            //
                            // Layout (verified by `test_jit_mic_slot_offsets` in
                            // jit/src/lib.rs; struct is `#[repr(C)]`):
                            //   offset  0  AtomicU32  cached_class_id
                            //   offset  8  AtomicU64  cached_entry_ptr
                            //   offset 16  AtomicBool cached_needs_context
                            //
                            // HIGH-7 follow-up — Inline 4-way PIC fast-path
                            // guard. `JitPICSlot` is now `#[repr(C)]` with
                            // hot atomic fields at the front (see
                            // `jit/src/lib.rs:1305` and the layout assertion
                            // `test_jit_pic_slot_offsets`):
                            //
                            //   CLASS_ID_OFFSETS      = [0, 4, 8, 12]
                            //   ENTRY_PTR_OFFSETS     = [16, 24, 32, 40]
                            //   NEEDS_CONTEXT_OFFSETS = [48, 49, 50, 51]
                            //
                            // The Mutex<Option<String>> array (`class_names`)
                            // is moved to the tail so its unstable layout
                            // cannot disturb these offsets.
                            //
                            // When a PIC slot is allocated at this PC, we
                            // emit a 4-way cascade in place of the MIC probe.
                            // PIC supersedes MIC (it is a 4-entry superset)
                            // so we do not emit BOTH guards.
                            //
                            // Hot-path sequence (PIC, ≈5 cycles on slot-0 hit):
                            //   mov   r10, imm64(pic)
                            //   mov   rax, [rbp - receiver_spill]
                            //   mov   eax, [rax]                       ; class_id @ ObjectHeader+0
                            //   ; --- per slot i in 0..4 ---
                            //   cmp   eax, [r10 + CLASS_ID_OFFSETS[i]]
                            //   jne   .try_{i+1}  (or .miss for i==2)
                            //   cmp   byte [r10 + NEEDS_CONTEXT_OFFSETS[i]], 0
                            //   je    .noctx
                            //   <load context ABI: vm_ptr + arg_slots[0..n]>
                            //   jmp   .call
                            // .noctx:
                            //   <load context-free ABI: arg_slots[0..n]>
                            // .call:
                            //   call  qword [r10 + ENTRY_PTR_OFFSETS[i]]
                            //   jmp   .done
                            //   ; --- end per-slot ---
                            // .miss:
                            //   <existing helper-ABI setup>
                            //   call  jit_invoke_virtual_mic            ; same helper —
                            //                                            ; it consults the
                            //                                            ; underlying cache
                            //                                            ; (MIC or PIC via the
                            //                                            ; adaptive recompiler).
                            // .done:
                            //
                            // Raw memory loads of the atomics are equivalent
                            // to `Ordering::Relaxed` reads (no fences). A
                            // torn class_id or stale entry pointer at worst
                            // causes a miss → slow path; the helper
                            // revalidates and re-resolves authoritatively.
                            // Empty PIC entries hold class_id == 0, which the
                            // doc reserves for `java.lang.Object` (never a
                            // dispatch target here), so an empty slot
                            // naturally fails its CMP and falls through.
                            //
                            // Fast-path eligibility (same as MIC):
                            //   1. pic_ptr OR mic_ptr is Some.
                            //   2. The receiver exists (n >= 1).
                            //   3. The cached entry's `needs_context` bit is
                            //      checked inline and selects the matching
                            //      compiled-entry ABI.
                            //   4. Total callee-ABI arg count (1 vm_ptr + n)
                            //      fits in ARG_REGS.
                            let needs_ctx_arg_count = n + 1; // vm_ptr + n receiver/params
                            let args_fit = n >= 1 && needs_ctx_arg_count <= ARG_REGS.len();
                            // The inline MIC/PIC path emits a raw CALL into
                            // another compiled body.  That bypasses the
                            // interpreter-owned JitEntryGuard, leaving the
                            // active-RBP mirror pointing at the callee while
                            // the root-chain metadata still names the caller.
                            // That is NOT a reclaim hazard, which is what this
                            // comment used to claim: `chain_entry_rbp_is_foreign`
                            // detects it, the moving-young coverage proof comes
                            // back incomplete, and the cycle falls back to the
                            // non-moving sweep. It costs precision, not
                            // correctness — measured, nothing is reclaimed.
                            // `direct_jit_callee_calls_enabled()` gates the
                            // inline MIC path (default-ON — see its doc
                            // comment for the closing fixes and the
                            // regression this default avoids; opt out with
                            // `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0`). The MIC
                            // and PIC both publish entry pointer + ABI before
                            // the release-store of class id. Generated guards
                            // also reject a zero entry pointer, covering
                            // class-only profile seeds.
                            //
                            // Spring SpEL's flawed-pattern threshold test drives
                            // catastrophic regex backtracking through the mutually
                            // recursive BmpCharPropertyGreedy/GroupHead pair. The
                            // raw inline IC direct-call path skips the dispatch
                            // helper's frame bookkeeping for that recursion shape
                            // and short-circuits the search after only ~2k
                            // CharSequence accesses. Keep just this pair on the
                            // helper path even when the flag is set.
                            let regex_backtracking_frame = self
                                .method_label
                                .starts_with("java/util/regex/Pattern$BmpCharPropertyGreedy.match")
                                || self
                                    .method_label
                                    .starts_with("java/util/regex/Pattern$GroupHead.match");
                            // Cached direct entries cannot publish the caller's
                            // precise exception frame. A protected call in a method
                            // whose handler reads locals must take the dispatch path,
                            // where `emit_post_invoke_exception_check` records that
                            // complete caller state before entering its handler.
                            let protected_precise_handler_call =
                                self.precise_exception_frames && self.pc_is_protected(pc);
                            // Per-site bisect levers (`CRATONVM_JIT_SP_IC_ONLY`
                            // / `_DENY`). Inert unless one is set: the whole
                            // cascade is a program-wide switch otherwise, which
                            // localises a defect to this edge but not to a site.
                            let site_allowed = sp_ic_site_allowed(
                                &self.method_label,
                                info_ref.class_name,
                                info_ref.method_name,
                            );
                            let inline_virtual_ic_allowed =
                                crate::direct_jit_callee_calls_enabled()
                                    && sp_inline_ic_enabled()
                                    && site_allowed
                                    && !regex_backtracking_frame
                                    && !protected_precise_handler_call;
                            let pic_inline = inline_virtual_ic_allowed
                                && sp_inline_pic_enabled()
                                && pic_ptr.is_some()
                                && args_fit;
                            let mic_inline = inline_virtual_ic_allowed
                                && sp_inline_mic_enabled()
                                && !pic_inline
                                && mic_ptr.is_some()
                                && args_fit;
                            if sp_ic_site_trace() {
                                eprintln!(
                                    "[SP_IC_SITE] {}||{}.{}{} pc={} pic={} mic={}",
                                    self.method_label,
                                    info_ref.class_name,
                                    info_ref.method_name,
                                    info_ref.descriptor,
                                    pc,
                                    pic_inline,
                                    mic_inline,
                                );
                            }
                            // `.done` patches collected from each emitted
                            // fast-path. Multiple in PIC's case (one per
                            // slot), one in MIC's, none if neither inline
                            // fires. All are JMP rel32 (5 bytes) so the
                            // patch records a 4-byte signed displacement at
                            // `patch_pos`.
                            let mut done_patches32: Vec<usize> = Vec::new();
                            // `.done` patches that are JMP rel8 (single
                            // byte); MIC and the last PIC slot use these
                            // when the skip distance is small enough.
                            let mut done_patch: Option<usize> = None;
                            let mut mic_miss_patches32: Vec<usize> = Vec::new();

                            if pic_inline {
                                let pic = pic_ptr.expect("pic_inline ⇒ pic_ptr Some");

                                // Cache the layout constants locally so a
                                // future const-rename in lib.rs surfaces as
                                // a compile error here.
                                const CLASS_ID_OFFS: [u8; 4] = [0, 4, 8, 12];
                                const ENTRY_PTR_OFFS: [u8; 4] = [16, 24, 32, 40];
                                const NEEDS_CTX_OFFS: [u8; 4] = [48, 49, 50, 51];

                                // Compile-time sanity: the byte offsets we
                                // hardcode in the encodings below must
                                // match the public constants exported by
                                // `JitPICSlot`. A mismatch here would
                                // silently dispatch to a stale entry_ptr.
                                const _: () = assert!(
                                    crate::JitPICSlot::CLASS_ID_OFFSETS[0] == 0
                                        && crate::JitPICSlot::CLASS_ID_OFFSETS[1] == 4
                                        && crate::JitPICSlot::CLASS_ID_OFFSETS[2] == 8
                                        && crate::JitPICSlot::CLASS_ID_OFFSETS[3] == 12
                                        && crate::JitPICSlot::ENTRY_PTR_OFFSETS[0] == 16
                                        && crate::JitPICSlot::ENTRY_PTR_OFFSETS[1] == 24
                                        && crate::JitPICSlot::ENTRY_PTR_OFFSETS[2] == 32
                                        && crate::JitPICSlot::ENTRY_PTR_OFFSETS[3] == 40
                                        && crate::JitPICSlot::NEEDS_CONTEXT_OFFSETS[0] == 48
                                        && crate::JitPICSlot::NEEDS_CONTEXT_OFFSETS[1] == 49
                                        && crate::JitPICSlot::NEEDS_CONTEXT_OFFSETS[2] == 50
                                        && crate::JitPICSlot::NEEDS_CONTEXT_OFFSETS[3] == 51
                                );

                                // R10 = pic_ptr (imm64, fixed 10-byte form).
                                // Task #60: use the fixed-length form so the
                                // unroll duplicator can locate the imm64 at a
                                // deterministic offset (+2 from the MOV start)
                                // and patch it to a freshly-allocated PIC slot
                                // per unrolled copy. Without this, all copies
                                // would share the original slot — per-iteration
                                // cache hits would collide and miss across
                                // copies for any receiver-type-varying loop.
                                let ic_imm64_off = self.buf.pos() + 2;
                                self.emit_mov_imm64_full(R10, pic as *const _ as i64); // Cast: function pointer for JIT call target
                                self.ic_patches
                                    .push((ic_imm64_off, 1, pic as *const _ as usize)); // 1 = PIC
                                                                                        // SECURITY FIX (V1) INVARIANT: R10 holds the
                                                                                        // PIC slot base pointer from here until each
                                                                                        // per-slot `MOV R11,[R10+disp]; CALL R11`
                                                                                        // below. Code emitted in this window (receiver
                                                                                        // load, NPE guard, class_id load, the per-slot
                                                                                        // CMP/JNE cascade) must touch only RAX and R10
                                                                                        // itself — it must NOT route through any helper
                                                                                        // that clobbers R10 (e.g. emit_bounds_check,
                                                                                        // SIMD lowering). The ABI marshalling below
                                                                                        // targets ARG_REGS only (RCX/RDX/RSI/R8/R9/RDI
                                                                                        // — never R10), so R10 stays the trusted base.

                                // ---- Hoist callee ABI marshalling out of
                                // the 4-way cascade. Previously each slot
                                // re-loaded `vm_ptr + n args` into
                                // ARG_REGS[0..=n] (~26 bytes per slot on
                                // x86-64 SysV with n=4), tripling the
                                // marshalling cost. Hoisting once:
                                //
                                //   * Cuts ~78 bytes per PIC site (≈26B
                                //     × 2 redundant copies).
                                //   * Keeps the per-slot body to: type
                                //     CMP + JNE, needs-ctx CMP + JE,
                                //     CALL [R10+disp], JMP rel32 .done.
                                //   * Args remain live across the inter-
                                //     slot CMP/JNE pairs because those
                                //     instructions touch only RAX and
                                //     R10 (neither is in ARG_REGS).
                                //   * On a successful CALL, ARG_REGS are
                                //     caller-saved and may be clobbered
                                //     by the callee — but we JMP to
                                //     .done immediately afterwards, so
                                //     no other slot's CALL needs them.
                                //   * On the miss path, the slow-path
                                //     prelude (~line 10709) overwrites
                                //     ARG_REGS with its helper-ABI
                                //     arguments (vm_ptr, info, buf,
                                //     n[, mic, pic]) before the helper
                                //     call. The hoisted values are
                                //     already dead at that point.
                                //
                                // We must materialize the receiver-load
                                // *first* (its source spill could alias
                                // ARG_REGS[0] in degenerate frames) and
                                // RAX holds the class_id used by every
                                // per-slot CMP, so RAX is loaded last.
                                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                for j in 0..n {
                                    let spill_off = args_base_offset + ((n - 1 - j) as i32) * 8; // Cast: x86-64 immediate encoding
                                    self.emit_load_local(ARG_REGS[j + 1], spill_off);
                                }
                                // ARG_REGS[1] now holds the receiver
                                // (arg_slots[0]). Reuse it as the source
                                // of the class_id load so we avoid an
                                // extra reload from the receiver spill —
                                // this saves an additional ~5 bytes vs
                                // the prior `emit_load_local(RAX, ...)`.
                                // MOV EAX, dword [ARG_REGS[1]] — load
                                // class_id (ObjectHeader+0). Encoding
                                // depends on whether the receiver reg is
                                // an extended register (R8+).
                                // round-7 audit (bug 4): ARG_REGS[1]
                                // here is the receiver register — the
                                // loop above (`emit_load_local(ARG_REGS[j+1], …)`)
                                // wrote `arg_slots[0]` (= the receiver
                                // by JVM invokevirtual/interface
                                // calling convention) into ARG_REGS[0+1].
                                // The debug_assert below is therefore
                                // checking the right register; verified.
                                let recv_reg = ARG_REGS[1];
                                // NPE guard: a null receiver must not be
                                // dereferenced by the class_id load below
                                // (`MOV EAX, [recv_reg]`) — that faults at
                                // address 0 and SIGSEGVs the VM (observed
                                // in Tomcat: JIT-compiled `Locale.hashCode`
                                // dispatching `BaseLocale.hashCode` on a
                                // null receiver). Route a null receiver to
                                // the `.miss` slow path, which decodes args
                                // and bails to the interpreter where the
                                // invokevirtual receiver null check raises
                                // a proper NullPointerException.
                                //   TEST recv_reg, recv_reg
                                if recv_reg >= 8 {
                                    self.buf.emit(&[
                                        0x4D,
                                        0x85,
                                        0xC0 | ((recv_reg & 7) << 3) | (recv_reg & 7),
                                    ]);
                                } else {
                                    self.buf.emit(&[
                                        0x48,
                                        0x85,
                                        0xC0 | ((recv_reg & 7) << 3) | (recv_reg & 7),
                                    ]);
                                }
                                //   JZ rel32 → .miss (patched at miss_off)
                                self.buf.emit(&[0x0F, 0x84, 0x00, 0x00, 0x00, 0x00]);
                                let pic_null_miss_patch = self.buf.pos() - 4;
                                // ARRAY-RECEIVER GUARD. `ObjectHeader.class_id` sits at offset 0 for objects AND for
                                // arrays, and a reference array stores its
                                // COMPONENT class id there — a `Foo[]` and a
                                // `Foo` present the SAME 4-byte guard word. A
                                // class-id-only guard therefore lets a site
                                // warmed on a `Foo` receiver dispatch a later
                                // `Foo[]` receiver straight into `Foo`'s own
                                // method body, where the first `checkcast Foo`
                                // throws `class [LFoo; cannot be cast to class
                                // Foo`. (The helper never INSTALLS an entry for
                                // an array receiver — `cacheable_receiver` is
                                // false for `ObjectKind::Array` — so only the
                                // consumption guard was ever wrong.)
                                // `ObjectHeader.kind` (offset 4) separates the
                                // two; anything that is not a plain object goes
                                // to the miss path, which resolves on the real
                                // receiver.
                                //   CMP BYTE [recv_reg + KIND_TAGS_BYTE_OFFSET], Object
                                // mod=01 (disp8) / reg=/7 (CMP imm8) / rm=recv.
                                // `recv_reg & 7 != 4` is already asserted below
                                // (mod=00 would need a SIB there), and mod=01
                                // makes rm==5 an ordinary [reg+disp8].
                                if recv_reg >= 8 {
                                    self.buf.emit_byte(0x41); // REX.B
                                }
                                self.buf.emit(&[
                                    0x80,
                                    0x78 | (recv_reg & 7),
                                    cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
                                    cratonvm_types::ObjectKind::Object as u8,
                                ]);
                                //   JNE rel32 → .miss
                                self.buf.emit(&[0x0F, 0x85, 0x00, 0x00, 0x00, 0x00]);
                                let pic_kind_miss_patch = self.buf.pos() - 4;
                                // MED (round-5 review): mod=00 encoding
                                // reuses the low-3 bits of the register as
                                // r/m, where r/m==4 (RSP/R12) means
                                // SIB-follows and r/m==5 (RBP/R13) means
                                // RIP-relative — both would mis-encode this
                                // displacement-free load. Safe today
                                // because RCX(.1)/RDX(.2)/RSI(.6) are all
                                // outside {4,5}, but any future ARG_REGS
                                // shuffle would silently corrupt this PIC
                                // slot. Trap brittleness in debug builds.
                                debug_assert!(
                                    (recv_reg & 7) != 4 && (recv_reg & 7) != 5,
                                    "PIC slot-0 mod=00 requires ARG_REGS[1] low3 not in {{4,5}} (got {})",
                                    recv_reg,
                                );
                                if recv_reg >= 8 {
                                    // REX.B + 8B /r, modrm = mod(00) reg(EAX=0) rm(recv&7)
                                    self.buf.emit(&[0x41, 0x8B, recv_reg & 7]);
                                } else {
                                    // 8B /r, modrm = mod(00) reg(EAX=0) rm(recv)
                                    self.buf.emit(&[0x8B, recv_reg]);
                                }

                                // Per-slot cascade. Inter-slot `jne` jumps
                                // are rel8 and patched once we know the
                                // start of the next slot. Final slot's
                                // `jne` and every `je needs_ctx → .miss`
                                // jump to the shared `.miss` label.
                                //
                                // `slot_starts[i]` is the byte position of
                                // slot i's first emitted byte (the CMP
                                // opcode); used to resolve inter-slot
                                // `jne` rel8 patches once all slots are
                                // emitted.
                                let mut slot_starts: [usize; 4] = [0; 4];
                                // (patch_pos, target_slot_index) for each
                                // inter-slot `jne` rel8 that needs to land
                                // at the start of slot `target_slot_index`.
                                let mut next_slot_patches: Vec<(usize, usize)> = Vec::new();
                                // CRIT-3 — miss branches use rel32 form
                                // unconditionally. With n>=5 args on Linux
                                // the cumulative body across all three
                                // slots + slow-path prelude can exceed 127
                                // bytes, overflowing the previous rel8
                                // encoding (`0x74`/`0x75`). The rel32
                                // forms (`0x0F 0x84`/`0x0F 0x85` + 4-byte
                                // disp) always fit. `miss_patches_rel32`
                                // stores the byte offset of the 4-byte
                                // displacement immediate, patched at the
                                // shared `.miss` label below.
                                // Seeded with the receiver-null-check JZ
                                // emitted above the cascade so it is patched
                                // to the same shared `.miss` target.
                                let mut miss_patches_rel32: Vec<usize> =
                                    vec![pic_null_miss_patch, pic_kind_miss_patch];

                                for i in 0..crate::JIT_PIC_ENTRIES {
                                    slot_starts[i] = self.buf.pos();

                                    // CMP EAX, dword [R10 + CLASS_ID_OFFS[i]]
                                    // For disp == 0 (slot 0 today) emit the
                                    // mod=00 form with no displacement byte:
                                    // saves 1 byte per JIT site on the hot
                                    // slot-0 cascade. For disp != 0 use the
                                    // mod=01 (disp8) form. R10 in mod=00 is
                                    // ModRM 00_000_010 = 0x02.
                                    if CLASS_ID_OFFS[i] == 0 {
                                        // 3 bytes: REX.B + 3B /r + ModRM(00,000,010).
                                        self.buf.emit(&[0x41, 0x3B, 0x02]);
                                    } else {
                                        // 4 bytes: REX.B + 3B /r + ModRM(01,000,010) + disp8.
                                        self.buf.emit(&[0x41, 0x3B, 0x42, CLASS_ID_OFFS[i]]);
                                    }

                                    if i + 1 < crate::JIT_PIC_ENTRIES {
                                        // JNE rel32 → start of slot i+1
                                        // (patched below once slot i+1's
                                        // start position is known).
                                        //
                                        // This was rel8 on the claim that "a
                                        // single slot body is ~30 bytes for
                                        // n<=5". That stopped being true: a
                                        // slot body now also carries the
                                        // post-call innermost-RBP republish and
                                        // the callee-deopt service check, which
                                        // together put it well past 127 bytes.
                                        // The patch truncated the displacement
                                        // (`rel as u8`) behind a
                                        // `debug_assert!`, so release builds
                                        // silently got `JNE -128` — a branch
                                        // into the middle of the pre-call
                                        // shadow-stack push, which then ran as
                                        // an unguarded infinite push loop and
                                        // walked off the end of the thread's
                                        // shadow buffer (SIGSEGV ~170 MiB
                                        // later, at the arena end). Only
                                        // reachable with raw JIT-to-JIT calls
                                        // enabled, which is why the closed gate
                                        // hid it. rel32 has no such cliff.
                                        self.buf.emit(&[0x0F, 0x85, 0x00, 0x00, 0x00, 0x00]);
                                        let patch = self.buf.pos() - 4;
                                        next_slot_patches.push((patch, i + 1));
                                    } else {
                                        // Final slot: JNE rel32 → .miss
                                        // (6 bytes: 0x0F 0x85 + i32 disp).
                                        // The final slot's miss target sits past
                                        // the whole cascade and may be reachable
                                        // in rel8 but we keep rel32 for
                                        // consistency with slot 0/1 and
                                        // because the slow-path prelude
                                        // following the cascade can push
                                        // the distance over 127 bytes.
                                        self.buf.emit(&[0x0F, 0x85, 0x00, 0x00, 0x00, 0x00]);
                                        miss_patches_rel32.push(self.buf.pos() - 4);
                                    }

                                    // A receiver profile may pre-populate only
                                    // class_id; entry_ptr remains zero until
                                    // the resolving helper compiles and
                                    // publishes a concrete target.
                                    // CMP QWORD [R10 + ENTRY_PTR_OFFS[i]], 0
                                    self.buf.emit(&[0x49, 0x83, 0x7A, ENTRY_PTR_OFFS[i], 0x00]);
                                    // JE rel32 → .miss
                                    self.buf.emit(&[0x0F, 0x84, 0x00, 0x00, 0x00, 0x00]);
                                    miss_patches_rel32.push(self.buf.pos() - 4);

                                    // CMP BYTE [R10 + NEEDS_CTX_OFFS[i]], 0
                                    // 5 bytes: REX.B (0x41) + 80 /7 + modrm
                                    //   modrm = mod(01) reg(/7=111) rm(010)
                                    //         = 0b01_111_010 = 0x7A
                                    //   + disp8 + imm8(0)
                                    self.buf.emit(&[0x41, 0x80, 0x7A, NEEDS_CTX_OFFS[i], 0x00]);

                                    // JE rel32 → .noctx. Both ABI shapes are
                                    // valid cache hits; small interface
                                    // implementations are commonly
                                    // context-free.
                                    self.buf.emit(&[0x0F, 0x84, 0x00, 0x00, 0x00, 0x00]);
                                    let noctx_patch = self.buf.pos() - 4;

                                    // Callee ABI args (vm_ptr + n) have
                                    // already been materialised once in
                                    // ARG_REGS[0..=n] above the cascade
                                    // (HIGH-perf hoist; see comment at
                                    // the top of the PIC body). Slot
                                    // bodies must NOT touch ARG_REGS.

                                    // Context ABI is already live. Skip the
                                    // alternate marshalling block.
                                    self.buf.emit(&[0xE9, 0x00, 0x00, 0x00, 0x00]);
                                    let ctx_call_patch = self.buf.pos() - 4;

                                    // .noctx: Java args start at ARG_REGS[0].
                                    let noctx_off = self.buf.pos();
                                    let noctx_rel = (noctx_off as i64) - (noctx_patch as i64 + 4);
                                    debug_assert!(
                                        (i32::MIN as i64..=i32::MAX as i64).contains(&noctx_rel)
                                    );
                                    self.buf.try_patch_i32(noctx_patch, noctx_rel as i32).ok();
                                    for j in 0..n {
                                        let spill_off = args_base_offset + ((n - 1 - j) as i32) * 8;
                                        self.emit_load_local(ARG_REGS[j], spill_off);
                                    }

                                    // .call
                                    let call_off = self.buf.pos();
                                    let call_rel = (call_off as i64) - (ctx_call_patch as i64 + 4);
                                    debug_assert!(
                                        (i32::MIN as i64..=i32::MAX as i64).contains(&call_rel)
                                    );
                                    self.buf.try_patch_i32(ctx_call_patch, call_rel as i32).ok();

                                    // SECURITY FIX (V1): do NOT keep the
                                    // call target live in memory addressed
                                    // through R10 across an indirect CALL.
                                    // R10 is the shared bounds-check / SIMD
                                    // scratch register (see SCRATCH_REGS
                                    // exclusion and emit_bounds_check, which
                                    // clobbers R10D). Previously this site
                                    // emitted `CALL qword [R10 + disp]`, so
                                    // ANY R10-clobbering instruction emitted
                                    // in the window between `MOV R10,&slot`
                                    // and the CALL would corrupt the call
                                    // target → indirect call to an attacker-
                                    // influenced address. We close the window
                                    // to a single, fixed instruction pair:
                                    // load the entry_ptr into R11 (a
                                    // caller-saved scratch reg that is NOT in
                                    // ARG_REGS / SCRATCH_REGS / LOCAL_REGS and
                                    // is clobbered by the call anyway) and
                                    // CALL R11. The R10→R11 load reads R10
                                    // exactly once, immediately before the
                                    // CALL, with nothing emittable in between,
                                    // so no later codegen can perturb the
                                    // target. INVARIANT: nothing may be
                                    // emitted between this entry-ptr load and
                                    // the paired `CALL R11` below.
                                    //
                                    // MOV R11, qword [R10 + ENTRY_PTR_OFFS[i]]
                                    if ENTRY_PTR_OFFS[i] == 0 {
                                        // 3 bytes: REX.WRB + 8B /r + ModRM(00,R11,R10)
                                        self.buf.emit(&[0x4D, 0x8B, 0x1A]);
                                    } else {
                                        // 4 bytes: REX.WRB + 8B /r + ModRM(01,R11,R10) + disp8
                                        self.buf.emit(&[0x4D, 0x8B, 0x5A, ENTRY_PTR_OFFS[i]]);
                                    }
                                    // CALL R11  (3 bytes: REX.B + FF /2 + ModRM(11,/2,R11))
                                    self.buf.emit(&[0x41, 0xFF, 0xD3]);
                                    // A compiled callee's prologue publishes
                                    // ITS rbp into the innermost-RBP mirror the
                                    // GC reads. Nothing on the return path of a
                                    // raw JIT-to-JIT call restores this
                                    // caller's, so without this the mirror
                                    // names a DEAD frame from here on and the
                                    // next GC applies THIS method's oop map to
                                    // whatever has since reused that stack
                                    // memory. The MIC arm below and the hashed
                                    // stub have always republished; this
                                    // cascade did not, and since a PIC slot is
                                    // allocated eagerly at every eligible site
                                    // (`pic_inline` wins over `mic_inline`
                                    // whenever `pic_ptr` is Some) it is the arm
                                    // that actually runs. See
                                    // `conservative_roots::top_rbp_mirror_write`
                                    // for the Rust-side analogue of the same
                                    // contract.
                                    self.emit_post_call_rbp_republish();
                                    self.emit_inline_callee_deopt_check(info, n, args_base_offset);

                                    // JMP rel32 → .done. Use rel32 because
                                    // for slots 0 and 1 the skip distance
                                    // (remaining slot bodies + slow path)
                                    // routinely exceeds 127 bytes. Slot 2's
                                    // .done jump is short but we keep rel32
                                    // uniform to keep patching simple.
                                    // branching logic.
                                    // E9 cd: JMP rel32 (5 bytes).
                                    self.buf.emit(&[0xE9, 0x00, 0x00, 0x00, 0x00]);
                                    done_patches32.push(self.buf.pos() - 4);
                                }

                                // Resolve inter-slot `jne` rel8 patches now
                                // that every slot's start is known.
                                for (jne_patch, target_slot) in &next_slot_patches {
                                    let slot_start = slot_starts[*target_slot];
                                    // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                                    let rel = (slot_start as i64) - (*jne_patch as i64 + 4);
                                    debug_assert!(
                                        // Widening: i32 bound -> i64 (range comparison)
                                        (i32::MIN as i64..=i32::MAX as i64).contains(&rel),
                                        "inline PIC inter-slot jne overflowed rel32 ({} bytes)",
                                        rel
                                    );
                                    // Cast: rel32 displacement. On Err,
                                    // try_patch_i32 marks the buffer
                                    // overflowed and the compile bails.
                                    self.buf.try_patch_i32(*jne_patch, rel as i32).ok();
                                }

                                // .miss: patch all `je needs_ctx → .miss`
                                // and the final slot's `jne → .miss` to land
                                // HERE — the start of the slow-path block
                                // emitted below.  CRIT-3: these are all
                                // rel32 form, so the patch site holds a
                                // 4-byte signed displacement computed
                                // from the byte AFTER the immediate
                                // (patch + 4) to the target.
                                let miss_off = self.buf.pos();
                                for patch in &miss_patches_rel32 {
                                    // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                                    let rel = (miss_off as i64) - (*patch as i64 + 4);
                                    debug_assert!(
                                        // Widening: i32 bound -> i64 (range comparison)
                                        (i32::MIN as i64..=i32::MAX as i64).contains(&rel),
                                        "inline PIC miss branch overflowed rel32 ({} bytes)",
                                        rel
                                    );
                                    self.buf
                                        .try_patch_i32(*patch, rel as i32) // Cast: rel32 displacement
                                        .ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                                }
                                // The MIC arm maintains its own rel32 miss
                                // patches; PIC uses the vector above.
                            } else if mic_inline {
                                let mic = mic_ptr.expect("mic_inline ⇒ mic_ptr Some");
                                // R10 = mic_ptr (imm64, fixed 10-byte form).
                                // Task #60: same rationale as the PIC arm —
                                // fixed-length encoding gives the unroll
                                // duplicator a deterministic imm64 location
                                // to overwrite with a per-copy fresh MIC slot.
                                let ic_imm64_off = self.buf.pos() + 2;
                                self.emit_mov_imm64_full(R10, mic as *const _ as i64); // Cast: function pointer for JIT call target
                                self.ic_patches
                                    .push((ic_imm64_off, 0, mic as *const _ as usize)); // 0 = MIC
                                                                                        // SECURITY FIX (V1) INVARIANT: R10 holds the
                                                                                        // MIC slot base pointer from here until the
                                                                                        // `MOV R11,[R10+8]; CALL R11` below. Every
                                                                                        // instruction emitted in this window (receiver
                                                                                        // load into RAX, NPE guard, class_id/needs-ctx
                                                                                        // checks, and the ARG_REGS marshalling loop)
                                                                                        // touches only RAX, R10, and ARG_REGS — never
                                                                                        // R10 as a destination — so the call target
                                                                                        // base cannot be perturbed before the CALL.

                                // Load receiver pointer into RAX. Receiver is
                                // arg_slots[0], spilled at the *highest* offset
                                // (lowest address) in the args buffer.
                                let receiver_spill = args_base_offset + ((n as i32) - 1) * 8; // Cast: x86-64 immediate encoding
                                self.emit_load_local(RAX, receiver_spill);

                                // NPE guard: a null receiver must not reach
                                // the `MOV EAX,[RAX]` class_id load below —
                                // dereferencing address 0 SIGSEGVs the VM.
                                // Route null to `.miss` (slow helper →
                                // interpreter), which raises a proper
                                // NullPointerException per JVMS invokevirtual.
                                //   TEST RAX, RAX  (48 85 C0)
                                self.buf.emit(&[0x48, 0x85, 0xC0]);
                                //   JZ rel32 → .miss. The dual-ABI
                                // marshalling blocks make the miss span larger
                                // than rel8 for otherwise tiny callees.
                                self.buf.emit(&[0x0F, 0x84, 0x00, 0x00, 0x00, 0x00]);
                                mic_miss_patches32.push(self.buf.pos() - 4);

                                // ARRAY-RECEIVER GUARD. `ObjectHeader.class_id` sits at offset 0 for objects AND for
                                // arrays, and a reference array stores its
                                // COMPONENT class id there — a `Foo[]` and a
                                // `Foo` present the SAME 4-byte guard word. A
                                // class-id-only guard therefore lets a site
                                // warmed on a `Foo` receiver dispatch a later
                                // `Foo[]` receiver straight into `Foo`'s own
                                // method body, where the first `checkcast Foo`
                                // throws `class [LFoo; cannot be cast to class
                                // Foo`. (The helper never INSTALLS an entry for
                                // an array receiver — `cacheable_receiver` is
                                // false for `ObjectKind::Array` — so only the
                                // consumption guard was ever wrong.)
                                // `ObjectHeader.kind` (offset 4) separates the
                                // two; anything that is not a plain object goes
                                // to the miss path, which resolves on the real
                                // receiver.
                                //   CMP BYTE [RAX + KIND_TAGS_BYTE_OFFSET], Object
                                self.buf.emit(&[
                                    0x80,
                                    0x78,
                                    cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
                                    cratonvm_types::ObjectKind::Object as u8,
                                ]);
                                //   JNE rel32 → .miss
                                self.buf.emit(&[0x0F, 0x85, 0x00, 0x00, 0x00, 0x00]);
                                mic_miss_patches32.push(self.buf.pos() - 4);

                                // MOV EAX, dword [RAX]  — load class_id (ObjectHeader+0).
                                // 2 bytes: 8B 00
                                self.buf.emit(&[0x8B, 0x00]);

                                // CMP EAX, dword [R10 + 0]  — vs cached_class_id.
                                // 3 bytes: REX.B (0x41) + 3B /r + modrm(00 000 010)
                                self.buf.emit(&[0x41, 0x3B, 0x02]);

                                // JNE rel32 → .miss.
                                self.buf.emit(&[0x0F, 0x85, 0x00, 0x00, 0x00, 0x00]);
                                mic_miss_patches32.push(self.buf.pos() - 4);

                                // A profiled MIC can publish the receiver class
                                // before the helper has installed a compiled
                                // target. Never CALL a class-only seed.
                                // CMP QWORD [R10 + 8], 0
                                self.buf.emit(&[0x49, 0x83, 0x7A, 0x08, 0x00]);
                                // JE rel32 → .miss
                                self.buf.emit(&[0x0F, 0x84, 0x00, 0x00, 0x00, 0x00]);
                                mic_miss_patches32.push(self.buf.pos() - 4);

                                // CMP BYTE [R10 + 16], 0  — select cached entry ABI.
                                // 5 bytes: REX.B (0x41) + 80 /7 + modrm(01 111 010) + disp8 + imm8
                                self.buf.emit(&[0x41, 0x80, 0x7A, 0x10, 0x00]);

                                // JE rel32 → .noctx. Context-free compiled
                                // methods use Java arg0 in ARG_REGS[0], while
                                // context-using methods reserve that register
                                // for vm_ptr. The cache publishes this ABI bit;
                                // honoring both shapes is essential because
                                // small interface implementations almost
                                // always compile context-free.
                                self.buf.emit(&[0x0F, 0x84, 0x00, 0x00, 0x00, 0x00]);
                                let noctx_patch = self.buf.pos() - 4;

                                // ---- Set up callee ABI: (vm_ptr, arg_slots[0..n]) ----
                                // vm_ptr → ARG_REGS[0]
                                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                // arg_slots[i] → ARG_REGS[i + 1]
                                for i in 0..n {
                                    let spill_off = args_base_offset + ((n - 1 - i) as i32) * 8; // Cast: x86-64 immediate encoding
                                    self.emit_load_local(ARG_REGS[i + 1], spill_off);
                                }

                                // JMP rel32 → .call, skipping the no-context
                                // marshalling block.
                                self.buf.emit(&[0xE9, 0x00, 0x00, 0x00, 0x00]);
                                let ctx_call_patch = self.buf.pos() - 4;

                                // .noctx: Java args begin at ARG_REGS[0].
                                let noctx_off = self.buf.pos();
                                let noctx_rel = (noctx_off as i64) - (noctx_patch as i64 + 4);
                                debug_assert!(
                                    (i32::MIN as i64..=i32::MAX as i64).contains(&noctx_rel)
                                );
                                self.buf.try_patch_i32(noctx_patch, noctx_rel as i32).ok();
                                for i in 0..n {
                                    let spill_off = args_base_offset + ((n - 1 - i) as i32) * 8;
                                    self.emit_load_local(ARG_REGS[i], spill_off);
                                }

                                // .call
                                let call_off = self.buf.pos();
                                let call_rel = (call_off as i64) - (ctx_call_patch as i64 + 4);
                                debug_assert!(
                                    (i32::MIN as i64..=i32::MAX as i64).contains(&call_rel)
                                );
                                self.buf.try_patch_i32(ctx_call_patch, call_rel as i32).ok();

                                // SECURITY FIX (V1): same hardening as the
                                // PIC arm — never CALL indirectly through a
                                // target addressed by R10, because R10 is the
                                // shared bounds-check / SIMD scratch register
                                // and any R10-clobbering instruction emitted
                                // in the window between `MOV R10,&slot` and
                                // the CALL would redirect the call. The
                                // ABI-marshalling loop above this point loads
                                // into ARG_REGS (never R10), so R10 is intact
                                // here today, but we still tighten the window
                                // to a single fixed pair: load the cached
                                // entry_ptr into R11 (caller-saved scratch,
                                // not in ARG_REGS / SCRATCH_REGS / LOCAL_REGS,
                                // clobbered by the call anyway) and CALL R11.
                                // INVARIANT: nothing may be emitted between
                                // this load and the paired `CALL R11`.
                                //
                                // MOV R11, qword [R10 + 8]  — cached_entry_ptr.
                                // 4 bytes: REX.WRB + 8B /r + ModRM(01,R11,R10) + disp8
                                self.buf.emit(&[0x4D, 0x8B, 0x5A, 0x08]);
                                // CALL R11  (3 bytes: REX.B + FF /2 + ModRM(11,/2,R11))
                                self.buf.emit(&[0x41, 0xFF, 0xD3]);
                                self.emit_post_call_rbp_republish();
                                self.emit_inline_callee_deopt_check(info, n, args_base_offset);

                                // JMP rel32 → .done. The root-frame
                                // republish above makes the distance exceed
                                // the old rel8 budget.
                                self.buf.emit(&[0xE9, 0x00, 0x00, 0x00, 0x00]);
                                done_patches32.push(self.buf.pos() - 4);

                                // .miss: patch both rel32 sites here.
                                let miss_off = self.buf.pos();
                                for patch in &mic_miss_patches32 {
                                    // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                                    let rel = (miss_off as i64) - (*patch as i64 + 4);
                                    debug_assert!(
                                        (i32::MIN as i64..=i32::MAX as i64).contains(&rel),
                                        "inline MIC miss branch overflowed rel32 ({} bytes)",
                                        rel
                                    );
                                    self.buf.try_patch_i32(*patch, rel as i32).ok();
                                    // on Err try_patch_byte set buf.overflowed; compile bails
                                }
                            }

                            // ---- Shared compact hashed/vtable stub ----
                            // Both the baseline and optimizing tiers lower
                            // megamorphic misses through this exact library.
                            // It reloads arg0, performs two lock-free probes,
                            // and falls through here only on a real miss.
                            if let Some(pic) = pic_ptr
                                .filter(|_| inline_virtual_ic_allowed && sp_inline_mega_enabled())
                            {
                                let arg_offsets: Vec<i32> = (0..n)
                                    .map(|i| args_base_offset + ((n - 1 - i) as i32) * 8)
                                    .collect();
                                done_patches32.extend(
                                    crate::runtime_lowering::emit_hashed_vtable_stub(
                                        &mut self.buf,
                                        pic as usize,
                                        self.heap_local_offset,
                                        &arg_offsets,
                                        if self.precise_maps {
                                            self.helpers.frame_record
                                        } else {
                                            0
                                        },
                                        self.helpers.service_callee_deopt,
                                        info as usize,
                                        // This backend stages its outgoing
                                        // arguments into one descending block,
                                        // so element 0 already names it.
                                        arg_offsets[0],
                                    ),
                                );
                            }

                            // ---- Slow path: helper ABI setup + call ----
                            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                            self.emit_mov_imm64(ARG_REGS[1], info as *const _ as i64); // Cast: function pointer for JIT call target
                            if n > 0 {
                                let buf_start = args_base_offset + ((n as i32) - 1) * 8; // Cast: x86-64 immediate encoding
                                self.emit_lea_frame_slot(ARG_REGS[2], buf_start);
                            } else {
                                self.emit_xor_reg_self(ARG_REGS[2]);
                            }
                            self.emit_mov_imm32_sx(ARG_REGS[3], n as i32); // Cast: x86-64 immediate encoding

                            // Round-8 wave-3: defensive callee-saved spill
                            // before any GC-triggering dispatch CALL. Under
                            // `precise_maps` (or SB-CRASH-04 `safepoint_reg_spill`)
                            // the spill was already emitted before the inline
                            // cascade (so it dominates the inline-hit path too — see
                            // the PRECISE-MAPS FIX comment above); emitting it again
                            // here would be a redundant double-spill (and, with the
                            // shadow stack, an unbalanced double push). Gate-OFF:
                            // unchanged — this is the single conservative spill on
                            // the slow path.
                            if !self.precise_maps && !self.safepoint_reg_spill {
                                self.emit_pre_safepoint_spill();
                            }

                            if let Some(mic) = mic_ptr {
                                // MIC-optimized dispatch: pass MIC slot as 5th
                                // arg and (CRIT-1) PIC slot as 6th arg so the
                                // helper can populate the inline 4-way cascade
                                // via `JitPICSlot::install` on every successful
                                // resolution. A `pic_ptr == 0` tells the helper
                                // no PIC is installed for this site.
                                //
                                // On Windows x64, args 5 and 6 go on the stack
                                // at [RSP+32] and [RSP+40] (shadow + spill).
                                // RAX is caller-saved so we stage each
                                // pointer through it before storing — safe
                                // regardless of which call encoding
                                // `emit_call_absolute` picks (rel32 leaves
                                // RAX alone; the imm64-via-RAX fallback
                                // would overwrite RAX anyway, but that
                                // happens after we've already stored).
                                let pic_arg: i64 =
                                    pic_ptr.map(|p| p as *const _ as i64).unwrap_or(0); // Cast: function pointer for JIT call target
                                #[cfg(target_os = "windows")]
                                {
                                    // 5th arg at [RSP + 32]
                                    let mic_arg_imm64_off = self.buf.pos() + 2;
                                    self.emit_mov_imm64_full(RAX, mic as *const _ as i64);
                                    self.ic_patches.push((
                                        mic_arg_imm64_off,
                                        0,
                                        mic as *const _ as usize,
                                    ));
                                    // MOV [RSP + 32], RAX
                                    self.rex_w();
                                    self.buf.emit(&[0x89, 0x44, 0x24, 0x20]);
                                    // 6th arg at [RSP + 40]
                                    let pic_arg_imm64_off = self.buf.pos() + 2;
                                    self.emit_mov_imm64_full(RAX, pic_arg);
                                    if let Some(pic) = pic_ptr {
                                        self.ic_patches.push((
                                            pic_arg_imm64_off,
                                            1,
                                            pic as *const _ as usize,
                                        ));
                                    }
                                    // MOV [RSP + 40], RAX
                                    self.rex_w();
                                    self.buf.emit(&[0x89, 0x44, 0x24, 0x28]);
                                }
                                #[cfg(not(target_os = "windows"))]
                                {
                                    // SysV: 5th arg in R8, 6th in R9.
                                    let mic_arg_imm64_off = self.buf.pos() + 2;
                                    self.emit_mov_imm64_full(R8, mic as *const _ as i64);
                                    self.ic_patches.push((
                                        mic_arg_imm64_off,
                                        0,
                                        mic as *const _ as usize,
                                    ));
                                    let pic_arg_imm64_off = self.buf.pos() + 2;
                                    self.emit_mov_imm64_full(R9, pic_arg);
                                    if let Some(pic) = pic_ptr {
                                        self.ic_patches.push((
                                            pic_arg_imm64_off,
                                            1,
                                            pic as *const _ as usize,
                                        ));
                                    }
                                }
                                self.emit_call_absolute(self.helpers.invoke_virtual_mic);
                            } else {
                                // Plain dispatch without MIC
                                self.emit_call_absolute(self.helpers.invoke_dispatch);
                            }

                            // .done: patch the fast-path forward JMP(s).
                            //   - `done_patch`     (rel8, MIC inline)
                            //   - `done_patches32` (rel32, PIC inline — one
                            //                       per cache slot)
                            let done_off = self.buf.pos();
                            if let Some(patch) = done_patch {
                                // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                                let rel = (done_off as i64) - (patch as i64 + 1);
                                debug_assert!(
                                    (-128..=127).contains(&rel),
                                    "inline MIC done jump overflowed rel8 ({} bytes)",
                                    rel
                                );
                                Self::patch_rel8_or_bail(&mut self.buf, patch, rel);
                            }
                            for patch in &done_patches32 {
                                // `patch` points at the start of the rel32
                                // immediate (4 bytes); the JMP opcode (E9)
                                // precedes it by 1 byte. The displacement
                                // is computed from the byte AFTER the
                                // immediate (patch + 4) to the target.
                                // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
                                let rel = (done_off as i64) - (*patch as i64 + 4);
                                debug_assert!(
                                    // Widening: i32 bound -> i64 (range comparison)
                                    (i32::MIN as i64..=i32::MAX as i64).contains(&rel),
                                    "inline PIC done jump overflowed rel32 ({} bytes)",
                                    rel
                                );
                                self.buf
                                    .try_patch_i32(*patch, rel as i32) // Cast: rel32 displacement
                                    .ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                            }
                            // T1.1.2 — every virtual/interface dispatch is
                            // a full safepoint: the callee may allocate,
                            // throw, or block. Record the oop map before
                            // the return value is pushed so the GC root
                            // walker has precise coverage at the return PC.
                            self.emit_oop_map_for_safepoint();

                            // After the dispatch returns, check whether the
                            // callee threw a Java exception. The dispatch
                            // helpers return `i64::MIN` (and stash the
                            // exception object in `JIT_PENDING_EXCEPTION`)
                            // when the callee throws; without this guard the
                            // JIT would push the bogus return value and keep
                            // running, masking the real exception with a
                            // downstream secondary failure. The guard deopts
                            // out so the interpreter routes the exception
                            // through this method's exception table.
                            self.emit_post_invoke_exception_check(info_ref.return_type);

                            // Reclaim spill slots used for invoke args AND the
                            // popped arg values — restoring to `pre_pop_spill`
                            // here was the Bug-4 cursor creep (n slots per
                            // non-void dispatch).
                            self.next_spill_offset = post_pop_spill;

                            if info_ref.return_type != b'V' {
                                if matches!(info_ref.return_type, b'D' | b'F') {
                                    self.push_from_rax_as_xmm0();
                                } else {
                                    self.push_from_rax();
                                }
                                // Tag the return as an oop when the
                                // descriptor is L... or [....
                                if matches!(info_ref.return_type, b'L' | b'[') {
                                    self.mark_top_as_oop();
                                }
                            }
                        }
                    }
                    for done in guarded_virtual_done_patches {
                        self.patch_rel32_to_here(done);
                    }
                    if op == 0xb9 {
                        pc += 5;
                    } else {
                        pc += 3;
                    }
                }

                // invokedynamic — unconditional deopt to the interpreter.
                //
                // This instruction is never actually JIT-executed: rather than
                // building call-site machinery for MethodHandle/CallSite
                // dispatch, the codegen jumps straight to the EXISTING shared
                // uncommon-trap deopt stub (reason 8 = `UnreachedCode`), whose
                // pre-existing policy (`DeoptimizationController::recommend_action`,
                // `jit/src/deopt.rs`) gives up immediately on first occurrence —
                // exactly the right fail-safe: if this exact program point is
                // ever actually reached at runtime (assertions enabled, or a
                // genuinely live indy), the method permanently reverts to
                // interpreter-only execution for the rest of the process (i.e.
                // today's status quo for that one method), while the
                // overwhelmingly common case — a dead `assert cond : "msg" +
                // var;` branch — never takes the trap and the surrounding hot
                // method compiles and runs at full JIT speed.
                //
                // Only the STACK EFFECT is modeled here (pop the call's args,
                // push a placeholder result of the correct kind) so the
                // compiler's simulated operand stack stays consistent for
                // whatever bytecode follows the (unreachable, but still
                // compiled) invokedynamic — e.g. the assert-message pattern's
                // `invokespecial AssertionError.<init>` + `athrow`.
                0xba => {
                    // O(1) pc-indexed lookup — see `indy_info` field doc.
                    let info = self
                        .indy_info_idx
                        .get(&pc)
                        .map(|&i| self.indy_info[i].clone());
                    let Some((_pc, arg_slots, ret_type, arg_type_tags, bridge_site)) = info else {
                        // No resolver, or this site couldn't be resolved at
                        // compile time: fail safe and bail the whole method,
                        // exactly like every other CP-resolved metadata miss
                        // in this backend (see 0x12/0x13 above) — never emit
                        // unsound code for an unresolvable call site.
                        return false;
                    };

                    self.flush_scratch_registers();

                    // A BRIDGED site has a resolved, process-lifetime
                    // handler, so call it directly instead of taking the
                    // uncommon trap below. This is what removes RBC.7's premise
                    // for the common `println("..." + x)`-after-a-loop shape:
                    // with no trap at the indy bci there is nothing for an OSR
                    // frame to resume imprecisely, so the OSR artifact keeps
                    // running.
                    //
                    // The bridged set is `StringConcatFactory` and, since
                    // 2026-08-23, `LambdaMetafactory`, `SwitchBootstraps` and
                    // `ObjectMethods` — every bootstrap whose implementation
                    // reaches the frame only through its operand stack and its
                    // class id, which is all a bridge can offer. Between them
                    // they cover a lambda or method reference, a
                    // pattern-matching `switch`, and a record's
                    // `equals`/`hashCode`/`toString`. The lambda one is what
                    // lets a method that CREATES a lambda stay compiled at all:
                    // before it, such a method took this trap on its first
                    // execution and was retired with `MakeNotCompilable`, which
                    // on Reactor/WebFlux assembly means essentially nothing is
                    // ever compiled. The site pointer is self-describing (its
                    // `kind` tag), so ONE call sequence and one entry serve
                    // both. Every other bootstrap kind still falls through to
                    // the trap.
                    let bridge_entry =
                        crate::INDY_BRIDGE_FN.load(std::sync::atomic::Ordering::Relaxed);
                    if bridge_site != 0 && bridge_entry != 0 && ret_type != b'V' {
                        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JITC").is_some() {
                            eprintln!("[cratonvm-jitc] indy bridge pc={} args={}", pc, arg_slots);
                        }
                        let pre_pop_spill = self.next_spill_offset;
                        // `pop_invoke_args`, not a bare `pop_stack` loop: it
                        // also hands back the per-argument OOP MARKS, and the
                        // staged buffer below is the only thing holding those
                        // references across a call that runs a bootstrap and
                        // allocates. Without `pending_staged_arg_oops` the
                        // safepoint map does not name them, which for a
                        // capturing lambda means every captured object is
                        // invisible to a collection that happens inside its own
                        // creation. The concat bridge this arm grew out of had
                        // the same gap and never showed it, because a
                        // `StringConcatFactory` argument is read into a Rust
                        // `String` before anything can allocate.
                        let (arg_slots_vec, arg_oops) = self.pop_invoke_args(arg_slots);
                        let post_pop_spill = self.next_spill_offset;
                        if arg_slots > 0 {
                            let Some(args_end) =
                                self.checked_spill_range_end(pre_pop_spill, arg_slots)
                            else {
                                if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JITC")
                                    .is_some()
                                {
                                    eprintln!(
                                        "[cratonvm-jitc] indy bridge spill overflow pc={} base={} args={}",
                                        pc, pre_pop_spill, arg_slots
                                    );
                                }
                                return false;
                            };
                            self.next_spill_offset = args_end;
                            for (i, slot) in arg_slots_vec.iter().enumerate() {
                                let offset = pre_pop_spill + ((arg_slots - 1 - i) as i32) * 8;
                                self.load_slot_to_reg(RAX, *slot);
                                self.emit_store_local(offset, RAX);
                                if arg_oops[i] {
                                    self.pending_staged_arg_oops.push(offset);
                                }
                            }
                        }
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.emit_mov_imm64(ARG_REGS[1], bridge_site as i64);
                        if arg_slots > 0 {
                            self.emit_lea_frame_slot(
                                ARG_REGS[2],
                                pre_pop_spill + ((arg_slots as i32) - 1) * 8,
                            );
                        } else {
                            self.emit_xor_reg_self(ARG_REGS[2]);
                        }
                        self.emit_mov_imm32_sx(ARG_REGS[3], arg_slots as i32);
                        self.emit_pre_safepoint_spill();
                        self.emit_call_absolute(bridge_entry);
                        self.emit_oop_map_for_safepoint();
                        self.next_spill_offset = post_pop_spill;
                        // The bridge is FALLIBLE, and the `LambdaMetafactory`
                        // half is fallible in a way the concat half is not: a
                        // bootstrap can raise `BootstrapMethodError` /
                        // `LambdaConversionError`, and the VM-side entry stashes
                        // that and returns the `i64::MIN` deopt sentinel. Route
                        // it through the same shared stub every other fallible
                        // helper call uses; without it the sentinel bits would
                        // be pushed AS AN OBJECT REFERENCE and dereferenced by
                        // whatever consumes the result.
                        //
                        // Added with the generic bridge rather than before it
                        // because the concat entry's only failure answer is `0`,
                        // i.e. a null `String` — wrong, but not a wild pointer.
                        self.emit_post_invoke_exception_check(ret_type);
                        // Pushed by the DESCRIPTOR, the same three-way choice
                        // the invoke lowering above makes: `xmm0` for `D`/`F`,
                        // a plain slot otherwise, oop-marked for `L`/`[`. A
                        // `V` site never reaches here — it is refused at
                        // admission, because a void bridge has no push for
                        // this to model.
                        if matches!(ret_type, b'D' | b'F') {
                            self.push_from_rax_as_xmm0();
                        } else {
                            self.push_from_rax();
                        }
                        if matches!(ret_type, b'L' | b'[') {
                            self.mark_top_as_oop();
                        }
                        pc += 5;
                        continue;
                    }

                    // Unconditional JMP to the shared deopt stub. Mirrors the
                    // conditional String-intrinsic bail edges elsewhere in
                    // this file (`emit_jcc_rel32_patch` + `deopt_stubs.push`),
                    // but unconditional (`emit_jmp_rel32_patch`) since this
                    // instruction is NEVER taken on the JIT-compiled path.
                    // FIX (silent data corruption, HHH-15895 InPredicateTest /
                    // AccumRepro3 residual): the fallback for this trap when no
                    // precise snapshot exists ("safe reject" in the VM's
                    // `try_osr()`) can only rewind execution to the method's OSR
                    // entry bci, discarding every side effect committed by
                    // JIT-compiled code between OSR entry and this trap — this
                    // instruction can be reached arbitrarily late in a method
                    // (e.g. inside a `println` well after earlier loops/mutations
                    // already ran to completion), so "rewind to entry" silently
                    // re-executes or drops already-committed work. Unlike the
                    // experimental speculative-guard snapshots elsewhere in this
                    // file (gated behind `deopt_real_enabled()`), this one is
                    // unconditional: `emit_deopt_stubs` always uses it for reason
                    // 8 (see the matching fix note there), because the imprecise
                    // fallback here has PROVEN silent corruption risk, not just
                    // performance cost. Reuses the existing OSR-exit snapshot
                    // machinery (frame reconstruction from live registers/spill
                    // slots) so the VM can resume precisely at THIS bci instead of
                    // rewinding. Tagged `UnreachedCode` (the trap's true
                    // reason) so the resume sinks' de-speculation applies the
                    // give-up-immediately policy — see
                    // `emit_osr_exit_map_at_reason`.
                    //
                    // deopt-osr indy-arg-types fix: record this call's own
                    // per-argument type tags BEFORE the snapshot is built, so
                    // the operand-stack loop can precisely type the top
                    // `arg_type_tags.len()` stack entries instead of falling
                    // back to the coarse `wide_fp` gate — see
                    // `indy_stack_arg_types`'s doc comment.
                    if !arg_type_tags.is_empty() {
                        self.indy_stack_arg_types.insert(pc, arg_type_tags.clone());
                    }
                    self.emit_osr_exit_map_at_reason(pc, crate::deopt::DeoptReason::UnreachedCode);

                    // This trap is UNCONDITIONAL: every execution of this bci
                    // deopts. So if the snapshot just built cannot be
                    // materialised back into an interpreter frame, the method
                    // is guaranteed to fail on its first compiled call --
                    // `build_deopt_frame_inner` returns `None` and the resume
                    // sink refuses with `precise deoptimization unavailable
                    // ... refusing side-effecting replay`, a hard
                    // `InternalError` rather than a slow path.
                    //
                    // The usual producer of an unmaterialisable slot here is
                    // the coarse `wide_fp` gate in the snapshot's operand-stack
                    // loop: in a method that touches any long/float/double, a
                    // non-oop stack entry that is NOT one of this call site own
                    // arguments has no per-entry width source and is recorded
                    // `Unsupported`. javac `ClassReader.readInnerClasses` is
                    // the canonical shape -- `optPoolEntry(int, IntFunction,
                    // Object)` leaves an `int` underneath the lambda argument,
                    // so the indy-arg tags type the top entry but not that one.
                    //
                    // Compiling such a method is strictly worse than
                    // interpreting it, so bail the whole compile. This is what
                    // the per-method SPRING-TESTCOMPILER / HIB-STOREDPROC-JIT
                    // bans did by hand for the javac family; deciding it from
                    // the snapshot itself covers every method with this shape
                    // rather than the ones somebody happened to hit.
                    let unresumable_trap = self
                        .deopt_points
                        .last()
                        .is_some_and(|p| !crate::deopt::frame_state_is_resumable(&p.frame_state));
                    if unresumable_trap {
                        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JITC").is_some() {
                            eprintln!(
                                "[cratonvm-jitc] compile-bail unresumable-indy-trap bci={pc}"
                            );
                        }
                        self.buf.mark_codegen_unencodable("unresumable-indy-trap");
                    }

                    let patch = self.emit_jmp_rel32_patch();
                    self.deopt_stubs.push((patch, pc, 8)); // 8 = DEOPT_REASON_UNREACHED_CODE

                    // Stack-effect-only bookkeeping for subsequent (unreachable
                    // but still-compiled) bytecode: pop the call's arguments —
                    // popping is type-agnostic, so only the count matters —
                    // then push a single placeholder of the correct STACK-SLOT
                    // KIND for the descriptor's return type (control never
                    // reaches past the trap above, so the placeholder's actual
                    // bit-pattern is irrelevant; only its kind must match what
                    // downstream codegen expects).
                    for _ in 0..arg_slots {
                        self.pop_stack();
                    }
                    match ret_type {
                        b'V' => {}
                        b'F' | b'D' => {
                            self.emit_xor_reg_self(RAX);
                            self.push_from_rax_as_xmm0();
                        }
                        b'L' | b'[' => {
                            self.emit_xor_reg_self(RAX);
                            self.push_from_rax();
                            self.mark_top_as_oop();
                        }
                        _ => {
                            // int / long / short / byte / char / boolean
                            self.emit_xor_reg_self(RAX);
                            self.push_from_rax();
                        }
                    }
                    pc += 5;
                }

                // newarray — allocate a new primitive array
                0xbc => {
                    self.flush_scratch_registers();
                    let atype = code[pc + 1] as i32; // Widening: always safe
                    let count_slot = self.pop_stack();
                    // Load heap pointer → ARG_REGS[0]
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                    // atype immediate → ARG_REGS[1]
                    self.emit_mov_imm32_sx(ARG_REGS[1], atype);
                    // count → ARG_REGS[2]
                    self.load_slot_to_reg(ARG_REGS[2], count_slot);
                    // Round-8 wave-3: defensive callee-saved spill
                    // before any GC-triggering CALL.
                    self.emit_pre_safepoint_spill();
                    self.emit_call_absolute(self.helpers.newarray);
                    // T1.1.a — `newarray` is a GC-triggering safepoint.
                    self.emit_oop_map_for_safepoint();
                    // Heap-exhaustion guard: a null result means OOM (the helper
                    // stashed an OutOfMemoryError). Bail before the null is pushed
                    // and dereferenced by a following `arraylength`/store.
                    self.emit_post_alloc_oom_check();
                    self.push_from_rax();
                    // A primitive array header is still an object reference.
                    self.mark_top_as_oop();
                    pc += 2;
                }

                // new — allocate a new Java object (heap allocation via helper)
                // Scalar replacement: if the object doesn't escape, store fields
                // in the JIT frame instead of heap-allocating.
                0xbb => {
                    if let Some(sr_obj) = self.scalar_replaced.get(&pc).cloned() {
                        // Scalar-replaced: zero-initialize field slots in the frame
                        self.emit_xor_reg_self(RAX);
                        for i in 0..sr_obj.num_fields {
                            let field_off =
                                sr_obj.field_base_offset + (i as i32) * (SLOT_SIZE as i32); // Cast: x86-64 immediate encoding
                            self.emit_store_local(field_off, RAX);
                            self.emit_store_local(field_off + 8, RAX);
                        }
                        // Push a dummy zero "object reference" — never dereferenced
                        self.push_from_rax();
                        pc += 3;
                    } else {
                        self.flush_scratch_registers();
                        // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                        let resolved = self.new_info_idx.get(&pc).map(|&i| self.new_info[i]);
                        let (_, class_id_raw, num_fields, has_prim_init, has_finalizer) =
                            match resolved {
                                Some(info) => info,
                                None => {
                                    // Not compile-time resolvable: the target
                                    // class was not loaded when this method was
                                    // compiled (the cold `throw new
                                    // SomeException(...)` shape). Emit the
                                    // CP-indexed helper, which resolves +
                                    // initialises + allocates at run time, the
                                    // way the interpreter's 0xbb handler does.
                                    // No compile-time class knowledge exists
                                    // here, so neither the inline TLAB bump nor
                                    // scalar replacement applies — this site is
                                    // always the helper call, which is exactly
                                    // right for a branch that is (by hypothesis)
                                    // cold.
                                    let Some(&i) = self.new_deferred_idx.get(&pc) else {
                                        return false;
                                    };
                                    let (_, holder_class_id, cp_idx) = self.new_deferred_info[i];
                                    if self.helpers.new_object_cp == 0 || !self.needs_heap {
                                        return false;
                                    }
                                    self.emit_pre_safepoint_spill();
                                    crate::runtime_lowering::emit_new_object_cp_stub(
                                        &mut self.buf,
                                        self.heap_local_offset,
                                        self.helpers.new_object_cp,
                                        holder_class_id,
                                        cp_idx,
                                        self.helpers.frame_record,
                                    );
                                    // Same post-call contract as the resolved
                                    // arm below: GC-triggering safepoint, then
                                    // the 0/null sentinel guard (the helper
                                    // reports a failed class resolution,
                                    // `<clinit>` failure or OOM by stashing a
                                    // pending exception and returning 0).
                                    self.emit_oop_map_for_safepoint();
                                    self.emit_post_alloc_oom_check();
                                    self.push_from_rax();
                                    self.mark_top_as_oop();
                                    pc += 3;
                                    continue;
                                }
                            };

                        // HIGH-6 JIT audit (object_allocation/1000 3-5x gap):
                        // emit an inline TLAB bump-pointer fast path when the
                        // helper table is fully wired AND the class is small
                        // enough to bump in a single LEA disp32 (< 256 bytes
                        // total, which covers HashMap.Node, ArrayList$Itr,
                        // and ~99% of common allocation sites). Larger
                        // objects and the test-helper path (no `get_current_thread`
                        // wired) fall through to the unconditional helper call.
                        //
                        // The inline path bumps `thread.tlab.cursor`, writes
                        // `class_id` at obj_ptr+0, then tail-calls
                        // `jit_post_tlab_init` to finish header + primitive
                        // defaults + finalizer registration. The bump itself
                        // is ~6 instructions; HotSpot achieves ~5-7. The
                        // remaining work (hash mint, num_slots write, class-
                        // metadata RwLock for primitive defaults) is in the
                        // post-init helper — kept out of inline because
                        // synthesising it would require per-field descriptor
                        // plumbing that isn't currently in `new_info`.
                        let total_size = HEADER_SIZE + num_fields * SLOT_SIZE;
                        // `total_size` here is the legacy upper bound used only
                        // for the <=256 inline-eligibility gate; the compact body
                        // is smaller, so a legacy fit implies a compact fit.
                        // `emit_inline_tlab_new` computes the real compact size +
                        // writes array_length/GC_FLAG_COMPACT inline.
                        // The pure inline path publishes a complete canonical
                        // header before advancing the TLAB cursor and cannot
                        // call into GC. Enable that safe subset by default.
                        // Body clearing makes int-family typed zeroes safe in
                        // the pure inline path. Sites requiring non-zero Value
                        // tags (long/float/double) or finalizer registration
                        // retain the helper path unless explicitly opted in.
                        //
                        // (Restored 2026-07-14: merge 96a1d0c2 resolved this
                        // region to the pre-0ee32e122 opt-in gate — reverting
                        // "Optimize Binary Trees allocation and recursion" and
                        // regressing bt18 1.9s→6.0s. The old "not yet safe
                        // with the precise moving young collector" rationale
                        // belonged to the pre-redesign inline path; the
                        // current one completes the header before the cursor
                        // advance, which is what made default-on safe.)
                        let skip_helper = !has_prim_init && !has_finalizer;
                        let can_inline = cratonvm_types::flags::runtime_var_os(
                            "CRATONVM_JIT_DISABLE_INLINE_NEW",
                        )
                        .is_none()
                            && (skip_helper
                                || cratonvm_types::flags::runtime_var_os(
                                    "CRATONVM_JIT_ENABLE_INLINE_NEW",
                                )
                                .is_some())
                            && self.helpers.get_current_thread != 0
                            && self.helpers.tlab_post_init != 0
                            && self.helpers.new_object != 0
                            && total_size <= 256
                            && self.needs_heap; // need vm_ptr in heap_local slot

                        // Round-8 wave-3: defensive callee-saved spill
                        // before the `new` safepoint (both inline TLAB
                        // and slow-path helper can reach GC via
                        // jit_post_tlab_init / new_object).
                        //
                        // ...but only the paths that CALL one of those can. With
                        // `skip_helper` the inline arm emits no call at all
                        // between here and the merge point, so no collector can
                        // observe this frame on it, and the blind full-GPR half
                        // of the spill is dead there. Ask for it to be sunk onto
                        // the allocation's slow-path edges instead; the request
                        // degrades to the full spill if the safepoint emitter
                        // cannot honour it. `inline_tlab_new_enabled()` is part
                        // of the condition because its opt-out turns the inline
                        // arm back into a bare `new_object` call.
                        self.sink_alloc_blind_spill = alloc_spill_sink_enabled()
                            && can_inline
                            && skip_helper
                            && inline_tlab_new_enabled()
                            && self.jit_thread_slot_off != 0;
                        self.emit_pre_safepoint_spill();
                        if can_inline {
                            // CRIT-2 — when neither primitive-init nor
                            // finalizer registration is required, skip
                            // the `jit_post_tlab_init` helper and write
                            // identity_hash/num_slots inline. Most JDK
                            // micro-objects (HashMap.Node, ArrayList$Itr,
                            // Iterator chains, all-reference field
                            // bearers) hit this fast path. The
                            // resolution of these flags currently
                            // requires extending `cp_new_resolver` (see
                            // the `new_info` doc in `jit/src/lib.rs`),
                            // so the conservative default `(true,true)`
                            // keeps the helper call in place for now.
                            self.emit_inline_tlab_new(class_id_raw, num_fields, skip_helper);
                        } else {
                            // Slow path: full helper-call dispatch. Used when
                            //   - the helper table is partial (tests),
                            //   - the object exceeds 256 bytes (rare —
                            //     HotSpot also bails on these),
                            //   - or the method's prologue did not stash
                            //     `vm_ptr` in a frame slot.
                            crate::runtime_lowering::emit_new_object_stub(
                                &mut self.buf,
                                self.heap_local_offset,
                                self.helpers.new_object,
                                class_id_raw,
                                num_fields,
                                self.helpers.frame_record,
                            );
                        }
                        // The withheld half of the blind spill must have been
                        // emitted by now — the only consumer is the arm above.
                        // Fail the compile closed rather than ship a safepoint
                        // whose spill is missing eleven registers: an unspilled
                        // register-resident oop is a reclaimed live object, and
                        // the cost of bailing is one interpreted method.
                        if self.deferred_alloc_blind_spill {
                            self.deferred_alloc_blind_spill = false;
                            self.fail("singlepass-codegen/alloc-spill-sink-unconsumed");
                        }
                        // T1.1.a — `new` is a GC-triggering safepoint.
                        // Emit an oop map for the slots that were live
                        // BEFORE the call (the return value hasn't
                        // been pushed yet, so the stack state here
                        // reflects the surviving operands). Both arms
                        // (inline and slow) may trigger GC: the inline
                        // path's post-init helper can grow the
                        // finalizer queue and the slow path obviously
                        // can young-GC.
                        self.emit_oop_map_for_safepoint();
                        // Heap-exhaustion guard: both the inline-TLAB slow path
                        // and the slow-path helper return the 0/null sentinel on
                        // OOM (jit_new_object -> jit_alloc_oom). Bail before the
                        // null is pushed and dereferenced by a following
                        // getfield/putfield. (Scalar-replaced `new` never reaches
                        // here, so its dummy-zero push is unaffected.)
                        self.emit_post_alloc_oom_check();
                        self.push_from_rax();
                        // The result is an object reference.
                        self.mark_top_as_oop();
                        pc += 3;
                    }
                }

                // anewarray — allocate a new reference array via helper
                0xbd => {
                    self.flush_scratch_registers();
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let resolved = self
                        .anewarray_info_idx
                        .get(&pc)
                        .map(|&i| self.anewarray_info[i]);
                    let (_, component_class_id_raw) = match resolved {
                        Some(info) => info,
                        None => {
                            // Component class not loaded at compile time — the
                            // `anewarray` sibling of the deferred `new` arm
                            // above. Emit the CP-indexed helper, which resolves
                            // the component class at run time and then does
                            // exactly what `jit_anewarray_object` does.
                            let Some(&i) = self.anewarray_deferred_idx.get(&pc) else {
                                return false; // genuinely unresolvable — bail
                            };
                            let (_, holder_class_id, cp_idx) = self.anewarray_deferred_info[i];
                            if self.helpers.anewarray_object_cp == 0 || !self.needs_heap {
                                return false;
                            }
                            let count_slot = self.pop_stack();
                            // jit_anewarray_object_cp(vm, holder_class_id, cp_idx, length)
                            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                            self.emit_mov_imm32_sx(ARG_REGS[1], holder_class_id as i32); // Cast: x86-64 immediate encoding
                            self.emit_mov_imm32_sx(ARG_REGS[2], cp_idx as i32); // Cast: x86-64 immediate encoding
                            self.load_slot_to_reg(ARG_REGS[3], count_slot);
                            self.emit_pre_safepoint_spill();
                            self.emit_call_absolute(self.helpers.anewarray_object_cp);
                            // Resolution can run a user `ClassLoader.loadClass`,
                            // i.e. arbitrary Java on this thread — republish the
                            // frame afterwards exactly as `emit_new_object_stub`
                            // does for the `new` side.
                            crate::runtime_lowering::emit_post_call_frame_republish(
                                &mut self.buf,
                                self.helpers.frame_record,
                            );
                            self.emit_oop_map_for_safepoint();
                            self.emit_post_alloc_oom_check();
                            self.push_from_rax();
                            self.mark_top_as_oop();
                            pc += 3;
                            continue;
                        }
                    };
                    let count_slot = self.pop_stack();
                    // jit_anewarray_object(heap, component_class_id_raw, length) → i64 array ptr
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                    self.emit_mov_imm32_sx(ARG_REGS[1], component_class_id_raw as i32); // Cast: x86-64 immediate encoding
                    self.load_slot_to_reg(ARG_REGS[2], count_slot);
                    // Round-8 wave-3: defensive callee-saved spill
                    // before any GC-triggering CALL.
                    self.emit_pre_safepoint_spill();
                    self.emit_call_absolute(self.helpers.anewarray_object);
                    // T1.1.a — `anewarray` is a GC-triggering safepoint.
                    self.emit_oop_map_for_safepoint();
                    // Heap-exhaustion / negative-length guard: jit_anewarray_object
                    // returns the 0/null sentinel on OOM (-> jit_alloc_oom) or on
                    // a negative length (-> jit_negative_array_size). Bail before
                    // the null is pushed and dereferenced.
                    self.emit_post_alloc_oom_check();
                    self.push_from_rax();
                    // The result is a reference array — an object reference.
                    self.mark_top_as_oop();
                    pc += 3;
                }

                // arraylength — get array length from header (inline)
                0xbe => {
                    let arr_slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, arr_slot);
                    // NPE on null array (JVMS §arraylength). Without this
                    // guard `emit_arraylength_regs` does a raw
                    // `MOV EAX, [RAX + ARRAY_LENGTH_OFFSET]` which faults on
                    // a null receiver. Observed in Tomcat: a JIT-compiled
                    // `String.length()` invoked with a null `this` reads its
                    // `value` byte[] field (helper safely yields 0), then
                    // `arraylength` on the null array SIGSEGV'd the VM
                    // instead of throwing NPE. Mirrors the round-9 inline
                    // null check that loads/stores already carry; the
                    // dedicated `emit_null_check_arraylength` always emits
                    // the TEST/JZ since the `_at` dataflow-elision helper
                    // parses a load/store bytecode shape that arraylength
                    // (no index push) does not match.
                    self.emit_null_check_arraylength(code, pc);
                    self.emit_arraylength_regs();
                    self.push_from_rax();
                    pc += 1;
                }

                // if_acmpeq (0xa5) — reference equality branch
                0xa5 => {
                    self.flush_scratch_registers();
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32; // Widening: always safe
                    let target_pc = match pc.checked_add_signed(offset as isize) {
                        // Cast: address arithmetic
                        Some(t) => t,
                        None => return false, // invalid branch target
                    };
                    if target_pc <= pc {
                        self.emit_safepoint_poll();
                    }

                    // Canonicalize for forward merge points BEFORE popping the
                    // operands (see ifeq..ifle). if_acmp historically skipped
                    // both the canonicalization and the depth record, so a
                    // taken edge with a non-empty remaining stack reached a
                    // merge whose layout the two paths never agreed on
                    // (surfacing as a simulated-stack underflow that bailed
                    // the whole method to the interpreter).
                    if target_pc > pc && self.stack.len() > 2 {
                        self.canonicalize_stack();
                    }
                    let val2 = self.pop_stack();
                    let val1 = self.pop_stack();
                    self.load_slot_to_reg(RCX, val2);
                    self.load_slot_to_reg(RAX, val1);
                    // CMP RAX, RCX (REX.W + 0x39 /r)
                    self.rex_w();
                    self.buf.emit(&[0x39, 0xC8]); // CMP RAX, RCX

                    // JE rel32
                    self.buf.emit_byte(0x0F);
                    self.buf.emit_byte(0x84); // JE
                    let patch_offset = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                    self.forward_patches.push((patch_offset, target_pc));
                    self.record_branch_target_depth(target_pc);
                    self.reset_spills();
                    pc += 3;
                }

                // if_acmpne (0xa6) — reference inequality branch
                0xa6 => {
                    self.flush_scratch_registers();
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32; // Widening: always safe
                    let target_pc = match pc.checked_add_signed(offset as isize) {
                        // Cast: address arithmetic
                        Some(t) => t,
                        None => return false, // invalid branch target
                    };
                    if target_pc <= pc {
                        self.emit_safepoint_poll();
                    }

                    // Canonicalize for forward merge points BEFORE popping the
                    // operands (see if_acmpeq above).
                    if target_pc > pc && self.stack.len() > 2 {
                        self.canonicalize_stack();
                    }
                    let val2 = self.pop_stack();
                    let val1 = self.pop_stack();
                    self.load_slot_to_reg(RCX, val2);
                    self.load_slot_to_reg(RAX, val1);
                    // CMP RAX, RCX (REX.W + 0x39 /r)
                    self.rex_w();
                    self.buf.emit(&[0x39, 0xC8]); // CMP RAX, RCX

                    // JNE rel32
                    self.buf.emit_byte(0x0F);
                    self.buf.emit_byte(0x85); // JNE
                    let patch_offset = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                    self.forward_patches.push((patch_offset, target_pc));
                    self.record_branch_target_depth(target_pc);
                    self.reset_spills();
                    pc += 3;
                }

                // checkcast (0xc0) — type check (pass-through or exception)
                0xc0 => {
                    self.flush_scratch_registers();
                    // Look up resolved typecheck info for this PC
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let (_, name_ptr, name_len) = self
                        .typecheck_info_idx
                        .get(&pc)
                        .map(|&i| self.typecheck_info[i])
                        .unwrap_or((pc, std::ptr::null(), 0));

                    // ---- inline class-id fast path (see `checkcast_inline_enabled`) ----
                    //
                    // The target `ClassId` needs NO new plumbing: the main
                    // compile door already resolves it and interns the site's
                    // name under that identity, precisely so the runtime helper
                    // can compare ids instead of re-resolving a name, and
                    // `typecheck_target_for_site` is the public read of that
                    // table. The id and the name pointer baked into the helper
                    // call below are therefore the SAME pair — they cannot
                    // disagree about which class this site means.
                    //
                    // A `ClassId` is per-VM and that table is process-wide, but
                    // `intern_typecheck_target` keys its intern on the id, so
                    // two VMs resolving one name differently get two DISTINCT
                    // pointers and two distinct rows. The `JitCache` is a
                    // per-VM field (`vm_init`), so a body only ever runs in the
                    // VM whose compile baked the immediate.
                    //
                    // The doors that do not intern (OSR, eager first-call) get
                    // `None` here and keep the unconditional call. That is a
                    // real coverage gap and it is COUNTED rather than assumed —
                    // see `CHECKCAST_INLINE_NO_TARGET_ID`.
                    let target_class_id = if name_ptr.is_null() {
                        None
                    } else {
                        crate::typecheck_target_for_site(name_ptr)
                    }
                    .filter(|&id| id != 0);

                    // Read BEFORE the pop: `stack_oop_marks` is parallel to
                    // `stack`, so the operand's mark is the last one only while
                    // the operand is still on it. Same three clauses, read the
                    // same way, as the compact inline `getfield` arm above —
                    // whose fast path raw-dereferences these same references at
                    // a FIELD offset behind a bare null test, which is why
                    // reading the class id at offset 0 asks nothing new of them.
                    let trusted_have_key = !self.method_key.is_empty();
                    let trusted_marks_exact = self.stack_oop_marks_exact;
                    let trusted_top_is_oop = self.stack_oop_marks.last().copied().unwrap_or(false);
                    let operand_is_trusted_oop =
                        trusted_have_key && trusted_marks_exact && trusted_top_is_oop;

                    let obj_slot = self.pop_stack();

                    // The 1-D primitive-array variant. It needs NO class id —
                    // it proves its answer from the header's kind/element tags —
                    // which is the whole point, because a primitive array's
                    // class id is 0 and could never have matched.
                    // SAFETY: `name_ptr`/`name_len` are an `intern_typecheck_target`
                    // pair, leaked for the life of the process.
                    let prim_array_tag = unsafe { crate::typecheck_site_name(name_ptr, name_len) }
                        .and_then(cratonvm_types::primitive_array_kind_tags_byte)
                        .filter(|_| checkcast_inline_enabled() && operand_is_trusted_oop);
                    let inline_target = target_class_id.filter(|_| {
                        checkcast_inline_enabled()
                            && operand_is_trusted_oop
                            && prim_array_tag.is_none()
                    });
                    if checkcast_inline_enabled() {
                        use std::sync::atomic::Ordering::Relaxed;
                        // Name the refusal per CAUSE. "Not inlined" is a
                        // verdict; these two are the reasons, and they want
                        // opposite fixes.
                        if prim_array_tag.is_some() {
                            crate::CHECKCAST_INLINE_SITES_PRIM_ARRAY.fetch_add(1, Relaxed);
                        } else if inline_target.is_some() {
                            crate::CHECKCAST_INLINE_SITES.fetch_add(1, Relaxed);
                        } else if target_class_id.is_none() {
                            crate::CHECKCAST_INLINE_NO_TARGET_ID.fetch_add(1, Relaxed);
                        } else {
                            crate::CHECKCAST_INLINE_UNTRUSTED.fetch_add(1, Relaxed);
                        }
                        if inline_target.is_none()
                            && prim_array_tag.is_none()
                            && cratonvm_types::flags::runtime_var_os(
                                "CRATONVM_DBG_CHECKCAST_INLINE",
                            )
                            .is_some()
                        {
                            eprintln!(
                                "[checkcast-inline] pc={pc} REFUSED target_id={target_class_id:?} have_key={trusted_have_key} marks_exact={trusted_marks_exact} top_is_oop={trusted_top_is_oop} method={}",
                                self.method_key,
                            );
                        }
                    }
                    let mut hit_patches: Vec<usize> = Vec::new();
                    if let Some(tag) = prim_array_tag {
                        // ONE comparison settles a 1-D primitive array: the
                        // KIND_TAGS byte is `kind | (element_type << 2)` with
                        // bits 6..7 reserved zero, so `byte[]` is exactly 0x21
                        // and nothing else is. No null test is needed BEFORE it
                        // — but the load would fault on null, so it still comes
                        // first.
                        self.load_slot_to_reg(RAX, obj_slot);
                        let mut slow = self.emit_trusted_oop_receiver_check();
                        self.buf.emit(&[
                            0x80,
                            0x78,
                            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                            tag,
                        ]);
                        hit_patches.push(self.emit_jcc_rel32_patch(0x84)); // JE → done
                        for p in slow.drain(..) {
                            self.patch_rel32_to_here(p);
                        }
                    } else if let Some(target_class_id) = inline_target {
                        self.load_slot_to_reg(RAX, obj_slot);
                        // Null is a LEGAL cast and the helper already returns 0
                        // for it, so null goes to the helper rather than growing
                        // a second null path here. `emit_trusted_oop_receiver_check`
                        // is that bare `TEST/JZ`, shared with the `getfield` arm.
                        // ARRAY RECEIVERS MUST NOT REACH THE COMPARE.
                        // `regression-suite` `RJitArrayTypecheck` caught this
                        // version of the fix before it left the branch, and the
                        // vector it caught it with is the one written for
                        // BUG-JIT-ARRAY-INSTANCEOF-20260726 — the same defect,
                        // reintroduced by the same reasoning.
                        //
                        // An array's header does NOT hold its own class id: a
                        // reference array holds its COMPONENT's, and a primitive
                        // array holds 0 (it has no class entry at all). So
                        // `checkcast java/lang/String` on a `String[]` receiver
                        // finds `String`'s id in the header, matches the baked
                        // target, and accepts a cast that must throw. That is
                        // exactly why the runtime helper screens its own
                        // recorded-id shortcut with `!recv_is_array` and answers
                        // arrays from `array_descriptor_of` instead.
                        //
                        // One instruction settles it: a plain object's KIND_TAGS
                        // byte is zero — `ObjectKind::Object` and
                        // `ArrayElementType::Reference` are both 0, pinned by
                        // `const _: () = assert!` in `heap_types.rs` precisely so
                        // JIT guards may do this. Arrays take the helper, which
                        // is authoritative for them.
                        let mut slow = self.emit_trusted_oop_receiver_check();
                        // CMP BYTE [RAX+KIND_TAGS_BYTE_OFFSET], 0 (80 /7 ib,
                        // ModRM 0x78 = mod01 disp8 /7 rm=RAX).
                        self.buf.emit(&[
                            0x80,
                            0x78,
                            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                            0x00,
                        ]);
                        slow.push(self.emit_jcc_rel32_patch(0x85)); // JNE → helper
                                                                    // CMP DWORD [RAX+class_id_off], target_class_id.
                                                                    // 81 /7 id with ModRM 0xB8 = mod10 (disp32) /7 rm=RAX —
                                                                    // the disp32 twin of the `0x81, 0x78` (disp8) form the
                                                                    // guarded-virtual inline arm emits.
                        let cid_off = self.helpers.class_id_offset_in_obj as i32; // Cast: x86-64 disp32
                        let cid_off_bytes = cid_off.to_le_bytes();
                        let target_bytes = target_class_id.to_le_bytes();
                        self.buf.emit(&[0x81, 0xB8]);
                        self.buf.emit(&cid_off_bytes);
                        self.buf.emit(&target_bytes);
                        // Equal ⇒ the receiver IS the target class ⇒ the cast
                        // succeeds and its result is the reference we already
                        // hold, which is exactly what the helper would return.
                        hit_patches.push(self.emit_jcc_rel32_patch(0x84)); // JE → done
                        for p in slow.drain(..) {
                            self.patch_rel32_to_here(p);
                        }
                    }

                    // Call jit_checkcast(vm_ptr, obj_ptr, class_name_ptr, class_name_len) → obj_ptr
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset); // vm_ptr
                    self.load_slot_to_reg(ARG_REGS[1], obj_slot); // obj_ptr
                    self.emit_mov_imm64(ARG_REGS[2], name_ptr as i64); // class_name_ptr // Cast: JIT ABI convention
                    self.emit_mov_imm64(ARG_REGS[3], name_len as i64); // class_name_len // Cast: JIT ABI convention
                                                                       // Round-8 wave-3: defensive callee-saved spill
                                                                       // before any GC-triggering CALL.
                    self.emit_pre_safepoint_spill();
                    self.emit_call_absolute(self.helpers.checkcast);
                    // T1.1.2 — checkcast may resolve the target class
                    // on demand (first access) which allocates a
                    // `java/lang/Class` mirror. That's a GC-triggering
                    // safepoint — emit the oop map before pushing.
                    self.emit_oop_map_for_safepoint();
                    // Residual-6 companion fix: a definitively-failed cast now
                    // stashes a ClassCastException and returns the i64::MIN
                    // sentinel (see `jit_checkcast`) instead of a silent null.
                    // Route the sentinel through the shared exception stub like
                    // every other fallible helper; `emitted_checkcast_throw`
                    // forces `has_dispatch` so the helper has the JIT_THREAD
                    // TLS it needs to construct the CCE and the entry path
                    // drains the pending exception.
                    self.emit_post_invoke_exception_check(b'L');
                    self.emitted_checkcast_throw = true;
                    // Join. The fast path jumps here with the receiver still in
                    // RAX — the same value the helper returns on a hit — having
                    // skipped the blind spill, the safepoint's oop map and the
                    // exception check, none of which a path that makes no call
                    // and cannot throw is owed.
                    for p in hit_patches {
                        self.patch_rel32_to_here(p);
                    }
                    // Result (obj_ptr or 0 for null) is in RAX — push onto stack
                    self.push_from_rax();
                    // checkcast returns the same reference (or null).
                    self.mark_top_as_oop();
                    pc += 3;
                }

                // instanceof (0xc1) — type check (returns 0 or 1)
                0xc1 => {
                    self.flush_scratch_registers();
                    // Look up resolved typecheck info for this PC
                    // MED-4 / Fix 3 — O(1) pc-indexed lookup.
                    let (_, name_ptr, name_len) = self
                        .typecheck_info_idx
                        .get(&pc)
                        .map(|&i| self.typecheck_info[i])
                        .unwrap_or((pc, std::ptr::null(), 0));

                    let obj_slot = self.pop_stack();

                    // Call jit_instanceof(vm_ptr, obj_ptr, class_name_ptr, class_name_len) → 0/1
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset); // vm_ptr
                    self.load_slot_to_reg(ARG_REGS[1], obj_slot); // obj_ptr
                    self.emit_mov_imm64(ARG_REGS[2], name_ptr as i64); // class_name_ptr // Cast: JIT ABI convention
                    self.emit_mov_imm64(ARG_REGS[3], name_len as i64); // class_name_len // Cast: JIT ABI convention
                                                                       // Round-8 wave-3: defensive callee-saved spill
                                                                       // before any GC-triggering CALL.
                    self.emit_pre_safepoint_spill();
                    self.emit_call_absolute(self.helpers.instanceof_check);
                    // T1.1.2 — instanceof may resolve the target class
                    // on demand, allocating a Class mirror. Emit the
                    // oop map even though the return value is a
                    // primitive int.
                    self.emit_oop_map_for_safepoint();
                    // Result (0 or 1) is in RAX — push onto stack
                    self.push_from_rax();
                    pc += 3;
                }

                // multianewarray — allocate multi-dimensional array (2D only)
                0xc5 => {
                    self.flush_scratch_registers();
                    let _cp_hi = code[pc + 1];
                    let _cp_lo = code[pc + 2];
                    let ndims = code[pc + 3] as usize; // Widening: always safe
                    debug_assert_eq!(ndims, 2);

                    // Pop dimensions: top of stack = last dimension
                    let dim2_slot = self.pop_stack(); // inner dimension
                    let dim1_slot = self.pop_stack(); // outer dimension

                    // The packed `(holder_class_id | cp_idx << 32)` site
                    // descriptor for this pc. The helper resolves the array
                    // class from it at run time, loader-faithfully, through the
                    // same `interpreter::multianewarray_alloc` the interpreter
                    // uses — so both tiers stamp the same component classes
                    // into the allocated levels.
                    //
                    // This used to be a bare leaf element-type code, which
                    // carried no class at all; the helper then allocated every
                    // level with `ClassId(0)` and a compiled `new String[a][b]`
                    // came back as `[Ljava.lang.Object;`. A site with no entry
                    // cannot be compiled correctly at all now (there is no
                    // "default" array class), so bail rather than emit a call
                    // that would allocate the wrong type.
                    let Some(&(_, site)) = self.multianewarray_info.iter().find(|(p, _)| *p == pc)
                    else {
                        return false;
                    };

                    // Call jit_multianewarray_2d(heap_ptr, site, dim1, dim2)
                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                    self.emit_mov_imm64(ARG_REGS[1], site);
                    self.load_slot_to_reg(ARG_REGS[2], dim1_slot);
                    self.load_slot_to_reg(ARG_REGS[3], dim2_slot);
                    // Round-8 wave-3: defensive callee-saved spill
                    // before any GC-triggering CALL.
                    self.emit_pre_safepoint_spill();
                    self.emit_call_absolute(self.helpers.multianewarray_2d);
                    // Resolution can run a user `ClassLoader.loadClass`, i.e.
                    // arbitrary Java on this thread — republish the frame
                    // afterwards exactly as the `new`/`anewarray` CP-indexed
                    // arms do.
                    crate::runtime_lowering::emit_post_call_frame_republish(
                        &mut self.buf,
                        self.helpers.frame_record,
                    );
                    // T1.1.2 — multianewarray is a GC-triggering safepoint.
                    self.emit_oop_map_for_safepoint();
                    // Negative dimension / OOM / failed resolution all come back
                    // as the 0/null sentinel with a pending exception; bail into
                    // the method's exception table instead of pushing the null
                    // and dereferencing it.
                    self.emit_post_alloc_oom_check();
                    self.push_from_rax();
                    // The result is a reference array.
                    self.mark_top_as_oop();
                    pc += 4;
                }

                // ifnull (0xc6) — branch if reference is null
                0xc6 => {
                    self.flush_scratch_registers();
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32; // Widening: always safe
                    let target_pc = match pc.checked_add_signed(offset as isize) {
                        // Cast: address arithmetic
                        Some(t) => t,
                        None => return false, // invalid branch target
                    };
                    if target_pc <= pc {
                        self.emit_safepoint_poll();
                    }

                    // Canonicalize for forward merge points BEFORE popping the
                    // operand (see ifeq..ifle): popping first could let the
                    // relocation of a remaining register-resident slot clobber
                    // the operand's frame slot before the TEST reads it.
                    if target_pc > pc && self.stack.len() > 1 {
                        self.canonicalize_stack();
                    }
                    let slot = self.pop_stack();
                    // HIGH-1 / Fix 1 — wire null-check elimination.
                    // If the value on top of stack came from an aload of a
                    // local that is proven non-null at this PC, the TEST
                    // can never be zero so `ifnull` is dead and the
                    // fall-through is always taken. Skip both the TEST
                    // and the JE.
                    let proven_nonnull = preceding_aload_nonnull_local(code, pc)
                        .is_some_and(|l| self.is_local_nonnull(pc, l));
                    if proven_nonnull {
                        // No-op: fall through. We still need a non-empty
                        // branch-target record so downstream merges see
                        // the expected stack depth.
                        self.record_branch_target_depth(target_pc);
                        self.reset_spills();
                        pc += 3;
                    } else {
                        self.load_slot_to_reg(RCX, slot);
                        // TEST RCX, RCX (REX.W + 0x85 /r)
                        self.rex_w();
                        self.buf.emit(&[0x85, 0xC9]); // TEST RCX, RCX

                        // JE rel32 (jump if null / zero)
                        self.buf.emit_byte(0x0F);
                        self.buf.emit_byte(0x84); // JE
                        let patch_offset = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                        self.forward_patches.push((patch_offset, target_pc));
                        self.record_branch_target_depth(target_pc);
                        self.reset_spills();
                        pc += 3;
                    }
                }

                // ifnonnull (0xc7) — branch if reference is not null
                0xc7 => {
                    self.flush_scratch_registers();
                    let offset = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32; // Widening: always safe
                    let target_pc = match pc.checked_add_signed(offset as isize) {
                        // Cast: address arithmetic
                        Some(t) => t,
                        None => return false, // invalid branch target
                    };
                    if target_pc <= pc {
                        self.emit_safepoint_poll();
                    }

                    // Canonicalize for forward merge points BEFORE popping the
                    // operand (see ifeq..ifle / ifnull above).
                    if target_pc > pc && self.stack.len() > 1 {
                        self.canonicalize_stack();
                    }
                    let slot = self.pop_stack();
                    // HIGH-1 / Fix 1 — null-check elimination. If the
                    // tested value is proven non-null, `ifnonnull` is
                    // always taken: emit an unconditional JMP rel32 and
                    // skip the TEST + Jcc pair. Saves the 3-byte TEST
                    // + 1-byte (Jcc opcode-pair high byte) for every
                    // proven site.
                    let proven_nonnull = preceding_aload_nonnull_local(code, pc)
                        .is_some_and(|l| self.is_local_nonnull(pc, l));
                    if proven_nonnull {
                        // JMP rel32 (5 bytes; patched).
                        self.buf.emit_byte(0xE9);
                        let patch_offset = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                        self.forward_patches.push((patch_offset, target_pc));
                        self.record_branch_target_depth(target_pc);
                        self.reset_spills();
                        pc += 3;
                    } else {
                        self.load_slot_to_reg(RCX, slot);
                        // TEST RCX, RCX (REX.W + 0x85 /r)
                        self.rex_w();
                        self.buf.emit(&[0x85, 0xC9]); // TEST RCX, RCX

                        // JNE rel32 (jump if not null / non-zero)
                        self.buf.emit_byte(0x0F);
                        self.buf.emit_byte(0x85); // JNE
                        let patch_offset = self.buf.pos();
                        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

                        self.forward_patches.push((patch_offset, target_pc));
                        self.record_branch_target_depth(target_pc);
                        self.reset_spills();
                        pc += 3;
                    }
                }

                // monitorenter / monitorexit: exact lock elision followed by
                // the direct thin-lock runtime stub for every live receiver.
                0xC2 | 0xC3 => {
                    if self.sr_monitor_scalar_ops.contains(&pc) {
                        // Proven scalar receiver: lock cannot be observed or
                        // contended. Phase C records its depth for deopt relock.
                        let _ = self.pop_stack();
                        pc += 1;
                        continue;
                    }

                    // The old "any scalar replacement in this method" test
                    // could elide a lock on an unrelated escaping receiver.
                    // Only the exact per-PC proof above may remove the lock.
                    let helper = if op == 0xC2 {
                        crate::MONITOR_ENTER_DIRECT_FN.load(std::sync::atomic::Ordering::Acquire)
                    } else {
                        crate::MONITOR_EXIT_DIRECT_FN.load(std::sync::atomic::Ordering::Acquire)
                    };
                    if helper == 0 || !self.needs_heap {
                        return false;
                    }
                    // Keep the receiver on the abstract stack while publishing
                    // safepoint roots; moving GC can then rewrite its shadow
                    // home during a contended enter. Pop only after the push.
                    self.flush_scratch_registers();
                    self.emit_pre_safepoint_spill();
                    let recv_slot = self.pop_stack();
                    let recv_offset = match recv_slot {
                        StackSlot::Frame(offset) => offset,
                        StackSlot::CalleeSaved(reg) | StackSlot::Scratch(reg, ..) => {
                            let Some(offset) = self.reserve_spill_slots(1) else {
                                return false;
                            };
                            self.emit_store_local(offset, reg);
                            offset
                        }
                        StackSlot::Xmm(_) => return false,
                    };
                    crate::runtime_lowering::emit_monitor_stub(
                        &mut self.buf,
                        self.heap_local_offset,
                        recv_offset,
                        helper,
                        self.helpers.frame_record,
                    );
                    self.emitted_monitor_call = true;
                    self.emit_oop_map_for_safepoint();
                    self.emit_post_invoke_exception_check(b'V');
                    pc += 1;
                }

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
    fn emit_iinc_local(&mut self, idx: usize, inc: i32) {
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
