use super::*;

fn ask() -> PagePollAsk {
    PagePollAsk {
        back_edge: true,
        mode_exit: false,
        stress: false,
        owed_latch: false,
        pair_base: 0x1000_0000,
    }
}

#[test]
fn j27_page_poll_admits_a_plain_or_mode_exit_back_edge() {
    assert!(shape_admits(ask()));
    assert!(shape_admits(PagePollAsk {
        mode_exit: true,
        ..ask()
    }));
    assert!(shape_admits(PagePollAsk {
        owed_latch: true,
        ..ask()
    }));
    let refused = [
        PagePollAsk {
            back_edge: false,
            ..ask()
        },
        PagePollAsk {
            stress: true,
            ..ask()
        },
        PagePollAsk {
            pair_base: 0,
            ..ask()
        },
        PagePollAsk {
            pair_base: 0x1000_0010,
            ..ask()
        },
        PagePollAsk {
            pair_base: 0x1000_1000,
            ..ask()
        },
        PagePollAsk {
            pair_base: 0x7FFF_F000,
            ..ask()
        },
    ];
    for a in refused {
        assert!(!shape_admits(a), "{a:?} must take the byte poll");
    }
}

#[test]
fn j27_page_poll_bytes_are_the_single_pass_encoding() {
    let bytes = page_poll_bytes(0x64, 0x0000_0A28, 0x1000_0000);
    assert_eq!(
        bytes,
        [
            0x64, 0x4C, 0x8B, 0x1C, 0x25, 0x28, 0x0A, 0x00, 0x00, // MOV R11, fs:[0xA28]
            0x41, 0x85, 0x83, 0x00, 0x00, 0x00, 0x10, // TEST [R11 + 0x10000000], EAX
        ]
    );
    assert_eq!(&bytes[SITE_OFFSET..SITE_OFFSET + 3], &[0x41, 0x85, 0x83]);
}

#[test]
fn j27_page_poll_inline_skip_is_jz_off_and_jmp_on() {
    assert_eq!(inline_skip_opcode(false), &[0x0F, 0x84]);
    assert_eq!(inline_skip_opcode(true), &[0xE9]);
}

#[cfg(target_arch = "x86_64")]
#[test]
fn forced_exit_poll_cc_accepts_and_patches_page_form() {
    let mut code = [
        0x64, 0x4C, 0x8B, 0x1C, 0x25, 0x28, 0x0A, 0x00, 0x00, // MOV R11, fs:[0xA28]
        0x41, 0x85, 0x83, 0x00, 0x00, 0x00, 0x10, // TEST [R11 + 0x10000000], EAX
    ];
    let cc = SITE_OFFSET + 4;
    let b = crate::not_entrant::forced_exit_poll_cc(&code, 0x2000_0000, cc, 0)
        .expect("page-form exit poll must be recognized");
    assert_eq!(b, 0x20);
    code[cc] = b;
    let patched_disp = u32::from_le_bytes([code[12], code[13], code[14], code[15]]) as usize;
    assert_eq!(
        patched_disp,
        0x1000_0000 + crate::poll_page::FORCED_EXIT_OFFSET
    );
    assert_eq!(
        crate::not_entrant::forced_exit_poll_cc(&code, 0x2000_0000, cc, 0),
        Some(0x20)
    );
    let bad_align = [
        0x64, 0x4C, 0x8B, 0x1C, 0x25, 0x28, 0x0A, 0x00, 0x00, 0x41, 0x85, 0x83, 0x00, 0x10, 0x00,
        0x10,
    ];
    assert_eq!(
        crate::not_entrant::forced_exit_poll_cc(&bad_align, 0x2000_0000, cc, 0),
        None
    );
}

#[test]
fn try_emit_exact_final_checkcast_fires_when_proxy_cell_present() {
    use crate::ir_lower::*;
    let mut g = Graph::empty(8);
    let ctrl = g.add(Op::Region, IrType::Control, vec![], None);
    let obj = g.add(Op::Param(0), IrType::Ref, vec![], None);
    let (ptr, len) =
        crate::intern_typecheck_target_with_finality("java/lang/String", Some(42), true);
    let cast = g.add(
        Op::CheckCast {
            name_ptr: ptr as usize,
            name_len: len,
        },
        IrType::Ref,
        vec![ctrl, obj],
        None,
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, cast], None);

    let g = Box::leak(Box::new(g));
    let sched = Box::leak(Box::new(crate::ir_schedule::schedule(g)));
    let plan = Box::leak(Box::new(plan_slots(g, sched, None)));
    let mut lo = crate::ir_lower::tests::lowerer_over(g, sched, plan, 4096);
    lo.cur_def = Some(cast);
    let slot = lo.alloc_slot(cast);
    let mut hit_patch = None;
    let mut to_join = Vec::new();
    assert!(lo.try_emit_exact_final_checkcast(
        cast,
        obj,
        slot,
        ptr as usize,
        len,
        42,
        Some(0x1000),
        false,
        false,
        &mut hit_patch,
        &mut to_join,
    ));
    assert!(hit_patch.is_some());
    assert!(lo.checkcast_null_or_exact_proven.contains(&(cast, 42)));
    assert!(!lo.checkcast_exact_proven(cast, 42));
    lo.note_receiver_non_null(cast);
    assert!(lo.checkcast_exact_proven(cast, 42));
}

#[test]
fn note_elide_refusal_marks_dropped_const_and_retry_keeps_home() {
    use crate::ir_lower::*;
    let scope = ElideRetryScope::enter();
    let mut g = Graph::empty(8);
    let ctrl = g.add(Op::Region, IrType::Control, vec![], None);
    let c = g.add(Op::Const(42), IrType::Int, vec![], None);
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, c], None);

    let g = Box::leak(Box::new(g));
    let sched = Box::leak(Box::new(crate::ir_schedule::schedule(g)));
    let plan = Box::leak(Box::new(plan_slots(g, sched, None)));
    let mut lo = crate::ir_lower::tests::lowerer_over(g, sched, plan, 4096);
    lo.phi_copy_regs_on = true;
    lo.const_imm_on = true;
    lo.mir_mode = MirMode::Off;
    lo.home_dropped = vec![false; g.nodes.len()];
    lo.elide_excluded = vec![false; g.nodes.len()];
    assert!(lo.drop_const_homes(true));
    assert!(lo.is_dropped_const(c));

    lo.note_elide_refusal(c);
    assert!(scope.exclude_refused());
    assert!(elide_excluded_ids().contains(&c));

    lo.home_dropped[c as usize] = false;
    lo.elide_excluded[c as usize] = true;
    assert!(!lo.drop_const_homes(true));
    assert!(!lo.home_dropped[c as usize]);
}

#[test]
fn on_phi_recurrence_matches_4_step_unrolled_accumulator_chain() {
    use crate::ir::{Graph, IrType, Op, NO_NODE};
    use crate::ir_lower::on_phi_recurrence;
    let mut g = Graph::empty(16);
    let ctrl = g.add(Op::Region, IrType::Control, vec![], None);
    let init = g.add(Op::Const(0), IrType::Int, vec![], None);
    let v = g.add(Op::Const(1), IrType::Int, vec![], None);
    let side_in = g.add(Op::Const(2), IrType::Int, vec![], None);
    let phi = g.add(Op::Phi, IrType::Int, vec![ctrl, init, NO_NODE], None);
    let s0 = g.add(Op::Add, IrType::Int, vec![phi, v], None);
    let s1 = g.add(Op::Add, IrType::Int, vec![s0, v], None);
    let s2 = g.add(Op::Add, IrType::Int, vec![s1, v], None);
    let s3 = g.add(Op::Add, IrType::Int, vec![s2, v], None);
    g.nodes[phi as usize].inputs[2] = s3;
    let unrelated = g.add(Op::Add, IrType::Int, vec![side_in, v], None);
    let _sink = g.add(Op::Add, IrType::Int, vec![s3, unrelated], None);

    assert!(on_phi_recurrence(&g, s0 as usize));
    assert!(on_phi_recurrence(&g, s1 as usize));
    assert!(on_phi_recurrence(&g, s2 as usize));
    assert!(!on_phi_recurrence(&g, unrelated as usize));
}

#[test]
fn establish_getfield_registers_implicit_null_unless_withdrawn() {
    use crate::ir_lower::*;
    crate::implicit_null::note_fault_handler_installed();
    assert!(crate::implicit_null::active());

    let run = |withdrawn: bool| -> usize {
        let mut g = Graph::empty(8);
        let ctrl = g.add(Op::Region, IrType::Control, vec![], None);
        let base = g.add(Op::Param(0), IrType::Ref, vec![], None);
        let mem = g.add(Op::Start, IrType::Memory, vec![], None);
        let off = g.add(Op::Const(0), IrType::Int, vec![], None);
        let load = g.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            vec![ctrl, mem, base, off],
            Some(10),
        );
        g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, load], None);
        let mut set = std::collections::HashSet::new();
        if withdrawn {
            set.insert(10);
        }
        g.proven_compact_withdrawn_pcs = Some(set);

        let g = Box::leak(Box::new(g));
        let sched = Box::leak(Box::new(crate::ir_schedule::schedule(g)));
        let plan = Box::leak(Box::new(plan_slots(g, sched, None)));
        let mut lo = crate::ir_lower::tests::lowerer_over(g, sched, plan, 4096);
        lo.cur_def = Some(load);
        lo.cur_node_pc = Some(10);
        lo.cur_bci = 10;
        lo.proven_fields_on = true;
        let slot = lo.alloc_slot(load);
        lo.emit_proven_receiver_getfield(
            ProvenReceiverMode::Establish,
            10,
            IrType::Int,
            base,
            0,
            slot,
            b'I',
            8,
            false,
            None,
        );
        lo.page_poll_sites.len()
            + lo.deferred_getfield_tails
                .iter()
                .filter(|t| t.implicit_nn_site.is_some())
                .count()
    };

    assert_eq!(run(false), 1);
    assert_eq!(run(true), 0);
}
