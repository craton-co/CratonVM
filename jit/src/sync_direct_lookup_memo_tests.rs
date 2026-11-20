use super::*;
use crate::compile_request::SyncDirectTarget;

/// Round 14 wave 5 (lane fixup2, RV4-1): each index reaches the VM's
/// lookup once per memo, a refusal (`None`) is remembered as well as an
/// answer, and a fresh memo (the next compile request) asks again.
#[test]
fn each_index_reaches_the_vm_lookup_once_per_request() {
    let calls = std::cell::Cell::new(0u32);
    let target = SyncDirectTarget {
        entry: 0,
        needs_ctx: false,
        class_cp_idx: 7,
        mirror_slot: 0,
        instance: false,
        unoverridable: false,
    };
    let lookup = |cp_idx: u16| -> Option<SyncDirectTarget> {
        calls.set(calls.get() + 1);
        (cp_idx == 5).then_some(target)
    };
    let memo = std::cell::RefCell::new(SyncDirectLookupMemo::new());
    assert_eq!(sync_direct_lookup_memoized(&memo, &lookup, 5), Some(target));
    assert_eq!(sync_direct_lookup_memoized(&memo, &lookup, 5), Some(target));
    assert_eq!(calls.get(), 1);
    assert_eq!(sync_direct_lookup_memoized(&memo, &lookup, 9), None);
    assert_eq!(sync_direct_lookup_memoized(&memo, &lookup, 9), None);
    assert_eq!(calls.get(), 2);
    let next_request = std::cell::RefCell::new(SyncDirectLookupMemo::new());
    assert_eq!(
        sync_direct_lookup_memoized(&next_request, &lookup, 5),
        Some(target)
    );
    assert_eq!(calls.get(), 3);
}
