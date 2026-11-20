//! gcg g1/o: the boot warm-up of the classes a caught OOME's trace needs,
//! and HotSpot's traced pool under the immutable-shape switch.
use super::*;
use crate::config::VmConfig;
use crate::vm::realms::heap_realm::OomeKind;
use std::sync::atomic::{AtomicU32, Ordering};

fn fake(addr: usize) -> ObjectRef {
    // SAFETY: non-null, 8-byte aligned, never dereferenced.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// Four traced errors per VM, then none, and the counter stops at four.
#[test]
fn gcg_g1o_the_traced_pool_holds_hotspots_four() {
    let used = AtomicU32::new(0);
    for _ in 0..HOTSPOT_PREALLOCATED_OOME_COUNT {
        assert!(take_traced_oome_slot(&used));
    }
    assert!(!take_traced_oome_slot(&used));
    assert!(!take_traced_oome_slot(&used));
    assert_eq!(
        used.load(Ordering::Relaxed),
        HOTSPOT_PREALLOCATED_OOME_COUNT
    );
}
