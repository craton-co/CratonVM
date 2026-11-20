use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeHeapAccess as _, NativeSystemAccess as _};

/// This test's own VM identity: the tables and the lock-key registry are
/// process-wide and tests run in parallel.
const VM: usize = 0xC29_0001;

fn at(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; the mock hashes the address and the
    // lock-key registry only records it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// Drops the VM's lock keys and zip rows, even when the test fails.
struct Teardown;
impl Drop for Teardown {
    fn drop(&mut self) {
        crate::forget_vm_lock_keys(VM);
        forget_vm_zip_side_tables(VM);
    }
}

/// The key used to be `(VM, identity hash)`, so two live same-hash objects
/// shared every row: two `Deflater`s one zlib stream, two
/// `ZipOutputStream`s one entry buffer, two `InflaterInputStream`s one
/// payload -- and the w17-c owner map, holding one owner per key, let the
/// first one's death drop rows the second still used. Now each object has
/// its own key and rows, a death frees only the dead object's, and a
/// hash-preserving move keeps the survivor's rows reachable from its new
/// address.
#[test]
fn two_live_same_hash_objects_keep_their_own_rows_through_a_death_and_a_move() {
    let _teardown = Teardown;
    let ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let base = 0x1_C290_0100usize;
    let (a, b) = (at(base), at(base + (1usize << 32)));
    assert_eq!(
        ctx.identity_hash_code(a),
        ctx.identity_hash_code(b),
        "premise: the two objects share an identity hash"
    );

    let (ka, kb) = (zo_buf_key(&ctx, a), zo_buf_key(&ctx, b));
    assert_eq!((ka.0, kb.0), (VM, VM));
    assert_ne!(ka, kb, "a live collider gets a key of its own");
    assert_eq!(zo_existing_key(&ctx, a), Some(ka));
    assert_eq!(zo_existing_key(&ctx, b), Some(kb));

    deflater_states()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(ka, DeflaterState::new(-1, false));
    deflater_states()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(kb, DeflaterState::new(-1, true));
    zo_bufs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(ka, b"entry A".to_vec());
    zo_bufs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(kb, b"entry B".to_vec());
    iis_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            ka,
            IisState {
                data: vec![0xA],
                pos: 0,
            },
        );
    iis_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            kb,
            IisState {
                data: vec![0xB],
                pos: 0,
            },
        );
    let nowrap_of = |key: ZoKey| {
        deflater_states()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .map(|s| s.nowrap)
    };
    let buf_of = |key: ZoKey| {
        zo_bufs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
    };
    let payload_of = |key: ZoKey| {
        iis_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .map(|s| s.data.clone())
    };
    assert_eq!(nowrap_of(ka), Some(false), "A's own zlib state");
    assert_eq!(nowrap_of(kb), Some(true), "B's own zlib state");
    assert_eq!(buf_of(ka).as_deref(), Some(&b"entry A"[..]));
    assert_eq!(buf_of(kb).as_deref(), Some(&b"entry B"[..]));
    assert_eq!(payload_of(ka), Some(vec![0xA]));
    assert_eq!(payload_of(kb), Some(vec![0xB]));

    // A dies; B lives. Only A's key is freed, so only A's rows go.
    let a_addr = base;
    assert_eq!(crate::gc_sweep_lock_keys(VM, &|x| x != a_addr), 1);
    assert_eq!(zo_existing_key(&ctx, a), None, "A's key was freed");
    assert_eq!(nowrap_of(ka), None);
    assert_eq!(buf_of(ka), None);
    assert_eq!(payload_of(ka), None);
    assert_eq!(nowrap_of(kb), Some(true), "B's rows survive A's death");
    assert_eq!(buf_of(kb).as_deref(), Some(&b"entry B"[..]));
    assert_eq!(payload_of(kb), Some(vec![0xB]));

    // B moves, keeping its identity hash (the low 32 bits under the
    // mock); the epilogue re-addresses its key.
    let b_from = base + (1usize << 32);
    let b_to = base + (2usize << 32);
    let moved = at(b_to);
    assert_eq!(
        ctx.identity_hash_code(moved),
        ctx.identity_hash_code(b),
        "premise: a hash-preserving move"
    );
    let mut map = cratonvm_types::PointerMap::new();
    map.insert(b_from, b_to);
    assert_eq!(crate::gc_sweep_and_remap_lock_keys(VM, &|_| true, &map), 0);
    assert_eq!(
        zo_existing_key(&ctx, moved),
        Some(kb),
        "the moved object finds its key"
    );
    assert_eq!(
        zo_existing_key(&ctx, b),
        None,
        "the vacated address has none"
    );
    assert_eq!(nowrap_of(kb), Some(true));
    assert_eq!(buf_of(kb).as_deref(), Some(&b"entry B"[..]));
    assert_eq!(payload_of(kb), Some(vec![0xB]));

    // A newcomer at A's vacated address (same hash again) is another
    // object: a fresh key, and none of A's or B's rows.
    let newcomer = zo_buf_key(&ctx, a);
    assert_ne!(newcomer, ka, "a freed key is never minted again");
    assert_ne!(newcomer, kb);
    assert_eq!(nowrap_of(newcomer), None);
    assert_eq!(buf_of(newcomer), None);
    assert_eq!(payload_of(newcomer), None);
}
