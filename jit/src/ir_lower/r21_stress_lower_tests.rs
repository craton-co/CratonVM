use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

#[test]
fn poll_stress_values_parse_to_a_period_and_an_optional_seed() {
    assert_eq!(parse_poll_stress(""), None);
    assert_eq!(parse_poll_stress("0"), None);
    assert_eq!(parse_poll_stress("off"), None);
    assert_eq!(parse_poll_stress("false"), None);
    assert_eq!(parse_poll_stress("0:5"), None);
    assert_eq!(parse_poll_stress("1"), Some((1, None)));
    assert_eq!(parse_poll_stress(" 50 "), Some((50, None)));
    assert_eq!(parse_poll_stress("50:7"), Some((50, Some(7))));
    assert_eq!(parse_poll_stress("50:x"), Some((50, None)));
    assert_eq!(
        parse_poll_stress("on"),
        Some((POLL_STRESS_DEFAULT_PERIOD, None))
    );
}

#[test]
fn a_seed_staggers_the_first_forced_arrival_inside_the_period() {
    assert_eq!(poll_stress_initial_countdown(10, None, 3), 10);
    assert_eq!(poll_stress_initial_countdown(0, None, 0), 1);
    assert_eq!(poll_stress_initial_countdown(1, Some(9), 5), 1);
    let firsts: std::collections::BTreeSet<u32> = (0..64)
        .map(|site| poll_stress_initial_countdown(10, Some(12345), site))
        .collect();
    assert!(firsts.iter().all(|c| (1..=10).contains(c)));
    assert!(firsts.len() > 1, "sites do not all fire together");
}

#[test]
fn the_countdown_is_inert_off_and_jumps_to_the_poll_jcc_on() {
    let mut lo = super::r16_codegen_tests::emitter_lowerer();
    lo.poll_stress_period = 0;
    let at = lo.buf.pos();
    assert_eq!(lo.emit_poll_stress_countdown(), None);
    assert_eq!(lo.buf.pos(), at, "nothing emitted off");
    assert!(lo.poll_stress_cells.is_empty());

    lo.poll_stress_period = 5;
    lo.poll_stress_seed = None;
    let at = lo.buf.pos();
    let disp = lo.emit_poll_stress_countdown().expect("emitted on");
    assert_eq!(lo.poll_stress_cells.len(), 1);
    assert_eq!(lo.poll_stress_cells[0].load(Ordering::Relaxed), 5);
    let cell = lo.poll_stress_cells[0].as_ref() as *const AtomicU32 as u64;
    let code = lo.buf.as_slice()[at..].to_vec();
    assert_eq!(&code[..2], &[0x49, 0xBB], "MOV R11, imm64");
    assert_eq!(&code[2..10], &cell.to_le_bytes());
    assert_eq!(
        &code[10..],
        &[
            0xF0, 0x41, 0x83, 0x2B, 0x01, // LOCK SUB DWORD [R11], 1
            0x75, 0x0C, // JNZ normal
            0x41, 0xC7, 0x03, 5, 0, 0, 0, // MOV DWORD [R11], 5
            0x48, 0x85, 0xE4, // TEST RSP, RSP
            0xEB, 0x00, // JMP rel8 (patched by the poll)
        ]
    );
    assert_eq!(disp, lo.buf.pos() - 1, "the rel8 the poll patches");
}

#[test]
fn a_dead_home_clear_writes_zero_off_and_the_poison_on() {
    let mut lo = super::r16_codegen_tests::emitter_lowerer();
    lo.dead_home_poison = false;
    let at = lo.buf.pos();
    lo.emit_dead_home_clear(8);
    assert_eq!(
        &lo.buf.as_slice()[at..],
        &[0x48, 0xC7, 0x85, 0xF8, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0],
        "off: the zero store, byte for byte"
    );
    lo.dead_home_poison = true;
    let at = lo.buf.pos();
    lo.emit_dead_home_clear(8);
    let poison = cratonvm_types::jit_activation::DEAD_HOME_POISON;
    let mut want: Vec<u8> = vec![0xC7, 0x85, 0xF8, 0xFF, 0xFF, 0xFF];
    want.extend_from_slice(&(poison as u32).to_le_bytes());
    want.extend_from_slice(&[0xC7, 0x85, 0xFC, 0xFF, 0xFF, 0xFF]);
    want.extend_from_slice(&((poison >> 32) as u32).to_le_bytes());
    assert_eq!(&lo.buf.as_slice()[at..], want.as_slice());
}

/// The countdown sits in front of every flag test of a back-edge poll
/// (the thread word, the STW flag, the owed latch) and its jump is
/// patched after the last of them.
#[test]
fn the_back_edge_poll_patches_the_countdown_past_every_flag_test() {
    let src = concat!(
        include_str!("../ir_lower/mod.rs"),
        "\n",
        include_str!("../ir_lower/virtual_object_info.rs"),
        "\n",
        include_str!("../ir_lower/mir_plan_and_init.rs"),
        "\n",
        include_str!("../ir_lower/vector_preloop.rs"),
        "\n",
        include_str!("../ir_lower/value_phi_copies.rs"),
        "\n",
        include_str!("../ir_lower/field_proof_entry_is_rechecked_osr.rs"),
        "\n",
        include_str!("../ir_lower/int_expr_bounds.rs"),
        "\n",
        include_str!("../ir_lower/character_to_chars_prefix.rs"),
        "\n",
        include_str!("../ir_lower/getfield_legacy_read.rs"),
        "\n",
        include_str!("../ir_lower/post_call_exit_site_for.rs"),
        "\n",
        include_str!("../ir_lower/entry_fold_node.rs"),
        "\n",
        include_str!("../ir_lower/call_loaded_ic_entry_releasing.rs"),
        "\n",
        include_str!("../ir_lower/fallthrough_poll_spills.rs"),
        "\n",
        include_str!("../ir_lower/ir_ic_cascade_admits_kind.rs"),
        "\n",
        include_str!("../ir_lower/implicit_kind_matches_catch.rs"),
        "\n",
        include_str!("../ir_lower/scoped_raw_tail.rs"),
        "\n",
        include_str!("../ir_lower/frame_needs.rs"),
        "\n",
        include_str!("../ir_lower/back_edge_dead_ref_homes.rs"),
        "\n",
        include_str!("../ir_lower/block_lazy_clear_survivors.rs"),
        "\n",
        include_str!("../ir_lower/promoted_pending_mark.rs"),
        "\n",
        include_str!("../ir_lower/site_value_live_at.rs"),
        "\n",
        include_str!("../ir_lower/plan_register_residency_with.rs"),
        "\n",
        include_str!("../ir_lower/inner_attempt.rs"),
        "\n",
        include_str!("../ir_lower/g1_aastore_screen.rs"),
        "\n",
        include_str!("../ir_lower/scoped_session_confined_rows.rs")
    )
    .replace("\r\n", "\n");
    let start = src
        .find("fn emit_safepoint_poll(")
        .expect("the poll emitter");
    let poll = &src[start..];
    let poll = &poll[..poll.find("\n    \u{7d}\n").expect("its end")];
    let countdown = poll
        .find("self.emit_poll_stress_countdown()")
        .expect("countdown");
    let word = poll.find("let word_tested =").expect("thread word test");
    let owed = poll.find("let owed_latch =").expect("owed latch test");
    let patch = poll
        .find("if let Some(jmp_disp) = stress_jmp")
        .expect("patch");
    let jcc = poll.find("// JZ .clear").expect("the inline Jcc");
    assert!(countdown < word && word < owed && owed < patch && patch < jcc);
}
