//! Round 9 wave 9 (arr9): the native loop unroller's patch snapshot.

use super::*;

/// Every unrolled copy of a fallible site that routes its "threw" edge to
/// a compiled local handler gets its own `local_handler_stubs` entry,
/// shifted by the copy's distance from the body. Without it the copy's
/// `rel32` stays the placeholder `0` and the branch falls through into the
/// keep path, losing the exception for that iteration.
#[test]
fn an_unrolled_copy_keeps_its_local_handler_stub_entry() {
    let mut c = super::super::emit::tests::test_compiler();
    // 0: nop ; 1: nop ; 2: goto 0 ; (padding)
    let code = [0x00u8, 0x00, 0xa7, 0xff, 0xfe, 0x00, 0x00];
    let code_len = 5;
    c.pc_to_native = vec![-1; code.len() + 1];
    let body_start = c.buf.pos();
    c.pc_to_native[0] = body_start as i32;
    // A stand-in fallible site: `nop ; JO rel32` with the edge recorded
    // as a local-handler stub (site 7, throw bci 1, shared exit).
    c.buf.emit(&[0x90]);
    let patch = c.emit_jcc_rel32_patch(0x80);
    c.local_handler_stubs.push((patch, 7, 1, false));
    c.pc_to_native[2] = c.buf.pos() as i32;
    let body_len = c.buf.pos() - body_start;
    c.unroll_loops = vec![(0, 2, 2)];
    let mut dead = false;
    let targets = vec![false; code.len()];
    let starts = vec![true; code.len()];
    let edges = super::super::arith::MinMaxEdgeCounts::default();
    let step = c.walk_control(
        &code, code_len, 0xa7, 2, &mut dead, &targets, &starts, &edges,
    );
    assert!(matches!(step, WalkStep::Next(5)));
    assert_eq!(
        c.local_handler_stubs,
        vec![
            (patch, 7, 1, false),
            (patch + body_len, 7, 1, false),
            (patch + 2 * body_len, 7, 1, false),
        ],
        "one entry per copy, same site and bci, shifted patch offset"
    );
}

/// Round 11 wave 18 (P2 of `r11-orch-callee-compiled-bodies-are-never-promoted`):
/// a backward `goto` in a body whose prologue allocated the entry-counter
/// box bumps the box's back-edge word (`mov r11, imm64(base)` then
/// `add qword [r11+16], 1`, 15 bytes); a body with no box emits exactly
/// what it emitted before.
#[test]
fn a_back_edge_bumps_the_back_edge_word_only_in_a_counted_body() {
    if !crate::entry_counter::callee_promotion_enabled() {
        return; // the kill switch is set in this environment
    }
    // 0: nop ; 1: nop ; 2: goto 0 ; (padding)
    let code = [0x00u8, 0x00, 0xa7, 0xff, 0xfe, 0x00, 0x00];
    let code_len = 5;
    let targets = vec![false; code.len()];
    let starts = vec![true; code.len()];
    let edges = super::super::arith::MinMaxEdgeCounts::default();
    let mut emitted: Vec<Vec<u8>> = Vec::new();
    let mut want: Vec<u8> = Vec::new();
    for counted in [false, true] {
        let mut c = super::super::emit::tests::test_compiler();
        c.pc_to_native = vec![-1; code.len() + 1];
        c.pc_to_native[0] = c.buf.pos() as i32;
        if counted {
            let counter = Box::new(crate::entry_counter::JitEntryCounter::new());
            want = vec![0x49, 0xBB]; // MOV R11, imm64
            want.extend_from_slice(&(counter.base_addr() as u64).to_le_bytes());
            // ADD QWORD [R11 + 16], 1
            want.extend_from_slice(&[0x49, 0x83, 0x43, 0x10, 0x01]);
            c.entry_counter = Some(counter);
        }
        let start = c.buf.pos();
        let mut dead = false;
        let step = c.walk_control(
            &code, code_len, 0xa7, 2, &mut dead, &targets, &starts, &edges,
        );
        assert!(matches!(step, WalkStep::Next(5)));
        emitted.push(c.buf.as_slice()[start..c.buf.pos()].to_vec());
    }
    assert_eq!(emitted[1].len(), emitted[0].len() + want.len());
    assert!(
        emitted[1].windows(want.len()).any(|w| w == want.as_slice()),
        "the counted body carries the bump against its own box"
    );
    assert!(
        !emitted[0]
            .windows(2)
            .any(|w| w == [0x49u8, 0xBB].as_slice()),
        "an uncounted body emits no bump"
    );
}
