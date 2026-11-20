use super::*;

#[test]
fn gci_i31_gq_the_door_carry_is_read_once_and_only_at_its_stamp() {
    let mut proc = ReferenceProcessor::new();
    proc.carry_door_queued_finalizables(&[0x100, 0, 0x200], 5);
    assert_eq!(proc.take_carried_door_queued_for(5), vec![0x100, 0x200]);
    assert!(proc.take_carried_door_queued_for(5).is_empty(), "taken");
    proc.carry_door_queued_finalizables(&[0x300], 6);
    assert!(proc.take_carried_door_queued_for(7).is_empty(), "a later collection");
    assert!(proc.take_carried_door_queued_for(6).is_empty(), "emptied by the stale read");
    proc.carry_door_queued_finalizables(&[0x400], 8);
    proc.carry_door_queued_finalizables(&[], 9);
    assert!(proc.take_carried_door_queued_for(8).is_empty(), "replaced");
}

#[test]
fn gci_i31_gq_the_staged_set_is_taken_once_and_dies_with_the_round() {
    let mut proc = ReferenceProcessor::new();
    assert!(proc.take_remark_staged_resurrected().is_empty());
    proc.stage_remark_resurrected(vec![0x100, 0x140]);
    assert_eq!(proc.take_remark_staged_resurrected(), vec![0x100, 0x140]);
    assert!(proc.take_remark_staged_resurrected().is_empty());
    proc.stage_remark_resurrected(vec![0x180]);
    proc.finish_pre_gc_cycle();
    assert!(proc.take_remark_staged_resurrected().is_empty());
}
