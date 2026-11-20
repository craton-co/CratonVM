use super::*;
use std::sync::atomic::AtomicU8;

/// A published body whose one class-init cell is `cell` (a side table its
/// code would read by address), and its buffer start.
fn published_body_with_cell(cell: &Arc<AtomicU8>) -> (CompiledMethod, usize) {
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // RET
    let mut cm = CompiledMethod::new(buf);
    let _ = cm._buffer.mark_published();
    cm._jit_class_init_cells.push(Arc::clone(cell));
    let base = cm.entry_ptr() as usize;
    (cm, base)
}

/// Drop `cm` the way the round-14 cascade does (inside another buffer's
/// reclamation scope) while this thread is inside compiled code, and
/// answer how many owners `cell` has right after.
fn owners_after_a_cascaded_drop(cell: &Arc<AtomicU8>) -> usize {
    let token = jit_execution_enter();
    let (cm, base) = published_body_with_cell(cell);
    {
        let _other = AuthorisedReclaim::enter_for(base.wrapping_add(0x10_0000));
        drop(cm);
    }
    let owners = Arc::strong_count(cell);
    jit_execution_leave(token);
    owners
}

/// The `gcd-d10v` cascade kept the MAPPING of a body a thread may still be
/// inside, and freed every box its code reads by address. Now they wait in
/// the queue with it: the cell is still owned after the drop. Before the
/// fix its count fell to 1 at once.
#[test]
fn a_cascaded_body_keeps_its_side_tables_until_its_own_proof() {
    let cell = Arc::new(AtomicU8::new(0));
    assert_eq!(
        owners_after_a_cascaded_drop(&cell),
        2,
        "a rescued body's side tables must stay alive with its mapping"
    );
}


/// An ordinary authorised release (the queue's own proof covering this
/// buffer) is not a rescue: nothing is kept.
#[test]
fn an_authorised_release_keeps_nothing() {
    let cell = Arc::new(AtomicU8::new(0));
    let (cm, _base) = published_body_with_cell(&cell);
    {
        let _proof = AuthorisedReclaim::enter();
        assert!(!cm._buffer.release_would_be_rescued());
        drop(cm);
    }
    assert_eq!(Arc::strong_count(&cell), 1);
}

/// CODECACHE21-5 (tag cc21): the poll-stress countdown cells are baked
/// into the code as imm64s, so a rescued body must carry them with its
/// mapping like every other box its code reads by address. The move keeps
/// each box's heap address, which is what the code baked.
#[test]
fn cc21_rescued_tables_carry_the_poll_stress_cells() {
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // RET
    let mut cm = CompiledMethod::new(buf);
    cm._poll_stress_cells
        .push(Box::new(std::sync::atomic::AtomicU32::new(7)));
    let baked = &*cm._poll_stress_cells[0] as *const std::sync::atomic::AtomicU32 as usize;
    let tables = RescuedSideTables::take_from(&mut cm);
    assert!(cm._poll_stress_cells.is_empty(), "moved out of the dying body");
    assert_eq!(tables.poll_stress_cells.len(), 1);
    let kept = &*tables.poll_stress_cells[0] as *const std::sync::atomic::AtomicU32 as usize;
    assert_eq!(kept, baked, "the box the code baked did not move");
}
