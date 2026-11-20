use super::*;

/// Interpreter round i1 wave 13, lane L4: a conditional back edge records
/// its exit map only for a frame the in-place transfer takes, so the new
/// point can never make `osr_exit_policy` veto the artifact.
#[test]
fn a_branch_exit_frame_must_be_one_the_transfer_takes() {
    use crate::deopt::{FrameState, FrameValue, MonitorInfo};
    let frame = |locals: Vec<FrameValue>, stack: Vec<FrameValue>| FrameState {
        class_id: 0,
        method_key: "T.m:()I".to_string(),
        bci: 9,
        locals,
        stack,
        monitors: Vec::new(),
        caller: None,
    };
    let ok = frame(
        vec![FrameValue::StackSlot(8)],
        vec![FrameValue::StackSlot(16), FrameValue::StackSlot(24)],
    );
    assert!(branch_exit_frame_is_transferable(&ok));
    assert!(!branch_exit_frame_is_transferable(&frame(
        vec![FrameValue::StackSlot(8)],
        vec![FrameValue::Unsupported, FrameValue::StackSlot(24)],
    )));
    assert!(!branch_exit_frame_is_transferable(&frame(
        vec![FrameValue::VirtualObjectRef(3)],
        vec![FrameValue::StackSlot(16)],
    )));
    let mut locked = ok.clone();
    locked.monitors.push(MonitorInfo {
        object: FrameValue::StackSlotRef(8),
        lock_depth: 1,
        relock: true,
    });
    assert!(!branch_exit_frame_is_transferable(&locked));
    let mut inlined = ok.clone();
    inlined.caller = Some(Box::new(ok.clone()));
    assert!(!branch_exit_frame_is_transferable(&inlined));
}

/// The truth table of the relocation gate.
///
/// The `map_incomplete=true, shadow_complete=true` row is the defect this
/// exists for: before the coupling, that row published complete coverage.
/// `record_oop_map` had ALREADY discovered the map was short — an oop left
/// in a register, or an offset that would not fit `i16` — and routed that
/// knowledge only to `mapped_safepoint_pcs`, which decides whether the
/// CONSERVATIVE backstop stays on. A conservative sweep marks what the map
/// missed, so that was sufficient while nothing moved. Relocation must
/// REWRITE the slot, which the backstop cannot do, so the short map went
/// to the collector labelled complete and `remap_one_jit_frame` left every
/// unnamed live reference pointing into from-space.
#[test]
fn a_short_map_cannot_claim_relocation_coverage() {
    assert!(
        !relocation_coverage_complete(true, true),
        "a map this function knows is short must not gate relocation, \
         however good the shadow publication was"
    );
    assert!(relocation_coverage_complete(true, false));
    assert!(!relocation_coverage_complete(false, false));
    assert!(!relocation_coverage_complete(false, true));
}

/// The gate is one-directional: it can only ever REMOVE a claim.
///
/// Stated as a property because the failure mode that matters is someone
/// later "simplifying" it into something that can turn a false into a
/// true — which would hand the collector a frame nothing proved.
#[test]
fn the_gate_only_ever_subtracts() {
    for shadow in [false, true] {
        for incomplete in [false, true] {
            assert!(
                !relocation_coverage_complete(shadow, incomplete) || shadow,
                "gate turned shadow_complete=false into a claim"
            );
        }
    }
}

/// `map_incomplete` must actually REACH the gate.
///
/// The bug was not a wrong expression, it was a value that existed, was
/// maintained in seven places, and was never wired to the thing it should
/// have gated. A truth table over the predicate cannot see that; this can.
#[test]
fn the_pushed_map_gates_on_map_incomplete() {
    let src = include_str!("../safepoint.rs");
    let at = src
        .find("moving_young_coverage_complete: relocation_coverage_complete(")
        .expect("the pushed OopMapEntry must gate through relocation_coverage_complete");
    let tail = &src[at..at + 200];
    assert!(
        tail.contains("map_incomplete"),
        "the gate at the OopMapEntry push must be fed `map_incomplete`; \
         found instead: {tail:?}"
    );
}

/// Every register a `ShadowHome::Reg` can actually name must encode as
/// ITSELF. The `CRATONVM_DBG_SHADOW_RELOAD` probe hard-coded `0x49`
/// (REX.W|REX.B) with the comment "r is r8..r15 here"; `LOCAL_REGS`
/// contains RBX on every platform (and RSI/RDI on Windows), so for those
/// homes the emitted `cmp` targeted `r + 8` instead. With `r == RBX` that
/// is R11 — the shadow-buffer read address, always a large pointer — so
/// `JAE` was always taken and the probe could never report the
/// non-pointer reload it exists to catch.
#[test]
fn cmp_r64_imm32_sets_rex_b_only_for_the_extended_half() {
    for &r in SCRATCH_REGS.iter().chain(LOCAL_REGS.iter()) {
        let [rex, opcode, modrm] = cmp_r64_imm32_opcode(r);
        assert_eq!(opcode, 0x81, "reg {r}: opcode");
        assert_eq!(modrm >> 6, 0b11, "reg {r}: mod must be register-direct");
        assert_eq!((modrm >> 3) & 7, 7, "reg {r}: /7 selects CMP");
        assert_eq!(modrm & 7, r & 7, "reg {r}: rm must be the low 3 bits");
        assert_eq!(rex & 0x48, 0x48, "reg {r}: REX.W must be set");
        // The bug: REX.B is the fourth bit of `rm`, so it must be set iff
        // the register really is r8..r15.
        assert_eq!(
            rex & 0x01,
            u8::from(r >= 8),
            "reg {r}: REX.B must track the register's high bit — setting \
             it unconditionally re-targets the compare at r{}",
            r + 8
        );
    }
}

/// A staged invoke argument is published on the SHADOW stack, not only
/// named in the oop map.
///
/// The two are different channels with different consumers. The map makes a
/// slot a precise root for MARKING; the shadow stack is the REWRITABLE
/// channel, and it is the only one
/// `conservative_roots::moving_young_unpublished_frame_oop_present`
/// consults — it asks `published_shadow_values(shadow_window_from_frame(..))`
/// and never looks at the map. So a staged argument that reached the map but
/// not the shadow stack sat in the frame's spill band as a movable-resident
/// word the shadow stack never published, and the band verifier correctly
/// refused the entire collection with `UNPUBLISHED_FRAME_OOP`.
///
/// `a51077342` added the map half (`emit_oop_map_for_safepoint`'s Stage 3)
/// and stopped there. Measured consequence on
/// `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821`:
/// `CRATONVM_JIT_INDY_BRIDGE=0` took `compiled-frame-oop-not-published`
/// from 28 to 5 per 76 collections where four other switches left it above
/// 21 — a bridged `invokedynamic` stages a capturing lambda's arguments
/// across a call that runs a bootstrap and allocates.
///
/// The assertion is `shadow ⊇ map Stage 3`, which is the invariant that was
/// violated; it deliberately does not require equality, because the shadow
/// list also carries register homes the map has no slot for.
#[test]
fn a_staged_invoke_argument_is_published_on_the_shadow_stack_too() {
    // The staged buffer only has to be published under MOVING coverage —
    // that is the mode whose band verifier demands it, and the mode
    // `collect_live_oop_homes` gates its frame-slot homes on.
    crate::x64::set_moving_young_override(Some(true));

    const STAGED: [i32; 3] = [64, 72, 80];
    let mut c = staged_arg_test_compiler();
    c.pending_staged_arg_oops = STAGED.to_vec();

    let homes = c.collect_live_oop_homes();
    for off in STAGED {
        assert!(
            homes.contains(&ShadowHome::Frame(off)),
            "staged arg at [rbp-{off}] is named by the oop map's Stage 3 but \
             was not published on the shadow stack; homes={homes:?}"
        );
    }

    // The list the map will consume must still be intact: this collection
    // READS it, and taking it here would silently empty Stage 3.
    assert_eq!(
        c.pending_staged_arg_oops, STAGED,
        "collect_live_oop_homes must not consume the staged-arg list"
    );

    crate::x64::set_moving_young_override(None);
}

/// A frame-slot home is published by ADDRESS and never copied back.
///
/// The push stores `rbp - off + 1` (the slot's address with
/// `ShadowStack::INDIRECT_TAG`), and the reload restores nothing into the
/// slot, because the collector rewrote the slot itself. With the switch off
/// the value form comes back: load the slot, push the value, and copy the
/// shadow word back into the slot after the call.
#[test]
fn a_frame_home_is_published_by_address_and_never_copied_back() {
    crate::x64::set_moving_young_override(Some(true));
    const OFF: i32 = 64;
    let tagged_lea: Vec<u8> = [0x48, 0x8D, 0x85]
        .into_iter()
        .chain((1 - OFF).to_le_bytes())
        .collect();
    // MOV R8, [R11 + 0] — the reload's read of the first shadow word.
    let copy_back_read: [u8; 7] = [0x4D, 0x8B, 0x83, 0, 0, 0, 0];
    for indirect in [true, false] {
        SHADOW_INDIRECT_FORCE.with(|c| c.set(Some(indirect)));
        let mut c = staged_arg_test_compiler();
        c.shadow_enabled = true;
        c.helpers.get_current_thread = 0x7fff_0000_1000;
        c.shadow_thread_slot_off = 40;
        c.shadow_savebase_slot_off = 48;
        c.shadow_off_in_thread = 0x2B8;
        c.pending_staged_arg_oops = vec![OFF];
        let start = c.buf.pos();
        c.emit_shadow_push();
        assert_eq!(c.pending_shadow, vec![ShadowHome::Frame(OFF)]);
        c.emit_shadow_reload();
        let code = c.buf.as_slice()[start..].to_vec();
        let count = |needle: &[u8]| code.windows(needle.len()).filter(|w| *w == needle).count();
        assert_eq!(
            count(&tagged_lea),
            usize::from(indirect),
            "indirect={indirect}: the push publishes the slot's tagged address exactly when indirect"
        );
        assert_eq!(
            count(&copy_back_read),
            usize::from(!indirect),
            "indirect={indirect}: the reload copies the slot back exactly when it pushed a value"
        );
    }
    SHADOW_INDIRECT_FORCE.with(|c| c.set(None));
    crate::x64::set_moving_young_override(None);
}

/// The control. Non-moving coverage keeps the conservative frame scan, which
/// already finds a staged slot on the stack, so publishing it would only
/// over-pin — the hazard `collect_live_oop_homes` documents as the bt18
/// small-heap OOM. Without this, the test above would pass just as well if
/// the homes were published unconditionally.
#[test]
fn a_staged_invoke_argument_is_not_published_when_nothing_moves() {
    crate::x64::set_moving_young_override(Some(false));
    let mut c = staged_arg_test_compiler();
    c.pending_staged_arg_oops = vec![64];
    let homes = c.collect_live_oop_homes();
    assert!(
        !homes.contains(&ShadowHome::Frame(64)),
        "the non-moving path must not publish staged args; homes={homes:?}"
    );
    crate::x64::set_moving_young_override(None);
}

/// The SELF-RECURSIVE call arm must name its reference arguments rather
/// than refuse the safepoint.
///
/// `CRATONVM_JIT_DIRECT_CALL_ARG_MAPS` gave the two direct-call arms a
/// nameable service range and stopped them raising
/// `pending_staged_args_unmapped`; the 0xb8 self-recursive arm kept
/// raising it for any reference argument, which makes `map_incomplete`
/// true at the next safepoint and so takes `fully_shadow_covered` false
/// for the whole method. Measured on `probes/OopMapSelfCall.java`: its two
/// self-recursive methods were the ONLY two uncovered methods in the run.
///
/// This is the source-level half of that repair — that the arm reaches the
/// staging channel at all, and that it fails closed on a home a frame-slot
/// map genuinely cannot describe.
#[test]
fn the_self_recursive_arm_stages_its_reference_arguments() {
    let src = crate::x64::bytecode_walk::WALK_SOURCES;
    let at = src
        .find("let staged_self_args_mark = self.pending_staged_arg_oops.len();")
        .expect("the self-recursive arm must mark the staged-arg buffer");
    let body = &src[at..at + 1400];
    assert!(
        body.contains("self_call_arg_maps_enabled()"),
        "the repair must be behind its kill switch"
    );
    assert!(
        body.contains("StackSlot::Frame(off) => {"),
        "a frame-resident argument oop must be pushed to the staging buffer"
    );
    assert!(
        body.contains("_ => unnameable = true,"),
        "an argument with a non-frame home must still fail the safepoint closed"
    );
    // The legacy arm has to survive verbatim, or the kill switch does not
    // bisect anything.
    assert!(
        body.contains("} else if arg_oops.iter().any(|&o| o) {"),
        "`=0` must restore the unconditional refusal"
    );
}

/// The TAIL form of that arm must un-stage what it staged.
///
/// It emits no safepoint before the `JMP`, and `reset_spills` hands the
/// argument slots straight back to the spill allocator — so a staged
/// offset left pending there is taken by a later, unrelated safepoint and
/// names a slot something else now owns. That is the mirror of the defect
/// the staging fixes, and it is the reason the mark exists rather than a
/// bare `push`.
#[test]
fn the_tail_self_call_form_unstages_before_it_jumps() {
    let src = crate::x64::bytecode_walk::WALK_SOURCES;
    let at = src
        .find("let staged_self_args_mark = self.pending_staged_arg_oops.len();")
        .expect("the self-recursive arm must mark the staged-arg buffer");
    let rest = &src[at..];
    let trunc = rest
        .find(".truncate(staged_self_args_mark);")
        .expect("the tail form must truncate the staged-arg buffer back to the mark");
    // The truncate belongs to the tail form: after that path's argument
    // moves, and -- since round 9 wave 5 added a back-edge poll to the
    // tail form -- BEFORE that poll and its JMP, so the poll's map cannot
    // consume the staged homes the moves just vacated.
    let window = &rest[..trunc];
    assert!(
        window.contains("self.emit_self_tail_param_moves(&arg_slots);"),
        "the truncate must sit on the TAIL path, after its argument moves"
    );
    let after = &rest[trunc..];
    let poll = after
        .find("self.emit_self_tail_safepoint_poll();")
        .expect("the tail form polls on its back edge");
    let jmp = after
        .find("JMP rel32 back to body entry")
        .expect("the tail form JMPs back to the body entry");
    assert!(
        poll < jmp,
        "the tail form must un-stage, then poll, then JMP"
    );
}

/// An incomplete map must be counted where a pc SET cannot mask it.
///
/// `mapped_safepoint_pcs` is keyed by bytecode pc, and a pc is not a
/// safepoint: a splice emits one safepoint per `invoke*` in the callee
/// under one enclosing bci, and the self-recursive arm emits its
/// stack-guard safepoint and its recursive CALL under one bci too. A
/// COMPLETE map at that bci inserts the pc, and the subset test then reads
/// true with an incomplete map sitting beside it -- the same mistake
/// `remap_one_jit_frame` made using `find` on `bytecode_pc` where every
/// other reader used `filter`.
///
/// The count is what `fully_oop_covered` tests, so it has to be maintained
/// beside the set and not derived from it.
#[test]
fn an_incomplete_map_is_counted_not_just_withheld_from_the_pc_set() {
    let src = include_str!("../safepoint.rs");
    let at = src
        .find("let push_map = !slots.is_empty() || self.precise_maps;")
        .expect("the map push must be findable");
    let body = &src[at..at + 700];
    let count_at = body
        .find("self.incomplete_oop_maps += 1;")
        .expect("an incomplete map must bump the unmaskable count");
    let set_at = body
        .find("self.mapped_safepoint_pcs.insert(")
        .expect("the pc set must still be maintained");
    assert!(
        count_at < set_at,
        "the count is guarded by `if map_incomplete`, the set by its negation; \
         they must be separate statements, not one branch"
    );

    // ...and the predicate must actually spend it.
    let driver = include_str!("../driver.rs");
    let at = driver
        .find("cm.fully_oop_covered = compiler.precise_maps")
        .expect("the coverage predicate must be findable");
    let pred = &driver[at..at + 400];
    assert!(
        pred.contains("compiler.incomplete_oop_maps == 0"),
        "the coverage predicate must test the unmaskable count; found: {pred:?}"
    );
    assert!(
        pred.contains("inline_sites.is_empty()"),
        "`CRATONVM_JIT_INLINE_OOP_COVERAGE=0` must restore the previous term \
         verbatim, or the pair cannot be bisected"
    );
}

/// A bare `Compiler` with no locals, no operand stack and no register
/// assignments, so `collect_live_oop_homes` returns exactly what the staged
/// buffer contributes and nothing else can be mistaken for it.
fn staged_arg_test_compiler() -> Compiler {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    Compiler::new(
        "staged-arg-shadow-test".to_string(),
        ExecutableBuffer::new(4096).expect("test executable buffer"),
        0,
        0,
        8,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        alloc_result,
        false,
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` with all-integer
        // fields, so an all-zero bit pattern is a valid value. Nothing here
        // dereferences a helper pointer.
        unsafe { std::mem::zeroed() },
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    )
}


/// gen r5w6/oomjit10: a Java invoke's claim that RAX is dead at its CALL
/// clears exactly RAX's bit and nothing else; without the claim RAX stays
/// in the mask, as it always did; and the claim never ADDS a register.
#[test]
fn a_call_that_reads_no_rax_leaves_its_bit_clear() {
    let c = staged_arg_test_compiler();
    let rax = crate::x64::licm::spill_gpr_bit(RAX);
    let arg = crate::x64::licm::spill_gpr_bit(ARG_REGS[1]);
    let Some(kept) = c.live_oop_register_mask(false, arg, false) else {
        // Masks switched off in this environment: nothing to compare.
        return;
    };
    assert_eq!(kept, rax | arg, "the unclaimed mask keeps RAX");
    let dead = c
        .live_oop_register_mask(false, arg, true)
        .expect("the claim does not withdraw the mask");
    assert_eq!(
        dead, arg,
        "the claim clears RAX and keeps the call's argument"
    );
    assert_eq!(dead & !kept, 0, "the claim never widens the mask");
}



/// Spot-check two concrete encodings against the ISA so the property test
/// above cannot pass a self-consistent but wrong rule.
#[test]
fn cmp_r64_imm32_matches_known_encodings() {
    // 48 81 FB — cmp rbx, imm32
    assert_eq!(cmp_r64_imm32_opcode(RBX), [0x48, 0x81, 0xFB]);
    // 49 81 FC — cmp r12, imm32
    assert_eq!(cmp_r64_imm32_opcode(R12), [0x49, 0x81, 0xFC]);
}

/// The poll's slow-path CALL carries the flag address it tested in
/// argument register 0, loaded AFTER the pre-safepoint spill and before
/// the CALL, so the helper can name the VM whose flag fired.
#[test]
fn the_poll_passes_its_flag_address_in_arg0() {
    if !jit_safepoint_polls_enabled() {
        return;
    }
    // Both far from any code buffer: the flag test takes the `MOV R11`
    // form and the CALL the absolute `imm64` form, so each value appears
    // in the stream as a searchable immediate.
    const FLAG: usize = 0x0000_7abc_def0_1230;
    const SLOW: usize = 0x0000_7bcd_ef01_2340;
    let mut c = staged_arg_test_compiler();
    c.precise_maps = false;
    c.shadow_enabled = false;
    c.helpers.safepoint_flag_addr = FLAG;
    c.helpers.safepoint_slow_path = SLOW;
    let start = c.buf.pos();
    c.emit_safepoint_poll();
    let code = c.buf.as_slice()[start..].to_vec();

    let mut probe = staged_arg_test_compiler();
    let p0 = probe.buf.pos();
    // Cast: x86-64 immediate encoding
    probe.emit_mov_imm64(ARG_REGS[0], FLAG as i64);
    let arg0_load = probe.buf.as_slice()[p0..].to_vec();

    let find = |needle: &[u8]| code.windows(needle.len()).position(|w| w == needle);
    let load_at = find(&arg0_load).expect("the poll loads its flag address into ARG_REGS[0]");
    let call_at = find(&SLOW.to_le_bytes()).expect("the poll calls the slow path");
    assert!(
        load_at < call_at,
        "the flag address must be in ARG_REGS[0] before the CALL"
    );
}

/// gcg g4/oo hunk 5.2 (b), applied by g4/jm: with the owed-cycle latch
/// published (`DirectHelperTable::gen_owed_poll_flag_addr`), the poll also
/// tests it -- its address is a `MOV R11` immediate before the slow path's
/// CALL; unpublished, no latch byte appears and the poll is shorter.
#[test]
fn gcg_g4jm_the_poll_tests_the_owed_cycle_latch_only_when_published() {
    if !jit_safepoint_polls_enabled() {
        return;
    }
    const FLAG: usize = 0x0000_7abc_def0_1230;
    const SLOW: usize = 0x0000_7bcd_ef01_2340;
    const LATCH: usize = 0x0000_7cde_f012_3450;
    let emit = |latch: usize| {
        let mut c = staged_arg_test_compiler();
        c.precise_maps = false;
        c.shadow_enabled = false;
        c.helpers.safepoint_flag_addr = FLAG;
        c.helpers.safepoint_slow_path = SLOW;
        c.direct_helpers.gen_owed_poll_flag_addr = latch;
        let start = c.buf.pos();
        c.emit_safepoint_poll();
        c.buf.as_slice()[start..].to_vec()
    };
    let base = emit(0);
    let armed = emit(LATCH);
    let find =
        |code: &[u8], needle: &[u8]| code.windows(needle.len()).position(|w| w == needle);
    assert!(
        find(&base, &LATCH.to_le_bytes()).is_none(),
        "unpublished: no latch test"
    );
    let latch_at =
        find(&armed, &LATCH.to_le_bytes()).expect("the armed poll loads the latch address");
    let call_at =
        find(&armed, &SLOW.to_le_bytes()).expect("the armed poll calls the slow path");
    assert!(
        latch_at < call_at,
        "the latch is tested before the slow path"
    );
    assert!(armed.len() > base.len());
}

/// t1 wave 8, lane SAFEPOLL (`CRATONVM_JIT_PAGE_POLL`): the page form is
/// `MOV R11, seg:[disp32]`, `TEST [R11 + base], EAX` (the registered
/// site), `JMP .done`, then the byte poll's slow path at the registered
/// recovery offset, and nothing on the fast path tests the flag.
#[test]
fn t1w8_safepoll_the_page_form_registers_its_site_and_slow_path() {
    const FLAG: usize = 0x0000_7abc_def0_1230;
    const SLOW: usize = 0x0000_7bcd_ef01_2340;
    const PAIR: usize = 0x3000_0000;
    let mut c = staged_arg_test_compiler();
    c.precise_maps = false;
    c.shadow_enabled = false;
    c.helpers.safepoint_flag_addr = FLAG;
    c.helpers.safepoint_slow_path = SLOW;
    c.direct_helpers.page_poll_base = PAIR;
    let sites_before = c.implicit_null_sites.len();
    let start = c.buf.pos();
    c.emit_page_form_poll();
    let code = c.buf.as_slice().to_vec();
    assert_eq!(c.implicit_null_sites.len(), sites_before + 1);
    let (site, slow) = *c.implicit_null_sites.last().unwrap();
    // The TLS load first: segment prefix, then `4C 8B 1C 25 disp32`.
    assert_eq!(code[start], crate::x64::inline_rbp_tls_segment_prefix());
    assert_eq!(&code[start + 1..start + 5], &[0x4C, 0x8B, 0x1C, 0x25]);
    assert_eq!(site, start + 9);
    let mut touch = vec![0x41, 0x85, 0x83];
    // Cast: the pair base is a disp32.
    touch.extend_from_slice(&(PAIR as u32).to_le_bytes());
    assert_eq!(&code[site..site + 7], touch.as_slice(), "TEST [R11 + base], EAX");
    assert_eq!(code[site + 7], 0xE9, "JMP rel32 over the slow path");
    assert_eq!(slow, site + 12, "the recovery pc follows the JMP");
    let rel = i32::from_le_bytes([code[site + 8], code[site + 9], code[site + 10], code[site + 11]]);
    // Cast: a rel32 jump displacement.
    let done = (site + 12).wrapping_add_signed(rel as isize);
    assert_eq!(done, code.len(), "the JMP lands after the slow path");
    let find = |needle: &[u8]| code[slow..].windows(needle.len()).position(|w| w == needle);
    assert!(find(&SLOW.to_le_bytes()).is_some(), "the slow path calls the helper");
    assert!(find(&FLAG.to_le_bytes()).is_some(), "with the flag in ARG_REGS[0]");
    assert!(
        code[start..slow].windows(8).all(|w| w != FLAG.to_le_bytes()),
        "the fast path never reads the flag"
    );
}

/// Off (no published pair, or the switch / handler absent), the poll is the
/// byte poll: no page touch and no implicit-null site.
#[test]
fn t1w8_safepoll_without_a_pair_or_the_switch_the_poll_is_the_byte_poll() {
    if !jit_safepoint_polls_enabled() {
        return;
    }
    const FLAG: usize = 0x0000_7abc_def0_1230;
    const SLOW: usize = 0x0000_7bcd_ef01_2340;
    for pair in [0usize, 0x3000_0000] {
        if pair != 0 && crate::poll_page::emit_enabled() {
            continue; // the process runs with the switch on and a handler
        }
        let mut c = staged_arg_test_compiler();
        c.precise_maps = false;
        c.shadow_enabled = false;
        c.helpers.safepoint_flag_addr = FLAG;
        c.helpers.safepoint_slow_path = SLOW;
        c.direct_helpers.page_poll_base = pair;
        let sites_before = c.implicit_null_sites.len();
        let start = c.buf.pos();
        c.emit_safepoint_poll();
        let code = c.buf.as_slice()[start..].to_vec();
        assert_eq!(c.implicit_null_sites.len(), sites_before);
        assert!(code.windows(3).all(|w| w != [0x41, 0x85, 0x83]));
    }
}

/// t1 wave 11, lane GCA: the page form's shape gate (the switch aside)
/// admits a top-level poll with a pair and refuses one inside a splice
/// (whose rollback does not rewind `implicit_null_sites`), one with an
/// exit header, one with the owed-cycle latch, and one with no pair.
#[test]
fn t1w11_gca_the_page_form_is_refused_inside_a_splice() {
    const PAIR: usize = 0x3000_0000;
    let top = || {
        let mut c = staged_arg_test_compiler();
        c.direct_helpers.page_poll_base = PAIR;
        c
    };
    assert!(top().page_form_poll_shape_admits(None), "top level, pair in reach");
    assert!(!top().page_form_poll_shape_admits(Some(7)), "an exit header");
    let mut c = top();
    c.direct_helpers.gen_owed_poll_flag_addr = 0x1000;
    assert!(!c.page_form_poll_shape_admits(None), "the owed-cycle latch");
    let mut c = top();
    c.direct_helpers.page_poll_base = 0;
    assert!(!c.page_form_poll_shape_admits(None), "no pair");
    let mut c = top();
    c.inline_oop_scopes.push(InlineOopScope {
        local_base: 64,
        num_locals: 1,
        masks: vec![0b1; 4],
        reached: vec![true; 4],
        cur_pc: 0,
    });
    assert!(!c.page_form_poll_shape_admits(None), "inside a splice");
}

/// Wave 17, lane L1: the slow-path CALL carries this compile's id in
/// argument register 1, after the flag address, and the flag-clear path is
/// unchanged by it: everything up to and including the `JZ` is the same
/// bytes whatever the id.
#[test]
fn the_poll_names_its_body_in_arg1_on_the_slow_path_only() {
    if !jit_safepoint_polls_enabled() {
        return;
    }
    const FLAG: usize = 0x0000_7abc_def0_1230;
    const SLOW: usize = 0x0000_7bcd_ef01_2340;
    let emit = |reserve: bool| {
        let mut c = staged_arg_test_compiler();
        c.precise_maps = false;
        c.shadow_enabled = false;
        c.helpers.safepoint_flag_addr = FLAG;
        c.helpers.safepoint_slow_path = SLOW;
        // Set either way: `Compiler::new` reserves an id of its own when
        // the inline frame record is available.
        c.compile_id = if reserve {
            crate::CompileIdReservation::reserve()
        } else {
            crate::CompileIdReservation::none()
        };
        let id = c.compile_id.id();
        let start = c.buf.pos();
        c.emit_safepoint_poll();
        (id, c.buf.as_slice()[start..].to_vec())
    };
    let (id, named) = emit(true);
    if id == 0 {
        // Every compile id is live or awaiting grace in this process.
        return;
    }
    let (none, unnamed) = emit(false);
    assert_eq!(none, 0);

    let mut probe = staged_arg_test_compiler();
    let p0 = probe.buf.pos();
    // Cast: x86-64 immediate encoding
    probe.emit_mov_imm64(ARG_REGS[0], FLAG as i64);
    let arg0_load = probe.buf.as_slice()[p0..].to_vec();
    let p1 = probe.buf.pos();
    probe.emit_mov_imm64(ARG_REGS[1], i64::from(id));
    let arg1_load = probe.buf.as_slice()[p1..].to_vec();

    let find =
        |code: &[u8], needle: &[u8]| code.windows(needle.len()).position(|w| w == needle);
    let arg0_at = find(&named, &arg0_load).expect("the flag address in ARG_REGS[0]");
    let arg1_at = find(&named, &arg1_load).expect("the compile id in ARG_REGS[1]");
    let call_at = find(&named, &SLOW.to_le_bytes()).expect("the slow-path CALL");
    assert!(
        arg0_at < arg1_at && arg1_at < call_at,
        "flag, id, then CALL"
    );
    // The fast path: the flag test and the `JZ` over the slow path open
    // both sequences with the same bytes (only the `JZ`'s rel32 differs,
    // by the slow path's extra length), and so does the spill after them.
    let unnamed_arg0 = find(&unnamed, &arg0_load).expect("the unnamed poll's flag load");
    assert_eq!(
        unnamed_arg0, arg0_at,
        "the same bytes precede the argument loads"
    );
    // The thread-word probe can have its own JZ. Find the branch whose
    // decoded target is the continuation after the slow path.
    let skip_jz = |code: &[u8]| {
        code[..arg0_at]
            .windows(6)
            .enumerate()
            .find_map(|(at, bytes)| {
                if bytes[..2] != [0x0F, 0x84] {
                    return None;
                }
                let rel = i32::from_le_bytes(bytes[2..].try_into().unwrap());
                let target = (at + 6).checked_add_signed(rel as isize);
                (target == Some(code.len())).then_some(at)
            })
            .expect("a JZ reaches the end of the poll")
    };
    let jz = skip_jz(&named);
    assert_eq!(jz, skip_jz(&unnamed));
    assert_eq!(
        &named[..jz + 2],
        &unnamed[..jz + 2],
        "flag test and JZ opcode"
    );
    assert_eq!(
        &named[jz + 6..arg0_at],
        &unnamed[jz + 6..arg0_at],
        "the spill before the argument loads"
    );
    let skip = |code: &[u8]| {
        i32::from_le_bytes([code[jz + 2], code[jz + 3], code[jz + 4], code[jz + 5]])
    };
    assert!(
        skip(&named) > skip(&unnamed),
        "only the skipped slow path grew"
    );
}

/// Restores the thread-local moving-young override on drop, so a failing
/// assertion cannot leave it set for the next test on this thread.
struct MovingYoungOverride;
impl MovingYoungOverride {
    fn on() -> Self {
        crate::x64::set_moving_young_override(Some(true));
        MovingYoungOverride
    }
}
impl Drop for MovingYoungOverride {
    fn drop(&mut self) {
        crate::x64::set_moving_young_override(None);
    }
}

/// The `ShadowOddLongProbe.main` shape, reduced: local 0 is a reference
/// parameter the body never reads, local 1 a live reference, locals 2-3 a
/// `long`, and the allocator gave locals 0 and 2 the same register (they
/// never interfere). Three one-byte instructions; at each of them locals 1
/// and 2 are live and local 0 is not.
fn shared_home_compiler() -> Compiler {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: vec![None; 4],
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut c = Compiler::new(
        "dead-shared-home-test".to_string(),
        ExecutableBuffer::new(4096).expect("test executable buffer"),
        4,
        1,
        8,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        alloc_result,
        false,
        // SAFETY: as in `staged_arg_test_compiler`.
        unsafe { std::mem::zeroed() },
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    );
    c.local_assignments = vec![Some(R12), Some(R15), Some(R12), None];
    c.param_oop_mask = 0b001;
    c.local_oop_masks = vec![0b011; 3];
    c.local_oop_reached = vec![true; 3];
    c.local_oop_windows = 1;
    c.local_oop_stride = 3;
    c.local_liveness = vec![0b0110; 3];
    c.local_liveness_words = 1;
    c.local_liveness_covered = vec![true, true, true, false];
    c.cur_bc_pc = 1;
    c
}

/// gc-common w36-b (`common-w4o-shadow-stack-dereferences-a-mistyped-odd-primitive`):
/// a dead reference local whose register another local shares is not a
/// reference home -- not in the mask, not on the shadow stack.
#[test]
fn a_dead_reference_local_sharing_its_register_is_not_published() {
    let _moving = MovingYoungOverride::on();
    let mut c = shared_home_compiler();

    assert_eq!(
        c.local_oop_mask_at_current_pc(),
        Some(0b010),
        "local 0 is dead and r12 holds the long: it must not be named"
    );
    let mut named = Vec::new();
    assert!(c.for_each_oop_local_at_current_pc(|k| named.push(k)));
    assert_eq!(named, vec![1]);
    let homes = c.collect_live_oop_homes();
    assert!(
        !homes.contains(&ShadowHome::Reg(R12)),
        "r12 holds the counter here; homes={homes:?}"
    );
    assert!(homes.contains(&ShadowHome::Reg(R15)), "homes={homes:?}");

    // The method-entry poll: every parameter home holds its own argument.
    c.cur_bc_pc = ENTRY_POLL_BC_PC;
    assert_eq!(c.local_oop_mask_at_current_pc(), Some(0b001));
}

/// The filter drops only what it can prove wrong, and each condition is
/// load-bearing on its own.
#[test]
fn a_reference_home_is_kept_unless_dead_and_shared() {
    let _moving = MovingYoungOverride::on();

    // A register only local 0 owns still holds local 0's last value.
    let mut c = shared_home_compiler();
    c.local_assignments[2] = Some(RBX);
    assert_eq!(c.local_oop_mask_at_current_pc(), Some(0b011));
    assert!(c.collect_live_oop_homes().contains(&ShadowHome::Reg(R12)));

    // Live at the NEXT instruction (a store emitted under this pc).
    let mut c = shared_home_compiler();
    c.local_liveness[2] = 0b0111;
    assert_eq!(c.local_oop_mask_at_current_pc(), Some(0b011));

    // Kept for a debugger.
    let mut c = shared_home_compiler();
    c.local_assigned = vec![0b0001; 3];
    assert_eq!(c.local_oop_mask_at_current_pc(), Some(0b011));

    // No liveness table (the ungated compile): no claim, nothing dropped.
    let mut c = shared_home_compiler();
    c.local_liveness_words = 0;
    assert_eq!(c.local_oop_mask_at_current_pc(), Some(0b011));

    // An uncovered pc, or no next instruction to consult.
    let mut c = shared_home_compiler();
    c.cur_bc_pc = 2;
    assert_eq!(
        c.local_oop_mask_at_current_pc(),
        Some(0b011),
        "pc 2 has no covered successor"
    );
}

/// The shared-register rule drops a dead reference local only when its
/// register is shared (gcg g6/jm: kept from the deleted
/// `CRATONVM_JIT_SP_LOCAL_LIVENESS` test, whose other half is gone).
#[test]
fn gcg_g1s_the_shared_register_rule_drops_only_a_shared_registers_dead_local() {
    let c = shared_home_compiler();
    assert_eq!(c.dead_oop_locals_with(0, 0b011), 0b001, "shared register");
    let mut c = shared_home_compiler();
    c.local_assignments[2] = Some(RBX);
    assert_eq!(c.dead_oop_locals_with(0, 0b011), 0, "unshared register");
    let mut c = shared_home_compiler();
    c.local_assignments[0] = None;
    assert_eq!(c.dead_oop_locals_with(0, 0b011), 0, "frame-homed");
}

/// gch h4/jm (`CRATONVM_JIT_SP_MAP_LOCAL_LIVENESS`): with the rule on, a
/// dead reference local is dropped whether its register is shared,
/// unshared or absent (frame-homed); a live one never is; and every
/// refusal of the shared-register rule (uncovered pc, no successor, the
/// entry poll, the debugger's kept set) still refuses.
#[test]
fn gch_h4jm_every_dead_reference_local_is_dropped_under_the_map_liveness_rule() {
    // Shared register (r12 with the long): both rules drop local 0.
    let c = shared_home_compiler();
    assert_eq!(c.dead_oop_locals_rule(0, 0b011, false), 0b001);
    assert_eq!(c.dead_oop_locals_rule(0, 0b011, true), 0b001);

    // Unshared register: only the liveness rule drops it.
    let mut c = shared_home_compiler();
    c.local_assignments[2] = Some(RBX);
    assert_eq!(c.dead_oop_locals_rule(0, 0b011, false), 0);
    assert_eq!(c.dead_oop_locals_rule(0, 0b011, true), 0b001);

    // Frame-homed (the `relay` parameter's shape): only the liveness rule.
    let mut c = shared_home_compiler();
    c.local_assignments[0] = None;
    assert_eq!(c.dead_oop_locals_rule(0, 0b011, false), 0);
    assert_eq!(c.dead_oop_locals_rule(0, 0b011, true), 0b001);

    // Live at the next instruction: kept.
    let mut c = shared_home_compiler();
    c.local_assignments[0] = None;
    c.local_liveness[2] = 0b0111;
    assert_eq!(c.dead_oop_locals_rule(0, 0b011, true), 0);

    // The debugger's kept set: kept.
    let mut c = shared_home_compiler();
    c.local_assignments[0] = None;
    c.local_assigned = vec![0b0001; 3];
    assert_eq!(c.dead_oop_locals_rule(0, 0b011, true), 0);

    // No covered successor, an uncovered pc, the entry poll: nothing.
    let mut c = shared_home_compiler();
    c.local_assignments[0] = None;
    c.cur_bc_pc = 2;
    assert_eq!(c.dead_oop_locals_rule(0, 0b011, true), 0, "no successor");
    c.cur_bc_pc = 3;
    assert_eq!(c.dead_oop_locals_rule(0, 0b011, true), 0, "uncovered");
    c.cur_bc_pc = ENTRY_POLL_BC_PC;
    assert_eq!(c.dead_oop_locals_rule(0, 0b011, true), 0, "entry poll");

    // The live local 1 is never dropped by either rule.
    let c = shared_home_compiler();
    assert_eq!(c.dead_oop_locals_rule(0, 0b010, true), 0);
}


/// gci i8/sp (`CRATONVM_JIT_SP_MAP_LOCAL_LIVENESS_IN_SPLICES`): inside a
/// splice the map-liveness rule is refused unless the second switch is on
/// too; with both on, the OUTER method's dead local 0 is dropped at the
/// splice's safepoint exactly as at a top-level one; the second switch
/// alone changes nothing; a debugger still refuses; and outside a splice
/// the second switch is never consulted (same answer either way).



/// OOM21-6 / ARGWIN21-6: under the map-liveness rule the monotone form
/// looks past the 64-byte next-instruction bound, answers from the pc's
/// own liveness with no later instruction, and drops every candidate at
/// an uncovered pc; the shared-register rule and the zeroing callers'
/// conservative form are unchanged at each of those points.
#[test]
fn oom21_6_the_map_liveness_rule_is_monotone() {
    let unshared = || {
        let mut c = shared_home_compiler();
        c.local_assignments[2] = Some(RBX);
        c
    };
    // A gap past the bound: the next instruction is 100 bytes on.
    let mut c = unshared();
    c.local_liveness = vec![0b0110; 102];
    c.local_liveness_covered = vec![false; 102];
    c.local_liveness_covered[1] = true;
    c.local_liveness_covered[101] = true;
    c.local_oop_masks = vec![0b011; 102];
    c.local_oop_reached = vec![true; 102];
    c.local_oop_stride = 102;
    assert_eq!(c.dead_oop_locals_rule_monotone(0, 0b011, true, true), 0b001, "far next");
    assert_eq!(c.dead_oop_locals_rule(0, 0b011, true), 0, "conservative form");
    c.local_liveness[101] = 0b0111;
    assert_eq!(c.dead_oop_locals_rule_monotone(0, 0b011, true, true), 0, "live far on");
    // No later instruction at all: the pc's own liveness.
    let mut c = unshared();
    c.local_liveness_covered = vec![false; 4];
    c.local_liveness_covered[1] = true;
    assert_eq!(c.dead_oop_locals_rule_monotone(0, 0b011, true, true), 0b001, "last insn");
    assert_eq!(c.dead_oop_locals_rule(0, 0b011, true), 0);
    // An uncovered pc: every candidate.
    let mut c = unshared();
    c.cur_bc_pc = 2;
    c.local_liveness_covered = vec![true, true, false, true];
    assert_eq!(c.dead_oop_locals_rule_monotone(0, 0b011, true, true), 0b011, "uncovered");
    assert_eq!(c.dead_oop_locals_rule(0, 0b011, true), 0);
    // The shared-register rule never takes the monotone form.
    assert_eq!(c.dead_oop_locals_rule_monotone(0, 0b011, false, true), 0);
}
