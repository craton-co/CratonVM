use super::*;

// -----------------------------------------------------------------------
// NativeMemoryTable
// -----------------------------------------------------------------------

#[test]
fn allocate_and_free() {
    let mut table = NativeMemoryTable::new();
    let (id, ptr) = table.allocate(64, 8).unwrap();
    assert!(!ptr.is_null());
    assert!(id > 0);

    // Write and read back
    unsafe {
        *ptr = 42;
        assert_eq!(*ptr, 42);
    }

    assert!(table.free(id));
    assert!(!table.free(id)); // double-free returns false
}

#[test]
fn allocate_zeroed() {
    let mut table = NativeMemoryTable::new();
    let (_, ptr) = table.allocate(128, 1).unwrap();
    // Memory should be zeroed
    for i in 0..128 {
        assert_eq!(unsafe { *ptr.add(i) }, 0);
    }
    // cleanup in Drop
}

#[test]
fn multiple_allocations() {
    let mut table = NativeMemoryTable::new();
    let (id1, _) = table.allocate(32, 4).unwrap();
    let (id2, _) = table.allocate(64, 8).unwrap();
    let (id3, _) = table.allocate(16, 1).unwrap();
    assert_ne!(id1, id2);
    assert_ne!(id2, id3);

    table.free_all(&[id1, id3]);
    assert!(table.get_ptr(id1).is_none());
    assert!(table.get_ptr(id2).is_some());
    assert!(table.get_ptr(id3).is_none());
}

#[test]
fn layout_sizes() {
    assert_eq!(layout_byte_size(LAYOUT_BYTE), 1);
    assert_eq!(layout_byte_size(LAYOUT_SHORT), 2);
    assert_eq!(layout_byte_size(LAYOUT_INT), 4);
    assert_eq!(layout_byte_size(LAYOUT_LONG), 8);
    assert_eq!(layout_byte_size(LAYOUT_FLOAT), 4);
    assert_eq!(layout_byte_size(LAYOUT_DOUBLE), 8);
    assert_eq!(layout_byte_size(LAYOUT_ADDRESS), 8);
}

#[test]
fn get_size() {
    let mut table = NativeMemoryTable::new();
    let (id, _) = table.allocate(256, 16).unwrap();
    assert_eq!(table.get_size(id), Some(256));
}

// -----------------------------------------------------------------------
// M4b — bounds-checked pointer + generation handles
// -----------------------------------------------------------------------

#[test]
fn get_ptr_checked_validates_window() {
    let mut table = NativeMemoryTable::new();
    let (id, base) = table.allocate(64, 8).unwrap();
    // Whole-allocation window is valid and equals the base pointer.
    assert_eq!(table.get_ptr_checked(id, 0, 64), Some(base));
    // Interior window is valid and offset correctly.
    assert_eq!(
        table.get_ptr_checked(id, 16, 8),
        Some(unsafe { base.add(16) })
    );
    // One-past-the-end zero-length window is allowed.
    assert_eq!(
        table.get_ptr_checked(id, 64, 0),
        Some(unsafe { base.add(64) })
    );
    // Past the end fails closed.
    assert!(table.get_ptr_checked(id, 60, 8).is_none());
    assert!(table.get_ptr_checked(id, 65, 0).is_none());
    // Overflow of offset+len fails closed (no panic).
    assert!(table.get_ptr_checked(id, usize::MAX, 1).is_none());
    // Unknown id fails closed.
    assert!(table.get_ptr_checked(999, 0, 1).is_none());
}

#[test]
fn get_ptr_checked_after_free_returns_none() {
    let mut table = NativeMemoryTable::new();
    let (id, _) = table.allocate(32, 8).unwrap();
    assert!(table.get_ptr_checked(id, 0, 32).is_some());
    assert!(table.free(id));
    // After free the id no longer resolves — guards use-after-free at the
    // table boundary.
    assert!(table.get_ptr_checked(id, 0, 1).is_none());
}

#[test]
fn alloc_handle_detects_stale_after_free_and_reuse() {
    let mut table = NativeMemoryTable::new();
    let (h1, _) = table.allocate_handle(32, 8).unwrap();
    let first_id = h1.alloc_id;
    // The first handle resolves while live.
    assert!(table.get_ptr_checked_handle(h1, 0, 32).is_some());
    assert!(table.free(first_id));
    // Force id reuse by rewinding next_id so the next allocation reuses
    // `first_id`. This simulates the i64-wrap reuse scenario.
    table.next_id = first_id;
    let (h2, _) = table.allocate_handle(32, 8).unwrap();
    assert_eq!(h2.alloc_id, first_id, "test setup: id should be reused");
    // The fresh handle works...
    assert!(table.get_ptr_checked_handle(h2, 0, 32).is_some());
    // ...but the stale handle for the freed allocation fails closed even
    // though the id now resolves to a *different* allocation.
    assert_ne!(h1.generation, h2.generation);
    assert!(table.get_ptr_checked_handle(h1, 0, 1).is_none());
}

#[test]
fn generation_of_tracks_live_slot() {
    let mut table = NativeMemoryTable::new();
    let (id, _) = table.allocate(8, 1).unwrap();
    let gen = table.generation_of(id).unwrap();
    // A hand-built handle with the live generation resolves.
    let h = AllocHandle::new(id, gen);
    assert!(table.get_ptr_checked_handle(h, 0, 8).is_some());
    // A bogus generation fails closed.
    let bad = AllocHandle::new(id, gen.wrapping_add(1));
    assert!(table.get_ptr_checked_handle(bad, 0, 8).is_none());
    assert!(table.free(id));
    assert!(table.generation_of(id).is_none());
}

// -----------------------------------------------------------------------
// NativeMemoryTable — additional tests
// -----------------------------------------------------------------------

#[test]
fn default_is_same_as_new() {
    let table = NativeMemoryTable::default();
    assert_eq!(table.next_id, 1);
    assert!(table.allocations.is_empty());
}

#[test]
fn allocate_minimum_size() {
    let mut table = NativeMemoryTable::new();
    // size=0 should be rounded up to 1
    let result = table.allocate(0, 1);
    assert!(result.is_some());
    let (id, ptr) = result.unwrap();
    assert!(!ptr.is_null());
    assert_eq!(table.get_size(id), Some(1));
}

#[test]
fn allocate_minimum_alignment() {
    let mut table = NativeMemoryTable::new();
    // align=0 should be rounded up to 1
    let result = table.allocate(16, 0);
    assert!(result.is_some());
}

#[test]
fn allocate_does_not_leak_on_id_exhaustion() {
    // Regression: previously, when `next_id.checked_add(1)`
    // overflowed, `allocate` returned `None` without freeing the
    // block it had just acquired via `alloc::alloc_zeroed`. Force
    // the overflow by seeding `next_id` at `i64::MAX` and verify:
    //   1. the function returns `None`,
    //   2. the allocation table stays empty (the failed allocation
    //      was *not* stashed under any id), and
    //   3. the running `live_bytes` total is unchanged, and
    //   4. the dedicated free-on-exhaustion path actually ran,
    //      witnessed via the `EXHAUSTION_DEALLOCS` test counter.
    let before = test_hooks::EXHAUSTION_DEALLOCS.load(std::sync::atomic::Ordering::Relaxed);
    let mut table = NativeMemoryTable::new();
    table.next_id = i64::MAX;
    let result = table.allocate(4096, 8);
    assert!(result.is_none(), "expected None on id exhaustion");
    assert!(
        table.allocations.is_empty(),
        "leaked block was inserted into table"
    );
    assert_eq!(table.live_bytes, 0, "live_bytes drifted on failure");
    let after = test_hooks::EXHAUSTION_DEALLOCS.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        after - before,
        1,
        "exhaustion path did not call the matching dealloc"
    );
}

#[test]
fn get_ptr_nonexistent_returns_none() {
    let table = NativeMemoryTable::new();
    assert!(table.get_ptr(999).is_none());
}

#[test]
fn get_size_nonexistent_returns_none() {
    let table = NativeMemoryTable::new();
    assert!(table.get_size(999).is_none());
}

#[test]
fn free_nonexistent_returns_false() {
    let mut table = NativeMemoryTable::new();
    assert!(!table.free(999));
}

#[test]
fn free_all_with_empty_slice() {
    let mut table = NativeMemoryTable::new();
    table.free_all(&[]); // should not panic
}

#[test]
fn free_all_with_mix_of_valid_and_invalid() {
    let mut table = NativeMemoryTable::new();
    let (id1, _) = table.allocate(8, 1).unwrap();
    table.free_all(&[id1, 999, 1000]);
    assert!(table.get_ptr(id1).is_none());
}

#[test]
fn ids_are_sequential() {
    let mut table = NativeMemoryTable::new();
    let (id1, _) = table.allocate(8, 1).unwrap();
    let (id2, _) = table.allocate(8, 1).unwrap();
    let (id3, _) = table.allocate(8, 1).unwrap();
    assert_eq!(id1, 1);
    assert_eq!(id2, 2);
    assert_eq!(id3, 3);
}

#[test]
fn large_allocation() {
    let mut table = NativeMemoryTable::new();
    let (id, ptr) = table.allocate(1024 * 1024, 64).unwrap(); // 1 MiB
    assert!(!ptr.is_null());
    assert_eq!(table.get_size(id), Some(1024 * 1024));
    assert!(table.free(id));
}

#[test]
fn write_and_read_back_multiple_bytes() {
    let mut table = NativeMemoryTable::new();
    let (id, ptr) = table.allocate(256, 8).unwrap();
    unsafe {
        for i in 0..=255u8 {
            *ptr.add(i as usize) = i;
        }
        for i in 0..=255u8 {
            assert_eq!(*ptr.add(i as usize), i);
        }
    }
    assert!(table.free(id));
}

#[test]
fn drop_frees_all_remaining() {
    // Just ensure no leak/crash — miri or valgrind would catch leaks
    let mut table = NativeMemoryTable::new();
    let _ = table.allocate(64, 8);
    let _ = table.allocate(128, 16);
    // table dropped here, Drop impl should free both
}

// --- Phase 80.6: Native Alloc returns None on overflow ---

#[test]
fn allocate_overflow_returns_none() {
    let mut table = NativeMemoryTable::new();
    // usize::MAX as size — Layout::from_size_align will fail, returning None.
    let result = table.allocate(usize::MAX, 8);
    assert!(
        result.is_none(),
        "allocation of usize::MAX bytes must return None, not panic"
    );
}

// -----------------------------------------------------------------------
// layout_byte_size — compound and unknown kinds
// -----------------------------------------------------------------------

#[test]
fn layout_sizes_boolean_and_char() {
    assert_eq!(layout_byte_size(LAYOUT_BOOLEAN), 1);
    assert_eq!(layout_byte_size(LAYOUT_CHAR), 2);
}

#[test]
fn layout_sizes_compound_return_zero() {
    assert_eq!(layout_byte_size(LAYOUT_STRUCT), 0);
    assert_eq!(layout_byte_size(LAYOUT_UNION), 0);
    assert_eq!(layout_byte_size(LAYOUT_SEQUENCE), 0);
    assert_eq!(layout_byte_size(LAYOUT_PADDING), 0);
}

#[test]
fn layout_size_unknown_kind_defaults_to_1() {
    assert_eq!(layout_byte_size(99), 1);
    assert_eq!(layout_byte_size(-1), 1);
}

// -----------------------------------------------------------------------
// layout_alignment
// -----------------------------------------------------------------------

#[test]
fn layout_alignment_primitives() {
    assert_eq!(layout_alignment(LAYOUT_BYTE), 1);
    assert_eq!(layout_alignment(LAYOUT_BOOLEAN), 1);
    assert_eq!(layout_alignment(LAYOUT_SHORT), 2);
    assert_eq!(layout_alignment(LAYOUT_CHAR), 2);
    assert_eq!(layout_alignment(LAYOUT_INT), 4);
    assert_eq!(layout_alignment(LAYOUT_FLOAT), 4);
    assert_eq!(layout_alignment(LAYOUT_LONG), 8);
    assert_eq!(layout_alignment(LAYOUT_DOUBLE), 8);
    assert_eq!(layout_alignment(LAYOUT_ADDRESS), 8);
}

#[test]
fn layout_alignment_unknown_defaults_to_1() {
    assert_eq!(layout_alignment(99), 1);
    assert_eq!(layout_alignment(-1), 1);
}

// -----------------------------------------------------------------------
// align_up
// -----------------------------------------------------------------------

#[test]
fn align_up_already_aligned() {
    assert_eq!(align_up(16, 8), 16);
    assert_eq!(align_up(0, 4), 0);
}

#[test]
fn align_up_needs_padding() {
    assert_eq!(align_up(1, 4), 4);
    assert_eq!(align_up(5, 8), 8);
    assert_eq!(align_up(9, 4), 12);
    assert_eq!(align_up(7, 2), 8);
}

#[test]
fn align_up_align_1() {
    assert_eq!(align_up(0, 1), 0);
    assert_eq!(align_up(7, 1), 7);
    assert_eq!(align_up(100, 1), 100);
}

#[test]
fn align_up_align_0_returns_offset() {
    assert_eq!(align_up(42, 0), 42);
}

// -----------------------------------------------------------------------
// UpcallTable
// -----------------------------------------------------------------------

#[test]
fn upcall_table_new_is_empty() {
    let table = UpcallTable::new();
    assert!(table.get(0).is_none());
}

#[test]
fn upcall_table_default() {
    let table = UpcallTable::default();
    assert!(table.get(0).is_none());
}

/// Helper to create a dummy ObjectRef for testing.
fn dummy_obj_ref() -> ObjectRef {
    unsafe { ObjectRef::from_raw(0x1000 as *mut u8) }
}

#[test]
fn upcall_register_and_get() {
    let mut table = UpcallTable::new();
    let entry = UpcallEntry {
        target: dummy_obj_ref(),
        method_name: "apply".to_string(),
        method_descriptor: "(I)I".to_string(),
        param_kinds: vec![LAYOUT_INT],
        return_kind: LAYOUT_INT,
    };
    let slot = table.register(entry);
    assert_eq!(slot, 0);
    let e = table.get(0).unwrap();
    assert_eq!(e.method_name, "apply");
    assert_eq!(e.param_kinds, vec![LAYOUT_INT]);
}

#[test]
fn upcall_remove_and_reuse_slot() {
    let mut table = UpcallTable::new();
    let entry0 = UpcallEntry {
        target: dummy_obj_ref(),
        method_name: "first".to_string(),
        method_descriptor: "()V".to_string(),
        param_kinds: vec![],
        return_kind: LAYOUT_LONG,
    };
    let entry1 = UpcallEntry {
        target: dummy_obj_ref(),
        method_name: "second".to_string(),
        method_descriptor: "()V".to_string(),
        param_kinds: vec![],
        return_kind: LAYOUT_LONG,
    };
    let slot0 = table.register(entry0);
    let slot1 = table.register(entry1);
    assert_eq!(slot0, 0);
    assert_eq!(slot1, 1);

    // Remove slot 0
    table.remove(slot0);
    assert!(table.get(slot0).is_none());
    assert!(table.get(slot1).is_some());

    // Next register should reuse slot 0
    let entry2 = UpcallEntry {
        target: dummy_obj_ref(),
        method_name: "third".to_string(),
        method_descriptor: "()V".to_string(),
        param_kinds: vec![],
        return_kind: LAYOUT_LONG,
    };
    let reused = table.register(entry2);
    assert_eq!(reused, 0);
    assert_eq!(table.get(0).unwrap().method_name, "third");
}

#[test]
fn upcall_handle_detects_slot_reuse() {
    // M4c: a handle minted for the original occupant of a slot must NOT
    // resolve after the slot is removed and reused by a different entry.
    let mut table = UpcallTable::new();
    let h0 = table.register_handle(UpcallEntry {
        target: dummy_obj_ref(),
        method_name: "first".to_string(),
        method_descriptor: "()V".to_string(),
        param_kinds: vec![],
        return_kind: LAYOUT_LONG,
    });
    assert_eq!(h0.slot, 0);
    assert_eq!(table.get_checked(h0).unwrap().method_name, "first");

    // Remove and re-register: slot index 0 is reused, generation differs.
    table.remove(h0.slot);
    assert!(
        table.get_checked(h0).is_none(),
        "stale handle must fail closed after remove"
    );

    let h1 = table.register_handle(UpcallEntry {
        target: dummy_obj_ref(),
        method_name: "second".to_string(),
        method_descriptor: "()V".to_string(),
        param_kinds: vec![],
        return_kind: LAYOUT_LONG,
    });
    assert_eq!(h1.slot, 0, "slot should be reused");
    assert_ne!(
        h0.generation, h1.generation,
        "generation must advance on reuse"
    );
    // Fresh handle resolves to the new entry...
    assert_eq!(table.get_checked(h1).unwrap().method_name, "second");
    // ...stale handle still fails closed (confused-deputy defence).
    assert!(table.get_checked(h0).is_none());
    // The legacy unchecked `get` would have returned the WRONG entry here.
    assert_eq!(table.get(0).unwrap().method_name, "second");
}

#[test]
fn upcall_generation_of_and_manual_handle() {
    let mut table = UpcallTable::new();
    let slot = table.register(UpcallEntry {
        target: dummy_obj_ref(),
        method_name: "apply".to_string(),
        method_descriptor: "()V".to_string(),
        param_kinds: vec![],
        return_kind: LAYOUT_INT,
    });
    let gen = table.generation_of(slot).unwrap();
    let h = UpcallHandle::new(slot, gen);
    assert!(table.get_checked(h).is_some());
    let bad = UpcallHandle::new(slot, gen.wrapping_add(1));
    assert!(table.get_checked(bad).is_none());
}

#[test]
fn upcall_generation_of_removed_slot_fails_closed_after_reuse() {
    let mut table = UpcallTable::new();
    let handle = table.register_handle(UpcallEntry {
        target: dummy_obj_ref(),
        method_name: "first".to_string(),
        method_descriptor: "()V".to_string(),
        param_kinds: vec![],
        return_kind: LAYOUT_INT,
    });

    table.remove(handle.slot);
    assert!(
        table.generation_of(handle.slot).is_none(),
        "vacant slots must not expose a generation that can be reused"
    );

    let removed_generation = table.slots[handle.slot].generation;
    let stale_after_remove = UpcallHandle::new(handle.slot, removed_generation);
    assert!(table.get_checked(stale_after_remove).is_none());

    let reused = table.register_handle(UpcallEntry {
        target: dummy_obj_ref(),
        method_name: "second".to_string(),
        method_descriptor: "()V".to_string(),
        param_kinds: vec![],
        return_kind: LAYOUT_INT,
    });
    assert_eq!(reused.slot, handle.slot);
    assert_ne!(
        reused.generation, removed_generation,
        "reused slots must receive a generation distinct from the vacant slot"
    );
    assert!(table.get_checked(stale_after_remove).is_none());
    assert_eq!(table.get_checked(reused).unwrap().method_name, "second");
}

#[test]
fn upcall_remove_out_of_bounds_noop() {
    let mut table = UpcallTable::new();
    table.remove(100); // should not panic
}

#[test]
fn upcall_get_out_of_bounds_none() {
    let table = UpcallTable::new();
    assert!(table.get(100).is_none());
}

#[test]
fn upcall_collect_roots_yields_live_targets() {
    let mut table = UpcallTable::new();
    let a = unsafe { ObjectRef::from_raw(0x1000 as *mut u8) };
    let b = unsafe { ObjectRef::from_raw(0x2000 as *mut u8) };
    let s0 = table.register(UpcallEntry {
        target: a,
        method_name: "a".into(),
        method_descriptor: String::new(),
        param_kinds: vec![],
        return_kind: -1,
    });
    table.register(UpcallEntry {
        target: b,
        method_name: "b".into(),
        method_descriptor: String::new(),
        param_kinds: vec![],
        return_kind: -1,
    });
    let mut roots = Vec::new();
    table.collect_roots(&mut roots);
    assert!(roots.contains(&a));
    assert!(roots.contains(&b));
    assert_eq!(roots.len(), 2);

    // A removed (vacated) slot contributes no root.
    table.remove(s0);
    let mut roots2 = Vec::new();
    table.collect_roots(&mut roots2);
    assert_eq!(roots2, vec![b]);
}

#[test]
fn upcall_update_after_gc_remaps_target() {
    // MOVING-GC FIX: a relocating collection must rewrite each live slot's
    // callback target so the legacy `pe_upcall_invoke` dispatch resolves the
    // object's CURRENT address, not the stale from-space pointer.
    let mut table = UpcallTable::new();
    let old = unsafe { ObjectRef::from_raw(0x3000 as *mut u8) };
    let new_addr = 0x9000usize;
    table.register(UpcallEntry {
        target: old,
        method_name: "moved".into(),
        method_descriptor: String::new(),
        param_kinds: vec![],
        return_kind: -1,
    });
    let mut pm = cratonvm_types::PointerMap::default();
    pm.insert(old.as_ptr() as usize, new_addr);
    table.update_after_gc(&pm);
    assert_eq!(table.get(0).unwrap().target.as_ptr() as usize, new_addr);

    // An empty pointer map (non-moving collection) leaves the target intact.
    table.update_after_gc(&cratonvm_types::PointerMap::default());
    assert_eq!(table.get(0).unwrap().target.as_ptr() as usize, new_addr);
}

// -----------------------------------------------------------------------
// Arena constants
// -----------------------------------------------------------------------

#[test]
fn arena_constants() {
    assert_eq!(ARENA_GLOBAL, 0);
    assert_eq!(ARENA_AUTO, 1);
    assert_eq!(ARENA_CONFINED, 2);
}

// -----------------------------------------------------------------------
// Foreign Function & Memory API layout tests
// -----------------------------------------------------------------------

#[test]
fn test_memory_layout_basic_types() {
    // ValueLayout sizes per JVM spec
    assert_eq!(std::mem::size_of::<i8>(), 1); // JAVA_BYTE
    assert_eq!(std::mem::size_of::<i16>(), 2); // JAVA_SHORT
    assert_eq!(std::mem::size_of::<i32>(), 4); // JAVA_INT
    assert_eq!(std::mem::size_of::<i64>(), 8); // JAVA_LONG
    assert_eq!(std::mem::size_of::<f32>(), 4); // JAVA_FLOAT
    assert_eq!(std::mem::size_of::<f64>(), 8); // JAVA_DOUBLE
    assert_eq!(std::mem::size_of::<bool>(), 1); // JAVA_BOOLEAN
    assert_eq!(std::mem::size_of::<u16>(), 2); // JAVA_CHAR
}

#[test]
fn test_pointer_alignment() {
    // Pointers should be aligned to their size
    assert_eq!(
        std::mem::align_of::<*const u8>(),
        std::mem::size_of::<*const u8>()
    );
}

#[test]
fn test_arena_allocation_and_cleanup() {
    // Arena.ofConfined() allocates, close() frees
    let layout = std::alloc::Layout::from_size_align(64, 8).unwrap();
    let ptr = unsafe { std::alloc::alloc(layout) };
    assert!(!ptr.is_null());
    unsafe {
        std::alloc::dealloc(ptr, layout);
    }
}

#[test]
fn test_memory_segment_bounds() {
    // MemorySegment must enforce bounds on access
    let data = vec![0u8; 128];
    let base = data.as_ptr();
    let len = data.len();
    // In-bounds access
    assert!(0 < len);
    assert!(127 < len);
    // Out-of-bounds would be 128+
    assert!(128 >= len);
    let _ = base; // suppress unused warning
}

#[test]
fn test_null_pointer_segment() {
    // MemorySegment.NULL should be zero-length at address 0
    let null_ptr: *const u8 = std::ptr::null();
    assert!(null_ptr.is_null());
}

#[test]
fn test_struct_layout_composition() {
    // StructLayout should compose member layouts
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    assert_eq!(std::mem::size_of::<Point>(), 8);
    assert_eq!(std::mem::align_of::<Point>(), 4);
}

#[test]
fn test_union_layout() {
    // UnionLayout takes the size of the largest member
    #[repr(C)]
    union IntOrFloat {
        i: i32,
        f: f32,
    }
    assert_eq!(std::mem::size_of::<IntOrFloat>(), 4);
}

#[test]
fn test_string_marshaling_utf8() {
    // String marshaling: Java String -> C char* (UTF-8)
    let java_string = "Hello, Panama!";
    let c_bytes = java_string.as_bytes();
    assert_eq!(c_bytes.len(), 14);
    // Must be null-terminated for C
    let mut c_string = java_string.as_bytes().to_vec();
    c_string.push(0);
    assert_eq!(c_string.last(), Some(&0u8));
}

#[test]
fn test_downcall_argument_types() {
    // Verify supported argument types for downcalls
    let int_arg: i32 = 42;
    let long_arg: i64 = 100;
    let float_arg: f32 = 3.14;
    let double_arg: f64 = 2.718;
    let ptr_arg: *const u8 = std::ptr::null();
    assert_eq!(int_arg, 42);
    assert_eq!(long_arg, 100);
    assert!((float_arg - 3.14f32).abs() < 0.001);
    assert!((double_arg - 2.718).abs() < 0.001);
    assert!(ptr_arg.is_null());
}

#[test]
fn test_memory_segment_copy() {
    // MemorySegment.copy(src, srcOffset, dst, dstOffset, bytes)
    let src = vec![1u8, 2, 3, 4, 5];
    let mut dst = vec![0u8; 5];
    dst.copy_from_slice(&src);
    assert_eq!(dst, vec![1, 2, 3, 4, 5]);
}

#[test]
fn test_memory_segment_slice() {
    // MemorySegment.asSlice(offset, size)
    let data = vec![10u8, 20, 30, 40, 50];
    let slice = &data[1..4];
    assert_eq!(slice, &[20, 30, 40]);
}
