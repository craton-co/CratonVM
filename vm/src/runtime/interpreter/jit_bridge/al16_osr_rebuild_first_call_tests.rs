use super::some_first_call_cell_fired;
use std::sync::atomic::{AtomicU8, Ordering};

unsafe extern "C" fn no_resolve(_: *const cratonvm_jit::JitFirstCallCell) {}

fn cell() -> Box<cratonvm_jit::JitFirstCallCell> {
    Box::new(cratonvm_jit::JitFirstCallCell {
        armed: AtomicU8::new(1),
        resolve: no_resolve,
        ctx: 0,
        referencing: 7,
        owner: 9,
    })
}

fn addr(cell: &cratonvm_jit::JitFirstCallCell) -> usize {
    cell as *const cratonvm_jit::JitFirstCallCell as usize
}

/// A body remembered with armed cells stays current until one fires.
#[test]
fn an_entry_is_stale_once_one_of_its_cells_fired() {
    let (a, b) = (cell(), cell());
    let cells = [addr(&a), addr(&b)];
    assert!(!some_first_call_cell_fired(&cells));
    b.armed.store(0, Ordering::Release);
    assert!(some_first_call_cell_fired(&cells));
}

/// No cells at build (every site resolved, or the switch off): never stale.
#[test]
fn an_entry_without_cells_is_never_stale() {
    assert!(!some_first_call_cell_fired(&[]));
}
