use super::*;

/// gc-common w9-b (`handoff-w9b-server-ports-per-vm.md`): a row is
/// remapped and forgotten only by the VM that bound it. Since gc-common
/// w18-f no row is a root, for either VM.
#[test]
fn a_bound_row_belongs_to_its_vm() {
    const VM_A: usize = 0x9B50_0A01;
    const VM_B: usize = 0x9B50_0A02;
    let addr = 0x9B50_1000usize;
    // SAFETY: never dereferenced; only the address is compared.
    let obj = unsafe { ObjectRef::from_raw(addr as *mut u8) };
    record_addr(VM_A, 0x9B51, obj, "127.0.0.1", 1111);
    record_addr(VM_B, 0x9B52, obj, "127.0.0.1", 2222);
    let scan = |vm: usize| {
        let mut out = Vec::new();
        gc_scan_roots(vm, &mut out);
        out.iter().filter(|o| o.as_ptr() as usize == addr).count()
    };
    assert_eq!((scan(VM_A), scan(VM_B)), (0, 0), "the rows are weak");

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(addr, addr + 0x100);
    gc_update_after_gc(VM_B, &map);
    assert_eq!(
        get(0x9B51, obj),
        Some(1111),
        "B's map must not move A's row"
    );
    assert_eq!(get(0x9B52, obj), None, "B's row moved");

    forget_vm(VM_A);
    assert_eq!(get(0x9B51, obj), None);
    forget_vm(VM_B);
    assert_eq!(
        get(0x9B52, unsafe {
            ObjectRef::from_raw((addr + 0x100) as *mut u8)
        }),
        None
    );
}

/// Sweep until `want` rows of `vm` have gone. A parallel test's write
/// between a sweep's collection and its drop defers that drop to the next
/// sweep (the [`WRITE_EPOCH`] check), so one sweep is not guaranteed to
/// finish. Bounded.
fn sweep_expecting(
    vm: usize,
    map: &cratonvm_types::PointerMap,
    is_live: &dyn Fn(usize) -> bool,
    want: usize,
) -> usize {
    let mut gone = 0;
    for _ in 0..256 {
        gone += gc_sweep_rows(vm, map, is_live);
        if gone >= want {
            break;
        }
    }
    gone
}

/// gc-common w18-f (`common-w17a-dropped-open-server-socket-keeps-its-listener-and-row`):
/// a dead receiver's row goes, a live one's stays, and another VM's
/// collection judges none of this VM's rows.
#[test]
fn a_dead_receivers_row_goes_and_a_live_ones_stays() {
    const VM: usize = 0x18F0_0A01;
    const OTHER: usize = 0x18F0_0A02;
    let dead_addr = 0x18F0_1000usize;
    let live_addr = 0x18F0_2000usize;
    // SAFETY: never dereferenced; only the addresses are compared.
    let dead = unsafe { ObjectRef::from_raw(dead_addr as *mut u8) };
    let live = unsafe { ObjectRef::from_raw(live_addr as *mut u8) };
    record_addr(VM, 0x18F1, dead, "0.0.0.0", 4001);
    record_addr(VM, 0x18F2, live, "0.0.0.0", 4002);
    let map = cratonvm_types::PointerMap::default();
    let only_live = move |a: usize| a == live_addr;

    // Another VM's collection sees none of these rows.
    assert_eq!(gc_sweep_rows(OTHER, &map, &|_| false), 0);
    assert_eq!(get(0x18F1, dead), Some(4001));

    assert_eq!(sweep_expecting(VM, &map, &only_live, 1), 1);
    assert_eq!(get(0x18F1, dead), None, "the dead receiver's row went");
    assert_eq!(
        get(0x18F2, live),
        Some(4002),
        "the live receiver's row stayed"
    );
    forget_vm(VM);
}

/// A receiver the collection MOVED is not judged at its vacated address:
/// its row is kept for the remap, which then follows it.
#[test]
fn a_moved_receiver_is_kept_and_followed() {
    const VM: usize = 0x18F0_0A03;
    let from = 0x18F0_3000usize;
    let to = 0x18F0_4000usize;
    // SAFETY: never dereferenced; only the addresses are compared.
    let obj = unsafe { ObjectRef::from_raw(from as *mut u8) };
    let moved = unsafe { ObjectRef::from_raw(to as *mut u8) };
    record_addr(VM, 0x18F3, obj, "127.0.0.1", 4003);
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(from, to);
    // Nothing is live at the OLD address; the row must survive anyway.
    assert_eq!(gc_sweep_rows(VM, &map, &|_| false), 0);
    gc_update_after_gc(VM, &map);
    assert_eq!(get(0x18F3, moved), Some(4003));
    assert_eq!(get(0x18F3, obj), None);
    forget_vm(VM);
}
