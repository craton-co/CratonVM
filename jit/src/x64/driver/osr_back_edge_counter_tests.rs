use super::park_back_edge_counter_for_door;
use crate::compile_gate::CompileDoor;

/// Round 11 wave 19: a back edge walked after the OSR door parked the box
/// emits no bump, the two method-entry doors still emit it, and the parked
/// box is the one the prologue allocated (so finalize hands the artifact
/// the word its prologue names).
#[test]
fn only_the_osr_door_parks_the_back_edge_counter() {
    if !crate::entry_counter::callee_promotion_enabled() {
        return; // the kill switch is set in this environment
    }
    // 0: nop ; 1: nop ; 2: goto 0 ; (padding)
    let code = [0x00u8, 0x00, 0xa7, 0xff, 0xfe, 0x00, 0x00];
    let code_len = 5;
    let targets = vec![false; code.len()];
    let starts = vec![true; code.len()];
    let edges = crate::x64::arith::MinMaxEdgeCounts::default();
    for door in CompileDoor::ALL {
        let mut c = crate::x64::emit::tests::test_compiler();
        let counter = Box::new(crate::entry_counter::JitEntryCounter::new());
        let base = counter.base_addr();
        let mut bump = vec![0x49u8, 0xBB]; // MOV R11, imm64(base)
        bump.extend_from_slice(&(base as u64).to_le_bytes());
        c.entry_counter = Some(counter);
        c.pc_to_native = vec![-1; code.len() + 1];
        c.pc_to_native[0] = c.buf.pos() as i32;

        let parked = park_back_edge_counter_for_door(&mut c.entry_counter, door);
        assert_eq!(parked.is_some(), door == CompileDoor::Osr, "{door:?}");

        let start = c.buf.pos();
        let mut dead = false;
        let step = c.walk_control(
            &code, code_len, 0xa7, 2, &mut dead, &targets, &starts, &edges,
        );
        assert!(matches!(step, crate::x64::bytecode_walk::WalkStep::Next(5)));
        let emitted = c.buf.as_slice()[start..c.buf.pos()].to_vec();
        let bumped = emitted.windows(bump.len()).any(|w| w == bump.as_slice());
        assert_eq!(bumped, door != CompileDoor::Osr, "{door:?}");

        let restored = c.entry_counter.take().or(parked);
        assert_eq!(
            restored.map(|b| b.base_addr()),
            Some(base),
            "{door:?}: the artifact must own the box its prologue baked"
        );
    }
}
