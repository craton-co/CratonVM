#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use super::*;
use crate::test_utils::{mock_ctx, mock_reclaims, mock_refuse_allocations};
use cratonvm_types::error::VmError;

struct KnobReset;
impl Drop for KnobReset {
    fn drop(&mut self) {
        mock_refuse_allocations(0, false);
    }
}

fn is_oom(err: &MethodCallFailed) -> bool {
    matches!(
        err,
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
            message
        })) if message.starts_with("Java heap space")
    )
}

/// A refused mint reclaims once and the retry is served: the segment
/// answers the pointer.
#[test]
fn a_refused_collecting_mint_reclaims_and_is_served() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    // Warm the once-per-VM global arena so the refusal lands on the mint.
    let _ = address_return_segment(&mut ctx, 0x1000, 0, 1, true).expect("room");
    mock_refuse_allocations(1, true);
    let seg = address_return_segment(&mut ctx, 0x2000, 16, 1, true)
        .expect("the retry after the reclaim is served");
    assert_eq!(mock_reclaims(), 1, "one reclaim ran");
    assert_eq!(ctx.get_field(seg, 0), Value::Long(0x2000));
    assert_eq!(ctx.get_field(seg, 1), Value::Long(16));
}

/// Every rung refused: the catchable `OutOfMemoryError` (HotSpot's
/// `Java heap space`), never the one-attempt door; the upcall argument
/// mint likewise.
#[test]
fn a_spent_collecting_mint_is_an_oome() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    let _ = address_return_segment(&mut ctx, 0x1000, 0, 1, true).expect("room");
    mock_refuse_allocations(usize::MAX, false);
    let err = address_return_segment(&mut ctx, 0x2000, 0, 1, true)
        .expect_err("a full heap after the ladder is an OutOfMemoryError");
    assert!(is_oom(&err), "{err:?}");
    let err = alloc_segment_carrier_with(&mut ctx, 6, true)
        .expect_err("the carrier mint of an upcall argument too");
    assert!(is_oom(&err), "{err:?}");
}

/// Positive control of the gate: `collecting == false` never consults the
/// fallible door or the ladder (no reclaim even with refusals pending),
/// and a context that is not the VM's ZGC heap never collects here.
#[test]
fn the_one_attempt_arm_never_reclaims() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    let _ = address_return_segment(&mut ctx, 0x1000, 0, 1, false).expect("room");
    mock_refuse_allocations(1, true);
    let seg = address_return_segment(&mut ctx, 0x3000, 0, 1, false).expect("served");
    assert_eq!(mock_reclaims(), 0);
    assert_eq!(ctx.get_field(seg, 0), Value::Long(0x3000));
    assert!(!ffm_mint_collects(&ctx), "the mock is not a ZGC VM");
}

/// zt t6/rv3 (`zgc-zt2x1-*` item 5, carried by `zgc-zt4x3-*`): the memo
/// site itself, not the pure gate. On a context that is not the VM's ZGC
/// heap (the mock) `memo_address_return_segment` takes dev's
/// every-collector arm: a refused mint reclaims once and the retry is
/// served. A site that dropped the gate for a literal `false` fails here.
/// The ZGC arm needs a ZGC context, which the mock is not: its deciding
/// evidence stays the probe row (`t1-nd-report.md`).
#[test]
fn zt6rv3_the_memo_address_return_site_collects_off_zgc() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    assert!(!ctx.heap_is_zgc(), "the mock is not a ZGC VM");
    let _ = memo_address_return_segment(&mut ctx, 0, 0x1000).expect("room");
    mock_refuse_allocations(1, true);
    let seg = memo_address_return_segment(&mut ctx, 0, 0x2000)
        .expect("the retry after the reclaim is served");
    assert_eq!(mock_reclaims(), 1, "the memo site ran the reclaim ladder");
    assert_eq!(ctx.get_field(seg, 0), Value::Long(0x2000));
}

/// zt t1/nd: the memo `ADDRESS` return collects on every collector but
/// ZGC under its kill (`CRATONVM_ZGC_FFM_MINT_COLLECT=0` /
/// `CRATONVM_ZGC_NATIVE_DOORS_COLLECT=0`, i.e. `ffm_mint_collects` false);
/// the merge had made it `true` on every arm. The mock is not ZGC, so it
/// takes dev's every-collector arm.
#[test]
fn zt1nd_memo_address_return_honours_the_zgc_kill() {
    // Generational / G1: dev's unconditional reclaim, whatever the switch.
    assert!(memo_address_return_collects(false, false));
    assert!(memo_address_return_collects(false, true));
    // ZGC: the switch decides (positive and kill arm).
    assert!(memo_address_return_collects(true, true));
    assert!(!memo_address_return_collects(true, false));
    let ctx = mock_ctx();
    assert!(!ctx.heap_is_zgc(), "the mock is not a ZGC VM");
}

/// The alignment refusal comes before any allocation on both arms.
#[test]
fn the_alignment_refusal_precedes_the_mint() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    mock_refuse_allocations(usize::MAX, false);
    for collecting in [false, true] {
        let err = address_return_segment(&mut ctx, 0x1001, 0, 8, collecting).unwrap_err();
        assert!(format!("{err:?}").contains("Invalid alignment"), "{err:?}");
    }
    assert_eq!(mock_reclaims(), 0);
}
