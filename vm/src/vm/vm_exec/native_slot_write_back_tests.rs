use super::*;
use crate::threading::jvm_thread::{JvmThread, NativeSlotFixup, ThreadId};

/// gen r5w1/crash5: the blocked-peer native-stack write-back stores only
/// into the calling thread's own live stack. A word of this test's own
/// frame is written; a word in the allocator's heap -- where a capture
/// routed to the wrong thread, or a thread whose stack was since reused,
/// would point -- is refused even though it still reads `orig`.
#[test]
fn a_native_slot_outside_the_own_stack_is_never_written() {
    use std::sync::atomic::Ordering;
    if cratonvm_gc::shadow_stack::current_thread_stack().is_none() {
        // No stack bounds on this target: the bound is `[sp, usize::MAX)`
        // and where the heap lies relative to it is not decidable here.
        return;
    }
    let mut thread = JvmThread::new(ThreadId(9), "native-slot-bound");
    let mut on_stack: usize = 0x1000;
    let on_heap = Box::new(0x1000usize);
    let stack_addr = std::hint::black_box(&mut on_stack) as *mut usize as usize;
    let heap_addr = &*on_heap as *const usize as usize;
    {
        let mut ns = thread.gc_block_state.native_slots.lock();
        ns.push(NativeSlotFixup {
            addr: stack_addr,
            orig: 0x1000,
            cur: 0x2000,
        });
        ns.push(NativeSlotFixup {
            addr: heap_addr,
            orig: 0x1000,
            cur: 0x2000,
        });
    }
    let refused_before = crate::threading::thread_registry::NATIVE_SLOTS_REFUSED_OFF_STACK
        .load(Ordering::Relaxed);
    let written = apply_native_slot_fixups(&mut thread, 0);
    // SAFETY: both addresses name live, aligned `usize`s owned by this test.
    let (stack_now, heap_now) = unsafe {
        (
            std::ptr::read_volatile(stack_addr as *const usize),
            std::ptr::read_volatile(heap_addr as *const usize),
        )
    };
    assert_eq!(
        heap_now, 0x1000,
        "a word outside this thread's stack is left alone"
    );
    assert_eq!(
        stack_now, 0x2000,
        "a live word of this thread's own stack is written"
    );
    assert_eq!(written, 1);
    assert!(
        crate::threading::thread_registry::NATIVE_SLOTS_REFUSED_OFF_STACK
            .load(Ordering::Relaxed)
            > refused_before
    );
    assert!(
        thread.gc_block_state.native_slots.lock().is_empty(),
        "drained"
    );
    drop(on_heap);
}

/// gcj-j1pn-raw-root-write-owner-findings item 2: the wake floor cuts the
/// band below, and only a floor strictly inside it does.
#[test]
fn gcj1pn_the_wake_floor_cuts_the_band_below() {
    use cratonvm_gc::shadow_stack::SlotBand;
    let own = SlotBand::new(0x4000, 0x9000);
    assert_eq!(native_fixup_store_band(own, 0), own, "no floor");
    assert_eq!(native_fixup_store_band(own, 0x6004), SlotBand::new(0x6000, 0x9000));
    assert_eq!(native_fixup_store_band(own, 0x3000), own, "a floor below sp");
    assert_eq!(native_fixup_store_band(own, 0x9000), own, "a floor at the top");
    assert_eq!(native_fixup_store_band(own, 0x4000), own);
}

