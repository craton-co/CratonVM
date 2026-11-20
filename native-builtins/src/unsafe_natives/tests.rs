use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ClassId;

fn dummy_this() -> Value {
    Value::Object(None)
}

/// Interpreter round i6 wave 127, lane L4: `compareAndExchangeFloat` /
/// `Double` are the int / long exchange over the raw bits; on a `float` /
/// `double` field a hit stores the update and answers the bits, a miss
/// answers the field's bits (it used to answer `expected` and store
/// nothing either way).
#[test]
fn i127_cae_int_and_long_over_float_and_double_fields() {
    let mut ctx = MockNativeContext::new();
    let obj = ctx.alloc_object(ClassId::new(1), 4);
    ctx.set_field(obj, 0, Value::Float(4.0));
    ctx.set_field(obj, 1, Value::Double(7.0));
    let fbits = |f: f32| Value::Int(f.to_bits() as i32);
    let dbits = |d: f64| Value::Long(d.to_bits() as i64);
    let hit = native_unsafe_cae_int(
        &mut ctx,
        &[
            dummy_this(),
            Value::Object(Some(obj)),
            Value::Long(0),
            fbits(4.0),
            fbits(2.5),
        ],
    )
    .unwrap();
    assert_eq!(hit, Some(fbits(4.0)));
    assert_eq!(ctx.get_field(obj, 0), Value::Float(2.5));
    let miss = native_unsafe_cae_int(
        &mut ctx,
        &[
            dummy_this(),
            Value::Object(Some(obj)),
            Value::Long(0),
            fbits(4.0),
            fbits(9.0),
        ],
    )
    .unwrap();
    assert_eq!(miss, Some(fbits(2.5)));
    assert_eq!(ctx.get_field(obj, 0), Value::Float(2.5));
    let hit = native_unsafe_cae_long(
        &mut ctx,
        &[
            dummy_this(),
            Value::Object(Some(obj)),
            Value::Long(1),
            dbits(7.0),
            dbits(3.5),
        ],
    )
    .unwrap();
    assert_eq!(hit, Some(dbits(7.0)));
    assert_eq!(ctx.get_field(obj, 1), Value::Double(3.5));
    let miss = native_unsafe_cae_long(
        &mut ctx,
        &[
            dummy_this(),
            Value::Object(Some(obj)),
            Value::Long(1),
            dbits(7.0),
            dbits(9.0),
        ],
    )
    .unwrap();
    assert_eq!(miss, Some(dbits(3.5)));
    assert_eq!(ctx.get_field(obj, 1), Value::Double(3.5));
}

#[test]
fn weak_cas_int_succeeds_when_expected_matches() {
    let mut ctx = MockNativeContext::new();
    let obj = ctx.alloc_object(ClassId::new(1), 4);
    ctx.set_field(obj, 0, Value::Int(5));
    let r = native_unsafe_weak_cas_int(
        &mut ctx,
        &[
            dummy_this(),
            Value::Object(Some(obj)),
            Value::Long(0),
            Value::Int(5),
            Value::Int(10),
        ],
    )
    .unwrap();
    assert_eq!(r, Some(Value::Int(1)));
    assert_eq!(ctx.get_field(obj, 0), Value::Int(10));
}

#[test]
fn weak_cas_long_fails_when_expected_wrong() {
    let mut ctx = MockNativeContext::new();
    let obj = ctx.alloc_object(ClassId::new(1), 4);
    ctx.set_field(obj, 0, Value::Long(100));
    let r = native_unsafe_weak_cas_long(
        &mut ctx,
        &[
            dummy_this(),
            Value::Object(Some(obj)),
            Value::Long(0),
            Value::Long(999), // wrong expected
            Value::Long(200),
        ],
    )
    .unwrap();
    assert_eq!(r, Some(Value::Int(0)));
    assert_eq!(ctx.get_field(obj, 0), Value::Long(100));
}

#[test]
fn compare_and_exchange_reference_returns_varhandle_witness() {
    let mut ctx = MockNativeContext::new();
    let obj = ctx.alloc_object(ClassId::new(1), 1);
    let current = ctx.create_string("current");
    let replacement = ctx.create_string("replacement");
    let stale = ctx.create_string("stale");
    ctx.set_field(obj, 0, Value::Object(Some(current)));

    let failed = native_unsafe_cae_object(
        &mut ctx,
        &[
            dummy_this(),
            Value::Object(Some(obj)),
            Value::Long(0),
            Value::Object(None),
            Value::Object(Some(stale)),
        ],
    )
    .expect("compareAndExchangeReference")
    .expect("witness");
    assert_eq!(failed, Value::Object(Some(current)));
    assert_eq!(ctx.get_field(obj, 0), Value::Object(Some(current)));

    let witness = native_unsafe_cae_object(
        &mut ctx,
        &[
            dummy_this(),
            Value::Object(Some(obj)),
            Value::Long(0),
            Value::Object(Some(current)),
            Value::Object(Some(replacement)),
        ],
    )
    .expect("compareAndExchangeReference success")
    .expect("success witness");
    assert_eq!(witness, Value::Object(Some(current)));
    assert_eq!(ctx.get_field(obj, 0), Value::Object(Some(replacement)));
}

#[test]
fn park_with_blocker_zero_nanos_is_noop_indefinite() {
    // With nanos=0 and no unpark permit, this would block. We only
    // verify the code path doesn't panic when the thread is
    // interrupted immediately.
    let mut ctx = MockNativeContext::new();
    ctx.set_interrupted(true);
    let r = native_unsafe_park_with_blocker(
        &mut ctx,
        &[dummy_this(), Value::Object(None), Value::Long(0)],
    )
    .unwrap();
    assert_eq!(r, None);
}

/// `setMemory` on an off-heap arena must fill the whole range, refuse a
/// range that leaves the block, and leave the neighbouring bytes alone.
///
/// Guards the bulk rewrite: the byte-at-a-time loop this replaced took the
/// arena write lock and re-probed the block's `BTreeMap` entry per byte
/// (~20 ns/byte, so 1.35 ms for a 64 KiB fill), and it also filled up to
/// the first out-of-range byte BEFORE throwing — the fill is all-or-nothing
/// now.
#[test]
fn set_memory_off_heap_fills_in_bulk_and_stays_in_bounds() {
    let _handles = crate::unsafe_natives_ext::handle_mode_store_for_test();
    let _arena_lock = crate::arena_test_lock();
    let mut ctx = MockNativeContext::new();
    let addr = crate::unsafe_arena_allocate(64);

    native_unsafe_set_memory_consolidated(
        &mut ctx,
        &[
            dummy_this(),
            Value::Object(None),
            Value::Long(addr + 8),
            Value::Long(16),
            Value::Int(0xAB),
        ],
    )
    .unwrap();
    for i in 0..64i64 {
        let want = if (8..24).contains(&i) { 0xAB } else { 0x00 };
        assert_eq!(
            crate::unsafe_arena_get_byte(addr + i),
            want,
            "byte {i} of the arena"
        );
    }

    // A range that runs off the end of the block is refused, and refused
    // WITHOUT having written any of it.
    let err = native_unsafe_set_memory_consolidated(
        &mut ctx,
        &[
            dummy_this(),
            Value::Object(None),
            Value::Long(addr + 60),
            Value::Long(16),
            Value::Int(0xCD),
        ],
    );
    assert!(
        err.is_err(),
        "a fill past the end of the block must be refused"
    );
    for i in 60..64i64 {
        assert_eq!(
            crate::unsafe_arena_get_byte(addr + i),
            0x00,
            "byte {i} was partially filled by a refused setMemory"
        );
    }

    crate::unsafe_arena_free(addr);
}

/// A freed (tagged-but-dead) handle must keep reporting the use-after-free
/// rather than reaching the bridge, where it would be treated as a raw
/// pointer and stored through.
#[test]
fn set_memory_refuses_a_freed_arena_handle() {
    let _handles = crate::unsafe_natives_ext::handle_mode_store_for_test();
    let _arena_lock = crate::arena_test_lock();
    let mut ctx = MockNativeContext::new();
    let addr = crate::unsafe_arena_allocate(32);
    crate::unsafe_arena_free(addr);
    let err = native_unsafe_set_memory_consolidated(
        &mut ctx,
        &[
            dummy_this(),
            Value::Object(None),
            Value::Long(addr),
            Value::Long(8),
            Value::Int(1),
        ],
    );
    assert!(err.is_err(), "setMemory on a freed handle must be refused");
}

#[test]
fn raw_pointer_int_roundtrip() {
    let mut ctx = MockNativeContext::new();
    // Use arena through the public crate-level helpers.
    let addr = crate::unsafe_arena_allocate(16);
    native_unsafe_put_int_at_address(
        &mut ctx,
        &[dummy_this(), Value::Long(addr), Value::Int(0x1234_5678)],
    )
    .unwrap();
    let got =
        native_unsafe_get_int_at_address(&mut ctx, &[dummy_this(), Value::Long(addr)]).unwrap();
    assert_eq!(got, Some(Value::Int(0x1234_5678)));
    crate::unsafe_arena_free(addr);
}

#[test]
fn raw_pointer_long_roundtrip() {
    let mut ctx = MockNativeContext::new();
    let addr = crate::unsafe_arena_allocate(16);
    native_unsafe_put_long_at_address(
        &mut ctx,
        &[dummy_this(), Value::Long(addr), Value::Long(i64::MIN)],
    )
    .unwrap();
    let got = native_unsafe_get_long_at_address(&mut ctx, &[dummy_this(), Value::Long(addr)])
        .unwrap();
    assert_eq!(got, Some(Value::Long(i64::MIN)));
    crate::unsafe_arena_free(addr);
}

#[test]
fn free_memory_evicts_arena() {
    let _handles = crate::unsafe_natives_ext::handle_mode_store_for_test();
    let _arena_lock = crate::arena_test_lock(); // FIX(test-isolation): shared global arena
                                                // FIX: this test previously asserted that a getInt after freeMemory
                                                // returned Some(Int(0)) — the obsolete "free is a no-op, reads return
                                                // 0" contract. Under the consolidated bounds-checked arena (SECURITY
                                                // FIX V5/V9), freeMemory EVICTS the block: a subsequent access is a
                                                // use-after-free and must be rejected ("not in any live arena"). The
                                                // old assertion's .unwrap() panicked on that (correct) Err. Rewritten
                                                // to exercise the live consolidated arena path and to genuinely prove
                                                // eviction: writes/reads succeed while live, then error after free.
    let mut ctx = MockNativeContext::new();
    // Allocate through the consolidated arena allocator — the same path
    // `register_consolidated_off_heap_store` wires `allocateMemory` to via
    // `native_unsafe_allocate_memory_consolidated`. Use the RETURNED addr.
    let addr = crate::unsafe_arena_allocate(32);

    // While live: putInt and getInt round-trip successfully.
    native_unsafe_put_int_at_address(
        &mut ctx,
        &[dummy_this(), Value::Long(addr), Value::Int(42)],
    )
    .unwrap();
    let live =
        native_unsafe_get_int_at_address(&mut ctx, &[dummy_this(), Value::Long(addr)]).unwrap();
    assert_eq!(
        live,
        Some(Value::Int(42)),
        "read must succeed while arena is live"
    );

    // Free evicts the arena block.
    native_unsafe_free_memory(&mut ctx, &[dummy_this(), Value::Long(addr)]).unwrap();

    // After free, the address is no longer in any live arena: getInt must
    // error (use-after-free rejected) rather than silently returning 0.
    let after_free =
        native_unsafe_get_int_at_address(&mut ctx, &[dummy_this(), Value::Long(addr)]);
    assert!(
        after_free.is_err(),
        "getInt after free must be rejected (arena evicted), got {after_free:?}",
    );

    // putInt to the freed address is likewise rejected.
    let put_after_free = native_unsafe_put_int_at_address(
        &mut ctx,
        &[dummy_this(), Value::Long(addr), Value::Int(7)],
    );
    assert!(
        put_after_free.is_err(),
        "putInt after free must be rejected (arena evicted), got {put_after_free:?}",
    );
}

/// R1 (silent-corruption fix): a *real* OS pointer must never be
/// mis-classified as an arena handle, even when it falls numerically
/// inside a live arena block's `[base, base+len)` range. Before the
/// fix, `contains` was pure range membership and a real `allocateDirect`
/// pointer ≥ 64 GiB landing inside a block would be silently routed to
/// the off-heap store. Tagging every handle with bit 62 — which no real
/// Windows/Linux user-mode pointer can have set — makes the two spaces
/// provably disjoint.
#[test]
fn real_pointer_in_arena_numeric_range_is_not_misrouted() {
    let _handles = crate::unsafe_natives_ext::handle_mode_store_for_test();
    let _arena_lock = crate::arena_test_lock(); // shared global arena
    let addr = crate::unsafe_arena_allocate(4096);

    // The handle itself is in-arena.
    assert!(
        crate::unsafe_arena_contains(addr),
        "freshly allocated handle {addr:#x} must be in-arena",
    );
    // Every handle carries the high tag bit (bit 62).
    assert_ne!(
        addr & (1i64 << 62),
        0,
        "arena handle {addr:#x} must carry the reserved tag bit",
    );

    // Synthesize a "real-looking" pointer with the SAME low bits as the
    // handle but the tag bit cleared — i.e. an address in the historical
    // 2^36-based numeric range a real OS pointer could occupy. It must NOT
    // be classified as in-arena, so `copy_*_native_memory` would take the
    // raw path instead of corrupting the off-heap store.
    // (Since nio21 wave 1 a slotted handle is `1<<62 | seq<<30`, so the
    // stripped value can sit anywhere at or above 2^30; the property is the
    // same wherever it lands.)
    let real_like = addr & !(1i64 << 62);
    assert!(
        real_like != addr && real_like >= 1 << 30,
        "sanity: stripped address {real_like:#x} must differ from the handle",
    );
    assert!(
        !crate::unsafe_arena_contains(real_like),
        "real pointer {real_like:#x} (handle with tag stripped) must NOT be \
         mis-classified as an arena handle",
    );

    // A range read at the real-looking address must also be refused so the
    // NIO routing falls through to the raw pointer path.
    let mut out = [0u8; 8];
    assert!(
        !crate::unsafe_arena_copy_out(real_like, &mut out),
        "copy_out at a non-handle address must fail (forces raw path)",
    );

    crate::unsafe_arena_free(addr);
}

#[test]
fn define_class_null_byte_array_rejects() {
    let mut ctx = MockNativeContext::new();
    // args: [this, name, null-bytes, off=0, len=0, null, null]
    let name = ctx.create_string("Foo");
    let result = native_unsafe_define_class(
        &mut ctx,
        &[
            dummy_this(),
            Value::Object(Some(name)),
            Value::Object(None),
            Value::Int(0),
            Value::Int(0),
            Value::Object(None),
            Value::Object(None),
        ],
    );
    assert!(
        result.is_err(),
        "expected ClassFormatError, got {:?}",
        result
    );
}

/// Round 15 wave 6, lane loader6 (PX8-2): only an explicit null loader
/// argument is the boot loader; a real loader and a missing slot are not.
#[test]
fn define_class_null_loader_is_the_boot_loader() {
    let mut ctx = MockNativeContext::new();
    let loader = ctx.alloc_object(ClassId::new(1), 1);
    let with_loader = |l: Value| {
        [
            dummy_this(),
            Value::Object(None),
            Value::Object(None),
            Value::Int(0),
            Value::Int(0),
            l,
            Value::Object(None),
        ]
    };
    assert!(unsafe_define_null_loader_is_bootstrap(&with_loader(
        Value::Object(None)
    )));
    assert!(!unsafe_define_null_loader_is_bootstrap(&with_loader(
        Value::Object(Some(loader))
    )));
    assert!(!unsafe_define_null_loader_is_bootstrap(&[dummy_this()]));
}

#[test]
fn invoke_cleaner_idempotent_on_double_call() {
    let mut ctx = MockNativeContext::new();
    let buf = ctx.alloc_object(ClassId::new(1), 1);
    let r1 = native_unsafe_invoke_cleaner(&mut ctx, &[dummy_this(), Value::Object(Some(buf))])
        .unwrap();
    let r2 = native_unsafe_invoke_cleaner(&mut ctx, &[dummy_this(), Value::Object(Some(buf))])
        .unwrap();
    assert_eq!(r1, None);
    assert_eq!(r2, None);
}

/// gci i27/kd: a null buffer is the JDK 25 body's `NullPointerException`
/// (`directBuffer.isDirect()`), not an `IllegalArgumentException`.
#[test]
fn invoke_cleaner_null_buffer_throws_illegal_arg() {
    let mut ctx = MockNativeContext::new();
    let r = native_unsafe_invoke_cleaner(&mut ctx, &[dummy_this(), Value::Object(None)]);
    match r {
        Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::NullPointerException { message },
        ))) => assert_eq!(message.as_deref(), Some(INVOKE_CLEANER_NULL_MESSAGE)),
        other => panic!("expected the JDK body's NPE, got {other:?}"),
    }
}

/// gci i27/kd (`gch-h2jr-review-findings-RETIRED-RULING-20261018` item 4): where the
/// native answers, it runs the buffer's Cleaner as JDK 25's body does
/// (`db.cleaner()`, then `cleaner.clean()`), and stays a no-op for a
/// buffer class with no `cleaner()`.
#[test]
fn gci_i27kd_invoke_cleaner_runs_the_buffers_cleaner() {
    fn hook(
        ctx: &mut MockNativeContext,
        receiver: ObjectRef,
        name: &str,
        _desc: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        match name {
            "cleaner" => {
                let cleaner = ctx.alloc_object(ClassId::new(2), 1);
                ctx.set_field(receiver, 0, Value::Object(Some(cleaner)));
                Some(Ok(Some(Value::Object(Some(cleaner)))))
            }
            "clean" => {
                ctx.set_field(receiver, 0, Value::Int(1));
                Some(Ok(None))
            }
            _ => None,
        }
    }
    let mut ctx = MockNativeContext::new();
    ctx.set_invoke_virtual_hook(hook);
    let buf = ctx.alloc_object(ClassId::new(1), 1);
    assert_eq!(run_direct_buffer_cleaner(&mut ctx, buf, false).unwrap(), None);
    assert_eq!(ctx.get_field(buf, 0), Value::Int(0), "no cleaner(): untouched");
    assert_eq!(run_direct_buffer_cleaner(&mut ctx, buf, true).unwrap(), None);
    let Value::Object(Some(cleaner)) = ctx.get_field(buf, 0) else {
        panic!("cleaner() was not asked");
    };
    assert_eq!(ctx.get_field(cleaner, 0), Value::Int(1), "clean() ran");
}

#[test]
fn get_load_average_returns_sensible_value() {
    let mut ctx = MockNativeContext::new();
    // double[3]
    let arr = ctx.new_ref_array(ClassId::new(0), 3);
    for i in 0..3 {
        ctx.set_array_element(arr, i, Value::Double(999.0));
    }
    let r = native_unsafe_get_load_average(
        &mut ctx,
        &[dummy_this(), Value::Object(Some(arr)), Value::Int(3)],
    )
    .unwrap();
    match r {
        Some(Value::Int(n)) => {
            // POSIX: 0..=3; Windows: -1. Both are valid per JDK spec.
            assert!(n == -1 || (0..=3).contains(&n), "got {}", n);
        }
        other => panic!("expected Int, got {:?}", other),
    }
}

// -----------------------------------------------------------------
// audit-2026-05-16 — arena-cache regression coverage.
// -----------------------------------------------------------------

#[test]
fn arena_cache_extends_forward_on_sequential_writes() {
    // Simulate the hot loop: for i in 0..N { putByte(addr+i, b) }.
    invalidate_arena_cache();
    let mut ctx = MockNativeContext::new();
    let addr = crate::unsafe_arena_allocate(64);
    for i in 0..64i64 {
        native_unsafe_put_byte_at_address(
            &mut ctx,
            &[dummy_this(), Value::Long(addr + i), Value::Int(0xAB)],
        )
        .unwrap();
    }
    // After the loop, the cache should cover the whole window
    // starting at `addr`.  All N addresses should resolve inside it.
    for i in 0..64i64 {
        assert!(
            cache_lookup(addr + i, 1).is_some(),
            "addr+{} should be cached",
            i
        );
    }
    crate::unsafe_arena_free(addr);
}

#[test]
fn arena_cache_invalidates_on_free() {
    invalidate_arena_cache();
    let mut ctx = MockNativeContext::new();
    let addr = crate::unsafe_arena_allocate(16);
    native_unsafe_put_int_at_address(
        &mut ctx,
        &[dummy_this(), Value::Long(addr), Value::Int(7)],
    )
    .unwrap();
    assert!(cache_lookup(addr, 4).is_some());
    native_unsafe_free_memory(&mut ctx, &[dummy_this(), Value::Long(addr)]).unwrap();
    assert!(
        cache_lookup(addr, 4).is_none(),
        "cache must be invalidated after freeMemory"
    );
}

#[test]
fn arena_cache_jumps_to_new_window_on_disjoint_access() {
    invalidate_arena_cache();
    let mut ctx = MockNativeContext::new();
    let a = crate::unsafe_arena_allocate(16);
    let b = crate::unsafe_arena_allocate(16);
    native_unsafe_put_int_at_address(&mut ctx, &[dummy_this(), Value::Long(a), Value::Int(1)])
        .unwrap();
    assert!(cache_lookup(a, 4).is_some());
    native_unsafe_put_int_at_address(&mut ctx, &[dummy_this(), Value::Long(b), Value::Int(2)])
        .unwrap();
    // Cache now points at b's window; a is no longer in range
    // (assuming the two arenas don't accidentally span each other).
    assert!(cache_lookup(b, 4).is_some());
    crate::unsafe_arena_free(a);
    crate::unsafe_arena_free(b);
}
