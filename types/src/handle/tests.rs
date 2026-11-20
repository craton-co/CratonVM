use super::*;

/// Minimal in-memory `HandleStorage`: an append/tombstone `Vec`, no GC —
/// enough to exercise the root/get/unroot contract in isolation.
#[derive(Default)]
struct TestStorage {
    slots: Vec<Option<ObjectRef>>,
}

fn fake_ref(addr: usize) -> ObjectRef {
    debug_assert!(addr != 0 && addr % 8 == 0);
    // SAFETY: non-null, 8-byte aligned; never dereferenced in tests,
    // only compared/round-tripped by pointer value.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

impl HandleStorage for TestStorage {
    fn root(&mut self, r: ObjectRef) -> u32 {
        self.slots.push(Some(r));
        (self.slots.len() - 1) as u32
    }
    fn unroot(&mut self, slot: u32) {
        if let Some(s) = self.slots.get_mut(slot as usize) {
            *s = None;
        }
    }
    fn get(&self, slot: u32) -> ObjectRef {
        self.slots[slot as usize].expect("read of an unrooted slot")
    }
}

#[test]
fn rooted_handle_reads_through_storage() {
    let mut storage = TestStorage::default();
    let handle = RootedHandle::new(&mut storage, fake_ref(0x1000));
    assert_eq!(handle.get(&storage).as_ptr() as usize, 0x1000);
}

#[test]
fn rooted_handle_sees_a_moved_object() {
    // Simulates what a GC does: root an object, then simulate a moving
    // collection by overwriting the slot in place (exactly what
    // `gc::update_all_roots` does to `native_pin_roots` after a move).
    // The handle sees the new address with no extra bookkeeping on the
    // caller's part — the entire point of reading through the slot
    // instead of caching the pointer.
    let mut storage = TestStorage::default();
    let handle = RootedHandle::new(&mut storage, fake_ref(0x1000));
    storage.slots[handle.slot() as usize] = Some(fake_ref(0x2000));
    assert_eq!(handle.get(&storage).as_ptr() as usize, 0x2000);
}

#[test]
fn from_slot_wraps_an_externally_minted_slot() {
    let mut storage = TestStorage::default();
    let slot = storage.root(fake_ref(0x3000));
    let handle = RootedHandle::from_slot(slot);
    assert_eq!(handle.get(&storage).as_ptr() as usize, 0x3000);
}

#[test]
fn handle_scope_pop_unroots_everything_rooted_through_it() {
    let mut storage = TestStorage::default();
    {
        let mut scope = HandleScope::new(&mut storage);
        let a = scope.root(fake_ref(0x1000));
        let b = scope.root(fake_ref(0x2000));
        assert_eq!(scope.get(&a).as_ptr() as usize, 0x1000);
        assert_eq!(scope.get(&b).as_ptr() as usize, 0x2000);
    } // scope drops here -> both slots unrooted
    assert!(storage.slots.iter().all(|s| s.is_none()));
}

#[test]
fn handle_scope_pop_is_idempotent() {
    let mut storage = TestStorage::default();
    let mut scope = HandleScope::new(&mut storage);
    let _ = scope.root(fake_ref(0x1000));
    scope.pop();
    scope.pop(); // must not panic / double-free a slot
}

#[test]
fn nested_scopes_unroot_independently() {
    let mut storage = TestStorage::default();
    let outer_handle;
    {
        let mut outer = HandleScope::new(&mut storage);
        outer_handle = outer.root(fake_ref(0x1000));
        {
            // Reborrow (not move) `outer.storage` so `outer` stays whole
            // once `inner` drops below.
            let mut inner = HandleScope::new(&mut *outer.storage);
            let inner_handle = inner.root(fake_ref(0x2000));
            assert_eq!(inner.get(&inner_handle).as_ptr() as usize, 0x2000);
        } // inner drops -> only its own slot is released
        assert_eq!(outer.get(&outer_handle).as_ptr() as usize, 0x1000);
    } // outer drops -> its slot is released too
    assert!(storage.slots.iter().all(|s| s.is_none()));
}
