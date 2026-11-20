//! JIT round 21, lane hashlock (`CRATONVM_INFLATED_MARK_HASH`): a compact
//! instance's INFLATED word keeps (or, hashed while locked, receives) its
//! identity-hash bits, every reader answers the one value the monitor
//! holds, and the switch's `=0` arm leaves INFLATED words bare.
use super::tests::compact_object;
use super::{displaced_hash_at, header_of, MonitorTable};
use crate::memory::heap::Heap;
use crate::threading::jvm_thread::ThreadId;
use cratonvm_types::ObjectHeader;
use std::sync::atomic::Ordering;


fn heap() -> &'static Heap {
    Box::leak(Box::new(Heap::with_capacity(1 << 16)))
}

fn mark_of(obj: cratonvm_types::ObjectRef) -> u32 {
    header_of(obj).mark_word.load(Ordering::Acquire)
}



