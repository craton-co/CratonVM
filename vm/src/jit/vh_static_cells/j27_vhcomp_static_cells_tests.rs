use super::*;

const TARGET: VhStaticTarget = VhStaticTarget {
    base_cell: 0x4000,
    field_index: 2,
    kind: b'I',
    class_id: 77,
};

fn filed(cells: &VhStaticCells, key: (u32, u32)) -> Option<(usize, u32, u8)> {
    let addr = cells.cell_addr(key);
    assert_ne!(addr, 0, "{key:?} has a cell");
    // SAFETY: a cell address `cell_addr` answered; the table owns it for
    // its life and never frees one.
    unsafe { &*(addr as *const VhStaticCell) }.filed()
}

#[test]
fn a_registered_field_has_an_unfilled_cell_until_its_handle_is_noted() {
    let cells = VhStaticCells::new();
    assert_eq!(cells.cell_addr((5, 0)), 0, "nothing registered");
    cells.register(&[(5, 0), (5, 1)]);
    cells.register(&[(5, 0)]);
    let addr = cells.cell_addr((5, 0));
    assert!(cells.has_unresolved());
    assert_eq!(filed(&cells, (5, 0)), None);
    // Field (5,0) holds handle 0x100, field (5,1) handle 0x200.
    let handle_of = |key: (u32, u32)| -> Option<usize> { Some(if key.1 == 0 { 0x100 } else { 0x200 }) };
    assert_eq!(cells.settle(0x300, Some(TARGET), &handle_of), 0, "another handle");
    assert_eq!(cells.settle(0x100, Some(TARGET), &handle_of), 1);
    assert_eq!(filed(&cells, (5, 0)), Some((0x4000, 32, b'I')));
    assert_eq!(cells.cell_addr((5, 0)), addr, "the address a compile baked");
    assert_eq!(filed(&cells, (5, 1)), None);
    assert!(cells.has_unresolved());
    // A read-only (or otherwise refused) handle settles its field unfilled.
    assert_eq!(cells.settle(0x200, None, &handle_of), 0);
    assert!(!cells.has_unresolved());
    assert_eq!(filed(&cells, (5, 1)), None);
}

#[test]
fn an_uninitialized_holder_stays_unresolved() {
    let cells = VhStaticCells::new();
    cells.register(&[(9, 4)]);
    assert_eq!(cells.settle(0x100, Some(TARGET), &|_| None), 0);
    assert!(cells.has_unresolved());
    assert_eq!(cells.settle(0x100, Some(TARGET), &|_| Some(0x100)), 1);
}

#[test]
fn an_unload_of_either_class_clears_the_cell() {
    let cells = VhStaticCells::new();
    cells.register(&[(5, 0), (6, 0), (8, 0)]);
    let all = |_: (u32, u32)| -> Option<usize> { Some(0x100) };
    assert_eq!(cells.settle(0x100, Some(TARGET), &all), 3);
    let held = cells.cell_addr((6, 0));
    cells.forget_classes(&|id| id == 5);
    assert_eq!(cells.cell_addr((5, 0)), 0, "the handle's holder unloaded");
    assert_eq!(filed(&cells, (6, 0)), Some((0x4000, 32, b'I')));
    cells.forget_classes(&|id| id == TARGET.class_id);
    assert_eq!(cells.cell_addr((6, 0)), 0, "the static's class unloaded");
    // SAFETY: a retired cell stays allocated for compiled code.
    assert_eq!(unsafe { &*(held as *const VhStaticCell) }.filed(), None);
}
