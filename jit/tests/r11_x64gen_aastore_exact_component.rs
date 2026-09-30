// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! r11 x64gen: the single-pass `aastore` answers the JVMS covariance question
//! inline when the value is a PLAIN object whose class id equals the array's
//! header class id (a reference array's header holds its COMPONENT class), and
//! calls `aastore_type_check` for everything else — an array value (whose
//! header carries ITS component's id, so an equal id proves nothing), a
//! different class, and class id 0 (a primitive array's and an unnameable
//! component's id).
//!
//! Executes `static void set(Object[] a, int i, Object v) { a[i] = v; }`
//! against hand-built headers and counts the helper calls, in the shape of
//! `r9_ea_aastore_gates.rs`.
#![cfg(target_arch = "x86_64")]

use cratonvm_jit::x64::compile;
use cratonvm_jit_api::JitRuntimeHelpers;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

static PRE_GATE: AtomicU8 = AtomicU8::new(0);
static POST_GATE: AtomicU8 = AtomicU8::new(0);
static TYPE_CHECKS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn type_check_ok(_vm: i64, _array: i64, _value: i64) -> i64 {
    TYPE_CHECKS.fetch_add(1, Ordering::SeqCst);
    0
}
unsafe extern "C" fn ignore_satb(_vm: i64, _old: i64) {}
unsafe extern "C" fn ignore_write_barrier(_vm: i64, _obj: i64, _val: i64) {}
unsafe extern "C" fn record_throw_bci(_bci: i64) {}
unsafe extern "C" fn unexpected() {
    panic!("r11_x64gen_aastore_exact_component: an unwired runtime helper was called");
}

fn helpers() -> JitRuntimeHelpers {
    let s = unexpected as *const () as usize;
    JitRuntimeHelpers {
        aastore: s,
        throw_aioobe: s,
        throw_exception: s,
        jit_npe_with_action: s,
        dispatch_threw: s,
        aastore_type_check: type_check_ok as *const () as usize,
        satb_pre_write_barrier: ignore_satb as *const () as usize,
        write_barrier: ignore_write_barrier as *const () as usize,
        set_throw_bci: record_throw_bci as *const () as usize,
        ref_store_pre_gate: std::ptr::addr_of!(PRE_GATE) as usize,
        ref_store_post_gate: std::ptr::addr_of!(POST_GATE) as usize,
        ref_store_post_young_floor: 0,
        ref_store_post_skip_mask: usize::from(cratonvm_types::GC_FLAG_OLD_GEN),
        tlab_cursor_offset_in_thread: 0,
        tlab_end_offset_in_thread: 8,
        ..Default::default()
    }
}

/// A 64-byte, 8-aligned header-shaped buffer: `class_id` at the helper
/// table's `class_id_offset_in_obj`, the KIND_TAGS byte, and — for an array —
/// a length of 4.
fn fake(class_id: u32, kind_tags: u8, array_len: Option<u32>) -> Box<[u64; 8]> {
    let mut words = Box::new([0u64; 8]);
    let cid_off = helpers().class_id_offset_in_obj;
    let kind_off = cratonvm_types::KIND_TAGS_BYTE_OFFSET;
    assert!(cid_off + 4 <= 64 && kind_off < 64, "the fake header fits in 64 bytes");
    // SAFETY: every write lands inside the 64-byte buffer (asserted above, and
    // ARRAY_LENGTH_OFFSET + 4 <= 16).
    unsafe {
        let p = words.as_mut_ptr() as *mut u8;
        std::ptr::write_unaligned(p.add(cid_off) as *mut u32, class_id);
        *p.add(kind_off) = kind_tags;
        if let Some(len) = array_len {
            std::ptr::write_unaligned(p.add(cratonvm_types::ARRAY_LENGTH_OFFSET) as *mut u32, len);
        }
    }
    words
}

fn element(a: &[u64; 8], i: usize) -> u64 {
    let off = cratonvm_types::ARRAY_DATA_OFFSET + i * 8;
    // SAFETY: `off + 8 <= 64` for `i < 4` with a 16-byte data offset.
    unsafe { std::ptr::read_unaligned((a.as_ptr() as *const u8).add(off) as *const u64) }
}

#[test]
fn an_exact_component_store_skips_the_type_check_and_nothing_else_does() {
    // static void set(Object[] a, int i, Object v) { a[i] = v; }
    let code: Vec<u8> = vec![0x2a, 0x1b, 0x2c, 0x53, 0xb1];
    let h = helpers();
    let compiled = compile(
        &code,
        code.len(),
        3,
        3,
        true, // needs_heap: the helpers take the vm pointer
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &h,
        HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("aastore must compile");

    let array_kind = cratonvm_types::ObjectKind::Array as u8;
    let plain_kind = cratonvm_types::ObjectKind::Object as u8;
    assert_ne!(array_kind, plain_kind);
    let narrow = cratonvm_types::narrow_oop::narrow_oops_enabled();

    // (array class id, value class id, value kind, expected type-check calls)
    let cases: [(u32, u32, u8, usize, &str); 5] = [
        (7, 7, plain_kind, 0, "plain value of exactly the component class"),
        (7, 8, plain_kind, 1, "plain value of another class"),
        (7, 7, array_kind, 1, "an ARRAY value whose component id matches"),
        (0, 0, plain_kind, 1, "class id 0 on both sides proves nothing"),
        (7, 0, plain_kind, 1, "class id 0 value"),
    ];
    for (i, &(array_id, value_id, value_kind, want, what)) in cases.iter().enumerate() {
        let array = fake(array_id, array_kind, Some(4));
        let value = fake(value_id, value_kind, None);
        let value_addr = value.as_ptr() as usize as i64;
        let slot = i % 4;
        TYPE_CHECKS.store(0, Ordering::SeqCst);
        // SAFETY: JIT code from valid bytecode; the receiver is a live buffer
        // shaped like a 4-element reference array, the value a live buffer
        // shaped like a header, and no helper stores anything.
        unsafe {
            compiled.call_with_heap(0, &[array.as_ptr() as usize as i64, slot as i64, value_addr]);
        }
        assert_eq!(TYPE_CHECKS.load(Ordering::SeqCst), want, "{what}");
        if !narrow {
            assert_eq!(element(&array, slot), value_addr as u64, "the element is stored: {what}");
        }
    }

    // A null value still skips the check, as it always did.
    let array = fake(7, array_kind, Some(4));
    TYPE_CHECKS.store(0, Ordering::SeqCst);
    // SAFETY: as above.
    unsafe {
        compiled.call_with_heap(0, &[array.as_ptr() as usize as i64, 0, 0]);
    }
    assert_eq!(TYPE_CHECKS.load(Ordering::SeqCst), 0, "null is legal everywhere");
}
