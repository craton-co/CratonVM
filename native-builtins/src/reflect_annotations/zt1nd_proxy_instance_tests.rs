#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use super::*;
use crate::test_utils::{mock_ctx, mock_reclaims, mock_refuse_allocations};

struct KnobReset;
impl Drop for KnobReset {
    fn drop(&mut self) {
        mock_refuse_allocations(0, false);
    }
}

/// Generational / G1 (the mock is not a ZGC VM): the gate is closed, so
/// the one-attempt door stays, unchanged.
#[test]
fn zt1nd_the_proxy_instance_gate_is_closed_off_zgc() {
    let ctx = mock_ctx();
    assert!(!ctx.heap_is_zgc());
    assert!(!proxy_instance_collects(&ctx));
}

/// The collecting mint: a refusal reclaims once and the retry is served;
/// a spent ladder answers `None` (the caller's `OutOfMemoryError`).
#[test]
fn zt1nd_the_proxy_instance_mint_reclaims_then_gives_up() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    let cid = cratonvm_types::ClassId::new(1);
    mock_refuse_allocations(1, true);
    assert!(proxy_instance_reclaiming(&mut ctx, cid, 1).is_some());
    assert_eq!(mock_reclaims(), 1, "one reclaim ran before the retry");
    mock_refuse_allocations(usize::MAX, false);
    assert!(proxy_instance_reclaiming(&mut ctx, cid, 1).is_none());
}
