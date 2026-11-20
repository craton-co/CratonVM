#[allow(unused_imports)]
use super::*;

impl SiteLive {
    /// `None` when the model did not converge or cannot answer live-in, so no
    /// value is site-named on a guess.
    pub(in crate::ir_lower) fn extract(
        live: &crate::regalloc::LiveModel,
        schedule: &Schedule,
        graph: &Graph,
        gp_reg_of: &[Option<u8>],
    ) -> Option<SiteLive> {
        if !live.converged || live.span.len() != schedule.blocks.len() {
            return None;
        }
        let n_blocks = schedule.blocks.len();
        let mut facts: HashMap<NodeId, (Vec<usize>, Vec<bool>)> = HashMap::new();
        for (id, reg) in gp_reg_of.iter().enumerate() {
            if reg.is_none() {
                continue;
            }
            let Some(node) = graph.nodes.get(id) else {
                continue;
            };
            if !matches!(node.ty, IrType::Int | IrType::Long) {
                continue;
            }
            // Cast: an index into the node arena is a `NodeId`.
            let nid = id as NodeId;
            let mut live_in = Vec::with_capacity(n_blocks);
            for b in 0..n_blocks {
                live_in.push(live.live_in_at(b, nid)?);
            }
            let uses = live.uses.get(id).cloned().unwrap_or_default();
            facts.insert(nid, (uses, live_in));
        }
        Some(SiteLive {
            pos_of: live.pos_of.clone(),
            span: live.span.clone(),
            block_of_pos: live.block_of_pos.clone(),
            facts,
        })
    }
}

/// [`Lowerer::site_live_at`]'s question at linear position `p`: is `v` live
/// there in the model `sl` was copied from? Defined before `p` (earlier in
/// `p`'s block, or live into it) and read STRICTLY after `p` within the block
/// (its outgoing-edge position included), or live into a successor.
pub(in crate::ir_lower) fn site_value_live_at(
    sl: &SiteLive,
    blocks: &[crate::ir_schedule::Block],
    v: NodeId,
    p: usize,
) -> bool {
    let Some((uses, live_in)) = sl.facts.get(&v) else {
        return false;
    };
    let Some(&b) = sl.block_of_pos.get(p) else {
        return false;
    };
    let Some(&(_, end)) = sl.span.get(b) else {
        return false;
    };
    let defined_before = match sl.pos_of.get(v as usize).copied().flatten() {
        Some(d) => (d < p && sl.block_of_pos.get(d) == Some(&b)) || live_in.get(b) == Some(&true),
        None => false,
    };
    if !defined_before {
        return false;
    }
    let idx = uses.partition_point(|&u| u <= p);
    let read_after = uses.get(idx).is_some_and(|&u| u <= end);
    let live_out = blocks
        .get(b)
        .is_some_and(|blk| blk.successors.iter().any(|&s| live_in.get(s) == Some(&true)));
    read_after || live_out
}

/// The values this compile's earlier attempts refused an elision of.
pub(in crate::ir_lower) fn elide_excluded_ids() -> Vec<NodeId> {
    ELIDE_RETRY.with(|r| r.borrow().excluded.clone())
}

/// `CRATONVM_JIT_IR_DEOPT_NAME_PAST_PRELOOP` (JIT round 23 wave 10, lane L7;
/// perf, **default ON**, `=0` kill): a deopt frame may name a loop value by its
/// register when every other value sharing that register lives and dies
/// before the loop (`Lowerer::preloop_exclusive_owners`), not only when the
/// value is the register's one holder in the whole method. `heapGet`'s and
/// W2c's phis share their registers with the pre-header's temporaries (the
/// predicate guards, the hoisted field reads), so no loop phi was ever
/// nameable, no phi home was ever dropped (`home_publish_skipped=0`), and
/// every back edge wrote both phi homes. With it the phi and its coalesced
/// back-edge source are named by register and their homes drop
/// (`phi_home_droppable`); a single-use value the frame-pinned residency
/// admits drops its home the same way (`value_home_droppable`). A frame
/// state a dropped home cannot be described for still refuses the compile
/// (`frame_value_for`), and the lowering retries with every home kept. Read
/// per compile; no `static`.
pub(in crate::ir_lower) fn ir_deopt_name_past_preloop_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_CHECKCAST_HIT_TO_JOIN` (JIT round 23 wave 9, lane W9-L7;
/// perf, **default ON since j27 2026-10-28**, `=0` kills; was opt-in after the
/// n23-w9a / n23-w9b A/Bs: neutral -- W2c cast
/// 1.851/1.985/1.838 ns/elem on against 1.801/2.783/1.733 off on w9b, mixed on
/// w9a): a `checkcast` whose readers were forwarded to its operand
/// (`Graph::checkcast_value_forwarded`) sends its exact-class hit and its
/// null branch straight to the arm's join, instead of through `hit_at`'s
/// `JMP join` (n23-w8g `lambda$main$1`: `cmp dword [rax],cid; je hit_at`
/// then `hit_at: jmp join`, two taken branches per element of
/// `W2cTypecheckLoops`' cast row). Nothing reads RAX after the join as the
/// cast's result, so the two paths need not agree on it. Read per site; no
/// `static`.
pub(crate) fn ir_checkcast_hit_to_join_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// `CRATONVM_JIT_IR_GETFIELD_COLD_TAIL` (JIT round 23 wave 9, lane W9-L7;
/// perf, **default ON since j27 2026-10-28**, `=0` kills; was opt-in after the
/// n23-w9a / n23-w9b A/Bs: neutral -- W2c nocast 1.498/1.769/1.312 ns/elem on against 1.404/1.769/1.300 off,
/// SegMono heapget 110/143/108 ns/op against 109/129/124): an inline field
/// read's compact arm falls straight through to its join, and its
/// legacy-layout read and checked helper go to a cold tail past the body
/// (`Lowerer::emit_deferred_getfield_tails`), where the inline form jumped
/// over both with a taken `JMP done` on every compact read (n23-w8g
/// `lambda$main$2`: `movsxd rax,[rax+8]; jmp join`, the W2c loop's one field
/// read). Only where the tail is context-free
/// (`Lowerer::getfield_cold_tail_admitted`); a colored-store ZGC read and a
/// legacy-only site keep the inline form. Read per site; no `static`.
pub(crate) fn ir_getfield_cold_tail_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// `CRATONVM_JIT_IR_GETFIELD_GUARD_NON_NULL` (JIT round 23 wave 9, lane
/// W9-L7; perf, **default ON since j27 2026-10-28**, `=0` kills; was opt-in
/// after the n23-w9a / n23-w9b A/Bs: no win --
/// W2c nocast at `=0` 1.295/1.752/1.474 ns/elem, within the default arm's
/// spread): an inline field read whose receiver a `Guard(receiver != null)`
/// earlier in the same block proved emits no null test
/// (`Lowerer::block_guard_proves_non_null`): the null-base load predicate's
/// pre-header copies. Read per site; no `static`.
pub(crate) fn ir_getfield_guard_non_null_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// (See [`ir_home_reload_forward_enabled`] above for the reload half.)
pub(in crate::ir_lower) fn ir_poll_slow_path_dead_home_clear_enabled() -> bool {
    true
}

/// Round 17 (CG16P-4 of `jit-r16-codegen-proposals-DONE-20261005.md`):
/// `CRATONVM_JIT_IR_CONST_HOME_DROP` drops the home word of every `int` /
/// `long` constant without a register ([`Lowerer::drop_const_homes`]); a
/// reader that still wants one fails the attempt, which is lowered again with
/// every home kept. **Default ON** since round 17 (`0` keeps every home),
/// measured together with [`ir_back_edge_poll_outline_enabled`]. Read per
/// compile.
pub(in crate::ir_lower) fn ir_const_home_drop_enabled() -> bool {
    true
}

/// Round 17: `CRATONVM_JIT_IR_ALU_REG_OPERAND` -- an `int`/`long` ADD, SUB,
/// AND, OR or XOR reads a register-resident second operand in place
/// ([`Lowerer::emit_alu_acc_reg`]): one `MOV` fewer per such op. Default OFF:
/// 2-3 % on the loop kernels, and `R16Bench strings` read worse with it in a
/// loaded run. Read per op.
pub(in crate::ir_lower) fn ir_alu_reg_operand_enabled() -> bool {
    false
}



/// Does the back-edge poll's slow path give the IR XMM file back? Round 11
/// wave 6 (`r11w5-ircore-back-edge-poll-evicts-fp-loop-values`).
///
/// When it does, `ir_lower_machine_model` stops modelling the poll as an XMM
/// clobber, and the inline slow path reloads, after its `CALL`, every FP value
/// the residency plan holds in a caller-saved XMM across that edge
/// (`RegResidency::poll_fp_reloads`, `Lowerer::emit_poll_fp_reloads`). The
/// reload reads the value's HOME word, which `fp_store_value`'s write-through
/// invariant keeps current, so nothing is saved and no frame word is added.
///
/// One configuration keeps the old clobber, because the reload is argued only
/// for the default shape: split residency (`CRATONVM_JIT_IR_LS_SPLITS=1`) moves
/// a value between registers per position, so "the register a value holds at
/// the edge" is no longer `reg_of`. That switch is therefore also this
/// change's kill switch. (The outlined back-edge poll, which emitted no
/// reload, was the other until its switch was deleted in round 14 wave 2.)
pub(in crate::ir_lower) fn ir_poll_preserves_fp() -> bool {
    !ir_ls_splits_enabled()
}



/// `(fall-throughs elided, block JMPs still emitted)`, process-wide.
pub fn ir_fallthrough_census() -> (u64, u64) {
    (
        FALLTHROUGHS_ELIDED.load(std::sync::atomic::Ordering::Relaxed),
        BLOCK_JMPS_EMITTED.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// Calls this tier lowered to the blind `jit_invoke_dispatch` helper:
/// `[0]` in the compiling method's own code, `[1]` inside a spliced body.
///
/// The split is the whole point. A blind dispatch in the method's own code is
/// ordinary -- an unbindable kind, a site with no inline cache. One inside a
/// SPLICED body is a contradiction: the splice exists to delete a frame, and
/// paying a name resolution for the calls left behind costs far more than the
/// frame it removed. That combination is what made `SpliceCallProbe`'s
/// optimizing body 8.5x slower than its single-pass one (~455 ms against
/// 53 ms), and it had no reading of any kind -- which is how it survived a
/// measurement campaign that concluded the two bodies were
/// throughput-neutral. With the rows produced, this row reads 0 on that probe
/// and the optimizing body is at parity.
pub(in crate::ir_lower) static IR_BLIND_DISPATCH: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

pub(in crate::ir_lower) fn note_ir_blind_dispatch(in_splice: bool) {
    IR_BLIND_DISPATCH[usize::from(in_splice)].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Also charge it to THIS compile, so the acceptance gate can price the
    // splice trade for the body in front of it. The census above is cumulative
    // across every compile in the process and cannot answer that question.
    if in_splice {
        crate::ir_evidence::note_blind_dispatch_in_splice();
    }
}

/// `(in the method's own code, inside a spliced body)`. See
/// [`IR_BLIND_DISPATCH`]; a non-zero second element is the one to act on.
pub fn ir_blind_dispatch_census() -> (u64, u64) {
    (
        IR_BLIND_DISPATCH[0].load(std::sync::atomic::Ordering::Relaxed),
        IR_BLIND_DISPATCH[1].load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// `aastore` sites this tier lowered, process-wide.
pub(in crate::ir_lower) static IR_AASTORE_LOWERED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(in crate::ir_lower) fn note_ir_aastore_lowered() {
    IR_AASTORE_LOWERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// How many `aastore` sites the optimizing tier emitted. A zero says the
/// workload compiles none in this tier, which is not the same as the arm being
/// broken -- the distinction this file has been burned by repeatedly.
pub fn ir_aastore_census() -> u64 {
    IR_AASTORE_LOWERED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The bytes of `CMP a, b` — 64-bit when `wide` — and how many of them.
///
/// A free function so the encoding can be tested against known-good vectors
/// without standing up a `Lowerer`. It is worth testing: a ModRM field swapped
/// here compares two registers that both exist, so the result is a plausible
/// wrong branch rather than a fault.
///
/// `39 /r` is `CMP r/m, r`, so the FIRST operand lands in `r/m` and the second
/// in `reg` — the opposite nesting from the mnemonic. REX.R extends the `reg`
/// field (the second operand) and REX.B the `r/m` field (the first).
pub(in crate::ir_lower) fn cmp_reg_reg_bytes(a: u8, b: u8, wide: bool) -> ([u8; 3], usize) {
    let rex = if wide { 0x48u8 } else { 0x40u8 } | (((b >= 8) as u8) << 2) | ((a >= 8) as u8);
    let modrm = 0xC0 | ((b & 7) << 3) | (a & 7);
    if rex == 0x40 {
        // No extended register and no width prefix — two bytes, exactly the
        // `CMP EAX, ECX` this replaced.
        ([0x39, modrm, 0], 2)
    } else {
        ([rex, 0x39, modrm], 3)
    }
}

/// The bytes of `CMP a, imm` — 64-bit when `wide` — and how many of them.
///
/// Group 1 `/7`, the same `ModRM.reg`-as-opcode-extension column `ADD`/`SUB`
/// live in: `83 /7 ib` sign-extends an `imm8`, `81 /7 id` an `imm32`. The
/// short form is not a nicety — a Java counted loop compares against a small
/// literal, so `imm8` is the case, and the long form is what catches the rest.
///
/// Both SIGN-EXTEND to the operand width, which is why [`Lowerer::alu_imm32`]
/// declines a constant that does not fit `i32`: such a constant cannot be
/// written as an immediate at all, and the register form is not a fallback but
/// the only correct encoding.
///
/// A free function for the same reason `cmp_reg_reg_bytes` is one: the failure
/// mode of a wrong `/digit` is another real instruction. `/7` is CMP; `/0` is
/// ADD and `/5` is SUB, both of which WRITE the register and would corrupt a
/// loop counter rather than fault.
pub(in crate::ir_lower) fn cmp_reg_imm_bytes(a: u8, imm: i32, wide: bool) -> ([u8; 7], usize) {
    // REX.B extends the r/m field, which is where the single operand goes.
    // There is no `reg` operand here — `/7` occupies that field — so REX.R is
    // never set.
    let rex = if wide { 0x48u8 } else { 0x40u8 } | u8::from(a >= 8);
    let modrm = 0xF8 | (a & 7); // mod=11, /7, r/m=a
    let mut out = [0u8; 7];
    let mut n = 0;
    if rex != 0x40 {
        out[n] = rex;
        n += 1;
    }
    match i8::try_from(imm) {
        Ok(b) => {
            out[n] = 0x83;
            out[n + 1] = modrm;
            // Cast: `i8` to its two's-complement byte, which is what the
            // sign-extending `ib` field wants.
            out[n + 2] = b as u8;
            n += 3;
        }
        Err(_) => {
            out[n] = 0x81;
            out[n + 1] = modrm;
            out[n + 2..n + 6].copy_from_slice(&imm.to_le_bytes());
            n += 6;
        }
    }
    (out, n)
}

/// The bytes of `CMP [RBP - offset], imm` — 64-bit when `wide`.
///
/// The same `/7` column as [`cmp_reg_imm_bytes`] with a memory `r/m`, and the
/// displacement chosen exactly as [`Lowerer::emit_rbp_modrm_disp`] chooses it
/// so the two cannot drift.
///
/// The 32-bit form reads four bytes of the slot where the `MOV` it replaces
/// read eight — the same comparison, since the `CMP EAX, ECX` it replaces only
/// ever looked at those four either.
pub(in crate::ir_lower) fn cmp_frame_imm_bytes(offset: i32, imm: i32, wide: bool) -> ([u8; 11], usize) {
    let mut out = [0u8; 11];
    let mut n = 0;
    if wide {
        out[n] = 0x48; // REX.W. RBP needs no REX.B, and `/7` leaves no `reg`.
        n += 1;
    }
    let short = i8::try_from(imm);
    out[n] = if short.is_ok() { 0x83 } else { 0x81 };
    n += 1;
    let neg = -offset;
    if let Ok(d) = i8::try_from(neg) {
        // mod=01, reg=/7, r/m=RBP(101), disp8; and the cast is the
        // two's-complement byte of a displacement that fits `i8`.
        out[n] = 0x7D;
        out[n + 1] = d as u8;
        n += 2;
    } else {
        out[n] = 0xBD; // mod=10, reg=/7, r/m=RBP(101), disp32
        out[n + 1..n + 5].copy_from_slice(&neg.to_le_bytes());
        n += 5;
    }
    match short {
        Ok(b) => {
            // Cast: as above.
            out[n] = b as u8;
            n += 1;
        }
        Err(_) => {
            out[n..n + 4].copy_from_slice(&imm.to_le_bytes());
            n += 4;
        }
    }
    (out, n)
}

/// `CRATONVM_JIT_IR_AND_ZERO_TEST_BRANCH` (JIT rx27 wave 4, lane regalloc):
/// an `If` on `(x & mask) == 0` / `!= 0` branches on
/// `TEST x, mask` ([`Lowerer::plan_and_zero_tests`]). Read once per lowering;
/// no `static`.
///
/// **Default ON** since the 2026-10-29 regalloc flip (`=0` kills it). The four
/// switches `AND_ZERO_TEST_BRANCH`, `AND_ZERO_TEST_GUARD`, `OPERAND_IN_PLACE` and
/// `PUBLISH_RELOAD_ELIDE` flip together (coordinator decision on b17, 14 rows, 5 reps: all four
/// against the chain default, find-ci -2.8 % and find-email -4.8 %, both
/// disjoint, no row worse; without `PUBLISH_RELOAD_ELIDE` find-ci stayed
/// bimodal; facts and the G1 / Serial GC battery identical in both modes).
pub(in crate::ir_lower) fn ir_and_zero_test_branch_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_OPERAND_IN_PLACE` (JIT rx27 wave 8, lane regalloc): a
/// compare (fused or not) and an ADD / SUB / AND / OR /
/// XOR read their second operand where it lives -- an immediate, its
/// register or its home word (`cmp eax,[rbp-x]`, `and eax,[rbp-x]`) --
/// instead of `MOV RCX, [rbp-x]` first; a fused compare whose first operand
/// arrives in RAX does too ([`Lowerer::emit_cmp_rax_with`],
/// [`Lowerer::emit_alu_acc_frame`]). Only reads change: every home word is
/// written as before, so no deopt or GC point sees anything new. Read per
/// lowering; no `static`.
///
/// **Default ON** since the 2026-10-29 regalloc flip (`=0` kills it). The four
/// switches `AND_ZERO_TEST_BRANCH`, `AND_ZERO_TEST_GUARD`, `OPERAND_IN_PLACE` and
/// `PUBLISH_RELOAD_ELIDE` flip together (coordinator decision on b17, 14 rows, 5 reps: all four
/// against the chain default, find-ci -2.8 % and find-email -4.8 %, both
/// disjoint, no row worse; without `PUBLISH_RELOAD_ELIDE` find-ci stayed
/// bimodal; facts and the G1 / Serial GC battery identical in both modes).
pub(in crate::ir_lower) fn ir_operand_in_place_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_PUBLISH_RELOAD_ELIDE` (JIT rx27 wave 9, lane regalloc):
/// when a definition's register publish `MOV reg, RAX`
/// (with at most its own home store after it) is the last instruction
/// emitted and the next read of that value wants it in RAX, the
/// `MOV RAX, reg` is not emitted: RAX already is the value on the
/// fall-through path. The prof lane's find-email disassembly (b16) shows the
/// pair `mov rbx,rax` / `mov rax,rbx` in the hot `Start.match` loop. Every
/// skipped read is checked after the body for a branch landing on it
/// ([`Lowerer::verify_publish_reload_elides`]); a hit refuses the attempt and
/// the retry lowers again without that value's skip. Nothing a deopt, GC
/// point or frame state reads changes. Read per lowering; no `static`.
///
/// **Default ON** since the 2026-10-29 regalloc flip (`=0` kills it). The four
/// switches `AND_ZERO_TEST_BRANCH`, `AND_ZERO_TEST_GUARD`, `OPERAND_IN_PLACE` and
/// `PUBLISH_RELOAD_ELIDE` flip together (coordinator decision on b17, 14 rows, 5 reps: all four
/// against the chain default, find-ci -2.8 % and find-email -4.8 %, both
/// disjoint, no row worse; without `PUBLISH_RELOAD_ELIDE` find-ci stayed
/// bimodal; facts and the G1 / Serial GC battery identical in both modes).
pub(in crate::ir_lower) fn ir_publish_reload_elide_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_AND_ZERO_TEST_GUARD` (JIT rx27 wave 6, lane regalloc): a
/// `Guard` on `(x & mask) == 0` / `!= 0` deopts on
/// `TEST x, mask`, and when the guard's own frame state names the `And` its
/// deopt pad rebuilds the `And`'s home from `x`
/// ([`Lowerer::plan_and_zero_guard_tests`]). Read once per lowering; no
/// `static`.
///
/// **Default ON** since the 2026-10-29 regalloc flip (`=0` kills it). The four
/// switches `AND_ZERO_TEST_BRANCH`, `AND_ZERO_TEST_GUARD`, `OPERAND_IN_PLACE` and
/// `PUBLISH_RELOAD_ELIDE` flip together (coordinator decision on b17, 14 rows, 5 reps: all four
/// against the chain default, find-ci -2.8 % and find-email -4.8 %, both
/// disjoint, no row worse; without `PUBLISH_RELOAD_ELIDE` find-ci stayed
/// bimodal; facts and the G1 / Serial GC battery identical in both modes).
pub(in crate::ir_lower) fn ir_and_zero_test_guard_enabled() -> bool {
    true
}

/// Where an and-zero `TEST` read its operand `x`
/// ([`Lowerer::emit_and_zero_test`]); a deopt pad entered from the test's
/// `Jcc` finds it there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum XLoc {
    Reg(u8),
    Frame(i32),
    Rax,
}

/// JIT rx27 wave 6: the home word a guard's deopt pad rebuilds before the
/// stub reads the frame -- `[rbp - slot] = x & imm` at the `And`'s width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) struct PadRemat {
    pub(in crate::ir_lower) x_loc: XLoc,
    pub(in crate::ir_lower) imm: i32,
    pub(in crate::ir_lower) wide: bool,
    pub(in crate::ir_lower) slot: i32,
    pub(in crate::ir_lower) is_add: bool,
}

/// The bytes of `TEST a, imm32` (`F7 /0 id`), 64-bit when `wide` (the
/// immediate sign-extended, as the `And` it replaces computed).
pub(in crate::ir_lower) fn test_reg_imm_bytes(a: u8, imm: i32, wide: bool) -> ([u8; 7], usize) {
    let rex = if wide { 0x48u8 } else { 0x40u8 } | u8::from(a >= 8);
    let mut out = [0u8; 7];
    let mut n = 0;
    if rex != 0x40 {
        out[n] = rex;
        n += 1;
    }
    out[n] = 0xF7;
    out[n + 1] = 0xC0 | (a & 7); // mod=11, /0, r/m=a
    out[n + 2..n + 6].copy_from_slice(&imm.to_le_bytes());
    n += 6;
    (out, n)
}

/// The bytes of `TEST [RBP - offset], imm32`, 64-bit when `wide`; the
/// displacement chosen as [`cmp_frame_imm_bytes`] chooses it.
pub(in crate::ir_lower) fn test_frame_imm_bytes(offset: i32, imm: i32, wide: bool) -> ([u8; 11], usize) {
    let mut out = [0u8; 11];
    let mut n = 0;
    if wide {
        out[n] = 0x48;
        n += 1;
    }
    out[n] = 0xF7;
    n += 1;
    let neg = -offset;
    if let Ok(d) = i8::try_from(neg) {
        out[n] = 0x45; // mod=01, /0, r/m=RBP(101), disp8
        // Cast: the two's-complement byte of a displacement that fits `i8`.
        out[n + 1] = d as u8;
        n += 2;
    } else {
        out[n] = 0x85; // mod=10, /0, r/m=RBP(101), disp32
        out[n + 1..n + 5].copy_from_slice(&neg.to_le_bytes());
        n += 5;
    }
    out[n..n + 4].copy_from_slice(&imm.to_le_bytes());
    n += 4;
    (out, n)
}

/// The bytes of `LEA dst, [base + disp]` — 64-bit when `wide` — and how many.
///
/// `8D /r`. The point of using it for `x + constant` is that it READS its
/// operands and WRITES a third register without going through an accumulator:
/// `mov rax,rbx; add eax,1; mov r14,rax` becomes `lea r14d,[rbx+1]`. It also
/// leaves the flags alone, which the `ADD` it replaces does not — nothing here
/// depends on that, but it is one fewer thing between an arm and a fused
/// compare.
///
/// Two encoding traps, and both are silent rather than faulting:
///
/// * a base whose low three bits are `100` (RSP, **R12**) means "SIB follows"
///   in the `r/m` field, so it needs a `24` SIB byte naming itself with no
///   index. R12 is in this backend's GP file, so this is a live case, not a
///   theoretical one.
/// * a base whose low three bits are `101` (RBP, **R13**) has no `mod=00`
///   form — that encoding means RIP-relative. This always emits a
///   displacement, `disp8` of zero if that is what it takes, which sidesteps
///   it for every base at the cost of one byte in the `+0` case.
pub(in crate::ir_lower) fn lea_reg_base_disp_bytes(dst: u8, base: u8, disp: i32, wide: bool) -> ([u8; 8], usize) {
    // REX.R extends the destination (the `reg` field), REX.B the base (`r/m`,
    // or the SIB base when there is one) — the opposite assignment from
    // `cmp_reg_imm_bytes`, where the sole operand is the `r/m`.
    let rex = if wide { 0x48u8 } else { 0x40u8 } | (u8::from(dst >= 8) << 2) | u8::from(base >= 8);
    let mut out = [0u8; 8];
    let mut n = 0;
    if rex != 0x40 {
        out[n] = rex;
        n += 1;
    }
    out[n] = 0x8D;
    n += 1;
    let short = i8::try_from(disp);
    let md = if short.is_ok() { 0x40u8 } else { 0x80u8 };
    out[n] = md | ((dst & 7) << 3) | (base & 7);
    n += 1;
    if base & 7 == 4 {
        out[n] = 0x24; // SIB: scale=0, index=none(100), base=r/m
        n += 1;
    }
    match short {
        Ok(d) => {
            // Cast: two's-complement byte of a displacement that fits `i8`.
            out[n] = d as u8;
            n += 1;
        }
        Err(_) => {
            out[n..n + 4].copy_from_slice(&disp.to_le_bytes());
            n += 4;
        }
    }
    (out, n)
}

/// The bytes of `LEA dst, [base + index]` (scale 1, no displacement) --
/// 64-bit when `wide` -- and how many. Round 9 wave 10 (irl10), the
/// register-register form of [`Lowerer::emit_add_lea_reg_reg`].
///
/// `8D /r` with a SIB byte. Three encoding traps, all silent:
///
/// * the SIB `index` field value `100` without REX.X means "no index", so RSP
///   can never be an index. Addition commutes, so an RSP index is swapped into
///   the base (RSP as a SIB base is fine). R12 as an index is fine: REX.X
///   makes it `1100`, a real register;
/// * a SIB `base` whose low three bits are `101` (RBP, **R13**) with `mod = 00`
///   means "disp32, no base", so such a base takes `mod = 01` and a zero disp8;
/// * the 32-bit form computes the 64-bit sum and writes its low half,
///   zero-extended -- exactly `ADD EAX, ECX`'s result in RAX.
pub(in crate::ir_lower) fn lea_reg_base_index_bytes(dst: u8, base: u8, index: u8, wide: bool) -> ([u8; 5], usize) {
    let (base, index) = if index == 4 {
        (index, base)
    } else {
        (base, index)
    };
    let rex = if wide { 0x48u8 } else { 0x40u8 }
        | (u8::from(dst >= 8) << 2)
        | (u8::from(index >= 8) << 1)
        | u8::from(base >= 8);
    let mut out = [0u8; 5];
    let mut n = 0;
    if rex != 0x40 {
        out[n] = rex;
        n += 1;
    }
    out[n] = 0x8D;
    n += 1;
    let needs_disp8 = base & 7 == 5;
    let md = if needs_disp8 { 0x40u8 } else { 0x00u8 };
    out[n] = md | ((dst & 7) << 3) | 0x04; // rm = 100: SIB follows
    n += 1;
    out[n] = ((index & 7) << 3) | (base & 7); // scale = 00
    n += 1;
    if needs_disp8 {
        out[n] = 0x00;
        n += 1;
    }
    (out, n)
}

/// The high half [`Lowerer::emit_poison_int_high`] adds into an `int`: neither
/// all zeros nor all ones, so neither extension survives it.
pub(in crate::ir_lower) const INT_POISON_HIGH: u64 = 0xA5A5_A5A5_0000_0000;

/// The bytes of the int poisoning sequence for `reg` --
/// `PUSH R11; MOV R11, INT_POISON_HIGH; LEA reg, [reg + R11]; POP R11` -- and
/// how many; `None` for RSP, R11 (the addend) and a non-register.
pub(in crate::ir_lower) fn poison_int_high_bytes(reg: u8) -> Option<([u8; 20], usize)> {
    if reg == R11 || reg == 4 || reg > 15 {
        return None;
    }
    let mut out = [0u8; 20];
    out[..4].copy_from_slice(&[0x41, 0x53, 0x49, 0xBB]); // PUSH R11; MOV R11, imm64
    out[4..12].copy_from_slice(&INT_POISON_HIGH.to_le_bytes());
    let (lea, lea_len) = lea_reg_base_index_bytes(reg, reg, R11, true);
    let mut n = 12;
    out[n..n + lea_len].copy_from_slice(&lea[..lea_len]);
    n += lea_len;
    out[n..n + 2].copy_from_slice(&[0x41, 0x5B]); // POP R11
    Some((out, n + 2))
}


/// `CRATONVM_JIT_IR_IMUL_CONST_FIRST` (JIT round 23 wave 11, lane L7; perf,
/// **default ON since j27 2026-10-28**, `=0` kills; was opt-in, neutral on n23-w11a, SegMono heapget 116/105/105 default
/// vs 139/105/103 off): `Op::Mul` with a foldable constant FIRST and a
/// non-constant second operand (`31 * h`, javac's order) multiplies by the
/// immediate (`Lowerer::emit_imul_const_first`) instead of materialising the
/// constant in RAX, copying the other operand to RCX and `IMUL EAX, ECX`. A
/// straight peephole: two instructions fewer, same bits. Read per node; no
/// `static`.
pub(in crate::ir_lower) fn ir_imul_const_first_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// The bytes of `IMUL EAX/RAX, src, imm` -- `6B /r ib` when the immediate
/// fits a sign-extended byte, `69 /r id` otherwise -- and how many. The
/// destination is RAX (`reg = 000`), the source any GP register in `r/m`
/// (REX.B extends it). Round 9 wave 10 (irl10), see
/// [`Lowerer::emit_imul_reg_imm3`].
pub(in crate::ir_lower) fn imul_rax_reg_imm_bytes(src: u8, imm: i32, wide: bool) -> ([u8; 7], usize) {
    let rex = if wide { 0x48u8 } else { 0x40u8 } | u8::from(src >= 8);
    let mut out = [0u8; 7];
    let mut n = 0;
    if rex != 0x40 {
        out[n] = rex;
        n += 1;
    }
    let modrm = 0xC0 | (src & 7); // reg = RAX (000), rm = src
    match i8::try_from(imm) {
        Ok(imm8) => {
            out[n] = 0x6B;
            out[n + 1] = modrm;
            // Cast: two's-complement byte of an immediate that fits `i8`.
            out[n + 2] = imm8 as u8;
            n += 3;
        }
        Err(_) => {
            out[n] = 0x69;
            out[n + 1] = modrm;
            out[n + 2..n + 6].copy_from_slice(&imm.to_le_bytes());
            n += 6;
        }
    }
    (out, n)
}

/// Where an `x + k` / `x - k` arm left its result, and therefore what the arm
/// still owes. See [`Lowerer::emit_add_lea`].
pub(in crate::ir_lower) enum AddForm {
    /// A `LEA` wrote the result's own register and settled its home word. The
    /// arm is finished.
    Done,
    /// A `LEA` left the result in RAX, exactly as `mov rax,x; add eax,k` would
    /// have. The arm's `store_rax` runs unchanged.
    InRax,
    /// Nothing emitted — the accumulator sequence stands.
    No,
}

/// The longest instruction run an [`RcxTwin`] replaces: `MOV RAX, r` (3) plus
/// `REX.W C1 /digit ib` (4).
pub(in crate::ir_lower) const RCX_TWIN_MAX: usize = 8;

/// An instruction run the CURRENT definition's arm just emitted to leave its
/// value in RAX, together with an equal-length run that leaves the same value
/// in RCX instead (round 9 wave 11, irl11,
/// `perf-long-counted-loops-lose-the-optimizing-osr-body`).
///
/// # What it removes
///
/// A value planned as an RCX carry with a dropped home is finished by
/// `store_rax` with `MOV RCX, RAX` and nothing else. In `mulOnly`'s optimizing
/// OSR body that was two of the loop's fifteen instructions:
///
/// ```text
/// mov rax,r15 ; sar rax,1 ; mov rcx,rax     -> mov rcx,r15 ; sar rcx,1
/// add rax,rcx ; mov rcx,rax                 -> add rcx,rax
/// ```
///
/// # Why a twin and not a second arm shape
///
/// The arms are held to their shape by source audits (the carry consumer's
/// `gp_load_value(RAX, ..)` / `gp_load_value(RCX, ..)` order, one `store_rax`
/// per arm, the fold guards' RCX discipline). A twin changes none of that: the
/// arm emits exactly what it always did and merely RECORDS the alternative, and
/// only `store_rax` -- the one place that knows the value is an RCX carry whose
/// home is dropped -- swaps it in, where it would otherwise have emitted the
/// `MOV RCX, RAX`. The recorded run is checked byte-for-byte against the
/// buffer when it is offered and the buffer position when it is applied, so a
/// run that is not exactly what the offer describes is never rewritten.
///
/// The swap changes what RAX holds afterwards and nothing else: RCX ends with
/// the value either way, and RCX is written at the point the `MOV` would have
/// written it (the run is the last thing emitted before that `MOV`). Nothing
/// reads RAX as this value: the carry names RCX, the home store is dropped, and
/// a carried value is never resident, so `publish_def_at_store` never runs.
#[derive(Clone, Copy)]
pub(in crate::ir_lower) struct RcxTwin {
    /// The definition the run computes.
    pub(in crate::ir_lower) id: NodeId,
    /// `[start, end)` in the code buffer.
    pub(in crate::ir_lower) start: usize,
    pub(in crate::ir_lower) end: usize,
    /// The replacement, `end - start` bytes long.
    pub(in crate::ir_lower) bytes: [u8; RCX_TWIN_MAX],
    pub(in crate::ir_lower) len: usize,
}

/// Where a fused compare reads its two operands.
///
/// Named rather than a nest of `Option`/`Result` because there are four forms
/// now and the reader of the arm should not have to decode which `Err` means
/// "frame slot". Each variant is one instruction; the fallback the arm keeps
/// for `None` is three.
pub(in crate::ir_lower) enum CmpForm {
    /// Both operands already in registers: `cmp ebx, r12d`.
    RegReg(u8, u8),
    /// First resident, second still in its slot: `cmp ebx, [rbp-60h]`.
    RegFrame(u8, i32),
    /// First resident, second a constant: `cmp ebx, 100`.
    RegImm(u8, i32),
    /// First in its slot, second a constant: `cmp [rbp-60h], 100`.
    FrameImm(i32, i32),
}

/// Value operand of a checked integral element store.
#[derive(Clone, Copy)]
pub(in crate::ir_lower) enum ArrayStoreSource {
    Reg(u8),
    IntImm(i32),
}

/// Does this op's lowering arm leave RCX untouched from entry to exit?
///
/// Asked of the ONE arm that runs between a deferred RCX carry's `MOV RCX, RAX`
/// and the consumer that reads RCX — see `plan_carries`' second-operand pass
/// and [`Lowerer::deferred_rcx`]. If the arm writes RCX, the consumer reads
/// whatever the arm left there instead of its operand: a wrong answer, not a
/// crash, and one no existing test would show.
///
/// **Two entries, and the shortness is the point.** Every BINARY arm loads its
/// own second operand with `gp_load_value(RCX, node.inputs[1])`, so none of the
/// `Add`/`Sub`/`And`/`Or`/`Xor` family can ever be here; `Op::Neg` wrote RCX in
/// its `IrType::Double` arm until round 11 (now `BTC RAX, 63`; admitting it
/// here would need the byte pins of `every_rcx_preserving_arm_leaves_rcx_alone`
/// extended first); the shift family puts the count in CL, which is
/// RCX. What is left is the width conversions, whose arms are a load into RAX,
/// one fixed instruction and a `store_rax`.
///
/// That still covers the shape this was built for — mixed `int`/`long`
/// arithmetic, where an `I2L` widens one operand of a `long` expression — which
/// is common enough in Java to be worth the machinery.
///
/// **Widening this list means reading the arm, not the op name.** The first
/// attempt at it listed six ops that all write RCX, and both the unit suite and
/// the `ir_vs_singlepass` differential suite passed with it in.
/// `every_rcx_preserving_arm_leaves_rcx_alone` is what makes a wrong entry
/// fail: it scans the claimed arms' source for any mention of RCX and pins the
/// exact bytes they emit, because a raw `buf.emit(&[..])` can name RCX in a
/// ModRM byte where no identifier scan would see it.
pub(crate) fn op_preserves_rcx(op: &Op) -> bool {
    matches!(op, Op::I2L | Op::L2I)
}

/// JIT round 25 item 1a (wave 2): does a trap at `op`'s bci leave the frame
/// only through a pad this lowering emits at the site (a deopt guard's pad, a
/// rethrow pad, the call-exception stub that publishes no frame)? These ops
/// call no Java method, allocate nothing, take no lock and reach no
/// safepoint, so no reader inspects their frame without the site's register
/// file. A reference access is left out: its barrier or store-check helpers
/// are calls into the collector's side of the VM.
/// [`Lowerer::site_named_blocked`] keeps the home of a value named at any
/// other trapping bci.
pub(in crate::ir_lower) fn op_traps_only_through_pads(op: &Op) -> bool {
    match op {
        Op::Guard { .. } | Op::ArrayLength | Op::Div | Op::Rem => true,
        Op::Load(k) | Op::Store(k) | Op::ArrayLoad(k) | Op::ArrayStore(k) => {
            !matches!(k, MemKind::Ref)
        }
        _ => false,
    }
}

pub(crate) fn op_home_is_one_store_rax(op: &Op) -> bool {
    matches!(
        op,
        Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Rem
            | Op::Neg
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Shl
            | Op::Shr
            | Op::UShr
            | Op::I2L
            | Op::L2I
            | Op::F2I
            | Op::F2L
            | Op::D2I
            | Op::D2L
            | Op::ArrayLength
            // 2026-09-06, and both entries carry the SAME argument the FP
            // exclusion above rests on: the arm has one `store_rax(slot)` with
            // RAX holding the value, and any other path it has produces a type
            // `value_home_droppable` refuses.
            //
            // A pure arithmetic node with no control edge, no memory edge and
            // no trap. The safest member of this list.
            | Op::ScalarIntrinsic(_)
            // The integral element load. Its FP path returns through
            // `fp_store_value`, and its `MemKind::Ref` path produces an
            // `IrType::Ref` -- `value_home_droppable` admits only `Int` and
            // `Long`, so neither is reachable for a dropped home, exactly as
            // for `Op::Div`/`Rem`/`Neg`. The guards it emits before the load
            // deopt BEFORE the value is defined, so no frame state can name a
            // home that was going to be written and was not.
            | Op::ArrayLoad(_)
            // JIT round 21 (lane charat, STRINTR21-5): an `Int` char in RAX at
            // its one `store_rax(slot)`, on both of its paths; it cannot deopt.
            | Op::StrCharDecode
    )
}

/// Emit an OSR entry stub for every bci this body can safely be entered at --
/// **default ON** since 2026-09-05; `CRATONVM_JIT_IR_OSR_ENTRY=0` is the kill
/// switch.
///
/// # This is WIRED (corrected 2026-09-16)
///
/// This comment said "Nothing in the VM calls these yet: `compile_osr_artifact`
/// reaches `x64::compile_with_param_slots` directly and knows nothing about
/// this tier." That was true when the stubs landed and stopped being true the
/// same week. The consumer is
/// `vm::runtime::interpreter::jit_bridge::try_osr`, which — gated by
/// `CRATONVM_JIT_OSR_OPTIMIZING`, **default ON since 2026-09-05** — builds an
/// optimizing artifact through `compile_optimizing_artifact`, keeps it in a
/// per-`(class, method, pc)` cache, and enters it with
/// `CompiledMethod::ir_osr_enter`. `compile_osr_artifact` is the single-pass
/// FALLBACK beneath that, not the only door.
///
/// The stale sentence mattered: it is the sentence a reader checks before
/// deciding whether an optimizing-tier OSR entry can be reached at all, and it
/// said no.
///
/// # The reach limit, which is real and much narrower than the gate
///
/// `try_osr` admits an artifact only when
/// `ir_osr_entry_addr(pc).is_some() && ir_osr_sentinel_free`. The second
/// conjunct means the body emits no deopt stub and no call-exception stub, so
/// **any method containing a call fails it**. `lower_inner` therefore skips
/// this emission entirely for a body that is not sentinel-free — see the call
/// site — because those stubs can never be entered.
///
/// (Round 15 wave 6, lane irosr: no longer the whole rule. Since round 11
/// waves 10 and 12 the door also admits a body whose exits the exception- or
/// guard-exit fact admits, and `lower_inner`'s `osr_entries_wanted` emits the
/// stubs for exactly those bodies too.)
///
/// `pub(crate)` since round 11 wave 17 (lane tier): `try_compile_inner` asks
/// it before handing the builder the opaque-arm liveness, so no arm is built
/// into a body that can emit no entry.
pub(crate) fn ir_osr_entry_enabled() -> bool {
    #[cfg(test)]
    {
        if let Some(forced) = OSR_ENTRY_FORCE.with(|c| c.get()) {
            return forced;
        }
    }
    // 2026-09-05: DEFAULT ON. `=0` is the kill switch.
    true
}

/// `CRATONVM_JIT_IR_OSR_REMAT_INVARIANTS`, **default ON** (`=0` is the kill
/// switch) -- round 15 wave 6, lane irosr
/// (`r15w5-osrtier-accepted-optimizing-osr-body-never-entered`): an OSR entry
/// whose only obstacle is a pure integer value live across the header that no
/// local names -- a post-loop expression of loop-invariant locals, which the
/// schedule places EARLY, in front of the loop -- recomputes that value in the
/// stub from the interpreter's locals instead of being refused. See
/// [`Lowerer::osr_entry_remat_program`]. Asked once per
/// `emit_osr_entry_stubs` (no `static`: `process_global_statics_ratchet`).
pub(crate) fn ir_osr_remat_invariants_enabled() -> bool {
    true
}

/// At most this many unnamed values recomputed by one OSR entry stub.
pub(in crate::ir_lower) const OSR_REMAT_MAX_ROOTS: usize = 8;
/// At most this many expression nodes visited for one entry, all roots
/// together (a shared subexpression is recomputed, and counted, per use).
pub(in crate::ir_lower) const OSR_REMAT_MAX_NODES: u32 = 64;
/// At most this deep an expression tree; also bounds the stub's PUSH depth.
pub(in crate::ir_lower) const OSR_REMAT_MAX_DEPTH: u32 = 12;

/// One instruction of an OSR entry stub's recomputation of the hoisted
/// invariants ([`Lowerer::osr_entry_remat_program`]): a stack machine whose
/// accumulator is RAX, whose second operand is RCX, and which clobbers RDX on
/// a division. None of the three is ever a residency register
/// (`IR_LOWER_LS_GPRS`), and a carry never spans a block, so the body the stub
/// jumps into reads none of them. `wide` selects the 64-bit (`long`) form;
/// the 32-bit form is exactly what the body's own `int` arithmetic emits
/// (upper half zero).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum OsrRematInsn {
    /// `MOV RAX, [R11 + 8*local]`: a value the header snapshot names.
    Local(usize),
    /// `MOV RAX, imm`: an `Op::Const`.
    Imm(i64),
    /// `PUSH RAX`.
    Push,
    /// `POP RCX`.
    PopRcx,
    /// `op RAX, RCX` for `Add`/`Sub`/`Mul`/`And`/`Or`/`Xor`.
    Alu { op: OsrRematAlu, wide: bool },
    /// `SHL`/`SAR`/`SHR RAX, CL`: the hardware masks the count to 5 or 6
    /// bits, which is exactly JLS 15.19's `& 0x1f` / `& 0x3f`.
    Shift { op: OsrRematShift, wide: bool },
    /// `NEG RAX`.
    Neg { wide: bool },
    /// `MOVSXD RAX, EAX`.
    I2L,
    /// `MOV EAX, EAX`.
    L2I,
    /// Signed division (or remainder) of RAX by a constant that is neither 0
    /// nor -1, so `IDIV` can neither trap nor overflow; Java truncates toward
    /// zero and the remainder takes the dividend's sign, as `IDIV` does.
    DivImm { d: i64, rem: bool, wide: bool },
    /// `CMP EAX/RAX, ECX/RCX; SETcc AL; MOVZX EAX, AL` -- the `Op::Cmp` arm's
    /// materialised 0/1. `setcc` is the second opcode byte (`CmpOp::x64_cc`
    /// plus `0x10`); `wide` for `long` and reference operands (round 15
    /// wave 6, lane irosr2).
    Cmp { setcc: u8, wide: bool },
    /// The `Op::LCmp` arm's `{-1, 0, 1}` of RAX against RCX, sign-extended.
    /// Clobbers RDX.
    LCmp,
    /// The `Op::ExactClassIs` arm's header test of the reference in RAX:
    /// `1` iff non-null, a plain object and of exactly `class_id`.
    ExactClassIs { class_id: u32 },
    /// The `Op::ClassWordOf` arm's header read of the reference in RAX
    /// ([`class_word_of_bytes`]): the class word of a plain object, `0` for a
    /// null or an array (JIT round 19, lane osr19).
    ClassWordOf,
    /// `MOV EAX, [RAX + ARRAY_LENGTH_OFFSET]` behind a null test that answers
    /// `0` for a null array (a stub cannot deopt; see
    /// [`Lowerer::osr_entry_remat_program`] for when a null can reach it).
    ArrayLength,
    /// JIT round 19 wave 2 (lane irops2, IROPS19-7): `MOV RAX, imm64(cell) ;
    /// MOV RAX, [RAX] ; MOVSXD RAX, [RAX + disp]` (`MOV` for `long`): a
    /// hoisted direct static read, `emit_inline_getstatic`'s sequence.
    StaticLoad {
        base_cell: u64,
        disp: i32,
        wide: bool,
    },
    /// JIT round 19 wave 4 (lane osr4, IROPS2-4): `MOV RAX, imm64(cell) ;
    /// MOV RAX, [RAX] ; MOV EAX, [RAX + disp]` -- a hoisted `float` static
    /// read, ZERO-extended as `emit_inline_getstatic` reads it (a `MOVSXD`
    /// would smear a negative float's sign into the home word's high half).
    StaticLoadF32 { base_cell: u64, disp: i32 },
    /// Write RAX to the value's home word and its assigned register, exactly
    /// as [`Lowerer::emit_osr_seed`] writes a seeded local.
    Publish(NodeId),
}

/// Which of the round-15-wave-6 (lane irosr2, IRO-4) shapes an OSR entry's
/// recomputation may use. Read once per [`Lowerer::osr_entry_remat_program`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) struct OsrRematPolicy {
    /// `Cmp`, `LCmp`, `ExactClassIs`, `ClassWordOf`
    /// ([`ir_osr_remat_compares_enabled`]).
    pub(in crate::ir_lower) compares: bool,
    /// `ArrayLength` ([`ir_osr_remat_array_length`]).
    pub(in crate::ir_lower) array_length: OsrRematArrayLength,
    /// `StaticLoad` of a hoisted direct static read
    /// ([`ir_osr_remat_static_read_enabled`]).
    pub(in crate::ir_lower) static_read: bool,
    /// JIT round 19 wave 4 (lane osr4, IROPS2-4): the same for a `float`,
    /// `double` or reference hoisted static read
    /// ([`ir_osr_remat_static_read_wide_enabled`], only with `static_read`).
    pub(in crate::ir_lower) static_read_wide: bool,
}

/// `CRATONVM_JIT_IR_OSR_REMAT_STATIC_READ_WIDE`, **default on** (default on) --
/// JIT round 19 proposal wave 4, lane osr4, IROPS2-4 of
/// `docs/feature-designs/jit-ir-optimizations.md`: with `CRATONVM_JIT_IR_OSR_REMAT_STATIC_READ`,
/// the OSR entry stub also re-reads a `float`, `double` or reference direct
/// static read the direct-static LICM hoisted (`OsrRematInsn::StaticLoad` /
/// `StaticLoadF32`), publishing an FP value to its home and its XMM register
/// and a reference to its home word, which every later safepoint's map
/// already names (a hoisted read is live across the loop). No `static`.
pub(crate) fn ir_osr_remat_static_read_wide_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_OSR_REMAT_STATIC_READ`, **default OFF** (opt-in) -- JIT
/// round 19 wave 2, lane irops2, IROPS19-7 of `docs/feature-designs/jit-ir-optimizations.md`:
/// an OSR entry stub may re-read an `int`-category or `long` direct static
/// read that `CRATONVM_JIT_IR_LICM_DIRECT_STATIC` hoisted to a loop
/// pre-header (`Graph::licm_hoisted_static_reads`), instead of refusing the
/// entry because a value live across it is named by no local. Needs
/// `CRATONVM_JIT_IR_OSR_REMAT_INVARIANTS` (default on). No `static`.
pub(crate) fn ir_osr_remat_static_read_enabled() -> bool {
    false
}



/// `CRATONVM_JIT_IR_OSR_REMAT_ARRAY_LENGTH`'s three answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum OsrRematArrayLength {
    /// An off-word: no `ArrayLength` is recomputed.
    Off,
    /// Unset (the default): only for an array the IR proves non-null on every
    /// path (`ir_check_elim::definitely_non_null`).
    NonNull,
    /// `any`: for any array the snapshot names, on LICM's precondition (see
    /// [`Lowerer::osr_entry_remat_program`]). Opt-in.
    Any,
}

/// `CRATONVM_JIT_IR_OSR_REMAT_COMPARES`, **default ON** (`=0` is the kill
/// switch) -- round 15 wave 6, lane irosr2 (IRO-4 of
/// `jit-r15w6-irosr-proposals-RETIRED-20261003.md`): an OSR entry stub may recompute an unnamed
/// `Cmp` / `LCmp` / `ExactClassIs` live across the entry (a loop-invariant
/// guard condition hoisted in front of the loop), reading its operands from
/// the locals the header snapshot names. Total operations only; see
/// [`Lowerer::osr_entry_remat_program`]. Needs
/// `CRATONVM_JIT_IR_OSR_REMAT_INVARIANTS` on. No `static`.
pub(in crate::ir_lower) fn ir_osr_remat_compares_enabled() -> bool {
    true
}



/// `CRATONVM_JIT_IR_OSR_INNER_PREDICATE_REFUSAL` -- JIT round 25 wave 8
/// (lane W8-7), **default ON**, `=0` restores the same-bci-only refusal.
/// Needs `CRATONVM_JIT_IR_OSR_PREDICATE_REFUSAL`. Read per compile; no
/// `static`.
pub(in crate::ir_lower) fn ir_osr_inner_predicate_refusal_enabled() -> bool {
    true
}

/// JIT round 25 wave 8 (lane W8-7): the blocks an ordinary OSR entry must not
/// start at because they lie INSIDE a loop whose pre-header holds a guard the
/// entry would skip. `guards` is `(guard bci, guard block)`; a pre-header
/// predicate carries its loop header's bci and sits in a block strictly
/// dominating that header. For each such guard and each header block `hb`
/// whose bci is the guard's (`block_bci`), the natural loop of `hb` (every
/// block that reaches one of its back-edge sources without passing `hb`) is
/// answered, `hb` included -- every block of it is dominated by `hb` and so
/// by the guard. A guard in `hb` itself (a builder guard for the header's own
/// bytecode) licenses nothing an entry skips and is ignored, as is a header
/// with no back edge. Blocks after the loop (dominated, but outside it) are
/// not answered: an entry there runs no code that read the guard's fact.
pub(in crate::ir_lower) fn osr_entries_inside_a_predicated_loop(
    schedule: &Schedule,
    guards: &[(usize, usize)],
    block_bci: &dyn Fn(usize) -> Option<usize>,
) -> std::collections::HashSet<usize> {
    let mut out = std::collections::HashSet::new();
    let nblocks = schedule.blocks.len();
    let dom = &schedule.dom;
    if guards.is_empty() {
        return out;
    }
    for hb in 0..nblocks {
        if !dom.is_reachable(hb) {
            continue;
        }
        let Some(own) = block_bci(hb) else {
            continue;
        };
        let guarded = guards
            .iter()
            .any(|&(gb, g_block)| gb == own && g_block != hb && dom.dominates(g_block, hb));
        if !guarded {
            continue;
        }
        let sources: Vec<usize> = schedule.blocks[hb]
            .predecessors
            .iter()
            .copied()
            .filter(|&p| p < nblocks && dom.is_reachable(p) && dom.dominates(hb, p))
            .collect();
        if sources.is_empty() {
            continue;
        }
        let mut body: std::collections::HashSet<usize> = std::collections::HashSet::new();
        body.insert(hb);
        let mut work = sources;
        while let Some(b) = work.pop() {
            if !body.insert(b) {
                continue;
            }
            for &p in &schedule.blocks[b].predecessors {
                if p < nblocks && dom.is_reachable(p) && !body.contains(&p) {
                    work.push(p);
                }
            }
        }
        out.extend(body);
    }
    out
}

/// `CRATONVM_JIT_IR_OSR_EXCUSE_MEMORY_TOKENS=1` -- round 16, lane osrdoor
/// (`r16-alloc-hashmap-put-get-premium-is-the-call-path-not-allocation`).
/// **Default OFF** until an A/B measures it. An OSR entry stub owes no seed to
/// a value with no machine representation (`Memory`, `Control`, `Void`): the
/// header's memory φ is the common one. See
/// [`Lowerer::osr_entry_value_needs_no_seed`]. No `static`: asked once per
/// `emit_osr_entry_stubs`.
pub(in crate::ir_lower) fn ir_osr_excuse_memory_tokens_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_OSR_DATAFLOW_LIVE_IN=1` -- round 16, lane osrdoor.
/// **Default OFF** until an A/B measures it. An OSR entry stub owes no seed to
/// a value the converged backward dataflow says is NOT live into the entry
/// block, although its linear interval covers the entry (an outer loop's φ,
/// whose interval the phi-edge interference rule stretches to the outer latch).
/// The same model the opaque arms are admitted by (`LiveModel::live_in_at`).
/// See [`Lowerer::osr_entry_value_needs_no_seed`]. No `static`.
pub(in crate::ir_lower) fn ir_osr_dataflow_live_in_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_OSR_DATAFLOW_LIVE_IN_REFS`, **default ON** (`=0` is the
/// kill switch) -- JIT round 21 wave 17, lane osrrefs (OD16-2;
/// `docs/feature-designs/jit-osr-exit-and-recompile.md`).
/// The reference half of [`ir_osr_dataflow_live_in_enabled`], independent of
/// it: an OSR entry stub owes no seed to an unnamed `Ref` value the converged
/// dataflow says is not live into the entry block, and writes null to its
/// home word and register instead. See
/// [`Lowerer::osr_entry_ref_needs_no_seed_by`]. No `static`: asked once per
/// `emit_osr_entry_stubs`.
pub(in crate::ir_lower) fn ir_osr_dataflow_live_in_refs_enabled() -> bool {
    true
}





/// OD16-4 (round 16, lane osrdoor; `jit-r16-osrdoor-proposals-RETIRED-20261006.md`): the run's
/// ordinary-OSR-entry outcomes, by refusal reason and, for "a value live here
/// is named by no local", by the unnamed value's shape (`Op` variant : type).
/// Printed once at exit beside the `OSR lifecycle` line
/// ([`osr_entry_census_line`]). The census this lane needed took a graph
/// dump, a linear-scan trace and a script joining node ids between them; a
/// bare `Some(Phi)` hid that 24 of 26 such refusals were memory φs. Written
/// only on the compile path (one lock per refusal), never per execution.
pub(in crate::ir_lower) static OSR_ENTRY_CENSUS: std::sync::Mutex<std::collections::BTreeMap<String, u64>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// The census key under which emitted entries are counted.
pub(in crate::ir_lower) const OSR_ENTRY_CENSUS_EMITTED: &str = "(emitted)";

/// Count `n` under `key` in [`OSR_ENTRY_CENSUS`].
pub(in crate::ir_lower) fn note_osr_entry_census_n(key: &str, n: u64) {
    if n == 0 {
        return;
    }
    let mut census = OSR_ENTRY_CENSUS.lock().unwrap_or_else(|e| e.into_inner());
    match census.get_mut(key) {
        Some(c) => *c = c.saturating_add(n),
        None => {
            census.insert(key.to_string(), n);
        }
    }
}

/// Count one under `key` in [`OSR_ENTRY_CENSUS`].
pub(in crate::ir_lower) fn note_osr_entry_census(key: &str) {
    note_osr_entry_census_n(key, 1);
}

/// `unnamed:<Op variant>:<type>` -- the variant without its fields, so
/// `ExactClassIs { class_id: 64 }` and `{ class_id: 192 }` share a row.
pub(in crate::ir_lower) fn osr_entry_census_shape_key(op: &Op, ty: IrType) -> String {
    let full = format!("{op:?}");
    let variant: String = full
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    format!("unnamed:{variant}:{ty:?}")
}

/// The exit line of [`OSR_ENTRY_CENSUS`], or `None` when no optimizing body
/// planned an OSR entry this run.
pub fn osr_entry_census_line() -> Option<String> {
    let census = OSR_ENTRY_CENSUS.lock().unwrap_or_else(|e| e.into_inner());
    if census.is_empty() {
        return None;
    }
    let rows: Vec<String> = census.iter().map(|(k, v)| format!("{k}={v}")).collect();
    Some(format!("[cratonvm] OSR entry census: {}", rows.join(" | ")))
}

/// `CRATONVM_JIT_IR_OSR_REMAT_ARRAY_LENGTH` -- round 15 wave 6, lane irosr2
/// (IRO-4): may an OSR entry stub recompute an unnamed `ArrayLength`? Unset:
/// only of a provably non-null array ([`OsrRematArrayLength::NonNull`], the
/// default); `any`: of any array the snapshot names
/// ([`OsrRematArrayLength::Any`], opt-in until a census says the LICM argument
/// holds on real code); an off-word (`0`, `false`, `off`, `no`, empty): never.
/// Needs `CRATONVM_JIT_IR_OSR_REMAT_INVARIANTS` on. No `static`.
pub(in crate::ir_lower) fn ir_osr_remat_array_length() -> OsrRematArrayLength {
    match None::<std::ffi::OsString> {
        None => OsrRematArrayLength::NonNull,
        Some(value) => {
            let value = value.to_string_lossy().trim().to_ascii_lowercase();
            if value.is_empty() || cratonvm_types::flags::value_is_off(&value) {
                OsrRematArrayLength::Off
            } else if value == "any" {
                OsrRematArrayLength::Any
            } else {
                OsrRematArrayLength::NonNull
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum OsrRematAlu {
    Add,
    Sub,
    Mul,
    And,
    Or,
    Xor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum OsrRematShift {
    Shl,
    Shr,
    UShr,
}

/// The array-length displacement every OSR entry stub reads through: the
/// recomputation's `ArrayLength` and the round-16 (lane matrix, MX16-1)
/// re-checks share this one site. Evaluated at build time: a layout constant
/// past 127 fails the build rather than encode a negative disp8.
pub(in crate::ir_lower) const OSR_ARRAY_LENGTH_DISP8: i8 = crate::x64::disp::disp8_const(ARRAY_LENGTH_OFFSET as i64);

/// Checked payload displacement shared by the scalar array element encoders.
pub(in crate::ir_lower) const ARRAY_DATA_DISP8: u8 = crate::x64::disp::disp8_const(HEADER_SIZE as i64) as u8;

/// What one OSR entry stub recomputes ([`Lowerer::osr_entry_remat_program`]).
/// Empty for every entry that needs nothing recomputed, which emits nothing.
#[derive(Clone, Debug, Default)]
pub(in crate::ir_lower) struct OsrRemat {
    /// The stack-machine program, roots in order, each ending in `Publish`.
    pub(in crate::ir_lower) prog: Vec<OsrRematInsn>,
    /// The unnamed values it writes.
    pub(in crate::ir_lower) roots: Vec<NodeId>,
    /// The JVM locals it reads: their kind becomes a constraint of the entry
    /// (`CompiledMethod::ir_osr_entry_kinds`) even when the body itself never
    /// reads the local again, so the door refuses a slot of another type.
    pub(in crate::ir_lower) leaf_locals: Vec<usize>,
    /// Round 16 (lane matrix, MX16-1): the re-check program, run after the
    /// seeding (`Lowerer::osr_entry_recheck_program`). Empty with
    /// `CRATONVM_JIT_IR_OSR_REMAT_ARRAY_LOAD` off, which emits nothing.
    pub(in crate::ir_lower) checked: Vec<OsrCheckInsn>,
    /// The frame its failure exit resumes: the entry's header snapshot.
    pub(in crate::ir_lower) fail_frame: Option<FrameState>,
    /// JIT round 21 wave 17 (lane osrrefs, OD16-2): the unnamed references
    /// this entry owes no seed (`Lowerer::osr_entry_ref_needs_no_seed`). The
    /// stub writes null to each one's home word and register
    /// (`Lowerer::emit_osr_null_ref`). NOT roots: a deopt frame that names
    /// one still refuses the entry. Empty with
    /// `CRATONVM_JIT_IR_OSR_DATAFLOW_LIVE_IN_REFS=0`.
    pub(in crate::ir_lower) null_refs: Vec<NodeId>,
}

/// One instruction of an OSR entry stub's re-check program (round 16, lane
/// matrix, MX16-1). Every test jumps to the program's one failure exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum OsrCheckInsn {
    /// A total recomputation step ([`OsrRematInsn`]).
    Remat(OsrRematInsn),
    /// RAX = array, RCX = index: null and bounds tests, then the element
    /// load the body's own arm emits (`emit_gpr_array_elem_load`).
    ArrayLoad(MemKind),
    /// RAX = array: a null test, then its length.
    ArrayLength,
    /// Fail when RAX is zero (a predicate's compare answered false).
    FailIfZero,
    /// Item 4 residual 2: RAX = a receiver a skipped deopting compactness
    /// guard proved -- fail when it is null or not `GC_FLAG_COMPACT`.
    CompactReceiver,
    /// Item 4 residual 2: fail when the layout replacement epoch moved from
    /// the one this compile baked (a skipped deopting layout guard's
    /// compare). Clobbers R11 and ECX on the non-RIP form; R11 is reloaded.
    LayoutEpoch,
    /// Write RAX to the value's home word and register, as a seed is written.
    Publish(NodeId),
}

/// `CRATONVM_JIT_OSR_OPTIMIZING_EXC_EXITS=1` — tier proposal 27 (round 11
/// wave 10, lane irexc): emit OSR entry stubs into a body whose only exits are
/// exceptional (`CompiledMethod::ir_osr_exception_exit_bcis`), the population
/// the optimizing OSR door admits under the SAME variable
/// (`jit_bridge::osr_optimizing_exception_exits_admissible`). DEFAULT ON since
/// round 11 wave 11 (soaked: full R11 battery, legacy probes and IR census
/// clean; `R11X64genSyncCost sync-method-1t` ~1000 -> ~80 ms); `=0` is the
/// kill switch and restores the stub-free body. Declared by
/// `r11w10-irexc-types-ir-precise-handler-frames-flag-patch-FIXED-20260924.md`. No
/// `static` (`process_global_statics_ratchet`): asked once per compile.
pub(in crate::ir_lower) fn ir_osr_exception_exit_entries_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_OSR_OPTIMIZING_GUARD_EXITS=1` — tier proposal 27 phase 3
/// (round 11 wave 12, lane irexc): emit OSR entry stubs into a body whose
/// guard exits the optimizing OSR door can resume without a plan
/// (`CompiledMethod::ir_osr_guard_exit_bcis`), the population
/// `jit_bridge::osr_optimizing_guard_exits_admissible` admits under the SAME
/// variable. DEFAULT ON since round 11 wave 14 (`=0` is the kill switch;
/// soaked in waves 12 and 13). The door's resume arm is
/// `r11w12-irexc-tier-try-osr-planless-guard-exit-patch-FIXED-20260924.md`, and
/// the variable is declared by
/// `r11w12-irexc-types-osr-optimizing-guard-exits-flag-patch-FIXED-20260924.md`.
/// No `static` (`process_global_statics_ratchet`): asked once per compile.
pub(in crate::ir_lower) fn ir_osr_guard_exit_entries_enabled() -> bool {
    true
}

/// Interpreter round i2 wave 54, lane L2: does `keys` name the method key of
/// every SPLICED scope of the caller chain `fs` (every scope but the
/// outermost, which is the compiling method itself)? A method-entry post-call
/// chain exit is admitted only then (`ir::Graph::entry_chain_source_keys`):
/// each such scope can be rebuilt from the bytecode the compile spliced when
/// its class was redefined, which is exactly when the exit is forced. `false`
/// for a frame with no caller (not a chain).
pub(in crate::ir_lower) fn chain_scopes_have_kept_sources(fs: &FrameState, keys: &[String]) -> bool {
    if fs.caller.is_none() {
        return false;
    }
    let mut scope = Some(fs);
    while let Some(f) = scope {
        // The outermost scope (no caller of its own) is the compiling method.
        if f.caller.is_some() && !keys.iter().any(|k| *k == f.method_key) {
            return false;
        }
        scope = f.caller.as_deref();
    }
    true
}

/// Emit OSR entry stubs even into a body whose door cannot enter it.
///
/// `CRATONVM_JIT_IR_OSR_ENTRY_ALWAYS=1`. Default OFF: see the skip at
/// `lower_inner`'s `emit_osr_entry_stubs` call for why a body that is not
/// `ir_osr_sentinel_free` carries stubs nothing can reach. Exists so the skip
/// can be A/B'd on one binary, and so a test can exercise a stub in a body
/// that would otherwise carry none.
pub(in crate::ir_lower) fn ir_osr_entry_always_enabled() -> bool {
    #[cfg(test)]
    {
        if let Some(forced) = OSR_ENTRY_ALWAYS_FORCE.with(|c| c.get()) {
            return forced;
        }
    }
    false
}

/// Bodies whose OSR entry stubs were skipped because the body is not
/// `ir_osr_sentinel_free`, so `try_osr` would refuse them anyway.
pub(in crate::ir_lower) static OSR_ENTRY_SKIPPED_NOT_SENTINEL_FREE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(in crate::ir_lower) fn note_osr_entry_skipped_not_sentinel_free() {
    OSR_ENTRY_SKIPPED_NOT_SENTINEL_FREE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// How many optimizing bodies declined OSR entry stubs for want of
/// sentinel-freeness.
///
/// Reported rather than left to a `debug!`, which `release_max_level_info`
/// compiles to a no-op (see the workspace `Cargo.toml` note): a zero here is
/// meaningful — it would mean every optimizing body in the run was
/// call-free — and a meaningful zero needs a counter, not a log line.
pub fn ir_osr_entry_skipped_census() -> u64 {
    OSR_ENTRY_SKIPPED_NOT_SENTINEL_FREE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Reserve the 256-byte [`crate::deopt::SavedRegisters`] region in this
/// backend's frame, spill the register file into it at the deopt stub, and let
/// a deopt frame NAME a register -- **default ON** since 2026-09-05;
/// `CRATONVM_JIT_IR_DEOPT_REGS=0` is the kill switch.
///
/// Off is the historical arrangement, whose contract `ir_deopt_entry` states:
/// "the IR lowerer keeps every live value in a frame slot, so no register file
/// is needed". That is what makes a deopt-named value's home word mandatory,
/// and therefore what stops a register-resident value from ever losing it --
/// see the `deopt` cause of [`Lowerer::census_home_blocks`].
pub(in crate::ir_lower) fn ir_deopt_regs_enabled() -> bool {
    #[cfg(test)]
    {
        if let Some(forced) = DEOPT_REGS_FORCE.with(|c| c.get()) {
            return forced;
        }
    }
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        // 2026-09-05: DEFAULT ON. `=0` is the kill switch — which is why
        // this reads the VALUE now rather than only asking whether the
        // variable is present.
        // `runtime_flag_default_on` (round 9): `off`/`no` are kill words
        // too, matching every other default-ON switch in this file.
        true
    })
}

/// Bytes the frame reserves for the deopt register image.
///
/// `size_of::<SavedRegisters>()` exactly: `{ gpr: [u64; 16], xmm: [u64; 16] }`,
/// `#[repr(C)]`, and `saved_registers_layout_matches_the_stub_spill_region` in
/// `deopt.rs` pins it.
/// The whole struct is reserved rather than only the GPR half, because the
/// pointer handed to `ir_deopt_entry` is a `*const SavedRegisters` and the
/// resolver indexes both halves.
pub(in crate::ir_lower) fn ir_deopt_regs_bytes() -> i32 {
    if ir_deopt_regs_enabled() {
        DEOPT_IMAGE_FULL_BYTES
    } else {
        0
    }
}

/// `CRATONVM_JIT_IR_PUBLISH_LAST_LOCAL` -- round 16 wave 11 (lane frames2,
/// FR-B1): publish `FrameLayout::java_locals_hi` as the exclusive end of the
/// Java-local words (`(num_locals + 1) * 8`) instead of `num_locals * 8`, so the
/// collector's band scan reads the last Java-local word instead of treating it
/// as reserved tail. A correctness fix: default ON, `=0` is the kill switch.
/// Read per compile; no static.
pub(in crate::ir_lower) fn ir_publish_last_local_enabled() -> bool {
    true
}

/// `FrameLayout::java_locals_hi` for an optimizing-tier frame whose Java
/// locals take `locals_size` bytes (`num_locals * 8`, local `i` at `[rbp - (i +
/// 1) * 8]`): the exclusive end `locals_size + 8` under the fix, the
/// historical `locals_size` (which left the last local out) without it, and 0
/// for a method with no locals either way (no word to name).
pub(in crate::ir_lower) fn ir_published_java_locals_hi(locals_size: i32, fixed: bool) -> i32 {
    if fixed && locals_size > 0 {
        locals_size + 8
    } else {
        locals_size
    }
}

/// `size_of::<SavedRegisters>()`: sixteen GPR words, then sixteen XMM words.
pub(in crate::ir_lower) const DEOPT_IMAGE_FULL_BYTES: i32 = 256;

/// `CRATONVM_JIT_IR_DEOPT_IMAGE_SIZED=1` -- round 16 wave 11 (lane frames2,
/// FR16P-1 / backlog FR-B2): reserve only the deopt-image words a frame state
/// of the body can name ([`Lowerer::size_deopt_image`]) instead of the whole
/// 256-byte `SavedRegisters`. A performance feature (smaller frames; a smaller
/// fresh-frame clear for a body with no resident GPR), so default OFF until
/// measured. Read per compile; no static.
pub(in crate::ir_lower) fn ir_deopt_image_sized_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_DEOPT_IMAGE_EXITLESS=1` -- round 16 wave 12 (lane
/// polyclear, FR2P-1 of `jit-r16-frames2-proposals-RETIRED-20261005.md`): reserve no deopt
/// register image at all in a body none of whose exits can name a register
/// ([`Lowerer::deopt_image_never_read`], [`Lowerer::drop_unread_deopt_image`]),
/// whatever the residency plan hands out. Independent of
/// `CRATONVM_JIT_IR_DEOPT_IMAGE_SIZED`: it drops the full 256-byte image or a
/// sized one alike. A performance feature (a frame up to 256 bytes smaller and
/// 128 bytes less fresh-frame clear per call -- `R16Bench poly`'s `area()`
/// leaves). Default ON since wave 12 (`R16Bench poly` 1238 -> 1174 ms, other
/// rows flat, 40/40 loaded runs); `=0` turns it off. Read per compile; no static.
pub(in crate::ir_lower) fn ir_deopt_image_exitless_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SAVE_BANDS_SIZED=1` -- round 16 wave 17 (lane frames2,
/// PC-1 of `jit-r16-polyclear-proposals-RETIRED-20261005.md`): size the callee-saved XMM and GPR
/// save bands to the registers this body's residency plan hands out, packed
/// from the shallow end ([`Lowerer::size_save_bands`]), instead of one slot per
/// register of the file. An unused save slot is never written, so the band scan
/// roots whatever the previous stack occupant left there and the fresh-frame
/// clear has to zero it on every call; taking it out of the frame removes both.
/// A performance feature (a smaller frame and clear), so default OFF until
/// measured. Read per compile; no static.
pub(in crate::ir_lower) fn ir_save_bands_sized_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_STAGE_CLEAR_REBASE=0` -- the kill switch of a round 16
/// wave 17 (lane frames2) correctness fix. `plan_dead_home_clears` records the
/// argument-staging words each call site zeroes as frame OFFSETS, before the
/// residency plan; every later layout step that shrinks the frame below the
/// spill cap (`size_deopt_image`, `drop_unread_deopt_image`,
/// `size_save_bands`) moves the staging region shallower by its cut, and the
/// recorded offsets did not follow. Under `CRATONVM_JIT_IR_DEOPT_IMAGE_SIZED=1`
/// a call site then zeroed a word `cut` bytes deeper than the staging word it
/// meant (in the shadow/reserve, or below `RSP`) and left the stale staged
/// reference rooted. [`Lowerer::rebase_planned_stage_clears`] moves them with
/// the region. Default ON; a frame no step shrinks is byte-identical either
/// way. Read per compile; no static.
pub(in crate::ir_lower) fn ir_stage_clear_rebase_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_PROLOGUE_THREAD_IN_RAX=1` -- round 16 wave 17 (lane
/// frames2, FR16P-3 of `jit-r16-frames-proposals-RETIRED-20261005.md`): the prologue's
/// shadow-watermark capture takes the thread pointer from RAX, which the fetch
/// just loaded and stored, instead of reloading the word it stored
/// ([`Lowerer::fetch_current_thread`]). One store-forwarded load off the entry
/// path of every optimizing body that publishes roots (`bintrees`'
/// allocating methods). A performance feature, default OFF until measured.
/// Read per compile; no static.
pub(in crate::ir_lower) fn ir_prologue_thread_in_rax_enabled() -> bool {
    false
}

/// The bytes a sized deopt image reserves for a frame whose whole-method GPR
/// set is `saved_gp` (`Lowerer::ls_saved_gp`): nothing when the set is empty
/// (no register can be named), else `gpr[0..=k]`, `k` the highest register in
/// the set and at least 5 (`gpr[5]`, the RBP image, which the collector's
/// residue test reads at `deopt_gpr_hi - 48`), rounded up to 16 bytes so the
/// frame keeps its alignment. Never more than the GPR half: this tier names no
/// XMM register in a frame state.
pub(in crate::ir_lower) fn sized_deopt_image_bytes(saved_gp: &[bool; 16]) -> i32 {
    let Some(k) = (0..16).rev().find(|&r| saved_gp[r]) else {
        return 0;
    };
    // Cast: a register index below 16.
    let words = (k.max(5) + 1) as i32;
    (words * 8 + 15) & !15
}

/// The part of the whole-method GPR set `saved_gp` a frame state may name in
/// the deopt image: everything but [`IR_LS_CALLER_SAVED_GPRS`]. A caller-saved
/// value (`CRATONVM_JIT_IR_LS_CALLER_SAVED`) is never deopt-nameable -- every
/// call, poll and guard pad may clobber R8-R11 before the stub stores them, so
/// its frame state names the home -- so sizing the image for one only reserved
/// `gpr[0..=11]` (96 bytes) that nothing reads. Round 25 item 1b's "Risks".
pub(in crate::ir_lower) fn deopt_image_named_gprs(saved_gp: &[bool; 16]) -> [bool; 16] {
    let mut named = *saved_gp;
    for &r in IR_LS_CALLER_SAVED_GPRS {
        if let Some(cell) = named.get_mut(r as usize) {
            *cell = false;
        }
    }
    named
}

/// Bytes the frame reserves for [`IR_LOWER_SAVED_GPRS`].
///
/// Eight per register — a GPR is 8 bytes and, unlike the XMM band, there is no
/// wider value hiding in it. Zero unless the linear-scan path is on, for the
/// same reason `ir_saved_xmm_bytes` returns zero then: a frame must not pay for
/// a register nothing can hand out.
pub(in crate::ir_lower) fn ir_saved_gpr_bytes() -> i32 {
    if IR_LOWER_SAVED_GPRS.is_empty() || !linear_scan_enabled() {
        return 0;
    }
    // Sized from the file this compile may hand out, so `CRATONVM_JIT_IR_GP_WIDE`
    // moves the reservation and the handout together and the OFF arm reserves
    // exactly what it reserved before RSI/RDI existed. Both this and
    // `Lowerer::new` read it through one `OnceLock`, so the two cannot disagree
    // mid-process — the drift `saved_xmm_bytes` warns about.
    // Cast: a small compile-time constant — five registers, seven on Win64.
    ir_gp_file().len() as i32 * 8
}

/// `CRATONVM_JIT_IR_ISEL_SHADOW` — run the instruction selector over this
/// compile's blocks, count what it would have produced, and **discard it**.
///
/// `docs/feature-designs/jit-machine-level-and-instruction-selection.md`,
/// increment 0. Default **off**. Turning it
/// on changes no emitted byte — `shadow_selection_changes_no_emitted_byte`
/// pins that — and costs one tiling pass per compile. It exists to replace the
/// contract's ten-shape synthetic coverage figure with one taken over real
/// compiles, because that number is what decides whether the rest of the
/// HIR/MIR migration is worth its cost.
///
/// Report the result with `CRATONVM_DBG=ir-isel`.
pub(in crate::ir_lower) fn isel_shadow_enabled() -> bool {
    false
}

/// `CRATONVM_DBG=ir-isel` — print one shadow-selection line per compile.
pub(in crate::ir_lower) fn isel_shadow_reporting() -> bool {
    false
}

/// `CRATONVM_JIT=ir-isel-verify` — build the level-2 machine list and check the
/// tile encoder against the per-opcode arms, byte for byte. Default **off**.
///
/// Changes no emitted byte: the per-opcode arms still emit, and the encoder's
/// output is compared against what they wrote. This is the method-scale form of
/// the anchoring property each `PATTERNS` row carries per row, and it is the
/// gate [`isel_emit_enabled`] has to pass on a real corpus first.
pub(in crate::ir_lower) fn isel_verify_enabled() -> bool {
    #[cfg(test)]
    {
        if let Some(mode) = mir_forced() {
            return mode == MirMode::Verify;
        }
    }
    false
}

/// `CRATONVM_JIT=ir-isel-emit` — the level-2 machine list emits, for the tiles
/// the encoder covers. Default **off**.
///
/// Fail-closed, unlike `ir-isel-shadow`: a block the selector does not cover, a
/// tile whose bytes disagree with the destination slot the allocator hands out,
/// or an encoder refusal *after* the tile was admitted all discard the
/// artifact, and the method runs in a lower tier. That is always valid, and it
/// is the trade increment 0 was explicitly exempt from because its documented
/// effect was a count.
pub(in crate::ir_lower) fn isel_emit_enabled() -> bool {
    #[cfg(test)]
    {
        if let Some(mode) = mir_forced() {
            return mode == MirMode::Emit;
        }
    }
    false
}

/// May the level-2 encoder answer for node `id`'s lowering with this tile?
///
/// Three conditions, and the third is the one that is not obvious. The caller
/// skips **every** node a tile covers, so a tile that absorbed a node the
/// encoder does not actually fold leaves that node computed nowhere — the same
/// failure `BlockSelection::covers` exists to make unrepresentable, one level
/// further down. `Rule::AluReg` never absorbs, so today this clause is
/// insurance rather than a live filter; a rule added to the set below without a
/// fold is exactly what it is insurance against.
pub(in crate::ir_lower) fn mir_tile_is_emittable(tile: &crate::x64::isel::Tile, id: NodeId) -> bool {
    use crate::x64::isel::Rule;

    tile.root == id && tile.rule == Rule::AluReg && tile.covered.as_slice() == [id]
}

/// Build the level-2 machine list for one method, or refuse.
///
/// `None` means "do not compile this method through the machine level" and is
/// returned for exactly one reason: a block the selector did not cover. That is
/// the invariant `BlockSelection::covers` exists for — a node covered zero times
/// is a dropped instruction — and increment 0 measured it holding on all 1 911
/// blocks of a real Spring Boot workload, so a violation here is a compiler bug
/// and not a shape to work around.
///
/// `SelectOptions::default()` for the same reason `shadow_select_method` uses
/// it: `require_encodable: true`, and `fold_loads: false` because the fold's
/// address half is still `AddrSource::Opaque`.
pub(in crate::ir_lower) fn build_mir_plan(graph: &Graph, schedule: &Schedule) -> Option<MirPlan> {
    use crate::x64::isel::SelectOptions;

    // `frame_homed: true` is the one departure from `shadow_select_method`'s
    // options, and it is a statement about the consumer rather than a tuning
    // knob: `encode_tile_frame_homed` puts every value in its frame word, so
    // the two-address copy the cost model would otherwise charge the ALU form
    // does not exist. Left `false` it makes `Rule::Lea` outbid `Rule::AluReg`
    // on `a + b` whenever `a` is live afterwards, and the `LEA` that wins is a
    // byte longer than the `ADD` it replaced.
    let opts = SelectOptions {
        frame_homed: true,
        ..SelectOptions::default()
    };
    build_mir_plan_with(graph, schedule, opts)
}

pub(in crate::ir_lower) fn build_allocated_mir_plan(
    graph: &Graph,
    schedule: &Schedule,
    protected: &[NodeId],
    checks: Option<&crate::ir_check_elim::CheckElision>,
) -> Option<MirPlan> {
    use crate::x64::isel::SelectOptions;
    let opts = SelectOptions {
        allocated_division: true,
        allocated_setcc: true,
        fold_loads: checks.is_some(),
        ..SelectOptions::default()
    };
    build_mir_plan_with_protected(graph, schedule, opts, protected, checks)
}

pub(in crate::ir_lower) fn build_mir_plan_with(
    graph: &Graph,
    schedule: &Schedule,
    opts: crate::x64::isel::SelectOptions,
) -> Option<MirPlan> {
    build_mir_plan_with_protected(graph, schedule, opts, &[], None)
}

pub(in crate::ir_lower) fn build_mir_plan_with_protected(
    graph: &Graph, schedule: &Schedule, opts: crate::x64::isel::SelectOptions,
    protected: &[NodeId],
    checks: Option<&crate::ir_check_elim::CheckElision>,
) -> Option<MirPlan> {
    let mut selector = crate::x64::isel::MethodSelector::new(graph);
    selector.protect_values(protected);
    if let Some(checks) = checks {
        selector.allow_checked_array_reads(checks);
    }
    let mut blocks = Vec::with_capacity(schedule.blocks.len());
    let mut tile_of: Vec<Option<(u32, u32)>> = vec![None; graph.nodes.len()];
    for (bi, block) in schedule.blocks.iter().enumerate() {
        let sel = selector.select_block(&block.nodes, block.terminator, &opts);
        if !sel.covers(&block.nodes) {
            return None;
        }
        let bi = u32::try_from(bi).ok()?;
        for (ti, tile) in sel.tiles.iter().enumerate() {
            let ti = u32::try_from(ti).ok()?;
            if let Some(cell) = tile_of.get_mut(tile.root as usize) {
                // Two tiles rooted at one node would mean the node is computed
                // twice; `covers` already refused that, so this is belt and
                // braces on the index rather than a second policy.
                if cell.is_some() {
                    return None;
                }
                *cell = Some((bi, ti));
            }
        }
        blocks.push(sel);
    }
    Some(MirPlan { blocks, tile_of })
}

/// What [`verify_mir_allocation`] concluded — the three-valued shape the
/// design doc requires, not an `Option`.
///
/// `emit_safepoint_map` already refuses to conflate "nothing to cover" with
/// "could not describe it" (an oop-free safepoint publishes
/// `moving_young_coverage_complete: true`; an indescribable slot publishes
/// `false` and diverts the cycle). A level-2 form owes the same distinction,
/// because the two have opposite consequences: one is a healthy compile, the
/// other is a compile whose allocation nothing proved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum MirAllocVerdict {
    /// An allocation was built over the tile list and `verify_allocation`
    /// accepted it.
    Verified {
        /// Values the allocation gives a location to.
        located: usize,
        /// Tiles whose root the allocation gives NO location.
        ///
        /// Legal, and worth counting rather than refusing: `plan_slots` gives
        /// no home word to a value nothing reads from memory — the last value
        /// in `(a + b) * a` is returned straight out of RAX — while
        /// `select_block` still roots a tile at it, because a tile is about
        /// *computing* the value, not about storing it. The encoder already
        /// declines such a tile (`frame_destination(dst)?`) and the per-opcode
        /// arm emits it instead.
        ///
        /// It is a number rather than a silence because it is exactly the
        /// gap increment 3 closes: a value with no home is a value that MUST
        /// stay in a register, so this count is the size of the register file's
        /// first real customer.
        unlocated_tile_roots: usize,
    },
    /// The tile list defines no value that needs a location. Vacuous, and
    /// counted separately so a run of these cannot be read as coverage.
    NothingToCover,
    /// The allocation could not be *described*, before any verifier ran: the
    /// liveness model did not converge, or it disagrees with `plan_slots` about
    /// which values want a location.
    ///
    /// Not a failure. `plan_register_residency` treats exactly the same
    /// disagreement as "decline to promote and keep the colourer's answer",
    /// and the reasoning carries: the colourer's layout is correct on its own
    /// terms and is the one the frame was built from. What this must not do is
    /// report itself as `Verified`.
    Indescribable(&'static str),
}

/// Build an allocation over the machine list and put it through the **existing**
/// [`crate::regalloc::verify_allocation`].
///
/// This is increment 2 of
/// `docs/feature-designs/jit-machine-level-and-instruction-selection.md`: "an
/// allocation over it, verified by `verify_allocation`". Deliberately not a new
/// verifier — the value of the increment is that the level-2 artifact is
/// checkable by the allocator's *own* checker, and a second checker written for
/// it would be a second model to keep in step.
///
/// # What the allocation says
///
/// Every value lives in its frame home for its entire live range, with an empty
/// register file. That is not a placeholder: it is exactly what
/// [`Lowerer::encode_tile_frame_homed`] emits, so the allocation being verified
/// is the one the encoder actually implements. The register file is empty
/// rather than XMM-shaped because the MIR encoder holds nothing in a long-lived
/// register — RAX is reloaded per tile.
///
/// # What that buys, given nothing is in a register
///
/// The register rules go quiet; four others do not, and they are the ones that
/// bite a memory-only backend:
///
///   * **the timeline covers the live range** — a value the tile list reads
///     after its recorded death, or defines without a location, is refused
///     here instead of `?`-ing out silently inside the encoder;
///   * **no two simultaneously live values share a home word** — checked
///     against `build_live_model`'s ranges, which are computed *independently*
///     of `plan_slots`'. Two implementations of one liveness model drifting
///     apart is precisely how a backend comes to alias two live values, and
///     the tile list is a third consumer of that model;
///   * **home pools stay separated** — a word holding a `Ref` for one value and
///     a pinned primitive for another is the shape a moving collector
///     misreads;
///   * **no reference is register-resident at a safepoint** — vacuous today
///     and the whole point tomorrow: increment 3 swaps the empty `RegFile` for
///     a real one at this exact call site, and this check is what makes that
///     swap reviewable rather than an act of faith.
///
/// # Failure policy
///
/// `Err` only from `verify_allocation`, and only that. A model disagreement is
/// [`MirAllocVerdict::Indescribable`], not an error, for the reason
/// `plan_register_residency` gives. The caller decides what an `Err` costs:
/// `MirMode::Verify` changes no emitted byte and records it, `MirMode::Emit`
/// refuses the compile, because emitting against an allocation nothing proved
/// is the trade that has produced silent heap corruption in this VM before.
pub(in crate::ir_lower) fn verify_mir_allocation(
    graph: &Graph,
    schedule: &Schedule,
    plan: &SlotPlan,
    mir: &MirPlan,
) -> CompileResult<MirAllocVerdict> {
    use crate::regalloc::{
        build_live_model, verify_allocation, Allocation, RegFile, RegSpec, Segment,
    };

    let live = build_live_model(graph, schedule);
    if !live.converged {
        return Ok(MirAllocVerdict::Indescribable("liveness did not converge"));
    }
    // The same agreement check `plan_register_residency` runs, and for the same
    // reason: `build_live_model` re-implements `plan_slots`' position model
    // rather than calling it, so the two are only interchangeable while they
    // agree. Checked before either is used.
    let expected_positions: usize = schedule
        .blocks
        .iter()
        .map(|b| b.nodes.len() + usize::from(b.terminator.is_some()) + 1)
        .sum();
    if live.total_positions != expected_positions {
        return Ok(MirAllocVerdict::Indescribable("position models disagree"));
    }
    if live.wants_loc.len() != plan.node_color.len()
        || live
            .wants_loc
            .iter()
            .zip(plan.node_color.iter())
            .any(|(wants, color)| *wants != color.is_some())
    {
        return Ok(MirAllocVerdict::Indescribable(
            "liveness and the colourer disagree about which values want a location",
        ));
    }

    let n = graph.nodes.len();
    let mut segments: Vec<Vec<Segment>> = vec![Vec::new(); n];
    let mut located = 0usize;
    for id in 0..n {
        if !live.wants_loc.get(id).copied().unwrap_or(false) {
            continue;
        }
        let Some(range) = live.range.get(id).copied().flatten() else {
            // `wants_loc` without a range is the model contradicting itself.
            // Describing it would mean inventing a range, so say so instead.
            return Ok(MirAllocVerdict::Indescribable(
                "a value wants a location but has no live range",
            ));
        };
        segments[id].push(Segment { range, reg: None });
        located += 1;
    }
    if located == 0 {
        return Ok(MirAllocVerdict::NothingToCover);
    }

    // How many tiles the allocation cannot back. Counted, not refused — see
    // `MirAllocVerdict::Verified::unlocated_tile_roots` for why a tile rooted
    // at a homeless value is the normal case rather than a defect.
    let unlocated_tile_roots = mir
        .tile_of
        .iter()
        .enumerate()
        .filter(|(id, cell)| cell.is_some() && segments.get(*id).is_none_or(|s| s.is_empty()))
        .count();

    let alloc = Allocation {
        segments,
        // `Allocation::stack_slot`'s own contract: "a drop-in replacement for
        // `ir_lower`'s `SlotPlan::node_color` — same numbering, same
        // Ref/Prim/pinned pool separation". Passing the colourer's answer
        // rather than a re-derived one is what makes the home-aliasing check
        // above a check of the *frame the encoder addresses*, not of a
        // parallel invention.
        stack_slot: plan.node_color.clone(),
        stack_slots: plan.slots,
        spills: 0,
        reloads: 0,
        remats: 0,
        reg_moves: 0,
        splits: 0,
        promoted: 0,
        events: Vec::new(),
        peak_live: live.peak_live,
    };
    let empty_file = RegFile::from_specs(std::iter::empty::<RegSpec>());
    let model = ir_lower_machine_model(graph, schedule, &live, empty_file, None);
    verify_allocation(graph, &live, &model, &alloc)?;
    Ok(MirAllocVerdict::Verified {
        located,
        unlocated_tile_roots,
    })
}

/// Does a register for `id` pay for itself, counting loop frequency?
///
/// The publish is one memory→register load at the definition; each read it
/// replaces is one memory→register load at its own site. So the trade is
/// `uses_frequency >= 2 × definition_frequency`, and the static `use_count >= 2`
/// is that same test with every frequency pinned to 1.
///
/// `live.weight[id]` is already the loop-frequency-weighted use count. The
/// definition's frequency is derived from `live.loop_depth` and `live.span`,
/// both public, using the same `LOOP_WEIGHT_PER_DEPTH` model — so the two sides
/// of the comparison come from one model rather than two.
///
/// Falls back to the static rule whenever the loop model cannot place the
/// definition, which keeps a graph the scheduler left unusual on the old
/// behaviour rather than on a guess.
pub(in crate::ir_lower) fn ir_residency_pays_here(
    live: &crate::regalloc::LiveModel,
    schedule: &Schedule,
    id: usize,
    static_uses: u32,
) -> bool {
    if !ir_residency_loop_weight_enabled() {
        return static_uses >= 2;
    }
    let Some(def_pos) = live.pos_of.get(id).copied().flatten() else {
        return static_uses >= 2;
    };
    // Which block holds the definition? `span[b]` is that block's
    // (first position, outgoing-edge position).
    let mut def_depth: Option<u32> = None;
    for b in 0..schedule.blocks.len() {
        if let Some(&(lo, hi)) = live.span.get(b) {
            if lo <= def_pos && def_pos <= hi {
                def_depth = live.loop_depth.get(b).copied();
                break;
            }
        }
    }
    let Some(depth) = def_depth else {
        return static_uses >= 2;
    };
    let def_freq = u64::from(
        crate::regalloc::LOOP_WEIGHT_PER_DEPTH
            .checked_pow(depth)
            .unwrap_or(crate::regalloc::MAX_LOOP_DEPTH_WEIGHT)
            .min(crate::regalloc::MAX_LOOP_DEPTH_WEIGHT),
    );
    let uses_freq = live.weight.get(id).copied().unwrap_or(0);
    uses_freq >= def_freq.saturating_mul(2)
}

/// Weigh the residency trade by loop frequency instead of a static use count —
/// **default OFF**, opt in with `CRATONVM_JIT_IR_LS_LOOP_WEIGHT=1`.
///
/// Off is the static `use_count >= 2` this file has always used.
pub(in crate::ir_lower) fn ir_residency_loop_weight_enabled() -> bool {
    // t1 wave 3 (lane Y): read per call, no process `OnceLock` latch.
    false
}



/// Copy a loop-live int/long PARAMETER into a callee-saved register at entry —
/// **default on**, opt in with `CRATONVM_JIT_IR_PARAM_COPY=1`.
///
/// The optimizing tier cannot promote an entry parameter at all: they are
/// pinned to their incoming ABI registers, which are caller-saved and outside
/// this file, and the allocator skips a pinned value. The baseline tier copies
/// them into callee-saved registers in its prologue and reads them from there;
/// this is that copy.
///
/// Off is exactly the previous emission: the parameter reaches every use
/// through its frame slot.
pub(in crate::ir_lower) fn ir_param_prologue_copy_enabled() -> bool {
    // t1 wave 3 (lane Y): read per call, no process `OnceLock` latch.
    true
}



/// Seed each block's null proofs with the receiver — **default ON since round
/// 16**; `CRATONVM_JIT_IR_THIS_NONNULL=0` (or `false`/`off`/`no`) is the kill
/// switch. Round 16 measured it as a bundle with
/// `CRATONVM_JIT_IR_FIELD_STORE_FORWARD` and
/// `CRATONVM_JIT_IR_STORE_NULL_CHECK_CSE` (whose store null checks use the
/// seed): SyncM p 14→12 ns, alloc kernel -13 %, in two interleaved campaigns.
/// The history below is why it was off until then.
///
/// # It was off because it measured SLOWER, which nobody expected (2026-09-03)
///
/// It is correct and it engages (`seeded=9 elided=2 emitted=0` against
/// `0/0/2` with it off, same answer). On the loop it was built for — the one
/// the 2026-09-03 tier comparison found inverted — it is **~20% slower with
/// the check removed than with it emitted**: medians 1.78/1.93 on against
/// 1.49/1.61 off, two replicate pairs, within-config spread 8%, same direction
/// both times.
///
/// **The cost is not compile time.** At `reps=1`, where the loop barely runs,
/// the two arms are 0.19 against 0.18 — so the per-block seed scan is ~0.01s
/// and the 0.3s is in the emitted code.
///
/// Removing two instructions cannot make a loop 20% slower on its own, so what
/// this really says is that the body is dominated by something layout- or
/// branch-structure-sensitive, and deleting a never-taken forward `JZ` moved
/// it. That is a lead worth pulling for the residual tier inversion itself,
/// and it is the reason this switch stays available rather than being deleted:
/// it is the smallest known perturbation that moves that loop by 20%.
///
/// If nobody finds the cause, withdraw it.
///
/// The optimizing-tier half of `CRATONVM_JIT_THIS_NONNULL`, which seeds the
/// single-pass backend's null-check dataflow. Separate switch because the two
/// tiers reach the fact by different routes — a bytecode dataflow there, the
/// graph's `receiver_param` here — and a single flag would make a bisect
/// unable to say which one moved.
///
/// Off restores the previous emission exactly: a `TEST`/`JZ` at every
/// `getfield` whose receiver the per-block CSE has not already proven.
///
/// `pub(crate)` since round 17 wave 2 (lane monitor2): `ir_optimize`'s loop
/// block coarsening admits a `synchronized (this.lock)` re-read only when this
/// seed drops its null check in every block.
pub(crate) fn ir_this_nonnull_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        true
    })
}

/// Run the linear-scan allocator and use its result as a register read cache.
/// **Default ON** since 2026-09-02; `CRATONVM_JIT_IR_LINEAR_SCAN=0` opts out.
///
/// # What this file gained, and how the default moved
///
/// It shipped OFF while the file was XMM-only, and while it was XMM-only that
/// was the right default: the wiring's own comment said *"this wiring is still
/// FP-only. An `int` loop counter gets nothing out of it"*, so turning it on
/// bought two callee-saved XMM registers on Windows and nothing anywhere else.
///
/// [`IR_LOWER_LS_GPRS`] closes that half. It is the answer to a measured
/// inversion: this backend read every `int` value out of a frame word while the
/// single-pass backend it supersedes colours Java locals into callee-saved
/// GPRs, and on five one-line kernels — same binary, `CRATONVM_JIT_IR_LONG=0`
/// to route them to single-pass — the optimizing tier came out **1.25x to 1.9x
/// slower** on Windows and **~3.2x** slower on a quieter Linux host, on every
/// kernel whose cost was not already dominated by an out-of-line helper call.
///
/// **The default stayed off at first because the fix did not reach those
/// kernels, and the census said so rather than a guess.** With
/// `CRATONVM_DBG_IR_LINEAR_SCAN=1`, at that time:
///
/// * on `BinTrees.itemCheck` the file works —
///   `resident=7 (fp=0 gp=7) demoted=0`, `phi=0`, with 13 candidates lost to
///   splits and 2 to type;
/// * on all five probe kernels it never runs at all: `refused: liveness and
///   colourer disagree about which values want a home`, 5 of 5.
///
/// That refusal is a PRE-EXISTING gate, not something the GP file introduced —
/// `regalloc::ir_op_defines_value` and `ir_lower`'s `op_defines_result_slot`
/// are two enumerations of one question (the second and third of the three
/// `the_three_ir_op_enumerations` names), and any disagreement declines the
/// whole method. It was declining the XMM cache the same way and nobody could
/// see it, because the flag printed only on success. The refusal now names the
/// op, so reconciling the two is a one-line fix with a test rather than a
/// search through fifty variants.
///
/// **Both prerequisites landed on 2026-09-02 and the default moved with them.**
/// The enumeration disagreement was the one `ir_op_defines_value` had over
/// `Op::ArrayLength` and `Op::NewArray` — it declined the file on every method
/// containing an `arraylength`, which is every counted `for` loop over an
/// array. Phis are admitted separately (`ir_phi_residency_enabled`). The
/// regression suite is green with the file on (88/88).
///
/// **What the flip did NOT settle** is the tiering inversion this paragraph
/// was written about. Re-measured 2026-09-03 on a RELEASE binary, arms
/// interleaved, with a second arm of each configuration to establish the noise
/// floor: four of five loop shapes are indistinguishable between tiers, and the
/// field-read loop is still **~1.65x slower at the optimizing tier**
/// (C1 0.86/0.83 against C2 1.39/1.41, fifteen reps, controls agreeing to 3.5%
/// and 1.4%). So this file closed the part of the gap it was built for and the
/// remaining case is elsewhere — see `docs/JIT_OPTIMIZATION.md`.
///
/// **Retaken 2026-09-10** on `probes/FieldLoop.java` `sum`, after seven more
/// register flags went default-ON: **1.594x** (503 ms against 812 ms over a
/// 2.6% floor). Still there, and widening the GP file does not close it
/// either — see [`ir_gp_wide_enabled`].
///
/// Off is exactly the pre-change emission: no register is handed out, no save
/// area is reserved, every read goes to its home word.
///
/// Declared in `types/src/flag_groups.rs` as `jit/ir-linear-scan`, so `-XX:`
/// options and `flags::with_thread_overrides` reach it.
pub(in crate::ir_lower) fn linear_scan_enabled() -> bool {
    #[cfg(test)]
    {
        if let Some(forced) = ls_forced() {
            return forced;
        }
    }
    // 2026-09-02: DEFAULT ON. The enumeration disagreement that declined the
    // file on every array-touching method was reconciled the same day, phis
    // are admitted (`ir_phi_residency_enabled`), and the census below says
    // where the file engages. `=0` is the kill switch.
    !matches!(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
        Ok("0") | Ok("false") | Ok("off") | Ok("no")
    )
}

/// `CRATONVM_JIT_IR_LOWER_TOKEN_READ_GUARD=1` (opt-in; interpreter round i2
/// wave 64, lane L2; `i63-L2-proposal-the-lowering-refuses-a-token-slot-operand-read`):
/// refuse a compile in which a lowering arm reads a machine value through an
/// input it names ONLY in a memory-token slot (`ir::is_memory_token_slot`).
///
/// `CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES` (JIT round 17), the slot-plan
/// switch `CRATONVM_JIT_IR_SLOTS_TOKEN_EDGES_NOT_USES` (wave 64) and the
/// wave-58 OSR excuse all assume no arm does: a token is an ordering edge, the
/// arm never `slot_of`s it, never asks for its register. This turns the
/// assumption into a checked one -- see [`Lowerer::token_read_check`] for the
/// funnels and what is exempt. Compile-time only; costs nothing at run time,
/// and nothing at compile time while off (the per-node set stays empty).
///
/// Implied by the slot-plan switch: a shortened home some arm still reads is
/// the wrong-code case this exists to refuse.
pub(in crate::ir_lower) fn ir_lower_token_read_guard_enabled() -> bool {
    false
        || crate::regalloc::slots_token_edges_not_uses_enabled()
        || (cfg!(debug_assertions) && ir_lower_token_read_guard_debug_default_enabled())
}

/// `CRATONVM_JIT_IR_LOWER_TOKEN_READ_GUARD_DEBUG_DEFAULT=1` (default on; JIT round
/// 19 proposal wave 11, lane lv11, TOK10-2): in a build with
/// `debug_assertions`, the token-read guard is on unless
/// `CRATONVM_JIT_IR_LOWER_TOKEN_READ_GUARD` says `0` (or another off word).
/// No effect in a release build. Flipping this switch default ON is the
/// TOK10-2 decision, taken on TOK10-1's count (the strict lib-test run) being
/// zero; a debug-only refusal changes which methods a debug build compiles,
/// which is the cost the flip weighs.
pub(in crate::ir_lower) fn ir_lower_token_read_guard_debug_default_enabled() -> bool {
    true
        && !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
                .ok()
                .map(|v| cratonvm_types::flags::value_is_off(&v)),
            Some(true)
        )
}

/// One location transition the emission must materialise for a value the
/// linear scan SPLIT.
///
/// The machine-level form of a `regalloc::SpillEvent`, narrowed to the four
/// shapes this backend can emit between two nodes without a scratch register.
/// `gp` picks the bank; a node's type picks its bank once and for all, so the
/// flag is carried rather than re-derived at every emission site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum LsTransition {
    /// `reg ← [home(node)]`. A `SpillKind::Load`, and also what a `Move` and a
    /// non-integer `Remat` degrade to.
    Load { node: NodeId, reg: u8, gp: bool },
    /// `reg ← imm`. A `SpillKind::Remat` of an `Op::Const`: no memory access,
    /// which is the entire point of the kind.
    RematConst { node: NodeId, reg: u8, value: i64 },
    /// `reg ← base + addend` (`LEA`, or `MOV`+`ADD` with the flags dead): a
    /// planned `SpillKind::Load` of an `int`/`long` `x + c`, under
    /// `CRATONVM_JIT_IR_LS_REMAT_ADD` (j27 wave 4). GP only. Degrades to the
    /// home load at emission unless the base is resident there; see
    /// `ls_remat_add`. Sequenced like a load (its source is no register).
    RematAdd {
        node: NodeId,
        reg: u8,
        remat: crate::regalloc::RematAdd,
    },
    /// `to ← from`. A `SpillKind::Move`, already sequentialised against every
    /// other move at this position by `regalloc::resolve_parallel_copy`.
    Move {
        node: NodeId,
        from: u8,
        to: u8,
        gp: bool,
    },
    /// `node`'s value is in its home word from here on. Emits nothing; see
    /// [`Lowerer::ls_drop_residency`].
    Drop { node: NodeId, gp: bool },
}

impl LsTransition {
    /// The value this transition is about.
    pub(in crate::ir_lower) fn node(self) -> NodeId {
        match self {
            LsTransition::Load { node, .. }
            | LsTransition::RematConst { node, .. }
            | LsTransition::RematAdd { node, .. }
            | LsTransition::Move { node, .. }
            | LsTransition::Drop { node, .. } => node,
        }
    }

    /// `(register, is_gp)` this transition publishes into, if it publishes.
    ///
    /// `None` for a [`LsTransition::Drop`], which takes a register away rather
    /// than filling one. Read by `set_residency` to build the save-set
    /// snapshot, so a transition kind added without an arm here would leave a
    /// register written by the body and not saved by the prologue — the
    /// exhaustive match is what makes that a compile error.
    pub(in crate::ir_lower) fn destination(self) -> Option<(u8, bool)> {
        match self {
            LsTransition::Load { reg, gp, .. } => Some((reg, gp)),
            // A remat is integer-only; see `ls_split_transitions`.
            LsTransition::RematConst { reg, .. } | LsTransition::RematAdd { reg, .. } => {
                Some((reg, true))
            }
            LsTransition::Move { to, gp, .. } => Some((to, gp)),
            LsTransition::Drop { .. } => None,
        }
    }
}

/// Where every SPLIT value is, position by position, and what to emit at each
/// transition.
///
/// # Why this exists
///
/// `plan_register_residency` used to answer one question per value — "does it
/// hold one register over its entire live range?" — and decline everything
/// else. That threw away the allocator's whole answer: a value given a register
/// everywhere except across one call came back with no register at all and paid
/// a home store plus a reload at *every* use, exactly as it did before the
/// allocator existed. `Allocation::events`, `SpillKind::Remat`, `reg_moves`,
/// `splits` and the eviction cost model that produces them had no reader
/// outside `verify_allocation` and the tests. See `NOTES-regalloc.md` §1.
///
/// This is the reader. Empty unless [`ir_ls_splits_enabled`].
///
/// # What a managed value gives up
///
/// Three things, and each is a refusal rather than a hope:
///
/// * **Its home word stays.** `set_residency` withholds `home_dropped` from
///   every managed value, because the transitions LOAD from that word.
/// * **No deopt frame names its register.** `set_residency` poisons the
///   exclusive-ownership count for every register a managed value visits, so
///   `register_frame_value` answers `None` — a frame state says "this value is
///   in RBX" with no position attached, and a split value is in RBX only
///   sometimes.
/// * **Every transition sits at a NODE position.** Not at a terminator, not at
///   an outgoing edge. That is what keeps every publish inside `lower_block`'s
///   per-node invalidation audit, which is the whole of the argument that a
///   `Ref` in a register cannot outlive a point where a collector ran.
#[derive(Clone, Debug, Default)]
pub(in crate::ir_lower) struct LsSplitPlan {
    /// `at[pos]` = what to materialise immediately before the node at linear
    /// position `pos`. Sized to the whole position space; empty nearly
    /// everywhere.
    pub(in crate::ir_lower) at: Vec<Vec<LsTransition>>,
    /// `block_start[b]` = block `b`'s first linear position.
    pub(in crate::ir_lower) block_start: Vec<usize>,
    /// `managed[id]` = this value's residency is position-driven.
    pub(in crate::ir_lower) managed: Vec<bool>,
    /// How many values that is, and how many transitions they cost.
    pub(in crate::ir_lower) values: usize,
    pub(in crate::ir_lower) transitions: usize,
    /// The CFG-edge resolution copies for managed values the split timeline
    /// leaves in the wrong place on some non-fall-through edge. Empty unless
    /// [`ir_ls_edge_resolution_enabled`]; see [`LsEdgeCopies`].
    pub(in crate::ir_lower) edges: Vec<LsEdgeCopies>,
}

/// The edge-resolution copies one CFG edge owes (round 9 wave 6, lane
/// irlower6; `linear-scan-split-timeline-has-no-edge-resolution-20260918.md`).
///
/// `copies[i] = (node, reg, gp)`: before control reaches `succ` along
/// `pred -> succ`, `reg` (in the GP bank when `gp`) must hold `node`, because
/// the code of `succ` was emitted against the LAYOUT predecessor's state, in
/// which it does. The source is always the value's home word, never a
/// register, and that is a choice rather than a shortcut:
///
/// * a managed value's home is never dropped and is written at the definition,
///   which dominates every block the value is live into, so the word is
///   always right on this edge;
/// * the copies then read no register at all, so no order among them and no
///   order against the edge's phi copies can make one read a register another
///   has already overwritten -- the whole of the parallel-copy hazard is gone,
///   and no scratch is needed for a cycle;
/// * the one remaining interaction, a destination register one of the edge's
///   phis is published into, is refused by the planner (the allocator's
///   `held_by_other` test at the target's first position) and checked again at
///   emission (`Lowerer::ls_emit_edge_copies`).
///
/// The price is one load per repaired value per edge execution, on a path that
/// used to have no register for that value at all.
#[derive(Clone, Debug, Default)]
pub(in crate::ir_lower) struct LsEdgeCopies {
    pub(in crate::ir_lower) pred: usize,
    pub(in crate::ir_lower) succ: usize,
    pub(in crate::ir_lower) copies: Vec<(NodeId, u8, bool)>,
}

/// `CRATONVM_JIT_IR_LS_EDGE_RESOLUTION=1` -- **default OFF**, and inert unless
/// `CRATONVM_JIT_IR_LS_SPLITS` is on as well (round 9 wave 6).
///
/// Off, a split value the allocator leaves in the wrong place on some CFG edge
/// is refused split residency (`regalloc::split_edge_conflicts`, the wave-1
/// gate). On, such a value is admitted when `regalloc::split_edge_resolution`
/// can repair every one of its edges and every such edge leaves a block whose
/// lowering has an edge-specific copy site (`ls_edge_copy_site_exists`); the
/// repair is emitted by `Lowerer::ls_emit_edge_copies`.
///
/// Read uncached, like `ir_ls_splits_enabled`, because it is asked once per
/// compile and the tests flip it per case.
pub(in crate::ir_lower) fn ir_ls_edge_resolution_enabled() -> bool {
    false
}

/// Does lowering block `pred` emit a copy site that runs on exactly one
/// outgoing edge, for EVERY outgoing edge -- the place `emit_phi_copies` (and
/// with it `Lowerer::ls_emit_edge_copies`) runs?
///
/// Two terminator shapes do: no terminator at all (`lower_block`'s goto path,
/// one successor, copies after the back-edge poll), and `Op::If`, all of whose
/// arms put an edge's copies on straight-line code of that edge alone once
/// `edge_has_phi_copies` answers `true` for it (which it does for an edge
/// carrying resolution copies). Everything else -- a switch above all, which
/// latches a bailout on any edge with copies -- is refused, and the value that
/// would need a copy there keeps the wave-1 refusal.
///
/// A prediction, and checked: `lower_inner_attempt` refuses the compile if
/// an edge's copies were never emitted.
pub(in crate::ir_lower) fn ls_edge_copy_site_exists(graph: &Graph, schedule: &Schedule, pred: usize) -> bool {
    let Some(block) = schedule.blocks.get(pred) else {
        return false;
    };
    match block.terminator {
        None => block.successors.len() == 1,
        Some(term) => matches!(
            graph.nodes.get(term as usize).map(|node| &node.op),
            Some(Op::If)
        ),
    }
}

/// Which bank a `(type, register)` pair belongs to, or `None` when the pair is
/// one neither file may hold.
///
/// The same decision the single-segment path makes inline, as a function
/// because the split path has to make it once per SEGMENT and the two must not
/// drift. The `Ref` arm is deliberately identical, obligation included: a
/// reference in a GP register is admitted only under
/// `CRATONVM_JIT_IR_REF_RESIDENCY`, whose price is `invalidate_ref_residency`
/// dropping the cached copy wherever a collector could have run. `Ref` is never
/// in the FP file at all, so the FP arm needs no such clause.
pub(in crate::ir_lower) fn ls_bank_for(ty: Option<IrType>, reg: crate::regalloc::PhysReg) -> Option<bool> {
    use crate::regalloc::RegClass;
    match (reg.class, ty) {
        (RegClass::Xmm, Some(IrType::Float) | Some(IrType::Double))
            if IR_LOWER_LS_XMMS.contains(&reg.num) =>
        {
            Some(false)
        }
        (RegClass::Gp, Some(IrType::Int) | Some(IrType::Long))
            if IR_LOWER_LS_GPRS.contains(&reg.num) =>
        {
            Some(true)
        }
        (RegClass::Gp, Some(IrType::Ref))
            if ir_ref_residency_enabled() && IR_LOWER_LS_GPRS.contains(&reg.num) =>
        {
            Some(true)
        }
        _ => None,
    }
}

/// Every transition node `id` needs, or `None` when this file cannot place it
/// and it must fall back to no register at all.
///
/// # Where the transitions come from
///
/// The `Load` / `Remat` / `Move` set is `alloc.events`, filtered to this node —
/// those are the allocator's own statement of what has to happen and where.
/// The **drops** are not in `events` and have to come from the timeline: a
/// `SpillKind::Store`'s `pos` is the last position at which the register is
/// still GOOD, not the position at which it stops being good, and a
/// rematerializable value produces no `Store` at all while still leaving its
/// register. `segments` is the thing `location_at` reads, so taking the drop
/// from the segment boundary is taking it from the authority.
///
/// # Why `SpillKind::Store` emits nothing
///
/// Because this backend is write-through. `store_rax` / `gp_store_value` /
/// `fp_store_value` write the home word at EVERY definition, promoted or not,
/// and `set_residency` refuses a dropped home to every managed value — so by
/// the time any transition runs, the home already holds the value and a store
/// would write bytes that are already there. This also disposes of the one
/// ordering hazard `Store` carries: its `pos` may be the definition's own
/// position, which is a position this emitter reaches BEFORE the definition has
/// run.
///
/// # The refusals, and why each is a refusal
///
/// * **A transition outside the node positions.** A terminator or an outgoing
///   edge is outside `lower_block`'s per-node loop, so a `Ref` published there
///   would survive the back-edge poll's own call with nothing to drop it, and
///   an operand published there would have to be re-proved against
///   `emit_phi_copies`' own register discipline. Neither argument is written,
///   so the value is refused.
/// * **A transition at or before the definition.** Liveness is widened to whole
///   block spans, so a loop-carried value's range can START before the node
///   that defines it. A load from the home word there reads bytes nothing has
///   written — an `int` with a garbage value, or for an `IrType::Ref` a
///   fabricated pointer handed to the collector.
/// * **A segment register outside this value's bank or file.** The same
///   defensive test the single-segment path makes, applied per segment.
/// * **An event with no destination.** Nothing but `Store` has one legitimately,
///   and an unfamiliar shape is refused rather than guessed at.
pub(in crate::ir_lower) fn ls_split_transitions(
    graph: &Graph,
    alloc: &crate::regalloc::Allocation,
    id: usize,
    gp: bool,
    def: usize,
    first: Option<u8>,
    node_pos: &[bool],
) -> Option<Vec<(usize, LsTransition)>> {
    use crate::regalloc::{remat_of, Remat, SpillKind};
    let node = id as NodeId;
    let segs = alloc.segments.get(id)?;
    let mut out: Vec<(usize, LsTransition)> = Vec::new();

    // Register → home, read off the timeline. See the doc comment.
    for pair in segs.windows(2) {
        if pair[0].reg.is_some() && pair[1].reg.is_none() {
            out.push((pair[1].range.lo, LsTransition::Drop { node, gp }));
        }
    }

    for event in alloc.events.iter().filter(|e| e.node == node) {
        if event.kind == SpillKind::Store {
            continue;
        }
        let to = event.to?;
        let transition = match event.kind {
            // j27 wave 4: an `x + c` reload may become a `RematAdd`
            // (`CRATONVM_JIT_IR_LS_REMAT_ADD`); every other one is the load.
            SpillKind::Load => ls_remat_add::ls_reload_transition(graph, node, to.num, gp),
            SpillKind::Remat => match remat_of(graph, node) {
                // The only true re-materialisation this backend can perform
                // between two nodes. Everything else would need a scratch
                // register, and there is none here that is free by
                // construction: RAX may be carrying a value to the next node
                // and RCX a deferred one.
                //
                // `Remat::ConstF` is the case that names the constraint — the
                // bit pattern reaches an XMM register through a GP immediate
                // and a `movq`, so it costs the very register the carry is in.
                // `Remat::Param` would be a load from the incoming argument
                // slot, which is a second home word for the same value and
                // buys nothing over the one below.
                //
                // Both therefore lower as a home load, which is correct for
                // exactly the reason the `Store` paragraph above gives: the
                // home word is written at the definition, the definition
                // precedes every transition, and no managed value has a
                // dropped home.
                Some(Remat::Const(value)) if gp => LsTransition::RematConst {
                    node,
                    reg: to.num,
                    value,
                },
                _ => LsTransition::Load {
                    node,
                    reg: to.num,
                    gp,
                },
            },
            SpillKind::Move => LsTransition::Move {
                node,
                from: event.from?.num,
                to: to.num,
                gp,
            },
            // Filtered above. Spelled as a `continue` and not an
            // `unreachable!`, because a compiler that panics on a shape it did
            // not expect is a VM that dies on a method it could have declined.
            SpillKind::Store => continue,
        };
        out.push((event.pos, transition));
    }

    out.sort_by_key(|&(pos, _)| pos);
    let mut kept: Vec<(usize, LsTransition)> = Vec::with_capacity(out.len());
    for (pos, transition) in out {
        if pos < def || !node_pos.get(pos).copied().unwrap_or(false) {
            return None;
        }
        if pos == def {
            // A transition AT the definition is realised by the definition's
            // own write-through publish, which puts the value in `first` — so
            // emitting one here would either duplicate that publish or, if it
            // named a different register, contradict it. The first is elided;
            // the second is refused, because a contradiction between the
            // allocator's timeline and this file's publish site is not
            // something to resolve by picking one.
            match (transition.destination(), first) {
                (Some((reg, _)), Some(f)) if reg == f => continue,
                (None, None) => continue,
                _ => return None,
            }
        }
        kept.push((pos, transition));
    }
    Some(kept)
}

/// Can this file place split value `id` position by position, and if so where
/// does it start and what does it owe?
///
/// `Some((gp, first, transitions))`: which bank, the register the value's own
/// definition should publish into (`None` when the allocator has it in memory
/// at that point), and every transition after that.
///
/// Refuses, and the caller then falls back to today's behaviour — no register
/// at all — rather than to a guess:
///
/// * no home word from `plan_slots`, which write-through has nothing to write
///   to and no transition has anything to load from;
/// * no definition position, so "after the definition" is not a question this
///   pass can answer;
/// * a segment register the type's bank may not hold, or that is outside the
///   file this backend saves — the same defensive test the single-segment path
///   makes, per segment;
/// * two segments in DIFFERENT banks, which would mean a node changed type;
/// * no register anywhere in the timeline, which is a spilled value and not a
///   split one;
/// * anything [`ls_split_transitions`] cannot place.
pub(in crate::ir_lower) fn ls_split_place(
    graph: &Graph,
    plan: &SlotPlan,
    live: &crate::regalloc::LiveModel,
    alloc: &crate::regalloc::Allocation,
    id: usize,
    segs: &[crate::regalloc::Segment],
    node_pos: &[bool],
) -> Option<(bool, Option<u8>, Vec<(usize, LsTransition)>)> {
    use crate::regalloc::ValueLoc;
    // Write-through needs a home to write, and every transition below reads
    // one. This is the same requirement `skip_home` states for whole-range
    // values, asked earlier because the split path never reaches that check.
    plan.node_color.get(id).copied().flatten()?;
    let ty = graph.nodes.get(id).map(|node| node.ty);
    let def = live.pos_of.get(id).copied().flatten()?;

    let mut bank: Option<bool> = None;
    for seg in segs {
        let Some(reg) = seg.reg else { continue };
        let seg_bank = ls_bank_for(ty, reg)?;
        match bank {
            None => bank = Some(seg_bank),
            // A node has one type and a type picks one bank, so this is
            // unreachable unless `RegClass::of` or the bank match moved under
            // this code — which is exactly when a refusal is wanted.
            Some(b) if b == seg_bank => {}
            Some(_) => return None,
        }
    }
    let gp = bank?;

    // Where the value is at its OWN definition, which is what the definition's
    // write-through publish will put it in. Taken from `location_at` rather
    // than from `segs[0]`: liveness is widened to whole block spans, so a
    // loop-carried value's first segment can begin before the node that defines
    // it and `segs[0]` is then the wrong answer.
    let first = match alloc.location_at(id as NodeId, def) {
        Some(ValueLoc::Reg(reg)) => Some(reg.num),
        _ => None,
    };
    let transitions = ls_split_transitions(graph, alloc, id, gp, def, first, node_pos)?;
    // Counted only once the value is actually placed. The whole-range site
    // bumps this before its own second-opinion passes can take the register
    // back, which is already one source of over-count; adding a second, for
    // values this function is about to refuse outright, would make the number
    // mean nothing at all.
    if matches!(ty, Some(IrType::Ref)) {
        REF_RESIDENCY_ADMITTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    Some((gp, first, transitions))
}

/// Which linear positions are NODE positions — the ones `lower_block`'s
/// per-node loop reaches, and therefore the only ones a transition may sit at.
///
/// A terminator's position and a block's outgoing-edge position are the two
/// that are not here, and leaving them out is the safepoint argument rather
/// than a simplification: both are outside the per-node loop that invalidates
/// reference residency, so a `Ref` published at either would survive the
/// back-edge poll's own call with nothing to drop it. See
/// [`ls_split_transitions`].
pub(in crate::ir_lower) fn ls_node_positions(schedule: &Schedule, live: &crate::regalloc::LiveModel) -> Vec<bool> {
    let mut node_pos = vec![false; live.total_positions];
    for (b, block) in schedule.blocks.iter().enumerate() {
        let Some(&(start, _)) = live.span.get(b) else {
            continue;
        };
        for offset in 0..block.nodes.len() {
            if let Some(cell) = node_pos.get_mut(start + offset) {
                *cell = true;
            }
        }
    }
    node_pos
}

/// The location a transition's register denotes, in the allocator's vocabulary.
pub(in crate::ir_lower) fn ls_reg_loc(reg: u8, gp: bool) -> crate::regalloc::ValueLoc {
    use crate::regalloc::{PhysReg, ValueLoc};
    ValueLoc::Reg(if gp {
        PhysReg::gp(reg)
    } else {
        PhysReg::xmm(reg)
    })
}

/// Bucket every managed value's transitions by position and put each bucket in
/// an order that is safe to emit sequentially, or refuse the whole plan.
///
/// # Why an order is needed at all
///
/// The transitions at one position are SIMULTANEOUS. The allocator's statement
/// is "at position `p`, these values are in these registers"; it says nothing
/// about an order, and every order is not equally good. `a ← b` beside `b ← a`
/// is a swap, and emitting the two in sequence makes both `b`. A load into a
/// register that another value is still being moved out of destroys that
/// value's source the same way.
///
/// That is exactly a parallel copy, and `regalloc::resolve_parallel_copy` is
/// the sequencer for one — the same one `emit_phi_copies` feeds its edge webs
/// to. Loads and remats go into it as well as moves: their DESTINATION races
/// with everybody else's source even though their own source is memory (or an
/// immediate), and `ValueLoc::Slot` is how a source that is not a register is
/// spelled.
///
/// # Why a cycle refuses the whole method
///
/// Breaking a cycle costs a scratch register, and `CopyOp::Save` deliberately
/// does not name one — the backend is supposed to supply whatever it keeps free
/// at that point. Between two nodes this backend keeps nothing free: RAX may be
/// carrying a value to the next node and RCX a deferred one (`plan_carries`),
/// and the whole register file is either in use or callee-saved and owned by
/// somebody. So there is no scratch to supply, and the honest answer is to
/// decline every split value in the method rather than to pick a register and
/// hope. The caller sends them all back to memory, which is what they had
/// before this pass existed.
///
/// Also refuses when two transitions publish into ONE register at one position
/// (`resolve_parallel_copy` rejects a destination written twice), and when the
/// sequence comes back with a different number of moves than it was given —
/// both are states in which this file's model of the allocation has come apart
/// from the allocation, and neither is one to emit code from.
pub(in crate::ir_lower) fn ls_sequence_split_plan(
    taken: &[(usize, bool, Vec<(usize, LsTransition)>)],
    total_positions: usize,
) -> Option<Vec<Vec<LsTransition>>> {
    use crate::regalloc::{resolve_parallel_copy, CopyOp, ValueLoc};
    let mut buckets: Vec<Vec<LsTransition>> = vec![Vec::new(); total_positions];
    for (_, _, transitions) in taken {
        for &(pos, transition) in transitions {
            buckets.get_mut(pos)?.push(transition);
        }
    }
    for at in buckets.iter_mut() {
        // A drop emits nothing and `ls_apply_transitions` runs every drop
        // before any publish, so only the publishes need sequencing — and one
        // publish cannot race itself.
        let publishes: Vec<LsTransition> = at
            .iter()
            .copied()
            .filter(|t| t.destination().is_some())
            .collect();
        if publishes.len() < 2 {
            continue;
        }
        let mut copies: Vec<(ValueLoc, ValueLoc)> = Vec::with_capacity(publishes.len());
        for transition in &publishes {
            let (reg, gp) = transition.destination()?;
            let source = match *transition {
                LsTransition::Move { from, gp, .. } => ls_reg_loc(from, gp),
                // A load or a remat reads no register, so it depends on nothing
                // here and only has to be ordered after whoever still reads its
                // destination. `Slot` is how `resolve_parallel_copy` spells
                // "not a register"; a `Slot` can never equal a destination,
                // which are all `Reg`, so the index is free to be the node.
                _ => ValueLoc::Slot(transition.node()),
            };
            copies.push((ls_reg_loc(reg, gp), source));
        }
        let sequence = resolve_parallel_copy(&copies).ok()?;
        let mut ordered: Vec<LsTransition> = at
            .iter()
            .copied()
            .filter(|t| t.destination().is_none())
            .collect();
        for op in sequence {
            match op {
                CopyOp::Move { to, .. } => {
                    let found = publishes.iter().find(|t| match t.destination() {
                        Some((reg, gp)) => ls_reg_loc(reg, gp) == to,
                        None => false,
                    })?;
                    ordered.push(*found);
                }
                // A cycle. See the doc comment: there is no scratch here.
                CopyOp::Save { .. } | CopyOp::Restore { .. } => return None,
            }
        }
        if ordered.len() != at.len() {
            return None;
        }
        *at = ordered;
    }
    Some(buckets)
}

/// Which XMM register holds each value for its whole life, plus what the
/// decision cost.
#[derive(Clone, Debug, Default)]
pub(in crate::ir_lower) struct RegResidency {
    /// `reg_of[id]` = the XMM number holding node `id`'s value from its
    /// definition to its last use, with no split, no spill and no reload.
    /// `None` for everything else, which is the majority.
    ///
    /// For a value named by [`Self::splits`] this is instead the register the
    /// value's DEFINITION publishes into, and `ls_apply_transitions` re-points
    /// it at each transition. See [`LsSplitPlan`].
    pub(in crate::ir_lower) reg_of: Vec<Option<u8>>,
    /// `gp_reg_of[id]` = the same, in the GENERAL-PURPOSE file. Disjoint from
    /// `reg_of` by construction: a node has one type, and the two files serve
    /// disjoint type sets (FP here, int/long there, `Ref` in neither).
    pub(in crate::ir_lower) gp_reg_of: Vec<Option<u8>>,
    /// How many values that is.
    pub(in crate::ir_lower) promoted: usize,
    /// Values the allocator promoted that this file then refused, because the
    /// allocator's liveness model and `plan_slots`' disagreed about them. A
    /// non-zero count is a compiler bug worth chasing; it is not wrong code,
    /// because a refusal only sends the value back to memory.
    pub(in crate::ir_lower) demoted: usize,
    /// Copied out of the liveness model so the caller can report it.
    #[allow(dead_code)]
    pub(in crate::ir_lower) peak_live: usize,
    /// The position-driven half of the plan, for the values the scan SPLIT.
    /// Default (empty) unless [`ir_ls_splits_enabled`]; see [`LsSplitPlan`].
    pub(in crate::ir_lower) splits: LsSplitPlan,
    /// What happened to every value-typed `Op::Phi` in the graph. See
    /// [`PhiCensus`].
    pub(in crate::ir_lower) phi_census: PhiCensus,
    /// Per back-edge BLOCK: the FP values resident in a caller-saved XMM at
    /// that block's poll, which its slow path must reload from their home
    /// words (round 11 wave 6, see [`ir_poll_preserves_fp`]). Empty whenever
    /// the model still clobbers the XMM bank at the poll, because no value can
    /// then hold such a register there.
    pub(in crate::ir_lower) poll_fp_reloads: HashMap<usize, Vec<NodeId>>,
    /// Round 25 wave 2 (`CRATONVM_JIT_IR_LS_CALLER_SAVED`): `(after, value)`
    /// reloads into a caller-saved register, see `CallerSavedPlan::republish`.
    /// Empty unless the switch is on.
    pub(in crate::ir_lower) caller_saved_republish: Vec<(NodeId, NodeId)>,
}

/// Why each phi ended up where it did.
///
/// # Why a phi-scoped census beside the general one
///
/// The general skip census has a single `phi=` field, and until 2026-09-02 it
/// meant one thing: *"refused because it is a phi"*, unconditionally, because
/// [`plan_register_residency`] refused the op outright. Once phis are admitted
/// that field stops distinguishing the two answers a reader needs to tell
/// apart — **"the mechanism did not apply"** against **"the mechanism applied
/// and this phi lost anyway"** — and a single number that conflates them is
/// exactly the shape of diagnostic this file has been burned by twice: the
/// residency flag that printed only on success and hid a pre-existing refusal
/// for months, and the `droppable=0` line that was quoted as evidence a lever
/// was dead while the same compile was pulling it three times.
///
/// So each refusal gets its own field, and they are *disjoint*: a phi is
/// counted once and only once, under the first cause that stopped it.
///
/// # Attributed AFTER every pass, not at the refusal
///
/// The obvious implementation counts at each `continue` inside the scan loop.
/// That over-reports, because `ir_reserve_carried_enabled`'s pass runs
/// afterwards and can hand a register to a phi the scan refused — a loop
/// counter the scan split is exactly its target population. A census taken at
/// the `continue` would report that phi as `no_register` while the emitted code
/// reads it from RBX, which is the same class of error as the two above: a
/// diagnostic computing its own answer instead of reading the one the emission
/// used.
///
/// The causes are therefore *recorded* per node during the scan and *tallied*
/// at the end, against the final `reg_of`/`gp_reg_of`. A phi that any later
/// pass rescued is counted under [`Self::resident`] and its recorded cause is
/// discarded. What this census reports is what the emission does.
///
/// The accounting identity — every VALUE-typed phi in the graph lands in
/// exactly one field — is asserted in `debug_assert` form at the tally site,
/// for the same reason `census_home_blocks` asserts its own: a cause added to
/// the scan and not here makes the census read low, and a census that reads low
/// retires levers that are engaged.
///
/// # What is NOT counted
///
/// `Memory` phis. `IrBuilder` builds one at every merge where it builds a value
/// phi, so a counted loop has one at its header; it carries no runtime value,
/// `RegClass::of` answers `None` for it, and `allocate_linear_scan` never gives
/// it an interval. Counting it would put one guaranteed refusal per loop in
/// this census and make `refused == 0` unassertable on the very shape the
/// mechanism exists for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::ir_lower) struct PhiCensus {
    /// Phis holding a register in the emitted body, in either bank.
    pub(in crate::ir_lower) resident: usize,
    /// Refused because `CRATONVM_JIT_IR_PHI_RESIDENCY` is off. This is the
    /// "did not apply" bucket and the only one the pre-2026-09-02 `phi=` field
    /// ever counted.
    pub(in crate::ir_lower) disabled: usize,
    /// Refused because the phi's `IrType` is `Ref`. **A hard refusal, not a
    /// heuristic** — see the refusal site in [`plan_register_residency`].
    pub(in crate::ir_lower) ref_type: usize,
    /// Refused because the scan gave it no single whole-range register: no
    /// interval at all, a spilled interval, or a split one.
    ///
    /// A split one stays here even under `CRATONVM_JIT_IR_LS_SPLITS`, which
    /// admits split values everywhere else. A phi's publish site is
    /// `emit_phi_copies`, on the EDGE -- outside the per-node loop that owns
    /// the reference-invalidation audit, and with its own discipline for
    /// reading sources and staging destinations. Admitting one needs that
    /// argument written; it is not, so this is a refusal and not a `TODO`.
    pub(in crate::ir_lower) no_register: usize,
    /// Refused because `plan_slots` gave it no home word. Write-through has
    /// nothing to write to.
    pub(in crate::ir_lower) no_home: usize,
    /// Refused because its type is in neither bank for a reason other than
    /// `Ref` — which means this file or `RegClass::of` changed under the bank
    /// match, since a phi's type otherwise picks its bank.
    pub(in crate::ir_lower) wrong_bank: usize,
    /// Assigned a register and then taken back by one of the two second-opinion
    /// passes (an aliasing disagreement between the two liveness models, or a
    /// clobber `plan_slots`' range sees and the allocator's did not).
    pub(in crate::ir_lower) demoted: usize,
}

impl PhiCensus {
    /// Every phi accounted for, which by the identity asserted at the tally
    /// site is every value-typed `Op::Phi` in the graph.
    pub(in crate::ir_lower) fn total(&self) -> usize {
        self.resident
            + self.disabled
            + self.ref_type
            + self.no_register
            + self.no_home
            + self.wrong_bank
            + self.demoted
    }

    /// Phis that reached no register, whatever the cause.
    ///
    /// The number a test asserts zero on when it means "this loop's carried
    /// values all moved". Written as `total - resident` rather than as a sum of
    /// the refusals so that a cause added to [`PhiCensus`] and not to this
    /// method cannot make it read low.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(in crate::ir_lower) fn refused(&self) -> usize {
        self.total() - self.resident
    }
}

/// One phi's recorded refusal, pending the tally described on [`PhiCensus`].
///
/// Deliberately not `Default`: "no cause recorded" is `Option::None` and means
/// the scan never refused this phi, which is a different statement from any of
/// these and must not be spelled by one of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir_lower) enum PhiRefusal {
    Disabled,
    RefType,
    NoRegister,
    NoHome,
    WrongBank,
}

/// The machine model for `ir_lower`'s emission, over `regs`.
///
/// [`crate::regalloc::MachineModel::for_graph`] derives clobbers from ops. It
/// is correct as far as it goes, and it cannot know about the calls a
/// particular backend emits *between* ops. This adds the one `ir_lower` has:
/// the cooperative safepoint poll, which on a loop back edge tests a byte and
/// on a set flag calls its slow path (`emit_safepoint_poll`) — at the block's
/// outgoing-edge position, which belongs to no node. `for_graph` already marks
/// those positions safepoints; they must also be clobbers, because a value in
/// a caller-saved register across a back edge would be destroyed by that call.
///
/// The entry poll (before block 0) needs nothing: no value is live yet.
pub(in crate::ir_lower) fn ir_lower_machine_model(
    graph: &Graph,
    schedule: &Schedule,
    live: &crate::regalloc::LiveModel,
    regs: crate::regalloc::RegFile,
    selected: Option<&MirPlan>,
) -> crate::regalloc::MachineModel {
    use std::collections::BTreeMap;
    // The back-edge safepoint poll is a CALL site, so what it destroys is the
    // caller-saved subset — the same rule `MachineModel::for_graph` applies at
    // an `Op::Call`, applied here because the poll is not a graph node and
    // `for_graph` therefore never sees it.
    //
    // This used to be every register in the file. That was safe but wrong in
    // the expensive direction: it made a loop-carried value unpromotable into
    // XMM6/XMM7 even on the target where this frame saves them, which is
    // exactly the case the save area was built for. The narrowing is sound
    // because the poll's slow path is `jit_safepoint_slow_path`, an ordinary
    // `extern "C"` function — Win64 obliges it to preserve XMM6–XMM15, and on
    // System V no XMM is callee-saved so `IR_LOWER_SAVED_XMMS` is empty and
    // this set is still the whole file, byte for byte as before.
    //
    // Round 11 wave 6: minus the XMM bank when `ir_poll_preserves_fp`, because
    // the slow path then reloads every FP value resident across the edge from
    // its home word. On System V this was the whole IR XMM file, so no FP
    // value stayed in a register across a loop iteration.
    let preserve_fp = ir_poll_preserves_fp();
    let poll_destroys: Vec<crate::regalloc::PhysReg> = regs
        .specs()
        .iter()
        .filter(|s| s.caller_saved)
        .filter(|s| !(preserve_fp && s.reg.class == crate::regalloc::RegClass::Xmm))
        .map(|s| s.reg)
        .collect();
    let mut model = crate::regalloc::MachineModel::for_graph_with_selected_fixed(
        graph, schedule, live, regs, |id| selected.is_some_and(|plan| plan.division(id).is_some()),
    );
    // Reference residency. The allocator's own rule — no reference may hold a
    // register across a safepoint — is the right default for a backend whose
    // register file is authoritative. This one's is a write-through read cache
    // over a frame slot the oop map DOES name, so the collector's view is
    // complete without it and the obligation this lifts is discharged instead
    // by `invalidate_ref_residency`, which drops the cached copy at every point
    // a collector could have run. See `ir_ref_residency_enabled`.
    model.refs_may_cross_safepoints =
        ir_ref_residency_enabled() && ir_ref_residency_cross_safepoint_enabled();

    // `MachineModel::clobbered_at` binary-searches by position, so the list
    // must stay sorted AND hold one entry per position. Merge through a map
    // rather than push-and-sort.
    let mut merged: BTreeMap<usize, Vec<crate::regalloc::PhysReg>> = BTreeMap::new();
    // JIT round 21, lane tcmiss (TCPERF21-1, `CRATONVM_JIT_TYPECHECK_NONCALL`):
    // a type check's helper call is on its slow path only, which reloads the
    // caller-saved XMM residents from their homes after it
    // (`typecheck_fp_reload_sets`), so the check does not destroy the XMM
    // bank -- at a position no other node shares. Same condition as the poll's
    // (`preserve_fp`: whole-range residency).
    let typecheck_exempt: Vec<usize> =
        if preserve_fp && crate::typecheck_rows::typecheck_noncall_enabled() {
            typecheck_noncall_positions(graph, live)
                .into_iter()
                .map(|(pos, _)| pos)
                .collect()
        } else {
            Vec::new()
        };
    for (pos, mut regs) in model.clobbers.drain(..) {
        if typecheck_exempt.binary_search(&pos).is_ok() {
            regs.retain(|r| r.class != crate::regalloc::RegClass::Xmm);
            if regs.is_empty() {
                continue;
            }
        }
        merged.entry(pos).or_default().extend(regs);
    }
    for (b, block) in schedule.blocks.iter().enumerate() {
        if !block.successors.iter().any(|&s| s <= b) {
            continue;
        }
        let Some(&(_, edge)) = live.span.get(b) else {
            continue;
        };
        // `lower_terminator` and `lower_block`'s fall-through arm both emit the
        // poll immediately before the edge's phi copies, i.e. between the
        // terminator position and the edge position. Clobber both, for the same
        // reason `MachineModel::for_graph` makes both safepoints.
        for pos in [edge.saturating_sub(1), edge] {
            merged
                .entry(pos)
                .or_default()
                .extend_from_slice(&poll_destroys);
        }
    }
    model.clobbers = merged
        .into_iter()
        .map(|(pos, mut regs)| {
            regs.sort_unstable();
            regs.dedup();
            (pos, regs)
        })
        .collect();
    // Last use at a call (round 9 wave 4, irlower4 -- page
    // `linear-scan-last-use-at-a-call-still-splits`). AFTER every clobber above
    // is merged, as `mark_call_operand_reads_first` requires: the exemption
    // covers every register clobbered at a listed position, the back-edge poll
    // included, and that is sound here because the poll runs after the node at
    // that position was lowered (it is emitted before the edge's phi copies),
    // i.e. after the operand read. Only the audited ops are listed -- see
    // `ir_call_reads_operands_first`.
    {
        model.mark_operand_reads_first_where(graph, live, ir_call_reads_operands_first);
    }
    model
}



/// Does this file's lowering of `node` read EVERY register operand before the
/// first instruction that can destroy a caller-saved register? The audit
/// `MachineModel::reads_precede_clobber` asks the emitter for, per op.
///
/// Admitted (round 9 wave 4):
///
/// * `Op::Store(_)`, every route. The receiver goes to RAX and the null check
///   is a `TEST` plus a deopt JUMP (the deopt path reads its frame from home
///   words, which the write-through invariant keeps complete, and never
///   returns into the body). The helper routes then load their arguments and
///   `CALL` once, with nothing after the call; the gated reference store's
///   bails reload from HOME words (`slot_of`), not registers; the inline int
///   store makes no call at all. No frame record, no safepoint map. And the
///   stored value is read through `gp_load_value`, which reads an FP value
///   from its home word (no GP residency), so an XMM register is never read.
/// * An FP `Op::Rem`: `fp_load_value(XMM0, a)`, `fp_load_value(XMM1, b)`, then
///   `MOV RAX, helper ; CALL RAX`, and the result from XMM0. Both operands are
///   read before the call.
///
/// NOT admitted, with the reason, so the list is extended by audit and not by
/// guess:
///
/// * `Op::Call`, `Op::LambdaIntToDouble`, `Op::MonitorEnter/Exit`,
///   `Op::ArrayStore(Ref)`: `emit_safepoint_map` runs FIRST, and its shadow
///   push CALLS `get_current_thread`, destroying every volatile register
///   before the arguments are marshalled. (A monitor op's inline thin-lock
///   fast path runs before even that, and writes RAX/RCX/RDX/R10/R11.)
/// * `Op::New`, `Op::NewArray`, `Op::ConstString`, `Op::ConstClass`,
///   `Op::LoadStatic`, `Op::Load`, `Op::InstanceOf`, `Op::CheckCast`: not
///   audited. Their operands are references or ints, which live in the
///   callee-saved GP file here and are never clobbered at a call, so there is
///   nothing to gain from admitting them.
pub(in crate::ir_lower) fn ir_call_reads_operands_first(node: &Node) -> bool {
    match node.op {
        Op::Store(_) => true,
        Op::Rem => matches!(node.ty, IrType::Float | IrType::Double),
        _ => false,
    }
}

/// [`clobbered_within`] for ONE value, honouring the allocator's last-use
/// exemption: a clobber at `hi` does not count when
/// `MachineModel::read_before_clobber` holds for `id` there (the position is
/// listed in `reads_precede_clobber`, and it is both the end of `id`'s live
/// range and its last use). The same predicate the scan and
/// `verify_allocation` apply, so the second opinion cannot demote exactly the
/// values the opt-in exists to keep -- and, with the list empty (the default),
/// this is `clobbered_within` exactly.
pub(in crate::ir_lower) fn clobbered_within_for_value(
    model: &crate::regalloc::MachineModel,
    live: &crate::regalloc::LiveModel,
    id: usize,
    lo: usize,
    hi: usize,
    reg: crate::regalloc::PhysReg,
) -> bool {
    let start = model.clobbers.partition_point(|(pos, _)| *pos < lo);
    model.clobbers[start..]
        .iter()
        .take_while(|(pos, _)| *pos <= hi)
        .any(|(pos, regs)| {
            regs.contains(&reg)
                && !(*pos == hi && model.read_before_clobber(live, id as NodeId, *pos))
        })
}

/// Plan register residency for one scheduled graph, or `None` when nothing is
/// promotable and the colourer's memory layout stands unchanged.
///
/// Every refusal here is a *demotion*, never a bailout: a value that does not
/// take a register keeps the home slot the lowerer has always given it, which
/// is correct by construction. The one hard failure is
/// [`crate::regalloc::verify_allocation`] rejecting the allocation, which is a
/// compiler bug — that returns `Err` and the caller refuses the compile rather
/// than emitting against an allocation nothing proved.
/// Report why residency was declined, under `CRATONVM_DBG_IR_LINEAR_SCAN`.
///
/// Every refusal in `plan_register_residency` used to be a bare `Ok(None)`,
/// and the flag printed only on SUCCESS -- so "no output" meant "declined,
/// somewhere, for one of four reasons" and could not be acted on. Naming the
/// conjunct is the difference between a count and a diagnosis.
pub(in crate::ir_lower) fn ls_refuse(reason: &str) -> Option<RegResidency> {
    if false {
        eprintln!("[ir-ls] refused: {reason}");
    }
    None
}

/// Is `reg` in any clobber set at a position in `lo..=hi`?
///
/// `clobbers` is a `MachineModel::clobbers` list, which is sorted by position
/// with one entry per position (`ir_lower_machine_model` rebuilds it through a
/// `BTreeMap`, and `verify_allocation` rejects a model that is not), so the
/// window is found by binary search instead of scanning the whole list once
/// per promoted value — `plan_register_residency`'s re-checks were `O(values * clobbers)`, and a
/// call-heavy method has a clobber entry per call.
pub(in crate::ir_lower) fn clobbered_within(
    clobbers: &[(usize, Vec<crate::regalloc::PhysReg>)],
    lo: usize,
    hi: usize,
    reg: crate::regalloc::PhysReg,
) -> bool {
    let start = clobbers.partition_point(|(pos, _)| *pos < lo);
    clobbers[start..]
        .iter()
        .take_while(|(pos, _)| *pos <= hi)
        .any(|(_, regs)| regs.contains(&reg))
}

/// Keep the scan's certain refusals out of the scan -- **default ON**
/// (round 13 wave 11, lane callcost5, proposal CC4-3 in
/// `jit-r13-callcost4-proposals-RETIRED-20260929.md`); `CRATONVM_JIT_IR_LS_PREPIN_REFUSED=0` hands
/// every candidate to the scan as before.
///
/// Every register the kept values hold is saved in the prologue and restored
/// at every exit (`ls_saved_gp`). The scan's free-register choice is already
/// "lowest free in file order" -- first fit, which on intervals taken in start
/// order uses as few registers as the candidates' overlap allows -- so CC4-3's
/// "reuse a register already handed out before opening a new one" was true of
/// the scan all along. What made `fib` save more registers than it keeps
/// values across a call is WHO the scan hands them to: constants (read as
/// immediates), values read once, and (by default) references, all of which
/// the refusal loop in [`plan_register_residency`] then throws away. A refused
/// value that held RBX while a kept one was defined pushed the kept one to R12;
/// the refusal left RBX idle and R12 saved. Pinning them first is exactly the
/// refusal, made before the scan instead of after it: a pinned value keeps its
/// home word, which is what the refusal gives it.
///
/// Read per compile (no `OnceLock`): nothing in the frame layout depends on it
/// beyond the residency plan it shapes.
pub(in crate::ir_lower) fn ir_ls_prepin_refused_enabled() -> bool {
    true
}

/// A single-use `int`/`long` whose home is one `store_rax` and whose range
/// crosses a Java call may take a register -- **default ON** (round 14 wave 1,
/// lane calls, proposal CC5-1 in `jit-r13-callcost5-proposals-RETIRED-20260929.md`);
/// `CRATONVM_JIT_IR_RESIDENCY_CALL_CROSSING=0` restores the plain pays rule.
///
/// In `return fib(n-1) + fib(n-2)` the first result is read once, by the add,
/// after the second call. The pays rule refuses it (`skip_single_use`), so it
/// is stored to its home after its call and reloaded at the add. A register
/// makes that home droppable (`value_home_droppable`: the direct self-call's
/// result is one `store_rax`), which turns the pair into one register move.
/// This is the narrow case of `CRATONVM_JIT_IR_RESIDENCY_CROSSBLOCK` (still
/// off: it regresses `mixNarrow`): only a value a real `Op::Call` separates
/// from its use, and it is charged against the same carried-reservation
/// price, so a loop kernel's carried values are not displaced by it.
///
/// Residency is a pure placement decision here (every register of the GP
/// file is callee-saved and survives the call; the publish and home-drop
/// rules are the ordinary ones), so the switch changes cost, never answers.
/// Read per compile.
pub(in crate::ir_lower) fn ir_residency_call_crossing_enabled() -> bool {
    true
}

/// The sorted positions of every `Op::Call` the schedule places, for
/// [`single_use_crosses_a_call`]; empty (so nothing is a candidate) when
/// [`ir_residency_call_crossing_enabled`] is off. The switch is read here, once
/// per plan, not once per candidate.
pub(in crate::ir_lower) fn ls_call_positions(graph: &Graph, live: &crate::regalloc::LiveModel) -> Vec<usize> {
    if !ir_residency_call_crossing_enabled() {
        return Vec::new();
    }
    let mut out: Vec<usize> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| matches!(node.op, Op::Call { .. }))
        .filter_map(|(id, _)| live.pos_of.get(id).copied().flatten())
        .collect();
    out.sort_unstable();
    out
}

/// Is `id` a CC5-1 candidate ([`ir_residency_call_crossing_enabled`])? A value
/// read once (`uses == 1`), of type `int`/`long`, whose home is written by one
/// `store_rax` (so a register lets it drop), with an `Op::Call` strictly inside
/// its live range (its one read is after a call it is live across).
/// `call_pos` is [`ls_call_positions`]. Shared by the pre-pin
/// ([`ls_certain_refusals`]) and the refusal loop so the two cannot disagree.
pub(in crate::ir_lower) fn single_use_crosses_a_call(
    graph: &Graph,
    live: &crate::regalloc::LiveModel,
    call_pos: &[usize],
    id: usize,
    uses: u32,
) -> bool {
    if uses != 1 || call_pos.is_empty() {
        return false;
    }
    let Some(node) = graph.nodes.get(id) else {
        return false;
    };
    if !matches!(node.ty, IrType::Int | IrType::Long) {
        return false;
    }
    let one_store = op_home_is_one_store_rax(&node.op)
        || (matches!(node.op, Op::Call { .. })
            && Lowerer::is_direct_self_call(node)
            && ir_self_call_states_unread_enabled());
    if !one_store {
        return false;
    }
    let Some(range) = live.range.get(id).copied().flatten() else {
        return false;
    };
    let first_after_def = call_pos.partition_point(|&p| p <= range.lo);
    call_pos.get(first_after_def).is_some_and(|&p| p < range.hi)
}

/// `CRATONVM_JIT_IR_RESIDENCY_FRAME_PINNED` (JIT round 23 wave 10, lane L7;
/// perf, **default ON since j27 2026-10-28**, `=0` kills; was opt-in, neutral
/// on n23-w10a SegMono, W2c `nocast` slightly worse, battery SAME): a register for a single-use `int` /
/// `long` defined inside a loop whose one read the carry window cannot reach
/// and which a frame state names ([`single_use_frame_pinned_in_loop`]).
///
/// `31 * h` in `R23W4SegMono.heapGet`: it sits on the JVM operand stack across
/// the `getAtIndex` invoke, so the segment access's deopt frame names it, and
/// its one reader (the `add`) comes after that access. The single-use rule
/// (`ir_residency_pays_here`: static `uses >= 2`) refused it a register, so it
/// was written to its home before the access and RELOADED for the add -- a
/// store-forwarding round trip on the loop-carried `h` chain every iteration
/// (n23-w9c `heapGet` 3fd / 58d). With a register the add reads it there; the
/// frame names the register when the value owns it exclusively
/// (`deopt_nameable`, the existing register image: `FrameValue::Register`),
/// and its home word otherwise, which the write-through definition still
/// writes. Charged against the loop-carried reservation like the crossblock
/// admission (`carried_displacement_price`), so it never displaces a value
/// read more often. Read per compile; no `static`.
pub(in crate::ir_lower) fn ir_residency_frame_pinned_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// The values some `SafepointSnapshot` slot names (locals, stack, monitors).
pub(in crate::ir_lower) fn frame_named_mask(graph: &Graph) -> Vec<bool> {
    let mut named = vec![false; graph.nodes.len()];
    for sp in &graph.safepoints {
        for v in sp.locals.iter().chain(sp.stack.iter()).chain(sp.monitors.iter()).copied().chain(sp.sb_virtual_nodes()) {
            if let Some(cell) = named.get_mut(v as usize) {
                *cell = true;
            }
        }
    }
    named
}

/// The loop depth of the block holding `id`'s definition, by the live model's
/// position spans (the rule `ir_residency_pays_here` uses).
pub(in crate::ir_lower) fn def_loop_depth(
    live: &crate::regalloc::LiveModel,
    schedule: &Schedule,
    id: usize,
) -> Option<u32> {
    let def_pos = live.pos_of.get(id).copied().flatten()?;
    for b in 0..schedule.blocks.len() {
        if let Some(&(lo, hi)) = live.span.get(b) {
            if lo <= def_pos && def_pos <= hi {
                return live.loop_depth.get(b).copied();
            }
        }
    }
    None
}

/// JIT round 23 wave 10 (lane L7, [`ir_residency_frame_pinned_enabled`]): is
/// `id` a single-use `int` / `long`, defined inside a loop, named by a frame
/// state, whose one read is NOT the next node of its block (the carry window
/// would hand it over in RAX for free)?
pub(in crate::ir_lower) fn single_use_frame_pinned_in_loop(
    graph: &Graph,
    schedule: &Schedule,
    live: &crate::regalloc::LiveModel,
    frame_named: &[bool],
    id: usize,
    uses: u32,
) -> bool {
    if !ir_residency_frame_pinned_enabled() || uses != 1 {
        return false;
    }
    let Some(node) = graph.nodes.get(id) else {
        return false;
    };
    if !matches!(node.ty, IrType::Int | IrType::Long)
        || matches!(node.op, Op::Phi | Op::Const(_) | Op::Param(_))
    {
        return false;
    }
    frame_named.get(id).copied().unwrap_or(false)
        && !single_use_is_in_the_carry_window(graph, schedule, id)
        && def_loop_depth(live, schedule, id).is_some_and(|d| d > 0)
}

fn is_recurrence_alu(node: &crate::ir::Node) -> bool {
    matches!(node.ty, IrType::Int | IrType::Long)
        && matches!(
            node.op,
            Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Xor
                | Op::Or
                | Op::And
                | Op::Shl
                | Op::Shr
                | Op::UShr
                | Op::Neg
        )
}

fn single_reader(graph: &Graph, id: usize) -> Option<usize> {
    let mut reader: Option<usize> = None;
    for (uid, un) in graph.nodes.iter().enumerate() {
        if un.op != Op::Dead && un.inputs.iter().any(|&i| i != NO_NODE && i as usize == id) {
            if reader.is_some() {
                return None;
            }
            reader = Some(uid);
        }
    }
    reader
}

fn reaches_phi_backward(
    graph: &Graph,
    cur: usize,
    phi_id: usize,
    depth: usize,
    visited: &mut [usize; 8],
    n_vis: &mut usize,
) -> bool {
    let Some(node) = graph.nodes.get(cur) else {
        return false;
    };
    if node
        .inputs
        .iter()
        .any(|&i| i != NO_NODE && i as usize == phi_id)
    {
        return true;
    }
    if depth >= 8 || *n_vis >= visited.len() {
        return false;
    }
    visited[*n_vis] = cur;
    *n_vis += 1;
    for &inp in &node.inputs {
        if inp == NO_NODE {
            continue;
        }
        let iid = inp as usize;
        if visited[..*n_vis].contains(&iid) {
            continue;
        }
        if let Some(in_node) = graph.nodes.get(iid) {
            if is_recurrence_alu(in_node)
                && reaches_phi_backward(graph, iid, phi_id, depth + 1, visited, n_vis)
            {
                return true;
            }
        }
    }
    false
}

/// JIT round 23 wave 10b (lane L7): is `id` on a loop phi's recurrence
/// chain (within up to 8 single-use `Int`/`Long` ALU steps forward into an
/// edge input of a value `Op::Phi`, and reachable backward from that phi)?
pub(in crate::ir_lower) fn on_phi_recurrence(graph: &Graph, id: usize) -> bool {
    if graph.nodes.get(id).is_none() {
        return false;
    }
    let mut visited = [usize::MAX; 8];
    let mut cur = id;
    for step in 0..8 {
        if visited[..step].contains(&cur) {
            return false;
        }
        visited[step] = cur;
        let Some(reader) = single_reader(graph, cur) else {
            return false;
        };
        let Some(rnode) = graph.nodes.get(reader) else {
            return false;
        };
        // Check whether `cur` itself feeds `phi` directly (for the final step)
        // or `reader` feeds a value `Op::Phi`'s edge input (`inputs[1..]`).
        if rnode.op == Op::Phi
            && is_value_ty(rnode.ty)
            && rnode
                .inputs
                .iter()
                .skip(1)
                .any(|&e| e != NO_NODE && e as usize == cur)
        {
            let mut bvis = [usize::MAX; 8];
            let mut n_vis = 0;
            if reaches_phi_backward(graph, id, reader, 0, &mut bvis, &mut n_vis) {
                return true;
            }
        }
        let mut matched_phi = false;
        for (pid, pn) in graph.nodes.iter().enumerate() {
            if pn.op == Op::Phi
                && is_value_ty(pn.ty)
                && pn
                    .inputs
                    .iter()
                    .skip(1)
                    .any(|&e| e != NO_NODE && e as usize == reader)
            {
                let mut bvis = [usize::MAX; 8];
                let mut n_vis = 0;
                if reaches_phi_backward(graph, id, pid, 0, &mut bvis, &mut n_vis) {
                    matched_phi = true;
                    break;
                }
            }
        }
        if matched_phi {
            return true;
        }
        if !is_recurrence_alu(rnode) {
            return false;
        }
        cur = reader;
    }
    false
}

/// JIT round 23 wave 10b (lane L7): the weight the frame-pinned admission
/// pays the carried reservation's price with. Measured on n23-w10a
/// (`CRATONVM_DBG_IR_LINEAR_SCAN`, `R23W4SegMono.heapGet`): `31 * h` is ONE
/// loop-weighted read (weight 10), the narrow file has five registers, and
/// the fifth-heaviest carried value weighs 11 (`price=11 taken=0`), so the
/// admission never fired there. A value on a phi recurrence
/// ([`on_phi_recurrence`]) counts double: its reload costs the recurrence's
/// latency, while the displaced carried value's reload is one more L1 load
/// off that chain. Everything else pays its plain weight, as before.
pub(in crate::ir_lower) fn frame_pinned_admission_weight(
    graph: &Graph,
    live: &crate::regalloc::LiveModel,
    id: usize,
) -> u64 {
    let w = live.weight.get(id).copied().unwrap_or(0);
    if on_phi_recurrence(graph, id) {
        w.saturating_mul(2)
    } else {
        w
    }
}

/// `CRATONVM_JIT_IR_LOOP_CARRIED_FP_REGS` (the FP half of L1W3-1; perf,
/// **default ON**, `=0` kill): keep a loop's carried `float` / `double`
/// recurrence in XMM registers. Three parts, one switch:
///
/// * the residency plan admits every single-use FP step of a phi's recurrence
///   ([`single_use_fp_recurrence_in_loop`]), which the single-use rule refused:
///   `s += f[j]` unrolled four times is `s1 = s + a; s2 = s1 + b; ...`, and each
///   `sK` was written to its home and RELOADED by the next add -- a
///   store-forwarding round trip on the add chain per element;
/// * an FP phi's edge copy writes the phi's home from its source's register
///   (`Lowerer::emit_copy_op`), and the phi is published from that register
///   (`Lowerer::emit_value_phi_copies`), not reloaded from the home word. The
///   copy through RAX was a 64-bit load of a word a 32-bit `MOVSS` had just
///   written, which store forwarding cannot serve;
/// * an FP binop computes in its result's register (`Lowerer::fp_binop_value`)
///   instead of the XMM0/XMM1 scratch pair and a copy out.
///
/// Measured on the Linux host (EPYC Turin): `R21VectorRows` /
/// `L1W3FpCarried` `float-sum-1024` 4234 -> 586 ns (HotSpot 565), which must
/// stay scalar; `double-sum-1024` 3777 -> 585. The homes are still written
/// (the FP write-through invariant), off the chain, so deopt frames, safepoint
/// maps and the poll's reloads are unchanged; only the reads move to
/// registers. Read per compile; no `static`.
pub(crate) fn ir_loop_carried_fp_regs_enabled() -> bool {
    true
}

/// For [`single_use_fp_recurrence_in_loop`]: each node's reader when it has
/// exactly one (`None` for zero or several), over live nodes.
pub(in crate::ir_lower) fn single_readers(graph: &Graph) -> Vec<Option<usize>> {
    let n = graph.nodes.len();
    let mut reader: Vec<Option<usize>> = vec![None; n];
    let mut many = vec![false; n];
    for (uid, un) in graph.nodes.iter().enumerate() {
        if un.op == Op::Dead {
            continue;
        }
        for &i in &un.inputs {
            if i == NO_NODE || i as usize >= n {
                continue;
            }
            let slot = i as usize;
            match reader[slot] {
                None if !many[slot] => reader[slot] = Some(uid),
                Some(r) if r == uid => {}
                _ => {
                    reader[slot] = None;
                    many[slot] = true;
                }
            }
        }
    }
    reader
}

/// An FP arithmetic step: `float` / `double` `Add`, `Sub`, `Mul` or `Div`.
pub(in crate::ir_lower) fn is_fp_arith(node: &Node) -> bool {
    matches!(node.ty, IrType::Float | IrType::Double)
        && matches!(node.op, Op::Add | Op::Sub | Op::Mul | Op::Div)
}

/// [`ir_loop_carried_fp_regs_enabled`]'s admission: is `id` a single-use FP
/// arithmetic step defined inside a loop, on the recurrence of an FP phi `p`?
/// That is, its single readers lead through FP steps to `p`, and it depends on
/// `p` through FP steps. Bounded walks (64 steps / nodes): a longer chain is
/// left to the rules that already apply to it. `readers` is empty when the
/// switch is off.
pub(in crate::ir_lower) fn single_use_fp_recurrence_in_loop(
    graph: &Graph,
    schedule: &Schedule,
    live: &crate::regalloc::LiveModel,
    readers: &[Option<usize>],
    id: usize,
    uses: u32,
) -> bool {
    if readers.is_empty() || uses != 1 {
        return false;
    }
    let Some(node) = graph.nodes.get(id) else {
        return false;
    };
    if !is_fp_arith(node) || !def_loop_depth(live, schedule, id).is_some_and(|d| d > 0) {
        return false;
    }
    // Forward: the single readers, through FP steps, to an FP phi.
    let mut cur = id;
    let mut phi: Option<usize> = None;
    for _ in 0..64 {
        let Some(r) = readers.get(cur).copied().flatten() else {
            return false;
        };
        let Some(rn) = graph.nodes.get(r) else {
            return false;
        };
        if rn.op == Op::Phi && rn.ty == node.ty {
            phi = Some(r);
            break;
        }
        if !is_fp_arith(rn) {
            return false;
        }
        cur = r;
    }
    let Some(phi) = phi else {
        return false;
    };
    // Backward: `id` reads `phi` through FP steps.
    let mut stack: Vec<usize> = vec![id];
    let mut seen: Vec<usize> = Vec::new();
    while let Some(x) = stack.pop() {
        if seen.contains(&x) {
            continue;
        }
        if seen.len() >= 64 {
            return false;
        }
        seen.push(x);
        let Some(xn) = graph.nodes.get(x) else {
            continue;
        };
        for &i in &xn.inputs {
            if i == NO_NODE {
                continue;
            }
            if i as usize == phi {
                return true;
            }
            if graph.nodes.get(i as usize).is_some_and(is_fp_arith) {
                stack.push(i as usize);
            }
        }
    }
    false
}


/// [`ls_certain_refusals`] with the register allocator's EFFECTIVE switch
/// (`r25_w7a_reg_alloc::effective`) given, so the pre-pin, the plan and the
/// lowerer read one answer (JIT round 25 wave 9, lane W9-2).
pub(in crate::ir_lower) fn ls_certain_refusals_with(
    graph: &Graph,
    schedule: &Schedule,
    live: &crate::regalloc::LiveModel,
    reg_alloc: bool,
) -> Vec<NodeId> {
    let pays = ir_residency_pays_enabled();
    // JIT round 19 proposal wave 12, lane lv12 (LV11-1,
    // `CRATONVM_JIT_IR_LS_PREPIN_REFS_UNDER_SPLITS`): the split arm refuses a
    // `Ref` too unless reference residency is on (`ls_bank_for`), so under
    // `LS_SPLITS` alone a `Ref` is still a certain refusal.
    let refs_refused = !ir_ref_residency_enabled()
        && (!ir_ls_splits_enabled() || ir_ls_prepin_refs_under_splits_enabled());
    let crossblock = ir_residency_crossblock_enabled();
    // JIT r23 wave 10 (lane L7): the frame-pinned admission's population.
    let frame_named = frame_named_mask(graph);
    // `CRATONVM_JIT_IR_LOOP_CARRIED_FP_REGS`'s population (empty when off).
    let fp_readers = if ir_loop_carried_fp_regs_enabled() {
        single_readers(graph)
    } else {
        Vec::new()
    };
    let n = graph.nodes.len();
    // The refusal loop's own `use_count`, computed the same way.
    let mut use_count = vec![0u32; n];
    for node in &graph.nodes {
        for &input in &node.inputs {
            if input != NO_NODE {
                if let Some(c) = use_count.get_mut(input as usize) {
                    *c = c.saturating_add(1);
                }
            }
        }
    }
    let call_pos = ls_call_positions(graph, live);
    let mut out: Vec<NodeId> = Vec::new();
    for (id, node) in graph.nodes.iter().enumerate() {
        if !live.wants_loc.get(id).copied().unwrap_or(false) || matches!(node.op, Op::Phi) {
            continue;
        }
        let uses = use_count.get(id).copied().unwrap_or(0);
        let crossblock_candidate =
            crossblock && uses == 1 && !single_use_is_in_the_carry_window(graph, schedule, id);
        // CC5-1: the refusal loop may keep this one, so it is not certain.
        let call_crossing_candidate = single_use_crosses_a_call(graph, live, &call_pos, id, uses);
        let frame_pinned_candidate =
            single_use_frame_pinned_in_loop(graph, schedule, live, &frame_named, id, uses);
        // JIT round 25 wave 7 (lane W7-A): the refusal loop may keep this one.
        let reg_alloc_candidate =
            reg_alloc && r25_w7a_reg_alloc::single_use_candidate(graph, schedule, id, uses);
        // `CRATONVM_JIT_IR_LOOP_CARRIED_FP_REGS`: the refusal loop keeps this one.
        let fp_recurrence_candidate =
            single_use_fp_recurrence_in_loop(graph, schedule, live, &fp_readers, id, uses);
        let refused = (pays && matches!(node.op, Op::Const(_)))
            || (pays
                && !ir_residency_pays_here(live, schedule, id, uses)
                && !crossblock_candidate
                && !call_crossing_candidate
                && !frame_pinned_candidate
                && !reg_alloc_candidate
                && !r25_w7a_reg_alloc::direct_single_use_candidate(graph, schedule, id, uses)
                && !fp_recurrence_candidate)
            || (refs_refused && node.ty == IrType::Ref);
        if refused {
            // Cast: an index into the node arena is a `NodeId`.
            out.push(id as NodeId);
        }
    }
    out
}

/// `plan_register_residency`'s `ranges_agree`: the slot plan and the
/// allocator's liveness model give every value the same range (or both give
/// it none). Split out in interpreter round i2 wave 65 (lane L2) so the φ-edge
/// fix can be tested against the exact predicate the gate uses.
pub(in crate::ir_lower) fn slot_and_live_ranges_agree(plan: &SlotPlan, live: &crate::regalloc::LiveModel) -> bool {
    plan.range.len() == live.range.len()
        && plan
            .range
            .iter()
            .zip(live.range.iter())
            .all(|(colourer, scan)| match (colourer, scan) {
                (Some(c), Some(s)) => c.lo == s.lo && c.hi == s.hi,
                (None, None) => true,
                _ => false,
            })
}

/// The `[ir-ls] ranges-disagree:` census line (JIT round 19 proposal wave 10,
/// lane lv10; the measurement half of
/// `i65-L2-proposal-measure-the-split-path-once-the-gate-opens`): why
/// [`slot_and_live_ranges_agree`] is false. `count=` values whose two ranges
/// differ (`phi=` of them φs, `slot_only=` / `live_only=` ranged by one model
/// alone), `len_differs=` when the two range tables are not even the same
/// length, and the first four as `n<id>:<op>:slot=<lo>..<hi>/live=<lo>..<hi>`
/// (`-` for no range). Printed under `CRATONVM_DBG_IR_LINEAR_SCAN` only.
pub(in crate::ir_lower) fn ranges_disagreement_census(
    graph: &Graph,
    plan: &SlotPlan,
    live: &crate::regalloc::LiveModel,
) -> String {
    let fmt = |r: Option<LiveRange>| match r {
        Some(r) => format!("{}..{}", r.lo, r.hi),
        None => "-".to_string(),
    };
    let (mut count, mut phi, mut slot_only, mut live_only) = (0usize, 0usize, 0usize, 0usize);
    let mut first: Vec<String> = Vec::new();
    for (id, (c, s)) in plan.range.iter().zip(live.range.iter()).enumerate() {
        let same = match (c, s) {
            (Some(c), Some(s)) => c.lo == s.lo && c.hi == s.hi,
            (None, None) => true,
            _ => false,
        };
        if same {
            continue;
        }
        count += 1;
        let op = graph.nodes.get(id).map(|n| &n.op);
        if matches!(op, Some(Op::Phi)) {
            phi += 1;
        }
        match (c, s) {
            (Some(_), None) => slot_only += 1,
            (None, Some(_)) => live_only += 1,
            _ => {}
        }
        if first.len() < 4 {
            let name = op.map_or_else(
                || "?".to_string(),
                |op| {
                    let full = format!("{op:?}");
                    full.split([' ', '{', '('])
                        .next()
                        .unwrap_or("?")
                        .to_string()
                },
            );
            first.push(format!(
                "n{id}:{name}:slot={}/live={}",
                fmt(*c),
                fmt((*s).map(|r| LiveRange { lo: r.lo, hi: r.hi }))
            ));
        }
    }
    format!(
        "[ir-ls] ranges-disagree: count={count} phi={phi} slot_only={slot_only} \
         live_only={live_only} len_differs={} first=[{}]",
        plan.range.len() != live.range.len(),
        first.join(" ")
    )
}

/// The loop-carried reservation of [`plan_register_residency_with`]: which
/// carried-and-read values get which free GP register, in the pass's own
/// order (loop weight, then id), given the registers already `taken`.
/// Factored out unchanged in JIT round 19 proposal wave 11 (lane lv11) so the
/// `CRATONVM_JIT_IR_LS_SPLITS_CARRIED_FIRST` dry run asks the very question
/// the pass asks.
pub(in crate::ir_lower) fn carried_reservation(
    graph: &Graph,
    plan: &SlotPlan,
    live: &crate::regalloc::LiveModel,
    clobbers: &[(usize, Vec<crate::regalloc::PhysReg>)],
    gp_reg_of: &[Option<u8>],
    taken: &[u8],
) -> Vec<(usize, u8)> {
    let n = graph.nodes.len();
    let mut out: Vec<(usize, u8)> = Vec::new();
    let mut free: Vec<u8> = ir_gp_file()
        .iter()
        .copied()
        .filter(|r| !taken.contains(r))
        .collect();
    // `carried_and_read`, not `carried` (round 11 wave 6): a value live
    // THROUGH the loop and read only after it covers the back edge too,
    // but it is not read on any iteration, so a register reserved for it
    // idles for the whole loop. The crossblock budget above counts with
    // this same predicate.
    let loop_aware = crate::regalloc::loop_phi_residency_enabled();
    let mut cands: Vec<(u64, usize)> = (0..n)
        .filter(|&id| live.carried_and_read.get(id).copied().unwrap_or(false))
        .filter(|&id| gp_reg_of.get(id).copied().flatten().is_none())
        // Write-through needs a home to write, exactly as the main loop
        // above requires.
        .filter(|&id| plan.node_color.get(id).copied().flatten().is_some())
        .filter(|&id| {
            match graph.nodes.get(id) {
                // `Ref` is excluded for the safepoint reason the bank match
                // gives, not as tuning: `OopMapEntry` names frame slots
                // only. FP belongs to the other file.
                Some(node) => {
                    matches!(node.ty, IrType::Int | IrType::Long)
                    // A constant is materialised as an immediate by every
                    // reader, so its register could never be read.
                    && !matches!(node.op, Op::Const(_))
                    // A phi is only publishable when its edges publish it.
                    && (ir_phi_residency_enabled() || !matches!(node.op, Op::Phi))
                }
                None => false,
            }
        })
        // j27 wave 2 (lane RESIDENCY, `CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY`):
        // a value redefined every trip also pays its definition out of a
        // register, so it outranks an invariant read as often. Off: the read
        // weight, as before.
        .map(|id| (crate::regalloc::ranking_weight(live, id, loop_aware), id))
        .collect();
    cands.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, id) in cands {
        let Some(range) = plan.range.get(id).copied().flatten() else {
            continue;
        };
        // The same clobber question the demotion pass asks. Inert for this
        // file today — every register in it is callee-saved and no fixed
        // x86 operand names one — but asking it here keeps the two places
        // that hand out a register from disagreeing about the rule.
        let Some(pos) = free.iter().position(|r| {
            !clobbered_within(
                clobbers,
                range.lo,
                range.hi,
                crate::regalloc::PhysReg::gp(*r),
            )
        }) else {
            continue;
        };
        let reg = free.remove(pos);
        out.push((id, reg));
        if free.is_empty() {
            break;
        }
    }
    out
}

