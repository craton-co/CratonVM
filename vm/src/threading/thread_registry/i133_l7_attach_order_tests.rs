//! Interpreter round i6 wave 133, lane L7 (`CRATONVM_THREADS_ATTACH_ORDER`):
//! the attach-ordered listing behind `enumerate_threads` is `ThreadId`
//! order whatever the map's hash order, drops a dead thread without
//! reordering the rest, and the gate defaults on.
use super::*;

fn fake(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; an address only.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

#[test]
fn i133_l7_live_threads_list_in_attach_order() {
    let registry = ThreadRegistry::new();
    // Registered out of id order, and enough of them that the hash
    // order is not the id order by accident.
    let ids: [u64; 12] = [7, 0, 11, 3, 9, 1, 5, 10, 2, 8, 4, 6];
    for id in ids {
        let addr = 0x0133_0000 + (id as usize) * 0x40;
        registry.register(ThreadId(id), &format!("t{id}"), Some(fake(addr)));
    }
    registry.mark_dead(ThreadId(5));
    let got: Vec<usize> = registry
        .alive_thread_objects_in_id_order()
        .iter()
        .map(|r| r.as_ptr() as usize)
        .collect();
    let want: Vec<usize> = (0u64..12)
        .filter(|&id| id != 5)
        .map(|id| 0x0133_0000 + (id as usize) * 0x40)
        .collect();
    assert_eq!(got, want);
    // The hash-order listing holds the same set.
    let mut unordered: Vec<usize> = registry
        .alive_thread_objects(usize::MAX)
        .iter()
        .map(|r| r.as_ptr() as usize)
        .collect();
    unordered.sort_unstable();
    assert_eq!(unordered, want);
}

