// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! r9-ea: the single-pass `aastore` skips its two barrier CALLs on the
//! collector's published gates, exactly as the reference `putfield` has since
//! 2026-09-02 — and still makes each call when its gate says there may be work.
//!
//! Executes `static void set(Object[] a, int i, Object v) { a[i] = v; }` against
//! a hand-built array header and counts the helper calls. The plan published
//! here is the generational one: the MASK post-barrier shape
//! (`GC_FLAG_OLD_GEN`), with `post_active` held at 1 so that the mask — not a
//! global "nothing is old" shortcut — is what rules the post barrier out.
#![cfg(target_arch = "x86_64")]

use cratonvm_jit::x64::compile;
use cratonvm_jit_api::JitRuntimeHelpers;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

static PRE_GATE: AtomicU8 = AtomicU8::new(0);
static POST_GATE: AtomicU8 = AtomicU8::new(1);
static TYPE_CHECKS: AtomicUsize = AtomicUsize::new(0);
static SATB_CALLS: AtomicUsize = AtomicUsize::new(0);
static BARRIER_CALLS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn type_check_ok(_vm: i64, _array: i64, _value: i64) -> i64 {
    TYPE_CHECKS.fetch_add(1, Ordering::SeqCst);
    0
}
unsafe extern "C" fn count_satb(_vm: i64, _old: i64) {
    SATB_CALLS.fetch_add(1, Ordering::SeqCst);
}
unsafe extern "C" fn count_write_barrier(_vm: i64, _obj: i64, _val: i64) {
    BARRIER_CALLS.fetch_add(1, Ordering::SeqCst);
}
unsafe extern "C" fn record_throw_bci(_bci: i64) {}
unsafe extern "C" fn unexpected() {
    panic!("r9_ea_aastore_gates: an unwired runtime helper was called");
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
        satb_pre_write_barrier: count_satb as *const () as usize,
        write_barrier: count_write_barrier as *const () as usize,
        set_throw_bci: record_throw_bci as *const () as usize,
        ref_store_pre_gate: std::ptr::addr_of!(PRE_GATE) as usize,
        ref_store_post_gate: std::ptr::addr_of!(POST_GATE) as usize,
        ref_store_post_young_floor: 0,
        ref_store_post_skip_mask: usize::from(cratonvm_types::GC_FLAG_OLD_GEN),
        // Inline TLAB bump off (no `new` here anyway), as in every backend test.
        tlab_cursor_offset_in_thread: 0,
        tlab_end_offset_in_thread: 8,
        ..Default::default()
    }
}

/// A fake four-element reference array. 64 bytes, 8-aligned.
fn fake_array(flags: u8) -> Box<[u64; 8]> {
    let mut a = Box::new([0u64; 8]);
    // SAFETY: both writes land inside the 64-byte buffer's 16-byte header.
    unsafe {
        let p = a.as_mut_ptr() as *mut u8;
        std::ptr::write_unaligned(p.add(cratonvm_types::ARRAY_LENGTH_OFFSET) as *mut u32, 4u32);
        *p.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) = flags;
    }
    a
}

fn element(a: &[u64; 8], i: usize) -> u64 {
    let off = cratonvm_types::ARRAY_DATA_OFFSET + i * 8;
    // SAFETY: `off + 8 <= 64` for `i < 4` with a 16-byte data offset.
    unsafe { std::ptr::read_unaligned((a.as_ptr() as *const u8).add(off) as *const u64) }
}

fn counts() -> (usize, usize, usize) {
    (
        TYPE_CHECKS.load(Ordering::SeqCst),
        SATB_CALLS.load(Ordering::SeqCst),
        BARRIER_CALLS.load(Ordering::SeqCst),
    )
}

fn reset() {
    TYPE_CHECKS.store(0, Ordering::SeqCst);
    SATB_CALLS.store(0, Ordering::SeqCst);
    BARRIER_CALLS.store(0, Ordering::SeqCst);
}

#[test]
fn aastore_barriers_follow_the_published_gates() {
    // static void set(Object[] a, int i, Object v) { a[i] = v; }
    let code: Vec<u8> = vec![0x2a, 0x1b, 0x2c, 0x53, 0xb1];
    let h = helpers();
    let compiled = compile(
        &code,
        code.len(),
        3,    // num_params
        3,    // max_locals
        true, // needs_heap: the barrier helpers take the vm pointer
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

    let value = Box::new([0u64; 4]);
    let value_addr = value.as_ptr() as usize as i64;
    let narrow = cratonvm_types::narrow_oop::narrow_oops_enabled();

    // 1. Marking idle, YOUNG receiver: the store happens and neither barrier
    //    helper is called. Only the covariance check (non-null value) runs.
    PRE_GATE.store(0, Ordering::SeqCst);
    POST_GATE.store(1, Ordering::SeqCst);
    let young = fake_array(0);
    reset();
    // SAFETY: JIT code from valid bytecode; the receiver is a live buffer
    // shaped like a 4-element reference array, and no helper stores anything.
    unsafe {
        compiled.call_with_heap(0, &[young.as_ptr() as usize as i64, 1, value_addr]);
    }
    assert_eq!(
        counts(),
        (1, 0, 0),
        "young receiver, idle marker: no barrier call"
    );
    if !narrow {
        assert_eq!(
            element(&young, 1),
            value_addr as u64,
            "the element is stored"
        );
    }

    // 2. OLD receiver: the post barrier must run.
    let old = fake_array(cratonvm_types::GC_FLAG_OLD_GEN);
    reset();
    // SAFETY: as above.
    unsafe {
        compiled.call_with_heap(0, &[old.as_ptr() as usize as i64, 2, value_addr]);
    }
    assert_eq!(
        counts(),
        (1, 0, 1),
        "an old receiver still reaches write_barrier"
    );

    // 3. Marking ACTIVE: the SATB pre-barrier must run.
    PRE_GATE.store(1, Ordering::SeqCst);
    reset();
    // SAFETY: as above.
    unsafe {
        compiled.call_with_heap(0, &[young.as_ptr() as usize as i64, 3, value_addr]);
    }
    assert_eq!(
        counts(),
        (1, 1, 0),
        "an armed marker still reaches the SATB helper"
    );

    // 4. A null value into an old receiver: no type check, no post barrier.
    PRE_GATE.store(0, Ordering::SeqCst);
    reset();
    // SAFETY: as above.
    unsafe {
        compiled.call_with_heap(0, &[old.as_ptr() as usize as i64, 0, 0]);
    }
    assert_eq!(counts(), (0, 0, 0), "storing null records no edge");

    // 5. No old objects anywhere (`post_active == 0`): even an old receiver
    //    skips the post barrier.
    POST_GATE.store(0, Ordering::SeqCst);
    reset();
    // SAFETY: as above.
    unsafe {
        compiled.call_with_heap(0, &[old.as_ptr() as usize as i64, 1, value_addr]);
    }
    assert_eq!(counts(), (1, 0, 0), "post gate closed: no barrier call");
    POST_GATE.store(1, Ordering::SeqCst);
}
