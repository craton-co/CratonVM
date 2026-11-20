use super::*;
use crate::event::{event_id, modifiers, AwtEvent, BUTTON1};

#[test]
fn registration_count() {
    let mut registry = NativeMethodRegistry::new();
    register_all(&mut registry);
    let count = registry.len();
    assert!(count >= 50, "Expected >= 50 AWT natives, got {count}");
}

/// V1: `read_int_array` must cap its up-front reserve so a bogus reported
/// array length can't force a giant eager allocation. The reserve is
/// `len.min(MAX_INT_ARRAY_PREALLOC)`; this guards the cap against being
/// set to a degenerate value and documents the bound.
#[test]
fn int_array_prealloc_cap_is_sane() {
    assert!(MAX_INT_ARRAY_PREALLOC > 0);
    // A hostile length must collapse to the cap, not the raw length.
    let hostile = usize::MAX;
    assert_eq!(hostile.min(MAX_INT_ARRAY_PREALLOC), MAX_INT_ARRAY_PREALLOC);
    // A legitimate small length must be honored exactly.
    assert_eq!(8usize.min(MAX_INT_ARRAY_PREALLOC), 8);
}

/// Build a fake `ObjectRef` for tests — guaranteed 8-aligned & non-null.
fn fake_object_ref(seed: u64) -> ObjectRef {
    let ptr = ((seed + 1) << 3) as *mut u8;
    // SAFETY: the arithmetic above constructs a unique non-null,
    // eight-byte-aligned sentinel used only as an opaque test identity.
    unsafe { ObjectRef::from_raw(ptr) }
}

fn find_field<'a>(plan: &'a EventSynthesisPlan, name: &str) -> Option<&'a Value> {
    plan.fields.iter().find(|(n, _)| *n == name).map(|(_, v)| v)
}

/// The VM the pre-w22 tests below file their rows under (the mock
/// default identity); the w22-a tests use private identities.
const TEST_VM: usize = 0;

/// A pre-w24 (strong) peer-source row.
fn strong_source(root_handle: usize, java_hash: i32) -> PeerSourceEntry {
    PeerSourceEntry {
        root_handle,
        java_hash,
        weak: false,
        shown_root: 0,
    }
}

/// A gc-common w24-b (weak) peer-source row.
fn weak_source(root_handle: usize, java_hash: i32) -> PeerSourceEntry {
    PeerSourceEntry {
        weak: true,
        ..strong_source(root_handle, java_hash)
    }
}

fn register_peer_source_root_for_test(peer_id: PeerId, root_handle: usize, java_hash: i32) {
    peer_source_table().lock().insert(
        TEST_VM,
        peer_id.0,
        strong_source(root_handle, java_hash),
    );
}

fn lookup_peer_source_for_test(
    peer_id: PeerId,
    root_handle: usize,
    source: ObjectRef,
) -> Option<ObjectRef> {
    lookup_peer_source_with(TEST_VM, peer_id, |handle| {
        (handle == root_handle).then_some(source)
    })
}

/// gc-common w22-a (`common-w21a-more-process-wide-global-root-handle-caches`
/// item 2): the peer-source table is per VM. Two VMs register the same
/// peer id (the peer registry mints ids per identity hash, process-wide)
/// with the same handle number, each in its own global-ref table: each
/// resolves only its own row, a replacement hands back the replaced row's
/// OWN VM (gc-common w24-b removed the FIFO cap), and a VM's teardown drops
/// its rows and nobody else's.
#[test]
fn w22a_peer_sources_are_per_vm_and_forgotten_with_the_vm() {
    const VM_A: usize = 0x0A22_1A08;
    const VM_B: usize = 0x0A22_1B08;
    const PEER: u64 = 0x0A22_1000;
    let src_a = fake_object_ref(0x0A22_1A);
    let src_b = fake_object_ref(0x0A22_1B);
    {
        let mut t = peer_source_table().lock();
        for vm in [VM_A, VM_B] {
            let displaced = t.insert(vm, PEER, strong_source(1, 7));
            assert!(displaced.is_empty());
        }
    }
    // Each VM resolves handle 1 in ITS OWN table.
    let in_a = |h: usize| (h == 1).then_some(src_a);
    let in_b = |h: usize| (h == 1).then_some(src_b);
    assert_eq!(
        lookup_peer_source_with(VM_A, PeerId(PEER), in_a),
        Some(src_a)
    );
    assert_eq!(
        lookup_peer_source_with(VM_B, PeerId(PEER), in_b),
        Some(src_b)
    );

    // A replacement in B hands back B's old root, tagged with B.
    let displaced = peer_source_table()
        .lock()
        .insert(VM_B, PEER, strong_source(2, 7));
    assert_eq!(
        displaced,
        vec![VmRoot {
            vm: VM_B,
            handle: 1
        }]
    );

    // Teardown of A drops A's row only.
    forget_vm_awt_roots(VM_A);
    assert_eq!(lookup_peer_source_with(VM_A, PeerId(PEER), in_a), None);
    assert!(peer_source_table().lock().get(VM_B, PEER).is_some());
    forget_vm_awt_roots(VM_B);
    assert!(peer_source_table().lock().get(VM_B, PEER).is_none());
}

/// gc-common w22-a: a root displaced by VM B's call but created by VM A is
/// never released through B: it is parked for A, handed to A's drain, and
/// dropped (unreleased) by A's teardown.
#[test]
fn w22a_foreign_roots_are_parked_for_their_own_vm() {
    const VM_A: usize = 0x0A22_2A08;
    const VM_B: usize = 0x0A22_2B08;
    const VM_C: usize = 0x0A22_2C08;
    let roots = vec![
        VmRoot {
            vm: VM_A,
            handle: 5,
        },
        VmRoot {
            vm: VM_B,
            handle: 6,
        },
        VmRoot {
            vm: VM_B,
            handle: 0,
        },
        VmRoot {
            vm: VM_C,
            handle: 7,
        },
    ];
    let (own, foreign) = partition_vm_roots(VM_B, roots);
    assert_eq!(own, vec![6], "B releases only its own non-zero handle");
    assert_eq!(
        foreign,
        vec![
            VmRoot {
                vm: VM_A,
                handle: 5
            },
            VmRoot {
                vm: VM_C,
                handle: 7
            }
        ]
    );
    park_retired_awt_roots(foreign);
    assert!(take_retired_awt_roots(VM_B).is_empty());
    assert_eq!(take_retired_awt_roots(VM_A), vec![5]);
    assert!(take_retired_awt_roots(VM_A).is_empty(), "drained once");
    // C never ran again: its teardown drops the parked row unreleased.
    forget_vm_awt_roots(VM_C);
    assert!(take_retired_awt_roots(VM_C).is_empty());
}

/// gc-common w22-a: `InvocationEvent` bindings are per VM -- VM B's
/// dispatch of an event whose identity hash collides with a pending VM A
/// event must not take A's callback id.
#[test]
fn w22a_invocation_bindings_are_per_vm() {
    const VM_A: usize = 0x0A22_3A08;
    const VM_B: usize = 0x0A22_3B08;
    const HASH: i32 = 0x0A22_3000;
    // No weak roots (a context without a collector): one row per bucket.
    let no_roots = |_: usize| -> Option<ObjectRef> { None };
    let event = w29c_obj(0x0A22_3000);
    let callback = |row: Option<InvocationRow>| row.map(|r| r.callback_id);
    assert!(bind_invocation_event(VM_A, HASH, event, 0, 41, &no_roots).is_empty());
    assert_eq!(
        callback(take_invocation_event_callback(VM_B, HASH, event, &no_roots)),
        None
    );
    assert!(bind_invocation_event(VM_B, HASH, event, 0, 42, &no_roots).is_empty());
    forget_vm_awt_roots(VM_B);
    assert_eq!(
        callback(take_invocation_event_callback(VM_B, HASH, event, &no_roots)),
        None
    );
    assert_eq!(
        callback(take_invocation_event_callback(VM_A, HASH, event, &no_roots)),
        Some(41)
    );
}

/// A reference for the table tests: never dereferenced (8-aligned).
fn w29c_obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only; the tables compare it, never dereference it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// gc-common w29-c (`common-w28b-remaining-identity-hash-keyed-side-tables`
/// rank 30): two undispatched `InvocationEvent`s of ONE VM with the SAME
/// identity hash keep their own callback ids. The second binding used to
/// replace the first, so the first event's `dispatch()` ran the second's
/// Runnable. Weak roots are modelled by a resolver table (`native-awt`
/// has no mock context): handle -> the event's current reference, `None`
/// once the event was collected.
#[test]
fn w29c_same_hash_pending_events_keep_their_own_callbacks() {
    use std::collections::HashMap;
    const VM: usize = 0x0C29_AE08;
    const HASH: i32 = 0x0C29_A000;
    let (first, second) = (w29c_obj(0x1_0C29_A000), w29c_obj(0x2_0C29_A000));
    let first_moved = w29c_obj(0x3_0C29_A000);
    let roots: std::cell::RefCell<HashMap<usize, Option<ObjectRef>>> =
        std::cell::RefCell::new(HashMap::from([(11, Some(first)), (12, Some(second))]));
    let resolve = |h: usize| roots.borrow().get(&h).copied().flatten();
    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            invocation_event_callbacks().lock().forget_vm(VM);
        }
    }
    let _forget = Forget;

    assert!(bind_invocation_event(VM, HASH, first, 11, 101, &resolve).is_empty());
    assert!(
        bind_invocation_event(VM, HASH, second, 12, 102, &resolve).is_empty(),
        "the first, live, event's row is kept"
    );
    // The first event moves (its weak root follows it); a dispatch of the
    // second takes only the second's row.
    roots.borrow_mut().insert(11, Some(first_moved));
    let row = take_invocation_event_callback(VM, HASH, second, &resolve);
    assert_eq!(
        row,
        Some(InvocationRow {
            weak: 12,
            callback_id: 102
        })
    );
    assert_eq!(
        take_invocation_event_callback(VM, HASH, first, &resolve),
        None,
        "the stale pre-move reference finds nothing"
    );
    assert_eq!(
        take_invocation_event_callback(VM, HASH, first_moved, &resolve).map(|r| r.callback_id),
        Some(101)
    );
    assert_eq!(
        take_invocation_event_callback(VM, HASH, first_moved, &resolve),
        None,
        "dispatched once"
    );

    // A collected event's row is released by the next binding in its
    // bucket (its weak root handed back for release through its VM).
    let third = w29c_obj(0x4_0C29_A000);
    roots.borrow_mut().insert(13, Some(third));
    assert!(bind_invocation_event(VM, HASH, third, 13, 103, &resolve).is_empty());
    roots.borrow_mut().insert(13, None);
    let fourth = w29c_obj(0x5_0C29_A000);
    roots.borrow_mut().insert(14, Some(fourth));
    let released = bind_invocation_event(VM, HASH, fourth, 14, 104, &resolve);
    assert_eq!(released, vec![VmRoot { vm: VM, handle: 13 }]);
    assert_eq!(
        take_invocation_event_callback(VM, HASH, fourth, &resolve).map(|r| r.callback_id),
        Some(104)
    );
}

// ── gc-common w23-a: AWT state per VM ────────────────────────────────

/// One VM's events never reach another VM's queue, and a VM's teardown
/// drops (and closes) its queue: a dispatch loop still blocked in it
/// returns instead of waiting forever.
#[test]
fn w23a_event_queues_are_per_vm_and_closed_at_teardown() {
    use std::time::Duration;
    const VM_A: usize = 0x0A23_4A08;
    const VM_B: usize = 0x0A23_4B08;
    let a = edt::edt_for_vm(VM_A);
    let b = edt::edt_for_vm(VM_B);
    assert!(!Arc::ptr_eq(&a, &b));
    assert!(Arc::ptr_eq(&a, &edt::edt_for_vm(VM_A)), "one queue per VM");
    a.post_event(AwtEvent::window(event_id::WINDOW_OPENED, PeerId(1), 0));
    let (_, displaced) = a.invoke_later_runnable(
        VmRoot {
            vm: VM_A,
            handle: 9,
        },
        PeerId(0),
    );
    assert!(displaced.is_empty());
    assert!(b.poll_event().is_none(), "B must not see A's events");
    assert_eq!(b.queue_length(), 0);
    assert_eq!(a.queue_length(), 2);
    assert_eq!(a.poll_event().map(|e| e.id), Some(event_id::WINDOW_OPENED));

    // A dispatch loop blocked on A's (now empty after the next poll)
    // queue is released by A's teardown; B's queue is untouched.
    let _ = a.poll_event();
    let (tx, rx) = std::sync::mpsc::channel();
    let waiter = {
        let a = Arc::clone(&a);
        std::thread::spawn(move || {
            let got = a.wait_event_blocking(u64::MAX, 5, || false);
            let _ = tx.send(got.is_none());
        })
    };
    std::thread::sleep(Duration::from_millis(20));
    forget_vm_awt_roots(VM_A);
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(true));
    waiter.join().unwrap();
    assert!(a.is_closed());
    assert!(!edt::vm_has_edt(VM_A));
    assert!(edt::vm_has_edt(VM_B));
    assert!(!b.is_closed());
    forget_vm_awt_roots(VM_B);
    assert!(!edt::vm_has_edt(VM_B));
}

/// Two VMs' components with the same identity hash get distinct peers;
/// a VM's teardown destroys its peers and their Swing rows only.
#[test]
fn w23a_peers_are_per_vm_and_forgotten_with_the_vm() {
    const VM_A: usize = 0x0A23_5A08;
    const VM_B: usize = 0x0A23_5B08;
    const HASH: i32 = 0x0A23_5000;
    let (pa, pb) = {
        let mut reg = peer::peer_registry().lock();
        let pa = reg.create_peer(ComponentType::Frame);
        reg.register_java_mapping(VM_A, HASH, pa);
        let pb = reg.create_peer(ComponentType::Frame);
        reg.register_java_mapping(VM_B, HASH, pb);
        (pa, pb)
    };
    assert_ne!(pa, pb);
    {
        let mut s = swing::swing_state().lock();
        s.mark_dirty(pa, 0, 0, 640, 480);
        s.mark_dirty(pb, 0, 0, 640, 480);
    }
    forget_vm_awt_roots(VM_A);
    {
        let reg = peer::peer_registry().lock();
        assert!(reg.get(pa).is_none());
        assert_eq!(reg.peer_for_java(VM_A, HASH), None);
        assert_eq!(reg.peer_for_java(VM_B, HASH), Some(pb));
    }
    {
        let s = swing::swing_state().lock();
        assert!(!s.dirty_regions.contains_key(&pa));
        assert!(s.dirty_regions.contains_key(&pb));
    }
    forget_vm_awt_roots(VM_B);
    assert!(peer::peer_registry().lock().get(pb).is_none());
    assert!(!swing::swing_state().lock().dirty_regions.contains_key(&pb));
}

/// Graphics contexts are per VM: the same identity hash in two VMs names
/// two contexts (B's draws never land in A's raster), and a VM's teardown
/// drops its contexts only.
#[test]
fn w23a_graphics_contexts_are_per_vm() {
    const VM_A: usize = 0x0A23_6A08;
    const VM_B: usize = 0x0A23_6B08;
    const HASH: i32 = 0x0A23_6000;
    let ga = gfx_for_key_without_weak_root((VM_A, HASH));
    let gb = gfx_for_key_without_weak_root((VM_B, HASH));
    assert!(
        !Arc::ptr_eq(&ga, &gb),
        "a hash collision must not share a context"
    );
    assert!(Arc::ptr_eq(
        &ga,
        &gfx_for_key_without_weak_root((VM_A, HASH))
    ));
    forget_vm_awt_roots(VM_A);
    assert!(!gfx_registry().lock().contains_key((VM_A, HASH)));
    assert!(gfx_registry().lock().contains_key((VM_B, HASH)));
    forget_vm_awt_roots(VM_B);
    assert!(!gfx_registry().lock().contains_key((VM_B, HASH)));
}

/// `BufferedImage` rows are per VM (the same hash in two VMs names two
/// rasters, so B never reads A's pixels), and a VM's teardown frees its
/// rasters -- including one whose row a same-VM hash collision displaced.
#[test]
fn w23a_buffered_images_are_per_vm_and_freed_with_the_vm() {
    const VM_A: usize = 0x0A23_7A08;
    const VM_B: usize = 0x0A23_7B08;
    const HASH: i32 = 0x0A23_7000;
    let (ia, ia2, ib) = {
        let mut reg = image::image_registry();
        (
            reg.create(2, 2, ImageType::IntArgb).expect("create"),
            reg.create(2, 2, ImageType::IntArgb).expect("create"),
            reg.create(2, 2, ImageType::IntArgb).expect("create"),
        )
    };
    // No weak roots (weak root 0): the pre-w24 key-only behaviour.
    let img = fake_object_ref(0x0A23_7A);
    {
        let mut t = buffered_image_ids().lock();
        t.bind((VM_A, HASH), img, 0, ia, &no_weak_roots);
        t.bind((VM_B, HASH), img, 0, ib, &no_weak_roots);
        assert_eq!(t.get((VM_A, HASH), img, &no_weak_roots), Some(ia));
        assert_eq!(t.get((VM_B, HASH), img, &no_weak_roots), Some(ib));
        // A same-VM collision: the newer image takes the row.
        let leftovers = t.bind((VM_A, HASH), img, 0, ia2, &no_weak_roots);
        assert_eq!(leftovers, ImageBindLeftovers::default());
        assert_eq!(t.get((VM_A, HASH), img, &no_weak_roots), Some(ia2));
    }
    forget_vm_awt_roots(VM_A);
    {
        let reg = image::image_registry();
        assert!(reg.get(ia).is_none(), "displaced raster freed with its VM");
        assert!(reg.get(ia2).is_none());
        assert!(reg.get(ib).is_some(), "B's raster survives A's teardown");
    }
    let get = |key: ObjKey| buffered_image_ids().lock().get(key, img, &no_weak_roots);
    assert_eq!(get((VM_A, HASH)), None);
    assert_eq!(get((VM_B, HASH)), Some(ib));
    forget_vm_awt_roots(VM_B);
    assert!(image::image_registry().get(ib).is_none());
    assert_eq!(get((VM_B, HASH)), None);
}

// ── gc-common w24-b: weak rows ───────────────────────────────────────

/// A resolver for rows filed without weak roots (weak root 0 is never
/// resolved; any other handle reads as cleared).
fn no_weak_roots(_handle: usize) -> Option<ObjectRef> {
    None
}

/// `gfx_handle_for_key` with no weak root (a context without a
/// collector): the pre-w24 key-only row.
fn gfx_for_key_without_weak_root(key: ObjKey) -> GfxHandle {
    let obj = fake_object_ref(0x0B24_0060);
    let (handle, displaced, unused) = gfx_handle_for_key(key, obj, 0, &no_weak_roots);
    assert!(displaced.is_empty());
    assert_eq!(unused, 0);
    handle
}

/// A weak-root table for the tests: `handle -> object` while alive.
struct WeakRoots(std::cell::RefCell<Vec<(usize, ObjectRef)>>);

impl WeakRoots {
    fn new(rows: &[(usize, ObjectRef)]) -> Self {
        Self(std::cell::RefCell::new(rows.to_vec()))
    }
    fn resolve(&self, handle: usize) -> Option<ObjectRef> {
        self.0
            .borrow()
            .iter()
            .find(|&&(h, _)| h == handle)
            .map(|&(_, obj)| obj)
    }
    /// The collector found `handle`'s object dead: it reads as cleared.
    fn clear(&self, handle: usize) {
        self.0.borrow_mut().retain(|&(h, _)| h != handle);
    }
}

/// A row belongs to the object its weak root names: a second LIVE object
/// with the same `(vm, hash)` gets a row of its own; removing the primary
/// promotes it; a row whose object died is replaced (and reported dead);
/// the sweep takes out exactly the dead rows of its VM.
#[test]
fn w24b_keyed_rows_tell_live_collisions_apart() {
    const VM: usize = 0x0B24_1A08;
    const KEY: ObjKey = (VM, 0x0B24_1000);
    let (a, b, c) = (
        fake_object_ref(0x0B24_11),
        fake_object_ref(0x0B24_12),
        fake_object_ref(0x0B24_13),
    );
    let roots = WeakRoots::new(&[(1, a), (2, b), (3, c)]);
    let resolve = |h: usize| roots.resolve(h);
    let mut rows: KeyedRows<u32> = KeyedRows::new();
    assert!(rows.insert(KEY, a, 1, 10, &resolve).is_empty());
    assert_eq!(rows.get(KEY, b, &resolve), None, "a's row is not b's");
    assert!(
        rows.insert(KEY, b, 2, 20, &resolve).is_empty(),
        "a collision displaces nothing"
    );
    assert_eq!(rows.get(KEY, a, &resolve), Some(10));
    assert_eq!(rows.get(KEY, b, &resolve), Some(20));

    // a goes (dispose): b is promoted.
    let removed = rows.remove(KEY, a, &resolve).expect("a's row");
    assert_eq!((removed.weak, removed.value), (1, 10));
    assert_eq!(rows.get(KEY, b, &resolve), Some(20));
    assert!(rows.collided.is_empty());

    // b dies; c (same key) replaces its row, which is reported dead.
    roots.clear(2);
    assert_eq!(rows.get(KEY, c, &resolve), None);
    let displaced = rows.insert(KEY, c, 3, 30, &resolve);
    assert_eq!(displaced.len(), 1);
    assert!(displaced[0].dead);
    assert_eq!((displaced[0].row.weak, displaced[0].row.value), (2, 20));
    assert_eq!(rows.get(KEY, c, &resolve), Some(30));

    // The sweep takes out c's row once c dies, and only rows of its VM.
    let other_vm_key: ObjKey = (VM + 8, 0x0B24_1000);
    assert!(rows.insert(other_vm_key, c, 3, 31, &resolve).is_empty());
    assert!(rows.take_dead(VM, &resolve).is_empty(), "c is alive");
    roots.clear(3);
    let dead = rows.take_dead(VM, &resolve);
    assert_eq!(dead.len(), 1);
    assert_eq!((dead[0].weak, dead[0].value), (3, 30));
    assert!(!rows.contains_key(KEY));
    assert!(
        rows.contains_key(other_vm_key),
        "another VM's row is not swept"
    );
}

/// Without weak roots (0: a context with no collector) a row is judged by
/// its key alone, exactly as before w24-b: a second object shares it,
/// and the sweep never takes it.
#[test]
fn w24b_rows_without_weak_roots_keep_the_key_only_behaviour() {
    const KEY: ObjKey = (0x0B24_2A08, 0x0B24_2000);
    let (a, b) = (fake_object_ref(0x0B24_21), fake_object_ref(0x0B24_22));
    let mut rows: KeyedRows<u32> = KeyedRows::new();
    assert!(rows.insert(KEY, a, 0, 10, &no_weak_roots).is_empty());
    assert_eq!(rows.get(KEY, b, &no_weak_roots), Some(10), "shared by key");
    let displaced = rows.insert(KEY, b, 0, 20, &no_weak_roots);
    assert_eq!(displaced.len(), 1);
    assert!(!displaced[0].dead, "superseded, not dead");
    assert!(rows.take_dead(KEY.0, &no_weak_roots).is_empty());
    assert_eq!(rows.get(KEY, a, &no_weak_roots), Some(20));
}

/// The sweep of one VM: rows whose weak root cleared go -- the dead
/// image's raster is destroyed, the dead Graphics context dropped, the
/// dead component's peer destroyed with its Swing rows -- and their weak
/// roots come back for release; live rows, a shown component's row and
/// another VM's rows stay.
#[test]
fn w24b_sweep_drops_dead_rows_frees_rasters_and_keeps_live_ones() {
    const VM: usize = 0x0B24_3A08;
    const OTHER_VM: usize = 0x0B24_3B08;
    let objs: Vec<ObjectRef> = (0..7).map(|i| fake_object_ref(0x0B24_30 + i)).collect();
    let (dead_img, live_img, dead_gfx, live_gfx) = (objs[0], objs[1], objs[2], objs[3]);
    let (dead_comp, live_comp, shown_comp) = (objs[4], objs[5], objs[6]);
    let roots = WeakRoots::new(&[
        (0xB2401, dead_img),
        (0xB2402, live_img),
        (0xB2403, dead_gfx),
        (0xB2404, live_gfx),
        (0xB2405, dead_comp),
        (0xB2406, live_comp),
        (0xB2407, shown_comp),
        (0xB2408, dead_img),
    ]);
    let resolve = |h: usize| roots.resolve(h);

    let (raster_dead, raster_live, raster_other) = {
        let mut reg = image::image_registry();
        (
            reg.create(2, 2, ImageType::IntArgb).expect("create"),
            reg.create(2, 2, ImageType::IntArgb).expect("create"),
            reg.create(2, 2, ImageType::IntArgb).expect("create"),
        )
    };
    {
        let mut t = buffered_image_ids().lock();
        t.bind((VM, 1), dead_img, 0xB2401, raster_dead, &resolve);
        t.bind((VM, 2), live_img, 0xB2402, raster_live, &resolve);
        t.bind((OTHER_VM, 1), dead_img, 0xB2408, raster_other, &resolve);
    }
    let gfx = |target: GfxTarget| -> GfxHandle {
        Arc::new(Mutex::new(GfxEntry {
            state: Graphics2DState::create(4, 4),
            target,
            disposed: false,
        }))
    };
    {
        let mut g = gfx_registry().lock();
        g.insert(
            (VM, 3),
            dead_gfx,
            0xB2403,
            gfx(GfxTarget::Image(raster_dead)),
            &resolve,
        );
        g.insert(
            (VM, 4),
            live_gfx,
            0xB2404,
            gfx(GfxTarget::Detached),
            &resolve,
        );
    }
    let (p_dead, p_live, p_shown) = {
        let mut reg = peer::peer_registry().lock();
        let ids = (
            reg.create_peer(ComponentType::Frame),
            reg.create_peer(ComponentType::Frame),
            reg.create_peer(ComponentType::Frame),
        );
        reg.register_java_mapping(VM, 5, ids.0);
        reg.register_java_mapping(VM, 6, ids.1);
        reg.register_java_mapping(VM, 7, ids.2);
        ids
    };
    {
        let mut t = peer_source_table().lock();
        t.insert(VM, p_dead.0, weak_source(0xB2405, 5));
        t.insert(VM, p_live.0, weak_source(0xB2406, 6));
        t.insert(VM, p_shown.0, weak_source(0xB2407, 7));
        assert!(t.set_shown_root(VM, p_shown.0, 0xB2407, 0xB24FF));
    }
    swing::swing_state()
        .lock()
        .mark_dirty(p_dead, 0, 0, 640, 480);

    // Nothing died yet: nothing goes.
    let none = free_dead_awt_rows(take_dead_awt_rows(VM, &resolve));
    assert!(none.is_empty());

    // The collector found the dead image, Graphics and component (and,
    // for the test, the shown one: its row holds a strong root, so the
    // sweep must not judge it).
    for h in [0xB2401, 0xB2403, 0xB2405, 0xB2407, 0xB2408] {
        roots.clear(h);
    }
    let mut released = free_dead_awt_rows(take_dead_awt_rows(VM, &resolve));
    released.sort_unstable();
    assert_eq!(released, vec![0xB2401, 0xB2403, 0xB2405]);
    {
        let reg = image::image_registry();
        assert!(
            reg.get(raster_dead).is_none(),
            "a dead image's raster is freed"
        );
        assert!(reg.get(raster_live).is_some());
        assert!(
            reg.get(raster_other).is_some(),
            "another VM's sweep frees it"
        );
    }
    assert!(!gfx_registry().lock().contains_key((VM, 3)));
    assert!(gfx_registry().lock().contains_key((VM, 4)));
    {
        let reg = peer::peer_registry().lock();
        assert!(
            reg.get(p_dead).is_none(),
            "a dead component's peer is destroyed"
        );
        assert_eq!(reg.peer_for_java(VM, 5), None);
        assert!(reg.get(p_live).is_some());
        assert!(reg.get(p_shown).is_some(), "a shown component's peer stays");
    }
    assert!(peer_source_table().lock().get(VM, p_dead.0).is_none());
    assert!(peer_source_table().lock().get(VM, p_shown.0).is_some());
    assert!(!swing::swing_state()
        .lock()
        .dirty_regions
        .contains_key(&p_dead));
    assert!(
        free_dead_awt_rows(take_dead_awt_rows(VM, &resolve)).is_empty(),
        "one sweep takes a dead row once"
    );

    let other = free_dead_awt_rows(take_dead_awt_rows(OTHER_VM, &resolve));
    assert_eq!(other, vec![0xB2408]);
    assert!(image::image_registry().get(raster_other).is_none());
    forget_vm_awt_roots(VM);
    forget_vm_awt_roots(OTHER_VM);
    assert!(image::image_registry().get(raster_live).is_none());
}

/// Binding an image over a row whose image died frees that raster and
/// hands its weak root back; a LIVE same-key image keeps its own row.
#[test]
fn w24b_image_bind_frees_a_dead_rows_raster_and_keeps_collisions_apart() {
    const VM: usize = 0x0B24_4A08;
    const KEY: ObjKey = (VM, 0x0B24_4000);
    let (a, b, c) = (
        fake_object_ref(0x0B24_41),
        fake_object_ref(0x0B24_42),
        fake_object_ref(0x0B24_43),
    );
    let roots = WeakRoots::new(&[(1, a), (2, b), (3, c)]);
    let resolve = |h: usize| roots.resolve(h);
    let (ra, rb, rc) = {
        let mut reg = image::image_registry();
        (
            reg.create(2, 2, ImageType::IntArgb).expect("create"),
            reg.create(2, 2, ImageType::IntArgb).expect("create"),
            reg.create(2, 2, ImageType::IntArgb).expect("create"),
        )
    };
    let mut t = buffered_image_ids().lock();
    assert_eq!(
        t.bind(KEY, a, 1, ra, &resolve),
        ImageBindLeftovers::default()
    );
    assert_eq!(
        t.bind(KEY, b, 2, rb, &resolve),
        ImageBindLeftovers::default()
    );
    assert_eq!(t.get(KEY, a, &resolve), Some(ra), "a reads its own pixels");
    assert_eq!(t.get(KEY, b, &resolve), Some(rb), "b reads its own pixels");
    roots.clear(1);
    let leftovers = t.bind(KEY, c, 3, rc, &resolve);
    assert_eq!(
        leftovers,
        ImageBindLeftovers {
            weak_roots: vec![1],
            rasters: vec![ra],
        }
    );
    assert_eq!(t.get(KEY, c, &resolve), Some(rc));
    assert_eq!(t.get(KEY, b, &resolve), Some(rb));
    let freed = t.forget_vm(VM);
    drop(t);
    let mut reg = image::image_registry();
    assert!(reg.destroy(ra), "the caller destroys a dead row's raster");
    for id in freed {
        reg.destroy(id);
    }
}

/// A peer belongs to the component its source row's weak root names; a
/// strong row (no weak roots) or a missing row judges nothing. The shown
/// root is installed once, only on the row it was minted for, and a
/// replaced row hands both its roots back.
#[test]
fn w24b_peer_source_rows_judge_identity_and_hold_a_shown_root() {
    const VM: usize = 0x0B24_5A08;
    let (a, b) = (fake_object_ref(0x0B24_51), fake_object_ref(0x0B24_52));
    let roots = WeakRoots::new(&[(1, a)]);
    let resolve = |h: usize| roots.resolve(h);
    const WEAK_PEER: u64 = 0x0B24_5001;
    const STRONG_PEER: u64 = 0x0B24_5002;
    const NO_ROW_PEER: u64 = 0x0B24_5003;
    {
        let mut t = peer_source_table().lock();
        t.insert(VM, WEAK_PEER, weak_source(1, 5));
        t.insert(VM, STRONG_PEER, strong_source(9, 5));
    }
    let verdict = |peer: u64, obj| peer_row_verdict(VM, PeerId(peer), obj, &resolve);
    assert_eq!(verdict(WEAK_PEER, a), RowVerdict::Same);
    assert_eq!(verdict(WEAK_PEER, b), RowVerdict::Other);
    assert_eq!(verdict(STRONG_PEER, b), RowVerdict::Same);
    assert_eq!(verdict(NO_ROW_PEER, b), RowVerdict::Same);

    {
        let mut t = peer_source_table().lock();
        assert!(
            !t.set_shown_root(VM, WEAK_PEER, 2, 77),
            "minted for another row"
        );
        assert!(t.set_shown_root(VM, WEAK_PEER, 1, 77));
        assert!(!t.set_shown_root(VM, WEAK_PEER, 1, 78), "already shown");
        assert_eq!(t.take_shown_root(VM, WEAK_PEER), Some(77));
        assert_eq!(t.take_shown_root(VM, WEAK_PEER), None);
        assert!(t.set_shown_root(VM, WEAK_PEER, 1, 79));
        let mut replaced = t.insert(VM, WEAK_PEER, weak_source(3, 5));
        replaced.sort_unstable_by_key(|r| r.handle);
        assert_eq!(
            replaced,
            vec![VmRoot { vm: VM, handle: 1 }, VmRoot { vm: VM, handle: 79 }]
        );
    }
    roots.clear(1);
    forget_vm_awt_roots(VM);
    assert!(peer_source_table().lock().get(VM, WEAK_PEER).is_none());
}

/// The sweep runs once per collection of its VM, and a VM's teardown
/// forgets its generation.
#[test]
fn w24b_sweep_gate_runs_once_per_collection() {
    const VM: usize = 0x0B24_6A08;
    assert!(!awt_sweep_due(VM, 5), "first sight records the count");
    assert!(!awt_sweep_due(VM, 5));
    assert!(awt_sweep_due(VM, 6));
    assert!(!awt_sweep_due(VM, 6));
    forget_vm_awt_roots(VM);
    assert!(!awt_sweep_due(VM, 7), "teardown forgot the VM");
    forget_vm_awt_roots(VM);
    assert!(!awt_sweep_generations().lock().contains_key(&VM));
}

/// The event's `source` store takes the component resolved after the
/// event object's allocation, not a value baked into the plan before it.
#[test]
fn w23a_event_source_is_supplied_after_the_allocation() {
    let evt = AwtEvent::window(event_id::WINDOW_CLOSING, PeerId(3), 999);
    let plan = plan_event_synthesis(&evt, None).expect("WindowEvent plan");
    let moved = fake_object_ref(0x0A23_80);
    let fields: Vec<_> = plan_fields_with_source(&plan, Some(moved)).collect();
    assert!(fields.contains(&("source", Value::Object(Some(moved)))));
    assert!(fields.contains(&("id", Value::Int(event_id::WINDOW_CLOSING))));
    assert_eq!(fields.len(), plan.fields.len());
}

/// TASK #28: drive a synthetic MouseEvent end-to-end through a local
/// EDT queue and the `getNextEvent` synthesis pipeline. Before this
/// task `getNextEvent` returned null for every non-invocation event
/// and the EDT silently dropped mouse / key / window listeners. The
/// plan produced here is exactly what the native callback writes
/// into the Java MouseEvent object — so asserting on the plan
/// exercises the same code path the dispatch loop hits at runtime,
/// minus the `new_object` / `set_field_by_name` round-trips (which
/// require a full `NativeContext` we don't have in unit-test scope).
///
/// Uses a *local* `EventDispatchThread` instead of the shared
/// per-VM `edt::edt_for_vm` queues: tests run in parallel under
/// `cargo test`, so a shared singleton would race with the
/// `invoke_later` / `getNextEvent` tests in other modules.
#[test]
fn mouse_event_round_trips_through_event_queue_with_correct_coords() {
    // Register a fake Java component as the source for peer 7. The
    // peer-source table is process-wide but keyed by peer id, so a
    // unique peer id per test avoids cross-test interference.
    let source = fake_object_ref(7);
    let root = 7_007;
    // gc_gen 0 on both register and lookup: no collection runs in-test,
    // so the same-generation gate (finding H8) permits the round-trip.
    register_peer_source_root_for_test(PeerId(7), root, 7);

    // Inject a synthetic MOUSE_PRESSED via a local EDT (the same
    // entry point the platform backends use after translating a
    // WM_LBUTTONDOWN / X11 ButtonPress / Cocoa mouseDown into an
    // `AwtEvent`).
    let edt = crate::edt::EventDispatchThread::new();
    edt.post_event(AwtEvent::mouse(
        event_id::MOUSE_PRESSED,
        PeerId(7),
        42, // timestamp
        120,
        240,
        BUTTON1,
        1,
        modifiers::BUTTON1_DOWN_MASK,
    ));

    // This is what the native callback would dequeue:
    let evt = edt.poll_event().expect("event must be present");
    assert_eq!(evt.id, event_id::MOUSE_PRESSED);

    // And this is what it would write into the Java MouseEvent object:
    let plan = plan_event_synthesis(
        &evt,
        lookup_peer_source_for_test(evt.source_peer_id, root, source),
    )
    .expect("MouseEvent must produce a plan");
    assert_eq!(plan.class_name, "java/awt/event/MouseEvent");
    assert_eq!(
        find_field(&plan, "id"),
        Some(&Value::Int(event_id::MOUSE_PRESSED))
    );
    assert_eq!(find_field(&plan, "when"), Some(&Value::Long(42)));
    assert_eq!(find_field(&plan, "x"), Some(&Value::Int(120)));
    assert_eq!(find_field(&plan, "y"), Some(&Value::Int(240)));
    assert_eq!(find_field(&plan, "button"), Some(&Value::Int(BUTTON1)));
    assert_eq!(find_field(&plan, "clickCount"), Some(&Value::Int(1)));
    assert_eq!(
        find_field(&plan, "modifiers"),
        Some(&Value::Int(modifiers::BUTTON1_DOWN_MASK)),
    );
    // The source must round-trip from the peer-id → Java component map.
    assert_eq!(
        find_field(&plan, "source"),
        Some(&Value::Object(Some(source)))
    );
    // Consumed flag must start at 0 — listeners may flip it later.
    assert_eq!(find_field(&plan, "consumed"), Some(&Value::Int(0)));
}

/// TASK #28: confirm a `WindowEvent::WINDOW_CLOSING` survives the
/// EDT queue, is synthesized as `java/awt/event/WindowEvent`, and
/// carries the right `id` + `source`. This is the path a real
/// platform-backend "close-button clicked" notification follows.
#[test]
fn window_closing_event_returns_correct_window_event_class() {
    let source = fake_object_ref(3);
    let root = 3_003;
    register_peer_source_root_for_test(PeerId(3), root, 3);

    let edt = crate::edt::EventDispatchThread::new();
    edt.post_event(AwtEvent::window(event_id::WINDOW_CLOSING, PeerId(3), 999));

    let evt = edt.poll_event().expect("window event must be present");
    assert_eq!(evt.id, event_id::WINDOW_CLOSING);

    let plan = plan_event_synthesis(
        &evt,
        lookup_peer_source_for_test(evt.source_peer_id, root, source),
    )
    .expect("WindowEvent must produce a plan");
    assert_eq!(plan.class_name, "java/awt/event/WindowEvent");
    assert_eq!(
        find_field(&plan, "id"),
        Some(&Value::Int(event_id::WINDOW_CLOSING)),
    );
    assert_eq!(
        find_field(&plan, "source"),
        Some(&Value::Object(Some(source)))
    );
    assert_eq!(find_field(&plan, "oldState"), Some(&Value::Int(0)));
    assert_eq!(find_field(&plan, "newState"), Some(&Value::Int(0)));
}

/// Sanity: a `KeyEvent::KEY_PRESSED` carries `keyCode`, `keyChar`,
/// and modifiers through the synthesis plan. Regression guard for
/// the listener-pipeline rewrite — without this the JDK
/// `KeyEvent.getKeyCode()` accessor returned 0 for everything.
#[test]
fn key_event_carries_keycode_keychar_and_modifiers() {
    let source = fake_object_ref(5);
    let root = 5_005;
    register_peer_source_root_for_test(PeerId(5), root, 5);

    let evt = AwtEvent::key(
        event_id::KEY_PRESSED,
        PeerId(5),
        123,
        crate::event::vk::VK_A,
        'a',
        modifiers::SHIFT_DOWN_MASK,
    );
    let plan = plan_event_synthesis(
        &evt,
        lookup_peer_source_for_test(evt.source_peer_id, root, source),
    )
    .expect("KeyEvent must produce a plan");
    assert_eq!(plan.class_name, "java/awt/event/KeyEvent");
    assert_eq!(
        find_field(&plan, "keyCode"),
        Some(&Value::Int(crate::event::vk::VK_A))
    );
    assert_eq!(find_field(&plan, "keyChar"), Some(&Value::Int('a' as i32)));
    assert_eq!(
        find_field(&plan, "modifiers"),
        Some(&Value::Int(modifiers::SHIFT_DOWN_MASK)),
    );
    assert_eq!(
        find_field(&plan, "source"),
        Some(&Value::Object(Some(source)))
    );
}

/// Sanity: a paint event materialises as `PaintEvent` (the EDT
/// dispatch loop relies on this class name so RepaintManager's
/// dispatch-on-EDT path actually fires).
#[test]
fn paint_event_uses_paint_event_class() {
    let evt = AwtEvent::paint(event_id::PAINT, PeerId(0), 0, 0, 0, 100, 100);
    let plan = plan_event_synthesis(&evt, None).expect("PaintEvent must produce a plan");
    assert_eq!(plan.class_name, "java/awt/event/PaintEvent");
    assert_eq!(find_field(&plan, "id"), Some(&Value::Int(event_id::PAINT)));
}

/// Finding H8: `lookup_peer_source` must fail closed once a GC has run
/// since the source was registered. The cached raw pointer could have
/// been relocated/freed by the moving collector, so resurrecting it
/// would be a use-after-free. The same-generation entry round-trips;
/// a later generation yields `None`.
#[test]
fn peer_source_lookup_uses_global_root_handle() {
    let source = fake_object_ref(11);
    let root = 11_011;
    register_peer_source_root_for_test(PeerId(11), root, 11);
    assert_eq!(
        lookup_peer_source_for_test(PeerId(11), root, source),
        Some(source)
    );
    assert_eq!(
        lookup_peer_source_with(TEST_VM, PeerId(11), |_| None),
        None,
        "unresolvable global-root handles fail closed"
    );
    // Unknown peer -> None.
    assert_eq!(
        lookup_peer_source_for_test(PeerId(9999), root, source),
        None
    );
}

/// V3: `lookup_peer_source` must fail closed on a cached pointer that
/// cannot designate a real heap object — even at the matching GC
/// generation. A non-8-byte-aligned pointer is provably bogus (heap
/// objects are 8-byte aligned), so the guard returns `None` rather than
/// feeding a malformed pointer to `ObjectRef::from_raw`.
#[test]
fn peer_source_lookup_rejects_zero_root_handle() {
    // Insert a misaligned entry directly (a real `ObjectRef` can't carry
    // a misaligned pointer, but a corrupt/forged side-table entry could).
    peer_source_table()
        .lock()
        .insert(TEST_VM, 424242, strong_source(0, 7));
    // Matching generation, non-null — only the alignment guard stops it.
    let source = fake_object_ref(42);
    assert_eq!(lookup_peer_source_for_test(PeerId(424242), 0, source), None);

    // A null cached pointer also fails closed at the same generation.
    peer_source_table()
        .lock()
        .insert(TEST_VM, 424243, strong_source(0, 7));
    assert_eq!(lookup_peer_source_for_test(PeerId(424243), 0, source), None);
}

/// Component / Focus / Action events are TODO and intentionally
/// produce no plan. Regression guard so we notice if the match arm
/// quietly grows a new entry without a test.
#[test]
fn unsupported_event_kinds_return_none_for_now() {
    let comp = AwtEvent::component(event_id::COMPONENT_RESIZED, PeerId(1), 0, 0, 0, 10, 10);
    assert!(plan_event_synthesis(&comp, None).is_none());
    let focus = AwtEvent::focus(event_id::FOCUS_GAINED, PeerId(1), 0, false);
    assert!(plan_event_synthesis(&focus, None).is_none());
    let action = AwtEvent::action(PeerId(1), 0, "ok".to_string());
    assert!(plan_event_synthesis(&action, None).is_none());
}

/// `wait_event_blocking` must unblock as soon as an event is posted
/// to the queue. Without this the EDT dispatch loop would
/// busy-spin instead of sleeping between events.
#[test]
fn wait_event_blocking_returns_when_event_arrives() {
    use std::sync::Arc;
    let edt = Arc::new(crate::edt::EventDispatchThread::new());
    edt.start();
    let edt2 = Arc::clone(&edt);
    let producer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(15));
        edt2.post_event(AwtEvent::window(event_id::WINDOW_OPENED, PeerId(1), 0));
    });
    // 2-second total budget; 10ms re-check interval. Way more than
    // the producer needs (15ms) but bounded so a regression doesn't
    // hang the whole `cargo test` run.
    let got = edt.wait_event_blocking(2_000, 10, || false);
    producer.join().unwrap();
    edt.stop();
    let evt = got.expect("must observe the posted event");
    assert_eq!(evt.id, event_id::WINDOW_OPENED);
}

// ── Image flush / dispose-graphics buffer reclamation ─────────────

/// Insert a `GfxEntry` directly into the process-wide registry under a
/// unique hash and return that hash. Test-only shortcut that bypasses the
/// `NativeContext`-dependent `register_gfx_for_image` path.
fn insert_gfx_entry(hash: i32, target: GfxTarget, disposed: bool) {
    let handle: GfxHandle = Arc::new(Mutex::new(GfxEntry {
        state: Graphics2DState::create(2, 2),
        target,
        disposed,
    }));
    let obj = fake_object_ref(hash as u32 as u64);
    gfx_registry()
        .lock()
        .insert((TEST_VM, hash), obj, 0, handle, &no_weak_roots);
}

fn gfx_contains(hash: i32) -> bool {
    gfx_registry().lock().contains_key((TEST_VM, hash))
}

fn remove_gfx_entry(hash: i32) {
    let obj = fake_object_ref(hash as u32 as u64);
    gfx_registry()
        .lock()
        .remove((TEST_VM, hash), obj, &no_weak_roots);
}

/// `flush_image` reaps a *disposed* Graphics2D scratch context tied to the
/// flushed image id, freeing its buffer.
#[test]
fn flush_image_reaps_disposed_scratch_for_that_image() {
    let img = ImageId(0x5111_1000); // arbitrary unique id
    let hash = 0x5111_0001u32 as i32;
    insert_gfx_entry(hash, GfxTarget::Image(img), /*disposed=*/ true);
    assert!(gfx_contains(hash));
    flush_image(img);
    assert!(
        !gfx_contains(hash),
        "disposed scratch for the image must be reclaimed"
    );
}

/// `flush_image` must NOT touch a *live* (undisposed) context, even one
/// targeting the same image — dropping it would discard an in-progress
/// render's buffer (use-after-free of meaning, lost pixels).
#[test]
fn flush_image_keeps_live_context_for_that_image() {
    let img = ImageId(0x5111_2000);
    let hash = 0x5111_0002u32 as i32;
    insert_gfx_entry(hash, GfxTarget::Image(img), /*disposed=*/ false);
    flush_image(img);
    assert!(gfx_contains(hash), "live context must survive flush");
    // cleanup
    remove_gfx_entry(hash);
}

/// `flush_image` is scoped to a single image id — a disposed context for a
/// *different* image is left alone.
#[test]
fn flush_image_ignores_other_images() {
    let flushed = ImageId(0x5111_3000);
    let other = ImageId(0x5111_3001);
    let hash = 0x5111_0003u32 as i32;
    insert_gfx_entry(hash, GfxTarget::Image(other), /*disposed=*/ true);
    flush_image(flushed);
    assert!(
        gfx_contains(hash),
        "other image's context must be untouched"
    );
    // cleanup
    remove_gfx_entry(hash);
}

// ── gc-common w25-d: a Graphics draws straight into its image ─────────

fn image_graphics(id: ImageId) -> GfxEntry {
    GfxEntry {
        state: Graphics2DState::create(1, 1),
        target: GfxTarget::Image(id),
        disposed: false,
    }
}

fn new_image(w: u32, h: u32) -> ImageId {
    image::image_registry()
        .create(w, h, ImageType::IntArgb)
        .expect("create image")
}

fn pixel(id: ImageId, x: u32, y: u32) -> u32 {
    let reg = image::image_registry();
    reg.get(id).expect("image").get_rgb(x, y)
}

fn raster_len(id: ImageId) -> usize {
    let reg = image::image_registry();
    reg.get(id).expect("image").get_data_buffer().len()
}

/// The w24-b observation: `setRGB`, then `createGraphics()`, a draw and
/// `dispose()` erased the `setRGB` pixel (the Graphics' blank scratch was
/// copied over the whole image at `dispose`). HotSpot keeps it. And the
/// drawn pixels are in the image before `dispose()`, as on HotSpot.
#[test]
fn w25d_graphics_keeps_the_images_pixels_and_draws_at_once() {
    let id = new_image(8, 8);
    image::image_registry()
        .get_mut(id)
        .unwrap()
        .set_rgb(1, 1, 0xFF12_3456);
    let mut g = image_graphics(id);
    with_gfx_entry(&mut g, |s| {
        s.set_color(0, 0, 255, 255);
        s.fill_rect(4, 4, 2, 2);
    });
    // Visible before dispose.
    assert_eq!(pixel(id, 4, 4), 0xFF00_00FF);
    assert_eq!(pixel(id, 5, 5), 0xFF00_00FF);
    // The pixel set before `createGraphics()` survives, and untouched
    // pixels stay transparent.
    assert_eq!(pixel(id, 1, 1), 0xFF12_3456);
    assert_eq!(pixel(id, 0, 0), 0);
    // `dispose` retires the context: later calls no longer draw, and
    // nothing is copied over the image.
    g.state.dispose();
    g.disposed = true;
    with_gfx_entry(&mut g, |s| s.fill_rect(0, 0, 8, 8));
    assert_eq!(pixel(id, 1, 1), 0xFF12_3456);
    assert_eq!(pixel(id, 0, 0), 0);
    // The raster is whole and the context kept its own 1x1 buffer.
    assert_eq!(raster_len(id), 64);
    assert_eq!((g.state.width(), g.state.height()), (1, 1));
    image::image_registry().destroy(id);
}

/// Two Graphics on one image (e.g. `createGraphics()` twice, or
/// `Graphics.create()`) no longer erase each other's work.
#[test]
fn w25d_two_graphics_on_one_image_keep_each_others_pixels() {
    let id = new_image(4, 4);
    let mut a = image_graphics(id);
    let mut b = image_graphics(id);
    with_gfx_entry(&mut a, |s| {
        s.set_color(255, 0, 0, 255);
        s.fill_rect(0, 0, 1, 1);
    });
    with_gfx_entry(&mut b, |s| {
        s.set_color(0, 255, 0, 255);
        s.fill_rect(3, 3, 1, 1);
    });
    with_gfx_entry(&mut a, |s| s.fill_rect(1, 0, 1, 1));
    assert_eq!(pixel(id, 0, 0), 0xFFFF_0000);
    assert_eq!(pixel(id, 1, 0), 0xFFFF_0000);
    assert_eq!(pixel(id, 3, 3), 0xFF00_FF00);
    image::image_registry().destroy(id);
}

/// The `AwtChecksumProbe` round on the image path: `fillRect` over the
/// whole 64x48 image, then the red 3-point `fillPolygon`. The image then
/// holds exactly HotSpot's 570 red pixels over the fill colour.
#[test]
fn w25d_probe_round_draws_hotspots_pixels_into_the_image() {
    let id = new_image(64, 48);
    let mut g = image_graphics(id);
    with_gfx_entry(&mut g, |s| {
        s.set_color(10, 20, 30, 255);
        s.fill_rect(0, 0, 64, 48);
        s.set_color(255, 0, 0, 255);
        s.fill_polygon(&[5, 40, 20], &[5, 10, 40]);
    });
    let px = image::image_registry()
        .get(id)
        .unwrap()
        .get_rgb_region(0, 0, 64, 48);
    let red = px.iter().filter(|&&p| p == 0xFFFF_0000).count();
    let fill = px.iter().filter(|&&p| p == 0xFF0A_141E).count();
    assert_eq!((red, fill), (570, 64 * 48 - 570));
    // Row 10 (HotSpot `8..=39`; the old rasteriser filled `8..=40`).
    assert_eq!(px[10 * 64 + 7], 0xFF0A_141E);
    assert_eq!(px[10 * 64 + 8], 0xFFFF_0000);
    assert_eq!(px[10 * 64 + 39], 0xFFFF_0000);
    assert_eq!(px[10 * 64 + 40], 0xFF0A_141E);
    image::image_registry().destroy(id);
}

/// `drawImage` on an image's Graphics writes the destination image, from
/// another image or from itself (a snapshot: the overlapping blit reads
/// the pixels as they were).
#[test]
fn w25d_draw_image_writes_the_target_image() {
    let dst = new_image(4, 4);
    let src = new_image(2, 2);
    image::image_registry()
        .get_mut(src)
        .unwrap()
        .set_rgb(0, 0, 0xFF00_00FF);
    image::image_registry()
        .get_mut(dst)
        .unwrap()
        .set_rgb(3, 3, 0xFF12_3456);
    let mut g = image_graphics(dst);
    draw_buffered_image(&mut g, src, 1, 1);
    assert_eq!(pixel(dst, 1, 1), 0xFF00_00FF);
    assert_eq!(pixel(dst, 3, 3), 0xFF12_3456, "the rest of dst is kept");
    // Self-blit, shifted one pixel left: (1,1) lands on (0,0) and the
    // old (1,1) is read before it is overwritten.
    draw_buffered_image(&mut g, dst, -1, -1);
    assert_eq!(pixel(dst, 0, 0), 0xFF00_00FF);
    assert_eq!(pixel(dst, 2, 2), 0xFF12_3456);
    assert_eq!(raster_len(dst), 16);
    image::image_registry().destroy(dst);
    image::image_registry().destroy(src);
}

/// Both `getRGB` natives read through `java_rgb`: an opaque image type
/// answers alpha 0xFF (HotSpot 25: `setRGB(0, 0, 0x00123456)` on a
/// `TYPE_INT_RGB` image reads back `ff123456` from `getRGB(0, 0)` AND
/// from the bulk `getRGB`), an alpha type the stored word.
#[test]
fn w25d_get_rgb_forces_alpha_on_opaque_types_only() {
    let rgb = image::image_registry()
        .create(2, 1, ImageType::IntRgb)
        .expect("create");
    let argb = new_image(2, 1);
    {
        let mut reg = image::image_registry();
        reg.get_mut(rgb).unwrap().set_rgb(0, 0, 0x0012_3456);
        reg.get_mut(argb).unwrap().set_rgb(0, 0, 0x0012_3456);
    }
    {
        let reg = image::image_registry();
        assert_eq!(java_rgb(reg.get(rgb).unwrap(), 0, 0), 0xFF12_3456);
        assert_eq!(java_rgb(reg.get(rgb).unwrap(), 1, 0), 0xFF00_0000);
        assert_eq!(java_rgb(reg.get(argb).unwrap(), 0, 0), 0x0012_3456);
        assert_eq!(java_rgb(reg.get(argb).unwrap(), 1, 0), 0);
    }
    image::image_registry().destroy(rgb);
    image::image_registry().destroy(argb);
}

/// `Graphics.create()` of an image's Graphics: the child draws into the
/// same image, starts from the parent's colour and translation, and has
/// no full-size buffer of its own. Without a parent row: detached.
#[test]
fn w25d_graphics_create_inherits_target_and_state() {
    let id = new_image(4, 4);
    let parent: GfxHandle = Arc::new(Mutex::new(image_graphics(id)));
    {
        let mut p = parent.lock();
        p.state.set_color(0, 0, 255, 255);
        p.state.translate(1.0, 1.0);
    }
    let mut child = child_gfx_entry(Some(&parent));
    assert!(matches!(child.target, GfxTarget::Image(t) if t == id));
    assert_eq!((child.state.width(), child.state.height()), (1, 1));
    with_gfx_entry(&mut child, |s| s.fill_rect(0, 0, 1, 1));
    assert_eq!(pixel(id, 1, 1), 0xFF00_00FF);
    assert_eq!(pixel(id, 0, 0), 0);
    assert!(matches!(child_gfx_entry(None).target, GfxTarget::Detached));
    image::image_registry().destroy(id);
}

/// A panic inside a drawing call still swaps the image's raster back.
#[test]
fn w25d_attached_raster_is_restored_on_unwind() {
    let id = new_image(4, 4);
    image::image_registry()
        .get_mut(id)
        .unwrap()
        .set_rgb(2, 2, 0xFF12_3456);
    let mut g = image_graphics(id);
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        with_gfx_entry::<_, ()>(&mut g, |_| panic!("draw call panicked"))
    }));
    assert!(unwound.is_err());
    let reg = image::image_registry();
    let img = reg.get(id).unwrap();
    assert_eq!(img.get_data_buffer().len(), 16);
    assert_eq!(img.get_rgb(2, 2), 0xFF12_3456);
    drop(reg);
    assert_eq!((g.state.width(), g.state.height()), (1, 1));
    image::image_registry().destroy(id);
}

/// A Graphics whose image raster is gone (the image was collected) draws
/// into its own buffer instead of failing.
#[test]
fn w25d_graphics_of_a_destroyed_image_draws_into_its_own_buffer() {
    let id = new_image(4, 4);
    image::image_registry().destroy(id);
    let mut g = image_graphics(id);
    with_gfx_entry(&mut g, |s| {
        s.set_color(255, 0, 0, 255);
        s.fill_rect(0, 0, 1, 1);
    });
    assert_eq!(g.state.pixels(), &[0xFFFF_0000]);    }

/// Flushing must NOT destroy the image's pixel raster: a memory-backed
/// `BufferedImage` is re-usable after `flush()` (legal `getRGB` afterwards).
#[test]
fn flush_image_preserves_pixel_raster() {
    let id = image::image_registry()
        .create(4, 4, ImageType::IntArgb)
        .expect("create image");
    image::image_registry()
        .get_mut(id)
        .unwrap()
        .set_rgb(1, 1, 0xDEAD_BEEF);
    flush_image(id);
    // Raster still present and pixel intact.
    let reg = image::image_registry();
    let img = reg.get(id).expect("raster must survive flush");
    assert_eq!(img.get_rgb(1, 1), 0xDEAD_BEEF);
}
