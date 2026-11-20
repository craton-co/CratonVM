//! JIT round 19 proposal wave 7, lane gc7 (SYNC619-2): a compiled
//! activation's hold is published only as the lock-stack slot of the
//! object at that address, merged in report order.
use super::merge_compiled_hold_entries;
use crate::types::ObjectRef;

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; the merge only compares addresses.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

#[test]
fn compiled_holds_merge_by_trace_index_and_resolve_to_lock_stack_slots() {
    let keys = [obj(0x1000), obj(0x2000), obj(0x3000)];
    // Interpreter monitors at trace indices 6 and 1 (innermost first).
    let interp = vec![(6, 0), (1, 2)];
    // Compiled holds at index 4 (two blocks, newest first) and 9; one
    // address on no lock-stack slot, one index past the trace.
    let compiled = [
        (9, 0x2000),
        (4, 0x3000),
        (4, 0x1000),
        (4, 0x5000),
        (12, 0x1000),
    ];
    let merged = merge_compiled_hold_entries(interp.clone(), &compiled, &keys, 10);
    assert_eq!(merged, vec![(9, 1), (6, 0), (4, 2), (4, 0), (1, 2)]);
    // Nothing compiled, or nothing that resolves: the interpreter list as is.
    assert_eq!(
        merge_compiled_hold_entries(interp.clone(), &[], &keys, 10),
        interp
    );
    assert_eq!(
        merge_compiled_hold_entries(interp.clone(), &[(3, 0x9000)], &keys, 10),
        interp
    );
}
