use super::*;

/// Interpreter round i1 wave 28, lane L2: the shared staging fills the
/// three table fields in table order, the catch-all bit from
/// `catch_type == 0`, and leaves `precise_exception_frames` alone.
#[test]
fn the_exception_table_is_staged_in_table_order() {
    use cratonvm_reader::attribute::ExceptionTableEntry;
    let table = [
        ExceptionTableEntry {
            start_pc: 2,
            end_pc: 9,
            handler_pc: 12,
            catch_type: 7,
        },
        ExceptionTableEntry {
            start_pc: 2,
            end_pc: 12,
            handler_pc: 20,
            catch_type: 0,
        },
    ];
    let mut request = BackendRequest::default();
    request.stage_exception_table(&table);
    assert_eq!(request.exception_ranges, vec![(2, 9, 12), (2, 12, 20)]);
    assert_eq!(request.exception_catch_all, vec![false, true]);
    assert_eq!(request.protected_ranges, vec![(2, 9), (2, 12)]);
    assert!(!request.precise_exception_frames);
    request.stage_exception_table(&[]);
    assert!(request.exception_ranges.is_empty());
    assert!(request.exception_catch_all.is_empty());
    assert!(request.protected_ranges.is_empty());
}

#[test]
fn the_default_request_asks_for_nothing() {
    let request = BackendRequest::default();
    assert_eq!(request.direct_helpers, crate::DirectHelperTable::EMPTY);
    assert_eq!(request.verified_max_stack, None);
    assert!(request.exception_ranges.is_empty());
    assert!(request.exception_catch_all.is_empty());
    assert!(request.local_handler_table.is_empty());
    assert!(request.protected_ranges.is_empty());
    assert!(!request.kernel_reg_homes);
    assert!(!request.kernel_reg_homes_osr);
    assert!(!request.precise_exception_frames);
    assert!(!request.baseline_mode);
    assert!(request.volatile_field_pcs.is_empty());
    assert!(request.ldc_slots.is_empty());
    assert!(!request.mode_exit_polls);
    assert!(!request.debugger_observes_locals);
    assert!(request.param_slot_tags.is_empty());
    assert!(!request.self_lock_receiver);
    assert_eq!(request.self_lock_mirror_slot, 0);
    assert!(request.first_call_cells.is_empty());
    assert!(!request.method_known_unsynchronized);
}
